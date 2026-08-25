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
//! write the whole set back to `places.toml` **atomically** (scratch file in
//! the same directory + `rename(2)`, so an interrupted save can never truncate
//! the config), and then publish the new set on the same handle the live
//! reload uses — so departures, weather and place detection pick the edit up
//! immediately, with no shell restart and no wait for the mtime poll. Nothing
//! on this path panics on bad input; every rejection is a [`PlacesError`].
//!
//! A save also **re-reads and reparses the file first** and refuses if it no
//! longer parses, or parses to something other than what the shell has in
//! memory. The set in memory is the user's config only when the file could be
//! read: a malformed one leaves memory on the built-in default indefinitely
//! (the load happens once, the watcher is mtime-gated), and rewriting that
//! would replace a hand-written config with one default place. See [`edit`].
//!
//! A save is a **format-preserving patch, not a re-render**: `places.toml` has
//! two permanent authors (the operator editing it by hand, which #703 asked to
//! keep, and the control-center editor writing it back), so a save touches only
//! the keys whose values actually moved. The documented preamble, per-key
//! comments inside a `[[place]]`, hand-chosen key ordering, keys the model
//! doesn't know about and unrelated tables all survive it byte for byte. See
//! [`hytte_config::places::render_places`].
//!
//! # Where the model lives
//!
//! Everything with no runtime attached — the schema, [`Place`],
//! [`PlacesError`], validation, and the writer itself — lives in the GTK-free
//! [`hytte_config::places`] leaf crate and is re-exported here, because
//! `trollshell-control-center` is the *other* editor and cannot link this crate
//! (it pulls `gtk`, `pipewire` and `hytte-ecal`). Two editors over one file have
//! to agree byte for byte on how it is validated and written, so they share one
//! copy rather than each keeping their own (#640). What stays here is the part
//! that needs a runtime: resolution, the service, the live-reload task, and
//! [`edit`]'s republish-under-lock.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_reactive::{Service, registry, shared, spawn_supervised};
use tokio::sync::Notify;

use crate::geoclue::{self, LocationSnapshot, LocationSource, LocationState};
use crate::hooks;
use crate::wifiscan::{self, AccessPoint};

/// The GTK-free half of this module — schema, validation, writer.
use hytte_config::places as model;
pub use hytte_config::places::{Place, PlacesError, ResolvedPlace};

// ── Config ────────────────────────────────────────────────────────────────--

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
        let loaded = model::load_places();
        model::warn_unsatisfiable_fingerprints(&loaded);
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
    edit(|current| model::added(current, place))
}

/// Replace the place named `target` — keeping its position in the file — with
/// `place`. `place.name` may differ from `target`, so this covers a rename
/// bundled with other edits.
pub fn update_place(target: &str, place: Place) -> Result<(), PlacesError> {
    edit(|current| model::updated(current, target, place))
}

/// Rename the place named `from` to `to`, leaving everything else alone.
pub fn rename_place(from: &str, to: &str) -> Result<(), PlacesError> {
    edit(|current| model::renamed(current, from, to))
}

/// Delete the place named `name`.
///
/// Deleting the last place is allowed, and round-trips to the built-in default
/// exactly as a hand-emptied file does: the file is written with an empty
/// `place = []`, which `load_places` reads back as the default — so that is
/// also what [`configured`] republishes, immediately rather than one config
/// poll later.
pub fn remove_place(name: &str) -> Result<(), PlacesError> {
    edit(|current| model::removed(current, name))
}

