//! Supervised task spawning, and the process panic hook.
//!
//! Every hytte service spawns its I/O tasks onto the shared tokio runtime
//! ([`crate::runtime`]). Nothing joins those `JoinHandle`s, so a task that
//! **panics** simply dies: tokio's default panic hook prints the panic to
//! stderr, the runtime survives, but the task never runs again. For a service
//! task that owns a [`futures_signals::signal::Mutable`], that means the
//! signal is frozen forever — the UI shows the last value and never
//! reconnects, with no visible tell beyond the stderr line.
//!
//! [`spawn_supervised`] closes that gap. It spawns a *factory*-produced future
//! and joins it in a wrapper task. On a panic it logs at `error` level and
//! re-runs the factory with **capped exponential backoff**. On a clean
//! completion (`Ok(())`) it does *not* restart — the task finished its job.
//!
//! [`spawn_supervised_blocking`] is the same supervisor over a
//! `spawn_blocking` closure, for the services whose client library is
//! synchronous (niri's line-based IPC socket, for one). It shares the backoff
//! schedule, the log line, and the restart/stop rules with the async variant —
//! there is deliberately only *one* supervision policy in this crate.
//!
//! Restarting is safe by design: hytte services are thin clients to persistent
//! system daemons (see the crate-level docs on the registry), so a respawned
//! task reconnects to the daemon without losing state. That covers daemon
//! state; it does not cover a poisoned `Mutable` — see the precondition on
//! [`spawn_supervised_blocking`] for the one case restarting cannot recover
//! from.
//!
//! Because a `multi_thread` tokio runtime cannot use the runtime-level
//! `unhandled_panic` policy (that is `current_thread`-only), a per-spawn
//! wrapper like this is the right seam for catching task panics.
//!
//! # Health
//!
//! Every supervisor publishes what it knows about its task — runs, panics,
//! current backoff — to [`crate::health`], which is where a diagnostics view
//! reads "the niri connection has panicked four times in the last minute" from.
//! The bookkeeping lives in [`supervise_runs`], the one loop both spawn
//! functions funnel through, so it covers both variants and any future entry
//! point that reuses that loop (#238, #690, #691).
//!
//! # Extending this API
//!
//! Both spawn functions return `()` on purpose, and it is worth keeping that
//! way until something actually needs otherwise: ~35 call sites invoke them in
//! statement position, so **the return type can still be widened to a handle
//! without touching a single one** — as long as the handle is neither
//! `#[must_use]` nor `Drop`-cancelling. That is the seam #633 wants for
//! cancellation, and the constraint it has to respect: an `ExportHandle`-style
//! "dropping the last clone stops the task" guard cannot be bolted onto these
//! two, because every existing caller drops it immediately. #633 should add a
//! *separate* cancellable entry point (`spawn_supervised_handle`, per its own
//! recommendation) that funnels through [`supervise_runs`] — which is also how
//! it inherits health tracking for free, and where it must call
//! [`crate::health::stopped`] when a cancelled supervisor unwinds.
//!
//! # The process panic hook
//!
//! The supervisor only sees panics on tasks it spawned. [`install_panic_hook`]
//! covers everything else — GTK main-thread callbacks, un-supervised
//! `spawn`/`spawn_blocking` calls, the main thread itself — by routing every
//! panic in the process through `tracing` before delegating to the hook that
//! was already installed. It is an explicit, opt-in call because installing a
//! process-global hook is the *binary's* decision, never a library's: hytte
//! never calls it for you.

use crate::{health, runtime};
use std::future::Future;
use std::panic::PanicHookInfo;
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// Capped-exponential-backoff schedule for [`spawn_supervised`] restarts.
#[derive(Clone, Copy, Debug)]
struct Backoff {
    /// Delay before the first restart after a panic.
    initial: Duration,
    /// Ceiling the delay is clamped to as it doubles.
    max: Duration,
    /// A run that lasted at least this long is treated as "healthy": the
    /// backoff delay resets to `initial` after it, so a task that ran fine for
    /// a while and only then panicked restarts promptly instead of inheriting
    /// an accumulated (long) delay from an unrelated earlier flap.
    reset_after: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            reset_after: Duration::from_secs(30),
        }
    }
}

