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

/// iwd's station D-Bus interface: iwd 3.12 `doc/station-api.txt:5`.
pub(super) const IWD_STATION_IFACE: &str = "net.connman.iwd.Station";

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

/// What one `InterfacesRemoved` takes away from what the watcher publishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Removed {
    /// The published adapter's object: its `net.connman.iwd.Adapter` at its
    /// path. iwd removes it only with the wiphy itself (`wiphy_destroy`, iwd
    /// 3.12 `src/wiphy.c:2478-2490`), and every netdev on that wiphy, the
    /// station's included, goes with it.
    Adapter,
    /// The published station's `net.connman.iwd.Station` at its path, and
    /// nothing at the adapter's. Powering the adapter off does this: the
    /// soft rfkill takes the netdev down and iwd removes the station's
    /// interface (`station_netdev_watch`, `src/station.c:5941-5945`), while
    /// the adapter's object stays (#1656).
    Station,
    /// Anything else: a network, a BSS, a known network, another station.
    Other,
}

/// Classify an `InterfacesRemoved` against the published station and adapter
/// paths, `None` for one that is not published. A body that does not decode
/// is [`Removed::Other`].
pub(super) fn removed_from_event(
    msg: &zbus::Message,
    station_path: Option<&str>,
    adapter_path: Option<&str>,
) -> Removed {
    let Ok((path, removed_ifaces)) = msg
        .body()
        .deserialize::<(zbus::zvariant::OwnedObjectPath, Vec<String>)>()
    else {
        return Removed::Other;
    };
    let removes = |at: Option<&str>, iface: &str| {
        at == Some(path.as_str()) && removed_ifaces.iter().any(|i| i == iface)
    };
    if removes(adapter_path, IWD_ADAPTER_IFACE) {
        Removed::Adapter
    } else if removes(station_path, IWD_STATION_IFACE) {
        Removed::Station
    } else {
        Removed::Other
    }
}

/// Whether an `InterfacesAdded` may bring a station or an adapter: it adds
/// `net.connman.iwd.Station` or `net.connman.iwd.Adapter` at any path, or its
/// body does not decode, so only a snapshot can say. Powering the adapter on
/// brings the netdev up and iwd adds the station's interface back
/// (`station_netdev_watch`'s `NETDEV_WATCH_EVENT_UP`, `src/station.c:5936-5939`).
pub(super) fn adds_station_or_adapter(msg: &zbus::Message) -> bool {
    type Added = (
        zbus::zvariant::OwnedObjectPath,
        HashMap<String, HashMap<String, OwnedValue>>,
    );
    msg.body()
        .deserialize::<Added>()
        .map_or(true, |(_, ifaces)| {
            ifaces.contains_key(IWD_STATION_IFACE) || ifaces.contains_key(IWD_ADAPTER_IFACE)
        })
}

// ── Delta application ─────────────────────────────────────────────────────────

/// Apply a `PropertiesChanged` delta for `net.connman.iwd.Station`: the
/// signal's changed dict, then its invalidated list.
///
/// iwd reports a disconnect's `ConnectedNetwork` in the **invalidated** list,
/// not as a changed value (#1654): `station_reset_connection_state` marks it
/// changed once the network is gone (iwd 3.12 `src/station.c:1988-1991`), its
/// getter then answers nothing (`station_property_get_connected_network`,
/// `:4709-4722`), and ell lists a property whose getter answers nothing as
/// invalidated (`ell/dbus-service.c:985-994`). Reading only the dict kept the
/// old network after a disconnect.
///
/// The SSID follows [`with_kept_ssid`]'s rule, the one discovery uses: kept
/// while the station stays on the same network, blank otherwise until the
/// network read the caller makes next names the new one.
pub(super) fn apply_station_props_delta(
    changed: &HashMap<String, OwnedValue>,
    invalidated: &[String],
    station_mutable: &Mutable<Option<Station>>,
) {
    let mut guard = station_mutable.lock_mut();
    let Some(previous) = guard.as_ref() else {
        return;
    };
    let mut st = previous.clone();

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
    // `ConnectedNetwork` is the only property read here that iwd can
    // invalidate. Its `State` and `Scanning` getters always answer
    // (`src/station.c:4742-4796`), so ell never lists them.
    if invalidated.iter().any(|key| key == "ConnectedNetwork") {
        st.connected_network = None;
    }

    let next = with_kept_ssid(Some(previous), st);
    *guard = Some(next);
}

