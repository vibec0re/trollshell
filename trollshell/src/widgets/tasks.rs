//! Sidebar tasks widget. Subscribes to
//! [`hytte::services::tasks::tasks()`] and renders incomplete VTODOs from
//! every EDS task list. Rows in the editable list ([`EDITABLE_LIST_UID`])
//! get full CRUD affordances — checkbox, click-to-edit, context delete —
//! while rows from other (CalDAV/Google/etc.) lists render read-only.
//!
//! ## Layout
//!
//! Same shape as the calendar widget's "UPCOMING" block: a small header
//! row with title + add-button, then a scrolled list of rows. Sits
//! immediately under the calendar widget in `overlays::sidebar::build_card`.

use std::cell::RefCell;
use std::rc::Rc;

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveTime, TimeZone};
use hytte::adw::{self, prelude::*};
use hytte::gtk::{self, glib};
use hytte::prelude::*;
use hytte::services::tasks::{self, Task};

/// Build the sidebar tasks widget. Owns its own subscription to
/// `tasks::tasks()`; refreshes on each sidebar open like the calendar
/// widget so the user never sees up-to-60-second-stale data.
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let column = build_block();
    wire_open_refresh(monitor);
    column.upcast()
}

fn build_block() -> gtk::Box {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.add_css_class("ts-sidebar-tasks");

    column.append(&build_header());

    let group = adw::PreferencesGroup::new();
    group.add_css_class("ts-sidebar-tasks-list");

    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_min_content_height(160);
    scrolled.set_max_content_height(320);
    scrolled.set_child(Some(&group));
    column.append(&scrolled);

    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let placeholder_track: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));
    wire_tasks_bind(&group, &rows_track, &placeholder_track);

    column
}

// ── Header row ───────────────────────────────────────────────────────────────

fn build_header() -> gtk::Box {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    header.add_css_class("ts-sidebar-tasks-header");

    let title = gtk::Label::new(Some("TASKS"));
    title.add_css_class("ts-sidebar-cal-header");
    title.set_halign(gtk::Align::Start);
    title.set_hexpand(true);
    title.set_xalign(0.0);
    header.append(&title);

    let add_btn = gtk::MenuButton::new();
    add_btn.set_icon_name("list-add-symbolic");
    add_btn.add_css_class("flat");
    add_btn.add_css_class("ts-sidebar-tasks-add-btn");
    add_btn.set_popover(Some(&build_create_popover(&add_btn)));
    header.append(&add_btn);

    header
}

// ── Bind: list re-renders on each tasks() emission ───────────────────────────

fn wire_tasks_bind(
    group: &adw::PreferencesGroup,
    rows_track: &Rc<RefCell<Vec<adw::ActionRow>>>,
    placeholder_track: &Rc<RefCell<Option<adw::ActionRow>>>,
) {
    let rows_track = rows_track.clone();
    let placeholder_track = placeholder_track.clone();
    bind(tasks::tasks(), group, move |group, ts| {
        rebuild_list(group, &rows_track, &placeholder_track, &ts);
    });
}

fn rebuild_list(
    group: &adw::PreferencesGroup,
    rows_track: &Rc<RefCell<Vec<adw::ActionRow>>>,
    placeholder_track: &Rc<RefCell<Option<adw::ActionRow>>>,
    ts: &[Task],
) {
    for row in rows_track.borrow_mut().drain(..) {
        group.remove(&row);
    }
    if let Some(p) = placeholder_track.borrow_mut().take() {
        group.remove(&p);
    }

    if ts.is_empty() {
        let placeholder = adw::ActionRow::builder()
            .title("No tasks")
            .subtitle("Tap + to add one.")
            .activatable(false)
            .build();
        group.add(&placeholder);
        *placeholder_track.borrow_mut() = Some(placeholder);
        return;
    }

    let mut new_rows: Vec<adw::ActionRow> = Vec::with_capacity(ts.len());
    for t in ts {
        let row = build_task_row(t);
        group.add(&row);
        new_rows.push(row);
    }
    *rows_track.borrow_mut() = new_rows;
}

// ── One row ──────────────────────────────────────────────────────────────────

