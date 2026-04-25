//! Modal page builder functions — one per category.
//!
//! Each `page_*()` fn returns a `gtk::Widget` that will be mounted as a
//! named child of the per-monitor `gtk::Stack` in `modal.rs`. Content is
//! the same as the old per-widget `detail_widget()` functions; this is a
//! structural relocation, not a content redesign.

use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;

use chrono::{DateTime, Local};
use hytte::adw::{self, prelude::*};
use hytte::futures_signals::map_ref;
use hytte::gtk::{self, gdk, glib};
use hytte::prelude::*;
use hytte::services::bluetooth::{self, Device, PairPrompt, PromptKind};
use hytte::services::brightness;
use hytte::services::mpris::{self, PlaybackStatus};
use hytte::services::networkd::{self, OperationalState};
use hytte::services::notifications;
use hytte::services::pipewire::{self, PlaybackStream, Sink, Source};
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

/// Wrap a finished page widget (Box or Grid) in an `AdwClamp` so a child
/// reporting a pathological natural width (e.g. an `AdwActionRow` subtitle
/// that ends up holding a long single-line list) can't push the
/// layer-shell modal surface to full-screen width. Belt-and-suspenders
/// against the same class of bug — individual rows should still constrain
/// themselves (multi-line subtitles, `subtitle_lines(0)`, etc.) but this
/// catches the ones that don't.
fn finish_page(content: &impl IsA<gtk::Widget>) -> gtk::Widget {
    let clamp = adw::Clamp::builder()
        .maximum_size(680)
        .tightening_threshold(560)
        .child(content)
        .build();
    clamp.upcast()
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

#[allow(clippy::too_many_lines)]
pub fn page_media() -> gtk::Widget {
    let grid = page_grid();
    grid.set_column_homogeneous(false);

    // ── Art panel (col 0) ─────────────────────────────────────────────────────
    let art_box = panel("");
    art_box.set_size_request(220, 220);
    art_box.add_css_class("ts-media-art");
    let art_image = gtk::Image::new();
    art_image.set_icon_name(Some("audio-x-generic-symbolic"));
    art_image.set_pixel_size(200);
    art_box.append(&art_image);
    grid.attach(&art_box, 0, 0, 1, 1);

    // ── Metadata + controls panel (col 1) ─────────────────────────────────────
    let info = panel("Now Playing");

    let title_label = gtk::Label::new(Some("\u{2014}"));
    title_label.add_css_class("ts-media-title");
    title_label.set_xalign(0.0);
    title_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title_label.set_max_width_chars(40);
    info.append(&title_label);

    let artist_label = gtk::Label::new(None);
    artist_label.set_xalign(0.0);
    artist_label.add_css_class("ts-media-artist");
    artist_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    artist_label.set_max_width_chars(40);
    info.append(&artist_label);

    let album_label = gtk::Label::new(None);
    album_label.set_xalign(0.0);
    album_label.add_css_class("ts-media-album");
    album_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    album_label.set_max_width_chars(40);
    info.append(&album_label);

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    controls.set_margin_top(8);
    let prev_btn = gtk::Button::from_icon_name("media-skip-backward-symbolic");
    let play_pause_btn = gtk::Button::from_icon_name("media-playback-start-symbolic");
    let next_btn = gtk::Button::from_icon_name("media-skip-forward-symbolic");
    controls.append(&prev_btn);
    controls.append(&play_pause_btn);
    controls.append(&next_btn);
    info.append(&controls);

    let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.001);
    seek.set_draw_value(false);
    seek.set_hexpand(true);
    info.append(&seek);

    let time_line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let pos_label = gtk::Label::new(Some("0:00"));
    pos_label.set_xalign(0.0);
    pos_label.set_hexpand(true);
    let len_label = gtk::Label::new(Some("0:00"));
    len_label.set_xalign(1.0);
    len_label.set_hexpand(true);
    time_line.append(&pos_label);
    time_line.append(&len_label);
    info.append(&time_line);

    grid.attach(&info, 1, 0, 1, 1);

    // ── Shared state for click handlers and signal closure ────────────────────
    let current_bus: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let current_track_id: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let current_length: Rc<Cell<u64>> = Rc::new(Cell::new(0));
    let last_art_url: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let seek_suppress: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    // Controls.
    {
        let bus = current_bus.clone();
        prev_btn.connect_clicked(move |_| {
            if let Some(b) = bus.borrow().as_ref() {
                mpris::previous(b);
            }
        });
    }
    {
        let bus = current_bus.clone();
        play_pause_btn.connect_clicked(move |_| {
            if let Some(b) = bus.borrow().as_ref() {
                mpris::play_pause(b);
            }
        });
    }
    {
        let bus = current_bus.clone();
        next_btn.connect_clicked(move |_| {
            if let Some(b) = bus.borrow().as_ref() {
                mpris::next(b);
            }
        });
    }

    // Seek-bar → SetPosition on user drag (not on programmatic updates).
    {
        let bus = current_bus.clone();
        let tid = current_track_id.clone();
        let len = current_length.clone();
        let suppress = seek_suppress.clone();
        seek.connect_value_changed(move |s| {
            if suppress.get() {
                return;
            }
            let bus_opt = bus.borrow();
            let tid_opt = tid.borrow();
            let (Some(b), Some(t)) = (bus_opt.as_ref(), tid_opt.as_ref()) else {
                return;
            };
            let pos_fraction = s.value().clamp(0.0, 1.0);
            let length = len.get();
            if length == 0 {
                return;
            }
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            let pos_us = (pos_fraction * length as f64) as i64;
            mpris::set_position(b, t, pos_us);
        });
    }

    // Signal binding — handles ALL UI updates.
    let art_image_for_bind = art_image.clone();
    let title_label_for_bind = title_label.clone();
    let artist_label_for_bind = artist_label.clone();
    let album_label_for_bind = album_label.clone();
    let prev_for_bind = prev_btn.clone();
    let pp_for_bind = play_pause_btn.clone();
    let next_for_bind = next_btn.clone();
    let seek_for_bind = seek.clone();
    let pos_label_for_bind = pos_label.clone();
    let len_label_for_bind = len_label.clone();

    bind(mpris::active_player(), &title_label, move |_, maybe_player| {
        match maybe_player {
            None => {
                *current_bus.borrow_mut() = None;
                *current_track_id.borrow_mut() = None;
                current_length.set(0);
                title_label_for_bind.set_text("No player");
                artist_label_for_bind.set_text("");
                album_label_for_bind.set_text("");
                pos_label_for_bind.set_text("0:00");
                len_label_for_bind.set_text("0:00");
                seek_suppress.set(true);
                seek_for_bind.set_value(0.0);
                seek_suppress.set(false);
                art_image_for_bind.set_paintable(None::<&gdk::Paintable>);
                art_image_for_bind.set_icon_name(Some("audio-x-generic-symbolic"));
                art_image_for_bind.set_pixel_size(200);
                prev_for_bind.set_sensitive(false);
                pp_for_bind.set_sensitive(false);
                next_for_bind.set_sensitive(false);
            }
            Some(player) => {
                *current_bus.borrow_mut() = Some(player.bus_name.clone());
                (*current_track_id.borrow_mut()).clone_from(&player.track_id);
                current_length.set(player.length_us);

                title_label_for_bind.set_text(&player.title);
                artist_label_for_bind.set_text(&player.artists);
                album_label_for_bind.set_text(&player.album);

                prev_for_bind.set_sensitive(player.can_go_previous);
                pp_for_bind.set_sensitive(player.can_play_pause);
                next_for_bind.set_sensitive(player.can_go_next);

                let icon = if player.status == PlaybackStatus::Playing {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                };
                pp_for_bind.set_icon_name(icon);

                pos_label_for_bind.set_text(&fmt_us(player.position_us));
                len_label_for_bind.set_text(&fmt_us(player.length_us));

                if player.length_us > 0 {
                    #[allow(clippy::cast_precision_loss)]
                    let frac = (player.position_us as f64) / (player.length_us as f64);
                    seek_suppress.set(true);
                    seek_for_bind.set_value(frac.clamp(0.0, 1.0));
                    seek_suppress.set(false);
                }

                // Art: only re-fetch when URL changes.
                if *last_art_url.borrow() != player.art_url {
                    (*last_art_url.borrow_mut()).clone_from(&player.art_url);
                    let url = player.art_url.clone();
                    let art = art_image_for_bind.clone();
                    glib::MainContext::default().spawn_local(async move {
                        if let Some(bytes) = mpris::art_for_url(&url).await {
                            let glib_bytes = glib::Bytes::from(&bytes);
                            if let Ok(texture) = gdk::Texture::from_bytes(&glib_bytes) {
                                art.set_pixel_size(-1);
                                art.set_paintable(Some(&texture));
                                art.set_size_request(200, 200);
                            }
                        }
                    });
                }
            }
        }
    });

    finish_page(&grid)
}

