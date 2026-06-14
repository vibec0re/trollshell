# Calendar Widget: second sidebar content

**Status:** design approved 2026-05-14
**Scope:** new `trollshell/src/widgets/calendar.rs`, sidebar wiring, CSS additions. Second content widget for the left sidebar landed in `2026-05-14-sidebar-design.md`. Sibling to (not replacement of) the existing drawer panel `trollshell/src/panels/calendar.rs`.

## Motivation

The sidebar shipped with a `Label("sidebar")` placeholder. The first content widget (weather) is specced in `2026-05-14-weather-widget-design.md` but not yet implemented. This task lands a second piece of content — a calendar widget showing a month grid plus the next 7 days of events — that drops into the sidebar's vertical card.

Two stacked widgets exercise the sidebar's column composition (weather is short and dense; calendar is taller and scrolls), and weather + calendar together cover the "what's going on right now in my day" use case that justifies leaving the sidebar open while working.

The existing drawer panel `panels/calendar.rs` is left in place. Sidebar and drawer surfaces have different ergonomics (always-on-and-pushing vs modal-and-overlay), so two parallel renderings of the same `calendar::events()` signal is acceptable. Consolidation (factor out a shared builder, or decommission the drawer panel) is a follow-up once both surfaces have settled.

## Design

### Placement

Sidebar order (top → bottom, once both widgets exist):

```
.ts-sidebar (vertical Box)
├── weather widget   (specced separately; not yet built)
└── calendar widget  (this spec)
```

Until weather lands, the calendar widget is the sole occupant of `.ts-sidebar`. When weather lands, it gets prepended via `card.append(&weather::widget(monitor))` before the calendar `append`. No re-design needed in either direction.

### Widget

New `trollshell/src/widgets/calendar.rs`:

```rust
pub fn widget(monitor: &Monitor) -> gtk::Widget;
```

Returns a `gtk::Box` (vertical, `.ts-sidebar-calendar`). The `monitor` parameter is taken for parity with other widget builders and for the sidebar-open subscription described below; the rendered tree itself is monitor-agnostic.

Widget structure:

```
gtk::Box (vertical, .ts-sidebar-calendar)
├── gtk::Calendar                 (.ts-calendar)
│     • show_heading = true
│     • show_day_names = true
│     • show_week_numbers = false
│     • marked days = days with ≥1 event in the currently-visible month
│     • connect_next_month / connect_prev_month → re-apply marks
│     • connect_day_selected → scroll event list to first matching row + flash
├── gtk::Label                    (.ts-sidebar-cal-header)  "Upcoming"
└── gtk::ScrolledWindow           (vertical only, min 220, max 360)
    └── adw::PreferencesGroup     (.ts-sidebar-cal-list)
        ├── adw::ActionRow per CalendarEvent
        │     title    = ev.summary
        │     subtitle = format_when(ev) + "\n" + ev.location  (if present)
        │     prefix   = gtk::Image "x-office-calendar-symbolic"
        │     activatable = false
        │     subtitle_lines = 0   (wrap)
        │     title_lines = 1
        │  …or, when empty…
        └── adw::ActionRow "No upcoming events"
              subtitle = "Add a calendar via Settings → Online Accounts."
              activatable = false
```

The widget reuses `hytte::services::calendar::format_when` (already `pub` on the service module). It does **not** import any helpers from `panels::calendar`; the helpers there (`scroll_row_into_view`, `flash_row_highlight`, `apply_event_marks`, `build_calendar_row`) are private to that file. The sidebar widget gets its own private copies for now. When both surfaces have stabilised, a follow-up factors a `widgets::calendar_block` module out and the duplication goes away.

### Behavior

**Mirrors the drawer panel one-for-one** for the parts that overlap:

1. **Bind to `calendar::events()`.** Use `hytte::prelude::bind` against the `adw::PreferencesGroup`. On each emission:
   - Drain previously-tracked event rows from the group.
   - Drain the placeholder row (if mounted).
   - Stash the new event vec in an `Rc<RefCell<…>>` so the prev/next-month handlers see fresh data.
   - Call `apply_event_marks(&cal, &evs)` to re-mark the currently-visible month.
   - If `evs.is_empty()`: build and append the placeholder row.
   - Else: build an `adw::ActionRow` per event, append, and stash the `(NaiveDate, row)` pair for click-day lookup.

