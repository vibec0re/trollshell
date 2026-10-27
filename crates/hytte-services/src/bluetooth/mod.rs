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
mod trust;
mod types;

pub(crate) use types::AgentReply;
pub use types::{Adapter, Device, PairPrompt, PromptKind};

use agent::{AGENT_ANCHOR_NAME, AGENT_PATH, PairAgent, ReplySlot, run_agent};
use devices::{ListenEnd, listen, wait_for_bluez};
use futures_signals::signal::{Mutable, Signal};
use hytte_bus::BusKind;
use hytte_reactive::{Service, registry, runtime, spawn_supervised};
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

use crate::retry;

/// How long bluetoothd gives our agent to answer one prompt before it gives up
/// and sends `Cancel`: `REQUEST_TIMEOUT` in `BlueZ`'s `src/agent.c` (60 s).
/// Copied from the C source for the test that holds [`PAIRING_CALL_TIMEOUT`]
/// above it.
#[cfg(test)]
const BLUEZ_AGENT_REQUEST_TIMEOUT: Duration = Duration::from_mins(1);

/// Deadline for `Device1.Pair` and `Device1.Connect`. It matches the deadline
/// `BlueZ` puts on its own client's calls.
///
/// Both calls block for as long as bluetoothd works on them. `Pair` also blocks
/// while the user reads a code or types a PIN on our agent's prompt, and
/// bluetoothd allows that prompt a full minute on its own. `hytte_bus::call`'s
/// default is 25 s, shorter than one prompt. With that default, a user still
/// checking the code got a "Couldn't pair: call timeout" toast while bluetoothd
/// went on pairing. The trust-and-connect follow-up in [`pair_device`] never
/// ran. The row came back enabled, so a second tap got
/// `org.bluez.Error.InProgress`.
///
/// Five minutes is `METHOD_CALL_TIMEOUT` (300 s) in `BlueZ`'s
/// `gdbus/client.c`. `bluetoothctl` puts that deadline on every method call
/// it makes, `pair` and `connect` included. bluetoothd bounds both calls
/// itself: each agent prompt by `REQUEST_TIMEOUT`, each connection attempt by
/// the controller's and the kernel's own timeouts. So this deadline is a
/// backstop for a bluetoothd that has stopped answering. A bluetoothd that
/// exits does not reach it: the bus answers the pending call with `NoReply`
/// as soon as the daemon disconnects.
const PAIRING_CALL_TIMEOUT: Duration = Duration::from_mins(5);

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
//     `request_confirmation`, `request_authorization`, `authorize_service`)
//     call `lookup_alias`, which reads the device list; they once used
//     `registry::with`. The prompt and reply slots they and `cancel` reach
//     are clones the agent is built with, not lookups.
//
// The cross-thread-safe alternative is `hytte_reactive::shared` (the
// process-global registry mirror): `Service::start` publishes this bag with
// `shared::insert`, and any thread reads it back via [`shared_state`] —
// `Mutable<T>` and `Arc<AsyncMutex<…>>` are `Send + Sync`.
pub(super) struct BluetoothShared {
    pub(super) devices: Mutable<Vec<Device>>,
    pub(super) device_actions: Mutable<HashSet<String>>,
    pub(super) pair_prompt: Mutable<Option<PairPrompt>>,
    pub(super) pending_response: Arc<ReplySlot>,
    /// Most recent connect/pair/disconnect failure (#1171), or `None` before
    /// the first one. `Mutable::set` always notifies subscribers, even on an
    /// unchanged value (see `mark_busy`'s doc below for the same invariant),
    /// so a *second* identical failure is a fresh `set` rather than a
    /// swallowed no-op — but a subscriber only observes what is there when it
    /// next polls, so two failures inside one main-loop turn coalesce into one
    /// toast (#1192 review, NIT-2). Harmless in practice: D-Bus action
    /// failures are seconds apart.
    ///
    /// This handle is never cleared, and that is deliberate — see
    /// [`ActionError::at`] for why staleness is handled by timestamp instead.
    pub(super) action_error: Mutable<Option<ActionError>>,
    /// The devices being paired, the same set the agent holds: `pair_device`
    /// puts a device in for the length of its `Pair` call.
    pub(super) pairings: trust::Pairings,
}

