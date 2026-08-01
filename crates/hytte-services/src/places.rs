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
//!
//! # Editing (#640 / #703)
//!
//! The set is also *writable*: [`add_place`], [`update_place`],
//! [`rename_place`], [`remove_place`] and [`save_places`] validate an edit,
//! render the whole set back to `places.toml` **atomically** (scratch file in
//! the same directory + `rename(2)`, so an interrupted save can never truncate
//! the config), and then publish the new set on the same handle the live
//! reload uses — so departures, weather and place detection pick the edit up
//! immediately, with no shell restart and no wait for the mtime poll. Nothing
//! on this path panics on bad input; every rejection is a [`PlacesError`].
//!
//! **Known limitation:** serialising is a *re-render*, not a textual patch. The
//! file's leading comment block (the documented preamble written on first run)
//! is carried forward verbatim, but comments attached to individual keys — and
//! any hand-chosen key ordering inside a `[[place]]` — are lost on the first
//! programmatic save. Preserving those needs a format-preserving parser
//! (`toml_edit`), which is not a workspace dependency today.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_reactive::{Service, registry, shared, spawn_supervised};
use tokio::sync::Notify;

use crate::config_file;
use crate::geoclue::{self, LocationSnapshot, LocationSource, LocationState};
use crate::hooks;
use crate::wifiscan::{self, AccessPoint};

// ── Config ────────────────────────────────────────────────────────────────--

/// Config file under `~/.config/trollshell/`.
const CONFIG_FILE: &str = "places.toml";

/// How often the running shell re-checks `places.toml` for live reload on AC
/// power. Each tick is a single `stat` on a cached inode, so it stays snappy
/// while you edit with no measurable idle cost; the file is only re-read when
/// the mtime moves.
const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Re-check cadence on battery power: 3x AC (#505). The `stat` is nearly free
/// either way, but it's still a wakeup, and config edits are rare enough that
/// a slower catch-up on battery goes unnoticed.
const BATTERY_CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(9);

/// How often [`wait_cadence`] re-checks the target cadence against elapsed
/// wait time — see the identical rationale on `crate::wifiscan::RECHECK`.
const RECHECK: Duration = Duration::from_secs(1);

/// Battery-aware config re-check cadence: [`BATTERY_CONFIG_POLL_INTERVAL`]
/// while on battery power, else [`CONFIG_POLL_INTERVAL`]. Pure so the
/// on-battery → interval mapping is unit-testable.
fn cadence(on_battery: bool) -> Duration {
    if on_battery {
        BATTERY_CONFIG_POLL_INTERVAL
    } else {
        CONFIG_POLL_INTERVAL
    }
}

/// Best-effort on-battery snapshot — see
/// [`crate::upower::on_battery_snapshot`] for why this reads the cross-thread
/// `shared` bag rather than the thread-local registry, and why an unknown
/// state resolves to AC (#505).
fn on_battery() -> bool {
    crate::upower::on_battery_snapshot()
}

/// Wait out the current battery-aware cadence, re-checking every [`RECHECK`]
/// so a mid-wait power-state flip shortens or lengthens the remaining wait
/// instead of only taking effect on the next cycle.
async fn wait_cadence() {
    let mut waited = Duration::ZERO;
    loop {
        let target = cadence(on_battery());
        if waited >= target {
            return;
        }
        let step = RECHECK.min(target.saturating_sub(waited));
        tokio::time::sleep(step).await;
        waited += step;
    }
}

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

# Departures here (optional): a station id, optionally narrowed by a
# line/direction filter (see below). Verify the id names the same station as
# `name` above — https://v6.bvg.transport.rest/locations?query=<name> — the
# two silently drifting apart (#641) is exactly the bug that made this widget
# never work: the fetch succeeds against a real, nearby, WRONG station, so a
# populated filter then matches nothing, forever, and the widget just looks
# like a quiet evening instead of telling you it's misconfigured.
station = "900192001" # S Schöneweide Bhf (Berlin)

# Walk time from here to the platform, in minutes. With this set, the list
# shows a leave-by countdown ("leave 7 min") instead of the raw departs-in
# time, and fades trains you can no longer make. 0 (the default) keeps the
# plain "departs in" label.
walk_minutes = 10

# `lines`/`directions` narrow which departures show; both empty/absent (the
# default here) means show everything suburban through this station. That's
# deliberate: a filter that's wrong (station drifted, typo'd line name, …)
# fails *invisibly* — an empty list forever, indistinguishable from "nothing's
# running" — while no filter fails *visibly*, since you immediately see
# unexpected lines/directions and can narrow from there. Uncomment and edit
# once you've confirmed the unfiltered board works:
# lines = ["S8", "S85", "S9"]
# directions = ["Spandau", "Birkenwerder", "Hohen Neuendorf", "Waidmannslust"]
"#;

/// A configured place: location identity, Wi-Fi fingerprint, and optional
/// transit config. One `[[place]]` block in `places.toml`.
///
/// Public (and field-public) since #640: it is both what [`configured`] hands
/// out and what the editing API takes back, so an editor round-trips this type
/// rather than a parallel wire struct.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    /// Display name, and this place's identity: the editing API addresses
    /// places by it, and the `place-changed` hook dedups on it. Unique
    /// case-insensitively across the set.
    pub name: String,
    /// Latitude in degrees, `-90..=90`.
    pub lat: f64,
    /// Longitude in degrees, `-180..=180`.
    pub lon: f64,
    /// `GeoClue` fallback radius in kilometres; must be positive.
    pub radius_km: f64,
    /// Network SSIDs forming this place's fingerprint (matched verbatim).
    pub ssids: Vec<String>,
    /// How many of `ssids` must be visible to call it a match.
    pub match_min: usize,
    /// Transit station id for departures here, if any.
    pub station: Option<String>,
    /// Walking minutes from here to the platform; drives departures'
    /// leave-by countdown. `0` = no walk budget (plain departs-in label).
    pub walk_minutes: u32,
    /// Allowed line names. Empty = all lines.
    pub lines: Vec<String>,
    /// Allowed destination substrings. Empty = all.
    pub directions: Vec<String>,
}

