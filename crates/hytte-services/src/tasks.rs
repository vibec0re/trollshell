//! Task list service backed by evolution-data-server's on-disk VTODO cache.
//!
//! Mirrors [`crate::calendar`] in shape: we read EDS's local file cache
//! (`~/.local/share/evolution/tasks/<source-uid>/calendar.ics`) and parse
//! VTODO components with the `icalendar` crate. The signal [`tasks()`]
//! emits a sorted, filtered list of incomplete tasks across every task
//! list EDS knows about.
//!
//! ## Writes
//!
//! Unlike [`crate::calendar`] (which is read-only), this service supports
//! create/edit/complete/delete via direct iCalendar file rewrites on a
//! dedicated **local-backend** task list provisioned at install time
//! (UID [`EDITABLE_LIST_UID`]).
//!
//! Writes are scoped to that one list — EDS's local backend just owns
//! the `.ics` file with no remote sync, so atomic write (tempfile + rename)
//! in the cache directory is safe. EDS's inotify watch on the file picks
//! up changes the next time any client queries.
//!
//! Tasks belonging to other lists (CalDAV/Google/etc.) are surfaced
//! read-only — their `editable` flag is `false`. Writing to those lists
//! would require driving EDS's per-source private bus connection, which
//! is libecal-only (no rust bindings).
//!
//! ## Provisioning the editable list
//!
//! [`ensure_editable_list`] writes
//! `~/.config/evolution/sources/<EDITABLE_LIST_UID>.source` if missing
//! and seeds the cache `.ics` with an empty VCALENDAR so the first read
//! doesn't race the first write. Called from the polling loop on every
//! refresh — idempotent.
//!
//! ## Background refresh
//!
//! 60-second polling parallel to the calendar service. EDS rewrites its
//! local `.ics` files on sync (for backends that sync); polling catches
//! changes within a minute without an inotify dep.

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveTime, TimeZone};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use icalendar::{
    Calendar, CalendarComponent, CalendarDateTime, Component, DatePerhapsTime, Todo, TodoStatus,
};
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

// ── Public data types ────────────────────────────────────────────────────────

/// One incomplete task ready for rendering. Mirrors the shape of
/// [`crate::calendar::CalendarEvent`] but for VTODO components.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    /// VTODO UID. Stable across edits; used as the row key in the widget
    /// and as the lookup key for write operations.
    pub uid: String,
    /// SUMMARY field, trimmed. Empty SUMMARYs become `"(no title)"`.
    pub summary: String,
    /// Local-time due, if DUE was present on the VTODO. `None` for tasks
    /// with no due date — these sort after dated tasks in [`tasks()`].
    pub due: Option<DateTime<Local>>,
    /// True when DUE was a DATE (no time-of-day). Drives the "All day"
    /// vs. "HH:MM" branch in [`format_due`].
    pub due_all_day: bool,
    /// RFC 5545 PRIORITY: 1 = highest, 9 = lowest, 0 / absent = undefined.
    pub priority: Option<u8>,
    /// Coarse STATUS bucket. Filtered to `NeedsAction` or `InProcess` in
    /// the signal — Completed and Cancelled are dropped at parse time.
    pub status: TaskStatus,
    /// Source-dir name (UID) of the task list this task lives in.
    pub list_uid: String,
    /// Best-effort display name for the list, from the `.source` file
    /// when available, else the source-dir name.
    pub list_name: String,
    /// True iff the task lives in [`EDITABLE_LIST_UID`]. The widget
    /// surfaces this as: full edit affordances vs. read-only display.
    pub editable: bool,
}

/// Subset of `icalendar::TodoStatus` we care about. The signal drops
/// Completed + Cancelled, so widget code only ever sees the first two.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskStatus {
    NeedsAction,
    InProcess,
}

/// UID of the dedicated local-backend task list trollshell owns for
/// writes. Provisioned by [`ensure_editable_list`]. Matches the file name
/// `<uid>.source` in `~/.config/evolution/sources/` and the cache dir
/// `~/.local/share/evolution/tasks/<uid>/`.
pub const EDITABLE_LIST_UID: &str = "trollshell-tasks";

