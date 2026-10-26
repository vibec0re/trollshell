//! Calendar service backed by Evolution Data Server via libecal FFI.
//!
//! Reads go through [`hytte_ecal`] (our hand-written libecal-2.0 /
//! libedataserver-1.2 / libical-glib bindings) — the same path the task
//! service uses, so this sees **ANY** EDS backend the user has
//! configured: local `.ics`, `CalDAV` (Nextcloud / generic), Google (via
//! the GOA bridge), Microsoft EWS, etc. This replaces the original
//! `.ics`-cache file-poller, which could only see local sources: `CalDAV`
//! calendars cache to an `SQLite` `cache.db` under `~/.cache/evolution`, not
//! the `calendar.ics` the poller walked, so they were invisible by
//! construction. libecal reads them all uniformly.
//!
//! ## Threading
//!
//! Calling libecal from arbitrary tokio worker threads doesn't compose
//! with `GObject`'s main-context model and isn't [`Sync`]-safe — so (as in
//! [`crate::tasks`]) all EDS work happens on a single dedicated thread that
//! owns one [`hytte_ecal::Registry`] and a [`HashMap`] of cached
//! [`hytte_ecal::CalClient`] connections (one per calendar source UID,
//! opened lazily on first use).
//!
//! Both the init and the cache are resilient (#432, shared machinery in
//! `crate::eds_retry`): a failed worker init retries with backoff (the
//! boot race against evolution-data-server's own activation), a cached
//! client that errors is evicted and reconnected (EDS restart / calendar
//! removed), and repeated all-sources failures rebuild the whole session.
//!
//! The service exposes [`events()`] — `Signal<Vec<CalendarEvent>>`, sorted
//! by start time. It covers the **union** of:
//!
//! 1. The currently-viewed calendar month (set by the widget via
//!    [`set_viewed_month`]) — so past-day dots + past-day click-to-list work
//!    for the visible month grid (issue #100).
//! 2. The forward upcoming window `[now, now + WINDOW_DAYS]` — keeps the
//!    Upcoming list data intact.
//!
//! [`refresh()`] enqueues an out-of-cycle re-scan onto the worker. The
//! public surface is read-only and identical to the prior file-poller, so
//! the bar widget + drawer page need no changes beyond calling
//! `set_viewed_month`.
//!
//! ## Recurrence expansion (issue #29)
//!
//! The scan asks libecal to **expand** every component over the query window
//! via [`CalClient::generate_instances`][hytte_ecal::CalClient::generate_instances]
//! (which drives libical's recurrence iterator). Recurring events (RRULE)
//! come back as **one occurrence per instance** inside the window — a daily
//! standup yields ~`WINDOW_DAYS` rows, matching GNOME Calendar — and
//! non-recurring events as a single instance.
//!
//! Previously the scan used `get_object_strings("#t")`, which returns only
//! the **master** component: a recurring event surfaced at most one row (on
//! its original DTSTART, frequently outside the then-7-day window → nothing
//! at all). That was the empty-month-grid half of #29.
//!
//! The window is the **only** bound on expansion, so an unbounded
//! `FREQ=DAILY` series is naturally capped by the range rather than expanded
//! years out. The rest of the recurrence set (RFC 5545 §3.8.5) is applied by
//! `hytte-ecal` during that expansion: `EXDATE` occurrences are dropped and
//! in-window `RDATE`s added (deduped against the RRULE-expanded starts, with
//! `EXDATE` winning when the two collide), in both DATE and DATE-TIME forms.
//!
//! # Limitations / notes
//!
//! - Timezones: instance bounds come back as absolute UTC seconds (libical
//!   anchors the recurrence to UTC during expansion), so the old per-field
//!   `WithTimezone`/chrono-tz fallback no longer gates recurring events.
//!   All-day instances anchor to local midnight.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Duration as StdDuration;

use chrono::{
    DateTime, Datelike, Duration, Local, MappedLocalTime, NaiveDate, NaiveDateTime, NaiveTime,
    Offset, TimeZone, Utc,
};
use futures_signals::signal::{Mutable, Signal};
use hytte_ecal::sys::ECalClientSourceType;
use hytte_ecal::{CalClient, EventInstance, Registry, Source};
use hytte_reactive::{Service, registry, spawn_supervised};
use icalendar::{Calendar, CalendarComponent, Component, EventLike, EventStatus};

use crate::eds_labels::short_date;
use crate::eds_retry::{
    INIT_BACKOFF_START, SourceFailureStreak, maybe_rebuild_session, next_backoff, spawn_eds_worker,
    wait_backoff, with_client_retry,
};

// ── Public data types ────────────────────────────────────────────────────────

/// One upcoming calendar event ready for rendering. Times are normalised
/// to local time at parse time so the UI never re-runs timezone math.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarEvent {
    /// VEVENT UID. Stable across edits to the same event; used by future
    /// "open in calendar app" actions.
    pub uid: String,
    /// SUMMARY field, trimmed. Empty SUMMARYs become `"(no title)"`.
    pub summary: String,
    /// Local-time start. For all-day events this is midnight on the date.
    pub start: DateTime<Local>,
    /// Local-time end. For all-day events this is the end-of-day boundary
    /// derived from DTEND or DTSTART+1d.
    pub end: DateTime<Local>,
    /// LOCATION field if present and non-empty.
    pub location: Option<String>,
    /// True when DTSTART was a DATE (no time-of-day). Drives the "All day"
    /// label in the page.
    pub all_day: bool,
    /// Calendar source label, from the EDS source's `DisplayName=`. With
    /// the libecal backend this is the human-readable calendar title
    /// (e.g. "Personal", "Work") rather than the UUID dir-name the old
    /// `.ics` poller was limited to.
    pub calendar_name: String,
    /// The colour the user picked for this calendar, from the EDS source's
    /// `[Calendar] Color=` key ([`hytte_ecal::Source::color`]) — a `#rrggbb`
    /// or `#rrggbbaa` string, already shape-checked there. `None` when the
    /// source carries no colour, when its spelling isn't hex, **or** when EDS
    /// is reporting its own construct-time default rather than a choice
    /// ([`hytte_ecal::EDS_DEFAULT_CALENDAR_COLOR`] — there is no "unset" at
    /// the FFI layer, so without that the fallback below would be dead code
    /// and every uncoloured calendar would share one blue). In each case the
    /// UI falls back to its hash-derived palette keyed by
    /// [`calendar_name`](Self::calendar_name). Deliberately **not** on the
    /// plugin wire — `plugins::pump` sends `calendar_name` only (#542).
    pub calendar_color: Option<String>,
}

/// How far ahead to expand + surface events, as a rolling window from now.
///
/// **Scope default (issue #29):** 30 days. This decouples the data window
/// from the old hard 7-day cap, so events further out — and recurrences
/// expanded across the next month — actually show. The recurrence
/// expansion is bounded by this same window (a daily event yields ~30
/// instances, never an unbounded series), so widening it is the only knob.
/// Tune here if the upcoming list should reach further (e.g. the full
/// visible month-grid) or shorter.
const WINDOW_DAYS: i64 = 30;

/// Background refresh cadence. EDS syncs upstream on its own schedule
/// (typically minutes); 60 s catches changes within a minute without
/// hammering the backend.
const POLL_INTERVAL: StdDuration = StdDuration::from_mins(1);

// ── Worker channel ───────────────────────────────────────────────────────────

/// Channel handle to the dedicated EDS thread. A unit message is a
/// "re-scan now" request — the calendar is read-only, so there's nothing
/// richer to carry (cf. `tasks::Op`, which also encodes writes).
static SENDER: OnceLock<mpsc::Sender<()>> = OnceLock::new();

fn send_refresh() {
    let Some(tx) = SENDER.get() else {
        tracing::warn!("calendar: worker not started; refresh dropped");
        return;
    };
    if let Err(e) = tx.send(()) {
        tracing::warn!(error = %e, "calendar: worker channel closed");
    }
}

// ── Viewed-month state (issue #100) ─────────────────────────────────────────

/// The (year, month) the grid is currently displaying, as set by the widget
/// via [`set_viewed_month`]. `None` means "not yet set" — the scan falls
/// back to the current month. Stored as a pair of `i32`/`u32` wrapped in a
/// `Mutex` so the GTK-main-thread widget can write without a tokio context.
static VIEWED_MONTH: Mutex<Option<(i32, u32)>> = Mutex::new(None);

/// Tell the service which month the calendar grid is currently showing.
///
/// Call this from the widget whenever `viewed` changes — in `prev_btn` /
/// `next_btn` handlers, in `on_day_clicked` if the month changes, and once
/// at init. The service will compute a query window that is the **union** of
/// the viewed month's full 6-week grid range and the forward upcoming window,
/// then trigger a re-scan so the grid can show past-day dots and click-to-list
/// events.
pub fn set_viewed_month(year: i32, month: u32) {
    {
        let mut guard = VIEWED_MONTH
            .lock()
            .expect("calendar VIEWED_MONTH lock poisoned");
        *guard = Some((year, month));
    }
    send_refresh();
}

// ── Service handle ───────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct CalendarHandles {
    pub(crate) events: Mutable<Vec<CalendarEvent>>,
}

impl Default for CalendarHandles {
    fn default() -> Self {
        Self {
            events: Mutable::new(Vec::new()),
        }
    }
}