/// Replace the whole set in one write — the "the editor sent us its model
/// back" path, and the only one that can reorder places (which matters: the
/// first `[[place]]` is the provisional home before the first location fix).
pub fn save_places(places: Vec<Place>) -> Result<(), PlacesError> {
    edit(move |_| {
        let next: Vec<Place> = places.into_iter().map(model::normalize).collect();
        model::validate(&next)?;
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

/// The shell's write path: check the file still says what memory says, apply
/// `f` to the current set, persist the result atomically, then publish it.
///
/// The check and the write are [`hytte_config::places::check_base`] and
/// [`hytte_config::places::persist_to`] — the same two the control center's
/// editor composes through
/// [`hytte_config::places::save_to`]. This can't just call `save_to`, because
/// what makes the shell's version different is the third step: publishing on
/// the reactive handle *inside the same critical section*, so no other edit
/// can interleave between the write and the republish.
///
/// **The re-read is a data-loss guard, not a nicety** — see `check_base` for
/// the two ways the in-memory set can be a lie about what's on disk. It matters
/// especially here because the shell reads the file exactly once at startup and
/// its watcher is mtime-gated, so a malformed config leaves memory on the
/// built-in default indefinitely.
///
/// Order then matters. The file is written **first**, so a failed write leaves
/// the in-memory set untouched and the two still agree. Publishing second is
/// what makes an edit visible immediately: `resolve_loop` subscribes to this
/// very handle, so departures/weather/place-detection re-resolve on the spot
/// rather than waiting up to [`BATTERY_CONFIG_POLL_INTERVAL`] for the mtime
/// poll to notice our own write. That poll still runs, and finds nothing to do
/// — it content-compares the reparse against what we published, and
/// `normalize` guarantees they match.
fn edit(f: impl FnOnce(&[Place]) -> Result<Vec<Place>, PlacesError>) -> Result<(), PlacesError> {
    let shared = shared::get::<Shared>().ok_or(PlacesError::NotRunning)?;
    let handle = shared.configured.clone();
    let path = model::config_path().ok_or(PlacesError::NoConfigPath)?;
    let _serialized = EDIT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let current = handle.get_cloned();
    model::check_base(&path, &current)?;
    let next = f(current.as_slice())?;
    model::persist_to(&path, &next)?;
    model::warn_unsatisfiable_fingerprints(&next);
    tracing::info!(count = next.len(), "places: config saved");
    // An emptied config reads back as the built-in default (documented at
    // `remove_place`), so publish what a reload of what we just wrote would
    // give. Publishing the literal empty set instead would put memory and file
    // in disagreement until the next config poll — a visible empty-then-default
    // flicker, and a base the very next `edit` would reject as stale.
    handle.set(Arc::new(if next.is_empty() {
        model::builtin_default()
    } else {
        next
    }));
    Ok(())
}

/// Poll `places.toml` and republish the parsed list when it changes, so config
/// edits take effect within [`CONFIG_POLL_INTERVAL`] (or
/// [`BATTERY_CONFIG_POLL_INTERVAL`] on battery — #505) without restarting the
/// shell. [`resolve_loop`] subscribes to the same handle and re-resolves on
/// each swap, exactly as it does for the Wi-Fi and `GeoClue` sensors.
async fn watch_config(places: Mutable<Arc<Vec<Place>>>) {
    let mut watcher = model::ConfigWatcher::new();
    loop {
        wait_cadence().await;
        let current = places.get_cloned();
        if let Some(reloaded) = watcher.poll(&current) {
            model::warn_unsatisfiable_fingerprints(&reloaded);
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
    use std::path::Path;

    use super::*;
    use hytte_config::places::{DEFAULT_CONFIG, builtin_default, load_places, parse_places};

    fn ap(ssid: &str) -> AccessPoint {
        AccessPoint {
            ssid: ssid.to_string(),
            strength: 70,
        }
    }

    /// A place with *every* field non-default — the same fixture the model
    /// crate's write-path tests use, so a field silently dropped somewhere in
    /// the shell's editing API can't hide behind a coincidental default.
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

    /// A hand-written `places.toml` carrying every kind of formatting the
    /// editing API has to leave alone: a preamble, per-key comment blocks, a
    /// trailing comment beside a value, non-canonical key ordering, an
    /// unmodelled key, and an unrelated top-level table.
    /// `hytte_config::places` proves the *writer* preserves these; the test
    /// below proves they survive the shell's public API end to end.
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

    // ── Write path: the public API, end to end (#640) ───────────────────────

    /// The `shared` map is process-global and `reset_for_tests` clears *all* of
    /// it, so the cases that publish into (and clear) it are serialized —
    /// cargo runs tests in parallel threads of one process. This used to be a
    /// private `static SHARED_LOCK` local to this module, which raced
    /// `upower::tests` (a different module in this same crate, clearing the
    /// same process-global map through `registry::reset_for_tests()` with no
    /// lock at all) and produced the `NotRunning` flake CI caught on #775.
    /// `hytte_reactive::test_lock::TEST_LOCK` is the crate-spanning fix
    /// (#777): the one lock every test that touches this map takes, in this
    /// crate and its dependency `hytte-reactive` alike.
    ///
    /// Seed `$HOME/.config/trollshell/places.toml` with `seed`, publish the
    /// shared handles the editing API writes through — seeded exactly the way
    /// `PlacesService::start` seeds them, via `load_places`, so a config the
    /// loader can't read leaves memory on the built-in default just as it does
    /// in a running shell — and run `f` with the config path and that handle.
    ///
    /// Bytes rather than `&str` because two of the cases are files that aren't
    /// valid UTF-8.
    fn with_seeded_config(seed: &[u8], f: impl FnOnce(&Path, &Mutable<Arc<Vec<Place>>>)) {
        let _guard = hytte_reactive::test_lock::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".config/trollshell")).expect("mkdir");
        let cfg = dir.path().join(".config/trollshell/places.toml");
        std::fs::write(&cfg, seed).expect("seed");

        // `temp_env` serializes $HOME mutation across tests and restores it.
        temp_env::with_var("HOME", Some(dir.path().as_os_str()), || {
            let handle = Mutable::new(Arc::new(load_places()));
            shared::insert(Shared {
                place: Mutable::default(),
                location: Mutable::default(),
                configured: handle.clone(),
            });
            f(&cfg, &handle);
            hytte_reactive::shared::reset_for_tests();
        });
    }

    /// The requirement that makes the write path usable from a running shell:
    /// after an edit the published handle and the file on disk agree, so
    /// `resolve_loop` (and through it departures/weather) sees the new set
    /// immediately and the mtime watcher finds nothing to churn.
    #[test]
    fn editing_api_writes_the_file_and_republishes_the_set() {
        with_seeded_config(DEFAULT_CONFIG.as_bytes(), |cfg, handle| {
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
            let text = std::fs::read_to_string(cfg).expect("readable");
            assert!(text.starts_with("# trollshell places"));
        });
    }

    /// #640's settlement, end to end through the public API: a hand-annotated
    /// `places.toml` edited from the shell (as the control-center editor does)
    /// keeps every annotation its operator wrote. This is the assertion the
    /// live-verify repeats by hand.
    #[test]
    fn the_editing_api_keeps_a_hand_annotated_config_annotated() {
        with_seeded_config(HAND_WRITTEN.as_bytes(), |cfg, handle| {
            let before = handle.get_cloned();
            assert_eq!(before.len(), 2, "the fixture must load as itself");

            update_place(
                "Home",
                Place {
                    walk_minutes: 4,
                    ..before[0].clone()
                },
            )
            .expect("updates");
            add_place(Place::new("Gym", 52.49, 13.42)).expect("adds");
            rename_place("Office", "Werk").expect("renames");

            let text = std::fs::read_to_string(cfg).expect("readable");
            for kept in [
                "# my places file",
                "# station ids: https://v6.bvg.transport.rest/locations?query=",
                "# the pretty name, shown in the bar",
                "# picked off the neighbours",
                "station = \"900192001\" # S Schöneweide Bhf",
                "notes = \"the balcony one\"",
                "# work — no fingerprint captured yet",
                "[scratch]",
            ] {
                assert!(text.contains(kept), "{kept:?} must survive: {text}");
            }
            assert!(text.contains("walk_minutes = 4"));
            assert!(text.contains("name = \"Werk\""));
            assert_eq!(load_places(), *handle.get_cloned());
            assert_eq!(handle.get_cloned().len(), 3);
        });
    }

    /// Concurrent edits are read-modify-write against one file, so without
    /// serialization the loser's change is silently written away. Eight
    /// threads, eight distinct places, all eight must land.
    #[test]
    fn concurrent_edits_do_not_lose_each_other() {
        with_seeded_config(DEFAULT_CONFIG.as_bytes(), |_cfg, handle| {
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
        });
    }

    // ── Write path: never write over a config we can't account for (#640) ───
    //
    // `hytte_config::places` owns the guard itself (and its own tests); these
    // pin that the *public editing API* actually reaches it, through the shared
    // handle and the edit lock, on every one of its entry points.

    /// Four hand-configured places and a typo. `load_places` can't parse it, so
    /// it deliberately leaves the bytes alone and memory holds the *built-in
    /// default* — and nothing ever corrects that, because the load happens once
    /// and the watcher is mtime-gated. A read-memory-modify-write save would
    /// therefore render one default place over the four, atomically and with no
    /// backup. It must refuse instead.
    #[test]
    fn a_save_refuses_to_overwrite_a_config_that_no_longer_parses() {
        let broken = concat!(
            "# my places\n",
            "[[place]]\nname = \"Home\"\nlat = 52.5\nlon = 13.4\n",
            "[[place]]\nname = \"Office\"\nlat = 52.6\nlon = 13.5\n",
            "[[place]]\nname = \"Gym\"\nlat = 52.7\nlon = 13.6\n",
            "[[place]]\nname = \"Cabin\"\nlat = \nlon = 13.7\n",
        );
        with_seeded_config(broken.as_bytes(), |cfg, handle| {
            // The precondition that makes this dangerous.
            assert_eq!(
                *handle.get_cloned(),
                builtin_default(),
                "memory should be the built-in default, not the file's four places"
            );

            let err = add_place(Place::new("Office", 52.5, 13.4)).expect_err("must refuse");
            assert!(matches!(err, PlacesError::Unreadable(_)), "got {err:?}");
            // …including the path that ignores the current set entirely, which
            // is the one an editor's "save my model" button lands on.
            let err = save_places(vec![Place::new("Office", 52.5, 13.4)]).expect_err("must refuse");
            assert!(matches!(err, PlacesError::Unreadable(_)), "got {err:?}");
            assert!(matches!(
                remove_place("Schöneweide"),
                Err(PlacesError::Unreadable(_))
            ));

            assert_eq!(
                std::fs::read_to_string(cfg).expect("readable"),
                broken,
                "the user's four places must survive byte for byte"
            );
            assert_eq!(
                *handle.get_cloned(),
                builtin_default(),
                "a refused edit must not move memory either"
            );
        });
    }

    /// The other half of the guard, and the case `load_places`' comment calls
    /// out by name: a file we can't even decode (permissions, non-UTF-8). It
    /// doesn't overwrite it; neither may a save.
    #[test]
    fn a_save_refuses_to_overwrite_a_config_it_cannot_decode() {
        let bytes: &[u8] = &[
            0xff, 0xfe, b'[', b'[', b'p', b'l', b'a', b'c', b'e', b']', b']',
        ];
        with_seeded_config(bytes, |cfg, handle| {
            assert_eq!(*handle.get_cloned(), builtin_default());
            assert!(matches!(
                add_place(Place::new("Office", 52.5, 13.4)),
                Err(PlacesError::Unreadable(_))
            ));
            assert_eq!(std::fs::read(cfg).expect("readable"), bytes);
            assert_eq!(*handle.get_cloned(), builtin_default());
        });
    }

    /// An `$EDITOR` save landing between two config polls: the file parses, but
    /// to a different set than the edit was computed against. Applying it would
    /// write the hand edit away, so it is refused; once the watcher republishes
    /// (simulated here) the same edit lands.
    #[test]
    fn a_save_refuses_a_base_the_file_has_moved_on_from() {
        with_seeded_config(DEFAULT_CONFIG.as_bytes(), |cfg, handle| {
            let hand_edited = "[[place]]\nname = \"Cabin\"\nlat = 1.0\nlon = 2.0\n";
            std::fs::write(cfg, hand_edited).expect("out-of-process edit");

            assert_eq!(
                add_place(Place::new("Office", 52.5, 13.4)),
                Err(PlacesError::ChangedOnDisk)
            );
            assert_eq!(
                std::fs::read_to_string(cfg).expect("readable"),
                hand_edited,
                "the hand edit must survive"
            );

            // What `watch_config` does a tick later. The retry then lands, on
            // the hand-edited base rather than over it.
            handle.set(Arc::new(load_places()));
            add_place(Place::new("Office", 52.5, 13.4)).expect("retry lands");
            let set = handle.get_cloned();
            assert_eq!(set.len(), 2);
            assert_eq!(set[0].name, "Cabin");
            assert_eq!(load_places(), *set, "file and memory must agree again");
        });
    }

    /// The empty-set edge the `agree()` case above never reaches (it removes
    /// down to one place, never zero). On-disk semantics are unchanged — the
    /// file is emptied, and an emptied file has always read back as the
    /// built-in default — so that is what gets published, immediately, instead
    /// of an empty list the next config poll would have to correct.
    #[test]
    fn removing_the_last_place_publishes_the_default_the_file_reads_back_as() {
        with_seeded_config(DEFAULT_CONFIG.as_bytes(), |cfg, handle| {
            let only = handle.get_cloned()[0].name.clone();
            remove_place(&only).expect("removes");

            let text = std::fs::read_to_string(cfg).expect("readable");
            assert!(
                parse_places(&text).expect("still parses").is_empty(),
                "the file is emptied, not re-defaulted: {text}"
            );
            assert!(
                text.starts_with("# trollshell places"),
                "…and keeps its documented preamble"
            );
            assert_eq!(
                *handle.get_cloned(),
                builtin_default(),
                "memory must hold what a reload of that file gives"
            );
            assert_eq!(
                load_places(),
                *handle.get_cloned(),
                "file and memory must agree at zero places too"
            );

            // And the published set is a usable base again: the next edit isn't
            // rejected as stale, and builds on the default the file means.
            add_place(Place::new("Office", 52.5, 13.4)).expect("adds");
            let set = handle.get_cloned();
            assert_eq!(set.len(), 2);
            assert_eq!(load_places(), *set);
        });
    }

    #[test]
    fn editing_api_reports_a_missing_service_instead_of_panicking() {
        let _guard = hytte_reactive::test_lock::TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        hytte_reactive::shared::reset_for_tests();
        assert_eq!(
            add_place(Place::new("Home", 1.0, 2.0)),
            Err(PlacesError::NotRunning)
        );
    }
}
