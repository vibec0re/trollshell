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
use std::collections::HashMap;
use std::rc::Rc;

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
/// callback, tagged with the originating node's [`NodeId`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EventKind {
    /// A [`Node::Button`] was clicked.
    Click,
    /// A [`Node::Box`] with `scroll: true` was scrolled. `dx`/`dy` are the
    /// raw GTK scroll deltas.
    Scroll { dx: f64, dy: f64 },
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
    /// Both `text` and `max_width_chars` update in place.
    Text {
        id: Option<NodeId>,
        text: String,
        max_width_chars: Option<i32>,
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
    /// crisp "LCD"-style pixels. `data` is a **mutable** prop: a same-id
    /// re-render swaps the texture in place (like [`Node::Label`]'s `text`).
    /// An inconsistent buffer renders nothing (the widget is panic-safe); the
    /// upstream host validates and warns.
    Pixels {
        id: Option<NodeId>,
        width: u32,
        height: u32,
        data: Vec<u8>,
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
    /// A `gtk::Revealer`; `open` drives `set_reveal_child`.
    Revealer {
        id: Option<NodeId>,
        open: bool,
        child: Box<Node>,
    },
    /// A `gtk::Separator`.
    Separator { classes: Vec<String> },
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
    /// is clicked or a scroll-enabled (`scroll: true`) [`Node::Box`] is
    /// scrolled.
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
    Revealer,
    Separator,
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

// ── Build / update ──────────────────────────────────────────────────────────

/// Build a fresh widget subtree for `node`, wiring its event handlers **once**
/// (this is the only place `connect_clicked` / the scroll controller is
/// attached, so reuse across renders can never stack duplicate handlers).
// One exhaustive arm per node variant — the length is the vocabulary size, not
// complexity; splitting it hurts readability more than it helps.
#[allow(clippy::too_many_lines)]
fn build_node(node: &Node, on_event: &EventFn) -> RetainedNode {
    let mut scroll_controller = None;
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
            let kids = build_children(&boxw, children, on_event);
            (boxw.upcast(), kids)
        }
        Node::Row {
            classes, children, ..
        } => {
            let boxw = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            apply_classes(&boxw, classes);
            let kids = build_children(&boxw, children, on_event);
            (boxw.upcast(), kids)
        }
        Node::ListBox {
            classes, children, ..
        } => {
            let boxw = gtk::Box::new(gtk::Orientation::Vertical, 0);
            apply_classes(&boxw, classes);
            let kids = build_children(&boxw, children, on_event);
            (boxw.upcast(), kids)
        }
        Node::Label { text, classes, .. } => {
            let label = gtk::Label::new(Some(text));
            apply_classes(&label, classes);
            (label.upcast(), Vec::new())
        }
        Node::Text {
            text,
            max_width_chars,
            classes,
            ..
        } => {
            let label = gtk::Label::new(Some(text));
            // A wrapping label: word-then-char boundaries so an unbroken long
            // token still can't force its container wider (the #281 fix).
            label.set_wrap(true);
            label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
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
            classes,
            ..
        } => {
            let surface = crate::pixels::PixelSurface::new();
            surface.set_pixels(*width, *height, data);
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
    };

    RetainedNode {
        widget,
        desc: desc_of(node),
        children,
        scroll_controller,
    }
}

/// Build and append each child into the `gtk::Box` container, returning the
/// realized children in render order. Shared by the `Box` / `Row` / `ListBox`
/// container arms of [`build_node`].
fn build_children(
    container: &gtk::Box,
    children: &[Node],
    on_event: &EventFn,
) -> Vec<RetainedNode> {
    let mut kids = Vec::with_capacity(children.len());
    for child in children {
        let realized = build_node(child, on_event);
        container.append(&realized.widget);
        kids.push(realized);
    }
    kids
}

/// Update an already-realized node in place. Precondition (guaranteed by the
/// caller via [`reusable`] / [`plan_diff`]): `retained`'s kind and id match
/// `new`, so the widget downcast always succeeds and event handlers stay
/// valid (a `Button`/`Box` never changes identity across an id change).
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
            diff_children(boxw, &mut retained.children, children, on_event);
        }
        Node::Row {
            classes, children, ..
        }
        | Node::ListBox {
            classes, children, ..
        } => {
            // Row/ListBox are gtk::Box containers with no scroll or orientation
            // props to mutate (orientation is fixed by the kind, which reuse
            // already matched on), so only classes + children reconcile.
            let boxw = downcast::<gtk::Box>(&retained.widget);
            reconcile_classes(boxw, &retained.desc.classes, classes);
            diff_children(boxw, &mut retained.children, children, on_event);
        }
        Node::Label { text, classes, .. } => {
            let label = downcast::<gtk::Label>(&retained.widget);
            label.set_text(text);
            reconcile_classes(label, &retained.desc.classes, classes);
        }
        Node::Text {
            text,
            max_width_chars,
            classes,
            ..
        } => {
            let label = downcast::<gtk::Label>(&retained.widget);
            label.set_text(text);
            // `-1` is GTK's "no maximum", so a flip back to `None` resets it.
            label.set_max_width_chars(max_width_chars.unwrap_or(-1));
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
            classes,
            ..
        } => {
            let surface = downcast::<crate::pixels::PixelSurface>(&retained.widget);
            // `data` is a mutable prop: swap the texture in place (no rebuild).
            surface.set_pixels(*width, *height, data);
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

/// Reconcile a `gtk::Box`'s children using the keyed [`plan_diff`].
fn diff_children(
    container: &gtk::Box,
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
    // the box (reorder); freshly created ones are not yet (insert).
    let mut prev_sibling: Option<gtk::Widget> = None;
    for (slot, op) in plan.ops.iter().enumerate() {
        let widget = next[slot].widget.clone();
        match *op {
            SlotOp::Create => container.insert_child_after(&widget, prev_sibling.as_ref()),
            SlotOp::Reuse(_) => container.reorder_child_after(&widget, prev_sibling.as_ref()),
        }
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
        Node::Revealer { .. } => NodeKind::Revealer,
        Node::Separator { .. } => NodeKind::Separator,
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
        Node::Button { id, .. } => Some(id.as_str()),
        Node::Separator { .. } => None,
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
        | Node::Separator { classes } => classes,
        // `Revealer` carries no classes of its own (see the `Node` vocab); it
        // is a transparent open/close wrapper, so style its child instead.
        Node::Revealer { .. } => &[],
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

    #[gtk::test]
    fn listbox_and_row_build_as_oriented_boxes() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&listbox(
            Some("list"),
            vec![row(Some("r0"), vec![text(None, "hi", None)])],
        ));

        let list = root
            .first_child()
            .unwrap()
            .downcast::<gtk::Box>()
            .expect("ListBox → gtk::Box");
        assert_eq!(list.orientation(), gtk::Orientation::Vertical);
        let inner_row = list
            .first_child()
            .unwrap()
            .downcast::<gtk::Box>()
            .expect("Row → gtk::Box");
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
    fn listbox_rows_keyed_diff_insert_and_remove() {
        let root = root();
        let mut rec = Reconciler::new(&root, |_, _| {});
        rec.render(&listbox(
            Some("list"),
            vec![row(Some("a"), vec![]), row(Some("b"), vec![])],
        ));
        let list = root.first_child().unwrap();
        let before = children(&list);
        assert_eq!(before.len(), 2);

        // Insert "z" between a and b; a and b keep their widget identities.
        rec.render(&listbox(
            Some("list"),
            vec![
                row(Some("a"), vec![]),
                row(Some("z"), vec![]),
                row(Some("b"), vec![]),
            ],
        ));
        let after = children(&root.first_child().unwrap());
        assert_eq!(after.len(), 3);
        assert_eq!(after[0], before[0], "row a reused in place");
        assert_eq!(after[2], before[1], "row b reused, shifted right");

        // Drop "a": b survives untouched.
        rec.render(&listbox(Some("list"), vec![row(Some("b"), vec![])]));
        let last = children(&root.first_child().unwrap());
        assert_eq!(last.len(), 1);
        assert_eq!(last[0], before[1], "row b is the surviving sibling");
    }
}