fn fmt_us(us: u64) -> String {
    let secs = us / 1_000_000;
    let m = secs / 60;
    let s = secs % 60;
    format!("{m}:{s:02}")
}

// ── Network page ──────────────────────────────────────────────────────────────

pub fn page_network() -> gtk::Widget {
    let grid = page_grid();

    let conn_panel = panel("Connection");
    conn_panel.append(&build_connection_group());
    grid.attach(&conn_panel, 0, 0, 1, 1);

    let traffic_panel = panel("Traffic");
    traffic_panel.append(&build_traffic_group());
    grid.attach(&traffic_panel, 1, 0, 1, 1);

    let wifi_panel = panel("Wi-Fi");
    append_wifi_section(&wifi_panel);
    grid.attach(&wifi_panel, 0, 1, 2, 1);

    finish_page(&grid)
}

fn build_connection_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();

    let primary_row = adw::ActionRow::builder().title("Primary").build();
    bind(
        networkd::primary().map(|p| match p {
            Some(link) => link.name,
            None => "(none)".to_string(),
        }),
        &primary_row,
        |row, name| row.set_title(&name),
    );
    bind(
        networkd::primary().map(|p| match p {
            Some(link) => describe_state(link.operational).to_string(),
            None => "Disconnected".to_string(),
        }),
        &primary_row,
        |row, text| row.set_subtitle(&text),
    );
    group.add(&primary_row);

    let links_row = adw::ActionRow::builder().title("Links").build();
    // Multi-line subtitle: one link per line. Comma-joined produced a
    // single very wide subtitle (AdwActionRow subtitles don't ellipsize)
    // which pushed the whole modal to full-screen width on systems with
    // many interfaces (wlp/eth/virbr/docker/veth/...).
    links_row.set_subtitle_lines(0);
    bind(
        networkd::links().map(|ls| {
            let lines: Vec<String> = ls
                .iter()
                .map(|l| format!("{} ({})", l.name, describe_state(l.operational)))
                .collect();
            if lines.is_empty() {
                "(none)".to_string()
            } else {
                lines.join("\n")
            }
        }),
        &links_row,
        |row, text| row.set_subtitle(&text),
    );
    group.add(&links_row);

    let dns_row = adw::ActionRow::builder().title("DNS").build();
    bind(
        resolved::dns().map(|state| {
            if state.configured() {
                format!("{} server(s)", state.servers.len())
            } else {
                "Not configured".to_string()
            }
        }),
        &dns_row,
        |row, text| row.set_subtitle(&text),
    );
    group.add(&dns_row);

    group
}

