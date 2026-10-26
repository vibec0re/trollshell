//! System-tray host via the `StatusNotifierItem` protocol.
//!
//! Registers `org.kde.StatusNotifierWatcher` on the session bus, accepts
//! `RegisterStatusNotifierItem(...)` calls, queries each item's properties,
//! and exposes a [`Mutable<Vec<TrayItem>>`] signal.
//!
//! # Protocol notes
//!
//! The `service` argument to `RegisterStatusNotifierItem` may be either an
//! object path (starts with `/`) or a bus name.  When it is an object path
//! the bus name is taken from the D-Bus message sender.
//!
//! # v0.3.3 scope
//!
//! `IconPixmap` fallback, rich `Tooltip`, and `DBusMenu` via
//! `com.canonical.dbusmenu`.
//!
//! # Module layout
//!
//! The untrusted-input parsers (the `com.canonical.dbusmenu` layout tree and
//! the `RegisterStatusNotifierItem` argument) live in `parse`, which is
//! pure and hermetically unit-tested; the menu data shapes live in `types`.
//! This file keeps the service, D-Bus, and signal-emit logic.

mod parse;
mod types;

pub use parse::strip_accel;
pub use types::{Menu, MenuEntry, MenuItem, ToggleType};

use anyhow::{Context, Result};
use futures_signals::signal::SignalExt;
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_bus::{BusKind, OwnNameSignal, OwnState, ProxyState, SignalItem, call, proxy, signals};
use hytte_reactive::{Service, registry, runtime, spawn_supervised, spawn_supervised_bounded};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Structure, Value};

use crate::retry;

// ── Public data shapes ────────────────────────────────────────────────────────

/// Active/passive/attention status of a [`TrayItem`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ItemStatus {
    /// The item is not relevant to the user at the moment.
    #[default]
    Passive,
    /// The item is active and relevant.
    Active,
    /// The item needs the user's attention.
    NeedsAttention,
}

impl ItemStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "Active" => Self::Active,
            "NeedsAttention" => Self::NeedsAttention,
            _ => Self::Passive,
        }
    }
}

/// A single tray item.
#[derive(Clone, Debug)]
pub struct TrayItem {
    /// Stable key: `"{bus_name}{object_path}"`.
    pub key: String,
    /// The D-Bus bus name that owns this item.
    pub bus_name: String,
    /// The D-Bus object path for this item.
    pub object_path: String,
    /// Human-readable title (may be empty).
    pub title: String,
    /// Icon name for [`gtk::Image`] (may be empty; use a generic fallback).
    pub icon_name: String,
    /// Current item status.
    pub status: ItemStatus,
    /// Raw ARGB32 pixmap — `(width, height, bytes)` — picked from the largest
    /// entry of the `IconPixmap` array. `None` when no pixmap is available.
    /// Always populated when the app provides one, even when `icon_name` is
    /// also set — the widget layer decides which to use based on theme
    /// availability.
    pub icon_pixmap: Option<(i32, i32, Vec<u8>)>,
    /// Tooltip title (may be empty).
    pub tooltip_title: String,
    /// Tooltip description (may be empty).
    pub tooltip_description: String,
    /// Object path of the `com.canonical.dbusmenu` menu, if any.
    pub menu_path: Option<String>,
    /// `ItemIsMenu` — when `true`, the SNI spec asks visualizations to treat
    /// primary click as "show menu" rather than `Activate`. Common for Qt/KDE
    /// status icons that have no separate primary action.
    pub item_is_menu: bool,
}

// ── Service handle ────────────────────────────────────────────────────────────

/// Service handle returned by [`service()`].
#[doc(hidden)]
pub struct TrayHandles {
    pub(crate) items: Mutable<Vec<TrayItem>>,
    /// The `own_name` task's live [`OwnState`], published by [`ownership()`]
    /// so a lost name race is visible in the UI rather than presenting as a
    /// permanently empty tray (#747, #653). Also what keeps the `own_name`
    /// task owning `org.kde.StatusNotifierWatcher` for the process lifetime.
    ///
    /// Held as the [`OwnNameSignal`] itself since #750 — see the equivalent
    /// field on `notifications`' handles for the `Mutable` mirror this
    /// replaced and why it existed.
    ownership: OwnNameSignal,
}

// ── Service entry-point ───────────────────────────────────────────────────────

/// The tray service marker type.
pub struct TrayService;

impl Service for TrayService {
    type Handles = TrayHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let items = Mutable::new(Vec::new());
        let ownership_slot: Arc<OnceLock<OwnNameSignal>> = Arc::new(OnceLock::new());

        let state = State {
            items: items.clone(),
            registered: Arc::new(AsyncMutex::new(HashMap::new())),
            ownership: ownership_slot.clone(),
        };

        let watcher = Watcher {
            state: state.clone(),
        };

        // Own `org.kde.StatusNotifierWatcher` + mount the watcher interface.
        // The OwnNameSignal is stored in TrayHandles (process lifetime) and in
        // `state.ownership` so that watch_item tasks can emit signals directly
        // without a round-trip D-Bus call.
        let ownership = hytte_bus::own_name(BusKind::Session, "org.kde.StatusNotifierWatcher")
            .at_path(WATCHER_PATH, watcher)
            .start();

        // Populate the OnceLock so watch_item tasks can call emit_unregistered.
        // Items only start registering after the watcher interface is mounted,
        // so this is always set before any watch_item task runs.
        let _ = ownership_slot.set(ownership.clone());

        // Spawn the NameOwnerChanged watcher to prune items when their bus
        // name disappears. Supervised so a panic in the watcher restarts the
        // reconnect loop rather than leaving pruning dead for the process
        // lifetime; the inner `loop` still drives the ordinary reconnect.
        //
        // The reconnect delay used to be an uncapped flat 2s; #795's first
        // half moved it onto `retry::RECONNECT_RETRY`, resetting the attempt
        // count after a `watch_name_owner_changes` run that stayed up at
        // least `retry::RECONNECT_RESET_AFTER` so a merely-flaky session bus
        // doesn't ratchet to the 30s ceiling and stay there. #806 moved the
        // counter itself into `retry::ReconnectBackoff`: the reset/read/advance
        // ordering it encodes was hand-rolled here (and in three sibling
        // loops), and hand-rolled wrong — the reset landed a cycle late, so a
        // run that stayed healthy for hours still reconnected at the ratcheted
        // delay. All this loop owns now is the clock and the `warn!`.
        let state2 = state;
        spawn_supervised("tray", move || {
            let state = state2.clone();
            async move {
                let mut backoff = retry::ReconnectBackoff::new();
                loop {
                    let started = std::time::Instant::now();
                    let outcome = watch_name_owner_changes(&state).await;
                    let delay = backoff.delay_after_run(started.elapsed());
                    match outcome {
                        Ok(()) => {
                            tracing::warn!(?delay, "tray NOC stream closed, reconnecting");
                        }
                        Err(e) => {
                            tracing::warn!(?delay, error = %e, "tray NOC watcher error, reconnecting");
                        }
                    }
                    tokio::time::sleep(delay).await;
                }
            }
        });

