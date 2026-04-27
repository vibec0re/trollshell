//! `BlueZ` bluetooth adapter + device tracking via the system D-Bus.
//!
//! Discovers the first adapter exposing `org.bluez.Adapter1` via
//! `ObjectManager.GetManagedObjects`, then watches `InterfacesAdded`,
//! `InterfacesRemoved`, and `PropertiesChanged` for live updates.
//!
//! # Public API
//!
//! ```ignore
//! // Register once at startup:
//! .with(bluetooth::service())
//!
//! // Subscribe in widgets:
//! bluetooth::adapter() -> impl Signal<Item = Option<Adapter>>
//! bluetooth::devices() -> impl Signal<Item = Vec<Device>>
//!
//! // Fire-and-forget commands:
//! bluetooth::set_powered(true);
//! bluetooth::set_discoverable(true);
//! bluetooth::start_discovery();
//! bluetooth::stop_discovery();
//! bluetooth::pair_device(path);     // Pair, then auto-trust + auto-connect on success
//! bluetooth::connect_device(path);
//! bluetooth::disconnect_device(path);
//! bluetooth::set_trusted(path, true);
//! bluetooth::remove_device(path);   // unpair / forget
//! ```

use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_bus::{BusKind, SignalSubscription};
use hytte_reactive::{registry, runtime, Service};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::sync::Mutex as AsyncMutex;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

// ── Public data shapes ────────────────────────────────────────────────────────

/// Snapshot of the Bluetooth adapter state.
#[derive(Clone, Debug, Default)]
pub struct Adapter {
    /// D-Bus object path, e.g. `"/org/bluez/hci0"`.
    pub path: String,
    pub address: String,
    pub name: String,
    pub powered: bool,
    pub discoverable: bool,
    pub discovering: bool,
}

/// What sort of pairing prompt the `BlueZ` agent is asking us to handle.
/// The UI uses this to choose copy/buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    /// "Confirm pairing with X — code 123456" style. The user matches the
    /// number against the one shown on the remote device. Most modern path.
    ConfirmPasskey,
    /// Bare "allow this device to pair?" without a code. Older or
    /// no-input devices.
    Authorize,
    /// Legacy: the device wants the user to type a free-form PIN string
    /// (length up to 16 chars, ASCII). Used by older pre-SSP devices.
    EnterPinCode,
    /// Legacy: the device wants the user to type a 0..=999999 numeric
    /// passkey. Pre-SSP path; rare on modern hardware.
    EnterPasskey,
}

/// A pending Bluetooth pairing prompt the user must accept or reject.
/// The agent suspends pairing on the `BlueZ` side until
/// `respond_to_prompt(...)` is called.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairPrompt {
    pub device_path: String,
    /// Resolved alias from the `devices()` snapshot at prompt time.
    /// Falls back to the bare path if the device isn't yet in our cache.
    pub alias: String,
    pub passkey: Option<u32>,
    pub kind: PromptKind,
}

/// Snapshot of a single paired/nearby Bluetooth device.
#[derive(Clone, Debug, Default)]
pub struct Device {
    /// D-Bus object path, e.g. `"/org/bluez/hci0/dev_XX_XX_XX_XX_XX_XX"`.
    pub path: String,
    pub address: String,
    /// User-friendly alias (falls back to Name then Address).
    pub alias: String,
    /// Freedesktop icon name from `BlueZ`, e.g. `"audio-headphones"`.  Empty
    /// when `BlueZ` doesn't report one.
    pub icon: String,
    pub paired: bool,
    pub connected: bool,
    pub trusted: bool,
    /// Battery percentage 0..=100, when the device exposes the
    /// `org.bluez.Battery1` interface (mostly headphones, mice, keyboards).
    /// `None` when `BlueZ` doesn't report one — either the device doesn't
    /// support it or it's currently disconnected.
    pub battery: Option<u8>,
}

// ── Adapter path storage ──────────────────────────────────────────────────────

/// Filled by the listen loop on adapter discovery; read by command fns.
static ADAPTER_PATH: OnceLock<String> = OnceLock::new();
/// Kept as a Mutable so commands can read the current value even after
/// the `OnceLock` is set (`OnceLock` is set-once; the adapter path doesn't
/// change within a process lifetime for the typical single-adapter case).
static ADAPTER_PATH_CELL: OnceLock<Arc<tokio::sync::RwLock<String>>> = OnceLock::new();

fn adapter_path_store() -> &'static Arc<tokio::sync::RwLock<String>> {
    ADAPTER_PATH_CELL.get_or_init(|| Arc::new(tokio::sync::RwLock::new(String::new())))
}

async fn get_adapter_path() -> String {
    adapter_path_store().read().await.clone()
}

async fn set_adapter_path(path: &str) {
    *adapter_path_store().write().await = path.to_string();
    let _ = ADAPTER_PATH.set(path.to_string());
}

