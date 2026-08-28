//! The **seven-segment readout**: classic tapered-hexagon segment geometry
//! with the authentic dim ghost of every unlit segment behind the lit ones,
//! in any [`DisplayStyle`] skin. Digits, `:`, `-`, and space — enough for a
//! clock, a counter, or a countdown.
//!
//! One renderer, style palettes on top: the ghost pass paints all eight
//! elements flat (skipped entirely on OLED — an off segment emits nothing),
//! the lit pass stamps the active segments into the emission grid, and the
//! style's bloom (VFD phosphor, OLED pixel glow) bleeds off them.
//!
//! # Geometry
//!
//! Segments are tapered bars — each row/column of the bar is inset toward
//! its ends, so adjoining segments meet in the classic mitred diagonal gaps
//! instead of butting squarely. A digit cell is
//! [`DIGIT_W`]`×`[`DIGIT_H`] px, a colon cell [`COLON_W`] px wide;
//! `"12:34"` comes out 188×70 — sized for a sidebar card at natural size
//! (the #313 lesson), with [`Frame::upscale`] there if you want it chunkier.

use super::frame::Frame;
use super::style::{DisplayStyle, Emission};

/// Segment bar thickness in pixels.
const THICK: usize = 6;
/// Digit cell width.
const DIGIT_W: usize = 30;
/// Digit cell height.
const DIGIT_H: usize = 54;
/// Colon cell width.
const COLON_W: usize = 12;
/// Gap between adjacent cells.
const GAP: usize = 10;
/// Field padding around the readout.
const PAD: usize = 8;

/// Segment bits, the classic lettering: `A` top, `B` top-right, `C`
/// bottom-right, `D` bottom, `E` bottom-left, `F` top-left, `G` middle.
const SEG_A: u8 = 1;
const SEG_B: u8 = 1 << 1;
const SEG_C: u8 = 1 << 2;
const SEG_D: u8 = 1 << 3;
const SEG_E: u8 = 1 << 4;
const SEG_F: u8 = 1 << 5;
const SEG_G: u8 = 1 << 6;
/// All seven segments — the ghost pass and the digit 8.
const SEG_ALL: u8 = 0x7f;

/// One display cell: a digit-shaped cell with a lit-segment mask, or the
/// two-dot colon.
#[derive(Clone, Copy)]
enum Cell {
    /// A digit-width cell; `0` lights nothing (space / unknown chars).
    Digit(u8),
    /// A colon cell, both dots lit.
    Colon,
}

impl Cell {
    fn width(self) -> usize {
        match self {
            Self::Digit(_) => DIGIT_W,
            Self::Colon => COLON_W,
        }
    }
}

/// The lit-segment mask for one char. Space lights nothing (ghosts still
/// show); an unmapped char renders exactly like space rather than panicking
/// or guessing.
fn cell(c: char) -> Cell {
    if c == ':' {
        return Cell::Colon;
    }
    Cell::Digit(match c {
        '0' => SEG_A | SEG_B | SEG_C | SEG_D | SEG_E | SEG_F,
        '1' => SEG_B | SEG_C,
        '2' => SEG_A | SEG_B | SEG_G | SEG_E | SEG_D,
        '3' => SEG_A | SEG_B | SEG_G | SEG_C | SEG_D,
        '4' => SEG_F | SEG_G | SEG_B | SEG_C,
        '5' => SEG_A | SEG_F | SEG_G | SEG_C | SEG_D,
        '6' => SEG_A | SEG_F | SEG_G | SEG_E | SEG_D | SEG_C,
        '7' => SEG_A | SEG_B | SEG_C,
        '8' => SEG_ALL,
        '9' => SEG_A | SEG_B | SEG_C | SEG_D | SEG_F | SEG_G,
        '-' => SEG_G,
        _ => 0,
    })
}