/// Apply a `PropertiesChanged` delta for `net.connman.iwd.Adapter`. It reads
/// the changed dict only: the adapter's `Powered` and `Name` getters always
/// answer (iwd 3.12 `src/wiphy.c:2522-2533`, `:2594-2617`), so iwd never
/// invalidates either.
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

/// The adapter at `path`, from its `net.connman.iwd.Adapter` properties: a
/// snapshot's, or a re-read's ([`super::client::refresh_adapter`]).
pub(super) fn adapter_from_props(path: &str, props: &HashMap<String, OwnedValue>) -> Adapter {
    Adapter {
        path: path.to_owned(),
        powered: prop_bool(props, "Powered"),
        name: prop_str(props, "Name"),
    }
}

/// What one discovery snapshot holds for the watcher to publish.
pub(super) struct Discovered {
    /// iwd's whole object tree, for the station's own properties.
    pub(super) managed: ManagedObjects,
    /// The station ([`station_in`]), `None` while iwd has none: the adapter
    /// is powered off, or iwd has no netdev up at all.
    pub(super) station: Option<zbus::zvariant::OwnedObjectPath>,
    /// The adapter ([`adapter_in`]), `None` when iwd has none to publish.
    pub(super) adapter: Option<Adapter>,
}

/// Read a `GetManagedObjects` answer into what the watcher publishes.
/// `kept_station` and `kept_adapter` are the paths of the station and the
/// adapter the watcher publishes now, if any ([`station_in`],
/// [`adapter_in`]).
pub(super) fn discovered(
    managed: ManagedObjects,
    kept_station: Option<&str>,
    kept_adapter: Option<&str>,
) -> Discovered {
    let station = station_in(&managed, kept_station);
    let adapter = adapter_in(&managed, station.as_ref().map(|p| p.as_str()), kept_adapter);
    Discovered {
        managed,
        station,
        adapter,
    }
}

/// The station a snapshot names: `kept`, the station the watcher publishes
/// now, while the snapshot still has a `net.connman.iwd.Station` at its path,
/// else the object carrying one with the lowest path (byte order), so the
/// choice does not move from pass to pass: [`adapter_in`]'s rule without a
/// station (#1659). Until #1659 this took the first station in the answer's
/// order, which is the map's, so with two Wi-Fi interfaces the published
/// station, and its adapter with it, could change from one pass to the next.
pub(super) fn station_in(
    managed: &ManagedObjects,
    kept: Option<&str>,
) -> Option<zbus::zvariant::OwnedObjectPath> {
    let stations = managed
        .iter()
        .filter(|(_, ifaces)| ifaces.contains_key(IWD_STATION_IFACE))
        .map(|(path, _)| path);
    stations
        .clone()
        .find(|path| Some(path.as_str()) == kept)
        .or_else(|| stations.min_by(|a, b| a.as_str().cmp(b.as_str())))
        .cloned()
}