// ── Service handle ────────────────────────────────────────────────────────────

/// Shared mutable state held in the service registry.
#[doc(hidden)]
pub struct BluetoothHandles {
    pub(crate) adapter: Mutable<Option<Adapter>>,
    pub(crate) devices: Mutable<Vec<Device>>,
    /// Set of device paths with an in-flight action (`Connect` / `Disconnect`
    /// / `Pair` / `RemoveDevice` / `Set Trusted`). The UI binds to this so it
    /// can show a spinner and disable the row while the D-Bus call is pending.
    pub(crate) device_actions: Mutable<HashSet<String>>,
    /// Currently-active pairing prompt from the `BlueZ` `Agent1` implementation,
    /// or `None` when no prompt is pending. The UI renders a banner when
    /// `Some` and calls `respond_to_prompt(accept)` to resolve it.
    pub(crate) pair_prompt: Mutable<Option<PairPrompt>>,
    /// Sender half of the oneshot the agent method is awaiting. Held under
    /// an async mutex so `respond_to_prompt` / `submit_pin` / `submit_passkey`
    /// / `Cancel` can race-lessly take it. `None` when no prompt is in-flight.
    pub(crate) pending_response: Arc<AsyncMutex<Option<oneshot::Sender<AgentReply>>>>,
}

/// Internal reply variants from the UI back to the agent's awaiting
/// method handler. Each variant maps to a specific Agent1 method's
/// expected return shape.
#[derive(Debug)]
pub(crate) enum AgentReply {
    /// User clicked Confirm on a yes/no prompt (`RequestConfirmation`,
    /// `RequestAuthorization`).
    Confirm,
    /// User explicitly rejected — agent throws `org.bluez.Error.Rejected`.
    Reject,
    /// User submitted a PIN code for `RequestPinCode`.
    Pin(String),
    /// User submitted a numeric passkey for `RequestPasskey`.
    Passkey(u32),
}

impl Default for BluetoothHandles {
    fn default() -> Self {
        Self {
            adapter: Mutable::new(None),
            devices: Mutable::new(Vec::new()),
            device_actions: Mutable::new(HashSet::new()),
            pair_prompt: Mutable::new(None),
            pending_response: Arc::new(AsyncMutex::new(None)),
        }
    }
}

// ── Service marker ────────────────────────────────────────────────────────────

/// The Bluetooth service marker type — pass to `App::with`.
pub struct BluetoothService;

impl Service for BluetoothService {
    type Handles = BluetoothHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = BluetoothHandles::default();
        let adapter_mutable = handles.adapter.clone();
        let devices_mutable = handles.devices.clone();

