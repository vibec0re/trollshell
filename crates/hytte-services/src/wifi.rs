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

use anyhow::{Context, Result, anyhow};
use futures_channel::oneshot;
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_reactive::{registry, runtime, Service};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::OnceCell;
use zbus::zvariant::OwnedValue;
use zbus::Connection;

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

/// Filled by the listen loop on station discovery; read by command helpers.
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

/// Filled by the listen loop on adapter discovery; read by command helpers.
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

/// Derive the adapter path from the station path, store it in the cache,
/// and publish an initial Adapter snapshot from the `GetManagedObjects` map.
async fn capture_initial_adapter(
    managed: &ManagedObjects,
    station_path: &str,
    adapter_mutable: &Mutable<Option<Adapter>>,
) {
    let adapter_path = adapter_path_from_station(station_path);
    if adapter_path.is_empty() {
        set_current_adapter_path("").await;
        adapter_mutable.set(None);
        return;
    }
    set_current_adapter_path(&adapter_path).await;

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

// ── Agent waiter map (module-level OnceLock for public API access) ────────────

type WaitersMap = Arc<AsyncMutex<HashMap<u64, oneshot::Sender<Result<String, String>>>>>;

static WAITERS: OnceLock<WaitersMap> = OnceLock::new();

fn waiters() -> Option<&'static WaitersMap> {
    WAITERS.get()
}

// ── Service handle ────────────────────────────────────────────────────────────

/// Shared mutable state held in the service registry.
#[doc(hidden)]
pub struct WifiHandles {
    pub(crate) station: Mutable<Option<Station>>,
    pub(crate) networks: Mutable<Vec<WifiNetwork>>,
    pub(crate) prompts: Mutable<Option<PromptRequest>>,
    pub(crate) adapter: Mutable<Option<Adapter>>,
}

impl Default for WifiHandles {
    fn default() -> Self {
        Self {
            station: Mutable::new(None),
            networks: Mutable::new(Vec::new()),
            prompts: Mutable::new(None),
            adapter: Mutable::new(None),
        }
    }
}

// ── Service marker ────────────────────────────────────────────────────────────

/// The Wi-Fi service marker type — pass to `App::with`.
pub struct WifiService;

impl Service for WifiService {
    type Handles = WifiHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = WifiHandles::default();
        let station_mutable = handles.station.clone();
        let networks_mutable = handles.networks.clone();
        let prompts_mutable = handles.prompts.clone();
        let adapter_mutable = handles.adapter.clone();

        rt.spawn(async move {
            loop {
                match listen(
                    &station_mutable,
                    &networks_mutable,
                    &prompts_mutable,
                    &adapter_mutable,
                )
                .await
                {
                    Ok(()) => {
                        tracing::warn!("wifi watcher closed, reconnecting in 2s");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "wifi watcher error, reconnecting in 2s");
                    }
                }
                station_mutable.set(None);
                networks_mutable.set(Vec::new());
                prompts_mutable.set(None);
                adapter_mutable.set(None);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });

