//! MPRIS media player tracking.
//!
//! Discovers all `org.mpris.MediaPlayer2.*` names on the session bus at
//! startup and follows `NameOwnerChanged` to track live add/remove of
//! players. Per-player tokio tasks subscribe to `PropertiesChanged` on the
//! `org.mpris.MediaPlayer2.Player` interface and re-read metadata + Can*
//! flags + playback status on each change.
//!
//! # Public API
//!
//! ```ignore
//! // Register once at startup:
//! .with(mpris::service())
//!
//! // Subscribe in widgets:
//! mpris::active_player() -> impl Signal<Item = Option<Player>>
//! mpris::players()       -> impl Signal<Item = Vec<Player>>
//!
//! // Fire-and-forget commands:
//! mpris::play_pause(bus_name);
//! mpris::next(bus_name);
//! mpris::previous(bus_name);
//! ```

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_reactive::{registry, runtime, Service};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use zbus::zvariant::OwnedValue;
use zbus::Connection;

// ── Public data shapes ────────────────────────────────────────────────────────

/// Playback status of an MPRIS player.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    #[default]
    Stopped,
}

impl PlaybackStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "Playing" => Self::Playing,
            "Paused" => Self::Paused,
            _ => Self::Stopped,
        }
    }
}

/// Snapshot of a single MPRIS player's state.
#[derive(Clone, Debug, Default)]
pub struct Player {
    /// The full D-Bus session bus name, e.g. `"org.mpris.MediaPlayer2.spotify"`.
    pub bus_name: String,
    /// `Identity` from the `org.mpris.MediaPlayer2` interface (e.g. `"Spotify"`).
    pub identity: String,
    /// Current playback status.
    pub status: PlaybackStatus,
    /// Track title (from `xesam:title` metadata).
    pub title: String,
    /// Comma-joined artist list (from `xesam:artist` metadata, or empty).
    pub artists: String,
    /// Album name (from `xesam:album` metadata, or empty).
    pub album: String,
    /// Whether the player supports `PlayPause`.
    pub can_play_pause: bool,
    /// Whether the player supports `Next`.
    pub can_go_next: bool,
    /// Whether the player supports `Previous`.
    pub can_go_previous: bool,
}

// ── Service handle ────────────────────────────────────────────────────────────

/// Shared mutable state held by the service registry.
#[doc(hidden)]
pub struct MprisHandles {
    pub(crate) players: Mutable<Vec<Player>>,
    pub(crate) active: Mutable<Option<Player>>,
}

impl Default for MprisHandles {
    fn default() -> Self {
        Self {
            players: Mutable::new(Vec::new()),
            active: Mutable::new(None),
        }
    }
}

// ── Service marker ────────────────────────────────────────────────────────────

/// The MPRIS service marker type — pass to `App::with`.
pub struct MprisService;

impl Service for MprisService {
    type Handles = MprisHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = MprisHandles::default();
        let players_mutable = handles.players.clone();
        let active_mutable = handles.active.clone();

        rt.spawn(async move {
            loop {
                match listen(&players_mutable, &active_mutable).await {
                    Ok(()) => {
                        tracing::warn!("mpris watcher stream closed, reconnecting in 2s");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "mpris watcher error, reconnecting in 2s");
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });

        handles
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the MPRIS service to register with the hytte runtime.
#[must_use]
pub fn service() -> MprisService {
    MprisService
}

/// Signal that emits the current list of all tracked MPRIS players.
pub fn players() -> impl Signal<Item = Vec<Player>> {
    registry::with(|r| {
        r.get::<MprisHandles>()
            .expect("mpris::service() not registered")
            .players
            .signal_cloned()
    })
}

/// Signal that emits the currently "active" player (best according to
/// the Playing > Paused > first heuristic), or `None` when no player is
/// tracked.
pub fn active_player() -> impl Signal<Item = Option<Player>> {
    registry::with(|r| {
        r.get::<MprisHandles>()
            .expect("mpris::service() not registered")
            .active
            .signal_cloned()
    })
}

/// Fire-and-forget: send `PlayPause` to the given bus name.
pub fn play_pause(bus_name: &str) {
    let bus = bus_name.to_string();
    runtime::handle().spawn(async move {
        let conn = match Connection::session().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "mpris play_pause: failed to open session bus");
                return;
            }
        };
        let _ = conn
            .call_method(
                Some(bus.as_str()),
                "/org/mpris/MediaPlayer2",
                Some("org.mpris.MediaPlayer2.Player"),
                "PlayPause",
                &(),
            )
            .await;
    });
}

/// Fire-and-forget: send `Next` to the given bus name.
pub fn next(bus_name: &str) {
    let bus = bus_name.to_string();
    runtime::handle().spawn(async move {
        let conn = match Connection::session().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "mpris next: failed to open session bus");
                return;
            }
        };
        let _ = conn
            .call_method(
                Some(bus.as_str()),
                "/org/mpris/MediaPlayer2",
                Some("org.mpris.MediaPlayer2.Player"),
                "Next",
                &(),
            )
            .await;
    });
}

