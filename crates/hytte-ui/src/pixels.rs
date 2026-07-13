//! `PixelSurface` — a raster widget that scales a small RGBA8 buffer up with
//! **nearest-neighbor** filtering, for the plugin reconciler's
//! [`Node::Pixels`](crate::widget_tree::Node::Pixels).
//!
//! A plain [`gtk::Picture`] / [`gtk::Image`] filters linearly and blurs chunky
//! pixels into mush; the LCD look requires crisp, hard pixel edges. So this is a
//! minimal `gtk::Widget` subclass whose `snapshot` vfunc uploads the buffer as a
//! [`gdk::MemoryTexture`] and paints it with
//! [`gsk::ScalingFilter::Nearest`](gtk::gsk::ScalingFilter::Nearest) via
//! [`append_scaled_texture`](gtk::prelude::SnapshotExt::append_scaled_texture).
//! (It's the only subclass in `hytte-ui` — the rest of the crate uses
//! `DrawingArea` + cairo, but cairo can't paint a `GdkTexture` with a chosen
//! scaling filter, and the RGBA8 → premultiplied-BGRA conversion cairo would
//! need is exactly what `MemoryTexture` avoids.)
//!
//! # Buffer contract
//!
//! [`set_pixels`](PixelSurface::set_pixels) takes a `width`×`height` block of
//! RGBA8 (row-major, 4 bytes/pixel `[R, G, B, A]`, non-premultiplied). Its
//! length MUST equal `width * height * 4`. The widget is a **defensive
//! backstop**: given any inconsistent buffer (wrong length, zero dimension,
//! dimensions that overflow `i32`) it renders **nothing** rather than handing
//! `gdk::MemoryTexture::new` an under-sized buffer (which trips a GTK
//! `g_return_if_fail`). It does **not** log — the trust boundary that validates
//! untrusted plugin buffers and warns is the host's `to_ui_node`, upstream.
//!
//! # Sizing — aspect-ratio locked
//!
//! The buffer's pixel dimensions carry an aspect ratio, and the widget honors
//! it on **both** ends so a `128×128` LCD never renders stretched in a wide card
//! (issue #302):
//!
//! - **Geometry.** The widget is [`HeightForWidth`](gtk::SizeRequestMode): its
//!   natural width is the buffer width, and `measure` for the height returns
//!   `for_width * buf_h / buf_w` — so layout itself requests the right *shape*.
//!   The minimum stays 0 on both axes, so CSS/layout can still scale the widget
//!   up freely (small buffer, big widget — the LCD look).
//! - **Draw.** As a backstop against a CSS-forced wrong-shape allocation, the
//!   texture is drawn into the largest buffer-aspect rect that fits the
//!   allocation, centered ([`fit_rect`]) — letterboxing (padding) rather than
//!   distorting. Still nearest-neighbor.

use gtk::glib;
use gtk::graphene;
use gtk::gsk;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

/// Whether `data_len` is exactly `width * height * 4` (RGBA8), computed in
/// `u64` so no intermediate product can overflow.
fn rgba_len_ok(width: u32, height: u32, data_len: usize) -> bool {
    let expected = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|n| n.checked_mul(4));
    expected == u64::try_from(data_len).ok()
}

/// The aspect-locked height a `buf_w`×`buf_h` buffer wants at a proposed width
/// of `for_width` pixels: `for_width * buf_h / buf_w`, the height-for-width
/// request. Computed in `i64` so the intermediate product can't overflow, and
/// guarded so any non-positive input (an unconstrained `for_width == -1`, or an
/// empty buffer) yields `0` — the caller then falls back to the natural height.
fn scaled_height(for_width: i32, buf_w: i32, buf_h: i32) -> i32 {
    if for_width <= 0 || buf_w <= 0 || buf_h <= 0 {
        return 0;
    }
    let h = i64::from(for_width) * i64::from(buf_h) / i64::from(buf_w);
    i32::try_from(h).unwrap_or(i32::MAX)
}

