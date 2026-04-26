//! Modal page builder functions — one per category.
//!
//! Each `page_*()` fn returns a `gtk::Widget` that will be mounted as a
//! named child of the per-monitor `gtk::Stack` in `modal.rs`. Content is
//! the same as the old per-widget `detail_widget()` functions; this is a
//! structural relocation, not a content redesign.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use chrono::{DateTime, Datelike, Local};
use hytte::adw::{self, prelude::*};
use hytte::futures_signals::map_ref;
use hytte::gtk::{self, gdk, gio, glib};
use hytte::prelude::*;
use hytte::services::bluetooth::{self, Device, PairPrompt, PromptKind};
use hytte::services::bluetooth_audio;
use hytte::services::brightness;
use hytte::services::calendar::{self, CalendarEvent};
use hytte::services::clipboard::{self, ClipEntry, ClipKind};
use hytte::services::displays::{self, Output};
use hytte::services::dnd;
use hytte::services::mpris::{self, PlaybackStatus};
use hytte::services::networkd::{self, OperationalState};
use hytte::services::notifications;
use hytte::services::notifications_mute;
use hytte::services::pipewire::{self, PlaybackStream, Sink, Source};
use hytte::services::power_profiles::{self, humanize_profile};
use hytte::services::resolved;
use hytte::services::sensors::{self, CpuLoad};
use hytte::services::systemd;
use hytte::services::upower::{self, Battery, BatteryState};
use hytte::services::wallpaper;
use hytte::services::wifi;
use hytte::ui::Sparkline;

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

    // Signal binding — handles ALL UI updates.
    let art_image_for_bind = art_image.clone();
    let title_label_for_bind = title_label.clone();
    let artist_label_for_bind = artist_label.clone();
    let album_label_for_bind = album_label.clone();
    let prev_for_bind = prev_btn.clone();
    let pp_for_bind = play_pause_btn.clone();
    let next_for_bind = next_btn.clone();
    let pos_label_for_bind = pos_label.clone();
    let len_label_for_bind = len_label.clone();

    // Pre-clone shared state for the seek bind_two_way below (the big bind
    // takes ownership of the originals via `move`).
    let bus_for_seek = current_bus.clone();
    let tid_for_seek = current_track_id.clone();
    let len_for_seek = current_length.clone();

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

    // Seek value mirror + user-driven SetPosition. Subscribes to active_player
    // independently of the title/art bind above; futures-signals allows
    // multiple subscribers and bind_two_way owns the user-handler block.
    bind_two_way(
        mpris::active_player().map(|maybe| {
            let Some(p) = maybe else { return 0.0; };
            if p.length_us == 0 { 0.0 } else {
                #[allow(clippy::cast_precision_loss)]
                ((p.position_us as f64) / (p.length_us as f64)).clamp(0.0, 1.0)
            }
        }),
        &seek,
        gtk::prelude::RangeExt::set_value,
        move |s| s.connect_value_changed(move |s| {
            let bus_opt = bus_for_seek.borrow();
            let tid_opt = tid_for_seek.borrow();
            let (Some(b), Some(t)) = (bus_opt.as_ref(), tid_opt.as_ref()) else {
                return;
            };
            let pos_fraction = s.value().clamp(0.0, 1.0);
            let length = len_for_seek.get();
            if length == 0 { return; }
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
            let pos_us = (pos_fraction * length as f64) as i64;
            mpris::set_position(b, t, pos_us);
        }),
    );

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
    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.set_spacing(16);

    column.append(build_connection_group_v2().upcast_ref::<gtk::Widget>());
    column.append(build_traffic_group_v2().upcast_ref::<gtk::Widget>());
    column.append(build_wifi_group_v2().upcast_ref::<gtk::Widget>());

    finish_page(&column)
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
    bind_two_way(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.powered)),
        &power_switch,
        gtk::Switch::set_active,
        |w| w.connect_active_notify(|sw| bluetooth::set_powered(sw.is_active())),
    );

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
    bind_two_way(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.discoverable)),
        &disc_switch,
        gtk::Switch::set_active,
        |w| w.connect_active_notify(|sw| bluetooth::set_discoverable(sw.is_active())),
    );
    disc_row.add_suffix(&disc_switch);
    disc_row.set_activatable_widget(Some(&disc_switch));
    group.add(&disc_row);

    // Auto-switch audio: when a BT audio device connects, make it the
    // default pipewire sink (and restore the previous one on disconnect).
    let auto_row = adw::ActionRow::builder()
        .title("Auto-switch audio")
        .subtitle("Use Bluetooth audio devices when they connect")
        .build();
    let auto_switch = gtk::Switch::new();
    auto_switch.set_valign(gtk::Align::Center);
    bind_two_way(
        bluetooth_audio::auto_switch_enabled(),
        &auto_switch,
        gtk::Switch::set_active,
        |w| w.connect_active_notify(|sw| bluetooth_audio::set_auto_switch_enabled(sw.is_active())),
    );
    auto_row.add_suffix(&auto_switch);
    auto_row.set_activatable_widget(Some(&auto_switch));
    group.add(&auto_row);

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
    bind_two_way(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.discovering)),
        &scan_btn,
        |w, discovering| {
            w.set_active(discovering);
            w.set_label(if discovering { "Stop" } else { "Scan" });
        },
        |w| w.connect_toggled(|btn| {
            if btn.is_active() {
                bluetooth::start_discovery();
            } else {
                bluetooth::stop_discovery();
            }
        }),
    );
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

