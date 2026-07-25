//! The **oscilloscope**: a glow-trace waveform over a graticule, with real
//! phosphor persistence — the kit's live-signal display (#556, a #397 skin).
//!
//! Unlike every other kit widget the scope is **stateful across frames**: it
//! owns a persistence buffer (a per-pixel intensity grid) that carries the beam
//! trail from one frame to the next. Each [`advance`](Scope::advance):
//!
//! 1. **decays** every stored intensity exponentially — a tuned
//!    [`persistence`](Scope::persistence) constant (`≈0.72` of the intensity
//!    retained per tick by default) multiplies the whole grid, so the beam
//!    *fades*; it is never redrawn from black — then
//! 2. **stamps** the fresh trace at full intensity: the input `&[f32]` is
//!    linearly interpolated across the columns into a connected polyline (so a
//!    handful of sparse samples render as a continuous waveform, never a bar
//!    chart), each beam pixel drawn with a bright core and a soft vertical glow
//!    falloff (the "2-3 brightness levels").
//!
//! [`render`](Scope::render) then composes a frame: the **graticule** — a faint
//! grid plus a brighter center cross, redrawn flat every frame so it never
//! decays or flickers — sits *under* the phosphor trace, which is bloomed and
//! composited toward the skin's accent-tinted ink (#376) exactly like the rest
//! of the kit, never a parallel color path. The buffer then upscales by the
//! [`scale`](Scope::scale) hint (#358) — the `.caw-lcd`/`.pet-lcd` sizing
//! lesson: size in buffer pixels, never shell CSS.
//!
//! # Input
//!
//! Any normalized signal in **`-1.0..=1.0`** — audio bins, a CPU-load history,
//! a net-rate series; it is **not** audio-specific. `0.0` is the center axis,
//! `+1.0` the top, `-1.0` the bottom. Out-of-range and non-finite samples are
//! clamped defensively (`NaN`/`±inf` read as `0.0`), so no input can push the
//! beam out of the buffer or panic. Empty/absent input flatlines on the axis
//! while the old trail keeps decaying honestly (a silent sink still ghosts).
//!
//! # State lives in the plugin (the kit owns no clock)
//!
//! Like [`PeakHold`](super::PeakHold), the scope is a small stateful value the
//! plugin holds in its model and drives at its own cadence: [`advance`] once
//! per animation tick with the latest samples, [`render`] into the view (or the
//! [`tick`](Scope::tick) convenience for both at once). The kit owns no clock
//! (see the `preem` module docs on timing). Because the beam trail is
//! cross-frame state, the skin is taken at *render* time rather than
//! construction — that lets the plugin (or a live host re-tint) re-skin a
//! running trace without dropping its phosphor.
//!
//! ```
//! use hytte_plugin::preem::{DisplayStyle, Scope};
//!
//! let mut scope = Scope::new();
//! scope.advance(&[0.0, 0.5, 1.0, 0.5, 0.0, -0.5, -1.0]); // one frame of samples
//! let frame = scope.render(DisplayStyle::Vfd);
//! assert_eq!(frame.data().len(), frame.width() * frame.height() * 4);
//! ```

use super::frame::{Frame, Rgba};
use super::style::{DisplayStyle, Emission, mix};

/// Default logical buffer width (pre-upscale). 144 columns at [`DEFAULT_SCALE`]
/// render 288 px wide — inside the ~296 px sidebar card (the #313 lesson).
const DEFAULT_COLS: usize = 144;
/// Default logical buffer height (pre-upscale). 48 rows at [`DEFAULT_SCALE`]
/// render 96 px tall — a classic wide scope face.
const DEFAULT_ROWS: usize = 48;
/// Default integer upscale baked into the output ([`Frame::upscale`]): chunky,
/// nearest-neighbor pixels, the kit's house look.
const DEFAULT_SCALE: usize = 2;

