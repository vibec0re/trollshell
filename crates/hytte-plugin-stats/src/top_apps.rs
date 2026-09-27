//! The drawer page's **Top apps · CPU / RAM** lists (#1419 item 3), sampled in
//! this plugin's own process.
//!
//! # The same walker the native page reads
//!
//! The native Stats page's two "Top apps" expanders read
//! `hytte_services::app_usage`, whose `/proc` + cgroup walker #1422 moved into
//! [`hytte_sensors::app_usage`] byte-for-byte so a plugin could reach it. This
//! module calls that walker — [`sample_proc`] — and nothing else: the grouping
//! (systemd app scope, then the deepest `.service`, then `System`), the
//! ranking and the six-row cap are the native page's, not a second
//! implementation that agrees by coincidence.
//!
//! # What gates the walk: an open list, not the page
//!
//! The walk is the heaviest read this plugin makes — two or three small files
//! per PID, hundreds of PIDs — so it must not run while nobody is looking. The
//! native poller parks while the Stats drawer is hidden. **This plugin cannot
//! see its page's visibility**: the host tells a plugin when its *mount
//! surface* shows or hides (`SlotVisibility`, which for a bar mount is a
//! constant `true`), but not when the drawer opens or closes on its page. A
//! chip click toggles the drawer, and Esc or a click outside closes it, and
//! none of that reaches the plugin. Growing the wire a panel-visibility push is
//! a protocol change, and out of scope here.
//!
//! So the gate is the one piece of page state the plugin *does* hold: the two
//! lists' expander flags (the wire's `Node::Expander` is plugin-driven, so the
//! plugin is the only place they live). The walker runs while **either list is
//! expanded** and parks the moment both are collapsed. What that buys and costs
//! against the native page:
//!
//! - **Collapsed, which is the default, walks nothing at all** — cheaper than
//!   native, which walks for as long as the drawer is open. That holds only
//!   until the first expand; see the known cost below.
//! - **The collapsed header's summary reads `—` while nothing is being
//!   measured.** Native shows the heaviest app beside the chevron even when
//!   the list is collapsed; here the summary only carries a name while one of
//!   the two lists is open (either one — both lists come out of the same
//!   walk). Showing the last reading instead would be showing a number
//!   nobody is measuring any more as if it were live.
//!
//! ## Known cost: a list left open keeps walking after the drawer closes
//!
//! The expander state lives in the plugin and survives the drawer closing, so
//! the ordinary path — expand a list, close the drawer with Esc or a click
//! outside — leaves the walker running every [`POLL`] until that list is
//! collapsed again or the plugin restarts. Native parks this same walk
//! whenever its Stats page is off screen (#50). Every such walk also changes
//! the page, so the plugin sends a render frame nobody is looking at.
//!
//! Measured by the #1426 review (`sample_proc`, release build, one pinned
//! core, median of 30 walks): 12.8 ms a walk at 341 processes, 23 ms at 638,
//! 47 ms at 1240 — about **1.2 % of a core** at ~640 processes on the 2 s
//! cadence, and 2.4 % at ~1240. This PR does not fix it: the fix is the host
//! telling the plugin when its page is on screen, `PageVisible`, which is
//! #1427. Until that lands, this cost is accepted, not hidden.
//!
//! # A CPU share needs two walks
//!
//! [`sample_proc`] is handed the previous walk's per-PID jiffies and
//! `/proc/stat` total, and a CPU share is the delta between the two. So the
//! [`Walker`] threads that state from one walk to the next — a fresh map every
//! walk would make every share `0` — and on a walk that has no baseline (the
//! first after the gate opens) it publishes the RAM list but **withholds the
//! CPU list**, the same rule `sample.rs`'s `cpu_half` follows for the CPU
//! headline (#1277 MEDIUM 3): every share on a cold walk is `0`, a measurement
//! it never made. The baseline is dropped when the gate reopens (#1277 LOW 4's
//! rule), so the first CPU list after a reopen is a share over a fresh window
//! rather than the mean over however long the lists were shut.
//!
//! ## Known wrinkle: a quick close and reopen
//!
//! The same rule applies to a close and reopen within one walk (a
//! double-click on the header). The CPU list then reads `—` for about 2 s,
//! even though the baseline it dropped was at most 2 s old. If a walk was in
//! flight across the close, its rows land after the reopen and the cold walk
//! then blanks them, so the list goes rows → `—` → rows. This is left as is,
//! deliberately (#1426 review, NIT 8). Keeping a young baseline needs the
//! loop to know how long the list was closed. Carrying the rows over a cold
//! walk needs a limit on how many cold walks in a row it may bridge.
//! Otherwise a reading nobody took could stay on screen. Either is new
//! timing state in code this plugin's reopen correctness rests on, for a 2 s
//! cosmetic gap.
//!
//! # Not here
//!
//! - **Icons.** Native resolves each row's icon through `gio::DesktopAppInfo`;
//!   the plugin knows the app id but not a `GIcon`. How the page gets one is
//!   an open question on #1419, so the rows carry no icon node at all.
//! - **Display names.** A row shows the group's raw key, not a display name:
//!   the scope's app id, the unit name for a service, or `System`. On niri
//!   ≥ 26.04 every `spawn`ed app runs in `app-niri-<bin>-<pid>.scope`, so the
//!   key is `niri-firefox`, `niri-foot` and so on. Native shows `Firefox`
//!   there, because its `app_meta` lookup also matches a `.desktop` stem the
//!   id merely *contains* — so on niri most rows differ from native, not only
//!   the rare app with no `.desktop` file. The names stay raw ids until a
//!   follow-up ports `trollshell/src/components/app_meta.rs`'s three layers
//!   (exact id, id containment, `Exec` basename) to plain `.desktop` parsing
//!   the plugin can run. That port serves `Name=` now and `Icon=` once #1419
//!   answers the icon question.
//! - **The battery-aware cadence.** Native stretches its 2 s poll to 8 s on
//!   battery (#505), reading `UPower` over D-Bus, which a plugin does not
//!   have. The walker keeps the AC cadence.

