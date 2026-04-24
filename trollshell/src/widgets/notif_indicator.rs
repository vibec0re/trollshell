use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::notifications;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-notif-indicator");

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let icon = gtk::Image::from_icon_name("notification-symbolic");
    row.append(&icon);
    let badge = gtk::Label::new(None);
    badge.add_css_class("ts-notif-badge");
    row.append(&badge);
    btn.set_child(Some(&row));

    bind(notifications::active(), &badge, |w, list| {
        let n = list.len();
        if n == 0 {
            w.set_text("");
            w.set_visible(false);
        } else {
            w.set_text(&n.to_string());
            w.set_visible(true);
        }
    });

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |_| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Notifications);
    });

    btn.upcast()
}
