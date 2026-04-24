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
//! # v0.3.2 scope
//!
//! `IconName`-only (no `IconPixmap`), no tooltip rendering, no `DBusMenu`,
//! no `ScrollEvent`.

use anyhow::{anyhow, Context, Result};
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_reactive::{registry, runtime, Service};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, fdo};

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
}

/// Service handle returned by [`service()`].
#[doc(hidden)]
pub struct TrayHandles {
    pub(crate) items: Mutable<Vec<TrayItem>>,
}

impl Default for TrayHandles {
    fn default() -> Self {
        Self {
            items: Mutable::new(Vec::new()),
        }
    }
}

// ── Service entry-point ───────────────────────────────────────────────────────

/// The tray service marker type.
pub struct TrayService;

impl Service for TrayService {
    type Handles = TrayHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = TrayHandles::default();
        let items_writer = handles.items.clone();

        rt.spawn(async move {
            loop {
                match listen(&items_writer).await {
                    Ok(()) => {
                        tracing::warn!("tray watcher stream closed, reconnecting in 2s");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "tray watcher error, reconnecting in 2s");
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });

        handles
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

/// Fire-and-forget: send `Activate(0, 0)` to the given `StatusNotifierItem`.
pub fn activate(bus_name: &str, object_path: &str) {
    let bus_name = bus_name.to_string();
    let object_path = object_path.to_string();
    runtime::handle().spawn(async move {
        if let Err(e) = do_activate(&bus_name, &object_path).await {
            tracing::warn!(error = %e, bus_name, object_path, "tray activate failed");
        }
    });
}

async fn do_activate(bus_name: &str, object_path: &str) -> Result<()> {
    let conn = Connection::session()
        .await
        .context("open session bus for activate")?;
    conn.call_method(
        Some(bus_name),
        object_path,
        Some("org.kde.StatusNotifierItem"),
        "Activate",
        &(0i32, 0i32),
    )
    .await
    .context("call Activate")?;
    Ok(())
}

// ── Watcher state ─────────────────────────────────────────────────────────────

/// Shared mutable state threaded through the watcher and per-item tasks.
#[derive(Clone)]
struct State {
    items: Mutable<Vec<TrayItem>>,
    registered: Arc<AsyncMutex<HashMap<String, TrayItem>>>,
    conn: Connection,
}

impl State {
    fn new(items: Mutable<Vec<TrayItem>>, conn: Connection) -> Self {
        Self {
            items,
            registered: Arc::new(AsyncMutex::new(HashMap::new())),
            conn,
        }
    }

    /// Re-read one item's properties and update the map.  Returns `false`
    /// when the item should be removed (proxy read failed).
    async fn refresh_item(&self, bus_name: &str, object_path: &str) -> bool {
        match read_item_props(&self.conn, bus_name, object_path).await {
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
}

// ── `StatusNotifierWatcher` D-Bus interface ───────────────────────────────────

/// Service-side implementation of `org.kde.StatusNotifierWatcher`.
struct Watcher {
    state: State,
}

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
        let (bus_name, object_path) = parse_service(service, &sender);

        tracing::debug!(bus_name, object_path, "RegisterStatusNotifierItem");

        // Initial property read.
        match read_item_props(&self.state.conn, &bus_name, &object_path).await {
            Ok(item) => {
                let key = item.key.clone();
                self.state.registered.lock().await.insert(key.clone(), item);
                self.state.rebuild_published_list().await;

                let _ = Self::status_notifier_item_registered(
                    &emitter,
                    format!("{bus_name}{object_path}"),
                )
                .await;

                // Spawn a per-item watcher.
                let state = self.state.clone();
                let emitter_owned = emitter.to_owned();
                tokio::spawn(async move {
                    watch_item(state, bus_name, object_path, emitter_owned).await;
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, bus_name, object_path, "initial property read failed");
            }
        }
    }

    /// No-op: we are the host.
    #[allow(clippy::unused_async)]
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

async fn watch_item(
    state: State,
    bus_name: String,
    object_path: String,
    emitter: SignalEmitter<'static>,
) {
    // Build with owned strings so the proxy is `'static`-compatible.
    let proxy = match zbus::Proxy::new_owned(
        state.conn.clone(),
        bus_name.clone(),
        object_path.clone(),
        "org.kde.StatusNotifierItem".to_string(),
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, bus_name, object_path, "failed to create item proxy");
            return;
        }
    };

    // Subscribe to the four update signals.  Spawn a sub-task per signal to
    // avoid lifetime issues with merging streams.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(4);

    for sig_name in ["NewIcon", "NewTitle", "NewStatus", "NewToolTip"] {
        let tx2 = tx.clone();
        let proxy2 = proxy.clone();
        let sig = sig_name.to_string();
        tokio::spawn(async move {
            match proxy2.receive_signal(sig.clone()).await {
                Ok(mut stream) => {
                    while stream.next().await.is_some() {
                        if tx2.send(()).await.is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, signal = sig, "subscribe signal failed");
                }
            }
        });
    }
    drop(tx); // close sender side so channel closes when all sub-tasks end

    // Process update notifications.
    while rx.recv().await.is_some() {
        let still_alive = state.refresh_item(&bus_name, &object_path).await;
        state.rebuild_published_list().await;
        if !still_alive {
            let key = format!("{bus_name}{object_path}");
            tracing::debug!(key, "item disappeared, unregistering");
            let _ = Watcher::status_notifier_item_unregistered(&emitter, key).await;
            return;
        }
    }

    // All signal streams ended (item disconnected).
    let key = format!("{bus_name}{object_path}");
    state.registered.lock().await.remove(&key);
    state.rebuild_published_list().await;
    let _ = Watcher::status_notifier_item_unregistered(&emitter, key).await;
}

// ── Main listen loop ──────────────────────────────────────────────────────────

async fn listen(items: &Mutable<Vec<TrayItem>>) -> Result<()> {
    let conn = Connection::session()
        .await
        .context("connect session bus")?;

    let state = State::new(items.clone(), conn.clone());
    let watcher = Watcher {
        state: state.clone(),
    };

    // Register the watcher object.
    conn.object_server()
        .at("/StatusNotifierWatcher", watcher)
        .await
        .context("register /StatusNotifierWatcher")?;

    // Acquire the well-known name, replacing any existing holder.
    let dbus = fdo::DBusProxy::new(&conn)
        .await
        .context("create DBusProxy")?;

    let flags = fdo::RequestNameFlags::ReplaceExisting | fdo::RequestNameFlags::DoNotQueue;
    let reply = dbus
        .request_name("org.kde.StatusNotifierWatcher".try_into().unwrap(), flags)
        .await
        .context("request_name")?;

    if reply != fdo::RequestNameReply::PrimaryOwner && reply != fdo::RequestNameReply::AlreadyOwner
    {
        return Err(anyhow!(
            "could not acquire org.kde.StatusNotifierWatcher: {reply:?}"
        ));
    }

    tracing::info!("org.kde.StatusNotifierWatcher acquired");

    // Watch for bus names disappearing so we can clean up their items.
    let mut noc_stream = dbus
        .receive_name_owner_changed()
        .await
        .context("subscribe NameOwnerChanged")?;

    while let Some(signal) = noc_stream.next().await {
        let args = match signal.args() {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(error = %e, "NameOwnerChanged parse error");
                continue;
            }
        };

        // `new_owner` is empty → the bus name was released.
        if args.new_owner().is_none() {
            let gone = args.name().to_string();
            tracing::debug!(name = gone, "bus name released, pruning tray items");
            state.unregister_by_bus_name(&gone).await;
        }
    }

    Ok(())
}

// ── Helper: read item properties ──────────────────────────────────────────────

async fn read_item_props(conn: &Connection, bus_name: &str, object_path: &str) -> Result<TrayItem> {
    let proxy = zbus::Proxy::new(conn, bus_name, object_path, "org.kde.StatusNotifierItem")
        .await
        .context("create item proxy")?;

    let title: String = proxy.get_property("Title").await.unwrap_or_default();
    let icon_name: String = proxy.get_property("IconName").await.unwrap_or_default();
    let status_str: String = proxy.get_property("Status").await.unwrap_or_default();

    Ok(TrayItem {
        key: format!("{bus_name}{object_path}"),
        bus_name: bus_name.to_string(),
        object_path: object_path.to_string(),
        title,
        icon_name,
        status: ItemStatus::from_str(&status_str),
    })
}

// ── Helper: parse service argument ───────────────────────────────────────────

fn parse_service(service: &str, sender: &str) -> (String, String) {
    if service.starts_with('/') {
        (sender.to_string(), service.to_string())
    } else {
        (service.to_string(), "/StatusNotifierItem".to_string())
    }
}
