//! The departures feed: the visibility-gated poll task, the HAFAS
//! (`v6.bvg.transport.rest`) client, and the `places.toml` station config.
//!
//! Ported from `hytte-services::departures` (the HTTP client + wire format +
//! filter) and `hytte-services::places` (the config file). HAFAS is plain
//! HTTPS, so — unlike the native service — this needs no D-Bus: the same
//! blocking `ureq` + `spawn_blocking` idiom as the pet's brain and the native
//! fetcher (confirmed on #290's thread).
//!
//! # The gate
//!
//! [`poll_task`] is the I/O side of the visibility gate. It owns a fetch
//! interval and drains the command lane fed by the reducer's
//! [`crate::Cmd::SetVisible`]. While hidden it parks (no ticks, no HTTP); on a
//! hidden→visible edge ([`on_visibility`]) it fires an immediate refresh — the
//! native board's open-edge poll — then re-polls every [`REFRESH_WHILE_OPEN`].
//!
//! # Station config
//!
//! [`load_station_config`] reads the **first `[[place]]`** of
//! `~/.config/trollshell/places.toml` (the exact path + schema the native
//! `places` service owns and writes a documented default for). Re-read on every
//! fetch, so an edit saved while the board is open is picked up on the next
//! poll — the live-reload the native service does via mtime polling. Without
//! D-Bus this plugin can't run the native Wi-Fi/`GeoClue` place *resolution*, so
//! it always shows that first place (home) — matching the provisional-home the
//! native resolver falls back to before its first sensor fix.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use hytte_plugin::CmdReceiver;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::{BoardMsg, Cmd};

// ── Tunables ─────────────────────────────────────────────────────────────────

/// While the mount surface is visible, re-poll on this cadence. Mirrors the
/// native sidebar's `REFRESH_WHILE_OPEN` (`overlays/sidebar.rs`, 30 s); while
/// hidden the poller parks entirely (the shell's own poller is likewise a
/// no-op while closed).
pub(crate) const REFRESH_WHILE_OPEN: Duration = Duration::from_secs(30);

/// How many departures to request. Larger than the display count so a
/// direction/line filter still has enough rows to fill the list. Native
/// `FETCH_COUNT`.
const FETCH_COUNT: usize = 30;
/// How many departures to display after filtering. Native `DISPLAY_COUNT`.
const DISPLAY_COUNT: usize = 8;

const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the station config lives, relative to `$HOME`. Same file the native
/// `places` service reads/writes.
const CONFIG_REL_PATH: &str = ".config/trollshell/places.toml";

// ── Row model ────────────────────────────────────────────────────────────────

/// One upcoming S-Bahn departure, ready to render. The GTK-free, unix-seconds
/// analogue of the native `Departure`: `actual_unix` for the relative/leave-by
/// math, `hhmm` pre-formatted from the departure's own RFC 3339 offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    /// Line label, e.g. `"S9"`.
    pub line: String,
    /// Destination string, e.g. `"Spandau"`.
    pub direction: String,
    /// Local `HH:MM` of the actual departure (its own offset, not the machine's).
    pub hhmm: String,
    /// Actual departure as unix seconds — the reference for the leave-by math.
    pub actual_unix: i64,
    /// Lateness in minutes; `0` on time, negative if early.
    pub delay_minutes: i64,
    /// `true` for explicitly cancelled rows.
    pub cancelled: bool,
    /// HAFAS trip id, stable across refreshes for a given run. Kept for #236's
    /// arm-a-train, which anchors an armed trip on it.
    pub trip_id: String,
    /// Walk budget (minutes) to the platform, stamped from config. `0` = plain
    /// departs-in label; positive turns the row into a leave-by countdown.
    pub walk_minutes: u32,
}

// ── Filter ───────────────────────────────────────────────────────────────────

/// Which departures to keep. An empty axis allows everything on it; a departure
/// must pass both axes. Ported verbatim from the native `Filter`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Filter {
    /// Allowed line names, matched case-insensitively. Empty = allow every line.
    lines: Vec<String>,
    /// Allowed destination substrings, matched case-insensitively. Empty = all.
    directions: Vec<String>,
}

