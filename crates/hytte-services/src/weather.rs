//! Current-weather service. Fetches Open-Meteo for the location resolved by
//! [`crate::places`], maps WMO weather codes to freedesktop symbolic icons,
//! and refreshes every [`POLL_INTERVAL`] (plus on demand via [`refresh`] and
//! whenever the location changes).
//!
//! `places::service()` MUST be registered before `weather::service()` in the
//! `App` builder — `start` reads places' shared location handle (place coords
//! when a place is matched, else the raw `GeoClue` fix) to wire the
//! re-fetch-on-location-change bridge.

use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_reactive::{Service, registry, spawn_supervised};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use tokio::sync::Notify;

use crate::geoclue::{LocationSnapshot, LocationState};
use crate::places;

/// Periodic refresh cadence. The first `interval` tick fires immediately, so
/// boot triggers one fetch without waiting 15 minutes.
const POLL_INTERVAL: Duration = Duration::from_mins(15);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(20);

/// State of the weather card. Starts [`WeatherState::Loading`]; a successful
/// fetch yields [`WeatherState::Resolved`]; failures yield
/// [`WeatherState::Error`] (unless we already have a `Resolved` value, which
/// we keep showing rather than clobbering on a transient network blip).
#[derive(Clone, Debug, Default, PartialEq)]
pub enum WeatherState {
    #[default]
    Loading,
    Resolved(WeatherSnapshot),
    Error(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct WeatherSnapshot {
    pub location: String,
    pub temp_c: f64,
    pub apparent_c: f64,
    /// Forecast daily high/low for today. Falls back to the current temp when
    /// the API omits the daily block.
    pub temp_max_c: f64,
    pub temp_min_c: f64,
    pub humidity_pct: u8,
    pub wind_kmh: f64,
    pub condition: Condition,
    pub fetched_at: SystemTime,
}

/// A weather condition: the raw WMO code plus a display label and a
/// freedesktop symbolic icon name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Condition {
    pub code: u8,
    pub label: &'static str,
    pub icon: &'static str,
}

/// Pure mapping of a WMO weather code to a [`Condition`]. Unmapped codes fall
/// through to a generic severe-alert glyph.
#[must_use]
pub fn condition_for_code(code: u8) -> Condition {
    let (label, icon) = match code {
        0 => ("Clear", "weather-clear-symbolic"),
        1..=3 => ("Partly cloudy", "weather-few-clouds-symbolic"),
        45 | 48 => ("Fog", "weather-fog-symbolic"),
        51 | 53 | 55 | 56 | 57 | 61 | 63 | 65 | 66 | 67 => ("Rain", "weather-showers-symbolic"),
        71 | 73 | 75 | 77 => ("Snow", "weather-snow-symbolic"),
        80..=82 => ("Showers", "weather-showers-scattered-symbolic"),
        85 | 86 => ("Snow showers", "weather-snow-symbolic"),
        95 | 96 | 99 => ("Thunderstorm", "weather-storm-symbolic"),
        _ => ("Unknown", "weather-severe-alert-symbolic"),
    };
    Condition { code, label, icon }
}

#[doc(hidden)]
#[derive(Default)]
pub struct WeatherHandles {
    pub(crate) state: Mutable<WeatherState>,
    pub(crate) notify: Arc<Notify>,
}

pub struct WeatherService;

impl Service for WeatherService {
    type Handles = WeatherHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = WeatherHandles::default();
        let state = handles.state.clone();
        let notify = handles.notify.clone();
        // Supervised: the poll loop owns the `state` signal and parses upstream
        // weather JSON, so a panic there must restart rather than freeze it.
        spawn_supervised("weather", {
            let notify = notify.clone();
            move || poll_loop(state.clone(), notify.clone())
        });

        // Bridge: re-fetch whenever the resolved location changes (including
        // its first resolution). Reads places' shared handle, which exists
        // because places::service() is registered first.
        if let Some(loc) = places::shared_location() {
            rt.spawn(async move {
                loc.signal_ref(|_| ())
                    .for_each(move |()| {
                        notify.notify_one();
                        std::future::ready(())
                    })
                    .await;
            });
        } else {
            tracing::warn!(
                "weather: places not registered before weather; auto-refresh-on-location disabled"
            );
        }