/// A user-facing message describing why a connect/pair/disconnect action
/// failed (#1171). The UI binds [`action_error`] and forwards each *fresh*
/// emission to `notifications::post_local`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionError {
    pub message: String,
    /// When the failure happened.
    ///
    /// Load-bearing, not decoration (#1192 review, MEDIUM-2). `bind` drives
    /// `signal_cloned()`, whose **first** poll yields the handle's current
    /// value — so a panel built *after* a failure immediately forwards that
    /// old error to a toast. Concretely: a connect fails on monitor A at
    /// 10:00 and toasts; at 11:00 a second monitor is plugged in, its
    /// Bluetooth page is built, and the same hour-old "Couldn't connect: …"
    /// toasts again with nothing having failed. The 10 s `post_local` rate
    /// limit does not span that gap (it does collapse the N-monitors-N-binds
    /// duplicate while the failure is live, which is why that variant was
    /// only cosmetic).
    ///
    /// So a binding records when it was created and forwards only errors
    /// newer than itself — [`ActionError::is_newer_than`]. Clearing the
    /// handle after posting would be the other option, but it races between
    /// per-monitor binds: whichever one polls first would take the error and
    /// blank it for the others, so which monitors toast would depend on
    /// scheduling. A timestamp needs no coordination at all.
    pub at: std::time::Instant,
}

impl ActionError {
    /// Is this failure newer than `since` — i.e. did it happen after the
    /// subscriber asking was created? The freshness gate described on
    /// [`ActionError::at`]; a subscriber built after the failure gets
    /// `false` and stays quiet.
    #[must_use]
    pub fn is_newer_than(&self, since: std::time::Instant) -> bool {
        self.at > since
    }
}