fn build_task_row(task: &Task) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&task.summary).as_str())
        .activatable(task.editable)
        .build();
    row.add_css_class("ts-task-row");
    let subtitle = subtitle_text(task);
    if !subtitle.is_empty() {
        row.set_subtitle(&glib::markup_escape_text(&subtitle));
    }

    // Checkbox prefix — wired to set_completed for editable rows, disabled
    // for read-only ones. Mark NeedsAction+InProcess rows as not-yet-done
    // (we only get those from the service).
    let check = gtk::CheckButton::new();
    check.set_valign(gtk::Align::Center);
    check.set_active(false);
    check.set_sensitive(task.editable);
    if task.editable {
        let uid = task.uid.clone();
        check.connect_toggled(move |c| {
            tasks::set_completed(&uid, c.is_active());
        });
    }
    row.add_prefix(&check);

    // Tap-to-edit on the row body for editable rows. AdwActionRow's own
    // `activated` signal fires when the body (not the checkbox) is
    // clicked.
    if task.editable {
        let task = task.clone();
        row.connect_activated(move |r| {
            open_edit_popover(r, &task);
        });
    }

    if !task.editable {
        // Tiny lock indicator on non-editable rows so the user knows why
        // their checkbox doesn't budge.
        let lock = gtk::Image::from_icon_name("changes-prevent-symbolic");
        lock.add_css_class("dim-label");
        lock.set_tooltip_text(Some(&format!(
            "Read-only — lives in '{}'. Edits happen in the Trollshell Tasks list.",
            task.list_name,
        )));
        row.add_suffix(&lock);
    }

    row
}

/// Subtitle: due label, optionally followed by ` · <list name>` when the
/// task lives outside the editable list. Skipped entirely when both parts
/// would be empty so the row stays compact.
fn subtitle_text(task: &Task) -> String {
    let due = tasks::format_due(task);
    match (due.is_empty(), task.editable) {
        (true, true) => String::new(),
        (true, false) => task.list_name.clone(),
        (false, true) => due,
        (false, false) => format!("{due} \u{00b7} {}", task.list_name),
    }
}

// ── Create popover (add button) ──────────────────────────────────────────────

fn build_create_popover(anchor: &gtk::MenuButton) -> gtk::Popover {
    let popover = gtk::Popover::new();
    popover.add_css_class("ts-task-popover");

    let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
    column.set_margin_top(8);
    column.set_margin_bottom(8);
    column.set_margin_start(8);
    column.set_margin_end(8);
    column.set_width_request(260);

    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some("New task…"));
    entry.add_css_class("ts-task-entry");
    column.append(&entry);

    let due_picker = DuePicker::new();
    column.append(due_picker.widget());

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    cancel.add_css_class("flat");
    let create = gtk::Button::with_label("Add");
    create.add_css_class("suggested-action");
    create.set_sensitive(false);
    actions.append(&cancel);
    actions.append(&create);
    column.append(&actions);

    // Enable Create button only when the entry has non-whitespace text.
    let create_for_changed = create.clone();
    entry.connect_changed(move |e| {
        create_for_changed.set_sensitive(!e.text().trim().is_empty());
    });

    // Cancel: just close the popover and reset state.
    let popover_for_cancel = popover.clone();
    let entry_for_cancel = entry.clone();
    let due_picker_for_cancel = due_picker.clone();
    cancel.connect_clicked(move |_| {
        popover_for_cancel.popdown();
        entry_for_cancel.set_text("");
        due_picker_for_cancel.reset();
    });

    // Create.
    let popover_for_create = popover.clone();
    let entry_for_create = entry.clone();
    let anchor_for_create = anchor.clone();
    let due_picker_for_create = due_picker.clone();
    let do_create = move || {
        let summary = entry_for_create.text().trim().to_string();
        if summary.is_empty() {
            return;
        }
        let _ = tasks::create_task(summary, due_picker_for_create.value());
        entry_for_create.set_text("");
        due_picker_for_create.reset();
        popover_for_create.popdown();
        anchor_for_create.grab_focus();
    };
    let do_create_for_button = do_create.clone();
    create.connect_clicked(move |_| do_create_for_button());

    let do_create_for_entry = do_create;
    entry.connect_activate(move |_| do_create_for_entry());

    // Reset state every time the popover opens.
    let entry_for_show = entry.clone();
    let due_picker_for_show = due_picker.clone();
    popover.connect_show(move |_| {
        entry_for_show.set_text("");
        due_picker_for_show.reset();
        entry_for_show.grab_focus();
    });

    popover.set_child(Some(&column));
    popover
}

