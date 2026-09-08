//! Declarative widget-tree → `gtk::Widget` reconciler.
//!
//! This is the decision-independent host-side foundation for the
//! out-of-process widget-plugin work (issues #195 / #35, "frontend B"). A
//! plugin describes its UI as an in-memory tree of [`Node`]s; the host hands
//! that tree to a [`Reconciler`], which diffs it against the previously
//! rendered tree and mutates real GTK widgets **in place** rather than
//! tearing the subtree down and rebuilding it.
//!
//! Everything about *how the tree arrives* — sockets, encoding, the plugin
//! supervisor, the capability/effect broker — is deliberately **out of
//! scope**. This module is pure in-memory `Node` → widgets with keyed
//! diffing and a single [`EventKind`] callback hook; the transport layer
//! will later drive [`Reconciler::render`] and wire the `on_event` callback
//! to outbound `Event` frames.
//!
//! # Keyed diffing
//!
//! The point of this over the naive "remove every child, rebuild from
//! scratch" pattern (see `trollshell`'s `widgets/workspaces.rs` /
//! `widgets/tray.rs`) is **identity preservation**: a child that carries a
//! stable [`NodeId`] is matched to its previous widget across renders, so a
//! reorder or a prop change reuses the existing widget (keeping focus,
//! animation state, and avoiding flicker) instead of destroying it.
//! Children *without* an id fall back to positional matching. The diff
//! decision is factored into the pure, GTK-free [`plan_diff`] so its
//! insert / remove / reorder / keyed-vs-positional behaviour is unit-tested
//! without a display server.
//!
//! # Example
//!
//! ```ignore
//! let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
//! let mut rec = Reconciler::new(&root, |id, kind| {
//!     // forward to the plugin transport later
//!     tracing::debug!(?id, ?kind, "widget event");
//! });
//!
//! rec.render(&Node::Box {
//!     id: None,
//!     dir: Dir::Horizontal,
//!     spacing: 4,
//!     scroll: false,
//!     classes: vec!["ts-plugin".into()],
//!     children: vec![Node::Label {
//!         id: Some("title".into()),
//!         text: "hello".into(),
//!         classes: vec![],
//!     }],
//! });
//! ```

use gtk::glib;
use gtk::prelude::*;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Stable, plugin-meaningful node identity. Doubles as the diff key and the
/// event target. A `Button` requires one; other nodes may omit it (and then
/// fall back to positional matching).
pub type NodeId = String;

/// Orientation for a [`Node::Box`], mapped to `gtk::Orientation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// Lay children out left-to-right (`gtk::Orientation::Horizontal`).
    Horizontal,
    /// Lay children out top-to-bottom (`gtk::Orientation::Vertical`).
    Vertical,
}

/// A user interaction the reconciler surfaces through the `on_event`
/// callback, tagged with the originating node's [`NodeId`]. (Not `Copy`:
/// [`Submitted`](EventKind::Submitted) carries its `String`.)
#[derive(Clone, Debug, PartialEq)]
pub enum EventKind {
    /// A [`Node::Button`] was clicked.
    Click,
    /// A [`Node::Box`] with `scroll: true` was scrolled. `dx`/`dy` are the
    /// raw GTK scroll deltas.
    Scroll {
        /// Horizontal scroll delta.
        dx: f64,
        /// Vertical scroll delta.
        dy: f64,
    },
    /// A [`Node::Slider`] was moved by the user (drag / scroll / keyboard).
    /// `value` is its new position, clamped to the slider's `min..=max`. Emitted
    /// on a trailing-edge throttle (never one per raw motion tick) and **only**
    /// for user-driven changes — a programmatic re-render that moves the thumb
    /// does not fire it (the reconciler wires `change-value`, not
    /// `value-changed`; see [`attach_slider`]).
    ValueChanged {
        /// The slider's new position, clamped to its `min..=max`.
        value: f64,
    },
    /// A [`Node::Entry`]'s text was submitted (the user pressed
    /// Enter/activate); `text` is the entry's full contents at that moment.
    /// Fired **only** for the user's activate — a programmatic `set_text`
    /// never emits GTK's `activate`, so a re-render echoing `text` back can't
    /// re-enter the event path (the entry analogue of the slider's
    /// `change-value` wiring). No per-keystroke event exists (v1 — see the
    /// wire vocab's rationale).
    Submitted {
        /// The entry's full contents at submit time.
        text: String,
    },
}

/// The closed widget vocabulary. A plugin tree is a single root `Node`.
///
/// `classes` are applied verbatim as GTK CSS classes (`add_css_class`); the
/// plugin is expected to use the existing `ts-*` / `hytte-*` token contract.
///
/// # Tooltips
///
/// [`Box`](Node::Box), [`Label`](Node::Label), [`Icon`](Node::Icon),
/// [`Shader`](Node::Shader) (#893) and — since #961 — [`Row`](Node::Row),
/// [`Text`](Node::Text) and [`Expander`](Node::Expander) carry an optional
/// `tooltip`, applied with `gtk::Widget::set_tooltip_text` (#957) — **plain
/// text, never markup**: a plugin tree is untrusted input, and
/// `set_tooltip_markup` would hand it a parser.
///
/// It is a **mutable prop**, reconciled centrally rather than per variant (see
/// [`node_tooltip`] and its two call sites in [`build_node`]/[`update_in_place`]),
/// so build and update cannot drift: a changed string retitles in place, and a
/// drop back to `None` **clears** the tooltip instead of leaving a stale one
/// stuck on the widget. It is not part of a node's identity — [`reusable`] keys
/// on kind and id only — so a tooltip change never rebuilds a widget.
///
/// Two refinements came with #961:
///
/// - **Which widget it is armed on** is [`tooltip_target`]'s answer, not always
///   the node's own widget: an [`Expander`](Node::Expander) arms it on the
///   header button, because the node's widget is the outer box that also holds
///   the revealed body.
/// - **[`Text`](Node::Text) has a default.** An ellipsizing `Text` with no
///   explicit tooltip gets its own full `text` as the hover, which is the only
///   way a reader can see what the `…` swallowed. [`node_tooltip`] is where
///   that derivation lives, so the snapshot in [`NodeDesc::tooltip`] records
///   the *effective* string and a later text change re-applies it. A derived
///   hover is a child's tooltip like any other, so inside an
///   [`Expander`](Node::Expander) header it wins over the header's own legend
///   — see the wire vocabulary's tooltip section for why that is left alone.
/// - **A blank tooltip arms nothing.** Empty *or* whitespace-only, derived or
///   explicit: [`node_tooltip`] filters, because GTK normalises `""` but not
///   `"   "` and would pop an empty tooltip window on hover.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// A `gtk::Box`. `id` (optional) keys the node for diffing/reordering;
    /// `scroll` independently controls whether it becomes a scroll event
    /// target (an `EventControllerScroll` forwarding raw deltas via
    /// [`EventKind::Scroll`]).
    Box {
        /// Optional diff/reorder key (see [`NodeId`]).
        id: Option<NodeId>,
        /// Child layout orientation.
        dir: Dir,
        /// Inter-child gap in pixels.
        spacing: i32,
        /// Make the box a scroll event target (emits [`EventKind::Scroll`]).
        scroll: bool,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
        /// Child nodes, diffed by key/position.
        children: Vec<Node>,
        /// Hover text (`set_tooltip_text`), `None` for none — see
        /// [the tooltip section](Node#tooltips).
        tooltip: Option<String>,
    },
    /// A list **row** — a horizontal `gtk::Box` sibling of [`Node::Box`] for
    /// list-y cards. Children are diffed exactly like a `Box`'s.
    Row {
        /// Optional diff/reorder key (see [`NodeId`]).
        id: Option<NodeId>,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
        /// Inter-child gap in pixels (`set_spacing`; mutable prop). `0` is the
        /// flush layout a `Row` had before #966.
        spacing: i32,
        /// Child nodes, diffed by key/position.
        children: Vec<Node>,
        /// Hover text (`set_tooltip_text`), `None` for none — see
        /// [the tooltip section](Node#tooltips). A child's own tooltip wins
        /// where it has one; this is the row's fallback legend.
        tooltip: Option<String>,
    },
    /// A vertical list **container** stacking its children (typically
    /// [`Node::Row`]s). Materialized as a real `gtk::ListBox`; children diff
    /// like a `Box`'s.
    ListBox {
        /// Optional diff/reorder key (see [`NodeId`]).
        id: Option<NodeId>,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
        /// Drop the auto-created `GtkListBoxRow` wrappers' theme height floor
        /// (mutable prop; #966) by marking each with
        /// [`DENSE_ROW_CLASS`](crate::widget_tree::DENSE_ROW_CLASS). See
        /// [`apply_dense_rows`].
        dense: bool,
        /// Child nodes (typically [`Node::Row`]s), diffed by key/position.
        children: Vec<Node>,
    },
    /// A **bounded viewport**: a `gtk::ScrolledWindow` that is as tall as its
    /// child up to `max_height`, and scrolls the rest (#966).
    ///
    /// Distinct from [`Node::Box`]'s `scroll`, which only makes a box a *source*
    /// of [`EventKind::Scroll`] and neither clips nor bounds anything.
    Scrolled {
        /// Optional diff/reorder key (see [`NodeId`]).
        id: Option<NodeId>,
        /// Maximum height in pixels (`set_max_content_height`; mutable prop).
        /// `0` (or negative) means unbounded — a pass-through wrapper.
        max_height: i32,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
        /// The viewport's single child node.
        child: Box<Node>,
    },
    /// A `gtk::Label`.
    Label {
        /// Optional diff/reorder key (see [`NodeId`]).
        id: Option<NodeId>,
        /// Label text (mutable prop: updated in place on a same-id re-render).
        text: String,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
        /// Hover text (`set_tooltip_text`), `None` for none — see
        /// [the tooltip section](Node#tooltips).
        tooltip: Option<String>,
    },
    /// A **wrapping** `gtk::Label` (word/char wrap): unlike [`Node::Label`] its
    /// natural width doesn't force its container wider — the fix for the pet's
    /// 320 px blow-out. `max_width_chars`, when `Some`, caps the natural width
    /// (`set_max_width_chars`); `None` leaves the wrap bounded by the container.
    /// When `ellipsize` is `true` the label instead runs **single-line** and
    /// truncates with a trailing ellipsis (`EllipsizeMode::End`) — the native
    /// departures-row look. `text`, `max_width_chars`, and `ellipsize` all update
    /// in place (a same-id re-render flips the flow mode without a rebuild).
    Text {
        /// Optional diff/reorder key (see [`NodeId`]).
        id: Option<NodeId>,
        /// Label text (mutable prop: updated in place on a same-id re-render).
        text: String,
        /// Cap on the natural width (`set_max_width_chars`); `None` leaves the
        /// wrap bounded by the container.
        max_width_chars: Option<i32>,
        /// Run single-line and truncate with a trailing ellipsis instead of
        /// wrapping (mutable prop).
        ellipsize: bool,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
        /// Hover text (`set_tooltip_text`), or `None` — which is **not** the
        /// same as "no tooltip": an `ellipsize: true` node with `None` here
        /// gets its own `text` as the hover, unless that text is blank (#961).
        /// See [the tooltip section](Node#tooltips) and [`node_tooltip`], which
        /// is where that default is applied.
        tooltip: Option<String>,
    },
    /// A `gtk::Image` set from a themed icon `name`.
    Icon {
        /// Optional diff/reorder key (see [`NodeId`]).
        id: Option<NodeId>,
        /// Themed icon name (mutable prop: swapped in place on a same-id
        /// re-render).
        name: String,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
        /// Hover text (`set_tooltip_text`), `None` for none — see
        /// [the tooltip section](Node#tooltips).
        tooltip: Option<String>,
    },
    /// A raster image: a `width`×`height` block of **RGBA8** pixels
    /// (`data`, row-major, 4 bytes/pixel `[R, G, B, A]`, non-premultiplied,
    /// length `width * height * 4`), materialized by a [`crate::pixels`]
    /// `PixelSurface` and scaled up with **nearest-neighbor** filtering for
    /// crisp "LCD"-style pixels. `scale` (#358) is an integer upscale hint:
    /// the surface's natural size becomes `width*scale` × `height*scale`
    /// (`0` means `1`), so a small buffer can request a crisp integer blow-up
    /// without a CSS px rule. `data` and `scale` are **mutable** props: a
    /// same-id re-render swaps the texture / natural size in place (like
    /// [`Node::Label`]'s `text`). An inconsistent buffer renders nothing (the
    /// widget is panic-safe); the upstream host validates and warns, and also
    /// clamps an absurd `scale` before it reaches this node.
    ///
    /// `data` is an [`Arc<[u8]>`](std::sync::Arc) rather than a `Vec<u8>`
    /// (#911) because one buffer is routinely shown on **several** surfaces:
    /// the shell rasterises a preem widget once per tick and maps the result
    /// once per monitor, and the host's own re-map is per monitor too. Sharing
    /// the allocation makes that fan-out a refcount instead of a full RGBA
    /// clone per monitor, and lets the reconciler hand it to
    /// [`PixelSurface::set_pixels_shared`](crate::pixels::PixelSurface::set_pixels_shared),
    /// whose guard is then an `Arc::ptr_eq` and whose upload adopts the
    /// allocation with no copy at all. A producer that owns its bytes converts
    /// once at the boundary (`Arc::from(&bytes[..])` — the same single copy a
    /// `Vec` clone was).
    Pixels {
        /// Optional diff/reorder key (see [`NodeId`]).
        id: Option<NodeId>,
        /// Buffer width in pixels.
        width: u32,
        /// Buffer height in pixels.
        height: u32,
        /// Row-major RGBA8 pixels, `width * height * 4` bytes (mutable prop).
        /// Shared: cloning this node, or mapping one frame onto a second
        /// monitor, costs a refcount rather than a copy.
        data: Arc<[u8]>,
        /// Integer upscale hint (`0` means `1`); natural size becomes
        /// `width*scale` × `height*scale` (mutable prop).
        scale: u32,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
    },
    /// A **GPU** surface: a [`crate::gl_surface::GlSurface`] running the
    /// host-registered shader pipeline named by `program`, over the plain-data
    /// uniforms in `state` (#893 stage B).
    ///
    /// The GPU sibling of [`Node::Pixels`], and the difference is where the
    /// pixels live. `Pixels` carries a rasterised RGBA8 buffer; this carries
    /// the **state** a shader rasterises from, and the frame never leaves the
    /// GPU. Rendering into an FBO and reading it back into an `Arc<[u8]>` would
    /// have reused the `Pixels` machinery and cost a pipeline stall per chip
    /// per frame, which is the whole thing #863 set out to remove.
    ///
    /// `width`/`height` are the **logical** natural size in pixels — for a
    /// preem widget, `cols * scale` by `rows * scale` — measured exactly as
    /// `Pixels` measures, with `state.grid` carrying the pre-upscale grid the
    /// offscreen passes run at. GTK allocates the framebuffer at that size
    /// times the integer `scale_factor` and the surface point-samples into it,
    /// which is `Pixels`' nearest-neighbour rule moved into a fragment shader.
    ///
    /// `program` and `state` are **mutable props**: a same-id re-render points
    /// the existing surface at the new state in place, and a state equal to the
    /// one already held costs nothing at all — the `Arc` is compared by pointer
    /// first, which is the case a frame mapped onto a second monitor hits.
    ///
    /// `state.step_seq` is what makes the draw idempotent; see the
    /// [`gl_surface`](crate::gl_surface) module docs, which is also where the
    /// CPU-fallback contract for a failed context lives.
    GlSurface {
        /// Optional diff/reorder key (see [`NodeId`]).
        id: Option<NodeId>,
        /// Natural width in logical pixels.
        width: u32,
        /// Natural height in logical pixels.
        height: u32,
        /// Which registered pipeline draws it (mutable prop).
        program: crate::gl_surface::GlProgram,
        /// The uniforms, data strip, grid and step count (mutable prop).
        /// Shared, so mapping one frame onto a second monitor costs a refcount
        /// and settles on an `Arc::ptr_eq`.
        state: Arc<crate::gl_surface::GlUniforms>,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
    },
    /// A **plugin-supplied** shader surface: a
    /// [`ShaderSurface`](crate::shader_surface::ShaderSurface) running a
    /// fragment body the host compiles once and then feeds a data buffer
    /// (#893).
    ///
    /// The third member of the `Pixels` / `GlSurface` / `Shader` family, and it
    /// is deliberately its **own kind** rather than a mode of either. Against
    /// [`Pixels`](Node::Pixels): the pixels are computed on the GPU from state,
    /// not carried as bytes. Against [`GlSurface`](Node::GlSurface): the shader
    /// arrives at runtime instead of naming a host-registered pipeline, so the
    /// two widgets have different resources, a different cache and a different
    /// failure mode. Reusing one widget for the other node would put a
    /// `downcast` `expect` — the kind invariant — one id collision away from an
    /// abort.
    ///
    /// `width`/`height` are the **logical** natural size in pixels (the wire's
    /// size times its `scale` hint), measured exactly as `Pixels` measures, and
    /// `state.data_size` is the independent data grid.
    ///
    /// `state` is a **mutable prop**: a same-id re-render points the existing
    /// surface at the new state in place, keeping the compiled program, and a
    /// state equal to the one already held costs nothing at all.
    Shader {
        /// Optional diff/reorder key (see [`NodeId`]). Recommended: without one
        /// a re-order rebuilds the widget, which throws the compiled program
        /// away and restarts `u_time`.
        id: Option<NodeId>,
        /// Natural width in logical pixels.
        width: u32,
        /// Natural height in logical pixels.
        height: u32,
        /// The source, the data buffer and the theme bag (mutable prop).
        ///
        /// **Shared**: mapping one frame onto a second monitor costs a refcount
        /// and settles on an `Arc::ptr_eq`, both in `set_state`'s dedup and in
        /// the surface's data-upload guard. That is a property of the
        /// *producer* — `trollshell`'s `shader_map` caches one `Arc` per node
        /// per frame — not something this type can enforce; before #968's
        /// review it was claimed here and not true anywhere.
        state: Arc<crate::shader_surface::ShaderState>,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
        /// Hover text (mutable prop; `None` clears it) — see
        /// [`node_tooltip`](Node#tooltips).
        tooltip: Option<String>,
    },
    /// A `gtk::Button`. `id` is **required** — it is the click event target.
    Button {
        /// **Required** diff key and [`EventKind::Click`] target.
        id: NodeId,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
        /// The button's single child node (its label/content).
        child: Box<Node>,
    },
    /// A `gtk::ProgressBar`, `fraction` in `0.0..=1.0`.
    Progress {
        /// Optional diff/reorder key (see [`NodeId`]).
        id: Option<NodeId>,
        /// Fill fraction in `0.0..=1.0` (mutable prop).
        fraction: f64,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
    },
    /// An interactive horizontal `gtk::Scale` — the writable counterpart to
    /// [`Node::Progress`]. `id` is **required** (like [`Node::Button`]): it is
    /// the [`EventKind::ValueChanged`] target. `min`/`max`/`step`/`value` set the
    /// range, keyboard/scroll increment, and initial position; the host draws no
    /// value label (style via `classes`) and makes it `hexpand`.
    ///
    /// `value` is a **mutable prop** updated in place — but suppressed while the
    /// user is actively dragging, so a plugin echoing the value back can't fight
    /// the grab (see [`update_in_place`]). Events are wired via the `change-value`
    /// signal (user-only) rather than `value-changed`, so a programmatic
    /// `set_value` never re-enters the event path — the `bind_two_way`
    /// feedback-loop problem, avoided structurally.
    ///
    /// `enabled` maps to `set_sensitive`: `false` greys the scale and stops it
    /// taking input (so an insensitive slider fires no [`EventKind::ValueChanged`]).
    /// A mutable prop — a same-id re-render flips sensitivity in place.
    Slider {
        /// **Required** diff key and [`EventKind::ValueChanged`] target.
        id: NodeId,
        /// Range lower bound.
        min: f64,
        /// Range upper bound.
        max: f64,
        /// Current position (mutable prop; suppressed while the user drags).
        value: f64,
        /// Keyboard/scroll increment.
        step: f64,
        /// Whether the scale accepts input (`set_sensitive`; mutable prop).
        enabled: bool,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
    },
    /// A `gtk::Revealer`; `open` drives `set_reveal_child`.
    Revealer {
        /// Optional diff/reorder key (see [`NodeId`]).
        id: Option<NodeId>,
        /// Whether the child is revealed (`set_reveal_child`; mutable prop).
        open: bool,
        /// The revealer's single child node.
        child: Box<Node>,
    },
    /// A `gtk::Separator`.
    Separator {
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
    },
    /// An **expanding gap**: an empty, style-less `gtk::Box` with `hexpand` and
    /// `vexpand` set, so it soaks up a container's slack and justifies its
    /// siblings (`Label + Spacer + Label` right-pins the trailing label in a
    /// [`Node::Row`]). Carries no id and no children — purely structural. Both
    /// axes expand so the one node works in a horizontal *or* vertical parent
    /// without knowing its orientation; the cross-axis expand is inert (an empty
    /// box has zero natural size). Consecutive spacers reuse by kind.
    Spacer,
    /// A collapsible **expander row** — the analogue of `AdwExpanderRow` (#333).
    /// Materialized as a flat, full-width header (`gtk::Button` wrapping `header`,
    /// with a trailing, dimmed disclosure chevron) above a `gtk::Revealer` holding
    /// `children` stacked vertically. Clicking the header fires
    /// [`EventKind::Click`] addressed by `id` (like [`Node::Button`]); the plugin
    /// flips its own `expanded` and re-renders — the host never self-toggles, so
    /// there is no hidden host state to desync. `expanded` is a **mutable prop**: a
    /// same-id re-render reveals/hides the body and swaps the chevron
    /// (`pan-end` ⇄ `pan-down`) in place without a rebuild. `id` is **required** —
    /// it is the click target.
    Expander {
        /// **Required** diff key and header-click [`EventKind::Click`] target.
        id: NodeId,
        /// The always-visible header node (wrapped in the clickable button).
        header: Box<Node>,
        /// Body nodes revealed when expanded, stacked vertically.
        children: Vec<Node>,
        /// Whether the body is revealed (mutable prop; the plugin owns the
        /// toggle).
        expanded: bool,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
        /// Hover text for the **header button**, `None` for none (#961) — not
        /// for the outer box, which also holds the revealed body. See
        /// [the tooltip section](Node#tooltips) and [`tooltip_target`].
        tooltip: Option<String>,
    },
    /// A single-line text input — a `gtk::Entry` (#357). `id` is **required**
    /// (like [`Node::Button`]): it is the [`EventKind::Submitted`] target,
    /// fired when the user presses Enter/activate with the entry's full text.
    ///
    /// `text` is the **echo prop**: applied on build, and on update **only
    /// when the prop changed since the last render** — a re-render that merely
    /// echoes the unchanged value leaves the widget alone, so in-progress user
    /// typing is never clobbered (the entry analogue of [`Node::Slider`]'s
    /// drag suppression), while a real prop change (clear-after-submit,
    /// prefill) still applies even while focused. `placeholder` is the greyed
    /// empty-state hint (`""` for none); both are mutable props. Events are
    /// wired on GTK's `activate` (user-only; a programmatic `set_text` never
    /// fires it), so there is no echo/feedback loop to break.
    Entry {
        /// **Required** diff key and [`EventKind::Submitted`] target.
        id: NodeId,
        /// Echo prop: applied on build and on update only when it changed
        /// since the last render, so in-progress typing is never clobbered.
        text: String,
        /// Greyed empty-state hint (`""` for none; mutable prop).
        placeholder: String,
        /// GTK CSS classes applied verbatim (`add_css_class`).
        classes: Vec<String>,
    },
}

/// Boxed event callback shared (`Rc`) into every widget's signal handler.
type EventFn = Rc<dyn Fn(NodeId, EventKind)>;

/// Drives a `gtk::Box` from a declarative [`Node`] tree.
///
/// `render` is idempotent against an unchanged tree (it diffs, finds no
/// changes, and touches nothing). Call it whenever the plugin emits a new
/// tree.
pub struct Reconciler {
    root: gtk::Box,
    on_event: EventFn,
    /// The single root node we have mounted into `root`, retained so the
    /// next `render` can diff against it. `None` until the first render.
    tree: Option<RetainedNode>,
}

impl Reconciler {
    /// Build a reconciler that mounts its tree as a child of `root`.
    ///
    /// `on_event` is invoked on the GTK main thread when a [`Node::Button`]
    /// is clicked, a scroll-enabled (`scroll: true`) [`Node::Box`] is
    /// scrolled, a [`Node::Slider`] is moved by the user, or a
    /// [`Node::Entry`] is submitted (Enter/activate).
    #[must_use]
    pub fn new(root: &gtk::Box, on_event: impl Fn(NodeId, EventKind) + 'static) -> Self {
        Self {
            root: root.clone(),
            on_event: Rc::new(on_event),
            tree: None,
        }
    }

    /// Diff `tree` against the previous render and mutate the mounted widgets
    /// in place. The first call builds the tree; later calls reuse widgets
    /// wherever the diff allows.
    pub fn render(&mut self, tree: &Node) {
        match self.tree.take() {
            // Root reusable (same kind, same id) → update in place.
            Some(mut retained) if reusable(&retained.desc, tree) => {
                update_in_place(&mut retained, tree, &self.on_event);
                self.tree = Some(retained);
            }
            // Root kind/id changed → swap the whole subtree.
            Some(old) => {
                self.root.remove(&old.widget);
                let retained = build_node(tree, &self.on_event);
                self.root.append(&retained.widget);
                self.tree = Some(retained);
            }
            // First render.
            None => {
                let retained = build_node(tree, &self.on_event);
                self.root.append(&retained.widget);
                self.tree = Some(retained);
            }
        }
    }
}

