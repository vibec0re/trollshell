//! Shared "places" model + the current-place resolver.
//!
//! A *place* is somewhere you frequent (home, office): coordinates, a Wi-Fi
//! fingerprint (the set of network SSIDs you see there), and optional transit
//! config (a station + line/direction filter for [`crate::departures`]).
//! Places load from `~/.config/trollshell/places.toml`; a documented default
//! is written on first run. The file's mtime is polled, so saved edits are
//! picked up live — the resolver re-resolves on change, no restart needed.
//!
//! The resolver fuses sensors into "where am I", in priority order:
//!   1. **Wi-Fi fingerprint** — at least `match_min` of a place's SSIDs are
//!      currently visible (via [`crate::wifiscan`]). Definite, precise, no
//!      network calls. Keyed on SSID not BSSID so it survives router swaps; the
//!      neighbouring networks discriminate between places even when your own
//!      SSID is deployed at all of them.
//!   2. **`GeoClue2` / `beaconDB`** raw fix — nearest place within `radius_km`.
//!   3. **Away** — no place matches; weather uses the raw `GeoClue` fix and
//!      departures shows the nearest station. Before the first fix at startup
//!      the first `[[place]]` is used as a provisional home.
//!
//! Publishes [`current_place`] (for departures: station + filter + coords) and
//! [`current`] (for weather: place coords + name when matched, else the raw
//! `GeoClue` passthrough), each with a cross-thread shared handle. Requires
//! `wifiscan::service()` and `geoclue::service()` registered first.
//!
//! Also fires the `place-changed` hook (see [`crate::hooks`]) on a genuine
//! place *transition* — deduped on the place name, not the raw resolution
//! (`GeoClue` re-resolves jitter coordinates without changing the matched
//! place). The very first resolution after startup is recorded but never
//! fires, so login stays quiet.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_reactive::{Service, registry, spawn_supervised};
use tokio::sync::Notify;

use crate::geoclue::{self, LocationSnapshot, LocationSource, LocationState};
use crate::hooks;
use crate::wifiscan::{self, AccessPoint};

// ── Config ────────────────────────────────────────────────────────────────--

const CONFIG_REL_PATH: &str = ".config/trollshell/places.toml";

/// How often the running shell re-checks `places.toml` for live reload. Each
/// tick is a single `stat` on a cached inode, so it stays snappy while you edit
/// with no measurable idle cost; the file is only re-read when the mtime moves.
const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Documented default, written on first run and used as the fallback for a
/// missing/empty/malformed config. Kept *as TOML* so the loader has one parse
/// path and the written file matches behaviour.
const DEFAULT_CONFIG: &str = r#"# trollshell places — where you frequent, how the shell recognises each, and
# (optionally) which departures to show there. Edits are picked up live: save
# the file and the shell re-resolves within a few seconds, no restart needed.
#
# Current place is resolved in order:
#   1. Wi-Fi fingerprint — at least `match_min` of a place's listed SSIDs are
#      visible. Capture them by standing there and running:
#          trollshell --scan-aps
#      then pasting the block below. (SSIDs, not BSSIDs, so it survives router
#      swaps; lean on neighbouring networks to tell your places apart.)
#   2. GeoClue2 / beaconDB location, nearest place within `radius_km`.
#   3. Otherwise "away": weather uses your raw location; departures shows the
#      nearest station. Before the first fix at startup the FIRST [[place]] is
#      used as home.
#
# Station ids: https://v6.bvg.transport.rest/locations?query=Schöneweide
#
# Moving between named places fires the `place-changed` hook — drop a script
# at ~/.config/trollshell/hooks/place-changed and it runs with
# $TROLLSHELL_PLACE (the place name) and $TROLLSHELL_PLACE_STATION (its
# station id, empty when unset). Transitions are deduped by name, and the
# very first resolution after login/startup never fires — only actual
# changes do. See docs/superpowers/specs/2026-05-05-settings-hooks-design.md.

[[place]]
name = "Schöneweide"
lat = 52.4556
lon = 13.5085

# Wi-Fi fingerprint — paste from `trollshell --scan-aps`. List a few SSIDs you
# reliably see HERE but not at your other places (usually neighbours). Empty =
# never matches (falls through to GeoClue). `match_min` = how many must be seen.
ssids = []
match_min = 2

# GeoClue fallback radius (km) when no fingerprint matches. Generous because
# GeoClue is city-level; once you've captured a fingerprint it's moot.
radius_km = 12.0

