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
//! years out. (RDATE/EXDATE refinement is a `hytte-ecal` follow-up.)
//!
//! # Limitations / notes
//!
//! - Timezones: instance bounds come back as absolute UTC seconds (libical
//!   anchors the recurrence to UTC during expansion), so the old per-field
//!   `WithTimezone`/chrono-tz fallback no longer gates recurring events.
//!   All-day instances anchor to local midnight.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, mpsc};
use std::thread;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone, Utc};
use futures_signals::signal::{Mutable, Signal};
use hytte_ecal::sys::ECalClientSourceType;
use hytte_ecal::{CalClient, EventInstance, Registry, Source};
use hytte_reactive::{Service, registry};
use icalendar::{Calendar, CalendarComponent, Component, EventLike, EventStatus};

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

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = CalendarHandles::default();
        let writer = handles.events.clone();

        // Channel: tokio ticker + public refresh() → dedicated EDS thread.
        let (tx, rx) = mpsc::channel::<()>();
        let _ = SENDER.set(tx);

        // libecal isn't Sync and wants to be pinned to one thread (the same
        // constraint the tasks service handles); give the calendar its own
        // EDS worker thread owning a Registry + a per-source CalClient cache.
        thread::Builder::new()
            .name("hytte-eds-cal".into())
            .spawn(move || run_worker(&rx, &writer))
            .expect("spawn calendar EDS worker thread");

        // Refresh ticker. The first send fires immediately (initial
        // populate); thereafter every POLL_INTERVAL.
        rt.spawn(async {
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
}

impl Worker {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            registry: Registry::new()?,
            clients: HashMap::new(),
        })
    }

    /// Open (lazily, cached) the Events [`CalClient`] for `source_uid`.
    /// 5 s connect budget — same as the task service; bumped above the
    /// libecal default so CalDAV/Google backends have time to come online.
    fn client(&mut self, source_uid: &str) -> anyhow::Result<&CalClient> {
        if !self.clients.contains_key(source_uid) {
            let src = self.lookup_source(source_uid)?;
            let client = CalClient::connect(&src, ECalClientSourceType::Events, 5)?;
            self.clients.insert(source_uid.to_string(), client);
        }
        Ok(self
            .clients
            .get(source_uid)
            .expect("just inserted; lookup can't miss"))
    }

    fn lookup_source(&self, source_uid: &str) -> anyhow::Result<Source> {
        match self.registry.ref_source(source_uid)? {
            Some(s) => Ok(s),
            None => anyhow::bail!("EDS source '{source_uid}' not found"),
        }
    }

    /// Re-scan every calendar source and emit a fresh `Vec` only if it
    /// differs from the current snapshot (`PartialEq` dedup avoids
    /// re-emitting an identical list every minute).
    fn refresh(&mut self, writer: &Mutable<Vec<CalendarEvent>>) {
        let snapshot = self.scan_all();
        let changed = {
            let cur = writer.lock_ref();
            *cur != snapshot
        };
        if changed {
            writer.set(snapshot);
        }
    }

    fn scan_all(&mut self) -> Vec<CalendarEvent> {
        let now = Local::now();
        let today = now.date_naive();

        // ── Compute query window = union(viewed-month grid, forward window) ───
        //
        // Forward upcoming window: [now, now + WINDOW_DAYS].
        let forward_end = now + Duration::days(WINDOW_DAYS);

        // Viewed-month window: the full 6-week grid that the calendar widget
        // shows. The grid starts at the Monday on/before the first of the month
        // and spans 42 days (6 weeks). We extend one extra day on each side as
        // a buffer for timezone rounding at the edges.
        let viewed_ym = VIEWED_MONTH
            .lock()
            .expect("calendar VIEWED_MONTH lock poisoned")
            .unwrap_or_else(|| (today.year(), today.month()));
        let (vy, vm) = viewed_ym;
        let month_first = NaiveDate::from_ymd_opt(vy, vm, 1).unwrap_or(today);
        let dow_offset = i64::from(month_first.weekday().num_days_from_monday());
        // Grid origin = Monday on/before month_first
        let grid_origin = month_first - Duration::days(dow_offset);
        // Grid covers 42 cells (6 weeks); end is exclusive (+1 day buffer).
        let grid_end_date = grid_origin + Duration::days(42 + 1);

        // Union: earliest start = min(grid_origin, now); latest end = max(grid_end_date, forward_end).
        // We want to include past events in the viewed month, so the scan start
        // can be before now. We never go more than ~6 weeks back.
        let scan_start_date = grid_origin.min(today);
        let scan_start = Local
            .from_local_datetime(&scan_start_date.and_hms_opt(0, 0, 0).unwrap_or_default())
            .earliest()
            .unwrap_or(now);

        // Scan end = max(forward window, grid end). Convert grid_end_date to a
        // DateTime for comparison.
        let grid_end_dt = Local
            .from_local_datetime(&grid_end_date.and_hms_opt(0, 0, 0).unwrap_or_default())
            .earliest()
            .unwrap_or(forward_end);
        let scan_end = forward_end.max(grid_end_dt);

        let start_unix = scan_start.timestamp();
        let end_unix = scan_end.timestamp();

        // Re-read the source list each pass so a calendar added/removed at
        // runtime (e.g. a new Nextcloud calendar discovered under the
        // collection source) is picked up without a restart.
        let sources = self.registry.calendars();
        let mut out: Vec<CalendarEvent> = Vec::new();
        for src in sources {
            let source_uid = src.uid();
            let calendar_name = src.display_name();
            let client = match self.client(&source_uid) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(source = %source_uid, error = %e, "calendar: client connect failed");
                    continue;
                }
            };
            // Ask libecal to EXPAND every component over the window: each
            // recurring event yields one instance per occurrence inside
            // [start_unix, end_unix), with authoritative per-instance
            // start/end (RRULE/RDATE applied, EXDATE removed). Non-recurring
            // events come back as a single instance. This replaces the old
            // `get_object_strings("#t")` master-only path — see the
            // module-level "Recurrence expansion" note (#29).
            let instances = match client.generate_instances(start_unix, end_unix) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(source = %source_uid, error = %e, "calendar: instance expansion failed");
                    continue;
                }
            };
            for inst in instances {
                if let Some(ev) =
                    instance_to_calendar_event(&inst, &calendar_name, scan_start, out.len())
                {
                    out.push(ev);
                }
            }
        }
        out.sort_by_key(|e| e.start);
        out
    }
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
/// no UID of their own.
fn instance_to_calendar_event(
    inst: &EventInstance,
    calendar_name: &str,
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
        start + Duration::days(1)
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
    // by appending the instance start — keeps PartialEq dedup + per-row
    // identity correct when several instances are in the window.
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
    })
}

