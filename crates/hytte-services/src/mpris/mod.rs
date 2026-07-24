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
//! mpris::set_active(bool);              // gate the position poller (#228)
//!
//! // Art fetch (async, cached):
//! mpris::art_for_url(url).await -> Option<Vec<u8>>
//! ```
//!
//! # Module layout
//!
//! The untrusted-input metadata parsers (the pure functions that pull fields
//! out of an arbitrary player's `a{sv}` `Metadata` map) live in [`parse`],
//! which is free of I/O and hermetically unit-tested. This file keeps the
//! service, D-Bus, per-player-task, and signal-emit logic — including the
//! bus-touching `read_metadata` orchestrator that fetches the map and hands
//! it to [`parse::parse_metadata`].

mod parse;

use anyhow::{Context, Result};
use futures_signals::map_ref;
use futures_signals::signal::{Mutable, Signal, SignalExt};
use futures_util::StreamExt;
use hytte_bus::{BusKind, BusProxy, ProxyState, call, proxy, signals};
use hytte_reactive::{Service, registry, runtime, spawn_supervised};
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
    /// Gate for the per-player `Position` pollers (#228). While `false`, every
    /// `poll_position` task parks and forks no D-Bus calls; flipping it back
    /// to `true` resumes 250 ms sampling immediately (the loop `select!`s on
    /// this so reactivation isn't delayed a full tick) and takes one eager
    /// poll on resume so the seek bar snaps fresh the instant a media panel
    /// opens.
    ///
    /// Defaults to `true` so position sampling runs eagerly at startup —
    /// `set_active(false)` parks it once the binary reports no
    /// `Page::uses_mpris_position` panel is visible. See [`set_active`].
    /// This single gate is shared across *all* per-player pollers (cloned
    /// into [`State`], which is where the pollers actually live — see
    /// `State::active`).
    pub(crate) active: Mutable<bool>,
}

impl Default for MprisHandles {
    fn default() -> Self {
        Self {
            players: Mutable::new(Vec::new()),
            selected: Mutable::new(None),
            active: Mutable::new(true),
        }
    }
}

// ── Service marker ────────────────────────────────────────────────────────────

/// The MPRIS service marker type — pass to `App::with`.
pub struct MprisService;

impl Service for MprisService {
    type Handles = MprisHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = MprisHandles::default();
        let players_mutable = handles.players.clone();
        let active_mutable = handles.active.clone();

