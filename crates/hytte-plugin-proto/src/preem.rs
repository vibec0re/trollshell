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
//! # Style is a *reference*, not colors
//!
//! [`StyleRef`] carries the [`StyleName`] (`vfd`/`lcd`/`oled`/`crt`) plus an
//! optional **semantic** [`AccentRole`]. It never carries resolved RGBA. The
//! shell resolves both against the live desktop theme, which is the whole
//! payoff: a `@accent_color` change re-tints every preem widget on screen with
//! zero plugin involvement and zero frames on the wire.
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
//! hostile plugin must never be able to crash or wedge the shell. See
//! [`MAX_TEXT_LEN`], [`MAX_SCOPE_SAMPLES`], [`MAX_BUFFER_DIM`], [`MAX_SCALE`],
//! [`MAX_LEDS`] and [`MAX_CELLS`], and [`PreemWidget::clamped`] — the shared
//! enforcement both ends use so the host never hand-rolls it.

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
/// Generous — every widget here truncates or wraps long text on its own terms
/// (the [`TextBox`](PreemWidget::TextBox) ellipsises past
/// [`TextBoxConfig::max_lines`], the [`FlipBoard`](PreemWidget::FlipBoard)
/// ignores anything past its last cell) — so this is not a layout knob but the
/// allocation bound: it is what stops a plugin sending a 100 MB string the
/// shell would have to rasterise. [`PreemWidget::clamped`] truncates to the
/// nearest char boundary at or below this length, never mid-codepoint.
pub const MAX_TEXT_LEN: usize = 4096;

/// Cap on [`ScopeState::samples`] **per update**.
///
/// The scope stamps a polyline across [`ScopeConfig::cols`] columns (144 by
/// default), so anything past a few hundred samples per batch is already
/// oversampled into invisibility; 4096 leaves a wide margin for a plugin
/// feeding a raw audio block while keeping one update's decode bounded to
/// ~16 KiB.
pub const MAX_SCOPE_SAMPLES: usize = 4096;

/// Cap on any single **logical buffer dimension** — [`ScopeConfig::cols`] /
/// [`rows`](ScopeConfig::rows), [`GaugeConfig::cols`] /
/// [`rows`](GaugeConfig::rows), [`MarqueeConfig::window_px`].
///
/// The shell allocates `cols * rows * scale²  * 4` bytes to draw one of these,
/// so the dimensions are exactly the numbers a hostile plugin would inflate.
/// The kit's own widgets target a ~296 px sidebar card; 4096 is far past any
/// real surface and still bounds the allocation.
pub const MAX_BUFFER_DIM: u32 = 4096;

/// Cap on any integer upscale factor (`scale` on
/// [`TextBox`](TextBoxConfig::scale) / [`Scope`](ScopeConfig::scale) /
/// [`Gauge`](GaugeConfig::scale) / [`FlipBoard`](FlipBoardConfig::scale)).
///
/// Scale multiplies **both** buffer dimensions, so it is quadratic in the
/// allocation and needs a much tighter cap than [`MAX_BUFFER_DIM`]. The kit's
/// own defaults are 1× and 2×; 8× is already a chunkier pixel than any skin
/// reads well at.
pub const MAX_SCALE: u32 = 8;

/// Cap on [`LedStripConfig::leds`].
///
/// The strip's rendered width grows linearly with the segment count
/// (`2*PAD + n*CELL_W + (n-1)*GAP` px), and the kit's default is 24 across a
/// ~296 px card, so 256 segments is already many times any real surface.
pub const MAX_LEDS: u32 = 256;

/// Cap on [`FlipBoardConfig::cells`].
///
/// A board's width grows linearly with the cell count, and its default is 8
/// (`HH:MM:SS`). 128 cells is far past a departure board's longest row.
pub const MAX_CELLS: u32 = 128;

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

/// A **semantic** ink role, resolved shell-side against the live theme.
///
/// This is what replaces a plugin reaching for RGBA (the kit's `TextBox::colors`
/// escape hatch). A plugin says *what the reading means*; the shell decides what
/// that looks like in the current accent and color scheme, so a re-tint costs no
/// wire traffic and no plugin rebuild.
///
/// `None` on [`StyleRef::accent`] — the default — means the style's own
/// hard-coded ink, i.e. exactly today's look.
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

/// How a preem widget asks to be skinned: a [`StyleName`] plus an optional
/// semantic [`AccentRole`]. Never resolved colors — see the module docs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleRef {
    /// The display skin.
    pub style: StyleName,
    /// The semantic ink role, or `None` for the skin's own hard-coded ink
    /// (the default, and what a frame that omits the key decodes to).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<AccentRole>,
}