fn build_traffic_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();

    let rate_row = adw::ActionRow::builder().title("Live").build();
    rate_row.set_subtitle_lines(0);
    bind(
        sensors::network().map(|net| {
            let parts: Vec<String> = net
                .interfaces
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
                .collect();
            if parts.is_empty() {
                "(no active interfaces)".to_string()
            } else {
                parts.join("\n")
            }
        }),
        &rate_row,
        |row, text| row.set_subtitle(&text),
    );
    group.add(&rate_row);

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

    let networks_group = adw::PreferencesGroup::new();
    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    scrolled.set_child(Some(&networks_group));
    column.append(&scrolled);

    let group_for_bind = networks_group.clone();
    let rows_track_for_bind = rows_track.clone();
    bind(wifi::networks(), &networks_group, move |_, nets| {
        // PreferencesGroup has no first_child traversal API for its rows;
        // track and remove explicitly.
        for row in rows_track_for_bind.borrow_mut().drain(..) {
            group_for_bind.remove(&row);
        }
        let mut new_rows = Vec::with_capacity(nets.len());
        for net in nets {
            let row = build_network_row(&net);
            group_for_bind.add(&row);
            new_rows.push(row);
        }
        *rows_track_for_bind.borrow_mut() = new_rows;
    });
}

fn build_network_row(net: &wifi::WifiNetwork) -> adw::ActionRow {
    let suffix_text = if net.connected {
        "Connected"
    } else if net.known {
        "Known"
    } else if net.security == "open" {
        "Open"
    } else {
        &net.security
    };
    let row = adw::ActionRow::builder()
        .title(&net.ssid)
        .subtitle(suffix_text)
        .activatable(true)
        .build();

    let icon = gtk::Image::from_icon_name(signal_icon(net.signal_dbm));
    row.add_prefix(&icon);

    if net.security != "open" {
        let lock = gtk::Image::from_icon_name("system-lock-screen-symbolic");
        lock.add_css_class("dim-label");
        lock.set_tooltip_text(Some(&net.security));
        row.add_suffix(&lock);
    }

    let path = net.path.clone();
    let connected = net.connected;
    row.connect_activated(move |_| {
        if connected {
            wifi::disconnect();
        } else {
            wifi::connect_network(&path);
        }
    });

    row
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

    column.append(&build_bluetooth_header());
    column.append(&build_pair_prompt_banner());
    column.append(&build_bluetooth_controls());
    column.append(&build_bluetooth_device_groups());

    finish_page(&column)
}

/// Top header row: "Bluetooth" title with adapter name as subtitle and a
/// proper `GtkSwitch` for Power. Wrapped in an `AdwPreferencesGroup` so it gets
/// the boxed-list look every other row uses.
fn build_bluetooth_header() -> gtk::Widget {
    let group = adw::PreferencesGroup::new();

    let row = adw::ActionRow::builder().title("Bluetooth").build();
    bind(
        bluetooth::adapter().map(|a| match a {
            Some(ad) => ad.name,
            None => "No Bluetooth adapter".to_string(),
        }),
        &row,
        |w, name| w.set_subtitle(&name),
    );

    let power_switch = gtk::Switch::new();
    power_switch.set_valign(gtk::Align::Center);
    bind(
        bluetooth::adapter().map(|a| a.is_some()),
        &power_switch,
        gtk::prelude::WidgetExt::set_sensitive,
    );
    bind(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.powered)),
        &power_switch,
        |w, on| {
            if w.is_active() != on {
                w.set_active(on);
            }
        },
    );
    power_switch.connect_active_notify(|sw| {
        bluetooth::set_powered(sw.is_active());
    });

    row.add_suffix(&power_switch);
    row.set_activatable_widget(Some(&power_switch));
    group.add(&row);

    group.upcast()
}

