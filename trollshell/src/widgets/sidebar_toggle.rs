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

    // Bundled Material icon: modern Adwaita dropped `view-sidebar-symbolic`
    // (it lives at `sidebar-show-symbolic` now, but Material's view_sidebar
    // matches the rest of the bar's icon style — see icons/cpu.svg etc.).
    let icon = gtk::Image::from_file(crate::assets::path("icons/view-sidebar.svg"));
    icon.set_pixel_size(16);
    btn.set_child(Some(&icon));

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |_| {
        crate::overlays::sidebar::toggle(&monitor_for_click);
    });

    btn.upcast()
}
