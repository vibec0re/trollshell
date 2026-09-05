//! `display` — preem widgets that **emit typed state** when the host speaks the
//! preem vocabulary, and CPU-rasterise when it doesn't (#884, epic #881).
//!
//! This is the plugin-facing half of the #882 vocabulary. A plugin builds the
//! same widget the same way in both worlds; the SDK decides **at render time**,
//! from the generation the host advertised in
//! [`HostMsg::Hello`](crate::proto::HostMsg::Hello), whether that widget goes on the
//! wire as a typed [`Node::Preem`](crate::proto::Node::Preem) or as a CPU-rasterised
//! [`Node::Pixels`](crate::proto::Node::Pixels).
//!
//! ```ignore
//! use hytte_plugin::display::{Gauge, StyleName};
//!
//! // once, in `init`:
//! let gauge = Gauge::new(StyleName::Vfd).range(0.0, 100.0);
//!
//! // in `update`, on every heartbeat — one code path:
//! self.gauge.set_target(reading);
//! self.gauge.advance(dt);            // ticks the needle ONLY in raster mode
//!
//! // in `view`:
//! self.gauge.node("my-gauge")        // Node::Preem, or Node::Pixels
//! ```
//!
//! # The seam: who owns the animation
//!
//! The whole point of the state vocabulary is that **the shell animates**. So
//! every wrapper here has two kinds of method, and the split *is* the seam:
//!
//! - **State setters** ([`Gauge::set_target`], [`FlipBoard::set_text`],
//!   [`Scope::push`], [`LedStrip::set_level`]) always take effect. They are what
//!   the plugin has to say, in both modes.
//! - **`advance(dt)`** ticks the plugin-side animation and is a **no-op while
//!   the host speaks preem** — the shell owns the needle spring, the phosphor
//!   decay, the flip clocks, the scroll offset and the peak-hold fall, and runs
//!   them on its own frame clock. Call it unconditionally on your heartbeat: in
//!   state mode it costs nothing and in raster mode it is exactly the tick you
//!   write today.
//!
//! That is what lets one `update`/`view` pair serve both hosts. It also means a
//! plugin talking to a preem-speaking shell does **no rasterisation at all** —
//! no [`Frame`](crate::preem::Frame) is allocated *per render*, and no
//! needle/phosphor math runs — which is the CPU half of the win, next to the
//! wire half. (Per *session* the stateful wrappers still build and hold their
//! kit renderer, phosphor buffer included, in either mode: the widget is
//! constructed before the host has advertised anything, and it has to be ready
//! if the advertisement never comes.)
//!
//! The physics **getters** follow the same rule rather than reporting the
//! stopped local animation: [`Gauge::value`] is where the shell will land the
//! needle, and [`Gauge::is_settled`] / [`FlipBoard::is_settled`] are `true`
//! while the host speaks preem, because there is no local motion left to
//! advance. A plugin that gates work on "has the needle arrived yet?" therefore
//! keeps working instead of waiting forever on a tick that is a no-op. Each has
//! an `_in` twin that states the mode explicitly, for tests.
//! [`Marquee::scroll_dots`] is the one that cannot follow — the wire carries no
//! scroll offset to read back — and its docs say so.
//!
//! # Non-finite readings
//!
//! Every **state** setter takes its value through the same rule the kit applies
//! to it, so a `NaN`/`±inf` reading cannot make the two arms disagree — and,
//! just as importantly, cannot defeat the runtime's render dedup, which
//! compares `Node`s and would find a `NaN` unequal to itself forever. See the
//! `level_reading` / `sample_reading` block below for the per-widget rules and
//! why each is the kit's output unchanged rather than an improvement on it.
//!
//! **Config** floats are the other half, and they are not sanitised here: a
//! `NaN` handed to [`FlipBoard::duration_secs`], [`Gauge::range`],
//! [`Gauge::sweep_deg`] and the rest is stored as given. It reaches the wire
//! through [`PreemWidget::clamped`], which `lower`'s state arm already calls on
//! every emission, so extending the proto's sanitiser over those fields (#899)
//! closes all of them with no change on this side — and until it lands, a
//! non-finite *config* float still defeats dedup the way a state one used to.
//! The split is deliberate: the state rules have to mirror the kit per widget
//! and so belong next to the widgets, while the config caps are the wire's
//! safety contract and belong with the wire.
//!
//! # Negotiation
//!
//! [`Manifest::new`](crate::proto::Manifest::new) stamps `vocab_max`, which is the
//! structural opt-in: a host sends [`Hello`](crate::proto::HostMsg::Hello) exactly to a
//! plugin that declared it, so a plugin too old to decode the frame never gets
//! one. On `Hello` the session records
//! [`Manifest::negotiated_vocab`](crate::proto::Manifest::negotiated_vocab) — the
//! minimum of what this plugin can speak and what the host offered — and
//! re-renders, so the mode switches within the same session.
//!
//! Against a host that never says `Hello` (an older shell, or one whose renderer
//! isn't wired yet) the recorded generation stays at the manifest's
//! *unconditional* [`vocab`](crate::proto::Manifest::vocab), which is below
//! [`PREEM_VOCAB`](crate::proto::preem::PREEM_VOCAB) — so every widget here rasterises
//! exactly as it does today. Version skew degrades to the status quo, never to
//! a blank chip and never to a refused handshake.
//!
//! The recorded generation is **thread-local**, and that is deliberate: it is
//! read on the thread [`run`](crate::run) drives the session on, and a read that
//! somehow misses it degrades to [`RenderMode::Raster`] — the fail-safe
//! direction. Nothing here can make a plugin claim a vocabulary the host did not
//! advertise.
//!
//! # Dedup, and why the wire goes quiet
//!
//! The runtime's render dedup is unchanged: it compares the whole
//! [`View`](crate::View) and sends a frame only when it differs. State nodes
//! make that comparison both **cheaper** (a handful of scalars and a short
//! string, versus a `memcmp` over a 64 KiB RGBA buffer) and **more effective**:
//! a marquee scrolling an unchanged title, a settled gauge, and a flip board
//! mid-fold all render byte-identical state, so they send *nothing*, while their
//! rasterised twins produce a different buffer on every tick.
//!
//! One deliberate exception is worth knowing about. [`Scope`]'s state is
//! *consumed*, not held (the vocabulary's own wording): each batch is stamped
//! over the decaying phosphor. Two **identical consecutive** batches therefore
//! dedup to one stamp instead of two, where the raster path would have drawn a
//! second frame — not because the new trace differs (an identical batch
//! re-lights exactly the same pixels) but because everything *else* on the
//! screen, the fading remains of earlier traces, moved on in between.
//!
//! That difference is invisible in practice, because the decay is precisely
//! what the shell already runs on its own pump: it fades the old trace whether
//! or not a frame arrives, which is why a plugin with nothing to say can simply
//! stop sending and watch the trace ghost out honestly. It is called out here
//! because it is the one place "this frame says nothing new" and "this frame
//! would have looked the same" come apart.
//!
//! # Style is a reference
//!
//! [`StyleRef`] carries a [`StyleName`] plus a semantic [`AccentRole`], so the
//! shell can re-tint every live widget when the desktop accent changes with no
//! wire traffic at all (#396) — say what a reading *means* and let the shell
//! decide what that looks like in the current theme.
//!
//! Every constructor here defaults the role to [`AccentRole::Accent`] rather
//! than to the wire default of `None`. That is a **stated** intent rather than a
//! borrowed one: the raster kit tints its ink with the host-pushed accent
//! ([`preem::set_accent`](crate::preem::set_accent), #376), and a widget that
//! names the role gets that tint from any host, whatever it resolves an unstated
//! role to. Reach for [`neutral`](Gauge::neutral) to hold a widget at the skin's
//! own ink while the desktop re-tints around it, or
//! [`accent_role`](Gauge::accent_role) for `Success`/`Warning`/`Error`.
//!
//! ## …and the escape hatch: [`ink`](Gauge::ink), [`field`](Gauge::field),
//! [`notdef`](TextBox::notdef)
//!
//! These pin literal colors, and a pinned widget is **excluded from the live
//! re-tint** — that is what pinning means, and it is the cost of taking this
//! path. Reach for them only when the color *is* the meaning and no role names
//! it (a brand color, a plugin's signature tint, a widget that has to match
//! something hand-drawn beside it); reach for a role for anything that means
//! "good", "warn" or "broken".
//!
//! Between them they cover the kit `TextBox::colors()` hatch's three slots and
//! no more: the ghost, the bloom and the CRT pass have no pin at all and stay
//! the skin's, so a pinned widget still reads as the same device. Both arms
//! honor all three — the raster arm resolves the palette locally with
//! [`preem::with_pins`](crate::preem::with_pins), so a pin looks the same
//! against a shell that speaks preem and one that does not (see [`raster_ink`]
//! for the one part of role resolution a plugin process cannot do).
//!
//! #912 shipped the ink alone and called widening it a non-goal; #884 measured
//! the two plugins that actually reach for a pin (the `pet` and `caw` speech
//! bubbles, three colors each) and Annika settled the widening on #885.
//!
//! # The escape hatch
//!
//! Nothing here removes the raster path. A plugin that wants literal pixels
//! keeps calling the kit directly and shipping
//! [`Frame::into_node`](crate::preem::Frame::into_node) — that is the
//! `Node::Pixels` escape hatch, unchanged, and it is the right answer for
//! drawing the vocabulary has no word for (the pet's and caw's hand-drawn
//! faces; their *bubbles* are `display::TextBox` since #884's palette pins).

use hytte_plugin_proto::preem::{
    DotMatrixConfig, DotMatrixState, FlipBoardConfig, FlipBoardState, GaugeConfig, GaugeRange,
    GaugeState, LedStripConfig, LedStripState, MarqueeConfig, MarqueeState, PREEM_VOCAB,
    PeakHoldConfig, PreemWidget, ScopeConfig, ScopeState, SevenSegConfig, SevenSegState,
    TextBoxConfig, TextBoxState,
};
use hytte_plugin_proto::wire::{Cls, Node};
use hytte_preem as kit;
use hytte_preem::{DisplayStyle, Frame};

pub use hytte_plugin_proto::preem::{
    AccentRole, Mechanism, Rgba, StyleName, StyleRef, TextBoxWidth,
};

// ── negotiation state ───────────────────────────────────────────────────────

thread_local! {
    /// The wire-vocabulary generation this session negotiated with the host —
    /// `min(what this plugin can speak, what the host advertised)`.
    ///
    /// Seeded per session to the manifest's *unconditional* `vocab` and raised
    /// only by a real [`HostMsg::Hello`](crate::proto::HostMsg::Hello). Thread-local
    /// rather than a process global so the value cannot leak between the
    /// runtime and a test, and so a stray read on another thread degrades to
    /// [`RenderMode::Raster`] instead of over-claiming.
    static NEGOTIATED: std::cell::Cell<u16> = const { std::cell::Cell::new(0) };
}

/// Record the generation this session negotiated. Called by the session loop at
/// (re)connect to **seed** the unconditional floor — the one place the value is
/// allowed to go down, which is what makes a reconnect degrade.
pub(crate) fn set_negotiated(vocab: u16) {
    NEGOTIATED.with(|v| v.set(vocab));
}

/// Raise the negotiated generation to `vocab`, never lower it — the `Hello`
/// path.
///
/// Monotonic on purpose (#898 review N1). A second `Hello` carrying a *lower*
/// generation would otherwise flip a live session back to `Raster` mid-flight,
/// and the wrappers' plugin-side physics have been standing still since the
/// upgrade, so the first frames after such a downgrade would jump. No host
/// sends a second `Hello` today; this makes the SDK's behaviour not depend on
/// that.
pub(crate) fn raise_negotiated(vocab: u16) {
    NEGOTIATED.with(|v| v.set(v.get().max(vocab)));
}

/// The wire-vocabulary generation this session negotiated with the host — the
/// minimum of what this plugin can speak and what the host advertised in
/// [`Hello`](crate::proto::HostMsg::Hello).
///
/// `0` before a session has started. A session that never receives a `Hello`
/// sits at the manifest's unconditional
/// [`vocab`](crate::proto::Manifest::vocab), which is below
/// [`PREEM_VOCAB`](crate::proto::preem::PREEM_VOCAB) by construction.
#[must_use]
pub fn negotiated_vocab() -> u16 {
    NEGOTIATED.with(std::cell::Cell::get)
}

/// Whether the host advertised the preem widget vocabulary (#882) — i.e.
/// whether [`negotiated_vocab`] has reached
/// [`PREEM_VOCAB`](crate::proto::preem::PREEM_VOCAB).
#[must_use]
pub fn host_speaks_preem() -> bool {
    negotiated_vocab() >= PREEM_VOCAB
}

/// How a preem widget will be put on the wire for the current session.
///
/// A plugin rarely needs to branch on this — every wrapper in this module
/// already does — but it is public so a plugin can skip work of its own (a
/// costly sample decimation that only the raster path consumes, say) and so
/// tests can pin both arms explicitly through the `node_in` methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RenderMode {
    /// CPU-rasterise with the kit and ship a
    /// [`Node::Pixels`](crate::proto::Node::Pixels) — the status quo, and what an
    /// unadvertised host gets.
    Raster,
    /// Emit a typed [`Node::Preem`](crate::proto::Node::Preem); the shell draws it and
    /// owns its animation.
    State,
}

/// The mode this session's renders will use, from the negotiated generation.
#[must_use]
pub fn render_mode() -> RenderMode {
    if host_speaks_preem() {
        RenderMode::State
    } else {
        RenderMode::Raster
    }
}

/// Test seams for plugins that assert **both** wire shapes.
///
/// A migrated plugin's `view()` produces a typed state tree or a rasterised one
/// depending on what the host advertised, and both are worth pinning — the state
/// tree because it is what the shell will draw, the raster tree because it is
/// the compat promise. Neither is reachable from a unit test without a live
/// session, so this module provides the seam.
///
/// `hytte-plugin-preem-demo` is the reference consumer.
pub mod testing {
    use super::{NEGOTIATED, RenderMode};
    use hytte_plugin_proto::preem::PREEM_VOCAB;

    /// Puts the previous generation back however `f` returns — including by
    /// panic, which is the normal way a failing assertion leaves.
    struct Restore(u16);

    impl Drop for Restore {
        fn drop(&mut self) {
            NEGOTIATED.with(|v| v.set(self.0));
        }
    }

    /// Call `f` with the render mode forced, then restore the session's real
    /// negotiated generation.
    ///
    /// **Tests only.** Forcing [`RenderMode::State`] inside a *running* plugin
    /// would emit a [`Node::Preem`](crate::proto::Node::Preem) at a host that
    /// never advertised it — which cannot decode the frame, drops the session,
    /// and leaves the plugin in exactly the #437 redial crash-loop the whole
    /// negotiation exists to prevent. That is why there is no plain setter.
    pub fn with_render_mode<T>(mode: RenderMode, f: impl FnOnce() -> T) -> T {
        let _restore = Restore(super::negotiated_vocab());
        NEGOTIATED.with(|v| {
            v.set(match mode {
                RenderMode::State => PREEM_VOCAB,
                RenderMode::Raster => 0,
            });
        });
        f()
    }
}

// ── style conversion ────────────────────────────────────────────────────────

/// The wire [`StyleName`] for a kit [`DisplayStyle`] — the migration helper for
/// a plugin that already rotates `DisplayStyle` values.
#[must_use]
pub fn style_name(style: DisplayStyle) -> StyleName {
    match style {
        DisplayStyle::Vfd => StyleName::Vfd,
        DisplayStyle::Lcd => StyleName::Lcd,
        DisplayStyle::Oled => StyleName::Oled,
        DisplayStyle::Crt => StyleName::Crt,
    }
}

/// The kit [`DisplayStyle`] for a wire [`StyleName`] — the inverse of
/// [`style_name`], and what the raster path resolves a [`StyleRef`] through.
#[must_use]
pub fn display_style(name: StyleName) -> DisplayStyle {
    match name {
        StyleName::Vfd => DisplayStyle::Vfd,
        StyleName::Lcd => DisplayStyle::Lcd,
        StyleName::Oled => DisplayStyle::Oled,
        StyleName::Crt => DisplayStyle::Crt,
    }
}

