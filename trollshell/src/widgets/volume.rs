use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::pipewire::{self, Volume};

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = crate::components::chip::indicator("ts-volume", crate::modal::Page::Audio, monitor);

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    bind(pipewire::default_sink(), &icon, |w, v: Volume| {
        w.set_icon_name(Some(icon_name(&v)));
    });

    btn.upcast()
}

fn icon_name(v: &Volume) -> &'static str {
    if v.muted {
        "audio-volume-muted-symbolic"
    } else if v.linear < 0.34 {
        "audio-volume-low-symbolic"
    } else if v.linear < 0.67 {
        "audio-volume-medium-symbolic"
    } else {
        "audio-volume-high-symbolic"
    }
}
