//! Drawer panel for the active MPRIS player — art + metadata + transport
//! controls + seek bar. Backed by `hytte::services::mpris` which surfaces
//! whichever player is currently top-priority.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::mpris::{self, PlaybackStatus};

use crate::components::format::fmt_us;
use crate::components::layout::{finish_page, page_grid, section};

#[allow(clippy::too_many_lines)]
pub fn panel_media() -> gtk::Widget {
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
