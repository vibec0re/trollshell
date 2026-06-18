//! App-usage service — walks `/proc` every ~2 s and exposes the top processes
//! by CPU share and by resident memory, aggregated by **systemd app-scope**
//! (`/proc/<pid>/cgroup`). PIDs that don't live inside a recognised app scope
//! collapse into a single "System" bucket.
//!
//! v2 of #38 ("most expensive apps: group by systemd slice + app icons").
//! v1 aggregated by `comm` (kernel process name); this cut re-keys by the
//! app-id parsed from the cgroup leaf, which makes browsers, Electron apps,
//! Flatpaks, and multi-process apps all collapse into one row.
//!
//! # Cgroup leaf parsing
//!
//! systemd places apps under scopes whose leaf segment follows one of:
//! - `app-<id>.scope` (plain user app, e.g. `app-firefox.scope`)
//! - `app-<id>-<random>.scope` (instantiated unit, e.g. `app-Alacritty-abc123.scope`)
//! - `app-flatpak-<id>-<n>.scope` (Flatpak, e.g. `app-flatpak-org.gnome.Nautilus-1234.scope`)
//!
//! The match is heuristic — the scope id isn't always exactly the `.desktop`
//! app id, so `DesktopAppInfo` lookups may miss. Misses are handled gracefully.
//!
//! # Panel-gating
//!
//! The poller is gated on Stats-drawer visibility via [`set_active`]: it parks
//! (walking nothing) while the panel is hidden and resumes — taking a fresh
//! sample immediately — when it reappears (#50, item 5 of #42).
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

use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_reactive::{Service, registry};

/// Resident page size assumed when converting `/proc/<pid>/statm` pages to
/// bytes. 4 KiB on every platform trollshell targets; reading the real value
/// needs `sysconf` (FFI/`unsafe`), which this crate forbids.
const PAGE_SIZE: u64 = 4096;

/// Rows kept in each list.
const TOP_N: usize = 6;

/// Poll period. Heavier than the aggregate `sensors` reads (2 files per PID),
/// so it runs at half that cadence; it's additionally gated to "Stats panel
/// visible" via [`set_active`] so it idles entirely when no one's looking.
const POLL: Duration = Duration::from_secs(2);

/// Synthetic group key for all pids that don't belong to a recognised app scope.
const SYSTEM_BUCKET: &str = "System";

/// One aggregated process group — all PIDs sharing an app-scope id (v2) or
/// process name fallback. The `app_id` is `None` for the "System" bucket and
/// for comm-fallback groups.
#[derive(Clone, Debug)]
pub struct ProcSample {
    /// Display name: the app-id parsed from the cgroup scope, or `"System"` for
    /// the aggregate non-app bucket.
    pub name: String,
    /// The raw app-id from the cgroup scope leaf (e.g. `"org.gnome.Nautilus"`),
    /// used by the UI to resolve a `DesktopAppInfo` and its icon. `None` for
    /// the "System" bucket.
    pub app_id: Option<String>,
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
    /// Gate for the `/proc` poller. While `false`, the poll loop parks and
    /// walks nothing; flipping it back to `true` resumes sampling immediately
    /// (the loop `select!`s on this so reactivation isn't delayed a full tick).
    ///
    /// Defaults to `true` so the first sample is taken eagerly at startup —
    /// the top-apps lists are then already populated the instant the Stats
    /// drawer opens, and `set_active(false)` parks the poller once the binary
    /// reports that panel is hidden. See [`set_active`].
    pub(crate) active: Mutable<bool>,
}

pub struct AppUsageService;