/// The largest rect preserving the buffer's aspect ratio (`buf_w:buf_h`) that
/// fits inside an `alloc_w`×`alloc_h` allocation, centered — the letterbox
/// backstop for `snapshot`. Returns `(x, y, w, h)`; a degenerate input (any
/// dimension `<= 0`) yields a zero rect so the caller draws nothing.
fn fit_rect(alloc_w: f32, alloc_h: f32, buf_w: f32, buf_h: f32) -> (f32, f32, f32, f32) {
    if alloc_w <= 0.0 || alloc_h <= 0.0 || buf_w <= 0.0 || buf_h <= 0.0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    // Scale to the tighter axis so the whole buffer fits; the slack on the other
    // axis becomes the (centered) letterbox padding.
    let scale = (alloc_w / buf_w).min(alloc_h / buf_h);
    let w = buf_w * scale;
    let h = buf_h * scale;
    let x = (alloc_w - w) / 2.0;
    let y = (alloc_h - h) / 2.0;
    (x, y, w, h)
}

mod imp {
    use super::{fit_rect, glib, graphene, gsk, rgba_len_ok, scaled_height};
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    pub struct PixelSurface {
        /// The current texture, rebuilt whenever the buffer changes. `None`
        /// renders nothing (empty/degraded/invalid buffer).
        texture: RefCell<Option<gtk::gdk::MemoryTexture>>,
        /// Natural (unscaled) buffer size in pixels, honored by `measure`.
        nat_width: Cell<i32>,
        nat_height: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PixelSurface {
        const NAME: &'static str = "HyttePixelSurface";
        type Type = super::PixelSurface;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for PixelSurface {}

    impl WidgetImpl for PixelSurface {
        /// Height-for-width: the widget's height is a function of the width it is
        /// given, so layout requests the buffer's aspect ratio rather than a
        /// fixed box.
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let bw = self.nat_width.get();
            let bh = self.nat_height.get();
            let natural = if orientation == gtk::Orientation::Horizontal {
                // Width: the buffer's natural width.
                bw
            } else {
                // Height-for-width: at a known proposed width (`for_size > 0`),
                // request the aspect-locked height; with the width still
                // unconstrained (`for_size == -1`), fall back to the natural
                // buffer height.
                if for_size > 0 {
                    scaled_height(for_size, bw, bh)
                } else {
                    bh
                }
            };
            // (min, natural, min_baseline, natural_baseline). min = 0 lets CSS /
            // layout scale the widget above its buffer size (the LCD look).
            (0, natural, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let Some(texture) = self.texture.borrow().clone() else {
                return;
            };
            let widget = self.obj();
            let alloc_w = crate::cast::i32_to_f32(widget.width());
            let alloc_h = crate::cast::i32_to_f32(widget.height());
            let buf_w = crate::cast::i32_to_f32(self.nat_width.get());
            let buf_h = crate::cast::i32_to_f32(self.nat_height.get());
            // Letterbox: draw the texture into the largest buffer-aspect rect
            // that fits the allocation, centered — a CSS-forced wrong-shape
            // allocation then pads instead of distorting.
            let (x, y, w, h) = fit_rect(alloc_w, alloc_h, buf_w, buf_h);
            if w <= 0.0 || h <= 0.0 {
                return;
            }
            let bounds = graphene::Rect::new(x, y, w, h);
            snapshot.append_scaled_texture(&texture, gsk::ScalingFilter::Nearest, &bounds);
        }
    }

    impl PixelSurface {
        /// Swap in a new RGBA8 buffer, or clear to "render nothing" for any
        /// inconsistent input. Returns whether the natural size changed (so the
        /// caller can pick `queue_resize` vs `queue_draw`).
        pub(super) fn set_pixels(&self, width: u32, height: u32, data: &[u8]) -> bool {
            let old_w = self.nat_width.get();
            let old_h = self.nat_height.get();

            // Defensive: never build a texture from an inconsistent buffer.
            let texture = match (i32::try_from(width), i32::try_from(height)) {
                (Ok(w), Ok(h)) if w > 0 && h > 0 && rgba_len_ok(width, height, data.len()) => {
                    let bytes = glib::Bytes::from(data);
                    let stride = crate::cast::u32_to_usize(width) * 4;
                    self.nat_width.set(w);
                    self.nat_height.set(h);
                    Some(gtk::gdk::MemoryTexture::new(
                        w,
                        h,
                        gtk::gdk::MemoryFormat::R8g8b8a8,
                        &bytes,
                        stride,
                    ))
                }
                _ => {
                    // Empty / invalid: render nothing, reserve no space.
                    self.nat_width.set(0);
                    self.nat_height.set(0);
                    None
                }
            };
            self.texture.replace(texture);
            self.nat_width.get() != old_w || self.nat_height.get() != old_h
        }
    }
}

glib::wrapper! {
    /// A raster widget that paints a small RGBA8 buffer scaled up with
    /// nearest-neighbor filtering. See the [module docs](self).
    pub struct PixelSurface(ObjectSubclass<imp::PixelSurface>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl PixelSurface {
    /// Build an empty pixel surface (renders nothing until
    /// [`set_pixels`](Self::set_pixels)).
    #[must_use]
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// Replace the displayed buffer with `width`×`height` RGBA8 `data`
    /// (`width * height * 4` bytes; row-major, non-premultiplied). Any
    /// inconsistent buffer clears the surface to render nothing. Queues the
    /// minimal invalidation (`queue_resize` only when the natural size changed,
    /// otherwise `queue_draw`), so a same-size data swap is a cheap redraw.
    pub fn set_pixels(&self, width: u32, height: u32, data: &[u8]) {
        if self.imp().set_pixels(width, height, data) {
            self.queue_resize();
        } else {
            self.queue_draw();
        }
    }
}

impl Default for PixelSurface {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{fit_rect, rgba_len_ok, scaled_height};

    #[test]
    fn rgba_len_ok_matches_exact_product() {
        assert!(rgba_len_ok(2, 3, 24)); // 2*3*4
        assert!(rgba_len_ok(0, 0, 0)); // degenerate-but-consistent
        assert!(rgba_len_ok(1, 1, 4));
    }

    #[test]
    fn rgba_len_ok_rejects_mismatch() {
        assert!(!rgba_len_ok(2, 3, 23));
        assert!(!rgba_len_ok(2, 3, 25));
        assert!(!rgba_len_ok(4, 4, 0));
    }

    #[test]
    fn rgba_len_ok_no_overflow_on_huge_dims() {
        // width*height*4 overflows u32 but not u64; must not panic, must reject
        // a realistically-sized buffer against absurd dimensions.
        assert!(!rgba_len_ok(u32::MAX, u32::MAX, 16));
    }

    #[test]
    fn scaled_height_is_the_aspect_locked_height() {
        // Square buffer → square request.
        assert_eq!(scaled_height(200, 128, 128), 200);
        // 2:1 buffer at width 100 → height 50.
        assert_eq!(scaled_height(100, 4, 2), 50);
        // 1:2 buffer at width 100 → height 200.
        assert_eq!(scaled_height(100, 2, 4), 200);
    }

    #[test]
    fn scaled_height_guards_degenerate_inputs() {
        // Unconstrained width (GTK passes -1), and empty buffers, yield 0 so the
        // caller falls back to the natural height.
        assert_eq!(scaled_height(-1, 128, 128), 0);
        assert_eq!(scaled_height(0, 128, 128), 0);
        assert_eq!(scaled_height(200, 0, 0), 0);
        // A huge width can't overflow (i64 math), and never panics.
        assert!(scaled_height(i32::MAX, 1, 1) > 0);
    }

    #[test]
    fn fit_rect_fills_a_matching_allocation_exactly() {
        // Same aspect ratio → no letterbox: fills the whole allocation, origin 0.
        let (x, y, w, h) = fit_rect(256.0, 256.0, 128.0, 128.0);
        assert!((x - 0.0).abs() < 1e-3 && (y - 0.0).abs() < 1e-3);
        assert!((w - 256.0).abs() < 1e-3 && (h - 256.0).abs() < 1e-3);
    }

    #[test]
    fn fit_rect_letterboxes_a_too_wide_allocation() {
        // A square buffer in a 300×150 allocation: height-bound, so the square is
        // 150×150, centered horizontally (75px bars either side), y = 0.
        let (x, y, w, h) = fit_rect(300.0, 150.0, 128.0, 128.0);
        assert!((w - 150.0).abs() < 1e-3, "w = {w}");
        assert!((h - 150.0).abs() < 1e-3, "h = {h}");
        assert!((x - 75.0).abs() < 1e-3, "x = {x}");
        assert!((y - 0.0).abs() < 1e-3, "y = {y}");
    }

    #[test]
    fn fit_rect_letterboxes_a_too_tall_allocation() {
        // A square buffer in a 150×300 allocation: width-bound, 150×150, centered
        // vertically (75px bars top/bottom), x = 0.
        let (x, y, w, h) = fit_rect(150.0, 300.0, 128.0, 128.0);
        assert!((w - 150.0).abs() < 1e-3 && (h - 150.0).abs() < 1e-3);
        assert!((x - 0.0).abs() < 1e-3, "x = {x}");
        assert!((y - 75.0).abs() < 1e-3, "y = {y}");
    }

    #[test]
    fn fit_rect_zero_on_degenerate_input() {
        assert_eq!(fit_rect(0.0, 100.0, 128.0, 128.0), (0.0, 0.0, 0.0, 0.0));
        assert_eq!(fit_rect(100.0, 100.0, 0.0, 128.0), (0.0, 0.0, 0.0, 0.0));
        assert_eq!(fit_rect(-5.0, 100.0, 128.0, 128.0), (0.0, 0.0, 0.0, 0.0));
    }
}

// ── GTK integration tests (need a display → gated to `system-tests`) ─────────

#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use super::PixelSurface;
    use gtk::prelude::*;

    /// A 2×1 (RGBA8) buffer — an aspect ratio of 2:1 so the height request is a
    /// clean half of the width.
    fn wide_surface() -> PixelSurface {
        let s = PixelSurface::new();
        // 2 px wide, 1 px tall = 2*1*4 = 8 bytes.
        s.set_pixels(2, 1, &[0xff; 8]);
        s
    }

    #[gtk::test]
    fn measure_requests_the_buffer_aspect_height_for_a_width() {
        let s = wide_surface();
        // Height-for-width: at width 200 the 2:1 buffer wants height 100.
        let (_, nat_h, _, _) = s.measure(gtk::Orientation::Vertical, 200);
        assert_eq!(nat_h, 100, "a 2:1 buffer measured at width 200 is 100 tall");
        // Natural width is the buffer width; min is 0 so CSS can still shrink it.
        let (min_w, nat_w, _, _) = s.measure(gtk::Orientation::Horizontal, -1);
        assert_eq!((min_w, nat_w), (0, 2));
    }

    #[gtk::test]
    fn request_mode_is_height_for_width() {
        let s = wide_surface();
        assert_eq!(s.request_mode(), gtk::SizeRequestMode::HeightForWidth);
    }

    #[gtk::test]
    fn an_empty_surface_requests_nothing() {
        // No buffer set: natural size 0 on both axes, no aspect to lock.
        let s = PixelSurface::new();
        assert_eq!(s.measure(gtk::Orientation::Horizontal, -1).1, 0);
        assert_eq!(s.measure(gtk::Orientation::Vertical, 200).1, 0);
    }
}
