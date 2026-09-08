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

use hytte_plugin_proto::{Cls, Node, NodeId, SCROLLED_VOCAB};

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

#[cfg(test)]
mod tests {
    use super::{host_speaks_scrolled, list, row, scrolled};
    use hytte_plugin_proto::{Node, SCROLLED_VOCAB};

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
}
