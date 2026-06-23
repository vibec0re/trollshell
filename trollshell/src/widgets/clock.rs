use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::clock;

/// Clock chip — date + time. Click toggles the per-monitor Calendar drawer
/// page so the user can browse upcoming events.
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = crate::components::chip::indicator("ts-clock", crate::modal::Page::Calendar, monitor);

    let label = gtk::Label::new(None);
    bind_text(
        clock::now().map(|t| t.format("%a %d %b  %H:%M").to_string()),
        &label,
    );
    btn.set_child(Some(&label));

    btn.upcast()
}