impl Place {
    /// A place with only an identity: the same defaults the config schema
    /// applies to an omitted key (`radius_km` 12, `match_min` 2), no
    /// fingerprint and no transit config. The starting point for "add a
    /// place" in an editor.
    #[must_use]
    pub fn new(name: impl Into<String>, lat: f64, lon: f64) -> Self {
        Self {
            name: name.into(),
            lat,
            lon,
            radius_km: default_radius_km(),
            ssids: Vec::new(),
            match_min: default_match_min(),
            station: None,
            walk_minutes: 0,
            lines: Vec::new(),
            directions: Vec::new(),
        }
    }

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
    config_file::path(CONFIG_FILE)
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
            write_default_config();
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

fn write_default_config() {
    if config_file::write("places", CONFIG_FILE, DEFAULT_CONFIG) {
        tracing::info!(file = CONFIG_FILE, "places: wrote default config");
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

// ── Editing (#640 / #703) ───────────────────────────────────────────────────

/// Latitude bound, in degrees (`±MAX_LAT`).
const MAX_LAT: f64 = 90.0;

/// Longitude bound, in degrees (`±MAX_LON`).
const MAX_LON: f64 = 180.0;

/// Why an edit to the place set was rejected, or why persisting it failed.
///
/// Every way user input can be wrong is a variant here — the editing API never
/// panics on it, and never writes a file it would refuse to accept back.
#[derive(Clone, Debug, PartialEq)]
pub enum PlacesError {
    /// A place name was empty or whitespace-only. Names are the identity the
    /// rest of the system addresses a place by, so a blank one is unusable.
    EmptyName,
    /// Two places would share a name (compared trimmed + case-insensitively).
    DuplicateName(String),
    /// Latitude outside `-90..=90` (or not a finite number).
    Latitude {
        /// The offending place's name.
        place: String,
        /// The rejected value.
        lat: f64,
    },
    /// Longitude outside `-180..=180` (or not a finite number).
    Longitude {
        /// The offending place's name.
        place: String,
        /// The rejected value.
        lon: f64,
    },
    /// `radius_km` was zero, negative, or not a finite number — such a place
    /// could never match by [`GeoClue`](crate::geoclue) radius.
    Radius {
        /// The offending place's name.
        place: String,
        /// The rejected value.
        radius_km: f64,
    },
    /// No place in the set carries this name.
    NotFound(String),
    /// `places::service()` isn't registered in this process, so there is no
    /// set to edit.
    NotRunning,
    /// `$HOME` is unset — nowhere to write `places.toml`.
    NoConfigPath,
    /// The set could not be rendered as TOML.
    Encode(String),
    /// The atomic write failed; the previous config is untouched.
    Write(String),
}

impl std::fmt::Display for PlacesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyName => write!(f, "a place needs a name"),
            Self::DuplicateName(name) => {
                write!(f, "a place named \"{name}\" already exists")
            }
            Self::Latitude { place, lat } => {
                write!(f, "\"{place}\": latitude {lat} is outside -90..=90")
            }
            Self::Longitude { place, lon } => {
                write!(f, "\"{place}\": longitude {lon} is outside -180..=180")
            }
            Self::Radius { place, radius_km } => {
                write!(f, "\"{place}\": radius_km {radius_km} must be positive")
            }
            Self::NotFound(name) => write!(f, "no place named \"{name}\""),
            Self::NotRunning => write!(f, "the places service is not registered"),
            Self::NoConfigPath => write!(f, "cannot locate places.toml: $HOME is unset"),
            Self::Encode(e) => write!(f, "could not render places.toml: {e}"),
            Self::Write(e) => {
                write!(
                    f,
                    "could not write places.toml ({e}); the previous config is unchanged"
                )
            }
        }
    }
}

impl std::error::Error for PlacesError {}

