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

// The six metrics below, [`BARS`], [`COLON_DOTS`], [`taper`], [`layout`] and
// [`size`] are `pub` since #1154, re-exported at the crate root under the
// `SEVEN_SEG_`/`seven_seg_` family prefix a name like `GAP` or `size` needs
// once it leaves this module. The reason is the `Gauge::dial` one #1148 set
// and `LED_CELL_W` and friends followed (#1153): the shell's GPU arm
// re-rasterises this widget in GLSL, and a `.frag` cannot read a Rust `const`
// — so either the shell carries a hand mirror of this widget's segment
// geometry, or it reads it here and hands it to the shader as uniforms. It
// reads it here.
//
// [`BARS`] is the one item that is more than a visibility change:
// [`stamp_cell`] used to compute the seven segments' origins inline, so
// there was no value for the shell to read. It walks the table now, which
// makes the table the geometry's single definition rather than a second copy
// of it — and `the_rendered_bytes_are_pinned_by_digest` (taken on the tree
// *before* that move) is what says it moved no pixel.

/// Segment bar thickness in pixels.
pub const THICK: usize = 6;
/// Digit cell width.
pub const DIGIT_W: usize = 30;
/// Digit cell height.
pub const DIGIT_H: usize = 54;
/// Colon cell width.
pub const COLON_W: usize = 12;
/// Gap between adjacent cells.
pub const GAP: usize = 10;
/// Field padding around the readout.
pub const PAD: usize = 8;

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

/// One stamped element of a cell: an axis-aligned bar [`THICK`] px across and
/// `len` px along, its near corner at (`x`, `y`) **relative to the cell
/// origin**.
///
/// The shape the whole widget is built out of, and the value the shell's GL
/// arm reads instead of re-deriving (#1154). A `tapered` bar is the classic
/// mitred hexagon — each row/column inset from both ends by [`taper`] — and an
/// untapered one is a plain rectangle, which is what the colon's dots are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    /// Near-corner x, relative to the cell origin.
    pub x: usize,
    /// Near-corner y, relative to the cell origin.
    pub y: usize,
    /// Length along the bar's long axis.
    pub len: usize,
    /// `true` when `len` runs down the y axis and [`THICK`] across x.
    pub vertical: bool,
    /// `true` when the ends are mitred by [`taper`].
    pub tapered: bool,
}

impl Bar {
    /// A horizontal tapered segment.
    const fn hbar(x: usize, y: usize, len: usize) -> Self {
        Self {
            x,
            y,
            len,
            vertical: false,
            tapered: true,
        }
    }

    /// A vertical tapered segment.
    const fn vbar(x: usize, y: usize, len: usize) -> Self {
        Self {
            x,
            y,
            len,
            vertical: true,
            tapered: true,
        }
    }

    /// One colon dot: a plain [`THICK`]×[`THICK`] square, never mitred.
    const fn dot(x: usize, y: usize) -> Self {
        Self {
            x,
            y,
            len: THICK,
            vertical: false,
            tapered: false,
        }
    }
}

/// `G`'s top row — horizontal bars sit at the cell's vertical centre.
const MID: usize = (DIGIT_H - THICK) / 2;
/// Horizontal bars span the cell minus 1 px at each end.
const HBAR_X: usize = 1;
/// …which is [`DIGIT_W`] less those two.
const HBAR_LEN: usize = DIGIT_W - 2;
/// Verticals stop just short of `G` on both sides of it.
const UPPER_LEN: usize = MID - 2;
/// Where the lower pair starts.
const LOWER_Y: usize = MID + THICK + 1;
/// …and how far it runs.
const LOWER_LEN: usize = DIGIT_H - 1 - LOWER_Y;

/// The seven segments of a digit cell, **in the bit order of `SEG_A` …
/// `SEG_G`** — index `i` is the bar lit by bit `i` of a cell's mask.
///
/// The order is load-bearing: `stamp_cell` and the shell's shader both
/// index it by bit, so a reordering would light the wrong bars on both sides
/// at once.
pub const BARS: [Bar; 7] = [
    Bar::hbar(HBAR_X, 0, HBAR_LEN),                 // A — top
    Bar::vbar(DIGIT_W - THICK, 1, UPPER_LEN),       // B — top right
    Bar::vbar(DIGIT_W - THICK, LOWER_Y, LOWER_LEN), // C — bottom right
    Bar::hbar(HBAR_X, DIGIT_H - THICK, HBAR_LEN),   // D — bottom
    Bar::vbar(0, LOWER_Y, LOWER_LEN),               // E — bottom left
    Bar::vbar(0, 1, UPPER_LEN),                     // F — top left
    Bar::hbar(HBAR_X, MID, HBAR_LEN),               // G — middle
];

/// The colon cell's two dots, relative to its origin.
pub const COLON_DOTS: [Bar; 2] = [
    Bar::dot((COLON_W - THICK) / 2, DIGIT_H / 3 - THICK / 2),
    Bar::dot((COLON_W - THICK) / 2, 2 * DIGIT_H / 3 - THICK / 2),
];

