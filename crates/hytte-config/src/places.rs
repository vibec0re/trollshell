//! The `places.toml` model: its schema, its validation rules, and the
//! format-preserving writer both of its editors go through.
//!
//! A *place* is somewhere you frequent (home, office): coordinates, a Wi-Fi
//! fingerprint (the set of network SSIDs you see there), and optional transit
//! config (a station + line/direction filter for the departures widget).
//! Places load from `~/.config/trollshell/places.toml`; a documented default
//! is written on first run.
//!
//! This crate holds the half that has no runtime: parsing, validation, the
//! four whole-set mutations, and the writer. Resolving *which* place you are
//! at — the Wi-Fi/`GeoClue` fusion, the live-reload task, the published
//! signals — lives in `hytte_services::places`, which re-exports the types
//! below so consumers see one API.
//!
//! # Editing (#640 / #703)
//!
//! [`save_to`] is the whole-set write path: it refuses to overwrite a file it
//! cannot account for, normalises and validates the set, then patches it into
//! the existing document and replaces the file atomically. `hytte-services`
//! uses the same [`check_base`]/[`persist_to`] pair under its own lock so it
//! can republish the reactive handle in the same critical section.
//!
//! A save is a **format-preserving patch, not a re-render** — see
//! [`render_places`]. The file has two permanent authors (the operator, and
//! the control-center editor), so a save touches only the keys whose values
//! actually moved: the documented preamble, per-key comments inside a
//! `[[place]]`, hand-chosen key ordering, keys this model does not even know
//! about, and unrelated top-level tables all survive it byte for byte. `toml`
//! stays the *reader* — one parse path, one schema — and `toml_edit` writes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::file as config_file;

/// Config file under `~/.config/trollshell/`.
const CONFIG_FILE: &str = "places.toml";

