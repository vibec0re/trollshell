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
const NM_ACTIVE_CONNECTION_IFACE: &str = "org.freedesktop.NetworkManager.Connection.Active";

/// A connection settings dict: `a{sa{sv}}` — setting name → (key → value).
/// This is exactly the shape `Settings.Connection.GetSettings()` returns.
type ConnectionSettings = HashMap<String, HashMap<String, OwnedValue>>;

/// A saved `NetworkManager` wired (ethernet) connection profile, surfaced to the
/// network panel so it can be activated / deactivated / forgotten.
///
/// Built once per refresh tick by [`nm_wired_profiles`] from the
/// `802-3-ethernet` saved connections, joined against NM's active connections
/// (for [`WiredProfile::active`]) and the ethernet devices (for
/// [`WiredProfile::device_path`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WiredProfile {
    /// Display name (the `connection.id` field of the saved profile).
    pub name: String,
    /// The saved-connection object path
    /// (e.g. `/org/freedesktop/NetworkManager/Settings/3`). `forget` deletes
    /// this; `activate` passes it to `ActivateConnection`.
    pub connection_path: String,
    /// The NM ethernet device this profile binds to, when one can be resolved
    /// (by `connection.interface-name` match, or by the device whose active
    /// connection references this profile). `activate`/`deactivate` need it.
    pub device_path: Option<String>,
    /// Whether this profile is currently active (an active connection references
    /// its settings path).
    pub active: bool,
}

/// A saved `NetworkManager` VPN connection profile, surfaced to the VPN panel
/// so it can be activated / deactivated.
///
/// Built once per refresh tick by [`nm_vpn_profiles`] from the `vpn` saved
/// connections, joined against NM's active connections. Unlike a wired profile,
/// a VPN does **not** bind to a device — it rides the primary connection — so
/// activation passes `"/"` for both the device and specific-object. Deactivation
/// targets the *active-connection* object path (not a device), captured here in
/// [`VpnProfile::active_connection_path`] when the profile is up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VpnProfile {
    /// Display name (the `connection.id` field of the saved profile).
    pub name: String,
    /// The saved-connection object path
    /// (e.g. `/org/freedesktop/NetworkManager/Settings/5`). `activate` passes
    /// this to `ActivateConnection`.
    pub connection_path: String,
    /// Whether this profile is currently active (an active connection references
    /// its settings path).
    pub active: bool,
    /// When [`VpnProfile::active`], the NM *active-connection* object path
    /// (e.g. `/org/freedesktop/NetworkManager/ActiveConnection/7`) that
    /// `DeactivateConnection` targets. `None` when inactive.
    pub active_connection_path: Option<String>,
}

/// Stable identifier for our secret agent. NM keys registered agents by this
/// reverse-DNS string; reusing it across restarts lets NM replace a stale
/// registration cleanly.
pub(crate) const NM_AGENT_IDENTIFIER: &str = "mov.vibec0re.trollshell";

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

/// Read a string field from the `connection` setting sub-dict
/// (e.g. `connection.id`, `connection.interface-name`).
///
/// Returns `None` if the `connection` setting or the key is absent, or the
/// value isn't a string.
fn connection_string_field(settings: &ConnectionSettings, key: &str) -> Option<String> {
    settings
        .get("connection")?
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| String::try_from(v).ok())
}

/// Extract the wired-profile display name from a saved connection's settings
/// dict, or `None` if it isn't an ethernet (`802-3-ethernet`) profile.
///
/// A profile is wired iff it carries an `802-3-ethernet` setting. The display
/// name is `connection.id`; if that's missing or blank we fall back to
/// `"Wired connection"` so the row is never nameless. Wi-Fi / VPN / other
/// profiles return `None` and are skipped by [`nm_wired_profiles`].
fn wired_profile_name(settings: &ConnectionSettings) -> Option<String> {
    if !settings.contains_key("802-3-ethernet") {
        return None;
    }
    let name = connection_string_field(settings, "id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Wired connection".to_string());
    Some(name)
}

