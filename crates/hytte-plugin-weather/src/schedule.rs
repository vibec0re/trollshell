//! Wall-clock scheduling decisions for the weather worker and the reducer.
//!
//! Everything here is a pure function of `SystemTime` values passed in, so the
//! behaviour is testable without sleeping. Wall-clock time (not `Instant`) is
//! deliberate: on Linux `Instant` and `tokio::time` run on `CLOCK_MONOTONIC`,
//! which does not advance while the machine is suspended, so a monotonic timer
//! can wake up to a whole period late after resume. A short monotonic tick plus
//! a wall-clock "is it due?" check closes that gap.

use std::time::{Duration, SystemTime};

use crate::location::LocationSnapshot;

/// How often the worker wakes to ask "is anything due?". After a suspend the
/// first tick lands at most this late, which bounds the post-resume delay.
pub(crate) const TICK: Duration = Duration::from_mins(1);

/// A reading older than this without a successful refresh is shown as stale.
/// Four missed 15-minute polls: long enough that one or two transient failures
/// never flag a perfectly good reading, short enough that conditions have
/// plausibly changed and "current weather" would be a lie.
pub(crate) const STALE_AFTER: Duration = Duration::from_hours(1);

/// How often the location is re-resolved. `GeoClue` is asked at city accuracy
/// via a short-lived client (started, read once, dropped), so this is one fix
/// per hour: frequent enough that a laptop which travelled stops reporting the
/// old city within the hour, rare enough to stay far from continuous tracking.
pub(crate) const LOCATION_RECHECK: Duration = Duration::from_hours(1);

/// A gap between two worker wakes longer than this means the machine slept
/// (the tick is [`TICK`]); the location is re-resolved straight away because
/// the likeliest reason to wake somewhere new is having travelled.
pub(crate) const RESUME_GAP: Duration = Duration::from_mins(5);

/// Two fixes closer than this (degrees, about 1 km) are the same place;
/// `GeoClue` jitter must not trigger a refetch.
const SAME_PLACE_DEG: f64 = 0.01;

/// First retry delay after a failed attempt; doubles per further failure.
const RETRY_BASE: Duration = Duration::from_mins(1);

/// How long to wait before the next attempt given `failures` consecutive
/// failures so far: the full `cap` (the normal period) after a success, else
/// one minute doubling per failure, never above `cap`. The minute floor is
/// also the worker's tick, so a retry is never sooner than the next wake.
pub(crate) fn interval_after(failures: u32, cap: Duration) -> Duration {
    if failures == 0 {
        return cap;
    }
    RETRY_BASE
        .saturating_mul(1u32 << (failures - 1).min(16))
        .min(cap)
}

/// Time from `since` to `now`, or `None` when the wall clock went backwards.
fn elapsed(since: SystemTime, now: SystemTime) -> Option<Duration> {
    now.duration_since(since).ok()
}

/// Whether something last done at `last` is due again after `interval`. Never
/// done is due; a clock that stepped backwards is also due, so a bad clock
/// cannot postpone refreshes indefinitely.
pub(crate) fn due(last: Option<SystemTime>, now: SystemTime, interval: Duration) -> bool {
    match last {
        None => true,
        Some(t) => elapsed(t, now).is_none_or(|e| e >= interval),
    }
}

/// Whether a reading fetched at `fetched_at` is stale at `now`. A backwards
/// clock step is not stale: nothing is known about the age.
pub(crate) fn is_stale(fetched_at: SystemTime, now: SystemTime) -> bool {
    elapsed(fetched_at, now).is_some_and(|e| e >= STALE_AFTER)
}

/// Whether the gap since the previous worker wake looks like a suspend.
pub(crate) fn resumed(last_wake: SystemTime, now: SystemTime) -> bool {
    elapsed(last_wake, now).is_some_and(|e| e > RESUME_GAP)
}

/// Whether two resolved locations are the same place (to about 1 km).
pub(crate) fn same_place(a: &LocationSnapshot, b: &LocationSnapshot) -> bool {
    (a.lat - b.lat).abs() < SAME_PLACE_DEG
        && (a.lon - b.lon).abs() < SAME_PLACE_DEG
        && a.label_hint == b.label_hint
}

