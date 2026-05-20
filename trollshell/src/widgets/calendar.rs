//! Sidebar calendar widget: month grid + 7-day upcoming events list.
//! Sibling to the drawer panel `trollshell/src/panels/calendar.rs` —
//! same data source (`hytte::services::calendar::events`), parallel
//! rendering for the always-on left sidebar surface. See
//! `docs/superpowers/specs/2026-05-14-calendar-widget-design.md`.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::calendar::CalendarEvent;

/// Build the sidebar calendar widget. Caller appends the returned widget
/// to `.ts-sidebar`; the widget owns its own subscriptions to
/// `calendar::events()` and `sidebar::open_signal(monitor)`.
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.add_css_class("ts-sidebar-calendar");

    let cal = gtk::Calendar::new();
    cal.set_show_heading(true);
    cal.set_show_day_names(true);
    cal.set_show_week_numbers(false);
    cal.add_css_class("ts-calendar");
    column.append(&cal);

    let header = gtk::Label::new(Some("UPCOMING"));
    header.add_css_class("ts-sidebar-cal-header");
    header.set_halign(gtk::Align::Start);
    column.append(&header);

    let group = adw::PreferencesGroup::new();
    group.add_css_class("ts-sidebar-cal-list");

    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_min_content_height(220);
    scrolled.set_max_content_height(360);
    scrolled.set_child(Some(&group));
    column.append(&scrolled);

    // Tracked rows + placeholder so each emission can remove the previous
    // contents before re-adding. The drawer panel uses the same pattern.
    let rows_track: Rc<RefCell<Vec<(chrono::NaiveDate, adw::ActionRow)>>> =
        Rc::new(RefCell::new(Vec::new()));
    let placeholder_track: Rc<RefCell<Option<adw::ActionRow>>> =
        Rc::new(RefCell::new(None));
    // Latest events snapshot, shared between the bind closure and the
    // prev/next-month handlers added in Task 5.
    let current_events: Rc<RefCell<Vec<CalendarEvent>>> =
        Rc::new(RefCell::new(Vec::new()));

    wire_events_bind(&group, &rows_track, &placeholder_track, &cal, &current_events);
    wire_month_navigation(&cal, &current_events);
    wire_day_selection(&cal, &rows_track, &scrolled);
    wire_open_refresh(monitor);

    column.upcast()
}

