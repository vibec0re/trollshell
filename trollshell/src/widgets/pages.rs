//! Modal page builder functions — one per category.
//!
//! Each `page_*()` fn returns a `gtk::Widget` that will be mounted as a
//! named child of the per-monitor `gtk::Stack` in `modal.rs`. Content is
//! the same as the old per-widget `detail_widget()` functions; this is a
//! structural relocation, not a content redesign.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use chrono::{DateTime, Local};
use hytte::adw::{self, prelude::*};
use hytte::futures_signals::map_ref;
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::brightness;
use hytte::services::dnd;
use hytte::services::notifications;
use hytte::services::notifications_mute;
use hytte::services::power_profiles::{self, humanize_profile};
use hytte::services::sensors::{self, CpuLoad};
use hytte::services::systemd;
use hytte::services::upower::{self, Battery, BatteryState};
use hytte::services::vpn;

use crate::components::deep_link_row::deep_link_row;
use crate::components::format::{fmt_bytes, fmt_rate, humanize_since};
use crate::components::history_row::build_history_row;
use crate::components::layout::{finish_page, page_box, page_grid, section};


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

// ── Power page ────────────────────────────────────────────────────────────────

pub fn page_power() -> gtk::Widget {
    let grid = page_grid();

    // ── Battery panel (col 0) ─────────────────────────────────────────────────
    let battery = section("Battery");

    let battery_group = adw::PreferencesGroup::new();
    let battery_row = adw::ActionRow::builder().title("Charge").build();
    bind(
        upower::battery().map(|b: Battery| describe_battery(&b)),
        &battery_row,
        |row, text| row.set_subtitle(&text),
    );
    let pct_lbl = gtk::Label::new(None);
    pct_lbl.add_css_class("dim-label");
    bind_text(
        upower::battery().map(|b: Battery| format!("{:.0}%", b.percentage)),
        &pct_lbl,
    );
    battery_row.add_suffix(&pct_lbl);
    battery_group.add(&battery_row);
    battery_group.add(&build_power_profile_expander());
    battery.append(&battery_group);
    grid.attach(&battery, 0, 0, 1, 1);

    let bright = section("Brightness");
    bright.append(&build_brightness_row());
    grid.attach(&bright, 1, 0, 1, 1);

    finish_page(&grid)
}

/// Boxed `gtk::ListBox` matching Adwaita's `boxed-list` style. Used by
/// the power and audio panels; will move out when the power panel does.
fn boxed_list() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list
}

/// Adwaita-flavoured brightness control: icon + slider + live percentage,
/// inside a single boxed-list row so it visually matches the battery panel.
fn build_brightness_row() -> gtk::ListBox {
    let list = boxed_list();

    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);

    let inner = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.set_margin_top(8);
    inner.set_margin_bottom(8);

    let icon = gtk::Image::from_icon_name("display-brightness-symbolic");
    icon.add_css_class("dim-label");
    inner.append(&icon);

    let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.05, 1.0, 0.05);
    slider.set_draw_value(false);
    slider.set_hexpand(true);

    bind_two_way(
        brightness::current(),
        &slider,
        |s, b| {
            if let Some(b) = b {
                s.set_value(b.level);
            }
            s.set_sensitive(b.is_some());
        },
        |s| s.connect_value_changed(|s| brightness::set(s.value())),
    );
    inner.append(&slider);

    let pct_lbl = gtk::Label::new(None);
    pct_lbl.add_css_class("dim-label");
    pct_lbl.set_width_chars(4);
    pct_lbl.set_xalign(1.0);
    bind_text(
        brightness::current().map(|b| match b {
            Some(b) => format!("{:.0}%", b.level * 100.0),
            None => "—".to_string(),
        }),
        &pct_lbl,
    );
    inner.append(&pct_lbl);

    row.set_child(Some(&inner));
    list.append(&row);
    list
}

// ── Battery helpers ───────────────────────────────────────────────────────────

