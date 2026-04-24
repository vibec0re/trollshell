use futures_signals::signal::SignalExt;
use gtk::prelude::*;
use hytte::prelude::*;
use hytte::services::clock;

pub fn widget() -> gtk::Widget {
    let label = gtk::Label::new(None);
    label.add_css_class("trollshell-clock");
    bind_text(
        clock::now().map(|t| t.format("%a %H:%M").to_string()),
        &label,
    );
    label.upcast()
}
