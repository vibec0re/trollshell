use std::cell::Cell;
use std::rc::Rc;

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::brightness;

use crate::components::chip::wire_scroll;

/// Brightness change per scroll notch.
const BRIGHTNESS_STEP: f64 = 0.05;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn =
        crate::components::chip::indicator("ts-brightness", crate::modal::Page::Power, monitor);

    let icon = gtk::Image::from_icon_name("display-brightness-symbolic");
    btn.set_child(Some(&icon));

    // Hide the indicator when no backlight device exists (desktops).
    bind(
        brightness::current().map(|b| b.is_some()),
        &btn,
        gtk::prelude::WidgetExt::set_visible,
    );

    // Last-bound level, kept around so the scroll handler can compute
    // `current ± step` — the visibility bind above maps the level away, so
    // this is a second, dedicated subscription.
    let current = Rc::new(Cell::new(0.0_f64));
    let current_for_bind = Rc::clone(&current);
    bind(brightness::current(), &btn, move |_, b| {
        if let Some(b) = b {
            current_for_bind.set(b.level);
        }
    });

    wire_scroll(&btn, move |direction| {
        // GDK `dy` is positive for scroll-down; treat that as "decrease" so
        // scroll-up raises brightness, matching the volume chip.
        let next = current.get() - direction * BRIGHTNESS_STEP;
        brightness::set(next);
    });

    btn.upcast()
}
