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
//! by start time, filtered to events that haven't ended and start within
//! [`NEXT_DAYS`] days. [`refresh()`] enqueues an out-of-cycle re-scan onto
//! the worker. The public surface is read-only and identical to the prior
//! file-poller, so the bar widget + drawer page need no changes.
//!
//! # Limitations
//!
//! - Recurring events: only the master entry's DTSTART is surfaced, and
//!   only if it falls in the upcoming window. `get_object_strings` returns
//!   master components, not expanded instances. Full RRULE expansion now
//!   becomes *feasible* (`e_cal_client_generate_instances_sync` expands a
//!   range server-side) but needs a new hytte-ecal binding — a follow-up.
//! - Timezones: `WithTimezone` DTSTARTs are converted via
//!   `try_into_utc()` (which uses chrono-tz when available); when that
//!   fails we treat the inner naive datetime as local time. Floating
//!   datetimes are interpreted as local time.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::thread;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone};
use futures_signals::signal::{Mutable, Signal};
use hytte_ecal::sys::ECalClientSourceType;
use hytte_ecal::{CalClient, Registry, Source};
use hytte_reactive::{Service, registry};
use icalendar::{
    Calendar, CalendarComponent, CalendarDateTime, Component, DatePerhapsTime, EventLike,
    EventStatus,
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
}

/// How far ahead to surface events. Anything starting after `now + NEXT_DAYS`
/// is dropped from the signal.
const NEXT_DAYS: i64 = 7;

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

/// Signal of the next [`NEXT_DAYS`] days of events, sorted ascending by
/// start. Empty until the first refresh completes (or if EDS has no
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
        let window_end = now + Duration::days(NEXT_DAYS);
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
            // "#t" = match-all; the upcoming-window filter happens in Rust
            // so behaviour matches the prior .ics path exactly. (Components
            // come back master-only — recurrence instances are not
            // expanded here; see the module-level limitations note.)
            let objects = match client.get_object_strings("#t") {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(source = %source_uid, error = %e, "calendar: query failed");
                    continue;
                }
            };
            for body in objects {
                out.extend(parse_ics_body(&body, &calendar_name, now, window_end));
            }
        }
        out.sort_by_key(|e| e.start);
        out
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

// ── iCalendar parsing ────────────────────────────────────────────────────────

/// Parse one iCalendar body (a VCALENDAR, or a bare VEVENT that libecal
/// sometimes hands back) into the [`CalendarEvent`]s it contains that fall
/// inside the `[now, window_end]` upcoming window.
fn parse_ics_body(
    body: &str,
    calendar_name: &str,
    now: DateTime<Local>,
    window_end: DateTime<Local>,
) -> Vec<CalendarEvent> {
    // libecal usually hands back a full VCALENDAR; some backends return a
    // bare VEVENT, so fall back to wrapping it before parsing (same trick
    // as `tasks::parse_one`).
    let Some(parsed) = body.parse::<Calendar>().ok().or_else(|| {
        let wrapped = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//hytte//\r\n{body}\r\nEND:VCALENDAR\r\n"
        );
        wrapped.parse::<Calendar>().ok()
    }) else {
        tracing::warn!("calendar: iCal body failed to parse");
        return Vec::new();
    };

    let mut out = Vec::new();
    for event in parsed.components.iter().filter_map(|c| match c {
        CalendarComponent::Event(e) => Some(e),
        _ => None,
    }) {
        if let Some(ev) = event_to_calendar_event(event, calendar_name, now, window_end, out.len()) {
            out.push(ev);
        }
    }
    out
}

/// Convert a single parsed VEVENT into a [`CalendarEvent`], or `None` if it
/// is cancelled, undatable, already over, or starts past the window.
/// `anon_index` disambiguates the synthesised UID when a VEVENT carries no
/// UID of its own.
fn event_to_calendar_event(
    event: &icalendar::Event,
    calendar_name: &str,
    now: DateTime<Local>,
    window_end: DateTime<Local>,
    anon_index: usize,
) -> Option<CalendarEvent> {
    if event.get_status() == Some(EventStatus::Cancelled) {
        return None;
    }
    let start_dpt = event.get_start()?;
    let (start_local, all_day) = dpt_to_local(start_dpt)?;
    // RFC 5545 §3.6.1: a VEVENT carries DTEND xor DURATION (never both).
    // Google Calendar and many recurring-instance emitters prefer
    // DURATION, so we have to honour both.
    let end_local = if let Some((dt, _)) = event.get_end().and_then(dpt_to_local) {
        dt
    } else if let Some(dur) = event
        .property_value("DURATION")
        .and_then(parse_iso8601_duration)
    {
        start_local + dur
    } else if all_day {
        // Per RFC 5545: if neither DTEND nor DURATION is present on a
        // DATE-typed VEVENT, the event ends at the end of DTSTART's day.
        start_local + Duration::days(1)
    } else {
        // Missing DTEND/DURATION on a DATE-TIME means a zero-duration event
        // in iCal, but for UI purposes we'd rather show a 1-hour bar than a
        // coincident edge.
        start_local + Duration::hours(1)
    };

    // Window filter: include if the event hasn't ended yet AND its start
    // lies inside the next-N-days window. The "hasn't ended" half catches
    // multi-day events that started in the past but are still ongoing.
    if end_local < now {
        return None;
    }
    if start_local > window_end {
        return None;
    }

    let uid = event.get_uid().map_or_else(
        || format!("anon:{calendar_name}:{anon_index}"),
        str::to_string,
    );
    let summary = event
        .get_summary()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(no title)".to_string());
    let location = event
        .get_location()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    Some(CalendarEvent {
        uid,
        summary,
        start: start_local,
        end: end_local,
        location,
        all_day,
        calendar_name: calendar_name.to_string(),
    })
}

