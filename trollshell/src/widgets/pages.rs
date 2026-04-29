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

use chrono::{DateTime, Local};
use hytte::adw::{self, prelude::*};
use hytte::futures_signals::map_ref;
use hytte::gtk::{self, gdk, glib};
use hytte::prelude::*;
use hytte::services::brightness;
use hytte::services::dnd;
use hytte::services::mpris::{self, PlaybackStatus};
use hytte::services::netconn;
use hytte::services::networkd::{self, OperationalState};
use hytte::services::notifications;
use hytte::services::notifications_mute;
use hytte::services::power_profiles::{self, humanize_profile};
use hytte::services::resolved;
use hytte::services::sensors::{self, CpuLoad};
use hytte::services::systemd;
use hytte::services::upower::{self, Battery, BatteryState};
use hytte::services::vpn;
use hytte::services::wifi;
use hytte::ui::Sparkline;

use crate::components::deep_link_row::deep_link_row;
use crate::components::format::{fmt_bytes, fmt_rate, fmt_us, humanize_since};
use crate::components::history_row::build_history_row;
use crate::components::layout::{finish_page, page_box, page_grid, section};

// ── Media page ────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
pub fn page_media() -> gtk::Widget {
    let grid = page_grid();
    grid.set_column_homogeneous(false);

    // ── Art panel (col 0) ─────────────────────────────────────────────────────
    let art_box = section("");
    art_box.set_size_request(220, 220);
    art_box.add_css_class("ts-media-art");
    let art_image = gtk::Image::new();
    art_image.set_icon_name(Some("audio-x-generic-symbolic"));
    art_image.set_pixel_size(200);
    art_box.append(&art_image);
    grid.attach(&art_box, 0, 0, 1, 1);

    // ── Metadata + controls panel (col 1) ─────────────────────────────────────
    let info = section("Now Playing");

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

// ── Network page ──────────────────────────────────────────────────────────────

pub fn page_network() -> gtk::Widget {
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
///
/// `container` is a vertical `gtk::Box` stacking a `build_history_row`
/// (name + full-hexpand sparkline + small unit) on top, and a detail
/// label carrying the rate breakdown below — matching the
/// `build_history_network_row` pattern in `page_stats` so the rate
/// label reads under the graph rather than crammed beside it.
struct IfaceRow {
    container: gtk::Box,
    spark: Sparkline,
    value: gtk::Label,
}

/// One per-interface traffic row laid out vertically:
///
/// ```text
/// [iface  | sparkline......| B/s ]
/// [          ↓ X ↑ Y                ]
/// ```
///
/// Mirrors `build_history_network_row` from `page_stats`: top row is
/// `build_history_row` with a static "B/s" unit on the right; bottom
/// row is a detail label indented to align under the sparkline column
/// (80 px name col + 8 px box spacing = 88 px). The bind updates the
/// sparkline samples and the detail label text in place across
/// `sensors::network()` emissions.
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