// ── Retained state ──────────────────────────────────────────────────────────

/// The realized widget for one node plus the bookkeeping the next diff needs.
struct RetainedNode {
    widget: gtk::Widget,
    /// Shallow snapshot of the node as last rendered: enough to key it and to
    /// diff its CSS classes. Child structure lives in `children`, not here.
    desc: NodeDesc,
    /// Realized children, in render order. `Box` holds its full child list;
    /// `Button`/`Revealer` hold exactly one; leaf nodes hold none.
    children: Vec<RetainedNode>,
    /// A [`Node::Box`]'s scroll controller, present iff it was last rendered
    /// with `scroll: true`. Retained so [`update_in_place`] can attach or
    /// detach it as the flag flips across renders without rebuilding the
    /// rest of the subtree. Always `None` for every other node kind.
    scroll_controller: Option<gtk::EventControllerScroll>,
    /// A [`Node::Slider`]'s outbound throttle + drag-suppression state, wired
    /// once at build. Retained so [`update_in_place`] can read the last
    /// user-interaction time and skip a programmatic `set_value` that would
    /// fight an active drag. Always `None` for every other node kind.
    slider: Option<Rc<SliderCtl>>,
    /// A [`Node::Expander`]'s realized sub-widgets (revealer, chevron, header
    /// box, single header child, body container), wired once at build. Retained
    /// so [`update_in_place`] can reveal/hide the body, swap the chevron, and
    /// reconcile the header child without navigating the widget tree. The body
    /// children live in [`RetainedNode::children`] (diffed into `body_box`).
    /// Always `None` for every other node kind.
    expander: Option<Box<ExpanderState>>,
    /// A [`Node::Entry`]'s last-rendered `text` **prop** (not the widget's
    /// live text, which the user mutates freely). Retained so
    /// [`update_in_place`] can tell a real prop change (apply `set_text`) from
    /// a re-render merely echoing the unchanged prop (leave the widget alone,
    /// preserving in-progress typing). Always `None` for every other node kind.
    entry_text: Option<String>,
    /// A [`Node::Entry`]'s "submitted since last render" latch, set by the
    /// `activate` handler just before it fires [`EventKind::Submitted`]. Once
    /// the user submits, the widget's live text has diverged from the plugin's
    /// prop model, so the plugin's *next* render is authoritative and must be
    /// applied unconditionally — even when the new prop equals the last-rendered
    /// one (the clear-after-submit flow: `text: ""` → typed → submit → `text:
    /// ""` again). [`update_in_place`] consumes (resets) the latch and forces
    /// the `set_text`. Shared with the closure via `Rc<Cell<_>>` — GTK is
    /// single-threaded, so no locking is needed. Always `None` for every other
    /// node kind.
    entry_submitted: Option<Rc<Cell<bool>>>,
}

/// A [`Node::Expander`]'s retained pieces (see [`RetainedNode::expander`]).
struct ExpanderState {
    /// The clickable header button — the whole header row. Held because it is
    /// the widget an expander's tooltip is armed on ([`tooltip_target`]): the
    /// node's own widget is the outer box, which also holds the revealed body.
    ///
    /// **Invariant: this is the button currently mounted under the outer box.**
    /// It is written once, in [`build_node`], and read once, in
    /// [`tooltip_target`] — alone among this struct's fields, whose other
    /// handles are each read for a mutation a test observes *through* the tree,
    /// so a stale one of those goes red on its own. A stale one here would
    /// silently arm the tooltip on an orphan. [`update_in_place`] keeps the
    /// invariant by rebuilding only the header **child** inside
    /// [`header_box`](ExpanderState::header_box), never the button;
    /// `an_expander_tooltip_survives_a_header_rebuild` is what pins that
    /// (#971 review, MEDIUM-1) — read it before touching the header path.
    header_button: gtk::Button,
    /// The body wrapper; `expanded` drives `set_reveal_child`.
    revealer: gtk::Revealer,
    /// The trailing disclosure chevron; its icon is swapped on an `expanded` flip.
    chevron: gtk::Image,
    /// The horizontal header box — child 0 is the realized `header` node, child 1
    /// the chevron. Held so a header rebuild can re-parent in place before the
    /// chevron.
    header_box: gtk::Box,
    /// The realized `header` node, kept as a single-element vec so it reconciles
    /// through the same reuse-or-rebuild path as a `Button`/`Revealer` child.
    header: Vec<RetainedNode>,
    /// The revealer's inner vertical box — the container the body children diff
    /// into.
    body_box: gtk::Box,
}

/// Shallow per-node snapshot used for keying and class diffing.
#[derive(Clone)]
struct NodeDesc {
    id: Option<NodeId>,
    kind: NodeKind,
    classes: Vec<String>,
    /// The tooltip as last applied (`None` = none was set).
    ///
    /// Retained because [`reconcile_tooltip`] needs the **previous** value to
    /// know whether the widget currently has a tooltip to take away: `new` alone
    /// cannot distinguish "this node never had one" from "this node had one and
    /// dropped it". That is the whole job — and it is exactly what mutating
    /// [`desc_of`] to store `tooltip: None` falsifies, turning
    /// `a_tooltip_dropped_to_none_is_cleared` red and nothing else.
    ///
    /// Note this is **not** the `classes` situation, despite the parallel shape:
    /// that snapshot is load-bearing for *removals* (GTK has no "set the class
    /// list" call, so the old list is the only way to know what to unset), and
    /// it is not here about suppressing churn — `gtk_widget_set_tooltip_text`
    /// already dedups by value, so re-setting the same string emits no
    /// `notify::tooltip-text`.
    tooltip: Option<String>,
}

impl NodeDesc {
    fn key(&self) -> ChildKey {
        ChildKey {
            id: self.id.clone(),
            kind: self.kind,
        }
    }
}

/// Node-variant discriminant. Two widgets are only reuse-compatible when
/// their kinds match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeKind {
    Box,
    Row,
    ListBox,
    Scrolled,
    Label,
    Text,
    Icon,
    Pixels,
    GlSurface,
    Shader,
    Button,
    Progress,
    Slider,
    Revealer,
    Separator,
    Spacer,
    Expander,
    Entry,
}

// ── The pure diff algorithm ─────────────────────────────────────────────────

/// The matching identity of a child for one diff pass: its id (if any) plus
/// its node kind. The heart of keyed diffing operates purely on these, with
/// no GTK involved — which is what makes [`plan_diff`] unit-testable headless.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ChildKey {
    id: Option<NodeId>,
    kind: NodeKind,
}

/// What to do with one slot of the *new* child list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotOp {
    /// Reuse the previous child at this index (update it in place).
    Reuse(usize),
    /// No compatible previous child — build a fresh widget.
    Create,
}

/// The full diff decision for one container's children.
#[derive(Debug, PartialEq, Eq)]
struct DiffPlan {
    /// One entry per *new* child, in new-tree order.
    ops: Vec<SlotOp>,
    /// Previous-child indices with no match in the new tree — to be removed.
    removals: Vec<usize>,
}

/// Compute the keyed diff between the previous children (`prev`) and the new
/// children (`next`), each represented only by their [`ChildKey`].
///
/// Matching rules:
/// - **Keyed** (the child has an id): matched to the first not-yet-consumed
///   previous child with the *same id and same kind*. An id reused across a
///   kind change can't reuse the widget, so it becomes a `Create` and the old
///   widget is removed.
/// - **Keyless** (no id): positional fallback — matched to the next
///   not-yet-consumed *keyless* previous child, reused only if the kind also
///   matches. This is intentionally dumb; carry an id to survive reordering.
///
/// Every previous index ends up either referenced by exactly one `Reuse` op
/// or listed in `removals` (the two sets partition `0..prev.len()`).
fn plan_diff(prev: &[ChildKey], next: &[ChildKey]) -> DiffPlan {
    let mut consumed = vec![false; prev.len()];
    let mut ops = Vec::with_capacity(next.len());

    // Index keyed previous children: id → its prev indices, in order.
    let mut by_id: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, key) in prev.iter().enumerate() {
        if let Some(id) = key.id.as_deref() {
            by_id.entry(id).or_default().push(i);
        }
    }

    // Walks the keyless previous children left-to-right for positional reuse.
    let mut keyless_cursor = 0usize;

    for nk in next {
        let reuse = if let Some(id) = nk.id.as_deref() {
            by_id.get(id).and_then(|indices| {
                indices
                    .iter()
                    .copied()
                    .find(|&i| !consumed[i] && prev[i].kind == nk.kind)
            })
        } else {
            // Advance to the next available keyless prev child; reuse iff its
            // kind matches this slot's, otherwise leave it for removal.
            let mut found = None;
            while keyless_cursor < prev.len() {
                let i = keyless_cursor;
                keyless_cursor += 1;
                if consumed[i] || prev[i].id.is_some() {
                    continue;
                }
                if prev[i].kind == nk.kind {
                    found = Some(i);
                }
                break;
            }
            found
        };

        if let Some(i) = reuse {
            consumed[i] = true;
            ops.push(SlotOp::Reuse(i));
        } else {
            ops.push(SlotOp::Create);
        }
    }

    let removals: Vec<usize> = (0..prev.len()).filter(|&i| !consumed[i]).collect();
    DiffPlan { ops, removals }
}

// ── Container abstraction (Box vs ListBox) ───────────────────────────────────

/// The widget a set of diffed children mount into. Abstracts the two backing
/// container kinds so the keyed [`diff_children`] machinery is shared:
///
/// - [`Container::Box`] backs [`Node::Box`]/[`Node::Row`] — a plain `gtk::Box`
///   whose children sit directly in it, reordered by sibling position.
/// - [`Container::List`] backs [`Node::ListBox`] — a real `gtk::ListBox`
///   (`selection-mode: none`) so libadwaita's `.boxed-list` card styling, which
///   selects `list.boxed-list` (the `GtkListBox` CSS node) and `> row`, actually
///   paints. A `gtk::ListBox` **auto-wraps** each appended child in a
///   `GtkListBoxRow`, so this variant transparently reaches a child's wrapper via
///   `widget.parent()` for remove/reorder, and places by **absolute index**
///   (`GtkListBox` has no sibling-relative reorder). The retained `widget` stays
///   the plugin's own node widget, so [`update_in_place`]'s downcasts are
///   unaffected by the wrapping.
enum Container {
    Box(gtk::Box),
    List(gtk::ListBox),
}

impl Container {
    /// The main-axis orientation, used to constrain a [`Node::Spacer`]. A list
    /// always stacks vertically.
    fn orientation(&self) -> gtk::Orientation {
        match self {
            Container::Box(b) => b.orientation(),
            Container::List(_) => gtk::Orientation::Vertical,
        }
    }

    /// Append `child` at the end (build path, in order). For a list, GTK wraps it
    /// in a fresh `GtkListBoxRow`.
    fn append(&self, child: &gtk::Widget) {
        match self {
            Container::Box(b) => b.append(child),
            Container::List(l) => l.append(child),
        }
    }

    /// Remove `child` (the plugin node widget). For a list this removes its
    /// enclosing auto-created `GtkListBoxRow`.
    fn remove(&self, child: &gtk::Widget) {
        match self {
            Container::Box(b) => b.remove(child),
            Container::List(l) => {
                if let Some(row) = list_row_of(child) {
                    l.remove(&row);
                } else {
                    l.remove(child);
                }
            }
        }
    }

    /// Place `child` at slot `index`, after `prev_sibling` in render order. A
    /// freshly-built child is inserted; an existing one is reordered to that slot.
    fn place(
        &self,
        child: &gtk::Widget,
        index: usize,
        prev_sibling: Option<&gtk::Widget>,
        created: bool,
    ) {
        match self {
            Container::Box(b) => {
                if created {
                    b.insert_child_after(child, prev_sibling);
                } else {
                    b.reorder_child_after(child, prev_sibling);
                }
            }
            Container::List(l) => {
                // GtkListBox has no sibling-relative reorder — place by absolute
                // index. A freshly-built widget is inserted (GTK auto-wraps it in a
                // row); an existing one is moved only if its wrapping row isn't
                // already at `index`, by removing + re-inserting the *same* row
                // (identity preserved; we hold a ref across the move).
                let idx = i32::try_from(index).unwrap_or(-1);
                if created {
                    l.insert(child, idx);
                } else if let Some(row) = list_row_of(child)
                    && row.index() != idx
                {
                    l.remove(&row);
                    l.insert(&row, idx);
                }
            }
        }
    }
}

/// The `GtkListBoxRow` auto-created to wrap a list child, i.e. the child's
/// parent. `None` if the child isn't (yet) inside a list row.
fn list_row_of(child: &gtk::Widget) -> Option<gtk::ListBoxRow> {
    child.parent().and_downcast::<gtk::ListBoxRow>()
}

/// The CSS class [`apply_dense_rows`] marks a dense list's auto-created
/// `GtkListBoxRow` wrappers with. The rule that gives it meaning
/// (`min-height: 0; padding: 0`) ships in the **library** stylesheet
/// (`assets/hytte-ui/style.css`), because `dense` is part of the node
/// vocabulary rather than anything trollshell-specific — hence the `hytte-`
/// prefix the repo's class convention gives library classes.
///
/// A class rather than a direct property call because the floor is *CSS*:
/// libadwaita sets `min-height` on the `row` node, and GTK's
/// `gtk_widget_set_size_request` can only raise a widget's minimum, never lower
/// it below what the style computes. So the only lever that reaches it is
/// another CSS rule at a higher provider priority — which is what
/// `install_default_css` loads the library sheet at.
pub const DENSE_ROW_CLASS: &str = "hytte-dense-row";

/// Mark (or unmark) every auto-created `GtkListBoxRow` wrapper in `list` for the
/// dense rule (#966).
///
/// Walks the list's direct children — which *are* the wrappers — rather than
/// hooking the insert path, so one call covers newly built rows, rows reused
/// from the previous render, and a `dense` flip in either direction. That
/// single-seam shape is deliberate: build and update call the same function
/// with the same argument, so they cannot drift the way two per-arm edits can
/// (the reason [`node_tooltip`] is shaped that way too).
fn apply_dense_rows(list: &gtk::ListBox, dense: bool) {
    let mut cursor = list.first_child();
    while let Some(row) = cursor {
        cursor = row.next_sibling();
        if dense {
            row.add_css_class(DENSE_ROW_CLASS);
        } else {
            row.remove_css_class(DENSE_ROW_CLASS);
        }
    }
}

// ── Build / update ──────────────────────────────────────────────────────────

/// Build a fresh widget subtree for `node`, wiring its event handlers **once**
/// (this is the only place `connect_clicked` / the scroll controller is
/// attached, so reuse across renders can never stack duplicate handlers).
// One exhaustive arm per node variant — the length is the vocabulary size, not
// complexity; splitting it hurts readability more than it helps.
#[allow(clippy::too_many_lines)]
fn build_node(node: &Node, on_event: &EventFn) -> RetainedNode {
    let mut scroll_controller = None;
    let mut slider = None;
    let mut expander = None;
    let mut entry_text = None;
    let mut entry_submitted = None;
    let (widget, children): (gtk::Widget, Vec<RetainedNode>) = match node {
        Node::Box {
            id,
            dir,
            spacing,
            scroll,
            classes,
            children,
            // Bound centrally after this match, together with `Label`'s and
            // `Icon`'s — see the `apply_tooltip` call below.
            tooltip: _,
        } => {
            let boxw = gtk::Box::new(orientation(*dir), *spacing);
            apply_classes(&boxw, classes);
            // Scroll behaviour is driven purely by `scroll`; `id` (if any)
            // is only along for the ride as the fired event's target.
            if *scroll {
                scroll_controller = Some(attach_scroll(&boxw, id.as_deref(), on_event));
            }
            let kids = build_children(&Container::Box(boxw.clone()), children, on_event);
            (boxw.upcast(), kids)
        }
        Node::Row {
            classes,
            spacing,
            children,
            ..
        } => {
            let boxw = gtk::Box::new(gtk::Orientation::Horizontal, *spacing);
            apply_classes(&boxw, classes);
            let kids = build_children(&Container::Box(boxw.clone()), children, on_event);
            (boxw.upcast(), kids)
        }
        Node::ListBox {
            classes,
            dense,
            children,
            ..
        } => {
            // A **real** `gtk::ListBox` (not a plain vertical Box) so libadwaita's
            // `.boxed-list` card styling — which selects `list.boxed-list` — can
            // actually paint (see [`Container`]). Selection-less: a plugin list is
            // a display/command surface, and the vocab carries no selection event.
            let list = gtk::ListBox::new();
            list.set_selection_mode(gtk::SelectionMode::None);
            apply_classes(&list, classes);
            let kids = build_children(&Container::List(list.clone()), children, on_event);
            // After the children exist: the wrappers this marks are created by
            // GTK on `append`, so there is nothing to mark before that.
            apply_dense_rows(&list, *dense);
            (list.upcast(), kids)
        }
        Node::Scrolled {
            max_height,
            classes,
            child,
            ..
        } => {
            let sw = new_viewport();
            apply_max_height(&sw, *max_height);
            apply_classes(&sw, classes);
            let realized = build_node(child, on_event);
            sw.set_child(Some(&realized.widget));
            (sw.upcast(), vec![realized])
        }
        Node::Label { text, classes, .. } => {
            let label = gtk::Label::new(Some(text));
            // Left-align the text: a GTK label defaults to `xalign 0.5`, so a
            // label that fills its box centres — but card/list text should read
            // from the leading edge (the native widgets set `halign(Start)`).
            // `xalign` positions the text without shrinking the label, so
            // `max_width_chars`/ellipsize still work. Per-node override tracked
            // in #333.
            label.set_xalign(0.0);
            apply_classes(&label, classes);
            (label.upcast(), Vec::new())
        }
        Node::Text {
            text,
            max_width_chars,
            ellipsize,
            classes,
            ..
        } => {
            let label = gtk::Label::new(Some(text));
            apply_text_flow(&label, *ellipsize);
            label.set_xalign(0.0); // left-align by default — see Node::Label above (#333)
            if let Some(n) = max_width_chars {
                label.set_max_width_chars(*n);
            }
            apply_classes(&label, classes);
            (label.upcast(), Vec::new())
        }
        Node::Icon { name, classes, .. } => {
            let image = gtk::Image::new();
            image.set_icon_name(Some(name));
            apply_classes(&image, classes);
            (image.upcast(), Vec::new())
        }
        Node::Pixels {
            width,
            height,
            data,
            scale,
            classes,
            ..
        } => {
            let surface = crate::pixels::PixelSurface::new();
            // Shared, not copied: the same `Arc` is what the node's other
            // mountings (one per monitor) hand their own surfaces (#911).
            surface.set_pixels_shared(*width, *height, data);
            surface.set_scale(*scale);
            apply_classes(&surface, classes);
            (surface.upcast(), Vec::new())
        }
        Node::GlSurface {
            width,
            height,
            program,
            state,
            classes,
            ..
        } => {
            let surface = crate::gl_surface::GlSurface::new();
            // Shared, not copied — the same `Arc` every monitor's mounting of
            // this node hands its own surface, exactly as the `Pixels` arm does.
            surface.set_state(*program, *width, *height, state);
            apply_classes(&surface, classes);
            (surface.upcast(), Vec::new())
        }
        Node::Shader {
            width,
            height,
            state,
            classes,
            ..
        } => {
            // A `ShaderSurface`, never a `GlSurface` and never a
            // `PixelSurface`: the compiled-program cache and the runtime source
            // live in that widget, and `NodeKind::Shader` is what keeps the
            // three from ever being handed one another's node.
            let surface = crate::shader_surface::ShaderSurface::new();
            surface.set_state(*width, *height, state);
            apply_classes(&surface, classes);
            (surface.upcast(), Vec::new())
        }
        Node::Button { id, classes, child } => {
            let button = gtk::Button::new();
            apply_classes(&button, classes);
            // Click handler bound once, here, to this widget identity.
            let on_click = on_event.clone();
            let click_id = id.clone();
            button.connect_clicked(move |_| on_click(click_id.clone(), EventKind::Click));
            let realized = build_node(child, on_event);
            button.set_child(Some(&realized.widget));
            (button.upcast(), vec![realized])
        }
        Node::Progress {
            fraction, classes, ..
        } => {
            let bar = gtk::ProgressBar::new();
            bar.set_fraction(*fraction);
            apply_classes(&bar, classes);
            (bar.upcast(), Vec::new())
        }
        Node::Slider {
            id,
            min,
            max,
            value,
            step,
            enabled,
            classes,
        } => {
            // Build via an explicit `Adjustment` (not `Scale::with_range`, which
            // asserts `min < max` and `step != 0` itself) — but that swap alone
            // does not make an ill-formed range safe: `gtk_adjustment_new` carries
            // its own `g_return_val_if_fail (lower + page_size <= upper, NULL)`
            // (gtkadjustment.c:395), and the #910 review reproduced the resulting
            // `!ptr.is_null()` panic on main for both `max < min` and `NaN`. What
            // actually holds here is the wire seam: `wire_map`'s `Slider` arm runs
            // `wire::sane_slider_floats` (#904) before a `Node` ever reaches
            // `build_node`, so by the time this call is made `min <= max` and every
            // float is finite. `page_size = 0`: a scale is a point selector, so the
            // whole `min..=max` is reachable.
            let adj = gtk::Adjustment::new(*value, *min, *max, *step, *step, 0.0);
            let scale = gtk::Scale::new(gtk::Orientation::Horizontal, Some(&adj));
            scale.set_draw_value(false);
            scale.set_hexpand(true);
            // `enabled: false` ⇒ insensitive: greyed and non-interactive, so it
            // fires no `change-value` (see the node docs).
            scale.set_sensitive(*enabled);
            apply_classes(&scale, classes);
            // The change-value handler (user-driven only) is bound once here, to
            // this widget identity — like `Button`'s click — so reuse never stacks
            // duplicate handlers.
            slider = Some(attach_slider(&scale, id.clone(), on_event));
            (scale.upcast(), Vec::new())
        }
        Node::Revealer { open, child, .. } => {
            let revealer = gtk::Revealer::new();
            revealer.set_reveal_child(*open);
            let realized = build_node(child, on_event);
            revealer.set_child(Some(&realized.widget));
            (revealer.upcast(), vec![realized])
        }
        Node::Separator { classes } => {
            let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
            apply_classes(&sep, classes);
            (sep.upcast(), Vec::new())
        }
        Node::Spacer => {
            // An empty box that eats the container's slack to justify its
            // siblings. Built expanding on both axes as a default; the real,
            // container-aware axis is set by `constrain_spacer_axis` once the
            // parent orientation is known (a cross-axis expand is NOT inert — it
            // propagates up and stretches the box on that axis, #330). No id, no
            // children, no classes — it is styled by its neighbours, never itself.
            let boxw = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            boxw.set_hexpand(true);
            boxw.set_vexpand(true);
            (boxw.upcast(), Vec::new())
        }
        Node::Expander {
            id,
            header,
            children,
            expanded,
            classes,
            // Armed centrally after this match, on the header button rather
            // than on `outer` — see `tooltip_target`.
            tooltip: _,
        } => {
            let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
            apply_classes(&outer, classes);

            // Header: a flat, full-width button — the whole header is the click
            // target. Bound once here (like `Button`), so reuse never double-fires.
            let button = gtk::Button::new();
            button.add_css_class("flat");
            let on_click = on_event.clone();
            let click_id = id.clone();
            button.connect_clicked(move |_| on_click(click_id.clone(), EventKind::Click));

            let header_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let header_realized = build_node(header, on_event);
            // The header content fills so the chevron pins to the trailing edge —
            // no Spacer dance needed by the plugin.
            header_realized.widget.set_hexpand(true);
            header_box.append(&header_realized.widget);
            let chevron = gtk::Image::from_icon_name(chevron_icon(*expanded));
            chevron.add_css_class("dim-label"); // subtle, like a native disclosure
            header_box.append(&chevron);
            button.set_child(Some(&header_box));
            outer.append(&button);

            // Body: a revealer over a vertical box of children.
            let revealer = gtk::Revealer::new();
            revealer.set_reveal_child(*expanded);
            let body_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let body_kids = build_children(&Container::Box(body_box.clone()), children, on_event);
            revealer.set_child(Some(&body_box));
            outer.append(&revealer);

            expander = Some(Box::new(ExpanderState {
                header_button: button,
                revealer,
                chevron,
                header_box,
                header: vec![header_realized],
                body_box,
            }));
            (outer.upcast(), body_kids)
        }
        Node::Entry {
            id,
            text,
            placeholder,
            classes,
        } => {
            let entry = gtk::Entry::new();
            entry.set_text(text);
            apply_entry_placeholder(&entry, placeholder);
            apply_classes(&entry, classes);
            // The activate handler (user-driven only: GTK's `activate` fires on
            // Enter, never on a programmatic `set_text`) is bound once here, to
            // this widget identity — like `Button`'s click — so reuse never
            // stacks duplicate handlers and an echoed `text` can't re-emit.
            let on_submit = on_event.clone();
            let submit_id = id.clone();
            // Latch a submit so the plugin's *next* render re-asserts its `text`
            // prop unconditionally (see [`RetainedNode::entry_submitted`]): the
            // handler flips it true *before* firing, so even a synchronous
            // re-render triggered by the event sees it set.
            let submitted = Rc::new(Cell::new(false));
            let submitted_handler = submitted.clone();
            entry.connect_activate(move |entry| {
                submitted_handler.set(true);
                on_submit(
                    submit_id.clone(),
                    EventKind::Submitted {
                        text: entry.text().to_string(),
                    },
                );
            });
            entry_text = Some(text.clone());
            entry_submitted = Some(submitted);
            (entry.upcast(), Vec::new())
        }
    };

    // Applied here, once, rather than in the seven arms that carry the field
    // (#957): the arms differ in widget *type*, not in how a tooltip is set, and
    // a central call is what keeps this from drifting away from the matching
    // `reconcile_tooltip` in `update_in_place`.
    apply_tooltip(
        &tooltip_target(&widget, expander.as_deref()),
        node_tooltip(node),
    );

    RetainedNode {
        widget,
        desc: desc_of(node),
        children,
        scroll_controller,
        slider,
        expander,
        entry_text,
        entry_submitted,
    }
}

