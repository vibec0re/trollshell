# Sidebar departures: S Schöneweide S-Bahn in the left sidebar

**Status:** design approved 2026-05-14
**Scope:** new `crates/hytte-services/src/departures.rs`, new `trollshell/src/widgets/departures.rs`, edits to `trollshell/src/overlays/sidebar.rs`, new CSS in `trollshell/style.css`, registration in `trollshell/src/main.rs`.

## Motivation

The sidebar shipped with the prior spec [2026-05-14-sidebar-design.md](2026-05-14-sidebar-design.md) lives on every monitor as a 320-px left surface and currently shows a single placeholder label `"sidebar"`. The mechanics (push/reflow, frame integration, animation, lifecycle) are in place. We now want real content in it.

For Phase 2 content, ship **current S-Bahn departures from S Schöneweide** — the user's home station. The goal is a glanceable departure list, same mental model as the DFI boards on the platform: a vertical stack of the next eight trains, each row showing line, destination, "X min from now · HH:MM" and (when relevant) delay or cancellation.

This module is the first concrete sidebar payload; future panels (calendar, weather, system stats) can land alongside it and the architecture chosen here should leave room for that.

## Design

### Three pieces

| piece              | file                                              | role                                                                                                                                                             |
| ------------------ | ------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| departures service | `crates/hytte-services/src/departures.rs` _(new)_ | background 15-minute poll of v6.bvg.transport.rest, exposes `Mutable<DeparturesState>` and `refresh()`.                                                          |
| departures widget  | `trollshell/src/widgets/departures.rs` _(new)_    | GTK vertical list of departure rows, subscribes to service signal, re-renders relative time on every clock tick.                                                 |
| sidebar wiring     | `trollshell/src/overlays/sidebar.rs` _(edit)_     | replaces the placeholder `Label` with the widget; nudges `departures::refresh()` on the open false→true edge so a freshly-opened sidebar reflects current state. |

Service registration lives in `main.rs`, parallel to `clock::service()`.

### Service: `hytte-services/src/departures.rs`

```rust
pub const SCHOENEWEIDE_ID: &str = "900180001";  // BVG/HAFAS ID for S Schöneweide
pub const POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);
pub const STALE_DROP_AFTER: Duration = Duration::from_secs(30 * 60);
pub const RESULTS: usize = 8;
pub const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Departure {
    pub line: String,                // e.g., "S9"
    pub direction: String,           // e.g., "Spandau"
    pub planned: DateTime<Local>,    // scheduled time
    pub actual:  DateTime<Local>,    // scheduled + delay
    pub delay_minutes: i64,          // 0 on time, negative when early
    pub cancelled: bool,
    pub trip_id: String,             // stable key for diffing/logging
}

#[derive(Clone, Debug)]
pub enum DeparturesState {
    Loading,
    Ok    { at: DateTime<Local>, items: Vec<Departure> },
    Stale { at: DateTime<Local>, items: Vec<Departure>, err: String },
    Err   { err: String },
}

pub struct DeparturesService;

#[derive(Clone)]
#[doc(hidden)]
pub struct DeparturesHandles {
    pub(crate) state: Mutable<DeparturesState>,
    pub(crate) notify: Arc<tokio::sync::Notify>,
}

impl Service for DeparturesService { /* … */ }

#[must_use] pub fn service() -> DeparturesService { DeparturesService }
pub fn current() -> impl Signal<Item = DeparturesState>;
pub fn refresh();   // wakes the poll task once
```

**Loop body (background task spawned by `Service::start`):**

```rust
async fn run(state: Mutable<DeparturesState>, notify: Arc<Notify>) {
    let in_flight = Arc::new(AtomicBool::new(false));
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        // Either the 15-min tick or a refresh nudge wakes us.
        tokio::select! {
            _ = tick.tick() => {}
            _ = notify.notified() => {}
        }
        if in_flight.swap(true, Ordering::SeqCst) { continue; }
        let result = tokio::task::spawn_blocking(fetch_once).await
            .unwrap_or_else(|e| Err(format!("join: {e}")));
        in_flight.store(false, Ordering::SeqCst);

        update_state(&state, result);
    }
}

fn fetch_once() -> Result<Vec<Departure>, String> {
    let url = format!(
        "https://v6.bvg.transport.rest/stops/{SCHOENEWEIDE_ID}/departures\
         ?results={RESULTS}&suburban=true&subway=false&bus=false&tram=false\
         &regional=false&express=false&ferry=false&tariff=false&language=de"
    );
    // ureq 3.x agent with connect + global request timeout. Exact builder
    // calls follow whatever shape this crate-version exposes — match the
    // pattern in `crates/hytte-services/src/mpris.rs` and add timeouts via
    // `ureq::config::Config::builder()`.
    let agent: ureq::Agent = build_agent_with_timeouts();
    let mut resp = agent.get(&url).call().map_err(|e| format!("http: {e}"))?;
    let body: ApiResponse = resp.body_mut().read_json()
        .map_err(|e| format!("decode: {e}"))?;
    Ok(body.departures.into_iter().filter_map(into_departure).collect())
}
```