/// Adapter sub-controls: Discoverable + Scan. Disabled when the adapter is
/// off or absent, so toggling Power is the obvious entry point.
fn build_bluetooth_controls() -> gtk::Widget {
    let group = adw::PreferencesGroup::new();
    bind(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.powered)),
        &group,
        gtk::prelude::WidgetExt::set_sensitive,
    );

    // Discoverable
    let disc_row = adw::ActionRow::builder().title("Discoverable").build();
    bind(
        bluetooth::adapter().map(|a| match a {
            Some(ad) if ad.discoverable && !ad.name.is_empty() => {
                format!("Visible as \u{201c}{}\u{201d}", ad.name)
            }
            Some(ad) if ad.discoverable => "Visible to other devices".to_string(),
            _ => "Hidden from other devices".to_string(),
        }),
        &disc_row,
        |row, text| row.set_subtitle(&text),
    );
    let disc_switch = gtk::Switch::new();
    disc_switch.set_valign(gtk::Align::Center);
    bind(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.discoverable)),
        &disc_switch,
        |w, on| {
            if w.is_active() != on {
                w.set_active(on);
            }
        },
    );
    disc_switch.connect_active_notify(|sw| {
        bluetooth::set_discoverable(sw.is_active());
    });
    disc_row.add_suffix(&disc_switch);
    disc_row.set_activatable_widget(Some(&disc_switch));
    group.add(&disc_row);

    // Scan with inline spinner showing live progress.
    let scan_row = adw::ActionRow::builder().title("Scan for devices").build();

    let spinner = gtk::Spinner::new();
    spinner.set_valign(gtk::Align::Center);
    bind(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.discovering)),
        &spinner,
        |w, on| {
            w.set_spinning(on);
            w.set_visible(on);
        },
    );
    scan_row.add_suffix(&spinner);

    let scan_btn = gtk::ToggleButton::new();
    scan_btn.set_valign(gtk::Align::Center);
    bind(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.discovering)),
        &scan_btn,
        |w, discovering| {
            if w.is_active() != discovering {
                w.set_active(discovering);
            }
            w.set_label(if discovering { "Stop" } else { "Scan" });
        },
    );
    scan_btn.connect_toggled(|btn| {
        if btn.is_active() {
            bluetooth::start_discovery();
        } else {
            bluetooth::stop_discovery();
        }
    });
    scan_row.add_suffix(&scan_btn);
    scan_row.set_activatable_widget(Some(&scan_btn));
    group.add(&scan_row);

    group.upcast()
}

/// Container holding three boxed-list groups (Connected / Paired /
/// Available). Each group is rebuilt on every `devices()`/`device_actions()`
/// emission. Empty groups are omitted entirely so the page doesn't show
/// dangling section headers.
fn build_bluetooth_device_groups() -> gtk::Widget {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let outer_for_bind = outer.clone();

    let combined = map_ref! {
        let devs = bluetooth::devices(),
        let actions = bluetooth::device_actions() => {
            (devs.clone(), actions.clone())
        }
    };

    bind(combined, &outer, move |_, (devs, actions)| {
        while let Some(child) = outer_for_bind.first_child() {
            outer_for_bind.remove(&child);
        }
        let mut connected = Vec::new();
        let mut paired = Vec::new();
        let mut available = Vec::new();
        for dev in &devs {
            if dev.connected {
                connected.push(dev);
            } else if dev.paired {
                paired.push(dev);
            } else {
                available.push(dev);
            }
        }
        for (title, group_devs) in [
            ("Connected", connected),
            ("Paired", paired),
            ("Available", available),
        ] {
            if group_devs.is_empty() {
                continue;
            }
            let group = adw::PreferencesGroup::builder().title(title).build();
            for dev in &group_devs {
                let is_busy = actions.contains(&dev.path);
                let row = build_device_row(dev, is_busy);
                group.add(&row);
            }
            outer_for_bind.append(&group);
        }
    });

    outer.upcast()
}

/// Banner shown above the device list while `BlueZ`'s `Agent1` callback is
/// waiting on the user to accept or reject a pairing. Hidden otherwise.
fn build_pair_prompt_banner() -> gtk::Widget {
    let prompt_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    prompt_box.add_css_class("ts-bluetooth-prompt");
    prompt_box.set_visible(false);
    let prompt_box_for_bind = prompt_box.clone();
    bind(
        bluetooth::pair_prompts(),
        &prompt_box,
        move |_, prompt: Option<PairPrompt>| {
            while let Some(child) = prompt_box_for_bind.first_child() {
                prompt_box_for_bind.remove(&child);
            }
            let Some(p) = prompt else {
                prompt_box_for_bind.set_visible(false);
                return;
            };
            prompt_box_for_bind.set_visible(true);
            populate_pair_prompt(&prompt_box_for_bind, &p);
        },
    );
    prompt_box.upcast()
}

