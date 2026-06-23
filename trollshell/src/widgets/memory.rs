use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors;

use crate::components::cast;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = crate::components::chip::indicator("ts-memory", crate::modal::Page::Stats, monitor);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 3);

    let icon = gtk::Image::from_file(crate::assets::path("icons/memory.svg"));
    icon.set_pixel_size(crate::scale::scale(16));
    row.append(&icon);

    let bar = crate::components::chip::vertical_bar();
    row.append(&bar);

    btn.set_child(Some(&row));

    bind(sensors::memory(), &bar, |w, m| {
        if m.total == 0 {
            w.set_fraction(0.0);
            w.set_tooltip_text(Some("Memory: unknown"));
        } else {
            let frac = (cast::u64_to_f64(m.used) / cast::u64_to_f64(m.total)).clamp(0.0, 1.0);
            w.set_fraction(frac);
            w.set_tooltip_text(Some(&format!("Memory {:.0}%", frac * 100.0)));
        }
    });

    btn.upcast()
}
