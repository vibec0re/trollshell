//! Modal page builder functions — one per category.
//!
//! Each `page_*()` fn returns a `gtk::Widget` that will be mounted as a
//! named child of the per-monitor `gtk::Stack` in `modal.rs`. Content is
//! the same as the old per-widget `detail_widget()` functions; this is a
//! structural relocation, not a content redesign.

use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::sensors::{self, CpuLoad};
use hytte::services::systemd;
use hytte::services::vpn;

use crate::components::format::{fmt_bytes, fmt_rate, humanize_since};
use crate::components::history_row::build_history_row;
use crate::components::layout::{finish_page, page_box};


// ── Stats page ────────────────────────────────────────────────────────────────

pub fn page_stats() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.set_spacing(16);

    column.append(build_stats_live_group_v2().upcast_ref::<gtk::Widget>());
    column.append(build_stats_history_group().upcast_ref::<gtk::Widget>());
    column.append(build_stats_services_group().upcast_ref::<gtk::Widget>());

    finish_page(&column)
}

fn build_stats_live_group_v2() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();

    group.add(&build_live_cpu_row());
    group.add(&build_live_per_core_row());
    group.add(&build_live_memory_row());
    group.add(&build_live_swap_row());
    group.add(&build_live_processes_row());
    group.add(&build_live_gpu_row());
    group.add(&build_live_disk_expander());

    group
}

fn build_live_cpu_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("CPU").build();
    bind(
        sensors::cpu().map(|c| format!("{:.0}%", c.overall * 100.0)),
        &row,
        |r, t| r.set_subtitle(&t),
    );
    let temp_label = gtk::Label::new(None);
    temp_label.set_valign(gtk::Align::Center);
    bind(
        sensors::cpu_temp().map(|t| match t.package_celsius {
            Some(c) => format!("{c:.0} \u{00b0}C"),
            None => String::new(),
        }),
        &temp_label,
        move |label, txt| {
            label.set_text(&txt);
            label.set_visible(!txt.is_empty());
        },
    );
    row.add_suffix(&temp_label);
    row
}

fn build_live_per_core_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("Per-core").build();
    row.set_activatable(false);
    row.set_selectable(false);
    bind(
        sensors::cpu().map(|c| format!("{} cores", c.per_core.len())),
        &row,
        |r, t| r.set_subtitle(&t),
    );

    let cores_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    cores_row.add_css_class("ts-cores-row");
    cores_row.set_margin_top(4);
    cores_row.set_margin_bottom(4);
    cores_row.set_hexpand(true);
    cores_row.set_valign(gtk::Align::Center);

    let core_bars: Rc<RefCell<Vec<gtk::ProgressBar>>> = Rc::new(RefCell::new(Vec::new()));
    let cores_row_for_bind = cores_row.clone();
    let bars_for_bind = core_bars.clone();
    bind(sensors::cpu(), &cores_row, move |_, c: CpuLoad| {
        let mut bars = bars_for_bind.borrow_mut();
        if bars.len() != c.per_core.len() {
            while let Some(child) = cores_row_for_bind.first_child() {
                cores_row_for_bind.remove(&child);
            }
            bars.clear();
            for _ in 0..c.per_core.len() {
                let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
                col.set_hexpand(true);
                col.set_halign(gtk::Align::Center);
                let bar = gtk::ProgressBar::new();
                bar.add_css_class("ts-core-bar");
                bar.set_orientation(gtk::Orientation::Vertical);
                bar.set_inverted(true);
                bar.set_valign(gtk::Align::End);
                col.append(&bar);
                cores_row_for_bind.append(&col);
                bars.push(bar);
            }
        }
        for (bar, load) in bars.iter().zip(c.per_core.iter()) {
            bar.set_fraction(load.clamp(0.0, 1.0));
            bar.set_tooltip_text(Some(&format!("{:.0}%", load * 100.0)));
        }
    });

    row.add_suffix(&cores_row);
    row
}

fn build_live_memory_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("Memory").build();
    bind(
        sensors::memory().map(|m| {
            if m.total == 0 {
                "—".to_string()
            } else {
                #[allow(clippy::cast_precision_loss)]
                let pct = (m.used as f64 / m.total as f64) * 100.0;
                format!("{} / {} ({pct:.0}%)", fmt_bytes(m.used), fmt_bytes(m.total))
            }
        }),
        &row,
        |r, t| r.set_subtitle(&t),
    );

    let bar = gtk::ProgressBar::new();
    bar.add_css_class("ts-stat-progress");
    bar.set_valign(gtk::Align::Center);
    bind(
        sensors::memory().map(|m| {
            if m.total == 0 {
                0.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                let frac = m.used as f64 / m.total as f64;
                frac.clamp(0.0, 1.0)
            }
        }),
        &bar,
        gtk::ProgressBar::set_fraction,
    );
    row.add_suffix(&bar);

    row
}

