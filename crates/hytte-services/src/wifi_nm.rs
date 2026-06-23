//! `NetworkManager` Wi-Fi backend for the [`crate::wifi`] service.
//!
//! Populates the same [`crate::wifi::WifiHandles`] state that the iwd backend
//! fills — widgets are backend-agnostic. All D-Bus calls use
//! [`hytte_bus::call`] / [`hytte_bus::signals`] on the **system bus**.
//!
//! # Saved connections & forget
//!
//! NM has no per-network "known" flag; instead it stores connection *profiles*
//! under `Settings`. Each refresh tick enumerates them once
//! ([`nm_saved_connections`]) and matches by SSID, so visible networks report
//! `known: true` with their connection object path. `forget`
//! ([`nm_forget`]) deletes that profile via `Settings.Connection.Delete`.
//!
//! # Limitations (MVP)
//!
//! * `connect_network` uses `ActivateConnection` with `"/"` for the connection
//!   path (NM auto-selects the best stored connection). For secured networks
//!   without stored credentials, NM asks the registered secret agent (see
//!   [`register_nm_agent`] and the `wifi::nm_agent` module) for the passphrase
//!   via the same prompt overlay the iwd backend uses.

use futures_signals::signal::Mutable;
use futures_util::StreamExt;
use hytte_bus::BusKind;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::wifi::{Adapter, Station, StationState, WifiNetwork};

const NM_NAME: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_IFACE: &str = "org.freedesktop.NetworkManager";
const NM_DEVICE_IFACE: &str = "org.freedesktop.NetworkManager.Device";
const NM_WIRELESS_IFACE: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const NM_AP_IFACE: &str = "org.freedesktop.NetworkManager.AccessPoint";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const NM_AGENT_MANAGER_PATH: &str = "/org/freedesktop/NetworkManager/AgentManager";
const NM_AGENT_MANAGER_IFACE: &str = "org.freedesktop.NetworkManager.AgentManager";
const NM_SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const NM_SETTINGS_IFACE: &str = "org.freedesktop.NetworkManager.Settings";
const NM_SETTINGS_CONNECTION_IFACE: &str = "org.freedesktop.NetworkManager.Settings.Connection";

/// A connection settings dict: `a{sa{sv}}` — setting name → (key → value).
/// This is exactly the shape `Settings.Connection.GetSettings()` returns.
type ConnectionSettings = HashMap<String, HashMap<String, OwnedValue>>;

/// Stable identifier for our secret agent. NM keys registered agents by this
/// reverse-DNS string; reusing it across restarts lets NM replace a stale
/// registration cleanly.
pub(crate) const NM_AGENT_IDENTIFIER: &str = "cc.hannig.trollshell";

/// Standard object path NM secret agents export their interface at.
/// (NM itself does not require a specific path — it records our unique name —
///  but a stable, conventional path keeps introspection tidy.)
pub(crate) const NM_AGENT_PATH: &str = "/org/freedesktop/NetworkManager/SecretAgent";

// ── Pure conversion helpers ───────────────────────────────────────────────────

/// Decode an NM `Ssid` byte array to a UTF-8 string, lossily.
fn ssid_bytes_to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Extract the Wi-Fi SSID from a saved connection's settings dict
/// (`Settings.Connection.GetSettings() -> a{sa{sv}}`).
///
/// The SSID lives at `settings["802-11-wireless"]["ssid"]` as a byte array
/// (`ay`). Returns `None` for connections that aren't Wi-Fi (no
/// `802-11-wireless` setting), or whose SSID is missing/empty after trimming —
/// callers skip those, so they never end up keyed in the saved-connection map.
fn saved_connection_ssid(settings: &ConnectionSettings) -> Option<String> {
    let bytes = settings
        .get("802-11-wireless")?
        .get("ssid")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| <Vec<u8>>::try_from(v).ok())?;
    let ssid = ssid_bytes_to_string(&bytes).trim().to_string();
    if ssid.is_empty() { None } else { Some(ssid) }
}

/// Map an NM device state `u32` to a [`StationState`].
///
/// NM `NMDeviceState` values (from `libnm`):
/// * 100 (Activated) → [`StationState::Connected`]
/// * 110 (Deactivating) → [`StationState::Disconnecting`]
/// * 40–99 (Prepare / Config / `NeedAuth` / `IpConfig` / `IpCheck` / Secondaries) →
///   [`StationState::Connecting`]
/// * anything else (0–39, 120+) → [`StationState::Disconnected`]
fn nm_device_state_to_station_state(state: u32) -> StationState {
    match state {
        100 => StationState::Connected,
        110 => StationState::Disconnecting,
        40..=99 => StationState::Connecting,
        _ => StationState::Disconnected,
    }
}

