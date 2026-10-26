//! Shared scaffold for bar-chip indicator buttons.
//!
//! Every clickable bar chip follows the same two-step pattern:
//!
//! 1. **Open** — create a `gtk::Button` with `"ts-indicator"` + a per-chip
//!    class, then attach the chip-specific child widget.
//! 2. **Close** — wire `connect_clicked` to
//!    [`crate::modal::toggle`]`(monitor, page, btn)`.
//!
//! `indicator` covers both steps; chips call it instead of repeating the
//! boilerplate inline. `indicator` *always* wires a click-through to some
//! drawer `Page`, so `action_indicator` covers the chip whose click is a
//! one-shot command with no page to open — same CSS scaffold, but
//! `connect_clicked` runs a caller-supplied closure (#578: the screencast
//! privacy chip stopping the cast).
//!
//! There was a third shape, `static_indicator`, wiring no click at all for a
//! pure status light. The screencast chip was its only caller and #578 gave
//! that chip a click, so it was removed rather than left dead — resurrect it
//! from git history if a genuinely inert chip ever turns up.
//!
//! For the bar chips that display a small vertical fill bar (cpu / memory /
//! gpu / disk), `vertical_bar` builds the `gtk::ProgressBar` with the
//! standard orientation, inversion and alignment. The disk chip creates its
//! bars dynamically inside a bind closure and calls `vertical_bar` there.

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;

use crate::modal::Page;

/// Build the standard chip button: `"ts-indicator"` + `class`, with
/// `connect_clicked` wired to [`crate::modal::toggle`] for `page` on
/// `monitor`.
///
/// The caller is responsible for attaching a child widget (icon, label, …)
/// via `btn.set_child(…)` after this call.
pub(crate) fn indicator(class: &str, page: Page, monitor: &Monitor) -> gtk::Button {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class(class);

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |b| {
        crate::modal::toggle(&monitor_for_click, page, b);
    });

    btn
}

/// Like [`indicator`], but wired to [`crate::modal::toggle_keep_open`]
/// instead of [`crate::modal::toggle`], and runs `on_click` first.
///
/// Used by chips that route into a page shared with other triggers (#516:
/// the five combined-Stats resource chips point at the same `Page::Stats`).
/// `toggle_keep_open`'s "don't retract an already-open shared page" behavior
/// means clicking a *different* resource chip while the Stats drawer is open
/// jumps within it instead of closing it; `on_click` is where each chip
/// stashes which card to land on (e.g. `panels::stats::set_scroll_target`)
/// before the toggle call runs.
pub(crate) fn indicator_scroll(
    class: &str,
    page: Page,
    monitor: &Monitor,
    on_click: impl Fn() + 'static,
) -> gtk::Button {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class(class);

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |b| {
        on_click();
        crate::modal::toggle_keep_open(&monitor_for_click, page, b);
    });

    btn
}

/// Build a chip button that acts on click without opening a drawer `Page`:
/// same `"ts-indicator"` + `class` scaffold as [`indicator`], but
/// `connect_clicked` runs `on_click` instead of [`crate::modal::toggle`].
///
/// For chips whose click *is* the whole interaction — a one-shot command
/// with no detail view to drill into (#578: the screencast privacy chip
/// stopping the cast). Targetable and focusable like [`indicator`], so the
/// click is reachable by pointer *and* keyboard.
///
/// The caller is responsible for attaching a child widget (icon, label, …)
/// via `btn.set_child(…)` after this call.
pub(crate) fn action_indicator(class: &str, on_click: impl Fn() + 'static) -> gtk::Button {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class(class);
    btn.connect_clicked(move |_| on_click());
    btn
}

