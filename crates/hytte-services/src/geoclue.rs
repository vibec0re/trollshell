//! Location resolution for location-dependent widgets (weather, and future
//! sunrise/sunset, timezone-aware clock, …).
//!
//! Two sources, tried in order:
//!
//! 1. **`GeoClue2`** (`org.freedesktop.GeoClue2`, system bus). `GetClient` →
//!    set `DesktopId` + `RequestedAccuracyLevel` (City) → subscribe
//!    `LocationUpdated` → `Start`. We take the first location and stop; the
//!    whole attempt is bounded by [`GEOCLUE_TIMEOUT`].
//! 2. **Env-var fallback** `TROLLSHELL_WEATHER_CITY`. Forward-geocoded via
//!    Open-Meteo's geocoding endpoint. Used when `GeoClue2` is absent,
//!    denied, or times out.
//!
//! The [`LocationState`] (Resolving → Resolved/Unavailable) is published on a
//! `Mutable` exposed both via [`current`] (registry signal, for main-thread
//! widgets) and via [`shared_location`] (a process-global clone, so
//! `weather`'s tokio task can read it without touching the thread-local
//! registry).

use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_bus::{BusKind, call};
use hytte_reactive::{Service, registry};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Notify;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const GEOCLUE_NAME: &str = "org.freedesktop.GeoClue2";
const MANAGER_PATH: &str = "/org/freedesktop/GeoClue2/Manager";
const MANAGER_IFACE: &str = "org.freedesktop.GeoClue2.Manager";
const CLIENT_IFACE: &str = "org.freedesktop.GeoClue2.Client";
const LOCATION_IFACE: &str = "org.freedesktop.GeoClue2.Location";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";

/// Accuracy level 4 == "City" in the `GeoClue2` enum. Coarse is plenty for
/// a weather widget and avoids prompting for precise GPS.
const ACCURACY_CITY: u32 = 4;

/// How long the whole `GeoClue2` attempt may take before we fall back to the
/// env var. Covers `GetClient` + `Start` + waiting for the first
/// `LocationUpdated`.
const GEOCLUE_TIMEOUT: Duration = Duration::from_secs(10);

const GEOCODE_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const GEOCODE_READ_TIMEOUT: Duration = Duration::from_secs(12);

/// Where a [`LocationSnapshot`] came from. `Configured` already carries a
/// human name in `label_hint`; `GeoClue` does not, so `weather` reverse-
/// geocodes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocationSource {
    GeoClue,
    Configured,
}

/// A resolved location.
#[derive(Clone, Debug, PartialEq)]
pub struct LocationSnapshot {
    pub lat: f64,
    pub lon: f64,
    /// Friendly place name when the source already knows it (the env-var
    /// city, forward-geocoded). `None` for `GeoClue`, which only gives
    /// coordinates — `weather` reverse-geocodes those.
    pub label_hint: Option<String>,
    pub source: LocationSource,
}

/// Lifecycle of location resolution, published to `weather`. Starts
/// [`LocationState::Resolving`] (the first attempt is in flight); a successful
/// attempt yields [`LocationState::Resolved`]; an attempt that finds no source
/// (no `GeoClue2`, `TROLLSHELL_WEATHER_CITY` unset/empty) yields
/// [`LocationState::Unavailable`]. The split lets `weather` keep its loading
/// state at boot instead of flashing an error before the first fix lands.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum LocationState {
    #[default]
    Resolving,
    Resolved(LocationSnapshot),
    Unavailable,
}

/// Runtime place override the control-center (#391) sets over the `Control`
/// D-Bus interface. It lives here (shell-side runtime state) rather than in the
/// Nix/env config so the companion app can change the location **live**, no
/// rebuild — see [`set_manual_city`] / [`set_auto_location`].
///
/// `auto` mirrors the historical behaviour: `GeoClue2` first, the
/// `TROLLSHELL_WEATHER_CITY` env var as fallback. When `auto` is `false` and a
/// `manual_city` is set, that city is forward-geocoded (via the same Open-Meteo
/// path the env-var fallback uses — no reimplementation) and `GeoClue2` is
/// skipped entirely. Manual mode with no city yet degrades to auto so weather
/// never gets stuck.
///
/// **Session-only for v1:** the override is not persisted across shell
/// restarts (a restart reverts to `auto`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaceOverride {
    /// `true` = `GeoClue2` auto-location (the default); `false` = use
    /// `manual_city`.
    pub auto: bool,
    /// City to forward-geocode in manual mode. Preserved across an `auto`
    /// toggle so flipping back to manual restores the last city.
    pub manual_city: Option<String>,
}

