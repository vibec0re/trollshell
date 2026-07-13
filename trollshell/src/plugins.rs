//! Out-of-process widget-plugin **host transport** (frontend B; #35 PR 2, on
//! the #266 wire protocol and the #199 reconciler).
//!
//! The host **listens** on one same-user socket
//! (`$XDG_RUNTIME_DIR/trollshell/plugin.sock`); plugins are systemd user units
//! that dial in and self-identify with [`PluginMsg::Register`]. Supervision is
//! systemd's job — a crash is just a disconnect. This module is the transport
//! only: it does **not** spawn or supervise plugins (see the spec's topology).
//!
//! ## Topology & threading
//!
//! The UDS listener and every per-plugin read/write task run on the process-wide
//! tokio runtime (`runtime::handle()`). The reconciler, the effect broker, and
//! the clock pump run on the GTK main thread. The two sides are bridged **only**
//! through [`futures_signals::signal::Mutable`] (tokio→GTK render mailbox) and a
//! [`tokio::sync::watch`] channel (GTK→tokio clock state) — never a shared
//! `Arc<Mutex>` threaded through widgets.
//!
//! ```text
//! plugin ──Register/Render/Log/Pong──▶ per-conn reader task ──▶ region mailbox (Mutable<Vec<..>>)
//!                                                              ──▶ effect channel (mpsc)
//!                                                              ──▶ tracing / liveness
//! GTK region ◀── region mailbox ── one reconciler per plugin card, maps wire::Node → hytte_ui::Node
//! GTK effect broker ◀── effect channel ── drained in arrival order (non-lossy)
//! GTK reconciler ──on_event──▶ per-conn outbound (mpsc) ──▶ writer task ──▶ plugin (Event)
//! GTK clock pump ──▶ watch ──▶ per-conn snapshot task ──▶ writer task ──▶ plugin (StateSnapshot)
//! GTK sidebar open/close ──▶ watch ──▶ per-conn visibility task ──▶ writer task ──▶ plugin (SlotVisibility)
//! ```
//!
//! ## Slot visibility (park pollers while hidden) (#288)
//!
//! A migrated poller (e.g. a departures plugin) should idle while its card
//! isn't on screen, the way the shell already gates its built-in pollers. The
//! host pushes a [`HostMsg::SlotVisibility`] on every sidebar open/close and
//! **once at register** (so a reconnecting plugin starts correct) — mirroring the
//! clock pump's `watch` + seed-send shape. Delivery is latest-wins (it's state,
//! not a one-shot event, so no #277 lossiness concern).
//!
//! The push is **opt-in via the manifest**: it goes only to a connection that
//! subscribes [`StateKey::SlotVisible`] (#305), exactly like the `Clock`
//! snapshot. #294 originally sent it to *every* connection, which crash-looped
//! plugins built against a pre-#294 proto that can't decode the variant — so
//! visibility now obeys the same state-subset rule as every other host→plugin
//! push (see the crate-level compat rule: a new push must be opt-in, never
//! unconditional). A plugin that wants gating declares the subscription
//! (`hytte-plugin-departures` does); one that doesn't (weather/pet/clock-demo)
//! simply never receives the frame.
//!
//! Sidebars are **per-monitor** and a plugin's card mirrors onto every monitor's
//! sidebar region, so `visible` is the **OR across monitors**: `true` while *any*
//! sidebar is open, `false` only once all are closed. The GTK side keeps a
//! per-monitor open flag ([`SLOT_VISIBILITY_BY_MONITOR`], fed by `sidebar.rs` via
//! [`set_sidebar_visibility`] / [`forget_sidebar_visibility`] — the latter on
//! monitor hot-unplug, so a disappearing monitor that held the only open sidebar
//! drops `visible` to `false`) and publishes the aggregate ([`any_sidebar_open`])
//! only when it actually changes.
//!
//! ## Mount regions (#274)
//!
//! Each sidebar mount is a **region** holding N plugin cards, not one slot for a
//! single plugin: the region mailbox is a `Mutable<Vec<SlotRender>>`, one entry
//! per connected plugin id, kept sorted by `(manifest.order, plugin_id)`
//! ascending (a manifest `order: Option<i32>`; `None` sorts as `0`, ties break
//! on the stable id). The GTK side mounts **one reconciler per plugin card** in
//! a vertical container, so plugins on the same mount coexist (pet + clock-demo
//! no longer need `Conflicts=`) and each card joins / updates / reorders /
//! leaves without disturbing its siblings.
//!
//! ## Coalescing (latest-wins) — trees only
//!
//! The view bridges coalesce, per the spec's "always accept new view state, no
//! deltas, latest-wins":
//! - The region mailbox is a `Mutable<Vec<SlotRender>>`; a plugin's new frame
//!   overwrites *its own* entry in place (latest-wins per plugin id), and a slow
//!   GTK consumer only ever observes the newest region snapshot (superseded
//!   frames of any plugin are dropped).
//! - The clock bridge is a `watch` channel; a per-conn task reads
//!   `borrow_and_update()` so bursts collapse to the latest `ClockState`.
//!
//! Effects are the exception: they are **one-shot**, so coalescing could drop a
//! click (#277). The reader task strips each frame's effects onto a dedicated
//! `mpsc::unbounded` channel drained on the GTK thread — non-lossy, ordered per
//! connection — instead of letting them ride the latest-wins mailbox. That
//! channel is **global** (one per host, not per region/plugin), so the region
//! change leaves the #277 exactly-once guarantee untouched.
//!
//! ## v1 scope (see the PR body for the full deferred list)
//!
//! - **Mounts:** the three sidebar regions ([`Mount::SidebarLead`] /
//!   [`Mount::SidebarTop`] / [`Mount::SidebarBottom`]); bar mounts are accepted
//!   but their renders are dropped (deferred). `SidebarLead` (#301) leads the
//!   sidebar — its cards render *above* the built-in weather/calendar/tasks
//!   cards, which `SidebarTop`/`SidebarBottom` (mounted after them) cannot.
//! - **State:** only [`StateKey::Clock`].
//! - **Effects:** only [`Effect::OpenPage`] is brokered (mapped to the modal
//!   drawer); every other effect is logged "unsupported in v1" and skipped.
//!   Capability enforcement / audit-log / the `RunCommand` round-trip are
//!   deferred.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Local};
use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::reactive::registry;
use hytte::services::clock;
use hytte::ui::{Dir as UiDir, EventKind as UiEventKind, Node as UiNode, NodeId, Reconciler};
use hytte_plugin_proto::{
    ClockState, Effect, HostMsg, LogLevel, Mount, Page, PluginMsg, StateKey, StateSnapshot,
    read_frame, socket_path, wire, write_frame,
};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};

// ── Service ─────────────────────────────────────────────────────────────────

/// The plugin host transport service. Registered in `main.rs` via `App::with`.
pub struct PluginsService;

/// Monotonic per-connection token. Stamped on every [`SlotRender`] a connection
/// parks so a card's ownership is **connection-scoped, not plugin-id-scoped**: a
/// fast-reconnecting plugin (the SDK backs off from 100 ms) can have its new
/// connection replace its region entry before the old connection's teardown
/// runs, and a plugin-id-only compare would let the stale teardown evict the
/// live successor (#278). The generation compare cannot — each connection has a
/// unique token, so teardown removes a card only when the *same connection*
/// still owns it (see [`clear_region_if_owned`]).
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);

/// One plugin's rendered card parked in a mount region's coalescing mailbox: the
/// producing plugin's `id` and requested `order` (region sort key), the
/// declarative `tree`, the [`NEXT_GENERATION`] token of the connection that
/// produced it, and a handle to send frames **back** to that connection (event
/// round-trip). Effects do **not** ride here — they are one-shot, so they go
/// down the non-lossy effect channel instead (#277). `Clone` so it can ride a
/// `Mutable` signal to the GTK reconcilers.
#[derive(Clone)]
struct SlotRender {
    plugin_id: String,
    order: i32,
    generation: u64,
    tree: wire::Node,
    outbound: mpsc::UnboundedSender<HostMsg>,
}

/// One effect stripped off a render frame, queued on the non-lossy effect
/// channel for the GTK-side broker. Carries the producing plugin's id purely
/// for the audit log.
struct BrokeredEffect {
    plugin_id: String,
    effect: Effect,
}

