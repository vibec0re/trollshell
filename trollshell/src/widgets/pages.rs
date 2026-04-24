//! Modal page builder functions — one per category.
//!
//! Each `page_*()` fn returns a `gtk::Widget` that will be mounted as a
//! named child of the per-monitor `gtk::Stack` in `modal.rs`. Content is
//! the same as the old per-widget `detail_widget()` functions; this is a
//! structural relocation, not a content redesign.

use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::bluetooth::{self, Device};
use hytte::services::brightness;
use hytte::services::mpris::{self, PlaybackStatus};
use hytte::services::networkd::{self, OperationalState};
use hytte::services::pipewire::{self, Volume};
use hytte::services::resolved;
use hytte::services::sensors::{self, CpuLoad};
use hytte::services::upower::{self, Battery, BatteryState};
use hytte::services::wifi;

use super::util::{fmt_bytes, fmt_rate};

// ── helpers ───────────────────────────────────────────────────────────────────

fn page_box() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 4);
    b.add_css_class("ts-modal-page");
    b
}

/// Two-column (or more) grid for rich modal pages. Panels attach via
/// `grid.attach(&panel, col, row, 1, 1)`.
fn page_grid() -> gtk::Grid {
    let g = gtk::Grid::new();
    g.add_css_class("ts-modal-page");
    g.add_css_class("ts-page-grid");
    g.set_row_spacing(12);
    g.set_column_spacing(12);
    g.set_column_homogeneous(true);
    g
}

/// Card-style section with a title header. Caller appends content by
/// calling `outer.append(&child)` on the returned Box.
fn panel(title: &str) -> gtk::Box {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 4);
    outer.add_css_class("ts-panel");
    outer.set_hexpand(true);
    outer.set_vexpand(true);
    let title_label = gtk::Label::new(Some(title));
    title_label.add_css_class("ts-panel-title");
    title_label.set_xalign(0.0);
    outer.append(&title_label);
    outer
}

// ── Media page ────────────────────────────────────────────────────────────────

pub fn page_media() -> gtk::Widget {
    let column = page_box();

    let headline = gtk::Label::new(None);
    headline.set_xalign(0.0);
    headline.add_css_class("ts-popup-headline");

    // Controls row: prev / play-pause / next.
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let prev_btn = gtk::Button::new();
    prev_btn.set_child(Some(
        &gtk::Image::from_icon_name("media-skip-backward-symbolic"),
    ));
    let play_pause_btn = gtk::Button::new();
    play_pause_btn.set_child(Some(&gtk::Image::from_icon_name(
        "media-playback-start-symbolic",
    )));
    let next_btn = gtk::Button::new();
    next_btn.set_child(Some(
        &gtk::Image::from_icon_name("media-skip-forward-symbolic"),
    ));
    controls.append(&prev_btn);
    controls.append(&play_pause_btn);
    controls.append(&next_btn);

    let artist_title = gtk::Label::new(None);
    artist_title.set_xalign(0.0);
    artist_title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    artist_title.set_max_width_chars(50);

    let no_player = gtk::Label::new(Some("No active player"));
    no_player.set_xalign(0.0);

    column.append(&headline);
    column.append(&controls);
    column.append(&artist_title);
    column.append(&no_player);

    // Shared bus name for click handlers.
    let current_bus: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let bus_for_prev = current_bus.clone();
    prev_btn.connect_clicked(move |_| {
        if let Some(b) = bus_for_prev.borrow().as_ref() {
            mpris::previous(b);
        }
    });

    let bus_for_pp = current_bus.clone();
    play_pause_btn.connect_clicked(move |_| {
        if let Some(b) = bus_for_pp.borrow().as_ref() {
            mpris::play_pause(b);
        }
    });

    let bus_for_next = current_bus.clone();
    next_btn.connect_clicked(move |_| {
        if let Some(b) = bus_for_next.borrow().as_ref() {
            mpris::next(b);
        }
    });

    bind(
        mpris::active_player(),
        &column,
        move |_, maybe_player| {
            match maybe_player {
                None => {
                    *current_bus.borrow_mut() = None;
                    controls.set_visible(false);
                    artist_title.set_visible(false);
                    no_player.set_visible(true);
                    headline.set_text("No player");
                }
                Some(player) => {
                    *current_bus.borrow_mut() = Some(player.bus_name.clone());

                    let text = if player.artists.is_empty() {
                        player.title.clone()
                    } else {
                        format!("{} \u{2013} {}", player.artists, player.title)
                    };
                    headline.set_text(&player.title);
                    artist_title.set_text(&text);
                    artist_title.set_tooltip_text(Some(&text));

                    prev_btn.set_sensitive(player.can_go_previous);
                    play_pause_btn.set_sensitive(player.can_play_pause);
                    next_btn.set_sensitive(player.can_go_next);

                    let icon = if player.status == PlaybackStatus::Playing {
                        "media-playback-pause-symbolic"
                    } else {
                        "media-playback-start-symbolic"
                    };
                    play_pause_btn
                        .child()
                        .and_downcast::<gtk::Image>()
                        .iter()
                        .for_each(|img| img.set_icon_name(Some(icon)));

                    controls.set_visible(true);
                    artist_title.set_visible(true);
                    no_player.set_visible(false);
                }
            }
        },
    );

    column.upcast()
}

