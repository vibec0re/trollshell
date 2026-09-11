//! The **dot-matrix display**: one line of text as char cells of round
//! dots, in any [`DisplayStyle`] skin — the kit's ticker/readout widget.
//!
//! Each 5×7 font pixel renders as a `dot_px`×`dot_px` dot with a radial
//! falloff (bright core, dim rim), so the grid reads as discrete round dots
//! rather than square pixels. Ghosting styles (VFD faintly, LCD visibly)
//! paint **every** dot position of every cell first — the unlit matrix
//! showing through, exactly like the hardware — and glowing styles bleed a
//! halo off the lit dots. Single line by design: it's a ticker, not a
//! paragraph — wrapped text is [`TextBox`](super::TextBox)'s job.
//!
//! # The dot pitch is a knob (#1091)
//!
//! [`DotMatrix::dot_px`] sets the pitch, clamped to
//! [`MIN_DOT_PX`]`..=`[`MAX_DOT_PX`] and defaulting to [`DEFAULT_DOT_PX`] —
//! which is what the free-function [`dot_matrix`] renders at, so every caller
//! written before the knob existed keeps its exact bytes. The pitch is the
//! **only** thing that sets the display's height: the bezel is one dot cell on
//! every side, so a strip is `2*dot_px + GLYPH_H*dot_px` = `9*dot_px` buffer
//! pixels tall — 36 px at the default, 27 at 3, and **18 at 2**, which is what
//! finally fits a dot-matrix readout or a [`Marquee`](super::Marquee) inside a
//! 32 px bar without the bar growing around it.
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
//! reuses this module's dot *hardware* — the [`Dots`] pitch, bezel and
//! falloff painters — so a marquee dot is pixel-for-pixel the dot you see
//! here, at whatever pitch the two were built with.

use super::font;
use super::frame::{Frame, Rgba};
use super::style::{DisplayStyle, Emission, mix};

/// The default edge length of one dot cell in buffer pixels: every font pixel
/// becomes a `DOT`×`DOT` round dot. At 4 px a char cell advances 24 px, so ~11
/// chars fill a ~296 px sidebar card (see the `preem` docs on sizing).
///
/// This is the kit's **virtual pixel**: the physical dot pitch of every
/// dot-matrix surface. [`Marquee`](super::Marquee) scrolls in whole units of
/// it, never in buffer pixels (#839). Since #1091 it is only the *default* —
/// [`DotMatrix::dot_px`] and [`Marquee::dot_px`](super::Marquee::dot_px) move
/// it — but the free-function [`dot_matrix`] still renders at exactly this
/// pitch, so nothing that predates the knob changed a byte.
pub const DEFAULT_DOT_PX: usize = 4;

/// The smallest dot pitch the kit will render. Below 2 px a "dot" is a single
/// pixel with no room for a falloff at all, so the widget stops being a dot
/// matrix and becomes a bitmap.
pub const MIN_DOT_PX: usize = 2;

/// The largest dot pitch the kit will render — 8 px is already a chunkier dot
/// than any skin reads well at, and the wire's own strip bound
/// (`MAX_STRIP_DIM`) is derived against it.
pub const MAX_DOT_PX: usize = 8;

/// Intensity of a fully-lit dot pixel (0..=255).
const CORE: u16 = 255;

/// The dot **hardware** at one pitch: the pitch itself, the bezel it implies,
/// and the radial falloff table for a cell of that size.
///
/// Shared verbatim with [`Marquee`](super::Marquee) — which is the point. A
/// scrolled dot has to be pixel-for-pixel a static one, and the way to
/// guarantee that is for both surfaces to paint through the same value rather
/// than through two consts that happen to agree.
///
/// `pad` is not a field: the bezel **is** one dot cell on every side, so it is
/// [`Dots::pad`], derived from the pitch. Before #1091 it was a separate
/// `PAD = DOT` const, which is exactly the shape that lets a pitch change leave
/// the bezel behind.
///
/// The falloff is a fixed [`MAX_DOT_PX`]² array rather than a `Vec` so the
/// whole thing stays `Copy` and allocation-free; only the top-left
/// `dot`×`dot` corner is ever read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Dots {
    /// Edge length of one dot cell in buffer pixels, in
    /// [`MIN_DOT_PX`]`..=`[`MAX_DOT_PX`].
    dot: usize,
    /// Intensity per pixel of the `dot`×`dot` cell (0..=255), row-major.
    falloff: [[u16; MAX_DOT_PX]; MAX_DOT_PX],
}

