//! Which axis an aspect-locked surface is *fitted* on — the one rule
//! [`PixelSurface`](crate::PixelSurface), [`GlSurface`](crate::GlSurface) and
//! [`ShaderSurface`](crate::ShaderSurface) share for `request_mode` and
//! `measure` (#1387).
//!
//! # Why there is a choice at all
//!
//! All three widgets carry a buffer whose pixel dimensions are an **aspect
//! ratio**, not a box: the minimum is `0` on both axes so CSS can scale them
//! freely, and the draw letterboxes into the largest buffer-aspect rect that
//! fits whatever allocation they got. What is left is which of the two
//! dimensions the layout hands down and which one the widget derives from it —
//! and that is a property of the **mount**, not of the buffer:
//!
//! - A **sidebar card** (or a drawer page) is a vertical stack of full-width
//!   rows. The width comes down from the layout and the height follows it, so
//!   the surface is [`FitAxis::Width`] — height-for-width, which is what all
//!   three measured unconditionally before #1387 and is still the default.
//! - A **bar chip** is the mirror: the bar's height is fixed and the row is
//!   horizontal, so the *height* comes down and the width has to follow.
//!   That is [`FitAxis::Height`] — width-for-height.
//!
//! Getting that backwards is not a distortion (the draw still letterboxes) but
//! a **reservation**: a height-for-width surface in a bar is never asked "how
//! wide for this height", so it answers with its whole buffer width and then
//! draws the much smaller rect the bar's height allows, centred. #1387 is that
//! bug — the timer's `mm:ss` readout is a 188×70 buffer reserving 188 px in a
//! bar that draws it ~24 px tall, i.e. ~75 px of readout with ~56 px of empty
//! chip either side.
//!
//! # Why the arithmetic lives here and not in the three widgets
//!
//! It is the same four lines three times over, it is the whole content of the
//! fix, and it needs no GTK instance to be true — so it is one pure function
//! with its own hermetic tests, and each `measure` is the two lines that read
//! the widget's natural size and hand it over. The widgets keep their own
//! `measure` docs, because what a *natural size* means still differs between
//! them (`PixelSurface` multiplies its buffer by an integer upscale; the two GL
//! surfaces take theirs from the node).

/// Which axis the layout constrains, and therefore which one an aspect-locked
/// surface derives from the other.
///
/// [`Width`](Self::Width) is the default and is what every surface measured
/// before #1387: height-for-width, the right answer for a sidebar card or a
/// drawer page. [`Height`](Self::Height) is its mirror for a **bar** mount,
/// where the bar's height is the fixed dimension. See the [module docs](self)
/// for why the wrong one letterboxes rather than distorts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FitAxis {
    /// Height-for-width: the layout hands down a width, the height follows the
    /// buffer's aspect ratio. The default, and what a sidebar card wants.
    #[default]
    Width,
    /// Width-for-height: the layout hands down a height, the width follows the
    /// buffer's aspect ratio. What a **bar** chip wants (#1387).
    Height,
}

impl FitAxis {
    /// The GTK size-request mode a surface fitted on this axis reports.
    ///
    /// This is the half of the fix GTK actually reads: a widget only ever gets
    /// asked for its width *for a height* when it says it is
    /// [`WidthForHeight`](gtk::SizeRequestMode::WidthForHeight), and GTK's own
    /// containers propagate the mode up (a `GtkBox`/`GtkBinLayout` reports the
    /// mode most of its children want), so one chip deep inside a bar's button
    /// is enough to make the bar's row measure it the right way round.
    pub(crate) fn request_mode(self) -> gtk::SizeRequestMode {
        match self {
            Self::Width => gtk::SizeRequestMode::HeightForWidth,
            Self::Height => gtk::SizeRequestMode::WidthForHeight,
        }
    }

