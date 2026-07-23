//! Task list service backed by Evolution Data Server via libecal FFI.
//!
//! Reads and writes go through [`hytte_ecal`] (our hand-written
//! libecal-2.0 / libedataserver-1.2 / libical-glib bindings). Same API
//! libecal provides to gnome-tasks and Evolution, so this works against
//! ANY EDS backend the user has configured: local files, `CalDAV`
//! (Nextcloud, generic), Google Tasks (via the `goa-google` bridge),
//! Microsoft EWS, etc.
//!
//! ## Threading
//!
//! Calling libecal from arbitrary tokio worker threads doesn't compose
//! with `GObject`'s main-context model and isn't [`Sync`]-safe — so all
//! EDS work happens on a single dedicated thread that owns one
//! [`hytte_ecal::Registry`] and a [`HashMap`] of cached
//! [`hytte_ecal::CalClient`] connections (one per source UID, opened
//! lazily on first use).
//!
//! Both the init and the caches are resilient (#432, shared machinery in
//! [`crate::eds_retry`]): a failed worker init retries with backoff (the
//! boot race against evolution-data-server's own activation; user writes
//! queued meanwhile are replayed), a cached client that errors is evicted —
//! together with its live view — and reconnected (EDS restart / list
//! removed), and repeated all-lists failures rebuild the whole session.
//!
//! Public functions enqueue [`Op`] variants onto the worker's channel
//! and return immediately. Writes are fire-and-forget — errors are
//! logged via `tracing::warn`. Reads are pushed to a `Mutable<Vec<Task>>`
//! signal that subscribers (the sidebar widget) bind to.
//!
//! ## Refresh cadence
//!
//! Refreshes are **event-driven**: the worker opens a live
//! [`hytte_ecal::CalClientView`] over each task list (`watch`) and EDS
//! pushes `objects-added/modified/removed` notifications the moment *any*
//! client — Endeavour, Evolution, a `CalDAV` sync — touches a task. The
//! worker re-reads and updates the signal on each push, so external edits
//! surface in ~instantly rather than on a poll boundary (issue #33).
//!
//! [`POLL_INTERVAL`] (5 min) remains only as a cheap safety net — a backend
//! whose view stalls (transient D-Bus hiccup, a source added at runtime
//! before its watch is wired) still reconciles within five minutes. It's
//! deliberately long so the idle path stays quiet (no per-minute wakeups);
//! the live view, not the poll, is what makes the list feel live. Writes
//! also enqueue an immediate refresh.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::thread;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveTime, TimeZone};
use futures_signals::signal::{Mutable, Signal};
use hytte_ecal::sys::ECalClientSourceType;
use hytte_ecal::{CalClient, CalClientView, MainContext, Registry, Source, Waker};
use hytte_reactive::{Service, registry, spawn_supervised};
use icalendar::{
    Calendar, CalendarComponent, CalendarDateTime, Component, DatePerhapsTime, Todo, TodoStatus,
};

use crate::eds_retry::{INIT_BACKOFF_START, SourceFailureStreak, next_backoff, wait_backoff};

// ── Public data types ────────────────────────────────────────────────────────

/// One incomplete task ready for rendering. Surfaced in the
/// [`tasks()`] signal; the sidebar widget binds to that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    /// VTODO UID assigned by EDS. Stable for the lifetime of the task.
    pub uid: String,
    /// SUMMARY field, trimmed. Empty SUMMARYs become `"(no title)"`.
    pub summary: String,
    /// DESCRIPTION field, trimmed. `None` when absent or whitespace-only.
    pub description: Option<String>,
    /// Local-time due, if DUE was present on the VTODO.
    pub due: Option<DateTime<Local>>,
    /// True when DUE was a DATE (no time-of-day).
    pub due_all_day: bool,
    /// Coarse STATUS bucket — Completed and Cancelled are dropped at
    /// parse time.
    pub status: TaskStatus,
    /// EDS source UID; needed to dispatch writes to the right client.
    pub list_uid: String,
    /// Best-effort display name (from the source's `DisplayName=`).
    pub list_name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskStatus {
    NeedsAction,
    InProcess,
}

/// One configured task list (EDS source). Surfaced via [`task_lists()`]
/// so the widget can populate a list picker in the create popover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskList {
    /// EDS source UID — the value to pass back as `list_uid` on
    /// [`create_task`]/etc.
    pub uid: String,
    /// `DisplayName=` from the `.source` file.
    pub display_name: String,
}

/// Safety-net reconciliation interval. The live [`hytte_ecal::CalClientView`]
/// is the primary refresh path (see the module docs); this long poll only
/// catches the rare case where a view never delivers (e.g. a source added at
/// runtime before its watch is wired). Kept long so the idle path stays quiet.
const POLL_INTERVAL: StdDuration = StdDuration::from_mins(5);

// ── Worker channel ───────────────────────────────────────────────────────────