impl Default for Dots {
    fn default() -> Self {
        Self::new(DEFAULT_DOT_PX)
    }
}

impl Dots {
    /// The dot hardware at `dot_px`, clamped to
    /// [`MIN_DOT_PX`]`..=`[`MAX_DOT_PX`].
    pub(super) fn new(dot_px: usize) -> Self {
        let dot = dot_px.clamp(MIN_DOT_PX, MAX_DOT_PX);
        let mut falloff = [[0; MAX_DOT_PX]; MAX_DOT_PX];
        let denom = dot * dot;
        for (j, row) in falloff.iter_mut().enumerate().take(dot) {
            for (i, cell) in row.iter_mut().enumerate().take(dot) {
                *cell = intensity(radius_sq(i, dot), radius_sq(j, dot), denom);
            }
        }
        Self { dot, falloff }
    }

    /// The dot pitch in buffer pixels.
    pub(super) const fn dot(self) -> usize {
        self.dot
    }

    /// The field padding around the dot grid — one dot cell on every side, so
    /// this is the pitch by definition.
    pub(super) const fn pad(self) -> usize {
        self.dot
    }

    /// The buffer height of any surface built on this hardware:
    /// `2*pad + GLYPH_H*dot`, i.e. `9*dot`.
    pub(super) const fn height(self) -> usize {
        2 * self.pad() + font::GLYPH_H * self.dot
    }

    /// The per-character horizontal advance: `(GLYPH_W + SPACING) * dot`.
    pub(super) const fn advance(self) -> usize {
        (font::GLYPH_W + font::SPACING) * self.dot
    }

    /// Paint one unlit ghost dot flat into the frame: the falloff shape, mixed
    /// from the field toward the ghost color. `x`/`y` are the top-left buffer
    /// pixel of the `dot`×`dot` cell. Shared with
    /// [`Marquee`](super::Marquee) so both surfaces show the *same* unlit
    /// matrix.
    pub(super) fn ghost_dot(self, frame: &mut Frame, x: usize, y: usize, ghost: Rgba) {
        for (j, row) in self.falloff.iter().enumerate().take(self.dot) {
            for (i, &t) in row.iter().enumerate().take(self.dot) {
                let under = frame.at(x + i, y + j);
                frame.set(x + i, y + j, mix(under, ghost, t));
            }
        }
    }

    /// Stamp one lit dot's falloff into the emission grid. `x`/`y` are the
    /// top-left buffer pixel of the `dot`×`dot` cell. Shared with
    /// [`Marquee`](super::Marquee) so a scrolled dot lights exactly like a
    /// static one.
    pub(super) fn lit_dot(self, lit: &mut Emission, x: usize, y: usize) {
        for (j, row) in self.falloff.iter().enumerate().take(self.dot) {
            for (i, &t) in row.iter().enumerate().take(self.dot) {
                lit.add(x + i, y + j, t);
            }
        }
    }
}

/// `(2*k + 1 - dot)²` — four times the squared distance from the cell's centre
/// to pixel `k`'s centre along one axis, kept integral by doubling.
///
/// Doubling is what keeps the whole falloff in exact integer arithmetic: the
/// cell centre sits on a half-pixel for an even pitch, so the undoubled
/// distance is not an integer and a float would put the pitch-4 reproduction of
/// the shipped table at the mercy of rounding.
const fn radius_sq(k: usize, dot: usize) -> usize {
    let delta = (2 * k + 1).abs_diff(dot);
    delta * delta
}

