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
//! - [`multi_sparkline`] (#1419) is the same again for
//!   [`Node::MultiSparkline`], the shell's per-core history graph, negotiated
//!   on [`MULTI_SPARKLINE_VOCAB`]: it degrades to a [`sparkline`] — which
//!   degrades in turn — so one call is safe against every shell.
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
    Cls, Dir, HOMOGENEOUS_CLASS, MULTI_SPARKLINE_VOCAB, Node, NodeId, SCROLLED_VOCAB,
    SPARKLINE_VOCAB,
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

/// Start a [`Node::MultiSparkline`] — the shell's multi-series history graph
/// — over `series`, one window per line, each **oldest first**.
///
/// Defaults: no id, no classes, `max: None` (auto-scale to the largest sample
/// across every series), and the older-shell fallback line is the per-sample
/// mean of the series (see [`MultiSparkline::fallback`]). Set
/// [`MultiSparkline::max`] for a quantity with a natural ceiling — `1.0` for a
/// per-core load fraction.
///
/// Read [`MultiSparkline::build`] before using it: the node is
/// **negotiated**, and `build` is where that is handled.
#[must_use]
pub fn multi_sparkline(series: impl Into<Vec<Vec<f32>>>) -> MultiSparkline {
    MultiSparkline {
        id: None,
        series: series.into(),
        max: None,
        classes: Vec::new(),
        fallback: None,
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
    /// lossy: the degraded tree is consistently shaped for as long as the host's
    /// answer holds, so it matches itself render to render through `plan_diff`'s
    /// keyless positional path; and the classes styled a viewport that no longer
    /// exists. (The answer is not fixed from a session's first frame, though:
    /// the seed render goes out before the host's `Hello` arrives, at the
    /// unconditional floor, so the first frame is always this fallback and the
    /// viewport arrives with the next one. The reconciler builds the new shape
    /// rather than reusing the old one across that swap.) Put anything the
    /// *card* needs on the child, not here. (Contrast
    /// [`shader`](crate::shader), which hands the fallback back
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
    /// of auto-scaling to the largest sample. A `max` of zero, a negative one
    /// or `NaN` auto-scales anyway, and `+inf` becomes `f32::MAX` — a fixed
    /// top, which draws every finite sample on the bottom rail; see
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
    /// The fallback keeps this node's `id` and `class`es, so the degraded tree
    /// diffs against itself by the same key for as long as the host's answer
    /// holds. That answer is **not** fixed from a session's first frame: the
    /// seed render goes out before the host's `Hello` arrives, at the
    /// unconditional floor, so the first frame of every session is this bar
    /// and the next one is the line, at the same id. The reconciler keys by id
    /// *and* kind, so it builds a new widget for the line rather than handing
    /// it the bar's.
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

/// Builder for [`Node::MultiSparkline`]; see [`multi_sparkline`].
#[derive(Clone, Debug)]
pub struct MultiSparkline {
    id: Option<NodeId>,
    series: Vec<Vec<f32>>,
    max: Option<f32>,
    classes: Vec<Cls>,
    fallback: Option<Vec<f32>>,
}

impl MultiSparkline {
    /// Set the diff/reorder key. The older-shell fallback keeps it too.
    #[must_use]
    pub fn id(mut self, id: impl Into<NodeId>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Fix the top of the shared y axis at `max` (every series draws
    /// `0..=max`), instead of auto-scaling to the largest sample across all of
    /// them. A `max` of zero, a negative one or `NaN` auto-scales anyway, and
    /// `+inf` becomes `f32::MAX` — a fixed top, which draws every finite sample
    /// on the bottom rail; see
    /// [`sane_sparkline_max`](hytte_plugin_proto::sane_sparkline_max). The
    /// fallback line uses the same axis.
    #[must_use]
    pub fn max(mut self, max: f32) -> Self {
        self.max = Some(max);
        self
    }

    /// Append one CSS class. It styles the widget (its height, its margins);
    /// the lines' colours are the widget's own per-series hues and no class
    /// changes them. The fallback keeps the classes too.
    #[must_use]
    pub fn class(mut self, class: impl Into<Cls>) -> Self {
        self.classes.push(class.into());
        self
    }

    /// The single line an older shell draws **instead** of this graph, oldest
    /// first — for when the plugin already has a better summary than the
    /// default, such as the overall CPU load it keeps beside its per-core
    /// windows.
    ///
    /// Unset, the fallback is the **per-sample mean** of the series, aligned
    /// at the newest sample: its last point is the mean of every series' last
    /// sample, the one before it the mean of every series long enough to have
    /// one, and so on back to the longest series' oldest. It averages exactly
    /// the samples a current shell would **draw** — the series past
    /// [`MAX_MULTI_SPARKLINE_SERIES`](hytte_plugin_proto::MAX_MULTI_SPARKLINE_SERIES)
    /// and the samples [`multi_sparkline_keep`](hytte_plugin_proto::multi_sparkline_keep)
    /// cuts feed nothing, so the older shell's line summarises the graph a
    /// newer one shows. Each sample is sanitised first
    /// ([`sane_sparkline_sample`](hytte_plugin_proto::sane_sparkline_sample)),
    /// so a `NaN` counts as a zero in its slot's mean rather than poisoning it.
    #[must_use]
    pub fn fallback(mut self, values: impl Into<Vec<f32>>) -> Self {
        self.fallback = Some(values.into());
        self
    }

    /// Finish the node — **or fall back to a [`Node::Sparkline`]** (built with
    /// [`sparkline`]) if this session's host never advertised
    /// [`MULTI_SPARKLINE_VOCAB`].
    ///
    /// [`Node::MultiSparkline`] is a negotiated variant (#1419): a host that
    /// predates it cannot decode the frame at all, so emitting one
    /// unconditionally would turn a richer picture into #437's silent 5 s
    /// reconnect loop. The check is against [`MULTI_SPARKLINE_VOCAB`] and
    /// nothing earlier — a generation-7 or -8 shell draws a single line and
    /// cannot decode this node.
    ///
    /// **Why a single line.** It is the closest thing an older shell draws: the
    /// same history, on the same axis, in the same flat native look, only
    /// summarised to one series (by default the mean — for per-core loads,
    /// the overall load). The fallback goes through [`Sparkline::build`], so it
    /// is negotiated in turn and becomes a [`Node::Progress`] on a shell older
    /// than [`SPARKLINE_VOCAB`]: one call is safe against every shell. It
    /// keeps this node's `id`, `max` and `class`es, so the degraded tree diffs
    /// against itself by the same key for as long as the host's answer holds.
    ///
    /// That answer is **not** fixed from a session's first frame: the seed
    /// render goes out before the host's `Hello` arrives, at the unconditional
    /// floor, so the first frame of every session is the fallback (a
    /// `Progress` bar, there) and the next one is the graph, at the same id.
    /// The reconciler keys by id *and* kind, so each swap builds the new
    /// kind's widget rather than reusing the old one's.
    ///
    /// Use [`build_unnegotiated`](Self::build_unnegotiated) only where the wire
    /// shape itself is under test.
    #[must_use]
    pub fn build(self) -> Node {
        if host_speaks_multi_sparkline() {
            self.build_unnegotiated()
        } else {
            self.build_fallback()
        }
    }

    /// The [`Node::MultiSparkline`] itself, with no host check.
    ///
    /// For tests that pin the wire shape. In a live plugin this is only correct
    /// behind your own [`host_speaks_multi_sparkline`] branch — see
    /// [`build`](Self::build).
    #[must_use]
    pub fn build_unnegotiated(self) -> Node {
        Node::MultiSparkline {
            id: self.id,
            series: self.series,
            max: self.max,
            classes: self.classes,
        }
    }

    /// The older-shell arm of [`build`](Self::build): the fallback line (or
    /// the newest-aligned mean) as a [`sparkline`], which negotiates itself.
    fn build_fallback(self) -> Node {
        let values = self.fallback.unwrap_or_else(|| drawn_mean(&self.series));
        Sparkline {
            id: self.id,
            values,
            max: self.max,
            classes: self.classes,
        }
        .build()
    }
}

/// [`MultiSparkline`]'s default fallback line (see
/// [`MultiSparkline::fallback`]): the [`newest_aligned_mean`] of exactly the
/// windows a current shell draws — the first
/// [`MAX_MULTI_SPARKLINE_SERIES`](hytte_plugin_proto::MAX_MULTI_SPARKLINE_SERIES)
/// series, each cut to its newest
/// [`multi_sparkline_keep`](hytte_plugin_proto::multi_sparkline_keep) samples,
/// the trim `Node::clamp_in_place` and the host apply (#1438 review NIT 5).
fn drawn_mean(series: &[Vec<f32>]) -> Vec<f32> {
    use hytte_plugin_proto::{MAX_MULTI_SPARKLINE_SERIES, multi_sparkline_keep};

    let keep = multi_sparkline_keep(series);
    let windows: Vec<&[f32]> = series
        .iter()
        .take(MAX_MULTI_SPARKLINE_SERIES)
        .map(|s| &s[s.len().saturating_sub(keep)..])
        .collect();
    newest_aligned_mean(&windows)
}

/// The per-sample mean of `series`, aligned at the newest sample, oldest first.
///
/// As long as the longest series. Slot `k` from the end averages every series
/// that has a sample `k` from its own end, in `f64` so a column of saturated
/// samples cannot overflow before the division.
fn newest_aligned_mean<S: AsRef<[f32]>>(series: &[S]) -> Vec<f32> {
    use hytte_plugin_proto::sane_sparkline_sample;

    let longest = series.iter().map(|s| s.as_ref().len()).max().unwrap_or(0);
    let mut line = vec![0.0_f32; longest];
    for (back, slot) in line.iter_mut().rev().enumerate() {
        let (sum, count) = series
            .iter()
            .map(<S as AsRef<[f32]>>::as_ref)
            .filter_map(|s| s.len().checked_sub(back + 1).map(|i| s[i]))
            .fold((0.0_f64, 0.0_f64), |(sum, count), sample| {
                (sum + f64::from(sane_sparkline_sample(sample)), count + 1.0)
            });
        // `back < longest`, so the longest series always contributes and
        // `count >= 1`. The mean of finite `f32`s is within `f32`'s range, so
        // the narrowing only rounds.
        #[allow(clippy::cast_possible_truncation)]
        let mean = (sum / count) as f32;
        *slot = mean;
    }
    line
}

/// Whether this session's host advertised the multi-series graph (#1419) —
/// i.e. whether [`negotiated_vocab`](crate::display::negotiated_vocab) has
/// reached [`MULTI_SPARKLINE_VOCAB`].
///
/// [`MultiSparkline::build`] consults this for you; call it directly only to
/// skip work of your own — the per-core windows an older shell would never
/// see drawn, say, or a toggle that would only reveal the fallback line.
#[must_use]
pub fn host_speaks_multi_sparkline() -> bool {
    crate::display::negotiated_vocab() >= MULTI_SPARKLINE_VOCAB
}

#[cfg(test)]
mod tests {
    use super::{
        container, host_speaks_multi_sparkline, host_speaks_scrolled, host_speaks_sparkline, list,
        multi_sparkline, newest_aligned_mean, row, scrolled, sparkline,
    };
    use hytte_plugin_proto::{
        Dir, HOMOGENEOUS_CLASS, MULTI_SPARKLINE_VOCAB, Node, PAGE_VISIBLE_VOCAB, SCROLLED_VOCAB,
        SPARKLINE_VOCAB,
    };

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

    // ── MultiSparkline (#1419) ─────────────────────────────────────────────

    #[test]
    fn multi_sparkline_defaults_auto_scale_with_no_id_or_classes() {
        assert_eq!(
            multi_sparkline(vec![vec![0.5], vec![1.5]]).build_unnegotiated(),
            Node::MultiSparkline {
                id: None,
                series: vec![vec![0.5], vec![1.5]],
                max: None,
                classes: vec![],
            },
        );
    }

    /// The whole fallback chain on one thread (the `NEGOTIATED` thread-local):
    /// a generation-9 host gets the graph; a generation-7 or -8 one the mean
    /// `Sparkline`, id, top and classes kept; anything older a `Progress` at
    /// the mean line's newest level.
    ///
    /// **Falsified** by making `build` return `build_unnegotiated()`
    /// unconditionally (a generation-8 shell is sent a variant it cannot
    /// decode), or by building the fallback as a bare `Node::Sparkline` literal
    /// instead of through `Sparkline::build` (a generation-6 shell is sent a
    /// `Sparkline` it cannot decode either).
    #[test]
    fn multi_sparkline_falls_back_a_generation_at_a_time() {
        let graph = || {
            multi_sparkline(vec![vec![0.2, 0.4], vec![0.6, 0.8]])
                .id("per-core-load")
                .max(1.0)
                .class("ts-cores")
        };

        crate::display::set_negotiated(MULTI_SPARKLINE_VOCAB);
        assert!(host_speaks_multi_sparkline());
        assert_eq!(
            graph().build(),
            Node::MultiSparkline {
                id: Some("per-core-load".into()),
                series: vec![vec![0.2, 0.4], vec![0.6, 0.8]],
                max: Some(1.0),
                classes: vec!["ts-cores".into()],
            },
        );

        for single_line_shell in [SPARKLINE_VOCAB, PAGE_VISIBLE_VOCAB] {
            crate::display::set_negotiated(single_line_shell);
            assert!(!host_speaks_multi_sparkline(), "gen {single_line_shell}");
            assert_eq!(
                graph().build(),
                Node::Sparkline {
                    id: Some("per-core-load".into()),
                    values: vec![0.4, 0.6],
                    max: Some(1.0),
                    classes: vec!["ts-cores".into()],
                },
                "a generation-{single_line_shell} shell draws the mean line, id, top and \
                 classes kept",
            );
        }

        crate::display::set_negotiated(SPARKLINE_VOCAB - 1);
        match graph().build() {
            Node::Progress {
                id,
                fraction,
                classes,
            } => {
                assert_eq!(id.as_deref(), Some("per-core-load"));
                assert!(
                    (fraction - 0.6).abs() < 1e-6,
                    "the mean line's newest sample, 0.6 of 1.0: {fraction}"
                );
                assert_eq!(classes, vec!["ts-cores".to_owned()]);
            }
            other => panic!("an older shell still gets a bar, got {other:?}"),
        }
        crate::display::set_negotiated(0);
        assert!(!host_speaks_multi_sparkline(), "no Hello, no graph");
        assert!(matches!(graph().build(), Node::Progress { .. }));
    }

    /// Generation 8 — #1427's push, the generation right before this one and
    /// not itself `Hello`-negotiated — is still an older shell: `>=` against
    /// `MULTI_SPARKLINE_VOCAB`, not against the single line's marker or
    /// whichever came last.
    ///
    /// **Falsified** by comparing against `SPARKLINE_VOCAB` (or
    /// `PAGE_VISIBLE_VOCAB`) in `host_speaks_multi_sparkline`.
    #[test]
    fn vocab_eight_does_not_unlock_the_multi_sparkline() {
        assert_eq!(
            MULTI_SPARKLINE_VOCAB - 1,
            PAGE_VISIBLE_VOCAB,
            "precondition"
        );
        crate::display::set_negotiated(MULTI_SPARKLINE_VOCAB - 1);
        assert!(!host_speaks_multi_sparkline());
        assert!(host_speaks_sparkline(), "…though it does draw one line");
        assert!(matches!(
            multi_sparkline(vec![vec![1.0]]).build(),
            Node::Sparkline { .. }
        ));
        crate::display::set_negotiated(0);
    }

    /// An explicit fallback line replaces the mean, on the same axis and key.
    #[test]
    fn an_explicit_fallback_line_replaces_the_mean() {
        crate::display::set_negotiated(SPARKLINE_VOCAB);
        assert_eq!(
            multi_sparkline(vec![vec![0.9, 0.9]])
                .fallback(vec![0.1, 0.2, 0.3])
                .id("load")
                .build(),
            Node::Sparkline {
                id: Some("load".into()),
                values: vec![0.1, 0.2, 0.3],
                max: None,
                classes: vec![],
            },
        );
        // …and the explicit line is what an older shell's bar reads, too.
        crate::display::set_negotiated(0);
        assert!(matches!(
            multi_sparkline(vec![vec![0.9]]).fallback(vec![0.25]).max(1.0).build(),
            Node::Progress { fraction, .. } if (fraction - 0.25).abs() < 1e-9
        ));
    }

    /// The default fallback is the per-sample mean **aligned at the newest
    /// sample**: series of unequal length line up at their right-hand ends,
    /// a slot averages only the series that reach it, and a poisoned sample
    /// counts as zero rather than poisoning its slot.
    ///
    /// **Falsified** by aligning at the oldest sample (index from the front:
    /// the last slot reads 4.0, not 3.0), or by dividing every slot by the
    /// series count (the oldest slot, reached by one series, halves), or by
    /// averaging unsanitised samples (the `NaN` slot is `NaN`).
    #[test]
    fn the_default_fallback_is_the_newest_aligned_mean() {
        // [1, 2, 3, 4] and [2, 2]: the last two slots average both series
        // (4 and 2 → 3; 3 and 2 → 2.5), the first two only the long one.
        assert_eq!(
            newest_aligned_mean(&[vec![1.0, 2.0, 3.0, 4.0], vec![2.0, 2.0]]),
            vec![1.0, 2.0, 2.5, 3.0],
        );
        assert_eq!(
            newest_aligned_mean(&[vec![f32::NAN, 1.0], vec![1.0, 1.0]]),
            vec![0.5, 1.0],
        );
        assert_eq!(
            newest_aligned_mean(&[vec![f32::INFINITY], vec![f32::INFINITY]]),
            vec![f32::MAX],
            "saturated samples do not overflow the sum",
        );
        assert!(
            newest_aligned_mean::<Vec<f32>>(&[]).is_empty(),
            "no series, no line"
        );
        assert!(
            newest_aligned_mean(&[Vec::<f32>::new(), Vec::new()]).is_empty(),
            "empty windows"
        );
    }

    /// The default fallback averages exactly what a current shell would
    /// **draw** — the series cap, the per-series cap and the point cap all
    /// apply first, through the proto's own `multi_sparkline_keep` (#1438
    /// review NIT 5) — so an older shell's line summarises the graph a newer
    /// one shows, and is never longer than a single line's cap.
    ///
    /// **Falsified** by averaging the raw series (every assertion below reds:
    /// the dropped 257th series moves the mean, the long window comes back
    /// 2 000 samples long, and the point-cut samples show up at the front).
    #[test]
    fn the_default_fallback_averages_only_what_the_shell_would_draw() {
        use hytte_plugin_proto::{MAX_MULTI_SPARKLINE_SERIES, MAX_SPARKLINE_SAMPLES};
        let line = |series: Vec<Vec<f32>>| match multi_sparkline(series).build() {
            Node::Sparkline { values, .. } => values,
            other => panic!("a single-line shell gets the line, got {other:?}"),
        };
        crate::display::set_negotiated(SPARKLINE_VOCAB);

        // The series cap: a 257th series is never drawn, so it moves nothing.
        let mut capped = vec![vec![1.0_f32; 2]; MAX_MULTI_SPARKLINE_SERIES];
        capped.push(vec![1_000.0; 2]);
        assert_eq!(
            line(capped),
            vec![1.0, 1.0],
            "the dropped series is not averaged"
        );

        // The per-series cap: a 2 000-sample window is drawn as its newest
        // `MAX_SPARKLINE_SAMPLES`, and so is its fallback line.
        let long: Vec<f32> = (0..2_000_u16).map(f32::from).collect();
        let fallback = line(vec![long]);
        assert_eq!(fallback.len(), MAX_SPARKLINE_SAMPLES);
        assert_eq!(fallback.first().copied(), Some(976.0), "the newest 1 024");

        // The point cap: 65 full lines keep their newest 1 008, so the 16
        // oldest samples of each — 1 000 here, 0 everywhere else — are cut
        // before the average, not averaged in.
        let mut full = vec![0.0_f32; MAX_SPARKLINE_SAMPLES];
        full[..16].fill(1_000.0);
        let fallback = line(vec![full; 65]);
        assert_eq!(fallback.len(), 1008);
        assert!(
            fallback.iter().all(|v| v.abs() < f32::EPSILON),
            "{fallback:?}"
        );

        crate::display::set_negotiated(0);
    }
}