/// Operations the EDS worker thread processes. All fire-and-forget;
/// writes that need to surface success/failure log via `tracing`.
enum Op {
    /// Re-query every task list and push a fresh `Vec<Task>` to the signal.
    Refresh,
    /// Create a new VTODO on `list_uid` with the given summary + optional
    /// due. The caller-provided UID is included in the iCal payload so
    /// the same string can later identify the row.
    Create {
        list_uid: String,
        uid: String,
        summary: String,
        due: Option<DateTime<Local>>,
    },
    /// Flip an existing VTODO between NEEDS-ACTION and COMPLETED (with
    /// PERCENT-COMPLETE=100 + COMPLETED stamp on the way to done).
    SetCompleted {
        list_uid: String,
        uid: String,
        completed: bool,
    },
    /// Replace SUMMARY + DUE on an existing VTODO, preserving every
    /// other property by reading the current iCal, mutating, and
    /// writing back.
    Edit {
        list_uid: String,
        uid: String,
        summary: String,
        due: Option<DateTime<Local>>,
    },
    /// Remove a VTODO. No undo.
    Delete { list_uid: String, uid: String },
}

/// Channel handle to the worker. `OnceLock` so the service can be
/// registered exactly once; subsequent `service()` calls overwrite
/// nothing.
static SENDER: OnceLock<mpsc::Sender<Op>> = OnceLock::new();

/// Wakes the worker out of its blocking [`MainContext`] iteration once an op
/// has been queued, so commands are picked up promptly instead of waiting for
/// the next EDS push or poll tick. Set once the worker's context exists.
static WAKER: OnceLock<Waker> = OnceLock::new();

fn send_op(op: Op) {
    let Some(tx) = SENDER.get() else {
        tracing::warn!("tasks: worker not started; op dropped");
        return;
    };
    if let Err(e) = tx.send(op) {
        tracing::warn!(error = %e, "tasks: worker channel closed");
        return;
    }
    // Break the worker out of `MainContext::iterate(block=true)` so it drains
    // the queue now rather than on the next push/poll. No-op until the worker
    // has published its waker.
    if let Some(w) = WAKER.get() {
        w.wake();
    }
}

// ── Service handle ───────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct TaskHandles {
    pub(crate) tasks: Mutable<Vec<Task>>,
    pub(crate) lists: Mutable<Vec<TaskList>>,
}

impl Default for TaskHandles {
    fn default() -> Self {
        Self {
            tasks: Mutable::new(Vec::new()),
            lists: Mutable::new(Vec::new()),
        }
    }
}

/// Service marker. Pass to `App::with` to register handles + spawn the
/// EDS worker thread + start the 60 s refresh ticker.
pub struct TasksService;

impl Service for TasksService {
    type Handles = TaskHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = TaskHandles::default();
        let tasks_writer = handles.tasks.clone();
        let lists_writer = handles.lists.clone();

        // Channel: tokio polling task + public API → EDS worker.
        let (tx, rx) = mpsc::channel::<Op>();
        let _ = SENDER.set(tx);

        // Dedicated thread. EDS state lives here exclusively. We don't
        // store a JoinHandle — shell processes that quit cleanly let
        // the thread drop along with the OnceLock; ungraceful exits get
        // the same OS cleanup either way.
        thread::Builder::new()
            .name("hytte-eds".into())
            .spawn(move || run_worker(&rx, &tasks_writer, &lists_writer))
            .expect("spawn EDS worker thread");

        // Refresh ticker on the tokio runtime.
        spawn_supervised("tasks", || async {
            loop {
                send_op(Op::Refresh);
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        });

        handles
    }
}