/// Registry handles for the plugin host. The three sidebar render mailboxes are
/// written from tokio (a plugin's reader task) and read on the GTK thread (the
/// reconcilers). `clock_tx` is written on the GTK thread (the clock pump) and
/// subscribed from tokio (per-conn snapshot tasks). `effects_rx` is the receive
/// end of the non-lossy effect channel; [`install`] takes it once to drain the
/// broker on the GTK thread (`RefCell<Option<…>>` because the registry is
/// thread-local to that thread and the receiver is single-consumer).
#[doc(hidden)]
pub struct PluginHandles {
    sidebar_lead: Mutable<Vec<SlotRender>>,
    sidebar_top: Mutable<Vec<SlotRender>>,
    sidebar_bottom: Mutable<Vec<SlotRender>>,
    clock_tx: watch::Sender<Option<ClockState>>,
    /// Aggregate slot visibility (OR of every monitor's sidebar open flag),
    /// written on the GTK thread ([`set_sidebar_visibility`]) and subscribed
    /// from tokio (per-conn visibility tasks). Starts `false` (nothing open at
    /// boot). See the module-level "Slot visibility" note (#288).
    visibility_tx: watch::Sender<bool>,
    effects_rx: RefCell<Option<mpsc::UnboundedReceiver<BrokeredEffect>>>,
}

/// Clones of the shared handles handed to the tokio listener + per-conn tasks.
#[derive(Clone)]
struct ListenerCtx {
    sidebar_lead: Mutable<Vec<SlotRender>>,
    sidebar_top: Mutable<Vec<SlotRender>>,
    sidebar_bottom: Mutable<Vec<SlotRender>>,
    clock_rx: watch::Receiver<Option<ClockState>>,
    visibility_rx: watch::Receiver<bool>,
    effects_tx: mpsc::UnboundedSender<BrokeredEffect>,
}

impl Service for PluginsService {
    type Handles = PluginHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let (clock_tx, clock_rx) = watch::channel(None);
        // Slot visibility seeds `false`: no sidebar is open at boot, and each
        // monitor's `install` re-asserts `false` as it wires up (#288).
        let (visibility_tx, visibility_rx) = watch::channel(false);
        let (effects_tx, effects_rx) = mpsc::unbounded_channel();
        let handles = PluginHandles {
            sidebar_lead: Mutable::new(Vec::new()),
            sidebar_top: Mutable::new(Vec::new()),
            sidebar_bottom: Mutable::new(Vec::new()),
            clock_tx,
            visibility_tx,
            effects_rx: RefCell::new(Some(effects_rx)),
        };
        let ctx = ListenerCtx {
            sidebar_lead: handles.sidebar_lead.clone(),
            sidebar_top: handles.sidebar_top.clone(),
            sidebar_bottom: handles.sidebar_bottom.clone(),
            clock_rx,
            visibility_rx,
            effects_tx,
        };
        rt.spawn(async move {
            if let Err(e) = listen(&ctx).await {
                tracing::warn!(error = %e, "plugin host listener stopped");
            }
        });
        handles
    }
}

/// Register the plugin host transport on an `App`.
#[must_use]
pub fn service() -> PluginsService {
    PluginsService
}

// ── GTK-side install: clock pump + effect broker ─────────────────────────────

/// Wire the GTK-main-thread halves of the transport: the clock→wire pump and
/// the (single, global) effect broker per sidebar mount. Call **once** from the
/// `App::run` body after services are registered — the reconcilers themselves
/// mount per-monitor via [`sidebar_lead_slot`] / [`sidebar_top_slot`] /
/// [`sidebar_bottom_slot`].
pub fn install() {
    // Clock state pump: project the live `clock::now()` into a GTK-free wire
    // `ClockState` and publish it on the watch channel the per-conn snapshot
    // tasks subscribe to. `clock::now()` replays its current value on subscribe,
    // so a plugin that dials in later still gets an initial snapshot.
    glib::MainContext::default().spawn_local(clock::now().for_each(|dt| {
        set_clock(to_clock_state(&dt));
        std::future::ready(())
    }));

    // Effect broker: drain the non-lossy effect channel in arrival order.
    // Effects are one-shot, so — unlike the idempotent trees on the render
    // mailbox — they must never be coalesced away (#277); the reader tasks strip
    // them off each frame and queue them here. Draining on the GTK main thread
    // is exactly once per effect, regardless of how many monitors mirror the
    // tree (the mailbox fan-out never sees effects). Reconcilers render the
    // tree; they never touch effects.
    let effects_rx = registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .effects_rx
            .borrow_mut()
            .take()
    });
    if let Some(mut effects_rx) = effects_rx {
        glib::MainContext::default().spawn_local(async move {
            while let Some(brokered) = effects_rx.recv().await {
                broker_effect(&brokered.plugin_id, &brokered.effect);
            }
        });
    }
}

/// Publish the latest clock state to the per-conn snapshot tasks.
fn set_clock(cs: ClockState) {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .clock_tx
            .send_replace(Some(cs));
    });
}

// ── Slot visibility (#288): OR of every monitor's sidebar open flag ───────────

thread_local! {
    /// GTK-thread-only per-monitor sidebar open flag, keyed by connector. The OR
    /// across its values is the single `visible` bool pushed to every connected
    /// plugin: a plugin's card mirrors onto **every** monitor's sidebar region,
    /// so it is "visible" while any one sidebar is open. Fed by `sidebar.rs`
    /// through [`set_sidebar_visibility`] (open/close) and
    /// [`forget_sidebar_visibility`] (hot-unplug).
    static SLOT_VISIBILITY_BY_MONITOR: RefCell<HashMap<String, bool>> =
        RefCell::new(HashMap::new());
}

/// A plugin's card is visible iff **any** monitor's sidebar is open — the card
/// mirrors onto every monitor's sidebar region, so one open sidebar shows it.
/// (An empty map — no monitors tracked yet — is not visible.)
fn any_sidebar_open(open_by_monitor: &HashMap<String, bool>) -> bool {
    open_by_monitor.values().any(|&open| open)
}

/// Record `monitor_key`'s open flag in `map`, returning the new OR-aggregate.
/// Pure so the hot-plug aggregation is unit-testable without the registry.
fn apply_open(map: &mut HashMap<String, bool>, monitor_key: &str, open: bool) -> bool {
    map.insert(monitor_key.to_owned(), open);
    any_sidebar_open(map)
}

/// Drop `monitor_key` from `map` (hot-unplug), returning the new OR-aggregate —
/// so a disappearing monitor that held the only open sidebar flips it to `false`.
/// Pure, for the same reason as [`apply_open`].
fn apply_forget(map: &mut HashMap<String, bool>, monitor_key: &str) -> bool {
    map.remove(monitor_key);
    any_sidebar_open(map)
}

/// Record a monitor's sidebar open-state and, if the OR-aggregate changed, push
/// the new [`HostMsg::SlotVisibility`] to every connected plugin. Called from
/// `sidebar.rs` on each open/close edge. GTK-thread-only.
pub fn set_sidebar_visibility(monitor_key: &str, open: bool) {
    let visible =
        SLOT_VISIBILITY_BY_MONITOR.with(|m| apply_open(&mut m.borrow_mut(), monitor_key, open));
    publish_visibility(visible);
}

/// Forget a monitor's sidebar on hot-unplug and push the recomputed aggregate.
/// The disappearing monitor's flag leaves the OR, so if it held the only open
/// sidebar `visible` correctly drops to `false`. GTK-thread-only.
pub fn forget_sidebar_visibility(monitor_key: &str) {
    let visible =
        SLOT_VISIBILITY_BY_MONITOR.with(|m| apply_forget(&mut m.borrow_mut(), monitor_key));
    publish_visibility(visible);
}

/// Push `visible` on the watch channel, but only when it differs from the last
/// published value (`send_if_modified`) — so redundant open/close churn on one
/// monitor while another stays open doesn't wake the per-conn tasks. Latest-wins
/// is fine either way (it's state, not a one-shot event).
fn publish_visibility(visible: bool) {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .visibility_tx
            .send_if_modified(|current| {
                if *current == visible {
                    false
                } else {
                    *current = visible;
                    true
                }
            });
    });
}

fn lead_render_signal() -> impl Signal<Item = Vec<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .sidebar_lead
            .signal_cloned()
    })
}

