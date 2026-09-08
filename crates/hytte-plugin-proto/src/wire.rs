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

/// `serde(skip_serializing_if)` predicate for an additive scalar field whose
/// [`Default`] is the pre-existing behaviour — [`Node::Row`]'s `spacing` and
/// [`Node::ListBox`]'s `dense` (#966).
///
/// Keeping the default **off the wire** is what makes those two fields free in
/// both compat directions *and* leaves every already-committed golden fixture
/// byte-identical: a tree that sets neither encodes exactly the bytes it did
/// before the fields existed.
///
/// Deliberately **generic**. The obvious spelling — `fn(&u16) -> bool`,
/// `fn(&bool) -> bool` — trips `clippy::trivially_copy_pass_by_ref` (pedantic,
/// denied workspace-wide), which is the stated reason [`Node::Text`]'s
/// `ellipsize` carries no `skip_serializing_if` at all. A type parameter has no
/// known size, so the lint does not fire, and serde infers `T` at each call
/// site.
fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// The closed widget vocabulary. A plugin's view is a single root [`Node`].
///
/// Mirrors `hytte_ui::Node`: `Box { scroll }` carries the scroll flag
/// explicitly, so mapping to the GTK-side node is a trivial 1:1 in the host.
///
/// Appending a variant here ⇒ **bump [`VOCAB`](crate::VOCAB)** (#437): a plugin
/// renders these, so the counter is how an older host refuses one it can't decode.
///
/// # Tooltips
///
/// Seven variants carry an optional `tooltip: Option<String>`, which the host
/// maps straight onto `gtk::Widget::set_tooltip_text` (#957). It is the
/// vocabulary's only self-explanation primitive: a bar chip is a handful of
/// glyphs with nowhere to say what they mean, and a plugin that is deliberately
/// panel-less (the claude-bridge chip, #866) has no other surface to put the
/// legend on.
///
/// - It is a **mutable prop**: a same-id re-render with a different string
///   retitles the widget in place, and a re-render dropping it back to `None`
///   **clears** the tooltip rather than leaving the last one stuck.
/// - It is **plain text**, not Pango markup — the host calls `set_tooltip_text`,
///   so `<b>` arrives as four literal characters. (A plugin's tree is untrusted
///   input; `set_tooltip_markup` would hand it a parser.)
/// - It is **not** part of a node's identity: only `kind` and `id` decide reuse,
///   so changing a tooltip never rebuilds a widget.
/// - **Seven** of the seventeen variants, not all of them — the ones a chip or a
///   list card is made of: [`Box`](Node::Box), [`Label`](Node::Label),
///   [`Icon`](Node::Icon) and [`Shader`](Node::Shader) (#893, a picture with
///   nowhere else to say what it is), then — since #961 — [`Row`](Node::Row),
///   [`Text`](Node::Text) and [`Expander`](Node::Expander). Extending the set
///   again is the same additive move each of these was.
///
/// ## Where the host arms it (#961)
///
/// On the widget the node itself materializes as, with **one** exception:
/// [`Expander`](Node::Expander) arms it on the **header button**, not on the
/// outer box that also holds the revealed body. An expander's tooltip explains
/// the row the pointer clicks; hanging it on the outer box would float that
/// legend over the expanded body too, including over children that deliberately
/// carry none of their own. The header is what the pointer is over when it
/// hovers "the row", and it is already the click target.
///
/// GTK resolves a hover against the deepest widget under the pointer and walks
/// **up** until one answers, so a child's own tooltip always wins over its
/// container's: a [`Row`](Node::Row) tooltip explains the whole row *except*
/// wherever a child says something more specific.
///
/// ## An ellipsized [`Text`](Node::Text) tooltips itself (#961)
///
/// [`Text`](Node::Text) is the one variant with a **default**. When
/// `ellipsize` is `true` and the node sets no explicit `tooltip`, the host uses
/// the node's **full `text`** as the hover — which is the whole point of the
/// property: an ellipsized label truncates with `…` and the reader otherwise
/// has no way to see the rest. An explicit `tooltip` always wins, and a `Text`
/// that does not ellipsize gets no tooltip unless it asks for one.
///
/// The host does **not** check whether the label is *actually* truncated:
/// `pango_layout_is_ellipsized` is a function of the allocation, which changes
/// with every resize, so gating on it would mean a per-label allocation hook
/// re-deciding the tooltip on every layout pass. The cost of not gating is one
/// redundant hover on a short string in a wide-enough container, which reads as
/// a legend rather than as a bug; set `tooltip: Some(…)` — or leave `ellipsize`
/// off — where even that is unwanted.
///
/// **Additive, so [`PROTO_VERSION`](crate::PROTO_VERSION) and
/// [`VOCAB`](crate::VOCAB) both stay put** — the crate root's compat rules put an
/// optional field on the same-version side ("Adding an **optional field** to a
/// struct (carry `#[serde(default)]`, and `#[serde(skip_serializing_if = …)]`
/// where it should stay off the wire)"), and the [`VOCAB`](crate::VOCAB) rule is
/// scoped to *appending a wire variant*, which this is not. Both directions hold
/// concretely: a pre-#957 frame carries no `tooltip` key and `#[serde(default)]`
/// decodes it to `None`, and a new frame's key is skipped by an older host's
/// decoder (named-map encoding, no `deny_unknown_fields`) — so a new plugin
/// against an old shell renders exactly as it did before, with no tooltip,
/// instead of breaking the session. There is no #437 crash-loop hazard here
/// because an unknown *field* is skipped where an unknown *variant* would fail
/// the whole decode. Pinned by `tests/proto.rs` in both directions, and by the
/// `tests/fixtures/plugin_render_v1.hex` golden bytes, which did not move: the
/// `skip_serializing_if` keeps a `None` tooltip off the wire entirely.
///
/// # Laying out a list card (#966)
///
/// A real list card — Mara's live test of the agents plugin (#963) — hit three
/// walls at once, all of them host-side, all of them things the plugin could not
/// work around from its side of the socket:
///
/// - **[`Row`](Node::Row) had no `spacing`**, so its children butted up against
///   each other (`⚙argus`) unless the plugin padded with [`Spacer`](Node::Spacer)s.
///   Fixed by [`Row::spacing`](Node::Row#structfield.spacing), an additive
///   `u16` (`0` = the old behaviour), mapped to `gtk_box_set_spacing`.
///   [`Box`](Node::Box) already carried a `spacing`, so it is untouched.
/// - **[`ListBox`](Node::ListBox) auto-wraps every child** in a `GtkListBoxRow`,
///   which carries libadwaita's row min-height — most of the ~700 px a 12-row
///   card took. Fixed by [`ListBox::dense`](Node::ListBox#structfield.dense), an
///   additive `bool` that has the host drop that floor.
/// - **There was no viewport at all.** See the next section.
///
/// # `Box { scroll }` is an event target, **not** a viewport
///
/// [`Box::scroll`](Node::Box#structfield.scroll) makes the box a *source of
/// scroll events*: the host attaches a `GtkEventControllerScroll` and forwards
/// raw wheel deltas as [`EventKind::Scroll`], for a plugin that wants to treat
/// the wheel as an input (step a value, page a list it re-renders itself). It
/// **does not clip, does not scroll, and does not bound the box's height** — the
/// box still measures and renders at its children's full natural size.
///
/// That is the confusion #966 came from, and it was unfixable plugin-side: GTK
/// CSS has no `max-height`, so a plugin could not bound its own card by any
/// combination of the vocabulary it had. [`Scrolled`](Node::Scrolled) is the
/// viewport half; `scroll` keeps its existing meaning unchanged.
///
/// # Compat class of the three
///
/// `spacing` and `dense` are **optional fields**, so
/// [`PROTO_VERSION`](crate::PROTO_VERSION) *and* [`VOCAB`](crate::VOCAB) both
/// stay put — the crate root's rules put an optional field
/// (`#[serde(default)]` + `#[serde(skip_serializing_if = …)]`) on the
/// same-version side, and the `VOCAB` rule is scoped to *appending a wire
/// variant*. Their `skip_serializing_if` ([`is_default`]) keeps the pre-#966
/// value off the wire entirely, which is why every committed golden fixture that
/// predates them is byte-identical.
///
/// [`Scrolled`](Node::Scrolled) **is** an appended variant, so it bumps
/// [`VOCAB`](crate::VOCAB) (3 → 4) and is **negotiated** on
/// [`SCROLLED_VOCAB`] exactly as #882's preem and #893's shader generations are:
/// a plugin emits it only after the host advertised that generation in
/// [`HostMsg::Hello`](crate::msg::HostMsg::Hello), so an old host can never
/// receive a variant it cannot decode and
/// [`VOCAB_UNCONDITIONAL`](crate::VOCAB_UNCONDITIONAL) stays at 1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Node {
    /// A `gtk::Box`. `id` (optional) keys the node for diffing/reordering;
    /// `scroll` independently makes it a scroll event target.
    ///
    /// A `Box` is the usual place to hang a [`tooltip`](Node#tooltips): a chip's
    /// root box covers the whole pill, so one string explains every glyph inside
    /// it without the plugin having to guess which child the pointer is over.
    Box {
        id: Option<NodeId>,
        dir: Dir,
        /// Inter-child gap in pixels (`gtk_box_set_spacing`). `Box` has carried
        /// this since v1 — [`Row`](Node::Row) is the one that grew it in #966.
        spacing: i32,
        /// Make the box a **scroll event target**: the host attaches an
        /// event controller and forwards wheel deltas as
        /// [`EventKind::Scroll`].
        ///
        /// **This is not a viewport.** It neither clips nor scrolls nor bounds
        /// the box's height; the box still measures at its children's full
        /// natural size. For an inner viewport that clips and scrolls, use
        /// [`Scrolled`](Node::Scrolled) — see
        /// [the section on this enum](Node#box--scroll--is-an-event-target-not-a-viewport).
        scroll: bool,
        classes: Vec<Cls>,
        children: Vec<Node>,
        /// Hover text for the whole box, or `None` for no tooltip. See the
        /// [tooltip section](Node#tooltips) on this enum for the semantics and
        /// the compat argument.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tooltip: Option<String>,
    },
    /// A horizontal list **row** — a semantic sibling of [`Box`](Node::Box) for
    /// list-y cards (planned additive in the spec's node vocab, #199). Children
    /// are modelled exactly like `Box`'s and laid out left-to-right; style via
    /// `classes`. The host materializes it as a horizontal `gtk::Box`.
    Row {
        id: Option<NodeId>,
        classes: Vec<Cls>,
        /// Inter-child gap in pixels, mapped straight onto the backing
        /// `gtk::Box`'s `spacing` (#966). A **mutable prop**: a same-id
        /// re-render with a new value re-spaces the row in place rather than
        /// rebuilding it.
        ///
        /// `0` — the default, and what a pre-#966 frame decodes to — is exactly
        /// the old flush layout, so nothing that did not ask for spacing moves.
        /// `u16` rather than `Box`'s `i32` because a negative gap is not a thing
        /// a plugin can mean; the host widens it to `i32` at the map.
        #[serde(default, skip_serializing_if = "is_default")]
        spacing: u16,
        children: Vec<Node>,
        /// Hover text for the whole row, or `None` for no tooltip (#961). See
        /// the [tooltip section](Node#tooltips) on this enum.
        ///
        /// This is what a list card wants: one legend per row, on the row. A
        /// child that says something more specific still wins the hover, so a
        /// row tooltip is a fallback rather than a blanket. Before this field
        /// the only way to get hover text on a row was to spell it as a
        /// horizontal `Box` instead, which is two spellings of a row in one
        /// file (#963).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tooltip: Option<String>,
    },
    /// A vertical list **container** stacking its children (typically
    /// [`Row`](Node::Row)s) top-to-bottom — the list-y counterpart to
    /// [`Box`](Node::Box). The host materializes it as a selection-less list
    /// surface; style the list chrome via `classes`.
    ListBox {
        id: Option<NodeId>,
        classes: Vec<Cls>,
        /// Drop the per-row height floor (#966).
        ///
        /// The host materializes a `ListBox` as a real `GtkListBox`, which
        /// **auto-wraps every child** in a `GtkListBoxRow` so libadwaita's
        /// `.boxed-list` card styling can paint. That wrapper carries the
        /// theme's row min-height and padding, which is most of the height a
        /// dense list of one-line rows takes — a plugin cannot reach the
        /// wrapper (it never appears in the tree it sent), so this is the only
        /// way to ask for a tight list.
        ///
        /// `true` has the host mark each wrapper so the shipped stylesheet
        /// zeroes its `min-height` and `padding`; the rows are then exactly as
        /// tall as their content. A **mutable prop**: flipping it on a same-id
        /// re-render re-marks the existing wrappers in place, both directions.
        ///
        /// Style the *rows'* own padding from the plugin as usual — `classes`
        /// on the [`Row`](Node::Row) children — which is the point: dense hands
        /// the height budget back to the plugin instead of spending it in the
        /// wrapper.
        #[serde(default, skip_serializing_if = "is_default")]
        dense: bool,
        children: Vec<Node>,
    },
    /// A **bounded viewport**: a vertical scroller that is as tall as its child
    /// wants to be, up to `max_height`, and scrolls the rest (#966).
    ///
    /// This is the vocabulary's only way for a plugin to bound its own card.
    /// GTK CSS has no `max-height`, and
    /// [`Box::scroll`](Node::Box#structfield.scroll) is an *event target* that
    /// neither clips nor scrolls — so before this variant a long list inside a
    /// card grew without limit and pushed everything below it off the surface.
    ///
    /// The host materializes it as a `GtkScrolledWindow` with
    /// `propagate-natural-height`, a **vertical-only** policy (never / automatic)
    /// and overlay scrolling, so:
    ///
    /// - a child **shorter** than `max_height` is not stretched — the node is
    ///   invisible in that case, which is what lets a plugin wrap a list whose
    ///   length it does not know;
    /// - a child **taller** than `max_height` is clipped to it and scrollable;
    /// - the child's natural **width** is propagated unchanged (no horizontal
    ///   scrollbar, no horizontal clipping).
    ///
    /// `max_height` is a **mutable prop** (a same-id re-render re-bounds the
    /// viewport in place). `0` means *unbounded* — the node becomes a
    /// pass-through wrapper — so a plugin can compute the cap and hand over `0`
    /// rather than branching on whether to emit the node at all.
    ///
    /// # Nesting: the innermost viewport wins
    ///
    /// A `Scrolled` inside a scrolling surface (the shell's sidebar, #965) takes
    /// the wheel while the pointer is over it, and hands the gesture back to the
    /// surface once its own adjustment is at the end — that is `GtkScrolledWindow`'s
    /// standard kinetic chaining, not something the host adds. So a bounded card
    /// scrolls inside itself first, and the sidebar scrolls once the card is at
    /// its end.
    ///
    /// # Negotiated (#966)
    ///
    /// Appending this variant bumps [`VOCAB`](crate::VOCAB) to
    /// [`SCROLLED_VOCAB`], and — like [`Preem`](Node::Preem) and
    /// [`Shader`](Node::Shader) — a plugin must only emit it once the host has
    /// advertised that generation in
    /// [`HostMsg::Hello`](crate::msg::HostMsg::Hello). The fallback is trivial
    /// and lossless in kind: render the child on its own, unbounded, which is
    /// exactly what every pre-#966 card did. The SDK's
    /// `hytte_plugin::nodes::scrolled` does that branch for you.
    ///
    /// **The bump costs an old shell nothing**, measured against the real
    /// `origin/main` proto crate linked alongside this one (#969 review):
    /// [`Manifest::new`](crate::manifest::Manifest::new) stamps
    /// `vocab = VOCAB_UNCONDITIONAL = 1`, which is the number an old host
    /// exact-checks, so a plugin rebuilt on this SDK still registers cleanly;
    /// that host then advertises 3, `negotiated_vocab` yields 3, and the variant
    /// is never emitted. An old host also silently skips `Row::spacing` and
    /// `ListBox::dense` as unknown keys. What the negotiation buys is visible in
    /// the one case that bypasses it: a `Scrolled` put on the wire regardless
    /// makes an old decoder fail with ``unknown variant `Scrolled` `` — the
    /// whole frame, i.e. #437's 5 s reconnect loop.
    ///
    /// # Why a variant, and not a `max_height` field on [`Box`](Node::Box)
    ///
    /// A field would have been additive and bumped nothing, so it was the
    /// default answer; two things ruled it out.
    ///
    /// **It is not expressible without changing the reconciler's
    /// `update_in_place` contract.** A host's retained widget is simultaneously
    /// the one its parent container appends/removes/reorders and the one
    /// `update_in_place` downcasts; wrapping a box in a scroller splits those
    /// into two objects, and flipping the field on a *reused* node would have to
    /// swap which of them is parented — which `update_in_place` cannot do,
    /// because it holds no handle on the parent, and there are five different
    /// parent shapes. That is a contract change, not an impossibility: folding
    /// the flag into the reconciler's kind discriminant so a flip rebuilds would
    /// work, at the cost of the discriminant no longer being a variant
    /// discriminant.
    ///
    /// **The decisive reason is the other repair.** Making the flip sound by
    /// always wrapping *every* [`Box`](Node::Box) in a scroller would, for every
    /// already-deployed plugin and for a field almost none of them set:
    ///
    /// - rename the styled widget's GTK CSS node from `box` to
    ///   `scrolledwindow`, so every `box.foo` selector silently stops matching
    ///   (the host applies a node's `classes` to exactly that widget);
    /// - drop every box's **minimum** height to near zero, since that is a
    ///   `GtkScrolledWindow`'s own minimum — changing how existing cards behave
    ///   under pressure;
    /// - put a scroll-**consuming** widget in front of
    ///   [`Box::scroll`](Node::Box#structfield.scroll)'s controller, i.e. break
    ///   the one meaning that flag has.
    ///
    /// A separate variant costs an old shell nothing (above) and names the thing
    /// — which, given #966 opened on `scroll` being mistaken for a viewport, is
    /// a real benefit rather than the justification.
    Scrolled {
        id: Option<NodeId>,
        /// The viewport's maximum height in pixels; `0` = unbounded.
        max_height: u16,
        classes: Vec<Cls>,
        /// The single child the viewport bounds.
        child: Box<Node>,
    },
    /// A `gtk::Label`.
    Label {
        id: Option<NodeId>,
        text: String,
        classes: Vec<Cls>,
        /// Hover text, or `None` for no tooltip. See the
        /// [tooltip section](Node#tooltips) on this enum.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tooltip: Option<String>,
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
        /// Hover text, or `None` (#961).
        ///
        /// **`None` is not "no tooltip" here.** When `ellipsize` is `true` and
        /// this is `None`, the host uses the node's full `text` as the hover —
        /// the truncated-string default, which is the case the property exists
        /// for. A value set here always wins; when `ellipsize` is `false` and
        /// this is `None`, the label gets no tooltip at all. See the
        /// [tooltip section](Node#tooltips) on this enum for why the host does
        /// not gate the default on *actual* truncation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tooltip: Option<String>,
    },
    /// A `gtk::Image` set from a themed icon `name` (name only — never pixels).
    ///
    /// The name is resolved against the host's `GtkIconTheme`, so it must be one
    /// the *shell* can find: an Adwaita symbolic (`emblem-ok-symbolic`, …) or one
    /// of the shell's own bundled icons, whose directory the shell puts on the
    /// theme's search path (`claude-symbolic`, `cpu`, `memory`, …). An
    /// unresolvable name renders as `image-missing`.
    Icon {
        id: Option<NodeId>,
        name: String,
        classes: Vec<Cls>,
        /// Hover text, or `None` for no tooltip — the load-bearing case for an
        /// icon, which otherwise carries no words at all. See the
        /// [tooltip section](Node#tooltips) on this enum.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tooltip: Option<String>,
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
        /// Hover text for the **header**, or `None` for no tooltip (#961).
        ///
        /// Armed on the header button rather than on the whole expander, so an
        /// expanded body does not inherit the header's legend — see
        /// [where the host arms it](Node#where-the-host-arms-it-961).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tooltip: Option<String>,
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
    /// **ordinal among the un-id'd preem nodes** of that tree, and the node
    /// still renders. That fallback is only stable while those nodes keep their
    /// order *and* their count —
    /// inserting or removing an anonymous sibling shifts every later one down a
    /// slot, and because interchangeable widgets have identical configs by
    /// construction the host cannot tell the difference and updates the survivor
    /// in place: the third gauge renders the second's needle, a phosphor history
    /// moves onto another signal, a variable-length row of per-core meters
    /// glitches on every insert.
    ///
    /// The host says so in its log — but only **once per plugin tree for the
    /// life of the shell process**, deliberately, so a node that appears and
    /// disappears cannot turn the diagnostic into a per-frame stream. It is not
    /// once per plugin *session*: a plugin restarted under the same id will not
    /// produce a second line. Check the journal from the top of the shell's run.
    ///
    /// **Ids must be unique within a tree** (#918). Two preem nodes in one tree
    /// claiming the same `id` collapse onto one renderer instance, which then
    /// has both widgets applied to it every pass — two targets fighting one
    /// needle, or one phosphor, or one flip clock. The host keeps the last
    /// writer (nothing disappears) and warns once per plugin tree, naming the id
    /// and both widget kinds. A tree renders both trees' worth of nodes, so the
    /// namespace to be unique in is the tree, not the plugin: the same `"cpu"`
    /// in a plugin's chip and in its drawer panel is fine.
    ///
    /// The Rust SDK's `display` wrappers stamp the id from the widget key they
    /// already take (`display::gauge::node("cpu")`), so a plugin built on them
    /// never reaches the fallback. A hand-rolled client should do the same. Use
    /// [`preem_id`](crate::preem::preem_id) rather than
    /// [`preem`](crate::preem::preem) when constructing a node by hand.
    ///
    /// # How many of these a tree may carry (#901)
    ///
    /// [`MAX_PREEM_NODES_PER_TREE`] of them, within [`MAX_NODES_PER_TREE`] nodes
    /// and [`MAX_TREE_DEPTH`] levels of nesting overall — see those constants
    /// for the numbers, what they bound and what the host does past them.
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
        /// an ordinal key, warns once per tree per shell run, and animation
        /// state moves between siblings on any insert or removal.
        id: Option<NodeId>,
        classes: Vec<Cls>,
        widget: Box<PreemWidget>,
    },
    /// A **plugin-supplied fragment shader** over a **plugin-supplied data
    /// buffer**, drawn by the shell on the GPU (#893).
    ///
    /// This is the vocabulary's one open-ended widget. Every other variant names
    /// a thing the host knows how to draw; this one carries the drawing *code*.
    /// The plugin writes a fragment shader body and a data grid; the shell
    /// compiles the body **once**, keeps the linked program, and per frame
    /// re-uploads only [`data`](Node::Shader::data) and the uniforms. That
    /// asymmetry is the whole design: a shader widget's steady-state cost is its
    /// data buffer, not its source.
    ///
    /// # The interface contract (versioned — see [`SHADER_VOCAB`])
    ///
    /// **The plugin writes the fragment body only.** No `#version`, no
    /// `precision` declarations, no `in`/`out` declarations — the shell prepends
    /// its own header (`#version 320 es` plus the `highp` precision defaults —
    /// the same one its tree-owned shaders compile with) and a **preamble**
    /// declaring the interface below. A body that re-declares any of these is a
    /// duplicate-declaration compile error, which surfaces as the broken-widget
    /// placeholder.
    ///
    /// Guaranteed inputs, all `uniform` unless stated:
    ///
    /// | name | type | meaning |
    /// |---|---|---|
    /// | `v_uv` | `in vec2` | `0..1` across the drawn rect, origin bottom-left |
    /// | `u_time` | `float` | seconds since this surface's first frame, **wrapping every 3600** |
    /// | `u_resolution` | `vec2` | the drawn rect, in framebuffer pixels |
    /// | `u_scale` | `float` | the node's integer [`scale`](Node::Shader::scale) hint |
    /// | `u_data` | `sampler2D` | the data buffer, **nearest**-filtered, clamped |
    /// | `u_data_size` | `vec2` | `(data_width, data_height)`, in texels |
    /// | `u_bg` | `vec4` | the skin's screen field |
    /// | `u_fg` | `vec4` | the skin's **own** lit ink, un-tinted |
    /// | `u_accent` | `vec4` | the same ink as the desktop accent tints it — what an un-pinned preem widget draws with. Equal to `u_fg` when no accent is installed, or on a skin that declines to follow one |
    /// | `u_success`, `u_warning`, `u_error` | `vec4` | the status roles, admitted to be legible on the skin's ground (#940) |
    ///
    /// The single output is `out vec4 fragColor`, declared by the preamble and
    /// **not** by the body.
    ///
    /// # Colour: write **premultiplied** alpha
    ///
    /// `fragColor` is written verbatim into GTK's own framebuffer, which GSK
    /// imports as `GDK_MEMORY_DEFAULT` — premultiplied. So a half-transparent
    /// red is `vec4(0.5, 0.0, 0.0, 0.5)`, not `vec4(1.0, 0.0, 0.0, 0.5)`: scale
    /// the colour by the alpha yourself, as the six theme `vec4`s already are
    /// (they are opaque, so `rgb * a == rgb`). An `a` of `0` is a transparent
    /// pixel whatever the `rgb`, and fully-opaque output — what a chip normally
    /// wants, and what the bundled demo writes — needs no thought at all.
    ///
    /// The shell cannot convert for you: it never sees the colour, the shader
    /// writes it, and a second full-surface pass to premultiply would be a
    /// texture round trip to undo one multiply. Stated as a contract rather than
    /// discovered on glass — an earlier draft of these docs said "straight
    /// (non-premultiplied) alpha", which was the one clause here with neither a
    /// test nor a live-verify item behind it (#968 review M4).
    ///
    /// Which channel a texel lands in depends on
    /// [`format`](Node::Shader::format) — see [`ShaderData`].
    ///
    /// # When it repaints, and what `u_time` is
    ///
    /// **On state change, and only on state change.** The surface runs no frame
    /// clock of its own: `u_time` is sampled when a render happens, and a render
    /// happens when the plugin sends a frame whose `Shader` node differs from
    /// the one on screen (new `data`, new `fragment`, new size). So a shader
    /// animates at the rate its plugin pushes data, and a plugin that stops
    /// pushing leaves the last frame up rather than burning a GPU at 60 Hz for
    /// ever. A plugin that wants motion sends data on a timer.
    ///
    /// Two properties of that clock, both stated because a shader can see them:
    ///
    /// - **It wraps every 3600 s.** `u_time` is an `f32`, whose ulp grows with
    ///   magnitude — unwrapped seconds lose a quarter-second of resolution after
    ///   a month of uptime and a full second after three, so a shader animating
    ///   on it would judder and then freeze on a long-running desktop. Any
    ///   period that divides 3600 (a second, four seconds, a minute, ten
    ///   minutes) is continuous across the wrap; one that does not jumps once an
    ///   hour. Nothing checks this for you — pick a rate of `2π·k/3600` for a
    ///   sine, or drive motion from `fract(u_time / P)` with `P` dividing 3600.
    ///   The bundled reference shader
    ///   (`crates/hytte-plugin-preem-demo/shaders/spectrum.frag`) obeys the rule
    ///   in both of its animations and says so at each.
    /// - **It restarts when the widget is unmapped and remapped**, because the
    ///   surface's GL objects and its epoch go together and a remap is genuinely
    ///   a new first frame.
    ///
    /// # Sizing
    ///
    /// [`width`](Node::Shader::width) / [`height`](Node::Shader::height) are the
    /// widget's logical size in pixels and [`scale`](Node::Shader::scale) is the
    /// same integer upscale hint [`Pixels`](Node::Pixels) carries, with the same
    /// defaults (`0` and an absent key both mean `1`) and the same host-side
    /// clamp on an absurd scaled dimension. They are **not** the data grid:
    /// `data_width` × `data_height` size the texture independently, so a
    /// 16-texel spectrum can paint a 288×96 chip.
    ///
    /// # What the host enforces, and what it does not
    ///
    /// Settled on #893 and written down in
    /// `docs/superpowers/specs/2026-09-06-preem-gl-renderer-design.md`'s
    /// "Trust boundary for #893 — route 0": **the plugin socket is the
    /// boundary**. It is `0600` in a `0700` directory under `$XDG_RUNTIME_DIR`
    /// (see [`topology`](crate::topology)), so whoever can reach it already runs
    /// as the user, and a shader is exactly as trusted as the native code that
    /// plugin already runs.
    ///
    /// - **Enforced.** [`Capability::Shader`](crate::manifest::Capability::Shader)
    ///   must be in the manifest — an ordinary auto-granted capability, like the
    ///   rest — or the node renders the broken-widget placeholder with one
    ///   warning. [`MAX_SHADER_SOURCE_BYTES`] on the source and
    ///   [`MAX_SHADER_DATA_BYTES`] on the buffer, both refused to the same
    ///   placeholder, because they cost nothing. `data.len()` must equal
    ///   `data_width * data_height * format.bytes_per_texel()` — the invariant
    ///   [`Pixels`](Node::Pixels) already carries. A compile or link error draws
    ///   nothing and logs the driver's first info-log line once. A session whose
    ///   GL context failed renders the placeholder too — there is no CPU arm to
    ///   fall back to, which is what "GPU-only by design" costs.
    /// - **Not enforced, deliberately.** No source validator: naga cannot parse
    ///   GLSL ES at all (`#version 300/310/320 es` each fail `InvalidVersion` +
    ///   `InvalidProfile("es")`, measured twice independently), so route 1's
    ///   IR-expression and unbounded-loop caps had no enforcer and were dropped
    ///   rather than pretended. No provenance check on the socket peer — it
    ///   would only re-derive what the file mode already guarantees.
    /// - **Blast radius, plainly: the whole shell.** GTK requests no robust
    ///   context and there is one GL share group per display, so a shader that
    ///   hangs or resets the GPU takes every GL surface in the process with it,
    ///   and the realistic worst case is a shell restart. The upgrade path, if a
    ///   plugin is ever *not* trusted, is an out-of-process shader host
    ///   ("route 3") — named in the spec, not built.
    ///
    /// # Negotiated, like [`Preem`](Node::Preem)
    ///
    /// A plugin must not emit this on sight. It emits it only once the host has
    /// advertised [`SHADER_VOCAB`] or better in
    /// [`HostMsg::Hello`](crate::msg::HostMsg::Hello); an older host never
    /// advertises, so it can never receive a variant it cannot decode. There is
    /// no CPU form to degrade *to* — the widget is GPU-only by design (Annika,
    /// #893: "EGL should be available for all targets") — so a plugin whose host
    /// does not speak it renders something else, or nothing.
    ///
    /// One honest limit: declaring
    /// [`Capability::Shader`](crate::manifest::Capability::Shader) is **not**
    /// negotiated, because the manifest goes out before the host has said
    /// anything. A pre-#893 host cannot decode that variant and drops the
    /// connection. That is true of every capability ever appended, and a plugin
    /// built around a shader widget was never going to work on such a host.
    Shader {
        /// Optional reconciliation key (see [`NodeId`]). Recommended: without
        /// one the reconciler matches positionally, and a shader that changes
        /// place among its siblings rebuilds — which throws away the compiled
        /// program and restarts `u_time`.
        id: Option<NodeId>,
        /// The widget's logical width in pixels, before `scale`.
        width: u32,
        /// The widget's logical height in pixels, before `scale`.
        height: u32,
        /// Integer upscale hint, exactly [`Pixels`](Node::Pixels)'s: the natural
        /// size is `width*scale` × `height*scale`. Defaulted, so a frame
        /// omitting the key is 1×; `0` is treated as `1`.
        #[serde(default = "pixels_scale_default")]
        scale: u32,
        /// The fragment shader **body** — no `#version`, no interface
        /// declarations (see the variant docs). At most
        /// [`MAX_SHADER_SOURCE_BYTES`] bytes.
        fragment: String,
        /// The data buffer: `data_width * data_height * format.bytes_per_texel()`
        /// bytes, row-major from row 0. Rides the wire as one `MessagePack`
        /// `bin` blob (`serde_bytes`), like [`Pixels`](Node::Pixels)'s. At most
        /// [`MAX_SHADER_DATA_BYTES`] bytes.
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
        /// How to read [`data`](Node::Shader::data).
        format: ShaderData,
        /// The data grid's width, in texels.
        data_width: u32,
        /// The data grid's height, in texels. `1` for a 1-D buffer.
        data_height: u32,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<Cls>,
        /// Hover text — see the [tooltip section](Node#tooltips) on this enum.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tooltip: Option<String>,
    },
}

