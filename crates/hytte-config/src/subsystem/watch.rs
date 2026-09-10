//! Live reload: poll every layer's change stamp and republish when one moves
//! (hoisted out of the `core-leds.toml` pilot by #1044).
//!
//! The payoff #869 exists for — edit the file, save it, watch the shell
//! re-dress within a few seconds, with no restart — and the half of a subsystem
//! that is *entirely* mechanism: nothing below knows a key name.
//!
//! # Why this is behind the `watch` cargo feature
//!
//! This module is the only part of `hytte-config` that needs an async runtime
//! ([`poll_loop`] sleeps) and a reactive handle ([`futures_signals::signal::Mutable`]).
//! `trollshell-control-center` links this crate for its Places tab and has no
//! runtime of its own to speak of; making `tokio` and `futures-signals`
//! unconditional dependencies of the shared leaf would grow the settings app a
//! runtime it never drives. The feature is off by default and enabled by the
//! shell's dependency line alone.
//!
//! # The three orderings that are not obvious
//!
//! **Stamp, then load** ([`Watcher::stamping_before`]). The baseline has to
//! predate the value. Stamp first and an edit landing in the window is read by
//! that very load, with a stamp that predates it — so the next [`Watcher::poll`]
//! sees the stamp move and re-reads, and the edit is one tick late at worst.
//! Stamp *after* and the baseline already includes an edit the published value
//! does not, and because `poll` updates its stamps unconditionally that edit is
//! missed **forever** (#1040 V2; measured: file said `crt`, panel said `lcd`,
//! three polls, nothing).
//!
//! **One load per process.** The constructor takes the loader as a parameter
//! and [`boot`] is the only caller, so a typo cannot produce two `unknown key`
//! warnings and a broken file cannot produce two `config unusable` errors
//! (#1040 F1).
//!
//! **The stamps update unconditionally** on every observed change, even when
//! the load that follows fails. That is what makes a malformed file warn once
//! per *save* rather than once per *tick* (#1040 T2): a file left half-typed is
//! not re-read until it is saved again.
//!
//! # What a stamp is, and what it is not
//!
//! [`Stamp`] is `(mtime, len)`. An mtime-only stamp misses an edit saved inside
//! the same mtime granule as the poll's own `stat`, and misses it
//! **permanently** — the stamp is updated unconditionally, so the movement is
//! never seen again. Linux's ext4/btrfs/tmpfs carry nanoseconds, so on a normal
//! machine the window is theoretical, but a coarse-granularity filesystem (a
//! network mount, a FAT stick someone points `XDG_CONFIG_DIRS` at) is not
//! exotic. A length is a cheap discriminator on the same `stat` call, not a
//! second syscall, and it catches the overwhelmingly common shape of a
//! same-granule edit (a value getting longer or shorter).
//!
//! It is **not** a hash: two edits that land in one granule *and* keep the byte
//! count identical are still missed, which is the honest limit of stat-polling
//! and the reason a real watch (inotify) is the eventual answer rather than a
//! finer stamp. `places`' own `ConfigWatcher` predates this and is mtime-only.

use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures_signals::signal::Mutable;

use super::env::Deprecations;
use super::{Subsystem, initial_load, load_layer};

/// How often a running shell re-checks its config layers. Each tick is one
/// `stat` per layer (two, normally: the nix base and the overlay) on a cached
/// inode, so it stays snappy while you edit at no measurable idle cost; the
/// files are only re-read when a stamp actually moves.
///
/// `places.toml`'s AC cadence (`hytte_services::places::CONFIG_POLL_INTERVAL`).
/// It does *not* slow down on battery the way places does since #505: that
/// needs `upower::on_battery_snapshot`, which is `pub(crate)` to
/// `hytte-services` and so unreachable from here. Noted rather than worked
/// around — widening it is a `hytte-services` change and belongs to whichever
/// subsystem migration first needs it.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// A layer's change stamp — its last-modified time **and** its length — or
/// `None` when the layer does not exist (the normal case for an overlay) or
/// cannot be stat'd. See the module doc for why the length is not decoration.
pub type Stamp = Option<(SystemTime, u64)>;

