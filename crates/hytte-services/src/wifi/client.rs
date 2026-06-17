//! iwd D-Bus client helpers: `ObjectManager`, network reading, refresh helpers,
//! and agent registration.

use futures_signals::signal::Mutable;
use hytte_bus::BusKind;
use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

use super::parse::{ManagedObjects, parse_state, prop_bool, prop_str};
use super::types::{Station, WifiNetwork};

// ── ObjectManager ─────────────────────────────────────────────────────────────

pub(super) async fn get_managed_objects() -> Result<ManagedObjects, hytte_bus::BusError> {
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
pub(super) async fn read_networks(
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

/// Read the Name and Type properties from a net.connman.iwd.Network object.
/// Falls back to the last path segment for the SSID on any error.
pub(super) async fn read_network_metadata(path: &str) -> (String, String) {
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

/// Re-read Station properties via Properties.GetAll and update the mutable.
pub(super) async fn refresh_station(
    station_path: &str,
    station_mutable: &Mutable<Option<Station>>,
) {
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
pub(super) async fn refresh_networks(
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
pub(super) async fn register_iwd_agent(agent_path: &str) -> Result<(), hytte_bus::BusError> {
    let agent_obj_path = zbus::zvariant::ObjectPath::try_from(agent_path)
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
        .args((agent_obj_path,))
        .send::<()>()
        .await
}

// ── Discovery ─────────────────────────────────────────────────────────────────

pub(super) async fn discover_station() -> Option<(ManagedObjects, zbus::zvariant::OwnedObjectPath)>
{
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
