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
//! per device — here across the whole device *set*, maintained as devices come
//! and go, see [`DeviceWatches`]). No `/sys` scraping.
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
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
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

/// One successful NM read: the links to publish, plus the device object paths
/// they were read from.
///
/// The paths are the *whole* `GetDevices` answer, not just the devices that
/// yielded a [`Link`] — a device with no usable `Interface` yet is exactly the
/// one whose next `PropertiesChanged` will give it one, so it still needs a
/// subscription (see [`DeviceWatches`]).
struct NmSnapshot {
    links: Vec<Link>,
    devices: Vec<String>,
}

/// Read every NM device and build the full [`Link`] list.
///
/// # Errors
///
/// Propagates a failed `GetDevices` rather than collapsing it into an empty
/// list: "NM answered with no devices" and "NM did not answer" are different
/// facts, and only the first may be rendered as one (#608).
async fn read_nm_links() -> Result<NmSnapshot, hytte_bus::BusError> {
    let devices = get_devices().await?;

    let mut links = Vec::with_capacity(devices.len());
    let mut paths = Vec::with_capacity(devices.len());
    for dev in devices {
        let path = dev.as_str().to_string();
        if let Some(link) = link_from_device(&path).await {
            links.push(link);
        }
        paths.push(path);
    }
    // Stable ordering by ifindex so the list doesn't churn between refreshes.
    links.sort_by_key(|l| l.idx);
    Ok(NmSnapshot {
        links,
        devices: paths,
    })
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
/// Returns the device object paths NM reported, so the caller can reconcile its
/// per-device subscriptions against them — or `None` when NM did not answer.
///
/// A failed read publishes [`LinkSource::Unknown`] and leaves the previous list
/// **in place**. Before #608 it published an empty list instead, which the panel
/// rendered as "Offline / 0 interface(s)" — a negative claim assembled out of a
/// question that failed. This is also the path a host with no NM at all takes,
/// since the backend probe reaches this watcher optimistically when it cannot
/// establish whether NM exists (#607). `None` (rather than an empty device list)
/// keeps the same distinction on the subscription side: a transient `GetDevices`
/// failure must not be read as "every device vanished" and tear every live
/// subscription down.
async fn refresh(
    links_out: &Mutable<Vec<Link>>,
    primary_out: &Mutable<Option<Link>>,
    source_out: &Mutable<LinkSource>,
) -> Option<Vec<String>> {
    match read_nm_links().await {
        Ok(snapshot) => {
            let primary = pick_primary(&snapshot.links);
            links_out.set(snapshot.links);
            primary_out.set(primary);
            source_out.set_neq(LinkSource::NetworkManager);
            Some(snapshot.devices)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "networkd_nm: GetDevices failed; link source reported as unknown"
            );
            source_out.set_neq(LinkSource::Unknown);
            None
        }
    }
}

// ── Per-device subscription set (#731) ─────────────────────────────────────────

/// Depth of the queue carrying per-device wakeups to the watcher loop.
///
/// Small on purpose: every entry means the same thing ("something changed, go
/// re-read"), so a full queue is not backpressure to wait on — it is already a
/// pending refresh that will observe the newer change too. See
/// [`pump_device_props`].
const DEVICE_WAKE_QUEUE: usize = 8;

/// What one reconciliation round has to do to the live subscription set.
///
/// Both lists are sorted and disjoint by construction (they come from set
/// differences), which is what makes [`DeviceWatches::apply`] order-independent.
#[derive(Debug, Default, PartialEq, Eq)]
struct WatchPlan {
    /// Device paths NM reports that are not watched yet — subscribe.
    add: Vec<String>,
    /// Device paths watched that NM no longer reports — unsubscribe.
    remove: Vec<String>,
}

impl WatchPlan {
    fn is_empty(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty()
    }
}

