use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::brightness;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-brightness");

    let icon = gtk::Image::from_icon_name("display-brightness-symbolic");
    btn.set_child(Some(&icon));

    // Hide the indicator when no backlight device exists (desktops).
    bind(
        brightness::current().map(|b| b.is_some()),
        &btn,
        gtk::prelude::WidgetExt::set_visible,
    );

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |b| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Power, b);
    });
    btn.upcast()
}
