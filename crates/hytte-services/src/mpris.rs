//! MPRIS media player tracking.
//!
//! Discovers all `org.mpris.MediaPlayer2.*` names on the session bus at
//! startup and follows `NameOwnerChanged` to track live add/remove of
//! players. Per-player [`BusProxy`] handles survive bus reconnects and detect
//! peer departure via [`ProxyState::PeerGone`]. Per-player tokio tasks
//! subscribe to `PropertiesChanged` on the `org.mpris.MediaPlayer2.Player`
//! interface and re-read metadata + Can* flags + playback status on each
//! change.
//!
//! # Public API
//!
//! ```ignore
//! // Register once at startup:
//! .with(mpris::service())
//!
//! // Subscribe in widgets:
//! mpris::active_player()   -> impl Signal<Item = Option<Player>>
//! mpris::players()         -> impl Signal<Item = Vec<Player>>
//! mpris::selected_player() -> impl Signal<Item = Option<String>>
//!
//! // Fire-and-forget commands:
//! mpris::play_pause(bus_name);
//! mpris::next(bus_name);
//! mpris::previous(bus_name);
//! mpris::set_position(bus_name, track_id, position_us);
//! mpris::select_player(Some(bus_name)); // pin; None reverts to automatic
//!
//! // Art fetch (async, cached):
//! mpris::art_for_url(url).await -> Option<Vec<u8>>
//! ```

use anyhow::{Context, Result};
use futures_signals::map_ref;
use futures_signals::signal::{Mutable, Signal, SignalExt};
use futures_util::StreamExt;
use hytte_bus::{BusKind, BusProxy, ProxyState, call, proxy, signals};
use hytte_reactive::{Service, registry, runtime};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::RwLock;
use zbus::zvariant::OwnedValue;

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
    /// `xesam:artUrl` from metadata — `file://` or `http(s)://` URL. Empty
    /// string when unavailable.
    pub art_url: String,
    /// Current playback position, microseconds. Updated by the position
    /// poller (4 Hz while playing).
    pub position_us: u64,
    /// Track length, microseconds (from `mpris:length` in metadata).
    pub length_us: u64,
    /// Track identifier — the value of `mpris:trackid` in metadata. Needed
    /// for `SetPosition` calls. Some players supply this as an `ObjectPath`,
    /// some as a bare String; we store the raw string representation.
    pub track_id: Option<String>,
}

// ── Service handle ────────────────────────────────────────────────────────────

/// Shared mutable state held by the service registry.
#[doc(hidden)]
pub struct MprisHandles {
    pub(crate) players: Mutable<Vec<Player>>,
    /// Manual override: the `bus_name` of the player the user explicitly
    /// pinned. `None` means "automatic" (follow the [`pick_active`]
    /// heuristic). Consumed read-side by [`active_player`].
    pub(crate) selected: Mutable<Option<String>>,
}

