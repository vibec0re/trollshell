//! Network drawer panel — two-column layout of flat per-card sections
//! (left: Connection + Wi-Fi cards; right: Total/TCP + per-interface graph
//! cards) plus an "Active connections" drill-down. Backed by `networkd`,
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
use crate::components::layout::{finish_page, page_box, page_grid};

/// Inter-card spacing within a column; matches the grid's 12px row/column
/// spacing so both columns and the page padding read as one uniform rhythm.
const CARD_SPACING: i32 = 12;

/// Build a plain vertical column box that hosts top-level cards directly
/// (no parent `section()` wrapper). Each `AdwPreferencesGroup` added to it
/// renders as its own boxed-list card.
fn card_column() -> gtk::Box {
    let column = gtk::Box::new(gtk::Orientation::Vertical, CARD_SPACING);
    column.set_hexpand(true);
    column
}

pub fn panel_network() -> gtk::Widget {
    // Outer container holds the two-column grid up top, then a drill-down
    // row to the Connections page.
    let outer = page_box();
    outer.add_css_class("ts-popup-column");
    outer.set_spacing(CARD_SPACING);

    let grid = page_grid();
    // The outer `page_box` already contributes `ts-modal-page` padding; strip
    // the duplicate from the grid so the columns align with the cards outside
    // the grid (e.g. the "Active connections" row below it).
    grid.remove_css_class("ts-modal-page");

    // Left column: Connection card, then Wi-Fi card — each its own top-level
    // card (no parent "Configuration" wrapper).
    let left = card_column();
    left.append(&connection::build_connection_group());

    let wifi_group = wifi::build_wifi_group();
    // Hide the Wi-Fi card entirely when no adapter is present (e.g. a
    // desktop machine with no wireless hardware).
    bind(
        wifi_svc::adapter().map(|a| a.is_some()),
        &wifi_group,
        gtk::prelude::WidgetExt::set_visible,
    );
    left.append(&wifi_group);
    grid.attach(&left, 0, 0, 1, 1);

    // Right column: live traffic — Total/TCP card on TOP, per-interface
    // graph card below. Each its own top-level card (no parent "Live"
    // wrapper).
    let right = card_column();
    let (iface_group, totals_group) = traffic::build_traffic_groups();
    right.append(&totals_group);
    right.append(&iface_group);
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