/// Cross-thread accessor for the shared handle bag published in
/// `Service::start`. `None` until the service has started. Callable from any
/// thread (unlike `registry::with`, which only works on the GTK main thread).
pub(super) fn shared_state() -> Option<Arc<BluetoothShared>> {
    hytte_reactive::shared::get::<BluetoothShared>()
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
    /// Sender half of the oneshot the agent method is awaiting, keyed by the
    /// [`PairPrompt::id`] of the prompt it raised. Held under an async mutex
    /// so `respond_to_prompt` / `submit_pin` / `submit_passkey` / `Cancel`
    /// can race-lessly take it; the three answers take it only for their own
    /// prompt's id. `None` when no prompt is in-flight.
    pub(crate) pending_response: Arc<ReplySlot>,
    /// Most recent connect/pair/disconnect failure, or `None` before the
    /// first one (#1171). The UI binds [`action_error`] and forwards each
    /// emission **newer than the binding itself** to a toast — see
    /// [`ActionError::at`].
    pub(crate) action_error: Mutable<Option<ActionError>>,
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

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        // The agent raises its prompts on these two handles, and the UI
        // answers through them, so they exist before the agent does.
        let pair_prompt = Mutable::new(None);
        let pending_response: Arc<ReplySlot> = Arc::new(AsyncMutex::new(None));
        // The devices being paired: `pair_device` and the agent's accepted
        // prompts put them in, and the agent's `AuthorizeService` reads it.
        let pairings = trust::Pairings::default();
        // It answers only bluetoothd, whose unique name it looks up per
        // call: the object is reachable by any peer on the system bus.
        let agent = PairAgent::new(
            pending_response.clone(),
            pair_prompt.clone(),
            Arc::new(trust::SystemBluez),
            pairings.clone(),
        );

        // Build the own_name handle first so it can be stored in Handles for
        // parity with other services (polkit, screensaver, etc.) and passed
        // into run_agent to keep the interface alive for its lifetime.
        let ownership = hytte_bus::own_name(BusKind::System, AGENT_ANCHOR_NAME)
            .at_path(AGENT_PATH, agent)
            .start();

        let handles = BluetoothHandles {
            adapter: Mutable::new(None),
            devices: Mutable::new(Vec::new()),
            device_actions: Mutable::new(HashSet::new()),
            pair_prompt,
            pending_response,
            action_error: Mutable::new(None),
            _ownership: ownership.clone(),
        };
        hytte_reactive::shared::insert(BluetoothShared {
            devices: handles.devices.clone(),
            device_actions: handles.device_actions.clone(),
            pair_prompt: handles.pair_prompt.clone(),
            pending_response: handles.pending_response.clone(),
            action_error: handles.action_error.clone(),
            pairings,
        });

        let adapter_mutable = handles.adapter.clone();
        let devices_mutable = handles.devices.clone();
        let path_store = adapter_path_store().clone();

        // The reconnect delay used to be an uncapped flat 2s; #646's second
        // half moved it onto `retry::RECONNECT_RETRY`, resetting the attempt
        // count after a `listen` that stayed up at least
        // `retry::RECONNECT_RESET_AFTER` so a merely-flaky adapter doesn't
        // ratchet to the 30s ceiling and stay there. #806 moved the counter
        // itself into `retry::ReconnectBackoff`: the reset/read/advance
        // ordering it encodes was hand-rolled here (and in three sibling
        // loops), and hand-rolled wrong — the reset landed a cycle late, so a
        // run that stayed healthy for hours still reconnected at the ratcheted
        // delay. All this loop owns now is the clock and the `warn!`.
        spawn_supervised("bluetooth", move || {
            let adapter_mutable = adapter_mutable.clone();
            let devices_mutable = devices_mutable.clone();
            let path_store = path_store.clone();
            async move {
                let mut backoff = retry::ReconnectBackoff::new();
                loop {
                    let started = std::time::Instant::now();
                    let outcome = listen(&adapter_mutable, &devices_mutable, &path_store).await;
                    let delay = backoff.delay_after_run(started.elapsed());
                    match outcome {
                        Ok(ListenEnd::StreamClosed) => {
                            tracing::warn!(?delay, "bluetooth watcher closed, reconnecting");
                        }
                        Ok(ListenEnd::DaemonExited) => {
                            tracing::info!("bluetooth watcher: waiting for bluetoothd to return");
                        }
                        Err(e) => {
                            tracing::warn!(?delay, error = %e, "bluetooth watcher error, reconnecting");
                        }
                    }
                    // Clear state when the adapter disappears: the device list,
                    // any in-flight action markers, the active pair prompt, and
                    // the pending agent reply (so the agent method handler that's
                    // awaiting it returns Reject instead of hanging forever).
                    adapter_mutable.set(None);
                    devices_mutable.set(Vec::new());
                    let pending_response = shared_state().map(|s| {
                        s.device_actions.lock_mut().clear();
                        if s.pair_prompt.lock_ref().is_some() {
                            s.pair_prompt.set(None);
                        }
                        s.pending_response.clone()
                    });
                    if let Some(pending) = pending_response {
                        agent::cancel_in(&pending).await;
                    }
                    // Also reset the adapter path store so the next listen()
                    // call starts fresh.
                    set_adapter_path_local("").await;
                    // Reconnect only to a bluetoothd that is running. The
                    // next listen() calls org.bluez, and that call would
                    // D-Bus-activate a stopped one. This applies to every
                    // exit, not only `DaemonExited`: a dying bluetoothd can
                    // remove its adapter just before it lets go of the name.
                    // With bluetoothd up, this returns after one round trip
                    // to the bus.
                    wait_for_bluez().await;
                    tokio::time::sleep(delay).await;
                }
            }
        });

        // Pairing-agent loop, independent of the watcher: stays registered
        // for the process lifetime, retrying on errors (e.g. bluetoothd
        // restart). The agent connection is managed by bus::own_name which
        // handles reconnects automatically.
        spawn_supervised("bluetooth", move || {
            let ownership = ownership.clone();
            async move {
                run_agent(ownership).await;
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

/// Signal emitting the most recent connect/pair/disconnect failure, or
/// `None` before the first one (#1171).
///
/// **A subscriber must gate on freshness, not just on `Some`.** This is a
/// `signal_cloned()` over a handle that is never cleared, so the first poll
/// of a *newly built* subscriber yields whatever failure is currently
/// recorded, however old — which is a toast for a failure that already
/// happened, possibly hours ago on another monitor. Record an
/// `Instant::now()` where the binding is created and forward only errors
/// satisfying [`ActionError::is_newer_than`]; `panels/bluetooth.rs` is the
/// worked example.
///
/// Repeats are real: `Mutable::set` always notifies, even on an unchanged
/// value (see `mark_busy`'s doc for the same invariant), so a second
/// identical failure is a fresh emission — though two inside one main-loop
/// turn coalesce, since a polling subscriber only ever sees the latest.
pub fn action_error() -> impl Signal<Item = Option<ActionError>> {
    registry::with(|r| {
        r.get::<BluetoothHandles>()
            .expect("bluetooth::service() not registered")
            .action_error
            .signal_cloned()
    })
}

/// Record a connect/pair/disconnect failure for [`action_error`] to surface
/// (#1171). Callable from any thread — goes through [`shared_state`], not
/// the GTK-thread-only registry, since every call site here runs on the
/// tokio runtime. No-op if the service isn't registered (test harness).
fn report_action_error(message: impl Into<String>) {
    let Some(shared) = shared_state() else {
        return;
    };
    set_action_error(&shared.action_error, message.into());
}

/// The actual state transition (#1171), split out of [`report_action_error`]
/// so it's testable without touching the process-global `shared` map: record
/// `message` as the latest connect/pair/disconnect failure, stamped with the
/// moment it happened (see [`ActionError::at`]).
fn set_action_error(action_error: &Mutable<Option<ActionError>>, message: String) {
    action_error.set(Some(ActionError {
        message,
        at: std::time::Instant::now(),
    }));
}

/// Resolve the yes/no pairing prompt `id` ([`PairPrompt::id`], the prompt
/// the user answered). `accept = true` returns `Ok` to `BlueZ`, completing
/// the pair; `false` returns `org.bluez.Error.Rejected` and aborts.
///
/// The answer reaches only the request that raised prompt `id`. If that
/// request is no longer waiting (cancelled, timed out, or already answered),
/// the answer is dropped and decides nothing, even if another request is
/// waiting by then (#1551): a Confirm clicked on one device's code must never
/// pair a different device whose code the user never saw.
pub fn respond_to_prompt(id: u64, accept: bool) {
    send_reply(
        id,
        if accept {
            AgentReply::Confirm
        } else {
            AgentReply::Reject
        },
    );
}

/// Resolve the `RequestPinCode` prompt `id` by submitting the user's PIN.
/// Empty string is treated as a reject so the user can dismiss the
/// banner without triggering a malformed pair. Bound to `id` the way
/// [`respond_to_prompt`] is.
pub fn submit_pin(id: u64, pin: String) {
    if pin.is_empty() {
        send_reply(id, AgentReply::Reject);
    } else {
        send_reply(id, AgentReply::Pin(pin));
    }
}

/// Resolve the `RequestPasskey` prompt `id` by submitting the user's numeric
/// passkey. Out-of-range values reject so we never hand `BlueZ` something
/// it'll trip on. Bound to `id` the way [`respond_to_prompt`] is.
pub fn submit_passkey(id: u64, passkey: u32) {
    if passkey > 999_999 {
        send_reply(id, AgentReply::Reject);
    } else {
        send_reply(id, AgentReply::Passkey(passkey));
    }
}

fn send_reply(id: u64, reply: AgentReply) {
    let Some(pending) = shared_state().map(|s| s.pending_response.clone()) else {
        return;
    };
    runtime::handle().spawn(async move {
        agent::answer_in(&pending, id, reply).await;
    });
}

fn mark_busy(path: &str) {
    let Some(shared) = shared_state() else {
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
    let Some(shared) = shared_state() else {
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
            report_action_error(format!("Couldn't connect: {e}"));
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
            report_action_error(format!("Couldn't disconnect: {e}"));
        }
        mark_idle(&path);
    });
}

/// Fire-and-forget: call `Pair` on the given device; once it succeeds, mark
/// it trusted (so it auto-reconnects next session and never asks before using
/// a service) and connect it. While the pairing runs, the agent lets the
/// device's service requests through; a pairing that fails sets nothing
/// (`trust::pair_then_trust`). The pair-trust-connect chain is what most
/// users expect from a "tap to pair my headphones" interaction. For devices
/// that require a PIN/passkey `BlueZ` delegates to a registered `Agent1`;
/// without one such pairings will silently fail.
pub fn pair_device(device_path: &str) {
    let path = device_path.to_string();
    mark_busy(&path);
    // Shared with the agent, so its `AuthorizeService` sees this pairing.
    let pairings = shared_state()
        .map(|s| s.pairings.clone())
        .unwrap_or_default();
    runtime::handle().spawn(async move {
        match trust::pair_then_trust(&trust::SystemBluez, &pairings, &path).await {
            PairOutcome::Paired => {}
            PairOutcome::PairedDespite(e) => {
                tracing::info!(
                    error = %e,
                    path,
                    "Pair answered an error but the device is paired; trusted and connecting"
                );
            }
            PairOutcome::Failed(e) => {
                tracing::warn!(error = %e, path, "bluetooth pair_device failed");
                report_action_error(format!("Couldn't pair: {e}"));
            }
        }
        mark_idle(&path);
    });
}

/// What [`pair_device`] does once its `Pair` call has answered.
#[derive(Debug)]
enum PairOutcome {
    /// `Pair` succeeded: run the follow-up.
    Paired,
    /// `Pair` answered an error, but the device is paired anyway: run the
    /// follow-up and post no toast. The error is kept only for the journal.
    PairedDespite(hytte_bus::BusError),
    /// The device is not paired: post this error as a toast.
    Failed(hytte_bus::BusError),
}

/// Decide [`PairOutcome`] from the `Pair` reply and from whether the device
/// shows as paired once that reply is in.
///
/// The follow-up depends on the device being paired, not on `Pair` saying
/// so, because the two can disagree:
/// - `org.bluez.Error.AlreadyExists` is bluetoothd's own answer for "this
///   device is already bonded". The usual causes are a stale row, or the
///   remote side finishing the pairing first. Until now that toasted
///   "Couldn't pair", and the device stayed untrusted and disconnected.
/// - A reply that arrives after [`PAIRING_CALL_TIMEOUT`] is lost to us, but
///   bluetoothd may still finish the pairing. If the device shows as paired by
///   the time the deadline error comes back, the follow-up runs anyway.
///
/// `paired_now` reads the watcher's snapshot, and it is weighed **only**
/// against the deadline error ([`is_deadline`]), the one error that says
/// nothing about how the pairing went. Every other error is bluetoothd's
/// answer, and it outranks the snapshot. Until #1551 any error counted as
/// success while the snapshot said paired. A stale `paired` (the watcher not
/// yet caught up with an unpair, say) plus a real refusal such as
/// `org.bluez.Error.AuthenticationRejected` then trusted and connected the
/// device, with no toast. `paired_now` is never a reason to treat a
/// *successful* reply as a failure either.
fn pair_outcome(reply: Result<(), hytte_bus::BusError>, paired_now: bool) -> PairOutcome {
    match reply {
        Ok(()) => PairOutcome::Paired,
        Err(e) if is_already_exists(&e) || (paired_now && is_deadline(&e)) => {
            PairOutcome::PairedDespite(e)
        }
        Err(e) => PairOutcome::Failed(e),
    }
}

/// The reason `hytte-bus` gives when a call's own deadline runs out: `call.rs`
/// maps the elapsed `tokio::time::timeout` to `zbus::Error::Failure("call
/// timeout")`, zbus 5 displays a `Failure` as its bare string, and
/// `BusError::from_zbus` carries it into `BusError::Permanent` with no D-Bus
/// error name. `trollshell-control-center`'s `plugins_tab.rs` reads the same
/// shape the same way. `the_deadline_is_recognised_as_hytte_bus_spells_it`
/// reads the literal out of `call.rs`, and
/// `hytte_bus_carries_its_deadline_to_what_is_deadline_reads` runs the real
/// `BusError::from_zbus` over it, so a change to either reddens here.
const HYTTE_BUS_CALL_TIMEOUT: &str = "call timeout";

/// Is this the `Pair` call's own deadline running out, rather than an answer
/// from bluetoothd? An error bluetoothd sends always carries a D-Bus error
/// name, which is what tells the two apart.
///
/// The bus's `org.freedesktop.DBus.Error.NoReply` is deliberately not
/// counted. It means bluetoothd went away with the call unanswered, so the
/// snapshot describes a daemon that is gone, and the follow-up's own calls to
/// `org.bluez` would D-Bus-activate a bluetoothd that was stopped on purpose.
fn is_deadline(e: &hytte_bus::BusError) -> bool {
    matches!(
        e,
        hytte_bus::BusError::Permanent { reason, dbus_name: None }
            if reason == HYTTE_BUS_CALL_TIMEOUT
    )
}

/// Is this bluetoothd's "already bonded" answer to `Pair`?
fn is_already_exists(e: &hytte_bus::BusError) -> bool {
    matches!(
        e,
        hytte_bus::BusError::Permanent { dbus_name: Some(name), .. }
            if name == "org.bluez.Error.AlreadyExists"
    )
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
    hytte_bus::call(BusKind::System, "org.bluez")
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
    hytte_bus::call(BusKind::System, "org.bluez")
        .at_path(device_path.to_string())
        .iface("org.freedesktop.DBus.Properties")
        .method("Set")
        .args(("org.bluez.Device1".to_string(), prop, value))
        .send::<()>()
        .await
}

async fn do_adapter_call(adapter_path: &str, method: &str) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, "org.bluez")
        .at_path(adapter_path.to_string())
        .iface("org.bluez.Adapter1")
        .method(method)
        .args(())
        .send::<()>()
        .await
}

async fn do_device_call(device_path: &str, method: &str) -> Result<(), hytte_bus::BusError> {
    let call = hytte_bus::call(BusKind::System, "org.bluez")
        .at_path(device_path.to_string())
        .iface("org.bluez.Device1")
        .method(method);
    let call = match device_call_deadline(method) {
        Some(deadline) => call.timeout(deadline),
        None => call,
    };
    call.args(()).send::<()>().await
}

/// The deadline a `Device1` method gets, or `None` for `hytte_bus::call`'s
/// default. Only `Pair` and `Connect` take longer than that default. See
/// [`PAIRING_CALL_TIMEOUT`].
fn device_call_deadline(method: &str) -> Option<Duration> {
    matches!(method, "Pair" | "Connect").then_some(PAIRING_CALL_TIMEOUT)
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
    hytte_bus::call(BusKind::System, "org.bluez")
        .at_path(adapter_path.to_string())
        .iface("org.bluez.Adapter1")
        .method("RemoveDevice")
        .args((dev_op,))
        .send::<()>()
        .await
}

#[cfg(test)]
mod tests {
    use super::{
        ActionError, BLUEZ_AGENT_REQUEST_TIMEOUT, HYTTE_BUS_CALL_TIMEOUT, PairOutcome,
        device_call_deadline, is_deadline, pair_outcome, set_action_error,
    };
    use futures_signals::signal::{Mutable, Signal};
    use hytte_bus::BusError;
    use std::time::Instant;

    /// `Pair` blocks while the user answers our agent's prompt, and
    /// bluetoothd gives that prompt 60 s. A deadline shorter than that fires
    /// while pairing is still in progress: a false "Couldn't pair" toast, no
    /// follow-up, and a re-enabled row that answers `InProgress`. `Connect`
    /// has the same deadline, because it is also the second half of that
    /// follow-up.
    ///
    /// **Falsification:** make `device_call_deadline` return `None` for
    /// `Pair` (the 25 s default) and this fails.
    #[test]
    fn pair_and_connect_outlast_an_agent_prompt() {
        for method in ["Pair", "Connect"] {
            let deadline = device_call_deadline(method);
            assert!(
                deadline.is_some_and(|d| d > BLUEZ_AGENT_REQUEST_TIMEOUT),
                "{method} must wait out a whole agent prompt, got {deadline:?}"
            );
        }
        assert_eq!(
            device_call_deadline("Disconnect"),
            None,
            "a call that waits on nobody keeps the bus default"
        );
    }

    fn method_error(name: &str) -> BusError {
        BusError::Permanent {
            reason: name.rsplit('.').next().unwrap_or(name).to_owned(),
            dbus_name: Some(name.to_owned()),
        }
    }

    /// What `hytte_bus::call` answers when its own deadline fires.
    fn deadline_error() -> BusError {
        BusError::Permanent {
            reason: HYTTE_BUS_CALL_TIMEOUT.to_owned(),
            dbus_name: None,
        }
    }

    /// An error bluetoothd sent outranks a snapshot that shows the device as
    /// paired: only the deadline error leaves the pairing's fate open.
    ///
    /// **Falsification:** let `paired_now` win over any error again (the
    /// pre-#1551 `Err(e) if paired_now || …` guard) and every row here is
    /// trusted and connected instead of toasted.
    #[test]
    fn a_refusal_outranks_a_snapshot_that_says_paired() {
        for err in [
            method_error("org.bluez.Error.AuthenticationRejected"),
            method_error("org.bluez.Error.AuthenticationFailed"),
            method_error("org.bluez.Error.AuthenticationCanceled"),
            method_error("org.bluez.Error.ConnectionAttemptFailed"),
            method_error("org.freedesktop.DBus.Error.NoReply"),
            // An error from bluetoothd that happens to read like the deadline
            // still carries its name, so it is still an answer.
            BusError::Permanent {
                reason: HYTTE_BUS_CALL_TIMEOUT.to_owned(),
                dbus_name: Some("org.bluez.Error.Failed".to_owned()),
            },
        ] {
            let shown = format!("{err:?}");
            assert!(
                matches!(pair_outcome(Err(err), true), PairOutcome::Failed(_)),
                "{shown} must toast even when the snapshot says paired"
            );
        }
    }

    /// The deadline error is recognised the way `hytte-bus` spells it, read
    /// from its own source rather than from a copy: `call.rs` gives up with
    /// `zbus::Error::Failure("call timeout")`. How `error.rs` carries that
    /// `Failure` into a `BusError` is pinned by running it, in
    /// `hytte_bus_carries_its_deadline_to_what_is_deadline_reads`.
    ///
    /// **Falsification:** change `HYTTE_BUS_CALL_TIMEOUT`, or the literal in
    /// `hytte-bus`'s `call.rs`, and the first assertion fails.
    #[test]
    fn the_deadline_is_recognised_as_hytte_bus_spells_it() {
        let call_rs = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../hytte-bus/src/call.rs"
        ));
        // Not squashed: the literal's own space is part of what is pinned.
        assert!(
            call_rs.contains(&format!(
                "zbus::Error::Failure({HYTTE_BUS_CALL_TIMEOUT:?}.into())"
            )),
            "hytte-bus no longer spells its call deadline {HYTTE_BUS_CALL_TIMEOUT:?}"
        );

        assert!(is_deadline(&deadline_error()));
        assert!(!is_deadline(&method_error("org.bluez.Error.Failed")));
        assert!(!is_deadline(&BusError::Permanent {
            reason: "disk full".to_owned(),
            dbus_name: None,
        }));
    }

    /// The deadline as `hytte-bus` actually delivers it: its own
    /// `zbus::Error::Failure("call timeout")` run through the real
    /// `BusError::from_zbus`, so the mapping and zbus's `Display` of a
    /// `Failure` are both exercised (#1573 review). A source scan of
    /// `error.rs` only proved a catch-all arm existed, and stayed green when
    /// a dedicated `Failure` arm in front of it changed the reason.
    ///
    /// **Falsification:** give `BusError::from_zbus` a `Failure` arm that
    /// rewrites the reason (e.g. `format!("bus failure: {msg}")`) and this
    /// fails, while a device paired at the 5-minute deadline would toast.
    #[test]
    fn hytte_bus_carries_its_deadline_to_what_is_deadline_reads() {
        let e = BusError::from_zbus(zbus::Error::Failure(HYTTE_BUS_CALL_TIMEOUT.into()));
        assert!(is_deadline(&e), "{e:?}");
    }

    /// The trust-and-connect follow-up runs when the device is paired, not
    /// only when the `Pair` reply says so.
    #[test]
    fn the_follow_up_runs_whenever_the_device_is_paired() {
        assert!(matches!(pair_outcome(Ok(()), false), PairOutcome::Paired));

        // bluetoothd's own "already bonded": the follow-up must still run,
        // whatever the snapshot shows.
        assert!(matches!(
            pair_outcome(Err(method_error("org.bluez.Error.AlreadyExists")), false),
            PairOutcome::PairedDespite(_)
        ));

        // The deadline fired, but by the time its error came back the device
        // showed as paired: the reply was late, the pairing was not lost.
        assert!(matches!(
            pair_outcome(Err(deadline_error()), true),
            PairOutcome::PairedDespite(_)
        ));
    }

    /// A pairing that really failed still reaches the toast.
    #[test]
    fn a_pairing_that_failed_still_toasts() {
        for err in [
            method_error("org.bluez.Error.AuthenticationFailed"),
            method_error("org.bluez.Error.AuthenticationCanceled"),
            deadline_error(),
        ] {
            assert!(
                matches!(pair_outcome(Err(err), false), PairOutcome::Failed(_)),
                "an unpaired device after an error is a failure"
            );
        }
    }

    /// The message on the current `action_error`, or `None`.
    fn message(action_error: &Mutable<Option<ActionError>>) -> Option<String> {
        action_error.get_cloned().map(|e| e.message)
    }

    /// #1171: a connect/pair/disconnect failure moves `action_error` from
    /// `None` to `Some`, and a later failure replaces it rather than
    /// stacking — the UI shows one toast at a time, forwarding whatever the
    /// latest emission carries.
    #[test]
    fn action_error_transitions_from_none_to_the_latest_failure() {
        let action_error: Mutable<Option<ActionError>> = Mutable::new(None);
        assert_eq!(action_error.get_cloned(), None, "starts with no failure");

        set_action_error(&action_error, "Couldn't connect: timed out".to_string());
        assert_eq!(
            message(&action_error).as_deref(),
            Some("Couldn't connect: timed out"),
        );

        set_action_error(&action_error, "Couldn't pair: rejected".to_string());
        assert_eq!(
            message(&action_error).as_deref(),
            Some("Couldn't pair: rejected"),
            "a later failure replaces the earlier one",
        );
    }

    /// #1192 review, MEDIUM-2 — the stale-toast bug, driven through the same
    /// signal a panel binds.
    ///
    /// `signal_cloned()` replays: a subscriber created *after* a failure gets
    /// that failure on its first poll. That is a Bluetooth toast on a second
    /// monitor plugged in an hour later, for a connect that failed once and
    /// was already reported. The gate is `is_newer_than(built_at)`, where
    /// `built_at` is stamped when the binding is created — so a late
    /// subscriber sees the value and declines to toast it, while a failure
    /// that happens afterwards still gets through.
    ///
    /// **Falsification:** make `is_newer_than` return `true`
    /// unconditionally and the stale half of this test fails.
    #[test]
    fn a_late_subscriber_does_not_toast_a_failure_older_than_itself() {
        let action_error: Mutable<Option<ActionError>> = Mutable::new(None);

        // 10:00 — a connect fails on the monitor that already has a panel.
        set_action_error(&action_error, "Couldn't connect: timed out".to_string());

        // 11:00 — a second monitor is plugged in and its panel is built.
        // ("created after the failure" is the whole predicate, so a
        // millisecond of separation models an hour of it exactly; the sleep
        // is only there so the two `Instant`s cannot land on one tick and
        // make the comparison vacuous.)
        std::thread::sleep(std::time::Duration::from_millis(1));
        let built_at = Instant::now();

        // Its binding's FIRST poll yields the hour-old failure…
        let mut signal = std::pin::pin!(action_error.signal_cloned());
        let replayed =
            futures_executor::block_on(std::future::poll_fn(|cx| signal.as_mut().poll_change(cx)))
                .expect("the signal must not end")
                .expect("a fresh subscriber replays the current value");
        assert_eq!(
            replayed.message, "Couldn't connect: timed out",
            "this replay is the bug's raw material — it is expected here",
        );

        // …and the freshness gate is what keeps it off the screen.
        assert!(
            !replayed.is_newer_than(built_at),
            "a failure older than the binding must not be toasted",
        );

        // A genuinely new failure still gets through the same gate.
        std::thread::sleep(std::time::Duration::from_millis(1));
        set_action_error(&action_error, "Couldn't pair: rejected".to_string());
        let fresh = action_error.get_cloned().expect("a failure was just set");
        assert!(
            fresh.is_newer_than(built_at),
            "a failure after the binding was created must still toast",
        );
    }
}
