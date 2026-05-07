use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::notifications;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-notif-indicator");

    let overlay = gtk::Overlay::new();
    let icon = gtk::Image::from_icon_name("notification-symbolic");
    overlay.set_child(Some(&icon));

    let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    dot.add_css_class("ts-notif-dot");
    dot.set_halign(gtk::Align::End);
    dot.set_valign(gtk::Align::Start);
    overlay.add_overlay(&dot);
    btn.set_child(Some(&overlay));

    bind(notifications::active(), &dot, |w, list| {
        let n = list.len();
        w.set_visible(n > 0);
        if n > 0 {
            w.set_tooltip_text(Some(&n.to_string()));
        } else {
            w.set_tooltip_text(None);
        }
    });

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |b| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Notifications, b);
    });

    btn.upcast()
}