/// Derive a security string from NM access-point flags.
///
/// * `rsn_flags != 0` → `"psk"` (RSN/WPA2, or `"8021x"` if bit 9 is set but
///   we simplify to `"psk"` for this increment)
/// * `wpa_flags != 0` → `"psk"` (WPA1)
/// * `flags & 1` → `"wep"` (privacy bit, no RSN/WPA → legacy WEP)
/// * otherwise → `"open"`
fn security_from_flags(flags: u32, wpa_flags: u32, rsn_flags: u32) -> String {
    if rsn_flags != 0 || wpa_flags != 0 {
        "psk".to_string()
    } else if (flags & 1) != 0 {
        "wep".to_string()
    } else {
        "open".to_string()
    }
}

/// Convert an NM signal-strength percentage (0–100) to an approximate dBm value.
///
/// Maps linearly: 0 % → −100 dBm, 100 % → −50 dBm.
fn strength_to_dbm(strength: u8) -> i16 {
    -100 + i16::from(strength) / 2
}

// ── Generic D-Bus property helper ─────────────────────────────────────────────

fn property<T>(props: &HashMap<String, OwnedValue>, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| T::try_from(v).ok())
}

fn prop_bytes(props: &HashMap<String, OwnedValue>, key: &str) -> Option<Vec<u8>> {
    let v = props.get(key)?.try_clone().ok()?;
    <Vec<u8>>::try_from(v).ok()
}

// ── NM D-Bus calls ────────────────────────────────────────────────────────────

async fn get_devices() -> Result<Vec<OwnedObjectPath>, hytte_bus::BusError> {
    hytte_bus::call(NM_NAME)
        .bus(BusKind::System)
        .at_path(NM_PATH)
        .iface(NM_IFACE)
        .method("GetDevices")
        .args(())
        .send::<Vec<OwnedObjectPath>>()
        .await
}

async fn get_device_props(
    device: &str,
) -> Result<HashMap<String, OwnedValue>, hytte_bus::BusError> {
    hytte_bus::call(NM_NAME)
        .bus(BusKind::System)
        .at_path(device.to_string())
        .iface(PROPS_IFACE)
        .method("GetAll")
        .args((NM_DEVICE_IFACE,))
        .send::<HashMap<String, OwnedValue>>()
        .await
}

async fn get_wireless_props(
    device: &str,
) -> Result<HashMap<String, OwnedValue>, hytte_bus::BusError> {
    hytte_bus::call(NM_NAME)
        .bus(BusKind::System)
        .at_path(device.to_string())
        .iface(PROPS_IFACE)
        .method("GetAll")
        .args((NM_WIRELESS_IFACE,))
        .send::<HashMap<String, OwnedValue>>()
        .await
}

async fn get_manager_props() -> Result<HashMap<String, OwnedValue>, hytte_bus::BusError> {
    hytte_bus::call(NM_NAME)
        .bus(BusKind::System)
        .at_path(NM_PATH)
        .iface(PROPS_IFACE)
        .method("GetAll")
        .args((NM_IFACE,))
        .send::<HashMap<String, OwnedValue>>()
        .await
}

async fn get_ap_props(ap: &str) -> Result<HashMap<String, OwnedValue>, hytte_bus::BusError> {
    hytte_bus::call(NM_NAME)
        .bus(BusKind::System)
        .at_path(ap.to_string())
        .iface(PROPS_IFACE)
        .method("GetAll")
        .args((NM_AP_IFACE,))
        .send::<HashMap<String, OwnedValue>>()
        .await
}

async fn get_all_access_points(device: &str) -> Result<Vec<OwnedObjectPath>, hytte_bus::BusError> {
    hytte_bus::call(NM_NAME)
        .bus(BusKind::System)
        .at_path(device.to_string())
        .iface(NM_WIRELESS_IFACE)
        .method("GetAllAccessPoints")
        .args(())
        .send::<Vec<OwnedObjectPath>>()
        .await
}

// ── Saved-connection enumeration ───────────────────────────────────────────────

/// List all saved connection profiles (`Settings.ListConnections() -> ao`).
async fn list_connections() -> Result<Vec<OwnedObjectPath>, hytte_bus::BusError> {
    hytte_bus::call(NM_NAME)
        .bus(BusKind::System)
        .at_path(NM_SETTINGS_PATH)
        .iface(NM_SETTINGS_IFACE)
        .method("ListConnections")
        .args(())
        .send::<Vec<OwnedObjectPath>>()
        .await
}

/// Read one saved connection's settings (`Settings.Connection.GetSettings() ->
/// a{sa{sv}}`).
async fn get_connection_settings(
    connection_path: &str,
) -> Result<ConnectionSettings, hytte_bus::BusError> {
    hytte_bus::call(NM_NAME)
        .bus(BusKind::System)
        .at_path(connection_path.to_string())
        .iface(NM_SETTINGS_CONNECTION_IFACE)
        .method("GetSettings")
        .args(())
        .send::<ConnectionSettings>()
        .await
}

