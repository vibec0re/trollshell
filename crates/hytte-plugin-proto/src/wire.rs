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
//!
//! **Appending a [`Node`] or [`EventKind`] variant ⇒ bump
//! [`VOCAB`](crate::VOCAB)** (#437): a plugin can render the new variant, so an
//! older host must be able to detect and refuse it at the handshake rather than
//! silently failing to decode the render frame.

use crate::preem::PreemWidget;
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
///
/// Appending a variant here ⇒ **bump [`VOCAB`](crate::VOCAB)** (#437), the same
/// as [`Node`] — a plugin can be the target of the new event once it renders the
/// node that emits it.
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
///
/// Appending a variant here ⇒ **bump [`VOCAB`](crate::VOCAB)** (#437): a plugin
/// renders these, so the counter is how an older host refuses one it can't decode.
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
    /// **The four floats have a contract, and it is enforced** — by
    /// [`clamp_in_place`](Node::clamp_in_place) / [`sane_slider_floats`], which
    /// the SDK runs on every view and the host re-runs on every frame it
    /// receives. `min` and `max` must be finite with a finite, strictly
    /// positive span; `value` must be finite and inside `min..=max`; `step`
    /// must be finite and inside `(0.0, max - min]`. Anything else is
    /// *rewritten*, not rejected: a degenerate range falls back to
    /// [`DEFAULT_SLIDER_MIN`]`..=`[`DEFAULT_SLIDER_MAX`] with both ends
    /// replaced together (which relocates `value` with it), a non-finite or
    /// non-positive `step` becomes [`DEFAULT_SLIDER_STEP_FRACTION`] of the span
    /// — or the whole span, where a subnormal span makes that underflow to zero
    /// — and a `step` wider than the span is capped to it. The reason this is
    /// enforced rather than merely documented is that GTK's own
    /// `gtk_adjustment_new` returns `NULL` for a degenerate range, which
    /// **aborts the host**; see the mapping table on
    /// [`clamp_in_place`](Node::clamp_in_place) for the whole derivation.
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
    /// A **preem retro-display widget** rendered shell-side from typed state
    /// rather than shipped as pixels (#882, epic #881) — see the
    /// [`preem`](crate::preem) module for the whole vocabulary, the
    /// config-vs-state contract, and the animation-ownership rules.
    ///
    /// One wrapper variant carrying a [`PreemWidget`], not eight flat `Node`
    /// variants: the preem vocabulary then versions as a single unit, the host
    /// dispatches to its renderers from one arm here, and appending a ninth
    /// widget never touches this enum again.
    ///
    /// # `id` is **required** on this variant (#900)
    ///
    /// Everywhere else in this enum an `id` is an optimisation: it keys the node
    /// for diffing and reordering, and going without it costs at worst a widget
    /// rebuilt where it could have been updated. Here it is the contract,
    /// because the host holds a *renderer instance* per node — phosphor buffer,
    /// needle velocity, flip clocks, scroll offset, held peak — and the id is
    /// the only thing that ties an instance to the node it belongs to. State the
    /// vocabulary deliberately keeps off the wire cannot be re-derived from a
    /// frame, so a mis-keyed node does not merely restart: it inherits
    /// **another widget's** animation.
    ///
    /// The type stays `Option` for wire compatibility (the field is optional in
    /// every other variant and a hand-rolled client can omit it), so the host
    /// degrades rather than refusing: an anonymous preem node is keyed by its
    /// **ordinal among the un-id'd preem nodes** of that tree, the host logs one
    /// warning per plugin session, and the node still renders. That fallback is
    /// only stable while those nodes keep their order *and* their count —
    /// inserting or removing an anonymous sibling shifts every later one down a
    /// slot, and because interchangeable widgets have identical configs by
    /// construction the host cannot tell the difference and updates the survivor
    /// in place: the third gauge renders the second's needle, a phosphor history
    /// moves onto another signal, a variable-length row of per-core meters
    /// glitches on every insert.
    ///
    /// The Rust SDK's `display` wrappers stamp the id from the widget key they
    /// already take (`display::gauge::node("cpu")`), so a plugin built on them
    /// never reaches the fallback. A hand-rolled client should do the same. Use
    /// [`preem_id`](crate::preem::preem_id) rather than
    /// [`preem`](crate::preem::preem) when constructing a node by hand.
    ///
    /// **Negotiated, not unconditional.** Unlike every other variant here, a
    /// plugin must not emit this one on sight: it emits it only once the host
    /// has advertised [`PREEM_VOCAB`](crate::preem::PREEM_VOCAB) or better in
    /// [`HostMsg::Hello`](crate::msg::HostMsg::Hello), and rasterises to
    /// [`Pixels`](Node::Pixels) otherwise. That is what makes a preem-capable
    /// plugin work unchanged against a shell that has never heard of preem
    /// nodes — see the [`preem` module docs](crate::preem#compat-contract) for
    /// the full compat matrix.
    ///
    /// Additive: a brand-new name-tagged variant, so every existing frame
    /// decodes unchanged and [`PROTO_VERSION`](crate::PROTO_VERSION) stays put.
    /// It does grow the vocabulary, so it bumps [`VOCAB`](crate::VOCAB) — but
    /// **not** [`VOCAB_UNCONDITIONAL`](crate::VOCAB_UNCONDITIONAL), because the
    /// negotiation above means an old host can never receive it.
    /// `widget` is boxed so one preem node — [`Gauge`](crate::preem::PreemWidget::Gauge)
    /// alone carries eleven scalars — doesn't set the size of *every* [`Node`],
    /// including the `Label`s and `Row`s a tree is mostly made of. `Box<T>`
    /// serializes transparently as `T`, so the boxing is invisible on the wire
    /// (pinned by the `plugin_render_preem_v1` golden fixture, which did not
    /// move when it was introduced).
    Preem {
        /// The reconciliation key — **required in practice**, `Option` only for
        /// wire shape. See the variant docs: without it the host falls back to
        /// an ordinal key, warns once per session, and animation state moves
        /// between siblings on any insert or removal.
        id: Option<NodeId>,
        classes: Vec<Cls>,
        widget: Box<PreemWidget>,
    },
}

