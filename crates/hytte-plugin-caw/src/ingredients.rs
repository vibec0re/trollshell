//! The morning briefing's **ingredients** — the one-shot, blocking I/O side
//! (#407).
//!
//! Once a day (see [`crate::briefing`]) caw gathers the day's shape: the
//! weather at home and the first useful departure. Each fetch runs exactly once
//! per briefing — this module is *not* a poller; the always-on pollers stay
//! where they live (the weather/departures plugins, the native services).
//!
//! # Data path
//!
//! [`gather`] fetches only weather and departures — the two ingredients with
//! no host-pushed equivalent. Calendar events are **not** fetched here: the
//! proto grew domain `StateKey`s (`CalendarUpcoming`, `SessionLocked`,
//! `NowPlaying`) in #484 (shipped in PR #539), `main.rs` subscribes
//! `StateKey::CalendarUpcoming` and relays each push through
//! [`EventBrief::from_upcoming`], and [`crate::briefing::brief_now`]
//! overwrites [`Ingredients::events`] with the latest host-pushed list right
//! before composing. `gather()` itself never touches the calendar:
//!
//! - **Weather** — open-meteo, located by the first `[[place]]`'s `lat`/`lon`
//!   in `~/.config/trollshell/places.toml` (home), falling back to a forward
//!   geocode of `$TROLLSHELL_WEATHER_CITY` (the same env the weather stack
//!   honors). No `GeoClue`: a once-a-day briefing doesn't warrant a D-Bus
//!   dependency, and "weather at home in the morning" is the right semantic
//!   anyway. No host `StateKey` carries weather, so this stays a direct fetch.
//! - **Departures** — the HAFAS endpoint (`v6.bvg.transport.rest`), for the
//!   same first place's `station`, filtered by its `lines`/`directions` and
//!   walk budget; the *first catchable* row wins. Same story: no host
//!   `StateKey` for departures either.
//! - **Calendar** — sourced entirely from the host push described above.
//!   [`gather`] leaves [`Ingredients::events`] empty; the caller
//!   ([`crate::briefing::brief_now`]) fills it in from the command lane.
//!
//! Everything here is best-effort: a missing config or a failed fetch just
//! leaves that ingredient absent and the composer degrades gracefully — caw
//! always caws *something*.

use std::path::PathBuf;
use std::time::Duration;

use chrono::DateTime;
use serde::Deserialize;

// Same BVG fetch as `hytte-services/src/departures.rs` and
// `hytte-plugin-departures/src/feed.rs` (30 / 5s / 10s, independently
// implemented — plugins can't link `hytte-services`). This module's
// numbers differ on purpose: `FETCH_COUNT` is 20 (see its own comment
// below); the read timeout is 15s because this is a once-a-day briefing
// fetch, not a live panel, so a slow reply delays that day's briefing
// instead of stalling something on screen. #826
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// Where the shared home/station config lives, relative to `$HOME` — the same
/// `places.toml` the native `places` service owns and the departures plugin
/// reads.
const CONFIG_REL_PATH: &str = ".config/trollshell/places.toml";

/// How many departures to request — enough that the line/direction filter and
/// the walk budget still leave a catchable row.
const FETCH_COUNT: usize = 20;

/// Everything the briefing composer works from. Any part may be absent.
#[derive(Debug, Default)]
pub(crate) struct Ingredients {
    pub weather: Option<WeatherBrief>,
    pub departure: Option<DepartureBrief>,
    /// Upcoming calendar events. [`gather`] always leaves this empty — the
    /// host-pushed `CalendarUpcoming` list (#484) is filled in by
    /// [`crate::briefing::brief_now`], not by this struct's own gather path
    /// (see the module docs).
    pub events: Vec<EventBrief>,
}

/// One weather reading, already briefing-shaped.
#[derive(Debug)]
pub(crate) struct WeatherBrief {
    pub temp_c: f64,
    /// Lowercased condition label ("rain", "partly cloudy", …).
    pub label: &'static str,
    /// Today's forecast high.
    pub high_c: f64,
}

