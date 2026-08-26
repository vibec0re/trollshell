//! `preem` — the SDK's GTK-free **retro raster kit** (issue #356; the #354
//! whimsy umbrella).
//!
//! Everything here renders into plain CPU-side RGBA8 buffers destined for
//! [`Node::Pixels`](crate::proto::Node::Pixels): row-major (row 0 first),
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
//!   in one of the three [`DisplayStyle`] skins.
//! - [`Marquee`] — a scrolling [`dot_matrix`] ticker: a fixed-width window
//!   panned across a pre-rendered strip, one frame-offset step at a time.
//! - [`seven_seg`] — a classic seven-segment readout (digits, `:`, `-`,
//!   space) with the authentic dim ghost segments behind the lit ones.
//! - [`TextBox`] — the "8bit textbox": wrapped 5×7 pixel-font text on a
//!   rounded field (the pet's speech bubble is exactly this, dressed in
//!   lilac).
//! - [`LedStrip`] — a row of discrete LEDs lighting with a `0.0..=1.0` level,
//!   topped by a peak-hold dot that floats and decays ([`PeakHold`]) — the VU
//!   meter (#506).
//! - [`Scope`] — a glow-trace oscilloscope over a graticule, with real
//!   phosphor persistence: the beam trail decays exponentially across frames
//!   rather than redrawing from black (#556, a #397 skin).
//! - [`Gauge`] — a needle gauge over a swept scale: tick marks, a lit value
//!   arc filling to the reading, and a tapered pointer driven by a real
//!   damped-oscillator [`Needle`] that overshoots a step change and settles,
//!   with a time-sampled motion blur behind it (a #397 skin).
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
//! steps, blink cycles, style rotation) belongs to the plugin's own
//! [`sources`](crate::Plugin::sources) stream or its snapshot flow, exactly
//! as the pet already does.

pub mod font;

mod dot_matrix;
mod frame;
mod gauge;
mod led_strip;
mod marquee;
mod scope;
mod seven_seg;
mod style;
mod textbox;

pub use dot_matrix::dot_matrix;
pub use frame::{Frame, Rgba};
pub use gauge::{DEFAULT_DAMPING, DEFAULT_FREQ_HZ, Gauge, Needle, OVERTRAVEL, TRAIL_SPAN_SECS};
pub use led_strip::{DEFAULT_LEDS, DEFAULT_WIDTH, LedStrip, PeakHold, led_strip};
pub use marquee::{Marquee, MarqueeStrip};
pub use scope::Scope;
pub use seven_seg::seven_seg;
pub use style::DisplayStyle;
pub use textbox::TextBox;

/// Install the host-resolved desktop accent as the kit's default widget tint
/// (#376). Crate-internal: the SDK transport runtime calls it from the
/// [`HostMsg::Accent`](hytte_plugin_proto::HostMsg::Accent) frame; a plugin
/// author never does (an explicit palette still wins).
pub(crate) use style::set_accent;