        handles
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
///
/// For known networks this succeeds immediately. For unknown protected
/// networks iwd will return an error (no agent registered); that error is
/// silently logged — agent support is deferred to v0.6.1.
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
        if let Err(e) = do_set_adapter_bool(&path, "Powered", on).await {
            tracing::warn!(error = %e, on, "wifi set_powered failed");
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

/// Shared command-channel connection. Avoids opening a fresh system bus
/// connection on every iwd call. The listen loop keeps its own
/// connection because its long-lived signal subscriptions are
/// independent of command identity.
static CMD_CONN: OnceCell<Connection> = OnceCell::const_new();

async fn cmd_conn() -> Result<&'static Connection> {
    CMD_CONN
        .get_or_try_init(|| async {
            Connection::system()
                .await
                .context("open shared wifi command connection")
        })
        .await
}

async fn do_station_call(station_path: &str, method: &str) -> Result<()> {
    let conn = cmd_conn().await?;
    conn.call_method(
        Some("net.connman.iwd"),
        station_path,
        Some("net.connman.iwd.Station"),
        method,
        &(),
    )
    .await
    .with_context(|| format!("call Station.{method}"))?;
    Ok(())
}

async fn do_network_call(network_path: &str, method: &str) -> Result<()> {
    let conn = cmd_conn().await?;
    conn.call_method(
        Some("net.connman.iwd"),
        network_path,
        Some("net.connman.iwd.Network"),
        method,
        &(),
    )
    .await
    .with_context(|| format!("call Network.{method}"))?;
    Ok(())
}

async fn do_set_adapter_bool(adapter_path: &str, prop: &str, on: bool) -> Result<()> {
    let conn = cmd_conn().await?;
    conn.call_method(
        Some("net.connman.iwd"),
        adapter_path,
        Some("org.freedesktop.DBus.Properties"),
        "Set",
        &(
            "net.connman.iwd.Adapter1",
            prop,
            zbus::zvariant::Value::from(on),
        ),
    )
    .await
    .with_context(|| format!("call Properties.Set Adapter1.{prop}"))?;
    Ok(())
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

type ManagedObjects = HashMap<
    zbus::zvariant::OwnedObjectPath,
    HashMap<String, HashMap<String, OwnedValue>>,
>;

async fn get_managed_objects(conn: &Connection) -> Result<ManagedObjects> {
    let reply = conn
        .call_method(
            Some("net.connman.iwd"),
            "/",
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        )
        .await
        .context("GetManagedObjects on net.connman.iwd")?;
    let body = reply.body();
    body.deserialize()
        .context("deserialise GetManagedObjects reply")
}

// ── Network list reader ───────────────────────────────────────────────────────

/// Call `Station.GetOrderedNetworks()` and read per-network properties.
async fn read_networks(
    conn: &Connection,
    station_path: &str,
    connected_network_path: Option<&str>,
) -> Result<Vec<WifiNetwork>> {
    // GetOrderedNetworks returns Vec<(ObjectPath, i16)>
    let reply = conn
        .call_method(
            Some("net.connman.iwd"),
            station_path,
            Some("net.connman.iwd.Station"),
            "GetOrderedNetworks",
            &(),
        )
        .await
        .context("Station.GetOrderedNetworks")?;

    let ordered: Vec<(zbus::zvariant::OwnedObjectPath, i16)> = reply
        .body()
        .deserialize()
        .context("deserialise GetOrderedNetworks")?;

    let mut networks = Vec::with_capacity(ordered.len());

    for (net_path, signal_raw) in ordered {
        let net_path_str = net_path.as_str();

        // Read per-network properties via Properties.GetAll
        let props_reply = conn
            .call_method(
                Some("net.connman.iwd"),
                net_path_str,
                Some("org.freedesktop.DBus.Properties"),
                "GetAll",
                &("net.connman.iwd.Network",),
            )
            .await;

        let props: HashMap<String, OwnedValue> = match props_reply {
            Ok(r) => r.body().deserialize().unwrap_or_default(),
            Err(e) => {
                tracing::debug!(path = net_path_str, error = %e, "failed to read network props");
                continue;
            }
        };

        let ssid = prop_str(&props, "Name");
        let security = prop_str(&props, "Type");

        // KnownNetwork is an object path; "/" means no stored credentials.
        let known_network_path = props
            .get("KnownNetwork")
            .and_then(|v| v.try_clone().ok())
            .and_then(|v| zbus::zvariant::OwnedObjectPath::try_from(v).ok())
            .map(|p| p.as_str().to_string())
            .unwrap_or_default();
        let known = !known_network_path.is_empty() && known_network_path != "/";

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
        });
    }

    Ok(networks)
}

// ── Internal watcher state ────────────────────────────────────────────────────

#[derive(Clone)]
struct State {
    station: Mutable<Option<Station>>,
    networks: Mutable<Vec<WifiNetwork>>,
    prompts: Mutable<Option<PromptRequest>>,
    adapter: Mutable<Option<Adapter>>,
    waiters: WaitersMap,
    next_id: Arc<AtomicU64>,
    conn: Arc<Connection>,
    station_path: String,
}

impl State {
    #[allow(clippy::too_many_arguments)]
    fn new(
        station: Mutable<Option<Station>>,
        networks: Mutable<Vec<WifiNetwork>>,
        prompts: Mutable<Option<PromptRequest>>,
        adapter: Mutable<Option<Adapter>>,
        waiters: WaitersMap,
        next_id: Arc<AtomicU64>,
        conn: Arc<Connection>,
        station_path: String,
    ) -> Self {
        Self {
            station,
            networks,
            prompts,
            adapter,
            waiters,
            next_id,
            conn,
            station_path,
        }
    }

