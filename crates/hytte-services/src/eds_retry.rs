//! Shared resilience primitives for the two EDS worker threads
//! ([`crate::calendar`] and [`crate::tasks`]) — issue #432.
//!
//! Both services own a dedicated thread that opens a
//! [`hytte_ecal::Registry`] at startup and caches per-source
//! [`hytte_ecal::CalClient`] handles. Two failure modes used to be
//! permanent:
//!
//! 1. **Init failure** — at session bring-up trollshell and
//!    evolution-data-server activate concurrently, so the blocking
//!    `Registry::new()` D-Bus round-trip can time out. The worker used to
//!    drain its channel and return, leaving the service inert for the whole
//!    session. Now it retries with the exponential backoff defined here.
//! 2. **Dead cached handles** — an EDS crash/restart (or a source removed
//!    at runtime) kills every cached `CalClient`, but the caches had no
//!    eviction, so every poll failed quietly forever. The workers now evict
//!    on error and, when *every* known source keeps failing, rebuild the
//!    whole session ([`SourceFailureStreak`] decides when).
//!
//! Since #1170 it also owns the third: a **panic** on either worker thread.
//! Both were bare `std::thread::spawn`s, so an unwind killed the thread and
//! froze the service for the session with no log line — the residual #430 left
//! behind. [`spawn_eds_worker`] is the one place that supervision lives.
//!
//! Both `run_worker`s (`calendar.rs`, `tasks.rs`) also **return** on purpose
//! once every sender has disconnected — the service tearing down — which
//! #1196 declares through [`spawn_supervised_blocking_bounded`] rather than
//! the plain `spawn_supervised_blocking`: a `debug!` and a released health row
//! instead of a `warn!` and a permanent `Returned` one.
//!
//! Everything here is pure logic + std channels — hermetically testable,
//! no EDS required.