impl Service for AppUsageService {
    type Handles = AppUsageHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = AppUsageHandles {
            by_cpu: Mutable::new(Vec::new()),
            by_mem: Mutable::new(Vec::new()),
            active: Mutable::new(true),
        };
        let by_cpu = handles.by_cpu.clone();
        let by_mem = handles.by_mem.clone();
        let active = handles.active.clone();
        rt.spawn(poll_loop(by_cpu, by_mem, active));
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

/// Gate the `/proc` poller: `true` resumes ~2 s sampling (immediately taking a
/// fresh sample), `false` parks it so it walks nothing while the Stats drawer
/// panel — the only consumer of these lists — is hidden.
///
/// Fire-and-forget command: the binary wires the Stats-drawer-visibility signal
/// to this so the always-on poller idles when no one's looking (#50, realizing
/// item 5 of #42). A no-op `set` to the same value is skipped to avoid spurious
/// loop wakeups.
pub fn set_active(active: bool) {
    registry::with(|r| {
        let handle = &r
            .get::<AppUsageHandles>()
            .expect("app_usage::service() not registered")
            .active;
        if handle.get() != active {
            handle.set(active);
        }
    });
}

/// Accumulator while folding PIDs into their app-scope group within one sample.
#[derive(Default)]
struct Agg {
    cpu_jiffies: u64,
    mem_bytes: u64,
    procs: u32,
    /// The app-id (if any) for this group. All entries in a group share the
    /// same app-id (or `None` for the System bucket).
    app_id: Option<String>,
}

async fn poll_loop(
    by_cpu: Mutable<Vec<ProcSample>>,
    by_mem: Mutable<Vec<ProcSample>>,
    active: Mutable<bool>,
) {
    // Per-PID cumulative CPU jiffies from the previous sample, and the previous
    // aggregate total CPU jiffies — the two halves of the delta ratio.
    let mut prev_pid: HashMap<u32, u64> = HashMap::new();
    let mut prev_total: u64 = 0;

    loop {
        // Park (walking nothing) while gated inactive. `wait_for(true)` resolves
        // as soon as `set_active(true)` lands — `Mutable::signal()` replays the
        // current value on first poll, so if we've already been reactivated by
        // the time we get here it returns immediately, with no lost wakeup.
        // Reactivation is thus instant rather than waiting out a sleep tick.
        //
        // The CPU fractions are jiffy *deltas* over the inter-sample interval,
        // so the resume sample's delta spans the whole parked gap. That's
        // fine: `prev_total` grows by the same wall-clock span as the per-PID
        // jiffies, so the ratio stays a valid "share of total capacity" — a
        // process that was idle the whole time still reads ~0.
        if !active.get() {
            let _ = active.signal().wait_for(true).await;
        }
        let total_now = read_total_cpu_jiffies();
        let d_total = total_now.saturating_sub(prev_total);

        let mut cur_pid: HashMap<u32, u64> = HashMap::new();
        // Key: group name (app-id or SYSTEM_BUCKET).
        let mut groups: HashMap<String, Agg> = HashMap::new();

        for pid in pids() {
            let Some((cpu_jiffies, rss)) = read_pid_stats(pid) else {
                continue;
            };
            cur_pid.insert(pid, cpu_jiffies);
            // Unseen PIDs delta to themselves → 0, so a freshly-spawned process
            // doesn't spike on its first appearance.
            let delta =
                cpu_jiffies.saturating_sub(prev_pid.get(&pid).copied().unwrap_or(cpu_jiffies));

            // Determine the group key from the cgroup leaf; fall back to System.
            // A pid that exits between the stat read and the cgroup read returns
            // None here and is harmlessly misattributed to System for that tick
            // (no panic, no double-count).
            let app_id = read_pid_app_id(pid);
            let group_key = app_id.as_deref().unwrap_or(SYSTEM_BUCKET).to_string();

            let g = groups.entry(group_key).or_insert_with(|| Agg {
                app_id: app_id.clone(),
                ..Agg::default()
            });
            g.cpu_jiffies = g.cpu_jiffies.saturating_add(delta);
            g.mem_bytes = g.mem_bytes.saturating_add(rss);
            g.procs = g.procs.saturating_add(1);
        }

        let samples = finalize(groups, d_total);
        by_cpu.set(top_by(&samples, |s| Reverse(OrderedF64(s.cpu_frac))));
        by_mem.set(top_by(&samples, |s| Reverse(s.mem_bytes)));

        prev_pid = cur_pid;
        prev_total = total_now;
        // Sleep the inter-sample interval, but bail out early if we get gated
        // inactive mid-wait — no point holding the timer when parked. The
        // top-of-loop park then handles the resume edge.
        tokio::select! {
            () = tokio::time::sleep(POLL) => {}
            _ = active.signal().wait_for(false) => {}
        }
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

/// Turn the per-scope accumulators into samples, computing each group's CPU
/// fraction from its summed jiffy-delta over the interval's total.
fn finalize(groups: HashMap<String, Agg>, d_total: u64) -> Vec<ProcSample> {
    groups
        .into_iter()
        .map(|(name, g)| ProcSample {
            name,
            app_id: g.app_id,
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

/// Read both CPU jiffies and RSS for a PID in one call, returning `None` if
/// either `/proc/<pid>/stat` or `/proc/<pid>/statm` is unreadable.
fn read_pid_stats(pid: u32) -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, jiffies) = parse_pid_cpu(&stat)?;
    let rss = std::fs::read_to_string(format!("/proc/{pid}/statm"))
        .ok()
        .and_then(|t| parse_rss_pages(&t))
        .map_or(0, |pages| pages.saturating_mul(PAGE_SIZE));
    Some((jiffies, rss))
}

/// Read `/proc/<pid>/cgroup` and extract the app-id from a systemd app scope.
/// Returns `None` for kernel threads, pids outside an app scope, or read errors.
fn read_pid_app_id(pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    parse_app_id_from_cgroup(&text)
}

/// Parse the app-id from a `/proc/<pid>/cgroup` file.
///
/// The relevant line looks like:
/// ```text
/// 0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-firefox.scope
/// ```
/// or for Flatpak:
/// ```text
/// 0::/user.slice/.../app.slice/app-flatpak-org.gnome.Nautilus-1234.scope
/// ```
///
/// We scan all lines for a segment ending in `.scope` that starts with `app-`,
/// and extract the id from the leaf segment.
pub(crate) fn parse_app_id_from_cgroup(text: &str) -> Option<String> {
    for line in text.lines() {
        // Each line: `<hierarchy>:<controllers>:<path>`
        // Use `else { continue }` rather than `?` so a single malformed or
        // empty line is skipped instead of returning `None` for the whole file.
        let Some(path) = line.splitn(3, ':').nth(2) else {
            continue;
        };
        // Find the last path segment that ends in `.scope` (case-insensitive
        // per clippy::case_sensitive_file_extension_comparisons). Use `rfind`
        // (= `filter` + `next_back` in one call, per clippy::filter_next).
        if let Some(leaf) = path.split('/').rfind(|s| {
            std::path::Path::new(s)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("scope"))
        }) && let Some(id) = parse_scope_leaf(leaf)
        {
            return Some(id);
        }
    }
    None
}

/// Extract the app-id from a systemd scope leaf name.
///
/// Handles:
/// - `app-<id>.scope` → `<id>`
/// - `app-<id>-<hex-or-random>.scope` → `<id>` (strip the trailing `-<random>`)
/// - `app-flatpak-<id>-<n>.scope` → `<id>` (the Flatpak app id)
///
/// Returns `None` if the leaf doesn't match the `app-` prefix pattern.
pub(crate) fn parse_scope_leaf(leaf: &str) -> Option<String> {
    // Must start with "app-" and end with ".scope"
    let inner = leaf.strip_prefix("app-")?.strip_suffix(".scope")?;

    // Flatpak form: "flatpak-<app-id>-<instance-number>"
    // The app-id contains dots (e.g. org.gnome.Nautilus) and the trailing
    // part is "-<digits>". Strip "flatpak-" prefix and the trailing "-<digits>".
    if let Some(flatpak_rest) = inner.strip_prefix("flatpak-") {
        // Strip trailing `-<digits>` (the instance number).
        // The app-id may also contain hyphens, so we only strip when the last
        // component is purely numeric.
        return Some(strip_trailing_numeric_component(flatpak_rest).to_string());
    }

    // Plain and instantiated form: "<id>" or "<id>-<random>"
    // Heuristic: if the last hyphen-separated component looks like a random
    // suffix (all hex chars, 8+ chars, with at least one digit; or all digits),
    // strip it.
    Some(strip_trailing_random_component(inner).to_string())
}

/// Strip a trailing `-<digits>` from `s` (used for Flatpak instance numbers).
fn strip_trailing_numeric_component(s: &str) -> &str {
    if let Some(pos) = s.rfind('-') {
        let suffix = &s[pos + 1..];
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
            return &s[..pos];
        }
    }
    s
}

/// Strip a trailing `-<random>` from `s` where `<random>` looks like a
/// generated suffix (all hex, 8+ chars with at least one digit) or a
/// PID-like suffix (all digits).
///
/// The hex branch requires **8+ chars and at least one digit** to avoid
/// false positives on short hex-looking words that are part of a real app-id
/// (e.g. `decade`, `facade`, `gnome-decade` — 6-char pure-alpha words with
/// all-hex letters are NOT stripped). Real systemd random suffixes are long
/// blobs that virtually always contain a digit.
///
/// This keeps the app-id intact for names like `app-org.x.Y-1234.scope`
/// where the trailing part is an instance number, while leaving alone names
/// like `app-MyApp.scope` (no suffix).
fn strip_trailing_random_component(s: &str) -> &str {
    if let Some(pos) = s.rfind('-') {
        let suffix = &s[pos + 1..];
        let is_numeric = !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit());
        let is_hex_random = suffix.len() >= 8
            && suffix.chars().all(|c| c.is_ascii_hexdigit())
            && suffix.chars().any(|c| c.is_ascii_digit());
        if is_numeric || is_hex_random {
            return &s[..pos];
        }
    }
    s
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
fn read_total_cpu_jiffies() -> u64 {
    std::fs::read_to_string("/proc/stat").map_or(0, |t| parse_total_cpu(&t))
}

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

    // ── cgroup / scope-leaf parsing ──────────────────────────────────────────

    #[test]
    fn scope_leaf_plain() {
        // app-Foo.scope → Foo
        assert_eq!(parse_scope_leaf("app-Foo.scope"), Some("Foo".to_string()));
    }

    #[test]
    fn scope_leaf_plain_with_dots() {
        // app-org.x.Y.scope → org.x.Y (no numeric/hex suffix to strip)
        assert_eq!(
            parse_scope_leaf("app-org.x.Y.scope"),
            Some("org.x.Y".to_string())
        );
    }

    #[test]
    fn scope_leaf_instantiated_hex() {
        // app-Alacritty-a1b2c3d4.scope → Alacritty (8-char hex suffix with digit stripped)
        assert_eq!(
            parse_scope_leaf("app-Alacritty-a1b2c3d4.scope"),
            Some("Alacritty".to_string())
        );
    }

    #[test]
    fn scope_leaf_instantiated_numeric() {
        // app-org.x.Y-1234.scope → org.x.Y (numeric suffix stripped)
        assert_eq!(
            parse_scope_leaf("app-org.x.Y-1234.scope"),
            Some("org.x.Y".to_string())
        );
    }

    #[test]
    fn scope_leaf_flatpak() {
        // app-flatpak-org.x.Y-1234.scope → org.x.Y
        assert_eq!(
            parse_scope_leaf("app-flatpak-org.x.Y-1234.scope"),
            Some("org.x.Y".to_string())
        );
    }

    #[test]
    fn scope_leaf_flatpak_gnome() {
        // app-flatpak-org.gnome.Nautilus-123.scope → org.gnome.Nautilus
        assert_eq!(
            parse_scope_leaf("app-flatpak-org.gnome.Nautilus-123.scope"),
            Some("org.gnome.Nautilus".to_string())
        );
    }

    #[test]
    fn scope_leaf_not_app() {
        // Non-app scopes don't match.
        assert_eq!(parse_scope_leaf("session-1.scope"), None);
        assert_eq!(parse_scope_leaf("init.scope"), None);
    }

    #[test]
    fn cgroup_system_slice_returns_none() {
        let cgroup = "0::/system.slice/dbus.service\n";
        assert_eq!(parse_app_id_from_cgroup(cgroup), None);
    }

    #[test]
    fn cgroup_kernel_thread_returns_none() {
        // Kernel threads have an empty path.
        let cgroup = "0::/\n";
        assert_eq!(parse_app_id_from_cgroup(cgroup), None);
    }

    #[test]
    fn cgroup_plain_app() {
        let cgroup =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-firefox.scope\n";
        assert_eq!(
            parse_app_id_from_cgroup(cgroup),
            Some("firefox".to_string())
        );
    }

    #[test]
    fn cgroup_flatpak_app() {
        let cgroup = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-org.gnome.Nautilus-1234.scope\n";
        assert_eq!(
            parse_app_id_from_cgroup(cgroup),
            Some("org.gnome.Nautilus".to_string())
        );
    }

    #[test]
    fn cgroup_instantiated_app() {
        let cgroup = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-Alacritty-a1b2c3d4.scope\n";
        assert_eq!(
            parse_app_id_from_cgroup(cgroup),
            Some("Alacritty".to_string())
        );
    }

    // Fix 1: malformed / short cgroup lines are skipped, not fatal.

    #[test]
    fn cgroup_empty_line_skipped_not_fatal() {
        // A leading empty line (e.g. from "\n\n") must be skipped; the valid
        // second line should still resolve the app-id.
        let cgroup = "\n0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo.scope\n";
        assert_eq!(parse_app_id_from_cgroup(cgroup), Some("foo".to_string()));
    }

    #[test]
    fn cgroup_one_colon_line_skipped() {
        // A line with only one colon has no third field; it must be skipped.
        let cgroup = "malformed:line\n0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-bar.scope\n";
        assert_eq!(parse_app_id_from_cgroup(cgroup), Some("bar".to_string()));
    }

    // Fix 2: short / pure-alpha hex-looking suffixes are NOT stripped.

    #[test]
    fn scope_leaf_short_hex_word_not_stripped() {
        // "decade" is 6 chars and all-hex letters — must NOT be stripped.
        // app-gnome-decade.scope → gnome-decade (not "gnome")
        assert_eq!(
            parse_scope_leaf("app-gnome-decade.scope"),
            Some("gnome-decade".to_string())
        );
    }

    #[test]
    fn scope_leaf_genuine_hex_random_stripped() {
        // "a1b2c3d4" is 8 chars, all-hex, contains digits — IS stripped.
        // app-foo-a1b2c3d4.scope → foo
        assert_eq!(
            parse_scope_leaf("app-foo-a1b2c3d4.scope"),
            Some("foo".to_string())
        );
    }

    // ── legacy stat/statm parsing (preserved from v1) ───────────────────────

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
                app_id: Some("heavy".to_string()),
            },
        );
        groups.insert(
            "light".into(),
            Agg {
                cpu_jiffies: 10,
                mem_bytes: 900,
                procs: 1,
                app_id: Some("light".to_string()),
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
                app_id: None,
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
                app_id: None,
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
