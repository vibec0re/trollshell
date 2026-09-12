//! tokio-side: per-connection lifecycle.
//!
//! [`serve_conn`] drives one plugin connection — handshake, the four opt-in
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
    AudioSpectrum, Capability, ClockState, DatasourceError, DatasourceOutcome, Effect, HostMsg,
    LogLevel, MAX_BODY_TEXT_BYTES, MAX_DISPLAY_TEXT_BYTES, Manifest, Mount, NowPlaying, PluginMsg,
    ProtoError, StateKey, StateSnapshot, UpcomingEvent, VOCAB, read_frame, write_frame,
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

/// Count of spectrum subscribers that are currently **on-screen** (#405/#559).
/// The capture tap is toggled active on the 0→1 edge and inactive on the 1→0
/// edge, so the default sink's monitor is only tapped while a plugin is both
/// subscribed to [`StateKey::AudioSpectrum`] **and** visible (its sidebar is
/// open, or it is bar-mounted — always on-screen). Before #559 this counted
/// *connected* subscribers, which pinned the tap on all session because the
/// audio-widget plugin is a persistent unit; keying on visibility means a
/// closed sidebar drops the tap to inactive and an idle desktop genuinely pays
/// nothing. Each connection contributes at most one unit, owned by its
/// [`SpectrumDemand`] guard, so it can neither double-count nor miss a re-arm.
static SPECTRUM_SUBSCRIBERS: AtomicUsize = AtomicUsize::new(0);

/// Per-connection spectrum-tap demand state machine (#559). A spectrum
/// subscriber "demands" the capture tap only while on-screen; this owns the
/// connection's single contribution to the process-wide [`SPECTRUM_SUBSCRIBERS`]
/// refcount so the counting is unit-testable in isolation from the live socket
/// plumbing and the shared static.
pub(super) struct SpectrumGate {
    /// Whether this connection is currently contributing its one refcount unit.
    demanding: bool,
}

impl SpectrumGate {
    pub(super) fn new() -> Self {
        Self { demanding: false }
    }

    /// Flip this connection's demand and fold the ±1 into `count`, returning the
    /// tap-activation edge to drive: `Some(true)` when the count crosses 0→1
    /// (start the capture), `Some(false)` on 1→0 (stop it), and `None` when
    /// neither the demand nor the global edge changed. Idempotent per connection
    /// — a repeated `demand` is a no-op — so a connection contributes at most one
    /// unit and can neither double-count (an invisible→disconnect decrements
    /// exactly once) nor miss a re-arm (an invisible→visible re-adds). Takes the
    /// counter by reference so a test drives it over a local [`AtomicUsize`],
    /// never the shared static.
    pub(super) fn apply(&mut self, demand: bool, count: &AtomicUsize) -> Option<bool> {
        if demand == self.demanding {
            return None;
        }
        self.demanding = demand;
        if demand {
            (count.fetch_add(1, Ordering::SeqCst) == 0).then_some(true)
        } else {
            (count.fetch_sub(1, Ordering::SeqCst) == 1).then_some(false)
        }
    }
}

/// RAII spectrum-tap demand for one connection (#559): the [`SpectrumGate`]
/// state machine wired to the real [`SPECTRUM_SUBSCRIBERS`] refcount and
/// `pipewire::set_spectrum_active`. [`set`](SpectrumDemand::set) flips the
/// connection's demand and drives the tap's 0↔1 activation edge; `Drop` releases
/// a held unit — so a connection tearing down (its `spectrum_task` aborted)
/// while it was demanding fires the 1→0 deactivation exactly once, with no
/// inline decrement needed in teardown.
struct SpectrumDemand {
    gate: SpectrumGate,
}

impl SpectrumDemand {
    fn new() -> Self {
        Self {
            gate: SpectrumGate::new(),
        }
    }

    fn set(&mut self, demand: bool) {
        if let Some(active) = self.gate.apply(demand, &SPECTRUM_SUBSCRIBERS) {
            pipewire::set_spectrum_active(active);
        }
    }
}

impl Drop for SpectrumDemand {
    fn drop(&mut self) {
        self.set(false);
    }
}

/// Route one plugin `Render` frame (#274 / #277 / #349 PR2): strip its one-shot
/// effects onto the global non-lossy broker channel, park (or clear) its optional
/// drawer panel in the dedicated `panels` mailbox, and upsert its chip/card
/// `tree` into the mount's region mailbox (latest-wins per plugin id). Factored
/// out of [`serve_conn`] so that reader loop stays within the line budget.
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
///
/// **It bounds frames, not bytes**, which is why it is not the whole story
/// (#1165 item 7). Every other `HostMsg` is a small, host-built value — a clock
/// tick, an accent colour, a spectrum frame — but the two datasource legs carry
/// *plugin*-supplied opaque payloads, bounded on the wire only by
/// `MAX_FRAME_LEN`. 256 × 16 MiB is four gigabytes of queue behind one plugin
/// that stopped reading, so those payloads carry their own byte cap:
/// [`MAX_DATASOURCE_PAYLOAD_BYTES`].
pub(super) const OUTBOUND_CAPACITY: usize = 256;

/// The largest opaque datasource payload the host will forward, in bytes
/// (#1165 item 7) — [`Effect::DatasourceQuery`]'s `params` and
/// [`Effect::DatasourceResult`]'s `Ready` body.
///
/// These are the only plugin-supplied blobs that ride the host→plugin outbound
/// queue, and [`OUTBOUND_CAPACITY`] counts frames rather than bytes, so without
/// this a stalled provider's queue is bounded at 256 × `MAX_FRAME_LEN` = 4 GiB.
/// With it, 64 MiB — the same order as every other buffer the host holds.
///
/// **256 KiB** is generous for a query answer: a departures board, a weather
/// digest or an agent roster is single-digit kilobytes of JSON, so this is two
/// orders of magnitude of headroom. A datasource that genuinely needs to move
/// more than this is asking for a file path or a socket, not a reply frame.
///
/// **Refused, not truncated** — the opposite of the display-string caps, and
/// for a stated reason: these payloads are opaque JSON, and a JSON document cut
/// at 256 KiB is not a smaller document, it is a parse error at the far end. A
/// refusal the requester can see beats a corruption it cannot.
pub(super) const MAX_DATASOURCE_PAYLOAD_BYTES: usize = 256 * 1024;

/// Max effect tokens a **plugin** may hold — the burst of [`Effect`]s it can emit
/// back-to-back before the sustained cap ([`EFFECT_REFILL_PER_SEC`]) applies.
///
/// Per plugin id since #1165, not per connection: the bucket used to live in
/// `handle_conn`'s stack frame, so a crash-looping plugin got a **fresh burst
/// on every reconnect** — and the SDK backs off from 100 ms, which makes the
/// sustained cap a ~10× multiple of its stated value for exactly the plugin the
/// cap exists for. See [`EffectBuckets`].
pub(super) const EFFECT_BURST: u32 = 8;

/// Sustained effect budget refilled per second (a token-bucket rate). Together
/// with [`EFFECT_BURST`] this caps how fast a plugin can flood the drawer / OSD /
/// toast broker; user-driven effects (a click → `OpenPage`) never approach it.
const EFFECT_REFILL_PER_SEC: f64 = 1.0;

/// Max connector names a single [`PluginMsg::Render`]'s `hidden_on` may carry
/// (#1058, from PR #1068's review: the field shipped with no length cap at
/// all, bounded only by the 16 MiB frame limit). A real machine has a handful
/// of outputs; 64 is generous headroom over any plausible fan-out, the same
/// "generous but bounded, not tight" posture as the shader node's
/// [`MAX_SHADER_SOURCE_BYTES`](hytte_plugin_proto::MAX_SHADER_SOURCE_BYTES) /
/// [`MAX_SHADER_DATA_BYTES`](hytte_plugin_proto::MAX_SHADER_DATA_BYTES).
///
/// This bounds **retention**, not the decode-time allocation: `read_frame`
/// has already materialized the whole `Vec<String>` off the wire by the time
/// [`capped_hidden_on`] runs (`read_frame` itself is bounded by the 16 MiB
/// frame limit). What the cap actually saves is what [`SlotRender`] stores
/// long-term and what every monitor's reconciler re-compares and
/// `clone_from`s each frame (review LOW-2).
pub(super) const MAX_HIDDEN_ON_ENTRIES: usize = 64;

