//! The **departures** datasource: a one-shot, consent-gated fetch of the next N
//! catchable S-Bahn departures.
//!
//! This is a deliberate, acknowledged **code dup** of `hytte-plugin-departures`'
//! fetch path (the HAFAS `v6.bvg.transport.rest` client, the `places.toml`
//! station config, the row filter). Phase 1a accepts the duplication; the dedup
//! is **phase 2**, once the plugin proto grows a first-class `Datasource`
//! capability a broker can source *through* a running datasource plugin instead
//! of re-fetching itself. Until then, keeping the broker self-contained (no IPC
//! to the departures plugin, which may not even be enabled) is the simpler,
//! more robust choice.
//!
//! The one behavioural difference from the plugin: the broker returns a **scoped
//! JSON payload** ([`crate::wire::DepartureOut`]) — the next `limit` catchable
//! departures, computed against the fetch instant — rather than a widget tree.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::wire::DepartureOut;

/// Default cap when a `get` omits `limit`. The native board's `DISPLAY_COUNT`.
pub const DEFAULT_LIMIT: usize = 8;
/// Hard cap on `limit`, so a client can't ask for an unbounded payload.
pub const MAX_LIMIT: usize = 30;
/// How many rows to request upstream — larger than the display cap so a
/// line/direction filter still has enough to fill the list. Native `FETCH_COUNT`.
const FETCH_COUNT: usize = 30;

const HTTP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const HTTP_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Where the station config lives, relative to `$HOME` — the same file the shell
/// and the departures plugin read.
const CONFIG_REL_PATH: &str = ".config/trollshell/places.toml";

// ── Row model (internal; mapped to the wire `DepartureOut`) ───────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    line: String,
    direction: String,
    hhmm: String,
    actual_unix: i64,
    delay_minutes: i64,
    cancelled: bool,
}

// ── Filter (ported verbatim from the native `Filter`) ─────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Filter {
    lines: Vec<String>,
    directions: Vec<String>,
}

impl Filter {
    fn matches(&self, r: &Row) -> bool {
        let line_ok =
            self.lines.is_empty() || self.lines.iter().any(|l| l.eq_ignore_ascii_case(&r.line));
        let dir_ok = self.directions.is_empty() || {
            let dir = r.direction.to_lowercase();
            self.directions
                .iter()
                .any(|want| dir.contains(&want.to_lowercase()))
        };
        line_ok && dir_ok
    }
}

// ── Wire format (HAFAS / transport.rest) ──────────────────────────────────────

#[derive(Deserialize, Debug)]
struct ApiResponse {
    #[serde(default)]
    departures: Vec<ApiDeparture>,
}

#[derive(Deserialize, Debug)]
struct ApiDeparture {
    #[serde(default)]
    when: Option<String>,
    #[serde(default, rename = "plannedWhen")]
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

#[derive(Deserialize, Debug)]
struct ApiLine {
    #[serde(default)]
    name: String,
    #[serde(default)]
    product: String,
}

/// Convert one wire row into a [`Row`], dropping what we can't render: non-
/// suburban products, rows already departed (> 60 s past, grace for skew), and
/// rows whose timestamps fail to parse. `now_unix` is the fetch instant.
fn into_row(row: ApiDeparture, now_unix: i64) -> Option<Row> {
    let line = row.line?;
    if line.product != "suburban" {
        return None;
    }
    let line_name = line.name;
    if line_name.is_empty() {
        return None;
    }
    let planned_raw = row.planned_when.as_deref()?;
    if DateTime::parse_from_rfc3339(planned_raw).is_err() {
        return None;
    }
    let actual_raw = row.when.as_deref().unwrap_or(planned_raw);
    let actual = DateTime::parse_from_rfc3339(actual_raw).ok()?;
    let actual_unix = actual.timestamp();
    if actual_unix < now_unix - 60 {
        return None;
    }
    let delay_minutes = row.delay.unwrap_or(0) / 60;
    Some(Row {
        line: line_name,
        direction: row.direction.unwrap_or_default(),
        hhmm: actual.format("%H:%M").to_string(),
        actual_unix,
        delay_minutes,
        cancelled: row.cancelled,
    })
}

/// Parse a raw response body into rows, filtering as [`into_row`] describes.
fn parse_response(body: &str, now_unix: i64) -> Result<Vec<Row>, String> {
    let api: ApiResponse = serde_json::from_str(body).map_err(|e| format!("decode: {e}"))?;
    Ok(api
        .departures
        .into_iter()
        .filter_map(|r| into_row(r, now_unix))
        .collect())
}

/// Whole minutes from `now_unix` until `actual_unix` departs, floored at `0`
/// (an imminent or just-past-grace train reads as `0`, never negative).
fn in_minutes(now_unix: i64, actual_unix: i64) -> i64 {
    ((actual_unix - now_unix).max(0)) / 60
}

/// Map an internal [`Row`] to the scoped wire row.
fn to_out(r: &Row, now_unix: i64) -> DepartureOut {
    DepartureOut {
        line: r.line.clone(),
        direction: r.direction.clone(),
        hhmm: r.hhmm.clone(),
        in_minutes: in_minutes(now_unix, r.actual_unix),
        delay_minutes: r.delay_minutes,
        cancelled: r.cancelled,
    }
}

// ── Station config (ported subset of places.toml) ─────────────────────────────

#[derive(Debug)]
struct StationConfig {
    station: String,
    filter: Filter,
}

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    place: Vec<PlaceCfg>,
}