/// How the host reads a [`Node::Shader`]'s data buffer into the `u_data`
/// texture.
///
/// Three formats, chosen because they are the three shapes a plugin's data
/// actually has: a byte per cell, a colour per cell, and a float per cell.
/// All three are sampled with **nearest** filtering and `CLAMP_TO_EDGE` — a
/// data grid is data, and an interpolated read of it is a wrong answer rather
/// than a smoother one.
///
/// Appending a variant here ⇒ **bump [`VOCAB`](crate::VOCAB)** (#437), like any
/// other wire enum a plugin can put on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ShaderData {
    /// One unsigned byte per texel, sampled as `texture(u_data, uv).r` in
    /// `0.0..=1.0` (`GL_R8`). `g`/`b` read `0.0` and `a` reads `1.0`.
    R8,
    /// Four unsigned bytes per texel in `[R, G, B, A]` order, straight (not
    /// premultiplied) alpha, each sampled in `0.0..=1.0` (`GL_RGBA8`).
    ///
    /// **The data grid is straight; the output is premultiplied.** So
    /// `fragColor = texture(u_data, uv)` on a partially transparent `Rgba8`
    /// grid is the one composition the contract does not do for you — write
    /// `vec4(c.rgb * c.a, c.a)` instead. Fully opaque texels (the common case)
    /// pass through unchanged either way.
    Rgba8,
    /// One little-endian IEEE-754 `f32` per texel — four bytes — sampled
    /// verbatim in `.r`, unclamped (`GL_R32F`). The format for a signal that is
    /// not a colour: a spectrum, a waveform, a temperature.
    ///
    /// **Little-endian on the wire**, stated rather than assumed: the host
    /// decodes with `f32::from_le_bytes` and uploads native floats, so the
    /// buffer means the same thing whatever the plugin was built on.
    R32f,
}

