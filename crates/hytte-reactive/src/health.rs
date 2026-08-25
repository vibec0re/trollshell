//! Health of the tasks [`crate::supervisor`] supervises: which ones are up,
//! which ones are flapping, and how hard.
//!
//! # Why this exists
//!
//! [`crate::spawn_supervised`] recovers a panicked task, but until now the only
//! record that it *had* to was one `error!` line per restart. A task caught in a
//! restart loop is then visible in the journal and nowhere else — nothing a
//! widget can bind to, nothing a diagnostics view can read, nothing a test can
//! assert beyond "an error was logged". #238 asked for a health handle when it
//! specified the supervisor; #690 deferred it; #691 is the record of the
//! dependency.
//!
//! # What is actually new here
//!
//! Almost nothing, and that is deliberate. The supervisor already had to track
//! *how long the last run lived* and *how far the backoff has climbed* in order
//! to decide when to restart; this module keeps a handful of counters alongside
//! them and publishes the result. The first consumer is the supervisor's own
//! log line, which can now say `panics=4 consecutive_panics=4` instead of
//! leaving a reader to count restarts by hand — so these numbers earn their keep
//! before any UI exists. [`signal`] is the same record offered to a widget when
//! there is one to offer it to.
//!
//! # Shape
//!
//! One process-global [`Mutable`] holding a `Vec<TaskHealth>`, in the order
//! supervision started. Not the thread-local [`crate::registry`]: supervised
//! tasks are spawned from tokio worker threads, before and independently of any
//! `App`, so the registry's GTK-main-thread confinement is the wrong home. This
//! matches [`crate::runtime::handle`], the other process-global the supervisor
//! leans on.
//!
//! **Entries are live, not historical.** A task is added when supervision
//! starts and *removed* when the supervisor stops — a clean completion, or a
//! cancellation. Retaining terminated entries would be a slow leak rather than a
//! feature: `mpris-player` and `tray-item` supervise one task per discovered
//! player/item, so a long session churns through an unbounded number of them.
//! What this answers is "what is being supervised right now, and is it healthy",
//! which is the question a diagnostics view asks.
//!
//! ```ignore
//! use hytte::prelude::*;
//! use hytte::reactive::health;
//!
//! bind(health::signal(), &label, |label, tasks| {
//!     let sick = tasks.iter().filter(|t| t.consecutive_panics > 0).count();
//!     label.set_text(&format!("{sick} flapping"));
//! });
//! ```

use futures_signals::signal::{Mutable, Signal};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Identity of one supervised task, valid from the moment supervision starts
/// until the supervisor stops.
///
/// Distinct per *supervisor*, not per name: several services supervise more
/// than one task under the same label (`sensors` runs four, `upower` three,
/// `mpris-player` one per player), so a view that keyed on
/// [`TaskHealth::name`] alone would show them overwriting one another. Group by
/// the name for display; key on this for identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TaskId(u64);

/// Whether a supervised task is running, or waiting out a restart backoff.
///
/// There is no terminal variant: a supervisor that has stopped has no entry at
/// all (see the module docs on live-not-historical).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskState {
    /// A run is in flight.
    Running,
    /// The last run panicked; the supervisor is sleeping out
    /// [`TaskHealth::backoff`] before it starts the next one.
    Restarting,
}