/// Extract the VPN-profile display name from a saved connection's settings
/// dict, or `None` if it isn't a VPN profile.
///
/// A profile is a VPN iff it carries a top-level `vpn` setting **or** its
/// `connection.type` is `"vpn"` (NM always writes both for a real VPN, but we
/// accept either so a sparse dict still matches). The display name is
/// `connection.id`; if that's missing or blank we fall back to `"VPN
/// connection"` so the row is never nameless. Non-VPN profiles return `None`
/// and are skipped by [`nm_vpn_profiles`].
fn vpn_profile_name(settings: &ConnectionSettings) -> Option<String> {
    let has_vpn_setting = settings.contains_key("vpn");
    let is_vpn_type = connection_string_field(settings, "type").as_deref() == Some("vpn");
    if !has_vpn_setting && !is_vpn_type {
        return None;
    }
    let name = connection_string_field(settings, "id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "VPN connection".to_string());
    Some(name)
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
    hytte_bus::call(BusKind::System, NM_NAME)
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
    hytte_bus::call(BusKind::System, NM_NAME)
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
    hytte_bus::call(BusKind::System, NM_NAME)
        .at_path(device.to_string())
        .iface(PROPS_IFACE)
        .method("GetAll")
        .args((NM_WIRELESS_IFACE,))
        .send::<HashMap<String, OwnedValue>>()
        .await
}

async fn get_manager_props() -> Result<HashMap<String, OwnedValue>, hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, NM_NAME)
        .at_path(NM_PATH)
        .iface(PROPS_IFACE)
        .method("GetAll")
        .args((NM_IFACE,))
        .send::<HashMap<String, OwnedValue>>()
        .await
}

async fn get_ap_props(ap: &str) -> Result<HashMap<String, OwnedValue>, hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, NM_NAME)
        .at_path(ap.to_string())
        .iface(PROPS_IFACE)
        .method("GetAll")
        .args((NM_AP_IFACE,))
        .send::<HashMap<String, OwnedValue>>()
        .await
}

async fn get_all_access_points(device: &str) -> Result<Vec<OwnedObjectPath>, hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, NM_NAME)
        .at_path(device.to_string())
        .iface(NM_WIRELESS_IFACE)
        .method("GetAllAccessPoints")
        .args(())
        .send::<Vec<OwnedObjectPath>>()
        .await
}

async fn get_active_connection_props(
    active: &str,
) -> Result<HashMap<String, OwnedValue>, hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, NM_NAME)
        .at_path(active.to_string())
        .iface(PROPS_IFACE)
        .method("GetAll")
        .args((NM_ACTIVE_CONNECTION_IFACE,))
        .send::<HashMap<String, OwnedValue>>()
        .await
}

// ── Wired (ethernet) enumeration ───────────────────────────────────────────────

/// Read an object-path property out of a props dict, normalising NM's `"/"`
/// (no-object) sentinel to `None`.
fn prop_object_path(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let path = props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| OwnedObjectPath::try_from(v).ok())?;
    let s = path.as_str();
    if s == "/" { None } else { Some(s.to_string()) }
}

/// Collect the saved-connection settings paths of all *currently active*
/// connections (manager `ActiveConnections` → each active connection's
/// `Connection` property). Used to mark wired profiles active.
///
/// A failure on any single active connection is logged and skipped; a failure
/// to read the manager's `ActiveConnections` yields an empty set (every profile
/// reports inactive — the safe degraded state).
async fn active_connection_settings_paths() -> std::collections::HashSet<String> {
    let manager_props = match get_manager_props().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "wifi_nm: manager GetAll (ActiveConnections) failed");
            return std::collections::HashSet::new();
        }
    };
    let actives: Vec<OwnedObjectPath> = manager_props
        .get("ActiveConnections")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| <Vec<OwnedObjectPath>>::try_from(v).ok())
        .unwrap_or_default();

    let mut paths = std::collections::HashSet::new();
    for active in actives {
        match get_active_connection_props(active.as_str()).await {
            Ok(props) => {
                if let Some(conn) = prop_object_path(&props, "Connection") {
                    paths.insert(conn);
                }
            }
            Err(e) => {
                tracing::debug!(path = active.as_str(), error = %e, "wifi_nm: active connection props read failed");
            }
        }
    }
    paths
}

/// Build a map **settings path → active-connection object path** over all
/// currently-active connections (manager `ActiveConnections` → each active
/// connection's `Connection` property).
///
/// Used to mark VPN profiles active *and* capture the active-connection object
/// path that `DeactivateConnection` needs. (Wired uses
/// [`active_connection_settings_paths`], which only needs the set of settings
/// paths; VPN additionally needs the active-connection path to deactivate, since
/// VPNs deactivate via `DeactivateConnection(active)`, not `Device.Disconnect`.)
///
/// A failure on any single active connection is logged and skipped; a failure to
/// read the manager's `ActiveConnections` yields an empty map (every profile
/// reports inactive — the safe degraded state).
async fn active_connection_map() -> HashMap<String, String> {
    let manager_props = match get_manager_props().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "wifi_nm: manager GetAll (ActiveConnections, vpn) failed");
            return HashMap::new();
        }
    };
    let actives: Vec<OwnedObjectPath> = manager_props
        .get("ActiveConnections")
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| <Vec<OwnedObjectPath>>::try_from(v).ok())
        .unwrap_or_default();

    let mut map = HashMap::new();
    for active in actives {
        let active_path = active.as_str().to_string();
        match get_active_connection_props(&active_path).await {
            Ok(props) => {
                if let Some(conn) = prop_object_path(&props, "Connection") {
                    // Last active wins on the rare duplicate; fine for this control.
                    map.insert(conn, active_path);
                }
            }
            Err(e) => {
                tracing::debug!(path = %active_path, error = %e, "wifi_nm: active connection props read failed (vpn)");
            }
        }
    }
    map
}