        // Supervised so a panic inside the listener (e.g. in a future parser
        // regression) restarts the reconnect loop instead of freezing the
        // `players` signal forever. The inner `loop` still handles the ordinary
        // stream-closed / error reconnect; the supervisor only re-runs the
        // whole factory on an actual panic.
        spawn_supervised("mpris", move || {
            let players = players_mutable.clone();
            let active = active_mutable.clone();
            async move {
                loop {
                    match listen(&players, &active).await {
                        Ok(()) => {
                            tracing::warn!("mpris watcher stream closed, reconnecting in 2s");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "mpris watcher error, reconnecting in 2s");
                        }
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
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

/// Gate the per-player `Position` pollers (#228): `true` resumes 250 ms
/// sampling (taking one poll immediately so the seek bar snaps fresh),
/// `false` parks every `poll_position` task so they fork no D-Bus calls
/// while no media drawer page — the only consumer of `position_us` — is
/// visible.
///
/// Fire-and-forget command: the binary wires the media-drawer-visibility
/// signal to this so the always-on pollers idle when no one's looking (#228,
/// mirroring the #50 `netconn`/`app_usage` gates). A no-op `set` to the same
/// value is skipped to avoid spurious loop wakeups.
pub fn set_active(active: bool) {
    registry::with(|r| {
        let handle = &r
            .get::<MprisHandles>()
            .expect("mpris::service() not registered")
            .active;
        if handle.get() != active {
            handle.set(active);
        }
    });
}

/// Fire-and-forget: send `PlayPause` to the given bus name.
pub fn play_pause(bus_name: &str) {
    call(BusKind::Session, bus_name)
        .at_path(MPRIS_PATH)
        .iface(PLAYER_IFACE)
        .method("PlayPause")
        .args(())
        .fire_and_forget();
}

/// Fire-and-forget: send `Next` to the given bus name.
pub fn next(bus_name: &str) {
    call(BusKind::Session, bus_name)
        .at_path(MPRIS_PATH)
        .iface(PLAYER_IFACE)
        .method("Next")
        .args(())
        .fire_and_forget();
}

/// Fire-and-forget: send `Previous` to the given bus name.
pub fn previous(bus_name: &str) {
    call(BusKind::Session, bus_name)
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
        call(BusKind::Session, bus.as_str())
            .at_path(MPRIS_PATH)
            .iface(PLAYER_IFACE)
            .method("SetPosition")
            .args((path, position_us))
            .fire_and_forget();
    });
}

// ── Art cache ─────────────────────────────────────────────────────────────────

/// How many distinct art URLs to keep decoded in memory. Album art is up to
/// 4 MiB per entry, so an unbounded cache leaks memory monotonically over a
/// weeks-long session with a streaming player (a fresh art URL per track); this
/// caps the working set at [`ART_CACHE_CAP`] × 4 MiB (#434).
const ART_CACHE_CAP: usize = 16;

/// A tiny bounded LRU keyed by art URL. Not general-purpose: `N` is small
/// enough that the `Vec<String>` recency list is cheaper than a linked
/// hash-map, and lookups happen only on a track change (never hot).
#[derive(Default)]
struct ArtCache {
    entries: HashMap<String, Vec<u8>>,
    /// URLs least- to most-recently-used; `order.last()` is the newest.
    order: Vec<String>,
}

impl ArtCache {
    /// Fetch a cached entry, marking it most-recently-used.
    fn get(&mut self, url: &str) -> Option<Vec<u8>> {
        let bytes = self.entries.get(url)?.clone();
        self.touch(url);
        Some(bytes)
    }

    /// Insert (or refresh) an entry, evicting the least-recently-used once over
    /// [`ART_CACHE_CAP`].
    fn insert(&mut self, url: String, bytes: Vec<u8>) {
        if self.entries.insert(url.clone(), bytes).is_some() {
            self.touch(&url);
        } else {
            self.order.push(url);
            while self.order.len() > ART_CACHE_CAP {
                let oldest = self.order.remove(0);
                self.entries.remove(&oldest);
            }
        }
    }

    /// Move `url` to the most-recently-used end of `order`.
    fn touch(&mut self, url: &str) {
        if let Some(pos) = self.order.iter().position(|u| u == url) {
            let u = self.order.remove(pos);
            self.order.push(u);
        }
    }
}

type ArtCacheHandle = Arc<RwLock<ArtCache>>;

static ART_CACHE: OnceLock<ArtCacheHandle> = OnceLock::new();

fn art_cache() -> ArtCacheHandle {
    ART_CACHE
        .get_or_init(|| Arc::new(RwLock::new(ArtCache::default())))
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

    // Check cache first. Takes a write lock because a hit refreshes LRU
    // recency; lookups are per-track-change, never hot, so this is fine.
    {
        let cache = art_cache();
        let mut guard = cache.write().await;
        if let Some(bytes) = guard.get(url) {
            return Some(bytes);
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
    /// Gate for the per-player `Position` pollers (#228), cloned from
    /// [`MprisHandles::active`]. `poll_position` is spawned once per player
    /// (unlike netconn's single global loop), so the gate lives here — the
    /// shared `State` — rather than being threaded straight into each
    /// poller task individually.
    active: Mutable<bool>,
}

impl State {
    fn new(players: Mutable<Vec<Player>>, active: Mutable<bool>) -> Self {
        Self {
            map: Arc::new(AsyncMutex::new(HashMap::new())),
            order: Arc::new(AsyncMutex::new(Vec::new())),
            players,
            active,
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
    let player_proxy = match proxy(BusKind::Session, bus_name.as_str())
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
    let props_changed = signals(BusKind::Session, bus_name.as_str())
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
///
/// Gated on `state.active` (#228): while inactive (no media drawer page
/// visible on any monitor), the loop parks and forks no D-Bus calls at all —
/// not even the "is it Playing" state-map check. Reactivation is instant
/// (`select!`s on the gate rather than sleeping through a stale tick) and
/// takes one eager poll immediately on resume, via `reset_immediately`, so
/// the seek bar snaps to the true position the instant the panel opens
/// rather than waiting up to 250 ms for the next tick.
async fn poll_position(state: State, bus_name: String) {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // Park (forking nothing) while gated inactive. `wait_for(true)`
        // resolves immediately if we're already active by the time we get
        // here (no lost wakeup, mirrors netconn.rs/app_usage.rs). Once
        // reactivated, reset the interval so the following tick fires right
        // away instead of waiting out whatever was left of the last 250 ms
        // window before we parked.
        if !state.active.get() {
            let _ = state.active.signal().wait_for(true).await;
            interval.reset_immediately();
        }

        // Wait for the next tick, but bail out early if we get gated
        // inactive mid-wait — no point holding the timer while parked.
        tokio::select! {
            _ = interval.tick() => {}
            _ = state.active.signal().wait_for(false) => {
                continue;
            }
        }

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
        let pos_result = call(BusKind::Session, bus_name.as_str())
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

async fn listen(players: &Mutable<Vec<Player>>, active: &Mutable<bool>) -> Result<()> {
    let state = State::new(players.clone(), active.clone());

    // Subscribe to NameOwnerChanged on the session bus BEFORE listing current
    // names, so we don't miss any registrations during the startup window.
    let owner_changes = signals(BusKind::Session, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .signal("NameOwnerChanged")
        .start();

    // List all current names and register existing MPRIS players.
    let names: Vec<String> = call(BusKind::Session, "org.freedesktop.DBus")
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
            // Supervised: `spawn_player_tasks` reads + parses this player's
            // (untrusted) metadata, so it's the real panic surface. A clean
            // completion (player closed / stream ended) does not restart.
            spawn_supervised("mpris-player", move || {
                let state = state2.clone();
                let bus_name = bus_name.clone();
                async move {
                    spawn_player_tasks(state, bus_name).await;
                }
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
            // Supervised — same rationale as the startup-discovery spawn above.
            spawn_supervised("mpris-player", move || {
                let state = state2.clone();
                let bus_name = bus_name.clone();
                async move {
                    spawn_player_tasks(state, bus_name).await;
                }
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
    let v: OwnedValue = call(BusKind::Session, bus_name)
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
///
/// The (I/O-touching) bus fetch lives here; the pure field extraction is
/// delegated to [`parse::parse_metadata`].
async fn read_metadata(bus_name: &str) -> (String, String, String, String, u64, Option<String>) {
    match get_property::<OwnedValue>(bus_name, PLAYER_IFACE, "Metadata").await {
        Ok(raw) => parse::parse_metadata(raw),
        Err(_) => (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            0,
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{ART_CACHE_CAP, ArtCache};

    /// Store an entry whose payload is the URL's own bytes, so a `get` can
    /// assert it got the right entry back without index→byte casts (which trip
    /// clippy's truncation lint).
    fn put(cache: &mut ArtCache, url: &str) {
        cache.insert(url.to_string(), url.as_bytes().to_vec());
    }

    #[test]
    fn art_cache_evicts_oldest_over_cap() {
        let mut cache = ArtCache::default();
        // Insert one more than the cap; the very first URL must be evicted.
        for i in 0..=ART_CACHE_CAP {
            put(&mut cache, &format!("url{i}"));
        }
        assert_eq!(cache.entries.len(), ART_CACHE_CAP);
        assert_eq!(cache.order.len(), ART_CACHE_CAP);
        assert!(
            cache.get("url0").is_none(),
            "oldest entry should be evicted"
        );
        let newest = format!("url{ART_CACHE_CAP}");
        assert_eq!(cache.get(&newest), Some(newest.into_bytes()));
    }

    #[test]
    fn art_cache_get_refreshes_recency() {
        let mut cache = ArtCache::default();
        for i in 0..ART_CACHE_CAP {
            put(&mut cache, &format!("url{i}"));
        }
        // Touch the oldest so it's now most-recently-used, then overflow by one.
        assert_eq!(cache.get("url0"), Some(b"url0".to_vec()));
        put(&mut cache, "overflow");
        // url0 was refreshed, so url1 (now the oldest) is evicted instead.
        assert_eq!(
            cache.get("url0"),
            Some(b"url0".to_vec()),
            "refreshed entry survives"
        );
        assert!(cache.get("url1").is_none(), "new oldest should be evicted");
    }

    #[test]
    fn art_cache_reinsert_does_not_grow_order() {
        let mut cache = ArtCache::default();
        cache.insert("same".to_string(), vec![1]);
        cache.insert("same".to_string(), vec![2]);
        assert_eq!(cache.order.len(), 1);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.get("same"), Some(vec![2]));
    }
}
