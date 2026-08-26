//! The **dot-matrix display**: one line of text as char cells of round
//! dots, in any [`DisplayStyle`] skin — the kit's ticker/readout widget.
//!
//! Each 5×7 font pixel renders as a [`DOT`]×[`DOT`] dot with a radial
//! falloff (bright core, dim rim), so the grid reads as discrete round dots
//! rather than square pixels. Ghosting styles (VFD faintly, LCD visibly)
//! paint **every** dot position of every cell first — the unlit matrix
//! showing through, exactly like the hardware — and glowing styles bleed a
//! halo off the lit dots. Single line by design: it's a ticker, not a
//! paragraph — wrapped text is [`TextBox`](super::TextBox)'s job.
//!
//! # The static display's grid is per **character cell**
//!
//! [`dot_matrix`] models a fixed row of char cells: the ghost pass paints the
//! `GLYPH_W`×`GLYPH_H` dots of each cell, and the spacing column between two
//! cells carries no dots — the hardware has none there either. That cell
//! structure is right for a *static* readout and deliberately wrong for a
//! *scrolling* one, where a travelling cell structure is exactly what can't
//! be scrolled smoothly (#839). [`Marquee`](super::Marquee) therefore does
//! **not** scroll a `dot_matrix` render: it owns its own continuous grid and
//! reuses this module's dot *hardware* — the [`DOT`] pitch, the [`PAD`]
//! bezel, and the [`ghost_dot`]/[`lit_dot`] painters — so a marquee dot is
//! pixel-for-pixel the dot you see here.

use super::font;
use super::frame::{Frame, Rgba};
use super::style::{DisplayStyle, Emission, mix};

/// Edge length of one dot cell in buffer pixels: every font pixel becomes a
/// `DOT`×`DOT` round dot. At 4 px a char cell advances 24 px, so ~11 chars
/// fill a ~296 px sidebar card (see the `preem` docs on sizing).
///
/// This is the kit's **virtual pixel**: the physical dot pitch of every
/// dot-matrix surface. [`Marquee`](super::Marquee) scrolls in whole units of
/// it, never in buffer pixels (#839).
pub(super) const DOT: usize = 4;

/// Field padding around the dot grid, one dot cell on every side.
pub(super) const PAD: usize = DOT;

/// The radial falloff of one dot: intensity per pixel of the `DOT`×`DOT`
/// cell (0..=255), bright 2×2 core, dim rim, near-dark corners — what makes
/// the dots read as *round* under nearest-neighbor upscaling.
const FALLOFF: [[u16; DOT]; DOT] = [
    [25, 120, 120, 25],
    [120, 255, 255, 120],
    [120, 255, 255, 120],
    [25, 120, 120, 25],
];

/// Render one line of `text` as a dot-matrix display in `style`.
///
/// The buffer is fully opaque and always satisfies the host's
/// `len == w * h * 4` invariant, for any input including the empty string.
/// An uncovered char renders as the hollow [`font::NOTDEF`] box. Width
/// grows linearly with the char count — `2*PAD + n*24 - 4` px at the
/// current metrics — so keep tickers to ~11 chars for a sidebar card.
#[must_use]
pub fn dot_matrix(text: &str, style: DisplayStyle) -> Frame {
    let palette = style.palette();
    let n = text.chars().count();
    let advance = (font::GLYPH_W + font::SPACING) * DOT;
    let width = if n == 0 {
        2 * PAD
    } else {
        2 * PAD + n * advance - font::SPACING * DOT
    };
    let height = 2 * PAD + font::GLYPH_H * DOT;
    let mut frame = Frame::filled(width, height, palette.bg);

    // Ghost pass: the unlit matrix shows through on ghosting styles — every
    // dot position of every char cell, lit or not (spacing columns carry no
    // dots on the hardware either).
    if let Some(ghost) = palette.ghost {
        for cell in 0..n {
            let ox = PAD + cell * advance;
            for row in 0..font::GLYPH_H {
                for col in 0..font::GLYPH_W {
                    ghost_dot(&mut frame, ox + col * DOT, PAD + row * DOT, ghost);
                }
            }
        }
    }

    // Lit pass: stamp each set font pixel as a falloff dot, bloom if the
    // style glows, then composite toward the ink.
    let mut lit = Emission::new(width, height);
    for (cell, ch) in text.chars().enumerate() {
        let rows = font::glyph(ch).unwrap_or(&font::NOTDEF);
        let ox = PAD + cell * advance;
        for (ry, &bits) in rows.iter().enumerate() {
            for cx in 0..font::GLYPH_W {
                if (bits >> (font::GLYPH_W - 1 - cx)) & 1 == 1 {
                    lit_dot(&mut lit, ox + cx * DOT, PAD + ry * DOT);
                }
            }
        }
    }
    if let Some(bloom) = palette.bloom {
        lit.bloom(bloom);
    }
    lit.composite(&mut frame, palette.ink);
    frame
}