/// What the supervisor knows about one task it is supervising.
///
/// A point-in-time copy — reading it takes no lock beyond the clone, and it
/// does not update itself. Re-read [`snapshot`], or subscribe to [`signal`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskHealth {
    /// Stable identity for as long as this supervisor runs. See [`TaskId`].
    pub id: TaskId,
    /// The label the task was supervised under — the same string that appears
    /// as `service` in the supervisor's log lines. **Not unique.**
    pub name: &'static str,
    /// Running, or backing off before a restart.
    pub state: TaskState,
    /// Runs started so far, including the one in flight. `1` for a task that
    /// has never panicked.
    pub runs: u32,
    /// Runs that ended in a panic, over this supervisor's whole life.
    pub panics: u32,
    /// Panics since the last run that stayed up long enough to count as
    /// healthy (the same threshold that resets the backoff — 30 s by default).
    ///
    /// This is the flapping number, and the one worth showing: `panics` alone
    /// cannot tell "crashed once an hour ago, fine since" from "crashing every
    /// 30 seconds right now".
    pub consecutive_panics: u32,
    /// When the last panic happened, or `None` if there has not been one.
    ///
    /// An [`Instant`] rather than a wall-clock time because the useful question
    /// is "how long ago" — call [`Instant::elapsed`]. Monotonic, so it does not
    /// jump when the clock is stepped.
    pub last_panic: Option<Instant>,
    /// While [`TaskState::Restarting`], how long the supervisor is sleeping
    /// before the next run; [`Duration::ZERO`] while running.
    ///
    /// This is the supervisor's own capped-exponential delay, so it doubles as
    /// a severity reading: at the 30 s cap the task has been failing for a
    /// while.
    pub backoff: Duration,
}

/// Every task currently under supervision, in the order supervision started.
///
/// Callable from any thread. Cheap (a `Vec` clone of a few dozen `Copy`
/// records) but not free — prefer [`signal`] for anything that wants to react
/// to changes rather than poll.
#[must_use]
pub fn snapshot() -> Vec<TaskHealth> {
    TASKS.get_cloned()
}

/// Signal of [`snapshot`], for binding a diagnostics view to.
///
/// Emits on every transition the supervisor makes: a run starting, a run
/// panicking, a supervisor stopping. Steady state is silent — a healthy shell
/// emits one burst at start-up (one event per service task) and then nothing.
pub fn signal() -> impl Signal<Item = Vec<TaskHealth>> {
    TASKS.signal_cloned()
}

// ── Supervisor-side bookkeeping ──────────────────────────────────────────────

/// The counters [`panicked`] just updated, handed back so the supervisor can
/// put them in its log line without re-reading the table.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PanicCounts {
    /// [`TaskHealth::panics`] after this panic.
    pub(crate) total: u32,
    /// [`TaskHealth::consecutive_panics`] after this panic.
    pub(crate) consecutive: u32,
}

/// Start tracking a supervisor. Returns the id its later updates quote.
///
/// Called by `supervise_runs` — the single loop `spawn_supervised`,
/// `spawn_supervised_blocking` and `spawn_supervised_handle` all funnel
/// through — so every supervision entry point is covered by construction,
/// including ones that do not exist yet.
pub(crate) fn register(name: &'static str) -> TaskId {
    let id = TaskId(NEXT_ID.fetch_add(1, Ordering::Relaxed));
    TASKS.lock_mut().push(TaskHealth {
        id,
        name,
        state: TaskState::Running,
        runs: 0,
        panics: 0,
        consecutive_panics: 0,
        last_panic: None,
        backoff: Duration::ZERO,
    });
    id
}

/// A run is about to start: count it and clear the backoff reading.
pub(crate) fn run_started(id: TaskId) {
    with_task(id, |task| {
        task.runs = task.runs.saturating_add(1);
        task.state = TaskState::Running;
        task.backoff = Duration::ZERO;
    });
}

/// A run panicked and the supervisor is about to sleep `backoff` before the
/// next one.
///
/// `after_healthy_run` is the supervisor's own "this run stayed up long enough"
/// verdict — the same one that resets the backoff — so the streak and the delay
/// can never disagree about what counts as healthy.
pub(crate) fn panicked(id: TaskId, backoff: Duration, after_healthy_run: bool) -> PanicCounts {
    let mut counts = PanicCounts::default();
    with_task(id, |task| {
        task.panics = task.panics.saturating_add(1);
        task.consecutive_panics = if after_healthy_run {
            1
        } else {
            task.consecutive_panics.saturating_add(1)
        };
        task.last_panic = Some(Instant::now());
        task.state = TaskState::Restarting;
        task.backoff = backoff;
        counts = PanicCounts {
            total: task.panics,
            consecutive: task.consecutive_panics,
        };
    });
    counts
}