impl Default for MprisHandles {
    fn default() -> Self {
        Self {
            players: Mutable::new(Vec::new()),
            selected: Mutable::new(None),
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

        rt.spawn(async move {
            loop {
                match listen(&players_mutable).await {
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

/// Signal that emits the currently "active" player.
///
/// When a player has been pinned via [`select_player`] *and* it is still
/// present in [`players`], that player (with fresh metadata cloned from the
/// live list) wins. Otherwise we fall back to the Playing > Paused > first
/// heuristic ([`pick_active`]). This means a pinned player that closes
/// (vanishes from `players`) automatically reverts to the heuristic — the
/// user is never stuck on a dead player.
///
/// Both the Media panel and the bar chip consume this accessor, so the
/// manual selection is honoured everywhere for free.
pub fn active_player() -> impl Signal<Item = Option<Player>> {
    registry::with(|r| {
        let handles = r
            .get::<MprisHandles>()
            .expect("mpris::service() not registered");
        let players = handles.players.signal_cloned();
        let selected = handles.selected.signal_cloned();
        map_ref! {
            let players = players,
            let selected = selected => {
                resolve_active(players, selected.as_deref())
            }
        }
    })
}

/// Signal that emits the `bus_name` of the manually pinned player, or `None`
/// when in automatic mode. Useful for a panel to flag which entry is pinned
/// versus merely heuristically active.
pub fn selected_player() -> impl Signal<Item = Option<String>> {
    registry::with(|r| {
        r.get::<MprisHandles>()
            .expect("mpris::service() not registered")
            .selected
            .signal_cloned()
    })
}

/// Resolve the active player from the live list plus an optional manual
/// override. A `Some(bus)` that matches a live player pins it (fresh
/// metadata from the list); anything else falls back to [`pick_active`].
fn resolve_active(players: &[Player], selected: Option<&str>) -> Option<Player> {
    if let Some(bus) = selected
        && let Some(p) = players.iter().find(|p| p.bus_name == bus)
    {
        return Some(p.clone());
    }
    pick_active(players)
}

/// Fire-and-forget: pin a specific player by `bus_name`, or revert to
/// automatic (heuristic) selection with `None`. Just a `Mutable` set on the
/// GTK main thread — no async needed.
pub fn select_player(bus_name: Option<String>) {
    registry::with(|r| {
        r.get::<MprisHandles>()
            .expect("mpris::service() not registered")
            .selected
            .set(bus_name);
    });
}

/// Fire-and-forget: send `PlayPause` to the given bus name.
pub fn play_pause(bus_name: &str) {
    call(bus_name)
        .bus(BusKind::Session)
        .at_path(MPRIS_PATH)
        .iface(PLAYER_IFACE)
        .method("PlayPause")
        .args(())
        .fire_and_forget();
}

/// Fire-and-forget: send `Next` to the given bus name.
pub fn next(bus_name: &str) {
    call(bus_name)
        .bus(BusKind::Session)
        .at_path(MPRIS_PATH)
        .iface(PLAYER_IFACE)
        .method("Next")
        .args(())
        .fire_and_forget();
}

/// Fire-and-forget: send `Previous` to the given bus name.
pub fn previous(bus_name: &str) {
    call(bus_name)
        .bus(BusKind::Session)
        .at_path(MPRIS_PATH)
        .iface(PLAYER_IFACE)
        .method("Previous")
        .args(())
        .fire_and_forget();
}

/// Fire-and-forget: send `SetPosition` to the given bus name.
///
/// `track_id` must be the same object path the player provided in
/// `mpris:trackid`. If the track has changed by the time the call arrives,
/// the player silently ignores it — that is the defined safe behaviour.
pub fn set_position(bus_name: &str, track_id: &str, position_us: i64) {
    let bus = bus_name.to_string();
    let track_id = track_id.to_string();
    runtime::handle().spawn(async move {
        let Ok(path) = zbus::zvariant::OwnedObjectPath::try_from(track_id.as_str()) else {
            tracing::warn!(track = %track_id, "mpris::set_position: invalid track id");
            return;
        };
        call(bus.as_str())
            .bus(BusKind::Session)
            .at_path(MPRIS_PATH)
            .iface(PLAYER_IFACE)
            .method("SetPosition")
            .args((path, position_us))
            .fire_and_forget();
    });
}

// ── Art cache ─────────────────────────────────────────────────────────────────

type ArtCacheInner = HashMap<String, Vec<u8>>;
type ArtCacheHandle = Arc<RwLock<ArtCacheInner>>;

#[allow(clippy::type_complexity)]
static ART_CACHE: OnceLock<ArtCacheHandle> = OnceLock::new();

fn art_cache() -> ArtCacheHandle {
    ART_CACHE
        .get_or_init(|| Arc::new(RwLock::new(HashMap::new())))
        .clone()
}

/// Fetch album art bytes for a URL, using an in-memory cache keyed by URL.
///
/// Supports `file://` (read from disk) and `http(s)://` (blocking HTTP via
/// ureq, capped at 4 MiB). Returns `None` for empty or unsupported URLs, or
/// on fetch failure.
pub async fn art_for_url(url: &str) -> Option<Vec<u8>> {
    if url.is_empty() {
        return None;
    }

    // Check cache first (cheap read lock).
    {
        let cache = art_cache();
        let guard = cache.read().await;
        if let Some(bytes) = guard.get(url) {
            return Some(bytes.clone());
        }
    }

    let url_owned = url.to_string();
    let (tx, rx) = futures_channel::oneshot::channel::<Option<Vec<u8>>>();
    runtime::handle().spawn_blocking(move || {
        let bytes = fetch_art_blocking(&url_owned);
        let _ = tx.send(bytes);
    });

    let bytes = rx.await.ok().flatten()?;

    // Populate cache.
    {
        let cache = art_cache();
        let mut guard = cache.write().await;
        guard.insert(url.to_string(), bytes.clone());
    }

    Some(bytes)
}

/// Synchronous art fetcher, intended to run on a blocking thread via
/// `spawn_blocking`. Handles `file://` and `http(s)://` URLs.
fn fetch_art_blocking(url: &str) -> Option<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read(path).ok();
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        const MAX_BYTES: u64 = 4 * 1024 * 1024;
        let mut resp = ureq::get(url).call().ok()?;
        return resp
            .body_mut()
            .with_config()
            .limit(MAX_BYTES)
            .read_to_vec()
            .ok();
    }
    None
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
    /// Player data keyed by bus name.
    map: Arc<AsyncMutex<HashMap<String, Player>>>,
    /// Stable discovery order (bus names in registration order).
    order: Arc<AsyncMutex<Vec<String>>>,
    /// Published signal for the full player list. The "active" player is
    /// derived read-side in [`active_player`] from this list plus the
    /// `selected` override, so the watcher only needs to publish the list.
    players: Mutable<Vec<Player>>,
}

impl State {
    fn new(players: Mutable<Vec<Player>>) -> Self {
        Self {
            map: Arc::new(AsyncMutex::new(HashMap::new())),
            order: Arc::new(AsyncMutex::new(Vec::new())),
            players,
        }
    }

    /// Re-read one player's properties and update the map. Returns `false`
    /// when the player should be dropped (property read failed).
    async fn refresh_player(&self, bus_name: &str) -> bool {
        match read_player_props(bus_name).await {
            Ok(mut player) => {
                let mut map = self.map.lock().await;
                // `Position` is intentionally not part of `PropertiesChanged`
                // per MPRIS spec, so `read_player_props` always returns 0 for
                // it. Preserve whatever the position poller last published so
                // a property change (e.g. CanGoNext flipping) doesn't snap
                // the seek bar back to 0.
                if let Some(prev) = map.get(bus_name) {
                    player.position_us = prev.position_us;
                }
                map.insert(bus_name.to_string(), player);
                true
            }
            Err(e) => {
                tracing::debug!(error = %e, bus_name, "player property read failed, removing");
                self.map.lock().await.remove(bus_name);
                false
            }
        }
    }

    /// Rebuild and publish the player list. The active player is derived
    /// from this (plus the `selected` override) on the read side.
    async fn publish(&self) {
        let map = self.map.lock().await;
        let order = self.order.lock().await;
        let list: Vec<Player> = order.iter().filter_map(|k| map.get(k).cloned()).collect();
        drop(map);
        drop(order);
        self.players.set(list);
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
        self.order.lock().await.retain(|k| k != bus_name);
        self.publish().await;
    }
}

// ── Per-player watcher task ───────────────────────────────────────────────────

/// Spawn per-player tasks: one watches `PropertiesChanged`, another polls
/// `Position`, and a third watches the [`BusProxy`] liveness signal for
/// `PeerGone`.
async fn spawn_player_tasks(state: State, bus_name: String) {
    // Register in discovery order first.
    state.register(&bus_name).await;

    // Initial property read.
    if !state.refresh_player(&bus_name).await {
        state.unregister(&bus_name).await;
        return;
    }
    state.publish().await;

    // Build a long-lived proxy for this player. The proxy monitors
    // NameOwnerChanged for this exact bus name, giving us PeerGone when the
    // player exits.
    let player_proxy = match proxy(bus_name.as_str())
        .bus(BusKind::Session)
        .at_path(MPRIS_PATH)
        .iface(PLAYER_IFACE)
        .build()
        .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::debug!(error = %e, bus_name, "failed to build BusProxy for player");
            state.unregister(&bus_name).await;
            return;
        }
    };

    // Subscribe to PropertiesChanged for this player.
    let props_changed = signals(bus_name.as_str())
        .bus(BusKind::Session)
        .at_path(MPRIS_PATH)
        .iface("org.freedesktop.DBus.Properties")
        .signal("PropertiesChanged")
        .start();

    // Spawn liveness watcher.
    {
        let state2 = state.clone();
        let bus2 = bus_name.clone();
        let proxy2 = player_proxy.clone();
        runtime::handle().spawn(async move {
            watch_liveness(state2, bus2, proxy2).await;
        });
    }

    // Spawn position poller.
    {
        let state2 = state.clone();
        let bus2 = bus_name.clone();
        runtime::handle().spawn(async move {
            poll_position(state2, bus2).await;
        });
    }

    // Run PropertiesChanged watcher in this task.
    watch_properties(state, bus_name, props_changed).await;
}

/// Watch the `BusProxy` liveness signal. When `PeerGone` fires, unregister
/// the player. The watcher exits after `PeerGone` — the NOC subscription in
/// the main loop will handle re-discovery if the player comes back.
async fn watch_liveness(state: State, bus_name: String, player_proxy: BusProxy) {
    let mut liveness_stream = player_proxy.liveness().to_stream();
    while let Some(state_val) = liveness_stream.next().await {
        if state_val == ProxyState::PeerGone {
            tracing::debug!(bus_name, "mpris player proxy: PeerGone");
            state.unregister(&bus_name).await;
            return;
        }
    }
}

/// Watch `PropertiesChanged` for a player. Re-reads all properties on each
/// emission for the `org.mpris.MediaPlayer2.Player` interface.
async fn watch_properties(state: State, bus_name: String, sub: hytte_bus::SignalSubscription) {
    let mut events = sub.events();
    while let Some(event) = events.next().await {
        // Decode body: (interface_name, changed_properties, invalidated_properties)
        let Ok((iface, _changed, _invalidated)) =
            event
                .body
                .body()
                .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
        else {
            continue;
        };

        // Only react to changes on the Player interface.
        if iface != PLAYER_IFACE {
            continue;
        }

        let still_alive = state.refresh_player(&bus_name).await;
        state.publish().await;
        if !still_alive {
            tracing::debug!(bus_name, "player disappeared mid-watch");
            return;
        }
    }

    tracing::debug!(bus_name, "PropertiesChanged stream ended for player");
}

/// Per-player position poller task. Ticks every 250 ms while the player is
/// Playing, reads the `Position` property directly (it is intentionally not
/// notified via `PropertiesChanged` in the MPRIS spec), updates `position_us`
/// in state, and re-publishes. Self-exits when the bus name disappears from
/// state.
async fn poll_position(state: State, bus_name: String) {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        interval.tick().await;

        // Check whether the player still exists and is Playing.
        let is_playing = {
            let players = state.map.lock().await;
            match players.get(&bus_name) {
                None => return, // player unregistered — self-exit
                Some(p) => p.status == PlaybackStatus::Playing,
            }
        };

        if !is_playing {
            continue;
        }

        // Read the Position property via a one-shot call.
        let pos_result = call(bus_name.as_str())
            .bus(BusKind::Session)
            .at_path(MPRIS_PATH)
            .iface("org.freedesktop.DBus.Properties")
            .method("Get")
            .args((PLAYER_IFACE, "Position"))
            .send::<OwnedValue>()
            .await;

        let pos_us = match pos_result {
            Ok(v) => {
                let pos_i64 = i64::try_from(v).unwrap_or(0);
                u64::try_from(pos_i64).unwrap_or(0)
            }
            Err(_) => continue,
        };

        // Update position in state and re-publish.
        {
            let mut players = state.map.lock().await;
            if let Some(p) = players.get_mut(&bus_name) {
                p.position_us = pos_us;
            } else {
                return; // unregistered while we were fetching
            }
        }
        state.publish().await;
    }
}

// ── Main listen loop ──────────────────────────────────────────────────────────

async fn listen(players: &Mutable<Vec<Player>>) -> Result<()> {
    let state = State::new(players.clone());

    // Subscribe to NameOwnerChanged on the session bus BEFORE listing current
    // names, so we don't miss any registrations during the startup window.
    let owner_changes = signals("org.freedesktop.DBus")
        .bus(BusKind::Session)
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .signal("NameOwnerChanged")
        .start();

    // List all current names and register existing MPRIS players.
    let names: Vec<String> = call("org.freedesktop.DBus")
        .bus(BusKind::Session)
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("ListNames")
        .args(())
        .send()
        .await
        .context("ListNames")?;

    for name in names {
        if name.starts_with("org.mpris.MediaPlayer2.") {
            tracing::debug!(name, "found existing mpris player");
            let state2 = state.clone();
            let bus_name = name.clone();
            tokio::spawn(async move {
                spawn_player_tasks(state2, bus_name).await;
            });
        }
    }

    // Process NameOwnerChanged events.
    let mut events = owner_changes.events();
    while let Some(event) = events.next().await {
        let Ok((name, _old_owner, new_owner)) =
            event.body.body().deserialize::<(String, String, String)>()
        else {
            tracing::debug!("NameOwnerChanged parse error");
            continue;
        };

        if !name.starts_with("org.mpris.MediaPlayer2.") {
            continue;
        }

        if new_owner.is_empty() {
            // Player released its name (NameOwnerChanged with empty new_owner).
            // The BusProxy liveness watcher handles this for registered players,
            // but we also handle it here for the edge case where the proxy was
            // never successfully built.
            tracing::debug!(name, "mpris player disappeared (NOC)");
            state.unregister(&name).await;
        } else {
            // New player appeared.
            tracing::debug!(name, "mpris player appeared");
            let state2 = state.clone();
            let bus_name = name.clone();
            tokio::spawn(async move {
                spawn_player_tasks(state2, bus_name).await;
            });
        }
    }

    Ok(())
}

// ── Property readers ──────────────────────────────────────────────────────────

const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_IFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

/// Helper: read a single D-Bus property via `org.freedesktop.DBus.Properties.Get`.
///
/// The reply body is always a `Variant` (signature `v`), so we deserialize as
/// `OwnedValue` first and unwrap to the requested `T` via `TryFrom`. Asking
/// zbus to deserialize the body directly as e.g. `bool` or `String` fails
/// with `SignatureMismatch` because the wire signature is `v`, not `b`/`s`.
async fn get_property<T>(
    bus_name: &str,
    iface: &'static str,
    prop: &'static str,
) -> Result<T, hytte_bus::BusError>
where
    T: TryFrom<OwnedValue> + 'static,
{
    let v: OwnedValue = call(bus_name)
        .bus(BusKind::Session)
        .at_path(MPRIS_PATH)
        .iface("org.freedesktop.DBus.Properties")
        .method("Get")
        .args((iface, prop))
        .send::<OwnedValue>()
        .await?;
    T::try_from(v).map_err(|_| hytte_bus::BusError::Permanent {
        reason: format!("type mismatch reading {iface}.{prop}"),
        dbus_name: None,
    })
}

async fn read_player_props(bus_name: &str) -> Result<Player> {
    let identity: String = get_property(bus_name, MPRIS_IFACE, "Identity")
        .await
        .unwrap_or_default();

    let status_str: String = get_property(bus_name, PLAYER_IFACE, "PlaybackStatus")
        .await
        .unwrap_or_default();
    let status = PlaybackStatus::from_str(&status_str);

    let can_play_pause: bool = get_property(bus_name, PLAYER_IFACE, "CanPlay")
        .await
        .unwrap_or(false);
    let can_go_next: bool = get_property(bus_name, PLAYER_IFACE, "CanGoNext")
        .await
        .unwrap_or(false);
    let can_go_previous: bool = get_property(bus_name, PLAYER_IFACE, "CanGoPrevious")
        .await
        .unwrap_or(false);

    let (title, artists, album, art_url, length_us, track_id) = read_metadata(bus_name).await;

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
        art_url,
        position_us: 0,
        length_us,
        track_id,
    })
}