/// A [`StyleRef`] carrying the SDK's default semantic role — see the module
/// docs on why that is [`AccentRole::Accent`] rather than the wire default.
fn style_ref(style: StyleName) -> StyleRef {
    StyleRef::new(style).with_accent(AccentRole::Accent)
}

/// The kit ink the **raster** arm renders a [`StyleRef`] with — as much of the
/// shell's resolution as a plugin process can honestly do.
///
/// A pinned [`StyleRef::ink`] and [`AccentRole::Neutral`] are both fully
/// resolvable here: one is a literal color, the other is "no tint at all", and
/// neither needs a theme. So the raster arm honors both, and a widget that pins
/// its ink looks the same against an old shell as against a new one — which is
/// the whole point of the seam ([`lower`]).
///
/// `Success`/`Warning`/`Error` are the ones that cannot be: `@success_color`
/// lives in the shell's GTK theme, which a plugin process has no access to (and
/// the wire carries no theme). They fall back to [`kit::Ink::Default`] — the
/// session accent the host pushed — exactly as they rendered before the roles
/// existed, rather than to a locally invented approximation.
fn raster_ink(style: StyleRef) -> kit::Ink {
    match (style.ink, style.accent) {
        (Some(ink), _) => kit::Ink::Fixed(ink),
        (None, Some(AccentRole::Neutral)) => kit::Ink::Base,
        (None, _) => kit::Ink::Default,
    }
}

/// The kit palette scope the **raster** arm renders a [`StyleRef`] with:
/// [`raster_ink`]'s answer plus the pinned [`StyleRef::field`].
///
/// The field needs no theme either — it is a literal color or it is absent — so
/// the raster arm honors it in full, and a widget that pins its ground looks the
/// same against an old shell as against a new one. Same rule, same reason, as
/// the ink pin.
fn raster_pins(style: StyleRef) -> kit::Pins {
    kit::Pins {
        ink: raster_ink(style),
        field: style.field,
    }
}

// ── the shared lowering seam ────────────────────────────────────────────────

/// Lower one widget into the node the host will receive.
///
/// Both arms are lazy on purpose: in [`RenderMode::State`] no
/// [`Frame`] is ever allocated (the CPU half of the win), and in
/// [`RenderMode::Raster`] no config/state structs are cloned, so the raster path
/// costs exactly what it costs today.
///
/// The state arm runs [`PreemWidget::clamped`] before emitting. The host clamps
/// too — it must, it cannot trust a plugin — but clamping here as well means the
/// value the runtime *dedups on* is the value the shell will actually draw, so
/// two over-cap states that clamp to the same thing send one frame, not two.
///
/// The raster arm deliberately does **not** clamp. It is exactly the kit call a
/// plugin author writes by hand behind [`Frame::into_node`], and holding it to
/// that byte for byte is this seam's whole compat promise (see the
/// `the_raster_arm_is_byte_identical_*` tests) — so an
/// out-of-range config allocates locally here just as it does today, rather
/// than rendering one size against an old shell and another against a new one.
/// Nothing hostile reaches this arm: the config is the plugin's own, and it is
/// the plugin's own address space that pays for it.
///
/// It does resolve the widget's **palette** ([`raster_pins`]) around the kit
/// call, so a pinned ink or field, or a `neutral()` opt-out, survives the
/// fallback instead of being a state-mode-only feature. That is one scope in one
/// place rather than eight wrapped closures, and it is why `style` is a
/// parameter here: every caller already has it, and none of them can forget it.
/// (A `TextBox`'s `notdef` is not in the scope — no kit palette has that slot —
/// so it rides [`TextBox::kit`]'s builder chain on both arms instead.)
fn lower(
    mode: RenderMode,
    style: StyleRef,
    id: &str,
    classes: Vec<Cls>,
    widget: impl FnOnce() -> PreemWidget,
    raster: impl FnOnce() -> Frame,
) -> Node {
    match mode {
        RenderMode::State => Node::Preem {
            id: Some(id.to_owned()),
            classes,
            widget: Box::new(widget().clamped()),
        },
        RenderMode::Raster => {
            kit::with_pins(raster_pins(style), raster).into_node(Some(id), classes)
        }
    }
}

// ── non-finite readings: the kit's own rules, applied one step earlier ───────

// Every kit widget already defends itself against `NaN`/`±inf`, and each does
// it slightly differently because each has its own idea of "no reading":
// `Needle::set_target` (`gauge.rs`) **ignores** the value outright, `Scope`'s
// `sanitize` (`scope.rs`) maps any of the three to `0.0` — the axis — and
// `lit_count` / `peak_led` / `PeakHold::push` (`led_strip.rs`) clamp `±inf` to
// the ends of the scale and absorb only `NaN`. The setters below apply the
// *same* rule, **per widget**, before the value reaches `state`, for two
// reasons (#898 review R1):
//
// 1. **Parity.** The raster arm hands the value straight to the kit, which
//    sanitises it; the state arm puts it on the wire, where the shell's copy of
//    the same kit sanitises it later. Sanitising here makes both arms render the
//    same pixels from the same input — the invariant the
//    `the_raster_arm_is_byte_identical_*` tests exist to protect, extended to
//    the one input class those tests cannot otherwise reach. Each rule below is
//    chosen so the kit's output is **unchanged**, never improved.
// 2. **Dedup.** `Node` derives `PartialEq` and `NaN != NaN`, so one non-finite
//    reading in `state` makes the runtime's `view != last_view` true *forever*:
//    a `Render` every heartbeat until the plugin restarts, the exact inverse of
//    the "the wire goes quiet" property this module exists for. Dedup happens
//    plugin-side, so this is the load-bearing place for it whatever the wire
//    format clamps downstream.

/// A `0.0..=1.0` meter reading: `NaN` read as rest, everything else clamped.
///
/// Kit-identical, and note that `NaN` is the *only* special case here — unlike
/// [`sample_reading`], where the kit lumps all three non-finite values together.
/// `lit_count`, `peak_led` and `PeakHold::push` each `clamp(0.0, 1.0)` first,
/// and a clamp passes `±inf` straight through to the ends of the scale
/// (`f32::INFINITY.clamp(0.0, 1.0) == 1.0`, a **full** strip) while `NaN`
/// survives it and is then absorbed — by a saturating cast in `lit_count`, an
/// `is_nan` arm in `peak_led`, a failed `>` in `PeakHold::push` — into the same
/// answer `0.0` gives. Reading `+inf` as rest, which is what "non-finite means
/// no reading" would have done, darkens a strip the kit lights fully; the test
/// `sanitising_a_non_finite_reading_leaves_the_raster_arm_byte_identical`
/// caught exactly that.
fn level_reading(level: f32) -> f32 {
    if level.is_nan() {
        0.0
    } else {
        level.clamp(0.0, 1.0)
    }
}

/// A normalized `-1.0..=1.0` sample, with **any** non-finite value read as the
/// axis — character for character the kit's own `sanitize` (`scope.rs`), which
/// every sample already passes through inside `Scope::advance`.
///
/// Deliberately a different rule from [`level_reading`]'s, because the kit's is
/// different: the scope rejects `±inf` outright rather than clamping it to the
/// rails. Mirror each widget, never unify them.
fn sample_reading(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// The dial scale the kit will actually read on, given a stated range.
///
/// `Needle::range` (`gauge.rs`) **rejects** a degenerate, inverted or
/// non-finite range and keeps its default `0.0..=1.0` rather than dividing by a
/// zero span — so a wrapper that folded a target through `config.range`
/// unconditionally would answer on a scale the kit refused. `range(5.0, 5.0)`
/// with a target of `7.0` read `1.0` from the raster arm and `5.0` from the
/// state arm before this existed (#898 re-check).
///
/// The same guard, character for character, so the two arms resolve one scale.
fn effective_range(range: GaugeRange) -> GaugeRange {
    let GaugeRange { low, high } = range;
    if low.is_finite() && high.is_finite() && high > low {
        range
    } else {
        GaugeRange {
            low: 0.0,
            high: 1.0,
        }
    }
}

/// Ceiling on the plugin-side scroll accumulator, in dots.
///
/// A bound exists only so the `f32` accumulator stays exactly
/// integer-representable (`f32` is exact to 2^24) and [`offset_dots`]' cast can
/// never saturate; the strip wraps modulo its own period, so where the ceiling
/// sits is otherwise arbitrary. 10^7 leaves room for a plugin that states an
/// absolute offset derived from a wall clock — `preem-demo` uses
/// `2 × (unix mod 10^6)`, up to ~2×10^6 — without the SDK folding a number the
/// plugin computed exactly.
const MAX_OFFSET_DOTS: f32 = 1.0e7;

/// Convert an accumulated dot offset to the kit's `usize` window offset.
/// Non-finite and negative values park at 0; the strip wraps modulo its own
/// period, so the absolute magnitude is irrelevant beyond staying in range.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to a small positive range immediately above the cast"
)]
fn offset_dots(dots: f32) -> usize {
    if !dots.is_finite() || dots <= 0.0 {
        return 0;
    }
    dots.min(MAX_OFFSET_DOTS) as usize
}

/// The accumulator value for an absolute offset stated in whole dots.
#[allow(
    clippy::cast_precision_loss,
    reason = "bounded to MAX_OFFSET_DOTS, well inside f32's exact integer range"
)]
fn dots_offset(dots: usize) -> f32 {
    (dots as f32).min(MAX_OFFSET_DOTS)
}

/// Advance a wrapping dot accumulator by `speed * dt`, keeping it bounded.
fn advance_dots(offset: &mut f32, speed: f32, dt: f32) {
    if !speed.is_finite() || !dt.is_finite() {
        return;
    }
    let next = *offset + speed * dt;
    *offset = if next.is_finite() {
        next.rem_euclid(MAX_OFFSET_DOTS)
    } else {
        0.0
    };
}

/// Clamp a `u32` wire dimension into the `usize` the kit builders take.
fn dim(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

// ── DotMatrix ───────────────────────────────────────────────────────────────

/// A static 5×7 dot-matrix text strip.
///
/// Pure in both modes — nothing about it animates — so its whole state is the
/// `text` handed to [`node`](Self::node), which keeps it usable straight from a
/// `&self` `view()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DotMatrix {
    config: DotMatrixConfig,
}

impl DotMatrix {
    /// A dot-matrix strip in `style`.
    #[must_use]
    pub fn new(style: StyleName) -> Self {
        Self {
            config: DotMatrixConfig {
                style: style_ref(style),
            },
        }
    }

    /// Switch the skin (a **config** change: in state mode the shell rebuilds
    /// the renderer instance).
    pub fn style(&mut self, style: StyleName) {
        self.config.style.style = style;
    }

    /// Ask for a semantic ink role instead of the default [`AccentRole::Accent`].
    #[must_use]
    pub fn accent_role(mut self, role: AccentRole) -> Self {
        self.config.style.accent = Some(role);
        self
    }

    /// Pin this widget to the skin's own ink, ignoring the desktop accent.
    #[must_use]
    pub fn neutral(self) -> Self {
        self.accent_role(AccentRole::Neutral)
    }

    /// Pin an explicit color as this widget's ink, opting it **out** of the live
    /// re-tint — the escape hatch, not the path. See [`StyleRef::ink`].
    ///
    /// A **config** change, so *changing* the pin rebuilds the shell's renderer
    /// and loses the animation state it owned. Set it once; don't drive it from
    /// a reading.
    #[must_use]
    pub fn ink(mut self, ink: Rgba) -> Self {
        self.config.style.ink = Some(ink);
        self
    }

    /// Pin an explicit color as this widget's **field** — the panel background
    /// it floods before drawing anything — opting that slot out of the live
    /// re-tint the same way [`ink`](Self::ink) opts the glyphs out. See
    /// [`StyleRef::field`], and reach for a [`StyleName`] first: the field is
    /// most of the widget's area, so pinning it is the larger opt-out of the
    /// two.
    ///
    /// A **config** change, exactly like [`ink`](Self::ink): *changing* the pin
    /// rebuilds the shell's renderer and loses the animation state it owned. Set
    /// it once; don't drive it from a reading.
    #[must_use]
    pub fn field(mut self, field: Rgba) -> Self {
        self.config.style.field = Some(field);
        self
    }

    /// The node for `text`, in whichever mode this session negotiated.
    #[must_use]
    pub fn node(&self, id: &str, text: &str) -> Node {
        self.node_in(render_mode(), id, Vec::new(), text)
    }

    /// [`node`](Self::node) with CSS classes on the host widget.
    #[must_use]
    pub fn node_classed(&self, id: &str, classes: Vec<Cls>, text: &str) -> Node {
        self.node_in(render_mode(), id, classes, text)
    }

    /// [`node`](Self::node) with the mode stated explicitly — the primitive the
    /// other two call, exposed so a test can pin both arms without a session.
    #[must_use]
    pub fn node_in(&self, mode: RenderMode, id: &str, classes: Vec<Cls>, text: &str) -> Node {
        lower(
            mode,
            self.config.style,
            id,
            classes,
            || PreemWidget::DotMatrix {
                config: self.config,
                state: DotMatrixState {
                    text: text.to_owned(),
                },
            },
            || kit::dot_matrix(text, display_style(self.config.style.style)),
        )
    }
}

// ── SevenSeg ────────────────────────────────────────────────────────────────

/// A seven-segment readout. Pure, like [`DotMatrix`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SevenSeg {
    config: SevenSegConfig,
}

impl SevenSeg {
    /// A seven-segment readout in `style`.
    #[must_use]
    pub fn new(style: StyleName) -> Self {
        Self {
            config: SevenSegConfig {
                style: style_ref(style),
            },
        }
    }

    /// Switch the skin (a **config** change).
    pub fn style(&mut self, style: StyleName) {
        self.config.style.style = style;
    }

    /// Ask for a semantic ink role instead of the default [`AccentRole::Accent`].
    #[must_use]
    pub fn accent_role(mut self, role: AccentRole) -> Self {
        self.config.style.accent = Some(role);
        self
    }

    /// Pin this widget to the skin's own ink.
    #[must_use]
    pub fn neutral(self) -> Self {
        self.accent_role(AccentRole::Neutral)
    }

    /// Pin an explicit color as this widget's ink, opting it **out** of the live
    /// re-tint — the escape hatch, not the path. See [`StyleRef::ink`].
    ///
    /// A **config** change, so *changing* the pin rebuilds the shell's renderer
    /// and loses the animation state it owned. Set it once; don't drive it from
    /// a reading.
    #[must_use]
    pub fn ink(mut self, ink: Rgba) -> Self {
        self.config.style.ink = Some(ink);
        self
    }

    /// Pin an explicit color as this widget's **field** — the panel background
    /// it floods before drawing anything — opting that slot out of the live
    /// re-tint the same way [`ink`](Self::ink) opts the glyphs out. See
    /// [`StyleRef::field`], and reach for a [`StyleName`] first: the field is
    /// most of the widget's area, so pinning it is the larger opt-out of the
    /// two.
    ///
    /// A **config** change, exactly like [`ink`](Self::ink): *changing* the pin
    /// rebuilds the shell's renderer and loses the animation state it owned. Set
    /// it once; don't drive it from a reading.
    #[must_use]
    pub fn field(mut self, field: Rgba) -> Self {
        self.config.style.field = Some(field);
        self
    }

    /// The node for `text`, in whichever mode this session negotiated.
    #[must_use]
    pub fn node(&self, id: &str, text: &str) -> Node {
        self.node_in(render_mode(), id, Vec::new(), text)
    }

    /// [`node`](Self::node) with CSS classes on the host widget.
    #[must_use]
    pub fn node_classed(&self, id: &str, classes: Vec<Cls>, text: &str) -> Node {
        self.node_in(render_mode(), id, classes, text)
    }

    /// [`node`](Self::node) with the mode stated explicitly.
    #[must_use]
    pub fn node_in(&self, mode: RenderMode, id: &str, classes: Vec<Cls>, text: &str) -> Node {
        lower(
            mode,
            self.config.style,
            id,
            classes,
            || PreemWidget::SevenSeg {
                config: self.config,
                state: SevenSegState {
                    text: text.to_owned(),
                },
            },
            || kit::seven_seg(text, display_style(self.config.style.style)),
        )
    }
}

