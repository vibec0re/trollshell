//! Audio drawer panel — output sinks, input sources, and per-stream
//! playback volume.
//!
//! Each section is a `boxed-list`-styled `gtk::ListBox` that diffs every
//! pipewire snapshot against a live per-row map and updates fields in
//! place. Rebuilding rows on each emission would tear down the slider
//! the user is holding, so rows are kept alive across emissions and the
//! snapshot drives field-level updates.
//!
//! Echo cancellation: pipewire's polling is async, so a snapshot that
//! arrives after a `pactl` write may still report the OLD volume. Each
//! row tracks a `pending` value the user just sent; while pending,
//! mismatched snapshots are ignored to keep the slider thumb where the
//! user left it. Pending clears once pipewire confirms (snapshot ≈
//! pending) or after `ECHO_TIMEOUT` as a safety net.

mod playback;
mod sinks;
mod sources;

use std::cell::Cell;
use std::time::{Duration, Instant};

use hytte::gtk::{self, prelude::*};

use crate::components::layout::{finish_page, page_box};

/// Snapshot considered to match our pending write when within this much
/// of the value we sent. `pactl` rounds to integer percent so 0.005 is a
/// comfortable margin below the 0.01 step.
pub(super) const ECHO_TOLERANCE: f64 = 0.005;

/// Stop ignoring snapshots after this long, in case `pactl` failed and
/// no confirming snapshot will ever arrive.
const ECHO_TIMEOUT: Duration = Duration::from_secs(1);

/// Tolerance for skipping a redundant programmatic `set_value` when the
/// slider already shows the same volume as the snapshot.
pub(super) const SLIDER_NOOP_TOLERANCE: f64 = 0.001;

pub fn panel_audio() -> gtk::Widget {
    let column = page_box();
    column.append(&audio_section("Output", &sinks::build_sink_list()));
    column.append(&audio_section("Input", &sources::build_source_list()));
    column.append(&audio_section("Playback", &playback::build_playback_list()));
    finish_page(&column)
}

fn audio_section(title: &str, list: &gtk::ListBox) -> gtk::Box {
    let section = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let title_lbl = gtk::Label::new(Some(title));
    title_lbl.add_css_class("heading");
    title_lbl.set_xalign(0.0);
    section.append(&title_lbl);
    section.append(list);
    section
}

pub(super) fn boxed_list() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list
}

pub(super) fn truncate_desc(s: &str) -> String {
    if s.len() > 40 {
        format!("{}…", &s[..39])
    } else {
        s.to_string()
    }
}

pub(super) fn default_radio_glyph(is_default: bool) -> &'static str {
    if is_default { "\u{25cf}" } else { "\u{25cb}" }
}

pub(super) fn toggle_class<W: IsA<gtk::Widget>>(widget: &W, class: &str, on: bool) {
    if on {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}

/// Should the snapshot value be applied to the UI? Returns `true` when
/// no write is pending, the snapshot confirms the pending write, or the
/// pending write has timed out. Mutates `pending` to clear on
/// confirmation/timeout.
pub(super) fn echo_settled<T: Copy + PartialEq>(
    pending: &Cell<Option<(T, Instant)>>,
    snapshot: T,
    matches: impl FnOnce(T, T) -> bool,
) -> bool {
    match pending.get() {
        None => true,
        Some((expected, sent)) => {
            if matches(expected, snapshot) || sent.elapsed() > ECHO_TIMEOUT {
                pending.set(None);
                true
            } else {
                false
            }
        }
    }
}
