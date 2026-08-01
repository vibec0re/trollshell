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
//! the `RegisterStatusNotifierItem` argument) live in [`parse`], which is
//! pure and hermetically unit-tested; the menu data shapes live in [`types`].
//! This file keeps the service, D-Bus, and signal-emit logic.

mod parse;
mod types;

pub use parse::strip_accel;
pub use types::{Menu, MenuEntry, MenuItem, ToggleType};

use anyhow::{Context, Result};
use futures_signals::signal::SignalExt;
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_bus::{BusKind, OwnNameSignal, OwnState, ProxyState, call, proxy, signals};
use hytte_reactive::{Service, registry, runtime, spawn_supervised};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Structure, Value};

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
    /// Kept alive so the `own_name` task continues owning
    /// `org.kde.StatusNotifierWatcher` for the process lifetime.
    _ownership: OwnNameSignal,
    /// Mirror of the `own_name` task's [`OwnState`], published by
    /// [`ownership()`] so a lost name race is visible in the UI rather than
    /// presenting as a permanently empty tray (#747, #653). See
    /// [`crate::notifications::mirror_own_state`] for why this is a mirror and
    /// not the [`OwnNameSignal`] itself.
    own_state: Mutable<OwnState>,
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
        let state2 = state;
        spawn_supervised("tray", move || {
            let state = state2.clone();
            async move {
                loop {
                    match watch_name_owner_changes(&state).await {
                        Ok(()) => {
                            tracing::warn!("tray NOC stream closed, reconnecting in 2s");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "tray NOC watcher error, reconnecting in 2s");
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        });

        TrayHandles {
            items,
            own_state: crate::notifications::mirror_own_state(&ownership),
            _ownership: ownership,
        }
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
            .own_state
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
    /// when the item should be removed (proxy read failed).
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

        // Initial property read.
        match read_item_props(&bus_name, &object_path).await {
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
                // surface; a clean completion (item disconnected) does not
                // restart.
                let state = self.state.clone();
                spawn_supervised("tray-item", move || {
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
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(4);

    for sig_name in ["NewIcon", "NewTitle", "NewStatus", "NewToolTip"] {
        let tx2 = tx.clone();
        let bus2 = bus_name.clone();
        let path2 = object_path.clone();
        let sig = sig_name.to_string();
        let sub = signals(BusKind::Session, bus2.as_str())
            .at_path(path2)
            .iface(SNI_IFACE)
            .signal(sig.clone())
            .start();
        tokio::spawn(async move {
            let mut events = sub.events();
            while events.next().await.is_some() {
                if tx2.send(()).await.is_err() {
                    break;
                }
            }
            tracing::debug!(signal = sig, bus_name = bus2, "signal stream ended");
        });
    }
    drop(tx); // close sender side so channel closes when all sub-tasks end

    // Spawn liveness watcher for PeerGone.
    {
        let state2 = state.clone();
        let bus2 = bus_name.clone();
        let path2 = object_path.clone();
        let proxy2 = item_proxy.clone();
        tokio::spawn(async move {
            let mut liveness = proxy2.liveness().to_stream();
            while let Some(s) = liveness.next().await {
                if s == ProxyState::PeerGone {
                    tracing::debug!(bus_name = bus2, object_path = path2, "item proxy: PeerGone");
                    let key = format!("{bus2}{path2}");
                    state2.registered.lock().await.remove(&key);
                    state2.rebuild_published_list().await;
                    state2.emit_unregistered(key).await;
                    return;
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

// ── NameOwnerChanged watcher ──────────────────────────────────────────────────

/// Subscribe to `NameOwnerChanged` on the session bus and prune items when
/// their bus name is released.
async fn watch_name_owner_changes(state: &State) -> Result<()> {
    let owner_changes = signals(BusKind::Session, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .signal("NameOwnerChanged")
        .start();

    let mut events = owner_changes.events();
    while let Some(event) = events.next().await {
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

async fn read_item_props(bus_name: &str, object_path: &str) -> Result<TrayItem> {
    let title: String = get_sni_property(bus_name, object_path, "Title")
        .await
        .unwrap_or_default();
    let icon_name: String = get_sni_property(bus_name, object_path, "IconName")
        .await
        .unwrap_or_default();
    let status_str: String = get_sni_property(bus_name, object_path, "Status")
        .await
        .unwrap_or_default();

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
}