/// Fire-and-forget: send `Previous` to the given bus name.
pub fn previous(bus_name: &str) {
    let bus = bus_name.to_string();
    runtime::handle().spawn(async move {
        let conn = match Connection::session().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "mpris previous: failed to open session bus");
                return;
            }
        };
        let _ = conn
            .call_method(
                Some(bus.as_str()),
                "/org/mpris/MediaPlayer2",
                Some("org.mpris.MediaPlayer2.Player"),
                "Previous",
                &(),
            )
            .await;
    });
}

// ── Active-player heuristic ───────────────────────────────────────────────────

fn pick_active(players: &[Player]) -> Option<Player> {
    // 1. Prefer any player currently Playing.
    if let Some(p) = players.iter().find(|p| p.status == PlaybackStatus::Playing) {
        return Some(p.clone());
    }
    // 2. Else any Paused.
    if let Some(p) = players.iter().find(|p| p.status == PlaybackStatus::Paused) {
        return Some(p.clone());
    }
    // 3. Else the first registered (arbitrary but stable).
    players.first().cloned()
}

// ── Shared watcher state ──────────────────────────────────────────────────────

/// State shared between the main listener loop and per-player watcher tasks.
#[derive(Clone)]
struct State {
    /// Ordered list of tracked players (order = discovery order).
    map: Arc<AsyncMutex<HashMap<String, Player>>>,
    /// Order list for stable iteration (bus names in discovery order).
    order: Arc<AsyncMutex<Vec<String>>>,
    /// Published signal for the full player list.
    players: Mutable<Vec<Player>>,
    /// Published signal for the active player.
    active: Mutable<Option<Player>>,
    conn: Connection,
}

impl State {
    fn new(
        players: Mutable<Vec<Player>>,
        active: Mutable<Option<Player>>,
        conn: Connection,
    ) -> Self {
        Self {
            map: Arc::new(AsyncMutex::new(HashMap::new())),
            order: Arc::new(AsyncMutex::new(Vec::new())),
            players,
            active,
            conn,
        }
    }

    /// Re-read one player's properties and update the map.  Returns `false`
    /// when the player should be dropped (proxy read failed).
    async fn refresh_player(&self, bus_name: &str) -> bool {
        match read_player_props(&self.conn, bus_name).await {
            Ok(player) => {
                self.map.lock().await.insert(bus_name.to_string(), player);
                true
            }
            Err(e) => {
                tracing::debug!(error = %e, bus_name, "player property read failed, removing");
                self.map.lock().await.remove(bus_name);
                false
            }
        }
    }

    /// Rebuild and publish the players + active signals.
    async fn publish(&self) {
        let map = self.map.lock().await;
        let order = self.order.lock().await;
        let list: Vec<Player> = order
            .iter()
            .filter_map(|k| map.get(k).cloned())
            .collect();
        drop(map);
        drop(order);
        let active = pick_active(&list);
        self.players.set(list);
        self.active.set(active);
    }

    /// Register a new bus name in the tracking order.
    async fn register(&self, bus_name: &str) {
        let mut order = self.order.lock().await;
        if !order.contains(&bus_name.to_string()) {
            order.push(bus_name.to_string());
        }
    }

    /// Remove a bus name from tracking and publish.
    async fn unregister(&self, bus_name: &str) {
        self.map.lock().await.remove(bus_name);
        self.order
            .lock()
            .await
            .retain(|k| k != bus_name);
        self.publish().await;
    }
}

// ── Per-player watcher task ───────────────────────────────────────────────────

async fn watch_player(state: State, bus_name: String) {
    // Build PropertiesProxy for the Player interface.
    let props_proxy = match zbus::fdo::PropertiesProxy::builder(&state.conn)
        .destination(bus_name.clone())
        .and_then(|b| b.path("/org/mpris/MediaPlayer2"))
        .map_err(|e| anyhow::anyhow!("proxy builder: {e}"))
    {
        Ok(b) => match b.build().await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(error = %e, bus_name, "failed to build PropertiesProxy");
                return;
            }
        },
        Err(e) => {
            tracing::debug!(error = %e, bus_name, "failed to configure PropertiesProxy");
            return;
        }
    };

    let mut changes = match props_proxy.receive_properties_changed().await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, bus_name, "subscribe PropertiesChanged failed");
            return;
        }
    };

    while let Some(signal) = changes.next().await {
        // Only react to changes on the Player interface.
        let Ok(args) = signal.args() else { continue };
        if args.interface_name.as_str() != "org.mpris.MediaPlayer2.Player" {
            continue;
        }

        let still_alive = state.refresh_player(&bus_name).await;
        state.publish().await;
        if !still_alive {
            tracing::debug!(bus_name, "player disappeared mid-watch");
            return;
        }
    }

    // Stream ended → player likely disconnected.
    tracing::debug!(bus_name, "PropertiesChanged stream ended for player");
}

// ── Main listen loop ──────────────────────────────────────────────────────────

