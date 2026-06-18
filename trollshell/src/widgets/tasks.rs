//! Sidebar tasks widget. Subscribes to
//! [`hytte::services::tasks::tasks()`] and renders incomplete VTODOs from
//! every EDS task list. Reads + writes go through libecal via
//! [`hytte_services::tasks`], so the widget treats every task (local
//! and remote-synced alike) as editable — the service handles per-
//! backend transport.
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
use hytte::services::tasks::{self, Task, TaskList};

/// Build the sidebar tasks widget. Owns its own subscription to
/// `tasks::tasks()`; refreshes on each sidebar open like the calendar
/// widget so the user never sees up-to-60-second-stale data.
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let column = build_block(monitor);
    wire_open_refresh(monitor);
    column.upcast()
}

/// Maximum number of task rows rendered inline. The list isn't
/// scrollable (an inner SW inside the sidebar swallowed mouse-wheel
/// events for nested scroll, and felt visually cramped) — anything
/// past this surfaces as a "+N more" indicator.
const MAX_VISIBLE_TASKS: usize = 5;

fn build_block(monitor: &Monitor) -> gtk::Box {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.add_css_class("ts-sidebar-tasks");

    // Shared track of the currently-known task lists. The header's create
    // popover binds to this to render its list-picker dropdown; refreshes
    // come in via [`tasks::task_lists`].
    let lists_track: Rc<RefCell<Vec<TaskList>>> = Rc::new(RefCell::new(Vec::new()));

    column.append(&build_header(&lists_track, monitor));

    let group = adw::PreferencesGroup::new();
    group.add_css_class("ts-sidebar-tasks-list");
    column.append(&group);

    let rows_track: Rc<RefCell<Vec<adw::PreferencesRow>>> = Rc::new(RefCell::new(Vec::new()));
    let placeholder_track: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));
    let overflow_track: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));
    wire_tasks_bind(
        &group,
        &rows_track,
        &placeholder_track,
        &overflow_track,
        monitor,
    );
    wire_lists_bind(&group, &lists_track);

    column
}

/// Track the latest `tasks::task_lists()` snapshot on `lists_track` so
/// the create popover always picks from a current set. We anchor the
/// bind to a long-lived widget that lives at least as long as the
/// popover (the prefs group). No widget mutation needed — just keep the
/// `Rc<RefCell<Vec<TaskList>>>` warm.
fn wire_lists_bind(anchor: &adw::PreferencesGroup, lists_track: &Rc<RefCell<Vec<TaskList>>>) {
    let lists_track = lists_track.clone();
    bind(tasks::task_lists(), anchor, move |_, ls| {
        *lists_track.borrow_mut() = ls;
    });
}

// ── Header row ───────────────────────────────────────────────────────────────

fn build_header(lists_track: &Rc<RefCell<Vec<TaskList>>>, monitor: &Monitor) -> gtk::Box {
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
    add_btn.set_popover(Some(&build_create_popover(&add_btn, lists_track, monitor)));
    header.append(&add_btn);

    header
}

// ── Bind: list re-renders on each tasks() emission ───────────────────────────

fn wire_tasks_bind(
    group: &adw::PreferencesGroup,
    rows_track: &Rc<RefCell<Vec<adw::PreferencesRow>>>,
    placeholder_track: &Rc<RefCell<Option<adw::ActionRow>>>,
    overflow_track: &Rc<RefCell<Option<adw::ActionRow>>>,
    monitor: &Monitor,
) {
    let rows_track = rows_track.clone();
    let placeholder_track = placeholder_track.clone();
    let overflow_track = overflow_track.clone();
    let monitor = monitor.clone();
    bind(tasks::tasks(), group, move |group, ts| {
        rebuild_list(
            group,
            &rows_track,
            &placeholder_track,
            &overflow_track,
            &ts,
            &monitor,
        );
    });
}

