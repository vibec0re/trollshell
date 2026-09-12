//! Device discovery and watcher: listens to `BlueZ` `ObjectManager` signals and
//! keeps the `adapter` and `devices` mutables up to date.

use super::parse::{parse_adapter_props, parse_device_props, prop_bool, prop_str, property};
use super::types::{Adapter, Device};
use futures_signals::signal::Mutable;
use futures_util::StreamExt;
use hytte_bus::{BusKind, SignalEvent, SignalItem, SignalSubscription};
use hytte_reactive::runtime;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;
use zbus::zvariant::OwnedValue;

// ── Internal watcher state ────────────────────────────────────────────────────

#[derive(Clone)]
pub(super) struct State {
    pub(super) adapter: Mutable<Option<Adapter>>,
    pub(super) devices_map: Arc<AsyncMutex<HashMap<String, Device>>>,
    pub(super) devices: Mutable<Vec<Device>>,
}

impl State {
    pub(super) fn new(adapter: Mutable<Option<Adapter>>, devices: Mutable<Vec<Device>>) -> Self {
        Self {
            adapter,
            devices_map: Arc::new(AsyncMutex::new(HashMap::new())),
            devices,
        }
    }

    /// Snapshot the device map to a sorted Vec and publish it.
    pub(super) async fn publish_devices(&self) {
        let map = self.devices_map.lock().await;
        let mut list: Vec<Device> = map.values().cloned().collect();
        drop(map);
        // Sort: connected first, then paired, then alphabetical alias.
        list.sort_by(|a, b| {
            b.connected
                .cmp(&a.connected)
                .then(b.paired.cmp(&a.paired))
                .then(a.alias.to_lowercase().cmp(&b.alias.to_lowercase()))
        });
        self.devices.set(list);
    }

    /// Apply a partial property update to an existing adapter snapshot.
    pub(super) fn apply_adapter_props(&self, changed: &HashMap<String, OwnedValue>) {
        let mut guard = self.adapter.lock_mut();
        if let Some(a) = guard.as_mut() {
            if changed.contains_key("Powered") {
                a.powered = prop_bool(changed, "Powered");
            }
            if changed.contains_key("Discoverable") {
                a.discoverable = prop_bool(changed, "Discoverable");
            }
            if changed.contains_key("Discovering") {
                a.discovering = prop_bool(changed, "Discovering");
            }
            if changed.contains_key("Name") {
                a.name = prop_str(changed, "Name");
            }
            if changed.contains_key("Address") {
                a.address = prop_str(changed, "Address");
            }
        }
    }

    /// Apply a partial property update to an existing device entry.
    pub(super) async fn apply_device_props(
        &self,
        path: &str,
        changed: &HashMap<String, OwnedValue>,
    ) {
        let mut map = self.devices_map.lock().await;
        if let Some(dev) = map.get_mut(path) {
            if changed.contains_key("Connected") {
                dev.connected = prop_bool(changed, "Connected");
            }
            if changed.contains_key("Paired") {
                dev.paired = prop_bool(changed, "Paired");
            }
            if changed.contains_key("Trusted") {
                dev.trusted = prop_bool(changed, "Trusted");
            }
            if changed.contains_key("Alias") {
                dev.alias = prop_str(changed, "Alias");
            }
            if changed.contains_key("Icon") {
                dev.icon = prop_str(changed, "Icon");
            }
        }
    }
}

// ── Main listen loop ──────────────────────────────────────────────────────────

pub(super) type ManagedObjects =
    HashMap<zbus::zvariant::OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

pub(super) async fn get_managed_objects() -> Result<ManagedObjects, hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, "org.bluez")
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .method("GetManagedObjects")
        .args(())
        .send::<ManagedObjects>()
        .await
}

pub(super) async fn set_adapter_path(path: &str, store: &Arc<tokio::sync::RwLock<String>>) {
    *store.write().await = path.to_string();
}

pub(super) async fn listen(
    adapter_mutable: &Mutable<Option<Adapter>>,
    devices_mutable: &Mutable<Vec<Device>>,
    adapter_path_store: &Arc<tokio::sync::RwLock<String>>,
) -> Result<(), anyhow::Error> {
    let managed = get_managed_objects()
        .await
        .map_err(|e| anyhow::anyhow!("GetManagedObjects: {e}"))?;

    // ── Find the first adapter ────────────────────────────────────────────────

    let Some((adapter_obj_path, adapter_ifaces)) = managed
        .iter()
        .find(|(_, ifaces)| ifaces.contains_key("org.bluez.Adapter1"))
    else {
        return Err(anyhow::anyhow!("no org.bluez.Adapter1 found"));
    };

    let adapter_path = adapter_obj_path.as_str().to_string();
    set_adapter_path(&adapter_path, adapter_path_store).await;

    let adapter_props = adapter_ifaces
        .get("org.bluez.Adapter1")
        .expect("adapter iface present — just checked");
    let initial_adapter = parse_adapter_props(&adapter_path, adapter_props);
    adapter_mutable.set(Some(initial_adapter));

    tracing::info!(path = adapter_path, "bluetooth adapter found");

    // ── Initial device list ───────────────────────────────────────────────────

    let state = State::new(adapter_mutable.clone(), devices_mutable.clone());

    {
        let mut map = state.devices_map.lock().await;
        for (obj_path, ifaces) in &managed {
            let p = obj_path.as_str();
            if p.starts_with(adapter_path.as_str())
                && let Some(dev_props) = ifaces.get("org.bluez.Device1")
            {
                let mut dev = parse_device_props(p, dev_props);
                if let Some(bat_props) = ifaces.get("org.bluez.Battery1") {
                    dev.battery = property::<u8>(bat_props, "Percentage");
                }
                map.insert(p.to_string(), dev);
            }
        }
    }
    state.publish_devices().await;

    // ── Signal subscriptions ──────────────────────────────────────────────────

    event_loop(&state, &adapter_path, adapter_path_store).await
}

