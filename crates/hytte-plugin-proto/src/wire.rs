//! The GTK-free widget vocabulary.
//!
//! This mirrors `hytte_ui`'s widget-tree types (`Node`, `Dir`, `EventKind`)
//! **field-for-field**, minus every GTK dependency, so a plugin author links
//! *this* crate (no GTK, no `hytte-ui`) and the host can map a [`Node`] to a
//! `hytte_ui::Node` 1:1. That host-side `wire::Node -> hytte_ui::Node` mapping
//! lives in the host (PR 2) and is deliberately **not** in this crate.
//!
//! The set is closed and the encoding is name-tagged (see the crate root's
//! compat rules), so new node kinds and new node fields are additive.

use serde::{Deserialize, Serialize};

/// Stable, plugin-meaningful node identity. Doubles as the diff key and the
/// event target. A [`Node::Button`] requires one; other nodes may omit it and
/// then fall back to positional matching in the host reconciler.
pub type NodeId = String;

/// A CSS class token, applied verbatim by the host — one `add_css_class` call
/// per token, no filtering or remapping. Plugins style through these tokens,
/// never raw CSS: see the `hytte-plugin` SDK crate docs' `# Styling` section
/// for the blessed libadwaita classes, the automatic `.ts-plugin-card`
/// sidebar-mount guarantee, and which `ts-*`/`hytte-*` classes are
/// shell-internal and not safe to copy.
pub type Cls = String;

/// Orientation for a [`Node::Box`]. Mirrors `hytte_ui::Dir`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dir {
    Horizontal,
    Vertical,
}

/// A user interaction on a rendered node, addressed by [`NodeId`]. Mirrors
/// `hytte_ui::EventKind` exactly (the reconciler ships no `Hover`).
///
/// # Wire compat — the `ValueChanged` push is opt-in *by vocabulary* (#305/#315)
///
/// [`Event`](crate::msg::HostMsg::Event) frames flow **host → plugin**, so
/// appending a variant here is subject to the same "a new host→plugin push must
/// be opt-in, never unconditional" rule as [`HostMsg`](crate::msg::HostMsg) (see
/// the crate root). Appending [`ValueChanged`](EventKind::ValueChanged) satisfies
/// that rule **structurally**, not by a manifest opt-in: the host only ever
/// addresses an `Event` at a node the plugin itself rendered, and a plugin built
/// against a pre-#315 proto can't emit a [`Node::Slider`] — so it can never be
/// the target of a `ValueChanged`, and its `rmp-serde` never has to decode the
/// unknown variant. A plugin only starts receiving `ValueChanged` once it opts
/// in by rendering a `Slider`, i.e. once it was rebuilt against this proto and
/// *can* decode it. (Contrast [`HostMsg::SlotVisibility`](crate::msg::HostMsg::SlotVisibility),
/// which is state the host would otherwise push unconditionally, so that one
/// needs an explicit [`StateKey`](crate::manifest::StateKey) subscription.)
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    /// A [`Node::Button`] was clicked.
    Click,
    /// A `scroll: true` [`Node::Box`] was scrolled; `dx`/`dy` are raw deltas.
    Scroll { dx: f64, dy: f64 },
    /// A [`Node::Slider`] was moved by the user (drag, scroll, or keyboard);
    /// `value` is the slider's new position, clamped to its `min..=max` range.
    ///
    /// The host emits these on a **trailing-edge throttle** (≈one per 50 ms plus
    /// a final settle), never one per raw motion tick — but a plugin driving a
    /// network round-trip per value (the vibectl per-light brightness case)
    /// should still debounce its own side effects, since a drag is inherently a
    /// stream. The value only ever reflects a user action: a programmatic
    /// re-render that echoes a new `value` back into the slider does **not**
    /// produce a `ValueChanged` (see `hytte_ui`'s `change-value` vs
    /// `value-changed` note), so there is no echo/feedback loop.
    ValueChanged { value: f64 },
}