/// Default phosphor persistence: 256ths of intensity **retained** each tick.
/// `184/256 ≈ 0.72`, so a full-intensity beam pixel fades `255 → 183 → 131 →
/// 94 → 67 → 48 → …` — a visible trail of ~6-7 frames. Higher is a
/// longer-persistence phosphor (`256` never fades); `0` clears every tick.
const DEFAULT_PERSISTENCE: u16 = 184;

/// The persistence ceiling: `256/256 = 1.0`, an infinite-persistence phosphor
/// that never fades. Values above this are meaningless, so the builder clamps.
const MAX_PERSISTENCE: u16 = 256;

/// Graticule grid pitch in logical pixels — a grid line every this many
/// columns/rows.
const GRID_DIV: usize = 12;
/// Graticule grid intensity, toward the ink (of 255): a faint reference grid.
const GRID_T: u16 = 26;
/// Graticule center-cross intensity, toward the ink (of 255): brighter than the
/// grid so the zero axis reads clearly, still faint against the trace.
const AXIS_T: u16 = 56;

/// The beam core intensity — full brightness.
const CORE: u16 = 255;
/// The inner glow step, one pixel above/below the core.
const GLOW_INNER: u16 = 130;
/// The outer glow step, two pixels above/below the core.
const GLOW_OUTER: u16 = 45;
/// Rows of glow on each side of the beam core; the amplitude mapping keeps this
/// margin from the top/bottom so the glow never clips.
const GLOW_SPAN: usize = 2;
/// The beam's vertical glow kernel: a bright core with two dimmer falloff steps
/// above and below, so the trace reads as a soft beam, not a 1 px hard line.
const GLOW: [(i32, u16); 5] = [
    (-2, GLOW_OUTER),
    (-1, GLOW_INNER),
    (0, CORE),
    (1, GLOW_INNER),
    (2, GLOW_OUTER),
];

/// A stateful oscilloscope tile: a glow-trace waveform over a graticule with
/// real phosphor persistence. Held in the plugin's model and driven per frame
/// ([`advance`](Self::advance) with samples, [`render`](Self::render) into the
/// view). See the module docs for the decay/glow/graticule mechanics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// Logical buffer width in columns (pre-upscale).
    cols: usize,
    /// Logical buffer height in rows (pre-upscale).
    rows: usize,
    /// Integer upscale baked into the rendered [`Frame`].
    scale: usize,
    /// Intensity retained per tick, in 256ths (see [`DEFAULT_PERSISTENCE`]).
    persistence: u16,
    /// The persistence buffer: one intensity (`0..=255`) per logical pixel,
    /// row-major, carried across frames — this is the phosphor.
    phosphor: Vec<u16>,
}

impl Scope {
    /// A scope at the default geometry ([`DEFAULT_COLS`]×[`DEFAULT_ROWS`] at
    /// [`DEFAULT_SCALE`]) and default [`persistence`](Self::persistence), its
    /// phosphor at rest (a dark screen).
    #[must_use]
    pub fn new() -> Self {
        Self::with_size(DEFAULT_COLS, DEFAULT_ROWS)
    }

    /// A scope with an explicit **logical** buffer size (pre-upscale), clamped
    /// to at least 1×1. The rendered frame is `width`×`scale` by `height`×`scale`
    /// px — keep it within the ~296 px sidebar card (the default is 288 px wide).
    #[must_use]
    pub fn with_size(width: usize, height: usize) -> Self {
        let cols = width.max(1);
        let rows = height.max(1);
        Self {
            cols,
            rows,
            scale: DEFAULT_SCALE,
            persistence: DEFAULT_PERSISTENCE,
            phosphor: vec![0u16; cols * rows],
        }
    }

    /// Set the integer upscale baked into the output (clamped to at least 1) —
    /// the kit bakes chunkiness into the buffer rather than leaning on shell CSS
    /// (the `.caw-lcd` lesson). A consuming builder; call it at construction.
    #[must_use]
    pub fn scale(mut self, factor: usize) -> Self {
        self.scale = factor.max(1);
        self
    }