# Departures here (optional): station id + "toward the centre" filter. Omit a
# filter axis to allow everything on it.
station = "900180001"

# Walk time from here to the platform, in minutes. With this set, the list
# shows a leave-by countdown ("leave 7 min") instead of the raw departs-in
# time, and fades trains you can no longer make. 0 (the default) keeps the
# plain "departs in" label.
walk_minutes = 10

lines = ["S8", "S85", "S9"]
directions = ["Spandau", "Birkenwerder", "Hohen Neuendorf", "Waidmannslust"]
"#;

/// A configured place: location identity, Wi-Fi fingerprint, and optional
/// transit config.
#[derive(Clone, Debug, PartialEq)]
struct Place {
    name: String,
    lat: f64,
    lon: f64,
    radius_km: f64,
    /// Network SSIDs forming this place's fingerprint (matched verbatim).
    ssids: Vec<String>,
    /// How many of `ssids` must be visible to call it a match.
    match_min: usize,
    station: Option<String>,
    /// Walking minutes from here to the platform; drives departures'
    /// leave-by countdown. `0` = no walk budget (plain departs-in label).
    walk_minutes: u32,
    lines: Vec<String>,
    directions: Vec<String>,
}

impl Place {
    fn resolved(&self) -> ResolvedPlace {
        ResolvedPlace {
            name: self.name.clone(),
            lat: self.lat,
            lon: self.lon,
            station: self.station.clone(),
            walk_minutes: self.walk_minutes,
            lines: self.lines.clone(),
            directions: self.directions.clone(),
        }
    }
}

/// The resolved current place, observed by consumers. In the "away" case this
/// carries the raw location coordinates with `station: None` (departures then
/// looks up the nearest station).
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedPlace {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    /// Transit station id when this place has one; `None` means "away / look
    /// up the nearest station for `(lat, lon)`".
    pub station: Option<String>,
    /// Walking minutes from here to the platform; `0` = no walk budget. The
    /// departures widget renders a leave-by countdown when this is positive.
    pub walk_minutes: u32,
    /// Allowed line names (case-insensitive). Empty = all lines.
    pub lines: Vec<String>,
    /// Allowed destination substrings (case-insensitive). Empty = all.
    pub directions: Vec<String>,
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
    #[serde(default)]
    ssids: Vec<String>,
    #[serde(default = "default_match_min")]
    match_min: usize,
    #[serde(default)]
    station: Option<String>,
    #[serde(default)]
    walk_minutes: u32,
    #[serde(default)]
    lines: Vec<String>,
    #[serde(default)]
    directions: Vec<String>,
}

fn default_radius_km() -> f64 {
    12.0
}

fn default_match_min() -> usize {
    2
}

fn config_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(CONFIG_REL_PATH))
}

/// Drop empty/whitespace-only entries (a stray `""` would otherwise become an
/// accidental allow-all, since an empty needle is a substring of everything).
fn nonblank(items: Vec<String>) -> Vec<String> {
    items.into_iter().filter(|s| !s.trim().is_empty()).collect()
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
            // SSIDs are matched verbatim (case-sensitive); just drop blanks.
            ssids: nonblank(p.ssids),
            match_min: p.match_min,
            station: p.station,
            walk_minutes: p.walk_minutes,
            lines: nonblank(p.lines),
            directions: nonblank(p.directions),
        })
        .collect())
}

/// Load places, writing the documented default on first run. Returns the
/// default for a missing/empty/malformed user config. (The built-in default is
/// parse-tested, so in practice this is non-empty; if a malformed
/// `DEFAULT_CONFIG` ever shipped it degrades to an empty list — logged loudly —
/// rather than crashing the whole shell on cold start.)
fn load_places() -> Vec<Place> {
    let default = || {
        parse_places(DEFAULT_CONFIG).unwrap_or_else(|e| {
            tracing::error!(error = %e, "built-in default places config failed to parse");
            Vec::new()
        })
    };
    let Some(path) = config_path() else {
        return default();
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No places.toml yet. One-time: carry forward a pre-rename
            // departures.toml if present; otherwise write the documented
            // default so the schema is discoverable.
            if let Some(migrated) = migrate_legacy_departures(&path) {
                return migrated;
            }
            write_default_config(&path);
            return default();
        }
        Err(e) => {
            // Exists but unreadable (permissions, non-UTF-8, …): use the default
            // but DON'T overwrite — the bytes may be a config we just can't read.
            tracing::warn!(error = %e, path = %path.display(), "places: config unreadable; using built-in default (not overwriting)");
            return default();
        }
    };
    match parse_places(&text) {
        Ok(places) if !places.is_empty() => places,
        Ok(_) => {
            tracing::warn!(path = %path.display(), "places: config has no [[place]]; using default");
            default()
        }
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "places: config parse failed; using default");
            default()
        }
    }
}

