//! Font-relative pixel scaling for the Rust-set sizes that CSS can't reach.
//!
//! # Why this exists
//!
//! Most shell sizing is expressed in CSS (`em`, `-gtk-icon-size`, …), which
//! GTK already scales with the user's font size and the GNOME
//! text-scaling-factor (see the `Sizing convention (#114)` header in
//! `style.css`). But a handful of sizes are set imperatively from Rust —
//! `gtk::Image::set_pixel_size`, `Widget::set_size_request`, cairo surface
//! dimensions in the frame overlay — and those take a raw `i32` in pixels with
//! no CSS unit to ride on. This module is the imperative counterpart to the
//! `em` convention: author the value at the **design baseline** and wrap it in
//! [`scale`] so it tracks font size / text-scaling the same way the CSS does.
//!
//! # Design baseline
//!
//! Sizes passed to [`scale`] are authored as if at the Adwaita/GNOME default:
//! **font 11pt rendered at 96 DPI**, i.e. one `em` ≈
//! `11 * 96 / 72 ≈ 14.667 px` ([`BASE_EM_PX`]). At that baseline [`scale`] is
//! a **no-op** — `scale(px) == px` — so introducing it changes nothing for a
//! default-configured desktop. As the effective font grows (a larger
//! `gtk-font-name` point size, or a text-scaling-factor / DPI bump carried via
//! `gtk-xft-dpi`), [`scale`] grows the value proportionally.
//!
//! # What it deliberately does *not* track
//!
//! Monitor / `HiDPI` scale is applied **separately** by GTK: every value here is
//! in *logical* pixels, and the compositor's fractional/integer monitor scale
//! multiplies on top when the surface is composited. So [`scale`] must **not**
//! also multiply by the monitor scale factor — doing so would double-count it.
//! It tracks font size + text-scaling only.
//!
//! # Usage
//!
//! Phase 2 (#116) wires the first call sites (bar widget icons). Remaining
//! phases (#117–#119) will convert `set_size_request` and cairo-dimension
//! sites:
//!
//! ```rust,ignore
//! use crate::scale::scale;
//! image.set_pixel_size(scale(16)); // 16px at the default font, larger when scaled
//! ```

use hytte::gtk;

use crate::components::cast;

/// One `em` at the Adwaita/GNOME default: font 11pt rendered at 96 DPI.
///
/// `11 pt * 96 dpi / 72 (pt per inch) ≈ 14.667 px`. This is the reference the
/// design-baseline pixel values handed to [`scale`] are authored against, and
/// the denominator of the scaling factor (so the factor is `1.0` at default).
const BASE_EM_PX: f64 = 11.0 * 96.0 / 72.0;

/// The CSS base `font-size`, in pixels at the 1× baseline.
///
/// Every CSS `em` in the shell resolves against the root `font-size`, so this
/// is the single literal the whole CSS `em` convention (`#114`) scales from.
/// It is injected from Rust as `* { font-size: CSS_BASE_FONT_PX * `
/// [`scale_factor`]`() px }` (see [`css_base_font_px`] and
/// `main.rs::install_scaled_base_font`) rather than hard-coded in `style.css`,
/// so it rides the *same* factor [`scale`] uses — CSS text and Rust-set sizes
/// grow together instead of drifting (`#135` part 2).
///
/// Kept as its own literal, distinct from [`BASE_EM_PX`] (14.667): the two
/// baselines are deliberately *not* merged, because folding them into one
/// number would force re-authoring either every CSS `em` divisor or every
/// [`scale`] call site and risk breaking 1× identity on that side. Both ride
/// the same [`scale_factor`], which is what keeps them from drifting.
pub(crate) const CSS_BASE_FONT_PX: f64 = 13.0;

/// The point size GTK falls back to when `gtk-font-name` is missing or can't be
/// parsed as a point size.
const DEFAULT_FONT_PT: f64 = 11.0;

/// The DPI GTK assumes when `gtk-xft-dpi` is unset (`-1`).
const DEFAULT_DPI: f64 = 96.0;