/// Resolve a `DatePerhapsTime` to local time. Returns `(local_dt, all_day)`,
/// or `None` if the date couldn't be normalised (e.g. `WithTimezone` with an
/// unknown TZID and chrono-tz disabled).
fn dpt_to_local(dpt: DatePerhapsTime) -> Option<(DateTime<Local>, bool)> {
    match dpt {
        DatePerhapsTime::Date(d) => {
            // All-day: anchor to midnight in local time. Calendars that
            // treat DATEs as floating across timezones still get a stable
            // local representation this way.
            let naive = d.and_time(NaiveTime::from_hms_opt(0, 0, 0)?);
            Some((Local.from_local_datetime(&naive).single()?, true))
        }
        DatePerhapsTime::DateTime(cdt) => match cdt {
            CalendarDateTime::Utc(dt) => Some((dt.with_timezone(&Local), false)),
            CalendarDateTime::Floating(naive) => {
                Some((Local.from_local_datetime(&naive).single()?, false))
            }
            ref other @ CalendarDateTime::WithTimezone {
                ref date_time,
                ref tzid,
            } => {
                // WithTimezone: chrono-tz-backed conversion when the TZID
                // is one chrono-tz knows; otherwise interpret the wall-
                // clock time as local. The fallback is wrong for events
                // authored in another zone but at least keeps them
                // visible in the upcoming list.
                if let Some(utc) = other.try_into_utc() {
                    Some((utc.with_timezone(&Local), false))
                } else {
                    tracing::debug!(
                        tzid = %tzid,
                        "calendar: unknown TZID; falling back to local interpretation",
                    );
                    Some((Local.from_local_datetime(date_time).single()?, false))
                }
            }
        },
    }
}

