use hytte::futures_signals::map_ref;
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::bluetooth::{self, Device};

pub fn widget() -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-bluetooth");

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    // Combine adapter + devices to pick the correct icon and visibility.
    let combined = map_ref! {
        let adapter = bluetooth::adapter(),
        let devs = bluetooth::devices() => {
            let any_connected = devs.iter().any(|d| d.connected);
            (adapter.clone(), any_connected)
        }
    };

    bind(combined, &btn, |w, (adapter, any_connected)| {
        match &adapter {
            None => {
                w.set_visible(false);
            }
            Some(a) => {
                w.set_visible(true);
                let img = w
                    .child()
                    .and_downcast::<gtk::Image>()
                    .expect("button child is an Image");
                let icon_name = if a.powered && any_connected {
                    "bluetooth-active-symbolic"
                } else if a.powered {
                    "bluetooth-symbolic"
                } else {
                    "bluetooth-disabled-symbolic"
                };
                img.set_icon_name(Some(icon_name));
            }
        }
    });

    let _ = icon; // moved into button child above; keep reference for bind
    let detail = detail_widget();
    let popup = Popup::new(&btn)
        .child(detail)
        .position(PopupPosition::Bottom)
        .css_class("ts-bluetooth-popup")
        .build();

    btn.connect_clicked(move |_| popup.toggle());
    btn.upcast()
}

fn detail_widget() -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 6);
    column.add_css_class("ts-popup-column");

    // ── Headline ──────────────────────────────────────────────────────────────
    let headline = gtk::Label::new(None);
    headline.set_xalign(0.0);
    headline.add_css_class("ts-popup-headline");
    bind_text(
        bluetooth::adapter().map(|a| match a {
            Some(adapter) => format!(
                "{} — {}",
                adapter.name,
                if adapter.powered { "on" } else { "off" }
            ),
            None => "Bluetooth — no adapter".to_string(),
        }),
        &headline,
    );
    column.append(&headline);

    // ── Power toggle ──────────────────────────────────────────────────────────
    let power_btn = gtk::ToggleButton::with_label("Power");
    bind(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.powered)),
        &power_btn,
        |w, powered| {
            // Block the toggle signal while we set state programmatically.
            w.set_active(powered);
        },
    );
    power_btn.connect_toggled(|btn| {
        bluetooth::set_powered(btn.is_active());
    });
    column.append(&power_btn);

    // ── Scan toggle ───────────────────────────────────────────────────────────
    let scan_btn = gtk::ToggleButton::new();
    bind(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.discovering)),
        &scan_btn,
        |w, discovering| {
            w.set_active(discovering);
            w.set_label(if discovering { "Stop scan" } else { "Scan…" });
        },
    );
    scan_btn.connect_toggled(|btn| {
        if btn.is_active() {
            bluetooth::start_discovery();
        } else {
            bluetooth::stop_discovery();
        }
    });
    column.append(&scan_btn);

    // ── Separator ─────────────────────────────────────────────────────────────
    let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
    column.append(&sep);

    // ── Device list ───────────────────────────────────────────────────────────
    let device_list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let device_list_for_bind = device_list.clone();

    bind(bluetooth::devices(), &device_list, move |_, devs| {
        // Clear existing rows.
        while let Some(child) = device_list_for_bind.first_child() {
            device_list_for_bind.remove(&child);
        }

        for dev in &devs {
            let row = build_device_row(dev);
            device_list_for_bind.append(&row);
        }
    });
    column.append(&device_list);

    column.upcast()
}

fn build_device_row(dev: &Device) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-bluetooth-device");
    if dev.connected {
        btn.add_css_class("connected");
    }

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("ts-bluetooth-row");

    // Icon.
    let icon_name = if dev.icon.is_empty() {
        "bluetooth-symbolic"
    } else {
        &dev.icon
    };
    let img = gtk::Image::from_icon_name(icon_name);
    row.append(&img);

    // Alias label.
    let alias_label = gtk::Label::new(Some(&dev.alias));
    alias_label.set_hexpand(true);
    alias_label.set_xalign(0.0);
    row.append(&alias_label);

    // State suffix.
    let state_text = if dev.connected {
        "(connected)"
    } else if dev.paired {
        "(paired)"
    } else {
        ""
    };
    if !state_text.is_empty() {
        let state_label = gtk::Label::new(Some(state_text));
        state_label.add_css_class("dim-label");
        row.append(&state_label);
    }

    btn.set_child(Some(&row));

    // Click → toggle connect/disconnect.
    let path = dev.path.clone();
    let connected = dev.connected;
    btn.connect_clicked(move |_| {
        if connected {
            bluetooth::disconnect_device(&path);
        } else {
            bluetooth::connect_device(&path);
        }
    });

    btn.upcast()
}