#[must_use]
pub fn service() -> TasksService {
    TasksService
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Signal of incomplete tasks across every EDS task list. Sorted so
/// dated tasks ascend by due (overdue → soonest), then no-due tasks
/// alphabetised. Empty until the first refresh completes.
pub fn tasks() -> impl Signal<Item = Vec<Task>> {
    registry::with(|r| {
        r.get::<TaskHandles>()
            .expect("tasks::service() not registered")
            .tasks
            .signal_cloned()
    })
}

/// Signal of the EDS task lists this account knows about. The widget
/// uses this to populate the create-popover's list picker. Refreshed
/// in lockstep with [`tasks()`].
pub fn task_lists() -> impl Signal<Item = Vec<TaskList>> {
    registry::with(|r| {
        r.get::<TaskHandles>()
            .expect("tasks::service() not registered")
            .lists
            .signal_cloned()
    })
}

/// Trigger an out-of-cycle refresh. Safe to call from page-show /
/// sidebar-open handlers.
pub fn refresh() {
    send_op(Op::Refresh);
}

/// Create a new task on `list_uid` with the given summary + optional
/// due. The returned UID is generated client-side and burned into the
/// VTODO before submission, so the caller can correlate the new row
/// without waiting for the refresh round-trip.
#[must_use = "the returned UID is the only way to address the new task"]
pub fn create_task(list_uid: String, summary: String, due: Option<DateTime<Local>>) -> String {
    let uid = generate_uid();
    send_op(Op::Create {
        list_uid,
        uid: uid.clone(),
        summary,
        due,
    });
    uid
}

/// Toggle a task's completed flag. Reads the current VTODO from EDS,
/// flips STATUS + PERCENT-COMPLETE + COMPLETED, writes it back —
/// preserves every other property (DESCRIPTION, CATEGORIES, etc.).
pub fn set_completed(list_uid: &str, uid: &str, completed: bool) {
    send_op(Op::SetCompleted {
        list_uid: list_uid.to_string(),
        uid: uid.to_string(),
        completed,
    });
}

/// Edit a task's SUMMARY + DUE. Same read-modify-write cycle as
/// [`set_completed`].
pub fn edit_task(list_uid: &str, uid: &str, summary: String, due: Option<DateTime<Local>>) {
    send_op(Op::Edit {
        list_uid: list_uid.to_string(),
        uid: uid.to_string(),
        summary,
        due,
    });
}

/// Remove a task. No undo path; the widget should confirm before
/// calling.
pub fn delete_task(list_uid: &str, uid: &str) {
    send_op(Op::Delete {
        list_uid: list_uid.to_string(),
        uid: uid.to_string(),
    });
}

// ── EDS worker thread ───────────────────────────────────────────────────────

struct Worker {
    registry: Registry,
    clients: HashMap<String, CalClient>,
    list_names: HashMap<String, String>,
    /// Live push subscriptions, one per task list, keyed by source UID. Kept
    /// alive here so EDS keeps delivering `objects-{added,modified,removed}`
    /// for that list; opened lazily in [`Worker::ensure_watch`] right after the
    /// list's [`CalClient`]. Dropping an entry stops its view.
    views: HashMap<String, CalClientView>,
    /// Consecutive all-lists-failed scan tracker (#432). When it trips,
    /// [`Worker::maybe_rebuild`] tears down and reopens the whole session.
    failure_streak: SourceFailureStreak,
    /// Set by [`Worker::scan_all`] when the streak trips; consumed by
    /// [`Worker::maybe_rebuild`].
    rebuild_pending: bool,
}

impl Worker {
    fn new() -> anyhow::Result<Self> {
        let registry = Registry::new()?;
        let mut list_names = HashMap::new();
        for src in registry.task_lists() {
            list_names.insert(src.uid(), src.display_name());
        }
        // Make sure the user always has somewhere to put a new task.
        // Users with only Calendar sources configured (no Task List
        // sources) would otherwise see a "+" with a disabled Add
        // button — provision a local "Trollshell Tasks" source so the
        // widget can write somewhere on a fresh account.
        if list_names.is_empty() {
            match ensure_default_local_list() {
                Ok(()) => {
                    tracing::info!("tasks: provisioned default local list 'trollshell-tasks'");
                    // The newly-written .source file races EDS's
                    // inotify pickup; refresh the registry so we don't
                    // miss it on this first scan.
                    let registry = Registry::new()?;
                    for src in registry.task_lists() {
                        list_names.insert(src.uid(), src.display_name());
                    }
                    return Ok(Self::from_parts(registry, list_names));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "tasks: failed to provision default local list");
                }
            }
        }
        Ok(Self::from_parts(registry, list_names))
    }

    fn from_parts(registry: Registry, list_names: HashMap<String, String>) -> Self {
        Self {
            registry,
            clients: HashMap::new(),
            list_names,
            views: HashMap::new(),
            failure_streak: SourceFailureStreak::default(),
            rebuild_pending: false,
        }
    }

    /// Open the [`CalClient`] for `list_uid` (lazily) and return a borrow.
    /// 5 s connect budget — bumped for CalDAV/EWS at the cost of slower
    /// initial-open feedback on broken networks.
    fn client(&mut self, list_uid: &str) -> anyhow::Result<&CalClient> {
        if !self.clients.contains_key(list_uid) {
            let src = self.lookup_source(list_uid)?;
            let client = CalClient::connect(&src, ECalClientSourceType::Tasks, 5)?;
            self.clients.insert(list_uid.to_string(), client);
        }
        Ok(self
            .clients
            .get(list_uid)
            .expect("just inserted; lookup can't miss"))
    }

    /// Drop `list_uid`'s cached connection state — the client **and** its
    /// live view. The view holds the (possibly dead) client's proxy: keeping
    /// it would both stop push delivery for good and block
    /// [`Worker::ensure_watch`] from ever re-subscribing (#432).
    fn evict(&mut self, list_uid: &str) {
        if self.clients.remove(list_uid).is_some() {
            tracing::debug!(list = %list_uid, "tasks: evicted cached EDS client");
        }
        self.views.remove(list_uid);
    }

    /// Run `op` against `list_uid`'s (lazily opened) client, evicting the
    /// cached connection state on failure so the next use reconnects instead
    /// of being served a dead handle forever (#432). No automatic retry —
    /// the safe default for writes, where a timed-out-but-applied call must
    /// not be replayed.
    fn with_client<T>(
        &mut self,
        list_uid: &str,
        op: impl FnOnce(&CalClient) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let res = op(self.client(list_uid)?);
        if res.is_err() {
            self.evict(list_uid);
        }
        res
    }

    /// Like [`Worker::with_client`], but when the failure hit a *previously
    /// cached* client — the EDS-restart signature: the daemon died under a
    /// handle we were holding — reconnect and retry once immediately. Only
    /// for idempotent operations (reads, opening a view).
    fn with_client_retry<T>(
        &mut self,
        list_uid: &str,
        op: impl Fn(&CalClient) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let cached = self.clients.contains_key(list_uid);
        match self.with_client(list_uid, &op) {
            Err(e) if cached => {
                tracing::info!(
                    list = %list_uid,
                    error = %e,
                    "tasks: cached EDS client failed; reconnecting"
                );
                self.with_client(list_uid, &op)
            }
            r => r,
        }
    }

    /// Open a live [`CalClientView`] over `list_uid`'s client (once) so EDS
    /// pushes change notifications for it. The view's callback enqueues an
    /// `Op::Refresh` — coalesced by the channel + the content-diff in
    /// [`Worker::refresh`] — so any external edit re-reads the list. Idempotent:
    /// a list already watched is a no-op. Best-effort — a watch that fails to
    /// open just leaves that list on the safety-net poll (with the failing
    /// client evicted, so the next pass starts from a fresh connection).
    fn ensure_watch(&mut self, list_uid: &str) {
        if self.views.contains_key(list_uid) {
            return;
        }
        // The callback runs on this worker thread (inside MainContext::iterate),
        // so it just queues a refresh; the loop drains it on the next turn.
        match self.with_client_retry(list_uid, |c| c.watch("#t", || send_op(Op::Refresh))) {
            Ok(view) => {
                tracing::debug!(list = %list_uid, "tasks: live view watching");
                self.views.insert(list_uid.to_string(), view);
            }
            Err(e) => {
                tracing::warn!(list = %list_uid, error = %e, "tasks: get_view failed; poll only");
            }
        }
    }

    fn lookup_source(&self, list_uid: &str) -> anyhow::Result<Source> {
        match self.registry.ref_source(list_uid)? {
            Some(s) => Ok(s),
            None => anyhow::bail!("EDS source '{list_uid}' not found"),
        }
    }

    /// If the last scan tripped the all-lists-failed streak, tear down and
    /// rebuild the whole EDS session (fresh [`Registry`] + empty client and
    /// view caches) — the per-client evict path can't help when the registry
    /// connection itself died. Returns `true` when a rebuild happened (the
    /// caller should rescan immediately on the fresh session).
    fn maybe_rebuild(&mut self) -> bool {
        if !self.rebuild_pending {
            return false;
        }
        self.rebuild_pending = false;
        match Registry::new() {
            Ok(r) => {
                tracing::info!("tasks: rebuilt EDS session after repeated scan failures");
                self.registry = r;
                self.clients.clear();
                self.views.clear();
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "tasks: EDS session rebuild failed; keeping current one");
                false
            }
        }
    }

    /// Re-scan every task list and emit fresh signals if either the
    /// tasks Vec or the lists Vec differs from the current snapshot.
    fn refresh(
        &mut self,
        tasks_writer: &Mutable<Vec<Task>>,
        lists_writer: &Mutable<Vec<TaskList>>,
    ) {
        let (mut tasks, mut lists) = self.scan_all();
        if self.maybe_rebuild() {
            // The session was just rebuilt after repeated total failures —
            // rescan now on the fresh connection rather than serving the
            // failed pass and waiting out another poll interval.
            (tasks, lists) = self.scan_all();
        }
        let tasks_changed = {
            let cur = tasks_writer.lock_ref();
            *cur != tasks
        };
        if tasks_changed {
            tasks_writer.set(tasks);
        }
        let lists_changed = {
            let cur = lists_writer.lock_ref();
            *cur != lists
        };
        if lists_changed {
            lists_writer.set(lists);
        }
    }

    fn scan_all(&mut self) -> (Vec<Task>, Vec<TaskList>) {
        // Re-read the source list each refresh — a user adding/removing
        // an account at runtime is rare, but cheap to handle.
        let sources = self.registry.task_lists();
        // Drop cached clients + views for sources that no longer exist — a
        // removed list's handles would otherwise sit dead in the caches
        // forever (#432).
        let live: HashSet<String> = sources.iter().map(Source::uid).collect();
        self.clients.retain(|uid, _| live.contains(uid));
        self.views.retain(|uid, _| live.contains(uid));
        let mut failed = 0usize;
        let mut tasks: Vec<Task> = Vec::new();
        let mut lists: Vec<TaskList> = Vec::with_capacity(sources.len());
        for src in &sources {
            let list_uid = src.uid();
            let list_name = src.display_name();
            self.list_names.insert(list_uid.clone(), list_name.clone());
            lists.push(TaskList {
                uid: list_uid.clone(),
                display_name: list_name.clone(),
            });
            // Establish the live push subscription before reading (idempotent),
            // so newly-appeared lists start delivering change notifications.
            self.ensure_watch(&list_uid);
            let objects = match self.with_client_retry(&list_uid, |c| c.get_object_strings("#t")) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(list = %list_uid, error = %e, "tasks: query failed; will reconnect on the next refresh");
                    failed += 1;
                    continue;
                }
            };
            for body in objects {
                if let Some(t) = parse_one(&body, &list_uid, &list_name) {
                    tasks.push(t);
                }
            }
        }
        // Every list failing repeatedly means the registry session itself is
        // likely dead — schedule a full rebuild (#432).
        if self.failure_streak.record(sources.len(), failed) {
            self.rebuild_pending = true;
        }
        tasks.sort_by(sort_tasks);
        (tasks, lists)
    }

    fn create(
        &mut self,
        list_uid: &str,
        uid: &str,
        summary: &str,
        due: Option<DateTime<Local>>,
    ) -> anyhow::Result<()> {
        let mut todo = Todo::new();
        todo.uid(uid);
        todo.summary(summary);
        todo.status(TodoStatus::NeedsAction);
        stamp_now(&mut todo);
        if let Some(dt) = due {
            todo.due(date_perhaps_time_from_local(dt));
        }
        let ical = wrap(&todo);
        // Evict-on-error only (no auto-retry): a timed-out-but-applied
        // create must not be replayed into a duplicate. The eviction still
        // means the *next* attempt reconnects fresh (#432).
        self.with_client(list_uid, |client| {
            client.create_from_ical(&ical).map(|_uid| ())
        })
    }

    fn modify_in_place<F: FnOnce(&mut Todo)>(
        &mut self,
        list_uid: &str,
        uid: &str,
        mutate: F,
    ) -> anyhow::Result<()> {
        // Read phase — idempotent, so a dead cached client is retried on a
        // fresh connection: the first edit after an EDS restart succeeds
        // instead of burning the user's click on the reconnect (#432).
        let current = self
            .with_client_retry(list_uid, |c| c.get_object_as_string(uid, None))?
            .ok_or_else(|| anyhow::anyhow!("task '{uid}' not found on list '{list_uid}'"))?;
        let parsed: Calendar = current
            .parse()
            .map_err(|e| anyhow::anyhow!("parse current: {e}"))?;
        // Take ownership of the first VTODO so we can mutate via &mut.
        let mut todo = parsed
            .components
            .into_iter()
            .find_map(|c| match c {
                CalendarComponent::Todo(t) => Some(t),
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("current iCal has no VTODO"))?;
        mutate(&mut todo);
        bump_last_modified(&mut todo);
        let ical = wrap(&todo);
        // Write phase — evict-on-error only: a timed-out-but-applied modify
        // must not be replayed. The read phase above just proved the
        // connection, so this almost always runs against a live client.
        self.with_client(list_uid, |c| c.modify_from_ical(&ical))
    }

    fn delete(&mut self, list_uid: &str, uid: &str) -> anyhow::Result<()> {
        // Evict-on-error only: replaying a delete that actually applied
        // would surface a confusing not-found warning.
        self.with_client(list_uid, |c| c.remove(uid, None))
    }
}

