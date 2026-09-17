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
//!
//! # The nix base layer (#1227 item 2)
//!
//! Since #1227 `places.toml` is **read** through the #866/#868 layering like
//! every other family — `$XDG_CONFIG_DIRS/trollshell/places.toml` (nix's,
//! read-only) under `~/.config/trollshell/places.toml` (yours) under
//! [`DEFAULT_CONFIG`] — and still **written** to exactly one file, the
//! overlay. [`load_layered`] is the reader; [`layer_paths`] is what it reads.
//!
//! Three things about that are specific to `places` and are decided here
//! rather than in [`crate::subsystem`], which this module deliberately does
//! not go through (it keeps its own byte-pinned writer — see
//! [`render_places`] and `tests/places_byte_identical.rs`):
//!
//! 1. **The overlay slot is [`config_path`], not
//!    [`crate::xdg::Env::overlay_path`].** `places` predates the layering and
//!    its writer has always resolved `$HOME/.config/trollshell/places.toml`
//!    (via [`crate::file::path`]); a reader that took `$XDG_CONFIG_HOME`
//!    instead would, on a box where the two differ, read a file the writer
//!    never writes. Only the *base* slots come from `XDG_CONFIG_DIRS`
//!    ([`crate::xdg::Env::base_config_layers`]).
//!
//! 2. **`place` is an array, and rule 3 replaces arrays whole.** So a base
//!    layer that declares places supplies the *whole* list, an overlay that
//!    declares places *replaces* the whole list, and there is no merging of
//!    individual `[[place]]` blocks across layers — the same semantics every
//!    other array key in this workspace has. The consequence for the two
//!    editors is that a save writes the **whole merged set** into the overlay:
//!    "the control-center edited one place" becomes "the overlay now carries
//!    the full list", which is the only thing array-replace can mean.
//!
//! 3. **A locked `place` makes the set read-only, and the writer says so.**
//!    Under #1331's rule the union of every base layer's `_locked` binds the
//!    overlay, so `programs.trollshell.config.places.place` renders
//!    `_locked = ["place"]` and the next load would refuse an overlay array.
//!    Rather than let a save be silently reverted one tick later,
//!    [`check_unlocked`] refuses it up front with [`PlacesError::Locked`] —
//!    the same invariant `subsystem::save_overlay_to_locked` has for the
//!    families that do go through [`crate::subsystem`]: a value the reader
//!    would reject is never written.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::file as config_file;
use crate::subsystem::Finding;
use crate::{merge, subsystem, xdg};

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
#
# That URL is also where the fetch reads by default: BVG (Berlin) is the
# transport.rest backend a station id resolves against unless `[departures]`
# below names a different one (#1124) — outside Berlin, set that first.
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

# Departures backend (optional, #1124): which transport.rest deployment the
# fetch talks to. Absent means bvg — Berlin only. Short names: bvg, vbb, db;
# or paste a full https://... base URL for another transport.rest deployment
# (e.g. hvv, oebb). Not per-place — the whole shell fetches from one backend.
#
# VBB and BVG share the VBB station id space, but DB uses its own EVA ids, so
# switching backend usually means finding a new `station` id from that
# backend's own https://v6.<name>.transport.rest/locations?query=<name> route
# rather than reusing the one above.
# [departures]
# endpoint = "vbb"
"#;

/// A configured place: location identity, Wi-Fi fingerprint, and optional
/// transit config. One `[[place]]` block in `places.toml`.
///
/// Public (and field-public) since #640: it is both what `hytte_services::places::configured` hands
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

impl ConfigFile {
    /// The file's raw `[[place]]` blocks as [`Place`]s. Split out of
    /// [`parse_places`] because #1227 item 2's layered reader arrives with a
    /// merged [`toml::Table`] rather than with text and must not grow a second
    /// copy of this mapping.
    fn into_places(self) -> Vec<Place> {
        self.place
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
            .collect()
    }
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

/// This family's name in the XDG layering — `places.toml` in every layer, the
/// same stem [`crate::subsystem::Subsystem::NAME`] would carry if `places`
/// went through that trait (it does not; see the module docs).
const SUBSYSTEM: &str = "places";

/// The dotted `_locked` path naming the whole `[[place]]` array.
///
/// One path, not one per place: rule 3 replaces arrays whole, so a locked
/// array is atomic and nix's `lockedLeafPaths` renders exactly this string for
/// `programs.trollshell.config.places.place`.
pub const PLACE_KEY: &str = "place";

/// The dotted `_locked` path naming `[departures].endpoint` (#1124).
pub const ENDPOINT_KEY: &str = "departures.endpoint";

/// The nix-written base layers for `places.toml`, lowest precedence first.
/// Paths, existing or not — [`read_base_layers`] skips the missing ones.
#[must_use]
pub fn base_layer_paths() -> Vec<PathBuf> {
    xdg::base_config_layers(SUBSYSTEM)
}

/// Every layer [`load_layered`] consults, lowest precedence first: the base
/// layers, then the overlay ([`config_path`]).
///
/// [`DEFAULT_CONFIG`] is not in here — it is not a file. [`ConfigWatcher`]
/// polls exactly this list, so a `nixos-rebuild` that moves the base layer is
/// picked up by the same two-second tick a hand edit is.
#[must_use]
pub fn layer_paths() -> Vec<PathBuf> {
    let mut paths = base_layer_paths();
    paths.extend(config_path());
    paths
}

/// Read every base layer that exists, lowest precedence first, as
/// `(path, body)`.
///
/// A missing base layer is the normal case (nothing has declared
/// `programs.trollshell.config.places`); an **unreadable** one is warned and
/// skipped rather than fatal, because `places` has no error channel to a
/// caller — [`load_places`] has always answered with *some* set — and showing
/// nix's places as though they were the user's is not an option either way.
#[must_use]
pub fn read_base_layers() -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for path in base_layer_paths() {
        match std::fs::read_to_string(&path) {
            Ok(body) => out.push((path, body)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "places: base layer unreadable; ignoring it");
            }
        }
    }
    out
}

/// What a layered load of `places.toml` produced, and what it learned on the
/// way — [`crate::subsystem::Loaded`]'s shape for the one family that does not
/// go through that module.
///
/// `#[non_exhaustive]` for [`crate::subsystem::Loaded`]'s reason: every
/// construction site is in this crate and this is the type that grows a field.
/// Deliberately **not** `Default`: the one invariant this type has is that
/// [`Self::places`] is never empty, and a derived `Default` would hand out the
/// one value that breaks it.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Layered {
    /// The merged set. Never empty: a merge that yields no `[[place]]` falls
    /// back to [`builtin_default`], exactly as [`load_places`] always has.
    pub places: Vec<Place>,
    /// The merged `[departures].endpoint` (#1124), normalised the way
    /// [`parse_departures_endpoint`] normalises it.
    pub endpoint: Option<String>,
    /// Dotted paths the **base** layers pinned — [`PLACE_KEY`] and/or
    /// [`ENDPOINT_KEY`] today. The set the fold actually enforced, with
    /// [`crate::merge::Merged::locked`]'s two exclusions, so the overlay's own
    /// marker is not in here.
    pub locked: BTreeSet<String>,
    /// Every `_locked` problem this load found, in
    /// [`crate::subsystem::Loaded::lock_findings`]'s vocabulary and wording:
    /// a refused override, a malformed marker, an inert one.
    pub lock_findings: Vec<Finding>,
    /// Layer files that existed and contributed, lowest precedence first.
    /// [`DEFAULT_CONFIG`] is not listed — it is not a file.
    ///
    /// Nothing in the workspace reads this yet (#1338 review, L5), and it is
    /// here for the same reason [`crate::subsystem::Loaded::sources`] is: a
    /// diagnostic has to be able to answer *"which files did this answer come
    /// from?"*, and the only place that knows is the load. Given the whole
    /// point of #1227 item 2 is that there is now more than one file, an
    /// editor or a `Control` method that says "these places come from nix"
    /// should be able to name which nix — and rebuilding the search path at
    /// the call site would be a second reading of what this load actually did,
    /// which is the exact drift [`Self::locked`]'s own doc argues against.
    pub sources: Vec<PathBuf>,
}

impl Layered {
    /// Whether a base layer pinned `path` — itself, or by an ancestor table
    /// being locked whole.
    ///
    /// The same predicate [`crate::subsystem::Loaded::is_locked`] answers, over
    /// the same walk, because it *is* that function's body.
    #[must_use]
    pub fn is_locked(&self, path: &str) -> bool {
        subsystem::locked_here(&self.locked, path)
    }

    /// Whether the `[[place]]` array is nix's, i.e. whether the two editors
    /// must go read-only for it.
    #[must_use]
    pub fn places_are_locked(&self) -> bool {
        self.is_locked(PLACE_KEY)
    }

    /// Whether `[departures].endpoint` is nix's.
    #[must_use]
    pub fn endpoint_is_locked(&self) -> bool {
        self.is_locked(ENDPOINT_KEY)
    }
}

/// Merge [`DEFAULT_CONFIG`], every base layer body and the overlay body — the
/// pure core of [`load_layered`], so every rule above is unit-testable with no
/// filesystem and no environment.
///
/// `bases` are lowest precedence first; `overlay` is the operator's own file,
/// `None` when it does not exist. The split is handed to
/// [`crate::merge::merge_all_locked`] as its two arguments because that split
/// **is** #1331's rule: the union of every base's `_locked` binds the overlay
/// and nothing else, base layers fold among themselves by XDG precedence with
/// no enforcement between them, and the overlay's own marker pins nothing.
///
/// A layer that is not valid TOML is warned and **skipped**, not fatal. That is
/// [`load_places`]'s contract ("use the default for a malformed config, and do
/// not overwrite it") generalised one layer at a time: a typo in the overlay
/// costs the overlay, not the nix base layer underneath it.
#[must_use]
pub fn assemble_places(
    bases: &[(PathBuf, String)],
    overlay: Option<&(PathBuf, String)>,
) -> Layered {
    // Two parallel vectors, `crate::subsystem::assemble_layers`' shape: the
    // diagnostics want the file name beside each table, and `merge`'s own
    // whole-stack questions want the tables as one slice.
    let mut paths: Vec<Option<&Path>> = vec![None];
    let mut tables: Vec<toml::Table> = vec![parse_layer(DEFAULT_CONFIG, None)];
    let mut sources = Vec::new();

    for (path, body) in bases {
        paths.push(Some(path.as_path()));
        tables.push(parse_layer(body, Some(path)));
        sources.push(path.clone());
    }
    if let Some((path, body)) = overlay {
        paths.push(Some(path.as_path()));
        tables.push(parse_layer(body, Some(path)));
        sources.push(path.clone());
    }

    let mut lock_findings = subsystem::locked_marker_findings(SUBSYSTEM, &paths, &tables);
    // Who *set* `place`, asked before the fold flattens it away — the input to
    // "an empty array from a base layer means no places" below. Index 0 is
    // `DEFAULT_CONFIG`, then the bases, then the overlay if there is one.
    let base_sets_place = tables[1..=bases.len()]
        .iter()
        .any(|t| t.contains_key(PLACE_KEY));
    let overlay_sets_place = overlay.is_some() && tables[bases.len() + 1].contains_key(PLACE_KEY);

    // The last layer is the overlay when there is one — the split #1331's rule
    // is stated over, handed over as two arguments rather than guessed from a
    // position.
    let overlay_table = if overlay.is_some() {
        tables.pop()
    } else {
        None
    };
    let merged = merge::merge_all_locked(tables, overlay_table);
    lock_findings.extend(subsystem::shadowed_findings(
        SUBSYSTEM,
        &paths,
        &merged.shadowed,
    ));

    // Which layer's `place` array actually survived: the overlay's only if it
    // set one *and* no base pinned the key. Anything else that set `place` is
    // a base layer (`DEFAULT_CONFIG` is not one).
    let overlay_wins_place =
        overlay_sets_place && !subsystem::locked_here(&merged.locked, PLACE_KEY);
    let base_supplies_place = base_sets_place && !overlay_wins_place;

    let (places, endpoint) = read_merged(&merged.table, base_supplies_place);
    Layered {
        places,
        endpoint,
        locked: merged.locked,
        lock_findings,
        sources,
    }
}