fn build_live_swap_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("Swap").build();

    // Hide entirely when no swap is configured.
    bind(
        sensors::memory().map(|m| m.swap_total > 0),
        &row,
        gtk::prelude::WidgetExt::set_visible,
    );

    bind(
        sensors::memory().map(|m| {
            if m.swap_total == 0 {
                String::new()
            } else {
                #[allow(clippy::cast_precision_loss)]
                let pct = (m.swap_used as f64 / m.swap_total as f64) * 100.0;
                format!(
                    "{} / {} ({pct:.0}%)",
                    fmt_bytes(m.swap_used),
                    fmt_bytes(m.swap_total)
                )
            }
        }),
        &row,
        |r, t| r.set_subtitle(&t),
    );

    let bar = gtk::ProgressBar::new();
    bar.add_css_class("ts-stat-progress");
    bar.set_valign(gtk::Align::Center);
    bind(
        sensors::memory().map(|m| {
            if m.swap_total == 0 {
                0.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                let frac = m.swap_used as f64 / m.swap_total as f64;
                frac.clamp(0.0, 1.0)
            }
        }),
        &bar,
        gtk::ProgressBar::set_fraction,
    );
    row.add_suffix(&bar);

    row
}

fn build_live_processes_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("Processes").build();
    let count_label = gtk::Label::new(None);
    count_label.set_valign(gtk::Align::Center);
    bind(
        sensors::process_count().map(|n| format!("{n}")),
        &count_label,
        |label, txt| label.set_text(&txt),
    );
    row.add_suffix(&count_label);
    row
}

fn build_live_gpu_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("GPU").build();

    // Hide when no GPU detected.
    bind(
        sensors::gpu().map(|g| g.is_some()),
        &row,
        gtk::prelude::WidgetExt::set_visible,
    );

    bind(
        sensors::gpu().map(|g| match g {
            Some(state) => state.name.clone(),
            None => String::new(),
        }),
        &row,
        |r, t| r.set_subtitle(&t),
    );

    let suffix = gtk::Label::new(None);
    suffix.set_valign(gtk::Align::Center);
    bind(
        sensors::gpu().map(|g| match g {
            Some(state) => match state.temperature_celsius {
                Some(t) => format!("{t:.0} \u{00b0}C"),
                None => match state.load {
                    Some(l) => format!("{:.0}%", l * 100.0),
                    None => String::new(),
                },
            },
            None => String::new(),
        }),
        &suffix,
        |label, txt| {
            label.set_text(&txt);
            label.set_visible(!txt.is_empty());
        },
    );
    row.add_suffix(&suffix);

    row
}

fn build_live_disk_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("Disk").build();
    bind(
        sensors::disk().map(|d| format!("{} mount(s)", d.mounts.len())),
        &expander,
        |r, t| r.set_subtitle(&t),
    );

    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let expander_for_bind = expander.clone();
    let rows_for_bind = rows_track.clone();
    bind(sensors::disk(), &expander, move |_, d| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            expander_for_bind.remove(&row);
        }
        let mut new_rows = Vec::with_capacity(d.mounts.len());
        for m in &d.mounts {
            let row = adw::ActionRow::builder()
                .title(&m.path)
                .activatable(false)
                .build();
            #[allow(clippy::cast_precision_loss)]
            let pct = if m.total_bytes > 0 {
                (m.used_bytes as f64 / m.total_bytes as f64) * 100.0
            } else {
                0.0
            };
            let label = gtk::Label::new(Some(&format!(
                "{} / {} ({pct:.0}%)",
                fmt_bytes(m.used_bytes),
                fmt_bytes(m.total_bytes),
            )));
            label.set_valign(gtk::Align::Center);
            row.add_suffix(&label);
            expander_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    expander
}

fn build_stats_history_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("History").build();

    group.add(&build_history_cpu_row());
    group.add(&build_history_memory_row());
    group.add(&build_history_network_row());
    group.add(&build_history_gpu_temp_row());

    group
}


fn build_history_cpu_row() -> gtk::Box {
    let (row, spark, value) = build_history_row("CPU");
    spark.set_domain_max(Some(1.0));

    let spark_clone = spark.clone();
    let value_clone = value.clone();
    bind(sensors::cpu(), &row, move |_, c: CpuLoad| {
        spark_clone.push(c.overall);
        value_clone.set_text(&format!("{:.0}%", c.overall * 100.0));
    });

    row
}