impl ShaderData {
    /// How many bytes one texel of this format occupies in
    /// [`Node::Shader::data`].
    #[must_use]
    pub const fn bytes_per_texel(self) -> usize {
        match self {
            Self::R8 => 1,
            Self::Rgba8 | Self::R32f => 4,
        }
    }

    /// Whether `data_len` is exactly `width * height * bytes_per_texel` —
    /// computed in `u64` so the product cannot overflow.
    ///
    /// The [`Node::Shader`] analogue of the `len == w * h * 4` invariant
    /// [`Pixels`](Node::Pixels) carries, and enforced in the same place: the
    /// host, which is the trust boundary and the layer with `tracing`. A
    /// mismatch renders the broken-widget placeholder rather than handing GL a
    /// buffer shorter than the region it is told to read.
    ///
    /// `(0, 0, 0)` is consistent — a legitimately empty grid, which the host
    /// still refuses to *draw* (there is nothing to sample) but which is not a
    /// malformed frame.
    #[must_use]
    pub fn data_len_ok(self, width: u32, height: u32, data_len: usize) -> bool {
        let expected = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|n| {
                u64::try_from(self.bytes_per_texel())
                    .ok()
                    .and_then(|per| n.checked_mul(per))
            });
        expected == u64::try_from(data_len).ok()
    }
}

