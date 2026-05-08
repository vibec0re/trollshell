//! Network drawer panel — two-column layout (Configuration left, Live right)
//! plus an "Active connections" drill-down. Backed by `networkd`,
//! `resolved`, `sensors`, `wifi`, and `netconn` services.

mod connection;
mod traffic;
mod wifi;

use hytte::adw::{self, prelude::*};
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::netconn;
use hytte::services::wifi as wifi_svc;

use crate::components::deep_link_row::deep_link_row;
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
    left.append(&connection::build_connection_group());
    grid.attach(&left, 0, 0, 1, 1);

    // Right column: live stats.
    let right = section("Live");
    right.append(&traffic::build_traffic_group());

    let wifi_group = wifi::build_wifi_group();
    // Hide the Wi-Fi section entirely when no adapter is present (e.g. a
    // desktop machine with no wireless hardware).
    bind(
        wifi_svc::adapter().map(|a| a.is_some()),
        &wifi_group,
        gtk::prelude::WidgetExt::set_visible,
    );
    right.append(&wifi_group);
    grid.attach(&right, 1, 0, 1, 1);

    outer.append(&grid);

    // Active connections drill-down — opens Page::Connections for the full list.
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

/// Pill-styled label used for connection states (connected / known) and
/// link operational states. Always vertically centered for use as an
/// `ActionRow` suffix.
pub(super) fn pill_label(text: &str, variant_class: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_valign(gtk::Align::Center);
    label.add_css_class("ts-net-pill");
    label.add_css_class(variant_class);
    label
}