/// Trimmed, case-folded key for a place name. A name is a place's identity —
/// the editing API addresses places by it and the `place-changed` hook dedups
/// on it — so `"home"`, `"Home"` and `" Home "` must not coexist.
fn name_key(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Position of the place named `name` (see [`name_key`] for the comparison).
fn index_of(places: &[Place], name: &str) -> Option<usize> {
    let key = name_key(name);
    places.iter().position(|p| name_key(&p.name) == key)
}

/// Canonicalise an incoming place exactly the way [`parse_places`] would after
/// a reload: trim the name, drop a blank station, drop blank list entries.
///
/// This is what keeps memory and file in agreement (#640) — without it a UI
/// could hand us `ssids = ["", "x"]`, we'd write it, the reparse would drop the
/// blank, and the mtime watcher would then see a *different* set than the one
/// we published and churn a spurious reload.
fn normalize(mut place: Place) -> Place {
    place.name = place.name.trim().to_string();
    place.station = place.station.and_then(|s| {
        let trimmed = s.trim().to_string();
        (!trimmed.is_empty()).then_some(trimmed)
    });
    place.ssids = nonblank(place.ssids);
    place.lines = nonblank(place.lines);
    place.directions = nonblank(place.directions);
    place
}

/// Validate a whole place set before it is written.
///
/// Deliberately checks the **result** of an edit, not just the part that
/// changed: whatever we are about to put on disk has to be something we'd
/// accept back. The practical consequence is that an already-invalid file
/// (say, a hand-edited `lat = 500`) has to be repaired before unrelated edits
/// go through — which is the safe direction: a save can never make the file
/// worse than it already is.
fn validate(places: &[Place]) -> Result<(), PlacesError> {
    let mut seen: HashSet<String> = HashSet::new();
    for p in places {
        let key = name_key(&p.name);
        if key.is_empty() {
            return Err(PlacesError::EmptyName);
        }
        if !seen.insert(key) {
            return Err(PlacesError::DuplicateName(p.name.clone()));
        }
        // `contains` is false for NaN/±inf too, so this is the whole check.
        if !(-MAX_LAT..=MAX_LAT).contains(&p.lat) {
            return Err(PlacesError::Latitude {
                place: p.name.clone(),
                lat: p.lat,
            });
        }
        if !(-MAX_LON..=MAX_LON).contains(&p.lon) {
            return Err(PlacesError::Longitude {
                place: p.name.clone(),
                lon: p.lon,
            });
        }
        if !(p.radius_km.is_finite() && p.radius_km > 0.0) {
            return Err(PlacesError::Radius {
                place: p.name.clone(),
                radius_km: p.radius_km,
            });
        }
    }
    Ok(())
}

/// `places` with `place` appended. Pure, so each rule is unit-testable.
fn added(places: &[Place], place: Place) -> Result<Vec<Place>, PlacesError> {
    let mut next = places.to_vec();
    next.push(normalize(place));
    validate(&next)?;
    Ok(next)
}

/// `places` with the place named `target` replaced by `place` (in place, so
/// ordering — and therefore the "first place is provisional home" rule —
/// survives an edit). `place.name` may differ, i.e. this also renames.
fn updated(places: &[Place], target: &str, place: Place) -> Result<Vec<Place>, PlacesError> {
    let idx = index_of(places, target).ok_or_else(|| PlacesError::NotFound(target.to_string()))?;
    let mut next = places.to_vec();
    next[idx] = normalize(place);
    validate(&next)?;
    Ok(next)
}

/// `places` with the place named `from` renamed to `to`, keeping its position.
fn renamed(places: &[Place], from: &str, to: &str) -> Result<Vec<Place>, PlacesError> {
    let idx = index_of(places, from).ok_or_else(|| PlacesError::NotFound(from.to_string()))?;
    let mut next = places.to_vec();
    next[idx].name = to.trim().to_string();
    validate(&next)?;
    Ok(next)
}

/// `places` without the place named `name`.
fn removed(places: &[Place], name: &str) -> Result<Vec<Place>, PlacesError> {
    let idx = index_of(places, name).ok_or_else(|| PlacesError::NotFound(name.to_string()))?;
    let mut next = places.to_vec();
    next.remove(idx);
    validate(&next)?;
    Ok(next)
}

/// Serialisation mirror of [`ConfigFile`]. Separate from the `Deserialize`
/// side because the two are not symmetric: reading tolerates omitted keys (the
/// `#[serde(default)]`s), writing spells every one of them out so a reparse of
/// what we wrote yields exactly the set we published — no default-drift.
#[derive(serde::Serialize)]
struct ConfigOut<'a> {
    place: Vec<PlaceOut<'a>>,
}

#[derive(serde::Serialize)]
struct PlaceOut<'a> {
    name: &'a str,
    lat: f64,
    lon: f64,
    radius_km: f64,
    ssids: &'a [String],
    match_min: usize,
    /// Omitted entirely when unset, which is how the schema spells "no
    /// departures here" (`Option<String>` + `#[serde(default)]` on the way in).
    #[serde(skip_serializing_if = "Option::is_none")]
    station: Option<&'a str>,
    walk_minutes: u32,
    lines: &'a [String],
    directions: &'a [String],
}

impl<'a> From<&'a Place> for PlaceOut<'a> {
    fn from(p: &'a Place) -> Self {
        Self {
            name: &p.name,
            lat: p.lat,
            lon: p.lon,
            radius_km: p.radius_km,
            ssids: &p.ssids,
            match_min: p.match_min,
            station: p.station.as_deref(),
            walk_minutes: p.walk_minutes,
            lines: &p.lines,
            directions: &p.directions,
        }
    }
}

/// Render a place set as the body of a `places.toml`. Pure, so round-trip
/// fidelity is unit-testable.
fn serialize_places(places: &[Place]) -> Result<String, PlacesError> {
    let out = ConfigOut {
        place: places.iter().map(PlaceOut::from).collect(),
    };
    toml::to_string(&out).map_err(|e| PlacesError::Encode(e.to_string()))
}

/// The file's leading comment block — every line up to (not including) the
/// first line that is neither blank nor a `#` comment — normalised to end in
/// exactly one blank line, or empty when the file opens with content.
///
/// This is the half of the user's formatting a re-render *can* preserve, and
/// it's the valuable half: it's where the shipped default puts the whole
/// documented schema (how to capture a fingerprint, what `match_min` means,
/// the station-id lookup URL). Per-key comments inside a `[[place]]` are lost
/// — see the module docs.
fn leading_comments(text: &str) -> String {
    let block: Vec<&str> = text
        .lines()
        .take_while(|l| {
            let t = l.trim_start();
            t.is_empty() || t.starts_with('#')
        })
        .collect();
    let end = block
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map_or(0, |i| i + 1);
    if end == 0 {
        return String::new();
    }
    let mut header = block[..end].join("\n");
    header.push_str("\n\n");
    header
}