fn write_default_config(path: &Path) {
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(error = %e, path = %parent.display(), "places: mkdir for default config failed");
        return;
    }
    match std::fs::write(path, DEFAULT_CONFIG) {
        Ok(()) => tracing::info!(path = %path.display(), "places: wrote default config"),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "places: writing default config failed");
        }
    }
}

/// One-time migration of the pre-rename `departures.toml` (whose schema is a
/// forward-compatible subset of `places.toml`). When `places.toml` is absent
/// but a parseable `departures.toml` exists, rename it forward so the user's
/// station/lines/directions survive the rename. Returns its places on success.
fn migrate_legacy_departures(places_path: &Path) -> Option<Vec<Place>> {
    let home = std::env::var("HOME").ok()?;
    let legacy = PathBuf::from(home).join(".config/trollshell/departures.toml");
    let text = std::fs::read_to_string(&legacy).ok()?;
    let places = parse_places(&text).ok()?;
    if places.is_empty() {
        return None;
    }
    match std::fs::rename(&legacy, places_path) {
        Ok(()) => {
            tracing::info!(from = %legacy.display(), to = %places_path.display(), "places: migrated departures.toml → places.toml");
        }
        Err(e) => {
            tracing::warn!(error = %e, "places: parsed legacy departures.toml but rename failed; using it as-is");
        }
    }
    Some(places)
}

/// Warn (once, at load) about places whose `match_min` exceeds their number of
/// listed `ssids` — an unsatisfiable fingerprint that silently never matches.
fn warn_unsatisfiable_fingerprints(places: &[Place]) {
    for p in places {
        if !p.ssids.is_empty() && p.match_min > p.ssids.len() {
            tracing::warn!(
                place = %p.name,
                match_min = p.match_min,
                ssids = p.ssids.len(),
                "places: match_min exceeds listed ssids; this fingerprint can never match (falling back to GeoClue radius)"
            );
        }
    }
}

/// File modification time, or `None` when it can't be stat'd (missing or
/// unreadable). [`ConfigWatcher`] compares this across polls to detect edits.
fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Watches `places.toml` for live reload by polling its mtime. Remembers the
/// last-seen mtime so a poll only re-reads the file when it actually moved, and
/// content-checks the reparse so a `touch` or no-op save doesn't churn a
/// re-resolve.
struct ConfigWatcher {
    path: Option<PathBuf>,
    last: Option<SystemTime>,
}

impl ConfigWatcher {
    fn new() -> Self {
        let path = config_path();
        let last = path.as_deref().and_then(mtime);
        Self { path, last }
    }

    /// Reload and return the fresh places when the file's mtime has moved since
    /// the previous poll *and* the parsed list differs from `current`; otherwise
    /// `None` (unchanged mtime, no config path, or an identical reparse).
    fn poll(&mut self, current: &[Place]) -> Option<Vec<Place>> {
        let path = self.path.as_deref()?;
        let now = mtime(path);
        if now == self.last {
            return None;
        }
        self.last = now;
        let reloaded = load_places();
        (reloaded.as_slice() != current).then_some(reloaded)
    }
}

// ── Resolution ──────────────────────────────────────────────────────────────

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

/// The place whose fingerprint overlaps the currently-visible SSIDs the most,
/// provided the overlap meets its `match_min` (and at least one). Places with
/// no fingerprint never match here.
fn fingerprint_match<'a>(places: &'a [Place], visible: &HashSet<String>) -> Option<&'a Place> {
    places
        .iter()
        .filter(|p| !p.ssids.is_empty())
        .map(|p| (p, p.ssids.iter().filter(|s| visible.contains(*s)).count()))
        .filter(|(p, overlap)| *overlap >= p.match_min.max(1))
        // `max_by_key` keeps the LAST of equal-overlap places; a tie needs
        // overlapping SSID sets across places, and is deterministic anyway.
        .max_by_key(|(_, overlap)| *overlap)
        .map(|(p, _)| p)
}