/// The first useful departure.
#[derive(Debug)]
pub(crate) struct DepartureBrief {
    /// Line label, e.g. `"S9"`.
    pub line: String,
    /// Destination, e.g. `"Spandau"`.
    pub direction: String,
    /// Minutes until the (delay-adjusted) departure.
    pub mins: i64,
    /// Minutes until you must leave (`mins - walk_minutes`), when the place
    /// configures a walk budget.
    pub leave_in: Option<i64>,
}

/// One upcoming calendar event, briefing-shaped. Filled from the host's
/// [`CalendarUpcoming`](hytte_plugin::proto::StateKey::CalendarUpcoming) push
/// (#484) via [`EventBrief::from_upcoming`]. `Clone` so the briefing loop can keep
/// the latest list and hand a copy to each `spawn_blocking` compose.
#[derive(Debug, Clone)]
pub(crate) struct EventBrief {
    /// Local `HH:MM` start.
    pub hhmm: String,
    pub summary: String,
}

impl EventBrief {
    /// Project a host-pushed [`UpcomingEvent`](hytte_plugin::proto::UpcomingEvent)
    /// onto the briefing shape (#484): the start's local `HH:MM` plus the title.
    /// Times ride the wire as Unix seconds, so the local formatting lives here.
    pub(crate) fn from_upcoming(event: &hytte_plugin::proto::UpcomingEvent) -> Self {
        let hhmm = DateTime::from_timestamp(event.start_unix, 0).map_or_else(String::new, |dt| {
            dt.with_timezone(&chrono::Local).format("%H:%M").to_string()
        });
        Self {
            hhmm,
            summary: event.title.clone(),
        }
    }
}

/// Gather every ingredient, best-effort. Blocking (two HTTPS round-trips at
/// most) — run on a `spawn_blocking` thread.
pub(crate) fn gather() -> Ingredients {
    let place = load_first_place();
    Ingredients {
        weather: weather_brief(place.as_ref()),
        departure: departure_brief(place.as_ref(), chrono::Utc::now().timestamp()),
        events: Vec::new(),
    }
}

fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(HTTP_CONNECT_TIMEOUT))
        .timeout_global(Some(HTTP_READ_TIMEOUT))
        .build();
    config.into()
}

// ── The home place (shared `places.toml` subset) ─────────────────────────────

/// The departures- and location-relevant subset of a `[[place]]` — unknown
/// fields (the resolver's `ssids`, `radius_km`, …) are simply ignored.
#[derive(Debug, Deserialize)]
pub(crate) struct PlaceCfg {
    #[serde(default)]
    station: Option<String>,
    #[serde(default)]
    walk_minutes: u32,
    #[serde(default)]
    lines: Vec<String>,
    #[serde(default)]
    directions: Vec<String>,
    #[serde(default)]
    lat: Option<f64>,
    #[serde(default)]
    lon: Option<f64>,
}

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    place: Vec<PlaceCfg>,
}

/// Parse the **first** `[[place]]` (home — the provisional-home stance the
/// departures plugin documents) out of a `places.toml` body. Pure, so the
/// schema subset is unit-testable.
fn parse_first_place(toml_text: &str) -> Option<PlaceCfg> {
    let cfg: ConfigFile = toml::from_str(toml_text).ok()?;
    cfg.place.into_iter().next()
}

fn config_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(CONFIG_REL_PATH))
}

fn load_first_place() -> Option<PlaceCfg> {
    let text = std::fs::read_to_string(config_path()?).ok()?;
    parse_first_place(&text)
}

// ── Weather (open-meteo, one shot) ───────────────────────────────────────────

