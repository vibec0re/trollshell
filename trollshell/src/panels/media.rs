//! Drawer panel for the active MPRIS player — art + metadata + transport
//! controls + seek bar. Backed by `hytte::services::mpris` which surfaces
//! whichever player is currently top-priority.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::mpris;

use crate::components::cast;
use crate::components::format::fmt_us;
use crate::components::layout::{finish_page, page_grid, section};
use crate::components::mpris_controls::{bind_transport_button, play_pause_icon};
use crate::scale::scale;

/// Labels + buttons that the bind closure updates on every emission.
#[derive(Clone)]
struct InfoWidgets {
    title: gtk::Label,
    artist: gtk::Label,
    album: gtk::Label,
    prev_btn: gtk::Button,
    play_pause_btn: gtk::Button,
    next_btn: gtk::Button,
    seek: gtk::Scale,
    pos: gtk::Label,
    len: gtk::Label,
}

/// Mutable per-render state shared between the bind closure and the
/// click/seek handlers. `bus` is its own `Rc<RefCell<…>>` so the shared
/// transport-button helper in `components::mpris_controls` can hold a
/// clone without taking the whole `PlayerState`.
#[derive(Default)]
struct PlayerState {
    bus: Rc<RefCell<Option<String>>>,
    track_id: RefCell<Option<String>>,
    length_us: Cell<u64>,
    last_art_url: RefCell<String>,
}

pub fn panel_media() -> gtk::Widget {
    let grid = page_grid();
    grid.set_column_homogeneous(false);

    // Source switcher spans both columns at the top; shown only with >=2
    // players (it wires its own reactive visibility).
    let switcher = build_switcher();
    grid.attach(&switcher, 0, 0, 2, 1);

    let art_image = build_art_panel(&grid);
    let info = build_info_panel();
    grid.attach(&info.section, 1, 1, 1, 1);

    let state = Rc::new(PlayerState::default());
    wire_transport_buttons(&info.widgets, &state);
    wire_player_bind(&info.widgets, &art_image, &state);
    wire_seek(&info.widgets.seek, &state);

    finish_page(&grid)
}

/// Per-render switcher chips: `bus_name` (`None` = the "Auto" chip) paired
/// with its toggle button, so the active-marking bind can restyle without a
/// full rebuild.
type SwitcherChips = Rc<RefCell<Vec<(Option<String>, gtk::ToggleButton)>>>;

/// A horizontal row of selectable source chips: one per live MPRIS player
/// plus an "Auto" chip to revert to the heuristic. Rebuilt reactively from
/// `mpris::players()`; the whole row hides unless >=2 players are present.
/// The chip matching `mpris::active_player()` is marked active.
fn build_switcher() -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("ts-media-switcher");

    let chips: SwitcherChips = Rc::new(RefCell::new(Vec::new()));

    // Rebuild the chip set only when the ROSTER (each player's bus_name +
    // display label) actually changes. `mpris::players()` re-emits ~4Hz from
    // the position poller, so binding the rebuild straight to it would tear
    // down and recreate every chip 4x/second. Project to the roster + dedupe
    // so a rebuild fires only on add/remove/relabel.
    let chips_for_build = chips.clone();
    bind(
        mpris::players()
            .map(|players| {
                players
                    .iter()
                    .map(|p| (p.bus_name.clone(), player_label(p)))
                    .collect::<Vec<(String, String)>>()
            })
            .dedupe_cloned(),
        &row,
        move |row, roster| {
            rebuild_switcher(row, &roster, &chips_for_build);
        },
    );

    // Mark whichever chip is currently active (pinned or heuristic).
    let chips_for_active = chips.clone();
    bind(
        mpris::active_player().map(|p| p.map(|p| p.bus_name)),
        &row,
        move |_, active_bus| {
            for (bus, btn) in chips_for_active.borrow().iter() {
                // The "Auto" chip (bus == None) is never the active *source*;
                // it only reflects whether we're in automatic mode, which we
                // can't tell from active_player alone, so leave it unset and
                // let the matching player chip light up instead.
                let is_active = bus.as_deref() == active_bus.as_deref();
                set_toggle_silently(btn, is_active);
            }
        },
    );

    row.upcast()
}

