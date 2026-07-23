//! Out-of-process widget-plugin **host transport** (frontend B; #35 PR 2, on
//! the #266 wire protocol and the #199 reconciler).
//!
//! The host **listens** on one same-user socket
//! (`$XDG_RUNTIME_DIR/trollshell/plugin.sock`); plugins are systemd user units
//! that dial in and self-identify with [`PluginMsg::Register`]. Supervision is
//! systemd's job — a crash is just a disconnect. This module is the transport
//! only: it does **not** spawn or supervise plugins (see the spec's topology).
//! *Launching* declared plugins (as transient units via `systemd-run --user`)
//! is [`crate::plugin_launcher`]'s job since #419 — the transport doesn't care
//! who spawned a connecting plugin.
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
//!   [`Mount::SidebarTop`] / [`Mount::SidebarBottom`]) render as sidebar cards,
//!   and the three bar regions ([`Mount::BarLeft`] / [`Mount::BarCenter`] /
//!   [`Mount::BarRight`]) render as bar **chips** (#349) — the plugin's `view()`
//!   tree wrapped in a `.ts-plugin-chip` pill in the matching bar group. Both
//!   sides share one reconciler path ([`build_region`]); a bar region just lays
//!   its cards out horizontally with the chip class. `SidebarLead` (#301) leads
//!   the sidebar — its cards render *above* the built-in weather/calendar/tasks
//!   cards, which `SidebarTop`/`SidebarBottom` (mounted after them) cannot. A
//!   plugin (chip or card) may **also** define an optional drawer *panel* (#349
//!   PR2): a second `Node` tree carried on the render frame's `panel` field,
//!   parked in the dedicated [`PluginHandles::panels`] mailbox and rendered by
//!   the per-monitor plugin drawer child ([`plugin_panel_slot`]); the plugin
//!   opens it by emitting `Effect::OpenPage(Page::PluginSelf)`. A chip/card need
//!   not have a panel (`panel: None`).
//! - **State:** [`StateKey::Clock`] (the snapshot pump), plus the opt-in
//!   host→plugin pushes gated on their own keys — [`StateKey::SlotVisible`]
//!   (#288), [`StateKey::Accent`] (#376), and [`StateKey::AudioSpectrum`] (the
//!   ~20 Hz audio tap, #405).
//! - **Effects:** [`Effect::OpenPage`] (→ the modal drawer, incl. `PluginSelf`
//!   → the plugin's own panel, #349 PR2), [`Effect::RaiseOsd`] (→ the transient
//!   OSD nudge, #236), and [`Effect::Notify`] (→ a local notification toast
//!   through the shell's own daemon, #406) are brokered; every other effect is
//!   logged "unsupported in v1" and skipped. Capability
//!   enforcement stays **declarative-only** (the cap is requested + audit-logged
//!   but not enforced by a cap store — v1 parity); audit-log / the `RunCommand`
//!   round-trip remain deferred.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use hytte::adw;
use hytte::futures_signals::map_ref;
use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::reactive::registry;
use hytte::reactive::spawn_supervised;
use hytte::services::{clock, notifications, pipewire};
use hytte::ui::{Dir as UiDir, EventKind as UiEventKind, Node as UiNode, NodeId, Reconciler};
use hytte_plugin_proto::{
    AudioSpectrum, ClockState, Effect, HostMsg, LogLevel, Mount, Page, PluginMsg, StateKey,
    StateSnapshot, read_frame, socket_path, wire, write_frame,
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
    /// The plugin's optional drawer *panel* tree (#349 PR2), carried alongside
    /// the chip/card `tree` on the same frame. Ignored by the region
    /// reconcilers (they render `tree`); read by the per-monitor plugin drawer
    /// child ([`plugin_panel_slot`]) when this plugin's panel is the active one.
    /// A `SlotRender` parked in the dedicated `panels` mailbox always carries
    /// `Some`.
    panel: Option<wire::Node>,
    outbound: mpsc::Sender<HostMsg>,
}

/// One effect stripped off a render frame, queued on the non-lossy effect
/// channel for the GTK-side broker. Carries the producing plugin's id purely
/// for the audit log.
struct BrokeredEffect {
    plugin_id: String,
    effect: Effect,
}

/// Registry handles for the plugin host. The six render mailboxes (three
/// sidebar + three bar, #349) are written from tokio (a plugin's reader task)
/// and read on the GTK thread (the reconcilers). `clock_tx` is written on the
/// GTK thread (the clock pump) and subscribed from tokio (per-conn snapshot
/// tasks). `effects_rx` is the receive end of the non-lossy effect channel;
/// [`install`] takes it once to drain the broker on the GTK thread
/// (`RefCell<Option<…>>` because the registry is thread-local to that thread and
/// the receiver is single-consumer).
#[doc(hidden)]
pub struct PluginHandles {
    sidebar_lead: Mutable<Vec<SlotRender>>,
    sidebar_top: Mutable<Vec<SlotRender>>,
    sidebar_bottom: Mutable<Vec<SlotRender>>,
    bar_left: Mutable<Vec<SlotRender>>,
    bar_center: Mutable<Vec<SlotRender>>,
    bar_right: Mutable<Vec<SlotRender>>,
    /// Every plugin's latest drawer *panel* tree (#349 PR2), coalesced
    /// latest-wins per plugin id and carrying its generation for teardown — the
    /// same shape as a region mailbox, but a **single** list across all mounts
    /// (a plugin has one panel regardless of where its chip/card mounts).
    /// Written by the reader task; read by the per-monitor plugin drawer child.
    /// Routing it through the same [`upsert_region`]/[`clear_region_if_owned`]
    /// as the six regions inherits the #278 generation guard for free.
    panels: Mutable<Vec<SlotRender>>,
    /// The plugin id whose panel is currently shown in a drawer (any monitor),
    /// or `None`. GTK-thread-only: set by [`set_active_panel`] on drawer
    /// open/switch, cleared on close. Combined with `panels`, it drives which
    /// panel tree each per-monitor plugin drawer child renders.
    active_panel_id: Mutable<Option<String>>,
    clock_tx: watch::Sender<Option<ClockState>>,
    /// Aggregate slot visibility (OR of every monitor's sidebar open flag),
    /// written on the GTK thread ([`set_sidebar_visibility`]) and subscribed
    /// from tokio (per-conn visibility tasks). Starts `false` (nothing open at
    /// boot). See the module-level "Slot visibility" note (#288).
    visibility_tx: watch::Sender<bool>,
    /// The desktop accent (`@accent_color`) resolved on the GTK thread and
    /// handed to accent-subscribing plugins (#376). Written by [`install`]
    /// (via [`publish_accent`]), subscribed from tokio (per-conn accent tasks).
    /// Starts `None` (unresolved) — a plugin then keeps the kit's hard-coded
    /// default until the value lands. Re-published on every accent/scheme
    /// change, not just at startup (#396).
    accent_tx: watch::Sender<Option<[u8; 4]>>,
    /// The latest audio-reactive spectrum off the default sink's monitor (#405),
    /// pumped from `pipewire::audio_spectrum()` on the GTK thread ([`install`]
    /// via [`publish_spectrum`]) and forwarded to spectrum-subscribing plugins
    /// from tokio (per-conn spectrum tasks). Starts `None` (capture inactive);
    /// the tap only runs while [`SPECTRUM_SUBSCRIBERS`] > 0.
    spectrum_tx: watch::Sender<Option<AudioSpectrum>>,
    effects_rx: RefCell<Option<mpsc::UnboundedReceiver<BrokeredEffect>>>,
}

/// Clones of the shared handles handed to the tokio listener + per-conn tasks.
#[derive(Clone)]
struct ListenerCtx {
    sidebar_lead: Mutable<Vec<SlotRender>>,
    sidebar_top: Mutable<Vec<SlotRender>>,
    sidebar_bottom: Mutable<Vec<SlotRender>>,
    bar_left: Mutable<Vec<SlotRender>>,
    bar_center: Mutable<Vec<SlotRender>>,
    bar_right: Mutable<Vec<SlotRender>>,
    /// The dedicated panel mailbox (#349 PR2); see [`PluginHandles::panels`].
    panels: Mutable<Vec<SlotRender>>,
    clock_rx: watch::Receiver<Option<ClockState>>,
    visibility_rx: watch::Receiver<bool>,
    /// Subscriber end of [`PluginHandles::accent_tx`] (#376).
    accent_rx: watch::Receiver<Option<[u8; 4]>>,
    /// Subscriber end of [`PluginHandles::spectrum_tx`] (#405).
    spectrum_rx: watch::Receiver<Option<AudioSpectrum>>,
    effects_tx: mpsc::UnboundedSender<BrokeredEffect>,
}

/// Count of connected plugins subscribing [`StateKey::AudioSpectrum`] (#405).
/// The capture tap is toggled active on the 0→1 edge and inactive on the 1→0
/// edge, so the default sink's monitor is only tapped while a plugin actually
/// consumes it — an idle desktop (or one with no audio-reactive plugin) pays
/// nothing.
static SPECTRUM_SUBSCRIBERS: AtomicUsize = AtomicUsize::new(0);