/// Refresh the upcoming-events list and month marks on every snapshot from
/// `calendar::events()`. Stashes the snapshot in `current_events` so the
/// month-nav handlers see fresh data without re-subscribing.
fn wire_events_bind(
    group: &adw::PreferencesGroup,
    rows_track: &Rc<RefCell<Vec<(chrono::NaiveDate, adw::ActionRow)>>>,
    placeholder_track: &Rc<RefCell<Option<adw::ActionRow>>>,
    cal: &gtk::Calendar,
    current_events: &Rc<RefCell<Vec<CalendarEvent>>>,
) {
    use hytte::services::calendar;

    let group_for_bind = group.clone();
    let rows_for_bind = rows_track.clone();
    let placeholder_for_bind = placeholder_track.clone();
    let cal_for_bind = cal.clone();
    let current_events_for_bind = current_events.clone();
    bind(calendar::events(), group, move |_, evs| {
        for (_d, row) in rows_for_bind.borrow_mut().drain(..) {
            group_for_bind.remove(&row);
        }
        if let Some(p) = placeholder_for_bind.borrow_mut().take() {
            group_for_bind.remove(&p);
        }

        current_events_for_bind.borrow_mut().clone_from(&evs);
        apply_event_marks(&cal_for_bind, &evs);

        if evs.is_empty() {
            let placeholder = adw::ActionRow::builder()
                .title("No upcoming events")
                .subtitle("Add a calendar via Settings \u{2192} Online Accounts.")
                .activatable(false)
                .build();
            group_for_bind.add(&placeholder);
            *placeholder_for_bind.borrow_mut() = Some(placeholder);
            return;
        }

        let mut new_rows: Vec<(chrono::NaiveDate, adw::ActionRow)> =
            Vec::with_capacity(evs.len());
        for ev in &evs {
            let row = build_calendar_row(ev);
            group_for_bind.add(&row);
            new_rows.push((ev.start.date_naive(), row));
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });
}

/// `connect_next_month` bumps year on December rollover internally, so a
/// separate `connect_year_changed` isn't needed.
fn wire_month_navigation(cal: &gtk::Calendar, current_events: &Rc<RefCell<Vec<CalendarEvent>>>) {
    let events_for_next = current_events.clone();
    cal.connect_next_month(move |c| apply_event_marks(c, &events_for_next.borrow()));
    let events_for_prev = current_events.clone();
    cal.connect_prev_month(move |c| apply_event_marks(c, &events_for_prev.borrow()));
}

fn wire_day_selection(
    cal: &gtk::Calendar,
    rows_track: &Rc<RefCell<Vec<(chrono::NaiveDate, adw::ActionRow)>>>,
    scrolled: &gtk::ScrolledWindow,
) {
    let rows_for_select = rows_track.clone();
    let scrolled_for_select = scrolled.clone();
    cal.connect_day_selected(move |c| {
        let gdt = c.date();
        let y = gdt.year();
        let (Ok(m), Ok(day)) = (u32::try_from(gdt.month()), u32::try_from(gdt.day_of_month()))
        else {
            return;
        };
        let Some(d) = chrono::NaiveDate::from_ymd_opt(y, m, day) else { return };
        let rows = rows_for_select.borrow();
        let Some((_d, row)) = rows.iter().find(|(date, _)| *date == d) else { return };
        scroll_row_into_view(&scrolled_for_select, row);
        flash_row_highlight(row);
    });
}

/// Force a fresh scan when the user opens the sidebar — avoids showing
/// up-to-60-second-stale data on open. Edge-triggered via a `Cell` so the
/// initial state replay from `signal()` doesn't fire a refresh when the
/// sidebar starts closed.
fn wire_open_refresh(monitor: &Monitor) {
    use std::cell::Cell;
    use hytte::gtk::glib;

    let last_open = Rc::new(Cell::new(false));
    glib::MainContext::default().spawn_local(
        crate::overlays::sidebar::open_signal(monitor).for_each(move |open| {
            let prev = last_open.replace(open);
            if open && !prev {
                hytte::services::calendar::refresh();
            }
            async {}
        }),
    );
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

/// Build an `adw::ActionRow` for a single calendar event.
/// The subtitle shows the when-string and, if present, the location on
/// a separate line so long venue names wrap without inflating sidebar width.
fn build_calendar_row(ev: &CalendarEvent) -> adw::ActionRow {
    use hytte::adw::{self, prelude::*};
    use hytte::services::calendar::format_when;

    let when = format_when(ev);
    let subtitle = match &ev.location {
        Some(loc) => format!("{when}\n{loc}"),
        None => when,
    };

    let row = adw::ActionRow::builder()
        .title(&ev.summary)
        .subtitle(&subtitle)
        .activatable(false)
        .build();
    row.set_subtitle_lines(0); // wrap
    row.set_title_lines(1);

    let icon = gtk::Image::from_icon_name("x-office-calendar-symbolic");
    icon.set_valign(gtk::Align::Center);
    row.add_prefix(&icon);

    row
}

/// Scroll `scrolled` so that `row` is visible. Uses `compute_point` to
/// translate the row's origin into the scrolled-window child's coordinate
/// space; an 8px lead-in keeps the row from butting against the top edge.
fn scroll_row_into_view(scrolled: &gtk::ScrolledWindow, row: &adw::ActionRow) {
    use hytte::gtk::prelude::{AdjustmentExt, WidgetExt};
    let Some(child) = scrolled.child() else {
        return;
    };
    let origin = gtk::graphene::Point::new(0.0, 0.0);
    let Some(point) = row.compute_point(&child, &origin) else {
        return;
    };
    let y = f64::from(point.y());
    let adj = scrolled.vadjustment();
    let target = (y - 8.0).max(adj.lower());
    let max = (adj.upper() - adj.page_size()).max(adj.lower());
    adj.set_value(target.min(max));
}

/// Add `.ts-cal-day-hit` to `row` for ~1.5s, then remove it. The existing
/// CSS rule on `.ts-cal-day-hit` has a 600ms transition so the highlight
/// fades rather than flashes.
fn flash_row_highlight(row: &adw::ActionRow) {
    use hytte::gtk::glib;
    use hytte::gtk::prelude::WidgetExt;
    row.add_css_class("ts-cal-day-hit");
    let row_for_clear = row.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
        row_for_clear.remove_css_class("ts-cal-day-hit");
    });
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
