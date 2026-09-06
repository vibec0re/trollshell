//! Shell-side preem renderers (#883): the typed [`Node::Preem`](hytte_plugin_proto::wire::Node::Preem)
//! widgets #882 put on the wire, rasterised **in this process** with
//! `hytte-preem` — the same kit the Stats drawer's per-core LED panel already
//! draws with (#857), into the same [`UiNode::Pixels`]/`PixelSurface` machinery
//! the legacy `Node::Pixels` arm feeds.
//!
//! # What this module owns
//!
//! Everything the vocabulary deliberately left off the wire. A
//! [`vocab::PreemWidget`] carries a **config** (rebuild on change) and a
//! **state** (animate toward the target); it carries no phosphor buffer, no
//! needle position, no flip clock, no scroll offset and no held peak. Those
//! live here, in a per-node renderer instance, advanced from the **GTK frame
//! clock** of whichever mount is showing them (#897 — `pump::install_animation`)
//! rather than by every plugin ticking its own rasterisation over the socket.
//! That is what makes the traffic go quiet: a marquee scrolling a track title
//! sends one frame when the title changes, not twenty a second.
//!
//! # Instance lifecycle
//!
//! Instances live in a GTK-thread-local table keyed by **[`Scope`] + node key**,
//! and follow the rule the reconciler already uses for widgets:
//!
//! - same key, same widget **kind**, same **config** → *update* the existing
//!   instance's state (the animation continues from where it is);
//! - a **config** change or a **kind** change → *rebuild* the instance (a new
//!   scope clears its phosphor, a new gauge rests its needle, a new board
//!   blanks its cells — exactly what the vocabulary's per-config docs promise);
//! - a node that stops appearing in the tree → dropped at the end of the
//!   mapping pass ([`end_pass`]); a whole plugin card leaving → [`forget_scope`].
//!
//! ## Why a scope, and what the node key is
//!
//! Node ids are namespaced *per plugin*, and a plugin renders two independent
//! trees (its chip/card and its drawer panel). A bare node id is therefore not
//! a unique key: two plugins both calling a gauge `"cpu"` would otherwise share
//! one needle. [`Scope`] is that namespace — the plugin id plus the tree role —
//! and it is also the unit [`forget_scope`] drops on teardown.
//!
//! Within a scope the key is the node's **`id`, and the id is the contract**
//! (#900): a preem node is required to carry one, because the animation state
//! this module owns is exactly the state a frame cannot re-derive, so the id is
//! the only thing that can tie an instance to the node it belongs to. The SDK's
//! `display` wrappers stamp it from the widget key they already take
//! (`display::gauge::node("cpu")` → `id: Some("cpu")`), so every bundled plugin
//! and every `display`-based plugin satisfies it for free.
//!
//! An anonymous node is a **fallback, not a supported shape**. It is keyed by
//! its **ordinal among the un-id'd preem nodes** of that tree, in traversal
//! order (the tradeoff React's index keys make), and [`map_widget`] logs one
//! `warn` the first time it sees one — **at most once per plugin tree for the
//! shell's run**, latched in [`WARNED`] rather than on the [`ScopeState`], which
//! a single preem-less frame destroys. It falls back rather than refusing to
//! draw so a hand-rolled (non-Rust-SDK) plugin degrades to the pre-#900
//! behaviour instead of losing the widget.
//!
//! What that fallback costs, and why the warning exists: the ordinal is stable
//! only while those nodes keep their order and their count. **Insert or remove
//! an anonymous sibling and the ones after it shift down a slot**, inheriting
//! the animation state of the node that used to hold it — two interchangeable
//! gauges have identical configs by construction, so [`same_config`] agrees, the
//! survivor is *updated in place*, and it renders the removed node's needle
//! before springing to its own target. For a `Scope` a whole phosphor history
//! moves onto another signal. A variable-length list of anonymous widgets —
//! per-core gauges, per-sink strips — glitches on every insert and remove.
//!
//! A structural key (the parent chain's child indices) would narrow that, but it
//! would still be positional and it would still transplant across a *reorder*;
//! #900 settled the policy at "require the id" instead, which makes the failure
//! diagnosable (one journal line naming the plugin) rather than silent.
//!
//! ## Two nodes, one id (#918)
//!
//! The other end of the same keying contract: two preem nodes in one tree
//! claiming the same explicit `id` **collapse onto one renderer instance**, and
//! every mapping pass applies both widgets to it in turn. One needle is dragged
//! between two targets, one phosphor accumulates two signals, one flip board is
//! rewritten twice a frame — permanently, not for one frame the way the ordinal
//! transplant is. The last node rendered wins, which is what [`map_widget`]
//! deliberately keeps doing: refusing the second node would make a widget
//! *disappear* to fix a widget that *jitters*.
//!
//! It is diagnosed instead. `state.touched` already knows — it is a per-pass
//! set, so the second `insert` of a key returns `false` — and that is the whole
//! detection. One `warn` per tree names the id and both widget kinds.
//!
//! ## Bounds (#901)
//!
//! An instance is expensive in a way a wire node is not: a ~40-byte config
//! becomes a phosphor buffer, a needle, flip clocks and a cached RGBA frame. The
//! node count is the multiplier, so both are capped, and the two caps live on
//! the wire (`hytte_plugin_proto::wire`) where a plugin author reads them:
//!
//! - [`MAX_PREEM_NODES_PER_TREE`] renderer instances per [`Scope`] — a scope is
//!   exactly one plugin tree, so the wire's per-tree cap *is* this module's
//!   per-scope instance cap. Past it a node renders as the unknown-widget
//!   placeholder (the same empty surface a kind this build can't render
//!   degrades to), with one warning per tree.
//! - `wire::MAX_NODES_PER_TREE` nodes of every kind together, and
//!   `wire::MAX_TREE_DEPTH` levels of nesting, both enforced one layer up in
//!   [`wire_map`](super::wire_map) where the tree is actually walked. The count
//!   is the bound; this one is the multiplier; the depth is what keeps the walk
//!   itself off the main thread's stack limit.
//!
//! The instance cap is charged against the nodes **this pass admitted**, not
//! against the live instance count, precisely so it does not violate the
//! idempotence rule immediately below: `end_pass` sweeps per pass and every
//! monitor is a pass, so a cap that consulted the surviving instances would
//! answer differently for the first monitor than for the second. A node that
//! already holds an instance is admitted either way — otherwise the frame a tree
//! grew past the cap on would also be the frame its existing widgets lost their
//! animation state. See [`map_widget`] for what that bounds at rest and in
//! flight.
//!
//! ## Idempotence — the multi-monitor requirement
//!
//! `to_ui_node` runs **once per monitor** on every render frame (each monitor
//! has its own region container and reconciler), and again for the drawer
//! panel. So applying a widget must be idempotent: state is applied only when
//! the incoming widget differs from the one last applied to that instance.
//! Without that, a two-monitor session would stamp every scope sample batch
//! twice and decay its phosphor twice per frame. The rasterised frame is cached
//! for the same reason — the second monitor's mapping pass re-uses the bytes the
//! first one produced, and since #911 it re-uses the *allocation*: the cache
//! holds an `Arc<[u8]>` and every monitor's [`UiNode::Pixels`] carries a handle
//! on it, so one frame is rasterised once, copied never, and settled on each
//! surface by an `Arc::ptr_eq` when nothing moved.
//!
//! The equality that gates this is [`same_widget`], **not** `PreemWidget`'s
//! derived `PartialEq`. The derived one is not reflexive over a `NaN`, and a
//! short-circuit that never fires is not a missed optimisation here: for a
//! `Scope` it re-arms `pending` and zeroes `idle` every pass, which pins the
//! animation clock at 20 Hz forever. See [`canonicalize_non_finite`].
//!
//! # Clamping
//!
//! Every widget reaching this module has already been through
//! [`PreemWidget::clamp_in_place`](vocab::PreemWidget::clamp_in_place) in
//! `wire_map`'s Preem arm. That is the wire contract (#895): a ~40-byte config
//! must never become a multi-GB allocation or a multi-million-segment render.
//! **Nothing here may re-derive geometry from an unclamped value.**
//!
//! # Styles, roles and the live re-tint (#885)
//!
//! A [`vocab::StyleRef`] is three things, and this module resolves all three.
//!
//! The **skin** goes to the kit's `DisplayStyle` by name ([`display_style`]),
//! the same linear scan over `DisplayStyle::ALL` the Stats panel's config parser
//! uses. That is #397's payoff: one skin implementation, shell-side, serving
//! every plugin's widgets.
//!
//! The **[`vocab::AccentRole`]** and the optional pinned
//! [`ink`](vocab::StyleRef::ink) become one `kit::Ink` ([`ink_for`]), which
//! [`pins_for`] carries — beside the optional pinned
//! [`field`](vocab::StyleRef::field) — into one `kit::Pins`, scoped around the
//! rasterisation with `hytte_preem::with_pins`. That is the piece the kit did
//! not have before #885: `hytte_preem::set_accent` is a *process* global, which
//! is exactly right in a plugin (one process, one plugin, one session tint) and
//! not enough in a shell, which rasterises many plugins' widgets — each with its
//! own role — in this one process.
//!
//! A `TextBox`'s third color, [`notdef`](vocab::TextBoxConfig::notdef), is not a
//! palette slot (no other widget has one), so it rides the kit builder in
//! [`text_box`] instead of the scope.
//!
//! `Success`/`Warning`/`Error` resolve against the live theme ([`role_inks`],
//! `@success_color` and friends, memoized until the theme moves); `Accent` and a
//! role-less `StyleRef` take the session accent the shell already installs
//! (#862, `pump::tint_in_process_surfaces`), so a frame that says nothing looks
//! exactly as it did before this existed; `Neutral` refuses even that and takes
//! the skin's own ink.
//!
//! A theme color resolved for a role is then **offered to the skin** before it
//! is pinned (`DisplayStyle::admit_ink`, #935): it is the shell's answer rather
//! than the author's, so the skin still owns what an ink it did not choose looks
//! like on its field — which is what keeps the status trio readable on the
//! reflective `Lcd`. An author's explicit `ink` pin is *not* offered; see
//! [`ink_for`] for the whole of that distinction.
//!
//! # …and what makes it *live* (#396)
//!
//! Nothing here is baked into an instance. Every rasterisation resolves the
//! palette afresh, so a desktop accent or color-scheme change only has to drop
//! the cached frames — which is what [`invalidate_cached_frames`] already does
//! on the accent path, and where the memoized role colors are dropped too. The
//! next mapping pass re-renders every widget in the new theme with no plugin
//! involvement and no wire traffic.
//!
//! The one deliberate exception is a **pin**: a `StyleRef` carrying an explicit
//! ink or field (or a `TextBox` carrying a notdef) re-rasterises like everything
//! else and produces byte-identical pixels, because the colors it was pinned to
//! did not change. That is what pinning means, and the wire docs say so.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use hytte::gtk::{self, prelude::*};
use hytte::ui::Node as UiNode;
use hytte_plugin_proto::preem as vocab;
use hytte_plugin_proto::wire::MAX_PREEM_NODES_PER_TREE;
use hytte_preem as kit;

/// The wall-clock length of one **step**, for the two kit primitives that
/// advance per call rather than per elapsed second (`Scope::advance` and
/// `PeakHold::decay`).
///
/// Since #897 this is **not** a tick cadence: preem animation rides each mount's
/// GTK frame clock, so ticks arrive at the display's refresh (60 Hz here, 144 on
/// a fast panel) and [`Steps`] converts the real frame `dt` into whole steps,
/// carrying the remainder. What the constant still pins is how long one of those
/// steps *lasts*, and 20 Hz is not arbitrary for that: it is the rate the kit's
/// own consumers animate at (`hytte-plugin-audio-widget`'s `TICK`), and the rate
/// [`vocab::MarqueeConfig::speed_dots_per_sec`]'s default was derived from —
/// `12.0` dots/s — #929, PR #932. Anchoring the
/// step-based widgets to elapsed time rather than to
/// callbacks keeps a phosphor trail's fade the same length whether it is being
/// driven at 60 Hz, at 144 Hz, or by a stalling clock.
pub(super) const ANIM_STEP_SECS: f32 = 1.0 / 20.0;

/// Most steps one advance call issues to a step-based widget.
///
/// A tick arriving after a long stall (a resume from suspend, or a blocked GTK
/// thread) carries a `dt` worth hundreds of steps; replaying them all would burn
/// the stall's length in phosphor decays for no visible benefit. Clamping
/// catches the trail up to "fully faded" and moves on.
///
/// [`advance_scopes`] clamps its own `dt` to exactly this many steps' worth
/// ([`MAX_TICK_DT_US`]), so on the production path this is the live guard for a
/// resume from suspend — not, as the first cut of #897 had it, a constant made
/// unreachable by a tighter clamp upstream.
pub(super) const MAX_CATCHUP_STEPS: u32 = 8;