impl Service for PluginsService {
    type Handles = PluginHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let (clock_tx, clock_rx) = watch::channel(None);
        // Slot visibility seeds `false`: no sidebar is open at boot, and each
        // monitor's `install` re-asserts `false` as it wires up (#288).
        let (visibility_tx, visibility_rx) = watch::channel(false);
        // Accent seeds `None` (unresolved): `install` resolves `@accent_color`
        // on the GTK thread and publishes it once the display's CSS is up (#376).
        let (accent_tx, accent_rx) = watch::channel(None);
        // Audio spectrum seeds `None` (capture inactive): `install` pumps
        // `pipewire::audio_spectrum()` and the tap only runs once a subscriber
        // flips it on (#405).
        let (spectrum_tx, spectrum_rx) = watch::channel(None);
        let (effects_tx, effects_rx) = mpsc::unbounded_channel();
        let handles = PluginHandles {
            sidebar_lead: Mutable::new(Vec::new()),
            sidebar_top: Mutable::new(Vec::new()),
            sidebar_bottom: Mutable::new(Vec::new()),
            bar_left: Mutable::new(Vec::new()),
            bar_center: Mutable::new(Vec::new()),
            bar_right: Mutable::new(Vec::new()),
            panels: Mutable::new(Vec::new()),
            active_panel_id: Mutable::new(None),
            clock_tx,
            visibility_tx,
            accent_tx,
            spectrum_tx,
            effects_rx: RefCell::new(Some(effects_rx)),
        };
        let ctx = ListenerCtx {
            sidebar_lead: handles.sidebar_lead.clone(),
            sidebar_top: handles.sidebar_top.clone(),
            sidebar_bottom: handles.sidebar_bottom.clone(),
            bar_left: handles.bar_left.clone(),
            bar_center: handles.bar_center.clone(),
            bar_right: handles.bar_right.clone(),
            panels: handles.panels.clone(),
            clock_rx,
            visibility_rx,
            accent_rx,
            spectrum_rx,
            effects_tx,
        };
        spawn_supervised("plugins", move || {
            let ctx = ctx.clone();
            async move {
                if let Err(e) = listen(&ctx).await {
                    tracing::warn!(error = %e, "plugin host listener stopped");
                }
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

    // Desktop accent (#376): resolve libadwaita's `@accent_color` once, now that
    // the display's CSS providers are up, and publish it so accent-subscribing
    // plugins can tint their `preem` widgets' default color to match the shell.
    // A failed resolve leaves `None`, so plugins keep the kit's hard-coded
    // default (no regression).
    publish_accent(resolve_accent_color());

    // Live re-tint (#396, #376 follow-up): re-resolve and re-publish whenever
    // the accent changes, or the light/dark scheme flips (`@accent_color` can
    // resolve to a different RGBA per scheme, so a scheme flip needs the same
    // re-resolve as an accent change). `publish_accent` just re-sends on the
    // existing `watch::Sender`; the per-conn accent tasks (and the SDK on the
    // plugin side) already treat a second `Accent` frame as latest-wins, so
    // nothing else changes. Both signals fire on the GTK main thread, same as
    // this `install()` call, so no thread hop is needed. `connect_notify_local`
    // (rather than a typed `connect_accent_color_notify`) sidesteps the same
    // v1_6-feature gap `resolve_accent_color` already works around.
    let style_manager = adw::StyleManager::default();
    style_manager.connect_notify_local(Some("accent-color"), |_, _| {
        publish_accent(resolve_accent_color());
    });
    style_manager.connect_dark_notify(|_| {
        publish_accent(resolve_accent_color());
    });

    // Audio spectrum pump (#405): project the live `pipewire::audio_spectrum()`
    // (a services `AudioSpectrum`, or `None` while the tap is inactive) onto the
    // GTK-free wire `AudioSpectrum` and publish it on the watch channel the
    // per-conn spectrum tasks subscribe to. The signal replays its current value
    // on subscribe, so this is up to date the moment a plugin dials in.
    glib::MainContext::default().spawn_local(pipewire::audio_spectrum().for_each(|spectrum| {
        publish_spectrum(spectrum.map(to_wire_spectrum));
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

/// Publish the resolved desktop accent to the per-conn accent tasks (#376).
fn publish_accent(accent: Option<[u8; 4]>) {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .accent_tx
            .send_replace(accent);
    });
}

/// Publish the latest audio spectrum to the per-conn spectrum tasks (#405).
fn publish_spectrum(spectrum: Option<AudioSpectrum>) {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .spectrum_tx
            .send_replace(spectrum);
    });
}

/// Project a services [`pipewire::AudioSpectrum`] onto the GTK-free plugin-proto
/// [`AudioSpectrum`] the wire carries (field-for-field, #405).
fn to_wire_spectrum(s: pipewire::AudioSpectrum) -> AudioSpectrum {
    AudioSpectrum {
        peak: s.peak,
        bins: s.bins,
    }
}

/// Resolve libadwaita's `@accent_color` to an opaque RGBA byte quad on the GTK
/// thread (#376). Mirrors what the shell's CSS already does for the sparkline
/// (`.ts-sparkline { color: @accent_color; }`), but materialized in Rust so the
/// value can be handed to out-of-process plugins that can't read GTK themselves.
///
/// libadwaita registers `@accent_color` as a display-scope named color, so a
/// throwaway, unrealized widget resolves it. The style-context color lookup is
/// deprecated in GTK4, but the pinned libadwaita is on the `v1_4` feature and
/// `StyleManager::accent_color_rgba` needs `v1_6` — so this scoped-`allow`s the
/// deprecation rather than bumping the whole adw feature surface (which would
/// also risk the sandboxed `nix build` link). `None` when the color isn't
/// defined yet (e.g. providers not loaded), so the caller falls back to the
/// kit's hard-coded default.
fn resolve_accent_color() -> Option<[u8; 4]> {
    let probe = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    #[allow(deprecated)]
    let rgba = probe.style_context().lookup_color("accent_color")?;
    Some(rgba_to_bytes(&rgba))
}

/// A `gdk::RGBA` (channels in `0.0..=1.0`) as an opaque `[r, g, b, 0xff]` byte
/// quad — the layout `preem` and [`HostMsg::Accent`] carry. Alpha is forced
/// opaque: preem frames are screens and the accent is used as an opaque ink.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rgba_to_bytes(rgba: &gtk::gdk::RGBA) -> [u8; 4] {
    // Each channel is clamped to 0.0..=1.0 then ×255 → 0.0..=255.0 and rounded,
    // so the cast is exact (mirrors `hytte-plugin-caw`'s `intensity`).
    let chan = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [
        chan(rgba.red()),
        chan(rgba.green()),
        chan(rgba.blue()),
        0xff,
    ]
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

fn bar_left_render_signal() -> impl Signal<Item = Vec<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .bar_left
            .signal_cloned()
    })
}

fn bar_center_render_signal() -> impl Signal<Item = Vec<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .bar_center
            .signal_cloned()
    })
}

fn bar_right_render_signal() -> impl Signal<Item = Vec<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .bar_right
            .signal_cloned()
    })
}

/// Map one wire [`Effect`] onto a real host command. Handles [`Effect::OpenPage`]
/// (→ the modal drawer), [`Effect::RaiseOsd`] (→ the transient OSD nudge, #236),
/// and [`Effect::Notify`] (→ a local notification toast, #406); anything else is
/// logged and skipped. Capability gating is intentionally **declarative-only** at
/// this stage — like `OpenPage`, the `RaiseOsd`/`Notify` caps are requested in the
/// manifest and audit-logged here but not enforced by a cap store (v1 parity;
/// audit-log + the `RunCommand` round-trip remain deferred — see the module doc /
/// PR body).
fn broker_effect(plugin_id: &str, effect: &Effect) {
    match effect {
        Effect::OpenPage(page) => match resolve_open_page(*page) {
            PageAction::OpenBuiltin(target) => {
                tracing::info!(plugin = %plugin_id, ?target, "plugin effect: OpenPage");
                crate::modal::open_on_focused(None, target);
            }
            PageAction::OpenPluginSelf => {
                tracing::info!(plugin = %plugin_id, "plugin effect: OpenPage(PluginSelf)");
                crate::modal::open_plugin_on_focused(None, plugin_id);
            }
        },
        Effect::RaiseOsd { title, body, icon } => {
            tracing::info!(plugin = %plugin_id, title = %title, "plugin effect: RaiseOsd");
            crate::overlays::osd::nudge(title, body, icon.as_deref());
        }
        Effect::Notify { summary, body } => {
            // trollshell owns `org.freedesktop.Notifications`, so a plugin toast
            // is injected through the shell's own local-post path (#227) rather
            // than a D-Bus round-trip — same rendering as an external `Notify`
            // (history, DND gating, rate-limiting). Attributed to the plugin id
            // as the app name. `Normal` urgency: a plugin alert is informational,
            // not error-scope, so DND may hold it (see `post_local`'s docs).
            tracing::info!(plugin = %plugin_id, summary = %summary, "plugin effect: Notify");
            notifications::post_local(plugin_id, summary, body, notifications::Urgency::Normal);
        }
        other => {
            tracing::warn!(plugin = %plugin_id, ?other, "plugin effect unsupported in v1; skipped");
        }
    }
}

// ── GTK-side mount: reconciler-backed regions (sidebar cards + bar chips) ─────

/// The [`Mount::SidebarLead`] **region** — a vertical container of N plugin
/// cards. Built per monitor from `overlays::sidebar::build_card` and mounted at
/// the very **top** of the sidebar, above the built-in weather/calendar/tasks
/// cards, so a plugin here leads the sidebar (#301).
#[must_use]
pub fn sidebar_lead_slot() -> gtk::Widget {
    build_region(
        lead_render_signal(),
        gtk::Orientation::Vertical,
        "ts-plugin-card",
    )
}

