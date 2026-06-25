//! Multi-series time-series visualization (GNOME-System-Monitor-style).
//! Owns N fixed-capacity ring buffers of `f64` samples and renders each
//! as its own anti-aliased polyline via cairo, one evenly-spread HSL hue
//! per series. The sibling of [`Sparkline`](crate::Sparkline): same ring
//! buffer + cairo draw shape, but N polylines instead of one.
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

        cr.new_path();
        for (i, sample) in samples.iter().enumerate() {
            let x = crate::cast::usize_to_f64(i) * step_x;
            let norm = (*sample / denom).clamp(0.0, 1.0);
            let y = h - norm * h;
            if i == 0 {
                cr.move_to(x, y);
            } else {
                cr.line_to(x, y);
            }
        }
        let (r, g, b) = series_color(idx, count);
        cr.set_source_rgba(r, g, b, STROKE_ALPHA);
        let _ = cr.stroke();
    }
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

// These call gtk::init() / build widgets, so they need a display server —
// gated into the `system-tests` bucket rather than run by default.
#[cfg(all(test, feature = "system-tests"))]
mod widget_tests {
    use super::*;

    fn ensure_gtk_init() {
        static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ONCE.get_or_init(|| {
            gtk::init().ok();
        });
    }

    #[test]
    fn push_frame_caps_each_series_at_capacity() {
        ensure_gtk_init();
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

    #[test]
    fn changing_width_resets_series() {
        ensure_gtk_init();
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

    #[test]
    fn empty_frame_is_noop_shape() {
        ensure_gtk_init();
        let g = MultiSparkline::new(5);
        g.push_frame(&[]);
        assert!(g.series.borrow().is_empty());
    }

    #[test]
    fn clear_empties() {
        ensure_gtk_init();
        let g = MultiSparkline::new(10);
        g.push_frame(&[1.0, 2.0]);
        g.clear();
        assert!(g.series.borrow().is_empty());
    }

    #[test]
    fn set_domain_max_round_trips() {
        ensure_gtk_init();
        let g = MultiSparkline::new(5);
        g.set_domain_max(Some(1.0));
        assert_eq!(g.domain_max.get(), Some(1.0));
        g.set_domain_max(None);
        assert_eq!(g.domain_max.get(), None);
    }
}
