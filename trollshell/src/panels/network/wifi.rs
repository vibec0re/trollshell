//! Live column bottom: Wi-Fi adapter, scan controls, and network list.

use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::map_ref;
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::wifi;

use super::pill_label;

pub(super) fn build_wifi_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Wi-Fi").build();

    // Live description bound to (adapter, station, networks).
    let combined = map_ref! {
        let adapter = wifi::adapter(),
        let station = wifi::station(),
        let networks = wifi::networks() => {
            (adapter.clone(), station.clone(), networks.clone())
        }
    };
    bind(combined, &group, |g, (adapter, station, networks)| {
        let text = wifi_description_text(adapter.as_ref(), station.as_ref(), &networks);
        g.set_description(Some(&text));
    });

    let header = build_wifi_header_suffix();
    group.set_header_suffix(Some(&header));

    // Network list inside a bounded ScrolledWindow.
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_min_content_height(160);
    scrolled.set_max_content_height(240);
    scrolled.add_css_class("ts-wifi-list");
    let networks_group = adw::PreferencesGroup::new();
    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let placeholder_track: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));
    scrolled.set_child(Some(&networks_group));
    group.add(&scrolled);

    // Power-off greying for the network list (Switch stays sensitive).
    bind(
        wifi::adapter().map(|a| a.is_some_and(|ad| ad.powered)),
        &scrolled,
        gtk::prelude::WidgetExt::set_sensitive,
    );

    let group_for_bind = networks_group.clone();
    let rows_for_bind = rows_track.clone();
    let placeholder_for_bind = placeholder_track.clone();
    bind(wifi::networks(), &networks_group, move |_, nets| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            group_for_bind.remove(&row);
        }
        if let Some(p) = placeholder_for_bind.borrow_mut().take() {
            group_for_bind.remove(&p);
        }
        if nets.is_empty() {
            let placeholder = adw::ActionRow::builder()
                .title("No networks found")
                .subtitle("Tap Scan to refresh")
                .activatable(false)
                .build();
            group_for_bind.add(&placeholder);
            *placeholder_for_bind.borrow_mut() = Some(placeholder);
        } else {
            let mut new_rows = Vec::with_capacity(nets.len());
            for net in &nets {
                let row = build_network_row(net);
                group_for_bind.add(&row);
                new_rows.push(row);
            }
            *rows_for_bind.borrow_mut() = new_rows;
        }
    });

    group
}

fn build_wifi_header_suffix() -> gtk::Box {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.set_valign(gtk::Align::Center);

    let power_switch = gtk::Switch::new();
    power_switch.set_valign(gtk::Align::Center);
    bind(
        wifi::adapter().map(|a| a.is_some()),
        &power_switch,
        gtk::prelude::WidgetExt::set_sensitive,
    );
    bind_two_way(
        wifi::adapter().map(|a| a.is_some_and(|ad| ad.powered)),
        &power_switch,
        gtk::Switch::set_active,
        |w| w.connect_active_notify(|sw| wifi::set_powered(sw.is_active())),
    );
    header.append(&power_switch);

    let scan_btn = gtk::Button::with_label("Scan");
    scan_btn.connect_clicked(|_| wifi::scan());
    let scan_sensitive_signal = map_ref! {
        let adapter = wifi::adapter(),
        let station = wifi::station() => {
            let powered = adapter.as_ref().is_some_and(|a| a.powered);
            let scanning = station.as_ref().is_some_and(|s| s.scanning);
            powered && !scanning
        }
    };
    bind(
        scan_sensitive_signal,
        &scan_btn,
        gtk::prelude::WidgetExt::set_sensitive,
    );
    header.append(&scan_btn);

    let spinner = gtk::Spinner::new();
    spinner.set_valign(gtk::Align::Center);
    bind(
        wifi::station().map(|s| s.is_some_and(|st| st.scanning)),
        &spinner,
        |w, scanning| {
            w.set_spinning(scanning);
            w.set_visible(scanning);
        },
    );
    header.append(&spinner);

    header
}

fn wifi_description_text(
    adapter: Option<&wifi::Adapter>,
    station: Option<&wifi::Station>,
    networks: &[wifi::WifiNetwork],
) -> String {
    let Some(a) = adapter else {
        return "No adapter".to_string();
    };
    if !a.powered {
        return "Disabled".to_string();
    }
    let Some(st) = station else {
        return "Disconnected".to_string();
    };
    match st.state {
        wifi::StationState::Connecting => "Connecting\u{2026}".to_string(),
        wifi::StationState::Roaming => "Roaming".to_string(),
        wifi::StationState::Connected => {
            if let Some(ssid) = &st.connected_ssid {
                if let Some(n) = networks.iter().find(|n| n.connected) {
                    format!(
                        "{ssid} \u{00b7} {} dBm ({})",
                        n.signal_dbm,
                        dbm_label(n.signal_dbm)
                    )
                } else {
                    ssid.clone()
                }
            } else {
                "Connected".to_string()
            }
        }
        _ => "Disconnected".to_string(),
    }
}

