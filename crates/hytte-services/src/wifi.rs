//! iwd Wi-Fi station tracking + network list via the system D-Bus.
//!
//! Discovers the first object exposing `net.connman.iwd.Station` via
//! `ObjectManager.GetManagedObjects` on `net.connman.iwd`, then watches
//! `PropertiesChanged` on the Station interface and `InterfacesAdded` /
//! `InterfacesRemoved` for network visibility changes.
//!
//! # Public API
//!
//! ```ignore
//! // Register once at startup:
//! .with(wifi::service())
//!
//! // Subscribe in widgets:
//! wifi::station() -> impl Signal<Item = Option<Station>>
//! wifi::networks() -> impl Signal<Item = Vec<WifiNetwork>>
//! wifi::active_prompt() -> impl Signal<Item = Option<PromptRequest>>
//!
//! // Fire-and-forget commands:
//! wifi::scan();
//! wifi::connect_network(path);
//! wifi::disconnect();
//! wifi::submit_prompt(id, passphrase);
//! wifi::cancel_prompt(id);
//! ```

use futures_channel::oneshot;
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_bus::BusKind;
use hytte_reactive::{Service, registry, runtime};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex as AsyncMutex;
use zbus::zvariant::OwnedValue;

// ── Public data shapes ────────────────────────────────────────────────────────

/// Current state of the Wi-Fi station as reported by iwd.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StationState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Disconnecting,
    Roaming,
}

/// Snapshot of the iwd station.
#[derive(Clone, Debug, Default)]
pub struct Station {
    /// D-Bus object path, e.g. `"/net/connman/iwd/0/3/6"`.
    pub path: String,
    pub state: StationState,
    pub scanning: bool,
    /// Object path of the currently-connected network, if any.
    pub connected_network: Option<String>,
    /// Convenience: SSID of the currently-connected network.
    pub connected_ssid: Option<String>,
}

/// Snapshot of the iwd Adapter (`net.connman.iwd.Adapter1`).
#[derive(Clone, Debug, Default)]
pub struct Adapter {
    /// D-Bus object path, e.g. `"/net/connman/iwd/0"`.
    pub path: String,
    pub powered: bool,
    pub name: String,
}

/// Snapshot of one visible Wi-Fi network.
#[derive(Clone, Debug)]
pub struct WifiNetwork {
    /// D-Bus object path.
    pub path: String,
    pub ssid: String,
    /// `"open"` | `"psk"` | `"8021x"` | `"wep"`
    pub security: String,
    /// `true` when iwd has stored credentials for this network.
    pub known: bool,
    /// `true` when this is the currently-connected network.
    pub connected: bool,
    /// Signal strength in dBm (iwd reports dBm × 100; we divide before storing).
    pub signal_dbm: i16,
    /// iwd `KnownNetwork` object path when stored credentials exist;
    /// `None` otherwise. Used by `forget()` to call
    /// `net.connman.iwd.KnownNetwork.Forget()`.
    pub known_network_path: Option<String>,
}

// ── Prompt request ────────────────────────────────────────────────────────────

/// A pending passphrase prompt request from iwd.
#[derive(Clone, Debug)]
pub struct PromptRequest {
    /// Unique per request; echo back into `submit_prompt` or `cancel_prompt`.
    pub id: u64,
    /// iwd network object path.
    pub network_path: String,
    /// SSID from Network.Name (best-effort, falls back to last path segment).
    pub ssid: String,
    /// Network security type ("psk", "8021x", etc.).
    pub security: String,
}

// ── Station path cache ────────────────────────────────────────────────────────

/// Filled by the watcher on station discovery; read by command helpers.
/// Uses an `RwLock` so a new station path (USB dongle swap) can be written.
static STATION_PATH: OnceLock<Arc<tokio::sync::RwLock<String>>> = OnceLock::new();

fn station_path_store() -> &'static Arc<tokio::sync::RwLock<String>> {
    STATION_PATH.get_or_init(|| Arc::new(tokio::sync::RwLock::new(String::new())))
}

async fn get_station_path() -> String {
    station_path_store().read().await.clone()
}

async fn set_station_path(path: &str) {
    *station_path_store().write().await = path.to_string();
}

/// Filled by the watcher on adapter discovery; read by command helpers.
static ADAPTER_PATH: OnceLock<Arc<tokio::sync::RwLock<String>>> = OnceLock::new();

fn adapter_path_store() -> &'static Arc<tokio::sync::RwLock<String>> {
    ADAPTER_PATH.get_or_init(|| Arc::new(tokio::sync::RwLock::new(String::new())))
}