/// Documented default, written on first run and used as the fallback for a
/// missing/empty/malformed config. Kept *as TOML* so the loader has one parse
/// path and the written file matches behaviour.
pub const DEFAULT_CONFIG: &str = r#"# trollshell places — where you frequent, how the shell recognises each, and
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

    #[must_use]
    pub fn resolved(&self) -> ResolvedPlace {
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

#[must_use]
pub fn default_radius_km() -> f64 {
    12.0
}

#[must_use]
pub fn default_match_min() -> usize {
    2
}

#[must_use]
pub fn config_path() -> Option<PathBuf> {
    config_file::path(CONFIG_FILE)
}

/// Drop empty/whitespace-only entries (a stray `""` would otherwise become an
/// accidental allow-all, since an empty needle is a substring of everything).
fn nonblank(items: Vec<String>) -> Vec<String> {
    items.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

/// Parse a config body into places. Pure, so the schema is unit-testable.
pub fn parse_places(toml_text: &str) -> Result<Vec<Place>, String> {
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

/// [`DEFAULT_CONFIG`] parsed — the set every "can't use the user's config"
/// path falls back to, and (since a config that renders to zero places reads
/// back as this) the set an emptied file means.
///
/// The built-in default is parse-tested, so in practice this is non-empty; if
/// a malformed `DEFAULT_CONFIG` ever shipped it degrades to an empty list —
/// logged loudly — rather than crashing the whole shell on cold start.
#[must_use]
pub fn builtin_default() -> Vec<Place> {
    parse_places(DEFAULT_CONFIG).unwrap_or_else(|e| {
        tracing::error!(error = %e, "built-in default places config failed to parse");
        Vec::new()
    })
}

/// Load places, writing the documented default on first run. Returns
/// [`builtin_default`] for a missing/empty/malformed user config.
#[must_use]
pub fn load_places() -> Vec<Place> {
    let default = builtin_default;
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
pub fn warn_unsatisfiable_fingerprints(places: &[Place]) {
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
pub struct ConfigWatcher {
    path: Option<PathBuf>,
    last: Option<SystemTime>,
}

impl Default for ConfigWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigWatcher {
    /// Start watching from *now*: the file's current mtime is taken as the
    /// baseline, so the first [`poll`](Self::poll) reports only edits made
    /// after construction rather than replaying the state at startup.
    #[must_use]
    pub fn new() -> Self {
        let path = config_path();
        let last = path.as_deref().and_then(mtime);
        Self { path, last }
    }

    /// Reload and return the fresh places when the file's mtime has moved since
    /// the previous poll *and* the parsed list differs from `current`; otherwise
    /// `None` (unchanged mtime, no config path, or an identical reparse).
    pub fn poll(&mut self, current: &[Place]) -> Option<Vec<Place>> {
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
pub const MAX_LAT: f64 = 90.0;

/// Longitude bound, in degrees (`±MAX_LON`).
pub const MAX_LON: f64 = 180.0;

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
    /// `places.toml` exists but its contents can't be established: unreadable
    /// (permissions), non-UTF-8, or not valid TOML. Refusing rather than
    /// overwriting bytes we can't account for — see [`edit`] for why this is a
    /// data-loss guard and not just tidiness.
    Unreadable(String),
    /// `places.toml` parses, but to a *different* set than the one in memory —
    /// something edited it since we last loaded it. Refusing, because the edit
    /// was computed against a stale base and applying it would write the
    /// out-of-process change away. See [`edit`].
    ChangedOnDisk,
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
            Self::Unreadable(e) => write!(
                f,
                "places.toml could not be read back ({e}); refusing to overwrite it — fix or move the file, then retry"
            ),
            Self::ChangedOnDisk => write!(
                f,
                "places.toml changed on disk since it was loaded; refusing to overwrite it — the change is picked up within a few seconds, then retry"
            ),
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
#[must_use]
pub fn normalize(mut place: Place) -> Place {
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
pub fn validate(places: &[Place]) -> Result<(), PlacesError> {
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
pub fn added(places: &[Place], place: Place) -> Result<Vec<Place>, PlacesError> {
    let mut next = places.to_vec();
    next.push(normalize(place));
    validate(&next)?;
    Ok(next)
}

/// `places` with the place named `target` replaced by `place` (in place, so
/// ordering — and therefore the "first place is provisional home" rule —
/// survives an edit). `place.name` may differ, i.e. this also renames.
pub fn updated(places: &[Place], target: &str, place: Place) -> Result<Vec<Place>, PlacesError> {
    let idx = index_of(places, target).ok_or_else(|| PlacesError::NotFound(target.to_string()))?;
    let mut next = places.to_vec();
    next[idx] = normalize(place);
    validate(&next)?;
    Ok(next)
}

/// `places` with the place named `from` renamed to `to`, keeping its position.
pub fn renamed(places: &[Place], from: &str, to: &str) -> Result<Vec<Place>, PlacesError> {
    let idx = index_of(places, from).ok_or_else(|| PlacesError::NotFound(from.to_string()))?;
    let mut next = places.to_vec();
    next[idx].name = to.trim().to_string();
    validate(&next)?;
    Ok(next)
}

/// `places` without the place named `name`.
pub fn removed(places: &[Place], name: &str) -> Result<Vec<Place>, PlacesError> {
    let idx = index_of(places, name).ok_or_else(|| PlacesError::NotFound(name.to_string()))?;
    let mut next = places.to_vec();
    next.remove(idx);
    validate(&next)?;
    Ok(next)
}

// ── Serialisation: a format-preserving document patch (#640) ────────────────
//
// Writing is deliberately *not* the mirror image of reading. Reading is a
// serde deserialize into [`PlaceCfg`], which tolerates omitted keys and
// ignores ones it doesn't know. Writing used to be the symmetric
// `toml::to_string` of a fixed output struct, which had three consequences
// nobody wanted once the file gained a second author (#703):
//
//   * every comment attached to a key, and every hand-chosen key ordering
//     inside a `[[place]]`, was discarded on the first programmatic save;
//   * so was any key the typed model doesn't know about — a hand-added
//     annotation parsed fine and then vanished, silently;
//   * so was any *other* top-level table someone kept in the file.
//
// The writer below therefore edits the parsed document instead of rebuilding
// one: it locates the `[[place]]` table each wanted entry belongs to, assigns
// only the keys whose value actually moved, and leaves everything else — other
// keys, other tables, whitespace, comments — exactly as it found it. The typed
// model stays the *validation* surface; it is no longer the write surface, so
// it can't destroy what it doesn't model.

/// Render `places` back into whatever document `existing` holds, patching it
/// rather than re-rendering it. Pure, so both the fidelity and the round-trip
/// are unit-testable.
///
/// Each wanted place is matched to an existing `[[place]]` table (see
/// [`align`]) and patched in place ([`patch_table`]); entries with no match are
/// appended as freshly built tables ([`new_table`]); tables nothing matched are
/// dropped. The file's opening comment block is carried across the rebuild by
/// [`take_header`]/[`put_header`], because `toml_edit` glues it to whichever
/// table happens to come first and removing or reordering places would
/// otherwise take it along.
///
/// A reparse of the result is exactly `places` again: a key is left alone only
/// when what the file already says parses back to the value we want, defaults
/// included, so "untouched" can never mean "drifted".
///
/// # Errors
/// [`PlacesError::Encode`] when `existing` isn't valid TOML — refusing rather
/// than replacing bytes we couldn't account for, the same instinct as
/// [`read_on_disk`]. Callers arriving through [`edit`] have already parsed the
/// file, so this is unreachable there.
pub fn render_places(existing: &str, places: &[Place]) -> Result<String, PlacesError> {
    let mut doc: toml_edit::DocumentMut = existing.parse().map_err(|e: toml_edit::TomlError| {
        PlacesError::Encode(format!("the file being replaced is not valid TOML: {e}"))
    })?;
    let header = take_header(&mut doc);

    // The reusable tables, paired with what each parses to, so a patch can
    // compare "what the file says" against "what we want" field by field. The
    // two must line up 1:1; when they don't — a `place = [{…}]` inline array
    // parses to places with no `[[place]]` tables behind them — nothing is
    // reusable and every entry is rendered fresh.
    let tables: Vec<toml_edit::Table> = doc
        .get("place")
        .and_then(toml_edit::Item::as_array_of_tables)
        .map(|a| a.iter().cloned().collect())
        .unwrap_or_default();
    let parsed = parse_places(existing).unwrap_or_default();
    let (tables, parsed) = if tables.len() == parsed.len() {
        (tables, parsed)
    } else {
        (Vec::new(), Vec::new())
    };

    let source = align(&parsed, places);
    let mut array = toml_edit::ArrayOfTables::new();
    for (want, from) in places.iter().zip(&source) {
        match *from {
            Some(i) => {
                let mut table = tables[i].clone();
                patch_table(&mut table, &parsed[i], want);
                array.push(table);
            }
            None => array.push(new_table(want)),
        }
    }
    space_tables(&mut array, &source);
    doc.as_table_mut()
        .insert("place", toml_edit::Item::ArrayOfTables(array));
    put_header(&mut doc, &header);
    Ok(doc.to_string())
}

/// Match each wanted place onto the index of the `[[place]]` table it should be
/// written into, or `None` for one that needs a fresh table.
///
/// Two passes, because a place's identity and its position are both meaningful
/// and an edit can move either:
///
/// 1. **By name** — a place's name *is* its identity (see [`name_key`]), so
///    this covers updates, additions, deletions and reordering: an unchanged
///    entry finds its own table wherever it moved to.
/// 2. **By position**, for whatever pass 1 left unmatched — which is what a
///    rename looks like from here (the old name disappeared and a new one
///    appeared at the same index). Without it, renaming a place would rebuild
///    its table from scratch and drop the comments inside it.
///
/// A whole-set save that renames *and* reorders in one shot can mis-pair in
/// pass 2. The cost is bounded and cosmetic: that one entry's table is rebuilt
/// from the model, so it loses its comments. No data is lost either way —
/// every field of every wanted place is written.
fn align(parsed: &[Place], want: &[Place]) -> Vec<Option<usize>> {
    let mut claimed = vec![false; parsed.len()];
    let mut source = vec![None; want.len()];
    for (j, place) in want.iter().enumerate() {
        let key = name_key(&place.name);
        if let Some(i) =
            (0..parsed.len()).find(|&i| !claimed[i] && name_key(&parsed[i].name) == key)
        {
            claimed[i] = true;
            source[j] = Some(i);
        }
    }
    for (j, slot) in source.iter_mut().enumerate() {
        if slot.is_none() && j < parsed.len() && !claimed[j] {
            claimed[j] = true;
            *slot = Some(j);
        }
    }
    source
}

/// Patch an existing `[[place]]` table — whose current contents parse to `was`
/// — so it reads back as `want`, touching only the keys that actually change.
///
/// Comparing against `was` (the *parsed* table, defaults applied) rather than
/// against the raw keys is what keeps a save minimal in both directions: a key
/// the file omits and doesn't need stays omitted, and a key whose value is
/// already right keeps its exact spelling, its spacing and its trailing
/// comment. Keys this model doesn't know about are never named here, so they
/// are never touched.
fn patch_table(table: &mut toml_edit::Table, was: &Place, want: &Place) {
    if was.name != want.name {
        set_value(table, "name", toml_edit::Value::from(want.name.as_str()));
    }
    if !same_f64(was.lat, want.lat) {
        set_value(table, "lat", toml_edit::Value::from(want.lat));
    }
    if !same_f64(was.lon, want.lon) {
        set_value(table, "lon", toml_edit::Value::from(want.lon));
    }
    if !same_f64(was.radius_km, want.radius_km) {
        set_value(table, "radius_km", toml_edit::Value::from(want.radius_km));
    }
    if was.ssids != want.ssids {
        set_value(table, "ssids", string_array(&want.ssids));
    }
    if was.match_min != want.match_min {
        set_value(table, "match_min", count(want.match_min));
    }
    match (was.station.as_deref(), want.station.as_deref()) {
        (had, Some(now)) if had != Some(now) => {
            set_value(table, "station", toml_edit::Value::from(now));
        }
        // The schema spells "no departures here" as an absent key, not an
        // empty one — so unsetting a station removes it (and, with it, any
        // comment that was documenting that station specifically).
        (Some(_), None) => {
            table.remove("station");
        }
        _ => {}
    }
    if was.walk_minutes != want.walk_minutes {
        set_value(
            table,
            "walk_minutes",
            toml_edit::Value::from(i64::from(want.walk_minutes)),
        );
    }
    if was.lines != want.lines {
        set_value(table, "lines", string_array(&want.lines));
    }
    if was.directions != want.directions {
        set_value(table, "directions", string_array(&want.directions));
    }
}

/// Build a `[[place]]` table from scratch, for a place with no table to patch.
///
/// Unlike [`patch_table`] this spells every key out (bar an unset `station`,
/// which the schema writes by omission), so an entry the editor added is as
/// discoverable in `$EDITOR` as one from the shipped default, and a reparse
/// yields exactly what was published with no reliance on the defaults.
fn new_table(place: &Place) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    set_value(
        &mut table,
        "name",
        toml_edit::Value::from(place.name.as_str()),
    );
    set_value(&mut table, "lat", toml_edit::Value::from(place.lat));
    set_value(&mut table, "lon", toml_edit::Value::from(place.lon));
    set_value(
        &mut table,
        "radius_km",
        toml_edit::Value::from(place.radius_km),
    );
    set_value(&mut table, "ssids", string_array(&place.ssids));
    set_value(&mut table, "match_min", count(place.match_min));
    if let Some(station) = place.station.as_deref() {
        set_value(&mut table, "station", toml_edit::Value::from(station));
    }
    set_value(
        &mut table,
        "walk_minutes",
        toml_edit::Value::from(i64::from(place.walk_minutes)),
    );
    set_value(&mut table, "lines", string_array(&place.lines));
    set_value(&mut table, "directions", string_array(&place.directions));
    table
}

/// Assign `value` to `key`, keeping the key's own decor — the comment block
/// written *above* it, which documents the field — and dropping only the
/// trailing comment that annotated the old value.
///
/// Replacing the `Item` behind an existing key (rather than re-inserting the
/// key) is what preserves that block. The trailing comment goes deliberately:
/// it annotates the value being replaced, and carrying `# S Schöneweide Bhf`
/// onto a station id that is no longer Schöneweide manufactures exactly the
/// confident-and-wrong label that made #641 invisible for months. A comment
/// *above* the key describes the field and survives; one *beside* the value
/// describes the value and doesn't.
fn set_value(table: &mut toml_edit::Table, key: &str, value: toml_edit::Value) {
    let mut value = value;
    value.decor_mut().set_prefix(" ");
    value.decor_mut().set_suffix("");
    if let Some(existing) = table.get_mut(key) {
        *existing = toml_edit::Item::Value(value);
    } else {
        table.insert(key, toml_edit::Item::Value(value));
    }
}

/// A `Vec<String>` field as a TOML array.
fn string_array(items: &[String]) -> toml_edit::Value {
    toml_edit::Value::Array(items.iter().map(String::as_str).collect())
}

/// `match_min` as a TOML integer.
///
/// `usize` → `i64` cannot realistically fail — it counts listed SSIDs — and a
/// value that did would already be an unsatisfiable fingerprint (see
/// [`warn_unsatisfiable_fingerprints`]). Clamping keeps a save from failing
/// over an input that is nonsense for an unrelated reason.
fn count(n: usize) -> toml_edit::Value {
    toml_edit::Value::from(i64::try_from(n).unwrap_or(i64::MAX))
}

/// Exact equality for a config float.
///
/// `clippy::float_cmp` is about "are these two computed quantities close
/// enough"; the question here is the opposite and much narrower — *would
/// writing this value change the file?* — where bit-for-bit sameness is
/// precisely what's being asked, and both sides are the same number's round
/// trip through the same parser rather than the result of any arithmetic.
#[allow(clippy::float_cmp)]
fn same_f64(a: f64, b: f64) -> bool {
    a == b
}

/// Space out the rebuilt `[[place]]` blocks and pin their render order.
///
/// Two fix-ups, both only reachable when the edit moved something:
/// * `toml_edit` renders tables by their recorded document position, not by
///   their index in the array, so a reorder that doesn't restate the positions
///   silently writes the *old* order back.
/// * A table that was first carries no separating blank line, and one that
///   wasn't carries one. Promoting or demoting a block would otherwise weld it
///   onto its neighbour, or leave a stray blank line under the preamble.
///
/// A table that stayed put keeps its own spacing untouched, so the common case
/// — editing one entry in place — changes nothing but the value.
fn space_tables(array: &mut toml_edit::ArrayOfTables, source: &[Option<usize>]) {
    for (j, table) in array.iter_mut().enumerate() {
        let stayed = source.get(j).copied().flatten() == Some(j);
        if j == 0 {
            if !stayed {
                table.decor_mut().set_prefix("");
            }
        } else if decor_prefix(table.decor()).is_empty() {
            table.decor_mut().set_prefix("\n");
        }
        table.set_position(Some(isize::try_from(j).unwrap_or(isize::MAX)));
    }
}

/// A decor prefix as an owned string, or empty when there is none.
fn decor_prefix(decor: &toml_edit::Decor) -> String {
    decor
        .prefix()
        .and_then(toml_edit::RawString::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Detach the file's opening comment block — the documented preamble the
/// shipped default writes on first run — from whatever `toml_edit` glued it to.
///
/// A document's leading comments become the decor *prefix* of the first thing
/// that follows them: the first `[[place]]` table when the file has one, and
/// the document's trailing decor when it holds nothing but comments. Either
/// way the preamble belongs to the *file*, not to whichever place happens to
/// be listed first, so deleting or reordering places must not carry it off.
/// [`put_header`] puts it back at the top afterwards.
fn take_header(doc: &mut toml_edit::DocumentMut) -> String {
    if let Some(first) = doc
        .get_mut("place")
        .and_then(toml_edit::Item::as_array_of_tables_mut)
        .and_then(|a| a.get_mut(0))
    {
        let header = decor_prefix(first.decor());
        first.decor_mut().set_prefix("");
        return header;
    }
    let header = doc.trailing().as_str().unwrap_or_default().to_owned();
    doc.set_trailing("");
    header
}

/// Put back what [`take_header`] detached, in front of whatever now sits at the
/// top — the first `[[place]]` table, or (when the edit left no places at all)
/// the document's trailing decor, which is the only place a comment can live in
/// a file with no tables.
fn put_header(doc: &mut toml_edit::DocumentMut, header: &str) {
    if header.is_empty() {
        return;
    }
    if let Some(first) = doc
        .get_mut("place")
        .and_then(toml_edit::Item::as_array_of_tables_mut)
        .and_then(|a| a.get_mut(0))
    {
        let rest = decor_prefix(first.decor());
        first.decor_mut().set_prefix(format!("{header}{rest}"));
        return;
    }
    let rest = doc.trailing().as_str().unwrap_or_default().to_owned();
    doc.set_trailing(format!("{header}{rest}"));
}

/// What `places.toml` currently holds, from the point of view of a writer that
/// is about to replace it.
///
/// This is [`load_places`]'s classification with its two "can't tell" arms
/// split back out. `load_places` folds an unreadable and an unparseable file
/// into the same silent fallback as a missing one — correct for a *reader*
/// (the shell still needs somewhere to be), catastrophic for a *writer*, which
/// would then render that fallback over the bytes it couldn't read.
#[derive(Debug)]
enum OnDisk {
    /// No file yet: a save creates it, and there is nothing to lose.
    Absent,
    /// A parseable file. A config that yields zero places is reported as
    /// [`builtin_default`], because that is what [`load_places`] makes of it —
    /// so "disk" and "memory" are compared in the same units.
    Places(Vec<Place>),
    /// The file is there but its contents can't be established. Never write
    /// over this.
    Unknown(String),
}

/// Classify the current `places.toml` for a writer — see [`OnDisk`].
fn read_on_disk(path: &Path) -> OnDisk {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return OnDisk::Absent,
        Err(e) => return OnDisk::Unknown(e.to_string()),
    };
    match parse_places(&text) {
        Ok(places) if places.is_empty() => OnDisk::Places(builtin_default()),
        Ok(places) => OnDisk::Places(places),
        Err(e) => OnDisk::Unknown(e),
    }
}

/// Patch + atomically write `places` to `path`, so tests can drive it into a
/// tempdir instead of the user's real config.
///
/// The existing file is the document being edited (see [`render_places`]), so
/// everything the edit doesn't touch survives it. When there is no file yet —
/// or it can't be read — the *shipped default* stands in as that document, so
/// a config this API creates is as self-documenting as the one first run
/// writes: it keeps the whole documented preamble, and the first place lands in
/// the default's commented `[[place]]` scaffold rather than in a bare table.
///
/// The write itself is [`config_file::write_atomic`] — the crate's one copy of
/// the scratch-file + `fsync` + `rename(2)` algorithm (#739), which is also
/// what makes it symlink- and permission-safe. `places.toml` picks
/// [`config_file::Durability::FsyncParent`], unlike the click-driven toggles
/// behind `config_file::write`: it is user-authored data, saved rarely and
/// deliberately through an API that reports success, so an acknowledged save
/// must survive a power cut. `config_file` gives us the real `io::Error` here,
/// which is what [`PlacesError::Write`] needs to carry.
///
/// # Errors
/// [`PlacesError::Encode`] when the file that is there doesn't parse — a patch
/// has nothing to patch, and replacing bytes we can't account for is precisely
/// what [`edit`]'s guard exists to prevent. Reaching this through [`edit`] is
/// impossible (it refuses first, with the more specific
/// [`PlacesError::Unreadable`]). [`PlacesError::Write`] when the atomic write
/// itself fails; the previous config is then untouched.
pub fn persist_to(path: &Path, places: &[Place]) -> Result<(), PlacesError> {
    let existing = std::fs::read_to_string(path).unwrap_or_else(|_| DEFAULT_CONFIG.to_owned());
    let body = render_places(&existing, places)?;
    config_file::write_atomic(path, &body, config_file::Durability::FsyncParent)
        .map_err(|e| PlacesError::Write(e.to_string()))
}

/// Establish that `path` still holds `base` — i.e. that an edit computed
/// against `base` is safe to write over what is actually there.
///
/// **This is a data-loss guard, not a nicety.** `base` is whatever its caller
/// last read, and there are two ways that can be a lie:
///
/// * The file might not be readable or parseable at all. Both readers here
///   ([`load_places`] and the control center) fall back to the *built-in
///   default* in that case, and nothing corrects it afterwards — the shell
///   loads once and its watcher is mtime-gated. Writing that base back would
///   render one built-in place over however many the user had hand-configured,
///   atomically and with no backup ([`PlacesError::Unreadable`]).
/// * Or the file might have moved on. `places.toml`'s whole premise is that it
///   stays hand-editable, so an `$EDITOR` save can land between a read and a
///   write; applying an edit computed against the old contents would write the
///   hand edit away ([`PlacesError::ChangedOnDisk`]). The caller re-reads and
///   retries — the shell's watcher republishes within a poll tick, and the
///   control center offers a reload.
///
/// It is a *narrowing*, not a cure: nothing stops a third party writing
/// between this check and the `rename(2)` microseconds later. Closing that
/// needs an `O_EXCL` lock file every editor honours, which no editor does.
///
/// # Errors
/// [`PlacesError::Unreadable`] / [`PlacesError::ChangedOnDisk`], as above.
pub fn check_base(path: &Path, base: &[Place]) -> Result<(), PlacesError> {
    match read_on_disk(path) {
        // Nothing on disk to lose, or it says exactly what we think it says.
        OnDisk::Absent => Ok(()),
        OnDisk::Places(disk) if disk == base => Ok(()),
        OnDisk::Places(_) => Err(PlacesError::ChangedOnDisk),
        OnDisk::Unknown(why) => {
            tracing::warn!(path = %path.display(), reason = %why, "places: refusing to save over a config we can't read back");
            Err(PlacesError::Unreadable(why))
        }
    }
}

/// The whole-set save an out-of-process editor makes: check what is on disk
/// against the `base` the editor started from, canonicalise and validate the
/// new set, then patch it into the existing document and replace the file
/// atomically.
///
/// This is what `trollshell-control-center`'s places editor calls. The shell's
/// own editing API can't use it as-is — it has to publish the new set on its
/// reactive handle inside the same critical section — so it composes the same
/// [`check_base`] → [`validate`] → [`persist_to`] steps itself. Both therefore
/// go through one validator and one writer, which is the property that makes
/// "two editors, one file" safe to have at all.
///
/// # Errors
/// Whatever [`check_base`], [`validate`] or [`persist_to`] reject.
pub fn save_to(path: &Path, base: &[Place], next: Vec<Place>) -> Result<(), PlacesError> {
    check_base(path, base)?;
    let next: Vec<Place> = next.into_iter().map(normalize).collect();
    validate(&next)?;
    persist_to(path, &next)?;
    warn_unsatisfiable_fingerprints(&next);
    tracing::info!(count = next.len(), "places: config saved");
    Ok(())
}

/// [`save_to`] against the user's real `~/.config/trollshell/places.toml`.
///
/// # Errors
/// [`PlacesError::NoConfigPath`] when `$HOME` is unset, else as [`save_to`].
pub fn save(base: &[Place], next: Vec<Place>) -> Result<(), PlacesError> {
    let path = config_path().ok_or(PlacesError::NoConfigPath)?;
    save_to(&path, base, next)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    fn default_places() -> Vec<Place> {
        parse_places(DEFAULT_CONFIG).expect("default config parses")
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
    fn render_round_trips_every_field() {
        let places = vec![
            full_place("Schöneweide"),
            Place::new("Office", -33.87, 151.21),
        ];
        let text = render_places("", &places).expect("renders");
        assert_eq!(parse_places(&text).expect("reparses"), places);
    }

    #[test]
    fn render_omits_station_when_unset() {
        let places = vec![Place::new("Nowhere", 0.0, 0.0)];
        let text = render_places("", &places).expect("renders");
        assert!(
            !text.contains("station"),
            "an unset station must be absent, not empty: {text}"
        );
        assert_eq!(parse_places(&text).expect("reparses")[0].station, None);
    }

    /// Unsetting a station on an entry that *had* one removes the key rather
    /// than writing an empty string — the schema spells "no departures here"
    /// by omission, and an empty `station = ""` would be a request for a
    /// station whose id is the empty string.
    #[test]
    fn unsetting_a_station_removes_the_key() {
        let before = "[[place]]\nname = \"Home\"\nlat = 1.0\nlon = 2.0\nstation = \"900192001\"\n";
        let mut want = parse_places(before).expect("parses");
        want[0].station = None;
        let text = render_places(before, &want).expect("renders");
        assert!(!text.contains("station"), "got {text}");
        assert_eq!(parse_places(&text).expect("reparses"), want);
    }

    #[test]
    fn default_config_round_trips() {
        let places = default_places();
        let text = render_places("", &places).expect("renders");
        assert_eq!(parse_places(&text).expect("reparses"), places);
        // …and patched back into its own document, which is the real path.
        let patched = render_places(DEFAULT_CONFIG, &places).expect("renders");
        assert_eq!(
            patched, DEFAULT_CONFIG,
            "re-saving the shipped default unchanged must be a byte-for-byte no-op"
        );
    }

    /// The headline guarantee: parse → mutate → render → parse is the set we
    /// asked for, field for field, ordering included.
    #[test]
    fn parse_mutate_render_parse_is_stable() {
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

        let text = render_places(DEFAULT_CONFIG, &retuned).expect("renders");
        let reparsed = parse_places(&text).expect("reparses");
        assert_eq!(reparsed, retuned);
        assert_eq!(reparsed[0].name, "Home", "rename keeps position");
        assert_eq!(reparsed[1].walk_minutes, 4);
        assert_eq!(reparsed[1].station, None);

        let without_office = removed(&reparsed, "OFFICE").expect("removes");
        assert_eq!(
            parse_places(&render_places(&text, &without_office).expect("renders"))
                .expect("reparses"),
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
        let text = render_places("", std::slice::from_ref(&clean)).expect("renders");
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

    // ── Write path: atomic file replacement (#640 / #739) ───────────────────
    //
    // #739 folded `places`' own copy of this algorithm into
    // `config_file::write_atomic`. These cases stayed here because they assert
    // through a `persist_to`-shaped local helper (`write_atomic` below, not to
    // be confused with `config_file::write_atomic`) against `places.toml`-
    // flavoured paths and bodies, and two of them drive the real `persist_to`
    // directly. `config_file`'s own suite covers the algorithm in the
    // abstract, including both `Durability` arms
    // (`both_durability_choices_write_the_same_file`); what pins that
    // `persist_to` actually takes the `FsyncParent` branch is
    // `persist_to_pins_the_fsync_parent_durability_choice`, further down.

    /// Leftover scratch files (they're dotfiles named after their target).
    fn scratch_files(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("dir readable")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect()
    }

    /// Exactly the call [`persist_to`] makes, minus the TOML rendering, so the
    /// cases below can assert on a body they chose. The two #739 regressions go
    /// through the real `persist_to` instead — see further down.
    fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
        config_file::write_atomic(path, body, config_file::Durability::FsyncParent)
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

    /// #739: a save must write *through* a symlinked target, not replace the
    /// link with a regular file. A dotfiles-repo `places.toml`
    /// (stow/chezmoi/a plain symlink) must survive a programmatic save.
    ///
    /// Driven through the real [`persist_to`] rather than the shared core
    /// directly, so it also pins that `places` keeps *using* the safe path —
    /// the original defect was a private copy of the algorithm that skipped the
    /// `canonicalize`, and a future one would look the same from here.
    #[test]
    fn persist_to_writes_through_a_symlinked_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real_dir = dir.path().join("real");
        std::fs::create_dir(&real_dir).expect("mkdir real");
        let real_file = real_dir.join("places.toml");
        std::fs::write(&real_file, "# kept\n\n").expect("seed");
        let link = dir.path().join("places.toml");
        std::os::unix::fs::symlink(&real_file, &link).expect("symlink");

        persist_to(&link, &[full_place("Home")]).expect("persists");

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("link still exists")
                .file_type()
                .is_symlink(),
            "the symlink must survive the save, not get replaced by a regular file"
        );
        let text = std::fs::read_to_string(&real_file).expect("real file readable");
        assert!(
            text.starts_with("# kept\n"),
            "the real file the symlink points at must receive the save: {text:?}"
        );
        assert_eq!(
            parse_places(&text).expect("reparses"),
            vec![full_place("Home")],
            "the real file the symlink points at must receive the new set"
        );
        assert!(
            scratch_files(&real_dir).is_empty(),
            "no scratch file left in the real file's directory"
        );
    }

    /// #739: an existing target's mode must be applied to the scratch file
    /// *before* the body is written, not after — so a `0600` config's
    /// contents never sit in a briefly umask-default (typically world- or
    /// group-readable) file while being written. Racy but never flaky (like
    /// `config_file`'s `a_reader_never_observes_a_partial_file`): scheduling
    /// luck decides whether the watcher thread samples the scratch file
    /// mid-write, so an unlucky run proves less, but a lucky one under the
    /// old post-write-`chmod` ordering catches the loose window directly.
    ///
    /// Through [`persist_to`] for the same reason as the symlink case above.
    #[test]
    fn persist_to_applies_the_targets_mode_before_the_body_is_written() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("secretish.toml");
        // Valid TOML, because `persist_to` now *patches* the file it replaces
        // and refuses one it can't parse. The body is irrelevant to what this
        // test measures (the scratch file's mode); only its existence and its
        // permissions are.
        std::fs::write(&target, "# old\n").expect("seed");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        let stop = Arc::new(AtomicBool::new(false));
        let violation = Arc::new(AtomicBool::new(false));
        let watcher = std::thread::spawn({
            let (dir, stop, violation) = (
                dir.path().to_path_buf(),
                Arc::clone(&stop),
                Arc::clone(&violation),
            );
            move || {
                while !stop.load(Ordering::Relaxed) {
                    for name in scratch_files(&dir) {
                        if let Ok(meta) = std::fs::metadata(dir.join(&name)) {
                            let mode = meta.permissions().mode() & 0o777;
                            if mode != 0o600 {
                                violation.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                    std::thread::yield_now();
                }
            }
        });

        for i in 0..64 {
            persist_to(&target, &[Place::new(format!("P{i}"), 1.0, 2.0)]).expect("persists");
        }
        stop.store(true, Ordering::Relaxed);
        watcher.join().expect("watcher thread");

        assert!(
            !violation.load(Ordering::Relaxed),
            "the scratch file must never be observed at other than the target's mode"
        );
    }

    /// Pins that [`persist_to`] actually takes the [`Durability::FsyncParent`]
    /// branch inside `config_file::write_atomic`, not just that it *compiles*
    /// against that variant. The `fsync` itself can't be observed in-process
    /// (see [`config_file::fsync_parent_attempts`]'s doc), but whether the
    /// branch fired can: this fails if the `matches!` guard in `write_atomic`
    /// is inverted, and it fails just as surely if `persist_to` is edited to
    /// pass `Durability::FileOnly`. Neither of those trips any other test —
    /// see #767's review thread.
    #[test]
    fn persist_to_pins_the_fsync_parent_durability_choice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("places.toml");
        let before = config_file::fsync_parent_attempts();

        persist_to(&target, &[full_place("Home")]).expect("persists");

        assert_eq!(
            config_file::fsync_parent_attempts(),
            before + 1,
            "persist_to must take the Durability::FsyncParent branch in \
             write_atomic exactly once per call"
        );
    }

    // ── Write path: comment preservation + persist (#640) ───────────────────

    /// A hand-written `places.toml` exercising every kind of formatting a save
    /// has to survive: a preamble, per-key comment blocks, a trailing comment
    /// beside a value, hand-chosen (non-canonical) key ordering, omitted keys,
    /// a key the typed model doesn't know about, and an unrelated top-level
    /// table. The fixture for the byte-for-byte tests below.
    const HAND_WRITTEN: &str = "\
# my places file
# station ids: https://v6.bvg.transport.rest/locations?query=

[[place]]
# the pretty name, shown in the bar
name = \"Home\"
# picked off the neighbours — I never see these at the office
ssids = [\"FRITZ!Box 7590\", \"Telekom-ABC\"]
match_min = 2
lat = 52.4556
lon = 13.5085
station = \"900192001\" # S Schöneweide Bhf
walk_minutes = 10
notes = \"the balcony one\"

[[place]]
# work — no fingerprint captured yet
name = \"Office\"
lat = 52.5200
lon = 13.4050
radius_km = 4.0

[scratch]
mine = true
";

    /// The whole point of #640's settlement: `places.toml` has two permanent
    /// authors, so a programmatic save must be a patch. Editing one entry
    /// leaves every byte the edit didn't ask for exactly where it was —
    /// preamble, per-key comments, hand-chosen key order, the unrelated
    /// `[scratch]` table, and the entry that wasn't touched.
    #[test]
    fn a_save_edits_one_entry_and_leaves_every_other_byte_alone() {
        let places = parse_places(HAND_WRITTEN).expect("parses");
        let edited = updated(
            &places,
            "Home",
            Place {
                walk_minutes: 4,
                ..places[0].clone()
            },
        )
        .expect("updates");

        let text = render_places(HAND_WRITTEN, &edited).expect("renders");

        assert_eq!(
            text,
            HAND_WRITTEN.replace("walk_minutes = 10", "walk_minutes = 4"),
            "only the edited value may move"
        );
        assert_eq!(parse_places(&text).expect("reparses"), edited);
    }

    /// The `#[serde(default)]` schema *ignores* keys it doesn't model, so a
    /// hand-added annotation reads back fine — and the old re-render then
    /// silently deleted it on the next save, along with any other top-level
    /// table in the file. A patch can't: it never names a key it doesn't
    /// intend to change. Validation is unchanged (it doesn't see these at all),
    /// which is the point — unknown content is preserved on write, never
    /// rejected and never destroyed.
    #[test]
    fn a_save_preserves_keys_and_tables_the_model_does_not_know_about() {
        let places = parse_places(HAND_WRITTEN).expect("parses");
        // Edit the *other* entry, so nothing about Home is even considered.
        let edited = updated(
            &places,
            "Office",
            Place {
                radius_km: 6.5,
                ..places[1].clone()
            },
        )
        .expect("updates");

        let text = render_places(HAND_WRITTEN, &edited).expect("renders");

        assert!(
            text.contains("notes = \"the balcony one\""),
            "an unmodelled key inside a [[place]] must survive: {text}"
        );
        assert!(
            text.contains("[scratch]\nmine = true"),
            "an unrelated top-level table must survive: {text}"
        );
        assert_eq!(
            text,
            HAND_WRITTEN.replace("radius_km = 4.0", "radius_km = 6.5"),
            "only the edited value may move"
        );
    }

    /// A comment written *above* a key documents the field, so it survives a
    /// value change. A comment written *beside* a value documents that value,
    /// so replacing the value drops it rather than carrying a now-false label
    /// onto the new one — which is exactly the confidently-wrong station label
    /// that hid #641.
    #[test]
    fn changing_a_value_keeps_the_key_comment_and_drops_the_value_comment() {
        let places = parse_places(HAND_WRITTEN).expect("parses");
        let edited = updated(
            &places,
            "Home",
            Place {
                station: Some("900193002".into()),
                ..places[0].clone()
            },
        )
        .expect("updates");

        let text = render_places(HAND_WRITTEN, &edited).expect("renders");

        assert!(text.contains("station = \"900193002\"\n"), "got {text}");
        assert!(
            !text.contains("S Schöneweide Bhf"),
            "the old value's label must not survive onto a new id: {text}"
        );
        assert!(
            text.contains("# the pretty name, shown in the bar"),
            "a key's own comment block documents the field and must survive: {text}"
        );
        assert!(text.contains("# picked off the neighbours"));
    }

    /// Adding a place appends a fully-spelled-out block and touches nothing
    /// above it.
    #[test]
    fn adding_a_place_appends_and_leaves_the_file_above_it_untouched() {
        let places = parse_places(HAND_WRITTEN).expect("parses");
        let edited = added(&places, Place::new("Gym", 52.49, 13.42)).expect("adds");

        let text = render_places(HAND_WRITTEN, &edited).expect("renders");

        assert!(
            text.starts_with(HAND_WRITTEN.trim_end_matches("[scratch]\nmine = true\n")),
            "the existing blocks must be untouched: {text}"
        );
        assert_eq!(parse_places(&text).expect("reparses"), edited);
        let gym = &parse_places(&text).expect("reparses")[2];
        assert_eq!(gym.name, "Gym");
        assert!(
            same_f64(gym.radius_km, default_radius_km()),
            "a fresh block spells out the defaults rather than omitting them"
        );
    }

    /// Deleting the *first* place must not take the file's preamble with it —
    /// `toml_edit` glues a document's opening comments to whichever table comes
    /// first, so the naive patch loses the whole documented schema the moment
    /// someone removes the shipped default place.
    #[test]
    fn deleting_the_first_place_keeps_the_preamble_and_the_survivors() {
        let places = parse_places(HAND_WRITTEN).expect("parses");
        let edited = removed(&places, "home").expect("removes");

        let text = render_places(HAND_WRITTEN, &edited).expect("renders");

        assert!(
            text.starts_with("# my places file\n# station ids:"),
            "got {text}"
        );
        assert!(
            !text.contains("FRITZ!Box"),
            "the deleted block must be gone"
        );
        assert!(text.contains("# work — no fingerprint captured yet"));
        assert!(text.contains("[scratch]"));
        assert_eq!(parse_places(&text).expect("reparses"), edited);
    }

    /// A rename is the case name-matching alone can't see, so the writer falls
    /// back to position for it. Without that fallback the entry's table would
    /// be rebuilt from the model and every comment in it lost.
    #[test]
    fn renaming_a_place_keeps_the_comments_inside_its_block() {
        let places = parse_places(HAND_WRITTEN).expect("parses");
        let edited = renamed(&places, "Home", "Zuhause").expect("renames");

        let text = render_places(HAND_WRITTEN, &edited).expect("renders");

        assert_eq!(
            text,
            HAND_WRITTEN.replace("name = \"Home\"", "name = \"Zuhause\""),
            "a rename may move exactly one value"
        );
    }

    /// Reordering is what a whole-set save from an editor produces, and it is
    /// where `toml_edit`'s by-position rendering bites: without restating the
    /// positions the old order is silently written back.
    #[test]
    fn reordering_rewrites_the_order_and_keeps_each_block_intact() {
        let places = parse_places(HAND_WRITTEN).expect("parses");
        let swapped = vec![places[1].clone(), places[0].clone()];

        let text = render_places(HAND_WRITTEN, &swapped).expect("renders");
        let reparsed = parse_places(&text).expect("reparses");

        assert_eq!(reparsed, swapped, "the order on disk must be the new one");
        assert!(
            text.starts_with("# my places file"),
            "preamble stays on top: {text}"
        );
        assert!(text.contains("# work — no fingerprint captured yet"));
        assert!(text.contains("# picked off the neighbours"));
        assert!(text.contains("notes = \"the balcony one\""));
        // And a second save of the same set changes nothing further — no
        // blank-line creep from repeatedly re-spacing the blocks.
        assert_eq!(render_places(&text, &swapped).expect("renders"), text);
    }

    /// The shipped default's preamble is what makes the schema discoverable,
    /// and it survives every shape of edit — including the one that leaves no
    /// `[[place]]` table for it to hang off at all.
    #[test]
    fn the_preamble_survives_every_shape_of_edit() {
        let start = default_places();
        for (label, next) in [
            ("add", added(&start, full_place("Office")).expect("adds")),
            (
                "rename",
                renamed(&start, "Schöneweide", "Home").expect("renames"),
            ),
            ("empty", removed(&start, "Schöneweide").expect("removes")),
        ] {
            let text = render_places(DEFAULT_CONFIG, &next).expect("renders");
            assert!(
                text.starts_with("# trollshell places"),
                "{label}: the documented preamble must survive: {text}"
            );
            assert!(
                text.contains("trollshell --scan-aps"),
                "{label}: …all of it, not just the first line"
            );
            assert_eq!(parse_places(&text).expect("reparses"), next, "{label}");
            // …and it is still there after a save that starts from that file.
            let again = render_places(&text, &next).expect("renders");
            assert_eq!(again, text, "{label}: a no-op save must change nothing");
        }
    }

    /// A file the user stripped down to nothing but places stays that way.
    #[test]
    fn a_file_without_a_preamble_does_not_grow_one() {
        let bare = "[[place]]\nname = \"Home\"\nlat = 1.0\nlon = 2.0\n";
        let places = parse_places(bare).expect("parses");
        let edited = added(&places, Place::new("Office", 3.0, 4.0)).expect("adds");
        let text = render_places(bare, &edited).expect("renders");
        assert!(text.starts_with("[[place]]"), "got {text}");
    }

    #[test]
    fn rendering_refuses_a_document_that_is_not_valid_toml() {
        let err = render_places("[[place]]\nname = \n", &[]).expect_err("must refuse");
        assert!(matches!(err, PlacesError::Encode(_)), "got {err:?}");
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

    /// The same guarantee as the `render_places` tests, but through the real
    /// file path — tmp file, `fsync`, `rename(2)` and all — because that is
    /// what an editor actually calls.
    #[test]
    fn persist_to_patches_the_file_instead_of_rewriting_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("places.toml");
        std::fs::write(&target, HAND_WRITTEN).expect("seed");

        let places = parse_places(HAND_WRITTEN).expect("parses");
        let next = updated(
            &places,
            "Office",
            Place {
                walk_minutes: 7,
                ..places[1].clone()
            },
        )
        .expect("updates");
        persist_to(&target, &next).expect("persists");

        let text = std::fs::read_to_string(&target).expect("readable");
        assert!(text.starts_with("# my places file"));
        assert!(text.contains("# picked off the neighbours"));
        assert!(text.contains("station = \"900192001\" # S Schöneweide Bhf"));
        assert!(text.contains("notes = \"the balcony one\""));
        assert!(text.contains("[scratch]"));
        assert_eq!(parse_places(&text).expect("reparses"), next);
    }

    // ── Write path: never write over a config we can't account for (#640) ───

    #[test]
    fn read_on_disk_splits_out_the_two_arms_load_places_papers_over() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("places.toml");

        assert!(matches!(read_on_disk(&target), OnDisk::Absent));

        // A config that yields no places is reported in the same units memory
        // holds it in — what `load_places` makes of it, i.e. the default.
        std::fs::write(&target, "place = []\n").expect("seed");
        assert!(matches!(read_on_disk(&target), OnDisk::Places(p) if p == builtin_default()));
        std::fs::write(&target, "# only comments\n").expect("seed");
        assert!(matches!(read_on_disk(&target), OnDisk::Places(p) if p == builtin_default()));

        // A real config comes back as itself.
        std::fs::write(
            &target,
            "[[place]]\nname = \"Home\"\nlat = 1.0\nlon = 2.0\n",
        )
        .expect("seed");
        assert!(
            matches!(read_on_disk(&target), OnDisk::Places(p) if p.len() == 1 && p[0].name == "Home")
        );

        // And the two `load_places` collapses into a silent fallback stay
        // distinguishable here: unparseable, and undecodable.
        std::fs::write(&target, "[[place]]\nname = \"Home\"\nlat = \n").expect("seed");
        assert!(matches!(read_on_disk(&target), OnDisk::Unknown(_)));
        std::fs::write(&target, [0xff, 0xfe]).expect("seed");
        assert!(matches!(read_on_disk(&target), OnDisk::Unknown(_)));
    }

    /// Four hand-configured places and a typo. Neither editor can parse that,
    /// so both fall back to the *built-in default* in memory — and nothing
    /// corrects it afterwards. A read-memory-modify-write save would therefore
    /// render one default place over the four, atomically and with no backup.
    /// [`check_base`] is what stops it, and [`save_to`] is what proves the
    /// out-of-process editor gets that protection too.
    #[test]
    fn a_save_refuses_to_overwrite_a_config_that_cannot_be_accounted_for() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("places.toml");
        let broken = concat!(
            "# my places\n",
            "[[place]]\nname = \"Home\"\nlat = 52.5\nlon = 13.4\n",
            "[[place]]\nname = \"Office\"\nlat = 52.6\nlon = 13.5\n",
            "[[place]]\nname = \"Gym\"\nlat = 52.7\nlon = 13.6\n",
            "[[place]]\nname = \"Cabin\"\nlat = \nlon = 13.7\n",
        );
        std::fs::write(&target, broken).expect("seed");

        // The precondition that makes this dangerous: what a reader has in hand
        // is the built-in default, not the user's four places.
        let base = builtin_default();
        assert!(matches!(
            check_base(&target, &base),
            Err(PlacesError::Unreadable(_))
        ));
        assert!(matches!(
            save_to(&target, &base, vec![Place::new("Office", 52.5, 13.4)]),
            Err(PlacesError::Unreadable(_))
        ));
        assert_eq!(
            std::fs::read_to_string(&target).expect("readable"),
            broken,
            "the user's four places must survive byte for byte"
        );

        // The same guard for a file we can't even decode.
        std::fs::write(&target, [0xff, 0xfe, b'[']).expect("seed");
        assert!(matches!(
            save_to(&target, &base, vec![Place::new("Office", 52.5, 13.4)]),
            Err(PlacesError::Unreadable(_))
        ));
        assert_eq!(
            std::fs::read(&target).expect("readable"),
            [0xff, 0xfe, b'[']
        );
    }

    /// An `$EDITOR` save landing between the editor's read and its write: the
    /// file parses, but to a different set than the edit was computed against.
    /// Applying it would write the hand edit away, so it is refused until the
    /// editor re-reads.
    #[test]
    fn a_save_refuses_a_base_the_file_has_moved_on_from() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("places.toml");
        std::fs::write(&target, DEFAULT_CONFIG).expect("seed");
        let base = parse_places(DEFAULT_CONFIG).expect("parses");

        let hand_edited = "[[place]]\nname = \"Cabin\"\nlat = 1.0\nlon = 2.0\n";
        std::fs::write(&target, hand_edited).expect("out-of-process edit");

        let mut next = base.clone();
        next.push(Place::new("Office", 52.5, 13.4));
        assert_eq!(
            save_to(&target, &base, next.clone()),
            Err(PlacesError::ChangedOnDisk)
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("readable"),
            hand_edited,
            "the hand edit must survive"
        );

        // Re-read, and the same edit lands — on the hand-edited base, not over
        // it.
        let base = parse_places(hand_edited).expect("parses");
        let mut next = base.clone();
        next.push(Place::new("Office", 52.5, 13.4));
        save_to(&target, &base, next.clone()).expect("retry lands");
        assert_eq!(load_from(&target), next);
    }

    /// The whole-set save canonicalises its input the same way a reparse would,
    /// and rejects the same things [`validate`] does — so an editor handing us
    /// a padded name and blank list entries can't put memory and file out of
    /// step, and one handing us nonsense can't write it.
    #[test]
    fn save_to_normalizes_and_validates_what_an_editor_hands_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("places.toml");

        let messy = Place {
            name: "  Office  ".into(),
            station: Some("   ".into()),
            ssids: vec![String::new(), "wifi".into()],
            ..full_place("ignored")
        };
        save_to(&target, &[], vec![messy]).expect("saves");
        let saved = load_from(&target);
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "Office");
        assert_eq!(saved[0].station, None);
        assert_eq!(saved[0].ssids, ["wifi"]);

        // …and a rejected save leaves the file exactly as it was.
        let before = std::fs::read_to_string(&target).expect("readable");
        assert_eq!(
            save_to(
                &target,
                &saved,
                vec![saved[0].clone(), Place::new("office", 1.0, 2.0)]
            ),
            Err(PlacesError::DuplicateName("office".to_string()))
        );
        assert!(matches!(
            save_to(&target, &saved, vec![Place::new("Moon", 1000.0, 0.0)]),
            Err(PlacesError::Latitude { .. })
        ));
        assert_eq!(std::fs::read_to_string(&target).expect("readable"), before);
    }

    /// `load_places` against an explicit path, so the tests above can assert on
    /// a tempdir without mutating `$HOME`.
    fn load_from(path: &Path) -> Vec<Place> {
        parse_places(&std::fs::read_to_string(path).expect("readable")).expect("parses")
    }
}