// ── Network page ──────────────────────────────────────────────────────────────

pub fn page_network() -> gtk::Widget {
    let grid = page_grid();

    // ── Connection panel (col 0, row 0) ──────────────────────────────────────
    let conn_panel = panel("Connection");

    let headline = gtk::Label::new(None);
    headline.set_xalign(0.0);
    bind_text(
        networkd::primary().map(|p| match p {
            Some(link) => format!("{}: {}", link.name, describe_state(link.operational)),
            None => "Disconnected".to_string(),
        }),
        &headline,
    );
    conn_panel.append(&headline);

    let links_label = gtk::Label::new(None);
    links_label.set_xalign(0.0);
    bind_text(
        networkd::links().map(|ls| {
            let lines: Vec<String> = ls
                .iter()
                .map(|l| format!("{} ({})", l.name, describe_state(l.operational)))
                .collect();
            lines.join("\n")
        }),
        &links_label,
    );
    conn_panel.append(&links_label);

    let dns = gtk::Label::new(None);
    dns.set_xalign(0.0);
    bind_text(
        resolved::dns().map(|state| {
            if state.configured() {
                format!("DNS: {} server(s)", state.servers.len())
            } else {
                "DNS: not configured".to_string()
            }
        }),
        &dns,
    );
    conn_panel.append(&dns);
    grid.attach(&conn_panel, 0, 0, 1, 1);

    // ── Traffic panel (col 1, row 0) ─────────────────────────────────────────
    let traffic = panel("Traffic");

    let rate = gtk::Label::new(None);
    rate.set_xalign(0.0);
    bind_text(
        sensors::network().map(|net| {
            net.interfaces
                .iter()
                .filter(|i| i.name != "lo")
                .map(|i| {
                    format!(
                        "{}: \u{2193} {} \u{2191} {}",
                        i.name,
                        fmt_rate(i.rx_rate_bps),
                        fmt_rate(i.tx_rate_bps),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }),
        &rate,
    );
    traffic.append(&rate);

    let totals = gtk::Label::new(None);
    totals.set_xalign(0.0);
    bind_text(
        sensors::network().map(|net| {
            let (rx, tx) = net
                .interfaces
                .iter()
                .filter(|i| i.name != "lo")
                .fold((0u64, 0u64), |(rx, tx), i| {
                    (rx + i.rx_bytes_total, tx + i.tx_bytes_total)
                });
            format!(
                "Total: \u{2193} {} \u{2191} {}",
                fmt_bytes(rx),
                fmt_bytes(tx),
            )
        }),
        &totals,
    );
    traffic.append(&totals);

    let conns = gtk::Label::new(None);
    conns.set_xalign(0.0);
    bind_text(
        sensors::net_connections().map(|c| {
            format!(
                "TCP: {} established, {} listening",
                c.established_total(),
                c.tcp_listen + c.tcp6_listen,
            )
        }),
        &conns,
    );
    traffic.append(&conns);
    grid.attach(&traffic, 1, 0, 1, 1);

    // ── Wi-Fi panel spanning both columns (row 1) ────────────────────────────
    let wifi_panel = panel("Wi-Fi");
    append_wifi_section(&wifi_panel);
    grid.attach(&wifi_panel, 0, 1, 2, 1);

    grid.upcast()
}

fn append_wifi_section(column: &gtk::Box) {
    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.set_margin_top(4);
    separator.set_margin_bottom(4);
    column.append(&separator);

    let wifi_headline = gtk::Label::new(None);
    wifi_headline.set_xalign(0.0);
    wifi_headline.add_css_class("ts-wifi-headline");
    bind_text(
        wifi::station().map(|s| match s {
            Some(st) => match st.connected_ssid {
                Some(ssid) => format!("Wi-Fi: {ssid}"),
                None => match st.state {
                    wifi::StationState::Connecting => "Wi-Fi: connecting\u{2026}".to_string(),
                    wifi::StationState::Roaming => "Wi-Fi: roaming".to_string(),
                    _ => "Wi-Fi: disconnected".to_string(),
                },
            },
            None => "Wi-Fi: no adapter".to_string(),
        }),
        &wifi_headline,
    );
    column.append(&wifi_headline);

    let scan_btn = gtk::Button::with_label("Scan");
    scan_btn.connect_clicked(|_| wifi::scan());
    bind(
        wifi::station().map(|s| !s.is_some_and(|st| st.scanning)),
        &scan_btn,
        gtk::prelude::WidgetExt::set_sensitive,
    );
    column.append(&scan_btn);

    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_min_content_height(160);
    scrolled.set_max_content_height(240);
    scrolled.add_css_class("ts-wifi-list");

    let networks_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    scrolled.set_child(Some(&networks_box));
    column.append(&scrolled);

    let networks_box_for_signal = networks_box.clone();
    bind(wifi::networks(), &networks_box, move |_, nets| {
        while let Some(child) = networks_box_for_signal.first_child() {
            networks_box_for_signal.remove(&child);
        }
        for net in nets {
            let row = build_network_row(&net);
            networks_box_for_signal.append(&row);
        }
    });
}

fn build_network_row(net: &wifi::WifiNetwork) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-wifi-row");
    if net.connected {
        btn.add_css_class("connected");
    }

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);

    let icon = gtk::Image::from_icon_name(signal_icon(net.signal_dbm));
    row.append(&icon);

    let label = gtk::Label::new(Some(&net.ssid));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    row.append(&label);

    let suffix = if net.connected {
        "connected".to_string()
    } else if net.known {
        "known".to_string()
    } else if net.security == "open" {
        "open".to_string()
    } else {
        net.security.clone()
    };
    let suffix_label = gtk::Label::new(Some(&suffix));
    suffix_label.add_css_class("ts-wifi-suffix");
    row.append(&suffix_label);

    btn.set_child(Some(&row));

    let path = net.path.clone();
    let connected = net.connected;
    btn.connect_clicked(move |_| {
        if connected {
            wifi::disconnect();
        } else {
            wifi::connect_network(&path);
        }
    });

    btn.upcast()
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

pub(crate) fn describe_state(s: OperationalState) -> &'static str {
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

// ── Bluetooth page ────────────────────────────────────────────────────────────

pub fn page_bluetooth() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    let headline = gtk::Label::new(None);
    headline.set_xalign(0.0);
    headline.add_css_class("ts-popup-headline");
    bind_text(
        bluetooth::adapter().map(|a| match a {
            Some(adapter) => format!(
                "{} — {}",
                adapter.name,
                if adapter.powered { "on" } else { "off" }
            ),
            None => "Bluetooth — no adapter".to_string(),
        }),
        &headline,
    );
    column.append(&headline);

    let power_btn = gtk::ToggleButton::with_label("Power");
    bind(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.powered)),
        &power_btn,
        |w, powered| {
            w.set_active(powered);
        },
    );
    power_btn.connect_toggled(|btn| {
        bluetooth::set_powered(btn.is_active());
    });
    column.append(&power_btn);

    let scan_btn = gtk::ToggleButton::new();
    bind(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.discovering)),
        &scan_btn,
        |w, discovering| {
            w.set_active(discovering);
            w.set_label(if discovering { "Stop scan" } else { "Scan\u{2026}" });
        },
    );
    scan_btn.connect_toggled(|btn| {
        if btn.is_active() {
            bluetooth::start_discovery();
        } else {
            bluetooth::stop_discovery();
        }
    });
    column.append(&scan_btn);

    let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
    column.append(&sep);

    let device_list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let device_list_for_bind = device_list.clone();

    bind(bluetooth::devices(), &device_list, move |_, devs| {
        while let Some(child) = device_list_for_bind.first_child() {
            device_list_for_bind.remove(&child);
        }
        for dev in &devs {
            let row = build_device_row(dev);
            device_list_for_bind.append(&row);
        }
    });
    column.append(&device_list);

    column.upcast()
}