fn build_history_memory_row() -> gtk::Box {
    let (row, spark, value) = build_history_row("Memory");
    spark.set_domain_max(Some(1.0));

    let spark_clone = spark.clone();
    let value_clone = value.clone();
    bind(sensors::memory(), &row, move |_, m| {
        if m.total == 0 {
            spark_clone.push(0.0);
            value_clone.set_text("\u{2014}");
        } else {
            #[allow(clippy::cast_precision_loss)]
            let frac = (m.used as f64 / m.total as f64).clamp(0.0, 1.0);
            spark_clone.push(frac);
            value_clone.set_text(&format!("{:.0}%", frac * 100.0));
        }
    });

    row
}

fn build_history_network_row() -> gtk::Box {
    // Vertical container: [main row | detail line]
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 2);

    let (top_row, spark, value) = build_history_row("Network");
    spark.set_domain_max(None);
    value.set_text("B/s");
    outer.append(&top_row);

    // Detail line: indented to align under the sparkline column.
    // 80px (name col) + 8px (Box spacing) = 88px left margin.
    let detail = gtk::Label::new(None);
    detail.add_css_class("ts-stat-value");
    detail.set_xalign(0.0);
    detail.set_margin_start(88);
    detail.set_margin_bottom(4);
    outer.append(&detail);

    let spark_clone = spark.clone();
    let detail_clone = detail.clone();
    bind(sensors::network(), &outer, move |_, net| {
        let (rx_total, tx_total) = net
            .interfaces
            .iter()
            .filter(|i| i.name != "lo")
            .fold((0.0_f64, 0.0_f64), |(rx, tx), i| {
                (rx + i.rx_rate_bps, tx + i.tx_rate_bps)
            });
        let combined = rx_total + tx_total;
        spark_clone.push(combined);
        detail_clone.set_text(&format!(
            "\u{2193} {} \u{2191} {}",
            fmt_rate(rx_total),
            fmt_rate(tx_total)
        ));
    });

    outer
}

fn build_history_gpu_temp_row() -> gtk::Box {
    let (row, spark, value) = build_history_row("GPU temp");
    spark.set_domain_max(None);

    // Hide unless GPU is present with a temperature reading.
    bind(
        sensors::gpu().map(|g| g.and_then(|s| s.temperature_celsius).is_some()),
        &row,
        gtk::prelude::WidgetExt::set_visible,
    );

    let spark_clone = spark.clone();
    let value_clone = value.clone();
    bind(sensors::gpu(), &row, move |_, g| {
        if let Some(state) = g
            && let Some(t) = state.temperature_celsius
        {
            spark_clone.push(t);
            value_clone.set_text(&format!("{t:.0} \u{00b0}C"));
        }
    });

    row
}

fn build_stats_services_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Services").build();

    bind(
        systemd::failed_units().map(|units| {
            if units.is_empty() {
                "All services running".to_string()
            } else {
                format!("{} failed unit(s)", units.len())
            }
        }),
        &group,
        |g, txt| g.set_description(Some(&txt)),
    );

    group.add(&build_failed_units_expander());
    group
}

fn build_failed_units_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("Failed units").build();
    bind(
        systemd::failed_units().map(|u| !u.is_empty()),
        &expander,
        gtk::prelude::WidgetExt::set_visible,
    );
    bind(
        systemd::failed_units().map(|u| {
            if u.is_empty() {
                "None".to_string()
            } else {
                format!("{} unit(s)", u.len())
            }
        }),
        &expander,
        |r, t| r.set_subtitle(&t),
    );

    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let expander_for_bind = expander.clone();
    let rows_for_bind = rows_track.clone();
    bind(systemd::failed_units(), &expander, move |_, units| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            expander_for_bind.remove(&row);
        }
        let mut new_rows = Vec::with_capacity(units.len());
        for unit in &units {
            let row = adw::ActionRow::builder()
                .title(&unit.name)
                .activatable(false)
                .build();
            let subtitle = if unit.description.is_empty() {
                unit.sub_state.clone()
            } else {
                unit.description.clone()
            };
            row.set_subtitle(&subtitle);

            let pill = gtk::Label::new(Some("failed"));
            pill.set_valign(gtk::Align::Center);
            pill.add_css_class("ts-pill-error");
            row.add_suffix(&pill);

            expander_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    expander
}




// ── VPN page ──────────────────────────────────────────────────────────────────

