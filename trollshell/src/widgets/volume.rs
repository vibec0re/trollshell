use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::pipewire::{self, Volume};

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
fn detail_widget() -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.add_css_class("ts-popup-column");

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

    let device = gtk::Label::new(Some("Default sink"));
    device.set_xalign(0.0);
    column.append(&device);

    column.upcast()
}
