//! Named numeric cast helpers for lossy-but-intentional conversions.
//!
//! The workspace's pedantic clippy configuration denies `cast_precision_loss`,
//! `cast_possible_truncation`, `cast_sign_loss`, and `cast_possible_wrap`.
//! Rather than silencing each individual site with a per-function `#[allow]`,
//! these helpers document the lossy-but-fine intent **once**, contain the
//! single justified `#[allow]` internally, and give call sites a
//! self-describing name.
//!
//! **Do not add helpers here for casts that are semantically dangerous.**
//! Each helper must carry a comment explaining why the loss is acceptable.

/// Convert a `usize` count to `f64` for coordinate arithmetic (e.g. computing
/// the x-step between sparkline samples).
///
/// # Precision contract
///
/// `f64` has a 53-bit mantissa, so integers up to 2^53 round-trip exactly.
/// Sparkline sample counts (and similar small collection lengths used for pixel
/// geometry) are at most a few thousand elements in practice — no precision is
/// lost for this use case.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn usize_to_f64(n: usize) -> f64 {
    n as f64
}

/// Convert a `GLib` `u32` item count to `usize` for Rust collection sizing.
///
/// # Wrap contract
///
/// `usize` is at least 32 bits on every platform Rust targets, so a `u32`
/// value always fits without wrapping.  `GLib`'s `ListModel::n_items` returns
/// `u32`; this helper bridges the type boundary for `Vec::with_capacity` and
/// similar sizing calls.
#[allow(clippy::cast_possible_wrap)]
pub(crate) fn u32_to_usize(n: u32) -> usize {
    n as usize
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
        // Small sample counts used for sparkline geometry must be exact.
        assert!((usize_to_f64(1) - 1.0).abs() < f64::EPSILON);
        assert!((usize_to_f64(2) - 2.0).abs() < f64::EPSILON);
        assert!((usize_to_f64(60) - 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn usize_to_f64_large_value_representable() {
        // 2^32 fits comfortably within f64's 53-bit mantissa; use it as a
        // boundary check rather than 2^53 to avoid usize width portability.
        let result = usize_to_f64(1_usize << 32);
        assert!(result.is_finite());
        assert!(result > 0.0);
    }

    // ── u32_to_usize ──────────────────────────────────────────────────────────

    #[test]
    fn u32_to_usize_zero() {
        assert_eq!(u32_to_usize(0), 0_usize);
    }

    #[test]
    fn u32_to_usize_one() {
        assert_eq!(u32_to_usize(1), 1_usize);
    }

    #[test]
    fn u32_to_usize_max_u32() {
        // u32::MAX must fit in usize on all supported platforms (usize >= 32 bits).
        let result = u32_to_usize(u32::MAX);
        assert_eq!(result, u32::MAX as usize);
    }

    #[test]
    fn u32_to_usize_typical_monitor_count() {
        // Monitor counts returned by GDK are small; spot-check a typical value.
        assert_eq!(u32_to_usize(4), 4_usize);
    }
}
