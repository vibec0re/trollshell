# Calendar Widget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a sidebar calendar widget (month grid + 7-day upcoming list) that replaces the current `Label("sidebar")` placeholder. Same data source as the existing drawer panel; parallel rendering.

**Architecture:** New `trollshell/src/widgets/calendar.rs` mirroring the drawer panel's structure (`gtk::Calendar` + `adw::PreferencesGroup` of `adw::ActionRow`s) but built for the dark sidebar surface. Reuses the `calendar::events()` signal already provided by `hytte-services::calendar`. Wires sidebar-open → `calendar::refresh()` so opening the sidebar always shows fresh data.

**Tech Stack:** Rust 2024, GTK4 + libadwaita, hytte reactive layer (`bind` + `Signal`), `chrono` for date math.

**Spec:** `docs/superpowers/specs/2026-05-14-calendar-widget-design.md`

---

## File Structure

- **Create:** `trollshell/src/widgets/calendar.rs` — the widget builder (`pub fn widget(monitor: &Monitor) -> gtk::Widget`) plus three private helpers (`apply_event_marks`, `scroll_row_into_view`, `flash_row_highlight`) and a `build_calendar_row` helper. Single file, ~250 lines, mirrors `trollshell/src/panels/calendar.rs` in shape.
- **Modify:** `trollshell/src/widgets/mod.rs` — register `pub mod calendar;`.
- **Modify:** `trollshell/src/overlays/sidebar.rs` — replace the placeholder `Label` with `widgets::calendar::widget(monitor)`. Drop the `placeholder` local and the `ts-sidebar-placeholder` CSS class.
- **Modify:** `trollshell/style.css` — add `.ts-sidebar-calendar*` rules; drop the now-unused `.ts-sidebar-placeholder` rule.

No changes to `panels/calendar.rs`, `crates/hytte-services/src/calendar.rs`, `widgets/sidebar_toggle.rs`, or `etc/niri/frame.kdl`. No new dependencies.

---

### Task 1: Skeleton module + register

**Files:**
- Create: `trollshell/src/widgets/calendar.rs`
- Modify: `trollshell/src/widgets/mod.rs`

- [ ] **Step 1.1: Create the empty widget module**

Create `trollshell/src/widgets/calendar.rs` with:

```rust
//! Sidebar calendar widget: month grid + 7-day upcoming events list.
//! Sibling to the drawer panel `trollshell/src/panels/calendar.rs` —
//! same data source (`hytte::services::calendar::events`), parallel
//! rendering for the always-on left sidebar surface. See
//! `docs/superpowers/specs/2026-05-14-calendar-widget-design.md`.

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;

/// Build the sidebar calendar widget. Caller appends the returned widget
/// to `.ts-sidebar`; the widget owns its own subscriptions to
/// `calendar::events()` and `sidebar::open_signal(monitor)`.
pub fn widget(_monitor: &Monitor) -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.add_css_class("ts-sidebar-calendar");
    column.upcast()
}
```

- [ ] **Step 1.2: Register the module**

Edit `trollshell/src/widgets/mod.rs` and add `pub mod calendar;` in alphabetical order (between `brightness` and `clock`):

```rust
pub mod battery;
pub mod bluetooth;
pub mod brightness;
pub mod calendar;
pub mod clock;
// … rest unchanged
```

- [ ] **Step 1.3: Verify it builds**

Run: `cargo build -p trollshell`
Expected: builds cleanly. There will be an unused-warning on `widget` until it's wired in later — that's fine for this task.

- [ ] **Step 1.4: Commit**

```bash
git add trollshell/src/widgets/calendar.rs trollshell/src/widgets/mod.rs
git commit -m "feat(widgets/calendar): module skeleton" -m "Empty sidebar calendar widget module. Subsequent commits fill in the month grid, upcoming-events list, and behavior."
```

---

### Task 2: Pure helper `apply_event_marks` with TDD

**Files:**
- Modify: `trollshell/src/widgets/calendar.rs`

The drawer panel's `apply_event_marks` is private to `panels/calendar.rs`. We copy + test it here. The function is the only piece of this widget that's straightforwardly unit-testable; the rest is GTK widget plumbing covered by interactive verification.

