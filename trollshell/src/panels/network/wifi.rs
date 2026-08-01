//! Live column bottom: Wi-Fi adapter, scan controls, and network list.

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::map_ref;
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::wifi;

use super::pill_label;
use crate::components::reactive_list::reactive_list;

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

    // Network list inside a bounded ScrolledWindow. No forced min height
    // (so a short list sizes to its content rather than showing a tall empty
    // block) but a max cap so a long list stays scroll-bounded inside the
    // card. The `.ts-wifi-list` class only rounds the scroll corners now —
    // the distinct background was dropped so the list reads as part of the
    // same card.
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_propagate_natural_height(true);
    // Design-baseline px, routed through `scale()` so the cap tracks font size
    // / text-scaling the same way `stats.rs`'s scroll wrapper does (#708).
    scrolled.set_max_content_height(crate::scale::scale(240));
    scrolled.set_hexpand(true);
    scrolled.add_css_class("ts-wifi-list");
    let networks_group = adw::PreferencesGroup::new();
    scrolled.set_child(Some(&networks_group));

    // Fold the scan list into a dedicated collapsible expander so it no longer
    // always fills the Wi-Fi card. The station/status info (header suffix +
    // description) stays inline; only the scanned-network list lives here. The
    // ScrolledWindow is a plain widget (not a GtkListBoxRow), so wrap it in a
    // non-interactive ListBoxRow — otherwise libadwaita renders it BELOW the
    // expander's boxed-list with no separators.
    let networks_expander = adw::ExpanderRow::builder()
        .title("Available networks")
        .build();
    let list_row = gtk::ListBoxRow::new();
    list_row.set_activatable(false);
    list_row.set_selectable(false);
    list_row.set_hexpand(true);
    list_row.set_child(Some(&scrolled));
    networks_expander.add_row(&list_row);
    group.add(&networks_expander);

    // Subtitle tracks the scanned-network count, matching the sibling
    // expanders (All links / DNS) in the Connection card.
    bind(
        wifi::networks().map(|nets| match nets.len() {
            0 => "No networks".to_string(),
            1 => "1 network".to_string(),
            n => format!("{n} networks"),
        }),
        &networks_expander,
        |w, sub| w.set_subtitle(&sub),
    );

    // Power-off greying for the network list (Switch stays sensitive).
    bind(
        wifi::adapter().map(|a| a.is_some_and(|ad| ad.powered)),
        &scrolled,
        gtk::prelude::WidgetExt::set_sensitive,
    );

    reactive_list(
        &networks_group,
        wifi::networks(),
        |net: &wifi::WifiNetwork| build_network_row(net),
        Some(|| {
            adw::ActionRow::builder()
                .title("No networks found")
                .subtitle("Tap Scan to refresh")
                .activatable(false)
                .build()
        }),
    );

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