fn run_worker(
    rx: &mpsc::Receiver<Op>,
    tasks_writer: &Mutable<Vec<Task>>,
    lists_writer: &Mutable<Vec<TaskList>>,
) {
    // Create the worker thread's private GMainContext *before* anything opens a
    // view: EDS attaches each `CalClientView`'s signal sources to whatever
    // context is thread-default at `get_view` time, and iterating this context
    // is what dispatches their callbacks (on this thread). Publish its waker so
    // `send_op` can break the blocking iteration to deliver commands promptly.
    let Some(ctx) = MainContext::new() else {
        tracing::error!("tasks: GMainContext alloc failed; service inert");
        for _ in rx {}
        return;
    };
    let _ = WAKER.set(ctx.waker());

    // Init with retry + backoff instead of going permanently inert: at
    // session bring-up trollshell and evolution-data-server activate
    // concurrently, so the first `Registry::new()` (a blocking D-Bus
    // round-trip) plausibly times out. Ops consumed while backing off are
    // kept: refreshes are subsumed by the post-init refresh, writes are
    // buffered and replayed so an early user action isn't dropped (#432).
    let mut attempts = 0u32;
    let mut backoff = INIT_BACKOFF_START;
    let mut pending: Vec<Op> = Vec::new();
    let mut worker = loop {
        match Worker::new() {
            Ok(w) => break w,
            Err(e) => {
                attempts = attempts.saturating_add(1);
                // First failure at warn for visibility; the rest at debug so
                // a machine where EDS never appears doesn't fill the journal.
                if attempts == 1 {
                    tracing::warn!(error = %e, retry_in_s = backoff.as_secs(), "tasks: EDS worker init failed; retrying");
                } else {
                    tracing::debug!(error = %e, attempts, retry_in_s = backoff.as_secs(), "tasks: EDS worker init failed; retrying");
                }
                let alive = wait_backoff(rx, backoff, |op| {
                    if !matches!(op, Op::Refresh) {
                        pending.push(op);
                    }
                });
                if !alive {
                    return; // every sender gone — shutting down
                }
                backoff = next_backoff(backoff);
            }
        }
    };
    if attempts > 0 {
        tracing::info!(attempts, "tasks: EDS worker init succeeded after retries");
    }
    if pending.is_empty() {
        if attempts > 0 {
            // Refresh pings were consumed while backing off — populate now
            // rather than waiting out the next poll tick.
            worker.refresh(tasks_writer, lists_writer);
        }
    } else {
        // Replay buffered writes; each one refreshes via `handle`.
        for op in pending {
            handle(&mut worker, op, tasks_writer, lists_writer);
        }
    }

    // Event loop. Each turn: drain every queued op (so a burst of commands or
    // view-pushed refreshes coalesces), then block in one GMainContext
    // iteration until EDS pushes a change notification or `send_op` wakes us.
    // No polling, no busy spin — fully idle when nothing is happening.
    loop {
        loop {
            match rx.try_recv() {
                Ok(op) => handle(&mut worker, op, tasks_writer, lists_writer),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        // Blocks until a view signal is ready or a wakeup fires. View callbacks
        // run *inside* this call (on this thread) and queue `Op::Refresh`s,
        // which the next `try_recv` drain above picks up.
        ctx.iterate(true);
    }
}

fn handle(
    worker: &mut Worker,
    op: Op,
    tasks_writer: &Mutable<Vec<Task>>,
    lists_writer: &Mutable<Vec<TaskList>>,
) {
    match op {
        Op::Refresh => worker.refresh(tasks_writer, lists_writer),
        Op::Create {
            list_uid,
            uid,
            summary,
            due,
        } => {
            tracing::info!(list = %list_uid, uid = %uid, %summary, "tasks: creating");
            if let Err(e) = worker.create(&list_uid, &uid, &summary, due) {
                tracing::warn!(error = %e, "tasks: create failed");
            }
            worker.refresh(tasks_writer, lists_writer);
        }
        Op::SetCompleted {
            list_uid,
            uid,
            completed,
        } => {
            let res = worker.modify_in_place(&list_uid, &uid, |todo| {
                if completed {
                    todo.status(TodoStatus::Completed);
                    todo.percent_complete(100);
                    todo.completed(chrono::Utc::now());
                } else {
                    todo.mark_uncompleted();
                }
            });
            if let Err(e) = res {
                tracing::warn!(error = %e, "tasks: set_completed failed");
            }
            worker.refresh(tasks_writer, lists_writer);
        }
        Op::Edit {
            list_uid,
            uid,
            summary,
            due,
        } => {
            let res = worker.modify_in_place(&list_uid, &uid, |todo| {
                todo.summary(&summary);
                if let Some(dt) = due {
                    todo.due(date_perhaps_time_from_local(dt));
                } else {
                    todo.remove_due();
                }
            });
            if let Err(e) = res {
                tracing::warn!(error = %e, "tasks: edit failed");
            }
            worker.refresh(tasks_writer, lists_writer);
        }
        Op::Delete { list_uid, uid } => {
            if let Err(e) = worker.delete(&list_uid, &uid) {
                tracing::warn!(error = %e, "tasks: delete failed");
            }
            worker.refresh(tasks_writer, lists_writer);
        }
    }
}

// ── iCalendar parsing + helpers ──────────────────────────────────────────────

/// Parse one iCalendar body that came back from libecal into a [`Task`],
/// or `None` when the body wasn't a usable VTODO (parse error, status
/// = COMPLETED/CANCELLED, or PERCENT-COMPLETE = 100).
fn parse_one(body: &str, list_uid: &str, list_name: &str) -> Option<Task> {
    let parsed: Calendar = body.parse().ok().or_else(|| {
        // libecal hands us bare VTODO without VCALENDAR wrapper sometimes —
        // wrap it ourselves before retrying.
        let wrapped = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//hytte//\r\n{body}\r\nEND:VCALENDAR\r\n"
        );
        wrapped.parse().ok()
    })?;
    let todo = parsed.components.iter().find_map(|c| match c {
        CalendarComponent::Todo(t) => Some(t),
        _ => None,
    })?;

    let status = match todo.get_status() {
        Some(TodoStatus::Completed | TodoStatus::Cancelled) => return None,
        Some(TodoStatus::InProcess) => TaskStatus::InProcess,
        Some(TodoStatus::NeedsAction) | None => TaskStatus::NeedsAction,
    };
    if todo.get_percent_complete() == Some(100) {
        return None;
    }

    let (due, due_all_day) = todo
        .get_due()
        .and_then(dpt_to_local)
        .map_or((None, false), |(dt, all_day)| (Some(dt), all_day));
    let uid = todo
        .get_uid()
        .map_or_else(|| format!("anon:{list_uid}"), str::to_string);
    let summary = todo
        .property_value("SUMMARY")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(no title)".to_string());
    let description = todo
        .property_value("DESCRIPTION")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    Some(Task {
        uid,
        summary,
        description,
        due,
        due_all_day,
        status,
        list_uid: list_uid.to_string(),
        list_name: list_name.to_string(),
    })
}

fn sort_tasks(a: &Task, b: &Task) -> std::cmp::Ordering {
    match (a.due, b.due) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.summary.cmp(&b.summary),
    }
}