/// Build and append each child into the [`Container`], returning the realized
/// children in render order. Shared by the `Box` / `Row` / `ListBox` / `Expander`
/// container arms of [`build_node`].
fn build_children(
    container: &Container,
    children: &[Node],
    on_event: &EventFn,
) -> Vec<RetainedNode> {
    let mut kids = Vec::with_capacity(children.len());
    for child in children {
        let realized = build_node(child, on_event);
        container.append(&realized.widget);
        constrain_spacer_axis(container, child, &realized.widget);
        kids.push(realized);
    }
    kids
}

/// A [`Node::Spacer`] must expand only along its container's **main** axis.
/// `build_node` builds the spacer widget with expand on *both* axes (there it
/// can't see its parent); here — where the container orientation is known — we
/// drop the cross-axis flag. Left in, GTK's `compute_expand` propagates that
/// cross-axis expand up to the parent box: a spacer justifying `label … value`
/// in a horizontal row would make the row — and, up the chain, the whole card —
/// claim vertical expansion, splaying the rows down the sidebar (#330).
fn constrain_spacer_axis(container: &Container, child: &Node, widget: &gtk::Widget) {
    if matches!(child, Node::Spacer) {
        let horizontal = container.orientation() == gtk::Orientation::Horizontal;
        widget.set_hexpand(horizontal);
        widget.set_vexpand(!horizontal);
    }
}

/// Update an already-realized node in place. Precondition (guaranteed by the
/// caller via [`reusable`] / [`plan_diff`]): `retained`'s kind and id match
/// `new`, so the widget downcast always succeeds and event handlers stay
/// valid (a `Button`/`Box` never changes identity across an id change).
// One exhaustive arm per node variant — the length is the vocabulary size, not
// complexity; splitting it hurts readability more than it helps (as in `build_node`).
#[allow(clippy::too_many_lines)]
fn update_in_place(retained: &mut RetainedNode, new: &Node, on_event: &EventFn) {
    match new {
        Node::Box {
            id,
            dir,
            spacing,
            scroll,
            classes,
            children,
            // Reconciled centrally after this match — see `reconcile_tooltip`.
            tooltip: _,
        } => {
            let boxw = downcast::<gtk::Box>(&retained.widget);
            boxw.set_orientation(orientation(*dir));
            boxw.set_spacing(*spacing);
            reconcile_classes(boxw, &retained.desc.classes, classes);
            // `scroll` is a plain mutable property (like `classes`), not
            // part of this node's identity, so a flip attaches/detaches the
            // controller in place rather than forcing a subtree rebuild.
            match (retained.scroll_controller.take(), *scroll) {
                (Some(ctrl), true) => retained.scroll_controller = Some(ctrl),
                (Some(ctrl), false) => boxw.remove_controller(&ctrl),
                (None, true) => {
                    retained.scroll_controller = Some(attach_scroll(boxw, id.as_deref(), on_event));
                }
                (None, false) => {}
            }
            diff_children(
                &Container::Box(boxw.clone()),
                &mut retained.children,
                children,
                on_event,
            );
        }
        Node::Row {
            classes,
            spacing,
            children,
            ..
        } => {
            // A Row is a horizontal gtk::Box with no scroll or orientation props
            // to mutate (orientation is fixed by the kind, which reuse already
            // matched on), so only spacing + classes + children reconcile.
            let boxw = downcast::<gtk::Box>(&retained.widget);
            boxw.set_spacing(*spacing);
            reconcile_classes(boxw, &retained.desc.classes, classes);
            diff_children(
                &Container::Box(boxw.clone()),
                &mut retained.children,
                children,
                on_event,
            );
        }
        Node::ListBox {
            classes,
            dense,
            children,
            ..
        } => {
            // A ListBox is a real gtk::ListBox; children reconcile through the
            // List container, which handles the GtkListBoxRow wrapping.
            let list = downcast::<gtk::ListBox>(&retained.widget);
            reconcile_classes(list, &retained.desc.classes, classes);
            diff_children(
                &Container::List(list.clone()),
                &mut retained.children,
                children,
                on_event,
            );
            // After the diff, for the same reason `build_node` marks after
            // building: a freshly-inserted child only has its wrapper once GTK
            // has created it. Marking the whole list every render also covers a
            // `dense` flip on rows that were merely reused.
            apply_dense_rows(list, *dense);
        }
        Node::Scrolled {
            max_height,
            classes,
            child,
            ..
        } => {
            let sw = downcast::<gtk::ScrolledWindow>(&retained.widget);
            apply_max_height(sw, *max_height);
            reconcile_classes(sw, &retained.desc.classes, classes);
            reconcile_single(&mut retained.children, child, on_event, |c| {
                sw.set_child(c);
            });
        }
        Node::Label { text, classes, .. } => {
            let label = downcast::<gtk::Label>(&retained.widget);
            label.set_text(text);
            reconcile_classes(label, &retained.desc.classes, classes);
        }
        Node::Text {
            text,
            max_width_chars,
            ellipsize,
            classes,
            ..
        } => {
            let label = downcast::<gtk::Label>(&retained.widget);
            label.set_text(text);
            // `-1` is GTK's "no maximum", so a flip back to `None` resets it.
            label.set_max_width_chars(max_width_chars.unwrap_or(-1));
            // `ellipsize` is a mutable prop: flip the flow mode (wrap ⇄ single-line
            // ellipsis) in place without rebuilding the label.
            apply_text_flow(label, *ellipsize);
            reconcile_classes(label, &retained.desc.classes, classes);
        }
        Node::Icon { name, classes, .. } => {
            let image = downcast::<gtk::Image>(&retained.widget);
            image.set_icon_name(Some(name));
            reconcile_classes(image, &retained.desc.classes, classes);
        }
        Node::Pixels {
            width,
            height,
            data,
            scale,
            classes,
            ..
        } => {
            let surface = downcast::<crate::pixels::PixelSurface>(&retained.widget);
            // `data` and `scale` are mutable props: swap the texture / natural
            // size in place (no rebuild; `set_scale` only queues a resize on a
            // real change).
            //
            // The shared setter (#911): a re-map that carries the *same* `Arc`
            // — a static preem chip re-mapped because a sibling animated, or
            // the second monitor's pass over one frame — is settled by an
            // `Arc::ptr_eq` without reading a byte, and a frame that really did
            // change is uploaded without a copy.
            surface.set_pixels_shared(*width, *height, data);
            surface.set_scale(*scale);
            reconcile_classes(surface, &retained.desc.classes, classes);
        }
        Node::GlSurface {
            width,
            height,
            program,
            state,
            classes,
            ..
        } => {
            let surface = downcast::<crate::gl_surface::GlSurface>(&retained.widget);
            // `program` and `state` are mutable props: point the existing
            // surface at the new state in place. The guard is inside
            // `set_state` and its fast path is an `Arc::ptr_eq`, so a re-map
            // that carries the *same* state — a static GL chip re-mapped
            // because a sibling animated, or the second monitor's pass over one
            // frame — queues no render at all.
            surface.set_state(*program, *width, *height, state);
            reconcile_classes(surface, &retained.desc.classes, classes);
        }
        Node::Shader {
            width,
            height,
            state,
            classes,
            ..
        } => {
            let surface = downcast::<crate::shader_surface::ShaderSurface>(&retained.widget);
            // `state` is a mutable prop, and updating in place is what makes
            // the compile-once claim true across frames: rebuilding the widget
            // would throw away the linked program and restart `u_time`. The
            // guard lives inside `set_state`, fast-pathed on `Arc::ptr_eq`.
            surface.set_state(*width, *height, state);
            reconcile_classes(surface, &retained.desc.classes, classes);
        }
        Node::Button { classes, child, .. } => {
            let button = downcast::<gtk::Button>(&retained.widget);
            reconcile_classes(button, &retained.desc.classes, classes);
            // Click handler is left untouched (bound at build time).
            reconcile_single(&mut retained.children, child, on_event, |c| {
                button.set_child(c);
            });
        }
        Node::Progress {
            fraction, classes, ..
        } => {
            let bar = downcast::<gtk::ProgressBar>(&retained.widget);
            bar.set_fraction(*fraction);
            reconcile_classes(bar, &retained.desc.classes, classes);
        }
        Node::Slider {
            min,
            max,
            value,
            step,
            enabled,
            classes,
            ..
        } => {
            let scale = downcast::<gtk::Scale>(&retained.widget);
            // `enabled` is a plain mutable prop: flip sensitivity in place. An
            // insensitive scale takes no input, so this also gates whether the
            // slider can fire `change-value` at all.
            scale.set_sensitive(*enabled);
            // Range/step are plain mutable props: push them through the live
            // adjustment (the same one built at `build_node`), then reconcile the
            // value below.
            let adj = scale.adjustment();
            adj.set_lower(*min);
            adj.set_upper(*max);
            adj.set_step_increment(*step);
            adj.set_page_increment(*step);
            // `value` is a mutable prop — but suppress the programmatic move while
            // the user is actively dragging, so a plugin echoing the value back
            // can't rubber-band the grab. Once the drag settles (last user change
            // older than the grab window) the echo applies and the thumb
            // reconciles. `set_value` fires `value-changed` only — never
            // `change-value` — so it never re-emits an event.
            let dragging = retained.slider.as_ref().is_some_and(|ctl| {
                slider_suppress_set(ctl.last_user.get(), Instant::now(), SLIDER_GRAB_WINDOW)
            });
            if !dragging {
                scale.set_value(*value);
            }
            reconcile_classes(scale, &retained.desc.classes, classes);
        }
        Node::Revealer { open, child, .. } => {
            let revealer = downcast::<gtk::Revealer>(&retained.widget);
            revealer.set_reveal_child(*open);
            reconcile_single(&mut retained.children, child, on_event, |c| {
                revealer.set_child(c);
            });
        }
        Node::Separator { classes } => {
            reconcile_classes(&retained.widget, &retained.desc.classes, classes);
        }
        // A `Spacer` has no mutable props (expand is fixed by the kind, which
        // reuse already matched on) and no classes — nothing to reconcile.
        Node::Spacer => {}
        Node::Expander {
            header,
            children,
            expanded,
            classes,
            ..
        } => {
            let outer = downcast::<gtk::Box>(&retained.widget);
            reconcile_classes(outer, &retained.desc.classes, classes);
            let es = retained
                .expander
                .as_mut()
                .expect("kind invariant: Expander retains its ExpanderState");
            // `expanded` is a mutable prop: reveal/hide + swap the chevron in place.
            // The click handler stays bound (from build time), so a toggle still
            // fires exactly once.
            es.revealer.set_reveal_child(*expanded);
            es.chevron.set_icon_name(Some(chevron_icon(*expanded)));
            // Reconcile the single header child, keeping it child 0 of the header
            // box (the chevron stays pinned after it).
            match es.header.pop() {
                Some(mut existing) if reusable(&existing.desc, header) => {
                    update_in_place(&mut existing, header, on_event);
                    existing.widget.set_hexpand(true);
                    es.header.push(existing);
                }
                other => {
                    if let Some(old) = other {
                        es.header_box.remove(&old.widget);
                    }
                    let realized = build_node(header, on_event);
                    realized.widget.set_hexpand(true);
                    es.header_box.prepend(&realized.widget); // before the chevron
                    es.header.clear();
                    es.header.push(realized);
                }
            }
            diff_children(
                &Container::Box(es.body_box.clone()),
                &mut retained.children,
                children,
                on_event,
            );
        }
        Node::Entry {
            text,
            placeholder,
            classes,
            ..
        } => {
            let entry = downcast::<gtk::Entry>(&retained.widget);
            // Apply `text` only on a real *prop* change (compared against the
            // last-rendered prop, not the widget's live text): a re-render that
            // merely echoes the unchanged value must never clobber what the
            // user is typing, while an actual change (prefill) applies even
            // while the entry is focused. `set_text` never fires `activate`, so
            // this can't re-enter the event path.
            //
            // The prop-diff alone can't deliver clear-after-submit: after the
            // user types into an Entry whose prop is "" and submits, the plugin
            // re-renders "" to clear — equal to the last-rendered prop, so the
            // diff would skip and the typed text would stick. The submit latch
            // (set by the `activate` handler, consumed here one-shot) forces the
            // re-assert so the plugin's post-submit render is always applied.
            let force = retained
                .entry_submitted
                .as_ref()
                .is_some_and(|c| c.replace(false));
            if force || retained.entry_text.as_deref() != Some(text.as_str()) {
                entry.set_text(text);
                retained.entry_text = Some(text.clone());
            }
            apply_entry_placeholder(entry, placeholder);
            reconcile_classes(entry, &retained.desc.classes, classes);
        }
    }

    // The tooltip is a mutable prop on every kind that carries one, and the
    // reconcile is kind-independent — so it lives here, next to the snapshot
    // refresh it reads, rather than repeated in seven arms (#957). Must run
    // *before* the refresh below, which overwrites the `prev` it compares to.
    reconcile_tooltip(
        &tooltip_target(&retained.widget, retained.expander.as_deref()),
        retained.desc.tooltip.as_deref(),
        node_tooltip(new),
    );

    // Refresh the snapshot so the *next* diff compares against current state.
    retained.desc = desc_of(new);
}

/// Reconcile a single-child slot (`Button`/`Revealer`). Reuses the existing
/// child if it is [`reusable`]; otherwise builds a fresh one and re-parents it
/// via `set_child` (which unparents the old widget).
fn reconcile_single(
    slot: &mut Vec<RetainedNode>,
    new_child: &Node,
    on_event: &EventFn,
    set_child: impl FnOnce(Option<&gtk::Widget>),
) {
    match slot.pop() {
        Some(mut existing) if reusable(&existing.desc, new_child) => {
            update_in_place(&mut existing, new_child, on_event);
            slot.push(existing); // widget identity preserved; no re-parent
        }
        _ => {
            let realized = build_node(new_child, on_event);
            set_child(Some(&realized.widget));
            slot.clear();
            slot.push(realized);
        }
    }
}

/// Reconcile a [`Container`]'s children using the keyed [`plan_diff`].
fn diff_children(
    container: &Container,
    retained: &mut Vec<RetainedNode>,
    new_children: &[Node],
    on_event: &EventFn,
) {
    let prev_keys: Vec<ChildKey> = retained.iter().map(|r| r.desc.key()).collect();
    let next_keys: Vec<ChildKey> = new_children.iter().map(child_key).collect();
    let plan = plan_diff(&prev_keys, &next_keys);

    // Move retained children into slots we can take from by index.
    let mut old: Vec<Option<RetainedNode>> = retained.drain(..).map(Some).collect();

    // Drop the widgets whose key vanished.
    for &i in &plan.removals {
        if let Some(gone) = old[i].take() {
            container.remove(&gone.widget);
        }
    }

    // Realize the new child list: reuse-in-place or build fresh.
    let mut next: Vec<RetainedNode> = Vec::with_capacity(new_children.len());
    for (slot, op) in plan.ops.iter().enumerate() {
        match *op {
            SlotOp::Reuse(i) => {
                let mut node = old[i]
                    .take()
                    .expect("plan_diff reuse points at a live child");
                update_in_place(&mut node, &new_children[slot], on_event);
                next.push(node);
            }
            SlotOp::Create => next.push(build_node(&new_children[slot], on_event)),
        }
    }
    debug_assert!(
        old.iter().all(Option::is_none),
        "every previous child must be reused or removed"
    );

    // Lay the children out in new-tree order. Reused widgets are already in
    // the container (reorder); freshly created ones are not yet (insert). The
    // [`Container`] hides the Box-vs-ListBox placement difference (sibling-after
    // vs absolute index + row wrapping).
    let mut prev_sibling: Option<gtk::Widget> = None;
    for (slot, op) in plan.ops.iter().enumerate() {
        let widget = next[slot].widget.clone();
        container.place(
            &widget,
            slot,
            prev_sibling.as_ref(),
            matches!(*op, SlotOp::Create),
        );
        // Re-assert the spacer's axis: covers a fresh spacer and a reused one
        // whose container flipped orientation since it was built.
        constrain_spacer_axis(container, &new_children[slot], &widget);
        prev_sibling = Some(widget);
    }

    *retained = next;
}

// ── Small helpers ───────────────────────────────────────────────────────────

/// Whether the realized node described by `prev` can be updated in place to
/// become `new`: same kind *and* same id. Requiring id-equality here keeps a
/// `Button`'s click handler (or a `Box`'s scroll handler), captured at build
/// time with the id, from ever firing a stale target after a single-child or
/// root swap.
fn reusable(prev: &NodeDesc, new: &Node) -> bool {
    prev.kind == node_kind(new) && prev.id.as_deref() == node_id(new)
}

fn orientation(dir: Dir) -> gtk::Orientation {
    match dir {
        Dir::Horizontal => gtk::Orientation::Horizontal,
        Dir::Vertical => gtk::Orientation::Vertical,
    }
}

/// The disclosure-chevron icon for a [`Node::Expander`]'s current state: pointing
/// down when open, at the trailing edge (right, in LTR) when collapsed — matching
/// the native `AdwExpanderRow` affordance.
fn chevron_icon(expanded: bool) -> &'static str {
    if expanded {
        "pan-down-symbolic"
    } else {
        "pan-end-symbolic"
    }
}

/// Build the `gtk::ScrolledWindow` behind a [`Node::Scrolled`] (#966).
///
/// The three settings are what make it a *bounded viewport* rather than a
/// scroller in the ordinary sense, and all three are fixed by the node kind (so
/// they are set once, at build, and never reconciled):
///
/// - `propagate_natural_height(true)` — the scroller asks for its child's
///   natural height, so a child **shorter** than the cap is not stretched to it.
///   Stated because it is the documented spelling of that intent, **not**
///   because it is measurably load-bearing today: removing it leaves
///   `a_child_shorter_than_max_height_is_not_stretched` green, because the
///   `GtkViewport` GTK wraps a non-scrollable child in already reports that
///   child's minimum height, and `max_content_height` clamps the minimum as well
///   as the natural. It is kept so the request does not quietly depend on that
///   wrapping — a natively scrollable child would come with no `GtkViewport`
///   and no propagation.
/// - `policy(Never, Automatic)` — vertical only. `Never` on the horizontal axis
///   is what propagates the child's natural **width** unchanged, so wrapping a
///   card in a viewport never narrows it or grows a horizontal scrollbar.
/// - `overlay_scrolling(true)` — the indicator floats over the content instead
///   of taking width from it, so wrapping a card does not reflow its rows.
///   **This restates GTK4's own default and pins nothing**: `overlay-scrolling`
///   is already `TRUE`, deleting the call cannot regress any test here, and no
///   assertion in this file checks that width guarantee. It is written out for
///   the same reason the policy above is — so the three properties that decide
///   what this widget *is* are read together — not because it is doing work.
fn new_viewport() -> gtk::ScrolledWindow {
    let sw = gtk::ScrolledWindow::new();
    sw.set_propagate_natural_height(true);
    sw.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    sw.set_overlay_scrolling(true);
    sw
}

/// Apply a [`Node::Scrolled`]'s height cap. Shared by build and update so the
/// two cannot drift, and so a changed `max_height` re-bounds a reused viewport.
///
/// `0` (and any negative value, which the wire's `u16` cannot produce but a
/// direct `hytte-ui` caller can) maps to GTK's `-1` — *no maximum* — so the node
/// degrades to a pass-through wrapper rather than collapsing to nothing.
fn apply_max_height(sw: &gtk::ScrolledWindow, max_height: i32) {
    sw.set_max_content_height(if max_height > 0 { max_height } else { -1 });
}

/// Set a [`Node::Text`] label's flow mode. Shared by build and update so the
/// two never drift, and so an `ellipsize` flip toggles in place:
/// - `ellipsize == true` → single-line, truncate with a trailing ellipsis
///   (`EllipsizeMode::End`) — the native departures-row look.
/// - `ellipsize == false` → wrap at word-then-char boundaries so an unbroken
///   long token still can't force the container wider (the #281 fix).
///
/// Both directions reset the opposite mode (`set_ellipsize(None)` vs
/// `set_wrap(false)`), so flipping the flag on a reused label is complete.
fn apply_text_flow(label: &gtk::Label, ellipsize: bool) {
    if ellipsize {
        label.set_wrap(false);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    } else {
        label.set_ellipsize(gtk::pango::EllipsizeMode::None);
        label.set_wrap(true);
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    }
}

/// Set a [`Node::Entry`]'s placeholder: the greyed hint shown while empty.
/// Shared by build and update so the two never drift; an empty string maps to
/// no placeholder (`None`) rather than an empty visible hint.
fn apply_entry_placeholder(entry: &gtk::Entry, placeholder: &str) {
    entry.set_placeholder_text((!placeholder.is_empty()).then_some(placeholder));
}

/// Attach a scroll controller to `boxw`, firing [`EventKind::Scroll`]
/// through `on_event` addressed at `id` (or an empty [`NodeId`] if the box
/// carries none — `id` is optional and only a diff key, never required for
/// scroll behaviour). Returns the controller so the caller can retain it and
/// `remove_controller` it later if `scroll` flips back to `false`.
fn attach_scroll(
    boxw: &gtk::Box,
    id: Option<&str>,
    on_event: &EventFn,
) -> gtk::EventControllerScroll {
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let on_event = on_event.clone();
    let id = id.map(ToOwned::to_owned).unwrap_or_default();
    scroll.connect_scroll(move |_, dx, dy| {
        on_event(id.clone(), EventKind::Scroll { dx, dy });
        // Don't consume: scrolling over a scroll-enabled box shouldn't
        // swallow the event from ancestor controllers.
        glib::Propagation::Proceed
    });
    boxw.add_controller(scroll.clone());
    scroll
}

// ── Slider: user-driven throttled emit + drag-fight suppression ───────────────

/// Minimum spacing between emitted [`EventKind::ValueChanged`] frames: a drag
/// fires `change-value` at the display refresh rate, so the raw stream is
/// coalesced to a leading edge + one trailing emit per this window (plus a final
/// settle). Keeps a network-bound consumer (vibectl per-light brightness) from
/// being flooded while still feeling live.
const SLIDER_CADENCE: Duration = Duration::from_millis(50);

/// How long after the user's last move a programmatic `set_value` stays
/// suppressed. A drag keeps refreshing the last-move time, so the slider ignores
/// echoed values for the whole grab and briefly after; once movement stops, the
/// plugin's echo applies and the thumb reconciles.
const SLIDER_GRAB_WINDOW: Duration = Duration::from_millis(250);

/// Per-[`Node::Slider`] outbound state: a trailing-edge throttle over the
/// user-driven `change-value` stream, plus the last user-interaction time
/// [`update_in_place`] reads to suppress a render-driven `set_value` that would
/// fight an active drag. Held by an `Rc` shared between the `change-value`
/// handler (strong, so it lives with the `gtk::Scale`) and the retained node.
struct SliderCtl {
    on_event: EventFn,
    id: NodeId,
    /// Instant of the most recent user-driven change (drag / scroll / key).
    last_user: Cell<Option<Instant>>,
    /// Instant of the last emitted `ValueChanged` (the leading-edge gate).
    last_emit: Cell<Option<Instant>>,
    /// Latest user value awaiting a trailing-edge flush (`None` once emitted).
    pending: Cell<Option<f64>>,
    /// Whether a trailing flush timer is currently armed (so we never stack more
    /// than one; the armed timer always flushes the freshest `pending`).
    armed: Cell<bool>,
}

/// The throttle verdict for one user move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThrottleAction {
    /// Emit now (leading edge — first move, or the cadence window has elapsed).
    Emit,
    /// Too soon; schedule a trailing flush after this delay.
    Defer(Duration),
}

/// Pure leading-edge decision: emit if we've never emitted or a full `cadence`
/// has passed since the last emit, else defer by the remaining time. Split out
/// so the throttle logic is unit-testable without GTK or a clock.
fn throttle_decision(
    last_emit: Option<Instant>,
    now: Instant,
    cadence: Duration,
) -> ThrottleAction {
    match last_emit {
        None => ThrottleAction::Emit,
        Some(prev) => {
            let elapsed = now.saturating_duration_since(prev);
            if elapsed >= cadence {
                ThrottleAction::Emit
            } else {
                // `elapsed < cadence` here, so this is the exact remaining time;
                // `saturating_sub` keeps clippy's unchecked-time-subtraction happy.
                ThrottleAction::Defer(cadence.saturating_sub(elapsed))
            }
        }
    }
}