**State transition rules** (in `update_state`):

| previous                                 | fetch result | next                                                       |
| ---------------------------------------- | ------------ | ---------------------------------------------------------- |
| any                                      | `Ok(items)`  | `Ok { at: now, items }`                                    |
| `Ok` or `Stale (age < STALE_DROP_AFTER)` | `Err(e)`     | `Stale { at: previous.at, items: previous.items, err: e }` |
| `Stale (age ≥ STALE_DROP_AFTER)`         | `Err(e)`     | `Err { err: e }`                                           |
| `Loading` or `Err`                       | `Err(e)`     | `Err { err: e }`                                           |

**Server-side filter + client-side guard:** the URL excludes every non-S-Bahn product (`suburban=true` plus all others false). As a defensive layer, `into_departure` also discards rows whose `line.product` is not `"suburban"` — protects against transport.rest interpreting unknown query params loosely.

**Past-departure filter:** rows whose `actual < now - 60s` are dropped at parse time (one minute of lag tolerated for clock skew). With the 8-row cap and 15-min poll, this prevents stale rows accumulating between refreshes.

**JSON shape** (only the fields we read — serde structs are `#[serde(default)]` on every field so the upstream adding new keys doesn't break us):

```jsonc
{
  "departures": [
    {
      "tripId": "1|123|0|80|14052026",
      "when": "2026-05-14T16:43:00+02:00", // actual
      "plannedWhen": "2026-05-14T16:42:00+02:00",
      "delay": 60, // seconds, or null
      "cancelled": false,
      "direction": "Spandau",
      "line": { "name": "S9", "product": "suburban" },
    },
  ],
}
```

`delay_minutes` is `delay / 60` (`i64`), clamped to `0` when null/missing. `actual` is parsed from `when`, falling back to `plannedWhen` when `when` is null (cancelled rows).

### Widget: `trollshell/src/widgets/departures.rs`

Public surface:

```rust
pub fn widget() -> gtk::Widget;
```

Internally:

```rust
pub fn widget() -> gtk::Widget {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 6);
    list.add_css_class("ts-departures");
    list.set_valign(gtk::Align::Start);

    // hytte-reactive's `bind` is (signal, &widget, apply) — signal first.
    bind(departures::current(), &list, |list, state| {
        rebuild(list, &state);
    });

    list.upcast()
}

fn rebuild(list: &gtk::Box, state: &DeparturesState) {
    // Drain existing children. gtk::Box has no `remove_all_children`; the
    // codebase uses the `first_child` / `remove` loop everywhere this is
    // needed (e.g. trollshell/src/panels/bluetooth.rs:175).
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    match state {
        DeparturesState::Loading       => list.append(&loading_row()),
        DeparturesState::Err   { err } => list.append(&error_row(err)),
        DeparturesState::Ok    { items, .. } |
        DeparturesState::Stale { items, .. } => {
            if items.is_empty() {
                list.append(&empty_row());
            } else {
                for d in items { list.append(&row(d)); }
            }
            if let DeparturesState::Stale { err, at, .. } = state {
                list.append(&stale_footer(err, *at));
            }
        }
    }
}
```

`row(d)` builds:

```
gtk::Box horizontal, class "ts-departure-row" (+ "ts-cancelled" iff d.cancelled)
├── gtk::Label "S9"          classes ["ts-line-badge", "ts-line-S9"]
├── gtk::Label "Spandau"     class  "ts-departure-direction" (hexpand, ellipsize end)
├── gtk::Label time_str      class  "ts-departure-time"
└── gtk::Label "+1"          class  "ts-departure-delay"   (visible iff delay_minutes > 0)
```

`time_str` re-binds on every `clock::now()` emission:

```rust
let actual = d.actual;
bind(clock::now(), &time_lbl, move |lbl, now| {
    let rel = relative_label(now, actual);
    lbl.set_text(&format!("{rel} · {}", actual.format("%H:%M")));
});
```

`relative_label`:

| seconds until departure  | label                                            |
| ------------------------ | ------------------------------------------------ |
| `<= 60` (incl. negative) | `"now"`                                          |
| `60 < s ≤ 90`            | `"1 min"`                                        |
| `s > 90`                 | `"{(s + 30) / 60} min"` (rounded to nearest min) |

`loading_row` / `error_row` / `empty_row` / `stale_footer` are one-line `gtk::Label` builders with the corresponding CSS class. Names of those classes are defined under "CSS" below.

### Sidebar wiring: `trollshell/src/overlays/sidebar.rs` edits

Two changes, both small:

1. Replace the placeholder card content. In `install`, swap:

   ```rust
   let placeholder = gtk::Label::new(Some("sidebar"));
   placeholder.add_css_class("ts-sidebar-placeholder");
   placeholder.set_halign(gtk::Align::Center);
   placeholder.set_valign(gtk::Align::Center);
   placeholder.set_vexpand(true);
   card.append(&placeholder);
   ```

   for:

   ```rust
   card.append(&crate::widgets::departures::widget());
   ```

2. Nudge a refresh on the open false→true edge. Inside the `open_state.signal().for_each(...)` closure that's already there, change:
   ```rust
   if open {
       window_for_open.set_visible(true);
       window_for_open.present();
       window_for_open.set_exclusive_zone(SIDEBAR_WIDTH - FRAME_THICKNESS_I32);
       revealer_for_open.set_reveal_child(true);
   } else { /* … */ }
   ```
   to add a single call after the existing `revealer_for_open.set_reveal_child(true);`:
   ```rust
   hytte::services::departures::refresh();
   ```
   (Re-exported path through the `hytte` umbrella crate, matching the import style in `main.rs`.) The notify is coalesced, so multiple monitors opening in quick succession only produce one fetch.

The `.ts-sidebar-placeholder` CSS rule becomes dead and is removed.

### Registration: `trollshell/src/main.rs`

Services register on the `App` builder via `.with(…)`. Two edits:

1. Add `departures` to the `use hytte::services::{…}` import group at the top of `main.rs` (alphabetical order, between `displays` and `dnd`).
2. Add one line to the `.with(…)` chain inside `App::new(…)`:
   ```rust
   .with(departures::service())
   ```
   Position alongside `clock::service()` / `calendar::service()` — order within the chain isn't load-bearing, but follow the rough thematic grouping already there.

### CSS: `trollshell/style.css`

New rules (insert in the existing dark-mode section, adjacent to the `.ts-sidebar` block):

```css
.ts-departures {
  padding: 12px;
}

.ts-departure-row {
  padding: 6px 4px;
}

.ts-line-badge {
  min-width: 36px;
  padding: 2px 6px;
  border-radius: 6px;
  font-weight: 700;
  color: white;
  margin-right: 8px;
}
.ts-line-S8 {
  background: #5dab46;
} /* dark green */
.ts-line-S9 {
  background: #882d7a;
} /* violet */
.ts-line-S41 {
  background: #aa5d3d;
}
.ts-line-S42 {
  background: #c36f33;
}
.ts-line-S46 {
  background: #c4923d;
} /* chestnut */
.ts-line-S47 {
  background: #c4923d;
}
.ts-line-S85 {
  background: #a7c539;
} /* yellow-green */

.ts-departure-direction {
  color: alpha(white, 0.95);
}

.ts-departure-time {
  color: alpha(white, 0.85);
  font-variant-numeric: tabular-nums;
}
.ts-departure-delay {
  color: #ff6b6b;
  margin-left: 6px;
  font-variant-numeric: tabular-nums;
}

.ts-departure-row.ts-cancelled .ts-departure-time,
.ts-departure-row.ts-cancelled .ts-departure-direction {
  text-decoration: line-through;
  color: alpha(#ff6b6b, 0.7);
}

.ts-departures-loading,
.ts-departures-empty,
.ts-departures-stale-footer {
  color: alpha(white, 0.45);
  font-style: italic;
  padding: 12px;
}

.ts-departures-error {
  color: #ff6b6b;
  padding: 12px;
}
```

Light-mode override mirrors the existing `.ts-sidebar` light rule: same per-line colors (S-Bahn line colors are theme-independent), body text flips to dark, `alpha(white, ...)` becomes `alpha(black, ...)`.

The dead `.ts-sidebar-placeholder` rule is removed in the same edit.

### Cargo deps

No new workspace deps. `hytte-services` already has `ureq`, `serde`, `serde_json`, `chrono`, `tokio`, `futures-signals`.

## Touched files

- `crates/hytte-services/src/departures.rs` — new.
- `crates/hytte-services/src/lib.rs` — `pub mod departures;`.
- `crates/hytte-services/tests/fixtures/departures-schoeneweide.json` — new (test fixture).
- `trollshell/src/widgets/departures.rs` — new.
- `trollshell/src/widgets/mod.rs` — `pub mod departures;`.
- `trollshell/src/overlays/sidebar.rs` — swap placeholder for departures widget; call `departures::refresh()` on open.
- `trollshell/src/main.rs` — register `departures::service()`.
- `trollshell/style.css` — `.ts-departures*`, `.ts-departure-row`, `.ts-line-badge` + per-line colors, light-mode mirror; remove `.ts-sidebar-placeholder`.

## Tests

`#[cfg(test)] mod tests` in `hytte-services/src/departures.rs`:

| test                                             | scenario                                                                     | expected                                                            |
| ------------------------------------------------ | ---------------------------------------------------------------------------- | ------------------------------------------------------------------- |
| `parse_normal_response`                          | feed the fixture JSON into the parser                                        | 8 `Departure`s with correct line, direction, planned, actual, delay |
| `parse_with_delay_and_cancellation`              | fixture-derived sample: one `cancelled: true`, one `delay: 300`              | `cancelled` flag set; `delay_minutes == 5` on that row              |
| `parse_empty_array`                              | `{"departures": []}`                                                         | `Ok(items: vec![])`, not `Err`                                      |
| `parse_malformed_json`                           | truncated body                                                               | parser returns `Err`                                                |
| `parse_filters_non_suburban`                     | injected row with `line.product == "bus"`                                    | row dropped (defensive client-side filter)                          |
| `parse_hides_already_departed`                   | row with `when` 2 minutes in the past                                        | row dropped                                                         |
| `state_transitions_ok_to_stale_on_error`         | seed `Ok`, simulate fetch error                                              | new state is `Stale` keeping old `items`                            |
| `state_transitions_stale_to_err_after_threshold` | seed `Stale` with `at` older than `STALE_DROP_AFTER`, simulate another error | result is `Err`                                                     |
| `state_transitions_err_to_ok_on_success`         | seed `Err`, simulate ok fetch                                                | new state is `Ok`                                                   |

State-transition tests call a pure `next_state(prev, fetch_result, now)` helper — keeps the test loop synchronous and trivially deterministic.

`#[cfg(test)] mod tests` in `trollshell/src/widgets/departures.rs`:

| test                        | scenario                            | expected                                                                              |
| --------------------------- | ----------------------------------- | ------------------------------------------------------------------------------------- |
| `relative_label_now`        | departure 30 s in the future        | `"now"`                                                                               |
| `relative_label_one_minute` | departure 75 s in the future        | `"1 min"`                                                                             |
| `relative_label_rounds`     | departure 449 s in the future       | `"7 min"`; 451 s → `"8 min"`                                                          |
| `relative_label_negative`   | departure 30 s in the past          | `"now"`                                                                               |
| `format_time_with_delay`    | delay = 2, actual = some `DateTime` | composed string contains `" · HH:MM"` and the row's separate delay label reads `"+2"` |
| `format_time_without_delay` | delay = 0                           | no `+N` label visible (helper returns `None`)                                         |

These are pure-function tests on the formatting helpers — no GTK widget instantiation needed.

Test fixture (`crates/hytte-services/tests/fixtures/departures-schoeneweide.json`) is a single real captured response from
`https://v6.bvg.transport.rest/stops/900180001/departures?results=8&suburban=true&...`, ~5 KB, committed to the repo for parser tests. Captured-at timestamp is documented in a sibling `.note` file.

No integration test against the live API — flaky by nature; the parse tests cover the contract.

## Out of scope

- Multiple stations / station switcher.
- Trams and buses from the Schöneweide stop complex.
- Reachability / "walk to platform" budget.
- Click-a-row → open in BVG app / browser.
- Configurable station ID, refresh interval, or results count via TOML/env.
- Persistent on-disk cache between app launches.
- Right-side sidebar or stacked sidebar pages.
- Light-mode color tuning beyond the mirror-of-`.ts-sidebar` rule.

## Verification

1. `cargo build -p hytte-services -p trollshell` succeeds; `cargo test -p hytte-services -p trollshell` passes (new + existing tests).
2. Launch trollshell on niri. Open the sidebar via the bar chip. Within ~1 s a list of 8 S-Bahn departures from S Schöneweide appears, line badges colored (S8 dark green, S9 violet, S46/S47 chestnut, S85 yellow-green), each row reads `LINE  Direction  X min · HH:MM` with `+N` in red where applicable.
3. Watch the top row's `X min` cell tick down with each wall-clock minute, without any visible re-fetch or list rebuild.
4. Network test: turn Wi-Fi off after the first successful load, wait for the next 15-min poll (or close+reopen the sidebar to trigger a manual refresh). The list stays populated and a thin footer reads `· stale (…)`. Re-enable Wi-Fi → next refresh removes the footer.
5. Cold-offline test: launch trollshell with Wi-Fi off, open the sidebar. The widget shows the "error" row in red.
6. Cancellation test: confirm a known cancelled departure (replay-fixture or real) renders with strike-through and red tint.
7. Multi-monitor: open the sidebar on monitor A and monitor B; both show identical lists. Backend logs show one fetch in flight at a time (the in-flight semaphore coalesces parallel refresh nudges).
8. Hot-plug: unplug a monitor while its sidebar is open. No panic; the service continues; the remaining monitor's sidebar is unaffected.
