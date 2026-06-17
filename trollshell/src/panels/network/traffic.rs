//! Live column top: per-interface byte rates plus totals and TCP summary.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::sensors;
use hytte::ui::Sparkline;

use crate::components::format::{fmt_bytes, fmt_rate};
use crate::components::history_row::build_history_row;

pub(super) fn build_traffic_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Traffic").build();

    // Per-interface rows keyed by interface name. We keep widgets across
    // emissions so the Sparkline accumulates history; only the set of
    // keys changes (hot-plug, VPN tunnels coming/going).
    let cache: Rc<RefCell<HashMap<String, IfaceRow>>> = Rc::new(RefCell::new(HashMap::new()));
    let group_for_bind = group.clone();
    let cache_for_bind = cache.clone();
    bind(sensors::network(), &group, move |_g, net| {
        let mut interfaces: Vec<&sensors::NetInterface> =
            net.interfaces.iter().filter(|i| i.name != "lo").collect();
        interfaces.sort_by(|a, b| a.name.cmp(&b.name));

        let mut cache_mut = cache_for_bind.borrow_mut();

        // Remove interfaces that disappeared.
        let live: HashSet<String> = interfaces.iter().map(|i| i.name.clone()).collect();
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
    });

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
            format!("\u{2193} {} \u{2191} {}", fmt_bytes(rx), fmt_bytes(tx))
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
/// `sensors::network()` emission. Stored in the network drawer's
/// interface cache.
struct IfaceRow {
    container: gtk::Box,
    spark: Sparkline,
    value: gtk::Label,
}

fn build_iface_traffic_row(iface: &sensors::NetInterface) -> IfaceRow {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 2);
    outer.set_hexpand(true);

    let (top_row, spark, top_value) = build_history_row(&iface.name);
    // Hide the right-hand label slot — units are shown in the detail row
    // below the graph, so the "B/s" label here is redundant (#83).
    top_value.set_visible(false);
    top_row.set_hexpand(true);
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
