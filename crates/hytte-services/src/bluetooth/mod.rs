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

mod agent;
mod devices;
mod parse;
mod types;

pub(crate) use types::AgentReply;
pub use types::{Adapter, Device, PairPrompt, PromptKind};

use agent::{AGENT_ANCHOR_NAME, AGENT_PATH, PairAgent, run_agent};
use devices::listen;
use futures_signals::signal::{Mutable, Signal};
use hytte_bus::BusKind;
use hytte_reactive::{Service, registry, runtime};
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::oneshot;

// ── Cross-thread shared handle ────────────────────────────────────────────────
//
// `hytte_reactive::registry` is a thread-local — initialised on the GTK main
// thread, empty on hytte-tokio worker threads. The following call paths run on
// hytte-tokio and must NOT use `registry::with`:
//   - The reconnect-cleanup loop in `Service::start`'s spawned task clears
//     `device_actions` / `pair_prompt` / `pending_response` on bluetoothd
//     restart; without a handle it silently skips the cleanup.
//   - `send_reply` (called by `respond_to_prompt`, `submit_pin`,
//     `submit_passkey`) spawns a task that signals `pending_response`; the
//     pairing dialog hangs forever on a no-op.
//   - `PairAgent` iface methods (`request_pin_code`, `request_passkey`,
//     `request_confirmation`, `request_authorization`, `cancel`) call
//     `lookup_alias`, `pending_response_arc`, `set_prompt`, `clear_prompt`
//     — all of which used `registry::with`.
//
// A static `OnceLock` populated by `Service::start` is the cross-thread-safe
// alternative — `Mutable<T>` and `Arc<AsyncMutex<…>>` are `Send + Sync`.
pub(super) struct BluetoothShared {
    pub(super) devices: Mutable<Vec<Device>>,
    pub(super) device_actions: Mutable<HashSet<String>>,
    pub(super) pair_prompt: Mutable<Option<PairPrompt>>,
    pub(super) pending_response: Arc<AsyncMutex<Option<oneshot::Sender<AgentReply>>>>,
}

pub(super) static SHARED: OnceLock<BluetoothShared> = OnceLock::new();

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

async fn set_adapter_path_local(path: &str) {
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
    /// `Some` and calls `respond_to_prompt(...)` on user action.
    pub(crate) pair_prompt: Mutable<Option<PairPrompt>>,
    /// Sender half of the oneshot the agent method is awaiting. Held under
    /// an async mutex so `respond_to_prompt` / `submit_pin` / `submit_passkey`
    /// / `Cancel` can race-lessly take it. `None` when no prompt is in-flight.
    pub(crate) pending_response: Arc<AsyncMutex<Option<oneshot::Sender<AgentReply>>>>,
    /// Keeps the `own_name` watcher task alive for the process lifetime so the
    /// system bus holds `AGENT_ANCHOR_NAME` and the `PairAgent` interface
    /// remains reachable at `AGENT_PATH`. Stored here for parity with other
    /// services (polkit, notifications, etc.) that keep their ownership handle
    /// in Handles.
    pub(crate) _ownership: hytte_bus::OwnNameSignal,
}

// ── Service marker ────────────────────────────────────────────────────────────

/// The Bluetooth service marker type — pass to `App::with`.
pub struct BluetoothService;

impl Service for BluetoothService {
    type Handles = BluetoothHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        // Build the own_name handle first so it can be stored in Handles for
        // parity with other services (polkit, screensaver, etc.) and passed
        // into run_agent to keep the interface alive for its lifetime.
        let ownership = hytte_bus::own_name(AGENT_ANCHOR_NAME)
            .bus(BusKind::System)
            .at_path(AGENT_PATH, PairAgent)
            .start();

        let handles = BluetoothHandles {
            adapter: Mutable::new(None),
            devices: Mutable::new(Vec::new()),
            device_actions: Mutable::new(HashSet::new()),
            pair_prompt: Mutable::new(None),
            pending_response: Arc::new(AsyncMutex::new(None)),
            _ownership: ownership.clone(),
        };
        let _ = SHARED.set(BluetoothShared {
            devices: handles.devices.clone(),
            device_actions: handles.device_actions.clone(),
            pair_prompt: handles.pair_prompt.clone(),
            pending_response: handles.pending_response.clone(),
        });

        let adapter_mutable = handles.adapter.clone();
        let devices_mutable = handles.devices.clone();
        let path_store = adapter_path_store().clone();

        rt.spawn(async move {
            loop {
                match listen(&adapter_mutable, &devices_mutable, &path_store).await {
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
                let pending_response = SHARED.get().map(|s| {
                    s.device_actions.lock_mut().clear();
                    if s.pair_prompt.lock_ref().is_some() {
                        s.pair_prompt.set(None);
                    }
                    s.pending_response.clone()
                });
                if let Some(pending) = pending_response
                    && let Some(tx) = pending.lock().await.take()
                {
                    let _ = tx.send(AgentReply::Reject);
                }
                // Also reset the adapter path store so the next listen()
                // call starts fresh.
                set_adapter_path_local("").await;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });

        // Pairing-agent loop, independent of the watcher: stays registered
        // for the process lifetime, retrying on errors (e.g. bluetoothd
        // restart). The agent connection is managed by bus::own_name which
        // handles reconnects automatically.
        rt.spawn(async move {
            run_agent(ownership).await;
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
    let Some(pending) = SHARED.get().map(|s| s.pending_response.clone()) else {
        return;
    };
    runtime::handle().spawn(async move {
        let mut guard = pending.lock().await;
        if let Some(tx) = guard.take() {
            let _ = tx.send(reply);
        }
    });
}

fn mark_busy(path: &str) {
    let Some(shared) = SHARED.get() else {
        return;
    };
    // Peek with a read lock first — `lock_mut()` always fires the
    // signal on drop, even if the contents didn't change, so we only
    // take a write lock when the value will actually flip.
    if shared.device_actions.lock_ref().contains(path) {
        return;
    }
    shared.device_actions.lock_mut().insert(path.to_string());
}

fn mark_idle(path: &str) {
    let Some(shared) = SHARED.get() else {
        return;
    };
    if !shared.device_actions.lock_ref().contains(path) {
        return;
    }
    shared.device_actions.lock_mut().remove(path);
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

async fn do_set_adapter_bool(
    adapter_path: &str,
    prop: &str,
    on: bool,
) -> Result<(), hytte_bus::BusError> {
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

async fn do_set_device_bool(
    device_path: &str,
    prop: &str,
    on: bool,
) -> Result<(), hytte_bus::BusError> {
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

async fn do_remove_device(
    adapter_path: &str,
    device_path: &str,
) -> Result<(), hytte_bus::BusError> {
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