/// One discovered NM ethernet device: its object path and the interface name
/// (`Interface` property), used to bind saved profiles to a device.
struct EthernetDevice {
    path: String,
    interface: String,
    /// The settings path this device's active connection references, if any.
    active_connection_settings: Option<String>,
}

/// Enumerate all NM ethernet devices (`DeviceType == 1`), reading each one's
/// interface name and active-connection settings path. Returns an empty vec on
/// `GetDevices` failure (every wired profile then reports no device — degraded
/// but non-fatal).
async fn find_ethernet_devices() -> Vec<EthernetDevice> {
    let devices = match get_devices().await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(error = %e, "wifi_nm: GetDevices (ethernet) failed");
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for dev in devices {
        let dev_str = dev.as_str();
        let props = match get_device_props(dev_str).await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(path = dev_str, error = %e, "wifi_nm: ethernet device props read failed");
                continue;
            }
        };
        // DeviceType: 1 = Ethernet.
        if property::<u32>(&props, "DeviceType") != Some(1) {
            continue;
        }
        let interface = property::<String>(&props, "Interface").unwrap_or_default();
        // The device's ActiveConnection points at an active-connection object,
        // whose `Connection` property is the saved-settings path that's up.
        let active_connection_settings = match prop_object_path(&props, "ActiveConnection") {
            Some(active_path) => match get_active_connection_props(&active_path).await {
                Ok(ac_props) => prop_object_path(&ac_props, "Connection"),
                Err(e) => {
                    tracing::debug!(path = %active_path, error = %e, "wifi_nm: ethernet ActiveConnection read failed");
                    None
                }
            },
            None => None,
        };
        out.push(EthernetDevice {
            path: dev_str.to_string(),
            interface,
            active_connection_settings,
        });
    }
    out
}

/// Choose the device path a wired profile binds to.
///
/// Preference order: (1) the device whose active connection already references
/// this profile's settings path (so deactivate targets the right NIC even with
/// no interface-name pin); (2) a device whose `Interface` matches the profile's
/// `connection.interface-name`; (3) the sole ethernet device if there's exactly
/// one (the common single-NIC case, where unpinned profiles are ambiguous but
/// in practice apply to that one NIC); else `None`.
fn device_for_wired_profile<'a>(
    settings: &ConnectionSettings,
    connection_path: &str,
    devices: &'a [EthernetDevice],
) -> Option<&'a EthernetDevice> {
    if let Some(dev) = devices
        .iter()
        .find(|d| d.active_connection_settings.as_deref() == Some(connection_path))
    {
        return Some(dev);
    }
    if let Some(iface) = connection_string_field(settings, "interface-name") {
        let iface = iface.trim();
        if !iface.is_empty()
            && let Some(dev) = devices.iter().find(|d| d.interface == iface)
        {
            return Some(dev);
        }
    }
    if devices.len() == 1 {
        return devices.first();
    }
    None
}

/// Build the list of saved wired (ethernet) profiles for the panel.
///
/// Enumerates `Settings.ListConnections`, keeps `802-3-ethernet` profiles,
/// resolves each to a device (see [`device_for_wired_profile`]) and marks it
/// active if its settings path is in the active-connection set. A failure on a
/// single connection is logged and skipped; a `ListConnections` failure yields
/// an empty list. Profiles are sorted by name for a stable display order.
async fn nm_wired_profiles() -> Vec<WiredProfile> {
    let connections = match list_connections().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "wifi_nm: Settings.ListConnections (wired) failed");
            return Vec::new();
        }
    };

    let devices = find_ethernet_devices().await;
    let active_paths = active_connection_settings_paths().await;

    let mut profiles = Vec::new();
    for conn in connections {
        let conn_str = conn.as_str();
        let settings = match get_connection_settings(conn_str).await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(path = conn_str, error = %e, "wifi_nm: GetSettings (wired) failed");
                continue;
            }
        };
        let Some(name) = wired_profile_name(&settings) else {
            continue;
        };
        let device_path =
            device_for_wired_profile(&settings, conn_str, &devices).map(|d| d.path.clone());
        let active = active_paths.contains(conn_str);
        profiles.push(WiredProfile {
            name,
            connection_path: conn_str.to_string(),
            device_path,
            active,
        });
    }

    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    profiles
}

