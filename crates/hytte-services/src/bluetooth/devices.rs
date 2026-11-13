//! Device discovery and watcher: listens to `BlueZ` `ObjectManager` signals and
//! keeps the `adapter` and `devices` mutables up to date.

use super::parse::{parse_adapter_props, parse_device_props, prop_bool, prop_str, property};
use super::types::{Adapter, Device};
use crate::handshake::{Handshake, subscribe_then_snapshot};
use futures_signals::signal::Mutable;
use futures_util::{Stream, StreamExt};
use hytte_bus::{BusError, BusKind, SignalItem, SignalSubscription};
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

/// One object's interfaces and their properties, as `GetManagedObjects` and
/// `InterfacesAdded` carry them.
type Interfaces = HashMap<String, HashMap<String, OwnedValue>>;

pub(super) async fn get_managed_objects() -> Result<ManagedObjects, hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, "org.bluez")
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .method("GetManagedObjects")
        .args(())
        .send::<ManagedObjects>()
        .await
}

/// `Properties.GetAll(iface)` on one `org.bluez` object.
async fn get_all_props(
    path: &str,
    iface: &'static str,
) -> Result<HashMap<String, OwnedValue>, hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, "org.bluez")
        .at_path(path.to_string())
        .iface("org.freedesktop.DBus.Properties")
        .method("GetAll")
        .args((iface,))
        .send::<HashMap<String, OwnedValue>>()
        .await
}

pub(super) async fn set_adapter_path(path: &str, store: &Arc<tokio::sync::RwLock<String>>) {
    *store.write().await = path.to_string();
}

/// A device as a snapshot describes it: its `Device1` properties, and its
/// `Battery1` percentage when it has one.
fn device_from(
    path: &str,
    device: &HashMap<String, OwnedValue>,
    battery: Option<&HashMap<String, OwnedValue>>,
) -> Device {
    let mut dev = parse_device_props(path, device);
    if let Some(battery) = battery {
        dev.battery = property::<u8>(battery, "Percentage");
    }
    dev
}

/// Every device under `adapter_path` in a `GetManagedObjects` answer. What
/// [`listen`] starts from and what [`resync`] replaces the map with, so a
/// snapshot means the same thing in both.
fn devices_from_managed(managed: &ManagedObjects, adapter_path: &str) -> HashMap<String, Device> {
    managed
        .iter()
        .filter(|(obj_path, _)| obj_path.as_str().starts_with(adapter_path))
        .filter_map(|(obj_path, ifaces)| {
            let p = obj_path.as_str();
            let device = ifaces.get("org.bluez.Device1")?;
            Some((
                p.to_string(),
                device_from(p, device, ifaces.get("org.bluez.Battery1")),
            ))
        })
        .collect()
}

pub(super) async fn listen(
    adapter_mutable: &Mutable<Option<Adapter>>,
    devices_mutable: &Mutable<Vec<Device>>,
    adapter_path_store: &Arc<tokio::sync::RwLock<String>>,
) -> Result<ListenEnd, anyhow::Error> {
    // bluetoothd itself (see `on_owner_item`), subscribed **before** the
    // snapshot below and with its receiver taken right away, so an owner
    // change that lands after the snapshot is queued for `event_loop` rather
    // than emitted before anything listens (#1551). Until then the
    // subscription was made in `event_loop`, after the snapshot and the
    // first publish, and a bluetoothd restart in between left the old
    // daemon's snapshot on screen until the next owner change.
    //
    // Replaying an owner change that predates the snapshot is harmless: a
    // `Lost` ends this watch and the supervised loop waits for the next
    // owner, an `Appeared` arms one extra re-sync. The fold's own
    // subscriptions cannot move up here, since the adapter's and the
    // devices' paths come from this snapshot; `event_loop` closes their
    // window differently, by waiting for them and then reading again (see
    // `fold`'s "Ordering").
    //
    // `SignalsBuilder::start` installs its match rule from a spawned task,
    // so `subscribe_then_snapshot` waits for the bus to have it before it
    // asks for the snapshot: `GetManagedObjects` is sent after the `AddMatch`
    // was answered. The wait has no deadline. With the system bus down it
    // stays here, where the snapshot would have failed into the supervised
    // loop's backoff, and resumes when the subscription is back.
    let ((owner_sub, owner_items), managed) = subscribe_then_snapshot(
        || {
            let sub = name_owner_changed();
            let items = sub.items();
            (sub, items)
        },
        get_managed_objects,
    )
    .await;
    let managed = managed.map_err(|e| anyhow::anyhow!("GetManagedObjects: {e}"))?;

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
    *state.devices_map.lock().await = devices_from_managed(&managed, &adapter_path);
    state.publish_devices().await;

    // ── Signal subscriptions ──────────────────────────────────────────────────

    let end = event_loop(&state, &adapter_path, owner_items).await;
    // The owner stream ends when its last subscription handle goes, so the
    // handle has to outlive the loop that reads it.
    drop(owner_sub);
    end
}

/// What a `NameOwnerChanged` for `org.bluez` says about bluetoothd.
#[derive(Debug, PartialEq, Eq)]
enum OwnerChange {
    /// `org.bluez` has no owner any more: bluetoothd exited.
    Lost,
    /// `org.bluez` has a new owner: a bluetoothd that started after the one
    /// our snapshot came from.
    Appeared,
}

/// Read one `NameOwnerChanged` body. `None` for any name other than
/// `org.bluez`, and for a body that does not parse.
fn bluez_owner_change(body: &zbus::Message) -> Option<OwnerChange> {
    let (name, _old, new) = body.body().deserialize::<(String, String, String)>().ok()?;
    if name != "org.bluez" {
        return None;
    }
    Some(if new.is_empty() {
        OwnerChange::Lost
    } else {
        OwnerChange::Appeared
    })
}

/// How [`listen`] ended, when it ended without an error.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ListenEnd {
    /// A signal stream ended. Reconnect on the usual backoff.
    StreamClosed,
    /// bluetoothd exited: `org.bluez` lost its owner. Clear the snapshot, then
    /// [`wait_for_bluez`] before reconnecting.
    DaemonExited,
}

/// A subscription to the bus's `NameOwnerChanged`, every name. Filtered in
/// code by [`bluez_owner_change`]. The pairing agent watches the same signal
/// the same way (`agent::run_agent`).
pub(super) fn name_owner_changed() -> SignalSubscription {
    hytte_bus::signals(BusKind::System, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .signal("NameOwnerChanged")
        .start()
}

/// One of bluez's `ObjectManager` signals (`InterfacesAdded` or
/// `InterfacesRemoved`) on its root object.
fn object_manager_signal(signal: &str) -> SignalSubscription {
    hytte_bus::signals(BusKind::System, "org.bluez")
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .signal(signal)
        .start()
}

/// `PropertiesChanged` on one bluez object: the adapter, or a device
/// ([`subscribe_device_props`]).
fn props_changed_signal(path: &str) -> SignalSubscription {
    hytte_bus::signals(BusKind::System, "org.bluez")
        .at_path(path.to_string())
        .iface("org.freedesktop.DBus.Properties")
        .signal("PropertiesChanged")
        .start()
}

/// React to one `NameOwnerChanged` item for the watcher. `Some` means
/// [`event_loop`] ends with that.
///
/// The state tracker never watched bluetoothd itself; only the pairing
/// agent did. A bluetoothd that exits does not reliably say goodbye first. A
/// crash or a `SIGKILL` emits nothing. On `SIGTERM`, `src/main.c` removes the
/// adapter's objects (`adapter_cleanup`) only after its main loop has
/// returned, so that `InterfacesRemoved` is nothing to rely on. So after
/// `systemctl restart bluetooth`, the old daemon's scanned devices,
/// `Discovering = true` and the rest of the snapshot stayed on screen, and
/// nothing replaced them.
///
/// - **Lost**: end the watch with [`ListenEnd::DaemonExited`]. The
///   supervised loop in `mod.rs` then does the cleanup it does for a removed
///   adapter: no adapter, no devices, no in-flight markers, the prompt
///   rejected. With no daemon, "no adapter" is the honest state. It does
///   *not* re-sync: a re-sync is a call to `org.bluez`, see [`wait_for_bluez`].
/// - **Appeared**: arm the coalesced re-sync, which replaces the snapshot
///   with the new daemon's. That re-sync fails into a full reconnect if the
///   adapter is gone. This covers a restart whose `Lost` half this
///   subscription did not see.
/// - A `Resubscribed`/`Lagged` marker: an owner change may have gone missing,
///   so re-sync, as for any other subscription in this fold.
fn on_owner_item(item: SignalItem, wake: &ResyncWake) -> Option<ListenEnd> {
    let evt = bluetooth_marker_or_event(item, "NameOwnerChanged", wake)?;
    match bluez_owner_change(&evt.body)? {
        OwnerChange::Lost => Some(ListenEnd::DaemonExited),
        OwnerChange::Appeared => {
            tracing::info!("org.bluez has a new owner; re-syncing devices");
            arm_resync(wake, "org.bluez new owner");
            None
        }
    }
}

/// How often [`wait_for_bluez`] asks the bus again without having been woken.
/// It is a backstop; the wake is the `NameOwnerChanged` for the new owner.
const BLUEZ_RECHECK: std::time::Duration = std::time::Duration::from_secs(10);

/// Wait until `org.bluez` has an owner again, without starting one.
///
/// This asks the bus (`NameHasOwner`), never `org.bluez` itself. Any method
/// call to `org.bluez` D-Bus-activates `bluetooth.service` (`BlueZ` ships an
/// `org.bluez` activation file, and the `NixOS` module aliases the unit as
/// `dbus-org.bluez.service`). Reconnecting straight away would therefore
/// turn a deliberate `systemctl stop bluetooth` into a restart.
///
/// It wakes on the `NameOwnerChanged` that announces the new owner. The
/// first `NameHasOwner` is asked only once the bus has this subscription's
/// match rule (#1551, through [`wait_for_bluez_with`]), so an owner that
/// appears after that answer always wakes it. Until then the question went
/// out at once, and an owner that appeared before the rule was in place
/// waited out the [`BLUEZ_RECHECK`] re-ask. That re-ask now covers a
/// `NameHasOwner` that failed, and a wake lost behind a marker.
///
/// The wait for the rule has no deadline. With the system bus down it stays
/// there, where the question would have failed as long. The rule is the same
/// one [`listen`]'s owner subscription installs next (and the pairing agent
/// holds for the process's life), so a bus that refuses it would park
/// `listen` in the same wait.
pub(super) async fn wait_for_bluez() {
    wait_for_bluez_with(
        || {
            let sub = name_owner_changed();
            let items = sub.items();
            (sub, items)
        },
        bluez_has_owner,
        BLUEZ_RECHECK,
    )
    .await;
}

/// [`wait_for_bluez`] with the subscription, the bus question and the
/// re-check interval injected, so a test can record the order and drive it
/// without a bus under paused time.
///
/// Subscribes (`subscribe`, which takes the receiver too), waits for the
/// bus to have the match rule, then asks `has_owner`, and returns on the
/// first `Ok(true)`. Otherwise waits for the next sign that bluetoothd may be
/// back ([`next_bluez_owner`]) or `recheck`, whichever comes first, and asks
/// again.
async fn wait_for_bluez_with<H, S, A, F>(
    subscribe: impl FnOnce() -> (H, S),
    has_owner: A,
    recheck: std::time::Duration,
) where
    H: Handshake,
    S: Stream<Item = SignalItem> + Unpin,
    A: Fn() -> F,
    F: Future<Output = Result<bool, BusError>>,
{
    // The handle has to outlive the loop: its stream ends when the last
    // handle goes.
    let ((_owner_sub, mut items), mut answer) =
        subscribe_then_snapshot(subscribe, &has_owner).await;
    loop {
        match answer {
            Ok(true) => return,
            Ok(false) => {}
            Err(e) => {
                tracing::debug!(error = %e, "bluetooth: NameHasOwner(org.bluez) failed; asking again");
            }
        }
        let _ = tokio::time::timeout(recheck, next_bluez_owner(&mut items)).await;
        answer = has_owner().await;
    }
}

/// Resolve on the next sign that `org.bluez` may have an owner again: the
/// `NameOwnerChanged` for a new owner, or a marker, since an owner change may
/// have gone missing. A stream that ends never resolves, so the caller's
/// timeout paces it instead of letting it spin.
async fn next_bluez_owner<S>(items: &mut S)
where
    S: Stream<Item = SignalItem> + Unpin,
{
    while let Some(item) = items.next().await {
        match item {
            SignalItem::Event(evt) => {
                if bluez_owner_change(&evt.body) == Some(OwnerChange::Appeared) {
                    return;
                }
            }
            SignalItem::Resubscribed | SignalItem::Lagged { .. } => return,
        }
    }
    std::future::pending::<()>().await;
}

/// Ask the bus whether anyone owns `org.bluez`. This is answered by the bus
/// itself, so it activates nothing.
pub(super) async fn bluez_has_owner() -> Result<bool, hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("NameHasOwner")
        .args(("org.bluez".to_string(),))
        .send::<bool>()
        .await
}

