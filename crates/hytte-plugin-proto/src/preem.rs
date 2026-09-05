//! The **preem widget state vocabulary** (#882, epic #881): typed retro-display
//! nodes that carry *what to show*, not the pixels showing it.
//!
//! Today a plugin that wants a VFD marquee or a needle gauge links the CPU
//! raster kit (`hytte-preem`), rasterises a frame per animation tick, and ships
//! ~64 KiB of RGBA down the socket as a [`Node::Pixels`](crate::wire::Node::Pixels).
//! This module is the other half of that trade: the plugin states the widget's
//! *configuration* and its *current target*, and the shell — which owns a real
//! frame clock, the desktop accent, and the display styles — does the drawing.
//!
//! # The two field classes
//!
//! Every widget here splits its fields into exactly two structs, and the split
//! is the kit's own pure-vs-stateful split:
//!
//! - **`…Config`** — what today lives in the kit's *builder* (geometry, timing
//!   knobs, the style reference). A config change means **rebuild the renderer
//!   instance**: the shell drops the widget's animation state (phosphor buffer,
//!   needle velocity, flip clocks) and constructs a fresh one. Config is
//!   therefore expected to be *stable* across renders — a plugin that jitters a
//!   config field kills its own animation.
//! - **`…State`** — what today is the argument to the kit's `render()` /
//!   `set_*()` call (text, level, samples, target). A state change means
//!   **animate toward the target**: the existing renderer instance keeps its
//!   momentum and moves.
//!
//! # Animation ownership — the wire never carries animation state
//!
//! What the kit holds as *internal mutable animation* stays out of the wire
//! entirely. The shell owns, and the plugin can neither read nor write:
//!
//! - the [`Scope`](PreemWidget::Scope)'s **phosphor** decay buffer,
//! - the [`Gauge`](PreemWidget::Gauge)'s **needle** position/velocity spring
//!   integration,
//! - the [`FlipBoard`](PreemWidget::FlipBoard)'s per-cell **flip clocks** and
//!   left-to-right stagger,
//! - the [`Marquee`](PreemWidget::Marquee)'s **scroll offset**,
//! - the [`LedStrip`](PreemWidget::LedStrip)'s **peak-hold decay** (unless the
//!   plugin overrides it — see [`LedStripState::peak`]).
//!
//! That is what makes the traffic go quiet: a marquee scrolling a track title
//! sends **one** frame when the title changes, not twenty a second, because the
//! offset advance lives on the shell's pump. It is also why none of these
//! widgets carry a phase, offset, or elapsed-time field — a field like that
//! would put the animation back on the wire and re-introduce exactly the
//! per-tick chatter this vocabulary removes.
//!
//! ## …which is why a preem node needs an `id` (#900)
//!
//! Shell-owned animation state has one consequence the plugin must honor: the
//! node's `id` is what ties a renderer instance to the widget it belongs to, and
//! nothing in a frame can re-derive it. So
//! [`Node::Preem::id`](crate::wire::Node::Preem) is **required in practice** —
//! see that variant's docs for the exact contract, the ordinal fallback an
//! anonymous node degrades to, and why a row of interchangeable widgets
//! transplants its needles without one. Build nodes with [`preem_id`] /
//! [`preem_styled`], or through the `hytte-plugin` SDK's `display` wrappers,
//! which stamp the id from the widget key they already take.
//!
//! # Style is a *reference*, not colors
//!
//! [`StyleRef`] carries the [`StyleName`] (`vfd`/`lcd`/`oled`/`crt`) plus an
//! optional **semantic** [`AccentRole`]. The shell resolves both against the
//! live desktop theme, which is the whole payoff: a `@accent_color` change
//! re-tints every preem widget on screen with zero plugin involvement and zero
//! frames on the wire.
//!
//! There is exactly one way to put a resolved color on the wire —
//! [`StyleRef::ink`], the explicit pin — and taking it costs precisely that
//! payoff: a pinned widget keeps its ink while the desktop re-tints around it.
//! It is ink-only (the skin still owns field, ghost, bloom and the CRT pass) and
//! it is the exception, for a color that *is* the meaning and that no role
//! names.
//!
//! # Compat contract
//!
//! [`Node::Preem`](crate::wire::Node::Preem) lands at wire-vocabulary generation
//! [`PREEM_VOCAB`], which is **negotiated, not unconditional** (see
//! [`VOCAB_UNCONDITIONAL`](crate::VOCAB_UNCONDITIONAL)): a plugin emits it only
//! after the host advertises support with
//! [`HostMsg::Hello`](crate::msg::HostMsg::Hello). Against a shell that never
//! advertises — an older one, or one that hasn't grown the renderer yet — the
//! `hytte-plugin` SDK keeps CPU-rasterising to
//! [`Node::Pixels`](crate::wire::Node::Pixels) exactly as it does today. Same
//! output, no silent blanks, no handshake refusal; the deprecation is soft on
//! both sides and version skew degrades to the status quo rather than to
//! breakage. In the other direction a *new* shell keeps its unchanged `Pixels`
//! arm, so an old plugin is unaffected.
//!
//! # Wire limits
//!
//! Every field the shell would size an allocation from is capped, in the same
//! spirit as `Node::Pixels`'s `len == w * h * 4` invariant: a malformed or
//! hostile plugin must never be able to crash or wedge the shell.
//!
//! The invariant that matters is the **product**, not the fields:
//! [`MAX_RASTER_PIXELS`] bounds a widget's rasterised buffer at
//! [`MAX_BUFFER_DIM`]² = `MAX_FRAME_LEN / 4`, so a preem node can never demand
//! more than one legacy `Pixels` frame could have carried. Per-field caps
//! ([`MAX_TEXT_LEN`], [`MAX_SCOPE_SAMPLES`], [`MAX_SCALE`], [`MAX_TEXT_COLS`],
//! [`MAX_TEXT_LINES`], [`MAX_PAD`], [`MAX_CORNER`], [`MAX_DIVISIONS`],
//! [`MAX_SUBDIVISIONS`], [`MAX_GAP_DOTS`], [`MAX_MARQUEE_SPEED_DPS`],
//! [`MAX_LEDS`], [`MAX_CELLS`], [`MAX_STRIP_DIM`]) exist to make that product hold — a dimension
//! and its multiplier capped independently bound nothing, which is why
//! [`PreemWidget::clamped`] fits `scale` to the buffer rather than just
//! ceiling it.
//!
//! [`PreemWidget::clamped`] is the shared enforcement both ends use so the host
//! never hand-rolls it, and it is **mandatory before any renderer sees a
//! widget**.
//!
//! # Non-finite floats are replaced, not passed through
//!
//! Every `f32` in this vocabulary is *sanitised* by [`PreemWidget::clamped`]
//! too: a `NaN` or an infinity — a `sum / count` with `count == 0`, a `-inf` dB
//! fold, an unread sensor — is replaced with a finite, in-range value before
//! any renderer sees it, and an out-of-range finite value clamps to the same
//! bound the kit would have used. The rule is **parity with the kit**: every
//! non-finite value takes the image the kit's own entry point gives it — `±inf`
//! saturating where the kit clamps outright, the kept default where the kit
//! guards on `is_finite` — and a documented per-field constant stands in only
//! where the kit's answer is a *stateful* keep-previous a stateless clamp
//! cannot reach. The per-field mapping, with the
//! `hytte-preem` line each row is derived from, is on
//! [`PreemWidget::clamp_in_place`] and is **contract**, not an implementation
//! detail.
//!
//! This is not decoration on top of the allocation caps. It is what makes the
//! derived `PartialEq` on [`PreemWidget`] — and so on a
//! [`Node::Preem`](crate::wire::Node::Preem) carrying one — a usable *did
//! anything change?* test, because `NaN` never compares equal to itself: one
//! poisoned reading makes a widget unequal to an identical copy of itself, and
//! that defeats the render dedup at **both** ends at once. The plugin SDK's
//! `view != last_view` stays true forever, so it emits one `Render` per
//! heartbeat; the host's `applied == widget` gate never short-circuits, so it
//! rebuilds or re-rasterises the widget on every mapping pass — for a picture
//! that never changes. Both loops are unbounded in time, and both end at this
//! one seam.
//!
//! The claim no longer stops at [`Node::Preem`](crate::wire::Node::Preem).
//! [`Node::Progress`](crate::wire::Node::Progress) and
//! [`Node::Slider`](crate::wire::Node::Slider) carry `f64`s that used to spin
//! the same two loops; #904 gave them their own seam,
//! [`Node::clamp_in_place`](crate::wire::Node::clamp_in_place), with a mapping
//! derived from *their* drawing code (GTK, not the kit) rather than a copy of
//! this one — and that seam delegates a `Preem` child straight back to
//! [`PreemWidget::clamp_in_place`], so one call over a render tree leaves every
//! float in it finite.

use serde::{Deserialize, Serialize};

use crate::wire::{Cls, NodeId};

// ── the vocabulary generation this module lands at ──────────────────────────

/// The wire-vocabulary generation at which the preem vocabulary appeared (#882).
///
/// A plugin may put a [`Node::Preem`](crate::wire::Node::Preem) on the wire only
/// once [`Manifest::negotiated_vocab`](crate::manifest::Manifest::negotiated_vocab)
/// — the minimum of what it can speak and what the host advertised in
/// [`HostMsg::Hello`](crate::msg::HostMsg::Hello) — has reached this value.
/// Below it, rasterise to [`Node::Pixels`](crate::wire::Node::Pixels) instead.
pub const PREEM_VOCAB: u16 = 2;

// ── wire limits ─────────────────────────────────────────────────────────────

/// Cap on every text field in this module, in **bytes** of UTF-8.
///
/// This is **not** a decode bound — decoding is already bounded by
/// [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN), and truncation here happens
/// *after* the frame is decoded. What it bounds is what the shell will go on
/// to **rasterise and retain**.
///
/// It is a *byte* cap and deliberately generous, because most widgets here
/// bound their own text geometrically: the [`TextBox`](PreemWidget::TextBox)
/// wraps to [`max_lines`](TextBoxConfig::max_lines), the
/// [`FlipBoard`](PreemWidget::FlipBoard) ignores anything past its last cell,
/// and the [`Marquee`](PreemWidget::Marquee) allocates only its window
/// (`marquee.rs:190` fills a `window_px`-wide frame; the scrolling strip is a
/// column bitmap, not a frame), so [`MarqueeConfig::window_px`] alone bounds
/// it.
///
/// The two that do **not** bound themselves — [`DotMatrix`](PreemWidget::DotMatrix)
/// and [`SevenSeg`](PreemWidget::SevenSeg), which lay the whole message out on
/// one line — get a second, *character*-count cap against their own pitch; see
/// `clamp_strip_text` and [`MAX_STRIP_DIM`]. Do not try to make this byte cap
/// carry that job: the two strips have different pitches (24 px and 40 px per
/// character), so no single byte number bounds both without crippling the
/// widgets that wrap.
///
/// [`PreemWidget::clamped`] truncates to the nearest char boundary at or below
/// this length, never mid-codepoint.
pub const MAX_TEXT_LEN: usize = 2048;