/// The radial falloff law, evaluated in exact integers.
///
/// `dx2 + dy2` over `denom` is `s`, the squared distance from the dot's centre
/// **normalised by the dot's half-pitch** — so `s == 1` is one half-pitch out
/// and the cell's corners sit at `s == 2 - 4/dot + 2/dot²` (1.125 at pitch 4,
/// 0.5 at pitch 2). The profile is therefore the same shape at every pitch,
/// sampled on a finer or coarser grid. It is a piecewise-linear ramp through
/// three knots:
///
/// | `s` | intensity | where it comes from |
/// |---|---|---|
/// | `≤ 1/2` | `255` | the plateau. It reaches exactly this far because at [`MIN_DOT_PX`] all four pixels sit at `s == 1/2`, and a 2×2 dot has no room for a falloff — so the smallest pitch renders solid rather than washed out |
/// | `5/8` | `120` | the shipped 4×4 table's rim (`(±0.5, ±1.5)` from centre) |
/// | `9/8` | `25` | the shipped table's near-dark corners (`(±1.5, ±1.5)`) |
///
/// Past the last knot the final segment simply continues, reaching `0` at
/// `s = 1.25…` — which is why a wide pitch's corners go properly dark instead
/// of holding a dim square.
///
/// **The pitch-4 reproduction is by construction, not by fit.** At `dot == 4`
/// the three distinct cell radii are `s = 1/8`, `5/8` and `9/8`: the first is
/// inside the plateau and the other two *are* knots, so the law returns the
/// shipped constants exactly. `dot_matrix.rs`'s `the_computed_falloff_is_the_shipped_table`
/// pins that against the literal table, which is kept as the oracle.
fn intensity(dx2: usize, dy2: usize, denom: usize) -> u16 {
    let num = dx2 + dy2;
    // s ≤ 1/2 — the plateau.
    if 2 * num <= denom {
        return CORE;
    }
    // s ≤ 5/8 — the 255 → 120 segment, `795 - 1080*s`.
    if 8 * num <= 5 * denom {
        let scaled = 795 * denom - 1080 * num;
        return round_div(scaled, denom);
    }
    // Everything beyond: the 120 → 25 segment, `238.75 - 190*s`, continued
    // past its knot until it hits the floor.
    let quarters = 955 * denom;
    let taken = 760 * num;
    if quarters <= taken {
        return 0;
    }
    round_div(quarters - taken, 4 * denom)
}

/// `round(num / denom)`, half away from zero, for non-negative integers.
fn round_div(num: usize, denom: usize) -> u16 {
    let rounded = (2 * num + denom) / (2 * denom);
    u16::try_from(rounded).unwrap_or(CORE)
}

/// A dot-matrix display: one line of text on a grid of round dots.
///
/// The pitch is the only knob — the skin comes from the [`DisplayStyle`] and
/// everything else is the font's metrics — so this is a two-field builder
/// rather than a config struct. [`dot_matrix`] is the one-call form at the
/// default pitch, mirroring [`led_strip`](super::led_strip) /
/// [`seven_seg`](super::seven_seg).
///
/// ```
/// use hytte_preem::{DisplayStyle, DotMatrix};
///
/// // A readout that fits a 32 px bar: 9 * 2 = 18 px tall.
/// let bar = DotMatrix::new(DisplayStyle::Vfd).dot_px(2).render("12:34");
/// assert_eq!(bar.height(), 18);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DotMatrix {
    style: DisplayStyle,
    dots: Dots,
}

impl DotMatrix {
    /// A dot-matrix display in `style`, at the [`DEFAULT_DOT_PX`] pitch.
    #[must_use]
    pub fn new(style: DisplayStyle) -> Self {
        Self {
            style,
            dots: Dots::default(),
        }
    }