fn build_power_profile_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder()
        .title("Power profile")
        .build();

    bind(
        power_profiles::state().map(|s| !s.available.is_empty()),
        &expander,
        gtk::prelude::WidgetExt::set_visible,
    );

    bind(
        power_profiles::state().map(|s| humanize_profile(&s.active)),
        &expander,
        |row, t| row.set_subtitle(&t),
    );

    let icon = gtk::Image::new();
    icon.set_valign(gtk::Align::Center);
    bind(
        power_profiles::state().map(|s| profile_icon_name(&s.active)),
        &icon,
        |w, name| w.set_icon_name(Some(name)),
    );
    expander.add_prefix(&icon);

    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let expander_for_bind = expander.clone();
    let rows_for_bind = rows_track.clone();
    bind(power_profiles::state(), &expander, move |_, state| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            expander_for_bind.remove(&row);
        }
        let mut new_rows = Vec::with_capacity(state.available.len());
        for profile in &state.available {
            let row = adw::ActionRow::builder()
                .title(humanize_profile(profile))
                .activatable(true)
                .build();
            if profile == &state.active {
                let check = gtk::Image::from_icon_name("object-select-symbolic");
                check.set_valign(gtk::Align::Center);
                row.add_suffix(&check);
            }
            let profile_owned = profile.clone();
            row.connect_activated(move |_| {
                power_profiles::set_active(&profile_owned);
            });
            expander_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    expander
}

fn profile_icon_name(active: &str) -> &'static str {
    match active {
        "performance" => "power-profile-performance-symbolic",
        "balanced" => "power-profile-balanced-symbolic",
        "power-saver" => "power-profile-power-saver-symbolic",
        _ => "system-run-symbolic",
    }
}

fn describe_battery(b: &Battery) -> String {
    let state = match b.state {
        BatteryState::Charging => "Charging",
        BatteryState::Discharging => "Discharging",
        BatteryState::Empty => "Empty",
        BatteryState::FullyCharged => "Fully charged",
        BatteryState::PendingCharge => "Pending charge",
        BatteryState::PendingDischarge => "Pending discharge",
        BatteryState::Unknown => "Unknown",
    };
    let remaining = match b.state {
        BatteryState::Discharging => b.time_to_empty.map(|d| fmt_dur(d, "until empty")),
        BatteryState::Charging => b.time_to_full.map(|d| fmt_dur(d, "until full")),
        _ => None,
    };
    match remaining {
        Some(r) => format!("{state} \u{2014} {r}"),
        None => state.to_string(),
    }
}

fn fmt_dur(d: std::time::Duration, suffix: &str) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    if h > 0 {
        format!("{h}h {m}m {suffix}")
    } else {
        format!("{m}m {suffix}")
    }
}

// ── Notifications history page ────────────────────────────────────────────────

