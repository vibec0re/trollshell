use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::networkd::{self, Link, OperationalState};
use hytte::services::resolved;

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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

    column.upcast()
}

#[allow(dead_code)]
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
