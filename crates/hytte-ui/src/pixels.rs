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
//! # Equality guard — an identical frame costs nothing (#902)
//!
//! [`set_pixels`](PixelSurface::set_pixels) is called from the plugin
//! reconciler on **every** re-map of a `Node::Pixels`, and the shell's preem
//! animation clock re-maps a whole mailbox whenever anything in it moves — so an
//! unchanged chip sitting next to an animating one used to rebuild a
//! [`glib::Bytes`] + [`gdk::MemoryTexture`](gtk::gdk::MemoryTexture) and
//! `queue_draw` 20× a second for a picture nobody changed.
//!
//! The surface therefore keeps the last **accepted** buffer and compares before
//! it uploads. *Equal* means, exactly:
//!
//! - the same `width` **and** the same `height` (compared first, so a reshape
//!   can never be mistaken for a no-op even when the byte block is identical —
//!   `2×1` and `1×2` are both 8 bytes), **and**
//! - the same bytes (`Arc::ptr_eq` short-circuits the shared path below, then a
//!   full slice compare; a pointer match implies byte equality because the
//!   buffer is immutable while the surface holds a reference to it).
//!
//! On equality `set_pixels` returns without touching GTK at all — no `Bytes`, no
//! texture, no `queue_draw`. An **inconsistent** buffer (see the contract above)
//! is never remembered, so it can never compare equal to anything: the cheap
//! rejection just re-runs.
//!
//! The [`set_scale`](PixelSurface::set_scale) hint is guarded separately and has
//! been since #358 — it only queues a resize when the *effective* scale changes.
//!
//! ## Sharing a buffer instead of cloning it
//!
//! [`set_pixels_shared`](PixelSurface::set_pixels_shared) takes an
//! `Arc<[u8]>` rather than a slice. A caller with a frame cache (the shell's
//! preem renderer, one cache feeding N monitors) can hand the *same* allocation
//! to every surface instead of a full RGBA clone each: the guard then costs one
//! pointer compare, and on a real change the `MemoryTexture` adopts the `Arc`
//! through [`glib::Bytes::from_owned`] with **no copy at all**. The slice
//! setter still works and still copies exactly once, as before.
//!
//! # Sizing — aspect-ratio locked, integer-scalable
//!
//! The buffer's pixel dimensions carry an aspect ratio, and the widget honors
//! it on **both** ends so a `128×128` LCD never renders stretched in a wide card
//! (issue #302):
//!
//! - **Geometry.** The widget is [`HeightForWidth`](gtk::SizeRequestMode): its
//!   natural width is the buffer width times the [`set_scale`](PixelSurface::set_scale)
//!   factor (#358 — a crisp integer blow-up without a shell CSS px rule), and
//!   `measure` for the height returns `for_width * buf_h / buf_w` — so layout
//!   itself requests the right *shape*. The minimum stays 0 on both axes, so
//!   CSS/layout can still scale the widget up freely (small buffer, big widget —
//!   the LCD look).
//! - **Draw.** As a backstop against a CSS-forced wrong-shape allocation, the
//!   texture is drawn into the largest buffer-aspect rect that fits the
//!   allocation, centered ([`fit_rect`]) — letterboxing (padding) rather than
//!   distorting. Still nearest-neighbor.

use gtk::glib;
use gtk::graphene;
use gtk::gsk;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use std::sync::Arc;

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

