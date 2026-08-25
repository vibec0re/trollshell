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
//! [`spawn_supervised_handle`] is [`spawn_supervised`] plus a way to stop it:
//! it returns a [`SupervisorHandle`] whose [`cancel`](SupervisorHandle::cancel)
//! aborts the run in flight and ends the restart loop. Reach for it only when
//! something genuinely has to be torn down — swapping the Wi-Fi service's
//! wireless backend at runtime is the case it was added for (#633) — and for
//! everything else prefer the plain [`spawn_supervised`], whose task lives as
//! long as the process.
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
//! The bookkeeping lives in [`supervise_runs`], the one loop every spawn
//! function funnels through, so it covers all three variants and any future
//! entry point that reuses that loop (#238, #690, #691).
//!
//! # Extending this API
//!
//! [`spawn_supervised`] and [`spawn_supervised_blocking`] return `()` on
//! purpose, and it is worth keeping that way: ~40 call sites invoke them in
//! statement position, so **the return type can still be widened to a handle
//! without touching a single one** — as long as the handle is neither
//! `#[must_use]` nor `Drop`-cancelling. An `ExportHandle`-style "dropping the
//! last clone stops the task" guard cannot be bolted onto these two, because
//! every existing caller drops it on the same line and would silently cancel
//! every supervised task in the shell.
//!
//! That is why #633 added [`spawn_supervised_handle`] as a *separate* entry
//! point rather than changing these two, and why [`SupervisorHandle`] is
//! deliberately neither `#[must_use]` nor `Drop`-cancelling: keeping it inert
//! on drop is what leaves the widening above open, so the two older entry
//! points could one day hand back the same type without a single call site
//! changing meaning. Do not add either property to it.
//!
//! All three funnel through [`supervise_runs`], which is how the cancellable
//! variant inherits [`crate::health`] tracking for free — including releasing
//! its row via [`crate::health::stopped`] when a cancelled supervisor unwinds,
//! without which every backend switch would leak one.
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
use tokio::sync::watch;
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
    // No `Stop`: this entry point hands back nothing, so nothing can ever ask
    // the supervisor to stop. `spawn_supervised_handle` is the one that can.
    runtime::handle().spawn(supervise(name, factory, Backoff::default(), None));
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

// ── Cancellable supervision ──────────────────────────────────────────────────

/// A handle that can stop a supervisor started by [`spawn_supervised_handle`].
///
/// Cheap to clone; every clone can cancel, and cancelling is idempotent.
///
/// # What cancellation guarantees
///
/// [`cancel`](Self::cancel) asks the supervisor to stop and returns
/// immediately. The supervisor then, in order: aborts the run in flight (or
/// abandons the backoff sleep it was in, or declines to start the next run —
/// whichever it was doing), **waits for that run to finish unwinding**,
/// releases its [`crate::health`] row, and returns. [`stopped`](Self::stopped)
/// resolves at that point, so `cancel(); stopped().await;` is the way to know
/// nothing the task owned is still writing.
///
/// A cancelled supervisor never runs the factory again. There is no resume; a
/// new supervisor is a new [`spawn_supervised_handle`] call.
///
/// # What cancellation cannot corrupt, and what it can
///
/// A run is aborted at an `await` point, which is the reason cancellation is
/// safe where a panic is not. The [`spawn_supervised_blocking`] docs describe
/// how a panic that unwinds while a `Mutable`'s `lock_mut()` guard is held
/// poisons that `Mutable` for the whole process; an abort cannot do that,
/// because a write guard is `!Send` and so cannot be held across the `await`
/// where the abort lands — a future that tried would not compile.
///
/// What cancellation *does* share with a restart is the partial-update hazard:
/// a run stopped between two related writes leaves them inconsistent, exactly
/// as a panicked-and-restarted run would. The restart-safety precondition on
/// [`spawn_supervised_blocking`] therefore applies unchanged to anything you
/// cancel — with the addition that the cancelling side usually wants to reset
/// the state the task was writing once [`stopped`](Self::stopped) has resolved,
/// rather than leave a half-finished snapshot on screen.
///
/// # Dropping is not cancelling
///
/// Dropping every clone leaves the supervisor running, deliberately: see this
/// module's "Extending this API" note for why this type must stay inert on drop
/// (and un-`#[must_use]`). Fire-and-forget callers should use
/// [`spawn_supervised`] and say so.
#[derive(Clone)]
pub struct SupervisorHandle {
    /// Flipped to `true` by [`SupervisorHandle::cancel`]; watched by
    /// [`supervise_runs`]. Behind an `Arc` so a clone cannot resurrect a
    /// dropped sender, and so dropping clones is observably different from
    /// cancelling.
    cancel: Arc<watch::Sender<bool>>,
    /// Paired with a sender the supervision loop owns and drops on its way out,
    /// which is what [`SupervisorHandle::stopped`] waits for.
    stopped: watch::Receiver<()>,
}