    /// Re-read the full networks list and publish it.
    async fn refresh_networks(&self) {
        let connected = self
            .station
            .lock_ref()
            .as_ref()
            .and_then(|s| s.connected_network.clone());

        match read_networks(&self.conn, &self.station_path, connected.as_deref()).await {
            Ok(nets) => self.networks.set(nets),
            Err(e) => {
                tracing::warn!(error = %e, "wifi: failed to refresh networks");
            }
        }
    }

    /// Apply a `PropertiesChanged` update for the Station interface.
    fn apply_station_props(&self, changed: &HashMap<String, OwnedValue>) {
        let mut guard = self.station.lock_mut();
        let Some(st) = guard.as_mut() else { return };

        for (key, value) in changed {
            match key.as_str() {
                "State" => {
                    if let Ok(s) = String::try_from(value.try_clone().unwrap_or_else(|_| {
                        OwnedValue::try_from(zbus::zvariant::Value::from("")).unwrap()
                    })) {
                        st.state = parse_state(&s);
                    } else if let Ok(s) = zbus::zvariant::Str::try_from(
                        value.try_clone().unwrap_or_else(|_| {
                            OwnedValue::try_from(zbus::zvariant::Value::from("")).unwrap()
                        }),
                    ) {
                        st.state = parse_state(s.as_str());
                    }
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
                    // "/" means no network
                    st.connected_network = path.filter(|p| !p.is_empty() && p != "/");
                }
                _ => {}
            }
        }
    }
}

// ── iwd Agent object ──────────────────────────────────────────────────────────

struct IwdAgent {
    state: State,
}

#[zbus::interface(name = "net.connman.iwd.Agent")]
impl IwdAgent {
    #[allow(clippy::unused_async)]
    async fn release(&self) {
        tracing::info!("iwd Agent::Release");
    }

    async fn request_passphrase(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
    ) -> zbus::fdo::Result<String> {
        let path = network.as_str().to_string();
        let (ssid, security) = read_network_metadata(&self.state.conn, &path).await;

        let id = self.state.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<Result<String, String>>();
        {
            let mut waiters = self.state.waiters.lock().await;
            waiters.insert(id, tx);
        }
        self.state.prompts.set(Some(PromptRequest {
            id,
            network_path: path,
            ssid,
            security,
        }));

        if let Ok(Ok(pass)) = rx.await {
            self.state.prompts.set(None);
            Ok(pass)
        } else {
            self.state.prompts.set(None);
            Err(zbus::fdo::Error::Failed("agent cancelled".into()))
        }
    }

    #[allow(clippy::unused_async)]
    #[allow(unused_variables)]
    async fn request_private_key_passphrase(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
    ) -> zbus::fdo::Result<String> {
        Err(zbus::fdo::Error::Failed(
            "hytte wifi agent does not support EAP".into(),
        ))
    }

    #[allow(clippy::unused_async)]
    #[allow(unused_variables)]
    async fn request_user_name_and_password(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
    ) -> zbus::fdo::Result<(String, String)> {
        Err(zbus::fdo::Error::Failed(
            "hytte wifi agent does not support EAP".into(),
        ))
    }

    #[allow(clippy::unused_async)]
    #[allow(unused_variables)]
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
        let mut waiters = self.state.waiters.lock().await;
        for (_, tx) in waiters.drain() {
            let _ = tx.send(Err("cancelled".into()));
        }
        self.state.prompts.set(None);
    }
}