/// The most animation time one frame-clock tick may advance a scope by, in
/// microseconds — the stall clamp, and the thing that makes a *park* free of
/// consequence (#897).
///
/// Two events hand a tick an unbounded elapsed time, and both land on the same
/// answer:
///
/// - a **stall** — a resume from suspend, or a blocked GTK thread — where the
///   frame clock's `frame_time` simply jumps;
/// - a **park**, where the mount's tick callback broke (settled, or unmapped)
///   and something re-armed it minutes later. The per-scope baseline
///   ([`ScopeState::last_advance_us`]) is deliberately *not* reset on re-arm, so
///   this clamp is what stops the parked interval from being replayed as
///   elapsed animation. Not resetting is the point: a scope shown by two mounts
///   must not have its baseline stomped by whichever one happens to arm second,
///   and a scope another mount kept advancing while this one was parked has a
///   fresh baseline already.
///
/// **[`ANIM_STEP_SECS`] × [`MAX_CATCHUP_STEPS`], not one step** (both agreements
/// asserted in `plugins::tests`). The first cut of #897 clamped at one step, and
/// the #926 review measured what that costs: the clamp does not only bound
/// *catch-up*, it truncates **every** frame interval longer than itself, so any
/// sustained clock below 20 Hz runs animation slow with no diagnostic — probe
/// P5 measured a 15 Hz clock advancing a 20 dots/s marquee 15 dots in a second
/// rather than 20, a silent 25 % rate error. That is a live hazard rather than a
/// corner, because #897's own cost note says the shell rasterises on the CPU per
/// tick per animating widget: enough widgets push the frame clock below 20 Hz,
/// which then makes phosphor fade length, needle settle time and marquee speed
/// all drift together.
///
/// At 400 ms a 15 Hz clock (66.7 ms), and even a compositor throttling to 10 Hz,
/// passes through **unclamped and rate-exact**, while a park or a suspend still
/// resumes with at most the 8 steps #897's body asked to keep. The cost is that
/// a resume is a hop of up to 8 steps rather than 1 — the issue explicitly
/// allows either ("clamp **or** reset `last_tick` on re-arm"), and #883 behaved
/// the same way.
///
/// An `i64` of microseconds, converted through [`micros_to_secs`] rather than a
/// cast: 400 000 does not fit the `u16` the one-step version converted through.
/// Spelled as a literal, with `the_tick_dt_clamp_is_the_resume_cap` pinning it
/// against the two constants it is derived from.
pub(super) const MAX_TICK_DT_US: i64 = 400_000;

/// `micros` as seconds, for a value already known to be within
/// `0..=MAX_TICK_DT_US`.
///
/// Split at the millisecond so both halves fit a `u16` and convert through
/// `f32::from` — **exact**, and clean under `clippy::cast_precision_loss`, which
/// a direct `i64 as f32` (or `u32 as f32`) trips regardless of how small the
/// range actually is. Values outside the range are clamped rather than
/// truncated, so the conversion is total: this runs inside a tick callback,
/// where a `panic!` would take the shell's main loop with it.
fn micros_to_secs(micros: i64) -> f32 {
    let clamped = micros.clamp(0, MAX_TICK_DT_US);
    let millis = u16::try_from(clamped / 1_000).unwrap_or(u16::MAX);
    let rest = u16::try_from(clamped % 1_000).unwrap_or(0);
    f32::from(millis) / 1_000.0 + f32::from(rest) / 1_000_000.0
}

/// A phosphor cell's maximum intensity in the kit's persistence buffer
/// (`hytte-preem/src/scope.rs`: "one intensity (`0..=255`) per logical pixel").
/// The brightest a freshly-stamped beam can be, and so the value whose fade
/// takes longest — which is what [`scope_settle_steps`] measures.
const MAX_PHOSPHOR_INTENSITY: u32 = 255;

/// The kit's persistence ceiling: `256/256 = 1.0`, an infinite-persistence
/// phosphor. `kit::Scope::persistence` clamps to this, so the wire's `u16` must
/// be clamped the same way before any reasoning about decay.
const KIT_MAX_PERSISTENCE: u16 = 256;

/// Hard ceiling on [`scope_settle_steps`], so a pathological persistence cannot
/// make the settle loop long. At `255` (the slowest fade that still fades) the
/// true answer is 255 steps, so this is a safety rail rather than a cap the
/// arithmetic ever reaches.
const MAX_SCOPE_SETTLE_STEPS: u32 = 512;

/// Idle animation steps after which a [`Scope`](vocab::PreemWidget::Scope) with
/// no new samples has nothing left to fade, so it stops asking for repaints.
///
/// **Derived from the configured persistence, not a constant.** The kit's decay
/// is `(v * retained) >> 8` per step (`hytte-preem/src/scope.rs`'s `decayed`),
/// so the number of steps a full-intensity trail needs to reach zero is a
/// function of `retained` — and a constant is wrong in *both* directions:
///
/// - Too low for a long phosphor. At `persistence = 255` the decay is exactly
///   `v - 1` per step, so a full trail needs 255 steps. The old constant of 64
///   stopped advancing at ~191/255 and `animates()` then went false, freezing
///   the ghost on screen **permanently** — the opposite of a fade. Anything
///   from roughly `persistence >= 240` had this.
/// - Too high for the default. At `184` the trail is gone in 17 steps, but the
///   old bound kept setting `moved = true` for 64, spending ~47 pixel-identical
///   global repaints (~2.3 s) after every scope went quiet.
///
/// Computed once per renderer build by replaying the kit's own integer decay
/// rather than by a floating-point log, so it agrees with the kit exactly
/// instead of approximately.
fn scope_settle_steps(persistence: u16) -> u32 {
    let retained = u32::from(persistence.min(KIT_MAX_PERSISTENCE));
    // `256` retains everything: the trail never fades, and the caller's `fades`
    // flag short-circuits before this bound is ever consulted.
    if retained >= u32::from(KIT_MAX_PERSISTENCE) {
        return 0;
    }
    let mut intensity = MAX_PHOSPHOR_INTENSITY;
    let mut steps = 0;
    while intensity > 0 && steps < MAX_SCOPE_SETTLE_STEPS {
        intensity = (intensity * retained) >> 8;
        steps += 1;
    }
    steps
}

/// Latch for the "this shell can't render that preem widget" warning, so a
/// plugin that keeps re-rendering an unsupported widget logs once per session
/// instead of once per node per frame (the #895 pattern — at 20 Hz with eight
/// nodes that would be 160 identical journal lines a second).
static UNSUPPORTED_WARNED: AtomicBool = AtomicBool::new(false);

/// How many journal lines each [`Warned`] diagnostic has produced.
///
/// The assertion seam for "warns **at most once per plugin tree**, not once per
/// frame and not once per appearance". This crate has no `tracing` subscriber
/// harness — nothing in `plugins/tests.rs` installs a collector — so the tests
/// count emissions at the one place that decides whether to emit
/// ([`warn_once`]) rather than capturing the event. The counter is bumped by
/// `warn_once` itself, on exactly the `true` its every call site turns into one
/// `warn!`, so a count and a journal line cannot drift.
///
/// One counter per variant rather than one per diagnostic site: the latch is
/// keyed by `(Scope, Warned)`, and a counter keyed by `Warned` is its twin — a
/// test asserting that two different diagnostics fire *independently* needs to
/// read them apart.
///
/// Process-global, so a test reads a **delta** around the operation under test
/// (every preem test already serialises on the ink lock).
#[cfg(test)]
static WARN_COUNTS: [std::sync::atomic::AtomicU32; 5] = [
    std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0),
    std::sync::atomic::AtomicU32::new(0),
];

/// `what`'s counter, indexed by the same [`Warned::slot`] that gives it its bit
/// in the latch — so the count and the latch cannot disagree about which
/// diagnostic they mean.
#[cfg(test)]
fn warn_counter(what: Warned) -> &'static std::sync::atomic::AtomicU32 {
    &WARN_COUNTS[what.slot() as usize]
}

/// Which of a plugin's two independent node trees a preem node came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Role {
    /// The chip/card tree a mount region reconciles.
    Card,
    /// The drawer panel tree (#349 PR2).
    Panel,
    /// A tree with no plugin behind it — see [`Scope::detached`].
    #[cfg(test)]
    Detached,
}

/// The namespace a plugin's preem nodes live in: its id plus which of its two
/// trees the node came from. See the module docs on why a bare node id is not a
/// key.
///
/// The two halves are kept as **fields** rather than concatenated into one
/// string so the repaint fan-out can ask a moved scope which plugin and which
/// tree it belongs to, and nudge only the mailboxes that actually hold it — see
/// [`advance_all`] and `pump::request_preem_repaint`. (The previous spelling
/// joined them with `\u{1}` and justified it by claiming a plugin id cannot
/// contain that byte; `session.rs` validates only that the id is non-empty, so
/// the stated reason was wrong even though the scheme was injective. Fields
/// moot the question.)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Scope {
    plugin: String,
    role: Role,
}

impl Scope {
    /// The scope of `plugin_id`'s chip/card tree (the one a mount region
    /// reconciles).
    pub(super) fn card(plugin_id: &str) -> Self {
        Self {
            plugin: plugin_id.to_owned(),
            role: Role::Card,
        }
    }

    /// The scope of `plugin_id`'s drawer panel tree (#349 PR2).
    pub(super) fn panel(plugin_id: &str) -> Self {
        Self {
            plugin: plugin_id.to_owned(),
            role: Role::Panel,
        }
    }

    /// A scope for a tree with no plugin behind it — what the unit tests hand
    /// the mapping when the preem instances are beside the point. Test-only
    /// because every production tree belongs to a plugin: the drawer's blank
    /// page is the one plugin-less tree, and it is a constant the panel child
    /// renders straight into the reconciler without mapping at all.
    #[cfg(test)]
    pub(super) fn detached(label: &str) -> Self {
        Self {
            plugin: label.to_owned(),
            role: Role::Detached,
        }
    }

    /// The plugin whose tree this scope namespaces — what the repaint fan-out
    /// matches against a mailbox's `SlotRender::plugin_id`.
    pub(super) fn plugin_id(&self) -> &str {
        &self.plugin
    }

    /// Which of the plugin's two trees this scope covers.
    pub(super) fn role(&self) -> Role {
        self.role
    }
}

// ── the per-node renderer instances ──────────────────────────────────────────

/// A stepping accumulator for the two kit primitives that advance **per call**
/// with no `dt` of their own. Converts elapsed seconds into whole
/// [`ANIM_STEP_SECS`] steps, carrying the remainder so a jittery timer doesn't
/// drift.
#[derive(Debug, Default)]
struct Steps {
    carry: f32,
}

impl Steps {
    /// Whole steps owed for `dt` seconds, capped at [`MAX_CATCHUP_STEPS`]; the
    /// sub-step remainder is carried to the next call. A non-finite or
    /// non-positive `dt` owes nothing and leaves the carry alone. A `dt` past
    /// the cap drops its surplus rather than banking it — otherwise one stall
    /// would keep the widget catching up for as long as the stall lasted.
    fn owed(&mut self, dt: f32) -> u32 {
        if !dt.is_finite() || dt <= 0.0 {
            return 0;
        }
        self.carry += dt;
        let mut steps = 0;
        while self.carry >= ANIM_STEP_SECS && steps < MAX_CATCHUP_STEPS {
            self.carry -= ANIM_STEP_SECS;
            steps += 1;
        }
        if self.carry >= ANIM_STEP_SECS {
            self.carry = 0.0;
        }
        steps
    }
}

/// One live preem widget's shell-side renderer: the kit objects plus the
/// animation state the wire deliberately doesn't carry.
#[derive(Debug)]
enum Renderer {
    /// Pure — no instance state; re-rendered from the text on change.
    DotMatrix {
        text: String,
    },
    /// Pure.
    SevenSeg {
        text: String,
    },
    /// Pure, but the builder is worth keeping: it *is* the config, pre-parsed
    /// (and, uniquely in the kit, with the skin's palette already baked in — see
    /// [`invalidate_cached_frames`]).
    TextBox {
        boxed: kit::TextBox,
        text: String,
    },
    LedStrip {
        strip: kit::LedStrip,
        level: f32,
        /// The plugin's own inter-frame peak, when it computes one. Wins for the
        /// render it arrives on and never disturbs `hold`.
        explicit_peak: Option<f32>,
        /// Shell-owned peak-hold, present iff the config declared one.
        hold: Option<kit::PeakHold>,
        /// The declared fall rate, kept because the kit exposes no accessor for
        /// it and [`Renderer::animates`] must know whether the dot can still
        /// move: a `PeakHold` at rate `0.0` holds forever and must not keep the
        /// animation clock awake.
        hold_rate: f32,
        steps: Steps,
    },
    Marquee {
        strip: kit::MarqueeStrip,
        text: String,
        /// Scroll position in **dots**, fractional so a slow speed still moves.
        offset: f32,
        speed_dots_per_sec: f32,
    },
    Scope {
        scope: kit::Scope,
        /// The newest sample batch, stamped by the next animation step rather
        /// than at apply time — one decay + stamp per step is the kit's model,
        /// and it keeps a two-monitor mapping pass from double-stamping.
        pending: Option<Vec<f32>>,
        /// Steps since the last batch, so a fully-faded trail stops asking for
        /// repaints. Saturates.
        idle: u32,
        /// `256` never fades, so a scope at that persistence is static once its
        /// pending batch is stamped.
        fades: bool,
        /// Idle steps this trail needs to reach black, from the configured
        /// persistence — see [`scope_settle_steps`]. Per instance, because it is
        /// a function of the config: a constant was both too low (freezing a
        /// long phosphor mid-fade, permanently) and too high (dozens of
        /// pixel-identical repaints after a default one went quiet).
        settle_steps: u32,
        steps: Steps,
    },
    Gauge {
        gauge: kit::Gauge,
    },
    FlipBoard {
        board: kit::FlipBoard,
    },
}