// ── TextBox ─────────────────────────────────────────────────────────────────

/// Wrapped pixel-font text on a rounded field — the "8bit textbox". Pure.
///
/// The kit's RGBA `colors()` hatch has a wire form here since #885's palette
/// widening, spread across three optional pins — [`field`](Self::field),
/// [`ink`](Self::ink), [`notdef`](Self::notdef) — but stating none of them is
/// still the path: an unpinned box takes a semantic [`AccentRole`], so a live
/// re-tint reaches it like every other widget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextBox {
    config: TextBoxConfig,
}

impl TextBox {
    /// A textbox in `style`, at the kit's defaults.
    #[must_use]
    pub fn new(style: StyleName) -> Self {
        Self {
            config: TextBoxConfig {
                style: style_ref(style),
                ..TextBoxConfig::default()
            },
        }
    }

    /// Wrap at an explicit column count.
    #[must_use]
    pub fn cols(mut self, cols: u32) -> Self {
        self.config.width = TextBoxWidth::Cols(cols);
        self
    }

    /// Wrap to the widest column count that fits a final pixel budget.
    #[must_use]
    pub fn fit_px(mut self, px: u32) -> Self {
        self.config.width = TextBoxWidth::FitPx(px);
        self
    }

    /// Hard cap on wrapped lines; overflow truncates with a trailing `…`.
    #[must_use]
    pub fn max_lines(mut self, lines: u32) -> Self {
        self.config.max_lines = lines;
        self
    }

    /// Field padding around the text block, in pre-scale pixels.
    #[must_use]
    pub fn pad(mut self, pad: u32) -> Self {
        self.config.pad = pad;
        self
    }

    /// Rounded-corner cut radius, in pre-scale pixels; `0` keeps square corners.
    #[must_use]
    pub fn corner(mut self, corner: u32) -> Self {
        self.config.corner = corner;
        self
    }

    /// Integer upscale.
    #[must_use]
    pub fn scale(mut self, scale: u32) -> Self {
        self.config.scale = scale;
        self
    }

    /// Render the full wrap width even for short text, so the box never resizes.
    #[must_use]
    pub fn fixed_width(mut self, fixed: bool) -> Self {
        self.config.fixed_width = fixed;
        self
    }

    /// Pin the color of the hollow `.notdef` box an **uncovered** char draws —
    /// the third of the kit `colors()` hatch's three slots, and the one that is
    /// not a palette slot (nothing else in the kit has a notdef). See
    /// [`TextBoxConfig::notdef`].
    ///
    /// Only worth setting when [`field`](Self::field) and [`ink`](Self::ink) are
    /// pinned too: on its own it drops one hand-picked color into a skin's
    /// palette. A **config** change, so *changing* it rebuilds the shell's
    /// renderer.
    #[must_use]
    pub fn notdef(mut self, notdef: Rgba) -> Self {
        self.config.notdef = Some(notdef);
        self
    }

    /// Switch the skin (a **config** change).
    pub fn style(&mut self, style: StyleName) {
        self.config.style.style = style;
    }

    /// Ask for a semantic ink role instead of the default [`AccentRole::Accent`].
    #[must_use]
    pub fn accent_role(mut self, role: AccentRole) -> Self {
        self.config.style.accent = Some(role);
        self
    }

    /// Pin this widget to the skin's own ink.
    #[must_use]
    pub fn neutral(self) -> Self {
        self.accent_role(AccentRole::Neutral)
    }

    /// Pin an explicit color as this widget's ink, opting it **out** of the live
    /// re-tint — the escape hatch, not the path. See [`StyleRef::ink`].
    ///
    /// A **config** change, so *changing* the pin rebuilds the shell's renderer
    /// and loses the animation state it owned. Set it once; don't drive it from
    /// a reading.
    #[must_use]
    pub fn ink(mut self, ink: Rgba) -> Self {
        self.config.style.ink = Some(ink);
        self
    }

    /// Pin an explicit color as this widget's **field** — the panel background
    /// it floods before drawing anything — opting that slot out of the live
    /// re-tint the same way [`ink`](Self::ink) opts the glyphs out. See
    /// [`StyleRef::field`], and reach for a [`StyleName`] first: the field is
    /// most of the widget's area, so pinning it is the larger opt-out of the
    /// two.
    ///
    /// A **config** change, exactly like [`ink`](Self::ink): *changing* the pin
    /// rebuilds the shell's renderer and loses the animation state it owned. Set
    /// it once; don't drive it from a reading.
    #[must_use]
    pub fn field(mut self, field: Rgba) -> Self {
        self.config.style.field = Some(field);
        self
    }

    /// The node for `text`, in whichever mode this session negotiated.
    #[must_use]
    pub fn node(&self, id: &str, text: &str) -> Node {
        self.node_in(render_mode(), id, Vec::new(), text)
    }

    /// [`node`](Self::node) with CSS classes on the host widget.
    #[must_use]
    pub fn node_classed(&self, id: &str, classes: Vec<Cls>, text: &str) -> Node {
        self.node_in(render_mode(), id, classes, text)
    }

    /// [`node`](Self::node) with the mode stated explicitly.
    #[must_use]
    pub fn node_in(&self, mode: RenderMode, id: &str, classes: Vec<Cls>, text: &str) -> Node {
        lower(
            mode,
            self.config.style,
            id,
            classes,
            || PreemWidget::TextBox {
                config: self.config,
                state: TextBoxState {
                    text: text.to_owned(),
                },
            },
            || self.kit().render(text),
        )
    }

    /// The kit builder this config describes.
    ///
    /// Call it inside the widget's palette scope (`lower`'s raster arm does):
    /// `TextBox::styled` resolves the field and the ink through
    /// `DisplayStyle::palette` at *construction*, so that is how a pin reaches
    /// them. [`notdef`](Self::notdef) is the exception — no kit palette carries
    /// one — and is applied after `styled`, over the color it derived from the
    /// skin's ghost.
    fn kit(&self) -> kit::TextBox {
        let mut b = kit::TextBox::styled(display_style(self.config.style.style));
        b = match self.config.width {
            TextBoxWidth::Cols(cols) => b.cols(dim(cols)),
            TextBoxWidth::FitPx(px) => b.fit_px(dim(px)),
        };
        b = b
            .max_lines(dim(self.config.max_lines))
            .pad(dim(self.config.pad))
            .corner(dim(self.config.corner))
            .scale(dim(self.config.scale))
            .fixed_width(self.config.fixed_width);
        match self.config.notdef {
            Some(notdef) => b.notdef(notdef),
            None => b,
        }
    }
}

// ── Marquee ─────────────────────────────────────────────────────────────────

/// A scrolling dot-matrix ticker.
///
/// The scroll offset is **shell-owned** in state mode: the wire carries the
/// message and a speed, and nothing else, so a marquee showing an unchanged
/// title sends one frame when the title changes rather than twenty a second.
/// [`advance`](Self::advance) integrates the same speed plugin-side for the
/// raster path, so both modes scroll at the same visual rate from one number.
#[derive(Clone, Debug, PartialEq)]
pub struct Marquee {
    config: MarqueeConfig,
    /// Raster-only scroll position, in dots. Untouched in state mode.
    offset: f32,
}

impl Marquee {
    /// A marquee in `style`, at the kit's defaults (192 px window, 20 dots/s).
    #[must_use]
    pub fn new(style: StyleName) -> Self {
        Self {
            config: MarqueeConfig {
                style: style_ref(style),
                ..MarqueeConfig::default()
            },
            offset: 0.0,
        }
    }

    /// Visible window width in final buffer pixels.
    #[must_use]
    pub fn window_px(mut self, px: u32) -> Self {
        self.config.window_px = px;
        self
    }

    /// The blank seam appended after the message before it loops, in dots.
    #[must_use]
    pub fn gap_dots(mut self, dots: u32) -> Self {
        self.config.gap_dots = dots;
        self
    }

    /// Scroll speed in dots per second. `0.0` parks the message.
    #[must_use]
    pub fn speed_dots_per_sec(mut self, speed: f32) -> Self {
        self.config.speed_dots_per_sec = speed;
        self
    }

    /// Switch the skin (a **config** change: in state mode this restarts the
    /// scroll from the left, since the shell rebuilds the renderer).
    pub fn style(&mut self, style: StyleName) {
        self.config.style.style = style;
    }

    /// Ask for a semantic ink role instead of the default [`AccentRole::Accent`].
    #[must_use]
    pub fn accent_role(mut self, role: AccentRole) -> Self {
        self.config.style.accent = Some(role);
        self
    }

    /// Pin this widget to the skin's own ink.
    #[must_use]
    pub fn neutral(self) -> Self {
        self.accent_role(AccentRole::Neutral)
    }

    /// Pin an explicit color as this widget's ink, opting it **out** of the live
    /// re-tint — the escape hatch, not the path. See [`StyleRef::ink`].
    ///
    /// A **config** change, so *changing* the pin rebuilds the shell's renderer
    /// and loses the animation state it owned. Set it once; don't drive it from
    /// a reading.
    #[must_use]
    pub fn ink(mut self, ink: Rgba) -> Self {
        self.config.style.ink = Some(ink);
        self
    }

    /// Pin an explicit color as this widget's **field** — the panel background
    /// it floods before drawing anything — opting that slot out of the live
    /// re-tint the same way [`ink`](Self::ink) opts the glyphs out. See
    /// [`StyleRef::field`], and reach for a [`StyleName`] first: the field is
    /// most of the widget's area, so pinning it is the larger opt-out of the
    /// two.
    ///
    /// A **config** change, exactly like [`ink`](Self::ink): *changing* the pin
    /// rebuilds the shell's renderer and loses the animation state it owned. Set
    /// it once; don't drive it from a reading.
    #[must_use]
    pub fn field(mut self, field: Rgba) -> Self {
        self.config.style.field = Some(field);
        self
    }

    /// Advance the **plugin-side** scroll by `dt` seconds at the configured
    /// speed. A no-op while the host speaks preem — the shell's pump owns the
    /// offset there — so this is safe (and correct) to call unconditionally.
    pub fn advance(&mut self, dt: f32) {
        self.advance_in(render_mode(), dt);
    }

    /// [`advance`](Self::advance) with the mode stated explicitly.
    pub fn advance_in(&mut self, mode: RenderMode, dt: f32) {
        if mode == RenderMode::Raster {
            advance_dots(&mut self.offset, self.config.speed_dots_per_sec, dt);
        }
    }

    /// Put the **plugin-side** scroll at an absolute offset in whole dots.
    ///
    /// The counterpart of [`advance`](Self::advance) for a plugin whose scroll
    /// is a pure function of a clock rather than an integration — which is what
    /// a plugin driven only by the host's ~1 Hz `Clock` snapshot has, and what
    /// `preem-demo` and `audio-widget` both do. A no-op while the host speaks
    /// preem, for `advance`'s reason: the shell owns the offset there.
    pub fn set_scroll_dots(&mut self, dots: usize) {
        self.set_scroll_dots_in(render_mode(), dots);
    }

    /// [`set_scroll_dots`](Self::set_scroll_dots) with the mode stated explicitly.
    pub fn set_scroll_dots_in(&mut self, mode: RenderMode, dots: usize) {
        if mode == RenderMode::Raster {
            self.offset = dots_offset(dots);
        }
    }

    /// The plugin-side scroll offset in whole dots — raster bookkeeping, exposed
    /// for tests and for a plugin that wants to observe its own scroll.
    ///
    /// **Stays at whatever it was left at while the host speaks preem** — `0`
    /// for a plugin that only ever called [`advance`](Self::advance) — because
    /// there is no plugin-side offset there to observe: the scroll is the
    /// shell's, integrated on its own frame clock from
    /// [`speed_dots_per_sec`](Self::speed_dots_per_sec), and the wire carries no
    /// offset for the plugin to read back. Unlike [`Gauge::value`] this one has
    /// no honest state-mode answer to substitute (#898 review R6).
    #[must_use]
    pub fn scroll_dots(&self) -> usize {
        offset_dots(self.offset)
    }

    /// The node for `text`, in whichever mode this session negotiated.
    #[must_use]
    pub fn node(&self, id: &str, text: &str) -> Node {
        self.node_in(render_mode(), id, Vec::new(), text)
    }

    /// [`node`](Self::node) with CSS classes on the host widget.
    #[must_use]
    pub fn node_classed(&self, id: &str, classes: Vec<Cls>, text: &str) -> Node {
        self.node_in(render_mode(), id, classes, text)
    }

    /// [`node`](Self::node) with the mode stated explicitly.
    #[must_use]
    pub fn node_in(&self, mode: RenderMode, id: &str, classes: Vec<Cls>, text: &str) -> Node {
        lower(
            mode,
            self.config.style,
            id,
            classes,
            || PreemWidget::Marquee {
                config: self.config,
                state: MarqueeState {
                    text: text.to_owned(),
                },
            },
            || {
                kit::Marquee::new(display_style(self.config.style.style))
                    .window_px(dim(self.config.window_px))
                    .gap_dots(dim(self.config.gap_dots))
                    .render(text)
                    .window(self.scroll_dots())
            },
        )
    }
}

// ── LedStrip ────────────────────────────────────────────────────────────────

/// A segmented level meter with an optional peak-hold dot.
///
/// With a [`peak_hold`](Self::peak_hold) declared, the fall is **shell-owned**
/// in state mode: the wire carries the level and the fall rate, and a steady
/// meter goes quiet. [`advance`](Self::advance) decays the plugin-side hold for
/// the raster path.
#[derive(Clone, Debug, PartialEq)]
pub struct LedStrip {
    config: LedStripConfig,
    state: LedStripState,
    /// Raster-only peak-hold. `None` unless [`peak_hold`](Self::peak_hold) was
    /// declared. Untouched in state mode.
    hold: Option<kit::PeakHold>,
}

impl LedStrip {
    /// A strip in `style` with the kit's 24 segments and no peak-hold.
    #[must_use]
    pub fn new(style: StyleName) -> Self {
        Self {
            config: LedStripConfig {
                style: style_ref(style),
                ..LedStripConfig::default()
            },
            state: LedStripState::default(),
            hold: None,
        }
    }

    /// Segment count.
    #[must_use]
    pub fn leds(mut self, leds: u32) -> Self {
        self.config.leds = leds;
        self
    }

    /// Declare a peak-hold dot falling by `rate` per animation tick. The shell
    /// runs it on its own pump in state mode; the plugin runs it in raster mode.
    #[must_use]
    pub fn peak_hold(mut self, rate: f32) -> Self {
        self.config.peak_hold = Some(PeakHoldConfig { rate });
        self.hold = Some(kit::PeakHold::new(rate));
        self
    }

    /// Switch the skin (a **config** change).
    pub fn style(&mut self, style: StyleName) {
        self.config.style.style = style;
    }

    /// Ask for a semantic ink role instead of the default [`AccentRole::Accent`].
    #[must_use]
    pub fn accent_role(mut self, role: AccentRole) -> Self {
        self.config.style.accent = Some(role);
        self
    }

    /// Pin this widget to the skin's own ink.
    #[must_use]
    pub fn neutral(self) -> Self {
        self.accent_role(AccentRole::Neutral)
    }

    /// Pin an explicit color as this widget's ink, opting it **out** of the live
    /// re-tint — the escape hatch, not the path. See [`StyleRef::ink`].
    ///
    /// A **config** change, so *changing* the pin rebuilds the shell's renderer
    /// and loses the animation state it owned. Set it once; don't drive it from
    /// a reading.
    #[must_use]
    pub fn ink(mut self, ink: Rgba) -> Self {
        self.config.style.ink = Some(ink);
        self
    }

