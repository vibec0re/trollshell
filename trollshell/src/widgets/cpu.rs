use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::sensors::{self, CpuLoad};

pub fn widget() -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-cpu");

    let label = gtk::Label::new(Some("--%"));
    btn.set_child(Some(&label));

    bind_text(
        sensors::cpu().map(|c| format!("{:>2.0}%", c.overall * 100.0)),
        &label,
    );

    let detail = detail_widget();
    let popup = Popup::new(&btn)
        .child(detail)
        .position(PopupPosition::Bottom)
        .css_class("ts-cpu-popup")
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
        sensors::cpu().map(|c| format!("CPU {:.0}%", c.overall * 100.0)),
        &headline,
    );
    column.append(&headline);

    let cores_label = gtk::Label::new(None);
    cores_label.set_xalign(0.0);
    bind_text(
        sensors::cpu().map(|c: CpuLoad| {
            c.per_core
                .iter()
                .enumerate()
                .map(|(i, l)| format!("Core {i}: {:>3.0}%", l * 100.0))
                .collect::<Vec<_>>()
                .join("\n")
        }),
        &cores_label,
    );
    column.append(&cores_label);

    let temp_label = gtk::Label::new(None);
    temp_label.set_xalign(0.0);
    bind_text(
        sensors::cpu_temp().map(|t| match t.package_celsius {
            Some(c) => format!("Temp: {c:.0}°C"),
            None => "Temp: —".to_string(),
        }),
        &temp_label,
    );
    column.append(&temp_label);

    column.upcast()
}