/// Build the list of saved VPN profiles for the panel.
///
/// Enumerates `Settings.ListConnections`, keeps `vpn` profiles (see
/// [`vpn_profile_name`]), and marks each active if its settings path is in the
/// active-connection map — capturing the *active-connection object path* for the
/// active ones so [`crate::wifi::vpn_deactivate`] can `DeactivateConnection` it.
/// A failure on a single connection is logged and skipped; a `ListConnections`
/// failure yields an empty list. Profiles are sorted by name for a stable order.
async fn nm_vpn_profiles() -> Vec<VpnProfile> {
    let connections = match list_connections().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "wifi_nm: Settings.ListConnections (vpn) failed");
            return Vec::new();
        }
    };

    let active_map = active_connection_map().await;

    let mut profiles = Vec::new();
    for conn in connections {
        let conn_str = conn.as_str();
        let settings = match get_connection_settings(conn_str).await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(path = conn_str, error = %e, "wifi_nm: GetSettings (vpn) failed");
                continue;
            }
        };
        let Some(name) = vpn_profile_name(&settings) else {
            continue;
        };
        let active_connection_path = active_map.get(conn_str).cloned();
        let active = active_connection_path.is_some();
        profiles.push(VpnProfile {
            name,
            connection_path: conn_str.to_string(),
            active,
            active_connection_path,
        });
    }

    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    profiles
}

// ── Saved-connection enumeration ───────────────────────────────────────────────

/// List all saved connection profiles (`Settings.ListConnections() -> ao`).
async fn list_connections() -> Result<Vec<OwnedObjectPath>, hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, NM_NAME)
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
    hytte_bus::call(BusKind::System, NM_NAME)
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
///
/// Also re-enumerates saved wired (ethernet) and VPN profiles into `wired` /
/// `vpn` on the same tick (one extra `ListConnections`/`GetSettings` pass each)
/// so the network/VPN panels stay in sync without a second NM watcher.
async fn refresh_nm_state(
    device_path: &str,
    station: &Mutable<Option<Station>>,
    networks: &Mutable<Vec<WifiNetwork>>,
    adapter: &Mutable<Option<Adapter>>,
    wired: &Mutable<Vec<WiredProfile>>,
    vpn: &Mutable<Vec<VpnProfile>>,
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

    // --- saved wired (ethernet) + VPN profiles (same tick) ---
    refresh_wired_profiles(wired).await;
    refresh_vpn_profiles(vpn).await;
}

/// Re-enumerate saved wired (ethernet) profiles and publish them, but only set
/// the mutable when the list actually changed — so a per-tick refresh on an
/// unchanged set doesn't churn the panel's bind (drain-rebuild) needlessly.
async fn refresh_wired_profiles(wired: &Mutable<Vec<WiredProfile>>) {
    let profiles = nm_wired_profiles().await;
    if *wired.lock_ref() != profiles {
        wired.set(profiles);
    }
}