/// Build a `[name | Sparkline | value]` row styled `.ts-history-row`.
/// Returns the box, the Sparkline (caller pushes samples), and the
/// value label (caller binds text on it).
fn build_history_row(name: &str) -> (gtk::Box, Sparkline, gtk::Label) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("ts-history-row");

    let name_label = gtk::Label::new(Some(name));
    name_label.add_css_class("ts-stat-name");
    name_label.set_xalign(0.0);
    name_label.set_size_request(80, -1);
    row.append(&name_label);

    let spark = Sparkline::new(60);
    spark.widget().set_hexpand(true);
    row.append(spark.widget());

    let value_label = gtk::Label::new(None);
    value_label.add_css_class("ts-stat-value");
    value_label.set_xalign(1.0);
    value_label.set_size_request(80, -1);
    row.append(&value_label);

    (row, spark, value_label)
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
    battery_group.add(&build_power_profile_expander());
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
            // right UX for a destructive session-end action. Pass
            // `--skip-confirmation` to suppress it if you want this row to
            // be the single point of confirmation.
            spawn_detached("niri", &["msg", "action", "quit"]);
        },
    ));

    group.add(&power_action_row(
        "Suspend",
        "Sleep until next interaction",
        "system-suspend-symbolic",
        None,
        || {
            spawn_detached("systemctl", &["suspend"]);
        },
    ));

    group.add(&power_action_row(
        "Reboot",
        "Restart the system",
        "system-reboot-symbolic",
        None,
        || {
            spawn_detached("systemctl", &["reboot"]);
        },
    ));

    group.add(&power_action_row(
        "Shutdown",
        "Power off",
        "system-shutdown-symbolic",
        Some("destructive-action"),
        || {
            spawn_detached("systemctl", &["poweroff"]);
        },
    ));

    column.append(&group);

    // Cancel row: just retract the drawer. ESC also closes (modal.rs handles
    // it) but a visible affordance helps when the page was opened by chip.
    let close_group = adw::PreferencesGroup::new();
    let close_row = adw::ActionRow::builder()
        .title("Close")
        .activatable(true)
        .build();
    let close_icon = gtk::Image::from_icon_name("window-close-symbolic");
    close_row.add_prefix(&close_icon);
    close_row.connect_activated(|_| {
        crate::modal::dismiss_all();
    });
    close_group.add(&close_row);
    column.append(&close_group);

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

/// Fire-and-forget process spawn for power-menu actions. systemctl calls
/// hit polkit (wired up in task #27); auth flows through the trollshell
/// polkit dialog. Errors are logged at warn level — the user already sees
/// the drawer close, so a silent failure would be confusing.
fn spawn_detached(program: &str, args: &[&str]) {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    if let Err(e) = cmd.spawn() {
        tracing::warn!(program, ?args, error = %e, "power-menu: spawn failed");
    }
}