    /// Set the phosphor persistence: 256ths of beam intensity **retained** each
    /// [`advance`](Self::advance) tick (clamped to `0..=256`). Higher is a
    /// longer beam trail; `256` never fades, `0` clears every tick. Default:
    /// [`DEFAULT_PERSISTENCE`] (`≈0.72`). A consuming builder.
    #[must_use]
    pub fn persistence(mut self, retained_256ths: u16) -> Self {
        self.persistence = retained_256ths.min(MAX_PERSISTENCE);
        self
    }

    /// The rendered frame width in px (logical columns × [`scale`](Self::scale)).
    #[must_use]
    pub fn width(&self) -> usize {
        self.cols * self.scale
    }

    /// The rendered frame height in px (logical rows × [`scale`](Self::scale)).
    #[must_use]
    pub fn height(&self) -> usize {
        self.rows * self.scale
    }

    /// Advance one frame: decay the whole phosphor buffer exponentially, then
    /// stamp the fresh interpolated glow trace of `samples` at full intensity.
    ///
    /// `samples` is any normalized `-1.0..=1.0` signal (see the module docs);
    /// out-of-range and non-finite values are clamped defensively, and an empty
    /// slice flatlines on the axis while the old trail keeps decaying.
    pub fn advance(&mut self, samples: &[f32]) {
        // 1. Exponential phosphor decay — the whole trail dims toward black.
        for v in &mut self.phosphor {
            *v = decayed(*v, self.persistence);
        }

        // 2. Stamp the fresh trace at full intensity as a connected polyline:
        // each column's row is joined to the previous column's row so a steep
        // waveform stays continuous (never gaps into disconnected dots).
        let mut prev: Option<usize> = None;
        for x in 0..self.cols {
            let value = sample_at(samples, x, self.cols);
            let row = row_for(value, self.rows);
            let (lo, hi) = match prev {
                Some(p) => (p.min(row), p.max(row)),
                None => (row, row),
            };
            for y in lo..=hi {
                self.stamp_beam(x, y);
            }
            prev = Some(row);
        }
    }

    /// Compose the current frame in `style`: the graticule (redrawn flat, so it
    /// never decays or flickers) under the bloomed phosphor trace, composited
    /// toward the skin's accent-tinted ink, then upscaled by [`scale`](Self::scale).
    /// The buffer is fully opaque and always satisfies the host's
    /// `len == w * h * 4` invariant.
    #[must_use]
    pub fn render(&self, style: DisplayStyle) -> Frame {
        let palette = style.palette();
        let mut frame = Frame::filled(self.cols, self.rows, palette.bg);

        // Graticule: flat, under the trace, redrawn every frame from the field
        // color so it is stable and never picks up the trace's decay.
        self.draw_graticule(&mut frame, palette.bg, palette.ink);

        // Trace: lift the persistence buffer into the kit's emission grid, bloom
        // it on glowing skins, and composite toward the (accent-tinted) ink.
        let mut lit = Emission::new(self.cols, self.rows);
        for y in 0..self.rows {
            for x in 0..self.cols {
                let v = self.phosphor[y * self.cols + x];
                if v > 0 {
                    lit.add(x, y, v);
                }
            }
        }
        if let Some(bloom) = palette.bloom {
            lit.bloom(bloom);
        }
        lit.composite(&mut frame, palette.ink);

        frame.upscale(self.scale)
    }

    /// [`advance`](Self::advance) then [`render`](Self::render) in one call — the
    /// convenience for a plugin that advances and re-renders on the same tick.
    #[must_use]
    pub fn tick(&mut self, samples: &[f32], style: DisplayStyle) -> Frame {
        self.advance(samples);
        self.render(style)
    }