**TDD note:** `GtkCalendar` is hard to instantiate headlessly, so we split the logic. `marked_days` is a pure function (`events × year × month_1_indexed → HashSet<u32>`) and gets unit tests. `apply_event_marks` is the thin GTK shim that calls `marked_days` and applies the marks; it's covered by interactive verification, not unit tests.

- [ ] **Step 2.1: Write the failing tests first**

Append to `trollshell/src/widgets/calendar.rs`:

```rust
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
```

- [ ] **Step 2.2: Run the tests, verify they fail**

Run: `cargo test -p trollshell widgets::calendar::tests`
Expected: **compile error** — `marked_days` is not defined yet. That's the failure we want.

- [ ] **Step 2.3: Add the production `marked_days` + `apply_event_marks`**

Replace the file's body (between the module docs and the `#[cfg(test)]` block) with:

```rust
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
```

- [ ] **Step 2.4: Run the tests, verify they pass**

Run: `cargo test -p trollshell widgets::calendar::tests`
Expected: all three tests pass.

- [ ] **Step 2.5: Commit**

```bash
git add trollshell/src/widgets/calendar.rs
git commit -m "feat(widgets/calendar): apply_event_marks + tests" -m "Mirrors panels/calendar.rs's private helper; standalone tests cover month-filter, dedup, and empty cases via a parallel pure shim."
```

---

### Task 3: Widget tree skeleton + sidebar wiring

**Files:**
- Modify: `trollshell/src/widgets/calendar.rs`
- Modify: `trollshell/src/overlays/sidebar.rs`

- [ ] **Step 3.1: Add the widget tree (no event binding yet)**

In `trollshell/src/widgets/calendar.rs`, replace the body of `pub fn widget(...)` with:

```rust
pub fn widget(_monitor: &Monitor) -> gtk::Widget {
    use hytte::adw::{self, prelude::*};

    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.add_css_class("ts-sidebar-calendar");

    // Month grid. GtkCalendar self-scrolls to the current month on
    // construction. Marks are applied later by apply_event_marks.
    let cal = gtk::Calendar::new();
    cal.set_show_heading(true);
    cal.set_show_day_names(true);
    cal.set_show_week_numbers(false);
    cal.add_css_class("ts-calendar");
    column.append(&cal);

    // Small section header above the events list.
    let header = gtk::Label::new(Some("UPCOMING"));
    header.add_css_class("ts-sidebar-cal-header");
    header.set_halign(gtk::Align::Start);
    column.append(&header);

    // adw::PreferencesGroup styled as a list. The group's `title` ends
    // up large; we use a separate `header` label above instead so we
    // can match the small-caps sidebar typography. The group itself
    // gets no title.
    let group = adw::PreferencesGroup::new();
    group.add_css_class("ts-sidebar-cal-list");

    // Bounded ScrolledWindow so the list scrolls independently when long.
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_min_content_height(220);
    scrolled.set_max_content_height(360);
    scrolled.set_child(Some(&group));
    column.append(&scrolled);

    column.upcast()
}
```

The `_monitor` argument stays underscored until Task 6 wires the sidebar-open hook.

- [ ] **Step 3.2: Wire the widget into the sidebar**

Open `trollshell/src/overlays/sidebar.rs`. Find this block in `install`:

```rust
    let placeholder = gtk::Label::new(Some("sidebar"));
    placeholder.add_css_class("ts-sidebar-placeholder");
    placeholder.set_halign(gtk::Align::Center);
    placeholder.set_valign(gtk::Align::Center);
    placeholder.set_vexpand(true);
    card.append(&placeholder);
```

Replace it with:

```rust
    card.append(&crate::widgets::calendar::widget(monitor));
```

- [ ] **Step 3.3: Verify it builds**

Run: `cargo build -p trollshell`
Expected: builds cleanly. The widget renders an empty month grid + empty list when the sidebar is opened.

- [ ] **Step 3.4: Commit**

```bash
git add trollshell/src/widgets/calendar.rs trollshell/src/overlays/sidebar.rs
git commit -m "feat(widgets/calendar): widget tree + sidebar wiring" -m "Builds month grid + scrolled empty list. Replaces the placeholder Label('sidebar') in overlays/sidebar.rs. No data binding yet — comes in Task 4."
```

---

### Task 4: Bind to `calendar::events()` + empty-state placeholder

**Files:**
- Modify: `trollshell/src/widgets/calendar.rs`

