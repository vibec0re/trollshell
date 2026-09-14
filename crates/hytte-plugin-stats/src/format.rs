//! The two number formats the drawer page needs that the card did not.
//!
//! Both are **deliberate mirrors** of `trollshell/src/components/format.rs`,
//! which is where the native Stats page gets its byte and percentage strings.
//! The plugin cannot link that module — it is `pub(crate)` inside the shell
//! binary, and the shell is the one thing a plugin never links — so the ladder
//! is restated here and pinned against the shell's own recorded outputs
//! (`components/format.rs`'s `fmt_bytes` tests: `1023 → "1023 B"`,
//! `1024 → "1.0 KiB"`, `1_048_575 → "1024.0 KiB"`, `1_048_576 → "1.0 MiB"`,
//! `1_073_741_824 → "1.0 GiB"`, `4_509_715_660 → "4.2 GiB"`).
//!
//! Those six literals are the whole point of the test below: a mirror that
//! asserts against its own arithmetic cannot see a divergence from the thing it
//! mirrors (#1026), so the expectations are copied from the shell's test file
//! rather than derived here.
//!
//! Percentages are not in this module: `crate::card::percent_text` already owns
//! the card's `0.0..=1.0 → "42%"` mapping including the `—` arm, and the page
//! uses that same function so the two surfaces cannot disagree.

/// Bytes as the native Stats page writes them: `GiB` / `MiB` / `KiB` / `B`,
/// one decimal place above the first threshold and none below it.
///
/// Binary units, matching the shell — `1024`, not `1000`.
#[must_use]
pub fn bytes(b: u64) -> String {
    // `u64 as f64` is lossy only past 2^53 bytes (8 EiB); the ladder's own
    // rounding is far coarser than that, and the workspace's pedantic lints
    // want the cast named rather than hidden.
    #[allow(clippy::cast_precision_loss)]
    let f = b as f64;
    if f >= 1_073_741_824.0 {
        format!("{:.1} GiB", f / 1_073_741_824.0)
    } else if f >= 1_048_576.0 {
        format!("{:.1} MiB", f / 1_048_576.0)
    } else if f >= 1024.0 {
        format!("{:.1} KiB", f / 1024.0)
    } else {
        format!("{f:.0} B")
    }
}

/// `used / total (pct%)` — the native Memory row's subtitle, and the same
/// string its Disks rows use per mount.
///
/// A zero `total` is the one case the shell special-cases, and it renders as an
/// em dash rather than as `0 B / 0 B (0%)` or a division by zero.
#[must_use]
pub fn used_of_total(used: u64, total: u64) -> String {
    if total == 0 {
        return "—".to_owned();
    }
    // Both casts are the `bytes` one; the quotient is a ratio of two byte
    // counts and cannot be non-finite here because `total` is non-zero.
    #[allow(clippy::cast_precision_loss)]
    let pct = (used as f64 / total as f64) * 100.0;
    format!("{} / {} ({pct:.0}%)", bytes(used), bytes(total))
}

/// `used / total` as a `0.0..=1.0` fraction, `0.0` when `total` is zero.
///
/// The one place the page turns two byte counts into a meter level, so the
/// clamp and the zero guard exist once.
#[must_use]
pub fn fraction(used: u64, total: u64) -> f32 {
    if total == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let f = used as f32 / total as f32;
    if f.is_nan() { 0.0 } else { f.clamp(0.0, 1.0) }
}

#[cfg(test)]
mod tests {
    use super::{bytes, fraction, used_of_total};

    /// **The mirror**, pinned against the shell's own recorded outputs rather
    /// than against this module's arithmetic — a mirror asserted from its own
    /// constants cannot see a divergence from what it mirrors (#1026).
    ///
    /// **Falsified** by switching any threshold to its decimal sibling
    /// (`1_000_000_000`), which reds the `4_509_715_660` row first.
    #[test]
    fn the_byte_ladder_is_the_shells_byte_ladder() {
        for (input, want) in [
            (0_u64, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1_048_575, "1024.0 KiB"),
            (1_048_576, "1.0 MiB"),
            (1_073_741_824, "1.0 GiB"),
            (4_509_715_660, "4.2 GiB"),
        ] {
            assert_eq!(bytes(input), want, "{input}");
        }
    }

    /// The memory/mount subtitle, including the zero-total arm the shell
    /// renders as an em dash.
    #[test]
    fn used_of_total_reads_like_the_native_row() {
        assert_eq!(
            used_of_total(11_999_999_000, 33_500_000_000),
            "11.2 GiB / 31.2 GiB (36%)",
        );
        assert_eq!(used_of_total(0, 0), "—", "a zero total is not 0 B / 0 B");
        assert_eq!(used_of_total(512, 1024), "512 B / 1.0 KiB (50%)");
    }

    /// The meter level: bounded, total over a zero denominator, and never a
    /// `NaN` (which would defeat the runtime's render dedup forever,
    /// #896/#898).
    #[test]
    fn the_fraction_is_bounded_and_total() {
        assert!((fraction(1, 2) - 0.5).abs() < 1e-6);
        assert!((fraction(0, 0) - 0.0).abs() < f32::EPSILON);
        assert!((fraction(9, 4) - 1.0).abs() < f32::EPSILON, "clamped");
        assert!(fraction(u64::MAX, u64::MAX).is_finite());
    }
}
