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
//! **Entries are live, not historical — with one bounded exception.** A task is
//! added when supervision starts and *removed* when the supervisor stops: by
//! cancellation, or by a clean return from a task spawned through
//! [`crate::spawn_supervised_bounded`], the entry point that *declares* a
//! return to be the expected end of that task. Retaining every terminated entry
//! would be a slow leak rather than a feature: `mpris-player` and `tray-item`
//! supervise one task per discovered player/item, so a long session churns
//! through an unbounded number of them. What most of this answers is "what is
//! being supervised right now, and is it healthy", which is the question a
//! diagnostics view asks.
//!
//! The exception is [`TaskState::Returned`] (#1174): a task spawned through the
//! *plain* [`crate::spawn_supervised`] (or its blocking/cancellable siblings)
//! is written as a loop meant to run for the life of the process, so a plain
//! return there is a bug that fell out of the loop early. Silently dropping the
//! row is exactly the "looks like it never existed" failure #1174 filed, so the
//! row is kept and marked `Returned` instead.
//!
//! That exception is **bounded by name**: [`returned`] replaces any earlier
//! `Returned` row carrying the same [`TaskHealth::name`], so the table holds at
//! most one per distinct supervised name — a few dozen, the same order as the
//! live rows. A diagnostics view that never lies does not need an unbounded
//! history to manage it; the Nth return of one name says nothing the first did
//! not, and the whole table is cloned into every subscriber on every transition.
//!
//! A `Returned` row keeps its `panics` total (and `last_panic`) as history, but
//! its [`TaskHealth::consecutive_panics`] streak is **cleared**: that field
//! means "panicking *now*", and a task that is not running is not. The shell's
//! Services bar chip and the Flapping card it opens both filter on exactly that
//! field, so a task that flapped and then returned would otherwise pin a red
//! badge on the bar for the rest of the session.
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
/// until the supervisor stops — except a task whose last run *returned*
/// unexpectedly (rather than being cancelled, or ending as
/// [`crate::spawn_supervised_bounded`] declares it may), whose id and row are
/// kept as a visible record until the next return under the same name replaces
/// it; see [`TaskState::Returned`].
///
/// Distinct per *supervisor*, not per name: several services supervise more
/// than one task under the same label (`sensors` runs four, `upower` three,
/// `mpris-player` one per player), so a view that keyed on
/// [`TaskHealth::name`] alone would show them overwriting one another. Group by
/// the name for display; key on this for identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TaskId(u64);

/// Whether a supervised task is running, waiting out a restart backoff, or has
/// returned and is no longer supervised at all.
///
/// [`Returned`](TaskState::Returned) is the one terminal variant: every other
/// way a supervisor stops (cancellation, or the declared end of a
/// [`crate::spawn_supervised_bounded`] task) drops the row entirely rather than
/// leaving it in a terminal state (see the module docs on live-not-historical).
///
/// `#[non_exhaustive]`: this is public, re-exported at the crate root, and
/// grew a variant in #1174 — which was a breaking change for any downstream
/// `match`, and cost nothing only because the tree's single match site
/// (`trollshell`'s `panels::stats::flapping_subtitle`) is in this repo. One
/// attribute now makes the next variant free.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TaskState {
    /// A run is in flight.
    Running,
    /// The last run panicked; the supervisor is sleeping out
    /// [`TaskHealth::backoff`] before it starts the next one.
    Restarting,
    /// The last run *returned* rather than panicking or being cancelled.
    /// Supervision has ended — there is no run in flight and none will be
    /// started — but unlike a cancelled supervisor, the row is kept rather
    /// than dropped (see the module docs on the live-not-historical
    /// exception, and [`crate::supervisor`]'s module docs on why this is not
    /// treated as an error worth restarting from).
    ///
    /// A row in this state always reads
    /// [`consecutive_panics`](TaskHealth::consecutive_panics) `== 0` — the
    /// streak means "panicking now", and nothing is running. Its lifetime
    /// [`panics`](TaskHealth::panics) total survives as history.
    Returned,
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
    /// Running, backing off before a restart, or (terminally) returned.
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
    ///
    /// It says *panicking now*, so it is also cleared when supervision ends by
    /// a clean return ([`TaskState::Returned`]) — a task that is not running
    /// cannot be flapping, whatever its `panics` total remembers.
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