/// Tear down and rebuild the switcher chips for the current roster (one
/// `(bus_name, label)` per live player).
fn rebuild_switcher(row: &gtk::Box, roster: &[(String, String)], chips: &SwitcherChips) {
    // Hidden unless there is an actual choice to make.
    row.set_visible(roster.len() >= 2);

    while let Some(child) = row.first_child() {
        row.remove(&child);
    }
    let mut new_chips = Vec::with_capacity(roster.len() + 1);

    // "Auto" chip — revert to the heuristic.
    let auto = source_chip("Auto");
    auto.connect_clicked(|_| mpris::select_player(None));
    row.append(&auto);
    new_chips.push((None, auto));

    // One chip per player, labelled by identity (fallbacks below).
    for (bus, label) in roster {
        let chip = source_chip(label);
        let bus_for_click = bus.clone();
        chip.connect_clicked(move |_| mpris::select_player(Some(bus_for_click.clone())));
        row.append(&chip);
        new_chips.push((Some(bus.clone()), chip));
    }

    *chips.borrow_mut() = new_chips;
}

/// Label for a source chip: `identity`, falling back to `title`, then
/// `bus_name`.
fn player_label(player: &mpris::Player) -> String {
    if !player.identity.is_empty() {
        player.identity.clone()
    } else if !player.title.is_empty() {
        player.title.clone()
    } else {
        player.bus_name.clone()
    }
}

fn source_chip(label: &str) -> gtk::ToggleButton {
    let btn = gtk::ToggleButton::with_label(label);
    btn.add_css_class("ts-media-source");
    btn
}

/// Set a chip's active (pressed) state. `set_active` only emits `toggled`,
/// not `clicked` (our selection handler is on `clicked`), so this purely
/// restyles the chip and never re-fires selection. Guarded to a no-op when
/// already in the desired state.
fn set_toggle_silently(btn: &gtk::ToggleButton, active: bool) {
    if btn.is_active() != active {
        btn.set_active(active);
    }
}

fn build_art_panel(grid: &gtk::Grid) -> gtk::Image {
    let art_box = section("");
    art_box.set_size_request(scale(220), scale(220));
    art_box.add_css_class("ts-media-art");
    let art_image = gtk::Image::new();
    art_image.set_icon_name(Some("audio-x-generic-symbolic"));
    art_image.set_pixel_size(scale(200));
    art_box.append(&art_image);
    grid.attach(&art_box, 0, 1, 1, 1);
    art_image
}

struct InfoPanel {
    section: gtk::Box,
    widgets: InfoWidgets,
}

fn build_info_panel() -> InfoPanel {
    let section = section("Now Playing");

    let title = ellipsized_label("ts-media-title", 40);
    title.set_text("\u{2014}");
    let artist = ellipsized_label("ts-media-artist", 40);
    let album = ellipsized_label("ts-media-album", 40);
    section.append(&title);
    section.append(&artist);
    section.append(&album);

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    controls.set_margin_top(8);
    let prev_btn = gtk::Button::from_icon_name("media-skip-backward-symbolic");
    let play_pause_btn = gtk::Button::from_icon_name("media-playback-start-symbolic");
    let next_btn = gtk::Button::from_icon_name("media-skip-forward-symbolic");
    controls.append(&prev_btn);
    controls.append(&play_pause_btn);
    controls.append(&next_btn);
    section.append(&controls);

    let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.001);
    seek.set_draw_value(false);
    seek.set_hexpand(true);
    section.append(&seek);

    let time_line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let pos = gtk::Label::new(Some("0:00"));
    pos.set_xalign(0.0);
    pos.set_hexpand(true);
    let len = gtk::Label::new(Some("0:00"));
    len.set_xalign(1.0);
    len.set_hexpand(true);
    time_line.append(&pos);
    time_line.append(&len);
    section.append(&time_line);

    InfoPanel {
        section,
        widgets: InfoWidgets {
            title,
            artist,
            album,
            prev_btn,
            play_pause_btn,
            next_btn,
            seek,
            pos,
            len,
        },
    }
}

fn ellipsized_label(css_class: &str, max_chars: i32) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.add_css_class(css_class);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(max_chars);
    label
}

fn wire_transport_buttons(w: &InfoWidgets, state: &Rc<PlayerState>) {
    bind_transport_button(&w.prev_btn, &state.bus, mpris::previous);
    bind_transport_button(&w.play_pause_btn, &state.bus, mpris::play_pause);
    bind_transport_button(&w.next_btn, &state.bus, mpris::next);
}

