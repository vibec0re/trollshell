//! Wi-Fi station tracking + network list, backed by either iwd or
//! `NetworkManager` depending on which daemon is running on the host.
//!
//! The backend is probed at startup via [`crate::wifi_backend::probe_backend`].
//! Widgets are backend-agnostic — they only see the public types and signal
//! accessors exported from this module.
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

mod agent;
mod client;
mod nm_agent;
mod parse;
mod types;
mod watcher;

use futures_channel::oneshot;
use futures_signals::signal::{Mutable, Signal};
use hytte_bus::BusKind;
use hytte_reactive::{Service, registry, runtime};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, OnceLock};
use tokio::sync::{Mutex as AsyncMutex, RwLock};

use crate::wifi_backend::BackendChoice;

// ── Public re-exports ─────────────────────────────────────────────────────────

pub use types::{Adapter, PromptRequest, Station, StationState, WifiNetwork};

// ── Station path cache ────────────────────────────────────────────────────────

/// Filled by the watcher on station discovery; read by command helpers.
/// Uses an `RwLock` so a new station path (USB dongle swap) can be written.
static STATION_PATH: OnceLock<Arc<tokio::sync::RwLock<String>>> = OnceLock::new();

fn station_path_store() -> &'static Arc<tokio::sync::RwLock<String>> {
    STATION_PATH.get_or_init(|| Arc::new(tokio::sync::RwLock::new(String::new())))
}

pub(super) async fn get_station_path() -> String {
    station_path_store().read().await.clone()
}

pub(super) async fn set_station_path(path: &str) {
    *station_path_store().write().await = path.to_string();
}

/// Filled by the watcher on adapter discovery; read by command helpers.
static ADAPTER_PATH: OnceLock<Arc<tokio::sync::RwLock<String>>> = OnceLock::new();

fn adapter_path_store() -> &'static Arc<tokio::sync::RwLock<String>> {
    ADAPTER_PATH.get_or_init(|| Arc::new(tokio::sync::RwLock::new(String::new())))
}

pub(super) async fn current_adapter_path() -> String {
    adapter_path_store().read().await.clone()
}

pub(super) async fn set_current_adapter_path(path: &str) {
    *adapter_path_store().write().await = path.to_string();
}

// ── Agent waiter map (module-level OnceLock for public API access) ────────────

pub(super) type WaitersMap = Arc<AsyncMutex<HashMap<u64, oneshot::Sender<Result<String, String>>>>>;

static WAITERS: OnceLock<WaitersMap> = OnceLock::new();

fn waiters() -> Option<&'static WaitersMap> {
    WAITERS.get()
}

pub(super) static NEXT_ID: AtomicU64 = AtomicU64::new(1);

// ── Backend discriminant (stored in WifiHandles) ──────────────────────────────

/// Which daemon is backing the Wi-Fi service on this host.
///
/// Cloneable so command functions can read it from the registry (GTK thread)
/// and then use it inside a spawned tokio task.
#[derive(Clone)]
pub(crate) enum WifiBackend {
    /// iwd is managing the radio. Commands use iwd D-Bus paths.
    Iwd,
    /// `NetworkManager` is managing the radio.
    /// The inner `Arc<RwLock<String>>` stores the current NM Wi-Fi device path
    /// (e.g. `/org/freedesktop/NetworkManager/Devices/3`), written by the
    /// watcher task once the device is discovered and updated if it changes.
    NetworkManager(Arc<RwLock<String>>),
    /// Neither backend was detected; commands are no-ops.
    None,
}

// ── Service handle ────────────────────────────────────────────────────────────

/// Shared mutable state held in the service registry.
#[doc(hidden)]
pub struct WifiHandles {
    pub(crate) station: Mutable<Option<Station>>,
    pub(crate) networks: Mutable<Vec<WifiNetwork>>,
    pub(crate) prompts: Mutable<Option<PromptRequest>>,
    pub(crate) adapter: Mutable<Option<Adapter>>,
    /// iwd name-ownership signal; `None` when the NM backend is active.
    _ownership: Option<hytte_bus::OwnNameSignal>,
    /// NM secret-agent export handle; `None` unless the NM backend is active.
    /// Held only to keep the exported agent object (and its re-mount task) alive
    /// for the service's lifetime.
    _nm_agent: Option<hytte_bus::ExportHandle>,
    pub(crate) backend: WifiBackend,
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

pub(super) const AGENT_PATH: &str = "/cc/hannig/trollshell/iwd_agent";
const ANCHOR_NAME: &str = "cc.hannig.trollshell.iwd-agent";

impl Service for WifiService {
    type Handles = WifiHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        // Probe which backend is available on this host.
        let backend_choice = rt.block_on(crate::wifi_backend::probe_backend());
        tracing::info!(?backend_choice, "wifi: selected backend");