    /// Pin an explicit color as this widget's **field** — the panel background
    /// it floods before drawing anything — opting that slot out of the live
    /// re-tint the same way [`ink`](Self::ink) opts the glyphs out. See
    /// [`StyleRef::field`], and reach for a [`StyleName`] first: the field is
    /// most of the widget's area, so pinning it is the larger opt-out of the
    /// two.
    ///
    /// A **config** change, exactly like [`ink`](Self::ink): *changing* the pin
    /// rebuilds the shell's renderer and loses the animation state it owned. Set
    /// it once; don't drive it from a reading.
    #[must_use]
    pub fn field(mut self, field: Rgba) -> Self {
        self.config.style.field = Some(field);
        self
    }

    /// Set the level to light, in `0.0..=1.0`. Always takes effect; also folds
    /// into the plugin-side peak-hold when one is declared and the host does not
    /// speak preem.
    ///
    /// The reading is taken through [`level_reading`] — the strip's own rule, so
    /// a non-finite level reads as rest in **both** modes rather than as a `NaN`
    /// the raster arm silently absorbs and the state arm ships forever.
    pub fn set_level(&mut self, level: f32) {
        self.set_level_in(render_mode(), level);
    }

    /// [`set_level`](Self::set_level) with the mode stated explicitly.
    pub fn set_level_in(&mut self, mode: RenderMode, level: f32) {
        let level = level_reading(level);
        self.state.level = level;
        if mode == RenderMode::Raster
            && let Some(hold) = self.hold.as_mut()
        {
            hold.push(level);
        }
    }

    /// Override the peak dot with an explicitly computed value (a true
    /// inter-frame peak the shell's per-render fold cannot see), or `None` to
    /// leave it to the declared [`peak_hold`](Self::peak_hold).
    ///
    /// A `NaN` override becomes `Some(0.0)` rather than `None`: `peak_led`
    /// draws no dot for either, so the rendered strip is unchanged, and keeping
    /// it a `Some` preserves the "explicit override wins" contract instead of
    /// silently handing the render back to the declared hold.
    pub fn set_peak(&mut self, peak: Option<f32>) {
        self.state.peak = peak.map(level_reading);
    }

    /// Decay the **plugin-side** peak-hold one tick. A no-op while the host
    /// speaks preem, and a no-op with no peak-hold declared — so, like every
    /// `advance` here, it is safe to call unconditionally.
    pub fn advance(&mut self) {
        self.advance_in(render_mode());
    }

    /// [`advance`](Self::advance) with the mode stated explicitly.
    pub fn advance_in(&mut self, mode: RenderMode) {
        if mode == RenderMode::Raster
            && let Some(hold) = self.hold.as_mut()
        {
            hold.decay();
        }
    }

    /// The node, in whichever mode this session negotiated.
    #[must_use]
    pub fn node(&self, id: &str) -> Node {
        self.node_in(render_mode(), id, Vec::new())
    }

    /// [`node`](Self::node) with CSS classes on the host widget.
    #[must_use]
    pub fn node_classed(&self, id: &str, classes: Vec<Cls>) -> Node {
        self.node_in(render_mode(), id, classes)
    }

    /// [`node`](Self::node) with the mode stated explicitly.
    #[must_use]
    pub fn node_in(&self, mode: RenderMode, id: &str, classes: Vec<Cls>) -> Node {
        lower(
            mode,
            self.config.style,
            id,
            classes,
            || PreemWidget::LedStrip {
                config: self.config,
                state: self.state,
            },
            || {
                // The explicit peak wins for this render without disturbing the
                // held value, exactly as `LedStripState::peak` documents; with
                // neither, `0.0` is the kit's "no dot".
                let peak = self
                    .state
                    .peak
                    .or_else(|| self.hold.as_ref().map(kit::PeakHold::value))
                    .unwrap_or(0.0);
                kit::LedStrip::new(display_style(self.config.style.style))
                    .leds(dim(self.config.leds))
                    .render(self.state.level, peak)
            },
        )
    }
}

// ── Scope ───────────────────────────────────────────────────────────────────

/// A glow-trace oscilloscope with phosphor persistence.
///
/// The phosphor buffer is **shell-owned** in state mode. [`push`](Self::push)
/// states the batch in both modes; it stamps the plugin-side phosphor only in
/// raster mode.
/// `PartialEq` but **not** `Eq`, which is one bound *short* of the kit's own
/// `Scope` — that one derives `Eq` because its phosphor is a `Vec<u16>`, while
/// this wrapper additionally holds the wire `ScopeState`, whose
/// `samples: Vec<f32>` cannot be. `PartialEq` is the bound a plugin model
/// actually needs, and it is here (#898 review R3).
#[derive(Clone, Debug, PartialEq)]
pub struct Scope {
    config: ScopeConfig,
    state: ScopeState,
    /// Raster-only phosphor buffer. Never advanced in state mode.
    kit: kit::Scope,
}

impl Scope {
    /// A scope in `style`, at the kit's defaults (144×48 at ×2).
    #[must_use]
    pub fn new(style: StyleName) -> Self {
        Self::with_config(ScopeConfig {
            style: style_ref(style),
            ..ScopeConfig::default()
        })
    }

    /// A scope with an explicit logical buffer size (pre-upscale).
    #[must_use]
    pub fn with_size(style: StyleName, cols: u32, rows: u32) -> Self {
        Self::with_config(ScopeConfig {
            style: style_ref(style),
            cols,
            rows,
            ..ScopeConfig::default()
        })
    }

    /// Integer upscale baked into the output.
    #[must_use]
    pub fn scale(mut self, scale: u32) -> Self {
        self.config.scale = scale;
        self.kit = Self::build(&self.config);
        self
    }

    /// Phosphor persistence: 256ths of beam intensity retained per tick.
    #[must_use]
    pub fn persistence(mut self, retained_256ths: u16) -> Self {
        self.config.persistence = retained_256ths;
        self.kit = Self::build(&self.config);
        self
    }

    /// Switch the skin. Cheap in raster mode (the kit takes the skin at render
    /// time, so the phosphor survives); a **config** change on the wire.
    pub fn style(&mut self, style: StyleName) {
        self.config.style.style = style;
    }

    /// Ask for a semantic ink role instead of the default [`AccentRole::Accent`].
    #[must_use]
    pub fn accent_role(mut self, role: AccentRole) -> Self {
        self.config.style.accent = Some(role);
        self
    }

    /// Pin this widget to the skin's own ink.
    #[must_use]
    pub fn neutral(self) -> Self {
        self.accent_role(AccentRole::Neutral)
    }

    /// Pin an explicit color as this widget's ink, opting it **out** of the live
    /// re-tint — the escape hatch, not the path. See [`StyleRef::ink`].
    ///
    /// A **config** change, so *changing* the pin rebuilds the shell's renderer
    /// and loses the animation state it owned. Set it once; don't drive it from
    /// a reading.
    #[must_use]
    pub fn ink(mut self, ink: Rgba) -> Self {
        self.config.style.ink = Some(ink);
        self
    }

    /// Pin an explicit color as this widget's **field** — the panel background
    /// it floods before drawing anything — opting that slot out of the live
    /// re-tint the same way [`ink`](Self::ink) opts the glyphs out. See
    /// [`StyleRef::field`], and reach for a [`StyleName`] first: the field is
    /// most of the widget's area, so pinning it is the larger opt-out of the
    /// two.
    ///
    /// A **config** change, exactly like [`ink`](Self::ink): *changing* the pin
    /// rebuilds the shell's renderer and loses the animation state it owned. Set
    /// it once; don't drive it from a reading.
    #[must_use]
    pub fn field(mut self, field: Rgba) -> Self {
        self.config.style.field = Some(field);
        self
    }

    /// State a fresh sample batch (a normalized `-1.0..=1.0` signal). Always
    /// recorded; stamps the plugin-side phosphor only in raster mode.
    ///
    /// Every sample is taken through [`sample_reading`], the kit's own
    /// `sanitize` — which `Scope::advance` applies to each one anyway, so the
    /// rasterised trace is byte-for-byte unchanged, while the batch that goes
    /// on the wire can no longer carry a `NaN` that compares unequal to itself.
    pub fn push(&mut self, samples: &[f32]) {
        self.push_in(render_mode(), samples);
    }

    /// [`push`](Self::push) with the mode stated explicitly.
    pub fn push_in(&mut self, mode: RenderMode, samples: &[f32]) {
        self.state.samples.clear();
        self.state
            .samples
            .extend(samples.iter().copied().map(sample_reading));
        if mode == RenderMode::Raster {
            // The sanitised batch, not the caller's: identical to the kit (it
            // re-applies `sanitize` internally, which is idempotent), and it
            // keeps the two arms reading from the one value.
            self.kit.advance(&self.state.samples);
        }
    }

    /// Drop the current batch and wipe the **plugin-side** phosphor.
    ///
    /// In state mode there is nothing local to wipe and the v2 vocabulary has no
    /// word for "clear" — but it does not need one: a parked plugin simply stops
    /// pushing batches and the shell's own decay fades the trace out, which is
    /// what real phosphor does anyway.
    ///
    /// The one mutator here that is **not** mode-gated, deliberately (#898
    /// review). Wiping the kit costs nothing while the host speaks preem, and
    /// leaving it dirty would mean a session that later degraded to raster —
    /// through a reconnect to an older shell — resumed on a phosphor from
    /// before the park, which is the stale trace `clear` exists to prevent.
    pub fn clear(&mut self) {
        self.state.samples.clear();
        self.kit.clear();
    }

    /// The node, in whichever mode this session negotiated.
    #[must_use]
    pub fn node(&self, id: &str) -> Node {
        self.node_in(render_mode(), id, Vec::new())
    }

    /// [`node`](Self::node) with CSS classes on the host widget.
    #[must_use]
    pub fn node_classed(&self, id: &str, classes: Vec<Cls>) -> Node {
        self.node_in(render_mode(), id, classes)
    }

    /// [`node`](Self::node) with the mode stated explicitly.
    #[must_use]
    pub fn node_in(&self, mode: RenderMode, id: &str, classes: Vec<Cls>) -> Node {
        lower(
            mode,
            self.config.style,
            id,
            classes,
            || PreemWidget::Scope {
                config: self.config,
                state: self.state.clone(),
            },
            || self.kit.render(display_style(self.config.style.style)),
        )
    }

    fn with_config(config: ScopeConfig) -> Self {
        Self {
            kit: Self::build(&config),
            config,
            state: ScopeState::default(),
        }
    }

    fn build(config: &ScopeConfig) -> kit::Scope {
        kit::Scope::with_size(dim(config.cols), dim(config.rows))
            .scale(dim(config.scale))
            .persistence(config.persistence)
    }
}

// ── Gauge ───────────────────────────────────────────────────────────────────

/// A needle gauge with damped-spring physics.
///
/// The needle's position and velocity are **shell-owned** in state mode: the
/// wire carries only the target, so a settled gauge sends nothing while it
/// swings. [`advance`](Self::advance) integrates the same spring plugin-side for
/// the raster path.
/// `PartialEq` like the kit's own `Gauge`, so a plugin model holding one still
/// derives it (#898 review R3). Not `Eq`: `GaugeState::target` is an `f32`.
#[derive(Clone, Debug, PartialEq)]
pub struct Gauge {
    config: GaugeConfig,
    state: GaugeState,
    /// Raster-only needle physics. Never advanced in state mode.
    kit: kit::Gauge,
}

impl Gauge {
    /// A gauge in `style`, at the kit's defaults (144×64 at ×2, `0.0..=1.0`).
    #[must_use]
    pub fn new(style: StyleName) -> Self {
        Self::with_config(GaugeConfig {
            style: style_ref(style),
            ..GaugeConfig::default()
        })
    }

    /// A gauge with an explicit logical buffer size (pre-upscale).
    #[must_use]
    pub fn with_size(style: StyleName, cols: u32, rows: u32) -> Self {
        Self::with_config(GaugeConfig {
            style: style_ref(style),
            cols,
            rows,
            ..GaugeConfig::default()
        })
    }

    /// The value scale the caller reads in.
    #[must_use]
    pub fn range(mut self, low: f32, high: f32) -> Self {
        self.config.range = GaugeRange { low, high };
        self.rebuild();
        self
    }

    /// Integer upscale baked into the output.
    #[must_use]
    pub fn scale(mut self, scale: u32) -> Self {
        self.config.scale = scale;
        self.rebuild();
        self
    }

    /// Total sweep of the scale, in degrees.
    #[must_use]
    pub fn sweep_deg(mut self, degrees: f32) -> Self {
        self.config.sweep_deg = degrees;
        self.rebuild();
        self
    }

    /// Major divisions and minor ticks per division.
    #[must_use]
    pub fn ticks(mut self, divisions: u32, subdivisions: u32) -> Self {
        self.config.divisions = divisions;
        self.config.subdivisions = subdivisions;
        self.rebuild();
        self
    }

    /// Undamped natural frequency in Hz — how fast the needle swings.
    #[must_use]
    pub fn frequency(mut self, hz: f32) -> Self {
        self.config.frequency_hz = hz;
        self.rebuild();
        self
    }

    /// Damping ratio — how much the needle overshoots.
    #[must_use]
    pub fn damping(mut self, zeta: f32) -> Self {
        self.config.damping = zeta;
        self.rebuild();
        self
    }

    /// Switch the skin. Cheap in raster mode (the kit takes the skin at render
    /// time, so the needle keeps its momentum); a **config** change on the wire.
    pub fn style(&mut self, style: StyleName) {
        self.config.style.style = style;
    }

    /// Ask for a semantic ink role instead of the default [`AccentRole::Accent`].
    #[must_use]
    pub fn accent_role(mut self, role: AccentRole) -> Self {
        self.config.style.accent = Some(role);
        self
    }

    /// Pin this widget to the skin's own ink.
    #[must_use]
    pub fn neutral(self) -> Self {
        self.accent_role(AccentRole::Neutral)
    }

    /// Pin an explicit color as this widget's ink, opting it **out** of the live
    /// re-tint — the escape hatch, not the path. See [`StyleRef::ink`].
    ///
    /// A **config** change, so *changing* the pin rebuilds the shell's renderer
    /// and loses the animation state it owned. Set it once; don't drive it from
    /// a reading.
    #[must_use]
    pub fn ink(mut self, ink: Rgba) -> Self {
        self.config.style.ink = Some(ink);
        self
    }

    /// Pin an explicit color as this widget's **field** — the panel background
    /// it floods before drawing anything — opting that slot out of the live
    /// re-tint the same way [`ink`](Self::ink) opts the glyphs out. See
    /// [`StyleRef::field`], and reach for a [`StyleName`] first: the field is
    /// most of the widget's area, so pinning it is the larger opt-out of the
    /// two.
    ///
    /// A **config** change, exactly like [`ink`](Self::ink): *changing* the pin
    /// rebuilds the shell's renderer and loses the animation state it owned. Set
    /// it once; don't drive it from a reading.
    #[must_use]
    pub fn field(mut self, field: Rgba) -> Self {
        self.config.style.field = Some(field);
        self
    }

    /// Point the needle at a new reading. Takes effect in either mode — it is
    /// the one thing the plugin has to say.
    ///
    /// A **non-finite** reading is ignored, exactly as the kit's own
    /// `Needle::set_target` ignores it (`gauge.rs`). Storing it would leave the
    /// raster arm on its last real target while the state arm shipped a `NaN`
    /// that never compares equal to itself — the two arms diverging on one
    /// input, and render dedup defeated for the rest of the session.
    pub fn set_target(&mut self, value: f32) {
        if !value.is_finite() {
            return;
        }
        self.state.target = value;
        self.kit.set_target(value);
    }

    /// Advance the **plugin-side** needle by `dt` seconds. A no-op while the
    /// host speaks preem — the shell integrates the spring against its own frame
    /// clock there.
    pub fn advance(&mut self, dt: f32) {
        self.advance_in(render_mode(), dt);
    }

    /// [`advance`](Self::advance) with the mode stated explicitly.
    pub fn advance_in(&mut self, mode: RenderMode, dt: f32) {
        if mode == RenderMode::Raster {
            self.kit.advance(dt);
        }
    }

    /// Snap the plugin-side needle to its target with no motion.
    pub fn settle(&mut self) {
        self.kit.settle();
    }