/// Build a map **SSID → connection object path** over all saved Wi-Fi
/// connections.
///
/// Enumerates `Settings.ListConnections`, then `GetSettings` per profile,
/// keeping only `802-11-wireless` profiles with a non-empty SSID. Non-Wi-Fi
/// profiles (Ethernet, VPN, …) and SSID-less ones are skipped. A failure on a
/// single connection is logged and skipped so it can't abort the whole map.
///
/// If two saved profiles share an SSID (rare), the last one wins — `forget`
/// then removes that profile; any duplicate is left, which is acceptable for
/// this control. Failure of the top-level `ListConnections` yields an empty map
/// (every network reports `known: false`), which is the safe degraded state.
async fn nm_saved_connections() -> HashMap<String, String> {
    let connections = match list_connections().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "wifi_nm: Settings.ListConnections failed");
            return HashMap::new();
        }
    };

    let mut by_ssid = HashMap::new();
    for conn in connections {
        let conn_str = conn.as_str();
        match get_connection_settings(conn_str).await {
            Ok(settings) => {
                if let Some(ssid) = saved_connection_ssid(&settings) {
                    by_ssid.insert(ssid, conn_str.to_string());
                }
            }
            Err(e) => {
                tracing::debug!(path = conn_str, error = %e, "wifi_nm: GetSettings failed");
            }
        }
    }
    by_ssid
}

// ── Device discovery ──────────────────────────────────────────────────────────

/// Find the first Wi-Fi device path from NM. Returns `None` on failure.
async fn find_wifi_device() -> Option<String> {
    let devices = match get_devices().await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "wifi_nm: GetDevices failed");
            return None;
        }
    };

    for dev in devices {
        let dev_str = dev.as_str();
        let props = match get_device_props(dev_str).await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(path = dev_str, error = %e, "wifi_nm: device props read failed");
                continue;
            }
        };
        // DeviceType: 2 = Wi-Fi
        if property::<u32>(&props, "DeviceType") == Some(2) {
            tracing::info!(path = dev_str, "wifi_nm: found Wi-Fi device");
            return Some(dev_str.to_string());
        }
    }

    tracing::debug!("wifi_nm: no Wi-Fi device found");
    None
}

// ── State refresh ─────────────────────────────────────────────────────────────

/// Build a [`WifiNetwork`] from a single NM AP's properties.
///
/// `saved` maps SSID → saved-connection object path (see
/// [`nm_saved_connections`]); a hit marks the network `known` and records the
/// connection path so [`crate::wifi::forget`] can `Delete` it.
fn wifi_network_from_ap_props(
    ap_path: &str,
    props: &HashMap<String, OwnedValue>,
    active_ap_path: &str,
    saved: &HashMap<String, String>,
) -> Option<WifiNetwork> {
    let ssid_bytes = prop_bytes(props, "Ssid").unwrap_or_default();
    let ssid = ssid_bytes_to_string(&ssid_bytes);
    let ssid = ssid.trim().to_string();
    if ssid.is_empty() {
        return None;
    }
    let strength = property::<u8>(props, "Strength").unwrap_or(0);
    let wpa_flags = property::<u32>(props, "WpaFlags").unwrap_or(0);
    let rsn_flags = property::<u32>(props, "RsnFlags").unwrap_or(0);
    let flags = property::<u32>(props, "Flags").unwrap_or(0);
    let security = security_from_flags(flags, wpa_flags, rsn_flags);
    let connected = ap_path == active_ap_path;
    let signal_dbm = strength_to_dbm(strength);
    // A saved connection for this SSID makes it a "known" network; its object
    // path is what `forget()` deletes.
    let known_network_path = saved.get(&ssid).cloned();

    Some(WifiNetwork {
        path: ap_path.to_string(),
        ssid,
        security,
        known: known_network_path.is_some(),
        connected,
        signal_dbm,
        known_network_path,
    })
}

/// Read all APs for `device_path` and return the network list.
///
/// `saved` (SSID → connection path) is computed once per refresh tick and
/// passed in so we don't `GetSettings` per AP per second.
async fn read_nm_networks(
    device_path: &str,
    active_ap_path: &str,
    saved: &HashMap<String, String>,
) -> Vec<WifiNetwork> {
    let ap_paths = match get_all_access_points(device_path).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "wifi_nm: GetAllAccessPoints failed");
            return Vec::new();
        }
    };

    let mut networks = Vec::with_capacity(ap_paths.len());
    for ap in ap_paths {
        let ap_str = ap.as_str();
        match get_ap_props(ap_str).await {
            Ok(props) => {
                if let Some(net) = wifi_network_from_ap_props(ap_str, &props, active_ap_path, saved)
                {
                    networks.push(net);
                }
            }
            Err(e) => {
                tracing::debug!(path = ap_str, error = %e, "wifi_nm: AP props read failed");
            }
        }
    }

    // Sort by signal strength descending (strongest first).
    networks.sort_by_key(|n| Reverse(n.signal_dbm));
    networks
}