/// Background refresh cadence — matches `calendar::POLL_INTERVAL`.
const POLL_INTERVAL: StdDuration = StdDuration::from_mins(1);

// ── Service handle ───────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct TaskHandles {
    pub(crate) tasks: Mutable<Vec<Task>>,
}

impl Default for TaskHandles {
    fn default() -> Self {
        Self {
            tasks: Mutable::new(Vec::new()),
        }
    }
}

/// Service marker. Pass to `App::with` to register handles + spawn the
/// 60 s refresh task. The first refresh runs immediately and also
/// provisions the editable local list if it doesn't exist yet.
pub struct TasksService;

impl Service for TasksService {
    type Handles = TaskHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = TaskHandles::default();
        let writer = handles.tasks.clone();
        rt.spawn(async move {
            poll_loop(writer).await;
        });
        handles
    }
}

#[must_use]
pub fn service() -> TasksService {
    TasksService
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Signal of incomplete tasks across every EDS task list. Sorted so:
/// 1. tasks with a `due` come first, ascending (overdue → soonest);
/// 2. tasks without a `due` come last, alphabetised by summary.
///
/// Completed + Cancelled tasks are dropped at parse time. Empty until the
/// first refresh completes (or if the EDS tasks cache directory is
/// missing).
pub fn tasks() -> impl Signal<Item = Vec<Task>> {
    registry::with(|r| {
        r.get::<TaskHandles>()
            .expect("tasks::service() not registered")
            .tasks
            .signal_cloned()
    })
}

/// Re-scan the EDS tasks cache and update the [`tasks()`] signal. Safe to
/// call from page-show / open handlers. Heavy work (filesystem walk +
/// iCal parse) runs on a blocking pool.
pub fn refresh() {
    let writer = registry::with(|r| r.get::<TaskHandles>().map(|h| h.tasks.clone()));
    let Some(writer) = writer else {
        tracing::warn!("tasks::refresh: service not registered");
        return;
    };
    hytte_reactive::runtime::handle().spawn_blocking(move || {
        do_refresh(&writer);
    });
}

/// Create a new task in the editable list. `due` may be `None` for
/// no-deadline tasks. Returns the generated UID. After the write the
/// signal is refreshed asynchronously so subscribers see the new row.
///
/// All writes target [`EDITABLE_LIST_UID`] — there's no per-list write
/// support yet (would require libecal for non-local backends).
#[must_use = "the returned UID is the only way to address the new task"]
pub fn create_task(summary: String, due: Option<DateTime<Local>>) -> String {
    let uid = generate_uid();
    let uid_clone = uid.clone();
    spawn_write(move || {
        let mut todo = Todo::new();
        todo.summary(&summary);
        todo.uid(&uid_clone);
        todo.status(TodoStatus::NeedsAction);
        stamp_now(&mut todo);
        if let Some(due) = due {
            todo.due(date_perhaps_time_from_local(due));
        }
        rewrite_editable(|cal| {
            cal.components.push(CalendarComponent::Todo(todo));
        })?;
        Ok(())
    });
    uid
}

/// Mark a task as completed (or re-open it). Only affects tasks in
/// [`EDITABLE_LIST_UID`]; calls against tasks from other lists are
/// silently ignored (the widget hides the checkbox in those cases).
pub fn set_completed(uid: &str, completed: bool) {
    let uid = uid.to_string();
    spawn_write(move || {
        rewrite_editable(|cal| {
            for c in &mut cal.components {
                let CalendarComponent::Todo(todo) = c else {
                    continue;
                };
                if todo.get_uid().is_some_and(|u| u == uid) {
                    if completed {
                        todo.status(TodoStatus::Completed);
                        todo.percent_complete(100);
                        todo.completed(chrono::Utc::now());
                    } else {
                        todo.mark_uncompleted();
                    }
                    bump_last_modified(todo);
                }
            }
        })?;
        Ok(())
    });
}

/// Edit a task's summary + due. Same scope rules as [`set_completed`].
pub fn edit_task(uid: &str, summary: String, due: Option<DateTime<Local>>) {
    let uid = uid.to_string();
    spawn_write(move || {
        rewrite_editable(|cal| {
            for c in &mut cal.components {
                let CalendarComponent::Todo(todo) = c else {
                    continue;
                };
                if todo.get_uid().is_some_and(|u| u == uid) {
                    todo.summary(&summary);
                    if let Some(due) = due {
                        todo.due(date_perhaps_time_from_local(due));
                    } else {
                        todo.remove_due();
                    }
                    bump_last_modified(todo);
                }
            }
        })?;
        Ok(())
    });
}

/// Remove a task. Same scope rules as [`set_completed`].
pub fn delete_task(uid: &str) {
    let uid = uid.to_string();
    spawn_write(move || {
        rewrite_editable(|cal| {
            cal.components.retain(|c| match c {
                CalendarComponent::Todo(t) => t.get_uid().is_none_or(|u| u != uid),
                _ => true,
            });
        })?;
        Ok(())
    });
}

// ── Polling loop ─────────────────────────────────────────────────────────────

async fn poll_loop(writer: Mutable<Vec<Task>>) {
    loop {
        let writer_for_blocking = writer.clone();
        if let Err(e) =
            tokio::task::spawn_blocking(move || do_refresh(&writer_for_blocking)).await
        {
            tracing::error!(error = %e, "tasks refresh task panicked");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn do_refresh(writer: &Mutable<Vec<Task>>) {
    // Idempotent provisioning on every tick — costs almost nothing and
    // means a fresh user account works the first time the shell starts
    // without any out-of-band setup.
    if let Err(e) = ensure_editable_list() {
        tracing::debug!(error = %e, "tasks: ensure_editable_list failed");
    }

    let snapshot = scan_cache_dir();
    let changed = {
        let cur = writer.lock_ref();
        *cur != snapshot
    };
    if changed {
        writer.set(snapshot);
    }
}

// ── Filesystem scanning ──────────────────────────────────────────────────────

fn xdg_data_home() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".local/share"))
}

fn xdg_config_home() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg));
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config"))
}