fn top_render_signal() -> impl Signal<Item = Vec<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .sidebar_top
            .signal_cloned()
    })
}

fn bottom_render_signal() -> impl Signal<Item = Vec<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .sidebar_bottom
            .signal_cloned()
    })
}

/// Map one wire [`Effect`] onto a real host command. v1 handles [`Effect::OpenPage`]
/// only (→ the modal drawer); anything else is logged and skipped. Capability
/// gating + audit-log + the `RunCommand` round-trip are deferred (see the module
/// doc / PR body).
fn broker_effect(plugin_id: &str, effect: &Effect) {
    match effect {
        Effect::OpenPage(page) => {
            let target = map_page(*page);
            tracing::info!(plugin = %plugin_id, ?target, "plugin effect: OpenPage");
            crate::modal::open_on_focused(None, target);
        }
        other => {
            tracing::warn!(plugin = %plugin_id, ?other, "plugin effect unsupported in v1; skipped");
        }
    }
}

// ── GTK-side mount: reconciler-backed sidebar regions ────────────────────────

/// The [`Mount::SidebarLead`] **region** — a vertical container of N plugin
/// cards. Built per monitor from `overlays::sidebar::build_card` and mounted at
/// the very **top** of the sidebar, above the built-in weather/calendar/tasks
/// cards, so a plugin here leads the sidebar (#301).
#[must_use]
pub fn sidebar_lead_slot() -> gtk::Widget {
    build_region(lead_render_signal())
}

/// The [`Mount::SidebarTop`] **region** — a vertical container of N plugin
/// cards. Built per monitor from `overlays::sidebar::build_card` and appended
/// above the built-in widgets.
#[must_use]
pub fn sidebar_top_slot() -> gtk::Widget {
    build_region(top_render_signal())
}

/// The [`Mount::SidebarBottom`] **region**, appended below the built-in sidebar
/// widgets.
#[must_use]
pub fn sidebar_bottom_slot() -> gtk::Widget {
    build_region(bottom_render_signal())
}

/// One plugin's mounted card within a region: its dedicated reconciler root (a
/// child of the region container), the [`Reconciler`] driving it, and the
/// outbound sender its `on_event` routes user interactions to. Keyed per plugin
/// id so a card can be updated, reordered, or removed without disturbing its
/// siblings. GTK-main-thread-only.
struct MountedCard {
    plugin_id: String,
    root: gtk::Box,
    reconciler: Reconciler,
    /// Outbound of the connection currently owning this plugin's card, swapped
    /// on each render so events always reach the live connection.
    outbound: Rc<RefCell<Option<mpsc::UnboundedSender<HostMsg>>>>,
}

/// Build the `gtk::Box` region driven by `signal` (a mount's sorted render
/// list). Each connected plugin gets its own reconciler-backed card; the region
/// reconciles cards in on join / update / reorder / leave, keyed by plugin id.
fn build_region(signal: impl Signal<Item = Vec<SlotRender>> + 'static) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    container.add_css_class("ts-plugin-region");

    // GTK-thread-only per-plugin card state. Order here is just a lookup table;
    // widget order is enforced against the region container directly.
    let cards: Rc<RefCell<Vec<MountedCard>>> = Rc::new(RefCell::new(Vec::new()));

    let cards_for_signal = cards.clone();
    let container_for_signal = container.clone();
    let handle = glib::MainContext::default().spawn_local(signal.for_each(move |renders| {
        reconcile_region(&container_for_signal, &cards_for_signal, &renders);
        std::future::ready(())
    }));

    // Best-effort teardown: abort the render subscription when the region widget
    // is destroyed (a sidebar rebuild on hot-plug), so it stops rendering into a
    // detached container and drops its captured handles.
    container.connect_destroy(move |_| handle.abort());

    container.upcast()
}

/// Reconcile a mount region's child cards against the latest sorted plugin
/// render list. Adds a card + reconciler for a newly-joined plugin, updates &
/// reorders existing cards, and removes cards whose plugin left — each keyed by
/// plugin id, so one plugin's join/leave never disturbs a sibling's widget
/// (the per-plugin removal semantics of #274).
fn reconcile_region(
    container: &gtk::Box,
    cards: &Rc<RefCell<Vec<MountedCard>>>,
    renders: &[SlotRender],
) {
    let mut cards = cards.borrow_mut();

    // 1. Drop cards whose plugin vanished from the region (left / disconnected).
    let present: HashSet<&str> = renders.iter().map(|r| r.plugin_id.as_str()).collect();
    cards.retain(|card| {
        let keep = present.contains(card.plugin_id.as_str());
        if !keep {
            container.remove(&card.root);
        }
        keep
    });

    // 2. Upsert each plugin's card in sorted (region) order, laying the roots out
    //    to match. `prev` walks the intended sibling order.
    let mut prev: Option<gtk::Widget> = None;
    for render in renders {
        let ui_tree = to_ui_node(&render.tree);
        if let Some(idx) = cards.iter().position(|c| c.plugin_id == render.plugin_id) {
            let card = &mut cards[idx];
            // Swap in the live connection's outbound, then re-render its tree.
            *card.outbound.borrow_mut() = Some(render.outbound.clone());
            card.reconciler.render(&ui_tree);
            container.reorder_child_after(&card.root, prev.as_ref());
            prev = Some(card.root.clone().upcast());
        } else {
            // New plugin joined: its own root + reconciler, wired to its own
            // outbound cell (so events reach the right connection).
            let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
            root.add_css_class("ts-plugin-card");
            let outbound: Rc<RefCell<Option<mpsc::UnboundedSender<HostMsg>>>> =
                Rc::new(RefCell::new(Some(render.outbound.clone())));
            let ev_outbound = outbound.clone();
            let mut reconciler = Reconciler::new(&root, move |id: NodeId, kind: UiEventKind| {
                if let Some(tx) = ev_outbound.borrow().as_ref() {
                    let _ = tx.send(HostMsg::Event {
                        node: id,
                        kind: to_wire_event(kind),
                    });
                }
            });
            reconciler.render(&ui_tree);
            container.insert_child_after(&root, prev.as_ref());
            prev = Some(root.clone().upcast());
            cards.push(MountedCard {
                plugin_id: render.plugin_id.clone(),
                root,
                reconciler,
                outbound,
            });
        }
    }
}

// ── tokio-side: listener + per-connection tasks ──────────────────────────────

/// Bind the host socket and accept plugin connections forever. The path comes
/// from [`hytte_plugin_proto::socket_path`] (shared with the plugin-side
/// runtime — the one definition both ends dial/bind; `None` = same-user-only
/// by spec, no fallback). Creates the parent dir (`0700`), unlinks any stale
/// socket before bind, and tightens the socket to `0600`.
async fn listen(ctx: &ListenerCtx) -> std::io::Result<()> {
    let Some(path) = socket_path() else {
        tracing::warn!("XDG_RUNTIME_DIR unset; plugin host socket not created");
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        // Same-user only. Best-effort — the runtime dir is already 0700.
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    // A stale socket left by a previous run makes `bind` fail with EADDRINUSE.
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(&path)?;
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    tracing::info!(socket = %path.display(), "plugin host listening");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    handle_conn(stream, &ctx).await;
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "plugin host accept failed; stopping");
                return Err(e);
            }
        }
    }
}