/// Diff the currently-watched device paths against the set NM just reported.
///
/// This is the whole of the churn logic, kept pure so it can be tested without
/// `NetworkManager`: devices appear and vanish (a dock, a USB tether, a VPN tun
/// going up and down), so the subscription set has to be *maintained*, not
/// established once. Re-subscribing to an already-watched device would leak a
/// task and a bus match rule per hotplug cycle, which is why `add` is a strict
/// difference rather than "everything NM reported".
///
/// Empty and root (`"/"`) paths are dropped: NM uses `"/"` as its nil object
/// path, and there is nothing to subscribe to there.
fn plan_watches(watched: &BTreeSet<String>, reported: &[String]) -> WatchPlan {
    let desired: BTreeSet<String> = reported
        .iter()
        .filter(|p| !p.is_empty() && p.as_str() != "/")
        .cloned()
        .collect();

    WatchPlan {
        add: desired.difference(watched).cloned().collect(),
        remove: watched.difference(&desired).cloned().collect(),
    }
}

/// A per-device pump task that is aborted when dropped.
///
/// Dropping a bare [`JoinHandle`] detaches the task instead of stopping it, so
/// without this wrapper every removed device would leave its pump — and the
/// [`hytte_bus`] subscription that pump owns — running for the life of the
/// process. That is the per-hotplug-cycle leak this type exists to prevent.
struct PumpTask(JoinHandle<()>);

impl PumpTask {
    fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

impl Drop for PumpTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The live set of per-device `PropertiesChanged` subscriptions.
///
/// One pump task per NM device path; the map *is* the subscription set, so
/// removing an entry is what unsubscribes (via [`PumpTask`]'s `Drop`).
struct DeviceWatches {
    pumps: BTreeMap<String, PumpTask>,
    wake_tx: mpsc::Sender<()>,
}

impl DeviceWatches {
    fn new(wake_tx: mpsc::Sender<()>) -> Self {
        Self {
            pumps: BTreeMap::new(),
            wake_tx,
        }
    }

    fn watched(&self) -> BTreeSet<String> {
        self.pumps.keys().cloned().collect()
    }

    /// Carry out a plan, creating each new pump with `spawn`.
    ///
    /// Removals run first so a path that is somehow in both lists ends up
    /// freshly subscribed rather than dropped.
    fn apply(&mut self, plan: &WatchPlan, spawn: impl Fn(&str) -> PumpTask) {
        for path in &plan.remove {
            self.pumps.remove(path);
        }
        for path in &plan.add {
            self.pumps.insert(path.clone(), spawn(path));
        }
    }

    /// Reconcile against the device set NM just reported, using `spawn` to
    /// create pumps. Split out from [`DeviceWatches::reconcile`] so the churn
    /// handling can be tested with a stub pump.
    fn reconcile_with(&mut self, reported: &[String], spawn: impl Fn(&str) -> PumpTask) {
        // A pump whose event stream ended is no longer a live subscription.
        // Forget it first so the plan below re-creates one for a device that is
        // still present (and simply drops the entry for one that isn't).
        self.pumps.retain(|_, pump| !pump.is_finished());

        let plan = plan_watches(&self.watched(), reported);
        if plan.is_empty() {
            return;
        }
        let (added, removed) = (plan.add.len(), plan.remove.len());
        self.apply(&plan, spawn);
        tracing::debug!(
            added,
            removed,
            watched = self.pumps.len(),
            "networkd_nm: reconciled per-device subscriptions"
        );
    }