/// How a subsystem's deprecated environment variables are read.
///
/// A boxed `Fn` rather than a direct `std::env::var`, because `unsafe_code =
/// "forbid"` rules out `std::env::set_var` (an `unsafe fn` in edition 2024): a
/// test that drove the real environment could not exist at all, and one that
/// read it would depend on the developer's shell.
pub type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// One layer's [`Stamp`].
fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Every layer's [`stamp`], in path order — one `stat` per layer.
///
/// **Every** layer, not just the overlay: a nix rebuild moves the base file,
/// and a shell that only watched the top layer would show a stale base until
/// the user happened to touch their own file (#1040 R9).
fn stamps_of(paths: &[PathBuf]) -> Vec<Stamp> {
    paths.iter().map(|p| stamp(p)).collect()
}

/// Watches every layer of one subsystem for live reload by polling their
/// change stamps, in the shape `places`' `ConfigWatcher` established.
///
/// Holds the last file layer that loaded cleanly, which is what a malformed
/// save keeps: the shell goes on rendering the last good config rather than
/// snapping back to the built-in default the moment a hand edit is mid-word.
pub struct Watcher<S: Subsystem> {
    paths: Vec<PathBuf>,
    stamps: Vec<Stamp>,
    last_good: S::Resolved,
    /// `fn() -> S` rather than `S`, so the watcher is `Send`/`Sync` on the
    /// strength of `S::Resolved` alone — `S` itself is a schema type that never
    /// crosses a thread.
    _subsystem: PhantomData<fn() -> S>,
}

// Hand-written rather than derived: `#[derive(Clone)]` would demand `S: Clone`,
// which a schema type has no reason to be. The watcher is `Clone` because
// `spawn_supervised` takes an `Fn` factory, and a supervised restart must
// resume from the same baseline rather than re-stamping (#1040 V2).
impl<S: Subsystem> Clone for Watcher<S> {
    fn clone(&self) -> Self {
        Self {
            paths: self.paths.clone(),
            stamps: self.stamps.clone(),
            last_good: self.last_good.clone(),
            _subsystem: PhantomData,
        }
    }
}

impl<S: Subsystem> Watcher<S> {
    /// **Stamp `paths`, then read them with `load`** — in that order, which is
    /// the entire contract of this constructor.
    ///
    /// The one load is `load`'s, and it is a parameter rather than a call to
    /// [`initial_load`] for two reasons. The first is #1040 F1: the constructor
    /// used to load *by itself* while the service had already loaded, so one
    /// typo produced two `unknown key` warnings and one broken file two `config
    /// unusable` errors. The second is that the ordering is otherwise a rule
    /// nothing enforces — two statements in a caller, swappable without a
    /// single test noticing (measured: mutation V2a green against a test that
    /// had inlined the two lines itself). With the load handed in, the order
    /// lives *here*, in one place, and a test can hand in a `load` that edits
    /// the file on its way out and watch the next poll either see it or lose
    /// it.
    ///
    /// See the module doc for why the order is the difference between an edit
    /// that is one tick late and an edit that is lost forever.
    pub fn stamping_before(
        paths: Vec<PathBuf>,
        load: impl FnOnce(&[PathBuf]) -> S::Resolved,
    ) -> Self {
        let stamps = stamps_of(&paths);
        let last_good = load(&paths);
        Self {
            paths,
            stamps,
            last_good,
            _subsystem: PhantomData,
        }
    }

    /// The last file layer that loaded cleanly — **not** the resolved value:
    /// resolving is where the environment wins, and a watcher seeded with a
    /// value the environment already overrode would make the file's own value
    /// unrecoverable on the first reload (#1040 R15).
    pub fn last_good(&self) -> &S::Resolved {
        &self.last_good
    }