/// Service marker. Pass to `App::with` to register handles, spawn the EDS
/// worker thread, and start the 60 s refresh ticker. The ticker fires an
/// initial refresh immediately.
pub struct CalendarService;

impl Service for CalendarService {
    type Handles = CalendarHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = CalendarHandles::default();
        let writer = handles.events.clone();

        // Channel: tokio ticker + public refresh() → dedicated EDS thread.
        let (tx, rx) = mpsc::channel::<()>();
        let _ = SENDER.set(tx);

        // libecal isn't Sync and wants to be pinned to one thread (the same
        // constraint the tasks service handles); give the calendar its own
        // EDS worker thread owning a Registry + a per-source CalClient cache.
        //
        // Supervised since #1170 (the residual of #430): a panic under
        // `run_worker` — libecal FFI, or `icalendar` parsing whatever a CalDAV
        // server returned — used to kill the thread and freeze `events` for the
        // session with no log line and nothing to restart it.
        //
        // **What a restart re-does:** a fresh `hytte_ecal::Registry` and an
        // empty per-source `CalClient` cache, then the #432 init-with-backoff
        // dance again — i.e. it reconnects to the daemon that holds the state,
        // which is the supervisor's own restart-safety argument. `SENDER` is
        // set once here, not per run, and the `Receiver` outlives every run
        // inside `spawn_eds_worker`, so refresh pings queued while the worker
        // was down are picked up by the run that follows — bar the one already
        // `recv`'d when the panic hit, which costs this service a refresh the
        // next ping repairs (`tasks`' loss is worse; see its call site).
        spawn_eds_worker("calendar-eds", rx, move |rx| run_worker(rx, &writer));

        // Refresh ticker. The first send fires immediately (initial
        // populate); thereafter every POLL_INTERVAL.
        spawn_supervised("calendar", || async {
            loop {
                send_refresh();
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        });

        handles
    }
}

#[must_use]
pub fn service() -> CalendarService {
    CalendarService
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Signal of events covering the union of the viewed month's grid range and
/// the forward upcoming window (`now … now + WINDOW_DAYS`), sorted ascending
/// by start. Empty until the first refresh completes (or if EDS has no
/// calendar sources configured).
pub fn events() -> impl Signal<Item = Vec<CalendarEvent>> {
    registry::with(|r| {
        r.get::<CalendarHandles>()
            .expect("calendar::service() not registered")
            .events
            .signal_cloned()
    })
}

/// Trigger an out-of-cycle refresh. Idempotent — safe to call from
/// page-show handlers (the drawer calls this on `Page::Calendar`). The
/// EDS round-trip runs on the dedicated worker thread, so this returns
/// immediately.
pub fn refresh() {
    send_refresh();
}

// ── EDS worker thread ────────────────────────────────────────────────────────

struct Worker {
    registry: Registry,
    clients: HashMap<String, CalClient>,
    /// Consecutive all-sources-failed scan tracker (#432). When it trips,
    /// [`Worker::maybe_rebuild`] tears down and reopens the whole session.
    failure_streak: SourceFailureStreak,
    /// Set by [`Worker::scan_all`] when the streak trips; consumed by
    /// [`Worker::maybe_rebuild`].
    rebuild_pending: bool,
}

impl Worker {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            registry: Registry::new()?,
            clients: HashMap::new(),
            failure_streak: SourceFailureStreak::default(),
            rebuild_pending: false,
        })
    }

    /// Query expanded instances for one source (#1172: the evict-and-retry
    /// policy now lives in [`with_client_retry`], shared with `tasks.rs`
    /// rather than hand-rolled here) — a cached [`CalClient`] whose backing
    /// daemon restarted (or whose calendar was removed) errors on use, and a
    /// failure on a *previously cached* client is the EDS-restart signature,
    /// so the shared helper evicts it and retries immediately on a fresh
    /// connection. One EDS restart then costs one poll cycle, not the rest of
    /// the session (#432).
    fn instances(
        &mut self,
        source_uid: &str,
        start_unix: i64,
        end_unix: i64,
    ) -> anyhow::Result<Vec<EventInstance>> {
        // Bound separately from `self.clients` so the two are disjoint
        // borrows across the call below (the `open` closure only touches
        // `registry`, never `self` as a whole).
        let registry = &self.registry;
        // 5 s connect budget — same as the task service; bumped above the
        // libecal default so CalDAV/Google backends have time to come online.
        with_client_retry(
            "calendar",
            &mut self.clients,
            source_uid,
            || {
                let Some(src) = registry.ref_source(source_uid)? else {
                    anyhow::bail!("EDS source '{source_uid}' not found");
                };
                CalClient::connect(&src, ECalClientSourceType::Events, 5)
            },
            |c| c.generate_instances(start_unix, end_unix),
            // Nothing else is tied to a source UID here — `calendar.rs` keeps
            // no per-source view cache the way `tasks.rs` does.
            || {},
        )
    }

    /// If the last scan tripped the all-sources-failed streak, tear down and
    /// rebuild the whole EDS session (#1172: shared with `tasks.rs` via
    /// [`maybe_rebuild_session`]) — the per-client evict path above can't
    /// help when the registry connection itself died. Returns `true` when a
    /// rebuild happened (the caller should rescan immediately on the fresh
    /// session).
    fn maybe_rebuild(&mut self) -> bool {
        maybe_rebuild_session(
            "calendar",
            &mut self.rebuild_pending,
            &mut self.registry,
            Registry::new,
            || self.clients.clear(),
        )
    }

    /// Re-scan every calendar source and publish a fresh `Vec` on **every**
    /// pass — unconditionally, not only when the event set changed. Downstream
    /// consumers derive *time-relative* views off this signal at emit time: the
    /// plugin host's upcoming-calendar digest projects a rolling 24 h window
    /// (`plugins::pump::to_upcoming_events`), and the drawer widget drops
    /// fully-elapsed events against a live `now`. Both need a wall-clock re-emit
    /// to stay fresh between calendar changes, so the minutely poll tick
    /// (`POLL_INTERVAL`) doubles as that re-anchor. Re-publishing an unchanged
    /// list every minute is cheap; the earlier `PartialEq` emit-dedup instead
    /// left those windows frozen at the last list change, drifting to surface
    /// just-ended events (#542). Writes are single-owner (this worker thread), so
    /// the unconditional `set` can't race the on-demand `refresh()` path.
    fn refresh(&mut self, writer: &Mutable<Vec<CalendarEvent>>) {
        let mut snapshot = self.scan_all();
        if self.maybe_rebuild() {
            // The session was just rebuilt after repeated total failures —
            // rescan now on the fresh connection rather than serving the
            // failed pass and waiting out another poll interval.
            snapshot = self.scan_all();
        }
        writer.set(snapshot);
    }

    fn scan_all(&mut self) -> Vec<CalendarEvent> {
        let viewed = *VIEWED_MONTH
            .lock()
            .expect("calendar VIEWED_MONTH lock poisoned");
        let (scan_start, scan_end) = scan_window(&Local::now(), viewed);

        let start_unix = scan_start.timestamp();
        let end_unix = scan_end.timestamp();

        // Re-read the source list each pass so a calendar added/removed at
        // runtime (e.g. a new Nextcloud calendar discovered under the
        // collection source) is picked up without a restart.
        let sources = self.registry.calendars();
        // Drop cached clients for sources that no longer exist — a removed
        // calendar's handle would otherwise sit dead in the cache forever
        // (#432).
        let live: HashSet<String> = sources.iter().map(Source::uid).collect();
        self.clients.retain(|uid, _| live.contains(uid));
        let mut failed = 0usize;
        let mut out: Vec<CalendarEvent> = Vec::new();
        for src in &sources {
            let source_uid = src.uid();
            let calendar_name = src.display_name();
            // Read once per source per scan, not once per instance — the
            // colour is a property of the source, and every event it yields
            // carries the same one.
            let calendar_color = src.color();
            // Ask libecal to EXPAND every component over the window: each
            // recurring event yields one instance per occurrence inside
            // [start_unix, end_unix), with authoritative per-instance
            // start/end (RRULE/RDATE applied, EXDATE removed). Non-recurring
            // events come back as a single instance. This replaces the old
            // `get_object_strings("#t")` master-only path — see the
            // module-level "Recurrence expansion" note (#29).
            let instances = match self.instances(&source_uid, start_unix, end_unix) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(source = %source_uid, error = %e, "calendar: source scan failed; will reconnect on the next refresh");
                    failed += 1;
                    continue;
                }
            };
            out.extend(source_instances_to_events(
                &instances,
                &calendar_name,
                calendar_color.as_deref(),
                scan_start,
                out.len(),
            ));
        }
        // Every source failing repeatedly means the registry session itself
        // is likely dead — schedule a full rebuild (#432).
        if self.failure_streak.record(sources.len(), failed) {
            self.rebuild_pending = true;
        }
        out.sort_by_key(|e| e.start);
        out
    }
}

