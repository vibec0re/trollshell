//! App-usage service — walks `/proc` every ~2 s and exposes the top processes
//! by CPU share and by resident memory, aggregated by process name (`comm`).
//!
//! Tier 1 of #28 ("most expensive apps"). CPU share is a unit-free
//! jiffies-delta ratio — the same technique [`crate::sensors`] uses for
//! aggregate CPU load: a process's `utime + stime` delta over the interval
//! divided by the aggregate `/proc/stat` `cpu` line's total delta, i.e. a
//! fraction of *total* CPU capacity (all cores), `0.0..=1.0`. No
//! `clk_tck`/`sysconf`, hence no `unsafe`.
//!
//! Out of scope here (tracked as follow-ups): cgroup/app grouping so a
//! browser's many PIDs collapse into one *app* row, plus app icons (#38); and
//! the design knobs — CPU scale, layout, a "system" bucket, panel-visibility
//! gating (#42).
//!
//! # Public API
//!
//! ```ignore
//! .with(app_usage::service())              // register once
//! app_usage::top_by_cpu() -> impl Signal<Item = Vec<ProcSample>>
//! app_usage::top_by_mem() -> impl Signal<Item = Vec<ProcSample>>
//! ```

use std::cmp::Reverse;
use std::collections::HashMap;
use std::time::Duration;

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, registry};

/// Resident page size assumed when converting `/proc/<pid>/statm` pages to
/// bytes. 4 KiB on every platform trollshell targets; reading the real value
/// needs `sysconf` (FFI/`unsafe`), which this crate forbids.
const PAGE_SIZE: u64 = 4096;

/// Rows kept in each list.
const TOP_N: usize = 6;

/// Poll period. Heavier than the aggregate `sensors` reads (2 files per PID),
/// so it runs at half that cadence; gating it to "panel visible" is a tracked
/// follow-up (#42).
const POLL: Duration = Duration::from_secs(2);

/// One aggregated process group — all PIDs sharing a `comm`.
#[derive(Clone, Debug)]
pub struct ProcSample {
    /// Process name (`/proc/<pid>/comm`), used as the group key + display name.
    pub name: String,
    /// Share of total CPU capacity across all cores, `0.0..=1.0`.
    pub cpu_frac: f64,
    /// Summed resident set size, bytes.
    pub mem_bytes: u64,
    /// How many PIDs collapsed into this row.
    pub procs: u32,
}

#[doc(hidden)]
pub struct AppUsageHandles {
    pub(crate) by_cpu: Mutable<Vec<ProcSample>>,
    pub(crate) by_mem: Mutable<Vec<ProcSample>>,
}

pub struct AppUsageService;

impl Service for AppUsageService {
    type Handles = AppUsageHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = AppUsageHandles {
            by_cpu: Mutable::new(Vec::new()),
            by_mem: Mutable::new(Vec::new()),
        };
        let by_cpu = handles.by_cpu.clone();
        let by_mem = handles.by_mem.clone();
        rt.spawn(poll_loop(by_cpu, by_mem));
        handles
    }
}

#[must_use]
pub fn service() -> AppUsageService {
    AppUsageService
}

/// Top processes by CPU share (descending), capped to the top N.
pub fn top_by_cpu() -> impl Signal<Item = Vec<ProcSample>> {
    registry::with(|r| {
        r.get::<AppUsageHandles>()
            .expect("app_usage::service() not registered")
            .by_cpu
            .signal_cloned()
    })
}

/// Top processes by resident memory (descending), capped to the top N.
pub fn top_by_mem() -> impl Signal<Item = Vec<ProcSample>> {
    registry::with(|r| {
        r.get::<AppUsageHandles>()
            .expect("app_usage::service() not registered")
            .by_mem
            .signal_cloned()
    })
}

/// Accumulator while folding PIDs into their `comm` group within one sample.
#[derive(Default)]
struct Agg {
    cpu_jiffies: u64,
    mem_bytes: u64,
    procs: u32,
}

