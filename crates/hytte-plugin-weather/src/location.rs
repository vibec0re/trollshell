//! Location resolution for the weather plugin — a port of the shell's
//! `hytte_services::geoclue`, but standalone.
//!
//! Two sources, tried in order (exactly the native path):
//!
//! 1. **`GeoClue2`** (`org.freedesktop.GeoClue2`, system bus). `GetClient` →
//!    set `DesktopId` + `RequestedAccuracyLevel` (City) → subscribe
//!    `LocationUpdated` → `Start`. First location wins; the whole attempt is
//!    bounded by [`GEOCLUE_TIMEOUT`].
//! 2. **Env-var fallback** `TROLLSHELL_WEATHER_CITY`, forward-geocoded via
//!    open-meteo. Used when `GeoClue2` is absent, denied, or times out — the
//!    no-geoclue path the Nix module documents, carried over unchanged.
//!
//! # Why `zbus` directly, not `hytte-bus`
//!
//! The shell's D-Bus layer (`hytte-bus`) is **not** usable from a plugin: every
//! primitive spawns onto `hytte_reactive::runtime::handle()`, and
//! `hytte-reactive` depends on `gtk4`. Depending on `hytte-bus` would therefore
//! link GTK into this out-of-process plugin and couple it to the shell's global
//! runtime — defeating the whole point of "frontend B". A plugin is an ordinary
//! separate process, so it opens its own GTK-free [`zbus`] connection directly.
//! The workspace bans raw `zbus::Connection::system` (a lint aimed at *shell*
//! code, to force connection pooling through `hytte-bus`); here it is the
//! correct and only option, so the single call site carries a scoped
//! `#[allow]`.

use std::time::Duration;

use tokio_stream::StreamExt as _;
use zbus::Connection;
use zbus::zvariant::OwnedObjectPath;

const GEOCLUE_NAME: &str = "org.freedesktop.GeoClue2";
const MANAGER_PATH: &str = "/org/freedesktop/GeoClue2/Manager";
const MANAGER_IFACE: &str = "org.freedesktop.GeoClue2.Manager";
const CLIENT_IFACE: &str = "org.freedesktop.GeoClue2.Client";
const LOCATION_IFACE: &str = "org.freedesktop.GeoClue2.Location";

/// Accuracy level 4 == "City" in the `GeoClue2` enum. Coarse is plenty for a
/// weather widget and avoids prompting for precise GPS.
const ACCURACY_CITY: u32 = 4;

/// How long the whole `GeoClue2` attempt may take before falling back to the
/// env var. Covers `GetClient` + `Start` + waiting for the first
/// `LocationUpdated`.
const GEOCLUE_TIMEOUT: Duration = Duration::from_secs(10);

const GEOCODE_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const GEOCODE_READ_TIMEOUT: Duration = Duration::from_secs(12);

/// Where a [`LocationSnapshot`] came from. `Configured` already carries a human
/// name in `label_hint`; `GeoClue` does not, so the fetcher reverse-geocodes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocationSource {
    GeoClue,
    Configured,
}

/// A resolved location.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LocationSnapshot {
    pub(crate) lat: f64,
    pub(crate) lon: f64,
    /// Friendly place name when the source already knows it (the env-var city).
    /// `None` for `GeoClue`, which gives only coordinates.
    pub(crate) label_hint: Option<String>,
    pub(crate) source: LocationSource,
}

/// Resolve a location once: try `GeoClue2` (bounded by [`GEOCLUE_TIMEOUT`]),
/// then the `TROLLSHELL_WEATHER_CITY` env-var fallback. `None` means no source
/// resolved at all.
pub(crate) async fn resolve_once() -> Option<LocationSnapshot> {
    match tokio::time::timeout(GEOCLUE_TIMEOUT, resolve_geoclue()).await {
        Ok(Some(loc)) => return Some(loc),
        Ok(None) => eprintln!("[weather] GeoClue2 gave no fix; trying $TROLLSHELL_WEATHER_CITY"),
        Err(_) => eprintln!("[weather] GeoClue2 timed out; trying $TROLLSHELL_WEATHER_CITY"),
    }
    resolve_configured().await
}