/// The `[start, end)` window [`Worker::scan_all`] asks libecal to expand every
/// source over: the union of the viewed month's grid (`viewed`, or `now`'s own
/// month when the widget has not set one) and the forward upcoming window
/// `[now, now + WINDOW_DAYS]`.
///
/// The grid is the full 6-week block the calendar widget shows: it starts at
/// the Monday on/before the first of the month and spans 42 days, with one
/// extra day at the end as a buffer for timezone rounding at the edge. The
/// start reaches back to the grid origin (or today, whichever is earlier) so
/// past events in the viewed month are kept.
///
/// Both grid edges are the first instant of their day ([`local_midnight`]).
/// They were `.earliest()`, which on Unix `Local` is the later of a fold's two
/// midnights, and which a zone that reads a gap strictly answers with `None`,
/// where `now` (start) and the forward end (end) stood in and clipped the
/// day's already-past events off the grid (#1554).
fn scan_window<Tz: TimeZone>(
    now: &DateTime<Tz>,
    viewed: Option<(i32, u32)>,
) -> (DateTime<Tz>, DateTime<Tz>) {
    let tz = now.timezone();
    let today = now.date_naive();
    let forward_end = now.clone() + Duration::days(WINDOW_DAYS);

    let (vy, vm) = viewed.unwrap_or_else(|| (today.year(), today.month()));
    let month_first = NaiveDate::from_ymd_opt(vy, vm, 1).unwrap_or(today);
    let dow_offset = i64::from(month_first.weekday().num_days_from_monday());
    // Grid origin = Monday on/before month_first.
    let grid_origin = month_first - Duration::days(dow_offset);
    // Grid covers 42 cells (6 weeks); end is exclusive (+1 day buffer).
    let grid_end_date = grid_origin + Duration::days(42 + 1);

    let scan_start = local_midnight(grid_origin.min(today), &tz);
    let scan_end = forward_end.max(local_midnight(grid_end_date, &tz));
    (scan_start, scan_end)
}

/// Map one source's expanded instances to [`CalendarEvent`]s, every one of them
/// carrying that source's display name and its `[Calendar] Color=` (#1223
/// item 1). `base_index` is the number of events already collected this scan,
/// so the synthesised UIDs [`instance_to_calendar_event`] mints for
/// UID-less components stay unique across sources.
///
/// Hoisted out of [`Worker::scan_all`]'s loop to give "the colour is read once
/// per source per scan and threaded onto *every* event that source yields" a
/// pure home a test can call: dropping `calendar_color` on the way in fails
/// `every_event_of_a_source_carries_that_sources_colour`, where inside the loop
/// it was green (#1223 review, MED-4).
///
/// What this deliberately does **not** pin is `src.color()` being *called* at
/// all — that needs a seam over `hytte_ecal::Source`, i.e. a live EDS. The
/// scan's own read is covered by the `eds-nixos-test` VM (which seeds one
/// coloured and one uncoloured calendar, so a constant cannot satisfy it) and
/// by the `docs/live-verify.md` entry, not by anything here.
fn source_instances_to_events(
    instances: &[EventInstance],
    calendar_name: &str,
    calendar_color: Option<&str>,
    window_start: DateTime<Local>,
    base_index: usize,
) -> Vec<CalendarEvent> {
    let mut out = Vec::with_capacity(instances.len());
    for inst in instances {
        if let Some(ev) = instance_to_calendar_event(
            inst,
            calendar_name,
            calendar_color,
            window_start,
            base_index + out.len(),
        ) {
            out.push(ev);
        }
    }
    out
}

/// Build a [`CalendarEvent`] from one libecal-expanded [`EventInstance`].
///
/// The **timing** (`start`/`end`/`all_day`) comes from the instance, which
/// libecal computed by applying the recurrence rule + timezone — so a
/// recurring event's third occurrence lands on the right day, not its
/// series origin. The **metadata** (UID, SUMMARY, LOCATION, cancelled
/// status) is parsed out of the component's iCal serialisation, which is
/// identical across a series' instances.
///
/// Returns `None` if the component is cancelled, undatable, or entirely
/// before the scan window start. `window_start` is the beginning of the
/// query window (which can be in the past for the viewed-month range); an
/// event whose `end` is before `window_start` is dropped. An ongoing
/// multi-day event that started before `window_start` but ends after it is
/// kept. `anon_index` disambiguates a synthesised UID for components with
/// no UID of their own. `calendar_color` is the source's own `[Calendar]
/// Color=` if it has one (#1223 item 1) — threaded through unchanged, so a
/// source with no colour yields events with `calendar_color: None` and the UI
/// falls back to its palette.
fn instance_to_calendar_event(
    inst: &EventInstance,
    calendar_name: &str,
    calendar_color: Option<&str>,
    window_start: DateTime<Local>,
    anon_index: usize,
) -> Option<CalendarEvent> {
    let start = unix_to_local(inst.start_unix, inst.all_day)?;
    // A zero/negative-length instance (libecal can hand back end == start
    // for a DATE-TIME with no DTEND) gets a UI-friendly fabricated end so
    // the row isn't a coincident edge — same spirit as the raw-parse path.
    let raw_end = unix_to_local(inst.end_unix, inst.all_day)?;
    let end = if raw_end > start {
        raw_end
    } else if inst.all_day {
        next_day_start(start)
    } else {
        start + Duration::hours(1)
    };

    // Drop only if the event ended before the scan window start.
    // This is always a no-op for a forward-only window (window_start == now,
    // generate_instances won't return events that end before the window
    // start), so the eds-nixos-test probe path is unaffected.
    // For a past-covering window (viewed month), past events within the
    // grid are kept; events before the grid start are already excluded by
    // the generate_instances call itself.
    if end < window_start {
        return None;
    }

    let meta = parse_event_meta(&inst.ical);
    if meta.cancelled {
        return None;
    }

    let uid = meta
        .uid
        .unwrap_or_else(|| format!("anon:{calendar_name}:{anon_index}"));
    // Disambiguate a recurring series' occurrences (which all share one UID)
    // by appending the instance start — keeps per-row identity correct when
    // several instances are in the window.
    let uid = format!("{uid}@{}", inst.start_unix);
    let summary = meta.summary.unwrap_or_else(|| "(no title)".to_string());

    Some(CalendarEvent {
        uid,
        summary,
        start,
        end,
        location: meta.location,
        all_day: inst.all_day,
        calendar_name: calendar_name.to_string(),
        calendar_color: calendar_color.map(str::to_string),
    })
}

/// The first instant of the calendar day `date` in `tz`, i.e. where that day
/// starts.
///
/// - Local 00:00, when that wall-clock time exists once.
/// - The **earlier** of the two instants when 00:00 happens twice, i.e. a
///   fall-back fold at midnight (America/Havana in November). The pair is
///   compared by value, not taken by position: chrono 0.4.44's Unix `Local`
///   hands a fold back as `(later, earlier)`, so `.earliest()` picks the
///   wrong one there (#1490). And only an instant the zone reads back at
///   00:00 or later counts: the same `Local` also answers `Ambiguous` at 00:00
///   on the day after a fall-back at 24:00 (Santiago in April, Cairo and
///   Beirut in October), where one side of the pair is the transition
///   instant, which reads 23:00 the day before.
/// - The first instant the day has when 00:00 is skipped, i.e. a
///   spring-forward gap at midnight (America/Santiago in September,
///   America/Havana in March). That is the transition itself, which reads
///   01:00 on a one-hour gap. If a zone skipped the whole day (Pacific/Apia
///   on 2011-12-30), it is the first instant of the next day, so the skipped
///   day holds nothing.
///
/// Total: it never returns `None` and never panics, for any date and zone.
/// The sites it replaced each lost a day on one of these (#1554): `.single()`
/// is `None` on a fold, which dropped a task's due date or an all-day event;
/// `.earliest()` on Unix `Local` is the later midnight of a fold; and a zone
/// that reads a gap strictly (chrono-tz does) answers `None` to both, where
/// the old fallbacks put `now` in place of the day's start. Unix `Local`
/// itself resolves a gap that begins exactly at 00:00 as `Single` at the
/// pre-gap offset, which is already the transition instant.
#[must_use]
pub fn local_midnight<Tz: TimeZone>(date: NaiveDate, tz: &Tz) -> DateTime<Tz> {
    let midnight = date.and_time(NaiveTime::MIN);
    // A candidate starts the day only if the zone's own UTC-to-local map reads
    // it at `midnight` or later. chrono 0.4.44's Unix `Local` closes a fold at
    // its upper end (`tz_info/timezone.rs:249-250`, `rule.rs:265-266` and
    // `277-278`), so on the day after a fall-back at 24:00 it answers
    // `Ambiguous` at 00:00, and the earlier of that pair reads 23:00 the day
    // before (#1577 review).
    let starts_the_day = |t: &DateTime<Tz>| t.with_timezone(tz).naive_local() >= midnight;
    let start = match tz.from_local_datetime(&midnight) {
        MappedLocalTime::Single(t) if starts_the_day(&t) => t,
        MappedLocalTime::Ambiguous(a, b) => match (starts_the_day(&a), starts_the_day(&b)) {
            (true, true) => a.min(b),
            (true, false) => a,
            (false, true) => b,
            (false, false) => first_instant_reading_at_least(midnight, tz),
        },
        _ => first_instant_reading_at_least(midnight, tz),
    };
    // Re-read the offset from the instant, so `Local`'s gap-start
    // `Single(pre-gap offset)` carries the offset the clock really shows.
    // Without it, a day the zone skipped whole (Pacific/Apia 2011-12-30)
    // keeps that day's date, and `next_day_start` returns the start itself.
    start.with_timezone(tz)
}