        TrayHandles { items, ownership }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the tray service to register with the hytte runtime.
#[must_use]
pub fn service() -> TrayService {
    TrayService
}

/// Signal that emits the current list of tray items.
pub fn items() -> impl Signal<Item = Vec<TrayItem>> {
    registry::with(|r| {
        r.get::<TrayHandles>()
            .expect("tray::service() not registered")
            .items
            .signal_cloned()
    })
}

/// Signal of the shell's hold on the `org.kde.StatusNotifierWatcher` bus name.
///
/// The watcher name is a session singleton in the same way
/// `org.freedesktop.Notifications` is: tray apps look it up once and register
/// with whoever answers. Lose it to a second bar, plasmashell, or a leftover
/// `snixembed`, and no item ever registers with us — [`items()`] stays empty
/// and, from the UI, that is indistinguishable from "no app has a tray icon".
///
/// See [`crate::notifications::ownership`] for how to read the individual
/// states; the rules are identical (`Acquiring`/`Lost` are in-flight and must
/// not be surfaced, `PermanentlyTaken`/`Denied` mean inert).
pub fn ownership() -> impl Signal<Item = OwnState> {
    registry::with(|r| {
        r.get::<TrayHandles>()
            .expect("tray::service() not registered")
            .ownership
            .signal_cloned()
    })
}

/// Fire-and-forget: send `Activate(0, 0)` to the given `StatusNotifierItem`.
pub fn activate(bus_name: &str, object_path: &str) {
    call(BusKind::Session, bus_name)
        .at_path(object_path)
        .iface(SNI_IFACE)
        .method("Activate")
        .args((0i32, 0i32))
        .fire_and_forget();
}

/// Fire-and-forget: send `ContextMenu(0, 0)` — asks the app to show its own
/// context menu. Used as a fallback when an item has no `com.canonical.dbusmenu`
/// path, and as the primary "show menu" action for `ItemIsMenu` items.
pub fn context_menu(bus_name: &str, object_path: &str) {
    call(BusKind::Session, bus_name)
        .at_path(object_path)
        .iface(SNI_IFACE)
        .method("ContextMenu")
        .args((0i32, 0i32))
        .fire_and_forget();
}

/// Fetch the `com.canonical.dbusmenu` layout tree for the given bus + path.
///
/// Calls `AboutToShow(0)` first (some apps need this to populate their menu),
/// then calls `GetLayout(-1)` to fetch the full tree in one round-trip.
/// Returns `None` on any error.
///
/// Internally dispatches the zbus work onto the hytte tokio runtime and
/// bridges the result back via a oneshot channel, so this future is safe
/// to await from any executor (e.g. `glib::MainContext::spawn_local`).
pub async fn fetch_menu(bus_name: &str, menu_path: &str) -> Option<Menu> {
    let bus = bus_name.to_string();
    let path = menu_path.to_string();
    let (tx, rx) = futures_channel::oneshot::channel();
    runtime::handle().spawn(async move {
        let result = match do_fetch_menu(&bus, &path).await {
            Ok(m) => Some(m),
            Err(e) => {
                // `{e:#}` (anyhow alternate Display) walks the full cause chain
                // on one line — plain `%e` shows only the outermost `.context`
                // ("call GetLayout"), hiding the real zbus error (#8).
                tracing::debug!(error = %format!("{e:#}"), bus_name = bus, menu_path = path, "fetch_menu failed");
                None
            }
        };
        let _ = tx.send(result);
    });
    rx.await.ok().flatten()
}

async fn do_fetch_menu(bus_name: &str, menu_path: &str) -> Result<Menu> {
    // Call AboutToShow(0) — ignore errors (not all apps implement it).
    let _ = call(BusKind::Session, bus_name)
        .at_path(menu_path)
        .iface(DBUSMENU_IFACE)
        .method("AboutToShow")
        .args((0i32,))
        .send::<bool>()
        .await;

    let property_names: Vec<&str> = vec![
        "label",
        "enabled",
        "visible",
        "type",
        "icon-name",
        "toggle-type",
        "toggle-state",
        "children-display",
    ];

    // GetLayout returns `(u(ia{sv}av))`: a `u32` revision plus the concrete
    // root layout-node struct. Deserialize into that exact shape — the old
    // `(u32, OwnedValue)` (signature `(uv)`) made zbus 5.x reject every reply
    // with a signature mismatch, dropping the tray to the plain ContextMenu
    // (#8). See `parse::LayoutNode`.
    let (_, layout): (u32, parse::LayoutNode) = call(BusKind::Session, bus_name)
        .at_path(menu_path)
        .iface(DBUSMENU_IFACE)
        .method("GetLayout")
        .args((0i32, -1i32, property_names))
        .send()
        .await
        .context("call GetLayout")?;

    Ok(parse::parse_layout_node(layout))
}

/// Fire-and-forget: send `Event(id, "clicked", null, timestamp)` on the
/// tokio runtime.
pub fn menu_event(bus_name: &str, menu_path: &str, item_id: i32) {
    let bus_name = bus_name.to_string();
    let menu_path = menu_path.to_string();
    runtime::handle().spawn(async move {
        if let Err(e) = do_menu_event(&bus_name, &menu_path, item_id).await {
            tracing::warn!(error = %format!("{e:#}"), bus_name, menu_path, item_id, "menu_event failed");
        }
    });
}

async fn do_menu_event(bus_name: &str, menu_path: &str, item_id: i32) -> Result<()> {
    #[allow(clippy::cast_possible_truncation)]
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as u32;

    // data: variant — pass an i32(0) variant as a no-op payload.
    let data = OwnedValue::try_from(Value::I32(0)).unwrap();

    call(BusKind::Session, bus_name)
        .at_path(menu_path)
        .iface(DBUSMENU_IFACE)
        .method("Event")
        .args((item_id, "clicked", data, timestamp))
        .send::<()>()
        .await
        .context("call Event")?;

    Ok(())
}

// ── Watcher state ─────────────────────────────────────────────────────────────

/// Shared mutable state threaded through the watcher and per-item tasks.
///
/// No longer holds a `Connection`; all D-Bus I/O goes through `hytte_bus::call`
/// and `hytte_bus::proxy`.
///
/// `ownership` is set once after `own_name(...).start()` returns. The
/// `OnceLock` is always populated before any `watch_item` task can call
/// `emit_unregistered`, because item registrations only arrive after the
/// watcher interface is fully mounted on the bus.
#[derive(Clone)]
struct State {
    items: Mutable<Vec<TrayItem>>,
    registered: Arc<AsyncMutex<HashMap<String, TrayItem>>>,
    ownership: Arc<OnceLock<OwnNameSignal>>,
}

impl State {
    /// Re-read one item's properties and update the map.  Returns `false`
    /// when the item should be removed: it answered none of its identifying
    /// properties (see [`read_item_props`]'s `# Errors`).
    async fn refresh_item(&self, bus_name: &str, object_path: &str) -> bool {
        match read_item_props(bus_name, object_path).await {
            Ok(item) => {
                let mut map = self.registered.lock().await;
                map.insert(item.key.clone(), item);
                true
            }
            Err(e) => {
                tracing::debug!(error = %e, bus_name, object_path, "item property read failed, removing");
                let key = format!("{bus_name}{object_path}");
                self.registered.lock().await.remove(&key);
                false
            }
        }
    }

    /// Remove all items owned by `bus_name`.
    async fn unregister_by_bus_name(&self, bus_name: &str) {
        let mut map = self.registered.lock().await;
        let before = map.len();
        map.retain(|_, v| v.bus_name != bus_name);
        if map.len() != before {
            drop(map);
            self.rebuild_published_list().await;
        }
    }

    /// Publish the current item list (sorted by key for stable order).
    async fn rebuild_published_list(&self) {
        let map = self.registered.lock().await;
        let mut list: Vec<TrayItem> = map.values().cloned().collect();
        drop(map);
        list.sort_by(|a, b| a.key.cmp(&b.key));
        self.items.set(list);
    }