/// The [`VOCAB`](crate::VOCAB) generation that carries the shader widget
/// ([`Node::Shader`], [`ShaderData`],
/// [`Capability::Shader`](crate::manifest::Capability::Shader)) — #893.
///
/// **Negotiated**, exactly like [`PREEM_VOCAB`](crate::preem::PREEM_VOCAB): a
/// plugin emits [`Node::Shader`] only once
/// [`Manifest::negotiated_vocab`](crate::manifest::Manifest::negotiated_vocab)
/// has reached this number, so an old host — which advertises nothing in
/// [`HostMsg::Hello`](crate::msg::HostMsg::Hello) — can never receive a variant
/// it cannot decode. That is why generation 3 bumps
/// [`VOCAB`](crate::VOCAB) (the census) and leaves
/// [`VOCAB_UNCONDITIONAL`](crate::VOCAB_UNCONDITIONAL) alone; see that const
/// for the rule.
pub const SHADER_VOCAB: u16 = 3;

/// The [`VOCAB`](crate::VOCAB) generation that carries the bounded viewport
/// ([`Node::Scrolled`]) — #966.
///
/// **Negotiated**, exactly like [`SHADER_VOCAB`] and
/// [`PREEM_VOCAB`](crate::preem::PREEM_VOCAB): a plugin emits [`Node::Scrolled`]
/// only once
/// [`Manifest::negotiated_vocab`](crate::manifest::Manifest::negotiated_vocab)
/// has reached this number, so an old host — which advertises nothing in
/// [`HostMsg::Hello`](crate::msg::HostMsg::Hello) — can never receive a variant
/// it cannot decode. Generation 4 therefore bumps [`VOCAB`](crate::VOCAB) (the
/// census) and leaves [`VOCAB_UNCONDITIONAL`](crate::VOCAB_UNCONDITIONAL) alone;
/// see that const for the rule.
///
/// The degradation is the cheapest of the three so far: where preem falls back
/// to a CPU raster and a shader to [`Node::Pixels`], a plugin that cannot use a
/// viewport simply renders the child unwrapped — an unbounded card, i.e. exactly
/// what it rendered before #966.
pub const SCROLLED_VOCAB: u16 = 4;