impl Filter {
    /// Line match is exact (case-insensitive); direction match is a
    /// case-insensitive substring so `"Spandau"` matches `"S+U Spandau Bhf"`.
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

// ── Wire format ──────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
struct ApiResponse {
    #[serde(default)]
    departures: Vec<ApiDeparture>,
}

#[derive(Deserialize, Debug)]
struct ApiDeparture {
    #[serde(default, rename = "tripId")]
    trip_id: String,
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

/// Convert one wire row into a [`Row`], dropping rows we can't render: non-
/// suburban products, rows already departed (> 60 s past, grace for skew), and
/// rows whose timestamps fail to parse. Ports the native `into_departure`;
/// `walk_minutes` is stamped later from config. `now_unix` is the fetch instant.
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
    // Require a parseable plannedWhen, same as the native filter.
    if DateTime::parse_from_rfc3339(planned_raw).is_err() {
        return None;
    }
    let actual_raw = row.when.as_deref().unwrap_or(planned_raw);
    let actual = DateTime::parse_from_rfc3339(actual_raw).ok()?;
    let actual_unix = actual.timestamp();

    // Drop departures more than 60 s in the past.
    if actual_unix < now_unix - 60 {
        return None;
    }

    // Integer division truncates toward zero; sub-minute precision isn't shown.
    let delay_minutes = row.delay.unwrap_or(0) / 60;

    Some(Row {
        line: line_name,
        direction: row.direction.unwrap_or_default(),
        hhmm: actual.format("%H:%M").to_string(),
        actual_unix,
        delay_minutes,
        cancelled: row.cancelled,
        trip_id: row.trip_id,
        walk_minutes: 0,
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

// ── Station config (ported subset of places.toml) ────────────────────────────

/// The station the board fetches for, plus its filter + walk budget.
#[derive(Debug)]
struct StationConfig {
    station: String,
    walk_minutes: u32,
    filter: Filter,
}

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    place: Vec<PlaceCfg>,
}

/// A forward-compatible subset of the native `places.toml` `[[place]]` — only
/// the departures-relevant fields; the resolver's `lat`/`lon`/`ssids`/… are
/// simply ignored (serde skips unknown fields).
#[derive(Deserialize)]
struct PlaceCfg {
    #[serde(default)]
    station: Option<String>,
    #[serde(default)]
    walk_minutes: u32,
    #[serde(default)]
    lines: Vec<String>,
    #[serde(default)]
    directions: Vec<String>,
}

/// Drop empty/whitespace-only entries (a stray `""` would be an accidental
/// allow-all, since an empty needle is a substring of everything). Ported from
/// the native `places::nonblank`.
fn nonblank(items: Vec<String>) -> Vec<String> {
    items.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

fn config_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(CONFIG_REL_PATH))
}

/// Parse the first place's station config out of a `places.toml` body. Pure, so
/// the schema port is unit-testable. `Ok(None)` = no place, or the first place
/// has no (non-blank) `station`.
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
        walk_minutes: first.walk_minutes,
        filter: Filter {
            lines: nonblank(first.lines),
            directions: nonblank(first.directions),
        },
    }))
}

/// Load the station config from disk. `Err` carries an actionable, prefix-free
/// message (rendered plainly, not under "can't reach BVG") when there's no
/// file/place/station; a real read error is surfaced as-is.
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

// ── Fetch ────────────────────────────────────────────────────────────────────

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

/// One full refresh: (re)load config, fetch, filter, stamp the walk budget, and
/// cap to the display count. Blocking — runs on a `spawn_blocking` thread.
/// Re-reading config here is the live-reload seam.
fn fetch_once() -> Result<Vec<Row>, String> {
    let cfg = load_station_config()?;
    let agent = http_agent();
    let all = fetch_departures(&agent, &cfg.station)?;
    let walk = cfg.walk_minutes;
    Ok(all
        .into_iter()
        .filter(|r| cfg.filter.matches(r))
        .take(DISPLAY_COUNT)
        .map(|r| Row {
            walk_minutes: walk,
            ..r
        })
        .collect())
}

// ── The visibility-gated poll task ───────────────────────────────────────────

/// The visibility-gate transition: given the current visible state and a
/// requested one, return the next state and whether an **immediate** refresh is
/// owed (a hidden→visible edge — the native board's open-edge poll). Pure, so
/// the gate is unit-testable without a runtime or a live fetch.
fn on_visibility(current: bool, requested: bool) -> (bool, bool) {
    let refresh_now = requested && !current;
    (requested, refresh_now)
}

/// Run a `spawn_blocking` fetch and forward the result as a [`BoardMsg`].
async fn fetch_and_send(msg_tx: &mpsc::UnboundedSender<BoardMsg>) {
    let result = tokio::task::spawn_blocking(fetch_once)
        .await
        .unwrap_or_else(|e| Err(format!("join: {e}")));
    if let Err(ref e) = result {
        eprintln!("[departures] fetch failed: {e}");
    }
    let _ = msg_tx.send(BoardMsg::Fetched(result));
}