/// One `PropertiesChanged` emission, forwarded from a per-device
/// subscription's forwarder task to [`event_loop`] (which is where the device
/// map lives).
pub(super) struct DeviceProps {
    /// The object path the signal came from.
    pub(super) path: String,
    /// The body of the `PropertiesChanged` signal: `(iface, changed,
    /// invalidated)`.
    pub(super) body: zbus::Message,
}

/// Depth of the queue carrying "re-sync, please" wakes to [`event_loop`].
///
/// Small on purpose, for [`ResyncWake`]'s reason: every entry asks for exactly
/// the same thing, so a full queue is not backpressure to wait on — it is a
/// re-read already pending, which will observe whatever the dropped wake was
/// about. Mirrors `networkd_nm`'s `DEVICE_WAKE_QUEUE`.
const RESYNC_WAKE_QUEUE: usize = 8;

/// The "please re-read everything" end of [`event_loop`]'s resync channel.
///
/// Every marker source in this fold arms it — the three top-level
/// subscriptions and one forwarder per tracked device — and the loop drains it
/// before taking a single `GetManagedObjects`. That indirection is the point:
/// a single system-bus reconnect hands **every** subscription on the shared
/// connection its own `Resubscribed` (`hytte_bus`'s `signals.rs` advances one
/// epoch and each subscription re-subscribes off it), so before #1201's review
/// one blip cost `3 + N` `GetManagedObjects` round trips and `3 + N`
/// `Mutable::set` UI rebuilds — thirteen of each on a laptop with ten known
/// devices, all producing the same snapshot. Draining bounds a burst to two:
/// the one that runs, plus one for whatever arrives while it runs.
///
/// The shape is `networkd_nm`'s wake channel (`DeviceWatches`'s `wake_tx` and
/// the watcher's `while wake_rx.try_recv().is_ok() {}` drain), which is why
/// that consumer's fan-out was already `3 + 1` rather than `3 + N`.
type ResyncWake = tokio::sync::mpsc::Sender<()>;

/// Arm a coalesced re-sync. A full queue means one is already pending, so the
/// wake is dropped rather than awaited — nothing that asks for it can afford
/// to block behind a multi-round-trip re-read, and the pending one answers the
/// same question.
fn arm_resync(wake: &ResyncWake, source: &str) {
    if wake.try_send(()).is_err() {
        tracing::debug!(
            source,
            "bluetooth: a re-sync is already pending; dropping this wake"
        );
    }
}

/// One coalesced re-sync: drain every wake queued behind the one just
/// received, then re-read once. See [`ResyncWake`] for why this exists and
/// what it bounds.
///
/// `resync` is injectable so a test can count re-reads without a system bus.
async fn drain_and_resync<F, Fut>(
    wake_rx: &mut tokio::sync::mpsc::Receiver<()>,
    resync: F,
) -> Result<(), anyhow::Error>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), anyhow::Error>>,
{
    while wake_rx.try_recv().is_ok() {}
    resync().await
}

/// Re-sync all bluetooth state from a fresh `GetManagedObjects` snapshot —
/// the repair for a `Resubscribed`/`Lagged` marker on any of this fold's
/// four signal subscriptions (the three top-level ones plus one per tracked
/// device, #1201). Between a subscription dying and its replacement going
/// up, an `InterfacesAdded`/`InterfacesRemoved`/`PropertiesChanged` emission
/// for it is gone for good — `GetManagedObjects` is the only source broad
/// enough to repair whichever one it was. Reconciles the adapter, replaces
/// the device map wholesale (batteries included), and diffs the per-device
/// subscription set against it, exactly like the initial [`listen`] snapshot
/// does. Mirrors [`handle_ifaces_removed`]'s "adapter removed" branch when
/// the adapter itself is gone: same cleanup, same `Err` to force a full
/// reconnect.
async fn resync(
    state: &State,
    adapter_path: &str,
    props_tx: &tokio::sync::mpsc::UnboundedSender<DeviceProps>,
    resync_tx: &ResyncWake,
    device_subs: &mut HashMap<String, SignalSubscription>,
) -> Result<(), anyhow::Error> {
    let managed = get_managed_objects()
        .await
        .map_err(|e| anyhow::anyhow!("GetManagedObjects (resync): {e}"))?;

    let Some(adapter_props) = managed
        .get(&zbus::zvariant::OwnedObjectPath::try_from(adapter_path)?)
        .and_then(|ifaces| ifaces.get("org.bluez.Adapter1"))
    else {
        tracing::warn!(
            path = adapter_path,
            "bluetooth: adapter missing on resync — reconnecting"
        );
        state.adapter.set(None);
        state.devices_map.lock().await.clear();
        state.devices.set(Vec::new());
        device_subs.clear();
        return Err(anyhow::anyhow!("adapter removed"));
    };
    state
        .adapter
        .set(Some(parse_adapter_props(adapter_path, adapter_props)));

    let mut new_map = HashMap::new();
    for (obj_path, ifaces) in &managed {
        let p = obj_path.as_str();
        if p.starts_with(adapter_path)
            && let Some(dev_props) = ifaces.get("org.bluez.Device1")
        {
            let mut dev = parse_device_props(p, dev_props);
            if let Some(bat_props) = ifaces.get("org.bluez.Battery1") {
                dev.battery = property::<u8>(bat_props, "Percentage");
            }
            new_map.insert(p.to_string(), dev);
        }
    }

    // Diff the subscription set: drop a device that's gone, subscribe one
    // that's new. Dropping the map's handle is the whole teardown — it is the
    // last `SignalSubscription` for that device, so the tracker hits zero, the
    // subscription task exits and removes its match rule, and the forwarder's
    // stream ends with it. That is only true because the forwarder holds the
    // *stream* and not a handle; see `subscribe_device_props`.
    device_subs.retain(|path, _| new_map.contains_key(path));
    for path in new_map.keys() {
        if !device_subs.contains_key(path) {
            let sub = subscribe_device_props(path, props_tx.clone(), resync_tx.clone());
            device_subs.insert(path.clone(), sub);
        }
    }

    *state.devices_map.lock().await = new_map;
    state.publish_devices().await;

    Ok(())
}

