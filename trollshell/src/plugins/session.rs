//! tokio-side: per-connection lifecycle.
//!
//! [`handle_conn`] drives one plugin connection — handshake, the four opt-in
//! host→plugin push tasks (clock / visibility / accent / spectrum), the reader
//! loop feeding renders into the mount mailboxes, and the shared teardown. It
//! also carries the containment (#435) and registration-hygiene (#436) guards:
//! the bounded outbound queue, the liveness ping, the effect rate cap, the
//! per-id [`IdGuard`], and capability enforcement ([`enforce_capabilities`]).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hytte::services::pipewire;
use hytte_plugin_proto::{
    AudioSpectrum, Capability, ClockState, Effect, HostMsg, LogLevel, Mount, PluginMsg, StateKey,
    StateSnapshot, read_frame, write_frame,
};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{mpsc, watch};

use super::region::{clear_region_if_owned, upsert_region};
use super::{BrokeredEffect, ListenerCtx, SlotRender};

/// Monotonic per-connection token. Stamped on every [`SlotRender`] a connection
/// parks so a card's ownership is **connection-scoped, not plugin-id-scoped**: a
/// fast-reconnecting plugin (the SDK backs off from 100 ms) can have its new
/// connection replace its region entry before the old connection's teardown
/// runs, and a plugin-id-only compare would let the stale teardown evict the
/// live successor (#278). The generation compare cannot — each connection has a
/// unique token, so teardown removes a card only when the *same connection*
/// still owns it (see [`clear_region_if_owned`]).
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Count of connected plugins subscribing [`StateKey::AudioSpectrum`] (#405).
/// The capture tap is toggled active on the 0→1 edge and inactive on the 1→0
/// edge, so the default sink's monitor is only tapped while a plugin actually
/// consumes it — an idle desktop (or one with no audio-reactive plugin) pays
/// nothing.
static SPECTRUM_SUBSCRIBERS: AtomicUsize = AtomicUsize::new(0);

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
            // The connection's outbound, so a two-way effect (consent, #487) can
            // send its reply frame back to this plugin.
            outbound: render.outbound.clone(),
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
pub(super) const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);

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
pub(super) const OUTBOUND_CAPACITY: usize = 256;

/// Max effect tokens a connection may hold — the burst of [`Effect`]s it can emit
/// back-to-back before the sustained cap ([`EFFECT_REFILL_PER_SEC`]) applies.
pub(super) const EFFECT_BURST: u32 = 8;

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
pub(super) struct EffectRateLimiter {
    tokens: f64,
    last: Instant,
}

impl EffectRateLimiter {
    fn new() -> Self {
        Self::new_at(Instant::now())
    }

    pub(super) fn new_at(now: Instant) -> Self {
        Self {
            tokens: f64::from(EFFECT_BURST),
            last: now,
        }
    }