        rt.spawn(async move {
            loop {
                match listen(&adapter_mutable, &devices_mutable).await {
                    Ok(()) => {
                        tracing::warn!("bluetooth watcher closed, reconnecting in 2s");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "bluetooth watcher error, reconnecting in 2s");
                    }
                }
                // Clear state when the adapter disappears: the device list,
                // any in-flight action markers, the active pair prompt, and
                // the pending agent reply (so the agent method handler that's
                // awaiting it returns Reject instead of hanging forever).
                adapter_mutable.set(None);
                devices_mutable.set(Vec::new());
                let pending_response = registry::with(|r| {
                    r.get::<BluetoothHandles>().map(|h| {
                        h.device_actions.lock_mut().clear();
                        if h.pair_prompt.lock_ref().is_some() {
                            h.pair_prompt.set(None);
                        }
                        h.pending_response.clone()
                    })
                });
                if let Some(pending) = pending_response
                    && let Some(tx) = pending.lock().await.take()
                {
                    let _ = tx.send(AgentReply::Reject);
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });

        // Pairing-agent loop, independent of the watcher: stays registered
        // for the process lifetime, retrying on errors (e.g. bluetoothd
        // restart). The agent connection is managed by bus::own_name which
        // handles reconnects automatically.
        rt.spawn(async move {
            run_agent().await;
        });

        handles
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the Bluetooth service to register with the hytte runtime.
#[must_use]
pub fn service() -> BluetoothService {
    BluetoothService
}

/// Signal that emits the current adapter state, or `None` when no adapter
/// is present.
pub fn adapter() -> impl Signal<Item = Option<Adapter>> {
    registry::with(|r| {
        r.get::<BluetoothHandles>()
            .expect("bluetooth::service() not registered")
            .adapter
            .signal_cloned()
    })
}

/// Signal that emits the current list of tracked devices.
pub fn devices() -> impl Signal<Item = Vec<Device>> {
    registry::with(|r| {
        r.get::<BluetoothHandles>()
            .expect("bluetooth::service() not registered")
            .devices
            .signal_cloned()
    })
}

/// Signal emitting the set of device paths that currently have an in-flight
/// action (connect, disconnect, pair, remove, set-trusted). The UI uses this
/// to render a spinner / disable controls until the D-Bus call returns.
pub fn device_actions() -> impl Signal<Item = HashSet<String>> {
    registry::with(|r| {
        r.get::<BluetoothHandles>()
            .expect("bluetooth::service() not registered")
            .device_actions
            .signal_cloned()
    })
}

/// Signal emitting the active pairing prompt from `BlueZ`'s `Agent1` callbacks,
/// or `None` when no prompt is pending. The UI shows a confirmation banner
/// while this is `Some` and calls `respond_to_prompt(...)` on user action.
pub fn pair_prompts() -> impl Signal<Item = Option<PairPrompt>> {
    registry::with(|r| {
        r.get::<BluetoothHandles>()
            .expect("bluetooth::service() not registered")
            .pair_prompt
            .signal_cloned()
    })
}

/// Resolve the active yes/no pairing prompt. `accept = true` returns `Ok`
/// to `BlueZ`, completing the pair; `false` returns `org.bluez.Error.Rejected`
/// and aborts. No-op when no prompt is in flight or the prompt is a
/// text-entry kind (use `submit_pin` / `submit_passkey` for those).
pub fn respond_to_prompt(accept: bool) {
    send_reply(if accept {
        AgentReply::Confirm
    } else {
        AgentReply::Reject
    });
}

/// Resolve a `RequestPinCode` prompt by submitting the user's PIN.
/// Empty string is treated as a reject so the user can dismiss the
/// banner without triggering a malformed pair.
pub fn submit_pin(pin: String) {
    if pin.is_empty() {
        send_reply(AgentReply::Reject);
    } else {
        send_reply(AgentReply::Pin(pin));
    }
}

/// Resolve a `RequestPasskey` prompt by submitting the user's numeric
/// passkey. Out-of-range values reject so we never hand `BlueZ` something
/// it'll trip on.
pub fn submit_passkey(passkey: u32) {
    if passkey > 999_999 {
        send_reply(AgentReply::Reject);
    } else {
        send_reply(AgentReply::Passkey(passkey));
    }
}

fn send_reply(reply: AgentReply) {
    runtime::handle().spawn(async move {
        let pending = registry::with(|r| {
            r.get::<BluetoothHandles>()
                .map(|h| h.pending_response.clone())
        });
        let Some(pending) = pending else { return };
        let mut guard = pending.lock().await;
        if let Some(tx) = guard.take() {
            let _ = tx.send(reply);
        }
    });
}

fn mark_busy(path: &str) {
    registry::with(|r| {
        let handles = r
            .get::<BluetoothHandles>()
            .expect("bluetooth::service() not registered");
        // Peek with a read lock first — `lock_mut()` always fires the
        // signal on drop, even if the contents didn't change, so we only
        // take a write lock when the value will actually flip.
        if handles.device_actions.lock_ref().contains(path) {
            return;
        }
        handles
            .device_actions
            .lock_mut()
            .insert(path.to_string());
    });
}

fn mark_idle(path: &str) {
    registry::with(|r| {
        let handles = r
            .get::<BluetoothHandles>()
            .expect("bluetooth::service() not registered");
        if !handles.device_actions.lock_ref().contains(path) {
            return;
        }
        handles.device_actions.lock_mut().remove(path);
    });
}

/// Fire-and-forget: set the `Powered` property on the adapter.
pub fn set_powered(on: bool) {
    runtime::handle().spawn(async move {
        let path = get_adapter_path().await;
        if path.is_empty() {
            tracing::warn!("set_powered: no adapter path known");
            return;
        }
        if let Err(e) = do_set_adapter_bool(&path, "Powered", on).await {
            tracing::warn!(error = %e, on, "bluetooth set_powered failed");
        }
    });
}

/// Fire-and-forget: set the `Discoverable` property on the adapter. While
/// discoverable, this device shows up in other phones'/laptops' scan
/// results so you can pair *to* it.
pub fn set_discoverable(on: bool) {
    runtime::handle().spawn(async move {
        let path = get_adapter_path().await;
        if path.is_empty() {
            tracing::warn!("set_discoverable: no adapter path known");
            return;
        }
        if let Err(e) = do_set_adapter_bool(&path, "Discoverable", on).await {
            tracing::warn!(error = %e, on, "bluetooth set_discoverable failed");
        }
    });
}

/// Fire-and-forget: call `StartDiscovery` on the adapter.
pub fn start_discovery() {
    runtime::handle().spawn(async move {
        let path = get_adapter_path().await;
        if path.is_empty() {
            return;
        }
        if let Err(e) = do_adapter_call(&path, "StartDiscovery").await {
            tracing::warn!(error = %e, "bluetooth start_discovery failed");
        }
    });
}

/// Fire-and-forget: call `StopDiscovery` on the adapter.
pub fn stop_discovery() {
    runtime::handle().spawn(async move {
        let path = get_adapter_path().await;
        if path.is_empty() {
            return;
        }
        if let Err(e) = do_adapter_call(&path, "StopDiscovery").await {
            tracing::warn!(error = %e, "bluetooth stop_discovery failed");
        }
    });
}

/// Fire-and-forget: call `Connect` on the given device path.
pub fn connect_device(device_path: &str) {
    let path = device_path.to_string();
    mark_busy(&path);
    runtime::handle().spawn(async move {
        if let Err(e) = do_device_call(&path, "Connect").await {
            tracing::warn!(error = %e, path, "bluetooth connect_device failed");
        }
        mark_idle(&path);
    });
}

/// Fire-and-forget: call `Disconnect` on the given device path.
pub fn disconnect_device(device_path: &str) {
    let path = device_path.to_string();
    mark_busy(&path);
    runtime::handle().spawn(async move {
        if let Err(e) = do_device_call(&path, "Disconnect").await {
            tracing::warn!(error = %e, path, "bluetooth disconnect_device failed");
        }
        mark_idle(&path);
    });
}

/// Fire-and-forget: call `Pair` on the given device. On success, also marks
/// the device trusted (so it auto-reconnects on next session) and starts a
/// connection. The pair-then-trust-then-connect chain is what most users
/// expect from a "tap to pair my headphones" interaction. For devices that
/// require a PIN/passkey `BlueZ` delegates to a registered `Agent1`; without
/// one such pairings will silently fail.
pub fn pair_device(device_path: &str) {
    let path = device_path.to_string();
    mark_busy(&path);
    runtime::handle().spawn(async move {
        match do_device_call(&path, "Pair").await {
            Ok(()) => {
                if let Err(e) = do_set_device_bool(&path, "Trusted", true).await {
                    tracing::warn!(error = %e, path, "auto-trust after pair failed");
                }
                if let Err(e) = do_device_call(&path, "Connect").await {
                    tracing::warn!(error = %e, path, "auto-connect after pair failed");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, path, "bluetooth pair_device failed");
            }
        }
        mark_idle(&path);
    });
}

/// Fire-and-forget: set the `Trusted` property on a device.
pub fn set_trusted(device_path: &str, on: bool) {
    let path = device_path.to_string();
    mark_busy(&path);
    runtime::handle().spawn(async move {
        if let Err(e) = do_set_device_bool(&path, "Trusted", on).await {
            tracing::warn!(error = %e, on, path, "bluetooth set_trusted failed");
        }
        mark_idle(&path);
    });
}

/// Fire-and-forget: ask the adapter to forget the device entirely
/// (unpair). Equivalent to `bluetoothctl remove <addr>`.
pub fn remove_device(device_path: &str) {
    let path = device_path.to_string();
    mark_busy(&path);
    runtime::handle().spawn(async move {
        let adapter = get_adapter_path().await;
        if adapter.is_empty() {
            tracing::warn!("remove_device: no adapter path known");
            mark_idle(&path);
            return;
        }
        if let Err(e) = do_remove_device(&adapter, &path).await {
            tracing::warn!(error = %e, path, "bluetooth remove_device failed");
        }
        mark_idle(&path);
    });
}

// ── Command helpers ───────────────────────────────────────────────────────────

async fn do_set_adapter_bool(adapter_path: &str, prop: &str, on: bool) -> Result<(), hytte_bus::BusError> {
    let value = zbus::zvariant::Value::from(on)
        .try_to_owned()
        .map_err(|e| hytte_bus::BusError::Permanent {
            reason: format!("failed to box bool: {e}"),
            dbus_name: None,
        })?;
    let prop = prop.to_string();
    hytte_bus::call("org.bluez")
        .bus(BusKind::System)
        .at_path(adapter_path.to_string())
        .iface("org.freedesktop.DBus.Properties")
        .method("Set")
        .args(("org.bluez.Adapter1".to_string(), prop, value))
        .send::<()>()
        .await
}

async fn do_set_device_bool(device_path: &str, prop: &str, on: bool) -> Result<(), hytte_bus::BusError> {
    let value = zbus::zvariant::Value::from(on)
        .try_to_owned()
        .map_err(|e| hytte_bus::BusError::Permanent {
            reason: format!("failed to box bool: {e}"),
            dbus_name: None,
        })?;
    let prop = prop.to_string();
    hytte_bus::call("org.bluez")
        .bus(BusKind::System)
        .at_path(device_path.to_string())
        .iface("org.freedesktop.DBus.Properties")
        .method("Set")
        .args(("org.bluez.Device1".to_string(), prop, value))
        .send::<()>()
        .await
}

async fn do_adapter_call(adapter_path: &str, method: &str) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call("org.bluez")
        .bus(BusKind::System)
        .at_path(adapter_path.to_string())
        .iface("org.bluez.Adapter1")
        .method(method)
        .args(())
        .send::<()>()
        .await
}

async fn do_device_call(device_path: &str, method: &str) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call("org.bluez")
        .bus(BusKind::System)
        .at_path(device_path.to_string())
        .iface("org.bluez.Device1")
        .method(method)
        .args(())
        .send::<()>()
        .await
}

