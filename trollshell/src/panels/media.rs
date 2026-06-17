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

    let art_image = build_art_panel(&grid);
    let info = build_info_panel();
    grid.attach(&info.section, 1, 0, 1, 1);

    let state = Rc::new(PlayerState::default());
    wire_transport_buttons(&info.widgets, &state);
    wire_player_bind(&info.widgets, &art_image, &state);
    wire_seek(&info.widgets.seek, &state);

    finish_page(&grid)
}

fn build_art_panel(grid: &gtk::Grid) -> gtk::Image {
    let art_box = section("");
    art_box.set_size_request(220, 220);
    art_box.add_css_class("ts-media-art");
    let art_image = gtk::Image::new();
    art_image.set_icon_name(Some("audio-x-generic-symbolic"));
    art_image.set_pixel_size(200);
    art_box.append(&art_image);
    grid.attach(&art_box, 0, 0, 1, 1);
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
    art.set_pixel_size(200);
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
        art.set_size_request(200, 200);
    });
}

fn wire_seek(seek: &gtk::Scale, state: &Rc<PlayerState>) {
    let state_for_handler = state.clone();
    bind_two_way(
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