/// A node's renderer plus the widget it was last built/updated from.
#[derive(Debug)]
struct Instance {
    /// The exact (already clamped) widget last applied. The equality check
    /// against this is what makes a mapping pass idempotent across monitors, and
    /// what distinguishes "config changed → rebuild" from "state changed →
    /// animate".
    applied: vocab::PreemWidget,
    /// `None` for a widget kind this build cannot render — see [`build`].
    renderer: Option<Renderer>,
    /// The last rasterised frame, re-used until something invalidates it.
    ///
    /// Shared, not owned (#911): every monitor's mapping pass takes a *handle*
    /// on this one allocation and hands it to that monitor's `PixelSurface`, so
    /// a frame costs one rasterisation and one buffer however many screens show
    /// it — see [`Instance::frame`].
    cached: Option<(u32, u32, Arc<[u8]>)>,
    /// How many times the renderer has been *built* (1 on first sight, +1 per
    /// config/kind change) and how many times a widget has been *applied* to it
    /// (a build or a state update; a no-op re-map doesn't count).
    ///
    /// Bookkeeping for the lifecycle tests: "updated in place" versus "rebuilt"
    /// is the whole contract of this module, and asserting it through rendered
    /// pixels would only prove that *something* differs. Two `u32`s per live
    /// preem node is not a cost worth `cfg`-ing away.
    builds: u32,
    applies: u32,
}

/// One plugin tree's instances, plus the bookkeeping of a mapping pass.
#[derive(Debug, Default)]
struct ScopeState {
    instances: HashMap<String, Instance>,
    /// Keys the in-flight mapping pass has touched; anything else is dropped
    /// when the pass ends.
    touched: HashSet<String>,
    /// Ordinal of the next un-id'd preem node in the in-flight pass.
    ordinal: usize,
    /// The `GdkFrameClock::frame_time` this scope was last advanced at, in
    /// microseconds — the baseline [`advance_scopes`] takes its `dt` from.
    ///
    /// **Per scope, not per mount**, and that is the whole double-mount rule
    /// (#897). One `Scope::card` is shared by every monitor's copy of that
    /// chip, and each of those mounts drives its own frame clock; a `dt`
    /// measured per mount would advance the shared instances once per mount per
    /// frame and run every animation at N× speed on a two-monitor desk — the
    /// same N× hazard the old single timer existed to avoid. Measuring from the
    /// scope's own last advance instead makes a second mount's tick cost
    /// whatever real time has passed since the first one's (often a fraction of
    /// a frame), so the animation runs at 1× no matter how many mounts show it,
    /// and gets the *union* of their refresh rates rather than the slowest.
    ///
    /// `None` until the first tick, which stamps and advances nothing: a scope
    /// has no meaningful baseline before some frame clock has looked at it, and
    /// one dropped frame at the start of a motion is invisible. Frame times come
    /// from `g_get_monotonic_time`, so clocks belonging to different monitors
    /// are directly comparable.
    ///
    /// Deliberately **not** touched by [`advance_all`], which takes an explicit
    /// `dt` and exists for the tests.
    last_advance_us: Option<i64>,
}

/// A one-shot per-tree diagnostic — something wrong with the *shape* of a
/// plugin's node tree that the host works around, tells the journal about once,
/// and then stops mentioning.
///
/// The latch is keyed by `(Scope, Warned)` rather than being a `bool` field on
/// [`ScopeState`], for two reasons. It has to **outlive** `ScopeState` (see
/// [`WARNED`]); and every diagnostic of this shape wants another key here rather
/// than a parallel table — which is what #918 and #901 then did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Warned {
    /// A preem node arrived with no `id` (#900): keyed by ordinal, so its
    /// animation state moves to a sibling on any insert or removal.
    NoId,
    /// Two preem nodes in one tree claimed the same `id` (#918), collapsing
    /// onto one renderer instance — two targets fighting one needle.
    ///
    /// Detected on the `false` that `state.touched.insert(key)` in
    /// [`map_widget`] already returns for a key touched twice in one pass, which
    /// is exactly the collision (the touched set is cleared per pass by
    /// [`begin_pass`], so a second monitor's pass is not a duplicate).
    DuplicateId,
    /// A tree asked for more than [`MAX_PREEM_NODES_PER_TREE`] renderer
    /// instances (#901); the nodes past the cap render as the unknown-widget
    /// placeholder.
    InstanceCap,
    /// A tree carried more than
    /// [`wire::MAX_NODES_PER_TREE`](hytte_plugin_proto::wire::MAX_NODES_PER_TREE)
    /// nodes of every kind together (#901); the mapped prefix is kept and the
    /// rest dropped. Raised by `wire_map`, which is where the tree is walked —
    /// one of the two [`Warned`]s this module does not emit itself.
    NodeCap,
    /// A tree nested deeper than
    /// [`wire::MAX_TREE_DEPTH`](hytte_plugin_proto::wire::MAX_TREE_DEPTH)
    /// (#901); everything below the cap is dropped.
    ///
    /// Its own variant rather than a `NodeCap` with a reason, because the two
    /// are different mistakes with different fixes — *send fewer nodes* versus
    /// *stop nesting* — and the latch is keyed per diagnostic, so a tree that is
    /// both too big and too deep gets told both things once each. Raised by
    /// `wire_map`.
    DepthCap,
}

impl Warned {
    /// This diagnostic's slot — its bit in [`WARNED`]'s per-scope mask, and its
    /// index in [`WARN_COUNTS`].
    ///
    /// One exhaustive match so a new variant is a compile error here rather than
    /// an aliased bit *and* an aliased counter. The mask is a `u8`, so it holds
    /// eight; a ninth variant needs a wider integer, and this is where to look.
    const fn slot(self) -> u32 {
        match self {
            Self::NoId => 0,
            Self::DuplicateId => 1,
            Self::InstanceCap => 2,
            Self::NodeCap => 3,
            Self::DepthCap => 4,
        }
    }

    /// This diagnostic's bit in a scope's mask. See [`slot`](Self::slot).
    const fn bit(self) -> u8 {
        1 << self.slot()
    }
}

thread_local! {
    /// GTK-main-thread-only renderer table. `to_ui_node` runs on the GTK thread
    /// (from `region.rs`'s reconcile and the drawer panel child) and so does the
    /// animation clock, so this never crosses a thread — the same discipline as
    /// `pump`'s slot-visibility map.
    static STORE: RefCell<HashMap<Scope, ScopeState>> = RefCell::new(HashMap::new());

    /// Which [`Warned`] diagnostics each tree has already been told about.
    ///
    /// **Deliberately outlives [`ScopeState`], and is never cleared.** A latch
    /// living on the `ScopeState` would be re-armed constantly, because a
    /// `ScopeState` is not a plugin session — [`end_pass`] drops the whole entry
    /// the moment a pass leaves the scope with zero instances, and
    /// [`forget_scope`] drops it on a drawer *close*
    /// (`region.rs`'s `forget_previous_panel_scope`), not only on a plugin
    /// leaving. Both are ordinary frames. A conditionally-rendered preem node
    /// would then re-warn on **every appearance** — a plugin toggling one at 20
    /// renders a second means ~10 journal lines a second, the exact noise the
    /// latch exists to prevent — and a panel would warn once per drawer open.
    ///
    /// So the contract is **at most once per shell run, per plugin tree**, not
    /// "once per plugin session": there is no clean GTK-thread hook for a
    /// session ending (`session.rs`'s teardown runs on a tokio task, and this
    /// table is main-thread-local), and every GTK-side teardown that *is*
    /// reachable fires on frames that are not session ends. A plugin restarted
    /// under the same id therefore does not get a second line — which is the
    /// quiet end of the trade, and stated as such in the wire docs so a plugin
    /// author knows to check the journal from the top of the shell's run.
    ///
    /// Growth is one entry per `(plugin id, tree, diagnostic)` that ever
    /// tripped: a short `String` and two discriminants, bounded in practice by
    /// the plugin roster. A plugin reconnecting under a *fresh* id each time
    /// would accumulate entries, but a process that can do that already runs
    /// code as this user.
    /// Growth is one entry per plugin tree that ever tripped anything: a short
    /// `String` and a `u8`.
    ///
    /// A `HashMap<Scope, u8>` bitmask rather than a `HashSet<(Scope, Warned)>`
    /// so the hot path can probe with a **borrow**. A set of pairs can only be
    /// asked `contains(&(scope.clone(), what))`, and since #901 the number of
    /// asks scales with the plugin's node count — a 4096-node tree past the
    /// instance cap asks once per refused node per pass per monitor, and every
    /// one of those after the first claims nothing. The map clones the `Scope`
    /// only on the insert that actually latches something.
    static WARNED: RefCell<HashMap<Scope, u8>> = RefCell::new(HashMap::new());
}

/// Claim the one-shot `what` diagnostic for `scope`: `true` the first time it is
/// asked for, `false` for the rest of the shell's run. See [`WARNED`].
///
/// **Every call site is `if warn_once(scope, what) { tracing::warn!(…) }`** and
/// nothing else — that is what makes the test counter this bumps a count of
/// journal lines rather than a count of claims. Claiming the latch without
/// emitting would silence the diagnostic forever and make the counter lie.
pub(super) fn warn_once(scope: &Scope, what: Warned) -> bool {
    let bit = what.bit();
    let claimed = WARNED.with_borrow_mut(|warned| {
        // The borrow-only path, which is the one a hostile tree takes thousands
        // of times a pass: look the scope up by reference, and only pay for the
        // clone when this really is the first time.
        if let Some(bits) = warned.get_mut(scope) {
            let first = *bits & bit == 0;
            *bits |= bit;
            first
        } else {
            warned.insert(scope.clone(), bit);
            true
        }
    });
    #[cfg(test)]
    if claimed {
        warn_counter(what).fetch_add(1, Ordering::Relaxed);
    }
    claimed
}

// ── mapping-pass API (called from `wire_map`) ────────────────────────────────

/// Open a mapping pass for `scope`: reset the un-id'd ordinal counter and the
/// touched set. Paired with [`end_pass`].
///
/// **This is the only place `touched` is cleared**, and deliberately so.
/// [`end_pass`] used to clear it too, after its last read of it; with both, a
/// mutation removing either one left the whole suite green, so the property
/// three comments attribute to *this* line — "a second monitor's pass over the
/// same tree is not a duplicate" (#918), and since #901 "the cap counts the
/// nodes this pass admitted" — was pinned by the pair rather than by the line
/// named. Clearing on the way *in* is also the stronger invariant: it holds at
/// the point it is relied on, for a pass that begins however the previous one
/// ended.
pub(super) fn begin_pass(scope: &Scope) {
    STORE.with_borrow_mut(|store| {
        if let Some(state) = store.get_mut(scope) {
            state.touched.clear();
            state.ordinal = 0;
        }
    });
}

/// Close a mapping pass for `scope`, dropping every instance the pass did not
/// touch — a preem node the plugin stopped rendering releases its phosphor
/// buffer / needle / board here rather than leaking for the session.
pub(super) fn end_pass(scope: &Scope) {
    STORE.with_borrow_mut(|store| {
        let Some(state) = store.get_mut(scope) else {
            return;
        };
        let ScopeState {
            instances, touched, ..
        } = state;
        instances.retain(|key, _| touched.contains(key));
        // `touched` is deliberately *not* cleared here — see [`begin_pass`],
        // which owns that and is where the comments say it happens.
        if state.instances.is_empty() {
            store.remove(scope);
        }
    });
}