    /// Stamp one beam column pixel and its vertical glow into the phosphor,
    /// taking the max with what is already there — a fresh full-intensity trace
    /// overwrites a decayed trail, but a decayed trail elsewhere is untouched.
    fn stamp_beam(&mut self, x: usize, y: usize) {
        for &(dy, intensity) in &GLOW {
            // i64 keeps a glow row above the top edge exact; a negative row
            // fails the usize conversion and clips, same as `Frame::plot`.
            let yy = i64::try_from(y).unwrap_or(i64::MAX) + i64::from(dy);
            if let Ok(yu) = usize::try_from(yy) {
                self.stamp(x, yu, intensity);
            }
        }
    }

    /// Raise the phosphor at (`x`, `y`) to at least `intensity`, silently
    /// clipping out-of-bounds (same contract as [`Frame::plot`]).
    fn stamp(&mut self, x: usize, y: usize, intensity: u16) {
        if x >= self.cols || y >= self.rows {
            return;
        }
        let i = y * self.cols + x;
        self.phosphor[i] = self.phosphor[i].max(intensity);
    }

    /// Paint the graticule flat into `frame`: a faint grid every [`GRID_DIV`]
    /// columns/rows, then a brighter center cross. Each pixel is mixed from the
    /// field color toward the ink (not blended over what is under it), so the
    /// grid is a stable, idempotent reference regardless of how lines overlap.
    fn draw_graticule(&self, frame: &mut Frame, bg: Rgba, ink: Rgba) {
        let grid = mix(bg, ink, GRID_T);
        // Vertical grid lines.
        for x in (0..self.cols).step_by(GRID_DIV) {
            for y in 0..self.rows {
                frame.set(x, y, grid);
            }
        }
        // Horizontal grid lines.
        for y in (0..self.rows).step_by(GRID_DIV) {
            for x in 0..self.cols {
                frame.set(x, y, grid);
            }
        }
        // The center cross (the zero axis), brighter, drawn last so it wins.
        let axis = mix(bg, ink, AXIS_T);
        let cx = self.cols / 2;
        let cy = self.rows / 2;
        for y in 0..self.rows {
            frame.set(cx, y, axis);
        }
        for x in 0..self.cols {
            frame.set(x, cy, axis);
        }
    }
}

impl Default for Scope {
    fn default() -> Self {
        Self::new()
    }
}

/// Decay one phosphor intensity by the persistence factor: `v * retained / 256`,
/// truncating toward zero. Exponential (a fixed multiplicative factor per tick,
/// so the absolute drop scales with the value) and monotone non-increasing;
/// with `retained < 256` any positive value strictly falls, bottoming out at 0.
fn decayed(v: u16, retained: u16) -> u16 {
    let scaled = (u32::from(v) * u32::from(retained)) >> 8;
    u16::try_from(scaled).unwrap_or(u16::MAX)
}

/// The interpolated sample value for logical column `x` of `width`, in
/// `-1.0..=1.0`. Linear interpolation across the sample points turns sparse
/// bins into a continuous waveform. Non-finite and out-of-range samples are
/// clamped defensively; an empty slice reads as `0.0` (flatline on the axis).
#[allow(clippy::cast_precision_loss)]
fn sample_at(samples: &[f32], x: usize, width: usize) -> f32 {
    match samples.len() {
        0 => 0.0,
        1 => sanitize(samples[0]),
        count => {
            // Fractional position in sample space: 0 at the left column, count-1
            // at the right, so the trace spans the full width edge to edge.
            let span = (count - 1) as f32;
            let across = if width <= 1 {
                0.0
            } else {
                x as f32 / (width - 1) as f32
            };
            let pos = across * span;
            // `pos` is in `0.0..=span`, so its floor is a valid `0..=count-1`
            // index and `idx + 1` clamps to the last sample at the right edge.
            let idx = clamp_index(pos, count);
            let next = (idx + 1).min(count - 1);
            let frac = pos - pos.floor();
            let lo = sanitize(samples[idx]);
            let hi = sanitize(samples[next]);
            lo + (hi - lo) * frac
        }
    }
}