fn dpt_to_local(dpt: DatePerhapsTime) -> Option<(DateTime<Local>, bool)> {
    match dpt {
        DatePerhapsTime::Date(d) => {
            let naive = d.and_time(NaiveTime::from_hms_opt(0, 0, 0)?);
            Some((Local.from_local_datetime(&naive).single()?, true))
        }
        DatePerhapsTime::DateTime(cdt) => match cdt {
            CalendarDateTime::Utc(dt) => Some((dt.with_timezone(&Local), false)),
            CalendarDateTime::Floating(naive) => {
                Some((Local.from_local_datetime(&naive).single()?, false))
            }
            ref other @ CalendarDateTime::WithTimezone { ref date_time, .. } => other
                .try_into_utc()
                .map(|utc| (utc.with_timezone(&Local), false))
                .or_else(|| Some((Local.from_local_datetime(date_time).single()?, false))),
        },
    }
}

fn date_perhaps_time_from_local(dt: DateTime<Local>) -> DatePerhapsTime {
    DatePerhapsTime::DateTime(CalendarDateTime::Utc(dt.with_timezone(&chrono::Utc)))
}

fn stamp_now(todo: &mut Todo) {
    let now = chrono::Utc::now();
    todo.add_property("DTSTAMP", format_utc(now).as_str());
    todo.add_property("CREATED", format_utc(now).as_str());
    todo.add_property("LAST-MODIFIED", format_utc(now).as_str());
}

