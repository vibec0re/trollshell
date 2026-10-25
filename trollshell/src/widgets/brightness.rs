use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::brightness;

use crate::components::chip::wire_scroll;

/// Brightness change per scroll notch.
const BRIGHTNESS_STEP: f64 = 0.05;

/// How long the chip trusts the level it last asked for over the polled one.
///
/// The service re-reads sysfs once a second, so a burst of scroll notches
/// lands well inside one poll: computing each notch from the polled level
/// made five fast notches move the backlight one step, not five. Inside this
/// window a notch steps from the chip's own last request instead. It has to
/// outlast a poll period plus logind's write, or a reading taken just before
/// the first write landed would pull the base back mid-burst; past it, the
/// polled level is authoritative again, so a change made elsewhere (Fn keys,
/// the drawer's slider, `brightnessctl`) is what the next notch starts from.
const SCROLL_HOLD: Duration = Duration::from_millis(1500);

/// The level one scroll notch asks for.
///
/// `polled` is the service's last reading; `pending` is the chip's own last
/// request and when it was made. `direction` is `wire_scroll`'s `±1.0`, where
/// GDK's `dy` is positive for scroll-down — treated as "decrease", so
/// scroll-up raises brightness, matching the volume chip. The result is
/// clamped by the service's own rule, so a run of notches past either end
/// does not accumulate a base outside what was actually written.
fn scroll_target(
    polled: f64,
    pending: Option<(f64, Instant)>,
    now: Instant,
    direction: f64,
) -> f64 {
    let base = match pending {
        Some((level, at)) if now.saturating_duration_since(at) < SCROLL_HOLD => level,
        _ => polled,
    };
    brightness::clamp_level(base - direction * BRIGHTNESS_STEP)
}

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn =
        crate::components::chip::indicator("ts-brightness", crate::modal::Page::Power, monitor);

    let icon = gtk::Image::from_icon_name("display-brightness-symbolic");
    btn.set_child(Some(&icon));

    // Hide the indicator when no backlight device exists (desktops).
    bind(
        brightness::current().map(|b| b.is_some()),
        &btn,
        gtk::prelude::WidgetExt::set_visible,
    );

    // Last-bound level, kept around so the scroll handler has a base when no
    // request of its own is recent — the visibility bind above maps the level
    // away, so this is a second, dedicated subscription.
    let polled = Rc::new(Cell::new(0.0_f64));
    let polled_for_bind = Rc::clone(&polled);
    bind(brightness::current(), &btn, move |_, b| {
        if let Some(b) = b {
            polled_for_bind.set(b.level);
        }
    });

    let pending: Cell<Option<(f64, Instant)>> = Cell::new(None);
    wire_scroll(&btn, move |direction| {
        let now = Instant::now();
        let next = scroll_target(polled.get(), pending.get(), now, direction);
        pending.set(Some((next, now)));
        brightness::set(next);
    });

    btn.upcast()
}

#[cfg(test)]
mod tests {
    use super::{BRIGHTNESS_STEP, SCROLL_HOLD, scroll_target};
    use hytte::services::brightness::MIN_LEVEL;
    use std::time::{Duration, Instant};

    const UP: f64 = -1.0;
    const DOWN: f64 = 1.0;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// Five notches inside one poll period move five steps. Before, each one
    /// stepped from the polled level, which had not moved yet, so the burst
    /// moved one step.
    #[test]
    fn a_fast_burst_accumulates_from_the_chips_own_requests() {
        let start = Instant::now();
        let polled = 0.50;
        let mut pending = None;
        for i in 0..5 {
            let now = start + Duration::from_millis(100 * i);
            let next = scroll_target(polled, pending, now, UP);
            pending = Some((next, now));
        }
        let (level, _) = pending.unwrap();
        assert!(close(level, 0.50 + 5.0 * BRIGHTNESS_STEP), "got {level}");
    }

    /// Once the hold has passed, the polled level wins again — so a change
    /// made elsewhere is what the next notch starts from.
    #[test]
    fn after_the_hold_the_polled_level_is_the_base() {
        let then = Instant::now();
        let now = then + SCROLL_HOLD;
        let next = scroll_target(0.20, Some((0.90, then)), now, UP);
        assert!(close(next, 0.20 + BRIGHTNESS_STEP), "got {next}");
    }

    /// Scrolling down stops at the service's floor (it used to clamp to
    /// `0.0`, which is "off" on some panels), and the floor is also what the
    /// next notch up starts from — the base cannot drift below it.
    #[test]
    fn scrolling_down_stops_at_the_floor_and_comes_back_in_one_notch() {
        let start = Instant::now();
        let mut pending = None;
        for i in 0..10 {
            let now = start + Duration::from_millis(50 * i);
            let next = scroll_target(0.10, pending, now, DOWN);
            pending = Some((next, now));
        }
        let (floor, at) = pending.unwrap();
        assert!(close(floor, MIN_LEVEL), "got {floor}");

        let up = scroll_target(0.10, pending, at + Duration::from_millis(50), UP);
        assert!(close(up, MIN_LEVEL + BRIGHTNESS_STEP), "got {up}");
    }

    #[test]
    fn scrolling_up_stops_at_full() {
        let now = Instant::now();
        assert!(close(scroll_target(0.98, None, now, UP), 1.0));
    }
}
