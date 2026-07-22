//! The **marquee / ticker**: scrolling dot-matrix text — a fixed-width window
//! panned across a pre-rendered strip, one frame-offset step at a time.
//!
//! The message is rasterized **once** into a wide [`dot_matrix`] strip (so the
//! glyph/dot rendering, the [`DisplayStyle`] skin, the accent tint, and the
//! ghost/bloom passes are all inherited, never re-implemented). A blank
//! [`gap`](Marquee::gap_px) of field columns is appended for the seam, and the
//! strip + gap becomes a seamless loop tile of width [`period`](MarqueeStrip::period).
//! [`MarqueeStrip::window`] then presents a `window_px`-wide slice of that tile
//! at any horizontal `offset`, wrapping around modulo the period — so the
//! caller just bumps a monotonic frame counter and the text scrolls forever.
//!
//! The kit owns no clock (see the module docs on timing): the plugin drives the
//! frame timer and the offset, exactly as the pet/caw already render at frame
//! cadence. The output is a plain [`Frame`], composed into
//! [`Node::Pixels`](hytte_plugin_proto::Node::Pixels) like every other kit
//! widget.
//!
//! ```
//! use hytte_plugin::preem::{DisplayStyle, Marquee};
//!
//! let strip = Marquee::new(DisplayStyle::Vfd).window_px(96).render("BREAKING NEWS");
//! // Bump the offset each frame; the window wraps seamlessly at `period`.
//! let f0 = strip.window(0);
//! let f1 = strip.window(1);
//! assert_eq!(f0.data().len(), f0.width() * f0.height() * 4);
//! assert_eq!(f0.width(), 96);
//! assert_ne!(f0, f1);
//! ```
//!
//! # Short text holds
//!
//! When the rendered message is **no wider than the window** it can't scroll
//! seamlessly (the loop would repeat the strip inside a single window), so the
//! marquee **holds** it static, left-aligned in the field, and ignores the
//! offset entirely — [`scrolls`](MarqueeStrip::scrolls) is then `false` and
//! [`period`](MarqueeStrip::period) is `0`. Only text wider than the window
//! actually scrolls.

use super::dot_matrix::dot_matrix;
use super::frame::{Frame, Rgba};
use super::style::DisplayStyle;

/// Default visible window width in pixels — a modest bar-chip ticker; override
/// with [`Marquee::window_px`] to fit the surface (the sidebar card is ~296 px).
const DEFAULT_WINDOW_PX: usize = 192;

/// Default blank-field gap appended after the message for the loop seam, in
/// pixels (≈ one glyph cell of clear space before the message restarts).
const DEFAULT_GAP_PX: usize = 24;

/// A builder for a scrolling dot-matrix marquee, rendered to a [`Frame`].
///
/// Holds the skin and window geometry; [`render`](Self::render) rasterizes the
/// message **once** into a reusable [`MarqueeStrip`] whose
/// [`window`](MarqueeStrip::window) is the cheap per-frame call. Defaults: a
/// [`DEFAULT_WINDOW_PX`]-wide window and a [`DEFAULT_GAP_PX`] seam gap.
///
/// Every knob is a consuming builder method, matching [`TextBox`](super::TextBox);
/// the builder is a value, so one `Marquee` renders many messages.
#[derive(Debug, Clone)]
pub struct Marquee {
    style: DisplayStyle,
    window_px: usize,
    gap_px: usize,
}

impl Marquee {
    /// A marquee in `style` with the default window and gap.
    #[must_use]
    pub fn new(style: DisplayStyle) -> Self {
        Self {
            style,
            window_px: DEFAULT_WINDOW_PX,
            gap_px: DEFAULT_GAP_PX,
        }
    }

    /// The visible window width in **final** pixels (the strip is already at the
    /// kit's baked dot scale, so this is 1:1 with strip pixels).
    #[must_use]
    pub fn window_px(mut self, px: usize) -> Self {
        self.window_px = px;
        self
    }