        handles
    }
}

#[must_use]
pub fn service() -> WeatherService {
    WeatherService
}

/// Signal of the current weather state. First emission is
/// [`WeatherState::Loading`].
pub fn current() -> impl Signal<Item = WeatherState> {
    registry::with(|r| {
        r.get::<WeatherHandles>()
            .expect("weather::service() not registered")
            .state
            .signal_cloned()
    })
}

/// Force a fetch now (e.g. when the sidebar opens with stale data). A no-op
/// if a fetch is already in flight (the loop's single-flight guard).
pub fn refresh() {
    registry::with(|r| {
        if let Some(h) = r.get::<WeatherHandles>() {
            h.notify.notify_one();
        }
    });
}

async fn poll_loop(state: Mutable<WeatherState>, notify: Arc<Notify>) {
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let in_flight = Arc::new(AtomicBool::new(false));

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            () = notify.notified() => {}
        }
        // Cheap synchronous location check before the single-flight guard,
        // which only needs to wrap the HTTP fetch. `Resolving` means geoclue's
        // first attempt is still in flight — stay on whatever we're showing
        // (Loading at boot) rather than flashing an error before the fix lands.
        let loc = match places::shared_location().map(|m| m.get_cloned()) {
            Some(LocationState::Resolved(loc)) => loc,
            None | Some(LocationState::Resolving) => continue,
            Some(LocationState::Unavailable) => {
                let err = WeatherState::Error(
                    "No location — enable GeoClue or set $TROLLSHELL_WEATHER_CITY".to_string(),
                );
                if state.get_cloned() != err {
                    state.set(err);
                }
                continue;
            }
        };

        if in_flight.swap(true, Ordering::SeqCst) {
            continue;
        }
        let next = match tokio::task::spawn_blocking(move || fetch_weather(&loc)).await {
            Ok(Ok(snap)) => WeatherState::Resolved(snap),
            Ok(Err(e)) => {
                tracing::warn!("weather: fetch failed: {e}");
                // Keep a good prior value rather than flashing an error.
                match state.get_cloned() {
                    prev @ WeatherState::Resolved(_) => prev,
                    _ => WeatherState::Error("network error".to_string()),
                }
            }
            Err(join) => {
                tracing::warn!("weather: fetch join failed: {join}");
                WeatherState::Error("network error".to_string())
            }
        };
        in_flight.store(false, Ordering::SeqCst);

        if state.get_cloned() != next {
            state.set(next);
        }
    }
}

#[derive(serde::Deserialize)]
struct ForecastResponse {
    current: CurrentBlock,
    #[serde(default)]
    daily: Option<DailyBlock>,
}

#[derive(serde::Deserialize)]
struct CurrentBlock {
    temperature_2m: f64,
    apparent_temperature: f64,
    relative_humidity_2m: f64,
    wind_speed_10m: f64,
    weather_code: u8,
}

/// Open-Meteo returns daily fields as parallel arrays (one entry per forecast
/// day); with `forecast_days=1` we want index 0.
#[derive(serde::Deserialize)]
struct DailyBlock {
    #[serde(default)]
    temperature_2m_max: Vec<f64>,
    #[serde(default)]
    temperature_2m_min: Vec<f64>,
}

/// Today's `(max, min)` from the daily block, or `None` if the API omitted it
/// (or returned empty arrays).
fn daily_min_max(f: &ForecastResponse) -> Option<(f64, f64)> {
    let d = f.daily.as_ref()?;
    Some((
        *d.temperature_2m_max.first()?,
        *d.temperature_2m_min.first()?,
    ))
}

