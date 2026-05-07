use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::clock;

/// Clock chip — date + time. Click toggles the per-monitor Calendar drawer
/// page so the user can browse upcoming events.
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-clock");

    let label = gtk::Label::new(None);
    bind_text(
        clock::now().map(|t| t.format("%a %d %b  %H:%M").to_string()),
        &label,
    );
    btn.set_child(Some(&label));

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |b| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Calendar, b);
    });

    btn.upcast()
}