async fn poll_loop(by_cpu: Mutable<Vec<ProcSample>>, by_mem: Mutable<Vec<ProcSample>>) {
    // Per-PID cumulative CPU jiffies from the previous sample, and the previous
    // aggregate total CPU jiffies — the two halves of the delta ratio.
    let mut prev_pid: HashMap<u32, u64> = HashMap::new();
    let mut prev_total: u64 = 0;

    loop {
        let total_now = read_total_cpu_jiffies();
        let d_total = total_now.saturating_sub(prev_total);

        let mut cur_pid: HashMap<u32, u64> = HashMap::new();
        let mut groups: HashMap<String, Agg> = HashMap::new();

        for pid in pids() {
            let Some((name, jiffies)) = read_pid_cpu(pid) else {
                continue;
            };
            cur_pid.insert(pid, jiffies);
            // Unseen PIDs delta to themselves → 0, so a freshly-spawned process
            // doesn't spike on its first appearance.
            let delta = jiffies.saturating_sub(prev_pid.get(&pid).copied().unwrap_or(jiffies));
            let g = groups.entry(name).or_default();
            g.cpu_jiffies = g.cpu_jiffies.saturating_add(delta);
            g.mem_bytes = g.mem_bytes.saturating_add(read_pid_rss(pid));
            g.procs = g.procs.saturating_add(1);
        }

        let samples = finalize(groups, d_total);
        by_cpu.set(top_by(&samples, |s| Reverse(OrderedF64(s.cpu_frac))));
        by_mem.set(top_by(&samples, |s| Reverse(s.mem_bytes)));

        prev_pid = cur_pid;
        prev_total = total_now;
        tokio::time::sleep(POLL).await;
    }
}

/// `f64` newtype giving a total order for sorting (the values are finite
/// fractions in `0.0..=1.0`, never `NaN`).
#[derive(Clone, Copy, PartialEq)]
struct OrderedF64(f64);
impl Eq for OrderedF64 {}
impl PartialOrd for OrderedF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrderedF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Turn the per-`comm` accumulators into samples, computing each group's CPU
/// fraction from its summed jiffy-delta over the interval's total.
fn finalize(groups: HashMap<String, Agg>, d_total: u64) -> Vec<ProcSample> {
    groups
        .into_iter()
        .map(|(name, g)| ProcSample {
            name,
            cpu_frac: if d_total == 0 {
                0.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                let frac = g.cpu_jiffies as f64 / d_total as f64;
                frac.clamp(0.0, 1.0)
            },
            mem_bytes: g.mem_bytes,
            procs: g.procs,
        })
        .collect()
}

/// Clone, sort descending by `key`, and keep the top [`TOP_N`].
fn top_by<K: Ord>(samples: &[ProcSample], key: impl Fn(&ProcSample) -> K) -> Vec<ProcSample> {
    let mut v = samples.to_vec();
    v.sort_by_key(|s| key(s));
    v.truncate(TOP_N);
    v
}

fn pids() -> Vec<u32> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.filter_map(std::result::Result::ok)
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()))
        .collect()
}

