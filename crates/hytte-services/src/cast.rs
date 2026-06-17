//! Named numeric cast helpers for lossy-but-intentional conversions.
//!
//! The workspace's pedantic clippy configuration denies `cast_precision_loss`,
//! `cast_possible_truncation`, and `cast_sign_loss`. Rather than silencing each
//! individual site with a per-function `#[allow]`, these helpers document the
//! lossy-but-fine intent **once**, contain the single justified `#[allow]`
//! internally, and give call sites a self-describing name.
//!
//! **Do not add helpers here for casts that are semantically dangerous.**
//! Each helper must carry a comment explaining why the loss is acceptable.

/// Convert a `usize` count to `f64` for arithmetic (e.g. dividing by channel
/// count to compute an average).
///
/// # Precision contract
///
/// `f64` has 53-bit mantissa, so integers up to 2^53 round-trip exactly.
/// A channel count (or any small collection length) is at most a few dozen
/// elements in practice — no precision is lost for this use case.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn usize_to_f64(n: usize) -> f64 {
    n as f64
}

/// Narrow a linear audio gain from `f64` to `f32`.
///
/// # Truncation contract
///
/// `PipeWire`'s `channelVolumes` element type is `f32`. A linear gain value
/// lives in `[0.0, 1.0]` (or slightly above for software boost), so the
/// narrowing from `f64` to `f32` loses at most ~7 decimal digits of precision —
/// well within the resolution of human-perceptible volume steps.  The value is
/// never used for accumulation, so rounding error does not compound.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn f64_to_f32_gain(v: f64) -> f32 {
    v as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── usize_to_f64 ──────────────────────────────────────────────────────────

    #[test]
    fn usize_to_f64_zero() {
        // 0 must map to exactly 0.0 — both are the zero value.
        assert!(usize_to_f64(0).abs() < f64::EPSILON);
    }

    #[test]
    fn usize_to_f64_small_counts_exact() {
        // Channel counts in practice are 1–8; the conversion must be exact:
        // the f64 value should equal the expected constant with no rounding.
        assert!((usize_to_f64(1) - 1.0).abs() < f64::EPSILON);
        assert!((usize_to_f64(2) - 2.0).abs() < f64::EPSILON);
        assert!((usize_to_f64(8) - 8.0).abs() < f64::EPSILON);
    }

    #[test]
    fn usize_to_f64_large_value_representable() {
        // 2^32 fits comfortably within f64's 53-bit mantissa; use it as a
        // boundary check rather than 2^53 to avoid usize width portability.
        let result = usize_to_f64(1_usize << 32);
        assert!(result.is_finite());
        assert!(result > 0.0);
    }

    // ── f64_to_f32_gain ───────────────────────────────────────────────────────

    #[test]
    fn f64_to_f32_gain_zero() {
        // Silence is silence: 0.0 → 0.0_f32 exactly.
        assert!(f64_to_f32_gain(0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn f64_to_f32_gain_one() {
        // Unity gain stays exactly representable.
        assert!((f64_to_f32_gain(1.0) - 1.0_f32).abs() < f32::EPSILON);
    }

    #[test]
    fn f64_to_f32_gain_half_roundtrips() {
        // 0.5 is exactly representable in both f32 and f64.
        assert!((f64_to_f32_gain(0.5) - 0.5_f32).abs() < f32::EPSILON);
    }

    #[test]
    fn f64_to_f32_gain_stays_finite() {
        // Values in the valid gain range must not become infinite or NaN.
        for i in 0..=100_u32 {
            let v = f64::from(i) / 100.0;
            let r = f64_to_f32_gain(v);
            assert!(r.is_finite(), "gain {v} produced non-finite f32");
        }
    }

    #[test]
    fn f64_to_f32_gain_preserves_sign() {
        // Negative values (if ever passed) should not flip sign.
        let r = f64_to_f32_gain(-0.5);
        assert!(r < 0.0);
    }
}