use std::collections::HashMap;
use std::time::Duration;

use hytte_sensors::app_usage::{self, ProcSample, sample_proc};

use crate::sample::Sample;

/// The walker's cadence: native `app_usage`'s `POLL` on AC power — half the
/// sensors' cadence there, since the walk reads files per PID rather than one
/// aggregate file.
pub const POLL: Duration = Duration::from_secs(2);

/// One walk's two ranked lists — at most [`app_usage::TOP_N`] rows each,
/// heaviest first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TopApps {
    /// Groups by CPU share, descending. **Empty** on a walk with no baseline —
    /// see the [module docs](self).
    pub by_cpu: Vec<ProcSample>,
    /// Groups by resident memory, descending. Valid from the very first walk:
    /// a resident size, unlike a jiffy count, needs no previous reading.
    pub by_mem: Vec<ProcSample>,
}

/// What [`Walker`] calls to read `/proc` — [`sample_proc`]'s own signature, so
/// the real walker is a plain function pointer and a test's fake is a closure.
pub type Walk = fn(&HashMap<u32, u64>, u64) -> app_usage::Sample;

/// The walker and the state it threads between two walks.
pub struct Walker<F = Walk> {
    /// The `/proc` read: [`sample_proc`] in production.
    walk: F,
    /// Per-PID CPU jiffies as of the previous walk — the baseline each PID's
    /// share is a delta from.
    prev_pid: HashMap<u32, u64>,
    /// The aggregate `/proc/stat` total as of the previous walk; `0` is "no
    /// baseline yet".
    prev_total: u64,
}

impl Default for Walker {
    /// The real walker, over [`sample_proc`], with no baseline.
    fn default() -> Self {
        Self::over(sample_proc)
    }
}

impl<F: FnMut(&HashMap<u32, u64>, u64) -> app_usage::Sample> Walker<F> {
    /// A walker over `walk`, with no baseline.
    #[must_use]
    pub fn over(walk: F) -> Self {
        Self {
            walk,
            prev_pid: HashMap::new(),
            prev_total: 0,
        }
    }

