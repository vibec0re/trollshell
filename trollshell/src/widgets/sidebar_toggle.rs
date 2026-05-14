//! Bar chip — toggles the per-monitor left sidebar on click. Mirrors the
//! shape of `widgets::settings_chip` (button + symbolic icon + indicator
//! CSS class). Mounts as the leftmost item in `main.rs::build_bar`'s
//! `.left([…])`.

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-sidebar-toggle");

    // `view-sidebar-symbolic` is the freedesktop-standard sidebar glyph.
    // If a theme lacks it, GTK falls back to its built-in missing-image
    // icon; we don't try a multi-name fallback here to keep the chip simple.
    let icon = gtk::Image::from_icon_name("view-sidebar-symbolic");
    btn.set_child(Some(&icon));

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |_| {
        crate::overlays::sidebar::toggle(&monitor_for_click);
    });

    btn.upcast()
}