/// The I/O side of the visibility gate (the `sources()` task). Owns the fetch
/// interval and drains the command lane: it parks while hidden, does an
/// immediate refresh on becoming visible, then re-polls every
/// [`REFRESH_WHILE_OPEN`] until hidden. Exits when the lane closes (the session
/// is tearing down).
pub(crate) async fn poll_task(mut cmds: CmdReceiver<Cmd>, msg_tx: mpsc::UnboundedSender<BoardMsg>) {
    let mut visible = false;
    let mut interval = tokio::time::interval(REFRESH_WHILE_OPEN);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            // Prefer visibility changes over interval ticks, so a close parks
            // the poller promptly rather than firing one more fetch first.
            biased;
            cmd = cmds.recv() => {
                let Some(Cmd::SetVisible(requested)) = cmd else {
                    return; // lane closed → session teardown
                };
                let (next_visible, refresh_now) = on_visibility(visible, requested);
                visible = next_visible;
                if refresh_now {
                    // Reset so the next scheduled tick lands a clean interval
                    // after this immediate refresh, not off the interval's
                    // already-elapsed first tick.
                    interval.reset();
                    fetch_and_send(&msg_tx).await;
                }
            }
            // Disabled while hidden — the poller parks (no ticks, no HTTP).
            _ = interval.tick(), if visible => {
                fetch_and_send(&msg_tx).await;
            }
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

    /// Before every fixture row (all 16:42–16:50+01:00).
    fn now_before() -> i64 {
        ts("2030-01-01T16:00:00+01:00")
    }

    /// After every fixture row.
    fn now_after() -> i64 {
        ts("2030-01-01T18:00:00+01:00")
    }

    const FIXTURE: &str = include_str!("../tests/fixtures/departures-schoeneweide.json");

    fn nth_wire_row(n: usize) -> ApiDeparture {
        let api: ApiResponse = serde_json::from_str(FIXTURE).expect("fixture parses");
        api.departures.into_iter().nth(n).expect("row exists")
    }

    fn row(line: &str, direction: &str) -> Row {
        Row {
            line: line.to_owned(),
            direction: direction.to_owned(),
            hhmm: "16:00".to_owned(),
            actual_unix: now_before(),
            delay_minutes: 0,
            cancelled: false,
            trip_id: "t".to_owned(),
            walk_minutes: 0,
        }
    }

    // ── into_row (ported from the native into_departure tests) ────────────────

    #[test]
    fn into_row_keeps_normal_row() {
        let r = into_row(nth_wire_row(0), now_before()).expect("kept");
        assert_eq!(r.line, "S9");
        assert_eq!(r.direction, "Spandau");
        assert_eq!(r.delay_minutes, 0);
        assert!(!r.cancelled);
        assert_eq!(r.trip_id, "trip-1-ontime");
        assert_eq!(r.hhmm, "16:42");
        assert_eq!(r.actual_unix, ts("2030-01-01T16:42:00+01:00"));
    }

    #[test]
    fn into_row_keeps_delayed_row() {
        let r = into_row(nth_wire_row(1), now_before()).expect("kept");
        assert_eq!(r.line, "S46");
        assert_eq!(r.delay_minutes, 5);
        // `when` (16:49) is planned (16:44) + 5 min; hhmm follows `when`.
        assert_eq!(r.hhmm, "16:49");
        assert_eq!(r.actual_unix, ts("2030-01-01T16:49:00+01:00"));
    }

    #[test]
    fn into_row_keeps_cancelled_row_with_planned_time() {
        let r = into_row(nth_wire_row(2), now_before()).expect("kept");
        assert!(r.cancelled);
        // `when` is null on cancelled → fall back to plannedWhen (16:49).
        assert_eq!(r.hhmm, "16:49");
        assert_eq!(r.actual_unix, ts("2030-01-01T16:49:00+01:00"));
    }

    #[test]
    fn into_row_drops_non_suburban() {
        assert!(into_row(nth_wire_row(3), now_before()).is_none());
    }

    #[test]
    fn into_row_drops_already_departed() {
        assert!(into_row(nth_wire_row(0), now_after()).is_none());
    }

    // ── parse_response ────────────────────────────────────────────────────────

    #[test]
    fn parse_response_drops_bus_keeps_three_suburban_in_order() {
        let parsed = parse_response(FIXTURE, now_before()).expect("parses");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].line, "S9");
        assert_eq!(parsed[1].line, "S46");
        assert_eq!(parsed[2].line, "S8");
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

    // ── Filter (ported from the native filter tests) ──────────────────────────

    #[test]
    fn filter_empty_allows_everything() {
        let f = Filter::default();
        assert!(f.matches(&row("S9", "Flughafen BER")));
        assert!(f.matches(&row("Bus 164", "Anywhere")));
    }

    #[test]
    fn filter_line_and_direction_are_anded() {
        let f = Filter {
            lines: vec!["S8".to_owned(), "S85".to_owned(), "S9".to_owned()],
            directions: vec!["Spandau".to_owned(), "Birkenwerder".to_owned()],
        };
        assert!(f.matches(&row("S9", "Spandau")));
        assert!(!f.matches(&row("S9", "Flughafen BER"))); // wrong direction
        assert!(!f.matches(&row("S46", "Spandau"))); // wrong line
    }

    #[test]
    fn filter_matches_case_insensitively_and_by_substring() {
        let f = Filter {
            lines: vec!["s8".to_owned()],
            directions: vec!["birkenwerder".to_owned()],
        };
        assert!(f.matches(&row("S8", "S+U Birkenwerder Bhf")));
    }

    #[test]
    fn filter_directions_only_ignores_line() {
        let f = Filter {
            lines: Vec::new(),
            directions: vec!["Spandau".to_owned()],
        };
        assert!(f.matches(&row("S9", "Spandau")));
        assert!(!f.matches(&row("S9", "Wildau")));
    }

    // ── Station config (ported subset of the native places schema) ────────────

    /// A realistic `places.toml` fragment in the native schema, including the
    /// resolver-only fields (`lat`/`lon`/`ssids`/…) this plugin ignores.
    const PLACES_TOML: &str = "\
        [[place]]\n\
        name = \"Schöneweide\"\n\
        lat = 52.4556\n\
        lon = 13.5085\n\
        ssids = []\n\
        match_min = 2\n\
        radius_km = 12.0\n\
        station = \"900180001\"\n\
        walk_minutes = 10\n\
        lines = [\"S8\", \"S85\", \"S9\"]\n\
        directions = [\"Spandau\", \"Birkenwerder\"]\n";

    #[test]
    fn config_reads_the_first_places_station_and_filter() {
        let cfg = parse_station_config(PLACES_TOML)
            .expect("parses")
            .expect("has a station");
        assert_eq!(cfg.station, "900180001");
        assert_eq!(cfg.walk_minutes, 10);
        assert_eq!(cfg.filter.lines, ["S8", "S85", "S9"]);
        assert_eq!(cfg.filter.directions, ["Spandau", "Birkenwerder"]);
    }

    #[test]
    fn config_uses_the_first_place_and_drops_blank_filter_entries() {
        let toml = "\
            [[place]]\n\
            name = \"Home\"\n\
            station = \"111\"\n\
            lines = [\"S1\", \"\", \"  \"]\n\
            [[place]]\n\
            name = \"Office\"\n\
            station = \"999\"\n";
        let cfg = parse_station_config(toml).unwrap().unwrap();
        assert_eq!(cfg.station, "111", "the first place wins");
        assert_eq!(cfg.filter.lines, ["S1"], "blank filter entries dropped");
    }

    #[test]
    fn config_none_when_no_place_or_no_station() {
        assert!(parse_station_config("").unwrap().is_none());
        let no_station = "[[place]]\nname = \"Home\"\nlat = 1.0\nlon = 2.0\n";
        assert!(parse_station_config(no_station).unwrap().is_none());
        // A blank/whitespace station is treated as unset.
        let blank = "[[place]]\nname = \"Home\"\nstation = \"  \"\n";
        assert!(parse_station_config(blank).unwrap().is_none());
    }

    #[test]
    fn config_malformed_is_err() {
        let err = parse_station_config("[[place]]\nstation = ").unwrap_err();
        assert!(err.starts_with("config:"), "got: {err}");
    }

    // ── The visibility gate ───────────────────────────────────────────────────

    #[test]
    fn on_visibility_refreshes_only_on_the_hidden_to_visible_edge() {
        // Open → become visible AND owe an immediate refresh.
        assert_eq!(on_visibility(false, true), (true, true));
        // Already visible, a redundant push → no extra refresh.
        assert_eq!(on_visibility(true, true), (true, false));
        // Close → park, no refresh.
        assert_eq!(on_visibility(true, false), (false, false));
        // Stay hidden → nothing.
        assert_eq!(on_visibility(false, false), (false, false));
    }
}
