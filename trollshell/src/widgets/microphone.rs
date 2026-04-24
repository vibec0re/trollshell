use hytte::futures_signals::map_ref;
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::pipewire::{self, Source};

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-microphone");

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
            let (name, off) = icon_state(default.as_ref(), recording);
            w.set_icon_name(Some(name));
            if off {
                btn_for_bind.add_css_class("ts-microphone-off");
            } else {
                btn_for_bind.remove_css_class("ts-microphone-off");
            }
        },
    );

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |_| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Audio);
    });
    btn.upcast()
}

/// Returns (icon name, whether to apply the dimmed-off style).
fn icon_state(source: Option<&Source>, recording: bool) -> (&'static str, bool) {
    match source {
        None => ("audio-input-microphone-symbolic", true),
        Some(s) if s.muted => ("microphone-sensitivity-muted-symbolic", false),
        _ if !recording => ("audio-input-microphone-symbolic", true),
        Some(s) if s.volume < 0.34 => ("microphone-sensitivity-low-symbolic", false),
        Some(s) if s.volume < 0.67 => ("microphone-sensitivity-medium-symbolic", false),
        Some(_) => ("microphone-sensitivity-high-symbolic", false),
    }
}

