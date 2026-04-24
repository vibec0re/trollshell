use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::clock;

pub fn widget() -> gtk::Widget {
    let label = gtk::Label::new(None);
    label.add_css_class("ts-clock");
    bind_text(
        clock::now().map(|t| t.format("%a %d %b  %H:%M").to_string()),
        &label,
    );
    label.upcast()
}
