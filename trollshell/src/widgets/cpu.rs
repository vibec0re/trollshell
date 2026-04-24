use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-cpu");

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);

    let icon = gtk::Image::from_file(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/cpu.svg"));
    icon.set_pixel_size(16);
    row.append(&icon);

    let bar = gtk::ProgressBar::new();
    bar.add_css_class("ts-indicator-bar");
    bar.set_size_request(42, -1);
    bar.set_valign(gtk::Align::Center);
    row.append(&bar);

    btn.set_child(Some(&row));

    bind(sensors::cpu(), &bar, |w, c| {
        w.set_fraction(c.overall.clamp(0.0, 1.0));
        w.set_tooltip_text(Some(&format!("CPU {:.0}%", c.overall * 100.0)));
    });

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |_| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Stats);
    });
    btn.upcast()
}