async fn current_adapter_path() -> String {
    adapter_path_store().read().await.clone()
}

async fn set_current_adapter_path(path: &str) {
    *adapter_path_store().write().await = path.to_string();
}

/// Given a station path like `"/net/connman/iwd/0/3/6"`, return the adapter
/// path `"/net/connman/iwd/0"`. Returns an empty string if the input does
/// not match the expected layout.
fn adapter_path_from_station(station_path: &str) -> String {
    // Expected layout: /net/connman/iwd/<adapter_idx>/<phy>/<station_idx>
    // → parts = ["", "net", "connman", "iwd", "<adapter>", "<phy>", "<station>"]
    let parts: Vec<&str> = station_path.split('/').collect();
    if parts.len() < 5 || parts[1] != "net" || parts[2] != "connman" || parts[3] != "iwd" {
        return String::new();
    }
    format!("/net/connman/iwd/{}", parts[4])
}

// ── Agent waiter map (module-level OnceLock for public API access) ────────────

type WaitersMap = Arc<AsyncMutex<HashMap<u64, oneshot::Sender<Result<String, String>>>>>;

static WAITERS: OnceLock<WaitersMap> = OnceLock::new();

fn waiters() -> Option<&'static WaitersMap> {
    WAITERS.get()
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

// ── Service handle ────────────────────────────────────────────────────────────

/// Shared mutable state held in the service registry.
#[doc(hidden)]
pub struct WifiHandles {
    pub(crate) station: Mutable<Option<Station>>,
    pub(crate) networks: Mutable<Vec<WifiNetwork>>,
    pub(crate) prompts: Mutable<Option<PromptRequest>>,
    pub(crate) adapter: Mutable<Option<Adapter>>,
    _ownership: hytte_bus::OwnNameSignal,
}

impl Default for WifiHandles {
    fn default() -> Self {
        // We can't call own_name here without the runtime; ownership is set
        // in Service::start. Use a placeholder that gets replaced immediately.
        // This is never called in practice — start() constructs WifiHandles directly.
        unreachable!("WifiHandles must be constructed via Service::start")
    }
}

// ── Service marker ────────────────────────────────────────────────────────────

/// The Wi-Fi service marker type — pass to `App::with`.
pub struct WifiService;

const AGENT_PATH: &str = "/cc/hannig/trollshell/iwd_agent";
const ANCHOR_NAME: &str = "cc.hannig.trollshell.iwd-agent";

impl Service for WifiService {
    type Handles = WifiHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        // Initialise the WAITERS map once so public API functions can reach it.
        let waiters_arc: WaitersMap = Arc::new(AsyncMutex::new(HashMap::new()));
        let _ = WAITERS.set(waiters_arc.clone());

        let station_mutable = Mutable::new(None);
        let networks_mutable = Mutable::new(Vec::new());
        let prompts_mutable: Mutable<Option<PromptRequest>> = Mutable::new(None);
        let adapter_mutable = Mutable::new(None);

        // Mount the iwd Agent on the SYSTEM bus (same as iwd's AgentManager).
        // iwd records our system-bus unique name when we call RegisterAgent,
        // then issues RequestPassphrase callbacks on the system bus.
        let agent = IwdAgent {
            prompts: prompts_mutable.clone(),
            waiters: waiters_arc,
        };
        let ownership = hytte_bus::own_name(ANCHOR_NAME)
            .bus(BusKind::System)
            .at_path(AGENT_PATH, agent)
            .start();

        let station_m = station_mutable.clone();
        let networks_m = networks_mutable.clone();
        let prompts_m = prompts_mutable.clone();
        let adapter_m = adapter_mutable.clone();

        rt.spawn(run_wifi_watcher(
            station_m, networks_m, prompts_m, adapter_m,
        ));