impl Default for PlaceOverride {
    fn default() -> Self {
        Self {
            auto: true,
            manual_city: None,
        }
    }
}

#[doc(hidden)]
#[derive(Default)]
pub struct GeoclueHandles {
    pub(crate) location: Mutable<LocationState>,
    pub(crate) notify: Arc<Notify>,
}

// Cross-thread shared handle. `hytte_reactive::registry` is thread-local
// (main thread only); `weather`'s tokio task reads location from here
// instead. `Mutable` + `Arc<Notify>` are `Send + Sync`.
struct Shared {
    location: Mutable<LocationState>,
    notify: Arc<Notify>,
    /// The manual/auto place override (#391), set live over D-Bus.
    place_override: Mutable<PlaceOverride>,
}
static SHARED: OnceLock<Shared> = OnceLock::new();

pub struct GeoclueService;

impl Service for GeoclueService {
    type Handles = GeoclueHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = GeoclueHandles::default();
        let location = handles.location.clone();
        let notify = handles.notify.clone();
        let place_override = Mutable::new(PlaceOverride::default());
        let _ = SHARED.set(Shared {
            location: location.clone(),
            notify: notify.clone(),
            place_override: place_override.clone(),
        });
        rt.spawn(resolve_loop(location, notify, place_override));
        handles
    }
}

#[must_use]
pub fn service() -> GeoclueService {
    GeoclueService
}

/// Signal of the location lifecycle: [`LocationState::Resolving`] until the
/// first attempt settles, then [`LocationState::Resolved`] or
/// [`LocationState::Unavailable`].
pub fn current() -> impl Signal<Item = LocationState> {
    registry::with(|r| {
        r.get::<GeoclueHandles>()
            .expect("geoclue::service() not registered")
            .location
            .signal_cloned()
    })
}

/// Re-run resolution: cancel any cached result and try `GeoClue2` + the env
/// var again. Lets consumers recover from a transient failure.
pub fn refresh() {
    if let Some(s) = SHARED.get() {
        s.notify.notify_one();
    }
}

/// The current runtime place override (auto vs. manual city). Reads the
/// cross-thread shared handle, so it is callable off the GTK main thread (e.g.
/// from the `Control` D-Bus interface handlers). Returns the default (`auto`)
/// when [`service`] hasn't started.
#[must_use]
pub fn current_override() -> PlaceOverride {
    SHARED
        .get()
        .map(|s| s.place_override.get_cloned())
        .unwrap_or_default()
}

/// Switch to manual location: forward-geocode `city` and use it, ignoring
/// `GeoClue2`. Triggers an immediate re-resolve. Fire-and-forget — a city that
/// fails to geocode simply keeps the last good location (see [`resolve_loop`]).
pub fn set_manual_city(city: String) {
    if let Some(s) = SHARED.get() {
        s.place_override.set(PlaceOverride {
            auto: false,
            manual_city: Some(city),
        });
        s.notify.notify_one();
    }
}

/// Toggle auto (`GeoClue2`) vs. manual location. Keeps any previously-set
/// `manual_city` so flipping back to manual restores it. Triggers a re-resolve.
pub fn set_auto_location(auto: bool) {
    if let Some(s) = SHARED.get() {
        let ov = PlaceOverride {
            auto,
            ..s.place_override.get_cloned()
        };
        s.place_override.set(ov);
        s.notify.notify_one();
    }
}

/// Cross-thread accessor: a clone of the location `Mutable`, for tokio tasks
/// in sibling services that can't reach the thread-local registry. `None`
/// until [`service`] has started.
pub(crate) fn shared_location() -> Option<Mutable<LocationState>> {
    SHARED.get().map(|s| s.location.clone())
}

/// Resolve once at boot, then again on every [`refresh`]. We take a single
/// location per attempt (no live re-subscription) — matches the design's
/// "first `LocationUpdated` wins" rule.
async fn resolve_loop(
    location: Mutable<LocationState>,
    notify: Arc<Notify>,
    place_override: Mutable<PlaceOverride>,
) {
    loop {
        if let Some(loc) = resolve_once(&place_override.get_cloned()).await {
            location.set(LocationState::Resolved(loc));
        } else {
            tracing::info!(
                "geoclue: no location (GeoClue2 unavailable, TROLLSHELL_WEATHER_CITY unset?)"
            );
            // Don't clobber a previously-resolved fix on a transient re-resolve
            // failure; only surface Unavailable if we never had one (i.e.
            // genuinely no source at boot).
            if !matches!(location.get_cloned(), LocationState::Resolved(_)) {
                location.set(LocationState::Unavailable);
            }
        }
        notify.notified().await;
    }
}

