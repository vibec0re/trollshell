//! Multi-series time-series visualization (GNOME-System-Monitor-style).
//! Owns N fixed-capacity ring buffers of `f64` samples and renders each
//! as its own anti-aliased smooth (Catmull-Rom) curve via cairo, one
//! evenly-spread HSL hue per series. The sibling of
//! [`Sparkline`](crate::Sparkline): same ring buffer + cairo draw shape, but
//! N smoothed curves instead of one.
//!
//! Unlike `Sparkline` (whose single line resolves through the widget's GTK4
//! theme color), the per-series colors are **generated** — `hue = i / count`
//! around the full circle at a fixed saturation/lightness tuned for a dark
//! UI — so the palette scales to any series count (8, 16, 32+ cores) without
//! repeating, matching GNOME System Monitor's per-core hue spread.
//!
//! Used by `trollshell`'s stats page CPU card for the per-core history graph;
//! designed to be reusable for any future multi-series history surface.
//!
//! # Example
//!
//! ```ignore
//! let g = MultiSparkline::new(60);
//! g.set_domain_max(Some(1.0));   // fractions in 0..=1 (per-core load)
//! container.append(g.widget());
//!
//! // Each tick, push one snapshot — one value per series:
//! g.push_frame(&cpu_load.per_core);
//! ```

use gtk::cairo;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

/// Saturation of the generated per-series hues (`0.0..=1.0`). Tuned vivid but
/// not neon so lines stay legible on a dark background.
const SERIES_SATURATION: f64 = 0.65;
/// Lightness of the generated per-series hues (`0.0..=1.0`). Mid-bright so the
/// colors read clearly over a dark UI without washing out.
const SERIES_LIGHTNESS: f64 = 0.6;
/// Stroke alpha. Slightly translucent so overlapping core lines remain
/// distinguishable where they cross.
const STROKE_ALPHA: f64 = 0.85;

#[derive(Clone)]
pub struct MultiSparkline {
    inner: gtk::DrawingArea,
    /// One ring buffer per series. The outer length is the series count; it is
    /// reset (cleared and re-sized) whenever a pushed frame's width changes, so
    /// a changing core count is handled gracefully.
    series: Rc<RefCell<Vec<VecDeque<f64>>>>,
    capacity: usize,
    domain_max: Rc<Cell<Option<f64>>>,
}

impl MultiSparkline {
    /// Build a multi-series sparkline that retains the most recent `capacity`
    /// frames per series. `capacity` MUST be > 0 (panics otherwise — caller
    /// error).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "MultiSparkline capacity must be > 0");

        let inner = gtk::DrawingArea::new();
        inner.add_css_class("ts-multi-sparkline");

        let series: Rc<RefCell<Vec<VecDeque<f64>>>> = Rc::new(RefCell::new(Vec::new()));
        let domain_max: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));

        let series_for_draw = series.clone();
        let domain_max_for_draw = domain_max.clone();
        inner.set_draw_func(move |_area, cr, width, height| {
            let series = series_for_draw.borrow();
            let dmax = domain_max_for_draw.get();
            draw_multi_sparkline(cr, width, height, &series, dmax);
        });

        Self {
            inner,
            series,
            capacity,
            domain_max,
        }
    }

    /// The underlying widget. Cheap clone (GTK refcount).
    #[must_use]
    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.inner
    }

    /// Push one frame: one sample per series, in series order. Drops the oldest
    /// frame per series once at capacity, and queues a redraw.
    ///
    /// If `frame.len()` differs from the current series count (e.g. the first
    /// real frame after construction, or a CPU hot-plug changing the core
    /// count), the buffers are reset to the new width and history restarts.
    pub fn push_frame(&self, frame: &[f64]) {
        {
            let mut series = self.series.borrow_mut();
            if series.len() != frame.len() {
                series.clear();
                series.resize_with(frame.len(), || VecDeque::with_capacity(self.capacity));
            }
            for (buf, &sample) in series.iter_mut().zip(frame.iter()) {
                if buf.len() == self.capacity {
                    buf.pop_front();
                }
                buf.push_back(sample);
            }
        }
        self.inner.queue_draw();
    }

    /// Replace every series' ring with `frames` (one ring per series, oldest
    /// first, most recent last), keeping at most `capacity` trailing samples per
    /// series. The multi-series twin of [`Sparkline::set_samples`]: for when the
    /// history is owned elsewhere — the sensors service (#338) republishes the
    /// whole per-core window as a snapshot each tick, so a lazily-built per-core
    /// row backfills instantly instead of opening to empty/short graphs. Queues
    /// a redraw.
    ///
    /// The series count is taken from `frames.len()` — a differing width just
    /// re-seeds to the new width (matching [`push_frame`]'s hot-plug reset).
    ///
    /// [`Sparkline::set_samples`]: crate::Sparkline::set_samples
    /// [`push_frame`]: Self::push_frame
    pub fn set_frames(&self, frames: &[VecDeque<f64>]) {
        {
            let mut series = self.series.borrow_mut();
            series.clear();
            for frame in frames {
                let skip = frame.len().saturating_sub(self.capacity);
                series.push(frame.iter().skip(skip).copied().collect());
            }
        }
        self.inner.queue_draw();
    }

    /// Set a fixed domain max (e.g. `Some(1.0)` for 0..=1 fractions).
    /// `None` enables auto-scaling to the max sample across all series.
    pub fn set_domain_max(&self, max: Option<f64>) {
        self.domain_max.set(max);
        self.inner.queue_draw();
    }

    /// Drop all series and samples. Triggers a redraw.
    pub fn clear(&self) {
        self.series.borrow_mut().clear();
        self.inner.queue_draw();
    }
}

