//! Sidebar calendar widget: month grid + 7-day upcoming events list.
//! Sibling to the drawer panel `trollshell/src/panels/calendar.rs` —
//! same data source (`hytte::services::calendar::events`), parallel
//! rendering for the always-on left sidebar surface. See
//! `docs/superpowers/specs/2026-05-14-calendar-widget-design.md`.

use std::collections::HashSet;

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::calendar::CalendarEvent;

/// Build the sidebar calendar widget. Caller appends the returned widget
/// to `.ts-sidebar`; the widget owns its own subscriptions to
/// `calendar::events()` and `sidebar::open_signal(monitor)`.
pub fn widget(_monitor: &Monitor) -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.add_css_class("ts-sidebar-calendar");
    column.upcast()
}

/// Pure helper: which day-of-month numbers in `(year, month_1_indexed)`
/// have at least one event? Split out from `apply_event_marks` so the
/// month-filter logic can be unit-tested without a GtkCalendar.
fn marked_days(events: &[CalendarEvent], year: i32, month_1_indexed: u32) -> HashSet<u32> {
    use chrono::Datelike;
    events
        .iter()
        .filter_map(|e| {
            let d = e.start.date_naive();
            (d.year() == year && d.month() == month_1_indexed).then_some(d.day())
        })
        .collect()
}

/// Mark each day in `cal`'s currently-visible month that has at least
/// one event. `GtkCalendar::month()` is 0-indexed; chrono's `month()` is
/// 1-indexed — bridge that here, not at call sites.
fn apply_event_marks(cal: &gtk::Calendar, events: &[CalendarEvent]) {
    cal.clear_marks();
    let month = u32::try_from(cal.month() + 1).unwrap_or(0);
    let year = cal.year();
    for day in marked_days(events, year, month) {
        cal.mark_day(day);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};
    use hytte::services::calendar::CalendarEvent;

    /// Build a test event whose start is `start` and end is `start + 1h`.
    fn ev(start: chrono::DateTime<chrono::Local>) -> CalendarEvent {
        CalendarEvent {
            uid: "u".into(),
            summary: "s".into(),
            start,
            end: start + chrono::Duration::hours(1),
            location: None,
            all_day: false,
            calendar_name: "c".into(),
        }
    }

    #[test]
    fn marks_only_current_month_events() {
        // Two events: one on day 15 of a fixed (year, month), one on day 3
        // of the following month. Viewing the first month should mark day
        // 15 only.
        let a = Local.with_ymd_and_hms(2026, 5, 15, 9, 0, 0).single().unwrap();
        let b = Local.with_ymd_and_hms(2026, 6, 3, 9, 0, 0).single().unwrap();
        let evs = vec![ev(a), ev(b)];

        let marked = marked_days(&evs, 2026, 5);
        assert!(marked.contains(&15));
        assert!(!marked.contains(&3));
        assert_eq!(marked.len(), 1);
    }

    #[test]
    fn deduplicates_same_day_events() {
        // Two events on the same day → marked set has that day once.
        let a = Local.with_ymd_and_hms(2026, 5, 10, 9, 0, 0).single().unwrap();
        let b = Local.with_ymd_and_hms(2026, 5, 10, 14, 0, 0).single().unwrap();
        let evs = vec![ev(a), ev(b)];

        let marked = marked_days(&evs, 2026, 5);
        assert_eq!(marked.len(), 1);
        assert!(marked.contains(&10));
    }

    #[test]
    fn empty_events_marks_nothing() {
        let marked = marked_days(&[], 2026, 5);
        assert!(marked.is_empty());
    }
}