/// Drop every instance in `scope` — a plugin's card leaving its region, or its
/// panel going away. Idempotent.
///
/// Drops **renderer state only**. The one-shot diagnostics in [`WARNED`] survive
/// on purpose: this fires on a drawer *close* as well as on a plugin leaving
/// (`region.rs`'s `forget_previous_panel_scope`), so clearing them here would
/// re-warn once per drawer open.
pub(super) fn forget_scope(scope: &Scope) {
    STORE.with_borrow_mut(|store| {
        store.remove(scope);
    });
}

/// What one [`map_widget`] call learned inside the [`STORE`] borrow, carried
/// back out so the `tracing::warn!`s can be emitted with the borrow released (a
/// subscriber is arbitrary code, and this table is thread-local and re-entered
/// by every mapping pass).
struct Mapped {
    /// The instance's `(width, height, RGBA8)`; `(0, 0, empty)` for the
    /// placeholder an over-cap or unrenderable node degrades to. The buffer is
    /// a handle on the instance's cached frame, never a copy of it (#911).
    frame: (u32, u32, Arc<[u8]>),
    /// The widget kind is one this build cannot render — [`build`] returned
    /// `None`. Latched process-wide, not per tree.
    unsupported: bool,
    /// The ordinal key an anonymous node landed in (#900), or `None` for an
    /// id'd one.
    anonymous_key: Option<String>,
    /// The kind of the node that claimed this id first *this pass*, when a
    /// second node in the pass claimed the same explicit id (#918). The id
    /// itself is not carried: [`report`] already takes it, and this field is
    /// built ahead of the latch on every collision on every pass.
    duplicate_of: Option<&'static str>,
    /// The node was refused an instance by [`MAX_PREEM_NODES_PER_TREE`] (#901).
    over_cap: bool,
}

/// Materialize one **already clamped** [`vocab::PreemWidget`] as the
/// [`UiNode::Pixels`] the reconciler blits into a `PixelSurface`.
///
/// Creates the renderer instance on first sight, updates it in place when only
/// the state moved, and rebuilds it when the config or the kind changed. A
/// widget kind this build cannot render degrades to a nothing-rendered surface
/// with a latched warning — `id` and `classes` are kept either way, so CSS
/// chrome stays and a later valid frame updates the same surface in place (the
/// posture the malformed-`Pixels` seam takes).
///
/// **A node with no `id` falls back to an ordinal key and warns** (#900) — see
/// the module docs for what that costs. The fallback is deliberate: `id` is
/// required by contract, but refusing to draw would only make a hand-rolled
/// client harder to write, so the node renders and the journal says why it may
/// misbehave. The warning is latched in [`WARNED`], which is **not** per pass and
/// **not** per [`ScopeState`]: at most once per plugin tree for the shell's run.
///
/// **Two preem nodes in one tree sharing an explicit `id` collapse onto one
/// renderer instance**, applying both widgets to it every pass — two targets
/// fighting one needle. The reconciler's own node keying has the same shape, so
/// this is not a new hazard, but it used to be undiagnosed; since #918 the
/// second touch warns once per tree ([`Warned::DuplicateId`]) on the `false`
/// `state.touched.insert(key)` returns for a key twice in one pass, naming the
/// id and both widget kinds. The behaviour is deliberately unchanged — last
/// writer wins, so nothing disappears.
///
/// **A tree gets at most [`MAX_PREEM_NODES_PER_TREE`] renderer instances**
/// (#901). Past that a node renders as the unknown-widget placeholder — the
/// same empty surface an unrenderable kind degrades to, keeping `id` and
/// classes — with one warning per tree ([`Warned::InstanceCap`]).
///
/// **The verdict is a pure function of the tree.** The cap is charged against
/// the nodes this *pass* has admitted (`touched`), so the first
/// `MAX_PREEM_NODES_PER_TREE` preem nodes in document order are admitted and
/// the rest refused — on every pass, on every monitor, for the same wire frame.
/// Charging it against the *live* instance count instead would make the answer
/// depend on what the previous pass's [`end_pass`] happened to sweep, and since
/// every monitor is a pass, the two monitors would render one frame
/// differently. A node that already holds an instance is admitted regardless,
/// which is what keeps a widget from losing its animation state on the frame
/// its tree reaches the cap.
///
/// The bound is therefore `MAX_PREEM_NODES_PER_TREE` instances **at rest**.
/// Within a pass a scope can transiently hold up to twice that — the incumbents
/// the plugin has stopped rendering, not yet swept, plus the newcomers this pass
/// admitted — and [`end_pass`] closes it back down to the touched set.
pub(super) fn map_widget(
    scope: &Scope,
    id: Option<&str>,
    classes: &[String],
    widget: &vocab::PreemWidget,
) -> UiNode {
    let mapped = STORE.with_borrow_mut(|store| {
        let state = store.entry(scope.clone()).or_default();
        // `None` for an id'd node; for an anonymous one, the ordinal key just
        // minted — the only handle that node has, and so what the warning names.
        let mut anonymous_key = None;
        let key = if let Some(id) = id {
            format!("id\u{1}{id}")
        } else {
            let ordinal = state.ordinal;
            state.ordinal += 1;
            let ordinal_key = format!("#{ordinal}");
            anonymous_key = Some(ordinal_key.clone());
            ordinal_key
        };

        // The instance cap (#901), checked before anything is inserted.
        //
        // Charged against `touched` — the nodes **this pass has already
        // admitted** — and deliberately not against `instances.len()`, which is
        // whatever survived the *previous* pass's `end_pass` sweep. Gating on
        // the carried-over set makes the verdict a function of history rather
        // than of the frame, and this module renders every frame once per
        // monitor: the two monitors would then disagree about the same wire
        // frame (the newcomer refused on the first pass, admitted on the second
        // after the sweep freed a slot), which is exactly what the idempotence
        // rule in the module docs forbids. It also made a tree pinned at
        // *exactly* the cap with one rotating id blank that node on every other
        // frame, for ever, without ever being over the cap.
        //
        // Against `touched` the answer is a pure function of the tree: the first
        // `MAX_PREEM_NODES_PER_TREE` preem nodes in document order are admitted,
        // the rest refused, on every pass and every monitor alike.
        //
        // A key that already holds an instance is always let through — that
        // clause is what keeps a node from losing its animation state on the
        // frame its tree happened to reach the cap. The ordinal is minted above
        // regardless, so an over-cap anonymous node does not silently renumber
        // its in-cap siblings.
        if !state.instances.contains_key(&key) && state.touched.len() >= MAX_PREEM_NODES_PER_TREE {
            return Mapped {
                frame: (0, 0, nothing()),
                unsupported: false,
                anonymous_key,
                duplicate_of: None,
                over_cap: true,
            };
        }

        // `false` ⇒ this pass already mapped a node under this key: two nodes in
        // one tree claiming the same `id` (#918). `begin_pass` clears the set,
        // so a second monitor's pass over the same tree is not a duplicate, and
        // an anonymous node cannot trip it — its ordinal is fresh by
        // construction.
        let first_touch = state.touched.insert(key.clone());
        let instance = state.instances.entry(key).or_insert_with(|| Instance {
            applied: widget.clone(),
            // No renderer yet: `apply` sees `None` and builds, which is also
            // what keeps the freshly-inserted `applied` above from short-
            // circuiting the very first apply.
            renderer: None,
            cached: None,
            builds: 0,
            applies: 0,
        });
        // Read *before* `apply` overwrites it: this is the kind of the node that
        // got here first this pass, which is half of what makes the warning
        // actionable. A duplicate always finds an existing instance (the earlier
        // touch created it), so this is never the incoming widget's own kind.
        //
        // A `&'static str` and not the id: `report` already has the id, and this
        // sits on the hot path *ahead* of the latch, so a plugin that duplicates
        // an id would otherwise pay a `String` allocation per duplicate node per
        // pass per monitor at 20 Hz for ever, for a line printed once.
        let duplicate_of = match (first_touch, id) {
            (false, Some(_)) => Some(instance.applied.kind()),
            // An anonymous node cannot collide: its ordinal is minted fresh.
            _ => None,
        };
        apply(instance, widget);
        let unsupported = instance.renderer.is_none();
        Mapped {
            frame: instance.frame(),
            unsupported,
            anonymous_key,
            duplicate_of,
            over_cap: false,
        }
    });
    report(scope, id, widget, &mapped);
    let (width, height, data) = mapped.frame;

    UiNode::Pixels {
        id: id.map(str::to_owned),
        width,
        height,
        data,
        // The kit bakes its own upscale into the buffer (`Frame::upscale`, and
        // every widget's `scale` knob), so the host must not scale again —
        // exactly what `Frame::into_node` hard-codes for the plugin-side path.
        scale: 1,
        classes: classes.to_vec(),
    }
}

/// Emit the one-shot journal lines this mapping of `widget` earned.
///
/// Split out of [`map_widget`] rather than inlined: it is called with the
/// [`STORE`] borrow **released**, because a `tracing` subscriber is arbitrary
/// code and the table is a thread-local that every mapping pass re-enters. The
/// latch ([`WARNED`]) is a second thread-local for the same reason it is a
/// separate table — it must outlive the [`ScopeState`] that borrow touched.
///
/// Each block is `if <claim the latch> { warn! }` and nothing else, which is
/// what makes [`warn_once`]'s test counter a count of journal lines. The four
/// diagnostics are independent: an anonymous node beside a duplicate pair
/// produces both lines, because the latch is keyed by `(Scope, Warned)`.
fn report(scope: &Scope, id: Option<&str>, widget: &vocab::PreemWidget, mapped: &Mapped) {
    if let Some(key) = mapped.anonymous_key.as_deref()
        && warn_once(scope, Warned::NoId)
    {
        // `key` is the ordinal slot this node landed in — the only handle it
        // has, since it declined to name itself. It is `#0` in practice (the
        // first un-id'd node of a pass is the one that trips the latch); the
        // count of anonymous nodes in the tree would be the more useful number
        // but is not known until `end_pass` closes, and is not worth deferring
        // the whole diagnostic for.
        tracing::warn!(
            plugin = scope.plugin_id(),
            tree = ?scope.role(),
            node = %key,
            kind = widget.kind(),
            "preem node without an id — animation state cannot be tracked across reorders; \
             the node falls back to a positional key and may inherit a sibling's phosphor, \
             needle or flip clocks (further occurrences in this tree are silenced for the \
             rest of this shell run)",
        );
    }

    if let Some((first_kind, node)) = mapped.duplicate_of.zip(id)
        && warn_once(scope, Warned::DuplicateId)
    {
        tracing::warn!(
            plugin = scope.plugin_id(),
            tree = ?scope.role(),
            node = %node,
            first_kind,
            second_kind = widget.kind(),
            "two preem nodes in this tree share an id — they collapse onto one renderer \
             instance and both are applied to it every frame, so one widget's needle, \
             phosphor or flip clocks fight the other's; the last one rendered wins. Give \
             each preem node an id unique within its tree (further occurrences in this tree \
             are silenced for the rest of this shell run)",
        );
    }

    if mapped.over_cap && warn_once(scope, Warned::InstanceCap) {
        tracing::warn!(
            plugin = scope.plugin_id(),
            tree = ?scope.role(),
            node = ?id,
            kind = widget.kind(),
            cap = MAX_PREEM_NODES_PER_TREE,
            "this tree asks for more preem renderer instances than the host will hold; the \
             nodes past the cap render as an empty surface. A per-core/per-sink readout is \
             one LedStrip or one Scope, not one widget each (further occurrences in this \
             tree are silenced for the rest of this shell run)",
        );
    }

    if mapped.unsupported && !UNSUPPORTED_WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            node = ?id,
            kind = widget.kind(),
            "this shell cannot render that preem widget kind; rendering nothing \
             (further occurrences are silenced)",
        );
    }
}

/// Bring `instance` in line with `widget`: rebuild on a kind/config change,
/// update state otherwise, and no-op when nothing moved (the multi-monitor
/// case).
fn apply(instance: &mut Instance, widget: &vocab::PreemWidget) {
    // `same_widget`, not `==`: derived `PartialEq` is not reflexive over a
    // non-finite float, and a short-circuit that never fires is a permanent
    // 20 Hz loop rather than a missed optimisation. See `sanitize_in_place`.
    if instance.renderer.is_some() && same_widget(&instance.applied, widget) {
        return;
    }
    let rebuild = instance
        .renderer
        .as_ref()
        .is_none_or(|renderer| !renderer.matches_kind(widget))
        || !same_config(&instance.applied, widget);
    if rebuild {
        instance.renderer = build(widget);
        instance.builds = instance.builds.saturating_add(1);
    } else if let Some(renderer) = instance.renderer.as_mut() {
        renderer.update(widget);
    }
    instance.applies = instance.applies.saturating_add(1);
    instance.applied = widget.clone();
    instance.cached = None;
}

