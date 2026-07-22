//! Sidebar calendar widget: GNOME-Calendar-style month grid + 7-day
//! upcoming events list. Sibling to the drawer panel
//! `trollshell/src/panels/calendar.rs` — same data source
//! (`hytte::services::calendar::events`), parallel rendering for the
//! always-on left sidebar surface.
//!
//! The month grid is custom-built (not `gtk::Calendar`) so each day cell
//! can hold a row of small dots colored by calendar source, and so the
//! today/selected highlight is a proper Adwaita-style filled pill instead
//! of GTK's default underline. See
//! `docs/superpowers/specs/2026-05-14-calendar-widget-design.md` for the
//! original (`gtk::Calendar`) design — this is the v2 redesign.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, Local, NaiveDate, TimeZone as _, Timelike,
};
use hytte::adw::{self, prelude::*};
use hytte::gtk::{self, glib};
use hytte::prelude::*;
use hytte::services::calendar::{self, CalendarEvent};

/// Stable palette of dot colors. Each calendar source name hashes into one
/// of these indices via [`color_class_for_calendar`]; the CSS rules for
/// `.ts-cal-color-N` live in `style.css`.
const PALETTE_SIZE: usize = 8;

/// Up to this many dots per day cell before we stop appending. Three is
/// enough to convey "busy" without crowding the 36 px cell.
const MAX_DOTS_PER_DAY: usize = 3;

/// Maximum number of event rows shown in the upcoming list (day-section
/// headers do not count toward this cap). Five rows keeps the list compact
/// without a scrollbar while still covering a typical busy day + a peek at
/// the next.
const UPCOMING_LIMIT: usize = 5;

/// Shared state used by every wired handler — render reads from here, the
/// nav/click handlers write to it and trigger a re-render.
#[derive(Clone)]
struct State {
    viewed: Rc<Cell<(i32, u32)>>,
    selected: Rc<Cell<Option<NaiveDate>>>,
    today: Rc<Cell<NaiveDate>>,
    events: Rc<RefCell<Vec<CalendarEvent>>>,
    cells: Rc<Vec<DayCell>>,
    month_label: gtk::Label,
}

/// One slot in the 6×7 day grid. Built once at widget-build time; the
/// `date`/text/css-classes/dot row are rewritten on each render.
struct DayCell {
    button: gtk::Button,
    number: gtk::Label,
    dots: gtk::Box,
    date: Cell<Option<NaiveDate>>,
}

/// Build the sidebar calendar widget. Owns its own subscriptions to
/// `calendar::events()`, `clock::now()`, and `sidebar::open_signal(monitor)`
/// — the last one fires a refresh on each sidebar open so users don't see
/// up-to-60-second-stale data.
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let column = build_block();
    wire_open_refresh(monitor);
    column.upcast()
}

/// Same calendar block used in the drawer's Calendar page. Identical UI to
/// [`widget`] minus the sidebar-open refresh — the drawer triggers
/// `calendar::refresh()` from `modal::on_page_show` instead.
pub fn widget_for_drawer() -> gtk::Widget {
    build_block().upcast()
}

/// Common builder used by both surfaces. Returns the configured column
/// without any monitor-specific wiring.
fn build_block() -> gtk::Box {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.add_css_class("ts-sidebar-calendar");

    let today = Local::now().date_naive();
    let state = State {
        viewed: Rc::new(Cell::new((today.year(), today.month()))),
        selected: Rc::new(Cell::new(None)),
        today: Rc::new(Cell::new(today)),
        events: Rc::new(RefCell::new(Vec::new())),
        cells: Rc::new((0..42).map(|_| build_day_cell()).collect()),
        month_label: gtk::Label::new(None),
    };

    column.append(&build_grid_header(&state));
    column.append(&build_day_names_row());
    column.append(&build_day_grid(&state.cells));

    let upcoming_header = gtk::Label::new(Some("UPCOMING"));
    upcoming_header.add_css_class("ts-sidebar-cal-header");
    upcoming_header.set_halign(gtk::Align::Start);
    column.append(&upcoming_header);

    let group = adw::PreferencesGroup::new();
    group.add_css_class("ts-sidebar-cal-list");
    column.append(&group);

    let rows_track: Rc<RefCell<Vec<(NaiveDate, gtk::Widget)>>> = Rc::new(RefCell::new(Vec::new()));
    let placeholder_track: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));

    wire_day_clicks(&state, &group, &rows_track, &placeholder_track);
    wire_events_bind(&state, &group, &rows_track, &placeholder_track);
    wire_clock_bind(&state, &column, &group, &rows_track, &placeholder_track);

    // Inform the service of the initial viewed month so the first scan
    // covers the current month's past days (issue #100).
    let (iy, im) = state.viewed.get();
    calendar::set_viewed_month(iy, im);

    render(&state);
    column
}

// ── Header (prev | "Month YYYY" | next) ───────────────────────────────────────