/// Clamp a sample into `-1.0..=1.0`, mapping any non-finite value (`NaN`,
/// `±inf`) to `0.0` — the axis — so no input can push the beam out of bounds.
fn sanitize(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// The integer sample index for a non-negative fractional position, clamped to
/// `0..=n-1`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn clamp_index(pos: f32, n: usize) -> usize {
    // `pos` is finite and `>= 0.0`, so the truncating cast is exact and never
    // wraps; the `.min` caps it below the sample count.
    (pos as usize).min(n - 1)
}

/// Map a `-1.0..=1.0` value to a buffer row: `0.0` is the center axis, `+1.0`
/// the top, `-1.0` the bottom. The amplitude leaves a [`GLOW_SPAN`] margin so
/// the beam's glow never clips at the top/bottom edge; the result is clamped
/// into `0..=height-1`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn row_for(value: f32, height: usize) -> usize {
    if height == 0 {
        return 0;
    }
    let last = (height - 1) as f32;
    let center = last / 2.0;
    let amplitude = (center - GLOW_SPAN as f32).max(0.0);
    // `value` is already clamped to `-1.0..=1.0`, so `row` sits within the
    // buffer up to the glow margin; the final clamp guards the degenerate
    // small-buffer case where the margin ate the whole amplitude.
    let row = (center - value * amplitude).round().clamp(0.0, last);
    row as usize
}

#[cfg(test)]
mod tests {
    use super::super::DisplayStyle;
    use super::{
        CORE, DEFAULT_PERSISTENCE, GLOW_INNER, GLOW_OUTER, GLOW_SPAN, Scope, decayed, row_for,
        sample_at,
    };

    /// The intensity at logical pixel (`x`, `y`) in a scope's phosphor buffer.
    fn phos(s: &Scope, x: usize, y: usize) -> u16 {
        s.phosphor[y * s.cols + x]
    }

    // ── Phosphor decay ───────────────────────────────────────────────────────

    /// Decay is **exponential** (a fixed multiplicative factor, so the absolute
    /// drop scales with the value) and **monotone** non-increasing, bottoming
    /// out at zero — never a linear constant-drop.
    #[test]
    fn decay_is_exponential_and_monotone() {
        let p = DEFAULT_PERSISTENCE;
        // The exact multiplicative rule (not a linear `v - c`).
        for v in [0u16, 1, 64, 128, 200, 255] {
            let want = u16::try_from((u32::from(v) * u32::from(p)) >> 8).unwrap();
            assert_eq!(decayed(v, p), want, "v={v}");
            assert!(decayed(v, p) <= v, "never rises (v={v})");
        }
        // Exponential signature: a bigger value drops by more in absolute terms
        // (a linear decay would drop the same amount regardless).
        let drop = |v: u16| v - decayed(v, p);
        assert!(
            drop(255) > drop(128) && drop(128) > drop(64),
            "the per-tick drop scales with the value — exponential, not linear"
        );
        // Repeated application is a strictly decreasing sequence to zero.
        let mut v = 255u16;
        let mut prev = u16::MAX;
        let mut ticks = 0;
        while v > 0 {
            assert!(v < prev, "each tick falls");
            prev = v;
            v = decayed(v, p);
            ticks += 1;
            assert!(ticks < 1000, "decays to zero in finite time");
        }
    }

