use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::upower;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-battery");

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    bind(
        upower::battery().map(|b| b.icon_name.clone()),
        &icon,
        |w, name| {
            if name.is_empty() {
                w.set_icon_name(Some("battery-missing-symbolic"));
            } else {
                w.set_icon_name(Some(&name));
            }
        },
    );

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |_| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Power);
    });
    btn.upcast()
}
