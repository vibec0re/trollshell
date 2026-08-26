//! The **marquee / ticker**: scrolling dot-matrix text on a *fixed* dot grid —
//! the message slides through the grid one whole dot at a time, and the grid
//! itself never moves.
//!
//! The message is rasterized **once** into a **font-space bitmap** — one bit
//! per virtual pixel, i.e. per dot, [`font::GLYPH_H`] rows tall — and the
//! window owns a fixed physical grid of dot cells. Every
//! [`window`](MarqueeStrip::window) paints that grid's unlit (ghost) dots at
//! their fixed positions and then lights the dots the bitmap says are on at
//! this offset. The dot *hardware* is
//! [`dot_matrix`](super::dot_matrix)'s — same [`DOT`] pitch, same [`PAD`]
//! bezel, same falloff painters — so a scrolled dot is pixel-for-pixel a
//! static one, and the [`DisplayStyle`] skin, the accent tint and the
//! ghost/bloom passes are all inherited rather than re-implemented.
//!
//! # The unit is a virtual pixel (#839)
//!
//! [`MarqueeStrip::window`] takes its `offset` in **whole dots**, and
//! [`period`](MarqueeStrip::period) and [`Marquee::gap_dots`] are in dots too.
//! There is no buffer-pixel offset anywhere in the type, so a sub-dot position
//! is *inexpressible* — which is the whole point. Before #839 the strip was a
//! pre-rendered buffer panned by raw buffer pixels: a step of 3 against the
//! 4-pixel dot pitch put every dot between grid positions, and the text
//! smeared instead of stepping. Scrolling can now only ever land dot-on-grid.
//!
//! The ghost matrix is likewise not part of what scrolls: it is baked once,
//! at render time, into the window-sized backdrop every frame starts from
//! (`MarqueeStrip::base`). The offset cannot reach it.
//!
//! # A continuous ticker matrix, not travelling char cells
//!
//! [`dot_matrix`](super::dot_matrix) models a row of **character cells** — the
//! spacing column between two cells carries no dots, because the hardware has
//! none there. A scrolling window can't keep that: cell structure that
//! *travels* is exactly the artefact a fixed grid removes. So the marquee's
//! grid is uniform across the whole window — a continuous ticker matrix, like
//! the real scrolling-VFD hardware — while the static display keeps its
//! per-cell look, unchanged.
//!
//! At the kit's metrics the two still line up exactly: a 268 px window is 65
//! dot columns, which is precisely the 11-char static ticker's dot count, so
//! stacking a `dot_matrix` readout above a marquee lands the two grids on the
//! same pixel columns.
//!
//! ```
//! use hytte_plugin::preem::{DisplayStyle, Marquee};
//!
//! let strip = Marquee::new(DisplayStyle::Vfd).window_px(96).render("BREAKING NEWS");
//! // Bump the offset by whole dots each frame; the window wraps seamlessly
//! // at `period` (also dots).
//! let f0 = strip.window(0);
//! let f1 = strip.window(1); // one dot to the left — never a fraction of one
//! assert_eq!(f0.data().len(), f0.width() * f0.height() * 4);
//! assert_eq!(f0.width(), 96);
//! assert_ne!(f0, f1);
//! ```
//!
//! The kit owns no clock (see the module docs on timing): the plugin drives the
//! frame timer and the offset, exactly as the pet/caw already render at frame
//! cadence. The output is a plain [`Frame`], composed into
//! [`Node::Pixels`](hytte_plugin_proto::Node::Pixels) like every other kit
//! widget.
//!
//! # Short text holds
//!
//! When the rendered message is **no wider than the window's grid** it can't
//! scroll seamlessly (the loop would repeat the message inside a single
//! window), so the marquee **holds** it static, left-aligned on the grid, and
//! ignores the offset entirely — [`scrolls`](MarqueeStrip::scrolls) is then
//! `false` and [`period`](MarqueeStrip::period) is `0`. Only text wider than
//! the grid actually scrolls.

use super::dot_matrix::{DOT, PAD, ghost_dot, lit_dot};
use super::font;
use super::frame::Frame;
use super::style::{DisplayStyle, Emission};

