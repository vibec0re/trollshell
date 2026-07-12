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

/// A CSS class token, applied verbatim by the host (`ts-*` binary / `hytte-*`
/// library contract). Plugins style via these tokens, never raw CSS.
pub type Cls = String;

/// Orientation for a [`Node::Box`]. Mirrors `hytte_ui::Dir`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dir {
    Horizontal,
    Vertical,
}

/// A user interaction on a rendered node, addressed by [`NodeId`]. Mirrors
/// `hytte_ui::EventKind` exactly (v1: `Click` + `Scroll`; the reconciler ships
/// no `Hover`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum EventKind {
    /// A [`Node::Button`] was clicked.
    Click,
    /// A `scroll: true` [`Node::Box`] was scrolled; `dx`/`dy` are raw deltas.
    Scroll { dx: f64, dy: f64 },
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
    /// A `gtk::Label`.
    Label {
        id: Option<NodeId>,
        text: String,
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
    /// A `gtk::Revealer`; `open` drives `set_reveal_child`.
    Revealer {
        id: Option<NodeId>,
        open: bool,
        child: Box<Node>,
    },
    /// A `gtk::Separator`.
    Separator { classes: Vec<Cls> },
}