/// Max bytes in one `hidden_on` connector name. A real Wayland/DRM connector
/// name (`"DP-2"`, `"HDMI-A-1"`, …) is a handful of ASCII bytes; 64 is
/// generous headroom, not a tight fit.
pub(super) const MAX_HIDDEN_ON_NAME_BYTES: usize = 64;

/// Which shape violation [`capped_hidden_on`] latches (#1058 review MEDIUM-2):
/// a `hidden_on` set is either over the entry-count cap or carries an
/// oversized name — kept as separate slots, mirroring `preem_render::Warned`'s
/// one-slot-per-diagnostic shape, so a set that trips one after already
/// tripping the other still gets both messages once each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum HiddenOnViolation {
    TooManyEntries,
    NameTooLong,
}

/// Enforce the [`MAX_HIDDEN_ON_ENTRIES`] / [`MAX_HIDDEN_ON_NAME_BYTES`] shape
/// cap on a decoded `Render.hidden_on` (#1058). Unlike [`enforce_capabilities`]
/// this isn't a declaration check, and a violation is not a reason to drop the
/// connection or the frame — `tree`/`panel`/`effects` are still perfectly
/// good. It degrades to the empty set instead: no `hidden_on` means the card
/// shows on every screen (#1050's own baseline), which is always a safe
/// default, so an oversized set costs the plugin its per-screen hiding for
/// that one frame rather than the whole render.
///
/// Pure — it does no logging itself, returning the survivors plus at most one
/// `(HiddenOnViolation, message)` pair for the caller to warn with. This is
/// deliberate (review MEDIUM-2): a plugin's `hidden_on` set does not change
/// frame to frame, so an over-cap set is over-cap on **every** frame for the
/// life of the connection — at the SDK's own ~30 Hz view-rate cap, an
/// unlatched warning would be ~30 journal lines a second, forever, matching
/// exactly the reasoning `shader_map::warn`'s latch documents for the shader
/// caps. `violated` is the per-connection latch (owned by the caller, reset
/// on reconnect — the same shape as `enforce_capabilities`'s
/// `capability_warned` in `hytte-plugin`), so the message comes back only the
/// first time a given kind is violated this connection; every frame after
/// that still gets the empty set, just silently.
pub(super) fn capped_hidden_on(
    hidden_on: Vec<String>,
    violated: &mut std::collections::HashSet<HiddenOnViolation>,
) -> (Vec<String>, Option<(HiddenOnViolation, String)>) {
    if hidden_on.len() > MAX_HIDDEN_ON_ENTRIES {
        let entries = hidden_on.len();
        let msg = format!(
            "Render.hidden_on carries {entries} connector names, over the {MAX_HIDDEN_ON_ENTRIES} cap; treating it as empty (card shows on every screen)"
        );
        let violation = violated
            .insert(HiddenOnViolation::TooManyEntries)
            .then_some((HiddenOnViolation::TooManyEntries, msg));
        return (Vec::new(), violation);
    }
    if let Some(over) = hidden_on
        .iter()
        .find(|name| name.len() > MAX_HIDDEN_ON_NAME_BYTES)
    {
        let len = over.len();
        let msg = format!(
            "Render.hidden_on carries a connector name {len} bytes long, over the {MAX_HIDDEN_ON_NAME_BYTES} cap; treating the set as empty (card shows on every screen)"
        );
        let violation = violated
            .insert(HiddenOnViolation::NameTooLong)
            .then_some((HiddenOnViolation::NameTooLong, msg));
        return (Vec::new(), violation);
    }
    (hidden_on, None)
}

/// Per-connection latch for the effect-level payload caps (#1165), keyed by
/// effect **kind**.
///
/// `std::mem::Discriminant<Effect>` rather than the effect itself: an effect
/// carries the very strings being capped, so keying on the value would make the
/// latch a per-payload memo — unbounded, and useless (a plugin that raises a
/// counter in an over-cap OSD title would get a fresh line every frame). The
/// kind is what an author fixes, so the kind is what is latched.
///
/// Per connection, like [`EffectRateLimiter`]'s own scope and
/// `capped_hidden_on`'s `violated` set: a reconnect re-arms it, so the same
/// mistake is named once per connection rather than once per frame or once for
/// the life of the shell.
pub(super) type EffectWarnLatch = HashSet<std::mem::Discriminant<Effect>>;

/// Cap the human-facing strings a plugin puts in front of the user through an
/// effect (#1165) — the non-node half of the display-text cap that
/// `wire_map::map_node` applies to a render tree.
///
/// The three effects here are the ones whose payload becomes a widget on the
/// **GTK main thread** without passing through `wire_map` at all:
/// [`Effect::RaiseOsd`] sets a `gtk::Label` in the OSD overlay,
/// [`Effect::Notify`] goes through the shell's own notification daemon, and
/// [`Effect::RequestConsent`] renders four fields into the consent card. Each is
/// bounded only by `MAX_FRAME_LEN` on the wire, so an 8 MiB
/// `RaiseOsd { title }` is a legal frame that stalls the main loop exactly as an
/// 8 MiB `Node::Label` does.
///
/// Single-line fields (a title, a summary, an icon name, the consent card's
/// agent/datasource/scope) take
/// [`MAX_DISPLAY_TEXT_BYTES`](hytte_plugin_proto::MAX_DISPLAY_TEXT_BYTES);
/// bodies and the consent detail take
/// [`MAX_BODY_TEXT_BYTES`](hytte_plugin_proto::MAX_BODY_TEXT_BYTES). **Truncate,
/// never refuse**: an OSD nudge whose title is cut still tells the user
/// something, and dropping the effect would make a plugin bug look like a dead
/// click.
///
/// Exhaustive over the effect vocabulary, like
/// [`Effect::required_capability`](hytte_plugin_proto::Effect::required_capability)
/// and `effects::effect_kind`, so an effect variant that grows a human-facing
/// string is a compile error here rather than a silent hole. The no-op arms say
/// why they are no-ops.
///
/// Pure, like [`capped_hidden_on`]: it returns the capped effect plus at most
/// one message for the caller to warn with, and `warned` is the caller's
/// per-connection latch.
pub(super) fn capped_effect_strings(
    mut effect: Effect,
    warned: &mut EffectWarnLatch,
) -> (Effect, Option<String>) {
    let mut longest = 0usize;
    let mut cut = |s: &mut String, max: usize| {
        if s.len() > max {
            longest = longest.max(s.len());
            *s = super::effects::truncate_on_char_boundary(s, max);
        }
    };
    match &mut effect {
        Effect::RaiseOsd { title, body, icon } => {
            cut(title, MAX_DISPLAY_TEXT_BYTES);
            cut(body, MAX_BODY_TEXT_BYTES);
            if let Some(icon) = icon.as_mut() {
                cut(icon, MAX_DISPLAY_TEXT_BYTES);
            }
        }
        Effect::Notify { summary, body } => {
            cut(summary, MAX_DISPLAY_TEXT_BYTES);
            cut(body, MAX_BODY_TEXT_BYTES);
        }
        Effect::RequestConsent {
            agent,
            datasource,
            scope,
            detail,
            ..
        } => {
            cut(agent, MAX_DISPLAY_TEXT_BYTES);
            cut(datasource, MAX_DISPLAY_TEXT_BYTES);
            cut(scope, MAX_DISPLAY_TEXT_BYTES);
            cut(detail, MAX_BODY_TEXT_BYTES);
        }
        // No human-facing strings at all: these carry enum payloads the host
        // maps onto its own actions.
        Effect::OpenPage(_) | Effect::Niri(_) | Effect::Media(_) | Effect::Audio(_) => {}
        // `argv` is a program invocation, not a display string: nothing renders
        // it, and `execve`'s own `ARG_MAX` is the bound that actually applies.
        Effect::RunCommand { .. } => {}
        // Capped in the broker by `MAX_URI_BYTES` (#1045), which refuses rather
        // than truncates — a cut URI is a different destination, so truncation
        // would be the wrong degradation here.
        Effect::OpenUri { .. } => {}
        // The datasource legs carry opaque JSON and identifiers, not display
        // strings; their bound is `MAX_DATASOURCE_PAYLOAD_BYTES`, applied by
        // `capped_effect_payload`, and it refuses rather than cuts — for the
        // same reason as a URI (#1165 item 7).
        Effect::DatasourceQuery { .. } | Effect::DatasourceResult { .. } => {}
    }
    let message = (longest > 0 && warned.insert(std::mem::discriminant(&effect))).then(|| {
        format!(
            "plugin effect carries a display string {longest} B long, over the host's \
             {MAX_DISPLAY_TEXT_BYTES} B line / {MAX_BODY_TEXT_BYTES} B body cap; the prefix is \
             shown. Every one of these becomes a pango layout on the GTK main thread, which \
             shapes the whole run before it can measure it (further occurrences of this effect \
             kind are silenced for the rest of this connection)"
        )
    });
    (effect, message)
}