fn populate_pair_prompt(container: &gtk::Box, p: &PairPrompt) {
    let title = gtk::Label::new(Some(&format!("Pair with {}?", p.alias)));
    title.set_xalign(0.0);
    title.add_css_class("ts-bluetooth-prompt-title");
    container.append(&title);

    let detail_text = match (p.kind, p.passkey) {
        (PromptKind::ConfirmPasskey, Some(code)) => {
            format!("Code: {code:06}\nMatch this on the other device, then Confirm.")
        }
        (PromptKind::ConfirmPasskey, None) => "Confirm pairing.".to_string(),
        (PromptKind::Authorize, _) => "Allow this device to pair with you.".to_string(),
        (PromptKind::EnterPinCode, _) => {
            "Enter the PIN shown on the other device.".to_string()
        }
        (PromptKind::EnterPasskey, _) => {
            "Enter the numeric passkey shown on the other device.".to_string()
        }
    };
    let detail = gtk::Label::new(Some(&detail_text));
    detail.set_xalign(0.0);
    detail.set_wrap(true);
    detail.add_css_class("ts-bluetooth-prompt-detail");
    container.append(&detail);

    match p.kind {
        PromptKind::ConfirmPasskey | PromptKind::Authorize => {
            container.append(&build_yes_no_row());
        }
        PromptKind::EnterPinCode => {
            container.append(&build_text_entry_row(false));
        }
        PromptKind::EnterPasskey => {
            container.append(&build_text_entry_row(true));
        }
    }
}

fn build_yes_no_row() -> gtk::Box {
    let btn_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let confirm_btn = gtk::Button::with_label("Confirm");
    confirm_btn.add_css_class("suggested-action");
    confirm_btn.connect_clicked(|_| bluetooth::respond_to_prompt(true));
    let reject_btn = gtk::Button::with_label("Reject");
    reject_btn.add_css_class("destructive-action");
    reject_btn.connect_clicked(|_| bluetooth::respond_to_prompt(false));
    btn_row.append(&confirm_btn);
    btn_row.append(&reject_btn);
    btn_row
}

/// Entry + Submit/Cancel row for legacy `RequestPinCode` / `RequestPasskey`.
/// `numeric_only` switches input filtering so passkey entries can't contain
/// non-digits — `BlueZ` would reject the malformed value anyway.
fn build_text_entry_row(numeric_only: bool) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);

    let entry = gtk::Entry::new();
    entry.set_hexpand(true);
    if numeric_only {
        entry.set_input_purpose(gtk::InputPurpose::Digits);
        entry.set_max_length(6);
        entry.set_placeholder_text(Some("0–999999"));
    } else {
        entry.set_max_length(16);
        entry.set_placeholder_text(Some("PIN"));
    }
    entry.set_activates_default(false);
    row.append(&entry);

    let submit_btn = gtk::Button::with_label("Submit");
    submit_btn.add_css_class("suggested-action");
    let entry_for_submit = entry.clone();
    submit_btn.connect_clicked(move |_| submit_entry(&entry_for_submit, numeric_only));
    let entry_for_activate = entry.clone();
    entry.connect_activate(move |_| submit_entry(&entry_for_activate, numeric_only));
    row.append(&submit_btn);

    let cancel_btn = gtk::Button::with_label("Cancel");
    cancel_btn.add_css_class("destructive-action");
    cancel_btn.connect_clicked(|_| bluetooth::respond_to_prompt(false));
    row.append(&cancel_btn);

    row
}

fn submit_entry(entry: &gtk::Entry, numeric_only: bool) {
    let text = entry.text().to_string();
    if numeric_only {
        match text.trim().parse::<u32>() {
            Ok(n) => bluetooth::submit_passkey(n),
            // Empty / non-numeric → reject so `BlueZ` doesn't see junk.
            Err(_) => bluetooth::respond_to_prompt(false),
        }
    } else {
        bluetooth::submit_pin(text);
    }
}

fn build_device_row(dev: &Device, is_busy: bool) -> adw::ActionRow {
    let subtitle = if dev.connected {
        "Connected"
    } else if dev.paired {
        "Paired"
    } else {
        "Tap to pair"
    };
    let row = adw::ActionRow::builder()
        .title(&dev.alias)
        .subtitle(subtitle)
        .activatable(true)
        .build();
    row.set_sensitive(!is_busy);
    if !dev.address.is_empty() {
        row.set_tooltip_text(Some(&dev.address));
    }

    // Prefix: device icon, or spinner while a D-Bus call is in flight.
    if is_busy {
        let spinner = gtk::Spinner::new();
        spinner.set_spinning(true);
        row.add_prefix(&spinner);
    } else {
        let icon_name = if dev.icon.is_empty() {
            "bluetooth-symbolic"
        } else {
            &dev.icon
        };
        let img = gtk::Image::from_icon_name(icon_name);
        row.add_prefix(&img);
    }

    // Battery suffix when reported.
    if let Some(pct) = dev.battery {
        let battery_lbl = gtk::Label::new(Some(&format!("{pct}%")));
        battery_lbl.add_css_class("dim-label");
        battery_lbl.set_tooltip_text(Some(&format!("Battery {pct}%")));
        row.add_suffix(&battery_lbl);
    }

    // Trust indicator: read-only star at row level. Actual toggle lives in
    // the ⋮ popover so it can't be hit by accident while reaching for
    // connect/disconnect.
    if dev.paired && dev.trusted {
        let star = gtk::Image::from_icon_name("starred-symbolic");
        star.set_tooltip_text(Some("Trusted — auto-reconnects"));
        star.add_css_class("dim-label");
        row.add_suffix(&star);
    }

    // ⋮ menu with Trust/Untrust + Forget. Only present on paired devices.
    if dev.paired {
        row.add_suffix(&build_device_menu(dev, is_busy));
    }

    // Row click → primary action (pair/connect/disconnect). Trust + Forget
    // are deliberately *not* reachable from this gesture so a misclick
    // can't untrust or unpair.
    let path = dev.path.clone();
    let connected = dev.connected;
    let paired = dev.paired;
    row.connect_activated(move |_| {
        if connected {
            bluetooth::disconnect_device(&path);
        } else if paired {
            bluetooth::connect_device(&path);
        } else {
            bluetooth::pair_device(&path);
        }
    });

    row
}