// ── Appearance page ──────────────────────────────────────────────────────────

/// Appearance / wallpaper drawer page.
///
/// v1: a single image, applied to every output. The user picks a file with
/// `gtk::FileDialog`; the wallpaper service writes the path to
/// `~/.config/trollshell/wallpaper.path` and restarts `swaybg.service` so
/// the change takes effect immediately.
///
/// Per-output (per-monitor) wallpaper, time-of-day rotation, and an explicit
/// "Clear" button are deliberately deferred. See `etc/wallpaper/README.md`.
pub fn page_appearance() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    let group = adw::PreferencesGroup::builder().title("Wallpaper").build();

    let row = adw::ActionRow::builder().title("Image").build();
    bind(
        wallpaper::current_path().map(|p| match p {
            Some(p) => wallpaper_basename(&p),
            None => "Not set".to_string(),
        }),
        &row,
        |row, text| row.set_subtitle(&text),
    );
    // Long file paths shouldn't push the modal wide; let the subtitle wrap.
    row.set_subtitle_lines(0);

    let browse = gtk::Button::with_label("Browse\u{2026}");
    browse.set_valign(gtk::Align::Center);
    browse.add_css_class("flat");
    let browse_for_handler = browse.clone();
    browse.connect_clicked(move |_| {
        open_wallpaper_picker(&browse_for_handler);
    });
    row.add_suffix(&browse);
    row.set_activatable_widget(Some(&browse));
    group.add(&row);

    column.append(&group);
    finish_page(&column)
}

/// Last path component of a wallpaper path, with the original returned if
/// it has no separator (e.g. relative or already-bare filename).
fn wallpaper_basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |s| s.to_string_lossy().into_owned())
}

/// Open a `gtk::FileDialog` to pick a wallpaper image. On selection, hands
/// the absolute path to the wallpaper service (which persists + restarts
/// the swaybg unit). Cancellation / error is logged at debug level.
fn open_wallpaper_picker(parent_widget: &gtk::Button) {
    let dialog = gtk::FileDialog::builder()
        .title("Select wallpaper")
        .modal(true)
        .build();

    // Filter to common still-image formats. swaybg renders PNG/JPEG and
    // (with the right build) GIF/PNM/etc.; the broad filter keeps us out
    // of the business of guessing exactly what swaybg supports today.
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Images"));
    for mime in ["image/png", "image/jpeg", "image/webp", "image/bmp"] {
        filter.add_mime_type(mime);
    }
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    dialog.set_filters(Some(&filters));
    dialog.set_default_filter(Some(&filter));

    // Resolve the parent window so the dialog is modal-anchored to the
    // drawer's layer-shell surface; without it the dialog is parentless
    // and may not receive focus correctly under niri.
    let parent = parent_widget
        .root()
        .and_then(|root| root.downcast::<gtk::Window>().ok());

    dialog.open(parent.as_ref(), gio::Cancellable::NONE, |result| {
        match result {
            Ok(file) => {
                if let Some(path) = file.path() {
                    let s = path.to_string_lossy().into_owned();
                    wallpaper::set_path(&s);
                } else {
                    tracing::warn!("wallpaper picker: selection had no local path");
                }
            }
            Err(e) => {
                // gtk's "Dismissed by user" comes back as an error too —
                // debug, not warn.
                tracing::debug!(error = %e, "wallpaper picker: dismissed");
            }
        }
    });
}

// ── Displays page ─────────────────────────────────────────────────────────────

