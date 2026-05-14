//! Sidebar departures widget. Subscribes to
//! [`hytte::services::departures::current()`] and renders the current
//! eight S-Bahn departures as a vertical list. Relative time labels
//! re-render on every emission of [`hytte::services::clock::now()`].

use chrono::{DateTime, Local};

/// Human-readable "minutes from now" label. Negative deltas and anything
/// within the next 60 s render as `"now"`. Above that, we round to the
/// nearest minute so `"7 min"` covers `[6m31s, 7m30s]`.
#[must_use]
pub fn relative_label(now: DateTime<Local>, departure: DateTime<Local>) -> String {
    let seconds = departure.signed_duration_since(now).num_seconds();
    if seconds <= 60 {
        return "now".to_string();
    }
    let minutes = (seconds + 30) / 60;
    format!("{minutes} min")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2030, 1, 1, h, m, s).unwrap()
    }

    #[test]
    fn relative_label_within_60s_is_now() {
        let now = at(16, 0, 0);
        assert_eq!(relative_label(now, at(16, 0, 30)), "now");
    }

    #[test]
    fn relative_label_in_the_past_is_now() {
        let now = at(16, 0, 30);
        assert_eq!(relative_label(now, at(16, 0, 0)), "now");
    }

    #[test]
    fn relative_label_rounds_up_at_31_seconds() {
        // 7m31s rounds up to 8.
        let now = at(16, 0, 0);
        assert_eq!(relative_label(now, at(16, 7, 31)), "8 min");
    }

    #[test]
    fn relative_label_rounds_down_at_29_seconds() {
        // 7m29s rounds down to 7.
        let now = at(16, 0, 0);
        assert_eq!(relative_label(now, at(16, 7, 29)), "7 min");
    }

    #[test]
    fn relative_label_one_minute_at_61s() {
        let now = at(16, 0, 0);
        assert_eq!(relative_label(now, at(16, 1, 1)), "1 min");
    }
}