    /// Walk `/proc` once. **Blocking** — `crate::sample`'s gated loop runs it
    /// under `spawn_blocking`, as `hytte-services`' own poller does (#434).
    ///
    /// Hands the walk the previous walk's baseline and keeps this one's for the
    /// next. The CPU list is published only when the shares it holds are a
    /// real delta: there was a baseline, and the `/proc/stat` total moved
    /// since it (a failed `/proc/stat` read answers `0`, and a total that did
    /// not advance divides nothing).
    pub fn walk(&mut self) -> TopApps {
        let sample = (self.walk)(&self.prev_pid, self.prev_total);
        let measured = self.prev_total > 0 && sample.total_now > self.prev_total;
        self.prev_pid = sample.cur_pid;
        self.prev_total = sample.total_now;
        TopApps {
            by_cpu: if measured { sample.by_cpu } else { Vec::new() },
            by_mem: sample.by_mem,
        }
    }

    /// Drop the baseline, so the next [`walk`](Self::walk) withholds its CPU
    /// list and the one after it is a share over a fresh window.
    pub fn reset(&mut self) {
        self.prev_pid.clear();
        self.prev_total = 0;
    }
}

impl<F> Sample for Walker<F>
where
    F: FnMut(&HashMap<u32, u64>, u64) -> app_usage::Sample + Send + 'static,
{
    type Reading = TopApps;

    fn tick(&mut self) -> TopApps {
        self.walk()
    }

    fn reset(&mut self) {
        Walker::reset(self);
    }
}

/// A group literal, for the tests here and in `crate::sample`.
#[cfg(test)]
pub(crate) fn app(name: &str, cpu_frac: f64, mem_bytes: u64) -> ProcSample {
    ProcSample {
        name: name.to_owned(),
        app_id: Some(name.to_owned()),
        cpu_frac,
        mem_bytes,
        procs: 1,
    }
}

/// The baselines a [`fake_proc`] was handed, one `(pid 7's jiffies, total)`
/// pair per walk, oldest first.
#[cfg(test)]
pub(crate) type Handed = std::sync::Arc<std::sync::Mutex<Vec<(Option<u64>, u64)>>>;