/// Extract track metadata from the `Metadata` property. Returns
/// `(title, artists, album, art_url, length_us, track_id)` — all default
/// to empty / zero / None on missing/malformed values.
async fn read_metadata(bus_name: &str) -> (String, String, String, String, u64, Option<String>) {
    let raw: OwnedValue = match get_property(bus_name, PLAYER_IFACE, "Metadata").await {
        Ok(v) => v,
        Err(_) => {
            return (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                0,
                None,
            );
        }
    };

    let map: HashMap<String, OwnedValue> = match HashMap::try_from(raw) {
        Ok(m) => m,
        Err(_) => {
            return (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                0,
                None,
            );
        }
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

    // xesam:artUrl — a plain string in most players.
    let art_url = map
        .get("xesam:artUrl")
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default();

    // mpris:length — u64 or i64 microseconds.
    let length_us = parse_length(map.get("mpris:length"));

    // mpris:trackid — ObjectPath or String.
    let track_id = parse_track_id(map.get("mpris:trackid"));

    (title, artists, album, art_url, length_us, track_id)
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

/// Parse `mpris:length` from an `OwnedValue`. The spec says u64 but some
/// players send i64. Saturate negatives to 0.
fn parse_length(val: Option<&OwnedValue>) -> u64 {
    let Some(v) = val else { return 0 };
    let Ok(owned) = v.try_clone() else { return 0 };

    // Try u64 first (spec-compliant).
    if let Ok(n) = u64::try_from(owned.clone()) {
        return n;
    }
    // Fall back to i64, saturate negatives.
    if let Ok(n) = i64::try_from(owned) {
        return u64::try_from(n).unwrap_or(0);
    }
    0
}

/// Parse `mpris:trackid` from an `OwnedValue`. May be an `ObjectPath`, a plain
/// String, or a Variant wrapping one of those. Returns the underlying path/
/// string as a `String`, or `None` if absent or unparseable.
fn parse_track_id(val: Option<&OwnedValue>) -> Option<String> {
    let v = val?;
    let Ok(owned) = v.try_clone() else {
        return None;
    };

    // Try ObjectPath first (most common).
    if let Ok(path) = zbus::zvariant::OwnedObjectPath::try_from(owned.clone()) {
        return Some(path.as_str().to_string());
    }
    // Try plain String.
    if let Ok(s) = String::try_from(owned) {
        return Some(s);
    }
    None
}