fn build_device_row(dev: &Device) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-bluetooth-device");
    if dev.connected {
        btn.add_css_class("connected");
    }

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("ts-bluetooth-row");

    let icon_name = if dev.icon.is_empty() {
        "bluetooth-symbolic"
    } else {
        &dev.icon
    };
    let img = gtk::Image::from_icon_name(icon_name);
    row.append(&img);

    let alias_label = gtk::Label::new(Some(&dev.alias));
    alias_label.set_hexpand(true);
    alias_label.set_xalign(0.0);
    row.append(&alias_label);

    let state_text = if dev.connected {
        "(connected)"
    } else if dev.paired {
        "(paired)"
    } else {
        ""
    };
    if !state_text.is_empty() {
        let state_label = gtk::Label::new(Some(state_text));
        state_label.add_css_class("dim-label");
        row.append(&state_label);
    }

    btn.set_child(Some(&row));

    let path = dev.path.clone();
    let connected = dev.connected;
    btn.connect_clicked(move |_| {
        if connected {
            bluetooth::disconnect_device(&path);
        } else {
            bluetooth::connect_device(&path);
        }
    });

    btn.upcast()
}

// ── Stats page ────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
pub fn page_stats() -> gtk::Widget {
    let grid = page_grid();

    // ── CPU panel (col 0, row 0) ──────────────────────────────────────────────
    let cpu = panel("CPU");

    let cpu_headline = gtk::Label::new(None);
    cpu_headline.set_xalign(0.0);
    bind_text(
        sensors::cpu().map(|c| format!("{:.0}%", c.overall * 100.0)),
        &cpu_headline,
    );
    cpu.append(&cpu_headline);

    let cores_label = gtk::Label::new(None);
    cores_label.set_xalign(0.0);
    bind_text(
        sensors::cpu().map(|c: CpuLoad| {
            c.per_core
                .iter()
                .enumerate()
                .map(|(i, l)| format!("Core {i}: {:>3.0}%", l * 100.0))
                .collect::<Vec<_>>()
                .join("\n")
        }),
        &cores_label,
    );
    cpu.append(&cores_label);

    let cpu_temp = gtk::Label::new(None);
    cpu_temp.set_xalign(0.0);
    bind_text(
        sensors::cpu_temp().map(|t| match t.package_celsius {
            Some(c) => format!("Temp: {c:.0}\u{00b0}C"),
            None => "Temp: \u{2014}".to_string(),
        }),
        &cpu_temp,
    );
    cpu.append(&cpu_temp);
    grid.attach(&cpu, 0, 0, 1, 1);

    // ── Memory panel (col 1, row 0) ───────────────────────────────────────────
    let mem = panel("Memory");

    let mem_headline = gtk::Label::new(None);
    mem_headline.set_xalign(0.0);
    bind_text(
        sensors::memory().map(|m| {
            if m.total == 0 {
                "--%".to_string()
            } else {
                #[allow(clippy::cast_precision_loss)]
                let pct = (m.used as f64 / m.total as f64) * 100.0;
                format!("{pct:.0}%")
            }
        }),
        &mem_headline,
    );
    mem.append(&mem_headline);

    let mem_used = gtk::Label::new(None);
    mem_used.set_xalign(0.0);
    bind_text(
        sensors::memory()
            .map(|m| format!("{} / {}", fmt_bytes(m.used), fmt_bytes(m.total))),
        &mem_used,
    );
    mem.append(&mem_used);

    let mem_avail = gtk::Label::new(None);
    mem_avail.set_xalign(0.0);
    bind_text(
        sensors::memory().map(|m| format!("available: {}", fmt_bytes(m.available))),
        &mem_avail,
    );
    mem.append(&mem_avail);
    grid.attach(&mem, 1, 0, 1, 1);

    // ── GPU panel (col 0, row 1) ──────────────────────────────────────────────
    let gpu = panel("GPU");

    let gpu_name = gtk::Label::new(None);
    gpu_name.set_xalign(0.0);
    bind_text(
        sensors::gpu().map(|g| match g {
            Some(state) => state.name.clone(),
            None => "no GPU detected".to_string(),
        }),
        &gpu_name,
    );
    gpu.append(&gpu_name);

    let gpu_temp = gtk::Label::new(None);
    gpu_temp.set_xalign(0.0);
    bind_text(
        sensors::gpu().map(|g| match g.as_ref().and_then(|s| s.temperature_celsius) {
            Some(t) => format!("Temp: {t:.0}\u{00b0}C"),
            None => "Temp: \u{2014}".to_string(),
        }),
        &gpu_temp,
    );
    gpu.append(&gpu_temp);

    let gpu_load = gtk::Label::new(None);
    gpu_load.set_xalign(0.0);
    bind_text(
        sensors::gpu().map(|g| match g.as_ref().and_then(|s| s.load) {
            Some(l) => format!("Load: {:.0}%", l * 100.0),
            None => "Load: \u{2014}".to_string(),
        }),
        &gpu_load,
    );
    gpu.append(&gpu_load);

    let gpu_mem = gtk::Label::new(None);
    gpu_mem.set_xalign(0.0);
    bind_text(
        sensors::gpu().map(|g| {
            let used = g.as_ref().and_then(|s| s.memory_used_bytes);
            let total = g.as_ref().and_then(|s| s.memory_total_bytes);
            match (used, total) {
                (Some(u), Some(t)) => format!("VRAM: {} / {}", fmt_bytes(u), fmt_bytes(t)),
                _ => "VRAM: \u{2014}".to_string(),
            }
        }),
        &gpu_mem,
    );
    gpu.append(&gpu_mem);
    grid.attach(&gpu, 0, 1, 1, 1);

    // ── Disk panel (col 1, row 1) ─────────────────────────────────────────────
    let disk = panel("Disk");

    let mounts_label = gtk::Label::new(None);
    mounts_label.set_xalign(0.0);
    bind_text(
        sensors::disk().map(|d| {
            if d.mounts.is_empty() {
                return "No mounts".to_string();
            }
            d.mounts
                .iter()
                .map(|m| {
                    format!(
                        "{}: {:.0}% ({} / {})",
                        m.path,
                        m.usage * 100.0,
                        fmt_bytes(m.used_bytes),
                        fmt_bytes(m.total_bytes),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }),
        &mounts_label,
    );
    disk.append(&mounts_label);
    grid.attach(&disk, 1, 1, 1, 1);

    grid.upcast()
}

// ── Audio page ────────────────────────────────────────────────────────────────

pub fn page_audio() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    let headline = gtk::Label::new(None);
    headline.set_xalign(0.0);
    headline.add_css_class("ts-popup-headline");
    bind_text(
        pipewire::default_sink().map(|v: Volume| {
            if v.muted {
                "Muted".to_string()
            } else {
                format!("{:.0}%", v.linear * 100.0)
            }
        }),
        &headline,
    );
    column.append(&headline);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);

    let mute_btn = gtk::Button::from_icon_name("audio-volume-muted-symbolic");
    mute_btn.add_css_class("ts-mute-btn");
    bind_class(
        pipewire::default_sink().map(|v: Volume| v.muted),
        &mute_btn,
        "active",
    );
    mute_btn.connect_clicked(|_| pipewire::toggle_mute());
    row.append(&mute_btn);

    let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
    slider.set_draw_value(false);
    slider.set_hexpand(true);
    slider.set_size_request(160, -1);

    let suppress = Rc::new(Cell::new(false));

    let suppress_for_handler = suppress.clone();
    slider.connect_value_changed(move |s| {
        if suppress_for_handler.get() {
            return;
        }
        pipewire::set_volume(s.value());
    });

    let suppress_for_bind = suppress.clone();
    bind(pipewire::default_sink(), &slider, move |s, v: Volume| {
        suppress_for_bind.set(true);
        s.set_value(v.linear);
        suppress_for_bind.set(false);
    });
    row.append(&slider);

    column.append(&row);

    let device = gtk::Label::new(Some("Default sink"));
    device.set_xalign(0.0);
    column.append(&device);

    column.upcast()
}

// ── Power page ────────────────────────────────────────────────────────────────

pub fn page_power() -> gtk::Widget {
    let grid = page_grid();

    // ── Battery panel (col 0) ─────────────────────────────────────────────────
    let battery = panel("Battery");

    let battery_headline = gtk::Label::new(None);
    battery_headline.set_xalign(0.0);
    bind_text(
        upower::battery().map(|b: Battery| format!("{:.0}%", b.percentage)),
        &battery_headline,
    );
    battery.append(&battery_headline);

    let state_label = gtk::Label::new(None);
    state_label.set_xalign(0.0);
    bind_text(upower::battery().map(|b: Battery| describe_battery(&b)), &state_label);
    battery.append(&state_label);
    grid.attach(&battery, 0, 0, 1, 1);

    // ── Brightness panel (col 1) ──────────────────────────────────────────────
    let bright = panel("Brightness");

    let bright_headline = gtk::Label::new(None);
    bright_headline.set_xalign(0.0);
    bind_text(
        brightness::current().map(|b| match b {
            Some(b) => format!("{:.0}%", b.level * 100.0),
            None => "no backlight".to_string(),
        }),
        &bright_headline,
    );
    bright.append(&bright_headline);

    let bright_slider =
        gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.05, 1.0, 0.05);
    bright_slider.set_draw_value(false);
    bright_slider.set_hexpand(true);

    let suppress = Rc::new(Cell::new(false));

    let suppress_for_handler = suppress.clone();
    bright_slider.connect_value_changed(move |s| {
        if suppress_for_handler.get() {
            return;
        }
        brightness::set(s.value());
    });

    let suppress_for_bind = suppress.clone();
    bind(brightness::current(), &bright_slider, move |s, b| {
        if let Some(b) = b {
            suppress_for_bind.set(true);
            s.set_value(b.level);
            suppress_for_bind.set(false);
        }
    });
    bright.append(&bright_slider);

    let device_label = gtk::Label::new(Some("Backlight"));
    device_label.set_xalign(0.0);
    bright.append(&device_label);
    grid.attach(&bright, 1, 0, 1, 1);

    grid.upcast()
}

// ── Battery helpers ───────────────────────────────────────────────────────────

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