/// One laid-out cell of a readout: where it starts in the buffer, and which
/// segments it lights — `None` for the colon, whose two dots are not segments
/// and take no mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    /// Buffer x of the cell's left edge. The top edge is always [`PAD`].
    pub x: usize,
    /// The lit segments as bits of [`BARS`], or `None` for a colon cell.
    pub mask: Option<u8>,
}

impl Cell {
    /// This cell's width: [`DIGIT_W`] for a digit, [`COLON_W`] for a colon.
    #[must_use]
    pub const fn width(self) -> usize {
        if self.mask.is_some() {
            DIGIT_W
        } else {
            COLON_W
        }
    }
}

/// The lit-segment mask for one char. Space lights nothing (ghosts still
/// show); an unmapped char renders exactly like space rather than panicking
/// or guessing.
fn cell(c: char) -> Cell {
    if c == ':' {
        return Cell { x: 0, mask: None };
    }
    let mask = match c {
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
    };
    Cell {
        x: 0,
        mask: Some(mask),
    }
}

/// Lay `text` out: one [`Cell`] per char, left to right, each carrying the
/// buffer x it starts at.
///
/// The one definition of this widget's horizontal layout — [`seven_seg`] walks
/// it for both passes, [`size`] measures it, and the shell's GL arm encodes it
/// into the strip its shader searches (#1154). Published so none of the three
/// has to re-derive `PAD + Σ(width + GAP)`.
#[must_use]
pub fn layout(text: &str) -> Vec<Cell> {
    let mut cells = Vec::new();
    let mut x = PAD;
    for c in text.chars() {
        let cell = Cell { x, ..cell(c) };
        cells.push(cell);
        x += cell.width() + GAP;
    }
    cells
}

/// The buffer `text` renders into, `(width, height)`.
///
/// Width grows with the cell count and the height is fixed — `"12:34"` is
/// 188×70. An empty readout is `2 * PAD` wide, never zero, which is what keeps
/// the host's `len == w * h * 4` invariant satisfiable.
#[must_use]
pub fn size(text: &str) -> (usize, usize) {
    extent(&layout(text))
}

/// [`size`] over an already-resolved layout, so [`seven_seg`] lays the cells
/// out once.
fn extent(cells: &[Cell]) -> (usize, usize) {
    let width = cells.last().map_or(2 * PAD, |c| c.x + c.width() + PAD);
    (width, 2 * PAD + DIGIT_H)
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
    let cells = layout(text);
    let (width, height) = extent(&cells);
    let mut frame = Frame::filled(width, height, palette.bg);

    // Ghost pass: every element of every cell, flat and dim. A digit cell
    // ghosts the full figure-8; a colon cell ghosts its two dots.
    if let Some(ghost) = palette.ghost {
        for &cell in &cells {
            let mut paint = |px: usize, py: usize| frame.set(px, py, ghost);
            stamp_cell(cell.x, PAD, cell.mask.map(|_| SEG_ALL), &mut paint);
        }
    }

    // Lit pass: the active segments, bloomed and composited toward the ink.
    let mut lit = Emission::new(width, height);
    for &cell in &cells {
        let mut stamp = |px: usize, py: usize| lit.add(px, py, 255);
        stamp_cell(cell.x, PAD, cell.mask, &mut stamp);
    }
    if let Some(bloom) = palette.bloom {
        lit.bloom(bloom);
    }
    lit.composite(&mut frame, palette.ink, palette.mask);
    frame
}

/// Emit the pixels of one cell's elements through `sink`, cell origin at
/// (`ox`, `oy`) — the geometry is shared verbatim by the ghost pass (paint)
/// and the lit pass (stamp).
///
/// `Some(mask)` is a digit cell lighting the [`BARS`] its bits name;
/// `None` is the colon, which lights both [`COLON_DOTS`] unconditionally.
///
/// The bars are stamped in table order rather than in the original
/// `A, G, D, F, B, E, C` one, and that is safe by construction: both sinks are
/// idempotent (`Frame::set` writes a colour, `Emission::add` saturates at
/// 255), so the stamp is a **set union** and its order cannot reach the
/// output. `the_rendered_bytes_are_pinned_by_digest` is what says so rather
/// than this comment.
fn stamp_cell(ox: usize, oy: usize, mask: Option<u8>, sink: &mut impl FnMut(usize, usize)) {
    let Some(mask) = mask else {
        for dot in COLON_DOTS {
            stamp_bar(ox, oy, dot, sink);
        }
        return;
    };
    for (bit, bar) in BARS.into_iter().enumerate() {
        if mask & (1 << bit) != 0 {
            stamp_bar(ox, oy, bar, sink);
        }
    }
}

/// One bar, stamped at a cell origin of (`ox`, `oy`).
///
/// Row/column `k` of [`THICK`] is inset from both ends by [`taper`] when the
/// bar is mitred and not at all when it is not, which is the hexagonal segment
/// shape and the colon's plain square respectively.
fn stamp_bar(ox: usize, oy: usize, bar: Bar, sink: &mut impl FnMut(usize, usize)) {
    let (x0, y0) = (ox + bar.x, oy + bar.y);
    for k in 0..THICK {
        let inset = if bar.tapered { taper(k) } else { 0 };
        for d in inset..bar.len.saturating_sub(inset) {
            if bar.vertical {
                sink(x0 + k, y0 + d);
            } else {
                sink(x0 + d, y0 + k);
            }
        }
    }
}

