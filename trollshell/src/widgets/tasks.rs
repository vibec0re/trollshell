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
    // Refresh on each sidebar open like the calendar widget, scoped to
    // `column`'s lifetime so a hot-plug rebuild drops the subscription instead
    // of leaking one per cycle (#439).
    crate::components::open_refresh::on_open(monitor, &column, tasks::refresh);
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
    // All three cells are taken, not `borrow_mut()`-ed: a chained `RefMut`
    // temporary stays alive across the `remove()` calls below (for a `for` it
    // spans the whole loop; for an `if let` the whole then-block), and a
    // synchronous emission re-entering any of them panics from inside a glib
    // callback, which aborts the process. `RefCell::take()` ends its borrow
    // before it returns (#643).
    for row in rows_track.take() {
        group.remove(&row);
    }
    if let Some(p) = placeholder_track.take() {
        group.remove(&p);
    }
    if let Some(o) = overflow_track.take() {
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
        // The VTODO DESCRIPTION is stored verbatim (only trimmed) in the
        // service layer, so it can carry embedded hard `\n` newlines. Pango
        // treats each `\n` as a separate paragraph, and `set_lines(n)` only
        // caps wrapping *within* a paragraph — it does NOT limit the number of
        // hard-newline paragraphs. That's why PR #129's `set_lines(2)` still
        // rendered every line of a multi-line body (issue #126). We must
        // pre-clamp the text ourselves before handing it to the label.
        let logical: Vec<&str> = desc.lines().take(2).collect();
        let note_lbl = if logical.len() == 2 {
            // ≥2 logical lines: keep the first two, one physical line each
            // (wrap OFF so a long line can't spill onto a 3rd line), and let
            // ellipsize trim any over-wide line. Guaranteed ≤2 visual lines.
            let clamped = logical[..2].join("\n");
            let lbl = gtk::Label::new(Some(&clamped));
            lbl.set_wrap(false);
            lbl
        } else {
            // Exactly one logical line: allow wrapping so a long single line
            // flows onto a 2nd line, then `set_lines(2)` + ellipsize cap it.
            let lbl = gtk::Label::new(Some(desc));
            lbl.set_wrap(true);
            lbl.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            lbl.set_lines(2);
            lbl
        };
        note_lbl.add_css_class("ts-task-note");
        note_lbl.set_halign(gtk::Align::Start);
        note_lbl.set_xalign(0.0);
        note_lbl.set_ellipsize(gtk::pango::EllipsizeMode::End);
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
        // Read the dropdown *before* borrowing (#643). The `drop(lists)` below
        // used to come after `list_picker_for_create.selected()`, so the `Ref`
        // was live across that call; the sweep's four-spelling definition says
        // "any borrow across a GTK call" and does not carve out getters, so
        // rather than lean on `selected()` being a plain property read, the two
        // statements are simply swapped. `wire_lists_bind` holds the
        // `borrow_mut()` counterparty on this cell.
        let idx = list_picker_for_create.selected() as usize;
        let lists = lists_for_create.borrow();
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
        // Clone out first: an argument-position `Ref` lives for the whole
        // statement, i.e. across `sync_list_picker`'s GTK work, while
        // `wire_lists_bind` holds the `borrow_mut()` counterparty (#643).
        let lists = lists_for_show.borrow().clone();
        sync_list_picker(&list_picker_for_show, &lists);
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

    // `Popup` folds in what this used to hand-roll: the per-monitor
    // outside-click catcher the sidebar layer surface needs under niri (#9),
    // and unparent-on-close so each row-tap builds a fresh popover instead of
    // accumulating them on the row.
    let popup = Popup::new(parent)
        .child(column)
        .has_arrow(true)
        .css_class("ts-task-popover")
        .dismiss_catcher(monitor)
        .unparent_on_close(true)
        .build();
    let popover = popup.popover().clone();

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

    popup.show();
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
        // Copy both values out first (#643). `match *self.mode.borrow() { … }`
        // borrows the place expression for the *whole* match, so the `Ref` was
        // live across every arm's `set_visible`/`set_text` — spelling (3) — with
        // a second, nested `self.selected.borrow()` inside the `Pick` arm. Three
        // `borrow_mut()` counterparties sit on `mode` (`set_mode`, `set_value`,
        // `reset`) and three more on `selected`, and one of them —
        // `set_mode` — is reached from a `connect_toggled` handler on the chips.
        // Both cells hold `Copy` payloads, so this is a plain register copy.
        let mode = *self.mode.borrow();
        let selected = *self.selected.borrow();
        match mode {
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
                let label = selected.map_or_else(
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

/// Regression coverage for [`DuePicker`]'s two synchronous re-entry edges
/// (#762, split out of #759, part of #674's `RefCell`-across-a-GTK-call
/// sweep). A `BorrowMutError` unwinding out of a GTK signal handler is a
/// **process abort**, not a test failure — that is the class #627/#630/#631/
/// #632/#638/#643/#644/#663/#673 fixed at roughly 50 sites, and here the fix
/// is invisible: it is *the semicolons*.
///
/// [`DuePicker`] is `#[derive(Clone)]` and clones itself into five of its own
/// widget handlers (four `connect_toggled`, one `connect_day_selected`), so
/// `mode` and `selected` are anything but closure-private. Two of the calls
/// [`DuePicker::set_value`] makes re-enter those handlers **synchronously**:
///
/// 1. `self.calendar.select_day(&gdt)` re-enters the `connect_day_selected`
///    handler, which does `*p.selected.borrow_mut() = Some(date)`.
/// 2. `self.set_active_chip(mode)` → `ToggleButton::set_active(true)`
///    re-enters the chip's `connect_toggled` handler, which calls
///    [`DuePicker::set_mode`] → `*self.mode.borrow_mut() = mode`.
///
/// Both survive only because the counterpart write in `set_value` is a
/// **statement-scoped temporary** (`*self.selected.borrow_mut() = …;`) whose
/// `RefMut` is already dropped when the emitting call runs. Written the
/// obvious way — `let mut sel = self.selected.borrow_mut();` and then
/// `select_day` inside that binding's scope — both abort. Verified: see the
/// PR for #762.
///
/// ## Measured, not argued (gtk4 crate 0.11.2 / GTK 4.22.4, under `xvfb-run`)
///
/// Both edges were probed with throwaway harnesses before this module was
/// written, because reasoning about which GTK setters emit is unreliable:
///
/// | call | emissions |
/// |---|---|
/// | `Calendar::select_day` onto a different **day-of-month** | 1 |
/// | `Calendar::select_day` onto a different month, same day-of-month | **0** |
/// | `Calendar::select_day` onto a different year, same day-of-month | **0** |
/// | `Calendar::select_day` onto the already-selected date | **0** |
/// | `ToggleButton::set_active(true)` on an **inactive** grouped member | 1 on the arriving chip + 1 on the leaving sibling |
/// | `ToggleButton::set_active(true)` on the **already-active** member | **0**, on every member of the group |
///
/// Two consequences that the tests below encode, and that anyone editing them
/// must not "simplify" away:
///
/// * **`day-selected` tracks the day-of-month only.** `gtk_calendar_select_day`
///   sets year, month and day through three change-guarded setters and only
///   the day setter emits. So the test date must differ from today *in its
///   day-of-month*, or the test silently covers nothing. `Local::now() +
///   5 days` satisfies this unconditionally: it is neither today nor tomorrow
///   (so `set_value` reaches the `else` branch that calls `select_day` at all)
///   and it can never share today's day-of-month, since that would need a
///   five-day month. "A month out" or "same day next year" both emit zero.
/// * **The `set_active_chip` edge exists only on a real flip — and that is
///   what bounds it.** [`DuePicker::set_active_chip`]'s doc comment is
///   accurate: on a genuine flip the arriving chip's `toggled` handler does
///   re-run `set_mode`, which does re-store the mode it already holds. What
///   the measurement adds is why that redundancy stops at one hop. The
///   re-entrant `set_mode` calls `set_active_chip` again, and by then the chip
///   *is* active — so `set_active(true)` short-circuits and emits nothing.
///   The already-active zero in the table above is the recursion's
///   termination condition, not a refutation of the comment. Hence the
///   emission log asserted below is exactly `["none:off", "pick:on"]`: one
///   hop, not an unbounded chain.
///
/// Also measured directly rather than inferred: holding a `RefMut` across
/// either call and `try_borrow_mut`-ing the same cell from the handler reports
/// `RefCell already borrowed`. The hazard is reachable; the statement scoping
/// is load-bearing.
///
/// ## Deliberately **not** covered
///
/// [`DuePicker::set_mode`]'s own `select_day` (the "seed the calendar with
/// today if nothing selected yet" branch) is **dead as a test target**: it
/// seeds *today* onto a `gtk::Calendar` that is already sitting on today, i.e.
/// row 4 of the table above — zero emissions, no re-entry, nothing to falsify.
///
/// [`DuePicker::refresh_summary`]'s pre-#643 shape (`match *self.mode.borrow()`
/// holding the `Ref` across every arm) is likewise unfalsifiable: every arm
/// touches only `self.summary_label`, and nothing is connected to that label,
/// so reverting it emits nothing and panics nowhere.
///
/// Needs a real display server (a `gtk::Calendar` has to be constructible and
/// actually run its setters), so this sits in the `system-tests` bucket and
/// runs under `xvfb-run`, like the rest of this bug class.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::{DueMode, DuePicker};
    use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone};
    use hytte::adw::{self, prelude::*};
    use hytte::gtk;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    /// `set_value` reads only `dt.date_naive()`, so the wall-clock time is
    /// irrelevant to the code under test — but local *midnight* does not exist
    /// on a spring-forward day in some zones, and `.single()` returns `None`
    /// there. Noon exists everywhere.
    fn at_noon(date: NaiveDate) -> DateTime<Local> {
        let noon = NaiveTime::from_hms_opt(12, 0, 0).expect("12:00:00 is a valid wall-clock time");
        Local
            .from_local_datetime(&date.and_time(noon))
            .single()
            .expect("local noon is unambiguous in every real time zone")
    }

    /// A Pick-mode date whose **day-of-month** is guaranteed to differ from
    /// today's — see the module doc. `+5 days` is neither today nor tomorrow
    /// (so `set_value` takes the branch that calls `select_day`) and cannot
    /// land on today's day-of-month, since that would need a five-day month.
    fn pick_date() -> NaiveDate {
        Local::now().date_naive() + Duration::days(5)
    }

    /// Count `day-selected` emissions on the picker's own calendar, and record
    /// whether `selected` was free at the moment the handler chain ran.
    fn watch_calendar(picker: &DuePicker) -> (Rc<Cell<u32>>, Rc<Cell<Option<bool>>>) {
        let emissions = Rc::new(Cell::new(0_u32));
        let free_inside = Rc::new(Cell::new(None));
        let n = Rc::clone(&emissions);
        let free = Rc::clone(&free_inside);
        let selected = Rc::clone(&picker.selected);
        picker.calendar.connect_day_selected(move |_| {
            n.set(n.get() + 1);
            free.set(Some(selected.try_borrow_mut().is_ok()));
        });
        (emissions, free_inside)
    }

    /// Leg 1: `set_value` → `Calendar::select_day` → `day-selected` →
    /// `*p.selected.borrow_mut()`.
    ///
    /// The emission count is asserted rather than assumed: if a future GTK
    /// stops emitting here, this test would otherwise keep passing while
    /// covering nothing at all.
    #[gtk::test]
    fn set_value_does_not_hold_selected_across_select_day() {
        adw::init().expect("libadwaita init");
        let picker = DuePicker::new();
        let (emissions, free_inside) = watch_calendar(&picker);

        let date = pick_date();
        picker.set_value(Some(at_noon(date)), false);

        assert_eq!(
            emissions.get(),
            1,
            "select_day must emit day-selected synchronously inside set_value — the count is read \
             with no main-loop pump. A zero here means this test covers nothing: check that the \
             date still differs from today in its day-of-month (see the module doc)."
        );
        assert_eq!(
            free_inside.get(),
            Some(true),
            "the `selected` cell must not be borrowed while the day-selected handler chain runs; \
             the handler's own `borrow_mut()` would abort the process, not fail this assertion"
        );
        assert_eq!(*picker.selected.borrow(), Some(date));
        assert_eq!(*picker.mode.borrow(), DueMode::Pick);
        assert!(picker.chip_pick.is_active());

        emissions.set(0);
        picker.set_value(Some(at_noon(date)), false);
        assert_eq!(
            emissions.get(),
            0,
            "re-seeding the same date must be silent — without this control the assertion above \
             could be satisfied by a calendar that emits on every select_day call"
        );
    }

    /// Leg 2: `set_value` → `set_active_chip` → `ToggleButton::set_active` →
    /// `toggled` → `set_mode` → `*self.mode.borrow_mut()`.
    ///
    /// Also pins the measured shape of the edge: the leaving sibling emits
    /// first, the arriving chip second, and a redundant `set_active(true)` on
    /// the already-active member emits nothing at all.
    #[gtk::test]
    fn set_value_does_not_hold_mode_across_set_active_chip() {
        adw::init().expect("libadwaita init");
        let picker = DuePicker::new();
        assert!(
            picker.chip_none.is_active(),
            "a fresh picker starts on \"No date\"; the flip this test needs depends on it"
        );

        let order: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));
        let mode_free_inside = Rc::new(Cell::new(None));

        let log = Rc::clone(&order);
        picker.chip_none.connect_toggled(move |b| {
            log.borrow_mut()
                .push(if b.is_active() { "none:on" } else { "none:off" });
        });
        let log = Rc::clone(&order);
        let free = Rc::clone(&mode_free_inside);
        let mode = Rc::clone(&picker.mode);
        picker.chip_pick.connect_toggled(move |b| {
            log.borrow_mut()
                .push(if b.is_active() { "pick:on" } else { "pick:off" });
            if b.is_active() {
                free.set(Some(mode.try_borrow_mut().is_ok()));
            }
        });

        picker.set_value(Some(at_noon(pick_date())), false);

        assert_eq!(
            *order.borrow(),
            ["none:off", "pick:on"],
            "set_active_chip must move the group synchronously inside set_value, leaving chip \
             first then arriving chip. An empty log means this test covers nothing."
        );
        assert_eq!(
            mode_free_inside.get(),
            Some(true),
            "the `mode` cell must not be borrowed while the toggled handler chain runs; the \
             chip's own handler re-runs set_mode, whose `borrow_mut()` would abort the process"
        );
        assert_eq!(*picker.mode.borrow(), DueMode::Pick);

        drop(order.take());
        mode_free_inside.set(None);
        picker.set_active_chip(DueMode::Pick);
        let redundant = order.take();
        assert!(
            redundant.is_empty(),
            "measured: set_active(true) on the already-active member of a group emits nothing on \
             any member. This zero is what bounds the re-entry above to a single hop — the \
             re-entrant set_mode calls set_active_chip on a chip that is already active. If this \
             ever fires, that bound is gone and the redundant set_mode chain no longer \
             terminates here: {redundant:?}"
        );
        assert_eq!(
            mode_free_inside.get(),
            None,
            "no toggled emission means the handler never ran, so the witness stays unset"
        );
    }

    /// The tripwire under both tests' choice of date: `day-selected` follows
    /// the **day-of-month**, not the date. A month-only or year-only move is
    /// silent, which is exactly how this coverage would rot into a false pass.
    ///
    /// If a future GTK emits on any field change these zeros flip and this
    /// test fails — that is a behaviour change to record, not a defect: the
    /// `+5 days` date the other tests use stays correct under either rule.
    #[gtk::test]
    fn day_selected_tracks_the_day_of_month_only() {
        adw::init().expect("libadwaita init");
        let picker = DuePicker::new();
        let (emissions, _) = watch_calendar(&picker);

        // A fresh gtk::Calendar sits on *today*, so the first move has to
        // differ from today's day-of-month to emit at all. Anchor on a day
        // that exists in every month and is never today's.
        let anchor = if Local::now().date_naive().day() == 15 {
            14
        } else {
            15
        };
        let on = |y, m, d| {
            NaiveDate::from_ymd_opt(y, m, d).expect("hand-written calendar date is valid")
        };
        let take = || {
            let n = emissions.get();
            emissions.set(0);
            n
        };

        picker.set_value(Some(at_noon(on(2020, 3, anchor))), false);
        assert_eq!(take(), 1, "a first move to a new day-of-month emits");
        picker.set_value(Some(at_noon(on(2020, 4, anchor))), false);
        assert_eq!(take(), 0, "month-only change is silent (measured)");
        picker.set_value(Some(at_noon(on(2021, 4, anchor))), false);
        assert_eq!(take(), 0, "year-only change is silent (measured)");
        picker.set_value(Some(at_noon(on(2021, 4, anchor + 1))), false);
        assert_eq!(take(), 1, "day-of-month change emits");
    }
}