fn cache_root() -> Option<PathBuf> {
    Some(xdg_data_home()?.join("evolution").join("tasks"))
}

fn editable_cache_dir() -> Option<PathBuf> {
    Some(cache_root()?.join(EDITABLE_LIST_UID))
}

fn editable_ics_path() -> Option<PathBuf> {
    Some(editable_cache_dir()?.join("calendar.ics"))
}

fn sources_dir() -> Option<PathBuf> {
    Some(xdg_config_home()?.join("evolution").join("sources"))
}

fn editable_source_path() -> Option<PathBuf> {
    Some(sources_dir()?.join(format!("{EDITABLE_LIST_UID}.source")))
}

fn scan_cache_dir() -> Vec<Task> {
    let Some(root) = cache_root() else {
        return Vec::new();
    };
    let entries = match std::fs::read_dir(&root) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::debug!(error = %e, dir = %root.display(), "tasks: cache dir read failed");
            return Vec::new();
        }
    };

    let mut out: Vec<Task> = Vec::new();
    for entry in entries.flatten() {
        let source_dir = entry.path();
        if !source_dir.is_dir() {
            continue;
        }
        let ics = source_dir.join("calendar.ics");
        if !ics.is_file() {
            continue;
        }
        let list_uid = source_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("tasks")
            .to_string();
        let list_name = source_display_name(&list_uid).unwrap_or_else(|| list_uid.clone());
        let editable = list_uid == EDITABLE_LIST_UID;
        match parse_ics_file(&ics, &list_uid, &list_name, editable) {
            Ok(mut ts) => out.append(&mut ts),
            Err(e) => tracing::warn!(error = %e, file = %ics.display(), "tasks: parse failed"),
        }
    }

    out.sort_by(sort_tasks);
    out
}