    /// The natural size a surface whose natural buffer is `nat_w`×`nat_h`
    /// reports for `orientation`, given GTK's `for_size` on the other axis.
    ///
    /// Along the **constrained** axis (the one this `FitAxis` names) the answer
    /// is the buffer's own natural size: nothing is derived, because that is
    /// the dimension the layout hands down. Along the **derived** axis the
    /// answer is the aspect-locked one, `for_size * this / that`, whenever GTK
    /// passed a real size (`for_size > 0`) and the buffer has an aspect ratio
    /// to lock to; with the other axis still unconstrained (`for_size == -1`,
    /// which is what GTK passes while it is collecting a widget's
    /// mode-independent request) it falls back to the buffer's natural size on
    /// this axis.
    ///
    /// Computed in `i64` so the intermediate product cannot overflow, saturated
    /// back into `i32`, and never negative — the caller pairs it with a
    /// **minimum of 0** on both axes, which is what lets CSS scale a surface
    /// above its grid (the LCD look) and what keeps a bar chip from forcing the
    /// bar taller.
    pub(crate) fn natural(
        self,
        orientation: gtk::Orientation,
        for_size: i32,
        nat_w: i32,
        nat_h: i32,
    ) -> i32 {
        let horizontal = orientation == gtk::Orientation::Horizontal;
        // (the size on this axis, the size on the other one)
        let (this, other) = if horizontal {
            (nat_w, nat_h)
        } else {
            (nat_h, nat_w)
        };
        let derived = match self {
            // Height-for-width derives the height; the width is handed down.
            Self::Width => !horizontal,
            // Width-for-height derives the width; the height is handed down.
            Self::Height => horizontal,
        };
        let natural = if derived && for_size > 0 && other > 0 {
            let scaled = i64::from(for_size) * i64::from(this) / i64::from(other);
            i32::try_from(scaled).unwrap_or(i32::MAX)
        } else {
            this
        };
        natural.max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::FitAxis;

    /// The timer's seven-segment `mm:ss` readout: `2·PAD + 4·DIGIT_W +
    /// COLON_W + 4·GAP` × `2·PAD + DIGIT_H` (`hytte-preem`'s `seven_seg`), the
    /// buffer #1387's screenshot was taken of.
    const SEVEN_SEG: (i32, i32) = (188, 70);

    #[test]
    fn width_is_the_default_and_means_height_for_width() {
        assert_eq!(FitAxis::default(), FitAxis::Width);
        assert_eq!(
            FitAxis::Width.request_mode(),
            gtk::SizeRequestMode::HeightForWidth
        );
        assert_eq!(
            FitAxis::Height.request_mode(),
            gtk::SizeRequestMode::WidthForHeight
        );
    }

    #[test]
    fn fitting_on_width_derives_the_height() {
        let (w, h) = SEVEN_SEG;
        // The width is handed down, so it is answered whole and `for_size` is
        // not consulted.
        assert_eq!(
            FitAxis::Width.natural(gtk::Orientation::Horizontal, -1, w, h),
            188
        );
        assert_eq!(
            FitAxis::Width.natural(gtk::Orientation::Horizontal, 28, w, h),
            188
        );
        // …and the height follows the aspect ratio of the width offered.
        assert_eq!(
            FitAxis::Width.natural(gtk::Orientation::Vertical, 188, w, h),
            70
        );
        assert_eq!(
            FitAxis::Width.natural(gtk::Orientation::Vertical, 94, w, h),
            35
        );
    }

    /// The #1387 fix in one assertion: at the bar's height the chip asks for
    /// the width it is going to *draw*, not the width of the buffer.
    #[test]
    fn fitting_on_height_derives_the_width() {
        let (w, h) = SEVEN_SEG;
        // The height is handed down, so it is answered whole.
        assert_eq!(
            FitAxis::Height.natural(gtk::Orientation::Vertical, -1, w, h),
            70
        );
        assert_eq!(
            FitAxis::Height.natural(gtk::Orientation::Vertical, 188, w, h),
            70
        );
        // …and the width follows: 28 px of bar buys 75 px of readout, not 188.
        assert_eq!(
            FitAxis::Height.natural(gtk::Orientation::Horizontal, 28, w, h),
            75
        );
        assert_ne!(
            FitAxis::Height.natural(gtk::Orientation::Horizontal, 28, w, h),
            188
        );
    }

    #[test]
    fn an_unconstrained_derived_axis_falls_back_to_the_buffer() {
        let (w, h) = SEVEN_SEG;
        // GTK passes -1 while it is collecting the mode-independent request;
        // both axes then answer the buffer's own size, whichever the fit.
        assert_eq!(
            FitAxis::Width.natural(gtk::Orientation::Vertical, -1, w, h),
            70
        );
        assert_eq!(
            FitAxis::Height.natural(gtk::Orientation::Horizontal, -1, w, h),
            188
        );
        // `0` is not a size either (GTK's other "no constraint" spelling in
        // practice), and neither is a negative one.
        assert_eq!(
            FitAxis::Height.natural(gtk::Orientation::Horizontal, 0, w, h),
            188
        );
        assert_eq!(
            FitAxis::Width.natural(gtk::Orientation::Vertical, -7, w, h),
            70
        );
    }

    #[test]
    fn a_buffer_with_no_aspect_ratio_derives_nothing() {
        // An empty surface: no aspect to lock to, so every answer is 0 rather
        // than a division by zero.
        for axis in [FitAxis::Width, FitAxis::Height] {
            for orientation in [gtk::Orientation::Horizontal, gtk::Orientation::Vertical] {
                assert_eq!(axis.natural(orientation, 200, 0, 0), 0);
                assert_eq!(axis.natural(orientation, -1, 0, 0), 0);
            }
        }
        // Degenerate on one axis only: the zero side cannot be a divisor, so
        // the derived answer falls back to this axis's own natural size.
        assert_eq!(
            FitAxis::Width.natural(gtk::Orientation::Vertical, 200, 0, 70),
            70
        );
        assert_eq!(
            FitAxis::Height.natural(gtk::Orientation::Horizontal, 200, 188, 0),
            188
        );
    }

    #[test]
    fn a_huge_request_saturates_instead_of_overflowing() {
        // i32::MAX px of width against a 1:1000 buffer would overflow an i32
        // product; the i64 math saturates and never panics.
        assert_eq!(
            FitAxis::Width.natural(gtk::Orientation::Vertical, i32::MAX, 1, 1000),
            i32::MAX
        );
        assert_eq!(
            FitAxis::Height.natural(gtk::Orientation::Horizontal, i32::MAX, 1000, 1),
            i32::MAX
        );
        // A negative natural size (not constructible from the setters, but the
        // fields are `i32`) never leaves through the return value.
        assert_eq!(
            FitAxis::Width.natural(gtk::Orientation::Horizontal, -1, -5, 70),
            0
        );
    }
}
