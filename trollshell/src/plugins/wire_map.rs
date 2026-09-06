//! Wire ⇄ host mappings (mechanical, but exhaustive).
//!
//! Projects the GTK-free [`wire`](hytte_plugin_proto::wire) vocabulary onto the
//! reconciler's `hytte_ui` types (and back for events). Each mapping is written
//! exhaustively so adding a variant to either side is a compile error here.

use std::cell::Cell;
use std::sync::Arc;

use hytte::ui::{Dir as UiDir, EventKind as UiEventKind, Node as UiNode};
use hytte_plugin_proto::wire::{self, MAX_NODES_PER_TREE, MAX_TREE_DEPTH};

use super::preem_render::{self, Scope, Warned};

/// One mapping pass's state: the scope its preem instances live in, plus the
/// two budgets [`MAX_NODES_PER_TREE`] and [`MAX_TREE_DEPTH`] give the tree
/// (#901).
struct Walk<'a> {
    scope: &'a Scope,
    /// Nodes still mappable. Decremented on **entry** to every node, so it
    /// bounds nodes *visited*.
    budget: Cell<usize>,
    /// How many [`map_node`] frames are currently on the stack. Bounded
    /// separately from `budget`, because a count cap alone is not a depth cap:
    /// 4096 nodes in a single-child chain are 4096 nested frames, measured at
    /// ~6.5 KiB each in a debug build against a main thread with 8 MiB.
    depth: Cell<usize>,
    /// Whether the count budget ran out, i.e. whether the tree on screen is a
    /// prefix of the tree the plugin sent. Read once, after the walk.
    over_budget: Cell<bool>,
    /// Whether anything was dropped for being nested too deep. Separate from
    /// `over_budget` because they are different mistakes with different fixes,
    /// and a tree can be both.
    over_depth: Cell<bool>,
}

/// The depth accounting for one live [`map_node`] frame: taken on entry,
/// released when the frame returns — **including** through the `?` a container
/// whose mandatory child was dropped propagates, which is why it is a guard and
/// not a pair of `set` calls.
struct Level<'a>(&'a Cell<usize>);

impl Drop for Level<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

impl Walk<'_> {
    /// Charge one node against the count budget and one level against the depth
    /// cap, returning the guard that releases the level again.
    ///
    /// `None` past either cap: from then on every node at or below that point is
    /// dropped and the pass is marked. The count is checked first so that a tree
    /// which is over both is reported as the size problem it primarily is.
    fn enter(&self) -> Option<Level<'_>> {
        let left = self.budget.get();
        if left == 0 {
            self.over_budget.set(true);
            return None;
        }
        let depth = self.depth.get();
        if depth >= MAX_TREE_DEPTH {
            self.over_depth.set(true);
            return None;
        }
        self.budget.set(left - 1);
        self.depth.set(depth + 1);
        Some(Level(&self.depth))
    }
}

