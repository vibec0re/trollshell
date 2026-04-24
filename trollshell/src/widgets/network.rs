use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::networkd::{self, Link, OperationalState};
use hytte::services::resolved;
use hytte::services::sensors;

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

    column.upcast()
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
