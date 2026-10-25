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
    hytte_bus::call(BusKind::System, "net.connman.iwd")
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
        match hytte_bus::call(BusKind::System, "net.connman.iwd")
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
        let props: HashMap<String, OwnedValue> = match hytte_bus::call(
            BusKind::System,
            "net.connman.iwd",
        )
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
            // iwd's Network objects are per-SSID by construction — it has
            // already collapsed a mesh for us, so there is never a group to
            // count and the panel never shows the badge on this backend.
            ap_count: 1,
            // Same reason: with one row per SSID there is no group max to
            // diverge from, so the associated AP's signal is just this row's
            // own — but the connected readout reads this field (#874), so it
            // still has to be populated rather than left `None`.
            active_signal_dbm: connected.then_some(signal_dbm),
        });
    }

    networks
}

/// Read the Name and Type properties from a net.connman.iwd.Network object.
/// Falls back to the last path segment for the SSID on any error.
pub(super) async fn read_network_metadata(path: &str) -> (String, String) {
    match hytte_bus::call(BusKind::System, "net.connman.iwd")
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
    let props: HashMap<String, OwnedValue> =
        match hytte_bus::call(BusKind::System, "net.connman.iwd")
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

/// How long iwd gives our agent to answer one `RequestPassphrase` before it
/// cancels it: `agent_timeout_input_request()` in iwd's `src/agent.c`
/// (120 s). Copied from the C source for the test that holds
/// [`NETWORK_CONNECT_TIMEOUT`] above it.
#[cfg(test)]
const IWD_AGENT_INPUT_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(2);

/// Deadline for iwd's `Network.Connect`.
///
/// `Connect` blocks until the connection has finished or failed. That
/// includes the passphrase prompt our agent raises: iwd waits two minutes
/// for the user there. Then, with iwd's own network configuration on, it
/// includes DHCP, which iwd documents as "roughly 30 seconds per BSS"
/// (`doc/network-api.txt`). `hytte_bus::call`'s 25 s default is shorter than
/// the prompt alone. With it, a user still typing the passphrase got a
/// Critical "Wi-Fi connection failed: call timeout" while iwd went on
/// connecting with the passphrase they then submitted.
///
/// iwd bounds every one of those steps itself, and `iwctl` puts no deadline
/// on `Connect` at all (`ell`'s D-Bus client has none). Five minutes is a
/// backstop for an iwd that has stopped answering: a whole prompt plus
/// several BSSes' worth of DHCP. An iwd that exits does not reach it: the
/// bus answers the pending call with `NoReply` as soon as iwd disconnects.
const NETWORK_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(5);

/// The deadline an iwd `Network` method gets, or `None` for
/// `hytte_bus::call`'s default. Only `Connect` waits on the user. See
/// [`NETWORK_CONNECT_TIMEOUT`].
pub(super) fn network_call_deadline(method: &str) -> Option<std::time::Duration> {
    (method == "Connect").then_some(NETWORK_CONNECT_TIMEOUT)
}

/// Register our agent object with iwd's `AgentManager`.
pub(super) async fn register_iwd_agent(agent_path: &str) -> Result<(), hytte_bus::BusError> {
    let agent_obj_path = zbus::zvariant::ObjectPath::try_from(agent_path)
        .map_err(|e| hytte_bus::BusError::Permanent {
            reason: format!("invalid agent path: {e}"),
            dbus_name: None,
        })?
        .to_owned();

    hytte_bus::call(BusKind::System, "net.connman.iwd")
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

#[cfg(test)]
mod tests {
    use super::{IWD_AGENT_INPUT_TIMEOUT, network_call_deadline};

    /// `Network.Connect` blocks while the user types the passphrase on our
    /// agent's prompt, and iwd waits two minutes for that. A deadline shorter
    /// than that fires while the prompt is still open: a Critical "Wi-Fi
    /// connection failed" toast for a connection that then succeeds.
    ///
    /// **Falsification:** make `network_call_deadline` return `None` for
    /// `Connect` (the 25 s default) and this fails.
    #[test]
    fn connect_outlasts_a_passphrase_prompt() {
        let deadline = network_call_deadline("Connect");
        assert!(
            deadline.is_some_and(|d| d > IWD_AGENT_INPUT_TIMEOUT),
            "Connect must wait out a whole passphrase prompt, got {deadline:?}"
        );
    }
}
