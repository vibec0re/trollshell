use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-disk");

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);

    let icon = gtk::Image::from_file(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/disk.svg"));
    icon.set_pixel_size(16);
    row.append(&icon);

    // One narrow ProgressBar per mount, rebuilt on each emission so the
    // set of bars follows the mounts list (hot-add / hot-remove safe).
    let mounts_container = gtk::Box::new(gtk::Orientation::Horizontal, 3);
    mounts_container.add_css_class("ts-disk-mounts");
    mounts_container.set_valign(gtk::Align::Center);
    row.append(&mounts_container);

    btn.set_child(Some(&row));

    let mounts_for_signal = mounts_container.clone();
    bind(sensors::disk(), &mounts_container, move |_, disk| {
        while let Some(c) = mounts_for_signal.first_child() {
            mounts_for_signal.remove(&c);
        }
        for m in &disk.mounts {
            let bar = gtk::ProgressBar::new();
            bar.add_css_class("ts-indicator-bar");
            bar.set_size_request(24, -1);
            bar.set_valign(gtk::Align::Center);
            bar.set_fraction(m.usage.clamp(0.0, 1.0));
            bar.set_tooltip_text(Some(&format!("{}: {:.0}%", m.path, m.usage * 100.0)));
            mounts_for_signal.append(&bar);
        }
    });

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |_| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Stats);
    });
    btn.upcast()
}
