//! Builders for the list-card layout vocabulary (#966).
//!
//! The rest of the SDK hands a plugin the raw [`Node`] enum and lets it write
//! struct literals, which is fine for a node whose fields a plugin sets all of.
//! These three are not that:
//!
//! - [`row`] and [`list`] grew an **additive** field each ([`Node::Row`]'s
//!   `spacing`, [`Node::ListBox`]'s `dense`), and a literal has to spell every
//!   field — so every plugin that never wanted either now writes `spacing: 0,`
//!   and `dense: false,` by hand. The builders default them, which is also what
//!   makes the *next* additive field free rather than another mechanical sweep.
//!   #961 collected on exactly that: `Row`'s `tooltip` cost the builder one
//!   defaulted field and one [`Row::tooltip`] method, and no call site.
//! - [`scrolled`] is a **negotiated** variant: emitting it against a host that
//!   never advertised [`SCROLLED_VOCAB`] would kill the session's decode. The
//!   builder does that check, exactly as [`shader`](crate::shader) does for
//!   [`Node::Shader`], so the safe path is the short one.
//! - [`sparkline`] (#1252) is the same shape of builder for [`Node::Sparkline`],
//!   the shell's own flat history line, negotiated on [`SPARKLINE_VOCAB`]: it
//!   degrades to a [`Node::Progress`] at the newest sample's level against a
//!   shell that cannot draw one.
//! - [`container`] builds a [`Node::Box`], and it and [`row`] carry
//!   `.homogeneous(true)` (#1252): equal-size children, spelt as the
//!   [`HOMOGENEOUS_CLASS`] the host reads rather than as a field every struct
//!   literal would have had to grow.
//!
//! ```ignore
//! use hytte_plugin::nodes;
//!
//! use hytte_plugin::proto::Node;
//!
//! let card = nodes::list(vec![
//!     nodes::row(vec![name, Node::Spacer, value])
//!         .id("argus")
//!         .spacing(6)
//!         .build(),
//!     // …one row per agent…
//! ])
//! .class("boxed-list")
//! .dense(true)
//! .build();
//!
//! // Bounded at 240 px against a shell that has a viewport; the bare card
//! // against one that hasn't.
//! nodes::scrolled(240, card).id("hive-body").build()
//! ```
//!
//! A native-look history row — the shell's Stats page's
//! `[name | line | value]` — is a [`row`] around a [`sparkline`]:
//!
//! ```ignore
//! nodes::row(vec![
//!     name_label,
//!     // The last minute of load, oldest first, on a fixed 0..=1 axis.
//!     nodes::sparkline(ring.iter().copied().collect::<Vec<f32>>())
//!         .id("cpu-history")
//!         .max(1.0)
//!         .build(),
//!     value_label,
//! ])
//! .spacing(8)
//! .build()
//! ```

use hytte_plugin_proto::{
    Cls, Dir, HOMOGENEOUS_CLASS, Node, NodeId, SCROLLED_VOCAB, SPARKLINE_VOCAB,
};

/// Add [`HOMOGENEOUS_CLASS`] to `classes` (once) or take it out — the one
/// spelling [`Container::homogeneous`] and [`Row::homogeneous`] share.
fn set_homogeneous(classes: &mut Vec<Cls>, on: bool) {
    classes.retain(|c| c != HOMOGENEOUS_CLASS);
    if on {
        classes.push(HOMOGENEOUS_CLASS.to_owned());
    }
}

/// Start a [`Node::Box`] laid out along `dir` with `children`.
///
/// Defaults: no id, no classes, `spacing: 0`, not a scroll target, no tooltip,
/// not homogeneous. The builder exists for [`Container::homogeneous`] (#1252):
/// a literal can spell every other field, but "make the columns equal" is a
/// class the host reads, and the builder is where that spelling lives.
#[must_use]
pub fn container(dir: Dir, children: Vec<Node>) -> Container {
    Container {
        id: None,
        dir,
        spacing: 0,
        scroll: false,
        classes: Vec::new(),
        children,
        tooltip: None,
    }
}

