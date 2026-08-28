//! The **LED matrix panel**: a grid of discrete LEDs, each lit to its own
//! `0.0..=1.0` brightness — Annika's "Blinken Lichten" (issue #857).
//!
//! Where [`LedStrip`](super::LedStrip) models *one* level lighting a **prefix**
//! of a row (a VU meter: a bar), this models **N independent brightnesses on a
//! grid** (a panel: one cell per source). The shell's per-core CPU readout is
//! the motivating consumer — 64 cores as 64 lamps rather than 64 progress bars
//! — but nothing here knows about CPUs.
//!
//! Like every other kit surface it renders in a [`DisplayStyle`] skin, so the
//! ghost matrix, phosphor bloom and the CRT pass's comb/vignette all come along
//! for free. Unlike every other kit surface it also takes a
//! [`ColorMap`](super::ColorMap): the panel has many cells, so "what colour is
//! a cell?" becomes a real question, and the answer is a **separate axis** from
//! the skin — see the [`color_map`](super::color_map) module docs for why that
//! orthogonality is the design rather than an accident.
//!
//! # The grid is fixed (#839/#843)
//!
//! Cells sit on a whole-pixel lattice — [`CELL`]-px squares on a [`CELL`]+[`GAP`]
//! pitch inside a [`PAD`] bezel — and a cell's *brightness* changes without its
//! *position* ever moving. Nothing is resampled, so the panel never shimmers
//! between frames and never lands a lamp half on a pixel.
//!
//! # Sizing — the #313 lesson
//!
//! Sized via its **buffer dimensions**, like the rest of the kit: a CSS minimum
//! below the buffer size is a silent no-op. The panel is small — a 64-cell
//! near-square grid is 93×93 px — so a host that wants it bigger should upscale
//! (integer, nearest-neighbour) rather than stretch.

use super::color_map::ColorMap;
use super::frame::{Frame, Rgba};
use super::style::{DisplayStyle, Emission};

/// Edge length of one LED cell, in buffer pixels. Square, unlike the strip's
/// 8×16 bars: a panel cell is a *lamp*, not a segment of a bar.
pub const CELL: usize = 8;
/// Blank field gap between adjacent cells, on both axes.
pub const GAP: usize = 3;
/// Field padding around the whole grid, on every side.
pub const PAD: usize = 4;

/// What to do with the slots a ragged last row leaves over — Annika's `fill`
/// option (#857).
///
/// A grid holds `cols * rows` slots but is fed `n` levels, and the two are
/// rarely equal: 64 cores at `rows = 3` needs 22 columns, so 66 slots hold 64
/// lamps and the last row runs two short. This says what those two slots look
/// like.
///
/// The difference is *only* the ghost pass, so on a skin with no ghost
/// ([`Oled`](DisplayStyle::Oled), [`Crt`](DisplayStyle::Crt)) the two render
/// identically — an unlit lamp and no lamp look the same when unlit lamps are
/// invisible. That is not a bug in either variant; it is what "no ghosting"
/// means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fill {
    /// Pad the last row out with **unlit LEDs**: the spare slots are real
    /// hardware that simply is not lit, so they ghost through like every other
    /// unlit cell. The panel reads as a complete rectangular device. The
    /// default.
    #[default]
    Spare,
    /// Leave the spare slots **blank**: no lamp there at all, just field. The
    /// lamps stay left-aligned and the last row visibly stops short — the
    /// panel reads as exactly as many lamps as there are sources.
    Blank,
}

/// A grid of independently-lit LEDs. A value, so one `LedMatrix` renders many
/// frames — matching [`LedStrip`](super::LedStrip) / [`Marquee`](super::Marquee).
#[derive(Debug, Clone)]
pub struct LedMatrix {
    style: DisplayStyle,
    cols: usize,
    rows: usize,
    fill: Fill,
    color: ColorMap,
}

impl LedMatrix {
    /// A `cols`×`rows` panel in `style`, [`Fill::Spare`], with the skin's own
    /// single ink ([`ColorMap::Style`]). Both dimensions are clamped to at
    /// least 1, so there is no degenerate empty buffer.
    #[must_use]
    pub fn new(style: DisplayStyle, cols: usize, rows: usize) -> Self {
        Self {
            style,
            cols: cols.max(1),
            rows: rows.max(1),
            fill: Fill::Spare,
            color: ColorMap::Style,
        }
    }

