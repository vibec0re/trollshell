//! `NetworkManager` link/interface source for the [`crate::networkd`] service.
//!
//! On hosts where **systemd-networkd is not the active link manager** (e.g. a
//! NetworkManager-managed desktop, see issue #80), networkd's `ListLinks`
//! errors or returns nothing, leaving the network panel's "All links" list
//! empty. This module is the NetworkManager-over-D-Bus fallback: it enumerates
//! NM's devices and produces the **same [`crate::networkd::Link`] list** the
//! panel already renders, so no panel changes are required.
//!
//! It mirrors [`crate::wifi_nm`] (the #96 Wi-Fi NM backend): the same
//! [`hytte_bus`] primitives on the **system bus**, the same
//! `GetDevices` + per-device `GetAll` reads, and the same live-update strategy
//! (watch `DeviceAdded`/`DeviceRemoved` on the manager + `PropertiesChanged`
//! per device). No `/sys` scraping.
//!
//! # Field coverage vs networkd
//!
//! Networkd's [`crate::networkd::Link`] carries name, ifindex, operational
//! state, addresses, gateways, and a route table. From NM we populate:
//!
//! * `name`        — `Device.Interface`
//! * `idx`         — `Device.Ip4Config`/`Ip6Config` don't expose ifindex, so we
//!   read `Device.Ifindex` (present since NM 1.34). Falls back to `0`.
//! * `operational` — mapped from `Device.State` (see [`device_state_to_op`]).
//! * `addresses`   — `IP4Config.AddressData` + `IP6Config.AddressData`.
//! * `gateway_v4`/`gateway_v6` — `IP4Config.Gateway` / `IP6Config.Gateway`.
//! * `routes`      — left empty: NM's `RouteData` is available but the panel
//!   does not render per-route detail for the link list (only addresses,
//!   gateways, and state), so we skip it to keep the read cheap. This is the
//!   one field networkd populates that the NM source intentionally does not.

use futures_signals::signal::Mutable;
use futures_util::StreamExt;
use hytte_bus::BusKind;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::networkd::{Link, LinkAddress, LinkSource, OperationalState};

const NM_NAME: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_IFACE: &str = "org.freedesktop.NetworkManager";
const NM_DEVICE_IFACE: &str = "org.freedesktop.NetworkManager.Device";
const NM_IP4_IFACE: &str = "org.freedesktop.NetworkManager.IP4Config";
const NM_IP6_IFACE: &str = "org.freedesktop.NetworkManager.IP6Config";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";

// ── Pure conversion helpers ───────────────────────────────────────────────────

/// Map an NM `NMDeviceState` (`Device.State`, a `u32`) onto networkd's
/// [`OperationalState`] so the panel's pills/priority logic is identical for
/// both backends.
///
/// NM `NMDeviceState` values (from `libnm`):
/// * 100 (Activated)              → [`OperationalState::Routable`]
/// * 90 (Secondaries) / 80 (`IpCheck`) / 70 (`IpConfig`) / 110 (Deactivating)
///   → [`OperationalState::Carrier`] (link up, not yet fully routable)
/// * 40–60 (Prepare / Config / `NeedAuth`)
///   → [`OperationalState::Dormant`] (coming up)
/// * 30 (Disconnected)            → [`OperationalState::NoCarrier`]
/// * 20 (Unavailable)             → [`OperationalState::Off`]
/// * 10 (Unmanaged) / 0 (Unknown) / anything else → [`OperationalState::Unknown`]
pub(crate) fn device_state_to_op(state: u32) -> OperationalState {
    match state {
        100 => OperationalState::Routable,
        70..=90 | 110 => OperationalState::Carrier,
        40..=60 => OperationalState::Dormant,
        30 => OperationalState::NoCarrier,
        20 => OperationalState::Off,
        _ => OperationalState::Unknown,
    }
}

