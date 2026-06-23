use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::notifications;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = crate::components::chip::indicator(
        "ts-notif-indicator",
        crate::modal::Page::Notifications,
        monitor,
    );

    let overlay = gtk::Overlay::new();
    let icon = gtk::Image::from_file(crate::assets::path("icons/notification.svg"));
    icon.set_pixel_size(crate::scale::scale(16));
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

    btn.upcast()
}