    /// The reading the needle is currently at, in the configured
    /// [`range`](Self::range).
    ///
    /// Plugin-side physics in raster mode. Once the host speaks preem it is
    /// where the **shell** will land the pointer: [`advance`](Self::advance) is
    /// a no-op there, so the local needle would otherwise sit frozen wherever
    /// the last raster tick left it — a value that is neither where the shell
    /// draws the pointer nor where it is heading.
    ///
    /// That is *not* simply [`target`](Self::target). The shell runs this same
    /// kit, and `Needle::set_target` folds a target into the dial's travel
    /// before the needle ever reaches it, so a `0.0..=100.0` gauge told to read
    /// `500.0` settles at `100.0` — which the raster arm reported all along and
    /// the state arm did not, until #898's review (R2) caught it. The state arm
    /// below is `Needle::set_target`'s normalisation followed by
    /// `Needle::value`, i.e. the kit's own two lines, so the modes cannot drift.
    /// A degenerate range is resolved through [`effective_range`] first, for the
    /// same reason: the kit **rejects** one rather than dividing by its zero
    /// span, so the raster needle is reading on the default `0.0..=1.0` scale
    /// whatever the config says, and the state arm has to read on that same
    /// scale to agree.
    #[must_use]
    pub fn value(&self) -> f32 {
        self.value_in(render_mode())
    }

    /// [`value`](Self::value) with the mode stated explicitly.
    #[must_use]
    pub fn value_in(&self, mode: RenderMode) -> f32 {
        match mode {
            RenderMode::Raster => self.kit.value(),
            RenderMode::State => {
                let GaugeRange { low, high } = effective_range(self.config.range);
                let span = high - low;
                low + ((self.state.target - low) / span).clamp(0.0, 1.0) * span
            }
        }
    }

    /// The reading the needle is heading for.
    #[must_use]
    pub fn target(&self) -> f32 {
        self.state.target
    }

    /// Whether the plugin-side needle has come to rest — i.e. whether there is
    /// any local motion left to [`advance`](Self::advance).
    ///
    /// **`true` while the host speaks preem**, always: the shell owns the
    /// spring there and the plugin has nothing left to run. Reporting the
    /// frozen local needle instead would leave a plugin that gates work on
    /// "settled yet?" waiting forever for a tick that is a no-op.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.is_settled_in(render_mode())
    }

    /// [`is_settled`](Self::is_settled) with the mode stated explicitly.
    #[must_use]
    pub fn is_settled_in(&self, mode: RenderMode) -> bool {
        mode == RenderMode::State || self.kit.is_settled()
    }

    /// The node, in whichever mode this session negotiated.
    #[must_use]
    pub fn node(&self, id: &str) -> Node {
        self.node_in(render_mode(), id, Vec::new())
    }

    /// [`node`](Self::node) with CSS classes on the host widget.
    #[must_use]
    pub fn node_classed(&self, id: &str, classes: Vec<Cls>) -> Node {
        self.node_in(render_mode(), id, classes)
    }

    /// [`node`](Self::node) with the mode stated explicitly.
    #[must_use]
    pub fn node_in(&self, mode: RenderMode, id: &str, classes: Vec<Cls>) -> Node {
        lower(
            mode,
            self.config.style,
            id,
            classes,
            || PreemWidget::Gauge {
                config: self.config,
                state: self.state,
            },
            || self.kit.render(display_style(self.config.style.style)),
        )
    }

    fn with_config(config: GaugeConfig) -> Self {
        let mut me = Self {
            kit: kit::Gauge::new(),
            config,
            state: GaugeState::default(),
        };
        me.rebuild();
        me
    }

    /// Rebuild the raster instance from the config. Only ever called from the
    /// consuming builders, i.e. before the needle has any momentum to lose.
    fn rebuild(&mut self) {
        let c = &self.config;
        self.kit = kit::Gauge::with_size(dim(c.cols), dim(c.rows))
            .scale(dim(c.scale))
            .sweep_deg(c.sweep_deg)
            .ticks(dim(c.divisions), dim(c.subdivisions))
            .frequency(c.frequency_hz)
            .damping(c.damping)
            .range(c.range.low, c.range.high);
        self.kit.set_target(self.state.target);
    }
}

// ── FlipBoard ───────────────────────────────────────────────────────────────

/// A split-flap board or nixie readout.
///
/// The per-cell flip clocks and the left-to-right stagger are **shell-owned** in
/// state mode: the wire carries the content and nothing else, so a board sends
/// one frame per change and nothing at all while the cards are in motion.
/// `PartialEq` like the kit's own `FlipBoard` (#898 review R3). Not `Eq`:
/// `FlipBoardConfig`'s `duration_secs`/`stagger_secs` are `Option<f32>`.
#[derive(Clone, Debug, PartialEq)]
pub struct FlipBoard {
    config: FlipBoardConfig,
    state: FlipBoardState,
    /// Raster-only flip clocks. Never advanced in state mode.
    kit: kit::FlipBoard,
}

impl FlipBoard {
    /// A board in `style` on `mechanism`, at the kit's defaults (8 cells).
    #[must_use]
    pub fn new(style: StyleName, mechanism: Mechanism) -> Self {
        Self::with_config(FlipBoardConfig {
            style: style_ref(style),
            mechanism,
            ..FlipBoardConfig::default()
        })
    }

    /// The board's physical width in character cells.
    #[must_use]
    pub fn cells(mut self, cells: u32) -> Self {
        self.config.cells = cells;
        self.rebuild();
        self
    }

    /// Logical pixels per font pixel.
    #[must_use]
    pub fn glyph_px(mut self, px: u32) -> Self {
        self.config.glyph_px = px;
        self.rebuild();
        self
    }

    /// Integer upscale baked into the output.
    #[must_use]
    pub fn scale(mut self, scale: u32) -> Self {
        self.config.scale = scale;
        self.rebuild();
        self
    }

    /// Per-cell transition length in seconds (the mechanism's own default when
    /// left unset).
    #[must_use]
    pub fn duration_secs(mut self, secs: f32) -> Self {
        self.config.duration_secs = Some(secs);
        self.rebuild();
        self
    }

    /// Per-cell left-to-right stagger in seconds (the mechanism's own default
    /// when left unset).
    #[must_use]
    pub fn stagger_secs(mut self, secs: f32) -> Self {
        self.config.stagger_secs = Some(secs);
        self.rebuild();
        self
    }

    /// Switch the skin. Cheap in raster mode (the kit takes the skin at render
    /// time, so the cards keep their clocks); a **config** change on the wire.
    pub fn style(&mut self, style: StyleName) {
        self.config.style.style = style;
    }

    /// Ask for a semantic ink role instead of the default [`AccentRole::Accent`].
    #[must_use]
    pub fn accent_role(mut self, role: AccentRole) -> Self {
        self.config.style.accent = Some(role);
        self
    }

    /// Pin this widget to the skin's own ink.
    #[must_use]
    pub fn neutral(self) -> Self {
        self.accent_role(AccentRole::Neutral)
    }

    /// Pin an explicit color as this widget's ink, opting it **out** of the live
    /// re-tint — the escape hatch, not the path. See [`StyleRef::ink`].
    ///
    /// A **config** change, so *changing* the pin rebuilds the shell's renderer
    /// and loses the animation state it owned. Set it once; don't drive it from
    /// a reading.
    #[must_use]
    pub fn ink(mut self, ink: Rgba) -> Self {
        self.config.style.ink = Some(ink);
        self
    }

    /// Pin an explicit color as this widget's **field** — the panel background
    /// it floods before drawing anything — opting that slot out of the live
    /// re-tint the same way [`ink`](Self::ink) opts the glyphs out. See
    /// [`StyleRef::field`], and reach for a [`StyleName`] first: the field is
    /// most of the widget's area, so pinning it is the larger opt-out of the
    /// two.
    ///
    /// A **config** change, exactly like [`ink`](Self::ink): *changing* the pin
    /// rebuilds the shell's renderer and loses the animation state it owned. Set
    /// it once; don't drive it from a reading.
    #[must_use]
    pub fn field(mut self, field: Rgba) -> Self {
        self.config.style.field = Some(field);
        self
    }

    /// Flip toward `text`. Always takes effect; re-stating the current content
    /// is inert, so unchanged cells are never disturbed.
    pub fn set_text(&mut self, text: &str) {
        text.clone_into(&mut self.state.text);
        self.kit.set_text(text);
    }

    /// Advance the **plugin-side** cards by `dt` seconds. A no-op while the host
    /// speaks preem — the shell runs the flip clocks there.
    pub fn advance(&mut self, dt: f32) {
        self.advance_in(render_mode(), dt);
    }

    /// [`advance`](Self::advance) with the mode stated explicitly.
    pub fn advance_in(&mut self, mode: RenderMode, dt: f32) {
        if mode == RenderMode::Raster {
            self.kit.advance(dt);
        }
    }

    /// Land every plugin-side card on its target immediately.
    pub fn settle(&mut self) {
        self.kit.settle();
    }

    /// The content the board is flipping toward, as the kit folds it onto the
    /// drum (uppercased, padded and truncated to the cell count).
    #[must_use]
    pub fn target(&self) -> String {
        self.kit.target()
    }

    /// Whether every plugin-side card has landed — i.e. whether there is any
    /// local motion left to [`advance`](Self::advance).
    ///
    /// **`true` while the host speaks preem**, for [`Gauge::is_settled`]'s
    /// reason: the shell runs the flip clocks there, so a board whose cards
    /// were still turning locally would never report landing again.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.is_settled_in(render_mode())
    }

    /// [`is_settled`](Self::is_settled) with the mode stated explicitly.
    #[must_use]
    pub fn is_settled_in(&self, mode: RenderMode) -> bool {
        mode == RenderMode::State || self.kit.is_settled()
    }

    /// The node, in whichever mode this session negotiated.
    #[must_use]
    pub fn node(&self, id: &str) -> Node {
        self.node_in(render_mode(), id, Vec::new())
    }

    /// [`node`](Self::node) with CSS classes on the host widget.
    #[must_use]
    pub fn node_classed(&self, id: &str, classes: Vec<Cls>) -> Node {
        self.node_in(render_mode(), id, classes)
    }

    /// [`node`](Self::node) with the mode stated explicitly.
    #[must_use]
    pub fn node_in(&self, mode: RenderMode, id: &str, classes: Vec<Cls>) -> Node {
        lower(
            mode,
            self.config.style,
            id,
            classes,
            || PreemWidget::FlipBoard {
                config: self.config,
                state: self.state.clone(),
            },
            || self.kit.render(display_style(self.config.style.style)),
        )
    }

    fn with_config(config: FlipBoardConfig) -> Self {
        let mut me = Self {
            kit: kit::FlipBoard::new(config.mechanism_kit()),
            config,
            state: FlipBoardState::default(),
        };
        me.rebuild();
        me
    }

    /// Rebuild the raster instance from the config. Only ever called from the
    /// consuming builders, i.e. before the board has any transition to lose.
    fn rebuild(&mut self) {
        let c = &self.config;
        let mut b = kit::FlipBoard::new(c.mechanism_kit())
            .cells(dim(c.cells))
            .glyph_px(dim(c.glyph_px))
            .scale(dim(c.scale));
        if let Some(secs) = c.duration_secs {
            b = b.duration_secs(secs);
        }
        if let Some(secs) = c.stagger_secs {
            b = b.stagger_secs(secs);
        }
        b.set_text(&self.state.text);
        self.kit = b;
    }
}

/// The kit mechanism for a wire [`Mechanism`]. A private extension trait keeps
/// the mapping next to the only place that needs it without adding a public
/// conversion nobody asked for.
trait MechanismKit {
    fn mechanism_kit(&self) -> kit::Mechanism;
}