/// The closed widget vocabulary. A plugin's view is a single root [`Node`].
///
/// Mirrors `hytte_ui::Node`: `Box { scroll }` carries the scroll flag
/// explicitly, so mapping to the GTK-side node is a trivial 1:1 in the host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Node {
    /// A `gtk::Box`. `id` (optional) keys the node for diffing/reordering;
    /// `scroll` independently makes it a scroll event target.
    Box {
        id: Option<NodeId>,
        dir: Dir,
        spacing: i32,
        scroll: bool,
        classes: Vec<Cls>,
        children: Vec<Node>,
    },
    /// A horizontal list **row** — a semantic sibling of [`Box`](Node::Box) for
    /// list-y cards (planned additive in the spec's node vocab, #199). Children
    /// are modelled exactly like `Box`'s and laid out left-to-right; style via
    /// `classes`. The host materializes it as a horizontal `gtk::Box`.
    Row {
        id: Option<NodeId>,
        classes: Vec<Cls>,
        children: Vec<Node>,
    },
    /// A vertical list **container** stacking its children (typically
    /// [`Row`](Node::Row)s) top-to-bottom — the list-y counterpart to
    /// [`Box`](Node::Box). The host materializes it as a selection-less list
    /// surface; style the list chrome via `classes`.
    ListBox {
        id: Option<NodeId>,
        classes: Vec<Cls>,
        children: Vec<Node>,
    },
    /// A `gtk::Label`.
    Label {
        id: Option<NodeId>,
        text: String,
        classes: Vec<Cls>,
    },
    /// A **wrapping** `gtk::Label`. Where [`Label`](Node::Label) is a single-line
    /// tag whose natural width forces its container wider (the pet's 320 px
    /// blow-out, #281), `Text` wraps at word/char boundaries so a long string
    /// stays within its container. `max_width_chars`, when set, caps the label's
    /// natural width at that many characters; when `None` the wrap is bounded by
    /// the container (e.g. the sidebar's 320 px clamp).
    ///
    /// `ellipsize` (default `false`) flips the flow mode: when `true` the label
    /// is **single-line** and truncates with a trailing ellipsis
    /// (`EllipsizeMode::End`) instead of wrapping — matching how the native
    /// departures row cuts a long destination at 22 chars (#296). Like
    /// `text`/`max_width_chars`, it is a **mutable prop**: a same-id re-render
    /// flipping it swaps the flow mode in place rather than rebuilding the label.
    ///
    /// Additive on two axes: `Text` is a brand-new variant vs `Label` (existing
    /// `Label` frames decode unchanged), and `ellipsize` is a **`#[serde(default)]`
    /// field**, so a `Text` frame built before #297 (no `ellipsize` key) still
    /// decodes — defaulting to `false`, i.e. the wrapping behaviour is preserved.
    /// (It carries no `skip_serializing_if`: serde has no by-value bool predicate
    /// and a `fn(&bool) -> bool` helper would trip `clippy::trivially_copy_pass_by_ref`.
    /// A `false` on the wire is a couple of bytes and costs nothing in compat —
    /// the decoder defaults the absent key either way.)
    Text {
        id: Option<NodeId>,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_width_chars: Option<i32>,
        #[serde(default)]
        ellipsize: bool,
        classes: Vec<Cls>,
    },
    /// A `gtk::Image` set from a themed icon `name` (name only — never pixels).
    Icon {
        id: Option<NodeId>,
        name: String,
        classes: Vec<Cls>,
    },
    /// A raster image: a `width`×`height` block of **RGBA8** pixels.
    ///
    /// - **Layout:** `data` is row-major (row 0 first), 4 bytes per pixel in
    ///   `[R, G, B, A]` order, **non-premultiplied** straight alpha. Its length
    ///   MUST equal `width * height * 4` — the host validates this and renders
    ///   nothing (with a warning) for a buffer that doesn't match, so a
    ///   malformed plugin can never crash the shell.
    /// - **Encoding:** `data` rides the wire as a single `MessagePack` `bin` blob
    ///   (via `serde_bytes`), not a per-byte int array, so a 128×128 RGBA frame
    ///   is ~64 KiB on the wire — well under [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN).
    /// - **Rendering:** the host scales the buffer up with **nearest-neighbor**
    ///   filtering (crisp, chunky pixels — the "LCD" look), never linear
    ///   interpolation. The buffer's natural size is `width`×`height`, but
    ///   CSS/layout may size the widget up; the small buffer is then drawn big.
    /// - `data` is a **mutable** prop: the same `id` re-rendered with new bytes
    ///   swaps the texture in place rather than rebuilding the widget.
    Pixels {
        id: Option<NodeId>,
        width: u32,
        height: u32,
        /// The RGBA8 buffer, `width * height * 4` bytes. `serde_bytes` keeps it
        /// a single binary blob on the wire (see the variant docs).
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        classes: Vec<Cls>,
    },
    /// A `gtk::Button`. `id` is **required** — it is the click event target.
    Button {
        id: NodeId,
        classes: Vec<Cls>,
        child: Box<Node>,
    },
    /// A `gtk::ProgressBar`, `fraction` in `0.0..=1.0`.
    Progress {
        id: Option<NodeId>,
        fraction: f64,
        classes: Vec<Cls>,
    },
    /// An interactive `gtk::Scale` (horizontal range control) — the writable
    /// counterpart to [`Progress`](Node::Progress). The user drags/scrolls/keys
    /// it to pick a `value` in `min..=max`; the host reports each user move as an
    /// [`EventKind::ValueChanged`] addressed by `id` (throttled — see that
    /// variant). `id` is **required**, exactly like [`Button`](Node::Button): it
    /// is the event target.
    ///
    /// `value` is a **mutable prop**: a same-id re-render moves the thumb in
    /// place — *except* while the user is actively dragging, when the host
    /// suppresses the programmatic move so a stale echo can't rubber-band the
    /// grab (the standard optimistic-state / reconcile-on-echo model; the
    /// motivating vibectl per-light brightness slider relies on it). `min`,
    /// `max`, and `step` are mutable too. `step` is the keyboard/scroll
    /// increment. Style via `classes` (e.g. an `.osd`/`.flat` hook) — the host
    /// draws no value label.
    ///
    /// Additive: a brand-new name-tagged variant, so every existing frame decodes
    /// unchanged and `PROTO_VERSION` stays put. See [`EventKind`] for why the
    /// paired host→plugin `ValueChanged` push is opt-in *by vocabulary* and needs
    /// no manifest subscription.
    Slider {
        id: NodeId,
        min: f64,
        max: f64,
        value: f64,
        step: f64,
        classes: Vec<Cls>,
    },
    /// A `gtk::Revealer`; `open` drives `set_reveal_child`.
    Revealer {
        id: Option<NodeId>,
        open: bool,
        child: Box<Node>,
    },
    /// A `gtk::Separator`.
    Separator { classes: Vec<Cls> },
    /// An **expanding gap** — an empty, style-less box that eats a container's
    /// slack so its siblings justify around it. The host materializes it as an
    /// empty `gtk::Box` with `hexpand`/`vexpand` set (dir-agnostic; the cross-axis
    /// expand is inert since the box has zero natural size), so a single `Spacer`
    /// between a cluster and a value right-pins the value in a [`Row`](Node::Row)
    /// (`Label + Spacer + Label`), and two spacers centre the meat between them.
    ///
    /// It carries **no id and no children** — purely structural, styled by its
    /// neighbours, never itself. This is how the native rows achieve justification
    /// (an expanding filler), mirrored as one additive, field-less variant so no
    /// existing node grows a `hexpand` field (#295/#296). Consecutive spacers
    /// reuse by kind in the reconciler (no id to key on) — which is fine, they're
    /// interchangeable.
    Spacer,
}