/// Map a plugin's whole node tree onto the reconciler's `hytte_ui::Node`,
/// within `scope` — the namespace its preem renderer instances live in (one per
/// plugin per tree; see [`preem_render`]).
///
/// This is the entry point every caller uses; the recursion itself is
/// [`map_node`]. Wrapping it is what gives the preem instances a mapping-pass
/// boundary: the un-id'd node ordinal is reset here, and an instance whose node
/// disappeared from the tree is dropped when the pass closes.
///
/// # The node and depth caps (#901)
///
/// It is also the boundary the tree-wide budgets are set at. A render frame is
/// bounded on the wire only by `MAX_FRAME_LEN` (16 MiB), and the cheapest node
/// encodes to a handful of bytes, so one legal frame can carry ~a million nodes
/// — each of which becomes a GTK widget here. [`MAX_NODES_PER_TREE`] caps that.
///
/// [`MAX_TREE_DEPTH`] caps the other axis, and the count cap does **not** stand
/// in for it. A tree of `MAX_NODES_PER_TREE` single-child containers is under
/// the count cap and is 4096 nested [`map_node`] frames; those were measured at
/// **6656 B each in a debug build** and 1296 B in the shipped release profile,
/// against a main thread with 8 MiB of stack. Release would fit, spending 63 %
/// of the stack on this one function; a debug build (`cargo run -p trollshell`)
/// overflows at around 1260 — three times under the count cap. And nothing in a
/// count cap bounds the *other* recursions over the same tree on the same
/// thread (`hytte_ui`'s `build_node` and `reconcile_single`, `UiNode`'s
/// recursive `Drop`, GTK's own measure/allocate/snapshot).
///
/// **Past either cap this keeps the mapped prefix and drops the rest** rather
/// than refusing the frame. Truncate rather than reject, for the reason the
/// malformed-`Pixels` arm below degrades instead of dropping the node: this
/// file's posture is *degrade, don't blank*. A rejected frame would leave the
/// previous frame on screen (or an empty region), which on glass is
/// indistinguishable from a hung plugin; a truncated one shows the plugin's
/// chrome and its first nodes, so what an operator sees matches the journal line
/// they can go and read. It is also the stable choice: the walk is
/// deterministic, so the same prefix maps every frame and the reconciler keeps
/// updating it in place instead of rebuilding.
///
/// The cost, stated: a truncated tree renders with pieces missing — a container
/// short of children, or a whole subtree gone where a dropped node was some
/// container's only child. That looks like a rendering bug rather than a cap,
/// which is what the two warnings are for — once per plugin tree each for the
/// life of the shell, on the same latch as the preem keying diagnostics
/// (`preem_render`'s `WARNED`), since a tree over a cap is over it on every
/// frame.
pub(super) fn to_ui_node(scope: &Scope, node: &wire::Node) -> UiNode {
    preem_render::begin_pass(scope);
    let walk = Walk {
        scope,
        budget: Cell::new(MAX_NODES_PER_TREE),
        depth: Cell::new(0),
        over_budget: Cell::new(false),
        over_depth: Cell::new(false),
    };
    // `None` if the root itself was refused, which takes a chain of *mandatory*
    // single-child containers (`Button`/`Revealer`/`Expander` header) past one
    // of the two caps: the innermost is dropped and every ancestor is dropped
    // with it. `MAX_TREE_DEPTH` is what makes that reachable in practice —
    // before it, the chain had to be `MAX_NODES_PER_TREE` long. A `Spacer` is
    // the reconciler's cheapest node and keeps the region non-empty.
    let mapped = map_node(&walk, node).unwrap_or(UiNode::Spacer);
    preem_render::end_pass(scope);

    if walk.over_budget.get() && preem_render::warn_once(scope, Warned::NodeCap) {
        tracing::warn!(
            plugin = scope.plugin_id(),
            tree = ?scope.role(),
            cap = MAX_NODES_PER_TREE,
            "plugin render tree exceeds the host's node cap; the nodes up to the cap are \
             rendered and the rest of the tree is dropped, so part of this plugin's UI is \
             missing — a container short of children, or a whole subtree gone where the \
             dropped node was some container's only child (further occurrences in this tree \
             are silenced for the rest of this shell run)",
        );
    }
    if walk.over_depth.get() && preem_render::warn_once(scope, Warned::DepthCap) {
        tracing::warn!(
            plugin = scope.plugin_id(),
            tree = ?scope.role(),
            cap = MAX_TREE_DEPTH,
            "plugin render tree nests deeper than the host will walk; everything below the \
             cap is dropped. The host recurses once per level on the GTK main thread, so this \
             is a stack bound rather than a taste one — flatten the tree (further occurrences \
             in this tree are silenced for the rest of this shell run)",
        );
    }
    mapped
}