fn dbm_label(dbm: i16) -> &'static str {
    if dbm >= -50 {
        "excellent"
    } else if dbm >= -60 {
        "good"
    } else if dbm >= -75 {
        "ok"
    } else {
        "weak"
    }
}

fn build_network_row(net: &wifi::WifiNetwork) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&net.ssid)
        .subtitle(format!(
            "{} dBm \u{00b7} {}",
            net.signal_dbm,
            security_label(&net.security)
        ))
        .activatable(true)
        .build();

    let icon = gtk::Image::from_icon_name(signal_icon(net.signal_dbm));
    row.add_prefix(&icon);

    // Pill suffix (only for connected / known states).
    if net.connected {
        row.add_suffix(&pill_label("Connected", "ts-pill-connected"));
    } else if net.known {
        row.add_suffix(&pill_label("Known", "ts-pill-known"));
    }

    let menu_btn = build_network_row_menu(net);
    row.add_suffix(&menu_btn);

    // Row activation: connect only when not currently connected.
    let connected = net.connected;
    let act_path = net.path.clone();
    row.connect_activated(move |_| {
        if !connected {
            wifi::connect_network(&act_path);
        }
    });

    row
}

fn build_network_row_menu(net: &wifi::WifiNetwork) -> gtk::MenuButton {
    let menu_btn = gtk::MenuButton::new();
    menu_btn.set_icon_name("view-more-symbolic");
    menu_btn.set_valign(gtk::Align::Center);
    menu_btn.add_css_class("flat");
    menu_btn.set_tooltip_text(Some("More actions"));

    let popover = gtk::Popover::new();
    let popover_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    popover_box.set_margin_top(4);
    popover_box.set_margin_bottom(4);
    popover_box.set_margin_start(4);
    popover_box.set_margin_end(4);

    let net_path = net.path.clone();
    let known_path_opt = net.known_network_path.clone();

    if net.connected {
        let pop_for_disc = popover.clone();
        let disconnect_btn = gtk::Button::with_label("Disconnect");
        disconnect_btn.add_css_class("flat");
        disconnect_btn.add_css_class("destructive-action");
        disconnect_btn.connect_clicked(move |_| {
            wifi::disconnect();
            pop_for_disc.popdown();
        });
        popover_box.append(&disconnect_btn);

        if let Some(known_path) = known_path_opt {
            let pop_for_forget = popover.clone();
            let forget_btn = gtk::Button::with_label("Forget");
            forget_btn.add_css_class("flat");
            forget_btn.add_css_class("destructive-action");
            forget_btn.connect_clicked(move |_| {
                wifi::forget(&known_path);
                pop_for_forget.popdown();
            });
            popover_box.append(&forget_btn);
        }
    } else {
        let pop_for_conn = popover.clone();
        let connect_path = net_path;
        let connect_btn = gtk::Button::with_label("Connect");
        connect_btn.add_css_class("flat");
        connect_btn.connect_clicked(move |_| {
            wifi::connect_network(&connect_path);
            pop_for_conn.popdown();
        });
        popover_box.append(&connect_btn);

        if let Some(known_path) = known_path_opt {
            let pop_for_forget = popover.clone();
            let forget_btn = gtk::Button::with_label("Forget");
            forget_btn.add_css_class("flat");
            forget_btn.add_css_class("destructive-action");
            forget_btn.connect_clicked(move |_| {
                wifi::forget(&known_path);
                pop_for_forget.popdown();
            });
            popover_box.append(&forget_btn);
        }
    }

    popover.set_child(Some(&popover_box));
    menu_btn.set_popover(Some(&popover));
    menu_btn
}

fn security_label(security: &str) -> &'static str {
    match security {
        "open" => "Open",
        "psk" => "WPA2",
        "8021x" => "802.1x",
        "wep" => "WEP",
        _ => "Secured",
    }
}

fn signal_icon(dbm: i16) -> &'static str {
    if dbm >= -50 {
        "network-wireless-signal-excellent-symbolic"
    } else if dbm >= -60 {
        "network-wireless-signal-good-symbolic"
    } else if dbm >= -75 {
        "network-wireless-signal-ok-symbolic"
    } else {
        "network-wireless-signal-weak-symbolic"
    }
}