use hytte_reactive::spawn_supervised_blocking_bounded;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Run an EDS worker body on a supervised blocking thread, handing it the
/// service's one `Receiver` on every run.
///
/// The two services' `Service::start` used to `std::thread::spawn` their worker
/// directly. A panic under `run_worker` — libecal FFI, an iCal parse of
/// whatever a `CalDAV` server returned — killed the thread, and with it every
/// later refresh: the `Mutable`s froze at their last value for the session, and
/// the only tell was a stderr line. `hytte_reactive::spawn_supervised_blocking`
/// is the crate's answer to exactly that (`niri.rs` states the argument
/// verbatim), and this is the EDS-shaped wrapper over it.
///
/// Spawned via [`spawn_supervised_blocking_bounded`], not the plain
/// `spawn_supervised_blocking` (#1196): both `run_worker`s return on purpose
/// once their `Receiver` reports every sender gone — `for _ in rx {}` /
/// `while rx.recv().is_ok() {}` falling through, or the equivalent
/// `TryRecvError::Disconnected` arm in `tasks`' event loop — which is this
/// worker shutting down, not a bug that fell out of a loop. The bounded
/// variant is what turns that into a `debug!` and a released health row
/// instead of a `warn!` and a permanent `Returned` one for the rest of the
/// session.
///
/// # Why the receiver is behind a mutex
///
/// The `Receiver` must outlive any one run — a restart that lost it would drop
/// every queued op and could never get another, since `SENDER` is a `OnceLock`
/// set once at registration. It cannot simply be *captured*, because
/// `spawn_supervised_blocking_bounded` takes an `Fn() + Send + Sync` (it
/// re-runs the closure from a fresh blocking thread per run) and
/// `mpsc::Receiver` is `Send` but not `Sync`. A `Mutex` makes it `Sync` and
/// hands the run exclusive use; only one run exists at a time, so the lock is
/// never contended.
///
/// **The poison tolerance is load-bearing, not boilerplate.** A panicking run
/// unwinds while this guard is held, which poisons the mutex; a plain
/// `.unwrap()` would panic the restarted run too, so *every* run after the
/// first would die on the lock rather than on the bug — measured:
/// `panics: 4, consecutive_panics: 4, backoff: 8s` where the tolerant version
/// picks the queued op up on run 2.
///
/// Two things that reading "poison-tolerant lock" as a hazard being accepted
/// would get wrong:
///
/// * **There is no state to expose.** The lock wraps a `mpsc::Receiver` and
///   nothing else. A `Receiver` carries no invariant across messages, so a
///   panic mid-`recv` cannot leave it half-updated; the poison flag here is
///   pure collateral from unwinding through the guard, not a signal about the
///   data. Poison tolerance is dangerous where the guarded value has a
///   multi-field invariant — this is the other case.
/// * **The panic loop it avoids is slow, not hot.** The supervisor's ramp is
///   1 → 2 → 4 → 8 → … → 30 s, so the `.unwrap()` variant costs one panic and
///   one `error!` per 30 s in the steady state. Unbounded, and the service
///   never comes back — but not a burning core, and worth knowing before
///   triaging one.
///
/// **One op is still lost per panic**: the one already `recv`'d when the panic
/// hit is gone with the run that took it. Ops still queued survive, which is
/// what the shared `Arc<Mutex<Receiver>>` buys. For `calendar` that costs a
/// refresh, which the next one repairs; for `tasks` it can be an `Op::Create`
/// or `Op::Delete`, i.e. a user write that silently does not happen. Recovering
/// that would mean acknowledging ops rather than consuming them, which is a
/// different design and not one #1170 bought.
pub(crate) fn spawn_eds_worker<T, F>(name: &'static str, rx: mpsc::Receiver<T>, body: F)
where
    T: Send + 'static,
    F: Fn(&mpsc::Receiver<T>) + Send + Sync + 'static,
{
    let rx = Arc::new(Mutex::new(rx));
    spawn_supervised_blocking_bounded(name, move || {
        let rx = rx.lock().unwrap_or_else(PoisonError::into_inner);
        body(&rx);
    });
}

/// First retry delay after a failed EDS worker init.
pub(crate) const INIT_BACKOFF_START: Duration = Duration::from_secs(1);

/// Ceiling for the init retry delay. Keeps the steady-state retry cost
/// negligible on a machine where EDS never comes up (one cheap D-Bus
/// activation attempt per minute) while bounding how stale the boot race
/// can leave us once it *does* come up.
pub(crate) const INIT_BACKOFF_CAP: Duration = Duration::from_mins(1);

/// Next delay in the doubling-with-cap backoff progression.
pub(crate) fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(INIT_BACKOFF_CAP)
}