    /// Emit `StatusNotifierItemUnregistered` directly on the owned watcher
    /// connection, bypassing the `_EmitUnregistered` round-trip.
    async fn emit_unregistered(&self, key: String) {
        let Some(ownership) = self.ownership.get() else {
            tracing::warn!(key, "emit_unregistered: ownership not yet set");
            return;
        };
        let result = ownership
            .emit(WATCHER_PATH, |emitter| async move {
                Watcher::status_notifier_item_unregistered(&emitter, key).await
            })
            .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, "StatusNotifierItemUnregistered emit failed");
        }
    }
}

// ── `StatusNotifierWatcher` D-Bus interface ───────────────────────────────────

const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const SNI_IFACE: &str = "org.kde.StatusNotifierItem";
const DBUSMENU_IFACE: &str = "com.canonical.dbusmenu";

/// Service-side implementation of `org.kde.StatusNotifierWatcher`.
///
/// Derives `Clone` as required by `own_name().at_path()`.
#[derive(Clone)]
struct Watcher {
    state: State,
}

// zbus's `#[interface]` macro requires every method to be `async fn` even
// when the body doesn't await. Allowing at the impl-block keeps the noise
// out of each method.
#[allow(clippy::unused_async)]
#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    /// Register a `StatusNotifierItem` client.
    ///
    /// `service` may be either a bus name or an object path starting with `/`.
    async fn register_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        let sender = header
            .sender()
            .map_or("", zbus::names::UniqueName::as_str)
            .to_string();
        let (bus_name, object_path) = parse::parse_service(service, &sender);

        tracing::debug!(bus_name, object_path, "RegisterStatusNotifierItem");

        // Initial property read, retried once after a grace delay (#1559).
        match read_item_props_with_retry(&bus_name, &object_path).await {
            Ok(item) => {
                let key = item.key.clone();
                self.state.registered.lock().await.insert(key.clone(), item);
                self.state.rebuild_published_list().await;

                let _ = Self::status_notifier_item_registered(
                    &emitter,
                    format!("{bus_name}{object_path}"),
                )
                .await;

                // Spawn a per-item watcher using bus::proxy for PeerGone detection
                // and bus::signals for property-update signals. Supervised:
                // `watch_item` re-reads + parses this item's (untrusted) tooltip
                // / icon-pixmap / menu properties, so it's the real panic
                // surface. Bounded (#1174): a clean completion means the item
                // disconnected, which is this task finishing its job — no
                // restart, and none of the `warn!`-plus-kept-row that
                // `spawn_supervised` gives an unexpected return.
                let state = self.state.clone();
                spawn_supervised_bounded("tray-item", move || {
                    let state = state.clone();
                    let bus_name = bus_name.clone();
                    let object_path = object_path.clone();
                    async move {
                        watch_item(state, bus_name, object_path).await;
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, bus_name, object_path, "initial property read failed");
            }
        }
    }

    /// No-op: we are the host.
    async fn register_status_notifier_host(&self, service: &str) {
        let _ = service;
    }

    /// List of all currently registered items.
    #[zbus(property)]
    async fn registered_status_notifier_items(&self) -> Vec<String> {
        self.state
            .registered
            .lock()
            .await
            .values()
            .map(|i| format!("{}{}", i.bus_name, i.object_path))
            .collect()
    }

    /// Always `true` — we are acting as the host.
    #[zbus(property)]
    #[allow(clippy::unused_self)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    /// Protocol version 0.
    #[zbus(property)]
    #[allow(clippy::unused_self)]
    fn protocol_version(&self) -> i32 {
        0
    }

    /// Emitted when a new `StatusNotifierItem` is registered.
    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &SignalEmitter<'_>,
        service: String,
    ) -> zbus::Result<()>;

    /// Emitted when a `StatusNotifierItem` is unregistered.
    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &SignalEmitter<'_>,
        service: String,
    ) -> zbus::Result<()>;
}

// ── Per-item watcher task ─────────────────────────────────────────────────────

async fn watch_item(state: State, bus_name: String, object_path: String) {
    // Build a long-lived proxy for this item. The proxy monitors
    // NameOwnerChanged for this exact bus name, giving us PeerGone when the
    // item app exits.
    let item_proxy = match proxy(BusKind::Session, bus_name.as_str())
        .at_path(object_path.clone())
        .iface(SNI_IFACE)
        .build()
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, bus_name, object_path, "failed to build item proxy");
            return;
        }
    };

    // Subscribe to the four update signals via bus::signals.
    //
    // Each forwarder is a named, supervised task rather than a bare
    // `tokio::spawn` (#1172): an unsupervised child of `watch_item` (itself
    // supervised, `spawn_supervised_bounded("tray-item", …)` above) — a panic
    // in one silently killed just that forwarder while `watch_item` stayed
    // healthy in the supervisor's eyes, freezing that one property (icon,
    // title, status or tooltip) for the item's whole session with nothing
    // surfacing it. `Bounded`, mirroring `tray-item` itself: the stream
    // ending (item disconnected) is this forwarder's normal, expected
    // completion, not a return worth a `warn!`. The subscription is
    // (re-)established *inside* the factory so a restart after a panic gets
    // a fresh one rather than reusing whatever the first, now-dead run built.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(4);

    for (sig_name, task_name) in [
        ("NewIcon", "tray-item-signal-new-icon"),
        ("NewTitle", "tray-item-signal-new-title"),
        ("NewStatus", "tray-item-signal-new-status"),
        ("NewToolTip", "tray-item-signal-new-tooltip"),
    ] {
        let tx2 = tx.clone();
        let bus2 = bus_name.clone();
        let path2 = object_path.clone();
        let sig = sig_name.to_string();
        spawn_supervised_bounded(task_name, move || {
            let tx2 = tx2.clone();
            let bus2 = bus2.clone();
            let path2 = path2.clone();
            let sig = sig.clone();
            async move {
                let sub = signals(BusKind::Session, bus2.as_str())
                    .at_path(path2)
                    .iface(SNI_IFACE)
                    .signal(sig.clone())
                    .start();
                let mut events = sub.events();
                while events.next().await.is_some() {
                    if tx2.send(()).await.is_err() {
                        break;
                    }
                }
                tracing::debug!(signal = sig, bus_name = bus2, "signal stream ended");
            }
        });
    }
    // Drop this function's own `Sender`, so only the forwarders' clones keep
    // the channel open.
    //
    // That used to be the whole story: each of the four clones lived in the
    // body of a bare `tokio::spawn`, so a forwarder dying dropped its sender,
    // and once all four were gone `rx.recv()` below returned `None` and fell
    // through to the stream-ended cleanup that unregisters the item. Since
    // #1172 the clones live in the **factories** above, which the supervisor
    // holds for the life of the supervised task — so a run that returns (or
    // panics) releases only its own per-run clone, and a permanently-panicking
    // forwarder holds its sender across every backoff. A forwarder dying no
    // longer unregisters the item. That is the better behaviour and is why the
    // code is unchanged: a crashed forwarder now shows up as a `health` row
    // and is restarted with backoff, instead of an item silently vanishing
    // from the tray.
    //
    // So the only route to `None` here is the designed one: all four signal
    // streams *ending* because the item disconnected. A `Bounded` return ends
    // the supervisor itself, which drops the factory and with it that clone —
    // so the last of the four to return closes the channel. (The liveness
    // watcher observes the same disconnect as `PeerGone` and unregisters
    // directly, without going through this channel at all.)
    drop(tx);

    // Liveness watcher for PeerGone — same rationale as the signal
    // forwarders above: a panic here used to silently stop PeerGone
    // detection for this item, leaving a disconnected item's row in the
    // published list forever with no observable cause.
    //
    // `PeerGone` alone cannot see a peer that left *before* this proxy's
    // `NameOwnerChanged` subscription existed — and that is the
    // register-then-exit race: the item is inserted after the session-wide
    // NOC watcher has already pruned (nothing, yet) for that name, and this
    // proxy subscribes after the name is gone, so no emission is left for
    // either to see. `watch_liveness` closes it by asking the bus whether the
    // name still has an owner each time the proxy reports `Live`, which
    // `hytte_bus` only does once that subscription is active.
    {
        let state2 = state.clone();
        let bus2 = bus_name.clone();
        let path2 = object_path.clone();
        let proxy2 = item_proxy.clone();
        spawn_supervised_bounded("tray-item-liveness", move || {
            let state2 = state2.clone();
            let bus2 = bus2.clone();
            let path2 = path2.clone();
            let proxy2 = proxy2.clone();
            async move {
                let end =
                    watch_liveness(proxy2.liveness().to_stream(), || name_has_owner(&bus2)).await;
                if end == LivenessEnd::Gone {
                    tracing::debug!(bus_name = bus2, object_path = path2, "item peer gone");
                    let key = format!("{bus2}{path2}");
                    state2.registered.lock().await.remove(&key);
                    state2.rebuild_published_list().await;
                    state2.emit_unregistered(key).await;
                }
            }
        });
    }

    // Process update notifications.
    while rx.recv().await.is_some() {
        let still_alive = state.refresh_item(&bus_name, &object_path).await;
        state.rebuild_published_list().await;
        if !still_alive {
            let key = format!("{bus_name}{object_path}");
            tracing::debug!(key, "item disappeared, unregistering");
            state.emit_unregistered(key).await;
            return;
        }
    }

    // All signal streams ended (item disconnected).
    let key = format!("{bus_name}{object_path}");
    state.registered.lock().await.remove(&key);
    state.rebuild_published_list().await;
    state.emit_unregistered(key).await;
}