/// Cap on the long axis of a **single-line strip** — exactly two widgets, the
/// [`DotMatrix`](PreemWidget::DotMatrix) and the [`SevenSeg`](PreemWidget::SevenSeg).
///
/// These are the one shape where [`MAX_BUFFER_DIM`] is the wrong bound: a strip
/// is a few pixels tall and as wide as its message, so it can be far wider than
/// a square widget while allocating far less. What actually constrains it is
/// texture upload — 16384 px is the common maximum texture edge, and it is the
/// same number the shell's legacy `Pixels` path already uses for its scaled
/// dimension (`wire_map.rs`'s `MAX_PIXELS_SCALED_DIM`).
///
/// Two widgets, not three or four. The [`Marquee`](PreemWidget::Marquee) is
/// *not* on this list — it allocates only its window (`marquee.rs:190`), so
/// [`MAX_BUFFER_DIM`] via [`window_px`](MarqueeConfig::window_px) already
/// bounds it — and neither is the [`LedStrip`](PreemWidget::LedStrip), whose
/// worst case is 1413 px, well inside [`MAX_BUFFER_DIM`]. Exempting a widget
/// that does not need it silently widens what the bound accepts.
///
/// The *area* of a strip stays bounded by [`MAX_RASTER_PIXELS`] like everything
/// else; this only relaxes the per-axis rule where the geometry justifies it.
/// It is not a knob a plugin sets — `clamp_strip_text` derives each strip's
/// character budget from it and that widget's own pitch
/// ([`DOT_MATRIX_PITCH_PX`] / [`SEVEN_SEG_PITCH_PX`]), and
/// `preem_worst_case_footprint_is_bounded` checks the arithmetic against the
/// kit's real geometry.
pub const MAX_STRIP_DIM: u32 = 16_384;

/// Cap on [`ScopeState::samples`] **per update**.
///
/// Like [`MAX_TEXT_LEN`] this bounds what the shell **retains and draws**, not
/// what it will decode — [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN) is the decode
/// bound, and this truncation runs after it. The scope stamps a polyline across
/// [`ScopeConfig::cols`] columns (144 by default), so anything past a few
/// hundred samples per batch is already oversampled into invisibility; 4096
/// leaves a wide margin for a plugin feeding a raw audio block.
pub const MAX_SCOPE_SAMPLES: usize = 4096;

/// Cap on any buffer dimension — **both** the logical (pre-upscale) value a
/// config states and the final (post-upscale) size the shell rasterises.
///
/// Enforcing it on the *scaled* dimension is the load-bearing half, and mirrors
/// what the legacy [`Pixels`](crate::wire::Node::Pixels) path already does
/// (`wire_map.rs`'s `clamp_pixels_scale`). Capping the logical dimension and
/// the scale factor **independently** would not bound anything useful: at
/// 4096 logical and 8× that is a 32768×32768 buffer — a 4 GiB allocation
/// demand from about thirty bytes of wire config.
///
/// 2048 is not arbitrary. `2048 * 2048 * 4 B` is exactly
/// [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN) (see [`MAX_RASTER_PIXELS`]), so a
/// preem node can never ask the shell to rasterise more than a single
/// `Node::Pixels` frame could have carried in the first place. The typed
/// vocabulary is a bandwidth win, never an amplification primitive. The kit's
/// own widgets target a ~296 px sidebar card, so this is still ~7× past any
/// real surface.
pub const MAX_BUFFER_DIM: u32 = 2048;

/// The resulting ceiling on a single preem widget's rasterised buffer, in
/// pixels: [`MAX_BUFFER_DIM`]², i.e. [`MAX_FRAME_LEN`](crate::MAX_FRAME_LEN)`/4`
/// RGBA8 pixels (4 194 304 px = 16 MiB).
///
/// Stated as its own constant because it is the invariant that actually
/// matters — every per-field cap below exists only to make *this* product
/// hold, and `preem_worst_case_footprint_is_bounded` in `tests/preem.rs`
/// checks it against the kit's real geometry for all eight widgets.
pub const MAX_RASTER_PIXELS: u32 = MAX_BUFFER_DIM * MAX_BUFFER_DIM;

/// Cap on any integer upscale factor (`scale` on
/// [`TextBox`](TextBoxConfig::scale) / [`Scope`](ScopeConfig::scale) /
/// [`Gauge`](GaugeConfig::scale) / [`FlipBoard`](FlipBoardConfig::scale)).
///
/// A ceiling on the knob itself; the *binding* constraint is the scaled-dimension
/// rule above, which pulls `scale` down further whenever the logical buffer is
/// already large. The kit's own defaults are 1× and 2×; 8× is already a
/// chunkier pixel than any skin reads well at.
pub const MAX_SCALE: u32 = 8;

/// Cap on [`TextBoxConfig::width`] in glyph cells (and on the `FitPx` budget,
/// which is in pixels and so is capped by [`MAX_BUFFER_DIM`] instead).
///
/// The kit's default is 16 cells and the widest real use is the pet's ~268 px
/// bubble (~44 cells). At the font's 6 px cell pitch, 256 cells is a 1536 px
/// line — generous, and it keeps the scaled width inside [`MAX_BUFFER_DIM`].
pub const MAX_TEXT_COLS: u32 = 256;

/// Cap on [`TextBoxConfig::max_lines`]. Kit default 3; 64 wrapped lines is far
/// past anything a sidebar card shows and keeps the box's height bounded.
pub const MAX_TEXT_LINES: u32 = 64;

/// Cap on [`TextBoxConfig::pad`], in pre-scale pixels.
///
/// Small on purpose: padding is added to **both** dimensions (`textbox.rs`'s
/// `buf_w = 2*pad + …`, `buf_h = 2*pad + …`), so an uncapped `pad` inflates the
/// buffer quadratically all on its own — 4096 padding at 8× is a ~17 GB box
/// around an empty string. Kit default 3; 64 is ~21× that.
pub const MAX_PAD: u32 = 64;

/// Cap on [`TextBoxConfig::corner`], the rounded-corner cut radius in pre-scale
/// pixels. Kit default 2. It does not change the buffer size, but it drives a
/// per-pixel corner test, so it is bounded for the same reason as [`MAX_PAD`].
pub const MAX_CORNER: u32 = 64;

/// Cap on [`GaugeConfig::divisions`]. Kit default 4.
pub const MAX_DIVISIONS: u32 = 64;

/// Cap on [`GaugeConfig::subdivisions`]. Kit default 5.
///
/// Capped **as a product** with [`MAX_DIVISIONS`], not just individually: the
/// gauge rasterises `divisions * subdivisions` tick marks every frame
/// (`gauge.rs`'s `tick_marks`), so two independently "reasonable" 4096s are
/// 16.7M line draws per frame at any buffer size — a CPU denial-of-service that
/// no buffer cap can see. 64 × 32 = 2048 ticks is already ~100× the kit's
/// default 20 and stays cheap.
pub const MAX_SUBDIVISIONS: u32 = 32;

/// Cap on [`MarqueeConfig::gap_dots`], the blank seam before the message loops.
/// It extends the rasterised strip, so it is bounded like a dimension. Kit
/// default 6.
pub const MAX_GAP_DOTS: u32 = 1024;

/// Cap on the magnitude of [`MarqueeConfig::speed_dots_per_sec`].
///
/// The one float in this vocabulary with **no kit clamp behind it** — the kit's
/// `Marquee` has no speed at all (`MarqueeStrip::window(offset)` is
/// caller-driven), so #882 invented the field and therefore owes it a bound.
/// At 1000 dots/s a message crosses a 268 px window about four times a second,
/// which is already unreadable. [`PreemWidget::clamped`] also maps a non-finite
/// speed to `0.0` (parked) rather than letting a `NaN` reach the shell's pump
/// integrator.
pub const MAX_MARQUEE_SPEED_DPS: f32 = 1000.0;

/// Cap on [`LedStripConfig::leds`].
///
/// The strip's rendered width grows linearly with the segment count
/// (`2*PAD + n*CELL_W + (n-1)*GAP` = `8 + 11n - 3` px), and the kit's default
/// is 24 across a ~296 px card. 128 segments is a 1413 px strip — over 5× the
/// default and still inside [`MAX_BUFFER_DIM`].
pub const MAX_LEDS: u32 = 128;

/// Cap on [`FlipBoardConfig::cells`].
///
/// A board's width grows linearly with the cell count *times*
/// [`FlipBoardConfig::glyph_px`] times `scale`, which is why
/// [`PreemWidget::clamped`] pulls those two multipliers down when a wide board
/// would otherwise overflow [`MAX_BUFFER_DIM`]. Kit default 8 (`HH:MM:SS`); 64
/// is past a departure board's longest row.
pub const MAX_CELLS: u32 = 64;

// ── float bounds ────────────────────────────────────────────────────────────
//
// The bounds below are the *kit's own* clamps, restated on the wire. The kit
// still applies them; naming them here is what lets `clamp_in_place` give every
// non-finite float a defined finite image, which a bound it cannot name it
// cannot do. The two ranges obvious enough not to earn a constant are the
// `0.0..=1.0` level/peak of a `LedStrip` (full scale *is* `1.0`) and the
// `-1.0..=1.0` of a normalized `Scope` sample.

/// Cap on [`PeakHoldConfig::rate`] — the peak dot's fall per animation tick, in
/// fractions of full scale.
///
/// `1.0` drops the dot the whole length of the strip in a single tick, so
/// nothing above it means anything; the floor is `0.0`, the kit's own
/// clamp-negative-to-zero "never falls", which is also the field's default.
///
/// It is the one bound here the kit does not state itself — `PeakHold::new`
/// only does `rate.max(0.0)` — and it is invisible: the hold decays by
/// `(value - rate).max(0.0)` from a value in `0.0..=1.0`, so every rate at or
/// above `1.0` empties it in one tick exactly as `1.0` does. It exists so a
/// non-finite rate has a finite image at the top end too.
pub const MAX_PEAK_HOLD_RATE: f32 = 1.0;

