//! `/proc/meminfo` parsing — memory usage snapshot.

use super::Memory;

pub(super) fn read_proc_meminfo() -> Result<Memory, std::io::Error> {
    let text = std::fs::read_to_string("/proc/meminfo")?;
    Ok(parse_meminfo(&text))
}

pub(super) fn parse_meminfo(text: &str) -> Memory {
    let mut total_kb: u64 = 0;
    let mut free_kb: u64 = 0;
    let mut available_kb: u64 = 0;
    let mut swap_total_kb: u64 = 0;
    let mut swap_free_kb: u64 = 0;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemFree:") {
            free_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("SwapTotal:") {
            swap_total_kb = parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("SwapFree:") {
            swap_free_kb = parse_kb(rest);
        }
    }

    let total = total_kb * 1024;
    let free = free_kb * 1024;
    let available = available_kb * 1024;
    let used = total.saturating_sub(available);
    let swap_total = swap_total_kb * 1024;
    let swap_free = swap_free_kb * 1024;
    let swap_used = swap_total.saturating_sub(swap_free);

    Memory {
        total,
        free,
        available,
        used,
        swap_used,
        swap_total,
    }
}

/// Parse a `/proc/meminfo` value field like `"  16331836 kB"` → `16331836`.
fn parse_kb(s: &str) -> u64 {
    s.split_ascii_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_meminfo_extracts_swap_fields() {
        let text = "\
MemTotal:       16331836 kB
MemFree:         1234567 kB
MemAvailable:    8000000 kB
SwapTotal:       8388604 kB
SwapFree:        4194302 kB
";
        let m = parse_meminfo(text);
        assert_eq!(m.total, 16_331_836 * 1024);
        assert_eq!(m.swap_total, 8_388_604 * 1024);
        assert_eq!(m.swap_used, 4_194_302 * 1024);
    }

    #[test]
    fn parse_meminfo_zero_swap_when_missing() {
        let text = "\
MemTotal:       16331836 kB
MemFree:         1234567 kB
MemAvailable:    8000000 kB
";
        let m = parse_meminfo(text);
        assert_eq!(m.swap_total, 0);
        assert_eq!(m.swap_used, 0);
    }

    /// Malformed `/proc/meminfo` (completely empty) must produce a zeroed
    /// `Memory` struct rather than panicking.
    #[test]
    fn parse_meminfo_empty_input_yields_zero() {
        let m = parse_meminfo("");
        assert_eq!(m.total, 0);
        assert_eq!(m.free, 0);
        assert_eq!(m.available, 0);
        assert_eq!(m.used, 0);
        assert_eq!(m.swap_total, 0);
        assert_eq!(m.swap_used, 0);
    }

    /// `/proc/meminfo` with unrecognised field names must not panic — unknown
    /// lines are simply ignored and the known fields stay at their defaults.
    #[test]
    fn parse_meminfo_unknown_fields_ignored() {
        let text = "\
GarbageField:     99999 kB
AnotherJunk:          0 kB
MemTotal:       16331836 kB
";
        let m = parse_meminfo(text);
        assert_eq!(m.total, 16_331_836 * 1024);
        assert_eq!(m.free, 0);
    }

    /// `/proc/meminfo` with numeric fields that are too large for `u64` must
    /// not panic — `parse_kb` uses `.parse().ok().unwrap_or(0)` and falls
    /// back to zero.
    #[test]
    fn parse_meminfo_overflow_value_falls_back_to_zero() {
        // A value larger than u64::MAX cannot parse as u64; must yield 0.
        let text = "MemTotal:       99999999999999999999999999999 kB\n";
        let m = parse_meminfo(text);
        assert_eq!(m.total, 0, "unparseable MemTotal must fall back to 0");
    }
}