/// Sleep for `delay`, consuming any messages that arrive on `rx` meanwhile
/// (handing each to `on_msg` — buffer or drop as the caller sees fit) so the
/// backoff can't be short-circuited by a burst of refresh requests. Returns
/// `false` when every sender has disconnected (shutdown) — the caller should
/// stop retrying and exit.
pub(crate) fn wait_backoff<T>(
    rx: &mpsc::Receiver<T>,
    delay: Duration,
    mut on_msg: impl FnMut(T),
) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        match rx.recv_timeout(deadline - now) {
            Ok(msg) => on_msg(msg),
            Err(mpsc::RecvTimeoutError::Timeout) => return true,
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Consecutive all-sources-failed scans before the worker tears down and
/// rebuilds its whole EDS session (registry + client caches). Per-client
/// evict-and-reconnect handles a plain EDS restart within one poll; the
/// session rebuild is the deeper fallback for a registry connection that
/// itself died. Three polls of total failure is unambiguous without being
/// trigger-happy about one bad pass.
const REBUILD_THRESHOLD: u32 = 3;

/// Tracks consecutive scans in which **every** known source failed — the
/// signature of a dead [`hytte_ecal::Registry`] session rather than one
/// flaky calendar. [`Self::record`] says when to rebuild.
#[derive(Debug, Default)]
pub(crate) struct SourceFailureStreak {
    consecutive: u32,
}

impl SourceFailureStreak {
    /// Record one scan's outcome (`total` sources seen, `failed` of them
    /// erroring). Returns `true` when the streak reaches the rebuild
    /// threshold; the streak then resets, so a failed rebuild attempt is
    /// naturally re-paced to every [`REBUILD_THRESHOLD`] polls. A scan with
    /// zero sources never counts — "no calendars configured" is not
    /// distinguishable from a dead registry, and rebuilding on it would
    /// churn forever on machines without EDS sources.
    pub(crate) fn record(&mut self, total: usize, failed: usize) -> bool {
        if total == 0 || failed < total {
            self.consecutive = 0;
            return false;
        }
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive >= REBUILD_THRESHOLD {
            self.consecutive = 0;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Poll `cond` until it holds or `within` elapses; returns whether it held.
    ///
    /// The supervisor's first restart delay is a real 1s sleep and there is no
    /// seam to shorten it from outside `hytte-reactive`, so the restart tests
    /// have to wait for wall clock. They poll rather than sleep a fixed time so
    /// a fast machine finishes in ~1s and a loaded one still passes.
    fn wait_until(within: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cond()
    }

    /// The health row a supervisor keeps for `name`, if it is still live.
    fn health_of(name: &str) -> Option<hytte_reactive::TaskHealth> {
        hytte_reactive::health::snapshot()
            .into_iter()
            .find(|h| h.name == name)
    }

    /// #1170's item 2 for both EDS workers: panic a run, and the *next* run
    /// starts with the same receiver and the ops queued meanwhile still on it.
    ///
    /// One test rather than two because `calendar` and `tasks` reach
    /// supervision through this one function; each service's own `start` is
    /// asserted by the compiler calling it.
    ///
    /// Three mechanisms hang on this, and deleting any of them reddens it:
    ///
    /// * the `spawn_supervised_blocking_bounded` call — without it the panic
    ///   ends the thread and there is no second run at all;
    /// * the shared `Arc<Mutex<Receiver>>` — capture a fresh receiver per run
    ///   and the queued op is gone (and, with `SENDER` set once, unrecoverable);
    /// * `unwrap_or_else(PoisonError::into_inner)` — the panic unwinds holding
    ///   that guard, so a plain `.unwrap()` makes every restarted run panic on
    ///   the lock instead, and this test times out rather than seeing run 2.
    #[test]
    fn a_panicking_eds_worker_restarts_with_the_same_receiver() {
        const NAME: &str = "test-eds-worker-restart";

        let (tx, rx) = mpsc::channel::<u32>();
        let runs = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::new()));

        spawn_eds_worker(NAME, rx, {
            let runs = Arc::clone(&runs);
            let seen = Arc::clone(&seen);
            move |rx| {
                let run = runs.fetch_add(1, Ordering::SeqCst);
                assert!(run > 0, "{NAME}: first run panics, deliberately");
                // Second run: drain forever, so the supervisor stays live (a
                // clean return would stop it and drop its health row) and the
                // test can read both the ops and the row.
                while let Ok(op) = rx.recv() {
                    seen.lock().unwrap_or_else(PoisonError::into_inner).push(op);
                }
            }
        });

        // Queued while run 1 is panicking / the supervisor is backing off.
        tx.send(7).expect("the receiver outlives the panicked run");

        assert!(
            wait_until(Duration::from_secs(15), || {
                seen.lock().unwrap_or_else(PoisonError::into_inner).len() == 1
            }),
            "no second run picked the queued op up: runs={}, health={:?}",
            runs.load(Ordering::SeqCst),
            health_of(NAME)
        );
        assert_eq!(
            *seen.lock().unwrap_or_else(PoisonError::into_inner),
            vec![7],
            "the op queued during the outage was lost or duplicated"
        );

        let health = health_of(NAME).expect("the supervisor publishes a live health row");
        assert_eq!(health.panics, 1, "the restart is not on the health record");
        assert!(
            health.runs >= 2,
            "health says {} run(s); the Stats drawer would not show the restart",
            health.runs
        );

        // Let the second run end so its blocking thread is not held for the
        // rest of the binary.
        drop(tx);
    }

    /// #1196: an EDS worker's clean return — every sender gone, i.e. the
    /// service tearing down — is designed, not a bug that fell out of a loop.
    /// `spawn_eds_worker` must go through `spawn_supervised_blocking_bounded`,
    /// not the plain `spawn_supervised_blocking`, so that shutdown costs a
    /// `debug!` and a released row instead of a `warn!` and a permanent
    /// `Returned` one — both `calendar-eds` and `tasks-eds` return exactly
    /// this way when their channel's last sender drops.
    ///
    /// Falsify by swapping `spawn_eds_worker`'s
    /// `spawn_supervised_blocking_bounded` back to `spawn_supervised_blocking`:
    /// the row then survives forever in `Returned` and this test times out.
    #[test]
    fn a_returning_eds_worker_releases_its_row_instead_of_sticking() {
        const NAME: &str = "test-eds-worker-bounded-return";

        let (tx, rx) = mpsc::channel::<u32>();
        drop(tx); // every sender gone before the worker even starts

        spawn_eds_worker(NAME, rx, |rx| {
            // Mirror the shutdown shape both real workers use: drain until
            // disconnected, then return.
            for _ in rx {}
        });

        assert!(
            wait_until(Duration::from_secs(5), || health_of(NAME).is_none()),
            "a designed return from an EDS worker must release its health row, not leave it \
             Returned forever: health={:?}",
            health_of(NAME)
        );
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut d = INIT_BACKOFF_START;
        let mut seen = Vec::new();
        for _ in 0..8 {
            seen.push(d.as_secs());
            d = next_backoff(d);
        }
        assert_eq!(seen, vec![1, 2, 4, 8, 16, 32, 60, 60]);
    }

    #[test]
    fn wait_backoff_times_out_true() {
        let (_tx, rx) = mpsc::channel::<()>();
        let start = Instant::now();
        assert!(wait_backoff(&rx, Duration::from_millis(30), |()| {}));
        assert!(start.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn wait_backoff_disconnect_false() {
        let (tx, rx) = mpsc::channel::<()>();
        drop(tx);
        assert!(!wait_backoff(&rx, Duration::from_mins(1), |()| {}));
    }

    #[test]
    fn wait_backoff_buffers_messages_and_holds_full_delay() {
        let (tx, rx) = mpsc::channel::<u32>();
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        let mut got = Vec::new();
        let start = Instant::now();
        // Messages arriving must be handed to on_msg but must NOT
        // short-circuit the backoff sleep.
        assert!(wait_backoff(&rx, Duration::from_millis(30), |m| got.push(m)));
        assert!(start.elapsed() >= Duration::from_millis(30));
        assert_eq!(got, vec![1, 2]);
    }

    #[test]
    fn streak_ignores_partial_failure() {
        let mut s = SourceFailureStreak::default();
        for _ in 0..10 {
            assert!(!s.record(3, 2));
        }
    }

    #[test]
    fn streak_ignores_zero_sources() {
        let mut s = SourceFailureStreak::default();
        for _ in 0..10 {
            assert!(!s.record(0, 0));
        }
    }

    #[test]
    fn streak_triggers_on_third_consecutive_total_failure() {
        let mut s = SourceFailureStreak::default();
        assert!(!s.record(2, 2));
        assert!(!s.record(2, 2));
        assert!(s.record(2, 2));
        // Reset after triggering: the next trigger needs three more.
        assert!(!s.record(2, 2));
        assert!(!s.record(2, 2));
        assert!(s.record(2, 2));
    }

    #[test]
    fn streak_resets_on_success() {
        let mut s = SourceFailureStreak::default();
        assert!(!s.record(1, 1));
        assert!(!s.record(1, 1));
        assert!(!s.record(1, 0)); // one healthy scan resets
        assert!(!s.record(1, 1));
        assert!(!s.record(1, 1));
        assert!(s.record(1, 1));
    }
}