    /// A quiet frame after a loud one fades the *old* trail geometrically rather
    /// than redrawing from black — a full-intensity beam pixel off the axis
    /// decays tick by tick per the exact `decayed` rule, monotone toward zero.
    #[test]
    fn the_old_trail_fades_not_redraws() {
        // A single sample holds the whole trace flat near the top, well off the
        // center axis, so the empty-input flatline never re-lights it.
        let mut s = Scope::with_size(48, 48).persistence(DEFAULT_PERSISTENCE);
        s.advance(&[1.0]);
        let top = row_for(1.0, s.rows);
        assert_eq!(
            phos(&s, 10, top),
            CORE,
            "the fresh beam is at full intensity"
        );

        let mut expected = CORE;
        let mut prev = u16::MAX;
        for _ in 0..6 {
            s.advance(&[]); // silence: flatline at the axis, old top trail decays
            expected = decayed(expected, s.persistence);
            let now = phos(&s, 10, top);
            assert_eq!(now, expected, "the trail follows the exact decay rule");
            assert!(now < prev, "and keeps fading, never redrawn to full");
            prev = now;
        }
        assert!(prev < CORE, "the trail is a ghost of the original beam");
    }

    // ── Glow trace ───────────────────────────────────────────────────────────

    /// The beam is a soft glow, not a hard line: the core is full intensity and
    /// its vertical neighbors step strictly down through the two glow levels.
    #[test]
    fn glow_neighbors_are_dimmer_than_the_core() {
        let mut s = Scope::with_size(64, 40);
        s.advance(&[0.0]); // a flat trace on the center axis
        let cx = 20;
        let axis = row_for(0.0, s.rows);
        let core = phos(&s, cx, axis);
        let inner = phos(&s, cx, axis - 1);
        let outer = phos(&s, cx, axis - 2);
        // The rendered glow steps strictly down from the core through two levels.
        assert!(
            core > inner && inner > outer,
            "the core is brighter than its glow neighbors: {core} > {inner} > {outer}"
        );
        // It is symmetric above/below the core and matches the named kernel.
        assert_eq!(core, CORE, "the core is full intensity");
        assert_eq!(
            (inner, phos(&s, cx, axis + 1)),
            (GLOW_INNER, GLOW_INNER),
            "inner glow, both sides"
        );
        assert_eq!(
            (outer, phos(&s, cx, axis + 2)),
            (GLOW_OUTER, GLOW_OUTER),
            "outer glow, both sides"
        );
        // Nothing lit beyond the glow span.
        assert_eq!(phos(&s, cx, axis - GLOW_SPAN - 1), 0, "dark past the glow");
    }

    // ── Continuous / interpolated trace ──────────────────────────────────────

    /// `sample_at` interpolates linearly between the sample points — the value
    /// at a column *between* two bins is their weighted blend, not a step.
    #[test]
    fn sample_at_interpolates_between_bins() {
        // Two samples span the whole width: +1 at the left, -1 at the right, so
        // the exact middle column interpolates to 0 even though no sample sits
        // there — proof it's a polyline, not a bar chart.
        let width = 101; // odd, so column 50 is the exact middle
        assert!(
            (sample_at(&[1.0, -1.0], 0, width) - 1.0).abs() < 1e-6,
            "left = +1"
        );
        assert!(
            (sample_at(&[1.0, -1.0], width - 1, width) + 1.0).abs() < 1e-6,
            "right = -1"
        );
        assert!(
            sample_at(&[1.0, -1.0], 50, width).abs() < 1e-6,
            "the middle interpolates to 0 — a connected trace"
        );
        // A quarter of the way is a quarter of the way down the ramp.
        assert!(
            (sample_at(&[1.0, -1.0], 25, width) - 0.5).abs() < 1e-6,
            "quarter column = +0.5"
        );
    }

    /// A known ramp lights the expected rows across columns, including an
    /// interpolated column that no raw sample lands on.
    #[test]
    fn a_known_input_lights_expected_rows() {
        let mut s = Scope::with_size(101, 60);
        s.advance(&[1.0, -1.0]); // ramp top → bottom
        // Left column at the top, right column at the bottom, middle on the axis.
        assert_eq!(phos(&s, 0, row_for(1.0, s.rows)), CORE, "left at the top");
        assert_eq!(
            phos(&s, s.cols - 1, row_for(-1.0, s.rows)),
            CORE,
            "right at the bottom"
        );
        let mid = s.cols / 2;
        assert_eq!(
            phos(&s, mid, row_for(0.0, s.rows)),
            CORE,
            "the interpolated middle column sits on the axis"
        );
        // The trace descends monotonically across the ramp (rows increase L→R).
        let row_of = |x: usize| (0..s.rows).find(|&y| phos(&s, x, y) == CORE).unwrap();
        assert!(row_of(0) < row_of(mid) && row_of(mid) < row_of(s.cols - 1));
    }