/// Shared reaction to one item from any of bluetooth's three top-level
/// signal subscriptions (`InterfacesAdded`, `InterfacesRemoved`, adapter
/// `PropertiesChanged`): hand an ordinary emission's event back to the caller,
/// and arm a coalesced [`resync`] for a `Resubscribed`/`Lagged` marker instead
/// of dropping it (#1201) — the repair `resync` performs is the same
/// regardless of which of the three subscriptions went stale, which is exactly
/// why the three (and every per-device forwarder) share one wake rather than
/// each taking its own `GetManagedObjects`. See [`ResyncWake`].
///
/// Arming is synchronous and infallible, so unlike the `FnOnce`-injected
/// version this replaces there is nothing here for a `resync` error to
/// propagate through: the re-read now runs in [`event_loop`]'s wake arm, and
/// its failure (a transient `GetManagedObjects`, or the adapter genuinely
/// gone) propagates from there, same as before — the supervised loop backs off
/// and restarts [`listen`] from scratch.
fn bluetooth_marker_or_event(
    item: SignalItem,
    source: &'static str,
    wake: &ResyncWake,
) -> Option<SignalEvent> {
    match item {
        SignalItem::Event(evt) => Some(evt),
        SignalItem::Resubscribed | SignalItem::Lagged { .. } => {
            tracing::info!(source, "bluetooth signal resubscribed; re-syncing devices");
            arm_resync(wake, source);
            None
        }
    }
}

async fn event_loop(
    state: &State,
    adapter_path: &str,
    adapter_path_store: &Arc<tokio::sync::RwLock<String>>,
) -> Result<(), anyhow::Error> {
    // Subscribe to ObjectManager signals on the root path.
    let ifaces_added_sub = hytte_bus::signals(BusKind::System, "org.bluez")
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .signal("InterfacesAdded")
        .start();

    let ifaces_removed_sub = hytte_bus::signals(BusKind::System, "org.bluez")
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .signal("InterfacesRemoved")
        .start();

    // Subscribe to PropertiesChanged on the adapter path.
    let adapter_props_sub = hytte_bus::signals(BusKind::System, "org.bluez")
        .at_path(adapter_path.to_string())
        .iface("org.freedesktop.DBus.Properties")
        .signal("PropertiesChanged")
        .start();

    // Channel for device-level PropertiesChanged events forwarded from
    // per-device subscriptions (added/removed as devices appear/disappear).
    let (props_tx, mut props_rx) = tokio::sync::mpsc::unbounded_channel::<DeviceProps>();

    // One coalescing wake for every "re-read everything" request in this fold,
    // whichever subscription noticed the hole. See `ResyncWake`.
    let (resync_tx, mut resync_rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);

    // Subscribe PropertiesChanged for all devices already in the map.
    let mut device_subs: HashMap<String, SignalSubscription> = {
        let map = state.devices_map.lock().await;
        map.keys()
            .map(|p| {
                let sub = subscribe_device_props(p, props_tx.clone(), resync_tx.clone());
                (p.clone(), sub)
            })
            .collect()
    };

    let mut ifaces_added_items = ifaces_added_sub.items();
    let mut ifaces_removed_items = ifaces_removed_sub.items();
    let mut adapter_props_items = adapter_props_sub.items();

    loop {
        tokio::select! {
            item = ifaces_added_items.next() => {
                let Some(item) = item else { break; };
                let evt = bluetooth_marker_or_event(item, "InterfacesAdded", &resync_tx);
                let Some(evt) = evt else { continue; };
                let added = handle_ifaces_added(
                    state,
                    adapter_path,
                    adapter_path_store,
                    evt.body,
                    props_tx.clone(),
                    resync_tx.clone(),
                    &mut device_subs,
                ).await;
                let _ = added; // result used inside handler
            }

            item = ifaces_removed_items.next() => {
                let Some(item) = item else { break; };
                let evt = bluetooth_marker_or_event(item, "InterfacesRemoved", &resync_tx);
                let Some(evt) = evt else { continue; };
                if handle_ifaces_removed(state, adapter_path, evt.body, &mut device_subs).await {
                    return Err(anyhow::anyhow!("adapter removed"));
                }
            }

            item = adapter_props_items.next() => {
                let Some(item) = item else { break; };
                let evt = bluetooth_marker_or_event(item, "adapter PropertiesChanged", &resync_tx);
                let Some(evt) = evt else { continue; };
                handle_adapter_props_changed(state, adapter_path, &evt.body);
            }

            Some(dev_evt) = props_rx.recv() => {
                handle_device_props_changed(state, &dev_evt.path, &dev_evt.body).await;
            }

            // One `GetManagedObjects` per burst of markers, however many
            // subscriptions noticed the same hole. See `ResyncWake`.
            Some(()) = resync_rx.recv() => {
                drain_and_resync(&mut resync_rx, || {
                    resync(state, adapter_path, &props_tx, &resync_tx, &mut device_subs)
                }).await?;
            }
        }
    }

    Ok(())
}