/// Re-enumerate saved VPN profiles and publish them, diffed-before-set (same
/// rationale as [`refresh_wired_profiles`]).
async fn refresh_vpn_profiles(vpn: &Mutable<Vec<VpnProfile>>) {
    let profiles = nm_vpn_profiles().await;
    if *vpn.lock_ref() != profiles {
        vpn.set(profiles);
    }
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
    wired: Mutable<Vec<WiredProfile>>,
    vpn: Mutable<Vec<VpnProfile>>,
    device_path_store: Arc<RwLock<String>>,
) {
    loop {
        // Retry discovery every 5 s if NM isn't ready yet. Wired/VPN profiles are
        // independent of the Wi-Fi device (a desktop may have ethernet/VPN but no
        // radio), so keep them refreshed even while no Wi-Fi device exists —
        // otherwise those cards would never populate on such machines.
        let Some(device_path) = find_wifi_device().await else {
            refresh_wired_profiles(&wired).await;
            refresh_vpn_profiles(&vpn).await;
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            continue;
        };

        // Publish device path for command helpers.
        *device_path_store.write().await = device_path.clone();

        // Initial full refresh.
        refresh_nm_state(&device_path, &station, &networks, &adapter, &wired, &vpn).await;

        // Subscribe to PropertiesChanged on the device and on the manager.
        let device_sub = hytte_bus::signals(BusKind::System, NM_NAME)
            .at_path(device_path.clone())
            .iface(PROPS_IFACE)
            .signal("PropertiesChanged")
            .start();

        let manager_sub = hytte_bus::signals(BusKind::System, NM_NAME)
            .at_path(NM_PATH)
            .iface(PROPS_IFACE)
            .signal("PropertiesChanged")
            .start();

        // Also watch AccessPointAdded / AccessPointRemoved on the device.
        let ap_added_sub = hytte_bus::signals(BusKind::System, NM_NAME)
            .at_path(device_path.clone())
            .iface(NM_WIRELESS_IFACE)
            .signal("AccessPointAdded")
            .start();

        let ap_removed_sub = hytte_bus::signals(BusKind::System, NM_NAME)
            .at_path(device_path.clone())
            .iface(NM_WIRELESS_IFACE)
            .signal("AccessPointRemoved")
            .start();

        // Watch the manager's DeviceRemoved signal so we can re-discover when
        // the Wi-Fi device is unplugged (USB dongle) or otherwise unregistered
        // by NM.  The signal carries a single object-path argument.
        let device_removed_sub = hytte_bus::signals(BusKind::System, NM_NAME)
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
                    refresh_nm_state(&device_path, &station, &networks, &adapter, &wired, &vpn).await;
                }
                Some(_) = manager_events.next() => {
                    refresh_nm_state(&device_path, &station, &networks, &adapter, &wired, &vpn).await;
                }
                Some(_) = ap_added_events.next() => {
                    refresh_nm_state(&device_path, &station, &networks, &adapter, &wired, &vpn).await;
                }
                Some(_) = ap_removed_events.next() => {
                    refresh_nm_state(&device_path, &station, &networks, &adapter, &wired, &vpn).await;
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
    hytte_bus::call(BusKind::System, NM_NAME)
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
    hytte_bus::call(BusKind::System, NM_NAME)
        .at_path(NM_PATH)
        .iface(NM_IFACE)
        .method("ActivateConnection")
        .args((connection_path, device_obj_path, ap_obj_path))
        .send::<OwnedObjectPath>()
        .await
        .map(|_| ())
}

/// Parse a string into an owned D-Bus `ObjectPath`, mapping a bad path to a
/// permanent [`hytte_bus::BusError`] (so callers `?`-propagate it).
fn owned_object_path(
    path: &str,
    what: &str,
) -> Result<zbus::zvariant::OwnedObjectPath, hytte_bus::BusError> {
    Ok(zbus::zvariant::ObjectPath::try_from(path)
        .map_err(|e| hytte_bus::BusError::Permanent {
            reason: format!("invalid {what}: {e}"),
            dbus_name: None,
        })?
        .to_owned()
        .into())
}

/// Activate a saved connection profile on a specific device via
/// `ActivateConnection(connection, device, specific_object="/")`.
///
/// Unlike [`nm_connect`] (which passes `"/"` for the connection so NM
/// auto-selects), this passes the *real* saved-connection object path and a
/// `"/"` specific-object — exactly the call needed to bring up a wired
/// (ethernet) profile on its NIC. NM consults the secret agent only if the
/// profile is missing stored secrets (ethernet profiles normally aren't).
///
/// # Errors
///
/// Returns a [`hytte_bus::BusError`] if either path is malformed or the D-Bus
/// call fails (e.g. policy denies activation).
pub(crate) async fn nm_activate_connection(
    connection_path: &str,
    device_path: &str,
) -> Result<(), hytte_bus::BusError> {
    let connection_obj_path = owned_object_path(connection_path, "connection path")?;
    let device_obj_path = owned_object_path(device_path, "device path")?;
    let specific_object = owned_object_path("/", "specific object")?;
    hytte_bus::call(BusKind::System, NM_NAME)
        .at_path(NM_PATH)
        .iface(NM_IFACE)
        .method("ActivateConnection")
        .args((connection_obj_path, device_obj_path, specific_object))
        .send::<OwnedObjectPath>()
        .await
        .map(|_| ())
}

/// Activate a saved VPN connection profile via
/// `ActivateConnection(connection, device="/", specific_object="/")`.
///
/// A VPN rides the primary (default) connection rather than binding to a
/// specific device, so we pass NM's `"/"` no-object sentinel for **both** the
/// device and the specific-object (unlike [`nm_activate_connection`], which
/// targets a real ethernet device). NM resolves the base device itself and, if
/// the profile is missing stored secrets, asks the registered secret agent
/// (our [`crate::wifi::nm_agent`]) for them via the prompt overlay.
///
/// # Errors
///
/// Returns a [`hytte_bus::BusError`] if the connection path is malformed or the
/// D-Bus call fails (e.g. policy denies activation).
pub(crate) async fn nm_activate_vpn(connection_path: &str) -> Result<(), hytte_bus::BusError> {
    let connection_obj_path = owned_object_path(connection_path, "connection path")?;
    let device_obj_path = owned_object_path("/", "device")?;
    let specific_object = owned_object_path("/", "specific object")?;
    hytte_bus::call(BusKind::System, NM_NAME)
        .at_path(NM_PATH)
        .iface(NM_IFACE)
        .method("ActivateConnection")
        .args((connection_obj_path, device_obj_path, specific_object))
        .send::<OwnedObjectPath>()
        .await
        .map(|_| ())
}

/// Deactivate an active connection via `Manager.DeactivateConnection(active)`.
///
/// `active_conn_path` is an *active-connection* object path (e.g.
/// `/org/freedesktop/NetworkManager/ActiveConnection/7`), as captured in
/// [`VpnProfile::active_connection_path`]. This is the correct teardown for a
/// VPN — VPNs are **not** disconnected via `Device.Disconnect` (they have no
/// device of their own), so [`nm_disconnect`] would be wrong here.
///
/// # Errors
///
/// Returns a [`hytte_bus::BusError`] if the path is malformed or the D-Bus call
/// fails (e.g. the connection was already torn down, or policy denies it).
pub(crate) async fn nm_deactivate_connection(
    active_conn_path: &str,
) -> Result<(), hytte_bus::BusError> {
    let active_obj_path = owned_object_path(active_conn_path, "active connection path")?;
    hytte_bus::call(BusKind::System, NM_NAME)
        .at_path(NM_PATH)
        .iface(NM_IFACE)
        .method("DeactivateConnection")
        .args((active_obj_path,))
        .send::<()>()
        .await
}

/// Disconnect the NM device.
///
/// # Errors
///
/// Returns a [`hytte_bus::BusError`] if the D-Bus call fails.
pub(crate) async fn nm_disconnect(device_path: &str) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, NM_NAME)
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
    hytte_bus::call(BusKind::System, NM_NAME)
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
    hytte_bus::call(BusKind::System, NM_NAME)
        .at_path(NM_PATH)
        .iface(PROPS_IFACE)
        .method("Set")
        .args((NM_IFACE, "WirelessEnabled", value))
        .send::<()>()
        .await
}