/// The [`Mount::SidebarTop`] **region** — a vertical container of N plugin
/// cards. Built per monitor from `overlays::sidebar::build_card` and appended
/// above the built-in widgets.
#[must_use]
pub fn sidebar_top_slot() -> gtk::Widget {
    build_region(
        top_render_signal(),
        gtk::Orientation::Vertical,
        "ts-plugin-card",
    )
}

/// The [`Mount::SidebarBottom`] **region**, appended below the built-in sidebar
/// widgets.
#[must_use]
pub fn sidebar_bottom_slot() -> gtk::Widget {
    build_region(
        bottom_render_signal(),
        gtk::Orientation::Vertical,
        "ts-plugin-card",
    )
}

/// The [`Mount::BarLeft`] **region** — a horizontal row of N plugin **chips**
/// (#349). Built per monitor from `main.rs`'s `build_bar` and appended into the
/// bar's left group. Each plugin's `view()` tree renders inside a
/// `.ts-plugin-chip` pill, mirroring the sidebar card path but laid out
/// horizontally so co-mounted chips sit side by side.
#[must_use]
pub fn bar_left_slot() -> gtk::Widget {
    build_region(
        bar_left_render_signal(),
        gtk::Orientation::Horizontal,
        "ts-plugin-chip",
    )
}

/// The [`Mount::BarCenter`] **region** — a horizontal row of N plugin chips,
/// appended into the bar's center group (#349).
#[must_use]
pub fn bar_center_slot() -> gtk::Widget {
    build_region(
        bar_center_render_signal(),
        gtk::Orientation::Horizontal,
        "ts-plugin-chip",
    )
}

/// The [`Mount::BarRight`] **region** — a horizontal row of N plugin chips,
/// appended into the bar's right group (#349).
#[must_use]
pub fn bar_right_slot() -> gtk::Widget {
    build_region(
        bar_right_render_signal(),
        gtk::Orientation::Horizontal,
        "ts-plugin-chip",
    )
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
    outbound: Rc<RefCell<Option<mpsc::Sender<HostMsg>>>>,
}

/// Build the `gtk::Box` region driven by `signal` (a mount's sorted render
/// list). Each connected plugin gets its own reconciler-backed card; the region
/// reconciles cards in on join / update / reorder / leave, keyed by plugin id.
///
/// `orientation` lays the cards out — `Vertical` for sidebar card stacks,
/// `Horizontal` for bar chip rows (#349). `card_class` is the CSS class stamped
/// on each card root: `ts-plugin-card` for a sidebar card, `ts-plugin-chip` for
/// a bar chip. The region hides itself while empty so an unused bar region never
/// introduces a phantom inter-widget gap in the bar group it sits in.
fn build_region(
    signal: impl Signal<Item = Vec<SlotRender>> + 'static,
    orientation: gtk::Orientation,
    card_class: &'static str,
) -> gtk::Widget {
    // Chips in a horizontal bar row want a small gap between co-mounted plugins;
    // sidebar cards stack tight (each card owns its own bottom margin in CSS).
    let spacing = match orientation {
        gtk::Orientation::Horizontal => 6,
        _ => 0,
    };
    let container = gtk::Box::new(orientation, spacing);
    container.add_css_class("ts-plugin-region");
    // Empty until a plugin dials in; a later reconcile reveals it once a card
    // exists (so an empty region contributes no spacing to its parent group).
    container.set_visible(false);

    // GTK-thread-only per-plugin card state. Order here is just a lookup table;
    // widget order is enforced against the region container directly.
    let cards: Rc<RefCell<Vec<MountedCard>>> = Rc::new(RefCell::new(Vec::new()));

    let cards_for_signal = cards.clone();
    let container_for_signal = container.clone();
    let handle = glib::MainContext::default().spawn_local(signal.for_each(move |renders| {
        reconcile_region(
            &container_for_signal,
            &cards_for_signal,
            &renders,
            card_class,
        );
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
    card_class: &str,
) {
    let mut cards = cards.borrow_mut();

    // Reveal the region exactly when it holds at least one card, so an empty
    // region (no plugin mounted here yet) adds no spacing to its parent group.
    container.set_visible(!renders.is_empty());

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
            root.add_css_class(card_class);
            let outbound: Rc<RefCell<Option<mpsc::Sender<HostMsg>>>> =
                Rc::new(RefCell::new(Some(render.outbound.clone())));
            let ev_outbound = outbound.clone();
            let mut reconciler = Reconciler::new(&root, move |id: NodeId, kind: UiEventKind| {
                if let Some(tx) = ev_outbound.borrow().as_ref() {
                    // Non-blocking: a stuck plugin's full outbound queue drops the
                    // event rather than blocking the GTK thread (#435). It's about
                    // to be reaped by the liveness ping anyway.
                    let _ = tx.try_send(HostMsg::Event {
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

// ── Plugin drawer panel (#349 PR2) ───────────────────────────────────────────

fn panels_render_signal() -> impl Signal<Item = Vec<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .panels
            .signal_cloned()
    })
}

fn active_panel_signal() -> impl Signal<Item = Option<String>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .active_panel_id
            .signal_cloned()
    })
}

/// Select which plugin's panel the per-monitor drawer children show (#349 PR2).
/// GTK thread. `Some(id)` on open/switch to a plugin panel; `None` on close.
/// `modal.rs` calls this from its plugin-open entry points and on drawer close /
/// monitor teardown.
pub fn set_active_panel(plugin_id: Option<&str>) {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .active_panel_id
            .set(plugin_id.map(str::to_owned));
    });
}

/// An empty panel tree — the blank page a drawer plugin child shows when no
/// plugin is active (or the active plugin left / has no panel).
fn empty_panel() -> UiNode {
    UiNode::Box {
        id: None,
        dir: UiDir::Vertical,
        spacing: 0,
        scroll: false,
        classes: Vec::new(),
        children: Vec::new(),
    }
}

/// The per-monitor plugin drawer child (#349 PR2): a single reconciler-backed
/// `gtk::Box` whose content is the **active** plugin's `panel` tree. One instance
/// lives in each monitor's drawer stack under the fixed `PLUGIN_STACK_CHILD`
/// name (see `modal.rs`); all mirror the same active panel — exactly how sidebar
/// plugin cards mirror onto every monitor's sidebar region. When no plugin is
/// active — or the active plugin left, or it has no panel — the child renders an
/// empty tree (a blank page); the user then closes the drawer.
///
/// Panel events (button / slider / entry) route to the **live** connection of
/// whichever plugin is active, via the same swapped-`outbound` cell as
/// [`MountedCard`], so a fast plugin reconnect redirects panel events without a
/// dangling send.
#[must_use]
pub fn plugin_panel_slot() -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("ts-plugin-panel");

    // The active connection's outbound, swapped on each render so panel events
    // reach whichever plugin is active now (mirrors the region card pattern).
    let outbound: Rc<RefCell<Option<mpsc::Sender<HostMsg>>>> = Rc::new(RefCell::new(None));
    let ev_outbound = outbound.clone();
    let mut reconciler = Reconciler::new(&root, move |id: NodeId, kind: UiEventKind| {
        if let Some(tx) = ev_outbound.borrow().as_ref() {
            // Non-blocking (#435): drop the panel event if the plugin's outbound
            // queue is full rather than block the GTK thread.
            let _ = tx.try_send(HostMsg::Event {
                node: id,
                kind: to_wire_event(kind),
            });
        }
    });

    // Derived signal: the active plugin's current panel `SlotRender` (or `None`
    // when nothing is active or the active plugin has no panel entry).
    let active = map_ref! {
        let panels = panels_render_signal(),
        let active_id = active_panel_signal() => {
            active_id
                .as_ref()
                .and_then(|id| panels.iter().find(|r| &r.plugin_id == id).cloned())
        }
    };

    let handle = glib::MainContext::default().spawn_local(active.for_each(move |slot| {
        if let Some(render) = slot.as_ref().filter(|r| r.panel.is_some()) {
            // Swap in the active connection's outbound, then render its panel.
            *outbound.borrow_mut() = Some(render.outbound.clone());
            reconciler.render(&to_ui_node(render.panel.as_ref().expect("filtered Some")));
        } else {
            // No active plugin (or it left / has no panel): blank the page and
            // drop any stale outbound so no event can reach a gone connection.
            *outbound.borrow_mut() = None;
            reconciler.render(&empty_panel());
        }
        std::future::ready(())
    }));

    // Best-effort teardown: abort the subscription when the drawer child is
    // destroyed (a per-monitor drawer rebuild on hot-plug).
    root.connect_destroy(move |_| handle.abort());
    root.upcast()
}

// ── tokio-side: listener + per-connection tasks ──────────────────────────────

/// A short backoff applied after a resource-pressure `accept(2)` error, so a
/// *persistent* one (sustained fd/memory exhaustion) degrades gracefully
/// instead of spinning the accept loop hot.
const ACCEPT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

