//! Screen-recording bar chip — a record toggle that lives next to the
//! screenshot chip and doubles as the "you're recording" indicator (#403).
//!
//! One always-visible chip rather than a separate start button + a
//! hidden-when-idle indicator: idle it's a neutral record dot (click starts a
//! recording, region picked via `slurp`); while recording it reads as an
//! unmissable red "REC" — pulsing red dot + a live elapsed timer — and a click
//! stops it (the `.ts-recording-live` class, added while recording, carries the
//! red styling, the visual family of the #221 cast indicator).
//!
//! Recording is its **own** state (`recorder::state()`), distinct from casting.
//! The keybind entry point lives on the #219 command surface as
//! `toggle-recording` (see `commands.rs`).

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::recorder;

pub fn widget(_monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-recording");

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let dot = gtk::Image::from_icon_name("media-record-symbolic");
    let time = gtk::Label::new(None);
    time.add_css_class("ts-recording-time");
    row.append(&dot);
    row.append(&time);
    btn.set_child(Some(&row));

    btn.connect_clicked(|_| recorder::toggle());

    // Red styling + elapsed timer only while recording.
    bind_class(
        recorder::state().map(|s| s.is_recording()),
        &btn,
        "ts-recording-live",
    );
    bind_text(
        recorder::state().map(|s| s.label().unwrap_or_default()),
        &time,
    );
    bind(recorder::state(), &btn, |b, s| {
        b.set_tooltip_text(Some(if s.is_recording() {
            "Recording — click to stop"
        } else {
            "Start screen recording"
        }));
    });

    btn.upcast()
}