impl std::fmt::Debug for SupervisorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisorHandle")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl SupervisorHandle {
    /// Ask the supervisor to stop. Returns immediately — the teardown it
    /// triggers is asynchronous; await [`stopped`](Self::stopped) to observe
    /// its completion. Idempotent.
    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }

    /// Whether [`cancel`](Self::cancel) has been called on this handle or any
    /// of its clones. Says nothing about whether the supervisor has finished
    /// unwinding yet — that is [`stopped`](Self::stopped).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    /// Resolve once the supervisor has stopped and released its
    /// [`crate::health`] row.
    ///
    /// Not cancellation-specific: it also resolves when the supervised task
    /// completed cleanly on its own (`Ok(())`, which the supervisor takes at
    /// face value and does not restart), so it doubles as "await this task's
    /// completion". Resolves immediately if the supervisor has already stopped.
    ///
    /// After a [`cancel`](Self::cancel) this waits for the aborted run to
    /// unwind, and a run only reaches its abort at an `await`: a run that never
    /// yields — a tight CPU loop with no `await` in it — cannot be aborted, and
    /// this waits for it as long as it takes. Callers that cannot block on a
    /// misbehaving task should wrap this in a `tokio::time::timeout`.
    pub async fn stopped(&self) {
        let mut stopped = self.stopped.clone();
        // `changed()` errors once the loop's sender is dropped, which is
        // exactly the event being waited for. Nothing is ever *sent* on this
        // channel; the drop is the whole message.
        while stopped.changed().await.is_ok() {}
    }
}

/// The supervision loop's half of a [`SupervisorHandle`].
struct Stop {
    /// Reads `true` once some handle has cancelled.
    cancel: watch::Receiver<bool>,
    /// Never sent on — dropped when the loop returns, which is what
    /// [`SupervisorHandle::stopped`] observes.
    _stopped: watch::Sender<()>,
}

/// Spawn a supervised task that can be stopped again.
///
/// Identical to [`spawn_supervised`] in every respect that matters at runtime —
/// same factory contract, same capped exponential backoff, same restart-on-panic
/// and stop-on-clean-completion rules, same [`crate::health`] publishing,
/// because it is literally the same loop — except that it hands back a
/// [`SupervisorHandle`] with which the task can later be torn down.
///
/// Use it only where teardown is a real requirement: the shell's long-lived
/// service tasks should stay on [`spawn_supervised`], whose lack of a handle is
/// an accurate statement that nothing stops them. The case this exists for is
/// swapping one implementation of a service for another at runtime — the Wi-Fi
/// service moving between iwd and `NetworkManager` (#633) — where the outgoing
/// backend's watcher must stop writing the shared state before the incoming
/// one starts.
///
/// There is no blocking twin, and that is not an oversight: a
/// `spawn_blocking` closure cannot be aborted once it has started running
/// (tokio's `JoinHandle::abort` can only stop a blocking task that has not yet
/// been picked up by a pool thread), so a cancellable
/// `spawn_supervised_blocking` could promise far less than this one does. A
/// blocking task that needs to stop should poll for it itself.
#[must_use]
pub fn spawn_supervised_handle<F, Fut>(name: &'static str, factory: F) -> SupervisorHandle
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    spawn_supervised_handle_with(name, factory, Backoff::default())
}

/// [`spawn_supervised_handle`] with an injectable backoff schedule, so the
/// hermetic tests can drive the cancel paths without wall-clock sleeps — the
/// same seam [`supervise`] is split out for.
fn spawn_supervised_handle_with<F, Fut>(
    name: &'static str,
    factory: F,
    cfg: Backoff,
) -> SupervisorHandle
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let (stopped_tx, stopped_rx) = watch::channel(());
    let stop = Stop {
        cancel: cancel_rx,
        _stopped: stopped_tx,
    };
    runtime::handle().spawn(supervise(name, factory, cfg, Some(stop)));
    SupervisorHandle {
        cancel: Arc::new(cancel_tx),
        stopped: stopped_rx,
    }
}