/// Pure drag-suppression predicate: a render-driven `set_value` is suppressed
/// while the user's last move is newer than `window`. Unit-testable, no GTK.
fn slider_suppress_set(last_user: Option<Instant>, now: Instant, window: Duration) -> bool {
    matches!(last_user, Some(t) if now.saturating_duration_since(t) < window)
}

impl SliderCtl {
    /// Record a user move and emit it under the trailing-edge throttle.
    fn on_user_change(self: &Rc<Self>, value: f64) {
        let now = Instant::now();
        self.last_user.set(Some(now));
        match throttle_decision(self.last_emit.get(), now, SLIDER_CADENCE) {
            ThrottleAction::Emit => self.emit(value, now),
            ThrottleAction::Defer(delay) => {
                self.pending.set(Some(value));
                self.arm_trailing(delay);
            }
        }
    }

    /// Emit a `ValueChanged` now and record the emit time (clearing any pending).
    fn emit(&self, value: f64, now: Instant) {
        self.last_emit.set(Some(now));
        self.pending.set(None);
        (self.on_event)(self.id.clone(), EventKind::ValueChanged { value });
    }

    /// Arm a one-shot trailing flush (unless one is already pending). The timer
    /// holds only a [`Weak`], so a torn-down slider's flush is a no-op — no post-
    /// teardown emit, no leak.
    fn arm_trailing(self: &Rc<Self>, delay: Duration) {
        if self.armed.replace(true) {
            return; // a flush is already scheduled; it picks up the latest pending
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(delay, move || {
            if let Some(ctl) = weak.upgrade() {
                ctl.armed.set(false);
                if let Some(v) = ctl.pending.take() {
                    ctl.emit(v, Instant::now());
                }
            }
        });
    }
}

/// Wire a [`Node::Slider`]'s user-driven value stream to `on_event`, addressed
/// at `id`, and return the retained [`SliderCtl`].
///
/// Bound on `change-value` — **not** `value-changed`. That distinction is the
/// whole re-entrancy story: `change-value` fires only for user actions (drag,
/// scroll wheel, arrow keys), while `value-changed` also fires on a programmatic
/// `set_value`. Listening on the former means a plugin re-render that echoes a
/// value back can never re-enter this handler, so there is no feedback loop to
/// break with `block_signal` (the `bind_two_way` problem — solved structurally
/// here). The handler returns [`glib::Propagation::Proceed`] so GTK still moves
/// the thumb to follow the cursor.
fn attach_slider(scale: &gtk::Scale, id: NodeId, on_event: &EventFn) -> Rc<SliderCtl> {
    let ctl = Rc::new(SliderCtl {
        on_event: on_event.clone(),
        id,
        last_user: Cell::new(None),
        last_emit: Cell::new(None),
        pending: Cell::new(None),
        armed: Cell::new(false),
    });
    let handler_ctl = ctl.clone();
    scale.connect_change_value(move |scale, _scroll, value| {
        // Clamp to the live adjustment bounds so the emitted value matches what
        // the scale will settle on (a drag can overshoot the ends slightly).
        let adj = scale.adjustment();
        handler_ctl.on_user_change(value.clamp(adj.lower(), adj.upper()));
        glib::Propagation::Proceed
    });
    ctl
}

fn apply_classes(widget: &impl IsA<gtk::Widget>, classes: &[String]) {
    for class in classes {
        widget.add_css_class(class);
    }
}

/// Apply the minimal class delta: remove what dropped, add what's new. Only
/// classes the reconciler itself added (those in `old`) are ever removed, so
/// a widget's built-in style classes are untouched.
fn reconcile_classes(widget: &impl IsA<gtk::Widget>, old: &[String], new: &[String]) {
    for class in old {
        if !new.contains(class) {
            widget.remove_css_class(class);
        }
    }
    for class in new {
        if !old.contains(class) {
            widget.add_css_class(class);
        }
    }
}

fn downcast<T: IsA<gtk::Widget>>(widget: &gtk::Widget) -> &T {
    widget
        .downcast_ref::<T>()
        .expect("kind invariant: widget type matches its node kind")
}

fn node_kind(node: &Node) -> NodeKind {
    match node {
        Node::Box { .. } => NodeKind::Box,
        Node::Row { .. } => NodeKind::Row,
        Node::ListBox { .. } => NodeKind::ListBox,
        Node::Scrolled { .. } => NodeKind::Scrolled,
        Node::Label { .. } => NodeKind::Label,
        Node::Text { .. } => NodeKind::Text,
        Node::Icon { .. } => NodeKind::Icon,
        Node::Pixels { .. } => NodeKind::Pixels,
        Node::GlSurface { .. } => NodeKind::GlSurface,
        Node::Shader { .. } => NodeKind::Shader,
        Node::Button { .. } => NodeKind::Button,
        Node::Progress { .. } => NodeKind::Progress,
        Node::Slider { .. } => NodeKind::Slider,
        Node::Revealer { .. } => NodeKind::Revealer,
        Node::Separator { .. } => NodeKind::Separator,
        Node::Spacer => NodeKind::Spacer,
        Node::Expander { .. } => NodeKind::Expander,
        Node::Entry { .. } => NodeKind::Entry,
    }
}

fn node_id(node: &Node) -> Option<&str> {
    match node {
        Node::Box { id, .. }
        | Node::Row { id, .. }
        | Node::ListBox { id, .. }
        | Node::Label { id, .. }
        | Node::Text { id, .. }
        | Node::Icon { id, .. }
        | Node::Pixels { id, .. }
        | Node::GlSurface { id, .. }
        | Node::Shader { id, .. }
        | Node::Progress { id, .. }
        | Node::Revealer { id, .. }
        | Node::Scrolled { id, .. } => id.as_deref(),
        // `Button`, `Slider`, `Expander`, and `Entry` all require an id — it is
        // their event target (a click for Button/Expander, a value change for
        // Slider, a text submit for Entry).
        Node::Button { id, .. }
        | Node::Slider { id, .. }
        | Node::Expander { id, .. }
        | Node::Entry { id, .. } => Some(id.as_str()),
        Node::Separator { .. } | Node::Spacer => None,
    }
}

fn node_classes(node: &Node) -> &[String] {
    match node {
        Node::Box { classes, .. }
        | Node::Row { classes, .. }
        | Node::ListBox { classes, .. }
        | Node::Label { classes, .. }
        | Node::Text { classes, .. }
        | Node::Icon { classes, .. }
        | Node::Pixels { classes, .. }
        | Node::GlSurface { classes, .. }
        | Node::Shader { classes, .. }
        | Node::Button { classes, .. }
        | Node::Progress { classes, .. }
        | Node::Slider { classes, .. }
        | Node::Expander { classes, .. }
        | Node::Entry { classes, .. }
        | Node::Separator { classes }
        // `Scrolled` *does* carry classes, unlike `Revealer`: a viewport has a
        // frame, and a plugin that wants one styled (a rule, a fade) has no
        // other handle on it — the `GtkScrolledWindow` is not a node it sent.
        | Node::Scrolled { classes, .. } => classes,
        // `Revealer` carries no classes of its own (see the `Node` vocab); it
        // is a transparent open/close wrapper, so style its child instead.
        // `Spacer` is style-less on purpose — a structural gap, never itself
        // themed.
        Node::Revealer { .. } | Node::Spacer => &[],
    }
}

/// A node's **effective** hover text, for the seven variants that carry one
/// (#957, #893, #961); `None` both for "this kind has no tooltip field" and for
/// "this node set none" — the two are indistinguishable to the widget, which
/// either has a tooltip or doesn't.
///
/// Deliberately shaped like [`node_classes`]: one accessor consulted by
/// [`build_node`] and [`update_in_place`] alike, so an eighth variant growing a
/// tooltip is a one-line change here rather than two per-arm edits that can
/// drift apart. It is also why [`Node::Text`]'s **default** lives here and not
/// in an arm: the effective string is what [`desc_of`] snapshots, so a `Text`
/// whose `text` changes re-applies the derived hover for free, and one that
/// stops ellipsizing has its hover cleared by the same `prev != new` compare
/// that clears an explicit one.
///
/// A **blank** string (empty, or only whitespace) is `None` here, for every
/// variant and for the derived `Text` string alike (#971 review, LOW-1). GTK
/// normalises `""` to no tooltip on its own but does **not** normalise `"   "`:
/// `has-tooltip` goes true and hovering pops an empty tooltip window. A blank
/// hover is strictly worse than none, and the filter sits at this seam rather
/// than in [`apply_tooltip`] so [`desc_of`]'s snapshot records the *effective*
/// value — otherwise a drop from `Some("  ")` to `None` would look like a
/// change to the reconciler while being a no-op on the widget.
fn node_tooltip(node: &Node) -> Option<&str> {
    let tooltip = match node {
        Node::Box { tooltip, .. }
        | Node::Label { tooltip, .. }
        | Node::Icon { tooltip, .. }
        // #893's shader widget is the fourth. It earns one for the reason the
        // first three did: a shader chip is a picture with nowhere to say what
        // it is, and the wire already carried the field (#968 review M3 — it
        // was a documented, SDK-exposed, golden-pinned no-op before this arm).
        | Node::Shader { tooltip, .. }
        // #961's three. `Row` is the list card's legend seam (the agents plugin
        // was spelling rows as horizontal `Box`es purely to get one), and
        // `Expander`'s is armed on the header — see `tooltip_target`.
        | Node::Row { tooltip, .. }
        | Node::Expander { tooltip, .. } => tooltip.as_deref(),
        // `Text` is the one variant with a default: an ellipsizing label
        // truncates with `…` and the full string is otherwise unreachable, so
        // absent an explicit tooltip the text *is* the tooltip (#961). An
        // explicit one always wins; a non-ellipsizing `Text` gets none.
        //
        // Not gated on `Label::layout().is_ellipsized()`: that is a function of
        // the allocation, so honouring it would mean a per-label size-allocate
        // hook re-deciding the hover on every layout pass. The cost of not
        // gating is a redundant hover on a short string in a wide container —
        // a legend, not a bug.
        Node::Text {
            tooltip,
            text,
            ellipsize,
            ..
        } => tooltip
            .as_deref()
            .or_else(|| ellipsize.then_some(text.as_str())),
        _ => None,
    };
    tooltip.filter(|t| !t.trim().is_empty())
}

/// The widget a node's tooltip is armed on.
///
/// For every kind this is the node's own widget — except [`Node::Expander`],
/// whose widget is the **outer** vertical box holding both the header and the
/// revealed body. Arming there would float the header's legend over the body
/// too, including over children that deliberately carry no tooltip of their own
/// (GTK resolves a hover against the deepest widget under the pointer and walks
/// *up* until one answers). The header button is what the pointer is over when
/// it hovers "the row", and it is already the click target, so that is where
/// the text belongs (#961).
///
/// One function rather than two per-call-site conditionals, for the same reason
/// [`node_tooltip`] is one accessor: [`build_node`] and [`update_in_place`] must
/// not be able to arm a tooltip on different widgets. Falsify it by returning
/// `widget.clone()` unconditionally —
/// `an_expander_arms_its_tooltip_on_the_header_not_the_body` goes red.
fn tooltip_target(widget: &gtk::Widget, expander: Option<&ExpanderState>) -> gtk::Widget {
    expander.map_or_else(
        || widget.clone(),
        |es| es.header_button.clone().upcast::<gtk::Widget>(),
    )
}

/// Apply a node's tooltip to its realized widget. `None` clears it: GTK's
/// `set_tooltip_text(None)` removes the tooltip and unsets `has-tooltip`, which
/// is what makes a plugin dropping the field actually take the hover text away.
fn apply_tooltip(widget: &gtk::Widget, tooltip: Option<&str>) {
    widget.set_tooltip_text(tooltip);
}

/// Reconcile a reused widget's tooltip against the one it was last rendered
/// with.
///
/// The `prev` argument is what makes the **clearing** case correct — see
/// [`NodeDesc::tooltip`]. The `prev != new` guard itself buys nothing at
/// runtime: `gtk_widget_set_tooltip_text` already dedups by value, so calling it
/// with an unchanged string emits no `notify::tooltip-text` either way. It is
/// kept for shape (one seam, mirroring [`reconcile_classes`]) and because
/// skipping a no-op call is free; do **not** read it as the thing preventing
/// per-tick churn.
fn reconcile_tooltip(widget: &gtk::Widget, prev: Option<&str>, new: Option<&str>) {
    if prev != new {
        apply_tooltip(widget, new);
    }
}

fn child_key(node: &Node) -> ChildKey {
    ChildKey {
        id: node_id(node).map(ToOwned::to_owned),
        kind: node_kind(node),
    }
}

fn desc_of(node: &Node) -> NodeDesc {
    NodeDesc {
        id: node_id(node).map(ToOwned::to_owned),
        kind: node_kind(node),
        classes: node_classes(node).to_vec(),
        tooltip: node_tooltip(node).map(ToOwned::to_owned),
    }
}

// ── Pure diff-plan tests (hermetic — no display server) ──────────────────────

#[cfg(test)]
mod diff_tests {
    use super::{
        ChildKey, DiffPlan, Node, NodeKind, SlotOp, child_key, node_classes, node_id, node_kind,
        plan_diff,
    };
    use crate::gl_surface::{GlProgram, GlUniforms};
    use std::sync::Arc;

    fn key(id: Option<&str>, kind: NodeKind) -> ChildKey {
        ChildKey {
            id: id.map(ToOwned::to_owned),
            kind,
        }
    }

    fn lbl(id: Option<&str>) -> ChildKey {
        key(id, NodeKind::Label)
    }

    #[test]
    fn empty_to_empty_is_noop() {
        let plan = plan_diff(&[], &[]);
        assert_eq!(
            plan,
            DiffPlan {
                ops: vec![],
                removals: vec![]
            }
        );
    }

    #[test]
    fn first_build_creates_all() {
        let next = vec![lbl(Some("a")), lbl(Some("b"))];
        let plan = plan_diff(&[], &next);
        assert_eq!(plan.ops, vec![SlotOp::Create, SlotOp::Create]);
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn unchanged_keyed_reuses_in_order() {
        let prev = vec![lbl(Some("a")), lbl(Some("b"))];
        let next = prev.clone();
        let plan = plan_diff(&prev, &next);
        assert_eq!(plan.ops, vec![SlotOp::Reuse(0), SlotOp::Reuse(1)]);
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn keyed_insert_in_middle() {
        let prev = vec![lbl(Some("a")), lbl(Some("b"))];
        let next = vec![lbl(Some("a")), lbl(Some("z")), lbl(Some("b"))];
        let plan = plan_diff(&prev, &next);
        assert_eq!(
            plan.ops,
            vec![SlotOp::Reuse(0), SlotOp::Create, SlotOp::Reuse(1)]
        );
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn keyed_remove() {
        let prev = vec![lbl(Some("a")), lbl(Some("b")), lbl(Some("c"))];
        let next = vec![lbl(Some("a")), lbl(Some("c"))];
        let plan = plan_diff(&prev, &next);
        assert_eq!(plan.ops, vec![SlotOp::Reuse(0), SlotOp::Reuse(2)]);
        assert_eq!(plan.removals, vec![1]);
    }

    #[test]
    fn keyed_reorder_reuses_every_widget() {
        let prev = vec![lbl(Some("a")), lbl(Some("b")), lbl(Some("c"))];
        let next = vec![lbl(Some("c")), lbl(Some("a")), lbl(Some("b"))];
        let plan = plan_diff(&prev, &next);
        assert_eq!(
            plan.ops,
            vec![SlotOp::Reuse(2), SlotOp::Reuse(0), SlotOp::Reuse(1)]
        );
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn keyed_kind_change_recreates() {
        // Same id "a" but Label → Button: the widget can't be reused.
        let prev = vec![lbl(Some("a"))];
        let next = vec![key(Some("a"), NodeKind::Button)];
        let plan = plan_diff(&prev, &next);
        assert_eq!(plan.ops, vec![SlotOp::Create]);
        assert_eq!(plan.removals, vec![0]);
    }

    #[test]
    fn consecutive_id_less_spacers_reuse_by_kind() {
        // A `Spacer` has no id, so a pair of them keys purely by kind — the
        // positional keyless path must reuse both across an identical re-render
        // (they're interchangeable), never churn them.
        let sp = || key(None, NodeKind::Spacer);
        let prev = vec![sp(), lbl(None), sp()];
        let next = vec![sp(), lbl(None), sp()];
        let plan = plan_diff(&prev, &next);
        assert_eq!(
            plan.ops,
            vec![SlotOp::Reuse(0), SlotOp::Reuse(1), SlotOp::Reuse(2)]
        );
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn keyless_positional_match() {
        // No ids anywhere → matched purely by position, same kind.
        let prev = vec![lbl(None), lbl(None)];
        let next = vec![lbl(None), lbl(None)];
        let plan = plan_diff(&prev, &next);
        assert_eq!(plan.ops, vec![SlotOp::Reuse(0), SlotOp::Reuse(1)]);
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn keyless_kind_mismatch_at_position_recreates() {
        // Position 0 was a Label, now an Icon → create + remove the Label;
        // position 1 (Icon ↔ Icon) reuses.
        let prev = vec![lbl(None), key(None, NodeKind::Icon)];
        let next = vec![key(None, NodeKind::Icon), key(None, NodeKind::Icon)];
        let plan = plan_diff(&prev, &next);
        assert_eq!(plan.ops, vec![SlotOp::Create, SlotOp::Reuse(1)]);
        assert_eq!(plan.removals, vec![0]);
    }

    #[test]
    fn keyless_append_and_truncate() {
        // Grow then shrink the keyless list.
        let grow = plan_diff(&[lbl(None)], &[lbl(None), lbl(None)]);
        assert_eq!(grow.ops, vec![SlotOp::Reuse(0), SlotOp::Create]);
        assert!(grow.removals.is_empty());

        let shrink = plan_diff(&[lbl(None), lbl(None)], &[lbl(None)]);
        assert_eq!(shrink.ops, vec![SlotOp::Reuse(0)]);
        assert_eq!(shrink.removals, vec![1]);
    }

    #[test]
    fn mixed_keyed_and_keyless() {
        // Keyed children are matched by id regardless of position; the
        // keyless one falls back to the next free keyless prev slot.
        let prev = vec![key(Some("a"), NodeKind::Button), lbl(None), lbl(Some("c"))];
        // New order: keyless first, then "a", drop "c".
        let next = vec![lbl(None), key(Some("a"), NodeKind::Button)];
        let plan = plan_diff(&prev, &next);
        assert_eq!(plan.ops, vec![SlotOp::Reuse(1), SlotOp::Reuse(0)]);
        assert_eq!(plan.removals, vec![2]);
    }

    #[test]
    fn vanished_keyed_is_removed_not_positionally_reused() {
        // "a" disappears; a new keyless Label must NOT silently adopt "a"'s
        // widget (keyed prev children are invisible to positional matching).
        let prev = vec![lbl(Some("a"))];
        let next = vec![lbl(None)];
        let plan = plan_diff(&prev, &next);
        assert_eq!(plan.ops, vec![SlotOp::Create]);
        assert_eq!(plan.removals, vec![0]);
    }

    // ── Node::GlSurface (#893 stage B) ──────────────────────────────────────

    /// A `GlSurface` node reports its own kind and carries its id and classes
    /// through the three shallow accessors every diff pass reads — the sites a
    /// new `Node` variant is silently *missed* at, because `node_id` and
    /// `node_classes` both end in a catch-all-ish `or`-pattern arm that
    /// compiles perfectly well without the new variant listed.
    ///
    /// **Falsified** by dropping `Node::GlSurface` from `node_id`'s or
    /// `node_classes`' arm: the node would land in `Separator | Spacer`'s
    /// "no id, no classes" bucket, every GL chip in a tree would key as
    /// `(None, GlSurface)`, and they would swap widgets — and phosphor trails
    /// — on any insert.
    #[test]
    fn a_gl_surface_node_carries_its_kind_id_and_classes() {
        let node = Node::GlSurface {
            id: Some("scope".to_owned()),
            width: 288,
            height: 96,
            program: GlProgram("preem.scope"),
            state: Arc::new(GlUniforms::default()),
            classes: vec!["ts-preem".to_owned()],
        };
        assert_eq!(node_kind(&node), NodeKind::GlSurface);
        assert_eq!(node_id(&node), Some("scope"));
        assert_eq!(node_classes(&node), ["ts-preem".to_owned()]);
        assert_eq!(child_key(&node), key(Some("scope"), NodeKind::GlSurface));
    }

    /// A `GlSurface` is its **own kind**, so an id reused across the
    /// `Pixels` ⇄ `GlSurface` boundary rebuilds instead of reusing the widget.
    ///
    /// This is the arm that matters for the kill switch: with
    /// `TROLLSHELL_PREEM_RENDERER=cpu` the shell emits `Pixels` for a `Scope`
    /// and without it a `GlSurface`, under the *same node id*. If both mapped
    /// to one `NodeKind`, `update_in_place` would downcast a `PixelSurface` to
    /// a `GlSurface` and hit the `downcast` expect — the "kind invariant"
    /// panic — on the first frame after a flip.
    ///
    /// **Falsified** by dropping the `&& prev[i].kind == nk.kind` clause from
    /// [`plan_diff`]'s keyed lookup: both directions then reuse.
    ///
    /// (An earlier version of this note claimed "falsified by mapping
    /// `Node::GlSurface` to `NodeKind::Pixels`". Measured while writing the
    /// `Shader` twin below: that mutation leaves this test **green**, because
    /// it builds its `ChildKey`s by hand and never calls `node_kind`. Corrected
    /// rather than left standing — in a tree where these claims are the review
    /// currency, a wrong one costs more than a missing one. The `node_kind`
    /// mapping is covered by
    /// [`a_gl_surface_node_carries_its_kind_id_and_classes`] above.)
    #[test]
    fn a_gl_surface_never_reuses_a_pixels_widget() {
        let prev = vec![key(Some("scope"), NodeKind::Pixels)];
        let next = vec![key(Some("scope"), NodeKind::GlSurface)];
        let plan = plan_diff(&prev, &next);
        assert_eq!(plan.ops, vec![SlotOp::Create]);
        assert_eq!(plan.removals, vec![0], "the raster surface is torn down");

        // …and back the other way, which is the kill switch being turned on.
        let back = plan_diff(&next, &prev);
        assert_eq!(back.ops, vec![SlotOp::Create]);
        assert_eq!(back.removals, vec![0]);
    }

    /// A same-id `GlSurface` re-render reuses its widget in place — the whole
    /// point of the node, since a rebuilt surface would drop its phosphor
    /// textures and its `last_drawn` step count on every frame.
    #[test]
    fn a_same_id_gl_surface_reuses_in_place() {
        let prev = vec![key(Some("scope"), NodeKind::GlSurface)];
        let next = prev.clone();
        let plan = plan_diff(&prev, &next);
        assert_eq!(plan.ops, vec![SlotOp::Reuse(0)]);
        assert!(plan.removals.is_empty());
    }

    /// GL surfaces reorder and insert like every other keyed child — the
    /// generic path, asserted here because a chip losing its accumulator to a
    /// sibling's insert is a *visible* regression (a phosphor trail jumping
    /// widgets) rather than a layout one.
    #[test]
    fn gl_surfaces_reorder_without_rebuilding() {
        let gl = |id| key(Some(id), NodeKind::GlSurface);
        let prev = vec![gl("a"), gl("b"), gl("c")];
        let next = vec![gl("c"), gl("a"), gl("b")];
        let plan = plan_diff(&prev, &next);
        assert_eq!(
            plan.ops,
            vec![SlotOp::Reuse(2), SlotOp::Reuse(0), SlotOp::Reuse(1)]
        );
        assert!(plan.removals.is_empty());
    }

    // ── Node::Shader (#893, the shader widget) ──────────────────────────────

    /// A `ShaderState` shaped like a plugin's, for the diff tests below.
    fn shader_state() -> Arc<crate::shader_surface::ShaderState> {
        Arc::new(crate::shader_surface::ShaderState {
            fragment: Arc::from("void main() { fragColor = u_fg; }"),
            data: Arc::from(&[0u8, 64, 128, 255][..]),
            format: crate::shader_surface::ShaderFormat::R8,
            data_size: (4, 1),
            scale: 1,
            values: vec![(
                "u_fg",
                crate::gl_surface::GlValue::Vec4([1.0, 1.0, 1.0, 1.0]),
            )],
        })
    }

    /// A `Shader` node reports its own kind and carries its id and classes
    /// through the three shallow accessors every diff pass reads.
    ///
    /// **Falsified** three ways, each measured rather than assumed:
    ///
    /// - `node_kind` returning `NodeKind::Pixels` for `Node::Shader` — the
    ///   first assertion. (This is the *only* test that covers that mapping;
    ///   `a_shader_never_reuses_a_pixels_or_gl_surface_widget` below builds its
    ///   `ChildKey`s by hand and stays green through it — verified.)
    /// - **Moving** `Node::Shader` into `node_id`'s `Separator | Spacer => None`
    ///   arm. Simply *omitting* it is a compile error — both matches are
    ///   exhaustive — so the reachable mistake is the wrong arm, which compiles
    ///   and silently keys every shader chip in a tree as `(None, Shader)`; they
    ///   then swap widgets, and compiled programs, on any insert.
    /// - The same move into `node_classes`' `Revealer | Spacer => &[]` arm.
    #[test]
    fn a_shader_node_carries_its_kind_id_and_classes() {
        let node = Node::Shader {
            id: Some("spectrum".to_owned()),
            width: 288,
            height: 96,
            state: shader_state(),
            classes: vec!["ts-shader".to_owned()],
            tooltip: None,
        };
        assert_eq!(node_kind(&node), NodeKind::Shader);
        assert_eq!(node_id(&node), Some("spectrum"));
        assert_eq!(node_classes(&node), ["ts-shader".to_owned()]);
        assert_eq!(child_key(&node), key(Some("spectrum"), NodeKind::Shader));
    }

    /// A `Shader` is its **own kind** against *both* of its neighbours, so an id
    /// reused across either boundary rebuilds rather than reusing the widget.
    ///
    /// This is not hypothetical: the host renders the broken-widget placeholder
    /// — a `Node::Pixels` of 0×0 — under the **same node id** when a plugin
    /// lacks `Capability::Shader` or busts a size cap, so a plugin that fixes
    /// its manifest flips `Pixels` → `Shader` in place. If the two shared a
    /// `NodeKind`, `update_in_place` would downcast a `PixelSurface` to a
    /// `ShaderSurface` and hit the kind-invariant `expect` — an abort — on the
    /// first frame after the flip. `GlSurface` is the same story from the other
    /// side: it is also a `GtkGLArea` subclass, so nothing but the kind stops
    /// them being confused.
    ///
    /// **Falsified** by dropping the `&& prev[i].kind == nk.kind` clause from
    /// [`plan_diff`]'s keyed lookup: both pairs then reuse. (Not by editing
    /// `ChildKey`'s `PartialEq` — `plan_diff` compares the `kind` fields
    /// directly and never uses that impl, measured; a mutation there leaves this
    /// green.)
    ///
    /// **What it does not cover, stated rather than implied:** this builds its
    /// `ChildKey`s directly, so it says nothing about `node_kind` — mapping
    /// `Node::Shader` to `NodeKind::Pixels` leaves it green, measured, not
    /// assumed. That mapping is
    /// [`a_shader_node_carries_its_kind_id_and_classes`]'s first assertion.
    #[test]
    fn a_shader_never_reuses_a_pixels_or_gl_surface_widget() {
        for neighbour in [NodeKind::Pixels, NodeKind::GlSurface] {
            let prev = vec![key(Some("chip"), neighbour)];
            let next = vec![key(Some("chip"), NodeKind::Shader)];
            let plan = plan_diff(&prev, &next);
            assert_eq!(plan.ops, vec![SlotOp::Create], "{neighbour:?} → Shader");
            assert_eq!(
                plan.removals,
                vec![0],
                "the {neighbour:?} widget is torn down"
            );

            // …and back the other way, which is the placeholder path.
            let back = plan_diff(&next, &prev);
            assert_eq!(back.ops, vec![SlotOp::Create], "Shader → {neighbour:?}");
            assert_eq!(back.removals, vec![0]);
        }
    }

    /// A same-id `Shader` re-render reuses its widget in place — which is what
    /// makes "compiled once" true across frames, since a rebuilt surface drops
    /// its program cache and restarts `u_time`.
    #[test]
    fn a_same_id_shader_reuses_in_place() {
        let prev = vec![key(Some("spectrum"), NodeKind::Shader)];
        let next = prev.clone();
        let plan = plan_diff(&prev, &next);
        assert_eq!(plan.ops, vec![SlotOp::Reuse(0)]);
        assert!(plan.removals.is_empty());
    }

    /// Shader surfaces reorder and insert like every other keyed child. Worth
    /// asserting rather than assuming: a chip that lost its widget to a
    /// sibling's insert would recompile its shader on that frame, which is the
    /// one cost this whole node exists to avoid.
    #[test]
    fn shaders_reorder_without_rebuilding() {
        let sh = |id| key(Some(id), NodeKind::Shader);
        let prev = vec![sh("a"), sh("b"), sh("c")];
        let next = vec![sh("c"), sh("a"), sh("b")];
        let plan = plan_diff(&prev, &next);
        assert_eq!(
            plan.ops,
            vec![SlotOp::Reuse(2), SlotOp::Reuse(0), SlotOp::Reuse(1)]
        );
        assert!(plan.removals.is_empty());
    }

    #[test]
    fn partition_invariant_holds() {
        // Every prev index is reused exactly once or removed exactly once.
        let prev = vec![
            lbl(Some("a")),
            lbl(None),
            key(Some("b"), NodeKind::Icon),
            lbl(None),
        ];
        let next = vec![lbl(None), lbl(Some("a")), key(Some("x"), NodeKind::Icon)];
        let plan = plan_diff(&prev, &next);

        let mut seen = vec![0usize; prev.len()];
        for op in &plan.ops {
            if let SlotOp::Reuse(i) = *op {
                seen[i] += 1;
            }
        }
        for &i in &plan.removals {
            seen[i] += 1;
        }
        assert!(
            seen.iter().all(|&c| c == 1),
            "each prev index used exactly once: {seen:?}"
        );
    }
}

// ── Slider throttle / drag-suppression (pure — hermetic) ─────────────────────

#[cfg(test)]
mod slider_tests {
    use super::{ThrottleAction, slider_suppress_set, throttle_decision};
    use std::time::{Duration, Instant};

    const CADENCE: Duration = Duration::from_millis(50);
    const WINDOW: Duration = Duration::from_millis(250);

    #[test]
    fn first_move_emits_on_leading_edge() {
        // Never emitted → emit immediately, whatever `now` is.
        assert_eq!(
            throttle_decision(None, Instant::now(), CADENCE),
            ThrottleAction::Emit
        );
    }

    #[test]
    fn move_after_cadence_emits() {
        let t0 = Instant::now();
        // A full cadence (or more) since the last emit → emit again.
        assert_eq!(
            throttle_decision(Some(t0), t0 + CADENCE, CADENCE),
            ThrottleAction::Emit
        );
        assert_eq!(
            throttle_decision(Some(t0), t0 + CADENCE + Duration::from_millis(10), CADENCE),
            ThrottleAction::Emit
        );
    }

    #[test]
    fn move_within_cadence_defers_by_remaining() {
        let t0 = Instant::now();
        // 20 ms into a 50 ms window → defer the remaining 30 ms (trailing flush).
        assert_eq!(
            throttle_decision(Some(t0), t0 + Duration::from_millis(20), CADENCE),
            ThrottleAction::Defer(Duration::from_millis(30))
        );
    }

    #[test]
    fn set_value_suppressed_only_within_grab_window() {
        let t0 = Instant::now();
        // No user interaction yet → never suppress a render-driven set_value.
        assert!(!slider_suppress_set(None, t0, WINDOW));
        // A move 100 ms ago (< window) → still "dragging", suppress the echo.
        assert!(slider_suppress_set(
            Some(t0),
            t0 + Duration::from_millis(100),
            WINDOW
        ));
        // A move older than the window → the grab settled, apply the echo.
        assert!(!slider_suppress_set(
            Some(t0),
            t0 + WINDOW + Duration::from_millis(1),
            WINDOW
        ));
    }
}

// ── GTK integration tests (need a display → gated to `system-tests`) ─────────

#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    // `#[gtk::test]` runs every test on one shared GTK main thread (GTK is
    // single-threaded), so these run serially but correctly under the default
    // multithreaded `cargo test` harness — no manual `gtk::init` juggling.
    use super::{DENSE_ROW_CLASS, Dir, EventKind, Node, Reconciler};
    use gtk::glib;
    use gtk::prelude::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    fn root() -> gtk::Box {
        gtk::Box::new(gtk::Orientation::Horizontal, 0)
    }

    fn children(widget: &impl IsA<gtk::Widget>) -> Vec<gtk::Widget> {
        let mut out = Vec::new();
        let mut cursor = widget.first_child();
        while let Some(child) = cursor {
            cursor = child.next_sibling();
            out.push(child);
        }
        out
    }

    fn lbl(id: Option<&str>, text: &str) -> Node {
        Node::Label {
            id: id.map(ToOwned::to_owned),
            text: text.to_owned(),
            classes: vec![],
            tooltip: None,
        }
    }

    fn btn(id: &str, text: &str) -> Node {
        Node::Button {
            id: id.to_owned(),
            classes: vec![],
            child: Box::new(lbl(None, text)),
        }
    }

    fn hbox(children: Vec<Node>) -> Node {
        Node::Box {
            id: None,
            dir: Dir::Horizontal,
            spacing: 0,
            scroll: false,
            classes: vec![],
            children,
            tooltip: None,
        }
    }

    #[gtk::test]
    fn first_render_builds_structure() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&hbox(vec![lbl(Some("a"), "x"), lbl(Some("b"), "y")]));

        let inner = root.first_child().expect("box mounted");
        let kids = children(&inner);
        assert_eq!(kids.len(), 2);
        let first = kids[0].downcast_ref::<gtk::Label>().expect("label");
        assert_eq!(first.text().as_str(), "x");
    }

    #[gtk::test]
    fn keyed_update_reuses_widget_and_sets_text() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&hbox(vec![lbl(Some("a"), "x")]));
        let inner1 = root.first_child().unwrap();
        let label1 = inner1.first_child().unwrap();

        rec.render(&hbox(vec![lbl(Some("a"), "y")]));
        let inner2 = root.first_child().unwrap();
        let label2 = inner2.first_child().unwrap();

        assert_eq!(inner1, inner2, "box reused");
        assert_eq!(label1, label2, "label reused, not recreated");
        let label = label2.downcast::<gtk::Label>().unwrap();
        assert_eq!(label.text().as_str(), "y");
    }

    #[gtk::test]
    fn reorder_preserves_widget_identity() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&hbox(vec![btn("a", "A"), btn("b", "B")]));
        let before = children(&root.first_child().unwrap());