/// The `GeoClue2` D-Bus dance. Untestable without a live daemon; the env-var
/// fallback in [`resolve_once`] covers any failure here (every step is `?`).
async fn resolve_geoclue() -> Option<LocationSnapshot> {
    let conn = system_bus().await.ok()?;

    let manager = zbus::Proxy::new(&conn, GEOCLUE_NAME, MANAGER_PATH, MANAGER_IFACE)
        .await
        .ok()?;
    let client: OwnedObjectPath = manager.call("GetClient", &()).await.ok()?;

    let client_proxy = zbus::Proxy::new(&conn, GEOCLUE_NAME, client.as_str(), CLIENT_IFACE)
        .await
        .ok()?;
    client_proxy
        .set_property("DesktopId", "trollshell")
        .await
        .ok()?;
    client_proxy
        .set_property("RequestedAccuracyLevel", ACCURACY_CITY)
        .await
        .ok()?;

    // Subscribe BEFORE Start so the first LocationUpdated can't be missed.
    let mut updates = client_proxy.receive_signal("LocationUpdated").await.ok()?;
    client_proxy.call::<_, _, ()>("Start", &()).await.ok()?;

    // LocationUpdated(o old, o new); we want the new Location object path.
    let msg = updates.next().await?;
    let (_old, new): (OwnedObjectPath, OwnedObjectPath) = msg.body().deserialize().ok()?;

    let loc_proxy = zbus::Proxy::new(&conn, GEOCLUE_NAME, new.as_str(), LOCATION_IFACE)
        .await
        .ok()?;
    let lat: f64 = loc_proxy.get_property("Latitude").await.ok()?;
    let lon: f64 = loc_proxy.get_property("Longitude").await.ok()?;

    // Deliberately ignore the location's `Description` (a source blurb like
    // "ipv4"/"wifi", not a place name): leaving `label_hint` `None` lets the
    // fetcher reverse-geocode the coordinates into a real city name.
    Some(LocationSnapshot {
        lat,
        lon,
        label_hint: None,
        source: LocationSource::GeoClue,
    })
}

/// Open a private system-bus connection. See the module docs on why a plugin
/// opens its own `zbus` connection rather than going through `hytte-bus`.
#[allow(
    clippy::disallowed_methods,
    reason = "a plugin is a separate process; hytte-bus links the shell's gtk4 \
              runtime and is unusable here — the ban targets shell code"
)]
async fn system_bus() -> zbus::Result<Connection> {
    Connection::system().await
}

/// Env-var fallback: `TROLLSHELL_WEATHER_CITY` forward-geocoded via open-meteo.
/// `None` when the var is unset/empty or the lookup fails.
async fn resolve_configured() -> Option<LocationSnapshot> {
    let city = std::env::var("TROLLSHELL_WEATHER_CITY")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    match tokio::task::spawn_blocking(move || geocode_city(&city)).await {
        Ok(Ok(snap)) => Some(snap),
        Ok(Err(e)) => {
            eprintln!("[weather] geocoding $TROLLSHELL_WEATHER_CITY failed: {e}");
            None
        }
        Err(join) => {
            eprintln!("[weather] geocode task failed: {join}");
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

/// Blocking forward-geocode of a city name. Runs on a `spawn_blocking` thread.
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
    use super::{LocationSource, parse_geocode};

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
    fn parse_geocode_empty_or_missing_results_is_err() {
        assert!(parse_geocode(r#"{"results":[]}"#).is_err());
        assert!(parse_geocode(r#"{"generationtime_ms":0.1}"#).is_err());
    }

    #[test]
    fn parse_geocode_garbage_is_err() {
        assert!(parse_geocode("not json").is_err());
    }
}