// ── float sanitisation (#904) ───────────────────────────────────────────────

/// The `min` a [`Node::Slider`] with a **degenerate** stated range falls back
/// to — see [`sane_slider_floats`] for what counts as degenerate and why the
/// two ends are replaced together.
///
/// With [`DEFAULT_SLIDER_MAX`] this is the same unit scale
/// [`GaugeRange::default`](crate::preem::GaugeRange) already uses, so the
/// vocabulary names one default scale rather than two.
pub const DEFAULT_SLIDER_MIN: f64 = 0.0;

/// The `max` of that fallback range — see [`DEFAULT_SLIDER_MIN`].
pub const DEFAULT_SLIDER_MAX: f64 = 1.0;

/// The `step` a [`Node::Slider`] falls back to when its stated one is unusable,
/// as a fraction of the **sanitised** span: one percent, i.e. a hundred
/// keyboard nudges from end to end.
///
/// Span-relative rather than absolute so the fallback means the same thing on a
/// `0.0..=1.0` slider and on a `0.0..=255.0` one.
pub const DEFAULT_SLIDER_STEP_FRACTION: f64 = 0.01;

/// Sanitise a [`Node::Progress`]'s `fraction`: finite and inside `0.0..=1.0`
/// afterwards, always.
///
/// `±inf` saturates exactly as `GtkProgressBar`'s own `CLAMP` does; `NaN` —
/// the one input that `CLAMP` passes straight through — takes `0.0`. See the
/// mapping table on [`Node::clamp_in_place`] for the derivation.
#[must_use]
pub fn sane_fraction(fraction: f64) -> f64 {
    if fraction.is_nan() {
        // `CLAMP` is `NaN`-transparent, so GTK does not decide this one: it
        // stores the `NaN` and carries it into integer allocation arithmetic.
        // `0.0` is the empty bar — the same "no reading" neutral #899 chose for
        // `LedStripState::level`, and the value that restores the `empty` CSS
        // class a `NaN` silently loses.
        0.0
    } else {
        fraction.clamp(0.0, 1.0)
    }
}