/// Why [`watch_liveness`] returned.
#[derive(Debug, PartialEq, Eq)]
enum LivenessEnd {
    /// The item's bus name has no owner: unregister the row.
    Gone,
    /// The liveness stream ended (every proxy handle dropped) without the peer
    /// leaving. Nothing to remove.
    StreamEnded,
}

/// Fold an item proxy's liveness stream into "is the peer gone".
///
/// Gone is either a [`ProxyState::PeerGone`] — the peer left while the
/// proxy's `NameOwnerChanged` subscription was up — or `still_owned`
/// answering `Some(false)` on a [`ProxyState::Live`]. `hytte_bus` reports
/// `Live` only once that subscription is active, so asking then covers exactly
/// the window the subscription cannot: a name that left before it existed (the
/// register-then-exit race), or during a bus reconnect, when the old
/// subscription is down and the new one has not been made. Any departure after
/// the question is answered arrives as `PeerGone`. `None` — the bus could not
/// answer — keeps the row; the next `Live` asks again.
///
/// `still_owned` is injected so the fold is testable without a bus.
async fn watch_liveness<S, Owned, Fut>(mut liveness: S, mut still_owned: Owned) -> LivenessEnd
where
    S: futures_util::Stream<Item = ProxyState> + Unpin,
    Owned: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<bool>>,
{
    while let Some(state) = liveness.next().await {
        match state {
            ProxyState::PeerGone => return LivenessEnd::Gone,
            ProxyState::Live => {
                if still_owned().await == Some(false) {
                    return LivenessEnd::Gone;
                }
            }
            ProxyState::Reconnecting => {}
        }
    }
    LivenessEnd::StreamEnded
}

/// `org.freedesktop.DBus.NameHasOwner(bus_name)`, or `None` if the bus did
/// not answer.
///
/// `NameHasOwner` rather than `GetNameOwner`: the answer we need is the
/// boolean, and `GetNameOwner` spells "no owner" as an error reply
/// (`NameHasNoOwner`) that would have to be told apart from a failed call by
/// its error name.
async fn name_has_owner(bus_name: &str) -> Option<bool> {
    match call(BusKind::Session, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("NameHasOwner")
        .args((bus_name.to_string(),))
        .send::<bool>()
        .await
    {
        Ok(owned) => Some(owned),
        Err(e) => {
            tracing::debug!(error = %e, bus_name, "tray: NameHasOwner failed; keeping the item");
            None
        }
    }
}

// ── NameOwnerChanged watcher ──────────────────────────────────────────────────

/// Sweep every registered item that is no longer there, using a fresh
/// `ListNames` plus a re-read of each survivor as the source of truth — the
/// repair for a `Resubscribed`/`Lagged` marker on the `NameOwnerChanged`
/// subscription (#1201). Before that fix a missed release (the emission that
/// would have called [`State::unregister_by_bus_name`]) left a dead tray icon
/// forever, since nothing else in this fold ever re-checks a bus name once
/// registered.
///
/// Two levels of "no longer there", because `ListNames` only answers the first
/// one (#1201 review L7):
///
/// * the **bus name** is gone — the item's process exited, or released the
///   name. `ListNames` says so.
/// * the **object** is gone while the process lives on — an app that dropped
///   its `StatusNotifierItem` during the gap. No `ListNames` answer covers
///   that, `PeerGone` never fires (the peer is alive), and [`watch_item`]'s
///   `still_alive` re-read only runs when one of its four signals fires — and
///   for an object that is already gone there is no next signal. So the row
///   was immortal. Re-reading each survivor's properties settles it: the read
///   fails and [`State::refresh_item`] drops the row, exactly as it does on
///   the `watch_item` path.
///
/// That re-read is not only a liveness probe: it also repairs whatever
/// `NewIcon`/`NewTitle`/`NewStatus`/`NewToolTip` was lost while the
/// subscription was down, which is the other half of the same hole.
///
/// `refresh` is injectable so a test can stand in for the per-item round trip.
async fn prune_dead_bus_names(state: &State) {
    let live: Vec<String> = match call(BusKind::Session, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("ListNames")
        .args(())
        .send()
        .await
    {
        Ok(names) => names,
        Err(e) => {
            tracing::warn!(error = %e, "tray: ListNames failed during re-sync; skipping this pass");
            return;
        }
    };

    resync_items(state, &live, |bus_name, object_path| async move {
        state.refresh_item(&bus_name, &object_path).await
    })
    .await;
}

/// The body of [`prune_dead_bus_names`] once `ListNames` has answered: drop
/// every item whose bus name is absent from `live`, then re-read each survivor
/// through `still_alive` and drop the ones that no longer answer.
async fn resync_items<Alive, Fut>(state: &State, live: &[String], mut still_alive: Alive)
where
    Alive: FnMut(String, String) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    // Level one: the bus name is gone.
    let mut gone: Vec<String> = Vec::new();
    {
        let mut map = state.registered.lock().await;
        map.retain(|key, item| {
            let keep = live.contains(&item.bus_name);
            if !keep {
                gone.push(key.clone());
            }
            keep
        });
    }

    // Level two: the bus name is live but the object may not be. Re-read each
    // survivor; `refresh_item` removes the ones whose read fails.
    let survivors: Vec<(String, String, String)> = state
        .registered
        .lock()
        .await
        .values()
        .map(|item| {
            (
                item.key.clone(),
                item.bus_name.clone(),
                item.object_path.clone(),
            )
        })
        .collect();
    for (key, bus_name, object_path) in survivors {
        if !still_alive(bus_name, object_path).await {
            // Remove it here rather than leaning on `refresh_item`'s own
            // removal: `still_alive` is a predicate to this function, and a
            // predicate whose answer only counts if it also mutated the map
            // would be a trap for the next caller (and for the tests).
            state.registered.lock().await.remove(&key);
            gone.push(key);
        }
    }

    if gone.is_empty() {
        return;
    }

    tracing::debug!(removed = gone.len(), "tray: pruned dead items on re-sync");
    state.rebuild_published_list().await;
    // Tell the rest of the session what we just dropped: we are the watcher,
    // and `watch_item`'s own removal paths emit this too.
    for key in gone {
        state.emit_unregistered(key).await;
    }
}

/// Drive the `NameOwnerChanged` items stream: prune a released bus name for
/// each ordinary emission (byte-identical to before #1201), and call
/// `resync` once per [`SignalItem::Resubscribed`]/[`SignalItem::Lagged`]
/// marker instead of dropping it — before #1201 this subscription was read
/// via `events()`, which cannot represent either marker, so a missed release
/// during a bus blip left a dead tray icon for the rest of the session (the
/// one repair, [`prune_dead_bus_names`], relies entirely on this loop
/// calling it). `resync` is injectable so a test can stand in for the real
/// `ListNames` round trip with a counter.
async fn run_owner_change_loop<S, Resync, Fut>(state: &State, mut items: S, mut resync: Resync)
where
    S: futures_util::Stream<Item = SignalItem> + Unpin,
    Resync: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    while let Some(item) = items.next().await {
        match item {
            SignalItem::Resubscribed | SignalItem::Lagged { .. } => {
                tracing::info!("tray NameOwnerChanged resubscribed; re-syncing owned bus names");
                resync().await;
            }
            SignalItem::Event(event) => {
                let Ok((name, _old_owner, new_owner)) =
                    event.body.body().deserialize::<(String, String, String)>()
                else {
                    tracing::debug!("NameOwnerChanged parse error");
                    continue;
                };

                // `new_owner` is empty → the bus name was released.
                if new_owner.is_empty() {
                    tracing::debug!(name, "bus name released, pruning tray items");
                    state.unregister_by_bus_name(&name).await;
                }
            }
        }
    }
}

