//! Calendar service backed by evolution-data-server's on-disk .ics cache.
//!
//! GNOME's evolution-data-server (EDS), provisioned by GNOME Online Accounts
//! (GOA) via `gnome-control-center → Online Accounts`, keeps a synced copy
//! of every calendar source in iCalendar form at:
//!
//!     ~/.local/share/evolution/calendar/<source-uid>/calendar.ics
//!
//! v1 reads those files directly and parses them with the `icalendar`
//! crate — no D-Bus, no `ECalClient` bindings. The trade-off is up to one
//! [`POLL_INTERVAL`] of sync lag; the win is dramatically simpler code.
//!
//! The service exposes [`events()`] — `Signal<Vec<CalendarEvent>>`, sorted
//! by start time, filtered to events that haven't ended and start within
//! [`NEXT_DAYS`] days. [`refresh()`] re-scans the cache directory and
//! re-parses every `.ics` file. The service spawns a background tokio task
//! that polls every [`POLL_INTERVAL`].
//!
//! # Limitations (v1)
//!
//! - Recurring events: only the master entry's DTSTART is surfaced, and
//!   only if it falls in the upcoming window. Full RRULE expansion is a
//!   v2 task; see `etc/calendar/README.md`.
//! - Calendar names: best-effort from the source-dir basename. EDS picks
//!   UUID-flavoured directory names, so the title isn't human-friendly
//!   until a v2 helper reads `metadata.xml` (or asks GOA over D-Bus).
//! - Timezones: `WithTimezone` DTSTARTs are converted via
//!   `try_into_utc()` (which uses chrono-tz when available); when that
//!   fails we treat the inner naive datetime as local time. Floating
//!   datetimes are interpreted as local time.
//!
//! # Background refresh
//!
//! Pure 60-second polling — no inotify. The cache directory turn-over is
//! infrequent enough that polling is fine, and avoiding the `notify` dep
//! keeps the build matrix small. If GOA pushes a sync inside the polling
//! gap the user sees up to 60 s of staleness; the drawer page can also
//! call [`refresh()`] on open if a future task wants near-zero lag.

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use icalendar::{Calendar, CalendarDateTime, Component, DatePerhapsTime, EventLike, EventStatus};
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

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
    /// Best-effort calendar source label, from the EDS source-dir name.
    /// Often a UUID; mapping to GOA-friendly names is a v2 task.
    pub calendar_name: String,
}

/// How far ahead to surface events. Anything starting after `now + NEXT_DAYS`
/// is dropped from the signal.
const NEXT_DAYS: i64 = 7;

/// Background refresh cadence. EDS rewrites `.ics` files on its own sync
/// cycle (typically minutes); 60 s catches changes within a minute without
/// hammering the disk.
const POLL_INTERVAL: StdDuration = StdDuration::from_secs(60);

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

/// Service marker. Pass to `App::with` to register handles + spawn the
/// 60 s refresh task. An initial refresh is fired immediately.
pub struct CalendarService;