/// Scratch-file counter, so two saves racing inside one process can't pick the
/// same temporary path.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Write `body` to `path` crash-safely.
///
/// Renders into a scratch file **in the same directory** (so the `rename(2)`
/// is a same-filesystem, atomic swap — a `/tmp` scratch file would be a
/// cross-device copy, which is not), `fsync`s it, carries the target's
/// permissions over, then renames it into place. A crash, a full disk or a
/// killed shell mid-write therefore leaves either the old config or the new
/// one, never a half-written file — the failure mode a plain
/// `File::create` + `write_all` has, which would silently truncate the user's
/// places to nothing.
///
/// On failure the scratch file is cleaned up and the target is untouched.
fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("config path has no parent directory"))?;
    std::fs::create_dir_all(dir)?;

    let stem = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{stem}.{}.{seq}.tmp", std::process::id()));

    let swap = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        drop(file);
        // Preserve the mode of the file we're replacing (a hand-tightened
        // 0600 config must not come back as 0644). A brand-new file keeps
        // `File::create`'s umask default, matching `config_file::write`.
        if let Ok(meta) = std::fs::metadata(path) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, path)
    };

    if let Err(e) = swap() {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // The rename itself is only durable once the *directory* entry is synced.
    // Best-effort: some filesystems refuse an fsync on a read-only dir handle.
    let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    Ok(())
}

/// Render + atomically write `places` to `places.toml`.
fn persist(places: &[Place]) -> Result<(), PlacesError> {
    let path = config_path().ok_or(PlacesError::NoConfigPath)?;
    persist_to(&path, places)
}

/// [`persist`] against an explicit path, so tests can drive it into a tempdir
/// instead of the user's real config.
///
/// Carries the existing file's leading comment block forward; when there is no
/// file yet (or it can't be read) the built-in default's header is used, so a
/// config this API creates is as self-documenting as the one first run writes.
fn persist_to(path: &Path, places: &[Place]) -> Result<(), PlacesError> {
    let header = match std::fs::read_to_string(path) {
        Ok(text) => leading_comments(&text),
        Err(_) => leading_comments(DEFAULT_CONFIG),
    };
    let body = serialize_places(places)?;
    write_atomic(path, &format!("{header}{body}")).map_err(|e| PlacesError::Write(e.to_string()))
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
    /// The configured set as last loaded from (or written to) `places.toml`.
    /// Both the live reload and the editing API publish here; `resolve_loop`
    /// re-resolves on every swap.
    pub(crate) configured: Mutable<Arc<Vec<Place>>>,
}

// Cross-thread shared handles for the tokio-side consumers (departures,
// weather), mirroring the geoclue pattern.
struct Shared {
    place: Mutable<Option<ResolvedPlace>>,
    location: Mutable<LocationState>,
    configured: Mutable<Arc<Vec<Place>>>,
}

pub struct PlacesService;

impl Service for PlacesService {
    type Handles = PlacesHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let loaded = load_places();
        warn_unsatisfiable_fingerprints(&loaded);
        let handles = PlacesHandles {
            configured: Mutable::new(Arc::new(loaded)),
            ..PlacesHandles::default()
        };
        let place = handles.place.clone();
        let location = handles.location.clone();
        let places = handles.configured.clone();
        shared::insert(Shared {
            place: place.clone(),
            location: location.clone(),
            configured: places.clone(),
        });
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
    shared::get::<Shared>().map(|s| s.place.clone())
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
    shared::get::<Shared>().map(|s| s.location.clone())
}

// ── Editing API (#640 / #703) ───────────────────────────────────────────────

/// Signal of the *configured* set — what `places.toml` says, as opposed to
/// [`current_place`]'s "where am I right now". Re-emits on a live reload and
/// after every successful edit, so an editor never has to poll.
pub fn configured() -> impl Signal<Item = Arc<Vec<Place>>> {
    registry::with(|r| {
        r.get::<PlacesHandles>()
            .expect("places::service() not registered")
            .configured
            .signal_cloned()
    })
}

/// Snapshot of the configured set, readable from any thread (the D-Bus
/// handlers that will front this run on the tokio runtime, not the GTK
/// thread). Empty when `places::service()` isn't registered.
#[must_use]
pub fn configured_snapshot() -> Vec<Place> {
    shared::get::<Shared>().map_or_else(Vec::new, |s| (*s.configured.get_cloned()).clone())
}

/// Add a place.
///
/// Rejects a blank name, a name already taken (trimmed, case-insensitively),
/// coordinates outside `±90`/`±180`, and a non-positive `radius_km`.
pub fn add_place(place: Place) -> Result<(), PlacesError> {
    edit(|current| added(current, place))
}

/// Replace the place named `target` — keeping its position in the file — with
/// `place`. `place.name` may differ from `target`, so this covers a rename
/// bundled with other edits.
pub fn update_place(target: &str, place: Place) -> Result<(), PlacesError> {
    edit(|current| updated(current, target, place))
}