impl Instance {
    /// The instance's current frame as `(width, height, RGBA8)`, rasterising it
    /// only when the cache is cold. `(0, 0, empty)` is the unsupported-widget
    /// placeholder.
    ///
    /// **A warm cache costs a refcount, not a copy** (#911). This runs once per
    /// monitor per mapping pass — the instance table is keyed by scope and
    /// shared across mounts — so returning an owned `Vec` here was a full RGBA
    /// clone per screen per tick, and a second one for every unchanged chip a
    /// blanket repaint walked past. The `Arc` travels all the way into
    /// `hytte_ui`'s `PixelSurface`, which adopts it for the texture upload and
    /// settles an unchanged frame with an `Arc::ptr_eq` (#907).
    fn frame(&mut self) -> (u32, u32, Arc<[u8]>) {
        if let Some((width, height, data)) = self.cached.as_ref() {
            return (*width, *height, Arc::clone(data));
        }
        let style = display_style(self.applied.style());
        // The palette is resolved here, per rasterisation, never baked into the
        // instance — which is what makes a theme change a cache drop rather than
        // a rebuild (#396). A pin resolves to the same colors every time, so a
        // pinned widget re-rasterises to identical bytes.
        let pins = pins_for(self.applied.style());
        let rendered = self.renderer.as_ref().map_or_else(
            || (0, 0, nothing()),
            |renderer| {
                let frame = kit::with_pins(pins, || renderer.render(style));
                // Both dimensions or neither: a lone `unwrap_or(0)` could pair a
                // zero dimension with a non-empty buffer and break the
                // `len == w * h * 4` invariant every `Node::Pixels` consumer
                // (and `mapped_pixels`) relies on. The wire caps put this far
                // out of reach; the seam is here so it cannot be reached at all.
                match (
                    u32::try_from(frame.width()),
                    u32::try_from(frame.height()),
                    u32::try_from(frame.data().len()),
                ) {
                    // One copy out of the kit's frame, exactly what `to_vec`
                    // was — an `Arc<[u8]>` carries its refcount inline ahead of
                    // the bytes, so it can never adopt a `Vec`'s allocation and
                    // there is nothing to be saved by going through one.
                    (Ok(width), Ok(height), Ok(_)) => (width, height, Arc::from(frame.data())),
                    _ => (0, 0, nothing()),
                }
            },
        );
        // A tuple clone of two `u32`s and a refcount — the cache and the caller
        // share the one buffer.
        self.cached = Some(rendered.clone());
        rendered
    }
}

/// The empty RGBA buffer every "renders nothing" placeholder shares — this
/// module's over-cap and unrenderable-kind degradations, and
/// [`wire_map`](super::wire_map)'s malformed-buffer one.
///
/// `Vec::new()` was free; `Arc::from(&[][..])` is not — it still allocates the
/// refcount header — and a placeholder is built per degraded node per mapping
/// pass per monitor. One process-wide empty buffer keeps that a refcount bump,
/// and makes every placeholder pointer-identical into the bargain.
pub(super) fn nothing() -> Arc<[u8]> {
    static NOTHING: std::sync::OnceLock<Arc<[u8]>> = std::sync::OnceLock::new();
    Arc::clone(NOTHING.get_or_init(|| Arc::from(&[][..])))
}

// ── the frame clock's half (#897) ────────────────────────────────────────────

/// Advance `scopes`' live renderers up to `frame_time_us` — one mount's
/// frame-clock tick — returning **which of them** changed and so need a repaint.
///
/// This is the production advance path since #897, called from
/// `pump::install_animation`'s tick callback with the mount's own
/// `GdkFrameClock::frame_time` and the scopes that mount is currently showing.
/// [`advance_all`] is its test-only sibling.
///
/// ## Where the `dt` comes from, and why it is not the caller's
///
/// The caller hands a *timestamp*, not an elapsed time, and each scope takes its
/// own `dt` from [`ScopeState::last_advance_us`] — see that field for the
/// double-mount argument in full. In one line: the instance table is keyed by
/// scope and shared across monitors, so two mounts showing the same chip would
/// each advance it by a full frame and run it at 2× speed if the `dt` were the
/// caller's. Measuring per scope makes the second mount's tick worth only the
/// real time since the first one's, whatever the two clocks' phase.
///
/// The `dt` is clamped to [`MAX_TICK_DT_US`], which is what makes a park and a
/// stall cost the same bounded catch-up rather than the interval they spanned —
/// and, at eight steps rather than one, leaves every frame interval a real
/// compositor can produce (down to 2.5 Hz) passing through **rate-exact**. See
/// that constant for why the tighter clamp was wrong.
///
/// A scope with no entry in the store is skipped: a mount can name a scope whose
/// instances a mapping pass has since dropped ([`end_pass`] removes an empty
/// entry), and that is a settled scope, not an error.
///
/// The return type is a scope list rather than a `bool` because the fan-out is
/// otherwise global: collapsing every instance into one flag made a single
/// animating marquee in one plugin's drawer panel re-map every plugin's whole
/// tree in every bar region on every monitor, 20× a second.
///
/// **This is the second guard, not the only one.** Since #907 the surface holds
/// the last accepted buffer and compares before it uploads (`hytte-ui`'s
/// `PixelSurface::set_pixels_shared`), so a blanket nudge no longer costs a
/// `glib::Bytes` + `gdk::MemoryTexture` + `queue_draw` per unchanged
/// `Node::Pixels` node it walks past — and since #911 an unchanged frame
/// reaches that surface as the *same* `Arc`, so the compare is a pointer
/// compare and no RGBA block is copied out of the store on the way. What a
/// blanket nudge still costs is everything upstream of that: re-running
/// `reconcile_region` over every plugin's whole tree and re-mapping every wire
/// node, per monitor, every frame. Naming the movers lets
/// `pump::request_preem_repaint` nudge only the mailboxes that actually hold
/// one and skip all of it.
pub(super) fn advance_scopes(scopes: &[Scope], frame_time_us: i64) -> Vec<Scope> {
    STORE.with_borrow_mut(|store| {
        let mut moved = Vec::new();
        for scope in scopes {
            let Some(state) = store.get_mut(scope) else {
                continue;
            };
            let dt = match state.last_advance_us.replace(frame_time_us) {
                // `saturating_sub`, then the clamp inside `micros_to_secs`: a
                // frame time that went backwards owes nothing, rather than
                // handing the kit a negative `dt` every primitive would have to
                // defend against on its own.
                Some(last) => micros_to_secs(frame_time_us.saturating_sub(last)),
                None => 0.0,
            };
            if advance_state(state, dt) {
                moved.push(scope.clone());
            }
        }
        moved
    })
}

/// Advance every live renderer by `dt` seconds, returning the scopes that moved.
///
/// **Test-only since #897**: production advances per mount through
/// [`advance_scopes`], which is where the per-scope `dt` and the frame-time
/// baseline live. This one takes the `dt` straight from its caller and leaves
/// [`ScopeState::last_advance_us`] alone — exactly what a test wanting "advance
/// everything by half a second" needs, and exactly what a frame clock must not
/// do.
#[cfg(test)]
pub(super) fn advance_all(dt: f32) -> Vec<Scope> {
    STORE.with_borrow_mut(|store| {
        let mut moved = Vec::new();
        for (scope, state) in store.iter_mut() {
            if advance_state(state, dt) {
                moved.push(scope.clone());
            }
        }
        moved
    })
}

/// Advance one scope's instances by `dt`, dropping the cached frame of each one
/// that moved; answers whether the scope as a whole needs a repaint.
///
/// The shared body of [`advance_scopes`] and [`advance_all`], split out so the
/// production path and the test path cannot drift on what "moved" means — which
/// is the property every parity assertion in `plugins::tests` reads through
/// `advance_all` and every frame on glass reads through `advance_scopes`.
fn advance_state(state: &mut ScopeState, dt: f32) -> bool {
    let mut moved = false;
    for instance in state.instances.values_mut() {
        if let Some(renderer) = instance.renderer.as_mut()
            && renderer.advance(dt)
        {
            instance.cached = None;
            moved = true;
        }
    }
    moved
}

/// Whether any of `scopes`' live renderers still has something to animate — the
/// predicate one mount's tick callback keeps itself armed on (#897).
///
/// Per mount rather than global, so a settled bar region parks its frame clock
/// while an open drawer's gauge is still swinging. A scope with no entry in the
/// store contributes nothing: it has no instances, so it has nothing to animate.
pub(super) fn any_animating_in(scopes: &[Scope]) -> bool {
    STORE.with_borrow(|store| {
        scopes
            .iter()
            .filter_map(|scope| store.get(scope))
            .any(state_animates)
    })
}

/// Whether any live renderer anywhere still has something to animate.
///
/// **Test-only since #897**: no production caller wants the global answer any
/// more, because nothing drives every scope at once — each mount asks
/// [`any_animating_in`] about its own. Kept because it is the shape a couple of
/// dozen assertions in `plugins::tests` are written against ("does a parked
/// marquee stop asking for ticks"), and narrowing each of them to a scope list
/// would only restate the scope it already built.
#[cfg(test)]
pub(super) fn any_animating() -> bool {
    STORE.with_borrow(|store| store.values().any(state_animates))
}

/// Whether one scope's instances have animation left to run.
fn state_animates(state: &ScopeState) -> bool {
    state
        .instances
        .values()
        .any(|instance| instance.renderer.as_ref().is_some_and(Renderer::animates))
}

/// Drop every cached frame **and the memoized role colors**, so the next mapping
/// pass re-rasterises against the theme as it now stands.
///
/// The kit reads the desktop accent from a process-global
/// (`hytte_preem::set_accent`), which the shell re-publishes on every accent /
/// color-scheme change (#396/#862). A cached frame was rasterised under the
/// *old* ink, so without this a shell-rendered preem widget would keep the
/// previous accent until its state next moved.
///
/// Since #885 the same call is the theme seam for the semantic roles: a color
/// scheme flip moves `@success_color` and friends exactly as it can move
/// `@accent_color`, so [`role_inks`]'s memo is dropped here rather than kept for
/// the session. A **pin** is the deliberate exception on the way out — an ink, a
/// field or a `TextBox`'s notdef — the frames are dropped like everyone's, and
/// the re-render reproduces them byte for byte, because the colors that widget
/// asked for did not change.
///
/// `TextBox` is the one kit widget that resolves its palette at **construction**
/// (`TextBox::styled` bakes bg/ink/notdef into the builder) rather than at
/// render time, so dropping its cached bytes is not enough — its renderer is
/// rebuilt. That is free: the text box is pure, so a rebuild loses no animation
/// state. Every other widget takes its `DisplayStyle` per render and re-tints on
/// the re-rasterise alone.
pub(super) fn invalidate_cached_frames() {
    ROLE_INKS.set(None);
    STORE.with_borrow_mut(|store| {
        for state in store.values_mut() {
            for instance in state.instances.values_mut() {
                instance.cached = None;
                if matches!(instance.renderer, Some(Renderer::TextBox { .. })) {
                    instance.renderer = build(&instance.applied);
                }
            }
        }
    });
}

// ── reflexive equality over non-finite floats ────────────────────────────────