/// Spawn a supervised task on the shared hytte runtime.
///
/// `factory` is called to produce the task future; it is called again — with
/// capped exponential backoff (1s → 2s → … → 30s cap, reset after a run that
/// stayed healthy for ≥30s) — every time a run **panics**. A clean
/// `Ok(())` completion is taken at face value (the task finished its work) and
/// is *not* restarted; a cancellation (the `JoinHandle` was aborted) likewise
/// stops the supervisor.
///
/// `factory: Fn() -> Fut` (not `FnOnce`) so each restart gets a fresh future;
/// capture cheap `Mutable`/`Arc` clones inside it and clone them per call.
///
/// `name` is a stable, human-readable label used only for log lines
/// (`tracing::error!(service = name, …)`).
pub fn spawn_supervised<F, Fut>(name: &'static str, factory: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    runtime::handle().spawn(supervise(name, factory, Backoff::default()));
}

/// Spawn a supervised **blocking** task on the shared hytte runtime.
///
/// The blocking twin of [`spawn_supervised`], for services whose client
/// library is synchronous (niri's line-based IPC socket, for one). `task` runs
/// on a `spawn_blocking` thread and is re-run — with the same capped
/// exponential backoff (1s → 2s → … → 30s cap, reset after a run that stayed
/// healthy for ≥30s) — every time a run **panics**. A run that returns
/// normally is taken at face value (the task finished its work) and is *not*
/// re-run; a cancellation likewise stops the supervisor. Same policy, same log
/// line, same stop conditions as the async variant: there is one supervision
/// idiom here, not two.
///
/// # What supervision *means* for a blocking task
///
/// **Restart the closure from the top.** That is sound for the same reason it
/// is sound for a future: a hytte service run owns no durable state. Whatever
/// a run allocates — sockets, buffers, parser state — is dropped by the unwind,
/// and the state that matters lives either in the system daemon (which the
/// restarted run reconnects to) or in the `Mutable`s the closure writes, which
/// outlive it — usually. Nothing is lost by starting over, and a frozen signal
/// is the alternative, *except* when the panic unwound while a `Mutable`'s
/// `lock_mut()` write guard was held: `Mutable` is backed by a poisoning
/// `std::sync::RwLock`, and every accessor (`lock_mut`, `set`, `signal_cloned`,
/// …) `.unwrap()`s the lock result. The guard's drop during unwind poisons that
/// `Mutable` permanently, so every later access panics too — not only from the
/// restarted run, but from any reader, including the GTK main thread. Restart
/// does not repair that; it turns a silent freeze into a repeating crash.
///
/// The precondition, which is the caller's to honour: `task` must be
/// **restart-safe** — a panic partway through a run must not leave shared state
/// a fresh run would misread (a half-updated pair of `Mutable`s that later
/// reads treat as consistent, say), and must never unwind while holding a
/// `Mutable`'s write guard. A closure that cannot promise that should not be
/// supervised: silently re-running it on corrupt (or poisoned) state is worse
/// than leaving it dead. Restarts are unbounded — there is no give-up count —
/// but the 30s backoff cap bounds a permanently-panicking task to one run per
/// 30s.
///
/// Panics are captured through tokio's `JoinHandle` (`JoinError::is_panic()`),
/// not `catch_unwind`, so callers are spared an `UnwindSafe` bound. This does
/// mean supervision requires **unwinding** panics; under `panic = "abort"` the
/// process is gone before any supervisor runs (the workspace does not set it).
///
/// `task: Fn()` (not `FnOnce`) so it can be re-run; it is held in an `Arc` and
/// invoked from a fresh blocking thread per run, hence the `Send + Sync` bound
/// — the async variant needs only `Send` because its factory stays put on one
/// task. Capture cheap `Mutable`/`Arc` clones and use them by reference.
///
/// `name` is a stable, human-readable label used only for log lines.
pub fn spawn_supervised_blocking<F>(name: &'static str, task: F)
where
    F: Fn() + Send + Sync + 'static,
{
    runtime::handle().spawn(supervise_blocking(name, Arc::new(task), Backoff::default()));
}

/// The async supervision loop. Split out from [`spawn_supervised`] so the
/// backoff schedule is injectable for hermetic tests (a zero-delay `Backoff`
/// keeps the retry-path test fast and non-flaky).
async fn supervise<F, Fut>(name: &'static str, factory: F, cfg: Backoff)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    supervise_runs(name, move || runtime::handle().spawn(factory()), cfg).await;
}

/// The blocking supervision loop — [`supervise`]'s twin, likewise split out so
/// tests can inject a zero-delay `Backoff`.
async fn supervise_blocking<F>(name: &'static str, task: Arc<F>, cfg: Backoff)
where
    F: Fn() + Send + Sync + 'static,
{
    supervise_runs(
        name,
        move || {
            let task = Arc::clone(&task);
            runtime::handle().spawn_blocking(move || task())
        },
        cfg,
    )
    .await;
}

/// The one supervision loop both variants run.
///
/// `spawn_run` starts a single run and hands back its `JoinHandle`; everything
/// above it — restart policy, backoff schedule, log lines, [`crate::health`]
/// bookkeeping — is shared, which is what keeps the async and blocking
/// supervisors from drifting apart.
async fn supervise_runs<S>(name: &'static str, spawn_run: S, cfg: Backoff)
where
    S: Fn() -> JoinHandle<()> + Send + 'static,
{
    // The health entry is this supervisor's, and lives exactly as long as this
    // loop: every `return` below drops it. See `health`'s module docs on why
    // nothing is retained for a supervisor that has stopped.
    let id = health::register(name);
    let mut delay = cfg.initial;
    loop {
        let started = Instant::now();
        health::run_started(id);
        // Spawn onto the shared runtime so tokio catches a panic and surfaces
        // it as `JoinError::is_panic()` on the handle we await here.
        let join = spawn_run();

        match join.await {
            // The task returned on its own — it finished its job. Do not
            // restart (restarting a completed task is the caller's bug, not a
            // failure to recover from).
            Ok(()) => {
                health::stopped(id);
                return;
            }

            Err(err) if err.is_panic() => {
                let ran = started.elapsed();
                // A healthy run resets the backoff so an isolated panic after
                // a long healthy stretch restarts promptly.
                let after_healthy_run = ran >= cfg.reset_after;
                if after_healthy_run {
                    delay = cfg.initial;
                }
                // Same verdict feeds the streak counter, so the published
                // `consecutive_panics` and the backoff can never disagree about
                // what counts as a healthy run.
                let counts = health::panicked(id, delay, after_healthy_run);
                tracing::error!(
                    service = name,
                    ran_secs = ran.as_secs_f64(),
                    backoff_secs = delay.as_secs_f64(),
                    panics = counts.total,
                    consecutive_panics = counts.consecutive,
                    "supervised task panicked; restarting after backoff"
                );
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                delay = delay.saturating_mul(2).min(cfg.max);
            }

            // Not a panic: the join handle was cancelled/aborted. There is no
            // work to resume, so stop supervising.
            Err(err) => {
                health::stopped(id);
                tracing::debug!(
                    service = name,
                    error = %err,
                    "supervised task cancelled; stopping supervisor"
                );
                return;
            }
        }
    }
}

/// A boxed `std::panic` hook, in the shape [`std::panic::set_hook`] wants.
type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static>;

/// Route every panic in this process through `tracing`, then hand it on to the
/// hook that was installed before.
///
/// Without this, panics reach stderr through the default hook: no timestamp,
/// no level, no target, and — under `journald` — interleaved with the
/// `tracing` log by luck rather than by format. This adds one structured
/// `error!` record (thread, source location, message) so a panic is greppable
/// next to the log lines that led to it.
///
/// **This is a process-global side effect, so a library must never do it
/// behind your back.** Nothing in hytte calls this; a binary calls it once,
/// from `main`, and only after its `tracing` subscriber is installed — an
/// event emitted before there is a subscriber goes nowhere.
///
/// The previously-installed hook is **chained, not replaced**: this logs and
/// then calls it. That keeps whatever the platform, the test harness, or
/// another library set up working — including the default hook's
/// `RUST_BACKTRACE` handling, which is worth more than the cost of the panic
/// message appearing twice (the second copy is the one carrying the
/// backtrace).
///
/// Calling this more than once is a no-op after the first: re-chaining would
/// log every panic once per call.
///
/// This is orthogonal to [`spawn_supervised`] and
/// [`spawn_supervised_blocking`], and they compose: a panic on a supervised
/// task produces this hook's record *and* the supervisor's
/// `restarting after backoff` line.
pub fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        std::panic::set_hook(logging_panic_hook(std::panic::take_hook()));
    });
}