/// Map a wire [`wire::Node`] onto the reconciler's `hytte_ui::Node`. The two
/// mirror each other field-for-field (#266), so this is a 1:1 recursion — but it
/// is written exhaustively so adding a node variant to either side is a compile
/// error here.
///
/// `None` means the node fell past [`MAX_NODES_PER_TREE`] or [`MAX_TREE_DEPTH`]
/// and is dropped: a child list simply loses it, and a container whose
/// *mandatory* child came back `None` is dropped in turn (`hytte_ui`'s
/// `Button`/`Revealer`/`Expander` header are not optional), so the mapped tree
/// is never larger than the budget even though the budget is charged on entry.
// One exhaustive arm per node variant — the length is the vocabulary size, not
// complexity.
#[allow(clippy::too_many_lines)]
fn map_node(walk: &Walk, node: &wire::Node) -> Option<UiNode> {
    // Held for the body of this frame: the guard is what puts the level back
    // when we return, down every `?` path below as well as the normal one.
    let _level = walk.enter()?;
    let mapped = match node {
        wire::Node::Box {
            id,
            dir,
            spacing,
            scroll,
            classes,
            children,
        } => UiNode::Box {
            id: id.clone(),
            dir: to_ui_dir(*dir),
            spacing: *spacing,
            scroll: *scroll,
            classes: classes.clone(),
            children: children
                .iter()
                .filter_map(|child| map_node(walk, child))
                .collect(),
        },
        wire::Node::Row {
            id,
            classes,
            children,
        } => UiNode::Row {
            id: id.clone(),
            classes: classes.clone(),
            children: children
                .iter()
                .filter_map(|child| map_node(walk, child))
                .collect(),
        },
        wire::Node::ListBox {
            id,
            classes,
            children,
        } => UiNode::ListBox {
            id: id.clone(),
            classes: classes.clone(),
            children: children
                .iter()
                .filter_map(|child| map_node(walk, child))
                .collect(),
        },
        wire::Node::Label { id, text, classes } => UiNode::Label {
            id: id.clone(),
            text: text.clone(),
            classes: classes.clone(),
        },
        wire::Node::Text {
            id,
            text,
            max_width_chars,
            ellipsize,
            classes,
        } => UiNode::Text {
            id: id.clone(),
            text: text.clone(),
            max_width_chars: *max_width_chars,
            ellipsize: *ellipsize,
            classes: classes.clone(),
        },
        wire::Node::Icon { id, name, classes } => UiNode::Icon {
            id: id.clone(),
            name: name.clone(),
            classes: classes.clone(),
        },
        wire::Node::Pixels {
            id,
            width,
            height,
            data,
            scale,
            classes,
        } => {
            // Validation seam: a plugin's RGBA8 buffer is untrusted, so this is
            // the one non-1:1 arm. `data.len()` MUST equal `width*height*4`; a
            // mismatch degrades to an empty (nothing-rendered) surface — id and
            // classes preserved so CSS chrome stays and a later valid frame
            // updates in place — with a `tracing::warn!`. This is the single
            // documented seam (the host is the trust boundary and the only layer
            // with `tracing`); the `hytte_ui` widget stays a silent panic-safe
            // backstop, and decode stays permissive so one bad node can't drop
            // the whole connection.
            // `Arc::from(&data[..])` and not `Arc::from(data.clone())`: both end
            // in one copy of the RGBA block (an `Arc<[u8]>` stores its refcount
            // inline ahead of the bytes, so it can never adopt a `Vec`'s
            // allocation), but the second would pay for a `Vec` on the way
            // through. This is exactly the single copy the `data.clone()` here
            // always was — plugin-side pixels arrive owned from MessagePack and
            // this mapping is per monitor, so there is no shared cache upstream
            // to hand a handle on (#911). Downstream of here the buffer is
            // shared like every other `Pixels` node's.
            let (width, height, data) = if pixels_len_ok(*width, *height, data.len()) {
                (*width, *height, Arc::from(&data[..]))
            } else {
                tracing::warn!(
                    node = ?id,
                    width = *width,
                    height = *height,
                    data_len = data.len(),
                    "plugin Pixels buffer size != width*height*4; rendering nothing"
                );
                (0, 0, preem_render::nothing())
            };
            // Same seam for the `scale` hint (#358): an absurd upscale is
            // clamped (with a warn) rather than honored, so a malformed plugin
            // can't request a monster allocation; `0` silently means `1` (the
            // wire contract's documented default alias, not worth a warning).
            let scale = {
                let clamped = clamp_pixels_scale(width, height, *scale);
                if clamped < *scale {
                    tracing::warn!(
                        node = ?id,
                        width,
                        height,
                        scale = *scale,
                        clamped,
                        "plugin Pixels scale exceeds the scaled-dimension cap; clamped"
                    );
                }
                clamped
            };
            UiNode::Pixels {
                id: id.clone(),
                width,
                height,
                data,
                scale,
                classes: classes.clone(),
            }
        }
        wire::Node::Button { id, classes, child } => UiNode::Button {
            id: id.clone(),
            classes: classes.clone(),
            child: Box::new(map_node(walk, child)?),
        },
        wire::Node::Progress {
            id,
            fraction,
            classes,
        } => {
            // The float seam for this arm (#904), the `f64` analogue of the
            // `Preem` arm's mandatory `clamp_in_place` below: a `NaN` here is
            // stored verbatim by `gtk_progress_bar_set_fraction` and then cast
            // into an `int` allocation width. Mapping runs once per monitor per
            // frame, so the per-field form is used rather than cloning the node
            // to call `wire::Node::clamp_in_place` — the two share one
            // implementation, so they cannot drift. The SDK sanitises the same
            // tree before sending it; this is the host's own defence, since an
            // SDK-built plugin is not the only thing that can dial the socket,
            // and it is free because the sanitiser is a fixpoint.
            let sane = wire::sane_fraction(*fraction);
            // Warned, not silent: the same trust-boundary treatment the `Pixels`
            // arm above gives a malformed buffer, for the same reason — the host
            // is the layer with `tracing`, and a rewritten value is precisely
            // what a plugin author needs told. Compared by bit pattern so the
            // `-0.0` the sanitiser deliberately preserves is not reported as a
            // change. Like the `Pixels` warns this fires per mapping pass rather
            // than once; the host keeps no per-node state to remember it in, and
            // inventing some for a log line is not worth it.
            if sane.to_bits() != fraction.to_bits() {
                tracing::warn!(
                    node = ?id,
                    fraction = *fraction,
                    sanitised = sane,
                    "plugin Progress fraction is non-finite or outside 0.0..=1.0; sanitised"
                );
            }
            UiNode::Progress {
                id: id.clone(),
                fraction: sane,
                classes: classes.clone(),
            }
        }
        wire::Node::Slider {
            id,
            min,
            max,
            value,
            step,
            enabled,
            classes,
        } => {
            // Same seam (#904). This one is not merely dedup hygiene: a
            // degenerate range reaches `gtk::Adjustment::new`, whose
            // `lower + page_size <= upper` guard returns NULL for `max < min`
            // or a `NaN` end — a null the gtk4 binding then wraps, which is a
            // debug-build panic and release-build UB in the shell.
            let sane = wire::sane_slider_floats(*min, *max, *value, *step);
            // Warned on the same terms as the `Progress` arm above — and this
            // one replaces the plugin's stated *scale*, which is the thing its
            // UI is about, so staying silent would be worse here than there.
            if sane.min.to_bits() != min.to_bits()
                || sane.max.to_bits() != max.to_bits()
                || sane.value.to_bits() != value.to_bits()
                || sane.step.to_bits() != step.to_bits()
            {
                tracing::warn!(
                    node = %id,
                    min = *min,
                    max = *max,
                    value = *value,
                    step = *step,
                    sane_min = sane.min,
                    sane_max = sane.max,
                    sane_value = sane.value,
                    sane_step = sane.step,
                    "plugin Slider floats are non-finite or out of range; sanitised"
                );
            }
            UiNode::Slider {
                id: id.clone(),
                min: sane.min,
                max: sane.max,
                value: sane.value,
                step: sane.step,
                enabled: *enabled,
                classes: classes.clone(),
            }
        }
        wire::Node::Revealer { id, open, child } => UiNode::Revealer {
            id: id.clone(),
            open: *open,
            child: Box::new(map_node(walk, child)?),
        },
        wire::Node::Separator { classes } => UiNode::Separator {
            classes: classes.clone(),
        },
        wire::Node::Spacer => UiNode::Spacer,
        wire::Node::Expander {
            id,
            header,
            children,
            expanded,
            classes,
        } => UiNode::Expander {
            id: id.clone(),
            header: Box::new(map_node(walk, header)?),
            children: children
                .iter()
                .filter_map(|child| map_node(walk, child))
                .collect(),
            expanded: *expanded,
            classes: classes.clone(),
        },
        wire::Node::Entry {
            id,
            text,
            placeholder,
            classes,
        } => UiNode::Entry {
            id: id.clone(),
            text: text.clone(),
            placeholder: placeholder.clone(),
            classes: classes.clone(),
        },
        wire::Node::Preem {
            id,
            classes,
            widget,
        } => {
            // #882's typed preem vocabulary, rendered in-process by #883's
            // renderer instances (`preem_render`).
            //
            // **`clamped()` is mandatory and must stay the first thing that
            // happens here.** It is the wire-limit enforcement seam — the preem
            // analogue of the `pixels_len_ok`/`clamp_pixels_scale` checks in the
            // arm above — and it is what stops a hostile config (a 32768×32768
            // scaled buffer, a 16.7M-tick gauge face) from reaching a renderer.
            // The renderer below rasterises **this** value, never the raw
            // `widget`, and nothing downstream re-derives geometry from the
            // unclamped one.
            //
            // `clamp_in_place` on an owned clone rather than the consuming
            // `clamped()`: this sits on the per-frame path, where the owning
            // form would clone every String/Vec in the config again just to feed
            // the clamp.
            let mut widget = widget.as_ref().clone();
            widget.clamp_in_place();

            // **Rendered whether or not the plugin negotiated the vocabulary.**
            // The contract says a plugin emits `Node::Preem` only above the
            // generation the host advertised in `HostMsg::Hello`, but this arm
            // does not re-check `negotiates_vocab()` — a plugin that sends one
            // anyway is drawn. That is deliberate: the value is clamped above
            // and the negotiation exists so a plugin knows what the *host* can
            // decode, not as an authorisation gate. Refusing to draw a
            // well-formed node the host understands would only make a
            // hand-rolled (non-Rust-SDK) client harder to write for no safety
            // gained.

            // `id` is the reconciliation key for the renderer instance (and
            // #900 makes it the contract, not an optimisation), but a missing
            // one is handled *there*, not here: `map_widget` falls back to a
            // positional key and warns at most once per tree per shell run, so
            // a hand-rolled plugin degrades rather than losing the widget.
            preem_render::map_widget(walk.scope, id.as_deref(), classes, &widget)
        }
    };
    Some(mapped)
}

