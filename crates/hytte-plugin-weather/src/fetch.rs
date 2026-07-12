//! The weather I/O worker and the open-meteo fetch.
//!
//! A single task owns both directions of the plugin's I/O (the SDK's #280
//! idiom): it drains the command lane (`RefreshNow` from a card click) and
//! re-emits fetch results as [`WeatherMsg`]s the reducer folds in. Location is
//! resolved once at startup ([`crate::location`], retried with backoff), then
//! open-meteo is fetched every [`POLL_INTERVAL`] and on demand.
//!
//! HTTP is blocking `ureq` on a `spawn_blocking` thread — the house idiom
//! (`hytte-services`' weather fetcher, the pet's brain). The parse/mapping code
//! is a verbatim port of `hytte_services::weather`.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use crate::location::{self, LocationSnapshot};
use crate::{Snapshot, WeatherCmd, WeatherMsg, condition_for_code};

/// Periodic refresh cadence (native: 15 minutes). tokio's first `interval` tick
/// fires immediately, so startup fetches once without waiting.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_mins(15);

const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(20);

/// The worker task. Runs until either channel end is gone (session teardown).
pub(crate) async fn run(
    mut cmd_rx: hytte_plugin::CmdReceiver<WeatherCmd>,
    msg_tx: UnboundedSender<WeatherMsg>,
) {
    let Some(location) = resolve_location(&msg_tx).await else {
        return; // session torn down while still resolving
    };

    let mut tick = tokio::time::interval(POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            cmd = cmd_rx.recv() => match cmd {
                // Coalesce a burst of clicks into a single fetch (politeness to
                // open-meteo / Nominatim).
                Some(WeatherCmd::RefreshNow) => while cmd_rx.try_recv().is_ok() {},
                None => return, // command lane closed → session teardown
            },
        }

        let loc = location.clone();
        let msg = match tokio::task::spawn_blocking(move || fetch_weather(&loc)).await {
            Ok(Ok(snap)) => WeatherMsg::Weather(snap),
            Ok(Err(e)) => {
                eprintln!("[weather] fetch failed: {e}");
                WeatherMsg::FetchError
            }
            Err(join) => {
                eprintln!("[weather] fetch task failed: {join}");
                WeatherMsg::FetchError
            }
        };
        if msg_tx.send(msg).is_err() {
            return; // reducer gone
        }
    }
}

/// Resolve a location, retrying with bounded backoff. Surfaces the actionable
/// "no location" state on each miss so the card is never blank. `None` only if
/// the session tore down (msg channel closed) mid-resolution.
async fn resolve_location(msg_tx: &UnboundedSender<WeatherMsg>) -> Option<LocationSnapshot> {
    let mut backoff = Backoff::new();
    loop {
        if let Some(loc) = location::resolve_once().await {
            return Some(loc);
        }
        if msg_tx.send(WeatherMsg::NoLocation).is_err() {
            return None;
        }
        tokio::time::sleep(backoff.delay()).await;
    }
}

/// Bounded exponential backoff for location retries: 5 s doubling to a 5 min cap.
struct Backoff {
    next: Duration,
}

impl Backoff {
    fn new() -> Self {
        Self {
            next: Duration::from_secs(5),
        }
    }

    fn delay(&mut self) -> Duration {
        let d = self.next;
        self.next = (d * 2).min(Duration::from_mins(5));
        d
    }
}

// ── open-meteo fetch (ported from hytte_services::weather) ───────────────────

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

/// open-meteo returns daily fields as parallel arrays (one per forecast day);
/// with `forecast_days=1` we want index 0.
#[derive(serde::Deserialize)]
struct DailyBlock {
    #[serde(default)]
    temperature_2m_max: Vec<f64>,
    #[serde(default)]
    temperature_2m_min: Vec<f64>,
}

/// Today's `(max, min)` from the daily block, or `None` if the API omitted it.
fn daily_min_max(f: &ForecastResponse) -> Option<(f64, f64)> {
    let d = f.daily.as_ref()?;
    Some((
        *d.temperature_2m_max.first()?,
        *d.temperature_2m_min.first()?,
    ))
}

/// One blocking open-meteo fetch + parse for `loc`. For a `GeoClue`-sourced
/// location (no name) it also reverse-geocodes a friendly place name.
fn fetch_weather(loc: &LocationSnapshot) -> Result<Snapshot, String> {
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
            .unwrap_or_else(|| "Current location".to_owned()),
    };

    Ok(Snapshot {
        location,
        temp_c: current.temperature_2m,
        apparent_c: current.apparent_temperature,
        temp_max_c,
        temp_min_c,
        humidity_pct: pct_to_u8(current.relative_humidity_2m),
        wind_kmh: current.wind_speed_10m,
        condition: condition_for_code(current.weather_code),
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

fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(HTTP_CONNECT_TIMEOUT))
        .timeout_global(Some(HTTP_READ_TIMEOUT))
        .build();
    config.into()
}

// ── Reverse geocoding (GeoClue source only) ──────────────────────────────────

/// Descriptive User-Agent for Nominatim, whose usage policy rejects stock
/// library User-Agents. Volume is tiny — we reverse-geocode a coordinate at
/// most once (the result is cached below).
const NOMINATIM_UA: &str = "trollshell/0.1 (+https://github.com/vibec0re/trollshell)";

/// Subset of a Nominatim `jsonv2` reverse response.
#[derive(serde::Deserialize)]
struct ReverseResponse {
    #[serde(default)]
    name: String,
    #[serde(default)]
    display_name: String,
}

/// Cache of rounded `(lat*100, lon*100)` → place name (process lifetime), so a
/// stable location isn't re-geocoded every refresh.
static GEO_CACHE: OnceLock<Mutex<HashMap<(i32, i32), String>>> = OnceLock::new();

#[allow(clippy::cast_possible_truncation)]
fn cache_key(lat: f64, lon: f64) -> (i32, i32) {
    ((lat * 100.0).round() as i32, (lon * 100.0).round() as i32)
}

/// Reverse-geocode `(lat, lon)` to a place name via OSM Nominatim, memoized by a
/// coarse coordinate key. `None` on any failure — the caller falls back to a
/// generic label. open-meteo has no reverse endpoint, hence Nominatim here.
fn reverse_geocode(agent: &ureq::Agent, lat: f64, lon: f64) -> Option<String> {
    let key = cache_key(lat, lon);
    let cache = GEO_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some(name) = guard.get(&key)
    {
        return Some(name.clone());
    }

    // zoom=14 ≈ suburb/district granularity; accept-language=en matches the
    // forward-geocode path.
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
    // No primary name (or an error response) — take the most specific segment of
    // the full address, if any.
    let first = parsed.display_name.split(',').next()?.trim();
    (!first.is_empty()).then(|| first.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{daily_min_max, parse_forecast, parse_reverse, pct_to_u8};

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
        let no_daily = r#"{"current":{"temperature_2m":18.0,"apparent_temperature":18.0,
            "relative_humidity_2m":50,"wind_speed_10m":5.0,"weather_code":0}}"#;
        assert_eq!(daily_min_max(&parse_forecast(no_daily).unwrap()), None);
        let empty_arrays = r#"{"current":{"temperature_2m":18.0,"apparent_temperature":18.0,
            "relative_humidity_2m":50,"wind_speed_10m":5.0,"weather_code":0},
            "daily":{"temperature_2m_max":[],"temperature_2m_min":[]}}"#;
        assert_eq!(daily_min_max(&parse_forecast(empty_arrays).unwrap()), None);
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