async fn resolve_once(ov: &PlaceOverride) -> Option<LocationSnapshot> {
    // Manual override (#391): forward-geocode the chosen city and skip
    // GeoClue2 entirely. Manual-mode-without-a-city falls through to auto so
    // weather isn't stuck until the user supplies one.
    if !ov.auto
        && let Some(city) = ov.manual_city.clone()
    {
        return geocode(city).await;
    }
    if let Some(loc) = resolve_geoclue().await {
        return Some(loc);
    }
    tracing::debug!("geoclue: GeoClue2 yielded nothing (or timed out), trying env var");
    resolve_configured().await
}

/// The `GeoClue2` D-Bus dance. Untestable without a live daemon; the env-var
/// fallback in [`resolve_once`] covers any failure here.
///
/// Acquires a client, takes a single fix, and — on **every** exit path
/// (success, no-fix, or timeout) — releases the client via [`release_client`]
/// so geoclue's Wi-Fi relocation machinery doesn't keep running for the process
/// lifetime (#434). The fix-acquisition is bounded internally rather than by the
/// caller so the release still runs on timeout (an external timeout would cancel
/// us mid-`await` and leak the client).
async fn resolve_geoclue() -> Option<LocationSnapshot> {
    let client: OwnedObjectPath = call(GEOCLUE_NAME)
        .bus(BusKind::System)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .method("GetClient")
        .timeout(GEOCLUE_TIMEOUT)
        .send()
        .await
        .ok()?;
    let client_path = client.as_str().to_owned();

    let outcome = tokio::time::timeout(GEOCLUE_TIMEOUT, acquire_fix(&client_path))
        .await
        .unwrap_or(None);

    release_client(&client, &client_path).await;
    outcome
}

/// Configure the client, `Start` it, and take the first `LocationUpdated`. Split
/// out from [`resolve_geoclue`] so the caller can bound it with a timeout and
/// still release the client afterwards regardless of the outcome.
async fn acquire_fix(client_path: &str) -> Option<LocationSnapshot> {
    set_client_prop(client_path, "DesktopId", Value::from("trollshell"))
        .await
        .ok()?;
    set_client_prop(
        client_path,
        "RequestedAccuracyLevel",
        Value::U32(ACCURACY_CITY),
    )
    .await
    .ok()?;

    // Subscribe BEFORE Start so we don't miss the first LocationUpdated.
    let updates = hytte_bus::signals(GEOCLUE_NAME)
        .bus(BusKind::System)
        .at_path(client_path.to_owned())
        .iface(CLIENT_IFACE)
        .signal("LocationUpdated")
        .start();
    let mut events = updates.events();

    call(GEOCLUE_NAME)
        .bus(BusKind::System)
        .at_path(client_path.to_owned())
        .iface(CLIENT_IFACE)
        .method("Start")
        .send::<()>()
        .await
        .ok()?;

    let event = events.next().await?;
    // LocationUpdated(o old, o new); we want the new Location object path.
    let (_old, new): (OwnedObjectPath, OwnedObjectPath) = event.body.body().deserialize().ok()?;
    let loc_path = new.as_str().to_owned();

    let lat = get_f64_prop(&loc_path, "Latitude").await?;
    let lon = get_f64_prop(&loc_path, "Longitude").await?;

    // Deliberately ignore the location's `Description`: it's a source/method
    // blurb ("ipv4", "wifi", "IP fallback (from WiFi data)"), not a place
    // name. Leaving `label_hint` `None` lets `weather` reverse-geocode the
    // coordinates into a real city name (see `LocationSnapshot::label_hint`).
    Some(LocationSnapshot {
        lat,
        lon,
        label_hint: None,
        source: LocationSource::GeoClue,
    })
}

/// Best-effort release of a `GeoClue2` client (#434): `Stop` it — so geoclue
/// tears down the Wi-Fi-based relocation machinery it spun up — then
/// `DeleteClient` on the Manager so the client object is dropped rather than
/// lingering `Start`ed for the process lifetime (a later [`refresh`] then gets a
/// fresh, un-`Start`ed client instead of re-`Start`ing this one). Errors are
/// ignored: the client may already be gone, and a failed cleanup must never fail
/// resolution.
async fn release_client(client: &OwnedObjectPath, client_path: &str) {
    let _ = call(GEOCLUE_NAME)
        .bus(BusKind::System)
        .at_path(client_path.to_owned())
        .iface(CLIENT_IFACE)
        .method("Stop")
        .send::<()>()
        .await;
    let _ = call(GEOCLUE_NAME)
        .bus(BusKind::System)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .method("DeleteClient")
        .args((client.clone(),))
        .send::<()>()
        .await;
}