/// Drawer page listing the connected outputs as reported by niri's IPC.
/// Each row shows make/model + the active mode and scale, with a switch
/// suffix that calls `displays::set_output_enabled`. Topology changes
/// (plug/unplug) are picked up by the displays service's 2 s polling loop
/// and trigger a full row-list rebuild here.
///
/// v1 is read-only beyond the on/off switch — there's no mode picker, no
/// scale editor, no rotation UI, and no position editor. Persistent
/// multi-monitor layouts live in `~/.config/kanshi/config` (see
/// `etc/kanshi/`); this page reflects whatever the running compositor +
/// kanshi have already decided.
pub fn page_displays() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    let group = adw::PreferencesGroup::builder().title("Displays").build();
    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    // In-flight set_output_enabled calls. The displays service polls niri
    // every 2 s; if niri's ack lags the next poll, the rebuilt row would
    // briefly flip the Switch back to the stale state. While a name is in
    // this map, the row build uses the stored intent instead of the
    // service snapshot, so the user's last toggle stands until niri
    // catches up.
    let pending: Rc<RefCell<HashMap<String, bool>>> = Rc::new(RefCell::new(HashMap::new()));

    column.append(&group);

    let group_for_bind = group.clone();
    let rows_track_for_bind = rows_track.clone();
    let pending_for_bind = pending.clone();
    bind(displays::outputs(), &group, move |_, list| {
        // PreferencesGroup has no row-traversal API, so we track the rows
        // we've added and remove them explicitly before each rebuild.
        for row in rows_track_for_bind.borrow_mut().drain(..) {
            group_for_bind.remove(&row);
        }
        if list.is_empty() {
            // Show a single placeholder row when niri reports nothing yet
            // — typically the first poll hasn't completed, or niri's IPC
            // socket isn't reachable.
            let placeholder = adw::ActionRow::builder()
                .title("No displays detected")
                .subtitle("Waiting for niri\u{2026}")
                .activatable(false)
                .build();
            group_for_bind.add(&placeholder);
            rows_track_for_bind.borrow_mut().push(placeholder);
            return;
        }
        let mut new_rows = Vec::with_capacity(list.len());
        for output in &list {
            let row = build_display_row(output, &pending_for_bind);
            group_for_bind.add(&row);
            new_rows.push(row);
        }
        *rows_track_for_bind.borrow_mut() = new_rows;
    });

    finish_page(&column)
}

fn build_display_row(o: &Output, pending: &Rc<RefCell<HashMap<String, bool>>>) -> adw::ActionRow {
    // Title prefers make+model when EDID is informative; falls back to the
    // bare connector name (e.g. virtual outputs, generic displays).
    let trimmed = format!("{} {}", o.make.trim(), o.model.trim());
    let title = if trimmed.trim().is_empty() {
        o.name.clone()
    } else {
        trimmed.trim().to_string()
    };

    let subtitle = if !o.enabled {
        "Disabled".to_string()
    } else if let Some(mode) = o.mode {
        let hz = f64::from(mode.refresh_mhz) / 1000.0;
        format!(
            "{}\u{00d7}{} @ {:.0} Hz, {:.1}\u{00d7}",
            mode.width, mode.height, hz, o.scale,
        )
    } else {
        // Enabled but no mode reported — odd but possible during
        // mode-switch races. Show what we know.
        format!("Enabled, scale {:.1}\u{00d7}", o.scale)
    };

    let row = adw::ActionRow::builder()
        .title(&title)
        .subtitle(&subtitle)
        .build();
    // Long-form titles or driver-injected EDIDs can blow the modal width;
    // let title and subtitle wrap rather than push the surface.
    row.set_subtitle_lines(0);

    let prefix = gtk::Label::new(Some(&o.name));
    prefix.add_css_class("ts-display-connector");
    prefix.add_css_class("monospace");
    prefix.set_valign(gtk::Align::Center);
    row.add_prefix(&prefix);

    let switch = gtk::Switch::new();
    switch.set_valign(gtk::Align::Center);
    // Skip the active-state restore while a toggle for this output is
    // in-flight: the next poll's row rebuild may carry the pre-toggle
    // state if niri's ack hasn't landed yet, which would visually flip
    // the switch back until the t+2 s poll catches up. When pending,
    // honor the user's last intent instead of the (possibly stale)
    // service snapshot.
    let pending_state = pending.borrow().get(&o.name).copied();
    switch.set_active(pending_state.unwrap_or(o.enabled));
    let name_for_toggle = o.name.clone();
    let pending_for_toggle = pending.clone();
    switch.connect_active_notify(move |sw| {
        let on = sw.is_active();
        pending_for_toggle
            .borrow_mut()
            .insert(name_for_toggle.clone(), on);
        displays::set_output_enabled(&name_for_toggle, on);
        // Disarm after 3 s — long enough for niri to ack and one or two
        // polls to land carrying the new state. After the timeout the
        // service's snapshot is treated as truth again.
        let pending_for_clear = pending_for_toggle.clone();
        let name_for_clear = name_for_toggle.clone();
        glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || {
            pending_for_clear.borrow_mut().remove(&name_for_clear);
        });
    });
    row.add_suffix(&switch);
    row.set_activatable_widget(Some(&switch));

    row
}

