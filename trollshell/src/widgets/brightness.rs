use std::cell::Cell;
use std::rc::Rc;

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::brightness;

pub fn widget() -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-brightness");

    let icon = gtk::Image::from_icon_name("display-brightness-symbolic");
    btn.set_child(Some(&icon));

    // Hide the indicator when no backlight device exists (desktops).
    bind(
        brightness::current().map(|b| b.is_some()),
        &btn,
        gtk::prelude::WidgetExt::set_visible,
    );

    let detail = detail_widget();
    let popup = Popup::new(&btn)
        .child(detail)
        .position(PopupPosition::Bottom)
        .css_class("ts-brightness-popup")
        .build();

    btn.connect_clicked(move |_| popup.toggle());
    btn.upcast()
}

fn detail_widget() -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
    column.add_css_class("ts-popup-column");

    let headline = gtk::Label::new(None);
    headline.set_xalign(0.0);
    headline.add_css_class("ts-popup-headline");
    bind_text(
        brightness::current().map(|b| match b {
            Some(b) => format!("{:.0}%", b.level * 100.0),
            None => "Brightness —".to_string(),
        }),
        &headline,
    );
    column.append(&headline);

    let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.05, 1.0, 0.05);
    slider.set_draw_value(false);
    slider.set_hexpand(true);
    slider.set_size_request(200, -1);

    // Break the bind → set_value → value-changed → set loop, same pattern
    // as the volume widget.
    let suppress = Rc::new(Cell::new(false));

    let suppress_for_handler = suppress.clone();
    slider.connect_value_changed(move |s| {
        if suppress_for_handler.get() {
            return;
        }
        brightness::set(s.value());
    });

    let suppress_for_bind = suppress.clone();
    bind(brightness::current(), &slider, move |s, b| {
        if let Some(b) = b {
            suppress_for_bind.set(true);
            s.set_value(b.level);
            suppress_for_bind.set(false);
        }
    });

    column.append(&slider);

    let device = gtk::Label::new(Some("Backlight"));
    device.set_xalign(0.0);
    column.append(&device);

    column.upcast()
}