fn rebuild_list(
    group: &adw::PreferencesGroup,
    rows_track: &Rc<RefCell<Vec<adw::PreferencesRow>>>,
    placeholder_track: &Rc<RefCell<Option<adw::ActionRow>>>,
    overflow_track: &Rc<RefCell<Option<adw::ActionRow>>>,
    ts: &[Task],
    monitor: &Monitor,
) {
    for row in rows_track.borrow_mut().drain(..) {
        group.remove(&row);
    }
    if let Some(p) = placeholder_track.borrow_mut().take() {
        group.remove(&p);
    }
    if let Some(o) = overflow_track.borrow_mut().take() {
        group.remove(&o);
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

    let visible = ts.len().min(MAX_VISIBLE_TASKS);
    let mut new_rows: Vec<adw::PreferencesRow> = Vec::with_capacity(visible);
    for t in ts.iter().take(visible) {
        let row = build_task_row(t, monitor);
        group.add(&row);
        new_rows.push(row);
    }
    *rows_track.borrow_mut() = new_rows;

    if ts.len() > visible {
        let hidden = ts.len() - visible;
        let overflow = adw::ActionRow::builder()
            .title(format!("+ {hidden} more"))
            .subtitle("Open in Evolution / GNOME To Do for the full list.")
            .activatable(false)
            .build();
        overflow.add_css_class("dim-label");
        group.add(&overflow);
        *overflow_track.borrow_mut() = Some(overflow);
    }
}

// ── One row ──────────────────────────────────────────────────────────────────

/// Build a custom two-line task row as an `adw::PreferencesRow`.
///
/// Layout:
/// ```text
/// ┌─────────────────────────────────────────────────────┐
/// │ [✓] Bold title            [due chip]  [list badge]  │
/// │     Note / description line (ellipsized, muted)     │
/// └─────────────────────────────────────────────────────┘
/// ```
///
/// Using `adw::PreferencesRow` (an `AdwPreferencesRow` subclass of
/// `GtkListBoxRow`) ensures `adw::PreferencesGroup::add` inserts the row
/// directly rather than double-wrapping it in an anonymous
/// `GtkListBoxRow`. The row is `activatable`; clicking its body (anywhere
/// except the checkbox) opens the edit popover. The checkbox fires
/// `set_completed` directly.
fn build_task_row(task: &Task, monitor: &Monitor) -> adw::PreferencesRow {
    let row = adw::PreferencesRow::new();
    row.add_css_class("ts-task-row");
    row.set_activatable(true);

    // Outer padding box — gives the row the same inset as adw::ActionRow.
    let outer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    outer.set_margin_top(8);
    outer.set_margin_bottom(8);
    outer.set_margin_start(12);
    outer.set_margin_end(12);
    outer.set_valign(gtk::Align::Center);

    // Checkbox — wired to set_completed. NeedsAction + InProcess both
    // render unchecked (those are the only states the service surfaces).
    let check = gtk::CheckButton::new();
    check.set_valign(gtk::Align::Center);
    check.set_active(false);
    let list_uid_cb = task.list_uid.clone();
    let uid_cb = task.uid.clone();
    check.connect_toggled(move |c| {
        tasks::set_completed(&list_uid_cb, &uid_cb, c.is_active());
    });
    outer.append(&check);

    // Content: vertical box holding header line + optional note line.
    let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
    content.set_hexpand(true);
    content.set_valign(gtk::Align::Center);

    // ── Header line: bold title | spacer | optional due chip | list badge ──
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    header.set_hexpand(true);

    let title_lbl = gtk::Label::new(Some(&task.summary));
    title_lbl.add_css_class("ts-task-title");
    title_lbl.set_halign(gtk::Align::Start);
    title_lbl.set_hexpand(true);
    title_lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title_lbl.set_xalign(0.0);
    header.append(&title_lbl);

    // Optional due chip — small muted label before the badge.
    let due_text = tasks::format_due(task);
    if !due_text.is_empty() {
        let due_lbl = gtk::Label::new(Some(&due_text));
        due_lbl.add_css_class("ts-task-due");
        due_lbl.set_halign(gtk::Align::End);
        due_lbl.set_valign(gtk::Align::Center);
        header.append(&due_lbl);
    }

    // List-name badge pill — top-right, identifies which task list.
    let badge = gtk::Label::new(Some(&task.list_name));
    badge.add_css_class("ts-task-badge");
    badge.set_halign(gtk::Align::End);
    badge.set_valign(gtk::Align::Center);
    header.append(&badge);

    content.append(&header);

    // ── Note line — hidden when description is None/empty ──────────────
    if let Some(ref desc) = task.description {
        let note_lbl = gtk::Label::new(Some(desc));
        note_lbl.add_css_class("ts-task-note");
        note_lbl.set_halign(gtk::Align::Start);
        note_lbl.set_xalign(0.0);
        note_lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
        note_lbl.set_wrap(false);
        content.append(&note_lbl);
    }

    outer.append(&content);
    row.set_child(Some(&outer));

    // Tap-to-edit: `row-activated` fires when the body (not checkbox) is clicked.
    let task_for_edit = task.clone();
    let monitor_for_edit = monitor.clone();
    row.connect_activate(move |r| {
        open_edit_popover(r.upcast_ref(), &task_for_edit, &monitor_for_edit);
    });

    row
}

// ── Create popover (add button) ──────────────────────────────────────────────

fn build_create_popover(
    anchor: &gtk::MenuButton,
    lists_track: &Rc<RefCell<Vec<TaskList>>>,
    monitor: &Monitor,
) -> gtk::Popover {
    let popover = gtk::Popover::new();
    popover.add_css_class("ts-task-popover");
    // The sidebar is a layer-shell surface; without a catcher this popover
    // wouldn't dismiss on outside-click under niri (issue #9).
    hytte::ui::attach_dismiss_catcher(&popover, monitor);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
    column.set_margin_top(8);
    column.set_margin_bottom(8);
    column.set_margin_start(8);
    column.set_margin_end(8);
    column.set_width_request(280);

    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some("New task…"));
    entry.add_css_class("ts-task-entry");
    column.append(&entry);

    // List picker — populated each time the popover opens from the
    // shared `lists_track`. Hidden when there's only one list (clutter
    // when there's no choice to make).
    let list_picker = gtk::DropDown::from_strings(&[]);
    list_picker.add_css_class("ts-task-list-picker");
    column.append(&list_picker);

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

    // Enable Create button only when the entry has non-whitespace text
    // AND at least one task list exists (otherwise there's nowhere to
    // put the new task).
    let create_for_changed = create.clone();
    let lists_for_changed = lists_track.clone();
    entry.connect_changed(move |e| {
        let has_text = !e.text().trim().is_empty();
        let has_list = !lists_for_changed.borrow().is_empty();
        create_for_changed.set_sensitive(has_text && has_list);
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
    let lists_for_create = lists_track.clone();
    let list_picker_for_create = list_picker.clone();
    let do_create = move || {
        let summary = entry_for_create.text().trim().to_string();
        if summary.is_empty() {
            return;
        }
        let lists = lists_for_create.borrow();
        let idx = list_picker_for_create.selected() as usize;
        let Some(list) = lists.get(idx) else {
            return;
        };
        let _ = tasks::create_task(list.uid.clone(), summary, due_picker_for_create.value());
        drop(lists);
        entry_for_create.set_text("");
        due_picker_for_create.reset();
        popover_for_create.popdown();
        anchor_for_create.grab_focus();
    };
    let do_create_for_button = do_create.clone();
    create.connect_clicked(move |_| do_create_for_button());

    let do_create_for_entry = do_create;
    entry.connect_activate(move |_| do_create_for_entry());

    // Reset state every time the popover opens; sync the list picker
    // to whatever the current `lists_track` contains.
    let entry_for_show = entry.clone();
    let due_picker_for_show = due_picker.clone();
    let list_picker_for_show = list_picker.clone();
    let lists_for_show = lists_track.clone();
    let create_for_show = create.clone();
    popover.connect_show(move |_| {
        entry_for_show.set_text("");
        due_picker_for_show.reset();
        sync_list_picker(&list_picker_for_show, &lists_for_show.borrow());
        // Re-evaluate the Add button: empty entry but maybe now with
        // lists (or still without).
        create_for_show.set_sensitive(false);
        entry_for_show.grab_focus();
    });

    popover.set_child(Some(&column));
    popover
}

/// Rebuild the `GtkDropDown`'s items from `lists`. Hides the picker
/// entirely when there's zero or one list (no choice to make). Selects
/// the first item by default.
fn sync_list_picker(picker: &gtk::DropDown, lists: &[TaskList]) {
    let names: Vec<&str> = lists.iter().map(|l| l.display_name.as_str()).collect();
    let strings = gtk::StringList::new(&names);
    picker.set_model(Some(&strings));
    if !lists.is_empty() {
        picker.set_selected(0);
    }
    picker.set_visible(lists.len() > 1);
}

// ── Edit popover (row body click) ────────────────────────────────────────────

fn open_edit_popover(parent: &gtk::Widget, task: &Task, monitor: &Monitor) {
    let popover = gtk::Popover::new();
    popover.add_css_class("ts-task-popover");
    popover.set_parent(parent);
    // Same as the create popover: the sidebar surface needs a catcher for
    // outside-click dismissal under niri (issue #9).
    hytte::ui::attach_dismiss_catcher(&popover, monitor);

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
    let list_uid_for_save = task.list_uid.clone();
    let uid_for_save = task.uid.clone();
    let do_save = move || {
        let summary = entry_for_save.text().trim().to_string();
        if summary.is_empty() {
            return;
        }
        tasks::edit_task(
            &list_uid_for_save,
            &uid_for_save,
            summary,
            due_picker_for_save.value(),
        );
        popover_for_save.popdown();
    };
    let do_save_for_button = do_save.clone();
    save.connect_clicked(move |_| do_save_for_button());

    let do_save_for_entry = do_save;
    entry.connect_activate(move |_| do_save_for_entry());

    let popover_for_cancel = popover.clone();
    cancel.connect_clicked(move |_| popover_for_cancel.popdown());

    let popover_for_delete = popover.clone();
    let list_uid_for_delete = task.list_uid.clone();
    let uid_for_delete = task.uid.clone();
    delete.connect_clicked(move |_| {
        tasks::delete_task(&list_uid_for_delete, &uid_for_delete);
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
        none_btn.connect_toggled(move |b| {
            if b.is_active() {
                p.set_mode(DueMode::None);
            }
        });
        let p = picker.clone();
        today_btn.connect_toggled(move |b| {
            if b.is_active() {
                p.set_mode(DueMode::Today);
            }
        });
        let p = picker.clone();
        tomorrow_btn.connect_toggled(move |b| {
            if b.is_active() {
                p.set_mode(DueMode::Tomorrow);
            }
        });
        let p = picker.clone();
        pick_btn.connect_toggled(move |b| {
            if b.is_active() {
                p.set_mode(DueMode::Pick);
            }
        });

        let p = picker.clone();
        calendar.connect_day_selected(move |c| {
            let y = c.year();
            let m = c.month() + 1; // gtk::Calendar months are 0-indexed
            let d = c.day();
            if let (Ok(m_u32), Ok(d_u32)) = (u32::try_from(m), u32::try_from(d))
                && let Some(date) = NaiveDate::from_ymd_opt(y, m_u32, d_u32)
            {
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
                let label = self.selected.borrow().map_or_else(
                    || "Pick a date".to_string(),
                    |d| format!("Due {}", short_date(d)),
                );
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
        self.calendar_wrap.set_reveal_child(mode == DueMode::Pick);
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