pub fn page_notifications() -> gtk::Widget {
    let column = page_box();

    // Do-Not-Disturb toggle. When on, non-critical toasts are suppressed;
    // history below still records every notification.
    let dnd_group = adw::PreferencesGroup::new();
    let dnd_row = adw::ActionRow::builder()
        .title("Do Not Disturb")
        .subtitle("Suppress toast popups; history still records.")
        .build();
    let dnd_switch = gtk::Switch::new();
    dnd_switch.set_valign(gtk::Align::Center);
    bind_two_way(
        dnd::enabled(),
        &dnd_switch,
        gtk::Switch::set_active,
        |w| w.connect_active_notify(|sw| dnd::set_enabled(sw.is_active())),
    );
    dnd_row.add_suffix(&dnd_switch);
    dnd_row.set_activatable_widget(Some(&dnd_switch));
    dnd_group.add(&dnd_row);
    column.append(&dnd_group);

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    header.set_margin_top(6);
    header.set_margin_bottom(6);
    let title = gtk::Label::new(Some("History"));
    title.add_css_class("ts-popup-headline");
    title.set_xalign(0.0);
    title.set_hexpand(true);
    header.append(&title);
    let clear_btn = gtk::Button::with_label("Clear all");
    clear_btn.add_css_class("ts-notif-clear-btn");
    clear_btn.connect_clicked(|_| notifications::clear_history());
    header.append(&clear_btn);
    column.append(&header);

    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_vexpand(true);
    scrolled.set_min_content_height(380);
    scrolled.add_css_class("ts-notif-history");

    // Group entries by app_name into per-app AdwExpanderRows. Each app row's
    // mute switch controls notifications_mute for future TOASTS only —
    // history always records (`page_notifications` lives in the drawer
    // history page, so the mute switch sits next to the entries it affects
    // future emissions of).
    let groups_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
    scrolled.set_child(Some(&groups_box));
    column.append(&scrolled);

    let groups_for_signal = groups_box.clone();
    // Track the per-app ExpanderRows from the previous bind emission so we can
    // restore each row's `is_expanded()` state across the clear+rebuild —
    // otherwise an arriving notification collapses every open app row.
    let current_rows: Rc<RefCell<HashMap<String, adw::ExpanderRow>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let combined = map_ref! {
        let entries = notifications::history(),
        let muted = notifications_mute::muted_apps() => {
            (entries.clone(), muted.clone())
        }
    };
    bind(combined, &groups_box, move |_, (entries, muted)| {
        // Stash prior expand-state keyed by app_name before teardown.
        let prior_expanded: HashMap<String, bool> = current_rows
            .borrow()
            .iter()
            .map(|(name, row)| (name.clone(), row.is_expanded()))
            .collect();
        current_rows.borrow_mut().clear();
        while let Some(child) = groups_for_signal.first_child() {
            groups_for_signal.remove(&child);
        }
        if entries.is_empty() {
            let group = adw::PreferencesGroup::new();
            let empty = adw::ActionRow::builder()
                .title("No notifications")
                .build();
            group.add(&empty);
            groups_for_signal.append(&group);
            return;
        }
        // Group entries by app_name, preserving newest-first ordering by
        // walking entries (already newest-first) and pushing into per-app
        // Vec<&HistoryEntry> on first sighting. The order of apps in the UI
        // becomes "app whose newest entry is most recent first".
        let mut order: Vec<String> = Vec::new();
        let mut buckets: HashMap<String, Vec<&notifications::HistoryEntry>> = HashMap::new();
        for entry in &entries {
            // freedesktop spec allows empty `app_name`; substitute "Unknown"
            // so we don't render a blank ExpanderRow or persist "" to the
            // muted-apps file when the user toggles its switch.
            let key = if entry.app_name.trim().is_empty() {
                "Unknown".to_string()
            } else {
                entry.app_name.clone()
            };
            if !buckets.contains_key(&key) {
                order.push(key.clone());
            }
            buckets.entry(key).or_default().push(entry);
        }
        let group = adw::PreferencesGroup::new();
        for app in &order {
            let bucket = buckets.get(app).expect("bucket present for tracked app");
            let row = build_history_app_row(app, bucket, &muted);
            if prior_expanded.get(app).copied().unwrap_or(false) {
                row.set_expanded(true);
            }
            group.add(&row);
            current_rows.borrow_mut().insert(app.clone(), row);
        }
        groups_for_signal.append(&group);
    });

    finish_page(&column)
}

/// Build the `AdwExpanderRow` for a single app's history bucket.
///
/// - Title: app name.
/// - Subtitle: most-recent summary plus an entry count.
/// - Trailing action: `Switch` bound to `notifications_mute`'s set, tooltipped
///   as "Mute toasts from this app".
/// - Children: up to 20 `AdwActionRow`s (most-recent-first), each with a
///   trailing per-action button row that re-fires the original action via
///   `notifications::invoke_action`.
fn build_history_app_row(
    app: &str,
    entries: &[&notifications::HistoryEntry],
    muted: &HashSet<String>,
) -> adw::ExpanderRow {
    const MAX_PER_APP: usize = 20;

    let row = adw::ExpanderRow::builder().title(app).build();
    let count = entries.len();
    if let Some(latest) = entries.first() {
        let subtitle = if count == 1 {
            latest.summary.clone()
        } else {
            format!("{} · {} entries", latest.summary, count)
        };
        row.set_subtitle(&subtitle);
    }

    // Per-app mute switch: feeds `notifications_mute::set_app_muted` and
    // subscribes to `muted_apps()` so toggles from another monitor's drawer
    // sync into this row's switch.
    // bind_two_way blocks the user handler around the apply, so no feedback loop.
    let mute_switch = gtk::Switch::new();
    mute_switch.set_valign(gtk::Align::Center);
    mute_switch.set_tooltip_text(Some("Mute toasts from this app"));
    mute_switch.set_active(muted.contains(app));
    let app_for_bind = app.to_string();
    let app_for_handler = app.to_string();
    bind_two_way(
        notifications_mute::muted_apps().map(move |m| m.contains(&app_for_bind)),
        &mute_switch,
        gtk::Switch::set_active,
        move |w| {
            w.connect_active_notify(move |sw| {
                notifications_mute::set_app_muted(&app_for_handler, sw.is_active());
            })
        },
    );
    // Trailing widget on the header (before the expander toggle). Uses
    // `add_suffix`; `add_action` was deprecated in libadwaita 1.4.
    row.add_suffix(&mute_switch);

    for entry in entries.iter().take(MAX_PER_APP) {
        row.add_row(&build_history_action_row(entry));
    }

    row
}

