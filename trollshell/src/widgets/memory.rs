use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-memory");

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 3);

    let icon = gtk::Image::from_file(crate::assets::path("icons/memory.svg"));
    icon.set_pixel_size(16);
    row.append(&icon);

    let bar = gtk::ProgressBar::new();
    bar.add_css_class("ts-indicator-bar");
    bar.set_orientation(gtk::Orientation::Vertical);
    bar.set_inverted(true);
    bar.set_valign(gtk::Align::Center);
    row.append(&bar);

    btn.set_child(Some(&row));

    bind(sensors::memory(), &bar, |w, m| {
        if m.total == 0 {
            w.set_fraction(0.0);
            w.set_tooltip_text(Some("Memory: unknown"));
        } else {
            #[allow(clippy::cast_precision_loss)]
            let frac = (m.used as f64 / m.total as f64).clamp(0.0, 1.0);
            w.set_fraction(frac);
            w.set_tooltip_text(Some(&format!("Memory {:.0}%", frac * 100.0)));
        }
    });

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |b| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Stats, b);
    });
    btn.upcast()
}