/// Floor on [`GaugeConfig::sweep_deg`] — the kit's own clamp (`gauge.rs`).
pub const MIN_SWEEP_DEG: f32 = 10.0;

/// Ceiling on [`GaugeConfig::sweep_deg`] — the kit's own clamp.
pub const MAX_SWEEP_DEG: f32 = 180.0;

/// Floor on [`GaugeConfig::frequency_hz`] — the kit's own clamp.
pub const MIN_FREQUENCY_HZ: f32 = 0.05;

/// Ceiling on [`GaugeConfig::frequency_hz`] — the kit's own clamp.
pub const MAX_FREQUENCY_HZ: f32 = 20.0;

/// Floor on [`GaugeConfig::damping`] — the kit's own clamp.
pub const MIN_DAMPING: f32 = 0.05;

/// Ceiling on [`GaugeConfig::damping`] — the kit's own clamp.
pub const MAX_DAMPING: f32 = 4.0;

/// Floor on [`FlipBoardConfig::duration_secs`] — the kit's own clamp.
pub const MIN_FLIP_DURATION_SECS: f32 = 0.01;

/// Ceiling on [`FlipBoardConfig::duration_secs`] — the kit's own clamp.
pub const MAX_FLIP_DURATION_SECS: f32 = 10.0;

/// Ceiling on [`FlipBoardConfig::stagger_secs`] — the kit's own clamp. The
/// floor is `0.0`: the whole row moving as one.
pub const MAX_FLIP_STAGGER_SECS: f32 = 2.0;

/// The kit's default gauge sweep, and the value a non-finite
/// [`GaugeConfig::sweep_deg`] resolves to. Shared with `GaugeConfig::default`
/// so the replacement and the default cannot drift apart.
const DEFAULT_SWEEP_DEG: f32 = 150.0;

/// The kit's default needle frequency, and the value a non-finite
/// [`GaugeConfig::frequency_hz`] resolves to.
const DEFAULT_FREQUENCY_HZ: f32 = 2.0;

/// The kit's default damping ratio, and the value a non-finite
/// [`GaugeConfig::damping`] resolves to.
const DEFAULT_DAMPING: f32 = 0.5;

// ── style reference ─────────────────────────────────────────────────────────

/// The retro display skin, **by name** — the wire form of `hytte-preem`'s
/// `DisplayStyle` (`crates/hytte-preem/src/style.rs`).
///
/// Deliberately not a color: the shell holds the palettes, so it can re-skin
/// every live widget when the desktop accent or color scheme changes without
/// the plugin knowing anything happened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StyleName {
    /// Vacuum fluorescent: pale cyan on near-black, phosphor glow, faint ghost.
    #[default]
    Vfd,
    /// Reflective LCD: dark ink on an olive field, ghost cells, no glow.
    Lcd,
    /// OLED: white-blue on true black, tight bloom, no ghosting.
    Oled,
    /// Phosphor CRT: P31-green, broad bloom, scanline comb + glass vignette.
    Crt,
}

impl StyleName {
    /// Every style name, in the kit's canonical rotation order — the wire
    /// mirror of `DisplayStyle::ALL`.
    pub const ALL: [Self; 4] = [Self::Vfd, Self::Lcd, Self::Oled, Self::Crt];

    /// The lowercase word form (`"vfd"` / `"lcd"` / `"oled"` / `"crt"`),
    /// matching `DisplayStyle::name` so a host can map either way by name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Vfd => "vfd",
            Self::Lcd => "lcd",
            Self::Oled => "oled",
            Self::Crt => "crt",
        }
    }
}

/// An explicit ink color, `[r, g, b, a]` — the same byte quad
/// [`HostMsg::Accent`](crate::msg::HostMsg::Accent) already carries, and the
/// same layout [`Node::Pixels`](crate::wire::Node::Pixels) uses per pixel, so it
/// crosses the SDK/kit boundary unconverted (`hytte_preem::Rgba` **is** this
/// type).
///
/// Deliberately `u8` channels rather than floats: there is nothing to sanitise
/// — every bit pattern is a valid color, so
/// [`PreemWidget::clamp_in_place`] has no rule to apply to it and
/// `clamped(w) == clamped(w)` stays reflexive with one in the widget. (The alpha
/// byte travels for symmetry with `HostMsg::Accent`; a preem frame is a
/// *screen*, so the kit draws ink opaque either way.)
///
/// Reaching for one is the exception, not the path — see [`StyleRef::ink`].
pub type Rgba = [u8; 4];

/// A **semantic** ink role, resolved shell-side against the live theme.
///
/// This is what replaces a plugin reaching for RGBA (the kit's `TextBox::colors`
/// escape hatch). A plugin says *what the reading means*; the shell decides what
/// that looks like in the current accent and color scheme, so a re-tint costs no
/// wire traffic and no plugin rebuild.
///
/// `None` on [`StyleRef::accent`] — the default — leaves the ink to the host:
/// the session accent when it has resolved one, otherwise the skin's own
/// hard-coded ink. That is what an accent-tinted host renders for a widget that
/// says nothing, so a frame omitting the key looks exactly as it did before this
/// field existed. [`Neutral`](Self::Neutral) is the *explicit* opt-out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AccentRole {
    /// The desktop accent (libadwaita's `@accent_color`) — the same tint the
    /// kit already applies via [`HostMsg::Accent`](crate::msg::HostMsg::Accent),
    /// named here so a widget can ask for it explicitly.
    Accent,
    /// A good/settled reading (`@success_color`).
    Success,
    /// A reading that wants attention but isn't a fault (`@warning_color`).
    Warning,
    /// A fault or an over-range reading (`@error_color`).
    Error,
    /// Deliberately un-tinted: the skin's own ink, ignoring any accent the host
    /// would otherwise apply. The way to pin one widget's look while the rest
    /// of the desktop re-tints around it.
    Neutral,
}

/// How a preem widget asks to be skinned: a [`StyleName`], a semantic
/// [`AccentRole`], and — the escape hatch — an explicit [`ink`](Self::ink).
///
/// The first two are the path (#885): the shell resolves the role against the
/// live theme on every render, so changing the desktop accent re-tints every
/// widget on screen with no wire traffic and no plugin restart. The third pins
/// one widget's ink and **opts it out of that**.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleRef {
    /// The display skin.
    pub style: StyleName,
    /// The semantic ink role, or `None` to leave the ink to the host — see
    /// [`AccentRole`] for what each resolves to, and what a frame that omits the
    /// key decodes to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<AccentRole>,
    /// An explicit ink, overriding whatever [`accent`](Self::accent) would have
    /// resolved to. `None` — the default, and what a frame omitting the key
    /// decodes to — leaves the role in charge.
    ///
    /// # This is the discouraged path, and it costs the live re-tint
    ///
    /// A pinned widget is **deliberately excluded from re-tinting**: it keeps
    /// this exact ink while the desktop accent changes around it, because that
    /// is what pinning means. Reach for it when the color *is* the meaning and
    /// no role names it (a brand color, a plugin's own signature tint); reach
    /// for [`AccentRole`] for everything else, including anything that means
    /// "good", "warn" or "broken".
    ///
    /// # Ink only
    ///
    /// It replaces the accent-tinted part — the lit ink — and nothing else. The
    /// skin still supplies the field, the ghost, the bloom and the CRT pass, so
    /// a pinned widget still reads as the same device. (Widening this to a whole
    /// palette override is a deliberate non-goal here; the skin *is* the
    /// vocabulary for the panel's physical character.)
    ///
    /// # Pin a color that does not move
    ///
    /// This lives in the **config**, so the module's config rule applies to it
    /// like any other field: *changing* it rebuilds the shell's renderer, and a
    /// rebuild loses the animation state that renderer owned — a needle back at
    /// rest, a cleared phosphor, a blanked flip board, a marquee snapped to
    /// offset 0. A steady pin costs nothing (an unchanged config is compared,
    /// not rebuilt), but driving one from a reading — `ink(RED)` above a
    /// threshold — resets the animation on every crossing. That case wants an
    /// [`AccentRole`], which has the same problem and the same answer: state it
    /// once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ink: Option<Rgba>,
}

impl StyleRef {
    /// A style reference with no accent role and no pinned ink — the host's
    /// default ink for the skin.
    #[must_use]
    pub fn new(style: StyleName) -> Self {
        Self {
            style,
            accent: None,
            ink: None,
        }
    }

    /// The same skin, asking for a semantic ink role. Chainable off
    /// [`StyleRef::new`].
    #[must_use]
    pub fn with_accent(mut self, accent: AccentRole) -> Self {
        self.accent = Some(accent);
        self
    }

    /// The same skin with an explicit ink pinned, opting the widget out of the
    /// live re-tint — see [`ink`](Self::ink) before reaching for it.
    #[must_use]
    pub fn with_ink(mut self, ink: Rgba) -> Self {
        self.ink = Some(ink);
        self
    }
}

// ── DotMatrix ───────────────────────────────────────────────────────────────

/// **Config** for [`PreemWidget::DotMatrix`] — a change rebuilds the renderer.
///
/// The kit's `dot_matrix(text, style)` is pure, so the skin is its only knob.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct DotMatrixConfig {
    /// The skin to draw in.
    pub style: StyleRef,
}

/// **State** for [`PreemWidget::DotMatrix`] — a change re-renders the text.
///
/// The static matrix has no animation of its own, so "animate toward the
/// target" degenerates to an immediate redraw here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct DotMatrixState {
    /// The line to display. Capped at [`MAX_TEXT_LEN`].
    pub text: String,
}

// ── SevenSeg ────────────────────────────────────────────────────────────────

/// **Config** for [`PreemWidget::SevenSeg`] — a change rebuilds the renderer.
///
/// The kit's `seven_seg(text, style)` is pure, so the skin is its only knob.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct SevenSegConfig {
    /// The skin to draw in.
    pub style: StyleRef,
}

/// **State** for [`PreemWidget::SevenSeg`] — a change re-renders the readout.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct SevenSegState {
    /// The readout to display. Capped at [`MAX_TEXT_LEN`].
    pub text: String,
}

// ── TextBox ─────────────────────────────────────────────────────────────────

/// How a [`TextBox`](PreemWidget::TextBox) picks its wrap width — the wire form
/// of the kit's private `WidthSpec` (`crates/hytte-preem/src/textbox.rs`), whose
/// two settings are mutually exclusive and so model as one enum rather than two
/// competing `Option`s.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TextBoxWidth {
    /// An explicit wrap width in glyph cells (the kit clamps to at least 1).
    Cols(u32),
    /// Fit a **final** (post-[`TextBoxConfig::scale`]) pixel budget: the widest
    /// column count whose rendered line — padding included — still fits.
    FitPx(u32),
}