/// One layer body as a table; a parse failure is warned, named, and read as an
/// empty table so the layers under it still apply.
fn parse_layer(body: &str, path: Option<&Path>) -> toml::Table {
    match body.parse::<toml::Table>() {
        Ok(table) => table,
        Err(e) => {
            match path {
                Some(path) => {
                    tracing::warn!(error = %e, path = %path.display(), "places: config parse failed; ignoring this layer");
                }
                None => {
                    tracing::error!(error = %e, "built-in default places config failed to parse");
                }
            }
            toml::Table::new()
        }
    }
}

/// The merged table as the two things this file models. Rule 4's "warn, never
/// fail" is [`PlaceCfg`]'s own `#[serde]` tolerance here: a key this schema
/// does not know is ignored, exactly as it is when one file is parsed.
///
/// The two fallbacks are [`load_places`]'s own, one layer up: a merged set that
/// holds no usable `[[place]]` reads as [`builtin_default`] — the shell still
/// has to be somewhere — and a `[departures]` table that yields nothing reads
/// as `None`, "use the default".
///
/// # `base_supplies_place`: an empty array from nix means **no places** (#1338 review, M2)
///
/// That fallback has exactly one exception, and it is the difference between
/// two empty arrays that TOML spells identically:
///
/// * the **overlay**'s `place = []` is how the editor spells *"I deleted my
///   last place"* (`hytte_services::places::remove_place` writes it and
///   documents the round trip), and the shell still has to be somewhere — so
///   it reads as [`builtin_default`], exactly as it has since #640;
/// * a **base layer**'s `place = []` is nix stating a fact. `programs.
///   trollshell.config.places.place = [ ];` renders `place = []` beside
///   `_locked = ["place"]`, and rule 3 says an array replaces whole — so
///   "no places" is what it says, and answering with the built-in Berlin
///   default would be the one list the operator provably did not ask for,
///   pinned, in a tab that says the places "come from
///   `programs.trollshell.config.places.place`".
///
/// `base_supplies_place` is the caller's answer to "did the array that
/// survived come from a base layer?" — computed in [`assemble_places`], where
/// the per-layer tables are still separate, because after the fold there is no
/// provenance left to ask.
fn read_merged(table: &toml::Table, base_supplies_place: bool) -> (Vec<Place>, Option<String>) {
    let value = toml::Value::Table(table.clone());
    let places = match value.clone().try_into::<ConfigFile>() {
        Ok(cfg) => cfg.into_places(),
        Err(e) => {
            tracing::warn!(error = %e, "places: merged config does not match the [[place]] schema; using default");
            Vec::new()
        }
    };
    let endpoint = value
        .try_into::<DeparturesConfigFile>()
        .ok()
        .and_then(|cfg| cfg.departures.endpoint)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    if places.is_empty() {
        if base_supplies_place {
            tracing::info!(
                "places: the base layer declares an empty [[place]] list; no places configured"
            );
            return (Vec::new(), endpoint);
        }
        tracing::warn!("places: merged config has no [[place]]; using default");
        return (builtin_default(), endpoint);
    }
    (places, endpoint)
}

/// [`assemble_places`] over the process environment's search path.
///
/// Reads the base layers ([`read_base_layers`]) and the overlay
/// ([`config_path`]); a missing overlay is the normal case and is simply
/// `None` — the locks still come back, because that is the set the editors grey
/// rows from and the set the very first save must skip.
#[must_use]
pub fn load_layered() -> Layered {
    let overlay = config_path().and_then(|path| {
        match std::fs::read_to_string(&path) {
            Ok(body) => Some((path, body)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                // Exists but unreadable (permissions, non-UTF-8, …): use the
                // layers below but DON'T overwrite — the bytes may be a config
                // we just can't read.
                tracing::warn!(error = %e, path = %path.display(), "places: config unreadable; using the layers below it (not overwriting)");
                None
            }
        }
    });
    assemble_places(&read_base_layers(), overlay.as_ref())
}