/// Decide how the accept loop should react to an `accept(2)` error. **No accept
/// error is fatal** — the socket bound successfully and stays valid, so a live
/// listener is always worth another `accept()`. Terminating the loop here is
/// exactly what stranded every plugin against a dead socket for the rest of the
/// session (#426): `accept(2)` returns transient errors (`ECONNABORTED` when a
/// peer aborts before we take it, or `EMFILE`/`ENFILE`/`ENOBUFS`/`ENOMEM` under
/// momentary resource pressure), yet the plugin-side SDK redials forever, so the
/// asymmetry left the host permanently deaf. Mirrors the `Lagged → continue`
/// survival the bus signal loop got in #428.
///
/// A connection aborted/reset/refused before we accepted it is a pure per-peer
/// hiccup — the listener is untouched — so retry **immediately** (`None`).
/// Anything else gets a short [`ACCEPT_BACKOFF`] (`Some`) before the retry.
/// Total by construction: every error maps to "retry", never "give up".
fn accept_backoff(err: &std::io::Error) -> Option<std::time::Duration> {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::ConnectionAborted
        | ErrorKind::ConnectionReset
        | ErrorKind::ConnectionRefused => None,
        _ => Some(ACCEPT_BACKOFF),
    }
}

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
                // Keep the listener alive: a transient `accept(2)` error must
                // NOT kill the loop, or one syscall hiccup strands every plugin
                // against a dead socket until restart (#426). Warn and retry;
                // back off on resource-pressure errors so a persistent one
                // degrades gracefully instead of spinning hot.
                match accept_backoff(&e) {
                    Some(delay) => {
                        tracing::warn!(error = %e, "plugin host accept failed; backing off and retrying");
                        tokio::time::sleep(delay).await;
                    }
                    None => {
                        tracing::debug!(error = %e, "plugin host accept: peer aborted before accept; retrying");
                    }
                }
            }
        }
    }
}

/// Route one plugin `Render` frame (#274 / #277 / #349 PR2): strip its one-shot
/// effects onto the global non-lossy broker channel, park (or clear) its optional
/// drawer panel in the dedicated `panels` mailbox, and upsert its chip/card
/// `tree` into the mount's region mailbox (latest-wins per plugin id). Factored
/// out of [`handle_conn`] so that reader loop stays within the line budget.
fn route_render(ctx: &ListenerCtx, mount: Mount, render: SlotRender, effects: Vec<Effect>) {
    // The mount picks which region mailbox (and thus per-monitor container) the
    // tree lands in: sidebar regions render as cards, bar regions as chips
    // (#349); both share the reconciler path.
    let region = match mount {
        Mount::SidebarLead => &ctx.sidebar_lead,
        Mount::SidebarTop => &ctx.sidebar_top,
        Mount::SidebarBottom => &ctx.sidebar_bottom,
        Mount::BarLeft => &ctx.bar_left,
        Mount::BarCenter => &ctx.bar_center,
        Mount::BarRight => &ctx.bar_right,
    };
    // One-shot effects first, over the (global) non-lossy channel, BEFORE
    // parking the idempotent tree — a superseding render frame could otherwise
    // coalesce this frame's click away (#277).
    for effect in effects {
        let _ = ctx.effects_tx.send(BrokeredEffect {
            plugin_id: render.plugin_id.clone(),
            effect,
        });
    }
    // The optional drawer panel (#349 PR2) rides the same frame but lands in the
    // dedicated `panels` mailbox (a single list across all mounts) so the
    // per-monitor drawer child can render whichever plugin is active. Upsert it
    // latest-wins per id when present; clear this connection's entry when the
    // plugin drops its panel (Some→None). The same `upsert_region` /
    // `clear_region_if_owned` as the regions → inherits the #278 generation guard.
    if render.panel.is_some() {
        upsert_region(&ctx.panels, render.clone());
    } else {
        clear_region_if_owned(&ctx.panels, &render.plugin_id, render.generation);
    }
    // Latest-wins per plugin id: upsert overwrites *this* plugin's card in place,
    // leaving siblings alone (#274).
    upsert_region(region, render);
}

// ── Plugin containment (#435) ────────────────────────────────────────────────
//
// Four measures so a misbehaving / hung / hostile plugin can't harm the shell.
// All limits are named consts; a well-behaved plugin is unaffected by every one.

/// Timeout on the `Register` handshake: a peer that dials the socket but never
/// identifies itself is dropped rather than parking a task + fd forever.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);

/// Interval between host→plugin liveness [`HostMsg::Ping`]s. A well-behaved
/// plugin answers each with a [`PluginMsg::Pong`]; a hung one is dropped after
/// [`MAX_MISSED_PONGS`] go unanswered (~`PING_INTERVAL * (MAX_MISSED_PONGS + 1)`
/// worst case), freeing its region slot instead of leaving a frozen card.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// How many consecutive unanswered pings mark a plugin as hung (drop it).
const MAX_MISSED_PONGS: u32 = 2;

/// Bound on the per-connection outbound (host→plugin) queue. A well-behaved
/// plugin drains it immediately so it sits near-empty; a plugin that stops
/// reading its socket backs it up to this cap, at which point new frames are
/// dropped (see [`push_state`]) rather than buffered without limit. Comfortably
/// above any real burst, so the happy path never fills it.
const OUTBOUND_CAPACITY: usize = 256;

/// Max effect tokens a connection may hold — the burst of [`Effect`]s it can emit
/// back-to-back before the sustained cap ([`EFFECT_REFILL_PER_SEC`]) applies.
const EFFECT_BURST: u32 = 8;

/// Sustained effect budget refilled per second (a token-bucket rate). Together
/// with [`EFFECT_BURST`] this caps how fast a plugin can flood the drawer / OSD /
/// toast broker; user-driven effects (a click → `OpenPage`) never approach it.
const EFFECT_REFILL_PER_SEC: f64 = 1.0;

/// Whether a non-blocking outbound push should keep its producer task running.
enum Push {
    /// The frame was sent, or dropped because the queue was momentarily full —
    /// either way keep going (the next change re-sends the latest value).
    Continue,
    /// The receiver (writer task) is gone: the connection is tearing down, stop.
    Stop,
}

/// Non-blocking push onto a connection's bounded outbound queue (#435). Every
/// host→plugin state frame is latest-wins, so a `Full` queue (the plugin stopped
/// reading) drops the frame rather than growing memory without bound — the stuck
/// plugin is separately reaped by the liveness ping (it can't answer pings while
/// not reading). `Closed` means the writer task exited; the producer stops.
fn push_state(out: &mpsc::Sender<HostMsg>, msg: HostMsg) -> Push {
    match out.try_send(msg) {
        Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Push::Continue,
        Err(mpsc::error::TrySendError::Closed(_)) => Push::Stop,
    }
}

/// Per-connection effect rate cap (#435): a token bucket over [`Effect`]
/// emissions. A plugin may fire up to [`EFFECT_BURST`] effects back-to-back;
/// beyond that it's limited to [`EFFECT_REFILL_PER_SEC`], so a buggy plugin
/// emitting an effect per render can't flood the (deliberately non-lossy #277)
/// effect broker with drawer-opens / OSD nudges / toasts.
struct EffectRateLimiter {
    tokens: f64,
    last: Instant,
}

impl EffectRateLimiter {
    fn new() -> Self {
        Self::new_at(Instant::now())
    }

    fn new_at(now: Instant) -> Self {
        Self {
            tokens: f64::from(EFFECT_BURST),
            last: now,
        }
    }

