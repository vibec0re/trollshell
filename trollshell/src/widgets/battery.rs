use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::upower::{self, Battery, BatteryState};

pub fn widget() -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-battery");

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    bind(
        upower::battery().map(|b| b.icon_name.clone()),
        &icon,
        |w, name| {
            if name.is_empty() {
                w.set_icon_name(Some("battery-missing-symbolic"));
            } else {
                w.set_icon_name(Some(&name));
            }
        },
    );

    let detail = detail_widget();
    let popup = Popup::new(&btn)
        .child(detail)
        .position(PopupPosition::Bottom)
        .css_class("ts-battery-popup")
        .build();

    btn.connect_clicked(move |_| popup.toggle());
    btn.upcast()
}

fn detail_widget() -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.add_css_class("ts-popup-column");

    let pct = gtk::Label::new(None);
    pct.set_xalign(0.0);
    pct.add_css_class("ts-popup-headline");
    bind_text(
        upower::battery().map(|b| format!("{:.0}%", b.percentage)),
        &pct,
    );
    column.append(&pct);

    let state_label = gtk::Label::new(None);
    state_label.set_xalign(0.0);
    bind_text(
        upower::battery().map(|b| describe(&b)),
        &state_label,
    );
    column.append(&state_label);

    column.upcast()
}

fn describe(b: &Battery) -> String {
    let state = match b.state {
        BatteryState::Charging => "Charging",
        BatteryState::Discharging => "Discharging",
        BatteryState::Empty => "Empty",
        BatteryState::FullyCharged => "Fully charged",
        BatteryState::PendingCharge => "Pending charge",
        BatteryState::PendingDischarge => "Pending discharge",
        BatteryState::Unknown => "Unknown",
    };
    let remaining = match b.state {
        BatteryState::Discharging => b.time_to_empty.map(|d| fmt_dur(d, "until empty")),
        BatteryState::Charging => b.time_to_full.map(|d| fmt_dur(d, "until full")),
        _ => None,
    };
    match remaining {
        Some(r) => format!("{state} — {r}"),
        None => state.to_string(),
    }
}

fn fmt_dur(d: std::time::Duration, suffix: &str) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    if h > 0 {
        format!("{h}h {m}m {suffix}")
    } else {
        format!("{m}m {suffix}")
    }
}