// ── Drawing ───────────────────────────────────────────────────────────────────

#[allow(clippy::many_single_char_names)]
fn draw_multi_sparkline(
    cr: &cairo::Context,
    width: i32,
    height: i32,
    series: &[VecDeque<f64>],
    domain_max: Option<f64>,
) {
    if width <= 0 || height <= 0 || series.is_empty() {
        return;
    }
    cr.set_antialias(cairo::Antialias::Default);

    let w = f64::from(width);
    let h = f64::from(height);
    let count = series.len();

    let denom = match domain_max {
        Some(m) if m > 0.0 => m,
        _ => series
            .iter()
            .flat_map(|s| s.iter().copied())
            .fold(0.0_f64, f64::max)
            .max(f64::EPSILON),
    };

    cr.set_line_width(1.5);
    for (idx, samples) in series.iter().enumerate() {
        if samples.is_empty() {
            continue;
        }
        let n = crate::cast::usize_to_f64(samples.len());
        let step_x = if n <= 1.0 { 0.0 } else { w / (n - 1.0) };

        // Project samples to pixel coordinates inside the plot box. Both axes
        // are already bounded: x ∈ [0, w] (i·step_x) and y ∈ [0, h] (norm
        // clamped to 0..=1), so every *data* point sits inside [0,w]×[0,h].
        let points: Vec<(f64, f64)> = samples
            .iter()
            .enumerate()
            .map(|(i, sample)| {
                let x = crate::cast::usize_to_f64(i) * step_x;
                let norm = (*sample / denom).clamp(0.0, 1.0);
                let y = h - norm * h;
                (x, y)
            })
            .collect();

        // Smooth the top edge with a Catmull-Rom spline expressed as cubic
        // beziers (GNOME-System-Monitor look). A lone point (or empty series)
        // can't form a segment: the single `move_to` strokes nothing, matching
        // the prior 1-sample render.
        cr.new_path();
        cr.move_to(points[0].0, points[0].1);
        for i in 0..points.len() - 1 {
            // Neighbours clamp to the endpoints at the boundaries (p0=p1 for the
            // first segment, p3=p2 for the last) — standard Catmull-Rom edges.
            let p0 = points[i.saturating_sub(1)];
            let p1 = points[i];
            let p2 = points[i + 1];
            let p3 = points[(i + 2).min(points.len() - 1)];
            let (c1, c2) = catmull_rom_control_points(p0, p1, p2, p3, w, h);
            cr.curve_to(c1.0, c1.1, c2.0, c2.1, p2.0, p2.1);
        }
        let (r, g, b) = series_color(idx, count);
        cr.set_source_rgba(r, g, b, STROKE_ALPHA);
        let _ = cr.stroke();
    }
}