/// Snapshot the full device state and push it to the mutables.
async fn refresh_nm_state(
    device_path: &str,
    station: &Mutable<Option<Station>>,
    networks: &Mutable<Vec<WifiNetwork>>,
    adapter: &Mutable<Option<Adapter>>,
) {
    // --- adapter (powered = WirelessEnabled on the manager) ---
    let manager_props = match get_manager_props().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "wifi_nm: manager GetAll failed");
            return;
        }
    };
    let powered = property::<bool>(&manager_props, "WirelessEnabled").unwrap_or(false);
    adapter.set(Some(Adapter {
        path: device_path.to_string(),
        powered,
        name: "Wi-Fi".to_string(),
    }));

    // --- station state ---
    let device_props = match get_device_props(device_path).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "wifi_nm: device GetAll failed");
            return;
        }
    };
    let dev_state = property::<u32>(&device_props, "State").unwrap_or(0);
    let station_state = nm_device_state_to_station_state(dev_state);

    // --- active AP ---
    let wireless_props = match get_wireless_props(device_path).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "wifi_nm: wireless GetAll failed");
            return;
        }
    };
    let active_ap_path = wireless_props
        .get("ActiveAccessPoint")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| OwnedObjectPath::try_from(v).ok())
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let active_ap_path = if active_ap_path == "/" {
        String::new()
    } else {
        active_ap_path
    };

    // --- saved connections (once per tick) → mark known networks ---
    // Saved connections change rarely; computing this map once per refresh and
    // reusing it for every AP avoids a GetSettings storm (one per AP per second).
    let saved = nm_saved_connections().await;

    // --- network list ---
    let nets = read_nm_networks(device_path, &active_ap_path, &saved).await;

    let connected_ssid = if active_ap_path.is_empty() {
        None
    } else {
        nets.iter()
            .find(|n| n.path == active_ap_path)
            .map(|n| n.ssid.clone())
    };

    let connected_network = if active_ap_path.is_empty() {
        None
    } else {
        Some(active_ap_path)
    };

    station.set(Some(Station {
        path: device_path.to_string(),
        state: station_state,
        scanning: false,
        connected_network,
        connected_ssid,
    }));

    networks.set(nets);
}

// ── Main watcher task ─────────────────────────────────────────────────────────

/// Main NM watcher loop. Discovers the first Wi-Fi device, reads initial state,
/// then watches `PropertiesChanged` on the device and manager for updates.
///
/// The function runs forever (until the runtime is shut down).
pub(crate) async fn run_nm_wifi_watcher(
    station: Mutable<Option<Station>>,
    networks: Mutable<Vec<WifiNetwork>>,
    adapter: Mutable<Option<Adapter>>,
    device_path_store: Arc<RwLock<String>>,
) {
    loop {
        // Retry discovery every 5 s if NM isn't ready yet.
        let Some(device_path) = find_wifi_device().await else {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            continue;
        };

        // Publish device path for command helpers.
        *device_path_store.write().await = device_path.clone();

        // Initial full refresh.
        refresh_nm_state(&device_path, &station, &networks, &adapter).await;

        // Subscribe to PropertiesChanged on the device and on the manager.
        let device_sub = hytte_bus::signals(NM_NAME)
            .bus(BusKind::System)
            .at_path(device_path.clone())
            .iface(PROPS_IFACE)
            .signal("PropertiesChanged")
            .start();

        let manager_sub = hytte_bus::signals(NM_NAME)
            .bus(BusKind::System)
            .at_path(NM_PATH)
            .iface(PROPS_IFACE)
            .signal("PropertiesChanged")
            .start();

        // Also watch AccessPointAdded / AccessPointRemoved on the device.
        let ap_added_sub = hytte_bus::signals(NM_NAME)
            .bus(BusKind::System)
            .at_path(device_path.clone())
            .iface(NM_WIRELESS_IFACE)
            .signal("AccessPointAdded")
            .start();

        let ap_removed_sub = hytte_bus::signals(NM_NAME)
            .bus(BusKind::System)
            .at_path(device_path.clone())
            .iface(NM_WIRELESS_IFACE)
            .signal("AccessPointRemoved")
            .start();

        // Watch the manager's DeviceRemoved signal so we can re-discover when
        // the Wi-Fi device is unplugged (USB dongle) or otherwise unregistered
        // by NM.  The signal carries a single object-path argument.
        let device_removed_sub = hytte_bus::signals(NM_NAME)
            .bus(BusKind::System)
            .at_path(NM_PATH)
            .iface(NM_IFACE)
            .signal("DeviceRemoved")
            .start();

        let mut device_events = device_sub.events();
        let mut manager_events = manager_sub.events();
        let mut ap_added_events = ap_added_sub.events();
        let mut ap_removed_events = ap_removed_sub.events();
        let mut device_removed_events = device_removed_sub.events();

        tracing::info!(path = %device_path, "wifi_nm: watching device");

        loop {
            tokio::select! {
                Some(_) = device_events.next() => {
                    refresh_nm_state(&device_path, &station, &networks, &adapter).await;
                }
                Some(_) = manager_events.next() => {
                    refresh_nm_state(&device_path, &station, &networks, &adapter).await;
                }
                Some(_) = ap_added_events.next() => {
                    refresh_nm_state(&device_path, &station, &networks, &adapter).await;
                }
                Some(_) = ap_removed_events.next() => {
                    refresh_nm_state(&device_path, &station, &networks, &adapter).await;
                }
                Some(evt) = device_removed_events.next() => {
                    // The DeviceRemoved signal body is a single object path `o`.
                    // Accept any decode failure gracefully and always re-discover —
                    // re-discovery is cheap and idempotent.
                    let removed_path = evt
                        .body
                        .body()
                        .deserialize::<zbus::zvariant::OwnedObjectPath>()
                        .ok()
                        .map(|p| p.as_str().to_string())
                        .unwrap_or_default();
                    let matches = removed_path.is_empty() || removed_path == device_path;
                    if matches {
                        tracing::warn!(
                            path = %device_path,
                            "wifi_nm: device removed — clearing state and re-discovering"
                        );
                        station.set(None);
                        networks.set(Vec::new());
                        adapter.set(None);
                        *device_path_store.write().await = String::new();
                        break;
                    }
                }
            }
        }
    }
}

