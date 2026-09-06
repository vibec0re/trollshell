//! `preem` — the GTK-free **retro raster kit** (issue #356; the #354 whimsy
//! umbrella).
//!
//! A leaf crate with no toolkit of its own: it draws retro displays into
//! plain byte buffers and hands them back, so both sides of the plugin
//! boundary can use it — a plugin ships the buffer over the wire as
//! [`Node::Pixels`](hytte_plugin_proto::Node::Pixels), while the shell can
//! rasterize one straight into a `PixelSurface`. `hytte-plugin` re-exports the
//! whole kit as `hytte_plugin::preem`, which is how every plugin reaches it.
//!
//! Everything here renders into plain CPU-side RGBA8 buffers destined for
//! [`Node::Pixels`](hytte_plugin_proto::Node::Pixels): row-major (row 0 first),
//! 4 bytes per pixel in `[R, G, B, A]` order with straight (non-premultiplied)
//! alpha, and `len == width * height * 4` — the invariant the host validates
//! before it draws anything. The host upscales the buffer **nearest-neighbor**,
//! so hard-edged pixels stay chunky and crisp; none of these renderers
//! anti-alias against the widget behind them.
//!
//! The kit exists because every raster plugin had been re-inventing the same
//! private toolkit: the pet hand-rolled `plot`/`fill`/`hline` helpers and a
//! 5×7 pixel font for its LCD face and speech bubble (#284/#304). Those
//! primitives are promoted here — the shared [`Frame`] buffer plus [`font`],
//! the glyph set — and three predefined display widgets are built on them:
//!
//! - [`dot_matrix`] — a single text line as char cells of round-falloff dots,
//!   in any [`DisplayStyle`] skin.
//! - [`Marquee`] — a scrolling [`dot_matrix`] ticker: the message is a
//!   font-space bitmap sampled onto a **fixed** physical dot grid, stepped one
//!   whole virtual pixel (one dot) at a time, so the grid never moves with the
//!   text (#839).
//! - [`seven_seg`] — a classic seven-segment readout (digits, `:`, `-`,
//!   space) with the authentic dim ghost segments behind the lit ones.
//! - [`TextBox`] — the "8bit textbox": wrapped 5×7 pixel-font text on a
//!   rounded field (the pet's speech bubble is exactly this, dressed in
//!   lilac).
//! - [`LedStrip`] — a row of discrete LEDs lighting with a `0.0..=1.0` level,
//!   topped by a peak-hold dot that floats and decays ([`PeakHold`]) — the VU
//!   meter (#506).
//! - [`LedMatrix`] — the strip's 2-D sibling: a **grid** of LEDs each lit to
//!   its own brightness, for N independent readings rather than one level
//!   ("Blinken Lichten", #857). Shapeable — explicit `(cols, rows)`, the
//!   near-square [`rect`](LedMatrix::rect), or the wide-rectangle
//!   [`wide`](LedMatrix::wide) — with a [`Fill`] policy for the
//!   slots a ragged last row leaves over, and the kit's one consumer of the
//!   [`ColorMap`] axis below.
//! - [`Scope`] — a glow-trace oscilloscope over a graticule, with real
//!   phosphor persistence: the beam trail decays exponentially across frames
//!   rather than redrawing from black (#556, a #397 skin).
//! - [`Gauge`] — a needle gauge over a swept scale: tick marks, a lit value
//!   arc filling to the reading, and a tapered pointer driven by a real
//!   damped-oscillator [`Needle`] that overshoots a step change and settles,
//!   with a time-sampled motion blur behind it (a #397 skin).
//! - [`FlipBoard`] — a fixed row of character cells that *change* by a visible
//!   mechanism ([`Mechanism`], a #397 skin): the airport board's card hinging
//!   down over its hinge slot, or a nixie tube's outgoing cathode fading out
//!   under the incoming one's strike. Closed-form in the elapsed time, like
//!   the gauge's physics.
//!
//! `hytte-plugin-preem-demo` is the reference consumer: one sidebar card
//! cycling every widget through every style; `hytte-plugin-audio-widget` is the
//! reference audio-reactive consumer (dot-matrix + spectrum + [`LedStrip`]).
//!
//! # Styles
//!
//! [`DisplayStyle`] is a palette + post-pass over **one** renderer per
//! widget, not per-style code paths:
//!
//! - **`Vfd`** — pale cyan on near-black, with a phosphor glow bleeding off
//!   lit pixels.
//! - **`Lcd`** — dark ink on an olive field, with the faint ghost of every
//!   unlit cell/segment showing through (no glow — reflective displays don't
//!   bloom).
//! - **`Oled`** — white-blue on true black, a tight per-pixel bloom, and
//!   **no** ghosting: an off OLED pixel emits nothing (#354, Annika's
//!   addition).
//! - **`Crt`** — P31 phosphor green on a near-black tube face, a broad
//!   phosphor bloom, and the raster itself: a scanline comb phased into the
//!   seams between dot rows plus a curved-glass vignette, multiplied into the
//!   lit layer at composite time (a #397 skin). Unlike the other three this is
//!   a **pass**, not a look for one widget — every kit surface above renders
//!   through the tube with no code of its own. Screen-space and
//!   resampling-free by design: curvature reads as a vignette rather than a
//!   barrel warp, so the fixed-grid discipline below survives it intact.
//!
//! The **ink** each skin lights up with is the part of a palette a host moves
//! routinely: [`set_accent`] answers it once per process (what a plugin gets),
//! and [`with_ink`] answers it per render (what a shell drawing many plugins'
//! widgets needs, #885). [`with_pins`] widens that scope by one slot — the
//! **field** — for the case #884 measured, a widget whose ground has to match
//! something hand-drawn beside it. Ghost, bloom and the CRT pass are the panel's
//! physical character and are never overridden — that is what keeps a re-tinted
//! widget still reading as the same device.
//!
//! # Colour — a second axis, not more styles (#857)
//!
//! [`ColorMap`] answers "what colour is *this cell*?", which is a different
//! question from [`DisplayStyle`]'s "what device is this?". A style contributes
//! one accent-tinted ink plus the panel's physical character; a map turns that
//! single ink into a per-cell one — a heat ramp, a hue sweep, a flag, a fixed
//! colour. They are deliberately orthogonal, so a heat-mapped panel still gets
//! the CRT pass's scanlines instead of choosing between them. The default
//! ([`ColorMap::Style`]) *is* the single ink, so every surface that does not
//! ask for a map renders exactly as it did before the axis existed.
//!
//! # Sizing — the #313 lesson
//!
//! Size a widget via its **buffer dimensions**: a `Pixels` node's natural
//! size is `width`×`height`, and shell CSS minimums below the buffer size
//! are silent no-ops (the `.caw-lcd`/`.pet-lcd` lesson). The sidebar card's
//! content width is ~296 px — keep buffers within that, and bake chunkiness
//! into the buffer itself ([`Frame::upscale`]) rather than hoping the host
//! scales it for you.
//!
//! # Timing
//!
//! The kit renders *frames*; it owns no clock. Animation cadence (ticker
//! steps, blink cycles, style rotation) belongs to whoever is driving the
//! redraw — a plugin's own `Plugin::sources` stream or its snapshot flow,
//! exactly as the pet already does.