// ── Edit popover (row body click) ────────────────────────────────────────────

fn open_edit_popover(parent: &adw::ActionRow, task: &Task) {
    let popover = gtk::Popover::new();
    popover.add_css_class("ts-task-popover");
    popover.set_parent(parent);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
    column.set_margin_top(8);
    column.set_margin_bottom(8);
    column.set_margin_start(8);
    column.set_margin_end(8);
    column.set_width_request(260);

    let entry = gtk::Entry::new();
    entry.set_text(&task.summary);
    column.append(&entry);

    let due_picker = DuePicker::new();
    due_picker.set_value(task.due, task.due_all_day);
    column.append(due_picker.widget());

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_halign(gtk::Align::End);
    let delete = gtk::Button::with_label("Delete");
    delete.add_css_class("destructive-action");
    delete.add_css_class("flat");
    let cancel = gtk::Button::with_label("Cancel");
    cancel.add_css_class("flat");
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    actions.append(&delete);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    actions.append(&spacer);
    actions.append(&cancel);
    actions.append(&save);
    column.append(&actions);

    let popover_for_save = popover.clone();
    let entry_for_save = entry.clone();
    let due_picker_for_save = due_picker.clone();
    let uid_for_save = task.uid.clone();
    let do_save = move || {
        let summary = entry_for_save.text().trim().to_string();
        if summary.is_empty() {
            return;
        }
        tasks::edit_task(&uid_for_save, summary, due_picker_for_save.value());
        popover_for_save.popdown();
    };
    let do_save_for_button = do_save.clone();
    save.connect_clicked(move |_| do_save_for_button());

    let do_save_for_entry = do_save;
    entry.connect_activate(move |_| do_save_for_entry());

    let popover_for_cancel = popover.clone();
    cancel.connect_clicked(move |_| popover_for_cancel.popdown());

    let popover_for_delete = popover.clone();
    let uid_for_delete = task.uid.clone();
    delete.connect_clicked(move |_| {
        tasks::delete_task(&uid_for_delete);
        popover_for_delete.popdown();
    });

    // Detach the popover from `parent` once closed so each click builds
    // a fresh one — keeps state hygiene simple and avoids accumulating
    // popovers on the row across edits.
    popover.connect_closed(|p| {
        p.unparent();
    });

    popover.popup();
    entry.grab_focus();
}

// ── Inline due picker (used by both popovers) ────────────────────────────────

