//! Sidebar calendar widget: month grid + 7-day upcoming events list.
//! Sibling to the drawer panel `trollshell/src/panels/calendar.rs` —
//! same data source (`hytte::services::calendar::events`), parallel
//! rendering for the always-on left sidebar surface. See
//! `docs/superpowers/specs/2026-05-14-calendar-widget-design.md`.

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;

/// Build the sidebar calendar widget. Caller appends the returned widget
/// to `.ts-sidebar`; the widget owns its own subscriptions to
/// `calendar::events()` and `sidebar::open_signal(monitor)`.
pub fn widget(_monitor: &Monitor) -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.add_css_class("ts-sidebar-calendar");
    column.upcast()
}
