//! Polled S-Bahn / transit departures for the place nearest your current
//! location, sourced from v6.bvg.transport.rest.
//!
//! Places are defined in `~/.config/trollshell/departures.toml` (a documented
//! default is written on first run). Each place pins a station and an optional
//! line/direction allowlist (the "toward the city centre" filter); the shell
//! picks the place nearest the GeoClue-resolved location, and falls back to
//! the nearest station — unfiltered — when you're away from every defined
//! place.
//!
//! A 15-minute tokio loop fetches departures and exposes them through a
//! [`Mutable<DeparturesState>`]; it also re-fetches whenever the location
//! changes. Consumers subscribe via [`current()`]. The sidebar's open-edge
//! handler nudges [`refresh()`] to keep the freshly-opened list current
//! without waiting for the next poll tick.
//!
//! `geoclue::service()` MUST be registered before `departures::service()` in
//! the `App` builder — `start` reads geoclue's shared location handle to wire
//! the re-fetch-on-location-change bridge.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};
use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_reactive::{registry, Service};
use tokio::sync::Notify;

use crate::geoclue::{self, LocationState};

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

// ── Places & config ──────────────────────────────────────────────────────────

const CONFIG_REL_PATH: &str = ".config/trollshell/departures.toml";

/// Documented default config, written to disk on first run and used as the
/// fallback when the user's file is missing, empty, or malformed. Keeping the
/// default *as TOML* means [`load_places`] has one parse path and the written
/// file always matches the running behaviour.
const DEFAULT_CONFIG: &str = r#"# trollshell departures — which station and which directions to show.
#
# The shell shows departures for the place nearest your current location
# (resolved via GeoClue). Define one [[place]] per location you care about.
# When no place is near — and before GeoClue's first fix at startup — the
# FIRST [[place]] below is used as home; when you're genuinely away from all
# of them, it shows the nearest station, unfiltered.
#
# Find a station id by name, e.g.:
#   https://v6.bvg.transport.rest/locations?query=Schöneweide
# (BVG endpoint covers Berlin/Brandenburg; the field you want is "id".)

[[place]]
name = "Schöneweide"
lat = 52.4556
lon = 13.5085
# Match radius in km. Generous by default because GeoClue is configured for
# city-level accuracy — at 12 km this covers most of inner Berlin, so station
# switching only kicks in once you leave the city. For per-neighbourhood
# switching, shrink this and add a [[place]] for each spot.
radius_km = 12.0
station = "900180001"

# Optional filters (omit either to allow everything on that axis):
#   lines      — only these line names (exact, case-insensitive)
#   directions — keep departures whose destination CONTAINS one of these
#                (case-insensitive substring) — i.e. "toward the city centre".
#                Prefer full terminus names; short fragments over-match. Blank
#                entries are ignored.
lines = ["S8", "S85", "S9"]
directions = ["Spandau", "Birkenwerder", "Hohen Neuendorf", "Waidmannslust"]
"#;

/// A place the user cares about: a station plus an optional departure filter,
/// matched against the current location by `(lat, lon)` within `radius_km`.
#[derive(Clone, Debug, PartialEq)]
struct Place {
    name: String,
    lat: f64,
    lon: f64,
    radius_km: f64,
    station: String,
    filter: Filter,
}

/// Which departures to keep at a place. An empty axis means "allow all on
/// that axis"; a departure must pass both axes.
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
            self.directions.iter().any(|want| dir.contains(&want.to_lowercase()))
        };
        line_ok && dir_ok
    }
}

#[derive(serde::Deserialize)]
struct ConfigFile {
    #[serde(default)]
    place: Vec<PlaceCfg>,
}

#[derive(serde::Deserialize)]
struct PlaceCfg {
    name: String,
    lat: f64,
    lon: f64,
    #[serde(default = "default_radius_km")]
    radius_km: f64,
    station: String,
    #[serde(default)]
    lines: Vec<String>,
    #[serde(default)]
    directions: Vec<String>,
}

fn default_radius_km() -> f64 {
    12.0
}

fn config_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(CONFIG_REL_PATH))
}

