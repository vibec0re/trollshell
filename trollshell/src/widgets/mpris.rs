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
use hytte::services::mpris::{self, Player};

use crate::components::mpris_controls::{bind_transport_button, play_pause_icon};
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

    let prev_btn = icon_button("media-skip-backward-symbolic");
    let play_pause_btn = icon_button("media-playback-start-symbolic");
    let next_btn = icon_button("media-skip-forward-symbolic");
    let label = build_clickable_label(monitor);

    container.append(&prev_btn);
    container.append(&play_pause_btn);
    container.append(&next_btn);
    container.append(&label);

    let current_bus: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    bind_transport_button(&prev_btn, &current_bus, mpris::previous);
    bind_transport_button(&play_pause_btn, &current_bus, mpris::play_pause);
    bind_transport_button(&next_btn, &current_bus, mpris::next);

    wire_visibility_and_state(
        &container,
        &label,
        &prev_btn,
        &play_pause_btn,
        &next_btn,
        &current_bus,
        monitor,
    );

    container.set_visible(false);
    container.upcast()
}

fn icon_button(icon: &str) -> gtk::Button {
    let btn = gtk::Button::new();
    btn.set_child(Some(&gtk::Image::from_icon_name(icon)));
    btn
}

fn build_clickable_label(monitor: &Monitor) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(60);

    let gesture = gtk::GestureClick::new();
    gesture.set_button(gdk::BUTTON_PRIMARY);
    let monitor = monitor.clone();
    let label_for_anchor = label.clone();
    gesture.connect_pressed(move |gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        crate::modal::toggle(&monitor, crate::modal::Page::Media, &label_for_anchor);
    });
    label.add_controller(gesture);
    label
}

/// MPRIS hides when there's no active player OR when the left cluster is
/// crowded enough that keeping it visible would risk pushing the right
/// cluster off-screen.
fn wire_visibility_and_state(
    container: &gtk::Box,
    label: &gtk::Label,
    prev_btn: &gtk::Button,
    play_pause_btn: &gtk::Button,
    next_btn: &gtk::Button,
    current_bus: &Rc<RefCell<Option<String>>>,
    monitor: &Monitor,
) {
    let combined = map_ref! {
        let player = mpris::active_player(),
        let wins = window_list::active_workspace_windows(monitor.connector()) => {
            (player.clone(), wins.len())
        }
    };

    let label = label.clone();
    let prev_btn = prev_btn.clone();
    let play_pause_btn = play_pause_btn.clone();
    let next_btn = next_btn.clone();
    let current_bus = current_bus.clone();

    bind(
        combined,
        container,
        move |container, (maybe_player, win_count): (Option<Player>, usize)| match maybe_player {
            None => {
                container.set_visible(false);
                *current_bus.borrow_mut() = None;
            }
            Some(_) if win_count >= HIDE_WHEN_WINDOWS_GTE => {
                container.set_visible(false);
            }
            Some(player) => {
                *current_bus.borrow_mut() = Some(player.bus_name.clone());
                apply_player_to_widgets(&player, &label, &prev_btn, &play_pause_btn, &next_btn);
                container.set_visible(true);
            }
        },
    );
}

fn apply_player_to_widgets(
    player: &Player,
    label: &gtk::Label,
    prev_btn: &gtk::Button,
    play_pause_btn: &gtk::Button,
    next_btn: &gtk::Button,
) {
    let text = if player.artists.is_empty() {
        player.title.clone()
    } else {
        format!("{} \u{2013} {}", player.artists, player.title)
    };
    label.set_text(&text);
    label.set_tooltip_text(Some(&text));

    prev_btn.set_sensitive(player.can_go_previous);
    play_pause_btn.set_sensitive(player.can_play_pause);
    next_btn.set_sensitive(player.can_go_next);

    let icon_name = play_pause_icon(player.status);
    if let Some(img) = play_pause_btn.child().and_downcast::<gtk::Image>() {
        img.set_icon_name(Some(icon_name));
    }
}
