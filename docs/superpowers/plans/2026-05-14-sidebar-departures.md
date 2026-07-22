# Sidebar Departures Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the sidebar's placeholder label with a live list of the next 8 S-Bahn departures from S Schöneweide, refreshed every 15 minutes and on sidebar open, sourced from v6.bvg.transport.rest.

**Architecture:** A new `hytte-services::departures` service polls the public BVG proxy on a tokio task, exposing a `Mutable<DeparturesState>` and a `refresh()` nudge over a `tokio::sync::Notify`. A new `trollshell::widgets::departures` widget subscribes to the service and re-renders relative timestamps each second from `clock::now()`. The sidebar swaps its placeholder for that widget and nudges `refresh()` on every open transition.

**Tech Stack:** Rust 2024 edition, GTK4 (`gtk4` crate v0.11.x), `futures-signals`, `tokio`, `ureq` 3.x with rustls, `serde` + `serde_json`, `chrono` (Local), `hytte-reactive` (`Service` + `registry::with` + `bind`).

**Spec:** [`docs/superpowers/specs/2026-05-14-sidebar-departures-design.md`](../specs/2026-05-14-sidebar-departures-design.md)

---

## File map

**Create:**

- `crates/hytte-services/src/departures.rs` — service: types, parser, state transitions, fetch, tokio poll task, public API.
- `crates/hytte-services/tests/fixtures/departures-schoeneweide.json` — hand-crafted fixture for parser tests (one normal, one delayed, one cancelled, one non-suburban).
- `trollshell/src/widgets/departures.rs` — widget: row builder, state→list rebuild, time-tick subscription, status rows (loading/error/empty/stale).

**Modify:**

- `crates/hytte-services/src/lib.rs` — add `pub mod departures;`.
- `trollshell/src/widgets/mod.rs` — add `pub mod departures;`.
- `trollshell/src/overlays/sidebar.rs` — replace placeholder `Label` with `widgets::departures::widget()`; call `hytte::services::departures::refresh()` on open.
- `trollshell/src/main.rs` — add `departures` to the `use hytte::services::{…}` import and to the `.with(…)` chain.
- `trollshell/style.css` — add `.ts-departures*` / `.ts-departure-row` / `.ts-line-badge` rules + light-mode mirror; delete the dead `.ts-sidebar-placeholder` rule.

---

## Task 1: Add the empty module to hytte-services

**Files:**

- Create: `crates/hytte-services/src/departures.rs`
- Modify: `crates/hytte-services/src/lib.rs:1-30`

- [ ] **Step 1: Create empty module file**

Create `crates/hytte-services/src/departures.rs` with this exact content:

```rust
//! Polled S-Bahn departures from S Schöneweide, sourced from
//! v6.bvg.transport.rest.
//!
//! A 15-minute tokio loop fetches the next 8 suburban-rail departures and
//! exposes them through a [`Mutable<DeparturesState>`]. Consumers subscribe
//! via [`current()`]. The sidebar's open-edge handler nudges [`refresh()`]
//! to keep the freshly-opened list current without waiting for the next
//! poll tick.
```

- [ ] **Step 2: Register the module**

Edit `crates/hytte-services/src/lib.rs`. Locate the existing `pub mod …;` block (the one with `bluetooth`, `calendar`, `clock`, …). Insert this line between `pub mod clock;` and `pub mod displays;` (alphabetical):

```rust
pub mod departures;
```

- [ ] **Step 3: Compile**

Run: `cargo check -p hytte-services`
Expected: success, no warnings related to departures (an unused-mod warning is fine — we'll address it next).

- [ ] **Step 4: Commit**

```bash
git add crates/hytte-services/src/departures.rs crates/hytte-services/src/lib.rs
git commit -m "feat(departures): empty module skeleton in hytte-services"
```

---

## Task 2: Public types and constants

**Files:**

- Modify: `crates/hytte-services/src/departures.rs`

- [ ] **Step 1: Add constants and `Departure` type**

Append to `crates/hytte-services/src/departures.rs`:

```rust
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};
use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_reactive::{registry, Service};
use tokio::sync::Notify;

// ── Configuration ───────────────────────────────────────────────────────────

/// BVG/HAFAS station ID for "S Schöneweide". Stable; verified at:
/// `https://v6.bvg.transport.rest/locations?query=schöneweide`.
pub const SCHOENEWEIDE_ID: &str = "900180001";

/// Background poll cadence. The sidebar's open-edge handler additionally
/// kicks [`refresh()`] for an immediate fetch.
pub const POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// After this much time elapses since the last successful fetch, a
/// continuing error transitions `Stale` → `Err` so the user sees the
/// list has gone cold.
pub const STALE_DROP_AFTER: Duration = Duration::from_secs(30 * 60);

/// How many departures to request and display.
pub const RESULTS: usize = 8;

pub const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(10);

// ── Public types ────────────────────────────────────────────────────────────

/// One upcoming S-Bahn departure, ready for rendering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Departure {
    /// Line label, e.g. `"S9"`.
    pub line: String,
    /// Destination string, e.g. `"Spandau"`.
    pub direction: String,
    /// Scheduled local departure time.
    pub planned: DateTime<Local>,
    /// Actual local departure time (= planned + delay).
    pub actual: DateTime<Local>,
    /// Lateness in minutes. `0` when on time; negative if early.
    pub delay_minutes: i64,
    /// `true` for explicitly cancelled rows.
    pub cancelled: bool,
    /// HAFAS trip identifier, stable across refreshes for a given run.
    pub trip_id: String,
}

/// The whole service surface, observed by the widget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeparturesState {
    /// Initial value before the first fetch returns.
    Loading,
    /// Most recent fetch succeeded; `at` is when it landed.
    Ok { at: DateTime<Local>, items: Vec<Departure> },
    /// A previous fetch succeeded and a later one failed; keep showing
    /// the prior list with a "stale" hint, up to `STALE_DROP_AFTER`.
    Stale { at: DateTime<Local>, items: Vec<Departure>, err: String },
    /// No usable data on hand and the latest fetch failed.
    Err { err: String },
}