/// One message from a per-device forwarder to [`fold`], which is where the
/// device map lives.
enum DeviceMsg {
    /// One `PropertiesChanged` emission on the device.
    Props {
        /// The object path the signal came from.
        path: String,
        /// The body of the `PropertiesChanged` signal: `(iface, changed,
        /// invalidated)`.
        body: zbus::Message,
    },
    /// The device's subscription has its match rule: read the device again
    /// ([`FoldBus::subscribe_device`], [`reread_device`]).
    Reread {
        /// The device's object path.
        path: String,
    },
}

/// Depth of the queue carrying "re-sync, please" wakes to [`fold`].
///
/// Small on purpose, for [`ResyncWake`]'s reason: every entry asks for exactly
/// the same thing, so a full queue is not backpressure to wait on — it is a
/// re-read already pending, which will observe whatever the dropped wake was
/// about. Mirrors `networkd_nm`'s `DEVICE_WAKE_QUEUE`.
const RESYNC_WAKE_QUEUE: usize = 8;

/// The "please re-read everything" end of [`fold`]'s resync channel.
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
/// [`open_fold`] arms it once too, when every subscription is in place (#1551).
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

// ── The bus side of the fold ──────────────────────────────────────────────────

/// What [`fold`] asks of the bus: the per-device subscriptions it makes as
/// devices come and go, and the two reads it repairs itself with. A trait so
/// a test can drive the fold's handlers with no bus; [`SystemBluez`] is the
/// one production impl.
trait FoldBus {
    /// One device's `PropertiesChanged` subscription. Dropping it tears the
    /// subscription down ([`subscribe_device_props`]).
    type DeviceSub;

    /// Subscribe to one device's `PropertiesChanged`, its forwarder feeding
    /// [`fold`]'s queues, and have the forwarder ask [`fold`] to read the
    /// device again once the bus has the rule ([`DeviceMsg::Reread`], #1551).
    ///
    /// Every device subscription asks for that re-read, because every one is
    /// made after its device's state was read: a device that appears is
    /// subscribed off its `InterfacesAdded`, and a device a resync finds,
    /// [`listen`]'s snapshot devices among them, off that resync's answer.
    /// Nothing waits for the device's rule before reading it, so a change
    /// made in between would be lost; the re-read is sent once the rule is in
    /// place, so it covers that change and every later one reaches the
    /// forwarder. Not waiting is deliberate (#1647 review): a rule the bus
    /// refuses then costs only its own device's live updates, as before
    /// #1551, rather than holding up the whole fold.
    fn subscribe_device(&self, path: &str) -> Self::DeviceSub;

    /// bluez's `GetManagedObjects`.
    fn managed_objects(&self) -> impl Future<Output = Result<ManagedObjects, BusError>>;

    /// `Properties.GetAll(iface)` on one bluez object.
    fn get_all(
        &self,
        path: &str,
        iface: &'static str,
    ) -> impl Future<Output = Result<HashMap<String, OwnedValue>, BusError>>;
}

/// [`FoldBus`] on the system bus. Per-device subscriptions forward into the
/// fold's own queues, which is why it holds their senders.
struct SystemBluez {
    props_tx: tokio::sync::mpsc::UnboundedSender<DeviceMsg>,
    resync_tx: ResyncWake,
}

impl FoldBus for SystemBluez {
    type DeviceSub = SignalSubscription;

    fn subscribe_device(&self, path: &str) -> SignalSubscription {
        subscribe_device_props(path, self.props_tx.clone(), self.resync_tx.clone())
    }

    fn managed_objects(&self) -> impl Future<Output = Result<ManagedObjects, BusError>> {
        get_managed_objects()
    }

    fn get_all(
        &self,
        path: &str,
        iface: &'static str,
    ) -> impl Future<Output = Result<HashMap<String, OwnedValue>, BusError>> {
        get_all_props(path, iface)
    }
}

// ── Opening the fold ──────────────────────────────────────────────────────────

/// The three top-level subscriptions [`fold`] reads besides the owner's, made
/// together and waited for together ([`open_fold`]).
///
/// No device's subscription is among them (#1647 review). The first resync,
/// which [`open_fold`] arms, subscribes every device it finds, and each of
/// those reads its device again once its own rule is in place
/// ([`FoldBus::subscribe_device`]). Until the review the snapshot's devices
/// were subscribed here and waited for with the rest, so a single device rule
/// the bus refused (one rule per known device, against a per-connection
/// limit) kept the whole fold from starting, owner stream included.
struct FoldSubscriptions<H> {
    /// `InterfacesAdded` on bluez's root object.
    added: H,
    /// `InterfacesRemoved` on bluez's root object.
    removed: H,
    /// `PropertiesChanged` on the adapter.
    adapter: H,
}

impl<H: Handshake> Handshake for FoldSubscriptions<H> {
    /// Resolves once the bus has every one of these rules.
    async fn subscribed(&self) {
        for sub in [&self.added, &self.removed, &self.adapter] {
            sub.subscribed().await;
        }
    }
}