#[derive(Deserialize)]
struct PlaceCfg {
    #[serde(default)]
    station: Option<String>,
    #[serde(default)]
    lines: Vec<String>,
    #[serde(default)]
    directions: Vec<String>,
}

fn nonblank(items: Vec<String>) -> Vec<String> {
    items.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

fn config_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(CONFIG_REL_PATH))
}

/// Parse the first place's station config out of a `places.toml` body. Pure.
/// `Ok(None)` = no place, or the first place has no (non-blank) `station`.
fn parse_station_config(toml_text: &str) -> Result<Option<StationConfig>, String> {
    let cfg: ConfigFile = toml::from_str(toml_text).map_err(|e| format!("config: {e}"))?;
    let Some(first) = cfg.place.into_iter().next() else {
        return Ok(None);
    };
    let Some(station) = first.station.filter(|s| !s.trim().is_empty()) else {
        return Ok(None);
    };
    Ok(Some(StationConfig {
        station,
        filter: Filter {
            lines: nonblank(first.lines),
            directions: nonblank(first.directions),
        },
    }))
}

/// Load the station config from disk, or an actionable error when there's no
/// file/place/station (the message is surfaced to the agent so it knows the
/// human has to configure a station).
fn load_station_config() -> Result<StationConfig, String> {
    let Some(path) = config_path() else {
        return Err("no departures station configured (HOME unset)".to_owned());
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "no departures station configured — add a [[place]] with a `station` to {}",
                path.display()
            ));
        }
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    match parse_station_config(&text)? {
        Some(cfg) => Ok(cfg),
        None => Err(format!(
            "no departures station configured — set `station` on the first [[place]] in {}",
            path.display()
        )),
    }
}

/// Human-readable one-line status of the departures datasource, for the panel:
/// either the configured station id or the actionable "not configured" hint.
#[must_use]
pub fn status() -> String {
    match load_station_config() {
        Ok(cfg) => format!("station {}", cfg.station),
        Err(e) => e,
    }
}

// ── Fetch ─────────────────────────────────────────────────────────────────────

fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(HTTP_CONNECT_TIMEOUT))
        .timeout_global(Some(HTTP_READ_TIMEOUT))
        .build();
    config.into()
}

/// One blocking HTTP fetch + parse of the suburban departures at `station`.
fn fetch_departures(agent: &ureq::Agent, station: &str) -> Result<Vec<Row>, String> {
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
    parse_response(&body, Utc::now().timestamp())
}

