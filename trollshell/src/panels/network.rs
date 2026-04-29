//! Network drawer panel — two-column layout (Configuration left, Live right)
//! plus an "Active connections" drill-down. Backed by `networkd`,
//! `resolved`, `sensors`, `wifi`, and `netconn` services.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::map_ref;
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::netconn;
use hytte::services::networkd::{self, OperationalState};
use hytte::services::resolved;
use hytte::services::sensors;
use hytte::services::wifi;
use hytte::ui::Sparkline;

use crate::components::deep_link_row::deep_link_row;
use crate::components::format::{fmt_bytes, fmt_rate};
use crate::components::history_row::build_history_row;
use crate::components::layout::{finish_page, page_box, page_grid, section};

pub fn panel_network() -> gtk::Widget {
    // Outer container holds the two-column grid up top, then a drill-down
    // row to the Connections page.
    let outer = page_box();
    outer.add_css_class("ts-popup-column");
    outer.set_spacing(16);

    let grid = page_grid();

    // Left column: configuration.
    let left = section("Configuration");
    left.append(&build_connection_group_v2());
    grid.attach(&left, 0, 0, 1, 1);

    // Right column: live stats.
    let right = section("Live");
    right.append(&build_traffic_group_v2());

    let wifi_group = build_wifi_group_v2();
    // Hide the Wi-Fi section entirely when no adapter is present (e.g. a
    // desktop machine with no wireless hardware).
    bind(
        wifi::adapter().map(|a| a.is_some()),
        &wifi_group,
        gtk::prelude::WidgetExt::set_visible,
    );
    right.append(&wifi_group);
    grid.attach(&right, 1, 0, 1, 1);

    outer.append(&grid);

    // Active connections drill-down — opens Page::Connections for the full list.
    // Subtitle is bound below so the count stays live without expanding.
    let drill = deep_link_row(
        "Active connections",
        None,
        "network-workgroup-symbolic",
        crate::modal::Page::Connections,
    );
    bind(
        netconn::connections().map(|cs| {
            let total = cs.len();
            let with_pid = cs.iter().filter(|c| c.pid.is_some()).count();
            format!("{total} sockets, {with_pid} with PID")
        }),
        &drill,
        |row, txt| row.set_subtitle(&txt),
    );
    let drill_group = adw::PreferencesGroup::new();
    drill_group.add(&drill);
    outer.append(&drill_group);

    finish_page(&outer)
}

fn build_connection_group_v2() -> adw::PreferencesGroup {
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

#[allow(clippy::too_many_lines)]
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

    let v4_addr_row = adw::ActionRow::builder().title("IPv4 address").build();
    let v4_value = gtk::Label::new(None);
    v4_value.add_css_class("ts-mono");
    v4_addr_row.add_suffix(&v4_value);
    bind(
        networkd::primary().map(|p| match p {
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
        }),
        &v4_addr_row,
        move |row, txt| {
            v4_value.set_text(&txt);
            row.set_visible(!txt.is_empty());
        },
    );
    expander.add_row(&v4_addr_row);

    let v4_gw_row = adw::ActionRow::builder().title("IPv4 gateway").build();
    let v4_gw_value = gtk::Label::new(None);
    v4_gw_value.add_css_class("ts-mono");
    v4_gw_row.add_suffix(&v4_gw_value);
    bind(
        networkd::primary().map(|p| {
            p.and_then(|l| l.gateway_v4.map(|g| g.to_string()))
                .unwrap_or_default()
        }),
        &v4_gw_row,
        move |row, txt| {
            v4_gw_value.set_text(&txt);
            row.set_visible(!txt.is_empty());
        },
    );
    expander.add_row(&v4_gw_row);

    let v6_addr_row = adw::ActionRow::builder().title("IPv6 address").build();
    let v6_value = gtk::Label::new(None);
    v6_value.add_css_class("ts-mono");
    v6_addr_row.add_suffix(&v6_value);
    bind(
        networkd::primary().map(|p| match p {
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
        }),
        &v6_addr_row,
        move |row, txt| {
            v6_value.set_text(&txt);
            row.set_visible(!txt.is_empty());
        },
    );
    expander.add_row(&v6_addr_row);

    let v6_gw_row = adw::ActionRow::builder().title("IPv6 gateway").build();
    let v6_gw_value = gtk::Label::new(None);
    v6_gw_value.add_css_class("ts-mono");
    v6_gw_row.add_suffix(&v6_gw_value);
    bind(
        networkd::primary().map(|p| {
            p.and_then(|l| l.gateway_v6.map(|g| g.to_string()))
                .unwrap_or_default()
        }),
        &v6_gw_row,
        move |row, txt| {
            v6_gw_value.set_text(&txt);
            row.set_visible(!txt.is_empty());
        },
    );
    expander.add_row(&v6_gw_row);

    expander
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
            let pill = build_link_state_pill(link.operational);
            row.add_suffix(&pill);
            expander_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    expander
}