// ── Command helpers ───────────────────────────────────────────────────────────

/// Trigger a Wi-Fi scan on the NM device.
///
/// # Errors
///
/// Returns a [`hytte_bus::BusError`] if the D-Bus call fails (e.g. NM is rate-limiting scans).
pub(crate) async fn nm_scan(device_path: &str) -> Result<(), hytte_bus::BusError> {
    let options: HashMap<String, Value<'static>> = HashMap::new();
    hytte_bus::call(NM_NAME)
        .bus(BusKind::System)
        .at_path(device_path.to_string())
        .iface(NM_WIRELESS_IFACE)
        .method("RequestScan")
        .args((options,))
        .send::<()>()
        .await
}

/// Attempt to connect to an AP via `ActivateConnection` with auto-selected connection.
///
/// Uses `"/"` for the connection path so NM auto-selects the best stored profile.
/// This works for previously-connected networks; new WPA networks without stored
/// credentials will fail silently (passphrase agent is a follow-up).
///
/// # Errors
///
/// Returns a [`hytte_bus::BusError`] if the D-Bus call fails.
pub(crate) async fn nm_connect(
    device_path: &str,
    ap_path: &str,
) -> Result<(), hytte_bus::BusError> {
    let connection_path = zbus::zvariant::ObjectPath::try_from("/")
        .map_err(|e| hytte_bus::BusError::Permanent {
            reason: format!("invalid path: {e}"),
            dbus_name: None,
        })?
        .to_owned();
    let device_obj_path = zbus::zvariant::ObjectPath::try_from(device_path)
        .map_err(|e| hytte_bus::BusError::Permanent {
            reason: format!("invalid device path: {e}"),
            dbus_name: None,
        })?
        .to_owned();
    let ap_obj_path = zbus::zvariant::ObjectPath::try_from(ap_path)
        .map_err(|e| hytte_bus::BusError::Permanent {
            reason: format!("invalid AP path: {e}"),
            dbus_name: None,
        })?
        .to_owned();
    hytte_bus::call(NM_NAME)
        .bus(BusKind::System)
        .at_path(NM_PATH)
        .iface(NM_IFACE)
        .method("ActivateConnection")
        .args((connection_path, device_obj_path, ap_obj_path))
        .send::<OwnedObjectPath>()
        .await
        .map(|_| ())
}

/// Disconnect the NM device.
///
/// # Errors
///
/// Returns a [`hytte_bus::BusError`] if the D-Bus call fails.
pub(crate) async fn nm_disconnect(device_path: &str) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(NM_NAME)
        .bus(BusKind::System)
        .at_path(device_path.to_string())
        .iface(NM_DEVICE_IFACE)
        .method("Disconnect")
        .args(())
        .send::<()>()
        .await
}

