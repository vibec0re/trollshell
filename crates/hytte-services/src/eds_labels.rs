//! Shared date-label helpers for the two EDS-backed drawer pages
//! ([`crate::calendar`]'s `format_when`, [`crate::tasks`]'s `format_due`) —
//! issue #1172.
//!
//! Both modules independently formatted a date relative to "today" as one of
//! "Today", "Tomorrow", or `"Wed 15 Apr"` — `calendar.rs` called it
//! `short_date`, `tasks.rs` called the identical body `day_label` — plus the
//! weekday/month abbreviation tables that back the third case. One copy
//! drifting from the other (a renamed weekday abbreviation, a reordered
//! `day month` vs `month day`) would silently make the calendar and tasks
//! drawers disagree about how "the same date" reads, so this module is the
//! one place both now import from.

use chrono::{Datelike, NaiveDate};

/// Render a date as one of "Today", "Tomorrow", or `"Mon 14 Apr"` relative to
/// `today`. Used by both the calendar drawer (event start/end) and the tasks
/// drawer (a task's due date).
pub(crate) fn short_date(d: NaiveDate, today: NaiveDate) -> String {
    let delta = d.signed_duration_since(today).num_days();
    match delta {
        0 => "Today".to_string(),
        1 => "Tomorrow".to_string(),
        _ => format!(
            "{} {} {}",
            weekday_short(d.weekday()),
            d.day(),
            month_short(d.month()),
        ),
    }
}

pub(crate) fn weekday_short(w: chrono::Weekday) -> &'static str {
    match w {
        chrono::Weekday::Mon => "Mon",
        chrono::Weekday::Tue => "Tue",
        chrono::Weekday::Wed => "Wed",
        chrono::Weekday::Thu => "Thu",
        chrono::Weekday::Fri => "Fri",
        chrono::Weekday::Sat => "Sat",
        chrono::Weekday::Sun => "Sun",
    }
}

pub(crate) fn month_short(m: u32) -> &'static str {
    match m {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The byte-identity pin (#1172).** Before this module existed,
    /// `calendar.rs`'s `short_date` and `tasks.rs`'s `day_label` were two
    /// independent copies of this exact body — written and verified
    /// byte-identical (`diff` of the two functions was empty) before they
    /// were collapsed into this one implementation. These are their outputs,
    /// pinned as literals, so a future edit to the shared function that
    /// changes what either drawer displays fails here first instead of the
    /// two call sites quietly re-diverging one edit at a time.
    ///
    /// Falsification: swap the `weekday_short`/`day`/`month_short` order, or
    /// change a weekday/month abbreviation, and the last two assertions red.
    #[test]
    fn short_date_matches_the_pre_consolidation_output() {
        let today = NaiveDate::from_ymd_opt(2026, 4, 1).expect("valid date");

        assert_eq!(short_date(today, today), "Today");
        assert_eq!(
            short_date(today + chrono::Duration::days(1), today),
            "Tomorrow"
        );

        // 2026-04-15 is a Wednesday.
        let far = NaiveDate::from_ymd_opt(2026, 4, 15).expect("valid date");
        assert_eq!(short_date(far, today), "Wed 15 Apr");

        // A date before "today" (an overdue task, a past event edge) takes
        // the same weekday-form branch — there is no special "yesterday"
        // case at this layer (tasks.rs's `overdue_label` adds that on top).
        let past = NaiveDate::from_ymd_opt(2026, 3, 20).expect("valid date");
        assert_eq!(short_date(past, today), "Fri 20 Mar");
    }

    #[test]
    fn weekday_short_covers_every_day() {
        use chrono::Weekday::{Fri, Mon, Sat, Sun, Thu, Tue, Wed};
        assert_eq!(weekday_short(Mon), "Mon");
        assert_eq!(weekday_short(Tue), "Tue");
        assert_eq!(weekday_short(Wed), "Wed");
        assert_eq!(weekday_short(Thu), "Thu");
        assert_eq!(weekday_short(Fri), "Fri");
        assert_eq!(weekday_short(Sat), "Sat");
        assert_eq!(weekday_short(Sun), "Sun");
    }

    #[test]
    fn month_short_covers_every_month_and_falls_back_on_invalid() {
        let expected = [
            (1, "Jan"),
            (2, "Feb"),
            (3, "Mar"),
            (4, "Apr"),
            (5, "May"),
            (6, "Jun"),
            (7, "Jul"),
            (8, "Aug"),
            (9, "Sep"),
            (10, "Oct"),
            (11, "Nov"),
            (12, "Dec"),
        ];
        for (m, label) in expected {
            assert_eq!(month_short(m), label);
        }
        assert_eq!(month_short(0), "?");
        assert_eq!(month_short(13), "?");
    }
}
