use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-disk");

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 3);

    let icon = gtk::Image::from_file(concat!(env!("CARGO_MANIFEST_DIR"), "/icons/disk.svg"));
    icon.set_pixel_size(16);
    row.append(&icon);

    // One tiny vertical bar per mount.
    let mounts_container = gtk::Box::new(gtk::Orientation::Horizontal, 2);
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
            bar.set_orientation(gtk::Orientation::Vertical);
            bar.set_inverted(true);
            bar.set_valign(gtk::Align::Center);
            bar.set_fraction(m.usage.clamp(0.0, 1.0));
            bar.set_tooltip_text(Some(&format!("{}: {:.0}%", m.path, m.usage * 100.0)));
            mounts_for_signal.append(&bar);
        }
    });

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |b| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Stats, b);
    });
    btn.upcast()
}