/// Compact mini-picker: "No date" / "Today" / "Tomorrow" / "Pick…" toggle
/// row. The "Pick…" branch surfaces a `gtk::Calendar` underneath. Dates
/// are stored as local-midnight (all-day semantics); the service maps
/// them back into a UTC DTSTART on write.
#[derive(Clone)]
struct DuePicker {
    container: gtk::Box,
    mode: Rc<RefCell<DueMode>>,
    selected: Rc<RefCell<Option<NaiveDate>>>,
    summary_label: gtk::Label,
    calendar: gtk::Calendar,
    calendar_wrap: gtk::Revealer,
    // Chip references kept so [`Self::set_value`] can sync the visible
    // toggle when seeding the picker from an existing task.
    chip_none: gtk::ToggleButton,
    chip_today: gtk::ToggleButton,
    chip_tomorrow: gtk::ToggleButton,
    chip_pick: gtk::ToggleButton,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DueMode {
    None,
    Today,
    Tomorrow,
    Pick,
}

impl DuePicker {
    fn new() -> Self {
        let container = gtk::Box::new(gtk::Orientation::Vertical, 6);
        container.add_css_class("ts-task-due-picker");

        let chips = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        chips.add_css_class("linked");

        let none_btn = gtk::ToggleButton::with_label("No date");
        let today_btn = gtk::ToggleButton::with_label("Today");
        let tomorrow_btn = gtk::ToggleButton::with_label("Tomorrow");
        let pick_btn = gtk::ToggleButton::with_label("Pick\u{2026}");
        for b in [&none_btn, &today_btn, &tomorrow_btn, &pick_btn] {
            b.add_css_class("ts-task-due-chip");
        }
        // Group so exactly one stays toggled at a time.
        today_btn.set_group(Some(&none_btn));
        tomorrow_btn.set_group(Some(&none_btn));
        pick_btn.set_group(Some(&none_btn));
        none_btn.set_active(true);

        chips.append(&none_btn);
        chips.append(&today_btn);
        chips.append(&tomorrow_btn);
        chips.append(&pick_btn);
        container.append(&chips);

        let summary_label = gtk::Label::new(None);
        summary_label.add_css_class("dim-label");
        summary_label.add_css_class("ts-task-due-summary");
        summary_label.set_halign(gtk::Align::Start);
        summary_label.set_visible(false);
        container.append(&summary_label);

        let calendar = gtk::Calendar::new();
        let calendar_wrap = gtk::Revealer::new();
        calendar_wrap.set_transition_type(gtk::RevealerTransitionType::SlideDown);
        calendar_wrap.set_transition_duration(140);
        calendar_wrap.set_child(Some(&calendar));
        container.append(&calendar_wrap);

        let picker = Self {
            container,
            mode: Rc::new(RefCell::new(DueMode::None)),
            selected: Rc::new(RefCell::new(None)),
            summary_label,
            calendar: calendar.clone(),
            calendar_wrap: calendar_wrap.clone(),
            chip_none: none_btn.clone(),
            chip_today: today_btn.clone(),
            chip_tomorrow: tomorrow_btn.clone(),
            chip_pick: pick_btn.clone(),
        };

        // Hook up the chips.
        let p = picker.clone();
        none_btn.connect_toggled(move |b| if b.is_active() { p.set_mode(DueMode::None); });
        let p = picker.clone();
        today_btn.connect_toggled(move |b| if b.is_active() { p.set_mode(DueMode::Today); });
        let p = picker.clone();
        tomorrow_btn.connect_toggled(move |b| if b.is_active() { p.set_mode(DueMode::Tomorrow); });
        let p = picker.clone();
        pick_btn.connect_toggled(move |b| if b.is_active() { p.set_mode(DueMode::Pick); });

        let p = picker.clone();
        calendar.connect_day_selected(move |c| {
            let y = c.year();
            let m = c.month() + 1; // gtk::Calendar months are 0-indexed
            let d = c.day();
            if let (Ok(m_u32), Ok(d_u32)) = (u32::try_from(m), u32::try_from(d))
                && let Some(date) = NaiveDate::from_ymd_opt(y, m_u32, d_u32) {
                *p.selected.borrow_mut() = Some(date);
                p.refresh_summary();
            }
        });

        picker
    }

    fn widget(&self) -> &gtk::Box {
        &self.container
    }

    fn set_mode(&self, mode: DueMode) {
        *self.mode.borrow_mut() = mode;
        match mode {
            DueMode::None | DueMode::Today | DueMode::Tomorrow => {
                self.calendar_wrap.set_reveal_child(false);
            }
            DueMode::Pick => {
                self.calendar_wrap.set_reveal_child(true);
                // Seed the calendar with today if nothing selected yet.
                if self.selected.borrow().is_none() {
                    let today = Local::now().date_naive();
                    *self.selected.borrow_mut() = Some(today);
                    if let Some(dt) = glib_date(today) {
                        self.calendar.select_day(&dt);
                    }
                }
            }
        }
        self.refresh_summary();
    }

    fn refresh_summary(&self) {
        match *self.mode.borrow() {
            DueMode::None => {
                self.summary_label.set_visible(false);
            }
            DueMode::Today => {
                self.summary_label.set_visible(true);
                self.summary_label.set_text("Due today");
            }
            DueMode::Tomorrow => {
                self.summary_label.set_visible(true);
                self.summary_label.set_text("Due tomorrow");
            }
            DueMode::Pick => {
                let label = self
                    .selected
                    .borrow()
                    .map_or_else(|| "Pick a date".to_string(), |d| format!("Due {}", short_date(d)));
                self.summary_label.set_visible(true);
                self.summary_label.set_text(&label);
            }
        }
    }