/// Sort: dated tasks ascending by due (overdue → soonest), then no-due
/// tasks alphabetised. Treats `None` as +∞ so the comparison is total.
fn sort_tasks(a: &Task, b: &Task) -> std::cmp::Ordering {
    match (a.due, b.due) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.summary.cmp(&b.summary),
    }
}

/// Best-effort `DisplayName` lookup from
/// `~/.config/evolution/sources/<uid>.source`. EDS keys are
/// `DisplayName=…` (plus localised variants); we honour only the
/// untagged value to avoid pulling a full locale-resolution helper in.
fn source_display_name(uid: &str) -> Option<String> {
    let path = sources_dir()?.join(format!("{uid}.source"));
    let body = std::fs::read_to_string(path).ok()?;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("DisplayName=") {
            let name = rest.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn parse_ics_file(
    path: &Path,
    list_uid: &str,
    list_name: &str,
    editable: bool,
) -> anyhow::Result<Vec<Task>> {
    let body = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let parsed: Calendar = body
        .parse()
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;

    let mut out = Vec::new();
    for todo in parsed.components.iter().filter_map(|c| match c {
        CalendarComponent::Todo(t) => Some(t),
        _ => None,
    }) {
        let status = match todo.get_status() {
            // Drop completed + cancelled at parse time — the signal only
            // ever surfaces actionable tasks.
            Some(TodoStatus::Completed | TodoStatus::Cancelled) => continue,
            Some(TodoStatus::InProcess) => TaskStatus::InProcess,
            // Absent STATUS defaults to NEEDS-ACTION per RFC 5545 §3.8.1.11.
            Some(TodoStatus::NeedsAction) | None => TaskStatus::NeedsAction,
        };

        // Belt-and-suspenders: some clients set PERCENT-COMPLETE=100
        // without flipping STATUS. Treat that as completed too.
        if todo.get_percent_complete() == Some(100) {
            continue;
        }

        let (due, due_all_day) = todo.get_due().and_then(dpt_to_local).map_or((None, false), |(dt, all_day)| (Some(dt), all_day));

        let uid = todo
            .get_uid()
            .map_or_else(|| format!("anon:{list_uid}:{}", out.len()), str::to_string);
        let summary = todo
            .property_value("SUMMARY")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(no title)".to_string());
        let priority = todo
            .property_value("PRIORITY")
            .and_then(|s| s.trim().parse::<u8>().ok())
            .filter(|p| *p > 0);

        out.push(Task {
            uid,
            summary,
            due,
            due_all_day,
            priority,
            status,
            list_uid: list_uid.to_string(),
            list_name: list_name.to_string(),
            editable,
        });
    }
    Ok(out)
}

/// Same conversion as `calendar::dpt_to_local`. Kept in-module to avoid
/// re-exporting a sibling crate's internal helper. Returns `(local_dt,
/// all_day)`, or `None` if normalisation failed.
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

// ── Write path: editable list provisioning + atomic rewrite ──────────────────

/// Ensure the editable local task list is provisioned. Creates the
/// `.source` file and an empty cache `.ics` if they're missing. Safe to
/// call concurrently with EDS — the operations are idempotent and the
/// cache write is an atomic `rename`.
pub fn ensure_editable_list() -> anyhow::Result<()> {
    let src_path =
        editable_source_path().ok_or_else(|| anyhow::anyhow!("no $HOME for sources dir"))?;
    if !src_path.exists() {
        if let Some(parent) = src_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &src_path,
            "[Data Source]\nDisplayName=Trollshell Tasks\nEnabled=true\nParent=local-stub\n\n\
             [Task List]\nBackendName=local\nColor=#e6194b\nSelected=true\n",
        )?;
        // Best-effort: nudge EDS to re-scan. If the bus isn't running
        // (headless test rig), we silently move on — the next refresh
        // tick or shell restart picks it up.
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
    }
    let ics_path = editable_ics_path().ok_or_else(|| anyhow::anyhow!("no $HOME for cache dir"))?;
    if !ics_path.exists() {
        if let Some(parent) = ics_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &ics_path,
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//hytte//trollshell//EN\r\n\
             END:VCALENDAR\r\n",
        )?;
    }
    Ok(())
}

