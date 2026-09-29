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
//! # What gates the walk: the page, as native (#1427)
//!
//! The walk is the heaviest read this plugin makes — two or three small files
//! per PID, hundreds of PIDs — so it must not run while nobody is looking.
//! Measured by the #1426 review (`sample_proc`, release build, one pinned
//! core, median of 30 walks): 12.8 ms a walk at 341 processes, 23 ms at 638,
//! 47 ms at 1240 — about **1.2 % of a core** at ~640 processes on the 2 s
//! cadence, and 2.4 % at ~1240.
//!
//! The native poller walks exactly while the Stats drawer page is on screen
//! (`app_usage::set_active`, wired to `modal::stats_visible_signal` in the
//! shell's `main.rs`, #50) and parks the rest of the time. This plugin does the
//! same: it subscribes `StateKey::PageVisible`, the host tells it through
//! `Plugin::page_visible` whenever its **own page** opens or closes — a chip
//! click, `Esc`, a click outside, another page replacing it, a dialog
//! dismissed, a monitor unplugged — and the reducer forwards that one bool to
//! this walker's gate. Nothing else opens it: not the bar chips (which are
//! always on screen), and not the two lists' expanders.
//!
//! Why the page alone, and not "the page **and** a list expanded": both lists
//! come out of one walk, and the collapsed headers show the heaviest app's
//! `name · value` beside the chevron — native does, and keeps it live for as
//! long as the drawer is open. A walker that ran only for an expanded list
//! would leave both headers reading `—` on every visit that does not expand
//! one, which is the one thing #1426 could not match. Page-only also costs no
//! more than native: the walker parks on the same edge native's does.
//!
//! A sidebar instance publishes no page today (its card is not a click
//! target), so the host only ever tells it `false` and its walker never runs.
//! It still forwards the push, so the day its card opens a page the gate
//! already follows it.
//!
//! When the page closes the reducer drops the lists, so a reopen never shows a
//! reading taken before the close; a walk already in flight at the close is
//! dropped when it lands (the gate never cancels one).
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
//! it never made. The baseline is dropped when the page reopens (#1277 LOW
//! 4's rule), so the first CPU list after a reopen is a share over a fresh
//! window rather than the mean over however long the page was shut. (Native
//! keeps its baseline across a close, so its first list after a reopen is
//! that mean.)
//!
//! ## A quick close and reopen keeps its baseline
//!
//! Closing the page and reopening it within a couple of walks — a second chip
//! click, `Esc` and straight back — used to drop a baseline at most one walk
//! old, so the CPU list read `—` for a whole [`POLL`] (#1426 review, NIT 8).
//! Now the gated loop keeps a baseline whose read started less than
//! [`KEEP_BASELINE`] before the reopen, so that reopen's first walk is warm:
//! both lists fill as soon as it lands. A delta from such a baseline is a real
//! reading over a window at most twice the usual one — not a mean over a long
//! close, which is what the re-baseline exists to prevent. Past
//! [`KEEP_BASELINE`] the reopen re-baselines as before. The age lives in the
//! loop (`crate::sample`'s `drive`), which already sees every read.
//!
//! # Names: the desktop entry's, as native shows them (#1428)
//!
//! A group's key is the scope's app id, the unit name for a service, or
//! `System`. On niri ≥ 26.04 every `spawn`ed app runs in
//! `app-niri-<bin>-<pid>.scope`, so the key is `niri-firefox`, `niri-foot` and
//! so on. Native shows `Firefox` there: `panels/stats.rs`' `sample_display_name`
//! runs an app group's id through `components/app_meta.rs`'s
//! `resolve_app_meta` and falls back to the raw id, and leaves a service or
//! `System` row's name alone.
//!
//! The walker does the same, with [`hytte_sensors::desktop_entry::Resolver`]
//! — the same three layers over the same `.desktop` files, without gio. It
//! replaces an app row's `name` with the entry's display name before the rows
//! leave the walk, so the page's rows and the collapsed header's `name · value`
//! both read it, and the page has one name to print, not two. Where no entry
//! matches, the name stays the raw id, as native's does.
//!
//! The lookup runs **inside the walk**, i.e. under the same `spawn_blocking`,
//! never on the session thread: a miss reads every desktop entry on the
//! search path. It is cached per app id for the walker's life — the native
//! page caches per expander, which lives as long — so a miss costs one scan,
//! and every unseen id a walk meets shares that one scan
//! ([`Resolver::resolve_all`]). A [`Walker::reset`] keeps the cache: it drops
//! the CPU baseline, and a name is not a baseline.
//!
//! # Not here
//!
//! - **Icons.** Native resolves each row's icon through `gio::DesktopAppInfo`;
//!   the plugin knows the app id but not a `GIcon`. How the page gets one is
//!   an open question on #1419, so the rows carry no icon node at all. The
//!   resolver above already reads each entry's `Icon=`
//!   ([`hytte_sensors::desktop_entry::AppMeta::icon`]), for the day that
//!   question is answered with "the plugin reads the icon itself".
//! - **The battery-aware cadence.** Native stretches its 2 s poll to 8 s on
//!   battery (#505), reading `UPower` over D-Bus, which a plugin does not
//!   have. The walker keeps the AC cadence.