/// Enforce [`MAX_DATASOURCE_PAYLOAD_BYTES`] on the two datasource legs (#1165
/// item 7) — the only plugin-supplied blobs that ride a host→plugin outbound
/// queue whose bound counts frames rather than bytes.
///
/// `None` means the effect is refused outright. The two legs degrade
/// differently, because the host can answer one of them and not the other:
///
/// - **`DatasourceResult`** (a provider's answer to a parked query) is rewritten
///   to `Failed { error: Provider, … }`. The requester still gets its reply, on
///   the correlation it is waiting on, saying the provider's answer was
///   unusable — which is true, and is exactly what `DatasourceError::Provider`
///   is for.
/// - **`DatasourceQuery`** is dropped, and the requester gets nothing. Stated
///   plainly because it is the one asymmetry here: the refusal happens *before*
///   the router parks anything, so there is no correlation to fail and no
///   host-sourced "your request was too large" in the wire vocabulary to fail
///   it with. Adding one is a wire change, which this is deliberately not. A
///   plugin that hits it has a bug, and the journal line names it — the same
///   terms on which an ungranted effect is dropped with no reply.
///
/// `provider` and `scope` are bounded too: they are identifiers, so an
/// over-long one is refused rather than cut (a truncated name is a *different*
/// datasource), and they are formatted into the failure messages the router
/// sends back, which would otherwise be a second way onto the same queue.
///
/// Pure and latched like [`capped_effect_strings`], and a separate function
/// from it because the two answer different questions: that one bounds what a
/// human will look at, this one bounds what a queue will hold.
pub(super) fn capped_effect_payload(
    mut effect: Effect,
    warned: &mut EffectWarnLatch,
) -> (Option<Effect>, Option<String>) {
    let kind = std::mem::discriminant(&effect);
    let refuse = |what: &str, bytes: usize, warned: &mut EffectWarnLatch| {
        let message = warned.insert(kind).then(|| {
            format!(
                "plugin {what} is {bytes} B, over the host's {MAX_DATASOURCE_PAYLOAD_BYTES} B \
                 datasource payload cap; refused. The host→plugin queue bounds frames, not \
                 bytes, so an unbounded payload is an unbounded queue (further occurrences of \
                 this effect kind are silenced for the rest of this connection)"
            )
        });
        (None, message)
    };
    match &mut effect {
        Effect::DatasourceQuery {
            provider,
            scope,
            params,
            ..
        } => {
            if params.len() > MAX_DATASOURCE_PAYLOAD_BYTES {
                return refuse("DatasourceQuery params", params.len(), warned);
            }
            if provider.len() > MAX_DISPLAY_TEXT_BYTES {
                return refuse("DatasourceQuery provider name", provider.len(), warned);
            }
            if scope.len() > MAX_DISPLAY_TEXT_BYTES {
                return refuse("DatasourceQuery scope name", scope.len(), warned);
            }
        }
        Effect::DatasourceResult { outcome, .. } => {
            let over = match outcome {
                DatasourceOutcome::Ready(payload) => {
                    (payload.len() > MAX_DATASOURCE_PAYLOAD_BYTES).then(|| payload.len())
                }
                // A failure's `message` is a human line, so it is cut rather
                // than refused — the requester learning *that* it failed
                // matters more than the tail of why.
                DatasourceOutcome::Failed { message, .. } => {
                    if message.len() > MAX_BODY_TEXT_BYTES {
                        *message =
                            super::effects::truncate_on_char_boundary(message, MAX_BODY_TEXT_BYTES);
                    }
                    None
                }
            };
            if let Some(bytes) = over {
                let message = warned.insert(kind).then(|| {
                    format!(
                        "plugin DatasourceResult payload is {bytes} B, over the host's \
                         {MAX_DATASOURCE_PAYLOAD_BYTES} B cap; the requester gets a Failed \
                         outcome instead of an unbounded frame (further occurrences of this \
                         effect kind are silenced for the rest of this connection)"
                    )
                });
                *outcome = DatasourceOutcome::Failed {
                    error: DatasourceError::Provider,
                    message: format!(
                        "provider payload is {bytes} B, over the host's \
                         {MAX_DATASOURCE_PAYLOAD_BYTES} B cap"
                    ),
                };
                return (Some(effect), message);
            }
        }
        // Everything else carries no opaque payload; the display-string caps in
        // `capped_effect_strings` are what bound their strings.
        Effect::OpenPage(_)
        | Effect::Niri(_)
        | Effect::Media(_)
        | Effect::Audio(_)
        | Effect::RunCommand { .. }
        | Effect::RaiseOsd { .. }
        | Effect::Notify { .. }
        | Effect::RequestConsent { .. }
        | Effect::OpenUri { .. } => {}
    }
    (Some(effect), None)
}

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

/// A token bucket: `burst` tokens to spend back-to-back, refilled at
/// `refill_per_sec`.
///
/// Extracted from [`EffectRateLimiter`] in #1165 because the effect cap is no
/// longer the only thing the host rate-limits — [`LogGate`] bounds
/// [`PluginMsg::Log`] and the broker bounds detached launches — and three
/// hand-copied `tokens/last` pairs would be three places for the refill
/// arithmetic to drift.
pub(super) struct TokenBucket {
    tokens: f64,
    last: Instant,
    burst: f64,
    refill_per_sec: f64,
}

impl TokenBucket {
    pub(super) fn new_at(now: Instant, burst: u32, refill_per_sec: f64) -> Self {
        Self {
            tokens: f64::from(burst),
            last: now,
            burst: f64::from(burst),
            refill_per_sec,
        }
    }

    /// What this bucket holds as of `now`, without spending anything.
    fn refilled(&self, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        (self.tokens + elapsed * self.refill_per_sec).min(self.burst)
    }

    /// Refill by the time elapsed since the last call (capped at the burst), then
    /// try to spend one token. `true` = allowed, `false` = over budget (drop).
    pub(super) fn allow(&mut self, now: Instant) -> bool {
        self.tokens = self.refilled(now);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Whether this bucket has refilled to its full burst as of `now`, i.e.
    /// whether it still remembers anything about what was spent.
    ///
    /// This is what makes a *keyed* bucket table sweepable (#1165 item 4): a
    /// full bucket is indistinguishable from a fresh one, so forgetting it
    /// changes no decision, while forgetting a partly-spent one would hand its
    /// owner a free burst.
    pub(super) fn is_full(&self, now: Instant) -> bool {
        self.refilled(now) >= self.burst
    }
}

/// Per-plugin effect rate cap (#435): a token bucket over [`Effect`]
/// emissions. A plugin may fire up to [`EFFECT_BURST`] effects back-to-back;
/// beyond that it's limited to [`EFFECT_REFILL_PER_SEC`], so a buggy plugin
/// emitting an effect per render can't flood the (deliberately non-lossy #277)
/// effect broker with drawer-opens / OSD nudges / toasts.
///
/// A thin newtype over [`TokenBucket`] so the two knobs live with the cap they
/// describe and callers cannot accidentally build an effect limiter with some
/// other plugin's budget.
pub(super) struct EffectRateLimiter(TokenBucket);

impl EffectRateLimiter {
    pub(super) fn new_at(now: Instant) -> Self {
        Self(TokenBucket::new_at(
            now,
            EFFECT_BURST,
            EFFECT_REFILL_PER_SEC,
        ))
    }