/// One full scoped fetch: (re)load config, fetch, filter, cap to `limit`, and
/// map to the wire rows. **Blocking** — the broker runs it on `spawn_blocking`.
/// `limit` is clamped to `1..=`[`MAX_LIMIT`]; `None` uses [`DEFAULT_LIMIT`].
///
/// # Errors
/// If the station is unconfigured, the HTTP call fails, or the body won't parse.
pub fn fetch_scoped(limit: Option<usize>) -> Result<Vec<DepartureOut>, String> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let cfg = load_station_config()?;
    let agent = http_agent();
    let now_unix = Utc::now().timestamp();
    let rows = fetch_departures(&agent, &cfg.station)?;
    Ok(rows
        .into_iter()
        .filter(|r| cfg.filter.matches(r))
        .take(limit)
        .map(|r| to_out(&r, now_unix))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(rfc3339: &str) -> i64 {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("valid rfc3339")
            .timestamp()
    }

    /// Before every fixture row.
    fn now_before() -> i64 {
        ts("2030-01-01T16:00:00+01:00")
    }

    const FIXTURE: &str = include_str!("../tests/fixtures/departures.json");

    fn row(line: &str, direction: &str) -> Row {
        Row {
            line: line.to_owned(),
            direction: direction.to_owned(),
            hhmm: "16:00".to_owned(),
            actual_unix: now_before(),
            delay_minutes: 0,
            cancelled: false,
        }
    }

    #[test]
    fn parse_response_keeps_suburban_in_order_drops_bus() {
        let parsed = parse_response(FIXTURE, now_before()).expect("parses");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].line, "S9");
        assert_eq!(parsed[0].direction, "Spandau");
        assert_eq!(parsed[1].line, "S46");
        assert_eq!(parsed[1].delay_minutes, 5);
        assert_eq!(parsed[2].line, "S8");
        assert!(parsed[2].cancelled);
    }

    #[test]
    fn into_row_drops_departed_and_non_suburban() {
        let api: ApiResponse = serde_json::from_str(FIXTURE).expect("fixture parses");
        let rows: Vec<ApiDeparture> = api.departures;
        // The bus row (index 3) is non-suburban → dropped.
        assert!(into_row_by_reparse(3, now_before()).is_none());
        // The S9 (index 0), if "now" is after it, is dropped as departed.
        let after = ts("2030-01-01T18:00:00+01:00");
        assert!(into_row_by_reparse(0, after).is_none());
        drop(rows);
    }

    /// Re-parse the fixture and run `into_row` on the nth wire row (the fixture
    /// is small; a fresh parse per call keeps the helper borrow-free).
    fn into_row_by_reparse(n: usize, now: i64) -> Option<Row> {
        let api: ApiResponse = serde_json::from_str(FIXTURE).expect("fixture parses");
        let wire = api.departures.into_iter().nth(n).expect("row exists");
        into_row(wire, now)
    }

    #[test]
    fn parse_response_empty_and_malformed() {
        assert!(
            parse_response(r#"{"departures": []}"#, now_before())
                .unwrap()
                .is_empty()
        );
        let err = parse_response("{not json", now_before()).unwrap_err();
        assert!(err.starts_with("decode:"), "got: {err}");
    }

    #[test]
    fn in_minutes_floors_at_zero_and_truncates() {
        let now = now_before();
        assert_eq!(
            in_minutes(now, now + 7 * 60 + 59),
            7,
            "truncates toward zero"
        );
        assert_eq!(in_minutes(now, now + 30), 0, "under a minute is 0");
        assert_eq!(in_minutes(now, now - 120), 0, "already past floors at 0");
    }

    #[test]
    fn to_out_maps_fields() {
        let now = now_before();
        let r = Row {
            line: "S9".to_owned(),
            direction: "Spandau".to_owned(),
            hhmm: "16:07".to_owned(),
            actual_unix: now + 7 * 60,
            delay_minutes: 2,
            cancelled: false,
        };
        let out = to_out(&r, now);
        assert_eq!(out.line, "S9");
        assert_eq!(out.direction, "Spandau");
        assert_eq!(out.hhmm, "16:07");
        assert_eq!(out.in_minutes, 7);
        assert_eq!(out.delay_minutes, 2);
        assert!(!out.cancelled);
    }

    #[test]
    fn filter_ands_line_and_direction_case_insensitively() {
        let f = Filter {
            lines: vec!["s9".to_owned()],
            directions: vec!["spandau".to_owned()],
        };
        assert!(f.matches(&row("S9", "S+U Spandau Bhf")));
        assert!(!f.matches(&row("S9", "Flughafen BER")));
        assert!(!f.matches(&row("S46", "Spandau")));
        // An empty filter allows everything.
        assert!(Filter::default().matches(&row("Bus 164", "Anywhere")));
    }

    #[test]
    fn config_reads_first_place_station_and_filter() {
        let toml = "\
            [[place]]\n\
            name = \"Home\"\n\
            lat = 52.4\n\
            station = \"900180001\"\n\
            walk_minutes = 10\n\
            lines = [\"S8\", \"\", \"S9\"]\n\
            directions = [\"Spandau\"]\n\
            [[place]]\n\
            name = \"Office\"\n\
            station = \"999\"\n";
        let cfg = parse_station_config(toml)
            .expect("parses")
            .expect("has station");
        assert_eq!(cfg.station, "900180001", "first place wins");
        assert_eq!(
            cfg.filter.lines,
            ["S8", "S9"],
            "blank filter entries dropped"
        );
        assert_eq!(cfg.filter.directions, ["Spandau"]);
    }

    #[test]
    fn config_none_without_place_or_station_and_err_on_garbage() {
        assert!(parse_station_config("").unwrap().is_none());
        assert!(
            parse_station_config("[[place]]\nname=\"x\"\n")
                .unwrap()
                .is_none()
        );
        assert!(
            parse_station_config("[[place]]\nstation=\"  \"\n")
                .unwrap()
                .is_none()
        );
        assert!(parse_station_config("[[place]]\nstation =").is_err());
    }
}
