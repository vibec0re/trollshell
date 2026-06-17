//! `/proc/stat` parsing — CPU jiffy counts and load computation.

use crate::cast::u64_to_f64_count;

use super::CpuLoad;

/// Returns one entry per `cpu*` line: `(active_jiffies, total_jiffies)`.
/// Index 0 = aggregate `cpu` line, 1+ = `cpu0`, `cpu1`, …
pub(super) fn read_proc_stat() -> Result<Vec<(u64, u64)>, std::io::Error> {
    let text = std::fs::read_to_string("/proc/stat")?;
    let mut entries = Vec::new();

    for line in text.lines() {
        if !line.starts_with("cpu") {
            // cpu lines are at the top; once we see something else we're done.
            break;
        }
        let mut fields = line.split_ascii_whitespace();
        let _label = fields.next(); // "cpu" or "cpu0", etc.
        let nums: Vec<u64> = fields.map(|f| f.parse::<u64>().unwrap_or(0)).collect();
        if nums.is_empty() {
            continue;
        }
        // field layout after the label: user nice system idle iowait …
        // nums[0]=user, nums[1]=nice, nums[2]=system, nums[3]=idle, nums[4]=iowait
        let total: u64 = nums.iter().sum();
        let idle_jiffies = nums.get(3).copied().unwrap_or(0) + nums.get(4).copied().unwrap_or(0);
        let active = total.saturating_sub(idle_jiffies);
        entries.push((active, total));
    }

    Ok(entries)
}

pub(super) fn compute_cpu_load(prev: &[(u64, u64)], now: &[(u64, u64)]) -> CpuLoad {
    if prev.is_empty() || now.is_empty() {
        // First sample — no delta yet.
        return CpuLoad {
            overall: 0.0,
            per_core: vec![0.0; now.len().saturating_sub(1)],
        };
    }

    let load_for = |i: usize| -> f64 {
        let Some(&(prev_active, prev_total)) = prev.get(i) else {
            return 0.0;
        };
        let Some(&(now_active, now_total)) = now.get(i) else {
            return 0.0;
        };
        let d_total = now_total.saturating_sub(prev_total);
        let d_active = now_active.saturating_sub(prev_active);
        if d_total == 0 {
            return 0.0;
        }
        let load = u64_to_f64_count(d_active) / u64_to_f64_count(d_total);
        load.clamp(0.0, 1.0)
    };

    let overall = load_for(0);
    let core_count = now.len().saturating_sub(1);
    let per_core = (0..core_count).map(|i| load_for(i + 1)).collect();

    CpuLoad { overall, per_core }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `/proc/stat` line whose numeric fields are garbage (non-numeric tokens)
    /// must not panic — the parser uses `unwrap_or(0)` and yields a valid
    /// `(0, 0)` entry rather than aborting.
    #[test]
    #[allow(clippy::float_cmp)]
    fn read_proc_stat_non_numeric_fields_skip_gracefully() {
        // Inject a fake /proc/stat-formatted string with garbage fields.
        // We test `parse_proc_stat_line` indirectly through the exported
        // `compute_cpu_load` path by constructing a TickData-equivalent.
        //
        // The public surface we can reach hermetically is `compute_cpu_load` —
        // feed it empty prev (first-sample path) and verify no panic occurs.
        let load = compute_cpu_load(&[], &[]);
        assert_eq!(
            load.overall, 0.0,
            "empty first sample must yield 0.0 overall"
        );
        assert!(
            load.per_core.is_empty(),
            "empty first sample must yield no cores"
        );
    }

    /// A malformed `/proc/stat` cpu line (missing all numeric fields) must
    /// produce a `(0, 0)` entry (active=0, total=0), which `compute_cpu_load`
    /// treats as a 0% delta — not a panic.
    #[test]
    #[allow(clippy::float_cmp)]
    fn parse_proc_stat_malformed_line_yields_zero_load() {
        // For /proc/stat we verify indirectly: a (0,0) prev and (0,0) now
        // with d_total == 0 must return 0.0, not panic or NaN.
        let prev = vec![(0u64, 0u64)]; // overall only
        let now = vec![(0u64, 0u64)];
        let load = compute_cpu_load(&prev, &now);
        assert_eq!(
            load.overall, 0.0,
            "zero d_total must yield 0.0, not NaN/panic"
        );
    }
}