// ── Clipboard page ────────────────────────────────────────────────────────────

/// Drawer page for clipboard history, backed by `cliphist`. Capture happens
/// out-of-process under two `wl-paste --watch cliphist store` units (text +
/// image); see `etc/cliphist/README.md`.
///
/// On page open, [`crate::modal::on_page_show`] invokes
/// [`clipboard::refresh()`], which re-runs `cliphist list` off the GTK
/// thread and updates the [`clipboard::history()`] signal. The bind below
/// rebuilds the row list whenever that signal fires.
///
/// Click an entry to re-paste it: the service pipes
/// `cliphist decode <id> | wl-copy`, then we dismiss the drawer so the
/// next Ctrl-V lands in whatever app the user was in.
///
/// v1 caps the visible list at ~50 entries (enforced upstream in
/// `clipboard::refresh`). No pinning, search, or multi-select.
pub fn page_clipboard() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    let group = adw::PreferencesGroup::builder()
        .title("Clipboard history")
        .build();

    // Track the rows we add so we can remove them before each rebuild —
    // PreferencesGroup has no row-traversal API. Same shape as the wifi
    // network list and the displays page.
    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let placeholder_track: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));

    column.append(&group);

    let group_for_bind = group.clone();
    let rows_for_bind = rows_track.clone();
    let placeholder_for_bind = placeholder_track.clone();
    bind(clipboard::history(), &group, move |_, entries| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            group_for_bind.remove(&row);
        }
        if let Some(p) = placeholder_for_bind.borrow_mut().take() {
            group_for_bind.remove(&p);
        }

        if entries.is_empty() {
            // Empty state: a single non-activatable row keeps the visual
            // weight of the boxed list while making it obvious the
            // history is empty (or cliphist isn't running yet).
            let placeholder = adw::ActionRow::builder()
                .title("No clipboard history")
                .subtitle("Copy something, then re-open this page.")
                .activatable(false)
                .build();
            group_for_bind.add(&placeholder);
            *placeholder_for_bind.borrow_mut() = Some(placeholder);
            return;
        }

        let mut new_rows = Vec::with_capacity(entries.len());
        for entry in &entries {
            let row = build_clipboard_row(entry);
            group_for_bind.add(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    finish_page(&column)
}

// ── Settings page ─────────────────────────────────────────────────────────────

/// Drawer page exposing trollshell-wide preferences. v1 (minimal) covers two
/// knobs:
///
/// - Theme (Light / Dark / Follow system) — mediated by `gsettings` on
///   `org.gnome.desktop.interface color-scheme`. The dropdown reads the
///   current value once at page mount and writes back on selection change.
///   We do NOT live-track external `gsettings` changes; for v1 the dropdown
///   is the source of truth while the page is open.
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
        .subtitle("Light, dark, or follow system.")
        .build();

    // Order matches THEME_KEYS below — the DropDown's selected index maps
    // 1:1 to the gsettings color-scheme value.
    let theme_dropdown = gtk::DropDown::from_strings(&["Light", "Dark", "Follow system"]);
    theme_dropdown.set_valign(gtk::Align::Center);
    theme_dropdown.set_selected(read_color_scheme_index());
    theme_dropdown.connect_selected_notify(|dd| {
        let key = THEME_KEYS
            .get(dd.selected() as usize)
            .copied()
            .unwrap_or("default");
        write_color_scheme(key);
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

/// Build an `AdwActionRow` that, on activation, swaps every open drawer to
/// `target` via `modal::switch_active`. Used by the Settings page "More"
/// group to surface drawer pages that don't have a dedicated bar chip.
fn deep_link_row(
    title: &str,
    subtitle: Option<&str>,
    icon_name: &str,
    target: crate::modal::Page,
) -> adw::ActionRow {
    let mut builder = adw::ActionRow::builder().title(title).activatable(true);
    if let Some(s) = subtitle {
        builder = builder.subtitle(s);
    }
    let row = builder.build();
    let icon = gtk::Image::from_icon_name(icon_name);
    row.add_prefix(&icon);
    let go_next = gtk::Image::from_icon_name("go-next-symbolic");
    row.add_suffix(&go_next);
    row.connect_activated(move |_| {
        crate::modal::switch_active(target);
    });
    row
}

/// Index ↔ gsettings value mapping for the theme dropdown. Order must match
/// the strings passed to `gtk::DropDown::from_strings` in `page_settings`.
const THEME_KEYS: [&str; 3] = ["prefer-light", "prefer-dark", "default"];

/// Read the current `org.gnome.desktop.interface color-scheme` value via a
/// one-shot synchronous `gsettings get` and map it to a dropdown index.
/// Returns the "Follow system" index (2) on any error so the UI is never
/// blank — including when `gsettings-desktop-schemas` is missing.
fn read_color_scheme_index() -> u32 {
    let output = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            // gsettings get prints quoted strings, e.g. `'prefer-dark'\n`.
            let raw = String::from_utf8_lossy(&out.stdout);
            let trimmed = raw.trim().trim_matches('\'').trim_matches('"');
            THEME_KEYS
                .iter()
                .position(|k| *k == trimmed)
                .and_then(|i| u32::try_from(i).ok())
                .unwrap_or(2)
        }
        Ok(out) => {
            tracing::warn!(
                stderr = %String::from_utf8_lossy(&out.stderr),
                "settings: gsettings get color-scheme failed",
            );
            2
        }
        Err(e) => {
            tracing::warn!(error = %e, "settings: gsettings unavailable");
            2
        }
    }
}

/// Set `org.gnome.desktop.interface color-scheme` to `value`. Spawned
/// detached — we don't care to wait on the exit; failures are logged when
/// the spawn itself fails, but a non-zero exit (e.g. unknown schema) is
/// silently dropped by the OS reaping the child. That's fine for v1: the
/// next page mount will re-read and surface the actual current value.
fn write_color_scheme(value: &str) {
    let result = std::process::Command::new("gsettings")
        .args(["set", "org.gnome.desktop.interface", "color-scheme", value])
        .spawn();
    if let Err(e) = result {
        tracing::warn!(error = %e, value, "settings: gsettings set color-scheme failed");
    }
}

// ── Calendar page ─────────────────────────────────────────────────────────────

/// Drawer page for upcoming calendar events. Backed by
/// `hytte::services::calendar`, which reads evolution-data-server's on-disk
/// `.ics` cache (populated by GNOME Online Accounts via gnome-control-center).
///
/// Top: a `gtk::Calendar` scrolled to the current month. v1 shows the
/// month grid only — no per-day event marking; that's a v2 task once the
/// signal carries enough data to compute marked-day sets.
///
/// Below: an `adw::PreferencesGroup` titled "Upcoming" listing every event
/// in the next 7 days, sorted ascending by start. Empty list ⇒ a single
/// non-activatable "No upcoming events" placeholder row.
///
/// Click is a no-op for v1; future "open in calendar app" would pipe the
/// event UID at a CLI helper.
pub fn page_calendar() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    // ── Month view ─────────────────────────────────────────────────────────
    // GtkCalendar scrolls itself to the current month on construction.
    // Days that have at least one event in the visible month get mark_day().
    let cal = gtk::Calendar::new();
    cal.set_show_heading(true);
    cal.set_show_day_names(true);
    cal.set_show_week_numbers(false);
    cal.add_css_class("ts-calendar");
    column.append(&cal);

    // Latest events snapshot, shared between the events() subscription and
    // the prev/next-month signal handlers.
    let current_events: Rc<RefCell<Vec<CalendarEvent>>> = Rc::new(RefCell::new(Vec::new()));

    // Re-mark on month navigation. connect_next_month bumps year on December
    // rollover internally, so connect_year_changed isn't needed.
    {
        let events_for_next = current_events.clone();
        cal.connect_next_month(move |c| apply_event_marks(c, &events_for_next.borrow()));
    }
    {
        let events_for_prev = current_events.clone();
        cal.connect_prev_month(move |c| apply_event_marks(c, &events_for_prev.borrow()));
    }

    // ── Upcoming list ──────────────────────────────────────────────────────
    let group = adw::PreferencesGroup::builder().title("Upcoming").build();

    // Track rows by date so click-day in the calendar can scroll the list to
    // the matching event. Plus the empty-state placeholder so we can swap
    // them on each signal emission. Same shape as page_clipboard.
    let rows_track: Rc<RefCell<Vec<(chrono::NaiveDate, adw::ActionRow)>>> =
        Rc::new(RefCell::new(Vec::new()));
    let placeholder_track: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));

    // Bounded ScrolledWindow so click-day can drive scrolling directly.
    // finish_page only wraps in adw::Clamp, which doesn't scroll.
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_min_content_height(220);
    scrolled.set_max_content_height(360);
    scrolled.add_css_class("ts-calendar-list");
    scrolled.set_child(Some(&group));
    column.append(&scrolled);

    // Click a marked day → scroll the events list to the first matching
    // event and briefly highlight it. We compare on NaiveDate so we don't
    // care about start-time; multiple events on the same day all match the
    // first one we pushed (events are already sorted ascending).
    {
        let rows_for_select = rows_track.clone();
        let scrolled_for_select = scrolled.clone();
        cal.connect_day_selected(move |c| {
            let gdt = c.date();
            let y = gdt.year();
            let Ok(m) = u32::try_from(gdt.month()) else { return; };
            let Ok(day) = u32::try_from(gdt.day_of_month()) else { return; };
            let Some(d) = chrono::NaiveDate::from_ymd_opt(y, m, day) else { return; };
            let rows = rows_for_select.borrow();
            let Some((_d, row)) = rows.iter().find(|(date, _)| *date == d) else {
                return;
            };
            scroll_row_into_view(&scrolled_for_select, row);
            flash_row_highlight(row);
        });
    }

    let group_for_bind = group.clone();
    let rows_for_bind = rows_track.clone();
    let placeholder_for_bind = placeholder_track.clone();
    let cal_for_bind = cal.clone();
    let events_for_bind = current_events.clone();
    bind(calendar::events(), &group, move |_, evs| {
        for (_date, row) in rows_for_bind.borrow_mut().drain(..) {
            group_for_bind.remove(&row);
        }
        if let Some(p) = placeholder_for_bind.borrow_mut().take() {
            group_for_bind.remove(&p);
        }

        // Stash the snapshot before re-marking so the month-change handlers
        // see the latest events too.
        events_for_bind.borrow_mut().clone_from(&evs);
        apply_event_marks(&cal_for_bind, &evs);

        if evs.is_empty() {
            let placeholder = adw::ActionRow::builder()
                .title("No upcoming events")
                .subtitle("Add a calendar via Settings \u{2192} Online Accounts.")
                .activatable(false)
                .build();
            group_for_bind.add(&placeholder);
            *placeholder_for_bind.borrow_mut() = Some(placeholder);
            return;
        }

        let mut new_rows: Vec<(chrono::NaiveDate, adw::ActionRow)> =
            Vec::with_capacity(evs.len());
        for ev in &evs {
            let row = build_calendar_row(ev);
            group_for_bind.add(&row);
            new_rows.push((ev.start.date_naive(), row));
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    finish_page(&column)
}

/// Scroll `scrolled` so that `row` is visible. Uses `compute_point` to
/// translate the row's origin into the scrolled-window child's coordinate
/// space; an 8px lead-in keeps the row from butting against the top edge.
fn scroll_row_into_view(scrolled: &gtk::ScrolledWindow, row: &adw::ActionRow) {
    use gtk::prelude::WidgetExt;
    let Some(child) = scrolled.child() else { return; };
    let origin = gtk::graphene::Point::new(0.0, 0.0);
    let Some(point) = row.compute_point(&child, &origin) else { return; };
    let y = f64::from(point.y());
    let adj = scrolled.vadjustment();
    let target = (y - 8.0).max(adj.lower());
    let max = (adj.upper() - adj.page_size()).max(adj.lower());
    adj.set_value(target.min(max));
}

/// Add `.ts-cal-day-hit` to `row` for ~1.5s, then remove it. The CSS rule
/// uses a 600ms transition so the highlight fades rather than flashes.
fn flash_row_highlight(row: &adw::ActionRow) {
    row.add_css_class("ts-cal-day-hit");
    let row_for_clear = row.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
        row_for_clear.remove_css_class("ts-cal-day-hit");
    });
}