    /// The blank-field gap appended after the message before it loops, in
    /// pixels — the seam that separates the end of the message from its restart.
    #[must_use]
    pub fn gap_px(mut self, px: usize) -> Self {
        self.gap_px = px;
        self
    }

    /// Rasterize `text` once into a reusable [`MarqueeStrip`]. Valid for every
    /// input (empty string, uncovered chars included) — the strip's frames
    /// always satisfy the host's `len == w * h * 4` invariant.
    #[must_use]
    pub fn render(&self, text: &str) -> MarqueeStrip {
        let strip = dot_matrix(text, self.style);
        let bg = self.style.palette().bg;
        let height = strip.height();
        let strip_w = strip.width();

        // Wider than the window ⇒ scroll: build the seamless loop tile
        // (strip + blank gap). Otherwise hold the strip static (period 0).
        if strip_w > self.window_px {
            let period = strip_w + self.gap_px;
            let mut tile = Frame::filled(period, height, bg);
            tile.blit(&strip, 0, 0);
            MarqueeStrip {
                tile,
                window_px: self.window_px,
                period,
                bg,
            }
        } else {
            MarqueeStrip {
                tile: strip,
                window_px: self.window_px,
                period: 0,
                bg,
            }
        }
    }
}

/// A message rasterized once for scrolling: the seamless loop tile plus the
/// window geometry. Produced by [`Marquee::render`]; window it per frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarqueeStrip {
    /// The loop tile (message strip + gap) when scrolling, or the bare message
    /// strip when holding.
    tile: Frame,
    /// Visible window width in pixels.
    window_px: usize,
    /// The scroll period: the tile width the offset wraps around, or `0` when
    /// the message holds static (fits the window).
    period: usize,
    /// The field color, used to pad the window when the message holds.
    bg: Rgba,
}

impl MarqueeStrip {
    /// The offset modulus for a seamless loop — advance the offset past this
    /// and the window returns to the start. `0` means the message
    /// [holds](Self::scrolls) static rather than scrolling.
    #[must_use]
    pub fn period(&self) -> usize {
        self.period
    }

    /// Whether the message scrolls (`true`) or holds static because it fits the
    /// window (`false`).
    #[must_use]
    pub fn scrolls(&self) -> bool {
        self.period != 0
    }

    /// The buffer height in pixels (constant across offsets).
    #[must_use]
    pub fn height(&self) -> usize {
        self.tile.height()
    }