    /// Refill by the time elapsed since the last call (capped at the burst), then
    /// try to spend one token. `true` = allowed, `false` = over budget (drop).
    pub(super) fn allow(&mut self, now: Instant) -> bool {
        self.0.allow(now)
    }

    /// Whether this plugin's bucket has refilled completely — see
    /// [`TokenBucket::is_full`]. The predicate [`sweep_effect_buckets`] retires
    /// an entry on.
    fn is_full(&self, now: Instant) -> bool {
        self.0.is_full(now)
    }
}

/// Max [`PluginMsg::Log`] frames a connection may emit back-to-back before the
/// sustained cap ([`LOG_REFILL_PER_SEC`]) applies (#1165).
///
/// 32 is deliberately looser than [`EFFECT_BURST`]: a plugin's startup is
/// legitimately chatty (a handful of `debug!` lines per subsystem), and a log
/// line costs the host a journal write, not a drawer-open.
pub(super) const LOG_BURST: u32 = 32;

/// Sustained [`PluginMsg::Log`] budget refilled per second (#1165). Five lines a
/// second, forever, is well above what any bundled plugin emits and well below
/// what fills a journal.
const LOG_REFILL_PER_SEC: f64 = 5.0;

/// Max bytes in one [`PluginMsg::Log`] message (#1165).
///
/// The frame was bounded only by `MAX_FRAME_LEN`, so a plugin could push 16 MiB
/// into a single `tracing` event — and a journal line is not a widget, so this
/// costs disk and `journalctl` rather than the GTK thread. 4 KiB matches
/// [`MAX_DISPLAY_TEXT_BYTES`] and is far past any line worth reading;
/// `systemd-journald` has its own field limit above it, so the host cuts first
/// and says so rather than letting the journal silently do it.
pub(super) const MAX_LOG_MSG_BYTES: usize = 4 * 1024;

/// What the host should do with one inbound [`PluginMsg::Log`] frame (#1165).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum LogAdmission {
    /// Surface this message at the frame's level.
    Emit {
        /// The message, cut to [`MAX_LOG_MSG_BYTES`] on a char boundary.
        msg: String,
        /// `Some(original_len)` the **first** time this connection sent an
        /// over-cap message, so the truncation is named once rather than on
        /// every line.
        over_cap: Option<usize>,
    },
    /// Over the rate cap: drop the line.
    Drop {
        /// `true` the **first** time this connection is over budget — the one
        /// journal line that says logs are being dropped. Latched, because an
        /// unlatched "dropped" line is itself the flood.
        warn: bool,
    },
}

/// Per-connection [`PluginMsg::Log`] gate (#1165): the rate bucket plus the two
/// one-shot latches, kept as one value so the policy is unit-testable without a
/// socket.
pub(super) struct LogGate {
    bucket: TokenBucket,
    rate_warned: bool,
    len_warned: bool,
}

impl LogGate {
    pub(super) fn new_at(now: Instant) -> Self {
        Self {
            bucket: TokenBucket::new_at(now, LOG_BURST, LOG_REFILL_PER_SEC),
            rate_warned: false,
            len_warned: false,
        }
    }

    /// Decide one log frame's fate. The rate cap is checked **first**: an
    /// over-budget line is dropped whole, so a flood costs no truncation work.
    pub(super) fn admit(&mut self, msg: &str, now: Instant) -> LogAdmission {
        if !self.bucket.allow(now) {
            let warn = !self.rate_warned;
            self.rate_warned = true;
            return LogAdmission::Drop { warn };
        }
        if msg.len() > MAX_LOG_MSG_BYTES {
            let over_cap = (!self.len_warned).then_some(msg.len());
            self.len_warned = true;
            return LogAdmission::Emit {
                msg: super::effects::truncate_on_char_boundary(msg, MAX_LOG_MSG_BYTES),
                over_cap,
            };
        }
        LogAdmission::Emit {
            msg: msg.to_owned(),
            over_cap: None,
        }
    }
}

/// The host's effect rate buckets, keyed by **plugin id** and shared across its
/// connections (#1165 item 4).
///
/// Before this the bucket was a local in `handle_conn`, so it died with the
/// connection: a plugin that crash-loops — or one written to reconnect on
/// purpose — got a fresh [`EFFECT_BURST`] every time it dialled back in, and
/// the SDK's backoff starts at 100 ms. The cap that reads as "8 then 1/s" was
/// therefore "8 per reconnect" for exactly the plugin it exists to bound.
/// Keyed by id and held on the [`ListenerCtx`], it survives the reconnect.
///
/// Host-scoped (not process-global) for the reason `live_ids` is: the
/// per-connection tests stay isolated from one another.
pub(super) type EffectBuckets = Arc<Mutex<std::collections::HashMap<String, EffectRateLimiter>>>;

/// How many plugin ids the host will keep a bucket for at once (#1165 item 4).
///
/// The table is swept at every registration ([`sweep_effect_buckets`]), and a
/// *full* bucket is forgotten there because it is indistinguishable from a
/// fresh one — so what the table actually holds is "ids that spent an effect
/// token within the last refill window", a handful in any real session. This
/// cap is the backstop against a synthetic flood of distinct ids registering
/// faster than the sweep retires them: past it, an unknown id's effects are
/// **refused** rather than tracked. Refusing is the safe direction — reaching
/// this at all takes a thousand plugin ids actively spending effects, which is
/// the abuse, not a deployment.
pub(super) const MAX_TRACKED_EFFECT_BUCKETS: usize = 1024;

/// Forget every bucket that has refilled to its full burst as of `now` (#1165
/// item 4).
///
/// Called once per registration — the moment a new id may be about to add an
/// entry — rather than on a timer, so the table has no background cost at all.
/// Dropping a *full* bucket changes no decision (it is exactly a fresh one);
/// dropping a partly-spent one would hand its owner the free burst this whole
/// change exists to close, which is why the predicate is `is_full` and not an
/// age.
pub(super) fn sweep_effect_buckets(buckets: &EffectBuckets, now: Instant) {
    buckets
        .lock()
        .expect("plugin effect buckets poisoned")
        .retain(|_, bucket| !bucket.is_full(now));
}

/// Spend up to `count` tokens from `plugin_id`'s persistent bucket, returning
/// one verdict per effect **in order** (#1165 item 4).
///
/// The lock is held for pure arithmetic only — the warn and the audit write for
/// a refused effect happen after it is released, in [`throttle_effects`] — so a
/// slow `tracing` subscriber can never stall another connection's reader.
fn spend_effect_tokens(
    buckets: &EffectBuckets,
    plugin_id: &str,
    count: usize,
    now: Instant,
) -> Vec<bool> {
    let mut guard = buckets.lock().expect("plugin effect buckets poisoned");
    if !guard.contains_key(plugin_id) {
        // Borrow-first: the `to_owned()` is paid only on the insert that
        // actually adds an id, not on every frame of every connection.
        if guard.len() >= MAX_TRACKED_EFFECT_BUCKETS {
            guard.retain(|_, bucket| !bucket.is_full(now));
        }
        if guard.len() >= MAX_TRACKED_EFFECT_BUCKETS {
            return vec![false; count];
        }
        guard.insert(plugin_id.to_owned(), EffectRateLimiter::new_at(now));
    }
    let bucket = guard
        .get_mut(plugin_id)
        .expect("present, or inserted just above");
    (0..count).map(|_| bucket.allow(now)).collect()
}