    /// Refill by the time elapsed since the last call (capped at the burst), then
    /// try to spend one token. `true` = allowed, `false` = over budget (drop).
    fn allow(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * EFFECT_REFILL_PER_SEC).min(f64::from(EFFECT_BURST));
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Filter a render frame's effects through the connection's rate limiter,
/// dropping (with a warn) any that exceed the cap. All effects in one frame share
/// a single `now`, so a burst frame depletes the bucket in order.
fn throttle_effects(
    rl: &mut EffectRateLimiter,
    plugin_id: &str,
    effects: Vec<Effect>,
) -> Vec<Effect> {
    if effects.is_empty() {
        return effects;
    }
    let now = Instant::now();
    let mut kept = Vec::with_capacity(effects.len());
    for effect in effects {
        if rl.allow(now) {
            kept.push(effect);
        } else {
            tracing::warn!(plugin = %plugin_id, ?effect, "plugin effect rate cap exceeded; dropped");
        }
    }
    kept
}

/// Drive one plugin connection: handshake, then read frames until the peer
/// disconnects, feeding renders into the mount mailbox and pushing state
/// snapshots + events back out.
// One cohesive per-connection lifecycle (handshake → the four opt-in push tasks
// → reader loop → teardown); splitting it would scatter the paired setup/abort
// of each task across helpers for no readability gain.
#[allow(clippy::too_many_lines)]
async fn handle_conn(stream: UnixStream, ctx: &ListenerCtx) {
    let (mut rd, wr) = stream.into_split();

    // Handshake: the first frame MUST be `Register`, and its proto must match
    // exactly — else drop the connection (schema skew fails loud). Bounded by
    // `REGISTER_TIMEOUT` (#435): a peer that dials in but never identifies itself
    // must not park a task + fd forever.
    let first =
        match tokio::time::timeout(REGISTER_TIMEOUT, read_frame::<PluginMsg, _>(&mut rd)).await {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "plugin handshake read failed; dropping");
                return;
            }
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_s = REGISTER_TIMEOUT.as_secs(),
                    "plugin did not Register within the handshake timeout; dropping",
                );
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

    // Outbound writer: the single point that serializes host→plugin frames. The
    // queue is **bounded** (#435): a plugin that stops reading its socket can no
    // longer make the host buffer frames without limit — producers drop onto a
    // full queue (`push_state`) and the liveness ping reaps the stuck connection.
    let (out_tx, out_rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
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
    //
    // A **bar** mount is the special case (#438): a bar chip is effectively
    // always on-screen — `SlotVisibility` models sidebar open/close, not bar-chip
    // presence (#288/#422) — so feeding it the sidebar-open aggregate would tell a
    // bar plugin that parks pollers on `SlotVisible` it's hidden while its chip is
    // fully visible. Seed a constant `visible: true` for bar mounts and hold no
    // task (a bar chip's visibility never changes, so there is nothing to track or
    // tear down); only sidebar mounts run the change-tracking `visibility_task`.
    let visibility = if manifest.subscribes.contains(&StateKey::SlotVisible) {
        if mount.is_bar() {
            let _ = out_tx.try_send(HostMsg::SlotVisibility { visible: true });
            None
        } else {
            Some(tokio::spawn(visibility_task(
                ctx.visibility_rx.clone(),
                out_tx.clone(),
            )))
        }
    } else {
        None
    };

    // Desktop accent (#376): seed the resolved `@accent_color` at register (and
    // re-send if it lands after connect) — but ONLY to a plugin that subscribes
    // `StateKey::Accent`, the same #305 opt-in gate as visibility above. The
    // `hytte-plugin` SDK auto-declares that subscription, so accent tracking is
    // out-of-the-box; a pre-#376 binary that never declared it never receives
    // the `HostMsg::Accent` variant it couldn't decode. The task's change-loop
    // is live (#396): it re-sends both a late resolve that lands after connect
    // and any subsequent accent/scheme change to an already-connected plugin.
    let accent = manifest
        .subscribes
        .contains(&StateKey::Accent)
        .then(|| tokio::spawn(accent_task(ctx.accent_rx.clone(), out_tx.clone())));

    // Audio spectrum (#405): forward the ~20 Hz `{peak, bins}` push — but ONLY to
    // a plugin that subscribes `StateKey::AudioSpectrum` (the #305 opt-in gate,
    // exactly like accent/visibility). The capture tap itself is reference-counted
    // across all such subscribers: the 0→1 edge starts it, the 1→0 edge (in
    // teardown below) stops it, so the default sink's monitor is only tapped while
    // something is listening. NOTE: this gates on a subscriber *existing*, not on
    // its slot being on-screen — finer per-slot visibility gating is deferred (see
    // the PR body: bar-chip visibility isn't modeled the way sidebar visibility is).
    let spectrum = manifest
        .subscribes
        .contains(&StateKey::AudioSpectrum)
        .then(|| {
            if SPECTRUM_SUBSCRIBERS.fetch_add(1, Ordering::SeqCst) == 0 {
                pipewire::set_spectrum_active(true);
            }
            tokio::spawn(spectrum_task(ctx.spectrum_rx.clone(), out_tx.clone()))
        });

    // Reader + liveness, raced (#435). The reader dispatches inbound frames; the
    // liveness task pings on an interval and drops the connection if the plugin
    // stops answering — a hung plugin never EOFs, so without this its stale card
    // would stay mounted forever. Whichever future finishes first — a peer
    // disconnect or a failed liveness probe — falls through to the shared teardown.
    //
    // `read_frame` is **not** cancellation-safe (a cancelled partial read would
    // desync the framing), so the reader is kept as a distinct future the
    // `select!` only ever *abandons* on teardown — it is never resumed after a
    // cancel, so no partial read is lost mid-stream.
    let pong_seen = AtomicBool::new(false);
    let mut effect_rl = EffectRateLimiter::new();
    let reader = async {
        loop {
            match read_frame::<PluginMsg, _>(&mut rd).await {
                Ok(PluginMsg::Render {
                    tree,
                    panel,
                    effects,
                }) => route_render(
                    ctx,
                    mount,
                    SlotRender {
                        plugin_id: plugin_id.clone(),
                        order,
                        generation,
                        tree,
                        panel,
                        outbound: out_tx.clone(),
                    },
                    // Effect rate cap (#435): over-budget effects are dropped so a
                    // plugin can't flood the drawer / OSD / toast broker.
                    throttle_effects(&mut effect_rl, &plugin_id, effects),
                ),
                Ok(PluginMsg::Register { .. }) => {
                    tracing::warn!(plugin = %plugin_id, "duplicate Register ignored");
                }
                Ok(PluginMsg::Log { level, msg }) => log_plugin(&plugin_id, level, &msg),
                Ok(PluginMsg::Pong { seq }) => {
                    pong_seen.store(true, Ordering::Relaxed);
                    tracing::trace!(plugin = %plugin_id, seq, "plugin pong");
                }
                Err(e) => {
                    tracing::info!(plugin = %plugin_id, reason = %e, "plugin disconnected");
                    break;
                }
            }
        }
    };
    let liveness = async {
        // First tick one full interval out, so a well-behaved plugin is never
        // probed before it settles (and short-lived tests never observe a ping).
        let mut ping =
            tokio::time::interval_at(tokio::time::Instant::now() + PING_INTERVAL, PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut seq: u64 = 0;
        let mut unanswered: u32 = 0;
        loop {
            ping.tick().await;
            if pong_seen.swap(false, Ordering::Relaxed) {
                unanswered = 0;
            } else if seq > 0 {
                // Only count a miss against a ping we actually sent.
                unanswered += 1;
            }
            if unanswered >= MAX_MISSED_PONGS {
                break;
            }
            seq += 1;
            // A full outbound queue here means the plugin stopped reading its
            // socket (measure 2) — treat it as dead, same as a missed pong.
            if out_tx.try_send(HostMsg::Ping { seq }).is_err() {
                break;
            }
        }
    };
    tokio::select! {
        () = reader => {}
        () = liveness => {
            tracing::warn!(plugin = %plugin_id, "plugin failed liveness ping; dropping as hung");
        }
    }

    // Teardown: remove THIS plugin's card from its region only if THIS connection
    // still owns it (removes just that card on the GTK side), then stop the
    // outbound + snapshot tasks. Connection-scoped + keyed per plugin id, so a
    // fast-reconnect successor is never evicted and sibling plugins are untouched.
    // We probe every region (a plugin lives in exactly one); `clear_region_if_owned`
    // read-locks first and returns early where this plugin isn't present, so the
    // extra probes are cheap.
    clear_region_if_owned(&ctx.sidebar_lead, &plugin_id, generation);
    clear_region_if_owned(&ctx.sidebar_top, &plugin_id, generation);
    clear_region_if_owned(&ctx.sidebar_bottom, &plugin_id, generation);
    clear_region_if_owned(&ctx.bar_left, &plugin_id, generation);
    clear_region_if_owned(&ctx.bar_center, &plugin_id, generation);
    clear_region_if_owned(&ctx.bar_right, &plugin_id, generation);
    // The panel mailbox (#349 PR2) is teardown-scoped the same way; if this
    // plugin's panel is the one currently shown, the drawer child's derived
    // signal yields `None` and renders empty (the user then closes the drawer).
    clear_region_if_owned(&ctx.panels, &plugin_id, generation);
    if let Some(snapshot) = snapshot {
        snapshot.abort();
    }
    if let Some(visibility) = visibility {
        visibility.abort();
    }
    if let Some(accent) = accent {
        accent.abort();
    }
    if let Some(spectrum) = spectrum {
        spectrum.abort();
        // 1→0 edge: this was the last spectrum subscriber, so stop the tap.
        if SPECTRUM_SUBSCRIBERS.fetch_sub(1, Ordering::SeqCst) == 1 {
            pipewire::set_spectrum_active(false);
        }
    }
    writer.abort();
}

/// Serialize host→plugin frames pulled off the outbound channel until the
/// channel closes or a write fails.
async fn writer_task(mut wr: OwnedWriteHalf, mut rx: mpsc::Receiver<HostMsg>) {
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
    out: mpsc::Sender<HostMsg>,
) {
    // Initial snapshot (the watch replays its current value).
    let initial = clock_rx.borrow_and_update().clone();
    if let Push::Stop = push_state(
        &out,
        HostMsg::StateSnapshot {
            snapshot: StateSnapshot { clock: initial },
        },
    ) {
        return;
    }
    while clock_rx.changed().await.is_ok() {
        let clock = clock_rx.borrow_and_update().clone();
        if let Push::Stop = push_state(
            &out,
            HostMsg::StateSnapshot {
                snapshot: StateSnapshot { clock },
            },
        ) {
            break;
        }
    }
}

/// Push the aggregate slot visibility on the initial subscribe (the register
/// seed, so a reconnecting plugin starts in the right state) and on every
/// change, coalescing bursts latest-wins via `borrow_and_update` (#288). Mirrors
/// [`snapshot_task`]; spawned **only** for a **sidebar** connection that
/// subscribes [`StateKey::SlotVisible`] (#305) — an unsubscribed plugin never
/// receives the frame, and a bar mount gets a constant `true` seed instead (its
/// chip is always on-screen; see `handle_conn`, #438), never this change loop.
async fn visibility_task(mut visibility_rx: watch::Receiver<bool>, out: mpsc::Sender<HostMsg>) {
    // Seed at register (the watch replays its current value).
    let initial = *visibility_rx.borrow_and_update();
    if let Push::Stop = push_state(&out, HostMsg::SlotVisibility { visible: initial }) {
        return;
    }
    while visibility_rx.changed().await.is_ok() {
        let visible = *visibility_rx.borrow_and_update();
        if let Push::Stop = push_state(&out, HostMsg::SlotVisibility { visible }) {
            break;
        }
    }
}

/// Push the resolved desktop accent on the initial subscribe (the register seed,
/// so a plugin starts tinted) and on any change, latest-wins via
/// `borrow_and_update` (#376). Mirrors [`snapshot_task`]/[`visibility_task`];
/// spawned **only** for a connection that subscribes [`StateKey::Accent`]
/// (#305) — an unsubscribed plugin never receives the frame. The change-loop
/// is live (#396): `install`'s `StyleManager` listener re-publishes on every
/// accent/scheme change, which lands here as an additional `watch` update
/// exactly like a late startup resolve.
async fn accent_task(mut accent_rx: watch::Receiver<Option<[u8; 4]>>, out: mpsc::Sender<HostMsg>) {
    // Seed at register (the watch replays its current value).
    let initial = *accent_rx.borrow_and_update();
    if let Push::Stop = push_state(&out, HostMsg::Accent { color: initial }) {
        return;
    }
    while accent_rx.changed().await.is_ok() {
        let color = *accent_rx.borrow_and_update();
        if let Push::Stop = push_state(&out, HostMsg::Accent { color }) {
            break;
        }
    }
}

/// Push the latest audio spectrum on subscribe and on every change, coalescing
/// bursts latest-wins via `borrow_and_update` (#405). Mirrors [`accent_task`],
/// but **skips** the `None` (capture inactive / no audio yet) state so a plugin
/// only ever receives real `{peak, bins}` frames. Spawned **only** for a
/// connection that subscribes [`StateKey::AudioSpectrum`] (#305) — an
/// unsubscribed plugin never receives the frame.
async fn spectrum_task(
    mut spectrum_rx: watch::Receiver<Option<AudioSpectrum>>,
    out: mpsc::Sender<HostMsg>,
) {
    // Seed at register (the watch replays its current value; often `None` until
    // audio flows through the freshly-activated tap).
    let seed = *spectrum_rx.borrow_and_update();
    if let Some(spectrum) = seed
        && let Push::Stop = push_state(&out, HostMsg::AudioSpectrum { spectrum })
    {
        return;
    }
    while spectrum_rx.changed().await.is_ok() {
        let current = *spectrum_rx.borrow_and_update();
        if let Some(spectrum) = current
            && let Push::Stop = push_state(&out, HostMsg::AudioSpectrum { spectrum })
        {
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
/// region this plugin isn't even in (each teardown checks *all six* regions —
/// three sidebar + three bar, #349); it re-finds under the write lock to stay
/// correct against a concurrent mutation.
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
            scale,
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
            // Same seam for the `scale` hint (#358): an absurd upscale is
            // clamped (with a warn) rather than honored, so a malformed plugin
            // can't request a monster allocation; `0` silently means `1` (the
            // wire contract's documented default alias, not worth a warning).
            let scale = {
                let clamped = clamp_pixels_scale(width, height, *scale);
                if clamped < *scale {
                    tracing::warn!(
                        node = ?id,
                        width,
                        height,
                        scale = *scale,
                        clamped,
                        "plugin Pixels scale exceeds the scaled-dimension cap; clamped"
                    );
                }
                clamped
            };
            UiNode::Pixels {
                id: id.clone(),
                width,
                height,
                data,
                scale,
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
            enabled,
            classes,
        } => UiNode::Slider {
            id: id.clone(),
            min: *min,
            max: *max,
            value: *value,
            step: *step,
            enabled: *enabled,
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
        wire::Node::Entry {
            id,
            text,
            placeholder,
            classes,
        } => UiNode::Entry {
            id: id.clone(),
            text: text.clone(),
            placeholder: placeholder.clone(),
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

/// Cap on a [`wire::Node::Pixels`]'s *scaled* natural dimension
/// (`max(width, height) * scale`), so a hostile/buggy `scale` can't request a
/// monster widget. 16384 px is a common max-texture edge and far beyond any
/// sane sidebar surface.
const MAX_PIXELS_SCALED_DIM: u32 = 16_384;

/// Clamp a [`wire::Node::Pixels`] `scale` hint to something the host will
/// honor: at least `1` (the wire contract treats `0`/absent as `1`), and small
/// enough that the scaled natural dimension stays within
/// [`MAX_PIXELS_SCALED_DIM`]. An empty (or already over-cap) buffer gets `1` —
/// scale is inert there anyway.
fn clamp_pixels_scale(width: u32, height: u32, scale: u32) -> u32 {
    let dim = width.max(height);
    if dim == 0 {
        return 1;
    }
    scale.clamp(1, (MAX_PIXELS_SCALED_DIM / dim).max(1))
}

/// Map a reconciler event back onto its wire form for the outbound `Event`
/// frame. Exhaustive over the `EventKind` set (Click, Scroll, `ValueChanged`,
/// `Submitted`), so adding a kind to either side breaks the build here rather
/// than silently dropping an event.
fn to_wire_event(kind: UiEventKind) -> wire::EventKind {
    match kind {
        UiEventKind::Click => wire::EventKind::Click,
        UiEventKind::Scroll { dx, dy } => wire::EventKind::Scroll { dx, dy },
        UiEventKind::ValueChanged { value } => wire::EventKind::ValueChanged { value },
        UiEventKind::Submitted { text } => wire::EventKind::Submitted { text },
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
        // `PluginSelf` (#349 PR2) has no built-in `modal::Page`: it is
        // intercepted by `resolve_open_page` in the broker and routed to the
        // requesting plugin's own panel, so it never reaches `map_page`. The
        // arm documents the split and keeps the match exhaustive over wire
        // `Page` (a page added to either side still breaks the build here).
        Page::PluginSelf => unreachable!(
            "PluginSelf is intercepted by resolve_open_page and never mapped to a modal::Page",
        ),
    }
}

/// The host action a wire [`Effect::OpenPage`] resolves to (#349 PR2). Split out
/// as a **pure** function so the [`Page::PluginSelf`] interception — which has no
/// `modal::Page` counterpart — is unit-testable without GTK, the way [`map_page`]
/// is. The broker ([`broker_effect`]) calls this, then dispatches: a built-in
/// page opens by `modal::Page`; `PluginSelf` opens the requesting plugin's own
/// panel (keyed by the effect's plugin id, which the broker already carries).
enum PageAction {
    OpenBuiltin(crate::modal::Page),
    OpenPluginSelf,
}

fn resolve_open_page(page: Page) -> PageAction {
    match page {
        Page::PluginSelf => PageAction::OpenPluginSelf,
        other => PageAction::OpenBuiltin(map_page(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ACCEPT_BACKOFF, BrokeredEffect, ClockState, Duration, EFFECT_BURST, Effect,
        EffectRateLimiter, HashMap, HostMsg, Instant, ListenerCtx, Mount, Mutable,
        OUTBOUND_CAPACITY, Page, PageAction, PluginMsg, REGISTER_TIMEOUT, SlotRender, StateKey,
        UiDir, UiEventKind, UiNode, UnixStream, accept_backoff, any_sidebar_open, apply_forget,
        apply_open, clamp_pixels_scale, clear_region_if_owned, handle_conn, map_page, mpsc,
        pixels_len_ok, read_frame, resolve_open_page, to_ui_node, to_wire_event, upsert_region,
        watch, wire, write_frame,
    };
    use hytte_plugin_proto::Manifest;

    /// Regression for #426: the accept loop's error policy must be **total** —
    /// every `accept(2)` error maps to a retry, never to loop termination.
    /// Before the fix the `Err` arm did `return Err(e)`, so one transient
    /// syscall error permanently killed the listener and stranded every plugin
    /// against a dead socket until the shell restarted. A per-peer abort/reset
    /// retries immediately (`None`); resource-pressure errors back off (`Some`).
    #[test]
    fn accept_error_never_terminates_the_loop() {
        use std::io::{Error, ErrorKind};

        // Per-peer hiccups: the listener is untouched, so retry immediately.
        for kind in [
            ErrorKind::ConnectionAborted,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionRefused,
        ] {
            assert_eq!(
                accept_backoff(&Error::from(kind)),
                None,
                "{kind:?} should retry immediately, not terminate the loop",
            );
        }

        // Resource pressure (EMFILE/ENFILE/ENOBUFS/ENOMEM surface as `Other`,
        // OutOfMemory, etc.): still retryable, but after a short backoff so a
        // persistent error doesn't spin the loop hot.
        for kind in [
            ErrorKind::Other,
            ErrorKind::OutOfMemory,
            ErrorKind::PermissionDenied,
        ] {
            assert_eq!(
                accept_backoff(&Error::from(kind)),
                Some(ACCEPT_BACKOFF),
                "{kind:?} should back off and retry, not terminate the loop",
            );
        }
    }

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
                    scale: 2,
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
                    enabled: false,
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
                    scale: 2,
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
                    enabled: false,
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
        assert_eq!(
            to_wire_event(UiEventKind::Submitted {
                text: "help".into()
            }),
            wire::EventKind::Submitted {
                text: "help".into()
            }
        );
    }

    /// The #357 `Entry` maps 1:1: the required id (the `Submitted` event
    /// target), the `text` echo prop, and the placeholder all carry across.
    #[test]
    fn wire_entry_maps_to_ui() {
        let tree = wire::Node::Entry {
            id: "term-input".into(),
            text: String::new(),
            placeholder: "type a command…".into(),
            classes: vec!["monospace".into()],
        };
        let expected = UiNode::Entry {
            id: "term-input".into(),
            text: String::new(),
            placeholder: "type a command…".into(),
            classes: vec!["monospace".into()],
        };
        assert_eq!(to_ui_node(&tree), expected);
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

    /// #349 PR2: `resolve_open_page` is the pure seam the broker uses to split a
    /// built-in page-open from the `PluginSelf` self-panel open. A built-in page
    /// resolves to its `modal::Page`; `PluginSelf` resolves to the plugin-self
    /// action (which the broker dispatches with the effect's plugin id) and never
    /// reaches `map_page`'s `unreachable!` arm.
    #[test]
    fn resolve_open_page_splits_pluginself_from_builtin() {
        assert!(
            matches!(
                resolve_open_page(Page::Media),
                PageAction::OpenBuiltin(crate::modal::Page::Media)
            ),
            "a built-in page resolves to its modal::Page",
        );
        assert!(
            matches!(
                resolve_open_page(Page::Settings),
                PageAction::OpenBuiltin(crate::modal::Page::Settings)
            ),
            "another built-in page resolves to its modal::Page",
        );
        assert!(
            matches!(
                resolve_open_page(Page::PluginSelf),
                PageAction::OpenPluginSelf
            ),
            "PluginSelf resolves to the plugin-self action, not a builtin",
        );
    }

    /// One plugin card carrying an id/order/generation + a label tree, for the
    /// region tests below.
    fn render_of(
        plugin_id: &str,
        order: i32,
        generation: u64,
        text: &str,
        tx: &mpsc::Sender<HostMsg>,
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
            panel: None,
            outbound: tx.clone(),
        }
    }

    /// Like [`render_of`], but the card also carries a distinct drawer `panel`
    /// tree (a `Label` with the given panel text) — for the panels-mailbox tests.
    fn render_with_panel(
        plugin_id: &str,
        order: i32,
        generation: u64,
        chip: &str,
        panel: &str,
        tx: &mpsc::Sender<HostMsg>,
    ) -> SlotRender {
        SlotRender {
            panel: Some(wire::Node::Label {
                id: Some("panel".into()),
                text: panel.to_owned(),
                classes: vec![],
            }),
            ..render_of(plugin_id, order, generation, chip, tx)
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
        let (tx, _rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
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
        let (tx, _rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
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

    /// #349 PR2: the dedicated panels mailbox reuses `upsert_region`/
    /// `clear_region_if_owned`, so it inherits their guarantees for free — a
    /// plugin's re-render coalesces its own panel latest-wins, and a stale
    /// (lower-generation) teardown never evicts a fast-reconnect successor's
    /// panel (the #278 generation guard, now covering panels).
    #[test]
    fn panel_upsert_coalesces_and_teardown_is_generation_scoped() {
        let (tx, _rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
        let panels: Mutable<Vec<SlotRender>> = Mutable::new(Vec::new());

        // The plugin renders its panel twice on generation 0: coalesced in place.
        upsert_region(&panels, render_with_panel("pet", 0, 0, "chip", "p1", &tx));
        upsert_region(&panels, render_with_panel("pet", 0, 0, "chip", "p2", &tx));
        assert_eq!(panels.lock_ref().len(), 1, "one panel entry per plugin id");
        assert!(
            matches!(
                &panels.lock_ref()[0].panel,
                Some(wire::Node::Label { text, .. }) if text == "p2"
            ),
            "the plugin's latest panel wins",
        );

        // A fast reconnect (generation 1) replaces the entry; the OLD
        // connection's teardown (generation 0) must NOT evict the successor.
        upsert_region(&panels, render_with_panel("pet", 0, 1, "chip", "p3", &tx));
        clear_region_if_owned(&panels, "pet", 0);
        assert_eq!(
            panels.lock_ref().len(),
            1,
            "a stale-generation teardown leaves the successor's panel",
        );

        // The owning teardown (generation 1) clears it.
        clear_region_if_owned(&panels, "pet", 1);
        assert!(
            panels.lock_ref().is_empty(),
            "the owning connection's teardown clears the panel",
        );
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
            scale: 2,
            classes: vec!["ts-lcd".into()],
        };
        assert_eq!(
            to_ui_node(&bad),
            UiNode::Pixels {
                id: Some("lcd".into()),
                width: 0,
                height: 0,
                data: vec![],
                // The degraded (empty) surface renders nothing; scale is inert
                // there, so it normalizes to 1.
                scale: 1,
                classes: vec!["ts-lcd".into()],
            },
            "malformed Pixels degrades to a nothing-rendered surface",
        );

        let good = wire::Node::Pixels {
            id: None,
            width: 1,
            height: 2,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8], // 1*2*4
            scale: 1,
            classes: vec![],
        };
        assert_eq!(
            to_ui_node(&good),
            UiNode::Pixels {
                id: None,
                width: 1,
                height: 2,
                data: vec![1, 2, 3, 4, 5, 6, 7, 8],
                scale: 1,
                classes: vec![],
            },
            "well-formed Pixels passes through 1:1",
        );
    }

    /// The #358 `scale` hint crosses the same trust boundary as the buffer:
    /// sane integer scales pass through, `0` aliases to `1`, and an absurd
    /// scale is clamped so the scaled natural dimension can never exceed the
    /// host's cap.
    #[test]
    fn pixels_scale_is_clamped_at_the_host_seam() {
        // Pure clamp behavior.
        assert_eq!(clamp_pixels_scale(128, 128, 2), 2, "sane scale passes");
        assert_eq!(clamp_pixels_scale(128, 128, 0), 1, "0 aliases to 1");
        assert_eq!(
            clamp_pixels_scale(128, 128, u32::MAX),
            16_384 / 128,
            "absurd scale clamps to the scaled-dimension cap"
        );
        assert_eq!(
            clamp_pixels_scale(20_000, 1, 3),
            1,
            "an already-over-cap buffer keeps scale 1"
        );
        assert_eq!(clamp_pixels_scale(0, 0, 7), 1, "empty surface: inert 1");

        // Through the mapping arm: the caw case — a 1×1 stand-in at 2× passes
        // untouched; a hostile scale on the same node is capped.
        let node = |scale: u32| wire::Node::Pixels {
            id: Some("lcd".into()),
            width: 1,
            height: 1,
            data: vec![9, 9, 9, 255],
            scale,
            classes: vec![],
        };
        let ui_scale = |n: &wire::Node| match to_ui_node(n) {
            UiNode::Pixels { scale, .. } => scale,
            other => panic!("expected Pixels, got {other:?}"),
        };
        assert_eq!(ui_scale(&node(2)), 2);
        assert_eq!(ui_scale(&node(0)), 1);
        assert_eq!(ui_scale(&node(u32::MAX)), 16_384);
    }

    /// #277 (preserved under the region model): a plugin's back-to-back frames
    /// coalesce its region card latest-wins, but a one-shot effect bundled on the
    /// superseded frame rides the dedicated **global** non-lossy channel and is
    /// delivered exactly once — not dropped, not duplicated.
    #[test]
    fn effects_survive_region_coalescing() {
        let (tx, _rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
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
        let (tx, _rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
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
        let (tx, _rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
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

    /// A bar mount is a real wire variant the reader routes to its own region
    /// (#349); assert the sidebar and bar mounts are all distinct from each other
    /// so the `handle_conn` match can't confuse two.
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
        let (tx, _rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
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
        // Accent is unresolved in the host-session tests (they exercise the
        // clock/visibility gates), and none of them subscribes `StateKey::Accent`,
        // so no accent task ever reads this; seed `None` and let the sender drop.
        let (_accent_tx, accent_rx) = watch::channel(None);
        // Likewise for the audio spectrum (#405): these tests subscribe neither
        // `StateKey::AudioSpectrum`, so no spectrum task reads this.
        let (_spectrum_tx, spectrum_rx) = watch::channel(None);
        let ctx = ListenerCtx {
            sidebar_lead: Mutable::new(Vec::new()),
            sidebar_top: Mutable::new(Vec::new()),
            sidebar_bottom: Mutable::new(Vec::new()),
            bar_left: Mutable::new(Vec::new()),
            bar_center: Mutable::new(Vec::new()),
            bar_right: Mutable::new(Vec::new()),
            panels: Mutable::new(Vec::new()),
            clock_rx,
            visibility_rx,
            accent_rx,
            spectrum_rx,
            effects_tx,
        };
        (ctx, effects_rx)
    }

    /// Poll a region mailbox until it holds at least one card — the reader task
    /// in `handle_conn` fills it asynchronously — failing (not hanging) if it
    /// never populates. The `lock_ref` guard is dropped before each `await`, so
    /// it never crosses a yield point.
    async fn wait_for_region(region: &Mutable<Vec<SlotRender>>) -> Vec<SlotRender> {
        for _ in 0..200 {
            {
                let cards = region.lock_ref();
                if !cards.is_empty() {
                    return cards.clone();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("region never populated within timeout");
    }

    /// #349: a `Bar*`-mounted plugin's render must now reach the matching bar
    /// region mailbox instead of being dropped (the v1 behavior this PR replaces).
    /// Registers a `BarCenter` plugin, sends one `Render`, and asserts the card
    /// lands in `bar_center` — and *only* there (no leak into the sibling bar
    /// regions or a sidebar). Proves the un-defer end to end through `handle_conn`,
    /// the same socketpair harness the visibility-gating tests use.
    #[tokio::test]
    async fn bar_mount_render_reaches_bar_region() {
        let (_clock_tx, clock_rx) = watch::channel(None);
        let (_vis_tx, vis_rx) = watch::channel(false);
        let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);
        // Clone the region handles (Mutable shares its state) before `ctx` moves
        // into the connection task, so the test can inspect the mailboxes after.
        let bar_center = ctx.bar_center.clone();
        let bar_left = ctx.bar_left.clone();
        let bar_right = ctx.bar_right.clone();
        let sidebar_top = ctx.sidebar_top.clone();
        let panels = ctx.panels.clone();

        let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
        tokio::spawn(async move { handle_conn(host_end, &ctx).await });

        let (_prd, mut pwr) = plugin_end.into_split();
        write_frame(
            &mut pwr,
            &PluginMsg::Register {
                manifest: Manifest::new("barchip", Mount::BarCenter),
            },
        )
        .await
        .expect("send Register");
        write_frame(
            &mut pwr,
            &PluginMsg::Render {
                tree: wire::Node::Label {
                    id: Some("t".into()),
                    text: "chip".into(),
                    classes: vec![],
                },
                // A panel-less render: the chip lands in its bar region, and the
                // dedicated panels mailbox (#349 PR2) must stay empty.
                panel: None,
                effects: vec![],
            },
        )
        .await
        .expect("send Render");

        let cards = wait_for_region(&bar_center).await;
        assert_eq!(
            cards.len(),
            1,
            "the BarCenter render reached bar_center (not dropped)"
        );
        assert_eq!(cards[0].plugin_id, "barchip");
        assert!(
            matches!(&cards[0].tree, wire::Node::Label { text, .. } if text == "chip"),
            "the plugin's view tree survived intact into the bar region",
        );

        // Routed to exactly one region: the sibling bar regions and the sidebar
        // stay empty (a BarCenter mount must not fan out or fall back).
        assert!(
            bar_left.lock_ref().is_empty(),
            "BarCenter didn't leak into BarLeft"
        );
        assert!(
            bar_right.lock_ref().is_empty(),
            "BarCenter didn't leak into BarRight"
        );
        assert!(
            sidebar_top.lock_ref().is_empty(),
            "a bar mount didn't leak into a sidebar region"
        );
        assert!(
            panels.lock_ref().is_empty(),
            "a panel-less render never touches the panels mailbox (#349 PR2)"
        );
    }

    /// #349 PR2: a render carrying a `panel` must reach BOTH the plugin's chip
    /// region AND the dedicated panels mailbox — the chip renders inline while
    /// the panel is available for the drawer child. A subsequent panel-less
    /// render (the plugin dropping its panel) clears the panels entry but leaves
    /// the chip. Drives it end to end through `handle_conn` on the socketpair
    /// harness the other host tests use.
    #[tokio::test]
    async fn panel_render_populates_panels_mailbox() {
        let (_clock_tx, clock_rx) = watch::channel(None);
        let (_vis_tx, vis_rx) = watch::channel(false);
        let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);
        let bar_center = ctx.bar_center.clone();
        let panels = ctx.panels.clone();

        let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
        tokio::spawn(async move { handle_conn(host_end, &ctx).await });

        let (_prd, mut pwr) = plugin_end.into_split();
        write_frame(
            &mut pwr,
            &PluginMsg::Register {
                manifest: Manifest::new("panelplug", Mount::BarCenter),
            },
        )
        .await
        .expect("send Register");
        // A panel-bearing render: a chip tree PLUS a distinct panel tree.
        write_frame(
            &mut pwr,
            &PluginMsg::Render {
                tree: wire::Node::Label {
                    id: Some("chip".into()),
                    text: "chip".into(),
                    classes: vec![],
                },
                panel: Some(wire::Node::Label {
                    id: Some("panel".into()),
                    text: "panel body".into(),
                    classes: vec![],
                }),
                effects: vec![],
            },
        )
        .await
        .expect("send panel Render");

        // The chip reaches its bar region…
        let chips = wait_for_region(&bar_center).await;
        assert_eq!(chips.len(), 1);
        assert!(
            matches!(&chips[0].tree, wire::Node::Label { text, .. } if text == "chip"),
            "the chip tree reached the bar region",
        );
        // …and the panel reaches the dedicated panels mailbox, tree intact.
        let panel_cards = wait_for_region(&panels).await;
        assert_eq!(panel_cards.len(), 1, "the panel reached the panels mailbox");
        assert_eq!(panel_cards[0].plugin_id, "panelplug");
        assert!(
            matches!(
                &panel_cards[0].panel,
                Some(wire::Node::Label { text, .. }) if text == "panel body"
            ),
            "the panel tree survived intact into the panels mailbox",
        );

        // Now the plugin drops its panel (Some→None): the panels entry clears,
        // but its chip stays in the bar region.
        write_frame(
            &mut pwr,
            &PluginMsg::Render {
                tree: wire::Node::Label {
                    id: Some("chip".into()),
                    text: "chip2".into(),
                    classes: vec![],
                },
                panel: None,
                effects: vec![],
            },
        )
        .await
        .expect("send panel-less Render");

        // Poll until the panels mailbox drains (the reader clears it async).
        let mut cleared = false;
        for _ in 0..200 {
            if panels.lock_ref().is_empty() {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            cleared,
            "dropping the panel (Some→None) clears the panels entry"
        );
        assert_eq!(
            bar_center.lock_ref().len(),
            1,
            "the chip stays in the bar region after the panel is dropped",
        );
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

    /// #438: a **bar**-mounted plugin that subscribes `SlotVisible` is on-screen
    /// whenever its chip is (a bar chip has no sidebar-style hide), so the host
    /// seeds a constant `visible: true` and never feeds it the sidebar-open
    /// aggregate — otherwise a bar plugin parking pollers on `SlotVisible` (#288)
    /// would idle while fully visible. The sidebar aggregate starts `false` (a
    /// sidebar mount would be seeded `false` here), and sidebar edges must not
    /// reach the bar mount at all.
    #[tokio::test]
    async fn visibility_is_constant_true_for_bar_mounts() {
        let (_clock_tx, clock_rx) = watch::channel(None);
        let (vis_tx, vis_rx) = watch::channel(false);
        let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);

        let (host_end, plugin_end) = UnixStream::pair().expect("socketpair");
        tokio::spawn(async move { handle_conn(host_end, &ctx).await });

        let (mut prd, mut pwr) = plugin_end.into_split();
        let mut manifest = Manifest::new("barchip", Mount::BarCenter);
        manifest.subscribes = vec![StateKey::SlotVisible];
        write_frame(&mut pwr, &PluginMsg::Register { manifest })
            .await
            .expect("send Register");

        // The register seed is a constant `true` for a bar chip, despite the
        // sidebar aggregate being `false`.
        assert!(
            matches!(
                recv(&mut prd).await,
                HostMsg::SlotVisibility { visible: true }
            ),
            "a bar mount is seeded visible=true regardless of sidebar state",
        );

        // Sidebar edges must not reach a bar mount — its chip visibility is
        // independent of every sidebar. Drive a couple; the constant task already
        // sent its one seed and returned, so nothing more may arrive.
        vis_tx.send_replace(true);
        vis_tx.send_replace(false);
        let quiet = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            read_frame::<HostMsg, _>(&mut prd),
        )
        .await;
        assert!(
            quiet.is_err(),
            "a bar mount receives no sidebar-driven visibility edges (only the seed)",
        );
    }

    // ── Containment (#435) ────────────────────────────────────────────────────

    /// The effect rate cap is a token bucket: a plugin may fire a full
    /// [`EFFECT_BURST`] back-to-back, then is limited to the sustained refill;
    /// a long idle refills back up to (but never beyond) the burst cap. Driven
    /// with synthetic instants so it's deterministic.
    #[test]
    fn effect_rate_limiter_caps_sustained_but_allows_burst() {
        let t0 = Instant::now();
        let mut rl = EffectRateLimiter::new_at(t0);

        // The whole burst is available up front, then the bucket is empty.
        for _ in 0..EFFECT_BURST {
            assert!(rl.allow(t0), "burst tokens available immediately");
        }
        assert!(!rl.allow(t0), "burst exhausted at the same instant");

        // One refill interval later, exactly one more effect is allowed.
        let t1 = t0 + Duration::from_secs(1);
        assert!(rl.allow(t1), "one token refilled after 1s");
        assert!(!rl.allow(t1), "only one token per refill interval");

        // A long idle refills to the burst cap — and saturates there, so idle
        // time can't bank unbounded budget for a later flood.
        let t2 = t1 + Duration::from_secs(100);
        for _ in 0..EFFECT_BURST {
            assert!(
                rl.allow(t2),
                "bucket refills up to the burst cap after idle"
            );
        }
        assert!(!rl.allow(t2), "refill saturates at the burst cap");
    }

    /// #435: a peer that dials the socket but never sends `Register` must be
    /// dropped after `REGISTER_TIMEOUT`, not park the connection task forever.
    /// Paused-time so the 10s wall-clock timeout resolves instantly; the plugin
    /// end is held open (never written) so the handshake read stays pending and
    /// the *timeout* — not an EOF — is what ends the connection.
    #[tokio::test(start_paused = true)]
    async fn handshake_timeout_drops_a_silent_connection() {
        let (_clock_tx, clock_rx) = watch::channel(None);
        let (_vis_tx, vis_rx) = watch::channel(false);
        let (ctx, _effects_rx) = ctx_with(clock_rx, vis_rx);

        let (host_end, _plugin_end) = UnixStream::pair().expect("socketpair");
        let conn = tokio::spawn(async move { handle_conn(host_end, &ctx).await });

        // Let the task arm its handshake timeout, then jump past it.
        tokio::task::yield_now().await;
        tokio::time::advance(REGISTER_TIMEOUT + Duration::from_secs(1)).await;

        tokio::time::timeout(Duration::from_secs(5), conn)
            .await
            .expect("handle_conn returns after the handshake timeout")
            .expect("conn task joined cleanly");
    }
}
