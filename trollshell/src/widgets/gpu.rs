use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-gpu");

    let label = gtk::Label::new(Some("--%"));
    btn.set_child(Some(&label));

    // Bind label text and visibility to the gpu signal.
    bind_text(
        sensors::gpu().map(|g| match &g {
            Some(state) => match state.load {
                Some(load) => format!("{:>2.0}%", load * 100.0),
                None => "--%".to_string(),
            },
            None => "--%".to_string(),
        }),
        &label,
    );

    // Hide the widget when no GPU is detected.
    bind_visible(sensors::gpu().map(|g| g.is_some()), &btn);

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |_| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Stats);
    });
    btn.upcast()
}