fn bump_last_modified(todo: &mut Todo) {
    let now = chrono::Utc::now();
    todo.add_property("LAST-MODIFIED", format_utc(now).as_str());
    todo.add_property("DTSTAMP", format_utc(now).as_str());
}

fn format_utc(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

fn wrap(todo: &Todo) -> String {
    let mut cal = Calendar::new();
    cal.push(todo.clone());
    cal.to_string()
}

/// Write `~/.config/evolution/sources/trollshell-tasks.source` if it
/// doesn't already exist, then ping EDS's `SourceManager.Reload` so the
/// registry picks it up immediately. Idempotent and best-effort — if
/// gdbus isn't on $PATH we just return the file-write result.
fn ensure_default_local_list() -> anyhow::Result<()> {
    let home = std::env::var("HOME").map_err(|e| anyhow::anyhow!("HOME unset: {e}"))?;
    let xdg_config = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("{home}/.config"));
    let sources_dir = std::path::PathBuf::from(xdg_config).join("evolution/sources");
    let src_path = sources_dir.join("trollshell-tasks.source");
    if src_path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&sources_dir)?;
    std::fs::write(
        &src_path,
        "[Data Source]\nDisplayName=Trollshell Tasks\nEnabled=true\nParent=local-stub\n\n\
         [Task List]\nBackendName=local\nColor=#e6194b\nSelected=true\n",
    )?;
    // Best-effort: SourceManager.Reload picks up our .source immediately.
    // EDS also inotify-watches the dir, so missing this is recoverable —
    // the source shows up on the next refresh tick either way.
    let _ = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.gnome.evolution.dataserver.Sources5",
            "--object-path",
            "/org/gnome/evolution/dataserver/SourceManager",
            "--method",
            "org.gnome.evolution.dataserver.SourceManager.Reload",
        ])
        .output();
    Ok(())
}

