//! Center-of-bar MPRIS media controls widget.
//!
//! Shows prev / play-pause / next icon buttons and an "artist – title" label.
//! Hidden when no player is active. Buttons disable when the player's
//! corresponding `can_*` flag is false. The play/pause button icon toggles
//! based on the player's playback status.
//!
//! Clicking the label (not the transport buttons) toggles the Media page in
//! the modal panel.
//!
//! ## Three visual states
//!
//! - **No player** — container hidden entirely.
//! - **Player + busy workspace (≥ `HIDE_WHEN_WINDOWS_GTE` windows)** — *narrow
//!   mode*: show only the `mini` icon button; all transport controls and the
//!   title label are hidden. Clicking `mini` opens the Media panel.
//! - **Player + uncrowded workspace** — *full mode*: show transport controls
//!   and title label; `mini` is hidden.

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

/// All child widgets of the MPRIS container, bundled so they can be passed
/// around without hitting the `too_many_arguments` clippy limit.
struct Chips {
    prev_btn: gtk::Button,
    play_pause_btn: gtk::Button,
    next_btn: gtk::Button,
    label: gtk::Label,
    mini: gtk::Button,
}

/// Build the MPRIS center-cluster widget.
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    container.add_css_class("ts-mpris");

    let chips = Chips {
        prev_btn: icon_button("media-skip-backward-symbolic"),
        play_pause_btn: icon_button("media-playback-start-symbolic"),
        next_btn: icon_button("media-skip-forward-symbolic"),
        label: build_clickable_label(monitor),
        mini: build_mini_button(monitor),
    };

    container.append(&chips.prev_btn);
    container.append(&chips.play_pause_btn);
    container.append(&chips.next_btn);
    container.append(&chips.label);
    container.append(&chips.mini);

    let current_bus: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    bind_transport_button(&chips.prev_btn, &current_bus, mpris::previous);
    bind_transport_button(&chips.play_pause_btn, &current_bus, mpris::play_pause);
    bind_transport_button(&chips.next_btn, &current_bus, mpris::next);

    wire_visibility_and_state(&container, chips, &current_bus, monitor);

    container.set_visible(false);
    container.upcast()
}

fn icon_button(icon: &str) -> gtk::Button {
    let btn = gtk::Button::new();
    btn.set_child(Some(&gtk::Image::from_icon_name(icon)));
    btn
}

/// A compact single-icon button shown in *narrow* mode (busy workspace).
/// Clicking it opens the Media panel — same destination as the full-mode label.
fn build_mini_button(monitor: &Monitor) -> gtk::Button {
    let btn = icon_button("audio-x-generic-symbolic");
    btn.add_css_class("ts-mpris-mini");
    let monitor = monitor.clone();
    btn.connect_clicked(move |btn| {
        crate::modal::toggle(&monitor, crate::modal::Page::Media, btn);
    });
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

/// Drives the three-state MPRIS presentation:
///
/// - **No player** → container hidden; `current_bus` cleared.
/// - **Player + busy workspace** (narrow) → container visible; only `mini`
///   shown; transport controls and label hidden. `current_bus` and player
///   state are still kept current so the full-mode controls work immediately
///   when the workspace becomes uncrowded again.
/// - **Player + uncrowded workspace** (full) → container visible; transport
///   controls and label shown; `mini` hidden.
fn wire_visibility_and_state(
    container: &gtk::Box,
    chips: Chips,
    current_bus: &Rc<RefCell<Option<String>>>,
    monitor: &Monitor,
) {
    let combined = map_ref! {
        let player = mpris::active_player(),
        let wins = window_list::active_workspace_windows(monitor.connector()) => {
            (player.clone(), wins.len())
        }
    };

    let Chips {
        prev_btn,
        play_pause_btn,
        next_btn,
        label,
        mini,
    } = chips;
    let current_bus = current_bus.clone();

    bind(
        combined,
        container,
        move |container, (maybe_player, win_count): (Option<Player>, usize)| match maybe_player {
            None => {
                container.set_visible(false);
                *current_bus.borrow_mut() = None;
            }
            Some(player) if win_count >= HIDE_WHEN_WINDOWS_GTE => {
                // Narrow mode: keep bus + state current so full mode works on
                // transition back, but show only the mini icon affordance.
                *current_bus.borrow_mut() = Some(player.bus_name.clone());
                apply_player_to_widgets(&player, &label, &prev_btn, &play_pause_btn, &next_btn);
                prev_btn.set_visible(false);
                play_pause_btn.set_visible(false);
                next_btn.set_visible(false);
                label.set_visible(false);
                mini.set_visible(true);
                container.set_visible(true);
            }
            Some(player) => {
                *current_bus.borrow_mut() = Some(player.bus_name.clone());
                apply_player_to_widgets(&player, &label, &prev_btn, &play_pause_btn, &next_btn);
                prev_btn.set_visible(true);
                play_pause_btn.set_visible(true);
                next_btn.set_visible(true);
                label.set_visible(true);
                mini.set_visible(false);
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
