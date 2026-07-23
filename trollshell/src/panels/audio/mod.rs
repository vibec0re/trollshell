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

mod endpoint;
mod playback;
mod sinks;
mod sources;

use std::cell::Cell;
use std::time::{Duration, Instant};

use hytte::gtk::{self, prelude::*};

use crate::components::layout::{finish_page, page_box};

// Re-export shared helpers so submodules can access them via `super::`.
pub(super) use crate::components::layout::{boxed_list, toggle_class};

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

pub(super) fn truncate_desc(s: &str) -> String {
    if s.chars().count() > 40 {
        let truncated: String = s.chars().take(39).collect();
        format!("{truncated}…")
    } else {
        s.to_string()
    }
}

pub(super) fn default_radio_glyph(is_default: bool) -> &'static str {
    if is_default { "\u{25cf}" } else { "\u{25cb}" }
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

#[cfg(test)]
mod tests {
    use super::truncate_desc;

    #[test]
    fn short_ascii_is_unchanged() {
        assert_eq!(truncate_desc("USB Audio"), "USB Audio");
    }

    #[test]
    fn ascii_over_forty_chars_truncates_with_ellipsis() {
        // 45 'a's — old byte-slice behavior kept the first 39 bytes/chars.
        let s = "a".repeat(45);
        let result = truncate_desc(&s);
        assert_eq!(result, format!("{}…", "a".repeat(39)));
        assert_eq!(result.chars().count(), 40);
    }

    #[test]
    fn ascii_exactly_forty_chars_is_unchanged() {
        let s = "a".repeat(40);
        assert_eq!(truncate_desc(&s), s);
    }

    #[test]
    fn non_ascii_description_truncates_without_panicking_on_char_boundary() {
        // Multibyte umlauts throughout — a byte-index slice at 39 would
        // very likely land mid-codepoint and panic (the #424 bug).
        let s = "Käthe's Büro-Kopfhörer – Bluetöoth Läutsprecher Änlage".to_string();
        assert!(s.len() > 40, "fixture should exceed the byte threshold");
        let result = truncate_desc(&s);
        assert!(result.ends_with('…'));
        assert_eq!(result.chars().count(), 40);
    }

    #[test]
    fn emoji_description_truncates_without_panicking() {
        // Emoji are multi-byte and some are >1 char (grapheme clusters via
        // combining/ZWJ), but `chars()` still walks scalar values safely.
        let s = "🎧".repeat(45);
        let result = truncate_desc(&s);
        assert!(result.ends_with('…'));
        assert_eq!(result.chars().count(), 40);
    }
}