/// Subscribe to `NameOwnerChanged` on the session bus and prune items when
/// their bus name is released.
async fn watch_name_owner_changes(state: &State) -> Result<()> {
    let owner_changes = signals(BusKind::Session, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .signal("NameOwnerChanged")
        .start();

    run_owner_change_loop(state, owner_changes.items(), || prune_dead_bus_names(state)).await;

    Ok(())
}

// ── Helper: read item properties via bus::call ────────────────────────────────

/// Read a single D-Bus property from a `StatusNotifierItem`.
async fn get_sni_property<T>(bus_name: &str, object_path: &str, prop: &'static str) -> Option<T>
where
    T: serde::de::DeserializeOwned + zbus::zvariant::Type + 'static,
{
    call(BusKind::Session, bus_name)
        .at_path(object_path)
        .iface("org.freedesktop.DBus.Properties")
        .method("Get")
        .args((SNI_IFACE, prop))
        .send::<T>()
        .await
        .ok()
}

/// The four `StatusNotifierItem` properties that say *something is there*:
/// each `None` is a `Get` that did not answer, as opposed to `Some("")`, a
/// `Get` that answered with an empty string.
///
/// The spec makes `Id` and `Status` mandatory, so a live item answers at least
/// those two. `Title` and `IconName` are included so an item that is merely
/// sloppy about one of the mandatory pair is still recognised.
#[derive(Debug, Default)]
struct Identity {
    title: Option<String>,
    id: Option<String>,
    status: Option<String>,
    icon_name: Option<String>,
}

impl Identity {
    async fn read(bus_name: &str, object_path: &str) -> Self {
        Self {
            title: get_sni_property(bus_name, object_path, "Title").await,
            id: get_sni_property(bus_name, object_path, "Id").await,
            status: get_sni_property(bus_name, object_path, "Status").await,
            icon_name: get_sni_property(bus_name, object_path, "IconName").await,
        }
    }

    /// Whether any of the four answered. `false` is the read that has to fail:
    /// a peer that has left the bus (`ServiceUnknown`) or an object that is no
    /// longer exported (`UnknownObject`/`UnknownMethod`) answers none of them.
    fn answered(&self) -> bool {
        self.title.is_some()
            || self.id.is_some()
            || self.status.is_some()
            || self.icon_name.is_some()
    }
}

/// How long to wait before the one retry of a failed initial read.
///
/// The retry is for an item whose properties do not answer yet when it
/// registers: an app that calls `RegisterStatusNotifierItem` before its object
/// is exported, or a read that failed in passing. Not every tray library can
/// cause the first: libayatana-appindicator's `check_connect` exports the
/// object with `g_dbus_connection_register_object` before it registers, and
/// its comment there says the order matters. For an app that does register
/// first, 400 ms is time to finish exporting, and it is short enough that a
/// row appearing that much later goes unnoticed. The read runs inside the
/// `RegisterStatusNotifierItem` handler, so the reply waits as well;
/// libayatana-appindicator makes that call with `GDBus`'s default timeout
/// (25 s), well clear of it.
const INITIAL_READ_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

/// What to do after one attempt at an item's initial property read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitialRead {
    /// The read worked: publish the row.
    Accept,
    /// It failed but the item may still be on its way: wait and read again.
    Retry,
    /// It failed for good: no row.
    Drop,
}

/// The decision for one attempt.
///
/// - a successful read is always accepted;
/// - a failed read whose peer is **known gone** (`peer_alive == Some(false)`)
///   is dropped at once, since the retry could only fail again. That only
///   skips the wait: a failed read never makes a row either way. What stops a
///   peer that leaves after a *successful* read from leaving a ghost row is
///   [`watch_liveness`]'s owner check on the item's first `Live` (#1494);
/// - otherwise (peer alive, or the owner check itself unanswered, which is
///   treated as alive like the rest of this module) the first failure is
///   retried and the second is dropped.
fn decide_initial_read(read_ok: bool, is_retry: bool, peer_alive: Option<bool>) -> InitialRead {
    if read_ok {
        InitialRead::Accept
    } else if peer_alive == Some(false) || is_retry {
        InitialRead::Drop
    } else {
        InitialRead::Retry
    }
}

/// Drive [`decide_initial_read`] over a reader and an owner check, so the
/// loop is testable without a bus.
async fn read_with_retry<T, E, R, RFut, P, PFut>(
    mut read: R,
    mut peer_alive: P,
    delay: std::time::Duration,
) -> Result<T, E>
where
    R: FnMut() -> RFut,
    RFut: std::future::Future<Output = Result<T, E>>,
    P: FnMut() -> PFut,
    PFut: std::future::Future<Output = Option<bool>>,
{
    let mut is_retry = false;
    loop {
        match read().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let alive = peer_alive().await;
                match decide_initial_read(false, is_retry, alive) {
                    InitialRead::Retry => {
                        is_retry = true;
                        tokio::time::sleep(delay).await;
                    }
                    InitialRead::Drop | InitialRead::Accept => return Err(e),
                }
            }
        }
    }
}

/// [`read_item_props`] with the one grace retry of [`INITIAL_READ_RETRY_DELAY`].
async fn read_item_props_with_retry(bus_name: &str, object_path: &str) -> Result<TrayItem> {
    read_with_retry(
        || read_item_props(bus_name, object_path),
        || name_has_owner(bus_name),
        INITIAL_READ_RETRY_DELAY,
    )
    .await
}

