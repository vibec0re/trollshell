use std::cell::Cell;
use std::rc::Rc;

use hytte::gtk::{self, gdk, prelude::*};
use hytte::prelude::*;
use hytte::services::pipewire::{self, Volume};

use crate::components::chip::wire_scroll;

/// Volume change per scroll notch.
const VOLUME_STEP: f64 = 0.05;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = crate::components::chip::indicator("ts-volume", crate::modal::Page::Audio, monitor);

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    // Last-bound linear volume, kept around so the scroll handler can
    // compute `current ± step` without a synchronous read-back of the
    // (async, cross-thread) service state.
    let current = Rc::new(Cell::new(0.0_f64));

    let current_for_bind = Rc::clone(&current);
    bind(pipewire::default_sink(), &icon, move |w, v: Volume| {
        current_for_bind.set(v.linear);
        w.set_icon_name(Some(icon_name(&v)));
    });

    wire_scroll(&btn, move |direction| {
        // GDK `dy` is positive for scroll-down; treat that as "decrease" so
        // scroll-up raises the volume, matching most volume sliders.
        let next = (current.get() - direction * VOLUME_STEP).max(0.0);
        pipewire::set_volume(next);
    });

    let mute_gesture = gtk::GestureClick::new();
    mute_gesture.set_button(gdk::BUTTON_MIDDLE);
    mute_gesture.connect_pressed(|gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        pipewire::toggle_mute();
    });
    btn.add_controller(mute_gesture);

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