/// `NMSecretAgentCapabilities` bit 0 — `VPN_HINTS`. Advertises that the agent
/// understands the VPN service-type / per-secret hints NM passes alongside a
/// `vpn` `GetSecrets` request, so NM includes them. (An agent receives `vpn`
/// `GetSecrets` callbacks regardless of this bit; setting it only enables the
/// hints — we use the first usable hint to pick which secret key to prompt for.)
const NM_SECRET_AGENT_CAPABILITY_VPN_HINTS: u32 = 0x1;

/// Register our secret agent with NM's `AgentManager`.
///
/// Uses `RegisterWithCapabilities(identifier, capabilities)` with
/// `capabilities = NM_SECRET_AGENT_CAPABILITY_VPN_HINTS` so NM passes the VPN
/// service-type / secret-name hints with `vpn` `GetSecrets` requests. NM records
/// the *unique* name of the connection this call arrives on and issues
/// `GetSecrets` callbacks back on it, so the agent object must already be
/// exported on the same shared system connection before calling this (it is —
/// both go through `hytte_bus`'s pooled system connection).
///
/// Idempotent: NM lets the same connection re-register; a stale registration
/// from a prior epoch is replaced.
///
/// # Errors
///
/// Returns a [`hytte_bus::BusError`] if the D-Bus call fails (e.g. NM is not
/// running, or policy refuses agent registration).
pub(crate) async fn register_nm_agent() -> Result<(), hytte_bus::BusError> {
    let capabilities: u32 = NM_SECRET_AGENT_CAPABILITY_VPN_HINTS;
    hytte_bus::call(BusKind::System, NM_NAME)
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
    let wired_m: Mutable<Vec<WiredProfile>> = Mutable::new(Vec::new());
    let vpn_m: Mutable<Vec<VpnProfile>> = Mutable::new(Vec::new());

    // Initial refresh.
    refresh_nm_state(
        &device_path,
        &station_m,
        &networks_m,
        &adapter_m,
        &wired_m,
        &vpn_m,
    )
    .await;

    let powered = adapter_m.get_cloned().is_some_and(|a: Adapter| a.powered);
    let station_state = station_m
        .get_cloned()
        .map_or_else(|| "None".to_string(), |s: Station| format!("{:?}", s.state));

    // Trigger a scan.
    let scan_ok = nm_scan(&device_path).await.is_ok();

    // Wait for scan results to populate (NM takes a couple of seconds minimum).
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;

    // Refresh after the scan.
    refresh_nm_state(
        &device_path,
        &station_m,
        &networks_m,
        &adapter_m,
        &wired_m,
        &vpn_m,
    )
    .await;

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

    // -- wired_profile_name ---------------------------------------------------
    //
    // Same `a{sa{sv}}` GetSettings shape, exercising the ethernet-profile
    // recogniser the wired card relies on.

    /// Build a minimal ethernet `connection`-settings dict.
    fn ethernet_conn(id: Option<&str>, iface: Option<&str>) -> ConnectionSettings {
        let mut conn: ConnectionSettings = HashMap::new();
        let mut connection = vec![("type", val("802-3-ethernet".to_string()))];
        if let Some(id) = id {
            connection.push(("id", val(id.to_string())));
        }
        if let Some(iface) = iface {
            connection.push(("interface-name", val(iface.to_string())));
        }
        conn.insert("connection".to_string(), setting(connection));
        conn.insert("802-3-ethernet".to_string(), HashMap::new());
        conn
    }

    #[test]
    fn ethernet_name_from_connection_id() {
        let conn = ethernet_conn(Some("Wired connection 1"), Some("enp3s0"));
        assert_eq!(
            wired_profile_name(&conn).as_deref(),
            Some("Wired connection 1"),
        );
    }

    #[test]
    fn ethernet_name_trims_whitespace() {
        let conn = ethernet_conn(Some("  Office LAN  "), None);
        assert_eq!(wired_profile_name(&conn).as_deref(), Some("Office LAN"));
    }

    #[test]
    fn ethernet_name_falls_back_when_id_missing() {
        let conn = ethernet_conn(None, Some("enp3s0"));
        assert_eq!(
            wired_profile_name(&conn).as_deref(),
            Some("Wired connection")
        );
    }

    #[test]
    fn ethernet_name_falls_back_when_id_blank() {
        let conn = ethernet_conn(Some("   "), None);
        assert_eq!(
            wired_profile_name(&conn).as_deref(),
            Some("Wired connection")
        );
    }

    #[test]
    fn none_for_wifi_connection() {
        // A Wi-Fi profile (no `802-3-ethernet` setting) is not a wired profile.
        let mut conn: ConnectionSettings = HashMap::new();
        conn.insert(
            "connection".to_string(),
            setting(vec![
                ("id", val("FRITZ!Box")),
                ("type", val("802-11-wireless")),
            ]),
        );
        conn.insert(
            "802-11-wireless".to_string(),
            setting(vec![("ssid", val(b"FRITZ!Box".to_vec()))]),
        );
        assert_eq!(wired_profile_name(&conn), None);
    }

    #[test]
    fn none_for_vpn_connection() {
        // A VPN profile (no `802-3-ethernet` setting) is not a wired profile.
        let mut conn: ConnectionSettings = HashMap::new();
        conn.insert(
            "connection".to_string(),
            setting(vec![("id", val("Work VPN")), ("type", val("vpn"))]),
        );
        conn.insert("vpn".to_string(), HashMap::new());
        assert_eq!(wired_profile_name(&conn), None);
    }

    // -- vpn_profile_name -----------------------------------------------------
    //
    // Same `a{sa{sv}}` GetSettings shape, exercising the VPN-profile recogniser
    // the VPN panel relies on.

    /// Build a minimal VPN `connection`-settings dict. `with_vpn_setting`
    /// controls whether a top-level `vpn` setting is present; `kind` sets
    /// `connection.type`.
    fn vpn_conn(
        id: Option<&str>,
        kind: Option<&str>,
        with_vpn_setting: bool,
    ) -> ConnectionSettings {
        let mut conn: ConnectionSettings = HashMap::new();
        let mut connection = Vec::new();
        if let Some(id) = id {
            connection.push(("id", val(id.to_string())));
        }
        if let Some(kind) = kind {
            connection.push(("type", val(kind.to_string())));
        }
        conn.insert("connection".to_string(), setting(connection));
        if with_vpn_setting {
            conn.insert("vpn".to_string(), HashMap::new());
        }
        conn
    }

    #[test]
    fn vpn_name_from_connection_id() {
        let conn = vpn_conn(Some("Work VPN"), Some("vpn"), true);
        assert_eq!(vpn_profile_name(&conn).as_deref(), Some("Work VPN"));
    }

    #[test]
    fn vpn_name_matches_on_type_without_vpn_setting() {
        // `connection.type == "vpn"` alone is enough to recognise a VPN.
        let conn = vpn_conn(Some("Sparse VPN"), Some("vpn"), false);
        assert_eq!(vpn_profile_name(&conn).as_deref(), Some("Sparse VPN"));
    }

    #[test]
    fn vpn_name_matches_on_vpn_setting_without_type() {
        // A top-level `vpn` setting alone is enough, even with no type.
        let conn = vpn_conn(Some("Settingful VPN"), None, true);
        assert_eq!(vpn_profile_name(&conn).as_deref(), Some("Settingful VPN"));
    }

    #[test]
    fn vpn_name_trims_whitespace() {
        let conn = vpn_conn(Some("  Office VPN  "), Some("vpn"), true);
        assert_eq!(vpn_profile_name(&conn).as_deref(), Some("Office VPN"));
    }

    #[test]
    fn vpn_name_falls_back_when_id_missing() {
        let conn = vpn_conn(None, Some("vpn"), true);
        assert_eq!(vpn_profile_name(&conn).as_deref(), Some("VPN connection"));
    }

    #[test]
    fn vpn_name_falls_back_when_id_blank() {
        let conn = vpn_conn(Some("   "), None, true);
        assert_eq!(vpn_profile_name(&conn).as_deref(), Some("VPN connection"));
    }

    #[test]
    fn vpn_name_none_for_ethernet() {
        let conn = ethernet_conn(Some("Wired connection 1"), Some("enp3s0"));
        assert_eq!(vpn_profile_name(&conn), None);
    }

    #[test]
    fn vpn_name_none_for_wifi() {
        // A Wi-Fi profile (no `vpn` setting, type != "vpn") is not a VPN.
        let mut conn: ConnectionSettings = HashMap::new();
        conn.insert(
            "connection".to_string(),
            setting(vec![
                ("id", val("FRITZ!Box")),
                ("type", val("802-11-wireless")),
            ]),
        );
        conn.insert(
            "802-11-wireless".to_string(),
            setting(vec![("ssid", val(b"FRITZ!Box".to_vec()))]),
        );
        assert_eq!(vpn_profile_name(&conn), None);
    }

    // -- connection_string_field ----------------------------------------------

    #[test]
    fn interface_name_read_from_connection_setting() {
        let conn = ethernet_conn(Some("LAN"), Some("enp3s0"));
        assert_eq!(
            connection_string_field(&conn, "interface-name").as_deref(),
            Some("enp3s0"),
        );
    }

    #[test]
    fn connection_field_none_when_absent() {
        let conn = ethernet_conn(Some("LAN"), None);
        assert_eq!(connection_string_field(&conn, "interface-name"), None);
    }

    // -- device_for_wired_profile ---------------------------------------------

    fn eth_device(path: &str, iface: &str, active_for: Option<&str>) -> EthernetDevice {
        EthernetDevice {
            path: path.to_string(),
            interface: iface.to_string(),
            active_connection_settings: active_for.map(ToString::to_string),
        }
    }

    #[test]
    fn device_match_prefers_active_connection() {
        // The device already running this profile wins, even if another device's
        // interface name would also match.
        let conn = ethernet_conn(Some("LAN"), Some("enp3s0"));
        let devices = vec![
            eth_device("/dev/0", "enp3s0", None),
            eth_device("/dev/1", "enp4s0", Some("/settings/7")),
        ];
        let dev = device_for_wired_profile(&conn, "/settings/7", &devices)
            .expect("active-connection match");
        assert_eq!(dev.path, "/dev/1");
    }

    #[test]
    fn device_match_by_interface_name() {
        let conn = ethernet_conn(Some("LAN"), Some("enp4s0"));
        let devices = vec![
            eth_device("/dev/0", "enp3s0", None),
            eth_device("/dev/1", "enp4s0", None),
        ];
        let dev =
            device_for_wired_profile(&conn, "/settings/7", &devices).expect("interface-name match");
        assert_eq!(dev.path, "/dev/1");
    }

    #[test]
    fn device_match_falls_back_to_sole_device() {
        // Unpinned profile (no interface-name) on a single-NIC machine binds to
        // that one ethernet device.
        let conn = ethernet_conn(Some("LAN"), None);
        let devices = vec![eth_device("/dev/0", "enp3s0", None)];
        let dev =
            device_for_wired_profile(&conn, "/settings/7", &devices).expect("sole-device fallback");
        assert_eq!(dev.path, "/dev/0");
    }

    #[test]
    fn device_match_none_when_ambiguous() {
        // Unpinned profile with multiple NICs and no active match → ambiguous.
        let conn = ethernet_conn(Some("LAN"), None);
        let devices = vec![
            eth_device("/dev/0", "enp3s0", None),
            eth_device("/dev/1", "enp4s0", None),
        ];
        assert!(device_for_wired_profile(&conn, "/settings/7", &devices).is_none());
    }

    // -- prop_object_path -----------------------------------------------------

    #[test]
    fn object_path_slash_sentinel_is_none() {
        let mut props = HashMap::new();
        props.insert(
            "Connection".to_string(),
            val(OwnedObjectPath::try_from("/").expect("valid path")),
        );
        assert_eq!(prop_object_path(&props, "Connection"), None);
    }

    #[test]
    fn object_path_real_path_read() {
        let mut props = HashMap::new();
        props.insert(
            "Connection".to_string(),
            val(OwnedObjectPath::try_from("/org/fd/NM/Settings/3").expect("valid path")),
        );
        assert_eq!(
            prop_object_path(&props, "Connection").as_deref(),
            Some("/org/fd/NM/Settings/3"),
        );
    }
}