impl StyleRef {
    /// A style reference with no accent role — the skin's own ink.
    #[must_use]
    pub fn new(style: StyleName) -> Self {
        Self {
            style,
            accent: None,
        }
    }

    /// The same skin, asking for a semantic ink role. Chainable off
    /// [`StyleRef::new`].
    #[must_use]
    pub fn with_accent(mut self, accent: AccentRole) -> Self {
        self.accent = Some(accent);
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
/// re-tint reaches this widget like every other. Defaults match the kit's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct TextBoxConfig {
    /// The skin to draw in.
    pub style: StyleRef,
    /// How the wrap width is chosen. Default: [`TextBoxWidth::Cols(16)`](TextBoxWidth::Cols).
    pub width: TextBoxWidth,
    /// Hard cap on wrapped lines; overflow truncates with a trailing `…`.
    /// Default `3`, clamped by the kit to at least 1.
    pub max_lines: u32,
    /// Field padding around the text block, in pre-scale pixels. Default `3`.
    pub pad: u32,
    /// Radius of the rounded-corner cut (to transparent), in pre-scale pixels;
    /// `0` keeps square corners. Default `2`.
    pub corner: u32,
    /// Integer upscale baked into the buffer. Default `1`; capped at
    /// [`MAX_SCALE`], and `0` reads as `1`.
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
pub struct PeakHoldConfig {
    /// Fall per animation tick, in fractions of full scale. The kit clamps a
    /// negative rate to `0.0` (a dot that never falls).
    pub rate: f32,
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
    pub level: f32,
    /// An **explicit** peak-dot position in `0.0..=1.0`, for a plugin that
    /// computes its own peak (a true inter-frame peak off a raw audio block,
    /// say, which the shell's per-render fold cannot see).
    ///
    /// `None` — the default — leaves the peak to
    /// [`LedStripConfig::peak_hold`]; when both are set the explicit value
    /// wins for that render, and the held value is *not* disturbed by it.
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
    /// `0.0` (or a non-finite value) parks the message.
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
    /// `1..=`[`MAX_SCALE`].
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
    pub samples: Vec<f32>,
}

// ── Gauge ───────────────────────────────────────────────────────────────────

/// The value scale a [`Gauge`](PreemWidget::Gauge) reads in: `low` at the left
/// end of the dial, `high` at the right.
///
/// The needle physics always runs in fraction-of-scale space, so the spring
/// constants never need re-tuning for a new range. The kit rejects a degenerate
/// (`high <= low`) or non-finite range and keeps the previous one.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GaugeRange {
    /// The value at the low end of the dial.
    pub low: f32,
    /// The value at the high end of the dial.
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
    /// `1..=`[`MAX_SCALE`].
    pub scale: u32,
    /// Total sweep of the scale, in **degrees**. Default `150.0`; the kit
    /// clamps to `10.0..=180.0`.
    pub sweep_deg: f32,
    /// Major divisions (intervals between long ticks). Default `4`, kit-clamped
    /// to at least 1.
    pub divisions: u32,
    /// Minor ticks per major division. Default `5`, kit-clamped to at least 1.
    pub subdivisions: u32,
    /// The value scale the caller reads in. Default `0.0..=1.0`.
    pub range: GaugeRange,
    /// Undamped natural frequency in Hz — how *fast* the needle swings. Default
    /// `2.0`; the kit clamps to `0.05..=20.0`.
    pub frequency_hz: f32,
    /// Damping ratio `ζ` — how much the needle *overshoots*. Default `0.5`
    /// (half critical: one obvious kick past the reading, then settle); the kit
    /// clamps to `0.05..=4.0`.
    pub damping: f32,
}

impl Default for GaugeConfig {
    fn default() -> Self {
        Self {
            style: StyleRef::default(),
            cols: 144,
            rows: 64,
            scale: 2,
            sweep_deg: 150.0,
            divisions: 4,
            subdivisions: 5,
            range: GaugeRange::default(),
            frequency_hz: 2.0,
            damping: 0.5,
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
    /// Where the needle should head, in [`GaugeConfig::range`]. Out-of-range
    /// values clamp to the ends; a non-finite value is ignored (the needle keeps
    /// its current target rather than being poisoned).
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
    /// `1..=`[`MAX_SCALE`].
    pub scale: u32,
    /// Per-cell transition length in seconds, or `None` for the
    /// **mechanism's own** default (`0.38` for a split flap, `0.30` for a
    /// nixie cross-fade). The kit clamps to `0.01..=10.0`.
    ///
    /// `Option` rather than a bare `f32` precisely because the default is not a
    /// constant — it depends on [`mechanism`](FlipBoardConfig::mechanism), which
    /// a struct-level default cannot see.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f32>,
    /// Per-cell left-to-right stagger in seconds, or `None` for the
    /// **mechanism's own** default (`0.055` for a split flap; `0.0` for a
    /// nixie, whose tubes are wired in parallel and switch together). `0.0`
    /// makes the whole row move as one. The kit clamps to `0.0..=2.0`.
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
    /// Floating-point *readings* (`level`, `target`, `speed_dots_per_sec`, the
    /// spring constants, the range) are deliberately **not** touched here: the
    /// kit already clamps or ignores out-of-range and non-finite values at every
    /// entry point, and no float on the wire sizes an allocation. This method
    /// bounds what the shell would *allocate*; the kit bounds what it draws.
    #[must_use]
    pub fn clamped(mut self) -> Self {
        match &mut self {
            Self::DotMatrix { state, .. } => clamp_text(&mut state.text),
            Self::SevenSeg { state, .. } => clamp_text(&mut state.text),
            Self::TextBox { config, state } => {
                config.width = match config.width {
                    TextBoxWidth::Cols(n) => TextBoxWidth::Cols(n.clamp(1, MAX_BUFFER_DIM)),
                    TextBoxWidth::FitPx(px) => TextBoxWidth::FitPx(px.clamp(1, MAX_BUFFER_DIM)),
                };
                config.max_lines = config.max_lines.clamp(1, MAX_BUFFER_DIM);
                config.pad = config.pad.min(MAX_BUFFER_DIM);
                config.corner = config.corner.min(MAX_BUFFER_DIM);
                config.scale = config.scale.clamp(1, MAX_SCALE);
                clamp_text(&mut state.text);
            }
            Self::LedStrip { config, .. } => {
                config.leds = config.leds.clamp(1, MAX_LEDS);
            }
            Self::Marquee { config, state } => {
                config.window_px = config.window_px.clamp(1, MAX_BUFFER_DIM);
                config.gap_dots = config.gap_dots.min(MAX_BUFFER_DIM);
                clamp_text(&mut state.text);
            }
            Self::Scope { config, state } => {
                config.cols = config.cols.clamp(1, MAX_BUFFER_DIM);
                config.rows = config.rows.clamp(1, MAX_BUFFER_DIM);
                config.scale = config.scale.clamp(1, MAX_SCALE);
                state.samples.truncate(MAX_SCOPE_SAMPLES);
            }
            Self::Gauge { config, .. } => {
                config.cols = config.cols.clamp(1, MAX_BUFFER_DIM);
                config.rows = config.rows.clamp(1, MAX_BUFFER_DIM);
                config.scale = config.scale.clamp(1, MAX_SCALE);
                config.divisions = config.divisions.clamp(1, MAX_BUFFER_DIM);
                config.subdivisions = config.subdivisions.clamp(1, MAX_BUFFER_DIM);
            }
            Self::FlipBoard { config, state } => {
                config.cells = config.cells.clamp(1, MAX_CELLS);
                config.glyph_px = config.glyph_px.clamp(2, 16);
                config.scale = config.scale.clamp(1, MAX_SCALE);
                clamp_text(&mut state.text);
            }
        }
        self
    }
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

/// A [`Node::Preem`](crate::wire::Node::Preem) wrapping `widget`, with no id and
/// no classes — the one-liner a plugin's `view` reaches for.
#[must_use]
pub fn preem(widget: PreemWidget) -> crate::wire::Node {
    crate::wire::Node::Preem {
        id: None,
        classes: Vec::new(),
        widget,
    }
}

/// A [`Node::Preem`](crate::wire::Node::Preem) with a stable [`NodeId`], so the
/// host reconciler keeps the *same* renderer instance across renders — which is
/// what preserves the phosphor, the needle's momentum, and the flip clocks.
///
/// Prefer this over [`preem`] for anything that animates: a positionally-matched
/// node whose siblings shift is a node the reconciler may rebuild, and a rebuild
/// resets the animation.
#[must_use]
pub fn preem_id(id: impl Into<NodeId>, widget: PreemWidget) -> crate::wire::Node {
    crate::wire::Node::Preem {
        id: Some(id.into()),
        classes: Vec::new(),
        widget,
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
        widget,
    }
}
