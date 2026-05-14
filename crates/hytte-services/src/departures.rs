//! Polled S-Bahn departures from S Schöneweide, sourced from
//! v6.bvg.transport.rest.
//!
//! A 15-minute tokio loop fetches the next 8 suburban-rail departures and
//! exposes them through a [`Mutable<DeparturesState>`]. Consumers subscribe
//! via [`current()`]. The sidebar's open-edge handler nudges [`refresh()`]
//! to keep the freshly-opened list current without waiting for the next
//! poll tick.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};
use futures_signals::signal::{Mutable, Signal};
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

/// Same threshold as [`STALE_DROP_AFTER`], typed as `chrono::Duration`
/// so it can be compared against age deltas without a runtime conversion.
const STALE_DROP_AFTER_CHRONO: chrono::Duration = chrono::Duration::seconds(30 * 60);

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
    })
}

/// Parse a raw response body into a `Vec<Departure>`, filtering as
/// described on [`into_departure`].
fn parse_response(body: &str, now: DateTime<Local>) -> Result<Vec<Departure>, String> {
    let api: ApiResponse =
        serde_json::from_str(body).map_err(|e| format!("decode: {e}"))?;
    Ok(api
        .departures
        .into_iter()
        .filter_map(|r| into_departure(r, now))
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
            DeparturesState::Ok { at, items } => {
                DeparturesState::Stale { at, items, err }
            }
            DeparturesState::Stale { at, items, err: _ } => {
                let age = now.signed_duration_since(at);
                if age >= STALE_DROP_AFTER_CHRONO {
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

#[cfg(test)]
mod tests {
    use super::*;

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
            actual:  later + chrono::Duration::minutes(7),
            delay_minutes: 0,
            cancelled: false,
            trip_id: "trip-fresh".into(),
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
        assert!(matches!(next, DeparturesState::Err { .. }),
                "exact-threshold age must drop to Err, got {next:?}");
    }
}