impl Default for DeparturesState {
    fn default() -> Self {
        Self::Loading
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p hytte-services`
Expected: success. The unused-import warnings for `Arc`/`Notify`/`Service`/etc. are fine — they get used in later tasks.

- [ ] **Step 3: Commit**

```bash
git add crates/hytte-services/src/departures.rs
git commit -m "feat(departures): Departure and DeparturesState types"
```

---

## Task 3: `delay_string` helper (pure)

**Files:**

- Modify: `crates/hytte-services/src/departures.rs`

A small helper used by the widget; lives in the service module because its rules (when to show, what to format) belong with the data definition.

- [ ] **Step 1: Write the failing tests**

Append to `crates/hytte-services/src/departures.rs`:

```rust
/// Formats the delay indicator shown after the time cell. `None` means
/// "render no badge"; `Some("+5")` means render `+5` in the delay style.
/// We only surface lateness — negative deltas (early trains) are silent
/// since they're not actionable to the passenger.
#[must_use]
pub fn delay_string(delay_minutes: i64) -> Option<String> {
    todo!("Task 3 step 3")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_string_hidden_when_on_time() {
        assert_eq!(delay_string(0), None);
    }

    #[test]
    fn delay_string_hidden_when_early() {
        assert_eq!(delay_string(-2), None);
    }

    #[test]
    fn delay_string_shows_when_late() {
        assert_eq!(delay_string(5), Some("+5".to_string()));
    }
}
```

- [ ] **Step 2: Run tests to confirm they fail**

Run: `cargo test -p hytte-services departures::tests::delay_string 2>&1 | tail -30`
Expected: all three tests fail with `not yet implemented`.

- [ ] **Step 3: Implement**

Replace the `todo!` body with:

```rust
pub fn delay_string(delay_minutes: i64) -> Option<String> {
    if delay_minutes > 0 {
        Some(format!("+{delay_minutes}"))
    } else {
        None
    }
}
```

- [ ] **Step 4: Re-run tests**

Run: `cargo test -p hytte-services departures::tests::delay_string`
Expected: all three pass.

- [ ] **Step 5: Commit**

```bash
git add crates/hytte-services/src/departures.rs
git commit -m "feat(departures): delay_string helper"
```

---

## Task 4: Hand-crafted JSON fixture

**Files:**

- Create: `crates/hytte-services/tests/fixtures/departures-schoeneweide.json`

- [ ] **Step 1: Create the fixture directory and file**

Run: `mkdir -p crates/hytte-services/tests/fixtures`

Create `crates/hytte-services/tests/fixtures/departures-schoeneweide.json` with this exact content. The four entries cover the four cases we test: normal on-time, delayed, cancelled, and a non-suburban product that the parser must drop.

```json
{
  "departures": [
    {
      "tripId": "trip-1-ontime",
      "when": "2030-01-01T16:42:00+01:00",
      "plannedWhen": "2030-01-01T16:42:00+01:00",
      "delay": 0,
      "cancelled": false,
      "direction": "Spandau",
      "line": { "name": "S9", "product": "suburban" }
    },
    {
      "tripId": "trip-2-delayed",
      "when": "2030-01-01T16:49:00+01:00",
      "plannedWhen": "2030-01-01T16:44:00+01:00",
      "delay": 300,
      "cancelled": false,
      "direction": "Königs Wusterhausen",
      "line": { "name": "S46", "product": "suburban" }
    },
    {
      "tripId": "trip-3-cancelled",
      "when": null,
      "plannedWhen": "2030-01-01T16:49:00+01:00",
      "delay": null,
      "cancelled": true,
      "direction": "Wildau",
      "line": { "name": "S8", "product": "suburban" }
    },
    {
      "tripId": "trip-4-bus-noise",
      "when": "2030-01-01T16:50:00+01:00",
      "plannedWhen": "2030-01-01T16:50:00+01:00",
      "delay": 0,
      "cancelled": false,
      "direction": "Bus stop bus stop",
      "line": { "name": "164", "product": "bus" }
    }
  ]
}
```

Note: dates are intentionally in 2030 so the past-departure filter doesn't drop them in tests.

- [ ] **Step 2: Commit the fixture**

```bash
git add crates/hytte-services/tests/fixtures/departures-schoeneweide.json
git commit -m "test(departures): hand-crafted fixture covering 4 row shapes"
```

---

## Task 5: API serde structs + `into_departure` conversion

**Files:**

- Modify: `crates/hytte-services/src/departures.rs`

- [ ] **Step 1: Add the internal API structs and `into_departure` stub**

Append to `crates/hytte-services/src/departures.rs` (before the `#[cfg(test)]` module):

```rust
// ── Wire format ─────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Debug)]
struct ApiResponse {
    #[serde(default)]
    departures: Vec<ApiDeparture>,
}

#[derive(serde::Deserialize, Debug)]
struct ApiDeparture {
    #[serde(default)]
    #[serde(rename = "tripId")]
    trip_id: String,
    #[serde(default)]
    when: Option<String>,
    #[serde(default)]
    #[serde(rename = "plannedWhen")]
    planned_when: Option<String>,
    #[serde(default)]
    delay: Option<i64>,
    #[serde(default)]
    cancelled: bool,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    line: Option<ApiLine>,
}

#[derive(serde::Deserialize, Debug)]
struct ApiLine {
    #[serde(default)]
    name: String,
    #[serde(default)]
    product: String,
}

/// Convert one wire-format row into a [`Departure`], dropping rows we
/// can't render. Returns `None` for non-suburban products, rows that
/// already departed (more than 60 s in the past), and rows whose
/// timestamps fail to parse. The 60 s grace covers small clock skew.
fn into_departure(row: ApiDeparture, now: DateTime<Local>) -> Option<Departure> {
    todo!("Task 5 step 3")
}
```

- [ ] **Step 2: Write failing tests**

Add inside the existing `#[cfg(test)] mod tests`:

```rust
    use chrono::TimeZone;

    fn future_now() -> DateTime<Local> {
        // 2030-01-01T16:00:00+01:00 — before every fixture row.
        Local.with_ymd_and_hms(2030, 1, 1, 16, 0, 0).unwrap()
    }

    fn load_fixture() -> ApiResponse {
        let raw = include_str!(
            "../tests/fixtures/departures-schoeneweide.json"
        );
        serde_json::from_str(raw).expect("fixture parses")
    }

    #[test]
    fn into_departure_keeps_normal_row() {
        let api = load_fixture();
        let now = future_now();
        let row = api.departures.into_iter().next().unwrap();
        let d = into_departure(row, now).expect("row should be kept");
        assert_eq!(d.line, "S9");
        assert_eq!(d.direction, "Spandau");
        assert_eq!(d.delay_minutes, 0);
        assert!(!d.cancelled);
        assert_eq!(d.trip_id, "trip-1-ontime");
    }

    #[test]
    fn into_departure_keeps_delayed_row() {
        let api = load_fixture();
        let now = future_now();
        let row = api.departures.into_iter().nth(1).unwrap();
        let d = into_departure(row, now).expect("row should be kept");
        assert_eq!(d.line, "S46");
        assert_eq!(d.delay_minutes, 5);
        // Actual = planned + 5 min.
        assert_eq!(d.actual - d.planned, chrono::Duration::seconds(300));
    }

    #[test]
    fn into_departure_keeps_cancelled_row_with_planned_time() {
        let api = load_fixture();
        let now = future_now();
        let row = api.departures.into_iter().nth(2).unwrap();
        let d = into_departure(row, now).expect("row should be kept");
        assert!(d.cancelled);
        // `when` is null on cancelled — fall back to plannedWhen.
        assert_eq!(d.actual, d.planned);
    }

    #[test]
    fn into_departure_drops_non_suburban() {
        let api = load_fixture();
        let now = future_now();
        let row = api.departures.into_iter().nth(3).unwrap();
        assert!(into_departure(row, now).is_none());
    }

    #[test]
    fn into_departure_drops_already_departed() {
        let api = load_fixture();
        // now > every fixture timestamp.
        let now = Local.with_ymd_and_hms(2030, 1, 1, 17, 0, 0).unwrap();
        let row = api.departures.into_iter().next().unwrap();
        assert!(into_departure(row, now).is_none());
    }
```

Add `serde_json` to dev-deps? — it's already in `[dependencies]` of `hytte-services`, so available transitively in tests.

- [ ] **Step 3: Run the tests, expect failures**

Run: `cargo test -p hytte-services departures::tests::into_departure 2>&1 | tail -30`
Expected: all five tests panic with `not yet implemented`.

- [ ] **Step 4: Implement `into_departure`**

Replace the `todo!` body with:

```rust
fn into_departure(row: ApiDeparture, now: DateTime<Local>) -> Option<Departure> {
    let line = row.line?;
    if line.product != "suburban" {
        return None;
    }
    let line_name = line.name;
    if line_name.is_empty() {
        return None;
    }

    let planned_raw = row.planned_when.as_deref()?;
    let planned: DateTime<Local> = DateTime::parse_from_rfc3339(planned_raw)
        .ok()?
        .with_timezone(&Local);

    let actual_raw = row.when.as_deref().unwrap_or(planned_raw);
    let actual: DateTime<Local> = DateTime::parse_from_rfc3339(actual_raw)
        .ok()?
        .with_timezone(&Local);

    // Drop departures more than 60 s in the past.
    if actual < now - chrono::Duration::seconds(60) {
        return None;
    }

    let delay_seconds = row.delay.unwrap_or(0);
    let delay_minutes = delay_seconds / 60;

    Some(Departure {
        line: line_name,
        direction: row.direction.unwrap_or_default(),
        planned,
        actual,
        delay_minutes,
        cancelled: row.cancelled,
        trip_id: row.trip_id,
    })
}
```

- [ ] **Step 5: Re-run tests**

Run: `cargo test -p hytte-services departures::tests::into_departure`
Expected: all five pass.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/departures.rs
git commit -m "feat(departures): JSON parsing and row conversion"
```

---

## Task 6: `parse_response` end-to-end

**Files:**

- Modify: `crates/hytte-services/src/departures.rs`

- [ ] **Step 1: Add the stub and tests**

Just after `into_departure`, append:

```rust
/// Parse a raw response body into a `Vec<Departure>`, filtering as
/// described on [`into_departure`].
fn parse_response(body: &str, now: DateTime<Local>) -> Result<Vec<Departure>, String> {
    todo!("Task 6 step 3")
}
```

Inside `#[cfg(test)] mod tests`, add:

```rust
    #[test]
    fn parse_response_drops_bus_keeps_three_suburban() {
        let body = include_str!("../tests/fixtures/departures-schoeneweide.json");
        let parsed = parse_response(body, future_now()).unwrap();
        assert_eq!(parsed.len(), 3);
        // Order preserved from the wire format.
        assert_eq!(parsed[0].line, "S9");
        assert_eq!(parsed[1].line, "S46");
        assert_eq!(parsed[2].line, "S8");
    }

    #[test]
    fn parse_response_empty_array() {
        let parsed = parse_response(r#"{"departures": []}"#, future_now()).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn parse_response_malformed_json_is_err() {
        let err = parse_response("{not json", future_now()).unwrap_err();
        assert!(err.to_lowercase().contains("decode"), "got: {err}");
    }
```

- [ ] **Step 2: Run tests, expect failures**

Run: `cargo test -p hytte-services departures::tests::parse_response 2>&1 | tail -20`
Expected: three failures with `not yet implemented` / `not yet implemented`.

- [ ] **Step 3: Implement**

Replace the `todo!` body with:

```rust
fn parse_response(body: &str, now: DateTime<Local>) -> Result<Vec<Departure>, String> {
    let api: ApiResponse =
        serde_json::from_str(body).map_err(|e| format!("decode: {e}"))?;
    Ok(api
        .departures
        .into_iter()
        .filter_map(|r| into_departure(r, now))
        .collect())
}
```

- [ ] **Step 4: Re-run tests**

Run: `cargo test -p hytte-services departures::tests::parse_response`
Expected: all three pass.

- [ ] **Step 5: Commit**

```bash
git add crates/hytte-services/src/departures.rs
git commit -m "feat(departures): parse_response wraps deserialize + filter"
```

---

## Task 7: `next_state` transition function

**Files:**

- Modify: `crates/hytte-services/src/departures.rs`

- [ ] **Step 1: Add the stub and tests**

Append after `parse_response`:

```rust
/// Apply a fetch result to the current state and return the next state.
/// Pure function so the transition rules can be unit-tested without any
/// runtime. The rules are:
///
/// | previous                                                  | result   | next                                  |
/// |-----------------------------------------------------------|----------|---------------------------------------|
/// | any                                                       | `Ok`     | `Ok { at: now, items }`               |
/// | `Ok` or `Stale` with `now - at < STALE_DROP_AFTER`        | `Err(e)` | `Stale { at, items, err: e }`         |
/// | `Stale` with `now - at >= STALE_DROP_AFTER`               | `Err(e)` | `Err { err: e }`                      |
/// | `Loading` or `Err`                                        | `Err(e)` | `Err { err: e }`                      |
fn next_state(
    prev: DeparturesState,
    result: Result<Vec<Departure>, String>,
    now: DateTime<Local>,
) -> DeparturesState {
    todo!("Task 7 step 3")
}
```

Inside `mod tests`, add:

```rust
    fn sample_items() -> Vec<Departure> {
        vec![Departure {
            line: "S9".into(),
            direction: "Spandau".into(),
            planned: future_now() + chrono::Duration::minutes(5),
            actual:  future_now() + chrono::Duration::minutes(5),
            delay_minutes: 0,
            cancelled: false,
            trip_id: "trip-1-ontime".into(),
        }]
    }

    #[test]
    fn next_state_ok_replaces_anything() {
        let now = future_now();
        let next = next_state(DeparturesState::Err { err: "boom".into() },
                              Ok(sample_items()), now);
        match next {
            DeparturesState::Ok { at, items } => {
                assert_eq!(at, now);
                assert_eq!(items.len(), 1);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn next_state_ok_then_err_becomes_stale() {
        let now = future_now();
        let prev = DeparturesState::Ok { at: now, items: sample_items() };
        // Later by 10 minutes — below the stale-drop threshold.
        let later = now + chrono::Duration::minutes(10);
        let next = next_state(prev, Err("net".into()), later);
        match next {
            DeparturesState::Stale { at, items, err } => {
                assert_eq!(at, now);
                assert_eq!(items.len(), 1);
                assert_eq!(err, "net");
            }
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn next_state_stale_beyond_threshold_becomes_err() {
        let now = future_now();
        let prev = DeparturesState::Stale {
            at: now,
            items: sample_items(),
            err: "earlier".into(),
        };
        // 31 minutes later — past STALE_DROP_AFTER (30 min).
        let much_later = now + chrono::Duration::minutes(31);
        let next = next_state(prev, Err("still net".into()), much_later);
        match next {
            DeparturesState::Err { err } => assert_eq!(err, "still net"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn next_state_loading_err_becomes_err() {
        let now = future_now();
        let next = next_state(DeparturesState::Loading, Err("boom".into()), now);
        assert!(matches!(next, DeparturesState::Err { .. }));
    }

    #[test]
    fn next_state_err_err_stays_err_with_new_message() {
        let now = future_now();
        let prev = DeparturesState::Err { err: "old".into() };
        let next = next_state(prev, Err("new".into()), now);
        match next {
            DeparturesState::Err { err } => assert_eq!(err, "new"),
            other => panic!("expected Err, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run tests, expect five failures**

Run: `cargo test -p hytte-services departures::tests::next_state 2>&1 | tail -25`
Expected: five panics with `not yet implemented`.

- [ ] **Step 3: Implement**

Replace the `todo!` body with:

```rust
fn next_state(
    prev: DeparturesState,
    result: Result<Vec<Departure>, String>,
    now: DateTime<Local>,
) -> DeparturesState {
    match result {
        Ok(items) => DeparturesState::Ok { at: now, items },
        Err(err) => match prev {
            DeparturesState::Ok { at, items } => {
                DeparturesState::Stale { at, items, err }
            }
            DeparturesState::Stale { at, items, err: _ } => {
                let age = now.signed_duration_since(at);
                if age >= chrono::Duration::from_std(STALE_DROP_AFTER).unwrap() {
                    DeparturesState::Err { err }
                } else {
                    DeparturesState::Stale { at, items, err }
                }
            }
            DeparturesState::Loading | DeparturesState::Err { .. } => {
                DeparturesState::Err { err }
            }
        },
    }
}
```

- [ ] **Step 4: Re-run tests**

Run: `cargo test -p hytte-services departures::tests::next_state`
Expected: all five pass.

- [ ] **Step 5: Commit**

```bash
git add crates/hytte-services/src/departures.rs
git commit -m "feat(departures): pure next_state transition function"
```

---

## Task 8: `fetch_once` HTTP + parse

**Files:**

- Modify: `crates/hytte-services/src/departures.rs`

No tests — live HTTP is flaky and the parser is already covered. We just wire the pieces together.

- [ ] **Step 1: Add `fetch_once`**

Append after `next_state`:

```rust
/// One blocking HTTP fetch + parse. Runs on a blocking thread via
/// `tokio::task::spawn_blocking`. Failures (any layer) are collapsed to a
/// short error string used in [`DeparturesState::Err`].
fn fetch_once() -> Result<Vec<Departure>, String> {
    let url = format!(
        "https://v6.bvg.transport.rest/stops/{SCHOENEWEIDE_ID}/departures\
         ?results={RESULTS}&suburban=true&subway=false&bus=false&tram=false\
         &regional=false&express=false&ferry=false&tariff=false&language=de"
    );

    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(HTTP_CONNECT_TIMEOUT))
        .timeout_global(Some(HTTP_READ_TIMEOUT))
        .build();
    let agent: ureq::Agent = config.into();

    let mut resp = agent
        .get(&url)
        .call()
        .map_err(|e| format!("http: {e}"))?;
    let body = resp
        .body_mut()
        .with_config()
        .limit(4 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| format!("body: {e}"))?;
    parse_response(&body, Local::now())
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p hytte-services 2>&1 | tail -30`
Expected: builds clean. If ureq 3.x exposes a different builder name in this crate version, swap the two `timeout_*` lines for the equivalents — names current as of `ureq = "3.3"` per the `hytte-services` Cargo.toml. The body-reading idiom matches `mpris.rs:294`.

- [ ] **Step 3: Run existing tests still pass**

Run: `cargo test -p hytte-services departures`
Expected: all previously passing tests still pass.

- [ ] **Step 4: Commit**

```bash
git add crates/hytte-services/src/departures.rs
git commit -m "feat(departures): fetch_once HTTP wrapper"
```

---

## Task 9: Service trait + tokio poll loop + Notify wiring

**Files:**

- Modify: `crates/hytte-services/src/departures.rs`

- [ ] **Step 1: Add `DeparturesService`, `DeparturesHandles`, and the poll loop**

Append after `fetch_once`:

```rust
// ── Service ─────────────────────────────────────────────────────────────────

pub struct DeparturesService;

#[derive(Clone, Default)]
#[doc(hidden)]
pub struct DeparturesHandles {
    pub(crate) state: Mutable<DeparturesState>,
    pub(crate) notify: Arc<Notify>,
}

impl Service for DeparturesService {
    type Handles = DeparturesHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = DeparturesHandles::default();
        let state = handles.state.clone();
        let notify = handles.notify.clone();
        rt.spawn(async move {
            poll_loop(state, notify).await;
        });
        handles
    }
}

#[must_use]
pub fn service() -> DeparturesService {
    DeparturesService
}

async fn poll_loop(state: Mutable<DeparturesState>, notify: Arc<Notify>) {
    // `interval` ticks immediately on first `.tick()` — so the loop body
    // fires once at boot, then every POLL_INTERVAL afterwards.
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Single-flight guard so a refresh() during an in-flight tick is a
    // no-op rather than a stampede on the public API.
    let in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = notify.notified() => {}
        }
        if in_flight.swap(true, std::sync::atomic::Ordering::SeqCst) {
            continue;
        }