/// Parse an ISO 8601 / RFC 5545 duration string into a `chrono::Duration`.
///
/// Coverage: `PT0S`, `PT15M`, `PT1H`, `PT1H30M`, `PT4H`, `P1D`, `P3D`,
/// `P1W`, and combined forms like `P1DT2H`. A leading `-` flips sign.
/// Anything that doesn't tokenise cleanly returns `None` rather than
/// crashing; the caller falls back to the prior heuristic.
fn parse_iso8601_duration(raw: &str) -> Option<Duration> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let (negative, rest) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    // Must start with a literal `P`.
    let rest = rest.strip_prefix('P')?;

    // Split into the date-part (before any `T`) and the time-part (after).
    let (date_part, time_part) = match rest.find('T') {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, ""),
    };

    // Both halves can be empty individually — `PT1H` has no date part,
    // `P1D` has no time part — but the *combined* result must contain at
    // least one segment. Reject `P` and `PT` outright.
    if date_part.is_empty() && time_part.is_empty() {
        return None;
    }

    let mut total = Duration::zero();

    // Date-part legal designators: D, W. (Y/M omitted: variable length,
    // not meaningful for a UI offset against an instant.)
    let mut num = String::new();
    for c in date_part.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        if num.is_empty() {
            return None;
        }
        let n: i64 = num.parse().ok()?;
        num.clear();
        match c {
            'D' => total += Duration::days(n),
            'W' => total += Duration::weeks(n),
            // Y and M would need a calendar anchor to be meaningful;
            // we deliberately don't support them.
            _ => return None,
        }
    }
    if !num.is_empty() {
        // Trailing digits with no designator — malformed.
        return None;
    }

    // Time-part legal designators: H, M, S.
    let mut num = String::new();
    for c in time_part.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        if num.is_empty() {
            return None;
        }
        let n: i64 = num.parse().ok()?;
        num.clear();
        match c {
            'H' => total += Duration::hours(n),
            'M' => total += Duration::minutes(n),
            'S' => total += Duration::seconds(n),
            _ => return None,
        }
    }
    if !num.is_empty() {
        return None;
    }

    if negative {
        total = -total;
    }
    Some(total)
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

    #[test]
    fn parse_minimal_ics() {
        // A timed event in the next 7 days is surfaced.
        let now = Local::now();
        let in_two_days = now + Duration::days(2);
        let body = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//\r\n\
             BEGIN:VEVENT\r\nUID:abc-123\r\nSUMMARY:Lunch\r\n\
             DTSTART:{}\r\nDTEND:{}\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n",
            in_two_days.naive_utc().format("%Y%m%dT%H%M%SZ"),
            (in_two_days + Duration::hours(1))
                .naive_utc()
                .format("%Y%m%dT%H%M%SZ"),
        );
        let evs = parse_ics_body(
            &body,
            "test-cal",
            now - Duration::hours(1),
            now + Duration::days(NEXT_DAYS),
        );
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].uid, "abc-123");
        assert_eq!(evs[0].summary, "Lunch");
        assert!(!evs[0].all_day);
        assert_eq!(evs[0].calendar_name, "test-cal");
    }

    #[test]
    fn skips_cancelled_and_out_of_window() {
        let now = Local::now();
        let body = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//\r\n\
             BEGIN:VEVENT\r\nUID:cx\r\nSUMMARY:Cancelled\r\nSTATUS:CANCELLED\r\n\
             DTSTART:{}\r\nEND:VEVENT\r\n\
             BEGIN:VEVENT\r\nUID:fx\r\nSUMMARY:Far future\r\n\
             DTSTART:{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            now.naive_utc().format("%Y%m%dT%H%M%SZ"),
            (now + Duration::days(NEXT_DAYS + 30))
                .naive_utc()
                .format("%Y%m%dT%H%M%SZ"),
        );
        let evs = parse_ics_body(&body, "test-cal", now, now + Duration::days(NEXT_DAYS));
        assert!(evs.is_empty());
    }

    #[test]
    fn parses_bare_vevent_without_vcalendar_wrapper() {
        // libecal sometimes returns a component with no VCALENDAR wrapper;
        // parse_ics_body wraps + retries.
        let now = Local::now();
        let in_two_days = now + Duration::days(2);
        let body = format!(
            "BEGIN:VEVENT\r\nUID:bare-1\r\nSUMMARY:Standup\r\nDTSTART:{}\r\nEND:VEVENT\r\n",
            in_two_days.naive_utc().format("%Y%m%dT%H%M%SZ"),
        );
        let evs = parse_ics_body(
            &body,
            "test-cal",
            now - Duration::hours(1),
            now + Duration::days(NEXT_DAYS),
        );
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].uid, "bare-1");
        assert_eq!(evs[0].summary, "Standup");
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
    fn iso8601_duration_pt_forms() {
        assert_eq!(parse_iso8601_duration("PT0S"), Some(Duration::zero()));
        assert_eq!(parse_iso8601_duration("PT15M"), Some(Duration::minutes(15)));
        assert_eq!(parse_iso8601_duration("PT1H"), Some(Duration::hours(1)));
        assert_eq!(
            parse_iso8601_duration("PT1H30M"),
            Some(Duration::hours(1) + Duration::minutes(30)),
        );
        assert_eq!(parse_iso8601_duration("PT4H"), Some(Duration::hours(4)));
    }

    #[test]
    fn iso8601_duration_p_forms() {
        assert_eq!(parse_iso8601_duration("P1D"), Some(Duration::days(1)));
        assert_eq!(parse_iso8601_duration("P3D"), Some(Duration::days(3)));
        assert_eq!(parse_iso8601_duration("P1W"), Some(Duration::weeks(1)));
        assert_eq!(
            parse_iso8601_duration("P1DT2H"),
            Some(Duration::days(1) + Duration::hours(2)),
        );
    }

    #[test]
    fn iso8601_duration_rejects_garbage() {
        assert_eq!(parse_iso8601_duration(""), None);
        assert_eq!(parse_iso8601_duration("P"), None);
        assert_eq!(parse_iso8601_duration("PT"), None);
        assert_eq!(parse_iso8601_duration("1H"), None);
        assert_eq!(parse_iso8601_duration("PT1X"), None);
    }

    #[test]
    fn duration_pt4h_overrides_dtend_fabrication() {
        // VEVENT with DURATION:PT4H and no DTEND must end 4h after start,
        // not 1h (the previous default).
        let now = Local::now();
        let start = now + Duration::hours(2);
        let body = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//\r\n\
             BEGIN:VEVENT\r\nUID:dur-pt4h\r\nSUMMARY:Workshop\r\n\
             DTSTART:{}\r\nDURATION:PT4H\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n",
            start.naive_utc().format("%Y%m%dT%H%M%SZ"),
        );
        let evs = parse_ics_body(
            &body,
            "test-cal",
            now - Duration::hours(1),
            now + Duration::days(NEXT_DAYS),
        );
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].end - evs[0].start, Duration::hours(4));
    }

    #[test]
    fn duration_p3d_overrides_for_all_day() {
        // All-day VEVENT (DTSTART;VALUE=DATE) with DURATION:P3D should
        // span 3 days, not 1 (the all-day fallback).
        let now = Local::now();
        let date = (now + Duration::days(1)).format("%Y%m%d");
        let body = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//\r\n\
             BEGIN:VEVENT\r\nUID:dur-p3d\r\nSUMMARY:Long weekend\r\n\
             DTSTART;VALUE=DATE:{date}\r\nDURATION:P3D\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n",
        );
        let evs = parse_ics_body(
            &body,
            "test-cal",
            now - Duration::hours(1),
            now + Duration::days(NEXT_DAYS),
        );
        assert_eq!(evs.len(), 1);
        assert!(evs[0].all_day);
        assert_eq!(evs[0].end - evs[0].start, Duration::days(3));
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
