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
//! task reconnects to the daemon without losing state.
//!
//! Because a `multi_thread` tokio runtime cannot use the runtime-level
//! `unhandled_panic` policy (that is `current_thread`-only), a per-spawn
//! wrapper like this is the right seam for catching task panics.
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

use crate::runtime;
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
/// outlive it. Nothing is lost by starting over, and a frozen signal is the
/// alternative.
///
/// The precondition, which is the caller's to honour: `task` must be
/// **restart-safe** — a panic partway through a run must not leave shared state
/// a fresh run would misread (a half-updated pair of `Mutable`s that later
/// reads treat as consistent, say). A closure that cannot promise that should
/// not be supervised: silently re-running it on corrupt state is worse than
/// leaving it dead. Restarts are unbounded — there is no give-up count — but
/// the 30s backoff cap bounds a permanently-panicking task to one run per 30s.
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
/// above it — restart policy, backoff schedule, log lines — is shared, which is
/// what keeps the async and blocking supervisors from drifting apart.
async fn supervise_runs<S>(name: &'static str, spawn_run: S, cfg: Backoff)
where
    S: Fn() -> JoinHandle<()> + Send + 'static,
{
    let mut delay = cfg.initial;
    loop {
        let started = Instant::now();
        // Spawn onto the shared runtime so tokio catches a panic and surfaces
        // it as `JoinError::is_panic()` on the handle we await here.
        let join = spawn_run();

        match join.await {
            // The task returned on its own — it finished its job. Do not
            // restart (restarting a completed task is the caller's bug, not a
            // failure to recover from).
            Ok(()) => return,

            Err(err) if err.is_panic() => {
                let ran = started.elapsed();
                // A healthy run resets the backoff so an isolated panic after
                // a long healthy stretch restarts promptly.
                if ran >= cfg.reset_after {
                    delay = cfg.initial;
                }
                tracing::error!(
                    service = name,
                    ran_secs = ran.as_secs_f64(),
                    backoff_secs = delay.as_secs_f64(),
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A zero-delay backoff, so the retry-path tests are fast and
    /// timing-independent.
    const ZERO_BACKOFF: Backoff = Backoff {
        initial: Duration::ZERO,
        max: Duration::ZERO,
        reset_after: Duration::ZERO,
    };

    /// A factory that panics its first three runs then completes cleanly must
    /// be retried until the clean completion — four calls total — and then
    /// stop (no restart on `Ok(())`).
    #[test]
    fn respawns_after_panic_until_clean_completion() {
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

    /// The panic must not vanish: each panicked run of a supervised blocking
    /// task emits exactly one `error!` from this module *and* is re-run.
    ///
    /// `Handle::block_on` drives the supervisor future on this thread, so the
    /// thread-local subscriber installed by `with_default` sees the
    /// supervisor's own log line (the closure runs on a blocking-pool thread;
    /// its events are not, and need not be, captured).
    #[test]
    fn a_panicking_blocking_task_is_logged_and_restarted() {
        let errors = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_task = Arc::clone(&calls);

        tracing::subscriber::with_default(ErrorCounter(Arc::clone(&errors)), || {
            runtime::handle().block_on(supervise_blocking(
                "test-blocking-log",
                Arc::new(move || {
                    let n = calls_task.fetch_add(1, Ordering::SeqCst);
                    assert!(n >= 1, "panic on the first run only");
                }),
                ZERO_BACKOFF,
            ));
        });

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the panicked run is re-run"
        );
        assert_eq!(
            errors.load(Ordering::SeqCst),
            1,
            "the panic is reported once, at error level"
        );
    }

    /// The installed hook logs the panic through `tracing` *and* passes it on:
    /// it must chain to the previous hook, never replace it.
    #[test]
    fn the_panic_hook_logs_then_delegates_to_the_previous_hook() {
        let delegated = Arc::new(AtomicUsize::new(0));
        let delegated_hook = Arc::clone(&delegated);
        let errors = Arc::new(AtomicUsize::new(0));

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
        let outcome = tracing::subscriber::with_default(ErrorCounter(Arc::clone(&errors)), || {
            std::panic::catch_unwind(|| panic!("supervisor panic-hook test"))
        });
        std::panic::set_hook(saved);

        assert!(outcome.is_err(), "the panic still unwinds");
        assert!(
            delegated.load(Ordering::SeqCst) >= 1,
            "the previous hook is still called"
        );
        // Thread-local subscriber: only the panic raised on *this* thread is
        // counted, so this is exact.
        assert_eq!(
            errors.load(Ordering::SeqCst),
            1,
            "the panic is logged through tracing"
        );
    }

    /// Counts `ERROR` events this module emits on the current thread.
    /// Hand-rolled so the crate needs no `tracing-subscriber` dev-dependency.
    struct ErrorCounter(Arc<AtomicUsize>);

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
            if *meta.level() == tracing::Level::ERROR
                && meta.target() == "hytte_reactive::supervisor"
            {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        fn enter(&self, _id: &tracing::span::Id) {}

        fn exit(&self, _id: &tracing::span::Id) {}
    }
}