        let result = match tokio::task::spawn_blocking(fetch_once).await {
            Ok(r) => r,
            Err(join) => Err(format!("join: {join}")),
        };
        in_flight.store(false, std::sync::atomic::Ordering::SeqCst);

        let now = Local::now();
        let prev = state.get_cloned();
        let next = next_state(prev, result, now);
        if next != state.get_cloned() {
            state.set(next);
        }
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p hytte-services 2>&1 | tail -20`
Expected: clean build.

- [ ] **Step 3: Run the test suite**

Run: `cargo test -p hytte-services departures`
Expected: every previously-passing test still passes.

- [ ] **Step 4: Commit**

```bash
git add crates/hytte-services/src/departures.rs
git commit -m "feat(departures): Service impl with tokio poll loop"
```

---

## Task 10: Public API — `current()` and `refresh()`

**Files:**

- Modify: `crates/hytte-services/src/departures.rs`

- [ ] **Step 1: Add the two public entry points**

Append after `poll_loop`:

```rust
// ── Public API ──────────────────────────────────────────────────────────────

/// Signal of the current departures state. Subscribers receive every
/// transition. The very first emission is [`DeparturesState::Loading`].
pub fn current() -> impl Signal<Item = DeparturesState> {
    registry::with(|r| {
        r.get::<DeparturesHandles>()
            .expect("departures::service() not registered")
            .state
            .signal_cloned()
    })
}

/// Wake the poll task once, triggering a fresh fetch. Idempotent and
/// cheap — coalesced if another wake-up is already pending. No-op if the
/// service hasn't been registered.
pub fn refresh() {
    let notify = registry::with(|r| {
        r.get::<DeparturesHandles>()
            .map(|h| h.notify.clone())
    });
    match notify {
        Some(n) => n.notify_one(),
        None => tracing::warn!("departures::refresh: service not registered"),
    }
}
```

- [ ] **Step 2: Build and run all tests**

Run: `cargo build -p hytte-services && cargo test -p hytte-services`
Expected: clean build; all tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/hytte-services/src/departures.rs
git commit -m "feat(departures): public current() and refresh()"
```

---

## Task 11: Trollshell widget — empty module scaffold

**Files:**

- Create: `trollshell/src/widgets/departures.rs`
- Modify: `trollshell/src/widgets/mod.rs`

- [ ] **Step 1: Create the empty widget file**

Create `trollshell/src/widgets/departures.rs` with this content:

```rust
//! Sidebar departures widget. Subscribes to
//! [`hytte::services::departures::current()`] and renders the current
//! eight S-Bahn departures as a vertical list. Relative time labels
//! re-render on every emission of [`hytte::services::clock::now()`].
```

- [ ] **Step 2: Register the module**

Read `trollshell/src/widgets/mod.rs` and add `pub mod departures;` in alphabetical position.

- [ ] **Step 3: Compile**

Run: `cargo check -p trollshell 2>&1 | tail -10`
Expected: clean (one unused-mod warning is fine; resolved in next task).

- [ ] **Step 4: Commit**

```bash
git add trollshell/src/widgets/departures.rs trollshell/src/widgets/mod.rs
git commit -m "feat(widgets): empty departures module"
```

---

## Task 12: `relative_label` helper (pure)

**Files:**

- Modify: `trollshell/src/widgets/departures.rs`

- [ ] **Step 1: Add stub + tests**

Append to `trollshell/src/widgets/departures.rs`:

```rust
use chrono::{DateTime, Local};

/// Human-readable "minutes from now" label. Negative deltas and anything
/// within the next 60 s render as `"now"`. Above that, we round to the
/// nearest minute so `"7 min"` covers `[6m31s, 7m30s]`.
#[must_use]
pub fn relative_label(now: DateTime<Local>, departure: DateTime<Local>) -> String {
    todo!("Task 12 step 3")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2030, 1, 1, h, m, s).unwrap()
    }

    #[test]
    fn relative_label_within_60s_is_now() {
        let now = at(16, 0, 0);
        assert_eq!(relative_label(now, at(16, 0, 30)), "now");
    }

    #[test]
    fn relative_label_in_the_past_is_now() {
        let now = at(16, 0, 30);
        assert_eq!(relative_label(now, at(16, 0, 0)), "now");
    }

    #[test]
    fn relative_label_rounds_up_at_31_seconds() {
        // 7m31s rounds up to 8.
        let now = at(16, 0, 0);
        assert_eq!(relative_label(now, at(16, 7, 31)), "8 min");
    }

    #[test]
    fn relative_label_rounds_down_at_29_seconds() {
        // 7m29s rounds down to 7.
        let now = at(16, 0, 0);
        assert_eq!(relative_label(now, at(16, 7, 29)), "7 min");
    }

    #[test]
    fn relative_label_one_minute_at_61s() {
        let now = at(16, 0, 0);
        assert_eq!(relative_label(now, at(16, 1, 1)), "1 min");
    }
}
```

- [ ] **Step 2: Run tests, expect five failures**

Run: `cargo test -p trollshell widgets::departures::tests::relative_label 2>&1 | tail -25`
Expected: five panics with `not yet implemented`.

- [ ] **Step 3: Implement**

Replace the `todo!` body with:

```rust
pub fn relative_label(now: DateTime<Local>, departure: DateTime<Local>) -> String {
    let seconds = departure.signed_duration_since(now).num_seconds();
    if seconds <= 60 {
        return "now".to_string();
    }
    let minutes = (seconds + 30) / 60;
    format!("{minutes} min")
}
```

- [ ] **Step 4: Re-run tests**

Run: `cargo test -p trollshell widgets::departures::tests::relative_label`
Expected: all five pass.

- [ ] **Step 5: Commit**

```bash
git add trollshell/src/widgets/departures.rs
git commit -m "feat(widgets/departures): relative_label helper"
```

---

## Task 13: `row()` — one departure row widget

**Files:**

- Modify: `trollshell/src/widgets/departures.rs`

This task wires GTK widgets; the helpers themselves are tested via `relative_label` + `delay_string`. The row builder is integration code, no unit test for it.

- [ ] **Step 1: Add imports and `row()` function**

Append to `trollshell/src/widgets/departures.rs`:

```rust
use hytte::futures_signals::signal::SignalExt;
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::{clock, departures};
use hytte::services::departures::{delay_string, Departure};

/// Build one row widget for `d`. The time cell re-renders on every clock
/// tick by binding to `clock::now()`. The row's CSS classes encode line
/// and cancellation state so styling is purely declarative.
fn row(d: &Departure) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("ts-departure-row");
    if d.cancelled {
        row.add_css_class("ts-cancelled");
    }

    // Line badge.
    let badge = gtk::Label::new(Some(&d.line));
    badge.add_css_class("ts-line-badge");
    badge.add_css_class(&format!("ts-line-{}", d.line));
    badge.set_halign(gtk::Align::Start);
    row.append(&badge);

    // Direction (takes the slack).
    let direction = gtk::Label::new(Some(&d.direction));
    direction.add_css_class("ts-departure-direction");
    direction.set_halign(gtk::Align::Start);
    direction.set_hexpand(true);
    direction.set_xalign(0.0);
    direction.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.append(&direction);

    // Time cell — re-renders each clock tick.
    let time_lbl = gtk::Label::new(None);
    time_lbl.add_css_class("ts-departure-time");
    let actual = d.actual;
    bind(clock::now(), &time_lbl, move |lbl, now| {
        let rel = relative_label(now, actual);
        lbl.set_text(&format!("{rel} · {}", actual.format("%H:%M")));
    });
    row.append(&time_lbl);

    // Delay indicator (hidden when on time).
    if let Some(text) = delay_string(d.delay_minutes) {
        let delay = gtk::Label::new(Some(&text));
        delay.add_css_class("ts-departure-delay");
        row.append(&delay);
    }

    row.upcast()
}
```

- [ ] **Step 2: Compile**

Run: `cargo check -p trollshell 2>&1 | tail -15`
Expected: clean. Watch for missing-import warnings — if `bind` isn't visible via `hytte::prelude::*`, add `use hytte::reactive::bind;` (resolve by grepping `bind\b` usage in `trollshell/src/widgets/clock.rs` or similar).

- [ ] **Step 3: Commit**

```bash
git add trollshell/src/widgets/departures.rs
git commit -m "feat(widgets/departures): row builder"
```

---

## Task 14: Status rows — loading, empty, error, stale footer

**Files:**

- Modify: `trollshell/src/widgets/departures.rs`

- [ ] **Step 1: Add the four small builders**

Append to `trollshell/src/widgets/departures.rs`:

```rust
fn loading_row() -> gtk::Widget {
    let lbl = gtk::Label::new(Some("loading departures…"));
    lbl.add_css_class("ts-departures-loading");
    lbl.set_halign(gtk::Align::Start);
    lbl.upcast()
}

fn empty_row() -> gtk::Widget {
    let lbl = gtk::Label::new(Some("no S-Bahn departures in the next 30 min"));
    lbl.add_css_class("ts-departures-empty");
    lbl.set_halign(gtk::Align::Start);
    lbl.upcast()
}

fn error_row(err: &str) -> gtk::Widget {
    let lbl = gtk::Label::new(Some(&format!("can't reach BVG: {err}")));
    lbl.add_css_class("ts-departures-error");
    lbl.set_halign(gtk::Align::Start);
    lbl.set_wrap(true);
    lbl.upcast()
}

fn stale_footer(err: &str, at: DateTime<Local>) -> gtk::Widget {
    let lbl = gtk::Label::new(Some(&format!(
        "· stale (last good {} — {})",
        at.format("%H:%M"),
        err
    )));
    lbl.add_css_class("ts-departures-stale-footer");
    lbl.set_halign(gtk::Align::Start);
    lbl.set_wrap(true);
    lbl.upcast()
}
```

- [ ] **Step 2: Compile**

Run: `cargo check -p trollshell`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add trollshell/src/widgets/departures.rs
git commit -m "feat(widgets/departures): loading/empty/error/stale rows"
```

---

## Task 15: `rebuild()` and public `widget()`

**Files:**

- Modify: `trollshell/src/widgets/departures.rs`

- [ ] **Step 1: Add `rebuild` and `widget`**

Append to `trollshell/src/widgets/departures.rs`:

```rust
use hytte::services::departures::DeparturesState;

/// Drain `list` and re-populate it from `state`. Eight rows max, so a
/// remove-all + append-fresh cycle per emission is cheap.
fn rebuild(list: &gtk::Box, state: &DeparturesState) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    match state {
        DeparturesState::Loading => list.append(&loading_row()),
        DeparturesState::Err { err } => list.append(&error_row(err)),
        DeparturesState::Ok { items, .. } | DeparturesState::Stale { items, .. } => {
            if items.is_empty() {
                list.append(&empty_row());
            } else {
                for d in items {
                    list.append(&row(d));
                }
            }
            if let DeparturesState::Stale { err, at, .. } = state {
                list.append(&stale_footer(err, *at));
            }
        }
    }
}

