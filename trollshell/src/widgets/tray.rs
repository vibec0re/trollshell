use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::tray::{self, TrayItem};

pub fn widget() -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    container.add_css_class("ts-tray");

    let container_for_signal = container.clone();
    bind(tray::items(), &container, move |_, items| {
        while let Some(child) = container_for_signal.first_child() {
            container_for_signal.remove(&child);
        }
        for item in items {
            let btn = build_item_button(&item);
            container_for_signal.append(&btn);
        }
    });

    container.upcast()
}

fn build_item_button(item: &TrayItem) -> gtk::Button {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-tray-item");

    let icon = gtk::Image::new();
    icon.set_icon_name(if item.icon_name.is_empty() {
        Some("application-x-executable-symbolic")
    } else {
        Some(&item.icon_name)
    });
    btn.set_child(Some(&icon));

    if !item.title.is_empty() {
        btn.set_tooltip_text(Some(&item.title));
    }

    let bus = item.bus_name.clone();
    let path = item.object_path.clone();
    btn.connect_clicked(move |_| tray::activate(&bus, &path));

    btn
}