/// Read every property a [`TrayItem`] carries.
///
/// # Errors
///
/// Fails when the item answered **none** of its identifying properties (see
/// [`Identity::answered`]). Every individual read is still best-effort — a
/// missing `ToolTip` or `IconPixmap` is an ordinary item — but before #1201's
/// follow-up this function could not fail at all, since every read ended in
/// `.ok()`/`unwrap_or_default()`. That made three things dead code:
/// registering an item whose peer had already left published an empty row,
/// [`watch_item`]'s `still_alive` re-read never said no, and the re-sync's
/// object-level re-read (`resync_items`, #1201 L7) always answered "alive" in
/// production, so the row it was written to prune stayed.
async fn read_item_props(bus_name: &str, object_path: &str) -> Result<TrayItem> {
    let identity = Identity::read(bus_name, object_path).await;
    if !identity.answered() {
        anyhow::bail!("item answered none of Title/Id/Status/IconName: its peer or object is gone");
    }
    let Identity {
        title,
        status,
        icon_name,
        ..
    } = identity;
    let title = title.unwrap_or_default();
    let icon_name = icon_name.unwrap_or_default();
    let status_str = status.unwrap_or_default();

    // IconPixmap: a(iiay) — always read; the widget layer decides whether to use
    // it (e.g. when `icon_name` is non-empty but not present in the icon theme).
    let icon_pixmap = read_icon_pixmap(bus_name, object_path).await;

    // Tooltip: (s, a(iiay), s, s) — (icon_name, icon_pixmap, title, description).
    let (tooltip_title, tooltip_description) = read_tooltip(bus_name, object_path).await;

    // Menu object path.
    let menu_path = read_menu_path(bus_name, object_path).await;

    // `ItemIsMenu` defaults to false when the property is absent.
    let item_is_menu: bool = get_sni_property(bus_name, object_path, "ItemIsMenu")
        .await
        .unwrap_or(false);

    Ok(TrayItem {
        key: format!("{bus_name}{object_path}"),
        bus_name: bus_name.to_string(),
        object_path: object_path.to_string(),
        title,
        icon_name,
        status: ItemStatus::from_str(&status_str),
        icon_pixmap,
        tooltip_title,
        tooltip_description,
        menu_path,
        item_is_menu,
    })
}

/// Upper bound on an SNI icon dimension — a sanity guard so a bogus giant
/// `IconPixmap` can't allocate the world. `32768` is well above any real tray
/// icon while keeping `width * height * 4` comfortably inside `i64`.
const MAX_ICON_DIM: i32 = 1 << 15;

/// Whether an SNI `IconPixmap` entry is a well-formed ARGB32 bitmap: positive,
/// sanely-bounded dimensions and **exactly** `width * height * 4` bytes.
///
/// `IconPixmap` is an arbitrary session-bus payload, so an entry whose declared
/// dimensions don't match its buffer length — or whose negative width would wrap
/// the widget's `w as usize * 4` stride into a huge value — must be rejected
/// before it reaches `gdk::MemoryTexture::new`, which would otherwise fail its
/// size check and panic the GTK main thread. `i64` math so no product overflows.
fn icon_pixmap_consistent(width: i32, height: i32, data_len: usize) -> bool {
    if width <= 0 || height <= 0 || width > MAX_ICON_DIM || height > MAX_ICON_DIM {
        return false;
    }
    let expected = i64::from(width) * i64::from(height) * 4;
    i64::try_from(data_len).is_ok_and(|len| len == expected)
}

/// Read `IconPixmap` and return the largest well-formed entry as `(w, h, bytes)`.
///
/// Entries whose geometry doesn't match their buffer (see
/// [`icon_pixmap_consistent`]) are skipped, so the widget only ever builds a
/// texture from a validated ARGB32 buffer and falls back to the icon-name path
/// when no entry is usable.
async fn read_icon_pixmap(bus_name: &str, object_path: &str) -> Option<(i32, i32, Vec<u8>)> {
    // Type: a(iiay)
    let raw: OwnedValue = get_sni_property(bus_name, object_path, "IconPixmap").await?;
    let arr = zbus::zvariant::Array::try_from(raw).ok()?;

    let mut best: Option<(i32, i32, Vec<u8>)> = None;
    let mut best_area = 0i64;

    for entry in arr.iter() {
        let cloned = entry.try_clone().ok()?;
        let owned = OwnedValue::try_from(cloned).ok()?;
        if let Ok(s) = Structure::try_from(owned) {
            let mut fields = s.into_fields();
            if fields.len() < 3 {
                continue;
            }
            let Ok(w) = i32::try_from(fields.remove(0)) else {
                continue;
            };
            let Ok(h) = i32::try_from(fields.remove(0)) else {
                continue;
            };
            let bytes_val = fields.remove(0);
            let Ok(bytes_arr) =
                zbus::zvariant::Array::try_from(OwnedValue::try_from(bytes_val).ok()?)
            else {
                continue;
            };
            let bytes: Vec<u8> = bytes_arr
                .iter()
                .filter_map(|v| u8::try_from(v.try_clone().ok()?).ok())
                .collect();
            if !icon_pixmap_consistent(w, h, bytes.len()) {
                tracing::warn!(
                    width = w,
                    height = h,
                    data_len = bytes.len(),
                    bus_name,
                    object_path,
                    "skipping malformed SNI IconPixmap entry (geometry / length mismatch)"
                );
                continue;
            }
            let area = i64::from(w) * i64::from(h);
            if area > best_area {
                best_area = area;
                best = Some((w, h, bytes));
            }
        }
    }

    best
}

/// Read `Tooltip` property and extract `(title, description)`.
async fn read_tooltip(bus_name: &str, object_path: &str) -> (String, String) {
    // Type: (s, a(iiay), s, s)
    let raw: OwnedValue = match get_sni_property(bus_name, object_path, "ToolTip").await {
        Some(v) => v,
        None => return (String::new(), String::new()),
    };

    let Ok(s) = Structure::try_from(raw) else {
        return (String::new(), String::new());
    };

    let mut fields = s.into_fields();
    if fields.len() < 4 {
        return (String::new(), String::new());
    }

    // fields[0] = icon_name (s) — skip
    // fields[1] = icon_pixmap (a(iiay)) — skip
    // fields[2] = title (s)
    // fields[3] = description (s)
    let _icon_name_val = fields.remove(0);
    let _icon_pixmap_val = fields.remove(0);
    let title = String::try_from(fields.remove(0)).unwrap_or_default();
    let description = String::try_from(fields.remove(0)).unwrap_or_default();

    (title, description)
}