/// Catmull-Rom → cubic-bezier control points for the segment `p1`→`p2`, given
/// its neighbours `p0` and `p3` (clamp `p0=p1` / `p3=p2` at the curve ends).
///
/// Returns the two bezier control points
/// `C1 = p1 + (p2 - p0)/6` and `C2 = p2 - (p3 - p1)/6`, each **clamped into the
/// plot box** `[0,w]×[0,h]`. Catmull-Rom can overshoot on sharp peaks; clamping
/// only the control points (never the data points) holds the smoothed curve
/// tight against the top edge and baseline without a visible bulge, while
/// keeping it smooth.
#[allow(clippy::many_single_char_names)]
fn catmull_rom_control_points(
    p0: (f64, f64),
    p1: (f64, f64),
    p2: (f64, f64),
    p3: (f64, f64),
    w: f64,
    h: f64,
) -> ((f64, f64), (f64, f64)) {
    let c1 = (
        (p2.0 - p0.0).mul_add(1.0 / 6.0, p1.0).clamp(0.0, w),
        (p2.1 - p0.1).mul_add(1.0 / 6.0, p1.1).clamp(0.0, h),
    );
    let c2 = (
        (p3.0 - p1.0).mul_add(-1.0 / 6.0, p2.0).clamp(0.0, w),
        (p3.1 - p1.1).mul_add(-1.0 / 6.0, p2.1).clamp(0.0, h),
    );
    (c1, c2)
}

// ── Color palette ─────────────────────────────────────────────────────────────

/// Color for series `index` of `count`, as evenly-spread `(r, g, b)` in
/// `0.0..=1.0`. Hue cycles the full circle (`index / count`) at a fixed
/// saturation/lightness, so the palette never repeats regardless of count.
fn series_color(index: usize, count: usize) -> (f64, f64, f64) {
    let hue = crate::cast::usize_to_f64(index) / crate::cast::usize_to_f64(count.max(1));
    hsl_to_rgb(hue, SERIES_SATURATION, SERIES_LIGHTNESS)
}

/// Convert HSL (`h, s, l` each in `0.0..=1.0`, `h` wrapping) to RGB
/// (`0.0..=1.0`). Standard CSS-style HSL conversion.
#[allow(clippy::many_single_char_names)]
fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (f64, f64, f64) {
    if s <= 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0f64.mul_add(l, -q);
    let r = hue_to_channel(p, q, h + 1.0 / 3.0);
    let g = hue_to_channel(p, q, h);
    let b = hue_to_channel(p, q, h - 1.0 / 3.0);
    (r, g, b)
}

/// One RGB channel from the HSL helper pair `(p, q)` at hue offset `t`.
fn hue_to_channel(p: f64, q: f64, t: f64) -> f64 {
    let t = t.rem_euclid(1.0);
    if t < 1.0 / 6.0 {
        (q - p).mul_add(6.0 * t, p)
    } else if t < 1.0 / 2.0 {
        q
    } else if t < 2.0 / 3.0 {
        (q - p).mul_add((2.0 / 3.0 - t) * 6.0, p)
    } else {
        p
    }
}

// ── Pure-logic tests (hermetic — no GTK) ────────────────────────────────────────

#[cfg(test)]
mod color_tests {
    use super::{hsl_to_rgb, series_color};

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn hsl_zero_saturation_is_gray() {
        let (r, g, b) = hsl_to_rgb(0.42, 0.0, 0.3);
        assert!(approx(r, 0.3) && approx(g, 0.3) && approx(b, 0.3));
    }

    #[test]
    fn hsl_pure_red_at_hue_zero() {
        // h=0, s=1, l=0.5 → pure red (1, 0, 0).
        let (r, g, b) = hsl_to_rgb(0.0, 1.0, 0.5);
        assert!(approx(r, 1.0) && approx(g, 0.0) && approx(b, 0.0));
    }

    #[test]
    fn hsl_channels_in_unit_range() {
        for i in 0..32 {
            let h = f64::from(i) / 32.0;
            let (r, g, b) = hsl_to_rgb(h, 0.65, 0.6);
            for c in [r, g, b] {
                assert!(
                    (0.0..=1.0).contains(&c),
                    "channel {c} out of range at h={h}"
                );
            }
        }
    }