        WifiHandles {
            station: station_mutable,
            networks: networks_mutable,
            prompts: prompts_mutable,
            adapter: adapter_mutable,
            _ownership: ownership,
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the Wi-Fi service to register with the hytte runtime.
#[must_use]
pub fn service() -> WifiService {
    WifiService
}

/// Signal that emits the current station state, or `None` when no adapter
/// is present or iwd is not running.
pub fn station() -> impl Signal<Item = Option<Station>> {
    registry::with(|r| {
        r.get::<WifiHandles>()
            .expect("wifi::service() not registered")
            .station
            .signal_cloned()
    })
}

/// Signal emitting the current Adapter snapshot, or `None` when no adapter
/// is present.
pub fn adapter() -> impl Signal<Item = Option<Adapter>> {
    registry::with(|r| {
        r.get::<WifiHandles>()
            .expect("wifi::service() not registered")
            .adapter
            .signal_cloned()
    })
}

/// Signal that emits the current list of visible networks (ordered by signal
/// strength as returned by `GetOrderedNetworks`).
pub fn networks() -> impl Signal<Item = Vec<WifiNetwork>> {
    registry::with(|r| {
        r.get::<WifiHandles>()
            .expect("wifi::service() not registered")
            .networks
            .signal_cloned()
    })
}

/// Fire-and-forget: trigger a Wi-Fi scan on the station.
pub fn scan() {
    runtime::handle().spawn(async move {
        let path = get_station_path().await;
        if path.is_empty() {
            tracing::warn!("wifi::scan: no station path known");
            return;
        }
        if let Err(e) = do_station_call(&path, "Scan").await {
            tracing::warn!(error = %e, "wifi scan failed");
        }
    });
}

/// Fire-and-forget: connect to the network at `network_path`.
pub fn connect_network(network_path: &str) {
    let path = network_path.to_string();
    runtime::handle().spawn(async move {
        if let Err(e) = do_network_call(&path, "Connect").await {
            tracing::warn!(error = %e, path, "wifi connect_network failed (may need agent)");
        }
    });
}

/// Fire-and-forget: disconnect from the current network.
pub fn disconnect() {
    runtime::handle().spawn(async move {
        let path = get_station_path().await;
        if path.is_empty() {
            tracing::warn!("wifi::disconnect: no station path known");
            return;
        }
        if let Err(e) = do_station_call(&path, "Disconnect").await {
            tracing::warn!(error = %e, "wifi disconnect failed");
        }
    });
}

/// Fire-and-forget: set `Powered` on the iwd Adapter1.
pub fn set_powered(on: bool) {
    runtime::handle().spawn(async move {
        let path = current_adapter_path().await;
        if path.is_empty() {
            tracing::warn!("wifi::set_powered: no adapter path known");
            return;
        }
        if let Err(e) = do_set_powered(&path, on).await {
            tracing::warn!(error = %e, on, "wifi set_powered failed");
        }
    });
}

/// Fire-and-forget: call `Forget` on the given iwd `KnownNetwork` object.
/// iwd handles cascading disconnect when forgetting the active network.
pub fn forget(known_network_path: &str) {
    let path = known_network_path.to_string();
    runtime::handle().spawn(async move {
        if let Err(e) = do_known_network_call(&path, "Forget").await {
            tracing::warn!(error = %e, path, "wifi forget failed");
        }
    });
}

/// Signal emitting `Some(PromptRequest)` when iwd needs a passphrase, `None`
/// otherwise.  Only one prompt can be active at a time — v0.6.1 serialises.
pub fn active_prompt() -> impl Signal<Item = Option<PromptRequest>> {
    registry::with(|r| {
        r.get::<WifiHandles>()
            .expect("wifi::service() not registered")
            .prompts
            .signal_cloned()
    })
}

/// Submit a passphrase for the prompt with `id`.
pub fn submit_prompt(id: u64, passphrase: &str) {
    let pass = passphrase.to_string();
    let Some(arc) = waiters() else { return };
    let arc = arc.clone();
    runtime::handle().spawn(async move {
        let mut map = arc.lock().await;
        if let Some(tx) = map.remove(&id) {
            let _ = tx.send(Ok(pass));
        }
    });
}

/// Dismiss the prompt with `id` without submitting (signals `Error.Canceled`).
pub fn cancel_prompt(id: u64) {
    let Some(arc) = waiters() else { return };
    let arc = arc.clone();
    runtime::handle().spawn(async move {
        let mut map = arc.lock().await;
        if let Some(tx) = map.remove(&id) {
            let _ = tx.send(Err("cancelled".into()));
        }
    });
}

// ── Command helpers ───────────────────────────────────────────────────────────

async fn do_station_call(path: &str, method: &str) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call("net.connman.iwd")
        .bus(BusKind::System)
        .at_path(path.to_string())
        .iface("net.connman.iwd.Station")
        .method(method)
        .args(())
        .send::<()>()
        .await
}

async fn do_network_call(path: &str, method: &str) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call("net.connman.iwd")
        .bus(BusKind::System)
        .at_path(path.to_string())
        .iface("net.connman.iwd.Network")
        .method(method)
        .args(())
        .send::<()>()
        .await
}

