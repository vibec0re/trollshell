//! Shared scaffold for bar-chip indicator buttons.
//!
//! Every bar chip follows the same two-step pattern:
//!
//! 1. **Open** — create a `gtk::Button` with `"ts-indicator"` + a per-chip
//!    class, then attach the chip-specific child widget.
//! 2. **Close** — wire `connect_clicked` to
//!    [`crate::modal::toggle`]`(monitor, page, btn)`.
//!
//! [`indicator`] covers both steps; chips call it instead of repeating the
//! boilerplate inline. For the bar chips that display a small vertical fill
//! bar (cpu / memory / gpu / disk), [`vertical_bar`] builds the
//! `gtk::ProgressBar` with the standard orientation, inversion and alignment.
//! The disk chip creates its bars dynamically inside a bind closure and calls
//! [`vertical_bar`] there.

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
