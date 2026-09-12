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

/// A message forwarded from a per-device subscription's forwarder task to
/// [`event_loop`]: either a `PropertiesChanged` emission, or the notice that
/// this device's subscription hit a [`SignalItem::Resubscribed`]/
/// [`SignalItem::Lagged`] marker and needs the shared [`resync`] (#1201) —
/// the forwarder can't call `resync` itself, since it doesn't own
/// `device_subs`.
pub(super) enum PropChangedEvent {
    /// A `PropertiesChanged` emission. `body` is `(iface, changed,
    /// invalidated)`.
    Changed {
        /// The object path the signal came from.
        path: String,
        /// The body of the `PropertiesChanged` signal.
        body: zbus::Message,
    },
    /// This device's subscription needs a re-sync.
    Resync,
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
    props_tx: &tokio::sync::mpsc::UnboundedSender<PropChangedEvent>,
    device_subs: &mut HashMap<String, SignalSubscription>,
) -> Result<(), anyhow::Error> {
    let managed = get_managed_objects()
        .await
        .map_err(|e| anyhow::anyhow!("GetManagedObjects (resync): {e}"))?;

    let Some(adapter_props) = managed
        .get(&zbus::zvariant::OwnedObjectPath::try_from(adapter_path)?)
        .and_then(|ifaces| ifaces.get("org.bluez.Adapter1"))
    else {
        tracing::warn!(path = adapter_path, "bluetooth: adapter missing on resync — reconnecting");
        state.adapter.set(None);
        state.devices_map.lock().await.clear();
        state.devices.set(Vec::new());
        device_subs.clear();
        return Err(anyhow::anyhow!("adapter removed"));
    };
    state.adapter.set(Some(parse_adapter_props(adapter_path, adapter_props)));

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

    // Diff the subscription set: drop a device that's gone (its
    // SignalSubscription's Drop tears the match rule down), subscribe one
    // that's new.
    device_subs.retain(|path, _| new_map.contains_key(path));
    for path in new_map.keys() {
        if !device_subs.contains_key(path) {
            let sub = subscribe_device_props(path, props_tx.clone());
            device_subs.insert(path.clone(), sub);
        }
    }

    *state.devices_map.lock().await = new_map;
    state.publish_devices().await;

    Ok(())
}