/// Default visible window width in pixels — a modest bar-chip ticker; override
/// with [`Marquee::window_px`] to fit the surface (the sidebar card is ~296 px).
const DEFAULT_WINDOW_PX: usize = 192;

/// Default blank-field gap appended after the message for the loop seam, in
/// **dots** — one glyph cell of clear space before the message restarts.
const DEFAULT_GAP_DOTS: usize = font::GLYPH_W + font::SPACING;

/// One column of the font-space bitmap: bit `row` is the dot at that row, top
/// row = bit 0. One bit per virtual pixel, which is the whole storage — the
/// bitmap knows nothing about buffer pixels.
type Column = u8;

const _: () = assert!(
    font::GLYPH_H <= std::mem::size_of::<Column>() * 8,
    "one bitmap column must hold one bit per glyph row"
);

/// Rasterize `text` into font space: [`GLYPH_W`](font::GLYPH_W) columns per
/// char with [`SPACING`](font::SPACING) blank columns between chars, exactly
/// the metrics [`dot_matrix`](super::dot_matrix) advances by. Uncovered chars
/// (emoji included) become the hollow [`font::NOTDEF`] box, never a panic; an
/// empty string is an empty bitmap.
fn rasterize(text: &str) -> Vec<Column> {
    let mut columns = Vec::new();
    for (i, ch) in text.chars().enumerate() {
        if i > 0 {
            columns.extend(std::iter::repeat_n(0, font::SPACING));
        }
        let rows = font::glyph(ch).unwrap_or(&font::NOTDEF);
        for cx in 0..font::GLYPH_W {
            let mut column: Column = 0;
            for (row, &bits) in rows.iter().enumerate() {
                if (bits >> (font::GLYPH_W - 1 - cx)) & 1 == 1 {
                    column |= 1 << row;
                }
            }
            columns.push(column);
        }
    }
    columns
}

/// A builder for a scrolling dot-matrix marquee, rendered to a [`Frame`].
///
/// Holds the skin and window geometry; [`render`](Self::render) does the
/// **once-per-message** work — rasterizing the glyphs into font space and
/// painting the window's fixed ghost matrix — into a reusable [`MarqueeStrip`]
/// that [`window`](MarqueeStrip::window) then lights per frame. Defaults: a
/// [`DEFAULT_WINDOW_PX`]-wide window and a [`DEFAULT_GAP_DOTS`] seam gap.
///
/// Every knob is a consuming builder method, matching [`TextBox`](super::TextBox);
/// the builder is a value, so one `Marquee` renders many messages.
#[derive(Debug, Clone)]
pub struct Marquee {
    style: DisplayStyle,
    window_px: usize,
    gap_dots: usize,
}

impl Marquee {
    /// A marquee in `style` with the default window and gap.
    #[must_use]
    pub fn new(style: DisplayStyle) -> Self {
        Self {
            style,
            window_px: DEFAULT_WINDOW_PX,
            gap_dots: DEFAULT_GAP_DOTS,
        }
    }

    /// The visible window width in **final** buffer pixels — the width of the
    /// [`Frame`] each [`window`](MarqueeStrip::window) hands back, so size it
    /// to the surface (the `Pixels` node's natural size *is* the buffer, per
    /// the kit's sizing docs).
    ///
    /// The dot grid inside is as many whole dot cells as fit between the
    /// [`PAD`] bezels, centered in the window; a width that isn't a whole
    /// number of dots widens the bezel rather than clipping a dot. This is the
    /// only knob in buffer pixels — everything that *moves* is in dots.
    #[must_use]
    pub fn window_px(mut self, px: usize) -> Self {
        self.window_px = px;
        self
    }

    /// The blank gap appended after the message before it loops, in **dots** —
    /// the seam that separates the end of the message from its restart. In
    /// dots, not pixels, so the seam is a whole number of grid columns like
    /// everything else the offset can reach (#839).
    #[must_use]
    pub fn gap_dots(mut self, dots: usize) -> Self {
        self.gap_dots = dots;
        self
    }

