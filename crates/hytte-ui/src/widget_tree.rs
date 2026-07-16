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
use std::time::{Duration, Instant};

/// Stable, plugin-meaningful node identity. Doubles as the diff key and the
/// event target. A `Button` requires one; other nodes may omit it (and then
/// fall back to positional matching).
pub type NodeId = String;

/// Orientation for a [`Node::Box`], mapped to `gtk::Orientation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Horizontal,
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
    Scroll { dx: f64, dy: f64 },
    /// A [`Node::Slider`] was moved by the user (drag / scroll / keyboard).
    /// `value` is its new position, clamped to the slider's `min..=max`. Emitted
    /// on a trailing-edge throttle (never one per raw motion tick) and **only**
    /// for user-driven changes — a programmatic re-render that moves the thumb
    /// does not fire it (the reconciler wires `change-value`, not
    /// `value-changed`; see [`attach_slider`]).
    ValueChanged { value: f64 },
    /// A [`Node::Entry`]'s text was submitted (the user pressed
    /// Enter/activate); `text` is the entry's full contents at that moment.
    /// Fired **only** for the user's activate — a programmatic `set_text`
    /// never emits GTK's `activate`, so a re-render echoing `text` back can't
    /// re-enter the event path (the entry analogue of the slider's
    /// `change-value` wiring). No per-keystroke event exists (v1 — see the
    /// wire vocab's rationale).
    Submitted { text: String },
}

/// The closed widget vocabulary. A plugin tree is a single root `Node`.
///
/// `classes` are applied verbatim as GTK CSS classes (`add_css_class`); the
/// plugin is expected to use the existing `ts-*` / `hytte-*` token contract.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// A `gtk::Box`. `id` (optional) keys the node for diffing/reordering;
    /// `scroll` independently controls whether it becomes a scroll event
    /// target (an `EventControllerScroll` forwarding raw deltas via
    /// [`EventKind::Scroll`]).
    Box {
        id: Option<NodeId>,
        dir: Dir,
        spacing: i32,
        scroll: bool,
        classes: Vec<String>,
        children: Vec<Node>,
    },
    /// A list **row** — a horizontal `gtk::Box` sibling of [`Node::Box`] for
    /// list-y cards. Children are diffed exactly like a `Box`'s.
    Row {
        id: Option<NodeId>,
        classes: Vec<String>,
        children: Vec<Node>,
    },
    /// A vertical list **container** stacking its children (typically
    /// [`Node::Row`]s). Materialized as a vertical `gtk::Box`; children diff
    /// like a `Box`'s.
    ListBox {
        id: Option<NodeId>,
        classes: Vec<String>,
        children: Vec<Node>,
    },
    /// A `gtk::Label`.
    Label {
        id: Option<NodeId>,
        text: String,
        classes: Vec<String>,
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
        id: Option<NodeId>,
        text: String,
        max_width_chars: Option<i32>,
        ellipsize: bool,
        classes: Vec<String>,
    },
    /// A `gtk::Image` set from a themed icon `name`.
    Icon {
        id: Option<NodeId>,
        name: String,
        classes: Vec<String>,
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
    Pixels {
        id: Option<NodeId>,
        width: u32,
        height: u32,
        data: Vec<u8>,
        scale: u32,
        classes: Vec<String>,
    },
    /// A `gtk::Button`. `id` is **required** — it is the click event target.
    Button {
        id: NodeId,
        classes: Vec<String>,
        child: Box<Node>,
    },
    /// A `gtk::ProgressBar`, `fraction` in `0.0..=1.0`.
    Progress {
        id: Option<NodeId>,
        fraction: f64,
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
        id: NodeId,
        min: f64,
        max: f64,
        value: f64,
        step: f64,
        enabled: bool,
        classes: Vec<String>,
    },
    /// A `gtk::Revealer`; `open` drives `set_reveal_child`.
    Revealer {
        id: Option<NodeId>,
        open: bool,
        child: Box<Node>,
    },
    /// A `gtk::Separator`.
    Separator { classes: Vec<String> },
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
        id: NodeId,
        header: Box<Node>,
        children: Vec<Node>,
        expanded: bool,
        classes: Vec<String>,
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
        id: NodeId,
        text: String,
        placeholder: String,
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
    Label,
    Text,
    Icon,
    Pixels,
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
            classes, children, ..
        } => {
            let boxw = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            apply_classes(&boxw, classes);
            let kids = build_children(&Container::Box(boxw.clone()), children, on_event);
            (boxw.upcast(), kids)
        }
        Node::ListBox {
            classes, children, ..
        } => {
            // A **real** `gtk::ListBox` (not a plain vertical Box) so libadwaita's
            // `.boxed-list` card styling — which selects `list.boxed-list` — can
            // actually paint (see [`Container`]). Selection-less: a plugin list is
            // a display/command surface, and the vocab carries no selection event.
            let list = gtk::ListBox::new();
            list.set_selection_mode(gtk::SelectionMode::None);
            apply_classes(&list, classes);
            let kids = build_children(&Container::List(list.clone()), children, on_event);
            (list.upcast(), kids)
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
            surface.set_pixels(*width, *height, data);
            surface.set_scale(*scale);
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
            // asserts `min < max` and `step != 0`) so an ill-formed plugin range
            // can never trip a GTK critical / NULL return. `page_size = 0`: a
            // scale is a point selector, so the whole `min..=max` is reachable.
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
            classes, children, ..
        } => {
            // A Row is a horizontal gtk::Box with no scroll or orientation props
            // to mutate (orientation is fixed by the kind, which reuse already
            // matched on), so only classes + children reconcile.
            let boxw = downcast::<gtk::Box>(&retained.widget);
            reconcile_classes(boxw, &retained.desc.classes, classes);
            diff_children(
                &Container::Box(boxw.clone()),
                &mut retained.children,
                children,
                on_event,
            );
        }
        Node::ListBox {
            classes, children, ..
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
            surface.set_pixels(*width, *height, data);
            surface.set_scale(*scale);
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
        Node::Label { .. } => NodeKind::Label,
        Node::Text { .. } => NodeKind::Text,
        Node::Icon { .. } => NodeKind::Icon,
        Node::Pixels { .. } => NodeKind::Pixels,
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
        | Node::Progress { id, .. }
        | Node::Revealer { id, .. } => id.as_deref(),
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
        | Node::Button { classes, .. }
        | Node::Progress { classes, .. }
        | Node::Slider { classes, .. }
        | Node::Expander { classes, .. }
        | Node::Entry { classes, .. }
        | Node::Separator { classes } => classes,
        // `Revealer` carries no classes of its own (see the `Node` vocab); it
        // is a transparent open/close wrapper, so style its child instead.
        // `Spacer` is style-less on purpose — a structural gap, never itself
        // themed.
        Node::Revealer { .. } | Node::Spacer => &[],
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
    }
}