fn build_grid_header(state: &State) -> gtk::Box {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    header.add_css_class("ts-cal-header");

    let prev_btn = nav_button("pan-start-symbolic");
    let next_btn = nav_button("pan-end-symbolic");

    state.month_label.add_css_class("ts-cal-month-label");
    state.month_label.set_hexpand(true);
    state.month_label.set_halign(gtk::Align::Center);

    header.append(&prev_btn);
    header.append(&state.month_label);
    header.append(&next_btn);

    let state_prev = state.clone();
    prev_btn.connect_clicked(move |_| {
        let (y, m) = state_prev.viewed.get();
        let (ny, nm) = prev_month(y, m);
        state_prev.viewed.set((ny, nm));
        // Notify the service so it re-scans to cover this month's past days.
        calendar::set_viewed_month(ny, nm);
        render(&state_prev);
    });
    let state_next = state.clone();
    next_btn.connect_clicked(move |_| {
        let (y, m) = state_next.viewed.get();
        let (ny, nm) = next_month(y, m);
        state_next.viewed.set((ny, nm));
        calendar::set_viewed_month(ny, nm);
        render(&state_next);
    });

    header
}

fn nav_button(icon: &str) -> gtk::Button {
    let btn = gtk::Button::from_icon_name(icon);
    btn.add_css_class("ts-cal-nav-btn");
    btn.add_css_class("flat");
    btn
}

// ── Day-name row (MO TU WE TH FR SA SU) ───────────────────────────────────────

fn build_day_names_row() -> gtk::Grid {
    let row = gtk::Grid::new();
    row.add_css_class("ts-cal-daynames");
    row.set_column_homogeneous(true);
    for (i, name) in ["MO", "TU", "WE", "TH", "FR", "SA", "SU"]
        .iter()
        .enumerate()
    {
        let lbl = gtk::Label::new(Some(name));
        lbl.add_css_class("ts-cal-dayname");
        row.attach(&lbl, i32::try_from(i).unwrap_or(0), 0, 1, 1);
    }
    row
}

// ── 6×7 day grid ──────────────────────────────────────────────────────────────

fn build_day_grid(cells: &Rc<Vec<DayCell>>) -> gtk::Grid {
    let grid = gtk::Grid::new();
    grid.add_css_class("ts-cal-grid");
    grid.set_column_homogeneous(true);
    grid.set_row_homogeneous(true);
    grid.set_row_spacing(2);
    grid.set_column_spacing(2);
    for (i, cell) in cells.iter().enumerate() {
        let col = i32::try_from(i % 7).unwrap_or(0);
        let row = i32::try_from(i / 7).unwrap_or(0);
        grid.attach(&cell.button, col, row, 1, 1);
    }
    grid
}

fn build_day_cell() -> DayCell {
    let button = gtk::Button::new();
    button.add_css_class("ts-cal-day");
    button.add_css_class("flat");
    // Don't let the homogeneous grid column stretch the button into a wide
    // rectangle — keep it at its 36×36 min size so the 18px border-radius reads
    // as a circle (the grid itself still spans the full width).
    button.set_halign(gtk::Align::Center);

    let cell_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    cell_box.set_valign(gtk::Align::Center);
    cell_box.set_halign(gtk::Align::Center);

    let number = gtk::Label::new(None);
    number.add_css_class("ts-cal-day-number");
    cell_box.append(&number);

    let dots = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    dots.add_css_class("ts-cal-event-dots");
    dots.set_halign(gtk::Align::Center);
    // Hidden until a day actually has events; an empty-but-visible dots row
    // reserves vertical space that pushes the day number above the circle's
    // centre. `repaint_dots` flips this back on for days with events.
    dots.set_visible(false);
    cell_box.append(&dots);

    button.set_child(Some(&cell_box));
    DayCell {
        button,
        number,
        dots,
        date: Cell::new(None),
    }
}

// ── Click wiring ──────────────────────────────────────────────────────────────

fn wire_day_clicks(
    state: &State,
    group: &adw::PreferencesGroup,
    rows_track: &Rc<RefCell<Vec<(NaiveDate, gtk::Widget)>>>,
    placeholder_track: &Rc<RefCell<Option<adw::ActionRow>>>,
) {
    for (idx, cell) in state.cells.iter().enumerate() {
        let state = state.clone();
        let group = group.clone();
        let rows_track = rows_track.clone();
        let placeholder_track = placeholder_track.clone();
        cell.button.connect_clicked(move |_| {
            let Some(d) = state.cells[idx].date.get() else {
                return;
            };
            on_day_clicked(d, &state, &group, &rows_track, &placeholder_track);
        });
    }
}

fn on_day_clicked(
    date: NaiveDate,
    state: &State,
    group: &adw::PreferencesGroup,
    rows_track: &Rc<RefCell<Vec<(NaiveDate, gtk::Widget)>>>,
    placeholder_track: &Rc<RefCell<Option<adw::ActionRow>>>,
) {
    state.selected.set(Some(date));
    let (vy, vm) = state.viewed.get();
    if date.year() != vy || date.month() != vm {
        let (ny, nm) = (date.year(), date.month());
        state.viewed.set((ny, nm));
        // Month changed — re-scan so the new month's events are loaded.
        calendar::set_viewed_month(ny, nm);
    }
    render(state);
    refresh_upcoming_list(state, group, rows_track, placeholder_track);

    let rows = rows_track.borrow();
    if let Some((_d, row)) = rows.iter().find(|(d, _)| *d == date) {
        flash_row_highlight(row);
    }
}

// ── Upcoming events list ──────────────────────────────────────────────────────