fn wire_player_bind(w: &InfoWidgets, art_image: &gtk::Image, state: &Rc<PlayerState>) {
    let w = w.clone();
    let state = state.clone();
    let art = art_image.clone();
    let title = w.title.clone();
    bind(
        mpris::active_player(),
        &title,
        move |_, maybe_player| match maybe_player {
            None => render_no_player(&w, &art, &state),
            Some(player) => render_player(&w, &art, &state, &player),
        },
    );
}

fn render_no_player(w: &InfoWidgets, art: &gtk::Image, state: &PlayerState) {
    *state.bus.borrow_mut() = None;
    *state.track_id.borrow_mut() = None;
    state.length_us.set(0);
    w.title.set_text("No player");
    w.artist.set_text("");
    w.album.set_text("");
    w.pos.set_text("0:00");
    w.len.set_text("0:00");
    art.set_paintable(None::<&gdk::Paintable>);
    art.set_icon_name(Some("audio-x-generic-symbolic"));
    art.set_pixel_size(scale(200));
    w.prev_btn.set_sensitive(false);
    w.play_pause_btn.set_sensitive(false);
    w.next_btn.set_sensitive(false);
}

fn render_player(w: &InfoWidgets, art: &gtk::Image, state: &PlayerState, player: &mpris::Player) {
    *state.bus.borrow_mut() = Some(player.bus_name.clone());
    (*state.track_id.borrow_mut()).clone_from(&player.track_id);
    state.length_us.set(player.length_us);

    w.title.set_text(&player.title);
    w.artist.set_text(&player.artists);
    w.album.set_text(&player.album);

    w.prev_btn.set_sensitive(player.can_go_previous);
    w.play_pause_btn.set_sensitive(player.can_play_pause);
    w.next_btn.set_sensitive(player.can_go_next);

    w.play_pause_btn
        .set_icon_name(play_pause_icon(player.status));

    w.pos.set_text(&fmt_us(player.position_us));
    w.len.set_text(&fmt_us(player.length_us));

    if *state.last_art_url.borrow() != player.art_url {
        (*state.last_art_url.borrow_mut()).clone_from(&player.art_url);
        spawn_art_fetch(art.clone(), player.art_url.clone());
    }
}

fn spawn_art_fetch(art: gtk::Image, url: String) {
    glib::MainContext::default().spawn_local(async move {
        let Some(bytes) = mpris::art_for_url(&url).await else {
            return;
        };
        let glib_bytes = glib::Bytes::from(&bytes);
        let Ok(texture) = gdk::Texture::from_bytes(&glib_bytes) else {
            return;
        };
        art.set_pixel_size(-1);
        art.set_paintable(Some(&texture));
        art.set_size_request(scale(200), scale(200));
    });
}

fn wire_seek(seek: &gtk::Scale, state: &Rc<PlayerState>) {
    let state_for_handler = state.clone();
    // Drag-safe: the mpris position poller writes this scale ~4×/s, which would
    // otherwise yank the thumb back mid-drag. `bind_two_way_drag_safe` suppresses
    // the poller's applies while the user is grabbing the slider (and briefly
    // after release) so the seek no longer fights the poller — see #445.
    bind_two_way_drag_safe(
        mpris::active_player().map(player_seek_fraction),
        seek,
        gtk::prelude::RangeExt::set_value,
        move |s| {
            let state = state_for_handler.clone();
            s.connect_value_changed(move |s| send_seek(s, &state))
        },
    );
}

fn player_seek_fraction(maybe: Option<mpris::Player>) -> f64 {
    let Some(p) = maybe else { return 0.0 };
    if p.length_us == 0 {
        return 0.0;
    }
    (cast::u64_to_f64(p.position_us) / cast::u64_to_f64(p.length_us)).clamp(0.0, 1.0)
}

fn send_seek(scale: &gtk::Scale, state: &PlayerState) {
    let bus_opt = state.bus.borrow();
    let tid_opt = state.track_id.borrow();
    let (Some(b), Some(t)) = (bus_opt.as_ref(), tid_opt.as_ref()) else {
        return;
    };
    let length = state.length_us.get();
    if length == 0 {
        return;
    }
    let pos_fraction = scale.value().clamp(0.0, 1.0);
    let pos_us = cast::f64_to_i64_trunc(pos_fraction * cast::u64_to_f64(length));
    mpris::set_position(b, t, pos_us);
}
