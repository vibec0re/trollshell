//! Wire ⇄ host mappings (mechanical, but exhaustive).
//!
//! Projects the GTK-free [`wire`](hytte_plugin_proto::wire) vocabulary onto the
//! reconciler's `hytte_ui` types (and back for events). Each mapping is written
//! exhaustively so adding a variant to either side is a compile error here.

use hytte::ui::{Dir as UiDir, EventKind as UiEventKind, Node as UiNode};
use hytte_plugin_proto::wire;

use super::preem_render::{self, Scope};

/// Map a plugin's whole node tree onto the reconciler's `hytte_ui::Node`,
/// within `scope` — the namespace its preem renderer instances live in (one per
/// plugin per tree; see [`preem_render`]).
///
/// This is the entry point every caller uses; the recursion itself is
/// [`map_node`]. Wrapping it is what gives the preem instances a mapping-pass
/// boundary: the un-id'd node ordinal is reset here, and an instance whose node
/// disappeared from the tree is dropped when the pass closes.
pub(super) fn to_ui_node(scope: &Scope, node: &wire::Node) -> UiNode {
    preem_render::begin_pass(scope);
    let mapped = map_node(scope, node);
    preem_render::end_pass(scope);
    mapped
}

/// Map a wire [`wire::Node`] onto the reconciler's `hytte_ui::Node`. The two
/// mirror each other field-for-field (#266), so this is a 1:1 recursion — but it
/// is written exhaustively so adding a node variant to either side is a compile
/// error here.
// One exhaustive arm per node variant — the length is the vocabulary size, not
// complexity.
#[allow(clippy::too_many_lines)]
fn map_node(scope: &Scope, node: &wire::Node) -> UiNode {
    match node {
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
                .map(|child| map_node(scope, child))
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
                .map(|child| map_node(scope, child))
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
                .map(|child| map_node(scope, child))
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
            let (width, height, data) = if pixels_len_ok(*width, *height, data.len()) {
                (*width, *height, data.clone())
            } else {
                tracing::warn!(
                    node = ?id,
                    width = *width,
                    height = *height,
                    data_len = data.len(),
                    "plugin Pixels buffer size != width*height*4; rendering nothing"
                );
                (0, 0, Vec::new())
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
            child: Box::new(map_node(scope, child)),
        },
        wire::Node::Progress {
            id,
            fraction,
            classes,
        } => UiNode::Progress {
            id: id.clone(),
            fraction: *fraction,
            classes: classes.clone(),
        },
        wire::Node::Slider {
            id,
            min,
            max,
            value,
            step,
            enabled,
            classes,
        } => UiNode::Slider {
            id: id.clone(),
            min: *min,
            max: *max,
            value: *value,
            step: *step,
            enabled: *enabled,
            classes: classes.clone(),
        },
        wire::Node::Revealer { id, open, child } => UiNode::Revealer {
            id: id.clone(),
            open: *open,
            child: Box::new(map_node(scope, child)),
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
            header: Box::new(map_node(scope, header)),
            children: children
                .iter()
                .map(|child| map_node(scope, child))
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

            preem_render::map_widget(scope, id.as_deref(), classes, &widget)
        }
    }
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