/// Refresh the upcoming-events list AND re-render the grid on every snapshot
/// from `calendar::events()`. The grid uses the same snapshot to draw dots
/// per day, so both consumers share the bind.
fn wire_events_bind(
    state: &State,
    group: &adw::PreferencesGroup,
    rows_track: &Rc<RefCell<Vec<(NaiveDate, gtk::Widget)>>>,
    placeholder_track: &Rc<RefCell<Option<adw::ActionRow>>>,
) {
    let state = state.clone();
    let rows_track = rows_track.clone();
    let placeholder_track = placeholder_track.clone();
    bind(calendar::events(), group, move |group, evs| {
        state.events.borrow_mut().clone_from(&evs);
        refresh_upcoming_list(&state, group, &rows_track, &placeholder_track);
        render(&state);
    });
}

/// Rebuild the upcoming list grouped into day sections, iOS-calendar style
/// (#46): a slim header per day ("Today" / "Tomorrow" / "Wed 18 Jun") with
/// that day's events under it.
///
/// `today` is the real calendar date (used for relative day labels and the
/// placeholder wording); `anchor` is the first day to display — normally
/// equal to `today`, but set to the selected day when the user has clicked a
/// day in the grid (fix for #36). `now` is the live wall-clock read used to
/// drop fully-elapsed events when the anchor is today (#389) — see
/// [`bucket_events_from_anchor`]. The anchor section always leads: when no
/// events fall on that day it shows a "No events" placeholder row.
fn rebuild_upcoming_list(
    group: &adw::PreferencesGroup,
    rows_track: &Rc<RefCell<Vec<(NaiveDate, gtk::Widget)>>>,
    placeholder_track: &Rc<RefCell<Option<adw::ActionRow>>>,
    evs: &[CalendarEvent],
    today: NaiveDate,
    anchor: NaiveDate,
    now: DateTime<Local>,
) {
    for (_d, row) in rows_track.borrow_mut().drain(..) {
        group.remove(&row);
    }
    if let Some(p) = placeholder_track.borrow_mut().take() {
        group.remove(&p);
    }

    if evs.is_empty() {
        let placeholder = adw::ActionRow::builder()
            .title("No upcoming events")
            .subtitle("Add a calendar via Settings \u{2192} Online Accounts.")
            .activatable(false)
            .build();
        group.add(&placeholder);
        *placeholder_track.borrow_mut() = Some(placeholder);
        return;
    }

    let by_day = bucket_events_from_anchor(evs, anchor, now);

    // Lead with the anchor day even when it has no events.
    let mut days: Vec<NaiveDate> = by_day.keys().copied().collect();
    if !by_day.contains_key(&anchor) {
        days.insert(0, anchor);
    }

    let mut new_rows: Vec<(NaiveDate, gtk::Widget)> = Vec::with_capacity(evs.len() + days.len());
    // Count only event rows (not day-section headers) against the cap.
    let mut event_count: usize = 0;
    for day in days {
        // Check the cap before emitting the day header so a day that exactly
        // fills the cap doesn't leave the next day's header dangling with no
        // event rows under it.
        if event_count >= UPCOMING_LIMIT {
            break;
        }

        if let Some(day_evs) = by_day.get(&day) {
            // Day has events: emit the slim section header first, then each
            // event row.
            let header = build_day_header(day, today);
            group.add(&header);
            new_rows.push((day, header.upcast()));
            for ev in day_evs {
                if event_count >= UPCOMING_LIMIT {
                    break;
                }
                let row = build_calendar_row(ev);
                group.add(&row);
                new_rows.push((day, row.upcast()));
                event_count += 1;
            }
        } else {
            // Only the anchor-day section reaches here (every other shown
            // day has at least one event). Collapse the header + "No events"
            // placeholder into a single combined row so there is no dead
            // double-row when today/the selected day is empty.
            let combined = build_empty_day_row(day, today);
            group.add(&combined);
            new_rows.push((day, combined.upcast()));
        }
    }
    *rows_track.borrow_mut() = new_rows;
}

/// Recompute and redraw the Upcoming list from the widget's current state
/// (selected/today anchor, cached events) against a fresh wall-clock read.
/// Shared by the events-signal callback, the day-click handler, and the
/// per-minute clock tick ([`wire_clock_bind`]) so a fully-elapsed event drops
/// off the list wherever any of the three fire next (#389).
fn refresh_upcoming_list(
    state: &State,
    group: &adw::PreferencesGroup,
    rows_track: &Rc<RefCell<Vec<(NaiveDate, gtk::Widget)>>>,
    placeholder_track: &Rc<RefCell<Option<adw::ActionRow>>>,
) {
    let today = state.today.get();
    // Anchor the list to the selected day when one is chosen; fall back to
    // today when nothing is selected (or today itself is selected).
    let anchor = state.selected.get().unwrap_or(today);
    let evs = state.events.borrow();
    rebuild_upcoming_list(
        group,
        rows_track,
        placeholder_track,
        &evs,
        today,
        anchor,
        Local::now(),
    );
}

/// iOS-style day-section label: "Today" / "Tomorrow" / "Wed 18 Jun".
fn day_header_label(day: NaiveDate, today: NaiveDate) -> String {
    match day.signed_duration_since(today).num_days() {
        0 => "Today".to_string(),
        1 => "Tomorrow".to_string(),
        // Code-generated (no user data) so it needs no markup escaping.
        _ => day.format("%a %-d %b").to_string(),
    }
}