    /// A steep waveform stays a **connected** trace: consecutive columns are
    /// vertically joined, so there is no gap in the beam even when the value
    /// jumps between adjacent bins.
    #[test]
    fn steep_transitions_stay_connected() {
        let mut s = Scope::with_size(3, 48);
        // Only 3 columns for 2 bins ⇒ the jump from +1 to -1 happens in one
        // column step; the connector must fill the whole span between them.
        s.advance(&[1.0, -1.0]);
        let top = row_for(1.0, s.rows);
        let bottom = row_for(-1.0, s.rows);
        // No horizontal gap: every row of the full top→bottom sweep is lit in
        // some column, so the beam is one connected trace, not disjoint dots.
        for y in top..=bottom {
            assert!(
                (0..s.cols).any(|x| phos(&s, x, y) > 0),
                "some column lights row {y} — the trace is connected"
            );
        }
    }

    // ── Graticule ────────────────────────────────────────────────────────────

    /// The graticule is redrawn flat each frame and never decays or flickers: a
    /// grid pixel far from the trace is identical across many ticks.
    #[test]
    fn graticule_persists_undecayed() {
        let mut s = Scope::with_size(48, 48);
        let style = DisplayStyle::Oled;
        // A grid pixel near the top edge, far from the center axis flatline and
        // any bloom off it (a vertical grid line, top row, in final px).
        let gx = i32::try_from(12 * s.scale).unwrap();
        let gy = 0;
        let baseline = s.render(style).get(gx, gy).unwrap();
        assert_ne!(
            baseline,
            style.palette().bg,
            "the graticule paints a visible grid pixel"
        );
        for _ in 0..8 {
            s.advance(&[]); // silence: only the axis flatline is stamped
            let now = s.render(style).get(gx, gy).unwrap();
            assert_eq!(now, baseline, "the grid pixel never decays or flickers");
        }
    }

    // ── Empty / defensive input ──────────────────────────────────────────────

    /// Empty input flatlines on the center axis (and only there) — the beam
    /// rests on the zero line, honestly.
    #[test]
    fn empty_input_flatlines_on_the_axis() {
        let mut s = Scope::with_size(40, 40);
        s.advance(&[]);
        let axis = row_for(0.0, s.rows);
        for x in 0..s.cols {
            assert_eq!(phos(&s, x, axis), CORE, "the flatline lights the axis row");
        }
        // Nothing lit beyond the glow span around the axis.
        assert_eq!(phos(&s, 5, axis - GLOW_SPAN - 1), 0, "dark above the beam");
        assert_eq!(phos(&s, 5, axis + GLOW_SPAN + 1), 0, "dark below the beam");
    }

