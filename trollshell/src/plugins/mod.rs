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
//! per-monitor open flag ([`SLOT_VISIBILITY_BY_MONITOR`](pump), fed by
//! `sidebar.rs` via [`set_sidebar_visibility`] / [`forget_sidebar_visibility`] —
//! the latter on monitor hot-unplug, so a disappearing monitor that held the only
//! open sidebar drops `visible` to `false`) and publishes the aggregate only when
//! it actually changes.
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
//!   sides share one reconciler path ([`build_region`](region)); a bar region
//!   just lays its cards out horizontally with the chip class. `SidebarLead`
//!   (#301) leads the sidebar — its cards render *above* the built-in
//!   weather/calendar/tasks cards, which `SidebarTop`/`SidebarBottom` (mounted
//!   after them) cannot. A plugin (chip or card) may **also** define an optional
//!   drawer *panel* (#349 PR2): a second `Node` tree carried on the render
//!   frame's `panel` field, parked in the dedicated [`PluginHandles::panels`]
//!   mailbox and rendered by the per-monitor plugin drawer child
//!   ([`plugin_panel_slot`]); the plugin opens it by emitting
//!   `Effect::OpenPage(Page::PluginSelf)`. A chip/card need not have a panel
//!   (`panel: None`).
//! - **State:** [`StateKey::Clock`] (the snapshot pump), plus the opt-in
//!   host→plugin pushes gated on their own keys — [`StateKey::SlotVisible`]
//!   (#288), [`StateKey::Accent`] (#376), and [`StateKey::AudioSpectrum`] (the
//!   ~20 Hz audio tap, #405).
//! - **Effects:** [`Effect::OpenPage`] (→ the modal drawer, incl. `PluginSelf`
//!   → the plugin's own panel, #349 PR2), [`Effect::RaiseOsd`] (→ the transient
//!   OSD nudge, #236), and [`Effect::Notify`] (→ a local notification toast
//!   through the shell's own daemon, #406) are brokered; every other effect is
//!   logged "unsupported in v1" and skipped. Capability **enforcement** is host
//!   policy (#436): an effect whose [`Capability`] the plugin didn't declare in
//!   its manifest is dropped with a warn in the connection reader
//!   ([`enforce_capabilities`](session)) before it ever reaches the broker,
//!   making the manifest's "the host auto-grants from the manifest" model true.
//!   A persisted audit-log and the `RunCommand` round-trip remain deferred.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use hytte::adw;
use hytte::futures_signals::signal::Mutable;
use hytte::gtk::{glib, prelude::*};
use hytte::prelude::*;
use hytte::reactive::registry;
use hytte::reactive::spawn_supervised;
use hytte::services::{clock, niri, pipewire};
use hytte_plugin_proto::{AudioSpectrum, ClockState, Effect, HostMsg, wire};
use tokio::sync::{mpsc, watch};

mod effects;
mod listener;
mod pump;
mod region;
mod session;
mod wire_map;

#[cfg(test)]
mod tests;

pub use pump::{forget_sidebar_visibility, set_sidebar_visibility};
pub use region::{
    bar_center_slot, bar_left_slot, bar_right_slot, plugin_panel_slot, set_active_panel,
    sidebar_bottom_slot, sidebar_lead_slot, sidebar_top_slot,
};

// ── Service ─────────────────────────────────────────────────────────────────

/// The plugin host transport service. Registered in `main.rs` via `App::with`.
pub struct PluginsService;

