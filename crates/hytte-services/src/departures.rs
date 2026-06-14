//! Polled S-Bahn / transit departures for the current place (resolved by
//! [`crate::places`]), sourced from v6.bvg.transport.rest.
//!
//! `places` decides which station + line/direction filter applies (Wi-Fi
//! fingerprint → `GeoClue` radius → nearest-station when away). This module just
//! fetches that station's departures and applies the filter.
//!
//! A 15-minute tokio loop refetches, and also whenever the resolved place
//! changes. Consumers subscribe via [`current()`]. The sidebar's open-edge
//! handler nudges [`refresh()`] to keep the freshly-opened list current
//! without waiting for the next poll tick.
//!
//! `places::service()` MUST be registered before `departures::service()` —
//! `start` reads places' shared current-place handle.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};
use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_reactive::{Service, registry};
use tokio::sync::Notify;

use crate::places::{self, ResolvedPlace};

// ── Tunables ─────────────────────────────────────────────────────────────────

/// Background poll cadence. The sidebar's open-edge handler additionally
/// kicks [`refresh()`] for an immediate fetch.
pub const POLL_INTERVAL: Duration = Duration::from_mins(15);

/// After this much time elapses since the last successful fetch, a
/// continuing error transitions `Stale` → `Err` so the user sees the
/// list has gone cold.
pub const STALE_DROP_AFTER: Duration = Duration::from_mins(30);

/// Same threshold as [`STALE_DROP_AFTER`], typed as `chrono::Duration`
/// so it can be compared against age deltas without a runtime conversion.
const STALE_DROP_AFTER_CHRONO: chrono::Duration = chrono::Duration::seconds(30 * 60);

/// How many departures to request from the API. Larger than the display
/// count so a direction/line filter still has enough rows left to fill the
/// list after dropping the outbound ones.
const FETCH_COUNT: usize = 30;

/// How many departures to display after filtering.
pub const DISPLAY_COUNT: usize = 8;

pub const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(10);

// ── Filter ────────────────────────────────────────────────────────────────--

/// Which departures to keep at a place. An empty axis means "allow all on
/// that axis"; a departure must pass both axes. Built from the resolved
/// place's `lines` / `directions` (already de-blanked by [`crate::places`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Filter {
    /// Allowed line names, matched case-insensitively (e.g. `"S8"`). Empty =
    /// allow every line.
    lines: Vec<String>,
    /// Allowed destination substrings, matched case-insensitively (e.g.
    /// `"Spandau"`). Empty = allow every direction.
    directions: Vec<String>,
}

impl Filter {
    /// Whether `d` passes this filter. Line match is exact (case-insensitive);
    /// direction match is a case-insensitive substring so `"Spandau"` matches
    /// API strings like `"S+U Spandau Bhf"`.
    fn matches(&self, d: &Departure) -> bool {
        let line_ok =
            self.lines.is_empty() || self.lines.iter().any(|l| l.eq_ignore_ascii_case(&d.line));
        let dir_ok = self.directions.is_empty() || {
            let dir = d.direction.to_lowercase();
            self.directions
                .iter()
                .any(|want| dir.contains(&want.to_lowercase()))
        };
        line_ok && dir_ok
    }
}

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
    /// Walk budget (minutes) to the platform, copied from the resolved place.
    /// `0` means no budget — the widget shows the plain departs-in countdown;
    /// positive turns the row into a leave-by countdown. Carried per-row so the
    /// widget needs no second subscription and `Stale` keeps the right budget.
    pub walk_minutes: u32,
}

/// The whole service surface, observed by the widget.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum DeparturesState {
    /// Initial value before the first fetch returns.
    #[default]
    Loading,
    /// Most recent fetch succeeded; `at` is when it landed.
    Ok {
        at: DateTime<Local>,
        items: Vec<Departure>,
    },
    /// A previous fetch succeeded and a later one failed; keep showing
    /// the prior list with a "stale" hint, up to `STALE_DROP_AFTER`.
    Stale {
        at: DateTime<Local>,
        items: Vec<Departure>,
        err: String,
    },
    /// No usable data on hand and the latest fetch failed.
    Err { err: String },
}