    /// A panel shaped to hold `cells` lamps in as near a **square** as an
    /// integer grid allows — Annika's `rect` shape (#857).
    ///
    /// Takes the fewest columns whose square covers `cells`
    /// (`cols = ⌈√cells⌉`), then the fewest rows that fit
    /// (`rows = ⌈cells / cols⌉`). The result is never taller than it is wide,
    /// never more than one row off square, and never has a wholly empty
    /// trailing column — 1 → 1×1, 4 → 2×2, 8 → 3×3, 16 → 4×4, 64 → 8×8.
    #[must_use]
    pub fn rect(style: DisplayStyle, cells: usize) -> Self {
        let (cols, rows) = near_square(cells);
        Self::new(style, cols, rows)
    }

    /// Set the grid shape explicitly (each clamped to at least 1) — Annika's
    /// `rows = N` shape: `LedMatrix::rect(style, n).shape(n.div_ceil(3), 3)`
    /// for a three-row panel, or build it with [`new`](Self::new) directly.
    #[must_use]
    pub fn shape(mut self, cols: usize, rows: usize) -> Self {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        self
    }

    /// Set what a ragged last row's leftover slots look like.
    #[must_use]
    pub fn fill(mut self, fill: Fill) -> Self {
        self.fill = fill;
        self
    }

    /// Set the colour axis — the map from a cell's position and level to its
    /// ink. [`ColorMap::Style`] (the default) is the skin's single accent-tinted
    /// ink, i.e. exactly what every other kit widget does.
    #[must_use]
    pub fn color(mut self, color: ColorMap) -> Self {
        self.color = color;
        self
    }

    /// The panel's column count (post-clamp).
    #[must_use]
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// The panel's row count (post-clamp).
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The buffer width [`render`](Self::render) will produce, in pixels —
    /// `2*PAD + cols*CELL + (cols-1)*GAP`. Available without rendering so a
    /// host can size a surface up front.
    #[must_use]
    pub fn width(&self) -> usize {
        span(self.cols)
    }

    /// The buffer height [`render`](Self::render) will produce, in pixels —
    /// `2*PAD + rows*CELL + (rows-1)*GAP`.
    #[must_use]
    pub fn height(&self) -> usize {
        span(self.rows)
    }

    /// Render the panel: `levels[i]` (`0.0..=1.0`) is the brightness of the
    /// `i`-th lamp, laid out **row-major** from the top-left.
    ///
    /// A level outside `0.0..=1.0` is clamped; a `NaN` lights nothing, matching
    /// [`LedStrip`](super::LedStrip)'s documented behaviour. Levels past
    /// `cols * rows` are ignored (the grid is the shape you asked for, not a
    /// shape derived from the data), and slots past `levels.len()` are handled
    /// per [`Fill`]. The buffer is fully opaque and always satisfies the host's
    /// `len == w * h * 4` invariant, for any input including an empty slice.
    #[must_use]
    pub fn render(&self, levels: &[f32]) -> Frame {
        let palette = self.style.palette();
        let slots = self.cols * self.rows;
        let used = levels.len().min(slots);
        let width = self.width();
        let height = self.height();
        let mut frame = Frame::filled(width, height, palette.bg);

        // Ghost pass: the unlit LED grid showing through, exactly like the
        // hardware. `Fill::Spare` ghosts every slot (the ragged tail is unlit
        // hardware); `Fill::Blank` ghosts only the slots that hold a lamp.
        if let Some(ghost) = palette.ghost {
            let ghosted = match self.fill {
                Fill::Spare => slots,
                Fill::Blank => used,
            };
            for i in 0..ghosted {
                fill_cell(&mut frame, i % self.cols, i / self.cols, ghost);
            }
        }

        // Level pass: stamp every lamp at its own intensity, bloom, composite.
        let mut lit = Emission::new(width, height);
        for (i, &level) in levels.iter().take(used).enumerate() {
            stamp_cell(&mut lit, i % self.cols, i / self.cols, intensity(level));
        }
        if let Some(bloom) = palette.bloom {
            lit.bloom(bloom);
        }

        if self.color == ColorMap::Style || used == 0 {
            // The default map is the identity on the palette ink, so take the
            // shared single-ink path verbatim — the same call `led_strip`,
            // `dot_matrix` and the rest make. Nothing about this render is
            // reachable from the colour axis.
            lit.composite(&mut frame, palette.ink, palette.mask);
        } else {
            // Resolve each lamp's ink once (not once per pixel), then map each
            // buffer pixel to the lamp it belongs to through two small
            // geometry tables — the same "keep it O(pixels) multiplies"
            // reasoning `MaskCols` documents for the CRT pass.
            let inks: Vec<Rgba> = levels
                .iter()
                .take(used)
                .enumerate()
                .map(|(i, &level)| self.color.ink(sweep_pos(i, used), level, palette.ink))
                .collect();
            let col_of = index_table(width, self.cols);
            let row_of = index_table(height, self.rows);
            lit.composite_with(&mut frame, palette.mask, |x, y| {
                // Bloom spills a lamp's light into the gutter around it, and
                // the gutter belongs to the nearest lamp — so a halo takes the
                // colour of the lamp it came off. Spare slots have no ink of
                // their own; they clamp onto the last real lamp, which only
                // matters for a neighbour's halo bleeding into them.
                inks[(row_of[y] * self.cols + col_of[x]).min(used - 1)]
            });
        }

        frame
    }
}