/// Filter a render frame's effects through the plugin's rate limiter,
/// dropping (with a warn) any that exceed the cap. All effects in one frame share
/// a single `now`, so a burst frame depletes the bucket in order.
fn throttle_effects(
    buckets: &EffectBuckets,
    plugin_id: &str,
    effects: Vec<Effect>,
    warned: &mut EffectWarnLatch,
) -> Vec<Effect> {
    if effects.is_empty() {
        return effects;
    }
    let now = Instant::now();
    let verdicts = spend_effect_tokens(buckets, plugin_id, effects.len(), now);
    let mut kept = Vec::with_capacity(effects.len());
    for (effect, allowed) in effects.into_iter().zip(verdicts) {
        if allowed {
            kept.push(effect);
        } else {
            // #1165 item 5: latched per connection per effect *kind*, and
            // naming the kind rather than `Debug`-formatting the whole effect.
            // A plugin over the rate cap is over it on every frame, and the
            // effect it is over with carries the very payloads the other caps
            // in this file exist to bound — so the unlatched `?effect` line was
            // a per-frame journal write of arbitrary plugin-supplied bytes,
            // which is the flood it was reporting. The audit log below still
            // records every dropped effect: that is the per-occurrence record,
            // and it is rotated and bounded.
            if warned.insert(std::mem::discriminant(&effect)) {
                tracing::warn!(
                    plugin = %plugin_id,
                    effect = super::effects::effect_kind(&effect),
                    burst = EFFECT_BURST,
                    per_sec = EFFECT_REFILL_PER_SEC,
                    "plugin effect rate cap exceeded; dropped (further drops of this effect \
                     kind on this connection are silenced — the audit log still records each \
                     one)",
                );
            }
            super::effects::record_audit(
                plugin_id,
                &effect,
                super::effects::AuditDecision::DroppedRateCap,
                // A dropped effect never reaches a launch, so there is no unit
                // to name (#953 M1).
                None,
            );
        }
    }
    kept
}

// ── Registration hygiene (#436) ──────────────────────────────────────────────
//
// Three Register/lifecycle guards so the blessed dev workflow (a `cargo run`
// beside the deployed user service) can't corrupt the live shell: the host
// takes the socket rather than seizing it (`listener::take_socket` — an
// exclusive lock, then the `socket_in_use` probe, #996/#436), one id owns at
// most one live connection ([`IdGuard`]), and an effect a plugin didn't request
// the capability for is dropped ([`enforce_capabilities`]).

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

/// The [`Capability`] a domain [`StateKey`] push additionally requires (#484/#528),
/// or `None` for an *ambient* key whose subscription alone is the opt-in
/// (`Clock`/`SlotVisible`/`Accent`/`AudioSpectrum`). Exhaustive over the key
/// vocabulary so adding a `StateKey` is a compile error here until it declares
/// whether — and behind which capability — it is gated (the same compiler-forced
/// mapping shape as [`Effect::required_capability`], per #495). The domain keys carry personal /
/// privacy-relevant data, so the host requires the capability **on top of** the
/// subscription — a subscribe-only plugin is refused the push (see [`push_gate`]).
pub(super) fn state_key_capability(key: StateKey) -> Option<Capability> {
    match key {
        StateKey::Clock | StateKey::SlotVisible | StateKey::Accent | StateKey::AudioSpectrum => {
            None
        }
        StateKey::CalendarUpcoming => Some(Capability::Calendar),
        StateKey::SessionLocked => Some(Capability::SessionState),
        StateKey::NowPlaying => Some(Capability::NowPlaying),
    }
}

/// Whether the host should push `key` to this connection: the plugin subscribed
/// it **and**, for a capability-gated domain key ([`state_key_capability`]),
/// declared the gating capability. A subscription without the required capability
/// is refused with a warn — the same "declared *and* enforced" posture as
/// [`enforce_capabilities`], applied to a host→plugin push rather than an effect.
pub(super) fn push_gate(manifest: &Manifest, key: StateKey) -> bool {
    if !manifest.subscribes.contains(&key) {
        return false;
    }
    match state_key_capability(key) {
        None => true,
        Some(cap) => {
            if manifest.capabilities.contains(&cap) {
                true
            } else {
                tracing::warn!(
                    plugin = %manifest.id,
                    ?key,
                    ?cap,
                    "plugin subscribed a capability-gated state key without declaring the capability; push refused",
                );
                false
            }
        }
    }
}

/// Drop any effect whose required [`Capability`] the plugin didn't declare in
/// its manifest (#436). The manifest's `capabilities` are the grant set — every
/// one of them auto-granted, verbatim, at `Register` — and an effect requesting
/// a cap the plugin didn't declare is skipped with a warn rather than brokered
/// (before this, any connected same-user process could emit `Notify`/`RaiseOsd`/
/// `OpenPage` without requesting the cap). Runs in the reader **before** the rate
/// cap so an ungranted flood costs no [`EffectRateLimiter`] tokens.
///
/// The per-effect mapping is [`Effect::required_capability`] (#1058) — exported
/// from the proto crate so this and `hytte-plugin`'s own SDK-side guard read the
/// same table rather than two hand-kept copies that could drift. An effect with
/// no `required_capability` (`None`) needs no declaration and is never dropped
/// here.
///
/// **This is not a trust boundary, and #436 didn't make one** (#998). What it
/// enforces is *declaration*: a plugin stays inside the surface it asked for,
/// and the audit log names a real grant set. It does not gate a
/// higher-trust cap behind anything — a manifest that declares
/// [`Capability::RunCommand`] is granted argv execution, because the boundary
/// is the socket itself: `$XDG_RUNTIME_DIR`, `0700` dir, `0600` socket,
/// same-user-only by spec, and a same-uid process that can open it could
/// `systemd-run --user` its own plugin unit anyway. That is route 0, the model
/// Annika settled on #893/#956; the frontend-B spec's older "keep `RunCommand`
/// a separately-granted, higher-trust cap" line described a second gate that
/// was never built, and this comment used to claim #436 had made the
/// *documented* model true when it made the weaker one true.
pub(super) fn enforce_capabilities(
    granted: &[Capability],
    plugin_id: &str,
    effects: Vec<Effect>,
    warned: &mut EffectWarnLatch,
) -> Vec<Effect> {
    effects
        .into_iter()
        .filter(|effect| {
            let Some(cap) = effect.required_capability() else {
                return true;
            };
            if granted.contains(&cap) {
                true
            } else {
                // #1165 item 5: one line per effect kind per connection, naming
                // the kind rather than `Debug`-formatting the effect. A missing
                // capability is a *manifest* mistake, so it is wrong on every
                // frame for the life of the connection — the unlatched form
                // wrote the plugin's whole (arbitrary-length, plugin-supplied)
                // effect payload to the journal at the plugin's frame rate. The
                // audit log keeps the per-occurrence record.
                if warned.insert(std::mem::discriminant(effect)) {
                    tracing::warn!(
                        plugin = %plugin_id,
                        effect = super::effects::effect_kind(effect),
                        ?cap,
                        "plugin effect requires a capability it didn't declare; dropped. Add it \
                         to the manifest's capabilities (further drops of this effect kind on \
                         this connection are silenced — the audit log still records each one)",
                    );
                }
                super::effects::record_audit(
                    plugin_id,
                    effect,
                    super::effects::AuditDecision::DroppedUngranted,
                    // A dropped effect never reaches a launch, so there is no
                    // unit to name (#953 M1).
                    None,
                );
                false
            }
        })
        .collect()
}

/// [`serve_conn`] with no unregistered-connection permit — one connection driven
/// on its own, which is what the per-connection tests do: a socketpair, no
/// listener, so there is no gate to hold a permit from.
///
/// `#[cfg(test)]` because production reaches a connection only through
/// [`listener::accept_loop`](super::listener::accept_loop), which always has a
/// permit to hand over. Keeping it as a test-only shim is what let the gate be
/// added without rewriting twenty-odd call sites that are about something else
/// entirely.
#[cfg(test)]
pub(super) async fn handle_conn(stream: UnixStream, ctx: &ListenerCtx) {
    serve_conn(stream, ctx, None).await;
}