    /// Set the dot pitch in buffer pixels, clamped to
    /// [`MIN_DOT_PX`]`..=`[`MAX_DOT_PX`]. A consuming builder, like the rest of
    /// the kit.
    ///
    /// This is the display's whole size knob: the height is `9*px` and each
    /// character advances `6*px`.
    #[must_use]
    pub fn dot_px(mut self, px: usize) -> Self {
        self.dots = Dots::new(px);
        self
    }

    /// The buffer height this display renders at, whatever the text:
    /// `2*dot_px + GLYPH_H*dot_px` = `9*dot_px`.
    #[must_use]
    pub const fn height(&self) -> usize {
        self.dots.height()
    }

    /// Render one line of `text`.
    ///
    /// The buffer is fully opaque and always satisfies the host's
    /// `len == w * h * 4` invariant, for any input including the empty string.
    /// An uncovered char renders as the hollow [`font::NOTDEF`] box. Width
    /// grows linearly with the char count — `2*dot_px + n*6*dot_px - dot_px` —
    /// so keep tickers to ~11 chars for a sidebar card at the default pitch.
    #[must_use]
    pub fn render(&self, text: &str) -> Frame {
        let palette = self.style.palette();
        let dots = self.dots;
        let (dot, pad) = (dots.dot(), dots.pad());
        let n = text.chars().count();
        let advance = dots.advance();
        let width = if n == 0 {
            2 * pad
        } else {
            2 * pad + n * advance - font::SPACING * dot
        };
        let height = dots.height();
        let mut frame = Frame::filled(width, height, palette.bg);

        // Ghost pass: the unlit matrix shows through on ghosting styles — every
        // dot position of every char cell, lit or not (spacing columns carry no
        // dots on the hardware either).
        if let Some(ghost) = palette.ghost {
            for cell in 0..n {
                let ox = pad + cell * advance;
                for row in 0..font::GLYPH_H {
                    for col in 0..font::GLYPH_W {
                        dots.ghost_dot(&mut frame, ox + col * dot, pad + row * dot, ghost);
                    }
                }
            }
        }

        // Lit pass: stamp each set font pixel as a falloff dot, bloom if the
        // style glows, then composite toward the ink.
        let mut lit = Emission::new(width, height);
        for (cell, ch) in text.chars().enumerate() {
            let rows = font::glyph(ch).unwrap_or(&font::NOTDEF);
            let ox = pad + cell * advance;
            for (ry, &bits) in rows.iter().enumerate() {
                for cx in 0..font::GLYPH_W {
                    if (bits >> (font::GLYPH_W - 1 - cx)) & 1 == 1 {
                        dots.lit_dot(&mut lit, ox + cx * dot, pad + ry * dot);
                    }
                }
            }
        }
        if let Some(bloom) = palette.bloom {
            lit.bloom(bloom);
        }
        lit.composite(&mut frame, palette.ink, palette.mask);
        frame
    }
}

/// Render one line of `text` as a dot-matrix display in `style`, at the
/// [`DEFAULT_DOT_PX`] pitch.
///
/// The convenience free-function form of [`DotMatrix`], mirroring
/// [`led_strip`](super::led_strip) / [`seven_seg`](super::seven_seg). Reach for
/// the builder when you need a pitch other than the default.
#[must_use]
pub fn dot_matrix(text: &str, style: DisplayStyle) -> Frame {
    DotMatrix::new(style).render(text)
}

#[cfg(test)]
mod tests {
    use super::super::DisplayStyle;
    use super::{DEFAULT_DOT_PX, DotMatrix, Dots, MAX_DOT_PX, MIN_DOT_PX, dot_matrix};

    /// The dot pitch the whole kit shipped with before #1091 made it a knob.
    const DOT: usize = DEFAULT_DOT_PX;
    /// The bezel that went with it.
    const PAD: usize = DEFAULT_DOT_PX;