    fn reconcile(&mut self, reported: &[String]) {
        let wake_tx = self.wake_tx.clone();
        self.reconcile_with(reported, move |path| {
            let path = path.to_string();
            let wake_tx = wake_tx.clone();
            PumpTask(hytte_reactive::runtime::handle().spawn(pump_device_props(path, wake_tx)))
        });
    }
}

/// Forward one device's `PropertiesChanged` emissions to the watcher loop.
///
/// The [`hytte_bus`] subscription is owned by this future on purpose: dropping
/// the last handle tears the subscription down, so aborting this task (which
/// drops the future, and with it `sub`) is what unsubscribes.
async fn pump_device_props(path: String, wake_tx: mpsc::Sender<()>) {
    let sub = hytte_bus::signals(BusKind::System, NM_NAME)
        .at_path(path.clone())
        .iface(PROPS_IFACE)
        .signal("PropertiesChanged")
        .start();

    let mut events = sub.events();
    while events.next().await.is_some() {
        // A `Full` queue is not backpressure to wait on: a wakeup is already
        // pending and the refresh it triggers re-reads NM, so it will observe
        // this change too. Dropping the duplicate also keeps the pump from
        // blocking behind a slow multi-round-trip refresh. Only `Closed` — the
        // watcher loop is gone — ends the pump.
        if let Err(mpsc::error::TrySendError::Closed(())) = wake_tx.try_send(()) {
            break;
        }
    }

    tracing::debug!(path, "networkd_nm: per-device signal stream ended");
}

// ── Main watcher task ──────────────────────────────────────────────────────────

/// Refresh the published state and, if NM answered, reconcile the per-device
/// subscription set against the device list it just reported.
async fn refresh_and_reconcile(
    watches: &mut DeviceWatches,
    links_out: &Mutable<Vec<Link>>,
    primary_out: &Mutable<Option<Link>>,
    source_out: &Mutable<LinkSource>,
) {
    if let Some(devices) = refresh(links_out, primary_out, source_out).await {
        watches.reconcile(&devices);
    }
}

/// NM link watcher loop. Reads the initial device list, then refreshes on:
///
/// * `DeviceAdded` / `DeviceRemoved` on the manager (hot-plug, NM
///   register/unregister),
/// * `PropertiesChanged` on the **manager** object — manager-level changes
///   (e.g. `State`/`PrimaryConnection`),
/// * `PropertiesChanged` on **every device NM currently reports** (#731) —
///   the push path for a per-device `State`/`Ip4Config` transition that leaves
///   the manager properties untouched. This mirrors what [`crate::wifi_nm`]
///   does for its single device, extended to the current device *set*: the set
///   is reconciled after every successful refresh, so a dock, a USB tether or a
///   VPN tun appearing or vanishing gains or loses exactly one subscription —
///   see [`plan_watches`] and [`PumpTask`] for why neither a task nor a match
///   rule accumulates across hotplug cycles.
/// * a 5-second poll, retained as the **safety net** it was always meant to
///   back: subscriptions can be missed (a bus reconnect window, a burst that
///   laps the broadcast channel, a lost `DeviceAdded` leaving a device
///   unwatched), and the poll is what recovers the subscription set in each
///   case. Before #731 it was the *primary* liveness mechanism for per-device
///   state and addresses, which is why a non-primary interface could read up to
///   ~5s stale.
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

    // Per-device `PropertiesChanged` wakeups arrive here. `watches` owns the
    // sender, so the receiver never closes while this loop lives — and dropping
    // `watches` (only on task teardown) unsubscribes every device at once.
    let (wake_tx, mut wake_rx) = mpsc::channel::<()>(DEVICE_WAKE_QUEUE);
    let mut watches = DeviceWatches::new(wake_tx);

    // Initial snapshot; also arms the first per-device subscriptions.
    refresh_and_reconcile(&mut watches, &links_out, &primary_out, &source_out).await;

    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await; // discard the immediate first tick

    tracing::info!("networkd_nm: watching NetworkManager devices for link list");

