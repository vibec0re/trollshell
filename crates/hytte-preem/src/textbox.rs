//! The **8bit textbox**: wrapped 5×7 pixel-font text on a rounded, opaque
//! field — the pet's speech bubble (#304/#323), promoted to a configurable
//! kit widget (#356). The pet now consumes this type dressed in its lilac.

use super::font;
use super::frame::{Frame, Rgba};
use super::style::{DisplayStyle, mix};

/// How the box picks its wrap width.
#[derive(Debug, Clone, Copy)]
enum WidthSpec {
    /// An explicit wrap width in glyph cells.
    Cols(usize),
    /// Fit a **final** (post-[`scale`](TextBox::scale)) pixel budget: the
    /// widest column count whose rendered line — padding included — still
    /// fits `px` after the upscale.
    FitPx(usize),
}

/// A builder for chunky pixel-font text boxes rendered to a [`Frame`].
///
/// Defaults: 16 columns, 3 lines max, 3 px padding, 2 px rounded-corner
/// cut, 1× scale, width hugging the text, white-on-black. Every knob is a
/// consuming builder method; [`render`](Self::render) is reusable — the
/// builder is a value, not a one-shot.
///
/// ```
/// use hytte_preem::TextBox;
///
/// let frame = TextBox::new().cols(10).scale(2).render("mrrp!");
/// assert_eq!(frame.data().len(), frame.width() * frame.height() * 4);
/// ```
#[derive(Debug, Clone)]
pub struct TextBox {
    width: WidthSpec,
    max_lines: usize,
    pad: usize,
    corner: usize,
    scale: usize,
    fixed_width: bool,
    bg: Rgba,
    ink: Rgba,
    notdef: Rgba,
}

impl Default for TextBox {
    fn default() -> Self {
        Self::new()
    }
}

impl TextBox {
    /// A textbox with the defaults above.
    #[must_use]
    pub fn new() -> Self {
        Self {
            width: WidthSpec::Cols(16),
            max_lines: 3,
            pad: 3,
            corner: 2,
            scale: 1,
            fixed_width: false,
            bg: [0x00, 0x00, 0x00, 0xff],
            ink: [0xff, 0xff, 0xff, 0xff],
            notdef: [0x80, 0x80, 0x80, 0xff],
        }
    }

    /// A textbox dressed in a [`DisplayStyle`]'s palette: the style's field
    /// and ink, with the ghost color (or a dim ink mix where the style has
    /// no ghost) as the `.notdef` box color.
    #[must_use]
    pub fn styled(style: DisplayStyle) -> Self {
        let p = style.palette();
        let notdef = p.ghost.unwrap_or_else(|| mix(p.bg, p.ink, 110));
        Self::new().colors(p.bg, p.ink, notdef)
    }

    /// Wrap width in glyph cells (clamped to at least 1).
    #[must_use]
    pub fn cols(mut self, cols: usize) -> Self {
        self.width = WidthSpec::Cols(cols.max(1));
        self
    }

    /// Pick the widest wrap width whose rendered box — padding included —
    /// still fits `px` **final** pixels, i.e. after [`scale`](Self::scale)
    /// (order-independent: the budget is resolved at render time).
    #[must_use]
    pub fn fit_px(mut self, px: usize) -> Self {
        self.width = WidthSpec::FitPx(px);
        self
    }

    /// Hard cap on wrapped lines (clamped to at least 1); overflow is
    /// truncated with a trailing `…`.
    #[must_use]
    pub fn max_lines(mut self, lines: usize) -> Self {
        self.max_lines = lines.max(1);
        self
    }

    /// Field padding around the text block, in pre-scale pixels.
    #[must_use]
    pub fn pad(mut self, pad: usize) -> Self {
        self.pad = pad;
        self
    }

    /// Radius of the rounded-corner cut (to transparent), in pre-scale
    /// pixels; `0` keeps the box fully opaque to its square corners.
    #[must_use]
    pub fn corner(mut self, corner: usize) -> Self {
        self.corner = corner;
        self
    }

    /// Integer upscale baked into the buffer (chunkier pixels; see the
    /// `preem` docs on sizing). `0`/`1` render at native 1×.
    #[must_use]
    pub fn scale(mut self, scale: usize) -> Self {
        self.scale = scale.max(1);
        self
    }