/// The largest [`Node::Shader::fragment`] the host will hand a driver, in bytes.
///
/// **Hygiene, not security.** It is kept because it costs nothing and catches
/// the obvious mistake (a plugin accidentally shipping a megabyte of generated
/// GLSL); it is not a trust boundary, because the socket already is one — see
/// the [`Node::Shader`] docs. 16 KiB is the number the design spec named, and
/// it is roughly 400 lines of shader: two orders of magnitude above anything a
/// widget needs and far below anything that stalls a compile.
///
/// Enforced on **both** sides: the `hytte-plugin` SDK's builder refuses to
/// construct an over-cap node, and the host refuses to draw one (broken-widget
/// placeholder plus one warning), because an SDK-built plugin is not the only
/// thing that can dial the socket.
pub const MAX_SHADER_SOURCE_BYTES: usize = 16 * 1024;

/// The largest [`Node::Shader::data`] buffer the host will upload, in bytes.
///
/// 4 MiB — a 1024×1024 `Rgba8` grid, or a million floats — is far past any
/// widget-sized data set and comfortably inside
/// [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN)'s 16 MiB, so the frame limit is not
/// silently doing this cap's job. Same posture as
/// [`MAX_SHADER_SOURCE_BYTES`]: hygiene, enforced on both sides, degrading to
/// the placeholder rather than dropping the connection.
pub const MAX_SHADER_DATA_BYTES: usize = 4 * 1024 * 1024;

