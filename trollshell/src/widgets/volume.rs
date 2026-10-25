use std::cell::Cell;
use std::rc::Rc;

use hytte::gtk::{self, gdk, prelude::*};
use hytte::prelude::*;
use hytte::services::pipewire::{self, Volume};

use crate::components::chip::wire_scroll;
use crate::plugins::effects::MAX_VOLUME;

/// Volume change per scroll notch.
const VOLUME_STEP: f64 = 0.05;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = crate::components::chip::indicator("ts-volume", crate::modal::Page::Audio, monitor);

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    // The level the next notch steps from, kept around so the scroll handler
    // can compute `current ± step` without a synchronous read-back of the
    // (async, cross-thread) service state. The bind below writes PipeWire's
    // answer into it; the scroll handler writes the level it just *asked
    // for*, because that answer is a round trip away.
    let current = Rc::new(Cell::new(0.0_f64));

    let current_for_bind = Rc::clone(&current);
    bind(pipewire::default_sink(), &icon, move |w, v: Volume| {
        current_for_bind.set(v.linear);
        w.set_icon_name(Some(icon_name(&v)));
    });

    wire_scroll(&btn, move |direction| {
        let next = scroll_target(current.get(), direction);
        pipewire::set_volume(next);
        // Step the next notch from here, not from the last level PipeWire
        // confirmed. `wire_scroll` fires this several times for one fast flick
        // (one call per whole notch the event's delta crossed), and a quick
        // wheel lands several events inside one round trip — each of which used
        // to step from the same stale `current` and send the same level, so
        // four notches moved the volume by one. The bind still overwrites this
        // with the real level when it arrives.
        current.set(next);
    });

    let mute_gesture = gtk::GestureClick::new();
    mute_gesture.set_button(gdk::BUTTON_MIDDLE);
    mute_gesture.connect_pressed(|gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        pipewire::toggle_mute();
    });
    btn.add_controller(mute_gesture);

    btn.upcast()
}

/// The level one scroll notch takes the default sink to from `current`.
///
/// GDK `dy` is positive for scroll-down; treat that as "decrease" so scroll-up
/// raises the volume, matching most volume sliders.
///
/// Bounded below by silence and above by [`MAX_VOLUME`], the same ceiling a
/// plugin's `SetVolume` is clamped to and the drawer's slider tops out at. It
/// used to be `.max(0.0)` alone, so the chip was the one surface in the shell
/// that would scroll a sink past 100 % — without limit, one notch at a time.
///
/// The ceiling is `MAX_VOLUME` **or `current`, whichever is higher**, and that
/// is deliberate: the shipped keybinds boost with `wpctl set-volume … -l 1.5`
/// (`etc/niri/binds.kdl`), which the OSD shows as e.g. "130%". A flat clamp to
/// 1.0 would turn a scroll *up* from there into a 30-point drop, and a scroll
/// down into a snap to 100 % rather than one step. With `current` as the ceiling
/// a scroll up over a boost does nothing and a scroll down lowers it by a notch.
///
/// `f64::clamp` cannot panic here: the lower bound is `0.0` and the upper is at
/// least `MAX_VOLUME` (`f64::max` ignores a NaN operand), so it is never below
/// the lower bound and never NaN.
fn scroll_target(current: f64, direction: f64) -> f64 {
    (current - direction * VOLUME_STEP).clamp(0.0, MAX_VOLUME.max(current))
}

fn icon_name(v: &Volume) -> &'static str {
    if v.muted {
        "audio-volume-muted-symbolic"
    } else if v.linear < 0.34 {
        "audio-volume-low-symbolic"
    } else if v.linear < 0.67 {
        "audio-volume-medium-symbolic"
    } else {
        "audio-volume-high-symbolic"
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_VOLUME, VOLUME_STEP, scroll_target};

    /// `wire_scroll`'s directions: `-1.0` is a notch up, `1.0` a notch down.
    const UP: f64 = -1.0;
    const DOWN: f64 = 1.0;

    fn assert_near(got: f64, want: f64) {
        assert!((got - want).abs() < 1e-9, "got {got}, want {want}");
    }

    /// The bug: scrolling up had no upper bound, so the chip walked a sink past
    /// 100 % one notch at a time — past what a plugin may set and past where the
    /// drawer's slider ends. It stops at the shared ceiling now.
    ///
    /// Falsify by dropping the upper bound (`.max(0.0)` alone): the first
    /// assertion reads 1.03.
    #[test]
    fn scrolling_up_stops_at_the_ceiling() {
        assert_near(scroll_target(0.98, UP), MAX_VOLUME);
        assert_near(scroll_target(MAX_VOLUME, UP), MAX_VOLUME);
        assert_near(scroll_target(0.5, UP), 0.5 + VOLUME_STEP);
    }

    #[test]
    fn scrolling_down_stops_at_silence() {
        assert_near(scroll_target(0.02, DOWN), 0.0);
        assert_near(scroll_target(0.0, DOWN), 0.0);
        assert_near(scroll_target(0.5, DOWN), 0.5 - VOLUME_STEP);
    }

    /// A boost made with the keybinds (`wpctl … -l 1.5`) is above the chip's
    /// ceiling. A scroll up must not *lower* it to the ceiling, and a scroll
    /// down lowers it by one notch rather than snapping it to 100 %.
    #[test]
    fn a_boost_from_the_keybinds_is_neither_raised_nor_snapped_down() {
        assert_near(scroll_target(1.3, UP), 1.3);
        assert_near(scroll_target(1.3, DOWN), 1.3 - VOLUME_STEP);
    }
}