/// Fold every non-finite (`NaN`/`±inf`) float in a widget onto one canonical
/// finite stand-in, in place — **for comparison only**.
///
/// # The bug this closes
///
/// `vocab::PreemWidget` derives `PartialEq`, and IEEE `NaN != NaN`. A widget
/// carrying one is therefore **never equal to itself**, which defeats the two
/// gates this whole module rests on:
///
/// - [`apply`]'s short-circuit never fires, so a `Scope` re-arms `pending` and
///   zeroes `idle` on every mapping pass. `animates()` stays true, `advance`
///   always has a batch to stamp, the clock reports movement, the fan-out
///   re-maps, and the re-map re-arms it again: a **permanent 20 Hz re-map plus
///   re-rasterisation loop** for as long as the plugin keeps sending that
///   frame. One `sum / count` with `count == 0` — the shape of every meter — is
///   enough to pin the shell there, and it makes #897's "park the clock when
///   nothing animates" unreachable.
/// - [`same_config`] never agrees either, so a non-finite **config** float
///   rebuilds the renderer on every pass: the needle returns to rest, the
///   phosphor clears and the board blanks 20× a second per monitor, and the
///   widget can never animate at all.
///
/// # This is not the boundary sanitiser
///
/// Scrubbing non-finite floats *at the wire* belongs in
/// `PreemWidget::clamp_in_place`, which already special-cases
/// `MarqueeConfig::speed_dots_per_sec` for exactly this reason ("a NaN/inf
/// speed would poison the shell's offset integrator") and simply never carried
/// the reasoning to the other twelve float fields. That fix is proto-side, on
/// `fix/preem-clamp-non-finite`, so `clamped(w) == clamped(w)` holds for every
/// consumer of the vocabulary — the SDK included, which has the same class of
/// bug. It is deliberately **not** duplicated here: the same scrubbing logic in
/// two crates would drift.
///
/// What stays here is the host's own robustness. This function never touches a
/// widget the renderer will draw; it only produces a throwaway canonical form
/// so [`same_widget`]/[`same_config`] stay reflexive even if an unsanitised
/// widget ever reaches this module — a proto older than that fix, a future
/// caller that skips the clamp, a test.
///
/// # The canonical form
///
/// Every non-finite float folds to `0.0`, and every non-finite `Option<f32>` to
/// `None`. It does not have to agree with whatever substitutions the proto
/// clamp settles on: nothing renders from this, and both sides of a comparison
/// go through it. The one thing it gives up is telling `NaN` from `+inf` — two
/// equally unrenderable readings compare equal, so a transition between them
/// does not churn the renderer. That is the conservative direction.
fn canonicalize_non_finite(widget: &mut vocab::PreemWidget) {
    use vocab::PreemWidget as W;
    match widget {
        // No floats at all: the text widgets carry only strings and integers.
        W::DotMatrix { .. } | W::SevenSeg { .. } | W::TextBox { .. } => {}
        W::LedStrip { config, state } => {
            if let Some(hold) = config.peak_hold.as_mut() {
                hold.rate = finite_or_zero(hold.rate);
            }
            state.level = finite_or_zero(state.level);
            state.peak = state.peak.filter(|peak| peak.is_finite());
        }
        W::Marquee { config, .. } => {
            config.speed_dots_per_sec = finite_or_zero(config.speed_dots_per_sec);
        }
        W::Scope { state, .. } => {
            for sample in &mut state.samples {
                *sample = finite_or_zero(*sample);
            }
        }
        W::Gauge { config, state } => {
            config.sweep_deg = finite_or_zero(config.sweep_deg);
            config.frequency_hz = finite_or_zero(config.frequency_hz);
            config.damping = finite_or_zero(config.damping);
            config.range.low = finite_or_zero(config.range.low);
            config.range.high = finite_or_zero(config.range.high);
            state.target = finite_or_zero(state.target);
        }
        W::FlipBoard { config, .. } => {
            config.duration_secs = config.duration_secs.filter(|secs| secs.is_finite());
            config.stagger_secs = config.stagger_secs.filter(|secs| secs.is_finite());
        }
    }
}

/// `value` when it is finite, `0.0` otherwise.
fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

/// Reflexive equality for two widgets, whatever their floats.
///
/// The fast path is the derived `PartialEq`, which is what runs for every
/// finite widget — i.e. every widget, once the wire clamp scrubs non-finite
/// floats. The slow path canonicalises both sides
/// ([`canonicalize_non_finite`]), so a pair of widgets differing only in
/// non-finite floats still compares equal and [`apply`] still short-circuits.
///
/// Deliberately built on the derived comparison of a canonicalised clone rather
/// than on a hand-written field-by-field walk: a hand-written one can *forget* a
/// field, and a forgotten field means a state change silently dropped — a worse
/// failure than the one being fixed. This spelling cannot miss a field, and its
/// clone only runs on a pass where the fast path already said "different".
fn same_widget(a: &vocab::PreemWidget, b: &vocab::PreemWidget) -> bool {
    if a == b {
        return true;
    }
    let (mut a, mut b) = (a.clone(), b.clone());
    canonicalize_non_finite(&mut a);
    canonicalize_non_finite(&mut b);
    a == b
}

// ── config / kind identity ───────────────────────────────────────────────────

/// Whether two widgets are the same kind **and** carry the same config — the
/// "update in place" predicate. A `false` here rebuilds the renderer, which is
/// what the vocabulary's per-config docs promise.
///
/// Non-finite-tolerant on the slow path for the same reason as [`same_widget`]:
/// a `NaN` in a `GaugeConfig` (four floats), a `FlipBoardConfig` (two) or a
/// `PeakHoldConfig` (one) would otherwise make a config unequal to itself and
/// rebuild the renderer on every mapping pass, freezing the widget at rest
/// while allocating a fresh kit object 20× a second per monitor.
fn same_config(a: &vocab::PreemWidget, b: &vocab::PreemWidget) -> bool {
    if config_eq(a, b) {
        return true;
    }
    let (mut a, mut b) = (a.clone(), b.clone());
    canonicalize_non_finite(&mut a);
    canonicalize_non_finite(&mut b);
    config_eq(&a, &b)
}

/// [`same_config`]'s comparison proper, over the values as given.
fn config_eq(a: &vocab::PreemWidget, b: &vocab::PreemWidget) -> bool {
    use vocab::PreemWidget as W;
    match (a, b) {
        (W::DotMatrix { config: x, .. }, W::DotMatrix { config: y, .. }) => x == y,
        (W::SevenSeg { config: x, .. }, W::SevenSeg { config: y, .. }) => x == y,
        (W::TextBox { config: x, .. }, W::TextBox { config: y, .. }) => x == y,
        (W::LedStrip { config: x, .. }, W::LedStrip { config: y, .. }) => x == y,
        (W::Marquee { config: x, .. }, W::Marquee { config: y, .. }) => x == y,
        (W::Scope { config: x, .. }, W::Scope { config: y, .. }) => x == y,
        (W::Gauge { config: x, .. }, W::Gauge { config: y, .. }) => x == y,
        (W::FlipBoard { config: x, .. }, W::FlipBoard { config: y, .. }) => x == y,
        // Different kinds: never the same config, always a rebuild.
        _ => false,
    }
}

/// Resolve a wire [`vocab::StyleRef`] to the kit's `DisplayStyle`, **by name**.
///
/// Matching on the lowercase word rather than on the enum shape is the idiom the
/// shell already uses for its own preem config (`panels::stats`'s
/// `parse_core_leds_style`), and it means the two enums can drift a variant apart
/// without a compile error here — a wire style this kit build has no counterpart
/// for falls back to the kit's own default rather than failing the render.
///
/// [`vocab::StyleRef::accent`] and [`vocab::StyleRef::ink`] are the *other* half
/// of a style reference and resolve through [`ink_for`].
fn display_style(style: vocab::StyleRef) -> kit::DisplayStyle {
    let name = style.style.name();
    kit::DisplayStyle::ALL
        .into_iter()
        .find(|candidate| candidate.name() == name)
        // `DisplayStyle` has no `Default`; `ALL` is the kit's canonical rotation
        // order and its head is `Vfd`, which is also `StyleName`'s own default —
        // so an unmatched name lands on the skin an omitted one would have.
        .unwrap_or(kit::DisplayStyle::ALL[0])
}

// ── semantic ink roles (#885) ────────────────────────────────────────────────

/// The three theme colors a [`vocab::AccentRole`] can name that the session
/// accent does not already cover.
///
/// `Accent` is deliberately absent: the shell installs `@accent_color` into the
/// kit's process global on every change (`pump::tint_in_process_surfaces`), so
/// "the accent" is what `kit::Ink::Default` already means. Resolving it a second
/// time here would be a second source of truth for one color.
///
/// A `None` field is a color this theme does not define (or a lookup made before
/// the display's CSS providers were up); the role then degrades to the session
/// accent rather than to something invented locally.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct RoleInks {
    pub(super) success: Option<kit::Rgba>,
    pub(super) warning: Option<kit::Rgba>,
    pub(super) error: Option<kit::Rgba>,
}

impl RoleInks {
    /// The ink for one role, or `None` for a role this struct does not carry.
    fn get(self, role: vocab::AccentRole) -> Option<kit::Rgba> {
        match role {
            vocab::AccentRole::Success => self.success,
            vocab::AccentRole::Warning => self.warning,
            vocab::AccentRole::Error => self.error,
            vocab::AccentRole::Accent | vocab::AccentRole::Neutral => None,
        }
    }
}

thread_local! {
    /// The resolved role colors, memoized until the theme moves.
    ///
    /// Resolution is a GTK named-color lookup, and a mapping pass rasterises
    /// many widgets — so doing it per render would build a throwaway widget per
    /// frame for a value that changes when the user picks a new accent. Dropped
    /// by [`invalidate_cached_frames`], which is *already* the shell's
    /// "the theme moved" seam, so the memo can never outlive the frames it
    /// tinted.
    static ROLE_INKS: RefCell<Option<RoleInks>> = const { RefCell::new(None) };
}

/// The role colors for this render, resolving them from the theme on first use
/// since the last invalidation.
fn role_inks() -> RoleInks {
    ROLE_INKS.with_borrow_mut(|memo| *memo.get_or_insert_with(resolve_role_inks))
}

/// Resolve `@success_color` / `@warning_color` / `@error_color` off the live
/// theme, the same way `pump::resolve_accent_color` resolves `@accent_color`:
/// libadwaita registers them as display-scope named colors, so a throwaway
/// unrealized widget resolves them.
///
/// Deliberately separate from `pump`'s accent resolver rather than a
/// generalization of it: that one materializes the *wire* accent for
/// out-of-process plugins and writes the kit's global, while these three are
/// shell-only and exist purely to ink a role. The style-context lookup is
/// deprecated in GTK4 and libadwaita's typed getters need `v1_6`, so this
/// carries the same scoped `allow` and the same reasoning as `pump`'s.
///
/// Returns every color unset when GTK is not up — the hermetic `cargo test`
/// case, where a widget probe would panic. A role then falls back to the session
/// accent, which is what an unthemed session renders anyway.
fn resolve_role_inks() -> RoleInks {
    if !gtk::is_initialized_main_thread() {
        return RoleInks::default();
    }
    let probe = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let lookup = |name: &str| {
        #[allow(deprecated)]
        let rgba = probe.style_context().lookup_color(name)?;
        // Clamped then scaled into `0..=255` and rounded, so the cast is exact
        // — the same conversion (and the same reasoning) as `pump`'s
        // `rgba_to_bytes`. Alpha is forced opaque: a preem frame is a screen.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let chan = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        Some([
            chan(rgba.red()),
            chan(rgba.green()),
            chan(rgba.blue()),
            0xff,
        ])
    };
    RoleInks {
        success: lookup("success_color"),
        warning: lookup("warning_color"),
        error: lookup("error_color"),
    }
}

/// Install role colors explicitly, bypassing the theme lookup — the seam the
/// role tests resolve against, since the hermetic test binary has no GTK display
/// and every role would otherwise degrade to the session accent (and so prove
/// nothing about resolution).
#[cfg(test)]
pub(super) fn set_role_inks(inks: RoleInks) {
    ROLE_INKS.set(Some(inks));
}

/// The kit ink a widget's style reference asks for.
///
/// The precedence *is* the design settled on #885:
///
/// 1. a pinned [`ink`](vocab::StyleRef::ink) wins outright, and by winning opts
///    the widget out of the live re-tint — it renders this exact color while the
///    desktop changes around it;
/// 2. `Success`/`Warning`/`Error` resolve against the live theme, falling back to
///    the session accent when the theme does not define one;
/// 3. `Neutral` takes the skin's own ink, refusing even the accent;
/// 4. `Accent` — and a `StyleRef` that names no role at all — take the session
///    accent, which is `kit::Ink::Default`: exactly what every preem widget
///    rendered before roles were resolved at all, so an old frame is unmoved.
///
/// # A resolved role is *asked about*, a pinned ink is not (#935)
///
/// Arms 1 and 2 both end at [`kit::Ink::Fixed`], and the kit cannot tell them
/// apart — so it treats both as stated on purpose and takes them verbatim, which
/// is right for a pin and wrong for a role. `Fixed` bypasses the skin's
/// [`AccentPolicy`][policy], and on the reflective `Lcd` — the one skin whose
/// field is *lighter* than its ink — libadwaita's dark-theme status colors then
/// landed at 1.06–1.48:1 against `#a9b47e`, which is invisible.
///
/// The fix is entirely on this side of the wire, because a role color is the
/// shell's answer, not the author's: it is run through
/// [`kit::DisplayStyle::admit_ink`] — #933's seam, the same ramp the accent path
/// takes inside the kit — *before* it becomes a pin. So the skin still decides
/// what an ink it did not choose looks like, and pins stay unconditional (they
/// return above, never reaching this match).
///
/// The ground it is admitted against is the **effective** one: the widget's own
/// [`field`](vocab::StyleRef::field) pin when it has one, the skin's otherwise.
/// This mirrors `palette_with`'s ordering for the accent, and for the same
/// reason — an ink chosen against a ground the widget never floods is a
/// guarantee about a pixel that does not exist.
///
/// `Neutral` and `Accent` are deliberately untouched: the first *is* the skin's
/// own ink, and the second reaches the kit as [`kit::Ink::Default`], which the
/// skin already admits for itself.
///
/// **Admitted at use, not memoized.** [`role_inks`] caches the three *theme*
/// colors; the admission is per `(role color, skin, effective field)` and so
/// would need a memo key three wide for a scan over a 65-stop integer ramp that
/// the accent path already pays, unmemoized, on every `Lcd` rasterisation. Doing
/// it here also keeps the invalidation story exactly as #912 left it — the memo
/// holds only what the theme said, so [`invalidate_cached_frames`]'s
/// `ROLE_INKS.set(None)` remains the whole of it and no derived value can go
/// stale behind it.
///
/// [policy]: kit::DisplayStyle::admit_ink
fn ink_for(style: vocab::StyleRef) -> kit::Ink {
    if let Some(ink) = style.ink {
        return kit::Ink::Fixed(ink);
    }
    match style.accent {
        Some(vocab::AccentRole::Neutral) => kit::Ink::Base,
        // Named before the memo, not through it: [`RoleInks`] deliberately does
        // not carry the accent, so resolving one for `Accent` would build a
        // throwaway probe widget and do three `lookup_color`s on the first
        // render after every theme change — in a session where nothing asks for
        // a status role at all — only to throw the answer away.
        Some(vocab::AccentRole::Accent) | None => kit::Ink::Default,
        Some(role) => role_inks().get(role).map_or(kit::Ink::Default, |ink| {
            kit::Ink::Fixed(display_style(style).admit_ink(ink, style.field))
        }),
    }
}