/// A slim, muted day-section header — a bare `GtkListBoxRow` wrapping a
/// label, NOT an `AdwActionRow` (whose intrinsic header-box min-height can't
/// be collapsed below a floor, making a 1-line header almost as tall as a
/// 2-line event row — #127). It must be a `GtkListBoxRow`, not a plain
/// `gtk::Label`: `adw_preferences_group_add` routes non-`GtkListBoxRow`
/// children to a box *below* the list, so a bare label would detach the
/// headers from their events instead of interleaving above each day. The
/// `ts-cal-day-header` class sits on the row (font props inherit to the
/// label) so styling and the day-click highlight flash cover the full row.
fn build_day_header(day: NaiveDate, today: NaiveDate) -> gtk::ListBoxRow {
    let label = gtk::Label::new(Some(day_header_label(day, today).as_str()));
    label.set_halign(gtk::Align::Start);
    label.set_xalign(0.0);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&label));
    row.set_activatable(false);
    row.set_selectable(false);
    row.add_css_class("ts-cal-day-header");
    row
}

/// Combined row for an anchor day with no events. Using a single
/// `adw::ActionRow` with title = the day label and subtitle = the empty
/// message collapses what was previously two stacked rows (a day-section
/// header + a separate placeholder) into one, eliminating dead space when
/// today / the selected day has nothing scheduled.
fn build_empty_day_row(day: NaiveDate, today: NaiveDate) -> adw::ActionRow {
    let subtitle = if day == today {
        "No more events today"
    } else {
        "No events"
    };
    let row = adw::ActionRow::builder()
        .title(day_header_label(day, today).as_str())
        .subtitle(subtitle)
        .activatable(false)
        .selectable(false)
        .build();
    row.add_css_class("ts-cal-day-empty");
    row
}

// ── Today rollover + live Upcoming-list refresh ──────────────────────────────

/// Re-render the month grid when the calendar date rolls over (midnight) so
/// "today" stays accurate, and re-filter the Upcoming list once a minute so a
/// fully-elapsed event drops off within a minute of ending rather than
/// lingering until some unrelated re-render (#389). `clock::now()` ticks
/// every second; the manual minute-key check below collapses the other
/// 59/60 ticks to no-ops.
fn wire_clock_bind(
    state: &State,
    anchor: &gtk::Box,
    group: &adw::PreferencesGroup,
    rows_track: &Rc<RefCell<Vec<(NaiveDate, gtk::Widget)>>>,
    placeholder_track: &Rc<RefCell<Option<adw::ActionRow>>>,
) {
    let state = state.clone();
    let group = group.clone();
    let rows_track = rows_track.clone();
    let placeholder_track = placeholder_track.clone();
    let last_minute: Rc<Cell<Option<(NaiveDate, u32)>>> = Rc::new(Cell::new(None));
    bind(hytte::services::clock::now(), anchor, move |_, now| {
        let minute_key = (now.date_naive(), now.hour() * 60 + now.minute());
        if last_minute.replace(Some(minute_key)) == Some(minute_key) {
            return;
        }
        let today = minute_key.0;
        if today != state.today.get() {
            state.today.set(today);
            render(&state);
        }
        refresh_upcoming_list(&state, &group, &rows_track, &placeholder_track);
    });
}

/// Force a fresh scan when the user opens the sidebar — avoids showing
/// up-to-60-second-stale data on open. Edge-triggered via a `Cell` so the
/// initial state replay from `signal()` doesn't fire a refresh when the
/// sidebar starts closed.
fn wire_open_refresh(monitor: &Monitor) {
    let last_open = Rc::new(Cell::new(false));
    glib::MainContext::default().spawn_local(
        crate::overlays::sidebar::open_signal(monitor).for_each(move |open| {
            let prev = last_open.replace(open);
            if open && !prev {
                calendar::refresh();
            }
            async {}
        }),
    );
}

// ── Render ────────────────────────────────────────────────────────────────────

fn render(state: &State) {
    let (year, month) = state.viewed.get();
    state.month_label.set_text(&month_label_text(year, month));

    let today = state.today.get();
    let selected = state.selected.get();
    let events_by_day = group_events_by_day(&state.events.borrow());
    let grid_start = grid_origin(year, month);

    for (i, cell) in state.cells.iter().enumerate() {
        let date = grid_start + ChronoDuration::days(i64::try_from(i).unwrap_or(0));
        paint_cell(cell, date, month, today, selected, events_by_day.get(&date));
    }
}

fn paint_cell(
    cell: &DayCell,
    date: NaiveDate,
    viewed_month: u32,
    today: NaiveDate,
    selected: Option<NaiveDate>,
    sources: Option<&Vec<String>>,
) {
    cell.date.set(Some(date));
    cell.number.set_text(&date.day().to_string());

    for klass in [
        "ts-cal-day-today",
        "ts-cal-day-selected",
        "ts-cal-day-othermonth",
    ] {
        cell.button.remove_css_class(klass);
    }
    if date.month() != viewed_month {
        cell.button.add_css_class("ts-cal-day-othermonth");
    }
    if date == today {
        cell.button.add_css_class("ts-cal-day-today");
    }
    if selected == Some(date) {
        cell.button.add_css_class("ts-cal-day-selected");
    }

    repaint_dots(&cell.dots, sources);
}