/// Forget (delete) a saved connection profile via
/// `Settings.Connection.Delete()`.
///
/// `connection_path` is a saved-connection object path (e.g.
/// `/org/freedesktop/NetworkManager/Settings/3`), as recorded in
/// [`WifiNetwork::known_network_path`] by [`nm_saved_connections`]. NM removes
/// the profile and emits `Settings.Connection.Removed`, after which the next
/// refresh tick reports the network as no longer `known`.
///
/// # Errors
///
/// Returns a [`hytte_bus::BusError`] if the D-Bus call fails (e.g. the profile
/// was already removed, or policy denies the delete).
pub(crate) async fn nm_forget(connection_path: &str) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(NM_NAME)
        .bus(BusKind::System)
        .at_path(connection_path.to_string())
        .iface(NM_SETTINGS_CONNECTION_IFACE)
        .method("Delete")
        .args(())
        .send::<()>()
        .await
}

/// Set NM's `WirelessEnabled` property via `org.freedesktop.DBus.Properties.Set`.
///
/// # Errors
///
/// Returns a [`hytte_bus::BusError`] if the D-Bus call fails.
pub(crate) async fn nm_set_powered(on: bool) -> Result<(), hytte_bus::BusError> {
    let value = zbus::zvariant::Value::from(on)
        .try_to_owned()
        .map_err(|e| hytte_bus::BusError::Permanent {
            reason: e.to_string(),
            dbus_name: None,
        })?;
    hytte_bus::call(NM_NAME)
        .bus(BusKind::System)
        .at_path(NM_PATH)
        .iface(PROPS_IFACE)
        .method("Set")
        .args((NM_IFACE, "WirelessEnabled", value))
        .send::<()>()
        .await
}

/// Register our secret agent with NM's `AgentManager`.
///
/// Uses `RegisterWithCapabilities(identifier, capabilities)` with
/// `capabilities = 0` (`NM_SECRET_AGENT_CAPABILITY_NONE` — we don't support VPN
/// hints). NM records the *unique* name of the connection this call arrives on
/// and issues `GetSecrets` callbacks back on it, so the agent object must
/// already be exported on the same shared system connection before calling
/// this (it is — both go through `hytte_bus`'s pooled system connection).
///
/// Idempotent: NM lets the same connection re-register; a stale registration
/// from a prior epoch is replaced.
///
/// # Errors
///
/// Returns a [`hytte_bus::BusError`] if the D-Bus call fails (e.g. NM is not
/// running, or policy refuses agent registration).
pub(crate) async fn register_nm_agent() -> Result<(), hytte_bus::BusError> {
    // capabilities = 0 (NONE)
    let capabilities: u32 = 0;
    hytte_bus::call(NM_NAME)
        .bus(BusKind::System)
        .at_path(NM_AGENT_MANAGER_PATH)
        .iface(NM_AGENT_MANAGER_IFACE)
        .method("RegisterWithCapabilities")
        .args((NM_AGENT_IDENTIFIER, capabilities))
        .send::<()>()
        .await
}

// ── Integration-test probe ─────────────────────────────────────────────────────

/// Machine-readable snapshot returned by [`probe_snapshot`].
pub struct ProbeSnapshot {
    /// NM device object path (e.g. `/org/freedesktop/NetworkManager/Devices/2`).
    pub device_path: String,
    /// Whether the Wi-Fi radio is enabled (`WirelessEnabled` on the NM manager).
    pub powered: bool,
    /// Station state as a debug string (e.g. `"Disconnected"`).
    pub station_state: String,
    /// Whether the scan call succeeded.
    pub scan_ok: bool,
    /// Number of visible networks after the scan.
    pub network_count: usize,
}

