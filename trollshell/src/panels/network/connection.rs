//! Configuration column: primary link details, all-links overview, DNS.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::networkd::{self, Link, OperationalState};
use hytte::services::resolved;

use super::pill_label;
use crate::components::reactive_list::reactive_list;

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

/// The four address/gateway detail row titles for a per-link disclosure, in
/// display order. Index-aligned with [`link_detail_values`].
const LINK_DETAIL_TITLES: [&str; 4] = [
    "IPv4 address",
    "IPv4 gateway",
    "IPv6 address",
    "IPv6 gateway",
];

/// The four address/gateway detail values for `link`, index-aligned with
/// [`LINK_DETAIL_TITLES`]. Empty strings mean the corresponding row hides
/// itself.
fn link_detail_values(link: &Link) -> [String; 4] {
    [
        ipv4_addresses(link),
        link.gateway_v4.map(|g| g.to_string()).unwrap_or_default(),
        ipv6_addresses(link),
        link.gateway_v6.map(|g| g.to_string()).unwrap_or_default(),
    ]
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

/// The widgets making up one link's disclosure row inside "All links". Kept in
/// a stable per-link cache (keyed by link name) so the bind handler can diff
/// the list in place across `networkd::links()` emissions — rather than
/// drain-rebuilding — and an expanded link survives carrier blips with its
/// chevron and revealer state intact (#152).
struct LinkRow {
    /// The `AdwActionRow` shown in the "All links" expander (name + pill +
    /// chevron). Added to / removed from the parent expander via `add_row` /
    /// `remove`.
    action: adw::ActionRow,
    /// The boxed-list wrapper holding this link's detail rows, slid open/shut
    /// by the chevron. A separate row in the parent expander, kept directly
    /// beneath `action` by the diff's re-ordering pass.
    detail_holder: gtk::ListBoxRow,
    /// The connection-state pill suffix, updated in place each emission.
    pill: gtk::Label,
    /// The four address/gateway detail rows (index-aligned with
    /// [`LINK_DETAIL_TITLES`]), updated in place — they hide themselves when
    /// their value is empty.
    detail_rows: [adw::ActionRow; 4],
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

    // Per-link rows keyed by link name, diffed in place on every
    // `networkd::links()` emission. networkd emits on any link change (carrier
    // flap, address renewal, a `vb-h-*` veth blinking); draining and rebuilding
    // collapsed rows each time destroyed any link you had expanded (#152), so
    // we instead remove only links that disappeared, add only new ones, and
    // update survivors in place — leaving their chevron/revealer untouched.
    let cache: Rc<RefCell<HashMap<String, LinkRow>>> = Rc::new(RefCell::new(HashMap::new()));
    let expander_for_bind = expander.clone();
    let cache_for_bind = cache.clone();
    bind(networkd::links(), &expander, move |_, links| {
        let mut named: Vec<&Link> = links.iter().filter(|l| l.name != "lo").collect();
        named.sort_by(|a, b| a.name.cmp(&b.name));

        let mut cache_mut = cache_for_bind.borrow_mut();

        // Remove links that disappeared.
        let live: HashSet<String> = named.iter().map(|l| l.name.clone()).collect();
        cache_mut.retain(|name, row| {
            let keep = live.contains(name);
            if !keep {
                expander_for_bind.remove(&row.action);
                expander_for_bind.remove(&row.detail_holder);
            }
            keep
        });

        // Update survivors in place; build rows for newly-seen links.
        let mut added_new = false;
        for link in &named {
            if let Some(row) = cache_mut.get(&link.name) {
                update_link_row(row, link);
            } else {
                let row = build_link_row(&expander_for_bind, link);
                cache_mut.insert(link.name.clone(), row);
                added_new = true;
            }
        }

        // Only re-order when the membership changed: detach every surviving
        // row, then re-add in sorted name order so a new link lands in the
        // right slot and each link's detail holder stays directly beneath its
        // action row. Survivor updates never touch ordering, so an expanded
        // link does not jump or re-collapse on a carrier blip.
        if added_new {
            for row in cache_mut.values() {
                expander_for_bind.remove(&row.action);
                expander_for_bind.remove(&row.detail_holder);
            }
            for link in &named {
                if let Some(row) = cache_mut.get(&link.name) {
                    expander_for_bind.add_row(&row.action);
                    expander_for_bind.add_row(&row.detail_holder);
                }
            }
        }
    });

    expander
}

/// Build one link's disclosure: an activatable `AdwActionRow` (name + state
/// pill + chevron we drive ourselves) over a `gtk::Revealer` holding the
/// link's address/gateway rows, added to `parent`. Activating the row toggles
/// the revealer and flips the chevron between `pan-end` (collapsed) and
/// `pan-down` (expanded) — so the disclosure state is ours, sidestepping
/// libadwaita's stuck nested-expander arrow (#152).
fn build_link_row(parent: &adw::ExpanderRow, link: &Link) -> LinkRow {
    let action = adw::ActionRow::builder()
        .title(&link.name)
        .activatable(true)
        .build();
    action.set_title_lines(1);

    let pill = pill_label(
        state_pill_text(link.operational),
        state_pill_class(link.operational),
    );
    action.add_suffix(&pill);

    let chevron = gtk::Image::from_icon_name("pan-end-symbolic");
    chevron.set_valign(gtk::Align::Center);
    action.add_suffix(&chevron);

    // Detail rows live in their own boxed-list inside the revealer, wrapped in a
    // `gtk::ListBoxRow` so the parent expander renders them as a row rather than
    // a separator-less child below the list (the adw routing gotcha).
    let detail_list = gtk::ListBox::new();
    detail_list.add_css_class("boxed-list");
    detail_list.set_selection_mode(gtk::SelectionMode::None);

    let values = link_detail_values(link);
    let detail_rows = std::array::from_fn(|i| {
        let row = build_addr_row(LINK_DETAIL_TITLES[i]);
        row.set_activatable(false);
        row.set_selectable(false);
        apply_addr_row(&row, &values[i]);
        detail_list.append(&row);
        row
    });

    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    revealer.set_reveal_child(false);
    revealer.set_child(Some(&detail_list));

    let detail_holder = gtk::ListBoxRow::new();
    detail_holder.set_activatable(false);
    detail_holder.set_selectable(false);
    detail_holder.set_child(Some(&revealer));

    // Activation drives the disclosure: flip the revealer, swap the chevron.
    let chevron_for_click = chevron.clone();
    action.connect_activated(move |_| {
        let now = !revealer.reveals_child();
        revealer.set_reveal_child(now);
        chevron_for_click.set_icon_name(Some(if now {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        }));
    });

    parent.add_row(&action);
    parent.add_row(&detail_holder);

    LinkRow {
        action,
        detail_holder,
        pill,
        detail_rows,
    }
}

/// Update a surviving link's row in place: refresh the state pill and the four
/// detail values. Never touches the chevron or revealer, so the link's
/// expanded/collapsed disclosure state persists across refreshes.
fn update_link_row(row: &LinkRow, link: &Link) {
    row.pill.set_text(state_pill_text(link.operational));
    // Reset to the two pill variants and re-apply the current one so the class
    // set doesn't accumulate across state changes.
    row.pill.remove_css_class("ts-pill-connected");
    row.pill.remove_css_class("ts-pill-known");
    row.pill.add_css_class(state_pill_class(link.operational));

    let values = link_detail_values(link);
    for (detail, value) in row.detail_rows.iter().zip(values.iter()) {
        apply_addr_row(detail, value);
    }
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

    reactive_list(
        &expander,
        resolved::dns().map(|state| state.servers),
        |ip: &std::net::IpAddr| {
            let row = adw::ActionRow::builder()
                .title(ip.to_string())
                .activatable(false)
                .build();
            row.set_title_lines(1);
            row.add_css_class("ts-mono");
            row
        },
        None::<fn() -> adw::ActionRow>,
    );

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
