use std::cell::Cell;
use std::rc::Rc;

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::pipewire::{self, Volume};

pub fn widget() -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-volume");

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    bind(pipewire::default_sink(), &icon, |w, v| {
        w.set_icon_name(Some(icon_name(v)));
    });

    let detail = detail_widget();
    let popup = Popup::new(&btn)
        .child(detail)
        .position(PopupPosition::Bottom)
        .css_class("ts-volume-popup")
        .build();

    btn.connect_clicked(move |_| popup.toggle());
    btn.upcast()
}

fn icon_name(v: Volume) -> &'static str {
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

fn detail_widget() -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
    column.add_css_class("ts-popup-column");

    // Headline: percentage or "Muted".
    let headline = gtk::Label::new(None);
    headline.set_xalign(0.0);
    headline.add_css_class("ts-popup-headline");
    bind_text(
        pipewire::default_sink().map(|v| {
            if v.muted {
                "Muted".to_string()
            } else {
                format!("{:.0}%", v.linear * 100.0)
            }
        }),
        &headline,
    );
    column.append(&headline);

    // Mute button + slider row.
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);

    let mute_btn = gtk::Button::from_icon_name("audio-volume-muted-symbolic");
    mute_btn.add_css_class("ts-mute-btn");
    bind_class(
        pipewire::default_sink().map(|v| v.muted),
        &mute_btn,
        "active",
    );
    mute_btn.connect_clicked(|_| pipewire::toggle_mute());
    row.append(&mute_btn);

    let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
    slider.set_draw_value(false);
    slider.set_hexpand(true);
    slider.set_size_request(160, -1);

    // Suppress the slider's value-changed handler when we're updating it
    // from the signal — otherwise the bind→set_value→handler→set_volume
    // loop fights itself.
    let suppress = Rc::new(Cell::new(false));

    let suppress_for_handler = suppress.clone();
    slider.connect_value_changed(move |s| {
        if suppress_for_handler.get() {
            return;
        }
        pipewire::set_volume(s.value());
    });

    let suppress_for_bind = suppress.clone();
    bind(pipewire::default_sink(), &slider, move |s, v| {
        suppress_for_bind.set(true);
        s.set_value(v.linear);
        suppress_for_bind.set(false);
    });
    row.append(&slider);

    column.append(&row);

    let device = gtk::Label::new(Some("Default sink"));
    device.set_xalign(0.0);
    column.append(&device);

    column.upcast()
}