/// Per-device "⋮" popover menu. Holds Trust/Untrust and Forget so the
/// row's primary activation gesture can stay focused on connect/pair
/// without surfacing destructive controls in the click target.
fn build_device_menu(dev: &Device, is_busy: bool) -> gtk::MenuButton {
    let menu_btn = gtk::MenuButton::new();
    menu_btn.set_icon_name("view-more-symbolic");
    menu_btn.add_css_class("flat");
    menu_btn.set_valign(gtk::Align::Center);
    menu_btn.set_sensitive(!is_busy);
    menu_btn.set_tooltip_text(Some("Device options"));

    let popover = gtk::Popover::new();
    let pop_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    pop_box.set_margin_start(6);
    pop_box.set_margin_end(6);
    pop_box.set_margin_top(6);
    pop_box.set_margin_bottom(6);

    let trust_lbl = if dev.trusted { "Untrust" } else { "Trust" };
    let trust_btn = gtk::Button::with_label(trust_lbl);
    trust_btn.add_css_class("flat");
    let path_t = dev.path.clone();
    let was_trusted = dev.trusted;
    let popover_for_trust = popover.clone();
    trust_btn.connect_clicked(move |_| {
        bluetooth::set_trusted(&path_t, !was_trusted);
        popover_for_trust.popdown();
    });
    pop_box.append(&trust_btn);

    let forget_btn = gtk::Button::with_label("Forget");
    forget_btn.add_css_class("flat");
    forget_btn.add_css_class("destructive-action");
    let path_f = dev.path.clone();
    let popover_for_forget = popover.clone();
    forget_btn.connect_clicked(move |_| {
        bluetooth::remove_device(&path_f);
        popover_for_forget.popdown();
    });
    pop_box.append(&forget_btn);

    popover.set_child(Some(&pop_box));
    menu_btn.set_popover(Some(&popover));
    menu_btn
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

    // Grid of vertical mini-bars — one per logical core. Rebuild when the
    // core count changes (rare), otherwise just update fractions in place.
    let cores_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    cores_row.add_css_class("ts-cores-row");
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
    cpu.append(&cores_row);

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

    finish_page(&grid)
}

// ── Audio page ────────────────────────────────────────────────────────────────

pub fn page_audio() -> gtk::Widget {
    let column = page_box();

    column.append(&audio_section("Output", &build_sink_list()));
    column.append(&audio_section("Input", &build_source_list()));
    column.append(&audio_section("Playback", &build_playback_list()));

    finish_page(&column)
}

/// Wrap a section title + `boxed-list`-styled `ListBox` in a vertical Box so
/// every audio section has the same Adwaita-style framing.
fn audio_section(title: &str, list: &gtk::ListBox) -> gtk::Box {
    let section = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let title_lbl = gtk::Label::new(Some(title));
    title_lbl.add_css_class("heading");
    title_lbl.set_xalign(0.0);
    section.append(&title_lbl);
    section.append(list);
    section
}

fn boxed_list() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list
}

fn build_sink_list() -> gtk::ListBox {
    let list = boxed_list();
    let list_for_bind = list.clone();
    bind(pipewire::sinks(), &list, move |_, sinks: Vec<Sink>| {
        while let Some(child) = list_for_bind.first_child() {
            list_for_bind.remove(&child);
        }
        for s in &sinks {
            list_for_bind.append(&sink_row(s));
        }
    });
    list
}

fn build_source_list() -> gtk::ListBox {
    let list = boxed_list();
    let list_for_bind = list.clone();
    bind(
        pipewire::sources(),
        &list,
        move |_, sources: Vec<Source>| {
            while let Some(child) = list_for_bind.first_child() {
                list_for_bind.remove(&child);
            }
            for s in &sources {
                list_for_bind.append(&source_row(s));
            }
        },
    );
    list
}

fn build_playback_list() -> gtk::ListBox {
    let list = boxed_list();
    let list_for_bind = list.clone();
    bind(
        pipewire::playback_streams(),
        &list,
        move |_, streams: Vec<PlaybackStream>| {
            while let Some(child) = list_for_bind.first_child() {
                list_for_bind.remove(&child);
            }
            if streams.is_empty() {
                let placeholder = gtk::Label::new(Some("No active streams"));
                placeholder.set_xalign(0.0);
                placeholder.add_css_class("dim-label");
                placeholder.set_margin_start(12);
                placeholder.set_margin_end(12);
                placeholder.set_margin_top(8);
                placeholder.set_margin_bottom(8);
                list_for_bind.append(&placeholder);
            } else {
                for s in &streams {
                    list_for_bind.append(&stream_row(s));
                }
            }
        },
    );
    list
}