/// Formats the delay indicator shown after the time cell. `None` means
/// "render no badge"; `Some("+5")` means render `+5` in the delay style.
/// We only surface lateness — negative deltas (early trains) are silent
/// since they're not actionable to the passenger.
#[must_use]
pub fn delay_string(delay_minutes: i64) -> Option<String> {
    if delay_minutes > 0 {
        Some(format!("+{delay_minutes}"))
    } else {
        None
    }
}

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

    // Integer division intentionally truncates toward zero; sub-minute precision
    // isn't displayed and trains rarely report non-round delays.
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
        // Stamped from the resolved place in `fetch_for_place`; the wire format
        // has no notion of a walk budget.
        walk_minutes: 0,
    })
}

/// Parse a raw response body into a `Vec<Departure>`, filtering as
/// described on [`into_departure`].
fn parse_response(body: &str, now: DateTime<Local>) -> Result<Vec<Departure>, String> {
    let api: ApiResponse = serde_json::from_str(body).map_err(|e| format!("decode: {e}"))?;
    Ok(api
        .departures
        .into_iter()
        .filter_map(|r| into_departure(r, now))
        .collect())
}

// ── Nearby-station lookup (away-from-home fallback) ──────────────────────────

#[derive(serde::Deserialize)]
struct NearbyStop {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    products: Option<NearbyProducts>,
}

#[derive(serde::Deserialize)]
struct NearbyProducts {
    #[serde(default)]
    suburban: bool,
}

/// Pick a station id + name from a `/locations/nearby` response: prefer the
/// nearest stop that has S-Bahn service, else the nearest stop with an id.
/// The endpoint returns stops sorted by distance, so first-match is nearest.
fn parse_nearby(body: &str) -> Option<(String, String)> {
    let stops: Vec<NearbyStop> = serde_json::from_str(body).ok()?;
    let pick = stops
        .iter()
        .find(|s| {
            s.kind.as_deref() == Some("stop")
                && s.id.is_some()
                && s.products.as_ref().is_some_and(|p| p.suburban)
        })
        .or_else(|| stops.iter().find(|s| s.id.is_some()))?;
    Some((
        pick.id.clone()?,
        pick.name.clone().unwrap_or_else(|| "Nearby".to_string()),
    ))
}

/// Blocking lookup of the nearest station to `(lat, lon)`. `None` on any
/// failure — the caller surfaces an error for that fetch.
fn fetch_nearby_station(agent: &ureq::Agent, lat: f64, lon: f64) -> Option<(String, String)> {
    let url = format!(
        "https://v6.bvg.transport.rest/locations/nearby\
         ?latitude={lat}&longitude={lon}&results=8&stops=true&poi=false\
         &linesOfStops=false&language=en"
    );
    let mut resp = agent.get(&url).call().ok()?;
    let body = resp
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .ok()?;
    parse_nearby(&body)
}

// ── Fetch ─────────────────────────────────────────────────────────────────--

fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(HTTP_CONNECT_TIMEOUT))
        .timeout_global(Some(HTTP_READ_TIMEOUT))
        .build();
    config.into()
}

/// One blocking HTTP fetch + parse of the suburban departures at `station`.
fn fetch_departures(agent: &ureq::Agent, station: &str) -> Result<Vec<Departure>, String> {
    let url = format!(
        "https://v6.bvg.transport.rest/stops/{station}/departures\
         ?results={FETCH_COUNT}&suburban=true&subway=false&bus=false&tram=false\
         &regional=false&express=false&ferry=false&tariff=false&language=de"
    );

    let mut resp = agent.get(&url).call().map_err(|e| format!("http: {e}"))?;
    let body = resp
        .body_mut()
        .with_config()
        .limit(4 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| format!("body: {e}"))?;
    parse_response(&body, Local::now())
}