impl Default for TextBoxWidth {
    /// The kit's default: 16 columns.
    fn default() -> Self {
        Self::Cols(16)
    }
}

/// **Config** for [`PreemWidget::TextBox`] — a change rebuilds the renderer.
///
/// Mirrors the kit's `TextBox` builder, minus its `colors()` escape hatch: an
/// explicit RGBA palette becomes a semantic [`StyleRef::accent`] here, so a live
/// re-tint reaches this widget like every other — or, where a color really is
/// the meaning, a one-color pin through [`StyleRef::ink`]. Defaults match the
/// kit's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct TextBoxConfig {
    /// The skin to draw in.
    pub style: StyleRef,
    /// How the wrap width is chosen. Default: [`TextBoxWidth::Cols(16)`](TextBoxWidth::Cols).
    /// Cells are capped at [`MAX_TEXT_COLS`], a pixel budget at
    /// [`MAX_BUFFER_DIM`].
    pub width: TextBoxWidth,
    /// Hard cap on wrapped lines; overflow truncates with a trailing `…`.
    /// Default `3`, clamped to `1..=`[`MAX_TEXT_LINES`].
    pub max_lines: u32,
    /// Field padding around the text block, in pre-scale pixels. Default `3`,
    /// capped at [`MAX_PAD`] — it is added to **both** dimensions, so it
    /// inflates the buffer quadratically if left unbounded.
    pub pad: u32,
    /// Radius of the rounded-corner cut (to transparent), in pre-scale pixels;
    /// `0` keeps square corners. Default `2`, capped at [`MAX_CORNER`].
    pub corner: u32,
    /// Integer upscale baked into the buffer. Default `1`; capped at
    /// [`MAX_SCALE`] and pulled lower whenever the box's own width or height
    /// would otherwise push the scaled buffer past [`MAX_BUFFER_DIM`]. `0`
    /// reads as `1`.
    pub scale: u32,
    /// `true` renders the full wrap width even for short text, so the box never
    /// resizes with the message; `false` (the default) hugs the longest line.
    pub fixed_width: bool,
}

impl Default for TextBoxConfig {
    fn default() -> Self {
        Self {
            style: StyleRef::default(),
            width: TextBoxWidth::default(),
            max_lines: 3,
            pad: 3,
            corner: 2,
            scale: 1,
            fixed_width: false,
        }
    }
}

/// **State** for [`PreemWidget::TextBox`] — a change re-wraps and redraws.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct TextBoxState {
    /// The message to display. Capped at [`MAX_TEXT_LEN`].
    pub text: String,
}

// ── LedStrip ────────────────────────────────────────────────────────────────

/// Shell-side peak-hold: the bright dot rides the recent maximum and falls by
/// `rate` per animation tick, the wire form of the kit's `PeakHold`.
///
/// Putting this in **config** rather than state is what makes a steady meter go
/// quiet on the wire: with a peak-hold declared, the shell folds each
/// [`LedStripState::level`] into the held value and decays it on its own pump,
/// so a plugin that has nothing new to say sends nothing at all.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PeakHoldConfig {
    /// Fall per animation tick, in fractions of full scale. The kit clamps a
    /// negative rate to `0.0` (a dot that never falls) — which is also the
    /// default here, so an omitted `rate` gives a peak dot that holds forever
    /// rather than a decode error.
    ///
    /// [`PreemWidget::clamp_in_place`] clamps it to
    /// `0.0..=`[`MAX_PEAK_HOLD_RATE`], matching the kit's own `rate.max(0.0)`
    /// (`crates/hytte-preem/src/led_strip.rs:108`): `NaN` becomes `0.0` because
    /// `f32::max` returns the non-`NaN` side, `-inf` becomes `0.0` for the same
    /// reason, and `+inf` becomes [`MAX_PEAK_HOLD_RATE`] — which decays a hold
    /// identically, since `decay` is `(value - rate).max(0.0)` over a value in
    /// `0.0..=1.0` (`led_strip.rs:127`).
    pub rate: f32,
}

impl Default for PeakHoldConfig {
    /// `0.0` — the kit's "never falls" rate, matching `PeakHold::new`'s
    /// clamp-negative-to-zero behaviour.
    fn default() -> Self {
        Self { rate: 0.0 }
    }
}

/// **Config** for [`PreemWidget::LedStrip`] — a change rebuilds the renderer
/// (and, with it, resets any held peak).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LedStripConfig {
    /// The skin to draw in.
    pub style: StyleRef,
    /// Segment count. Default `24` (the kit's `DEFAULT_LEDS`), clamped to
    /// `1..=`[`MAX_LEDS`].
    pub leds: u32,
    /// Shell-side peak-hold, or `None` for no peak dot unless the plugin
    /// supplies one explicitly via [`LedStripState::peak`]. Default `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_hold: Option<PeakHoldConfig>,
}

impl Default for LedStripConfig {
    fn default() -> Self {
        Self {
            style: StyleRef::default(),
            leds: 24,
            peak_hold: None,
        }
    }
}

/// **State** for [`PreemWidget::LedStrip`] — a change moves the meter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LedStripState {
    /// The level to light, in `0.0..=1.0`. The kit clamps out-of-range values
    /// and reads a `NaN` as rest.
    ///
    /// [`PreemWidget::clamp_in_place`] does the same before the kit ever sees
    /// it, matching `lit_count` (`crates/hytte-preem/src/led_strip.rs:65`,
    /// `level.clamp(0.0, 1.0)` with no guard in front of it): `+inf` becomes
    /// `1.0` (full strip), `-inf` becomes `0.0`, and `NaN` becomes `0.0` —
    /// which is what the kit lights for a `NaN` too, via the saturating
    /// `as usize` cast.
    pub level: f32,
    /// An **explicit** peak-dot position in `0.0..=1.0`, for a plugin that
    /// computes its own peak (a true inter-frame peak off a raw audio block,
    /// say, which the shell's per-render fold cannot see).
    ///
    /// `None` — the default — leaves the peak to
    /// [`LedStripConfig::peak_hold`]; when both are set the explicit value
    /// wins for that render, and the held value is *not* disturbed by it.
    ///
    /// [`PreemWidget::clamp_in_place`] clamps this into `0.0..=1.0` and leaves
    /// it `Some`, matching `peak_led` (`crates/hytte-preem/src/led_strip.rs:80`):
    /// `+inf` becomes `1.0` (the top LED), while `NaN` and `-inf` become `0.0`,
    /// for which the kit draws **no dot** — `Some(0.0)` is how the wire says
    /// that. It is deliberately **not** mapped to `None`: `None` means "no
    /// explicit peak, draw the shell-held decaying one", a different render the
    /// kit would never produce for these inputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak: Option<f32>,
}

// ── Marquee ─────────────────────────────────────────────────────────────────

/// **Config** for [`PreemWidget::Marquee`] — a change rebuilds the renderer
/// (and restarts the scroll from the left).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MarqueeConfig {
    /// The skin to draw in.
    pub style: StyleRef,
    /// Visible window width in **final** buffer pixels. Default `192` (the
    /// kit's `DEFAULT_WINDOW_PX`), capped at [`MAX_BUFFER_DIM`].
    pub window_px: u32,
    /// The blank seam appended after the message before it loops, in **dots**.
    /// Default `6` (the kit's `GLYPH_W + SPACING`).
    pub gap_dots: u32,
    /// Scroll speed in **dots per second** (#882 taste call 2).
    ///
    /// Today a plugin steps whole dots per clock beat — the audio widget's
    /// `MARQUEE_STEP = 1` at its 20 Hz `TICK`, i.e. ≈20 dots/s. Dots-per-second
    /// generalizes that: the shell's pump integrates it against real elapsed
    /// time, so the scroll runs at the same visual speed whatever the frame
    /// rate, and a plugin no longer owns a timer to keep it moving. Default
    /// `20.0`, matching the rate the kit's own consumers already scroll at.
    /// `0.0` parks the message, and so does **any** non-finite value,
    /// infinities included. This is the one float in the vocabulary with **no
    /// kit behaviour to match** — the kit's `Marquee` has no speed at all
    /// (`MarqueeStrip::window(offset)` is caller-driven), so #882 invented the
    /// field and owes it a rule of its own: parked is the safe neutral for an
    /// integrator, rather than a full-speed scroll nobody asked for. This
    /// mapping shipped with #895; see [`PreemWidget::clamp_in_place`].
    pub speed_dots_per_sec: f32,
}

impl Default for MarqueeConfig {
    fn default() -> Self {
        Self {
            style: StyleRef::default(),
            window_px: 192,
            gap_dots: 6,
            speed_dots_per_sec: 20.0,
        }
    }
}

/// **State** for [`PreemWidget::Marquee`] — a change re-rasterises the strip.
///
/// Note what is *absent*: there is no offset. The scroll position is shell-owned
/// animation (see the module docs), so a marquee showing an unchanged title
/// sends nothing at all while it scrolls.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct MarqueeState {
    /// The message to scroll. Capped at [`MAX_TEXT_LEN`].
    pub text: String,
}

// ── Scope ───────────────────────────────────────────────────────────────────

/// **Config** for [`PreemWidget::Scope`] — a change rebuilds the renderer,
/// which clears the phosphor buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct ScopeConfig {
    /// The skin to draw in.
    pub style: StyleRef,
    /// Logical buffer width in columns (pre-upscale). Default `144`, clamped to
    /// `1..=`[`MAX_BUFFER_DIM`].
    pub cols: u32,
    /// Logical buffer height in rows (pre-upscale). Default `48`, clamped to
    /// `1..=`[`MAX_BUFFER_DIM`].
    pub rows: u32,
    /// Integer upscale baked into the output. Default `2`, clamped to
    /// `1..=`[`MAX_SCALE`] and pulled lower whenever `cols`/`rows` would
    /// otherwise push the scaled buffer past [`MAX_BUFFER_DIM`].
    pub scale: u32,
    /// Phosphor persistence: 256ths of beam intensity **retained** per tick.
    /// Default `184` (≈0.72); `256` never fades, `0` clears every tick. The kit
    /// clamps to `0..=256`.
    pub persistence: u16,
}

impl Default for ScopeConfig {
    fn default() -> Self {
        Self {
            style: StyleRef::default(),
            cols: 144,
            rows: 48,
            scale: 2,
            persistence: 184,
        }
    }
}

