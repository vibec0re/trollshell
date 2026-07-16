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
/// [`Submitted`](EventKind::Submitted) (#357) is opt-in *by vocabulary* the
/// same way: only a plugin that renders a [`Node::Entry`] can ever be its
/// target, and rendering one requires a build that can decode it.
///
/// (No longer `Copy` since [`Submitted`](EventKind::Submitted) carries its
/// `String`; `Clone` where a by-value copy used to be implicit.)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// A [`Node::Entry`]'s text was submitted — the user pressed
    /// **Enter/activate** in the entry; `text` is its full contents at that
    /// moment.
    ///
    /// Fired on activate **only** — deliberately no per-keystroke `Changed`
    /// event in v1: a change stream needs the same throttle design as
    /// [`ValueChanged`](EventKind::ValueChanged) and nothing asked for it yet;
    /// additive later if a consumer appears. Like `ValueChanged`, it only ever
    /// reflects a user action: a programmatic re-render that echoes `text`
    /// back into the entry never fires GTK's `activate`, so there is no
    /// echo/feedback loop.
    Submitted { text: String },
}

/// Default for [`Node::Slider`]'s `enabled`: an omitted key means an
/// interactive slider, so a frame built before the field (an older plugin SDK)
/// decodes to a live, draggable slider rather than a greyed one.
fn slider_enabled_default() -> bool {
    true
}

