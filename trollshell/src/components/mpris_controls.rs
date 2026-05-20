//! Shared MPRIS transport-control helpers. Used by both the bar's
//! center-cluster widget (`widgets::mpris`) and the drawer's full media
//! page (`panels::media`).

use std::cell::RefCell;
use std::rc::Rc;

use hytte::gtk::{self, prelude::*};
use hytte::services::mpris::PlaybackStatus;

/// Wire a transport button so clicks call `action` against the currently
/// active player's bus name. Closure captures the `Rc<RefCell<…>>` so the
/// button always dispatches against the latest player even after the
/// active-player signal swaps.
pub fn bind_transport_button(
    btn: &gtk::Button,
    bus: &Rc<RefCell<Option<String>>>,
    action: fn(&str),
) {
    let bus = bus.clone();
    btn.connect_clicked(move |_| {
        if let Some(b) = bus.borrow().as_ref() {
            action(b);
        }
    });
}

/// Icon name the play/pause button should show for the given playback
/// status — pause glyph while playing, play glyph otherwise.
#[must_use]
pub fn play_pause_icon(status: PlaybackStatus) -> &'static str {
    if status == PlaybackStatus::Playing {
        "media-playback-pause-symbolic"
    } else {
        "media-playback-start-symbolic"
    }
}