    loop {
        tokio::select! {
            Some(_) = added_events.next() => {
                tracing::debug!("networkd_nm: DeviceAdded; refreshing links");
                refresh_and_reconcile(&mut watches, &links_out, &primary_out, &source_out).await;
            }
            Some(_) = removed_events.next() => {
                tracing::debug!("networkd_nm: DeviceRemoved; refreshing links");
                refresh_and_reconcile(&mut watches, &links_out, &primary_out, &source_out).await;
            }
            Some(_) = manager_events.next() => {
                refresh_and_reconcile(&mut watches, &links_out, &primary_out, &source_out).await;
            }
            Some(()) = wake_rx.recv() => {
                // Collapse a burst into one read. NM walks a device through
                // several states per activation, each emitting
                // `PropertiesChanged`, and every queued wake asks for the same
                // thing — a fresh read of all devices. Draining here bounds a
                // burst to two refreshes (this one, plus one for whatever
                // arrives while it runs) instead of one per emission.
                while wake_rx.try_recv().is_ok() {}
                tracing::debug!("networkd_nm: device PropertiesChanged; refreshing links");
                refresh_and_reconcile(&mut watches, &links_out, &primary_out, &source_out).await;
            }
            _ = interval.tick() => {
                refresh_and_reconcile(&mut watches, &links_out, &primary_out, &source_out).await;
            }
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    // ── Per-device subscription churn (#731) ─────────────────────────────────

    const WIFI: &str = "/org/freedesktop/NetworkManager/Devices/1";
    const DOCK: &str = "/org/freedesktop/NetworkManager/Devices/2";
    const TUN: &str = "/org/freedesktop/NetworkManager/Devices/3";

    fn watched(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    fn reported(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    #[test]
    fn plan_watches_is_empty_when_the_device_set_is_unchanged() {
        // The steady state — one refresh every 5 s must not churn subscriptions.
        let plan = plan_watches(&watched(&[WIFI, DOCK]), &reported(&[DOCK, WIFI]));
        assert_eq!(plan, WatchPlan::default());
        assert!(plan.is_empty());
    }

    #[test]
    fn plan_watches_subscribes_to_a_newly_appeared_device() {
        // Dock plugged in.
        let plan = plan_watches(&watched(&[WIFI]), &reported(&[WIFI, DOCK]));
        assert_eq!(plan.add, vec![DOCK.to_string()]);
        assert!(plan.remove.is_empty());
    }

    #[test]
    fn plan_watches_unsubscribes_a_vanished_device() {
        // Dock unplugged: the stale subscription must be dropped, or every
        // hotplug cycle leaves a task and a match rule behind.
        let plan = plan_watches(&watched(&[WIFI, DOCK]), &reported(&[WIFI]));
        assert!(plan.add.is_empty());
        assert_eq!(plan.remove, vec![DOCK.to_string()]);
    }

    #[test]
    fn plan_watches_handles_a_simultaneous_add_and_remove() {
        // A VPN comes up in the same refresh window the dock goes away.
        let plan = plan_watches(&watched(&[WIFI, DOCK]), &reported(&[WIFI, TUN]));
        assert_eq!(plan.add, vec![TUN.to_string()]);
        assert_eq!(plan.remove, vec![DOCK.to_string()]);
    }

    #[test]
    fn plan_watches_drops_everything_when_nm_reports_no_devices() {
        let plan = plan_watches(&watched(&[WIFI, DOCK]), &[]);
        assert!(plan.add.is_empty());
        assert_eq!(plan.remove, vec![WIFI.to_string(), DOCK.to_string()]);
    }

    #[test]
    fn plan_watches_ignores_duplicate_and_placeholder_paths() {
        let paths = vec![
            WIFI.to_string(),
            WIFI.to_string(),
            "/".to_string(),
            String::new(),
        ];
        let plan = plan_watches(&BTreeSet::new(), &paths);
        assert_eq!(plan.add, vec![WIFI.to_string()]);
        assert!(plan.remove.is_empty());
    }

    // ── DeviceWatches churn, with a stub pump (no D-Bus) ─────────────────────

    /// Bumps a shared counter when dropped, so a test can observe that the task
    /// holding it was actually torn down rather than detached.
    struct DropCounter(Arc<AtomicUsize>);

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A stub pump that parks forever, holding a drop counter — the stand-in
    /// for [`pump_device_props`] and the `SignalSubscription` it owns.
    fn parked_pump(dropped: &Arc<AtomicUsize>) -> PumpTask {
        let guard = DropCounter(dropped.clone());
        PumpTask(tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        }))
    }

    /// Give an aborted task a chance to be dropped by the runtime.
    async fn settle(dropped: &Arc<AtomicUsize>, want: usize) {
        for _ in 0..256 {
            if dropped.load(Ordering::SeqCst) >= want {
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    fn watches() -> DeviceWatches {
        // The receiver is dropped immediately: nothing in these tests sends.
        let (tx, _rx) = mpsc::channel::<()>(DEVICE_WAKE_QUEUE);
        DeviceWatches::new(tx)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reconcile_subscribes_once_per_device_and_not_again() {
        let spawned = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut w = watches();

        let spawn = |_: &str| {
            spawned.fetch_add(1, Ordering::SeqCst);
            parked_pump(&dropped)
        };

        w.reconcile_with(&reported(&[WIFI, DOCK]), spawn);
        assert_eq!(spawned.load(Ordering::SeqCst), 2);

        // A steady-state refresh must not re-subscribe: doing so would leak a
        // task and a match rule every 5 seconds.
        w.reconcile_with(&reported(&[WIFI, DOCK]), spawn);
        assert_eq!(spawned.load(Ordering::SeqCst), 2);
        assert_eq!(w.watched(), watched(&[WIFI, DOCK]));
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reconcile_tears_down_the_pump_of_a_vanished_device() {
        let spawned = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut w = watches();
        let spawn = |_: &str| {
            spawned.fetch_add(1, Ordering::SeqCst);
            parked_pump(&dropped)
        };

        w.reconcile_with(&reported(&[WIFI, DOCK]), spawn);
        // Dock unplugged.
        w.reconcile_with(&reported(&[WIFI]), spawn);

        assert_eq!(w.watched(), watched(&[WIFI]));
        assert_eq!(spawned.load(Ordering::SeqCst), 2, "wifi must not respawn");

        // The removed pump's task is aborted, not merely detached — that is the
        // difference between reclaiming the subscription and leaking one per
        // hotplug cycle.
        settle(&dropped, 1).await;
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "the removed device's pump task must be aborted and dropped"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_hotplug_cycle_leaves_exactly_one_pump_per_device() {
        let spawned = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut w = watches();
        let spawn = |_: &str| {
            spawned.fetch_add(1, Ordering::SeqCst);
            parked_pump(&dropped)
        };

        // Plug and unplug the dock three times.
        for _ in 0..3 {
            w.reconcile_with(&reported(&[WIFI, DOCK]), spawn);
            w.reconcile_with(&reported(&[WIFI]), spawn);
        }

        assert_eq!(w.watched(), watched(&[WIFI]));
        assert_eq!(w.pumps.len(), 1);
        // 1 wifi + 3 dock subscriptions created, 3 dock subscriptions dropped.
        assert_eq!(spawned.load(Ordering::SeqCst), 4);
        settle(&dropped, 3).await;
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reconcile_replaces_a_pump_whose_stream_ended() {
        let spawned = Arc::new(AtomicUsize::new(0));
        let mut w = watches();

        // A pump that exits immediately: its event stream closed, so the device
        // is no longer really watched even though the map still lists it.
        let spawn = |_: &str| {
            spawned.fetch_add(1, Ordering::SeqCst);
            PumpTask(tokio::spawn(async {}))
        };

        w.reconcile_with(&reported(&[WIFI]), spawn);
        assert_eq!(spawned.load(Ordering::SeqCst), 1);

        // Let the pump finish, then refresh: the dead entry must be replaced,
        // not silently kept — otherwise that device pushes nothing ever again
        // and only the 5 s poll would still see it.
        for _ in 0..256 {
            if w.pumps.values().all(PumpTask::is_finished) {
                break;
            }
            tokio::task::yield_now().await;
        }
        w.reconcile_with(&reported(&[WIFI]), spawn);
        assert_eq!(
            spawned.load(Ordering::SeqCst),
            2,
            "a pump whose stream ended must be re-created"
        );
        assert_eq!(w.watched(), watched(&[WIFI]));
    }
}