/// **State** for [`PreemWidget::Scope`] — a fresh sample batch to stamp.
///
/// Unlike the other widgets' state this is *consumed*, not held: each update's
/// samples are stamped at full intensity over the decaying phosphor the shell
/// carries between batches. An empty batch flatlines on the axis while the old
/// trail keeps decaying — so a plugin with nothing to send can simply stop, and
/// the trace fades out the way real phosphor does.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScopeState {
    /// A normalized `-1.0..=1.0` signal; the kit clamps out-of-range and
    /// non-finite values defensively. Capped at [`MAX_SCOPE_SAMPLES`] per
    /// update.
    ///
    /// [`PreemWidget::clamp_in_place`] sanitises the batch element-wise before
    /// the kit sees it, reproducing the kit's own `sanitize`
    /// (`crates/hytte-preem/src/scope.rs:386`) exactly: **every** non-finite
    /// sample — `NaN` and both infinities — becomes `0.0`, the axis, and finite
    /// samples clamp to `-1.0..=1.0`. Note the infinities do *not* go to the
    /// rails: `sanitize` is a guard, not a bare clamp. A single poisoned sample
    /// would otherwise make the whole widget unequal to itself — see the module
    /// docs.
    pub samples: Vec<f32>,
}

// ── Gauge ───────────────────────────────────────────────────────────────────

/// The value scale a [`Gauge`](PreemWidget::Gauge) reads in: `low` at the left
/// end of the dial, `high` at the right.
///
/// The needle physics always runs in fraction-of-scale space, so the spring
/// constants never need re-tuning for a new range. The kit rejects a degenerate
/// (`high <= low`) or non-finite range and keeps the previous one.
///
/// [`PreemWidget::clamp_in_place`] reproduces that: a range that is degenerate
/// (`high <= low`, either end non-finite, or a span so wide it overflows to
/// infinity) is replaced with [`GaugeRange::default`] — `0.0..=1.0`, which is
/// exactly what the kit keeps after rejecting it, since the shell builds a
/// fresh `Needle` per config and that is the value its constructor left there
/// (`crates/hytte-preem/src/gauge.rs:171`, guard at `:210`). The two ends are
/// replaced **as a unit**: mixing a caller's `low` with a default `high` would
/// invent a scale nobody asked for.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GaugeRange {
    /// The value at the low end of the dial. Default `0.0`.
    pub low: f32,
    /// The value at the high end of the dial. Default `1.0`.
    pub high: f32,
}

impl Default for GaugeRange {
    /// The kit's default: a `0.0..=1.0` scale.
    fn default() -> Self {
        Self {
            low: 0.0,
            high: 1.0,
        }
    }
}

/// **Config** for [`PreemWidget::Gauge`] — a change rebuilds the renderer,
/// which resets the needle to rest at the low end.
///
/// The spring constants live here rather than in state on purpose: they are the
/// *instrument's* character, not a reading. A plugin picks them once and then
/// only ever moves [`GaugeState::target`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GaugeConfig {
    /// The skin to draw in.
    pub style: StyleRef,
    /// Logical buffer width in columns (pre-upscale). Default `144`, clamped to
    /// `1..=`[`MAX_BUFFER_DIM`].
    pub cols: u32,
    /// Logical buffer height in rows (pre-upscale). Default `64`, clamped to
    /// `1..=`[`MAX_BUFFER_DIM`].
    pub rows: u32,
    /// Integer upscale baked into the output. Default `2`, clamped to
    /// `1..=`[`MAX_SCALE`] and pulled lower whenever `cols`/`rows` would
    /// otherwise push the scaled buffer past [`MAX_BUFFER_DIM`].
    pub scale: u32,
    /// Total sweep of the scale, in **degrees**. Default `150.0`; the kit
    /// clamps to [`MIN_SWEEP_DEG`]`..=`[`MAX_SWEEP_DEG`] behind an
    /// `is_finite` guard (`crates/hytte-preem/src/gauge.rs:598`), so **any**
    /// non-finite sweep keeps the value already there — the kit default on the
    /// fresh builder the shell constructs per config.
    /// [`PreemWidget::clamp_in_place`] reproduces exactly that: finite values
    /// clamp, and `NaN`, `+inf` and `-inf` all resolve to `150.0`.
    pub sweep_deg: f32,
    /// Major divisions (intervals between long ticks). Default `4`, clamped to
    /// `1..=`[`MAX_DIVISIONS`].
    pub divisions: u32,
    /// Minor ticks per major division. Default `5`, clamped to
    /// `1..=`[`MAX_SUBDIVISIONS`]. The gauge rasterises `divisions *
    /// subdivisions` ticks **per frame**, so the two caps are chosen as a
    /// product — see [`MAX_SUBDIVISIONS`].
    pub subdivisions: u32,
    /// The value scale the caller reads in. Default `0.0..=1.0`.
    pub range: GaugeRange,
    /// Undamped natural frequency in Hz — how *fast* the needle swings. Default
    /// `2.0`; the kit clamps to [`MIN_FREQUENCY_HZ`]`..=`[`MAX_FREQUENCY_HZ`]
    /// behind an `is_finite` guard (`crates/hytte-preem/src/gauge.rs:181`), so
    /// every non-finite value keeps the default.
    /// [`PreemWidget::clamp_in_place`] reproduces it: `NaN`, `+inf` and `-inf`
    /// all resolve to `2.0`.
    pub frequency_hz: f32,
    /// Damping ratio `ζ` — how much the needle *overshoots*. Default `0.5`
    /// (half critical: one obvious kick past the reading, then settle); the kit
    /// clamps to [`MIN_DAMPING`]`..=`[`MAX_DAMPING`] behind an `is_finite`
    /// guard (`crates/hytte-preem/src/gauge.rs:195`), so every non-finite value
    /// keeps the default. [`PreemWidget::clamp_in_place`] reproduces it: `NaN`,
    /// `+inf` and `-inf` all resolve to `0.5`.
    pub damping: f32,
}

impl Default for GaugeConfig {
    fn default() -> Self {
        Self {
            style: StyleRef::default(),
            cols: 144,
            rows: 64,
            scale: 2,
            sweep_deg: DEFAULT_SWEEP_DEG,
            divisions: 4,
            subdivisions: 5,
            range: GaugeRange::default(),
            frequency_hz: DEFAULT_FREQUENCY_HZ,
            damping: DEFAULT_DAMPING,
        }
    }
}

/// **State** for [`PreemWidget::Gauge`] — a change points the needle somewhere
/// new; the shell's spring integration walks it there.
///
/// Note what is *absent*: there is no needle position or velocity. Those are
/// shell-owned animation (see the module docs), so a settled gauge sends
/// nothing while it swings and settles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GaugeState {
    /// Where the needle should head, in [`GaugeConfig::range`].
    ///
    /// [`PreemWidget::clamp_in_place`] clamps an out-of-range value to the
    /// nearer end of the (already sanitised) range, saturates `+inf` to
    /// `range.high` and `-inf` to `range.low`, and **replaces** a `NaN` with
    /// `range.low`.
    ///
    /// This is the vocabulary's **one documented divergence** from the kit.
    /// `Needle::set_target` (`crates/hytte-preem/src/gauge.rs:223`) ignores
    /// `NaN`, `+inf` and `-inf` alike — it draws no distinction between them —
    /// and keeps the *live needle's* current target: genuinely stateful, and
    /// the only guarded kit entry point whose kept value a stateless clamp
    /// cannot reconstruct (the config knobs' kept value is just their default).
    /// So this row picks its own images rather than matching the kit: the
    /// infinities saturate to the ends of the range and `NaN` takes the low
    /// end, because a stateless clamp has to name *some* finite value. A plugin
    /// that hand-writes a `NaN` here gets the needle parked at the bottom of
    /// the scale, drawn. (The `hytte-plugin` SDK keeps the kit's keep-previous
    /// behaviour in its own setters; this is the wire's floor, not a
    /// replacement for it.)
    pub target: f32,
}

// ── FlipBoard ───────────────────────────────────────────────────────────────

/// How a [`FlipBoard`](PreemWidget::FlipBoard) cell replaces one character with
/// the next — the wire form of the kit's `Mechanism`.
///
/// Orthogonal to [`StyleName`]: the skin is the panel's palette and post-pass,
/// the mechanism is the moving part, and every skin renders every mechanism.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Mechanism {
    /// The airport board: the upper card hinges down over the lower one, and
    /// cells ripple left to right.
    #[default]
    SplitFlap,
    /// The glow tube: the outgoing cathode collapses while the incoming one
    /// strikes, simultaneously across the whole row.
    Nixie,
}

impl Mechanism {
    /// Every mechanism, in the kit's canonical rotation order.
    pub const ALL: [Self; 2] = [Self::SplitFlap, Self::Nixie];

    /// The lowercase word form (`"split-flap"` / `"nixie"`), matching
    /// `Mechanism::name` in the kit.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::SplitFlap => "split-flap",
            Self::Nixie => "nixie",
        }
    }
}

/// **Config** for [`PreemWidget::FlipBoard`] — a change rebuilds the board,
/// which resets every cell blank.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlipBoardConfig {
    /// The skin to draw in.
    pub style: StyleRef,
    /// How a cell changes character. Default [`Mechanism::SplitFlap`].
    pub mechanism: Mechanism,
    /// The board's *physical* width in character cells; a short string is
    /// padded with blanks and anything past the last cell is ignored. Default
    /// `8` (`HH:MM:SS`), clamped to `1..=`[`MAX_CELLS`].
    pub cells: u32,
    /// Logical pixels per font pixel. Default `2`; the kit clamps to `2..=16`
    /// and rounds **down to an even number** (the hinge cuts a 7-row glyph
    /// through the middle of its centre row, which only lands on a pixel
    /// boundary for even values).
    pub glyph_px: u32,
    /// Integer upscale baked into the output. Default `2`, clamped to
    /// `1..=`[`MAX_SCALE`] and pulled lower whenever `cols`/`rows` would
    /// otherwise push the scaled buffer past [`MAX_BUFFER_DIM`].
    pub scale: u32,
    /// Per-cell transition length in seconds, or `None` for the
    /// **mechanism's own** default (`0.38` for a split flap, `0.30` for a
    /// nixie cross-fade). The kit clamps to `0.01..=10.0`.
    ///
    /// `Option` rather than a bare `f32` precisely because the default is not a
    /// constant — it depends on [`mechanism`](FlipBoardConfig::mechanism), which
    /// a struct-level default cannot see. That is also why
    /// [`PreemWidget::clamp_in_place`] maps a non-finite duration back to
    /// `None`: the kit clamps behind an `is_finite` guard
    /// (`crates/hytte-preem/src/split_flap.rs:368`), so a non-finite value
    /// leaves the mechanism's own default in place — and `None` is exactly how
    /// this field spells that default, which makes the mapping a match rather
    /// than an approximation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f32>,
    /// Per-cell left-to-right stagger in seconds, or `None` for the
    /// **mechanism's own** default (`0.055` for a split flap; `0.0` for a
    /// nixie, whose tubes are wired in parallel and switch together). `0.0`
    /// makes the whole row move as one. The kit clamps to
    /// `0.0..=`[`MAX_FLIP_STAGGER_SECS`] behind the same `is_finite` guard
    /// (`crates/hytte-preem/src/split_flap.rs:380`), and
    /// [`PreemWidget::clamp_in_place`] maps a non-finite stagger back to `None`
    /// for the same reason as [`duration_secs`](FlipBoardConfig::duration_secs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stagger_secs: Option<f32>,
}