use std::collections::HashMap;
use std::time::Duration;

use hytte_sensors::app_usage::{self, ProcSample, sample_proc};
use hytte_sensors::desktop_entry::{Env, Resolver};

use crate::sample::Sample;

/// The walker's cadence: native `app_usage`'s `POLL` on AC power — half the
/// sensors' cadence there, since the walk reads files per PID rather than one
/// aggregate file.
pub const POLL: Duration = Duration::from_secs(2);

/// How young a baseline must be to survive a page reopen: two walks' worth of
/// [`POLL`]. A reopen within this long of the last walk's start keeps it, so
/// the reopen's first walk publishes a CPU list; an older one is dropped and
/// that walk is cold (see the [module docs](self)).
pub const KEEP_BASELINE: Duration = Duration::from_secs(POLL.as_secs() * 2);

/// One walk's two ranked lists — at most [`app_usage::TOP_N`] rows each,
/// heaviest first, each app row's `name` already its desktop entry's display
/// name where one resolves (see the [module docs](self)).
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
    /// App id → desktop entry, cached for the walker's life.
    names: Resolver,
}

impl Default for Walker {
    /// The real walker, over [`sample_proc`], with no baseline, naming apps
    /// from the process environment's desktop entries.
    fn default() -> Self {
        Self::over(sample_proc).naming_with(Resolver::from_env())
    }
}

impl<F: FnMut(&HashMap<u32, u64>, u64) -> app_usage::Sample> Walker<F> {
    /// A walker over `walk`, with no baseline, that **names nothing**: its
    /// resolver has an empty search path, so every row keeps its raw key and
    /// no desktop entry is ever read. [`Walker::naming_with`] gives it one.
    #[must_use]
    pub fn over(walk: F) -> Self {
        Self {
            walk,
            prev_pid: HashMap::new(),
            prev_total: 0,
            names: Resolver::new(Env::default()),
        }
    }

    /// This walker, naming app rows through `names` instead.
    #[must_use]
    pub fn naming_with(self, names: Resolver) -> Self {
        Self { names, ..self }
    }

    /// Walk `/proc` once. **Blocking** — `crate::sample`'s gated loop runs it
    /// under `spawn_blocking`, as `hytte-services`' own poller does (#434).
    ///
    /// Hands the walk the previous walk's baseline and keeps this one's for the
    /// next. The CPU list is published only when the shares it holds are a
    /// real delta: there was a baseline, and the `/proc/stat` total moved
    /// since it (a failed `/proc/stat` read answers `0`, and a total that did
    /// not advance divides nothing).
    ///
    /// Then names the published rows: each app row takes its desktop entry's
    /// display name, with one scan at most for every id not seen before.
    pub fn walk(&mut self) -> TopApps {
        let sample = (self.walk)(&self.prev_pid, self.prev_total);
        let measured = self.prev_total > 0 && sample.total_now > self.prev_total;
        self.prev_pid = sample.cur_pid;
        self.prev_total = sample.total_now;
        let mut apps = TopApps {
            by_cpu: if measured { sample.by_cpu } else { Vec::new() },
            by_mem: sample.by_mem,
        };
        self.name(&mut apps);
        apps
    }