/// Every task currently under supervision, in the order supervision started —
/// plus, per name, the most recent task that ended by *returning*
/// ([`TaskState::Returned`]; see the module docs on that bounded exception).
///
/// Callable from any thread. Cheap (a `Vec` clone of a few dozen `Copy`
/// records — the `Returned` rows are bounded to one per supervised name, so
/// they cannot turn this into a growing history) but not free: prefer
/// [`signal`] for anything that wants to react to changes rather than poll.
#[must_use]
pub fn snapshot() -> Vec<TaskHealth> {
    TASKS.get_cloned()
}

/// Signal of [`snapshot`], for binding a diagnostics view to.
///
/// Emits on every transition the supervisor makes: a run starting, a run
/// panicking, a supervisor stopping or returning. Steady state is silent — a
/// healthy shell emits one burst at start-up (one event per service task) and
/// then nothing.
///
/// "Silent" is about *panics*, not about the desktop being idle: the
/// per-instance supervisors register and release a row per discovered MPRIS
/// player and tray item, so playing music or restarting a tray app emits here
/// too. That has always been true (those rows were added and dropped before
/// #1174 as well); it is why the shell's consumers `dedupe_cloned` the
/// projection they actually care about rather than re-rendering on this.
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
/// `spawn_supervised_bounded`,
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

/// The supervisor was cancelled — drop its entry. See the module docs on why
/// nothing is retained for this path (contrast [`returned`], which is).
pub(crate) fn stopped(id: TaskId) {
    TASKS.lock_mut().retain(|task| task.id != id);
}

/// The supervisor's task *returned* rather than being cancelled or panicking:
/// mark it [`TaskState::Returned`] and leave the row in place, rather than
/// dropping it the way [`stopped`] does. See the module docs on why this one
/// path is the deliberate exception to "entries are live, not historical".
///
/// # The exception is bounded by name
///
/// Any *earlier* `Returned` row carrying the same [`TaskHealth::name`] is
/// dropped on the way through, so the table holds at most one `Returned` row
/// per distinct supervised name — a few dozen, the same order as the live rows,
/// which is what keeps [`snapshot`]'s "cheap" true and [`with_task`]'s linear
/// scan honest. Unbounded history was the alternative and it is not worth
/// paying for on every supervisor transition in the shell: the Nth return of
/// one name says nothing the first did not, and the whole `Vec` is cloned for
/// every subscriber each time anything moves.
///
/// Only `Returned` rows are eligible, never a *live* one. [`TaskHealth::name`]
/// is explicitly non-unique — `sensors` supervises four tasks under one label,
/// `upower` three — so dropping by name alone would delete a running sibling's
/// row the moment one of them ended.
///
/// The [`TaskHealth::consecutive_panics`] streak is **cleared** on the way
/// through, while the lifetime `panics` total is kept. The streak's whole
/// meaning is "panicking *now*" (it is the field [`TaskHealth::panics`] exists
/// to be contrasted with), and a task that is not running is not panicking now;
/// leaving it set was measured to pin the shell's Services chip on for the rest
/// of the session, because that chip and the Flapping card it opens both filter
/// on exactly this field and nothing ever clears a `Returned` row.
pub(crate) fn returned(id: TaskId) {
    let mut tasks = TASKS.lock_mut();
    let Some(task) = tasks.iter_mut().find(|task| task.id == id) else {
        return;
    };
    task.state = TaskState::Returned;
    task.backoff = Duration::ZERO;
    task.consecutive_panics = 0;
    let name = task.name;
    tasks.retain(|task| task.id == id || task.state != TaskState::Returned || task.name != name);
}