/// Drive one plugin connection: handshake, then read frames until the peer
/// disconnects, feeding renders into the mount mailbox and pushing state
/// snapshots + events back out.
async fn handle_conn(stream: UnixStream, ctx: &ListenerCtx) {
    let (mut rd, wr) = stream.into_split();

    // Handshake: the first frame MUST be `Register`, and its proto must match
    // exactly — else drop the connection (schema skew fails loud).
    let first = match read_frame::<PluginMsg, _>(&mut rd).await {
        Ok(msg) => msg,
        Err(e) => {
            tracing::warn!(error = %e, "plugin handshake read failed; dropping");
            return;
        }
    };
    let manifest = match first {
        PluginMsg::Register { manifest } => manifest,
        other => {
            tracing::warn!(?other, "plugin's first frame was not Register; dropping");
            return;
        }
    };
    if let Err(e) = manifest.check_proto() {
        tracing::warn!(plugin = %manifest.id, error = %e, "plugin proto mismatch; dropping");
        return;
    }
    let plugin_id = manifest.id.clone();
    let mount = manifest.mount;
    // Region sort key (advisory placement request); `None` sorts as `0` (#274).
    let order = manifest.order.unwrap_or(0);
    // Unique per-connection token stamped on every card this connection parks,
    // so teardown can distinguish "still my card in the region" from "a successor
    // connection already replaced it" (#278).
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    tracing::info!(
        plugin = %plugin_id,
        ?mount,
        subscribes = ?manifest.subscribes,
        capabilities = ?manifest.capabilities,
        "plugin registered",
    );

    // Outbound writer: the single point that serializes host→plugin frames.
    let (out_tx, out_rx) = mpsc::unbounded_channel::<HostMsg>();
    let writer = tokio::spawn(writer_task(wr, out_rx));

    // Initial + on-change state snapshots (Clock only, if subscribed).
    let snapshot = manifest
        .subscribes
        .contains(&StateKey::Clock)
        .then(|| tokio::spawn(snapshot_task(ctx.clock_rx.clone(), out_tx.clone())));

    // Slot visibility (#288): seeded at register + pushed on every change — but
    // ONLY to a plugin that subscribes `StateKey::SlotVisible` (#305). #294 sent
    // this push to EVERY connection unconditionally, which broke plugins built
    // against a pre-#294 proto: their `rmp-serde` can't decode the unknown
    // `HostMsg::SlotVisibility` variant, the session dies, the SDK redials, the
    // host re-seeds → crash-loop (the out-of-tree vibectl hit exactly this). The
    // `PROTO_VERSION` exact-match can't catch it (both sides are proto 1); the
    // "appending a name-tagged variant is additive" rule only holds while old
    // code never *receives* the new variant. Gating the push behind the manifest
    // restores the design's opt-in state-subset rule: the host serializes only
    // subscribed state, so an old binary that never asked for visibility never
    // receives it. Mirrors the `Clock` snapshot gate above.
    let visibility = manifest
        .subscribes
        .contains(&StateKey::SlotVisible)
        .then(|| tokio::spawn(visibility_task(ctx.visibility_rx.clone(), out_tx.clone())));

    // Reader loop: dispatch inbound frames until the peer disconnects.
    loop {
        match read_frame::<PluginMsg, _>(&mut rd).await {
            Ok(PluginMsg::Render { tree, effects }) => {
                let region = match mount {
                    Mount::SidebarLead => Some(&ctx.sidebar_lead),
                    Mount::SidebarTop => Some(&ctx.sidebar_top),
                    Mount::SidebarBottom => Some(&ctx.sidebar_bottom),
                    Mount::BarLeft | Mount::BarCenter | Mount::BarRight => {
                        tracing::debug!(plugin = %plugin_id, ?mount, "bar mount unsupported in v1; render dropped");
                        None
                    }
                };
                if let Some(region) = region {
                    // One-shot effects first, over the (global) non-lossy channel,
                    // BEFORE parking the idempotent tree — a superseding render
                    // frame could otherwise coalesce this frame's click away
                    // (#277). Unchanged by the region model.
                    for effect in effects {
                        let _ = ctx.effects_tx.send(BrokeredEffect {
                            plugin_id: plugin_id.clone(),
                            effect,
                        });
                    }
                    // Latest-wins per plugin id: upsert overwrites *this* plugin's
                    // card in place, leaving siblings alone (#274).
                    upsert_region(
                        region,
                        SlotRender {
                            plugin_id: plugin_id.clone(),
                            order,
                            generation,
                            tree,
                            outbound: out_tx.clone(),
                        },
                    );
                }
            }
            Ok(PluginMsg::Register { .. }) => {
                tracing::warn!(plugin = %plugin_id, "duplicate Register ignored");
            }
            Ok(PluginMsg::Log { level, msg }) => log_plugin(&plugin_id, level, &msg),
            Ok(PluginMsg::Pong { seq }) => tracing::trace!(plugin = %plugin_id, seq, "plugin pong"),
            Err(e) => {
                tracing::info!(plugin = %plugin_id, reason = %e, "plugin disconnected");
                break;
            }
        }
    }

    // Teardown: remove THIS plugin's card from its region only if THIS connection
    // still owns it (removes just that card on the GTK side), then stop the
    // outbound + snapshot tasks. Connection-scoped + keyed per plugin id, so a
    // fast-reconnect successor is never evicted and sibling plugins are untouched.
    clear_region_if_owned(&ctx.sidebar_lead, &plugin_id, generation);
    clear_region_if_owned(&ctx.sidebar_top, &plugin_id, generation);
    clear_region_if_owned(&ctx.sidebar_bottom, &plugin_id, generation);
    if let Some(snapshot) = snapshot {
        snapshot.abort();
    }
    if let Some(visibility) = visibility {
        visibility.abort();
    }
    writer.abort();
}

/// Serialize host→plugin frames pulled off the outbound channel until the
/// channel closes or a write fails.
async fn writer_task(mut wr: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<HostMsg>) {
    while let Some(msg) = rx.recv().await {
        if let Err(e) = write_frame(&mut wr, &msg).await {
            tracing::debug!(error = %e, "plugin outbound write failed; closing writer");
            break;
        }
    }
}

/// Push the full subscribed state subset (v1: `clock`) on the initial subscribe
/// and on every change, coalescing bursts latest-wins via `borrow_and_update`.
async fn snapshot_task(
    mut clock_rx: watch::Receiver<Option<ClockState>>,
    out: mpsc::UnboundedSender<HostMsg>,
) {
    // Initial snapshot (the watch replays its current value).
    let initial = clock_rx.borrow_and_update().clone();
    if out
        .send(HostMsg::StateSnapshot {
            snapshot: StateSnapshot { clock: initial },
        })
        .is_err()
    {
        return;
    }
    while clock_rx.changed().await.is_ok() {
        let clock = clock_rx.borrow_and_update().clone();
        if out
            .send(HostMsg::StateSnapshot {
                snapshot: StateSnapshot { clock },
            })
            .is_err()
        {
            break;
        }
    }
}

/// Push the aggregate slot visibility on the initial subscribe (the register
/// seed, so a reconnecting plugin starts in the right state) and on every
/// change, coalescing bursts latest-wins via `borrow_and_update` (#288). Mirrors
/// [`snapshot_task`]; spawned **only** for a connection that subscribes
/// [`StateKey::SlotVisible`] (#305) — an unsubscribed plugin never receives the
/// frame.
async fn visibility_task(
    mut visibility_rx: watch::Receiver<bool>,
    out: mpsc::UnboundedSender<HostMsg>,
) {
    // Seed at register (the watch replays its current value).
    let initial = *visibility_rx.borrow_and_update();
    if out
        .send(HostMsg::SlotVisibility { visible: initial })
        .is_err()
    {
        return;
    }
    while visibility_rx.changed().await.is_ok() {
        let visible = *visibility_rx.borrow_and_update();
        if out.send(HostMsg::SlotVisibility { visible }).is_err() {
            break;
        }
    }
}

/// Insert-or-replace `render`'s plugin card in its mount region, latest-wins per
/// plugin id, keeping the region sorted by `(order, plugin_id)` ascending. A
/// plugin's repeated renders overwrite its own card (coalescing — superseded
/// trees dropped); distinct plugins get distinct cards, so they never fight
/// (#274). Called from a connection's reader task on every `Render`.
fn upsert_region(region: &Mutable<Vec<SlotRender>>, render: SlotRender) {
    let mut cards = region.lock_mut();
    if let Some(existing) = cards.iter_mut().find(|c| c.plugin_id == render.plugin_id) {
        *existing = render;
    } else {
        cards.push(render);
    }
    cards.sort_by(|a, b| (a.order, &a.plugin_id).cmp(&(b.order, &b.plugin_id)));
}