impl Default for FlipBoardConfig {
    fn default() -> Self {
        Self {
            style: StyleRef::default(),
            mechanism: Mechanism::SplitFlap,
            cells: 8,
            glyph_px: 2,
            scale: 2,
            duration_secs: None,
            stagger_secs: None,
        }
    }
}

/// **State** for [`PreemWidget::FlipBoard`] — a change starts the cells that
/// differ flipping toward the new characters.
///
/// Note what is *absent*: there are no per-cell clocks or a board time. Those
/// are shell-owned animation (see the module docs), so a board sends one frame
/// per content change and nothing at all while the cards are in motion.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct FlipBoardState {
    /// The content to flip to. Folded onto the board's drum (uppercased;
    /// uncovered characters land on one shared notdef card), padded with blanks
    /// and truncated to [`FlipBoardConfig::cells`]. Capped at [`MAX_TEXT_LEN`].
    pub text: String,
}

// ── the widget enum ─────────────────────────────────────────────────────────

/// The preem widget vocabulary — the payload of
/// [`Node::Preem`](crate::wire::Node::Preem).
///
/// One wrapper variant in [`Node`](crate::wire::Node) rather than eight flat
/// ones (#882 taste call 3): the whole preem vocabulary then versions as a
/// single unit, the host dispatches from one `wire_map` arm, and appending a
/// ninth widget touches this enum instead of the shell's node vocabulary.
///
/// Each variant carries its `config` and its `state` as **separate named
/// structs** rather than one flat field soup, because the distinction is
/// load-bearing at render time — see the module docs. On the wire that is a
/// two-key map inside the variant's map, i.e.
/// `{"DotMatrix": {"config": {…}, "state": {…}}}`; both inner structs carry a
/// container-level `#[serde(default)]`, so **any** field a peer omits decodes to
/// the kit's own default and adding a config field later stays additive without
/// a per-field attribute.
///
/// **Unknown variants.** A host that meets a widget it does not know must render
/// a placeholder and warn, never drop the session — but note that the
/// negotiation contract (see the module docs) means it should not be able to:
/// a plugin only emits `Preem` at all once the host advertised
/// [`PREEM_VOCAB`](crate::preem::PREEM_VOCAB) or better.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PreemWidget {
    /// Static 5×7 dot-matrix text.
    DotMatrix {
        /// Rebuild-on-change knobs.
        config: DotMatrixConfig,
        /// The current target.
        state: DotMatrixState,
    },
    /// A seven-segment readout.
    SevenSeg {
        /// Rebuild-on-change knobs.
        config: SevenSegConfig,
        /// The current target.
        state: SevenSegState,
    },
    /// Wrapped pixel-font text on a rounded, opaque field.
    TextBox {
        /// Rebuild-on-change knobs.
        config: TextBoxConfig,
        /// The current target.
        state: TextBoxState,
    },
    /// A segmented level meter with an optional peak-hold dot.
    LedStrip {
        /// Rebuild-on-change knobs.
        config: LedStripConfig,
        /// The current target.
        state: LedStripState,
    },
    /// A scrolling dot-matrix ticker.
    Marquee {
        /// Rebuild-on-change knobs.
        config: MarqueeConfig,
        /// The current target.
        state: MarqueeState,
    },
    /// A glow-trace oscilloscope with phosphor persistence.
    Scope {
        /// Rebuild-on-change knobs.
        config: ScopeConfig,
        /// The current sample batch.
        state: ScopeState,
    },
    /// A needle gauge with damped spring physics.
    Gauge {
        /// Rebuild-on-change knobs.
        config: GaugeConfig,
        /// The current target.
        state: GaugeState,
    },
    /// A split-flap board / nixie readout.
    FlipBoard {
        /// Rebuild-on-change knobs.
        config: FlipBoardConfig,
        /// The current target.
        state: FlipBoardState,
    },
}

