//! iwd D-Bus client helpers: `ObjectManager`, network reading, refresh helpers,
//! and agent registration.

use futures_signals::signal::Mutable;
use hytte_bus::BusKind;
use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

use super::parse::{
    Discovered, IWD_ADAPTER_IFACE, ManagedObjects, adapter_from_props, discovered, parse_state,
    prop_bool, prop_str, with_kept_ssid,
};
use super::types::{Adapter, Station, WifiNetwork};

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

    let parsed = Station {
        path: station_path.to_string(),
        state: parse_state(&state_str),
        scanning,
        connected_network,
        connected_ssid: None, // kept by `with_kept_ssid` below; the network read sets it
    };
    // Discovery's rule (#1654): the SSID survives only a read that finds the
    // station on the same network. This used to keep it whatever the
    // network, so a re-read that found another one named the old network.
    let mut guard = station_mutable.lock_mut();
    let next = with_kept_ssid(guard.as_ref(), parsed);
    *guard = Some(next);
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
///
/// Sent with `no_auto_start()` (#1657 review). The watcher's pump polls the
/// registration beside its event loop ([`super::watcher`]'s
/// `beside_registration`), so a retry on the ramp can go out while a handler
/// awaits (a re-read, a network read, a removal's pause) with iwd's `Lost`
/// queued unread behind it. A plain call to iwd's name with no owner
/// D-Bus-activates the iwd that was just stopped (#1551, #1652). With the
/// flag the bus answers [`is_no_owner`] instead, which ends the registration
/// (`register_agent_until_accepted`). The registration never has to start
/// iwd: it runs only after a snapshot iwd answered.
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
        .no_auto_start()
        .args((agent_obj_path,))
        .send::<()>()
        .await
}

/// Is this iwd's answer that our connection already has an agent registered?
///
/// iwd keys agents by the caller's unique name and refuses a second
/// `RegisterAgent` from the same one with `net.connman.iwd.AlreadyExists`
/// (`agent_register` in iwd 3.12's `src/agent.c`). The watcher re-registers
/// after any hole in its owner subscription, and when iwd itself is fine that
/// is the answer it gets. For the watcher it means "registered", not a
/// failure to retry.
pub(super) fn is_already_registered(e: &hytte_bus::BusError) -> bool {
    matches!(
        e,
        hytte_bus::BusError::Permanent { dbus_name: Some(name), .. }
            if name == "net.connman.iwd.AlreadyExists"
    )
}

/// Is this the bus's answer that nothing owns iwd's name, to a call sent
/// `no_auto_start()`? `NameHasNoOwner` is what `dbus-daemon` and `dbus-broker`
/// both answer; `ServiceUnknown` is accepted as the same answer from a bus
/// that words it so, as `bluetooth`'s device fold does (#1651 review). Either
/// is the bus's own and says nothing about iwd's objects.
pub(super) fn is_no_owner(e: &hytte_bus::BusError) -> bool {
    matches!(
        e,
        hytte_bus::BusError::Permanent { dbus_name: Some(name), .. }
            if matches!(
                name.as_str(),
                "org.freedesktop.DBus.Error.NameHasNoOwner"
                    | "org.freedesktop.DBus.Error.ServiceUnknown"
            )
    )
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Read iwd's object tree and find its `Station` and its `Adapter`
/// ([`super::parse::discovered`]). An answer with no station still names the
/// adapter, which is what an adapter that is powered off looks like (#1656).
/// An error when iwd did not answer. The watcher publishes what an answer
/// holds and clears what it lacks; on an error it keeps what it published
/// (#1654).
///
/// This **calls `net.connman.iwd`**, and iwd is D-Bus-activatable
/// (`SystemdService=iwd.service` in its activation file), so on a bus where
/// iwd has no owner this call starts it. The watcher therefore asks
/// [`iwd_has_owner`] first on every discovery but its first (#1551), and
/// paces a retry itself. This used to sleep 5 s and return, and the watcher
/// called it again, so an iwd that was stopped cleanly (its station removed,
/// its name released) was restarted by the next attempt.
///
/// `station` and `adapter` are what the watcher publishes, read before the
/// call: a snapshot keeps publishing that station while iwd still has it
/// ([`super::parse::station_in`], #1659), and a snapshot with no station that
/// adapter ([`super::parse::adapter_in`]).
pub(super) async fn discover_station(
    station: &Mutable<Option<Station>>,
    adapter: &Mutable<Option<Adapter>>,
) -> Result<Discovered, hytte_bus::BusError> {
    let kept_station = station.lock_ref().as_ref().map(|st| st.path.clone());
    let kept_adapter = adapter.lock_ref().as_ref().map(|a| a.path.clone());
    let managed = get_managed_objects().await.inspect_err(|e| {
        tracing::debug!(error = %e, "iwd GetManagedObjects failed (iwd not running?)");
    })?;
    let found = discovered(managed, kept_station.as_deref(), kept_adapter.as_deref());
    if found.station.is_none() {
        tracing::debug!(
            adapter = ?found.adapter,
            "iwd has no Station object (adapter powered off, or none)"
        );
    }
    Ok(found)
}

/// Read the adapter at `adapter_path` again, live, and publish it when it
/// differs from what is published (#1656). The watcher sends this once the
/// bus has the adapter's `PropertiesChanged` rule, the station re-read's
/// discipline (#1551), and when an adapter signal does not decode.
///
/// Sent with `no_auto_start()`: the watcher sends it on its own, and a call
/// to an iwd that has just exited must not D-Bus-activate it (#1551, #1652).
/// The bus then answers `NameHasNoOwner`, which says nothing about the
/// adapter: the state is kept, and iwd's `NameOwnerChanged` clears it.
pub(super) async fn refresh_adapter(
    adapter_path: &str,
    adapter_mutable: &Mutable<Option<Adapter>>,
) {
    let props: HashMap<String, OwnedValue> =
        match hytte_bus::call(BusKind::System, "net.connman.iwd")
            .at_path(adapter_path.to_string())
            .iface("org.freedesktop.DBus.Properties")
            .method("GetAll")
            .no_auto_start()
            .args((IWD_ADAPTER_IFACE,))
            .send::<HashMap<String, OwnedValue>>()
            .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(error = %e, path = adapter_path, "wifi: adapter GetAll failed");
                return;
            }
        };
    adapter_mutable.set_neq(Some(adapter_from_props(adapter_path, &props)));
}