/// Parse an NM address string (`"192.168.1.42"` or `"2a02:..."`) into an
/// [`IpAddr`].
fn parse_ip(s: &str) -> Option<IpAddr> {
    s.parse::<IpAddr>().ok()
}

// ── Generic D-Bus property helpers (mirrors wifi_nm) ───────────────────────────

fn property<T>(props: &HashMap<String, OwnedValue>, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| T::try_from(v).ok())
}

fn prop_string(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    property::<String>(props, key)
}

fn prop_object_path(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    property::<OwnedObjectPath>(props, key).map(|p| p.as_str().to_string())
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

async fn get_all(
    path: &str,
    iface: &str,
) -> Result<HashMap<String, OwnedValue>, hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, NM_NAME)
        .at_path(path.to_string())
        .iface(PROPS_IFACE)
        .method("GetAll")
        .args((iface.to_string(),))
        .send::<HashMap<String, OwnedValue>>()
        .await
}

// ── IP config parsing ──────────────────────────────────────────────────────────

/// One entry from NM's `IP4Config.AddressData` / `IP6Config.AddressData`
/// (an `aa{sv}` — array of dicts keyed `address` (string) + `prefix` (u32)).
fn parse_address_data(entries: &[HashMap<String, OwnedValue>], out: &mut Vec<LinkAddress>) {
    for entry in entries {
        let Some(addr_str) = prop_string(entry, "address") else {
            continue;
        };
        let Some(addr) = parse_ip(&addr_str) else {
            continue;
        };
        let prefix_len = property::<u32>(entry, "prefix").unwrap_or(0);
        out.push(LinkAddress {
            addr,
            prefix_len: u8::try_from(prefix_len).unwrap_or(0),
        });
    }
}

/// Read an `IP4Config`/`IP6Config` object and return its addresses + gateway.
async fn read_ip_config(path: &str, iface: &str) -> (Vec<LinkAddress>, Option<IpAddr>) {
    if path.is_empty() || path == "/" {
        return (Vec::new(), None);
    }
    let props = match get_all(path, iface).await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(path, error = %e, "networkd_nm: IP config GetAll failed");
            return (Vec::new(), None);
        }
    };

    let mut addresses = Vec::new();
    if let Some(entries) = property::<Vec<HashMap<String, OwnedValue>>>(&props, "AddressData") {
        parse_address_data(&entries, &mut addresses);
    }

    let gateway = prop_string(&props, "Gateway")
        .filter(|g| !g.is_empty())
        .and_then(|g| parse_ip(&g));

    (addresses, gateway)
}

// ── Per-device → Link ──────────────────────────────────────────────────────────

/// Build a [`Link`] from a single NM device's properties, reading its IP
/// configs for addresses/gateways. Returns `None` if the device has no usable
/// interface name (e.g. an unnamed placeholder).
async fn link_from_device(device_path: &str) -> Option<Link> {
    let props = match get_all(device_path, NM_DEVICE_IFACE).await {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(path = device_path, error = %e, "networkd_nm: device GetAll failed");
            return None;
        }
    };

    let name = prop_string(&props, "Interface").filter(|n| !n.is_empty())?;
    let idx = property::<i32>(&props, "Ifindex").unwrap_or(0);
    let state = property::<u32>(&props, "State").unwrap_or(0);
    let operational = device_state_to_op(state);

    let ip4_path = prop_object_path(&props, "Ip4Config").unwrap_or_default();
    let ip6_path = prop_object_path(&props, "Ip6Config").unwrap_or_default();

    let (mut addresses, gw4) = read_ip_config(&ip4_path, NM_IP4_IFACE).await;
    let (v6_addrs, gw6) = read_ip_config(&ip6_path, NM_IP6_IFACE).await;
    addresses.extend(v6_addrs);

    let gateway_v4 = match gw4 {
        Some(IpAddr::V4(v4)) => Some(v4),
        _ => None,
    };
    let gateway_v6 = match gw6 {
        Some(IpAddr::V6(v6)) => Some(v6),
        _ => None,
    };

    Some(Link {
        idx,
        name,
        operational,
        addresses,
        gateway_v4,
        gateway_v6,
        // routes intentionally left empty for the NM source — see module docs.
        routes: Vec::new(),
    })
}