impl PreemWidget {
    /// The widget kind as a lowercase word — handy for host logs and CSS class
    /// suffixes, and the one place a host can name a widget without matching.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::DotMatrix { .. } => "dot-matrix",
            Self::SevenSeg { .. } => "seven-seg",
            Self::TextBox { .. } => "text-box",
            Self::LedStrip { .. } => "led-strip",
            Self::Marquee { .. } => "marquee",
            Self::Scope { .. } => "scope",
            Self::Gauge { .. } => "gauge",
            Self::FlipBoard { .. } => "flip-board",
        }
    }

    /// The skin this widget asked for — the field every variant shares.
    #[must_use]
    pub fn style(&self) -> StyleRef {
        match self {
            Self::DotMatrix { config, .. } => config.style,
            Self::SevenSeg { config, .. } => config.style,
            Self::TextBox { config, .. } => config.style,
            Self::LedStrip { config, .. } => config.style,
            Self::Marquee { config, .. } => config.style,
            Self::Scope { config, .. } => config.style,
            Self::Gauge { config, .. } => config.style,
            Self::FlipBoard { config, .. } => config.style,
        }
    }

    /// Enforce every [wire limit](crate::preem#wire-limits), returning the
    /// normalized widget.
    ///
    /// This is the shared enforcement point, deliberately in the protocol crate
    /// rather than hand-rolled in the host: it is the preem analogue of the
    /// `Node::Pixels` `len == w * h * 4` check, and like that check its job is
    /// to make a malformed or hostile plugin *harmless*, not to make it fail.
    /// Nothing is rejected — text truncates (on a char boundary, never
    /// mid-codepoint), sample batches truncate, and counts/dimensions clamp into
    /// range — so a bad frame renders a sane widget instead of dropping the
    /// session.
    ///
    /// [`StyleRef`] is the one config field with nothing to enforce: a
    /// [`StyleName`] and an [`AccentRole`] are closed enums, and an
    /// [`ink`](StyleRef::ink) is four `u8`s where every bit pattern is already a
    /// color. It passes through untouched, and cannot make `clamped(w)` differ
    /// from `clamped(w)`.
    ///
    /// It also **sanitises every float**, config and state alike: after this
    /// runs, every `f32` a renderer can reach is finite and inside its
    /// documented bounds, so a clamped widget always compares equal to itself
    /// under the derived `PartialEq`. See [`clamp_in_place`](Self::clamp_in_place)
    /// for the per-field mapping and why that equality is load-bearing.
    #[must_use]
    pub fn clamped(mut self) -> Self {
        self.clamp_in_place();
        self
    }

    /// [`clamped`](Self::clamped) in place, for a caller that already owns the
    /// widget mutably.
    ///
    /// The render path wants this one: once #883 rasterises real widgets, the
    /// owning form would cost a clone of every `String`/`Vec` in the config on
    /// **every frame**, purely to hand the clamp something to consume.
    ///
    /// # The float invariant
    ///
    /// Afterwards, **every** `f32` this widget carries — in `config` and in
    /// `state` — is finite and within its documented bounds. That makes the
    /// derived `PartialEq` on [`PreemWidget`] a usable *did anything change?*
    /// test for both ends' render dedup: `NaN != NaN`, so before this a single
    /// poisoned reading made a widget unequal to an identical copy of itself,
    /// and both the SDK's `view != last_view` and the host's
    /// `applied == widget` gate then spun forever (see the module docs). The
    /// routine is also a **fixpoint**: clamping a clamped widget changes
    /// nothing.
    ///
    /// # The mapping — contract, not implementation detail
    ///
    /// **The rule: `±inf` saturates exactly as the kit's own clamp would,
    /// because the kit's clamp is total on `±inf`; `NaN` alone gets the
    /// documented per-field constant (or `None`), because the kit's `NaN`
    /// behaviour — where it is keep-previous — is stateful and unreachable from
    /// a stateless clamp.** Where the kit is total on `NaN` too, the constant
    /// **is** the kit's value rather than an invention of this crate.
    ///
    /// The consequence is parity: a plugin rasterising through the kit (the
    /// `Raster` arm, an older shell) and a plugin stating the same widget on the
    /// wire (the `Preem` arm) agree on every input the kit defines a value for.
    ///
    /// Some kit entry points are not a bare `clamp` but a guard —
    /// `if value.is_finite() { … }`, which keeps the *current* value for every
    /// non-finite input, infinities included. For a **config** field that is
    /// still reachable: the shell builds a fresh kit renderer from the config,
    /// so "the current value" is that field's kit default, and this clamp
    /// reproduces it exactly. The one place it is **not** reachable is
    /// [`GaugeState::target`] — the kit keeps a *live needle's* target
    /// (`crates/hytte-preem/src/gauge.rs:223`), which no stateless clamp can
    /// see. That single row is the documented divergence, and it is worth being
    /// exact about what diverges: the kit ignores `+inf` and `-inf` there
    /// **exactly** as it ignores `NaN`, drawing no distinction between the
    /// three. Saturating the infinities to the ends of the range is therefore
    /// this vocabulary's own rule, chosen because a stateless clamp has to name
    /// *some* finite value and the ends are the least surprising ones — not a
    /// parity claim.
    ///
    /// Wherever the replacement is drawn rather than ignored, it really is
    /// drawn — a plugin that hand-writes a `NaN` gets the replacement on glass.
    /// (Plugins built on the `hytte-plugin` SDK keep the kit's keep-previous
    /// behaviour in the SDK's own setters; this is the wire's floor underneath
    /// that, for peers in every language.)
    ///
    /// | field | kit behaviour (`crates/hytte-preem/…`) | bounds | `NaN` | `+inf` | `-inf` |
    /// |---|---|---|---|---|---|
    /// | [`PeakHoldConfig::rate`] | `led_strip.rs:108` `rate.max(0.0)` — total | `0.0..=`[`MAX_PEAK_HOLD_RATE`] | `0.0` (`f32::max` returns the non-`NaN` side) | [`MAX_PEAK_HOLD_RATE`] | `0.0` |
    /// | [`LedStripState::level`] | `led_strip.rs:65` `lit_count` — total | `0.0..=1.0` | `0.0` (the kit lights nothing) | `1.0` | `0.0` |
    /// | [`LedStripState::peak`] | `led_strip.rs:80` `peak_led` — total | `0.0..=1.0`, stays `Some` | `Some(0.0)` (no dot) | `Some(1.0)` (top LED) | `Some(0.0)` (no dot) |
    /// | [`MarqueeConfig::speed_dots_per_sec`] | none — the kit has no speed | `±`[`MAX_MARQUEE_SPEED_DPS`] | `0.0` | `0.0` | `0.0` |
    /// | [`ScopeState::samples`] (each) | `scope.rs:386` `sanitize` — guarded | `-1.0..=1.0` | `0.0` (the axis) | `0.0` | `0.0` |
    /// | [`GaugeRange::low`] / [`high`](GaugeRange::high) | `gauge.rs:210` `Needle::range` — guarded | `high > low`, finite span | [`GaugeRange::default`], both ends as a unit | same | same |
    /// | [`GaugeConfig::sweep_deg`] | `gauge.rs:598` — guarded | [`MIN_SWEEP_DEG`]`..=`[`MAX_SWEEP_DEG`] | `150.0` (the kit default) | `150.0` | `150.0` |
    /// | [`GaugeConfig::frequency_hz`] | `gauge.rs:181` — guarded | [`MIN_FREQUENCY_HZ`]`..=`[`MAX_FREQUENCY_HZ`] | `2.0` (the kit default) | `2.0` | `2.0` |
    /// | [`GaugeConfig::damping`] | `gauge.rs:195` — guarded | [`MIN_DAMPING`]`..=`[`MAX_DAMPING`] | `0.5` (the kit default) | `0.5` | `0.5` |
    /// | [`GaugeState::target`] | `gauge.rs:223` `set_target` — guarded, **stateful** | the sanitised [`GaugeConfig::range`] | `range.low` | `range.high` | `range.low` |
    /// | [`FlipBoardConfig::duration_secs`] | `split_flap.rs:368` — guarded | [`MIN_FLIP_DURATION_SECS`]`..=`[`MAX_FLIP_DURATION_SECS`] | `None` | `None` | `None` |
    /// | [`FlipBoardConfig::stagger_secs`] | `split_flap.rs:380` — guarded | `0.0..=`[`MAX_FLIP_STAGGER_SECS`] | `None` | `None` | `None` |
    ///
    /// Four rows deserve their reasoning spelled out:
    ///
    /// - [`LedStripState::peak`] never becomes `None`. `None` means *no
    ///   explicit peak — draw the shell-held decaying one*, a different render
    ///   the kit would never produce for these inputs; `peak_led` draws no dot
    ///   for a `NaN` or a non-positive peak, and `Some(0.0)` is how the wire
    ///   says "no dot".
    /// - [`ScopeState::samples`] reads `±inf` as `0.0`, not as the rails: the
    ///   kit's `sanitize` is a guard, not a clamp, and maps every non-finite
    ///   sample to the axis.
    /// - [`MarqueeConfig::speed_dots_per_sec`] parks on **any** non-finite
    ///   value. There is no kit behaviour to match — the kit's `Marquee` has no
    ///   speed at all, #882 invented the field — so the rule has nothing to say
    ///   here, and parked is the safe neutral for an integrator rather than a
    ///   full-speed scroll nobody asked for. This mapping shipped with #895.
    /// - [`FlipBoardConfig::duration_secs`] / [`stagger_secs`](FlipBoardConfig::stagger_secs)
    ///   go to `None` because the kit keeps the *mechanism's* default for a
    ///   non-finite input, and `None` is precisely how this vocabulary spells
    ///   that default — matching the kit rather than approximating it.
    ///
    /// Order matters in one place: [`GaugeConfig::range`] is sanitised
    /// **before** [`GaugeState::target`] is clamped into it, so the target can
    /// never be clamped against a `NaN` bound (which also keeps `f32::clamp`
    /// from ever seeing `lo > hi`).
    pub fn clamp_in_place(&mut self) {
        match self {
            // A dot-matrix / seven-segment readout lays its whole message out
            // on one line, so the *character count* is a buffer dimension and
            // MAX_TEXT_LEN alone does not bound it — see `clamp_strip_text`.
            Self::DotMatrix { state, .. } => {
                clamp_strip_text(&mut state.text, DOT_MATRIX_PITCH_PX);
            }
            Self::SevenSeg { state, .. } => {
                clamp_strip_text(&mut state.text, SEVEN_SEG_PITCH_PX);
            }
            Self::TextBox { config, state } => {
                config.pad = config.pad.min(MAX_PAD);
                config.corner = config.corner.min(MAX_CORNER);
                config.max_lines = config.max_lines.clamp(1, MAX_TEXT_LINES);
                config.width = match config.width {
                    TextBoxWidth::Cols(n) => TextBoxWidth::Cols(n.clamp(1, MAX_TEXT_COLS)),
                    TextBoxWidth::FitPx(px) => TextBoxWidth::FitPx(px.clamp(1, MAX_BUFFER_DIM)),
                };
                // The box's logical width is `2*pad + cols*CELL_PITCH`; fit the
                // upscale to it so the *final* buffer stays inside the cap.
                let logical_w = match config.width {
                    TextBoxWidth::Cols(n) => 2 * config.pad + n * TEXT_CELL_PITCH_PX,
                    // A `FitPx` budget is already stated in final pixels, so it
                    // bounds itself — but the padding still rides on top of it.
                    TextBoxWidth::FitPx(px) => px + 2 * config.pad,
                };
                let logical_h = 2 * config.pad + config.max_lines * TEXT_LINE_PITCH_PX;
                config.scale = fit_scale(logical_w.max(logical_h), config.scale);
                clamp_text(&mut state.text);
            }
            Self::LedStrip { config, state } => {
                config.leds = config.leds.clamp(1, MAX_LEDS);
                if let Some(hold) = config.peak_hold.as_mut() {
                    hold.rate = clamp_reading(hold.rate, 0.0, MAX_PEAK_HOLD_RATE, 0.0);
                }
                state.level = clamp_reading(state.level, 0.0, 1.0, 0.0);
                // An explicit peak stays *explicit*: `None` would hand the
                // render to the shell-held decaying peak, which is not what the
                // kit draws for any of these inputs.
                state.peak = state.peak.map(|peak| clamp_reading(peak, 0.0, 1.0, 0.0));
            }
            Self::Marquee { config, state } => {
                // `window_px` is already the *final* buffer width (the kit takes
                // it in post-scale pixels and the marquee carries no `scale`),
                // so capping it directly is the whole bound on this axis.
                config.window_px = config.window_px.clamp(1, MAX_BUFFER_DIM);
                config.gap_dots = config.gap_dots.min(MAX_GAP_DOTS);
                config.speed_dots_per_sec = if config.speed_dots_per_sec.is_finite() {
                    config
                        .speed_dots_per_sec
                        .clamp(-MAX_MARQUEE_SPEED_DPS, MAX_MARQUEE_SPEED_DPS)
                } else {
                    // A NaN/inf speed would poison the shell's offset
                    // integrator; park the message instead.
                    0.0
                };
                clamp_text(&mut state.text);
            }
            Self::Scope { config, state } => {
                config.cols = config.cols.clamp(1, MAX_BUFFER_DIM);
                config.rows = config.rows.clamp(1, MAX_BUFFER_DIM);
                config.scale = fit_scale(config.cols.max(config.rows), config.scale);
                state.samples.truncate(MAX_SCOPE_SAMPLES);
                for sample in &mut state.samples {
                    *sample = clamp_guarded(*sample, -1.0, 1.0, 0.0);
                }
            }
            Self::Gauge { config, state } => {
                config.cols = config.cols.clamp(1, MAX_BUFFER_DIM);
                config.rows = config.rows.clamp(1, MAX_BUFFER_DIM);
                config.scale = fit_scale(config.cols.max(config.rows), config.scale);
                config.divisions = config.divisions.clamp(1, MAX_DIVISIONS);
                config.subdivisions = config.subdivisions.clamp(1, MAX_SUBDIVISIONS);
                config.sweep_deg = clamp_guarded(
                    config.sweep_deg,
                    MIN_SWEEP_DEG,
                    MAX_SWEEP_DEG,
                    DEFAULT_SWEEP_DEG,
                );
                config.frequency_hz = clamp_guarded(
                    config.frequency_hz,
                    MIN_FREQUENCY_HZ,
                    MAX_FREQUENCY_HZ,
                    DEFAULT_FREQUENCY_HZ,
                );
                config.damping =
                    clamp_guarded(config.damping, MIN_DAMPING, MAX_DAMPING, DEFAULT_DAMPING);
                // The range first: a target is only meaningful against a scale,
                // and clamping against a `NaN` bound would just re-poison it.
                config.range = sane_range(config.range);
                state.target = clamp_reading(
                    state.target,
                    config.range.low,
                    config.range.high,
                    config.range.low,
                );
            }
            Self::FlipBoard { config, state } => {
                config.cells = config.cells.clamp(1, MAX_CELLS);
                // A board's width is `(cells*CARD_PITCH + BEZEL) * glyph_px *
                // scale` — two multipliers stacked on a count, so both have to
                // come down for a wide board. `glyph_px` first (it has a hard
                // floor of 2 and must stay even, so it can only absorb so much),
                // then `scale` against whatever width that left.
                //
                // Fit on the **larger** of the two axes, not just width: a
                // one-cell board is 10 font-px wide but 11 tall, so a
                // width-only fit leaves the height unchecked and correct only
                // by accident of aspect ratio.
                let board_w_fpx = config.cells * FLIP_CARD_PITCH_FPX + FLIP_BEZEL_FPX;
                let board_fontpx = board_w_fpx.max(FLIP_BOARD_HEIGHT_FPX);
                let glyph_ceiling = (MAX_BUFFER_DIM / board_fontpx.max(1)).clamp(2, 16);
                config.glyph_px = config.glyph_px.clamp(2, 16).min(glyph_ceiling) / 2 * 2;
                config.scale = fit_scale(board_fontpx * config.glyph_px, config.scale);
                config.duration_secs = clamp_optional_reading(
                    config.duration_secs,
                    MIN_FLIP_DURATION_SECS,
                    MAX_FLIP_DURATION_SECS,
                );
                config.stagger_secs =
                    clamp_optional_reading(config.stagger_secs, 0.0, MAX_FLIP_STAGGER_SECS);
                clamp_text(&mut state.text);
            }
        }
    }
}