/// The first instant whose wall clock in `tz` reads `wall` or later.
///
/// A wall clock only jumps forward across a gap, so a binary search over whole
/// UTC seconds finds the transition exactly. No zone is a day or more off UTC
/// (chrono's `FixedOffset` is bounded by ±24 h), so an instant 25 h before
/// `wall` read as UTC still reads earlier than `wall`, and one 25 h after
/// reads later. Out at the ends of `NaiveDate`'s range, where that window
/// cannot be built, the answer degrades to `wall` read as UTC instead of
/// panicking.
fn first_instant_reading_at_least<Tz: TimeZone>(wall: NaiveDateTime, tz: &Tz) -> DateTime<Tz> {
    const WINDOW: i64 = 25 * 60 * 60;
    let reads_at_least = |secs: i64| {
        DateTime::<Utc>::from_timestamp(secs, 0).is_some_and(|utc| {
            let utc = utc.naive_utc();
            let offset = tz.offset_from_utc_datetime(&utc).fix();
            utc.checked_add_offset(offset)
                .is_none_or(|local| local >= wall)
        })
    };
    let target = wall.and_utc().timestamp();
    let (Some(mut lo), Some(mut hi)) = (target.checked_sub(WINDOW), target.checked_add(WINDOW))
    else {
        return tz.from_utc_datetime(&wall);
    };
    if reads_at_least(lo) || !reads_at_least(hi) {
        return tz.from_utc_datetime(&wall);
    }
    // Invariant: `lo` reads before `wall`, `hi` reads `wall` or later.
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if reads_at_least(mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    DateTime::<Utc>::from_timestamp(hi, 0)
        .map_or_else(|| tz.from_utc_datetime(&wall), |utc| utc.with_timezone(tz))
}

/// The first instant of the calendar day after `dt`'s, by [`local_midnight`].
/// A fixed `+ 24 h` is wrong on a DST transition: from local midnight it
/// lands at 23:00 the same day on a 25-hour fall-back day and at 01:00 on a
/// 23-hour spring-forward one. Not "the same wall-clock time tomorrow"
/// either: an all-day start on a day whose midnight was skipped reads 01:00,
/// and its next day starts at 00:00 (#1554). Only the last representable
/// date, which has no next day, falls back to `+ 24 h`, or to `dt` itself if
/// even that overflows.
fn next_day_start<Tz: TimeZone>(dt: DateTime<Tz>) -> DateTime<Tz> {
    match dt.date_naive().succ_opt() {
        Some(next) => local_midnight(next, &dt.timezone()),
        None => dt
            .clone()
            .checked_add_signed(Duration::hours(24))
            .unwrap_or(dt),
    }
}

/// Convert a POSIX UTC timestamp to local time. For all-day instances we
/// anchor to the start of the same calendar date ([`local_midnight`]):
/// libecal hands back a midnight-UTC `time_t` for DATE values, and
/// reinterpreting it as a local date keeps the dot on the intended day
/// regardless of the viewer's offset.
fn unix_to_local(unix: i64, all_day: bool) -> Option<DateTime<Local>> {
    unix_to_zone(unix, all_day, &Local)
}

/// [`unix_to_local`] in an explicit zone, so the all-day anchor can be tested
/// against a real DST rule without touching the process-wide `TZ`. `None`
/// only for a timestamp outside chrono's range. The all-day arm was
/// `.single()`, which is `None` when the date's midnight happens twice
/// (Havana's November fall-back), and that dropped the event (#1554).
fn unix_to_zone<Tz: TimeZone>(unix: i64, all_day: bool, tz: &Tz) -> Option<DateTime<Tz>> {
    let utc = DateTime::<Utc>::from_timestamp(unix, 0)?;
    if all_day {
        Some(local_midnight(utc.date_naive(), tz))
    } else {
        Some(utc.with_timezone(tz))
    }
}

/// Per-component metadata pulled out of an instance's iCal serialisation —
/// the fields that are constant across a recurring series' occurrences.
struct EventMeta {
    uid: Option<String>,
    summary: Option<String>,
    location: Option<String>,
    cancelled: bool,
}

/// Parse UID / SUMMARY / LOCATION / cancelled-status out of an iCal body.
/// Times are deliberately ignored here — the authoritative per-instance
/// bounds come from the [`EventInstance`], not the embedded (series-origin)
/// DTSTART.
fn parse_event_meta(ical: &str) -> EventMeta {
    let parsed = ical.parse::<Calendar>().ok().or_else(|| {
        let wrapped = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//hytte//\r\n{ical}\r\nEND:VCALENDAR\r\n"
        );
        wrapped.parse::<Calendar>().ok()
    });
    let Some(parsed) = parsed else {
        tracing::warn!("calendar: instance iCal failed to parse; using fallbacks");
        return EventMeta {
            uid: None,
            summary: None,
            location: None,
            cancelled: false,
        };
    };
    let event = parsed.components.iter().find_map(|c| match c {
        CalendarComponent::Event(e) => Some(e),
        _ => None,
    });
    let Some(event) = event else {
        return EventMeta {
            uid: None,
            summary: None,
            location: None,
            cancelled: false,
        };
    };
    EventMeta {
        uid: event.get_uid().map(str::to_string),
        summary: event
            .get_summary()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        location: event
            .get_location()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        cancelled: event.get_status() == Some(EventStatus::Cancelled),
    }
}

fn run_worker(rx: &mpsc::Receiver<()>, writer: &Mutable<Vec<CalendarEvent>>) {
    // Init with retry + backoff instead of going permanently inert: at
    // session bring-up trollshell and evolution-data-server activate
    // concurrently, so the first `Registry::new()` (a blocking D-Bus
    // round-trip) plausibly times out. Refresh pings consumed while backing
    // off are subsumed by the retry itself (#432).
    let mut attempts = 0u32;
    let mut backoff = INIT_BACKOFF_START;
    let mut worker = loop {
        match Worker::new() {
            Ok(w) => break w,
            Err(e) => {
                attempts = attempts.saturating_add(1);
                // First failure at warn for visibility; the rest at debug so
                // a machine where EDS never appears doesn't fill the journal.
                if attempts == 1 {
                    tracing::warn!(error = %e, retry_in_s = backoff.as_secs(), "calendar: EDS worker init failed; retrying");
                } else {
                    tracing::debug!(error = %e, attempts, retry_in_s = backoff.as_secs(), "calendar: EDS worker init failed; retrying");
                }
                if !wait_backoff(rx, backoff, |()| {}) {
                    return; // every sender gone — shutting down
                }
                backoff = next_backoff(backoff);
            }
        }
    };
    if attempts > 0 {
        tracing::info!(
            attempts,
            "calendar: EDS worker init succeeded after retries"
        );
        // Refresh pings were consumed while backing off — populate now
        // rather than waiting out the next poll tick.
        worker.refresh(writer);
    }
    while rx.recv().is_ok() {
        worker.refresh(writer);
    }
}

// ── Helpers used by the page (formatting) ────────────────────────────────────

/// Format an event's start (and optionally end) for an `AdwActionRow`
/// subtitle. Public so the page can use it without re-implementing the
/// rules; format choices match the user-visible spec in the task brief.
#[must_use]
pub fn format_when(event: &CalendarEvent) -> String {
    format_when_on(event, Local::now().date_naive())
}

/// [`format_when`] with "today" supplied, so the labels can be tested
/// without depending on the real date. Everything here is calendar-date
/// arithmetic on [`NaiveDate`]s, never elapsed time, so the length of a DST
/// day cannot shift a label.
fn format_when_on(event: &CalendarEvent, today: NaiveDate) -> String {
    let start_date = event.start.date_naive();
    let end_date = event.end.date_naive();

    if event.all_day {
        // iCal DTEND for DATE-typed events is exclusive (the day *after*
        // the last day). Subtract 1 day to recover the inclusive last-day
        // for display.
        let inclusive_end = end_date - chrono::Duration::days(1);
        if inclusive_end > start_date {
            return format!(
                "All day, {} \u{2192} {}",
                short_date(start_date, today),
                short_date(inclusive_end, today),
            );
        }
        return format!("All day, {}", short_date(start_date, today));
    }

    let start_label = short_date(start_date, today);
    let start_hm = event.start.format("%H:%M");
    if start_date == end_date {
        let end_hm = event.end.format("%H:%M");
        return format!("{start_label} {start_hm}\u{2013}{end_hm}");
    }

    // Multi-day timed event: show both endpoints so a Sat 09:00 → Mon
    // 17:00 conference doesn't lose its Monday.
    let end_label = short_date(end_date, today);
    let end_hm = event.end.format("%H:%M");
    format!("{start_label} {start_hm} \u{2192} {end_label} {end_hm}")
}

