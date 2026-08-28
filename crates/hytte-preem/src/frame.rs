//! The framebuffer core: [`Frame`], a CPU-side RGBA8 buffer with the small
//! set of clipped drawing primitives every raster widget needs.
//!
//! Promoted from the private helpers the pet's `face.rs` hand-rolled
//! (`plot`/`fill`/`hline`, #284) so plugins stop re-inventing them. All
//! primitives **clip silently**: a shape straddling (or entirely outside)
//! the buffer never panics and never wraps to the far edge — the same
//! contract the pet's originals kept.

use hytte_plugin_proto::{Cls, Node};

/// One RGBA8 pixel, `[r, g, b, a]`, straight (non-premultiplied) alpha —
/// exactly the byte layout [`Node::Pixels`] carries on the wire.
pub type Rgba = [u8; 4];

/// A `width`×`height` RGBA8 framebuffer whose byte layout is always valid
/// for [`Node::Pixels`]: row-major, 4 bytes per pixel,
/// `data.len() == width * height * 4` — the invariant the host validates.
/// Constructors establish it; nothing on the type can break it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    width: usize,
    height: usize,
    data: Vec<u8>,
}

impl Frame {
    /// A fully **transparent** frame (every byte zero).
    #[must_use]
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            data: vec![0u8; width * height * 4],
        }
    }

    /// A frame flooded with one color.
    #[must_use]
    pub fn filled(width: usize, height: usize, color: Rgba) -> Self {
        let mut f = Self::new(width, height);
        f.fill(color);
        f
    }

    /// Buffer width in pixels.
    #[must_use]
    pub fn width(&self) -> usize {
        self.width
    }

    /// Buffer height in pixels.
    #[must_use]
    pub fn height(&self) -> usize {
        self.height
    }

    /// The raw RGBA8 bytes (row-major, `width * height * 4` long).
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Set one pixel by unsigned coordinates, silently clipping
    /// out-of-bounds (the crate-internal fast path for loops that already
    /// work in `usize`).
    pub(crate) fn set(&mut self, x: usize, y: usize, color: Rgba) {
        if x >= self.width || y >= self.height {
            return;
        }
        let i = (y * self.width + x) * 4;
        self.data[i..i + 4].copy_from_slice(&color);
    }

    /// Read one pixel by unsigned coordinates; out-of-bounds reads come back
    /// fully transparent.
    pub(crate) fn at(&self, x: usize, y: usize) -> Rgba {
        if x >= self.width || y >= self.height {
            return [0, 0, 0, 0];
        }
        let i = (y * self.width + x) * 4;
        [
            self.data[i],
            self.data[i + 1],
            self.data[i + 2],
            self.data[i + 3],
        ]
    }

    /// Paint one pixel, silently clipping anything outside the buffer (so
    /// edge-straddling shapes never panic or wrap).
    pub fn plot(&mut self, x: i32, y: i32, color: Rgba) {
        let (Ok(px), Ok(py)) = (usize::try_from(x), usize::try_from(y)) else {
            return;
        };
        self.set(px, py, color);
    }

    /// Read one pixel, or `None` outside the buffer.
    #[must_use]
    pub fn get(&self, x: i32, y: i32) -> Option<Rgba> {
        let (Ok(px), Ok(py)) = (usize::try_from(x), usize::try_from(y)) else {
            return None;
        };
        if px >= self.width || py >= self.height {
            return None;
        }
        Some(self.at(px, py))
    }

    /// Flood the whole buffer with one color.
    pub fn fill(&mut self, color: Rgba) {
        for px in self.data.chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
    }

    /// Horizontal run from `x0` to `x1` (either order, inclusive of both
    /// ends) at row `y`.
    pub fn hline(&mut self, x0: i32, x1: i32, y: i32, color: Rgba) {
        for x in x0.min(x1)..=x0.max(x1) {
            self.plot(x, y, color);
        }
    }

    /// Vertical run from `y0` to `y1` (either order, inclusive of both ends)
    /// at column `x`.
    pub fn vline(&mut self, x: i32, y0: i32, y1: i32, color: Rgba) {
        for y in y0.min(y1)..=y0.max(y1) {
            self.plot(x, y, color);
        }
    }

    /// A filled `w`×`h` rectangle with its top-left corner at (`x`, `y`).
    /// Non-positive `w`/`h` draws nothing.
    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Rgba) {
        if w <= 0 || h <= 0 {
            return;
        }
        for yy in y..y.saturating_add(h) {
            self.hline(x, x.saturating_add(w) - 1, yy, color);
        }
    }

    /// Blit `src` with its top-left corner at (`x`, `y`): a plain sprite
    /// copy, no blending — each source pixel **replaces** the destination
    /// unless it is fully transparent (`a == 0`), which is skipped so a
    /// sprite's cut-out corners keep the backdrop. Clips like everything
    /// else.
    pub fn blit(&mut self, src: &Frame, x: i32, y: i32) {
        for sy in 0..src.height {
            for sx in 0..src.width {
                let px = src.at(sx, sy);
                if px[3] == 0 {
                    continue;
                }
                // i64 keeps a negative origin exact; a negative destination
                // fails the usize conversion and clips, same as `plot`.
                let dx = i64::from(x) + i64::try_from(sx).unwrap_or(i64::MAX);
                let dy = i64::from(y) + i64::try_from(sy).unwrap_or(i64::MAX);
                if let (Ok(dx), Ok(dy)) = (usize::try_from(dx), usize::try_from(dy)) {
                    self.set(dx, dy, px);
                }
            }
        }
    }

    /// Nearest-neighbor upscale by an integer `factor`, replicating each
    /// source pixel into a `factor`×`factor` block — how a kit widget bakes
    /// chunkiness into the buffer itself (see the module docs on sizing).
    /// `factor <= 1` returns an unchanged copy. Preserves the
    /// `len == w * h * 4` invariant and every exact pixel value.
    #[must_use]
    pub fn upscale(&self, factor: usize) -> Frame {
        if factor <= 1 {
            return self.clone();
        }
        let mut dst = Frame::new(self.width * factor, self.height * factor);
        for y in 0..self.height {
            for x in 0..self.width {
                let px = self.at(x, y);
                for sy in 0..factor {
                    for sx in 0..factor {
                        dst.set(x * factor + sx, y * factor + sy, px);
                    }
                }
            }
        }
        dst
    }

    /// Wrap the buffer into the [`Node::Pixels`] the host reconciles. Give
    /// an `id` when the widget re-renders every frame — a same-id re-render
    /// swaps the texture in place instead of rebuilding the widget.
    #[must_use]
    pub fn into_node(self, id: Option<&str>, classes: Vec<Cls>) -> Node {
        Node::Pixels {
            id: id.map(str::to_owned),
            width: u32::try_from(self.width).unwrap_or(0),
            height: u32::try_from(self.height).unwrap_or(0),
            data: self.data,
            // 1×: the preem kit bakes its own integer upscale into the buffer
            // (`Frame::upscale` / a widget's `scale` knob), so the buffer is
            // already at final resolution; the #358 proto hint is for surfaces
            // that instead ship a base-resolution buffer for the host to scale.
            scale: 1,
            classes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Frame, Rgba};
    use hytte_plugin_proto::Node;

    const RED: Rgba = [0xff, 0, 0, 0xff];
    const BLUE: Rgba = [0, 0, 0xff, 0xff];

    /// The wire invariant the host enforces: `len == width * height * 4`.
    #[test]
    fn buffers_satisfy_the_host_invariant() {
        for (w, h) in [(0, 0), (1, 1), (7, 3), (128, 128)] {
            let f = Frame::new(w, h);
            assert_eq!(f.data().len(), w * h * 4, "{w}x{h}");
            let f = Frame::filled(w, h, RED);
            assert_eq!(f.data().len(), w * h * 4, "{w}x{h} filled");
        }
    }

    #[test]
    fn plot_writes_and_clips() {
        let mut f = Frame::new(4, 4);
        f.plot(1, 2, RED);
        assert_eq!(f.get(1, 2), Some(RED));
        // Out-of-bounds plots are silent no-ops, not panics or wraps.
        f.plot(-1, 0, BLUE);
        f.plot(0, -1, BLUE);
        f.plot(4, 0, BLUE);
        f.plot(0, 4, BLUE);
        f.plot(i32::MIN, i32::MAX, BLUE);
        assert_eq!(f.get(0, 0), Some([0, 0, 0, 0]));
        assert_eq!(f.get(3, 0), Some([0, 0, 0, 0]));
        assert_eq!(f.get(-1, 0), None);
        assert_eq!(f.get(4, 0), None);
    }

    #[test]
    fn fill_and_lines_paint_expected_pixels() {
        let mut f = Frame::new(5, 5);
        f.fill(BLUE);
        assert!(f.data().chunks_exact(4).all(|px| px == BLUE));
        f.hline(4, 0, 2, RED); // reversed ends still draw
        assert!((0..5).all(|x| f.get(x, 2) == Some(RED)));
        f.vline(1, 3, 1, RED);
        assert_eq!(f.get(1, 1), Some(RED));
        assert_eq!(f.get(1, 3), Some(RED));
        // Off-buffer runs clip.
        f.hline(-10, 10, -1, RED);
        f.vline(-1, -10, 10, RED);
        assert_eq!(f.get(0, 0), Some(BLUE));
    }

    #[test]
    fn rect_fills_and_rejects_degenerate_sizes() {
        let mut f = Frame::new(6, 6);
        f.rect(1, 1, 3, 2, RED);
        for y in 0..6_i32 {
            for x in 0..6_i32 {
                let inside = (1..4).contains(&x) && (1..3).contains(&y);
                let want = if inside { RED } else { [0, 0, 0, 0] };
                assert_eq!(f.get(x, y), Some(want), "({x},{y})");
            }
        }
        let before = f.clone();
        f.rect(2, 2, 0, 5, BLUE);
        f.rect(2, 2, 5, -1, BLUE);
        assert_eq!(f, before, "degenerate rects draw nothing");
    }

    #[test]
    fn blit_copies_opaque_skips_transparent_and_clips() {
        let mut sprite = Frame::new(2, 2);
        sprite.plot(0, 0, RED);
        // (1, 1) stays transparent — it must not punch a hole.
        sprite.plot(1, 0, BLUE);
        let mut f = Frame::filled(4, 4, [9, 9, 9, 0xff]);
        f.blit(&sprite, 1, 1);
        assert_eq!(f.get(1, 1), Some(RED));
        assert_eq!(f.get(2, 1), Some(BLUE));
        assert_eq!(f.get(2, 2), Some([9, 9, 9, 0xff]), "transparent px skipped");
        // Straddling every edge (including a negative origin) never panics,
        // and the in-bounds part still lands.
        f.blit(&sprite, -1, -1);
        assert_eq!(f.get(0, 0), Some([9, 9, 9, 0xff]), "clipped px absent");
        f.blit(&sprite, 3, 3);
        assert_eq!(f.get(3, 3), Some(RED));
    }

    #[test]
    fn blit_negative_origin_keeps_in_bounds_pixels() {
        let sprite = Frame::filled(3, 3, RED);
        let mut f = Frame::new(4, 4);
        f.blit(&sprite, -1, -2);
        assert_eq!(f.get(0, 0), Some(RED), "sprite row 2 lands at y 0");
        assert_eq!(f.get(1, 0), Some(RED));
        assert_eq!(f.get(2, 0), Some([0, 0, 0, 0]), "sprite width respected");
    }

    #[test]
    fn upscale_replicates_pixels_and_keeps_the_invariant() {
        let mut f = Frame::new(2, 1);
        f.plot(0, 0, RED);
        f.plot(1, 0, BLUE);
        let up = f.upscale(3);
        assert_eq!((up.width(), up.height()), (6, 3));
        assert_eq!(up.data().len(), 6 * 3 * 4);
        for y in 0..3_i32 {
            for x in 0..3_i32 {
                assert_eq!(up.get(x, y), Some(RED));
                assert_eq!(up.get(x + 3, y), Some(BLUE));
            }
        }
        // Factor 0/1 are plain copies.
        assert_eq!(f.upscale(1), f);
        assert_eq!(f.upscale(0), f);
    }

    #[test]
    fn into_node_carries_the_buffer_verbatim() {
        let f = Frame::filled(3, 2, RED);
        let bytes = f.data().to_vec();
        let Node::Pixels {
            id,
            width,
            height,
            data,
            scale,
            classes,
        } = f.into_node(Some("x"), vec!["cls".to_owned()])
        else {
            panic!("into_node builds a Pixels node");
        };
        assert_eq!(id.as_deref(), Some("x"));
        assert_eq!((width, height), (3, 2));
        assert_eq!(data, bytes);
        assert_eq!(data.len(), 3 * 2 * 4);
        // Preem bakes its own upscale into the buffer, so the proto hint is 1×.
        assert_eq!(scale, 1);
        assert_eq!(classes, vec!["cls".to_owned()]);
    }
}