impl MechanismKit for FlipBoardConfig {
    fn mechanism_kit(&self) -> kit::Mechanism {
        match self.mechanism {
            Mechanism::SplitFlap => kit::Mechanism::SplitFlap,
            Mechanism::Nixie => kit::Mechanism::Nixie,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AccentRole, DotMatrix, FlipBoard, Gauge, LedStrip, Marquee, Mechanism, RenderMode, Scope,
        SevenSeg, StyleName, StyleRef, TextBox, display_style, host_speaks_preem, negotiated_vocab,
        raster_ink, raster_pins, render_mode, set_negotiated, style_name,
    };
    use hytte_plugin_proto::preem::{MAX_TEXT_LEN, PREEM_VOCAB, PreemWidget};
    use hytte_plugin_proto::wire::Node;
    use hytte_plugin_proto::{decode, encode};
    use hytte_preem as kit;
    use hytte_preem::DisplayStyle;

    /// The widget a `Node::Preem` carries, or a panic naming what came instead —
    /// the assertion every state-mode test funnels through.
    fn preem_of(node: &Node) -> &PreemWidget {
        match node {
            Node::Preem { widget, .. } => widget,
            other => panic!("expected Node::Preem, got {other:?}"),
        }
    }

    /// Assert a node is a rasterised `Pixels` buffer honoring the host's
    /// `len == w * h * 4` invariant.
    fn assert_pixels(node: &Node) {
        match node {
            Node::Pixels {
                width,
                height,
                data,
                ..
            } => {
                assert_eq!(
                    data.len(),
                    (*width as usize) * (*height as usize) * 4,
                    "the kit's buffer invariant"
                );
                assert!(*width > 0 && *height > 0);
            }
            other => panic!("expected Node::Pixels, got {other:?}"),
        }
    }

    /// Whether two nodes differ. Used instead of `assert_ne!`/`assert_eq!`
    /// wherever the operands can be `Node::Pixels`, whose own `Debug` would dump
    /// a hundred kilobytes of RGBA into the failure output.
    fn differ(a: &Node, b: &Node) -> bool {
        a != b
    }

    /// One of every widget, rendered both ways. The table is the acceptance
    /// surface: adding a ninth widget to the vocabulary without a wrapper here
    /// leaves this list visibly short.
    fn every_widget(mode: RenderMode) -> Vec<Node> {
        let mut led = LedStrip::new(StyleName::Vfd).leds(16).peak_hold(0.02);
        led.set_level_in(mode, 0.6);
        let mut scope = Scope::new(StyleName::Crt);
        scope.push_in(mode, &[0.0, 0.5, -0.5, 0.25]);
        let mut gauge = Gauge::new(StyleName::Lcd).range(0.0, 100.0);
        gauge.set_target(42.0);
        let mut board = FlipBoard::new(StyleName::Oled, Mechanism::SplitFlap).cells(8);
        board.set_text("12:34:56");
        vec![
            DotMatrix::new(StyleName::Vfd).node_in(mode, "dm", Vec::new(), "HELLO"),
            SevenSeg::new(StyleName::Vfd).node_in(mode, "ss", Vec::new(), "12:34"),
            TextBox::new(StyleName::Lcd)
                .cols(16)
                .node_in(mode, "tb", Vec::new(), "wrapped text"),
            Marquee::new(StyleName::Vfd).node_in(mode, "mq", Vec::new(), "SCROLLING"),
            led.node_in(mode, "led", Vec::new()),
            scope.node_in(mode, "sc", Vec::new()),
            gauge.node_in(mode, "ga", Vec::new()),
            board.node_in(mode, "fb", Vec::new()),
        ]
    }

    // ── negotiation ─────────────────────────────────────────────────────────

    /// The recorded generation drives the mode, and the crossing point is
    /// exactly `PREEM_VOCAB`. Thread-local, so this cannot disturb a sibling
    /// test running in parallel.
    #[test]
    fn the_mode_follows_the_negotiated_generation() {
        assert_eq!(negotiated_vocab(), 0, "nothing recorded before a session");
        assert_eq!(render_mode(), RenderMode::Raster);
        assert!(!host_speaks_preem());

        for below in 0..PREEM_VOCAB {
            set_negotiated(below);
            assert_eq!(
                render_mode(),
                RenderMode::Raster,
                "generation {below} is below PREEM_VOCAB",
            );
        }

        set_negotiated(PREEM_VOCAB);
        assert!(host_speaks_preem());
        assert_eq!(render_mode(), RenderMode::State);

        // A future generation still speaks preem — the check is a floor.
        set_negotiated(PREEM_VOCAB + 7);
        assert_eq!(render_mode(), RenderMode::State);

        set_negotiated(0);
    }

    /// The ambient `node()` really does route through `render_mode()` — without
    /// this, every `node_in` test below would be proving nothing about the path
    /// a plugin actually calls.
    #[test]
    fn the_ambient_node_follows_the_recorded_generation() {
        let dm = DotMatrix::new(StyleName::Vfd);

        set_negotiated(0);
        assert_pixels(&dm.node("dm", "HI"));

        set_negotiated(PREEM_VOCAB);
        assert!(matches!(dm.node("dm", "HI"), Node::Preem { .. }));

        set_negotiated(0);
    }

    // ── the emit-vs-rasterise decision ──────────────────────────────────────

    /// Every widget rasterises in raster mode and emits typed state in state
    /// mode, with the id and classes preserved either way.
    #[test]
    fn every_widget_rasterises_or_emits_state_by_mode() {
        for node in every_widget(RenderMode::Raster) {
            assert_pixels(&node);
        }

        let state = every_widget(RenderMode::State);
        assert_eq!(state.len(), 8, "one wrapper per vocabulary widget");
        let kinds: Vec<&str> = state.iter().map(|n| preem_of(n).kind()).collect();
        assert_eq!(
            kinds,
            vec![
                "dot-matrix",
                "seven-seg",
                "text-box",
                "marquee",
                "led-strip",
                "scope",
                "gauge",
                "flip-board",
            ],
            "each wrapper emits its own variant",
        );
    }

    /// Ids and classes ride both arms identically — the host reconciler keys on
    /// them, so losing one in a mode switch would rebuild the widget.
    #[test]
    fn ids_and_classes_survive_both_arms() {
        let dm = DotMatrix::new(StyleName::Vfd);
        let classes = vec!["dim-label".to_owned()];
        for mode in [RenderMode::Raster, RenderMode::State] {
            let node = dm.node_in(mode, "the-id", classes.clone(), "X");
            let (id, cls) = match &node {
                Node::Pixels { id, classes, .. } | Node::Preem { id, classes, .. } => (id, classes),
                other => panic!("unexpected {other:?}"),
            };
            assert_eq!(id.as_deref(), Some("the-id"), "{mode:?}");
            assert_eq!(cls, &classes, "{mode:?}");
        }
    }

    // ── the escape hatch and the seam must not drift ─────────────────────────

    /// The raster arm is **byte-identical** to rasterising by hand with the kit
    /// and shipping [`Frame::into_node`] — the `Node::Pixels` escape hatch #884
    /// keeps, and the whole of its compat promise stated as an equation.
    ///
    /// Everything under a `lower()` raster closure is a config→kit-builder
    /// translation. A wrapper that quietly drops a knob, or defaults one
    /// differently from the kit, still produces a perfectly valid buffer:
    /// `assert_pixels` passes, `every_widget_rasterises_or_emits_state_by_mode`
    /// passes, and an old shell renders something subtly different from what the
    /// same plugin drew before it migrated. Only comparing the bytes catches it.
    ///
    /// The right-hand side is deliberately written the way a **pre-#884 plugin
    /// author** wrote it — raw `hytte_preem` calls, no wrapper internals — so
    /// the two sides are free to drift and this test is the thing that notices.
    /// Compared with `==` rather than `assert_eq!` throughout: these are
    /// `Node::Pixels`, whose `Debug` would dump the whole RGBA buffer into a
    /// failure message.
    ///
    /// Split three ways — the stateless widgets here, then the *sampled* ones
    /// (a stream of values drives them) and the *physics* ones (a simulation
    /// does) below — because each stateful widget needs its own driving
    /// sequence on both sides and one function of all eight runs well past the
    /// workspace's `too_many_lines` ceiling.
    #[test]
    fn the_raster_arm_is_byte_identical_for_the_stateless_widgets() {
        let raster = RenderMode::Raster;
        let no_cls = Vec::new;

        assert!(
            DotMatrix::new(StyleName::Vfd).node_in(raster, "dm", no_cls(), "HELLO")
                == kit::dot_matrix("HELLO", DisplayStyle::Vfd).into_node(Some("dm"), no_cls()),
            "dot matrix",
        );
        assert!(
            SevenSeg::new(StyleName::Crt).node_in(raster, "ss", no_cls(), "12:34")
                == kit::seven_seg("12:34", DisplayStyle::Crt).into_node(Some("ss"), no_cls()),
            "seven segment",
        );
        assert!(
            TextBox::new(StyleName::Lcd)
                .cols(22)
                .max_lines(2)
                .pad(4)
                .corner(3)
                .scale(2)
                .fixed_width(true)
                .node_in(raster, "tb", no_cls(), "wrapped demo copy")
                == kit::TextBox::styled(DisplayStyle::Lcd)
                    .cols(22)
                    .max_lines(2)
                    .pad(4)
                    .corner(3)
                    .scale(2)
                    .fixed_width(true)
                    .render("wrapped demo copy")
                    .into_node(Some("tb"), no_cls()),
            "text box, Cols",
        );
        // …and the other `TextBoxWidth` arm. Without it a dropped `fit_px` in
        // `TextBox::kit()` is invisible: the `Cols` case above never reaches
        // that branch (#898 review N6).
        assert!(
            TextBox::new(StyleName::Oled)
                .fit_px(120)
                .max_lines(3)
                .node_in(raster, "tb", no_cls(), "fit to a pixel budget")
                == kit::TextBox::styled(DisplayStyle::Oled)
                    .fit_px(120)
                    .max_lines(3)
                    .render("fit to a pixel budget")
                    .into_node(Some("tb"), no_cls()),
            "text box, FitPx",
        );
    }

    /// The **sampled** widgets — a marquee's dot accumulator, a level meter's
    /// peak-hold, a scope's stamped batches. Same claim as
    /// `the_raster_arm_is_byte_identical_for_the_stateless_widgets`; each side
    /// is driven through the same sequence, so a dropped knob shows up as a
    /// different buffer rather than as a compile error.
    #[test]
    fn the_raster_arm_is_byte_identical_for_the_sampled_widgets() {
        const MSG: &str = "SCROLLING MARQUEE ~ DOT-MATRIX PIXEL TICKER ~ ";
        let raster = RenderMode::Raster;
        let no_cls = Vec::new;

        // — the marquee: the SDK's dot accumulator must land on the same whole
        //   window offset the author passed to `MarqueeStrip::window` by hand —
        let mut mq = Marquee::new(StyleName::Vfd)
            .window_px(268)
            .gap_dots(4)
            .speed_dots_per_sec(20.0);
        mq.advance_in(raster, 0.5);
        mq.advance_in(raster, 0.5);
        assert_eq!(mq.scroll_dots(), 20, "one second at 20 dots/s");
        assert!(
            mq.node_in(raster, "mq", no_cls(), MSG)
                == kit::Marquee::new(DisplayStyle::Vfd)
                    .window_px(268)
                    .gap_dots(4)
                    .render(MSG)
                    .window(20)
                    .into_node(Some("mq"), no_cls()),
            "marquee",
        );

        // — the led strip: the same push/decay sequence through the same
        //   `PeakHold`, and the dot the strip renders comes from it —
        let mut led = LedStrip::new(StyleName::Oled).leds(16).peak_hold(0.02);
        let mut hold = kit::PeakHold::new(0.02);
        for level in [0.8_f32, 0.3] {
            led.set_level_in(raster, level);
            led.advance_in(raster);
            hold.push(level);
            hold.decay();
        }
        assert!(
            led.node_in(raster, "led", no_cls())
                == kit::LedStrip::new(DisplayStyle::Oled)
                    .leds(16)
                    .render(0.3, hold.value())
                    .into_node(Some("led"), no_cls()),
            "led strip",
        );

        // — the scope: two stamped batches over a live phosphor —
        let batches: [&[f32]; 2] = [&[0.0, 0.5, -0.5, 0.25], &[0.9, -0.9, 0.1, 0.0]];
        let mut sc = Scope::with_size(StyleName::Crt, 96, 32)
            .scale(2)
            .persistence(200);
        let mut sc_kit = kit::Scope::with_size(96, 32).scale(2).persistence(200);
        for batch in batches {
            sc.push_in(raster, batch);
            sc_kit.advance(batch);
        }
        assert!(
            sc.node_in(raster, "sc", no_cls())
                == sc_kit
                    .render(DisplayStyle::Crt)
                    .into_node(Some("sc"), no_cls()),
            "scope",
        );
    }

    /// The **physics** widgets — a damped-spring needle and per-cell flip
    /// clocks. Same claim again; both are caught mid-motion, so the frequency,
    /// damping, sweep, duration and stagger all have to survive the config→kit
    /// translation to land on the same pixels.
    #[test]
    fn the_raster_arm_is_byte_identical_for_the_physics_widgets() {
        let raster = RenderMode::Raster;
        let no_cls = Vec::new;

        // — the gauge: the spring has to be integrated identically —
        let mut ga = Gauge::with_size(StyleName::Lcd, 120, 60)
            .scale(2)
            .range(0.0, 100.0)
            .sweep_deg(240.0)
            .ticks(5, 4)
            .frequency(2.0)
            .damping(0.5);
        let mut ga_kit = kit::Gauge::with_size(120, 60)
            .scale(2)
            .sweep_deg(240.0)
            .ticks(5, 4)
            .frequency(2.0)
            .damping(0.5)
            .range(0.0, 100.0);
        ga.set_target(42.0);
        ga_kit.set_target(42.0);
        for _ in 0..3 {
            ga.advance_in(raster, 0.1);
            ga_kit.advance(0.1);
        }
        assert!(
            ga.node_in(raster, "ga", no_cls())
                == ga_kit
                    .render(DisplayStyle::Lcd)
                    .into_node(Some("ga"), no_cls()),
            "gauge",
        );

        // — the flip board: per-cell clocks mid-fold, so the stagger and the
        //   duration have to survive the translation too —
        let mut fb = FlipBoard::new(StyleName::Oled, Mechanism::SplitFlap)
            .cells(6)
            .glyph_px(2)
            .scale(2)
            .duration_secs(0.4)
            .stagger_secs(0.05);
        let mut fb_kit = kit::FlipBoard::new(kit::Mechanism::SplitFlap)
            .cells(6)
            .glyph_px(2)
            .scale(2)
            .duration_secs(0.4)
            .stagger_secs(0.05);
        fb.set_text("12:34");
        fb_kit.set_text("12:34");
        for _ in 0..2 {
            fb.advance_in(raster, 0.1);
            fb_kit.advance(0.1);
        }
        assert!(
            !fb.is_settled_in(raster),
            "caught mid-fold, or the comparison proves nothing about the clocks",
        );
        assert!(
            fb.node_in(raster, "fb", no_cls())
                == fb_kit
                    .render(DisplayStyle::Oled)
                    .into_node(Some("fb"), no_cls()),
            "flip board",
        );
    }

    // ── non-finite readings ─────────────────────────────────────────────────

    /// The three non-finite values, and a heartbeat count long enough that a
    /// per-beat frame would be unmistakable.
    const NON_FINITE: [f32; 3] = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];
    const BEATS: usize = 5;

    /// A non-finite reading must not defeat render dedup (#898 review R1).
    ///
    /// The runtime ships a frame when `view != last_view`, [`Node`] derives
    /// `PartialEq`, and `NaN != NaN` — so one `NaN` reaching `state` meant one
    /// `Render` per heartbeat for the rest of the session, forever, which is
    /// the exact inverse of the property this module exists for. A plugin gets
    /// there by ordinary arithmetic: `sum / n` with `n == 0`, a `-inf` dB fold,
    /// an unread sensor.
    ///
    /// Asserted the way the runtime would see it: the node from the first
    /// heartbeat, then [`BEATS`] more that re-state the same bad reading, all
    /// compared for equality. Under the old code every one of them differed.
    #[test]
    fn a_non_finite_reading_does_not_defeat_dedup_in_state_mode() {
        let state = RenderMode::State;
        for bad in NON_FINITE {
            let mut gauge = Gauge::new(StyleName::Vfd).range(0.0, 100.0);
            gauge.set_target(bad);
            let first = gauge.node_in(state, "ga", Vec::new());
            for beat in 0..BEATS {
                gauge.set_target(bad);
                gauge.advance(0.1);
                assert_eq!(
                    gauge.node_in(state, "ga", Vec::new()),
                    first,
                    "gauge, {bad}, beat {beat}",
                );
            }

            let mut led = LedStrip::new(StyleName::Vfd).leds(16).peak_hold(0.02);
            led.set_level_in(state, bad);
            led.set_peak(Some(bad));
            let first = led.node_in(state, "led", Vec::new());
            for beat in 0..BEATS {
                led.set_level_in(state, bad);
                led.set_peak(Some(bad));
                led.advance_in(state);
                assert_eq!(
                    led.node_in(state, "led", Vec::new()),
                    first,
                    "led strip, {bad}, beat {beat}",
                );
            }

            let batch = [bad, 0.5, bad, -0.25];
            let mut scope = Scope::new(StyleName::Vfd);
            scope.push_in(state, &batch);
            let first = scope.node_in(state, "sc", Vec::new());
            for beat in 0..BEATS {
                scope.push_in(state, &batch);
                assert_eq!(
                    scope.node_in(state, "sc", Vec::new()),
                    first,
                    "scope, {bad}, beat {beat}",
                );
            }
        }
    }

    /// The same claim for every non-finite float a **config** builder can carry
    /// — the eight holes #898's review left open and #899's proto clamp closed
    /// (#884's first follow-up item, pinned here rather than left as a claim).
    ///
    /// The test above drives *state* floats (a reading, a level, a sample
    /// batch), each of which the SDK sanitises at its own setter. A config float
    /// has no setter rule: `sweep_deg(NAN)` is stored as written, travels in
    /// `config`, and `PreemWidget` derives `PartialEq` — so before the clamp,
    /// a widget configured with one was never equal to itself and shipped a
    /// `Render` per heartbeat forever, exactly like the state case.
    ///
    /// Nothing in the SDK closes it. The single [`PreemWidget::clamped`] call in
    /// [`lower`]'s state arm does, by folding every non-finite onto a canonical
    /// finite stand-in before the value is compared.
    ///
    /// **Falsified** by dropping `.clamped()` from `lower`'s state arm: every
    /// row here goes red (and nothing else in this module's suite does, which is
    /// what makes it *this* test's claim).
    #[test]
    fn a_non_finite_config_float_does_not_defeat_dedup_in_state_mode() {
        let state = RenderMode::State;
        let no_cls = Vec::new;
        for bad in NON_FINITE {
            // Each entry is a widget whose *only* non-finite value is in config;
            // the closure re-renders it with unchanged state, which is what the
            // runtime's dedup compares.
            let marquee = Marquee::new(StyleName::Vfd).speed_dots_per_sec(bad);
            let leds = LedStrip::new(StyleName::Vfd).leds(16).peak_hold(bad);
            let range = Gauge::new(StyleName::Vfd).range(bad, bad);
            let sweep = Gauge::new(StyleName::Vfd).sweep_deg(bad);
            let freq = Gauge::new(StyleName::Vfd).frequency(bad);
            let damping = Gauge::new(StyleName::Vfd).damping(bad);
            let duration = FlipBoard::new(StyleName::Vfd, Mechanism::SplitFlap).duration_secs(bad);
            let stagger = FlipBoard::new(StyleName::Vfd, Mechanism::SplitFlap).stagger_secs(bad);

            let rows: [(&str, &dyn Fn() -> Node); 8] = [
                ("marquee speed", &|| {
                    marquee.node_in(state, "mq", no_cls(), "TICKER")
                }),
                ("peak-hold rate", &|| leds.node_in(state, "led", no_cls())),
                ("gauge range", &|| range.node_in(state, "ga", no_cls())),
                ("gauge sweep", &|| sweep.node_in(state, "ga", no_cls())),
                ("gauge frequency", &|| freq.node_in(state, "ga", no_cls())),
                ("gauge damping", &|| damping.node_in(state, "ga", no_cls())),
                ("flip duration", &|| duration.node_in(state, "fb", no_cls())),
                ("flip stagger", &|| stagger.node_in(state, "fb", no_cls())),
            ];

            for (what, render) in rows {
                let first = render();
                for beat in 0..BEATS {
                    assert_eq!(render(), first, "{what}, {bad}, beat {beat}");
                }
            }
        }
    }