/// A place-anchored location: exact coords + the place name, so weather skips
/// reverse-geocoding (the name is the `label_hint`).
fn place_location(p: &Place) -> LocationState {
    LocationState::Resolved(LocationSnapshot {
        lat: p.lat,
        lon: p.lon,
        label_hint: Some(p.name.clone()),
        source: LocationSource::Configured,
    })
}

/// Resolve the current place + effective location from the sensors. Pure, so
/// the priority rules are unit-testable. Returns the place to show (always
/// some place once `places` is non-empty — the "away" case is a place with
/// `station: None` carrying the raw coordinates) and the location weather
/// should use.
fn resolve(
    places: &[Place],
    aps: &[AccessPoint],
    geoloc: &LocationState,
) -> (Option<ResolvedPlace>, LocationState) {
    let visible: HashSet<String> = aps.iter().map(|a| a.ssid.clone()).collect();

    // 1. Wi-Fi fingerprint — definite, beats everything.
    if let Some(p) = fingerprint_match(places, &visible) {
        return (Some(p.resolved()), place_location(p));
    }

    match geoloc {
        LocationState::Resolved(snap) => {
            // 2. Nearest configured place within its radius.
            if let Some(p) = nearest_place(places, snap.lat, snap.lon) {
                (Some(p.resolved()), place_location(p))
            } else {
                // 3. Away: keep the raw fix for weather; departures looks up
                //    the nearest station for these coordinates.
                let away = ResolvedPlace {
                    name: snap
                        .label_hint
                        .clone()
                        .unwrap_or_else(|| "Nearby".to_string()),
                    lat: snap.lat,
                    lon: snap.lon,
                    station: None,
                    walk_minutes: 0,
                    lines: Vec::new(),
                    directions: Vec::new(),
                };
                (Some(away), LocationState::Resolved(snap.clone()))
            }
        }
        // No fix yet (or none available): give departures a provisional home
        // (first place), but pass the raw GeoClue state THROUGH for weather —
        // so it still shows "loading" while resolving and the actionable
        // "enable GeoClue / set $TROLLSHELL_WEATHER_CITY" error when GeoClue is
        // unavailable, rather than faking a fix at the home coordinates.
        LocationState::Resolving | LocationState::Unavailable => {
            (places.first().map(Place::resolved), geoloc.clone())
        }
    }
}

// ── Service ───────────────────────────────────────────────────────────────--

#[doc(hidden)]
#[derive(Default)]
pub struct PlacesHandles {
    pub(crate) place: Mutable<Option<ResolvedPlace>>,
    pub(crate) location: Mutable<LocationState>,
}

// Cross-thread shared handles for the tokio-side consumers (departures,
// weather), mirroring the geoclue pattern.
struct Shared {
    place: Mutable<Option<ResolvedPlace>>,
    location: Mutable<LocationState>,
}
static SHARED: OnceLock<Shared> = OnceLock::new();

pub struct PlacesService;

impl Service for PlacesService {
    type Handles = PlacesHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = PlacesHandles::default();
        let place = handles.place.clone();
        let location = handles.location.clone();
        let _ = SHARED.set(Shared {
            place: place.clone(),
            location: location.clone(),
        });
        let loaded = load_places();
        warn_unsatisfiable_fingerprints(&loaded);
        let places = Mutable::new(Arc::new(loaded));
        spawn_supervised("places", {
            let places = places.clone();
            move || watch_config(places.clone())
        });
        spawn_supervised("places", move || {
            resolve_loop(place.clone(), location.clone(), places.clone())
        });
        handles
    }
}

#[must_use]
pub fn service() -> PlacesService {
    PlacesService
}

/// Signal of the resolved current place (for departures). `None` only before
/// the first resolution.
pub fn current_place() -> impl Signal<Item = Option<ResolvedPlace>> {
    registry::with(|r| {
        r.get::<PlacesHandles>()
            .expect("places::service() not registered")
            .place
            .signal_cloned()
    })
}

/// Process-global clone of the current-place handle, for tokio-side readers.
#[must_use]
pub fn shared_place() -> Option<Mutable<Option<ResolvedPlace>>> {
    SHARED.get().map(|s| s.place.clone())
}

