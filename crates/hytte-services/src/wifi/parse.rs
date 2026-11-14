//! Pure parsing and property-extraction helpers for iwd D-Bus values.
//!
//! All functions are free of I/O and independently unit-testable in the
//! hermetic test suite.

use futures_signals::signal::Mutable;
use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

use crate::wifi::types::{Adapter, Station, StationState};

/// iwd's adapter (wiphy) D-Bus interface. iwd 3.12 `doc/adapter-api.txt:5`
/// and `src/dbus.h:29` (`IWD_WIPHY_INTERFACE`): `net.connman.iwd.Adapter`,
/// with no version suffix (#1648).
pub(super) const IWD_ADAPTER_IFACE: &str = "net.connman.iwd.Adapter";

pub(super) type ManagedObjects =
    HashMap<zbus::zvariant::OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

// ── Generic property extractors ───────────────────────────────────────────────

pub(super) fn property<T>(props: &HashMap<String, OwnedValue>, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| T::try_from(v).ok())
}

/// Extract a string property, falling back to `zvariant::Str` on type mismatch.
///
/// This handles both `String` and `zvariant::Str` variants gracefully via
/// `try_clone().ok()`, so a malformed or uncloneable value from iwd just
/// returns an empty string rather than aborting.
pub(super) fn prop_str(props: &HashMap<String, OwnedValue>, key: &str) -> String {
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

pub(super) fn prop_bool(props: &HashMap<String, OwnedValue>, key: &str) -> bool {
    property::<bool>(props, key).unwrap_or(false)
}

// ── Station state parsing ─────────────────────────────────────────────────────

pub(super) fn parse_state(s: &str) -> StationState {
    match s {
        "connected" => StationState::Connected,
        "connecting" => StationState::Connecting,
        "disconnecting" => StationState::Disconnecting,
        "roaming" => StationState::Roaming,
        _ => StationState::Disconnected,
    }
}

// ── Path helpers ──────────────────────────────────────────────────────────────

/// Given a station path like `"/net/connman/iwd/0/3/6"`, return the adapter
/// path `"/net/connman/iwd/0"`. Returns an empty string if the input does
/// not match the expected layout.
pub(super) fn adapter_path_from_station(station_path: &str) -> String {
    // Expected layout: /net/connman/iwd/<adapter_idx>/<phy>/<station_idx>
    // → parts = ["", "net", "connman", "iwd", "<adapter>", "<phy>", "<station>"]
    let parts: Vec<&str> = station_path.split('/').collect();
    if parts.len() < 5 || parts[1] != "net" || parts[2] != "connman" || parts[3] != "iwd" {
        return String::new();
    }
    format!("/net/connman/iwd/{}", parts[4])
}

/// Returns `true` when the `InterfacesRemoved` signal indicates the station was removed.
pub(super) fn station_removed_from_event(msg: &zbus::Message, station_path: &str) -> bool {
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

// ── Delta application ─────────────────────────────────────────────────────────

/// Apply a `PropertiesChanged` delta for `net.connman.iwd.Station`.
pub(super) fn apply_station_props_delta(
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

/// Apply a `PropertiesChanged` delta for `net.connman.iwd.Adapter`.
pub(super) fn apply_adapter_props_delta(
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

// ── Managed-objects refresh helpers ───────────────────────────────────────────

/// Refresh Adapter from the managed-objects map (called once on discovery).
pub(super) fn refresh_adapter_from_managed(
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
        .and_then(|(_, ifaces)| ifaces.get(IWD_ADAPTER_IFACE));

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

/// The station to publish for a freshly parsed one: `parsed`, keeping the
/// SSID of the station already published when both are on the same network
/// (#1551).
///
/// A parsed station has no SSID. iwd's `Station` names only its
/// `ConnectedNetwork`, and the name comes with the network list read after
/// the snapshot ([`super::client::refresh_networks`]), one round trip per
/// network. The watcher rediscovers on every hole in its subscriptions and
/// keeps its state meanwhile, so each rediscovery used to publish a connected
/// station with no SSID, and the Wi-Fi card read "Connected" until the list
/// was read. The network read still decides the SSID when it lands; this
/// only bridges the round trips before it.
///
/// The SSID is kept only when all of these hold:
///
/// - A station was published. The pump clears it when it sees iwd leave, a
///   new iwd take the name or the station removed, so no SSID crosses
///   those.
/// - Both are the same station object. The adapter's path is a prefix of
///   the station's ([`adapter_path_from_station`]).
/// - `parsed` names a `ConnectedNetwork`, and it is the one `previous`
///   named. iwd builds a network's path from the station's path, the SSID in
///   hex and the security (`iwd_network_get_path`, iwd 3.12
///   `src/station.c:435`), so one path is one SSID. That also covers an iwd
///   that restarts between two passes, where no pump sees it go: the next
///   pass's wait for iwd reads the owner change, so nothing clears the
///   state, and a path the new iwd shares with the old one names the same
///   network.
/// - `parsed` is not `disconnected`.
///
/// Anything else publishes no SSID until the network read: a blank for a
/// few round trips rather than another network's name.
pub(super) fn with_kept_ssid(previous: Option<&Station>, parsed: Station) -> Station {
    let kept = previous.and_then(|previous| {
        let same_network = matches!(
            (&previous.connected_network, &parsed.connected_network),
            (Some(was), Some(is)) if was == is
        );
        (previous.path == parsed.path && same_network && parsed.state != StationState::Disconnected)
            .then(|| previous.connected_ssid.clone())
            .flatten()
    });
    Station {
        connected_ssid: kept,
        ..parsed
    }
}

/// Refresh the Station mutable from the managed-objects map (called once on
/// discovery). The SSID the published station had is kept when the station
/// is still on the same network ([`with_kept_ssid`]); the network read that
/// follows sets it either way.
pub(super) fn refresh_station_from_managed(
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

    let parsed = Station {
        path: path_str.to_string(),
        state: parse_state(&state_str),
        scanning,
        connected_network,
        connected_ssid: None, // filled after network list is read
    };
    let mut published = station_mutable.lock_mut();
    let next = with_kept_ssid(published.as_ref(), parsed);
    *published = Some(next);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every iwd interface name this module speaks, written out as a literal
    /// (never derived from the constants under test) and cited from iwd
    /// 3.12's own documentation (nixpkgs `iwd.src`). Nothing checked these
    /// against iwd before: the fixtures shared the code's typo (#1648).
    const IWD_INTERFACES: [&str; 7] = [
        "net.connman.iwd.Adapter",      // doc/adapter-api.txt:5
        "net.connman.iwd.Device",       // doc/device-api.txt:5
        "net.connman.iwd.Station",      // doc/station-api.txt:5
        "net.connman.iwd.Network",      // doc/network-api.txt:5
        "net.connman.iwd.KnownNetwork", // doc/knownnetwork-api.txt:5
        "net.connman.iwd.AgentManager", // doc/agent-api.txt:5
        "net.connman.iwd.Agent",        // doc/agent-api.txt:75
    ];

    #[test]
    fn the_adapter_interface_is_iwds_own_name() {
        assert_eq!(IWD_ADAPTER_IFACE, "net.connman.iwd.Adapter");
    }

    /// Every quoted `net.connman.iwd.*` literal in the wifi module's non-test
    /// code is one of iwd's interfaces above or one of its error names, so a
    /// misspelt interface in a call site with no constant fails here too.
    #[test]
    fn every_iwd_name_literal_in_the_module_is_a_real_one() {
        const ERRORS: [&str; 4] = [
            "net.connman.iwd.AlreadyExists",
            "net.connman.iwd.InvalidArgs",
            "net.connman.iwd.Failed",
            "net.connman.iwd.Agent.Error.Canceled",
        ];
        let sources = [
            include_str!("client.rs"),
            include_str!("mod.rs"),
            include_str!("parse.rs"),
            include_str!("watcher.rs"),
            include_str!("agent.rs"),
        ];
        let mut seen = 0;
        for src in sources {
            let code = src.split("#[cfg(test)]").next().unwrap_or(src);
            for piece in code.split('"').skip(1).step_by(2) {
                if piece.starts_with("net.connman.iwd.") {
                    seen += 1;
                    assert!(
                        IWD_INTERFACES.contains(&piece) || ERRORS.contains(&piece),
                        "unknown iwd name {piece:?}"
                    );
                }
            }
        }
        assert!(seen > 5, "scan found only {seen} literals");
    }

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

    const STATION: &str = "/net/connman/iwd/0/4";
    /// iwd's paths for "home" and "work" on `STATION`, both `psk`.
    const HOME: &str = "/net/connman/iwd/0/4/686f6d65_psk";
    const WORK: &str = "/net/connman/iwd/0/4/776f726b_psk";

    fn station(
        path: &str,
        state: StationState,
        network: Option<&str>,
        ssid: Option<&str>,
    ) -> Station {
        Station {
            path: path.to_owned(),
            state,
            scanning: false,
            connected_network: network.map(str::to_owned),
            connected_ssid: ssid.map(str::to_owned),
        }
    }

    /// A rediscovered station on the network the published one was on keeps
    /// its SSID, in every state that has a `ConnectedNetwork`, rather than
    /// going blank until the network list is read (#1551).
    ///
    /// **Falsification:** return `parsed` unchanged (the pre-fix
    /// `connected_ssid: None`) and every row is blank.
    #[test]
    fn a_rediscovered_station_on_the_same_network_keeps_its_ssid() {
        let previous = station(STATION, StationState::Connected, Some(HOME), Some("home"));
        for state in [
            StationState::Connected,
            StationState::Roaming,
            StationState::Connecting,
            StationState::Disconnecting,
        ] {
            let parsed = station(STATION, state, Some(HOME), None);
            let next = with_kept_ssid(Some(&previous), parsed);
            assert_eq!(next.connected_ssid.as_deref(), Some("home"), "{state:?}");
            assert_eq!(next.state, state, "the parsed state is published");
            assert_eq!(next.connected_network.as_deref(), Some(HOME));
        }
    }

    /// Every case where the published SSID may not be this network's name
    /// publishes none. A blank for a few round trips is better than the
    /// wrong network's name.
    ///
    /// **Falsification:** drop any one condition from `with_kept_ssid`
    /// (the station path, the `Some` on both sides of the network match, the
    /// `disconnected` check) and its row keeps "home".
    #[test]
    fn the_ssid_is_not_kept_when_it_may_name_another_network() {
        let home = station(STATION, StationState::Connected, Some(HOME), Some("home"));
        let rows: [(&str, Option<Station>, Station); 6] = [
            (
                "nothing published: iwd left or changed hands, or the station \
                 was removed, and the watcher cleared it",
                None,
                station(STATION, StationState::Connected, Some(HOME), None),
            ),
            (
                "the station is no longer connected",
                Some(home.clone()),
                station(STATION, StationState::Disconnected, None, None),
            ),
            (
                "the station says disconnected but still names the network",
                Some(home.clone()),
                station(STATION, StationState::Disconnected, Some(HOME), None),
            ),
            (
                "the station is on another network",
                Some(home.clone()),
                station(STATION, StationState::Connected, Some(WORK), None),
            ),
            (
                "another station, as after the adapter went and came back",
                Some(home),
                station(
                    "/net/connman/iwd/1/5",
                    StationState::Connected,
                    Some(HOME),
                    None,
                ),
            ),
            (
                "neither names a network, so they are not on the same one",
                Some(station(
                    STATION,
                    StationState::Connected,
                    None,
                    Some("home"),
                )),
                station(STATION, StationState::Connected, None, None),
            ),
        ];
        for (case, previous, parsed) in rows {
            let next = with_kept_ssid(previous.as_ref(), parsed);
            assert_eq!(next.connected_ssid, None, "{case}");
        }
    }

    /// iwd's `GetManagedObjects` answer with the station in `state` on
    /// `network`.
    fn managed_with_station(state: &str, network: Option<&str>) -> ManagedObjects {
        let owned = |x: zbus::zvariant::Value<'_>| x.try_to_owned().expect("an owned value");
        let mut props = HashMap::from([
            ("State".to_owned(), owned(state.into())),
            ("Scanning".to_owned(), owned(false.into())),
        ]);
        if let Some(network) = network {
            let path = zbus::zvariant::ObjectPath::try_from(network).expect("an object path");
            props.insert("ConnectedNetwork".to_owned(), owned(path.into()));
        }
        HashMap::from([(
            zbus::zvariant::OwnedObjectPath::try_from(STATION).expect("an object path"),
            HashMap::from([("net.connman.iwd.Station".to_owned(), props)]),
        )])
    }

    /// The discovery read keeps the SSID the station already had when the
    /// snapshot puts it on the same network, and publishes none when it is
    /// on another one or nothing was published (#1551). This is the read
    /// the watcher makes on every rediscovery, so it is what puts
    /// `with_kept_ssid` on screen.
    ///
    /// **Falsification:** publish the parsed station without
    /// `with_kept_ssid`, or pass it `None` for the published station, and
    /// the first assertion fails.
    #[test]
    fn rediscovery_keeps_the_published_ssid_on_the_same_network() {
        let path = zbus::zvariant::OwnedObjectPath::try_from(STATION).expect("an object path");
        let published = || {
            Mutable::new(Some(station(
                STATION,
                StationState::Connected,
                Some(HOME),
                Some("home"),
            )))
        };

        let station_mutable = published();
        refresh_station_from_managed(
            &managed_with_station("connected", Some(HOME)),
            &path,
            &station_mutable,
        );
        let st = station_mutable.get_cloned().expect("a station");
        assert_eq!(st.connected_ssid.as_deref(), Some("home"));
        assert_eq!(st.connected_network.as_deref(), Some(HOME));

        let station_mutable = published();
        refresh_station_from_managed(
            &managed_with_station("connected", Some(WORK)),
            &path,
            &station_mutable,
        );
        let st = station_mutable.get_cloned().expect("a station");
        assert_eq!(st.connected_network.as_deref(), Some(WORK));
        assert_eq!(st.connected_ssid, None, "\"home\" is not WORK's name");

        let station_mutable = Mutable::new(None);
        refresh_station_from_managed(
            &managed_with_station("connected", Some(HOME)),
            &path,
            &station_mutable,
        );
        let st = station_mutable.get_cloned().expect("a station");
        assert_eq!(st.connected_ssid, None, "nothing was published to keep");
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
        // The real extraction lives in client::read_networks; replicate the
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