/// Subscribe to `PropertiesChanged` on a single device path, forwarding
/// emissions to `tx` and arming a coalesced [`resync`] on a
/// `Resubscribed`/`Lagged` marker (#1201). Returns the `SignalSubscription`
/// handle — **the only one** — so dropping it cancels the subscription.
///
/// "The only one" is the fix for a leak this fold's re-read would otherwise
/// have turned into per-reconnect work (#1201 review M4). Until then the
/// forwarder was handed `sub.clone()`, and that clone lived as long as the
/// task: `HandleTracker` only wakes the subscription task when its count
/// reaches **zero**, and the task only exits on `all_dropped()`, so removing
/// the map's handle tore nothing down. The match rule stayed, the forwarder
/// stayed, and after `k` devices had come and gone each reconnect cost
/// `3 + N_live + k` `GetManagedObjects` instead of `3 + N_live` — while the
/// diff in [`resync`] documented the opposite.
///
/// `items()` is `+ use<>` and owns only its `broadcast::Receiver`, so handing
/// the forwarder the *stream* instead of a handle makes the map's handle the
/// last one, and `Drop` means what its doc says.
fn subscribe_device_props(
    device_path: &str,
    tx: tokio::sync::mpsc::UnboundedSender<DeviceProps>,
    resync_tx: ResyncWake,
) -> SignalSubscription {
    let path_str = device_path.to_string();
    let sub = hytte_bus::signals(BusKind::System, "org.bluez")
        .at_path(path_str.clone())
        .iface("org.freedesktop.DBus.Properties")
        .signal("PropertiesChanged")
        .start();

    // Detached deliberately: the forwarder's shutdown is its stream ending,
    // which is what dropping `sub` causes. The handle is returned only so a
    // test can await that.
    drop(spawn_device_forwarder(path_str, sub.items(), tx, resync_tx));

    sub
}

