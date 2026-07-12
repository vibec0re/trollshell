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
//! # Sizing
//!
//! The natural size is the buffer's pixel dimensions, with a minimum of 0 so
//! CSS/layout can size the widget up freely (small buffer, big widget). The
//! buffer is then drawn to fill the allocation, scaled nearest-neighbor.

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

mod imp {
    use super::{glib, graphene, gsk, rgba_len_ok};
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
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let natural = if orientation == gtk::Orientation::Horizontal {
                self.nat_width.get()
            } else {
                self.nat_height.get()
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
            let w = crate::cast::i32_to_f32(widget.width());
            let h = crate::cast::i32_to_f32(widget.height());
            if w <= 0.0 || h <= 0.0 {
                return;
            }
            let bounds = graphene::Rect::new(0.0, 0.0, w, h);
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
    use super::rgba_len_ok;

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
}