async fn set_client_prop(
    client_path: &str,
    name: &'static str,
    value: Value<'_>,
) -> Result<(), ()> {
    let owned = value.try_to_owned().map_err(|_| ())?;
    call(GEOCLUE_NAME)
        .bus(BusKind::System)
        .at_path(client_path.to_owned())
        .iface(PROPS_IFACE)
        .method("Set")
        .args((CLIENT_IFACE, name, owned))
        .send::<()>()
        .await
        .map_err(|e| tracing::debug!(prop = name, error = %e, "geoclue: set client prop failed"))
}

async fn get_f64_prop(path: &str, name: &'static str) -> Option<f64> {
    let v = get_prop(path, name).await?;
    f64::try_from(v).ok()
}

async fn get_prop(path: &str, name: &'static str) -> Option<OwnedValue> {
    call(GEOCLUE_NAME)
        .bus(BusKind::System)
        .at_path(path.to_owned())
        .iface(PROPS_IFACE)
        .method("Get")
        .args((LOCATION_IFACE, name))
        .send::<OwnedValue>()
        .await
        .ok()
}

/// Env-var fallback: `TROLLSHELL_WEATHER_CITY` forward-geocoded via
/// Open-Meteo. Returns `None` when the var is unset/empty or the lookup
/// fails.
async fn resolve_configured() -> Option<LocationSnapshot> {
    let city = std::env::var("TROLLSHELL_WEATHER_CITY")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    geocode(city).await
}

/// Forward-geocode a city name off-thread into a `Configured` snapshot, or
/// `None` on any failure. The single geocoding path shared by the env-var
/// fallback and the manual place override (#391).
async fn geocode(city: String) -> Option<LocationSnapshot> {
    match tokio::task::spawn_blocking(move || geocode_city(&city)).await {
        Ok(Ok(snap)) => Some(snap),
        Ok(Err(e)) => {
            tracing::warn!("geoclue: forward-geocoding city failed: {e}");
            None
        }
        Err(join) => {
            tracing::warn!("geoclue: geocode join failed: {join}");
            None
        }
    }
}

#[derive(serde::Deserialize)]
struct GeocodeResponse {
    #[serde(default)]
    results: Vec<GeocodeResult>,
}

#[derive(serde::Deserialize)]
struct GeocodeResult {
    name: String,
    latitude: f64,
    longitude: f64,
}

/// Blocking forward-geocode of a city name. Runs on a `spawn_blocking`
/// thread.
fn geocode_city(city: &str) -> Result<LocationSnapshot, String> {
    let agent = geocode_agent();
    let mut resp = agent
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
    parse_geocode(&body)
}

fn parse_geocode(body: &str) -> Result<LocationSnapshot, String> {
    let parsed: GeocodeResponse = serde_json::from_str(body).map_err(|e| format!("decode: {e}"))?;
    let first = parsed
        .results
        .into_iter()
        .next()
        .ok_or("no geocoding match")?;
    Ok(LocationSnapshot {
        lat: first.latitude,
        lon: first.longitude,
        label_hint: Some(first.name),
        source: LocationSource::Configured,
    })
}

fn geocode_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(GEOCODE_CONNECT_TIMEOUT))
        .timeout_global(Some(GEOCODE_READ_TIMEOUT))
        .build();
    config.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_geocode_takes_first_result() {
        let body = r#"{"results":[
            {"name":"Stockholm","latitude":59.33,"longitude":18.06,"country":"Sweden"},
            {"name":"Stockholm","latitude":39.6,"longitude":-75.4,"country":"USA"}
        ]}"#;
        let snap = parse_geocode(body).expect("parses");
        assert_eq!(snap.label_hint.as_deref(), Some("Stockholm"));
        assert!((snap.lat - 59.33).abs() < 1e-6);
        assert!((snap.lon - 18.06).abs() < 1e-6);
        assert_eq!(snap.source, LocationSource::Configured);
    }

    #[test]
    fn parse_geocode_empty_results_is_err() {
        assert!(parse_geocode(r#"{"results":[]}"#).is_err());
        // Missing `results` key defaults to empty → also an error, not a panic.
        assert!(parse_geocode(r#"{"generationtime_ms":0.1}"#).is_err());
    }

    #[test]
    fn parse_geocode_garbage_is_err() {
        assert!(parse_geocode("not json").is_err());
    }
}
