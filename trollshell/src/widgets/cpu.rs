use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-cpu");

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 3);

    let icon = gtk::Image::from_file(crate::assets::path("icons/cpu.svg"));
    icon.set_pixel_size(16);
    row.append(&icon);

    let temp_label = gtk::Label::new(None);
    temp_label.add_css_class("ts-cpu-temp");
    row.append(&temp_label);

    let bar = gtk::ProgressBar::new();
    bar.add_css_class("ts-indicator-bar");
    bar.set_orientation(gtk::Orientation::Vertical);
    bar.set_inverted(true); // fill from the bottom
    bar.set_valign(gtk::Align::Center);
    row.append(&bar);

    btn.set_child(Some(&row));

    bind(sensors::cpu(), &bar, |w, c| {
        w.set_fraction(c.overall.clamp(0.0, 1.0));
        w.set_tooltip_text(Some(&format!("CPU {:.0}%", c.overall * 100.0)));
    });

    bind(sensors::cpu_temp(), &temp_label, |w, t| match t.package_celsius {
        Some(c) => w.set_label(&format!("{c:.0}°")),
        None => w.set_label(""),
    });

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |b| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Stats, b);
    });
    btn.upcast()
}