/// End-inset of a bar's row/column `k` (of [`THICK`]): 0 on the center
/// rows, growing toward the faces — the taper that mitres the segments.
///
/// `pub` since #1154 so the shell's GL arm can hold its **continuous** reading
/// of this staircase — `max(0, |k + 0.5 - THICK/2| - 0.5)`, the 45° chamfer
/// these integers sample — against the integers themselves.
#[must_use]
pub fn taper(k: usize) -> usize {
    (2 * k).abs_diff(THICK - 1).saturating_sub(1) / 2
}

#[cfg(test)]
mod tests {
    use super::super::{DisplayStyle, Ink, Pins, with_pins};
    use super::{BARS, COLON_DOTS, COLON_W, DIGIT_H, DIGIT_W, PAD, THICK, seven_seg, size, taper};

    /// Every readout the digest below sweeps: the empty buffer, the all-ghost
    /// blank, the colon-bearing clock face, every digit, the minus, the widest
    /// figure-8 pair, and an uncovered char.
    const DIGEST_READOUTS: [&str; 8] =
        ["", " ", "12:34", "88:88", "-", "9876543210", "x?", "07:16"];

    /// FNV-1a 64 over every byte `seven_seg` renders for
    /// [`DIGEST_READOUTS`] × [`DisplayStyle::ALL`], dimensions included.
    fn render_digest() -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |byte: u8| {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        };
        with_pins(
            Pins {
                ink: Ink::Base,
                field: None,
            },
            || {
                for style in DisplayStyle::ALL {
                    for text in DIGEST_READOUTS {
                        let frame = seven_seg(text, style);
                        for dim in [frame.width(), frame.height()] {
                            for byte in u32::try_from(dim).unwrap_or(u32::MAX).to_le_bytes() {
                                eat(byte);
                            }
                        }
                        for &byte in frame.data() {
                            eat(byte);
                        }
                    }
                }
            },
        );
        hash
    }

    /// **The bytes this widget renders, pinned by digest** (#1154).
    ///
    /// The seven segments' geometry moved out of [`stamp_cell`]'s inline
    /// arithmetic and into the published [`super::BARS`] table so the shell's
    /// GL arm could *read* it instead of transcribing it (the `Gauge::dial`
    /// precedent, #1148). That is a refactor of a render path, not a visibility
    /// change, and the only thing that can say it moved no byte is a function of
    /// every byte — taken on the tree **before** the refactor and asserted on
    /// the tree after.
    ///
    /// Pinned under [`Ink::Base`] so the process-wide accent (an atomic other
    /// tests move in parallel) cannot make it flaky: `Ink::Base` ignores the
    /// accent outright, so the palette is the same whatever the global holds.
    ///
    /// **Falsified** by moving any bar in [`super::BARS`], by changing
    /// [`taper`], by reordering nothing at all (the stamp is a set union, so
    /// order genuinely does not matter — which is itself worth knowing) or by
    /// touching any of the six metrics.
    #[test]
    fn the_rendered_bytes_are_pinned_by_digest() {
        assert_eq!(render_digest(), 0x54e6_1c87_61d5_b261);
    }

    /// **[`size`] is the buffer [`seven_seg`] actually renders** — the
    /// published measurement and the render walk one [`layout`], so a consumer
    /// that sizes a surface from `size` and a kit that rasterises into
    /// `seven_seg` cannot disagree (#1154).
    ///
    /// **Falsified** by giving `size` its own width formula and then moving
    /// [`GAP`] or [`PAD`].
    #[test]
    fn the_published_size_is_the_rendered_buffer() {
        for text in DIGEST_READOUTS {
            let frame = seven_seg(text, DisplayStyle::Lcd);
            assert_eq!(size(text), (frame.width(), frame.height()), "{text:?}");
        }
    }

    /// **Every published [`Bar`] lands inside its cell**, so no element of the
    /// last cell can reach past the buffer's right edge that [`size`] computes
    /// from the cell widths alone (#1154).
    ///
    /// The premise behind `Frame::set` never clipping here, and behind the
    /// shell's shader needing no bounds test of its own.
    #[test]
    fn every_bar_lies_inside_its_cell() {
        for (bars, cell_w) in [(&BARS[..], DIGIT_W), (&COLON_DOTS[..], COLON_W)] {
            for bar in bars {
                let (across, along) = if bar.vertical {
                    ((bar.x + THICK, cell_w), (bar.y + bar.len, DIGIT_H))
                } else {
                    ((bar.y + THICK, DIGIT_H), (bar.x + bar.len, cell_w))
                };
                assert!(across.0 <= across.1, "{bar:?} across {across:?}");
                assert!(along.0 <= along.1, "{bar:?} along {along:?}");
            }
        }
    }

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