/// Build the pill label for a link's operational state.
fn build_link_state_pill(state: OperationalState) -> gtk::Label {
    let label = gtk::Label::new(Some(state_pill_text(state)));
    label.add_css_class("ts-net-pill");
    label.add_css_class(state_pill_class(state));
    label
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

fn build_traffic_group_v2() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Traffic").build();

    // Per-interface rows keyed by interface name. We keep widgets across
    // emissions so the Sparkline accumulates history; only the set of
    // keys changes (hot-plug, VPN tunnels coming/going).
    let cache: Rc<RefCell<HashMap<String, IfaceRow>>> = Rc::new(RefCell::new(HashMap::new()));
    let group_for_bind = group.clone();
    let cache_for_bind = cache.clone();
    bind(
        sensors::network(),
        &group,
        move |_g, net| {
            let mut interfaces: Vec<&sensors::NetInterface> =
                net.interfaces.iter().filter(|i| i.name != "lo").collect();
            interfaces.sort_by(|a, b| a.name.cmp(&b.name));

            let mut cache_mut = cache_for_bind.borrow_mut();

            // Remove interfaces that disappeared.
            let live: HashSet<String> =
                interfaces.iter().map(|i| i.name.clone()).collect();
            cache_mut.retain(|name, entry| {
                let keep = live.contains(name);
                if !keep {
                    group_for_bind.remove(&entry.container);
                }
                keep
            });

            // Update existing rows; create new ones for unseen names.
            for iface in interfaces {
                let combined = iface.rx_rate_bps + iface.tx_rate_bps;
                let value_text = format!(
                    "\u{2193} {} \u{2191} {}",
                    fmt_rate(iface.rx_rate_bps),
                    fmt_rate(iface.tx_rate_bps),
                );
                if let Some(entry) = cache_mut.get(&iface.name) {
                    entry.spark.push(combined);
                    entry.value.set_text(&value_text);
                } else {
                    // New interface arrived mid-session. Remove every
                    // surviving iface row from the group, insert the new
                    // entry into the cache, then re-add all rows in
                    // sorted order so display order matches name order.
                    // Totals/TCP rows are unaffected: they were added to
                    // the group synchronously before any iface row, so
                    // they remain at the bottom of the visual stack.
                    for entry in cache_mut.values() {
                        group_for_bind.remove(&entry.container);
                    }
                    let entry = build_iface_traffic_row(iface);
                    entry.spark.push(combined);
                    entry.value.set_text(&value_text);
                    cache_mut.insert(iface.name.clone(), entry);
                    let mut sorted_names: Vec<&String> = cache_mut.keys().collect();
                    sorted_names.sort();
                    for name in sorted_names {
                        if let Some(entry) = cache_mut.get(name) {
                            group_for_bind.add(&entry.container);
                        }
                    }
                }
            }
        },
    );

    // Totals row: sum across non-loopback interfaces.
    let totals_row = adw::ActionRow::builder().title("Total").build();
    bind(
        sensors::network().map(|net| {
            let (rx, tx) = net
                .interfaces
                .iter()
                .filter(|i| i.name != "lo")
                .fold((0u64, 0u64), |(rx, tx), i| {
                    (rx + i.rx_bytes_total, tx + i.tx_bytes_total)
                });
            format!(
                "\u{2193} {} \u{2191} {}",
                fmt_bytes(rx),
                fmt_bytes(tx),
            )
        }),
        &totals_row,
        |row, text| row.set_subtitle(&text),
    );
    group.add(&totals_row);

    let tcp_row = adw::ActionRow::builder().title("TCP").build();
    bind(
        sensors::net_connections().map(|c| {
            format!(
                "{} established, {} listening",
                c.established_total(),
                c.tcp_listen + c.tcp6_listen,
            )
        }),
        &tcp_row,
        |row, text| row.set_subtitle(&text),
    );
    group.add(&tcp_row);

    group
}

/// Per-interface traffic row holding the widgets the bind updates each
/// `sensors::network()` emission. Returned by `build_iface_traffic_row`
/// and stored in the network drawer's interface cache.
struct IfaceRow {
    container: gtk::Box,
    spark: Sparkline,
    value: gtk::Label,
}

fn build_iface_traffic_row(iface: &sensors::NetInterface) -> IfaceRow {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 2);

    let (top_row, spark, top_value) = build_history_row(&iface.name);
    top_value.set_text("B/s");
    outer.append(&top_row);

    let detail = gtk::Label::new(None);
    detail.add_css_class("ts-stat-value");
    detail.set_xalign(0.0);
    detail.set_margin_start(88);
    detail.set_margin_bottom(4);
    outer.append(&detail);

    IfaceRow {
        container: outer,
        spark,
        value: detail,
    }
}

fn build_wifi_group_v2() -> adw::PreferencesGroup {
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
                let row = build_network_row_v2(net);
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

fn build_network_row_v2(net: &wifi::WifiNetwork) -> adw::ActionRow {
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

    // ⋮ popover.
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

fn pill_label(text: &str, variant_class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_valign(gtk::Align::Center);
    label.add_css_class("ts-net-pill");
    label.add_css_class(variant_class);
    label
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