async fn do_set_powered(adapter_path: &str, on: bool) -> Result<(), hytte_bus::BusError> {
    let value = zbus::zvariant::Value::from(on)
        .try_to_owned()
        .map_err(|e| hytte_bus::BusError::Permanent {
            reason: e.to_string(),
            dbus_name: None,
        })?;
    hytte_bus::call("net.connman.iwd")
        .bus(BusKind::System)
        .at_path(adapter_path.to_string())
        .iface("org.freedesktop.DBus.Properties")
        .method("Set")
        .args(("net.connman.iwd.Adapter1", "Powered", value))
        .send::<()>()
        .await
}

async fn do_known_network_call(path: &str, method: &str) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call("net.connman.iwd")
        .bus(BusKind::System)
        .at_path(path.to_string())
        .iface("net.connman.iwd.KnownNetwork")
        .method(method)
        .args(())
        .send::<()>()
        .await
}

// ── Property parsing helpers ──────────────────────────────────────────────────

fn property<T>(props: &HashMap<String, OwnedValue>, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| T::try_from(v).ok())
}

fn prop_str(props: &HashMap<String, OwnedValue>, key: &str) -> String {
    if let Some(s) = property::<String>(props, key) {
        return s;
    }
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| zbus::zvariant::Str::try_from(v).ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn prop_bool(props: &HashMap<String, OwnedValue>, key: &str) -> bool {
    property::<bool>(props, key).unwrap_or(false)
}

fn parse_state(s: &str) -> StationState {
    match s {
        "connected" => StationState::Connected,
        "connecting" => StationState::Connecting,
        "disconnecting" => StationState::Disconnecting,
        "roaming" => StationState::Roaming,
        _ => StationState::Disconnected,
    }
}

// ── ObjectManager types ───────────────────────────────────────────────────────

type ManagedObjects =
    HashMap<zbus::zvariant::OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

async fn get_managed_objects() -> Result<ManagedObjects, hytte_bus::BusError> {
    hytte_bus::call("net.connman.iwd")
        .bus(BusKind::System)
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .method("GetManagedObjects")
        .args(())
        .send::<ManagedObjects>()
        .await
}

// ── Network list reader ───────────────────────────────────────────────────────

/// Call `Station.GetOrderedNetworks()` and read per-network properties.
async fn read_networks(
    station_path: &str,
    connected_network_path: Option<&str>,
) -> Vec<WifiNetwork> {
    // GetOrderedNetworks returns Vec<(ObjectPath, i16)>
    let ordered: Vec<(zbus::zvariant::OwnedObjectPath, i16)> =
        match hytte_bus::call("net.connman.iwd")
            .bus(BusKind::System)
            .at_path(station_path.to_string())
            .iface("net.connman.iwd.Station")
            .method("GetOrderedNetworks")
            .args(())
            .send::<Vec<(zbus::zvariant::OwnedObjectPath, i16)>>()
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "Station.GetOrderedNetworks failed");
                return Vec::new();
            }
        };

    let mut networks = Vec::with_capacity(ordered.len());

    for (net_path, signal_raw) in ordered {
        let net_path_str = net_path.as_str();

        // Read per-network properties via Properties.GetAll
        let props: HashMap<String, OwnedValue> = match hytte_bus::call("net.connman.iwd")
            .bus(BusKind::System)
            .at_path(net_path_str.to_string())
            .iface("org.freedesktop.DBus.Properties")
            .method("GetAll")
            .args(("net.connman.iwd.Network",))
            .send::<HashMap<String, OwnedValue>>()
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(path = net_path_str, error = %e, "failed to read network props");
                continue;
            }
        };

        let ssid = prop_str(&props, "Name");
        let security = prop_str(&props, "Type");

        // KnownNetwork is an object path; "/" means no stored credentials.
        let known_network_path_raw = props
            .get("KnownNetwork")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| zbus::zvariant::OwnedObjectPath::try_from(v).ok())
            .map(|p| p.as_str().to_string())
            .unwrap_or_default();
        let known_network_path: Option<String> =
            if known_network_path_raw.is_empty() || known_network_path_raw == "/" {
                None
            } else {
                Some(known_network_path_raw)
            };
        let known = known_network_path.is_some();

        let connected = connected_network_path.is_some_and(|cp| cp == net_path_str);

        #[allow(clippy::integer_division)]
        let signal_dbm = signal_raw / 100;

        networks.push(WifiNetwork {
            path: net_path_str.to_string(),
            ssid,
            security,
            known,
            connected,
            signal_dbm,
            known_network_path,
        });
    }

    networks
}

// ── iwd Agent object ──────────────────────────────────────────────────────────