fn sink_row(s: &Sink) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("ts-audio-row");
    if s.is_default {
        row.add_css_class("default");
    }

    // Radio indicator / default button.
    let radio_lbl = gtk::Label::new(Some(if s.is_default { "\u{25cf}" } else { "\u{25cb}" }));
    let radio_btn = gtk::Button::new();
    radio_btn.set_child(Some(&radio_lbl));
    radio_btn.add_css_class("ts-audio-default-btn");
    if s.is_default {
        radio_btn.add_css_class("active");
    }
    let sink_name_for_click = s.name.clone();
    radio_btn.connect_clicked(move |_| {
        pipewire::set_default_sink(&sink_name_for_click);
    });
    row.append(&radio_btn);

    // Name / description label.
    let desc = if s.description.len() > 40 {
        format!("{}…", &s.description[..39])
    } else {
        s.description.clone()
    };
    let name_lbl = gtk::Label::new(Some(&desc));
    name_lbl.set_xalign(0.0);
    name_lbl.set_hexpand(true);
    name_lbl.add_css_class("ts-audio-row-name");
    if !s.is_default {
        name_lbl.add_css_class("dim");
    }
    if s.description.len() > 40 {
        name_lbl.set_tooltip_text(Some(&s.description));
    }
    row.append(&name_lbl);

    // Volume slider.
    let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
    slider.set_draw_value(false);
    slider.set_hexpand(true);
    slider.set_size_request(160, -1);
    slider.set_value(s.volume);

    let sink_name_for_slider = s.name.clone();
    slider.connect_value_changed(move |sl| {
        pipewire::set_sink_volume(&sink_name_for_slider, sl.value());
    });
    // No signal bind here — we rebuild the whole row on each poll emission.
    row.append(&slider);

    // Mute button.
    let mute_btn = gtk::Button::from_icon_name("audio-volume-muted-symbolic");
    mute_btn.add_css_class("ts-audio-mute-btn");
    if s.muted {
        mute_btn.add_css_class("muted");
    }
    let muted_cell = Rc::new(Cell::new(s.muted));
    let sink_name_for_mute = s.name.clone();
    mute_btn.connect_clicked(move |btn| {
        let new_mute = !muted_cell.get();
        muted_cell.set(new_mute);
        pipewire::set_sink_mute(&sink_name_for_mute, new_mute);
        if new_mute {
            btn.add_css_class("muted");
        } else {
            btn.remove_css_class("muted");
        }
    });
    row.append(&mute_btn);

    row.upcast()
}

fn source_row(s: &Source) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("ts-audio-row");
    if s.is_default {
        row.add_css_class("default");
    }

    // Radio indicator / default button.
    let radio_lbl = gtk::Label::new(Some(if s.is_default { "\u{25cf}" } else { "\u{25cb}" }));
    let radio_btn = gtk::Button::new();
    radio_btn.set_child(Some(&radio_lbl));
    radio_btn.add_css_class("ts-audio-default-btn");
    if s.is_default {
        radio_btn.add_css_class("active");
    }
    let source_name_for_click = s.name.clone();
    radio_btn.connect_clicked(move |_| {
        pipewire::set_default_source(&source_name_for_click);
    });
    row.append(&radio_btn);

    // Name / description label.
    let desc = if s.description.len() > 40 {
        format!("{}…", &s.description[..39])
    } else {
        s.description.clone()
    };
    let name_lbl = gtk::Label::new(Some(&desc));
    name_lbl.set_xalign(0.0);
    name_lbl.set_hexpand(true);
    name_lbl.add_css_class("ts-audio-row-name");
    if !s.is_default {
        name_lbl.add_css_class("dim");
    }
    if s.description.len() > 40 {
        name_lbl.set_tooltip_text(Some(&s.description));
    }
    row.append(&name_lbl);

    // Volume slider.
    let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
    slider.set_draw_value(false);
    slider.set_hexpand(true);
    slider.set_size_request(160, -1);
    slider.set_value(s.volume);

    let source_name_for_slider = s.name.clone();
    slider.connect_value_changed(move |sl| {
        pipewire::set_source_volume(&source_name_for_slider, sl.value());
    });
    row.append(&slider);

    // Mute button.
    let mute_btn = gtk::Button::from_icon_name("audio-volume-muted-symbolic");
    mute_btn.add_css_class("ts-audio-mute-btn");
    if s.muted {
        mute_btn.add_css_class("muted");
    }
    let muted_cell = Rc::new(Cell::new(s.muted));
    let source_name_for_mute = s.name.clone();
    mute_btn.connect_clicked(move |btn| {
        let new_mute = !muted_cell.get();
        muted_cell.set(new_mute);
        pipewire::set_source_mute(&source_name_for_mute, new_mute);
        if new_mute {
            btn.add_css_class("muted");
        } else {
            btn.remove_css_class("muted");
        }
    });
    row.append(&mute_btn);

    row.upcast()
}

