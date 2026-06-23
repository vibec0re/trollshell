use hytte::futures_signals::map_ref;
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::pipewire::{self, Source};

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn =
        crate::components::chip::indicator("ts-microphone", crate::modal::Page::Audio, monitor);

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    // Show the mic as "off" unless an app is actually recording. When
    // something records, reflect the default source's mute / volume state.
    let combined = map_ref! {
        let sources = pipewire::sources(),
        let records = pipewire::record_streams() => {
            let default = sources.iter().find(|s| s.is_default).cloned();
            let recording = !records.is_empty();
            (default, recording)
        }
    };

    let btn_for_bind = btn.clone();
    bind(
        combined,
        &icon,
        move |w, (default, recording): (Option<Source>, bool)| {
            let (name, recording) = icon_state(default.as_ref(), recording);
            w.set_icon_name(Some(name));
            if recording {
                btn_for_bind.add_css_class("ts-microphone-recording");
            } else {
                btn_for_bind.remove_css_class("ts-microphone-recording");
            }
        },
    );

    btn.upcast()
}

/// Returns (icon name, whether an app is actively recording).
fn icon_state(source: Option<&Source>, recording: bool) -> (&'static str, bool) {
    match source {
        None => ("audio-input-microphone-symbolic", false),
        Some(s) if s.muted => ("microphone-sensitivity-muted-symbolic", false),
        _ if !recording => ("audio-input-microphone-symbolic", false),
        Some(s) if s.volume < 0.34 => ("microphone-sensitivity-low-symbolic", true),
        Some(s) if s.volume < 0.67 => ("microphone-sensitivity-medium-symbolic", true),
        Some(_) => ("microphone-sensitivity-high-symbolic", true),
    }
}
