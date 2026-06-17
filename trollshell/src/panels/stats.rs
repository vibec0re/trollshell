//! System stats drawer panel — live CPU/memory/swap/processes/GPU/disk
//! rows on top, history sparklines middle, failed-services expander
//! bottom.

use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::app_usage::{self, ProcSample};
use hytte::services::sensors::{self, CpuLoad};
use hytte::services::systemd;

use crate::components::cast;
use crate::components::format::{fmt_bytes, fmt_rate};
use crate::components::history_row::build_history_row;
use crate::components::layout::{finish_page, page_box};

pub fn panel_stats() -> gtk::Widget {
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
    group.add(&build_top_apps_expander(
        "Top apps \u{00b7} CPU",
        app_usage::top_by_cpu(),
        |s| format!("{:.0}%", s.cpu_frac * 100.0),
    ));
    group.add(&build_top_apps_expander(
        "Top apps \u{00b7} RAM",
        app_usage::top_by_mem(),
        |s| fmt_bytes(s.mem_bytes),
    ));

    group
}

/// A collapsible "Top apps" list (CPU or RAM) bound to an [`app_usage`] signal.
/// `value` formats each row's right-hand value. Mirrors
/// [`build_live_disk_expander`]'s drain-and-rebuild pattern. The `comm` is
/// rendered with markup off, so an adversarial process name can't inject Pango
/// markup (cf. #30).
fn build_top_apps_expander(
    title: &str,
    signal: impl Signal<Item = Vec<ProcSample>> + 'static,
    value: fn(&ProcSample) -> String,
) -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title(title).build();
    expander.set_expanded(true);

    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let expander_for_bind = expander.clone();
    let rows_for_bind = rows_track.clone();
    bind(signal, &expander, move |_, list| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            expander_for_bind.remove(&row);
        }
        // Collapsed summary: the heaviest entry, or an em-dash when empty.
        let subtitle = list.first().map_or_else(
            || "\u{2014}".to_string(),
            |s| format!("{} \u{00b7} {}", s.name, value(s)),
        );
        expander_for_bind.set_subtitle(&subtitle);

        let mut new_rows = Vec::with_capacity(list.len());
        for s in &list {
            let row = adw::ActionRow::builder().activatable(false).build();
            // Markup off: a process `comm` is untrusted and could otherwise
            // inject Pango markup into the title (cf. #30).
            row.set_use_markup(false);
            row.set_title(&s.name);
            if s.procs > 1 {
                row.set_subtitle(&format!("{} processes", s.procs));
            }
            let label = gtk::Label::new(Some(&value(s)));
            label.set_valign(gtk::Align::Center);
            row.add_suffix(&label);
            expander_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    expander
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
                let pct = (cast::u64_to_f64(m.used) / cast::u64_to_f64(m.total)) * 100.0;
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
                let frac = cast::u64_to_f64(m.used) / cast::u64_to_f64(m.total);
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
                let pct = (cast::u64_to_f64(m.swap_used) / cast::u64_to_f64(m.swap_total)) * 100.0;
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
                let frac = cast::u64_to_f64(m.swap_used) / cast::u64_to_f64(m.swap_total);
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
            let pct = if m.total_bytes > 0 {
                (cast::u64_to_f64(m.used_bytes) / cast::u64_to_f64(m.total_bytes)) * 100.0
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
            let frac = (cast::u64_to_f64(m.used) / cast::u64_to_f64(m.total)).clamp(0.0, 1.0);
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