/// Mark each day in the calendar's currently-visible month that has at
/// least one event. `GtkCalendar`'s `month()` is 0-indexed; chrono's
/// `NaiveDate::month()` is 1-indexed — adjust before comparing.
fn apply_event_marks(cal: &gtk::Calendar, events: &[CalendarEvent]) {
    cal.clear_marks();
    let month = u32::try_from(cal.month() + 1).unwrap_or(0);
    let year = cal.year();
    let mut marked: HashSet<u32> = HashSet::new();
    for ev in events {
        let d = ev.start.date_naive();
        if d.year() == year && d.month() == month {
            marked.insert(d.day());
        }
    }
    for day in marked {
        cal.mark_day(day);
    }
}

fn build_calendar_row(ev: &CalendarEvent) -> adw::ActionRow {
    // Compose the subtitle from when-string + optional location. Using two
    // lines keeps long venue names from blowing the modal width.
    let when = calendar::format_when(ev);
    let subtitle = match &ev.location {
        Some(loc) => format!("{when}\n{loc}"),
        None => when,
    };

    let row = adw::ActionRow::builder()
        .title(&ev.summary)
        .subtitle(&subtitle)
        .activatable(false)
        .build();
    // Allow the subtitle to wrap when it carries a multi-line location.
    row.set_subtitle_lines(0);
    row.set_title_lines(1);

    let icon = gtk::Image::from_icon_name("x-office-calendar-symbolic");
    icon.set_valign(gtk::Align::Center);
    row.add_prefix(&icon);

    row
}