/// Remove a plugin's card from a mount region on connection teardown — but only
/// if THIS connection still owns it (its `generation` matches the parked card's).
/// A fast-reconnect successor (same plugin id, higher generation) has already
/// replaced the card, so a stale teardown leaves it (the #278 guarantee, now
/// applied per plugin-id entry). A different plugin's card is keyed by a
/// different id and never matched, so siblings are undisturbed.
///
/// Probes with the read lock first so a teardown never spuriously notifies a
/// region this plugin isn't even in (each teardown checks *both* sidebar
/// regions); it re-finds under the write lock to stay correct against a
/// concurrent mutation.
fn clear_region_if_owned(region: &Mutable<Vec<SlotRender>>, plugin_id: &str, generation: u64) {
    let owns = region
        .lock_ref()
        .iter()
        .any(|c| c.plugin_id == plugin_id && c.generation == generation);
    if !owns {
        return;
    }
    let mut cards = region.lock_mut();
    if let Some(pos) = cards
        .iter()
        .position(|c| c.plugin_id == plugin_id && c.generation == generation)
    {
        cards.remove(pos);
    }
}

/// Surface a plugin's `Log` frame at the matching host `tracing` level.
fn log_plugin(plugin_id: &str, level: LogLevel, msg: &str) {
    match level {
        LogLevel::Error => tracing::error!(plugin = %plugin_id, "{msg}"),
        LogLevel::Warn => tracing::warn!(plugin = %plugin_id, "{msg}"),
        LogLevel::Info => tracing::info!(plugin = %plugin_id, "{msg}"),
        LogLevel::Debug => tracing::debug!(plugin = %plugin_id, "{msg}"),
        LogLevel::Trace => tracing::trace!(plugin = %plugin_id, "{msg}"),
    }
}

// ── wire ⇄ host mappings (mechanical, but exhaustive) ────────────────────────

/// Project `clock::now()`'s `DateTime<Local>` into the GTK-/chrono-free wire
/// [`ClockState`].
fn to_clock_state(dt: &DateTime<Local>) -> ClockState {
    ClockState {
        iso: dt.to_rfc3339(),
        unix: dt.timestamp(),
    }
}

/// Map a wire [`wire::Node`] onto the reconciler's `hytte_ui::Node`. The two
/// mirror each other field-for-field (#266), so this is a 1:1 recursion — but it
/// is written exhaustively so adding a node variant to either side is a compile
/// error here.
// One exhaustive arm per node variant — the length is the vocabulary size, not
// complexity.
#[allow(clippy::too_many_lines)]
fn to_ui_node(node: &wire::Node) -> UiNode {
    match node {
        wire::Node::Box {
            id,
            dir,
            spacing,
            scroll,
            classes,
            children,
        } => UiNode::Box {
            id: id.clone(),
            dir: to_ui_dir(*dir),
            spacing: *spacing,
            scroll: *scroll,
            classes: classes.clone(),
            children: children.iter().map(to_ui_node).collect(),
        },
        wire::Node::Row {
            id,
            classes,
            children,
        } => UiNode::Row {
            id: id.clone(),
            classes: classes.clone(),
            children: children.iter().map(to_ui_node).collect(),
        },
        wire::Node::ListBox {
            id,
            classes,
            children,
        } => UiNode::ListBox {
            id: id.clone(),
            classes: classes.clone(),
            children: children.iter().map(to_ui_node).collect(),
        },
        wire::Node::Label { id, text, classes } => UiNode::Label {
            id: id.clone(),
            text: text.clone(),
            classes: classes.clone(),
        },
        wire::Node::Text {
            id,
            text,
            max_width_chars,
            ellipsize,
            classes,
        } => UiNode::Text {
            id: id.clone(),
            text: text.clone(),
            max_width_chars: *max_width_chars,
            ellipsize: *ellipsize,
            classes: classes.clone(),
        },
        wire::Node::Icon { id, name, classes } => UiNode::Icon {
            id: id.clone(),
            name: name.clone(),
            classes: classes.clone(),
        },
        wire::Node::Pixels {
            id,
            width,
            height,
            data,
            classes,
        } => {
            // Validation seam: a plugin's RGBA8 buffer is untrusted, so this is
            // the one non-1:1 arm. `data.len()` MUST equal `width*height*4`; a
            // mismatch degrades to an empty (nothing-rendered) surface — id and
            // classes preserved so CSS chrome stays and a later valid frame
            // updates in place — with a `tracing::warn!`. This is the single
            // documented seam (the host is the trust boundary and the only layer
            // with `tracing`); the `hytte_ui` widget stays a silent panic-safe
            // backstop, and decode stays permissive so one bad node can't drop
            // the whole connection.
            let (width, height, data) = if pixels_len_ok(*width, *height, data.len()) {
                (*width, *height, data.clone())
            } else {
                tracing::warn!(
                    node = ?id,
                    width = *width,
                    height = *height,
                    data_len = data.len(),
                    "plugin Pixels buffer size != width*height*4; rendering nothing"
                );
                (0, 0, Vec::new())
            };
            UiNode::Pixels {
                id: id.clone(),
                width,
                height,
                data,
                classes: classes.clone(),
            }
        }
        wire::Node::Button { id, classes, child } => UiNode::Button {
            id: id.clone(),
            classes: classes.clone(),
            child: Box::new(to_ui_node(child)),
        },
        wire::Node::Progress {
            id,
            fraction,
            classes,
        } => UiNode::Progress {
            id: id.clone(),
            fraction: *fraction,
            classes: classes.clone(),
        },
        wire::Node::Slider {
            id,
            min,
            max,
            value,
            step,
            classes,
        } => UiNode::Slider {
            id: id.clone(),
            min: *min,
            max: *max,
            value: *value,
            step: *step,
            classes: classes.clone(),
        },
        wire::Node::Revealer { id, open, child } => UiNode::Revealer {
            id: id.clone(),
            open: *open,
            child: Box::new(to_ui_node(child)),
        },
        wire::Node::Separator { classes } => UiNode::Separator {
            classes: classes.clone(),
        },
        wire::Node::Spacer => UiNode::Spacer,
        wire::Node::Expander {
            id,
            header,
            children,
            expanded,
            classes,
        } => UiNode::Expander {
            id: id.clone(),
            header: Box::new(to_ui_node(header)),
            children: children.iter().map(to_ui_node).collect(),
            expanded: *expanded,
            classes: classes.clone(),
        },
    }
}

fn to_ui_dir(dir: wire::Dir) -> UiDir {
    match dir {
        wire::Dir::Horizontal => UiDir::Horizontal,
        wire::Dir::Vertical => UiDir::Vertical,
    }
}

/// Whether a [`wire::Node::Pixels`] buffer honors the RGBA8 size invariant:
/// `data_len == width * height * 4`, computed in `u64` so the product can't
/// overflow. `(0, 0, 0)` is consistent (a legitimate empty surface).
fn pixels_len_ok(width: u32, height: u32, data_len: usize) -> bool {
    let expected = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|n| n.checked_mul(4));
    expected == u64::try_from(data_len).ok()
}

/// Map a reconciler event back onto its wire form for the outbound `Event`
/// frame. Exhaustive over the `EventKind` set (Click + Scroll + `ValueChanged`),
/// so adding a kind to either side breaks the build here rather than silently
/// dropping an event.
fn to_wire_event(kind: UiEventKind) -> wire::EventKind {
    match kind {
        UiEventKind::Click => wire::EventKind::Click,
        UiEventKind::Scroll { dx, dy } => wire::EventKind::Scroll { dx, dy },
        UiEventKind::ValueChanged { value } => wire::EventKind::ValueChanged { value },
    }
}

