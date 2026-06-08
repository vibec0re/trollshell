//! Configuration column: primary link details, all-links overview, DNS.

use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::networkd::{self, Link, OperationalState};
use hytte::services::resolved;

use super::pill_label;

pub(super) fn build_connection_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Connection").build();

    // Live description on the group itself.
    bind(
        networkd::primary().map(|p| match p {
            Some(link) => match link.operational {
                OperationalState::Routable => format!("Online via {}", link.name),
                OperationalState::Carrier | OperationalState::DegradedCarrier => {
                    format!("Limited connectivity via {}", link.name)
                }
                other => format!("{} via {}", describe_state(other), link.name),
            },
            None => "Offline".to_string(),
        }),
        &group,
        |g, text| g.set_description(Some(&text)),
    );

    // Three expanders in vertical order; placeholder row replaces
    // Primary when no connection is active.
    group.add(&build_primary_expander());
    group.add(&build_no_connection_placeholder_row());
    group.add(&build_all_links_expander());
    group.add(&build_dns_expander());

    group
}

fn build_primary_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("Primary").build();

    bind(
        networkd::primary().map(|p| p.map_or(String::new(), |link| link.name)),
        &expander,
        |w, name| w.set_title(&name),
    );
    bind(
        networkd::primary().map(|p| match p {
            Some(link) => describe_state(link.operational).to_string(),
            None => String::new(),
        }),
        &expander,
        |w, sub| w.set_subtitle(&sub),
    );
    bind(
        networkd::primary().map(|p| p.is_some()),
        &expander,
        gtk::prelude::WidgetExt::set_visible,
    );

    expander.add_row(&primary_addr_row("IPv4 address", |p| match p {
        Some(link) => link
            .addresses
            .iter()
            .filter_map(|a| match a.addr {
                std::net::IpAddr::V4(v) => Some(format!("{v}/{}", a.prefix_len)),
                std::net::IpAddr::V6(_) => None,
            })
            .collect::<Vec<_>>()
            .join(", "),
        None => String::new(),
    }));
    expander.add_row(&primary_addr_row("IPv4 gateway", |p| {
        p.and_then(|l| l.gateway_v4.map(|g| g.to_string()))
            .unwrap_or_default()
    }));
    expander.add_row(&primary_addr_row("IPv6 address", |p| match p {
        Some(link) => {
            let v6: Vec<String> = link
                .addresses
                .iter()
                .filter_map(|a| match a.addr {
                    std::net::IpAddr::V6(v) if !v.is_unicast_link_local() => {
                        Some(format!("{v}/{}", a.prefix_len))
                    }
                    _ => None,
                })
                .collect();
            if v6.is_empty() {
                String::new()
            } else if v6.len() == 1 {
                v6[0].clone()
            } else {
                format!("{} (+{} more)", v6[0], v6.len() - 1)
            }
        }
        None => String::new(),
    }));
    expander.add_row(&primary_addr_row("IPv6 gateway", |p| {
        p.and_then(|l| l.gateway_v6.map(|g| g.to_string()))
            .unwrap_or_default()
    }));

    expander
}

/// Build one of the four address/gateway rows under the Primary expander.
/// Each row shows a mono-styled value derived from the primary link, and
/// auto-hides when the derived text is empty.
fn primary_addr_row(
    title: &str,
    derive: impl Fn(Option<Link>) -> String + 'static,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let value = gtk::Label::new(None);
    value.add_css_class("ts-mono");
    row.add_suffix(&value);
    bind(networkd::primary().map(derive), &row, move |row, txt| {
        value.set_text(&txt);
        row.set_visible(!txt.is_empty());
    });
    row
}

fn build_no_connection_placeholder_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("No connection")
        .activatable(false)
        .selectable(false)
        .build();
    row.set_subtitle("No primary network link");
    bind(
        networkd::primary().map(|p| p.is_none()),
        &row,
        gtk::prelude::WidgetExt::set_visible,
    );
    row
}

fn build_all_links_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("All links").build();
    bind(
        networkd::links().map(|ls| {
            let count = ls.iter().filter(|l| l.name != "lo").count();
            format!("{count} interface(s)")
        }),
        &expander,
        |w, sub| w.set_subtitle(&sub),
    );

    // Track child rows so we can drain & rebuild on each emission.
    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let expander_for_bind = expander.clone();
    let rows_for_bind = rows_track.clone();
    bind(networkd::links(), &expander, move |_, links| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            expander_for_bind.remove(&row);
        }
        let mut new_rows = Vec::new();
        for link in links.iter().filter(|l| l.name != "lo") {
            let row = adw::ActionRow::builder().title(&link.name).build();
            row.add_suffix(&pill_label(
                state_pill_text(link.operational),
                state_pill_class(link.operational),
            ));
            expander_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    expander
}

fn state_pill_text(state: OperationalState) -> &'static str {
    match state {
        OperationalState::Routable => "Online",
        OperationalState::Carrier | OperationalState::DegradedCarrier => "Carrier",
        OperationalState::Degraded => "Degraded",
        OperationalState::EnslavedRouting => "Enslaved",
        OperationalState::NoCarrier => "No carrier",
        OperationalState::Dormant => "Dormant",
        OperationalState::Off => "Off",
        OperationalState::Missing => "Missing",
        OperationalState::Unknown => "Unknown",
    }
}

fn state_pill_class(state: OperationalState) -> &'static str {
    match state {
        OperationalState::Routable => "ts-pill-connected",
        _ => "ts-pill-known",
    }
}

fn build_dns_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("DNS").build();
    bind(
        resolved::dns().map(|state| {
            if state.configured() {
                format!("{} server(s)", state.servers.len())
            } else {
                "Not configured".to_string()
            }
        }),
        &expander,
        |w, sub| w.set_subtitle(&sub),
    );

    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let expander_for_bind = expander.clone();
    let rows_for_bind = rows_track.clone();
    bind(resolved::dns(), &expander, move |_, state| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            expander_for_bind.remove(&row);
        }
        let mut new_rows = Vec::new();
        for ip in &state.servers {
            let row = adw::ActionRow::builder()
                .title(ip.to_string())
                .activatable(false)
                .build();
            row.set_title_lines(1);
            row.add_css_class("ts-mono");
            expander_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    expander
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