/// A buffer dimension times the widget's integer upscale factor, saturated to
/// `i32::MAX` (i64 math, so the product can't overflow) — the natural size one
/// axis requests. A `scale` of `0` is treated as `1` (the widget's "no scale"
/// default), so the natural size never collapses to zero on a degenerate input.
fn scaled_nat(dim: i32, scale: u32) -> i32 {
    let n = i64::from(dim) * i64::from(scale.max(1));
    i32::try_from(n).unwrap_or(i32::MAX)
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
    use super::{fit_rect, glib, graphene, gsk, rgba_len_ok, scaled_height, scaled_nat};
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use std::cell::{Cell, RefCell};
    use std::sync::Arc;

    /// What a `set_pixels*` call needs from GTK once it has updated the state.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(super) enum Invalidate {
        /// The surface already held exactly this frame: nothing was rebuilt and
        /// nothing needs repainting (the #902 guard).
        Nothing,
        /// New pixels at the same natural size — a cheap repaint.
        Draw,
        /// The natural size changed, so layout has to run again.
        Resize,
    }

    /// The last **accepted** frame, kept so the next `set_pixels*` call can be
    /// compared against it (#902).
    struct Held {
        width: u32,
        height: u32,
        /// The *same* allocation the live `texture` reads (the `glib::Bytes`
        /// handed to `MemoryTexture::new` is [`glib::Bytes::from_owned`] over
        /// this very `Arc`), so remembering it costs a refcount, not a copy.
        data: Arc<[u8]>,
    }

    impl Held {
        /// Whether this is the frame being handed in. Dimensions first: a
        /// reshape must never look like a no-op even when the byte block is
        /// identical (`2×1` and `1×2` are both 8 bytes).
        fn is(&self, width: u32, height: u32, data: &[u8]) -> bool {
            self.width == width && self.height == height && *self.data == *data
        }

        /// [`Held::is`] for a shared buffer. Same allocation ⇒ same bytes: an
        /// `Arc<[u8]>` the surface holds a reference to can't be mutated behind
        /// its back, so the pointer check is a sound short-circuit. The slice
        /// compare is the fallback for two equal-but-distinct buffers.
        fn is_shared(&self, width: u32, height: u32, data: &Arc<[u8]>) -> bool {
            if Arc::ptr_eq(&self.data, data) {
                return self.width == width && self.height == height;
            }
            self.is(width, height, data)
        }
    }

    #[derive(Default)]
    pub struct PixelSurface {
        /// The current texture, rebuilt whenever the buffer changes. `None`
        /// renders nothing (empty/degraded/invalid buffer).
        texture: RefCell<Option<gtk::gdk::MemoryTexture>>,
        /// The frame the surface is currently showing, for the #902 guard.
        /// `None` means it has never held a valid buffer, or the last call was
        /// inconsistent and cleared it; either way the next call can never
        /// compare equal.
        held: RefCell<Option<Held>>,
        /// Natural (unscaled) buffer size in pixels, honored by `measure`.
        nat_width: Cell<i32>,
        nat_height: Cell<i32>,
        /// Integer upscale factor applied to the natural size (#358). The
        /// `Default`-derived `0` is treated as `1` everywhere (see
        /// [`scaled_nat`]), so a freshly-built surface measures at 1×.
        scale: Cell<u32>,
        /// Test seam (#902): how many `MemoryTexture`s this surface has built,
        /// and how many invalidations it has asked GTK for. Compiled out
        /// entirely outside `cargo test`.
        #[cfg(all(test, feature = "system-tests"))]
        counts: Cell<super::Counts>,
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
            let scale = self.scale.get();
            let natural = if orientation == gtk::Orientation::Horizontal {
                // Width: the buffer's natural width × the integer upscale.
                scaled_nat(bw, scale)
            } else {
                // Height-for-width: at a known proposed width (`for_size > 0`),
                // request the aspect-locked height (scale-invariant — both axes
                // multiply by the same factor, so the ratio is unchanged); with
                // the width still unconstrained (`for_size == -1`), fall back to
                // the natural buffer height × the upscale.
                if for_size > 0 {
                    scaled_height(for_size, bw, bh)
                } else {
                    scaled_nat(bh, scale)
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

    /// The dimensions as `i32` if this `(width, height, len)` triple describes a
    /// texture that can actually be built: both axes positive, in range, and
    /// `len == width * height * 4`. `None` is the "render nothing" backstop.
    ///
    /// Checked *before* any allocation, so an inconsistent buffer never costs a
    /// copy — a misbehaving plugin can't make the reject path expensive.
    fn buildable(width: u32, height: u32, len: usize) -> Option<(i32, i32)> {
        match (i32::try_from(width), i32::try_from(height)) {
            (Ok(w), Ok(h)) if w > 0 && h > 0 && rgba_len_ok(width, height, len) => Some((w, h)),
            _ => None,
        }
    }

    impl PixelSurface {
        /// Swap in a new RGBA8 buffer from a slice, copying it exactly once into
        /// a shared allocation. Returns nothing to do when the surface already
        /// holds this very frame (#902).
        pub(super) fn set_pixels(&self, width: u32, height: u32, data: &[u8]) -> Invalidate {
            let unchanged = self
                .held
                .borrow()
                .as_ref()
                .is_some_and(|held| held.is(width, height, data));
            if unchanged {
                return Invalidate::Nothing;
            }
            self.replace(width, height, || Arc::from(data), data.len())
        }

        /// Swap in a caller-owned shared buffer — no RGBA copy on either the
        /// guard's fast path (a pointer compare) or the upload (`Bytes` adopts
        /// the `Arc`).
        pub(super) fn set_pixels_shared(
            &self,
            width: u32,
            height: u32,
            data: &Arc<[u8]>,
        ) -> Invalidate {
            let unchanged = self
                .held
                .borrow()
                .as_ref()
                .is_some_and(|held| held.is_shared(width, height, data));
            if unchanged {
                return Invalidate::Nothing;
            }
            self.replace(width, height, || Arc::clone(data), data.len())
        }

        /// The shared tail of both setters: validate, then either upload a new
        /// texture or clear to "render nothing", and report what GTK owes us.
        ///
        /// `data` is a thunk so the `Arc` is only materialised once the buffer
        /// is known to be consistent — the slice setter's single copy happens
        /// here, and not at all on the reject path.
        fn replace(
            &self,
            width: u32,
            height: u32,
            data: impl FnOnce() -> Arc<[u8]>,
            len: usize,
        ) -> Invalidate {
            let old_w = self.nat_width.get();
            let old_h = self.nat_height.get();

            // Defensive: never build a texture from an inconsistent buffer.
            let texture = if let Some((w, h)) = buildable(width, height, len) {
                let data = data();
                // Zero-copy: GBytes takes a reference on the Arc rather than
                // duplicating the RGBA block, and `held` keeps the same one.
                let bytes = glib::Bytes::from_owned(Arc::clone(&data));
                let stride = crate::cast::u32_to_usize(width) * 4;
                self.nat_width.set(w);
                self.nat_height.set(h);
                self.held.replace(Some(Held {
                    width,
                    height,
                    data,
                }));
                #[cfg(all(test, feature = "system-tests"))]
                self.counts.set(self.counts.get().with_build());
                Some(gtk::gdk::MemoryTexture::new(
                    w,
                    h,
                    gtk::gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    stride,
                ))
            } else {
                // Empty / invalid: render nothing, reserve no space. Forget the
                // buffer too — an inconsistent frame was never displayed, so it
                // must never let a later identical one short-circuit.
                self.nat_width.set(0);
                self.nat_height.set(0);
                self.held.replace(None);
                None
            };
            self.texture.replace(texture);
            let invalidate = if self.nat_width.get() == old_w && self.nat_height.get() == old_h {
                Invalidate::Draw
            } else {
                Invalidate::Resize
            };
            #[cfg(all(test, feature = "system-tests"))]
            self.counts
                .set(self.counts.get().with_invalidation(invalidate));
            invalidate
        }

        /// Set the integer upscale factor. Returns whether the *effective*
        /// scale changed (`0` and `1` are the same 1× — see [`scaled_nat`]), so
        /// the caller only queues a resize for a real change.
        pub(super) fn set_scale(&self, scale: u32) -> bool {
            let old = self.scale.replace(scale);
            old.max(1) != scale.max(1)
        }

        /// Test seam (#902): the running `(builds, draws, resizes)` tally.
        #[cfg(all(test, feature = "system-tests"))]
        pub(super) fn counts(&self) -> super::Counts {
            self.counts.get()
        }
    }
}

/// Test seam (#902): how much work a sequence of `set_pixels*` calls actually
/// cost. `builds` counts `MemoryTexture` constructions, `draws`/`resizes` the
/// invalidations queued on GTK — a guarded call increments none of the three.
#[cfg(all(test, feature = "system-tests"))]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Counts {
    builds: u32,
    draws: u32,
    resizes: u32,
}

#[cfg(all(test, feature = "system-tests"))]
impl Counts {
    fn with_build(mut self) -> Self {
        self.builds += 1;
        self
    }

    fn with_invalidation(mut self, what: imp::Invalidate) -> Self {
        match what {
            imp::Invalidate::Nothing => {}
            imp::Invalidate::Draw => self.draws += 1,
            imp::Invalidate::Resize => self.resizes += 1,
        }
        self
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
    ///
    /// **A frame identical to the one already displayed does nothing at all**
    /// (#902) — no `Bytes`, no texture, no `queue_draw`. See the
    /// [module docs](self) for what "identical" means. `data` is copied once
    /// into a shared
    /// allocation; a caller that already has one should use
    /// [`set_pixels_shared`](Self::set_pixels_shared) instead.
    pub fn set_pixels(&self, width: u32, height: u32, data: &[u8]) {
        self.invalidate(self.imp().set_pixels(width, height, data));
    }

    /// [`set_pixels`](Self::set_pixels) over a buffer the caller already holds
    /// in an [`Arc`], with the same contract and the same equality guard — but
    /// **no RGBA copy on any path**: the guard's common case is an
    /// `Arc::ptr_eq`, and a real change hands the allocation straight to
    /// `MemoryTexture` via [`glib::Bytes::from_owned`].
    ///
    /// This is the setter for a caller with a frame cache feeding several
    /// surfaces — the shell's preem renderer rasterises once per tick and shows
    /// the result on every monitor, which is a full RGBA clone per monitor per
    /// tick through the slice setter and a refcount bump through this one.
    /// (Adopting it there is a follow-up; the slice path is unchanged and stays
    /// the default.)
    ///
    /// The buffer must be treated as immutable — which an `Arc<[u8]>` the
    /// surface holds a reference to already is, since `Arc::get_mut` can't
    /// hand out a `&mut` while this surface is a second owner.
    pub fn set_pixels_shared(&self, width: u32, height: u32, data: &Arc<[u8]>) {
        self.invalidate(self.imp().set_pixels_shared(width, height, data));
    }

    /// Ask GTK for exactly the invalidation the state change earned — and for
    /// [`Invalidate::Nothing`](imp::Invalidate::Nothing), the #902 guard's
    /// verdict, ask for none.
    fn invalidate(&self, what: imp::Invalidate) {
        match what {
            imp::Invalidate::Nothing => {}
            imp::Invalidate::Draw => self.queue_draw(),
            imp::Invalidate::Resize => self.queue_resize(),
        }
    }

    /// Set the integer upscale factor (#358): the widget's natural size becomes
    /// the buffer size times `scale` (`128×128` at `2` requests 256px), so a
    /// small buffer can ask for a crisp integer blow-up without a CSS px rule.
    /// `0` is treated as `1` (the default, the buffer's natural size). The draw
    /// itself is unchanged — still nearest-neighbor into whatever the final
    /// allocation is; only the *requested* size scales. Queues a resize only
    /// when the effective scale actually changed.
    pub fn set_scale(&self, scale: u32) {
        if self.imp().set_scale(scale) {
            self.queue_resize();
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
    use super::{fit_rect, rgba_len_ok, scaled_height, scaled_nat};

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

    #[test]
    fn scaled_nat_multiplies_the_natural_dimension() {
        // The caw lesson: 128px buffer at 2× requests exactly 256px.
        assert_eq!(scaled_nat(128, 2), 256);
        assert_eq!(scaled_nat(2, 3), 6);
    }

    #[test]
    fn scaled_nat_zero_and_one_are_identity() {
        // `0` (the Cell default before any set_scale) and `1` both mean 1×.
        assert_eq!(scaled_nat(128, 0), 128);
        assert_eq!(scaled_nat(128, 1), 128);
        // An empty buffer stays empty at any scale.
        assert_eq!(scaled_nat(0, 5), 0);
    }

    #[test]
    fn scaled_nat_saturates_instead_of_overflowing() {
        // i32::MAX × u32::MAX overflows i32 but not i64; must saturate, never
        // panic or wrap.
        assert_eq!(scaled_nat(i32::MAX, u32::MAX), i32::MAX);
    }
}

// ── GTK integration tests (need a display → gated to `system-tests`) ─────────

#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use super::{Counts, PixelSurface};
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use std::sync::Arc;

    /// A 2×1 (RGBA8) buffer — an aspect ratio of 2:1 so the height request is a
    /// clean half of the width.
    fn wide_surface() -> PixelSurface {
        let s = PixelSurface::new();
        // 2 px wide, 1 px tall = 2*1*4 = 8 bytes.
        s.set_pixels(2, 1, &[0xff; 8]);
        s
    }

    /// The surface's `(builds, draws, resizes)` tally — the #902 test seam.
    fn counts(s: &PixelSurface) -> Counts {
        s.imp().counts()
    }

    /// A `Counts` literal, so the assertions below read as a budget.
    fn spent(builds: u32, draws: u32, resizes: u32) -> Counts {
        Counts {
            builds,
            draws,
            resizes,
        }
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

    #[gtk::test]
    fn set_scale_multiplies_the_natural_request() {
        let s = wide_surface(); // 2×1 buffer
        s.set_scale(3);
        // Natural size is the buffer × scale on both axes…
        assert_eq!(s.measure(gtk::Orientation::Horizontal, -1).1, 6);
        assert_eq!(s.measure(gtk::Orientation::Vertical, -1).1, 3);
        // …while the height-for-width aspect lock is scale-invariant (2:1 at
        // width 200 is still 100 tall), and the minimum stays 0 so CSS/layout
        // can shrink below the scaled natural.
        assert_eq!(s.measure(gtk::Orientation::Vertical, 200).1, 100);
        assert_eq!(s.measure(gtk::Orientation::Horizontal, -1).0, 0);
        // Back to 1× (and its 0 alias) restores the buffer's natural size.
        s.set_scale(1);
        assert_eq!(s.measure(gtk::Orientation::Horizontal, -1).1, 2);
        s.set_scale(0);
        assert_eq!(s.measure(gtk::Orientation::Horizontal, -1).1, 2);
    }

    // ── the #902 equality guard ─────────────────────────────────────────────

    /// (a) The headline: handing the surface the frame it already displays is
    /// free — no texture rebuilt, nothing queued on GTK. The animation clock
    /// re-maps a whole mailbox when anything in it moves, so every static
    /// `Pixels` chip in that mailbox lands here 20× a second.
    #[gtk::test]
    fn an_identical_frame_builds_nothing_and_queues_nothing() {
        let s = PixelSurface::new();
        // First frame: one build, and a resize because 0×0 → 2×1.
        s.set_pixels(2, 1, &[0xff; 8]);
        assert_eq!(counts(&s), spent(1, 0, 1));

        // A genuinely new frame at the same size: one more build, one draw.
        s.set_pixels(2, 1, &[0x11; 8]);
        assert_eq!(counts(&s), spent(2, 1, 1));

        // The same bytes again — twice, to show it is a steady state and not a
        // one-shot latch.
        s.set_pixels(2, 1, &[0x11; 8]);
        s.set_pixels(2, 1, &[0x11; 8]);
        assert_eq!(
            counts(&s),
            spent(2, 1, 1),
            "an unchanged frame must cost no texture build and no invalidation",
        );
        // …and the surface still displays it (the guard is a skip, not a clear).
        assert_eq!(s.measure(gtk::Orientation::Vertical, 200).1, 100);
    }

    /// (b) Equal bytes at different dimensions are a different picture: `2×1`
    /// and `1×2` are both 8 bytes, and confusing them would freeze a reshaping
    /// widget on its old aspect ratio.
    #[gtk::test]
    fn a_reshape_is_never_equal_even_with_identical_bytes() {
        let s = PixelSurface::new();
        s.set_pixels(2, 1, &[0xff; 8]);
        s.set_pixels(1, 2, &[0xff; 8]);
        assert_eq!(
            counts(&s).builds,
            2,
            "same bytes, transposed dimensions: the texture must be rebuilt",
        );
        // Both dimensions really did change, not just the bookkeeping.
        assert_eq!(s.measure(gtk::Orientation::Horizontal, -1).1, 1);
        assert_eq!(s.measure(gtk::Orientation::Vertical, 100).1, 200);
    }

    /// (c) Different bytes at the same size still upload — the guard must not
    /// swallow a real frame.
    #[gtk::test]
    fn different_bytes_at_the_same_size_still_build() {
        let s = PixelSurface::new();
        s.set_pixels(2, 1, &[0xff; 8]);
        s.set_pixels(2, 1, &[0xfe; 8]);
        assert_eq!(counts(&s), spent(2, 1, 1));
        // One byte apart is still a different frame.
        let mut nearly = [0xfe_u8; 8];
        nearly[7] = 0x00;
        s.set_pixels(2, 1, &nearly);
        assert_eq!(counts(&s), spent(3, 2, 1));
    }

    /// (d) The shared-buffer setter: handing back the *same* `Arc` is a pointer
    /// compare, and a distinct-but-equal buffer still short-circuits on the byte
    /// fallback.
    #[gtk::test]
    fn a_shared_buffer_handed_back_builds_once() {
        let s = PixelSurface::new();
        let frame: Arc<[u8]> = Arc::from([0x22_u8; 8].as_slice());
        s.set_pixels_shared(2, 1, &frame);
        s.set_pixels_shared(2, 1, &Arc::clone(&frame));
        s.set_pixels_shared(2, 1, &frame);
        assert_eq!(
            counts(&s),
            spent(1, 0, 1),
            "one cached frame shown N times must upload exactly once",
        );

        // A different allocation with equal bytes: the ptr_eq fast path misses,
        // the byte compare still catches it.
        let twin: Arc<[u8]> = Arc::from([0x22_u8; 8].as_slice());
        assert!(!Arc::ptr_eq(&frame, &twin));
        s.set_pixels_shared(2, 1, &twin);
        assert_eq!(counts(&s), spent(1, 0, 1));

        // A real new frame still uploads.
        s.set_pixels_shared(2, 1, &Arc::from([0x23_u8; 8].as_slice()));
        assert_eq!(counts(&s), spent(2, 1, 1));
    }

    /// The two setters share one guard: a shared buffer equal to what the slice
    /// setter uploaded (and vice versa) is still the same frame, so a caller can
    /// migrate to `set_pixels_shared` without a spurious re-upload.
    #[gtk::test]
    fn the_slice_and_shared_setters_share_the_guard() {
        let s = PixelSurface::new();
        s.set_pixels(2, 1, &[0x33; 8]);
        s.set_pixels_shared(2, 1, &Arc::from([0x33_u8; 8].as_slice()));
        assert_eq!(counts(&s), spent(1, 0, 1));
        s.set_pixels(2, 1, &[0x33; 8]);
        assert_eq!(counts(&s), spent(1, 0, 1));
    }

    /// An inconsistent buffer is never remembered, so it can never let a later
    /// identical one short-circuit: the surface must keep rejecting it, and a
    /// valid frame after it must still upload.
    #[gtk::test]
    fn an_invalid_buffer_is_not_remembered() {
        let s = PixelSurface::new();
        // len 3 != 2*2*4 — rejected, renders nothing, reserves no space.
        s.set_pixels(2, 2, &[1, 2, 3]);
        s.set_pixels(2, 2, &[1, 2, 3]);
        assert_eq!(counts(&s).builds, 0, "an invalid buffer builds no texture");
        assert_eq!(s.measure(gtk::Orientation::Horizontal, -1).1, 0);

        // A valid frame after it is a real change and must upload.
        s.set_pixels(1, 1, &[9, 9, 9, 255]);
        assert_eq!(counts(&s).builds, 1);
        assert_eq!(s.measure(gtk::Orientation::Horizontal, -1).1, 1);

        // …and an invalid frame after a valid one clears without remembering.
        s.set_pixels(1, 1, &[9, 9, 9]);
        s.set_pixels(1, 1, &[9, 9, 9, 255]);
        assert_eq!(
            counts(&s).builds,
            2,
            "the cleared surface must rebuild the frame it dropped",
        );
    }
}