// `short_date` (the "Today"/"Tomorrow"/"Mon 14 Apr" formatter) now lives in
// `crate::eds_labels` (#1172) — `tasks.rs`'s `day_label` was a byte-identical
// copy of this same body, imported at the top of this file.

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ev_at(start: DateTime<Local>, end: DateTime<Local>, all_day: bool) -> CalendarEvent {
        CalendarEvent {
            uid: "u".into(),
            summary: "s".into(),
            start,
            end,
            location: None,
            all_day,
            calendar_name: "c".into(),
            calendar_color: None,
        }
    }

    /// Build an `EventInstance` from a UID/summary plus explicit instance
    /// bounds — mirrors what libecal's expansion hands the worker.
    fn inst(
        uid: &str,
        summary: &str,
        start: DateTime<Local>,
        end: DateTime<Local>,
    ) -> EventInstance {
        let ical = format!(
            "BEGIN:VEVENT\r\nUID:{uid}\r\nSUMMARY:{summary}\r\nDTSTART:{}\r\nEND:VEVENT\r\n",
            start.naive_utc().format("%Y%m%dT%H%M%SZ"),
        );
        EventInstance {
            ical: ical.into(),
            start_unix: start.timestamp(),
            end_unix: end.timestamp(),
            all_day: false,
        }
    }

    #[test]
    fn instance_uses_authoritative_times_not_embedded_dtstart() {
        // The instance's embedded DTSTART is the SERIES ORIGIN (2 days ago);
        // the authoritative occurrence is in two days. The CalendarEvent must
        // follow the instance bounds, not the embedded master DTSTART.
        let now = Local::now();
        let series_origin = now - Duration::days(2);
        let occurrence = now + Duration::days(2);
        let ical = format!(
            "BEGIN:VEVENT\r\nUID:daily-1\r\nSUMMARY:Standup\r\nDTSTART:{}\r\n\
             RRULE:FREQ=DAILY\r\nEND:VEVENT\r\n",
            series_origin.naive_utc().format("%Y%m%dT%H%M%SZ"),
        );
        let instance = EventInstance {
            ical: ical.into(),
            start_unix: occurrence.timestamp(),
            end_unix: (occurrence + Duration::hours(1)).timestamp(),
            all_day: false,
        };
        // window_start is now - 1h (simulates a past-covering window)
        let ev =
            instance_to_calendar_event(&instance, "test-cal", None, now - Duration::hours(1), 0)
                .expect("occurrence is in-window");
        assert_eq!(ev.summary, "Standup");
        assert!(ev.uid.starts_with("daily-1@"), "uid was {}", ev.uid);
        // Within a second of the occurrence start, not the series origin.
        assert!((ev.start - occurrence).num_seconds().abs() <= 1);
        assert_eq!(ev.calendar_name, "test-cal");
        // A source with no `[Calendar] Color=` yields no colour — the UI then
        // falls back to its name-hashed palette (#1223 item 1).
        assert_eq!(ev.calendar_color, None);
    }

    /// The source's own colour reaches the event unchanged, and only when the
    /// source has one. Both occurrences of one series carry it, since it is a
    /// property of the source rather than of the instance.
    #[test]
    fn instance_carries_the_sources_colour_when_it_has_one() {
        let now = Local::now();
        let day1 = now + Duration::days(1);
        let day2 = now + Duration::days(2);

        let coloured = instance_to_calendar_event(
            &inst("series", "Daily", day1, day1 + Duration::hours(1)),
            "cal",
            Some("#ff8800"),
            now,
            0,
        )
        .expect("in-window");
        assert_eq!(coloured.calendar_color.as_deref(), Some("#ff8800"));

        let also_coloured = instance_to_calendar_event(
            &inst("series", "Daily", day2, day2 + Duration::hours(1)),
            "cal",
            Some("#ff8800"),
            now,
            1,
        )
        .expect("in-window");
        assert_eq!(also_coloured.calendar_color.as_deref(), Some("#ff8800"));

        // Same instance, colourless source: `None` stays `None` — the whole
        // fallback contract the widget relies on.
        let plain = instance_to_calendar_event(
            &inst("series", "Daily", day1, day1 + Duration::hours(1)),
            "cal",
            None,
            now,
            0,
        )
        .expect("in-window");
        assert_eq!(plain.calendar_color, None);
    }

    /// Every event a source yields in one scan carries **that source's**
    /// colour — the fan-out `scan_all` performs, with the colour read once per
    /// source and threaded onto each instance. Dropping the argument on the way
    /// through was green before this existed (#1223 review, MED-4); the
    /// `base_index` assertion keeps the anon-UID counter honest across sources
    /// at the same time.
    #[test]
    fn every_event_of_a_source_carries_that_sources_colour() {
        let now = Local::now();
        let day1 = now + Duration::days(1);
        let day2 = now + Duration::days(2);
        let instances = vec![
            inst("series", "Daily", day1, day1 + Duration::hours(1)),
            inst("series", "Daily", day2, day2 + Duration::hours(1)),
        ];

        let coloured = source_instances_to_events(&instances, "Work", Some("#ff8800"), now, 0);
        assert_eq!(coloured.len(), 2, "both occurrences are in-window");
        for ev in &coloured {
            assert_eq!(
                ev.calendar_color.as_deref(),
                Some("#ff8800"),
                "every event of a coloured source must carry its colour: {ev:?}"
            );
            assert_eq!(ev.calendar_name, "Work");
        }

        // A source with no `[Calendar] Color=` yields all-`None`, so the widget
        // falls back to its name-hashed palette for the whole calendar.
        let plain = source_instances_to_events(&instances, "Home", None, now, coloured.len());
        assert_eq!(plain.len(), 2);
        assert!(
            plain.iter().all(|e| e.calendar_color.is_none()),
            "a colourless source must yield no colours: {plain:?}"
        );

        // Two UID-less components in one scan must not collide on a synthesised
        // UID — the reason `base_index` is threaded in rather than each call
        // counting from zero. Same calendar name on both, so only `base_index`
        // can tell them apart.
        let anon = vec![EventInstance {
            ical: "BEGIN:VEVENT\r\nSUMMARY:Anon\r\nEND:VEVENT\r\n".into(),
            start_unix: day1.timestamp(),
            end_unix: (day1 + Duration::hours(1)).timestamp(),
            all_day: false,
        }];
        let first = source_instances_to_events(&anon, "Work", None, now, 0);
        let second = source_instances_to_events(&anon, "Work", None, now, 7);
        assert!(
            first[0].uid.starts_with("anon:Work:0@"),
            "uid was {}",
            first[0].uid
        );
        assert_ne!(
            first[0].uid, second[0].uid,
            "a UID-less component must not synthesise the same uid twice in one scan"
        );
    }

    #[test]
    fn instance_recurring_distinct_uids_per_occurrence() {
        // Two occurrences of one series (same UID) must produce DISTINCT
        // CalendarEvent uids so per-row identity holds.
        let now = Local::now();
        let day1 = now + Duration::days(1);
        let day2 = now + Duration::days(2);
        let a = instance_to_calendar_event(
            &inst("series", "Daily", day1, day1 + Duration::hours(1)),
            "cal",
            None,
            now,
            0,
        )
        .unwrap();
        let b = instance_to_calendar_event(
            &inst("series", "Daily", day2, day2 + Duration::hours(1)),
            "cal",
            None,
            now,
            1,
        )
        .unwrap();
        assert_ne!(a.uid, b.uid);
    }

    #[test]
    fn instance_skips_cancelled() {
        let now = Local::now();
        let start = now + Duration::days(1);
        let ical = format!(
            "BEGIN:VEVENT\r\nUID:cx\r\nSUMMARY:Off\r\nSTATUS:CANCELLED\r\nDTSTART:{}\r\nEND:VEVENT\r\n",
            start.naive_utc().format("%Y%m%dT%H%M%SZ"),
        );
        let instance = EventInstance {
            ical: ical.into(),
            start_unix: start.timestamp(),
            end_unix: (start + Duration::hours(1)).timestamp(),
            all_day: false,
        };
        assert!(instance_to_calendar_event(&instance, "cal", None, now, 0).is_none());
    }

    #[test]
    fn instance_past_event_kept_when_window_covers_it() {
        // An occurrence that ended before `now` is KEPT when window_start is
        // set to a past time (i.e. the viewed-month window covers the past).
        // This is the #100 fix: past events in the viewed month must be kept.
        let now = Local::now();
        let start = now - Duration::days(2);
        let end = now - Duration::days(2) + Duration::hours(1);
        // window_start is 3 days ago — the event falls inside the window.
        let window_start = now - Duration::days(3);
        let ev = instance_to_calendar_event(
            &inst("past-visible", "Old", start, end),
            "cal",
            None,
            window_start,
            0,
        );
        assert!(
            ev.is_some(),
            "past event inside viewed-month window must be kept"
        );
    }

    #[test]
    fn instance_past_event_dropped_when_before_window() {
        // An event that ended before window_start is always dropped.
        // In a forward-only window (window_start == now), this is the same
        // behaviour as before — the eds-nixos-test probe path is unaffected.
        let now = Local::now();
        let start = now - Duration::days(5);
        let end = now - Duration::days(5) + Duration::hours(1);
        // window_start == now: event ended 5 days before the window start.
        assert!(
            instance_to_calendar_event(&inst("past", "Old", start, end), "cal", None, now, 0)
                .is_none(),
            "event before window_start must be dropped"
        );
    }

    #[test]
    fn instance_keeps_ongoing_multiday() {
        // Started in the past but still running ⇒ kept (the "hasn't ended"
        // half of the window check).
        let now = Local::now();
        let start = now - Duration::days(1);
        let end = now + Duration::days(1);
        let ev =
            instance_to_calendar_event(&inst("ongoing", "Trip", start, end), "cal", None, now, 0)
                .expect("still running");
        assert_eq!(ev.summary, "Trip");
    }

    #[test]
    fn instance_zero_length_gets_fabricated_end() {
        // libecal can hand back end == start for a DATE-TIME with no DTEND.
        let now = Local::now();
        let start = now + Duration::days(1);
        let ev = instance_to_calendar_event(&inst("z", "Ping", start, start), "cal", None, now, 0)
            .expect("in-window");
        assert_eq!(ev.end - ev.start, Duration::hours(1));
    }

    #[test]
    fn instance_all_day_anchors_local_midnight() {
        let now = Local::now();
        let date = (now + Duration::days(1)).date_naive();
        let midnight_utc = date
            .and_time(NaiveTime::from_hms_opt(0, 0, 0).unwrap())
            .and_utc();
        let instance = EventInstance {
            ical: format!(
                "BEGIN:VEVENT\r\nUID:ad\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:{}\r\nEND:VEVENT\r\n",
                date.format("%Y%m%d"),
            )
            .into(),
            start_unix: midnight_utc.timestamp(),
            end_unix: midnight_utc.timestamp(),
            all_day: true,
        };
        let ev = instance_to_calendar_event(&instance, "cal", None, now, 0).expect("in-window");
        assert!(ev.all_day);
        assert_eq!(ev.start.time(), NaiveTime::from_hms_opt(0, 0, 0).unwrap());
        assert_eq!(ev.start.date_naive(), date);
    }

    #[test]
    fn parse_event_meta_extracts_fields() {
        let ical = "BEGIN:VEVENT\r\nUID:m1\r\nSUMMARY: Lunch \r\nLOCATION: Cafe \r\nEND:VEVENT\r\n";
        let meta = parse_event_meta(ical);
        assert_eq!(meta.uid.as_deref(), Some("m1"));
        assert_eq!(meta.summary.as_deref(), Some("Lunch"));
        assert_eq!(meta.location.as_deref(), Some("Cafe"));
        assert!(!meta.cancelled);
    }

    #[test]
    fn parse_event_meta_handles_bare_vevent_and_missing_fields() {
        // Bare VEVENT (no VCALENDAR wrapper) with no SUMMARY/LOCATION.
        let meta = parse_event_meta("BEGIN:VEVENT\r\nUID:bare\r\nEND:VEVENT\r\n");
        assert_eq!(meta.uid.as_deref(), Some("bare"));
        assert!(meta.summary.is_none());
        assert!(meta.location.is_none());
    }

    #[test]
    fn instance_missing_summary_falls_back_to_no_title() {
        let now = Local::now();
        let start = now + Duration::days(1);
        let instance = EventInstance {
            ical: "BEGIN:VEVENT\r\nUID:nt\r\nEND:VEVENT\r\n".into(),
            start_unix: start.timestamp(),
            end_unix: (start + Duration::hours(1)).timestamp(),
            all_day: false,
        };
        let ev = instance_to_calendar_event(&instance, "cal", None, now, 0).unwrap();
        assert_eq!(ev.summary, "(no title)");
    }

    /// Local wall-clock `h:00` on `date`. Dates, not elapsed time: adding
    /// `Duration::days(n)` to a `DateTime<Local>` is wrong across a DST
    /// transition (the wall-clock time shifts by an hour).
    fn local_at(date: NaiveDate, h: u32) -> DateTime<Local> {
        Local
            .from_local_datetime(&date.and_hms_opt(h, 0, 0).unwrap())
            .earliest()
            .unwrap()
    }

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// The 2026 EU transitions, plus an ordinary day for contrast. On the
    /// fall-back day local midnight + 24 h is 23:00 the *same* date, which
    /// is what this suite's fixture used to build "tomorrow" from.
    const DAYS: [(i32, u32, u32); 3] = [(2026, 3, 29), (2026, 10, 25), (2026, 6, 10)];

    #[test]
    fn format_when_all_day() {
        for (y, m, d) in DAYS {
            let today = ymd(y, m, d);
            let tomorrow = today.succ_opt().unwrap();
            let start = local_at(tomorrow, 0);
            let end = local_at(tomorrow.succ_opt().unwrap(), 0);
            let s = format_when_on(&ev_at(start, end, true), today);
            assert_eq!(s, "All day, Tomorrow", "today = {today}");
        }
    }

    #[test]
    fn format_when_all_day_today() {
        for (y, m, d) in DAYS {
            let today = ymd(y, m, d);
            let start = local_at(today, 0);
            let end = local_at(today.succ_opt().unwrap(), 0);
            let s = format_when_on(&ev_at(start, end, true), today);
            assert_eq!(s, "All day, Today", "today = {today}");
        }
    }

    #[test]
    fn format_when_today_timed() {
        for (y, m, d) in DAYS {
            let today = ymd(y, m, d);
            let event = ev_at(local_at(today, 14), local_at(today, 15), false);
            let s = format_when_on(&event, today);
            assert!(s.starts_with("Today"), "got {s}");
            assert!(s.contains("14:00"), "got {s}");
            assert!(s.contains("15:00"), "got {s}");
        }
    }

    #[test]
    fn format_when_multi_day_timed() {
        // Sat 09:00 -> Mon 17:00 must show both endpoints.
        for (y, m, d) in DAYS {
            let today = ymd(y, m, d);
            let start = local_at(today + chrono::Days::new(2), 9);
            let end = local_at(today + chrono::Days::new(4), 17);
            let s = format_when_on(&ev_at(start, end, false), today);
            assert!(s.contains("09:00"), "got {s}");
            assert!(s.contains("17:00"), "got {s}");
            assert!(s.contains('\u{2192}'), "expected arrow in {s}");
        }
    }

    #[test]
    fn format_when_multi_day_all_day() {
        for (y, m, d) in DAYS {
            let today = ymd(y, m, d);
            let start = local_at(today + chrono::Days::new(1), 0);
            // 3-day span: DTEND is exclusive => start + 3 calendar days.
            let end = local_at(today + chrono::Days::new(4), 0);
            let s = format_when_on(&ev_at(start, end, true), today);
            assert!(s.starts_with("All day, Tomorrow"), "got {s}");
            assert!(s.contains('\u{2192}'), "expected arrow in {s}");
        }
    }

    #[test]
    fn next_day_start_is_the_next_calendar_date() {
        // Only bites when the host zone observes DST (e.g. Europe/Berlin);
        // under UTC it still pins the contract.
        for (y, m, d) in DAYS {
            let today = ymd(y, m, d);
            let next = next_day_start(local_at(today, 0));
            assert_eq!(next.date_naive(), today.succ_opt().unwrap());
            assert_eq!(next.time(), NaiveTime::from_hms_opt(0, 0, 0).unwrap());
        }
    }

    #[test]
    fn all_day_instance_without_end_spans_one_calendar_day() {
        // libecal can hand back end == start; the fabricated end must be the
        // next local midnight even when that day is 23 or 25 hours long.
        for (y, m, d) in DAYS {
            let day = ymd(y, m, d);
            let ts = day.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp();
            let instance = EventInstance {
                ical: "BEGIN:VEVENT\r\nUID:x\r\nSUMMARY:s\r\nEND:VEVENT\r\n".into(),
                start_unix: ts,
                end_unix: ts,
                all_day: true,
            };
            let before = local_at(day, 0) - Duration::days(1);
            let ev =
                instance_to_calendar_event(&instance, "c", None, before, 0).expect("event kept");
            assert_eq!(ev.end.date_naive(), day.succ_opt().unwrap());
        }
    }
}