/// Parse a config body into places. Pure, so the schema is unit-testable.
fn parse_places(toml_text: &str) -> Result<Vec<Place>, String> {
    let cfg: ConfigFile = toml::from_str(toml_text).map_err(|e| format!("config: {e}"))?;
    Ok(cfg
        .place
        .into_iter()
        .map(|p| Place {
            name: p.name,
            lat: p.lat,
            lon: p.lon,
            radius_km: p.radius_km,
            station: p.station,
            filter: Filter { lines: nonblank(p.lines), directions: nonblank(p.directions) },
        })
        .collect())
}

/// Drop empty/whitespace-only entries. A stray `""` would otherwise turn a
/// filter into an accidental allow-all, since an empty needle is a substring
/// of every string (and `Filter`'s allow-all sentinel is an *empty* list).
fn nonblank(items: Vec<String>) -> Vec<String> {
    items.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

/// Load places from the user's config, writing a documented default on first
/// run. Always returns a non-empty list: a missing, empty, or malformed config
/// falls back to the built-in default.
fn load_places() -> Vec<Place> {
    let default = || parse_places(DEFAULT_CONFIG).expect("built-in default config parses");
    let Some(path) = config_path() else {
        return default();
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No config yet — write the documented default so it's discoverable.
            write_default_config(&path);
            return default();
        }
        Err(e) => {
            // Exists but unreadable (permissions, non-UTF-8, …). Use the
            // default but DON'T overwrite: the bytes may be a config we just
            // can't decode (e.g. umlauts saved as Latin-1), and clobbering it
            // would silently destroy the user's edits.
            tracing::warn!(error = %e, path = %path.display(), "departures: config unreadable; using built-in default (not overwriting)");
            return default();
        }
    };
    match parse_places(&text) {
        Ok(places) if !places.is_empty() => places,
        Ok(_) => {
            tracing::warn!(path = %path.display(), "departures: config has no [[place]]; using default");
            default()
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "departures: config parse failed; using default");
            default()
        }
    }
}

/// Best-effort write of [`DEFAULT_CONFIG`] to `path`. Failure is logged; the
/// in-memory default is used regardless.
fn write_default_config(path: &Path) {
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(error = %e, path = %parent.display(), "departures: mkdir for default config failed");
        return;
    }
    match std::fs::write(path, DEFAULT_CONFIG) {
        Ok(()) => tracing::info!(path = %path.display(), "departures: wrote default config"),
        Err(e) => tracing::warn!(error = %e, path = %path.display(), "departures: writing default config failed"),
    }
}

// ── Location → station resolution ────────────────────────────────────────────

/// Great-circle distance between two `(lat, lon)` points in kilometres.
fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_RADIUS_KM: f64 = 6371.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * a.sqrt().clamp(0.0, 1.0).asin()
}

/// The nearest place whose radius covers `(lat, lon)`, if any.
fn nearest_place(places: &[Place], lat: f64, lon: f64) -> Option<&Place> {
    places
        .iter()
        .map(|p| (p, haversine_km(lat, lon, p.lat, p.lon)))
        .filter(|(p, d)| *d <= p.radius_km)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(p, _)| p)
}

/// Outcome of matching a location against the configured places.
#[derive(Clone, Debug, PartialEq)]
enum Resolution {
    /// Use a known station + its filter (a matched place, or — when the
    /// location is still resolving — the home default).
    Known { station: String, filter: Filter, label: String },
    /// No configured place matched; look up the nearest station for these
    /// coordinates and show it unfiltered.
    Nearby { lat: f64, lon: f64 },
}