/// Shared reaction to one item from any of bluetooth's three top-level
/// signal subscriptions (`InterfacesAdded`, `InterfacesRemoved`, adapter
/// `PropertiesChanged`): forward an ordinary emission's event to the caller,
/// and call `do_resync` once on a `Resubscribed`/`Lagged` marker instead of
/// dropping it (#1201) — the repair [`resync`] performs is the same
/// regardless of which of the three subscriptions went stale. A `resync`
/// failure (transient `GetManagedObjects`, or the adapter genuinely gone) is
/// propagated rather than swallowed, exactly like a `GetManagedObjects`
/// failure elsewhere in this file — the caller's supervised loop backs off
/// and restarts [`listen`] from scratch.
///
/// `do_resync` is `FnOnce`, not `FnMut`: each call site below constructs a
/// fresh closure per loop iteration and this function calls it at most once,
/// so the plain (pre-`AsyncFn*`) "closure returning a future that borrows
/// its captures" shape is sound here — unlike an `FnMut`/`AsyncFnMut` bound,
/// which (a discarded earlier draft found) hits a real rustc limitation
/// once `event_loop` is spawned via `spawn_supervised`
/// (`Send`-for-a-generic-`AsyncFnMut`-closure "not general enough"). Kept
/// injectable so a test can stand in for the real `GetManagedObjects` round
/// trip with a counter.
async fn bluetooth_marker_or_event<Resync, Fut>(
    item: SignalItem,
    source: &'static str,
    do_resync: Resync,
) -> Result<Option<SignalEvent>, anyhow::Error>
where
    Resync: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), anyhow::Error>>,
{
    match item {
        SignalItem::Event(evt) => Ok(Some(evt)),
        SignalItem::Resubscribed | SignalItem::Lagged { .. } => {
            tracing::info!(source, "bluetooth signal resubscribed; re-syncing devices");
            do_resync().await?;
            Ok(None)
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
    let (props_tx, mut props_rx) = tokio::sync::mpsc::unbounded_channel::<PropChangedEvent>();

    // Subscribe PropertiesChanged for all devices already in the map.
    let mut device_subs: HashMap<String, SignalSubscription> = {
        let map = state.devices_map.lock().await;
        map.keys()
            .map(|p| {
                let sub = subscribe_device_props(p, props_tx.clone());
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
                let evt = bluetooth_marker_or_event(item, "InterfacesAdded", || {
                    resync(state, adapter_path, &props_tx, &mut device_subs)
                }).await?;
                let Some(evt) = evt else { continue; };
                let added = handle_ifaces_added(
                    state,
                    adapter_path,
                    adapter_path_store,
                    evt.body,
                    props_tx.clone(),
                    &mut device_subs,
                ).await;
                let _ = added; // result used inside handler
            }

            item = ifaces_removed_items.next() => {
                let Some(item) = item else { break; };
                let evt = bluetooth_marker_or_event(item, "InterfacesRemoved", || {
                    resync(state, adapter_path, &props_tx, &mut device_subs)
                }).await?;
                let Some(evt) = evt else { continue; };
                if handle_ifaces_removed(state, adapter_path, evt.body, &mut device_subs).await {
                    return Err(anyhow::anyhow!("adapter removed"));
                }
            }

            item = adapter_props_items.next() => {
                let Some(item) = item else { break; };
                let evt = bluetooth_marker_or_event(item, "adapter PropertiesChanged", || {
                    resync(state, adapter_path, &props_tx, &mut device_subs)
                }).await?;
                let Some(evt) = evt else { continue; };
                handle_adapter_props_changed(state, adapter_path, &evt.body);
            }

            Some(dev_evt) = props_rx.recv() => {
                match dev_evt {
                    PropChangedEvent::Changed { path, body } => {
                        handle_device_props_changed(state, &path, &body).await;
                    }
                    PropChangedEvent::Resync => {
                        tracing::info!(
                            source = "device PropertiesChanged",
                            "bluetooth signal resubscribed; re-syncing devices"
                        );
                        resync(state, adapter_path, &props_tx, &mut device_subs).await?;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Subscribe to `PropertiesChanged` on a single device path, forwarding
/// events (and Resubscribed/Lagged markers, as [`PropChangedEvent::Resync`],
/// #1201) to `tx`. Returns the `SignalSubscription` handle; dropping it
/// cancels the subscription.
fn subscribe_device_props(
    device_path: &str,
    tx: tokio::sync::mpsc::UnboundedSender<PropChangedEvent>,
) -> SignalSubscription {
    let path_str = device_path.to_string();
    let sub = hytte_bus::signals(BusKind::System, "org.bluez")
        .at_path(path_str.clone())
        .iface("org.freedesktop.DBus.Properties")
        .signal("PropertiesChanged")
        .start();

    // Spawn a forwarder task. It exits when the subscription is dropped
    // (event stream ends) or the channel is closed.
    let sub_clone = sub.clone();
    runtime::handle().spawn(async move {
        let mut items = sub_clone.items();
        while let Some(item) = items.next().await {
            let sent = match item {
                SignalItem::Event(evt) => tx.send(PropChangedEvent::Changed {
                    path: path_str.clone(),
                    body: evt.body,
                }),
                SignalItem::Resubscribed | SignalItem::Lagged { .. } => {
                    tx.send(PropChangedEvent::Resync)
                }
            };
            if sent.is_err() {
                break;
            }
        }
    });

    sub
}

async fn handle_ifaces_added(
    state: &State,
    adapter_path: &str,
    adapter_path_store: &Arc<tokio::sync::RwLock<String>>,
    msg: zbus::Message,
    props_tx: tokio::sync::mpsc::UnboundedSender<PropChangedEvent>,
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
            let sub = subscribe_device_props(p, props_tx);
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
        // Drop the PropertiesChanged subscription for this device.
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
    /// `bluetooth_marker_or_event` must call `do_resync` exactly once — for
    /// both `Resubscribed` and a broadcast `Lagged`.
    ///
    /// Falsifiable: deleting the `Resubscribed | Lagged { .. }` arm (or
    /// making it a no-op) drops both counts to 0.
    #[tokio::test(flavor = "current_thread")]
    async fn marker_triggers_resync_exactly_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let calls = calls.clone();
            let evt = bluetooth_marker_or_event(SignalItem::Resubscribed, "test", || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .expect("counter never errors");
            assert!(evt.is_none(), "a marker must not be forwarded as an event");
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Resubscribed must resync exactly once"
        );

        let calls = Arc::new(AtomicUsize::new(0));
        {
            let calls = calls.clone();
            let evt = bluetooth_marker_or_event(
                SignalItem::Lagged { skipped: 9 },
                "test",
                || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .expect("counter never errors");
            assert!(evt.is_none());
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Lagged must resync exactly once"
        );
    }

    /// An ordinary emission is unaffected: it must still reach the caller as
    /// an event, byte-identical to before #1201, and never touch `do_resync`.
    #[tokio::test(flavor = "current_thread")]
    async fn an_ordinary_event_is_forwarded_and_does_not_resync() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let evt = bluetooth_marker_or_event(SignalItem::Event(signal_event()), "test", || async move {
            calls2.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await
        .expect("no error path here");
        assert!(evt.is_some(), "an ordinary event must be forwarded");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
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