// ── Reentrancy regression tests ───────────────────────────────────────────────

/// The `RefCell`-across-a-GTK-call abort class (#674) for this file.
///
/// ## Why there is no production change alongside these tests
///
/// `rebuild_list` is already a private free function taking its three cells,
/// the task slice, and a `&Monitor` as explicit parameters — no seam needed to
/// reach it from a colocated test, by module privacy alone. Nothing was
/// extracted, reordered, or reshaped to make this module compile; see
/// `rebuild_list`'s own doc comment above for why all three cells are
/// `take()`n rather than `borrow_mut()`-ed (#643).
///
/// Unlike `workspaces.rs`'s `update_workspaces` or `window_list.rs`'s
/// `update_windows`, `rebuild_list` never diffs by identity — every call
/// unconditionally tears down whatever the last call left in all three cells
/// and rebuilds fresh from `ts`. That means a test doesn't need to change the
/// task set between calls to force a removal; calling twice with the *same*
/// tasks still exercises the full take/remove/rebuild path each time.
///
/// ## Why this needs `App::run`
///
/// `rebuild_list` threads its `&Monitor` down into `build_task_row` (for the
/// edit popover's dismiss-catcher target). `Monitor`'s only constructor is
/// `pub(crate)` to `hytte-ui`, so the sole way to obtain one from this crate
/// is `App::monitors` — which exists only inside a running `App::run` body.
/// `test_monitor` does that once, lazily, and caches the result. None of the
/// tests below ever activate a row (so the monitor is never read past the
/// type-check), but the signature still demands a real one.
///
/// ## Why the probe is `destroy`, not `unmap`
///
/// PR #817's first attempt at this bug class (`overlays/notifications.rs`)
/// found that a removed widget's `destroy` can be silently deferred when the
/// widget is still referenced by something outside the cell under test — its
/// toast cards held a focusable dismiss button, and with the toast window
/// actually mapped, that kept the card alive past `vbox.remove()`, so
/// `destroy` never fired *inside* the call and the probe had to switch to
/// `unmap` instead.
///
/// That trap doesn't apply here: `group` in these tests is a bare
/// `adw::PreferencesGroup`, never added to any shown window (production only
/// ever calls `rebuild_list` on a group some other function has already
/// mounted; these tests don't need it mounted at all). With no `GtkRoot`
/// there is no focus-widget chain to retain a removed row, placeholder, or
/// overflow row past `group.remove()` — `destroy` fires off pure refcounting,
/// the same signal `workspaces.rs`'s #814 and `window_list.rs`'s regression
/// tests rest on. Each test below asserts this rather than assuming it: the
/// anti-vacuity check on `fired_inside` is what would catch it if it were
/// ever untrue.
///
/// Needs a real display server (a `Monitor` has to come from a live GDK
/// backend), hence the `system-tests` gate, like the `DuePicker` tests above.
#[cfg(all(test, feature = "system-tests"))]
mod reentrancy_tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use hytte::adw::{self, prelude::*};
    use hytte::gtk;
    use hytte::prelude::*;
    use hytte::services::tasks::{Task, TaskStatus};

    use super::{MAX_VISIBLE_TASKS, rebuild_list};

    thread_local! {
        /// The output every `Monitor` in this module is built from, captured
        /// once by [`test_monitor`]. `Monitor` is `!Send` and this binary's
        /// `#[gtk::test]`s all share one thread, so a `thread_local!` needs no
        /// synchronisation.
        static TEST_MONITOR: RefCell<Option<Monitor>> = const { RefCell::new(None) };
    }

    /// A real `Monitor`, captured from a one-shot `App::run`.
    ///
    /// Cached, because `App::run` has process-global side effects (`adw::init`,
    /// the dark colour scheme, the default stylesheet, a leaked application
    /// hold) and this is a unit-test binary shared with several hundred other
    /// tests — running it per test would repeat all of that per test.
    /// `#[gtk::test]` runs every test in this binary on one thread, so the
    /// cache needs no locking and cannot be raced regardless of test order.
    fn test_monitor() -> Monitor {
        if let Some(monitor) = TEST_MONITOR.with(|cell| cell.borrow().clone()) {
            return monitor;
        }
        App::new("mov.vibec0re.trollshell.test.tasks-reentrancy")
            .run(|app| {
                let first = app.monitors().first().cloned();
                TEST_MONITOR.with(|cell| *cell.borrow_mut() = first);
                app.quit();
            })
            .expect("App::run");
        TEST_MONITOR
            .with(|cell| cell.borrow().clone())
            .expect("the display server must report at least one output; `xvfb-run` provides one")
    }

    /// A task identified by `uid`/`summary` alone. `rebuild_list` never diffs
    /// by identity, so no other field matters to these tests — `format_due`
    /// on a `None` due renders an empty chip, and `list_name` only feeds a
    /// label nothing here inspects.
    fn task(uid: &str, summary: &str) -> Task {
        Task {
            uid: uid.to_owned(),
            summary: summary.to_owned(),
            description: None,
            due: None,
            due_all_day: false,
            status: TaskStatus::NeedsAction,
            list_uid: "list-1".to_owned(),
            list_name: "Personal".to_owned(),
        }
    }

    /// `task("t{n}", "{n}")` for `n` in `1..=count` — the shape the overflow
    /// test needs to push past `MAX_VISIBLE_TASKS`.
    fn tasks(count: usize) -> Vec<Task> {
        (1..=count)
            .map(|n| task(&format!("t{n}"), &n.to_string()))
            .collect()
    }

    type RowsTrack = Rc<RefCell<Vec<adw::PreferencesRow>>>;
    type OptRow = Rc<RefCell<Option<adw::ActionRow>>>;

    /// The three cells `rebuild_list` takes, exactly as `build_block` builds
    /// them, plus a fresh group and a real `Monitor`.
    fn fresh() -> (adw::PreferencesGroup, RowsTrack, OptRow, OptRow, Monitor) {
        (
            adw::PreferencesGroup::new(),
            Rc::new(RefCell::new(Vec::new())),
            Rc::new(RefCell::new(None)),
            Rc::new(RefCell::new(None)),
            test_monitor(),
        )
    }

    /// Cell 1, the row list: `group.remove(&row)` drops the group's
    /// reference, and the loop-owned `row` binding drops its own at the end
    /// of that iteration — the last strong ref, so `GtkWidget::destroy` fires
    /// **synchronously** from dispose (see the module doc for why no focus
    /// chain defers it here).
    ///
    /// The handler re-enters `rebuild_list` on the same three cells. Against
    /// the pre-#643 `let mut rows = rows_track.borrow_mut();` (closing
    /// write-back dropped) the inner call hits a live `RefMut` and aborts the
    /// binary with `BorrowMutError` rather than failing one test — #663's
    /// SIGABRT, the failure mode #674 exists for. With `take()` the cell is
    /// free for the whole call, so the inner call simply finds an empty `Vec`.
    #[gtk::test]
    fn rebuild_list_tolerates_a_reentrant_rebuild_from_a_removed_rows_destroy() {
        let (group, rows_track, placeholder_track, overflow_track, monitor) = fresh();
        let seed = tasks(2);

        rebuild_list(
            &group,
            &rows_track,
            &placeholder_track,
            &overflow_track,
            &seed,
            &monitor,
        );
        assert_eq!(
            rows_track.borrow().len(),
            2,
            "both tasks must be rendered after the seeding call"
        );

        // True only while the outer `rebuild_list` is on the stack, so the
        // handler can record whether it ran inside the call or was deferred.
        let in_outer = Rc::new(Cell::new(false));
        let fired_inside = Rc::new(Cell::new(None::<bool>));

        // Arm the second row's destroy to re-enter rebuild_list.
        let row2 = rows_track.borrow()[1].clone();
        {
            let group = group.clone();
            let rows_track = Rc::clone(&rows_track);
            let placeholder_track = Rc::clone(&placeholder_track);
            let overflow_track = Rc::clone(&overflow_track);
            let monitor = monitor.clone();
            let seed = seed.clone();
            let in_outer = Rc::clone(&in_outer);
            let fired_inside = Rc::clone(&fired_inside);
            let armed = Cell::new(true);
            row2.connect_destroy(move |_| {
                if !armed.replace(false) {
                    return;
                }
                fired_inside.set(Some(in_outer.get()));
                rebuild_list(
                    &group,
                    &rows_track,
                    &placeholder_track,
                    &overflow_track,
                    &seed,
                    &monitor,
                );
            });
        }
        // Drop our clone before the removing pass: while it lives the row has
        // a second strong ref, `group.remove()` won't dispose it, and
        // `destroy` never fires — the test would pass vacuously.
        drop(row2);

        in_outer.set(true);
        rebuild_list(
            &group,
            &rows_track,
            &placeholder_track,
            &overflow_track,
            &seed,
            &monitor,
        );
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "the removed row's `destroy` must fire synchronously inside `rebuild_list`; if GTK \
             ever defers it, or the row outlives the removal loop, this test proves nothing about \
             the borrow discipline"
        );
        assert_eq!(
            rows_track.borrow().len(),
            2,
            "the outer call's write-back must still land: re-entry may not leave the cell holding \
             the inner call's rows or an empty Vec"
        );
    }

    /// Cell 2, the placeholder: an empty `ts` renders "No tasks", torn down
    /// and rebuilt on every call that stays empty — there is no reuse arm
    /// here either. Falsifiable independently of the row-list test above:
    /// reverting only `placeholder_track`'s `take()` to a held `borrow_mut()`
    /// makes the abort name `placeholder_track` and no other cell.
    #[gtk::test]
    fn rebuild_list_tolerates_a_reentrant_rebuild_from_a_removed_placeholder_destroy() {
        let (group, rows_track, placeholder_track, overflow_track, monitor) = fresh();

        rebuild_list(
            &group,
            &rows_track,
            &placeholder_track,
            &overflow_track,
            &[],
            &monitor,
        );
        assert!(
            placeholder_track.borrow().is_some(),
            "an empty task list must render the placeholder after the seeding call"
        );

        let in_outer = Rc::new(Cell::new(false));
        let fired_inside = Rc::new(Cell::new(None::<bool>));

        let placeholder = placeholder_track
            .borrow()
            .clone()
            .expect("just asserted Some above");
        {
            let group = group.clone();
            let rows_track = Rc::clone(&rows_track);
            let placeholder_track = Rc::clone(&placeholder_track);
            let overflow_track = Rc::clone(&overflow_track);
            let monitor = monitor.clone();
            let in_outer = Rc::clone(&in_outer);
            let fired_inside = Rc::clone(&fired_inside);
            let armed = Cell::new(true);
            placeholder.connect_destroy(move |_| {
                if !armed.replace(false) {
                    return;
                }
                fired_inside.set(Some(in_outer.get()));
                rebuild_list(
                    &group,
                    &rows_track,
                    &placeholder_track,
                    &overflow_track,
                    &[],
                    &monitor,
                );
            });
        }
        // Drop our clone before the removing pass, same reasoning as the row
        // test: a second strong ref would keep it alive past `group.remove()`.
        drop(placeholder);

        in_outer.set(true);
        rebuild_list(
            &group,
            &rows_track,
            &placeholder_track,
            &overflow_track,
            &[],
            &monitor,
        );
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "the retired placeholder's `destroy` must fire synchronously inside `rebuild_list`; \
             if it does not, this test proves nothing about the borrow discipline"
        );
        assert!(
            placeholder_track.borrow().is_some(),
            "the outer call's write-back must still land: re-entry may not leave the cell empty \
             or holding the inner call's placeholder"
        );
    }

    /// Cell 3, the overflow row: one more task than `MAX_VISIBLE_TASKS`
    /// collapses the tail into a "+N more" row, torn down and rebuilt on
    /// every call whose tail still overflows. Falsifiable independently of
    /// the two tests above: reverting only `overflow_track`'s `take()` names
    /// that cell and no other.
    #[gtk::test]
    fn rebuild_list_tolerates_a_reentrant_rebuild_from_a_removed_overflow_destroy() {
        let (group, rows_track, placeholder_track, overflow_track, monitor) = fresh();
        let over = tasks(MAX_VISIBLE_TASKS + 1);

        rebuild_list(
            &group,
            &rows_track,
            &placeholder_track,
            &overflow_track,
            &over,
            &monitor,
        );
        assert!(
            overflow_track.borrow().is_some(),
            "one task past the cap must collapse into an overflow row after the seeding call"
        );

        let in_outer = Rc::new(Cell::new(false));
        let fired_inside = Rc::new(Cell::new(None::<bool>));

        let overflow = overflow_track
            .borrow()
            .clone()
            .expect("just asserted Some above");
        {
            let group = group.clone();
            let rows_track = Rc::clone(&rows_track);
            let placeholder_track = Rc::clone(&placeholder_track);
            let overflow_track = Rc::clone(&overflow_track);
            let monitor = monitor.clone();
            let over = over.clone();
            let in_outer = Rc::clone(&in_outer);
            let fired_inside = Rc::clone(&fired_inside);
            let armed = Cell::new(true);
            overflow.connect_destroy(move |_| {
                if !armed.replace(false) {
                    return;
                }
                fired_inside.set(Some(in_outer.get()));
                rebuild_list(
                    &group,
                    &rows_track,
                    &placeholder_track,
                    &overflow_track,
                    &over,
                    &monitor,
                );
            });
        }
        drop(overflow);

        in_outer.set(true);
        rebuild_list(
            &group,
            &rows_track,
            &placeholder_track,
            &overflow_track,
            &over,
            &monitor,
        );
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "the retired overflow row's `destroy` must fire synchronously inside `rebuild_list`; \
             if it does not, this test proves nothing about the borrow discipline"
        );
        assert!(
            overflow_track.borrow().is_some(),
            "the outer call's write-back must still land: re-entry may not leave the cell empty \
             or holding the inner call's overflow row"
        );
        assert_eq!(
            rows_track.borrow().len(),
            MAX_VISIBLE_TASKS,
            "the outer call's row write-back must also land, not just the overflow row's"
        );
    }
}