    /// Rasterize `text` once into a reusable [`MarqueeStrip`]. Valid for every
    /// input (empty string, uncovered chars included) — the strip's frames
    /// always satisfy the host's `len == w * h * 4` invariant.
    #[must_use]
    pub fn render(&self, text: &str) -> MarqueeStrip {
        let palette = self.style.palette();
        // As many whole dot cells as fit between the bezels; the leftover
        // splits evenly, so the grid is centered and the margin is never
        // narrower than `PAD`.
        let cols = self.window_px.saturating_sub(2 * PAD) / DOT;
        let origin_x = (self.window_px - cols * DOT) / 2;
        let height = 2 * PAD + font::GLYPH_H * DOT;

        // The backdrop: the field plus the *fixed* ghost matrix, painted once.
        // Nothing the offset does can move it — every frame starts from this
        // exact buffer.
        let mut base = Frame::filled(self.window_px, height, palette.bg);
        if let Some(ghost) = palette.ghost {
            for col in 0..cols {
                for row in 0..font::GLYPH_H {
                    ghost_dot(&mut base, origin_x + col * DOT, PAD + row * DOT, ghost);
                }
            }
        }

        let bitmap = rasterize(text);
        // Wider than the grid ⇒ scroll, looping over message + gap. Otherwise
        // hold it static (period 0).
        let period = if bitmap.len() > cols {
            bitmap.len() + self.gap_dots
        } else {
            0
        };
        MarqueeStrip {
            bitmap,
            base,
            cols,
            origin_x,
            period,
            style: self.style,
        }
    }
}

/// A message rasterized once for scrolling: the font-space bitmap, the fixed
/// grid's backdrop, and the window geometry. Produced by [`Marquee::render`];
/// window it per frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarqueeStrip {
    /// The message in font space: one [`Column`] per virtual pixel, one bit
    /// per dot. Columns past its end are the loop gap — unlit.
    bitmap: Vec<Column>,
    /// The window-sized backdrop: field + the fixed ghost dot grid. Painted
    /// once at render time; every window is this buffer plus lit dots.
    base: Frame,
    /// Dot cells across the window's fixed grid.
    cols: usize,
    /// Buffer-pixel x of the grid's first dot cell (the bezel is `PAD` or
    /// wider). The grid's y origin is always `PAD`.
    origin_x: usize,
    /// The scroll period **in dots**: the bitmap+gap length the offset wraps
    /// around, or `0` when the message holds static (fits the grid).
    period: usize,
    /// The skin. Kept rather than a baked palette so the lit pass follows a
    /// live accent change (#376); `bg`/`ghost` are accent-independent, so the
    /// baked backdrop can't go stale.
    style: DisplayStyle,
}

impl MarqueeStrip {
    /// The offset modulus for a seamless loop, in **dots** — advance the offset
    /// past this and the window returns to the start. `0` means the message
    /// [holds](Self::scrolls) static rather than scrolling.
    #[must_use]
    pub fn period(&self) -> usize {
        self.period
    }

    /// Whether the message scrolls (`true`) or holds static because it fits the
    /// window's grid (`false`).
    #[must_use]
    pub fn scrolls(&self) -> bool {
        self.period != 0
    }

    /// The buffer height in pixels (constant across offsets).
    #[must_use]
    pub fn height(&self) -> usize {
        self.base.height()
    }