    /// Native `sample_display_name`: an app row's name becomes its entry's
    /// display name, or stays its raw id (which is what the walker's app rows
    /// are named already); a service or `System` row — no app id — is left
    /// alone.
    fn name(&mut self, apps: &mut TopApps) {
        let ids = apps.by_cpu.iter().chain(&apps.by_mem);
        self.names
            .resolve_all(ids.filter_map(|row| row.app_id.as_deref()));
        for row in apps.by_cpu.iter_mut().chain(&mut apps.by_mem) {
            if let Some(meta) = row.app_id.as_deref().and_then(|id| self.names.resolve(id)) {
                row.name.clone_from(&meta.display_name);
            }
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
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use hytte_sensors::app_usage::{ProcSample, Sample as ProcWalk};
    use hytte_sensors::desktop_entry::{Env, Resolver};

    use super::{Walker, app, fake_proc};

    /// A fixture search path under `root`: `firefox.desktop` (named
    /// `Firefox`) in `<root>/share/applications`, with the `firefox` its
    /// `Exec=` names in `<root>/bin`. Returns the [`Env`] that reads exactly
    /// that — never the host's `$XDG_DATA_DIRS`.
    fn firefox_entry(root: &Path) -> Env {
        let apps = root.join("share/applications");
        let bin = root.join("bin");
        std::fs::create_dir_all(&apps).expect("mkdir applications");
        std::fs::create_dir_all(&bin).expect("mkdir bin");
        std::fs::write(
            apps.join("firefox.desktop"),
            "[Desktop Entry]\nType=Application\nName=Firefox\nExec=firefox %u\n",
        )
        .expect("write entry");
        std::fs::write(bin.join("firefox"), "#!/bin/sh\n").expect("write program");
        std::fs::set_permissions(bin.join("firefox"), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        Env {
            dirs: vec![apps],
            path: vec![bin],
            languages: Vec::new(),
        }
    }

    /// A walk whose two lists hold an app niri spawned, a service, the
    /// `System` bucket and an app no entry matches — with a `/proc/stat`
    /// total that advances, so every walk after the first publishes its CPU
    /// list.
    fn mixed_walk() -> impl FnMut(&HashMap<u32, u64>, u64) -> ProcWalk + Send + 'static {
        let mut n = 0_u64;
        move |_: &HashMap<u32, u64>, _: u64| {
            n += 1;
            let rows = vec![
                app("niri-firefox", 0.5, 3 << 30),
                ProcSample {
                    app_id: None,
                    ..app("NetworkManager", 0.1, 1 << 20)
                },
                ProcSample {
                    app_id: None,
                    ..app("System", 0.1, 1 << 20)
                },
                app("niri-ghost", 0.1, 1 << 20),
            ];
            ProcWalk {
                cur_pid: HashMap::new(),
                total_now: 1_000 * n,
                by_cpu: rows.clone(),
                by_mem: rows,
            }
        }
    }

    fn names(rows: &[ProcSample]) -> Vec<&str> {
        rows.iter().map(|row| row.name.as_str()).collect()
    }

    /// **An app row reads its desktop entry's name** — `niri-firefox` reads
    /// `Firefox`, #1428's own case — in both lists; a service and `System`
    /// keep theirs; an app no entry matches keeps its raw id. The app id
    /// itself is untouched. Every unseen id of one walk shares one scan, and
    /// the next walk scans nothing.
    ///
    /// **Falsified** by dropping the `self.name(&mut apps)` call in `walk`
    /// (the rows read `niri-firefox`), by resolving only `by_mem` (the CPU
    /// list reads `niri-firefox`), and by a per-row `resolve` without the
    /// `resolve_all` batch (two scans on the first walk).
    #[test]
    fn the_rows_read_each_apps_desktop_entry_name() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut walker =
            Walker::over(mixed_walk()).naming_with(Resolver::new(firefox_entry(root.path())));
        let want = ["Firefox", "NetworkManager", "System", "niri-ghost"];

        let cold = walker.walk();
        assert!(
            cold.by_cpu.is_empty(),
            "a cold walk still withholds its CPU list"
        );
        assert_eq!(names(&cold.by_mem), want);
        assert_eq!(cold.by_mem[0].app_id.as_deref(), Some("niri-firefox"));
        assert_eq!(walker.names.scans(), 1, "one scan for both unseen ids");

        let warm = walker.walk();
        assert_eq!(names(&warm.by_cpu), want, "the CPU list is named too");
        assert_eq!(names(&warm.by_mem), want);
        assert_eq!(walker.names.scans(), 1, "every id was cached");

        // With the entry gone, only a kept cache still says `Firefox`.
        walker.reset();
        std::fs::remove_file(root.path().join("share/applications/firefox.desktop"))
            .expect("rm entry");
        assert_eq!(
            names(&walker.walk().by_mem),
            want,
            "a reset drops the baseline, not the names",
        );
        assert_eq!(walker.names.scans(), 1);
    }

    /// **A service or `System` row is never looked up**, even where an entry
    /// would match its name: native's `sample_display_name` resolves only an
    /// `app_id`. `gnome-system-monitor.desktop`'s stem contains `system`, so a
    /// lookup by name would turn the `System` bucket into `System Monitor`.
    #[test]
    fn a_service_or_system_row_keeps_its_name_where_an_entry_would_match() {
        let root = tempfile::tempdir().expect("tempdir");
        let env = firefox_entry(root.path());
        let apps = root.path().join("share/applications");
        for (file, name) in [
            ("gnome-system-monitor.desktop", "System Monitor"),
            ("networkmanager.desktop", "Network"),
        ] {
            std::fs::write(
                apps.join(file),
                format!("[Desktop Entry]\nType=Application\nName={name}\nExec=firefox\n"),
            )
            .expect("write entry");
        }
        let mut walker = Walker::over(mixed_walk()).naming_with(Resolver::new(env));
        assert_eq!(
            names(&walker.walk().by_mem),
            ["Firefox", "NetworkManager", "System", "niri-ghost"],
        );
    }

    /// `Walker::over` names nothing — the seam every other test here and in
    /// `crate::sample` uses, which is why none of them reads the host's
    /// desktop entries.
    #[test]
    fn a_walker_over_a_fake_walk_names_nothing() {
        let mut walker = Walker::over(mixed_walk());
        assert_eq!(
            names(&walker.walk().by_mem),
            ["niri-firefox", "NetworkManager", "System", "niri-ghost"],
        );
    }

    /// Marks the re-exec'd child of
    /// [`the_default_walker_names_from_the_process_environment`] and carries
    /// its fixture root.
    const NAMES_CHILD: &str = "HYTTE_PLUGIN_STATS_NAMES_TEST_CHILD";

    /// Printed by the child only after its assertion passed, so a stale
    /// `--exact` filter that runs nothing cannot pass for a success.
    const NAMES_CHILD_OK: &str = "names-child-reached-the-end";

    /// **The production walker names rows from the real environment's
    /// desktop entries** — the wrapper half of the seam the test above
    /// drives. Re-execs this test binary because `std::env::set_var` is
    /// `unsafe` in edition 2024 and this workspace forbids `unsafe` (the
    /// `plugin::tests::settings_reads_the_real_process_environment`
    /// precedent). The child's `XDG_DATA_HOME`, `XDG_DATA_DIRS` and `PATH`
    /// point at the fixture, so it reads none of the host's entries, and it
    /// never walks `/proc`: it asks the default walker's resolver directly.
    ///
    /// **Falsified** by `Walker::default()` without its `naming_with` (the
    /// child's resolver has no search path and answers `None`).
    #[test]
    fn the_default_walker_names_from_the_process_environment() {
        let root = tempfile::tempdir().expect("tempdir");
        let _ = firefox_entry(root.path());
        let inner = "top_apps::tests::the_default_walker_names_from_the_process_environment_inner";
        let out = std::process::Command::new(std::env::current_exe().expect("this test binary"))
            .args(["--exact", "--nocapture", "--test-threads=1", inner])
            .env(NAMES_CHILD, root.path())
            .env("XDG_DATA_HOME", root.path().join("home"))
            .env("XDG_DATA_DIRS", root.path().join("share"))
            .env("PATH", root.path().join("bin"))
            .output()
            .expect("re-exec this test binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "the child failed\n{stdout}\n{stderr}");
        assert!(
            stdout.contains(NAMES_CHILD_OK),
            "the child ran nothing\n{stdout}\n{stderr}"
        );
    }

    /// The body of [`the_default_walker_names_from_the_process_environment`];
    /// a no-op outside its child.
    #[test]
    fn the_default_walker_names_from_the_process_environment_inner() {
        if std::env::var_os(NAMES_CHILD).is_none() {
            return;
        }
        let mut walker = Walker::default();
        assert_eq!(
            walker
                .names
                .resolve("niri-firefox")
                .map(|meta| meta.display_name.as_str()),
            Some("Firefox"),
        );
        println!("{NAMES_CHILD_OK}");
    }

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