/// Spawn the forwarder task for one device subscription.
///
/// Takes the item **stream**, never the [`SignalSubscription`] it came from —
/// that is the contract [`subscribe_device_props`]'s doc explains, and taking
/// the stream by value is what enforces it rather than merely asking for it.
/// The task exits when the stream ends (the subscription was torn down) or the
/// channel is closed (the event loop is gone).
///
/// The stream's item type is also the guard on the `items()`/`events()` choice
/// this fold depends on: `events()` yields `SignalEvent`, which does not fit.
fn spawn_device_forwarder<S>(
    path: String,
    mut items: S,
    tx: tokio::sync::mpsc::UnboundedSender<DeviceProps>,
    resync_tx: ResyncWake,
) -> tokio::task::JoinHandle<()>
where
    S: futures_util::Stream<Item = SignalItem> + Unpin + Send + 'static,
{
    runtime::handle().spawn(async move {
        while let Some(item) = items.next().await {
            match item {
                SignalItem::Event(evt) => {
                    if tx
                        .send(DeviceProps {
                            path: path.clone(),
                            body: evt.body,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                SignalItem::Resubscribed | SignalItem::Lagged { .. } => {
                    tracing::info!(
                        path,
                        "bluetooth device PropertiesChanged resubscribed; re-syncing devices"
                    );
                    arm_resync(&resync_tx, "device PropertiesChanged");
                    if resync_tx.is_closed() {
                        break;
                    }
                }
            }
        }
        tracing::debug!(path, "bluetooth: per-device signal stream ended");
    })
}

async fn handle_ifaces_added(
    state: &State,
    adapter_path: &str,
    adapter_path_store: &Arc<tokio::sync::RwLock<String>>,
    msg: zbus::Message,
    props_tx: tokio::sync::mpsc::UnboundedSender<DeviceProps>,
    resync_tx: ResyncWake,
    device_subs: &mut HashMap<String, SignalSubscription>,
) -> bool {
    let Ok((path, ifaces)) = msg.body().deserialize::<(
        zbus::zvariant::OwnedObjectPath,
        HashMap<String, HashMap<String, OwnedValue>>,
    )>() else {
        return false;
    };

    let p = path.as_str();
    if ifaces.contains_key("org.bluez.Adapter1") && adapter_path.is_empty() {
        // New adapter appeared while we have none (edge case).
        if let Some(aprops) = ifaces.get("org.bluez.Adapter1") {
            let a = parse_adapter_props(p, aprops);
            state.adapter.set(Some(a));
            set_adapter_path(p, adapter_path_store).await;
        }
    }

    if p.starts_with(adapter_path)
        && let Some(dev_props) = ifaces.get("org.bluez.Device1")
    {
        let mut dev = parse_device_props(p, dev_props);
        if let Some(bat_props) = ifaces.get("org.bluez.Battery1") {
            dev.battery = property::<u8>(bat_props, "Percentage");
        }
        tracing::debug!(path = p, alias = dev.alias, "device added");
        state.devices_map.lock().await.insert(p.to_string(), dev);
        state.publish_devices().await;

        // Register a PropertiesChanged subscription for this new device.
        if !device_subs.contains_key(p) {
            let sub = subscribe_device_props(p, props_tx, resync_tx);
            device_subs.insert(p.to_string(), sub);
        }

        return true;
    }

    // Battery1 may appear *after* Device1 (added when device connects) on
    // its existing path. Update the stored device with the percentage.
    if p.starts_with(adapter_path)
        && let Some(bat_props) = ifaces.get("org.bluez.Battery1")
    {
        let pct = property::<u8>(bat_props, "Percentage");
        let mut map = state.devices_map.lock().await;
        if let Some(dev) = map.get_mut(p) {
            dev.battery = pct;
        }
        drop(map);
        state.publish_devices().await;
    }

    false
}

/// Returns `true` when the adapter was removed (caller should reconnect).
async fn handle_ifaces_removed(
    state: &State,
    adapter_path: &str,
    msg: zbus::Message,
    device_subs: &mut HashMap<String, SignalSubscription>,
) -> bool {
    let Ok((path, removed_ifaces)) = msg
        .body()
        .deserialize::<(zbus::zvariant::OwnedObjectPath, Vec<String>)>()
    else {
        return false;
    };

    let p = path.as_str();

    if removed_ifaces.iter().any(|i| i == "org.bluez.Adapter1") && p == adapter_path {
        tracing::warn!(path = p, "adapter removed — reconnecting");
        state.adapter.set(None);
        state.devices_map.lock().await.clear();
        state.devices.set(Vec::new());
        device_subs.clear();
        return true;
    }

    if removed_ifaces.iter().any(|i| i == "org.bluez.Device1") {
        tracing::debug!(path = p, "device removed");
        state.devices_map.lock().await.remove(p);
        // Drop the PropertiesChanged subscription for this device — the last
        // handle, so this really does tear it down (see
        // `subscribe_device_props`).
        device_subs.remove(p);
        state.publish_devices().await;
    } else if removed_ifaces.iter().any(|i| i == "org.bluez.Battery1") {
        // Device still exists, but Battery1 went away (typically on
        // disconnect). Clear the percentage on the stored device.
        let mut map = state.devices_map.lock().await;
        if let Some(dev) = map.get_mut(p) {
            dev.battery = None;
        }
        drop(map);
        state.publish_devices().await;
    }

    false
}

fn handle_adapter_props_changed(state: &State, adapter_path: &str, msg: &zbus::Message) {
    let Ok((iface_name, changed, _)) = msg
        .body()
        .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
    else {
        return;
    };

    if iface_name == "org.bluez.Adapter1" {
        let _ = adapter_path; // already filtered by subscription path
        state.apply_adapter_props(&changed);
    }
}

async fn handle_device_props_changed(state: &State, path: &str, body: &zbus::Message) {
    let Ok((iface_name, changed, _)) =
        body.body()
            .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
    else {
        return;
    };

    if iface_name == "org.bluez.Device1" {
        state.apply_device_props(path, &changed).await;
        state.publish_devices().await;
    } else if iface_name == "org.bluez.Battery1" {
        let mut map = state.devices_map.lock().await;
        if let Some(dev) = map.get_mut(path)
            && changed.contains_key("Percentage")
        {
            dev.battery = property::<u8>(&changed, "Percentage");
        }
        drop(map);
        state.publish_devices().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── #1201: bluetooth_marker_or_event routes markers to a resync ─────────

    use std::sync::atomic::{AtomicUsize, Ordering};

    fn signal_event() -> SignalEvent {
        let body = zbus::Message::signal("/t", "t.I", "Ping")
            .expect("signal builder")
            .build(&42u32)
            .expect("build signal message");
        SignalEvent {
            body,
            sender: None,
            timestamp: std::time::SystemTime::now(),
        }
    }

    /// Before #1201 `event_loop` read these three top-level subscriptions
    /// via `events()`, which cannot represent either marker, so a bus blip
    /// left device add/remove and adapter-property state stale until
    /// something else happened to refresh it. Pushing one marker through
    /// `bluetooth_marker_or_event` must arm exactly one re-sync — for both
    /// `Resubscribed` and a broadcast `Lagged`.
    ///
    /// Falsifiable: deleting the `Resubscribed | Lagged { .. }` arm (or
    /// making it a no-op) leaves the wake queue empty.
    #[tokio::test(flavor = "current_thread")]
    async fn marker_arms_resync_exactly_once() {
        for item in [SignalItem::Resubscribed, SignalItem::Lagged { skipped: 9 }] {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
            let evt = bluetooth_marker_or_event(item, "test", &tx);
            assert!(evt.is_none(), "a marker must not be forwarded as an event");
            assert!(rx.try_recv().is_ok(), "a marker must arm a re-sync");
            assert!(rx.try_recv().is_err(), "exactly one, not two");
        }
    }

    /// An ordinary emission is unaffected: it must still reach the caller as
    /// an event, byte-identical to before #1201, and never ask for a re-sync.
    #[tokio::test(flavor = "current_thread")]
    async fn an_ordinary_event_is_forwarded_and_does_not_resync() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
        let evt = bluetooth_marker_or_event(SignalItem::Event(signal_event()), "test", &tx);
        assert!(evt.is_some(), "an ordinary event must be forwarded");
        assert!(rx.try_recv().is_err());
    }

    // ── #1201 review M3: one re-read per burst, not one per subscription ────

    /// A single system-bus reconnect gives **every** subscription its own
    /// `Resubscribed`, so the three top-level ones plus one per known device
    /// all ask for the same `GetManagedObjects` at once. The wake arm must
    /// collapse that burst into one re-read (plus at most one more for
    /// whatever arrives while it runs), the way `networkd_nm`'s per-device
    /// wake channel already did.
    ///
    /// Falsifiable: deleting the `while wake_rx.try_recv().is_ok() {}` drain
    /// in `drain_and_resync` leaves the other two wakes queued, so the
    /// "nothing left to do" assertion fails.
    #[tokio::test(flavor = "current_thread")]
    async fn a_burst_of_markers_costs_one_resync() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);

        // Three subscriptions notice the same reconnect.
        for source in ["InterfacesAdded", "InterfacesRemoved", "device 1"] {
            assert!(
                bluetooth_marker_or_event(SignalItem::Resubscribed, "test", &tx).is_none(),
                "{source} arms a wake"
            );
        }

        // The loop's wake arm: one `recv`, then the coalesced re-read.
        let reads = Arc::new(AtomicUsize::new(0));
        rx.recv().await.expect("a wake is queued");
        {
            let reads = reads.clone();
            drain_and_resync(&mut rx, || async move {
                reads.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .expect("counter never errors");
        }

        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "three markers must cost one GetManagedObjects, not three"
        );
        assert!(
            rx.try_recv().is_err(),
            "and leave nothing queued to re-read again"
        );
    }

    /// A wake that arrives *while* a re-read is running is not swallowed: the
    /// next loop iteration picks it up, so a change the in-flight snapshot
    /// might have missed still gets a read of its own.
    #[tokio::test(flavor = "current_thread")]
    async fn a_wake_arriving_during_a_resync_survives() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
        arm_resync(&tx, "test");
        rx.recv().await.expect("a wake is queued");

        let tx2 = tx.clone();
        drain_and_resync(&mut rx, || async move {
            arm_resync(&tx2, "during the read");
            Ok(())
        })
        .await
        .expect("counter never errors");

        assert!(
            rx.try_recv().is_ok(),
            "a wake armed during the re-read must still be pending afterwards"
        );
    }

    /// The forwarder's own shutdown paths. The half that matters for M4 — it
    /// is handed a stream and never a `SignalSubscription`, so it cannot keep
    /// one alive — is enforced by the signature; what a test can still pin is
    /// that the task actually ends on each of its two exits, rather than
    /// spinning forever on a channel nobody reads.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_forwarder_stops_when_its_stream_ends_or_nobody_listens() {
        let (props_tx, props_rx) = tokio::sync::mpsc::unbounded_channel::<DeviceProps>();
        let (resync_tx, resync_rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);

        // 1. The subscription was torn down: the stream ends, so does the task.
        let ended = spawn_device_forwarder(
            "/dev/gone".to_string(),
            futures_util::stream::empty::<SignalItem>(),
            props_tx.clone(),
            resync_tx.clone(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), ended)
            .await
            .expect("a forwarder whose stream ended must exit")
            .expect("forwarder task panicked");

        // 2. The event loop is gone: an endless stream of markers must not
        //    keep the task (or a busy loop) alive.
        drop(props_rx);
        drop(resync_rx);
        let orphaned = spawn_device_forwarder(
            "/dev/x".to_string(),
            futures_util::stream::repeat(SignalItem::Resubscribed),
            props_tx,
            resync_tx,
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), orphaned)
            .await
            .expect("a forwarder nobody listens to must give up")
            .expect("forwarder task panicked");
    }

    fn make_owned_value<T: Into<zbus::zvariant::Value<'static>>>(v: T) -> OwnedValue {
        v.into()
            .try_to_owned()
            .expect("test value must be serialisable")
    }

    fn device(path: &str, alias: &str, connected: bool, paired: bool) -> Device {
        Device {
            path: path.to_string(),
            alias: alias.to_string(),
            connected,
            paired,
            ..Device::default()
        }
    }

    // ── publish_devices sort comparator ────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn publish_devices_sorts_connected_first_then_paired_then_alias() {
        let state = State::new(Mutable::new(None), Mutable::new(Vec::new()));
        {
            let mut map = state.devices_map.lock().await;
            map.insert("/dev/a".to_string(), device("/dev/a", "Zeta", false, false));
            map.insert("/dev/b".to_string(), device("/dev/b", "Alpha", true, false));
            map.insert("/dev/c".to_string(), device("/dev/c", "Beta", false, true));
        }
        state.publish_devices().await;

        let list = state.devices.get_cloned();
        let aliases: Vec<&str> = list.iter().map(|d| d.alias.as_str()).collect();
        // Connected wins first ("Alpha"), then paired-but-not-connected
        // ("Beta"), then neither, alphabetically ("Zeta").
        assert_eq!(aliases, vec!["Alpha", "Beta", "Zeta"]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn publish_devices_alias_tiebreak_is_case_insensitive() {
        let state = State::new(Mutable::new(None), Mutable::new(Vec::new()));
        {
            let mut map = state.devices_map.lock().await;
            map.insert(
                "/dev/a".to_string(),
                device("/dev/a", "banana", false, false),
            );
            map.insert(
                "/dev/b".to_string(),
                device("/dev/b", "Apple", false, false),
            );
        }
        state.publish_devices().await;

        let list = state.devices.get_cloned();
        let aliases: Vec<&str> = list.iter().map(|d| d.alias.as_str()).collect();
        assert_eq!(aliases, vec!["Apple", "banana"]);
    }

    // ── apply_adapter_props ─────────────────────────────────────────────────

    #[test]
    fn apply_adapter_props_updates_only_changed_fields() {
        let initial = Adapter {
            path: "/org/bluez/hci0".to_string(),
            address: "AA:BB:CC:DD:EE:FF".to_string(),
            name: "old-name".to_string(),
            powered: false,
            discoverable: false,
            discovering: false,
        };
        let state = State::new(Mutable::new(Some(initial)), Mutable::new(Vec::new()));

        let mut changed = HashMap::new();
        changed.insert("Powered".to_string(), make_owned_value(true));
        changed.insert("Name".to_string(), make_owned_value("new-name"));

        state.apply_adapter_props(&changed);

        let adapter = state.adapter.get_cloned().expect("adapter still present");
        assert!(adapter.powered, "Powered was in the changed set");
        assert_eq!(adapter.name, "new-name", "Name was in the changed set");
        // Fields absent from `changed` survive the partial update untouched.
        assert_eq!(adapter.address, "AA:BB:CC:DD:EE:FF");
        assert!(!adapter.discoverable);
        assert!(!adapter.discovering);
    }

    #[test]
    fn apply_adapter_props_no_op_when_adapter_absent() {
        let state = State::new(Mutable::new(None), Mutable::new(Vec::new()));
        let mut changed = HashMap::new();
        changed.insert("Powered".to_string(), make_owned_value(true));

        // Must not panic when there's no adapter snapshot yet to update.
        state.apply_adapter_props(&changed);

        assert!(state.adapter.get_cloned().is_none());
    }

    // ── apply_device_props ──────────────────────────────────────────────────

    #[tokio::test(flavor = "current_thread")]
    async fn apply_device_props_updates_only_changed_fields() {
        let state = State::new(Mutable::new(None), Mutable::new(Vec::new()));
        {
            let mut map = state.devices_map.lock().await;
            map.insert(
                "/dev/x".to_string(),
                Device {
                    path: "/dev/x".to_string(),
                    address: "11:22:33:44:55:66".to_string(),
                    alias: "old-alias".to_string(),
                    icon: "old-icon".to_string(),
                    paired: false,
                    connected: false,
                    trusted: false,
                    battery: None,
                },
            );
        }

        let mut changed = HashMap::new();
        changed.insert("Connected".to_string(), make_owned_value(true));
        changed.insert("Alias".to_string(), make_owned_value("new-alias"));

        state.apply_device_props("/dev/x", &changed).await;

        let map = state.devices_map.lock().await;
        let dev = map.get("/dev/x").expect("device still present");
        assert!(dev.connected, "Connected was in the changed set");
        assert_eq!(dev.alias, "new-alias", "Alias was in the changed set");
        // Fields absent from `changed` survive the partial update untouched.
        assert_eq!(dev.address, "11:22:33:44:55:66");
        assert_eq!(dev.icon, "old-icon");
        assert!(!dev.paired);
        assert!(!dev.trusted);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn apply_device_props_no_op_for_unknown_path() {
        let state = State::new(Mutable::new(None), Mutable::new(Vec::new()));
        let mut changed = HashMap::new();
        changed.insert("Connected".to_string(), make_owned_value(true));

        // Must not panic when the path isn't in the map (e.g. a stale event
        // for a device that was already removed).
        state.apply_device_props("/dev/unknown", &changed).await;

        assert!(state.devices_map.lock().await.is_empty());
    }
}

// The teardown contract `subscribe_device_props` documents — "dropping the
// map's handle tears the subscription down" — is a property of `hytte_bus`'s
// `HandleTracker`, and the only way to observe it is to drop the last handle of
// a **real** subscription and watch its task exit. That needs a broker, so this
// module is gated behind the `system-tests` cargo feature like every other
// dbus-daemon test in this crate; `wifi/nm_agent.rs`'s `system_tests` is the
// template and the `BusGuard`/`ephemeral_bus` harness below is that one, same
// shape (`screensaver.rs` carries the same copy — the harness is duplicated per
// module rather than shared, which is the house pattern here). Run with:
//   cargo test -p hytte-services --features system-tests --lib bluetooth
#[cfg(all(test, feature = "system-tests"))]
mod system_tests {
    use super::{DeviceProps, RESYNC_WAKE_QUEUE, spawn_device_forwarder};

    use hytte_bus::signals_with;
    use hytte_bus::test_support::SharedConnection;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::{Child, Command};
    use zbus::connection::Builder;

    /// Upper bound on any single round trip, or on waiting for a subscription
    /// task to notice it has no handles left. A liveness guard, not a latency
    /// assertion — the same reasoning and the same number as
    /// `wifi::nm_agent`'s budget: this runs inside `nix flake check` next to
    /// two nixosTest VMs and the whole workspace clippy, where CPU contention
    /// is the normal condition.
    const DBUS_REPLY_BUDGET: Duration = Duration::from_secs(30);

    /// Kills the spawned `dbus-daemon` on drop. Mirrors `hytte-bus`'s
    /// `BusGuard`: SIGKILL + a `block_in_place` wait so the socket `TempDir`
    /// outlives the process (hence the multi-thread runtime requirement on
    /// every test here).
    struct BusGuard {
        child: Option<Child>,
        _tmp: TempDir,
        address: String,
    }

    impl Drop for BusGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.start_kill();
                tokio::task::block_in_place(|| {
                    let handle = tokio::runtime::Handle::current();
                    let _ = handle.block_on(child.wait());
                });
            }
        }
    }

    /// Spawn a fresh `dbus-daemon` and return a guard that kills it on drop.
    async fn ephemeral_bus() -> BusGuard {
        let tmp = TempDir::new().expect("create tempdir for dbus-daemon");
        let socket: PathBuf = tmp.path().join("bus");
        let address = format!("unix:path={}", socket.display());

        let config = tmp.path().join("session.conf");
        std::fs::write(
            &config,
            format!(
                r#"<?xml version="1.0"?>
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>{address}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#
            ),
        )
        .expect("write dbus-daemon config");

        let mut child = Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(&config)
            .arg("--print-address=1")
            .arg("--nofork")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn dbus-daemon — install package `dbus` if missing");

        let stdout = child.stdout.take().expect("dbus-daemon stdout");
        let mut lines = BufReader::new(stdout).lines();
        let printed = tokio::time::timeout(DBUS_REPLY_BUDGET, lines.next_line())
            .await
            .expect("dbus-daemon address timeout")
            .expect("dbus-daemon read address")
            .expect("dbus-daemon closed stdout");
        assert!(
            printed.contains("unix:path="),
            "unexpected dbus-daemon address: {printed}"
        );

        BusGuard {
            child: Some(child),
            _tmp: tmp,
            address,
        }
    }

    /// Dropping the `device_subs` entry for a departed device must actually
    /// tear its subscription down — which means `HandleTracker`'s count has to
    /// reach **zero**, and the subscription task's exit probe is the only
    /// honest way to see that.
    ///
    /// This is #1201 review M4. Until it was fixed the forwarder held a
    /// `sub.clone()` for the life of the task, so the count never reached
    /// zero: `resync`'s `device_subs.retain` (and `handle_ifaces_removed`'s
    /// `remove`) left the match rule installed and the forwarder running, and
    /// every later reconnect re-synced once more per dead device on top of the
    /// live ones.
    ///
    /// Falsifiable: give `spawn_device_forwarder` the `SignalSubscription`
    /// instead of `sub.items()` and clone it into the task — this times out.
    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_a_device_subscription_tears_it_down() {
        let guard = ephemeral_bus().await;
        let conn = Builder::address(guard.address.as_str())
            .expect("bus address")
            .build()
            .await
            .expect("connect to ephemeral bus");
        let shared = SharedConnection::for_test_system(conn);

        let sub = signals_with(&shared, "org.bluez")
            .at_path("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF")
            .iface("org.freedesktop.DBus.Properties")
            .signal("PropertiesChanged")
            .start();

        // Take the exit probe while we still hold the only handle.
        let done_rx = sub
            .task_done_receiver()
            .await
            .expect("task_done_receiver is Some on first call");

        let (props_tx, _props_rx) = tokio::sync::mpsc::unbounded_channel::<DeviceProps>();
        let (resync_tx, _resync_rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
        let forwarder = spawn_device_forwarder(
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF".to_string(),
            sub.items(),
            props_tx,
            resync_tx,
        );

        // Let the subscription task and the forwarder both get scheduled, so
        // this exercises the "already parked in select!" path rather than only
        // the check at the top of the loop.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // What `device_subs.retain(...)` does: drop the map's handle. If the
        // forwarder held a clone, the count would not reach zero here.
        drop(sub);

        tokio::time::timeout(DBUS_REPLY_BUDGET, done_rx)
            .await
            .expect("subscription task did not exit after its last handle was dropped")
            .expect("task_done_tx dropped without sending — task panicked?");

        // And the forwarder goes with it: its stream ends when the
        // subscription task drops the broadcast sender.
        tokio::time::timeout(DBUS_REPLY_BUDGET, forwarder)
            .await
            .expect("forwarder did not exit with its subscription")
            .expect("forwarder task panicked");
    }
}