/// Read every NM device and build the full [`Link`] list.
///
/// # Errors
///
/// Propagates a failed `GetDevices` rather than collapsing it into an empty
/// list: "NM answered with no devices" and "NM did not answer" are different
/// facts, and only the first may be rendered as one (#608).
async fn read_nm_links() -> Result<Vec<Link>, hytte_bus::BusError> {
    let devices = get_devices().await?;

    let mut out = Vec::with_capacity(devices.len());
    for dev in devices {
        if let Some(link) = link_from_device(dev.as_str()).await {
            out.push(link);
        }
    }
    // Stable ordering by ifindex so the list doesn't churn between refreshes.
    out.sort_by_key(|l| l.idx);
    Ok(out)
}

/// Recompute the primary link (highest operational priority) the same way the
/// networkd path does.
fn pick_primary(links: &[Link]) -> Option<Link> {
    links
        .iter()
        .max_by_key(|l| l.operational.priority())
        .filter(|l| l.operational.priority() > 0)
        .cloned()
}

/// Snapshot all NM links and push them to the shared mutables, including whether
/// NM answered at all ([`LinkSource`]).
///
/// A failed read publishes [`LinkSource::Unknown`] and leaves the previous list
/// **in place**. Before #608 it published an empty list instead, which the panel
/// rendered as "Offline / 0 interface(s)" — a negative claim assembled out of a
/// question that failed. This is also the path a host with no NM at all takes,
/// since the backend probe reaches this watcher optimistically when it cannot
/// establish whether NM exists (#607).
async fn refresh(
    links_out: &Mutable<Vec<Link>>,
    primary_out: &Mutable<Option<Link>>,
    source_out: &Mutable<LinkSource>,
) {
    match read_nm_links().await {
        Ok(links) => {
            let primary = pick_primary(&links);
            links_out.set(links);
            primary_out.set(primary);
            source_out.set_neq(LinkSource::NetworkManager);
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "networkd_nm: GetDevices failed; link source reported as unknown"
            );
            source_out.set_neq(LinkSource::Unknown);
        }
    }
}

// ── Main watcher task ──────────────────────────────────────────────────────────

