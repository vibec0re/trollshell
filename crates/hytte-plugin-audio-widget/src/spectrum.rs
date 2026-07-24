//! The **spectrum scope tile**: the 16-band [`AudioSpectrum`] drawn as a bar
//! spectrum via the preem [`Frame`] primitives (#506).
//!
//! Plugin-local rather than a kit widget: the kit's skin palettes
//! (`DisplayStyle::palette`, the `Emission`/`Bloom` machinery) are `pub(crate)`,
//! so a plugin can only reach [`Frame`]'s public draw primitives — exactly like
//! `hytte-plugin-preem-demo`'s own scope tile, which this mirrors. The colors
//! are a fixed VFD-flavored palette (cyan bars on near-black) so the tile reads
//! cohesively beside the dot-matrix marquee and the LED strip.

use hytte_plugin::preem::{Frame, Rgba};
use hytte_plugin::proto::SPECTRUM_BINS;

/// Scope tile width in px — inside the ~296 px sidebar card (the #313 lesson).
pub const SCOPE_W: usize = 268;
/// Scope tile height in px.
pub const SCOPE_H: usize = 44;
/// Horizontal gap between adjacent bars, in px.
const GAP: usize = 2;

/// The near-black scope backdrop.
const BG: Rgba = [0x08, 0x0a, 0x10, 0xff];
/// The cyan bar fill (VFD ink flavored, so it sits beside the VFD marquee/LEDs).
const BAR: Rgba = [0x6d, 0xf0, 0xff, 0xff];
/// The bright cap topping each bar.
const CAP: Rgba = [0xff, 0xff, 0xff, 0xff];

/// Render the 16-band spectrum as a bar tile. Each bar's height tracks its band
/// level (`0.0..=1.0`) and carries a bright cap; a silent band still draws a
/// 1 px baseline so the tile always reads. The buffer is fully opaque and always
/// satisfies the host's `len == w * h * 4` invariant.
// The dimensions are compile-time-bounded constants (`SCOPE_W`/`SCOPE_H` ≤ 268)
// and a band count, so the usize→i32/f32 casts can neither wrap, truncate, nor
// lose precision — the same allow-set `hytte-plugin-preem-demo::scope_tile` uses.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
#[must_use]
pub fn scope_tile(bins: &[f32; SPECTRUM_BINS]) -> Frame {
    let mut frame = Frame::filled(SCOPE_W, SCOPE_H, BG);
    let bar_w = (SCOPE_W - GAP * (SPECTRUM_BINS - 1)) / SPECTRUM_BINS;
    let max_h = SCOPE_H - 2;
    for (i, &v) in bins.iter().enumerate() {
        let x = (i * (bar_w + GAP)) as i32;
        let h = (v.clamp(0.0, 1.0) * max_h as f32).round() as usize;
        if h == 0 {
            // A 1 px floor so silent bands still read as a baseline.
            frame.rect(x, (SCOPE_H - 1) as i32, bar_w as i32, 1, BAR);
            continue;
        }
        let y = (SCOPE_H - h) as i32;
        frame.rect(x, y, bar_w as i32, h as i32, BAR);
        frame.rect(x, y, bar_w as i32, 1, CAP);
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::{BG, GAP, SCOPE_H, SCOPE_W, scope_tile};
    use hytte_plugin::proto::SPECTRUM_BINS;

    /// Buffer honors the host's `len == w * h * 4` seam and the sidebar width.
    #[test]
    fn buffer_is_valid_and_fits_the_card() {
        let f = scope_tile(&[0.5; SPECTRUM_BINS]);
        assert_eq!(f.data().len(), f.width() * f.height() * 4);
        assert_eq!((f.width(), f.height()), (SCOPE_W, SCOPE_H));
        assert!(f.width() <= 296, "fits the ~296 px sidebar card");
        assert!(f.data().chunks_exact(4).all(|px| px[3] == 0xff), "opaque");
    }

    /// A loud band paints a taller bar than silence; extreme inputs never panic.
    #[test]
    fn a_loud_band_paints_taller() {
        let quiet = scope_tile(&[0.0; SPECTRUM_BINS]);
        let mut loud = [0.0_f32; SPECTRUM_BINS];
        loud[8] = 1.0;
        let loud = scope_tile(&loud);
        assert_ne!(quiet.data(), loud.data());
        // Over-unit / negative / NaN bands clamp rather than panicking.
        let _ = scope_tile(&[2.0; SPECTRUM_BINS]);
        let _ = scope_tile(&[-1.0; SPECTRUM_BINS]);
        let _ = scope_tile(&[f32::NAN; SPECTRUM_BINS]);
    }

    /// Bar height is monotone in the band level: a full band fills the drawable
    /// height, half is about half, and silence still leaves a 1 px baseline.
    #[test]
    fn bar_height_tracks_level() {
        fn bar0_height(level: f32) -> usize {
            let mut bins = [0.0_f32; SPECTRUM_BINS];
            bins[0] = level;
            let f = scope_tile(&bins);
            let bar_w = (SCOPE_W - GAP * (SPECTRUM_BINS - 1)) / SPECTRUM_BINS;
            let x = bar_w / 2;
            let w = f.width();
            let data = f.data();
            (0..f.height())
                .filter(|&y| {
                    let idx = (y * w + x) * 4;
                    data[idx..idx + 4] != BG
                })
                .count()
        }
        let max_h = SCOPE_H - 2;
        assert_eq!(bar0_height(1.0), max_h, "a full band fills the height");
        assert!(
            bar0_height(0.5).abs_diff(max_h / 2) <= 1,
            "half is about half"
        );
        assert!(bar0_height(0.25) < bar0_height(0.5));
        assert_eq!(bar0_height(0.0), 1, "a silent band draws a 1 px baseline");
    }
}