    /// `true` renders a **fixed-width** slot — the full wrap width even for
    /// short text — so the box never resizes with the message (#323, the
    /// pet's bubble). `false` (the default) hugs the longest line.
    #[must_use]
    pub fn fixed_width(mut self, fixed: bool) -> Self {
        self.fixed_width = fixed;
        self
    }

    /// The field, text ink, and `.notdef` box colors.
    #[must_use]
    pub fn colors(mut self, bg: Rgba, ink: Rgba, notdef: Rgba) -> Self {
        self.bg = bg;
        self.ink = ink;
        self.notdef = notdef;
        self
    }

    /// Render `text` into a fresh [`Frame`]. Valid for **every** input —
    /// the empty string, uncovered chars, overlong words — the buffer
    /// always satisfies the host's `len == w * h * 4` invariant.
    #[must_use]
    pub fn render(&self, text: &str) -> Frame {
        let cols = self.resolve_cols();
        let lines = font::wrap(text, cols, self.max_lines);
        let content_cols = if self.fixed_width {
            cols
        } else {
            lines.iter().map(|l| l.chars().count()).max().unwrap_or(0)
        };
        let n_lines = lines.len().max(1);
        let buf_w = (2 * self.pad + font::line_px(content_cols)).max(1);
        let buf_h = 2 * self.pad + n_lines * font::GLYPH_H + (n_lines - 1) * font::LINE_GAP;

        let mut frame = Frame::new(buf_w, buf_h);
        self.fill_background(&mut frame);
        for (row, line) in lines.iter().enumerate() {
            let oy = self.pad + row * (font::GLYPH_H + font::LINE_GAP);
            for (col, ch) in line.chars().enumerate() {
                let ox = self.pad + col * (font::GLYPH_W + font::SPACING);
                self.blit_glyph(&mut frame, ox, oy, ch);
            }
        }
        if self.scale > 1 {
            frame.upscale(self.scale)
        } else {
            frame
        }
    }

    /// The wrap width in glyph cells this box actually uses.
    fn resolve_cols(&self) -> usize {
        match self.width {
            WidthSpec::Cols(cols) => cols,
            WidthSpec::FitPx(px) => {
                let content = (px / self.scale.max(1)).saturating_sub(2 * self.pad);
                font::max_cols_for(content)
            }
        }
    }

    /// Paint the opaque field with the corners cut to transparent, so the
    /// upscaled buffer reads as a softly rounded chip.
    fn fill_background(&self, frame: &mut Frame) {
        let (w, h) = (frame.width(), frame.height());
        let (last_x, last_y) = (w.saturating_sub(1), h.saturating_sub(1));
        for y in 0..h {
            for x in 0..w {
                let dx = corner_delta(x, last_x, self.corner);
                let dy = corner_delta(y, last_y, self.corner);
                if dx * dx + dy * dy <= self.corner * self.corner {
                    frame.set(x, y, self.bg);
                }
            }
        }
    }

    /// Blit one glyph at cell origin (`ox`, `oy`); an uncovered char draws
    /// the dim hollow [`font::NOTDEF`] box instead.
    fn blit_glyph(&self, frame: &mut Frame, ox: usize, oy: usize, ch: char) {
        let (rows, color) = match font::glyph(ch) {
            Some(rows) => (rows, self.ink),
            None => (&font::NOTDEF, self.notdef),
        };
        for (ry, &bits) in rows.iter().enumerate() {
            for cx in 0..font::GLYPH_W {
                if (bits >> (font::GLYPH_W - 1 - cx)) & 1 == 1 {
                    frame.set(ox + cx, oy + ry, color);
                }
            }
        }
    }
}

/// Distance a coordinate `v` pokes into the `corner` margin at either end
/// of a `0..=last` span (`0` in the middle) — the rounded-corner test in
/// [`TextBox::fill_background`].
fn corner_delta(v: usize, last: usize, corner: usize) -> usize {
    corner
        .saturating_sub(v)
        .max((v + corner).saturating_sub(last))
}

#[cfg(test)]
mod tests {
    use super::super::DisplayStyle;
    use super::TextBox;