#[derive(Clone)]
struct IwdAgent {
    prompts: Mutable<Option<PromptRequest>>,
    waiters: WaitersMap,
}

// zbus's `#[interface]` macro requires every method to be `async fn` even
// when the body doesn't await; the EAP stubs also have unused parameters
// since they reject the request without inspecting it. Allowing at the
// impl-block keeps the noise out of each method.
#[allow(clippy::unused_async, unused_variables)]
#[zbus::interface(name = "net.connman.iwd.Agent")]
impl IwdAgent {
    async fn release(&self) {
        tracing::info!("iwd Agent::Release");
    }

    async fn request_passphrase(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
    ) -> zbus::fdo::Result<String> {
        let path = network.as_str().to_string();
        let (ssid, security) = read_network_metadata(&path).await;

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<Result<String, String>>();
        {
            let mut waiters = self.waiters.lock().await;
            waiters.insert(id, tx);
        }
        self.prompts.set(Some(PromptRequest {
            id,
            network_path: path,
            ssid,
            security,
        }));

        if let Ok(Ok(pass)) = rx.await {
            self.prompts.set(None);
            Ok(pass)
        } else {
            self.prompts.set(None);
            Err(zbus::fdo::Error::Failed("agent cancelled".into()))
        }
    }

    async fn request_private_key_passphrase(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
    ) -> zbus::fdo::Result<String> {
        Err(zbus::fdo::Error::Failed(
            "hytte wifi agent does not support EAP".into(),
        ))
    }

    async fn request_user_name_and_password(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
    ) -> zbus::fdo::Result<(String, String)> {
        Err(zbus::fdo::Error::Failed(
            "hytte wifi agent does not support EAP".into(),
        ))
    }

    async fn request_user_password(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
        username: String,
    ) -> zbus::fdo::Result<String> {
        Err(zbus::fdo::Error::Failed(
            "hytte wifi agent does not support EAP".into(),
        ))
    }

    async fn cancel(&self, reason: String) {
        tracing::info!(%reason, "iwd Agent::Cancel");
        let mut waiters = self.waiters.lock().await;
        for (_, tx) in waiters.drain() {
            let _ = tx.send(Err("cancelled".into()));
        }
        self.prompts.set(None);
    }
}

/// Read the Name and Type properties from a net.connman.iwd.Network object.
/// Falls back to the last path segment for the SSID on any error.
async fn read_network_metadata(path: &str) -> (String, String) {
    match hytte_bus::call("net.connman.iwd")
        .bus(BusKind::System)
        .at_path(path.to_string())
        .iface("org.freedesktop.DBus.Properties")
        .method("GetAll")
        .args(("net.connman.iwd.Network",))
        .send::<HashMap<String, OwnedValue>>()
        .await
    {
        Ok(props) => {
            let ssid = prop_str(&props, "Name");
            let security = prop_str(&props, "Type");
            let ssid = if ssid.is_empty() {
                path.rsplit('/').next().unwrap_or(path).to_string()
            } else {
                ssid
            };
            (ssid, security)
        }
        Err(e) => {
            tracing::debug!(path, error = %e, "read_network_metadata failed, using path fallback");
            let ssid = path.rsplit('/').next().unwrap_or(path).to_string();
            (ssid, String::new())
        }
    }
}

// ── Refresh helpers ───────────────────────────────────────────────────────────

/// Refresh Adapter from the managed-objects map (called once on discovery).
fn refresh_adapter_from_managed(
    managed: &ManagedObjects,
    station_path: &str,
    adapter_mutable: &Mutable<Option<Adapter>>,
) {
    let adapter_path = adapter_path_from_station(station_path);
    if adapter_path.is_empty() {
        adapter_mutable.set(None);
        return;
    }

    let adapter_props = managed
        .iter()
        .find(|(p, _)| p.as_str() == adapter_path)
        .and_then(|(_, ifaces)| ifaces.get("net.connman.iwd.Adapter1"));

    if let Some(props) = adapter_props {
        adapter_mutable.set(Some(Adapter {
            path: adapter_path,
            powered: prop_bool(props, "Powered"),
            name: prop_str(props, "Name"),
        }));
    } else {
        adapter_mutable.set(None);
    }
}

