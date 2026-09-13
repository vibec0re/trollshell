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

/// The button + row + elapsed-time label, with no click or reactive binding
/// wired in yet — those need `recorder::state()`, which `.expect()`s a
/// registered `Registry` (#831). Split out so #1177's CSS-class snapshot test
/// can build this much without one, the same split `disk.rs`'s
/// `bind_disk_mounts` makes for the same reason.
fn build_button() -> (gtk::Button, gtk::Label) {
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

    (btn, time)
}

pub fn widget(_monitor: &Monitor) -> gtk::Widget {
    let (btn, time) = build_button();

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

/// #1177: pins this chip's CSS class set as a snapshot, taken **before** the
/// hand-rolled scaffold in [`build_button`] is replaced with
/// `components::chip::action_indicator` — the two `add_css_class` calls there
/// must survive that refactor unchanged. Falsified by adding/removing/renaming
/// either class.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use hytte::gtk::{self, prelude::*};

    use super::build_button;

    #[gtk::test]
    fn css_classes_are_ts_indicator_and_ts_recording() {
        let (btn, _time) = build_button();
        assert!(btn.has_css_class("ts-indicator"));
        assert!(btn.has_css_class("ts-recording"));
    }
}