    /// Current value as a local-midnight `DateTime<Local>`. Returns `None`
    /// when the picker is in "No date" mode.
    fn value(&self) -> Option<DateTime<Local>> {
        let date = match *self.mode.borrow() {
            DueMode::None => return None,
            DueMode::Today => Local::now().date_naive(),
            DueMode::Tomorrow => Local::now().date_naive() + chrono::Duration::days(1),
            DueMode::Pick => (*self.selected.borrow())?,
        };
        Local
            .from_local_datetime(&date.and_time(NaiveTime::from_hms_opt(0, 0, 0)?))
            .single()
    }

    /// Seed the picker from an existing task. Picks "Today"/"Tomorrow" when
    /// the date matches; otherwise falls to "Pick" with the date selected.
    /// Flips the visible chip in sync so the popover doesn't open with
    /// `mode = Today` but the "No date" chip still pressed.
    fn set_value(&self, due: Option<DateTime<Local>>, _all_day: bool) {
        let mode = match due {
            None => DueMode::None,
            Some(dt) => {
                let date = dt.date_naive();
                let today = Local::now().date_naive();
                if date == today {
                    DueMode::Today
                } else if date == today + chrono::Duration::days(1) {
                    DueMode::Tomorrow
                } else {
                    *self.selected.borrow_mut() = Some(date);
                    if let Some(gdt) = glib_date(date) {
                        self.calendar.select_day(&gdt);
                    }
                    DueMode::Pick
                }
            }
        };
        *self.mode.borrow_mut() = mode;
        self.set_active_chip(mode);
        self.calendar_wrap
            .set_reveal_child(mode == DueMode::Pick);
        self.refresh_summary();
    }

    /// Flip the matching chip on; `set_group` deactivates the siblings.
    /// The resulting `toggled` signal fires the chip's handler which
    /// re-runs `set_mode` — that's idempotent (same mode is already
    /// stored), so the redundancy is harmless.
    fn set_active_chip(&self, mode: DueMode) {
        let target = match mode {
            DueMode::None => &self.chip_none,
            DueMode::Today => &self.chip_today,
            DueMode::Tomorrow => &self.chip_tomorrow,
            DueMode::Pick => &self.chip_pick,
        };
        target.set_active(true);
    }

    fn reset(&self) {
        *self.mode.borrow_mut() = DueMode::None;
        *self.selected.borrow_mut() = None;
        self.calendar_wrap.set_reveal_child(false);
        self.refresh_summary();
    }
}

fn glib_date(d: NaiveDate) -> Option<glib::DateTime> {
    let m = i32::try_from(d.month()).ok()?;
    let day = i32::try_from(d.day()).ok()?;
    glib::DateTime::from_local(d.year(), m, day, 0, 0, 0.0).ok()
}

fn short_date(d: NaiveDate) -> String {
    let today = Local::now().date_naive();
    let delta = d.signed_duration_since(today).num_days();
    match delta {
        0 => "today".to_string(),
        1 => "tomorrow".to_string(),
        _ => format!("{} {} {}", weekday(d), d.day(), month(d.month())),
    }
}

fn weekday(d: NaiveDate) -> &'static str {
    match d.weekday() {
        chrono::Weekday::Mon => "Mon",
        chrono::Weekday::Tue => "Tue",
        chrono::Weekday::Wed => "Wed",
        chrono::Weekday::Thu => "Thu",
        chrono::Weekday::Fri => "Fri",
        chrono::Weekday::Sat => "Sat",
        chrono::Weekday::Sun => "Sun",
    }
}

fn month(m: u32) -> &'static str {
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

// ── Open-refresh ─────────────────────────────────────────────────────────────

/// Force a fresh scan when the user opens the sidebar — mirrors the
/// calendar widget's `wire_open_refresh`. Edge-triggered so the initial
/// `false` from `signal()` doesn't fire a refresh against a still-closed
/// sidebar.
fn wire_open_refresh(monitor: &Monitor) {
    use std::cell::Cell;
    let last_open = Rc::new(Cell::new(false));
    glib::MainContext::default().spawn_local(
        crate::overlays::sidebar::open_signal(monitor).for_each(move |open| {
            let prev = last_open.replace(open);
            if open && !prev {
                tasks::refresh();
            }
            async {}
        }),
    );
}