/// The adapter to publish for a snapshot whose station is `station`.
///
/// - **With a station**: the adapter at the station's parent path
///   ([`adapter_path_from_station`]), the rule discovery has always used, or
///   none if that object has no `net.connman.iwd.Adapter`.
/// - **Without one**: `kept`, the adapter the watcher publishes now, while
///   the snapshot still has a `net.connman.iwd.Adapter` at its path (#1657
///   review). Switching the radio off removes the station, and the pass that
///   follows has none: with two adapters it must go on publishing the one
///   the panel's switch just turned off, so the switch turns that one back
///   on. Otherwise the object carrying `net.connman.iwd.Adapter` with the
///   lowest path (byte order), so the choice does not move from pass to pass.
///   That fallback is a guess on a machine with two adapters and no station:
///   a shell that starts with both off, or a pass after iwd left (which
///   clears the adapter), publishes the lowest. Until #1656 a snapshot with
///   no station published no adapter at all. iwd keeps the adapter's object
///   while the adapter is powered off (it removes it only with the wiphy,
///   `wiphy_destroy`, iwd 3.12 `src/wiphy.c:2478-2490`), but not the station,
///   so the panel read "No adapter" and its power switch went insensitive:
///   Wi-Fi could be switched off from the panel and never on again.
pub(super) fn adapter_in(
    managed: &ManagedObjects,
    station: Option<&str>,
    kept: Option<&str>,
) -> Option<Adapter> {
    let mut adapters = managed.iter().filter_map(|(path, ifaces)| {
        ifaces
            .get(IWD_ADAPTER_IFACE)
            .map(|props| (path.as_str(), props))
    });
    let (path, props) = match station {
        Some(station) => {
            let parent = adapter_path_from_station(station);
            adapters.find(|(path, _)| *path == parent)?
        }
        None => adapters
            .clone()
            .find(|(path, _)| Some(*path) == kept)
            .or_else(|| adapters.min_by(|(a, _), (b, _)| a.cmp(b)))?,
    };
    Some(adapter_from_props(path, props))
}