/// Build the hook [`install_panic_hook`] installs: log through `tracing`, then
/// delegate to `prev`.
///
/// Split out from the installer — which is `Once`-guarded and therefore only
/// observable once per process — so a test can compose it against a sentinel
/// `prev` and check the delegation actually happens.
fn logging_panic_hook(prev: PanicHook) -> PanicHook {
    Box::new(move |info| {
        let location = info
            .location()
            .map_or_else(|| "<unknown>".to_owned(), ToString::to_string);
        tracing::error!(
            thread = std::thread::current().name().unwrap_or("<unnamed>"),
            location = %location,
            // `payload_as_str` covers the `&str` and `String` payloads that
            // `panic!`/`assert!`/`unwrap` produce; anything else came from a
            // hand-rolled `panic_any` and has no printable form here.
            payload = info.payload_as_str().unwrap_or("<non-string payload>"),
            "thread panicked"
        );
        prev(info);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, PoisonError};

    /// A zero-delay backoff, so the retry-path tests are fast and
    /// timing-independent.
    const ZERO_BACKOFF: Backoff = Backoff {
        initial: Duration::ZERO,
        max: Duration::ZERO,
        reset_after: Duration::ZERO,
    };

    /// Zero-delay like [`ZERO_BACKOFF`], but with a `reset_after` no run can
    /// ever reach — so a panic streak *accumulates*.
    ///
    /// `ZERO_BACKOFF` cannot show that: its `reset_after` is zero, which makes
    /// every run healthy by definition and pins `consecutive_panics` at 1.
    const NEVER_RESET: Backoff = Backoff {
        initial: Duration::ZERO,
        max: Duration::ZERO,
        reset_after: Duration::MAX,
    };

    // ---- capturing what this module logs ---------------------------------
    //
    // Two tests below assert that a panic was *logged*. Capturing a `tracing`
    // event is harder than it looks, and the obvious tool —
    // `tracing::subscriber::with_default` — is the wrong one here for a reason
    // that is not the obvious one either. Both are worth stating, because the
    // first explanation is plausible, wrong, and is what shipped as this
    // module's original test comment.
    //
    // *Not* the reason: "`with_default` is thread-local and the supervisor
    // logs on a worker thread". `Handle::block_on` really does drive the
    // supervisor future on the calling thread, so the `error!` really is
    // emitted on the test thread. Thread-locality alone would have failed
    // deterministically; this failed intermittently.
    //
    // The actual reason: `tracing` caches each callsite's `Interest`
    // **process-globally**, and the first thread to execute a given `error!`
    // decides that cache for the whole process. On a thread with no subscriber
    // the dispatcher resolves to `NoSubscriber`, whose `register_callsite`
    // returns `Interest::never()` — and from then on the macro short-circuits
    // on *every* thread, including one sitting inside `with_default`. The
    // panicking tests in this module reach the supervisor's `error!` from
    // their own subscriber-less threads, concurrently, so whether the
    // log-asserting test ever saw its own event came down to which thread got
    // to that callsite first. Green locally, red in CI, and no amount of
    // asserting harder on the test thread would have helped.
    //
    // So: one **process-global** subscriber, installed once with
    // `set_global_default`. It is the dispatcher every thread resolves to, so
    // it both receives the event wherever it is emitted and makes the cached
    // `Interest` `always` rather than `never`.

    /// The tags the log-asserting tests claim — one per test.
    ///
    /// A process-global subscriber sees *every* event in this test binary,
    /// including those of tests running concurrently on other threads, so it
    /// cannot be scoped to one test the way `with_default` appeared to be. The
    /// usual answer is to serialise the log-asserting tests behind a mutex,
    /// but that only holds off *each other* — the tests that panic without
    /// asserting anything (`respawns_after_panic_until_clean_completion` and
    /// its blocking twin) emit this module's `error!` too, and would inflate
    /// any shared count.
    ///
    /// Tagging is what isolates them instead, and needs no serialisation: a
    /// tag is a string that appears verbatim as a **field value** on the event
    /// a test expects — the supervisor's `service`, or the panic hook's
    /// `payload` — and [`ErrorCounter`] keys its counts by tag, so an event
    /// belonging to another test is counted under that other test's tag.
    ///
    /// **The constraint is that no two tests may share a tag**; that is the
    /// entire isolation mechanism. A tag must also be listed here before a
    /// test uses it — an unlisted tag is never counted, so the mistake shows
    /// up as a test reading 0 rather than as one test silently consuming
    /// another's events.
    const CAPTURE_TAGS: &[&str] = &[
        // The `service` name `a_panicking_blocking_task_is_logged_and_restarted`
        // supervises under.
        "test-blocking-log",
        // The payload `the_panic_hook_logs_then_delegates_to_the_previous_hook`
        // panics with.
        "supervisor panic-hook test",
    ];

    /// Per-tag counts of `ERROR` events from this module.
    ///
    /// Never reset: a tag belongs to exactly one test, which runs once per
    /// process, so the cumulative count *is* that test's count.
    static TAGGED_ERRORS: Mutex<BTreeMap<&'static str, usize>> = Mutex::new(BTreeMap::new());

    /// Install the global `ERROR`-counting subscriber, once per process.
    ///
    /// **Every test in this module calls this first**, not only the two that
    /// assert on logs — uniformly, so there is no rule about which ones need
    /// it. The point is that no supervisor `error!` may execute before the
    /// global subscriber exists: whichever subscriber-less thread reaches that
    /// callsite first pins its process-global `Interest` to `never`, and the
    /// log-asserting tests then read 0 no matter what they do afterwards.
    fn install_error_counter() {
        static INSTALLED: Once = Once::new();
        INSTALLED.call_once(|| {
            tracing::subscriber::set_global_default(ErrorCounter)
                .expect("nothing else installs a global subscriber in this test binary");
        });
    }

    /// How many `ERROR` events from this module have carried `tag`.
    fn logged_errors(tag: &'static str) -> usize {
        TAGGED_ERRORS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(tag)
            .copied()
            .unwrap_or_default()
    }

    /// A factory that panics its first three runs then completes cleanly must
    /// be retried until the clean completion — four calls total — and then
    /// stop (no restart on `Ok(())`).
    #[test]
    fn respawns_after_panic_until_clean_completion() {
        install_error_counter();

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_factory = calls.clone();

        runtime::handle().block_on(async move {
            supervise(
                "test-retry",
                move || {
                    let calls = calls_factory.clone();
                    async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        assert!(n >= 3, "panic on the first three runs");
                    }
                },
                ZERO_BACKOFF,
            )
            .await;
        });

        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    /// The happy path: a factory that completes cleanly on its first run is
    /// called exactly once — the supervisor adds no restarts.
    #[test]
    fn clean_completion_runs_exactly_once() {
        install_error_counter();

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_factory = calls.clone();

        runtime::handle().block_on(async move {
            supervise(
                "test-clean",
                move || {
                    let calls = calls_factory.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                    }
                },
                Backoff::default(),
            )
            .await;
        });

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A blocking closure that panics its first three runs then returns
    /// normally must be re-run until that clean return — four runs total — and
    /// then stop. The blocking mirror of
    /// `respawns_after_panic_until_clean_completion`, and the pin on the
    /// decision that supervision of a blocking task means *restart it*.
    #[test]
    fn blocking_task_respawns_after_panic_until_clean_completion() {
        install_error_counter();

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_task = Arc::clone(&calls);

        runtime::handle().block_on(supervise_blocking(
            "test-blocking-retry",
            Arc::new(move || {
                let n = calls_task.fetch_add(1, Ordering::SeqCst);
                assert!(n >= 3, "panic on the first three runs");
            }),
            ZERO_BACKOFF,
        ));

        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    /// The panic must not vanish: the panicked run of a supervised blocking
    /// task emits exactly one `error!` from this module *and* is re-run.
    ///
    /// The `error!` is captured by the process-global [`ErrorCounter`] rather
    /// than a thread-local `with_default` subscriber, because the cache that
    /// decides whether that `error!` runs at all is itself process-global —
    /// see the note above [`CAPTURE_TAGS`]. Concurrency is handled by keying
    /// the count on the `service` field: the panics the sibling tests raise at
    /// the same time carry their own service names and are counted under their
    /// own tags, so this assertion stays exact without serialising anything.
    ///
    /// The closure itself runs on a blocking-pool thread; its panic is not,
    /// and need not be, captured here — that is the panic *hook*'s job, and
    /// [`the_panic_hook_logs_then_delegates_to_the_previous_hook`] covers it.
    #[test]
    fn a_panicking_blocking_task_is_logged_and_restarted() {
        install_error_counter();

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_task = Arc::clone(&calls);

        runtime::handle().block_on(supervise_blocking(
            "test-blocking-log",
            Arc::new(move || {
                let n = calls_task.fetch_add(1, Ordering::SeqCst);
                assert!(n >= 1, "panic on the first run only");
            }),
            ZERO_BACKOFF,
        ));

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the panicked run is re-run"
        );
        assert_eq!(
            logged_errors("test-blocking-log"),
            1,
            "the panic is reported once, at error level"
        );
    }

    /// The supervisor publishes what it knows to [`crate::health`]: a run
    /// counter that ticks before each run, panic counters that tick after each
    /// panic, and an entry that is gone once supervision ends.
    ///
    /// Observed *from inside the supervised closure*, because that is the only
    /// vantage point where the entry is live — `block_on` returns only after
    /// the supervisor has stopped and dropped it. Which is itself the last
    /// assertion here: the table tracks what is being supervised now, not a
    /// history (`mpris-player`/`tray-item` supervise one task per discovered
    /// item, so retaining finished entries would leak).
    #[test]
    fn the_supervisor_publishes_its_task_health() {
        const NAME: &str = "test-health-supervised";

        install_error_counter();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_task = Arc::clone(&seen);

        runtime::handle().block_on(supervise_blocking(
            NAME,
            Arc::new(move || {
                let mine = health::snapshot()
                    .into_iter()
                    .find(|task| task.name == NAME)
                    .expect("the supervisor registers before it starts a run");
                seen_task
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(mine);
                assert!(mine.runs >= 3, "panic on the first two runs");
            }),
            NEVER_RESET,
        ));

        let seen = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let progression: Vec<_> = seen
            .iter()
            .map(|task| (task.runs, task.panics, task.consecutive_panics))
            .collect();
        assert_eq!(
            progression,
            vec![(1, 0, 0), (2, 1, 1), (3, 2, 2)],
            "runs tick before each run; panics and the streak tick after each panic"
        );
        assert!(
            seen[0].last_panic.is_none() && seen[2].last_panic.is_some(),
            "the panic timestamp appears only once there has been a panic"
        );
        assert!(
            seen.iter()
                .all(|task| task.state == crate::health::TaskState::Running),
            "a run in flight always reads Running"
        );
        assert!(
            !health::snapshot().iter().any(|task| task.name == NAME),
            "the entry is dropped when the supervisor stops"
        );
    }

    /// The installed hook logs the panic through `tracing` *and* passes it on:
    /// it must chain to the previous hook, never replace it.
    ///
    /// This used a thread-local `with_default` subscriber too, and passed —
    /// but only by luck, and it carried the same latent flake as its sibling
    /// (see the note above [`CAPTURE_TAGS`]). The hook's `error!` is a
    /// callsite like any other, and while this test's hook is installed *any*
    /// panicking thread in the binary runs it: a sibling test's tokio thread
    /// reaching it first would have pinned its process-global `Interest` to
    /// `never` and left this test reading 0 forever after. So it reads the
    /// global [`ErrorCounter`] too.
    #[test]
    fn the_panic_hook_logs_then_delegates_to_the_previous_hook() {
        install_error_counter();

        let delegated = Arc::new(AtomicUsize::new(0));
        let delegated_hook = Arc::clone(&delegated);

        let sentinel: PanicHook = Box::new(move |_| {
            delegated_hook.fetch_add(1, Ordering::SeqCst);
        });

        // `set_hook` is process-global, so this briefly diverts every panic in
        // the test binary (the other tests in this module panic on purpose, on
        // tokio threads). Restore the previous hook straight after, and count
        // delegations with `>=` rather than `==` so a concurrent test's panic
        // landing in the sentinel cannot make this flake.
        let saved = std::panic::take_hook();
        std::panic::set_hook(logging_panic_hook(sentinel));
        let outcome = std::panic::catch_unwind(|| panic!("supervisor panic-hook test"));
        std::panic::set_hook(saved);

        assert!(outcome.is_err(), "the panic still unwinds");
        assert!(
            delegated.load(Ordering::SeqCst) >= 1,
            "the previous hook is still called"
        );
        // The counter is global, so it also sees the sibling tests' panics
        // routed through this same hook while it is installed. Those carry
        // their own payloads; keying on *this* test's payload is what keeps
        // the count exact.
        assert_eq!(
            logged_errors("supervisor panic-hook test"),
            1,
            "the panic is logged through tracing"
        );
    }

    /// Counts this module's `ERROR` events into [`TAGGED_ERRORS`], keyed by
    /// whichever [`CAPTURE_TAGS`] entry appears among the event's string
    /// fields. Hand-rolled so the crate needs no `tracing-subscriber`
    /// dev-dependency.
    ///
    /// Installed process-globally by [`install_error_counter`], which is what
    /// makes it visible from every thread — both as the receiver of the event
    /// and as the subscriber `tracing` asks when it caches a callsite's
    /// `Interest`.
    struct ErrorCounter;

    impl tracing::Subscriber for ErrorCounter {
        fn enabled(&self, _meta: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _id: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _id: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let meta = event.metadata();
            if *meta.level() != tracing::Level::ERROR
                || meta.target() != "hytte_reactive::supervisor"
            {
                return;
            }
            let mut visitor = TagVisitor(None);
            event.record(&mut visitor);
            if let Some(tag) = visitor.0 {
                *TAGGED_ERRORS
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entry(tag)
                    .or_default() += 1;
            }
        }

        fn enter(&self, _id: &tracing::span::Id) {}

        fn exit(&self, _id: &tracing::span::Id) {}
    }

    /// Finds the first [`CAPTURE_TAGS`] entry among an event's `&str` fields.
    struct TagVisitor(Option<&'static str>);

    impl tracing::field::Visit for TagVisitor {
        fn record_str(&mut self, _field: &tracing::field::Field, value: &str) {
            if self.0.is_none() {
                self.0 = CAPTURE_TAGS.iter().copied().find(|&tag| tag == value);
            }
        }

        /// Non-string fields — the supervisor's `ran_secs`/`backoff_secs`, the
        /// hook's `Display`-formatted `location` — can never carry a tag.
        fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
    }
}
