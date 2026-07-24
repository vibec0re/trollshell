//! Shared scaffold for bar-chip indicator buttons.
//!
//! Every clickable bar chip follows the same two-step pattern:
//!
//! 1. **Open** — create a `gtk::Button` with `"ts-indicator"` + a per-chip
//!    class, then attach the chip-specific child widget.
//! 2. **Close** — wire `connect_clicked` to
//!    [`crate::modal::toggle`]`(monitor, page, btn)`.
//!
//! [`indicator`] covers both steps; chips call it instead of repeating the
//! boilerplate inline. [`indicator`] *always* wires a click-through to some
//! drawer `Page` — there's no click-less variant of it. A chip that's a pure
//! status light with no page to open (e.g. the screencast privacy indicator)
//! uses [`static_indicator`] instead, which shares the same CSS scaffold but
//! wires no click at all.
//!
//! For the bar chips that display a small vertical fill bar (cpu / memory /
//! gpu / disk), [`vertical_bar`] builds the `gtk::ProgressBar` with the
//! standard orientation, inversion and alignment. The disk chip creates its
//! bars dynamically inside a bind closure and calls [`vertical_bar`] there.

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

/// Build a non-interactive chip button: same `"ts-indicator"` + `class`
/// scaffold as [`indicator`], but with no `connect_clicked` wiring and no
/// `Page` to open — for chips that are pure status lights (nothing to drill
/// into today). `can_target`/`focusable` are turned off so it doesn't eat
/// pointer/keyboard focus it has no use for.
///
/// The caller is responsible for attaching a child widget (icon, label, …)
/// via `btn.set_child(…)` after this call.
pub(crate) fn static_indicator(class: &str) -> gtk::Button {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class(class);
    btn.set_can_target(false);
    btn.set_focusable(false);
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
pub(crate) fn wire_scroll<W: IsA<gtk::Widget>>(widget: &W, on_step: impl Fn(f64) + 'static) {
    let controller = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    let accumulated = std::cell::Cell::new(0.0_f64);
    controller.connect_scroll(move |_, _dx, dy| {
        let mut total = accumulated.get() + dy;
        while total >= 1.0 {
            on_step(1.0);
            total -= 1.0;
        }
        while total <= -1.0 {
            on_step(-1.0);
            total += 1.0;
        }
        accumulated.set(total);
        gtk::glib::Propagation::Stop
    });
    widget.add_controller(controller);
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
/// The caller binds the fraction via [`bind`] after this call, or (for disk)
/// calls this inside the bind closure once per mount point.
pub(crate) fn vertical_bar() -> gtk::ProgressBar {
    let bar = gtk::ProgressBar::new();
    bar.add_css_class("ts-indicator-bar");
    bar.set_orientation(gtk::Orientation::Vertical);
    bar.set_inverted(true);
    bar.set_valign(gtk::Align::Center);
    bar
}
