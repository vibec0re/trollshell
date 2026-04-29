//! Drawer panel for upcoming calendar events. Backed by
//! `hytte::services::calendar`, which reads evolution-data-server's on-disk
//! `.ics` cache (populated by GNOME Online Accounts via gnome-control-center).
//!
//! Top: a `gtk::Calendar` scrolled to the current month. v1 shows the
//! month grid only — no per-day event marking; that's a v2 task once the
//! signal carries enough data to compute marked-day sets.
//!
//! Below: an `adw::PreferencesGroup` titled "Upcoming" listing every event
//! in the next 7 days, sorted ascending by start. Empty list ⇒ a single
//! non-activatable "No upcoming events" placeholder row.
//!
//! Click is a no-op for v1; future "open in calendar app" would pipe the
//! event UID at a CLI helper.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use chrono::Datelike;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self, glib};
use hytte::prelude::*;
use hytte::services::calendar::{self, CalendarEvent};

use crate::components::layout::{finish_page, page_box};

pub fn panel_calendar() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    // ── Month view ─────────────────────────────────────────────────────────
    // GtkCalendar scrolls itself to the current month on construction.
    // Days that have at least one event in the visible month get mark_day().
    let cal = gtk::Calendar::new();
    cal.set_show_heading(true);
    cal.set_show_day_names(true);
    cal.set_show_week_numbers(false);
    cal.add_css_class("ts-calendar");
    column.append(&cal);

    // Latest events snapshot, shared between the events() subscription and
    // the prev/next-month signal handlers.
    let current_events: Rc<RefCell<Vec<CalendarEvent>>> = Rc::new(RefCell::new(Vec::new()));

    // Re-mark on month navigation. connect_next_month bumps year on December
    // rollover internally, so connect_year_changed isn't needed.
    {
        let events_for_next = current_events.clone();
        cal.connect_next_month(move |c| apply_event_marks(c, &events_for_next.borrow()));
    }
    {
        let events_for_prev = current_events.clone();
        cal.connect_prev_month(move |c| apply_event_marks(c, &events_for_prev.borrow()));
    }

    // ── Upcoming list ──────────────────────────────────────────────────────
    let group = adw::PreferencesGroup::builder().title("Upcoming").build();

    // Track rows by date so click-day in the calendar can scroll the list to
    // the matching event. Plus the empty-state placeholder so we can swap
    // them on each signal emission.
    let rows_track: Rc<RefCell<Vec<(chrono::NaiveDate, adw::ActionRow)>>> =
        Rc::new(RefCell::new(Vec::new()));
    let placeholder_track: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));

    // Bounded ScrolledWindow so click-day can drive scrolling directly.
    // finish_page only wraps in adw::Clamp, which doesn't scroll.
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_min_content_height(220);
    scrolled.set_max_content_height(360);
    scrolled.add_css_class("ts-calendar-list");
    scrolled.set_child(Some(&group));
    column.append(&scrolled);

    // Click a marked day → scroll the events list to the first matching
    // event and briefly highlight it. We compare on NaiveDate so we don't
    // care about start-time; multiple events on the same day all match the
    // first one we pushed (events are already sorted ascending).
    {
        let rows_for_select = rows_track.clone();
        let scrolled_for_select = scrolled.clone();
        cal.connect_day_selected(move |c| {
            let gdt = c.date();
            let y = gdt.year();
            let Ok(m) = u32::try_from(gdt.month()) else { return; };
            let Ok(day) = u32::try_from(gdt.day_of_month()) else { return; };
            let Some(d) = chrono::NaiveDate::from_ymd_opt(y, m, day) else { return; };
            let rows = rows_for_select.borrow();
            let Some((_d, row)) = rows.iter().find(|(date, _)| *date == d) else {
                return;
            };
            scroll_row_into_view(&scrolled_for_select, row);
            flash_row_highlight(row);
        });
    }

    let group_for_bind = group.clone();
    let rows_for_bind = rows_track.clone();
    let placeholder_for_bind = placeholder_track.clone();
    let cal_for_bind = cal.clone();
    let events_for_bind = current_events.clone();
    bind(calendar::events(), &group, move |_, evs| {
        for (_date, row) in rows_for_bind.borrow_mut().drain(..) {
            group_for_bind.remove(&row);
        }
        if let Some(p) = placeholder_for_bind.borrow_mut().take() {
            group_for_bind.remove(&p);
        }

        // Stash the snapshot before re-marking so the month-change handlers
        // see the latest events too.
        events_for_bind.borrow_mut().clone_from(&evs);
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

    finish_page(&column)
}

/// Scroll `scrolled` so that `row` is visible. Uses `compute_point` to
/// translate the row's origin into the scrolled-window child's coordinate
/// space; an 8px lead-in keeps the row from butting against the top edge.
fn scroll_row_into_view(scrolled: &gtk::ScrolledWindow, row: &adw::ActionRow) {
    use gtk::prelude::WidgetExt;
    let Some(child) = scrolled.child() else { return; };
    let origin = gtk::graphene::Point::new(0.0, 0.0);
    let Some(point) = row.compute_point(&child, &origin) else { return; };
    let y = f64::from(point.y());
    let adj = scrolled.vadjustment();
    let target = (y - 8.0).max(adj.lower());
    let max = (adj.upper() - adj.page_size()).max(adj.lower());
    adj.set_value(target.min(max));
}

/// Add `.ts-cal-day-hit` to `row` for ~1.5s, then remove it. The CSS rule
/// uses a 600ms transition so the highlight fades rather than flashes.
fn flash_row_highlight(row: &adw::ActionRow) {
    row.add_css_class("ts-cal-day-hit");
    let row_for_clear = row.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
        row_for_clear.remove_css_class("ts-cal-day-hit");
    });
}

/// Mark each day in the calendar's currently-visible month that has at
/// least one event. `GtkCalendar`'s `month()` is 0-indexed; chrono's
/// `NaiveDate::month()` is 1-indexed — adjust before comparing.
fn apply_event_marks(cal: &gtk::Calendar, events: &[CalendarEvent]) {
    cal.clear_marks();
    let month = u32::try_from(cal.month() + 1).unwrap_or(0);
    let year = cal.year();
    let mut marked: HashSet<u32> = HashSet::new();
    for ev in events {
        let d = ev.start.date_naive();
        if d.year() == year && d.month() == month {
            marked.insert(d.day());
        }
    }
    for day in marked {
        cal.mark_day(day);
    }
}

fn build_calendar_row(ev: &CalendarEvent) -> adw::ActionRow {
    // Compose the subtitle from when-string + optional location. Using two
    // lines keeps long venue names from blowing the modal width.
    let when = calendar::format_when(ev);
    let subtitle = match &ev.location {
        Some(loc) => format!("{when}\n{loc}"),
        None => when,
    };

    let row = adw::ActionRow::builder()
        .title(&ev.summary)
        .subtitle(&subtitle)
        .activatable(false)
        .build();
    // Allow the subtitle to wrap when it carries a multi-line location.
    row.set_subtitle_lines(0);
    row.set_title_lines(1);

    let icon = gtk::Image::from_icon_name("x-office-calendar-symbolic");
    icon.set_valign(gtk::Align::Center);
    row.add_prefix(&icon);

    row
}