/// Rename the place named `from` to `to`, leaving everything else alone.
pub fn rename_place(from: &str, to: &str) -> Result<(), PlacesError> {
    edit(|current| renamed(current, from, to))
}

/// Delete the place named `name`.
///
/// Deleting the last place is allowed: `load_places` then falls back to the
/// built-in default, exactly as it does for a hand-emptied file.
pub fn remove_place(name: &str) -> Result<(), PlacesError> {
    edit(|current| removed(current, name))
}

/// Replace the whole set in one write — the "the editor sent us its model
/// back" path, and the only one that can reorder places (which matters: the
/// first `[[place]]` is the provisional home before the first location fix).
pub fn save_places(places: Vec<Place>) -> Result<(), PlacesError> {
    edit(move |_| {
        let next: Vec<Place> = places.into_iter().map(normalize).collect();
        validate(&next)?;
        Ok(next)
    })
}

/// Serializes [`edit`]'s read-modify-write.
///
/// Without it, two edits racing (two `Control` calls landing on different
/// tokio workers) would both read the *same* set and the second write would
/// silently drop the first's change. The critical section is a validate plus
/// one small file write with no `.await` inside, so a plain `Mutex` is both
/// safe here and cheap.
static EDIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The one write path: apply `f` to the current set, persist the result
/// atomically, then publish it.
///
/// Order matters. The file is written **first**, so a failed write leaves the
/// in-memory set untouched and the two still agree. Publishing second is what
/// makes an edit visible immediately: `resolve_loop` subscribes to this very
/// handle, so departures/weather/place-detection re-resolve on the spot rather
/// than waiting up to [`BATTERY_CONFIG_POLL_INTERVAL`] for the mtime poll to
/// notice our own write. That poll still runs, and finds nothing to do — it
/// content-compares the reparse against what we published, and
/// [`normalize`] guarantees they match.
fn edit(f: impl FnOnce(&[Place]) -> Result<Vec<Place>, PlacesError>) -> Result<(), PlacesError> {
    let shared = shared::get::<Shared>().ok_or(PlacesError::NotRunning)?;
    let handle = shared.configured.clone();
    let _serialized = EDIT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let current = handle.get_cloned();
    let next = f(current.as_slice())?;
    persist(&next)?;
    warn_unsatisfiable_fingerprints(&next);
    tracing::info!(count = next.len(), "places: config saved");
    handle.set(Arc::new(next));
    Ok(())
}