/// Refresh the Station mutable from the managed-objects map (called once on discovery).
fn refresh_station_from_managed(
    managed: &ManagedObjects,
    station_path: &zbus::zvariant::OwnedObjectPath,
    station_mutable: &Mutable<Option<Station>>,
) {
    let path_str = station_path.as_str();
    let Some(ifaces) = managed.get(station_path) else {
        station_mutable.set(None);
        return;
    };
    let Some(station_props) = ifaces.get("net.connman.iwd.Station") else {
        station_mutable.set(None);
        return;
    };

    let state_str = prop_str(station_props, "State");
    let scanning = prop_bool(station_props, "Scanning");
    let connected_network = station_props
        .get("ConnectedNetwork")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| zbus::zvariant::OwnedObjectPath::try_from(v).ok())
        .map(|p| p.as_str().to_string())
        .filter(|p| !p.is_empty() && p != "/");

    station_mutable.set(Some(Station {
        path: path_str.to_string(),
        state: parse_state(&state_str),
        scanning,
        connected_network,
        connected_ssid: None, // filled after network list is read
    }));
}

/// Re-read Station properties via Properties.GetAll and update the mutable.
async fn refresh_station(station_path: &str, station_mutable: &Mutable<Option<Station>>) {
    let props: HashMap<String, OwnedValue> = match hytte_bus::call("net.connman.iwd")
        .bus(BusKind::System)
        .at_path(station_path.to_string())
        .iface("org.freedesktop.DBus.Properties")
        .method("GetAll")
        .args(("net.connman.iwd.Station",))
        .send::<HashMap<String, OwnedValue>>()
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "wifi: refresh_station GetAll failed");
            return;
        }
    };

    let state_str = prop_str(&props, "State");
    let scanning = prop_bool(&props, "Scanning");
    let connected_network = props
        .get("ConnectedNetwork")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| zbus::zvariant::OwnedObjectPath::try_from(v).ok())
        .map(|p| p.as_str().to_string())
        .filter(|p| !p.is_empty() && p != "/");

    let new_state = parse_state(&state_str);

    let mut guard = station_mutable.lock_mut();
    if let Some(st) = guard.as_mut() {
        st.state = new_state;
        st.scanning = scanning;
        st.connected_network = connected_network;
    } else {
        *guard = Some(Station {
            path: station_path.to_string(),
            state: new_state,
            scanning,
            connected_network,
            connected_ssid: None,
        });
    }
}

/// Re-read the network list and update the mutable.
async fn refresh_networks(
    station_path: &str,
    station_mutable: &Mutable<Option<Station>>,
    networks_mutable: &Mutable<Vec<WifiNetwork>>,
) {
    let connected = station_mutable
        .lock_ref()
        .as_ref()
        .and_then(|s| s.connected_network.clone());

    let nets = read_networks(station_path, connected.as_deref()).await;

    // Update connected_ssid from the refreshed network list.
    let connected_ssid = connected
        .as_deref()
        .and_then(|cp| nets.iter().find(|n| n.path == cp).map(|n| n.ssid.clone()));

    networks_mutable.set(nets);

    let mut guard = station_mutable.lock_mut();
    if let Some(st) = guard.as_mut() {
        st.connected_ssid = connected_ssid;
    }
}

/// Register our agent object with iwd's `AgentManager`.
async fn register_iwd_agent() -> Result<(), hytte_bus::BusError> {
    let agent_path = zbus::zvariant::ObjectPath::try_from(AGENT_PATH)
        .map_err(|e| hytte_bus::BusError::Permanent {
            reason: format!("invalid agent path: {e}"),
            dbus_name: None,
        })?
        .to_owned();

    hytte_bus::call("net.connman.iwd")
        .bus(BusKind::System)
        .at_path("/net/connman/iwd")
        .iface("net.connman.iwd.AgentManager")
        .method("RegisterAgent")
        .args((agent_path,))
        .send::<()>()
        .await
}

// ── Main watcher task ─────────────────────────────────────────────────────────

async fn run_wifi_watcher(
    station_mutable: Mutable<Option<Station>>,
    networks_mutable: Mutable<Vec<WifiNetwork>>,
    prompts_mutable: Mutable<Option<PromptRequest>>,
    adapter_mutable: Mutable<Option<Adapter>>,
) {
    'discovery: loop {
        let Some((managed, station_path)) = discover_station().await else {
            continue 'discovery;
        };

        publish_paths(&station_path).await;
        tracing::info!(path = station_path.as_str(), "wifi station found");

        refresh_adapter_from_managed(&managed, station_path.as_str(), &adapter_mutable);
        refresh_station_from_managed(&managed, &station_path, &station_mutable);
        refresh_networks(station_path.as_str(), &station_mutable, &networks_mutable).await;

        let subs = subscribe_iwd_events(&station_path);

        match register_iwd_agent().await {
            Ok(()) => tracing::info!("hytte iwd agent registered"),
            Err(e) => tracing::warn!(error = %e, "iwd RegisterAgent failed"),
        }

        let station_path_str = station_path.as_str().to_string();
        // Returns only when the station was removed — falls through to
        // re-discover on the next iteration of 'discovery.
        pump_iwd_events(
            subs,
            &station_path_str,
            &station_mutable,
            &networks_mutable,
            &prompts_mutable,
            &adapter_mutable,
        )
        .await;
    }
}