// ── tree-shape caps (#901) ───────────────────────────────────────────────────
//
// The caps in [`preem`](crate::preem) bound one widget's *geometry*, and the
// proto enforces them itself in `PreemWidget::clamp_in_place`. These three bound
// the *shape of a render tree*, and the proto cannot enforce them: it decodes a
// frame, it never walks one. They are stated here so a plugin author reads the
// bound in the same crate as everything else on the wire, and the host enforces
// them where it walks the tree (`trollshell/src/plugins/wire_map.rs`).

/// How many [`Node`]s the host maps out of one render tree, of every kind
/// together.
///
/// A render frame is bounded on the wire only by
/// [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN) (16 MiB), and the cheapest node
/// ([`Node::Spacer`]) encodes to a handful of bytes — so one legal frame can
/// carry on the order of a million nodes, each of which becomes a GTK widget in
/// the shell. That is a pre-existing stability hazard rather than a new one (a
/// plugin is native code running as the user, #881), but it is unbounded, and
/// nothing about it is diagnosable.
///
/// **4096** is [`MAX_SCOPE_SAMPLES`](crate::preem::MAX_SCOPE_SAMPLES) — the
/// vocabulary's existing "a large but finite count of small things" — and it is
/// `64 ×` [`MAX_PREEM_NODES_PER_TREE`], keeping the general cap and the preem
/// one in a stated ratio rather than two independently-chosen numbers. It is
/// two orders of magnitude above any tree the bundled plugins render (the
/// largest is a departures panel in the low hundreds), and it cuts the
/// worst-case widget count by roughly 400×.
///
/// **Past the cap the host keeps the mapped prefix and drops the rest**, rather
/// than refusing the frame: that is the posture the malformed-`Pixels` seam in
/// the same walk already takes (degrade to something that still renders and say
/// so, never blank the plugin), and a truncated tree shows the plugin's chrome
/// and its first nodes, so what is on glass matches the journal line. Refusing
/// the frame would leave the *previous* frame up, which looks exactly like a
/// hung plugin. The host warns once per plugin tree for the life of the shell
/// process, on the same latch as the [`Node::Preem`] keying diagnostics.
///
/// This caps the tree's **size**, not its shape: 4096 nodes in a single-child
/// chain are under this cap and 4096 levels deep. Nesting is capped separately
/// by [`MAX_TREE_DEPTH`], because that one is a stack bound and this one is not
/// small enough to serve as one.
pub const MAX_NODES_PER_TREE: usize = 4096;