/// The four sanitised floats of a [`Node::Slider`], as produced by
/// [`sane_slider_floats`].
///
/// A named struct rather than a `(f64, f64, f64, f64)` because the four are
/// trivially transposable at a call site and a swapped `min`/`value` would be
/// silent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SliderFloats {
    /// Low end of the range; finite, and strictly below [`max`](Self::max).
    pub min: f64,
    /// High end of the range; finite, strictly above [`min`](Self::min), and
    /// with a finite span.
    pub max: f64,
    /// The thumb position; finite and inside `min..=max`.
    pub value: f64,
    /// The keyboard/scroll increment; finite, strictly positive, and no wider
    /// than the span.
    pub step: f64,
}

/// Sanitise a [`Node::Slider`]'s four `f64`s as a unit — the range first, then
/// the value and the step against it.
///
/// Afterwards `min` and `max` are finite with a finite, strictly positive span,
/// `value` is finite and inside `min..=max`, and `step` is finite and inside
/// `(0.0, max - min]`. See the mapping table on [`Node::clamp_in_place`] for
/// the per-field derivation and its `GtkAdjustment` citations.
#[must_use]
pub fn sane_slider_floats(min: f64, max: f64, value: f64, step: f64) -> SliderFloats {
    // The range first: a value and a step are only meaningful against a scale,
    // and clamping against a `NaN` bound would just re-poison them (and would
    // hand `f64::clamp` a `lo > hi` it panics on).
    let (min, max) = sane_slider_range(min, max);
    let span = max - min;
    let value = if value.is_nan() {
        min
    } else {
        value.clamp(min, max)
    };
    let step = if step.is_finite() && step > 0.0 {
        // A step wider than the range moves the thumb end to end, exactly as a
        // step equal to the span does, so capping costs no expressible
        // behaviour and keeps every float bounded by the range it belongs to.
        step.min(span)
    } else {
        let fallback = span * DEFAULT_SLIDER_STEP_FRACTION;
        // One percent of a subnormal span rounds to zero, which would leave a
        // dead control; the whole span is then the only step such a range can
        // express.
        if fallback > 0.0 { fallback } else { span }
    };
    SliderFloats {
        min,
        max,
        value,
        step,
    }
}

/// A usable slider range, replacing a degenerate one with
/// [`DEFAULT_SLIDER_MIN`]`..=`[`DEFAULT_SLIDER_MAX`].
///
/// Degenerate means any of: an end that is not finite, `max <= min`, or a span
/// so wide it overflows to infinity — `min = -1.7e308, max = 1.7e308` is two
/// finite numbers whose difference is not, and the span is the divisor for
/// every thumb position GTK computes.
///
/// Both ends are replaced **together**: mixing a caller's `min` with a default
/// `max` invents a scale nobody asked for. (The rule #899 settled for
/// [`GaugeRange`](crate::preem::GaugeRange), applied to the same shape.)
fn sane_slider_range(min: f64, max: f64) -> (f64, f64) {
    let span = max - min;
    if min.is_finite() && max.is_finite() && span.is_finite() && span > 0.0 {
        (min, max)
    } else {
        (DEFAULT_SLIDER_MIN, DEFAULT_SLIDER_MAX)
    }
}

