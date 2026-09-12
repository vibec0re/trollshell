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

use crate::networkd::Link;
use crate::retry;
use futures_signals::signal::{Mutable, Signal, SignalExt};
use futures_util::StreamExt;
use hytte_bus::{BusKind, call};
use hytte_reactive::{Service, registry, shared, spawn_supervised};
use std::future::Future;
use std::sync::Arc;
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

/// Retry ramp for a resolve attempt that came back with nothing (#1170).
///
/// **Unbounded**, for `networkd::STARTUP_REFRESH_RETRY`'s reason rather than a
/// new one: whatever made the boot-time attempt fail — `GeoClue2` not yet
/// activated, the host still offline, the geocoding endpoint unreachable — is
/// transient by nature, and until this existed a single failure at boot froze
/// weather, nightlight's auto sunset and places at "no location" for the whole
/// session, because [`refresh`] had no caller anywhere in the workspace. There
/// is no attempt count at which giving up would be the better answer.
///
/// The failure it cannot distinguish is a host with neither `GeoClue2` nor
/// `TROLLSHELL_WEATHER_CITY` — a genuinely sourceless machine, where this ramp
/// settles into one failed `GetClient` every 30 s forever. That is cheap (the
/// name is not on the bus, so the call fails without a round trip to anything)
/// and it is what makes a `GeoClue2` that is dbus-activated two minutes into
/// the session get picked up at all. It stays *quiet* rather than becoming a
/// 30 s log flood via [`retry::FailureLatch`] — see [`log_no_location`].
pub(crate) const RESOLVE_RETRY: retry::Policy = retry::Policy {
    max_attempts: None,
    initial: Duration::from_secs(1),
    max_backoff: Duration::from_secs(30),
};

/// How often the link-up watcher re-checks whether `networkd` has published its
/// cross-thread handle yet, and how many times before it stops asking.
///
/// Every service's `start` runs synchronously on the main thread while the
/// `App` is built, and this task is spawned from inside one of them, so the
/// window in which `networkd` has not published yet is sub-millisecond in
/// practice. The budget is three orders of magnitude wider than that, and its
/// expiry is the accurate answer for the other case: a shell that composes
/// `geoclue` without `networkd` at all, where no amount of waiting would help.
const LINK_PROBE_EVERY: Duration = Duration::from_millis(500);
const LINK_PROBE_ATTEMPTS: u32 = 20;

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

pub struct GeoclueService;

