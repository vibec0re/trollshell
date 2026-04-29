//! Center-of-bar MPRIS media controls widget.
//!
//! Shows prev / play-pause / next icon buttons and an "artist – title" label.
//! Hidden when no player is active. Buttons disable when the player's
//! corresponding `can_*` flag is false. The play/pause button icon toggles
//! based on the player's playback status.
//!
//! Clicking the label (not the transport buttons) toggles the Media page in
//! the modal panel.

use std::cell::RefCell;
use std::rc::Rc;

use hytte::futures_signals::map_ref;
use hytte::gtk::{self, gdk, prelude::*};
use hytte::prelude::*;
use hytte::services::mpris::{self, PlaybackStatus, Player};

use crate::widgets::window_list;

/// Hide MPRIS once the left cluster gets this busy. Even at 2 windows the
/// title labels can grow wide enough to collide with the centered MPRIS
/// row (`CenterBox` doesn't enforce non-overlap when content exceeds
/// capacity), so we yield early.
const HIDE_WHEN_WINDOWS_GTE: usize = 2;

/// Build the MPRIS center-cluster widget.
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    container.add_css_class("ts-mpris");

    // Buttons.
    let prev_btn = gtk::Button::new();
    prev_btn.set_child(Some(
        &gtk::Image::from_icon_name("media-skip-backward-symbolic"),
    ));

    let play_pause_btn = gtk::Button::new();
    let play_icon = gtk::Image::from_icon_name("media-playback-start-symbolic");
    play_pause_btn.set_child(Some(&play_icon));

    let next_btn = gtk::Button::new();
    next_btn.set_child(Some(
        &gtk::Image::from_icon_name("media-skip-forward-symbolic"),
    ));

    // Label — clicking it opens the Media modal page.
    let label = gtk::Label::new(None);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(60);

    // Add a GestureClick on the label for modal toggle.
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gdk::BUTTON_PRIMARY);
    let monitor_for_label = monitor.clone();
    gesture.connect_pressed(move |gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        crate::modal::toggle(&monitor_for_label, crate::modal::Page::Media);
    });
    label.add_controller(gesture);

    container.append(&prev_btn);
    container.append(&play_pause_btn);
    container.append(&next_btn);
    container.append(&label);

    // Shared bus name so click handlers always use the latest active player.
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

    // Bind the active player signal combined with the per-monitor window
    // count. MPRIS hides when there's no active player OR when the left
    // cluster is crowded enough that keeping it visible would risk pushing
    // the right cluster off-screen.
    let combined = map_ref! {
        let player = mpris::active_player(),
        let wins = window_list::active_workspace_windows(monitor.connector()) => {
            (player.clone(), wins.len())
        }
    };

    bind(
        combined,
        &container,
        move |container, (maybe_player, win_count): (Option<Player>, usize)| {
            match maybe_player {
                None => {
                    container.set_visible(false);
                    *current_bus.borrow_mut() = None;
                }
                Some(_) if win_count >= HIDE_WHEN_WINDOWS_GTE => {
                    container.set_visible(false);
                }
                Some(player) => {
                    *current_bus.borrow_mut() = Some(player.bus_name.clone());

                    // Update label text (truncate to 60 chars by max_width_chars,
                    // and show full text in the tooltip).
                    let text = if player.artists.is_empty() {
                        player.title.clone()
                    } else {
                        format!("{} \u{2013} {}", player.artists, player.title)
                    };
                    label.set_text(&text);
                    label.set_tooltip_text(Some(&text));

                    // Button sensitivity.
                    prev_btn.set_sensitive(player.can_go_previous);
                    play_pause_btn.set_sensitive(player.can_play_pause);
                    next_btn.set_sensitive(player.can_go_next);

                    // Play/pause icon toggles with status.
                    let icon_name = if player.status == PlaybackStatus::Playing {
                        "media-playback-pause-symbolic"
                    } else {
                        "media-playback-start-symbolic"
                    };
                    play_pause_btn
                        .child()
                        .and_downcast::<gtk::Image>()
                        .iter()
                        .for_each(|img| img.set_icon_name(Some(icon_name)));

                    container.set_visible(true);
                }
            }
        },
    );

    // Hide by default until first emission.
    container.set_visible(false);

    container.upcast()
}