        // Initialise the WAITERS map once so public API functions can reach it.
        let waiters_arc: WaitersMap = Arc::new(AsyncMutex::new(HashMap::new()));
        let _ = WAITERS.set(waiters_arc.clone());

        let station_mutable = Mutable::new(None);
        let networks_mutable = Mutable::new(Vec::new());
        let prompts_mutable: Mutable<Option<PromptRequest>> = Mutable::new(None);
        let adapter_mutable = Mutable::new(None);

        let (ownership, nm_agent, backend) = match backend_choice {
            BackendChoice::Iwd => {
                // Mount the iwd Agent on the SYSTEM bus (same as iwd's AgentManager).
                // iwd records our system-bus unique name when we call RegisterAgent,
                // then issues RequestPassphrase callbacks on the system bus.
                let agent = agent::IwdAgent {
                    prompts: prompts_mutable.clone(),
                    waiters: waiters_arc,
                };
                let own = hytte_bus::own_name(ANCHOR_NAME)
                    .bus(BusKind::System)
                    .at_path(AGENT_PATH, agent)
                    .start();

                let station_m = station_mutable.clone();
                let networks_m = networks_mutable.clone();
                let prompts_m = prompts_mutable.clone();
                let adapter_m = adapter_mutable.clone();
                rt.spawn(watcher::run_wifi_watcher(
                    station_m, networks_m, prompts_m, adapter_m,
                ));

                (Some(own), None, WifiBackend::Iwd)
            }
            BackendChoice::NetworkManager => {
                let device_path_store: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
                let station_m = station_mutable.clone();
                let networks_m = networks_mutable.clone();
                let adapter_m = adapter_mutable.clone();
                let store = Arc::clone(&device_path_store);
                rt.spawn(crate::wifi_nm::run_nm_wifi_watcher(
                    station_m, networks_m, adapter_m, store,
                ));

                // Mount the NM SecretAgent on the SYSTEM bus and register it
                // with NM's AgentManager. Unlike the iwd agent, NM secret
                // agents do NOT own a well-known name — NM records our system
                // connection's unique name and calls GetSecrets back on it, so
                // we export the object name-lessly (no system-bus policy entry
                // needed). Export first, then register, so the object is
                // present before NM can call back.
                let nm_agent = nm_agent::NmAgent {
                    prompts: prompts_mutable.clone(),
                    waiters: waiters_arc,
                };
                let export = hytte_bus::export_object(crate::wifi_nm::NM_AGENT_PATH)
                    .bus(BusKind::System)
                    .start(nm_agent);
                rt.spawn(async {
                    // Give the export a moment to mount on the live connection
                    // before registering, so NM never calls back before the
                    // object exists.
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    match crate::wifi_nm::register_nm_agent().await {
                        Ok(()) => tracing::info!("wifi_nm: secret agent registered with NM"),
                        Err(e) => {
                            tracing::warn!(error = %e, "wifi_nm: secret agent registration failed");
                        }
                    }
                });

                (
                    None,
                    Some(export),
                    WifiBackend::NetworkManager(device_path_store),
                )
            }
            BackendChoice::None => {
                tracing::warn!("wifi: no Wi-Fi backend detected — service is inactive");
                (None, None, WifiBackend::None)
            }
        };