    /// What this environment and the current file layers resolve to.
    pub fn resolved(
        &self,
        lookup: &dyn Fn(&str) -> Option<String>,
        announce: Deprecations,
    ) -> S::Resolved {
        S::resolve(self.last_good.clone(), lookup, announce)
    }

    /// Reload and return the fresh value when some layer's stamp has moved
    /// *and* the result differs from `current`; otherwise `None`.
    ///
    /// A layer that stops **parsing as TOML** keeps [`Self::last_good`] and
    /// warns — once per edit rather than once per tick, because the stamp is
    /// taken before the load, so a file left malformed is not re-read until it
    /// is saved again (#1040 T2). That is the mid-word save: half a string
    /// typed, the shell should not flicker.
    ///
    /// A layer that parses but holds a value nothing accepts is a *different*
    /// case since #1040 V1 and is **not** an error: the good keys apply, the bad
    /// key takes the built-in default, and one warning names it. The asymmetry
    /// is deliberate — a half-typed string is a file caught mid-edit, a
    /// finished file with one bad value is a finished file with one mistake in
    /// it, and reverting every *other* key to punish that one is what V1 was
    /// filed about.
    ///
    /// A layer that is **deleted** is a third case and is deliberately not
    /// treated as an error either (#1040 F7): its stamp goes to `None`, the load
    /// succeeds over the layers that remain, and the subsystem goes back to the
    /// built-in defaults. Deleting a config file is an intent — "I want the
    /// stock behaviour back" — not a mistake, and it is the only way to get the
    /// defaults back without hand-restoring every key.
    pub fn poll(
        &mut self,
        current: &S::Resolved,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Option<S::Resolved> {
        let now = stamps_of(&self.paths);
        if now == self.stamps {
            return None;
        }
        // Unconditional, and before the load: this is what makes a malformed
        // file warn once per save rather than once per tick (#1040 T2).
        self.stamps = now;
        match load_layer::<S>(&self.paths) {
            Ok(config) => self.last_good = config,
            Err(e) => tracing::warn!(
                subsystem = S::NAME,
                error = %e,
                "config changed but is unusable; keeping the last good one"
            ),
        }
        // Silent: the environment has not changed, and this runs every few
        // seconds for the life of the shell.
        let next = self.resolved(lookup, Deprecations::Silent);
        (next != *current).then_some(next)
    }
}

/// The process's **one** startup sequence for a subsystem: stamp the layers,
/// load them once, resolve the environment over them announcing every
/// deprecated variable that is set, and hand back the value to publish beside
/// the poller that will keep it fresh.
///
/// The ordering lives in [`Watcher::stamping_before`] rather than in two
/// statements here, which is the point: as two statements it was a rule nothing
/// enforced, and a mutation that swapped them left the suite green (#1040 V2).
/// There is no re-read loop on purpose — erring toward "poll again" is free,
/// and a stat-read-stat retry would only narrow a window that is already
/// harmless in that direction.
///
/// This is the only place in production that passes
/// [`Deprecations::Announce`], and it takes `paths`/`lookup` rather than
/// reaching for the process so the real call site is drivable in a test (#1040
/// F3/V3).
pub fn boot<S: Subsystem>(
    paths: &[PathBuf],
    lookup: &dyn Fn(&str) -> Option<String>,
) -> (S::Resolved, Watcher<S>) {
    let watcher = Watcher::<S>::stamping_before(paths.to_vec(), initial_load::<S>);
    let resolved = S::resolve(watcher.last_good.clone(), lookup, Deprecations::Announce);
    (resolved, watcher)
}

/// Poll the layers and republish on a real change, so an edit reaches the shell
/// within `interval` without a restart.
///
/// Takes the [`Watcher`] rather than building one: [`boot`] already stamped and
/// loaded, in that order, and re-doing either here would put back the double
/// diagnostic (#1040 F1) and the lost-edit window (#1040 V2). The watcher is
/// `Clone`, which is what lets this ride a supervisor's `Fn` factory — a
/// restart resumes from the same baseline instead of re-stamping.
///
/// `lookup` and `interval` are parameters for the same reason `paths` is one:
/// the loop is then drivable in a test at a cadence a test can wait for, and
/// the subsystem's `Service` is the single place production values are chosen.
pub async fn poll_loop<S: Subsystem>(
    values: Mutable<S::Resolved>,
    mut watcher: Watcher<S>,
    lookup: EnvLookup,
    interval: Duration,
) {
    loop {
        tokio::time::sleep(interval).await;
        if let Some(next) = watcher.poll(&values.get_cloned(), &*lookup) {
            tracing::info!(subsystem = S::NAME, "config changed; reloaded");
            values.set(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Watcher, boot};
    use crate::subsystem::env::{self, Deprecations, EnvKnob};
    use crate::subsystem::{InvalidValue, Subsystem, keep, spelling};
    use crate::test_support::{Overlay, capture};
    use std::path::PathBuf;

    const LEVEL: EnvKnob = EnvKnob::same("HYTTE_TEST_DIAL_LEVEL", "level", "a level from 0 to 9");

    /// The smallest subsystem that can exercise a watcher: one key, one parser,
    /// one migrated variable. Deliberately *not* the shell's `core-leds` —
    /// nothing generic here may know a real schema, and a test that borrowed one
    /// would be asserting the pilot's four parsers all over again.
    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    #[serde(default)]
    struct Dial {
        level: toml::Value,
    }

    impl Default for Dial {
        fn default() -> Self {
            Self {
                level: toml::Value::Integer(1),
            }
        }
    }

    impl Subsystem for Dial {
        const NAME: &'static str = "dial";
        const DEFAULT_TOML: &'static str = "# the dial\nlevel = 1\n";
        type Error = std::convert::Infallible;
        type Resolved = u8;

        fn validate(&self) -> Result<(), Self::Error> {
            Ok(())
        }

        fn parsed(&self) -> (u8, Vec<InvalidValue>) {
            let mut rejected = Vec::new();
            let level = keep(
                parse_level(&spelling(&self.level))
                    .map_err(|_| InvalidValue::of(&LEVEL, &self.level)),
                1,
                &mut rejected,
            );
            (level, rejected)
        }

        fn resolve(
            layered: u8,
            lookup: &dyn Fn(&str) -> Option<String>,
            announce: Deprecations,
        ) -> u8 {
            let raw = lookup(LEVEL.var);
            env::key(
                Self::NAME,
                &LEVEL,
                raw.as_deref(),
                parse_level,
                layered,
                announce,
            )
        }
    }

    fn parse_level(raw: &str) -> Result<u8, &str> {
        match raw.parse::<u8>() {
            Ok(n) if n <= 9 => Ok(n),
            _ => Err(raw),
        }
    }

    fn overlay() -> Overlay {
        Overlay::new(Dial::NAME)
    }

    fn no_env() -> impl Fn(&str) -> Option<String> {
        |_: &str| None
    }

    /// The **stamp-before-load** contract, driven the only way it can be:
    /// the loader handed to the constructor edits the file on its way out, so
    /// the edit lands strictly *after* the baseline was taken and strictly
    /// *before* the constructor returns.
    ///
    /// **Red if the two statements in [`Watcher::stamping_before`] are
    /// swapped** (#1040 V2, mutation R5). Stamp after the load and the baseline
    /// already includes an edit the published value does not — and because
    /// [`Watcher::poll`] updates its stamps unconditionally, that edit is missed
    /// **forever**, not merely late.
    #[test]
    fn a_watcher_stamps_before_it_loads_so_an_edit_in_the_window_is_not_lost() {
        let file = std::cell::RefCell::new(overlay());
        file.borrow_mut().write("level = 2\n");
        let paths = file.borrow().layers();

        let mut watcher = Watcher::<Dial>::stamping_before(paths, |paths| {
            let loaded = crate::subsystem::initial_load::<Dial>(paths);
            // Lands after the read this closure just did. With the stamp taken
            // first, the baseline predates it and the next poll sees it.
            file.borrow_mut().write("level = 7\n");
            loaded
        });

        assert_eq!(*watcher.last_good(), 2, "the load read the pre-edit file");
        assert_eq!(
            watcher.poll(&2, &no_env()),
            Some(7),
            "…and the edit made during the load is picked up, not baselined away"
        );
    }

    /// An unmoved stamp is not re-read, and a change that resolves to the same
    /// value is not republished (#1040 mutations M4, Z7).
    #[test]
    fn an_unmoved_stamp_does_nothing_and_an_unchanged_value_is_not_republished() {
        let mut file = overlay();
        file.write("level = 3\n");
        let (current, mut watcher) = boot::<Dial>(&file.layers(), &no_env());
        assert_eq!(current, 3);

        assert_eq!(watcher.poll(&current, &no_env()), None, "nothing moved");

        // The stamp moves, the value does not.
        file.write("level = 3 # same value, new bytes\n");
        assert_eq!(
            watcher.poll(&current, &no_env()),
            None,
            "a reload that changes nothing must not republish"
        );
    }

    /// The change stamp is `(mtime, len)`, so a save inside the poll's own mtime
    /// granule is still seen.
    ///
    /// **Red if `stamp` drops the length** (#1040 F8, mutation N4). An
    /// mtime-only stamp misses this edit *permanently*, because the stamp is
    /// updated unconditionally and the movement is never seen again.
    #[test]
    fn an_edit_inside_one_mtime_granule_is_still_seen() {
        let mut file = overlay();
        file.write("level = 1\n");
        let (current, mut watcher) = boot::<Dial>(&file.layers(), &no_env());

        file.write_in_the_same_granule("level = 4 # nudged\n");

        assert_eq!(watcher.poll(&current, &no_env()), Some(4));
    }

    /// …and the honest limit of the same mechanism, stated rather than implied:
    /// an edit that lands in one mtime granule **and** keeps the byte count
    /// identical is still missed, forever. A `(mtime, len)` pair is a cheap
    /// discriminator on the same `stat`, not a hash; closing this case wants a
    /// real watch (inotify), which is why the module doc says so instead of
    /// pretending the pair is complete.
    ///
    /// It is pinned so the limit is a decision somebody made rather than a
    /// surprise somebody hits: a fix that closed it would redden this test and
    /// have to say why.
    #[test]
    fn an_edit_of_the_same_length_in_the_same_granule_is_the_honest_limit() {
        let mut file = overlay();
        file.write("level = 1\n");
        let (current, mut watcher) = boot::<Dial>(&file.layers(), &no_env());

        // Same byte count, same mtime granule.
        file.write_in_the_same_granule("level = 4\n");

        assert_eq!(
            watcher.poll(&current, &no_env()),
            None,
            "stat-polling cannot see this, and does not claim to"
        );
    }

    /// A layer that stops parsing as TOML keeps the last good one and warns —
    /// **once per save, not once per tick**.
    ///
    /// Two mechanisms in one test, and both were unpinned at some point in
    /// #1040: the `Err` arm must not overwrite `last_good` (mutation M5), and
    /// `poll`'s `self.stamps = now;` must run unconditionally, before the load
    /// (T2, mutation R8 — deleting that line left 585 tests green while a bad
    /// file re-warned every three seconds for the life of the shell).
    #[test]
    fn a_malformed_layer_keeps_the_last_good_one_and_warns_once_per_save() {
        let mut file = overlay();
        file.write("level = 5\n");
        let (current, mut watcher) = boot::<Dial>(&file.layers(), &no_env());
        assert_eq!(current, 5);

        let (captured, _guard) = capture();
        file.write("level = \"unterminated\n");

        assert_eq!(
            watcher.poll(&current, &no_env()),
            None,
            "nothing republished"
        );
        assert_eq!(*watcher.last_good(), 5, "the last good layer is kept");
        for _ in 0..3 {
            assert_eq!(watcher.poll(&current, &no_env()), None);
        }
        let warned = captured.warnings();
        assert_eq!(
            warned.len(),
            1,
            "one line per save, not one per poll: {warned:#?}"
        );
    }

    /// A **deleted** layer is an intent, not a mistake: the load succeeds over
    /// what remains and the subsystem goes back to its built-in default
    /// (#1040 F7, mutation N8).
    #[test]
    fn a_deleted_layer_falls_back_to_the_built_in_default() {
        let mut file = overlay();
        file.write("level = 8\n");
        let (current, mut watcher) = boot::<Dial>(&file.layers(), &no_env());
        assert_eq!(current, 8);

        let (captured, _guard) = capture();
        file.delete();

        assert_eq!(
            watcher.poll(&current, &no_env()),
            Some(1),
            "back to DEFAULT_TOML's value"
        );
        assert_eq!(
            captured.warnings(),
            Vec::<String>::new(),
            "and it is not an error"
        );
    }

    /// **Every** layer is stamped, not just the highest-precedence one — a nix
    /// rebuild moves the base file and the shell must notice (#1040 mutation
    /// R9).
    #[test]
    fn an_edit_to_a_lower_layer_reloads_too() {
        let mut base = overlay();
        let overlay_file = overlay();
        base.write("level = 2\n");
        let paths: Vec<PathBuf> =
            vec![base.path().to_path_buf(), overlay_file.path().to_path_buf()];
        let (current, mut watcher) = boot::<Dial>(&paths, &no_env());
        assert_eq!(current, 2, "the base layer applies with no overlay over it");

        base.write("level = 6\n");

        assert_eq!(watcher.poll(&current, &no_env()), Some(6));
    }

    /// [`boot`] seeds the watcher with the **file layer**, not the resolved
    /// value, and announces the environment exactly once.
    ///
    /// **Red if `boot` seeds the resolved value** (#1040 mutation R15): the
    /// file's own value would then be unrecoverable on the first reload, since
    /// the environment had already overwritten it. **Red if `boot` passes
    /// `Silent`** (#1040 F3, mutation X1): nobody would ever hear a deprecation
    /// line again.
    #[test]
    fn boot_seeds_the_file_layer_and_announces_once() {
        let mut file = overlay();
        file.write("level = 3\n");
        let (captured, _guard) = capture();

        let (resolved, watcher) = boot::<Dial>(&file.layers(), &|name: &str| {
            (name == LEVEL.var).then(|| "9".to_string())
        });

        assert_eq!(resolved, 9, "the variable wins");
        assert_eq!(
            *watcher.last_good(),
            3,
            "…but the watcher holds the file layer, so a reload can recover it"
        );
        let warned = captured.warnings();
        assert_eq!(warned.len(), 1, "exactly one line: {warned:#?}");
        assert!(warned[0].contains(LEVEL.var), "{warned:#?}");
    }

    /// A reload re-resolves the same environment — the variable keeps winning —
    /// and says **nothing** about it (#1040 mutations M2, M3).
    #[test]
    fn a_reload_keeps_the_variable_winning_and_re_announces_nothing() {
        let mut file = overlay();
        file.write("level = 3\n");
        let lookup = |name: &str| (name == LEVEL.var).then(|| "9".to_string());
        let (resolved, mut watcher) = boot::<Dial>(&file.layers(), &lookup);
        assert_eq!(resolved, 9);

        let (captured, _guard) = capture();
        file.write("level = 4\n");

        assert_eq!(
            watcher.poll(&resolved, &lookup),
            None,
            "the variable still pins the value, so nothing republishes"
        );
        assert_eq!(*watcher.last_good(), 4, "…though the file layer did move");
        assert_eq!(
            captured.warnings(),
            Vec::<String>::new(),
            "a reload announces nothing"
        );
    }

    /// The poll cadence is a few seconds — `places.toml`'s AC cadence. Pinned so
    /// a change to it is a deliberate one.
    #[test]
    fn the_poll_interval_is_a_few_seconds() {
        assert_eq!(super::POLL_INTERVAL, std::time::Duration::from_secs(3));
    }
}