async fn listen(
    players: &Mutable<Vec<Player>>,
    active: &Mutable<Option<Player>>,
) -> Result<()> {
    let conn = Connection::session()
        .await
        .context("connect session bus")?;

    let state = State::new(players.clone(), active.clone(), conn.clone());

    // Get the DBus proxy.
    let dbus = zbus::fdo::DBusProxy::new(&conn)
        .await
        .context("create DBusProxy")?;

    // Subscribe to NameOwnerChanged before listing names so we don't miss
    // any registrations that happen during the startup window.
    let mut noc_stream = dbus
        .receive_name_owner_changed()
        .await
        .context("subscribe NameOwnerChanged")?;

    // List all current names and register MPRIS ones.
    let names: Vec<String> = dbus
        .list_names()
        .await
        .context("list_names")?
        .into_iter()
        .map(|n| n.to_string())
        .collect();

    for name in names {
        if name.starts_with("org.mpris.MediaPlayer2.") {
            tracing::debug!(name, "found existing mpris player");
            let state2 = state.clone();
            let bus_name = name.clone();
            tokio::spawn(async move {
                state2.register(&bus_name).await;
                if state2.refresh_player(&bus_name).await {
                    state2.publish().await;
                    watch_player(state2, bus_name).await;
                } else {
                    state2.unregister(&bus_name).await;
                }
            });
        }
    }

    // Process NameOwnerChanged events.
    while let Some(signal) = noc_stream.next().await {
        let args = match signal.args() {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(error = %e, "NameOwnerChanged parse error");
                continue;
            }
        };

        let name = args.name().to_string();
        if !name.starts_with("org.mpris.MediaPlayer2.") {
            continue;
        }

        if args.new_owner().is_some() {
            // New player appeared.
            tracing::debug!(name, "mpris player appeared");
            let state2 = state.clone();
            let bus_name = name.clone();
            tokio::spawn(async move {
                state2.register(&bus_name).await;
                if state2.refresh_player(&bus_name).await {
                    state2.publish().await;
                    watch_player(state2, bus_name).await;
                } else {
                    state2.unregister(&bus_name).await;
                }
            });
        } else {
            // Player released its name.
            tracing::debug!(name, "mpris player disappeared");
            state.unregister(&name).await;
        }
    }

    Ok(())
}

// ── Property readers ──────────────────────────────────────────────────────────

const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_IFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

async fn read_player_props(conn: &Connection, bus_name: &str) -> Result<Player> {
    // Two proxies — one for each interface.
    let base_proxy = zbus::Proxy::new(conn, bus_name, MPRIS_PATH, MPRIS_IFACE)
        .await
        .context("create base MediaPlayer2 proxy")?;

    let player_proxy = zbus::Proxy::new(conn, bus_name, MPRIS_PATH, PLAYER_IFACE)
        .await
        .context("create Player proxy")?;

    let identity: String = base_proxy
        .get_property("Identity")
        .await
        .unwrap_or_default();

    let status_str: String = player_proxy
        .get_property("PlaybackStatus")
        .await
        .unwrap_or_default();
    let status = PlaybackStatus::from_str(&status_str);

    let can_play_pause: bool = player_proxy
        .get_property("CanPlay")
        .await
        .unwrap_or(false);
    let can_go_next: bool = player_proxy
        .get_property("CanGoNext")
        .await
        .unwrap_or(false);
    let can_go_previous: bool = player_proxy
        .get_property("CanGoPrevious")
        .await
        .unwrap_or(false);

    let (title, artists, album) = read_metadata(&player_proxy).await;

    Ok(Player {
        bus_name: bus_name.to_string(),
        identity,
        status,
        title,
        artists,
        album,
        can_play_pause,
        can_go_next,
        can_go_previous,
    })
}

/// Extract `xesam:title`, `xesam:artist`, and `xesam:album` from the
/// `Metadata` property.  Returns `(title, artists, album)` — all default
/// to empty string on missing/malformed values.
async fn read_metadata(player_proxy: &zbus::Proxy<'_>) -> (String, String, String) {
    let raw: OwnedValue = match player_proxy.get_property("Metadata").await {
        Ok(v) => v,
        Err(_) => return (String::new(), String::new(), String::new()),
    };

    let map: HashMap<String, OwnedValue> = match HashMap::try_from(raw) {
        Ok(m) => m,
        Err(_) => return (String::new(), String::new(), String::new()),
    };

    let title = map
        .get("xesam:title")
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default();

    let album = map
        .get("xesam:album")
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default();

    // xesam:artist is a string array (as); handle gracefully if absent or malformed.
    let artists = parse_artist_array(map.get("xesam:artist"));

    (title, artists, album)
}

/// Parse `xesam:artist` from an `OwnedValue` that should be `as` (array of strings).
fn parse_artist_array(val: Option<&OwnedValue>) -> String {
    let Some(v) = val else { return String::new() };
    let Ok(owned) = v.try_clone() else {
        return String::new();
    };

    let Ok(arr) = zbus::zvariant::Array::try_from(owned) else {
        return String::new();
    };

    let parts: Vec<String> = arr
        .iter()
        .filter_map(|item| {
            let cloned = item.try_clone().ok()?;
            String::try_from(OwnedValue::try_from(cloned).ok()?).ok()
        })
        .collect();

    parts.join(", ")
}