impl Service for GeoclueService {
    type Handles = GeoclueHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = GeoclueHandles::default();
        let location = handles.location.clone();
        let notify = handles.notify.clone();
        let place_override = Mutable::new(PlaceOverride::default());
        shared::insert(Shared {
            location: location.clone(),
            notify: notify.clone(),
            place_override: place_override.clone(),
        });
        spawn_supervised("geoclue", move || {
            resolve_loop(location.clone(), notify.clone(), place_override.clone())
        });
        // The one caller of `refresh()` in the tree (#1170). Its own task, so a
        // panic in the watcher cannot take the resolve loop with it and vice
        // versa; restart-safe because it owns nothing but a subscription.
        spawn_supervised("geoclue-link-watch", || {
            link_up_watcher(wait_for_link_up_shared)
        });
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
///
/// In-tree its caller is [`link_up_watcher`], which fires it on every link-up
/// edge; a shell author can also call it from a "refresh location" affordance.
/// It is *not* the only thing that re-resolves — [`run_resolve_loop`] also
/// retries a failed attempt on the [`RESOLVE_RETRY`] ramp all by itself, which
/// is what keeps a boot-time failure from being permanent on a host with no
/// `networkd` (#1170).
pub fn refresh() {
    if let Some(s) = shared::get::<Shared>() {
        s.notify.notify_one();
    }
}

/// The current runtime place override (auto vs. manual city). Reads the
/// cross-thread shared handle, so it is callable off the GTK main thread (e.g.
/// from the `Control` D-Bus interface handlers). Returns the default (`auto`)
/// when [`service`] hasn't started.
#[must_use]
pub fn current_override() -> PlaceOverride {
    shared::get::<Shared>()
        .map(|s| s.place_override.get_cloned())
        .unwrap_or_default()
}

/// Switch to manual location: forward-geocode `city` and use it, ignoring
/// `GeoClue2`. Triggers an immediate re-resolve. Fire-and-forget — a city that
/// fails to geocode simply keeps the last good location (see [`resolve_loop`]).
pub fn set_manual_city(city: String) {
    if let Some(s) = shared::get::<Shared>() {
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
    if let Some(s) = shared::get::<Shared>() {
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
    shared::get::<Shared>().map(|s| s.location.clone())
}

/// Resolve once at boot, then again on every [`refresh`], on every link-up edge
/// and — while the last attempt found nothing — on the [`RESOLVE_RETRY`] ramp.
/// We take a single location per attempt (no live re-subscription) — matches the
/// design's "first `LocationUpdated` wins" rule.
///
/// Until #1170 this parked on the `Notify` after the *first* attempt whatever it
/// returned, and nothing in the workspace called [`refresh`], so a boot-time
/// failure was permanent for the session.
async fn resolve_loop(
    location: Mutable<LocationState>,
    notify: Arc<Notify>,
    place_override: Mutable<PlaceOverride>,
) {
    run_resolve_loop(
        location,
        notify,
        place_override,
        |ov| async move { resolve_once(&ov).await },
        RESOLVE_RETRY,
    )
    .await;
}

/// [`resolve_loop`]'s body, with the resolver and the ramp injected.
///
/// Never returns. The seam exists so the retry and wake behaviour can be
/// asserted without `GeoClue2`, a network or a wall-clock wait: `resolve_loop`
/// supplies the real two, the tests supply a counting resolver and a
/// millisecond-scale ramp.
async fn run_resolve_loop<R, RFut>(
    location: Mutable<LocationState>,
    notify: Arc<Notify>,
    place_override: Mutable<PlaceOverride>,
    resolve: R,
    policy: retry::Policy,
) where
    R: Fn(PlaceOverride) -> RFut,
    RFut: Future<Output = Option<LocationSnapshot>>,
{
    let mut latch = retry::FailureLatch::new();
    // 1-based and counts the attempt that produced the outcome being weighed,
    // per `retry::Policy::step`. Reset by a success, so an outage that heals
    // and returns is priced from the bottom of the ramp.
    let mut attempt: u32 = 1;

    loop {
        let outcome = resolve(place_override.get_cloned()).await;
        let verdict = policy.step(&outcome.as_ref().ok_or(()), attempt);
        let report = latch.record(outcome.is_some());

        let retry_in = match (outcome, verdict) {
            (Some(loc), _) => {
                if report == retry::Report::Recovered {
                    tracing::info!(
                        failed_attempts = attempt.saturating_sub(1),
                        "geoclue: location resolved after earlier attempts found none"
                    );
                }
                location.set(LocationState::Resolved(loc));
                attempt = 1;
                None
            }
            (None, verdict) => {
                log_no_location(report, attempt, verdict);
                // Don't clobber a previously-resolved fix on a transient
                // re-resolve failure; only surface Unavailable if we never had
                // one (i.e. genuinely no source at boot).
                if !matches!(location.get_cloned(), LocationState::Resolved(_)) {
                    location.set(LocationState::Unavailable);
                }
                attempt = attempt.saturating_add(1);
                match verdict {
                    retry::Step::Retry { after } => Some(after),
                    // `Proceed` cannot follow an `Err`, and `GiveUp` cannot
                    // follow an unbounded policy; either way there is no timer
                    // to arm and the loop waits for an external wake.
                    retry::Step::Proceed | retry::Step::GiveUp => None,
                }
            }
        };

        // Whichever comes first: a `refresh()` — from the control-center, or
        // from `link_up_watcher` when the network returns — or, while the last
        // attempt found nothing, the retry delay.
        match retry_in {
            Some(after) => {
                tokio::select! {
                    () = notify.notified() => {}
                    () = tokio::time::sleep(after) => {}
                }
            }
            None => notify.notified().await,
        }
    }
}

/// What a resolve that found nothing is worth saying, given the ones before it.
///
/// The condition is usually **static** — no `GeoClue2` on this host, no
/// configured city — so the streak is latched the way `hytte_bus::own` latches a
/// contested bus name (#668): one `info!` naming both the cause and the fact
/// that it will not repeat, then `debug!` for as long as nothing changes.
fn log_no_location(report: retry::Report, attempt: u32, verdict: retry::Step) {
    let retry_in_secs = match verdict {
        retry::Step::Retry { after } => after.as_secs_f64(),
        retry::Step::Proceed | retry::Step::GiveUp => 0.0,
    };
    match report {
        retry::Report::Opened => tracing::info!(
            attempt,
            retry_in_secs,
            "geoclue: no location (GeoClue2 unavailable, TROLLSHELL_WEATHER_CITY unset?). \
             Retrying with backoff, on every refresh() and whenever the network link \
             returns; this line will not repeat until a location resolves"
        ),
        retry::Report::Repeating => {
            tracing::debug!(attempt, retry_in_secs, "geoclue: still no location");
        }
        // Unreachable: this is the no-location arm, so the latch cannot have
        // just recorded a success. Kept total rather than `unreachable!()` —
        // a wrong log level is a better failure than a panicked service task.
        retry::Report::Recovered | retry::Report::Quiet => {
            tracing::debug!(attempt, "geoclue: no location");
        }
    }
}

/// Call [`refresh`] on every offline → online transition of the host's primary
/// link, forever.
///
/// This is the caller [`refresh`] never had (#1170): a resolve that failed
/// because the host was offline should not sit out the ramp once the link
/// demonstrably came back. It is a *separate* supervised task rather than a
/// third arm of [`run_resolve_loop`]'s `select!` so the resolve loop keeps one
/// wake source and stays testable without `networkd`, and so the public
/// [`refresh`] is the one path an out-of-band re-resolve ever takes.
///
/// `next_edge` is injected for the same reason the resolver is: the tests drive
/// a local `Mutable<Option<Link>>` instead of the process-global bag.
async fn link_up_watcher<W, WFut>(next_edge: W)
where
    W: Fn() -> WFut,
    WFut: Future<Output = ()>,
{
    loop {
        next_edge().await;
        tracing::debug!("geoclue: primary link came back; re-resolving the location");
        refresh();
    }
}

/// Resolve once the host's primary link goes from absent to present, reading
/// `networkd`'s cross-thread handle.
///
/// Parks forever rather than returning if `networkd::service()` was never
/// registered — a future that resolved would turn the caller's `select!` into a
/// spin. See [`LINK_PROBE_ATTEMPTS`] for why the wait is bounded but the park
/// is not.
async fn wait_for_link_up_shared() {
    for _ in 0..LINK_PROBE_ATTEMPTS {
        if let Some(primary) = crate::networkd::shared_primary() {
            wait_for_link_up(&primary).await;
            return;
        }
        tokio::time::sleep(LINK_PROBE_EVERY).await;
    }
    tracing::debug!(
        "geoclue: networkd::service() is not registered, so there is no link-up edge to \
         re-resolve on; the retry ramp still runs"
    );
    std::future::pending::<()>().await;
}

/// Resolve on the next *edge* from "no primary link" to "a primary link".
///
/// Two waits, not one, and that is the whole content of this function: a
/// `futures_signals` signal replays its current value to a new subscriber, so
/// waiting only for `true` would fire immediately on a host that is already
/// online — turning every re-arm of this future into another resolve, i.e. a hot
/// loop. Waiting for `false` first makes "we are online" mean "nothing to
/// report yet", and only a link that actually goes away and comes back wakes the
/// caller.
async fn wait_for_link_up(primary: &Mutable<Option<Link>>) {
    let _ = primary.signal_ref(Option::is_some).wait_for(false).await;
    let _ = primary.signal_ref(Option::is_some).wait_for(true).await;
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
    let client: OwnedObjectPath = call(BusKind::System, GEOCLUE_NAME)
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
    let updates = hytte_bus::signals(BusKind::System, GEOCLUE_NAME)
        .at_path(client_path.to_owned())
        .iface(CLIENT_IFACE)
        .signal("LocationUpdated")
        .start();
    let mut events = updates.events();

    call(BusKind::System, GEOCLUE_NAME)
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
    let _ = call(BusKind::System, GEOCLUE_NAME)
        .at_path(client_path.to_owned())
        .iface(CLIENT_IFACE)
        .method("Stop")
        .send::<()>()
        .await;
    let _ = call(BusKind::System, GEOCLUE_NAME)
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
    call(BusKind::System, GEOCLUE_NAME)
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
    call(BusKind::System, GEOCLUE_NAME)
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

    // ── The resolve loop's retry and its link-up wake (#1170) ───────────────
    //
    // Everything below drives `run_resolve_loop` / `link_up_watcher` directly:
    // no `GeoClue2`, no bus, no network, and a millisecond-scale ramp so the
    // whole block costs a few milliseconds of wall clock.

    use hytte_reactive::test_lock::TEST_LOCK;
    use std::sync::PoisonError;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// The test ramp: the same *shape* as `RESOLVE_RETRY` (unbounded, doubling,
    /// capped) three orders of magnitude faster. Deliberately not the shipped
    /// constant — tuning that must not redden these, and `retry.rs`'s
    /// `every_shipped_policy_*` tests own the real numbers.
    const FAST_RETRY: retry::Policy = retry::Policy {
        max_attempts: None,
        initial: Duration::from_millis(1),
        max_backoff: Duration::from_millis(4),
    };

    fn a_fix() -> LocationSnapshot {
        LocationSnapshot {
            lat: 59.33,
            lon: 18.06,
            label_hint: Some("Stockholm".into()),
            source: LocationSource::Configured,
        }
    }

    /// A routable primary link, the shape `networkd::pick_primary` publishes.
    fn a_link() -> Link {
        Link {
            idx: 1,
            name: "wlan0".into(),
            ..Link::default()
        }
    }

    /// #1170's item 1, first half: the boot-time failure is no longer
    /// permanent. A resolver that finds nothing twice and then succeeds must
    /// leave the state `Resolved` on its own — no `refresh()`, no link-up edge,
    /// nothing external at all.
    ///
    /// Falsify by deleting the `Some(after)` arm of `run_resolve_loop`'s final
    /// `match` (park on the notify whatever happened, which is what shipped
    /// before #1170): the resolver is called exactly once and this reads
    /// `Resolving`.
    #[tokio::test]
    async fn a_resolver_that_fails_twice_then_succeeds_ends_resolved() {
        let location = Mutable::new(LocationState::default());
        let notify = Arc::new(Notify::new());
        let calls = Arc::new(AtomicU32::new(0));

        let resolve = {
            let calls = calls.clone();
            move |_ov| {
                let calls = calls.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    (n >= 2).then(a_fix)
                }
            }
        };

        // The loop never returns; bound it and read the state it left behind.
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            run_resolve_loop(
                location.clone(),
                notify,
                Mutable::new(PlaceOverride::default()),
                resolve,
                FAST_RETRY,
            ),
        )
        .await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "the loop must retry a failed resolve on its own ramp"
        );
        assert!(
            matches!(location.get_cloned(), LocationState::Resolved(_)),
            "three attempts, the third of which found a fix, and the state is still \
             {:?}",
            location.get_cloned()
        );
    }