/// Resolve once the supervisor has been asked to stop — and **never** in any
/// other circumstance.
///
/// Two of those circumstances are worth naming, because both would be bugs if
/// they resolved: an un-cancellable supervisor (`stop` is `None`, i.e. one of
/// the two handle-less entry points) and a cancellable one whose every
/// [`SupervisorHandle`] clone was dropped without cancelling. The second is
/// what makes "dropping is not cancelling" true rather than aspirational —
/// once the sender is gone `changed()` starts erroring, and reading that as
/// cancellation would stop every supervisor whose handle the caller let fall
/// out of scope.
async fn cancel_requested(stop: &mut Option<Stop>) {
    let Some(stop) = stop.as_mut() else {
        std::future::pending::<()>().await;
        return;
    };
    // Read-then-drop the borrow: a `watch::Ref` held across the await below
    // would not be `Send`, and the supervisor future has to be.
    let already = *stop.cancel.borrow_and_update();
    if already {
        return;
    }
    while stop.cancel.changed().await.is_ok() {
        if *stop.cancel.borrow_and_update() {
            return;
        }
    }
    std::future::pending::<()>().await;
}

/// Whether a cancel has landed, without waiting for one.
fn cancel_pending(stop: Option<&Stop>) -> bool {
    stop.is_some_and(|stop| *stop.cancel.borrow())
}

/// Wind a cancelled supervisor down: release its health row and say so.
///
/// The health call is the load-bearing half. Entries there are live, not
/// historical (see [`crate::health`]'s module docs), and a cancel path that
/// skipped this would leak one row per teardown — for #633's backend switch,
/// one per switch, forever.
fn finish_cancelled(name: &'static str, id: health::TaskId) {
    health::stopped(id);
    tracing::debug!(
        service = name,
        "supervised task cancelled via its handle; supervisor stopped"
    );
}

/// The async supervision loop. Split out from [`spawn_supervised`] so the
/// backoff schedule is injectable for hermetic tests (a zero-delay `Backoff`
/// keeps the retry-path test fast and non-flaky).
async fn supervise<F, Fut>(name: &'static str, factory: F, cfg: Backoff, stop: Option<Stop>)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    supervise_runs(name, move || runtime::handle().spawn(factory()), cfg, stop).await;
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
        // Always uncancellable: a blocking run cannot be aborted once a pool
        // thread has picked it up, so there is no cancellable blocking entry
        // point to pass a `Stop` in from. See [`spawn_supervised_handle`].
        None,
    )
    .await;
}