    /// The host invariant, across pathological inputs, styles, and scales.
    #[test]
    fn every_buffer_satisfies_the_host_invariant() {
        let texts = [
            "",
            "hi",
            "smörgåsbord ölçäÜ é ßÅÄÖ",
            "💕 unmapped: ☺ \u{1F63A}",
            "supercalifragilisticexpialidocious",
            &"a".repeat(80),
        ];
        let boxes = [
            TextBox::new(),
            TextBox::new().cols(9).scale(2).fixed_width(true),
            TextBox::styled(DisplayStyle::Vfd).fit_px(126),
            TextBox::styled(DisplayStyle::Oled).corner(0).pad(0),
        ];
        for b in &boxes {
            for text in texts {
                let f = b.render(text);
                assert_eq!(
                    f.data().len(),
                    f.width() * f.height() * 4,
                    "buffer for {text:?} must be w*h*4"
                );
                assert!(f.width() > 0 && f.height() > 0, "{text:?} non-degenerate");
            }
        }
    }

    /// Renders are pure: same input, same bytes.
    #[test]
    fn render_is_deterministic() {
        let b = TextBox::styled(DisplayStyle::Vfd).cols(12).scale(2);
        assert_eq!(b.render("mrrp!"), b.render("mrrp!"));
    }

    /// A fixed-width box keeps one width across message lengths (#323);
    /// a hugging box does not.
    #[test]
    fn fixed_width_pins_the_slot() {
        let fixed = TextBox::new().cols(9).fixed_width(true);
        assert_eq!(
            fixed.render("hi").width(),
            fixed.render("a longer line that wraps").width()
        );
        let hug = TextBox::new().cols(9);
        assert!(hug.render("hi").width() < hug.render("wider line").width());
    }

    /// `fit_px` accounts for scale and padding: the final buffer never
    /// exceeds the budget (for any budget that fits at least one column).
    #[test]
    fn fit_px_respects_the_final_budget() {
        for px in [60, 126, 200, 296] {
            for scale in [1, 2] {
                let b = TextBox::new().scale(scale).fit_px(px).fixed_width(true);
                let f = b.render("wwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwwww");
                assert!(
                    f.width() <= px,
                    "budget {px} at {scale}x: got {}",
                    f.width()
                );
            }
        }
    }

    /// The corner cut leaves transparent pixels; `corner(0)` is fully
    /// opaque — the "full alpha where the style promises opacity" check.
    #[test]
    fn corners_cut_transparent_and_corner_zero_is_opaque() {
        let round = TextBox::new().render("hej");
        assert_eq!(round.get(0, 0).map(|px| px[3]), Some(0));
        let square = TextBox::new().corner(0).render("hej");
        assert!(square.data().chunks_exact(4).all(|px| px[3] == 0xff));
    }

    /// Scaling multiplies the buffer dimensions exactly.
    #[test]
    fn scale_multiplies_dimensions() {
        let one = TextBox::new().cols(9).fixed_width(true).render("hi");
        let two = TextBox::new()
            .cols(9)
            .fixed_width(true)
            .scale(2)
            .render("hi");
        assert_eq!(two.width(), one.width() * 2);
        assert_eq!(two.height(), one.height() * 2);
    }

    /// Text draws ink; an uncovered char draws the notdef color instead.
    #[test]
    fn ink_and_notdef_pixels_appear() {
        let ink = [1, 2, 3, 0xff];
        let notdef = [7, 8, 9, 0xff];
        let b = TextBox::new().colors([0, 0, 0, 0xff], ink, notdef);
        let has = |f: &super::Frame, c: [u8; 4]| f.data().chunks_exact(4).any(|px| px == c);
        let covered = b.render("hej");
        assert!(has(&covered, ink), "covered text draws ink");
        assert!(!has(&covered, notdef));
        let boxed = b.render("💕");
        assert!(has(&boxed, notdef), "uncovered char draws the notdef box");
    }

    /// Long text wraps down: the buffer grows taller, never wider than the
    /// fixed slot.
    #[test]
    fn long_text_wraps_taller_not_wider() {
        let b = TextBox::new().cols(9).fixed_width(true);
        let short = b.render("hi");
        let long = b.render(&"word ".repeat(12));
        assert!(long.height() > short.height());
        assert_eq!(long.width(), short.width());
    }
}