/// Paint one unlit ghost dot flat into the frame: the falloff shape, mixed
/// from the field toward the ghost color. `x`/`y` are the top-left buffer
/// pixel of the `DOT`×`DOT` cell. Shared with [`Marquee`](super::Marquee) so
/// both surfaces show the *same* unlit matrix.
pub(super) fn ghost_dot(frame: &mut Frame, x: usize, y: usize, ghost: Rgba) {
    for (j, row) in FALLOFF.iter().enumerate() {
        for (i, &t) in row.iter().enumerate() {
            let under = frame.at(x + i, y + j);
            frame.set(x + i, y + j, mix(under, ghost, t));
        }
    }
}

/// Stamp one lit dot's falloff into the emission grid. `x`/`y` are the
/// top-left buffer pixel of the `DOT`×`DOT` cell. Shared with
/// [`Marquee`](super::Marquee) so a scrolled dot lights exactly like a
/// static one.
pub(super) fn lit_dot(lit: &mut Emission, x: usize, y: usize) {
    for (j, row) in FALLOFF.iter().enumerate() {
        for (i, &t) in row.iter().enumerate() {
            lit.add(x + i, y + j, t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::DisplayStyle;
    use super::{DOT, PAD, dot_matrix};

    /// The host invariant across styles and inputs, empty string included.
    #[test]
    fn every_buffer_satisfies_the_host_invariant() {
        for style in DisplayStyle::ALL {
            for text in ["", " ", "PREEM", "åäö 💕", "0123456789~/"] {
                let f = dot_matrix(text, style);
                assert_eq!(
                    f.data().len(),
                    f.width() * f.height() * 4,
                    "{style:?} {text:?}"
                );
                assert!(f.width() > 0 && f.height() > 0);
            }
        }
    }

    /// Display widgets promise fully opaque frames — they are screens.
    #[test]
    fn every_pixel_is_opaque() {
        for style in DisplayStyle::ALL {
            let f = dot_matrix("HELLO", style);
            assert!(
                f.data().chunks_exact(4).all(|px| px[3] == 0xff),
                "{style:?} frame is opaque wall to wall"
            );
        }
    }

    #[test]
    fn render_is_deterministic() {
        assert_eq!(
            dot_matrix("TICK", DisplayStyle::Vfd),
            dot_matrix("TICK", DisplayStyle::Vfd)
        );
    }

    /// The three skins must actually look different on the same text.
    #[test]
    fn styles_render_differently() {
        let vfd = dot_matrix("88", DisplayStyle::Vfd);
        let lcd = dot_matrix("88", DisplayStyle::Lcd);
        let oled = dot_matrix("88", DisplayStyle::Oled);
        assert_ne!(vfd, lcd);
        assert_ne!(vfd, oled);
        assert_ne!(lcd, oled);
    }

    /// Ghosting: an all-unlit cell (a space) shows the ghost matrix on LCD
    /// but stays true black on OLED — Annika's no-ghosting OLED rule.
    #[test]
    fn lcd_ghosts_and_oled_does_not() {
        let lcd = dot_matrix(" ", DisplayStyle::Lcd);
        let lcd_bg = DisplayStyle::Lcd.palette().bg;
        assert!(
            lcd.data().chunks_exact(4).any(|px| px != lcd_bg),
            "LCD paints ghost cells behind unlit dots"
        );
        let oled = dot_matrix(" ", DisplayStyle::Oled);
        assert!(
            oled.data().chunks_exact(4).all(|px| px == [0, 0, 0, 0xff]),
            "an unlit OLED emits nothing at all"
        );
    }

    /// Different text, different pixels (the widget actually renders text).
    #[test]
    fn text_changes_the_render() {
        assert_ne!(
            dot_matrix("AB", DisplayStyle::Vfd),
            dot_matrix("BA", DisplayStyle::Vfd)
        );
    }

    /// Width follows the documented per-char advance; height is fixed.
    #[test]
    fn dimensions_follow_the_metrics() {
        let empty = dot_matrix("", DisplayStyle::Lcd);
        assert_eq!(empty.width(), 2 * PAD);
        let three = dot_matrix("abc", DisplayStyle::Lcd);
        assert_eq!(three.width(), 2 * PAD + 3 * 6 * DOT - DOT);
        assert_eq!(three.height(), 2 * PAD + 7 * DOT);
        // 11 chars stay within the ~296 px sidebar card (the #313 lesson).
        let ticker = dot_matrix(&"x".repeat(11), DisplayStyle::Vfd);
        assert!(ticker.width() <= 296, "{}", ticker.width());
    }
}