/// Map a wire [`Page`] onto the host's `modal::Page`. The two enums mirror each
/// other; written exhaustively so a page added to either side breaks the build
/// here rather than silently mis-routing. The one exception is `Stats`: the
/// host split it into per-resource flyouts, but the wire protocol keeps a single
/// `Stats` page — a plugin opening it lands on the CPU flyout (the primary
/// stats panel).
fn map_page(page: Page) -> crate::modal::Page {
    use crate::modal::Page as M;
    match page {
        Page::Media => M::Media,
        Page::Network => M::Network,
        Page::Vpn => M::Vpn,
        Page::Connections => M::Connections,
        Page::Bluetooth => M::Bluetooth,
        Page::Stats => M::StatsCpu,
        Page::Audio => M::Audio,
        Page::Power => M::Power,
        Page::PowerMenu => M::PowerMenu,
        Page::Notifications => M::Notifications,
        Page::Appearance => M::Appearance,
        Page::Displays => M::Displays,
        Page::Clipboard => M::Clipboard,
        Page::Calendar => M::Calendar,
        Page::Settings => M::Settings,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BrokeredEffect, ClockState, Effect, HashMap, HostMsg, ListenerCtx, Mount, Mutable, Page,
        PluginMsg, SlotRender, StateKey, UiDir, UiEventKind, UiNode, UnixStream, any_sidebar_open,
        apply_forget, apply_open, clear_region_if_owned, handle_conn, map_page, mpsc,
        pixels_len_ok, read_frame, to_ui_node, to_wire_event, upsert_region, watch, wire,
        write_frame,
    };
    use hytte_plugin_proto::Manifest;

    /// The `wire`→`hytte_ui` mapping is exhaustive over every node variant
    /// (incl. `Box { scroll }` and nesting) and produces a field-for-field
    /// mirror.
    #[test]
    #[allow(clippy::too_many_lines)] // one big paired tree literal; splitting hurts readability
    fn wire_node_maps_to_ui_node_exhaustively() {
        let tree = wire::Node::Box {
            id: Some("root".into()),
            dir: wire::Dir::Vertical,
            spacing: 4,
            scroll: true,
            classes: vec!["ts-a".into()],
            children: vec![
                wire::Node::Label {
                    id: None,
                    text: "hi".into(),
                    classes: vec!["ts-l".into()],
                },
                wire::Node::Icon {
                    id: Some("i".into()),
                    name: "battery-symbolic".into(),
                    classes: vec![],
                },
                wire::Node::Pixels {
                    id: Some("px".into()),
                    width: 1,
                    height: 1,
                    data: vec![10, 20, 30, 255],
                    classes: vec!["ts-lcd".into()],
                },
                wire::Node::Button {
                    id: "b".into(),
                    classes: vec!["ts-btn".into()],
                    child: Box::new(wire::Node::Label {
                        id: None,
                        text: "go".into(),
                        classes: vec![],
                    }),
                },
                wire::Node::Progress {
                    id: None,
                    fraction: 0.5,
                    classes: vec![],
                },
                wire::Node::Slider {
                    id: "sld".into(),
                    min: 0.0,
                    max: 1.0,
                    value: 0.3,
                    step: 0.1,
                    classes: vec!["ts-slider".into()],
                },
                wire::Node::Revealer {
                    id: Some("r".into()),
                    open: true,
                    child: Box::new(wire::Node::Separator {
                        classes: vec!["ts-sep".into()],
                    }),
                },
                wire::Node::Box {
                    id: None,
                    dir: wire::Dir::Horizontal,
                    spacing: 0,
                    scroll: false,
                    classes: vec![],
                    children: vec![],
                },
                wire::Node::Spacer,
            ],
        };
        let expected = UiNode::Box {
            id: Some("root".into()),
            dir: UiDir::Vertical,
            spacing: 4,
            scroll: true,
            classes: vec!["ts-a".into()],
            children: vec![
                UiNode::Label {
                    id: None,
                    text: "hi".into(),
                    classes: vec!["ts-l".into()],
                },
                UiNode::Icon {
                    id: Some("i".into()),
                    name: "battery-symbolic".into(),
                    classes: vec![],
                },
                UiNode::Pixels {
                    id: Some("px".into()),
                    width: 1,
                    height: 1,
                    data: vec![10, 20, 30, 255],
                    classes: vec!["ts-lcd".into()],
                },
                UiNode::Button {
                    id: "b".into(),
                    classes: vec!["ts-btn".into()],
                    child: Box::new(UiNode::Label {
                        id: None,
                        text: "go".into(),
                        classes: vec![],
                    }),
                },
                UiNode::Progress {
                    id: None,
                    fraction: 0.5,
                    classes: vec![],
                },
                UiNode::Slider {
                    id: "sld".into(),
                    min: 0.0,
                    max: 1.0,
                    value: 0.3,
                    step: 0.1,
                    classes: vec!["ts-slider".into()],
                },
                UiNode::Revealer {
                    id: Some("r".into()),
                    open: true,
                    child: Box::new(UiNode::Separator {
                        classes: vec!["ts-sep".into()],
                    }),
                },
                UiNode::Box {
                    id: None,
                    dir: UiDir::Horizontal,
                    spacing: 0,
                    scroll: false,
                    classes: vec![],
                    children: vec![],
                },
                UiNode::Spacer,
            ],
        };
        assert_eq!(to_ui_node(&tree), expected);
    }

    /// The list nodes map field-for-field: `Row`/`ListBox` recurse their
    /// children like `Box`, and `Text` carries `max_width_chars` **and** the
    /// #297 `ellipsize` flag. A `Spacer` between the cluster and the value maps
    /// 1:1 (the justification primitive).
    #[test]
    fn wire_row_listbox_text_map_to_ui() {
        let tree = wire::Node::ListBox {
            id: Some("list".into()),
            classes: vec!["ts-list".into()],
            children: vec![wire::Node::Row {
                id: Some("r0".into()),
                classes: vec!["ts-row".into()],
                children: vec![
                    wire::Node::Text {
                        id: None,
                        text: "an ellipsized destination".into(),
                        max_width_chars: Some(20),
                        ellipsize: true,
                        classes: vec!["ts-dest".into()],
                    },
                    wire::Node::Spacer,
                    wire::Node::Label {
                        id: None,
                        text: "12:30".into(),
                        classes: vec!["ts-time".into()],
                    },
                ],
            }],
        };
        let expected = UiNode::ListBox {
            id: Some("list".into()),
            classes: vec!["ts-list".into()],
            children: vec![UiNode::Row {
                id: Some("r0".into()),
                classes: vec!["ts-row".into()],
                children: vec![
                    UiNode::Text {
                        id: None,
                        text: "an ellipsized destination".into(),
                        max_width_chars: Some(20),
                        ellipsize: true,
                        classes: vec!["ts-dest".into()],
                    },
                    UiNode::Spacer,
                    UiNode::Label {
                        id: None,
                        text: "12:30".into(),
                        classes: vec!["ts-time".into()],
                    },
                ],
            }],
        };
        assert_eq!(to_ui_node(&tree), expected);
    }

    /// The #333 `Expander` maps 1:1: the boxed `header` and the body `children`
    /// recurse, and the `expanded` mutable prop carries across.
    #[test]
    fn wire_expander_maps_to_ui() {
        let tree = wire::Node::Expander {
            id: "room".into(),
            header: Box::new(wire::Node::Label {
                id: None,
                text: "Living Room".into(),
                classes: vec!["heading".into()],
            }),
            children: vec![wire::Node::Label {
                id: Some("d".into()),
                text: "Lamp".into(),
                classes: vec![],
            }],
            expanded: true,
            classes: vec!["boxed-list".into()],
        };
        let expected = UiNode::Expander {
            id: "room".into(),
            header: Box::new(UiNode::Label {
                id: None,
                text: "Living Room".into(),
                classes: vec!["heading".into()],
            }),
            children: vec![UiNode::Label {
                id: Some("d".into()),
                text: "Lamp".into(),
                classes: vec![],
            }],
            expanded: true,
            classes: vec!["boxed-list".into()],
        };
        assert_eq!(to_ui_node(&tree), expected);
    }

    #[test]
    fn ui_event_maps_to_wire_event() {
        assert_eq!(to_wire_event(UiEventKind::Click), wire::EventKind::Click);
        assert_eq!(
            to_wire_event(UiEventKind::Scroll { dx: 1.5, dy: -2.0 }),
            wire::EventKind::Scroll { dx: 1.5, dy: -2.0 }
        );
        assert_eq!(
            to_wire_event(UiEventKind::ValueChanged { value: 0.42 }),
            wire::EventKind::ValueChanged { value: 0.42 }
        );
    }

    /// Every wire `Page` maps to the identically-named `modal::Page`, except
    /// the single wire `Stats` page which lands on the host's CPU stats flyout.
    #[test]
    fn wire_page_maps_to_modal_page() {
        use crate::modal::Page as M;
        let cases = [
            (Page::Media, M::Media),
            (Page::Network, M::Network),
            (Page::Vpn, M::Vpn),
            (Page::Connections, M::Connections),
            (Page::Bluetooth, M::Bluetooth),
            (Page::Stats, M::StatsCpu),
            (Page::Audio, M::Audio),
            (Page::Power, M::Power),
            (Page::PowerMenu, M::PowerMenu),
            (Page::Notifications, M::Notifications),
            (Page::Appearance, M::Appearance),
            (Page::Displays, M::Displays),
            (Page::Clipboard, M::Clipboard),
            (Page::Calendar, M::Calendar),
            (Page::Settings, M::Settings),
        ];
        for (wire_page, modal_page) in cases {
            assert_eq!(map_page(wire_page), modal_page);
        }
    }

    /// One plugin card carrying an id/order/generation + a label tree, for the
    /// region tests below.
    fn render_of(
        plugin_id: &str,
        order: i32,
        generation: u64,
        text: &str,
        tx: &mpsc::UnboundedSender<HostMsg>,
    ) -> SlotRender {
        SlotRender {
            plugin_id: plugin_id.to_owned(),
            order,
            generation,
            tree: wire::Node::Label {
                id: None,
                text: text.to_owned(),
                classes: vec![],
            },
            outbound: tx.clone(),
        }
    }

    fn label_text(render: &SlotRender) -> &str {
        match &render.tree {
            wire::Node::Label { text, .. } => text,
            other => panic!("expected a Label, got {other:?}"),
        }
    }

    /// A region keeps **one card per plugin id** (a plugin's re-render coalesces
    /// its own card, latest-wins) and stays sorted by `(order, plugin_id)`.
    #[test]
    fn upsert_region_coalesces_per_plugin_and_sorts() {
        let (tx, _rx) = mpsc::unbounded_channel::<HostMsg>();
        let region: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());
        // Arrive out of order; "alpha" renders twice.
        upsert_region(&region, render_of("bravo", 0, 0, "b1", &tx));
        upsert_region(&region, render_of("alpha", 0, 0, "a1", &tx));
        upsert_region(&region, render_of("alpha", 0, 0, "a2", &tx));

        let cards = region.lock_ref();
        assert_eq!(cards.len(), 2, "one card per plugin id (alpha coalesced)");
        assert_eq!(cards[0].plugin_id, "alpha", "sorted by (order, id)");
        assert_eq!(cards[1].plugin_id, "bravo");
        assert_eq!(label_text(&cards[0]), "a2", "alpha's latest tree wins");
    }

    /// `(order, id)` ordering: lower `order` first; `None` (mapped to `0` by the
    /// reader) ties with `order: 0` and breaks on the stable id.
    #[test]
    fn region_orders_by_order_then_id() {
        let (tx, _rx) = mpsc::unbounded_channel::<HostMsg>();
        let region: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());
        // pet requests order 5 (renders later); clock had no order → 0; aaa → 0.
        upsert_region(&region, render_of("pet", 5, 0, "", &tx));
        upsert_region(&region, render_of("clock", 0, 1, "", &tx)); // None → 0
        upsert_region(&region, render_of("aaa", 0, 2, "", &tx)); // ties clock on order

        let ids: Vec<String> = region
            .lock_ref()
            .iter()
            .map(|c| c.plugin_id.clone())
            .collect();
        // (0,"aaa") < (0,"clock") < (5,"pet")
        assert_eq!(ids, vec!["aaa", "clock", "pet"]);
    }

    /// A `Pixels` node whose buffer size violates `width*height*4` must not
    /// reach the widget: the host validation seam degrades it to an empty
    /// (0×0, no data) surface, preserving id + classes so a later valid frame
    /// updates in place. A well-formed buffer passes through 1:1.
    #[test]
    fn pixels_bad_len_degrades_to_empty_surface() {
        assert!(pixels_len_ok(2, 2, 16));
        assert!(!pixels_len_ok(2, 2, 15));
        // Overflow-safe: absurd dims against a small buffer just report false.
        assert!(!pixels_len_ok(u32::MAX, u32::MAX, 4));

        let bad = wire::Node::Pixels {
            id: Some("lcd".into()),
            width: 2,
            height: 2,
            data: vec![0, 1, 2], // 3 bytes, needs 16
            classes: vec!["ts-lcd".into()],
        };
        assert_eq!(
            to_ui_node(&bad),
            UiNode::Pixels {
                id: Some("lcd".into()),
                width: 0,
                height: 0,
                data: vec![],
                classes: vec!["ts-lcd".into()],
            },
            "malformed Pixels degrades to a nothing-rendered surface",
        );

        let good = wire::Node::Pixels {
            id: None,
            width: 1,
            height: 2,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8], // 1*2*4
            classes: vec![],
        };
        assert_eq!(
            to_ui_node(&good),
            UiNode::Pixels {
                id: None,
                width: 1,
                height: 2,
                data: vec![1, 2, 3, 4, 5, 6, 7, 8],
                classes: vec![],
            },
            "well-formed Pixels passes through 1:1",
        );
    }

    /// #277 (preserved under the region model): a plugin's back-to-back frames
    /// coalesce its region card latest-wins, but a one-shot effect bundled on the
    /// superseded frame rides the dedicated **global** non-lossy channel and is
    /// delivered exactly once — not dropped, not duplicated.
    #[test]
    fn effects_survive_region_coalescing() {
        let (tx, _rx) = mpsc::unbounded_channel::<HostMsg>();
        let (eff_tx, mut eff_rx) = mpsc::unbounded_channel::<BrokeredEffect>();
        let region: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());

        // Frame A: a click's effect goes on the effect channel, then the tree is
        // parked. Frame B (a tick microseconds later) coalesces A's tree away.
        eff_tx
            .send(BrokeredEffect {
                plugin_id: "p".into(),
                effect: Effect::OpenPage(Page::PowerMenu),
            })
            .expect("effect queued");
        upsert_region(&region, render_of("p", 0, 0, "A", &tx));
        upsert_region(&region, render_of("p", 0, 0, "B", &tx));

        // The region observes only B for plugin p (load-shedding by design)…
        {
            let cards = region.lock_ref();
            assert_eq!(cards.len(), 1);
            assert_eq!(label_text(&cards[0]), "B");
        }
        // …but the effect survived, exactly once, in order.
        let got = eff_rx.try_recv().expect("effect not dropped by coalescing");
        assert_eq!(got.plugin_id, "p");
        assert!(matches!(got.effect, Effect::OpenPage(Page::PowerMenu)));
        assert!(eff_rx.try_recv().is_err(), "effect must not be duplicated");
    }

    /// #274 removal semantics: a plugin's teardown removes only *its own* card;
    /// a sibling plugin's card is keyed by a different id and stays put.
    #[test]
    fn per_plugin_teardown_leaves_siblings() {
        let (tx, _rx) = mpsc::unbounded_channel::<HostMsg>();
        let region: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());
        upsert_region(&region, render_of("a", 0, 10, "", &tx));
        upsert_region(&region, render_of("b", 0, 11, "", &tx));

        clear_region_if_owned(&region, "a", 10);

        let cards = region.lock_ref();
        assert_eq!(cards.len(), 1, "only plugin a's card removed");
        assert_eq!(cards[0].plugin_id, "b", "sibling b undisturbed");
    }

    /// #278 (preserved, now per plugin-id entry): a stale teardown (older
    /// generation) must never evict a fast-reconnect successor of the SAME
    /// plugin id; only the owning generation's own teardown clears the card.
    #[test]
    fn stale_teardown_never_evicts_same_id_successor() {
        let (tx, _rx) = mpsc::unbounded_channel::<HostMsg>();
        let region: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());
        // conn gen 1 parks plugin p; a fast reconnect (gen 2) replaces its card.
        upsert_region(&region, render_of("p", 0, 1, "v1", &tx));
        upsert_region(&region, render_of("p", 0, 2, "v2", &tx));

        // The old connection's teardown (gen 1) must NOT evict the gen-2 card.
        clear_region_if_owned(&region, "p", 1);
        {
            let cards = region.lock_ref();
            assert_eq!(cards.len(), 1);
            assert_eq!(cards[0].generation, 2, "successor survives stale teardown");
            assert_eq!(label_text(&cards[0]), "v2");
        }

        // The owning connection's own teardown (gen 2) does clear it.
        clear_region_if_owned(&region, "p", 2);
        assert!(
            region.lock_ref().is_empty(),
            "owning teardown clears the card"
        );
    }

    /// #288: slot visibility is the **OR across monitors** — a plugin's card
    /// mirrors onto every monitor's sidebar, so it's visible while any one is
    /// open. Walks the multi-monitor open/close lifecycle through the pure
    /// aggregation helpers (`apply_open` returns the recomputed aggregate).
    #[test]
    fn slot_visibility_is_or_across_monitors() {
        let mut map = HashMap::new();
        // No monitors tracked yet → not visible.
        assert!(!any_sidebar_open(&map));

        // Two monitors install, both closed → not visible.
        assert!(!apply_open(&mut map, "DP-1", false));
        assert!(!apply_open(&mut map, "HDMI-A-1", false));

        // Open one → visible (OR); the other opening too stays visible.
        assert!(apply_open(&mut map, "DP-1", true));
        assert!(apply_open(&mut map, "HDMI-A-1", true));

        // Close one while the other stays open → still visible.
        assert!(apply_open(&mut map, "DP-1", false));
        // Close the last open sidebar → not visible.
        assert!(!apply_open(&mut map, "HDMI-A-1", false));
    }

    /// #288: hot-unplug of the monitor holding the **only** open sidebar must
    /// drop visibility to `false` — its flag leaves the OR entirely, it isn't
    /// merely set closed.
    #[test]
    fn hot_unplug_of_only_open_monitor_drops_visibility() {
        let mut map = HashMap::new();
        apply_open(&mut map, "DP-1", true);
        apply_open(&mut map, "HDMI-A-1", false);
        assert!(any_sidebar_open(&map), "one open sidebar → visible");

        // The monitor with the only open sidebar disappears → visibility drops.
        assert!(!apply_forget(&mut map, "DP-1"));
        // Forgetting the remaining (closed) monitor leaves it not visible + empty.
        assert!(!apply_forget(&mut map, "HDMI-A-1"));
        assert!(map.is_empty(), "forgotten monitors leave no stale entries");
    }

    /// A bar mount is a real wire variant the reader must handle; assert the
    /// three sidebar mounts we support are distinct from each other and from the
    /// deferred bar mounts.
    #[test]
    fn sidebar_and_bar_mounts_are_distinct() {
        assert_ne!(Mount::SidebarLead, Mount::SidebarTop);
        assert_ne!(Mount::SidebarLead, Mount::SidebarBottom);
        assert_ne!(Mount::SidebarTop, Mount::BarLeft);
        assert_ne!(Mount::SidebarBottom, Mount::BarCenter);
        // Effect + StateKey are exercised elsewhere; touch them here so the test
        // module's imports stay honest if the broker/pump code is refactored.
        assert_ne!(StateKey::Clock, StateKey::SlotVisible);
        assert!(matches!(Effect::OpenPage(Page::Media), Effect::OpenPage(_)));
    }

    /// #301 teardown isolation across the **three** sidebar regions: a plugin's
    /// teardown probes all three (`handle_conn` calls `clear_region_if_owned` on
    /// each), and clearing the region it actually lives in leaves the other two
    /// regions' cards untouched. Mirrors the two-region isolation guarantees on
    /// the new lead region.
    #[test]
    fn teardown_is_isolated_across_the_three_regions() {
        let (tx, _rx) = mpsc::unbounded_channel::<HostMsg>();
        let lead: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());
        let top: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());
        let bottom: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());
        upsert_region(&lead, render_of("weather", -10, 0, "", &tx));
        upsert_region(&top, render_of("pet", 0, 1, "", &tx));
        upsert_region(&bottom, render_of("departures", 0, 2, "", &tx));

        // Weather (lead region) tears down: `handle_conn` probes every region.
        clear_region_if_owned(&lead, "weather", 0);
        clear_region_if_owned(&top, "weather", 0);
        clear_region_if_owned(&bottom, "weather", 0);

        assert!(lead.lock_ref().is_empty(), "weather's lead card removed");
        assert_eq!(top.lock_ref()[0].plugin_id, "pet", "pet (top) untouched");
        assert_eq!(
            bottom.lock_ref()[0].plugin_id,
            "departures",
            "departures (bottom) untouched"
        );
    }

    // ── Host session gating (#305): the SlotVisibility push is opt-in ─────────

    /// Read one host→plugin frame, failing (not hanging) if none arrives.
    async fn recv<R>(rd: &mut R) -> HostMsg
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_frame::<HostMsg, _>(rd),
        )
        .await
        .expect("a host frame within 5s")
        .expect("decode HostMsg")
    }

    fn ctx_with(
        clock_rx: watch::Receiver<Option<ClockState>>,
        visibility_rx: watch::Receiver<bool>,
    ) -> (ListenerCtx, mpsc::UnboundedReceiver<BrokeredEffect>) {
        let (effects_tx, effects_rx) = mpsc::unbounded_channel();
        let ctx = ListenerCtx {
            sidebar_lead: Mutable::new(Vec::new()),
            sidebar_top: Mutable::new(Vec::new()),
            sidebar_bottom: Mutable::new(Vec::new()),
            clock_rx,
            visibility_rx,
            effects_tx,
        };
        (ctx, effects_rx)
    }

    /// #305: a connection that does **not** subscribe `SlotVisible` must never
    /// receive a `SlotVisibility` frame — not the register seed, not an edge. The
    /// `Clock` snapshot is the ordered control channel: the plugin subscribes
    /// Clock only, so its only frames are clock snapshots; a visibility edge
    /// driven mid-stream produces nothing, and the very next frame the plugin
    /// sees is the following clock snapshot — proving the edge was *filtered*, not
    /// merely late. (This is the vibectl crash-loop, prevented.)
    #[tokio::test]
    async fn visibility_push_gated_off_when_not_subscribed() {
        let (clock_tx, clock_rx) = watch::channel(Some(ClockState {
            iso: "t0".into(),
            unix: 0,
        }));
        let (vis_tx, vis_rx) = watch::channel(false);
        let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);

        let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
        tokio::spawn(async move { handle_conn(host_end, &ctx).await });

        let (mut prd, mut pwr) = plugin_end.into_split();
        // A legacy-shaped plugin: subscribes Clock, NOT SlotVisible.
        let mut manifest = Manifest::new("legacy", Mount::SidebarTop);
        manifest.subscribes = vec![StateKey::Clock];
        write_frame(&mut pwr, &PluginMsg::Register { manifest })
            .await
            .expect("send Register");

        // Seed frame is the clock snapshot (the only subscription) — not visibility.
        assert!(
            matches!(recv(&mut prd).await, HostMsg::StateSnapshot { .. }),
            "register seed is a clock snapshot, never SlotVisibility",
        );

        // Drive a visibility edge (must be filtered) then a clock edge (passes).
        vis_tx.send_replace(true);
        clock_tx.send_replace(Some(ClockState {
            iso: "t1".into(),
            unix: 1,
        }));

        match recv(&mut prd).await {
            HostMsg::StateSnapshot { snapshot } => assert_eq!(
                snapshot.clock.map(|c| c.unix),
                Some(1),
                "the clock edge came through; the visibility edge produced no frame",
            ),
            HostMsg::SlotVisibility { .. } => {
                panic!("unsubscribed plugin received a SlotVisibility frame (#305 regression)")
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    /// #305: a connection that **does** subscribe `SlotVisible` gets the
    /// register-time seed and every subsequent edge — the departures poller's
    /// visibility gate keeps working. Subscribes `SlotVisible` only, so the only
    /// proactive frames are the visibility pushes (deterministic ordering).
    #[tokio::test]
    async fn visibility_push_delivered_when_subscribed() {
        let (_clock_tx, clock_rx) = watch::channel(None);
        let (vis_tx, vis_rx) = watch::channel(false);
        let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);

        let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
        tokio::spawn(async move { handle_conn(host_end, &ctx).await });

        let (mut prd, mut pwr) = plugin_end.into_split();
        let mut manifest = Manifest::new("board", Mount::SidebarBottom);
        manifest.subscribes = vec![StateKey::SlotVisible];
        write_frame(&mut pwr, &PluginMsg::Register { manifest })
            .await
            .expect("send Register");

        // Register seed: the current aggregate (false, nothing open at boot).
        assert!(
            matches!(
                recv(&mut prd).await,
                HostMsg::SlotVisibility { visible: false }
            ),
            "register seed carries the current visibility",
        );

        // An open edge is forwarded.
        vis_tx.send_replace(true);
        assert!(
            matches!(
                recv(&mut prd).await,
                HostMsg::SlotVisibility { visible: true }
            ),
            "the open edge reaches the subscriber",
        );
    }
}