/// Drop the work onto the tokio runtime's blocking pool. The closure
/// performs the load-mutate-save cycle; after success we trigger a
/// refresh so subscribers see the new state without waiting for the
/// 60-second poll tick.
fn spawn_write(work: impl FnOnce() -> anyhow::Result<()> + Send + 'static) {
    hytte_reactive::runtime::handle().spawn_blocking(move || {
        if let Err(e) = work() {
            tracing::warn!(error = %e, "tasks: write failed");
            return;
        }
        // Push fresh state to subscribers immediately.
        if let Some(writer) = registry::with(|r| r.get::<TaskHandles>().map(|h| h.tasks.clone())) {
            do_refresh(&writer);
        }
    });
}

/// Load the editable list's `.ics`, hand the parsed `Calendar` to `edit`,
/// then write the result back atomically (tempfile + rename in the same
/// directory). Caller is responsible for keeping the mutation O(1)/O(n)
/// — we hold no locks beyond the file's natural rename atomicity.
fn rewrite_editable(edit: impl FnOnce(&mut Calendar)) -> anyhow::Result<()> {
    ensure_editable_list()?;
    let ics_path =
        editable_ics_path().ok_or_else(|| anyhow::anyhow!("no $HOME for cache dir"))?;
    let body = std::fs::read_to_string(&ics_path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", ics_path.display()))?;
    let mut cal: Calendar = body
        .parse()
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", ics_path.display()))?;
    edit(&mut cal);

    let serialized = cal.to_string();
    let parent = ics_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("editable .ics has no parent dir"))?;
    let tmp = parent.join(format!(".calendar.ics.tmp.{}", std::process::id()));
    std::fs::write(&tmp, serialized.as_bytes())?;
    std::fs::rename(&tmp, &ics_path)?;
    Ok(())
}

fn stamp_now(todo: &mut Todo) {
    let now = chrono::Utc::now();
    // DTSTAMP is required by RFC 5545; LAST-MODIFIED is conventional so
    // clients displaying "edited at" timestamps don't show 1970.
    todo.add_property("DTSTAMP", format_utc_for_ical(now).as_str());
    todo.add_property("CREATED", format_utc_for_ical(now).as_str());
    todo.add_property("LAST-MODIFIED", format_utc_for_ical(now).as_str());
}

fn bump_last_modified(todo: &mut Todo) {
    let now = chrono::Utc::now();
    todo.add_property("LAST-MODIFIED", format_utc_for_ical(now).as_str());
    todo.add_property("DTSTAMP", format_utc_for_ical(now).as_str());
}

fn format_utc_for_ical(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

/// UUIDs would be nicer but the project doesn't yet take a `uuid` dep;
/// `pid + nanos + counter` is collision-resistant enough for a single
/// local user's task list.
fn generate_uid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("trollshell-{}-{nanos}-{n}", std::process::id())
}

/// Map a `chrono::DateTime<Local>` back into `icalendar`'s
/// `DatePerhapsTime` for the DUE property — UTC datetime, or DATE-only
/// when `due_all_day` is set on the original `Task`.
fn date_perhaps_time_from_local(dt: DateTime<Local>) -> DatePerhapsTime {
    // We always serialise as DATE-TIME (UTC) for simplicity — callers
    // wanting an all-day task can pass a midnight `dt`; the round-trip
    // through dpt_to_local will still surface it. Full DATE-only writes
    // are a future enhancement once the widget grows a "date only" UX.
    DatePerhapsTime::DateTime(CalendarDateTime::Utc(dt.with_timezone(&chrono::Utc)))
}

// ── Display helpers used by the widget ───────────────────────────────────────