    fn colors_eq(lhs: (f64, f64, f64), rhs: (f64, f64, f64)) -> bool {
        approx(lhs.0, rhs.0) && approx(lhs.1, rhs.1) && approx(lhs.2, rhs.2)
    }

    #[test]
    fn series_color_spreads_and_does_not_repeat() {
        // 16 cores: every hue is distinct (no Adwaita-8 repeat).
        let colors: Vec<(f64, f64, f64)> = (0..16).map(|i| series_color(i, 16)).collect();
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert!(
                    !colors_eq(colors[i], colors[j]),
                    "series {i} and {j} share a color"
                );
            }
        }
    }

    #[test]
    fn series_color_handles_zero_count() {
        // Defensive: count==0 must not divide by zero.
        let (r, g, b) = series_color(0, 0);
        for c in [r, g, b] {
            assert!(c.is_finite());
        }
    }
}

#[cfg(test)]
mod curve_tests {
    use super::catmull_rom_control_points;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn flat_line_control_points_stay_flat() {
        // Evenly spaced points all at the same height → both control points
        // keep that exact y (a flat segment smooths to a flat curve).
        let y = 5.0;
        let (c1, c2) =
            catmull_rom_control_points((0.0, y), (1.0, y), (2.0, y), (3.0, y), 100.0, 10.0);
        assert!(approx(c1.1, y), "c1.y drifted: {}", c1.1);
        assert!(approx(c2.1, y), "c2.y drifted: {}", c2.1);
    }

    #[test]
    fn control_points_clamped_into_box() {
        let w = 100.0;
        let h = 10.0;
        // Raw C1.y = p1.y + (p2.y - p0.y)/6 = 0.5 + (0 - 10)/6 ≈ -1.17 → clamp 0.
        let (c1, _c2) =
            catmull_rom_control_points((0.0, 10.0), (1.0, 0.5), (2.0, 0.0), (3.0, 0.0), w, h);
        assert!((0.0..=h).contains(&c1.1), "c1.y not clamped: {}", c1.1);
        assert!(approx(c1.1, 0.0), "c1.y should clamp to 0: {}", c1.1);

        // Raw C2.y = p2.y - (p3.y - p1.y)/6 = 9.5 - (0 - 10)/6 ≈ 11.17 → clamp h.
        let (_c1, c2) =
            catmull_rom_control_points((0.0, 10.0), (1.0, 10.0), (2.0, 9.5), (3.0, 0.0), w, h);
        assert!((0.0..=h).contains(&c2.1), "c2.y not clamped: {}", c2.1);
        assert!(approx(c2.1, h), "c2.y should clamp to h: {}", c2.1);
    }

    #[test]
    fn control_points_always_inside_box() {
        // Sweep a grid of inputs: control points never leave the plot box.
        let w = 60.0;
        let h = 24.0;
        for a in 0..5 {
            for b in 0..5 {
                let p0 = (0.0, f64::from(a) * h / 4.0);
                let p1 = (15.0, f64::from(b) * h / 4.0);
                let p2 = (30.0, f64::from((a + b) % 5) * h / 4.0);
                let p3 = (45.0, f64::from((a * b) % 5) * h / 4.0);
                let (c1, c2) = catmull_rom_control_points(p0, p1, p2, p3, w, h);
                for (cx, cy) in [c1, c2] {
                    assert!((0.0..=w).contains(&cx), "x {cx} outside [0,{w}]");
                    assert!((0.0..=h).contains(&cy), "y {cy} outside [0,{h}]");
                }
            }
        }
    }

    #[test]
    fn boundary_neighbour_clamp_is_finite() {
        // First-segment boundary uses p0=p1; result stays finite and in-box.
        let (c1, c2) = catmull_rom_control_points(
            (0.0, 3.0),
            (0.0, 3.0),
            (10.0, 7.0),
            (20.0, 2.0),
            100.0,
            10.0,
        );
        for (cx, cy) in [c1, c2] {
            assert!(cx.is_finite() && cy.is_finite());
        }
    }
}