/// The buffer extent covering `n` cells on one axis:
/// `2*PAD + n*CELL + (n-1)*GAP`.
fn span(n: usize) -> usize {
    let n = n.max(1);
    2 * PAD + n * CELL + (n - 1) * GAP
}

/// The near-square `(cols, rows)` grid holding `cells` lamps — see
/// [`LedMatrix::rect`]. `0` cells still yields a `1×1` grid, so no caller can
/// produce a zero-dimension buffer.
fn near_square(cells: usize) -> (usize, usize) {
    if cells <= 1 {
        return (1, 1);
    }
    // `⌈√cells⌉` without touching floats: `isqrt` truncates, so bump it unless
    // `cells` is a perfect square.
    let root = cells.isqrt();
    let cols = if root * root < cells { root + 1 } else { root };
    (cols, cells.div_ceil(cols))
}

/// A lamp's normalized position along the panel's colour sweep — its row-major
/// index over the lamp count, in `0.0..1.0`.
///
/// **Half-open, deliberately.** The obvious `index / (count - 1)` puts the last
/// lamp at exactly `1.0`, which is wrong for both kinds of positional map:
/// [`Rainbow`](ColorMap::Rainbow) is *cyclic*, so `1.0` wraps back onto `0.0`
/// and the first and last lamps come out the same colour; and
/// [`TransPride`](ColorMap::TransPride) bands `0.0..=1.0` into fifths, so an
/// inclusive endpoint makes the bands unequal (the last one one lamp short).
/// Dividing by `count` gives every lamp an equal slice of the sweep and never
/// lands two lamps on the same point of a cycle.
///
/// One rule for every shape (see the [`color_map`](super::color_map) docs): a
/// positional map's bands therefore wrap from one row into the next on a
/// multi-row panel and read as slanted, which is the price of not having a
/// shape-dependent branch here.
#[allow(clippy::cast_precision_loss)]
fn sweep_pos(index: usize, count: usize) -> f32 {
    if count == 0 {
        return 0.0;
    }
    // Both are small lamp counts; the ratio is exact enough that the sweep is
    // smooth, and precision loss at these magnitudes cannot move a band edge.
    index as f32 / count as f32
}

/// How brightly a `0.0..=1.0` level lights its lamp, as an [`Emission`]
/// intensity in `0..=255`.
///
/// `1.0` is full, `0.0` is dark, and the mapping is monotone in between — the
/// panel is analog per cell, unlike the strip's all-or-nothing segments. A
/// `NaN` level lights nothing, exactly as `led_strip::lit_count` documents for
/// the same case, and by the same mechanism: `f32::clamp` propagates the `NaN`
/// and the saturating `as` cast turns it into `0`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn intensity(level: f32) -> u16 {
    // The clamped product is `0.0..=255.0`, so the round-then-cast neither
    // loses precision nor wraps; `.min(255)` is belt-and-braces on the
    // endpoint, and the saturating cast maps NaN → 0.
    ((level.clamp(0.0, 1.0) * 255.0).round() as u16).min(255)
}

/// Per-pixel → cell-index lookup for one axis: which of `cells` cells each of
/// `extent` buffer pixels belongs to.
///
/// The bezel and the gutters have no cell of their own, so they are attributed
/// to the nearest one — the bezel to the first/last cell, a gutter to the cell
/// on its left/top. That is what makes a bloom halo take the colour of the
/// lamp it spilled off rather than of the field it landed on.
///
/// A table rather than a divide per lit pixel, for the reason `MaskCols`
/// spells out: an integer divide is tens of cycles and the divisor is
/// geometry-constant.
fn index_table(extent: usize, cells: usize) -> Vec<usize> {
    let last = cells.saturating_sub(1);
    (0..extent)
        .map(|p| p.saturating_sub(PAD) / (CELL + GAP))
        .map(|c| c.min(last))
        .collect()
}