/// Every live supervisor's record, in the order supervision started, plus at
/// most one [`TaskState::Returned`] row per supervised name (see [`returned`]).
///
/// A `Vec` rather than a map: the consumer wants the whole list in a stable
/// order, updates are rare (a run starting, a panic, a supervisor stopping or
/// returning) and there are a few dozen entries at most, so the linear scan in
/// [`with_task`] costs less than the ordering a map would take away.
///
/// "A few dozen at most" is what the per-name bound on `Returned` rows exists
/// to keep true — an unbounded backlog would put dead rows in front of every
/// live one that scan has to reach, and would be cloned into every subscriber
/// on every transition.
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
    use super::{TaskState, panicked, register, returned, run_started, snapshot, stopped};
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

    /// A task that flapped and then *returned* is not flapping any more, and
    /// its row has to say so: the streak is what "flapping now" is read from
    /// (the shell's Services chip and its Flapping card both filter on exactly
    /// that field), and nothing ever clears a `Returned` row, so a surviving
    /// streak is a red badge on the bar for the rest of the session.
    ///
    /// The lifetime total is the half that *is* history and stays.
    #[test]
    fn a_returned_task_keeps_its_panic_history_but_not_its_streak() {
        let id = register("test-health-return-clears-streak");
        panicked(id, Duration::from_secs(1), false);
        panicked(id, Duration::from_secs(2), false);

        let before = tagged("test-health-return-clears-streak");
        assert_eq!(
            before[0].consecutive_panics, 2,
            "precondition: the streak is live while the task is restarting"
        );

        returned(id);

        let after = tagged("test-health-return-clears-streak");
        assert_eq!(after.len(), 1, "the row is kept, not dropped");
        assert_eq!(after[0].state, TaskState::Returned);
        assert_eq!(
            after[0].consecutive_panics, 0,
            "a task that is not running cannot be flapping"
        );
        assert_eq!(
            after[0].panics, 2,
            "the lifetime total is history and survives the return"
        );
        assert_eq!(after[0].backoff, Duration::ZERO);
        assert!(
            after[0].last_panic.is_some(),
            "when it last panicked is history too"
        );

        stopped(id);
    }

    /// The `Returned` exception to entries-are-live is bounded by name: a
    /// thousand returns under one label leave one row, not a thousand.
    ///
    /// Measured on the unbounded shape before this: 1 000 rows at 72 B each,
    /// and — the part that actually costs — a 1.36 µs `Vec` clone per
    /// [`snapshot`] against ~50 ns for the live table, paid on *every*
    /// supervisor transition anywhere in the shell, plus an O(N) dedupe compare
    /// and an O(N) filter on the GTK main thread. The Nth return of one name
    /// says nothing the first did not.
    ///
    /// A thousand rather than a token handful because the number has to be big
    /// enough that an off-by-one retention rule (keep the last N) would show as
    /// a different answer than "one".
    #[test]
    fn returning_under_one_name_a_thousand_times_leaves_one_row() {
        const NAME: &str = "test-health-returned-bound";

        for _ in 0..1_000 {
            returned(register(NAME));
        }

        let mine = tagged(NAME);
        assert_eq!(mine.len(), 1, "one Returned row per name, not a backlog");
        assert_eq!(mine[0].state, TaskState::Returned);

        stopped(mine[0].id);
    }

    /// …and the one kept is the **newest**, which is the only one anybody would
    /// open a diagnostics view to read.
    #[test]
    fn the_newest_returned_row_is_the_one_kept() {
        const NAME: &str = "test-health-returned-newest";

        let older = register(NAME);
        run_started(older);
        returned(older);

        let newer = register(NAME);
        run_started(newer);
        run_started(newer);
        returned(newer);

        let mine = tagged(NAME);
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].id, newer, "the older row is the one dropped");
        assert_eq!(mine[0].runs, 2, "and it is the newer row's numbers on it");

        stopped(newer);
    }

    /// Bounding by name must never take out a **live** sibling. Names are
    /// explicitly non-unique — `sensors` supervises four tasks under one label,
    /// `upower` three — so a rule that dropped by name alone would blank a
    /// running task's row the moment one of its siblings ended, which is the
    /// opposite of what this table is for.
    #[test]
    fn bounding_returned_rows_leaves_a_live_sibling_alone() {
        const NAME: &str = "test-health-returned-sibling";

        fn state_of(rows: &[super::TaskHealth], id: super::TaskId) -> Option<TaskState> {
            rows.iter().find(|t| t.id == id).map(|t| t.state)
        }

        let live = register(NAME);
        run_started(live);
        let ended = register(NAME);
        returned(ended);

        let mine = tagged(NAME);
        assert_eq!(mine.len(), 2, "the running sibling keeps its row");
        assert_eq!(state_of(&mine, live), Some(TaskState::Running));
        assert_eq!(state_of(&mine, ended), Some(TaskState::Returned));

        // …and a *second* return under the name still only replaces the
        // returned row, not the live one.
        let ended_again = register(NAME);
        returned(ended_again);

        let mine = tagged(NAME);
        assert_eq!(mine.len(), 2);
        assert_eq!(state_of(&mine, live), Some(TaskState::Running));
        assert_eq!(
            state_of(&mine, ended_again),
            Some(TaskState::Returned),
            "the newest return is the surviving Returned row"
        );

        stopped(live);
        stopped(ended_again);
    }
}