/// Read `Menu` property and return the object path as a `String`, or `None`.
async fn read_menu_path(bus_name: &str, object_path: &str) -> Option<String> {
    let raw: OwnedValue = get_sni_property(bus_name, object_path, "Menu").await?;
    // The Menu property is an object path (o).
    let path = zbus::zvariant::OwnedObjectPath::try_from(raw).ok()?;
    let s = path.as_str().to_string();
    if s.is_empty() || s == "/" {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::icon_pixmap_consistent;

    #[test]
    fn icon_pixmap_accepts_exact_argb32() {
        // 16×16 ARGB32 = 16*16*4 = 1024 bytes.
        assert!(icon_pixmap_consistent(16, 16, 1024));
        // 1×1.
        assert!(icon_pixmap_consistent(1, 1, 4));
    }

    #[test]
    fn icon_pixmap_rejects_length_mismatch() {
        // One byte short of 16×16×4, and one byte over — both rejected (the SNI
        // spec is exact-length, unlike the padded notification path).
        assert!(!icon_pixmap_consistent(16, 16, 1023));
        assert!(!icon_pixmap_consistent(16, 16, 1025));
        // A 22×22 header over a 16×16 buffer: classic `height*stride > len`.
        assert!(!icon_pixmap_consistent(22, 22, 1024));
    }

    #[test]
    fn icon_pixmap_rejects_bad_dimensions() {
        assert!(!icon_pixmap_consistent(0, 16, 0)); // zero width
        assert!(!icon_pixmap_consistent(16, 0, 0)); // zero height
        // Negative width: the widget's `w as usize * 4` would wrap to a huge
        // stride — must be rejected here.
        assert!(!icon_pixmap_consistent(-16, 16, 1024));
        assert!(!icon_pixmap_consistent(16, -16, 1024));
    }

    #[test]
    fn icon_pixmap_rejects_absurd_dimensions_without_overflow() {
        // Bogus giant dimensions must be rejected by the bound, never overflow.
        assert!(!icon_pixmap_consistent(i32::MAX, i32::MAX, 4));
        assert!(!icon_pixmap_consistent(1 << 20, 1 << 20, 16));
    }

    // ── #1201: NameOwnerChanged re-syncs on Resubscribed/Lagged ─────────────

    use super::{Mutable, SignalItem, State, run_owner_change_loop};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, OnceLock};
    use tokio::sync::Mutex as AsyncMutex;

    fn empty_state() -> State {
        State {
            items: Mutable::new(Vec::new()),
            registered: Arc::new(AsyncMutex::new(HashMap::new())),
            ownership: Arc::new(OnceLock::new()),
        }
    }

    /// Before #1201 this loop was driven by `events()`, which cannot
    /// represent either marker, so a missed release during a bus blip left a
    /// dead tray icon for the rest of the session (nothing else in this fold
    /// ever re-checks a registered bus name). Pushing exactly one marker
    /// through `run_owner_change_loop` must call `resync` exactly once — for
    /// both `Resubscribed` and a broadcast `Lagged`.
    ///
    /// Falsifiable: deleting the `Resubscribed | Lagged { .. }` arm (or
    /// making it a no-op) drops both counts to 0.
    #[tokio::test(flavor = "current_thread")]
    async fn owner_change_loop_resyncs_exactly_once_per_marker() {
        let state = empty_state();

        let calls = Arc::new(AtomicUsize::new(0));
        {
            let calls = calls.clone();
            let items = futures_util::stream::iter(vec![SignalItem::Resubscribed]);
            run_owner_change_loop(&state, items, move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await;
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Resubscribed must re-sync exactly once"
        );

        let calls = Arc::new(AtomicUsize::new(0));
        {
            let calls = calls.clone();
            let items = futures_util::stream::iter(vec![SignalItem::Lagged { skipped: 5 }]);
            run_owner_change_loop(&state, items, move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                }
            })
            .await;
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Lagged must re-sync exactly once"
        );
    }

    // ── #1201 review L7: the prune covers object-level death too ────────────

    use super::{ItemStatus, TrayItem, resync_items};

    async fn register(state: &State, bus_name: &str, object_path: &str) -> String {
        let key = format!("{bus_name}{object_path}");
        state.registered.lock().await.insert(
            key.clone(),
            TrayItem {
                key: key.clone(),
                bus_name: bus_name.to_string(),
                object_path: object_path.to_string(),
                title: String::new(),
                icon_name: String::new(),
                status: ItemStatus::Active,
                icon_pixmap: None,
                tooltip_title: String::new(),
                tooltip_description: String::new(),
                menu_path: None,
                item_is_menu: false,
            },
        );
        state.rebuild_published_list().await;
        key
    }

    /// An item whose process is still alive but whose `StatusNotifierItem`
    /// object went away during the gap was immortal: `ListNames` still lists
    /// the bus name, `PeerGone` never fires for a live peer, and `watch_item`'s
    /// own `still_alive` re-read only runs when one of its four signals fires —
    /// and a gone object sends no next signal. The re-sync must re-read each
    /// survivor and drop the ones that no longer answer.
    ///
    /// Falsifiable: deleting the survivor loop from `resync_items` leaves the
    /// dead object published.
    #[tokio::test(flavor = "current_thread")]
    async fn resync_drops_an_item_whose_object_is_gone_but_bus_name_lives() {
        let state = empty_state();
        let alive_key = register(&state, ":1.7", "/StatusNotifierItem").await;
        let dead_key = register(&state, ":1.9", "/StatusNotifierItem").await;
        assert_eq!(state.items.get_cloned().len(), 2);

        // Both bus names are still on the bus; only one object still answers.
        let live = vec![":1.7".to_string(), ":1.9".to_string()];
        resync_items(&state, &live, |bus_name, _path| async move {
            bus_name == ":1.7"
        })
        .await;

        let keys: Vec<String> = state
            .items
            .get_cloned()
            .into_iter()
            .map(|i| i.key)
            .collect();
        assert_eq!(
            keys,
            vec![alive_key],
            "the item whose object stopped answering must be pruned"
        );
        assert!(!state.registered.lock().await.contains_key(&dead_key));
    }

    /// The bus-name level still works, and a dead bus name is not re-read —
    /// asking a name that is gone is a round trip that can only time out.
    #[tokio::test(flavor = "current_thread")]
    async fn resync_drops_a_dead_bus_name_without_re_reading_it() {
        let state = empty_state();
        let alive_key = register(&state, ":1.7", "/StatusNotifierItem").await;
        register(&state, ":1.9", "/StatusNotifierItem").await;

        let asked = Arc::new(AtomicUsize::new(0));
        let asked2 = asked.clone();
        let live = vec![":1.7".to_string()];
        resync_items(&state, &live, move |_bus, _path| {
            let asked = asked2.clone();
            async move {
                asked.fetch_add(1, Ordering::SeqCst);
                true
            }
        })
        .await;

        let keys: Vec<String> = state
            .items
            .get_cloned()
            .into_iter()
            .map(|i| i.key)
            .collect();
        assert_eq!(keys, vec![alive_key]);
        assert_eq!(
            asked.load(Ordering::SeqCst),
            1,
            "only the survivor is re-read"
        );
    }

    // ── #1559: the initial read retries once ─────────────────────────────────

    use super::{InitialRead, decide_initial_read, read_with_retry};

    #[test]
    fn initial_read_decision_covers_every_arm() {
        use InitialRead::{Accept, Drop, Retry};
        // success is accepted whatever else is true
        for retry in [false, true] {
            for peer in [None, Some(true), Some(false)] {
                assert_eq!(decide_initial_read(true, retry, peer), Accept);
            }
        }
        // first failure, peer alive or unknown: retry
        assert_eq!(decide_initial_read(false, false, Some(true)), Retry);
        assert_eq!(decide_initial_read(false, false, None), Retry);
        // first failure, peer gone: drop without waiting
        assert_eq!(decide_initial_read(false, false, Some(false)), Drop);
        // second failure: drop
        assert_eq!(decide_initial_read(false, true, Some(true)), Drop);
        assert_eq!(decide_initial_read(false, true, None), Drop);
        assert_eq!(decide_initial_read(false, true, Some(false)), Drop);
    }

    /// Run `read_with_retry` over scripted read results and one owner answer,
    /// returning the outcome and (reads, owner checks) made.
    async fn drive(
        reads: Vec<Result<u8, &'static str>>,
        peer: Option<bool>,
    ) -> (Result<u8, &'static str>, usize, usize) {
        let mut reads: VecDeque<_> = reads.into();
        let n_read = Arc::new(AtomicUsize::new(0));
        let n_peer = Arc::new(AtomicUsize::new(0));
        let (r, p) = (n_read.clone(), n_peer.clone());
        let out = read_with_retry(
            move || {
                r.fetch_add(1, Ordering::SeqCst);
                std::future::ready(reads.pop_front().unwrap_or(Err("exhausted")))
            },
            move || {
                p.fetch_add(1, Ordering::SeqCst);
                std::future::ready(peer)
            },
            std::time::Duration::ZERO,
        )
        .await;
        (
            out,
            n_read.load(Ordering::SeqCst),
            n_peer.load(Ordering::SeqCst),
        )
    }

    /// An item that registers before exporting: first read fails, second works.
    /// Falsifiable: with no retry this is `Err` after one read.
    #[tokio::test(flavor = "current_thread")]
    async fn a_late_exporting_item_appears_on_the_retry() {
        let (out, reads, _) = drive(vec![Err("early"), Ok(7)], Some(true)).await;
        assert_eq!(out, Ok(7));
        assert_eq!(reads, 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_read_that_works_first_time_asks_nothing_else() {
        let (out, reads, peers) = drive(vec![Ok(1)], Some(true)).await;
        assert_eq!((out, reads, peers), (Ok(1), 1, 0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_second_failure_drops_the_item_after_exactly_one_retry() {
        let (out, reads, _) = drive(vec![Err("a"), Err("b"), Ok(9)], Some(true)).await;
        assert_eq!(out, Err("b"));
        assert_eq!(reads, 2);
    }

    /// A peer that already left is not waited for: the retry could only fail.
    #[tokio::test(flavor = "current_thread")]
    async fn a_gone_peer_is_dropped_without_a_retry() {
        let (out, reads, _) = drive(vec![Err("gone"), Ok(9)], Some(false)).await;
        assert_eq!(out, Err("gone"));
        assert_eq!(reads, 1);
    }

    /// The retry waits the grace delay before it reads again. That delay is
    /// the whole point of the retry: an item that has not exported yet fails
    /// a second read made at once just as it failed the first. `drive` passes
    /// a zero delay, so nothing else here sees the wait (#1574 review).
    ///
    /// The paused clock is never stepped by hand: `read_with_retry`'s own
    /// `sleep` auto-advances it, and nothing runs under `spawn_blocking`, so
    /// the gap measured is exactly the delay slept, in debug and release.
    ///
    /// Falsifiable: delete the `sleep` in `read_with_retry` and the gap is 0.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn the_retry_reads_again_only_after_the_grace_delay() {
        use super::INITIAL_READ_RETRY_DELAY;
        let start = tokio::time::Instant::now();
        let at = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = at.clone();
        let mut script: VecDeque<Result<u8, &'static str>> = vec![Err("early"), Ok(7)].into();
        let out = read_with_retry(
            move || {
                log.lock().unwrap().push(start.elapsed());
                std::future::ready(script.pop_front().unwrap_or(Err("exhausted")))
            },
            || std::future::ready(Some(true)),
            INITIAL_READ_RETRY_DELAY,
        )
        .await;
        assert_eq!(out, Ok(7));
        let at = at.lock().unwrap();
        assert_eq!(at.len(), 2);
        assert_eq!(at[1].checked_sub(at[0]), Some(INITIAL_READ_RETRY_DELAY));
    }

    // ── Ghost rows: the identity check and the owner check ──────────────────

    use super::{Identity, LivenessEnd, ProxyState, watch_liveness};
    use std::collections::VecDeque;

    /// The read that has to fail. Before this, `read_item_props` could not
    /// fail at all, so an item whose peer had already left registered as an
    /// empty row, and both of the "re-read and drop it if it no longer
    /// answers" paths (`watch_item`'s `still_alive`, `resync_items`' survivor
    /// pass) always answered "alive".
    #[test]
    fn an_item_that_answered_nothing_is_not_an_item() {
        assert!(!Identity::default().answered());
    }

    /// One answer is enough, and an empty string is an answer: a live item
    /// with no title and a pixmap-only icon still says `Id`/`Status`.
    #[test]
    fn any_one_identifying_answer_is_enough_even_an_empty_one() {
        let empty = || Some(String::new());
        assert!(
            Identity {
                id: empty(),
                ..Identity::default()
            }
            .answered()
        );
        assert!(
            Identity {
                status: Some("Passive".to_string()),
                ..Identity::default()
            }
            .answered()
        );
        assert!(
            Identity {
                title: empty(),
                ..Identity::default()
            }
            .answered()
        );
        assert!(
            Identity {
                icon_name: empty(),
                ..Identity::default()
            }
            .answered()
        );
    }

    /// An owner check that answers from a fixed script, counting the asks.
    fn scripted(
        answers: Vec<Option<bool>>,
        asked: &Arc<AtomicUsize>,
    ) -> impl FnMut() -> std::future::Ready<Option<bool>> {
        let mut answers: VecDeque<Option<bool>> = answers.into();
        let asked = asked.clone();
        move || {
            asked.fetch_add(1, Ordering::SeqCst);
            std::future::ready(answers.pop_front().flatten())
        }
    }

    /// The register-then-exit race: the item's peer is gone before its proxy
    /// subscribes to `NameOwnerChanged`, so the proxy goes `Reconnecting` →
    /// `Live` and `PeerGone` never arrives. The owner check at `Live` is what
    /// removes the row.
    ///
    /// Falsifiable: without the check on `Live`, this returns `StreamEnded`
    /// — in production, a row that stays forever.
    #[tokio::test(flavor = "current_thread")]
    async fn a_peer_that_left_before_the_subscription_is_gone_at_live() {
        let asked = Arc::new(AtomicUsize::new(0));
        let states = futures_util::stream::iter([ProxyState::Reconnecting, ProxyState::Live]);
        let end = watch_liveness(states, scripted(vec![Some(false)], &asked)).await;
        assert_eq!(end, LivenessEnd::Gone);
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    /// A peer that is still there keeps its row; a later `PeerGone` removes it
    /// without another question.
    #[tokio::test(flavor = "current_thread")]
    async fn a_live_owner_keeps_the_item_until_peer_gone() {
        let asked = Arc::new(AtomicUsize::new(0));
        let states = futures_util::stream::iter([ProxyState::Live]);
        let end = watch_liveness(states, scripted(vec![Some(true)], &asked)).await;
        assert_eq!(end, LivenessEnd::StreamEnded);

        let asked = Arc::new(AtomicUsize::new(0));
        let states = futures_util::stream::iter([
            ProxyState::Reconnecting,
            ProxyState::Live,
            ProxyState::PeerGone,
        ]);
        let end = watch_liveness(states, scripted(vec![Some(true)], &asked)).await;
        assert_eq!(end, LivenessEnd::Gone);
        assert_eq!(
            asked.load(Ordering::SeqCst),
            1,
            "PeerGone needs no second question"
        );
    }

    /// A check the bus could not answer keeps the row, and the next `Live`
    /// (after a reconnect — the window in which a departure is also missed)
    /// asks again.
    #[tokio::test(flavor = "current_thread")]
    async fn an_unanswered_owner_check_keeps_the_item_and_asks_again_on_the_next_live() {
        let asked = Arc::new(AtomicUsize::new(0));
        let states = futures_util::stream::iter([
            ProxyState::Live,
            ProxyState::Reconnecting,
            ProxyState::Live,
        ]);
        let end = watch_liveness(states, scripted(vec![None, Some(false)], &asked)).await;
        assert_eq!(end, LivenessEnd::Gone);
        assert_eq!(asked.load(Ordering::SeqCst), 2);
    }

    /// A re-sync that finds everything present must not churn the published
    /// list (the widget layer's bind rebuilds on every `set`).
    #[tokio::test(flavor = "current_thread")]
    async fn resync_with_nothing_gone_publishes_nothing() {
        let state = empty_state();
        register(&state, ":1.7", "/StatusNotifierItem").await;
        let before = state.items.get_cloned();

        let live = vec![":1.7".to_string()];
        resync_items(&state, &live, |_bus, _path| async { true }).await;

        let after = state.items.get_cloned();
        assert_eq!(before.len(), after.len());
        assert_eq!(after[0].key, ":1.7/StatusNotifierItem");
    }
}