fn repaint_dots(dots: &gtk::Box, sources: Option<&Vec<String>>) {
    while let Some(child) = dots.first_child() {
        dots.remove(&child);
    }
    let Some(sources) = sources else {
        dots.set_visible(false);
        return;
    };
    for name in sources.iter().take(MAX_DOTS_PER_DAY) {
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.add_css_class("ts-cal-event-dot");
        dot.add_css_class(color_class_for_calendar(name));
        dots.append(&dot);
    }
    // Only reserve the dots row's vertical space when there's actually a dot to
    // show, so days without events keep the number vertically centred.
    dots.set_visible(!sources.is_empty());
}

// ── Month arithmetic ──────────────────────────────────────────────────────────

fn prev_month(year: i32, month: u32) -> (i32, u32) {
    if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

fn next_month(year: i32, month: u32) -> (i32, u32) {
    if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

fn month_label_text(year: i32, month: u32) -> String {
    NaiveDate::from_ymd_opt(year, month, 1)
        .map(|d| d.format("%B %Y").to_string())
        .unwrap_or_default()
}

/// The Monday on/before the first day of `(year, month)` — the date that
/// goes in cell 0 of the 6×7 grid.
fn grid_origin(year: i32, month: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(year, month, 1).unwrap_or_else(today_or_epoch);
    let dow = i64::from(first.weekday().num_days_from_monday());
    first - ChronoDuration::days(dow)
}

fn today_or_epoch() -> NaiveDate {
    Local::now().date_naive()
}

fn group_events_by_day(events: &[CalendarEvent]) -> HashMap<NaiveDate, Vec<String>> {
    let mut out: HashMap<NaiveDate, Vec<String>> = HashMap::new();
    for ev in events {
        out.entry(ev.start.date_naive())
            .or_default()
            .push(ev.calendar_name.clone());
    }
    out
}

/// Pure helper: group `events` into the day buckets that should appear in the
/// Upcoming list, relative to `anchor` — the first day the list should show.
/// `now` is the live wall-clock read, threaded in (rather than read
/// internally via `Local::now()`) so this stays pure and testable.
///
/// Rules (issue #100 fix — keeps the Upcoming list future-focused; issue
/// #389 — keeps it *live*-focused when the anchor is today):
/// - When `anchor` is the current day (`anchor == now.date_naive()` — the
///   default Upcoming view, whether reached by falling through or by
///   explicitly clicking today in the grid), the skip-threshold is **`now`**
///   itself: an event that has **fully ended** (`end <= now`) drops off the
///   list the moment it's over. An event you're currently in (`end > now`)
///   stays visible until it ends.
/// - When `anchor` is a *different* day — the user navigated to a past/other
///   day via the day grid (#36) — the threshold falls back to **midnight of
///   the anchor day**, so browsing still shows that day's full set instead of
///   hiding events relative to the live clock.
/// - Either way, events that started before the anchor but are still running
///   at the threshold are **clamped** to the anchor day — shown as ongoing
///   under today/the anchor section. Events on or after the anchor land on
///   their own start date.
///
/// The return value is sorted ascending by day. Used by
/// [`rebuild_upcoming_list`] and tested directly (no GTK required).
fn bucket_events_from_anchor<'a>(
    events: &'a [CalendarEvent],
    anchor: NaiveDate,
    now: DateTime<Local>,
) -> BTreeMap<NaiveDate, Vec<&'a CalendarEvent>> {
    // Anchor is "today" (the default forward view) → cut off on the live
    // clock so fully-elapsed events disappear as soon as they're over.
    // Anchor is some other day the user navigated to → cut off on that day's
    // midnight instead, so browsing still shows the full set for that day.
    let threshold = if anchor == now.date_naive() {
        now
    } else {
        Local
            .from_local_datetime(&anchor.and_hms_opt(0, 0, 0).unwrap_or_default())
            .earliest()
            .unwrap_or(now)
    };

    let mut by_day: BTreeMap<NaiveDate, Vec<&'a CalendarEvent>> = BTreeMap::new();
    for ev in events {
        // Skip events that fully ended before or exactly at the threshold.
        // Using <= so an all-day event ending exactly at 00:00 of the anchor
        // day (e.g. a 1-day all-day event on the day before) is excluded.
        if ev.end <= threshold {
            continue;
        }
        // Clamp ongoing events (started before anchor, still running) to anchor.
        let day = ev.start.date_naive().max(anchor);
        by_day.entry(day).or_default().push(ev);
    }
    by_day
}

// ── Calendar-source color hashing ────────────────────────────────────────────

/// Deterministic palette index for a calendar source name (djb2 mod
/// `PALETTE_SIZE`). Two calendars with the same name always hash to the
/// same index — useful when the same source appears across daily renders.
fn color_index_for_calendar(name: &str) -> usize {
    let mut h: u32 = 5381;
    for b in name.bytes() {
        h = h.wrapping_mul(33).wrapping_add(u32::from(b));
    }
    (h as usize) % PALETTE_SIZE
}

fn color_class_for_calendar(name: &str) -> &'static str {
    color_class_for_index(color_index_for_calendar(name))
}

