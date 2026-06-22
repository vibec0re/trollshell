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

    // Online/offline state as a card row (replaces the old floating group
    // description). The pill suffix updates in-place; CSS classes are
    // toggled rather than recreating a new label each emission.
    let state_row = adw::ActionRow::builder()
        .title("Status")
        .activatable(false)
        .selectable(false)
        .build();
    let state_pill = pill_label("Offline", "ts-pill-known");
    state_row.add_suffix(&state_pill);

    bind(
        networkd::primary().map(|p| {
            let subtitle = match p {
                Some(ref link) => match link.operational {
                    OperationalState::Routable => format!("Online via {}", link.name),
                    OperationalState::Carrier | OperationalState::DegradedCarrier => {
                        format!("Limited connectivity via {}", link.name)
                    }
                    other => format!("{} via {}", describe_state(other), link.name),
                },
                None => "Offline".to_string(),
            };
            let online = p.is_some_and(|l| l.operational == OperationalState::Routable);
            (subtitle, online)
        }),
        &state_row,
        {
            let pill = state_pill.clone();
            move |row, (subtitle, online)| {
                row.set_subtitle(&subtitle);
                pill.set_text(if online { "Online" } else { "Offline" });
                if online {
                    pill.remove_css_class("ts-pill-known");
                    pill.add_css_class("ts-pill-connected");
                } else {
                    pill.remove_css_class("ts-pill-connected");
                    pill.add_css_class("ts-pill-known");
                }
            }
        },
    );
    group.add(&state_row);

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

    // Each row reactively re-derives its value from `primary()`.
    expander.add_row(&addr_row("IPv4 address", ipv4_addresses));
    expander.add_row(&addr_row("IPv4 gateway", |link| {
        link.gateway_v4.map(|g| g.to_string()).unwrap_or_default()
    }));
    expander.add_row(&addr_row("IPv6 address", ipv6_addresses));
    expander.add_row(&addr_row("IPv6 gateway", |link| {
        link.gateway_v6.map(|g| g.to_string()).unwrap_or_default()
    }));

    expander
}

/// All IPv4 addresses of `link`, one per line, as `addr/prefix`.
fn ipv4_addresses(link: &Link) -> String {
    link.addresses
        .iter()
        .filter_map(|a| match a.addr {
            std::net::IpAddr::V4(v) => Some(format!("{v}/{}", a.prefix_len)),
            std::net::IpAddr::V6(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// All routable IPv6 addresses of `link` (link-local excluded), one per line,
/// as `addr/prefix`.
fn ipv6_addresses(link: &Link) -> String {
    link.addresses
        .iter()
        .filter_map(|a| match a.addr {
            std::net::IpAddr::V6(v) if !v.is_unicast_link_local() => {
                Some(format!("{v}/{}", a.prefix_len))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build one address/gateway row bound to the primary link. The value is
/// rendered as the row *subtitle* (full width, wraps freely) so long IPv6
/// values can never collapse the title into one char per line. The row
/// auto-hides when the derived value is empty.
fn addr_row(title: &str, derive: impl Fn(&Link) -> String + 'static) -> adw::ActionRow {
    let row = build_addr_row(title);
    bind(
        networkd::primary().map(move |p| p.as_ref().map(&derive).unwrap_or_default()),
        &row,
        |row, txt| apply_addr_row(row, &txt),
    );
    row
}

/// Build an empty address/gateway row with the wrapping-subtitle layout shared
/// by the primary block and the per-link expanders.
fn build_addr_row(title: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    // Keep the title on a single line; let the subtitle (the value) wrap.
    row.set_title_lines(1);
    row.set_subtitle_lines(0);
    row.add_css_class("ts-mono");
    row
}

/// Apply a derived value to an address/gateway row: set it as the subtitle and
/// hide the row when empty.
fn apply_addr_row(row: &adw::ActionRow, value: &str) {
    row.set_subtitle(value);
    row.set_visible(!value.is_empty());
}

/// Build the four (IPv4/IPv6 address + gateway) detail rows for a *specific*
/// link — the same layout the primary block uses — and append them to
/// `parent`. Empty rows hide themselves.
fn add_link_detail_rows(parent: &adw::ExpanderRow, link: &Link) {
    let rows = [
        ("IPv4 address", ipv4_addresses(link)),
        (
            "IPv4 gateway",
            link.gateway_v4.map(|g| g.to_string()).unwrap_or_default(),
        ),
        ("IPv6 address", ipv6_addresses(link)),
        (
            "IPv6 gateway",
            link.gateway_v6.map(|g| g.to_string()).unwrap_or_default(),
        ),
    ];
    for (title, value) in rows {
        let row = build_addr_row(title);
        apply_addr_row(&row, &value);
        parent.add_row(&row);
    }
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

    // Track child rows so we can drain & rebuild on each emission. Each link is
    // itself an expander (name + state pill) revealing that link's own
    // address/gateway rows — so the still-up Wi-Fi config stays reachable even
    // when ethernet becomes the primary/default route (#144).
    let rows_track: Rc<RefCell<Vec<adw::ExpanderRow>>> = Rc::new(RefCell::new(Vec::new()));
    let expander_for_bind = expander.clone();
    let rows_for_bind = rows_track.clone();
    bind(networkd::links(), &expander, move |_, links| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            expander_for_bind.remove(&row);
        }
        let mut new_rows = Vec::new();
        for link in links.iter().filter(|l| l.name != "lo") {
            let row = adw::ExpanderRow::builder().title(&link.name).build();
            row.add_suffix(&pill_label(
                state_pill_text(link.operational),
                state_pill_class(link.operational),
            ));
            add_link_detail_rows(&row, link);
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