/// Signal of the effective location (for weather): place coords + name when a
/// place is matched, else the raw `GeoClue` passthrough.
pub fn current() -> impl Signal<Item = LocationState> {
    registry::with(|r| {
        r.get::<PlacesHandles>()
            .expect("places::service() not registered")
            .location
            .signal_cloned()
    })
}

/// Process-global clone of the effective-location handle, for tokio-side
/// readers (weather).
#[must_use]
pub fn shared_location() -> Option<Mutable<LocationState>> {
    SHARED.get().map(|s| s.location.clone())
}

/// Poll `places.toml` and republish the parsed list when it changes, so config
/// edits take effect within [`CONFIG_POLL_INTERVAL`] without restarting the
/// shell. [`resolve_loop`] subscribes to the same handle and re-resolves on
/// each swap, exactly as it does for the Wi-Fi and `GeoClue` sensors.
async fn watch_config(places: Mutable<Arc<Vec<Place>>>) {
    let mut watcher = ConfigWatcher::new();
    let mut tick = tokio::time::interval(CONFIG_POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let current = places.get_cloned();
        if let Some(reloaded) = watcher.poll(&current) {
            warn_unsatisfiable_fingerprints(&reloaded);
            tracing::info!(count = reloaded.len(), "places: config changed; reloaded");
            places.set(Arc::new(reloaded));
        }
    }
}

/// Decide whether a freshly resolved `place` should fire the `place-changed`
/// hook, given whether we've resolved at least once before (`resolved_once`)
/// and the name last fired for (`last_fired`). Both are updated in place.
///
/// Dedups on the place **name** (identity), not the full [`ResolvedPlace`] —
/// `GeoClue` re-resolves emit a fresh struct (coordinates move) on every
/// jitter even when the matched place hasn't changed, which would otherwise
/// spam the hook on every sensor tick (#235). The very first resolution
/// (e.g. at login) seeds the bookkeeping but never fires, so a freshly
/// started shell stays quiet until an actual transition happens.
///
/// Returns the place to fire the hook for on a genuine transition, else
/// `None` (first resolution, no change, or the transition is *into* "no
/// place" — there's no name to report).
fn place_transition<'a>(
    place: Option<&'a ResolvedPlace>,
    resolved_once: &mut bool,
    last_fired: &mut Option<String>,
) -> Option<&'a ResolvedPlace> {
    let name = place.map(|p| p.name.clone());
    let is_first = !*resolved_once;
    *resolved_once = true;
    let transitioned = !is_first && name != *last_fired;
    *last_fired = name;
    if transitioned { place } else { None }
}