/// Per-notification `AdwActionRow` showing summary + body + action buttons.
/// Action buttons re-invoke `notifications::invoke_action` (the originating
/// app filters by `id`). Capped at 3 visible buttons to match the toast
/// widget — chatty notifications like calendar reminders can pack many.
fn build_history_action_row(entry: &notifications::HistoryEntry) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&entry.summary)
        .build();
    if !entry.body.is_empty() {
        row.set_subtitle(&entry.body);
    }
    if entry.urgency == notifications::Urgency::Critical {
        row.add_css_class("critical");
    }

    // Time stamp on the left side as a prefix (small label).
    let time_label = gtk::Label::new(Some(&fmt_notif_time(entry.dismissed_at)));
    time_label.add_css_class("dim-label");
    time_label.set_valign(gtk::Align::Center);
    row.add_prefix(&time_label);

    // Action buttons (cap at 3 — same as toasts).
    if !entry.actions.is_empty() {
        let actions_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        actions_box.set_valign(gtk::Align::Center);
        for action in entry.actions.iter().take(3) {
            let btn = gtk::Button::with_label(&action.label);
            btn.add_css_class("flat");
            let id = entry.id;
            let key = action.key.clone();
            btn.connect_clicked(move |_| {
                notifications::invoke_action(id, &key);
            });
            actions_box.append(&btn);
        }
        row.add_suffix(&actions_box);
    }

    row
}

fn fmt_notif_time(unix_secs: u64) -> String {
    let dt = DateTime::<Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix_secs),
    );
    dt.format("%H:%M").to_string()
}

// ── Power-menu page (lock / logout / suspend / reboot / shutdown) ─────────────

/// Drawer page with system-power actions. Distinct from [`page_power`] (the
/// battery + brightness page); this one is the lock / logout / suspend /
/// reboot / shutdown menu, ordered most-common at top, most-destructive at
/// bottom. Each row is an `AdwActionRow` whose activation fires the action
/// and dismisses the drawer.
pub fn page_power_menu() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    let group = adw::PreferencesGroup::new();

    group.add(&power_action_row(
        "Lock",
        "Lock the screen",
        "system-lock-screen-symbolic",
        None,
        || {
            hytte::services::screensaver::lock();
        },
    ));

    group.add(&power_action_row(
        "Logout",
        "End the niri session",
        "system-log-out-symbolic",
        None,
        || {
            // niri's `quit` shows its own confirmation overlay, which is the
            // right UX for a destructive session-end action. Pass `true` to
            // suppress it if this row should be the single point of
            // confirmation.
            hytte::services::niri::quit(false);
        },
    ));

    group.add(&power_action_row(
        "Suspend",
        "Sleep until next interaction",
        "system-suspend-symbolic",
        None,
        || {
            hytte::services::logind::suspend();
        },
    ));

    group.add(&power_action_row(
        "Reboot",
        "Restart the system",
        "system-reboot-symbolic",
        None,
        || {
            hytte::services::logind::reboot();
        },
    ));

    group.add(&power_action_row(
        "Shutdown",
        "Power off",
        "system-shutdown-symbolic",
        Some("destructive-action"),
        || {
            hytte::services::logind::poweroff();
        },
    ));

    column.append(&group);

    finish_page(&column)
}

/// Build one power-menu action row. `css_class` is for variants like
/// `destructive-action` on Shutdown. The callback runs on activation; the
/// drawer is dismissed afterwards so the user sees their action take effect.
fn power_action_row(
    title: &str,
    subtitle: &str,
    icon_name: &str,
    css_class: Option<&str>,
    on_activate: impl Fn() + 'static,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .activatable(true)
        .build();
    let icon = gtk::Image::from_icon_name(icon_name);
    row.add_prefix(&icon);
    if let Some(class) = css_class {
        row.add_css_class(class);
    }
    row.connect_activated(move |_| {
        on_activate();
        crate::modal::dismiss_all();
    });
    row
}


// ── Settings page ─────────────────────────────────────────────────────────────