/// "N min" / "N h" / "N d" age text for the stale line; `None` on a backwards
/// clock.
pub(crate) fn age_text(fetched_at: SystemTime, now: SystemTime) -> Option<String> {
    let e = elapsed(fetched_at, now)?;
    let mins = e.as_secs() / 60;
    Some(if mins < 120 {
        format!("{mins} min")
    } else if mins < 48 * 60 {
        format!("{} h", mins / 60)
    } else {
        format!("{} d", mins / (24 * 60))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::location::LocationSource;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + secs)
    }

    fn loc(lat: f64, lon: f64, hint: Option<&str>) -> LocationSnapshot {
        LocationSnapshot {
            lat,
            lon,
            label_hint: hint.map(str::to_owned),
            source: LocationSource::GeoClue,
        }
    }

    #[test]
    fn retry_backs_off_from_a_minute_up_to_the_cap() {
        let cap = Duration::from_mins(15);
        assert_eq!(interval_after(0, cap), cap);
        assert_eq!(interval_after(1, cap), Duration::from_mins(1));
        assert_eq!(interval_after(2, cap), Duration::from_mins(2));
        assert_eq!(interval_after(4, cap), Duration::from_mins(8));
        assert_eq!(interval_after(5, cap), cap);
        assert_eq!(interval_after(u32::MAX, cap), cap);
        assert_eq!(
            interval_after(1, LOCATION_RECHECK),
            Duration::from_mins(1),
            "a failed re-resolve is retried in a minute, not an hour"
        );
    }

    #[test]
    fn a_failed_attempt_is_due_again_in_a_minute_not_a_period() {
        let cap = Duration::from_mins(15);
        let i = interval_after(1, cap);
        assert!(!due(Some(at(0)), at(59), i));
        assert!(due(Some(at(0)), at(60), i));
        assert!(!due(Some(at(0)), at(60), interval_after(0, cap)));
    }

    #[test]
    fn never_done_is_due() {
        assert!(due(None, at(0), Duration::from_mins(15)));
    }

    #[test]
    fn due_exactly_at_the_interval_not_before() {
        let i = Duration::from_mins(15);
        assert!(!due(Some(at(0)), at(899), i));
        assert!(due(Some(at(0)), at(900), i));
    }

    #[test]
    fn a_whole_suspend_makes_the_refresh_due_on_the_first_wake() {
        // Slept 3 h wall-clock; the monotonic interval would have counted ~0.
        assert!(due(Some(at(0)), at(3 * 3600), Duration::from_mins(15)));
    }

    #[test]
    fn backwards_clock_is_due_but_never_stale() {
        assert!(due(Some(at(100)), at(0), Duration::from_mins(15)));
        assert!(!is_stale(at(100), at(0)));
    }

    #[test]
    fn stale_after_an_hour_without_a_refresh() {
        assert!(!is_stale(at(0), at(3599)));
        assert!(is_stale(at(0), at(3600)));
    }

    #[test]
    fn resume_is_a_gap_well_beyond_the_tick() {
        assert!(!resumed(at(0), at(61)));
        assert!(!resumed(at(0), at(300)));
        assert!(resumed(at(0), at(301)));
        assert!(!resumed(at(100), at(0)));
    }

    #[test]
    fn same_place_ignores_jitter_but_not_a_move_or_a_rename() {
        let a = loc(52.52, 13.40, None);
        assert!(same_place(&a, &loc(52.524, 13.404, None)));
        assert!(!same_place(&a, &loc(48.14, 11.58, None)));
        assert!(!same_place(&a, &loc(52.52, 13.40, Some("Berlin"))));
    }

    #[test]
    fn age_text_picks_a_readable_unit() {
        assert_eq!(age_text(at(0), at(90 * 60)).as_deref(), Some("90 min"));
        assert_eq!(age_text(at(0), at(5 * 3600)).as_deref(), Some("5 h"));
        assert_eq!(age_text(at(0), at(72 * 3600)).as_deref(), Some("3 d"));
        assert_eq!(age_text(at(10), at(0)), None);
    }
}