// These call gtk::init() / build widgets, so they need a display server —
// gated into the `system-tests` bucket rather than run by default.
//
// `#[gtk::test]` (not a manual `gtk::init()` OnceLock) runs every test on one
// shared GTK main thread, so these run serially but correctly under the
// default multithreaded `cargo test` harness — mirrors widget_tree.rs's
// gtk_tests. A hand-rolled OnceLock only initializes GTK on whichever thread
// happens to run first; libtest's worker pool dispatches other tests to
// different OS threads, and gtk4-rs panics ("GTK may only be used from the
// main thread") the moment one of those touches a widget.
#[cfg(all(test, feature = "system-tests"))]
mod widget_tests {
    use super::*;

    #[gtk::test]
    fn push_frame_caps_each_series_at_capacity() {
        let g = MultiSparkline::new(3);
        for i in 0..5 {
            g.push_frame(&[f64::from(i), f64::from(i) * 10.0]);
        }
        let series = g.series.borrow();
        assert_eq!(series.len(), 2);
        let s0: Vec<f64> = series[0].iter().copied().collect();
        let s1: Vec<f64> = series[1].iter().copied().collect();
        assert_eq!(s0, vec![2.0, 3.0, 4.0]);
        assert_eq!(s1, vec![20.0, 30.0, 40.0]);
    }

    #[gtk::test]
    fn changing_width_resets_series() {
        let g = MultiSparkline::new(10);
        g.push_frame(&[1.0, 2.0]);
        g.push_frame(&[3.0, 4.0]);
        // Core count changes 2 → 4: buffers reset, history restarts.
        g.push_frame(&[5.0, 6.0, 7.0, 8.0]);
        let series = g.series.borrow();
        assert_eq!(series.len(), 4);
        // Each new series holds exactly the one post-reset frame.
        for buf in series.iter() {
            assert_eq!(buf.len(), 1);
        }
    }

    #[gtk::test]
    fn empty_frame_is_noop_shape() {
        let g = MultiSparkline::new(5);
        g.push_frame(&[]);
        assert!(g.series.borrow().is_empty());
    }

    #[gtk::test]
    fn set_frames_seeds_all_series_keeping_trailing_capacity() {
        let g = MultiSparkline::new(3);
        // Two series, each longer than capacity → keep only the most recent 3.
        let s0: VecDeque<f64> = (0..5).map(f64::from).collect();
        let s1: VecDeque<f64> = (10..15).map(f64::from).collect();
        g.set_frames(&[s0, s1]);
        let series = g.series.borrow();
        assert_eq!(series.len(), 2);
        let got0: Vec<f64> = series[0].iter().copied().collect();
        let got1: Vec<f64> = series[1].iter().copied().collect();
        assert_eq!(got0, vec![2.0, 3.0, 4.0]);
        assert_eq!(got1, vec![12.0, 13.0, 14.0]);
    }

    #[gtk::test]
    fn set_frames_replaces_and_reseeds_width() {
        let g = MultiSparkline::new(10);
        g.push_frame(&[1.0, 2.0]);
        // A snapshot with a different series count replaces (not appends): the
        // core count shrinks 2 → 1 and the ring holds exactly the snapshot.
        let only: VecDeque<f64> = [7.0, 8.0].into_iter().collect();
        g.set_frames(&[only]);
        let series = g.series.borrow();
        assert_eq!(series.len(), 1);
        let got: Vec<f64> = series[0].iter().copied().collect();
        assert_eq!(got, vec![7.0, 8.0]);
    }

    #[gtk::test]
    fn set_frames_empty_clears() {
        let g = MultiSparkline::new(5);
        g.push_frame(&[1.0, 2.0]);
        g.set_frames(&[]);
        assert!(g.series.borrow().is_empty());
    }

    #[gtk::test]
    fn clear_empties() {
        let g = MultiSparkline::new(10);
        g.push_frame(&[1.0, 2.0]);
        g.clear();
        assert!(g.series.borrow().is_empty());
    }

    #[gtk::test]
    fn set_domain_max_round_trips() {
        let g = MultiSparkline::new(5);
        g.set_domain_max(Some(1.0));
        assert_eq!(g.domain_max.get(), Some(1.0));
        g.set_domain_max(None);
        assert_eq!(g.domain_max.get(), None);
    }
}