/// The supervisor stopped — drop its entry. See the module docs on why nothing
/// is retained.
pub(crate) fn stopped(id: TaskId) {
    TASKS.lock_mut().retain(|task| task.id != id);
}

/// Every live supervisor's record, in the order supervision started.
///
/// A `Vec` rather than a map: the consumer wants the whole list in a stable
/// order, updates are rare (a run starting, a panic, a supervisor stopping) and
/// there are a few dozen entries at most, so the linear scan in [`with_task`]
/// costs less than the ordering a map would take away.
static TASKS: LazyLock<Mutable<Vec<TaskHealth>>> = LazyLock::new(|| Mutable::new(Vec::new()));

/// Source of [`TaskId`]s. Starts at 1 so `TaskId(0)` is never handed out.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Mutate one entry in place, if it is still there.
///
/// It always is when the supervisor calls this — only [`stopped`] removes an
/// entry, and the loop that calls it returns immediately afterwards — so the
/// miss branch is defence against a future entry point, not a live case.
fn with_task(id: TaskId, f: impl FnOnce(&mut TaskHealth)) {
    let mut tasks = TASKS.lock_mut();
    if let Some(task) = tasks.iter_mut().find(|task| task.id == id) {
        f(task);
    }
}

#[cfg(test)]
mod tests {
    use super::{TaskState, panicked, register, run_started, snapshot, stopped};
    use std::time::Duration;

    /// Entries for one test's tasks. The table is process-global and cargo runs
    /// tests in parallel threads of one process, so every assertion here is
    /// scoped by name rather than by clearing the table — a reset would rip out
    /// a concurrently-running supervisor's live entry.
    fn tagged(name: &'static str) -> Vec<super::TaskHealth> {
        snapshot().into_iter().filter(|t| t.name == name).collect()
    }

    /// Two supervisors sharing a label are two entries, not one: `sensors` and
    /// `upower` really do supervise several tasks each, and collapsing them
    /// onto the name would show one task's restarts as another's.
    #[test]
    fn tasks_sharing_a_name_are_tracked_separately() {
        let a = register("test-health-shared-name");
        let b = register("test-health-shared-name");
        assert_ne!(a, b, "each registration gets its own id");

        run_started(a);
        run_started(a);
        run_started(b);

        let mine = tagged("test-health-shared-name");
        assert_eq!(mine.len(), 2);
        let runs = |id| mine.iter().find(|t| t.id == id).map(|t| t.runs);
        assert_eq!(runs(a), Some(2));
        assert_eq!(runs(b), Some(1));

        stopped(a);
        stopped(b);
        assert!(
            tagged("test-health-shared-name").is_empty(),
            "a stopped supervisor leaves no entry behind"
        );
    }

    /// The streak counts panics since the last *healthy* run, while the total
    /// counts them over the supervisor's whole life. A view that only had the
    /// total could not tell "flapping now" from "flapped once, long ago".
    #[test]
    fn a_healthy_run_resets_the_streak_but_not_the_total() {
        let id = register("test-health-streak");
        let one = |after_healthy_run| panicked(id, Duration::from_secs(2), after_healthy_run);

        assert_eq!(
            one(false),
            super::PanicCounts {
                total: 1,
                consecutive: 1
            }
        );
        assert_eq!(
            one(false),
            super::PanicCounts {
                total: 2,
                consecutive: 2
            }
        );
        // A run that stayed up long enough starts the streak over.
        assert_eq!(
            one(true),
            super::PanicCounts {
                total: 3,
                consecutive: 1
            }
        );

        let mine = tagged("test-health-streak");
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].state, TaskState::Restarting);
        assert_eq!(mine[0].backoff, Duration::from_secs(2));
        assert!(mine[0].last_panic.is_some());

        // …and starting the next run clears the backoff reading, so `backoff`
        // never lies about a task that is actually running.
        run_started(id);
        let mine = tagged("test-health-streak");
        assert_eq!(mine[0].state, TaskState::Running);
        assert_eq!(mine[0].backoff, Duration::ZERO);

        stopped(id);
    }
}