    /// Out-of-range and non-finite samples are clamped defensively: the beam
    /// still draws, stays inside the buffer, and nothing panics.
    #[test]
    fn defensive_clamp_handles_extreme_and_nan_samples() {
        let mut s = Scope::with_size(32, 32);
        s.advance(&[9.0, -9.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0]);
        assert!(s.phosphor.contains(&CORE), "the beam still draws");
        // Clamping lives at the sample layer: out-of-range values saturate to
        // the endpoints and non-finite values read as the axis (0.0).
        let w = s.cols;
        assert!(
            (sample_at(&[9.0], 0, w) - 1.0).abs() < 1e-6,
            "over-unit clamps to +1"
        );
        assert!(
            (sample_at(&[-9.0], 0, w) + 1.0).abs() < 1e-6,
            "under-unit clamps to -1"
        );
        assert!(
            sample_at(&[f32::NAN], 0, w).abs() < 1e-6,
            "NaN reads as the axis"
        );
        assert!(
            sample_at(&[f32::INFINITY], 0, w).abs() < 1e-6,
            "inf reads as the axis"
        );
        assert!(
            sample_at(&[f32::NEG_INFINITY], 0, w).abs() < 1e-6,
            "-inf reads as the axis"
        );
        // And the output row is always inside the buffer, even for a raw
        // over-unit value that skipped sanitizing.
        assert!(row_for(9.0, s.rows) < s.rows, "row stays in bounds");
        assert!(row_for(-9.0, s.rows) < s.rows, "row stays in bounds");
    }

    // ── Sizing / host invariant ──────────────────────────────────────────────

    /// The rendered buffer follows the scale hint, fits the sidebar card, and
    /// clamps degenerate sizes/scales rather than producing a broken buffer.
    #[test]
    fn buffer_dimensions_follow_the_scale_hint() {
        let s = Scope::new();
        let f = s.render(DisplayStyle::Vfd);
        assert_eq!((f.width(), f.height()), (s.width(), s.height()));
        assert_eq!(f.width(), 288, "the default fits the ~296 px sidebar card");
        assert!(f.width() <= 296);
        // The scale multiplies both dimensions (nearest-neighbor upscale).
        let scaled = Scope::with_size(20, 10).scale(3);
        assert_eq!((scaled.width(), scaled.height()), (60, 30));
        assert_eq!(scaled.render(DisplayStyle::Vfd).width(), 60);
        // Degenerate sizes/scales clamp to at least 1, never a zero buffer.
        let tiny = Scope::with_size(0, 0).scale(0);
        let tf = tiny.render(DisplayStyle::Oled);
        assert_eq!((tf.width(), tf.height()), (1, 1));
        assert_eq!(tf.data().len(), tf.width() * tf.height() * 4);
    }

    /// The host invariant across skins and inputs, and the frame is a screen:
    /// fully opaque, `len == w * h * 4`, for every input including the extremes.
    #[test]
    fn every_render_satisfies_the_host_invariant() {
        let inputs: [&[f32]; 5] = [
            &[],
            &[0.5],
            &[0.0, 0.5, 1.0, -1.0, 0.25],
            &[1.0; 16],
            &[9.0, f32::NAN, -9.0],
        ];
        for style in DisplayStyle::ALL {
            for samples in inputs {
                let mut s = Scope::with_size(64, 32);
                let f = s.tick(samples, style);
                assert_eq!(
                    f.data().len(),
                    f.width() * f.height() * 4,
                    "{style:?} {samples:?}"
                );
                assert!(f.width() > 0 && f.height() > 0);
                assert!(
                    f.data().chunks_exact(4).all(|px| px[3] == 0xff),
                    "{style:?} {samples:?}: the scope is a screen, wall to wall"
                );
            }
        }
    }

    /// Renders are deterministic and the three skins render differently.
    #[test]
    fn render_is_deterministic_and_skins_differ() {
        let mut a = Scope::with_size(48, 24);
        let mut b = Scope::with_size(48, 24);
        let sig = [0.0, 0.4, 0.8, 0.4, 0.0, -0.4, -0.8];
        a.advance(&sig);
        b.advance(&sig);
        assert_eq!(
            a.render(DisplayStyle::Vfd),
            b.render(DisplayStyle::Vfd),
            "same inputs, same bytes"
        );
        let vfd = a.render(DisplayStyle::Vfd);
        let lcd = a.render(DisplayStyle::Lcd);
        let oled = a.render(DisplayStyle::Oled);
        assert_ne!(vfd, lcd);
        assert_ne!(vfd, oled);
        assert_ne!(lcd, oled);
    }
}
