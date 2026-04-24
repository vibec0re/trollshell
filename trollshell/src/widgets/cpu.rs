use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-cpu");

    let label = gtk::Label::new(Some("--%"));
    btn.set_child(Some(&label));

    bind_text(
        sensors::cpu().map(|c| format!("{:>2.0}%", c.overall * 100.0)),
        &label,
    );

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |_| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Stats);
    });
    btn.upcast()
}
