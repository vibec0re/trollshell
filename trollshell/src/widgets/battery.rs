use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::upower::{self, BatteryState};

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-battery");

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 3);

    let icon = gtk::Image::new();
    row.append(&icon);

    let label = gtk::Label::new(None);
    label.add_css_class("ts-battery-label");
    row.append(&label);

    btn.set_child(Some(&row));

    // Icon follows UPower's icon_name.
    bind(upower::battery(), &icon, |w, b| {
        let name = if b.icon_name.is_empty() {
            "battery-missing-symbolic"
        } else {
            &b.icon_name
        };
        w.set_icon_name(Some(name));
    });

    // Percentage label.
    bind_text(
        upower::battery().map(|b| format!("{:.0}%", b.percentage)),
        &label,
    );

    // Hide entirely when no battery is present (desktop systems). UPower
    // reports state = Unknown (discriminant 0) on such systems.
    bind_visible(
        upower::battery().map(|b| b.state != BatteryState::Unknown),
        &btn,
    );

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |_| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Power);
    });
    btn.upcast()
}