/// Start a [`Node::Row`] — a horizontal list row — with `children`.
///
/// Defaults: no id, no classes, `spacing: 0` (the flush layout a `Row` had
/// before #966), no tooltip.
#[must_use]
pub fn row(children: Vec<Node>) -> Row {
    Row {
        id: None,
        classes: Vec::new(),
        spacing: 0,
        children,
        tooltip: None,
    }
}

/// Start a [`Node::ListBox`] — a vertical list container — with `children`.
///
/// Defaults: no id, no classes, `dense: false` (the libadwaita row height the
/// host has always given list children).
#[must_use]
pub fn list(children: Vec<Node>) -> List {
    List {
        id: None,
        classes: Vec::new(),
        dense: false,
        children,
    }
}

/// Start a [`Node::Scrolled`] — a bounded viewport — around `child`, capped at
/// `max_height` pixels (`0` = unbounded).
///
/// Read [`Scrolled::build`] before using it: the node is **negotiated**, and
/// `build` is where that is handled.
#[must_use]
pub fn scrolled(max_height: u16, child: Node) -> Scrolled {
    Scrolled {
        id: None,
        max_height,
        classes: Vec::new(),
        child: Box::new(child),
    }
}

/// Start a [`Node::Sparkline`] — the shell's flat history line — over
/// `values`, **oldest first**.
///
/// Defaults: no id, no classes, `max: None` (auto-scale to the largest sample,
/// which is what the native page does for a byte rate or a temperature). Set
/// [`Sparkline::max`] for a quantity with a natural ceiling — `1.0` for a load
/// fraction, `100.0` for a percentage.
///
/// Read [`Sparkline::build`] before using it: the node is **negotiated**, and
/// `build` is where that is handled.
#[must_use]
pub fn sparkline(values: impl Into<Vec<f32>>) -> Sparkline {
    Sparkline {
        id: None,
        values: values.into(),
        max: None,
        classes: Vec::new(),
    }
}

/// Builder for [`Node::Row`]; see [`row`].
#[derive(Clone, Debug)]
pub struct Row {
    id: Option<NodeId>,
    classes: Vec<Cls>,
    spacing: u16,
    children: Vec<Node>,
    tooltip: Option<String>,
}