/// The x of the cell at column `col`.
fn cell_x0(col: usize) -> usize {
    PAD + col * (CELL + GAP)
}

/// The y of the cell at row `row`.
fn cell_y0(row: usize) -> usize {
    PAD + row * (CELL + GAP)
}

/// Paint one cell flat into the frame (the ghost pass).
fn fill_cell(frame: &mut Frame, col: usize, row: usize, color: Rgba) {
    let (x0, y0) = (cell_x0(col), cell_y0(row));
    for y in y0..y0 + CELL {
        for x in x0..x0 + CELL {
            frame.set(x, y, color);
        }
    }
}

/// Stamp one cell into the emission grid at `amount` intensity (the lit pass).
/// A zero amount is skipped outright, so a dark lamp costs nothing and cannot
/// seed a halo.
fn stamp_cell(lit: &mut Emission, col: usize, row: usize, amount: u16) {
    if amount == 0 {
        return;
    }
    let (x0, y0) = (cell_x0(col), cell_y0(row));
    for y in y0..y0 + CELL {
        for x in x0..x0 + CELL {
            lit.add(x, y, amount);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ColorMap, DisplayStyle, Frame};
    use super::{
        CELL, Fill, GAP, LedMatrix, PAD, cell_x0, cell_y0, index_table, intensity, near_square,
        span, sweep_pos,
    };

    /// Coordinates of every pixel where two same-shaped frames differ.
    fn diff_pixels(a: &Frame, b: &Frame) -> Vec<(usize, usize)> {
        assert_eq!((a.width(), a.height()), (b.width(), b.height()));
        let mut out = Vec::new();
        for y in 0..a.height() {
            for x in 0..a.width() {
                if a.at(x, y) != b.at(x, y) {
                    out.push((x, y));
                }
            }
        }
        out
    }

    /// Every pixel of the cell at (`col`, `row`).
    fn cell_pixels(col: usize, row: usize) -> Vec<(usize, usize)> {
        let (x0, y0) = (cell_x0(col), cell_y0(row));
        (y0..y0 + CELL)
            .flat_map(|y| (x0..x0 + CELL).map(move |x| (x, y)))
            .collect()
    }

    /// The colour at the middle of the cell at (`col`, `row`).
    fn cell_center(f: &Frame, col: usize, row: usize) -> [u8; 4] {
        f.at(cell_x0(col) + CELL / 2, cell_y0(row) + CELL / 2)
    }

    // ── Level → lamp mapping ─────────────────────────────────────────────────

    /// `intensity` covers the full range at the endpoints, clamps out-of-range
    /// levels, and lights nothing on `NaN` — the strip's documented behaviour.
    ///
    /// Falsified by dropping the `.clamp(0.0, 1.0)` (an over-unit level then
    /// wraps or saturates elsewhere) or by replacing the saturating cast with
    /// a checked conversion that panics on `NaN`.
    #[test]
    fn intensity_maps_level_to_brightness() {
        assert_eq!(intensity(0.0), 0, "a rested lamp is dark");
        assert_eq!(intensity(1.0), 255, "a pinned lamp is full");
        assert_eq!(intensity(0.5), 128, "half level, half brightness");
        assert_eq!(intensity(2.0), 255, "an over-unit level clamps to full");
        assert_eq!(intensity(-1.0), 0, "a negative level lights nothing");
        assert_eq!(intensity(f32::NAN), 0, "NaN lights nothing (no panic)");
        assert_eq!(intensity(f32::INFINITY), 255);
        assert_eq!(intensity(f32::NEG_INFINITY), 0);
    }

    /// `intensity` is monotone non-decreasing across the whole range.
    #[test]
    fn intensity_is_monotone() {
        let mut prev = 0;
        for step in 0..=1000 {
            #[allow(clippy::cast_precision_loss)]
            let level = step as f32 / 1000.0;
            let v = intensity(level);
            assert!(v >= prev, "level {level} was dimmer than a quieter one");
            assert!(v <= 255);
            prev = v;
        }
        assert_eq!(prev, 255, "the sweep ends at full brightness");
    }

    /// Every lamp is independent: raising one level brightens exactly that
    /// lamp's cell and leaves every other lamp's cell alone.
    ///
    /// This is *the* difference from [`LedStrip`], whose one level lights a
    /// prefix — falsified by any render that derives cell `i`'s brightness
    /// from anything but `levels[i]`. Run on LCD (no bloom) so "leaves alone"
    /// means the pixels, not the pixels-modulo-halo.
    #[test]
    fn each_lamp_follows_its_own_level() {
        let m = LedMatrix::new(DisplayStyle::Lcd, 3, 2);
        let base = m.render(&[0.0; 6]);
        for i in 0..6usize {
            let mut levels = [0.0f32; 6];
            levels[i] = 1.0;
            let changed = diff_pixels(&base, &m.render(&levels));
            assert_eq!(
                changed,
                cell_pixels(i % 3, i / 3),
                "lamp {i} lit something other than its own cell"
            );
        }
    }

    // ── Shape ────────────────────────────────────────────────────────────────

    /// The `rect` shape covers every lamp, stays within one row of square,
    /// and is *minimal*: dropping a column would no longer fit.
    ///
    /// Falsified by the off-by-one that makes `cols = ⌊√n⌋` (16 → 4×4 survives,
    /// but 8 → 2×4 breaks the "within one row of square" bound) or by
    /// `rows = n / cols` truncating instead of `div_ceil` (the coverage bound
    /// breaks at 8).
    #[test]
    fn rect_picks_a_near_square_grid() {
        for cells in [1usize, 2, 3, 4, 5, 6, 7, 8, 12, 16, 20, 24, 32, 48, 64, 128] {
            let (cols, rows) = near_square(cells);
            assert!(cols * rows >= cells, "{cells}: {cols}×{rows} loses lamps");
            assert!(cols >= rows, "{cells}: {cols}×{rows} is taller than wide");
            assert!(
                cols - rows <= 1,
                "{cells}: {cols}×{rows} is not near-square"
            );
            assert!(
                (cols - 1) * rows < cells,
                "{cells}: {cols}×{rows} has a column to spare"
            );
        }
        // The shapes the issue names, pinned exactly.
        assert_eq!(near_square(1), (1, 1));
        assert_eq!(near_square(4), (2, 2));
        assert_eq!(near_square(8), (3, 3));
        assert_eq!(near_square(16), (4, 4));
        assert_eq!(near_square(64), (8, 8));
        // Degenerate input can't produce a zero-dimension buffer.
        assert_eq!(near_square(0), (1, 1));
    }

    /// The explicit `(cols, rows)` shape is what gets rendered, both dimensions
    /// clamp to at least one cell, and the buffer follows the cell metrics.
    #[test]
    fn shape_drives_the_buffer_dimensions() {
        let m = LedMatrix::new(DisplayStyle::Vfd, 22, 3);
        assert_eq!((m.cols(), m.rows()), (22, 3));
        assert_eq!(m.width(), 2 * PAD + 22 * CELL + 21 * GAP);
        assert_eq!(m.height(), 2 * PAD + 3 * CELL + 2 * GAP);
        let f = m.render(&[0.5; 64]);
        assert_eq!((f.width(), f.height()), (m.width(), m.height()));

        // A zero dimension clamps to one cell rather than a degenerate buffer.
        let z = LedMatrix::new(DisplayStyle::Vfd, 0, 0);
        assert_eq!((z.cols(), z.rows()), (1, 1));
        assert_eq!(z.width(), 2 * PAD + CELL);

        // `rect` at 64 lamps is the 8×8 panel, 93 px square.
        let r = LedMatrix::rect(DisplayStyle::Vfd, 64);
        assert_eq!((r.cols(), r.rows()), (8, 8));
        assert_eq!((r.width(), r.height()), (93, 93));
        assert_eq!(span(8), 93);
    }

    /// Levels past the grid's capacity are ignored rather than overflowing the
    /// buffer or panicking.
    #[test]
    fn extra_levels_are_ignored() {
        let m = LedMatrix::new(DisplayStyle::Vfd, 2, 2);
        let exact = m.render(&[1.0, 1.0, 1.0, 1.0]);
        let over = m.render(&[1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]);
        assert_eq!(exact, over, "a 4-slot grid draws 4 lamps, not 7");
    }

    // ── Fill ─────────────────────────────────────────────────────────────────

    /// **The ragged-tail case from the issue**: 64 lamps at `rows = 3` needs 22
    /// columns, so the last row runs two slots short. `Spare` ghosts those two
    /// as unlit hardware; `Blank` leaves them as bare field — and the two
    /// renders differ on *exactly* those two cells' pixels, nothing else.
    ///
    /// Driven on LCD because it ghosts and does **not** bloom: a glowing skin
    /// would let a neighbouring lamp's halo land on the spare cells and mix
    /// against the two different backgrounds, making the diff a superset of
    /// the cells and the "exactly" claim untestable.
    ///
    /// Falsified by ghosting `slots` in both arms, or `used` in both.
    #[test]
    fn spare_and_blank_differ_only_on_the_ragged_tail() {
        let levels = [0.4f32; 64];
        let base = LedMatrix::new(DisplayStyle::Lcd, 22, 3);
        let spare = base.clone().fill(Fill::Spare).render(&levels);
        let blank = base.fill(Fill::Blank).render(&levels);

        // 3 rows × 22 cols = 66 slots for 64 lamps: slots 64 and 65 are spare,
        // i.e. row 2, columns 20 and 21.
        let mut want = cell_pixels(20, 2);
        want.extend(cell_pixels(21, 2));
        want.sort_unstable();
        let mut got = diff_pixels(&spare, &blank);
        got.sort_unstable();
        assert_eq!(got, want, "the fill difference escaped the two spare cells");
    }

    /// The fill difference is *only* the ghost pass, so a skin with no ghost
    /// renders `Spare` and `Blank` identically.
    ///
    /// Falsified by making `Blank` also skip the *lit* pass for spare slots
    /// (there is nothing to skip — spare slots hold no level) or by giving
    /// either variant a geometry of its own.
    #[test]
    fn a_ghostless_skin_cannot_tell_spare_from_blank() {
        let levels = [0.4f32; 64];
        for style in [DisplayStyle::Oled, DisplayStyle::Crt] {
            let base = LedMatrix::new(style, 22, 3);
            assert_eq!(
                base.clone().fill(Fill::Spare).render(&levels),
                base.fill(Fill::Blank).render(&levels),
                "{style:?} has no ghost, so the two fills have nothing to differ on"
            );
        }
    }

    /// An exactly-full grid has no spare slots, so the two fills agree even on
    /// a ghosting skin — the difference is the *tail*, not the variant.
    #[test]
    fn a_full_grid_renders_the_same_under_both_fills() {
        let levels = [0.4f32; 64];
        let base = LedMatrix::new(DisplayStyle::Lcd, 8, 8);
        assert_eq!(
            base.clone().fill(Fill::Spare).render(&levels),
            base.fill(Fill::Blank).render(&levels)
        );
        assert_eq!(Fill::default(), Fill::Spare);
    }

    // ── Colour axis ──────────────────────────────────────────────────────────

    /// **The #857 regression guard, end to end.** The default [`ColorMap::Style`]
    /// render is byte-identical to a render that goes through the *generalised*
    /// per-pixel path with the palette ink at every cell — so the colour axis
    /// added a capability without moving a single byte of the old behaviour.
    ///
    /// The two sides take genuinely different code paths: `Style` short-circuits
    /// to `Emission::composite` (the call `led_strip` and friends make), while
    /// `Rgb(ink)` builds the ink table and runs `composite_with`. Falsified by
    /// any divergence between those two composites — a different rounding, a
    /// dropped mask stage, an off-by-one in the ink lookup tables.
    #[test]
    fn style_map_is_the_single_ink_path() {
        #[allow(clippy::cast_precision_loss)]
        let levels: Vec<f32> = (0..64).map(|i| i as f32 / 63.0).collect();
        for style in DisplayStyle::ALL {
            let [r, g, b, _] = style.palette().ink;
            let base = LedMatrix::rect(style, 64);
            assert_eq!(
                base.clone().color(ColorMap::Style).render(&levels),
                base.color(ColorMap::Rgb(r, g, b)).render(&levels),
                "{style:?}: the default map diverged from the single-ink path"
            );
        }
    }

    /// The colour axis and the skin **compose**: switching the map never
    /// switches the skin, and switching the skin never switches the map. Proved
    /// on the CRT, whose comb and vignette are the stage a "colours instead of
    /// skins" design would have had to give up.
    ///
    /// Falsified by routing a non-`Style` map around `composite_with`'s mask
    /// argument: the CRT's scanline/vignette variation down a uniformly-lit
    /// cell then disappears and the last assertion goes red.
    #[test]
    fn the_colour_axis_composes_with_the_skin() {
        let levels = [1.0f32; 16];
        let heat_crt = LedMatrix::rect(DisplayStyle::Crt, 16)
            .color(ColorMap::Heat)
            .render(&levels);
        let heat_oled = LedMatrix::rect(DisplayStyle::Oled, 16)
            .color(ColorMap::Heat)
            .render(&levels);
        let ink_crt = LedMatrix::rect(DisplayStyle::Crt, 16).render(&levels);
        // Same map, different skin: the panel is still a different device.
        assert_ne!(
            heat_crt, heat_oled,
            "the skin stopped mattering under a map"
        );
        // Same skin, different map: the device is still the same device.
        assert_ne!(heat_crt, ink_crt, "the map stopped mattering under a skin");

        // And the CRT pass is still *in* the heat render. Every pixel of an
        // interior cell is stamped at the same full intensity and the bloom
        // can only max-combine up to that, so without a screen-space mask the
        // whole cell composites to one flat colour — as it does on OLED. On
        // the tube, the comb dims one row in `Mask::CRT.pitch` and the
        // vignette falls off with position, so the same cell is not flat.
        let column = |f: &Frame| -> Vec<[u8; 4]> {
            let x = cell_x0(1) + CELL / 2;
            (cell_y0(1)..cell_y0(1) + CELL)
                .map(|y| f.at(x, y))
                .collect()
        };
        let oled_cell = column(&heat_oled);
        assert!(
            oled_cell.windows(2).all(|w| w[0] == w[1]),
            "a maskless skin should paint a fully-lit cell flat: {oled_cell:?}"
        );
        let crt_cell = column(&heat_crt);
        assert!(
            crt_cell.windows(2).any(|w| w[0] != w[1]),
            "the CRT comb/vignette vanished from the heat-mapped panel: {crt_cell:?}"
        );
    }

    /// A level-driven map colours cells by *how lit* they are: two lamps at
    /// different levels take different inks, and the hottest lamp is the
    /// reddest.
    ///
    /// Falsified by handing `ColorMap::ink` the sweep position in place of the
    /// level (the two halves then share a colour).
    #[test]
    fn heat_colours_the_panel_by_level() {
        // Two lamps, cold and hot, on a bloom-free ghost-free-enough skin.
        let m = LedMatrix::new(DisplayStyle::Lcd, 2, 1).color(ColorMap::Heat);
        let f = m.render(&[0.05, 1.0]);
        let (cold, hot) = (cell_center(&f, 0, 0), cell_center(&f, 1, 0));
        assert_ne!(cold, hot, "two levels took the same colour");
        assert!(
            hot[0] > cold[0],
            "the hot lamp is redder: {hot:?} vs {cold:?}"
        );
        assert!(
            cold[2] > hot[2],
            "the cold lamp is bluer: {cold:?} vs {hot:?}"
        );
    }

    /// A positional map colours cells by *where* they are: two lamps at the
    /// same level take different inks.
    ///
    /// Falsified by handing `ColorMap::ink` the level in place of the sweep
    /// position (both lamps then share a colour).
    #[test]
    fn rainbow_colours_the_panel_by_position() {
        let m = LedMatrix::new(DisplayStyle::Lcd, 4, 1).color(ColorMap::Rainbow);
        let f = m.render(&[0.8; 4]);
        let seen: Vec<_> = (0..4).map(|col| cell_center(&f, col, 0)).collect();
        for (i, a) in seen.iter().enumerate() {
            for b in &seen[i + 1..] {
                assert_ne!(a, b, "two equally-lit lamps share a colour: {seen:?}");
            }
        }
    }

    /// The sweep gives every lamp an equal, half-open slice of `0.0..1.0`, in
    /// row-major order, and never divides by zero.
    ///
    /// The half-open end is the point: with an inclusive `index / (count - 1)`
    /// the first and last lamp of a cyclic map land on the same colour — which
    /// is exactly what `rainbow_colours_the_panel_by_position` caught. Falsified
    /// by putting the `- 1` back.
    #[test]
    fn the_sweep_spans_the_lamps() {
        assert!(sweep_pos(0, 8) <= 0.0);
        assert!((sweep_pos(4, 8) - 0.5).abs() < 1e-6);
        assert!(
            sweep_pos(7, 8) < 1.0,
            "the last lamp stops short of the wrap point"
        );
        assert!(sweep_pos(0, 1) <= 0.0, "a lone lamp starts the sweep");
        assert!(
            sweep_pos(0, 0) <= 0.0,
            "an empty panel can't divide by zero"
        );
        // Equal slices: consecutive lamps are the same distance apart.
        let gap = sweep_pos(1, 64) - sweep_pos(0, 64);
        let mut prev = -1.0f32;
        for i in 0..64 {
            let p = sweep_pos(i, 64);
            assert!(p > prev, "the sweep went backwards at lamp {i}");
            assert!((0.0..1.0).contains(&p));
            if i > 0 {
                assert!((p - prev - gap).abs() < 1e-6, "uneven slice at lamp {i}");
            }
            prev = p;
        }
    }

    /// The per-pixel → cell table attributes bezel and gutter to the nearest
    /// cell and never runs off the end of the ink table.
    ///
    /// Falsified by dropping the `.min(last)` clamp: the trailing bezel then
    /// indexes one cell past the grid.
    #[test]
    fn the_index_table_clamps_bezel_and_gutter() {
        let cols = 3;
        let t = index_table(span(cols), cols);
        assert_eq!(t.len(), span(cols));
        assert_eq!(t[0], 0, "the leading bezel belongs to the first cell");
        assert_eq!(t[PAD], 0, "the first cell's first pixel");
        assert_eq!(t[cell_x0(1)], 1, "the second cell's first pixel");
        assert_eq!(
            t[cell_x0(1) - 1],
            0,
            "the gutter belongs to the cell left of it"
        );
        assert_eq!(
            *t.last().expect("non-empty"),
            cols - 1,
            "trailing bezel clamps"
        );
        assert!(t.iter().all(|&c| c < cols), "a pixel escaped the grid");
        // Monotone: the table never walks backwards across the buffer.
        assert!(t.windows(2).all(|w| w[1] >= w[0]));
    }

    // ── Host invariants ──────────────────────────────────────────────────────

    /// The host invariant across skins, maps, fills and hostile level slices.
    #[test]
    fn every_buffer_satisfies_the_host_invariant() {
        let cases: [&[f32]; 5] = [
            &[],
            &[0.0],
            &[1.0; 64],
            &[f32::NAN, f32::INFINITY, -2.0, 3.0],
            &[0.5; 7],
        ];
        for style in DisplayStyle::ALL {
            for map in ColorMap::ALL {
                for fill in [Fill::Spare, Fill::Blank] {
                    for levels in cases {
                        let m = LedMatrix::rect(style, 64).color(map).fill(fill);
                        let f = m.render(levels);
                        assert_eq!(
                            f.data().len(),
                            f.width() * f.height() * 4,
                            "{style:?}/{}/{fill:?}",
                            map.name()
                        );
                        assert!(f.width() > 0 && f.height() > 0);
                    }
                }
            }
        }
    }

    /// The panel is a screen: every pixel is opaque, wall to wall, under every
    /// colour map — including the ones that pick their own ink.
    #[test]
    fn every_pixel_is_opaque() {
        for style in DisplayStyle::ALL {
            for map in ColorMap::ALL {
                let f = LedMatrix::rect(style, 16).color(map).render(&[0.6; 16]);
                assert!(
                    f.data().chunks_exact(4).all(|px| px[3] == 0xff),
                    "{style:?}/{} panel is opaque",
                    map.name()
                );
            }
        }
    }

    /// Renders are deterministic, and a busier panel *glows brighter*.
    ///
    /// Total emitted light, not the count of non-background pixels: the bloom
    /// halo covers the same footprint at any level (it scales the intensity,
    /// not the radius), so a pixel-count metric is flat between a quarter-load
    /// and a pinned panel and would assert nothing. Falsified by dropping the
    /// level out of `intensity` and stamping every lamp at full.
    #[test]
    fn render_is_deterministic_and_load_shows() {
        let m = LedMatrix::rect(DisplayStyle::Oled, 16);
        assert_eq!(m.render(&[0.4; 16]), m.render(&[0.4; 16]));
        // OLED's field is true black and it has no ghost, so every non-zero
        // byte in the buffer is lamp light.
        let glow = |levels: &[f32]| -> u64 {
            m.render(levels)
                .data()
                .chunks_exact(4)
                .map(|px| u64::from(px[0]) + u64::from(px[1]) + u64::from(px[2]))
                .sum()
        };
        assert_eq!(glow(&[0.0; 16]), 0, "an idle box is dark on OLED");
        assert!(glow(&[0.3; 16]) > 0);
        assert!(
            glow(&[0.3; 16]) < glow(&[0.7; 16]),
            "a busier box glows more"
        );
        assert!(glow(&[0.7; 16]) < glow(&[1.0; 16]));
        // And a single busy lamp glows less than the whole panel busy.
        let mut one = [0.0f32; 16];
        one[5] = 1.0;
        assert!(glow(&one) < glow(&[1.0; 16]));
    }

    /// An empty level slice still renders a valid, fully-unlit panel of the
    /// requested shape rather than an empty or panicking buffer — the state a
    /// stats panel is in before its first sample arrives.
    #[test]
    fn an_unfed_panel_renders_dark() {
        for style in DisplayStyle::ALL {
            let m = LedMatrix::rect(style, 64).color(ColorMap::Heat);
            let f = m.render(&[]);
            assert_eq!((f.width(), f.height()), (93, 93));
            assert_eq!(
                f,
                LedMatrix::rect(style, 64).render(&[]),
                "{style:?}: an unfed panel is the same whatever the map"
            );
        }
    }
}