impl Node {
    /// Sanitise every float in this node **and its whole subtree**, returning
    /// the normalized tree.
    ///
    /// The [`Node`]-level counterpart to
    /// [`PreemWidget::clamped`](crate::preem::PreemWidget::clamped), and it
    /// subsumes it: a [`Preem`](Node::Preem) child is handed to that routine,
    /// so one call sanitises a whole render tree. See
    /// [`clamp_in_place`](Self::clamp_in_place) for the per-field mapping and
    /// why the equality it buys is load-bearing.
    #[must_use]
    pub fn clamped(mut self) -> Self {
        self.clamp_in_place();
        self
    }

    /// [`clamped`](Self::clamped) in place, for a caller that already owns the
    /// tree mutably — the form the per-frame paths want, since the owning form
    /// would clone every `String`/`Vec` in the tree just to feed the clamp.
    ///
    /// # The float invariant
    ///
    /// Afterwards, **every** `f64` this tree carries is finite and within its
    /// documented bounds, and every [`Preem`](Node::Preem) child satisfies
    /// [`PreemWidget::clamp_in_place`](crate::preem::PreemWidget::clamp_in_place)'s
    /// own invariant. That makes the derived `PartialEq` on [`Node`] a usable
    /// *did anything change?* test: `NaN != NaN`, so before this a single
    /// poisoned `fraction` made a tree unequal to an identical copy of itself,
    /// and the SDK's `view != last_view` (`hytte-plugin`'s `runtime.rs`, which
    /// calls this routine on every view for exactly that reason) stayed true
    /// forever — one `Render` per inbound event for a picture that never
    /// changes, bounded above only by #560's ~30 fps cap.
    ///
    /// The **host** cost is a different one, and worth stating precisely
    /// because the symmetry is tempting and wrong: `hytte_ui`'s
    /// `Reconciler::render` has no whole-tree equality gate at all — it
    /// re-applies every prop on every pass regardless — so there was never a
    /// host-side `Node` diff for a `NaN` to defeat. What a non-finite float
    /// costs the host is what the widget does with it (a stored `NaN`
    /// fraction, or an aborting `gtk_adjustment_new`), which the mapping below
    /// is derived from. The one host-side equality gate is
    /// `preem_render::apply`'s `instance.applied == *widget`, and that one is
    /// preem-only — #899's, not this seam's.
    ///
    /// The routine is also a **fixpoint**: clamping a clamped tree changes
    /// nothing, so a host that re-sanitises what an SDK already sanitised sees
    /// no movement and its own gates cannot fire a frame late.
    ///
    /// It is the *float* seam, and only that. [`Pixels`](Node::Pixels)'s
    /// `len == w * h * 4` and scale checks stay host-side
    /// (`trollshell/src/plugins/wire_map.rs`) because they want `tracing` and
    /// the host is the trust boundary; if they ever move into this crate, this
    /// is the seam they join.
    ///
    /// # The mapping — contract, not implementation detail
    ///
    /// The rule is #899's, carried over: **saturate where the drawing code
    /// saturates, and name a constant only where the drawing code's answer is
    /// a stateful keep-previous a stateless sanitiser cannot reach.** What
    /// draws these two nodes is `hytte_ui`'s `widget_tree` — `bar.set_fraction`
    /// for [`Progress`](Node::Progress) (`crates/hytte-ui/src/widget_tree.rs:819`
    /// building, `:1125` reconciling) and an explicit
    /// `gtk::Adjustment::new(value, min, max, step, step, 0.0)` for
    /// [`Slider`](Node::Slider) (`:836` building; `:1146`-`:1149` +
    /// `scale.set_value` reconciling) — so the citations below are GTK's own,
    /// read from **GTK 4.22.4**.
    ///
    /// The two GTK widgets behave in opposite ways, which is why the rows
    /// diverge:
    ///
    /// - `GtkProgressBar` **accepts everything silently**.
    ///   `gtk_progress_bar_set_fraction` is one `CLAMP`
    ///   (`gtk/gtkprogressbar.c:781`) and `CLAMP` (`glib/gmacros.h:984`) is
    ///   `NaN`-transparent, so a `NaN` is *stored*: it then multiplies into an
    ///   `int` allocation width (`gtkprogressbar.c:424`, a `double`→`int` cast
    ///   of a `NaN`), loses **both** the `empty` and `full` CSS classes
    ///   (`:277`-`:291`, since `NaN <= 0.0` and `NaN >= 1.0` are both false),
    ///   and renders `"nan %"` in the bar's label.
    /// - `GtkAdjustment` **refuses** non-finite input loudly and accepts
    ///   degenerate finite input silently. Every scalar setter is guarded —
    ///   `gtk_adjustment_set_value` `g_return_if_fail (isfinite (value))`
    ///   (`gtk/gtkadjustment.c:563`), `set_lower` `:622`, `set_upper` `:670`,
    ///   `set_step_increment` `:715`, `set_page_increment` `:760` — so a
    ///   non-finite field is a `CRITICAL` plus a **no-op**, leaving the live
    ///   adjustment on its previous value. `gtk_adjustment_new` refuses harder
    ///   still: `g_return_val_if_fail (lower + page_size <= upper, NULL)`
    ///   (`:395`) returns `NULL` for `max < min` **or** for a `NaN` end, and
    ///   `gtk4`'s `Adjustment::new` feeds that pointer to `from_glib_none`,
    ///   whose `debug_assert!(!ptr.is_null())` (the `wrapper!`-generated impl
    ///   for the concrete object type, `glib-0.22.5/src/object.rs:911`) panics
    ///   in a debug build and is undefined behaviour in a release one.
    ///   A plugin's `Slider { min: 1.0, max: 0.0 }` is therefore not merely
    ///   churn — it is a shell abort, and this seam is what stops it.
    ///
    /// | field | drawing code (GTK 4.22.4) | shape | bounds | `NaN` | `+inf` | `-inf` | finite, out of range |
    /// |---|---|---|---|---|---|---|---|
    /// | [`Progress::fraction`](Node::Progress) | `gtkprogressbar.c:781` `CLAMP (fraction, 0.0, 1.0)` | total on `±inf`, transparent on `NaN` | `0.0..=1.0` | `0.0` | `1.0` | `0.0` | clamp (parity) |
    /// | [`Slider::min`](Node::Slider) / [`max`](Node::Slider) | `gtkadjustment.c:395` `NULL` return; `:622`/`:670` `isfinite` guards | refuse, **stateful** | finite, `max > min`, finite span | [`DEFAULT_SLIDER_MIN`]`..=`[`DEFAULT_SLIDER_MAX`], both ends as a unit | same | same | same fallback when `max <= min` |
    /// | [`Slider::value`](Node::Slider) | `gtkadjustment.c:563` `isfinite` guard, then `:365`-`:372` `CLAMP (value, lower, MAX (lower, upper - page_size))` | refuse, **stateful**; clamp when finite | the sanitised `min..=max` | `min` | `max` | `min` | clamp (parity) |
    /// | [`Slider::step`](Node::Slider) | `gtkadjustment.c:715`/`:760` `isfinite` guards; `gtkrange.c:1072` validates nothing else | refuse, **stateful**; unbounded when finite | `(0.0, max - min]` | [`DEFAULT_SLIDER_STEP_FRACTION`] of the span, or the **whole span** where that underflows | same | same | `<= 0.0` takes the same fallback; `> span` caps to the span |
    ///
    /// Four rows deserve their reasoning spelled out:
    ///
    /// - **`fraction`'s `NaN` is the only row that is a free choice on a
    ///   silent widget.** GTK saturates the infinities itself, so those two are
    ///   pure parity; `NaN` is the input `CLAMP` declines to decide, and `0.0`
    ///   — an empty bar — is the same "no reading" neutral #899 gave
    ///   `LedStripState::level`, and the one value that puts the `empty` class
    ///   back.
    /// - **A degenerate slider range falls back as a *unit*, and is never
    ///   swapped.** GTK never swaps: it refuses (`NULL`, or a `CRITICAL`
    ///   no-op). Swapping an inverted `10.0..=5.0` would silently reverse the
    ///   control's polarity — a plugin's transposed arguments turned into a
    ///   working-but-backwards slider. Collapsing to `min..=min` is no better:
    ///   `gtk_adjustment_get_bounded_upper` (`gtkadjustment.c:356`) then makes
    ///   the usable interval `MAX (lower, upper - page_size)` = `lower`, an
    ///   immovable thumb, and `gtk_scale_new_with_range` rejects a zero span
    ///   outright (`gtk/gtkscale.c:989`, `min < max`). The unit scale keeps the
    ///   slider *usable*, and mixing a stated end with a default one would
    ///   invent a scale nobody asked for. One consequence is worth naming:
    ///   replacing the scale also **relocates the value**, since the value is
    ///   then clamped against the fallback — `min: NaN, max: 5.0, value: 3.0`
    ///   draws a *full* slider, not a 60% one. That is unavoidable once the
    ///   stated scale is gone (there is nothing left to read `3.0` against),
    ///   and it is the same trade #899 made for a degenerate
    ///   [`GaugeRange`](crate::preem::GaugeRange).
    /// - **`value`'s infinities go to the ends, though GTK refuses all three
    ///   non-finite inputs alike.** That is this vocabulary's own rule, not a
    ///   parity claim — stated as such, exactly as #899 stated it for
    ///   [`GaugeState::target`](crate::preem::GaugeState). It is chosen so the
    ///   sanitiser is *continuous* at the boundary: a huge finite value and
    ///   `+inf` land in the same place, via the very `CLAMP` GTK applies to the
    ///   finite case one line later. `NaN` takes the low end, matching
    ///   `Progress`'s empty bar and `GaugeState::target`'s `range.low`.
    /// - **A non-positive `step` is replaced even though GTK accepts it.**
    ///   `gtk_range_set_increments` (`gtkrange.c:1072`) validates nothing, and
    ///   the result is a control no plugin can have meant: `step == 0.0` makes
    ///   the arrow keys dead (`step_back`/`step_forward` compute `value ∓ 0.0`,
    ///   `gtkrange.c:2570`/`:2583`) and `step < 0.0` silently **inverts** them.
    ///   Both are "legal but pathological", the class this crate's own rustdoc
    ///   already rejects.
    ///
    /// Order matters in one place, the same place it did in #899: the range is
    /// sanitised **before** the value and the step are measured against it.
    pub fn clamp_in_place(&mut self) {
        match self {
            Self::Progress { fraction, .. } => *fraction = sane_fraction(*fraction),
            Self::Slider {
                min,
                max,
                value,
                step,
                ..
            } => {
                let sane = sane_slider_floats(*min, *max, *value, *step);
                *min = sane.min;
                *max = sane.max;
                *value = sane.value;
                *step = sane.step;
            }
            // The preem vocabulary has its own mapping, derived from the kit
            // that rasterises it; delegating is what makes the invariant above
            // hold for a whole tree rather than for this file's two variants.
            Self::Preem { widget, .. } => widget.clamp_in_place(),
            Self::Box { children, .. }
            | Self::Row { children, .. }
            | Self::ListBox { children, .. } => {
                for child in children {
                    child.clamp_in_place();
                }
            }
            Self::Button { child, .. } | Self::Revealer { child, .. } => child.clamp_in_place(),
            Self::Expander {
                header, children, ..
            } => {
                header.clamp_in_place();
                for child in children {
                    child.clamp_in_place();
                }
            }
            // No float and no children — stated rather than defaulted, so a
            // float added to one of these later fails to compile here instead
            // of going unsanitised.
            Self::Label { .. }
            | Self::Text { .. }
            | Self::Icon { .. }
            | Self::Pixels { .. }
            | Self::Separator { .. }
            | Self::Spacer
            | Self::Entry { .. } => {}
        }
    }
}