/// Read the Name and Type properties from a net.connman.iwd.Network object.
/// Falls back to the last path segment for the SSID on any error.
async fn read_network_metadata(conn: &Connection, path: &str) -> (String, String) {
    let result = conn
        .call_method(
            Some("net.connman.iwd"),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &("net.connman.iwd.Network",),
        )
        .await;

    match result {
        Ok(reply) => {
            let props: HashMap<String, OwnedValue> =
                reply.body().deserialize().unwrap_or_default();
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

// ── Main listen loop ──────────────────────────────────────────────────────────

async fn listen(
    station_mutable: &Mutable<Option<Station>>,
    networks_mutable: &Mutable<Vec<WifiNetwork>>,
    prompts_mutable: &Mutable<Option<PromptRequest>>,
    adapter_mutable: &Mutable<Option<Adapter>>,
) -> Result<()> {
    let conn = Connection::system()
        .await
        .context("connect system bus for wifi")?;

    let managed = get_managed_objects(&conn).await?;

    // ── Find the first Station ────────────────────────────────────────────────

    let Some((station_obj_path, station_ifaces)) = managed
        .iter()
        .find(|(_, ifaces)| ifaces.contains_key("net.connman.iwd.Station"))
    else {
        return Err(anyhow::anyhow!("no net.connman.iwd.Station found"));
    };

    let station_path = station_obj_path.as_str().to_string();
    set_station_path(&station_path).await;

    tracing::info!(path = station_path, "wifi station found");

    // ── Find the parent Adapter ───────────────────────────────────────────────

    capture_initial_adapter(&managed, &station_path, adapter_mutable).await;

    // ── Parse initial Station state ───────────────────────────────────────────

    let station_props = station_ifaces
        .get("net.connman.iwd.Station")
        .expect("Station iface present — just checked");

    let state_str = prop_str(station_props, "State");
    let scanning = prop_bool(station_props, "Scanning");
    let connected_network = station_props
        .get("ConnectedNetwork")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| zbus::zvariant::OwnedObjectPath::try_from(v).ok())
        .map(|p| p.as_str().to_string())
        .filter(|p| !p.is_empty() && p != "/");

    let initial_station = Station {
        path: station_path.clone(),
        state: parse_state(&state_str),
        scanning,
        connected_network: connected_network.clone(),
        connected_ssid: None, // filled after network list read
    };
    station_mutable.set(Some(initial_station));

    // ── Set up shared agent state ─────────────────────────────────────────────

    let waiters_arc: WaitersMap = Arc::new(AsyncMutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicU64::new(1));
    // Publish into module-level OnceLock so public API fns can reach the map.
    let _ = WAITERS.set(waiters_arc.clone());

    let conn = Arc::new(conn);
    let state = State::new(
        station_mutable.clone(),
        networks_mutable.clone(),
        prompts_mutable.clone(),
        adapter_mutable.clone(),
        waiters_arc,
        next_id,
        conn.clone(),
        station_path.clone(),
    );

    // ── Initial network list ──────────────────────────────────────────────────

    state.refresh_networks().await;

    // Populate connected_ssid from the just-loaded network list.
    if let Some(cn_path) = connected_network {
        let ssid = networks_mutable
            .lock_ref()
            .iter()
            .find(|n| n.path == cn_path)
            .map(|n| n.ssid.clone());
        let mut guard = station_mutable.lock_mut();
        if let Some(st) = guard.as_mut() {
            st.connected_ssid = ssid;
        }
    }

    // ── Register iwd Agent ────────────────────────────────────────────────────

    let agent = IwdAgent {
        state: state.clone(),
    };
    conn.object_server()
        .at("/cc/hannig/trollshell/iwd_agent", agent)
        .await
        .context("register iwd agent object")?;

    let agent_path =
        zbus::zvariant::OwnedObjectPath::try_from("/cc/hannig/trollshell/iwd_agent")
            .map_err(|e| anyhow!("build agent path: {e}"))?;

    let agent_mgr = zbus::Proxy::new(
        conn.as_ref(),
        "net.connman.iwd",
        "/net/connman/iwd",
        "net.connman.iwd.AgentManager",
    )
    .await
    .context("build AgentManager proxy")?;

    agent_mgr
        .call::<_, _, ()>("RegisterAgent", &(agent_path,))
        .await
        .context("RegisterAgent")?;

    tracing::info!("hytte iwd agent registered");

    // ── Event loop ────────────────────────────────────────────────────────────

    event_loop(&conn, &state).await
}

async fn event_loop(conn: &Connection, state: &State) -> Result<()> {
    // ObjectManager proxy for InterfacesAdded / InterfacesRemoved.
    let obj_mgr_proxy = zbus::Proxy::new(
        conn,
        "net.connman.iwd",
        "/",
        "org.freedesktop.DBus.ObjectManager",
    )
    .await
    .context("create iwd ObjectManager proxy")?;

    let mut ifaces_added = obj_mgr_proxy
        .receive_signal("InterfacesAdded")
        .await
        .context("subscribe iwd InterfacesAdded")?;

    let mut ifaces_removed = obj_mgr_proxy
        .receive_signal("InterfacesRemoved")
        .await
        .context("subscribe iwd InterfacesRemoved")?;

    // PropertiesChanged covering all iwd objects under /net/connman/iwd.
    let props_rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("net.connman.iwd")
        .map_err(|e| anyhow::anyhow!("match rule sender: {e}"))?
        .interface("org.freedesktop.DBus.Properties")
        .map_err(|e| anyhow::anyhow!("match rule interface: {e}"))?
        .member("PropertiesChanged")
        .map_err(|e| anyhow::anyhow!("match rule member: {e}"))?
        .path_namespace("/net/connman/iwd")
        .map_err(|e| anyhow::anyhow!("match rule path: {e}"))?
        .build();

    let mut props_changed = zbus::MessageStream::for_match_rule(props_rule, conn, None)
        .await
        .context("subscribe iwd PropertiesChanged")?;

    loop {
        tokio::select! {
            msg = ifaces_added.next() => {
                let Some(_msg) = msg else { break; };
                // A new network object appeared — refresh the list.
                state.refresh_networks().await;
            }

            msg = ifaces_removed.next() => {
                let Some(msg) = msg else { break; };
                // Check if the station itself was removed.
                if handle_ifaces_removed(state, &msg) {
                    return Err(anyhow::anyhow!("iwd station removed"));
                }
                state.refresh_networks().await;
            }

            msg = props_changed.next() => {
                let Some(msg) = msg else { break; };
                let Ok(msg) = msg else { continue; };
                handle_props_changed(state, msg).await;
            }
        }
    }

    Ok(())
}

/// Returns `true` when the station was removed (caller should reconnect).
fn handle_ifaces_removed(state: &State, msg: &zbus::Message) -> bool {
    let Ok((path, removed_ifaces)) =
        msg.body()
            .deserialize::<(zbus::zvariant::OwnedObjectPath, Vec<String>)>()
    else {
        return false;
    };

    let p = path.as_str();
    if removed_ifaces
        .iter()
        .any(|i| i == "net.connman.iwd.Station")
        && p == state.station_path
    {
        tracing::warn!(path = p, "iwd station removed — reconnecting");
        return true;
    }

    false
}

async fn handle_props_changed(state: &State, msg: zbus::Message) {
    let Ok((iface_name, changed, _)) = msg.body().deserialize::<(
        String,
        HashMap<String, OwnedValue>,
        Vec<String>,
    )>() else {
        return;
    };

    let obj_path = msg
        .header()
        .path()
        .map_or("", |p: &zbus::zvariant::ObjectPath<'_>| p.as_str())
        .to_string();

    if iface_name == "net.connman.iwd.Station" && obj_path == state.station_path {
        state.apply_station_props(&changed);
        // Re-read networks so connected flags and connected_ssid stay current.
        state.refresh_networks().await;

        // Update connected_ssid from the refreshed network list.
        let connected_path = state
            .station
            .lock_ref()
            .as_ref()
            .and_then(|s| s.connected_network.clone());

        let ssid = connected_path.as_deref().and_then(|cp| {
            state
                .networks
                .lock_ref()
                .iter()
                .find(|n| n.path == cp)
                .map(|n| n.ssid.clone())
        });

        let mut guard = state.station.lock_mut();
        if let Some(st) = guard.as_mut() {
            st.connected_ssid = ssid;
        }
    } else if iface_name == "net.connman.iwd.Network" {
        // A network's properties changed — refresh the list.
        state.refresh_networks().await;
    } else if iface_name == "net.connman.iwd.Adapter1" {
        let mut guard = state.adapter.lock_mut();
        if let Some(adapter) = guard.as_mut() {
            if changed.contains_key("Powered") {
                adapter.powered = prop_bool(&changed, "Powered");
            }
            if changed.contains_key("Name") {
                adapter.name = prop_str(&changed, "Name");
            }
        }
    }
}