- [ ] **Step 4.1: Add `build_calendar_row`**

In `trollshell/src/widgets/calendar.rs`, add this helper near `apply_event_marks`:

```rust
fn build_calendar_row(ev: &CalendarEvent) -> hytte::adw::ActionRow {
    use hytte::adw::{self, prelude::*};
    use hytte::services::calendar::format_when;

    // Subtitle: when-string, plus optional location on its own line. Lets
    // long venue names wrap without inflating the sidebar's width.
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
```

- [ ] **Step 4.2: Bind to `calendar::events()` in `widget()`**

Replace the `widget()` body so the trailing `column.upcast()` is preceded by the binding. Final body:

```rust
pub fn widget(_monitor: &Monitor) -> gtk::Widget {
    use std::cell::RefCell;
    use std::rc::Rc;

    use hytte::adw::{self, prelude::*};
    use hytte::services::calendar;

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

    let group_for_bind = group.clone();
    let rows_for_bind = rows_track.clone();
    let placeholder_for_bind = placeholder_track.clone();
    let cal_for_bind = cal.clone();
    let current_events_for_bind = current_events.clone();
    bind(calendar::events(), &group, move |_, evs| {
        // Drop previous rows.
        for (_d, row) in rows_for_bind.borrow_mut().drain(..) {
            group_for_bind.remove(&row);
        }
        if let Some(p) = placeholder_for_bind.borrow_mut().take() {
            group_for_bind.remove(&p);
        }

        // Stash the snapshot so month-nav handlers see fresh data.
        current_events_for_bind.borrow_mut().clone_from(&evs);

        // Re-mark the visible month with the new event set.
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

    column.upcast()
}
```

- [ ] **Step 4.3: Verify it builds**

Run: `cargo build -p trollshell`
Expected: builds cleanly. Unit tests still pass: `cargo test -p trollshell widgets::calendar::tests`.

- [ ] **Step 4.4: Commit**

```bash
git add trollshell/src/widgets/calendar.rs
git commit -m "feat(widgets/calendar): bind to calendar::events" -m "Populate adw::ActionRow per event from calendar::events(). Empty signal → 'No upcoming events' placeholder row. Re-applies month marks on each emission."
```

---

### Task 5: Month-navigation re-mark

**Files:**
- Modify: `trollshell/src/widgets/calendar.rs`

The bind from Task 4 re-marks only when the events signal emits. When the user clicks `>` or `<` to navigate months, the visible month changes but the events vec doesn't — we need to re-call `apply_event_marks` on month change too. The `current_events` `Rc<RefCell<…>>` introduced in Task 4 is exactly the stash these handlers borrow from.

- [ ] **Step 5.1: Wire month-nav handlers**

In `widget()`, after the `bind(...)` call (and before `column.upcast()`), add:

```rust
    // Re-mark on month navigation. connect_next_month bumps year on
    // December rollover internally, so connect_year_changed isn't needed.
    {
        let events_for_next = current_events.clone();
        cal.connect_next_month(move |c| {
            apply_event_marks(c, &events_for_next.borrow());
        });
    }
    {
        let events_for_prev = current_events.clone();
        cal.connect_prev_month(move |c| {
            apply_event_marks(c, &events_for_prev.borrow());
        });
    }
```

- [ ] **Step 5.2: Verify it builds**

Run: `cargo build -p trollshell`
Expected: builds cleanly.

- [ ] **Step 5.3: Commit**

```bash
git add trollshell/src/widgets/calendar.rs
git commit -m "feat(widgets/calendar): re-mark on month navigation" -m "connect_next_month / connect_prev_month re-apply marks against the latest events snapshot. Snapshot is stashed in an Rc<RefCell> populated by the events bind."
```

---

### Task 6: Click-day → scroll-and-flash + sidebar-open refresh

**Files:**
- Modify: `trollshell/src/widgets/calendar.rs`

- [ ] **Step 6.1: Add `scroll_row_into_view` and `flash_row_highlight` helpers**

After `build_calendar_row` (or anywhere outside `widget()`), add:

```rust
/// Scroll `scrolled` so that `row` is visible. Uses `compute_point` to
/// translate the row's origin into the scrolled-window child's coordinate
/// space; an 8px lead-in keeps the row from butting against the top edge.
fn scroll_row_into_view(scrolled: &gtk::ScrolledWindow, row: &hytte::adw::ActionRow) {
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
fn flash_row_highlight(row: &hytte::adw::ActionRow) {
    use hytte::gtk::glib;
    row.add_css_class("ts-cal-day-hit");
    let row_for_clear = row.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
        row_for_clear.remove_css_class("ts-cal-day-hit");
    });
}
```

- [ ] **Step 6.2: Wire `connect_day_selected`**

In `widget()`, after the month-nav handlers, add:

```rust
    {
        let rows_for_select = rows_track.clone();
        let scrolled_for_select = scrolled.clone();
        cal.connect_day_selected(move |c| {
            use chrono::Datelike;
            let gdt = c.date();
            let y = gdt.year();
            let Ok(m) = u32::try_from(gdt.month()) else {
                return;
            };
            let Ok(day) = u32::try_from(gdt.day_of_month()) else {
                return;
            };
            let Some(d) = chrono::NaiveDate::from_ymd_opt(y, m, day) else {
                return;
            };
            let rows = rows_for_select.borrow();
            let Some((_d, row)) = rows.iter().find(|(date, _)| *date == d) else {
                return;
            };
            scroll_row_into_view(&scrolled_for_select, row);
            flash_row_highlight(row);
        });
    }
```

- [ ] **Step 6.3: Wire sidebar-open → refresh**