        rec.render(&hbox(vec![btn("b", "B"), btn("a", "A")]));
        let after = children(&root.first_child().unwrap());

        assert_eq!(after.len(), 2);
        assert_eq!(after[0], before[1], "b moved to front, same widget");
        assert_eq!(after[1], before[0], "a moved to back, same widget");
    }

    #[gtk::test]
    fn button_click_fires_once_even_after_reuse() {
        let root = root();
        let events: Rc<RefCell<Vec<(String, EventKind)>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        let mut rec = Reconciler::new(&root, move |id, kind| sink.borrow_mut().push((id, kind)));

        rec.render(&hbox(vec![btn("go", "Go")]));
        // Re-render the identical tree: the button is reused. If the click
        // handler were re-connected on reuse, the next click would fire twice.
        rec.render(&hbox(vec![btn("go", "Go")]));

        let button = root
            .first_child()
            .unwrap()
            .first_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .unwrap();
        button.emit_clicked();

        let recorded = events.borrow();
        assert_eq!(recorded.len(), 1, "exactly one event, no double-fire");
        assert_eq!(recorded[0].0, "go");
        assert_eq!(recorded[0].1, EventKind::Click);
    }

    #[gtk::test]
    fn kind_change_recreates_widget() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&hbox(vec![lbl(Some("x"), "hi")]));
        let before = root.first_child().unwrap().first_child().unwrap();

        rec.render(&hbox(vec![btn("x", "hi")]));
        let after = root.first_child().unwrap().first_child().unwrap();

        assert_ne!(
            before, after,
            "Label→Button under same id is a fresh widget"
        );
        assert!(after.downcast::<gtk::Button>().is_ok());
    }

    #[gtk::test]
    fn class_delta_applied_on_update() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&Node::Label {
            id: None,
            text: "t".into(),
            classes: vec!["one".into()],
            tooltip: None,
        });
        let w1 = root.first_child().unwrap();
        assert!(w1.has_css_class("one"));

        rec.render(&Node::Label {
            id: None,
            text: "t".into(),
            classes: vec!["two".into()],
            tooltip: None,
        });
        let w2 = root.first_child().unwrap();
        assert_eq!(w1, w2, "reused via positional match");
        assert!(w2.has_css_class("two"));
        assert!(!w2.has_css_class("one"), "dropped class removed");
    }

    #[gtk::test]
    fn progress_and_revealer_update_in_place() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});

        rec.render(&Node::Progress {
            id: None,
            fraction: 0.25,
            classes: vec![],
        });
        let bar = root
            .first_child()
            .unwrap()
            .downcast::<gtk::ProgressBar>()
            .unwrap();
        assert!((bar.fraction() - 0.25).abs() < f64::EPSILON);
        rec.render(&Node::Progress {
            id: None,
            fraction: 0.75,
            classes: vec![],
        });
        assert!(
            (bar.fraction() - 0.75).abs() < f64::EPSILON,
            "reused bar updated"
        );

        rec.render(&Node::Revealer {
            id: None,
            open: true,
            child: Box::new(lbl(None, "body")),
        });
        let revealer = root
            .first_child()
            .unwrap()
            .downcast::<gtk::Revealer>()
            .unwrap();
        assert!(revealer.reveals_child());
        rec.render(&Node::Revealer {
            id: None,
            open: false,
            child: Box::new(lbl(None, "body")),
        });
        assert!(!revealer.reveals_child(), "reused revealer toggled closed");
    }

    #[gtk::test]
    fn nested_box_children_insert_and_remove() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&hbox(vec![lbl(Some("a"), "a")]));
        rec.render(&hbox(vec![
            lbl(Some("a"), "a"),
            lbl(Some("b"), "b"),
            lbl(Some("c"), "c"),
        ]));
        assert_eq!(children(&root.first_child().unwrap()).len(), 3);

        rec.render(&hbox(vec![lbl(Some("b"), "b")]));
        let kids = children(&root.first_child().unwrap());
        assert_eq!(kids.len(), 1);
        assert_eq!(
            kids[0]
                .downcast_ref::<gtk::Label>()
                .unwrap()
                .text()
                .as_str(),
            "b"
        );
    }

    fn pix(id: Option<&str>, width: u32, height: u32, data: &[u8]) -> Node {
        pix_shared(id, width, height, Arc::from(data), 1)
    }

    /// [`pix`] over a buffer the caller keeps a handle on, so a test can hand
    /// the *same* allocation to two renders (or two reconcilers) the way the
    /// shell's frame cache hands one frame to every monitor (#911).
    fn pix_shared(id: Option<&str>, width: u32, height: u32, data: Arc<[u8]>, scale: u32) -> Node {
        Node::Pixels {
            id: id.map(ToOwned::to_owned),
            width,
            height,
            data,
            scale,
            classes: vec![],
        }
    }

    #[gtk::test]
    fn pixels_builds_surface() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        // 1×1 opaque red.
        rec.render(&pix(Some("lcd"), 1, 1, &[255, 0, 0, 255]));
        let w = root.first_child().expect("pixel surface mounted");
        assert!(
            w.downcast_ref::<crate::pixels::PixelSurface>().is_some(),
            "Pixels maps to a PixelSurface"
        );
    }

    #[gtk::test]
    fn pixels_update_reuses_widget_on_same_id() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&pix(Some("lcd"), 1, 1, &[255, 0, 0, 255]));
        let before = root.first_child().unwrap();

        // Same id, new bytes: mutable-prop update, widget identity preserved.
        rec.render(&pix(Some("lcd"), 1, 1, &[0, 255, 0, 255]));
        let after = root.first_child().unwrap();
        assert_eq!(before, after, "same-id Pixels reuses the surface in place");
    }

    fn entry(id: &str, text: &str, placeholder: &str) -> Node {
        Node::Entry {
            id: id.to_owned(),
            text: text.to_owned(),
            placeholder: placeholder.to_owned(),
            classes: vec![],
        }
    }

    #[gtk::test]
    fn entry_builds_with_text_and_placeholder() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&entry("in", "prefill", "type here…"));
        let e = root
            .first_child()
            .expect("entry mounted")
            .downcast::<gtk::Entry>()
            .expect("Entry maps to a gtk::Entry");
        assert_eq!(e.text().as_str(), "prefill");
        assert_eq!(
            e.placeholder_text().as_deref(),
            Some("type here…"),
            "placeholder set"
        );
    }

    #[gtk::test]
    fn entry_submit_fires_once_even_after_reuse() {
        let root = root();
        let events: Rc<RefCell<Vec<(String, EventKind)>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        let mut rec = Reconciler::new(&root, move |id, kind| sink.borrow_mut().push((id, kind)));

        rec.render(&entry("in", "", ""));
        // Re-render the identical tree: the entry is reused. If the activate
        // handler were re-connected on reuse, a submit would fire twice.
        rec.render(&entry("in", "", ""));

        let e = root
            .first_child()
            .unwrap()
            .downcast::<gtk::Entry>()
            .unwrap();
        // Stand-in for the user's typing: `set_text` never fires `activate` —
        // only the explicit emit below does.
        e.set_text("caw --help");
        e.emit_activate();

        let recorded = events.borrow();
        assert_eq!(recorded.len(), 1, "exactly one event, no double-fire");
        assert_eq!(recorded[0].0, "in");
        assert_eq!(
            recorded[0].1,
            EventKind::Submitted {
                text: "caw --help".into()
            },
            "the submit carries the entry's full text"
        );
    }

    #[gtk::test]
    fn entry_echoed_text_prop_never_clobbers_typing() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&entry("in", "", "hint"));
        let e = root
            .first_child()
            .unwrap()
            .downcast::<gtk::Entry>()
            .unwrap();

        // The user types; a later re-render echoes the *unchanged* text prop
        // ("") — the reconciler must leave the widget alone (prop-diff, not
        // widget-diff), preserving the in-progress input.
        e.set_text("half-typed comm");
        rec.render(&entry("in", "", "hint"));
        assert_eq!(
            e.text().as_str(),
            "half-typed comm",
            "an unchanged text prop never clobbers user typing"
        );

        // A *changed* prop (the clear-after-submit flow) applies in place.
        rec.render(&entry("in", "cleared", "hint"));
        assert_eq!(e.text().as_str(), "cleared", "a real prop change applies");
        // …and clearing back to "" is itself a change from "cleared".
        rec.render(&entry("in", "", "hint"));
        assert_eq!(e.text().as_str(), "", "clearing to empty applies too");
    }

    #[gtk::test]
    fn entry_clear_after_submit_reapplies_equal_text_prop() {
        // The regression: an Entry resting at `text: ""` that the user typed
        // into and submitted must clear when the plugin re-renders `text: ""`.
        // The new prop *equals* the last-rendered one, so the prop-diff alone
        // would skip `set_text` and leave the typed text stuck — the submit
        // latch forces the re-assert.
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&entry("in", "", "hint"));
        let e = root
            .first_child()
            .unwrap()
            .downcast::<gtk::Entry>()
            .unwrap();

        // User types then presses Enter (submit).
        e.set_text("hello");
        e.emit_activate();

        // Plugin handles the submit and re-renders the resting prop ("") to
        // clear the field — the SAME value as the last-rendered prop.
        rec.render(&entry("in", "", "hint"));
        assert_eq!(
            e.text().as_str(),
            "",
            "a re-render after submit clears the entry even when the text prop is unchanged"
        );
    }

    #[gtk::test]
    fn entry_prefill_after_submit_applies() {
        // After a submit, a re-render with a DISTINCT non-empty text still
        // applies (the latch forces it; the prop-diff would have too).
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&entry("in", "", "hint"));
        let e = root
            .first_child()
            .unwrap()
            .downcast::<gtk::Entry>()
            .unwrap();

        e.set_text("hello");
        e.emit_activate();

        rec.render(&entry("in", "prefilled", "hint"));
        assert_eq!(
            e.text().as_str(),
            "prefilled",
            "a distinct text after submit prefills the entry"
        );
    }

    #[gtk::test]
    fn entry_submit_latch_is_one_shot() {
        // The latch is consumed by the first post-submit render. A *later* echo
        // re-render of the unchanged prop must fall back to the prop-diff path
        // and preserve the user's fresh typing — the latch must not linger and
        // clobber it.
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&entry("in", "", "hint"));
        let e = root
            .first_child()
            .unwrap()
            .downcast::<gtk::Entry>()
            .unwrap();

        // Type, submit, then the plugin's clearing render consumes the latch.
        e.set_text("hello");
        e.emit_activate();
        rec.render(&entry("in", "", "hint"));
        assert_eq!(e.text().as_str(), "", "the clearing render fired");

        // The user starts typing again; a plain echo re-render of the unchanged
        // ("") prop must leave the new input alone (latch already spent).
        e.set_text("again");
        rec.render(&entry("in", "", "hint"));
        assert_eq!(
            e.text().as_str(),
            "again",
            "once consumed, the latch never clobbers later typing"
        );
    }

    #[gtk::test]
    fn entry_placeholder_is_a_mutable_prop() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&entry("in", "", "old hint"));
        let e = root
            .first_child()
            .unwrap()
            .downcast::<gtk::Entry>()
            .unwrap();
        rec.render(&entry("in", "", "new hint"));
        assert_eq!(e.placeholder_text().as_deref(), Some("new hint"));
        // An empty placeholder clears the hint. (GTK reports a cleared
        // placeholder as `Some("")` once one was set — either way, nothing
        // renders.)
        rec.render(&entry("in", "", ""));
        assert_eq!(e.placeholder_text().as_deref().unwrap_or(""), "");
    }

    /// **#911.** The reconciler hands a `Node::Pixels` buffer to its surface
    /// *shared*, so a re-render carrying the same allocation — one monitor's
    /// static chip re-mapped because a sibling animated, once per frame the
    /// clock delivers — is settled by an `Arc::ptr_eq` inside the surface: no
    /// texture rebuilt,
    /// no invalidation queued, and not one byte of the buffer read.
    ///
    /// The byte-compare tally is what makes this a test of *this* line rather
    /// than of the #902 guard: the slice setter reaches the identical
    /// `(builds, draws, resizes)` verdict, just by scanning 4 KiB to get there
    /// on every one of those re-renders.
    #[gtk::test]
    fn pixels_re_render_of_one_shared_buffer_reads_no_bytes() {
        // Two containers up front: one per "monitor", bound before `root` is
        // shadowed by the first of them.
        let (root, other_root) = (root(), root());
        let mut rec = Reconciler::new(&root, |_, _| {});
        // 64×16 RGBA8 = 4 KiB, a bar-sized preem chip.
        let frame: Arc<[u8]> = Arc::from([0xab_u8; 4096].as_slice());
        for _ in 0..10 {
            rec.render(&pix_shared(Some("lcd"), 64, 16, Arc::clone(&frame), 1));
        }
        let surface = root
            .first_child()
            .unwrap()
            .downcast::<crate::pixels::PixelSurface>()
            .unwrap();
        assert_eq!(
            surface.counts().builds,
            1,
            "ten renders of one buffer must upload exactly one texture",
        );
        assert_eq!(
            surface.bytes_compared(),
            0,
            "the reconciler must pass the buffer through shared — a byte compare here means \
             it copied or fell back to the slice setter",
        );

        // The multi-monitor shape: a second reconciler over its own container
        // is handed the very same allocation and uploads it once, without a
        // copy and without a scan.
        let mut other = Reconciler::new(&other_root, |_, _| {});
        for _ in 0..10 {
            other.render(&pix_shared(Some("lcd"), 64, 16, Arc::clone(&frame), 1));
        }
        let second = other_root
            .first_child()
            .unwrap()
            .downcast::<crate::pixels::PixelSurface>()
            .unwrap();
        assert_eq!(second.counts().builds, 1);
        assert_eq!(
            second.bytes_compared(),
            0,
            "a second monitor's surface pays a pointer compare per tick, not a buffer scan",
        );

        // …and a frame that really did change still gets through.
        rec.render(&pix_shared(
            Some("lcd"),
            64,
            16,
            Arc::from([0xcd_u8; 4096].as_slice()),
            1,
        ));
        assert_eq!(surface.counts().builds, 2, "a new frame must still upload");
    }

    #[gtk::test]
    fn pixels_scale_is_a_mutable_prop() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&pix(Some("lcd"), 1, 1, &[255, 0, 0, 255]));
        let surface = root
            .first_child()
            .unwrap()
            .downcast::<crate::pixels::PixelSurface>()
            .unwrap();
        assert_eq!(surface.measure(gtk::Orientation::Horizontal, -1).1, 1);

        // Same id, new scale: mutable-prop update — the widget is reused and
        // its natural request grows to buffer × scale.
        rec.render(&pix_shared(
            Some("lcd"),
            1,
            1,
            Arc::from([255, 0, 0, 255].as_slice()),
            4,
        ));
        assert_eq!(
            root.first_child().unwrap(),
            surface.clone().upcast::<gtk::Widget>(),
            "same-id scale flip reuses the surface in place"
        );
        assert_eq!(surface.measure(gtk::Orientation::Horizontal, -1).1, 4);
    }

    #[gtk::test]
    fn pixels_bad_buffer_does_not_panic() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        // data.len() (3) != 2*2*4: the widget must degrade to rendering nothing
        // rather than hand MemoryTexture::new an under-sized buffer.
        rec.render(&pix(Some("lcd"), 2, 2, &[1, 2, 3]));
        let w = root
            .first_child()
            .unwrap()
            .downcast::<crate::pixels::PixelSurface>()
            .unwrap();
        // A subsequent valid frame still renders in place (kind/id unchanged).
        rec.render(&pix(Some("lcd"), 1, 1, &[9, 9, 9, 255]));
        assert_eq!(root.first_child().unwrap(), w.upcast::<gtk::Widget>());
    }

    #[gtk::test]
    fn scroll_flag_attaches_and_detaches_independent_of_id() {
        use gtk::gio::prelude::ListModelExt;

        fn hbox_scroll(scroll: bool) -> Node {
            Node::Box {
                id: None,
                dir: Dir::Horizontal,
                spacing: 0,
                scroll,
                classes: vec![],
                children: vec![],
                tooltip: None,
            }
        }

        // Baseline controller count for a plain `gtk::Box`, so the assertions
        // below don't assume this GTK version attaches zero controllers by
        // default.
        let base = gtk::Box::new(gtk::Orientation::Horizontal, 0)
            .observe_controllers()
            .n_items();

        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});

        // First render, no id at all, scroll: true → controller attached at
        // build time. Proves scroll no longer needs an id to activate.
        rec.render(&hbox_scroll(true));
        let inner = root.first_child().unwrap();
        assert_eq!(
            inner.observe_controllers().n_items(),
            base + 1,
            "scroll controller attached on first build, no id required"
        );

        // Same kind/id (both keyless) → reused in place; toggling `scroll`
        // off detaches the controller without rebuilding the widget.
        rec.render(&hbox_scroll(false));
        let inner2 = root.first_child().unwrap();
        assert_eq!(inner, inner2, "box reused across a scroll-flag flip");
        assert_eq!(
            inner2.observe_controllers().n_items(),
            base,
            "scroll controller detached in place"
        );

        // Toggling back on reattaches, still the same widget identity.
        rec.render(&hbox_scroll(true));
        let inner3 = root.first_child().unwrap();
        assert_eq!(inner, inner3, "box still reused");
        assert_eq!(inner3.observe_controllers().n_items(), base + 1);
    }

    fn text(id: Option<&str>, s: &str, max: Option<i32>) -> Node {
        Node::Text {
            id: id.map(ToOwned::to_owned),
            text: s.to_owned(),
            max_width_chars: max,
            ellipsize: false,
            classes: vec![],
            tooltip: None,
        }
    }

    fn listbox(id: Option<&str>, children: Vec<Node>) -> Node {
        Node::ListBox {
            id: id.map(ToOwned::to_owned),
            classes: vec![],
            dense: false,
            children,
        }
    }

    fn row(id: Option<&str>, children: Vec<Node>) -> Node {
        Node::Row {
            id: id.map(ToOwned::to_owned),
            classes: vec![],
            spacing: 0,
            children,
            tooltip: None,
        }
    }

    fn listbox_classed(id: Option<&str>, classes: Vec<&str>, children: Vec<Node>) -> Node {
        Node::ListBox {
            id: id.map(ToOwned::to_owned),
            classes: classes.into_iter().map(ToOwned::to_owned).collect(),
            dense: false,
            children,
        }
    }

    fn list_of(root: &gtk::Box) -> gtk::ListBox {
        root.first_child()
            .expect("list mounted")
            .downcast::<gtk::ListBox>()
            .expect("ListBox → real gtk::ListBox")
    }

    /// The inner node widgets (unwrapped from their auto-created `GtkListBoxRow`s)
    /// of a `gtk::ListBox`, in order.
    fn list_rows(list: &gtk::ListBox) -> Vec<gtk::Widget> {
        let mut out = Vec::new();
        let mut i = 0;
        while let Some(row) = list.row_at_index(i) {
            out.push(row.child().expect("list row has a child"));
            i += 1;
        }
        out
    }

    #[gtk::test]
    fn listbox_is_a_real_listbox_ready_for_boxed_list() {
        // The crux of #333: a ListBox must materialize as a *real* `gtk::ListBox`
        // (CSS node `list`) so libadwaita's `.boxed-list` rules (`list.boxed-list`)
        // actually paint — a plain `gtk::Box` (CSS node `box`) never would.
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&listbox_classed("l".into(), vec!["boxed-list"], vec![]));

        let list = list_of(&root);
        assert_eq!(
            list.selection_mode(),
            gtk::SelectionMode::None,
            "a plugin list is selection-less"
        );
        assert!(
            list.has_css_class("boxed-list"),
            "the .boxed-list blessing class applies to the real GtkListBox"
        );
    }

    #[gtk::test]
    fn listbox_wraps_rows_and_diffs_through_the_wrapper() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&listbox(
            Some("list"),
            vec![row(Some("r0"), vec![text(None, "hi", None)])],
        ));

        let list = list_of(&root);
        // GTK auto-wraps each child in a GtkListBoxRow.
        let wrapper = list
            .row_at_index(0)
            .expect("row 0")
            .downcast::<gtk::ListBoxRow>()
            .expect("child auto-wrapped in a GtkListBoxRow");
        let inner_row = wrapper
            .child()
            .unwrap()
            .downcast::<gtk::Box>()
            .expect("Row → horizontal gtk::Box inside the wrapper");
        assert_eq!(inner_row.orientation(), gtk::Orientation::Horizontal);
        let label = inner_row
            .first_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        assert_eq!(label.text().as_str(), "hi");
        assert!(label.wraps(), "Text is a wrapping label");
    }

    #[gtk::test]
    fn listbox_row_content_updates_through_the_wrapper() {
        // A same-id Row's inner content must update in place, with BOTH the plugin
        // Row widget and its auto-created wrapper row preserved across the render.
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&listbox(
            Some("list"),
            vec![row(Some("a"), vec![lbl(Some("t"), "x")])],
        ));
        let list = list_of(&root);
        let row_before = list_rows(&list)[0].clone();
        let wrapper_before = list.row_at_index(0).unwrap();
        let label_before = row_before.first_child().unwrap();

        rec.render(&listbox(
            Some("list"),
            vec![row(Some("a"), vec![lbl(Some("t"), "y")])],
        ));
        let after = list_rows(&list);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0], row_before, "Row box reused through the wrapper");
        assert_eq!(
            list.row_at_index(0).unwrap(),
            wrapper_before,
            "the GtkListBoxRow wrapper is preserved too"
        );
        let label_after = after[0].first_child().unwrap();
        assert_eq!(label_after, label_before, "inner label reused, not rebuilt");
        assert_eq!(
            label_after
                .downcast::<gtk::Label>()
                .unwrap()
                .text()
                .as_str(),
            "y",
            "text updated in place"
        );
    }

    #[gtk::test]
    fn listbox_rows_reorder_preserves_identity() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&listbox(
            Some("list"),
            vec![
                row(Some("a"), vec![]),
                row(Some("b"), vec![]),
                row(Some("c"), vec![]),
            ],
        ));
        let list = list_of(&root);
        let before = list_rows(&list); // [a, b, c]
        assert_eq!(before.len(), 3);

        // Reorder to [c, a, b]: every row keeps its widget identity, no rebuild.
        rec.render(&listbox(
            Some("list"),
            vec![
                row(Some("c"), vec![]),
                row(Some("a"), vec![]),
                row(Some("b"), vec![]),
            ],
        ));
        let after = list_rows(&list);
        assert_eq!(after.len(), 3);
        assert_eq!(after[0], before[2], "c moved to front, same widget");
        assert_eq!(after[1], before[0], "a shifted back, same widget");
        assert_eq!(after[2], before[1], "b shifted back, same widget");
    }

    #[gtk::test]
    fn text_wrap_and_max_width_update_in_place() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&text(Some("t"), "one", Some(10)));
        let label = root
            .first_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        assert!(label.wraps());
        assert_eq!(label.max_width_chars(), 10);

        // Same id → reused; text + max_width_chars are mutable props.
        rec.render(&text(Some("t"), "two", None));
        let after = root
            .first_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        assert_eq!(after, label, "wrapping label reused, not rebuilt");
        assert_eq!(after.text().as_str(), "two");
        assert_eq!(
            after.max_width_chars(),
            -1,
            "None resets the max to GTK's -1"
        );
    }

    #[gtk::test]
    fn text_ellipsize_toggles_flow_in_place() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});

        // Start ellipsizing: single-line, End-truncation, no wrap.
        rec.render(&Node::Text {
            id: Some("dest".into()),
            text: "a very long destination name".into(),
            max_width_chars: None,
            ellipsize: true,
            classes: vec![],
            tooltip: None,
        });
        let label = root
            .first_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        assert!(!label.wraps(), "ellipsize ⇒ single line (no wrap)");
        assert_eq!(label.ellipsize(), gtk::pango::EllipsizeMode::End);

        // Same id → reused; flipping `ellipsize` off restores the wrap flow
        // in place (mutable prop), same widget identity.
        rec.render(&Node::Text {
            id: Some("dest".into()),
            text: "a very long destination name".into(),
            max_width_chars: None,
            ellipsize: false,
            classes: vec![],
            tooltip: None,
        });
        let after = root
            .first_child()
            .unwrap()
            .downcast::<gtk::Label>()
            .unwrap();
        assert_eq!(after, label, "wrapping label reused, not rebuilt");
        assert!(after.wraps(), "wrap restored");
        assert_eq!(
            after.ellipsize(),
            gtk::pango::EllipsizeMode::None,
            "ellipsize mode cleared on the flip back"
        );
    }

    #[gtk::test]
    fn spacer_expands_only_along_its_container_axis() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        // The weather-row shape: label + expanding gap + value, in a HORIZONTAL
        // box. The spacer must justify horizontally WITHOUT claiming vertical
        // expand — a cross-axis expand propagates up and stretches the card (#330).
        rec.render(&hbox(vec![
            lbl(None, "wind"),
            Node::Spacer,
            lbl(None, "12"),
        ]));
        let inner = root.first_child().unwrap();
        let kids = children(&inner);
        assert_eq!(kids.len(), 3, "label + spacer + label");
        let spacer = kids[1]
            .downcast_ref::<gtk::Box>()
            .expect("Spacer → an empty gtk::Box");
        assert!(
            spacer.hexpands(),
            "spacer expands along the row's main axis"
        );
        assert!(
            !spacer.vexpands(),
            "spacer must NOT expand on the cross axis (would stretch the row/card, #330)"
        );
        assert!(spacer.first_child().is_none(), "spacer is empty");

        // Same node in a VERTICAL box: now it must expand vertically, not
        // horizontally — the axis follows the container.
        let vbox = Node::Box {
            id: None,
            dir: Dir::Vertical,
            spacing: 0,
            scroll: false,
            classes: vec![],
            children: vec![lbl(None, "a"), Node::Spacer, lbl(None, "b")],
            tooltip: None,
        };
        rec.render(&vbox);
        let inner = root.first_child().unwrap();
        let vkids = children(&inner);
        let vspacer = vkids[1]
            .downcast_ref::<gtk::Box>()
            .expect("Spacer → an empty gtk::Box");
        assert!(
            vspacer.vexpands(),
            "spacer expands along the column's main axis"
        );
        assert!(
            !vspacer.hexpands(),
            "spacer must NOT expand on the cross axis in a column (#330)"
        );
    }

    #[gtk::test]
    fn consecutive_spacers_reuse_by_kind() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        // Two adjacent, id-less spacers (a centring pair).
        rec.render(&hbox(vec![Node::Spacer, lbl(None, "x"), Node::Spacer]));
        let before = children(&root.first_child().unwrap());
        assert_eq!(before.len(), 3);

        // Re-render the same shape: both spacers reuse by kind (positional,
        // id-less), so every widget keeps its identity.
        rec.render(&hbox(vec![Node::Spacer, lbl(None, "x"), Node::Spacer]));
        let after = children(&root.first_child().unwrap());
        assert_eq!(after.len(), 3);
        assert_eq!(after[0], before[0], "leading spacer reused");
        assert_eq!(after[2], before[2], "trailing spacer reused");
    }

    #[gtk::test]
    fn listbox_rows_keyed_diff_insert_and_remove() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&listbox(
            Some("list"),
            vec![row(Some("a"), vec![]), row(Some("b"), vec![])],
        ));
        let list = list_of(&root);
        let before = list_rows(&list);
        assert_eq!(before.len(), 2);

        // Insert "z" between a and b; a and b keep their widget identities (through
        // the GtkListBoxRow wrapping).
        rec.render(&listbox(
            Some("list"),
            vec![
                row(Some("a"), vec![]),
                row(Some("z"), vec![]),
                row(Some("b"), vec![]),
            ],
        ));
        let after = list_rows(&list);
        assert_eq!(after.len(), 3);
        assert_eq!(after[0], before[0], "row a reused in place");
        assert_eq!(after[2], before[1], "row b reused, shifted right");

        // Drop "a": b survives untouched.
        rec.render(&listbox(Some("list"), vec![row(Some("b"), vec![])]));
        let last = list_rows(&list);
        assert_eq!(last.len(), 1);
        assert_eq!(last[0], before[1], "row b is the surviving sibling");
    }

    // ── Expander (#333) ──────────────────────────────────────────────────────

    fn expander(id: &str, header: Node, expanded: bool, children: Vec<Node>) -> Node {
        Node::Expander {
            id: id.to_owned(),
            header: Box::new(header),
            children,
            expanded,
            classes: vec![],
            tooltip: None,
        }
    }

    /// The (header button, chevron image, revealer) of a mounted Expander.
    fn expander_parts(root: &gtk::Box) -> (gtk::Button, gtk::Image, gtk::Revealer) {
        let outer = root
            .first_child()
            .unwrap()
            .downcast::<gtk::Box>()
            .expect("Expander → vertical gtk::Box");
        assert_eq!(outer.orientation(), gtk::Orientation::Vertical);
        let button = outer
            .first_child()
            .unwrap()
            .downcast::<gtk::Button>()
            .expect("header is a gtk::Button");
        let header_box = button.child().unwrap().downcast::<gtk::Box>().unwrap();
        let chevron = header_box
            .last_child()
            .unwrap()
            .downcast::<gtk::Image>()
            .expect("trailing chevron image");
        let revealer = outer
            .last_child()
            .unwrap()
            .downcast::<gtk::Revealer>()
            .expect("body revealer");
        (button, chevron, revealer)
    }

    #[gtk::test]
    fn expander_builds_header_chevron_and_revealer() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&expander(
            "e",
            lbl(Some("h"), "Living Room"),
            false,
            vec![lbl(Some("d"), "Lamp")],
        ));

        let (_button, chevron, revealer) = expander_parts(&root);
        assert!(!revealer.reveals_child(), "collapsed → body hidden");
        assert_eq!(
            chevron.icon_name().unwrap().as_str(),
            "pan-end-symbolic",
            "collapsed chevron points at the trailing edge"
        );
    }

    #[gtk::test]
    fn expander_expanded_prop_reveals_and_swaps_chevron_in_place() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&expander("e", lbl(Some("h"), "Room"), false, vec![]));
        let (button_before, chevron, revealer) = expander_parts(&root);

        // Same id, expanded now true: reveal + chevron swap, no rebuild.
        rec.render(&expander("e", lbl(Some("h"), "Room"), true, vec![]));
        let (button_after, chevron_after, revealer_after) = expander_parts(&root);
        assert_eq!(button_after, button_before, "header button reused");
        assert_eq!(chevron_after, chevron, "chevron reused");
        assert_eq!(revealer_after, revealer, "revealer reused");
        assert!(
            revealer.reveals_child(),
            "expanded → body revealed in place"
        );
        assert_eq!(
            chevron.icon_name().unwrap().as_str(),
            "pan-down-symbolic",
            "expanded chevron points down"
        );
    }

    #[gtk::test]
    fn expander_header_updates_in_place() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&expander("e", lbl(Some("h"), "Old"), false, vec![]));
        let (button, _chevron, _rev) = expander_parts(&root);
        let header_box = button.child().unwrap().downcast::<gtk::Box>().unwrap();
        let label_before = header_box.first_child().unwrap();

        rec.render(&expander("e", lbl(Some("h"), "New"), false, vec![]));
        let label_after = button
            .child()
            .unwrap()
            .downcast::<gtk::Box>()
            .unwrap()
            .first_child()
            .unwrap();
        assert_eq!(label_after, label_before, "same-id header label reused");
        assert_eq!(
            label_after
                .downcast::<gtk::Label>()
                .unwrap()
                .text()
                .as_str(),
            "New",
            "header text updated in place"
        );
    }

    #[gtk::test]
    fn expander_body_children_diff_in_the_revealer() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&expander(
            "e",
            lbl(Some("h"), "Room"),
            true,
            vec![lbl(Some("a"), "a")],
        ));
        let (_b, _c, revealer) = expander_parts(&root);
        let body = revealer.child().unwrap().downcast::<gtk::Box>().unwrap();
        assert_eq!(children(&body).len(), 1);

        rec.render(&expander(
            "e",
            lbl(Some("h"), "Room"),
            true,
            vec![lbl(Some("a"), "a"), lbl(Some("b"), "b")],
        ));
        assert_eq!(children(&body).len(), 2, "body child appended in place");
    }

    #[gtk::test]
    fn expander_header_click_fires_click_once_even_after_reuse() {
        let root = root();
        let events: Rc<RefCell<Vec<(String, EventKind)>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        let mut rec = Reconciler::new(&root, move |id, kind| sink.borrow_mut().push((id, kind)));

        rec.render(&expander("room", lbl(None, "h"), false, vec![]));
        // Re-render (reuse): the click handler must not be re-connected.
        rec.render(&expander("room", lbl(None, "h"), true, vec![]));

        let (button, _c, _r) = expander_parts(&root);
        button.emit_clicked();

        let recorded = events.borrow();
        assert_eq!(recorded.len(), 1, "exactly one Click, no double-fire");
        assert_eq!(recorded[0].0, "room", "addressed by the expander id");
        assert_eq!(recorded[0].1, EventKind::Click);
    }

    // ── Slider (#315) ────────────────────────────────────────────────────────

    fn slider(id: &str, min: f64, max: f64, value: f64, step: f64) -> Node {
        Node::Slider {
            id: id.to_owned(),
            min,
            max,
            value,
            step,
            enabled: true,
            classes: vec![],
        }
    }

    fn scale_of(root: &gtk::Box) -> gtk::Scale {
        root.first_child()
            .expect("slider mounted")
            .downcast::<gtk::Scale>()
            .expect("Slider → gtk::Scale")
    }

    #[gtk::test]
    fn slider_builds_with_range_value_step() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&slider("b", 0.0, 100.0, 40.0, 5.0));

        let scale = scale_of(&root);
        let adj = scale.adjustment();
        assert!(
            (adj.lower() - 0.0).abs() < f64::EPSILON,
            "min → adjustment lower"
        );
        assert!(
            (adj.upper() - 100.0).abs() < f64::EPSILON,
            "max → adjustment upper"
        );
        assert!(
            (adj.step_increment() - 5.0).abs() < f64::EPSILON,
            "step → step increment"
        );
        assert!((scale.value() - 40.0).abs() < f64::EPSILON, "value set");
        assert_eq!(
            scale.orientation(),
            gtk::Orientation::Horizontal,
            "slider is horizontal"
        );
        assert!(!scale.draws_value(), "no value label (styled via classes)");
    }

    #[gtk::test]
    fn slider_update_moves_value_when_not_dragging() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&slider("b", 0.0, 1.0, 0.2, 0.05));
        let before = scale_of(&root);

        // Same id, new value: with no user interaction the programmatic move
        // applies and the widget is reused in place (mutable prop).
        rec.render(&slider("b", 0.0, 1.0, 0.8, 0.05));
        let after = scale_of(&root);
        assert_eq!(before, after, "same-id Slider reused, not rebuilt");
        assert!(
            (after.value() - 0.8).abs() < f64::EPSILON,
            "value moved in place"
        );
    }

    #[gtk::test]
    fn slider_update_reconciles_range_in_place() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&slider("vol", 0.0, 1.0, 0.5, 0.05));
        let before = scale_of(&root);

        // A same-id re-render widening the range + step reconciles the live
        // adjustment without rebuilding the widget.
        rec.render(&slider("vol", 0.0, 10.0, 7.0, 1.0));
        let after = scale_of(&root);
        assert_eq!(
            before, after,
            "widget identity preserved across a range change"
        );
        let adj = after.adjustment();
        assert!((adj.upper() - 10.0).abs() < f64::EPSILON, "upper widened");
        assert!(
            (adj.step_increment() - 1.0).abs() < f64::EPSILON,
            "step updated"
        );
        assert!((after.value() - 7.0).abs() < f64::EPSILON, "value updated");
    }

    #[gtk::test]
    fn slider_enabled_toggles_sensitivity_in_place() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        // Enabled by default (the `slider` helper) → sensitive.
        rec.render(&slider("b", 0.0, 100.0, 40.0, 5.0));
        let before = scale_of(&root);
        assert!(before.is_sensitive(), "enabled slider is sensitive");

        // A same-id re-render flipping `enabled: false` greys it in place —
        // reused widget, now insensitive (the vibectl off-light case).
        rec.render(&Node::Slider {
            id: "b".into(),
            min: 0.0,
            max: 100.0,
            value: 40.0,
            step: 5.0,
            enabled: false,
            classes: vec![],
        });
        let after = scale_of(&root);
        assert_eq!(before, after, "same-id Slider reused, not rebuilt");
        assert!(!after.is_sensitive(), "enabled:false → insensitive");

        // …and back to interactive.
        rec.render(&slider("b", 0.0, 100.0, 40.0, 5.0));
        assert!(scale_of(&root).is_sensitive(), "flips back to sensitive");
    }

    #[gtk::test]
    fn slider_kind_change_recreates_widget() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&slider("x", 0.0, 1.0, 0.3, 0.1));
        let before = root.first_child().unwrap();
        // Same id "x" but Slider → Button: not reuse-compatible.
        rec.render(&btn("x", "hi"));
        let after = root.first_child().unwrap();
        assert_ne!(
            before, after,
            "Slider→Button under same id is a fresh widget"
        );
        assert!(after.downcast::<gtk::Button>().is_ok());
    }

    #[gtk::test]
    fn slider_user_change_emits_value_changed() {
        let root = root();
        let events: Rc<RefCell<Vec<(String, EventKind)>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = events.clone();
        let mut rec = Reconciler::new(&root, move |id, kind| sink.borrow_mut().push((id, kind)));
        rec.render(&slider("b", 0.0, 1.0, 0.0, 0.05));

        // Drive a user move via the `change-value` signal (what a drag emits).
        // The first move hits the throttle's leading edge → emits synchronously.
        let scale = scale_of(&root);
        let _: bool = scale.emit_by_name("change-value", &[&gtk::ScrollType::Jump, &0.7f64]);

        let recorded = events.borrow();
        assert_eq!(recorded.len(), 1, "one leading-edge ValueChanged");
        assert_eq!(recorded[0].0, "b", "addressed by the slider id");
        match &recorded[0].1 {
            EventKind::ValueChanged { value } => {
                assert!((value - 0.7).abs() < 1e-9, "carries the moved-to value");
            }
            other => panic!("expected ValueChanged, got {other:?}"),
        }
    }

    #[gtk::test]
    fn slider_render_suppressed_during_active_drag() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&slider("b", 0.0, 1.0, 0.1, 0.05));
        let scale = scale_of(&root);

        // Simulate the user grabbing and dragging to 0.7 (records last_user = now).
        let _: bool = scale.emit_by_name("change-value", &[&gtk::ScrollType::Jump, &0.7f64]);
        assert!(
            (scale.value() - 0.7).abs() < f64::EPSILON,
            "drag moved the thumb"
        );

        // A plugin re-render echoes a stale 0.1 back mid-drag: the programmatic
        // set_value must be suppressed so it can't rubber-band the grab.
        rec.render(&slider("b", 0.0, 1.0, 0.1, 0.05));
        assert!(
            (scale.value() - 0.7).abs() < f64::EPSILON,
            "echoed value suppressed during the active drag; thumb stays where the user put it",
        );
    }

    /// **`build_node` and `update_in_place` are the only two sites that move a
    /// `Node::GlSurface`'s props into the widget, and neither had coverage.**
    ///
    /// Deleting `surface.set_state(…)` from *either* arm left the whole
    /// `--features system-tests` suite green: the three reconciler tests that
    /// name `GlSurface` all go through `node_kind` / `node_id` / `node_classes`,
    /// the shallow accessors, and never look at what reached the widget. A GL
    /// chip mounted with no program, no uniforms and a 0×0 natural size — or
    /// one that never receives a new state, never resizes and never re-renders
    /// — shipped clean.
    ///
    /// This needs **no GL context**: `set_state` only writes the natural size
    /// and the dedup cell, and only `render` calls GL, so a surface reconciled
    /// into an unmapped `gtk::Box` never realizes. The size `measure()` reports
    /// is a hermetic witness that the props arrived.
    ///
    /// **Falsified** on both arms — replace either `surface.set_state(…)` with
    /// `let _ = (program, width, height, state);` and this goes red, on the
    /// build assertion and the update assertion respectively.
    #[gtk::test]
    fn a_gl_surface_node_applies_its_props_on_build_and_on_update() {
        fn gl(width: u32, height: u32) -> Node {
            Node::GlSurface {
                id: Some("scope".to_owned()),
                width,
                height,
                program: crate::gl_surface::GlProgram("preem.scope"),
                state: Arc::new(crate::gl_surface::GlUniforms {
                    grid: (width, height),
                    ..Default::default()
                }),
                classes: vec![],
            }
        }
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});

        // `build_node`: the node's natural size reaches the widget.
        rec.render(&hbox(vec![gl(288, 96)]));
        let first = root
            .first_child()
            .expect("the box mounted")
            .first_child()
            .expect("the surface mounted");
        let surface = first
            .downcast_ref::<crate::gl_surface::GlSurface>()
            .expect("a GlSurface, not some other widget");
        assert_eq!(
            first.measure(gtk::Orientation::Horizontal, -1).1,
            288,
            "build_node moved the node's width into the widget",
        );
        assert_eq!(
            first.measure(gtk::Orientation::Vertical, 288).1,
            96,
            "…and its height, aspect-locked for the width offered",
        );
        assert!(
            !surface.has_error(),
            "an unmapped surface never realized, so it has no context error",
        );

        // `update_in_place`: a same-id re-render keeps the widget and moves the
        // new props, rather than leaving the surface on its first frame for ever.
        rec.render(&hbox(vec![gl(144, 48)]));
        let same = root
            .first_child()
            .expect("the box survived")
            .first_child()
            .expect("the surface survived");
        assert_eq!(same, first, "the widget is reused, not rebuilt");
        assert_eq!(
            same.measure(gtk::Orientation::Horizontal, -1).1,
            144,
            "update_in_place moved the new width in",
        );
        assert_eq!(
            same.measure(gtk::Orientation::Vertical, 144).1,
            48,
            "…and the new height",
        );
    }

    /// The `Node::Shader` half of the same gap #954 found for `GlSurface`:
    /// `build_node` and `update_in_place` are the only two sites that move a
    /// shader node's props into the widget, and the four diff tests above go
    /// through the shallow accessors and never look at what reached it.
    ///
    /// Without this, a shader chip mounted with a 0×0 natural size — or one
    /// that never receives a new state, so it draws its first frame for ever —
    /// ships clean.
    ///
    /// **No GL context needed**: `set_state` writes the natural size and the
    /// dedup cell, and only `render` touches GL, so a surface reconciled into
    /// an unmapped `gtk::Box` never realizes. The size `measure()` reports is
    /// the hermetic witness that the props arrived.
    ///
    /// **Falsified** on both arms — replace either `surface.set_state(…)` with
    /// `let _ = (width, height, state);` and this goes red, on the build
    /// assertion and the update assertion respectively.
    #[gtk::test]
    fn a_shader_node_applies_its_props_on_build_and_on_update() {
        fn shader(width: u32, height: u32) -> Node {
            Node::Shader {
                id: Some("spectrum".to_owned()),
                width,
                height,
                state: Arc::new(crate::shader_surface::ShaderState {
                    fragment: Arc::from("void main() { fragColor = u_fg; }"),
                    data: Arc::from(&[0u8, 255][..]),
                    format: crate::shader_surface::ShaderFormat::R8,
                    data_size: (2, 1),
                    scale: 1,
                    values: vec![],
                }),
                classes: vec![],
                tooltip: None,
            }
        }
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});

        // `build_node`: the node's natural size reaches the widget — and the
        // widget really is a `ShaderSurface`, not the `GlSurface` beside it.
        rec.render(&hbox(vec![shader(288, 96)]));
        let first = root
            .first_child()
            .expect("the box mounted")
            .first_child()
            .expect("the surface mounted");
        let surface = first
            .downcast_ref::<crate::shader_surface::ShaderSurface>()
            .expect("a ShaderSurface, not some other widget");
        assert_eq!(
            first.measure(gtk::Orientation::Horizontal, -1).1,
            288,
            "build_node moved the node's width into the widget",
        );
        assert_eq!(
            first.measure(gtk::Orientation::Vertical, 288).1,
            96,
            "…and its height, aspect-locked for the width offered",
        );
        assert!(
            !surface.has_error(),
            "an unmapped surface never realized, so it has no context error",
        );

        // `update_in_place`: a same-id re-render keeps the widget — which is
        // the compiled program surviving — and moves the new props.
        rec.render(&hbox(vec![shader(144, 48)]));
        let same = root
            .first_child()
            .expect("the box survived")
            .first_child()
            .expect("the surface survived");
        assert_eq!(same, first, "the widget is reused, not rebuilt");
        assert_eq!(
            same.measure(gtk::Orientation::Horizontal, -1).1,
            144,
            "update_in_place moved the new width in",
        );
        assert_eq!(
            same.measure(gtk::Orientation::Vertical, 144).1,
            48,
            "…and the new height",
        );
    }

    // ── Tooltips (#957) ─────────────────────────────────────────────────────
    //
    // The claude-bridge chip's `sub 18/0` was unreadable to the person running
    // it, and the chip is deliberately panel-less, so the vocabulary grew an
    // optional `tooltip` instead. Three properties matter and each is falsifiable
    // by deleting exactly one line of the reconciler:
    //
    //   build   → `apply_tooltip(&widget, node_tooltip(node))` in `build_node`
    //   change  → the `reconcile_tooltip(…)` call in `update_in_place`
    //   clear   → the same call, whose `None` arm is the one a "set it when
    //             `Some`" shortcut would silently skip
    //
    // A reused widget is the interesting case throughout: the tooltip is not
    // part of a node's identity, so a change must move onto the *same* widget.

    /// A label carrying a tooltip.
    fn lbl_tip(id: Option<&str>, text: &str, tooltip: Option<&str>) -> Node {
        Node::Label {
            id: id.map(ToOwned::to_owned),
            text: text.to_owned(),
            classes: vec![],
            tooltip: tooltip.map(ToOwned::to_owned),
        }
    }

    /// The realized widget for the single child of the mounted root box.
    fn only_child(root: &gtk::Box) -> gtk::Widget {
        root.first_child()
            .expect("the box mounted")
            .first_child()
            .expect("its child mounted")
    }

    #[gtk::test]
    fn a_tooltip_is_applied_on_build() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&hbox(vec![lbl_tip(Some("a"), "x", Some("what x means"))]));

        let label = only_child(&root);
        assert_eq!(label.tooltip_text().as_deref(), Some("what x means"));
        assert!(label.has_tooltip(), "GTK arms the hover for it");
    }

    /// Every variant that declares the field actually gets it — including the
    /// two that matter for a bar chip: the root `Box` (one hover for the whole
    /// pill) and an `Icon` (a glyph with no words of its own).
    #[gtk::test]
    fn every_tooltip_carrying_variant_applies_it_on_build() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&Node::Box {
            id: Some("chip".to_owned()),
            dir: Dir::Horizontal,
            spacing: 0,
            scroll: false,
            classes: vec![],
            children: vec![Node::Icon {
                id: Some("glyph".to_owned()),
                name: "emblem-ok-symbolic".to_owned(),
                classes: vec![],
                tooltip: Some("the icon's own".to_owned()),
            }],
            tooltip: Some("the whole pill".to_owned()),
        });

        let boxw = root.first_child().expect("the box mounted");
        assert_eq!(boxw.tooltip_text().as_deref(), Some("the whole pill"));
        let icon = boxw.first_child().expect("the icon mounted");
        assert!(icon.is::<gtk::Image>(), "an Icon is a gtk::Image");
        assert_eq!(icon.tooltip_text().as_deref(), Some("the icon's own"));
    }

    /// **#968 review M3.** The shader surface is the **fourth** tooltip-carrying
    /// variant, and it is honoured on build, on change, and on clear — the three
    /// properties #957 pinned for the other three.
    ///
    /// Before this the wire carried a `tooltip`, the SDK shipped a public
    /// `Shader::tooltip(…)` builder method, a golden fixture pinned its bytes,
    /// and nothing anywhere read it: a plugin author following the field's own
    /// doc got silence. A shader chip is a picture with nowhere else to say what
    /// it is, which is exactly why the other three have one.
    ///
    /// **Falsified** by dropping the `Node::Shader` arm from `node_tooltip`: all
    /// three phases go red, because the central `apply_tooltip` /
    /// `reconcile_tooltip` plumbing reads through it.
    #[gtk::test]
    fn a_shader_tooltip_is_applied_changed_and_cleared() {
        fn shader(tooltip: Option<&str>) -> Node {
            Node::Shader {
                id: Some("spectrum".to_owned()),
                width: 32,
                height: 32,
                state: Arc::new(crate::shader_surface::ShaderState {
                    fragment: Arc::from("void main() { fragColor = u_fg; }"),
                    data: Arc::from(&[0u8, 255][..]),
                    format: crate::shader_surface::ShaderFormat::R8,
                    data_size: (2, 1),
                    scale: 1,
                    values: vec![],
                }),
                classes: vec![],
                tooltip: tooltip.map(ToOwned::to_owned),
            }
        }
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});

        // build
        rec.render(&hbox(vec![shader(Some("audio spectrum"))]));
        let surface = only_child(&root);
        assert!(
            surface.is::<crate::shader_surface::ShaderSurface>(),
            "a Shader is a ShaderSurface",
        );
        assert_eq!(surface.tooltip_text().as_deref(), Some("audio spectrum"));
        assert!(surface.has_tooltip(), "GTK arms the hover for it");

        // change, on the *same* widget — a tooltip is not part of a node's
        // identity, so this must retitle rather than rebuild.
        rec.render(&hbox(vec![shader(Some("cpu spectrum"))]));
        let same = only_child(&root);
        assert_eq!(same, surface, "the surface is reused, not rebuilt");
        assert_eq!(same.tooltip_text().as_deref(), Some("cpu spectrum"));

        // clear — the arm a "set it when `Some`" shortcut would silently skip,
        // leaving the last string stuck on the widget for ever.
        rec.render(&hbox(vec![shader(None)]));
        let cleared = only_child(&root);
        assert_eq!(cleared, surface, "still the same surface");
        assert_eq!(cleared.tooltip_text(), None, "dropping the field clears it");
        assert!(!cleared.has_tooltip(), "…and disarms the hover");
    }

    /// A same-id re-render with a *different* string retitles the widget in
    /// place.
    ///
    /// The `assert_eq!(after, before)` here guards the **`plan_diff` half** of
    /// reuse only — this label is a `Box` child, so it is keyed by
    /// [`child_key`], and putting the tooltip into [`ChildKey`] doesn't even
    /// compile (the `plan_diff` unit tests construct `ChildKey` literals). The
    /// other half, [`reusable`], is a *different* gate on a *different* set of
    /// nodes, and it is the one the claude-bridge chip actually takes — see
    /// [`a_changed_root_tooltip_reuses_the_root_widget`], which is what pins it.
    #[gtk::test]
    fn a_changed_tooltip_is_applied_to_the_reused_widget() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&hbox(vec![lbl_tip(Some("a"), "x", Some("18 served"))]));
        let before = only_child(&root);
        assert_eq!(before.tooltip_text().as_deref(), Some("18 served"));

        rec.render(&hbox(vec![lbl_tip(Some("a"), "x", Some("19 served"))]));
        let after = only_child(&root);

        assert_eq!(after, before, "the widget is reused, not rebuilt");
        assert_eq!(after.tooltip_text().as_deref(), Some("19 served"));
    }

    /// …and a re-render that drops the tooltip **clears** it. This is the
    /// property a "only set it when `Some`" reconciler gets wrong: the stale
    /// hover text stays armed on the widget for ever, explaining a state the
    /// plugin has left.
    #[gtk::test]
    fn a_tooltip_dropped_to_none_is_cleared() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&hbox(vec![lbl_tip(Some("a"), "x", Some("stale"))]));
        let before = only_child(&root);
        assert_eq!(before.tooltip_text().as_deref(), Some("stale"));

        rec.render(&hbox(vec![lbl_tip(Some("a"), "x", None)]));
        let after = only_child(&root);

        assert_eq!(after, before, "the widget is reused, not rebuilt");
        assert_eq!(after.tooltip_text(), None, "the stale hover is gone");
        assert!(!after.has_tooltip(), "…and GTK no longer arms one");
    }

    /// **The invariant the shipping consumer actually depends on.** [`reusable`]
    /// — not [`child_key`]/`plan_diff` — is what decides whether the **root**
    /// node (and a `Button`/`Revealer` child, or an `Expander` header) is
    /// updated or torn down, and the root is the only node the claude-bridge
    /// chip hangs a tooltip on, with a string that changes on every count tick.
    ///
    /// Every other tooltip test in this module puts its node *inside* an
    /// `hbox`, so all of them exercise the `plan_diff` gate and none can see
    /// [`reusable`] at all — adding `tooltip` to that gate leaves the whole
    /// suite green while the chip rebuilds its entire subtree every 5 s. This
    /// test is what closes that.
    #[gtk::test]
    fn a_changed_root_tooltip_reuses_the_root_widget() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        let chip = |tip: &str| Node::Box {
            id: Some("chip".to_owned()),
            dir: Dir::Horizontal,
            spacing: 0,
            scroll: false,
            classes: vec![],
            children: vec![lbl(None, "sub")],
            tooltip: Some(tip.to_owned()),
        };

        rec.render(&chip("Claude bridge · subscription · 18 served, 0 failed"));
        let before = root.first_child().expect("the chip mounted");
        rec.render(&chip("Claude bridge · subscription · 19 served, 0 failed"));
        let after = root.first_child().expect("the chip is still mounted");

        assert_eq!(after, before, "the ROOT widget is reused, not rebuilt");
        assert_eq!(
            after.tooltip_text().as_deref(),
            Some("Claude bridge · subscription · 19 served, 0 failed")
        );
    }

    /// A node that never carried a tooltip never grows one — the central
    /// `apply_tooltip` must not, say, stringify the node into the hover.
    #[gtk::test]
    fn a_node_without_a_tooltip_has_none() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&hbox(vec![lbl(Some("a"), "x")]));
        assert_eq!(only_child(&root).tooltip_text(), None);
        assert_eq!(root.first_child().unwrap().tooltip_text(), None);
    }

    // ── Tooltips, part two: Row / Text / Expander (#961) ─────────────────────
    //
    // #957 stopped at the three variants a *chip* is made of. A list card is
    // made of the other three, and the agents plugin (#963) was spelling its
    // rows as horizontal `Box`es purely to get hover text. Two things here are
    // not just "a fourth arm in `node_tooltip`":
    //
    //   `Expander` → the tooltip is armed on the **header button**, not on the
    //                node's own widget (the outer box, which also holds the
    //                revealed body) — `tooltip_target`.
    //   `Text`     → an ellipsizing label with no explicit tooltip gets its own
    //                `text` as the hover, which is the whole reason #961 exists.

    /// A row carrying a tooltip.
    fn row_tip(id: Option<&str>, children: Vec<Node>, tooltip: Option<&str>) -> Node {
        Node::Row {
            id: id.map(ToOwned::to_owned),
            classes: vec![],
            spacing: 0,
            children,
            tooltip: tooltip.map(ToOwned::to_owned),
        }
    }

    /// A `Text` node with both flow and hover under the test's control.
    fn text_tip(id: Option<&str>, text: &str, ellipsize: bool, tooltip: Option<&str>) -> Node {
        Node::Text {
            id: id.map(ToOwned::to_owned),
            text: text.to_owned(),
            max_width_chars: None,
            ellipsize,
            classes: vec![],
            tooltip: tooltip.map(ToOwned::to_owned),
        }
    }

    /// The #958 triple (set / change / clear) for a `Row`, on a **reused**
    /// widget throughout.
    ///
    /// **Falsified** by dropping the `Node::Row` arm from [`node_tooltip`]: all
    /// three phases go red, because the central `apply_tooltip` /
    /// `reconcile_tooltip` plumbing reads the effective string through it.
    #[gtk::test]
    fn a_row_tooltip_is_applied_changed_and_cleared() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});

        rec.render(&hbox(vec![row_tip(
            Some("argus"),
            vec![lbl(None, "argus")],
            Some("argus · running"),
        )]));
        let before = only_child(&root);
        assert_eq!(before.tooltip_text().as_deref(), Some("argus · running"));
        assert!(before.has_tooltip(), "GTK arms the hover for it");

        rec.render(&hbox(vec![row_tip(
            Some("argus"),
            vec![lbl(None, "argus")],
            Some("argus · failed"),
        )]));
        let changed = only_child(&root);
        assert_eq!(changed, before, "the widget is reused, not rebuilt");
        assert_eq!(changed.tooltip_text().as_deref(), Some("argus · failed"));

        rec.render(&hbox(vec![row_tip(
            Some("argus"),
            vec![lbl(None, "argus")],
            None,
        )]));
        let cleared = only_child(&root);
        assert_eq!(cleared, before, "still the same widget");
        assert_eq!(cleared.tooltip_text(), None, "dropping the field clears it");
        assert!(!cleared.has_tooltip(), "…and disarms the hover");
    }

    /// The [`reusable`] half for a `Row`, mirroring
    /// [`a_changed_root_tooltip_reuses_the_root_widget`]: a row is what a list
    /// card re-renders per tick, and a tooltip that changed with the row's
    /// status must not tear the row (and its children) down. Every other `Row`
    /// test here nests inside an `hbox` and so exercises `plan_diff` instead.
    #[gtk::test]
    fn a_changed_root_row_tooltip_reuses_the_root_widget() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});

        rec.render(&row_tip(
            Some("argus"),
            vec![lbl(None, "argus")],
            Some("3 running"),
        ));
        let before = root.first_child().expect("the row mounted");
        rec.render(&row_tip(
            Some("argus"),
            vec![lbl(None, "argus")],
            Some("4 running"),
        ));
        let after = root.first_child().expect("the row is still mounted");

        assert_eq!(after, before, "the ROOT widget is reused, not rebuilt");
        assert_eq!(after.tooltip_text().as_deref(), Some("4 running"));
    }

    /// An `Expander`'s tooltip lands on the **header button**, and the outer box
    /// (which also holds the revealed body) never gets one — the whole reason
    /// [`tooltip_target`] exists.
    ///
    /// **Falsified** by making `tooltip_target` return `widget.clone()`
    /// unconditionally: the header assertion goes red (`None`) and so does the
    /// "the body never inherits it" one (the outer box grows the hover).
    #[gtk::test]
    fn an_expander_arms_its_tooltip_on_the_header_not_the_body() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        let node = |tip: Option<&str>| Node::Expander {
            id: "hive".to_owned(),
            header: Box::new(lbl(Some("h"), "hive")),
            children: vec![lbl(Some("d"), "argus")],
            expanded: true,
            classes: vec![],
            tooltip: tip.map(ToOwned::to_owned),
        };

        rec.render(&node(Some("4 agents, 1 failed")));
        let outer = root.first_child().expect("the expander mounted");
        let (button, _chevron, _revealer) = expander_parts(&root);
        assert_eq!(
            button.tooltip_text().as_deref(),
            Some("4 agents, 1 failed"),
            "the header button is what the pointer hovers"
        );
        assert_eq!(
            outer.tooltip_text(),
            None,
            "the outer box holds the revealed body — it must never inherit the header's legend"
        );
        assert!(!outer.has_tooltip());

        // change, on the *same* button: a tooltip is not part of a node's identity.
        rec.render(&node(Some("4 agents, 2 failed")));
        let (same, _, _) = expander_parts(&root);
        assert_eq!(same, button, "the header button is reused, not rebuilt");
        assert_eq!(same.tooltip_text().as_deref(), Some("4 agents, 2 failed"));

        // clear.
        rec.render(&node(None));
        let (cleared, _, _) = expander_parts(&root);
        assert_eq!(cleared, button, "still the same button");
        assert_eq!(cleared.tooltip_text(), None, "dropping the field clears it");
        assert!(!cleared.has_tooltip());
        assert_eq!(
            root.first_child().unwrap().tooltip_text(),
            None,
            "and the outer box still has none"
        );
    }

    /// The #961 default: an ellipsizing `Text` with no explicit tooltip hovers
    /// its own full text — the string the `…` swallowed. And because the
    /// derivation lives in [`node_tooltip`], the snapshot in [`NodeDesc`] holds
    /// the *effective* string, so a changed `text` moves the hover with it.
    ///
    /// **Falsified** by returning `tooltip.as_deref()` alone from the
    /// `Node::Text` arm: both assertions go red with `None`.
    #[gtk::test]
    fn an_ellipsized_text_tooltips_itself() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        let long = "rebasing the agents plugin onto the viewport";

        rec.render(&hbox(vec![text_tip(Some("what"), long, true, None)]));
        let label = only_child(&root);
        assert_eq!(
            label.tooltip_text().as_deref(),
            Some(long),
            "the full text is the hover for a truncating label"
        );
        assert!(label.has_tooltip());

        rec.render(&hbox(vec![text_tip(
            Some("what"),
            "waiting on the gate",
            true,
            None,
        )]));
        let same = only_child(&root);
        assert_eq!(same, label, "the widget is reused, not rebuilt");
        assert_eq!(
            same.tooltip_text().as_deref(),
            Some("waiting on the gate"),
            "a new text carries a new derived hover",
        );
    }

    /// An explicit tooltip always wins over the derived one, in both
    /// directions: setting one replaces the text-derived hover, and dropping it
    /// falls back to the text rather than to nothing.
    #[gtk::test]
    fn an_explicit_text_tooltip_wins_over_the_derived_one() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        let long = "rebasing the agents plugin onto the viewport";

        rec.render(&hbox(vec![text_tip(
            Some("what"),
            long,
            true,
            Some("started 4 minutes ago"),
        )]));
        let label = only_child(&root);
        assert_eq!(
            label.tooltip_text().as_deref(),
            Some("started 4 minutes ago"),
            "an explicit tooltip beats the text",
        );

        rec.render(&hbox(vec![text_tip(Some("what"), long, true, None)]));
        assert_eq!(
            only_child(&root).tooltip_text().as_deref(),
            Some(long),
            "dropping it falls back to the derived hover, not to none",
        );
    }

    /// A `Text` that does not ellipsize gets no tooltip unless it asks for one
    /// — the default is scoped to the truncating flow mode, and flipping
    /// `ellipsize` off **clears** the derived hover rather than leaving it
    /// stuck. (`ellipsize` is itself a mutable prop, so the label is reused
    /// across the flip.)
    ///
    /// **Falsified** by dropping the `ellipsize` guard (`.or(Some(text))`): the
    /// first and last assertions go red with the label's own text.
    #[gtk::test]
    fn a_text_that_does_not_ellipsize_has_no_derived_tooltip() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        let long = "a wrapping paragraph that needs no hover to be readable";

        rec.render(&hbox(vec![text_tip(Some("p"), long, false, None)]));
        let label = only_child(&root);
        assert_eq!(
            label.tooltip_text(),
            None,
            "a wrapping label shows all of itself already"
        );
        assert!(!label.has_tooltip(), "…so GTK arms no hover");

        rec.render(&hbox(vec![text_tip(Some("p"), long, true, None)]));
        let same = only_child(&root);
        assert_eq!(same, label, "the flow flip reuses the label");
        assert_eq!(same.tooltip_text().as_deref(), Some(long));

        rec.render(&hbox(vec![text_tip(Some("p"), long, false, None)]));
        let back = only_child(&root);
        assert_eq!(back, label, "still the same label");
        assert_eq!(
            back.tooltip_text(),
            None,
            "flipping ellipsize off takes the derived hover away again"
        );
        assert!(!back.has_tooltip());
    }

    /// The header **button** is the widget an `Expander`'s tooltip is armed on
    /// ([`tooltip_target`]), and [`ExpanderState::header_button`] is the only
    /// handle to it — nothing else in [`update_in_place`]'s `Expander` arm reads
    /// that field, so a change to the header path that re-creates or re-parents
    /// the button would leave the tooltip on an orphan with every other test
    /// green. Every other `Expander` tooltip test keeps one header node for the
    /// whole render sequence, so none of them walks the **rebuild** branch at
    /// all; this is the render that does (the header node's *kind* changes, so
    /// [`reusable`] is false) while a tooltip is live. #963's agents card is
    /// exactly that shape — a title `Label` becoming a warning `Icon`.
    ///
    /// The code is already correct; this is the pin, not a fix (#971 review,
    /// MEDIUM-1 — the same shape as #958's).
    ///
    /// **Falsified** by adding `es.header_button = gtk::Button::new();` to the
    /// rebuild branch: this goes red on the second assertion, and it is the
    /// only test in the suite that does.
    #[gtk::test]
    fn an_expander_tooltip_survives_a_header_rebuild() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        let node = |header: Node, tip: &str| Node::Expander {
            id: "hive".to_owned(),
            header: Box::new(header),
            children: vec![lbl(Some("d"), "argus")],
            expanded: true,
            classes: vec![],
            tooltip: Some(tip.to_owned()),
        };

        rec.render(&node(lbl(Some("h"), "hive"), "4 agents, 1 failed"));
        let (button, _, _) = expander_parts(&root);
        assert_eq!(button.tooltip_text().as_deref(), Some("4 agents, 1 failed"));

        // Label → Icon: `reusable` is false, so the header child is torn down
        // and a fresh one is prepended into the same header box.
        rec.render(&node(
            Node::Icon {
                id: Some("h".to_owned()),
                name: "dialog-warning-symbolic".to_owned(),
                classes: vec![],
                tooltip: None,
            },
            "4 agents, 2 failed",
        ));
        let (after, _, _) = expander_parts(&root);
        assert_eq!(after, button, "the header BUTTON itself is never rebuilt");
        assert_eq!(
            after.tooltip_text().as_deref(),
            Some("4 agents, 2 failed"),
            "a header rebuild must not move the tooltip off the mounted button",
        );
    }

    /// A **blank** tooltip arms nothing — neither a derived one nor an explicit
    /// one (#971 review, LOW-1).
    ///
    /// GTK normalises `""` to no tooltip by itself, so the empty case was never
    /// broken and is asserted here to record that; `"   "` it does **not**
    /// normalise, and before the filter it set `has-tooltip` and popped an empty
    /// tooltip window on hover. The derived string is the case that matters —
    /// it is the one place a plugin grows a tooltip it never wrote — but the
    /// filter sits at [`node_tooltip`] and so covers all seven variants, which
    /// the `Label` half pins.
    ///
    /// **Falsified** by dropping the `.filter(…)` from [`node_tooltip`]: the two
    /// whitespace assertions go red (`has_tooltip` true, `tooltip_text`
    /// `Some("   ")`); the empty ones stay green, which is the measurement that
    /// says GTK — not this filter — handles `""`.
    #[gtk::test]
    fn a_blank_tooltip_arms_nothing() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});

        for text in ["", "   ", "\t\n "] {
            rec.render(&hbox(vec![text_tip(Some("t"), text, true, None)]));
            let label = only_child(&root);
            assert_eq!(
                label.tooltip_text(),
                None,
                "a blank derived hover is worse than none ({text:?})"
            );
            assert!(
                !label.has_tooltip(),
                "…and GTK must arm no hover ({text:?})"
            );
        }

        // The same guard covers an *explicit* tooltip, on any variant.
        rec.render(&hbox(vec![lbl_tip(Some("a"), "x", Some("  "))]));
        let label = only_child(&root);
        assert_eq!(label.tooltip_text(), None, "an explicit blank is blank too");
        assert!(!label.has_tooltip());

        // …and a real string still works after all that, so the filter is not
        // simply swallowing everything.
        rec.render(&hbox(vec![lbl_tip(Some("a"), "x", Some("what x means"))]));
        assert_eq!(
            only_child(&root).tooltip_text().as_deref(),
            Some("what x means")
        );
    }

    // ── List-card layout (#966) ──────────────────────────────────────────────

    /// Install libadwaita's stylesheet **and** the shipped `hytte-ui` sheet, at
    /// the same provider priority production uses.
    ///
    /// Both halves matter and neither is optional: without `adw::init` there is
    /// no row height floor to drop and the dense measurement is trivially equal;
    /// without the library sheet the `hytte-dense-row` class is an inert string
    /// and the test would prove only that the reconciler calls `add_css_class`.
    ///
    /// The CSS comes from [`crate::app::DEFAULT_STYLESHEET`] — the **shipped**
    /// file, `include_str!`'d there — rather than a copy retyped here, so
    /// deleting the rule from `assets/hytte-ui/style.css` turns these tests red.
    /// It is also the only reachable copy under `nix flake check`: crane's source
    /// filter strips `assets/` bar that one file.
    fn install_theme() {
        adw::init().expect("libadwaita init");
        let provider = gtk::CssProvider::new();
        provider.load_from_string(crate::app::DEFAULT_STYLESHEET);
        let display = gtk::gdk::Display::default().expect("a display");
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    fn spaced_row(id: &str, spacing: u16, children: Vec<Node>) -> Node {
        Node::Row {
            id: Some(id.to_owned()),
            classes: vec![],
            spacing: i32::from(spacing),
            children,
            tooltip: None,
        }
    }

    /// The shape the issue is about: a `.boxed-list` card of one-line rows —
    /// libadwaita's carded list, which is what makes the wrapper's height floor
    /// bite (a class-less `GtkListBox` gets a much smaller one).
    fn dense_list(dense: bool, rows: usize) -> Node {
        Node::ListBox {
            id: Some("agents".to_owned()),
            classes: vec!["boxed-list".to_owned()],
            dense,
            children: (0..rows)
                .map(|i| spaced_row(&format!("r{i}"), 0, vec![lbl(None, "argus")]))
                .collect(),
        }
    }

    /// The natural height of a realized list, at a width wide enough that no row
    /// wraps.
    fn natural_height(widget: &gtk::Widget) -> i32 {
        widget.measure(gtk::Orientation::Vertical, 320).1
    }

    /// #966 (2): a 12-row dense list must be **materially** shorter than the
    /// default one, because the height went into the `GtkListBoxRow` wrappers
    /// GTK auto-creates and a plugin cannot reach them.
    ///
    /// Every number is **measured in the same run**, never a magic constant: the
    /// floor is the theme's and moves with it, so the assertions are stated
    /// against a bare label and against the row count. They are printed too, and
    /// quoted in the PR — on this CI theme a 12-row `.boxed-list` goes 251 px →
    /// 203 px, and one row 20 px → 16 px, which is exactly its label.
    ///
    /// **Falsified** three ways, each turning a different assertion red:
    /// deleting the `apply_dense_rows` call from `build_node`, deleting the
    /// `.hytte-dense-row` rule from `assets/hytte-ui/style.css`, or dropping
    /// `padding: 0` from that rule (the one-row equality is what catches the
    /// last one — the `min-height` alone leaves the padding behind).
    #[gtk::test]
    fn a_dense_list_is_exactly_as_tall_as_its_rows_content() {
        const ROWS: usize = 12;
        install_theme();

        // One tree per measurement, each mounted fresh: the CSS class is set at
        // build, so nothing here depends on invalidation (the flip test below is
        // where that is exercised).
        let height_of = |tree: &Node| {
            let root = root();
            let mut rec = Reconciler::new(&root, |_, _| {});
            rec.render(tree);
            natural_height(&root.first_child().expect("mounted"))
        };
        let plain_h = height_of(&dense_list(false, ROWS));
        let dense_h = height_of(&dense_list(true, ROWS));
        // Single-row lists isolate one wrapper's own contribution from the
        // hairline separators a multi-row list also pays.
        let plain_1 = height_of(&dense_list(false, 1));
        let dense_1 = height_of(&dense_list(true, 1));
        let label_h = height_of(&lbl(None, "argus"));
        let rows = i32::try_from(ROWS).expect("12 fits an i32");
        let content_h = label_h * rows;

        println!(
            "#966 dense measurement: {ROWS} `.boxed-list` rows — default {plain_h}px, \
             dense {dense_h}px ({content_h}px of that is content); one row — default \
             {plain_1}px, dense {dense_1}px, bare label {label_h}px"
        );

        // The exact claim, and the strong one: with `min-height: 0; padding: 0`
        // the wrapper has no height of its own, so one dense row *is* its
        // content. Deleting either the class or the stylesheet rule turns this
        // back into the theme's floor.
        assert_eq!(
            dense_1, label_h,
            "one dense row is exactly its content: {dense_1}px vs a {label_h}px label",
        );
        // Not vacuous: the theme really does charge for the wrapper, so there is
        // something for `dense` to give back.
        assert!(
            plain_1 > dense_1,
            "…where the default row adds the theme's floor: {plain_1}px vs {dense_1}px",
        );
        assert!(
            dense_h < plain_h,
            "so a dense list is shorter: dense {dense_h}px vs default {plain_h}px",
        );
        // Across the whole list, everything left over the content is the
        // hairline separators between rows — at most a pixel each. The default
        // list is far over that, which is the ~700 px #966 opened on.
        assert!(
            dense_h - content_h <= rows,
            "a dense list carries at most a hairline per row over its content: \
             {dense_h}px vs {content_h}px of content",
        );
        assert!(
            plain_h - content_h > rows,
            "…where the default list carries the wrapper floor as well: \
             {plain_h}px vs {content_h}px of content",
        );
    }

    /// `dense` is a **mutable prop**, both directions: flipping it on a
    /// same-id re-render re-marks the wrappers of rows that were *reused*, not
    /// only ones built fresh.
    ///
    /// **Falsified** by moving the `apply_dense_rows` call out of
    /// `update_in_place` (build-only): the first flip does nothing, because
    /// every row is reused and no wrapper is ever revisited.
    #[gtk::test]
    fn flipping_dense_re_marks_reused_rows_in_both_directions() {
        const ROWS: usize = 3;
        install_theme();
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&dense_list(false, ROWS));

        // Realized in a window, because the *height* half of this test needs a
        // frame clock: a CSS class change invalidates the style, but the
        // recomputed size request only lands on a frame. Measuring an unrooted
        // tree straight after the render reads the stale cached height, and the
        // flip looks like it did nothing.
        root.set_valign(gtk::Align::Start);
        let window = gtk::Window::new();
        window.set_child(Some(&root));
        window.set_default_size(320, 600);
        window.present();
        pump();

        let widget = root.first_child().expect("list mounted");
        let before = widget.height();
        let wrappers = |w: &gtk::Widget| -> Vec<bool> {
            children(w)
                .iter()
                .map(|row| row.has_css_class(DENSE_ROW_CLASS))
                .collect()
        };
        assert_eq!(wrappers(&widget), vec![false; ROWS], "not dense to start");
        let first_row = widget.first_child().expect("a row").first_child();

        rec.render(&dense_list(true, ROWS));
        pump_until(2000, || widget.height() < before);
        assert_eq!(
            root.first_child().expect("still mounted"),
            widget,
            "the list widget is reused across the flip",
        );
        assert_eq!(
            widget.first_child().expect("a row").first_child(),
            first_row,
            "…and so are the rows, so this is the reuse path",
        );
        assert_eq!(
            wrappers(&widget),
            vec![true; ROWS],
            "every wrapper is marked — including ones that were only reused"
        );
        let dense = widget.height();
        assert!(dense < before, "dense {dense}px vs default {before}px");

        rec.render(&dense_list(false, ROWS));
        pump_until(2000, || widget.height() > dense);
        assert_eq!(
            wrappers(&widget),
            vec![false; ROWS],
            "…and unmarked again on the way back"
        );
        assert_eq!(
            widget.height(),
            before,
            "flipping back restores the original height exactly",
        );
    }

    /// #966 (1): a `Row`'s `spacing` reaches the backing box, and updates in
    /// place on a same-id re-render rather than rebuilding it.
    ///
    /// **Falsified** by dropping `boxw.set_spacing` from `update_in_place`'s
    /// `Row` arm: the widget is still reused, but the gap stays at its build
    /// value forever.
    #[gtk::test]
    fn row_spacing_applies_and_updates_in_place() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});

        rec.render(&spaced_row("r", 0, vec![lbl(None, "a"), lbl(None, "b")]));
        let widget = root.first_child().expect("row mounted");
        let boxw = widget.clone().downcast::<gtk::Box>().expect("a gtk::Box");
        assert_eq!(boxw.spacing(), 0, "the pre-#966 default is flush");
        let narrow = natural_width(&widget);

        rec.render(&spaced_row("r", 8, vec![lbl(None, "a"), lbl(None, "b")]));
        assert_eq!(
            root.first_child().expect("still mounted"),
            widget,
            "spacing is a mutable prop, not part of the node's identity",
        );
        assert_eq!(boxw.spacing(), 8);
        assert_eq!(
            natural_width(&widget),
            narrow + 8,
            "one gap between two children widens the row by exactly the spacing",
        );
    }

    fn natural_width(widget: &gtk::Widget) -> i32 {
        widget.measure(gtk::Orientation::Horizontal, -1).1
    }

    /// A tall child inside a `Node::Scrolled` is **realized in a window** so the
    /// viewport has a real allocation to clip against, then measured in the
    /// scroller's own coordinate space — the widget whose allocation clips.
    ///
    /// Returns `(scroller, body, last_row)`.
    fn mount_bounded_card(
        max_height: i32,
        rows: usize,
    ) -> (gtk::ScrolledWindow, gtk::Widget, gtk::Widget) {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        let tree = Node::Scrolled {
            id: Some("card".to_owned()),
            max_height,
            classes: vec![],
            child: Box::new(Node::Box {
                id: Some("body".to_owned()),
                dir: Dir::Vertical,
                spacing: 0,
                scroll: false,
                classes: vec![],
                children: (0..rows)
                    .map(|i| lbl(Some(&format!("r{i}")), "a row of text"))
                    .collect(),
                tooltip: None,
            }),
        };
        rec.render(&tree);
        let scroller = root
            .first_child()
            .expect("the viewport mounted")
            .downcast::<gtk::ScrolledWindow>()
            .expect("a GtkScrolledWindow");
        let body = scroller
            .child()
            .expect("the viewport has a child")
            // GTK wraps a non-scrollable child in a GtkViewport, so the box we
            // sent is one level further down than `set_child` suggests.
            .downcast::<gtk::Viewport>()
            .expect("a GtkViewport")
            .child()
            .expect("our box");
        let last = body.last_child().expect("the body has rows");

        // `max_content_height` bounds the viewport's **natural height request**;
        // the allocation only follows it where the parent honours that request.
        // A sidebar card stack does (children get their natural height, stack
        // top-aligned), and this mount says so explicitly — without the
        // `valign`, the window's own 600 px is handed straight down and the
        // scroller renders five times its cap with nothing wrong in the code.
        root.set_valign(gtk::Align::Start);
        let window = gtk::Window::new();
        window.set_child(Some(&root));
        window.set_default_size(320, 600);
        window.present();
        pump();
        (scroller, body, last)
    }

    fn pump() {
        while glib::MainContext::default().iteration(false) {}
    }

    /// Drive the main loop until `done()` or the deadline. A scroll only lands
    /// on a **frame** (it queues an allocation on the viewport), and
    /// `iteration(true)` is what lets the frame clock tick — spinning on
    /// `iteration(false)` starves it and the scroll looks like it never
    /// happened. Same shape, same reason, as the sidebar's `pump_until` (#965).
    fn pump_until(ms: u64, done: impl Fn() -> bool) {
        let expired = Rc::new(std::cell::Cell::new(false));
        let flag = expired.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(ms), move || {
            flag.set(true);
        });
        while !expired.get() && !done() {
            glib::MainContext::default().iteration(true);
        }
    }

    /// #966 (3), the whole point: a child taller than `max_height` is **clipped
    /// to it and scrollable**.
    ///
    /// Asserts the rectangle and the hit, never `is_visible()` (#851/#838): the
    /// last row is `visible` in both states here — that flag is orthogonal to
    /// being on-screen, which is exactly how a geometry bug ships past a
    /// visibility-based test.
    ///
    /// **Falsified** by dropping `apply_max_height` from `build_node` (the
    /// scroller then takes the child's full natural height and `upper ==
    /// page_size`, so there is nothing to scroll).
    #[gtk::test]
    fn a_child_taller_than_max_height_is_clipped_to_it_and_scrolls() {
        const CAP: i32 = 120;
        install_theme();
        let (scroller, _body, last) = mount_bounded_card(CAP, 30);

        assert!(
            natural_height(&scroller.clone().upcast()) <= CAP,
            "the viewport must not REQUEST more than its cap: {}px vs {CAP}px",
            natural_height(&scroller.clone().upcast()),
        );
        assert!(
            scroller.height() <= CAP,
            "…and, given a parent that honours the request, is allocated no more: {}px vs {CAP}px",
            scroller.height(),
        );
        let vadj = scroller.vadjustment();
        assert!(
            vadj.upper() > vadj.page_size(),
            "a clipped child leaves something to scroll: upper {} vs page {}",
            vadj.upper(),
            vadj.page_size(),
        );

        let viewport = f64::from(scroller.height());
        let bounds = |w: &gtk::Widget| w.compute_bounds(&scroller);
        let hits_last = |b: &gtk::graphene::Rect| {
            let (x, y) = (
                f64::from(b.x()) + f64::from(b.width()) / 2.0,
                f64::from(b.y()) + f64::from(b.height()) / 2.0,
            );
            scroller
                .pick(x, y, gtk::PickFlags::DEFAULT)
                .is_some_and(|w| w == last || w.is_ancestor(&last))
        };

        let before = bounds(&last).expect("the last row is inside the scroller");
        assert!(
            f64::from(before.y()) >= viewport,
            "before scrolling, the last row sits BELOW the viewport ({}px vs {viewport}px)",
            before.y(),
        );
        assert!(!hits_last(&before), "…and nothing there is pickable");
        assert!(
            last.is_visible(),
            "…while `is_visible` says the opposite — the #851 lesson, stated",
        );

        vadj.set_value(vadj.upper());
        pump_until(2000, || {
            bounds(&last).is_some_and(|b| f64::from(b.y()) < viewport)
        });
        let after = bounds(&last).expect("still inside the scroller");
        assert!(
            f64::from(after.y()) < viewport && f64::from(after.y() + after.height()) <= viewport,
            "after scrolling to the end the last row is fully inside the viewport: {after:?}",
        );
        assert!(hits_last(&after), "…and it is pickable there");
    }

    /// The other half of the contract: a child **shorter** than the cap is not
    /// stretched to it. That is what `propagate_natural_height` buys, and it is
    /// what lets a plugin wrap a list whose length it does not know.
    ///
    /// **Falsified** by dropping `apply_max_height` (the card inflates to the
    /// window). Deliberately *not* falsified by dropping
    /// `set_propagate_natural_height` — see [`new_viewport`] for why that
    /// setting is inert here and kept anyway.
    #[gtk::test]
    fn a_child_shorter_than_max_height_is_not_stretched() {
        const CAP: i32 = 400;
        install_theme();
        let (scroller, body, last) = mount_bounded_card(CAP, 2);
        let vadj = scroller.vadjustment();
        let content = natural_height(&body);
        let viewport = f64::from(scroller.height());

        assert!(
            content < CAP,
            "the premise: a two-row card is well under the cap ({content}px of {CAP}px)",
        );
        assert!(
            viewport < f64::from(CAP) / 2.0,
            "a two-row card must not be inflated to the cap: {viewport}px of {CAP}px",
        );
        // The *lower* bound is the load-bearing half, and an upper bound alone
        // stays green through exactly the bug this pins: a
        // `GtkScrolledWindow`'s own minimum is near zero, so without
        // `propagate_natural_height` the card **collapses** rather than
        // inflating. Stated as the rectangle and the hit (#851), not as
        // arithmetic on the scroller's chrome: the whole body must fit inside
        // the viewport, and the last row must be pickable where it is drawn.
        let bounds = body
            .compute_bounds(&scroller)
            .expect("the body is inside the scroller");
        assert!(
            f64::from(bounds.y()) >= 0.0 && f64::from(bounds.y() + bounds.height()) <= viewport,
            "the whole {content}px body fits inside the {viewport}px viewport: {bounds:?}",
        );
        let last_bounds = last
            .compute_bounds(&scroller)
            .expect("the last row is inside the scroller");
        let hit = scroller.pick(
            f64::from(last_bounds.x()) + f64::from(last_bounds.width()) / 2.0,
            f64::from(last_bounds.y()) + f64::from(last_bounds.height()) / 2.0,
            gtk::PickFlags::DEFAULT,
        );
        assert!(
            hit.is_some_and(|w| w == last || w.is_ancestor(&last)),
            "…and its last row is pickable there",
        );
        assert!(
            vadj.upper() <= vadj.page_size() + 1.0,
            "…so there is nothing to scroll: upper {} vs page {}",
            vadj.upper(),
            vadj.page_size(),
        );
    }

    /// `max_height` is a mutable prop: re-bounding a card reuses the viewport
    /// widget (and everything under it) rather than rebuilding the subtree.
    ///
    /// **Falsified** by dropping `apply_max_height` from `update_in_place`: the
    /// widget is still reused, but the cap stays at its build value.
    #[gtk::test]
    fn max_height_updates_in_place() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        let card = |max_height: i32| Node::Scrolled {
            id: Some("card".to_owned()),
            max_height,
            classes: vec![],
            child: Box::new(lbl(Some("body"), "x")),
        };

        rec.render(&card(120));
        let scroller = root
            .first_child()
            .expect("mounted")
            .downcast::<gtk::ScrolledWindow>()
            .expect("a GtkScrolledWindow");
        assert_eq!(scroller.max_content_height(), 120);

        rec.render(&card(240));
        assert_eq!(
            root.first_child().expect("still mounted"),
            scroller.clone().upcast::<gtk::Widget>(),
            "the viewport widget is reused",
        );
        assert_eq!(scroller.max_content_height(), 240);

        // `0` means unbounded, and GTK spells that `-1` — so a plugin can hand
        // over a computed cap of zero instead of branching on the node.
        rec.render(&card(0));
        assert_eq!(scroller.max_content_height(), -1);
    }

    /// A bounded card inside a **scrolling surface** (the sidebar, #965) keeps
    /// its own adjustment: driving the inner viewport to its end moves nothing
    /// on the outer one, so the two do not fight over a shared offset.
    ///
    /// That independence is the structural half of "no double-scroll fight".
    /// The *gesture* half — which of the two a wheel tick reaches — is
    /// `GtkScrolledWindow`'s standard kinetic chaining (innermost first, handing
    /// the gesture outward at its ends) and is **not** asserted here: this suite
    /// has no way to synthesize a `GdkScrollEvent` onto a surface, so it lives
    /// in `docs/live-verify.md` instead, said plainly rather than half-tested.
    #[gtk::test]
    fn a_bounded_card_inside_a_scrolling_surface_keeps_its_own_adjustment() {
        install_theme();
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&Node::Scrolled {
            id: Some("card".to_owned()),
            max_height: 120,
            classes: vec![],
            child: Box::new(Node::Box {
                id: Some("body".to_owned()),
                dir: Dir::Vertical,
                spacing: 0,
                scroll: false,
                classes: vec![],
                children: (0..30)
                    .map(|i| lbl(Some(&format!("r{i}")), "a row of text"))
                    .collect(),
                tooltip: None,
            }),
        });
        let inner = root
            .first_child()
            .expect("the viewport mounted")
            .downcast::<gtk::ScrolledWindow>()
            .expect("a GtkScrolledWindow");

        // The sidebar's own scroller, stood in for: a short surface with the
        // card plus enough below it that the surface scrolls too.
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.append(&root);
        for _ in 0..20 {
            column.append(&gtk::Label::new(Some("another card")));
        }
        let outer = gtk::ScrolledWindow::new();
        outer.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        outer.set_child(Some(&column));
        let window = gtk::Window::new();
        window.set_child(Some(&outer));
        window.set_default_size(320, 200);
        window.present();
        pump();

        let (iadj, oadj) = (inner.vadjustment(), outer.vadjustment());
        assert!(iadj.upper() > iadj.page_size(), "the card scrolls");
        assert!(oadj.upper() > oadj.page_size(), "…and so does the surface");
        assert!(
            oadj.value() < 1.0,
            "both start at the top: the surface is at {}",
            oadj.value(),
        );

        iadj.set_value(iadj.upper());
        pump_until(2000, || iadj.value() > 0.0);
        assert!(iadj.value() > 0.0, "the card scrolled inside itself");
        assert!(
            oadj.value() < 1.0,
            "…and the surface did not move with it (it is at {}) — separate adjustments, \
             no shared offset",
            oadj.value(),
        );
    }

    /// The distinction #966 came from, pinned rather than only documented:
    /// `Box { scroll: true }` is an **event target**, not a viewport. It builds
    /// a plain `gtk::Box` that measures at its children's full height and clips
    /// nothing — so a plugin that reached for it to bound a card got no bound.
    ///
    /// **Falsified** by making the `scroll` flag build a `ScrolledWindow`: the
    /// downcast fails and the height assertion goes with it.
    #[gtk::test]
    fn a_scroll_enabled_box_is_not_a_viewport() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        let tall = |scroll: bool| Node::Box {
            id: Some("b".to_owned()),
            dir: Dir::Vertical,
            spacing: 0,
            scroll,
            classes: vec![],
            children: (0..30)
                .map(|i| lbl(Some(&format!("r{i}")), "a row of text"))
                .collect(),
            tooltip: None,
        };

        rec.render(&tall(false));
        let plain = natural_height(&root.first_child().expect("mounted"));
        rec.render(&tall(true));
        let widget = root.first_child().expect("mounted");
        assert!(
            widget.downcast_ref::<gtk::ScrolledWindow>().is_none(),
            "`scroll` must not silently wrap the box in a viewport",
        );
        assert_eq!(
            natural_height(&widget),
            plain,
            "`scroll` bounds nothing — that is what `Node::Scrolled` is for",
        );
    }

    /// A `Scrolled` must never reuse a `Box`'s widget, even under a stable id —
    /// the two are the confusable pair this issue is about (a plugin migrating
    /// off `Box { scroll }` keeps its id). [`update_in_place`]'s
    /// `downcast::<gtk::ScrolledWindow>` is a main-thread panic if it ever does,
    /// so the kind discriminant is load-bearing for the whole shell.
    ///
    /// The generic machinery is already covered by
    /// `keyed_kind_change_recreates` and `kind_change_recreates_widget`, both
    /// `Label` → `Button` — and both stay green under the mutation below, which
    /// is exactly why #358 and #893 each wrote a per-variant instance for their
    /// own confusable pair (`a_gl_surface_never_reuses_a_pixels_widget`,
    /// `a_shader_never_reuses_a_pixels_or_gl_surface_widget`). This is #966's.
    ///
    /// **Falsified** by [`node_kind`]'s `Scrolled` arm returning
    /// `NodeKind::Box`: `kind invariant: widget type matches its node kind`,
    /// with every other test in this file still green.
    #[gtk::test]
    fn a_scrolled_never_reuses_a_box_widget() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&Node::Box {
            id: Some("body".to_owned()),
            dir: Dir::Vertical,
            spacing: 0,
            scroll: false,
            classes: vec![],
            children: vec![lbl(None, "x")],
            tooltip: None,
        });
        let first = root.first_child().expect("mounted");
        assert!(first.downcast_ref::<gtk::Box>().is_some(), "a Box first");

        rec.render(&Node::Scrolled {
            id: Some("body".to_owned()),
            max_height: 120,
            classes: vec![],
            child: Box::new(lbl(None, "x")),
        });
        let now = root.first_child().expect("mounted");
        assert!(
            now.downcast_ref::<gtk::ScrolledWindow>().is_some(),
            "the same id across a kind change must REBUILD, not reuse the Box",
        );
        assert_ne!(now, first, "a different widget object");
    }
}