    /// While it is still failing the state says `Unavailable` rather than
    /// staying on the boot-time `Resolving` — the split `LocationState` exists
    /// for, and the reason `weather` can show an error instead of a spinner
    /// forever.
    #[tokio::test]
    async fn a_failing_resolver_publishes_unavailable_while_it_retries() {
        let location = Mutable::new(LocationState::default());
        let calls = Arc::new(AtomicU32::new(0));
        let resolve = {
            let calls = calls.clone();
            move |_ov| {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    None
                }
            }
        };

        let _ = tokio::time::timeout(
            Duration::from_millis(50),
            run_resolve_loop(
                location.clone(),
                Arc::new(Notify::new()),
                Mutable::new(PlaceOverride::default()),
                resolve,
                FAST_RETRY,
            ),
        )
        .await;

        assert_eq!(location.get_cloned(), LocationState::Unavailable);
        assert!(
            calls.load(Ordering::SeqCst) > 1,
            "the ramp stopped after one attempt"
        );
    }

    /// #1170's item 1, second half: the offline → online edge triggers exactly
    /// **one** extra resolve, through the real public [`refresh`].
    ///
    /// The whole path is live here — `link_up_watcher` → `refresh()` →
    /// `shared::get::<Shared>()` → `Notify` → the resolve loop's wake — with
    /// only the edge source swapped for a local `Mutable` (production reads
    /// `networkd::shared_primary()`).
    ///
    /// "Exactly one" is the load-bearing half: `wait_for_link_up`'s first wait
    /// is what stops a signal's replay of its current value from re-firing the
    /// edge every time the watcher re-arms.
    ///
    /// **Falsifying this one needs care.** Deleting `wait_for_link_up`'s
    /// `wait_for(false)` line does not redden it — it makes the watcher spin,
    /// allocating a fresh signal subscription per turn, and the test binary
    /// climbs to tens of GB of RSS within a minute (measured while writing
    /// this). That runaway *is* the falsification; run it under a memory cap,
    /// or kill it on sight.
    ///
    /// Sync + `block_on` rather than `#[tokio::test]`: this one takes
    /// `TEST_LOCK` (it writes the process-global `shared` map), and a `std`
    /// guard must not be held across an `await`.
    #[test]
    fn the_offline_to_online_edge_triggers_exactly_one_extra_resolve() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        hytte_reactive::runtime::handle().block_on(async {
            let location = Mutable::new(LocationState::default());
            let notify = Arc::new(Notify::new());
            shared::insert(Shared {
                location: location.clone(),
                notify: notify.clone(),
                place_override: Mutable::new(PlaceOverride::default()),
            });

            let calls = Arc::new(AtomicU32::new(0));
            let resolve = {
                let calls = calls.clone();
                move |_ov| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Some(a_fix())
                    }
                }
            };

            // Offline at the start, so the edge is still ahead of us.
            let primary: Mutable<Option<Link>> = Mutable::new(None);
            let loop_task = tokio::spawn(run_resolve_loop(
                location,
                notify,
                Mutable::new(PlaceOverride::default()),
                resolve,
                FAST_RETRY,
            ));
            let watcher = tokio::spawn({
                let primary = primary.clone();
                async move { link_up_watcher(|| wait_for_link_up(&primary)).await }
            });

            // The boot resolve lands and the loop parks (a success arms no timer).
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(calls.load(Ordering::SeqCst), 1, "the boot resolve");

            primary.set(Some(a_link()));
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "the link came back and the location was not re-resolved"
            );

            // Still online: nothing further happens, however long we wait.
            tokio::time::sleep(Duration::from_millis(30)).await;
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "the link-up edge re-fired while the link never went down"
            );

            // …and it re-arms: down then up is a second edge, not a second no-op.
            primary.set(None);
            tokio::time::sleep(Duration::from_millis(10)).await;
            primary.set(Some(a_link()));
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(calls.load(Ordering::SeqCst), 3, "the edge did not re-arm");

            loop_task.abort();
            watcher.abort();
            hytte_reactive::shared::reset_for_tests();
        });
    }

    /// A link that is already up when the watcher starts is not an edge.
    /// Without this, a shell booting online would re-resolve immediately after
    /// the boot resolve, every time.
    #[tokio::test]
    async fn an_already_up_link_is_not_an_edge() {
        let primary: Mutable<Option<Link>> = Mutable::new(Some(a_link()));
        assert!(
            tokio::time::timeout(Duration::from_millis(30), wait_for_link_up(&primary))
                .await
                .is_err(),
            "a link that never went down reported a link-up edge"
        );
    }
}