/// Map a location state to a [`Resolution`]. Pure (the `Nearby` arm's HTTP
/// lookup happens in [`fetch_for_location`]), so it's unit-testable.
///
/// While the location is still resolving — or unavailable — we fall back to
/// the first configured place (home) so departures show immediately at boot
/// instead of waiting on `GeoClue`.
fn resolve_location(loc: &LocationState, places: &[Place]) -> Resolution {
    let home = |p: &Place| Resolution::Known {
        station: p.station.clone(),
        filter: p.filter.clone(),
        label: p.name.clone(),
    };
    match loc {
        LocationState::Resolved(s) => match nearest_place(places, s.lat, s.lon) {
            Some(p) => home(p),
            None => Resolution::Nearby { lat: s.lat, lon: s.lon },
        },
        // Unreachable degenerate (`load_places` guarantees ≥1 place): an empty
        // list with no fix yields a harmless Nearby(0,0).
        LocationState::Resolving | LocationState::Unavailable => places
            .first()
            .map_or(Resolution::Nearby { lat: 0.0, lon: 0.0 }, home),
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
}

/// The whole service surface, observed by the widget.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum DeparturesState {
    /// Initial value before the first fetch returns.
    #[default]
    Loading,
    /// Most recent fetch succeeded; `at` is when it landed.
    Ok { at: DateTime<Local>, items: Vec<Departure> },
    /// A previous fetch succeeded and a later one failed; keep showing
    /// the prior list with a "stale" hint, up to `STALE_DROP_AFTER`.
    Stale { at: DateTime<Local>, items: Vec<Departure>, err: String },
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
/// failure — the caller falls back to the home place.
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

/// Resolve the target station for `loc` (matched place, or a nearby-station
/// lookup), fetch its departures, apply the place's filter, and cap to
/// [`DISPLAY_COUNT`]. Runs on a blocking thread via `spawn_blocking`.
fn fetch_for_location(loc: &LocationState, places: &[Place]) -> Result<Vec<Departure>, String> {
    let agent = http_agent();
    let (station, filter, label) = match resolve_location(loc, places) {
        Resolution::Known { station, filter, label } => (station, filter, label),
        Resolution::Nearby { lat, lon } => match fetch_nearby_station(&agent, lat, lon) {
            Some((id, name)) => (id, Filter::default(), name),
            None => match places.first() {
                Some(p) => (p.station.clone(), p.filter.clone(), p.name.clone()),
                None => return Err("no places configured".to_string()),
            },
        },
    };
    tracing::debug!(station = %station, place = %label, "departures: resolved target");

    let all = fetch_departures(&agent, &station)?;
    Ok(all
        .into_iter()
        .filter(|d| filter.matches(d))
        .take(DISPLAY_COUNT)
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
        let places = Arc::new(load_places());
        rt.spawn(poll_loop(state, notify.clone(), places));

        // Bridge: re-fetch whenever geoclue's location changes (including its
        // first resolution). Mirrors `weather`; requires geoclue::service() to
        // be registered first so the shared handle exists.
        if let Some(loc) = geoclue::shared_location() {
            rt.spawn(async move {
                loc.signal_ref(|_| ())
                    .for_each(move |()| {
                        notify.notify_one();
                        std::future::ready(())
                    })
                    .await;
            });
        } else {
            tracing::warn!("departures: geoclue not registered before departures; auto-refresh-on-location disabled");
        }

        handles
    }
}

#[must_use]
pub fn service() -> DeparturesService {
    DeparturesService
}

/// Read the current location from geoclue's cross-thread shared handle.
/// `Unavailable` when geoclue isn't registered — [`resolve_location`] then
/// falls back to the home place.
fn current_location() -> LocationState {
    geoclue::shared_location().map_or(LocationState::Unavailable, |m| m.get_cloned())
}

async fn poll_loop(
    state: Mutable<DeparturesState>,
    notify: Arc<Notify>,
    places: Arc<Vec<Place>>,
) {
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

        let loc = current_location();
        let places = Arc::clone(&places);
        let result = match tokio::task::spawn_blocking(move || {
            fetch_for_location(&loc, &places[..])
        })
        .await
        {
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
    let notify = registry::with(|r| {
        r.get::<DeparturesHandles>()
            .map(|h| h.notify.clone())
    });
    if let Some(n) = notify {
        n.notify_one();
    } else {
        tracing::warn!("departures::refresh: service not registered");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::geoclue::{LocationSnapshot, LocationSource};
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

    fn sample_dep(line: &str, direction: &str) -> Departure {
        Departure {
            line: line.into(),
            direction: direction.into(),
            planned: future_now(),
            actual: future_now(),
            delay_minutes: 0,
            cancelled: false,
            trip_id: "t".into(),
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
        let f = Filter { lines: vec![], directions: vec!["Spandau".into()] };
        assert!(f.matches(&sample_dep("S9", "Spandau")));
        assert!(!f.matches(&sample_dep("S9", "Wildau")));
    }

    // ── Geo / resolution ───────────────────────────────────────────────────--

    #[test]
    fn haversine_is_zero_for_same_point_and_sane_for_known_pair() {
        assert!(haversine_km(52.0, 13.0, 52.0, 13.0) < 1e-6);
        // S Schöneweide ↔ Alexanderplatz ≈ 8–9 km.
        let d = haversine_km(52.4556, 13.5085, 52.5219, 13.4132);
        assert!((7.0..11.0).contains(&d), "got {d} km");
    }

    fn default_places() -> Vec<Place> {
        parse_places(DEFAULT_CONFIG).expect("default config parses")
    }

    #[test]
    fn nearest_place_matches_within_radius_only() {
        let places = default_places();
        assert_eq!(
            nearest_place(&places, 52.4556, 13.5085).map(|p| p.name.as_str()),
            Some("Schöneweide"),
        );
        // Hamburg — far outside the 12 km radius.
        assert!(nearest_place(&places, 53.5511, 9.9937).is_none());
    }

    fn resolved_at(lat: f64, lon: f64) -> LocationState {
        LocationState::Resolved(LocationSnapshot {
            lat,
            lon,
            label_hint: None,
            source: LocationSource::GeoClue,
        })
    }

    #[test]
    fn resolve_home_location_is_known_station() {
        let places = default_places();
        match resolve_location(&resolved_at(52.4556, 13.5085), &places) {
            Resolution::Known { station, label, .. } => {
                assert_eq!(station, "900180001");
                assert_eq!(label, "Schöneweide");
            }
            other @ Resolution::Nearby { .. } => panic!("expected Known, got {other:?}"),
        }
    }

    #[test]
    fn resolve_far_location_needs_nearby_lookup() {
        let places = default_places();
        assert!(matches!(
            resolve_location(&resolved_at(53.5511, 9.9937), &places),
            Resolution::Nearby { .. }
        ));
    }

    #[test]
    fn resolve_unresolved_falls_back_to_first_place() {
        let places = default_places();
        for loc in [LocationState::Resolving, LocationState::Unavailable] {
            match resolve_location(&loc, &places) {
                Resolution::Known { label, .. } => assert_eq!(label, "Schöneweide"),
                other @ Resolution::Nearby { .. } => panic!("expected Known for {loc:?}, got {other:?}"),
            }
        }
    }

    // ── Config parsing ─────────────────────────────────────────────────────--

    #[test]
    fn default_config_parses_to_schoeneweide() {
        let places = default_places();
        assert_eq!(places.len(), 1);
        assert_eq!(places[0].station, "900180001");
        assert_eq!(places[0].filter.lines, ["S8", "S85", "S9"]);
        assert!(places[0].filter.directions.iter().any(|d| d == "Spandau"));
        assert!((places[0].radius_km - 12.0).abs() < 1e-9);
    }

    #[test]
    fn parse_places_applies_radius_default_and_empty_filter() {
        let toml = "\
            [[place]]\n\
            name = \"Test\"\n\
            lat = 1.0\n\
            lon = 2.0\n\
            station = \"123\"\n";
        let places = parse_places(toml).expect("parses");
        assert_eq!(places.len(), 1);
        assert!((places[0].radius_km - 12.0).abs() < 1e-9); // serde default
        assert!(places[0].filter.lines.is_empty());
        assert!(places[0].filter.directions.is_empty());
    }

    #[test]
    fn parse_places_empty_doc_is_empty_not_error() {
        assert!(parse_places("").expect("empty parses").is_empty());
    }

    #[test]
    fn parse_places_drops_blank_filter_entries() {
        // A stray "" or whitespace entry must not survive into the filter,
        // or it would become an accidental allow-all (empty substring).
        let toml = "\
            [[place]]\n\
            name = \"T\"\n\
            lat = 1.0\n\
            lon = 2.0\n\
            station = \"1\"\n\
            lines = [\"\"]\n\
            directions = [\"Spandau\", \"\", \"  \"]\n";
        let places = parse_places(toml).expect("parses");
        assert_eq!(places[0].filter.directions, ["Spandau"]);
        assert!(places[0].filter.lines.is_empty()); // [""] collapses to allow-all
    }

    #[test]
    fn parse_places_malformed_is_err() {
        assert!(parse_places("[[place]]\nname = ").is_err());
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
        let body = r#"[{"type":"stop","id":"111","name":"Only Bus","products":{"suburban":false}}]"#;
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