/// Build the departures widget. Subscribes to
/// [`departures::current()`] and rebuilds the list on every emission.
#[must_use]
pub fn widget() -> gtk::Widget {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 6);
    list.add_css_class("ts-departures");
    list.set_valign(gtk::Align::Start);

    bind(departures::current(), &list, |list, state| {
        rebuild(list, &state);
    });

    list.upcast()
}
```

- [ ] **Step 2: Build and test**

Run: `cargo build -p trollshell && cargo test -p trollshell widgets::departures`
Expected: builds clean; the existing `relative_label_*` tests still pass.

- [ ] **Step 3: Commit**

```bash
git add trollshell/src/widgets/departures.rs
git commit -m "feat(widgets/departures): rebuild + widget public API"
```

---

## Task 16: Swap sidebar placeholder for the widget

**Files:**

- Modify: `trollshell/src/overlays/sidebar.rs` (around lines 162-169 and 196-211)

- [ ] **Step 1: Replace the placeholder Label**

In `trollshell/src/overlays/sidebar.rs`, locate this block (around lines 164-169):

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
    card.append(&crate::widgets::departures::widget());
```

- [ ] **Step 2: Nudge a refresh on open**

In the same file, locate the open/close subscription (around lines 196-211):

```rust
    let subscription =
        glib::MainContext::default().spawn_local(open_state.signal().for_each(move |open| {
            if open {
                window_for_open.set_visible(true);
                window_for_open.present();
                window_for_open.set_exclusive_zone(SIDEBAR_WIDTH - FRAME_THICKNESS_I32);
                revealer_for_open.set_reveal_child(true);
            } else {
                /* … */
                revealer_for_open.set_reveal_child(false);
            }
            async {}
        }));
```