fn stream_row(s: &PlaybackStream) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("ts-audio-row");

    // Spacer matching radio button width so labels align with sink/source rows.
    let spacer = gtk::Label::new(Some("  "));
    spacer.add_css_class("ts-audio-default-btn");
    row.append(&spacer);

    // App name label.
    let app_name = if s.app_name.len() > 40 {
        format!("{}…", &s.app_name[..39])
    } else {
        s.app_name.clone()
    };
    let name_lbl = gtk::Label::new(Some(&app_name));
    name_lbl.set_xalign(0.0);
    name_lbl.set_hexpand(true);
    name_lbl.add_css_class("ts-audio-row-name");
    if s.app_name.len() > 40 {
        name_lbl.set_tooltip_text(Some(&s.app_name));
    }
    row.append(&name_lbl);

    // Volume slider.
    let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
    slider.set_draw_value(false);
    slider.set_hexpand(true);
    slider.set_size_request(160, -1);
    slider.set_value(s.volume);

    let stream_id = s.id;
    slider.connect_value_changed(move |sl| {
        pipewire::set_stream_volume(stream_id, sl.value());
    });
    row.append(&slider);

    // Mute button.
    let mute_btn = gtk::Button::from_icon_name("audio-volume-muted-symbolic");
    mute_btn.add_css_class("ts-audio-mute-btn");
    if s.muted {
        mute_btn.add_css_class("muted");
    }
    let muted_cell = Rc::new(Cell::new(s.muted));
    mute_btn.connect_clicked(move |btn| {
        let new_mute = !muted_cell.get();
        muted_cell.set(new_mute);
        pipewire::set_stream_mute(stream_id, new_mute);
        if new_mute {
            btn.add_css_class("muted");
        } else {
            btn.remove_css_class("muted");
        }
    });
    row.append(&mute_btn);

    row.upcast()
}

// ── Power page ────────────────────────────────────────────────────────────────

pub fn page_power() -> gtk::Widget {
    let grid = page_grid();

    // ── Battery panel (col 0) ─────────────────────────────────────────────────
    let battery = panel("Battery");

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
    battery.append(&battery_group);
    grid.attach(&battery, 0, 0, 1, 1);

    let bright = panel("Brightness");
    bright.append(&build_brightness_row());
    grid.attach(&bright, 1, 0, 1, 1);

    finish_page(&grid)
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

    // Avoid feedback: when bind() reflects external state into the slider,
    // suppress the connect_value_changed → brightness::set() path. Without
    // this the slider would echo every poll back into a brightness write.
    let suppress = Rc::new(Cell::new(false));
    let suppress_for_handler = suppress.clone();
    slider.connect_value_changed(move |s| {
        if suppress_for_handler.get() {
            return;
        }
        brightness::set(s.value());
    });
    let suppress_for_bind = suppress.clone();
    bind(brightness::current(), &slider, move |s, b| {
        if let Some(b) = b {
            suppress_for_bind.set(true);
            s.set_value(b.level);
            suppress_for_bind.set(false);
        }
        s.set_sensitive(b.is_some());
    });
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

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
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

    let list = boxed_list();
    scrolled.set_child(Some(&list));
    column.append(&scrolled);

    let list_for_signal = list.clone();
    bind(notifications::history(), &list, move |_, entries| {
        while let Some(child) = list_for_signal.first_child() {
            list_for_signal.remove(&child);
        }
        if entries.is_empty() {
            let empty = gtk::Label::new(Some("No notifications"));
            empty.add_css_class("ts-notif-empty");
            empty.set_xalign(0.0);
            empty.set_margin_start(12);
            empty.set_margin_end(12);
            empty.set_margin_top(8);
            empty.set_margin_bottom(8);
            list_for_signal.append(&empty);
            return;
        }
        for entry in &entries {
            list_for_signal.append(&build_history_row(entry));
        }
    });

    finish_page(&column)
}

fn build_history_row(entry: &notifications::HistoryEntry) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
    row.add_css_class("ts-notif-row");
    if entry.urgency == notifications::Urgency::Critical {
        row.add_css_class("critical");
    }

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let app_label = gtk::Label::new(Some(&entry.app_name));
    app_label.add_css_class("ts-notif-row-app");
    app_label.set_xalign(0.0);
    app_label.set_hexpand(true);
    header.append(&app_label);
    let time_label = gtk::Label::new(Some(&fmt_notif_time(entry.dismissed_at)));
    time_label.add_css_class("ts-notif-row-time");
    header.append(&time_label);
    row.append(&header);

    let summary = gtk::Label::new(Some(&entry.summary));
    summary.add_css_class("ts-notif-row-summary");
    summary.set_xalign(0.0);
    summary.set_wrap(true);
    row.append(&summary);

    if !entry.body.is_empty() {
        let body = gtk::Label::new(Some(&entry.body));
        body.add_css_class("ts-notif-row-body");
        body.set_xalign(0.0);
        body.set_wrap(true);
        row.append(&body);
    }

    row.upcast()
}

fn fmt_notif_time(unix_secs: u64) -> String {
    let dt = DateTime::<Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix_secs),
    );
    dt.format("%H:%M").to_string()
}
