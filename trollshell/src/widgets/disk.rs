use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-disk");

    let label = gtk::Label::new(Some("--%"));
    btn.set_child(Some(&label));

    // Show the most-full mount's usage percentage.
    bind_text(
        sensors::disk().map(|d| {
            let max = d
                .mounts
                .iter()
                .map(|m| m.usage)
                .fold(f64::NEG_INFINITY, f64::max);
            if max.is_finite() {
                format!("{:>2.0}%", max * 100.0)
            } else {
                "--%".to_string()
            }
        }),
        &label,
    );

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |_| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Stats);
    });
    btn.upcast()
}