/// Font cell pitch in the kit's 5×7 bitmap font: `GLYPH_W + SPACING`. Used only
/// to bound a [`TextBox`](PreemWidget::TextBox)'s width — see [`fit_scale`].
const TEXT_CELL_PITCH_PX: u32 = 6;
/// Wrapped-line pitch: `GLYPH_H + LINE_GAP`.
const TEXT_LINE_PITCH_PX: u32 = 9;
/// A [`FlipBoard`](PreemWidget::FlipBoard) card's horizontal pitch in **font
/// pixels**: the glyph plus its card padding and inter-card gap.
const FLIP_CARD_PITCH_FPX: u32 = 8;
/// The board's bezel, in font pixels (both edges).
const FLIP_BEZEL_FPX: u32 = 2;
/// The board's height in font pixels: the glyph plus card padding and bezel.
/// A short board is taller than it is wide, so the fit has to see this axis.
const FLIP_BOARD_HEIGHT_FPX: u32 = 11;

/// Per-character horizontal advance of a [`DotMatrix`](PreemWidget::DotMatrix),
/// in **buffer pixels**.
///
/// `(GLYPH_W + SPACING) * DOT` = 6 × 4 = 24. The `DOT` factor is the trap: the
/// kit does not render one buffer pixel per font pixel — every font pixel
/// becomes a `DOT`×`DOT` round dot (`dot_matrix.rs:30-31`), so reasoning at the
/// bare 6 px font pitch under-counts a strip's width by **4×**.
const DOT_MATRIX_PITCH_PX: u32 = 24;

/// Per-character horizontal advance of a [`SevenSeg`](PreemWidget::SevenSeg),
/// in **buffer pixels**.
///
/// `DIGIT_W + GAP` = 30 + 10 (`seven_seg.rs:26,32`). A seven-segment readout
/// shares *nothing* with the font grid — not the cell size, not the padding,
/// not the height — so it needs its own number rather than a font-derived one.
/// The digit is the widest cell, so this is the worst-case pitch.
const SEVEN_SEG_PITCH_PX: u32 = 40;

/// Truncate a **single-line strip**'s text so its rendered width stays inside
/// [`MAX_STRIP_DIM`], on top of the [`MAX_TEXT_LEN`] byte cap.
///
/// [`MAX_TEXT_LEN`] alone cannot bound these: a dot matrix and a seven-segment
/// readout lay the entire message out on one line, so the *character count* is
/// a buffer dimension. At 2048 characters that is a 49 156 px dot-matrix strip
/// and an 81 926 × 70 seven-segment one — the latter 1.37× past
/// [`MAX_RASTER_PIXELS`], i.e. ~2 KB of wire becoming ~23 MB of RGBA.
///
/// Capping *here* rather than lowering [`MAX_TEXT_LEN`] keeps the byte cap
/// generous for the widgets that wrap or truncate on their own terms (the
/// [`TextBox`](PreemWidget::TextBox) wraps to `max_lines`, the
/// [`FlipBoard`](PreemWidget::FlipBoard) ignores anything past its last cell).
fn clamp_strip_text(text: &mut String, pitch_px: u32) {
    clamp_text(text);
    let max_chars = usize::try_from(MAX_STRIP_DIM / pitch_px).unwrap_or(usize::MAX);
    if text.chars().count() > max_chars {
        let end = text
            .char_indices()
            .nth(max_chars)
            .map_or(text.len(), |(i, _)| i);
        text.truncate(end);
    }
}

/// Clamp an upscale factor to **both** rules at once: at most [`MAX_SCALE`],
/// and small enough that `logical_dim * scale` stays within
/// [`MAX_BUFFER_DIM`]. At least `1`.
///
/// Both, not either — that is the whole lesson of the bug this replaced.
/// Applying only the dimension rule lets a 16×16 buffer take a 128× upscale;
/// applying only [`MAX_SCALE`] lets a 2048×2048 buffer take an 8× one. Neither
/// alone bounds the product.
///
/// The preem counterpart of the shell's `clamp_pixels_scale` for
/// [`Node::Pixels`](crate::wire::Node::Pixels) — the same shape for the same
/// reason: capping a dimension and its multiplier independently bounds neither.
///
/// The geometry constants it is used with (`TEXT_CELL_PITCH_PX` and friends)
/// mirror the kit's published font/card metrics. They are **upper-bound
/// estimates for allocation safety only**, not a layout model: this crate can't
/// depend on `hytte-preem` (it is the language-neutral schema anchor), so the
/// renderer in #883 still does the exact sizing against the real kit. Getting
/// these slightly wrong costs a widget that clamps a little early or late; it
/// cannot reintroduce the unbounded case, because [`MAX_BUFFER_DIM`] is applied
/// to the result either way.
fn fit_scale(logical_dim: u32, scale: u32) -> u32 {
    if logical_dim == 0 {
        return 1;
    }
    let by_dimension = (MAX_BUFFER_DIM / logical_dim).max(1);
    scale.clamp(1, MAX_SCALE.min(by_dimension))
}

/// Truncate `text` to at most [`MAX_TEXT_LEN`] bytes, cutting at the nearest
/// char boundary at or below the cap — `String::truncate` panics on a split
/// codepoint, and a wire cap must never be able to panic the shell.
fn clamp_text(text: &mut String) {
    if text.len() <= MAX_TEXT_LEN {
        return;
    }
    let mut end = MAX_TEXT_LEN;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

/// Sanitise a float the kit clamps **totally**, i.e. hands straight to
/// `f32::clamp` with no `is_finite` guard in front of it.
///
/// `f32::clamp` is already total on the infinities — `+inf` gives `hi`, `-inf`
/// gives `lo` — so passing them through it reproduces the kit's own answer
/// exactly. `NaN` is the one input `clamp` does not decide (it passes it
/// through), so the caller names its image; where the kit is total on `NaN`
/// too, `on_nan` is the kit's value, not an invention. See the table on
/// [`PreemWidget::clamp_in_place`].
fn clamp_reading(value: f32, lo: f32, hi: f32, on_nan: f32) -> f32 {
    if value.is_nan() {
        on_nan
    } else {
        value.clamp(lo, hi)
    }
}

/// Sanitise a float the kit **guards** — `if value.is_finite() { … }`, keeping
/// its current value for every non-finite input, infinities included.
///
/// A finite value clamps; all three non-finite ones take `on_non_finite`, which
/// is what "keep the current one" resolves to for this field on the fresh
/// builder the shell constructs from a config. Reproducing that keeps the state
/// path and the raster path in step; saturating instead would make the two
/// disagree on the same input.
///
/// With `(-1.0, 1.0, 0.0)` this *is* the kit's `sanitize`
/// (`crates/hytte-preem/src/scope.rs:386`), byte for byte.
fn clamp_guarded(value: f32, lo: f32, hi: f32, on_non_finite: f32) -> f32 {
    if value.is_finite() {
        value.clamp(lo, hi)
    } else {
        on_non_finite
    }
}

/// [`clamp_guarded`] for an optional field whose "keep the current one" is
/// spelled `None` on the wire.
///
/// The flip-board timings are the only fields shaped this way: the kit keeps
/// the *mechanism's* default for a non-finite input
/// (`crates/hytte-preem/src/split_flap.rs:368`, `:380`), and `None` is exactly
/// how this vocabulary spells that default — so the mapping matches the kit
/// rather than approximating it.
fn clamp_optional_reading(value: Option<f32>, lo: f32, hi: f32) -> Option<f32> {
    value.filter(|v| v.is_finite()).map(|v| v.clamp(lo, hi))
}

/// A usable [`GaugeRange`], replacing a degenerate one with
/// [`GaugeRange::default`].
///
/// Degenerate means any of: an end that is not finite, `high <= low` (the
/// needle physics divides by the span), or a span so wide it overflows to
/// infinity — `low = -3.4e38, high = 3.4e38` is two finite numbers whose
/// difference is not, and the fraction-of-scale the kit integrates in would
/// come out `NaN` all the same.
///
/// Both ends are replaced together: mixing a caller's `low` with a default
/// `high` invents a scale nobody asked for.
fn sane_range(range: GaugeRange) -> GaugeRange {
    let span = range.high - range.low;
    if range.low.is_finite() && range.high.is_finite() && span.is_finite() && span > 0.0 {
        range
    } else {
        GaugeRange::default()
    }
}

/// A [`Node::Preem`](crate::wire::Node::Preem) wrapping `widget`, with no id and
/// no classes.
///
/// **Prefer [`preem_id`].** An anonymous preem node is keyed by its ordinal
/// among the un-id'd preem nodes of its tree, which costs a host warning and —
/// the moment an anonymous sibling is inserted or removed — moves this widget's
/// phosphor / needle / flip clocks onto a different node (#900). This one-liner
/// is fine for a lone static readout in a fixed tree and wrong for anything
/// else; the [`Node::Preem`](crate::wire::Node::Preem) docs spell out why.
#[must_use]
pub fn preem(widget: PreemWidget) -> crate::wire::Node {
    crate::wire::Node::Preem {
        id: None,
        classes: Vec::new(),
        widget: Box::new(widget),
    }
}

/// A [`Node::Preem`](crate::wire::Node::Preem) with a stable [`NodeId`], so the
/// host reconciler keeps the *same* renderer instance across renders — which is
/// what preserves the phosphor, the needle's momentum, and the flip clocks.
///
/// Prefer this over [`preem`] for **every** preem node, and required for
/// anything that animates: an anonymous node falls back to a positional key, and
/// a positional key is not merely rebuilt when the siblings shift — it is
/// *reused by the wrong node*, so this widget's animation state continues under
/// another one (#900).
#[must_use]
pub fn preem_id(id: impl Into<NodeId>, widget: PreemWidget) -> crate::wire::Node {
    crate::wire::Node::Preem {
        id: Some(id.into()),
        classes: Vec::new(),
        widget: Box::new(widget),
    }
}

/// A [`Node::Preem`](crate::wire::Node::Preem) with a stable [`NodeId`] and CSS
/// classes applied to the host widget.
#[must_use]
pub fn preem_styled(
    id: impl Into<NodeId>,
    classes: Vec<Cls>,
    widget: PreemWidget,
) -> crate::wire::Node {
    crate::wire::Node::Preem {
        id: Some(id.into()),
        classes,
        widget: Box::new(widget),
    }
}