/// Drawer page for the active VPN tunnels.
///
/// Layout: header description shows live tunnel count. Each tunnel
/// becomes one `adw::PreferencesGroup` titled by name ("wg0"),
/// subtitle by kind (e.g. `WireGuard`), with rx/tx rows and (for
/// `WireGuard`) a nested peers expander. Empty state when no tunnel up.
///
/// Backed by `hytte::services::vpn`. The page consumes the `tunnels()`
/// signal — UI layer never spawns processes.
pub fn page_vpn() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.set_spacing(16);

    let header = adw::PreferencesGroup::builder().title("VPN").build();
    bind(
        vpn::tunnels().map(|ts| match ts.len() {
            0 => "No VPN active".to_string(),
            1 => "1 tunnel up".to_string(),
            n => format!("{n} tunnels up"),
        }),
        &header,
        |g, txt| g.set_description(Some(&txt)),
    );
    column.append(&header);

    // Empty-state row, only visible when tunnels list is empty.
    let empty_group = adw::PreferencesGroup::new();
    let empty_row = adw::ActionRow::builder()
        .title("No VPN active")
        .activatable(false)
        .selectable(false)
        .build();
    empty_row.set_subtitle("Bring a WireGuard, OpenVPN, or Tailscale tunnel up to see it here.");
    empty_group.add(&empty_row);
    bind(
        vpn::tunnels().map(|ts| ts.is_empty()),
        &empty_group,
        gtk::prelude::WidgetExt::set_visible,
    );
    column.append(&empty_group);

    // Per-tunnel groups. Set is dynamic; drain & rebuild on each emission.
    let groups_track: Rc<RefCell<Vec<adw::PreferencesGroup>>> = Rc::new(RefCell::new(Vec::new()));
    let column_for_bind = column.clone();
    let groups_for_bind = groups_track.clone();
    bind(
        vpn::tunnels(),
        &column,
        move |_col, tunnels| {
            let mut tracked = groups_for_bind.borrow_mut();
            for g in tracked.drain(..) {
                column_for_bind.remove(&g);
            }
            for tunnel in &tunnels {
                let g = build_tunnel_group(tunnel);
                column_for_bind.append(&g);
                tracked.push(g);
            }
        },
    );

    finish_page(&column)
}

fn build_tunnel_group(tunnel: &vpn::Tunnel) -> adw::PreferencesGroup {
    let kind_label = match tunnel.kind {
        vpn::TunnelKind::Wireguard => "WireGuard",
        vpn::TunnelKind::Tailscale => "Tailscale",
        vpn::TunnelKind::Tun => "tun",
        vpn::TunnelKind::Tap => "tap",
    };
    let g = adw::PreferencesGroup::builder()
        .title(&tunnel.name)
        .description(kind_label)
        .build();

    let transfer_row = adw::ActionRow::builder().title("Transfer").build();
    transfer_row.set_subtitle(&format!(
        "\u{2193} {} \u{2191} {}",
        fmt_bytes(tunnel.rx_bytes),
        fmt_bytes(tunnel.tx_bytes),
    ));
    g.add(&transfer_row);

    if let Some(summary) = tunnel.summary.as_ref() {
        let summary_row = adw::ActionRow::builder().title("Status").build();
        summary_row.set_subtitle(summary);
        g.add(&summary_row);
    }

    if let Some(since) = tunnel.since {
        let since_row = adw::ActionRow::builder().title("Since").build();
        since_row.set_subtitle(&humanize_since(since));
        g.add(&since_row);
    }

    if !tunnel.peers.is_empty() {
        let peers_expander = adw::ExpanderRow::builder()
            .title(format!("Peers ({})", tunnel.peers.len()))
            .build();
        for peer in &tunnel.peers {
            peers_expander.add_row(&build_peer_row(peer));
        }
        g.add(&peers_expander);
    }

    g
}

fn build_peer_row(peer: &vpn::Peer) -> adw::ActionRow {
    let key_short: String = peer.public_key.chars().take(8).collect();
    let row = adw::ActionRow::builder().title(&key_short).build();
    row.add_css_class("ts-mono");
    let mut subtitle_parts: Vec<String> = Vec::new();
    if let Some(ep) = peer.endpoint.as_deref() {
        subtitle_parts.push(format!("via {ep}"));
    }
    if !peer.allowed_ips.is_empty() {
        subtitle_parts.push(format!("allowed: {}", peer.allowed_ips.join(", ")));
    }
    if let Some(hs) = peer.last_handshake {
        subtitle_parts.push(format!("handshake {}", humanize_since(hs)));
    } else {
        subtitle_parts.push("never handshaken".to_string());
    }
    subtitle_parts.push(format!(
        "\u{2193} {} \u{2191} {}",
        fmt_bytes(peer.rx_bytes),
        fmt_bytes(peer.tx_bytes),
    ));
    row.set_subtitle(&subtitle_parts.join(" \u{00b7} "));
    row
}