fn color_class_for_index(idx: usize) -> &'static str {
    match idx % PALETTE_SIZE {
        0 => "ts-cal-color-0",
        1 => "ts-cal-color-1",
        2 => "ts-cal-color-2",
        3 => "ts-cal-color-3",
        4 => "ts-cal-color-4",
        5 => "ts-cal-color-5",
        6 => "ts-cal-color-6",
        _ => "ts-cal-color-7",
    }
}

// ── Upcoming-list row builder + helpers ──────────────────────────────────────

/// Format just the time portion of an event for the upcoming-list subtitle:
/// all-day → `"All day"`, timed same-day → `"HH:MM – HH:MM"` (en-dash),
/// timed same start==end → `"HH:MM"`. Location is intentionally excluded
/// (meeting join-URLs cause too much noise in the compact list — issue #101).
fn format_time_subtitle(ev: &CalendarEvent) -> String {
    if ev.all_day {
        return "All day".to_string();
    }
    let start_hm = ev.start.format("%H:%M");
    let end_hm = ev.end.format("%H:%M");
    let start_str = start_hm.to_string();
    let end_str = end_hm.to_string();
    if start_str == end_str {
        start_str
    } else {
        format!("{start_str}\u{2013}{end_str}")
    }
}

/// Build an `adw::ActionRow` for a single calendar event. The prefix is a
/// colored dot keyed by `calendar_name`; the subtitle shows the time only
/// (no location — join-URLs are too noisy in this compact list). The row is
/// activatable: clicking it launches `gnome-calendar` (graceful if absent).
fn build_calendar_row(ev: &CalendarEvent) -> adw::ActionRow {
    let subtitle = format_time_subtitle(ev);

    // AdwActionRow renders title/subtitle as Pango markup, so an unescaped
    // `&`/`<`/`>` in a summary silently blanks the field (#30).
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&ev.summary).as_str())
        .subtitle(glib::markup_escape_text(&subtitle).as_str())
        .activatable(true)
        .build();
    row.set_subtitle_lines(1);
    row.set_title_lines(1);

    row.connect_activated(|_row| {
        launch_gnome_calendar();
    });

    let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    dot.add_css_class("ts-cal-source-dot");
    dot.add_css_class(color_class_for_calendar(&ev.calendar_name));
    dot.set_valign(gtk::Align::Center);
    dot.set_halign(gtk::Align::Center);
    row.add_prefix(&dot);

    row
}

/// Launch `gnome-calendar`. Logs a warning if the binary is not found or
/// the spawn otherwise fails — never panics. The child is reaped in a
/// detached thread so no zombie accumulates in the long-running shell
/// process.
fn launch_gnome_calendar() {
    match std::process::Command::new("gnome-calendar").spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not launch gnome-calendar");
        }
    }
}