async fn resolve_loop(
    place_out: Mutable<Option<ResolvedPlace>>,
    location_out: Mutable<LocationState>,
    places: Mutable<Arc<Vec<Place>>>,
) {
    let aps = wifiscan::shared_aps();
    let geo = geoclue::shared_location();

    // Re-resolve whenever either sensor changes. Each `signal_ref` also emits
    // its current value immediately, so this fires at boot.
    let notify = Arc::new(Notify::new());
    if let Some(m) = aps.clone() {
        let n = notify.clone();
        tokio::spawn(async move {
            m.signal_ref(|_| ())
                .for_each(move |()| {
                    n.notify_one();
                    std::future::ready(())
                })
                .await;
        });
    } else {
        tracing::warn!("places: wifiscan not registered; Wi-Fi fingerprinting disabled");
    }
    if let Some(m) = geo.clone() {
        let n = notify.clone();
        tokio::spawn(async move {
            m.signal_ref(|_| ())
                .for_each(move |()| {
                    n.notify_one();
                    std::future::ready(())
                })
                .await;
        });
    } else {
        tracing::warn!("places: geoclue not registered; location fallback disabled");
    }
    // Re-resolve whenever the config is reloaded (watch_config swaps the list).
    {
        let n = notify.clone();
        let p = places.clone();
        tokio::spawn(async move {
            p.signal_ref(|_| ())
                .for_each(move |()| {
                    n.notify_one();
                    std::future::ready(())
                })
                .await;
        });
    }

    let mut place_hook_resolved_once = false;
    let mut place_hook_last_fired: Option<String> = None;

    loop {
        let current = places.get_cloned();
        let ap_list = match &aps {
            Some(m) => m.get_cloned(),
            None => Vec::new(),
        };
        let geoloc = match &geo {
            Some(m) => m.get_cloned(),
            None => LocationState::Unavailable,
        };
        let (place, location) = resolve(&current, &ap_list, &geoloc);

        if let Some(p) = place_transition(
            place.as_ref(),
            &mut place_hook_resolved_once,
            &mut place_hook_last_fired,
        ) {
            hooks::run(
                "place-changed",
                &[
                    ("TROLLSHELL_PLACE", p.name.as_str()),
                    (
                        "TROLLSHELL_PLACE_STATION",
                        p.station.as_deref().unwrap_or(""),
                    ),
                ],
            );
        }

        if place_out.get_cloned() != place {
            if let Some(p) = &place {
                tracing::debug!(place = %p.name, station = ?p.station, "places: resolved");
            }
            place_out.set(place);
        }
        if location_out.get_cloned() != location {
            location_out.set(location);
        }

        notify.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ap(ssid: &str) -> AccessPoint {
        AccessPoint {
            ssid: ssid.to_string(),
            strength: 70,
        }
    }

    fn default_places() -> Vec<Place> {
        parse_places(DEFAULT_CONFIG).expect("default config parses")
    }

    fn place(name: &str, lat: f64, lon: f64, ssids: &[&str], match_min: usize) -> Place {
        Place {
            name: name.to_string(),
            lat,
            lon,
            radius_km: 12.0,
            ssids: ssids.iter().map(|s| (*s).to_string()).collect(),
            match_min,
            station: Some(format!("station-{name}")),
            walk_minutes: 0,
            lines: Vec::new(),
            directions: Vec::new(),
        }
    }

    // ── Config ─────────────────────────────────────────────────────────────

    #[test]
    fn default_config_parses() {
        let places = default_places();
        assert_eq!(places.len(), 1);
        assert_eq!(places[0].name, "Schöneweide");
        assert_eq!(places[0].station.as_deref(), Some("900180001"));
        assert_eq!(places[0].match_min, 2);
        assert!(places[0].ssids.is_empty());
        assert_eq!(places[0].walk_minutes, 10);
        assert_eq!(places[0].lines, ["S8", "S85", "S9"]);
        assert!((places[0].radius_km - 12.0).abs() < 1e-9);
    }

    #[test]
    fn walk_minutes_defaults_to_zero_and_survives_resolve() {
        // Absent in config → 0; present → carried through to ResolvedPlace.
        let toml = "\
            [[place]]\n\
            name = \"NoWalk\"\n\
            lat = 1.0\n\
            lon = 2.0\n\
            [[place]]\n\
            name = \"Walk\"\n\
            lat = 3.0\n\
            lon = 4.0\n\
            walk_minutes = 7\n";
        let places = parse_places(toml).expect("parses");
        assert_eq!(places[0].walk_minutes, 0); // omitted → default
        assert_eq!(places[1].walk_minutes, 7);
        assert_eq!(places[1].resolved().walk_minutes, 7);
    }

    #[test]
    fn parse_keeps_ssids_verbatim_and_drops_blanks() {
        let toml = "\
            [[place]]\n\
            name = \"T\"\n\
            lat = 1.0\n\
            lon = 2.0\n\
            ssids = [\"FRITZ!Box 7590\", \"\", \"  \"]\n";
        let places = parse_places(toml).expect("parses");
        assert_eq!(places[0].ssids, ["FRITZ!Box 7590"]); // case kept, blanks dropped
        assert!(places[0].station.is_none()); // optional
        assert_eq!(places[0].match_min, 2); // default
    }

    #[test]
    fn parse_malformed_is_err() {
        assert!(parse_places("[[place]]\nname = ").is_err());
    }

    // ── Fingerprint ──────────────────────────────────────────────────────────

    #[test]
    fn fingerprint_needs_match_min_overlap() {
        let places = vec![place("Home", 0.0, 0.0, &["a", "b", "c"], 2)];
        let visible: HashSet<String> = ["a".into(), "b".into()].into_iter().collect();
        assert_eq!(
            fingerprint_match(&places, &visible).map(|p| p.name.as_str()),
            Some("Home")
        );
        let one: HashSet<String> = ["a".into()].into_iter().collect();
        assert!(fingerprint_match(&places, &one).is_none()); // only 1 < match_min 2
    }

    #[test]
    fn fingerprint_empty_ssids_never_matches() {
        let places = vec![place("Home", 0.0, 0.0, &[], 1)];
        let visible: HashSet<String> = ["x".into()].into_iter().collect();
        assert!(fingerprint_match(&places, &visible).is_none());
    }

    #[test]
    fn fingerprint_picks_strongest_overlap() {
        let places = vec![
            place("Home", 0.0, 0.0, &["a", "b"], 1),
            place("Office", 0.0, 0.0, &["c", "d", "e"], 1),
        ];
        // Sees one home AP but two office APs → Office wins.
        let visible: HashSet<String> = ["a".into(), "c".into(), "d".into()].into_iter().collect();
        assert_eq!(
            fingerprint_match(&places, &visible).map(|p| p.name.as_str()),
            Some("Office"),
        );
    }

    // ── Resolve priority ──────────────────────────────────────────────────--

    fn resolved_at(lat: f64, lon: f64) -> LocationState {
        LocationState::Resolved(LocationSnapshot {
            lat,
            lon,
            label_hint: None,
            source: LocationSource::GeoClue,
        })
    }

    #[test]
    fn resolve_fingerprint_beats_geoclue() {
        let places = vec![
            place("Home", 52.0, 13.0, &["aa"], 1),
            place("Office", 48.0, 11.0, &["bb"], 1),
        ];
        // Physically the GeoClue fix is at Home's coords, but we SEE Office's AP.
        let aps = [ap("bb")];
        let (place, loc) = resolve(&places, &aps, &resolved_at(52.0, 13.0));
        assert_eq!(place.as_ref().map(|p| p.name.as_str()), Some("Office"));
        match loc {
            LocationState::Resolved(s) => assert!((s.lat - 48.0).abs() < 1e-9),
            other => panic!("expected Office coords, got {other:?}"),
        }
    }

    #[test]
    fn resolve_radius_fallback_when_no_fingerprint() {
        let places = vec![place("Home", 52.4556, 13.5085, &["aa"], 1)];
        let (place, _) = resolve(&places, &[], &resolved_at(52.46, 13.51)); // ~near home, no APs seen
        assert_eq!(place.as_ref().map(|p| p.name.as_str()), Some("Home"));
        assert_eq!(
            place.and_then(|p| p.station),
            Some("station-Home".to_string())
        );
    }

    #[test]
    fn resolve_away_has_no_station_and_keeps_raw_fix() {
        let places = vec![place("Home", 52.4556, 13.5085, &["aa"], 1)];
        // Hamburg — far outside radius, no matching APs.
        let (place, loc) = resolve(&places, &[], &resolved_at(53.5511, 9.9937));
        let place = place.expect("always some place");
        assert!(place.station.is_none(), "away → no station");
        assert!((place.lat - 53.5511).abs() < 1e-9, "carries raw coords");
        match loc {
            LocationState::Resolved(s) => assert!((s.lat - 53.5511).abs() < 1e-9),
            other => panic!("expected raw fix, got {other:?}"),
        }
    }

    #[test]
    fn resolve_no_fix_uses_home_for_place_but_passes_location_through() {
        let places = vec![
            place("Home", 52.0, 13.0, &["aa"], 1),
            place("Office", 48.0, 11.0, &["bb"], 1),
        ];
        // Departures gets provisional home; weather sees the raw GeoClue state
        // (loading while resolving, the actionable error when unavailable) —
        // never a faked fix at the home coordinates.
        for geoloc in [LocationState::Resolving, LocationState::Unavailable] {
            let (place, loc) = resolve(&places, &[], &geoloc);
            assert_eq!(place.as_ref().map(|p| p.name.as_str()), Some("Home"));
            assert_eq!(loc, geoloc);
        }
    }

    // ── place-changed hook dedup ────────────────────────────────────────────

    fn resolved(name: &str, station: Option<&str>) -> ResolvedPlace {
        ResolvedPlace {
            name: name.to_string(),
            lat: 0.0,
            lon: 0.0,
            station: station.map(str::to_string),
            walk_minutes: 0,
            lines: Vec::new(),
            directions: Vec::new(),
        }
    }

    #[test]
    fn place_transition_silent_on_first_resolution() {
        let mut resolved_once = false;
        let mut last_fired = None;
        let home = Some(resolved("Home", Some("900180001")));

        assert!(place_transition(home.as_ref(), &mut resolved_once, &mut last_fired).is_none());
        assert!(resolved_once);
        assert_eq!(last_fired.as_deref(), Some("Home"));
    }

    #[test]
    fn place_transition_dedups_on_name_not_full_struct() {
        // Same name, different coords each call (GeoClue jitter) — must not
        // re-fire after the first (silent) resolution.
        let mut resolved_once = false;
        let mut last_fired = None;
        let away_1 = Some(ResolvedPlace {
            lat: 52.1,
            ..resolved("Nearby", None)
        });
        let away_2 = Some(ResolvedPlace {
            lat: 52.2,
            ..resolved("Nearby", None)
        });

        assert!(place_transition(away_1.as_ref(), &mut resolved_once, &mut last_fired).is_none()); // first: silent
        assert!(place_transition(away_2.as_ref(), &mut resolved_once, &mut last_fired).is_none()); // same name: no fire
        assert!(place_transition(away_1.as_ref(), &mut resolved_once, &mut last_fired).is_none()); // still no fire
    }

    #[test]
    fn place_transition_fires_on_genuine_name_change() {
        let mut resolved_once = false;
        let mut last_fired = None;
        let home = Some(resolved("Home", Some("900180001")));
        let office = Some(resolved("Office", Some("900008888")));

        assert!(place_transition(home.as_ref(), &mut resolved_once, &mut last_fired).is_none()); // first: silent
        let fired = place_transition(office.as_ref(), &mut resolved_once, &mut last_fired)
            .expect("name changed → fires");
        assert_eq!(fired.name, "Office");
        assert_eq!(fired.station.as_deref(), Some("900008888"));
        assert_eq!(last_fired.as_deref(), Some("Office"));

        // Back to Home is itself a transition.
        let fired_again = place_transition(home.as_ref(), &mut resolved_once, &mut last_fired)
            .expect("transition back also fires");
        assert_eq!(fired_again.name, "Home");
    }

    #[test]
    fn place_transition_into_no_place_updates_state_but_reports_nothing() {
        let mut resolved_once = false;
        let mut last_fired = None;
        let home = Some(resolved("Home", None));
        let none: Option<ResolvedPlace> = None;

        assert!(place_transition(home.as_ref(), &mut resolved_once, &mut last_fired).is_none()); // first: silent
        // Transitioning to "no place" has no name to report, even though it's
        // a real transition in the bookkeeping.
        assert!(place_transition(none.as_ref(), &mut resolved_once, &mut last_fired).is_none());
        assert_eq!(last_fired, None);
        // Coming back to Home is a transition again (last_fired was cleared).
        let fired = place_transition(home.as_ref(), &mut resolved_once, &mut last_fired)
            .expect("re-arriving fires again");
        assert_eq!(fired.name, "Home");
    }

    // ── Live reload ──────────────────────────────────────────────────────────

    #[test]
    fn config_watcher_reloads_only_on_changed_content() {
        use std::time::UNIX_EPOCH;

        let root = std::env::temp_dir().join(format!("hytte-places-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".config/trollshell")).unwrap();

        // `temp_env` serializes $HOME mutation across tests and restores it after.
        temp_env::with_var("HOME", Some(root.as_os_str()), || {
            let cfg = root.join(".config/trollshell/places.toml");
            let one = "[[place]]\nname = \"Home\"\nlat = 1.0\nlon = 2.0\n";
            let two = "[[place]]\nname = \"Home\"\nlat = 1.0\nlon = 2.0\n\
                       [[place]]\nname = \"Office\"\nlat = 3.0\nlon = 4.0\n";
            // Set the file's mtime to a fixed instant so change-detection is
            // deterministic (no sleeps, no filesystem-granularity flakiness).
            let set_mtime = |secs: u64| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&cfg)
                    .unwrap()
                    .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
                    .unwrap();
            };

            // One place on disk; the watcher records its mtime at construction.
            std::fs::write(&cfg, one).unwrap();
            set_mtime(1);
            let mut watcher = ConfigWatcher::new();
            let current = load_places();
            assert_eq!(current.len(), 1);

            // Unchanged mtime → no reload.
            assert!(watcher.poll(&current).is_none());

            // Add a place and move the mtime → reload sees both.
            std::fs::write(&cfg, two).unwrap();
            set_mtime(2);
            let reloaded = watcher.poll(&current).expect("changed → reload");
            assert_eq!(reloaded.len(), 2);

            // mtime moves but content is identical → no spurious republish.
            set_mtime(3);
            assert!(watcher.poll(&reloaded).is_none());
        });

        let _ = std::fs::remove_dir_all(&root);
    }
}
