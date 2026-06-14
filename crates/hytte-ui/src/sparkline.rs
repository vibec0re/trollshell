//! Minimal time-series visualization. Owns a fixed-capacity ring
//! buffer of `f64` samples and renders them as a single-stroke line
//! plus 15%-alpha fill via cairo. Color resolves through the widget's
//! GTK4 theme color (`.ts-sparkline { color: @accent_color; }` in
//! the consumer's stylesheet drives this).
//!
//! Used by `trollshell`'s stats page History group; designed to be
//! reusable for any future per-metric history surface.
//!
//! # Example
//!
//! ```ignore
//! let s = Sparkline::new(60);
//! s.set_domain_max(Some(1.0));   // fraction in 0..=1
//! container.append(s.widget());
//!
//! // Each tick:
//! s.push(current_load);
//! ```

use gtk::cairo;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

#[derive(Clone)]
pub struct Sparkline {
    inner: gtk::DrawingArea,
    samples: Rc<RefCell<VecDeque<f64>>>,
    capacity: usize,
    domain_max: Rc<Cell<Option<f64>>>,
}

impl Sparkline {
    /// Build a sparkline that retains the most recent `capacity` samples.
    /// `capacity` MUST be > 0 (panics otherwise — caller error).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "Sparkline capacity must be > 0");

        let inner = gtk::DrawingArea::new();
        inner.add_css_class("ts-sparkline");

        let samples: Rc<RefCell<VecDeque<f64>>> =
            Rc::new(RefCell::new(VecDeque::with_capacity(capacity)));
        let domain_max: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(None));

        let samples_for_draw = samples.clone();
        let domain_max_for_draw = domain_max.clone();
        inner.set_draw_func(move |area, cr, width, height| {
            let samples = samples_for_draw.borrow();
            let dmax = domain_max_for_draw.get();
            draw_sparkline(area, cr, width, height, &samples, dmax);
        });

        Self {
            inner,
            samples,
            capacity,
            domain_max,
        }
    }

    /// The underlying widget. Cheap clone (GTK refcount).
    #[must_use]
    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.inner
    }

    /// Push one sample. Drops the oldest if at capacity. Queues a redraw.
    pub fn push(&self, sample: f64) {
        {
            let mut s = self.samples.borrow_mut();
            if s.len() == self.capacity {
                s.pop_front();
            }
            s.push_back(sample);
        }
        self.inner.queue_draw();
    }

    /// Set a fixed domain max (e.g. `Some(1.0)` for 0..=1 fractions).
    /// `None` enables auto-scaling to the max sample currently in the
    /// ring.
    pub fn set_domain_max(&self, max: Option<f64>) {
        self.domain_max.set(max);
        self.inner.queue_draw();
    }

    /// Drop all samples. Triggers a redraw.
    pub fn clear(&self) {
        self.samples.borrow_mut().clear();
        self.inner.queue_draw();
    }
}

// ── Drawing ───────────────────────────────────────────────────────────────────

#[allow(clippy::many_single_char_names)]
fn draw_sparkline(
    area: &gtk::DrawingArea,
    cr: &cairo::Context,
    width: i32,
    height: i32,
    samples: &VecDeque<f64>,
    domain_max: Option<f64>,
) {
    if width <= 0 || height <= 0 || samples.is_empty() {
        return;
    }
    cr.set_antialias(cairo::Antialias::Default);

    let w = f64::from(width);
    let h = f64::from(height);

    let denom = match domain_max {
        Some(m) if m > 0.0 => m,
        _ => samples
            .iter()
            .copied()
            .fold(0.0_f64, f64::max)
            .max(f64::EPSILON),
    };

    #[allow(clippy::cast_precision_loss)]
    let count = samples.len() as f64;
    let step_x = if count <= 1.0 { 0.0 } else { w / (count - 1.0) };

    // Resolve theme color via widget.color() — driven by
    // `.ts-sparkline { color: @accent_color; }` in CSS.
    let color = area.color();
    let r = f64::from(color.red());
    let g = f64::from(color.green());
    let b = f64::from(color.blue());
    let a = f64::from(color.alpha());

    // Build path through samples.
    cr.new_path();
    for (i, sample) in samples.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let x = (i as f64) * step_x;
        let norm = (*sample / denom).clamp(0.0, 1.0);
        let y = h - norm * h;
        if i == 0 {
            cr.move_to(x, y);
        } else {
            cr.line_to(x, y);
        }
    }
    // Stroke the line.
    cr.set_source_rgba(r, g, b, a);
    cr.set_line_width(1.5);
    let _ = cr.stroke_preserve();

    // Close path along the bottom edge for the fill.
    cr.line_to(w, h);
    cr.line_to(0.0, h);
    cr.close_path();
    cr.set_source_rgba(r, g, b, a * 0.15);
    let _ = cr.fill();
}

// These call gtk::init() / build widgets, so they need a display server —
// gated into the `system-tests` bucket rather than run by default.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::*;

    fn ensure_gtk_init() {
        static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        ONCE.get_or_init(|| {
            gtk::init().ok();
        });
    }

    #[test]
    fn push_caps_at_capacity() {
        ensure_gtk_init();
        let s = Sparkline::new(3);
        for i in 0..5 {
            s.push(f64::from(i));
        }
        let v: Vec<f64> = s.samples.borrow().iter().copied().collect();
        assert_eq!(v.len(), 3);
        assert_eq!(v, vec![2.0, 3.0, 4.0]);
    }

    #[test]
    fn clear_empties() {
        ensure_gtk_init();
        let s = Sparkline::new(10);
        s.push(1.0);
        s.push(2.0);
        s.clear();
        assert!(s.samples.borrow().is_empty());
    }

    #[test]
    fn set_domain_max_round_trips() {
        ensure_gtk_init();
        let s = Sparkline::new(5);
        s.set_domain_max(Some(2.0));
        assert_eq!(s.domain_max.get(), Some(2.0));
        s.set_domain_max(None);
        assert_eq!(s.domain_max.get(), None);
    }
}