/// The one supervision loop all three entry points run.
///
/// `spawn_run` starts a single run and hands back its `JoinHandle`; everything
/// above it — restart policy, backoff schedule, log lines, [`crate::health`]
/// bookkeeping, cancellation — is shared, which is what keeps the async,
/// blocking and cancellable supervisors from drifting apart.
///
/// `stop` is `Some` only for [`spawn_supervised_handle`]. It is consulted at
/// each of the three points where this loop can be *between* useful work — before
/// starting a run, while awaiting one, and while sleeping out a backoff — so a
/// cancel is never left waiting on a 30 s backoff or on a run that has no reason
/// to end.
async fn supervise_runs<S>(name: &'static str, spawn_run: S, cfg: Backoff, mut stop: Option<Stop>)
where
    S: Fn() -> JoinHandle<()> + Send + 'static,
{
    // The health entry is this supervisor's, and lives exactly as long as this
    // loop: every `return` below drops it. See `health`'s module docs on why
    // nothing is retained for a supervisor that has stopped.
    let id = health::register(name);
    let mut delay = cfg.initial;
    loop {
        // A cancel that landed while the previous run was ending must not be
        // overtaken by the next one. This is also the *only* thing that can
        // stop a zero-backoff restart loop, which never reaches the sleep
        // below.
        if cancel_pending(stop.as_ref()) {
            finish_cancelled(name, id);
            return;
        }

        let started = Instant::now();
        health::run_started(id);
        // Spawn onto the shared runtime so tokio catches a panic and surfaces
        // it as `JoinError::is_panic()` on the handle we await here.
        let mut join = spawn_run();

        let finished = tokio::select! {
            // Cancellation first, so a cancel and a completion racing resolve
            // as a cancellation — the caller asked for teardown either way, and
            // a deterministic answer is what makes the tests non-flaky.
            biased;
            () = cancel_requested(&mut stop) => None,
            res = &mut join => Some(res),
        };

        let Some(outcome) = finished else {
            // Cancelled mid-run. Abort, then *wait for the abort to land*: a
            // supervisor that returned here while its run was still unwinding
            // would resolve `SupervisorHandle::stopped` on a task that is still
            // writing the state the canceller is about to reset.
            join.abort();
            let _aborted = join.await;
            finish_cancelled(name, id);
            return;
        };

        match outcome {
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
                    let slept = tokio::select! {
                        biased;
                        () = cancel_requested(&mut stop) => false,
                        () = tokio::time::sleep(delay) => true,
                    };
                    if !slept {
                        // Cancelled while backing off. There is no run in
                        // flight to abort, so this is the cheap path — and the
                        // one that matters most in practice, since a flapping
                        // task spends nearly all of its time here, up to 30 s
                        // per restart at the backoff cap.
                        finish_cancelled(name, id);
                        return;
                    }
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

    /// A backoff whose first delay is far longer than any test will wait, so a
    /// cancelled supervisor that stops *promptly* can only have done so by
    /// cutting the sleep short rather than sitting it out.
    const SLOW_BACKOFF: Backoff = Backoff {
        initial: Duration::from_hours(1),
        max: Duration::from_hours(1),
        reset_after: Duration::MAX,
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
                None,
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
                None,
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

    // ── Cancellation (#633) ───────────────────────────────────────────────
    //
    // These drive `spawn_supervised_handle_with` rather than the public
    // `spawn_supervised_handle` for the same reason `supervise` is split out:
    // the shipped `Backoff::default()` would put a wall-clock second between
    // restarts, and two of these tests are about what the loop does *while*
    // backing off.

    /// Poll until `f` holds, or fail the test. A poll rather than a
    /// notification because what is being waited for is a counter the
    /// supervised runs bump, and the run that bumps it is often about to panic.
    async fn until(label: &str, f: impl Fn() -> bool) {
        let polled = tokio::time::timeout(Duration::from_secs(5), async {
            while !f() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        assert!(polled.is_ok(), "timed out waiting for {label}");
    }

    /// The supervisor must stop within a generous bound. Every use of this is
    /// really an assertion that some cancel path fires: without it the waits
    /// below are unbounded.
    async fn expect_stopped(handle: &SupervisorHandle) {
        let stopped = tokio::time::timeout(Duration::from_secs(5), handle.stopped()).await;
        assert!(stopped.is_ok(), "the supervisor did not stop after cancel");
    }

    /// Whether `health` still carries a row for `name`.
    fn health_row_exists(name: &str) -> bool {
        health::snapshot().iter().any(|task| task.name == name)
    }

    /// Cancelling a supervisor whose run never ends on its own must abort that
    /// run, decline to start another, and give the health row back.
    ///
    /// The health assertion is not incidental. Entries there are live, not
    /// historical, so a cancel path that forgot to release one would leak a row
    /// per teardown — and the case this primitive exists for (#633's Wi-Fi
    /// backend switch) tears down once per switch, for the life of the session.
    #[test]
    fn cancel_aborts_the_run_in_flight_and_releases_the_health_row() {
        const NAME: &str = "test-cancel-running";

        install_error_counter();

        let runs = Arc::new(AtomicUsize::new(0));
        let runs_factory = Arc::clone(&runs);

        runtime::handle().block_on(async move {
            let handle = spawn_supervised_handle_with(
                NAME,
                move || {
                    let runs = Arc::clone(&runs_factory);
                    async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        // Never completes on its own: only cancellation can end
                        // this run, which is the point.
                        std::future::pending::<()>().await;
                    }
                },
                ZERO_BACKOFF,
            );

            until("the first run to start", || {
                runs.load(Ordering::SeqCst) == 1
            })
            .await;
            assert!(!handle.is_cancelled());
            assert!(health_row_exists(NAME), "a live supervisor publishes a row");

            handle.cancel();
            handle.cancel(); // idempotent
            assert!(handle.is_cancelled());
            expect_stopped(&handle).await;

            assert_eq!(
                runs.load(Ordering::SeqCst),
                1,
                "a cancelled supervisor does not resurrect the task"
            );
            assert!(
                !health_row_exists(NAME),
                "the health row is released on the cancel path, not leaked"
            );
        });
    }

    /// A flapping task spends nearly all its time asleep between restarts — up
    /// to 30 s at the shipped backoff cap — so a cancel that only took effect
    /// at the next run would be useless in exactly the case teardown is wanted.
    ///
    /// `SLOW_BACKOFF` puts the next run an hour away, so returning at all is
    /// the assertion.
    #[test]
    fn cancel_interrupts_a_restart_backoff_instead_of_waiting_it_out() {
        const NAME: &str = "test-cancel-backoff";

        install_error_counter();

        let runs = Arc::new(AtomicUsize::new(0));
        let runs_factory = Arc::clone(&runs);

        runtime::handle().block_on(async move {
            let handle = spawn_supervised_handle_with(
                NAME,
                move || {
                    let runs = Arc::clone(&runs_factory);
                    async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        panic!("cancel-backoff test panics every run");
                    }
                },
                SLOW_BACKOFF,
            );

            until("the first run to panic", || {
                runs.load(Ordering::SeqCst) == 1
            })
            .await;
            handle.cancel();
            expect_stopped(&handle).await;

            assert_eq!(
                runs.load(Ordering::SeqCst),
                1,
                "the backoff was abandoned, not slept through into another run"
            );
            assert!(!health_row_exists(NAME));
        });
    }

    /// With a zero-length backoff there is no sleep to interrupt: the loop goes
    /// straight from a panic into the next run. The check at the top of each
    /// iteration is then the only thing that can ever stop it, so this is that
    /// check's test — without it, `stopped()` below never resolves.
    #[test]
    fn cancel_stops_a_zero_backoff_restart_loop() {
        const NAME: &str = "test-cancel-hot-loop";

        install_error_counter();

        let runs = Arc::new(AtomicUsize::new(0));
        let runs_factory = Arc::clone(&runs);

        runtime::handle().block_on(async move {
            let handle = spawn_supervised_handle_with(
                NAME,
                move || {
                    let runs = Arc::clone(&runs_factory);
                    async move {
                        runs.fetch_add(1, Ordering::SeqCst);
                        panic!("cancel-hot-loop test panics every run");
                    }
                },
                ZERO_BACKOFF,
            );

            until("the loop to be restarting", || {
                runs.load(Ordering::SeqCst) >= 2
            })
            .await;
            handle.cancel();
            expect_stopped(&handle).await;

            // `stopped()` resolving means the loop has returned, so the count
            // is final by construction — no sleep-and-recheck needed.
            assert!(
                runs.load(Ordering::SeqCst) >= 2,
                "sanity: the loop really was restarting"
            );
            assert!(!health_row_exists(NAME));
        });
    }

    /// Dropping every handle must leave supervision running.
    ///
    /// This is the property that lets [`spawn_supervised`] and
    /// [`spawn_supervised_blocking`] one day return this same type without
    /// touching their ~40 statement-position call sites — all of which drop the
    /// value on the line that produced it. `ExportHandle`'s
    /// "dropping the last clone stops the task" is the *opposite* choice, and
    /// adopting it here would silently cancel every supervised task in the
    /// shell, compiling and linting clean the whole way.
    #[test]
    fn dropping_every_handle_leaves_the_supervisor_running() {
        const NAME: &str = "test-drop-not-cancel";

        install_error_counter();

        let runs = Arc::new(AtomicUsize::new(0));
        let runs_factory = Arc::clone(&runs);

        runtime::handle().block_on(async move {
            let handle = spawn_supervised_handle_with(
                NAME,
                move || {
                    let runs = Arc::clone(&runs_factory);
                    async move {
                        let n = runs.fetch_add(1, Ordering::SeqCst);
                        assert!(n >= 3, "panic on the first three runs");
                    }
                },
                ZERO_BACKOFF,
            );
            let clone = handle.clone();
            drop(handle);
            drop(clone);

            // If dropping cancelled, the restarts stop at run 1 and this times
            // out.
            until("the supervisor to restart through to its clean run", || {
                runs.load(Ordering::SeqCst) == 4
            })
            .await;
        });
    }

    /// `stopped()` is not cancellation-specific: it resolves whenever the
    /// supervisor is gone, including the clean-completion path that has always
    /// ended supervision. Calling it again afterwards must return at once
    /// rather than hang on a channel nobody will ever write to.
    #[test]
    fn stopped_also_resolves_on_a_clean_completion() {
        const NAME: &str = "test-handle-clean";

        install_error_counter();

        runtime::handle().block_on(async move {
            let handle = spawn_supervised_handle_with(NAME, || async {}, Backoff::default());

            expect_stopped(&handle).await;
            expect_stopped(&handle).await;

            assert!(
                !handle.is_cancelled(),
                "a task that finished its job was not cancelled"
            );
            assert!(!health_row_exists(NAME));
        });
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
