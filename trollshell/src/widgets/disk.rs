use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors;

use super::util::fmt_bytes;

pub fn widget() -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-disk");

    let label = gtk::Label::new(Some("--%"));
    btn.set_child(Some(&label));

    // Show the most-full mount's usage percentage.
    bind_text(
        sensors::disk().map(|d| {
            let max = d
                .mounts
                .iter()
                .map(|m| m.usage)
                .fold(f64::NEG_INFINITY, f64::max);
            if max.is_finite() {
                format!("{:>2.0}%", max * 100.0)
            } else {
                "--%".to_string()
            }
        }),
        &label,
    );

    let detail = detail_widget();
    let popup = Popup::new(&btn)
        .child(detail)
        .position(PopupPosition::Bottom)
        .css_class("ts-disk-popup")
        .build();
    btn.connect_clicked(move |_| popup.toggle());
    btn.upcast()
}

fn detail_widget() -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.add_css_class("ts-popup-column");

    let headline = gtk::Label::new(Some("Disk"));
    headline.set_xalign(0.0);
    headline.add_css_class("ts-popup-headline");
    column.append(&headline);

    let mounts_label = gtk::Label::new(None);
    mounts_label.set_xalign(0.0);
    bind_text(
        sensors::disk().map(|d| {
            if d.mounts.is_empty() {
                return "No mounts".to_string();
            }
            d.mounts
                .iter()
                .map(|m| {
                    format!(
                        "{}: {:.0}% ({} / {})",
                        m.path,
                        m.usage * 100.0,
                        fmt_bytes(m.used_bytes),
                        fmt_bytes(m.total_bytes),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }),
        &mounts_label,
    );
    column.append(&mounts_label);

    column.upcast()
}