/// Subscribe the three top-level signals [`fold`] reads (`subscribe`, which
/// takes the receivers too), wait for the bus to have their rules, then arm
/// one resync (#1551).
///
/// These subscriptions are made after [`listen`]'s snapshot, because the
/// adapter's path comes from it, so an emission between that snapshot and
/// their rules would be lost. They used to be made there and never waited
/// for, on purpose: replaying an `InterfacesAdded` older than the snapshot
/// would have applied an older device's properties over the snapshot's. Now
/// the window is closed by reading again after the rules are in place, and
/// the fold tolerates that replay instead (see [`fold`]'s "Ordering"). The
/// cost is a second `GetManagedObjects` per [`listen`], and, since that
/// resync is what subscribes the snapshot's devices, one `GetAll` per known
/// device once its rule is in place.
///
/// The wait is made inside [`subscribe_then_snapshot`], on what `subscribe`
/// returned, and the resync is armed here rather than by the caller, so
/// neither can be left out or moved ahead of the other. It has no deadline:
/// with the bus down it waits for it, and a bus that refuses one of these
/// three `AddMatch`es keeps the fold from starting, with the `listen`
/// snapshot on screen and that subscription's one `warn!` in the journal
/// (`hytte-bus`'s `subscribed()`, "Liveness"). A refused **device** rule does
/// not: no device's subscription is waited for here.
async fn open_fold<H, R>(
    subscribe: impl FnOnce() -> (FoldSubscriptions<H>, R),
    resync: &ResyncWake,
) -> (FoldSubscriptions<H>, R)
where
    H: Handshake,
{
    let (opened, ()) = subscribe_then_snapshot(subscribe, || async {
        arm_resync(resync, "every fold subscription installed");
    })
    .await;
    opened
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
/// propagate through: the re-read now runs in [`fold`]'s wake arm, and its
/// failure (a transient `GetManagedObjects`, or the adapter genuinely gone)
/// propagates from there, same as before — the supervised loop backs off and
/// restarts [`listen`] from scratch.
fn bluetooth_marker_or_event(
    item: SignalItem,
    source: &'static str,
    wake: &ResyncWake,
) -> Option<hytte_bus::SignalEvent> {
    match item {
        SignalItem::Event(evt) => Some(evt),
        SignalItem::Resubscribed | SignalItem::Lagged { .. } => {
            tracing::info!(source, "bluetooth signal resubscribed; re-syncing devices");
            arm_resync(wake, source);
            None
        }
    }
}

/// Subscribe to the three top-level signals the watch folds, wait for their
/// rules, arm one resync ([`open_fold`]), then [`fold`] until the watch ends.
/// That resync subscribes every device it finds ([`resync`]).
///
/// `owner_items` is the `NameOwnerChanged` stream [`listen`] subscribed to
/// before its snapshot, so an owner change made while that snapshot was
/// being taken is already queued here.
async fn event_loop<S>(
    state: &State,
    adapter_path: &str,
    owner_items: S,
) -> Result<ListenEnd, anyhow::Error>
where
    S: Stream<Item = SignalItem> + Unpin,
{
    // Device-level `PropertiesChanged` and re-read requests, forwarded from
    // the per-device subscriptions (added/removed as devices appear and
    // disappear).
    let (props_tx, props_rx) = tokio::sync::mpsc::unbounded_channel::<DeviceMsg>();
    // One coalescing wake for every "re-read everything" request in this fold,
    // whichever subscription noticed the hole. See `ResyncWake`.
    let (resync_tx, resync_rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
    let bus = SystemBluez {
        props_tx,
        resync_tx: resync_tx.clone(),
    };

    let (subscriptions, (added, removed, adapter)) = open_fold(
        || {
            let subscriptions = FoldSubscriptions {
                added: object_manager_signal("InterfacesAdded"),
                removed: object_manager_signal("InterfacesRemoved"),
                adapter: props_changed_signal(adapter_path),
            };
            let receivers = (
                subscriptions.added.items(),
                subscriptions.removed.items(),
                subscriptions.adapter.items(),
            );
            (subscriptions, receivers)
        },
        &resync_tx,
    )
    .await;

    let end = fold(
        state,
        adapter_path,
        &bus,
        FoldStreams {
            owner: owner_items,
            added,
            removed,
            adapter,
        },
        FoldQueues {
            props_rx,
            resync_tx,
            resync_rx,
        },
        // No device is subscribed yet: the resync `open_fold` armed
        // subscribes every one it finds.
        HashMap::new(),
    )
    .await;
    // The three streams end when their last handle goes, so the handles have
    // to outlive the loop that reads them.
    drop(subscriptions);
    end
}

/// The streams [`fold`] reads, all with their markers.
struct FoldStreams<O, S> {
    /// bluetoothd's owner ([`name_owner_changed`]), from [`listen`].
    owner: O,
    /// `InterfacesAdded` on bluez's root object.
    added: S,
    /// `InterfacesRemoved` on bluez's root object.
    removed: S,
    /// `PropertiesChanged` on the adapter.
    adapter: S,
}

/// [`fold`]'s own queues.
struct FoldQueues {
    /// What the per-device forwarders send.
    props_rx: tokio::sync::mpsc::UnboundedReceiver<DeviceMsg>,
    /// The resync wake's sending end, for the markers the loop itself reads.
    resync_tx: ResyncWake,
    /// The resync wake.
    resync_rx: tokio::sync::mpsc::Receiver<()>,
}

/// Fold every signal for the adapter and its devices into `state` until the
/// watch ends.
///
/// # Ordering (#1551)
///
/// [`open_fold`] arms one resync once the three top-level rules are in place:
/// a `GetManagedObjects` sent after those rules, so every change bluez makes
/// after answering it reaches a receiver here, or a marker follows it there,
/// which resyncs again. Device rules are covered device by device: that
/// resync subscribes every device it finds, and each device's subscription
/// asks for a re-read of its device once its own rule is in place
/// ([`FoldBus::subscribe_device`]). What the fold relies on, for those reads
/// and every other:
///
/// 1. **A read is never older than what it overwrites.** Every read the fold
///    makes (a [`resync`], a device's [`reread_device`]) is sent from inside
///    one arm of this loop and awaited there, so it is sent after every event
///    the loop has applied, and no event is applied while it is in flight.
///    bluez emitted each applied event before it was asked, so its answer
///    already reflects them: replacing the map with it (a resync) or one
///    device with it (a re-read) cannot bring back a device whose removal was
///    applied, or roll a property back.
/// 2. **An event applied after a read may be older than it.** An emission
///    made between a rule's `AddMatch` and a read may or may not be
///    delivered, and one that is can come out of its receiver after the read's
///    answer, since it crosses a task the reply does not (`hytte-bus`'s
///    `subscribed()`: "the consumer has to tolerate that replay"). Each kind
///    of event does:
///    - `PropertiesChanged` (adapter, device, battery) carries absolute
///      values. zbus hands a subscription every matching message it reads
///      once that subscription's sender is registered, so every later change
///      to the same property follows the replayed one on the same receiver,
///      or a marker does. The state ends at the newest value; an older one
///      can show until that next item is applied.
///    - `InterfacesAdded` never overwrites. A device or battery the map
///      already has is left as it is ([`Added::Known`]). A device it inserts is
///      subscribed and read again once that rule is in place, which drops it
///      if bluez no longer has it. So a replayed add can neither roll a device
///      back to the properties it was announced with, nor keep a device
///      removed before the read.
///    - `InterfacesRemoved` of a device the map does not have changes
///      nothing.
/// 3. **Nothing is held twice.** The map and the subscriptions are keyed by
///    object path, and a device is subscribed only when it has no
///    subscription.
///
/// What it does not rely on, because it does not hold: an order between two
/// receivers. `InterfacesAdded` and `InterfacesRemoved` are separate rules
/// with separate receivers, so a device removed and added back in quick
/// succession can be applied the other way round (the add first, kept as
/// [`Added::Known`], then the removal) and be missing until the next resync
/// or its next `InterfacesAdded`. That predates #1551 and is not changed by
/// it. The arm order below narrows it: a removal already queued is taken
/// before an add, so the mis-order needs the add to come out of its receiver
/// before the removal has reached its own. The other mis-order, an add taken
/// after its own object's removal, the add's re-read heals by dropping the
/// device.
///
/// # Arm order (#1647 review)
///
/// The loop is `biased`: each turn it takes the first ready arm in this
/// order, owner, `InterfacesRemoved`, `InterfacesAdded`, adapter, resync,
/// device messages.
///
/// - **The owner comes first** so a `Lost` that is already queued wins over
///   everything else that is. Every other arm can call `org.bluez` (a resync,
///   a re-read), and a call to a name with no owner D-Bus-activates the
///   bluetoothd the user just stopped. Unbiased, the handshake's resync beat
///   a `Lost` queued while [`open_fold`] waited 14 times in 20.
/// - **Removal before addition**, for the mis-order above.
/// - **The resync before the device messages**, because the device messages
///   are the one source that can come fast (every device's
///   `PropertiesChanged` during discovery: RSSI, advertising data). At the
///   bottom they can delay nothing above them.
///
/// Bias cannot starve an arm unless the arms above it stay ready on every
/// turn. Each turn takes one item and does a bounded piece of work; the four
/// signal arms are low-rate bus signals whose receivers are bounded (an
/// overrun becomes one `Lagged` marker, which only arms the resync); the
/// resync queue is drained whole on each turn and refilled only by markers,
/// a new owner and a re-announced adapter. So an arm waits for the backlog
/// queued above it, not for a stream, and the only stream that could keep
/// the arms above it busy is at the bottom.
async fn fold<B, O, S>(
    state: &State,
    adapter_path: &str,
    bus: &B,
    streams: FoldStreams<O, S>,
    queues: FoldQueues,
    mut device_subs: HashMap<String, B::DeviceSub>,
) -> Result<ListenEnd, anyhow::Error>
where
    B: FoldBus,
    O: Stream<Item = SignalItem> + Unpin,
    S: Stream<Item = SignalItem> + Unpin,
{
    let FoldStreams {
        owner: mut owner_items,
        added: mut ifaces_added_items,
        removed: mut ifaces_removed_items,
        adapter: mut adapter_props_items,
    } = streams;
    let FoldQueues {
        mut props_rx,
        resync_tx,
        mut resync_rx,
    } = queues;

    loop {
        tokio::select! {
            // See "Arm order" above: the order of these arms is load-bearing.
            biased;

            item = owner_items.next() => {
                let Some(item) = item else { break; };
                if let Some(end) = on_owner_item(item, &resync_tx) {
                    tracing::info!("bluetoothd exited (org.bluez has no owner)");
                    return Ok(end);
                }
            }

            item = ifaces_removed_items.next() => {
                let Some(item) = item else { break; };
                let evt = bluetooth_marker_or_event(item, "InterfacesRemoved", &resync_tx);
                let Some(evt) = evt else { continue; };
                if handle_ifaces_removed(state, adapter_path, &evt.body, &mut device_subs).await {
                    return Err(anyhow::anyhow!("adapter removed"));
                }
            }

            item = ifaces_added_items.next() => {
                let Some(item) = item else { break; };
                let evt = bluetooth_marker_or_event(item, "InterfacesAdded", &resync_tx);
                let Some(evt) = evt else { continue; };
                handle_ifaces_added(
                    state,
                    adapter_path,
                    &evt.body,
                    bus,
                    &resync_tx,
                    &mut device_subs,
                ).await;
            }

            item = adapter_props_items.next() => {
                let Some(item) = item else { break; };
                let evt = bluetooth_marker_or_event(item, "adapter PropertiesChanged", &resync_tx);
                let Some(evt) = evt else { continue; };
                handle_adapter_props_changed(state, adapter_path, &evt.body);
            }

            // One `GetManagedObjects` per burst of markers, however many
            // subscriptions noticed the same hole. See `ResyncWake`.
            Some(()) = resync_rx.recv() => {
                drain_and_resync(&mut resync_rx, || {
                    resync(state, adapter_path, bus, &mut device_subs)
                }).await?;
            }

            Some(msg) = props_rx.recv() => {
                match msg {
                    DeviceMsg::Props { path, body } => {
                        handle_device_props_changed(state, &path, &body).await;
                    }
                    DeviceMsg::Reread { path } => {
                        reread_device(state, &path, bus, &mut device_subs).await;
                    }
                }
            }
        }
    }

    Ok(ListenEnd::StreamClosed)
}

/// Re-sync all bluetooth state from a fresh `GetManagedObjects` snapshot —
/// the repair for a `Resubscribed`/`Lagged` marker on any of this fold's
/// signal subscriptions (the three top-level ones plus one per tracked
/// device, #1201), and the read that follows [`open_fold`]'s handshake
/// (#1551). Between a subscription dying and its replacement going up, an
/// `InterfacesAdded`/`InterfacesRemoved`/`PropertiesChanged` emission for it
/// is gone for good — `GetManagedObjects` is the only source broad enough to
/// repair whichever one it was. Reconciles the adapter, replaces the device
/// map wholesale (batteries included), and diffs the per-device subscription
/// set against it, subscribing a device it finds
/// ([`FoldBus::subscribe_device`], which reads it again once its rule is in
/// place). The first resync of a watch is the one that subscribes
/// [`listen`]'s snapshot devices. Mirrors [`handle_ifaces_removed`]'s "adapter
/// removed" branch when the adapter itself is gone: same cleanup, same `Err`
/// to force a full reconnect.
///
/// Replacing the map wholesale is safe for [`fold`]'s "Ordering" point 1: the
/// read is newer than everything the map holds.
async fn resync<B: FoldBus>(
    state: &State,
    adapter_path: &str,
    bus: &B,
    device_subs: &mut HashMap<String, B::DeviceSub>,
) -> Result<(), anyhow::Error> {
    let managed = bus
        .managed_objects()
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

    let new_map = devices_from_managed(&managed, adapter_path);

    // Diff the subscription set: drop a device that's gone, subscribe one
    // that's new. Dropping the map's handle is the whole teardown — it is the
    // last `SignalSubscription` for that device, so the tracker hits zero, the
    // subscription task exits and removes its match rule, and the forwarder's
    // stream ends with it. That is only true because the forwarder holds the
    // *stream* and not a handle; see `subscribe_device_props`.
    device_subs.retain(|path, _| new_map.contains_key(path));
    for path in new_map.keys() {
        if !device_subs.contains_key(path) {
            let sub = bus.subscribe_device(path);
            device_subs.insert(path.clone(), sub);
        }
    }

    *state.devices_map.lock().await = new_map;
    state.publish_devices().await;

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
/// last one, and `Drop` means what its doc says. The same holds for
/// `subscribed()`, which the forwarder is handed too ([`forward_device`]).
fn subscribe_device_props(
    device_path: &str,
    tx: tokio::sync::mpsc::UnboundedSender<DeviceMsg>,
    resync_tx: ResyncWake,
) -> SignalSubscription {
    forward_device(
        props_changed_signal(device_path),
        device_path,
        tx,
        resync_tx,
    )
}

/// Wire one device subscription to the fold: spawn its forwarder on the
/// subscription's items and on its handshake, so the forwarder asks for the
/// device's re-read once the rule is in place
/// ([`FoldBus::subscribe_device`]). Returns the handle, still the only one.
fn forward_device(
    sub: SignalSubscription,
    path: &str,
    tx: tokio::sync::mpsc::UnboundedSender<DeviceMsg>,
    resync_tx: ResyncWake,
) -> SignalSubscription {
    let installed = Some(sub.subscribed());
    // Detached deliberately: the forwarder's shutdown is its stream ending,
    // which is what dropping `sub` causes. The handle is returned only so a
    // test can await that.
    drop(spawn_device_forwarder(
        path.to_string(),
        sub.items(),
        installed,
        tx,
        resync_tx,
    ));
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
///
/// `installed` is the subscription's handshake ([`forward_device`] always
/// hands it one; `None` is for tests of the forwarder's other duties): once
/// it resolves the forwarder sends one [`DeviceMsg::Reread`]
/// ([`FoldBus::subscribe_device`]).
/// Where that lands among the device's own emissions does not matter: the
/// read is sent when the fold reaches it, after whatever came before it, and
/// whatever comes after is newer or a replay the fold tolerates ([`fold`]'s
/// "Ordering").
fn spawn_device_forwarder<S, F>(
    path: String,
    mut items: S,
    installed: Option<F>,
    tx: tokio::sync::mpsc::UnboundedSender<DeviceMsg>,
    resync_tx: ResyncWake,
) -> tokio::task::JoinHandle<()>
where
    S: Stream<Item = SignalItem> + Unpin + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    runtime::handle().spawn(async move {
        let mut asked = installed.is_none();
        let mut installed = std::pin::pin!(async move {
            match installed {
                Some(installed) => installed.await,
                None => std::future::pending().await,
            }
        });
        loop {
            tokio::select! {
                () = &mut installed, if !asked => {
                    asked = true;
                    if tx.send(DeviceMsg::Reread { path: path.clone() }).is_err() {
                        break;
                    }
                }
                item = items.next() => {
                    let Some(item) = item else { break; };
                    match item {
                        SignalItem::Event(evt) => {
                            let props = DeviceMsg::Props {
                                path: path.clone(),
                                body: evt.body,
                            };
                            if tx.send(props).is_err() {
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
            }
        }
        tracing::debug!(path, "bluetooth: per-device signal stream ended");
    })
}

/// What one `InterfacesAdded` did to the device map ([`apply_added`]).
#[derive(Debug, PartialEq, Eq)]
enum Added {
    /// A device the map did not have, inserted from the signal's properties.
    /// It needs a subscription, which reads it again once its rule is in
    /// place ([`FoldBus::subscribe_device`]).
    Inserted,
    /// A device the map already has, or a battery it already knows. Nothing
    /// was overwritten (#1551): the signal is either a replay older than the
    /// map's state ([`fold`]'s "Ordering" point 2), or an add that came out
    /// ahead of its own object's removal on the other receiver. Either way
    /// what it carries is no newer than what the map holds.
    Known,
    /// A `Battery1` on a device the map has, whose battery it did not know:
    /// filled in.
    BatteryFilled,
    /// Nothing for this fold: not under the adapter, not a device, or a
    /// battery for a device the map does not have.
    Ignored,
}

/// Fold one `InterfacesAdded` for `path` into the map: fill in what the map
/// does not have, and never overwrite what it has. See [`Added`].
async fn apply_added(state: &State, adapter_path: &str, path: &str, ifaces: &Interfaces) -> Added {
    if !path.starts_with(adapter_path) {
        return Added::Ignored;
    }
    let battery = ifaces.get("org.bluez.Battery1");
    let mut map = state.devices_map.lock().await;
    let added = if let Some(device) = ifaces.get("org.bluez.Device1") {
        if map.contains_key(path) {
            Added::Known
        } else {
            let dev = device_from(path, device, battery);
            tracing::debug!(path, alias = dev.alias, "device added");
            map.insert(path.to_string(), dev);
            Added::Inserted
        }
    } else if let Some(battery) = battery {
        // Battery1 may appear *after* Device1 (added when the device
        // connects) on its existing path.
        match map.get_mut(path) {
            Some(dev) if dev.battery.is_none() => {
                dev.battery = property::<u8>(battery, "Percentage");
                Added::BatteryFilled
            }
            Some(_) => Added::Known,
            None => Added::Ignored,
        }
    } else {
        Added::Ignored
    };
    drop(map);
    if matches!(added, Added::Inserted | Added::BatteryFilled) {
        state.publish_devices().await;
    }
    added
}

async fn handle_ifaces_added<B: FoldBus>(
    state: &State,
    adapter_path: &str,
    msg: &zbus::Message,
    bus: &B,
    resync_tx: &ResyncWake,
    device_subs: &mut HashMap<String, B::DeviceSub>,
) {
    let Ok((path, ifaces)) = msg
        .body()
        .deserialize::<(zbus::zvariant::OwnedObjectPath, Interfaces)>()
    else {
        return;
    };

    let p = path.as_str();
    if adapter_reappeared(p, &ifaces, adapter_path, resync_tx) {
        return;
    }

    let added = apply_added(state, adapter_path, p, &ifaces).await;
    // A device the map holds gets a subscription if it has none: always for
    // one just inserted, and for a known one as a backstop (a `listen`
    // snapshot device the first resync has not subscribed yet). It is read
    // again once that subscription's rule is in place.
    if matches!(added, Added::Inserted | Added::Known) && !device_subs.contains_key(p) {
        let sub = bus.subscribe_device(p);
        device_subs.insert(p.to_string(), sub);
    }
}

/// Arm a re-sync if this `InterfacesAdded` announces the tracked adapter
/// itself, and say whether it did.
///
/// The adapter object only appears for one reason while this watcher is
/// running: a bluetoothd that started after the one our snapshot came from,
/// re-announcing it at the same path. (A removed adapter ends the loop in
/// [`handle_ifaces_removed`], so it never gets this far.) The new daemon's
/// state is not ours, so re-read all of it. This covers a restart even if the
/// `NameOwnerChanged` for it was missed.
///
/// This replaces a branch that could never run. It adopted the adapter only
/// when `adapter_path` was empty, but [`listen`] returns before this loop
/// starts if it finds no adapter, so `adapter_path` was never empty here. An
/// adapter at a *different* path is still ignored: the watcher tracks the
/// first adapter only, as before.
fn adapter_reappeared(
    path: &str,
    ifaces: &Interfaces,
    adapter_path: &str,
    wake: &ResyncWake,
) -> bool {
    if path != adapter_path || !ifaces.contains_key("org.bluez.Adapter1") {
        return false;
    }
    tracing::info!(path, "bluetooth adapter re-announced; re-syncing devices");
    arm_resync(wake, "adapter InterfacesAdded");
    true
}

/// Returns `true` when the adapter was removed (caller should reconnect).
///
/// Removing a device the map does not have changes nothing, which is what a
/// replayed removal older than the last read needs ([`fold`]'s "Ordering").
async fn handle_ifaces_removed<D>(
    state: &State,
    adapter_path: &str,
    msg: &zbus::Message,
    device_subs: &mut HashMap<String, D>,
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

/// What reading one device again found ([`reread_device`]).
enum DeviceRead {
    /// bluez has the device: its `Device1` properties, and its `Battery1`
    /// ones when the map knew of a battery and bluez answered for it.
    Found {
        /// `GetAll(org.bluez.Device1)`.
        device: HashMap<String, OwnedValue>,
        /// `GetAll(org.bluez.Battery1)`, when asked and answered.
        battery: Option<HashMap<String, OwnedValue>>,
    },
    /// bluez answered that the device is not there ([`device_is_gone`]).
    Gone,
}

/// Whether bluez's answer to a `GetAll(org.bluez.Device1)` says the device is
/// not there. From `BlueZ` 5.87 and libdbus 1.16.2:
///
/// - `UnknownObject`: the path has no node left in libdbus's object tree,
///   which is a removed device (`dbus-connection.c`, `found_object` false).
/// - `UnknownMethod`: the path is a node with no handler, such as a removed
///   device whose GATT child objects are still registered.
/// - `InvalidArgs` ("No such interface"): the object exists without
///   `Device1` (gdbus `properties_get_all`).
///
/// `UnknownInterface` is the same answer from a peer that is not gdbus. Any
/// other error (a timeout, a lost connection, `NoReply`, an unowned name)
/// says nothing about the device.
fn device_is_gone(error: &BusError) -> bool {
    matches!(
        error,
        BusError::Permanent { dbus_name: Some(name), .. }
            if matches!(
                name.as_str(),
                "org.freedesktop.DBus.Error.UnknownObject"
                    | "org.freedesktop.DBus.Error.UnknownMethod"
                    | "org.freedesktop.DBus.Error.UnknownInterface"
                    | "org.freedesktop.DBus.Error.InvalidArgs"
            )
    )
}

/// Read one device again and fold the answer in: what a device's subscription
/// asks for once its rule is in place ([`FoldBus::subscribe_device`], #1551).
///
/// A device the map no longer has is not read: it was removed since the
/// forwarder asked, and a call to `org.bluez` for nothing is a call that can
/// D-Bus-activate a stopped bluetoothd. Its battery is read only when the map
/// knows of one: a `Battery1` that appears later comes with an
/// `InterfacesAdded` on the root object, whose rule has long been in place,
/// and most devices a scan finds have none.
async fn reread_device<B: FoldBus>(
    state: &State,
    path: &str,
    bus: &B,
    device_subs: &mut HashMap<String, B::DeviceSub>,
) {
    let known_battery = state
        .devices_map
        .lock()
        .await
        .get(path)
        .map(|dev| dev.battery.is_some());
    let Some(known_battery) = known_battery else {
        return;
    };
    let read = match bus.get_all(path, "org.bluez.Device1").await {
        Ok(device) => {
            let battery = if known_battery {
                bus.get_all(path, "org.bluez.Battery1").await.ok()
            } else {
                None
            };
            DeviceRead::Found { device, battery }
        }
        Err(e) if device_is_gone(&e) => DeviceRead::Gone,
        Err(e) => {
            tracing::debug!(path, error = %e, "bluetooth: re-reading a device failed; keeping it as it is");
            return;
        }
    };
    if apply_reread(state, path, read).await {
        // The last handle, so this tears the subscription down too.
        device_subs.remove(path);
    }
}

/// Fold one device re-read into the map, and say whether the device was
/// dropped, so the caller drops its subscription too.
///
/// The read was sent from inside [`fold`]'s loop, so it is newer than the
/// map's entry ("Ordering" point 1). `Found` replaces the entry outright,
/// built the way a snapshot builds one; the battery is kept from the map
/// unless bluez answered for it. `Gone` drops the device. A device the map no
/// longer has stays gone: the read never resurrects one.
///
/// The list is published only when the entry changed (#1647 review). Most
/// re-reads find what the map already has: a device the first resync
/// subscribed was read by that resync a moment earlier, and a device a scan
/// finds has usually not changed since its `InterfacesAdded`. Publishing
/// those anyway re-rendered the panel's list a second time per device.
async fn apply_reread(state: &State, path: &str, read: DeviceRead) -> bool {
    let mut map = state.devices_map.lock().await;
    let Some(known) = map.get(path) else {
        return false;
    };
    let (dropped, changed) = match read {
        DeviceRead::Found { device, battery } => {
            let mut dev = device_from(path, &device, battery.as_ref());
            if battery.is_none() {
                dev.battery = known.battery;
            }
            let changed = !same_device(known, &dev);
            if changed {
                map.insert(path.to_string(), dev);
            }
            (false, changed)
        }
        DeviceRead::Gone => {
            tracing::debug!(
                path,
                "bluetooth: a re-read device is not there any more; dropping it"
            );
            map.remove(path);
            (true, true)
        }
    };
    drop(map);
    if changed {
        state.publish_devices().await;
    }
    dropped
}

/// Whether two entries are the same, field for field ([`Device`] derives no
/// `PartialEq`). The fields are destructured without `..`, so a field added
/// to [`Device`] does not compile here until it is compared too.
fn same_device(a: &Device, b: &Device) -> bool {
    let Device {
        path,
        address,
        alias,
        icon,
        paired,
        connected,
        trusted,
        battery,
    } = a;
    *path == b.path
        && *address == b.address
        && *alias == b.alias
        && *icon == b.icon
        && *paired == b.paired
        && *connected == b.connected
        && *trusted == b.trusted
        && *battery == b.battery
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
    use hytte_bus::SignalEvent;
    use std::cell::{Cell, RefCell};

    /// A forwarder handed no handshake, so it never asks for a re-read.
    const NO_REREAD: Option<std::future::Ready<()>> = None;

    /// A subscription whose handshake is not ready until polled twice, so a
    /// helper that only *creates* the wait without awaiting it to the end is
    /// caught too. Logs its name, and whether the resync wake `armed` was
    /// already armed when its rule "landed".
    struct Recording<'a> {
        log: &'a RefCell<Vec<(&'static str, bool)>>,
        name: &'static str,
        armed: &'a ResyncWake,
    }

    impl Handshake for Recording<'_> {
        fn subscribed(&self) -> impl Future<Output = ()> {
            let Self { log, name, armed } = *self;
            async move {
                tokio::task::yield_now().await;
                let armed = armed.capacity() < RESYNC_WAKE_QUEUE;
                log.borrow_mut().push((name, armed));
            }
        }
    }

    /// `event_loop`'s three top-level subscriptions are made after
    /// `listen`'s snapshot, so the fold waits for the bus to have every one
    /// of their rules and only then arms one resync, which reads everything
    /// again (#1551). Until then they were never waited for and nothing was
    /// read again, and an emission between the snapshot and the rules was
    /// lost. No device rule is waited for (#1647 review): that resync
    /// subscribes the devices, and each reads its own device again.
    ///
    /// **Falsification:** drop the wait (from `open_fold` or
    /// `subscribe_then_snapshot`), or the loop in `FoldSubscriptions`'
    /// `subscribed`, and rules go missing from the log; arm the resync
    /// before the wait and a rule lands with the wake already armed; arm it
    /// twice, or not at all, and the wake count is wrong.
    #[tokio::test]
    async fn the_fold_arms_one_resync_once_every_rule_is_installed() {
        let (wake, mut wakes) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
        let log = RefCell::new(Vec::new());
        let rule = |name| Recording {
            log: &log,
            name,
            armed: &wake,
        };

        let (_subscriptions, ()) = open_fold(
            || {
                log.borrow_mut().push(("subscribe", false));
                let subscriptions = FoldSubscriptions {
                    added: rule("InterfacesAdded"),
                    removed: rule("InterfacesRemoved"),
                    adapter: rule("adapter PropertiesChanged"),
                };
                (subscriptions, ())
            },
            &wake,
        )
        .await;

        let mut log = log.into_inner();
        assert_eq!(log.first(), Some(&("subscribe", false)), "subscribed first");
        let mut installed = log.split_off(1);
        installed.sort_unstable();
        assert_eq!(
            installed,
            [
                ("InterfacesAdded", false),
                ("InterfacesRemoved", false),
                ("adapter PropertiesChanged", false),
            ],
            "every rule, each before the resync was armed"
        );
        assert!(wakes.try_recv().is_ok(), "one resync once every rule is in");
        assert!(wakes.try_recv().is_err(), "exactly one");
    }

    /// Waiting for bluetoothd asks `NameHasOwner` only once the bus has the
    /// owner subscription's rule (#1551). Until then it asked at once, and
    /// an owner that appeared before the rule was in place waited out the
    /// 10 s re-check.
    ///
    /// **Falsification:** ask before the wait, or drop it, and the order
    /// changes.
    #[tokio::test(start_paused = true)]
    async fn waiting_for_bluez_asks_once_the_owner_rule_is_installed() {
        let (wake, _wakes) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
        let log = RefCell::new(Vec::new());
        tokio::time::timeout(
            BLUEZ_RECHECK,
            wait_for_bluez_with(
                || {
                    log.borrow_mut().push(("subscribe", false));
                    let rule = Recording {
                        log: &log,
                        name: "owner rule installed",
                        armed: &wake,
                    };
                    (rule, futures_util::stream::pending::<SignalItem>())
                },
                || {
                    log.borrow_mut().push(("NameHasOwner", false));
                    async { Ok(true) }
                },
                BLUEZ_RECHECK,
            ),
        )
        .await
        .expect("an owner on the first answer ends the wait");
        assert_eq!(
            log.into_inner(),
            [
                ("subscribe", false),
                ("owner rule installed", false),
                ("NameHasOwner", false),
            ]
        );
    }

    /// The wait goes on asking after the handshake as it did before it: a
    /// "no", then a failure, then the yes, each re-ask after the re-check
    /// interval when nothing wakes it.
    #[tokio::test(start_paused = true)]
    async fn waiting_for_bluez_asks_until_it_has_an_owner() {
        let answers = RefCell::new(std::collections::VecDeque::from([
            Ok(false),
            Err(BusError::Permanent {
                reason: "Timeout".to_owned(),
                dbus_name: Some("org.freedesktop.DBus.Error.Timeout".to_owned()),
            }),
            Ok(true),
        ]));
        let asked = Cell::new(0_u32);
        let started = tokio::time::Instant::now();
        let (wake, _wakes) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
        let log = RefCell::new(Vec::new());

        wait_for_bluez_with(
            || {
                let rule = Recording {
                    log: &log,
                    name: "owner rule installed",
                    armed: &wake,
                };
                (rule, futures_util::stream::pending::<SignalItem>())
            },
            || {
                asked.set(asked.get() + 1);
                let answer = answers.borrow_mut().pop_front().expect("scripted");
                async move { answer }
            },
            BLUEZ_RECHECK,
        )
        .await;

        assert_eq!(asked.get(), 3, "a no, a failure, then the yes");
        assert_eq!(started.elapsed(), BLUEZ_RECHECK * 2);
    }

    // ── #1551: the fold's merge, against a resync and its replays ───────────

    const ADAPTER: &str = "/org/bluez/hci0";
    const HEADPHONES: &str = "/org/bluez/hci0/dev_AA_AA_AA_AA_AA_AA";
    const PHONE: &str = "/org/bluez/hci0/dev_BB_BB_BB_BB_BB_BB";
    const GONE: &str = "/org/bluez/hci0/dev_CC_CC_CC_CC_CC_CC";

    /// A device as bluez holds it in a [`FakeBluez`].
    #[derive(Clone)]
    struct Held {
        alias: &'static str,
        connected: bool,
        battery: Option<u8>,
    }

    fn device_props(alias: &str, connected: bool) -> HashMap<String, OwnedValue> {
        HashMap::from([
            ("Address".to_owned(), make_owned_value("AA:AA:AA:AA:AA:AA")),
            ("Alias".to_owned(), make_owned_value(alias.to_owned())),
            ("Connected".to_owned(), make_owned_value(connected)),
            ("Paired".to_owned(), make_owned_value(true)),
            ("Trusted".to_owned(), make_owned_value(false)),
        ])
    }

    fn battery_props(percentage: u8) -> HashMap<String, OwnedValue> {
        HashMap::from([("Percentage".to_owned(), make_owned_value(percentage))])
    }

    fn interfaces(held: &Held) -> Interfaces {
        let mut ifaces = HashMap::from([(
            "org.bluez.Device1".to_owned(),
            device_props(held.alias, held.connected),
        )]);
        if let Some(percentage) = held.battery {
            ifaces.insert("org.bluez.Battery1".to_owned(), battery_props(percentage));
        }
        ifaces
    }

    /// The `InterfacesAdded` bluez emits for `path`.
    fn interfaces_added(path: &str, ifaces: &Interfaces) -> zbus::Message {
        let path = zbus::zvariant::ObjectPath::try_from(path).expect("object path");
        zbus::Message::signal("/", "org.freedesktop.DBus.ObjectManager", "InterfacesAdded")
            .expect("signal builder")
            .build(&(path, ifaces))
            .expect("build InterfacesAdded")
    }

    /// The `InterfacesRemoved` bluez emits for `path`.
    fn interfaces_removed(path: &str, ifaces: &[&str]) -> zbus::Message {
        let path = zbus::zvariant::ObjectPath::try_from(path).expect("object path");
        zbus::Message::signal(
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesRemoved",
        )
        .expect("signal builder")
        .build(&(path, ifaces))
        .expect("build InterfacesRemoved")
    }

    /// A `PropertiesChanged` on a device's `Device1`.
    fn device_changed(changed: &HashMap<String, OwnedValue>) -> zbus::Message {
        let invalidated: Vec<String> = Vec::new();
        zbus::Message::signal(
            HEADPHONES,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
        )
        .expect("signal builder")
        .build(&("org.bluez.Device1", changed, invalidated))
        .expect("build PropertiesChanged")
    }

    fn not_there(name: &str) -> BusError {
        BusError::Permanent {
            reason: "not there".to_owned(),
            dbus_name: Some(name.to_owned()),
        }
    }

    /// A per-device subscription with no bus behind it, naming its device.
    struct FakeSub(String);

    /// What bluez holds, by path.
    type HeldMap = RefCell<HashMap<&'static str, Held>>;

    /// A change bluez makes while a read is in flight ([`FakeBluez::during_read`]).
    type DuringRead = Box<dyn FnOnce(&HeldMap)>;

    /// A [`FoldBus`] with no bus: bluez's objects are `held`, reads are
    /// answered from them, and every subscription and read is recorded.
    #[derive(Default)]
    struct FakeBluez {
        held: HeldMap,
        /// `subscribe_device` calls, in order.
        subscriptions: RefCell<Vec<String>>,
        /// `GetManagedObjects` calls.
        managed_reads: Cell<u32>,
        /// `GetAll` calls, in order.
        reads: RefCell<Vec<(String, &'static str)>>,
        /// When set, every subscription asks for its re-read at once, as its
        /// forwarder does once the rule is in place.
        rules_land: Option<tokio::sync::mpsc::UnboundedSender<DeviceMsg>>,
        /// Run once, while the fold's `GetManagedObjects` is in flight: after
        /// bluez has taken its answer, before the fold has applied it. Handed
        /// what bluez holds, so a change it makes there (bluez's state first,
        /// then its signal, as bluez does) is what a later read answers.
        during_read: RefCell<Option<DuringRead>>,
    }

    impl FakeBluez {
        fn holding(held: &[(&'static str, Held)]) -> Self {
            Self {
                held: RefCell::new(held.iter().cloned().collect()),
                ..Self::default()
            }
        }
    }

    impl FoldBus for FakeBluez {
        type DeviceSub = FakeSub;

        fn subscribe_device(&self, path: &str) -> FakeSub {
            self.subscriptions.borrow_mut().push(path.to_owned());
            if let Some(tx) = &self.rules_land {
                let _ = tx.send(DeviceMsg::Reread {
                    path: path.to_owned(),
                });
            }
            FakeSub(path.to_owned())
        }

        fn managed_objects(&self) -> impl Future<Output = Result<ManagedObjects, BusError>> {
            self.managed_reads.set(self.managed_reads.get() + 1);
            let mut managed: ManagedObjects = HashMap::from([(
                zbus::zvariant::OwnedObjectPath::try_from(ADAPTER).expect("path"),
                HashMap::from([("org.bluez.Adapter1".to_owned(), HashMap::new())]),
            )]);
            for (path, held) in self.held.borrow().iter() {
                managed.insert(
                    zbus::zvariant::OwnedObjectPath::try_from(*path).expect("path"),
                    interfaces(held),
                );
            }
            if let Some(during_read) = self.during_read.borrow_mut().take() {
                during_read(&self.held);
            }
            std::future::ready(Ok(managed))
        }

        fn get_all(
            &self,
            path: &str,
            iface: &'static str,
        ) -> impl Future<Output = Result<HashMap<String, OwnedValue>, BusError>> {
            self.reads.borrow_mut().push((path.to_owned(), iface));
            let held = self.held.borrow().get(path).cloned();
            std::future::ready(match (held, iface) {
                (None, _) => Err(not_there("org.freedesktop.DBus.Error.UnknownObject")),
                (Some(held), "org.bluez.Device1") => Ok(device_props(held.alias, held.connected)),
                (
                    Some(Held {
                        battery: Some(percentage),
                        ..
                    }),
                    "org.bluez.Battery1",
                ) => Ok(battery_props(percentage)),
                (Some(_), _) => Err(not_there("org.freedesktop.DBus.Error.InvalidArgs")),
            })
        }
    }

    fn state() -> State {
        State::new(Mutable::new(None), Mutable::new(Vec::new()))
    }

    /// The published list as `(alias, connected, battery)`, sorted by alias.
    fn published(state: &State) -> Vec<(String, bool, Option<u8>)> {
        let mut list: Vec<_> = state
            .devices
            .get_cloned()
            .into_iter()
            .map(|dev| (dev.alias, dev.connected, dev.battery))
            .collect();
        list.sort();
        list
    }

    fn row(alias: &str, connected: bool, battery: Option<u8>) -> (String, bool, Option<u8>) {
        (alias.to_owned(), connected, battery)
    }

    /// An `InterfacesAdded` that reaches the fold after its resync can be
    /// older than the resync's read (`hytte-bus` delivers an emission made
    /// between the `AddMatch` and the read, and it can come out after the
    /// answer). For a device the map already has it must change nothing:
    /// the device keeps the newer state, and gets no second entry and no
    /// second subscription (#1551).
    ///
    /// **Falsification:** let `apply_added` overwrite a device the map has
    /// (the pre-#1551 insert) and the device rolls back to the properties it
    /// was announced with.
    #[tokio::test]
    async fn a_replayed_add_does_not_roll_a_device_back() {
        let bus = FakeBluez::holding(&[(
            HEADPHONES,
            Held {
                alias: "Headphones",
                connected: true,
                battery: Some(80),
            },
        )]);
        let state = state();
        let (wake, _wakes) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
        let mut subs = HashMap::new();
        resync(&state, ADAPTER, &bus, &mut subs)
            .await
            .expect("the adapter is there");

        // The add bluez emitted when the device appeared, before the read.
        let announced = Held {
            alias: "dev_AA",
            connected: false,
            battery: Some(95),
        };
        for _ in 0..2 {
            handle_ifaces_added(
                &state,
                ADAPTER,
                &interfaces_added(HEADPHONES, &interfaces(&announced)),
                &bus,
                &wake,
                &mut subs,
            )
            .await;
        }

        assert_eq!(published(&state), [row("Headphones", true, Some(80))]);
        assert_eq!(subs.len(), 1);
        assert_eq!(
            *bus.subscriptions.borrow(),
            [HEADPHONES],
            "the resync subscribed the device; the replays subscribed nothing"
        );
    }

    /// The same for a battery: a `Battery1` add for a device whose battery
    /// the map knows changes nothing, and one for a device whose battery it
    /// does not know fills it in (the device connecting).
    ///
    /// **Falsification:** let a `Battery1` add overwrite a known battery and
    /// the percentage rolls back.
    #[tokio::test]
    async fn a_battery_add_fills_in_and_never_overwrites() {
        let state = state();
        let held = |battery| Held {
            alias: "Headphones",
            connected: true,
            battery,
        };
        let battery_only = |percentage| {
            HashMap::from([("org.bluez.Battery1".to_owned(), battery_props(percentage))])
        };
        state.devices_map.lock().await.insert(
            HEADPHONES.to_owned(),
            device_from(HEADPHONES, &device_props("Headphones", true), None),
        );

        assert_eq!(
            apply_added(&state, ADAPTER, HEADPHONES, &battery_only(60)).await,
            Added::BatteryFilled
        );
        assert_eq!(
            apply_added(&state, ADAPTER, HEADPHONES, &battery_only(95)).await,
            Added::Known
        );
        assert_eq!(published(&state), [row("Headphones", true, Some(60))]);
        assert_eq!(
            apply_added(&state, ADAPTER, PHONE, &battery_only(50)).await,
            Added::Ignored,
            "a battery for a device the map does not have"
        );
        assert_eq!(
            apply_added(&state, ADAPTER, HEADPHONES, &interfaces(&held(Some(10)))).await,
            Added::Known
        );
        assert_eq!(published(&state), [row("Headphones", true, Some(60))]);
    }

    /// A device removed before the resync's read cannot come back from a
    /// replay. Its removal, replayed, changes nothing; its add, replayed,
    /// inserts it as a new device, but that subscription reads it again
    /// once its rule is in place, and bluez answering that it is not there
    /// drops it, subscription included (#1551).
    ///
    /// **Falsification:** leave an added device unsubscribed, keep a device
    /// the re-read says is gone, or stop counting `UnknownObject` as gone,
    /// and the device or its subscription stays.
    #[tokio::test]
    async fn a_replayed_add_cannot_keep_a_removed_device() {
        let bus = FakeBluez::default();
        let state = state();
        let (wake, _wakes) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
        let mut subs = HashMap::new();
        resync(&state, ADAPTER, &bus, &mut subs)
            .await
            .expect("the adapter is there");

        let removed = handle_ifaces_removed(
            &state,
            ADAPTER,
            &interfaces_removed(GONE, &["org.bluez.Device1"]),
            &mut subs,
        )
        .await;
        assert!(!removed, "a device, not the adapter");
        assert!(published(&state).is_empty());

        let announced = Held {
            alias: "Gone",
            connected: false,
            battery: None,
        };
        handle_ifaces_added(
            &state,
            ADAPTER,
            &interfaces_added(GONE, &interfaces(&announced)),
            &bus,
            &wake,
            &mut subs,
        )
        .await;
        assert_eq!(
            *bus.subscriptions.borrow(),
            [GONE],
            "a device the map did not have is subscribed, and so read again"
        );

        // Its forwarder's re-read, once the rule is in place.
        reread_device(&state, GONE, &bus, &mut subs).await;
        assert!(published(&state).is_empty(), "bluez says it is gone");
        assert!(subs.is_empty(), "and its subscription went with it");
    }

    /// A device subscribed after its state was read is read again once its
    /// rule is in place, and that read is newer than the add it came from:
    /// a change bluez made before the rule (here the name resolving) shows
    /// (#1551).
    ///
    /// **Falsification:** make `apply_reread` keep the entry it has and the
    /// device keeps its address for a name.
    #[tokio::test]
    async fn a_device_read_again_is_newer_than_its_add() {
        let bus = FakeBluez::holding(&[(
            PHONE,
            Held {
                alias: "Phone",
                connected: true,
                battery: None,
            },
        )]);
        let state = state();
        let (wake, _wakes) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
        let mut subs = HashMap::new();

        let announced = Held {
            alias: "dev_BB",
            connected: false,
            battery: None,
        };
        handle_ifaces_added(
            &state,
            ADAPTER,
            &interfaces_added(PHONE, &interfaces(&announced)),
            &bus,
            &wake,
            &mut subs,
        )
        .await;
        assert_eq!(published(&state), [row("dev_BB", false, None)]);

        reread_device(&state, PHONE, &bus, &mut subs).await;
        assert_eq!(published(&state), [row("Phone", true, None)]);
        assert_eq!(
            *bus.reads.borrow(),
            [(PHONE.to_owned(), "org.bluez.Device1")],
            "no battery is asked for when the map knows of none"
        );
        assert_eq!(
            subs.len(),
            1,
            "a device that is there keeps its subscription"
        );
    }

    /// A re-read for a device removed since it was asked for reads nothing,
    /// so it neither calls bluez nor brings the device back; and a re-read
    /// answered for a device the map no longer has inserts nothing.
    ///
    /// **Falsification:** drop the "not in the map" check from
    /// `reread_device` and bluez is called; drop it from `apply_reread` and
    /// the device comes back.
    #[tokio::test]
    async fn a_re_read_never_brings_a_device_back() {
        let bus = FakeBluez::holding(&[(
            PHONE,
            Held {
                alias: "Phone",
                connected: true,
                battery: None,
            },
        )]);
        let state = state();
        let mut subs: HashMap<String, FakeSub> = HashMap::new();

        reread_device(&state, PHONE, &bus, &mut subs).await;
        assert!(bus.reads.borrow().is_empty(), "nothing to read");

        let read = DeviceRead::Found {
            device: device_props("Phone", true),
            battery: None,
        };
        assert!(!apply_reread(&state, PHONE, read).await);
        assert!(state.devices_map.lock().await.is_empty());
    }

    /// A re-read keeps the battery the map has unless bluez answered for it,
    /// and asks for it only when the map knows of one.
    #[tokio::test]
    async fn a_re_read_reads_a_known_battery() {
        let bus = FakeBluez::holding(&[(
            HEADPHONES,
            Held {
                alias: "Headphones",
                connected: true,
                battery: Some(40),
            },
        )]);
        let state = state();
        let mut subs: HashMap<String, FakeSub> = HashMap::new();
        state.devices_map.lock().await.insert(
            HEADPHONES.to_owned(),
            device_from(
                HEADPHONES,
                &device_props("Headphones", true),
                Some(&battery_props(90)),
            ),
        );

        reread_device(&state, HEADPHONES, &bus, &mut subs).await;
        assert_eq!(published(&state), [row("Headphones", true, Some(40))]);

        // bluez does not answer for the battery: the map's stays.
        bus.held
            .borrow_mut()
            .get_mut(HEADPHONES)
            .expect("held")
            .battery = None;
        reread_device(&state, HEADPHONES, &bus, &mut subs).await;
        assert_eq!(published(&state), [row("Headphones", true, Some(40))]);
    }

    /// A re-read that finds the device as the map has it publishes nothing,
    /// and one that finds it changed publishes the change (#1647 review,
    /// LOW 4). Every device a resync subscribes is read again moments after
    /// that resync read it, and publishing an unchanged list re-rendered the
    /// panel's list once more per device.
    ///
    /// **Falsification:** publish on every `Found` (the #1647 head) and the
    /// unchanged re-read overwrites the marker list below.
    #[tokio::test]
    async fn a_re_read_publishes_only_a_change() {
        let bus = FakeBluez::holding(&[(
            PHONE,
            Held {
                alias: "Phone",
                connected: false,
                battery: None,
            },
        )]);
        let state = state();
        let mut subs: HashMap<String, FakeSub> = HashMap::new();
        state.devices_map.lock().await.insert(
            PHONE.to_owned(),
            device_from(PHONE, &device_props("Phone", false), None),
        );
        // A marker in the published list: a publish replaces it.
        let marker = || vec![device("/marker", "not published since", false, false)];
        state.devices.set(marker());

        reread_device(&state, PHONE, &bus, &mut subs).await;
        assert_eq!(bus.reads.borrow().len(), 1, "the round trip is still made");
        assert_eq!(
            published(&state),
            [row("not published since", false, None)],
            "nothing changed, so nothing was published"
        );

        bus.held
            .borrow_mut()
            .get_mut(PHONE)
            .expect("held")
            .connected = true;
        reread_device(&state, PHONE, &bus, &mut subs).await;
        assert_eq!(published(&state), [row("Phone", true, None)]);
    }

    /// What counts as "the device is not there": the three answers bluez
    /// gives for one (and the generic one), and nothing that only says the
    /// call failed.
    #[test]
    fn only_a_not_there_answer_drops_a_device() {
        for name in [
            "org.freedesktop.DBus.Error.UnknownObject",
            "org.freedesktop.DBus.Error.UnknownMethod",
            "org.freedesktop.DBus.Error.InvalidArgs",
            "org.freedesktop.DBus.Error.UnknownInterface",
        ] {
            assert!(device_is_gone(&not_there(name)), "{name}");
        }
        for name in [
            "org.freedesktop.DBus.Error.NoReply",
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "org.freedesktop.DBus.Error.Timeout",
            "org.bluez.Error.Failed",
        ] {
            assert!(!device_is_gone(&not_there(name)), "{name}");
        }
        assert!(!device_is_gone(&BusError::Timeout {
            reason: "deadline".to_owned()
        }));
    }

    /// A resync replaces the map with what bluez holds, drops the
    /// subscription of a device it no longer has, and subscribes a device
    /// it finds (which reads it again once its rule is in place). That is
    /// also how `listen`'s snapshot devices get their subscriptions: the
    /// first resync finds them with none.
    ///
    /// **Falsification:** leave a device the resync finds unsubscribed, or
    /// keep a device it does not find.
    #[tokio::test]
    async fn a_resync_subscribes_what_it_finds_with_a_re_read() {
        let bus = FakeBluez::holding(&[(
            PHONE,
            Held {
                alias: "Phone",
                connected: false,
                battery: None,
            },
        )]);
        let state = state();
        state.devices_map.lock().await.insert(
            GONE.to_owned(),
            device_from(GONE, &device_props("Gone", false), None),
        );
        let mut subs = HashMap::from([(GONE.to_owned(), FakeSub(GONE.to_owned()))]);

        resync(&state, ADAPTER, &bus, &mut subs)
            .await
            .expect("the adapter is there");

        assert_eq!(published(&state), [row("Phone", false, None)]);
        assert_eq!(
            subs.iter()
                .map(|(path, sub)| (path.as_str(), sub.0.as_str()))
                .collect::<Vec<_>>(),
            [(PHONE, PHONE)]
        );
        assert_eq!(*bus.subscriptions.borrow(), [PHONE]);
    }

    /// The fold end to end, on the handshake's resync and the replays that
    /// can follow it (#1551). The fold starts as `event_loop` starts it: the
    /// resync queued and no device subscribed. While the resync's read is in
    /// flight bluez changes a device's name (newer than the answer), and two
    /// adds older than the answer come out of their receiver after it, which
    /// is what a replay is: one for a device the answer has, one for a
    /// device bluez removed before it. The read replaces `listen`'s older
    /// snapshot and subscribes both devices it found, the newer change lands
    /// on top of it, neither replay rolls a device back or keeps one bluez no
    /// longer has, and every subscribed device is read again.
    ///
    /// **Falsification:** ignore `DeviceMsg::Reread` in the loop, or leave
    /// `listen`'s snapshot devices unsubscribed, and the published list or
    /// the subscriptions are wrong.
    #[tokio::test(start_paused = true)]
    async fn the_fold_settles_on_bluezs_state_after_its_resync() {
        let (props_tx, props_rx) = tokio::sync::mpsc::unbounded_channel::<DeviceMsg>();
        let (resync_tx, resync_rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
        let (added_tx, added_rx) = tokio::sync::mpsc::unbounded_channel::<SignalItem>();
        let mut bus = FakeBluez::holding(&[
            (
                HEADPHONES,
                Held {
                    alias: "Headphones",
                    connected: true,
                    battery: None,
                },
            ),
            (
                PHONE,
                Held {
                    alias: "Phone",
                    connected: false,
                    battery: None,
                },
            ),
        ]);
        bus.rules_land = Some(props_tx.clone());
        let renamed = props_tx.clone();
        // A clone, so `added_tx` itself keeps the stream open past the
        // replays: a stream that ends ends the fold.
        let replayed = added_tx.clone();
        *bus.during_read.borrow_mut() = Some(Box::new(move |held: &HeldMap| {
            held.borrow_mut().get_mut(HEADPHONES).expect("held").alias = "Headphones (2)";
            let newer = HashMap::from([("Alias".to_owned(), make_owned_value("Headphones (2)"))]);
            let _ = renamed.send(DeviceMsg::Props {
                path: HEADPHONES.to_owned(),
                body: device_changed(&newer),
            });
            for (path, alias) in [(HEADPHONES, "dev_AA"), (GONE, "Gone")] {
                let announced = Held {
                    alias,
                    connected: false,
                    battery: None,
                };
                let _ = replayed.send(SignalItem::Event(SignalEvent {
                    body: interfaces_added(path, &interfaces(&announced)),
                    sender: None,
                    timestamp: std::time::SystemTime::now(),
                }));
            }
        }));

        // `listen`'s snapshot: older than the resync's read.
        let state = state();
        state.devices_map.lock().await.insert(
            HEADPHONES.to_owned(),
            device_from(HEADPHONES, &device_props("dev_AA", false), None),
        );

        // What `open_fold` leaves queued once every rule is in place.
        arm_resync(&resync_tx, "the handshake");
        let added = futures_util::stream::unfold(added_rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })
        .boxed();
        let quiet = || futures_util::stream::pending::<SignalItem>().boxed();

        let settled = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fold(
                &state,
                ADAPTER,
                &bus,
                FoldStreams {
                    owner: quiet(),
                    added,
                    removed: quiet(),
                    adapter: quiet(),
                },
                FoldQueues {
                    props_rx,
                    resync_tx,
                    resync_rx,
                },
                // `event_loop` subscribes no device before the resync.
                HashMap::new(),
            ),
        )
        .await;
        assert!(settled.is_err(), "the fold runs until the watch ends");
        drop(added_tx);

        assert_eq!(
            published(&state),
            [row("Headphones (2)", true, None), row("Phone", false, None)]
        );
        let mut subscribed = bus.subscriptions.borrow().clone();
        subscribed.sort();
        assert_eq!(
            subscribed,
            [HEADPHONES, PHONE, GONE],
            "the two devices the read found, and the one the replay brought in"
        );
        assert_eq!(bus.managed_reads.get(), 1, "one resync");
    }

    /// A device bluez removed and added back, whose removal and add are both
    /// queued, ends up in the map: the removal is taken first. Taken the other
    /// way round the add is `Known` (a no-op) and the removal drops a device
    /// bluez has (#1647 review, the fold's "Arm order").
    ///
    /// **Falsification:** put the `InterfacesAdded` arm above the
    /// `InterfacesRemoved` one and the device is gone; drop `biased;` and it
    /// is gone in about half the rounds.
    #[tokio::test(start_paused = true)]
    async fn a_device_removed_and_added_back_stays() {
        for round in 0..32 {
            let (props_tx, props_rx) = tokio::sync::mpsc::unbounded_channel::<DeviceMsg>();
            let (resync_tx, resync_rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
            let phone = Held {
                alias: "Phone",
                connected: false,
                battery: None,
            };
            let mut bus = FakeBluez::holding(&[(PHONE, phone.clone())]);
            bus.rules_land = Some(props_tx);
            let state = state();
            state.devices_map.lock().await.insert(
                PHONE.to_owned(),
                device_from(PHONE, &device_props("Phone", false), None),
            );
            let event = |body| {
                futures_util::stream::iter([SignalItem::Event(SignalEvent {
                    body,
                    sender: None,
                    timestamp: std::time::SystemTime::now(),
                })])
                .chain(futures_util::stream::pending())
                .boxed()
            };

            let settled = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                fold(
                    &state,
                    ADAPTER,
                    &bus,
                    FoldStreams {
                        owner: futures_util::stream::pending::<SignalItem>().boxed(),
                        added: event(interfaces_added(PHONE, &interfaces(&phone))),
                        removed: event(interfaces_removed(PHONE, &["org.bluez.Device1"])),
                        adapter: futures_util::stream::pending::<SignalItem>().boxed(),
                    },
                    FoldQueues {
                        props_rx,
                        resync_tx,
                        resync_rx,
                    },
                    HashMap::from([(PHONE.to_owned(), FakeSub(PHONE.to_owned()))]),
                ),
            )
            .await;
            assert!(settled.is_err(), "the fold runs until the watch ends");
            assert_eq!(
                published(&state),
                [row("Phone", false, None)],
                "round {round}: the device bluez has must stay"
            );
        }
    }

    /// bluetoothd leaving while reads are queued behind it: its `Lost` is
    /// taken before any of them, so the fold ends without calling `org.bluez`
    /// again. On a system bus a call to that name with no owner D-Bus-activates
    /// the bluetoothd that was just stopped (#1647 review, LOW 3: the
    /// handshake's resync beat a queued `Lost` 14 times in 20 there, against a
    /// real broker). Here a resync, a device re-read and an `InterfacesAdded`
    /// for a new device are all queued with the `Lost`.
    ///
    /// **Falsification:** drop `biased;` from the fold's `select!`, or move
    /// the owner arm down, and some of the rounds call bluez.
    #[tokio::test(start_paused = true)]
    async fn a_queued_lost_ends_the_fold_before_any_queued_read() {
        let lost = || {
            let body = zbus::Message::signal(
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "NameOwnerChanged",
            )
            .expect("signal builder")
            .build(&("org.bluez", ":1.4", ""))
            .expect("build NameOwnerChanged");
            futures_util::stream::iter([SignalItem::Event(SignalEvent {
                body,
                sender: None,
                timestamp: std::time::SystemTime::now(),
            })])
            .chain(futures_util::stream::pending())
            .boxed()
        };
        for round in 0..32 {
            let (props_tx, props_rx) = tokio::sync::mpsc::unbounded_channel::<DeviceMsg>();
            let (resync_tx, resync_rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
            let bus = FakeBluez::holding(&[(
                PHONE,
                Held {
                    alias: "Phone",
                    connected: false,
                    battery: None,
                },
            )]);
            let state = state();
            state.devices_map.lock().await.insert(
                PHONE.to_owned(),
                device_from(PHONE, &device_props("Phone", false), None),
            );
            arm_resync(&resync_tx, "the handshake");
            props_tx
                .send(DeviceMsg::Reread {
                    path: PHONE.to_owned(),
                })
                .expect("the fold's queue");
            let added = futures_util::stream::iter([SignalItem::Event(SignalEvent {
                body: interfaces_added(
                    HEADPHONES,
                    &interfaces(&Held {
                        alias: "Headphones",
                        connected: false,
                        battery: None,
                    }),
                ),
                sender: None,
                timestamp: std::time::SystemTime::now(),
            })])
            .chain(futures_util::stream::pending())
            .boxed();

            let ended = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                fold(
                    &state,
                    ADAPTER,
                    &bus,
                    FoldStreams {
                        owner: lost(),
                        added,
                        removed: futures_util::stream::pending::<SignalItem>().boxed(),
                        adapter: futures_util::stream::pending::<SignalItem>().boxed(),
                    },
                    FoldQueues {
                        props_rx,
                        resync_tx,
                        resync_rx,
                    },
                    HashMap::from([(PHONE.to_owned(), FakeSub(PHONE.to_owned()))]),
                ),
            )
            .await
            .expect("a Lost ends the fold");
            assert_eq!(
                ended.expect("no error"),
                ListenEnd::DaemonExited,
                "round {round}"
            );
            assert_eq!(bus.managed_reads.get(), 0, "round {round}: a resync ran");
            assert!(
                bus.reads.borrow().is_empty(),
                "round {round}: a device was read"
            );
            assert!(
                bus.subscriptions.borrow().is_empty(),
                "round {round}: an add was applied"
            );
        }
    }

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

    // ── bluetoothd restarts: NameOwnerChanged + a re-announced adapter ──────

    fn noc_item(name: &str, old: &str, new: &str) -> SignalItem {
        let body = zbus::Message::signal(
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameOwnerChanged",
        )
        .expect("signal builder")
        .build(&(name, old, new))
        .expect("build NameOwnerChanged");
        SignalItem::Event(SignalEvent {
            body,
            sender: None,
            timestamp: std::time::SystemTime::now(),
        })
    }

    /// bluetoothd exiting ends the watch, so the supervised loop clears the
    /// snapshot. Before this, nothing in the watcher noticed, and the old
    /// daemon's devices and `Discovering = true` stayed up.
    ///
    /// Falsifiable: make `Lost` return `None` and the `DaemonExited`
    /// assertion fails.
    #[test]
    fn bluetoothd_exiting_ends_the_watch() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
        assert_eq!(
            on_owner_item(noc_item("org.bluez", ":1.4", ""), &tx),
            Some(ListenEnd::DaemonExited),
            "a lost org.bluez must end the watch"
        );
        assert!(
            rx.try_recv().is_err(),
            "and must not re-sync: that is a call to org.bluez, which would start it again"
        );
    }

    /// A new bluetoothd arms exactly one re-sync, whether or not this
    /// subscription saw the old one go.
    #[test]
    fn a_new_bluetoothd_arms_one_resync() {
        for (old, new) in [("", ":1.9"), (":1.4", ":1.9")] {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
            assert_eq!(on_owner_item(noc_item("org.bluez", old, new), &tx), None);
            assert!(rx.try_recv().is_ok(), "{old:?} -> {new:?} must re-sync");
            assert!(rx.try_recv().is_err(), "exactly once");
        }
    }

    /// Every other name on the system bus passes through untouched. The
    /// match rule is unfiltered, so most of what arrives here is someone
    /// else's.
    #[test]
    fn other_names_are_ignored() {
        for name in ["org.freedesktop.UPower", "org.bluez.obex", ":1.4"] {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
            assert_eq!(on_owner_item(noc_item(name, ":1.4", ""), &tx), None);
            assert!(rx.try_recv().is_err(), "{name} must not re-sync bluetooth");
        }
    }

    /// A marker on this subscription may hide an owner change, so it
    /// re-syncs like any other marker in the fold.
    #[test]
    fn a_marker_on_the_owner_subscription_resyncs() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
        assert_eq!(on_owner_item(SignalItem::Resubscribed, &tx), None);
        assert!(rx.try_recv().is_ok());
    }

    /// Waiting for bluetoothd wakes on its new owner, and only on that.
    /// Someone else's name coming or going, and `org.bluez` going away, do
    /// not count.
    ///
    /// Falsifiable: return on any `Event` in `next_bluez_owner` and the
    /// second half fails, because the wait ends on an unrelated name.
    #[tokio::test(start_paused = true)]
    async fn waiting_for_bluez_wakes_on_its_new_owner_only() {
        let noise = || {
            futures_util::stream::iter([
                noc_item("org.freedesktop.UPower", "", ":1.3"),
                noc_item("org.bluez", ":1.4", ""),
                noc_item(":1.5", ":1.5", ""),
            ])
        };

        let mut items = noise()
            .chain(futures_util::stream::iter([noc_item(
                "org.bluez",
                "",
                ":1.9",
            )]))
            .chain(futures_util::stream::pending());
        tokio::time::timeout(BLUEZ_RECHECK, next_bluez_owner(&mut items))
            .await
            .expect("a new org.bluez owner must end the wait");

        let mut items = noise().chain(futures_util::stream::pending());
        assert!(
            tokio::time::timeout(BLUEZ_RECHECK, next_bluez_owner(&mut items))
                .await
                .is_err(),
            "nothing here announces a new bluetoothd"
        );
    }

    /// A marker ends the wait so the caller asks the bus again: the owner
    /// change it may hide could be the one being waited for. A stream that
    /// ends does not end the wait, so the caller's re-check interval paces
    /// it instead of a busy loop.
    #[tokio::test(start_paused = true)]
    async fn waiting_for_bluez_rechecks_on_a_marker_and_idles_on_a_closed_stream() {
        let mut items = futures_util::stream::iter([SignalItem::Lagged { skipped: 3 }])
            .chain(futures_util::stream::pending());
        tokio::time::timeout(BLUEZ_RECHECK, next_bluez_owner(&mut items))
            .await
            .expect("a marker must end the wait");

        let mut items = futures_util::stream::empty::<SignalItem>();
        assert!(
            tokio::time::timeout(BLUEZ_RECHECK, next_bluez_owner(&mut items))
                .await
                .is_err(),
            "a closed stream must leave the pacing to the timeout"
        );
    }

    /// The tracked adapter being announced again is a new daemon's adapter:
    /// re-read everything. Before this fix, the branch for it required an
    /// empty `adapter_path`, which the loop never has, so it never ran.
    #[test]
    fn the_adapter_reannounced_at_its_own_path_resyncs() {
        let ifaces =
            |iface: &str| HashMap::from([(iface.to_owned(), HashMap::<String, OwnedValue>::new())]);
        let adapter = "/org/bluez/hci0";

        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
        assert!(adapter_reappeared(
            adapter,
            &ifaces("org.bluez.Adapter1"),
            adapter,
            &tx
        ));
        assert!(
            rx.try_recv().is_ok(),
            "the re-announced adapter must re-sync"
        );

        // A second adapter elsewhere, and an ordinary device, are not it.
        assert!(!adapter_reappeared(
            "/org/bluez/hci1",
            &ifaces("org.bluez.Adapter1"),
            adapter,
            &tx
        ));
        assert!(!adapter_reappeared(
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF",
            &ifaces("org.bluez.Device1"),
            adapter,
            &tx
        ));
        assert!(rx.try_recv().is_err(), "neither may re-sync");
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
        let (props_tx, props_rx) = tokio::sync::mpsc::unbounded_channel::<DeviceMsg>();
        let (resync_tx, resync_rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);

        // 1. The subscription was torn down: the stream ends, so does the task.
        let ended = spawn_device_forwarder(
            "/dev/gone".to_string(),
            futures_util::stream::empty::<SignalItem>(),
            NO_REREAD,
            props_tx.clone(),
            resync_tx.clone(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), ended)
            .await
            .expect("a forwarder whose stream ended must exit")
            .expect("forwarder task panicked");

        // 1b. The same while its re-read still waits for a rule that will
        //     never be installed: the stream ending is what ends it.
        let ended = spawn_device_forwarder(
            "/dev/gone-before-its-rule".to_string(),
            futures_util::stream::empty::<SignalItem>(),
            Some(std::future::pending::<()>()),
            props_tx.clone(),
            resync_tx.clone(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), ended)
            .await
            .expect("a forwarder whose stream ended must exit, re-read or not")
            .expect("forwarder task panicked");

        // 2. The event loop is gone: an endless stream of markers must not
        //    keep the task (or a busy loop) alive.
        drop(props_rx);
        drop(resync_rx);
        let orphaned = spawn_device_forwarder(
            "/dev/x".to_string(),
            futures_util::stream::repeat(SignalItem::Resubscribed),
            NO_REREAD,
            props_tx,
            resync_tx,
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), orphaned)
            .await
            .expect("a forwarder nobody listens to must give up")
            .expect("forwarder task panicked");
    }

    /// The next message a forwarder sends, within `budget` (a liveness
    /// guard, not a latency assertion).
    async fn next_within(
        budget: std::time::Duration,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<DeviceMsg>,
    ) -> DeviceMsg {
        tokio::time::timeout(budget, rx.recv())
            .await
            .expect("the forwarder sent nothing")
            .expect("the forwarder dropped its sender")
    }

    /// A forwarder handed its subscription's handshake asks for one re-read,
    /// once the handshake resolves and not before, and goes on forwarding
    /// the device's emissions on either side of it (#1551). One handed none
    /// never asks.
    ///
    /// **Falsification:** ask before the handshake resolves, never ask, or
    /// ask again (polling the finished handshake again panics the task).
    #[tokio::test(flavor = "multi_thread")]
    async fn the_forwarder_asks_for_one_re_read_once_the_rule_is_installed() {
        const BUDGET: std::time::Duration = std::time::Duration::from_secs(30);
        let describe = |msg: &DeviceMsg| match msg {
            DeviceMsg::Props { path, .. } => format!("props {path}"),
            DeviceMsg::Reread { path } => format!("reread {path}"),
        };

        for asks in [true, false] {
            let (items_tx, items_rx) = tokio::sync::mpsc::unbounded_channel::<SignalItem>();
            let items = futures_util::stream::unfold(items_rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            })
            .boxed();
            let (installed_tx, installed_rx) = tokio::sync::oneshot::channel::<()>();
            let installed = asks.then_some(async move {
                let _ = installed_rx.await;
            });
            let (props_tx, mut props_rx) = tokio::sync::mpsc::unbounded_channel::<DeviceMsg>();
            let (resync_tx, _resync_rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
            let forwarder =
                spawn_device_forwarder("/dev/a".to_owned(), items, installed, props_tx, resync_tx);

            // Before the rule: emissions are forwarded, and no re-read is
            // asked for, since nothing has resolved the handshake.
            items_tx
                .send(SignalItem::Event(signal_event()))
                .expect("forwarder listening");
            assert_eq!(
                describe(&next_within(BUDGET, &mut props_rx).await),
                "props /dev/a"
            );

            let _ = installed_tx.send(());
            items_tx
                .send(SignalItem::Event(signal_event()))
                .expect("forwarder listening");
            let mut after = vec![describe(&next_within(BUDGET, &mut props_rx).await)];
            if asks {
                after.push(describe(&next_within(BUDGET, &mut props_rx).await));
                after.sort();
            }
            let expected: &[&str] = if asks {
                &["props /dev/a", "reread /dev/a"]
            } else {
                &["props /dev/a"]
            };
            assert_eq!(after, expected, "asks: {asks}");

            // Once: the stream ends with nothing more sent, and the task did
            // not panic on a finished handshake.
            items_tx
                .send(SignalItem::Event(signal_event()))
                .expect("forwarder listening");
            drop(items_tx);
            tokio::time::timeout(BUDGET, forwarder)
                .await
                .expect("the forwarder must exit with its stream")
                .expect("forwarder task panicked");
            let rest: Vec<String> = std::iter::from_fn(|| props_rx.try_recv().ok())
                .map(|msg| describe(&msg))
                .collect();
            assert_eq!(rest, ["props /dev/a"], "asks: {asks}");
        }
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
    use super::{DeviceMsg, RESYNC_WAKE_QUEUE, forward_device, spawn_device_forwarder};

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

        let (props_tx, _props_rx) = tokio::sync::mpsc::unbounded_channel::<DeviceMsg>();
        let (resync_tx, _resync_rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
        // With its handshake too, as `forward_device` hands every forwarder
        // one: holding that future must not keep the subscription up.
        let forwarder = spawn_device_forwarder(
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF".to_string(),
            sub.items(),
            Some(sub.subscribed()),
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

    /// `subscribe_then_snapshot` with a **real** subscription: the snapshot
    /// is not asked for while the bus has no match rule, and is once it has.
    /// The order test drives a recording double, so it cannot see the
    /// production `impl Handshake for SignalSubscription` the watchers go
    /// through (`listen` here, `open_discovery` in `wifi::watcher`, both the
    /// same impl); this does (#1645 review).
    ///
    /// **Falsification:** return `std::future::ready(())` from that impl and
    /// the snapshot is taken on the first poll.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_snapshot_waits_for_a_real_subscriptions_match_rule() {
        use futures_util::FutureExt;
        use std::sync::atomic::{AtomicBool, Ordering};

        let guard = ephemeral_bus().await;
        let conn = Builder::address(guard.address.as_str())
            .expect("bus address")
            .build()
            .await
            .expect("connect to ephemeral bus");
        let shared = SharedConnection::for_test_system(conn.clone());
        // No connection until the test installs one, so the rule cannot be
        // installed before then.
        shared.drop_connection_for_test().await;

        let taken = AtomicBool::new(false);
        let mut opened = std::pin::pin!(crate::handshake::subscribe_then_snapshot(
            || {
                let sub = signals_with(&shared, "org.bluez")
                    .at_path("/")
                    .iface("org.freedesktop.DBus.ObjectManager")
                    .signal("InterfacesAdded")
                    .start();
                let items = sub.items();
                (sub, items)
            },
            || {
                taken.store(true, Ordering::SeqCst);
                async {}
            },
        ));
        assert!(
            opened.as_mut().now_or_never().is_none(),
            "subscribe_then_snapshot finished with no match rule installed"
        );
        assert!(
            !taken.load(Ordering::SeqCst),
            "the snapshot was asked for before the bus had the match rule"
        );

        shared.install_fresh_connection_for_test(conn).await;
        let ((_subscription, _items), ()) = tokio::time::timeout(DBUS_REPLY_BUDGET, opened)
            .await
            .expect("the helper never went on once the rule was installed");
        assert!(taken.load(Ordering::SeqCst));
    }

    /// `forward_device` with a **real** subscription: the forwarder asks for
    /// its re-read once the bus has the device's match rule, and not while
    /// it has none (#1551). The
    /// hermetic forwarder test hands it a stand-in handshake, so it cannot
    /// see that `forward_device` wires the subscription's own `subscribed()`
    /// in; this does.
    ///
    /// **Falsification:** hand the forwarder no handshake and no re-read is
    /// ever asked for; hand it a ready one and the re-read is asked for with
    /// no rule installed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_device_is_read_again_once_its_real_rule_is_installed() {
        const PATH: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
        let guard = ephemeral_bus().await;
        let conn = Builder::address(guard.address.as_str())
            .expect("bus address")
            .build()
            .await
            .expect("connect to ephemeral bus");
        let shared = SharedConnection::for_test_system(conn.clone());
        // No connection until the test installs one, so the rule cannot be
        // installed before then.
        shared.drop_connection_for_test().await;

        let (props_tx, mut props_rx) = tokio::sync::mpsc::unbounded_channel::<DeviceMsg>();
        let (resync_tx, _resync_rx) = tokio::sync::mpsc::channel::<()>(RESYNC_WAKE_QUEUE);
        let sub = forward_device(
            signals_with(&shared, "org.bluez")
                .at_path(PATH)
                .iface("org.freedesktop.DBus.Properties")
                .signal("PropertiesChanged")
                .start(),
            PATH,
            props_tx,
            resync_tx,
        );

        // A bounded negative: with no connection the rule cannot be
        // installed, so nothing may arrive. A runtime too slow to run the
        // forwarder in this window passes it vacuously, never spuriously.
        assert!(
            tokio::time::timeout(Duration::from_millis(500), props_rx.recv())
                .await
                .is_err(),
            "the forwarder asked for a re-read with no match rule installed"
        );

        shared.install_fresh_connection_for_test(conn).await;
        let msg = tokio::time::timeout(DBUS_REPLY_BUDGET, props_rx.recv())
            .await
            .expect("no re-read once the rule was installed")
            .expect("the forwarder dropped its sender");
        assert!(
            matches!(&msg, DeviceMsg::Reread { path } if path == PATH),
            "the first message once the rule is in place is the re-read"
        );
        drop(sub);
    }
}

// The production `listen` → `event_loop` → `fold`, through the process-global
// system connection, against a real `dbus-daemon` and a scripted `org.bluez`,
// in a re-exec'd child (#1647 review R1–R3). A child module, not a file under
// `tests/`, because `listen` is private.
#[cfg(all(test, feature = "system-tests"))]
mod real_bus;
