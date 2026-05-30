use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-gpu");

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 3);

    let icon = gtk::Image::from_file(crate::assets::path("icons/gpu.svg"));
    icon.set_pixel_size(16);
    row.append(&icon);

    let temp_label = gtk::Label::new(None);
    temp_label.add_css_class("ts-gpu-temp");
    row.append(&temp_label);

    let bar = gtk::ProgressBar::new();
    bar.add_css_class("ts-indicator-bar");
    bar.set_orientation(gtk::Orientation::Vertical);
    bar.set_inverted(true);
    bar.set_valign(gtk::Align::Center);
    row.append(&bar);

    btn.set_child(Some(&row));

    bind(sensors::gpu(), &bar, |w, g| {
        let load = g.as_ref().and_then(|s| s.load).unwrap_or(0.0);
        w.set_fraction(load.clamp(0.0, 1.0));
        let tip = match g {
            Some(state) => match state.load {
                Some(l) => format!("{}: {:.0}%", state.name, l * 100.0),
                None => state.name.clone(),
            },
            None => "GPU".to_string(),
        };
        w.set_tooltip_text(Some(&tip));
    });

    bind(sensors::gpu(), &temp_label, |w, g| {
        match g.as_ref().and_then(|s| s.temperature_celsius) {
            Some(c) => w.set_label(&format!("{c:.0}°")),
            None => w.set_label(""),
        }
    });

    // Hide the widget when no GPU is detected.
    bind_visible(sensors::gpu().map(|g| g.is_some()), &btn);

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |b| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Stats, b);
    });
    btn.upcast()
}