// ── Pure diff-plan tests (hermetic — no display server) ──────────────────────

#[cfg(test)]
mod diff_tests {
    use super::{ChildKey, DiffPlan, NodeKind, SlotOp, plan_diff};

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
    use super::{Dir, EventKind, Node, Reconciler};
    use gtk::prelude::*;
    use std::cell::RefCell;
    use std::rc::Rc;

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
        });
        let w1 = root.first_child().unwrap();
        assert!(w1.has_css_class("one"));

        rec.render(&Node::Label {
            id: None,
            text: "t".into(),
            classes: vec!["two".into()],
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

    fn pix(id: Option<&str>, width: u32, height: u32, data: Vec<u8>) -> Node {
        Node::Pixels {
            id: id.map(ToOwned::to_owned),
            width,
            height,
            data,
            scale: 1,
            classes: vec![],
        }
    }

    #[gtk::test]
    fn pixels_builds_surface() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        // 1×1 opaque red.
        rec.render(&pix(Some("lcd"), 1, 1, vec![255, 0, 0, 255]));
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
        rec.render(&pix(Some("lcd"), 1, 1, vec![255, 0, 0, 255]));
        let before = root.first_child().unwrap();

        // Same id, new bytes: mutable-prop update, widget identity preserved.
        rec.render(&pix(Some("lcd"), 1, 1, vec![0, 255, 0, 255]));
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

    #[gtk::test]
    fn pixels_scale_is_a_mutable_prop() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&pix(Some("lcd"), 1, 1, vec![255, 0, 0, 255]));
        let surface = root
            .first_child()
            .unwrap()
            .downcast::<crate::pixels::PixelSurface>()
            .unwrap();
        assert_eq!(surface.measure(gtk::Orientation::Horizontal, -1).1, 1);

        // Same id, new scale: mutable-prop update — the widget is reused and
        // its natural request grows to buffer × scale.
        rec.render(&Node::Pixels {
            id: Some("lcd".into()),
            width: 1,
            height: 1,
            data: vec![255, 0, 0, 255],
            scale: 4,
            classes: vec![],
        });
        assert_eq!(
            root.first_child().unwrap(),
            surface.clone().upcast::<gtk::Widget>(),
            "same-id scale flip reuses the surface in place"
        );
        assert_eq!(surface.measure(gtk::Orientation::Horizontal, -1).1, 4);
    }

    #[gtk::test]
    fn pixels_scale_is_a_mutable_prop() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&pix(Some("lcd"), 1, 1, vec![255, 0, 0, 255]));
        let surface = root
            .first_child()
            .unwrap()
            .downcast::<crate::pixels::PixelSurface>()
            .unwrap();
        assert_eq!(surface.measure(gtk::Orientation::Horizontal, -1).1, 1);

        // Same id, new scale: mutable-prop update — the widget is reused and
        // its natural request grows to buffer × scale.
        rec.render(&Node::Pixels {
            id: Some("lcd".into()),
            width: 1,
            height: 1,
            data: vec![255, 0, 0, 255],
            scale: 4,
            classes: vec![],
        });
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
        rec.render(&pix(Some("lcd"), 2, 2, vec![1, 2, 3]));
        let w = root
            .first_child()
            .unwrap()
            .downcast::<crate::pixels::PixelSurface>()
            .unwrap();
        // A subsequent valid frame still renders in place (kind/id unchanged).
        rec.render(&pix(Some("lcd"), 1, 1, vec![9, 9, 9, 255]));
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
        }
    }

    fn listbox(id: Option<&str>, children: Vec<Node>) -> Node {
        Node::ListBox {
            id: id.map(ToOwned::to_owned),
            classes: vec![],
            children,
        }
    }

    fn row(id: Option<&str>, children: Vec<Node>) -> Node {
        Node::Row {
            id: id.map(ToOwned::to_owned),
            classes: vec![],
            children,
        }
    }

    fn listbox_classed(id: Option<&str>, classes: Vec<&str>, children: Vec<Node>) -> Node {
        Node::ListBox {
            id: id.map(ToOwned::to_owned),
            classes: classes.into_iter().map(ToOwned::to_owned).collect(),
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
}