/// How deeply the host will walk a render tree before it stops descending.
///
/// [`MAX_NODES_PER_TREE`] does not imply this. The host maps a tree by
/// recursing once per node on the **GTK main thread**, so a chain of
/// single-child containers turns nesting directly into stack frames — and a
/// 4096-node chain is well inside both the frame limit and the node cap.
/// Measured on the shipped stack (8 MiB): a mapping frame costs ~1.3 KiB in the
/// release profile the shell ships and ~6.5 KiB in a debug build, so a
/// node-cap-sized chain would spend 63 % of the main thread's stack in release
/// and overflow it outright at roughly a third of the cap in a debug build. Nor
/// is mapping the only recursion over the tree: the reconciler walks it again to
/// build and to diff, dropping it is recursive, and GTK measures, allocates and
/// snapshots the resulting widget nesting.
///
/// **64** is the same "generous, and still a small number" the rest of this
/// vocabulary means by it ([`MAX_CELLS`](crate::preem::MAX_CELLS),
/// [`MAX_DIVISIONS`](crate::preem::MAX_DIVISIONS),
/// [`MAX_PREEM_NODES_PER_TREE`]). Real trees nest a handful deep — a panel of
/// boxes inside rows inside an expander is under ten — so this is roughly six
/// times any plausible layout, while costing at most ~416 KiB of stack in the
/// expensive (debug) build: 5 % of the main thread, where the node cap alone
/// left none of it guaranteed.
///
/// Past the cap the host does what it does past [`MAX_NODES_PER_TREE`]: it keeps
/// what it has mapped, drops everything below, and warns once per plugin tree.
/// A container whose *only* child was dropped this way is dropped with it, so an
/// over-deep chain can cost a whole subtree rather than one leaf.
pub const MAX_TREE_DEPTH: usize = 64;