/// Format a task's due for an `AdwActionRow` subtitle. Returns labels like:
/// - `"Overdue \u{00b7} Yesterday"` / `"Overdue \u{00b7} 3 days ago"`
/// - `"Today, 14:00"` / `"Today"`
/// - `"Tomorrow"` / `"Mon 14 Apr"`
/// - empty string for tasks with no due date
#[must_use]
pub fn format_due(task: &Task) -> String {
    let Some(due) = task.due else { return String::new(); };
    let now = Local::now();
    let today = now.date_naive();
    let due_date = due.date_naive();
    let delta_days = due_date.signed_duration_since(today).num_days();
    let day_label = day_label(due_date, today);

    if delta_days < 0 {
        return overdue_label(delta_days, &day_label);
    }

    if task.due_all_day {
        return day_label;
    }
    format!("{day_label}, {}", due.format("%H:%M"))
}

fn overdue_label(delta_days: i64, day_label: &str) -> String {
    match delta_days {
        -1 => "Overdue \u{00b7} Yesterday".to_string(),
        d if d > -7 => format!("Overdue \u{00b7} {} days ago", -d),
        _ => format!("Overdue \u{00b7} {day_label}"),
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
    use chrono::{Duration, TimeZone};

    fn task_with(due: Option<DateTime<Local>>) -> Task {
        Task {
            uid: "u".into(),
            summary: "s".into(),
            due,
            due_all_day: false,
            priority: None,
            status: TaskStatus::NeedsAction,
            list_uid: "l".into(),
            list_name: "L".into(),
            editable: false,
        }
    }

    #[test]
    fn editable_list_uid_is_trollshell_tasks() {
        // Widget code keys off this constant for the editable affordances;
        // changing the literal would silently break write routing.
        assert_eq!(EDITABLE_LIST_UID, "trollshell-tasks");
    }

    #[test]
    fn parse_filters_completed_and_cancelled() {
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//\r\n\
                    BEGIN:VTODO\r\nUID:a\r\nSUMMARY:Done\r\nSTATUS:COMPLETED\r\nEND:VTODO\r\n\
                    BEGIN:VTODO\r\nUID:b\r\nSUMMARY:Killed\r\nSTATUS:CANCELLED\r\nEND:VTODO\r\n\
                    BEGIN:VTODO\r\nUID:c\r\nSUMMARY:Live\r\nEND:VTODO\r\n\
                    END:VCALENDAR\r\n";
        let path = std::env::temp_dir().join("hytte-tasks-test-filter.ics");
        std::fs::write(&path, body).unwrap();
        let ts = parse_ics_file(&path, "l", "L", false).unwrap();
        assert_eq!(ts.len(), 1);
        assert_eq!(ts[0].uid, "c");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parse_filters_percent_complete_100() {
        // Some clients (gnome-tasks) set PERCENT-COMPLETE=100 without
        // STATUS=COMPLETED. Treat that as completed regardless.
        let body = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//t//\r\n\
                    BEGIN:VTODO\r\nUID:a\r\nSUMMARY:HiddenDone\r\nPERCENT-COMPLETE:100\r\nEND:VTODO\r\n\
                    END:VCALENDAR\r\n";
        let path = std::env::temp_dir().join("hytte-tasks-test-pc100.ics");
        std::fs::write(&path, body).unwrap();
        let ts = parse_ics_file(&path, "l", "L", false).unwrap();
        assert!(ts.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sort_dated_before_undated_then_by_due() {
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
    fn format_due_overdue() {
        let now = Local::now();
        let yesterday = now - Duration::days(1);
        let t = Task { due: Some(yesterday), due_all_day: true, ..task_with(Some(yesterday)) };
        let s = format_due(&t);
        assert!(s.starts_with("Overdue"), "got {s}");
    }

    #[test]
    fn format_due_today_with_time() {
        let now = Local::now();
        let t_dt = Local
            .with_ymd_and_hms(now.year(), now.month(), now.day(), 14, 0, 0)
            .single()
            .unwrap();
        let t = task_with(Some(t_dt));
        let s = format_due(&t);
        assert!(s.starts_with("Today"), "got {s}");
        assert!(s.contains("14:00"), "got {s}");
    }

    #[test]
    fn format_due_no_due_is_empty() {
        assert_eq!(format_due(&task_with(None)), "");
    }
}