Inside the `if open { … }` branch, after `revealer_for_open.set_reveal_child(true);`, add:

```rust
                hytte::services::departures::refresh();
```

- [ ] **Step 3: Build**

Run: `cargo build -p trollshell 2>&1 | tail -15`
Expected: clean build.

- [ ] **Step 4: Run sidebar's own tests**

Run: `cargo test -p trollshell overlays::sidebar`
Expected: existing sidebar tests still pass (this edit doesn't change their assumptions).

- [ ] **Step 5: Commit**

```bash
git add trollshell/src/overlays/sidebar.rs
git commit -m "feat(sidebar): swap placeholder for departures widget"
```

---

## Task 17: Register the service in `main.rs`

**Files:**

- Modify: `trollshell/src/main.rs:13-48`

- [ ] **Step 1: Add `departures` to the import group**

Read `trollshell/src/main.rs:13-17`. The current import is:

```rust
use hytte::services::{
    bluetooth, bluetooth_audio, brightness, calendar, clipboard, clock, displays, dnd, mpris,
    netconn, networkd, niri, notifications, notifications_mute, pipewire, polkit, power_profiles,
    resolved, screensaver, sensors, systemd, tray, upower, vpn, wallpaper, wifi,
};
```

Insert `departures` between `clock` and `displays` (alphabetical: `clipboard`, `clock`, `departures`, `displays`):

```rust
use hytte::services::{
    bluetooth, bluetooth_audio, brightness, calendar, clipboard, clock, departures, displays,
    dnd, mpris, netconn, networkd, niri, notifications, notifications_mute, pipewire, polkit,
    power_profiles, resolved, screensaver, sensors, systemd, tray, upower, vpn, wallpaper, wifi,
};
```

- [ ] **Step 2: Register the service**

Add `.with(departures::service())` to the chain in `App::new(…)`. Place it right after `.with(clock::service())` (line 23):

```rust
    App::new("mov.vibec0re.trollshell")
        .with(clock::service())
        .with(departures::service())   // NEW
        .with(niri::service())
        // …
```

- [ ] **Step 3: Build**

Run: `cargo build -p trollshell 2>&1 | tail -10`
Expected: clean build.

- [ ] **Step 4: Commit**

```bash
git add trollshell/src/main.rs
git commit -m "feat(main): register departures service"
```

---

## Task 18: CSS — departures rules + per-line colors + light-mode mirror

**Files:**

- Modify: `trollshell/style.css`

- [ ] **Step 1: Find the right insertion point**

Open `trollshell/style.css`. Find the existing `.ts-sidebar { … }` block (added in the prior sidebar spec). Also find `.ts-sidebar-placeholder { … }` — that rule becomes dead and we'll delete it.

- [ ] **Step 2: Delete the dead placeholder rule**

Delete the entire `.ts-sidebar-placeholder { … }` rule (a few lines).

- [ ] **Step 3: Add the departures rules**

Insert this block immediately after `.ts-sidebar { … }` (or in the same dark-mode section adjacent to it):

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
} /* violet     */
.ts-line-S41 {
  background: #aa5d3d;
}
.ts-line-S42 {
  background: #c36f33;
}
.ts-line-S46 {
  background: #c4923d;
} /* chestnut   */
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

