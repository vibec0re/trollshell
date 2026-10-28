//! `/proc/stat` parsing — CPU jiffy counts and load computation.

use crate::cast::u64_to_f64_count;

use super::{CpuJiffies, CpuLoad};

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

/// The `N` of a `cpuN` label, given the part after `cpu`; `None` when that is
/// not a plain decimal number. (The aggregate line's empty suffix is told
/// apart by [`parse_proc_stat`] before it asks.)
fn cpu_number(suffix: &str) -> Option<u32> {
    if suffix.bytes().all(|b| b.is_ascii_digit()) {
        suffix.parse().ok()
    } else {
        None
    }
}

/// Every `cpu*` line of `/proc/stat`-formatted `text`, in file order, each
/// carrying the CPU its label names: the aggregate `cpu` line
/// (`core: None`), then one `cpuN` line per **online** CPU.
///
/// Split out of [`read_proc_stat`] so the parse is testable against a fixture
/// string rather than the machine the test runs on.
pub(crate) fn parse_proc_stat(text: &str) -> Vec<CpuJiffies> {
    let mut entries = Vec::new();

    for line in text.lines() {
        if !line.starts_with("cpu") {
            // cpu lines are at the top; once we see something else we're done.
            break;
        }
        let mut fields = line.split_ascii_whitespace();
        let Some(suffix) = fields.next().and_then(|label| label.strip_prefix("cpu")) else {
            continue;
        };
        let core = if suffix.is_empty() {
            None
        } else {
            let Some(n) = cpu_number(suffix) else {
                // A label this parser does not understand costs its own
                // line rather than being guessed at.
                continue;
            };
            Some(n)
        };
        let nums: Vec<u64> = fields.map(|f| f.parse::<u64>().unwrap_or(0)).collect();
        if nums.is_empty() {
            continue;
        }
        let (active, total) = active_and_total(&nums);
        entries.push(CpuJiffies {
            core,
            active,
            total,
        });
    }

    entries
}

/// Returns one entry per `cpu*` line of `/proc/stat`, in file order, each
/// labelled with the CPU it counts — see [`CpuJiffies`]. The aggregate `cpu`
/// line is the one with `core: None`.
pub fn read_proc_stat() -> Result<Vec<CpuJiffies>, std::io::Error> {
    Ok(parse_proc_stat(&std::fs::read_to_string("/proc/stat")?))
}

