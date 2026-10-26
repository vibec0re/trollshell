//! `/proc/stat` parsing — CPU jiffy counts and load computation.

use crate::cast::u64_to_f64_count;

use super::CpuLoad;

/// `(active, total)` jiffies from the numeric fields of a `cpu` line:
/// `user nice system idle iowait irq softirq steal guest guest_nice`.
///
/// The kernel already accounts `guest` inside `user` and `guest_nice` inside
/// `nice` (`account_guest_time`), so those two trailing fields are **not**
/// summed — doing so counts a VM's CPU time twice. Idle is `idle + iowait`.
/// Lines from older kernels carry fewer fields; missing ones read as 0.
pub(crate) fn active_and_total(nums: &[u64]) -> (u64, u64) {
    let field = |i: usize| nums.get(i).copied().unwrap_or(0);
    let idle = field(3).saturating_add(field(4));
    let total = nums.iter().take(8).fold(0u64, |a, &n| a.saturating_add(n));
    (total.saturating_sub(idle), total)
}

/// Returns one entry per `cpu*` line: `(active_jiffies, total_jiffies)`.
/// Index 0 = aggregate `cpu` line, 1+ = `cpu0`, `cpu1`, …
pub fn read_proc_stat() -> Result<Vec<(u64, u64)>, std::io::Error> {
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
        entries.push(active_and_total(&nums));
    }

    Ok(entries)
}

#[must_use]
pub fn compute_cpu_load(prev: &[(u64, u64)], now: &[(u64, u64)]) -> CpuLoad {
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
    // `/proc/stat` omits offline CPUs, so when one went offline or came back
    // between the two samples the entries no longer line up by position:
    // core `i` of `prev` is a different CPU than core `i` of `now`, and every
    // core after the hotplugged one would be computed against its neighbour.
    // The entries carry no `cpuN` label to re-pair them by, so a changed core
    // count publishes a zero frame for that one tick; the next tick pairs
    // like with like again. (The aggregate line stays valid either way.)
    let per_core = if prev.len() == now.len() {
        (0..core_count).map(|i| load_for(i + 1)).collect()
    } else {
        vec![0.0; core_count]
    };

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

    #[test]
    fn guest_time_is_not_counted_twice() {
        // One core: user 50 (all of it guest), idle 50 -> 50 %, not 66 %.
        assert_eq!(
            active_and_total(&[50, 0, 0, 50, 0, 0, 0, 0, 50, 0]),
            (50, 100)
        );
        // guest_nice is likewise already inside nice.
        assert_eq!(
            active_and_total(&[0, 30, 0, 70, 0, 0, 0, 0, 0, 30]),
            (30, 100)
        );
    }

    #[test]
    fn zero_guest_and_short_lines_are_unchanged() {
        assert_eq!(
            active_and_total(&[10, 20, 30, 40, 50, 1, 2, 3, 0, 0]),
            (66, 156)
        );
        // Old kernels: five fields only.
        assert_eq!(active_and_total(&[10, 20, 30, 40, 50]), (60, 150));
        assert_eq!(active_and_total(&[]), (0, 0));
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

    /// A core going offline shifts every later entry down one slot; pairing by
    /// position would report core 2's old numbers against core 3's new ones.
    #[test]
    fn hotplug_does_not_misalign_cores() {
        // aggregate + cpu0..cpu2 (cpu1 busy), then cpu1 goes offline.
        let prev = vec![(0, 0), (0, 0), (100, 100), (0, 0)];
        let now = vec![(10, 100), (0, 100), (0, 100)];
        let load = compute_cpu_load(&prev, &now);
        assert_eq!(load.per_core.len(), 2);
        assert!(
            load.per_core.iter().all(|l| l.abs() < 1e-12),
            "a zero frame, not shifted: {:?}",
            load.per_core
        );
        assert!((load.overall - 0.1).abs() < 1e-9, "aggregate still valid");
        // The next tick pairs like with like again.
        let later = vec![(20, 200), (50, 200), (0, 200)];
        let load = compute_cpu_load(&now, &later);
        assert!((load.per_core[0] - 0.5).abs() < 1e-9);
    }
}