/// Pure WMO-code → lowercase label mapping — the label half of the house
/// `condition_for_code` (the briefing needs no icon).
fn condition_label(code: u8) -> &'static str {
    match code {
        0 => "clear",
        1..=3 => "partly cloudy",
        45 | 48 => "fog",
        51 | 53 | 55 | 56 | 57 | 61 | 63 | 65 | 66 | 67 => "rain",
        71 | 73 | 75 | 77 => "snow",
        80..=82 => "showers",
        85 | 86 => "snow showers",
        95 | 96 | 99 => "thunderstorm",
        _ => "weird sky",
    }
}

#[derive(Deserialize)]
struct ForecastResponse {
    current: CurrentBlock,
    #[serde(default)]
    daily: Option<DailyBlock>,
}

#[derive(Deserialize)]
struct CurrentBlock {
    temperature_2m: f64,
    weather_code: u8,
}

/// open-meteo daily fields are parallel arrays; with `forecast_days=1` index 0
/// is today.
#[derive(Deserialize)]
struct DailyBlock {
    #[serde(default)]
    temperature_2m_max: Vec<f64>,
}

/// Parse a forecast body into a [`WeatherBrief`] (high degrades to the current
/// temp when the daily block is absent). Pure.
fn parse_forecast(body: &str) -> Result<WeatherBrief, String> {
    let f: ForecastResponse = serde_json::from_str(body).map_err(|e| format!("decode: {e}"))?;
    let high_c = f
        .daily
        .as_ref()
        .and_then(|d| d.temperature_2m_max.first().copied())
        .unwrap_or(f.current.temperature_2m);
    Ok(WeatherBrief {
        temp_c: f.current.temperature_2m,
        label: condition_label(f.current.weather_code),
        high_c,
    })
}

/// Resolve briefing coordinates: home's `lat`/`lon` from `places.toml`, else a
/// forward geocode of `$TROLLSHELL_WEATHER_CITY` (the weather stack's own
/// fallback env). `None` = no weather in today's caw.
fn coords(place: Option<&PlaceCfg>) -> Option<(f64, f64)> {
    if let Some(p) = place
        && let (Some(lat), Some(lon)) = (p.lat, p.lon)
    {
        return Some((lat, lon));
    }
    let city = std::env::var("TROLLSHELL_WEATHER_CITY")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    match geocode_city(&city) {
        Ok(pair) => Some(pair),
        Err(e) => {
            eprintln!("[caw] briefing geocode of $TROLLSHELL_WEATHER_CITY failed: {e}");
            None
        }
    }
}

#[derive(Deserialize)]
struct GeocodeResponse {
    #[serde(default)]
    results: Vec<GeocodeResult>,
}

#[derive(Deserialize)]
struct GeocodeResult {
    latitude: f64,
    longitude: f64,
}

/// Blocking forward geocode via open-meteo (the same endpoint the weather
/// plugin's city fallback uses).
fn geocode_city(city: &str) -> Result<(f64, f64), String> {
    let mut resp = http_agent()
        .get("https://geocoding-api.open-meteo.com/v1/search")
        .query("name", city)
        .query("count", "1")
        .query("language", "en")
        .query("format", "json")
        .call()
        .map_err(|e| format!("http: {e}"))?;
    let body = resp
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .map_err(|e| format!("body: {e}"))?;
    let parsed: GeocodeResponse =
        serde_json::from_str(&body).map_err(|e| format!("decode: {e}"))?;
    let first = parsed
        .results
        .into_iter()
        .next()
        .ok_or("no geocode match")?;
    Ok((first.latitude, first.longitude))
}

/// One blocking weather fetch for the briefing, or `None` (logged) on any
/// failure.
fn weather_brief(place: Option<&PlaceCfg>) -> Option<WeatherBrief> {
    let (lat, lon) = coords(place)?;
    let url = format!(
        "https://api.open-meteo.com/v1/forecast\
         ?latitude={lat}&longitude={lon}\
         &current=temperature_2m,weather_code\
         &daily=temperature_2m_max&forecast_days=1&timezone=auto"
    );
    let fetched = http_agent()
        .get(&url)
        .call()
        .map_err(|e| format!("http: {e}"))
        .and_then(|mut resp| {
            resp.body_mut()
                .with_config()
                .limit(1024 * 1024)
                .read_to_string()
                .map_err(|e| format!("body: {e}"))
        })
        .and_then(|body| parse_forecast(&body));
    match fetched {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("[caw] briefing weather fetch failed: {e}");
            None
        }
    }
}