/// Render `text` as a seven-segment readout in `style`.
///
/// Understands digits, `:`, `-`, and space; anything else renders as a
/// blank (all-ghost) cell. The buffer is fully opaque and always satisfies
/// the host's `len == w * h * 4` invariant, empty string included. Width
/// grows with the cell count — `"12:34"` is 188 px at the current metrics,
/// comfortably inside a ~296 px sidebar card.
#[must_use]
pub fn seven_seg(text: &str, style: DisplayStyle) -> Frame {
    let palette = style.palette();
    // Lay the cells out once; both passes walk the same origins.
    let mut cells: Vec<(usize, Cell)> = Vec::new();
    let mut x = PAD;
    for c in text.chars() {
        let cl = cell(c);
        cells.push((x, cl));
        x += cl.width() + GAP;
    }
    let width = cells
        .last()
        .map_or(2 * PAD, |(ox, cl)| ox + cl.width() + PAD);
    let height = 2 * PAD + DIGIT_H;
    let mut frame = Frame::filled(width, height, palette.bg);

    // Ghost pass: every element of every cell, flat and dim.
    if let Some(ghost) = palette.ghost {
        for &(ox, cl) in &cells {
            let mut paint = |px: usize, py: usize| frame.set(px, py, ghost);
            match cl {
                Cell::Digit(_) => stamp_digit(ox, PAD, SEG_ALL, &mut paint),
                Cell::Colon => stamp_colon(ox, PAD, &mut paint),
            }
        }
    }

    // Lit pass: the active segments, bloomed and composited toward the ink.
    let mut lit = Emission::new(width, height);
    for &(ox, cl) in &cells {
        let mut stamp = |px: usize, py: usize| lit.add(px, py, 255);
        match cl {
            Cell::Digit(mask) => stamp_digit(ox, PAD, mask, &mut stamp),
            Cell::Colon => stamp_colon(ox, PAD, &mut stamp),
        }
    }
    if let Some(bloom) = palette.bloom {
        lit.bloom(bloom);
    }
    lit.composite(&mut frame, palette.ink, palette.mask);
    frame
}

/// Emit the pixels of a digit cell's segments per `mask`, cell origin at
/// (`ox`, `oy`), through `sink` — the geometry is shared verbatim by the
/// ghost pass (paint) and the lit pass (stamp).
fn stamp_digit(ox: usize, oy: usize, mask: u8, sink: &mut impl FnMut(usize, usize)) {
    // G's top row; horizontal bars span the cell minus 1 px at each end.
    let mid = (DIGIT_H - THICK) / 2;
    let hbar_x = ox + 1;
    let hbar_len = DIGIT_W - 2;
    if mask & SEG_A != 0 {
        stamp_hbar(hbar_x, oy, hbar_len, sink);
    }
    if mask & SEG_G != 0 {
        stamp_hbar(hbar_x, oy + mid, hbar_len, sink);
    }
    if mask & SEG_D != 0 {
        stamp_hbar(hbar_x, oy + DIGIT_H - THICK, hbar_len, sink);
    }
    // Verticals stop just short of G on both sides of it.
    let upper_len = mid - 2;
    let lower_y = mid + THICK + 1;
    let lower_len = DIGIT_H - 1 - lower_y;
    if mask & SEG_F != 0 {
        stamp_vbar(ox, oy + 1, upper_len, sink);
    }
    if mask & SEG_B != 0 {
        stamp_vbar(ox + DIGIT_W - THICK, oy + 1, upper_len, sink);
    }
    if mask & SEG_E != 0 {
        stamp_vbar(ox, oy + lower_y, lower_len, sink);
    }
    if mask & SEG_C != 0 {
        stamp_vbar(ox + DIGIT_W - THICK, oy + lower_y, lower_len, sink);
    }
}

/// Emit a colon cell's two dots through `sink`.
fn stamp_colon(ox: usize, oy: usize, sink: &mut impl FnMut(usize, usize)) {
    let cx = ox + (COLON_W - THICK) / 2;
    for cy in [DIGIT_H / 3 - THICK / 2, 2 * DIGIT_H / 3 - THICK / 2] {
        for dy in 0..THICK {
            for dx in 0..THICK {
                sink(cx + dx, oy + cy + dy);
            }
        }
    }
}

/// A horizontal tapered bar: row `j` of [`THICK`] is inset from both ends
/// by its distance from the bar's center line, giving the hexagonal
/// segment shape.
fn stamp_hbar(x0: usize, y0: usize, len: usize, sink: &mut impl FnMut(usize, usize)) {
    for j in 0..THICK {
        let inset = taper(j);
        for x in (x0 + inset)..(x0 + len.saturating_sub(inset)) {
            sink(x, y0 + j);
        }
    }
}