/// The station to publish for a freshly parsed one: `parsed`, keeping the
/// SSID of the station already published when both are on the same network
/// (#1551).
///
/// It is the one rule for every station read: the discovery snapshot
/// ([`refresh_station_from_managed`]), the live re-read
/// ([`super::client::refresh_station`]) and a `PropertiesChanged` delta
/// ([`apply_station_props_delta`]). Until #1654 the last two kept the SSID
/// whatever the network, so a network change named the network just left
/// until the network list was read.
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
///   new iwd take the name or the station removed, and a discovery pass
///   clears it when it finds iwd without an owner or without a station
///   (#1654), so no SSID crosses those.
/// - Both are the same station object (the same path). Under iwd's own path
///   scheme this decides nothing the next condition does not: a network's
///   path extends its station's, so the same `ConnectedNetwork` already
///   means the same station. It is kept as a cheap guard against a path that
///   does not follow that scheme, and no fixture iwd could produce reaches it
///   on its own, so no test pins it.
/// - `parsed` names a `ConnectedNetwork`, and it is the one `previous`
///   named. iwd builds a network's path from the station's path, the SSID in
///   hex and the security (`iwd_network_get_path`, iwd 3.12
///   `src/station.c:435`), so one path is one SSID. That also covers an iwd
///   that restarts between two passes, before the next pass takes its owner
///   receiver: that owner change is sent before the receiver exists, so
///   nothing reads it. The next pass's wait for iwd asks the bus whether the
///   name has an owner and returns at once on yes ([`super::watcher`]'s
///   `wait_for_owner`), reading nothing. So nothing clears the state, and a
///   path the new iwd shares with the old one names the same network.
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
        connected_ssid: None, // kept by `with_kept_ssid` below; the network read sets it
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
        apply_station_props_delta(&changed, &[], &station);
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
    /// **Falsification:** drop any one of `with_kept_ssid`'s network
    /// conditions (the `Some` on both sides of the network match, the
    /// `disconnected` check) and its row keeps "home". Dropping the station
    /// path check keeps every row blank: each row uses a path iwd could
    /// build, and there the network path already carries the station's.
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
                "another station on a network of the same name, as after the \
                 adapter went and came back: iwd puts the network under the \
                 new station's path, so it is not the same network",
                Some(home),
                station(
                    "/net/connman/iwd/1/5",
                    StationState::Connected,
                    Some("/net/connman/iwd/1/5/686f6d65_psk"),
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

    fn owned<'a>(x: impl Into<zbus::zvariant::Value<'a>>) -> OwnedValue {
        x.into().try_to_owned().expect("an owned value")
    }

    fn object_path(p: &str) -> OwnedValue {
        owned(zbus::zvariant::ObjectPath::try_from(p).expect("an object path"))
    }

    /// A `PropertiesChanged` delta's changed dict and invalidated list, as
    /// the watcher decodes them from iwd's `(s, a{sv}, as)`.
    fn delta(
        changed: Vec<(&str, OwnedValue)>,
        invalidated: &[&str],
    ) -> (HashMap<String, OwnedValue>, Vec<String>) {
        (
            changed
                .into_iter()
                .map(|(k, x)| (k.to_owned(), x))
                .collect(),
            invalidated.iter().map(|&k| k.to_owned()).collect(),
        )
    }

    /// iwd's disconnect: `State` changed to `disconnected`, and
    /// `ConnectedNetwork` (with `ConnectedAccessPoint`) in the invalidated
    /// list, because its getter answers nothing once the network is gone
    /// (#1654). The station loses its network and its SSID, so the panel
    /// stops marking the old network "Connected". The same holds when the
    /// invalidation arrives in a signal of its own, with nothing changed.
    ///
    /// **Falsification:** ignore the invalidated list and the network stays
    /// `HOME` in both rows.
    #[test]
    fn an_invalidated_connected_network_clears_the_network_and_its_ssid() {
        let rows = [
            (
                "with the state change",
                delta(
                    vec![("State", owned("disconnected"))],
                    &["ConnectedNetwork", "ConnectedAccessPoint"],
                ),
                StationState::Disconnected,
            ),
            (
                "alone",
                delta(Vec::new(), &["ConnectedNetwork", "ConnectedAccessPoint"]),
                StationState::Connected,
            ),
        ];
        for (case, (changed, invalidated), state) in rows {
            let published = Mutable::new(Some(station(
                STATION,
                StationState::Connected,
                Some(HOME),
                Some("home"),
            )));
            apply_station_props_delta(&changed, &invalidated, &published);
            let st = published.get_cloned().expect("a station");
            assert_eq!(st.connected_network, None, "{case}");
            assert_eq!(st.connected_ssid, None, "{case}");
            assert_eq!(st.state, state, "{case}");
        }
    }

    /// A delta that moves the station to another network blanks the SSID
    /// until the network read names the new one, and a delta that keeps it
    /// on the same network keeps the SSID: [`with_kept_ssid`]'s rule (#1654).
    /// Until then a network change kept the old network's name.
    ///
    /// **Falsification:** keep the published SSID whatever the network (the
    /// pre-#1654 field update) and the first row names "home" for `WORK`;
    /// blank it on every delta and the other rows lose it; clear the network
    /// on any invalidated key rather than `ConnectedNetwork` alone and the
    /// last row loses `HOME`.
    #[test]
    fn a_delta_keeps_the_ssid_only_on_the_same_network() {
        let rows = [
            (
                "another network",
                vec![("ConnectedNetwork", object_path(WORK))],
                Vec::new(),
                Some(WORK),
                None,
            ),
            (
                "the same network again",
                vec![("ConnectedNetwork", object_path(HOME))],
                Vec::new(),
                Some(HOME),
                Some("home"),
            ),
            (
                "a roam on the same network",
                vec![("State", owned("roaming"))],
                Vec::new(),
                Some(HOME),
                Some("home"),
            ),
            (
                "a scan",
                vec![("Scanning", owned(true))],
                Vec::new(),
                Some(HOME),
                Some("home"),
            ),
            (
                "another key invalidated: iwd lists `ConnectedAccessPoint` \
                 only beside `ConnectedNetwork`, but only the latter clears \
                 the network",
                Vec::new(),
                vec!["ConnectedAccessPoint"],
                Some(HOME),
                Some("home"),
            ),
        ];
        for (case, changed, invalidated, network, ssid) in rows {
            let published = Mutable::new(Some(station(
                STATION,
                StationState::Connected,
                Some(HOME),
                Some("home"),
            )));
            let (changed, invalidated) = delta(changed, &invalidated);
            apply_station_props_delta(&changed, &invalidated, &published);
            let st = published.get_cloned().expect("a station");
            assert_eq!(st.connected_network.as_deref(), network, "{case}");
            assert_eq!(st.connected_ssid.as_deref(), ssid, "{case}");
        }
    }

    const ADAPTER: &str = "/net/connman/iwd/0";

    fn opath(p: &str) -> zbus::zvariant::OwnedObjectPath {
        zbus::zvariant::OwnedObjectPath::try_from(p).expect("an object path")
    }

    /// An object with each of `ifaces`, with `props` on every one.
    fn object(
        ifaces: &[&str],
        props: &[(&str, OwnedValue)],
    ) -> HashMap<String, HashMap<String, OwnedValue>> {
        ifaces
            .iter()
            .map(|iface| {
                let props = props
                    .iter()
                    .map(|(k, x)| ((*k).to_owned(), x.try_clone().expect("a cloneable value")))
                    .collect();
                ((*iface).to_owned(), props)
            })
            .collect()
    }

    fn adapter_object(powered: bool, name: &str) -> HashMap<String, HashMap<String, OwnedValue>> {
        object(
            &["net.connman.iwd.Adapter"],
            &[("Powered", owned(powered)), ("Name", owned(name))],
        )
    }

    /// iwd's tree with the adapter powered off, as iwd 3.12 leaves it: the
    /// adapter's object stays (it goes only with the wiphy,
    /// `src/wiphy.c:2478-2490`), the netdev's `Device` stays (it goes only
    /// with the netdev, `src/device.c:362-364`), and the `Station` is gone
    /// (`src/station.c:5941-5945`). An iwd that starts with the radio
    /// already off never adds it (`:5928-5934`).
    fn powered_off_tree() -> ManagedObjects {
        HashMap::from([
            (opath(ADAPTER), adapter_object(false, "phy0")),
            (
                opath(STATION),
                object(
                    &["net.connman.iwd.Device"],
                    &[("Name", owned("wlan0")), ("Powered", owned(false))],
                ),
            ),
        ])
    }

    /// A snapshot with an adapter and no station publishes the adapter,
    /// powered off, and no station (#1656). It used to publish neither, so
    /// the panel read "No adapter" and its power switch went insensitive.
    ///
    /// **Falsification:** return no adapter without a station (the pre-#1656
    /// discovery) and the first assertion fails.
    #[test]
    fn a_snapshot_with_an_adapter_and_no_station_publishes_the_adapter() {
        let found = discovered(powered_off_tree(), None, None);
        assert_eq!(
            found.adapter,
            Some(Adapter {
                path: ADAPTER.to_owned(),
                powered: false,
                name: "phy0".to_owned(),
            })
        );
        assert_eq!(found.station, None);
    }

    /// Which adapter a snapshot publishes. With a station, the station's
    /// parent, as discovery always took it, even when another adapter sorts
    /// first or another is published. Without one, the adapter already
    /// published while the snapshot still has it (#1657 review), else the
    /// lowest path. With none, none (#1656).
    ///
    /// **Falsification:** pick the lowest path with a station too and the
    /// second row names `/net/connman/iwd/0`; prefer the published adapter
    /// with a station too and the third row does; ignore the published one
    /// without a station and the fifth row names `/net/connman/iwd/0`; pick
    /// by map order without a station and the fourth or sixth row varies
    /// between runs of the process.
    #[test]
    fn the_adapter_a_snapshot_publishes() {
        /// A case, the snapshot, the published adapter's path, and the
        /// station's and adapter's paths the snapshot must publish.
        type Row<'a> = (
            &'a str,
            ManagedObjects,
            Option<&'a str>,
            Option<&'a str>,
            Option<&'a str>,
        );
        let station = || {
            object(
                &["net.connman.iwd.Device", "net.connman.iwd.Station"],
                &[("State", owned("connected"))],
            )
        };
        let two = |phy1_station: bool| {
            let mut managed = HashMap::from([
                (opath("/net/connman/iwd/0"), adapter_object(false, "phy0")),
                (opath("/net/connman/iwd/1"), adapter_object(true, "phy1")),
            ]);
            if phy1_station {
                managed.insert(opath("/net/connman/iwd/1/7"), station());
            }
            managed
        };
        let rows: [Row<'_>; 8] = [
            (
                "the station's own adapter",
                HashMap::from([
                    (opath(ADAPTER), adapter_object(true, "phy0")),
                    (opath(STATION), station()),
                ]),
                None,
                Some(STATION),
                Some(ADAPTER),
            ),
            (
                "the station's own adapter, though another sorts first",
                two(true),
                None,
                Some("/net/connman/iwd/1/7"),
                Some("/net/connman/iwd/1"),
            ),
            (
                "the station's own adapter, though another is published",
                two(true),
                Some("/net/connman/iwd/0"),
                Some("/net/connman/iwd/1/7"),
                Some("/net/connman/iwd/1"),
            ),
            (
                "no station and none published: the lowest path",
                HashMap::from([
                    (opath("/net/connman/iwd/3"), adapter_object(false, "phy3")),
                    (opath("/net/connman/iwd/1"), adapter_object(false, "phy1")),
                    (opath("/net/connman/iwd/2"), adapter_object(false, "phy2")),
                ]),
                None,
                None,
                Some("/net/connman/iwd/1"),
            ),
            (
                "no station: the published adapter, though another sorts first",
                two(false),
                Some("/net/connman/iwd/1"),
                None,
                Some("/net/connman/iwd/1"),
            ),
            (
                "no station, and the published adapter gone: the lowest path",
                HashMap::from([
                    (opath("/net/connman/iwd/3"), adapter_object(false, "phy3")),
                    (opath("/net/connman/iwd/2"), adapter_object(false, "phy2")),
                ]),
                Some("/net/connman/iwd/1"),
                None,
                Some("/net/connman/iwd/2"),
            ),
            (
                "a station whose parent is no adapter",
                HashMap::from([(opath(STATION), station())]),
                None,
                Some(STATION),
                None,
            ),
            ("nothing at all", HashMap::new(), None, None, None),
        ];
        for (case, managed, kept, station_expected, adapter_expected) in rows {
            let found = discovered(managed, None, kept);
            assert_eq!(
                found.station.as_ref().map(|p| p.as_str()),
                station_expected,
                "{case}"
            );
            assert_eq!(
                found.adapter.as_ref().map(|a| a.path.as_str()),
                adapter_expected,
                "{case}"
            );
        }
    }

    /// Which station a snapshot publishes with two Wi-Fi interfaces or more
    /// (#1659): the one already published while the snapshot still has it,
    /// else the lowest path, and the adapter is that station's parent. Each
    /// row reads 16 fresh maps, each iterating in its own order.
    ///
    /// **Falsification:** take the first station in the map's order (the
    /// pre-#1659 rule) and a row varies across the maps; ignore the published
    /// station and the second row names `/net/connman/iwd/0/4`; take the
    /// highest path and the first row names `/net/connman/iwd/2/6`.
    #[test]
    fn the_station_a_snapshot_publishes() {
        let three = || {
            let mut managed = HashMap::new();
            for phy in ["2/6", "0/4", "1/5"] {
                let station = format!("/net/connman/iwd/{phy}");
                let props = [("State", owned("connected"))];
                managed.insert(opath(&station), object(&[IWD_STATION_IFACE], &props));
                let adapter = adapter_path_from_station(&station);
                managed.insert(opath(&adapter), adapter_object(true, "phy"));
            }
            managed
        };
        let rows = [
            (
                "none published: the lowest path",
                None,
                "/net/connman/iwd/0/4",
            ),
            (
                "the published station, though another sorts first",
                Some("/net/connman/iwd/1/5"),
                "/net/connman/iwd/1/5",
            ),
            (
                "the published station gone: the lowest path",
                Some("/net/connman/iwd/3/7"),
                "/net/connman/iwd/0/4",
            ),
        ];
        for (case, kept, expected) in rows {
            for _ in 0..16 {
                let found = discovered(three(), kept, None);
                assert_eq!(
                    found.station.as_ref().map(|p| p.as_str()),
                    Some(expected),
                    "{case}"
                );
                assert_eq!(
                    found.adapter.map(|a| a.path),
                    Some(adapter_path_from_station(expected)),
                    "{case}: the station's own adapter"
                );
            }
        }
    }

    fn removed(path: &str, ifaces: &[&str]) -> zbus::Message {
        let path = zbus::zvariant::ObjectPath::try_from(path).expect("an object path");
        zbus::Message::signal(
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesRemoved",
        )
        .expect("signal builder")
        .build(&(path, ifaces))
        .expect("build InterfacesRemoved")
    }

    /// An `InterfacesRemoved` is the station's only when it takes the
    /// published station's `Station` interface, and the adapter's only when
    /// it takes the published adapter's `Adapter` (#1656). Powering off
    /// removes the first and not the second, and the watcher keeps the
    /// adapter for it.
    ///
    /// **Falsification:** read any removal at the station's path as the
    /// adapter's (the pre-#1656 clear) and the first row fails; read the
    /// adapter's by path alone and the `Properties`-only row does.
    #[test]
    fn a_removal_is_the_stations_or_the_adapters_by_path_and_interface() {
        let published = (Some(STATION), Some(ADAPTER));
        let rows = [
            (
                "the station's interface: powering off",
                removed(STATION, &["net.connman.iwd.Station"]),
                published,
                Removed::Station,
            ),
            (
                "the station's netdev, with its station",
                removed(
                    STATION,
                    &["net.connman.iwd.Device", "net.connman.iwd.Station"],
                ),
                published,
                Removed::Station,
            ),
            (
                "the adapter's object: the wiphy is gone",
                removed(
                    ADAPTER,
                    &["net.connman.iwd.Adapter", "org.freedesktop.DBus.Properties"],
                ),
                published,
                Removed::Adapter,
            ),
            (
                "the adapter removed while no station is published",
                removed(ADAPTER, &["net.connman.iwd.Adapter"]),
                (None, Some(ADAPTER)),
                Removed::Adapter,
            ),
            (
                "another interface at the adapter's path",
                removed(ADAPTER, &["org.freedesktop.DBus.Properties"]),
                published,
                Removed::Other,
            ),
            (
                "another adapter",
                removed("/net/connman/iwd/1", &["net.connman.iwd.Adapter"]),
                published,
                Removed::Other,
            ),
            (
                "another station",
                removed("/net/connman/iwd/1/7", &["net.connman.iwd.Station"]),
                published,
                Removed::Other,
            ),
            (
                "the station's path with no station published",
                removed(STATION, &["net.connman.iwd.Station"]),
                (None, Some(ADAPTER)),
                Removed::Other,
            ),
            (
                "a network",
                removed(HOME, &["net.connman.iwd.Network"]),
                published,
                Removed::Other,
            ),
        ];
        for (case, msg, (station, adapter), expected) in rows {
            assert_eq!(
                removed_from_event(&msg, station, adapter),
                expected,
                "{case}"
            );
        }
    }

    /// Without a station the watcher rediscovers on an `InterfacesAdded`
    /// that brings a station or an adapter, and on one it cannot read, and
    /// on nothing else (#1656).
    ///
    /// **Falsification:** answer `true` for every add and the known-network
    /// row fails; answer `false` for an undecodable one and its row does.
    #[test]
    fn an_add_may_bring_a_station_or_an_adapter() {
        let added = |path: &str, ifaces: &[&str]| {
            let path = zbus::zvariant::ObjectPath::try_from(path).expect("an object path");
            let ifaces: HashMap<&str, HashMap<&str, zbus::zvariant::Value<'_>>> =
                ifaces.iter().map(|i| (*i, HashMap::new())).collect();
            zbus::Message::signal("/", "org.freedesktop.DBus.ObjectManager", "InterfacesAdded")
                .expect("signal builder")
                .build(&(path, ifaces))
                .expect("build InterfacesAdded")
        };
        let rows = [
            (
                "the station, back with the radio",
                added(STATION, &["net.connman.iwd.Station"]),
                true,
            ),
            (
                "a new adapter",
                added("/net/connman/iwd/1", &["net.connman.iwd.Adapter"]),
                true,
            ),
            (
                "a device without its station",
                added(STATION, &["net.connman.iwd.Device"]),
                false,
            ),
            (
                "a known network",
                added(
                    "/net/connman/iwd/686f6d65_psk",
                    &["net.connman.iwd.KnownNetwork"],
                ),
                false,
            ),
            (
                "a body that is not InterfacesAdded's",
                zbus::Message::signal("/", "org.freedesktop.DBus.ObjectManager", "InterfacesAdded")
                    .expect("signal builder")
                    .build(&(7_u32,))
                    .expect("build"),
                true,
            ),
        ];
        for (case, msg, expected) in rows {
            assert_eq!(adds_station_or_adapter(&msg), expected, "{case}");
        }
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