// ── Departures (HAFAS, one shot) ─────────────────────────────────────────────

#[derive(Deserialize)]
struct ApiResponse {
    #[serde(default)]
    departures: Vec<ApiDeparture>,
}

#[derive(Deserialize)]
struct ApiDeparture {
    #[serde(default)]
    when: Option<String>,
    #[serde(default, rename = "plannedWhen")]
    planned_when: Option<String>,
    #[serde(default)]
    cancelled: bool,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    line: Option<ApiLine>,
}

#[derive(Deserialize)]
struct ApiLine {
    #[serde(default)]
    name: String,
    #[serde(default)]
    product: String,
}

/// Whether a row passes the place's line/direction filter (empty axis = allow
/// all; line exact, direction substring, both case-insensitive — the house
/// filter semantics).
fn filter_matches(place: &PlaceCfg, line: &str, direction: &str) -> bool {
    let line_ok = place.lines.is_empty()
        || place
            .lines
            .iter()
            .any(|l| !l.trim().is_empty() && l.eq_ignore_ascii_case(line));
    let dir_ok = place.directions.is_empty() || {
        let dir = direction.to_lowercase();
        place
            .directions
            .iter()
            .any(|want| !want.trim().is_empty() && dir.contains(&want.to_lowercase()))
    };
    line_ok && dir_ok
}

/// Pick the **first useful** departure out of a raw HAFAS body: suburban, not
/// cancelled, passing the filter, and still catchable given the walk budget.
/// Pure, so the pick logic is unit-testable.
fn pick_departure(body: &str, place: &PlaceCfg, now_unix: i64) -> Option<DepartureBrief> {
    let api: ApiResponse = serde_json::from_str(body).ok()?;
    let walk = i64::from(place.walk_minutes);
    api.departures.into_iter().find_map(|row| {
        let line = row.line?;
        if line.product != "suburban" || line.name.is_empty() || row.cancelled {
            return None;
        }
        let direction = row.direction.unwrap_or_default();
        if !filter_matches(place, &line.name, &direction) {
            return None;
        }
        let actual_raw = row.when.or(row.planned_when)?;
        let actual = DateTime::parse_from_rfc3339(&actual_raw).ok()?;
        let mins = (actual.timestamp() - now_unix) / 60;
        // Catchable: you can still make it after the walk to the platform.
        if mins < walk {
            return None;
        }
        Some(DepartureBrief {
            line: line.name,
            direction,
            mins,
            leave_in: (place.walk_minutes > 0).then_some(mins - walk),
        })
    })
}