/// The kit palette scope a widget's style reference asks for: [`ink_for`]'s
/// answer, plus the optional pinned [`field`](vocab::StyleRef::field).
///
/// The field has no role vocabulary and no accent behind it — there is exactly
/// one way for it to move, and that is a plugin naming it — so this is the whole
/// of its resolution. That asymmetry is the design (#885): the ink is the
/// semantic slot the desktop re-tints, the field is the ground, and a plugin
/// reaches for the ground only when it has to match something the skins cannot
/// express (#884's `pet` and `caw` bubbles, whose lilac sits beside a hand-drawn
/// face).
fn pins_for(style: vocab::StyleRef) -> kit::Pins {
    kit::Pins {
        ink: ink_for(style),
        field: style.field,
    }
}

/// The pins one style reference resolves to — the render path's own
/// [`pins_for`], reachable from the test module.
///
/// The role tests assert over every skin, and three of the four bloom or mask
/// their lit layer, so hunting a fully-lit pixel out of a rendered frame would
/// make the assertion about the post-passes rather than about the resolution.
/// The rendered claims stay rendered (see the byte pin and the dark-skin
/// parity test); this is for the ones that are about *what the shell decided*.
#[cfg(test)]
pub(super) fn resolved_pins(style: vocab::StyleRef) -> kit::Pins {
    pins_for(style)
}

// ── construction / update / advance / render ─────────────────────────────────

