use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;

/// Bar chip → opens the trollshell Settings drawer page.
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-settings");

    let icon = gtk::Image::from_file(crate::assets::path("icons/emblem-system.svg"));
    icon.set_pixel_size(crate::scale::scale(16));
    btn.set_child(Some(&icon));

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |b| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Settings, b);
    });

    btn.upcast()
}