- [ ] **Step 4: Add the light-mode mirror**

Find the existing light-mode section (the one with the `.ts-drawer` and `.ts-sidebar` light overrides; look for `@media (prefers-color-scheme: light)` or whatever the project uses — grep the file for the existing `.ts-sidebar` light-mode rule). Inside that block, append:

```css
.ts-departure-direction {
  color: alpha(black, 0.95);
}
.ts-departure-time {
  color: alpha(black, 0.85);
}
.ts-departures-loading,
.ts-departures-empty,
.ts-departures-stale-footer {
  color: alpha(black, 0.45);
}
/* Badge colours are theme-independent — no overrides. */
```

- [ ] **Step 5: Run trollshell briefly to confirm CSS parses**

Run: `cargo build -p trollshell && cargo run -p trollshell &`
Wait 3 seconds, then `pkill -f 'target.*trollshell$'`. Watch the console for CSS-parse warnings.

(If you're working in a sandboxed agent that can't run interactive GUIs, skip this step and rely on Task 19's manual verification.)

- [ ] **Step 6: Commit**

```bash
git add trollshell/style.css
git commit -m "style(departures): row layout, line-color badges, light-mode mirror"
```

---

## Task 19: End-to-end verification

**Files:** none modified.

These steps mirror the spec's verification list. Run them on a real Niri session.

- [ ] **Step 1: Full workspace build + test**

Run: `cargo build && cargo test 2>&1 | tail -40`
Expected: clean build; all tests pass — at least these new ones:

- `departures::tests::delay_string_*` (3)
- `departures::tests::into_departure_*` (5)
- `departures::tests::parse_response_*` (3)
- `departures::tests::next_state_*` (5)
- `widgets::departures::tests::relative_label_*` (5)

Total new: 21 tests passing.

- [ ] **Step 2: Launch trollshell and open the sidebar**

Run: `cargo run --release -p trollshell`
Then in the running Niri session, click the leftmost bar chip (sidebar toggle). Expected: within ~1 s a list of 8 S-Bahn rows appears, each formatted `LINE   Direction   X min · HH:MM` with line badges in the correct colors.

- [ ] **Step 3: Watch a minute pass**

Leave the sidebar open. Expected: the top row's `X min` cell ticks down by one each wall-clock minute, with no flicker. The list does not visibly rebuild.

- [ ] **Step 4: Offline test**

Disable Wi-Fi. Close and reopen the sidebar to force a refresh. Expected: a footer reads `· stale (last good HH:MM — …)` and the previous rows remain. Re-enable Wi-Fi, close+reopen again — the footer disappears.

- [ ] **Step 5: Cold-offline test**

Quit trollshell. Disable Wi-Fi. Launch trollshell, open the sidebar. Expected: the widget shows the error row in red.

- [ ] **Step 6: Multi-monitor**

Open the sidebar on monitor A and monitor B. Expected: identical lists on both. `RUST_LOG=hytte_services::departures=debug cargo run --release -p trollshell` shows a single fetch even with both opens happening close together (the `in_flight` guard collapses the second wake-up).

- [ ] **Step 7: Cancellation (if observable in the wild)**

If a real S Schöneweide departure shows `cancelled: true` in the current `/departures` response, confirm the row renders with strike-through and red tint. (Hard to test on demand; mark complete if you can't reproduce — the parser tests cover the data path.)

- [ ] **Step 8: Final commit (if anything was left uncommitted)**

```bash
git status
```

Expected: clean working tree.

---

## Done criteria

- All 21 new tests pass.
- `cargo build --release -p trollshell` is clean.
- Live sidebar shows 8 colored S-Bahn rows from S Schöneweide with correctly ticking relative times.
- Background refresh runs every 15 min; opening the sidebar nudges an immediate fetch.
- Offline failure modes (stale / err) render correctly.
- No regressions to existing sidebar mechanics (push, frame integration, ESC, multi-monitor).