fn generate_uid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("trollshell-{}-{nanos}-{n}", std::process::id())
}

// ── Display helpers used by the widget (unchanged from the prior impl) ──────

/// Format a task's due for an `AdwActionRow` subtitle. Empty string when
/// the task has no due.
#[must_use]
pub fn format_due(task: &Task) -> String {
    let Some(due) = task.due else {
        return String::new();
    };
    let now = Local::now();
    let today = now.date_naive();
    let due_date = due.date_naive();
    let delta_days = due_date.signed_duration_since(today).num_days();
    let day_label_str = day_label(due_date, today);

    if delta_days < 0 {
        return overdue_label(delta_days, &day_label_str);
    }
    if task.due_all_day {
        return day_label_str;
    }
    format!("{day_label_str}, {}", due.format("%H:%M"))
}

fn overdue_label(delta_days: i64, day_label_str: &str) -> String {
    match delta_days {
        -1 => "Overdue \u{00b7} Yesterday".to_string(),
        d if d > -7 => format!("Overdue \u{00b7} {} days ago", -d),
        _ => format!("Overdue \u{00b7} {day_label_str}"),
    }
}

fn day_label(d: NaiveDate, today: NaiveDate) -> String {
    let delta = d.signed_duration_since(today).num_days();
    match delta {
        0 => "Today".to_string(),
        1 => "Tomorrow".to_string(),
        _ => format!(
            "{} {} {}",
            weekday_short(d.weekday()),
            d.day(),
            month_short(d.month()),
        ),
    }
}