/// One plugin's rendered card parked in a mount region's coalescing mailbox: the
/// producing plugin's `id` and requested `order` (region sort key), the
/// declarative `tree`, the connection generation token that produced it (see
/// `session::NEXT_GENERATION`), and a handle to send frames **back** to that
/// connection (event round-trip). Effects do **not** ride here — they are
/// one-shot, so they go down the non-lossy effect channel instead (#277). `Clone`
/// so it can ride a `Mutable` signal to the GTK reconcilers.
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
/// channel for the GTK-side broker. Carries the producing plugin's id for the
/// audit log, and its connection's `outbound` so a **two-way** effect (consent,
/// #487) can route its reply back to the plugin as a [`HostMsg`]; one-way
/// effects ignore it.
struct BrokeredEffect {
    plugin_id: String,
    effect: Effect,
    outbound: mpsc::Sender<HostMsg>,
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
    /// Routing it through the same `upsert_region`/`clear_region_if_owned` as the
    /// six regions inherits the #278 generation guard for free.
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
    /// (via `pump::publish_accent`), subscribed from tokio (per-conn accent
    /// tasks). Starts `None` (unresolved) — a plugin then keeps the kit's
    /// hard-coded default until the value lands. Re-published on every
    /// accent/scheme change, not just at startup (#396).
    accent_tx: watch::Sender<Option<[u8; 4]>>,
    /// The latest audio-reactive spectrum off the default sink's monitor (#405),
    /// pumped from `pipewire::audio_spectrum()` on the GTK thread ([`install`]
    /// via `pump::publish_spectrum`) and forwarded to spectrum-subscribing
    /// plugins from tokio (per-conn spectrum tasks). Starts `None` (capture
    /// inactive); the tap only runs while a subscriber exists.
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
    /// This host's set of currently-connected plugin ids (#436). One live
    /// connection per id: a second `Register` for an id already present is
    /// rejected (see `session::IdGuard`), so two connections can't fight over one
    /// region card. An `Arc<Mutex<…>>` because it's shared across the tokio
    /// per-connection tasks and only ever touched briefly (claim/release), never
    /// across an await. Scoped to the host (not process-global) so the
    /// per-connection tests stay isolated from one another.
    live_ids: Arc<Mutex<HashSet<String>>>,
    effects_tx: mpsc::UnboundedSender<BrokeredEffect>,
}

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
            live_ids: Arc::new(Mutex::new(HashSet::new())),
            effects_tx,
        };
        spawn_supervised("plugins", move || {
            let ctx = ctx.clone();
            async move {
                if let Err(e) = listener::listen(&ctx).await {
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
        pump::set_clock(pump::to_clock_state(&dt));
        std::future::ready(())
    }));

    // Desktop accent (#376): resolve libadwaita's `@accent_color` once, now that
    // the display's CSS providers are up, and publish it so accent-subscribing
    // plugins can tint their `preem` widgets' default color to match the shell.
    // A failed resolve leaves `None`, so plugins keep the kit's hard-coded
    // default (no regression).
    pump::publish_accent(pump::resolve_accent_color());

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
        pump::publish_accent(pump::resolve_accent_color());
    });
    style_manager.connect_dark_notify(|_| {
        pump::publish_accent(pump::resolve_accent_color());
    });

    // Audio spectrum pump (#405): project the live `pipewire::audio_spectrum()`
    // (a services `AudioSpectrum`, or `None` while the tap is inactive) onto the
    // GTK-free wire `AudioSpectrum` and publish it on the watch channel the
    // per-conn spectrum tasks subscribe to. The signal replays its current value
    // on subscribe, so this is up to date the moment a plugin dials in.
    glib::MainContext::default().spawn_local(pipewire::audio_spectrum().for_each(|spectrum| {
        pump::publish_spectrum(spectrum.map(pump::to_wire_spectrum));
        std::future::ready(())
    }));

    // Focused output (#499, deferred #440 hunk): track niri's focused output so a
    // plugin-driven drawer-open (`Effect::OpenPage`) and the consent overlay
    // (#487) both land on the output the user is looking at, not an arbitrary one.
    // Local tracking pending the shared `components::focused_output` component
    // (#496/#440) — fold this in once that lands. Replays its current value on
    // subscribe, so it's up to date the moment a plugin/prompt needs it.
    glib::MainContext::default().spawn_local(niri::focused_output().for_each(|out| {
        FOCUSED_OUTPUT.with(|c| *c.borrow_mut() = out);
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
                effects::broker_effect(&brokered.plugin_id, &brokered.effect, &brokered.outbound);
            }
        });
    }
}

thread_local! {
    /// niri's most recent focused-output connector, tracked by the subscription
    /// [`install`] wires (#499, deferred #440 hunk). GTK-thread-only. Read by
    /// [`focused_output`] to route a plugin-driven drawer-open / consent prompt to
    /// the output the user is on. `None` until niri reports one (callers then fall
    /// back to a default output). Local pending the `components::focused_output`
    /// consolidation (#496/#440).
    static FOCUSED_OUTPUT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// niri's current focused-output connector name, or `None` if not yet known
/// (#499). GTK-thread-only. Feeds the drawer-open routing (`effects::broker_effect`)
/// and the consent overlay ([`crate::overlays::consent`]).
pub(crate) fn focused_output() -> Option<String> {
    FOCUSED_OUTPUT.with(|c| c.borrow().clone())
}
