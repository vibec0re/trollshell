use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::networkd::{self, Link, OperationalState};
use hytte::services::resolved;
use hytte::services::sensors;
use hytte::services::wifi;

use super::util::{fmt_bytes, fmt_rate};

pub fn widget() -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-network");

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    bind(networkd::primary(), &icon, |w, primary| {
        w.set_icon_name(Some(icon_name(primary.as_ref())));
    });

    let detail = detail_widget();
    let popup = Popup::new(&btn)
        .child(detail)
        .position(PopupPosition::Bottom)
        .css_class("ts-network-popup")
        .build();

    btn.connect_clicked(move |_| popup.toggle());
    btn.upcast()
}

fn icon_name(primary: Option<&Link>) -> &'static str {
    match primary.map(|l| l.operational) {
        Some(OperationalState::Routable) => "network-wired-symbolic",
        Some(OperationalState::Degraded | OperationalState::DegradedCarrier) => {
            "network-wired-acquiring-symbolic"
        }
        Some(_) => "network-wired-no-route-symbolic",
        None => "network-wired-disconnected-symbolic",
    }
}

fn detail_widget() -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.add_css_class("ts-popup-column");

    let headline = gtk::Label::new(None);
    headline.set_xalign(0.0);
    headline.add_css_class("ts-popup-headline");
    bind_text(
        networkd::primary().map(|p| match p {
            Some(link) => format!("{}: {}", link.name, describe_state(link.operational)),
            None => "Disconnected".to_string(),
        }),
        &headline,
    );
    column.append(&headline);

    let links_label = gtk::Label::new(None);
    links_label.set_xalign(0.0);
    bind_text(
        networkd::links().map(|ls| {
            let lines: Vec<String> = ls
                .iter()
                .map(|l| format!("{} ({})", l.name, describe_state(l.operational)))
                .collect();
            lines.join("\n")
        }),
        &links_label,
    );
    column.append(&links_label);

    let dns = gtk::Label::new(None);
    dns.set_xalign(0.0);
    bind_text(
        resolved::dns().map(|state| {
            if state.configured() {
                format!("DNS: {} server(s)", state.servers.len())
            } else {
                "DNS: not configured".to_string()
            }
        }),
        &dns,
    );
    column.append(&dns);

    let rate = gtk::Label::new(None);
    rate.set_xalign(0.0);
    bind_text(
        sensors::network().map(|net| {
            net.interfaces
                .iter()
                .filter(|i| i.name != "lo")
                .map(|i| {
                    format!(
                        "{}: \u{2193} {} \u{2191} {}",
                        i.name,
                        fmt_rate(i.rx_rate_bps),
                        fmt_rate(i.tx_rate_bps),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }),
        &rate,
    );
    column.append(&rate);

    // Cumulative totals across all non-loopback interfaces.
    let totals = gtk::Label::new(None);
    totals.set_xalign(0.0);
    bind_text(
        sensors::network().map(|net| {
            let (rx, tx) = net
                .interfaces
                .iter()
                .filter(|i| i.name != "lo")
                .fold((0u64, 0u64), |(rx, tx), i| {
                    (rx + i.rx_bytes_total, tx + i.tx_bytes_total)
                });
            format!(
                "Total: \u{2193} {} \u{2191} {}",
                fmt_bytes(rx),
                fmt_bytes(tx),
            )
        }),
        &totals,
    );
    column.append(&totals);

    // TCP connection counts.
    let conns = gtk::Label::new(None);
    conns.set_xalign(0.0);
    bind_text(
        sensors::net_connections().map(|c| {
            format!(
                "TCP: {} established, {} listening",
                c.established_total(),
                c.tcp_listen + c.tcp6_listen,
            )
        }),
        &conns,
    );
    column.append(&conns);

    append_wifi_section(&column);

    column.upcast()
}

fn append_wifi_section(column: &gtk::Box) {
    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.set_margin_top(4);
    separator.set_margin_bottom(4);
    column.append(&separator);

    let wifi_headline = gtk::Label::new(None);
    wifi_headline.set_xalign(0.0);
    wifi_headline.add_css_class("ts-wifi-headline");
    bind_text(
        wifi::station().map(|s| match s {
            Some(st) => match st.connected_ssid {
                Some(ssid) => format!("Wi-Fi: {ssid}"),
                None => match st.state {
                    wifi::StationState::Connecting => "Wi-Fi: connecting\u{2026}".to_string(),
                    wifi::StationState::Roaming => "Wi-Fi: roaming".to_string(),
                    _ => "Wi-Fi: disconnected".to_string(),
                },
            },
            None => "Wi-Fi: no adapter".to_string(),
        }),
        &wifi_headline,
    );
    column.append(&wifi_headline);

    let scan_btn = gtk::Button::with_label("Scan");
    scan_btn.connect_clicked(|_| wifi::scan());
    bind(
        wifi::station().map(|s| !s.is_some_and(|st| st.scanning)),
        &scan_btn,
        gtk::prelude::WidgetExt::set_sensitive,
    );
    column.append(&scan_btn);

    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_min_content_height(160);
    scrolled.set_max_content_height(240);
    scrolled.add_css_class("ts-wifi-list");

    let networks_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    scrolled.set_child(Some(&networks_box));
    column.append(&scrolled);

    let networks_box_for_signal = networks_box.clone();
    bind(
        wifi::networks(),
        &networks_box,
        move |_, nets| {
            while let Some(child) = networks_box_for_signal.first_child() {
                networks_box_for_signal.remove(&child);
            }
            for net in nets {
                let row = build_network_row(&net);
                networks_box_for_signal.append(&row);
            }
        },
    );
}

fn build_network_row(net: &wifi::WifiNetwork) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-wifi-row");
    if net.connected {
        btn.add_css_class("connected");
    }

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);

    let icon = gtk::Image::from_icon_name(signal_icon(net.signal_dbm));
    row.append(&icon);

    let label = gtk::Label::new(Some(&net.ssid));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    row.append(&label);

    let suffix = if net.connected {
        "connected".to_string()
    } else if net.known {
        "known".to_string()
    } else if net.security == "open" {
        "open".to_string()
    } else {
        net.security.clone()
    };
    let suffix_label = gtk::Label::new(Some(&suffix));
    suffix_label.add_css_class("ts-wifi-suffix");
    row.append(&suffix_label);

    btn.set_child(Some(&row));

    let path = net.path.clone();
    let connected = net.connected;
    btn.connect_clicked(move |_| {
        if connected {
            wifi::disconnect();
        } else {
            wifi::connect_network(&path);
        }
    });

    btn.upcast()
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

fn describe_state(s: OperationalState) -> &'static str {
    match s {
        OperationalState::Routable => "routable",
        OperationalState::Degraded => "degraded",
        OperationalState::DegradedCarrier => "degraded carrier",
        OperationalState::Carrier => "carrier",
        OperationalState::EnslavedRouting => "enslaved",
        OperationalState::NoCarrier => "no carrier",
        OperationalState::Dormant => "dormant",
        OperationalState::Off => "off",
        OperationalState::Missing => "missing",
        OperationalState::Unknown => "unknown",
    }
}