/// Drop empty/whitespace-only entries (a stray `""` would otherwise become an
/// accidental allow-all, since an empty needle is a substring of everything).
fn nonblank(items: Vec<String>) -> Vec<String> {
    items.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

/// Parse a config body into places. Pure, so the schema is unit-testable.
///
/// # Errors
/// A `String` if `toml_text` isn't valid TOML, or holds a `[[place]]` block
/// the schema cannot read.
pub fn parse_places(toml_text: &str) -> Result<Vec<Place>, String> {
    let cfg: ConfigFile = toml::from_str(toml_text).map_err(|e| format!("config: {e}"))?;
    Ok(cfg.into_places())
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

/// Load the **merged** place set — [`load_layered`]'s `places` — writing the
/// documented default on first run.
///
/// Returns [`builtin_default`] when no layer contributes a usable `[[place]]`.
///
/// # First run, and why nix suppresses it (#1227 item 2)
///
/// With no overlay file this has always written [`DEFAULT_CONFIG`] into
/// `~/.config/trollshell/places.toml` so the schema is discoverable, and
/// carried a pre-rename `departures.toml` forward if one is there. Both still
/// happen — **unless a base layer already declares `[[place]]`**. Writing the
/// documented default underneath a nix-declared list would create an overlay
/// whose `place` array *replaces* nix's (rule 3) on a box where the operator
/// asked for the opposite, and, when that array is locked, would be refused on
/// every subsequent load: one journal line per load, forever, about a file
/// nobody wrote on purpose.
#[must_use]
pub fn load_places() -> Vec<Place> {
    let bases = read_base_layers();
    let Some(path) = config_path() else {
        return assemble_places(&bases, None).places;
    };
    let overlay = match std::fs::read_to_string(&path) {
        Ok(text) => Some((path.clone(), text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No places.toml yet. One-time: carry forward a pre-rename
            // departures.toml if present; otherwise write the documented
            // default so the schema is discoverable. Neither when nix has
            // already supplied the list — see the section above.
            if base_declares_places(&bases) {
                // #1338 review, L4: the legacy `departures.toml` carry-forward
                // is suppressed here too, and silently suppressing it is how a
                // file stops being migrated with nobody the wiser. It is the
                // right call — the array it would write is one the merge
                // refuses — but the operator has a file sitting there that
                // will never be read, so say so once.
                if let Some(legacy) = legacy_departures_path()
                    && legacy.exists()
                {
                    tracing::info!(
                        path = %legacy.display(),
                        "places: a pre-rename departures.toml is present but not migrated — \
                         your places come from the nix base layer, which an overlay cannot \
                         override; copy anything you still want out of it by hand"
                    );
                }
                None
            } else if let Some(migrated) = migrate_legacy_departures(&path) {
                // Returned unmerged, and that is exact rather than a shortcut:
                // this arm is only reached when no base layer declares
                // `[[place]]`, so the overlay's array (which the legacy file
                // has just become) replaces `DEFAULT_CONFIG`'s and *is* the
                // merged list. The endpoint is not this function's to return,
                // and `load_departures_endpoint` merges it normally.
                return migrated;
            } else {
                write_default_config();
                None
            }
        }
        Err(e) => {
            // Exists but unreadable (permissions, non-UTF-8, …): use the layers
            // below but DON'T overwrite — the bytes may be a config we just
            // can't read.
            tracing::warn!(error = %e, path = %path.display(), "places: config unreadable; using the layers below it (not overwriting)");
            None
        }
    };
    assemble_places(&bases, overlay.as_ref()).places
}

/// Whether any base layer sets `[[place]]` itself — the question
/// [`load_places`]' first-run write and [`layered_fallback`] both ask, and the
/// one thing that distinguishes "nix has an opinion about the list" from "nix
/// set only `[departures]`".
fn base_declares_places(bases: &[(PathBuf, String)]) -> bool {
    bases.iter().any(|(_, body)| {
        body.parse::<toml::Table>()
            .is_ok_and(|table| table.contains_key(PLACE_KEY))
    })
}

fn write_default_config() {
    if config_file::write("places", CONFIG_FILE, DEFAULT_CONFIG) {
        tracing::info!(file = CONFIG_FILE, "places: wrote default config");
    }
}

/// The pre-rename `~/.config/trollshell/departures.toml`, whether or not it is
/// there. Split out of [`migrate_legacy_departures`] so [`load_places`] can
/// name the file in the one branch that deliberately does *not* migrate it
/// (#1338 review, L4).
fn legacy_departures_path() -> Option<PathBuf> {
    config_file::path("departures.toml")
}

/// One-time migration of the pre-rename `departures.toml` (whose schema is a
/// forward-compatible subset of `places.toml`). When `places.toml` is absent
/// but a parseable `departures.toml` exists, rename it forward so the user's
/// station/lines/directions survive the rename. Returns its places on success.
fn migrate_legacy_departures(places_path: &Path) -> Option<Vec<Place>> {
    let legacy = legacy_departures_path()?;
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

/// A layer's on-disk fingerprint: its mtime **and a content hash** — `None`
/// when it can't be stat'd or read (missing or unreadable, the ordinary
/// no-config-yet case). [`ConfigWatcher`] compares this across polls to
/// detect edits.
///
/// A bare mtime — what this compared before #1162 lens 7 item 1 — misses a
/// rewrite that lands inside the same timestamp granule as the previous read
/// and lands at the same length, and misses it **permanently**: the stamp is
/// replaced unconditionally on every poll, so the movement is never seen
/// again once that poll passes. Coarse-granularity filesystems (a network
/// mount, a FAT stick `$XDG_CONFIG_DIRS` might point at) make the window
/// routine rather than theoretical. Hashing the body closes it the same way
/// `subsystem::watch::stamp` (#1081 M5) closes it for the nine subsystems that
/// followed `places`: reimplemented here rather than imported, because that
/// module sits behind the crate's `watch` cargo feature — off by default so
/// `trollshell-control-center`, which builds a [`ConfigWatcher`] of its own
/// (`places_tab.rs`), never gains the tokio/futures-signals runtime that
/// feature pulls in for a settings app that has no other use for one.
///
/// `std::collections::hash_map::DefaultHasher` is deliberately not a
/// cryptographic hash — its output is unspecified across Rust releases, which
/// is fine here because a stamp is only ever compared against another stamp
/// read by this same process, never persisted or compared cross-process.
fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&bytes, &mut hasher);
    Some((meta.modified().ok()?, std::hash::Hasher::finish(&hasher)))
}

/// Watches `places.toml` for live reload by polling its `stamp` — mtime plus
/// a content hash. Remembers the last-seen stamp so a poll only re-reads the
/// file when it actually moved, and content-checks the reparse so a `touch`
/// or no-op save doesn't churn a re-resolve.
pub struct ConfigWatcher {
    /// Every layer, lowest precedence first — [`layer_paths`]. Since #1227
    /// item 2 that is the nix base layer(s) *and* the overlay, not the overlay
    /// alone: a `nixos-rebuild` moves the base layer's store path, which is a
    /// change to the merged set exactly the way an `$EDITOR` save is, and a
    /// watcher that only stamped the overlay would show the previous
    /// generation's places until the shell was restarted.
    paths: Vec<PathBuf>,
    /// One stamp per entry of [`Self::paths`], `None` for a layer that is not
    /// there — which is how an *appearing* or *vanishing* layer is seen at all.
    last: Vec<Option<(SystemTime, u64)>>,
}

impl Default for ConfigWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigWatcher {
    /// Start watching from *now*: the file's current stamp is taken as the
    /// baseline, so the first [`poll`](Self::poll) reports only edits made
    /// after construction rather than replaying the state at startup.
    #[must_use]
    pub fn new() -> Self {
        let paths = layer_paths();
        let last = paths.iter().map(|p| stamp(p)).collect();
        Self { paths, last }
    }

    /// Whether **some layer's** content has moved since the previous call —
    /// the whole of the watch, with no reload and no opinion about what
    /// changed (#1338 review, H2).
    ///
    /// Observing is consuming: the stamps are advanced here, so two callers
    /// must not share one watcher (each editor builds its own).
    ///
    /// This is the half [`Self::poll`] used to compute and throw away, and it
    /// exists because **a lock is not a list**. `poll` dedups on the merged
    /// place list, which is exactly right for the shell (its handle holds a
    /// list and nothing else) and silently wrong for an editor that also
    /// renders the lock set and the endpoint: a `nixos-rebuild` that adds or
    /// drops `programs.trollshell.config.places` while the overlay already
    /// holds the same list moves no place, so `poll` answers `None` and the
    /// greyed rows outlive the option that greyed them. A caller that renders
    /// more than the list asks this instead and compares what *it* shows.
    pub fn moved(&mut self) -> bool {
        if self.paths.is_empty() {
            return false;
        }
        let now: Vec<Option<(SystemTime, u64)>> = self.paths.iter().map(|p| stamp(p)).collect();
        if now == self.last {
            return false;
        }
        self.last = now;
        true
    }

    /// Reload and return the fresh places when **some layer's** stamp has moved
    /// since the previous poll *and* the merged list differs from `current`;
    /// otherwise `None` (no layer moved, no layer to watch, or an identical
    /// reparse).
    ///
    /// For a caller whose whole state *is* the list — the shell's
    /// `Mutable<Arc<Vec<Place>>>`. An editor that also shows the lock set wants
    /// [`Self::moved`]; see there for why this one cannot serve it.
    pub fn poll(&mut self, current: &[Place]) -> Option<Vec<Place>> {
        if !self.moved() {
            return None;
        }
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
    /// could never match by `GeoClue` radius.
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
    /// overwriting bytes we can't account for — see [`check_base`] for why this is a
    /// data-loss guard and not just tidiness.
    Unreadable(String),
    /// `places.toml` parses, but to a *different* set than the one in memory —
    /// something edited it since we last loaded it. Refusing, because the edit
    /// was computed against a stale base and applying it would write the
    /// out-of-process change away. See [`check_base`].
    ChangedOnDisk,
    /// The set could not be rendered as TOML.
    Encode(String),
    /// The atomic write failed; the previous config is untouched.
    Write(String),
    /// `[departures].endpoint` (#1124) is neither one of
    /// [`DEPARTURES_ENDPOINT_NAMES`] nor a `http(s)://` base URL.
    Endpoint {
        /// The rejected raw value.
        value: String,
    },
    /// A nix base layer pinned this key with `_locked` (#1227), so the merge
    /// would refuse an overlay value for it on the next load. Refused here
    /// instead of written and reverted a tick later — the invariant
    /// `subsystem::save_overlay_to_locked` holds for the families that go
    /// through [`crate::subsystem`]: a value the reader would reject is never
    /// written.
    Locked {
        /// The pinned dotted path — [`PLACE_KEY`] or [`ENDPOINT_KEY`].
        key: String,
    },
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
            Self::Endpoint { value } => write!(
                f,
                "\"{value}\" is not a departures endpoint — use {} or a full http(s):// base URL",
                DEPARTURES_ENDPOINT_NAMES.join(", ")
            ),
            // The house sentence for a refused override, composed exactly the
            // way `crate::subsystem`'s `shadowed_message` composes it — this
            // one just arrives at the *save* instead of at the load, because
            // `places` has an editor and can say so before the write.
            Self::Locked { key } => write!(
                f,
                "places.{key} is set in nix and cannot be overridden from {}",
                config_path().map_or_else(
                    || "your places.toml overlay".to_owned(),
                    |p| p.display().to_string()
                )
            ),
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
/// `align`) and patched in place (`patch_table`); entries with no match are
/// appended as freshly built tables (`new_table`); tables nothing matched are
/// dropped. The file's opening comment block is carried across the rebuild by
/// `take_header`/`put_header`, because `toml_edit` glues it to whichever
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
/// `read_on_disk`. Callers arriving through [`check_base`] have already parsed the
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
/// Unlike `patch_table` this spells every key out (bar an unset `station`,
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
/// `put_header` puts it back at the top afterwards.
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

/// Put back what `take_header` detached, in front of whatever now sits at the
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
    /// A parseable file. A config that yields zero places is reported as the
    /// caller's `fallback`, because that is what a *reader* makes of it — so
    /// "disk" and "memory" are compared in the same units.
    ///
    /// That fallback was flatly [`builtin_default`] before #1227 item 2, and
    /// still is for [`check_base`]. It stops being the right answer the moment
    /// a base layer declares `[[place]]`: an overlay holding only
    /// `[departures]` (which the endpoint row alone can create) yields zero
    /// places while the layered reader reports nix's list, and comparing the
    /// two would refuse every save as [`PlacesError::ChangedOnDisk`] against a
    /// file nothing had changed. [`check_base_layered`] is that arm, and it is
    /// the one the environment-resolving writers take.
    Places(Vec<Place>),
    /// The file is there but its contents can't be established. Never write
    /// over this.
    Unknown(String),
}

/// [`DEFAULT_CONFIG`] with every key a base layer pinned taken out — the
/// document a writer may create a **new** overlay from (#1338 review, H1).
///
/// Every writer here seeds the shipped default when there is no overlay yet,
/// so the file it creates is as self-documenting as the one first run writes.
/// Under a nix base layer that is a trap: the seed's own `[[place]]` block
/// lands in `~/.config/trollshell/places.toml`, the merge refuses it on the
/// next load, and the operator gets *"one journal line per load, forever,
/// about a file nobody wrote on purpose"* — the exact sentence
/// [`load_places`]' first-run suppression exists for, reached through a
/// different door. Measured: one click on the Endpoint row (which is
/// deliberately still sensitive while only `place` is locked) was enough.
///
/// Filtering the seed through the lock set rather than skipping it wholesale
/// keeps the documented preamble, which is the reason the seed exists: an
/// operator whose places are nix's still gets a file that explains
/// `[departures]` when the editor first writes one.
///
/// The removal itself is [`crate::subsystem::strip_locked`] — #1333's, which
/// landed for the `Subsystem` families while this was in review and is the
/// better primitive: it recurses, it answers through
/// [`crate::subsystem::Loaded::is_locked`]'s own predicate (so an ancestor lock
/// removes its descendants), and it takes a removed key's **attached comment**
/// with it instead of orphaning it. One walk for both families, so they cannot
/// drift on what "with the locked keys taken out" means.
///
/// What is `places`-specific — and why [`take_header`]/[`put_header`] still
/// wrap that call — is *where the file's own preamble lives*. In
/// `places.toml` it is the prefix decor of the first `[[place]]` element, i.e.
/// attached to the very key the lock removes, so the comment-aware removal
/// that is right everywhere else would take the whole documented header with
/// it. Detaching it first and putting it back afterwards is the same pair a
/// save already uses for the same reason (`render_places` re-homes it when an
/// edit removes the first place). Pinned by
/// `the_seed_drops_exactly_the_locked_keys`.
///
/// Removing the block leaves the comments that were attached *inside* it —
/// the `lines`/`directions` and `[departures]` paragraphs — standing at the
/// top level, describing a block that is no longer there. Cosmetic and
/// deliberate: they are still the only documentation of those keys, and the
/// alternative is a second hand-maintained default that would drift from the
/// first.
///
/// A malformed [`DEFAULT_CONFIG`] (a bug of ours, caught by
/// `default_config_parses`) degrades to seeding it whole, which is what every
/// caller did before this existed.
fn seed_for(locked: &BTreeSet<String>) -> String {
    if locked.is_empty() {
        return DEFAULT_CONFIG.to_owned();
    }
    let Ok(mut doc) = DEFAULT_CONFIG.parse::<toml_edit::DocumentMut>() else {
        tracing::error!("built-in default places config failed to parse; seeding it unfiltered");
        return DEFAULT_CONFIG.to_owned();
    };
    let header = take_header(&mut doc);
    subsystem::strip_locked(doc.as_table_mut(), locked, "");
    put_header(&mut doc, &header);
    doc.to_string()
}

/// Classify the current `places.toml` for a writer — see [`OnDisk`].
///
/// `path` is the overlay, the one file a writer ever replaces. `fallback` is
/// what a zero-places file means to whoever is asking: [`builtin_default`] for
/// [`check_base`], the layered reader's answer for [`check_base_layered`].
///
/// Pure with respect to the environment, deliberately: this is what
/// `tests/places_byte_identical.rs` and every explicit-path test here go
/// through, and a hidden `$XDG_CONFIG_DIRS` read in here would make all of them
/// depend on whether the developer's own box happens to use the feature they
/// are testing — and race any sibling test that redirects it.
fn read_on_disk(path: &Path, fallback: &[Place]) -> OnDisk {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return OnDisk::Absent,
        Err(e) => return OnDisk::Unknown(e.to_string()),
    };
    match parse_places(&text) {
        Ok(places) if places.is_empty() => OnDisk::Places(fallback.to_vec()),
        Ok(places) => OnDisk::Places(places),
        Err(e) => OnDisk::Unknown(e),
    }
}

/// What a zero-places overlay means to the **layered** reader: the nix base
/// layer's own list if one declares `[[place]]`, else [`builtin_default`].
///
/// Reads the environment, so it lives beside the other environment-resolving
/// helpers rather than inside [`read_on_disk`].
fn layered_fallback() -> Vec<Place> {
    let bases = read_base_layers();
    if base_declares_places(&bases) {
        assemble_places(&bases, None).places
    } else {
        builtin_default()
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
/// what [`check_base`] exists to prevent. Reaching this through [`save_to`] is
/// impossible (it refuses first, with the more specific
/// [`PlacesError::Unreadable`]). [`PlacesError::Write`] when the atomic write
/// itself fails; the previous config is then untouched.
pub fn persist_to(path: &Path, places: &[Place]) -> Result<(), PlacesError> {
    persist_to_seeded(path, places, DEFAULT_CONFIG)
}

/// [`persist_to`] against an explicit `seed` — the document to edit when there
/// is no file yet.
///
/// Its own function for [`read_on_disk`]'s reason: the seed is the one thing
/// about this writer that depends on the *environment* (which keys a nix base
/// layer pinned), and the public, explicit-path entry point stays pure so
/// `tests/places_byte_identical.rs` and every explicit-path test here keep
/// meaning what they meant. [`save`] passes [`seed_for`]'s filtered document;
/// everyone else passes [`DEFAULT_CONFIG`] whole.
fn persist_to_seeded(path: &Path, places: &[Place], seed: &str) -> Result<(), PlacesError> {
    let existing = std::fs::read_to_string(path).unwrap_or_else(|_| seed.to_owned());
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
    check_base_against(path, base, &builtin_default())
}

/// [`check_base`] for a caller that resolved `path` from the environment —
/// i.e. one whose `base` came out of the **layered** reader (#1227 item 2).
///
/// The only difference is the units a zero-places overlay is reported in: see
/// [`OnDisk::Places`] and [`layered_fallback`]. Everything else, including the
/// two data-loss arguments on [`check_base`], is identical.
///
/// # Errors
/// As [`check_base`].
pub fn check_base_layered(path: &Path, base: &[Place]) -> Result<(), PlacesError> {
    check_base_against(path, base, &layered_fallback())
}

fn check_base_against(path: &Path, base: &[Place], fallback: &[Place]) -> Result<(), PlacesError> {
    match read_on_disk(path, fallback) {
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
    save_to_against(path, base, next, &builtin_default(), DEFAULT_CONFIG)
}

fn save_to_against(
    path: &Path,
    base: &[Place],
    next: Vec<Place>,
    fallback: &[Place],
    seed: &str,
) -> Result<(), PlacesError> {
    check_base_against(path, base, fallback)?;
    let next: Vec<Place> = next.into_iter().map(normalize).collect();
    validate(&next)?;
    persist_to_seeded(path, &next, seed)?;
    warn_unsatisfiable_fingerprints(&next);
    tracing::info!(count = next.len(), "places: config saved");
    Ok(())
}

/// Refuse a write at a key a nix base layer pinned (#1227 item 2).
///
/// The guard every *environment-resolving* write goes through — [`save`],
/// [`save_departures_endpoint`], and `hytte_services::places::edit`, which
/// composes [`check_base`]/[`persist_to`] itself. The explicit-path pair
/// ([`save_to`], [`persist_to`]) deliberately does **not**: those are the test
/// seams and the byte-pinned writer, and a function handed an absolute path
/// should not go reading `$XDG_CONFIG_DIRS` behind its caller's back.
///
/// Reads the base layers each call. That is once per save — a rare,
/// deliberate, user-acknowledged action — and reading them fresh is the point:
/// a `nixos-rebuild` between the load and the save must not be answered from a
/// cached lock set.
///
/// # Errors
/// [`PlacesError::Locked`] when the union of the base layers' `_locked` names
/// `key` (or an ancestor of it).
pub fn check_unlocked(key: &str) -> Result<(), PlacesError> {
    refuse_if_locked(&base_locks(), key)
}

/// The union of every base layer's `_locked`, read fresh from the environment.
///
/// Fresh on every call, deliberately: a save is rare and deliberate, and a
/// `nixos-rebuild` between the load and the save must not be answered out of a
/// cache. The two writers keep the set they read, because they need it twice —
/// once to refuse, once to filter the seed ([`seed_for`]).
fn base_locks() -> BTreeSet<String> {
    assemble_places(&read_base_layers(), None).locked
}

/// [`PlacesError::Locked`] when `locked` pins `key` (itself or an ancestor).
fn refuse_if_locked(locked: &BTreeSet<String>, key: &str) -> Result<(), PlacesError> {
    if subsystem::locked_here(locked, key) {
        return Err(PlacesError::Locked {
            key: key.to_owned(),
        });
    }
    Ok(())
}

/// [`save_to`] against the user's real `~/.config/trollshell/places.toml`,
/// refusing first if nix pinned the list ([`check_unlocked`]).
///
/// # Errors
/// [`PlacesError::NoConfigPath`] when `$HOME` is unset,
/// [`PlacesError::Locked`] when a base layer pinned [`PLACE_KEY`], else as
/// [`save_to`].
pub fn save(base: &[Place], next: Vec<Place>) -> Result<(), PlacesError> {
    let path = config_path().ok_or(PlacesError::NoConfigPath)?;
    let locked = base_locks();
    refuse_if_locked(&locked, PLACE_KEY)?;
    // `save_to`'s own zero-places fallback is the built-in default, which is
    // what a reader with no base layer reports; this caller's `base` came from
    // the layered reader, so the comparison takes the layered fallback — see
    // [`check_base_layered`]. The seed is filtered for `seed_for`'s reason: a
    // writer must not create an overlay the next load would refuse.
    save_to_against(&path, base, next, &layered_fallback(), &seed_for(&locked))
}

// ── Departures endpoint (#1124) ──────────────────────────────────────────────
//
// A single `endpoint` key in a top-level `[departures]` table — deliberately
// *not* a `Place` field. `places_byte_identical.rs` pins `Place`'s derived
// `Debug` output verbatim (#640/#703); any field added there shows up in that
// output too, and the only way to keep the golden text green would be a
// hand-written `Debug` impl that omits it. A table this reader/writer pair has
// never heard of already round-trips untouched (the fixture's `[unrelated]`
// table proves it), so a brand-new one costs nothing to add this way, and BVG,
// VBB and DB are properties of *which network you live in*, not of any one
// place: unlike `station`, a value here is not tied to a `[[place]]` block.

/// Short names transport.rest answers for out of the box (#1124). Order here
/// is the order [`PlacesError::Endpoint`]'s hint lists them in.
pub const DEPARTURES_ENDPOINT_NAMES: [&str; 3] = ["bvg", "vbb", "db"];

/// `https://v6.<name>.transport.rest` — the HAFAS v6 REST base a short name
/// maps to. Meaningful only for a `name` in [`DEPARTURES_ENDPOINT_NAMES`];
/// [`resolve_departures_endpoint`] is the one caller and already checked
/// membership first.
#[must_use]
pub fn departures_base_url(name: &str) -> String {
    format!("https://v6.{name}.transport.rest")
}

/// Resolve a configured `[departures].endpoint` value into the base URL the
/// departures fetch should hit:
/// * `None`, or blank — the key absent, or present but empty — → `bvg`'s URL,
///   so an existing config with no key behaves exactly as before.
/// * A name in [`DEPARTURES_ENDPOINT_NAMES`] → that name's URL.
/// * Anything starting `http://` or `https://` → used verbatim, minus a
///   trailing slash.
/// * Anything else → [`PlacesError::Endpoint`].
///
/// # Errors
/// [`PlacesError::Endpoint`] for a value that is neither a recognised name nor
/// a URL.
pub fn resolve_departures_endpoint(endpoint: Option<&str>) -> Result<String, PlacesError> {
    let Some(value) = endpoint.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(departures_base_url("bvg"));
    };
    if value.starts_with("http://") || value.starts_with("https://") {
        return Ok(value.trim_end_matches('/').to_owned());
    }
    if DEPARTURES_ENDPOINT_NAMES.contains(&value) {
        return Ok(departures_base_url(value));
    }
    Err(PlacesError::Endpoint {
        value: value.to_owned(),
    })
}

/// Validate a configured endpoint without needing its resolved URL — what an
/// editor calls before a save. Same rules as [`resolve_departures_endpoint`].
///
/// # Errors
/// [`PlacesError::Endpoint`], as [`resolve_departures_endpoint`].
pub fn validate_departures_endpoint(endpoint: Option<&str>) -> Result<(), PlacesError> {
    resolve_departures_endpoint(endpoint).map(|_| ())
}

/// The `[departures]` table, as read. Only `endpoint` exists today; unknown
/// keys are ignored rather than rejected, matching [`PlaceCfg`]'s own
/// tolerance.
#[derive(serde::Deserialize, Default)]
struct DeparturesCfg {
    #[serde(default)]
    endpoint: Option<String>,
}

/// A config file, from the departures table's point of view — `place` isn't
/// modelled here, so it's ignored rather than rejected the same way an
/// unrelated top-level table is.
#[derive(serde::Deserialize, Default)]
struct DeparturesConfigFile {
    #[serde(default)]
    departures: DeparturesCfg,
}

/// Parse the `[departures].endpoint` key out of a `places.toml` body, if any.
/// Pure, so the schema is unit-testable independently of `[[place]]` parsing.
/// An absent table, an absent key, or a blank value all read as `None` — "use
/// the default", not an error.
///
/// # Errors
/// A `String` if `toml_text` isn't valid TOML.
pub fn parse_departures_endpoint(toml_text: &str) -> Result<Option<String>, String> {
    let cfg: DeparturesConfigFile =
        toml::from_str(toml_text).map_err(|e| format!("config: {e}"))?;
    Ok(cfg
        .departures
        .endpoint
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty()))
}

/// Render `endpoint` into `[departures].endpoint` of whatever document
/// `existing` holds, patching it rather than rebuilding — the endpoint-only
/// counterpart to [`render_places`], for the same format-preserving reasons,
/// and touching nothing [`render_places`] itself would touch.
///
/// `endpoint: None` (or blank) removes the key — and the whole `departures`
/// table, if this was its only key — rather than writing it blank: the schema
/// spells "use the default" as an absent key, the same convention `station`
/// uses. Validated *before* the document is touched, so a rejected save never
/// partially writes.
///
/// # Errors
/// [`PlacesError::Endpoint`] when `endpoint` is `Some` and neither a
/// recognised name nor a URL. [`PlacesError::Encode`] when `existing` isn't
/// valid TOML, or when a `departures` key exists but isn't a table.
pub fn render_departures_endpoint(
    existing: &str,
    endpoint: Option<&str>,
) -> Result<String, PlacesError> {
    let normalized = endpoint.map(str::trim).filter(|s| !s.is_empty());
    validate_departures_endpoint(normalized)?;

    let mut doc: toml_edit::DocumentMut = existing.parse().map_err(|e: toml_edit::TomlError| {
        PlacesError::Encode(format!("the file being replaced is not valid TOML: {e}"))
    })?;
    let root = doc.as_table_mut();
    match normalized {
        Some(value) => {
            match root.get("departures") {
                None => {
                    root.insert(
                        "departures",
                        toml_edit::Item::Table(toml_edit::Table::new()),
                    );
                }
                Some(item) if item.is_table() => {}
                Some(_) => {
                    return Err(PlacesError::Encode(
                        "`departures` exists but isn't a table".to_owned(),
                    ));
                }
            }
            let table = root
                .get_mut("departures")
                .and_then(toml_edit::Item::as_table_mut)
                .expect("just ensured `departures` is a table");
            set_value(table, "endpoint", toml_edit::Value::from(value));
        }
        None => {
            if let Some(table) = root
                .get_mut("departures")
                .and_then(toml_edit::Item::as_table_mut)
            {
                table.remove("endpoint");
                if table.is_empty() {
                    root.remove("departures");
                }
            }
        }
    }
    Ok(doc.to_string())
}

/// Patch + atomically write the departures endpoint to `path`. The
/// endpoint-only counterpart to [`persist_to`].
///
/// # Errors
/// As [`render_departures_endpoint`], plus [`PlacesError::Write`] if the
/// atomic write fails; the previous config is then untouched.
pub fn persist_departures_endpoint_to(
    path: &Path,
    endpoint: Option<&str>,
) -> Result<(), PlacesError> {
    persist_departures_endpoint_seeded(path, endpoint, DEFAULT_CONFIG)
}

/// [`persist_departures_endpoint_to`] against an explicit `seed` — see
/// [`persist_to_seeded`] for why the environment-dependent half is split out
/// of the public, explicit-path entry point.
fn persist_departures_endpoint_seeded(
    path: &Path,
    endpoint: Option<&str>,
    seed: &str,
) -> Result<(), PlacesError> {
    let existing = std::fs::read_to_string(path).unwrap_or_else(|_| seed.to_owned());
    let body = render_departures_endpoint(&existing, endpoint)?;
    config_file::write_atomic(path, &body, config_file::Durability::FsyncParent)
        .map_err(|e| PlacesError::Write(e.to_string()))
}

/// [`persist_departures_endpoint_to`] against the user's real
/// `~/.config/trollshell/places.toml`. No `check_base` counterpart here (as
/// [`save_to`] has for places): the key is a single scalar with no set of
/// things that could have drifted from underneath an edit the way
/// `[[place]]` can.
///
/// # Errors
/// [`PlacesError::NoConfigPath`] when `$HOME` is unset,
/// [`PlacesError::Locked`] when a base layer pinned [`ENDPOINT_KEY`] (#1227
/// item 2), else as [`persist_departures_endpoint_to`].
pub fn save_departures_endpoint(endpoint: Option<&str>) -> Result<(), PlacesError> {
    let path = config_path().ok_or(PlacesError::NoConfigPath)?;
    let locked = base_locks();
    refuse_if_locked(&locked, ENDPOINT_KEY)?;
    // The seed is filtered (#1338 review, H1). This writer is the one that
    // *reaches* the trap: `place` and `departures.endpoint` lock
    // independently, so with the list in nix and the backend left to the
    // operator this row is deliberately still sensitive — and an unfiltered
    // seed would put `DEFAULT_CONFIG`'s Schöneweide `[[place]]` into an
    // overlay nobody asked for, on one click, including on a `None` "clear
    // the endpoint" that writes no endpoint at all.
    persist_departures_endpoint_seeded(&path, endpoint, &seed_for(&locked))
}

/// Load the configured endpoint — [`load_layered`]'s `endpoint`.
///
/// Kept as its own function because the control center reads it on its own
/// (the entry row is built before the place list is), and because "the merged
/// endpoint" is exactly what both callers want: an overlay value wins over the
/// base layer's, an absent one falls through to it, and a locked one is nix's
/// whatever the overlay says.
///
/// `None` for an absent/blank key at every layer — every "can't tell" case
/// reads the same as "use the default", mirroring [`load_places`] falling back
/// to [`builtin_default`].
#[must_use]
pub fn load_departures_endpoint() -> Option<String> {
    load_layered().endpoint
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::subsystem::FindingKind;

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

        // `temp_env` serializes the mutation across tests and restores it
        // after. `with_layers` rather than `$HOME` alone: since #1227 item 2
        // both `load_places` and `ConfigWatcher` read `$XDG_CONFIG_DIRS`, so a
        // box that actually uses `programs.trollshell.config.places` would
        // otherwise feed this test a base layer it never asked for.
        with_layers(&root, &[], || {
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

    /// #1162 lens 7 item 1: a rewrite that lands at the **same length** and is
    /// then explicitly reset to the **same mtime** as the previous poll must
    /// still be detected. An mtime-only watcher (what `places::ConfigWatcher`
    /// was before this fix) would see no movement at all here and lose the
    /// edit forever — not just until the next poll, since the stamp is
    /// replaced unconditionally either way.
    ///
    /// Falsification: reverting [`stamp`] to compare mtime alone turns this
    /// red (`poll` returns `None` instead of the `Home2` reload) while
    /// leaving `config_watcher_reloads_only_on_changed_content` above green,
    /// which is exactly the gap #1162 named.
    #[test]
    fn config_watcher_detects_a_same_length_same_mtime_rewrite() {
        use std::time::UNIX_EPOCH;

        let root = std::env::temp_dir().join(format!("hytte-places-hash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".config/trollshell")).unwrap();

        // `with_layers`, not a bare `$HOME` — see the sibling test above.
        with_layers(&root, &[], || {
            let cfg = root.join(".config/trollshell/places.toml");
            let one = "[[place]]\nname = \"Home1\"\nlat = 1.0\nlon = 2.0\n";
            let two = "[[place]]\nname = \"Home2\"\nlat = 1.0\nlon = 2.0\n";
            assert_eq!(
                one.len(),
                two.len(),
                "the rewrite must not itself move the length"
            );

            let set_mtime = |secs: u64| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&cfg)
                    .unwrap()
                    .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
                    .unwrap();
            };

            std::fs::write(&cfg, one).unwrap();
            set_mtime(1);
            let mut watcher = ConfigWatcher::new();
            let current = load_places();
            assert_eq!(current[0].name, "Home1");

            // Same length, same mtime granule as the baseline poll — only the
            // content actually moved.
            std::fs::write(&cfg, two).unwrap();
            set_mtime(1);
            let reloaded = watcher
                .poll(&current)
                .expect("content changed even though mtime and length did not");
            assert_eq!(reloaded[0].name, "Home2");
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

        assert!(matches!(
            read_on_disk(&target, &builtin_default()),
            OnDisk::Absent
        ));

        // A config that yields no places is reported in the same units memory
        // holds it in — what `load_places` makes of it, i.e. the default.
        std::fs::write(&target, "place = []\n").expect("seed");
        assert!(
            matches!(read_on_disk(&target, &builtin_default()), OnDisk::Places(p) if p == builtin_default())
        );
        std::fs::write(&target, "# only comments\n").expect("seed");
        assert!(
            matches!(read_on_disk(&target, &builtin_default()), OnDisk::Places(p) if p == builtin_default())
        );

        // A real config comes back as itself.
        std::fs::write(
            &target,
            "[[place]]\nname = \"Home\"\nlat = 1.0\nlon = 2.0\n",
        )
        .expect("seed");
        assert!(
            matches!(read_on_disk(&target, &builtin_default()), OnDisk::Places(p) if p.len() == 1 && p[0].name == "Home")
        );

        // And the two `load_places` collapses into a silent fallback stay
        // distinguishable here: unparseable, and undecodable.
        std::fs::write(&target, "[[place]]\nname = \"Home\"\nlat = \n").expect("seed");
        assert!(matches!(
            read_on_disk(&target, &builtin_default()),
            OnDisk::Unknown(_)
        ));
        std::fs::write(&target, [0xff, 0xfe]).expect("seed");
        assert!(matches!(
            read_on_disk(&target, &builtin_default()),
            OnDisk::Unknown(_)
        ));
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

    // ── Departures endpoint (#1124) ───────────────────────────────────────────

    #[test]
    fn resolve_departures_endpoint_defaults_to_bvg_when_absent_or_blank() {
        assert_eq!(
            resolve_departures_endpoint(None).unwrap(),
            "https://v6.bvg.transport.rest"
        );
        assert_eq!(
            resolve_departures_endpoint(Some("   ")).unwrap(),
            "https://v6.bvg.transport.rest"
        );
    }

    #[test]
    fn resolve_departures_endpoint_maps_the_three_short_names() {
        assert_eq!(
            resolve_departures_endpoint(Some("bvg")).unwrap(),
            "https://v6.bvg.transport.rest"
        );
        assert_eq!(
            resolve_departures_endpoint(Some("vbb")).unwrap(),
            "https://v6.vbb.transport.rest"
        );
        assert_eq!(
            resolve_departures_endpoint(Some("db")).unwrap(),
            "https://v6.db.transport.rest"
        );
    }

    #[test]
    fn resolve_departures_endpoint_accepts_a_full_url_verbatim() {
        assert_eq!(
            resolve_departures_endpoint(Some("https://v6.hvv.transport.rest/")).unwrap(),
            "https://v6.hvv.transport.rest",
            "trailing slash trimmed"
        );
    }

    #[test]
    fn resolve_departures_endpoint_rejects_an_unknown_name_with_a_hint() {
        let err = resolve_departures_endpoint(Some("hamburg")).unwrap_err();
        assert_eq!(
            err,
            PlacesError::Endpoint {
                value: "hamburg".to_owned()
            }
        );
        let text = err.to_string();
        assert!(
            text.contains("bvg") && text.contains("vbb") && text.contains("db"),
            "hint should list the three names, got: {text}"
        );
    }

    #[test]
    fn validate_departures_endpoint_agrees_with_resolve() {
        assert!(validate_departures_endpoint(None).is_ok());
        assert!(validate_departures_endpoint(Some("db")).is_ok());
        assert!(validate_departures_endpoint(Some("hamburg")).is_err());
    }

    #[test]
    fn parse_departures_endpoint_reads_absent_present_and_blank() {
        assert_eq!(parse_departures_endpoint("").unwrap(), None);
        assert_eq!(
            parse_departures_endpoint("[[place]]\nname = \"Home\"\n").unwrap(),
            None,
            "no [departures] table at all"
        );
        assert_eq!(
            parse_departures_endpoint("[departures]\nendpoint = \"vbb\"\n").unwrap(),
            Some("vbb".to_owned())
        );
        assert_eq!(
            parse_departures_endpoint("[departures]\nendpoint = \"  \"\n").unwrap(),
            None,
            "a blank value reads the same as absent"
        );
    }

    #[test]
    fn parse_departures_endpoint_malformed_is_err() {
        assert!(parse_departures_endpoint("[departures]\nendpoint = ").is_err());
    }

    /// A file with no `[departures]` table, an unrelated top-level table and a
    /// `[[place]]` array — the same shape [`FIXTURE`] in
    /// `places_byte_identical.rs` pins, kept local here so this file's own
    /// tests don't depend on an integration test's fixture.
    const NO_DEPARTURES_TABLE: &str = "# A comment.\n\n[[place]]\nname = \"Home\"\nlat = 1.0\nlon = 2.0\n\n[unrelated]\nkept = true\n";

    #[test]
    fn render_departures_endpoint_absent_key_leaves_a_table_less_file_untouched() {
        // The writer's half of the byte-identical promise: nothing to do when
        // there is no key and nothing was asked for.
        let rendered = render_departures_endpoint(NO_DEPARTURES_TABLE, None).unwrap();
        assert_eq!(rendered, NO_DEPARTURES_TABLE);
    }

    #[test]
    fn render_departures_endpoint_writes_a_fresh_table_and_preserves_the_rest() {
        let rendered = render_departures_endpoint(NO_DEPARTURES_TABLE, Some("vbb")).unwrap();
        assert!(rendered.contains("[departures]"));
        assert!(rendered.contains("endpoint = \"vbb\""));
        // Everything else — the comment, the place, the unrelated table —
        // survives untouched.
        assert!(rendered.contains("# A comment."));
        assert!(rendered.contains("name = \"Home\""));
        assert!(rendered.contains("[unrelated]\nkept = true"));
        // A reparse gets the same value back.
        assert_eq!(
            parse_departures_endpoint(&rendered).unwrap(),
            Some("vbb".to_owned())
        );
    }

    #[test]
    fn render_departures_endpoint_updates_an_existing_key_and_keeps_its_comment() {
        let existing = "# a hand comment on the key\n[departures]\nendpoint = \"vbb\" # was vbb\n";
        let rendered = render_departures_endpoint(existing, Some("db")).unwrap();
        assert!(rendered.contains("# a hand comment on the key"));
        assert!(rendered.contains("endpoint = \"db\""));
        assert!(
            !rendered.contains("was vbb"),
            "the trailing value comment does not survive a changed value, matching `station`"
        );
    }

    #[test]
    fn render_departures_endpoint_none_removes_the_key_and_the_table() {
        let existing = "[departures]\nendpoint = \"vbb\"\n";
        let rendered = render_departures_endpoint(existing, None).unwrap();
        assert_eq!(parse_departures_endpoint(&rendered).unwrap(), None);
        assert!(
            !rendered.contains("[departures]"),
            "an emptied table is dropped entirely, got: {rendered}"
        );
    }

    #[test]
    fn render_departures_endpoint_none_keeps_a_departures_table_with_other_keys() {
        let existing = "[departures]\nendpoint = \"vbb\"\nunrelated_future_key = true\n";
        let rendered = render_departures_endpoint(existing, None).unwrap();
        assert_eq!(parse_departures_endpoint(&rendered).unwrap(), None);
        assert!(
            rendered.contains("[departures]") && rendered.contains("unrelated_future_key"),
            "a table with a key this model doesn't own survives, got: {rendered}"
        );
    }

    #[test]
    fn render_departures_endpoint_rejects_an_invalid_value_and_touches_nothing() {
        let err = render_departures_endpoint(NO_DEPARTURES_TABLE, Some("hamburg")).unwrap_err();
        assert_eq!(
            err,
            PlacesError::Endpoint {
                value: "hamburg".to_owned()
            }
        );
    }

    #[test]
    fn render_departures_endpoint_refuses_a_non_table_departures_key() {
        let existing = "departures = \"not a table\"\n";
        assert!(matches!(
            render_departures_endpoint(existing, Some("vbb")),
            Err(PlacesError::Encode(_))
        ));
    }

    #[test]
    fn persist_departures_endpoint_to_writes_and_reads_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("places.toml");
        std::fs::write(&path, NO_DEPARTURES_TABLE).expect("seed");

        persist_departures_endpoint_to(&path, Some("db")).expect("persist");
        let saved = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(
            parse_departures_endpoint(&saved).unwrap(),
            Some("db".to_owned())
        );
        // The rest of the file is untouched.
        assert!(saved.contains("name = \"Home\""));
        assert!(saved.contains("[unrelated]\nkept = true"));

        // An invalid value is refused and the file is left exactly as it was.
        let before = std::fs::read_to_string(&path).expect("readable");
        assert!(persist_departures_endpoint_to(&path, Some("hamburg")).is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("readable"), before);

        // Unsetting it again round-trips to the table-less shape.
        persist_departures_endpoint_to(&path, None).expect("unset");
        let cleared = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(parse_departures_endpoint(&cleared).unwrap(), None);
        assert!(!cleared.contains("[departures]"));
    }

    #[test]
    fn save_departures_endpoint_no_config_path_without_home() {
        temp_env::with_var_unset("HOME", || {
            assert_eq!(
                save_departures_endpoint(Some("vbb")),
                Err(PlacesError::NoConfigPath)
            );
        });
    }

    // ── The nix base layer (#1227 item 2) ────────────────────────────────────

    /// A scratch `$HOME` **and** a scratch `$XDG_CONFIG_DIRS`, so no test here
    /// can read — or be perturbed by — the developer's real base layer.
    ///
    /// #1227 item 2 made `places` read `$XDG_CONFIG_DIRS`, and on a box that
    /// sets `programs.trollshell.config.places` there is a real
    /// `/etc/xdg/trollshell/places.toml` these tests never asked for. Pinning
    /// the variable is the difference between "this suite is hermetic" and
    /// "this suite is hermetic on machines that do not use the feature it is
    /// testing" (#1101's rule, one directory over).
    ///
    /// An **empty** `base_dirs` cannot be spelled as an empty variable — the
    /// XDG spec (and [`crate::xdg`]) reads that as unset and falls back to
    /// `/etc/xdg` — so "no base layer" is a scratch directory that does not
    /// exist.
    fn with_layers<R>(home: &Path, base_dirs: &[&Path], body: impl FnOnce() -> R) -> R {
        let fallback = home.join("no-base-layer-here");
        let dirs = if base_dirs.is_empty() {
            fallback.display().to_string()
        } else {
            base_dirs
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(":")
        };
        temp_env::with_vars(
            [
                ("HOME", Some(std::ffi::OsString::from(home))),
                ("XDG_CONFIG_HOME", None),
                ("XDG_CONFIG_DIRS", Some(std::ffi::OsString::from(dirs))),
            ],
            body,
        )
    }

    /// `(path, body)` for a layer, without touching the filesystem — what
    /// [`assemble_places`] takes.
    fn layer(name: &str, body: &str) -> (PathBuf, String) {
        (
            PathBuf::from(format!("/nix/store/{name}.toml")),
            body.to_owned(),
        )
    }

    /// Write `body` to `<dir>/trollshell/places.toml` and hand back `dir`, so a
    /// scratch base directory reads like the real one.
    fn base_dir(dir: &Path, body: &str) -> PathBuf {
        let app = dir.join("trollshell");
        std::fs::create_dir_all(&app).expect("mkdir base layer");
        std::fs::write(app.join("places.toml"), body).expect("write base layer");
        dir.to_path_buf()
    }

    /// A rendered base layer with the list unlocked — what a hand-written
    /// `/etc/xdg/trollshell/places.toml` looks like.
    const BASE_TWO: &str = "\
[[place]]
name = \"Werkstatt\"
lat = 52.5
lon = 13.4

[[place]]
name = \"Bahnhof\"
lat = 52.4
lon = 13.5
";

    /// The same, as `programs.trollshell.config.places.place` renders it:
    /// `_locked` beside the values.
    const NIX_TWO: &str = "\
_locked = [\"place\"]

[[place]]
name = \"Werkstatt\"
lat = 52.5
lon = 13.4

[[place]]
name = \"Bahnhof\"
lat = 52.4
lon = 13.5
";

    /// The base layer supplies the list when nothing else does — the whole
    /// point of the option.
    ///
    /// **Mutation:** make `assemble_places` ignore `bases` (fold only the
    /// default and the overlay) and this is the first thing to red.
    #[test]
    fn a_base_layer_supplies_the_place_list_with_no_overlay() {
        let loaded = assemble_places(&[layer("base", NIX_TWO)], None);

        assert_eq!(
            loaded
                .places
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["Werkstatt", "Bahnhof"],
            "the nix base layer's array, not the built-in default's"
        );
        assert_ne!(
            loaded.places,
            builtin_default(),
            "if this passes trivially the fixture stopped differing from the default"
        );
    }

    /// Rule 3, stated for this file: the overlay's `place` array **replaces**
    /// the base's, it does not append to it.
    ///
    /// **Mutation:** read the overlay as the base and the base as the overlay
    /// (swap the two arguments) and this reds — it would report the nix list.
    #[test]
    fn the_overlays_array_replaces_the_bases_whole() {
        let overlay = layer(
            "overlay",
            "[[place]]\nname = \"Zuhause\"\nlat = 1.0\nlon = 2.0\n",
        );

        let loaded = assemble_places(&[layer("base", BASE_TWO)], Some(&overlay));

        assert_eq!(
            loaded
                .places
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["Zuhause"],
            "replace, never append (rule 3) — and never the base's two"
        );
    }

    /// The other direction of the same rule: an overlay that mentions no
    /// `place` at all inherits the base's array untouched. This is the shape a
    /// `[departures]`-only overlay has, which is what the endpoint row alone
    /// can produce.
    #[test]
    fn an_overlay_without_places_inherits_the_bases_array() {
        let overlay = layer("overlay", "[departures]\nendpoint = \"vbb\"\n");

        let loaded = assemble_places(&[layer("base", BASE_TWO)], Some(&overlay));

        assert_eq!(
            loaded
                .places
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["Werkstatt", "Bahnhof"]
        );
        assert_eq!(loaded.endpoint.as_deref(), Some("vbb"));
    }

    /// Rule 1 for the one scalar this file has: present in the overlay wins,
    /// absent falls through to the base.
    #[test]
    fn the_endpoint_overlays_when_present_and_falls_through_when_absent() {
        let base = layer("base", "[departures]\nendpoint = \"db\"\n");

        let inherited = assemble_places(std::slice::from_ref(&base), None);
        assert_eq!(inherited.endpoint.as_deref(), Some("db"));

        let overridden = assemble_places(
            std::slice::from_ref(&base),
            Some(&layer("overlay", "[departures]\nendpoint = \"bvg\"\n")),
        );
        assert_eq!(
            overridden.endpoint.as_deref(),
            Some("bvg"),
            "unlocked: the overlay wins"
        );
    }

    /// #1331's rule applied to this file: the base layer's `_locked` binds the
    /// overlay, the base value is kept, and the attempt is reported once — in
    /// the house wording, with the subsystem spelled `places`.
    #[test]
    fn a_locked_place_array_keeps_the_base_list_and_reports_once() {
        let (captured, _guard) = crate::test_support::capture();

        let loaded = assemble_places(
            &[layer("base", NIX_TWO)],
            Some(&layer(
                "overlay",
                "[[place]]\nname = \"Zuhause\"\nlat = 1.0\nlon = 2.0\n",
            )),
        );

        assert_eq!(
            loaded
                .places
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["Werkstatt", "Bahnhof"],
            "nix has precedence per key (#866, 2026-09-15)"
        );
        let refusals: Vec<&Finding> = loaded
            .lock_findings
            .iter()
            .filter(|f| f.kind == FindingKind::ShadowedLockedKey)
            .collect();
        assert_eq!(refusals.len(), 1, "one mistake, one line to read");
        assert_eq!(refusals[0].key, PLACE_KEY);
        assert!(
            refusals[0]
                .message
                .starts_with("places.place is set in nix and cannot be overridden from"),
            "the house sentence, with this file's own subsystem name: {}",
            refusals[0].message
        );
        assert_eq!(
            captured
                .warnings()
                .iter()
                .filter(|m| m.contains("cannot be overridden from an overlay"))
                .count(),
            1,
            "and one journal line, not two"
        );
    }

    /// An `_unset` at a locked key is an override attempt like any other.
    /// Without this the lock is one line from meaningless: `place` would fall
    /// through to `DEFAULT_CONFIG`, exactly the value nix was displacing.
    #[test]
    fn an_unset_cannot_erase_a_locked_place_array() {
        let loaded = assemble_places(
            &[layer("base", NIX_TWO)],
            Some(&layer("overlay", "_unset = [\"place\"]\n")),
        );

        assert_eq!(
            loaded
                .places
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["Werkstatt", "Bahnhof"]
        );
        assert_ne!(loaded.places, builtin_default());
    }

    /// The two keys lock independently — which is why the control center
    /// carries two booleans rather than one "managed by nix" flag.
    #[test]
    fn the_endpoint_locks_without_locking_the_place_array() {
        let loaded = assemble_places(
            &[layer(
                "base",
                "_locked = [\"departures.endpoint\"]\n[departures]\nendpoint = \"db\"\n",
            )],
            Some(&layer(
                "overlay",
                "[departures]\nendpoint = \"bvg\"\n\
                 [[place]]\nname = \"Zuhause\"\nlat = 1.0\nlon = 2.0\n",
            )),
        );

        assert_eq!(loaded.endpoint.as_deref(), Some("db"), "the nix endpoint");
        assert_eq!(
            loaded
                .places
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["Zuhause"],
            "…and the list is still the operator's"
        );
        assert!(loaded.endpoint_is_locked());
        assert!(!loaded.places_are_locked());
    }

    /// The lock set reaches the caller as data, which is what the control
    /// center greys rows from.
    ///
    /// **Mutation (seam):** return `locked: BTreeSet::new()` from
    /// `assemble_places` — the merge still enforces the lock, but no editor
    /// can learn which keys are pinned — and this plus the two save tests red.
    #[test]
    fn the_lock_set_reaches_the_caller_as_data() {
        let loaded = assemble_places(&[layer("base", NIX_TWO)], None);

        assert_eq!(
            loaded.locked.iter().map(String::as_str).collect::<Vec<_>>(),
            [PLACE_KEY]
        );
        assert!(loaded.places_are_locked());
        assert!(!loaded.endpoint_is_locked());
        assert!(loaded.is_locked(PLACE_KEY));
    }

    /// #1331 review, HIGH 1, for this file: the **overlay's own** `_locked`
    /// binds nothing and is not in the returned set. Otherwise a save driven by
    /// that set would drop the very value the operator just edited.
    #[test]
    fn an_overlays_own_lock_is_absent_from_the_returned_set() {
        let loaded = assemble_places(
            &[],
            Some(&layer(
                "overlay",
                "_locked = [\"place\"]\n[[place]]\nname = \"Zuhause\"\nlat = 1.0\nlon = 2.0\n",
            )),
        );

        assert!(
            loaded.locked.is_empty(),
            "there is no layer above the overlay"
        );
        assert!(!loaded.places_are_locked());
        assert_eq!(
            loaded
                .places
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["Zuhause"]
        );
    }

    /// A marker whose shape the fold cannot read pins nothing, and says so
    /// naming the layer — `merge::malformed_locked`'s argument is worse here
    /// than anywhere: the file is nix's and nobody opens it.
    #[test]
    fn a_malformed_locked_marker_in_the_base_is_reported() {
        let loaded = assemble_places(
            &[layer(
                "base",
                "_locked = \"place\"\n[[place]]\nname = \"W\"\nlat = 1.0\nlon = 2.0\n",
            )],
            Some(&layer(
                "overlay",
                "[[place]]\nname = \"Zuhause\"\nlat = 1.0\nlon = 2.0\n",
            )),
        );

        assert!(
            loaded
                .lock_findings
                .iter()
                .any(|f| f.kind == FindingKind::MalformedLocked && f.key == merge::LOCKED_KEY),
            "{:?}",
            loaded.lock_findings
        );
        assert_eq!(
            loaded
                .places
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["Zuhause"],
            "a marker that pins nothing does not pin anything"
        );
    }

    /// …and one naming a key its own layer does not set is reported too
    /// (#1331 review, LOW 4).
    #[test]
    fn an_inert_locked_marker_in_the_base_is_reported() {
        let loaded = assemble_places(
            &[layer("base", "_locked = [\"departures.endpoint\"]\n")],
            None,
        );

        assert!(
            loaded
                .lock_findings
                .iter()
                .any(|f| f.kind == FindingKind::InertLocked && f.key == ENDPOINT_KEY)
        );
        assert!(loaded.locked.is_empty());
    }

    /// A layer that is not TOML costs that layer, not the stack — the
    /// per-layer reading of `load_places`' old whole-file fallback.
    #[test]
    fn a_layer_that_is_not_toml_is_skipped_rather_than_fatal() {
        let loaded = assemble_places(
            &[layer("base", BASE_TWO)],
            Some(&layer("overlay", "this is not = = toml\n")),
        );

        assert_eq!(
            loaded
                .places
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["Werkstatt", "Bahnhof"],
            "the nix base layer still applies"
        );
    }

    /// With no file anywhere the answer is still the documented default.
    #[test]
    fn no_layer_at_all_is_the_builtin_default() {
        assert_eq!(assemble_places(&[], None).places, builtin_default());
    }

    /// An overlay that empties the array reads back as the built-in default —
    /// `load_places`' rule since #640, unchanged by the layering, and the rule
    /// `remove_place`'s doc in `hytte-services` leans on.
    #[test]
    fn an_emptied_overlay_array_still_reads_back_as_the_default() {
        let loaded = assemble_places(
            &[layer("base", BASE_TWO)],
            Some(&layer("overlay", "place = []\n")),
        );

        assert_eq!(loaded.places, builtin_default());
    }

    /// Backs the nix option's `either float int` (`nix/module-common.nix`): a
    /// coordinate written without a decimal point in the rendered base layer
    /// must still read back as an `f64`, or every `lat = 52` an operator types
    /// would fail the schema at load time rather than at eval.
    #[test]
    fn an_integer_coordinate_reads_back_as_a_float() {
        let loaded = assemble_places(
            &[layer(
                "base",
                "[[place]]\nname = \"W\"\nlat = 52\nlon = 13\nradius_km = 9\n",
            )],
            None,
        );

        assert_eq!(loaded.places.len(), 1);
        assert!((loaded.places[0].lat - 52.0).abs() < f64::EPSILON);
        assert!((loaded.places[0].lon - 13.0).abs() < f64::EPSILON);
        assert!((loaded.places[0].radius_km - 9.0).abs() < f64::EPSILON);
    }

    /// The path split this module argues for in its own docs: the bases come
    /// from `$XDG_CONFIG_DIRS`, the overlay is `config_path()`, and the overlay
    /// is last.
    #[test]
    fn the_layer_paths_are_the_bases_then_the_writers_own_file() {
        let home = std::env::temp_dir().join(format!("places-paths-{}", std::process::id()));
        let one = home.join("one");
        let two = home.join("two");

        with_layers(&home, &[one.as_path(), two.as_path()], || {
            assert_eq!(
                layer_paths(),
                vec![
                    two.join("trollshell/places.toml"),
                    one.join("trollshell/places.toml"),
                    config_path().expect("$HOME is set"),
                ],
                "XDG order reversed (lowest precedence first), overlay last"
            );
            assert_eq!(
                config_path(),
                Some(home.join(".config/trollshell/places.toml"))
            );
        });
    }

    /// First run with a nix base layer must NOT write the documented default
    /// into the overlay: that file's array would replace nix's (rule 3), and
    /// when nix locked it every subsequent load would refuse it — one journal
    /// line per load, forever, about a file nobody wrote on purpose.
    #[test]
    fn a_first_run_under_a_nix_base_layer_writes_no_overlay() {
        let root = std::env::temp_dir().join(format!("places-firstrun-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = base_dir(&root.join("xdg"), NIX_TWO);

        with_layers(&root, &[dir.as_path()], || {
            let loaded = load_places();
            assert_eq!(
                loaded.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
                ["Werkstatt", "Bahnhof"]
            );
            assert!(
                !config_path().expect("$HOME").exists(),
                "nix already supplied the list; nothing to seed"
            );
        });

        // …and with no base layer the first-run write still happens, so the
        // suppression above is about nix rather than about the write going
        // away.
        let bare =
            std::env::temp_dir().join(format!("places-firstrun-bare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&bare);
        with_layers(&bare, &[], || {
            assert_eq!(load_places(), builtin_default());
            assert!(
                config_path().expect("$HOME").exists(),
                "the seed still happens"
            );
        });

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&bare);
    }

    /// The `check_base` hole #1227 item 2 opened and closed in the same
    /// change: an overlay that parses to **zero** places (a `[departures]`-only
    /// file, which the endpoint row alone can create) must be classified as
    /// what the reader reports — the base layer's list — or every save is
    /// refused as `ChangedOnDisk` against a file nothing changed.
    #[test]
    fn an_overlay_with_no_places_is_classified_as_the_base_layers_list() {
        let root = std::env::temp_dir().join(format!("places-ondisk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = base_dir(
            &root.join("xdg"),
            "[[place]]\nname = \"W\"\nlat = 1.0\nlon = 2.0\n",
        );
        let overlay = root.join(".config/trollshell/places.toml");
        std::fs::create_dir_all(overlay.parent().expect("parent")).expect("mkdir");
        std::fs::write(&overlay, "[departures]\nendpoint = \"vbb\"\n").expect("write");

        with_layers(&root, &[dir.as_path()], || {
            let merged = load_places();
            assert_eq!(
                merged.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
                ["W"]
            );
            assert_eq!(
                check_base_layered(&overlay, &merged),
                Ok(()),
                "disk and memory must be compared in the same units"
            );
            // …and the explicit-path `check_base` deliberately does NOT read
            // the base layer, which is what keeps it (and every test that goes
            // through it, `tests/places_byte_identical.rs` included) hermetic.
            assert_eq!(
                check_base(&overlay, &merged),
                Err(PlacesError::ChangedOnDisk),
                "the pure arm compares against the built-in default, by design"
            );
        });

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A save the next load would refuse is refused up front, and leaves the
    /// overlay untouched.
    ///
    /// **Mutation:** drop the `check_unlocked` call from `save` and this reds —
    /// the write lands and the file changes.
    #[test]
    fn a_save_is_refused_while_nix_owns_the_list() {
        let root = std::env::temp_dir().join(format!("places-lockedsave-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = base_dir(&root.join("xdg"), NIX_TWO);

        with_layers(&root, &[dir.as_path()], || {
            let base = load_places();
            let mut next = base.clone();
            next.push(Place {
                name: "Drittes".into(),
                lat: 1.0,
                lon: 2.0,
                radius_km: default_radius_km(),
                ssids: Vec::new(),
                match_min: default_match_min(),
                station: None,
                walk_minutes: 0,
                lines: Vec::new(),
                directions: Vec::new(),
            });

            assert_eq!(
                save(&base, next),
                Err(PlacesError::Locked {
                    key: PLACE_KEY.to_owned()
                })
            );
            assert!(
                !config_path().expect("$HOME").exists(),
                "a refused save writes nothing at all"
            );
            assert_eq!(
                save_departures_endpoint(Some("vbb")),
                Ok(()),
                "…and the unlocked key is still writable"
            );
        });

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same guard read directly — `hytte_services::places::edit` composes
    /// its own write path and calls this rather than `save`.
    #[test]
    fn check_unlocked_answers_for_each_key_separately() {
        let root =
            std::env::temp_dir().join(format!("places-checkunlocked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = base_dir(&root.join("xdg"), NIX_TWO);

        with_layers(&root, &[dir.as_path()], || {
            assert!(check_unlocked(PLACE_KEY).is_err());
            assert_eq!(check_unlocked(ENDPOINT_KEY), Ok(()));
        });

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The watcher stamps the base layer too, so a rebuild that swaps the store
    /// path is picked up by the same tick a hand edit is.
    #[test]
    fn the_watcher_sees_the_base_layer_move() {
        let root = std::env::temp_dir().join(format!("places-watchbase-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = base_dir(
            &root.join("xdg"),
            "[[place]]\nname = \"Eins\"\nlat = 1.0\nlon = 2.0\n",
        );

        with_layers(&root, &[dir.as_path()], || {
            let mut watcher = ConfigWatcher::new();
            let current = load_places();
            assert_eq!(current.len(), 1);
            assert!(watcher.poll(&current).is_none());

            base_dir(
                &root.join("xdg"),
                "[[place]]\nname = \"Eins\"\nlat = 1.0\nlon = 2.0\n\
                 [[place]]\nname = \"Zwei\"\nlat = 3.0\nlon = 4.0\n",
            );
            let reloaded = watcher
                .poll(&current)
                .expect("the base layer moved → reload");
            assert_eq!(reloaded.len(), 2);
        });

        let _ = std::fs::remove_dir_all(&root);
    }

    // ── #1338 review: the fix round ─────────────────────────────────────────

    /// H1: the *other* writer must not create the very overlay
    /// [`load_places`]' first-run suppression exists to avoid.
    ///
    /// Seeding [`DEFAULT_CONFIG`] here writes its `[[place]]` block into a file
    /// whose `place` array the next merge refuses — one journal line per load,
    /// forever, about a file the operator never asked for. It is reachable on
    /// one click, because `place` and `departures.endpoint` lock
    /// independently and this row is deliberately still sensitive.
    ///
    /// **Mutation:** pass `DEFAULT_CONFIG` instead of `seed_for(&locked)` in
    /// `save_departures_endpoint` and this reds on the `[[place]]` assertion.
    #[test]
    fn an_endpoint_save_writes_no_place_block_while_nix_owns_the_list() {
        let root = std::env::temp_dir().join(format!("places-epseed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = base_dir(&root.join("xdg"), NIX_TWO);

        with_layers(&root, &[dir.as_path()], || {
            assert_eq!(save_departures_endpoint(Some("vbb")), Ok(()));

            let overlay = std::fs::read_to_string(config_path().expect("$HOME"))
                .expect("the endpoint save created the overlay");
            assert!(
                parse_places(&overlay).expect("valid TOML").is_empty(),
                "a save of the endpoint must not seed a list nix owns:\n{overlay}"
            );
            assert!(
                load_layered().lock_findings.is_empty(),
                "…and the next load must have nothing to refuse: {:?}",
                load_layered().lock_findings
            );
            // The control: the key it *was* asked to write is still there…
            assert_eq!(load_departures_endpoint().as_deref(), Some("vbb"));
            // …and the seed's documented preamble survived the filtering, which
            // is why the seed is filtered rather than skipped.
            assert!(
                overlay.contains("# trollshell places"),
                "the documented preamble is the reason to seed at all:\n{overlay}"
            );
        });

        let _ = std::fs::remove_dir_all(&root);
    }

    /// H1's second arm: `save_departures_endpoint(None)` — "clear the
    /// endpoint", which writes no endpoint key at all — seeded the same block.
    #[test]
    fn clearing_the_endpoint_writes_no_place_block_either() {
        let root = std::env::temp_dir().join(format!("places-epclear-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = base_dir(&root.join("xdg"), NIX_TWO);

        with_layers(&root, &[dir.as_path()], || {
            assert_eq!(save_departures_endpoint(None), Ok(()));
            let overlay = std::fs::read_to_string(config_path().expect("$HOME"))
                .expect("even a clear creates the overlay");
            assert!(
                parse_places(&overlay).expect("valid TOML").is_empty(),
                "{overlay}"
            );
            assert!(load_layered().lock_findings.is_empty());
        });

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The seed filter itself, as a pure function: locked keys out, everything
    /// else — the preamble above all — kept.
    #[test]
    fn the_seed_drops_exactly_the_locked_keys() {
        let unfiltered = seed_for(&BTreeSet::new());
        assert_eq!(unfiltered, DEFAULT_CONFIG, "nothing locked, nothing to do");

        let mut locked = BTreeSet::new();
        locked.insert(PLACE_KEY.to_owned());
        let filtered = seed_for(&locked);

        assert!(
            parse_places(&filtered)
                .expect("still valid TOML")
                .is_empty(),
            "the array is gone, not emptied of fields:\n{filtered}"
        );
        // Asserted on a *header at the start of a line*, not on the substring:
        // the documented preamble says "the FIRST [[place]] is used as home",
        // and a bare `contains` matches that comment — which is precisely the
        // text this filter has to keep.
        assert!(
            !filtered
                .lines()
                .any(|l| l.trim_start().starts_with("[[place]]")),
            "no [[place]] block survives:\n{filtered}"
        );
        assert!(
            filtered.contains("# trollshell places"),
            "the documented preamble survives the removal of the block it was attached to"
        );
        // And a key `DEFAULT_CONFIG` does not set costs nothing.
        let mut absent = BTreeSet::new();
        absent.insert(ENDPOINT_KEY.to_owned());
        assert_eq!(seed_for(&absent), DEFAULT_CONFIG);
    }

    /// M1: the endpoint wrapper's own refusal. `check_unlocked`'s body was
    /// pinned; the call site was not, and deleting it left every suite green
    /// (#1338 review, M1).
    ///
    /// **Mutation:** delete `refuse_if_locked(&locked, ENDPOINT_KEY)?;` from
    /// `save_departures_endpoint` and this reds.
    #[test]
    fn an_endpoint_save_is_refused_while_nix_owns_the_endpoint() {
        let root = std::env::temp_dir().join(format!("places-eplock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = base_dir(
            &root.join("xdg"),
            "_locked = [\"departures.endpoint\"]\n[departures]\nendpoint = \"db\"\n",
        );

        with_layers(&root, &[dir.as_path()], || {
            assert_eq!(
                save_departures_endpoint(Some("vbb")),
                Err(PlacesError::Locked {
                    key: ENDPOINT_KEY.to_owned()
                })
            );
            assert!(
                !config_path().expect("$HOME").exists(),
                "a refused save writes nothing at all"
            );
            assert_eq!(
                load_departures_endpoint().as_deref(),
                Some("db"),
                "and the nix value still stands"
            );
        });

        let _ = std::fs::remove_dir_all(&root);
    }

    /// H2: a base layer that appears, vanishes, or only changes its `_locked`
    /// line moves no place — so a watcher that dedups on the list reports
    /// nothing, and every editor that refreshes its lock state inside
    /// `if let Some(..) = poll(..)` keeps showing the previous answer. The
    /// greyed row would then outlive the option that greyed it.
    ///
    /// Both halves are asserted: `poll` (the shell's, list-dedup) is right to
    /// say nothing, and `moved` (the editors') must say something.
    #[test]
    fn a_lock_that_vanishes_without_moving_the_list_is_still_seen() {
        const LIST: &str = "[[place]]\nname = \"Eins\"\nlat = 1.0\nlon = 2.0\n";

        let root = std::env::temp_dir().join(format!("places-lockmove-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = base_dir(&root.join("xdg"), &format!("_locked = [\"place\"]\n{LIST}"));

        with_layers(&root, &[dir.as_path()], || {
            let overlay = config_path().expect("$HOME");
            std::fs::create_dir_all(overlay.parent().expect("parent")).expect("mkdir");
            std::fs::write(&overlay, LIST).expect("the operator's own copy of the same list");

            let mut watcher = ConfigWatcher::new();
            let mut list_watcher = ConfigWatcher::new();
            let before = load_layered();
            assert!(before.places_are_locked());
            assert!(!watcher.moved(), "nothing has moved yet");

            // `nixos-rebuild` drops the option: the lock goes, the list does not.
            std::fs::remove_file(dir.join("trollshell/places.toml")).expect("rm base layer");

            assert!(
                list_watcher.poll(&before.places).is_none(),
                "the list is unchanged, so the SHELL's watcher is right to say nothing"
            );
            assert!(
                watcher.moved(),
                "a layer moved — the editors must get the chance to re-read the lock"
            );
            assert!(!load_layered().places_are_locked());
            // The control, so a `moved` that always fires cannot pass:
            assert!(!watcher.moved(), "nothing moved since");
        });

        let _ = std::fs::remove_dir_all(&root);
    }

    /// H2's other direction, and the common one: a `[departures]`-only base
    /// layer never renders a `[[place]]`, so adding it moves no place at all.
    #[test]
    fn an_endpoint_lock_that_appears_without_moving_the_list_is_still_seen() {
        let root = std::env::temp_dir().join(format!("places-eplockmove-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let xdg = root.join("xdg");
        std::fs::create_dir_all(xdg.join("trollshell")).expect("mkdir");

        with_layers(&root, &[xdg.as_path()], || {
            let overlay = config_path().expect("$HOME");
            std::fs::create_dir_all(overlay.parent().expect("parent")).expect("mkdir");
            std::fs::write(
                &overlay,
                "[departures]\nendpoint = \"bvg\"\n\
                 [[place]]\nname = \"Eins\"\nlat = 1.0\nlon = 2.0\n",
            )
            .expect("overlay");

            let mut watcher = ConfigWatcher::new();
            let before = load_layered();
            assert!(!before.endpoint_is_locked());
            assert_eq!(before.endpoint.as_deref(), Some("bvg"));

            base_dir(
                &xdg,
                "_locked = [\"departures.endpoint\"]\n[departures]\nendpoint = \"vbb\"\n",
            );

            assert!(watcher.moved(), "the base layer appeared");
            let after = load_layered();
            assert!(after.endpoint_is_locked());
            assert_eq!(after.endpoint.as_deref(), Some("vbb"));
            assert_eq!(
                after
                    .places
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>(),
                ["Eins"],
                "…and not one place moved, which is why `poll` could not see this"
            );
        });

        let _ = std::fs::remove_dir_all(&root);
    }

    /// M2: `programs.trollshell.config.places.place = [ ];` renders
    /// `place = []` beside `_locked = ["place"]`. Rule 3 says an array
    /// replaces whole, so that says **no places** — answering with the
    /// built-in Berlin default would be the one list the operator provably did
    /// not ask for, pinned, under a tab saying the places come from nix.
    #[test]
    fn an_empty_array_from_a_base_layer_means_no_places() {
        let loaded = assemble_places(
            &[layer("base", "_locked = [\"place\"]\nplace = []\n")],
            None,
        );

        assert!(loaded.places.is_empty(), "{:?}", loaded.places);
        assert!(loaded.places_are_locked());
        assert!(
            loaded.lock_findings.is_empty(),
            "nothing was refused: {:?}",
            loaded.lock_findings
        );

        // …and unlocked too: it is rule 3 that decides this, not the lock.
        assert!(
            assemble_places(&[layer("base", "place = []\n")], None)
                .places
                .is_empty()
        );
    }

    /// The other empty array, which must keep its own meaning: the **overlay**
    /// spells "I deleted my last place" that way (`remove_place` writes it and
    /// documents the round trip), and the shell still has to be somewhere.
    #[test]
    fn an_empty_array_from_the_overlay_still_reads_as_the_default() {
        // Over a base that supplies a list…
        let over_base = assemble_places(
            &[layer("base", BASE_TWO)],
            Some(&layer("overlay", "place = []\n")),
        );
        assert_eq!(
            over_base.places,
            builtin_default(),
            "an emptied overlay is #640's 'deleted the last place', not nix's 'no places'"
        );

        // …and over nothing at all.
        assert_eq!(
            assemble_places(&[], Some(&layer("overlay", "place = []\n"))).places,
            builtin_default()
        );
    }

    /// The case that decides whether `base_supplies_place` is about *who set
    /// it* or about *whose array won*: the base's empty array is locked, so the
    /// overlay's list is refused and the base's emptiness is what survives.
    #[test]
    fn a_locked_empty_base_array_beats_an_overlays_list() {
        let loaded = assemble_places(
            &[layer("base", "_locked = [\"place\"]\nplace = []\n")],
            Some(&layer(
                "overlay",
                "[[place]]\nname = \"Zuhause\"\nlat = 1.0\nlon = 2.0\n",
            )),
        );

        assert!(
            loaded.places.is_empty(),
            "the lock kept nix's empty array; falling back to the default here would \
             invent a list neither layer holds: {:?}",
            loaded.places
        );
        assert_eq!(loaded.lock_findings.len(), 1);
    }

    /// L1: two base directories at the `places` level — the NixOS +
    /// home-manager box, which is the configuration #1331's rule was rewritten
    /// *for*, and the only shape where `assemble_places`' `paths`/`tables`
    /// indices can be wrong without a single-base test noticing.
    #[test]
    fn two_base_layers_fold_by_precedence_and_only_bind_the_overlay() {
        // `with_layers` order is the spec's: most important first. Here
        // `/hm` beats `/etc` for the list, `/etc`'s locked endpoint binds
        // only the overlay, and both markers union into the set.
        let etc = layer(
            "etc",
            "_locked = [\"departures.endpoint\"]\n\
             [departures]\nendpoint = \"db\"\n\
             [[place]]\nname = \"FromEtc\"\nlat = 1.0\nlon = 2.0\n",
        );
        let hm = layer(
            "hm",
            "_locked = [\"place\"]\n[[place]]\nname = \"FromHm\"\nlat = 3.0\nlon = 4.0\n",
        );
        // Lowest precedence first, the order `base_layer_paths` hands back.
        let loaded = assemble_places(
            &[etc, hm],
            Some(&layer(
                "overlay",
                "[departures]\nendpoint = \"bvg\"\n\
                 [[place]]\nname = \"Mine\"\nlat = 5.0\nlon = 6.0\n",
            )),
        );

        assert_eq!(
            loaded
                .places
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["FromHm"],
            "the more important base wins the value; no lock arbitrates between bases"
        );
        assert_eq!(loaded.endpoint.as_deref(), Some("db"));
        assert_eq!(
            loaded.locked.iter().map(String::as_str).collect::<Vec<_>>(),
            [ENDPOINT_KEY, PLACE_KEY],
            "both bases' markers union"
        );
        assert_eq!(
            loaded
                .lock_findings
                .iter()
                .map(|f| (f.key.as_str(), f.kind))
                .collect::<Vec<_>>(),
            [
                (ENDPOINT_KEY, FindingKind::ShadowedLockedKey),
                (PLACE_KEY, FindingKind::ShadowedLockedKey),
            ],
            "one refusal per key, and only the overlay is ever refused"
        );
        for finding in &loaded.lock_findings {
            assert!(
                finding.message.contains("overlay.toml"),
                "the sentence names the file the operator must edit: {}",
                finding.message
            );
        }
    }

    /// The inversion #1331's fix round exists for, at this level: a marker in
    /// the *least* important base must not bind the *most* important one — the
    /// fold order is the search path reversed, so "above" there means "more
    /// important".
    #[test]
    fn a_low_base_layers_lock_does_not_bind_a_higher_base_layer() {
        let loaded = assemble_places(
            &[
                layer(
                    "etc",
                    "_locked = [\"place\"]\n[[place]]\nname = \"FromEtc\"\nlat = 1.0\nlon = 2.0\n",
                ),
                layer("hm", "[[place]]\nname = \"FromHm\"\nlat = 3.0\nlon = 4.0\n"),
            ],
            None,
        );

        assert_eq!(
            loaded
                .places
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            ["FromHm"],
            "base layers do not lock each other"
        );
        assert!(
            loaded.lock_findings.is_empty(),
            "and nothing is reported between them: {:?}",
            loaded.lock_findings
        );
        assert!(
            loaded.places_are_locked(),
            "the marker still binds the overlay, which is the only thing it ever bound"
        );
    }
}
