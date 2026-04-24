use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors;

use super::util::fmt_bytes;

pub fn widget() -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-memory");

    let label = gtk::Label::new(Some("--%"));
    btn.set_child(Some(&label));

    bind_text(
        sensors::memory().map(|m| {
            if m.total == 0 {
                "--%".to_string()
            } else {
                #[allow(clippy::cast_precision_loss)]
                let pct = (m.used as f64 / m.total as f64) * 100.0;
                format!("{pct:>2.0}%")
            }
        }),
        &label,
    );

    let detail = detail_widget();
    let popup = Popup::new(&btn)
        .child(detail)
        .position(PopupPosition::Bottom)
        .css_class("ts-memory-popup")
        .build();
    btn.connect_clicked(move |_| popup.toggle());
    btn.upcast()
}

fn detail_widget() -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.add_css_class("ts-popup-column");

    let headline = gtk::Label::new(None);
    headline.set_xalign(0.0);
    headline.add_css_class("ts-popup-headline");
    bind_text(
        sensors::memory().map(|m| {
            if m.total == 0 {
                "Memory --%".to_string()
            } else {
                #[allow(clippy::cast_precision_loss)]
                let pct = (m.used as f64 / m.total as f64) * 100.0;
                format!("Memory {pct:.0}%")
            }
        }),
        &headline,
    );
    column.append(&headline);

    let used_label = gtk::Label::new(None);
    used_label.set_xalign(0.0);
    bind_text(
        sensors::memory().map(|m| {
            format!("{} / {}", fmt_bytes(m.used), fmt_bytes(m.total))
        }),
        &used_label,
    );
    column.append(&used_label);

    let avail_label = gtk::Label::new(None);
    avail_label.set_xalign(0.0);
    bind_text(
        sensors::memory().map(|m| format!("available: {}", fmt_bytes(m.available))),
        &avail_label,
    );
    column.append(&avail_label);

    column.upcast()
}