/// #1554: where a day starts when the zone skips or doubles its 00:00, against
/// real IANA rules (chrono-tz's bundled tz database). An explicit zone is what
/// makes these bite on a UTC CI host; the process-wide `TZ` cannot be set per
/// test without racing the parallel suite (#1472).
#[cfg(test)]
mod local_midnight_tests {
    use super::*;
    use chrono::FixedOffset;
    use chrono_tz::America::{Asuncion, Havana, Santiago, Sao_Paulo};
    use chrono_tz::Asia::Beirut;
    use chrono_tz::{Tz, TzOffset};

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn utc(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    /// A `chrono_tz` zone that hands a fold back the way chrono 0.4.44's Unix
    /// `Local` does: ordered by offset, which for a fall-back fold is
    /// `(later, earlier)` (#1490). Production runs on `Local`, so this is the
    /// order the helper meets there. Plain `chrono_tz` answers
    /// `(earlier, later)`, where `.earliest()` and comparing by value agree,
    /// so it alone could not tell the two apart.
    ///
    /// It models that order and nothing else. In particular it does **not**
    /// model where `Local` puts a fold's upper bound: `Local` closes a fold at
    /// its upper end, so on the day after a fall-back at 24:00 (Santiago in
    /// April, Cairo and Beirut in October) it answers `Ambiguous` at 00:00,
    /// where chrono-tz, and so this wrapper, answer `Single`. A helper that
    /// passes every `LocalOrder` sweep below can still be wrong on `Local`
    /// there; it was, until the #1577 review. `TzInfoFallBack` and
    /// `local_midnight_local_tests` cover that case.
    #[derive(Clone, Copy, Debug)]
    struct LocalOrder(Tz);

    fn by_offset(r: MappedLocalTime<TzOffset>) -> MappedLocalTime<TzOffset> {
        match r {
            MappedLocalTime::Ambiguous(a, b)
                if a.fix().local_minus_utc() > b.fix().local_minus_utc() =>
            {
                MappedLocalTime::Ambiguous(b, a)
            }
            other => other,
        }
    }

    impl TimeZone for LocalOrder {
        type Offset = TzOffset;

        fn from_offset(offset: &TzOffset) -> Self {
            Self(Tz::from_offset(offset))
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> MappedLocalTime<TzOffset> {
            by_offset(self.0.offset_from_local_date(local))
        }

        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> MappedLocalTime<TzOffset> {
            by_offset(self.0.offset_from_local_datetime(local))
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> TzOffset {
            self.0.offset_from_utc_date(utc)
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> TzOffset {
            self.0.offset_from_utc_datetime(utc)
        }
    }

    /// A hermetic zone with one fall-back, at the UTC instant `at`, from
    /// `before` to `after` seconds east (`before > after`), whose local lookup
    /// transcribes chrono 0.4.44's Unix `Local` for a backward transition
    /// (`tz_info/timezone.rs:243-257`): a wall clock below the fold is `before`,
    /// one from the fold's lower end **up to and including** its upper end is
    /// `Ambiguous(after, before)`, and one above it is `after`. chrono-tz
    /// excludes the upper end, so only this zone, not `LocalOrder`, shows the
    /// spurious fold `Local` reports at 00:00 after a fall-back at 24:00.
    #[derive(Clone, Copy, Debug)]
    struct TzInfoFallBack {
        at: i64,
        before: i32,
        after: i32,
    }

    /// The offset a [`TzInfoFallBack`] hands chrono. It carries its zone
    /// because chrono rebuilds a `TimeZone` from an offset alone.
    #[derive(Clone, Copy, Debug)]
    struct TzInfoOffset {
        zone: TzInfoFallBack,
        fix: FixedOffset,
    }

    impl Offset for TzInfoOffset {
        fn fix(&self) -> FixedOffset {
            self.fix
        }
    }

    impl TzInfoFallBack {
        fn offset(self, secs_east: i32) -> TzInfoOffset {
            TzInfoOffset {
                zone: self,
                fix: FixedOffset::east_opt(secs_east).unwrap(),
            }
        }
    }

    impl TimeZone for TzInfoFallBack {
        type Offset = TzInfoOffset;

        fn from_offset(offset: &TzInfoOffset) -> Self {
            offset.zone
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> MappedLocalTime<TzInfoOffset> {
            self.offset_from_local_datetime(&local.and_time(NaiveTime::MIN))
        }

        fn offset_from_local_datetime(
            &self,
            local: &NaiveDateTime,
        ) -> MappedLocalTime<TzInfoOffset> {
            let wall = local.and_utc().timestamp();
            // `transition_start`/`transition_end` in chrono's own naming: the
            // wall clock at `at` on the old offset, and on the new one.
            let upper = self.at + i64::from(self.before);
            let lower = self.at + i64::from(self.after);
            if wall < lower {
                MappedLocalTime::Single(self.offset(self.before))
            } else if wall <= upper {
                MappedLocalTime::Ambiguous(self.offset(self.after), self.offset(self.before))
            } else {
                MappedLocalTime::Single(self.offset(self.after))
            }
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> TzInfoOffset {
            self.offset_from_utc_datetime(&utc.and_time(NaiveTime::MIN))
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> TzInfoOffset {
            if utc.and_utc().timestamp() < self.at {
                self.offset(self.before)
            } else {
                self.offset(self.after)
            }
        }
    }

    /// The day after a fall-back at 24:00 starts at its own 00:00, not at the
    /// transition, on a zone with `Local`'s fold bounds (#1577 review). Modelled
    /// on Santiago 2026: Saturday 04-04 24:00 -03 (2026-04-05T03:00Z) becomes
    /// 23:00 -04, so Sunday 00:00 happens once, at 04:00Z.
    #[test]
    fn a_fall_back_at_24_00_starts_the_next_day_at_its_own_midnight() {
        let zone = TzInfoFallBack {
            at: utc(2026, 4, 5, 3).timestamp(),
            before: -3 * 3600,
            after: -4 * 3600,
        };
        let day = ymd(2026, 4, 5);
        let midnight = day.and_time(NaiveTime::MIN);
        let MappedLocalTime::Ambiguous(a, b) = zone.from_local_datetime(&midnight) else {
            panic!("this zone must answer the spurious fold `Local` does, or nothing is tested");
        };
        assert_eq!(
            a.min(b),
            utc(2026, 4, 5, 3),
            "the earlier side is the transition"
        );
        assert_eq!(
            utc(2026, 4, 5, 3).with_timezone(&zone).naive_local(),
            ymd(2026, 4, 4).and_hms_opt(23, 0, 0).unwrap(),
            "and the clock reads 23:00 the day before there"
        );

        let start = local_midnight(day, &zone);
        assert_eq!(start, utc(2026, 4, 5, 4));
        assert_first_instant_of(&start, day, "tz_info fall-back at 24:00");
        // The sites that go through it.
        assert_eq!(
            next_day_start(local_midnight(ymd(2026, 4, 4), &zone)),
            utc(2026, 4, 5, 4)
        );
        let at = day.and_time(NaiveTime::MIN).and_utc().timestamp();
        assert_eq!(
            unix_to_zone(at, true, &zone).map(|t| t.with_timezone(&Utc)),
            Some(utc(2026, 4, 5, 4))
        );
        // Viewing May, whose grid starts on Monday 2026-04-27, the scan
        // reaches back to today.
        let now = zone.with_ymd_and_hms(2026, 4, 5, 15, 0, 0).unwrap();
        let (scan_start, _) = scan_window(&now, Some((2026, 5)));
        assert_eq!(scan_start, utc(2026, 4, 5, 4));
    }

    /// One midnight gap per zone #1554 names, each checked against the bundled
    /// tz data by the first assertion of the test below.
    const MIDNIGHT_GAPS: [(Tz, (i32, u32, u32)); 5] = [
        // Brazil's last spring-forward; Brazil dropped DST in 2019.
        (Sao_Paulo, (2018, 11, 4)),
        (Havana, (2026, 3, 8)),
        (Beirut, (2026, 3, 29)),
        // Paraguay's last; it has been on permanent -03 since 2024-10-15.
        (Asuncion, (2024, 10, 6)),
        (Santiago, (2026, 9, 6)),
    ];

    /// `start` is the first instant of `day` in `start`'s zone: it reads `day`
    /// or later, and one second earlier still reads an earlier day.
    fn assert_first_instant_of<Z: TimeZone>(start: &DateTime<Z>, day: NaiveDate, what: &str) {
        let midnight = day.and_time(NaiveTime::MIN);
        assert!(
            start.naive_local() >= midnight,
            "{what} {day}: the start reads {}, before the day",
            start.naive_local()
        );
        let before = start.clone() - Duration::seconds(1);
        assert!(
            before.naive_local() < midnight,
            "{what} {day}: one second before the start already reads {}, so the day began earlier",
            before.naive_local()
        );
    }

    #[test]
    fn a_skipped_midnight_starts_the_day_at_the_transition() {
        for (tz, (y, m, d)) in MIDNIGHT_GAPS {
            let day = ymd(y, m, d);
            assert!(
                matches!(
                    tz.from_local_datetime(&day.and_time(NaiveTime::MIN)),
                    MappedLocalTime::None
                ),
                "{tz} {day}: 00:00 must fall in a gap for this case to test anything"
            );
            let start = local_midnight(day, &tz);
            // A one-hour gap at midnight: the day's first instant reads 01:00.
            assert_eq!(
                start.naive_local(),
                day.and_hms_opt(1, 0, 0).unwrap(),
                "{tz} {day}"
            );
            assert_first_instant_of(&start, day, &tz.to_string());
        }
    }

    #[test]
    fn a_doubled_midnight_starts_the_day_at_the_earlier_one() {
        // Havana falls back from 01:00 CDT to 00:00 CST on 2026-11-01, so
        // 00:00 happens at 04:00Z (CDT) and again at 05:00Z (CST).
        let day = ymd(2026, 11, 1);
        let midnight = day.and_time(NaiveTime::MIN);
        assert!(
            matches!(
                Havana.from_local_datetime(&midnight),
                MappedLocalTime::Ambiguous(..)
            ),
            "Havana's 2026-11-01 00:00 must be a fold for this case to test anything"
        );
        let local_order = LocalOrder(Havana);
        assert_eq!(
            local_order
                .from_local_datetime(&midnight)
                .earliest()
                .map(|t| t.with_timezone(&Utc)),
            Some(utc(2026, 11, 1, 5)),
            "the test zone must hand the fold back (later, earlier), as Unix `Local` does"
        );
        assert_eq!(local_midnight(day, &Havana), utc(2026, 11, 1, 4));
        assert_eq!(local_midnight(day, &local_order), utc(2026, 11, 1, 4));
    }

    #[test]
    fn an_ordinary_midnight_is_local_00_00() {
        let day = ymd(2026, 6, 10);
        // Both zones are on -04 in June.
        assert_eq!(local_midnight(day, &Santiago), utc(2026, 6, 10, 4));
        assert_eq!(local_midnight(day, &Havana), utc(2026, 6, 10, 4));
        assert_eq!(local_midnight(day, &Utc), utc(2026, 6, 10, 0));
        let ist = FixedOffset::east_opt(5 * 3600 + 1800).unwrap();
        assert_eq!(
            local_midnight(day, &ist),
            utc(2026, 6, 9, 18) + Duration::minutes(30)
        );
    }

    #[test]
    fn a_skipped_day_holds_nothing() {
        // Samoa skipped 2011-12-30 entirely: 2011-12-29 24:00 (-10) became
        // 2011-12-31 00:00 (+14) at 2011-12-30 10:00Z.
        let apia = chrono_tz::Pacific::Apia;
        let skipped = local_midnight(ymd(2011, 12, 30), &apia);
        assert_eq!(skipped, utc(2011, 12, 30, 10));
        assert_eq!(skipped, local_midnight(ymd(2011, 12, 31), &apia));
        assert_first_instant_of(&skipped, ymd(2011, 12, 30), "Pacific/Apia");
    }

    /// The definition itself, for every day of three years, in zones with a
    /// midnight gap, a midnight fold, a gap and a fold away from midnight and a
    /// 30-minute shift, each also in the Unix-`Local` fold order.
    #[test]
    fn every_day_starts_at_its_first_instant() {
        fn sweep<Z: TimeZone>(tz: &Z, what: &str) {
            let mut day = ymd(2024, 1, 1);
            while day < ymd(2027, 1, 1) {
                assert_first_instant_of(&local_midnight(day, tz), day, what);
                day = day.succ_opt().unwrap();
            }
        }
        for tz in [
            Santiago,
            Havana,
            Beirut,
            Asuncion,
            chrono_tz::Europe::Berlin,
            chrono_tz::Australia::Lord_Howe,
        ] {
            sweep(&tz, &tz.to_string());
            sweep(&LocalOrder(tz), &format!("{tz} in Local's fold order"));
        }
    }

    /// Never `None`, never a panic, even where the search window around
    /// midnight runs off the end of chrono's range.
    #[test]
    fn local_midnight_is_total_at_the_ends_of_the_calendar() {
        for day in [NaiveDate::MIN, NaiveDate::MAX] {
            let _ = local_midnight(day, &Utc);
            let _ = local_midnight(day, &FixedOffset::east_opt(86_399).unwrap());
            let _ = local_midnight(day, &FixedOffset::west_opt(86_399).unwrap());
            let _ = local_midnight(day, &Santiago);
            let _ = local_midnight(day, &chrono_tz::Asia::Tokyo);
            let _ = local_midnight(day, &LocalOrder(Havana));
        }
    }

    #[test]
    fn next_day_start_lands_on_the_next_days_first_instant() {
        // Out of a day that began at 01:00 because its midnight was skipped:
        // the next day still starts at its own 00:00, not at 01:00.
        let gap_day = local_midnight(ymd(2026, 9, 6), &Santiago);
        let next = next_day_start(gap_day);
        assert_eq!(next.naive_local(), ymd(2026, 9, 7).and_time(NaiveTime::MIN));
        // Into a skipped midnight, and into a doubled one in Local's order.
        assert_eq!(
            next_day_start(local_midnight(ymd(2026, 9, 5), &Santiago)),
            utc(2026, 9, 6, 4)
        );
        let havana = LocalOrder(Havana);
        assert_eq!(
            next_day_start(local_midnight(ymd(2026, 10, 31), &havana)),
            utc(2026, 11, 1, 4)
        );
    }

    #[test]
    fn an_all_day_instance_on_a_skipped_or_doubled_midnight_is_kept() {
        // libecal hands a DATE back as that date's midnight UTC.
        let at = |day: NaiveDate| day.and_time(NaiveTime::MIN).and_utc().timestamp();
        assert_eq!(
            unix_to_zone(at(ymd(2026, 9, 6)), true, &Santiago).map(|t| t.with_timezone(&Utc)),
            Some(utc(2026, 9, 6, 4))
        );
        assert_eq!(
            unix_to_zone(at(ymd(2026, 11, 1)), true, &LocalOrder(Havana))
                .map(|t| t.with_timezone(&Utc)),
            Some(utc(2026, 11, 1, 4))
        );
        // A timed instant is the instant, whatever the day.
        let t = utc(2026, 9, 6, 4).timestamp() - 1;
        assert_eq!(
            unix_to_zone(t, false, &Santiago).map(|t| t.timestamp()),
            Some(t)
        );
    }

    #[test]
    fn the_scan_window_starts_a_skipped_midnight_at_the_transition() {
        // Sunday 2026-09-06 skips 00:00 in Santiago. Viewing October, whose
        // grid starts on Monday 2026-09-28, the scan reaches back to today.
        let now = Santiago.with_ymd_and_hms(2026, 9, 6, 15, 0, 0).unwrap();
        let (start, _) = scan_window(&now, Some((2026, 10)));
        assert_eq!(start, utc(2026, 9, 6, 4));
    }

    #[test]
    fn the_scan_window_starts_a_doubled_midnight_at_the_earlier_one() {
        // Sunday 2026-11-01 has 00:00 twice in Havana. Viewing December, whose
        // grid starts on Monday 2026-11-30, the scan reaches back to today.
        let havana = LocalOrder(Havana);
        let now = havana.with_ymd_and_hms(2026, 11, 1, 15, 0, 0).unwrap();
        let (start, _) = scan_window(&now, Some((2026, 12)));
        assert_eq!(start, utc(2026, 11, 1, 4));
    }

    #[test]
    fn the_scan_window_spans_the_viewed_grid_and_the_forward_window() {
        // June 2026 starts on a Monday, so its grid is 06-01 .. 07-14 (43 days).
        let now = Santiago.with_ymd_and_hms(2026, 6, 10, 12, 0, 0).unwrap();
        let (start, end) = scan_window(&now, None);
        assert_eq!(start, utc(2026, 6, 1, 4));
        assert_eq!(end, utc(2026, 7, 14, 4));
        // A grid that ends before the forward window: the forward end wins.
        let (_, end) = scan_window(&now, Some((2026, 5)));
        assert_eq!(end, now + Duration::days(WINDOW_DAYS));
    }
}

/// #1577 review: chrono 0.4.44's Unix `Local` closes a fold at its upper end
/// (`offset/local/tz_info/timezone.rs:249-250`, `rule.rs:265-266` and
/// `277-278`), so a zone that falls back at 24:00 answers `Ambiguous` at
/// 00:00 the next day, and the earlier instant of that pair is the transition
/// itself, which reads 23:00 the day before. chrono-tz, and `LocalOrder` over
/// it, answer `Single` there, so only the real `Local` shows it. This re-execs
/// the test binary with a POSIX `TZ` string, which chrono parses itself, so no
/// tzdata file is needed and the parallel suite's `TZ` is never touched.
#[cfg(test)]
mod local_midnight_local_tests {
    use super::*;

    const CHILD: &str = "HYTTE_1577_LOCAL_MIDNIGHT_CHILD";
    const CHILD_OK: &str = "hytte-1577-local-midnight-child-reached-the-end";

    #[test]
    fn a_day_after_a_fall_back_at_24_00_starts_at_its_own_midnight_in_local() {
        for (tz, day, want) in [
            // Santiago: Saturday 2026-04-04 24:00 -03 becomes 23:00 -04.
            (
                "CLT4CLST,M9.1.6/24,M4.1.6/24",
                "2026-04-05",
                "2026-04-05T04:00:00+00:00",
            ),
            // Cairo: Thursday 2026-10-29 24:00 +03 becomes 23:00 +02.
            (
                "EET-2EEST,M4.5.5/0,M10.5.4/24",
                "2026-10-30",
                "2026-10-29T22:00:00+00:00",
            ),
            // Beirut: Saturday 2026-10-24 24:00 +03 becomes 23:00 +02.
            (
                "EET-2EEST,M3.5.0/0,M10.5.0/0",
                "2026-10-25",
                "2026-10-24T22:00:00+00:00",
            ),
            // Santiago's gap at 00:00, which this `Local` answers `None`.
            (
                "CLT4CLST,M9.1.6/24,M4.1.6/24",
                "2026-09-06",
                "2026-09-06T04:00:00+00:00",
            ),
        ] {
            let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "--nocapture",
                    "--test-threads=1",
                    "calendar::local_midnight_local_tests::local_midnight_child",
                ])
                .env("TZ", tz)
                .env(CHILD, format!("{day} {want}"))
                .output()
                .expect("re-exec the test binary");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && stdout.contains(CHILD_OK),
                "TZ={tz} {day}:\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// The child half of the test above; a no-op outside its child.
    #[test]
    fn local_midnight_child() {
        let Ok(arg) = std::env::var(CHILD) else {
            return;
        };
        let (day, want) = arg.split_once(' ').expect("`<day> <want>`");
        let day: NaiveDate = day.parse().expect("a date");
        let start = local_midnight(day, &Local);
        assert_eq!(start.with_timezone(&Utc).to_rfc3339(), want, "{day}");
        // Read back through the zone's own UTC-to-local map, not the offset
        // the result happens to carry.
        let midnight = day.and_time(NaiveTime::MIN);
        assert!(
            start.with_timezone(&Local).naive_local() >= midnight,
            "{day}: reads the day before"
        );
        assert!(
            (start - Duration::seconds(1))
                .with_timezone(&Local)
                .naive_local()
                < midnight,
            "{day}: the day began earlier"
        );
        println!("{CHILD_OK}");
    }
}