Change `_monitor` to `monitor` in the `widget` signature (drop the leading underscore — it's now used). Then, before `column.upcast()` at the end of `widget()`, add:

```rust
    // Force a fresh scan when the user opens the sidebar — avoids
    // showing up-to-60-second-stale data on open. Edge-triggered via
    // a Cell so the initial state replay from `signal()` doesn't fire
    // a refresh when the sidebar starts closed.
    {
        use std::cell::Cell;
        use std::rc::Rc;
        use hytte::gtk::glib;

        let last_open = Rc::new(Cell::new(false));
        let last_open_for_sub = last_open.clone();
        glib::MainContext::default().spawn_local(
            crate::overlays::sidebar::open_signal(monitor).for_each(move |open| {
                let prev = last_open_for_sub.replace(open);
                if open && !prev {
                    hytte::services::calendar::refresh();
                }
                async {}
            }),
        );
    }
```

`SignalExt::for_each` is already in scope via the file's `use hytte::prelude::*;` (the prelude re-exports `SignalExt` per `crates/hytte/src/lib.rs:21`). No extra import needed.

- [ ] **Step 6.4: Verify it builds**

Run: `cargo build -p trollshell`
Expected: builds cleanly. Existing tests still pass: `cargo test -p trollshell`.

- [ ] **Step 6.5: Commit**

```bash
git add trollshell/src/widgets/calendar.rs
git commit -m "feat(widgets/calendar): click-day scroll + open-refresh" -m "connect_day_selected scrolls the events list to the first matching event and flashes a fade-out highlight. Sidebar-open transition triggers calendar::refresh so the list never shows >60s stale data."
```

---

### Task 7: CSS additions + drop `.ts-sidebar-placeholder`

**Files:**
- Modify: `trollshell/style.css`

- [ ] **Step 7.1: Remove the now-unused placeholder rule**

In `trollshell/style.css`, find this block (around line 332):

```css
.ts-sidebar-placeholder {
    color: alpha(currentColor, 0.5);
    font-style: italic;
    font-size: 13px;
}
```

Delete it (including the blank line after, if you want — match surrounding style).

- [ ] **Step 7.2: Append the calendar-widget rules**

Find the existing `.ts-cal-day-hit` block (around line 583) — that rule already targets the click-day highlight via `@accent_color` and we **reuse it** unchanged.

Add a new section above or below `.ts-cal-day-hit` (whichever keeps the file logically grouped — calendar styling is fine anywhere; the file isn't strictly ordered). Add:

```css
/* ─── Calendar widget — sidebar surface ─────────────────────────── */

.ts-sidebar-calendar {
    padding-top: 4px;
}

.ts-sidebar-cal-header {
    color: alpha(currentColor, 0.5);
    font-size: 11px;
    letter-spacing: 1.5px;
    margin: 14px 4px 6px 4px;
}

/* GtkCalendar on the dark sidebar gradient. Default light card styling
 * is wrong here — neutralise the background and lift the text. */
.ts-sidebar-calendar .ts-calendar {
    background: transparent;
    color: white;
    padding: 4px;
}
.ts-sidebar-calendar .ts-calendar > header {
    color: white;
}
.ts-sidebar-calendar .ts-calendar > grid > label {
    color: alpha(currentColor, 0.85);
}
.ts-sidebar-calendar .ts-calendar > grid > label.day-name {
    color: alpha(currentColor, 0.5);
}
.ts-sidebar-calendar .ts-calendar > grid > label.other-month {
    color: alpha(currentColor, 0.25);
}

/* adw::PreferencesGroup palette swap for the dark sidebar. The exact
 * selectors below target the inner adw `list.boxed-list` element; if
 * the rendered tree differs, the rules degrade gracefully and only the
 * row colours change. */
.ts-sidebar-cal-list {
    margin-top: 0;
}
.ts-sidebar-cal-list listview,
.ts-sidebar-cal-list list.boxed-list,
.ts-sidebar-cal-list list.boxed-list > row {
    background: alpha(white, 0.04);
    color: white;
    border-radius: 8px;
}
.ts-sidebar-cal-list .title {
    color: white;
}
.ts-sidebar-cal-list .subtitle {
    color: alpha(white, 0.6);
}
```

The existing `.ts-cal-day-hit` rule at line 583 is reused as-is — no edit needed there.

- [ ] **Step 7.3: Verify it builds**

Run: `cargo build -p trollshell`
Expected: builds cleanly. CSS errors are runtime-only (logged when the stylesheet loads), so `build` won't catch them — interactive verification is the test.

- [ ] **Step 7.4: Commit**

```bash
git add trollshell/style.css
git commit -m "style(widgets/calendar): sidebar palette + drop placeholder rule" -m "Adds .ts-sidebar-calendar, .ts-sidebar-cal-header, .ts-sidebar-cal-list rules so GtkCalendar and adw::PreferencesGroup read correctly on the dark sidebar gradient. Drops the now-unused .ts-sidebar-placeholder rule."
```

---

### Task 8: Final verification

**Files:** none touched in this task.

- [ ] **Step 8.1: Full build**

Run: `cargo build -p trollshell`
Expected: clean build, no warnings introduced by these changes.

- [ ] **Step 8.2: Full test suite**

Run: `cargo test -p trollshell`
Expected: all tests pass. New `widgets::calendar::tests` shows three passing tests.

- [ ] **Step 8.3: Run trollshell and walk the interactive verification list**

Run: `cargo run -p trollshell` (inside a Niri session, or restart the existing trollshell process).

Walk through the spec's verification list:

1. Sidebar toggle chip works — clicking opens the sidebar.
2. Placeholder label is gone; month grid + "UPCOMING" header + event list visible.
3. Days in the current month with events are marked.
4. Click `>` on calendar header — marks update for next month.
5. Click a marked day — events list scrolls to first matching event, brief highlight fades.
6. Click a non-event day — nothing happens (no scroll, no error).
7. With no GOA calendars configured (or sidebar opened before first refresh completes): list shows "No upcoming events".
8. Close + re-open the sidebar — list re-fetches via the open-edge hook.
9. Light/dark theme switch (Settings → Appearance) — calendar text colors flip.
10. Multi-monitor: open sidebar on monitor A and on monitor B — both show the same events.
11. ESC closes the sidebar from inside the events list.

If any step regresses, file an issue or note here for follow-up. Do **not** commit further fixes in this task — they belong to their own task.

- [ ] **Step 8.4: Confirm completion**

Hand-off to user: "Calendar widget landed across 8 tasks. All checkboxes complete; cargo build + tests + interactive verification all green. See commits since `414945a` for the change history."

No commit in this task — verification only.

---

## Out of Scope

(Restated from the spec for the implementing agent's awareness — **do not** add these in this plan's tasks:)

- Removing or redirecting the drawer's `panels/calendar.rs`.
- Factoring out a shared builder between drawer panel and sidebar widget.
- Per-source color coding, recurring-event expansion, custom-rendered month grid, today-summary badge, event open-in-app action.
- Reactive marks during sidebar slide-out animation.
- Refining light-mode palette beyond what falls out of the `alpha(currentColor, …)` rules.

If the implementing agent finds something in one of these areas while working, they should note it (commit message footer or a stray comment) but **not act on it** in this plan.