/// A stand-in for `/proc` that behaves the way [`sample_proc`] does: one
/// process (PID 7) whose cumulative jiffies grow by a quarter of the machine's
/// every interval, with its share computed **from the baseline it is handed**
/// exactly as the real walk computes it (an unseen PID deltas to itself, the
/// interval is `total_now - prev_total`). It also records every baseline it
/// was handed.
///
/// So a walker that threads its baseline reads `0.25` from the second walk
/// on, and one that loses it reads `0` (or, with no total either, publishes no
/// CPU list at all). Shared with `crate::sample`'s tests, which run a real
/// [`Walker`] over it through the gated task.
#[cfg(test)]
pub(crate) fn fake_proc() -> (
    impl FnMut(&HashMap<u32, u64>, u64) -> app_usage::Sample + Send + 'static,
    Handed,
) {
    let seen: Handed = std::sync::Arc::default();
    let log = std::sync::Arc::clone(&seen);
    let mut n = 0_u64;
    let walk = move |prev_pid: &HashMap<u32, u64>, prev_total: u64| {
        n += 1;
        log.lock()
            .expect("not poisoned")
            .push((prev_pid.get(&7).copied(), prev_total));
        let total_now = 1_000 * n;
        let jiffies = 250 * n;
        let delta = jiffies - prev_pid.get(&7).copied().unwrap_or(jiffies);
        #[allow(clippy::cast_precision_loss)]
        let share = delta as f64 / total_now.saturating_sub(prev_total) as f64;
        app_usage::Sample {
            cur_pid: HashMap::from([(7, jiffies)]),
            total_now,
            by_cpu: vec![app("firefox", share, 1 << 30)],
            by_mem: vec![app("firefox", share, 1 << 30)],
        }
    };
    (walk, seen)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use hytte_sensors::app_usage::Sample as ProcWalk;

    use super::{Walker, app, fake_proc};

    /// **The baseline is threaded from one walk to the next** — the claim a
    /// CPU share rests on. The second walk is handed exactly what the first
    /// one saw, and the share it publishes is the real quarter.
    ///
    /// **Falsified** by each of: not storing `cur_pid` (the second walk is
    /// handed an empty map, the process reads as unseen and its share is `0`),
    /// not storing `total_now` (the second walk has no baseline and its CPU
    /// list is withheld), and building a fresh `HashMap` per walk.
    #[allow(clippy::float_cmp)]
    #[test]
    fn each_walk_is_handed_the_previous_walks_baseline() {
        let (walk, seen) = fake_proc();
        let mut walker = Walker::over(walk);

        let first = walker.walk();
        assert!(
            first.by_cpu.is_empty(),
            "a cold walk has no delta, so it publishes no CPU list: {first:?}",
        );
        assert_eq!(first.by_mem.len(), 1, "…but its RAM list is real");

        for _ in 0..3 {
            let next = walker.walk();
            assert_eq!(next.by_cpu.len(), 1);
            assert_eq!(
                next.by_cpu[0].cpu_frac, 0.25,
                "a quarter of every interval, measured over the interval",
            );
        }

        assert_eq!(
            *seen.lock().expect("not poisoned"),
            vec![
                (None, 0),
                (Some(250), 1_000),
                (Some(500), 2_000),
                (Some(750), 3_000)
            ],
            "each walk is handed the jiffies and the total the one before it saw",
        );
    }

    /// **A reset drops the baseline**: the next walk is cold again (no CPU
    /// list) and the one after it measures from the reset, not from before it.
    ///
    /// **Falsified** by an empty `reset` (the walk after it publishes a share
    /// straight away, measured across the whole time the lists were shut).
    #[test]
    fn a_reset_makes_the_next_walk_cold() {
        let (walk, seen) = fake_proc();
        let mut walker = Walker::over(walk);
        let _ = walker.walk();
        assert_eq!(walker.walk().by_cpu.len(), 1);

        walker.reset();
        assert!(walker.walk().by_cpu.is_empty(), "cold again after a reset");
        assert_eq!(walker.walk().by_cpu.len(), 1);
        assert_eq!(
            seen.lock().expect("not poisoned")[2],
            (None, 0),
            "the walk after the reset was handed no baseline",
        );
    }

    /// A `/proc/stat` total that did not move is not a measurement: the CPU
    /// list is withheld rather than published as a column of `0%`.
    #[test]
    fn a_total_that_did_not_advance_withholds_the_cpu_list() {
        let mut calls = 0_u32;
        let mut walker = Walker::over(move |_: &HashMap<u32, u64>, _: u64| {
            calls += 1;
            ProcWalk {
                cur_pid: HashMap::new(),
                // A failed `/proc/stat` read answers `0` every time.
                total_now: if calls == 1 { 1_000 } else { 0 },
                by_cpu: vec![app("x", 0.0, 1)],
                by_mem: vec![app("x", 0.0, 1)],
            }
        });
        let _ = walker.walk();
        let failed = walker.walk();
        assert!(failed.by_cpu.is_empty(), "{failed:?}");
        assert_eq!(failed.by_mem.len(), 1);
    }

    /// A total that reads fine but **has not moved** is no measurement
    /// either: every share would divide by nothing and read `0%`. The test
    /// above covers a total that fell to `0`; this one covers an equal one
    /// (from the #1426 review, NIT 7).
    ///
    /// **Falsified** by `sample.total_now >= self.prev_total` in `walk`.
    #[test]
    fn an_unchanged_total_withholds_the_cpu_list() {
        let mut walker = Walker::over(|_: &HashMap<u32, u64>, _: u64| ProcWalk {
            cur_pid: HashMap::new(),
            total_now: 1_000,
            by_cpu: vec![app("x", 0.0, 1)],
            by_mem: vec![app("x", 0.0, 1)],
        });
        let _ = walker.walk();
        let same = walker.walk();
        assert!(same.by_cpu.is_empty(), "{same:?}");
    }
}