fn flash_row_highlight(row: &gtk::Widget) {
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
    use chrono::{TimeZone, Weekday};

    /// Build a `DateTime<Local>` for a test's `now`/event boundaries without
    /// repeating the `from_local_datetime(...).single().unwrap()` dance.
    fn ldt(date: NaiveDate, hour: u32, min: u32, sec: u32) -> DateTime<Local> {
        Local
            .from_local_datetime(&date.and_hms_opt(hour, min, sec).unwrap())
            .single()
            .unwrap()
    }

    #[test]
    fn grid_origin_lands_on_monday_at_or_before_first_of_month() {
        // May 2026 starts on a Friday. The grid's cell 0 should be Mon Apr 27.
        let origin = grid_origin(2026, 5);
        assert_eq!(origin, NaiveDate::from_ymd_opt(2026, 4, 27).unwrap());
        assert_eq!(origin.weekday(), Weekday::Mon);
    }

    #[test]
    fn grid_origin_for_month_starting_monday_is_the_first() {
        // June 2026 starts on a Monday.
        let origin = grid_origin(2026, 6);
        assert_eq!(origin, NaiveDate::from_ymd_opt(2026, 6, 1).unwrap());
    }

    #[test]
    fn prev_next_month_wrap_year_boundaries() {
        assert_eq!(prev_month(2026, 1), (2025, 12));
        assert_eq!(next_month(2026, 12), (2027, 1));
        assert_eq!(prev_month(2026, 5), (2026, 4));
        assert_eq!(next_month(2026, 5), (2026, 6));
    }

    #[test]
    fn color_index_is_deterministic_per_name() {
        let a1 = color_index_for_calendar("work@example.com");
        let a2 = color_index_for_calendar("work@example.com");
        assert_eq!(a1, a2);
        assert!(a1 < PALETTE_SIZE);
    }

    #[test]
    fn color_index_distributes_across_palette() {
        // Sample names should land on more than one index — guards
        // against a hash collapse.
        let names = [
            "personal",
            "work",
            "shared-team",
            "birthdays",
            "holidays",
            "school",
            "fitness",
            "side-project",
        ];
        let mut seen = std::collections::BTreeSet::new();
        for n in names {
            seen.insert(color_index_for_calendar(n));
        }
        assert!(seen.len() > 1, "hash collapsed to one bucket");
    }

    #[test]
    fn month_label_formats_as_full_name_plus_year() {
        assert_eq!(month_label_text(2026, 5), "May 2026");
        assert_eq!(month_label_text(2026, 12), "December 2026");
    }

    // ── bucket_events_from_anchor tests (#36, #100) ───────────────────────────

    fn make_event(start: NaiveDate) -> CalendarEvent {
        let dt = Local
            .from_local_datetime(&start.and_hms_opt(9, 0, 0).unwrap())
            .single()
            .unwrap();
        CalendarEvent {
            uid: start.to_string(),
            summary: "test".into(),
            start: dt,
            end: dt + chrono::Duration::hours(1),
            location: None,
            all_day: false,
            calendar_name: "cal".into(),
        }
    }

    fn make_event_spanning(start: NaiveDate, end: NaiveDate) -> CalendarEvent {
        let start_dt = Local
            .from_local_datetime(&start.and_hms_opt(9, 0, 0).unwrap())
            .single()
            .unwrap();
        let end_dt = Local
            .from_local_datetime(&end.and_hms_opt(17, 0, 0).unwrap())
            .single()
            .unwrap();
        CalendarEvent {
            uid: format!("{start}-{end}"),
            summary: "spanning".into(),
            start: start_dt,
            end: end_dt,
            location: None,
            all_day: false,
            calendar_name: "cal".into(),
        }
    }

    /// Build a 1-day all-day event for `day`. The iCalendar convention for
    /// all-day events is `DTEND = day + 1` at 00:00 (exclusive upper bound).
    fn make_allday_event(day: NaiveDate) -> CalendarEvent {
        let start_dt = Local
            .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .unwrap();
        // iCal DTEND for a 1-day all-day event is the *next* day at 00:00.
        let end_dt = Local
            .from_local_datetime(
                &(day + chrono::Duration::days(1))
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
            )
            .earliest()
            .unwrap();
        CalendarEvent {
            uid: format!("allday-{day}"),
            summary: "all-day".into(),
            start: start_dt,
            end: end_dt,
            location: None,
            all_day: true,
            calendar_name: "cal".into(),
        }
    }

    #[test]
    fn anchor_today_shows_all_upcoming() {
        // When anchor == today (the default), all events are shown on their
        // actual start dates. `now` is midnight so the #389 live-clock
        // threshold doesn't interfere with this pre-existing scenario.
        let today = NaiveDate::from_ymd_opt(2026, 6, 17).unwrap();
        let now = ldt(today, 0, 0, 0);
        let ev_today = make_event(today);
        let ev_tomorrow = make_event(today + chrono::Duration::days(1));
        let ev_next_week = make_event(today + chrono::Duration::days(7));

        let evs = vec![ev_today.clone(), ev_tomorrow.clone(), ev_next_week.clone()];
        let by_day = bucket_events_from_anchor(&evs, today, now);

        let days: Vec<NaiveDate> = by_day.keys().copied().collect();
        assert_eq!(days.len(), 3);
        assert_eq!(days[0], today);
        assert_eq!(days[1], today + chrono::Duration::days(1));
        assert_eq!(days[2], today + chrono::Duration::days(7));
    }

    #[test]
    fn anchor_future_day_excludes_fully_past_short_events() {
        // When user clicks a day 3 days out, a short event (1h) that started
        // and ended on today must be fully excluded — NOT clamped to anchor.
        let today = NaiveDate::from_ymd_opt(2026, 6, 17).unwrap();
        let anchor = today + chrono::Duration::days(3);
        // `now` stays on `today`, distinct from `anchor`, so this exercises
        // the browsed-day (midnight-cutoff) path, not the #389 live-clock one.
        let now = ldt(today, 12, 0, 0);
        // ev_today: 1h event today; fully ends before anchor.
        let ev_today = make_event(today);
        let ev_anchor = make_event(anchor);
        let ev_later = make_event(anchor + chrono::Duration::days(2));

        let evs = vec![ev_today, ev_anchor.clone(), ev_later.clone()];
        let by_day = bucket_events_from_anchor(&evs, anchor, now);

        // ev_today fully ended before anchor → excluded entirely.
        assert!(
            !by_day.contains_key(&today),
            "fully-past event must be excluded from upcoming list"
        );
        // anchor and anchor+2d should be present.
        assert!(by_day.contains_key(&anchor));
        assert!(by_day.contains_key(&(anchor + chrono::Duration::days(2))));
        // No day before the anchor.
        for d in by_day.keys() {
            assert!(*d >= anchor, "found pre-anchor day {d}");
        }
    }

    #[test]
    fn anchor_multiday_ongoing_event_buckets_under_anchor() {
        // A multi-day event that started before the anchor but ends on/after
        // it (e.g. a conference that began yesterday) should appear under the
        // anchor day — it is still ongoing.
        let today = NaiveDate::from_ymd_opt(2026, 6, 17).unwrap();
        let anchor = today + chrono::Duration::days(2);
        let now = ldt(today, 12, 0, 0);

        // Event spans today → anchor+1: it is still running at the anchor.
        let ev_ongoing = make_event_spanning(today, anchor + chrono::Duration::days(1));

        let evs = vec![ev_ongoing];
        let by_day = bucket_events_from_anchor(&evs, anchor, now);

        // Must be bucketed under anchor, not under today.
        assert!(
            by_day.contains_key(&anchor),
            "ongoing event must appear under anchor"
        );
        assert!(
            !by_day.contains_key(&today),
            "ongoing event must not appear under pre-anchor day"
        );
    }

    #[test]
    fn anchor_fully_past_multiday_event_excluded() {
        // A multi-day event that ended before the anchor is fully past and
        // must be excluded from the list (issue #100: past events in the feed
        // must not pollute the Upcoming list).
        let today = NaiveDate::from_ymd_opt(2026, 6, 17).unwrap();
        let anchor = today + chrono::Duration::days(3);
        let now = ldt(today, 12, 0, 0);

        // Event ended yesterday (before anchor).
        let ev_past = make_event_spanning(
            today - chrono::Duration::days(2),
            today - chrono::Duration::days(1),
        );

        let evs = vec![ev_past];
        let by_day = bucket_events_from_anchor(&evs, anchor, now);

        assert!(
            by_day.is_empty(),
            "fully-past multi-day event must be excluded"
        );
    }

    /// Regression for the strict-`<` bug: a 1-day all-day event on the day
    /// before the anchor has `end == anchor 00:00 == anchor_day_start`.  With
    /// the old `<` it leaked into the anchor section; with `<=` it is excluded.
    #[test]
    fn allday_event_ending_exactly_at_anchor_midnight_is_excluded() {
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 17).unwrap();
        let yesterday = anchor - chrono::Duration::days(1);
        // `now` is 5 days after the browsed anchor, so this exercises the
        // midnight-of-anchor path, not the #389 live-clock one.
        let now = ldt(anchor + chrono::Duration::days(5), 12, 0, 0);
        // 1-day all-day event for yesterday: end = anchor 00:00.
        let ev_yesterday = make_allday_event(yesterday);
        let evs = [ev_yesterday];
        let by_day = bucket_events_from_anchor(&evs, anchor, now);
        assert!(
            by_day.is_empty(),
            "all-day event ending exactly at anchor midnight must be excluded"
        );
    }

    /// Complement: a 1-day all-day event *on* the anchor day has
    /// `end == anchor+1 00:00 > anchor_day_start` — it must be kept.
    #[test]
    fn allday_event_on_anchor_day_is_kept() {
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 17).unwrap();
        let now = ldt(anchor + chrono::Duration::days(5), 12, 0, 0);
        let ev_anchor = make_allday_event(anchor);
        let evs = [ev_anchor];
        let by_day = bucket_events_from_anchor(&evs, anchor, now);
        assert!(
            by_day.contains_key(&anchor),
            "all-day event on the anchor day must appear in the list"
        );
    }

    #[test]
    fn anchor_empty_events_gives_empty_map() {
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 17).unwrap();
        let now = ldt(anchor, 0, 0, 0);
        let by_day = bucket_events_from_anchor(&[], anchor, now);
        assert!(by_day.is_empty());
    }

    // ── #389: hide fully-past events from the "today" Upcoming view ──────────

    #[test]
    fn anchor_today_now_threshold_hides_fully_elapsed_event() {
        // When anchor is today, an event that has fully ended relative to the
        // live clock must be hidden even though it's well after the anchor's
        // midnight (the pre-#389 cutoff).
        let today = NaiveDate::from_ymd_opt(2026, 6, 17).unwrap();
        let ev_earlier = make_event(today); // 09:00–10:00
        let now = ldt(today, 14, 0, 0); // 14:00 — event is long over.

        let evs = vec![ev_earlier];
        let by_day = bucket_events_from_anchor(&evs, today, now);

        assert!(
            by_day.is_empty(),
            "fully-elapsed event on the anchor day must be hidden once past `now`"
        );
    }

    #[test]
    fn anchor_today_now_threshold_keeps_in_progress_event() {
        // An event you're currently in (end > now) must stay visible until it
        // ends — the triage's chosen semantics (cutoff on `end`, not `start`).
        let today = NaiveDate::from_ymd_opt(2026, 6, 17).unwrap();
        let ev_in_progress = make_event(today); // 09:00–10:00
        let now = ldt(today, 9, 30, 0); // 09:30 — mid-event.

        let evs = vec![ev_in_progress];
        let by_day = bucket_events_from_anchor(&evs, today, now);

        assert!(
            by_day.contains_key(&today),
            "in-progress event must remain visible until it ends"
        );
    }

    #[test]
    fn anchor_today_event_ending_exactly_at_now_is_hidden() {
        // `end <= now` (not strict `<`) so an event ending at this instant
        // already reads as over, matching the anchor-midnight `<=` convention.
        let today = NaiveDate::from_ymd_opt(2026, 6, 17).unwrap();
        let ev = make_event(today); // ends 10:00
        let now = ldt(today, 10, 0, 0);

        let evs = vec![ev];
        let by_day = bucket_events_from_anchor(&evs, today, now);

        assert!(
            by_day.is_empty(),
            "event ending exactly at `now` must be hidden"
        );
    }

    #[test]
    fn browsing_past_day_uses_midnight_not_live_clock() {
        // Navigating to a past/other day (anchor != now's date) must keep the
        // midnight-of-anchor cutoff — browsing still shows that day's full
        // set even though the event is long over relative to the live clock.
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        let ev = make_event(anchor); // 09:00–10:00 on the browsed day.
        let now = ldt(anchor + chrono::Duration::days(5), 14, 0, 0);

        let evs = vec![ev];
        let by_day = bucket_events_from_anchor(&evs, anchor, now);

        assert!(
            by_day.contains_key(&anchor),
            "browsed day's events must stay visible regardless of the live clock"
        );
    }
}