/// How many [`Node::Preem`] nodes in one tree the host gives a renderer
/// instance to.
///
/// [`MAX_NODES_PER_TREE`] is the bound; this is the *multiplier* it is applied
/// to. Every other node kind costs the host a GTK widget; a preem node costs a
/// widget **and** a renderer instance holding everything the vocabulary keeps
/// off the wire — phosphor buffer, needle velocity, flip clocks, scroll offset,
/// held peak, plus the cached RGBA frame. A ~40-byte config on the wire is
/// therefore a five- to six-order-of-magnitude memory amplifier, which is what
/// makes preem nodes worth a tighter cap than the tree as a whole.
///
/// **64** is the count this vocabulary already uses for "generous, and still a
/// small number" — [`MAX_CELLS`](crate::preem::MAX_CELLS),
/// [`MAX_DIVISIONS`](crate::preem::MAX_DIVISIONS),
/// [`MAX_TEXT_LINES`](crate::preem::MAX_TEXT_LINES),
/// [`MAX_PAD`](crate::preem::MAX_PAD) and
/// [`MAX_CORNER`](crate::preem::MAX_CORNER) are all 64. It is far above any
/// plausible design: a bar chip carries one to three preem nodes and a
/// dashboard panel a dozen. The shape that would reach it — one widget per CPU
/// core, per audio sink, per anything — is one the vocabulary already answers
/// with a single aggregate widget
/// ([`LedStrip`](crate::preem::PreemWidget::LedStrip) holds
/// [`MAX_LEDS`](crate::preem::MAX_LEDS) = 128 segments,
/// [`Scope`](crate::preem::PreemWidget::Scope) holds
/// [`MAX_SCOPE_SAMPLES`](crate::preem::MAX_SCOPE_SAMPLES) samples), so hitting
/// this cap is a sign to reach for those rather than a limit to raise.
///
/// **The memory this bounds.** An instance's resident cost is dominated by its
/// cached RGBA frame, capped at
/// [`MAX_RASTER_PIXELS`](crate::preem::MAX_RASTER_PIXELS) × 4 B = 16 MiB by the
/// geometry caps, so the adversarial worst case is 64 × ~16 MiB ≈ **1 GiB
/// resident** for a plugin that also maxes every dimension of every widget — a
/// bound, where there was none, not a comfortable number.
///
/// The **peak** is higher, and it is the number that decides whether the shell
/// survives: the host hands each frame to its widget tree by value, so a mapping
/// pass carries one more copy of every widget it maps, per tree per monitor,
/// until the reconciler has consumed it — ≥2 GiB on one monitor, ≥3 GiB on two,
/// before the surface takes its own. A realistic tree of 64 default-geometry
/// gauges is ~2 MiB resident and the same again in flight.
///
/// Past the cap a preem node renders as the host's unknown-widget placeholder
/// (an empty surface that keeps the node's `id` and classes, so CSS chrome
/// stays and a later in-cap frame updates it in place), with one warning per
/// plugin tree for the life of the shell process.
pub const MAX_PREEM_NODES_PER_TREE: usize = 64;

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
            Self::Button { child, .. }
            | Self::Revealer { child, .. }
            | Self::Scrolled { child, .. } => child.clamp_in_place(),
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
            // `Shader` sits here for the same reason `Pixels` does: it carries
            // no float. Its `fragment`/`data` limits are *size* checks and they
            // are the host's (`trollshell/src/plugins/shader_map.rs`), on the
            // same reasoning `Pixels`'s `len == w*h*4` check is host-side — the
            // host is the trust boundary and the layer with `tracing`.
            Self::Label { .. }
            | Self::Text { .. }
            | Self::Icon { .. }
            | Self::Pixels { .. }
            | Self::Shader { .. }
            | Self::Separator { .. }
            | Self::Spacer
            | Self::Entry { .. } => {}
        }
    }
}