impl Row {
    /// Set the diff/reorder key.
    #[must_use]
    pub fn id(mut self, id: impl Into<NodeId>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Set the inter-child gap in pixels (#966).
    #[must_use]
    pub fn spacing(mut self, px: u16) -> Self {
        self.spacing = px;
        self
    }

    /// Set the row's hover text (#961) — plain text, never markup.
    ///
    /// One legend for the whole row; a child with its own tooltip still wins
    /// the hover where the pointer is over it. An older shell skips the field
    /// and renders the row exactly as before.
    #[must_use]
    pub fn tooltip(mut self, text: impl Into<String>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    /// Append one CSS class.
    #[must_use]
    pub fn class(mut self, class: impl Into<Cls>) -> Self {
        self.classes.push(class.into());
        self
    }

    /// Give every child the same width (#1252) — see
    /// [`HOMOGENEOUS_CLASS`] for what the host does with it and why it is a
    /// class. An older shell ignores it and lays the row out as before.
    #[must_use]
    pub fn homogeneous(mut self, on: bool) -> Self {
        set_homogeneous(&mut self.classes, on);
        self
    }

    /// Finish the node.
    #[must_use]
    pub fn build(self) -> Node {
        Node::Row {
            id: self.id,
            classes: self.classes,
            spacing: self.spacing,
            children: self.children,
            tooltip: self.tooltip,
        }
    }
}

/// Builder for [`Node::Box`]; see [`container`].
#[derive(Clone, Debug)]
pub struct Container {
    id: Option<NodeId>,
    dir: Dir,
    spacing: i32,
    scroll: bool,
    classes: Vec<Cls>,
    children: Vec<Node>,
    tooltip: Option<String>,
}

impl Container {
    /// Set the diff/reorder key.
    #[must_use]
    pub fn id(mut self, id: impl Into<NodeId>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Set the inter-child gap in pixels.
    #[must_use]
    pub fn spacing(mut self, px: i32) -> Self {
        self.spacing = px;
        self
    }

    /// Make the box a scroll **event target** — not a viewport; see
    /// [`Node::Box`]'s `scroll` and [`scrolled`] for the difference.
    #[must_use]
    pub fn scroll(mut self, on: bool) -> Self {
        self.scroll = on;
        self
    }

    /// Set the box's hover text — plain text, never markup.
    #[must_use]
    pub fn tooltip(mut self, text: impl Into<String>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    /// Append one CSS class.
    #[must_use]
    pub fn class(mut self, class: impl Into<Cls>) -> Self {
        self.classes.push(class.into());
        self
    }

    /// Give every child the same size along the box's axis (#1252) — two
    /// columns of a page the same width, say, whatever their contents. See
    /// [`HOMOGENEOUS_CLASS`] for what the host does with it and why it is a
    /// class; an older shell ignores it and lays the box out as before.
    #[must_use]
    pub fn homogeneous(mut self, on: bool) -> Self {
        set_homogeneous(&mut self.classes, on);
        self
    }

    /// Finish the node.
    #[must_use]
    pub fn build(self) -> Node {
        Node::Box {
            id: self.id,
            dir: self.dir,
            spacing: self.spacing,
            scroll: self.scroll,
            classes: self.classes,
            children: self.children,
            tooltip: self.tooltip,
        }
    }
}

/// Builder for [`Node::ListBox`]; see [`list`].
#[derive(Clone, Debug)]
pub struct List {
    id: Option<NodeId>,
    classes: Vec<Cls>,
    dense: bool,
    children: Vec<Node>,
}

impl List {
    /// Set the diff/reorder key.
    #[must_use]
    pub fn id(mut self, id: impl Into<NodeId>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Drop the per-row height floor the host's `GtkListBoxRow` wrappers carry
    /// (#966), so a list of one-line rows is as tall as its rows.
    #[must_use]
    pub fn dense(mut self, dense: bool) -> Self {
        self.dense = dense;
        self
    }

    /// Append one CSS class.
    #[must_use]
    pub fn class(mut self, class: impl Into<Cls>) -> Self {
        self.classes.push(class.into());
        self
    }

    /// Finish the node.
    #[must_use]
    pub fn build(self) -> Node {
        Node::ListBox {
            id: self.id,
            classes: self.classes,
            dense: self.dense,
            children: self.children,
        }
    }
}

/// Builder for [`Node::Scrolled`]; see [`scrolled`].
#[derive(Clone, Debug)]
pub struct Scrolled {
    id: Option<NodeId>,
    max_height: u16,
    classes: Vec<Cls>,
    child: Box<Node>,
}

impl Scrolled {
    /// Set the diff/reorder key.
    #[must_use]
    pub fn id(mut self, id: impl Into<NodeId>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Set the height cap in pixels; `0` is unbounded.
    #[must_use]
    pub fn max_height(mut self, px: u16) -> Self {
        self.max_height = px;
        self
    }

    /// Append one CSS class.
    #[must_use]
    pub fn class(mut self, class: impl Into<Cls>) -> Self {
        self.classes.push(class.into());
        self
    }

    /// Finish the node — **or fall back to the bare child** if this session's
    /// host never advertised [`SCROLLED_VOCAB`].
    ///
    /// [`Node::Scrolled`] is a negotiated variant (#966): a host that predates
    /// it cannot decode the frame at all, so emitting one unconditionally would
    /// turn a layout nicety into a silent 5 s reconnect loop. The degradation is
    /// exact — the child renders unbounded, which is what every card did before
    /// the viewport existed — so a plugin can call this unconditionally and let
    /// the shell decide.
    ///
    /// The fallback returns the **child alone**, so this node's own
    /// [`id`](Self::id) and [`class`](Self::class)es go with the frame they
    /// described — an old host sees neither. That is deliberate rather than
    /// lossy: negotiation is fixed for the life of a session, so the degraded
    /// tree is consistently shaped and matches itself render to render through
    /// `plan_diff`'s keyless positional path; and the classes styled a viewport
    /// that no longer exists. Put anything the *card* needs on the child, not
    /// here. (Contrast [`shader`](crate::shader), which hands the fallback back
    /// to the plugin as an `Option` rather than choosing one — the right shape
    /// there, because a shader's fallback is a whole second rendering.)
    ///
    /// Use [`build_unnegotiated`](Self::build_unnegotiated) only where the wire
    /// shape itself is under test.
    #[must_use]
    pub fn build(self) -> Node {
        if host_speaks_scrolled() {
            self.build_unnegotiated()
        } else {
            *self.child
        }
    }

    /// The [`Node::Scrolled`] itself, with no host check.
    ///
    /// For tests that pin the wire shape. In a live plugin this is only correct
    /// behind your own [`host_speaks_scrolled`] branch — see [`build`](Self::build).
    #[must_use]
    pub fn build_unnegotiated(self) -> Node {
        Node::Scrolled {
            id: self.id,
            max_height: self.max_height,
            classes: self.classes,
            child: self.child,
        }
    }
}

/// Whether this session's host advertised the bounded-viewport vocabulary
/// (#966) — i.e. whether
/// [`negotiated_vocab`](crate::display::negotiated_vocab) has reached
/// [`SCROLLED_VOCAB`].
///
/// [`Scrolled::build`] consults this for you; call it directly only to skip work
/// of your own (computing a cap you would not use, say).
#[must_use]
pub fn host_speaks_scrolled() -> bool {
    crate::display::negotiated_vocab() >= SCROLLED_VOCAB
}

/// Builder for [`Node::Sparkline`]; see [`sparkline`].
#[derive(Clone, Debug)]
pub struct Sparkline {
    id: Option<NodeId>,
    values: Vec<f32>,
    max: Option<f32>,
    classes: Vec<Cls>,
}

impl Sparkline {
    /// Set the diff/reorder key.
    #[must_use]
    pub fn id(mut self, id: impl Into<NodeId>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Fix the top of the y axis at `max` (the line draws `0..=max`), instead
    /// of auto-scaling to the largest sample. A `max` that is not a positive
    /// finite number auto-scales anyway — see
    /// [`sane_sparkline_max`](hytte_plugin_proto::sane_sparkline_max).
    #[must_use]
    pub fn max(mut self, max: f32) -> Self {
        self.max = Some(max);
        self
    }

    /// Append one CSS class. A class whose rule sets `color` recolours the
    /// line (the widget strokes in its own theme colour).
    #[must_use]
    pub fn class(mut self, class: impl Into<Cls>) -> Self {
        self.classes.push(class.into());
        self
    }

    /// Finish the node — **or fall back to a [`Node::Progress`]** at the newest
    /// sample's level if this session's host never advertised
    /// [`SPARKLINE_VOCAB`].
    ///
    /// [`Node::Sparkline`] is a negotiated variant (#1252): a host that
    /// predates it cannot decode the frame at all, so emitting one
    /// unconditionally would turn a nicer picture into #437's silent 5 s
    /// reconnect loop. A plugin can therefore call this unconditionally and let
    /// the shell decide.
    ///
    /// **Why a progress bar.** The fallback has to be something every shell
    /// already draws. A preem `Scope` is the other trend line on the wire, but
    /// the node exists precisely so a page can read like the shell's own rather
    /// than a retro display, and handing an older shell a phosphor sweep would
    /// undo that. A `Label` of the newest value would repeat the reading a
    /// history row already prints beside its line. A `Progress` is a flat,
    /// native widget that still answers the question the line's right-hand end
    /// answers — *how much, now* — on the same axis: the newest sample over
    /// `max`, or over the largest sample when auto-scaling, exactly the
    /// `draw_sparkline` normalisation, so the bar is as full as the line's last
    /// point is high. What it loses is the history itself, and nothing on an
    /// older shell can draw that except the scope this is avoiding.
    ///
    /// The fallback keeps this node's `id` and `class`es — negotiation is
    /// fixed for the life of a session, so the degraded tree is consistently
    /// shaped and diffs against itself by the same key.
    ///
    /// Use [`build_unnegotiated`](Self::build_unnegotiated) only where the wire
    /// shape itself is under test.
    #[must_use]
    pub fn build(self) -> Node {
        if host_speaks_sparkline() {
            self.build_unnegotiated()
        } else {
            self.build_fallback()
        }
    }

    /// The [`Node::Sparkline`] itself, with no host check.
    ///
    /// For tests that pin the wire shape. In a live plugin this is only correct
    /// behind your own [`host_speaks_sparkline`] branch — see
    /// [`build`](Self::build).
    #[must_use]
    pub fn build_unnegotiated(self) -> Node {
        Node::Sparkline {
            id: self.id,
            values: self.values,
            max: self.max,
            classes: self.classes,
        }
    }

    /// The older-shell arm of [`build`](Self::build): a [`Node::Progress`] at
    /// the newest sample's level on the line's own axis.
    fn build_fallback(self) -> Node {
        use hytte_plugin_proto::{sane_fraction, sane_sparkline_max, sane_sparkline_sample};

        let newest = self
            .values
            .last()
            .copied()
            .map_or(0.0, sane_sparkline_sample);
        // `draw_sparkline`'s own denominator: the fixed top when there is a
        // usable one, else the largest sample (never below `EPSILON`, so an
        // all-zero line is an empty bar rather than a division by zero).
        let top = sane_sparkline_max(self.max).unwrap_or_else(|| {
            self.values
                .iter()
                .copied()
                .map(sane_sparkline_sample)
                .fold(0.0_f32, f32::max)
                .max(f32::EPSILON)
        });
        Node::Progress {
            id: self.id,
            fraction: sane_fraction(f64::from(newest / top)),
            classes: self.classes,
        }
    }
}

/// Whether this session's host advertised the flat trend line (#1252) — i.e.
/// whether [`negotiated_vocab`](crate::display::negotiated_vocab) has reached
/// [`SPARKLINE_VOCAB`].
///
/// [`Sparkline::build`] consults this for you; call it directly only to skip
/// work of your own (keeping a history ring an older shell would never see
/// drawn, say).
#[must_use]
pub fn host_speaks_sparkline() -> bool {
    crate::display::negotiated_vocab() >= SPARKLINE_VOCAB
}

#[cfg(test)]
mod tests {
    use super::{
        container, host_speaks_scrolled, host_speaks_sparkline, list, row, scrolled, sparkline,
    };
    use hytte_plugin_proto::{Dir, HOMOGENEOUS_CLASS, Node, SCROLLED_VOCAB, SPARKLINE_VOCAB};

    fn label(text: &str) -> Node {
        Node::Label {
            id: None,
            text: text.to_owned(),
            classes: vec![],
            tooltip: None,
        }
    }

    #[test]
    fn row_defaults_are_the_pre_966_shape() {
        assert_eq!(
            row(vec![label("a")]).build(),
            Node::Row {
                id: None,
                classes: vec![],
                spacing: 0,
                children: vec![label("a")],
                tooltip: None,
            },
            "an unconfigured row must encode exactly what a pre-#966 literal did"
        );
    }

    #[test]
    fn row_carries_id_spacing_and_classes() {
        assert_eq!(
            row(vec![]).id("r0").spacing(6).class("ts-row").build(),
            Node::Row {
                id: Some("r0".into()),
                classes: vec!["ts-row".into()],
                spacing: 6,
                children: vec![],
                tooltip: None,
            }
        );
    }

    /// #961's field, and the reason the builder was worth having: it defaults
    /// (`row_defaults_are_the_pre_966_shape` above pins the `None`) and it sets.
    #[test]
    fn row_carries_a_tooltip() {
        assert_eq!(
            row(vec![]).id("argus").tooltip("argus · running").build(),
            Node::Row {
                id: Some("argus".into()),
                classes: vec![],
                spacing: 0,
                children: vec![],
                tooltip: Some("argus · running".into()),
            }
        );
    }

    #[test]
    fn list_defaults_are_the_pre_966_shape() {
        assert_eq!(
            list(vec![]).build(),
            Node::ListBox {
                id: None,
                classes: vec![],
                dense: false,
                children: vec![],
            }
        );
    }

    #[test]
    fn list_carries_dense() {
        assert_eq!(
            list(vec![]).class("boxed-list").dense(true).build(),
            Node::ListBox {
                id: None,
                classes: vec!["boxed-list".into()],
                dense: true,
                children: vec![],
            }
        );
    }

    /// The negotiation, both arms. `NEGOTIATED` is a thread-local, and the two
    /// arms must be pinned on **one** thread so the second cannot pass merely
    /// because it landed on a fresh one.
    #[test]
    fn scrolled_emits_the_variant_only_once_the_host_advertised_it() {
        crate::display::set_negotiated(0);
        assert!(
            !host_speaks_scrolled(),
            "a session that never got a Hello speaks no negotiated generation"
        );
        assert_eq!(
            scrolled(240, label("body")).build(),
            label("body"),
            "against an unadvertised host the viewport degrades to its bare child"
        );

        crate::display::set_negotiated(SCROLLED_VOCAB);
        assert!(host_speaks_scrolled());
        assert_eq!(
            scrolled(240, label("body")).id("card").build(),
            Node::Scrolled {
                id: Some("card".into()),
                max_height: 240,
                classes: vec![],
                child: Box::new(label("body")),
            }
        );
        crate::display::set_negotiated(0);
    }

    /// One generation short of the marker is still "not advertised" — the check
    /// is `>=` against `SCROLLED_VOCAB`, not "the host said something".
    #[test]
    fn an_older_negotiated_generation_does_not_unlock_the_viewport() {
        crate::display::set_negotiated(SCROLLED_VOCAB - 1);
        assert!(!host_speaks_scrolled());
        assert_eq!(scrolled(240, label("body")).build(), label("body"));
        crate::display::set_negotiated(0);
    }

    #[test]
    fn container_defaults_are_a_plain_box() {
        assert_eq!(
            container(Dir::Vertical, vec![label("a")]).build(),
            Node::Box {
                id: None,
                dir: Dir::Vertical,
                spacing: 0,
                scroll: false,
                classes: vec![],
                children: vec![label("a")],
                tooltip: None,
            },
        );
    }

    /// `.homogeneous(true)` adds the host's class once, `.homogeneous(false)`
    /// takes it back out, and neither disturbs a class the plugin set itself —
    /// on a `Box` and on a `Row` alike.
    ///
    /// **Falsified** by `set_homogeneous` pushing without the `retain` (the
    /// class appears twice), or by making `false` a no-op.
    #[test]
    fn homogeneous_is_the_hosts_class_set_once_and_cleared() {
        let classes = |node: Node| match node {
            Node::Box { classes, .. } | Node::Row { classes, .. } => classes,
            other => panic!("{other:?}"),
        };
        let h = HOMOGENEOUS_CLASS.to_owned();
        assert_eq!(
            classes(
                container(Dir::Horizontal, vec![])
                    .class("ts-cols")
                    .homogeneous(true)
                    .homogeneous(true)
                    .build()
            ),
            vec!["ts-cols".to_owned(), h.clone()],
        );
        assert_eq!(
            classes(
                container(Dir::Horizontal, vec![])
                    .homogeneous(true)
                    .class("ts-cols")
                    .homogeneous(false)
                    .build()
            ),
            vec!["ts-cols".to_owned()],
        );
        assert_eq!(
            classes(row(vec![]).homogeneous(true).build()),
            vec![h.clone()]
        );
        assert_eq!(
            classes(row(vec![]).homogeneous(true).homogeneous(false).build()),
            Vec::<String>::new(),
        );
    }

    #[test]
    fn sparkline_defaults_auto_scale_with_no_id_or_classes() {
        assert_eq!(
            sparkline(vec![0.5, 1.5]).build_unnegotiated(),
            Node::Sparkline {
                id: None,
                values: vec![0.5, 1.5],
                max: None,
                classes: vec![],
            },
        );
    }

    /// #1252's negotiation, both arms, on one thread (the `NEGOTIATED`
    /// thread-local, same reason as the viewport test above): the variant on a
    /// host that advertised it, a `Progress` at the newest sample's level on
    /// one that did not.
    ///
    /// **Falsified** by making `build` return `build_unnegotiated()`
    /// unconditionally (the old-host arm emits a `Sparkline` an older shell
    /// cannot decode), or by comparing against `SCROLLED_VOCAB` (a generation-6
    /// shell would be sent one — the next test).
    #[test]
    fn sparkline_emits_the_variant_only_once_the_host_advertised_it() {
        let line = || {
            sparkline(vec![0.2, 0.9, 0.25])
                .id("cpu-history")
                .max(1.0)
                .class("ts-cpu")
        };

        crate::display::set_negotiated(0);
        assert!(!host_speaks_sparkline());
        assert_eq!(
            line().build(),
            Node::Progress {
                id: Some("cpu-history".into()),
                fraction: 0.25,
                classes: vec!["ts-cpu".into()],
            },
            "an unadvertised host gets a bar at the NEWEST sample's level, id and \
             classes kept",
        );

        crate::display::set_negotiated(SPARKLINE_VOCAB);
        assert!(host_speaks_sparkline());
        assert_eq!(
            line().build(),
            Node::Sparkline {
                id: Some("cpu-history".into()),
                values: vec![0.2, 0.9, 0.25],
                max: Some(1.0),
                classes: vec!["ts-cpu".into()],
            },
        );
        crate::display::set_negotiated(0);
    }

    /// A generation-6 shell (#1158's mounts — the generation right before this
    /// one, and not itself `Hello`-negotiated) is still an older shell: `>=`
    /// against `SPARKLINE_VOCAB`, not against whichever marker came last.
    #[test]
    fn an_older_negotiated_generation_does_not_unlock_the_sparkline() {
        crate::display::set_negotiated(SPARKLINE_VOCAB - 1);
        assert!(!host_speaks_sparkline());
        assert!(matches!(
            sparkline(vec![1.0]).build(),
            Node::Progress { .. }
        ));
        crate::display::set_negotiated(0);
    }

    /// The fallback's axis is the line's own: over `max` when there is one,
    /// over the largest sample when auto-scaling, and total over the inputs the
    /// sanitisers exist for.
    #[test]
    fn the_fallback_bar_reads_the_newest_sample_on_the_lines_own_axis() {
        crate::display::set_negotiated(0);
        let fraction = |node: Node| match node {
            Node::Progress { fraction, .. } => fraction,
            other => panic!("expected the fallback bar, got {other:?}"),
        };
        // Auto-scaled: 2 of a peak of 8 is a quarter.
        assert!((fraction(sparkline(vec![8.0, 4.0, 2.0]).build()) - 0.25).abs() < 1e-9);
        // …and the peak is the largest sample, not the first one (#1414
        // review, LOW 6 — `[8, 4, 2]` alone cannot tell the two apart).
        assert!((fraction(sparkline(vec![2.0, 8.0, 4.0]).build()) - 0.5).abs() < 1e-9);
        // Fixed top: 50 of 100.
        assert!((fraction(sparkline(vec![10.0, 50.0]).max(100.0).build()) - 0.5).abs() < 1e-9);
        // Over the top pins full, a negative pins empty — the line's own pin.
        assert!((fraction(sparkline(vec![3.0]).max(1.0).build()) - 1.0).abs() < 1e-9);
        assert!(fraction(sparkline(vec![-3.0]).max(1.0).build()).abs() < 1e-9);
        // Empty, all-zero and poisoned lines are an empty bar, never a NaN.
        assert!(fraction(sparkline(Vec::<f32>::new()).build()).abs() < 1e-9);
        assert!(fraction(sparkline(vec![0.0, 0.0]).build()).abs() < 1e-9);
        assert!(fraction(sparkline(vec![f32::NAN]).build()).abs() < 1e-9);
        assert!(fraction(sparkline(vec![1.0, f32::NAN]).max(f32::NAN).build()).abs() < 1e-9);
    }
}