    /// …and sanitising must not have moved a single pixel: the raster arm fed a
    /// non-finite reading is still byte-identical to the raw kit fed the same
    /// one (#898 review R1).
    ///
    /// This is the half that makes the fix a *parity* fix rather than a
    /// behaviour change. Each rule was picked to be the kit's existing answer
    /// restated: `Needle::set_target` ignores a non-finite target, `lit_count`
    /// and `peak_led` read one as an unlit strip with no dot (identically to
    /// `0.0`), and `Scope`'s `sanitize` already maps one to the axis. The
    /// `the_raster_arm_is_byte_identical_*` tests cannot reach this input class
    /// because they drive only finite values.
    #[test]
    fn sanitising_a_non_finite_reading_leaves_the_raster_arm_byte_identical() {
        let raster = RenderMode::Raster;
        let no_cls = Vec::new;
        for bad in NON_FINITE {
            // Gauge: a real reading, then a bad one, then a settle — the kit
            // must have ignored the bad one on both sides.
            let mut ga = Gauge::with_size(StyleName::Lcd, 120, 60).range(0.0, 100.0);
            let mut ga_kit = kit::Gauge::with_size(120, 60).range(0.0, 100.0);
            ga.set_target(42.0);
            ga_kit.set_target(42.0);
            ga.set_target(bad);
            ga_kit.set_target(bad);
            for _ in 0..BEATS {
                ga.advance_in(raster, 0.1);
                ga_kit.advance(0.1);
            }
            assert!(
                ga.node_in(raster, "ga", no_cls())
                    == ga_kit
                        .render(DisplayStyle::Lcd)
                        .into_node(Some("ga"), no_cls()),
                "gauge, {bad}",
            );

            // Led strip: the level *and* the explicit peak override.
            let mut led = LedStrip::new(StyleName::Oled).leds(16).peak_hold(0.02);
            let mut hold = kit::PeakHold::new(0.02);
            led.set_level_in(raster, bad);
            led.advance_in(raster);
            hold.push(bad);
            hold.decay();
            led.set_peak(Some(bad));
            assert!(
                led.node_in(raster, "led", no_cls())
                    == kit::LedStrip::new(DisplayStyle::Oled)
                        .leds(16)
                        .render(bad, bad)
                        .into_node(Some("led"), no_cls()),
                "led strip, {bad}",
            );

            // Scope: a batch with bad samples either side of good ones.
            let batch = [bad, 0.5, bad, -0.25];
            let mut sc = Scope::with_size(StyleName::Crt, 96, 32);
            let mut sc_kit = kit::Scope::with_size(96, 32).scale(2);
            for _ in 0..BEATS {
                sc.push_in(raster, &batch);
                sc_kit.advance(&batch);
            }
            assert!(
                sc.node_in(raster, "sc", no_cls())
                    == sc_kit
                        .render(DisplayStyle::Crt)
                        .into_node(Some("sc"), no_cls()),
                "scope, {bad}",
            );
        }
    }

    /// `Gauge::value()` reports the same reading in both modes (#898 review R2).
    ///
    /// The state arm used to return the raw `state.target`, so a `0.0..=100.0`
    /// gauge told to read `500.0` answered `500` against a preem shell and
    /// `100` against an old one — from identical plugin code and identical
    /// input. The shell runs this same kit, and `Needle::set_target` folds a
    /// target into the dial's travel before the needle reaches it, so `100` was
    /// right both times.
    ///
    /// A tolerance rather than exact equality: the raster arm arrives at full
    /// scale through the needle's own float arithmetic. It is a thousandth of
    /// one division, where the bug was five times full scale.
    #[test]
    fn the_gauge_reads_the_same_in_both_modes() {
        for target in [42.0_f32, 500.0, -80.0, 0.0, 100.0] {
            let mut ga = Gauge::new(StyleName::Vfd).range(0.0, 100.0);
            ga.set_target(target);
            ga.settle();
            let (raster, state) = (
                ga.value_in(RenderMode::Raster),
                ga.value_in(RenderMode::State),
            );
            assert!(
                (raster - state).abs() < 1e-3,
                "target {target}: raster={raster} state={state}",
            );
            assert!(
                (0.0..=100.0).contains(&state),
                "target {target}: {state} is off the dial",
            );
        }

        // A non-finite target is ignored (the kit's rule), so both modes keep
        // reporting the last real reading rather than one of them going NaN.
        for bad in NON_FINITE {
            let mut ga = Gauge::new(StyleName::Vfd).range(0.0, 100.0);
            ga.set_target(37.0);
            ga.settle();
            ga.set_target(bad);
            for mode in [RenderMode::Raster, RenderMode::State] {
                let v = ga.value_in(mode);
                assert!(v.is_finite(), "{mode:?} went non-finite on {bad}");
                assert!((v - 37.0).abs() < 1e-3, "{mode:?} moved to {v} on {bad}");
            }
            assert!(
                (ga.target() - 37.0).abs() < f32::EPSILON,
                "…and so did target()"
            );
        }

        // A range the kit *rejects* — degenerate, inverted, non-finite — is the
        // case the two arms parted company on (#898 re-check): `Needle::range`
        // keeps its default `0.0..=1.0` rather than dividing by a zero span, so
        // the raster needle reads on that scale while the state arm was reading
        // on one the kit never accepted. `range(5.0, 5.0)` with a target of
        // `7.0` gave raster 1.0, state 5.0.
        for (low, high) in [
            (5.0_f32, 5.0_f32),
            (100.0, 0.0),
            (f32::NAN, 1.0),
            (0.0, f32::INFINITY),
        ] {
            let mut ga = Gauge::new(StyleName::Vfd).range(low, high);
            ga.set_target(7.0);
            ga.settle();
            let (raster, state) = (
                ga.value_in(RenderMode::Raster),
                ga.value_in(RenderMode::State),
            );
            assert!(
                (raster - state).abs() < 1e-3,
                "range({low}, {high}): raster={raster} state={state}",
            );
            assert!(
                state.is_finite(),
                "range({low}, {high}) must not divide by a zero span",
            );
        }
    }

    /// The three wrappers that had lost it derive `PartialEq` again (#898 review
    /// R3), so a plugin model holding one still derives its own.
    ///
    /// Stated as a use, not a claim: this function would not compile if any of
    /// the three lacked the bound, and `preem-demo`'s model — which had to drop
    /// `PartialEq` for exactly this reason — carries it again.
    #[test]
    fn the_stateful_wrappers_compare_like_the_kit_types_they_replace() {
        #[derive(Clone, Debug, PartialEq)]
        struct Model {
            scope: Scope,
            gauge: Gauge,
            board: FlipBoard,
        }
        let model = Model {
            scope: Scope::new(StyleName::Vfd),
            gauge: Gauge::new(StyleName::Vfd),
            board: FlipBoard::new(StyleName::Vfd, Mechanism::Nixie),
        };
        let mut moved = model.clone();
        assert_eq!(moved, model, "a fresh clone compares equal");
        moved.gauge.set_target(1.0);
        assert_ne!(moved, model, "…and a stated reading makes it differ");
    }

    /// The SDK defaults the semantic role to `Accent` so a state-mode widget
    /// keeps the tint the raster kit already applies from `HostMsg::Accent`
    /// (#376) — see the module docs. `neutral()` opts back out.
    #[test]
    fn the_default_ink_role_preserves_the_desktop_accent() {
        let node = DotMatrix::new(StyleName::Vfd).node_in(RenderMode::State, "dm", Vec::new(), "X");
        assert_eq!(preem_of(&node).style().accent, Some(AccentRole::Accent));

        let node = DotMatrix::new(StyleName::Vfd).neutral().node_in(
            RenderMode::State,
            "dm",
            Vec::new(),
            "X",
        );
        assert_eq!(preem_of(&node).style().accent, Some(AccentRole::Neutral));
    }

    // ── the explicit ink pin (#885) ─────────────────────────────────────────

    /// A widget nobody pinned emits **exactly** the style reference it emitted
    /// before the field existed — same skin, same role, no pin — so #885 costs
    /// an unpinned plugin nothing at all, on the wire or in behavior.
    ///
    /// **Falsified** by defaulting `style_ref` to a pinned ink: the `ink: None`
    /// assertion goes red for every widget.
    #[test]
    fn an_unpinned_widget_emits_the_style_ref_it_always_did() {
        for node in every_widget(RenderMode::State) {
            let style = preem_of(&node).style();
            assert_eq!(
                style.ink,
                None,
                "{}: nothing pinned it, so nothing may be pinned",
                preem_of(&node).kind()
            );
            assert_eq!(
                style,
                StyleRef::new(style.style).with_accent(AccentRole::Accent),
                "{}: the SDK's unpinned reference is skin + Accent, unchanged",
                preem_of(&node).kind()
            );
        }
    }

    /// `.ink(…)` puts that exact color on the wire and it survives the codec —
    /// the state half of the pin, which is what makes the shell exclude the
    /// widget from the live re-tint.
    #[test]
    fn a_pinned_ink_reaches_the_host_through_the_state_arm() {
        let violet = [0x9b, 0x59, 0xb6, 0xff];
        let node = DotMatrix::new(StyleName::Vfd).ink(violet).node_in(
            RenderMode::State,
            "dm",
            Vec::new(),
            "X",
        );
        assert_eq!(preem_of(&node).style().ink, Some(violet));
        assert_eq!(
            preem_of(&node).style().accent,
            Some(AccentRole::Accent),
            "a pin does not clear the role it overrides — the host decides which wins"
        );

        let back: Node = decode(&encode(&node)).expect("a pinned node decodes");
        assert_eq!(preem_of(&back).style().ink, Some(violet));
    }

    /// [`raster_ink`] is the whole of what a *plugin process* can resolve: a
    /// literal color and "no tint" need no theme, the three status roles do, and
    /// an unstated role is the session accent. Stated as a table so a collapsed
    /// arm is a failure rather than a silent behavior change.
    ///
    /// **Falsified** by returning `Ink::Default` for `Neutral`: the neutral row
    /// goes red — which is the whole reason the raster arm gained a scope.
    #[test]
    fn the_raster_arm_resolves_what_a_plugin_can_and_defers_what_it_cannot() {
        let violet = [0x9b, 0x59, 0xb6, 0xff];
        let vfd = || StyleRef::new(StyleName::Vfd);
        assert_eq!(raster_ink(vfd()), kit::Ink::Default, "no role stated");
        assert_eq!(
            raster_ink(vfd().with_accent(AccentRole::Accent)),
            kit::Ink::Default,
            "the accent is what the kit already applies"
        );
        assert_eq!(
            raster_ink(vfd().with_accent(AccentRole::Neutral)),
            kit::Ink::Base,
            "neutral needs no theme, so the raster arm honors it"
        );
        for role in [AccentRole::Success, AccentRole::Warning, AccentRole::Error] {
            assert_eq!(
                raster_ink(vfd().with_accent(role)),
                kit::Ink::Default,
                "{role:?} lives in the shell's theme; a plugin defers to the accent"
            );
        }
        // A pin beats every one of them — including the role it sits next to.
        assert_eq!(
            raster_ink(vfd().with_ink(violet)),
            kit::Ink::Fixed(violet),
            "a pin is resolvable anywhere"
        );
        assert_eq!(
            raster_ink(vfd().with_accent(AccentRole::Neutral).with_ink(violet)),
            kit::Ink::Fixed(violet),
            "…and outranks a role"
        );
    }

    /// The #898 parity rule, extended to the pin: the raster arm is byte-for-byte
    /// the kit call a plugin author would write by hand — now including the ink
    /// scope. Pinning is **not** a state-mode-only feature, so a widget looks the
    /// same against a shell that speaks preem and one that does not.
    ///
    /// The second assertion is what keeps the first from being vacuous: without
    /// the scope in `lower`, the pinned render is simply the unpinned one and
    /// both sides of an "== kit call without a scope" comparison would agree.
    ///
    /// **Falsified** by dropping the `kit::with_ink` wrapper from `lower`'s
    /// raster arm: parity goes red for both widgets.
    #[test]
    fn the_raster_arm_is_byte_identical_for_a_pinned_widget() {
        let violet = [0x9b, 0x59, 0xb6, 0xff];
        let raster = RenderMode::Raster;
        let no_cls = Vec::new;

        let pinned =
            DotMatrix::new(StyleName::Vfd)
                .ink(violet)
                .node_in(raster, "dm", no_cls(), "HELLO");
        assert!(
            pinned
                == kit::with_ink(kit::Ink::Fixed(violet), || kit::dot_matrix(
                    "HELLO",
                    DisplayStyle::Vfd
                ))
                .into_node(Some("dm"), no_cls()),
            "dot matrix, pinned",
        );
        assert!(
            pinned != DotMatrix::new(StyleName::Vfd).node_in(raster, "dm", no_cls(), "HELLO"),
            "a pinned raster must differ from the unpinned one, or the parity above proves nothing",
        );

        // …and a widget that bakes its palette at construction rather than at
        // render, which is the arm a scope around only the kit call would miss.
        let pinned = TextBox::new(StyleName::Lcd).ink(violet).cols(12).node_in(
            raster,
            "tb",
            no_cls(),
            "pinned",
        );
        assert!(
            pinned
                == kit::with_ink(kit::Ink::Fixed(violet), || kit::TextBox::styled(
                    DisplayStyle::Lcd
                )
                .cols(12)
                .render("pinned"))
                .into_node(Some("tb"), no_cls()),
            "text box, pinned",
        );
        assert!(
            pinned
                != TextBox::new(StyleName::Lcd)
                    .cols(12)
                    .node_in(raster, "tb", no_cls(), "pinned"),
            "text box: a pin must change the pixels",
        );
    }

    // ── the palette widening: field + notdef (#885, for #884's two bubbles) ──

    /// [`raster_pins`] carries the field pin beside [`raster_ink`]'s answer, and
    /// carries it under *every* role — a role decides the ink and has nothing to
    /// say about the ground, so the two must not interfere.
    ///
    /// **Falsified** by returning `field: None` from `raster_pins`: every
    /// "carries the field" row goes red.
    #[test]
    fn the_raster_arm_carries_a_pinned_field_under_every_role() {
        let lilac = [0x3a, 0x22, 0x50, 0xff];
        let violet = [0x9b, 0x59, 0xb6, 0xff];
        let vfd = || StyleRef::new(StyleName::Vfd);

        assert_eq!(
            raster_pins(vfd()).field,
            None,
            "nothing pinned, nothing set"
        );
        assert_eq!(
            raster_pins(vfd().with_field(lilac)).field,
            Some(lilac),
            "a bare field pin reaches the scope"
        );
        for role in [
            AccentRole::Accent,
            AccentRole::Neutral,
            AccentRole::Success,
            AccentRole::Warning,
            AccentRole::Error,
        ] {
            let pins = raster_pins(vfd().with_accent(role).with_field(lilac));
            assert_eq!(pins.field, Some(lilac), "{role:?} carries the field");
            assert_eq!(
                pins.ink,
                raster_ink(vfd().with_accent(role)),
                "{role:?}: a field pin must not disturb the ink",
            );
        }
        let both = raster_pins(vfd().with_field(lilac).with_ink(violet));
        assert_eq!(
            (both.field, both.ink),
            (Some(lilac), kit::Ink::Fixed(violet)),
            "the two pins compose",
        );
    }