/// Probe snapshot for the `NixOS` integration test (`checks.wifi-nm-nixos-test`).
///
/// Confirms the NM backend is reachable, finds the Wi-Fi device, reads
/// initial state, triggers a scan, waits briefly, refreshes state, and
/// returns a machine-readable snapshot. Drives the real `wifi_nm` code
/// paths against a live `NetworkManager` end-to-end.
///
/// # Errors
///
/// Returns a string describing the failure if NM is unreachable or no
/// Wi-Fi device is found.
pub async fn probe_snapshot() -> Result<ProbeSnapshot, String> {
    use crate::wifi_backend::{BackendChoice, probe_backend};

    // Verify NM is the chosen backend on the bus.
    let backend = probe_backend().await;
    if backend != BackendChoice::NetworkManager {
        return Err(format!("backend is not NetworkManager: {backend:?}"));
    }

    // Find the Wi-Fi device.
    let device_path = find_wifi_device()
        .await
        .ok_or_else(|| "no Wi-Fi device found".to_string())?;

    // Temporary mutables standing in for the service handles.
    let station_m: Mutable<Option<Station>> = Mutable::new(None);
    let networks_m: Mutable<Vec<WifiNetwork>> = Mutable::new(Vec::new());
    let adapter_m: Mutable<Option<Adapter>> = Mutable::new(None);

    // Initial refresh.
    refresh_nm_state(&device_path, &station_m, &networks_m, &adapter_m).await;

    let powered = adapter_m.get_cloned().is_some_and(|a: Adapter| a.powered);
    let station_state = station_m
        .get_cloned()
        .map_or_else(|| "None".to_string(), |s: Station| format!("{:?}", s.state));

    // Trigger a scan.
    let scan_ok = nm_scan(&device_path).await.is_ok();

    // Wait for scan results to populate (NM takes a couple of seconds minimum).
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;

    // Refresh after the scan.
    refresh_nm_state(&device_path, &station_m, &networks_m, &adapter_m).await;

    let network_count = networks_m.lock_ref().len();

    Ok(ProbeSnapshot {
        device_path,
        powered,
        station_state,
        scan_ok,
        network_count,
    })
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -- ssid_bytes_to_string -------------------------------------------------

    #[test]
    fn ssid_bytes_valid_utf8() {
        assert_eq!(ssid_bytes_to_string(b"FRITZ!Box"), "FRITZ!Box");
    }

    #[test]
    fn ssid_bytes_empty() {
        assert_eq!(ssid_bytes_to_string(b""), "");
    }

    #[test]
    fn ssid_bytes_non_utf8_does_not_panic() {
        let result = ssid_bytes_to_string(&[0xff, 0xfe]);
        assert!(
            !result.is_empty(),
            "lossy decode should produce replacement chars"
        );
    }

    #[test]
    fn ssid_bytes_with_spaces() {
        assert_eq!(ssid_bytes_to_string(b"My Home Network"), "My Home Network");
    }

    // -- nm_device_state_to_station_state -------------------------------------
    //
    // NMDeviceState values from libnm:
    //   0   Unknown
    //   10  Unmanaged
    //   20  Unavailable
    //   30  Disconnected
    //   40  Prepare
    //   50  Config
    //   60  NeedAuth
    //   70  IpConfig
    //   80  IpCheck
    //   90  Secondaries
    //   100 Activated  (← Connected)
    //   110 Deactivating
    //   120 Failed

    #[test]
    fn nm_state_activated_100_is_connected() {
        assert_eq!(
            nm_device_state_to_station_state(100),
            StationState::Connected,
        );
    }

    #[test]
    fn nm_state_deactivating_110_is_disconnecting() {
        assert_eq!(
            nm_device_state_to_station_state(110),
            StationState::Disconnecting,
        );
    }

    #[test]
    fn nm_state_prepare_40_is_connecting() {
        assert_eq!(
            nm_device_state_to_station_state(40),
            StationState::Connecting,
        );
    }

    #[test]
    fn nm_state_need_auth_60_is_connecting() {
        assert_eq!(
            nm_device_state_to_station_state(60),
            StationState::Connecting,
        );
    }

    #[test]
    fn nm_state_ip_config_70_is_connecting() {
        assert_eq!(
            nm_device_state_to_station_state(70),
            StationState::Connecting,
        );
    }

    #[test]
    fn nm_state_disconnected_30_is_disconnected() {
        assert_eq!(
            nm_device_state_to_station_state(30),
            StationState::Disconnected,
        );
    }

    #[test]
    fn nm_state_failed_120_is_disconnected() {
        assert_eq!(
            nm_device_state_to_station_state(120),
            StationState::Disconnected,
        );
    }

    #[test]
    fn nm_state_zero_is_disconnected() {
        assert_eq!(
            nm_device_state_to_station_state(0),
            StationState::Disconnected,
        );
    }

    // -- security_from_flags --------------------------------------------------

    #[test]
    fn security_rsn_is_psk() {
        assert_eq!(security_from_flags(0, 0, 0x01), "psk");
    }

    #[test]
    fn security_wpa_is_psk() {
        assert_eq!(security_from_flags(0, 0x01, 0), "psk");
    }

    #[test]
    fn security_wep_privacy_only() {
        assert_eq!(security_from_flags(1, 0, 0), "wep");
    }

    #[test]
    fn security_open() {
        assert_eq!(security_from_flags(0, 0, 0), "open");
    }

    #[test]
    fn security_rsn_takes_priority_over_privacy_bit() {
        assert_eq!(security_from_flags(1, 0, 0x01), "psk");
    }

    // -- strength_to_dbm ------------------------------------------------------

    #[test]
    fn strength_zero_is_minus_100() {
        assert_eq!(strength_to_dbm(0), -100);
    }

    #[test]
    fn strength_100_is_minus_50() {
        assert_eq!(strength_to_dbm(100), -50);
    }

    #[test]
    fn strength_50_is_midpoint() {
        assert_eq!(strength_to_dbm(50), -75);
    }

    // -- saved_connection_ssid -----------------------------------------------
    //
    // Builds `a{sa{sv}}` connection-settings dicts (the GetSettings shape) and
    // checks the SSID-extraction / known-network-matching logic the watcher
    // relies on. Mirrors the OwnedValue-dict construction in nm_agent.rs tests.

    /// Wrap a value into an `OwnedValue`.
    fn val(v: impl Into<Value<'static>>) -> OwnedValue {
        v.into().try_to_owned().expect("to OwnedValue")
    }

    /// Build one setting sub-dict from `(key, value)` pairs.
    fn setting(pairs: Vec<(&str, OwnedValue)>) -> HashMap<String, OwnedValue> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn ssid_from_wireless_ssid_bytes() {
        let mut conn: ConnectionSettings = HashMap::new();
        conn.insert(
            "802-11-wireless".to_string(),
            setting(vec![("ssid", val(b"FRITZ!Box".to_vec()))]),
        );
        assert_eq!(saved_connection_ssid(&conn).as_deref(), Some("FRITZ!Box"));
    }

    #[test]
    fn ssid_trims_surrounding_whitespace() {
        let mut conn: ConnectionSettings = HashMap::new();
        conn.insert(
            "802-11-wireless".to_string(),
            setting(vec![("ssid", val(b"  My Home Net  ".to_vec()))]),
        );
        assert_eq!(saved_connection_ssid(&conn).as_deref(), Some("My Home Net"));
    }

    #[test]
    fn ssid_none_for_non_wifi_connection() {
        // Ethernet / VPN profiles have no `802-11-wireless` setting → skipped.
        let mut conn: ConnectionSettings = HashMap::new();
        conn.insert(
            "connection".to_string(),
            setting(vec![("id", val("Wired connection 1"))]),
        );
        conn.insert("802-3-ethernet".to_string(), HashMap::new());
        assert_eq!(saved_connection_ssid(&conn), None);
    }

    #[test]
    fn ssid_none_when_wireless_setting_lacks_ssid() {
        let mut conn: ConnectionSettings = HashMap::new();
        conn.insert("802-11-wireless".to_string(), HashMap::new());
        assert_eq!(saved_connection_ssid(&conn), None);
    }

    #[test]
    fn ssid_none_for_empty_ssid_bytes() {
        let mut conn: ConnectionSettings = HashMap::new();
        conn.insert(
            "802-11-wireless".to_string(),
            setting(vec![("ssid", val(Vec::<u8>::new()))]),
        );
        assert_eq!(saved_connection_ssid(&conn), None);
    }

    #[test]
    fn ssid_none_for_whitespace_only_ssid() {
        let mut conn: ConnectionSettings = HashMap::new();
        conn.insert(
            "802-11-wireless".to_string(),
            setting(vec![("ssid", val(b"   ".to_vec()))]),
        );
        assert_eq!(saved_connection_ssid(&conn), None);
    }

    // -- wifi_network_from_ap_props: known-network matching -------------------

    /// Minimal AP props dict with just an SSID; enough to exercise the
    /// saved-connection lookup that decides `known` / `known_network_path`.
    fn ap_props_with_ssid(ssid: &[u8]) -> HashMap<String, OwnedValue> {
        let mut props = HashMap::new();
        props.insert("Ssid".to_string(), val(ssid.to_vec()));
        props
    }

    #[test]
    fn ap_marked_known_when_saved_connection_matches() {
        let props = ap_props_with_ssid(b"FRITZ!Box");
        let mut saved = HashMap::new();
        saved.insert(
            "FRITZ!Box".to_string(),
            "/org/freedesktop/NetworkManager/Settings/3".to_string(),
        );
        let net = wifi_network_from_ap_props("/ap/0", &props, "", &saved)
            .expect("non-empty SSID yields a network");
        assert!(net.known);
        assert_eq!(
            net.known_network_path.as_deref(),
            Some("/org/freedesktop/NetworkManager/Settings/3"),
        );
    }

    #[test]
    fn ap_not_known_when_no_saved_connection() {
        let props = ap_props_with_ssid(b"FRITZ!Box");
        let saved: HashMap<String, String> = HashMap::new();
        let net = wifi_network_from_ap_props("/ap/0", &props, "", &saved)
            .expect("non-empty SSID yields a network");
        assert!(!net.known);
        assert_eq!(net.known_network_path, None);
    }

    #[test]
    fn ap_known_match_is_exact_on_ssid() {
        // A saved connection for a *different* SSID must not mark this AP known.
        let props = ap_props_with_ssid(b"FRITZ!Box");
        let mut saved = HashMap::new();
        saved.insert(
            "Other Net".to_string(),
            "/org/freedesktop/NetworkManager/Settings/9".to_string(),
        );
        let net = wifi_network_from_ap_props("/ap/0", &props, "", &saved)
            .expect("non-empty SSID yields a network");
        assert!(!net.known);
        assert_eq!(net.known_network_path, None);
    }
}