fn read_pid_cpu(pid: u32) -> Option<(String, u64)> {
    parse_pid_cpu(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

fn read_pid_rss(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/statm"))
        .ok()
        .and_then(|t| parse_rss_pages(&t))
        .map_or(0, |pages| pages.saturating_mul(PAGE_SIZE))
}

fn read_total_cpu_jiffies() -> u64 {
    std::fs::read_to_string("/proc/stat").map_or(0, |t| parse_total_cpu(&t))
}

/// Parse `/proc/<pid>/stat` into `(comm, utime + stime)` (CPU jiffies). `comm`
/// can contain spaces and parentheses, so split on the *last* `)`; after it the
/// whitespace tokens are fields 3.. — `utime` is field 14 (index 11), `stime`
/// field 15 (index 12).
fn parse_pid_cpu(text: &str) -> Option<(String, u64)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let name = text.get(open + 1..close)?.to_string();
    let fields: Vec<&str> = text.get(close + 1..)?.split_ascii_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((name, utime.saturating_add(stime)))
}

/// Resident pages from `/proc/<pid>/statm` (`size resident shared …` — field 1).
fn parse_rss_pages(text: &str) -> Option<u64> {
    text.split_ascii_whitespace().nth(1)?.parse().ok()
}

/// Sum the aggregate `cpu` line (first line) of `/proc/stat` into total jiffies.
fn parse_total_cpu(text: &str) -> u64 {
    text.lines().next().map_or(0, |line| {
        line.split_ascii_whitespace()
            .skip(1)
            .filter_map(|f| f.parse::<u64>().ok())
            .sum()
    })
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn parses_comm_with_spaces_and_parens() {
        // comm = "Web Content (tab)"; after the last ')': state(R) + 10 zeros
        // (idx 0..10) then utime=100 (idx 11), stime=50 (idx 12), trailing junk.
        let stat = "1234 (Web Content (tab)) R 0 0 0 0 0 0 0 0 0 0 100 50 99 99";
        assert_eq!(
            parse_pid_cpu(stat),
            Some(("Web Content (tab)".to_string(), 150))
        );
    }

    #[test]
    fn parse_pid_cpu_rejects_garbage() {
        assert_eq!(parse_pid_cpu("not a stat line"), None);
        assert_eq!(parse_pid_cpu("1 (init) R 0 0"), None); // too few fields
    }

    #[test]
    fn parses_statm_resident() {
        assert_eq!(parse_rss_pages("1000 250 40 1 0 30 0"), Some(250));
        assert_eq!(parse_rss_pages(""), None);
    }

    #[test]
    fn sums_aggregate_cpu_line() {
        let stat = "cpu  10 20 30 40 50\ncpu0 1 2 3 4 5\ncpu1 6 7 8 9 10\n";
        assert_eq!(parse_total_cpu(stat), 150);
        assert_eq!(parse_total_cpu(""), 0);
    }

    #[test]
    fn finalize_computes_fraction_and_top_by_orders() {
        let mut groups: HashMap<String, Agg> = HashMap::new();
        groups.insert(
            "heavy".into(),
            Agg {
                cpu_jiffies: 50,
                mem_bytes: 100,
                procs: 3,
            },
        );
        groups.insert(
            "light".into(),
            Agg {
                cpu_jiffies: 10,
                mem_bytes: 900,
                procs: 1,
            },
        );
        let samples = finalize(groups, 200);

        // CPU: heavy = 50/200 = 0.25 leads; light = 10/200 = 0.05.
        let by_cpu = top_by(&samples, |s| Reverse(OrderedF64(s.cpu_frac)));
        assert_eq!(by_cpu[0].name, "heavy");
        assert!((by_cpu[0].cpu_frac - 0.25).abs() < 1e-9);
        assert_eq!(by_cpu[0].procs, 3);

        // RAM: light (900) outweighs heavy (100).
        let by_mem = top_by(&samples, |s| Reverse(s.mem_bytes));
        assert_eq!(by_mem[0].name, "light");
        assert_eq!(by_mem[0].mem_bytes, 900);
    }

    #[test]
    fn finalize_zero_interval_is_zero_cpu() {
        let mut groups: HashMap<String, Agg> = HashMap::new();
        groups.insert(
            "x".into(),
            Agg {
                cpu_jiffies: 99,
                mem_bytes: 1,
                procs: 1,
            },
        );
        let samples = finalize(groups, 0);
        assert_eq!(samples[0].cpu_frac, 0.0);
    }

    #[test]
    fn top_by_caps_to_top_n() {
        let samples: Vec<ProcSample> = (0..20u32)
            .map(|i| ProcSample {
                name: format!("p{i}"),
                cpu_frac: f64::from(i),
                mem_bytes: u64::from(i),
                procs: 1,
            })
            .collect();
        let top = top_by(&samples, |s| Reverse(s.mem_bytes));
        assert_eq!(top.len(), TOP_N);
        assert_eq!(top[0].name, "p19"); // highest mem first
    }
}