/// Drawer page exposing trollshell-wide preferences. v1 (minimal) covers two
/// knobs:
///
/// - Theme (Light / Dark) — delegated to `hytte::services::theme`, which
///   fans out across GTK4/libadwaita, legacy GTK (gsettings + settings.ini),
///   and Qt (qt[56]ct.conf). The dropdown reads the current theme once at
///   page mount and writes back on selection change; we do NOT live-track
///   external changes. Trollshell *is* the compositor session, so "follow
///   system" is meaningless — if gsettings reads back `default` (externally
///   set), the service surfaces Dark and the next user pick makes it canonical.
/// - Do Not Disturb — duplicates the toggle at the top of `page_notifications`.
///   Both bindings drive the same `dnd::set_enabled` setter and observe the
///   same `dnd::enabled` signal, so they stay in sync.
///
/// Future v1.x: bar/drawer layout, idle timeouts (#28's swayidle is currently
/// hand-edited), accent color, notification policy. See task description.
///
/// Requires the `gsettings-desktop-schemas` package on Arch for the
/// `org.gnome.desktop.interface` schema; missing schema falls back to
/// "Follow system" with a logged warning.
pub fn page_settings() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    // ── Appearance ────────────────────────────────────────────────────────
    let appearance = adw::PreferencesGroup::builder().title("Appearance").build();

    let theme_row = adw::ActionRow::builder()
        .title("Theme")
        .subtitle("Light or dark.")
        .build();

    // Order: ["Light", "Dark"] — matches `theme_from_index` mapping.
    let theme_dropdown = gtk::DropDown::from_strings(&["Light", "Dark"]);
    theme_dropdown.set_valign(gtk::Align::Center);
    theme_dropdown.set_selected(theme_to_index(hytte::services::theme::current()));
    theme_dropdown.connect_selected_notify(|dd| {
        hytte::services::theme::set(theme_from_index(dd.selected()));
    });
    theme_row.add_suffix(&theme_dropdown);
    theme_row.set_activatable_widget(Some(&theme_dropdown));
    appearance.add(&theme_row);

    column.append(&appearance);

    // ── Notifications ─────────────────────────────────────────────────────
    let notif = adw::PreferencesGroup::builder().title("Notifications").build();

    let dnd_row = adw::ActionRow::builder()
        .title("Do Not Disturb")
        .subtitle("Suppress non-critical toasts; history still records.")
        .build();
    let dnd_switch = gtk::Switch::new();
    dnd_switch.set_valign(gtk::Align::Center);
    bind_two_way(
        dnd::enabled(),
        &dnd_switch,
        gtk::Switch::set_active,
        |w| w.connect_active_notify(|sw| dnd::set_enabled(sw.is_active())),
    );
    dnd_row.add_suffix(&dnd_switch);
    dnd_row.set_activatable_widget(Some(&dnd_switch));
    notif.add(&dnd_row);

    column.append(&notif);

    // ── More ──────────────────────────────────────────────────────────────
    // Deep-link rows to drawer pages that don't have a dedicated bar chip.
    // Each row swaps the currently-open drawer to the target page via
    // `modal::switch_active` (see modal.rs) so the user stays on the same
    // monitor's drawer surface; no `&Monitor` is plumbed through here.
    let more = adw::PreferencesGroup::builder().title("More").build();

    more.add(&deep_link_row(
        "Wallpaper",
        Some("Pick a desktop background"),
        "preferences-desktop-wallpaper-symbolic",
        crate::modal::Page::Appearance,
    ));
    more.add(&deep_link_row(
        "Displays",
        Some("Output layout and resolution"),
        "video-display-symbolic",
        crate::modal::Page::Displays,
    ));
    more.add(&deep_link_row(
        "Clipboard history",
        Some("Recent copies from cliphist"),
        "edit-paste-symbolic",
        crate::modal::Page::Clipboard,
    ));

    column.append(&more);

    finish_page(&column)
}

// Theme dropdown index <-> hytte::services::theme::Theme. Order matches
// the strings passed to `gtk::DropDown::from_strings` in `page_settings`.
fn theme_from_index(i: u32) -> hytte::services::theme::Theme {
    match i {
        0 => hytte::services::theme::Theme::Light,
        _ => hytte::services::theme::Theme::Dark,
    }
}

fn theme_to_index(t: hytte::services::theme::Theme) -> u32 {
    match t {
        hytte::services::theme::Theme::Light => 0,
        hytte::services::theme::Theme::Dark => 1,
    }
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


