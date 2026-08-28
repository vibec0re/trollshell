//! Named cast helpers for the trollshell binary.
//!
//! Every numeric-cast in the binary that clippy would flag carries one of a
//! small set of *documented* properties (precision loss on large integers,
//! truncation of a non-negative f64, …). Rather than scattering
//! `#[allow(clippy::cast_*)]` across every call site, each property gets one
//! named helper here with a single internal `#[allow]` and an explanation of
//! why the cast is safe in context.
//!
//! # Importing
//!
//! ```rust,ignore
//! use crate::components::cast;
//! // then: cast::u64_to_f64(n), cast::f64_to_u32_trunc(v), …
//! ```

/// Convert a `u64` counter or byte count to `f64` for ratio / display maths.
///
/// `f64` has 53-bit mantissa, so integers up to 2^53 (≈ 9 PiB, 9 quadrillion
/// microseconds) round-trip exactly. Values beyond that threshold lose up to
/// ~1 ULP of precision, which is acceptable for display and progress-bar use.
///
/// # Panics
///
/// Never panics; every `u64` has a finite `f64` representation.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn u64_to_f64(n: u64) -> f64 {
    n as f64
}

/// Round a non-negative `f64` to the nearest integer and return it as `u32`.
///
/// This is the display-percent counterpart to [`u64_to_f64`]. Callers are
/// responsible for ensuring `v` is in `[0.0, u32::MAX as f64]` — the helpers
/// in this module that produce values fed here (`u64_to_f64` ratios × 100,
/// linear-audio values × 100 after optional clamping) all satisfy this.
///
/// **Truncation:** `v.round()` is always in `[0.0, …)` so the fractional part
/// is at most 0.5, which `as u32` drops — identical to rounding.
/// **Sign loss:** `v ≥ 0.0` by caller contract, so there is no sign to lose.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn f64_to_u32_trunc(v: f64) -> u32 {
    v as u32
}

/// Truncate a non-negative `f64` to `i64`, used for MPRIS seek positions in
/// microseconds.
///
/// Seek positions are computed as `fraction × duration_us` where both inputs
/// are non-negative and the result fits comfortably in `i64` (max ~292 years).
///
/// **Truncation:** sub-microsecond precision is intentionally discarded.
/// **Sign loss:** the value is always non-negative; no sign to lose.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(crate) fn f64_to_i64_trunc(v: f64) -> i64 {
    v as i64
}

/// Round an `f64` to the nearest integer and return it as `i32`.
///
/// Used by [`crate::scale`] to turn a font-scaled pixel value back into the
/// `i32` that GTK setters (`set_pixel_size`, `set_size_request`) expect.
///
/// **Truncation:** `v.round()` has a zero fractional part, so the `as i32`
/// truncation is exact for any `v` whose rounded magnitude is `< 2^31`.
/// Callers feed design-baseline pixel sizes (tens to low hundreds) times a
/// small scaling factor, which never approaches that bound; a non-finite `v`
/// saturates to `0` / `i32::MAX` / `i32::MIN` rather than wrapping.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn f64_to_i32_round(v: f64) -> i32 {
    v.round() as i32
}

/// Narrow an `f64` load fraction to the `f32` the `hytte-preem` raster kit
/// takes (#857).
///
/// **Truncation:** `f32` has a 24-bit mantissa against `f64`'s 53, so this
/// drops precision — deliberately. The values are `0.0..=1.0` load fractions
/// heading for an 8-bit LED brightness, where a rounding difference below
/// `1/255` is invisible by construction. A non-finite `f64` stays non-finite,
/// which the kit's renderers already document as "lights nothing".
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn f64_to_f32(v: f64) -> f32 {
    v as f32
}

/// Convert a non-negative `i32` pixel width to `usize` for stride calculation.
///
/// SNI icon widths come from the D-Bus pixmap tuple `(w: i32, h: i32, …)`.
/// A negative width is invalid per the SNI spec, so sign loss cannot occur in
/// practice.
#[allow(clippy::cast_sign_loss)]
pub(crate) fn i32_to_usize(n: i32) -> usize {
    n as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u64_to_f64_small_exact() {
        // Use `.to_bits()` to avoid the pedantic `float_cmp` lint; these casts
        // are exact (values are within the 2^53 mantissa range).
        assert_eq!(u64_to_f64(0).to_bits(), 0.0_f64.to_bits());
        assert_eq!(u64_to_f64(1_000_000).to_bits(), 1_000_000.0_f64.to_bits());
        // 2^53 = 9_007_199_254_740_992 is the largest u64 that maps exactly.
        assert_eq!(
            u64_to_f64(9_007_199_254_740_992).to_bits(),
            9_007_199_254_740_992.0_f64.to_bits(),
        );
    }

    #[test]
    fn f64_to_u32_trunc_round_trip() {
        assert_eq!(f64_to_u32_trunc(0.0), 0_u32);
        assert_eq!(f64_to_u32_trunc(100.0), 100_u32);
        assert_eq!(f64_to_u32_trunc(99.6_f64.round()), 100_u32);
        assert_eq!(f64_to_u32_trunc(33.4_f64.round()), 33_u32);
    }

    #[test]
    fn f64_to_i64_trunc_positive() {
        assert_eq!(f64_to_i64_trunc(0.0), 0_i64);
        assert_eq!(f64_to_i64_trunc(1_500_000.7), 1_500_000_i64);
        assert_eq!(f64_to_i64_trunc(1.0e12), 1_000_000_000_000_i64);
    }

    #[test]
    fn f64_to_i32_round_nearest() {
        assert_eq!(f64_to_i32_round(0.0), 0_i32);
        assert_eq!(f64_to_i32_round(15.6), 16_i32);
        assert_eq!(f64_to_i32_round(15.4), 15_i32);
        assert_eq!(f64_to_i32_round(-2.5), -3_i32);
    }

    #[test]
    fn f64_to_f32_keeps_load_fractions() {
        // Exactly-representable fractions round-trip; `to_bits` sidesteps the
        // pedantic `float_cmp` lint the way the tests above do.
        assert_eq!(f64_to_f32(0.0).to_bits(), 0.0_f32.to_bits());
        assert_eq!(f64_to_f32(0.5).to_bits(), 0.5_f32.to_bits());
        assert_eq!(f64_to_f32(1.0).to_bits(), 1.0_f32.to_bits());
        // A load fraction stays well inside 1/255 of its `f64` original.
        assert!((f64::from(f64_to_f32(0.123_456_789)) - 0.123_456_789).abs() < 1.0 / 255.0);
        // Non-finite input stays non-finite (the kit reads that as "dark").
        assert!(f64_to_f32(f64::NAN).is_nan());
    }

    #[test]
    fn i32_to_usize_non_negative() {
        assert_eq!(i32_to_usize(0), 0_usize);
        assert_eq!(i32_to_usize(32), 32_usize);
        assert_eq!(i32_to_usize(1920) * 4, 7680_usize);
    }
}