/// Attach a vertical-scroll controller to `widget` that accumulates
/// smooth-scroll deltas into whole "notches" before firing `on_step`.
///
/// Trackpads and high-resolution mice report `scroll` events as fractional
/// pixel deltas (libinput smooth scrolling) rather than a wheel's integral
/// clicks, so a naive `dy.round()` per event either fires on every
/// sub-pixel tick or misses slow scrolls entirely. Accumulating the raw
/// deltas in a `Cell<f64>` and only firing once the running total crosses a
/// whole unit reproduces the one-notch-per-click feel of a physical wheel
/// for both input types, and lets a fast flick fire `on_step` more than
/// once per event.
///
/// `on_step` is called with `1.0` for a scroll down and `-1.0` for a scroll
/// up (matching raw GDK `dy` sign — down is positive); callers map that to
/// "increase"/"decrease" as appropriate for the chip.
///
/// The unspent fraction is **dropped** when the direction flips and after
/// [`SCROLL_IDLE_RESET`] of quiet ([`Notches`]), so it only ever carries
/// within one gesture.
pub(crate) fn wire_scroll<W: IsA<gtk::Widget>>(widget: &W, on_step: impl Fn(f64) + 'static) {
    let controller = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    let notches = std::cell::Cell::new(Notches::default());
    controller.connect_scroll(move |_, _dx, dy| {
        let mut acc = notches.get();
        let steps = acc.feed(dy, std::time::Instant::now());
        notches.set(acc);
        // Fire after the state is stored, so an `on_step` that re-enters this
        // handler (a synchronous emission) reads the settled remainder.
        for _ in 0..steps.unsigned_abs() {
            on_step(if steps > 0 { 1.0 } else { -1.0 });
        }
        gtk::glib::Propagation::Stop
    });
    widget.add_controller(controller);
}

/// How long a scroll may pause before its unspent fraction of a notch is
/// dropped.
///
/// Within one gesture the events come close together — a wheel spun by hand
/// reports its detents tens of milliseconds apart, a touchpad a frame every
/// ~10 ms — so half a second of nothing is the gesture over. Without the
/// reset, a sub-notch remainder survived indefinitely, and the first tiny
/// nudge of the next scroll, minutes later, completed a notch nobody asked
/// for.
const SCROLL_IDLE_RESET: std::time::Duration = std::time::Duration::from_millis(500);

/// [`wire_scroll`]'s accumulator: smooth-scroll deltas summed into whole
/// notches, the remainder carried only within one gesture.
///
/// Pure, so the arithmetic is tested without a scroll controller. Two resets
/// on top of the plain sum it replaces:
///
/// - **Direction flip.** A remainder left by scrolling one way is not credit
///   toward the other: `+0.9` then a full `-1.0` notch used to net `-0.1` and
///   fire nothing, so the first notch back was lost.
/// - **Idle** ([`SCROLL_IDLE_RESET`]).
///
/// An integral wheel (`dy = ±1.0` per detent) leaves no remainder, so neither
/// reset changes anything there — the brightness and volume chips' per-notch
/// behaviour (#1495) is untouched; both only ever see the `±1.0` steps this
/// emits.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Notches {
    /// The fraction of a notch carried so far, in `(-1.0, 1.0)`.
    remainder: f64,
    /// When the last delta arrived; `None` before the first.
    last: Option<std::time::Instant>,
}

impl Notches {
    /// Feed one event's `dy` at `now`; the signed number of whole notches it
    /// completes (positive = down).
    ///
    /// A non-finite `dy` is ignored rather than summed: one `NaN` would
    /// otherwise poison the remainder, and every comparison against it is
    /// false, so the chip would stop scrolling for good.
    fn feed(&mut self, dy: f64, now: std::time::Instant) -> i32 {
        if !dy.is_finite() {
            return 0;
        }
        let idle = self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) >= SCROLL_IDLE_RESET);
        let flipped = self.remainder * dy < 0.0;
        if idle || flipped {
            self.remainder = 0.0;
        }
        self.last = Some(now);
        let mut total = self.remainder + dy;
        let mut steps = 0_i32;
        while total >= 1.0 {
            steps = steps.saturating_add(1);
            total -= 1.0;
        }
        while total <= -1.0 {
            steps = steps.saturating_sub(1);
            total += 1.0;
        }
        self.remainder = total;
        steps
    }
}