/// Poll `places.toml` and republish the parsed list when it changes, so config
/// edits take effect within [`CONFIG_POLL_INTERVAL`] (or
/// [`BATTERY_CONFIG_POLL_INTERVAL`] on battery — #505) without restarting the
/// shell. [`resolve_loop`] subscribes to the same handle and re-resolves on
/// each swap, exactly as it does for the Wi-Fi and `GeoClue` sensors.
async fn watch_config(places: Mutable<Arc<Vec<Place>>>) {
    let mut watcher = ConfigWatcher::new();
    loop {
        wait_cadence().await;
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

    // ── Battery-aware cadence (#505) ─────────────────────────────────────────

    #[test]
    fn cadence_is_config_poll_interval_on_ac() {
        assert_eq!(cadence(false), CONFIG_POLL_INTERVAL);
    }

    #[test]
    fn cadence_stretches_on_battery() {
        assert_eq!(cadence(true), BATTERY_CONFIG_POLL_INTERVAL);
        assert!(BATTERY_CONFIG_POLL_INTERVAL > CONFIG_POLL_INTERVAL);
    }

    // ── Config ─────────────────────────────────────────────────────────────

    #[test]
    fn default_config_parses() {
        let places = default_places();
        assert_eq!(places.len(), 1);
        assert_eq!(places[0].name, "Schöneweide");
        assert_eq!(places[0].station.as_deref(), Some("900192001"));
        assert_eq!(places[0].match_min, 2);
        assert!(places[0].ssids.is_empty());
        assert_eq!(places[0].walk_minutes, 10);
        // Shipped commented out (#641): a wrong filter fails invisibly (zero
        // matches, forever), an absent one fails visibly (you see everything
        // and narrow from there) — so the default leaves both axes open.
        assert!(places[0].lines.is_empty());
        assert!(places[0].directions.is_empty());
        assert!((places[0].radius_km - 12.0).abs() < 1e-9);
    }

    /// Ties the shipped default's station id to the place it's named after —
    /// #641 shipped `900180001` ("S Köpenick/Parrisiusstr. (Berlin)", live
    /// BVG API) under the name "Schöneweide", served only by S3, none of
    /// which passes through Köpenick: a structural, permanent zero-match.
    /// `900192001` is "S Schöneweide Bhf (Berlin)", the correct id. A bare
    /// literal comparison (as the pre-#641 tests had) can't catch this class
    /// of bug — the constant is self-consistently wrong — so this pins the
    /// id/name *pair* and spells out the real-world station name in the
    /// failure message.
    #[test]
    fn default_station_id_matches_its_place_name() {
        let places = default_places();
        assert_eq!(places[0].name, "Schöneweide");
        assert_eq!(
            places[0].station.as_deref(),
            Some("900192001"),
            "default station id must stay \"900192001\" (\"S Schöneweide Bhf \
             (Berlin)\" per the live BVG API) — NOT \"900180001\", which is a \
             DIFFERENT, nearby station (\"S Köpenick/Parrisiusstr. (Berlin)\", \
             #641). If you're changing this id, verify the new one at \
             https://v6.bvg.transport.rest/locations?query=Schöneweide first."
        );
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
        // A fixture id, not a real BVG station — deliberately distinct from
        // both ids in #641 so this test never looks like it's asserting
        // anything about the real default config.
        let home = Some(resolved("Home", Some("900000001")));

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
        // Fixture ids, not real BVG stations — see the comment on
        // `place_transition_silent_on_first_resolution`.
        let home = Some(resolved("Home", Some("900000001")));
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

    // ── Write path: serialisation round-trip (#640) ─────────────────────────

    /// A place with *every* field non-default, so a dropped field in the
    /// serialiser can't hide behind a coincidental default on reparse.
    fn full_place(name: &str) -> Place {
        Place {
            name: name.to_string(),
            lat: 52.4556,
            lon: 13.5085,
            radius_km: 3.25,
            ssids: vec!["FRITZ!Box 7590".into(), "Telekom-ABC".into()],
            match_min: 2,
            station: Some("900192001".into()),
            walk_minutes: 10,
            lines: vec!["S8".into(), "S85".into()],
            directions: vec!["Spandau".into(), "Birkenwerder".into()],
        }
    }

    #[test]
    fn serialize_round_trips_every_field() {
        let places = vec![
            full_place("Schöneweide"),
            Place::new("Office", -33.87, 151.21),
        ];
        let text = serialize_places(&places).expect("renders");
        assert_eq!(parse_places(&text).expect("reparses"), places);
    }

    #[test]
    fn serialize_omits_station_when_unset() {
        let places = vec![Place::new("Nowhere", 0.0, 0.0)];
        let text = serialize_places(&places).expect("renders");
        assert!(
            !text.contains("station"),
            "an unset station must be absent, not empty: {text}"
        );
        assert_eq!(parse_places(&text).expect("reparses")[0].station, None);
    }

    #[test]
    fn default_config_round_trips() {
        let places = default_places();
        let text = serialize_places(&places).expect("renders");
        assert_eq!(parse_places(&text).expect("reparses"), places);
    }

    /// The headline guarantee: parse → mutate → serialize → parse is the set
    /// we asked for, field for field, ordering included.
    #[test]
    fn parse_mutate_serialize_parse_is_stable() {
        let start = default_places();

        let with_office = added(&start, full_place("Office")).expect("adds");
        let renamed_home = renamed(&with_office, "schöneweide", "Home").expect("renames");
        let retuned = updated(
            &renamed_home,
            "Office",
            Place {
                walk_minutes: 4,
                station: None,
                ..full_place("Office")
            },
        )
        .expect("updates");

        let text = serialize_places(&retuned).expect("renders");
        let reparsed = parse_places(&text).expect("reparses");
        assert_eq!(reparsed, retuned);
        assert_eq!(reparsed[0].name, "Home", "rename keeps position");
        assert_eq!(reparsed[1].walk_minutes, 4);
        assert_eq!(reparsed[1].station, None);

        let without_office = removed(&reparsed, "OFFICE").expect("removes");
        assert_eq!(
            parse_places(&serialize_places(&without_office).expect("renders")).expect("reparses"),
            start
                .iter()
                .cloned()
                .map(|p| Place {
                    name: "Home".into(),
                    ..p
                })
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn normalize_matches_what_a_reparse_would_yield() {
        // Exactly the input a UI can produce: padded name, blank list entries,
        // a whitespace-only station.
        let messy = Place {
            name: "  Office  ".into(),
            station: Some("   ".into()),
            ssids: vec![String::new(), "wifi".into(), "  ".into()],
            lines: vec!["S8".into(), " ".into()],
            directions: vec![String::new()],
            ..full_place("ignored")
        };
        let clean = normalize(messy);
        assert_eq!(clean.name, "Office");
        assert_eq!(clean.station, None);
        assert_eq!(clean.ssids, ["wifi"]);
        assert_eq!(clean.lines, ["S8"]);
        assert!(clean.directions.is_empty());
        // …and the file agrees with memory: a reparse is a no-op on it.
        let text = serialize_places(std::slice::from_ref(&clean)).expect("renders");
        assert_eq!(parse_places(&text).expect("reparses"), vec![clean]);
    }

    // ── Write path: validation (#640) ───────────────────────────────────────

    #[test]
    fn validate_accepts_the_shipped_default() {
        assert_eq!(validate(&default_places()), Ok(()));
    }

    #[test]
    fn validate_rejects_blank_names() {
        for name in ["", "   ", "\t\n"] {
            assert_eq!(
                validate(&[Place::new(name, 0.0, 0.0)]),
                Err(PlacesError::EmptyName),
                "{name:?} must be rejected"
            );
        }
        assert_eq!(
            added(&[], Place::new("  ", 1.0, 2.0)),
            Err(PlacesError::EmptyName)
        );
    }

    #[test]
    fn validate_rejects_duplicate_names_ignoring_case_and_padding() {
        let home = Place::new("Home", 1.0, 2.0);
        for clash in ["Home", "home", "  HOME  "] {
            let err = added(std::slice::from_ref(&home), Place::new(clash, 3.0, 4.0));
            assert_eq!(
                err,
                Err(PlacesError::DuplicateName(clash.trim().to_string())),
                "{clash:?} must collide with \"Home\""
            );
        }
        // A rename onto an existing name collides too.
        let set = vec![home, Place::new("Office", 3.0, 4.0)];
        assert_eq!(
            renamed(&set, "Office", "home"),
            Err(PlacesError::DuplicateName("home".to_string()))
        );
    }

    #[test]
    fn validate_rejects_out_of_range_latitude() {
        for lat in [90.5, -90.5, f64::NAN, f64::INFINITY] {
            let place = Place::new("P", lat, 0.0);
            match validate(std::slice::from_ref(&place)) {
                Err(PlacesError::Latitude { place, .. }) => assert_eq!(place, "P"),
                other => panic!("lat {lat} must be rejected, got {other:?}"),
            }
        }
        // The bounds themselves are valid.
        assert_eq!(validate(&[Place::new("P", 90.0, 0.0)]), Ok(()));
        assert_eq!(validate(&[Place::new("P", -90.0, 0.0)]), Ok(()));
    }

    #[test]
    fn validate_rejects_out_of_range_longitude() {
        for lon in [180.5, -180.5, f64::NAN, f64::NEG_INFINITY] {
            let place = Place::new("P", 0.0, lon);
            match validate(std::slice::from_ref(&place)) {
                Err(PlacesError::Longitude { place, .. }) => assert_eq!(place, "P"),
                other => panic!("lon {lon} must be rejected, got {other:?}"),
            }
        }
        assert_eq!(validate(&[Place::new("P", 0.0, 180.0)]), Ok(()));
        assert_eq!(validate(&[Place::new("P", 0.0, -180.0)]), Ok(()));
    }

    #[test]
    fn validate_rejects_non_positive_radius() {
        for radius_km in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let place = Place {
                radius_km,
                ..Place::new("P", 0.0, 0.0)
            };
            match validate(std::slice::from_ref(&place)) {
                Err(PlacesError::Radius { place, .. }) => assert_eq!(place, "P"),
                other => panic!("radius {radius_km} must be rejected, got {other:?}"),
            }
        }
    }

    #[test]
    fn edits_of_an_unknown_place_are_not_found() {
        let set = vec![Place::new("Home", 1.0, 2.0)];
        let missing = || Err(PlacesError::NotFound("Ghost".to_string()));
        assert_eq!(updated(&set, "Ghost", Place::new("G", 1.0, 2.0)), missing());
        assert_eq!(renamed(&set, "Ghost", "G"), missing());
        assert_eq!(removed(&set, "Ghost"), missing());
    }

    #[test]
    fn removing_the_last_place_is_allowed() {
        let set = vec![Place::new("Home", 1.0, 2.0)];
        assert_eq!(removed(&set, "Home"), Ok(Vec::new()));
    }

    // ── Write path: atomic file replacement (#640) ──────────────────────────

    /// Leftover scratch files (they're dotfiles named after their target).
    fn scratch_files(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("dir readable")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect()
    }

    #[test]
    fn write_atomic_replaces_content_and_leaves_no_scratch_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("places.toml");
        write_atomic(&target, "first").expect("writes");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first");
        write_atomic(&target, "second").expect("overwrites");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second");
        assert!(
            scratch_files(dir.path()).is_empty(),
            "scratch files must be renamed away, not left behind"
        );
    }

    #[test]
    fn write_atomic_preserves_the_targets_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("places.toml");
        std::fs::write(&target, "old").expect("seed");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        write_atomic(&target, "new").expect("writes");

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a hand-tightened config must stay tightened");
    }

    #[test]
    fn write_atomic_creates_the_parent_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("nested/deeper/places.toml");
        write_atomic(&target, "body").expect("writes");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "body");
    }

    #[test]
    fn write_atomic_failure_leaves_the_target_and_no_scratch_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A directory can't be replaced by `rename(2)` from a file: the swap
        // fails *after* the scratch file exists, which is the case that must
        // clean up after itself.
        let target = dir.path().join("victim");
        std::fs::create_dir(&target).expect("mkdir");

        assert!(write_atomic(&target, "body").is_err());
        assert!(target.is_dir(), "the target must be untouched");
        assert!(
            scratch_files(dir.path()).is_empty(),
            "a failed swap must not leave a scratch file: {:?}",
            scratch_files(dir.path())
        );
    }

    // ── Write path: comment preservation + persist (#640) ───────────────────

    #[test]
    fn leading_comments_stops_at_the_first_content_line() {
        let text = "# one\n\n# two\n\n[[place]]\nname = \"x\"\n# trailing\n";
        assert_eq!(leading_comments(text), "# one\n\n# two\n\n");
        assert_eq!(leading_comments("[[place]]\n# after\n"), "");
        assert_eq!(leading_comments(""), "");
        assert_eq!(leading_comments("\n\n\n"), "");
        // The shipped default's whole documented preamble survives.
        let header = leading_comments(DEFAULT_CONFIG);
        assert!(header.starts_with("# trollshell places"));
        assert!(header.contains("trollshell --scan-aps"));
        assert!(header.ends_with("\n\n"));
    }

    #[test]
    fn persist_to_keeps_the_header_and_reparses_to_the_saved_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("places.toml");
        std::fs::write(&target, DEFAULT_CONFIG).expect("seed");

        let next = added(&default_places(), full_place("Office")).expect("adds");
        persist_to(&target, &next).expect("persists");

        let text = std::fs::read_to_string(&target).expect("readable");
        assert!(
            text.starts_with("# trollshell places"),
            "the documented preamble must survive a save"
        );
        assert!(text.contains("trollshell --scan-aps"));
        assert_eq!(parse_places(&text).expect("reparses"), next);
    }

    #[test]
    fn persist_to_seeds_the_default_header_for_a_brand_new_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("places.toml");
        persist_to(&target, &[full_place("Home")]).expect("persists");
        let text = std::fs::read_to_string(&target).expect("readable");
        assert!(text.starts_with("# trollshell places"));
        assert_eq!(parse_places(&text).expect("reparses").len(), 1);
    }

    #[test]
    fn persist_to_respects_a_header_the_user_deleted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("places.toml");
        std::fs::write(
            &target,
            "[[place]]\nname = \"Home\"\nlat = 1.0\nlon = 2.0\n",
        )
        .expect("seed");
        persist_to(&target, &[Place::new("Home", 1.0, 2.0)]).expect("persists");
        let text = std::fs::read_to_string(&target).expect("readable");
        assert!(
            text.starts_with("[[place]]"),
            "a save must not re-add comments the user removed: {text}"
        );
    }

    // ── Write path: the public API, end to end (#640) ───────────────────────

    /// The `shared` map is process-global and `reset_for_tests` clears *all* of
    /// it, so the two cases that publish into (and clear) it are serialized —
    /// cargo runs tests in parallel threads of one process.
    static SHARED_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The requirement that makes the write path usable from a running shell:
    /// after an edit the published handle and the file on disk agree, so
    /// `resolve_loop` (and through it departures/weather) sees the new set
    /// immediately and the mtime watcher finds nothing to churn.
    #[test]
    fn editing_api_writes_the_file_and_republishes_the_set() {
        let _guard = SHARED_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".config/trollshell")).expect("mkdir");
        let cfg = dir.path().join(".config/trollshell/places.toml");
        std::fs::write(&cfg, DEFAULT_CONFIG).expect("seed");

        // `temp_env` serializes $HOME mutation across tests and restores it.
        temp_env::with_var("HOME", Some(dir.path().as_os_str()), || {
            let handle = Mutable::new(Arc::new(load_places()));
            shared::insert(Shared {
                place: Mutable::default(),
                location: Mutable::default(),
                configured: handle.clone(),
            });

            // Both sides agree after every mutation, and the watcher's
            // content-compare (reparse vs. published) finds them identical.
            let agree = |step: &str| {
                let published = handle.get_cloned();
                assert_eq!(
                    load_places(),
                    *published,
                    "{step}: file and memory must agree"
                );
                (*published).clone()
            };

            add_place(Place::new("Office", 52.5, 13.4)).expect("adds");
            let set = agree("add");
            assert_eq!(set.len(), 2);
            assert_eq!(set[1].name, "Office");

            rename_place("office", "Werk").expect("renames");
            let set = agree("rename");
            assert_eq!(set[1].name, "Werk");

            update_place("Werk", full_place("Werk")).expect("updates");
            let set = agree("update");
            assert_eq!(set[1].station.as_deref(), Some("900192001"));
            assert_eq!(set[1].walk_minutes, 10);

            save_places(vec![full_place("Werk"), Place::new("Home", 1.0, 2.0)]).expect("saves");
            let set = agree("save");
            assert_eq!(set[0].name, "Werk", "a whole-set save can reorder");

            remove_place("Werk").expect("removes");
            let set = agree("remove");
            assert_eq!(set.len(), 1);
            assert_eq!(set[0].name, "Home");

            // A rejected edit changes neither side.
            assert_eq!(
                add_place(Place::new("Home", 1.0, 2.0)),
                Err(PlacesError::DuplicateName("Home".to_string()))
            );
            assert_eq!(agree("rejected duplicate").len(), 1);
            assert!(matches!(
                add_place(Place::new("Moon", 1000.0, 0.0)),
                Err(PlacesError::Latitude { .. })
            ));
            assert_eq!(agree("rejected latitude").len(), 1);
            assert_eq!(
                remove_place("Ghost"),
                Err(PlacesError::NotFound("Ghost".into()))
            );

            // The documented preamble is still there after all of that.
            let text = std::fs::read_to_string(&cfg).expect("readable");
            assert!(text.starts_with("# trollshell places"));

            hytte_reactive::shared::reset_for_tests();
        });
    }

    /// Concurrent edits are read-modify-write against one file, so without
    /// serialization the loser's change is silently written away. Eight
    /// threads, eight distinct places, all eight must land.
    #[test]
    fn concurrent_edits_do_not_lose_each_other() {
        let _guard = SHARED_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".config/trollshell")).expect("mkdir");
        std::fs::write(
            dir.path().join(".config/trollshell/places.toml"),
            DEFAULT_CONFIG,
        )
        .expect("seed");

        temp_env::with_var("HOME", Some(dir.path().as_os_str()), || {
            let handle = Mutable::new(Arc::new(load_places()));
            shared::insert(Shared {
                place: Mutable::default(),
                location: Mutable::default(),
                configured: handle.clone(),
            });

            std::thread::scope(|s| {
                for i in 0..8 {
                    s.spawn(move || {
                        add_place(Place::new(format!("P{i}"), 1.0, 2.0)).expect("adds");
                    });
                }
            });

            let set = handle.get_cloned();
            assert_eq!(set.len(), 9, "the default + all eight adds must survive");
            assert_eq!(load_places(), *set, "file and memory must still agree");

            hytte_reactive::shared::reset_for_tests();
        });
    }

    #[test]
    fn editing_api_reports_a_missing_service_instead_of_panicking() {
        let _guard = SHARED_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        hytte_reactive::shared::reset_for_tests();
        assert_eq!(
            add_place(Place::new("Home", 1.0, 2.0)),
            Err(PlacesError::NotRunning)
        );
    }
}
