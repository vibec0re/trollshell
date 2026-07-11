//! Supervised task spawning.
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
//! Restarting is safe by design: hytte services are thin async clients to
//! persistent system daemons (see the crate-level docs on the registry), so a
//! respawned task reconnects to the daemon without losing state.
//!
//! Because a `multi_thread` tokio runtime cannot use the runtime-level
//! `unhandled_panic` policy (that is `current_thread`-only), a per-spawn
//! wrapper like this is the right seam for catching task panics.

use crate::runtime;
use std::future::Future;
use std::time::{Duration, Instant};

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

/// The supervision loop. Split out from [`spawn_supervised`] so the backoff
/// schedule is injectable for hermetic tests (a zero-delay `Backoff` keeps the
/// retry-path test fast and non-flaky).
async fn supervise<F, Fut>(name: &'static str, factory: F, cfg: Backoff)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut delay = cfg.initial;
    loop {
        let started = Instant::now();
        // Spawn onto the shared runtime so tokio catches a panic and surfaces
        // it as `JoinError::is_panic()` on the handle we await here.
        let join = runtime::handle().spawn(factory());

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A factory that panics its first three runs then completes cleanly must
    /// be retried until the clean completion — four calls total — and then
    /// stop (no restart on `Ok(())`). Uses a zero-delay backoff so the test is
    /// fast and timing-independent.
    #[test]
    fn respawns_after_panic_until_clean_completion() {
        let zero = Backoff {
            initial: Duration::ZERO,
            max: Duration::ZERO,
            reset_after: Duration::ZERO,
        };
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
                zero,
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
}