    /// Refill by the time elapsed since the last call (capped at the burst), then
    /// try to spend one token. `true` = allowed, `false` = over budget (drop).
    pub(super) fn allow(&mut self, now: Instant) -> bool {
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

// ── Registration hygiene (#436) ──────────────────────────────────────────────
//
// Three Register/lifecycle guards so the blessed dev workflow (a `cargo run`
// beside the deployed user service) can't corrupt the live shell: the host
// won't steal a live sibling's socket (`socket_in_use`, applied in `listen`),
// one id owns at most one live connection ([`IdGuard`]), and an effect a plugin
// didn't request the capability for is dropped ([`enforce_capabilities`]).

/// RAII claim on a plugin id within one host's live-id set (#436). Held for a
/// connection's lifetime and released on drop (teardown), so a legitimate
/// reconnect reclaims the id. Scoped to the host (the [`ListenerCtx`]) rather
/// than process-wide: "one live connection per id **on this host**", which is
/// also what keeps the per-connection tests isolated from one another.
pub(super) struct IdGuard {
    ids: Arc<Mutex<HashSet<String>>>,
    id: String,
}

impl IdGuard {
    /// Claim `id` in `ids` for this connection, or `None` if another live
    /// connection on the same host already holds it (the caller rejects the
    /// duplicate). `HashSet::insert` returning `false` — the id was already
    /// present — is exactly the "already connected" test.
    pub(super) fn claim(ids: &Arc<Mutex<HashSet<String>>>, id: &str) -> Option<Self> {
        let inserted = ids
            .lock()
            .expect("live plugin id set poisoned")
            .insert(id.to_owned());
        inserted.then(|| Self {
            ids: ids.clone(),
            id: id.to_owned(),
        })
    }
}

impl Drop for IdGuard {
    fn drop(&mut self) {
        self.ids
            .lock()
            .expect("live plugin id set poisoned")
            .remove(&self.id);
    }
}

/// The [`Capability`] a given [`Effect`] requires. Exhaustive over the effect
/// vocabulary so adding an effect variant is a compile error here (it must
/// declare which cap gates it), mirroring the wire↔host mapping tables below.
pub(super) fn effect_capability(effect: &Effect) -> Capability {
    match effect {
        Effect::OpenPage(_) => Capability::OpenPage,
        Effect::Niri(_) => Capability::Niri,
        Effect::Media(_) => Capability::Media,
        Effect::Audio(_) => Capability::Audio,
        Effect::RunCommand { .. } => Capability::RunCommand,
        Effect::RaiseOsd { .. } => Capability::RaiseOsd,
        Effect::Notify { .. } => Capability::Notify,
        Effect::RequestConsent { .. } => Capability::Consent,
    }
}

/// Drop any effect whose required [`Capability`] the plugin didn't declare in
/// its manifest (#436). Host-side capability enforcement: the manifest's
/// `capabilities` are the grant set, and an effect requesting an ungranted cap
/// is skipped with a warn rather than brokered — making the manifest's
/// documented "the host auto-grants from the manifest" model actually true
/// (before this, any connected same-user process could emit `Notify`/`RaiseOsd`/
/// `OpenPage` without requesting the cap). Runs in the reader **before** the rate
/// cap so an ungranted flood costs no [`EffectRateLimiter`] tokens.
pub(super) fn enforce_capabilities(
    granted: &[Capability],
    plugin_id: &str,
    effects: Vec<Effect>,
) -> Vec<Effect> {
    effects
        .into_iter()
        .filter(|effect| {
            let cap = effect_capability(effect);
            if granted.contains(&cap) {
                true
            } else {
                tracing::warn!(
                    plugin = %plugin_id,
                    ?effect,
                    ?cap,
                    "plugin effect requires a capability it didn't declare; dropped",
                );
                false
            }
        })
        .collect()
}

/// Drive one plugin connection: handshake, then read frames until the peer
/// disconnects, feeding renders into the mount mailbox and pushing state
/// snapshots + events back out.
// One cohesive per-connection lifecycle (handshake → the four opt-in push tasks
// → reader loop → teardown); splitting it would scatter the paired setup/abort
// of each task across helpers for no readability gain.
#[allow(clippy::too_many_lines)]
pub(super) async fn handle_conn(stream: UnixStream, ctx: &ListenerCtx) {
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
    // #436: an empty id can't key a region card or the audit log — reject it
    // outright (the connection is dropped, nothing is mounted).
    if plugin_id.is_empty() {
        tracing::warn!("plugin Register carried an empty id; dropping the connection");
        return;
    }
    // #436: one live connection per plugin id on this host. A second Register
    // for an id already connected (e.g. a dev binary dialing the same socket as
    // the systemd-launched unit) would otherwise have both connections
    // alternately overwrite one region card — the card flaps and events route to
    // whichever rendered last, silently. Claim the id for this connection's
    // lifetime: the duplicate is rejected here with a deterministic outcome (the
    // incumbent keeps the card, the newcomer is dropped) rather than left to
    // fight. The claim releases on teardown (RAII, dropped last), so a legitimate
    // reconnect — which the SDK backs off ≥100 ms before — reclaims the id.
    let Some(_id_guard) = IdGuard::claim(&ctx.live_ids, &plugin_id) else {
        tracing::warn!(
            plugin = %plugin_id,
            "plugin id already has a live connection; rejecting the duplicate",
        );
        return;
    };
    let mount = manifest.mount;
    // Region sort key (advisory placement request); `None` sorts as `0` (#274).
    let order = manifest.order.unwrap_or(0);
    // The manifest's granted capability set (#436), consulted per render frame in
    // the reader to drop effects the plugin never declared a cap for.
    let capabilities = manifest.capabilities.clone();
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
                    // #436: drop any effect whose capability the plugin never
                    // declared, THEN rate-cap the survivors (#435) so an
                    // ungranted flood costs no tokens. Both are host policy — the
                    // plugin may request anything; the host decides what runs.
                    throttle_effects(
                        &mut effect_rl,
                        &plugin_id,
                        enforce_capabilities(&capabilities, &plugin_id, effects),
                    ),
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