    /// The `window_px`-wide view of the marquee at horizontal `offset`. For a
    /// scrolling message the offset wraps modulo [`period`](Self::period), so
    /// any monotonically increasing frame counter loops seamlessly; a holding
    /// message ignores the offset and stays pinned to the left. The frame is
    /// fully opaque and always satisfies the host's `len == w * h * 4` invariant.
    #[must_use]
    pub fn window(&self, offset: usize) -> Frame {
        let height = self.tile.height();
        let mut out = Frame::filled(self.window_px, height, self.bg);
        if self.period == 0 {
            // Holds: the message sits at the left, offset ignored.
            out.blit(&self.tile, 0, 0);
        } else {
            let off = offset % self.period;
            for x in 0..self.window_px {
                let src = (off + x) % self.period;
                for y in 0..height {
                    out.set(x, y, self.tile.at(src, y));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::DisplayStyle;
    use super::{Frame, Marquee};

    /// A long message that overflows any reasonable window (so it scrolls).
    const LONG: &str = "PREEM RASTER KIT ~ SCROLLING TICKER ~ ";

    /// Extract column `x` of a frame as its RGBA bytes, top to bottom.
    fn column(f: &Frame, x: usize) -> Vec<u8> {
        let cx = i32::try_from(x).unwrap();
        (0..f.height())
            .flat_map(|y| f.get(cx, i32::try_from(y).unwrap()).unwrap())
            .collect()
    }

    /// The host invariant across styles, inputs, and offsets — the empty string
    /// and short (holding) text included.
    #[test]
    fn every_window_satisfies_the_host_invariant() {
        for style in DisplayStyle::ALL {
            for text in ["", " ", "HI", LONG, "åäö 💕 0123"] {
                let strip = Marquee::new(style).window_px(96).render(text);
                for offset in [0, 1, 7, strip.period().max(1) + 3, 9_999] {
                    let f = strip.window(offset);
                    assert_eq!(
                        f.data().len(),
                        f.width() * f.height() * 4,
                        "{style:?} {text:?} @ {offset}"
                    );
                    assert_eq!(f.width(), 96, "the window is fixed width");
                    assert!(f.height() > 0);
                }
            }
        }
    }

    /// Every window pixel is opaque — the marquee is a screen, wall to wall.
    #[test]
    fn every_window_pixel_is_opaque() {
        let strip = Marquee::new(DisplayStyle::Vfd).window_px(80).render(LONG);
        for offset in [0, 5, 40] {
            let f = strip.window(offset);
            assert!(
                f.data().chunks_exact(4).all(|px| px[3] == 0xff),
                "opaque @ {offset}"
            );
        }
    }

    /// Advancing the offset by one pans the window left by exactly one column:
    /// column `x` of `window(k+1)` equals column `x+1` of `window(k)`.
    #[test]
    fn one_step_pans_by_one_column() {
        let strip = Marquee::new(DisplayStyle::Oled).window_px(64).render(LONG);
        assert!(strip.scrolls(), "the long message scrolls");
        let a = strip.window(3);
        let b = strip.window(4);
        for x in 0..a.width() - 1 {
            assert_eq!(column(&b, x), column(&a, x + 1), "shift at column {x}");
        }
    }

    /// Offset 0 and offset N render different windows (it really scrolls).
    #[test]
    fn scrolling_offsets_differ() {
        let strip = Marquee::new(DisplayStyle::Lcd).window_px(72).render(LONG);
        assert_ne!(strip.window(0), strip.window(1));
        assert_ne!(strip.window(0), strip.window(9));
    }

    /// A full period wraps back to the start frame — the seamless loop.
    #[test]
    fn a_full_period_wraps_to_the_start() {
        let strip = Marquee::new(DisplayStyle::Vfd).window_px(64).render(LONG);
        let p = strip.period();
        assert!(p > 0);
        assert_eq!(strip.window(0), strip.window(p), "period wraps to start");
        assert_eq!(strip.window(5), strip.window(p + 5), "and at any phase");
    }

    /// Short text holds: no scroll, period 0, and the offset is ignored.
    #[test]
    fn short_text_holds_static() {
        let strip = Marquee::new(DisplayStyle::Vfd).window_px(200).render("HI");
        assert!(!strip.scrolls());
        assert_eq!(strip.period(), 0);
        assert_eq!(
            strip.window(0),
            strip.window(50),
            "offset ignored while held"
        );
        assert_eq!(strip.window(0).width(), 200, "still the fixed window width");
    }

    /// Renders are pure: same builder, same text, same bytes.
    #[test]
    fn render_is_deterministic() {
        let m = Marquee::new(DisplayStyle::Lcd).window_px(88).gap_px(12);
        assert_eq!(m.render(LONG).window(6), m.render(LONG).window(6));
    }

    /// The gap widens the loop period without changing the message pixels — a
    /// bigger seam, same scroll.
    #[test]
    fn gap_widens_the_period() {
        let narrow = Marquee::new(DisplayStyle::Oled)
            .window_px(64)
            .gap_px(4)
            .render(LONG);
        let wide = Marquee::new(DisplayStyle::Oled)
            .window_px(64)
            .gap_px(40)
            .render(LONG);
        assert_eq!(wide.period(), narrow.period() + 36);
        // The window at offset 0 starts on the same strip columns either way.
        assert_eq!(narrow.window(0), wide.window(0));
    }
}