// ── Clipboard helpers ─────────────────────────────────────────────────────────

fn build_clipboard_row(entry: &ClipEntry) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&entry.preview)
        .activatable(true)
        .build();
    row.set_title_lines(1);

    let icon_name = match entry.kind {
        ClipKind::Image => "image-x-generic-symbolic",
        ClipKind::Text => "edit-paste-symbolic",
    };
    let icon = gtk::Image::from_icon_name(icon_name);
    row.add_prefix(&icon);

    // ⋮ menu button: destructive "Delete entry" lives here, not on the row's
    // primary click target — destructive actions belong in a popover so they
    // can't be misclicked while reaching for paste.
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

    let delete_btn = gtk::Button::with_label("Delete entry");
    delete_btn.add_css_class("flat");
    delete_btn.add_css_class("destructive-action");
    let id_for_delete = entry.id;
    let popover_for_delete = popover.clone();
    delete_btn.connect_clicked(move |_| {
        clipboard::delete(id_for_delete);
        popover_for_delete.popdown();
    });
    popover_box.append(&delete_btn);
    popover.set_child(Some(&popover_box));
    menu_btn.set_popover(Some(&popover));
    row.add_suffix(&menu_btn);

    let id = entry.id;
    row.connect_activated(move |_| {
        clipboard::paste_entry(id);
        crate::modal::dismiss_all();
    });

    row
}