/// Ask the bus whether anyone owns iwd's name. The bus answers this itself,
/// so unlike any call to `net.connman.iwd` it starts nothing.
pub(super) async fn iwd_has_owner() -> Result<bool, hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("NameHasOwner")
        .args((crate::wifi_backend::IWD_BUS_NAME.to_string(),))
        .send::<bool>()
        .await
}

#[cfg(test)]
mod tests {
    use super::{
        IWD_AGENT_INPUT_TIMEOUT, is_already_registered, is_no_owner, network_call_deadline,
    };
    use hytte_bus::BusError;

    /// Only the bus's own no-owner answers read as "iwd has no owner"; iwd's
    /// refusals, and a failure with no D-Bus error name, do not (#1657
    /// review).
    ///
    /// **Falsification:** match any `org.freedesktop.DBus.Error.*` and the
    /// `NoReply` row reads as no owner.
    #[test]
    fn only_the_buss_no_owner_answers_read_as_no_owner() {
        let named = |name: &str| BusError::Permanent {
            reason: "x".to_owned(),
            dbus_name: Some(name.to_owned()),
        };
        for name in [
            "org.freedesktop.DBus.Error.NameHasNoOwner",
            "org.freedesktop.DBus.Error.ServiceUnknown",
        ] {
            assert!(is_no_owner(&named(name)), "{name}");
        }
        for name in [
            "org.freedesktop.DBus.Error.NoReply",
            "org.freedesktop.DBus.Error.UnknownObject",
            "net.connman.iwd.AlreadyExists",
        ] {
            assert!(!is_no_owner(&named(name)), "{name}");
        }
        assert!(!is_no_owner(&BusError::Permanent {
            reason: "org.freedesktop.DBus.Error.NameHasNoOwner".to_owned(),
            dbus_name: None,
        }));
    }

    /// Only iwd's own `AlreadyExists` reads as "registered". Any other
    /// refusal, and a failure with no D-Bus error name, is still a failure to
    /// retry.
    ///
    /// **Falsification:** match any `net.connman.iwd.*` error and the
    /// `InvalidArgs` row passes as registered.
    #[test]
    fn only_already_exists_reads_as_registered() {
        let named = |name: &str| BusError::Permanent {
            reason: "x".to_owned(),
            dbus_name: Some(name.to_owned()),
        };
        assert!(is_already_registered(&named(
            "net.connman.iwd.AlreadyExists"
        )));
        for name in [
            "net.connman.iwd.InvalidArgs",
            "net.connman.iwd.Failed",
            "org.freedesktop.DBus.Error.UnknownObject",
            "org.bluez.Error.AlreadyExists",
        ] {
            assert!(!is_already_registered(&named(name)), "{name}");
        }
        assert!(!is_already_registered(&BusError::Permanent {
            reason: "net.connman.iwd.AlreadyExists".to_owned(),
            dbus_name: None,
        }));
    }

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