/// One blocking departures fetch + pick, or `None` (logged) when there's no
/// configured station / no catchable row / no network.
fn departure_brief(place: Option<&PlaceCfg>, now_unix: i64) -> Option<DepartureBrief> {
    let place = place?;
    let station = place.station.as_deref()?.trim();
    if station.is_empty() {
        return None;
    }
    let url = format!(
        "https://v6.bvg.transport.rest/stops/{station}/departures\
         ?results={FETCH_COUNT}&suburban=true&subway=false&bus=false&tram=false\
         &regional=false&express=false&ferry=false&tariff=false&language=de"
    );
    let fetched = http_agent()
        .get(&url)
        .call()
        .map_err(|e| format!("http: {e}"))
        .and_then(|mut resp| {
            resp.body_mut()
                .with_config()
                .limit(4 * 1024 * 1024)
                .read_to_string()
                .map_err(|e| format!("body: {e}"))
        });
    match fetched {
        Ok(body) => pick_departure(&body, place, now_unix),
        Err(e) => {
            eprintln!("[caw] briefing departures fetch failed: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(rfc3339: &str) -> i64 {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("valid rfc3339")
            .timestamp()
    }

    /// A place with a walk budget and a filter, in the native schema.
    fn place() -> PlaceCfg {
        parse_first_place(
            "[[place]]\n\
             name = \"Schöneweide\"\n\
             lat = 52.4556\n\
             lon = 13.5085\n\
             station = \"900180001\"\n\
             walk_minutes = 10\n\
             lines = [\"S8\", \"S9\"]\n\
             directions = [\"Spandau\"]\n",
        )
        .expect("parses")
    }

    /// A HAFAS body: a bus (dropped), a cancelled S9 (dropped), an S9 too soon
    /// to catch with a 10-min walk (dropped), then the S9 that wins, then a
    /// later S8 that would also match.
    fn hafas_body() -> String {
        r#"{"departures":[
            {"line":{"name":"164","product":"bus"},"direction":"Spandau",
             "when":"2030-01-01T16:20:00+01:00","plannedWhen":"2030-01-01T16:20:00+01:00"},
            {"line":{"name":"S9","product":"suburban"},"direction":"Spandau","cancelled":true,
             "when":null,"plannedWhen":"2030-01-01T16:21:00+01:00"},
            {"line":{"name":"S9","product":"suburban"},"direction":"Spandau",
             "when":"2030-01-01T16:05:00+01:00","plannedWhen":"2030-01-01T16:05:00+01:00"},
            {"line":{"name":"S9","product":"suburban"},"direction":"Spandau",
             "when":"2030-01-01T16:12:00+01:00","plannedWhen":"2030-01-01T16:10:00+01:00"},
            {"line":{"name":"S8","product":"suburban"},"direction":"Spandau",
             "when":"2030-01-01T16:30:00+01:00","plannedWhen":"2030-01-01T16:30:00+01:00"}
        ]}"#
        .to_owned()
    }

    #[test]
    fn first_place_parses_the_briefing_subset() {
        let p = place();
        assert_eq!(p.station.as_deref(), Some("900180001"));
        assert_eq!(p.walk_minutes, 10);
        assert_eq!(p.lines, ["S8", "S9"]);
        assert_eq!(p.directions, ["Spandau"]);
        assert!((p.lat.expect("lat") - 52.4556).abs() < 1e-9);
        assert!((p.lon.expect("lon") - 13.5085).abs() < 1e-9);
    }

    #[test]
    fn first_place_empty_or_garbage_is_none() {
        assert!(parse_first_place("").is_none());
        assert!(parse_first_place("[[place]]\nstation = ").is_none());
    }

    #[test]
    fn coords_prefer_home_over_env() {
        // With a lat/lon place, no env/geocode is consulted at all.
        let got = coords(Some(&place())).expect("home coordinates");
        assert!((got.0 - 52.4556).abs() < 1e-9);
        // A station-only place has no coordinates (and this test env sets no
        // city), so weather is simply absent.
        let stationless =
            parse_first_place("[[place]]\nname = \"Home\"\nstation = \"111\"\n").expect("parses");
        if std::env::var("TROLLSHELL_WEATHER_CITY").is_err() {
            assert!(coords(Some(&stationless)).is_none());
        }
    }

    #[test]
    fn condition_labels_are_lowercase_briefing_words() {
        assert_eq!(condition_label(0), "clear");
        assert_eq!(condition_label(2), "partly cloudy");
        assert_eq!(condition_label(61), "rain");
        assert_eq!(condition_label(75), "snow");
        assert_eq!(condition_label(95), "thunderstorm");
        assert_eq!(condition_label(200), "weird sky");
    }

    #[test]
    fn parse_forecast_reads_current_and_daily_high() {
        let body = r#"{"current":{"temperature_2m":2.6,"weather_code":61},
            "daily":{"temperature_2m_max":[8.4]}}"#;
        let w = parse_forecast(body).expect("parses");
        assert!((w.temp_c - 2.6).abs() < 1e-9);
        assert_eq!(w.label, "rain");
        assert!((w.high_c - 8.4).abs() < 1e-9);
        // No daily block → the high degrades to the current temp.
        let flat = parse_forecast(r#"{"current":{"temperature_2m":3.0,"weather_code":0}}"#)
            .expect("parses");
        assert!((flat.high_c - 3.0).abs() < 1e-9);
        assert!(parse_forecast("not json").is_err());
    }

    #[test]
    fn pick_departure_takes_the_first_useful_row() {
        // 16:00 now, 10-min walk: the 16:05 S9 is uncatchable, the (delayed to)
        // 16:12 S9 wins over the later S8.
        let now = ts("2030-01-01T16:00:00+01:00");
        let d = pick_departure(&hafas_body(), &place(), now).expect("a catchable row");
        assert_eq!(d.line, "S9");
        assert_eq!(d.direction, "Spandau");
        assert_eq!(d.mins, 12, "delay-adjusted `when` drives the countdown");
        assert_eq!(d.leave_in, Some(2), "12 min out minus the 10-min walk");
    }

    #[test]
    fn pick_departure_respects_filter_walk_and_cancellation() {
        let now = ts("2030-01-01T16:00:00+01:00");
        // Direction filter that matches nothing → no pick.
        let mut p = place();
        p.directions = vec!["Birkenwerder".to_owned()];
        assert!(pick_departure(&hafas_body(), &p, now).is_none());
        // No walk budget → the 16:05 S9 is now the first useful one, with no
        // leave-by countdown.
        let mut p = place();
        p.walk_minutes = 0;
        let d = pick_departure(&hafas_body(), &p, now).expect("row");
        assert_eq!((d.mins, d.leave_in), (5, None));
        // After everything departed → none.
        assert!(pick_departure(&hafas_body(), &place(), ts("2030-01-01T18:00:00+01:00")).is_none());
        // Garbage body → none, no panic.
        assert!(pick_departure("not json", &place(), now).is_none());
    }

    #[test]
    fn event_brief_from_upcoming_formats_local_hhmm_and_keeps_the_title() {
        use hytte_plugin::proto::UpcomingEvent;
        // 2026-07-11T13:49:00Z — the local HH:MM depends on the test box's zone,
        // so assert the shape (two colon-separated fields) and the title rather
        // than a fixed clock.
        let e = EventBrief::from_upcoming(&UpcomingEvent {
            start_unix: 1_752_241_740,
            end_unix: 1_752_245_340,
            title: "standup".to_owned(),
            calendar: "Work".to_owned(),
        });
        assert_eq!(e.summary, "standup");
        let (h, m) = e.hhmm.split_once(':').expect("HH:MM");
        assert!(h.len() == 2 && m.len() == 2 && h.parse::<u8>().is_ok());
        // A zero/garbage timestamp degrades to an empty label, never a panic.
        let bad = EventBrief::from_upcoming(&UpcomingEvent {
            start_unix: i64::MAX,
            end_unix: i64::MAX,
            title: "overflow".to_owned(),
            calendar: String::new(),
        });
        assert_eq!(bad.hhmm, "");
    }

    #[test]
    fn filter_semantics_match_the_house_rules() {
        let p = place();
        assert!(
            filter_matches(&p, "s9", "S+U Spandau Bhf"),
            "ci + substring"
        );
        assert!(!filter_matches(&p, "S46", "Spandau"), "wrong line");
        assert!(
            !filter_matches(&p, "S9", "Flughafen BER"),
            "wrong direction"
        );
        let open = parse_first_place("[[place]]\nstation = \"1\"\n").expect("parses");
        assert!(filter_matches(&open, "M10", "Anywhere"), "empty axes allow");
    }
}
