use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-memory");

    let label = gtk::Label::new(Some("--%"));
    btn.set_child(Some(&label));

    bind_text(
        sensors::memory().map(|m| {
            if m.total == 0 {
                "--%".to_string()
            } else {
                #[allow(clippy::cast_precision_loss)]
                let pct = (m.used as f64 / m.total as f64) * 100.0;
                format!("{pct:>2.0}%")
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