/// One blocking Open-Meteo fetch + parse for `loc`. Runs on a
/// `spawn_blocking` thread. For a `GeoClue`-sourced location (no name) it
/// also reverse-geocodes a friendly place name.
fn fetch_weather(loc: &LocationSnapshot) -> Result<WeatherSnapshot, String> {
    let agent = http_agent();
    let url = format!(
        "https://api.open-meteo.com/v1/forecast\
         ?latitude={lat}&longitude={lon}\
         &current=temperature_2m,apparent_temperature,relative_humidity_2m,wind_speed_10m,weather_code\
         &daily=temperature_2m_max,temperature_2m_min&forecast_days=1\
         &timezone=auto",
        lat = loc.lat,
        lon = loc.lon,
    );

    let mut resp = agent.get(&url).call().map_err(|e| format!("http: {e}"))?;
    let body = resp
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .map_err(|e| format!("body: {e}"))?;
    let forecast = parse_forecast(&body)?;
    let current = &forecast.current;
    // Daily high/low for today; degrade to the current temp if absent.
    let (temp_max_c, temp_min_c) =
        daily_min_max(&forecast).unwrap_or((current.temperature_2m, current.temperature_2m));

    let location = match &loc.label_hint {
        Some(name) => name.clone(),
        None => reverse_geocode(&agent, loc.lat, loc.lon)
            .unwrap_or_else(|| "Current location".to_string()),
    };

    Ok(WeatherSnapshot {
        location,
        temp_c: current.temperature_2m,
        apparent_c: current.apparent_temperature,
        temp_max_c,
        temp_min_c,
        humidity_pct: pct_to_u8(current.relative_humidity_2m),
        wind_kmh: current.wind_speed_10m,
        condition: condition_for_code(current.weather_code),
        fetched_at: SystemTime::now(),
    })
}

fn parse_forecast(body: &str) -> Result<ForecastResponse, String> {
    serde_json::from_str::<ForecastResponse>(body).map_err(|e| format!("decode: {e}"))
}

/// Clamp a humidity percentage into `0..=100` and round to a `u8`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn pct_to_u8(v: f64) -> u8 {
    v.round().clamp(0.0, 100.0) as u8
}

// ── Reverse geocoding (GeoClue source only) ────────────────────────────────

/// Descriptive User-Agent for Nominatim, whose usage policy rejects stock
/// library User-Agents. Volume is tiny — we only reverse-geocode on a location
/// *change*, and cache the result below.
const NOMINATIM_UA: &str = "trollshell/0.1 (+https://github.com/vibec0re/trollshell)";

/// Subset of a Nominatim `jsonv2` reverse response. `name` is the localized
/// name of the matched feature at our requested zoom (a district like
/// "Oberschöneweide"); `display_name` is the full comma-joined address, used
/// only as a fallback when `name` is empty.
#[derive(serde::Deserialize)]
struct ReverseResponse {
    #[serde(default)]
    name: String,
    #[serde(default)]
    display_name: String,
}

/// Cache of rounded `(lat*100, lon*100)` → place name, so we don't re-geocode
/// every refresh when the location is unchanged. Process-lifetime only.
static GEO_CACHE: OnceLock<Mutex<HashMap<(i32, i32), String>>> = OnceLock::new();

#[allow(clippy::cast_possible_truncation)]
fn cache_key(lat: f64, lon: f64) -> (i32, i32) {
    ((lat * 100.0).round() as i32, (lon * 100.0).round() as i32)
}

/// Reverse-geocode `(lat, lon)` to a place name via OSM Nominatim, memoized by
/// a coarse coordinate key. Returns `None` on any failure — the caller falls
/// back to a generic label.
///
/// Open-Meteo (our forecast + forward-geocode provider) has no reverse
/// endpoint — `/v1/reverse` 404s — so this one call reaches for Nominatim.
fn reverse_geocode(agent: &ureq::Agent, lat: f64, lon: f64) -> Option<String> {
    let key = cache_key(lat, lon);
    let cache = GEO_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some(name) = guard.get(&key)
    {
        return Some(name.clone());
    }

    // zoom=14 ≈ suburb/district granularity ("Oberschöneweide", not just
    // "Berlin"); accept-language=en matches the forward-geocode path.
    let url = format!(
        "https://nominatim.openstreetmap.org/reverse\
         ?lat={lat}&lon={lon}&format=jsonv2&zoom=14&accept-language=en"
    );
    let mut resp = agent
        .get(&url)
        .header("User-Agent", NOMINATIM_UA)
        .call()
        .ok()?;
    let body = resp
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .ok()?;
    let name = parse_reverse(&body)?;

    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, name.clone());
    }
    Some(name)
}