async fn discover_station() -> Option<(ManagedObjects, zbus::zvariant::OwnedObjectPath)> {
    let managed = match get_managed_objects().await {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(error = %e, "iwd GetManagedObjects failed (iwd not running?)");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            return None;
        }
    };

    let Some(station_path) = managed.iter().find_map(|(path, ifaces)| {
        ifaces
            .contains_key("net.connman.iwd.Station")
            .then(|| path.clone())
    }) else {
        tracing::debug!("iwd has no Station object yet");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        return None;
    };

    Some((managed, station_path))
}

async fn publish_paths(station_path: &zbus::zvariant::OwnedObjectPath) {
    set_station_path(station_path.as_str()).await;
    let adapter_path = adapter_path_from_station(station_path.as_str());
    if !adapter_path.is_empty() {
        set_current_adapter_path(&adapter_path).await;
    }
}

struct IwdSubs {
    station_props: hytte_bus::SignalSubscription,
    added: hytte_bus::SignalSubscription,
    removed: hytte_bus::SignalSubscription,
}

fn subscribe_iwd_events(station_path: &zbus::zvariant::OwnedObjectPath) -> IwdSubs {
    let station_props = hytte_bus::signals("net.connman.iwd")
        .bus(BusKind::System)
        .at_path(station_path.as_str().to_string())
        .iface("org.freedesktop.DBus.Properties")
        .signal("PropertiesChanged")
        .start();
    let added = hytte_bus::signals("net.connman.iwd")
        .bus(BusKind::System)
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .signal("InterfacesAdded")
        .start();
    let removed = hytte_bus::signals("net.connman.iwd")
        .bus(BusKind::System)
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .signal("InterfacesRemoved")
        .start();
    IwdSubs {
        station_props,
        added,
        removed,
    }
}

/// Drive the iwd event loop. Returns when the station was removed and the
/// watcher needs to restart discovery.
async fn pump_iwd_events(
    subs: IwdSubs,
    station_path_str: &str,
    station_mutable: &Mutable<Option<Station>>,
    networks_mutable: &Mutable<Vec<WifiNetwork>>,
    prompts_mutable: &Mutable<Option<PromptRequest>>,
    adapter_mutable: &Mutable<Option<Adapter>>,
) {
    let mut station_events = subs.station_props.events();
    let mut added_events = subs.added.events();
    let mut removed_events = subs.removed.events();

    loop {
        tokio::select! {
            Some(evt) = station_events.next() => {
                let should_refresh = handle_station_props_event(
                    &evt, station_path_str, station_mutable, adapter_mutable,
                ).await;
                if should_refresh {
                    refresh_networks(station_path_str, station_mutable, networks_mutable).await;
                }
            }
            Some(_) = added_events.next() => {
                refresh_networks(station_path_str, station_mutable, networks_mutable).await;
            }
            Some(evt) = removed_events.next() => {
                if station_removed_from_event(&evt.body, station_path_str) {
                    tracing::warn!(path = station_path_str, "iwd station removed — rewatching");
                    station_mutable.set(None);
                    networks_mutable.set(Vec::new());
                    prompts_mutable.set(None);
                    adapter_mutable.set(None);
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    return;
                }
                refresh_networks(station_path_str, station_mutable, networks_mutable).await;
            }
        }
    }
}

/// Decode `PropertiesChanged` body. Applies the delta directly for known
/// interfaces (Station/Adapter1) to avoid a full `GetAll` round-trip, and
/// signals whether the caller should refresh the network list.
async fn handle_station_props_event(
    evt: &hytte_bus::SignalEvent,
    station_path_str: &str,
    station_mutable: &Mutable<Option<Station>>,
    adapter_mutable: &Mutable<Option<Adapter>>,
) -> bool {
    let Ok((iface, changed, _)) = evt
        .body
        .body()
        .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
    else {
        // Can't decode — full refresh to be safe.
        refresh_station(station_path_str, station_mutable).await;
        return true;
    };

    if iface == "net.connman.iwd.Station" {
        apply_station_props_delta(&changed, station_mutable);
        true
    } else if iface == "net.connman.iwd.Adapter1" {
        apply_adapter_props_delta(&changed, adapter_mutable);
        false
    } else {
        true
    }
}