    /// The **oracle**: the hand-tuned 4×4 radial falloff `dot_matrix.rs`
    /// carried as a literal const from #356 until #1091 generalised it to any
    /// pitch. Kept verbatim, and deliberately *not* the thing production reads
    /// — a computed law that has to reproduce a literal is a real check; a
    /// literal that reproduces itself is not.
    const FALLOFF_AT_4: [[u16; 4]; 4] = [
        [25, 120, 120, 25],
        [120, 255, 255, 120],
        [120, 255, 255, 120],
        [25, 120, 120, 25],
    ];

    /// FNV-1a 64 over a frame's bytes — a compact stand-in for pasting four
    /// kilobytes of RGBA into the source. Hand-rolled because the kit has (and
    /// wants) no hashing dependency.
    fn digest(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    /// #1091's load-bearing test: the **computed** falloff at the default pitch
    /// is the shipped table, cell for cell.
    ///
    /// **Falsified** by nudging any knot in `intensity` — moving the plateau off
    /// `s ≤ 1/2`, or either of the two shipped knots by one unit — which is the
    /// whole reason the law is expressed in exact integers rather than floats.
    #[test]
    fn the_computed_falloff_is_the_shipped_table() {
        let dots = Dots::new(DEFAULT_DOT_PX);
        for (j, row) in FALLOFF_AT_4.iter().enumerate() {
            for (i, &want) in row.iter().enumerate() {
                assert_eq!(dots.falloff[j][i], want, "falloff cell ({i},{j})");
            }
        }
    }

    /// The bytes on glass at the default pitch are the bytes `origin/main`
    /// rendered before the pitch became a parameter — recorded from that tree,
    /// not regenerated from this one.
    ///
    /// **Falsified** by any change to the falloff law at pitch 4, to the bezel,
    /// or to the advance. This is what makes "nothing that does not set
    /// `dot_px` changed a pixel" a checked claim rather than a promise.
    #[test]
    fn the_default_pitch_renders_the_pre_1091_bytes() {
        // (style, text, width, digest) recorded on origin/main @ bdb3a06.
        let cases: [(DisplayStyle, &str, usize, u64); 24] = [
            (DisplayStyle::Vfd, "", 8, 0x4586_a973_ad46_af65),
            (DisplayStyle::Vfd, " ", 28, 0xc1c5_8411_527f_f425),
            (DisplayStyle::Vfd, "PREEM", 124, 0x8241_b872_24c7_5189),
            (DisplayStyle::Vfd, "88:88", 124, 0xcaef_9bd7_997d_5b25),
            (DisplayStyle::Vfd, "åäö 💕", 124, 0x7473_0cf4_0fff_0fe9),
            (
                DisplayStyle::Vfd,
                "0123456789~/",
                292,
                0x914f_633e_fec0_afdd,
            ),
            (DisplayStyle::Lcd, "", 8, 0xac22_ef75_3302_65e5),
            (DisplayStyle::Lcd, " ", 28, 0xa24c_7b3c_189b_30bd),
            (DisplayStyle::Lcd, "PREEM", 124, 0x6873_a9e3_6e89_15c5),
            (DisplayStyle::Lcd, "88:88", 124, 0x6631_e93c_35b2_fc3d),
            (DisplayStyle::Lcd, "åäö 💕", 124, 0x351a_e3d6_d0eb_4195),
            (
                DisplayStyle::Lcd,
                "0123456789~/",
                292,
                0xe9d5_d549_d396_92ad,
            ),
            (DisplayStyle::Oled, "", 8, 0xfbf2_17cf_4070_f025),
            (DisplayStyle::Oled, " ", 28, 0x9668_ffbf_cdd5_b0a5),
            (DisplayStyle::Oled, "PREEM", 124, 0x49e2_7059_5ce5_f975),
            (DisplayStyle::Oled, "88:88", 124, 0xc7b2_b497_a032_f6f5),
            (DisplayStyle::Oled, "åäö 💕", 124, 0x7b15_b48a_8807_5865),
            (
                DisplayStyle::Oled,
                "0123456789~/",
                292,
                0x4c57_6dc0_a3f5_14fd,
            ),
            (DisplayStyle::Crt, "", 8, 0x6408_f107_ecf4_3ca5),
            (DisplayStyle::Crt, " ", 28, 0xc452_bcb6_60d2_1965),
            (DisplayStyle::Crt, "PREEM", 124, 0x73a6_646d_5a4c_d717),
            (DisplayStyle::Crt, "88:88", 124, 0x3950_58fb_a645_3488),
            (DisplayStyle::Crt, "åäö 💕", 124, 0x0bda_72c5_de73_9f61),
            (
                DisplayStyle::Crt,
                "0123456789~/",
                292,
                0xfa36_5f24_26b0_11fa,
            ),
        ];
        for (style, text, width, want) in cases {
            let frame = dot_matrix(text, style);
            assert_eq!(frame.width(), width, "{style:?} {text:?} width");
            assert_eq!(frame.height(), 36, "{style:?} {text:?} height");
            assert_eq!(
                digest(frame.data()),
                want,
                "{style:?} {text:?} drifted from the pre-#1091 bytes"
            );
        }
        // …and the builder at the default pitch is the free function.
        assert_eq!(
            DotMatrix::new(DisplayStyle::Vfd).render("PREEM"),
            dot_matrix("PREEM", DisplayStyle::Vfd)
        );
    }

    /// The heights the issue asked for: `2*dot_px + 7*dot_px`.
    ///
    /// **Falsified** by leaving the bezel at a fixed 4 while the pitch moves —
    /// the exact regression #1091 exists to prevent, and the reason `pad` is
    /// derived from the pitch rather than living beside it as a second const.
    #[test]
    fn the_height_is_nine_dots() {
        for (px, height) in [(2, 18), (3, 27), (4, 36), (5, 45), (8, 72)] {
            let display = DotMatrix::new(DisplayStyle::Vfd).dot_px(px);
            assert_eq!(display.height(), height, "dot_px {px}");
            for text in ["", "8", "12:34"] {
                assert_eq!(
                    display.render(text).height(),
                    height,
                    "dot_px {px} text {text:?}"
                );
            }
        }
        // The headline number: a marquee-sized readout inside a 32 px bar.
        assert_eq!(
            DotMatrix::new(DisplayStyle::Vfd)
                .dot_px(2)
                .render("12:34")
                .height(),
            18
        );
    }

    /// Width follows the pitch too: `2*dot_px + n*6*dot_px - dot_px`.
    #[test]
    fn the_width_follows_the_pitch() {
        for px in MIN_DOT_PX..=MAX_DOT_PX {
            let display = DotMatrix::new(DisplayStyle::Lcd).dot_px(px);
            assert_eq!(display.render("").width(), 2 * px, "empty at {px}");
            assert_eq!(
                display.render("abc").width(),
                2 * px + 3 * 6 * px - px,
                "three chars at {px}"
            );
        }
    }

    /// The pitch clamps rather than rejecting: `1` becomes [`MIN_DOT_PX`] and
    /// `9` (or `usize::MAX`) becomes [`MAX_DOT_PX`], because every kit entry
    /// point is total.
    #[test]
    fn the_pitch_clamps_into_range() {
        let at = |px| DotMatrix::new(DisplayStyle::Vfd).dot_px(px);
        assert_eq!(at(0).render("8"), at(MIN_DOT_PX).render("8"));
        assert_eq!(at(1).render("8"), at(MIN_DOT_PX).render("8"));
        assert_eq!(at(9).render("8"), at(MAX_DOT_PX).render("8"));
        assert_eq!(at(usize::MAX).render("8"), at(MAX_DOT_PX).render("8"));
        assert_eq!(at(1).height(), 9 * MIN_DOT_PX);
        assert_eq!(at(9).height(), 9 * MAX_DOT_PX);
    }

    /// Every pitch still paints a *dot*, not a square: the cell's centre is at
    /// full intensity and its corner is strictly dimmer. (At [`MIN_DOT_PX`] the
    /// 2×2 cell is all corner and all centre at once, so it is solid by
    /// design — the one pitch exempt from the strict inequality.)
    #[test]
    fn every_pitch_is_round() {
        for px in MIN_DOT_PX..=MAX_DOT_PX {
            let dots = Dots::new(px);
            let mid = px / 2;
            assert_eq!(dots.falloff[mid][mid], 255, "centre at {px}");
            if px > MIN_DOT_PX {
                assert!(
                    dots.falloff[0][0] < dots.falloff[mid][mid],
                    "corner at {px} is {} ",
                    dots.falloff[0][0]
                );
            }
            // Nothing outside the cell is ever written.
            for j in 0..MAX_DOT_PX {
                for i in 0..MAX_DOT_PX {
                    if i >= px || j >= px {
                        assert_eq!(dots.falloff[j][i], 0, "outside ({i},{j}) at {px}");
                    }
                }
            }
        }
        // A 2 px dot is solid: four pixels, no room for a rim.
        assert_eq!(Dots::new(MIN_DOT_PX).falloff[0][0], 255);
    }

    /// The host invariant across styles, inputs and pitches, empty string
    /// included.
    #[test]
    fn every_buffer_satisfies_the_host_invariant() {
        for style in DisplayStyle::ALL {
            for px in MIN_DOT_PX..=MAX_DOT_PX {
                for text in ["", " ", "PREEM", "åäö 💕", "0123456789~/"] {
                    let f = DotMatrix::new(style).dot_px(px).render(text);
                    assert_eq!(
                        f.data().len(),
                        f.width() * f.height() * 4,
                        "{style:?} {text:?} @ {px}"
                    );
                    assert!(f.width() > 0 && f.height() > 0);
                }
            }
        }
    }

    /// Display widgets promise fully opaque frames — they are screens.
    #[test]
    fn every_pixel_is_opaque() {
        for style in DisplayStyle::ALL {
            for px in MIN_DOT_PX..=MAX_DOT_PX {
                let f = DotMatrix::new(style).dot_px(px).render("HELLO");
                assert!(
                    f.data().chunks_exact(4).all(|pixel| pixel[3] == 0xff),
                    "{style:?} @ {px} frame is opaque wall to wall"
                );
            }
        }
    }

    #[test]
    fn render_is_deterministic() {
        assert_eq!(
            dot_matrix("TICK", DisplayStyle::Vfd),
            dot_matrix("TICK", DisplayStyle::Vfd)
        );
        let small = DotMatrix::new(DisplayStyle::Vfd).dot_px(2);
        assert_eq!(small.render("TICK"), small.render("TICK"));
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
    /// but stays true black on OLED — Annika's no-ghosting OLED rule. Holds at
    /// every pitch: the ghost pass reads the same falloff the lit pass does.
    #[test]
    fn lcd_ghosts_and_oled_does_not() {
        for px in MIN_DOT_PX..=MAX_DOT_PX {
            let lcd = DotMatrix::new(DisplayStyle::Lcd).dot_px(px).render(" ");
            let lcd_bg = DisplayStyle::Lcd.palette().bg;
            assert!(
                lcd.data().chunks_exact(4).any(|p| p != lcd_bg),
                "LCD paints ghost cells behind unlit dots @ {px}"
            );
            let oled = DotMatrix::new(DisplayStyle::Oled).dot_px(px).render(" ");
            assert!(
                oled.data().chunks_exact(4).all(|p| p == [0, 0, 0, 0xff]),
                "an unlit OLED emits nothing at all @ {px}"
            );
        }
    }

    /// Different text, different pixels (the widget actually renders text) —
    /// including at the bar-sized pitch, where the dots are smallest.
    #[test]
    fn text_changes_the_render() {
        assert_ne!(
            dot_matrix("AB", DisplayStyle::Vfd),
            dot_matrix("BA", DisplayStyle::Vfd)
        );
        let small = DotMatrix::new(DisplayStyle::Vfd).dot_px(2);
        assert_ne!(small.render("AB"), small.render("BA"));
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