/// Fetch + filter departures for the resolved `place`. When the place has a
/// `station`, fetch it and apply the line/direction filter; otherwise (away
/// from every defined place) look up the nearest station and show it
/// unfiltered. Runs on a blocking thread via `spawn_blocking`.
fn fetch_for_place(place: &ResolvedPlace) -> Result<Vec<Departure>, String> {
    let agent = http_agent();
    let (station, filter) = match &place.station {
        Some(station) => (
            station.clone(),
            Filter {
                lines: place.lines.clone(),
                directions: place.directions.clone(),
            },
        ),
        None => match fetch_nearby_station(&agent, place.lat, place.lon) {
            Some((id, _name)) => (id, Filter::default()),
            None => return Err("no station for current location".to_string()),
        },
    };
    tracing::debug!(station = %station, place = %place.name, "departures: fetching");

    let all = fetch_departures(&agent, &station)?;
    let walk_minutes = place.walk_minutes;
    Ok(all
        .into_iter()
        .filter(|d| filter.matches(d))
        .take(DISPLAY_COUNT)
        .map(|d| Departure { walk_minutes, ..d })
        .collect())
}

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
    match result {
        Ok(items) => DeparturesState::Ok { at: now, items },
        Err(err) => match prev {
            DeparturesState::Ok { at, items } => DeparturesState::Stale { at, items, err },
            DeparturesState::Stale { at, items, err: _ } => {
                let age = now.signed_duration_since(at);
                if age >= STALE_DROP_AFTER_CHRONO {
                    DeparturesState::Err { err }
                } else {
                    DeparturesState::Stale { at, items, err }
                }
            }
            DeparturesState::Loading | DeparturesState::Err { .. } => DeparturesState::Err { err },
        },
    }
}

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
        rt.spawn(poll_loop(state, notify.clone()));

        // Bridge: re-fetch whenever the resolved place changes (including its
        // first resolution). Reads places' shared handle, which exists because
        // places::service() is registered first.
        if let Some(place) = places::shared_place() {
            rt.spawn(async move {
                place
                    .signal_ref(|_| ())
                    .for_each(move |()| {
                        notify.notify_one();
                        std::future::ready(())
                    })
                    .await;
            });
        } else {
            tracing::warn!(
                "departures: places not registered before departures; auto-refresh-on-place disabled"
            );
        }

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
            () = notify.notified() => {}
        }
        if in_flight.swap(true, std::sync::atomic::Ordering::SeqCst) {
            continue;
        }

        // The resolved place; `None` only before the first resolution — stay on
        // the current state (Loading) until places resolves.
        let Some(place) = places::shared_place().and_then(|m| m.get_cloned()) else {
            in_flight.store(false, std::sync::atomic::Ordering::SeqCst);
            continue;
        };

        let result = match tokio::task::spawn_blocking(move || fetch_for_place(&place)).await {
            Ok(r) => r,
            Err(join) => Err(format!("join: {join}")),
        };
        in_flight.store(false, std::sync::atomic::Ordering::SeqCst);

        if let Err(ref e) = result {
            tracing::warn!("departures: fetch failed: {e}");
        }

        let now = Local::now();
        let prev = state.get_cloned();
        let prev_for_cmp = prev.clone();
        let next = next_state(prev, result, now);
        if next != prev_for_cmp {
            state.set(next);
        }
    }
}

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
    let notify = registry::with(|r| r.get::<DeparturesHandles>().map(|h| h.notify.clone()));
    if let Some(n) = notify {
        n.notify_one();
    } else {
        tracing::warn!("departures::refresh: service not registered");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::TimeZone;

    fn future_now() -> DateTime<Local> {
        // The same absolute instant as 2030-01-01T16:00:00+01:00 (before every
        // fixture row), pinned via Utc so it doesn't shift with the machine
        // timezone — under `nix build`'s sandbox there's no tzdata, so Local
        // there resolves to UTC and a Local-built literal would land elsewhere.
        chrono::Utc
            .with_ymd_and_hms(2030, 1, 1, 15, 0, 0)
            .unwrap()
            .with_timezone(&Local)
    }

    fn load_fixture() -> ApiResponse {
        let raw = include_str!("../tests/fixtures/departures-schoeneweide.json");
        serde_json::from_str(raw).expect("fixture parses")
    }

    fn sample_dep(line: &str, direction: &str) -> Departure {
        Departure {
            line: line.into(),
            direction: direction.into(),
            planned: future_now(),
            actual: future_now(),
            delay_minutes: 0,
            cancelled: false,
            trip_id: "t".into(),
            walk_minutes: 0,
        }
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
        // now > every fixture timestamp (17:00+01:00), pinned via Utc so it's
        // timezone-independent (see future_now).
        let now = chrono::Utc
            .with_ymd_and_hms(2030, 1, 1, 16, 0, 0)
            .unwrap()
            .with_timezone(&Local);
        let row = api.departures.into_iter().next().unwrap();
        assert!(into_departure(row, now).is_none());
    }

    fn sample_items() -> Vec<Departure> {
        vec![Departure {
            line: "S9".into(),
            direction: "Spandau".into(),
            planned: future_now() + chrono::Duration::minutes(5),
            actual: future_now() + chrono::Duration::minutes(5),
            delay_minutes: 0,
            cancelled: false,
            trip_id: "trip-1-ontime".into(),
            walk_minutes: 0,
        }]
    }

    #[test]
    fn next_state_ok_replaces_anything() {
        let now = future_now();
        let next = next_state(
            DeparturesState::Err { err: "boom".into() },
            Ok(sample_items()),
            now,
        );
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
        let prev = DeparturesState::Ok {
            at: now,
            items: sample_items(),
        };
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

    #[test]
    fn next_state_stale_to_ok_recovers() {
        let now = future_now();
        let prev = DeparturesState::Stale {
            at: now,
            items: sample_items(),
            err: "old".into(),
        };
        // 5 minutes later — well within STALE_DROP_AFTER.
        let later = now + chrono::Duration::minutes(5);
        let fresh = vec![Departure {
            line: "S46".into(),
            direction: "Königs Wusterhausen".into(),
            planned: later + chrono::Duration::minutes(7),
            actual: later + chrono::Duration::minutes(7),
            delay_minutes: 0,
            cancelled: false,
            trip_id: "trip-fresh".into(),
            walk_minutes: 0,
        }];
        let next = next_state(prev, Ok(fresh), later);
        match next {
            DeparturesState::Ok { at, items } => {
                assert_eq!(at, later);
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].line, "S46");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn next_state_stale_at_exact_threshold_drops_to_err() {
        // At STALE_DROP_AFTER exactly (30 min), the >= comparison must drop.
        let now = future_now();
        let prev = DeparturesState::Stale {
            at: now,
            items: sample_items(),
            err: "old".into(),
        };
        let exactly_threshold = now + chrono::Duration::minutes(30);
        let next = next_state(prev, Err("still net".into()), exactly_threshold);
        assert!(
            matches!(next, DeparturesState::Err { .. }),
            "exact-threshold age must drop to Err, got {next:?}"
        );
    }

    // ── Filter ───────────────────────────────────────────────────────────────

    #[test]
    fn filter_empty_allows_everything() {
        let f = Filter::default();
        assert!(f.matches(&sample_dep("S9", "Flughafen BER")));
        assert!(f.matches(&sample_dep("Bus 164", "Anywhere")));
    }

    #[test]
    fn filter_line_and_direction_are_anded() {
        let f = Filter {
            lines: vec!["S8".into(), "S85".into(), "S9".into()],
            directions: vec!["Spandau".into(), "Birkenwerder".into()],
        };
        assert!(f.matches(&sample_dep("S9", "Spandau")));
        assert!(!f.matches(&sample_dep("S9", "Flughafen BER"))); // wrong direction
        assert!(!f.matches(&sample_dep("S46", "Spandau"))); // wrong line
    }

    #[test]
    fn filter_matches_case_insensitively_and_by_substring() {
        let f = Filter {
            lines: vec!["s8".into()],
            directions: vec!["birkenwerder".into()],
        };
        // Line case-insensitive; direction is a substring of the API string.
        assert!(f.matches(&sample_dep("S8", "S+U Birkenwerder Bhf")));
    }

    #[test]
    fn filter_directions_only_ignores_line() {
        let f = Filter {
            lines: vec![],
            directions: vec!["Spandau".into()],
        };
        assert!(f.matches(&sample_dep("S9", "Spandau")));
        assert!(!f.matches(&sample_dep("S9", "Wildau")));
    }

    // ── Nearby parsing ─────────────────────────────────────────────────────--

    #[test]
    fn parse_nearby_prefers_suburban_stop() {
        let body = r#"[
            {"type":"stop","id":"111","name":"Bus Stop","products":{"suburban":false}},
            {"type":"stop","id":"222","name":"S Bahnhof","products":{"suburban":true}}
        ]"#;
        assert_eq!(
            parse_nearby(body),
            Some(("222".to_string(), "S Bahnhof".to_string()))
        );
    }

    #[test]
    fn parse_nearby_falls_back_to_first_with_id() {
        let body =
            r#"[{"type":"stop","id":"111","name":"Only Bus","products":{"suburban":false}}]"#;
        assert_eq!(
            parse_nearby(body),
            Some(("111".to_string(), "Only Bus".to_string()))
        );
    }

    #[test]
    fn parse_nearby_empty_or_garbage_is_none() {
        assert_eq!(parse_nearby("[]"), None);
        assert_eq!(parse_nearby("not json"), None);
    }
}