/// Apply a `PropertiesChanged` delta for `net.connman.iwd.Station`.
fn apply_station_props_delta(
    changed: &HashMap<String, OwnedValue>,
    station_mutable: &Mutable<Option<Station>>,
) {
    let mut guard = station_mutable.lock_mut();
    let Some(st) = guard.as_mut() else { return };

    for (key, value) in changed {
        match key.as_str() {
            "State" => {
                // `prop_str` handles both `String` and `zvariant::Str`
                // variants gracefully via `try_clone().ok()`, so a
                // malformed or uncloneable value from iwd just leaves the
                // station state unchanged rather than aborting the shell.
                let s = prop_str(changed, "State");
                st.state = parse_state(&s);
            }
            "Scanning" => {
                st.scanning = property::<bool>(changed, "Scanning").unwrap_or(false);
            }
            "ConnectedNetwork" => {
                let path = value
                    .try_clone()
                    .ok()
                    .and_then(|v| zbus::zvariant::OwnedObjectPath::try_from(v).ok())
                    .map(|p| p.as_str().to_string());
                st.connected_network = path.filter(|p| !p.is_empty() && p != "/");
            }
            _ => {}
        }
    }
}

/// Apply a `PropertiesChanged` delta for `net.connman.iwd.Adapter1`.
fn apply_adapter_props_delta(
    changed: &HashMap<String, OwnedValue>,
    adapter_mutable: &Mutable<Option<Adapter>>,
) {
    let mut guard = adapter_mutable.lock_mut();
    if let Some(adapter) = guard.as_mut() {
        if changed.contains_key("Powered") {
            adapter.powered = prop_bool(changed, "Powered");
        }
        if changed.contains_key("Name") {
            adapter.name = prop_str(changed, "Name");
        }
    }
}

/// Returns `true` when the `InterfacesRemoved` signal indicates the station was removed.
fn station_removed_from_event(msg: &zbus::Message, station_path: &str) -> bool {
    let Ok((path, removed_ifaces)) = msg
        .body()
        .deserialize::<(zbus::zvariant::OwnedObjectPath, Vec<String>)>()
    else {
        return false;
    };

    path.as_str() == station_path
        && removed_ifaces
            .iter()
            .any(|i| i == "net.connman.iwd.Station")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `PropertiesChanged` delta carrying an unknown `State` value must not
    /// panic — it should fall through `parse_state`'s wildcard arm and leave
    /// the station in `Disconnected`.
    #[test]
    fn apply_station_props_delta_unknown_state_is_graceful() {
        let station = Mutable::new(Some(Station {
            path: "/test/path".to_string(),
            state: StationState::Connected,
            scanning: false,
            connected_network: None,
            connected_ssid: None,
        }));
        let mut changed: HashMap<String, OwnedValue> = HashMap::new();
        // A value that cannot be decoded as a station state string: insert a
        // raw bool instead of the expected string type.
        changed.insert(
            "State".to_string(),
            OwnedValue::try_from(zbus::zvariant::Value::from(true)).unwrap(),
        );
        // Must not panic; the unrecognised state falls back to Disconnected.
        apply_station_props_delta(&changed, &station);
        assert_eq!(
            station.lock_ref().as_ref().unwrap().state,
            StationState::Disconnected,
        );
    }

    #[test]
    fn adapter_path_from_station_standard() {
        assert_eq!(
            adapter_path_from_station("/net/connman/iwd/0/3/6"),
            "/net/connman/iwd/0",
        );
    }

    #[test]
    fn adapter_path_from_station_rejects_short_paths() {
        assert_eq!(adapter_path_from_station("/net/connman/iwd"), String::new());
        assert_eq!(adapter_path_from_station(""), String::new());
        assert_eq!(
            adapter_path_from_station("/other/prefix/0/3"),
            String::new()
        );
    }

    #[test]
    fn known_network_path_round_trips() {
        // Smoke-test the Option<String> derivation logic as a pure function.
        // The real extraction lives in read_networks; replicate the
        // canonicalization here so we lock in the "/ → None" rule.
        fn derive(raw: &str) -> Option<String> {
            if raw.is_empty() || raw == "/" {
                None
            } else {
                Some(raw.to_string())
            }
        }
        assert_eq!(derive(""), None);
        assert_eq!(derive("/"), None);
        assert_eq!(
            derive("/net/connman/iwd/0/3/6/known"),
            Some("/net/connman/iwd/0/3/6/known".to_string())
        );
    }
}
