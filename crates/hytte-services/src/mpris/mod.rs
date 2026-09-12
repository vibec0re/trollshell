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
//! // Snapshot (for a command that must resolve a target, not re-render):
//! mpris::active_bus_name() -> Option<String>
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
use hytte_bus::{BusKind, BusProxy, ProxyState, SignalItem, call, proxy, signals};
use hytte_reactive::{Service, registry, runtime, spawn_supervised, spawn_supervised_bounded};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::RwLock;
use tokio::sync::watch;
use zbus::zvariant::OwnedValue;

use crate::retry;

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
        //
        // The reconnect delay used to be an uncapped flat 2s; #646's second
        // half moved it onto `retry::RECONNECT_RETRY`, resetting the attempt
        // count after a `listen` that stayed up at least
        // `retry::RECONNECT_RESET_AFTER` so a merely-flaky session bus doesn't
        // ratchet to the 30s ceiling and stay there. #806 moved the counter
        // itself into `retry::ReconnectBackoff`: the reset/read/advance
        // ordering it encodes was hand-rolled here (and in three sibling
        // loops), and hand-rolled wrong — the reset landed a cycle late, so a
        // run that stayed healthy for hours still reconnected at the ratcheted
        // delay. All this loop owns now is the clock and the `warn!`.
        spawn_supervised("mpris", move || {
            let players = players_mutable.clone();
            let active = active_mutable.clone();
            async move {
                let mut backoff = retry::ReconnectBackoff::new();
                loop {
                    let started = std::time::Instant::now();
                    let outcome = listen(&players, &active).await;
                    let delay = backoff.delay_after_run(started.elapsed());
                    match outcome {
                        Ok(()) => {
                            tracing::warn!(?delay, "mpris watcher stream closed, reconnecting");
                        }
                        Err(e) => {
                            tracing::warn!(?delay, error = %e, "mpris watcher error, reconnecting");
                        }
                    }
                    tokio::time::sleep(delay).await;
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

/// The `bus_name` [`active_player`] currently resolves to, **sampled once**
/// rather than subscribed (#648).
///
/// The signal accessors are what a *widget* wants: it re-renders whenever the
/// active player changes, and holds the latest bus name in its own closure to
/// address a transport call. A **command** that has to act on "whatever is
/// playing right now" has no such closure — the plugin host's `Effect::Media`
/// broker is handed a bus-name-less transport action and must resolve a target
/// at the instant of the call. This is that lookup, and it resolves exactly as
/// [`active_player`] does (a live manual pin wins, else the Playing > Paused >
/// first heuristic), so a plugin's play/pause drives the same player the bar
/// chip's button does.
///
/// GTK-main-thread only, like every registry read. `None` when no player is
/// tracked — and, unlike the signal accessors, also when [`service`] was never
/// registered rather than a panic: this is a fire-and-forget command's target
/// lookup, and "there is nothing to send this to" is the caller's branch in
/// both cases.
#[must_use]
pub fn active_bus_name() -> Option<String> {
    registry::with(|r| {
        let handles = r.get::<MprisHandles>()?;
        let players = handles.players.lock_ref();
        let selected = handles.selected.lock_ref();
        resolve_active(&players, selected.as_deref()).map(|p| p.bus_name)
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
    /// One cancellation channel per tracked player, so [`Self::unregister`]
    /// can actually stop that player's three tasks (see [`cancelled`]).
    ///
    /// Without it, "unregister" only meant "drop the row": the
    /// `PropertiesChanged` watcher stayed subscribed and its next
    /// [`SignalItem::Resubscribed`] marker re-ran `refresh_player`, which
    /// re-inserted the player into `map` — and, since #1197's per-player
    /// re-read, published it as a **blank** row (no identity, `Stopped`,
    /// every `Can*` false) for the rest of the session. The liveness watcher
    /// stayed parked on a proxy that never reports `PeerGone` for an unowned
    /// name, too. That is the half of the gap [`reconcile_players`]'s prune
    /// closes, and this is what makes the prune stick (#1201 review).
    cancels: Arc<AsyncMutex<HashMap<String, watch::Sender<bool>>>>,
}

impl State {
    fn new(players: Mutable<Vec<Player>>, active: Mutable<bool>) -> Self {
        Self {
            map: Arc::new(AsyncMutex::new(HashMap::new())),
            order: Arc::new(AsyncMutex::new(Vec::new())),
            players,
            active,
            cancels: Arc::new(AsyncMutex::new(HashMap::new())),
        }
    }

    /// Re-read one player's properties and update the map.
    ///
    /// `read_player_props` is infallible by construction — every property it
    /// reads defaults independently rather than failing the whole read (see
    /// its doc) — so there is no failure case here to report, and this used
    /// to return `bool` for one that could never actually happen: the
    /// `Err` arm was dead code, and the `state.unregister(&bus_name).await;
    /// return;` its callers held for it was unreachable. Removed rather than
    /// kept "in case a future error path returns" — the honest way to add
    /// one back is to let a property genuinely propagate a transient error
    /// (see the doc on [`spawn_player_tasks`]'s initial read for why that
    /// window is real and what closing it would take), not to leave an arm
    /// standing that nothing can currently reach.
    ///
    /// `identity` is `Some` only for the first read of a player, where
    /// [`spawn_player_tasks`] has just paid for that property to probe the
    /// bus: passing it on saves a second identical `Get` (#1201 review L6).
    /// Every later read passes `None` and reads it with the rest.
    async fn refresh_player(&self, bus_name: &str, identity: Option<String>) {
        let mut player = read_player_props(bus_name, identity).await;
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

    /// Register a new bus name in the tracking order, reporting whether this
    /// call is the one that inserted it.
    ///
    /// That answer is the **only** gate against watching one player twice, so
    /// every discovery path goes through [`Spawner::spawn_if_new`] rather than
    /// calling `spawn_player` directly (#1201 review). Until then this was
    /// silently idempotent and `spawn_player_tasks` registered from *inside*
    /// the spawned task, so two deliveries of the same name — the `ListNames`
    /// re-read's reply, and the `NameOwnerChanged` for a player that appeared
    /// while that round trip was in flight, buffered behind it — each spawned
    /// a full task set: two proxies, two `PropertiesChanged` subscriptions,
    /// two position pollers and two liveness watchers for the rest of the
    /// session, with nothing to notice (`unregister` is idempotent too).
    ///
    /// The [`CancelRx`] it hands back on success is the player's teardown
    /// signal, and there is exactly one per registration — which is the other
    /// reason this cannot be silently idempotent.
    async fn register(&self, bus_name: &str) -> Option<CancelRx> {
        // `order` is held across the `cancels` insert so a name is never in
        // one map without the other (both this and `unregister` take them in
        // this order, so they cannot deadlock).
        let mut order = self.order.lock().await;
        if order.iter().any(|n| n == bus_name) {
            return None;
        }
        let (tx, rx) = watch::channel(false);
        self.cancels.lock().await.insert(bus_name.to_string(), tx);
        order.push(bus_name.to_string());
        Some(rx)
    }

    /// Snapshot the tracked bus names, in registration order.
    ///
    /// Taken by [`discover_players`] *before* it asks `ListNames`, because
    /// only a name that was already tracked when the question was asked may
    /// be pruned by the answer — see [`reconcile_players`].
    async fn tracked(&self) -> Vec<String> {
        self.order.lock().await.clone()
    }

    /// Remove a bus name from tracking, stop its tasks, and publish.
    async fn unregister(&self, bus_name: &str) {
        self.map.lock().await.remove(bus_name);
        self.order.lock().await.retain(|k| k != bus_name);
        // Both halves of this are cancellation (see `cancelled`): the flag
        // going true, and the sender being dropped by `remove`.
        if let Some(cancel) = self.cancels.lock().await.remove(bus_name) {
            let _ = cancel.send(true);
        }
        self.publish().await;
    }
}

/// A per-player teardown signal, handed out by [`State::register`] and
/// resolved by [`cancelled`].
type CancelRx = watch::Receiver<bool>;

/// Resolve once this player has been unregistered.
///
/// Two things reach here and both mean the same thing: [`State::unregister`]
/// flips the flag to `true`, and it also drops the sender, which makes
/// `wait_for` return `Err`. Either way the player is gone, and the task that
/// awaited this must stop — a subscription, a poller and a liveness watcher
/// that outlive their row are exactly how a pruned player came back as a
/// blank one (see [`State::cancels`]).
async fn cancelled(cancel: &mut CancelRx) {
    let _ = cancel.wait_for(|gone| *gone).await;
}

// ── Per-player watcher task ───────────────────────────────────────────────────

/// How many times a player's setup is retried while the bus keeps answering
/// "mid-reconnect", before the player is given up.
///
/// **Why it is retried at all.** A player is discovered exactly once — from
/// `ListNames` at startup, or from a single `NameOwnerChanged` when it
/// appears — and the broker never re-announces a name that is already owned.
/// So a setup step that returns on its first failure returns forever: the
/// player goes on running and holding its name, and this service never watches
/// it again. Both of the steps below fail on a bus blip, and a blip at startup
/// is the likely case, since that is when the shell brings up every service at
/// once (#1173).
///
/// **Why it is bounded.** A task that cannot give up is a task that leaks: a
/// name that was on the bus at `ListNames` and is gone by the time the bus
/// comes back would keep a retry loop alive for the life of the process. At
/// [`retry::ReconnectBackoff`]'s ramp — 500 ms doubling to a 30 s ceiling —
/// eight attempts is about a minute of a session bus that will not answer,
/// which is far past any blip and still bounded.
const PLAYER_SETUP_ATTEMPTS: u32 = 8;

/// Why a [`setup_step`] gave up.
///
/// The two cases say different things about the player and the bus, and a
/// caller can reasonably treat them differently — the `Identity` probe does
/// (see [`spawn_player_tasks`]), which is why this is not an `Option`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SetupFail {
    /// The peer answered, and answered in a way no retry can change: it does
    /// not implement the interface, or the property has the wrong type. The
    /// bus is fine; this player is not conformant.
    Permanent,
    /// The whole [`PLAYER_SETUP_ATTEMPTS`] budget went on transient failures —
    /// about a minute of a session bus that will not answer.
    Exhausted,
}

/// Run `step` until it succeeds, fails in a way a retry cannot fix, or spends
/// [`PLAYER_SETUP_ATTEMPTS`]. Backs off on the crate's reconnect ramp.
///
/// Only *transient* failures are retried. A permanent one — the player does
/// not implement the interface, the name is malformed — will answer the same
/// way for as long as the budget lasts, so retrying it is pure latency.
async fn setup_step<T, F, Fut>(
    what: &'static str,
    bus_name: &str,
    mut step: F,
) -> Result<T, SetupFail>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, hytte_bus::BusError>>,
{
    let mut backoff = retry::ReconnectBackoff::new();
    for attempt in 1..=PLAYER_SETUP_ATTEMPTS {
        match step().await {
            Ok(v) => return Ok(v),
            Err(e) if !e.is_transient() => {
                tracing::debug!(error = %e, bus_name, what,
                    "mpris player setup failed permanently");
                return Err(SetupFail::Permanent);
            }
            Err(e) => {
                // `Duration::ZERO`: the "run" that just ended was a failed
                // attempt, so it never stayed up long enough to earn a reset.
                let delay = backoff.delay_after_run(Duration::ZERO);
                tracing::debug!(error = %e, bus_name, what, attempt, ?delay,
                    "mpris player setup hit a bus blip; retrying");
                tokio::time::sleep(delay).await;
            }
        }
    }
    tracing::warn!(
        bus_name,
        what,
        "mpris player setup kept failing transiently; giving up on this player \
         (the broker does not re-announce a name that is already owned, so only \
         a later `ListNames` re-read can pick it up again)"
    );
    Err(SetupFail::Exhausted)
}

/// Spawn per-player tasks: one watches `PropertiesChanged`, another polls
/// `Position`, and a third watches the [`BusProxy`] liveness signal for
/// `PeerGone`.
async fn spawn_player_tasks(state: State, bus_name: String, cancel: CancelRx) {
    // Registration already happened, synchronously, at the call site — see
    // [`Spawner::spawn_if_new`] for why it cannot happen here; `cancel` is
    // the receiver that registration handed out.

    // Build the long-lived proxy BEFORE the first property read — the reverse
    // of the order this ran in until #1173.
    //
    // `build()` fails exactly when `SharedConnection` has no live connection,
    // which makes it the one step here that can *tell* whether the bus is up:
    // `read_player_props` swallows every per-property error into a default, so
    // a read taken during a blip does not fail, it silently publishes a blank
    // player (no identity, Stopped, every Can* false) and leaves the drawer
    // showing it. Succeeding here means there is a connection for the reads
    // that follow.
    let Ok(player_proxy) = setup_step("proxy", &bus_name, || {
        proxy(BusKind::Session, bus_name.as_str())
            .at_path(MPRIS_PATH)
            .iface(PLAYER_IFACE)
            .build()
    })
    .await
    else {
        state.unregister(&bus_name).await;
        return;
    };

    // Probe `Identity` through `setup_step` before the initial property read.
    // `build()` succeeding just above proves the bus was up a moment ago, but
    // a blip in the window between that and the first `Get` **narrows** rather
    // than closes: `read_player_props` defaults every property independently
    // on failure (see its doc), so it can never itself report a blip, and a
    // blip in the window that is left still publishes a blank player for one
    // cycle. `Identity` is a real property call to the same object in the same
    // window, so routing it through `setup_step` gives the first `Get` the
    // same treatment `build()` gets — a transient failure is retried on the
    // reconnect ramp instead of publishing a default. Closing the window
    // outright would mean letting `read_player_props` propagate a transient
    // error, which is a bigger change than #1197's review asked for.
    //
    // The probed string is then *used* (#1201 review L6): `refresh_player`
    // takes it instead of issuing a second, identical `Get` one line later.
    //
    // The two failures are not the same failure, and only one of them is this
    // player's fault:
    //
    // * `Exhausted` — a minute of a session bus that will not answer. Nothing
    //   below would work either, so give up; a later `ListNames` re-read
    //   rediscovers the player once the bus is back (see
    //   `reconcile_players`).
    // * `Permanent` — the peer answered, and answered badly: no `Identity`
    //   property, or one that is not a string. The bus is fine and the player
    //   is real, so it keeps its row with an empty identity, exactly as it did
    //   before #1197 added this probe. Dropping it would make a
    //   non-conformant player invisible rather than merely unnamed.
    let identity = match setup_step("initial identity", &bus_name, || {
        get_property::<String>(bus_name.as_str(), MPRIS_IFACE, "Identity")
    })
    .await
    {
        Ok(identity) => Some(identity),
        Err(SetupFail::Permanent) => {
            tracing::debug!(
                bus_name,
                "mpris player has no readable Identity; watching it anyway, unnamed"
            );
            None
        }
        Err(SetupFail::Exhausted) => {
            state.unregister(&bus_name).await;
            return;
        }
    };

    state.refresh_player(&bus_name, identity).await;
    state.publish().await;

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
        let cancel2 = cancel.clone();
        runtime::handle().spawn(async move {
            watch_liveness(state2, bus2, proxy2, cancel2).await;
        });
    }

    // Spawn position poller.
    {
        let state2 = state.clone();
        let bus2 = bus_name.clone();
        let cancel2 = cancel.clone();
        runtime::handle().spawn(async move {
            poll_position(state2, bus2, cancel2).await;
        });
    }

    // Run PropertiesChanged watcher in this task.
    watch_properties(state, bus_name, props_changed, cancel).await;
}

/// Watch the `BusProxy` liveness signal. When `PeerGone` fires, unregister
/// the player. The watcher exits after `PeerGone` — the NOC subscription in
/// the main loop will handle re-discovery if the player comes back.
///
/// It also exits when the player is unregistered some other way, which is not
/// a courtesy: `PeerGone` is **not** a reliable notice here. A proxy rebuild
/// after a reconnect is `zbus::Proxy::new_owned`, which succeeds for a name
/// nobody owns, so liveness goes straight back to `Live` for a player that
/// quit during the gap and this watcher would park forever holding a proxy
/// for a peer that is gone (#1201 review).
async fn watch_liveness(
    state: State,
    bus_name: String,
    player_proxy: BusProxy,
    mut cancel: CancelRx,
) {
    let mut liveness_stream = player_proxy.liveness().to_stream();
    loop {
        let state_val = tokio::select! {
            biased;
            () = cancelled(&mut cancel) => {
                tracing::debug!(bus_name, "mpris player unregistered; dropping its liveness watcher");
                return;
            }
            next = liveness_stream.next() => match next {
                Some(v) => v,
                None => return,
            },
        };
        if state_val == ProxyState::PeerGone {
            tracing::debug!(bus_name, "mpris player proxy: PeerGone");
            state.unregister(&bus_name).await;
            return;
        }
    }
}

/// Watch `PropertiesChanged` for a player. Re-reads all properties on each
/// emission for the `org.mpris.MediaPlayer2.Player` interface — and on each
/// [`SignalItem::Resubscribed`] or [`SignalItem::Lagged`] marker.
///
/// This state is a **fold** over emissions: `refresh_player` is only ever
/// called because a `PropertiesChanged` said something moved. Between a
/// subscription dying and its replacement going up there is no match rule for
/// the broker to route through and nothing is replayed, so every change the
/// player made in that window is lost — and a paused player that never changes
/// again leaves the drawer showing a track that finished during the outage,
/// indefinitely. So this is `items()` rather than `events()`: these markers
/// are the only notice a consumer gets that its history has a hole, and the
/// correct reaction to either is the same one an emission gets (#1173).
///
/// Stops when the player is unregistered, which is load-bearing rather than
/// tidy: the markers keep arriving for the life of the process, so a watcher
/// that outlived its row would keep re-reading a player that is gone and
/// (since #1197) keep publishing it back as a blank row. See
/// [`State::cancels`].
async fn watch_properties(
    state: State,
    bus_name: String,
    sub: hytte_bus::SignalSubscription,
    cancel: CancelRx,
) {
    // `sub` is held by this scope, not moved into the driver: dropping the
    // last handle is what tears the match rule down, so it must outlive the
    // loop and die with it.
    drive_property_items(&state, &bus_name, sub.items(), cancel).await;
    tracing::debug!(bus_name, "PropertiesChanged stream ended for player");
}

/// The body of [`watch_properties`], over an injected item stream so the
/// teardown above is a testable property and not a hopeful comment.
///
/// The stream type is also the guard on the `items()`/`events()` choice this
/// fold depends on: `events()` yields `SignalEvent`, which does not fit here.
async fn drive_property_items<S>(state: &State, bus_name: &str, mut items: S, mut cancel: CancelRx)
where
    S: futures_util::Stream<Item = SignalItem> + Unpin,
{
    loop {
        let item = tokio::select! {
            biased;
            () = cancelled(&mut cancel) => {
                tracing::debug!(
                    bus_name,
                    "mpris player unregistered; dropping its PropertiesChanged subscription"
                );
                return;
            }
            next = items.next() => match next {
                Some(item) => item,
                None => return,
            },
        };
        match item {
            SignalItem::Resubscribed => {
                tracing::debug!(
                    bus_name,
                    "PropertiesChanged re-subscribed; re-reading player properties"
                );
            }
            SignalItem::Lagged { skipped } => {
                tracing::debug!(
                    bus_name,
                    skipped,
                    "PropertiesChanged consumer lagged behind the broadcast \
                     channel; re-reading player properties"
                );
            }
            SignalItem::Event(event) => {
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
            }
        }

        // `None`: a re-read reads `Identity` with everything else — only the
        // very first read has a probed value to reuse.
        state.refresh_player(bus_name, None).await;
        state.publish().await;
    }
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
async fn poll_position(state: State, bus_name: String, mut cancel: CancelRx) {
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // Park (forking nothing) while gated inactive. `wait_for(true)`
        // resolves immediately if we're already active by the time we get
        // here (no lost wakeup, mirrors netconn.rs/app_usage.rs). Once
        // reactivated, reset the interval so the following tick fires right
        // away instead of waiting out whatever was left of the last 250 ms
        // window before we parked.
        //
        // Unregistration wins over both waits: the self-exit below only runs
        // on a tick, so a poller parked on the gate (or on a 250 ms tick)
        // for a player that just went away would linger until the panel is
        // next opened.
        if !state.active.get() {
            tokio::select! {
                biased;
                () = cancelled(&mut cancel) => return,
                _ = state.active.signal().wait_for(true) => interval.reset_immediately(),
            }
        }

        // Wait for the next tick, but bail out early if we get gated
        // inactive mid-wait — no point holding the timer while parked.
        tokio::select! {
            biased;
            () = cancelled(&mut cancel) => return,
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

/// Spawn `spawn_player_tasks` for `bus_name`, supervised and bounded.
///
/// Supervised: `spawn_player_tasks` reads + parses this player's (untrusted)
/// metadata, so it's the real panic surface. Bounded (#1174): a clean
/// completion means the player closed, which is this task finishing its job —
/// no restart, and none of the `warn!`-plus-kept-row that `spawn_supervised`
/// gives an unexpected return. One per player over a session is a lot of
/// both.
fn spawn_player(state: &State, bus_name: String, cancel: CancelRx) {
    let state2 = state.clone();
    spawn_supervised_bounded("mpris-player", move || {
        let state = state2.clone();
        let bus_name = bus_name.clone();
        let cancel = cancel.clone();
        async move {
            spawn_player_tasks(state, bus_name, cancel).await;
        }
    });
}

/// The "start watching this player" step, as a value.
///
/// There are two places a player is discovered — the `ListNames` pass in
/// [`discover_players`] and the `NameOwnerChanged` "appeared" arm in
/// [`run_owner_change_loop`] — and a bus blip makes them race each other (see
/// [`State::register`] for the timeline). Funnelling both through
/// [`Self::spawn_if_new`] is what makes "one task set per player" a property
/// of the code rather than of the order two deliveries happen to arrive in,
/// and holding the spawn behind an `Arc<dyn Fn>` lets a test count spawns
/// without a session bus — including through `run_owner_change_loop`, so the
/// gate is exercised where the race actually happens.
#[derive(Clone)]
struct Spawner(Arc<dyn Fn(&State, String, CancelRx) + Send + Sync>);

impl Spawner {
    /// The production spawner: one supervised, bounded [`spawn_player`] task
    /// set per player.
    fn player() -> Self {
        Self(Arc::new(
            |state: &State, bus_name: String, cancel: CancelRx| {
                spawn_player(state, bus_name, cancel);
            },
        ))
    }

    /// Register `bus_name` and spawn its task set **only** if this call is the
    /// one that inserted the name. Returns whether it spawned.
    ///
    /// Registering here rather than inside the spawned task is the whole
    /// point: a duplicate is refused before a proxy, a subscription, a poller
    /// and a liveness watcher exist, not after.
    async fn spawn_if_new(&self, state: &State, bus_name: String) -> bool {
        let Some(cancel) = state.register(&bus_name).await else {
            tracing::debug!(
                bus_name,
                "mpris player is already watched; not spawning a second task set"
            );
            return false;
        };
        (self.0)(state, bus_name, cancel);
        true
    }
}

/// The session-bus name prefix every MPRIS player owns.
const PLAYER_NAME_PREFIX: &str = "org.mpris.MediaPlayer2.";

/// Ask the broker for every current session-bus name and reconcile the
/// tracked player set against the answer. Used both for the startup snapshot
/// and for the [`SignalItem::Resubscribed`]/[`SignalItem::Lagged`] re-read
/// (#1201): the `NameOwnerChanged` subscription this feeds is a fold exactly
/// like [`watch_properties`]'s — a hole in its history means a player that
/// appeared during the gap is never discovered at all, and one that quit
/// stays in the drawer forever (nothing re-announces a name that already went
/// away).
///
/// The tracked set is snapshotted **before** the round trip; see
/// [`reconcile_players`] for what that snapshot is for.
async fn discover_players(state: &State, spawner: &Spawner) -> Result<()> {
    let tracked_before = state.tracked().await;

    let names: Vec<String> = call(BusKind::Session, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("ListNames")
        .args(())
        .send()
        .await
        .context("ListNames")?;

    reconcile_players(state, &names, &tracked_before, spawner).await;

    Ok(())
}

/// Reconcile the tracked player set against a full `ListNames` snapshot:
/// start watching every MPRIS name that is not tracked yet, and **stop**
/// tracking every name that is tracked but absent from the reply.
///
/// The prune is the half of the re-read that #1201 first shipped without, and
/// it is the user-visible half. `ListNames` is the only thing that ever
/// re-answers "is this player still here": the broker does not re-announce a
/// name that already went away, and the per-player liveness watcher cannot
/// stand in for it (a proxy rebuild after a reconnect succeeds for an unowned
/// name, so it reports `Live`, never `PeerGone` — see [`watch_liveness`]). So
/// without this, a player that quit during a bus gap kept its row for the
/// rest of the session, and #1197's per-player re-read repainted that row
/// blank on every later marker.
///
/// Only names in `tracked_before` — the snapshot taken before `ListNames` was
/// asked — are candidates for pruning. A player discovered by the
/// `NameOwnerChanged` arm *while* the round trip was in flight is missing
/// from the reply too, and dropping it would be the very bug the re-read
/// exists to fix.
async fn reconcile_players(
    state: &State,
    names: &[String],
    tracked_before: &[String],
    spawner: &Spawner,
) {
    for name in names.iter().filter(|n| n.starts_with(PLAYER_NAME_PREFIX)) {
        if spawner.spawn_if_new(state, name.clone()).await {
            tracing::debug!(name = %name, "found mpris player");
        }
    }

    for gone in tracked_before
        .iter()
        .filter(|tracked| !names.iter().any(|live| live == *tracked))
    {
        tracing::info!(
            bus_name = %gone,
            "mpris player is no longer on the bus; unregistering (its release was missed)"
        );
        state.unregister(gone).await;
    }
}

// ── Main listen loop ──────────────────────────────────────────────────────────

/// Drive the `NameOwnerChanged` items stream: dispatch each ordinary
/// emission through the existing per-name register/unregister logic
/// (byte-identical to before #1201), and call `rediscover` once per
/// [`SignalItem::Resubscribed`]/[`SignalItem::Lagged`] marker instead of
/// dropping it — before #1201 this subscription was read via `events()`,
/// which cannot represent either marker, so a bus blip left a player that
/// appeared during the gap undiscovered for the rest of the session and one
/// that quit still shown (nothing re-announces a name that is already
/// gone). `rediscover` is injectable so a test can stand in for
/// [`discover_players`]'s real `ListNames` round trip with a counter — aside
/// from that seam, this is exactly what used to run inline in [`listen`].
async fn run_owner_change_loop<S, Rediscover, Fut>(
    state: &State,
    mut items: S,
    spawner: &Spawner,
    mut rediscover: Rediscover,
) where
    S: futures_util::Stream<Item = SignalItem> + Unpin,
    Rediscover: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    while let Some(item) = items.next().await {
        let event = match item {
            SignalItem::Resubscribed | SignalItem::Lagged { .. } => {
                tracing::info!("mpris NameOwnerChanged resubscribed; re-discovering players");
                if let Err(e) = rediscover().await {
                    tracing::warn!(error = %e, "mpris: re-discovery after resubscribe failed");
                }
                continue;
            }
            SignalItem::Event(event) => event,
        };

        let Ok((name, _old_owner, new_owner)) =
            event.body.body().deserialize::<(String, String, String)>()
        else {
            tracing::debug!("NameOwnerChanged parse error");
            continue;
        };

        if !name.starts_with(PLAYER_NAME_PREFIX) {
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
            // New player appeared. Through the same gate as the `ListNames`
            // pass: this arm and that one race whenever a player appears
            // during a re-read's round trip, and before #1201's review this
            // one spawned unconditionally.
            tracing::debug!(name, "mpris player appeared");
            spawner.spawn_if_new(state, name).await;
        }
    }
}

async fn listen(players: &Mutable<Vec<Player>>, active: &Mutable<bool>) -> Result<()> {
    let state = State::new(players.clone(), active.clone());

    // Subscribe to NameOwnerChanged on the session bus BEFORE listing current
    // names, so we don't miss any registrations during the startup window.
    let owner_changes = signals(BusKind::Session, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .signal("NameOwnerChanged")
        .start();

    // One gate for both discovery paths (see `Spawner`).
    let spawner = Spawner::player();

    // List all current names and register existing MPRIS players.
    discover_players(&state, &spawner).await?;

    run_owner_change_loop(&state, owner_changes.items(), &spawner, || {
        discover_players(&state, &spawner)
    })
    .await;

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

/// Read every player property, defaulting each one independently on failure
/// (see the call sites below) rather than failing the whole read — so this is
/// infallible by construction and returns `Player` directly rather than a
/// `Result` nothing can actually put an `Err` into. See [`State::refresh_player`]
/// for what that means for the caller.
///
/// `identity`, when given, is a value the caller already read (and retried) —
/// the first read of a player passes the string its bus probe paid for rather
/// than asking the same question again (#1201 review L6).
async fn read_player_props(bus_name: &str, identity: Option<String>) -> Player {
    let identity: String = match identity {
        Some(known) => known,
        None => get_property(bus_name, MPRIS_IFACE, "Identity")
            .await
            .unwrap_or_default(),
    };

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

    Player {
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
    }
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
    use super::{ART_CACHE_CAP, ArtCache, PLAYER_SETUP_ATTEMPTS, SetupFail, setup_step};
    use std::cell::Cell;

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

    // ── #1173: a player found during a bus blip is still watched ─────────────

    fn transient() -> hytte_bus::BusError {
        hytte_bus::BusError::Transient {
            source: zbus::Error::FDO(Box::new(zbus::fdo::Error::Disconnected(
                "bus mid-reconnect".to_owned(),
            ))),
        }
    }

    fn permanent() -> hytte_bus::BusError {
        hytte_bus::BusError::Permanent {
            reason: "no such interface".to_owned(),
            dbus_name: Some("org.freedesktop.DBus.Error.UnknownInterface".to_owned()),
        }
    }

    /// A blip must not cost the player: the step is retried and the setup goes
    /// on. Before #1173 both setup steps returned on the first error, and since
    /// the broker never re-announces a name that is already owned, that return
    /// was permanent — the player kept running and was never watched again.
    #[tokio::test(start_paused = true)]
    async fn a_transient_failure_is_retried() {
        let calls = Cell::new(0u32);
        let got = setup_step("test", "org.mpris.MediaPlayer2.x", || {
            let n = calls.get() + 1;
            calls.set(n);
            std::future::ready(if n == 3 { Ok(n) } else { Err(transient()) })
        })
        .await;
        assert_eq!(got, Ok(3));
        assert_eq!(calls.get(), 3, "no attempt after the one that worked");
    }

    /// A permanent failure is not retried: it will answer the same way for as
    /// long as the budget lasts, so a retry is pure latency.
    ///
    /// It must also be *distinguishable* from a spent budget (#1201 review
    /// L6): the `Identity` probe keeps a player that answered badly and drops
    /// one whose bus never answered, and it cannot tell them apart from an
    /// `Option`.
    #[tokio::test(start_paused = true)]
    async fn a_permanent_failure_is_not_retried() {
        let calls = Cell::new(0u32);
        let got: Result<u32, SetupFail> = setup_step("test", "org.mpris.MediaPlayer2.x", || {
            calls.set(calls.get() + 1);
            std::future::ready(Err(permanent()))
        })
        .await;
        assert_eq!(got, Err(SetupFail::Permanent));
        assert_eq!(
            calls.get(),
            1,
            "a permanent answer must be the last attempt"
        );
    }

    /// The budget is real: a bus that never comes back must not leave a retry
    /// loop alive for the life of the process.
    #[tokio::test(start_paused = true)]
    async fn the_budget_is_spent_and_the_player_given_up() {
        let calls = Cell::new(0u32);
        let got: Result<u32, SetupFail> = setup_step("test", "org.mpris.MediaPlayer2.x", || {
            calls.set(calls.get() + 1);
            std::future::ready(Err(transient()))
        })
        .await;
        assert_eq!(
            got,
            Err(SetupFail::Exhausted),
            "spending the budget is not the same failure as a bad answer"
        );
        assert_eq!(calls.get(), PLAYER_SETUP_ATTEMPTS);
    }

    // ── #1201: NameOwnerChanged re-discovers on Resubscribed/Lagged ─────────

    use super::{
        CancelRx, Duration, PLAYER_NAME_PREFIX, Player, Spawner, State, cancelled,
        drive_property_items, reconcile_players, run_owner_change_loop,
    };
    use futures_signals::signal::Mutable;
    use hytte_bus::{SignalEvent, SignalItem};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A `Spawner` that counts instead of spawning, so a test can drive the
    /// real discovery paths without a session bus.
    fn counting_spawner() -> (Spawner, Arc<AtomicUsize>) {
        let spawns = Arc::new(AtomicUsize::new(0));
        let counter = spawns.clone();
        (
            Spawner(Arc::new(move |_state, _bus_name, _cancel| {
                counter.fetch_add(1, Ordering::SeqCst);
            })),
            spawns,
        )
    }

    /// One `NameOwnerChanged` emission as it arrives on the wire:
    /// `(name, old_owner, new_owner)`.
    fn noc(name: &str, new_owner: &str) -> SignalItem {
        let body = zbus::Message::signal(
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameOwnerChanged",
        )
        .expect("signal builder")
        .build(&(name.to_owned(), String::new(), new_owner.to_owned()))
        .expect("build NameOwnerChanged message");
        SignalItem::Event(SignalEvent {
            body,
            sender: None,
            timestamp: std::time::SystemTime::now(),
        })
    }

    /// Before #1201 this loop was driven by `events()`, which cannot
    /// represent either marker, so a bus blip here left the player list
    /// stale until something else happened to poke it. Pushing exactly one
    /// marker through `run_owner_change_loop` must call `rediscover` exactly
    /// once — for both spellings of "history has a hole" (#1173's
    /// `Resubscribed`, and the review's `Lagged` fix).
    ///
    /// Falsifiable: deleting the `Resubscribed | Lagged { .. }` arm (or
    /// making it a no-op) drops both counts to 0.
    #[tokio::test(flavor = "current_thread")]
    async fn owner_change_loop_rediscovers_exactly_once_per_marker() {
        let state = State::new(Mutable::new(Vec::new()), Mutable::new(true));
        let (spawner, _spawns) = counting_spawner();

        let calls = Arc::new(AtomicUsize::new(0));
        {
            let calls = calls.clone();
            let items = futures_util::stream::iter(vec![SignalItem::Resubscribed]);
            run_owner_change_loop(&state, items, &spawner, move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })
            .await;
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Resubscribed must re-discover exactly once"
        );

        let calls = Arc::new(AtomicUsize::new(0));
        {
            let calls = calls.clone();
            let items = futures_util::stream::iter(vec![SignalItem::Lagged { skipped: 7 }]);
            run_owner_change_loop(&state, items, &spawner, move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
            })
            .await;
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Lagged must re-discover exactly once"
        );
    }

    // ── #1201 review: one task set per player, however it is discovered ─────

    /// The M2 race, end to end through the loop: the bus reconnects, the
    /// marker is read, the re-read's `ListNames` returns player Y, and Y's
    /// own `NameOwnerChanged` is buffered *behind* the marker so it is read
    /// straight after. Both deliveries name the same player, and before the
    /// review's fix each spawned a full task set — `register` ran inside the
    /// spawned task and only the discovery side checked.
    ///
    /// Falsifiable two ways: making `State::register` unconditionally return
    /// `true`, or having the NOC-appeared arm call `spawn_player` directly
    /// again, both give 2.
    #[tokio::test(flavor = "current_thread")]
    async fn a_player_delivered_by_both_paths_spawns_once() {
        let state = State::new(Mutable::new(Vec::new()), Mutable::new(true));
        let (spawner, spawns) = counting_spawner();
        let name = format!("{PLAYER_NAME_PREFIX}y");

        let items = futures_util::stream::iter(vec![
            SignalItem::Resubscribed,
            noc(&name, ":1.9"),
            // A second appearance of the same name (a blip the broker
            // re-announces) must not add a task set either.
            noc(&name, ":1.9"),
        ]);
        {
            let spawner2 = spawner.clone();
            let state2 = state.clone();
            let name2 = name.clone();
            run_owner_change_loop(&state, items, &spawner, move || {
                // Stand in for `discover_players`: the `ListNames` reply
                // includes Y, which the re-read must start watching.
                let spawner = spawner2.clone();
                let state = state2.clone();
                let name = name2.clone();
                async move {
                    spawner.spawn_if_new(&state, name).await;
                    Ok(())
                }
            })
            .await;
        }

        assert_eq!(
            spawns.load(Ordering::SeqCst),
            1,
            "one player must cost exactly one task set, however many paths deliver it"
        );
        assert_eq!(
            state.order.lock().await.len(),
            1,
            "and exactly one tracking entry"
        );
    }

    // ── #1201 review HIGH 1: the re-read prunes as well as adds ─────────────

    /// Register a player and give it a published row, the way a live
    /// `spawn_player_tasks` would. Returns its teardown receiver.
    async fn seed_player(state: &State, bus_name: &str) -> CancelRx {
        let cancel = state
            .register(bus_name)
            .await
            .expect("fresh name registers");
        state.map.lock().await.insert(
            bus_name.to_string(),
            Player {
                bus_name: bus_name.to_string(),
                identity: "Seeded".to_string(),
                ..Player::default()
            },
        );
        state.publish().await;
        cancel
    }

    /// The `ListNames` re-read must remove a player that is no longer on the
    /// bus, not just add ones that are. A player that quit during a bus gap
    /// is never re-announced (the broker only announces changes) and its
    /// liveness watcher cannot see it either (`Proxy::new_owned` succeeds for
    /// an unowned name, so the rebuilt proxy reports `Live`) — so this reply
    /// is the only notice the service will ever get, and before the review's
    /// fix it was ignored. #1197's per-player re-read then repainted the
    /// surviving row blank on every later marker.
    ///
    /// Falsifiable: deleting the prune loop in `reconcile_players` leaves the
    /// row published and the teardown signal unsent.
    #[tokio::test(flavor = "current_thread")]
    async fn a_departed_player_is_pruned_and_its_tasks_torn_down() {
        let players = Mutable::new(Vec::new());
        let state = State::new(players.clone(), Mutable::new(true));
        let (spawner, spawns) = counting_spawner();
        let name = format!("{PLAYER_NAME_PREFIX}x");

        let mut cancel = seed_player(&state, &name).await;
        assert_eq!(players.get_cloned().len(), 1, "seeded row is published");

        // The re-read's reply does not mention it any more.
        let tracked_before = state.tracked().await;
        reconcile_players(&state, &[], &tracked_before, &spawner).await;

        assert!(
            players.get_cloned().is_empty(),
            "a player absent from ListNames must lose its row"
        );
        assert!(state.tracked().await.is_empty(), "and its tracking entry");
        assert!(
            state.map.lock().await.is_empty(),
            "and its cached properties, so a later re-read cannot resurrect it"
        );
        assert!(
            state.cancels.lock().await.is_empty(),
            "and its teardown channel"
        );
        // Every per-player task awaits exactly this, so its resolving is what
        // "the subscription, the poller and the liveness watcher are gone"
        // means here.
        tokio::time::timeout(Duration::from_secs(1), cancelled(&mut cancel))
            .await
            .expect("unregistering a player must cancel its tasks");
        assert_eq!(spawns.load(Ordering::SeqCst), 0, "nothing to spawn");
    }

    /// The teardown signal is only worth anything if the tasks honour it, and
    /// the `PropertiesChanged` watcher is the one that matters: its markers
    /// keep arriving for the life of the process, and since #1197 each one
    /// re-reads and re-publishes the player — which is what turned a stale
    /// row into a blank one. Driven over a stream that never yields, so
    /// cancellation is the only thing that can end it.
    ///
    /// Falsifiable: deleting the `cancelled` arm from `drive_property_items`
    /// makes this hang until the timeout.
    #[tokio::test(flavor = "current_thread")]
    async fn unregistering_stops_the_properties_watcher() {
        let state = State::new(Mutable::new(Vec::new()), Mutable::new(true));
        let name = format!("{PLAYER_NAME_PREFIX}x");
        let cancel = seed_player(&state, &name).await;

        let both = async {
            tokio::join!(
                drive_property_items(
                    &state,
                    &name,
                    futures_util::stream::pending::<SignalItem>(),
                    cancel
                ),
                state.unregister(&name),
            );
        };
        tokio::time::timeout(Duration::from_secs(1), both)
            .await
            .expect("the properties watcher must exit when its player is unregistered");
    }

    /// The prune is scoped to the snapshot taken *before* `ListNames` was
    /// asked, because a player the NOC arm discovered while that round trip
    /// was in flight is missing from the reply too — and dropping it would be
    /// the very bug the re-read exists to fix.
    ///
    /// Falsifiable: pruning against the live `order` instead of
    /// `tracked_before` drops the newcomer.
    #[tokio::test(flavor = "current_thread")]
    async fn a_player_that_appeared_during_the_round_trip_survives_the_prune() {
        let players = Mutable::new(Vec::new());
        let state = State::new(players.clone(), Mutable::new(true));
        let (spawner, _spawns) = counting_spawner();

        // Snapshot first (empty), then the newcomer registers, then the reply
        // — which predates it — comes back.
        let tracked_before = state.tracked().await;
        let name = format!("{PLAYER_NAME_PREFIX}late");
        let _cancel = seed_player(&state, &name).await;

        reconcile_players(&state, &[], &tracked_before, &spawner).await;

        assert_eq!(
            state.tracked().await,
            vec![name],
            "a player discovered during the round trip must not be pruned by it"
        );
        assert_eq!(players.get_cloned().len(), 1);
    }

    /// A player still on the bus keeps its row and does not gain a second
    /// task set — the two halves of the reconcile must not fight each other.
    #[tokio::test(flavor = "current_thread")]
    async fn a_present_player_is_neither_pruned_nor_respawned() {
        let players = Mutable::new(Vec::new());
        let state = State::new(players.clone(), Mutable::new(true));
        let (spawner, spawns) = counting_spawner();
        let name = format!("{PLAYER_NAME_PREFIX}x");

        let _cancel = seed_player(&state, &name).await;
        let tracked_before = state.tracked().await;

        reconcile_players(
            &state,
            &[
                "org.freedesktop.DBus".to_string(),
                name.clone(),
                "org.example.NotAPlayer".to_string(),
            ],
            &tracked_before,
            &spawner,
        )
        .await;

        assert_eq!(state.tracked().await, vec![name]);
        assert_eq!(players.get_cloned().len(), 1);
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            0,
            "already watched, and a non-MPRIS name is not a player"
        );
    }

    /// A player that goes away and comes back is a *new* task set: the gate
    /// must not turn into a one-shot latch that leaves a returning player
    /// unwatched.
    #[tokio::test(flavor = "current_thread")]
    async fn a_returning_player_spawns_again() {
        let state = State::new(Mutable::new(Vec::new()), Mutable::new(true));
        let (spawner, spawns) = counting_spawner();
        let name = format!("{PLAYER_NAME_PREFIX}y");

        let items = futures_util::stream::iter(vec![
            noc(&name, ":1.9"),
            noc(&name, ""), // released
            noc(&name, ":1.11"),
        ]);
        run_owner_change_loop(&state, items, &spawner, || async { Ok(()) }).await;

        assert_eq!(spawns.load(Ordering::SeqCst), 2);
        assert_eq!(state.order.lock().await.len(), 1);
    }
}