/// `u32` → `usize` for a value the wire caps well below either type's range.
fn dim(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// Build a renderer for `widget`, or `None` for a kind this build cannot draw.
///
/// The match is exhaustive over today's vocabulary, so `None` is unreachable as
/// this file stands — the return type is the seam that keeps it *representable*
/// (a future `PreemWidget` variant whose kit widget this build predates), which
/// is what makes the unknown-widget placeholder in [`map_widget`] a real path
/// rather than dead code to be deleted. The instance is kept either way, so a
/// plugin that keeps sending it costs one warn, not one per frame.
// `unnecessary_wraps` is exactly right about today's body and exactly wrong
// about the contract: the `Option` *is* the placeholder seam, and collapsing it
// would delete the unknown-widget path #883 is required to keep (and that
// `an_unrenderable_preem_widget_degrades_to_an_empty_surface` covers).
#[allow(clippy::unnecessary_wraps)]
fn build(widget: &vocab::PreemWidget) -> Option<Renderer> {
    use vocab::PreemWidget as W;
    #[cfg(test)]
    if force_unsupported() {
        return None;
    }
    let style = display_style(widget.style());
    // The whole build runs inside the widget's palette scope, because `TextBox`
    // is the one kit widget that resolves its palette at *construction* — see
    // `invalidate_cached_frames`. Every other arm resolves at render time and is
    // unaffected by the scope being open here, so scoping the build wholesale
    // costs nothing and cannot miss a future widget that bakes.
    let pins = pins_for(widget.style());
    Some(kit::with_pins(pins, || match widget {
        W::DotMatrix { state, .. } => Renderer::DotMatrix {
            text: state.text.clone(),
        },
        W::SevenSeg { state, .. } => Renderer::SevenSeg {
            text: state.text.clone(),
        },
        W::TextBox { config, state } => Renderer::TextBox {
            boxed: text_box(*config, style),
            text: state.text.clone(),
        },
        W::LedStrip { config, state } => {
            let mut hold = config.peak_hold.map(|p| kit::PeakHold::new(p.rate));
            if let Some(hold) = hold.as_mut() {
                hold.push(state.level);
            }
            Renderer::LedStrip {
                strip: kit::LedStrip::new(style).leds(dim(config.leds)),
                level: state.level,
                explicit_peak: state.peak,
                hold,
                hold_rate: config.peak_hold.map_or(0.0, |p| p.rate),
                steps: Steps::default(),
            }
        }
        W::Marquee { config, state } => Renderer::Marquee {
            strip: marquee_strip(*config, style, &state.text),
            text: state.text.clone(),
            offset: 0.0,
            speed_dots_per_sec: config.speed_dots_per_sec,
        },
        W::Scope { config, state } => {
            let mut scope = kit::Scope::with_size(dim(config.cols), dim(config.rows))
                .scale(dim(config.scale))
                .persistence(config.persistence);
            // The debut batch is stamped now rather than queued, so the first
            // frame a plugin sends is on screen before the clock's first tick.
            scope.advance(&state.samples);
            Renderer::Scope {
                scope,
                pending: None,
                idle: 0,
                fades: config.persistence < KIT_MAX_PERSISTENCE,
                settle_steps: scope_settle_steps(config.persistence),
                steps: Steps::default(),
            }
        }
        W::Gauge { config, state } => {
            // `cols`/`rows` are the whole of the gauge's size story, small
            // square dials included (#931): the kit's `dial()` resolves a
            // centred face, its subdivision count, its needle width and its
            // bloom cap from the buffer alone, so a 48×48 gauge needs nothing
            // here beyond the two dimensions a 144×64 one already sent. A
            // config change rebuilds this instance (`same_config`), which is
            // what re-resolves the face when a plugin re-sizes its dial.
            let mut gauge = kit::Gauge::with_size(dim(config.cols), dim(config.rows))
                .scale(dim(config.scale))
                .sweep_deg(config.sweep_deg)
                .ticks(dim(config.divisions), dim(config.subdivisions))
                // `range` before `set_target`: the kit normalizes a target
                // against the range in force when it is set.
                .range(config.range.low, config.range.high)
                .frequency(config.frequency_hz)
                .damping(config.damping);
            gauge.set_target(state.target);
            Renderer::Gauge { gauge }
        }
        W::FlipBoard { config, state } => {
            let mut board = flip_board(*config, style);
            board.set_text(&state.text);
            Renderer::FlipBoard { board }
        }
    }))
}

/// The kit `TextBox` a [`vocab::TextBoxConfig`] describes. Split out so the
/// parity tests can build the *same* box the renderer does without re-stating
/// the builder chain (a duplicated chain is an oracle that agrees with the code
/// by construction, which is no oracle at all).
///
/// **Call it inside the widget's palette scope** ([`pins_for`] /
/// `kit::with_pins`), the way [`build`] does: `TextBox::styled` reads
/// `DisplayStyle::palette` here and now, so an ink or field pin reaches the box
/// through that scope and nothing else. The one color that does *not* is
/// [`notdef`](vocab::TextBoxConfig::notdef) — the kit's palette has no such slot
/// — so it is applied after `styled`, over whatever the skin's ghost derived.
fn text_box(config: vocab::TextBoxConfig, style: kit::DisplayStyle) -> kit::TextBox {
    let boxed = kit::TextBox::styled(style);
    let boxed = match config.width {
        vocab::TextBoxWidth::Cols(cols) => boxed.cols(dim(cols)),
        vocab::TextBoxWidth::FitPx(px) => boxed.fit_px(dim(px)),
    };
    let boxed = boxed
        .max_lines(dim(config.max_lines))
        .pad(dim(config.pad))
        .corner(dim(config.corner))
        .scale(dim(config.scale))
        .fixed_width(config.fixed_width);
    match config.notdef {
        Some(notdef) => boxed.notdef(notdef),
        None => boxed,
    }
}

/// The kit `MarqueeStrip` a [`vocab::MarqueeConfig`] + message describe.
fn marquee_strip(
    config: vocab::MarqueeConfig,
    style: kit::DisplayStyle,
    text: &str,
) -> kit::MarqueeStrip {
    kit::Marquee::new(style)
        .window_px(dim(config.window_px))
        .gap_dots(dim(config.gap_dots))
        .render(text)
}

/// The kit `FlipBoard` a [`vocab::FlipBoardConfig`] describes, blank.
fn flip_board(config: vocab::FlipBoardConfig, style: kit::DisplayStyle) -> kit::FlipBoard {
    let _ = style; // the board takes its skin at render time, not construction
    let mechanism = match config.mechanism {
        vocab::Mechanism::SplitFlap => kit::Mechanism::SplitFlap,
        vocab::Mechanism::Nixie => kit::Mechanism::Nixie,
    };
    let mut board = kit::FlipBoard::new(mechanism)
        // `cells` rebuilds the row blank, so it must precede the text.
        .cells(dim(config.cells))
        .glyph_px(dim(config.glyph_px))
        .scale(dim(config.scale));
    if let Some(secs) = config.duration_secs {
        board = board.duration_secs(secs);
    }
    if let Some(secs) = config.stagger_secs {
        board = board.stagger_secs(secs);
    }
    board
}

impl Renderer {
    /// Whether this renderer was built for `widget`'s kind. A mismatch means the
    /// plugin swapped one widget for another under the same node key, which
    /// rebuilds.
    fn matches_kind(&self, widget: &vocab::PreemWidget) -> bool {
        use vocab::PreemWidget as W;
        matches!(
            (self, widget),
            (Self::DotMatrix { .. }, W::DotMatrix { .. })
                | (Self::SevenSeg { .. }, W::SevenSeg { .. })
                | (Self::TextBox { .. }, W::TextBox { .. })
                | (Self::LedStrip { .. }, W::LedStrip { .. })
                | (Self::Marquee { .. }, W::Marquee { .. })
                | (Self::Scope { .. }, W::Scope { .. })
                | (Self::Gauge { .. }, W::Gauge { .. })
                | (Self::FlipBoard { .. }, W::FlipBoard { .. })
        )
    }

    /// Point the renderer at `widget`'s new **state**, keeping the animation it
    /// is already running. Only ever called after [`same_config`] agreed, so the
    /// config half of `widget` is the one this renderer was built from.
    fn update(&mut self, widget: &vocab::PreemWidget) {
        use vocab::PreemWidget as W;
        match (self, widget) {
            (Self::DotMatrix { text }, W::DotMatrix { state, .. }) => {
                text.clone_from(&state.text);
            }
            (Self::SevenSeg { text }, W::SevenSeg { state, .. }) => text.clone_from(&state.text),
            (Self::TextBox { text, .. }, W::TextBox { state, .. }) => text.clone_from(&state.text),
            (
                Self::LedStrip {
                    level,
                    explicit_peak,
                    hold,
                    ..
                },
                W::LedStrip { state, .. },
            ) => {
                *level = state.level;
                // The explicit peak wins for the render it arrives on but never
                // disturbs the held value — the vocabulary says so in as many
                // words, so it is deliberately not pushed into `hold`.
                *explicit_peak = state.peak;
                if let Some(hold) = hold.as_mut() {
                    hold.push(state.level);
                }
            }
            (Self::Marquee { strip, text, .. }, W::Marquee { config, state }) => {
                if *text != state.text {
                    // A new message re-rasterises the strip; the offset is
                    // deliberately *not* reset, so a ticker whose text changes
                    // mid-scroll keeps moving instead of snapping left.
                    // `window` wraps modulo the new period.
                    // Scoped like `build`'s: the strip bakes the skin's field and
                    // ghost, and while today it re-resolves the *ink* per
                    // `window()` call, that is the kit's business and not a
                    // contract this call site should depend on. Since #885's
                    // palette widening the field is a pin too, and the strip
                    // does bake *that* — so the scope has to be the widened one
                    // here, not just an ink.
                    //
                    // This is the **second** palette scope, and the only one a
                    // *state* change reaches: a new message leaves `same_config`
                    // agreeing, so `build` never runs and cannot cover it.
                    // `a_pinned_field_survives_a_marquee_text_change` is what
                    // measures it — before that test, narrowing this one line
                    // back to `with_ink` left the whole shell suite green.
                    // `a_pinned_field_survives_a_state_change_on_every_widget`
                    // is the enumeration behind "and no other arm bakes".
                    *strip = kit::with_pins(pins_for(config.style), || {
                        marquee_strip(*config, display_style(config.style), &state.text)
                    });
                    text.clone_from(&state.text);
                }
            }
            (Self::Scope { pending, idle, .. }, W::Scope { state, .. }) => {
                *pending = Some(state.samples.clone());
                *idle = 0;
            }
            (Self::Gauge { gauge }, W::Gauge { state, .. }) => gauge.set_target(state.target),
            (Self::FlipBoard { board }, W::FlipBoard { state, .. }) => board.set_text(&state.text),
            // Unreachable: `apply` rebuilds on a kind mismatch rather than
            // calling this. Dropping the update is the harmless outcome if that
            // ever stops being true.
            _ => {}
        }
    }

    /// Advance the shell-owned animation by `dt` seconds, returning `true` if
    /// anything moved (so the frame cache must be dropped and a repaint asked
    /// for).
    ///
    /// The two kit primitives that step per *call* rather than per elapsed
    /// second — `Scope::advance` and `PeakHold::decay` — are driven through a
    /// [`Steps`] accumulator so their cadence is anchored to wall-clock time.
    /// The rest take `dt` straight: the needle's spring and the flip board's
    /// clock are closed-form and frame-rate independent by construction, and the
    /// marquee's speed is stated in dots *per second*.
    fn advance(&mut self, dt: f32) -> bool {
        match self {
            Self::DotMatrix { .. } | Self::SevenSeg { .. } | Self::TextBox { .. } => false,
            Self::LedStrip {
                hold,
                steps,
                explicit_peak,
                ..
            } => {
                let owed = steps.owed(dt);
                let Some(hold) = hold.as_mut() else {
                    return false;
                };
                let before = hold.value();
                for _ in 0..owed {
                    hold.decay();
                }
                // The decay runs either way — the held value has to be current
                // the moment the plugin stops sending an explicit peak — but it
                // only *shows* when there is no explicit peak to mask it
                // (`peak_for`). Reporting movement while one is set would fan a
                // pixel-identical repaint out at 20 Hz for as long as the plugin
                // sends both, which the vocabulary explicitly blesses ("the
                // explicit peak wins for the render it arrives on and never
                // disturbs `hold`").
                if explicit_peak.is_some() {
                    return false;
                }
                changed(before, hold.value())
            }
            Self::Marquee {
                strip,
                offset,
                speed_dots_per_sec,
                ..
            } => {
                let period = strip.period();
                // A message short enough to sit still, or a parked speed — the
                // vocabulary's documented "`0.0` or non-finite parks".
                if period == 0 || !speed_dots_per_sec.is_finite() || *speed_dots_per_sec == 0.0 {
                    return false;
                }
                if !dt.is_finite() || dt <= 0.0 {
                    return false;
                }
                let before = dots(*offset, period);
                #[allow(clippy::cast_precision_loss)]
                let modulus = period as f32;
                // **The sign convention, which is a two-ended contract.** The
                // kit takes an unsigned `window(offset)` and has no signed API
                // to inherit from, so the direction lives entirely in whoever
                // integrates the offset — here, and in the SDK's raster path
                // (#884/#898). `window` reads source column `(offset + col) %
                // period`, so a *rising* offset walks the message leftwards past
                // the grid: the kit's documented "any monotonically increasing
                // frame counter loops seamlessly", and the conventional ticker
                // direction. A **positive** speed therefore raises the offset
                // and scrolls left; a **negative** speed lowers it and scrolls
                // right. The wire permits a negative speed (the cap is on
                // magnitude) and the proto does not spell the direction out, so
                // `marquee_scroll_direction_follows_the_speeds_sign` pins it
                // against the kit — if the two ends disagree, a plugin's ticker
                // reverses the day the host flips from raster to state.
                //
                // `rem_euclid` rather than `%` so the negative case wraps into
                // `0.0..period` instead of saturating an unsigned offset at zero.
                *offset = (*offset + *speed_dots_per_sec * dt).rem_euclid(modulus);
                before != dots(*offset, period)
            }
            Self::Scope {
                scope,
                pending,
                idle,
                fades,
                settle_steps,
                steps,
            } => {
                let owed = steps.owed(dt);
                let mut moved = false;
                for _ in 0..owed {
                    let batch = pending.take();
                    // Nothing pending and nothing left to fade: stop, so a
                    // settled trace doesn't keep the clock (and the reconcilers)
                    // awake.
                    if batch.is_none() && (!*fades || *idle >= *settle_steps) {
                        break;
                    }
                    // An empty batch flatlines on the axis while the existing
                    // trail keeps decaying — the kit's documented behaviour, and
                    // what lets a plugin with nothing to say simply stop.
                    scope.advance(batch.as_deref().unwrap_or(&[]));
                    *idle = if batch.is_some() {
                        0
                    } else {
                        idle.saturating_add(1)
                    };
                    moved = true;
                }
                moved
            }
            // The two seconds-based primitives are the only ones whose "did it
            // move" answer is not derived from a before/after comparison
            // (`LedStrip`, `Marquee`) or from a step count (`Scope`), so they are
            // the only ones that needed the `dt` guard spelled out. The kit
            // early-returns on a `dt` that cannot move anything
            // (`hytte-preem/src/gauge.rs`, `split_flap.rs`), so without this they
            // reported motion for a `dt` of zero — dropping the cached frame and
            // fanning a repaint out for a byte-identical frame on **every
            // scope's first tick** (the `None` baseline branch of
            // [`advance_scopes`]) and on every duplicate `frame_time` two
            // in-phase mounts hand it. Found by the #926 review, probes P2/P4.
            Self::Gauge { gauge } => {
                if gauge.is_settled() || !advances(dt) {
                    return false;
                }
                gauge.advance(dt);
                true
            }
            Self::FlipBoard { board } => {
                if board.is_settled() || !advances(dt) {
                    return false;
                }
                board.advance(dt);
                true
            }
        }
    }

    /// Whether this renderer still has animation left to run. Cheap — the
    /// animation clock calls it every tick to decide whether to do anything at
    /// all.
    fn animates(&self) -> bool {
        match self {
            Self::DotMatrix { .. } | Self::SevenSeg { .. } | Self::TextBox { .. } => false,
            // A peak dot only moves while it is above the floor, has a fall
            // rate, and is actually the value being drawn: the kit clamps a
            // negative or non-finite rate to `0.0` ("never falls"), and an
            // explicit peak masks the held one at render time — neither must
            // keep the clock awake.
            Self::LedStrip {
                hold,
                hold_rate,
                explicit_peak,
                ..
            } => {
                explicit_peak.is_none()
                    && hold_rate.is_finite()
                    && *hold_rate > 0.0
                    && hold.as_ref().is_some_and(|hold| hold.value() > 0.0)
            }
            Self::Marquee {
                strip,
                speed_dots_per_sec,
                ..
            } => strip.scrolls() && speed_dots_per_sec.is_finite() && *speed_dots_per_sec != 0.0,
            Self::Scope {
                pending,
                idle,
                fades,
                settle_steps,
                ..
            } => pending.is_some() || (*fades && *idle < *settle_steps),
            Self::Gauge { gauge } => !gauge.is_settled(),
            Self::FlipBoard { board } => !board.is_settled(),
        }
    }

    /// Rasterise the current frame in `style`.
    fn render(&self, style: kit::DisplayStyle) -> kit::Frame {
        match self {
            Self::DotMatrix { text } => kit::dot_matrix(text, style),
            Self::SevenSeg { text } => kit::seven_seg(text, style),
            // The box baked its palette at construction, so it takes no style
            // here — see `invalidate_cached_frames`.
            Self::TextBox { boxed, text } => boxed.render(text),
            Self::LedStrip {
                strip,
                level,
                explicit_peak,
                hold,
                ..
            } => strip.render(*level, peak_for(*explicit_peak, hold.as_ref())),
            Self::Marquee { strip, offset, .. } => strip.window(dots(*offset, strip.period())),
            Self::Scope { scope, .. } => scope.render(style),
            Self::Gauge { gauge } => gauge.render(style),
            Self::FlipBoard { board } => board.render(style),
        }
    }
}

/// Whether `dt` can move anything at all.
///
/// The guard [`Steps::owed`] already applies to the step-based primitives,
/// hoisted so the two that take real seconds (`Gauge`, `FlipBoard`) answer the
/// same way instead of each rolling its own — or, as they did until the #926
/// review, none at all.
fn advances(dt: f32) -> bool {
    dt.is_finite() && dt > 0.0
}

/// The peak-dot position a strip renders with: the plugin's explicit value when
/// it sent one, else the shell-held peak, else `0.0` (which the kit reads as "no
/// peak dot").
fn peak_for(explicit: Option<f32>, hold: Option<&kit::PeakHold>) -> f32 {
    explicit
        .or_else(|| hold.map(kit::PeakHold::value))
        .unwrap_or(0.0)
}

/// Exact `f32` inequality — "did this value change at all", which is precisely
/// the question a repaint gate asks. `float_cmp`'s suggested error margin would
/// be wrong in both directions here: it would swallow a slow decay's last steps
/// and report a still frame as moved.
#[allow(clippy::float_cmp)]
fn changed(before: f32, after: f32) -> bool {
    before != after
}

// ── test seams ───────────────────────────────────────────────────────────────

#[cfg(test)]
thread_local! {
    /// Makes [`build`] answer `None` for every widget, so the
    /// unsupported-widget placeholder — structurally unreachable while `build`'s
    /// match stays exhaustive over the whole vocabulary — is a *tested* path
    /// rather than a promise. It stands in for what a future `PreemWidget`
    /// variant this build predates would do. Thread-local, like the store it
    /// perturbs, so one test flipping it can't reach another.
    static FORCE_UNSUPPORTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn force_unsupported() -> bool {
    FORCE_UNSUPPORTED.get()
}

/// Run `body` with [`build`] pretending it doesn't know any widget kind.
#[cfg(test)]
pub(super) fn with_unsupported_widgets<T>(body: impl FnOnce() -> T) -> T {
    FORCE_UNSUPPORTED.set(true);
    let out = body();
    FORCE_UNSUPPORTED.set(false);
    out
}

/// `(builds, applies)` for the instance `id` keys in `scope`, or `None` if there
/// is no such instance — the lifecycle tests' window onto "updated in place"
/// versus "rebuilt". `id: None` probes the first un-id'd preem node's ordinal
/// slot.
#[cfg(test)]
pub(super) fn probe(scope: &Scope, id: Option<&str>) -> Option<(u32, u32)> {
    let key = match id {
        Some(id) => format!("id\u{1}{id}"),
        None => "#0".to_owned(),
    };
    STORE.with_borrow(|store| {
        store
            .get(scope)
            .and_then(|state| state.instances.get(&key))
            .map(|instance| (instance.builds, instance.applies))
    })
}

/// How many journal lines `what` has produced so far ([`WARN_COUNTS`]). Read as
/// a delta across the operation under test.
#[cfg(test)]
pub(super) fn warnings(what: Warned) -> u32 {
    warn_counter(what).load(Ordering::Relaxed)
}

/// How many "preem node without an id" warnings have been emitted so far
/// (#900). Read as a delta across the operation under test.
#[cfg(test)]
pub(super) fn anonymous_warnings() -> u32 {
    warnings(Warned::NoId)
}

/// How many "two preem nodes share an id" warnings have been emitted so far
/// (#918). Read as a delta across the operation under test.
#[cfg(test)]
pub(super) fn duplicate_id_warnings() -> u32 {
    warnings(Warned::DuplicateId)
}

/// How many "too many preem nodes in one tree" warnings have been emitted so
/// far (#901). Read as a delta across the operation under test.
#[cfg(test)]
pub(super) fn instance_cap_warnings() -> u32 {
    warnings(Warned::InstanceCap)
}

/// How many "too many nodes in one tree" warnings `wire_map` has emitted so far
/// (#901). Read as a delta across the operation under test.
#[cfg(test)]
pub(super) fn node_cap_warnings() -> u32 {
    warnings(Warned::NodeCap)
}

/// How many "tree nests too deep" warnings `wire_map` has emitted so far
/// (#901). Read as a delta across the operation under test.
#[cfg(test)]
pub(super) fn depth_cap_warnings() -> u32 {
    warnings(Warned::DepthCap)
}

/// How many renderer instances `scope` holds — `0` once the scope itself has
/// been swept away.
#[cfg(test)]
pub(super) fn instance_count(scope: &Scope) -> usize {
    STORE.with_borrow(|store| store.get(scope).map_or(0, |state| state.instances.len()))
}

/// A fractional dot offset as the whole-dot index `MarqueeStrip::window` takes.
/// `period == 0` (a message short enough to sit still) reads as `0`, which the
/// kit ignores anyway.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn dots(offset: f32, period: usize) -> usize {
    if period == 0 || !offset.is_finite() || offset <= 0.0 {
        return 0;
    }
    // `offset` is kept in `0.0..period` by `rem_euclid` and `period` is bounded
    // by the wire's text/gap caps, so the truncating cast is exact and cannot
    // lose a sign.
    (offset.floor() as usize) % period
}