/// The effective rendered size of one `em`, in *logical* pixels.
///
/// Derived from `gtk::Settings`:
/// - `gtk-font-name` (e.g. `"Cantarell 11"`) → point size via
///   `pango::FontDescription`. A font given in absolute (px) units, or one we
///   can't read a point size from, falls back to [`DEFAULT_FONT_PT`].
/// - `gtk-xft-dpi` → DPI. The property is in 1024ths of a DPI; `-1` means unset
///   (→ [`DEFAULT_DPI`]). This value carries the GNOME text-scaling-factor, so
///   reading it is how text-scaling reaches [`scale`].
///
/// Robust to GTK not being initialized (as in a headless unit test): in that
/// case it returns [`BASE_EM_PX`], which makes [`scale`] an exact no-op.
/// (`gtk::Settings::default()` *panics* off the initialized main thread, so we
/// must gate on [`gtk::is_initialized_main_thread`] rather than call it blindly
/// — this fn is only ever invoked from main-thread widget code in production.)
fn effective_font_px() -> f64 {
    if !gtk::is_initialized_main_thread() {
        return BASE_EM_PX;
    }
    let Some(settings) = gtk::Settings::default() else {
        return BASE_EM_PX;
    };

    let pt = settings
        .gtk_font_name()
        .and_then(|name| {
            let desc = gtk::pango::FontDescription::from_string(name.as_str());
            // An absolute size is in device units, not points — we have no
            // clean point conversion, so fall back rather than misinterpret.
            if desc.is_size_absolute() || desc.size() <= 0 {
                None
            } else {
                Some(f64::from(desc.size()) / f64::from(gtk::pango::SCALE))
            }
        })
        .unwrap_or(DEFAULT_FONT_PT);

    let xft = settings.gtk_xft_dpi();
    let dpi = if xft <= 0 {
        DEFAULT_DPI
    } else {
        f64::from(xft) / f64::from(gtk::pango::SCALE)
    };

    pt * dpi / 72.0
}

/// Apply a scaling `factor` to a design-baseline pixel value, rounding to the
/// nearest whole pixel.
///
/// Split out from [`scale`] so the rounding/scaling math is unit-testable
/// without a live `gtk::Settings`.
fn scale_with_factor(px: i32, factor: f64) -> i32 {
    cast::f64_to_i32_round(f64::from(px) * factor)
}

/// The dimensionless font-scaling factor shared by CSS and Rust sizing.
///
/// `effective_font_px() / `[`BASE_EM_PX`], i.e. how much larger the effective
/// font is than the 1× baseline. Exactly `1.0` at the Adwaita/GNOME default,
/// growing with a larger `gtk-font-name` point size or a text-scaling-factor /
/// DPI bump carried via `gtk-xft-dpi`. Both [`scale`] (Rust-set pixel sizes)
/// and [`css_base_font_px`] (the CSS base `font-size`) multiply through this
/// one factor, so CSS `em` and Rust `scale()` sizes track together (`#135`).
#[must_use]
pub(crate) fn scale_factor() -> f64 {
    effective_font_px() / BASE_EM_PX
}

/// Scale a design-baseline pixel value by the effective font size.
///
/// `px` is authored at the [`BASE_EM_PX`] baseline (font 11pt @ 96 DPI), where
/// this returns `px` unchanged. With a larger effective font / text-scaling it
/// returns a proportionally larger value. Monitor / `HiDPI` scale is applied
/// separately by GTK and is intentionally *not* included here (see the module
/// docs).
///
/// Phase 2 (#116) wires bar-chip icon sizes; phases 3–5 (#117–#119) cover
/// the remaining `set_size_request` and cairo-dimension sites.
#[must_use]
pub(crate) fn scale(px: i32) -> i32 {
    scale_with_factor(px, scale_factor())
}