/// Convert a POSIX UTC timestamp to local time. For all-day instances we
/// anchor to local midnight on the same calendar date (libecal hands back a
/// midnight-UTC `time_t` for DATE values; reinterpreting it as a local date
/// keeps the dot on the intended day regardless of the viewer's offset).
fn unix_to_local(unix: i64, all_day: bool) -> Option<DateTime<Local>> {
    let utc = DateTime::<Utc>::from_timestamp(unix, 0)?;
    if all_day {
        let date = utc.date_naive();
        let naive = date.and_time(NaiveTime::from_hms_opt(0, 0, 0)?);
        Local.from_local_datetime(&naive).single()
    } else {
        Some(utc.with_timezone(&Local))
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
    let mut worker = match Worker::new() {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(error = %e, "calendar: EDS worker init failed; service inert");
            // Drain the channel so the ticker's sends don't error out; we
            // can't recover without a restart.
            for () in rx {}
            return;
        }
    };
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
    let now = Local::now();
    let today = now.date_naive();
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

/// Render a date as one of "Today", "Tomorrow", or `"Mon 14 Apr"` relative
/// to `today`. Used for both all-day and timed events.
fn short_date(d: NaiveDate, today: NaiveDate) -> String {
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
            ical,
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
            ical,
            start_unix: occurrence.timestamp(),
            end_unix: (occurrence + Duration::hours(1)).timestamp(),
            all_day: false,
        };
        // window_start is now - 1h (simulates a past-covering window)
        let ev = instance_to_calendar_event(&instance, "test-cal", now - Duration::hours(1), 0)
            .expect("occurrence is in-window");
        assert_eq!(ev.summary, "Standup");
        assert!(ev.uid.starts_with("daily-1@"), "uid was {}", ev.uid);
        // Within a second of the occurrence start, not the series origin.
        assert!((ev.start - occurrence).num_seconds().abs() <= 1);
        assert_eq!(ev.calendar_name, "test-cal");
    }

    #[test]
    fn instance_recurring_distinct_uids_per_occurrence() {
        // Two occurrences of one series (same UID) must produce DISTINCT
        // CalendarEvent uids so PartialEq dedup + per-row identity hold.
        let now = Local::now();
        let day1 = now + Duration::days(1);
        let day2 = now + Duration::days(2);
        let a = instance_to_calendar_event(
            &inst("series", "Daily", day1, day1 + Duration::hours(1)),
            "cal",
            now,
            0,
        )
        .unwrap();
        let b = instance_to_calendar_event(
            &inst("series", "Daily", day2, day2 + Duration::hours(1)),
            "cal",
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
            ical,
            start_unix: start.timestamp(),
            end_unix: (start + Duration::hours(1)).timestamp(),
            all_day: false,
        };
        assert!(instance_to_calendar_event(&instance, "cal", now, 0).is_none());
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
            instance_to_calendar_event(&inst("past", "Old", start, end), "cal", now, 0).is_none(),
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
        let ev = instance_to_calendar_event(&inst("ongoing", "Trip", start, end), "cal", now, 0)
            .expect("still running");
        assert_eq!(ev.summary, "Trip");
    }

    #[test]
    fn instance_zero_length_gets_fabricated_end() {
        // libecal can hand back end == start for a DATE-TIME with no DTEND.
        let now = Local::now();
        let start = now + Duration::days(1);
        let ev = instance_to_calendar_event(&inst("z", "Ping", start, start), "cal", now, 0)
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
            ),
            start_unix: midnight_utc.timestamp(),
            end_unix: midnight_utc.timestamp(),
            all_day: true,
        };
        let ev = instance_to_calendar_event(&instance, "cal", now, 0).expect("in-window");
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
            ical: "BEGIN:VEVENT\r\nUID:nt\r\nEND:VEVENT\r\n".to_string(),
            start_unix: start.timestamp(),
            end_unix: (start + Duration::hours(1)).timestamp(),
            all_day: false,
        };
        let ev = instance_to_calendar_event(&instance, "cal", now, 0).unwrap();
        assert_eq!(ev.summary, "(no title)");
    }

    #[test]
    fn format_when_today_timed() {
        let now = Local::now();
        let start = Local
            .with_ymd_and_hms(now.year(), now.month(), now.day(), 14, 0, 0)
            .single()
            .unwrap();
        let end = start + Duration::hours(1);
        let s = format_when(&ev_at(start, end, false));
        assert!(s.starts_with("Today"), "got {s}");
        assert!(s.contains("14:00"));
        assert!(s.contains("15:00"));
    }

    #[test]
    fn format_when_all_day() {
        let now = Local::now();
        let start = Local
            .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
            .single()
            .unwrap()
            + Duration::days(1);
        let end = start + Duration::days(1);
        let s = format_when(&ev_at(start, end, true));
        assert_eq!(s, "All day, Tomorrow");
    }

    #[test]
    fn format_when_multi_day_timed() {
        // Sat 09:00 → Mon 17:00 must show both endpoints.
        let now = Local::now();
        let start = Local
            .with_ymd_and_hms(now.year(), now.month(), now.day(), 9, 0, 0)
            .single()
            .unwrap()
            + Duration::days(2);
        let end = start + Duration::days(2) + Duration::hours(8);
        let s = format_when(&ev_at(start, end, false));
        assert!(s.contains("09:00"), "got {s}");
        assert!(s.contains("17:00"), "got {s}");
        assert!(s.contains('\u{2192}'), "expected arrow in {s}");
    }

    #[test]
    fn format_when_multi_day_all_day() {
        let now = Local::now();
        let start = Local
            .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
            .single()
            .unwrap()
            + Duration::days(1);
        // 3-day span: DTEND is exclusive ⇒ start + 3 days.
        let end = start + Duration::days(3);
        let s = format_when(&ev_at(start, end, true));
        assert!(s.starts_with("All day, "), "got {s}");
        assert!(s.contains('\u{2192}'), "expected arrow in {s}");
    }
}
