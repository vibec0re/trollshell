use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors;

use super::util::fmt_bytes;

pub fn widget() -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-gpu");

    let label = gtk::Label::new(Some("--%"));
    btn.set_child(Some(&label));

    // Bind label text and visibility to the gpu signal.
    bind_text(
        sensors::gpu().map(|g| match &g {
            Some(state) => match state.load {
                Some(load) => format!("{:>2.0}%", load * 100.0),
                None => "--%" .to_string(),
            },
            None => "--%".to_string(),
        }),
        &label,
    );

    // Hide the widget when no GPU is detected.
    bind_visible(sensors::gpu().map(|g| g.is_some()), &btn);

    let detail = detail_widget();
    let popup = Popup::new(&btn)
        .child(detail)
        .position(PopupPosition::Bottom)
        .css_class("ts-gpu-popup")
        .build();
    btn.connect_clicked(move |_| popup.toggle());
    btn.upcast()
}

fn detail_widget() -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.add_css_class("ts-popup-column");

    // Headline: vendor name
    let headline = gtk::Label::new(None);
    headline.set_xalign(0.0);
    headline.add_css_class("ts-popup-headline");
    bind_text(
        sensors::gpu().map(|g| match g {
            Some(state) => state.name.clone(),
            None => "GPU".to_string(),
        }),
        &headline,
    );
    column.append(&headline);

    // Temperature line
    let temp_label = gtk::Label::new(None);
    temp_label.set_xalign(0.0);
    bind_text(
        sensors::gpu().map(|g| match g.as_ref().and_then(|s| s.temperature_celsius) {
            Some(t) => format!("Temp: {t:.0}°C"),
            None => "Temp: —".to_string(),
        }),
        &temp_label,
    );
    column.append(&temp_label);

    // Load line
    let load_label = gtk::Label::new(None);
    load_label.set_xalign(0.0);
    bind_text(
        sensors::gpu().map(|g| match g.as_ref().and_then(|s| s.load) {
            Some(l) => format!("Load: {:.0}%", l * 100.0),
            None => "Load: —".to_string(),
        }),
        &load_label,
    );
    column.append(&load_label);

    // Memory line
    let mem_label = gtk::Label::new(None);
    mem_label.set_xalign(0.0);
    bind_text(
        sensors::gpu().map(|g| {
            let used = g.as_ref().and_then(|s| s.memory_used_bytes);
            let total = g.as_ref().and_then(|s| s.memory_total_bytes);
            match (used, total) {
                (Some(u), Some(t)) => format!("VRAM: {} / {}", fmt_bytes(u), fmt_bytes(t)),
                _ => "VRAM: —".to_string(),
            }
        }),
        &mem_label,
    );
    column.append(&mem_label);

    column.upcast()
}