/// Build the standard vertical fill bar used in the cpu / memory / gpu / disk
/// chips.
///
/// Returns a `gtk::ProgressBar` with:
/// - CSS class `"ts-indicator-bar"`
/// - `Orientation::Vertical`
/// - `inverted = true` (fills from the bottom)
/// - `valign = Align::Center`
///
/// The caller binds the fraction via [`bind()`] after this call, or (for disk)
/// calls this inside the bind closure once per mount point.
pub(crate) fn vertical_bar() -> gtk::ProgressBar {
    let bar = gtk::ProgressBar::new();
    bar.add_css_class("ts-indicator-bar");
    bar.set_orientation(gtk::Orientation::Vertical);
    bar.set_inverted(true);
    bar.set_valign(gtk::Align::Center);
    bar
}

#[cfg(test)]
mod notch_tests {
    use super::{Notches, SCROLL_IDLE_RESET};
    use std::time::{Duration, Instant};

    /// The plain sum still works: fractions within one gesture add up, the
    /// remainder carries into the next event, and a big delta fires more than
    /// once.
    #[test]
    fn fractions_add_up_within_a_gesture() {
        let t = Instant::now();
        let mut n = Notches::default();
        assert_eq!(n.feed(0.4, t), 0);
        assert_eq!(n.feed(0.4, t + Duration::from_millis(10)), 0);
        assert_eq!(n.feed(0.4, t + Duration::from_millis(20)), 1);
        // 0.2 carried + 2.8 = three whole notches.
        assert_eq!(n.feed(2.8, t + Duration::from_millis(30)), 3);
        // Fresh, so no remainder (and no flip reset) is involved.
        assert_eq!(Notches::default().feed(-3.0, t), -3);
    }

    /// An integral wheel never carries anything, so the resets cannot change
    /// it: one detent, one step, whatever the gap.
    #[test]
    fn an_integral_wheel_is_unchanged() {
        let t = Instant::now();
        let mut n = Notches::default();
        assert_eq!(n.feed(1.0, t), 1);
        assert_eq!(n.feed(-1.0, t + Duration::from_millis(5)), -1);
        assert_eq!(n.feed(1.0, t + Duration::from_mins(1)), 1);
    }

    /// `+0.9` then a full notch the other way fires that notch. It used to net
    /// `-0.1` and fire nothing.
    ///
    /// **Falsification:** drop the `flipped` reset → this reds.
    #[test]
    fn a_direction_flip_drops_the_remainder() {
        let t = Instant::now();
        let mut n = Notches::default();
        assert_eq!(n.feed(0.9, t), 0);
        assert_eq!(n.feed(-1.0, t + Duration::from_millis(10)), -1);
    }

    /// A remainder left from an earlier gesture is gone after the idle reset:
    /// a small nudge later completes no notch.
    ///
    /// **Falsification:** drop the `idle` reset → this reds.
    #[test]
    fn an_idle_pause_drops_the_remainder() {
        let t = Instant::now();
        let mut n = Notches::default();
        assert_eq!(n.feed(0.9, t), 0);
        assert_eq!(n.feed(0.2, t + SCROLL_IDLE_RESET), 0);
        // …but inside the window it still carries.
        let mut m = Notches::default();
        assert_eq!(m.feed(0.9, t), 0);
        assert_eq!(
            m.feed(
                0.2,
                t + SCROLL_IDLE_RESET.saturating_sub(Duration::from_millis(1))
            ),
            1
        );
    }

    /// A non-finite delta is ignored, and does not poison what follows.
    #[test]
    fn a_non_finite_delta_is_ignored() {
        let t = Instant::now();
        let mut n = Notches::default();
        assert_eq!(n.feed(0.6, t), 0);
        assert_eq!(n.feed(f64::NAN, t), 0);
        assert_eq!(n.feed(f64::INFINITY, t), 0);
        assert_eq!(n.feed(0.6, t + Duration::from_millis(10)), 1);
    }
}