fn parse_reverse(body: &str) -> Option<String> {
    let parsed: ReverseResponse = serde_json::from_str(body).ok()?;
    let name = parsed.name.trim();
    if !name.is_empty() {
        return Some(name.to_owned());
    }
    // No primary name (or an error response) — take the most specific segment
    // of the full address, if any.
    let first = parsed.display_name.split(',').next()?.trim();
    (!first.is_empty()).then(|| first.to_owned())
}

fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(HTTP_CONNECT_TIMEOUT))
        .timeout_global(Some(HTTP_READ_TIMEOUT))
        .build();
    config.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condition_known_codes() {
        assert_eq!(condition_for_code(0).label, "Clear");
        assert_eq!(condition_for_code(0).icon, "weather-clear-symbolic");
        for c in [1, 2, 3] {
            assert_eq!(condition_for_code(c).label, "Partly cloudy");
        }
        for c in [45, 48] {
            assert_eq!(condition_for_code(c).icon, "weather-fog-symbolic");
        }
        assert_eq!(condition_for_code(61).label, "Rain");
        assert_eq!(condition_for_code(71).label, "Snow");
        assert_eq!(condition_for_code(95).label, "Thunderstorm");
    }

    #[test]
    fn condition_unknown_code_is_severe_alert() {
        let c = condition_for_code(200);
        assert_eq!(c.label, "Unknown");
        assert_eq!(c.icon, "weather-severe-alert-symbolic");
        assert_eq!(c.code, 200);
    }

    #[test]
    fn parse_forecast_ok() {
        let body = r#"{"current":{"temperature_2m":18.4,"apparent_temperature":16.1,
            "relative_humidity_2m":64,"wind_speed_10m":12.3,"weather_code":3},
            "daily":{"temperature_2m_max":[22.0],"temperature_2m_min":[14.0]}}"#;
        let f = parse_forecast(body).expect("parses");
        assert!((f.current.temperature_2m - 18.4).abs() < 1e-6);
        assert_eq!(f.current.weather_code, 3);
        assert_eq!(pct_to_u8(f.current.relative_humidity_2m), 64);
        assert_eq!(daily_min_max(&f), Some((22.0, 14.0)));
    }

    #[test]
    fn parse_forecast_missing_current_is_err() {
        assert!(parse_forecast(r#"{"latitude":59.3}"#).is_err());
        assert!(parse_forecast("nonsense").is_err());
    }

    #[test]
    fn daily_min_max_absent_or_empty_is_none() {
        // No daily block at all.
        let body = r#"{"current":{"temperature_2m":18.0,"apparent_temperature":18.0,
            "relative_humidity_2m":50,"wind_speed_10m":5.0,"weather_code":0}}"#;
        assert_eq!(daily_min_max(&parse_forecast(body).unwrap()), None);
        // Daily block present but arrays empty.
        let body = r#"{"current":{"temperature_2m":18.0,"apparent_temperature":18.0,
            "relative_humidity_2m":50,"wind_speed_10m":5.0,"weather_code":0},
            "daily":{"temperature_2m_max":[],"temperature_2m_min":[]}}"#;
        assert_eq!(daily_min_max(&parse_forecast(body).unwrap()), None);
    }

    #[test]
    fn pct_clamps_and_rounds() {
        assert_eq!(pct_to_u8(63.6), 64);
        assert_eq!(pct_to_u8(-5.0), 0);
        assert_eq!(pct_to_u8(250.0), 100);
    }

    #[test]
    fn parse_reverse_prefers_name() {
        let body = r#"{"name":"Oberschöneweide",
            "display_name":"Oberschöneweide, Treptow-Köpenick, Berlin, Germany"}"#;
        assert_eq!(parse_reverse(body).as_deref(), Some("Oberschöneweide"));
    }

    #[test]
    fn parse_reverse_falls_back_to_display_name() {
        let body = r#"{"name":"","display_name":"Reinbeckstraße, Berlin, Germany"}"#;
        assert_eq!(parse_reverse(body).as_deref(), Some("Reinbeckstraße"));
    }

    #[test]
    fn parse_reverse_error_or_garbage_is_none() {
        assert_eq!(parse_reverse(r#"{"error":"Unable to geocode"}"#), None);
        assert_eq!(parse_reverse("not json"), None);
    }
}
