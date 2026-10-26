use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::brightness;

use crate::components::chip::wire_scroll;

/// Brightness change per scroll notch.
const BRIGHTNESS_STEP: f64 = 0.05;

/// How long the chip trusts the level last asked for over the polled one.
///
/// The service re-reads sysfs once a second, so a burst of scroll notches
/// lands well inside one poll: computing each notch from the polled level
/// made five fast notches move the backlight one step, not five. Inside this
/// window a notch steps from the last request instead ([`LAST_REQUEST`]: the
/// chip's own, or the drawer slider's). It has to outlast a poll period plus
/// logind's write, or a reading taken just before the first write landed would
/// pull the base back mid-burst; past it, the polled level is authoritative
/// again, so a change made outside the shell (Fn keys, `brightnessctl`) is what
/// the next notch starts from.
const SCROLL_HOLD: Duration = Duration::from_millis(1500);

/// The level a brightness control last asked for, and when.
type LastRequest = Cell<Option<(f64, Instant)>>;

thread_local! {
    /// The last request of **any** brightness control: every monitor's chip and
    /// the Power drawer's slider record theirs here, and a chip's notch steps
    /// from it while it is under [`SCROLL_HOLD`] old.
    ///
    /// It used to be each chip's own, and the slider wrote nothing to it. So a
    /// notch inside the hold of a slider drag stepped from the chip's own older
    /// request, not from the slider's: scroll the chip up to 55 %, drag the
    /// slider down to 20 %, scroll up again within the hold, and the backlight
    /// jumped to 60 % (#1559). One record means whichever control moved last is
    /// what the next notch starts from, across chips on different monitors too.
    ///
    /// Thread-local, because every control that writes it lives on the GTK main
    /// thread.
    static LAST_REQUEST: LastRequest = const { Cell::new(None) };
}

/// The level one scroll notch asks for.
///
/// `polled` is the service's last reading; `pending` is the last request any
/// brightness control made ([`LAST_REQUEST`]) and when it was made.
/// `direction` is `wire_scroll`'s `±1.0`, where GDK's `dy` is positive for
/// scroll-down — treated as "decrease", so scroll-up raises brightness,
/// matching the volume chip. The result is clamped by the service's own rule,
/// so a run of notches past either end does not accumulate a base outside what
/// was actually written.
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

/// One scroll notch against `last`: the level to ask for, recorded in `last` as
/// the latest request.
fn notch(last: &LastRequest, polled: f64, now: Instant, direction: f64) -> f64 {
    let next = scroll_target(polled, last.get(), now, direction);
    last.set(Some((next, now)));
    next
}

/// A level the slider asked for, recorded in `last` as the latest request, so
/// the chip's next notch steps from it. Clamped the way [`brightness::set`]
/// clamps it, so the base is what was written. `None` for a `NaN`, which `set`
/// drops and so is no request at all.
fn slid(last: &LastRequest, level: f64, now: Instant) -> Option<f64> {
    if level.is_nan() {
        return None;
    }
    let level = brightness::clamp_level(level);
    last.set(Some((level, now)));
    Some(level)
}

/// What the Power drawer's brightness slider calls on `value-changed`: write
/// `level`, and make it the base the chip's next notch steps from.
pub(crate) fn set_from_slider(level: f64) {
    if let Some(level) = LAST_REQUEST.with(|last| slid(last, level, Instant::now())) {
        brightness::set(level);
    }
}

/// The level [`LAST_REQUEST`] holds on this thread, for the drawer row's
/// wiring test (`panels::power::slider_tests`).
#[cfg(all(test, feature = "system-tests"))]
pub(crate) fn last_request_level() -> Option<f64> {
    LAST_REQUEST.with(|last| last.get().map(|(level, _)| level))
}

/// Forget [`LAST_REQUEST`] on this thread. Every `#[gtk::test]` shares one GTK
/// thread, so a test that reads the record clears it first.
#[cfg(all(test, feature = "system-tests"))]
pub(crate) fn forget_last_request() {
    LAST_REQUEST.with(|last| last.set(None));
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
    // request is recent — the visibility bind above maps the level
    // away, so this is a second, dedicated subscription.
    let polled = Rc::new(Cell::new(0.0_f64));
    let polled_for_bind = Rc::clone(&polled);
    bind(brightness::current(), &btn, move |_, b| {
        if let Some(b) = b {
            polled_for_bind.set(b.level);
        }
    });

    wire_scroll(&btn, move |direction| {
        let next = LAST_REQUEST.with(|last| notch(last, polled.get(), Instant::now(), direction));
        brightness::set(next);
    });

    btn.upcast()
}

#[cfg(test)]
mod tests {
    use super::{BRIGHTNESS_STEP, LastRequest, SCROLL_HOLD, notch, scroll_target, slid};
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

    /// A notch inside the hold of a slider drag steps from where the slider
    /// left the backlight, not from the chip's own older request (#1559).
    ///
    /// The poll has not caught up with either write yet, which is the case the
    /// hold exists for: the polled level is still the one from before both.
    ///
    /// **Mutation:** make `slid` record nothing → this reds with `0.60`, a
    /// step up from the chip's own `0.55`.
    #[test]
    fn a_notch_right_after_a_slider_drag_steps_from_the_slider() {
        let last = LastRequest::new(None);
        let start = Instant::now();
        let polled = 0.50;

        let first = notch(&last, polled, start, UP);
        assert!(close(first, 0.55), "got {first}");

        let dragged = slid(&last, 0.20, start + Duration::from_millis(300));
        assert_eq!(dragged, Some(0.20));

        let next = notch(&last, polled, start + Duration::from_millis(600), UP);
        assert!(close(next, 0.20 + BRIGHTNESS_STEP), "got {next}");
    }

    /// The slider's request is recorded as the service writes it: clamped to the
    /// floor, so a notch after it does not step from below what was written. A
    /// `NaN` (which `brightness::set` drops) records nothing.
    #[test]
    fn the_slider_records_the_level_as_written() {
        let last = LastRequest::new(None);
        let now = Instant::now();

        assert_eq!(slid(&last, f64::NAN, now), None);
        assert_eq!(last.get(), None, "a dropped request is no base");

        assert_eq!(slid(&last, 0.0, now), Some(MIN_LEVEL));
        let up = notch(&last, 0.50, now + Duration::from_millis(100), UP);
        assert!(close(up, MIN_LEVEL + BRIGHTNESS_STEP), "got {up}");
    }

    /// Past the hold, a slider request is no more authoritative than a chip's:
    /// the polled level is the base again.
    #[test]
    fn past_the_hold_a_slider_request_gives_way_to_the_poll() {
        let last = LastRequest::new(None);
        let then = Instant::now();
        slid(&last, 0.90, then);
        let next = notch(&last, 0.30, then + SCROLL_HOLD, UP);
        assert!(close(next, 0.30 + BRIGHTNESS_STEP), "got {next}");
    }
}