/// The load between two `/proc/stat` samples.
///
/// `/proc/stat` lists only online CPUs, so a core's position in the file is
/// not its identity: when one CPU goes offline, every later one moves down a
/// slot. Cores are therefore paired **by their `cpuN` label**, never by
/// position (#1561):
///
/// - a CPU in both samples gets its own delta;
/// - a CPU only in `now` (it just came online) has no baseline yet, so it
///   reads `0.0` for this tick, and pairs with itself from the next one;
/// - a CPU only in `prev` (it went offline) is dropped.
///
/// The result's [`per_core`](CpuLoad::per_core) is in ascending CPU number,
/// with [`core_ids`](CpuLoad::core_ids) naming each entry. An empty `prev` (the
/// first sample) is the case where every CPU is new: the aggregate reads `0.0`
/// and so does every core. The aggregate `cpu` line pairs with the aggregate
/// line and is unaffected by any of this.
#[must_use]
pub fn compute_cpu_load(prev: &[CpuJiffies], now: &[CpuJiffies]) -> CpuLoad {
    let load_between = |before: Option<&CpuJiffies>, after: &CpuJiffies| -> f64 {
        let Some(before) = before else {
            return 0.0;
        };
        let d_total = after.total.saturating_sub(before.total);
        let d_active = after.active.saturating_sub(before.active);
        if d_total == 0 {
            return 0.0;
        }
        let load = u64_to_f64_count(d_active) / u64_to_f64_count(d_total);
        load.clamp(0.0, 1.0)
    };

    let aggregate = |sample: &[CpuJiffies]| sample.iter().find(|e| e.core.is_none()).copied();
    let overall =
        aggregate(now).map_or(0.0, |after| load_between(aggregate(prev).as_ref(), &after));

    // Each sample's cores in ascending CPU number. The kernel already prints
    // them that way; sorting anyway is what lets the result promise the order
    // without trusting the input to keep it, and lets `before` be searched.
    let cores_of = |sample: &[CpuJiffies]| {
        let mut cores: Vec<(u32, CpuJiffies)> = sample
            .iter()
            .filter_map(|e| e.core.map(|n| (n, *e)))
            .collect();
        cores.sort_by_key(|&(n, _)| n);
        cores.dedup_by_key(|&mut (n, _)| n);
        cores
    };
    let before = cores_of(prev);
    let after = cores_of(now);

    let per_core = after
        .iter()
        .map(|(n, sample)| {
            let paired = before
                .binary_search_by_key(n, |&(m, _)| m)
                .ok()
                .map(|i| &before[i].1);
            load_between(paired, sample)
        })
        .collect();
    let core_ids = after.iter().map(|&(n, _)| n).collect();

    CpuLoad {
        overall,
        per_core,
        core_ids,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One `/proc/stat` fixture line whose `(active, total)` is
    /// `(busy, busy + idle)`: `user` carries the busy jiffies and `idle` the
    /// rest, every other field zero.
    fn line(label: &str, busy: u64, idle: u64) -> String {
        format!("{label} {busy} 0 0 {idle} 0 0 0 0 0 0\n")
    }

    /// The per-core loads of `load`, asserting first that each is a real
    /// fraction — what a wrong pairing through the saturating maths does not
    /// promise.
    fn fractions(load: &CpuLoad) -> &[f64] {
        assert!(
            load.per_core
                .iter()
                .all(|l| l.is_finite() && (0.0..=1.0).contains(l)),
            "every per-core load is a fraction: {:?}",
            load.per_core
        );
        assert_eq!(
            load.per_core.len(),
            load.core_ids.len(),
            "one CPU number per load"
        );
        &load.per_core
    }

    fn close(got: &[f64], want: &[f64]) -> bool {
        got.len() == want.len() && got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-9)
    }

    /// A `/proc/stat` line whose numeric fields are garbage (non-numeric
    /// tokens) must not panic — the parser uses `unwrap_or(0)` and yields a
    /// valid `(0, 0)` entry rather than aborting.
    #[test]
    #[allow(clippy::float_cmp)]
    fn read_proc_stat_non_numeric_fields_skip_gracefully() {
        let parsed = parse_proc_stat("cpu x y z\ncpu0 a b c d\n");
        assert_eq!(
            parsed,
            vec![
                CpuJiffies {
                    core: None,
                    active: 0,
                    total: 0
                },
                CpuJiffies {
                    core: Some(0),
                    active: 0,
                    total: 0
                },
            ],
            "garbage fields read as zero jiffies, not a panic"
        );

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

    /// Each line keeps the CPU its label names; a label that is not `cpu` or
    /// `cpuN` costs its own line, and the first non-`cpu` line ends the scan.
    #[test]
    fn parse_reads_each_lines_cpu_label() {
        let text = format!(
            "{}{}{}cpufoo 1 2 3 4\n{}intr 12345\ncpu9 1 0 0 1\n",
            line("cpu", 30, 70),
            line("cpu0", 10, 30),
            line("cpu1", 20, 40),
            line("cpu12", 5, 5),
        );
        let parsed = parse_proc_stat(&text);
        let labels: Vec<Option<u32>> = parsed.iter().map(|e| e.core).collect();
        assert_eq!(labels, vec![None, Some(0), Some(1), Some(12)]);
        assert_eq!((parsed[3].active, parsed[3].total), (5, 10));
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
        let zero = CpuJiffies {
            core: None,
            active: 0,
            total: 0,
        };
        let load = compute_cpu_load(&[zero], &[zero]);
        assert_eq!(
            load.overall, 0.0,
            "zero d_total must yield 0.0, not NaN/panic"
        );
    }

    /// Same CPUs in both samples: every core is paired with itself, exactly
    /// as positional pairing always did when nothing was hotplugged.
    #[test]
    fn same_labels_pair_like_with_like() {
        let prev = parse_proc_stat(&format!(
            "{}{}{}",
            line("cpu", 0, 0),
            line("cpu0", 0, 0),
            line("cpu1", 100, 100),
        ));
        let now = parse_proc_stat(&format!(
            "{}{}{}",
            line("cpu", 60, 140),
            line("cpu0", 50, 50),
            line("cpu1", 110, 190),
        ));
        let load = compute_cpu_load(&prev, &now);
        assert!(close(fractions(&load), &[0.5, 0.1]), "{:?}", load.per_core);
        assert_eq!(load.core_ids, vec![0, 1]);
        assert!((load.overall - 0.3).abs() < 1e-9);
        // …and the cold first sample reads one idle core per CPU.
        let cold = compute_cpu_load(&[], &now);
        assert!(close(fractions(&cold), &[0.0, 0.0]));
        assert_eq!(cold.core_ids, vec![0, 1]);
    }

    /// A core going offline shifts every later entry down one slot; pairing by
    /// position would report core 2's old numbers against core 3's new ones.
    /// By label, the survivors keep their own real deltas through the change.
    #[test]
    fn hotplug_does_not_misalign_cores() {
        // aggregate + cpu0..cpu2 (cpu1 busy history), then cpu1 goes offline.
        let prev = parse_proc_stat(&format!(
            "{}{}{}{}",
            line("cpu", 0, 0),
            line("cpu0", 0, 0),
            line("cpu1", 900, 100),
            line("cpu2", 0, 0),
        ));
        let now = parse_proc_stat(&format!(
            "{}{}{}",
            line("cpu", 10, 90),
            line("cpu0", 0, 100),
            line("cpu2", 75, 25),
        ));
        let load = compute_cpu_load(&prev, &now);
        assert_eq!(load.core_ids, vec![0, 2], "cpu1 is dropped");
        // Positionally, cpu2 would be paired with cpu1's 900/1000 and read 0.
        assert!(
            close(fractions(&load), &[0.0, 0.75]),
            "each survivor keeps its own delta: {:?}",
            load.per_core
        );
        assert!((load.overall - 0.1).abs() < 1e-9, "aggregate still valid");
        // The next tick pairs like with like again.
        let later = parse_proc_stat(&format!(
            "{}{}{}",
            line("cpu", 20, 180),
            line("cpu0", 50, 150),
            line("cpu2", 75, 125),
        ));
        let load = compute_cpu_load(&now, &later);
        assert!(close(fractions(&load), &[0.5, 0.0]), "{:?}", load.per_core);
    }

    /// **#1561 bullet 3**: cpu2 goes offline and cpu4 comes online inside one
    /// interval, so the core count is unchanged. The count-only check this
    /// replaced saw nothing and paired by position: cpu3's counts against
    /// cpu2's, cpu4's against cpu3's.
    #[test]
    fn offline_and_online_in_one_interval_pairs_by_label() {
        let prev = parse_proc_stat(&format!(
            "{}{}{}{}{}",
            line("cpu", 1_200, 2_800),
            line("cpu0", 100, 900),
            line("cpu1", 200, 800),
            // cpu2: a busy history, which positional pairing would subtract
            // from cpu3's counts.
            line("cpu2", 900, 100),
            line("cpu3", 300, 700),
        ));
        let now = parse_proc_stat(&format!(
            "{}{}{}{}{}",
            line("cpu", 1_500, 2_900),
            line("cpu0", 150, 950),
            line("cpu1", 200, 900),
            // cpu3 spent the whole interval busy.
            line("cpu3", 400, 700),
            // cpu4 is back from an earlier online stint: large counters that
            // mean nothing against any of `prev`'s CPUs.
            line("cpu4", 2_000, 100),
        ));
        let load = compute_cpu_load(&prev, &now);
        assert_eq!(load.core_ids, vec![0, 1, 3, 4]);
        let per_core = fractions(&load);
        assert!(
            close(per_core, &[0.5, 0.0, 1.0, 0.0]),
            "cpu0/1/3 read their own deltas and the new cpu4 reads a fresh \
             baseline's 0, not cpu3's numbers: {per_core:?}"
        );
        assert!((load.overall - 0.75).abs() < 1e-9, "aggregate unaffected");

        // From the next tick cpu4 has a baseline of its own.
        let later = parse_proc_stat(&format!(
            "{}{}{}{}{}",
            line("cpu", 1_700, 3_100),
            line("cpu0", 150, 1_050),
            line("cpu1", 200, 1_000),
            line("cpu3", 400, 800),
            line("cpu4", 2_080, 120),
        ));
        let load = compute_cpu_load(&now, &later);
        assert!(close(fractions(&load), &[0.0, 0.0, 0.0, 0.8]));
    }

    /// **The restart key**: a per-core history restarts when
    /// [`CpuLoad::core_ids`] changes. A relabel at an unchanged core count
    /// must move that key exactly as a count change does, and an unchanged set
    /// of CPUs must not.
    #[test]
    fn a_relabel_moves_the_restart_key_as_a_count_change_does() {
        let sample = |labels: &[&str]| {
            parse_proc_stat(
                &labels
                    .iter()
                    .fold(line("cpu", 0, 0), |text, label| text + &line(label, 10, 10)),
            )
        };
        let a = sample(&["cpu0", "cpu1", "cpu2", "cpu3"]);
        let steady = sample(&["cpu0", "cpu1", "cpu2", "cpu3"]);
        let relabelled = sample(&["cpu0", "cpu1", "cpu3", "cpu4"]);
        let shrunk = sample(&["cpu0", "cpu1", "cpu3"]);

        let base = compute_cpu_load(&[], &a);
        let same = compute_cpu_load(&a, &steady);
        let relabel = compute_cpu_load(&steady, &relabelled);
        let count = compute_cpu_load(&relabelled, &shrunk);

        assert_eq!(same.core_ids, base.core_ids, "same CPUs: no restart");
        assert_eq!(
            relabel.per_core.len(),
            same.per_core.len(),
            "the relabel keeps the count, so a width check cannot see it"
        );
        assert_ne!(
            relabel.core_ids, same.core_ids,
            "…but the restart key moves for it"
        );
        assert_eq!(relabel.core_ids, vec![0, 1, 3, 4]);
        assert_ne!(
            count.core_ids, relabel.core_ids,
            "a count change moves the key too"
        );
        assert_eq!(count.core_ids, vec![0, 1, 3]);
    }

    /// CPUs can be offline from boot, so the labels need not be `0..n`: the
    /// first sample already names the real CPUs, and pairing never assumes a
    /// contiguous range. The result is in ascending CPU number even if the
    /// input is not.
    #[test]
    fn non_contiguous_labels_at_boot() {
        let boot = parse_proc_stat(&format!(
            "{}{}{}{}",
            line("cpu", 0, 0),
            line("cpu0", 0, 0),
            line("cpu1", 0, 0),
            line("cpu3", 0, 0),
        ));
        let cold = compute_cpu_load(&[], &boot);
        assert_eq!(cold.core_ids, vec![0, 1, 3]);
        assert!(close(fractions(&cold), &[0.0, 0.0, 0.0]));

        let mut next = parse_proc_stat(&format!(
            "{}{}{}{}",
            line("cpu", 60, 240),
            line("cpu0", 10, 90),
            line("cpu1", 20, 80),
            line("cpu3", 30, 70),
        ));
        let load = compute_cpu_load(&boot, &next);
        assert_eq!(load.core_ids, vec![0, 1, 3]);
        assert!(close(fractions(&load), &[0.1, 0.2, 0.3]));

        next.reverse();
        let reordered = compute_cpu_load(&boot, &next);
        assert_eq!(reordered.core_ids, vec![0, 1, 3]);
        assert!(close(fractions(&reordered), &[0.1, 0.2, 0.3]));
        assert!((reordered.overall - 0.2).abs() < 1e-9);
    }
}