fn weekday_short(w: chrono::Weekday) -> &'static str {
    match w {
        chrono::Weekday::Mon => "Mon",
        chrono::Weekday::Tue => "Tue",
        chrono::Weekday::Wed => "Wed",
        chrono::Weekday::Thu => "Thu",
        chrono::Weekday::Fri => "Fri",
        chrono::Weekday::Sat => "Sat",
        chrono::Weekday::Sun => "Sun",
    }
}

fn month_short(m: u32) -> &'static str {
    match m {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn task_with(due: Option<DateTime<Local>>) -> Task {
        Task {
            uid: "u".into(),
            summary: "s".into(),
            description: None,
            due,
            due_all_day: false,
            status: TaskStatus::NeedsAction,
            list_uid: "l".into(),
            list_name: "L".into(),
        }
    }

    #[test]
    fn sort_dated_before_undated() {
        let now = Local::now();
        let mut v = [
            task_with(None),
            task_with(Some(now + Duration::days(2))),
            task_with(Some(now - Duration::days(1))),
        ];
        v.sort_by(sort_tasks);
        assert_eq!(v[0].due, Some(now - Duration::days(1)));
        assert!(v[2].due.is_none());
    }

    #[test]
    fn parse_drops_completed() {
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//\r\n\
                    BEGIN:VTODO\r\nUID:a\r\nSUMMARY:Done\r\nSTATUS:COMPLETED\r\nEND:VTODO\r\n\
                    END:VCALENDAR\r\n";
        assert!(parse_one(body, "l", "L").is_none());
    }

    #[test]
    fn parse_keeps_needs_action() {
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//\r\n\
                    BEGIN:VTODO\r\nUID:a\r\nSUMMARY:Live\r\nSTATUS:NEEDS-ACTION\r\nEND:VTODO\r\n\
                    END:VCALENDAR\r\n";
        let t = parse_one(body, "l", "L").unwrap();
        assert_eq!(t.uid, "a");
        assert_eq!(t.summary, "Live");
        assert_eq!(t.status, TaskStatus::NeedsAction);
    }

    #[test]
    fn parse_drops_percent_complete_100() {
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//\r\n\
                    BEGIN:VTODO\r\nUID:a\r\nSUMMARY:Hidden\r\nPERCENT-COMPLETE:100\r\nEND:VTODO\r\n\
                    END:VCALENDAR\r\n";
        assert!(parse_one(body, "l", "L").is_none());
    }

    #[test]
    fn parse_description_present() {
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//\r\n\
                    BEGIN:VTODO\r\nUID:a\r\nSUMMARY:Task\r\nDESCRIPTION:Some note\r\nEND:VTODO\r\n\
                    END:VCALENDAR\r\n";
        let t = parse_one(body, "l", "L").unwrap();
        assert_eq!(t.description, Some("Some note".to_string()));
    }

    #[test]
    fn parse_description_absent_is_none() {
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//\r\n\
                    BEGIN:VTODO\r\nUID:a\r\nSUMMARY:Task\r\nEND:VTODO\r\n\
                    END:VCALENDAR\r\n";
        let t = parse_one(body, "l", "L").unwrap();
        assert_eq!(t.description, None);
    }

    #[test]
    fn parse_description_whitespace_only_is_none() {
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//\r\n\
                    BEGIN:VTODO\r\nUID:a\r\nSUMMARY:Task\r\nDESCRIPTION:   \r\nEND:VTODO\r\n\
                    END:VCALENDAR\r\n";
        let t = parse_one(body, "l", "L").unwrap();
        assert_eq!(t.description, None);
    }

    #[test]
    fn format_due_no_due_is_empty() {
        assert_eq!(format_due(&task_with(None)), "");
    }

    #[test]
    fn format_due_overdue_prefixed() {
        let now = Local::now();
        let s = format_due(&Task {
            due: Some(now - Duration::days(1)),
            due_all_day: true,
            ..task_with(Some(now - Duration::days(1)))
        });
        assert!(s.starts_with("Overdue"), "got {s}");
    }
}