async fn do_remove_device(adapter_path: &str, device_path: &str) -> Result<(), hytte_bus::BusError> {
    let dev_op = zbus::zvariant::ObjectPath::try_from(device_path)
        .map_err(|e| hytte_bus::BusError::Permanent {
            reason: format!("invalid device object path: {e}"),
            dbus_name: None,
        })?
        .to_owned();
    hytte_bus::call("org.bluez")
        .bus(BusKind::System)
        .at_path(adapter_path.to_string())
        .iface("org.bluez.Adapter1")
        .method("RemoveDevice")
        .args((dev_op,))
        .send::<()>()
        .await
}

// ── Property parsing helpers ──────────────────────────────────────────────────

fn property<T>(props: &HashMap<String, OwnedValue>, key: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| T::try_from(v).ok())
}

fn prop_str(props: &HashMap<String, OwnedValue>, key: &str) -> String {
    // Try direct String conversion first; fall back to zvariant::Str.
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

fn prop_bool(props: &HashMap<String, OwnedValue>, key: &str) -> bool {
    property::<bool>(props, key).unwrap_or(false)
}

fn parse_adapter_props(path: &str, props: &HashMap<String, OwnedValue>) -> Adapter {
    Adapter {
        path: path.to_string(),
        address: prop_str(props, "Address"),
        name: prop_str(props, "Name"),
        powered: prop_bool(props, "Powered"),
        discoverable: prop_bool(props, "Discoverable"),
        discovering: prop_bool(props, "Discovering"),
    }
}

fn parse_device_props(path: &str, props: &HashMap<String, OwnedValue>) -> Device {
    Device {
        path: path.to_string(),
        address: prop_str(props, "Address"),
        alias: prop_str(props, "Alias"),
        icon: prop_str(props, "Icon"),
        paired: prop_bool(props, "Paired"),
        connected: prop_bool(props, "Connected"),
        trusted: prop_bool(props, "Trusted"),
        battery: None,
    }
}

// ── Internal watcher state ────────────────────────────────────────────────────

#[derive(Clone)]
struct State {
    adapter: Mutable<Option<Adapter>>,
    devices_map: Arc<AsyncMutex<HashMap<String, Device>>>,
    devices: Mutable<Vec<Device>>,
}

impl State {
    fn new(adapter: Mutable<Option<Adapter>>, devices: Mutable<Vec<Device>>) -> Self {
        Self {
            adapter,
            devices_map: Arc::new(AsyncMutex::new(HashMap::new())),
            devices,
        }
    }

    /// Snapshot the device map to a sorted Vec and publish it.
    async fn publish_devices(&self) {
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
    fn apply_adapter_props(&self, changed: &HashMap<String, OwnedValue>) {
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
    async fn apply_device_props(&self, path: &str, changed: &HashMap<String, OwnedValue>) {
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

type ManagedObjects = HashMap<
    zbus::zvariant::OwnedObjectPath,
    HashMap<String, HashMap<String, OwnedValue>>,
>;

async fn get_managed_objects() -> Result<ManagedObjects, hytte_bus::BusError> {
    hytte_bus::call("org.bluez")
        .bus(BusKind::System)
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .method("GetManagedObjects")
        .args(())
        .send::<ManagedObjects>()
        .await
}

async fn listen(
    adapter_mutable: &Mutable<Option<Adapter>>,
    devices_mutable: &Mutable<Vec<Device>>,
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
    set_adapter_path(&adapter_path).await;

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

    event_loop(&state, &adapter_path).await
}

/// A `PropertiesChanged` event forwarded from a per-device or adapter subscription.
struct PropChangedEvent {
    /// The object path the signal came from.
    path: String,
    /// The body of the `PropertiesChanged` signal: (iface, changed, invalidated).
    body: zbus::Message,
}

async fn event_loop(state: &State, adapter_path: &str) -> Result<(), anyhow::Error> {
    // Subscribe to ObjectManager signals on the root path.
    let ifaces_added_sub = hytte_bus::signals("org.bluez")
        .bus(BusKind::System)
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .signal("InterfacesAdded")
        .start();

    let ifaces_removed_sub = hytte_bus::signals("org.bluez")
        .bus(BusKind::System)
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .signal("InterfacesRemoved")
        .start();

    // Subscribe to PropertiesChanged on the adapter path.
    let adapter_props_sub = hytte_bus::signals("org.bluez")
        .bus(BusKind::System)
        .at_path(adapter_path.to_string())
        .iface("org.freedesktop.DBus.Properties")
        .signal("PropertiesChanged")
        .start();

    // Channel for device-level PropertiesChanged events forwarded from
    // per-device subscriptions (added/removed as devices appear/disappear).
    let (props_tx, mut props_rx) =
        tokio::sync::mpsc::unbounded_channel::<PropChangedEvent>();

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

    let mut ifaces_added_events = ifaces_added_sub.events();
    let mut ifaces_removed_events = ifaces_removed_sub.events();
    let mut adapter_props_events = adapter_props_sub.events();

    loop {
        tokio::select! {
            evt = ifaces_added_events.next() => {
                let Some(evt) = evt else { break; };
                let added = handle_ifaces_added(
                    state,
                    adapter_path,
                    evt.body,
                    props_tx.clone(),
                    &mut device_subs,
                ).await;
                let _ = added; // result used inside handler
            }

            evt = ifaces_removed_events.next() => {
                let Some(evt) = evt else { break; };
                if handle_ifaces_removed(state, adapter_path, evt.body, &mut device_subs).await {
                    return Err(anyhow::anyhow!("adapter removed"));
                }
            }

            evt = adapter_props_events.next() => {
                let Some(evt) = evt else { break; };
                handle_adapter_props_changed(state, adapter_path, &evt.body);
            }

            Some(dev_evt) = props_rx.recv() => {
                handle_device_props_changed(state, dev_evt).await;
            }
        }
    }

    Ok(())
}

/// Subscribe to `PropertiesChanged` on a single device path, forwarding
/// events to `tx`. Returns the `SignalSubscription` handle; dropping it
/// cancels the subscription.
fn subscribe_device_props(
    device_path: &str,
    tx: tokio::sync::mpsc::UnboundedSender<PropChangedEvent>,
) -> SignalSubscription {
    let path_str = device_path.to_string();
    let sub = hytte_bus::signals("org.bluez")
        .bus(BusKind::System)
        .at_path(path_str.clone())
        .iface("org.freedesktop.DBus.Properties")
        .signal("PropertiesChanged")
        .start();

    // Spawn a forwarder task. It exits when the subscription is dropped
    // (event stream ends) or the channel is closed.
    let sub_clone = sub.clone();
    runtime::handle().spawn(async move {
        let mut stream = sub_clone.events();
        while let Some(evt) = stream.next().await {
            let _ = tx.send(PropChangedEvent {
                path: path_str.clone(),
                body: evt.body,
            });
        }
    });

    sub
}

async fn handle_ifaces_added(
    state: &State,
    adapter_path: &str,
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
            set_adapter_path(p).await;
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
    let Ok((path, removed_ifaces)) = msg.body().deserialize::<(
        zbus::zvariant::OwnedObjectPath,
        Vec<String>,
    )>() else {
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

fn handle_adapter_props_changed(
    state: &State,
    adapter_path: &str,
    msg: &zbus::Message,
) {
    let Ok((iface_name, changed, _)) = msg.body().deserialize::<(
        String,
        HashMap<String, OwnedValue>,
        Vec<String>,
    )>() else {
        return;
    };

    if iface_name == "org.bluez.Adapter1" {
        let _ = adapter_path; // already filtered by subscription path
        state.apply_adapter_props(&changed);
    }
}

async fn handle_device_props_changed(state: &State, evt: PropChangedEvent) {
    let Ok((iface_name, changed, _)) = evt.body.body().deserialize::<(
        String,
        HashMap<String, OwnedValue>,
        Vec<String>,
    )>() else {
        return;
    };

    if iface_name == "org.bluez.Device1" {
        state.apply_device_props(&evt.path, &changed).await;
        state.publish_devices().await;
    } else if iface_name == "org.bluez.Battery1" {
        let mut map = state.devices_map.lock().await;
        if let Some(dev) = map.get_mut(&evt.path)
            && changed.contains_key("Percentage")
        {
            dev.battery = property::<u8>(&changed, "Percentage");
        }
        drop(map);
        state.publish_devices().await;
    }
}

// ── Pairing agent ─────────────────────────────────────────────────────────────
//
// Implements `org.bluez.Agent1` so BlueZ can ask us to confirm pairings.
// Without a registered agent BlueZ rejects most pair attempts that need any
// user interaction (e.g. SSP numeric comparison). MVP scope:
//   * RequestConfirmation: user confirms the displayed code matches.
//   * RequestAuthorization: bare yes/no.
//   * AuthorizeService: auto-accept (typical for trusted devices reconnecting).
//   * PIN / Passkey entry methods: return Rejected (no text-input UI yet).
//   * Cancel: aborts the pending prompt.
//
// The agent is registered under bus::own_name on the SYSTEM bus. BlueZ
// records the system-bus unique name when we call RegisterAgent, then issues
// Agent1 callbacks on that same connection. This mirrors the polkit pattern:
// agent + anchor name both on the system bus.

const AGENT_PATH: &str = "/com/trollshell/BluetoothAgent";
const AGENT_ANCHOR_NAME: &str = "cc.hannig.trollshell.bluez-agent";

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
#[allow(dead_code)]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
    Canceled(String),
}

#[derive(Clone)]
struct PairAgent;

#[zbus::interface(name = "org.bluez.Agent1")]
impl PairAgent {
    #[allow(clippy::unused_async)]
    async fn release(&self) {
        tracing::debug!("agent released");
    }

    async fn request_pin_code(&self, device: OwnedObjectPath) -> Result<String, AgentError> {
        let path = device.as_str().to_string();
        let alias = lookup_alias(&path);
        let reply = await_reply(PairPrompt {
            device_path: path,
            alias,
            passkey: None,
            kind: PromptKind::EnterPinCode,
        })
        .await;
        match reply {
            AgentReply::Pin(s) => Ok(s),
            _ => Err(AgentError::Rejected("user did not provide PIN".into())),
        }
    }

    #[allow(clippy::unused_async, clippy::needless_pass_by_value)]
    async fn display_pin_code(&self, device: OwnedObjectPath, pincode: String) {
        // Display-only acknowledgement — there is no return value to gate.
        // The user enters the PIN on the remote device. Nothing to do.
        let _ = (device, pincode);
    }

    async fn request_passkey(&self, device: OwnedObjectPath) -> Result<u32, AgentError> {
        let path = device.as_str().to_string();
        let alias = lookup_alias(&path);
        let reply = await_reply(PairPrompt {
            device_path: path,
            alias,
            passkey: None,
            kind: PromptKind::EnterPasskey,
        })
        .await;
        match reply {
            AgentReply::Passkey(p) => Ok(p),
            _ => Err(AgentError::Rejected("user did not provide passkey".into())),
        }
    }

    #[allow(clippy::unused_async, clippy::needless_pass_by_value)]
    async fn display_passkey(&self, device: OwnedObjectPath, passkey: u32, entered: u16) {
        // Same as DisplayPinCode — no input from us.
        let _ = (device, passkey, entered);
    }

    async fn request_confirmation(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
    ) -> Result<(), AgentError> {
        let path = device.as_str().to_string();
        let alias = lookup_alias(&path);
        let reply = await_reply(PairPrompt {
            device_path: path,
            alias,
            passkey: Some(passkey),
            kind: PromptKind::ConfirmPasskey,
        })
        .await;
        match reply {
            AgentReply::Confirm => Ok(()),
            _ => Err(AgentError::Rejected("user rejected pairing".into())),
        }
    }

    async fn request_authorization(&self, device: OwnedObjectPath) -> Result<(), AgentError> {
        let path = device.as_str().to_string();
        let alias = lookup_alias(&path);
        let reply = await_reply(PairPrompt {
            device_path: path,
            alias,
            passkey: None,
            kind: PromptKind::Authorize,
        })
        .await;
        match reply {
            AgentReply::Confirm => Ok(()),
            _ => Err(AgentError::Rejected("user rejected pairing".into())),
        }
    }

    #[allow(clippy::unused_async, clippy::needless_pass_by_value)]
    async fn authorize_service(&self, device: OwnedObjectPath, uuid: String) {
        // Auto-accept service authorization. BlueZ asks per-service for
        // unknown protocols; for trusted/already-paired devices this is
        // generally fine and matches blueman-applet's default policy.
        let _ = (device, uuid);
    }

    async fn cancel(&self) {
        tracing::debug!("agent cancel — aborting pending prompt");
        if let Some(tx) = take_pending().await {
            let _ = tx.send(AgentReply::Reject);
        }
        clear_prompt();
    }
}

/// Resolve a device path to a user-facing label. Prefers the `BlueZ` Alias,
/// falls through to the MAC address, and ultimately "Unknown device" so a
/// raw D-Bus object path never bleeds into UI copy.
fn lookup_alias(path: &str) -> String {
    registry::with(|r| {
        r.get::<BluetoothHandles>()
            .and_then(|h| {
                let devs = h.devices.lock_ref();
                devs.iter().find(|d| d.path == path).map(|d| {
                    if !d.alias.is_empty() {
                        d.alias.clone()
                    } else if !d.address.is_empty() {
                        d.address.clone()
                    } else {
                        "Unknown device".to_string()
                    }
                })
            })
            .unwrap_or_else(|| "Unknown device".to_string())
    })
}

fn pending_response_arc() -> Option<Arc<AsyncMutex<Option<oneshot::Sender<AgentReply>>>>> {
    registry::with(|r| {
        r.get::<BluetoothHandles>()
            .map(|h| h.pending_response.clone())
    })
}

async fn take_pending() -> Option<oneshot::Sender<AgentReply>> {
    let arc = pending_response_arc()?;
    arc.lock().await.take()
}

fn set_prompt(p: Option<PairPrompt>) {
    registry::with(|r| {
        if let Some(h) = r.get::<BluetoothHandles>() {
            h.pair_prompt.set(p);
        }
    });
}

fn clear_prompt() {
    set_prompt(None);
}

/// Suspend the calling Agent1 method until the user responds via the UI.
/// Returns `AgentReply::Reject` if no prompt slot is available, the
/// channel is dropped, or another pair is already in flight — callers
/// pattern-match on the returned variant to shape their D-Bus return.
async fn await_reply(prompt: PairPrompt) -> AgentReply {
    let Some(pending) = pending_response_arc() else {
        return AgentReply::Reject;
    };

    let (tx, rx) = oneshot::channel::<AgentReply>();
    {
        let mut guard = pending.lock().await;
        if guard.is_some() {
            // Another pairing already pending — refuse cleanly so BlueZ
            // doesn't pile up coincident prompts.
            return AgentReply::Reject;
        }
        *guard = Some(tx);
    }

    set_prompt(Some(prompt));
    let reply = rx.await.unwrap_or(AgentReply::Reject);
    clear_prompt();
    reply
}

/// Start the pairing-agent registration loop. Uses `bus::own_name` on the
/// SYSTEM bus (`BlueZ` is on system bus; it records the unique name of the
/// connection that called `RegisterAgent`). Mounting `PairAgent` at
/// `AGENT_PATH` via `.at_path()` ensures the object is visible before
/// `RequestName` succeeds, so `BlueZ` never races a missing object.
///
/// After the name is owned, we call `RegisterAgent` and
/// `RequestDefaultAgent` once via `bus::call`. On bluetoothd restart the
/// `NameOwnerChanged` stream for `org.bluez` wakes us to re-register.
async fn run_agent() {
    // Own the anchor name on the system bus. The PairAgent interface is
    // mounted at AGENT_PATH on each connection established by own_name.
    // bus::own_name handles reconnects: if bluetoothd restarts and the name
    // is temporarily lost, own_name re-acquires and re-mounts the interface.
    let _ownership = hytte_bus::own_name(AGENT_ANCHOR_NAME)
        .bus(BusKind::System)
        .at_path(AGENT_PATH, PairAgent)
        .start();

    // Watch for org.bluez owner changes. When bluetoothd restarts (loses
    // its name), our registration is gone and we must re-register.
    // We re-register once the owner comes back.
    let bluez_gone_sub = hytte_bus::signals("org.freedesktop.DBus")
        .bus(BusKind::System)
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .signal("NameOwnerChanged")
        .start();

    // Initial registration attempt.
    try_register_agent().await;

    // Re-register whenever BlueZ (org.bluez) gains a new owner, which
    // indicates bluetoothd has restarted.
    let mut noc_events = bluez_gone_sub.events();
    while let Some(evt) = noc_events.next().await {
        let Ok((name, _old_owner, new_owner)) =
            evt.body.body().deserialize::<(String, String, String)>()
        else {
            continue;
        };
        if name != "org.bluez" {
            continue;
        }
        if new_owner.is_empty() {
            // bluetoothd died — our registration is gone. Wait for it to
            // come back (the next NameOwnerChanged with a non-empty
            // new_owner will trigger re-registration).
            tracing::warn!("org.bluez lost — will re-register agent on restart");
            continue;
        }
        // bluetoothd restarted: re-register.
        tracing::info!("org.bluez new owner — re-registering pairing agent");
        try_register_agent().await;
    }
}

/// Attempt to register the pairing agent with `BlueZ` once. Logs any failure
/// and returns (caller decides whether to retry or wait for `NameOwnerChanged`).
async fn try_register_agent() {
    let agent_op = match zbus::zvariant::ObjectPath::try_from(AGENT_PATH) {
        Ok(p) => p.to_owned(),
        Err(e) => {
            tracing::error!(error = %e, "bluetooth agent: bad agent path");
            return;
        }
    };

    // RegisterAgent — capability "DisplayYesNo": we can show a code and
    // accept yes/no, which is what RequestConfirmation needs.
    if let Err(e) = hytte_bus::call("org.bluez")
        .bus(BusKind::System)
        .at_path("/org/bluez")
        .iface("org.bluez.AgentManager1")
        .method("RegisterAgent")
        .args((agent_op.clone(), "DisplayYesNo".to_string()))
        .send::<()>()
        .await
    {
        tracing::warn!(error = %e, "bluetooth agent: RegisterAgent failed");
        return;
    }

    // RequestDefaultAgent — make us the system-wide default. Without this
    // BlueZ may use whichever Agent it sees first, including stale ones
    // from a previous trollshell run if any.
    if let Err(e) = hytte_bus::call("org.bluez")
        .bus(BusKind::System)
        .at_path("/org/bluez")
        .iface("org.bluez.AgentManager1")
        .method("RequestDefaultAgent")
        .args((agent_op,))
        .send::<()>()
        .await
    {
        tracing::warn!(error = %e, "bluetooth agent: RequestDefaultAgent failed");
        return;
    }

    tracing::info!("bluetooth pairing agent registered");
}