/// A vertical tapered bar, the mirror of [`stamp_hbar`].
fn stamp_vbar(x0: usize, y0: usize, len: usize, sink: &mut impl FnMut(usize, usize)) {
    for i in 0..THICK {
        let inset = taper(i);
        for y in (y0 + inset)..(y0 + len.saturating_sub(inset)) {
            sink(x0 + i, y);
        }
    }
}

/// End-inset of a bar's row/column `k` (of [`THICK`]): 0 on the center
/// rows, growing toward the faces — the taper that mitres the segments.
fn taper(k: usize) -> usize {
    (2 * k).abs_diff(THICK - 1).saturating_sub(1) / 2
}

#[cfg(test)]
mod tests {
    use super::super::DisplayStyle;
    use super::{DIGIT_H, PAD, seven_seg, taper};

    /// The host invariant across styles and inputs, empty string included.
    #[test]
    fn every_buffer_satisfies_the_host_invariant() {
        for style in DisplayStyle::ALL {
            for text in ["", " ", "12:34", "88:88", "-", "9876543210", "x?"] {
                let f = seven_seg(text, style);
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
            let f = seven_seg("12:34", style);
            assert!(
                f.data().chunks_exact(4).all(|px| px[3] == 0xff),
                "{style:?} frame is opaque wall to wall"
            );
        }
    }

    #[test]
    fn render_is_deterministic() {
        assert_eq!(
            seven_seg("07:16", DisplayStyle::Oled),
            seven_seg("07:16", DisplayStyle::Oled)
        );
    }

    #[test]
    fn styles_render_differently() {
        let vfd = seven_seg("88", DisplayStyle::Vfd);
        let lcd = seven_seg("88", DisplayStyle::Lcd);
        let oled = seven_seg("88", DisplayStyle::Oled);
        assert_ne!(vfd, lcd);
        assert_ne!(vfd, oled);
        assert_ne!(lcd, oled);
    }

    /// Ghost segments: a blank cell shows the full ghost figure-8 on LCD
    /// and VFD, but an unlit OLED emits nothing (Annika's no-ghosting rule).
    #[test]
    fn ghosts_show_except_on_oled() {
        for style in [DisplayStyle::Lcd, DisplayStyle::Vfd] {
            let f = seven_seg(" ", style);
            let bg = style.palette().bg;
            assert!(
                f.data().chunks_exact(4).any(|px| px != bg),
                "{style:?} paints ghost segments behind an unlit cell"
            );
        }
        let oled = seven_seg(" ", DisplayStyle::Oled);
        assert!(
            oled.data().chunks_exact(4).all(|px| px == [0, 0, 0, 0xff]),
            "an unlit OLED cell is true black"
        );
    }

    /// Every distinct digit renders a distinct cell (mask table sanity).
    #[test]
    fn digits_are_pairwise_distinct() {
        let renders: Vec<_> = ('0'..='9')
            .map(|c| seven_seg(&c.to_string(), DisplayStyle::Lcd))
            .collect();
        for (i, a) in renders.iter().enumerate() {
            for b in renders.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
    }

    /// Unknown chars render exactly like a space — deterministic blanks.
    #[test]
    fn unknown_chars_render_as_blank_cells() {
        assert_eq!(
            seven_seg("x", DisplayStyle::Vfd),
            seven_seg(" ", DisplayStyle::Vfd)
        );
    }

    /// A colon cell is narrower than a digit cell; the clock face fits the
    /// ~296 px sidebar card (the #313 lesson).
    #[test]
    fn layout_metrics_hold() {
        let colon = seven_seg(":", DisplayStyle::Lcd);
        let digit = seven_seg("8", DisplayStyle::Lcd);
        assert!(colon.width() < digit.width());
        assert_eq!(digit.height(), 2 * PAD + DIGIT_H);
        let clock = seven_seg("12:34", DisplayStyle::Vfd);
        assert!(clock.width() <= 296, "{}", clock.width());
    }

    /// The taper insets the outer rows/columns and leaves the center full.
    #[test]
    fn taper_is_symmetric_and_centered() {
        assert_eq!(taper(0), 2);
        assert_eq!(taper(1), 1);
        assert_eq!(taper(2), 0);
        assert_eq!(taper(3), 0);
        assert_eq!(taper(4), 1);
        assert_eq!(taper(5), 2);
    }
}