fn to_ui_dir(dir: wire::Dir) -> UiDir {
    match dir {
        wire::Dir::Horizontal => UiDir::Horizontal,
        wire::Dir::Vertical => UiDir::Vertical,
    }
}

/// Whether a [`wire::Node::Pixels`] buffer honors the RGBA8 size invariant:
/// `data_len == width * height * 4`, computed in `u64` so the product can't
/// overflow. `(0, 0, 0)` is consistent (a legitimate empty surface).
pub(super) fn pixels_len_ok(width: u32, height: u32, data_len: usize) -> bool {
    let expected = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|n| n.checked_mul(4));
    expected == u64::try_from(data_len).ok()
}

/// Cap on a [`wire::Node::Pixels`]'s *scaled* natural dimension
/// (`max(width, height) * scale`), so a hostile/buggy `scale` can't request a
/// monster widget. 16384 px is a common max-texture edge and far beyond any
/// sane sidebar surface.
const MAX_PIXELS_SCALED_DIM: u32 = 16_384;

/// Clamp a [`wire::Node::Pixels`] `scale` hint to something the host will
/// honor: at least `1` (the wire contract treats `0`/absent as `1`), and small
/// enough that the scaled natural dimension stays within
/// [`MAX_PIXELS_SCALED_DIM`]. An empty (or already over-cap) buffer gets `1` —
/// scale is inert there anyway.
pub(super) fn clamp_pixels_scale(width: u32, height: u32, scale: u32) -> u32 {
    let dim = width.max(height);
    if dim == 0 {
        return 1;
    }
    scale.clamp(1, (MAX_PIXELS_SCALED_DIM / dim).max(1))
}

/// Map a reconciler event back onto its wire form for the outbound `Event`
/// frame. Exhaustive over the `EventKind` set (Click, Scroll, `ValueChanged`,
/// `Submitted`), so adding a kind to either side breaks the build here rather
/// than silently dropping an event.
pub(super) fn to_wire_event(kind: UiEventKind) -> wire::EventKind {
    match kind {
        UiEventKind::Click => wire::EventKind::Click,
        UiEventKind::Scroll { dx, dy } => wire::EventKind::Scroll { dx, dy },
        UiEventKind::ValueChanged { value } => wire::EventKind::ValueChanged { value },
        UiEventKind::Submitted { text } => wire::EventKind::Submitted { text },
    }
}