pub mod font;

mod color_map;
mod contrast;
mod dot_matrix;
mod frame;
mod gauge;
mod led_matrix;
mod led_strip;
mod marquee;
mod scope;
mod seven_seg;
mod split_flap;
mod style;
mod textbox;

pub use color_map::ColorMap;
pub use dot_matrix::dot_matrix;
pub use frame::{Frame, Rgba};
pub use gauge::{DEFAULT_DAMPING, DEFAULT_FREQ_HZ, Gauge, Needle, OVERTRAVEL, TRAIL_SPAN_SECS};
pub use led_matrix::{Fill, LedMatrix};
pub use led_strip::{DEFAULT_LEDS, DEFAULT_WIDTH, LedStrip, PeakHold, led_strip};
pub use marquee::{Marquee, MarqueeStrip};
pub use scope::Scope;
pub use seven_seg::seven_seg;
pub use split_flap::{
    CHARSET, DEFAULT_FADE_SECS, DEFAULT_FLIP_SECS, DEFAULT_GLYPH_PX, DEFAULT_STAGGER_SECS,
    FlipBoard, Mechanism,
};
pub use style::DisplayStyle;
pub use textbox::TextBox;

/// Install the host-resolved desktop accent as the kit's default widget tint
/// (#376). Host-facing, not author-facing: the SDK transport runtime calls it
/// from the [`HostMsg::Accent`](hytte_plugin_proto::HostMsg::Accent) frame and
/// a shell drawing the kit in-process would call it once from wherever it
/// resolves the accent; a plugin author never does (an explicit palette still
/// wins). It was `pub(crate)` while the kit lived inside the SDK — the caller
/// is simply on the far side of a crate boundary now.
pub use style::set_accent;

/// The WCAG contrast ratio between two colors (#928): `1.0` when they are
/// equally luminous, `21.0` for black against white, symmetric.
///
/// Host-facing, and the reason it is public: a host that resolves colors of its
/// own — a shell mapping semantic roles onto the live theme — needs to ask
/// whether one of them can be read on a skin before it pins it, and
/// [`AA_TEXT`] is the bar the kit itself holds its own inks to. Pair it with
/// [`DisplayStyle::field`] for the skin's ground and
/// [`DisplayStyle::admit_ink`] for the skin's own answer.
pub use contrast::{AA_TEXT, ratio as contrast_ratio};

/// Scope one render to an explicit palette (#885) — host-facing like
/// [`set_accent`], and the *per-render* answer where that one is the
/// *per-process* answer. A shell rasterising many plugins' widgets in one
/// process resolves each widget's own semantic role (or its pinned colors)
/// around the call that draws it; a plugin never touches either.
///
/// [`with_pins`] names the ink and, optionally, the field; [`with_ink`] is the
/// one-slot form, kept because most callers pin exactly one color.
pub use style::{Ink, Pins, with_ink, with_pins};
