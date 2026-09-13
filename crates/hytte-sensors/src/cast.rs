//! Named numeric cast helpers for lossy-but-intentional conversions.
//!
//! The workspace's pedantic clippy configuration denies `cast_precision_loss`,
//! `cast_possible_truncation`, and `cast_sign_loss`. Rather than silencing each
//! individual site with a per-function `#[allow]`, these helpers document the
//! lossy-but-fine intent **once**, contain the single justified `#[allow]`
//! internally, and give call sites a self-describing name.
//!
//! This is the sampler-only subset of `hytte-services`' `cast` module (#1249):
//! that crate's copy also serves its `audio_native` code (`usize_to_f64`,
//! `f64_to_f32_gain`), which never crossed into `sensors/`, so it stayed put
//! rather than becoming a dependency of this leaf. Duplicating the handful of
//! functions the movers actually call — rather than threading a shared-utils
//! crate through both — is what lets every moved `sensors/*.rs` file keep its
//! `use crate::cast::…` line byte-for-byte identical to before the move.
//!
//! **Do not add helpers here for casts that are semantically dangerous.**
//! Each helper must carry a comment explaining why the loss is acceptable.

/// Convert a `u64` byte count to `f64` for network/disk rate arithmetic
/// (bytes/sec).
///
/// # Precision contract
///
/// `f64` has a 53-bit mantissa, so integers up to 2^53 (≈ 9 PiB) round-trip
/// exactly. A 1-second byte delta on a real NIC or disk is at most a few
/// hundred MiB — well within the exact range. Even at 100 Gbps sustained the
/// delta is ~12.5 GiB ≈ 1.3 × 10^10, safely below 2^53. No precision is lost
/// for this use case.
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
/// The caller (in `disk.rs`) already verifies each digit is in `'0'..='7'`
/// before calling this, and `proc(5)` guarantees only valid byte escapes are
/// emitted, so the value is always ≤ 255 and the low-byte truncation is safe
/// by construction.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn octal_byte_from_u32(v: u32) -> u8 {
    v as u8
}

#[cfg(test)]
mod tests {
    // `u64_to_f64_bytes` carries no dedicated unit test in this module: it is
    // exercised indirectly through `diskio::tests` (`compute_disk_io`'s rate
    // math calls it), and it is unit-tested directly where it also still
    // lives — `hytte-services`' own `cast.rs`, whose `apply_network` and
    // memory-history extractor are its other caller. Duplicating those two
    // tests here as well would double-count coverage without moving anything
    // (the function has two independent callers on two sides of the crate
    // boundary; see `lib.rs`'s and `hytte-services::cast`'s doc comments).
    use super::*;

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