/// Default for [`Node::Pixels`]'s `scale`: an omitted key means the buffer's
/// natural 1× size, so a frame built before the field (an older plugin SDK)
/// decodes to exactly the pre-#358 behavior.
fn pixels_scale_default() -> u32 {
    1
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
    ///   interpolation. The buffer's natural size is `width`×`height` times
    ///   `scale`, but CSS/layout may still size the widget up; the small buffer
    ///   is then drawn big.
    /// - **Sizing (`scale`, #358):** an integer upscale hint. The host requests
    ///   a natural size of `width*scale` × `height*scale`, so a plugin can ask
    ///   for a crisp integer blow-up (a 128×128 LCD at `scale: 2` renders 256px)
    ///   without a shell-side CSS px rule per widget. Shell CSS can still
    ///   override *upward*; the plugin just stops depending on it for a sane
    ///   default. `0` and an absent key both mean `1` (the buffer's natural
    ///   size); the host clamps an absurd scale (scaled dimension beyond its
    ///   size cap) rather than honoring it, mirroring how it degrades a
    ///   malformed buffer — never crashing the shell on bad input.
    /// - `data` and `scale` are **mutable** props: the same `id` re-rendered
    ///   with new bytes (or a new scale) swaps the texture / natural size in
    ///   place rather than rebuilding the widget.
    ///
    /// `scale` is additive exactly like `Text::ellipsize`: a
    /// **`#[serde(default = …)]` field**, so a `Pixels` frame built before #358
    /// (no `scale` key) still decodes — defaulting to `1`, i.e. the pre-#358
    /// sizing is preserved. The reverse direction holds too: a *new* frame's
    /// `scale` key is skipped by a pre-#358 decoder (named-map encoding, no
    /// `deny_unknown_fields`), so a new plugin talking to an older host renders
    /// at 1× instead of breaking the session. Both directions are pinned by
    /// tests in `tests/proto.rs`. (No `skip_serializing_if`: serde has no
    /// by-value predicate and a `fn(&u32) -> bool` helper would trip
    /// `clippy::trivially_copy_pass_by_ref`; a `scale: 1` on the wire is a few
    /// bytes and costs nothing in compat — the old decoder skips the key either
    /// way.)
    Pixels {
        id: Option<NodeId>,
        width: u32,
        height: u32,
        /// The RGBA8 buffer, `width * height * 4` bytes. `serde_bytes` keeps it
        /// a single binary blob on the wire (see the variant docs).
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        /// Integer upscale hint: the widget's natural size is the buffer size
        /// times this (see the variant docs). Defaulted so a pre-#358 frame
        /// decodes to the buffer's natural 1× size; `0` is treated as `1`.
        #[serde(default = "pixels_scale_default")]
        scale: u32,
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
    /// `enabled` (default `true`) is a **mutable prop** too: `false` renders the
    /// slider **insensitive** — the host calls `set_sensitive(false)`, so it
    /// greys out and stops taking drag/scroll/key input (and thus emits no
    /// [`EventKind::ValueChanged`]). It lets a plugin keep a slider *in place*,
    /// visibly inert, when it isn't currently adjustable — the vibectl off-light
    /// brightness case: the row stays put and greyed instead of the slider
    /// popping in and out as the light toggles. A same-id re-render flips
    /// sensitivity in place without a rebuild.
    ///
    /// Additive: a brand-new name-tagged variant, so every existing frame decodes
    /// unchanged and `PROTO_VERSION` stays put; `enabled` is a
    /// `#[serde(default)]` field (defaulting to `true`), so a frame built before
    /// it — including an older SDK's — decodes to an interactive slider, exactly
    /// like `Text::ellipsize`. See [`EventKind`] for why the paired host→plugin
    /// `ValueChanged` push is opt-in *by vocabulary* and needs no manifest
    /// subscription.
    Slider {
        id: NodeId,
        min: f64,
        max: f64,
        value: f64,
        step: f64,
        /// Interactive when `true` (the default); `false` ⇒ insensitive/greyed.
        /// Defaulted so a pre-field frame decodes to an interactive slider.
        #[serde(default = "slider_enabled_default")]
        enabled: bool,
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
    /// A collapsible **expander row** — the plugin-facing analogue of
    /// `AdwExpanderRow` (#333). The host materializes it as a flat, full-width
    /// header (a `gtk::Button` wrapping `header`, with a trailing disclosure
    /// chevron) above a `gtk::Revealer` that holds `children` stacked vertically.
    /// It lets a plugin stop hand-rolling the button + chevron + revealer + the
    /// right-pin dance (the motivating vibectl room panels) and get the chevron,
    /// trailing layout, and reveal for free.
    ///
    /// **Toggling is plugin-driven, not host-local.** Clicking the header fires an
    /// [`EventKind::Click`] addressed by `id` (exactly like [`Button`](Node::Button));
    /// the plugin flips its own `expanded` in its model and re-renders. The host
    /// never self-toggles, so the plugin's model stays the single source of truth
    /// (no hidden host state to desync). `expanded` is a **mutable prop**: a same-id
    /// re-render reveals/hides the body and swaps the chevron in place without a
    /// rebuild. `id` is **required** — it is the click target.
    ///
    /// Additive: a brand-new name-tagged variant, so every existing frame decodes
    /// unchanged and `PROTO_VERSION` stays put. Because the toggle round-trips as a
    /// plain [`EventKind::Click`] — which a plugin opts into simply by rendering the
    /// node — there is no new host→plugin push and so no #305 manifest opt-in is
    /// needed (contrast [`EventKind::ValueChanged`]).
    Expander {
        id: NodeId,
        header: Box<Node>,
        children: Vec<Node>,
        expanded: bool,
        classes: Vec<Cls>,
    },
    /// A single-line **text input** — a `gtk::Entry` (#357), the vocabulary
    /// half of the micro-terminal ask. The user types into it; pressing
    /// **Enter/activate** fires an [`EventKind::Submitted`] addressed by `id`
    /// carrying the entry's full text. `id` is **required**, exactly like
    /// [`Button`](Node::Button): it is the event target.
    ///
    /// `text` is the **echo prop** (reconciler-updatable like
    /// [`Slider`](Node::Slider)'s `value`): the plugin states what the entry
    /// should show — e.g. clear it to `""` after handling a submit, or prefill
    /// a suggestion. The host applies it **when the prop changed since the
    /// last render**, so a re-render that merely echoes the unchanged value
    /// never clobbers what the user is currently typing (the entry-shaped
    /// analogue of the slider's drag suppression) — **or unconditionally on
    /// the first render after a submit**: the render answering a
    /// [`Submitted`](EventKind::Submitted) is authoritative even when its
    /// `text` equals the last-rendered prop, which is what makes
    /// clear-after-submit work when the prop rests at the same value (render
    /// `""`, user types, Enter, render `""` again — the widget clears; a plain
    /// prop-diff would leave the typed text stuck). Anything typed between
    /// Enter and that answering render is overwritten by it. A programmatic
    /// `set_text` never fires GTK's `activate`, so an echo can't re-emit a
    /// [`Submitted`](EventKind::Submitted) — the same structural no-feedback
    /// guarantee as the slider's `change-value` wiring. `placeholder` is the
    /// greyed hint shown while empty (`""` for none); it and `text` are
    /// **mutable props**, updated in place on a same-id re-render.
    ///
    /// Deliberately **no per-keystroke event** in v1 — see
    /// [`EventKind::Submitted`].
    ///
    /// Additive: a brand-new name-tagged variant, so every existing frame
    /// decodes unchanged and [`PROTO_VERSION`](crate::PROTO_VERSION) stays
    /// put. The paired host→plugin `Submitted` push is opt-in *by vocabulary*
    /// (see [`EventKind`]) — a plugin that never renders an `Entry` never has
    /// to decode it, so no manifest opt-in is needed (the #315 Slider
    /// playbook, per the #305 rule).
    Entry {
        id: NodeId,
        /// The echo prop: what the entry should display (see the variant
        /// docs — applied on a prop *change* or unconditionally on the first
        /// render after a submit, so it never fights in-progress typing but
        /// still clears/rewrites reliably after Enter).
        text: String,
        /// Greyed hint text shown while the entry is empty; `""` for none.
        placeholder: String,
        classes: Vec<Cls>,
    },
}