        WifiHandles {
            station: station_mutable,
            networks: networks_mutable,
            prompts: prompts_mutable,
            adapter: adapter_mutable,
            _ownership: ownership,
            _nm_agent: nm_agent,
            backend,
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

/// Read the active [`WifiBackend`] from the registry.
fn get_backend() -> WifiBackend {
    registry::with(|r| {
        r.get::<WifiHandles>()
            .expect("wifi::service() not registered")
            .backend
            .clone()
    })
}

/// Fire-and-forget: trigger a Wi-Fi scan on the station.
pub fn scan() {
    match get_backend() {
        WifiBackend::Iwd => {
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
        WifiBackend::NetworkManager(store) => {
            runtime::handle().spawn(async move {
                let path = store.read().await.clone();
                if path.is_empty() {
                    tracing::warn!("wifi::scan: NM device path not yet known");
                    return;
                }
                if let Err(e) = crate::wifi_nm::nm_scan(&path).await {
                    tracing::warn!(error = %e, "wifi scan (NM) failed");
                }
            });
        }
        WifiBackend::None => {
            tracing::warn!("wifi::scan: no backend available");
        }
    }
}

/// Fire-and-forget: connect to the network at `network_path`.
///
/// For the iwd backend, `network_path` is an iwd `Network` object path.
/// For the `NetworkManager` backend, `network_path` is an NM `AccessPoint` object path.
pub fn connect_network(network_path: &str) {
    let path = network_path.to_string();
    match get_backend() {
        WifiBackend::Iwd => {
            runtime::handle().spawn(async move {
                if let Err(e) = do_network_call(&path, "Connect").await {
                    tracing::warn!(error = %e, path, "wifi connect_network failed (may need agent)");
                }
            });
        }
        WifiBackend::NetworkManager(store) => {
            runtime::handle().spawn(async move {
                let device_path = store.read().await.clone();
                if device_path.is_empty() {
                    tracing::warn!("wifi::connect_network: NM device path not yet known");
                    return;
                }
                if let Err(e) = crate::wifi_nm::nm_connect(&device_path, &path).await {
                    tracing::warn!(error = %e, path, "wifi connect_network (NM) failed");
                }
            });
        }
        WifiBackend::None => {
            tracing::warn!("wifi::connect_network: no backend available");
        }
    }
}

/// Fire-and-forget: disconnect from the current network.
pub fn disconnect() {
    match get_backend() {
        WifiBackend::Iwd => {
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
        WifiBackend::NetworkManager(store) => {
            runtime::handle().spawn(async move {
                let path = store.read().await.clone();
                if path.is_empty() {
                    tracing::warn!("wifi::disconnect: NM device path not yet known");
                    return;
                }
                if let Err(e) = crate::wifi_nm::nm_disconnect(&path).await {
                    tracing::warn!(error = %e, "wifi disconnect (NM) failed");
                }
            });
        }
        WifiBackend::None => {
            tracing::warn!("wifi::disconnect: no backend available");
        }
    }
}

/// Fire-and-forget: set the radio powered state.
///
/// For iwd, sets `Powered` on the `Adapter1` object.
/// For `NetworkManager`, sets `WirelessEnabled` on the manager.
pub fn set_powered(on: bool) {
    match get_backend() {
        WifiBackend::Iwd => {
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
        WifiBackend::NetworkManager(_) => {
            runtime::handle().spawn(async move {
                if let Err(e) = crate::wifi_nm::nm_set_powered(on).await {
                    tracing::warn!(error = %e, on, "wifi set_powered (NM) failed");
                }
            });
        }
        WifiBackend::None => {
            tracing::warn!("wifi::set_powered: no backend available");
        }
    }
}

/// Fire-and-forget: forget the network with the given path.
///
/// For iwd, calls `Forget` on the `KnownNetwork` object at `known_network_path`.
/// For `NetworkManager`, calls `Settings.Connection.Delete` on the saved
/// connection profile at `known_network_path` (the path the watcher records in
/// [`WifiNetwork::known_network_path`] from the saved-connection enumeration).
pub fn forget(known_network_path: &str) {
    let path = known_network_path.to_string();
    match get_backend() {
        WifiBackend::Iwd => {
            runtime::handle().spawn(async move {
                if let Err(e) = do_known_network_call(&path, "Forget").await {
                    tracing::warn!(error = %e, path, "wifi forget failed");
                }
            });
        }
        WifiBackend::NetworkManager(_) => {
            runtime::handle().spawn(async move {
                if path.is_empty() {
                    tracing::warn!("wifi::forget: NM connection path is empty");
                    return;
                }
                if let Err(e) = crate::wifi_nm::nm_forget(&path).await {
                    tracing::warn!(error = %e, path, "wifi forget (NM) failed");
                }
            });
        }
        WifiBackend::None => {
            tracing::warn!("wifi::forget: no backend available");
        }
    }
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