    /// The #898 parity rule at its widest: a `TextBox` with **all three** colors
    /// pinned rasterises byte-for-byte to the kit's `colors(bg, ink, notdef)`
    /// hatch. That is the exact claim `pet`'s and `caw`'s migration rests on —
    /// each was `TextBox::new().colors(…)` before #884, and each must still
    /// produce those bytes against a shell that does not speak preem.
    ///
    /// The three assertions after it are what keep the first from being vacuous:
    /// dropping any one pin has to change the pixels, or the parity above would
    /// hold for a wiring that ignored that pin entirely. The emoji in the text is
    /// deliberate — it is the only input that reaches the `notdef` slot.
    ///
    /// **Falsified** by dropping `kit::with_pins` from `lower`'s raster arm (the
    /// field and ink go missing), and — independently — by dropping the `notdef`
    /// arm from [`TextBox::kit`].
    #[test]
    fn the_raster_arm_is_byte_identical_for_a_fully_pinned_text_box() {
        let field = [0x3a, 0x22, 0x50, 0xff];
        let ink = [0xf0, 0xe0, 0xf8, 0xff];
        let notdef = [0x6c, 0x4e, 0x86, 0xff];
        let raster = RenderMode::Raster;
        let no_cls = Vec::new;
        let text = "mrrp 💕";

        let sdk = |b: TextBox| b.cols(10).scale(2).node_in(raster, "tb", no_cls(), text);
        let all = TextBox::new(StyleName::Lcd)
            .field(field)
            .ink(ink)
            .notdef(notdef);

        assert!(
            sdk(all.clone())
                == kit::TextBox::new()
                    .cols(10)
                    .scale(2)
                    .colors(field, ink, notdef)
                    .render(text)
                    .into_node(Some("tb"), no_cls()),
            "a fully pinned text box is the kit's `colors()` hatch, byte for byte",
        );

        let dropped_field = sdk(TextBox::new(StyleName::Lcd).ink(ink).notdef(notdef));
        let dropped_ink = sdk(TextBox::new(StyleName::Lcd).field(field).notdef(notdef));
        let dropped_notdef = sdk(TextBox::new(StyleName::Lcd).field(field).ink(ink));
        let full = sdk(all);
        assert!(full != dropped_field, "the field pin must move pixels");
        assert!(full != dropped_ink, "the ink pin must move pixels");
        assert!(
            full != dropped_notdef,
            "the notdef pin must move pixels — the emoji is what reaches it",
        );
    }

    /// The two style vocabularies map 1:1 in both directions, so a plugin
    /// rotating either one lands on the same skin.
    #[test]
    fn style_names_round_trip_through_the_kit() {
        for name in StyleName::ALL {
            assert_eq!(style_name(display_style(name)), name, "{name:?}");
            assert_eq!(name.name(), display_style(name).name(), "{name:?}");
        }
    }

    // ── the wire round trip ─────────────────────────────────────────────────

    /// An emitted state node survives the codec and decodes to the *same*
    /// widget, config and state — the claim the shell renderer relies on.
    #[test]
    fn an_emitted_state_node_round_trips_to_the_same_widget() {
        for node in every_widget(RenderMode::State) {
            let back: Node = decode(&encode(&node)).expect("a preem node decodes");
            assert_eq!(back, node, "{:?} round-trips", preem_of(&node).kind());
            assert_eq!(preem_of(&back), preem_of(&node));
        }
    }

    /// The decoded widget carries the values the plugin actually set, not just
    /// something structurally equal — a round-trip of two identical defaults
    /// would pass the test above while proving nothing.
    #[test]
    fn the_round_tripped_widget_carries_the_plugin_s_own_values() {
        let mut gauge = Gauge::new(StyleName::Crt).range(-20.0, 120.0).ticks(6, 4);
        gauge.set_target(37.5);
        let node = gauge.node_in(RenderMode::State, "ga", Vec::new());
        let back: Node = decode(&encode(&node)).expect("decodes");
        match preem_of(&back) {
            PreemWidget::Gauge { config, state } => {
                assert!((state.target - 37.5).abs() < f32::EPSILON);
                assert!((config.range.low - -20.0).abs() < f32::EPSILON);
                assert!((config.range.high - 120.0).abs() < f32::EPSILON);
                assert_eq!(config.divisions, 6);
                assert_eq!(config.subdivisions, 4);
                assert_eq!(config.style.style, StyleName::Crt);
            }
            other => panic!("expected a gauge, got {other:?}"),
        }
    }

    /// The state arm clamps before emitting, so the value the runtime dedups on
    /// is the value the shell will draw. An over-cap string is truncated on a
    /// char boundary, never mid-codepoint.
    #[test]
    fn the_state_arm_clamps_before_it_emits() {
        let long = "é".repeat(MAX_TEXT_LEN);
        let node = TextBox::new(StyleName::Vfd).node_in(RenderMode::State, "tb", Vec::new(), &long);
        match preem_of(&node) {
            PreemWidget::TextBox { state, .. } => {
                assert!(state.text.len() <= MAX_TEXT_LEN, "truncated to the cap");
                assert!(state.text.len() < long.len(), "and it really was over");
                assert!(
                    state.text.chars().all(|c| c == 'é'),
                    "cut on a char boundary",
                );
            }
            other => panic!("expected a text box, got {other:?}"),
        }
    }

    // ── dedup ───────────────────────────────────────────────────────────────

    /// The runtime dedups by comparing nodes, so "the wire goes quiet" is
    /// exactly "the node compares equal". A marquee scrolling an unchanged title
    /// is the canonical case: identical in state mode across ticks, different on
    /// every tick in raster mode.
    #[test]
    fn an_unchanged_marquee_is_quiet_in_state_mode_and_chatty_in_raster() {
        const TITLE: &str = "SOME TRACK TITLE THAT OVERFLOWS THE WINDOW";

        let mut state = Marquee::new(StyleName::Vfd).window_px(120);
        let first = state.node_in(RenderMode::State, "mq", Vec::new(), TITLE);
        for _ in 0..20 {
            state.advance_in(RenderMode::State, 0.05);
        }
        assert_eq!(
            state.node_in(RenderMode::State, "mq", Vec::new(), TITLE),
            first,
            "a second of shell-owned scrolling puts nothing on the wire",
        );
        assert_eq!(state.scroll_dots(), 0, "and the plugin never ticked");

        let mut raster = Marquee::new(StyleName::Vfd).window_px(120);
        let first = raster.node_in(RenderMode::Raster, "mq", Vec::new(), TITLE);
        for _ in 0..20 {
            raster.advance_in(RenderMode::Raster, 0.05);
        }
        assert!(raster.scroll_dots() > 0, "the plugin owns the scroll here");
        assert!(
            differ(
                &raster.node_in(RenderMode::Raster, "mq", Vec::new(), TITLE),
                &first
            ),
            "so every tick is a fresh buffer to send",
        );
    }

    /// The same quiet-vs-chatty split for the other three animated widgets: a
    /// gauge settling, a board mid-fold, and a strip's peak-hold falling all
    /// send one frame in state mode and one per tick in raster mode.
    #[test]
    fn settling_animations_are_quiet_in_state_mode() {
        let mut gauge = Gauge::new(StyleName::Vfd).range(0.0, 100.0);
        gauge.set_target(80.0);
        let first = gauge.node_in(RenderMode::State, "ga", Vec::new());
        for _ in 0..30 {
            gauge.advance_in(RenderMode::State, 0.033);
        }
        assert_eq!(gauge.node_in(RenderMode::State, "ga", Vec::new()), first);
        assert!(
            (gauge.value() - 0.0).abs() < f32::EPSILON,
            "the plugin-side needle never moved",
        );

        let mut board = FlipBoard::new(StyleName::Vfd, Mechanism::SplitFlap).cells(8);
        board.set_text("12:34:56");
        let first = board.node_in(RenderMode::State, "fb", Vec::new());
        for _ in 0..30 {
            board.advance_in(RenderMode::State, 0.033);
        }
        assert_eq!(board.node_in(RenderMode::State, "fb", Vec::new()), first);

        let mut led = LedStrip::new(StyleName::Vfd).peak_hold(0.05);
        led.set_level_in(RenderMode::State, 0.9);
        let first = led.node_in(RenderMode::State, "led", Vec::new());
        for _ in 0..30 {
            led.advance_in(RenderMode::State);
        }
        assert_eq!(led.node_in(RenderMode::State, "led", Vec::new()), first);
    }

    /// The one documented dedup *exception*, pinned rather than left to prose:
    /// [`Scope`]'s state is consumed rather than held, so a repeated batch
    /// compares equal and is coalesced, where the raster path draws a second
    /// frame — not because the new trace differs (an identical batch re-lights
    /// exactly the same pixels; the naive version of this test found that out
    /// the hard way) but because the *fading remains of the previous* trace
    /// moved on in between. Which is exactly the decay the shell runs on its own
    /// pump, frame or no frame — see the module docs.
    #[test]
    fn a_repeated_sample_batch_compares_equal_and_is_deduped() {
        const LOUD: [f32; 4] = [1.0, -1.0, 1.0, -1.0];
        const QUIET: [f32; 4] = [0.0, 0.0, 0.0, 0.0];

        let mut state = Scope::new(StyleName::Vfd);
        state.push_in(RenderMode::State, &LOUD);
        state.push_in(RenderMode::State, &QUIET);
        let first = state.node_in(RenderMode::State, "sc", Vec::new());
        state.push_in(RenderMode::State, &QUIET);
        assert_eq!(
            state.node_in(RenderMode::State, "sc", Vec::new()),
            first,
            "an identical batch says nothing new, so nothing goes on the wire",
        );

        let mut raster = Scope::new(StyleName::Vfd);
        raster.push_in(RenderMode::Raster, &LOUD);
        raster.push_in(RenderMode::Raster, &QUIET);
        let first = raster.node_in(RenderMode::Raster, "sc", Vec::new());
        raster.push_in(RenderMode::Raster, &QUIET);
        assert!(
            differ(
                &raster.node_in(RenderMode::Raster, "sc", Vec::new()),
                &first
            ),
            "the loud trace kept fading, so the raster buffer really did change",
        );
    }

    /// Dedup must still *fire* when the state genuinely changes — a wrapper that
    /// froze its state would pass every quiet-wire assertion above.
    #[test]
    fn a_real_state_change_still_produces_a_different_node() {
        let dm = DotMatrix::new(StyleName::Vfd);
        assert_ne!(
            dm.node_in(RenderMode::State, "dm", Vec::new(), "A"),
            dm.node_in(RenderMode::State, "dm", Vec::new(), "B"),
        );

        let mut gauge = Gauge::new(StyleName::Vfd).range(0.0, 100.0);
        gauge.set_target(10.0);
        let low = gauge.node_in(RenderMode::State, "ga", Vec::new());
        gauge.set_target(90.0);
        assert_ne!(gauge.node_in(RenderMode::State, "ga", Vec::new()), low);

        let mut board = FlipBoard::new(StyleName::Vfd, Mechanism::SplitFlap).cells(8);
        board.set_text("12:34:56");
        let a = board.node_in(RenderMode::State, "fb", Vec::new());
        board.set_text("12:34:57");
        assert_ne!(board.node_in(RenderMode::State, "fb", Vec::new()), a);

        let mut scope = Scope::new(StyleName::Vfd);
        scope.push_in(RenderMode::State, &[0.1, 0.2]);
        let a = scope.node_in(RenderMode::State, "sc", Vec::new());
        scope.push_in(RenderMode::State, &[0.3, 0.4]);
        assert_ne!(scope.node_in(RenderMode::State, "sc", Vec::new()), a);

        let mut strip = LedStrip::new(StyleName::Vfd);
        strip.set_level_in(RenderMode::State, 0.1);
        let a = strip.node_in(RenderMode::State, "led", Vec::new());
        strip.set_level_in(RenderMode::State, 0.9);
        assert_ne!(strip.node_in(RenderMode::State, "led", Vec::new()), a);
    }

    /// The plugin-side physics getters must not report the **frozen** local
    /// animation once the shell owns it.
    ///
    /// This is the trap `advance` being a no-op sets: the needle and the flip
    /// clocks stop where the last raster tick left them, so `value()` would
    /// answer with a deflection that is neither where the shell is drawing the
    /// pointer nor where it is heading, and `is_settled()` would answer `false`
    /// forever — stalling any plugin that gates work on "has it arrived yet?".
    /// In state mode there is no local motion left to run, so the honest
    /// answers are the target and `true`.
    #[test]
    fn the_physics_getters_do_not_report_a_frozen_needle_in_state_mode() {
        let mut gauge = Gauge::new(StyleName::Vfd).range(0.0, 100.0);
        gauge.set_target(80.0);

        // Raster: the local needle really is still down at the low end and
        // really has not arrived — that is the pre-#884 behaviour, unchanged.
        assert!(
            gauge.value_in(RenderMode::Raster) < 1.0,
            "the local needle has not moved without an advance",
        );
        assert!(!gauge.is_settled_in(RenderMode::Raster));

        // State: the shell owns the spring, so the plugin reports the reading
        // it stated and nothing outstanding.
        assert!((gauge.value_in(RenderMode::State) - 80.0).abs() < f32::EPSILON);
        assert!((gauge.value_in(RenderMode::State) - gauge.target()).abs() < f32::EPSILON);
        assert!(gauge.is_settled_in(RenderMode::State));

        let mut board = FlipBoard::new(StyleName::Vfd, Mechanism::SplitFlap).cells(8);
        board.set_text("12:34:56");
        assert!(
            !board.is_settled_in(RenderMode::Raster),
            "locally the cards are mid-fold",
        );
        assert!(
            board.is_settled_in(RenderMode::State),
            "…but there is nothing for this plugin to advance",
        );

        // …and the ambient getters route through `render_mode()`, or the two
        // `_in` assertions above would be proving nothing about `is_settled()`.
        set_negotiated(PREEM_VOCAB);
        assert!(gauge.is_settled() && board.is_settled());
        assert!((gauge.value() - 80.0).abs() < f32::EPSILON);
        set_negotiated(0);
        assert!(!gauge.is_settled() && !board.is_settled());
        assert!(gauge.value() < 1.0);
    }

    // ── the raster path is untouched ────────────────────────────────────────

    /// Raster mode still ticks everything the plugin used to tick itself, so an
    /// old shell sees exactly today's animation.
    #[test]
    fn raster_mode_still_runs_the_plugin_side_animation() {
        let mut gauge = Gauge::new(StyleName::Vfd).range(0.0, 100.0);
        gauge.set_target(80.0);
        let mut peak = f32::MIN;
        for _ in 0..120 {
            gauge.advance_in(RenderMode::Raster, 0.016);
            peak = peak.max(gauge.value_in(RenderMode::Raster));
        }
        assert!(peak > 80.0, "the needle overshot (peaked at {peak})");
        assert!(gauge.is_settled_in(RenderMode::Raster), "and settled");

        let mut board = FlipBoard::new(StyleName::Vfd, Mechanism::SplitFlap).cells(8);
        board.set_text("12:34:56");
        assert!(
            !board.is_settled_in(RenderMode::Raster),
            "a fresh face is mid-flip",
        );
        for _ in 0..60 {
            board.advance_in(RenderMode::Raster, 0.05);
        }
        assert!(board.is_settled_in(RenderMode::Raster), "and lands");

        let mut scope = Scope::new(StyleName::Vfd);
        let dark = scope.node_in(RenderMode::Raster, "sc", Vec::new());
        scope.push_in(RenderMode::Raster, &[1.0, -1.0, 1.0, -1.0]);
        assert!(
            differ(&scope.node_in(RenderMode::Raster, "sc", Vec::new()), &dark),
            "a batch stamps the phosphor",
        );
        scope.clear();
        assert!(
            !differ(&scope.node_in(RenderMode::Raster, "sc", Vec::new()), &dark),
            "and clear wipes it",
        );
    }

    /// The plugin-side peak-hold rides the level and falls in raster mode; an
    /// explicit peak wins for the render without disturbing the held value.
    #[test]
    fn the_raster_peak_hold_rides_and_falls() {
        let mut led = LedStrip::new(StyleName::Vfd).leds(24).peak_hold(0.1);
        led.set_level_in(RenderMode::Raster, 1.0);
        let held = led.node_in(RenderMode::Raster, "led", Vec::new());
        led.set_level_in(RenderMode::Raster, 0.0);
        assert!(
            differ(&led.node_in(RenderMode::Raster, "led", Vec::new()), &held),
            "the level dropped away from the held peak",
        );
        let dropped = led.node_in(RenderMode::Raster, "led", Vec::new());
        for _ in 0..10 {
            led.advance_in(RenderMode::Raster);
        }
        assert!(
            differ(
                &led.node_in(RenderMode::Raster, "led", Vec::new()),
                &dropped
            ),
            "and the dot fell",
        );
    }
}