/// NM link watcher loop. Reads the initial device list, then refreshes on:
///
/// * `DeviceAdded` / `DeviceRemoved` on the manager (hot-plug, NM
///   register/unregister),
/// * `PropertiesChanged` on the **manager** object — fires for manager-level
///   changes (e.g. `State`/`PrimaryConnection`), but NOT for a per-device
///   `State`/`Ip4Config` transition that leaves the manager properties
///   unchanged (we subscribe only at the manager path, not per device),
/// * a 5-second poll, which is therefore the **primary** liveness mechanism
///   for per-device state/address changes — not merely a safety net. A
///   non-primary interface's state or address can lag up to ~5s. (A future
///   refinement could re-subscribe to per-device `PropertiesChanged` for the
///   current device set, the way `wifi_nm` does for its single device.)
///
/// Runs forever (until the runtime shuts down). Per [`hytte_bus`], the shared
/// connection supervisor handles D-Bus reconnects; a re-poll on reconnect keeps
/// the list fresh across an NM restart.
pub(crate) async fn run_nm_links_watcher(
    links_out: Mutable<Vec<Link>>,
    primary_out: Mutable<Option<Link>>,
    source_out: Mutable<LinkSource>,
) {
    let device_added = hytte_bus::signals(BusKind::System, NM_NAME)
        .at_path(NM_PATH)
        .iface(NM_IFACE)
        .signal("DeviceAdded")
        .start();
    let device_removed = hytte_bus::signals(BusKind::System, NM_NAME)
        .at_path(NM_PATH)
        .iface(NM_IFACE)
        .signal("DeviceRemoved")
        .start();
    let manager_props = hytte_bus::signals(BusKind::System, NM_NAME)
        .at_path(NM_PATH)
        .iface(PROPS_IFACE)
        .signal("PropertiesChanged")
        .start();

    let mut added_events = device_added.events();
    let mut removed_events = device_removed.events();
    let mut manager_events = manager_props.events();

    // Initial snapshot.
    refresh(&links_out, &primary_out, &source_out).await;

    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await; // discard the immediate first tick

    tracing::info!("networkd_nm: watching NetworkManager devices for link list");

    loop {
        tokio::select! {
            Some(_) = added_events.next() => {
                tracing::debug!("networkd_nm: DeviceAdded; refreshing links");
                refresh(&links_out, &primary_out, &source_out).await;
            }
            Some(_) = removed_events.next() => {
                tracing::debug!("networkd_nm: DeviceRemoved; refreshing links");
                refresh(&links_out, &primary_out, &source_out).await;
            }
            Some(_) = manager_events.next() => {
                refresh(&links_out, &primary_out, &source_out).await;
            }
            _ = interval.tick() => {
                refresh(&links_out, &primary_out, &source_out).await;
            }
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn device_state_activated_is_routable() {
        assert_eq!(device_state_to_op(100), OperationalState::Routable);
    }

    #[test]
    fn device_state_ip_config_is_carrier() {
        assert_eq!(device_state_to_op(70), OperationalState::Carrier);
        assert_eq!(device_state_to_op(80), OperationalState::Carrier);
        assert_eq!(device_state_to_op(90), OperationalState::Carrier);
    }

    #[test]
    fn device_state_deactivating_is_carrier() {
        assert_eq!(device_state_to_op(110), OperationalState::Carrier);
    }

    #[test]
    fn device_state_prepare_is_dormant() {
        assert_eq!(device_state_to_op(40), OperationalState::Dormant);
        assert_eq!(device_state_to_op(60), OperationalState::Dormant);
    }

    #[test]
    fn device_state_disconnected_is_no_carrier() {
        assert_eq!(device_state_to_op(30), OperationalState::NoCarrier);
    }

    #[test]
    fn device_state_unavailable_is_off() {
        assert_eq!(device_state_to_op(20), OperationalState::Off);
    }

    #[test]
    fn device_state_unmanaged_and_unknown_are_unknown() {
        assert_eq!(device_state_to_op(10), OperationalState::Unknown);
        assert_eq!(device_state_to_op(0), OperationalState::Unknown);
        assert_eq!(device_state_to_op(120), OperationalState::Unknown);
    }

    #[test]
    fn parse_ip_v4_and_v6() {
        assert_eq!(
            parse_ip("192.168.1.42"),
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)))
        );
        assert!(matches!(
            parse_ip("2a02:3032:a:a4fb::1"),
            Some(IpAddr::V6(_))
        ));
        assert_eq!(parse_ip("not-an-ip"), None);
    }

    #[test]
    fn pick_primary_prefers_routable() {
        let links = vec![
            Link {
                idx: 1,
                name: "eth0".into(),
                operational: OperationalState::NoCarrier,
                ..Link::default()
            },
            Link {
                idx: 2,
                name: "wlan0".into(),
                operational: OperationalState::Routable,
                ..Link::default()
            },
        ];
        let primary = pick_primary(&links).expect("a routable link is primary");
        assert_eq!(primary.name, "wlan0");
    }

    #[test]
    fn pick_primary_none_when_all_down() {
        let links = vec![Link {
            idx: 1,
            name: "eth0".into(),
            operational: OperationalState::Off,
            ..Link::default()
        }];
        assert!(pick_primary(&links).is_none());
    }
}
