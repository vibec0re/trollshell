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

/// Convert a `u64` byte count to `f64` for network rate arithmetic (bytes/sec).
///
/// # Precision contract
///
/// `f64` has a 53-bit mantissa, so integers up to 2^53 (≈ 9 PiB) round-trip
/// exactly. A 1-second byte delta on a real NIC is at most a few hundred MiB —
/// well within the exact range. Even at 100 Gbps sustained the delta is
/// ~12.5 GiB ≈ 1.3 × 10^10, safely below 2^53. No precision is lost for
/// this use case.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn u64_to_f64_bytes(n: u64) -> f64 {
    n as f64
}

/// Convert a `u64` counter (jiffies, disk blocks, …) to `f64` for ratio
/// computation.
///
/// # Precision contract
///
/// Used where two `u64` counters form a ratio (e.g. `d_active / d_total` for
/// CPU load, `used / total` for disk usage). Both operands are realistic
/// kernel counters: jiffy totals are bounded by uptime × CPU count (never
/// near 2^53 in practice), and disk block counts on consumer hardware are
/// likewise well below 2^53. The resulting `f64` is immediately divided to
/// produce a fraction in `0.0..=1.0`; sub-ulp precision loss in the
/// numerator or denominator is irrelevant at display resolution.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn u64_to_f64_count(n: u64) -> f64 {
    n as f64
}

/// Convert a milli-Celsius reading from sysfs (`u64`, e.g. from
/// `/sys/class/hwmon/.../temp*_input`) to degrees Celsius (`f64`).
///
/// # Precision contract
///
/// sysfs reports temperatures in thousandths of a degree. A realistic CPU or
/// GPU temperature is 20 000 – 110 000 milli-°C. Dividing by 1 000.0 gives a
/// value in the tens-to-hundreds range — exactly representable in `f64` to
/// well beyond display precision. The `u64 → f64` cast could theoretically
/// lose precision for values near 2^53, but no real sensor produces values
/// anywhere near 9 × 10^12 °C.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn millicelsius_to_celsius(milli: u64) -> f64 {
    milli as f64 / 1_000.0
}

/// Convert a kHz frequency reading from sysfs (`u64`, e.g. from
/// `/sys/devices/system/cpu/cpuN/cpufreq/scaling_cur_freq`) to Hz (`f64`).
///
/// # Precision contract
///
/// sysfs reports CPU frequencies in kHz. A realistic current or ceiling
/// frequency is ~800 000 – 6 000 000 kHz; multiplying by 1 000.0 gives a value
/// of order 10^9 — far below `f64`'s 2^53 exact-integer range, so the `u64 →
/// f64` cast is exact and no precision is lost for this use case.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn khz_to_hz(khz: u64) -> f64 {
    khz as f64 * 1_000.0
}

/// Convert a whole-number percent in `u64` (e.g. GPU busy percent from
/// `/sys/class/drm/.../gpu_busy_percent`) to a `0.0..=1.0` ratio.
///
/// # Precision contract
///
/// The source is an integer in `0..=100`, so the cast to `f64` is exact for
/// every possible input value (all fit in the 53-bit mantissa). Dividing by
/// 100.0 gives a fraction accurate to the nearest hundredth — sufficient for
/// display.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn percent_u64_to_ratio(v: u64) -> f64 {
    v as f64 / 100.0
}

/// Extract the low 8 bits of a `u32` octal byte value decoded from
/// `/proc/self/mountinfo` path escapes (`\NNN`, where NNN ∈ `000`–`377` octal).
///
/// # Truncation contract
///
/// `mountinfo` octal escapes are `\000`–`\377`, encoding byte values 0–255.
/// The caller (in `sensors`) already verifies each digit is in `'0'..='7'`
/// before calling this, and `proc(5)` guarantees only valid byte escapes are
/// emitted, so the value is always ≤ 255 and the low-byte truncation is safe
/// by construction.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn octal_byte_from_u32(v: u32) -> u8 {
    v as u8
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

    // ── u64_to_f64_bytes ─────────────────────────────────────────────────────

    #[test]
    fn u64_to_f64_bytes_zero() {
        assert!(u64_to_f64_bytes(0).abs() < f64::EPSILON);
    }

    #[test]
    fn u64_to_f64_bytes_typical_nic_delta_exact() {
        // 100 MiB/s delta — well below 2^53; must convert exactly.
        let hundred_mib: u64 = 100 * 1024 * 1024;
        let result = u64_to_f64_bytes(hundred_mib);
        assert!((result - 104_857_600.0_f64).abs() < f64::EPSILON);
    }

    // ── u64_to_f64_count ─────────────────────────────────────────────────────

    #[test]
    fn u64_to_f64_count_zero() {
        assert!(u64_to_f64_count(0).abs() < f64::EPSILON);
    }

    #[test]
    fn u64_to_f64_count_small_exact() {
        assert!((u64_to_f64_count(1000) - 1000.0).abs() < f64::EPSILON);
    }

    // ── millicelsius_to_celsius ───────────────────────────────────────────────

    #[test]
    fn millicelsius_to_celsius_typical() {
        // 45 000 milli-°C = 45.0 °C exactly.
        assert!((millicelsius_to_celsius(45_000) - 45.0).abs() < f64::EPSILON);
    }

    #[test]
    fn millicelsius_to_celsius_zero() {
        assert!(millicelsius_to_celsius(0).abs() < f64::EPSILON);
    }

    #[test]
    fn millicelsius_to_celsius_precision() {
        // 52 125 milli-°C = 52.125 °C (exactly representable in f64).
        assert!((millicelsius_to_celsius(52_125) - 52.125).abs() < f64::EPSILON);
    }

    // ── khz_to_hz ─────────────────────────────────────────────────────────────

    #[test]
    fn khz_to_hz_zero() {
        assert!(khz_to_hz(0).abs() < f64::EPSILON);
    }

    #[test]
    fn khz_to_hz_typical() {
        // 2 400 000 kHz = 2.4 GHz exactly.
        assert!((khz_to_hz(2_400_000) - 2.4e9).abs() < f64::EPSILON);
    }

    // ── percent_u64_to_ratio ──────────────────────────────────────────────────

    #[test]
    fn percent_u64_to_ratio_zero() {
        assert!(percent_u64_to_ratio(0).abs() < f64::EPSILON);
    }

    #[test]
    fn percent_u64_to_ratio_hundred() {
        assert!((percent_u64_to_ratio(100) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn percent_u64_to_ratio_fifty() {
        assert!((percent_u64_to_ratio(50) - 0.5).abs() < f64::EPSILON);
    }

    // ── octal_byte_from_u32 ───────────────────────────────────────────────────

    #[test]
    fn octal_byte_from_u32_zero() {
        assert_eq!(octal_byte_from_u32(0), 0u8);
    }

    #[test]
    fn octal_byte_from_u32_space() {
        // \040 octal = 32 decimal = ASCII space.
        assert_eq!(octal_byte_from_u32(32), b' ');
    }

    #[test]
    fn octal_byte_from_u32_max_byte() {
        // \377 octal = 255 decimal — the largest valid mountinfo escape.
        assert_eq!(octal_byte_from_u32(255), 255u8);
    }
}