    /// The window at horizontal `offset`, in **whole dots** — a sub-dot
    /// position is not expressible, so the text can only ever step grid
    /// column to grid column (#839).
    ///
    /// For a scrolling message the offset wraps modulo
    /// [`period`](Self::period), so any monotonically increasing frame counter
    /// loops seamlessly; a holding message ignores the offset and stays pinned
    /// left. The grid — bezel, ghost dots, dot positions — is identical in
    /// every frame this returns; only which dots are *lit* depends on the
    /// offset. The frame is fully opaque and always satisfies the host's
    /// `len == w * h * 4` invariant.
    #[must_use]
    pub fn window(&self, offset: usize) -> Frame {
        let palette = self.style.palette();
        let mut out = self.base.clone();
        let mut lit = Emission::new(out.width(), out.height());
        let start = if self.period == 0 {
            0
        } else {
            offset % self.period
        };
        for col in 0..self.cols {
            let src = if self.period == 0 {
                col
            } else {
                (start + col) % self.period
            };
            // Past the bitmap is the loop gap (or the blank tail of a held
            // message): grid position with nothing lit on it.
            let Some(&column) = self.bitmap.get(src) else {
                continue;
            };
            for row in 0..font::GLYPH_H {
                if (column >> row) & 1 == 1 {
                    lit_dot(&mut lit, self.origin_x + col * DOT, PAD + row * DOT);
                }
            }
        }
        if let Some(bloom) = palette.bloom {
            lit.bloom(bloom);
        }
        lit.composite(&mut out, palette.ink, palette.mask);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::DisplayStyle;
    use super::super::dot_matrix::dot_matrix;
    use super::{DOT, Frame, Marquee, MarqueeStrip, PAD, font, rasterize};

    /// A long message that overflows any reasonable window (so it scrolls).
    const LONG: &str = "PREEM RASTER KIT ~ SCROLLING TICKER ~ ";

    /// Extract column `x` of a frame as its RGBA bytes, top to bottom.
    fn column(f: &Frame, x: usize) -> Vec<u8> {
        let cx = i32::try_from(x).unwrap();
        (0..f.height())
            .flat_map(|y| f.get(cx, i32::try_from(y).unwrap()).unwrap())
            .collect()
    }

    /// The unlit reference for a geometry: field + the fixed ghost grid and
    /// nothing else. Rendering the empty string lights no dot, so this *is*
    /// the backdrop every window is built on.
    fn reference(style: DisplayStyle, window_px: usize) -> Frame {
        Marquee::new(style)
            .window_px(window_px)
            .render("")
            .window(0)
    }

    /// Which grid cells are lit, read back off the rendered pixels: a cell is
    /// lit iff its dot's bright-core pixel departs from the unlit reference.
    /// Use a bloom-free skin (LCD) so nothing but a lit dot can change a pixel.
    fn lit_cells(strip: &MarqueeStrip, f: &Frame, want: &Frame) -> Vec<[bool; font::GLYPH_H]> {
        (0..strip.cols)
            .map(|col| {
                let mut cell = [false; font::GLYPH_H];
                for (row, on) in cell.iter_mut().enumerate() {
                    // (+1, +1) is the dot's 2×2 core — full intensity when lit.
                    let x = strip.origin_x + col * DOT + 1;
                    let y = PAD + row * DOT + 1;
                    *on = f.at(x, y) != want.at(x, y);
                }
                cell
            })
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

    /// #839, symptom two: **the dot background never moves with the text.** A
    /// message that is all spaces lights nothing, so every frame it renders is
    /// the bare grid — and the grid must be byte-identical at every offset, and
    /// identical to the blank reference. The pre-#839 marquee scrolled a strip
    /// with the ghost dots baked in, whose per-char-cell gaps travelled with
    /// the offset; this is the test that catches that.
    #[test]
    fn the_ghost_grid_never_moves_with_the_offset() {
        for style in DisplayStyle::ALL {
            let blank = " ".repeat(120);
            let strip = Marquee::new(style).window_px(96).render(&blank);
            assert!(strip.scrolls(), "120 spaces overflow a 96 px window");
            let want = reference(style, 96);
            for offset in [0, 1, 2, 3, 17, strip.period() - 1, strip.period(), 9_999] {
                assert_eq!(strip.window(offset), want, "{style:?} @ {offset}");
            }
        }
    }

    /// #839, symptom one: **light only ever lands on the fixed grid.** Every
    /// pixel that departs from the unlit reference sits inside the grid's dot
    /// cells — never in the bezel, never straddling two cells — at *every*
    /// offset, which is what makes a sub-dot position unrepresentable rather
    /// than merely unused. LCD: no bloom, so a lit dot is the only thing that
    /// can change a pixel.
    #[test]
    fn lit_dots_land_only_on_the_fixed_grid() {
        let style = DisplayStyle::Lcd;
        let strip = Marquee::new(style).window_px(96).render(LONG);
        assert!(strip.scrolls(), "the long message scrolls");
        let want = reference(style, 96);
        let grid_x = strip.origin_x..strip.origin_x + strip.cols * DOT;
        let grid_y = PAD..PAD + font::GLYPH_H * DOT;
        for offset in [0, 1, 2, 3, 4, 5, 11, strip.period() - 1] {
            let f = strip.window(offset);
            for y in 0..f.height() {
                for x in 0..f.width() {
                    if f.at(x, y) != want.at(x, y) {
                        assert!(
                            grid_x.contains(&x) && grid_y.contains(&y),
                            "light at ({x},{y}) is off the grid @ {offset}"
                        );
                    }
                }
            }
        }
    }

    /// One step of the offset shifts the lit pattern by exactly **one dot
    /// cell** — cell `c` at `offset + 1` is what cell `c + 1` showed at
    /// `offset`. Checked across the loop seam too (`period - 1` → `period`,
    /// which wraps to 0), so the wrap-around is seamless dot-wise and not just
    /// frame-equal.
    #[test]
    fn one_step_shifts_the_lit_pattern_by_one_dot() {
        let style = DisplayStyle::Lcd;
        let strip = Marquee::new(style).window_px(96).render(LONG);
        assert!(strip.scrolls(), "the long message scrolls");
        let want = reference(style, 96);
        for offset in [0, 1, 6, strip.period() - 1] {
            let a = lit_cells(&strip, &strip.window(offset), &want);
            let b = lit_cells(&strip, &strip.window(offset + 1), &want);
            for (c, (next, prev)) in b.iter().zip(a.iter().skip(1)).enumerate() {
                assert_eq!(next, prev, "cell {c} @ {offset} → {}", offset + 1);
            }
        }
    }

    /// The pixel-level consequence of the dot-cell shift: one step pans the
    /// grid by a whole `DOT` pitch, so the grid's interior columns are the same
    /// buffer columns `DOT` px over. (LCD again — a bloom halo crossing the
    /// window edge would blur the comparison at the margins.)
    #[test]
    fn one_step_pans_the_grid_by_one_dot_pitch() {
        let strip = Marquee::new(DisplayStyle::Lcd).window_px(96).render(LONG);
        let a = strip.window(3);
        let b = strip.window(4);
        for x in strip.origin_x..strip.origin_x + (strip.cols - 1) * DOT {
            assert_eq!(column(&b, x), column(&a, x + DOT), "shift at column {x}");
        }
    }

    /// A lit dot renders exactly as the static display's — same falloff, same
    /// ghost underneath, same skin passes. A held single glyph puts the
    /// marquee's first cell where `dot_matrix`'s first cell is, so the two
    /// glyph blocks compare pixel for pixel.
    ///
    /// Skins carrying a **screen-space** pass are excluded, and have to be: the
    /// CRT's masks are functions of the pixel's place on *its own screen*
    /// (#397), so the same dot at the same buffer coordinate is attenuated
    /// differently in a 96 px window than in a 28 px static frame — by design,
    /// since the two are different tubes. What this test protects, that a
    /// marquee dot *is* a `dot_matrix` dot, still holds underneath the pass:
    /// `style.rs`'s `the_mask_only_attenuates_light_the_skin_already_stamped`
    /// is where that lands. Filtered rather than listed so a fourth mask-free
    /// skin joins automatically.
    #[test]
    fn a_lit_dot_matches_the_static_display() {
        for style in DisplayStyle::ALL
            .into_iter()
            .filter(|s| s.palette().mask.is_none())
        {
            let statik = dot_matrix("A", style);
            let strip = Marquee::new(style).window_px(96).render("A");
            assert!(!strip.scrolls(), "one glyph fits the window");
            let f = strip.window(0);
            assert_eq!(f.height(), statik.height(), "{style:?} same dot rows");
            for row in 0..font::GLYPH_H {
                for col in 0..font::GLYPH_W {
                    for dy in 0..DOT {
                        for dx in 0..DOT {
                            let (x, y) = (col * DOT + dx, PAD + row * DOT + dy);
                            assert_eq!(
                                f.at(strip.origin_x + x, y),
                                statik.at(PAD + x, y),
                                "{style:?} dot ({col},{row}) px ({dx},{dy})"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Offset 0 and offset N render different windows (it really scrolls), and
    /// a single-dot step is already visible — the ticker moves every frame.
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
        assert_eq!(
            strip.window(p - 1),
            strip.window(2 * p - 1),
            "including the last dot before the seam"
        );
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
        let m = Marquee::new(DisplayStyle::Lcd).window_px(88).gap_dots(3);
        assert_eq!(m.render(LONG).window(6), m.render(LONG).window(6));
    }

    /// The gap widens the loop period — in dots — without changing the message
    /// pixels: a bigger seam, same scroll.
    #[test]
    fn gap_widens_the_period() {
        let narrow = Marquee::new(DisplayStyle::Oled)
            .window_px(64)
            .gap_dots(1)
            .render(LONG);
        let wide = Marquee::new(DisplayStyle::Oled)
            .window_px(64)
            .gap_dots(10)
            .render(LONG);
        assert_eq!(wide.period(), narrow.period() + 9);
        // The window at offset 0 starts on the same bitmap columns either way.
        assert_eq!(narrow.window(0), wide.window(0));
    }

    /// The window geometry: whole dot cells between bezels at least `PAD` wide,
    /// centered, and the same dot rows the static display uses — so a marquee
    /// stacks flush with a `dot_matrix` readout.
    #[test]
    fn the_grid_fills_the_window_in_whole_dots() {
        for window_px in [64, 96, 200, 268, 269, 270, 271] {
            let strip = Marquee::new(DisplayStyle::Vfd)
                .window_px(window_px)
                .render(LONG);
            let margin = window_px - strip.cols * DOT;
            assert!(margin >= 2 * PAD, "{window_px}: bezel keeps {margin} px");
            assert!(margin < 2 * PAD + DOT, "{window_px}: no room left over");
            assert_eq!(strip.origin_x, margin / 2, "{window_px}: centered");
            assert_eq!(strip.height(), 2 * PAD + font::GLYPH_H * DOT);
        }
        // The 11-char static ticker and a 268 px marquee share a grid.
        let strip = Marquee::new(DisplayStyle::Vfd).window_px(268).render(LONG);
        assert_eq!(
            strip.cols,
            11 * (font::GLYPH_W + font::SPACING) - font::SPACING
        );
        assert_eq!(strip.origin_x, PAD);
    }

    /// A window too narrow for a single dot degrades to a bare field instead of
    /// panicking or wrapping — every kit entry point is total.
    #[test]
    fn a_window_narrower_than_a_dot_still_renders() {
        for window_px in [0, 1, PAD, 2 * PAD, 2 * PAD + DOT - 1] {
            let strip = Marquee::new(DisplayStyle::Lcd)
                .window_px(window_px)
                .render(LONG);
            assert_eq!(strip.cols, 0, "{window_px}: no whole dot fits");
            let f = strip.window(7);
            assert_eq!(f.width(), window_px);
            assert_eq!(f.data().len(), f.width() * f.height() * 4);
        }
    }

    /// The font-space bitmap is one bit per virtual pixel, on the same metrics
    /// the static display advances by: `GLYPH_W` columns per char, `SPACING`
    /// blank between chars, and an uncovered char as the notdef box.
    #[test]
    fn the_bitmap_is_font_space() {
        assert!(rasterize("").is_empty());
        assert_eq!(rasterize("A").len(), font::GLYPH_W);
        assert_eq!(
            rasterize("AB").len(),
            2 * font::GLYPH_W + font::SPACING,
            "one blank column between glyphs"
        );
        assert!(
            rasterize(" ").iter().all(|&c| c == 0),
            "a space lights nothing"
        );
        // The notdef box's outer columns are solid — an uncovered char still
        // renders something rather than vanishing.
        let notdef = rasterize("💕");
        assert_eq!(notdef.len(), font::GLYPH_W);
        assert_eq!(
            notdef[0], 0b0111_1111,
            "notdef's left edge is a full column"
        );
        // Row bits read top-down: 'A' has a gap in its top row's outer columns.
        let a = rasterize("A");
        assert_eq!(a.len(), font::GLYPH_W);
        assert_eq!(a[0] & 1, 0, "'A' is not lit at its top-left corner");
    }
}