2. **`apply_event_marks(cal, events)`** is a free function in this module:
   - `cal.clear_marks()`
   - For each event whose `start.date_naive()` falls in `cal.year()` + `cal.month() + 1` (GtkCalendar months are 0-indexed; chrono's are 1-indexed), insert `day()` into a `HashSet<u32>`.
   - For each day in the set, `cal.mark_day(day)`.

3. **Month navigation:** clone the events `Rc` into each of `connect_next_month` and `connect_prev_month`, re-call `apply_event_marks` on the (already-updated) calendar. GtkCalendar's `connect_next_month` correctly bumps the year on December rollover, so no `connect_year_changed` is required.

4. **Click-day → scroll-and-flash:** `connect_day_selected`:
   - Read `cal.date()` → `glib::DateTime` → extract `(year, month, day_of_month)`.
   - Build a `chrono::NaiveDate`. Bail if any conversion fails.
   - Find the first `(date, row)` tuple in the tracked-rows vec that matches the date. If none, no-op (the day wasn't marked).
   - Call `scroll_row_into_view(&scrolled, row)` and `flash_row_highlight(row)`.

5. **`scroll_row_into_view`** is a free function. Mirrors the drawer panel:
   - Translate the row's origin into the scrolled-window child's coordinate space via `row.compute_point(child, &(0,0))`.
   - Set `vadjustment().value` to `(point.y - 8.0).clamp(lower, upper - page_size)`.

6. **`flash_row_highlight`** is a free function. Mirrors the drawer panel:
   - `row.add_css_class("ts-cal-day-hit")`
   - `glib::timeout_add_local_once(Duration::from_millis(1500), …)` to remove the class.
   - The existing `.ts-cal-day-hit` CSS rule already has a 600ms transition, so the highlight fades.

### Sidebar-open refresh

To avoid showing up-to-60-second-stale data, subscribe to `sidebar::open_signal(monitor)` and call `calendar::refresh()` on the false→true edge. Spawn-local pattern that matches the weather widget's planned hook:

```rust
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
```

The `last_open` cell makes the trigger genuinely edge-based: the initial signal emission with the current state value doesn't fire a refresh, and rapid toggle bursts collapse correctly. (`signal()` emits the current value on subscribe, so a naive "if open, refresh" would refresh once on widget build even when sidebar starts closed.)

`calendar::refresh()` is already idempotent and runs the FS walk on a blocking pool, so calling it from the GTK main context is safe.

### CSS (`style.css`)

```css
.ts-sidebar-calendar {
  padding: 8px 4px 0 4px;
}

/* Header above the events list. */
.ts-sidebar-cal-header {
  color: alpha(white, 0.55);
  font-size: 11px;
  letter-spacing: 1.5px;
  margin: 10px 4px 4px 4px;
}

/* GtkCalendar lives on the dark sidebar gradient. Default light card
 * styling is wrong here; override to a transparent/translucent surface. */
.ts-sidebar-calendar .ts-calendar {
  background: transparent;
  color: white;
}
.ts-sidebar-calendar .ts-calendar > header {
  color: white;
}
.ts-sidebar-calendar .ts-calendar > grid > label {
  color: alpha(white, 0.85);
}
.ts-sidebar-calendar .ts-calendar > grid > label.day-name {
  color: alpha(white, 0.5);
}
.ts-sidebar-calendar .ts-calendar > grid > label.other-month {
  color: alpha(white, 0.25);
}
.ts-sidebar-calendar .ts-calendar > grid > label.marked {
  /* Marked-day dot uses currentColor in the GTK default; matches white text. */
  font-weight: 600;
}

/* adw::PreferencesGroup palette swap for the dark sidebar. */
.ts-sidebar-cal-list .boxed-list,
.ts-sidebar-cal-list list.boxed-list,
.ts-sidebar-cal-list list > row {
  background: alpha(white, 0.04);
  color: white;
  border-radius: 8px;
}
.ts-sidebar-cal-list list > row > box > box.title > .title {
  color: white;
}
.ts-sidebar-cal-list list > row > box > box.title > .subtitle {
  color: alpha(white, 0.6);
}

/* Click-day flash. Reuses the existing .ts-cal-day-hit transition rule
 * if present; if not, declare it here. */
.ts-sidebar-cal-list .ts-cal-day-hit {
  background: alpha(white, 0.12);
  transition: background 600ms ease;
}
```

Light-mode override mirrors `.ts-drawer`'s light-mode rule pattern (palette swap, same structural rules).

The exact rule selectors above will need to be validated against the libadwaita CSS tree when the widget is rendered — adw rules sometimes target `.boxed-list` on the inner `list` element vs the outer `PreferencesGroup`, and small selector tweaks may be needed at PR time. The wireframe is the load-bearing part; CSS specificity is mechanical.

### Sidebar wiring

`trollshell/src/overlays/sidebar.rs::install()` currently has:

```rust
let placeholder = gtk::Label::new(Some("sidebar"));
placeholder.add_css_class("ts-sidebar-placeholder");
placeholder.set_halign(gtk::Align::Center);
placeholder.set_valign(gtk::Align::Center);
placeholder.set_vexpand(true);
card.append(&placeholder);
```

Replace with:

```rust
card.append(&crate::widgets::calendar::widget(monitor));
```

Drop the `placeholder` local. Drop the (now-unused) `.ts-sidebar-placeholder` CSS rule from `style.css`.

When the weather widget lands, the resulting sequence is:

```rust
card.append(&crate::widgets::weather::widget(monitor));
card.append(&crate::widgets::calendar::widget(monitor));
```

No other sidebar wiring changes.

### Module registration

`trollshell/src/widgets/mod.rs` — add `pub mod calendar;` (the file is sidebar-context only; no other call site).

## Tests

Unit tests in `widgets/calendar.rs` (`#[cfg(test)] mod tests`):

| test                                               | scenario                                                                                                                            | expected                                                         |
| -------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| `apply_event_marks_picks_current_month_only`       | events in month X and month X-1; calendar shows month X                                                                             | only month-X days are inserted into the marked set               |
| `apply_event_marks_handles_zero_indexed_gtk_month` | call `apply_event_marks` with a calendar whose `month()` returns `cal_month_minus_one`; events on day 15 of the matching real month | day 15 ends up in the marked set (no off-by-one)                 |
| `apply_event_marks_deduplicates_per_day`           | two events both starting on the same day                                                                                            | day appears once in the marked set (HashSet semantics confirmed) |

GtkCalendar instantiation, scroll-into-view, and flash behaviour are not unit-tested — they need a real GTK display and are covered by interactive verification. The `apply_event_marks` tests can run headless because the function takes a `&gtk::Calendar` only for `month()`/`year()` reads and `clear_marks()`/`mark_day()` writes; we can pass a freshly-constructed `gtk::Calendar` without mapping it (gtk-rs allows this in `#[gtk::test]`-annotated tests or with `gtk::init()` in the test setup). If the harness doesn't allow that, the tests degrade gracefully into pure-function tests by refactoring `apply_event_marks` to compute the `HashSet<u32>` separately and call `mark_day` from a thin shim — see Verification.

Existing drawer-panel tests (none today, per the file) are unaffected.

## Touched files

- `trollshell/src/widgets/calendar.rs` — new
- `trollshell/src/widgets/mod.rs` — `pub mod calendar;`
- `trollshell/src/overlays/sidebar.rs` — replace placeholder label with `widgets::calendar::widget(monitor)`; drop the `placeholder` local
- `trollshell/style.css` — add `.ts-sidebar-calendar`, `.ts-sidebar-cal-header`, `.ts-sidebar-cal-list` rules; drop the unused `.ts-sidebar-placeholder` rule

No changes to `panels/calendar.rs`, no changes to `crates/hytte-services/src/calendar.rs`, no new dependencies, no changes to `etc/niri/frame.kdl`.

## Out of scope

- **Removing or redirecting the drawer's calendar panel.** Drawer keeps its Calendar page unchanged. Consolidation deferred per the brainstorming discussion.
- **Factoring out a shared builder** between `panels/calendar.rs` and `widgets/calendar.rs`. The two will diverge or converge in later iterations; cross-cutting refactor is a follow-up.
- **Event color-coding by calendar source.** All rows look the same. Per-source palette is a v2 task once `calendar_name` is mapped to friendly names (`I` items in `calendar.rs`).
- **Recurring-event expansion.** Already a known v2 item in the calendar service (`I` items in `calendar.rs`); this widget renders whatever the service emits.
- **Custom-rendered month grid.** We ride `gtk::Calendar`. Replacing it with a custom grid (smaller cells, different typography) is a possible polish task; not in this iteration.
- **"Today" inline summary** (à la "next event in 30 min"). Could be a small badge above the month grid later.
- **Reactive marks during sidebar slide-out.** Marks update on month nav and on each `calendar::events()` emission. We don't re-apply on every animation tick.
- **Light-mode polish iteration.** A baseline `prefers-color-scheme: light` rule is included; refining its exact palette is iterative once the widget is in user hands.
- **Event open-in-app action.** Rows stay `activatable=false`. Click-through to a calendar app is a future task and likely belongs at the service layer.

## Verification

After landing:

1. `cargo build -p trollshell` succeeds.
2. `cargo test -p trollshell` passes (existing + new unit tests).
3. Launch trollshell on niri with at least one GOA calendar provisioned.
4. Open the sidebar. The placeholder is gone; in its place sits the month grid and an "Upcoming" list of events from `~/.local/share/evolution/calendar/<uid>/calendar.ics`.
5. Days in the current month that have events are marked (bold/dot per GtkCalendar's default).
6. Click `>` on the calendar header. Marks update for the next month.
7. Click a marked day. The events list scrolls so the first event on that date is near the top; a brief highlight fades on the matching row.
8. Click a non-event day. Nothing happens (no scroll, no flash).
9. If no calendars are configured, the list shows a single "No upcoming events" row with the Settings hint subtitle.
10. Force-close and re-open the sidebar after editing a calendar event upstream. The list reflects the change within the 60 s polling window — or immediately, because the sidebar-open hook calls `calendar::refresh()` on each open.
11. Light/dark theme switch (Settings → Appearance) — the widget colors flip to the light palette without restart.
12. Multi-monitor: opening the sidebar on monitor A and on monitor B both show the same upcoming events (they share the underlying `calendar::events()` signal).
13. Hot-unplug a monitor while its sidebar is open. No panic; per `sidebar::close_all`, the surface tears down cleanly and the calendar widget's subscriptions go with it.
14. With sidebar open, switch focus into the events list and Tab through rows — no keyboard-focus traps. ESC still closes the sidebar (handled by `sidebar.rs`).