/// Drive one plugin connection: handshake, then read frames until the peer
/// disconnects, feeding renders into the mount mailbox and pushing state
/// snapshots + events back out.
///
/// `unregistered` is the listener's gate permit (#1165 item 6), released the
/// moment this connection is registered — see the `drop` after the [`IdGuard`]
/// claim. It is an `Option` because the gate belongs to the accept loop: a test
/// driving one socketpair has no listener and passes `None`.
// One cohesive per-connection lifecycle (handshake → the four opt-in push tasks
// → reader loop → teardown); splitting it would scatter the paired setup/abort
// of each task across helpers for no readability gain.
#[allow(clippy::too_many_lines)]
pub(super) async fn serve_conn(
    stream: UnixStream,
    ctx: &ListenerCtx,
    unregistered: Option<tokio::sync::OwnedSemaphorePermit>,
) {
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
    // #437: reject a plugin built against a NEWER wire vocabulary than this host —
    // one that can render a `Node`/`Effect` variant this host can't decode. The
    // `PROTO_VERSION` exact-match above can't catch that (both sides are the same
    // proto), so without this the plugin's first render frame would fail to decode,
    // the host would treat it as an ordinary disconnect, and the redialing SDK
    // would crash-loop it every 5 s with only an info-level trace. Rejecting here
    // turns that silent loop into one loud, self-explanatory handshake refusal. An
    // older plugin that predates the field decodes to `vocab = 0` and always passes.
    if let Err(e) = manifest.check_vocab() {
        tracing::warn!(
            plugin = %manifest.id,
            plugin_vocab = manifest.vocab,
            host_vocab = VOCAB,
            error = %e,
            "plugin built against a newer wire vocabulary than this host understands — update the shell; rejecting the connection",
        );
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
    // #1165 item 6: **registered**, so the listener's gate permit is released
    // here rather than at teardown. What the gate bounds is *unregistered*
    // connections — a peer that dials and says nothing — and a registered
    // plugin is a long-lived, identified connection that should not count
    // against the handshake budget. Held to exactly this point: the `IdGuard`
    // above is the last thing that can reject a registration, so releasing
    // before it would let a rejected duplicate free a permit it never earned.
    // A connection that never gets here drops its permit when `REGISTER_TIMEOUT`
    // (or a decode failure) returns from this function.
    drop(unregistered);
    // #1165 item 4: the one moment a new plugin id may be about to take a slot
    // in the host's effect-bucket table, and therefore the moment to retire the
    // entries that have refilled. This connection's own bucket is deliberately
    // NOT reset here — surviving the reconnect is the whole point.
    sweep_effect_buckets(&ctx.effect_buckets, Instant::now());
    let mount = manifest.mount;
    // Region sort key (advisory placement request); `None` sorts as `0` (#274).
    let order = manifest.order.unwrap_or(0);
    // The manifest's granted capability set (#436), consulted per render frame in
    // the reader to drop effects the plugin never declared a cap for.
    let capabilities = manifest.capabilities.clone();
    // The same manifest, read for the caps that gate a *node* rather than an
    // effect (#893's `Capability::Shader`). Resolved once here and stamped on
    // every `SlotRender` this connection parks, so the mapping pass on the GTK
    // thread applies exactly what `enforce_capabilities` applies on this one.
    let grants = super::shader_map::Grants::from_manifest(&manifest);
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
    // Live runtime mirror (#423): this connection is now connected (not yet
    // rendering). The matching `runtime_remove` runs in teardown while this
    // connection still owns the id (its `IdGuard` above hasn't released), so a
    // fast-reconnect successor never clobbers the wrong entry.
    super::runtime_register(&ctx.runtime, &plugin_id, mount);

    // Outbound writer: the single point that serializes host→plugin frames. The
    // queue is **bounded** (#435): a plugin that stops reading its socket can no
    // longer make the host buffer frames without limit — producers drop onto a
    // full queue (`push_state`) and the liveness ping reaps the stuck connection.
    let (out_tx, out_rx) = mpsc::channel::<HostMsg>(OUTBOUND_CAPACITY);
    let writer = tokio::spawn(writer_task(wr, out_rx));

    // Vocabulary advertisement (#882/#883): the first frame after an accepted
    // `Register`, and the thing that makes this shell able to *receive*
    // `Node::Preem` at all — a plugin emits the typed preem vocabulary only
    // above the generation the host advertised, and rasterises to `Node::Pixels`
    // otherwise.
    //
    // **Sent if and only if `manifest.negotiates_vocab()`.** This gate is the
    // whole reason `vocab_max` exists as a separate, `Option` field: a plugin
    // can only set it if it was built against the proto that also carries
    // `HostMsg::Hello`, so gating on its presence means a pre-#882 binary never
    // meets a frame its `rmp-serde` cannot decode. Sending `Hello`
    // unconditionally is not a cosmetic slip — it is the #437 crash-loop on
    // every deployed plugin at once: decode failure → session close →
    // `Restart=on-failure` redial → the host re-sends it.
    if manifest.negotiates_vocab() {
        let negotiated = manifest.negotiated_vocab(VOCAB);
        // `try_send` on a fresh, empty queue: this cannot be full, and a
        // connection that died between the handshake and here is about to be
        // reaped by the reader anyway.
        let _ = out_tx.try_send(HostMsg::Hello { vocab: VOCAB });
        tracing::debug!(
            plugin = %plugin_id,
            host_vocab = VOCAB,
            plugin_vocab_max = ?manifest.vocab_max,
            negotiated,
            "advertised the host wire vocabulary",
        );
    }

    // Datasource providers (#509): register each datasource this connection serves
    // so the effect broker can route a matching `Effect::DatasourceQuery` (from any
    // requester) to THIS connection's outbound. Gated on BOTH a non-empty `provides`
    // entry AND `Capability::DatasourceProvider` — the same declared-*and*-enforced
    // posture as the domain-state push gate (`push_gate`): a plugin that lists
    // datasources but omits the capability is refused registration and warned.
    // Registered under this connection's `generation`, so teardown removes only
    // entries a fast-reconnect successor hasn't already replaced.
    if !manifest.provides.is_empty() {
        if capabilities.contains(&Capability::DatasourceProvider) {
            for ds in &manifest.provides {
                ctx.datasource.register_provider(
                    &ds.id,
                    &plugin_id,
                    ds.scopes.clone(),
                    out_tx.clone(),
                    generation,
                );
            }
        } else {
            tracing::warn!(
                plugin = %plugin_id,
                "plugin lists `provides` datasources without declaring Capability::DatasourceProvider; not registered as a provider",
            );
        }
    }

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
    // Whether this connection is a gated now-playing subscriber (#528): reused
    // below for the now-playing push task AND for the #542 unpark re-seed, which
    // the sidebar visibility task carries so it lands *after* the
    // `SlotVisibility(true)` frame (see `visibility_task`).
    let now_playing_gated = push_gate(&manifest, StateKey::NowPlaying);
    let visibility = if manifest.subscribes.contains(&StateKey::SlotVisible) {
        if mount.is_bar() {
            let _ = out_tx.try_send(HostMsg::SlotVisibility { visible: true });
            None
        } else {
            // #542: hand the sidebar visibility task the now-playing receiver
            // too (only for a gated now-playing subscriber), so the unpark
            // rising edge can re-seed the current track right after
            // `SlotVisibility(true)` — a parked marquee drops now-playing pushes,
            // so without the re-seed it resumes stale on reopen.
            let np_rx = now_playing_gated.then(|| ctx.now_playing_rx.clone());
            Some(tokio::spawn(visibility_task(
                ctx.visibility_rx.clone(),
                np_rx,
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

    // Audio spectrum (#405/#559): forward the ~20 Hz `{peak, bins}` push — but
    // ONLY to a plugin that subscribes `StateKey::AudioSpectrum` (the #305 opt-in
    // gate, exactly like accent/visibility). The capture tap is reference-counted
    // across the subscribers that are currently ON-SCREEN: the `spectrum_task`'s
    // `SpectrumDemand` guard adds this connection's unit while it is visible
    // (sidebar open, or a bar mount — always on-screen) and removes it when it
    // goes off-screen or the connection tears down, driving the tap's 0↔1
    // `set_spectrum_active` edge. So the default sink's monitor is tapped only
    // while an audio-reactive card is actually being looked at — a closed sidebar
    // (the audio-widget's card's usual state) drops it to inactive within a tick.
    let spectrum = manifest
        .subscribes
        .contains(&StateKey::AudioSpectrum)
        .then(|| {
            tokio::spawn(spectrum_task(
                ctx.spectrum_rx.clone(),
                ctx.visibility_rx.clone(),
                mount.is_bar(),
                out_tx.clone(),
            ))
        });

    // The #484/#528 domain pushes: calendar / session-locked / now-playing. Each
    // is gated on the plugin BOTH subscribing the key AND declaring its gating
    // capability ([`push_gate`], per #495's exhaustive mapping) — a subscribe-only
    // plugin is refused (and warned) because these carry personal / privacy-
    // relevant data. Mirrors the seed-then-on-change accent/spectrum tasks.
    let calendar = push_gate(&manifest, StateKey::CalendarUpcoming)
        .then(|| tokio::spawn(calendar_task(ctx.calendar_rx.clone(), out_tx.clone())));
    let locked = push_gate(&manifest, StateKey::SessionLocked)
        .then(|| tokio::spawn(locked_task(ctx.locked_rx.clone(), out_tx.clone())));
    let now_playing = now_playing_gated
        .then(|| tokio::spawn(now_playing_task(ctx.now_playing_rx.clone(), out_tx.clone())));

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
    // #1058 review MEDIUM-2: per-connection latch for `capped_hidden_on`'s two
    // violation kinds, mirroring the SDK's own `capability_warned` — reset on
    // every reconnect, so a
    // long-lived misconfiguration is named once per connection, not once per
    // frame.
    let mut hidden_on_warned = HashSet::new();
    // #1165: the per-connection latch for `capped_effect_strings`, on exactly
    // the same terms as `hidden_on_warned` above — one line per effect kind per
    // connection, not one per frame.
    let mut effect_text_warned = EffectWarnLatch::new();
    // #1165 item 7: the datasource payload cap's own latch, separate from the
    // display-string one because an effect kind can trip both.
    let mut effect_payload_warned = EffectWarnLatch::new();
    // #1165 item 5: the two drop-warn latches. Kept apart rather than shared,
    // because an effect kind can be dropped for *both* reasons over one
    // connection's life and the two name different fixes — a manifest edit
    // versus a slower emitter.
    let mut ungranted_warned = EffectWarnLatch::new();
    let mut rate_cap_warned = EffectWarnLatch::new();
    // #1165: the `PluginMsg::Log` length + rate gate, per connection like the
    // effect limiter beside it.
    let mut log_gate = LogGate::new_at(Instant::now());
    let reader = async {
        loop {
            match read_frame::<PluginMsg, _>(&mut rd).await {
                Ok(PluginMsg::Render {
                    tree,
                    panel,
                    hidden_on,
                    effects,
                }) => {
                    // #436: drop any effect whose capability the plugin never
                    // declared, THEN rate-cap the survivors (#435) so an
                    // ungranted flood costs no tokens. Both are host policy — the
                    // plugin may request anything; the host decides what runs.
                    let requested = effects.len();
                    let kept = throttle_effects(
                        &ctx.effect_buckets,
                        &plugin_id,
                        enforce_capabilities(
                            &capabilities,
                            &plugin_id,
                            effects,
                            &mut ungranted_warned,
                        ),
                        &mut rate_cap_warned,
                    );
                    // #1165: the payload caps run LAST — after the two host
                    // policies have decided which effects run at all, so a
                    // dropped effect costs no capping work, and before the
                    // broker, which is the GTK main thread.
                    let kept = kept
                        .into_iter()
                        .filter_map(|effect| {
                            let (effect, message) =
                                capped_effect_strings(effect, &mut effect_text_warned);
                            if let Some(message) = message {
                                tracing::warn!(plugin = %plugin_id, "{message}");
                            }
                            let (effect, message) =
                                capped_effect_payload(effect, &mut effect_payload_warned);
                            if let Some(message) = message {
                                tracing::warn!(plugin = %plugin_id, "{message}");
                            }
                            effect
                        })
                        .collect::<Vec<_>>();
                    // Runtime mirror (#423): this frame proves the plugin is
                    // rendering; the guards' drops feed its violation count.
                    let dropped =
                        u32::try_from(requested.saturating_sub(kept.len())).unwrap_or(u32::MAX);
                    super::runtime_render(&ctx.runtime, &plugin_id, dropped);
                    // #1050: connector *names* are not validated here (nor
                    // could they usefully be: this task has no monitor list,
                    // and an output that is currently off is a legitimate
                    // thing to name — each monitor's reconciler decides
                    // whether a name is *its* name). The *shape* — how many
                    // entries, how long each one is — is capped by
                    // `capped_hidden_on` (#1058), since that bound has
                    // nothing to do with which monitors exist. The cap is
                    // pure (review MEDIUM-2); this is where its one warning
                    // per connection per violation kind actually fires.
                    let (hidden_on, violation) = capped_hidden_on(hidden_on, &mut hidden_on_warned);
                    if let Some((_, msg)) = violation {
                        tracing::warn!(plugin = %plugin_id, "{msg}");
                    }
                    route_render(
                        ctx,
                        mount,
                        SlotRender {
                            plugin_id: plugin_id.clone(),
                            order,
                            generation,
                            tree,
                            // #1073: the wire frame boxes `panel` (a per-frame
                            // envelope size fix); `SlotRender` — parked in a
                            // coalescing mailbox, not a per-frame value — keeps
                            // its own unboxed `Option<Node>`, so unbox here at
                            // the one place the wire is decoded.
                            panel: panel.map(|p| *p),
                            hidden_on,
                            grants,
                            outbound: out_tx.clone(),
                        },
                        kept,
                    );
                }
                Ok(PluginMsg::Register { .. }) => {
                    tracing::warn!(plugin = %plugin_id, "duplicate Register ignored");
                }
                // #1165: a `Log` frame was neither length- nor rate-capped —
                // the one inbound message kind that reaches the journal
                // directly, bounded only by the 16 MiB frame limit and by how
                // fast the plugin can write.
                Ok(PluginMsg::Log { level, msg }) => match log_gate.admit(&msg, Instant::now()) {
                    LogAdmission::Emit { msg, over_cap } => {
                        if let Some(bytes) = over_cap {
                            tracing::warn!(
                                plugin = %plugin_id,
                                bytes,
                                cap = MAX_LOG_MSG_BYTES,
                                "plugin Log message is over the host's length cap; the \
                                 prefix is logged (further occurrences on this connection \
                                 are silenced)",
                            );
                        }
                        log_plugin(&plugin_id, level, &msg);
                    }
                    LogAdmission::Drop { warn } => {
                        if warn {
                            tracing::warn!(
                                plugin = %plugin_id,
                                burst = LOG_BURST,
                                per_sec = LOG_REFILL_PER_SEC,
                                "plugin exceeded the host's log rate cap; lines are being \
                                 dropped (further occurrences on this connection are \
                                 silenced)",
                            );
                        }
                    }
                },
                Ok(PluginMsg::Pong { seq }) => {
                    pong_seen.store(true, Ordering::Relaxed);
                    tracing::trace!(plugin = %plugin_id, seq, "plugin pong");
                }
                // A clean EOF (the plugin closed its socket) is an ordinary
                // disconnect — info. A *decode* failure is not: a well-behaved
                // peer never sends a frame this host can't parse, so it points at
                // a schema skew — most likely a plugin built against a newer wire
                // vocabulary than this host (#437; check_vocab catches the common
                // case at the handshake, but a mid-session decode failure gets the
                // same hint). Surface it at warn so the crash-loop leaves a trace.
                Err(ProtoError::Decode(e)) => {
                    tracing::warn!(
                        plugin = %plugin_id,
                        error = %e,
                        "plugin frame failed to decode (schema skew? a plugin built against a newer wire vocabulary than this host); closing the connection",
                    );
                    break;
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
    // Datasource providers (#509): drop each datasource this connection served,
    // generation-guarded so a fast-reconnect successor's registration is never
    // evicted (the same #278 guard as the region clears above). A no-op for a
    // non-provider, or for an entry a newer connection already replaced.
    for ds in &manifest.provides {
        ctx.datasource.unregister_provider(&ds.id, generation);
    }
    // Drop this connection from the runtime mirror (#423) — done here, still
    // inside the id's exclusive-ownership window (the `IdGuard` releases only
    // when `serve_conn` returns), so it cannot evict a fast-reconnect successor.
    super::runtime_remove(&ctx.runtime, &plugin_id);
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
        // The task's `SpectrumDemand` guard releases this connection's refcount
        // unit on drop (aborting the task drops its future) — firing the 1→0
        // tap-deactivation edge if it was the last on-screen subscriber — so no
        // inline decrement here (which would double-count against the guard).
        spectrum.abort();
    }
    if let Some(calendar) = calendar {
        calendar.abort();
    }
    if let Some(locked) = locked {
        locked.abort();
    }
    if let Some(now_playing) = now_playing {
        now_playing.abort();
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
/// chip is always on-screen; see `serve_conn`, #438), never this change loop.
///
/// #542: when `now_playing_rx` is `Some` (a gated now-playing subscriber), the
/// unpark rising edge (`false`→`true`) additionally re-seeds the current
/// now-playing **after** the `SlotVisibility(true)` frame. A parked card drops
/// now-playing pushes while hidden (the audio-widget's marquee does), so without
/// this re-seed it would resume showing a stale/stopped track until the next
/// change. Carrying it here — rather than in a separate task — is what
/// guarantees the ordering: the plugin flips `visible` before it adopts the
/// re-seeded track (a separate task would race the visibility frame).
async fn visibility_task(
    mut visibility_rx: watch::Receiver<bool>,
    now_playing_rx: Option<watch::Receiver<NowPlaying>>,
    out: mpsc::Sender<HostMsg>,
) {
    // Seed at register (the watch replays its current value).
    let mut visible = *visibility_rx.borrow_and_update();
    if let Push::Stop = push_state(&out, HostMsg::SlotVisibility { visible }) {
        return;
    }
    while visibility_rx.changed().await.is_ok() {
        let now_visible = *visibility_rx.borrow_and_update();
        let rising = now_visible && !visible;
        visible = now_visible;
        if let Push::Stop = push_state(&out, HostMsg::SlotVisibility { visible }) {
            break;
        }
        // #542: re-seed the current now-playing on the unpark edge, ordered
        // after the `SlotVisibility(true)` frame above.
        if rising && let Some(np_rx) = now_playing_rx.as_ref() {
            let now_playing = np_rx.borrow().clone();
            if let Push::Stop = push_state(&out, HostMsg::NowPlaying { now_playing }) {
                break;
            }
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
///
/// #559: this task also owns the connection's demand on the capture tap via a
/// [`SpectrumDemand`] guard. The tap is only worth running while the card is
/// on-screen, so demand tracks visibility — a sidebar card demands while its
/// sidebar is open (the `visibility_rx` edges), and a bar chip is always
/// on-screen (`always_visible`, no edges to track). The guard drives the
/// `SPECTRUM_SUBSCRIBERS` 0↔1 `set_spectrum_active` edge and, on `Drop`
/// (task abort at teardown), releases this connection's unit exactly once.
async fn spectrum_task(
    mut spectrum_rx: watch::Receiver<Option<AudioSpectrum>>,
    mut visibility_rx: watch::Receiver<bool>,
    always_visible: bool,
    out: mpsc::Sender<HostMsg>,
) {
    // Seed the tap demand from the initial visibility: a bar chip demands
    // unconditionally, a sidebar card only while its sidebar is currently open.
    let mut demand = SpectrumDemand::new();
    demand.set(always_visible || *visibility_rx.borrow_and_update());

    // Seed the current spectrum (the watch replays its value; often `None` until
    // audio flows through the freshly-activated tap).
    let seed = *spectrum_rx.borrow_and_update();
    if let Some(spectrum) = seed
        && let Push::Stop = push_state(&out, HostMsg::AudioSpectrum { spectrum })
    {
        return;
    }
    loop {
        tokio::select! {
            changed = spectrum_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let current = *spectrum_rx.borrow_and_update();
                if let Some(spectrum) = current
                    && let Push::Stop = push_state(&out, HostMsg::AudioSpectrum { spectrum })
                {
                    break;
                }
            }
            // Re-compute demand as the sidebar opens/closes. A bar chip is always
            // on-screen, so it never tracks these edges (the arm is disabled) and
            // its demand stays the constant `true` seeded above.
            changed = visibility_rx.changed(), if !always_visible => {
                if changed.is_err() {
                    break;
                }
                demand.set(*visibility_rx.borrow_and_update());
            }
        }
    }
    // `demand` drops here (natural exit) or when this task's future is dropped
    // (teardown abort), releasing the tap contribution exactly once.
}

/// Push the upcoming-calendar digest on subscribe (the register seed) and on every
/// change, coalescing bursts latest-wins via `borrow_and_update` (#484). Mirrors
/// [`accent_task`]; an empty list is meaningful ("no upcoming events"), so — unlike
/// [`spectrum_task`] — it is always sent. Spawned only for a connection that passes
/// [`push_gate`] for [`StateKey::CalendarUpcoming`] (subscribed + `Capability::Calendar`).
async fn calendar_task(
    mut calendar_rx: watch::Receiver<Vec<UpcomingEvent>>,
    out: mpsc::Sender<HostMsg>,
) {
    let initial = calendar_rx.borrow_and_update().clone();
    if let Push::Stop = push_state(&out, HostMsg::CalendarUpcoming { events: initial }) {
        return;
    }
    while calendar_rx.changed().await.is_ok() {
        let events = calendar_rx.borrow_and_update().clone();
        if let Push::Stop = push_state(&out, HostMsg::CalendarUpcoming { events }) {
            break;
        }
    }
}

/// Push the session-locked hint on subscribe (the register seed, so a plugin
/// starts in the right state) and on every change, latest-wins (#484). Mirrors
/// [`visibility_task`]; spawned only for a connection that passes [`push_gate`]
/// for [`StateKey::SessionLocked`] (subscribed + `Capability::SessionState`).
async fn locked_task(mut locked_rx: watch::Receiver<bool>, out: mpsc::Sender<HostMsg>) {
    let initial = *locked_rx.borrow_and_update();
    if let Push::Stop = push_state(&out, HostMsg::SessionLocked { locked: initial }) {
        return;
    }
    while locked_rx.changed().await.is_ok() {
        let locked = *locked_rx.borrow_and_update();
        if let Push::Stop = push_state(&out, HostMsg::SessionLocked { locked }) {
            break;
        }
    }
}

/// Push the now-playing digest on subscribe and on every change, coalescing bursts
/// latest-wins via `borrow_and_update` (#528). Mirrors [`accent_task`]; the empty,
/// not-playing default is meaningful, so it is always sent. Spawned only for a
/// connection that passes [`push_gate`] for [`StateKey::NowPlaying`]
/// (subscribed + `Capability::NowPlaying`).
async fn now_playing_task(
    mut now_playing_rx: watch::Receiver<NowPlaying>,
    out: mpsc::Sender<HostMsg>,
) {
    let initial = now_playing_rx.borrow_and_update().clone();
    if let Push::Stop = push_state(
        &out,
        HostMsg::NowPlaying {
            now_playing: initial,
        },
    ) {
        return;
    }
    while now_playing_rx.changed().await.is_ok() {
        let now_playing = now_playing_rx.borrow_and_update().clone();
        if let Push::Stop = push_state(&out, HostMsg::NowPlaying { now_playing }) {
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
