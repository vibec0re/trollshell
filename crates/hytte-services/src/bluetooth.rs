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
//! bluetooth::start_discovery();
//! bluetooth::stop_discovery();
//! bluetooth::connect_device(path);
//! bluetooth::disconnect_device(path);
//! ```

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_reactive::{registry, runtime, Service};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use zbus::zvariant::OwnedValue;
use zbus::Connection;

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
}

impl Default for BluetoothHandles {
    fn default() -> Self {
        Self {
            adapter: Mutable::new(None),
            devices: Mutable::new(Vec::new()),
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
                // Clear state when adapter disappears.
                adapter_mutable.set(None);
                devices_mutable.set(Vec::new());
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
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

/// Fire-and-forget: set the `Powered` property on the adapter.
pub fn set_powered(on: bool) {
    runtime::handle().spawn(async move {
        let path = get_adapter_path().await;
        if path.is_empty() {
            tracing::warn!("set_powered: no adapter path known");
            return;
        }
        if let Err(e) = do_set_powered(&path, on).await {
            tracing::warn!(error = %e, on, "bluetooth set_powered failed");
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
    runtime::handle().spawn(async move {
        if let Err(e) = do_device_call(&path, "Connect").await {
            tracing::warn!(error = %e, path, "bluetooth connect_device failed");
        }
    });
}

/// Fire-and-forget: call `Disconnect` on the given device path.
pub fn disconnect_device(device_path: &str) {
    let path = device_path.to_string();
    runtime::handle().spawn(async move {
        if let Err(e) = do_device_call(&path, "Disconnect").await {
            tracing::warn!(error = %e, path, "bluetooth disconnect_device failed");
        }
    });
}

// ── Command helpers ───────────────────────────────────────────────────────────

async fn do_set_powered(adapter_path: &str, on: bool) -> Result<()> {
    let conn = Connection::system()
        .await
        .context("open system bus for set_powered")?;
    conn.call_method(
        Some("org.bluez"),
        adapter_path,
        Some("org.freedesktop.DBus.Properties"),
        "Set",
        &(
            "org.bluez.Adapter1",
            "Powered",
            zbus::zvariant::Value::from(on),
        ),
    )
    .await
    .context("call Properties.Set Powered")?;
    Ok(())
}

async fn do_adapter_call(adapter_path: &str, method: &str) -> Result<()> {
    let conn = Connection::system()
        .await
        .context("open system bus for adapter call")?;
    conn.call_method(
        Some("org.bluez"),
        adapter_path,
        Some("org.bluez.Adapter1"),
        method,
        &(),
    )
    .await
    .with_context(|| format!("call Adapter1.{method}"))?;
    Ok(())
}

async fn do_device_call(device_path: &str, method: &str) -> Result<()> {
    let conn = Connection::system()
        .await
        .context("open system bus for device call")?;
    conn.call_method(
        Some("org.bluez"),
        device_path,
        Some("org.bluez.Device1"),
        method,
        &(),
    )
    .await
    .with_context(|| format!("call Device1.{method}"))?;
    Ok(())
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

async fn get_managed_objects(conn: &Connection) -> Result<ManagedObjects> {
    let reply = conn
        .call_method(
            Some("org.bluez"),
            "/",
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        )
        .await
        .context("GetManagedObjects")?;
    let body = reply.body();
    body.deserialize().context("deserialise GetManagedObjects reply")
}

async fn listen(
    adapter_mutable: &Mutable<Option<Adapter>>,
    devices_mutable: &Mutable<Vec<Device>>,
) -> Result<()> {
    let conn = Connection::system()
        .await
        .context("connect system bus for bluetooth")?;

    let managed = get_managed_objects(&conn).await?;

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
                let dev = parse_device_props(p, dev_props);
                map.insert(p.to_string(), dev);
            }
        }
    }
    state.publish_devices().await;

    // ── Signal subscriptions ──────────────────────────────────────────────────

    event_loop(&conn, &state, &adapter_path).await
}

async fn event_loop(conn: &Connection, state: &State, adapter_path: &str) -> Result<()> {
    // Build a proxy on the ObjectManager root object.
    let obj_mgr_proxy = zbus::Proxy::new(
        conn,
        "org.bluez",
        "/",
        "org.freedesktop.DBus.ObjectManager",
    )
    .await
    .context("create ObjectManager proxy")?;

    let mut ifaces_added = obj_mgr_proxy
        .receive_signal("InterfacesAdded")
        .await
        .context("subscribe InterfacesAdded")?;

    let mut ifaces_removed = obj_mgr_proxy
        .receive_signal("InterfacesRemoved")
        .await
        .context("subscribe InterfacesRemoved")?;

    // `PropertiesChanged` is sent per-object — use a match rule covering all
    // paths under /org/bluez from sender org.bluez.
    let props_rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.bluez")
        .map_err(|e| anyhow::anyhow!("match rule sender: {e}"))?
        .interface("org.freedesktop.DBus.Properties")
        .map_err(|e| anyhow::anyhow!("match rule interface: {e}"))?
        .member("PropertiesChanged")
        .map_err(|e| anyhow::anyhow!("match rule member: {e}"))?
        .path_namespace("/org/bluez")
        .map_err(|e| anyhow::anyhow!("match rule path: {e}"))?
        .build();

    let mut props_changed = zbus::MessageStream::for_match_rule(props_rule, conn, None)
        .await
        .context("subscribe PropertiesChanged")?;

    loop {
        tokio::select! {
            msg = ifaces_added.next() => {
                let Some(msg) = msg else { break; };
                handle_ifaces_added(state, adapter_path, msg).await;
            }

            msg = ifaces_removed.next() => {
                let Some(msg) = msg else { break; };
                if handle_ifaces_removed(state, adapter_path, msg).await {
                    return Err(anyhow::anyhow!("adapter removed"));
                }
            }

            msg = props_changed.next() => {
                let Some(msg) = msg else { break; };
                let Ok(msg) = msg else { continue; };
                handle_props_changed(state, adapter_path, msg).await;
            }
        }
    }

    Ok(())
}

async fn handle_ifaces_added(
    state: &State,
    adapter_path: &str,
    msg: zbus::Message,
) {
    let Ok((path, ifaces)) = msg.body().deserialize::<(
        zbus::zvariant::OwnedObjectPath,
        HashMap<String, HashMap<String, OwnedValue>>,
    )>() else {
        return;
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
        let dev = parse_device_props(p, dev_props);
        tracing::debug!(path = p, alias = dev.alias, "device added");
        state.devices_map.lock().await.insert(p.to_string(), dev);
        state.publish_devices().await;
    }
}

/// Returns `true` when the adapter was removed (caller should reconnect).
async fn handle_ifaces_removed(
    state: &State,
    adapter_path: &str,
    msg: zbus::Message,
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
        return true;
    }

    if removed_ifaces.iter().any(|i| i == "org.bluez.Device1") {
        tracing::debug!(path = p, "device removed");
        state.devices_map.lock().await.remove(p);
        state.publish_devices().await;
    }

    false
}

async fn handle_props_changed(
    state: &State,
    adapter_path: &str,
    msg: zbus::Message,
) {
    let Ok((iface_name, changed, _)) = msg.body().deserialize::<(
        String,
        HashMap<String, OwnedValue>,
        Vec<String>,
    )>() else {
        return;
    };

    let obj_path = msg
        .header()
        .path()
        .map_or("", |p: &zbus::zvariant::ObjectPath<'_>| p.as_str())
        .to_string();

    if iface_name == "org.bluez.Adapter1" && obj_path == adapter_path {
        state.apply_adapter_props(&changed);
    } else if iface_name == "org.bluez.Device1" {
        state.apply_device_props(&obj_path, &changed).await;
        state.publish_devices().await;
    }
}