impl Service for CalendarService {
    type Handles = CalendarHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = CalendarHandles::default();
        let writer = handles.events.clone();
        rt.spawn(async move {
            poll_loop(writer).await;
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
/// start. Empty until the first refresh completes (or if the EDS cache
/// directory doesn't exist).
pub fn events() -> impl Signal<Item = Vec<CalendarEvent>> {
    registry::with(|r| {
        r.get::<CalendarHandles>()
            .expect("calendar::service() not registered")
            .events
            .signal_cloned()
    })
}

/// Re-scan the EDS cache directory and update the [`events()`] signal.
/// Idempotent — safe to call from page-show handlers or other event hooks.
/// Heavy work (filesystem walk + iCal parse) runs on a blocking pool.
pub fn refresh() {
    let writer = registry::with(|r| r.get::<CalendarHandles>().map(|h| h.events.clone()));
    let Some(writer) = writer else {
        tracing::warn!("calendar::refresh: service not registered");
        return;
    };
    hytte_reactive::runtime::handle().spawn_blocking(move || {
        do_refresh(&writer);
    });
}

// ── Polling loop ─────────────────────────────────────────────────────────────

async fn poll_loop(writer: Mutable<Vec<CalendarEvent>>) {
    loop {
        // Refresh inline on a blocking thread so we don't park a tokio
        // worker on filesystem I/O.
        let writer_for_blocking = writer.clone();
        let _ =
            tokio::task::spawn_blocking(move || do_refresh(&writer_for_blocking)).await;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn do_refresh(writer: &Mutable<Vec<CalendarEvent>>) {
    let snapshot = scan_cache_dir();
    // PartialEq dedup: avoid re-emitting identical lists every minute.
    let changed = {
        let cur = writer.lock_ref();
        *cur != snapshot
    };
    if changed {
        writer.set(snapshot);
    }
}

// ── Filesystem scanning ──────────────────────────────────────────────────────

fn cache_root() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".local/share/evolution/calendar"))
}

fn scan_cache_dir() -> Vec<CalendarEvent> {
    let Some(root) = cache_root() else {
        tracing::debug!("calendar: $HOME unset; skipping scan");
        return Vec::new();
    };
    let entries = match std::fs::read_dir(&root) {
        Ok(rd) => rd,
        Err(e) => {
            // ENOENT ⇒ EDS hasn't been provisioned (no GOA accounts).
            // Surface as empty list, log at debug to avoid noise.
            tracing::debug!(error = %e, dir = %root.display(), "calendar: cache dir read failed");
            return Vec::new();
        }
    };

    let now = Local::now();
    let window_end = now + Duration::days(NEXT_DAYS);
    let mut out: Vec<CalendarEvent> = Vec::new();

    for entry in entries.flatten() {
        let source_dir = entry.path();
        if !source_dir.is_dir() {
            continue;
        }
        let ics = source_dir.join("calendar.ics");
        if !ics.is_file() {
            continue;
        }
        let calendar_name = source_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("calendar")
            .to_string();
        match parse_ics_file(&ics, &calendar_name, now, window_end) {
            Ok(mut evs) => out.append(&mut evs),
            Err(e) => {
                tracing::warn!(error = %e, file = %ics.display(), "calendar: parse failed");
            }
        }
    }

    out.sort_by_key(|e| e.start);
    out
}

fn parse_ics_file(
    path: &Path,
    calendar_name: &str,
    now: DateTime<Local>,
    window_end: DateTime<Local>,
) -> anyhow::Result<Vec<CalendarEvent>> {
    let body = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    // `Calendar::parse` is the high-level entry point and accepts a string.
    let parsed: Calendar = body
        .parse()
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;

    let mut out = Vec::new();
    for event in parsed.components.iter().filter_map(|c| match c {
        icalendar::CalendarComponent::Event(e) => Some(e),
        _ => None,
    }) {
        if event.get_status() == Some(EventStatus::Cancelled) {
            continue;
        }
        let Some(start_dpt) = event.get_start() else {
            continue;
        };
        let Some((start_local, all_day)) = dpt_to_local(start_dpt) else {
            continue;
        };
        let end_local = event.get_end().and_then(dpt_to_local).map_or_else(
            || {
                if all_day {
                    // Per RFC 5545: if DTEND is missing on a DATE-typed
                    // VEVENT, the event ends at the end of DTSTART's day.
                    start_local + Duration::days(1)
                } else {
                    // Missing DTEND on a DATE-TIME means a zero-duration
                    // event in iCal, but for UI purposes we'd rather see
                    // a 1-hour bar than a coincident edge.
                    start_local + Duration::hours(1)
                }
            },
            |(dt, _)| dt,
        );

        // Window filter: include if the event hasn't ended yet AND its
        // start lies inside the next-N-days window. The "hasn't ended"
        // half catches multi-day events that started in the past but
        // are still ongoing — we want to surface those too.
        if end_local < now {
            continue;
        }
        if start_local > window_end {
            continue;
        }

        let uid = event
            .get_uid()
            .map_or_else(|| format!("anon:{calendar_name}:{}", out.len()), str::to_string);
        let summary = event
            .get_summary()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(no title)".to_string());
        let location = event
            .get_location()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        out.push(CalendarEvent {
            uid,
            summary,
            start: start_local,
            end: end_local,
            location,
            all_day,
            calendar_name: calendar_name.to_string(),
        });
    }
    Ok(out)
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
            ref other @ CalendarDateTime::WithTimezone { ref date_time, .. } => {
                // WithTimezone: chrono-tz-backed conversion when the TZID
                // is one chrono-tz knows; otherwise interpret the wall-
                // clock time as local. The fallback is wrong for events
                // authored in another zone but at least keeps them
                // visible in the upcoming list.
                if let Some(utc) = other.try_into_utc() {
                    Some((utc.with_timezone(&Local), false))
                } else {
                    Some((Local.from_local_datetime(date_time).single()?, false))
                }
            }
        },
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

    if event.all_day {
        return format!("All day, {}", short_date(start_date, today));
    }

    let start_label = short_date(start_date, today);
    let start_hm = event.start.format("%H:%M");
    if event.start.date_naive() == event.end.date_naive() {
        let end_hm = event.end.format("%H:%M");
        format!("{start_label} {start_hm}\u{2013}{end_hm}")
    } else {
        format!("{start_label} {start_hm}")
    }
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
        let path = std::env::temp_dir().join("hytte-calendar-test.ics");
        std::fs::write(&path, body).unwrap();
        let evs = parse_ics_file(
            &path,
            "test-cal",
            now - Duration::hours(1),
            now + Duration::days(NEXT_DAYS),
        )
        .unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].uid, "abc-123");
        assert_eq!(evs[0].summary, "Lunch");
        assert!(!evs[0].all_day);
        let _ = std::fs::remove_file(&path);
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
        let path = std::env::temp_dir().join("hytte-calendar-test2.ics");
        std::fs::write(&path, body).unwrap();
        let evs = parse_ics_file(
            &path,
            "test-cal",
            now,
            now + Duration::days(NEXT_DAYS),
        )
        .unwrap();
        assert!(evs.is_empty());
        let _ = std::fs::remove_file(&path);
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

}