/// The CSS base `font-size` in pixels for the *current* effective font.
///
/// [`CSS_BASE_FONT_PX`] (13 at 1×) times the shared [`scale_factor`]. Injected
/// from `main.rs` as `* { font-size: <this>px }` so every CSS `em` in the shell
/// rides the same factor as [`scale`]. At the default font the factor is `1.0`,
/// so this is exactly `13.0` → 1× is pixel-identical to the old static
/// `* { font-size: 13px }` (`#135` part 2).
#[must_use]
pub(crate) fn css_base_font_px() -> f64 {
    CSS_BASE_FONT_PX * scale_factor()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_zero() {
        assert_eq!(scale(0), 0);
    }

    #[test]
    fn no_op_at_default() {
        // In a headless test GTK isn't initialized, so `effective_font_px()`
        // returns BASE_EM_PX and the factor is exactly 1.0 — `scale` must be a
        // pure no-op. If GTK *is* up and the configured font happens to differ
        // from the baseline, allow ±1px of rounding slack.
        if gtk::is_initialized_main_thread() {
            assert!((scale(16) - 16).abs() <= 1, "scale(16) = {}", scale(16));
        } else {
            assert_eq!(scale(16), 16);
        }
    }

    #[test]
    fn factor_scales_linearly() {
        // Pure-math: a 2× factor doubles, a 0.5× factor halves, 1× is identity.
        assert_eq!(scale_with_factor(16, 1.0), 16);
        assert_eq!(scale_with_factor(16, 2.0), 32);
        assert_eq!(scale_with_factor(16, 0.5), 8);
        // Rounds to nearest: 15 * 1.5 = 22.5 → 23.
        assert_eq!(scale_with_factor(15, 1.5), 23);
        assert_eq!(scale_with_factor(0, 3.0), 0);
    }

    /// Pins that the three in-card scroll-height literals (#708) grow
    /// proportionally with the font-scaling factor, the same way any other
    /// `scale()` call site does — `connections.rs`'s `set_max_content_height`
    /// cap (480), `network/wifi.rs`'s network-list cap (240), and
    /// `notifications.rs`'s history-list *floor* (380, a
    /// `set_min_content_height` rather than a max — the direction doesn't
    /// change the math). At 1× each is a no-op (matching the module's no-op
    /// guarantee); above 1× each grows with the same factor CSS `em`s use, so
    /// none of the three can drift back to a raw, non-tracking pixel value
    /// without this test's expected numbers moving too.
    #[test]
    fn three_scroll_heights_scale_with_factor() {
        // 1× — identity, same guarantee `no_op_at_default` pins for `scale()`.
        assert_eq!(scale_with_factor(480, 1.0), 480); // connections.rs cap
        assert_eq!(scale_with_factor(240, 1.0), 240); // network/wifi.rs cap
        assert_eq!(scale_with_factor(380, 1.0), 380); // notifications.rs floor

        // 1.5× — a plausible large-text-scaling-factor bump.
        assert_eq!(scale_with_factor(480, 1.5), 720);
        assert_eq!(scale_with_factor(240, 1.5), 360);
        assert_eq!(scale_with_factor(380, 1.5), 570);
    }

    #[test]
    fn baseline_em_is_default() {
        // Sanity-check the documented baseline arithmetic.
        assert!((BASE_EM_PX - 14.666_666).abs() < 1e-3, "{BASE_EM_PX}");
    }

    /// Pure-math counterpart to [`css_base_font_px`], so the CSS-base scaling
    /// is unit-testable without a live `gtk::Settings` (mirrors
    /// [`scale_with_factor`]). `css_base_font_px()` == `this(scale_factor())`.
    fn css_base_font_px_with_factor(factor: f64) -> f64 {
        CSS_BASE_FONT_PX * factor
    }

    #[test]
    fn css_base_scales_linearly() {
        // 1× is pixel-identical to the old static `* { font-size: 13px }`.
        assert!((css_base_font_px_with_factor(1.0) - 13.0).abs() < 1e-9);
        // A larger effective font grows the base proportionally…
        assert!((css_base_font_px_with_factor(2.0) - 26.0).abs() < 1e-9);
        // …and a smaller one shrinks it.
        assert!((css_base_font_px_with_factor(0.5) - 6.5).abs() < 1e-9);
    }

    #[test]
    fn css_base_no_op_at_default() {
        // Headless: GTK isn't initialized, so `scale_factor()` is exactly 1.0
        // and the CSS base is exactly the 1× literal — the same no-op guarantee
        // `scale()` gives, keeping 1× pixel-identical. If GTK *is* up with a
        // non-baseline font, allow a little slack.
        if gtk::is_initialized_main_thread() {
            assert!(
                (css_base_font_px() - CSS_BASE_FONT_PX).abs() <= 1.0,
                "css_base_font_px() = {}",
                css_base_font_px()
            );
        } else {
            assert!((css_base_font_px() - CSS_BASE_FONT_PX).abs() < 1e-9);
        }
    }
}
