//! Sidebar departures widget. Subscribes to
//! [`hytte::services::departures::current()`] and renders the current
//! eight S-Bahn departures as a vertical list. Relative time labels
//! re-render on every emission of [`hytte::services::clock::now()`].
//!
//! That same per-second tick also prunes the list: a row whose train has
//! actually departed (past [`DEPARTED_GRACE`]) is hidden, so already-gone
//! trains don't linger as "now" between the service's polls. With a walk
//! budget the lead cell is a `walk` icon + the leave-by remainder ("7 min" /
//! "now") rather than the wordy "leave …", keeping the row narrow.

use chrono::{DateTime, Local};
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::departures::{Departure, DeparturesState, delay_string};
use hytte::services::{clock, departures};
use std::cell::RefCell;
use std::rc::Rc;

/// Handle to one row's mutable time cell — used by the widget-level clock
/// subscription to update the "X min · HH:MM" string (and the leave-by fade)
/// in place each tick, without rebuilding the list.
struct TimeRowRef {
    actual: DateTime<Local>,
    /// Walk budget to the platform (minutes); `0` = plain departs-in label.
    walk_minutes: u32,
    time_lbl: gtk::Label,
    /// The row container, so the clock tick can toggle the unreachable fade.
    row_box: gtk::Box,
}

#[derive(Default, Clone)]
struct TimeRows(Rc<RefCell<Vec<TimeRowRef>>>);

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

/// The relative token shown before "· HH:MM". With no walk budget
/// (`walk_minutes == 0`) it's the plain departs-in label from
/// [`relative_label`]. With a positive budget it's a leave-by countdown — whole
/// minutes until you must leave to still catch the train (`"7 min"`),
/// collapsing to `"now"` once that hits zero. The leave-by case is marked by a
/// `walk` icon the caller prepends (so the bare number reads as "leave in"),
/// which is why the token here omits the word "leave". The returned bool is
/// whether the train is already unreachable (you can't make it even leaving
/// this instant), which the caller renders faded.
#[must_use]
pub fn lead_label(
    now: DateTime<Local>,
    departs: DateTime<Local>,
    walk_minutes: u32,
) -> (String, bool) {
    if walk_minutes == 0 {
        return (relative_label(now, departs), false);
    }
    // Seconds of slack: how long you can still wait before you must leave.
    let slack = departs.signed_duration_since(now).num_seconds() - i64::from(walk_minutes) * 60;
    let minutes = (slack + 30) / 60;
    let token = if minutes <= 0 {
        "now".to_string()
    } else {
        format!("{minutes} min")
    };
    (token, slack < 0)
}

/// How long after a train's actual departure time we keep its row on screen
/// before hiding it. A small grace absorbs clock skew and lets "now" linger a
/// beat rather than vanishing the instant the scheduled second ticks past.
const DEPARTED_GRACE: chrono::Duration = chrono::Duration::seconds(30);

/// Whether a train counts as already gone — its actual departure is more than
/// [`DEPARTED_GRACE`] in the past. Pure so the prune boundary is unit-testable.
#[must_use]
pub fn departed(now: DateTime<Local>, actual: DateTime<Local>) -> bool {
    now.signed_duration_since(actual) > DEPARTED_GRACE
}

/// Apply the current time/leave label, the unreachable fade, and the
/// departed-row prune to one row. Shared by the initial paint in [`row`] and
/// the per-tick clock subscription so they never drift. A train that has
/// already left (past [`DEPARTED_GRACE`]) is hidden, so the open board doesn't
/// keep showing departures from the past between the service's polls.
fn apply_row(r: &TimeRowRef, now: DateTime<Local>) {
    r.row_box.set_visible(!departed(now, r.actual));

    let (token, unreachable) = lead_label(now, r.actual, r.walk_minutes);
    r.time_lbl
        .set_text(&format!("{token} · {}", r.actual.format("%H:%M")));
    if unreachable {
        r.row_box.add_css_class("ts-departure-unreachable");
    } else {
        r.row_box.remove_css_class("ts-departure-unreachable");
    }
}

/// Build one row widget for `d` and a `TimeRowRef` for the widget-level
/// clock subscription to update the time label in place. No per-row
/// `bind(clock::now(), …)` — that would spawn a new glib task on every
/// rebuild, leaking without bound.
fn row(d: &Departure) -> (gtk::Widget, TimeRowRef) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("ts-departure-row");
    if d.cancelled {
        row.add_css_class("ts-cancelled");
    }

    // Line badge. Sanitize the line name for the CSS class so a stray
    // non-alphanumeric character from the API (e.g. "Bus 194", "SEV S9")
    // doesn't trip gtk::add_css_class's debug-mode assertion.
    let badge = gtk::Label::new(Some(&d.line));
    badge.add_css_class("ts-line-badge");
    let safe_line: String = d
        .line
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    badge.add_css_class(&format!("ts-line-{safe_line}"));
    badge.set_halign(gtk::Align::Start);
    row.append(&badge);

    // Direction (takes the slack). max_width_chars guarantees ellipsis
    // triggers even when the sidebar is wide enough that hexpand alone
    // wouldn't have constrained the label.
    let direction = gtk::Label::new(Some(&d.direction));
    direction.add_css_class("ts-departure-direction");
    direction.set_halign(gtk::Align::Start);
    direction.set_hexpand(true);
    direction.set_xalign(0.0);
    direction.set_ellipsize(gtk::pango::EllipsizeMode::End);
    direction.set_max_width_chars(22);
    row.append(&direction);

    // Time cell — an optional walk icon (leave-by mode) followed by the
    // "{token} · HH:MM" label. The label text is set initially via apply_row
    // against the current clock, then updated in place by the widget-level
    // clock subscription; the icon is static (a row's walk budget never
    // changes). No per-row bind here.
    let time_cell = gtk::Box::new(gtk::Orientation::Horizontal, 3);
    if d.walk_minutes > 0 {
        let walk = gtk::Image::from_file(crate::assets::path("icons/walk.svg"));
        walk.set_pixel_size(crate::scale::scale(14));
        walk.set_valign(gtk::Align::Center);
        walk.add_css_class("ts-departure-walk-icon");
        time_cell.append(&walk);
    }
    let time_lbl = gtk::Label::new(None);
    time_lbl.add_css_class("ts-departure-time");
    time_cell.append(&time_lbl);
    row.append(&time_cell);

    // Delay indicator (hidden when on time).
    if let Some(text) = delay_string(d.delay_minutes) {
        let delay = gtk::Label::new(Some(&text));
        delay.add_css_class("ts-departure-delay");
        row.append(&delay);
    }

    let row_ref = TimeRowRef {
        actual: d.actual,
        walk_minutes: d.walk_minutes,
        time_lbl,
        row_box: row.clone(),
    };
    // Initial paint so a freshly-built row is correct before the first tick.
    apply_row(&row_ref, chrono::Local::now());
    (row.upcast(), row_ref)
}

fn loading_row() -> gtk::Widget {
    let lbl = gtk::Label::new(Some("loading departures…"));
    lbl.add_css_class("ts-departures-loading");
    lbl.set_halign(gtk::Align::Start);
    lbl.upcast()
}

fn empty_row() -> gtk::Widget {
    let lbl = gtk::Label::new(Some("no matching S-Bahn departures right now"));
    lbl.add_css_class("ts-departures-empty");
    lbl.set_halign(gtk::Align::Start);
    lbl.upcast()
}

fn error_row(err: &str) -> gtk::Widget {
    let lbl = gtk::Label::new(Some(&format!("can't reach BVG: {err}")));
    lbl.add_css_class("ts-departures-error");
    lbl.set_halign(gtk::Align::Start);
    lbl.set_wrap(true);
    lbl.upcast()
}

fn stale_footer(err: &str, at: DateTime<Local>) -> gtk::Widget {
    let lbl = gtk::Label::new(Some(&format!(
        "· stale (last good {} — {})",
        at.format("%H:%M"),
        err
    )));
    lbl.add_css_class("ts-departures-stale-footer");
    lbl.set_halign(gtk::Align::Start);
    lbl.set_wrap(true);
    lbl.upcast()
}

/// Drain `list` and re-populate it from `state`. Eight rows max, so a
/// remove-all + append-fresh cycle per emission is cheap. Clears and
/// refills `time_rows` so the widget-level clock subscription always
/// addresses exactly the current set of labels.
fn rebuild(list: &gtk::Box, state: &DeparturesState, time_rows: &TimeRows) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let mut rows = time_rows.0.borrow_mut();
    rows.clear();
    match state {
        DeparturesState::Loading => list.append(&loading_row()),
        DeparturesState::Err { err } => list.append(&error_row(err)),
        DeparturesState::Ok { items, .. } | DeparturesState::Stale { items, .. } => {
            if items.is_empty() {
                list.append(&empty_row());
            } else {
                for d in items {
                    let (w, r) = row(d);
                    list.append(&w);
                    rows.push(r);
                }
            }
            if let DeparturesState::Stale { err, at, .. } = state {
                list.append(&stale_footer(err, *at));
            }
        }
    }
}

/// Build the departures widget. Uses exactly two `bind` subscriptions on
/// the long-lived `list` widget: one for state changes (rebuilds the list
/// and refreshes `time_rows`), one for clock ticks (updates each row's
/// time label in place). This avoids the per-row leak where
/// `bind(clock::now(), &time_lbl, …)` would spawn a new glib task on
/// every rebuild without a cancellation handle.
#[must_use]
pub fn widget() -> gtk::Widget {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 6);
    list.add_css_class("ts-departures");
    list.set_valign(gtk::Align::Start);

    let time_rows = TimeRows::default();

    // State changes rebuild the list (clears + repopulates time_rows).
    let time_rows_for_state = time_rows.clone();
    bind(departures::current(), &list, move |list, state| {
        rebuild(list, &state, &time_rows_for_state);
    });

    // One clock subscription, updates every time label in place. The bind
    // future lives as long as `list` does, which is the lifetime of the
    // sidebar — exactly what we want.
    let time_rows_for_clock = time_rows.clone();
    bind(clock::now(), &list, move |_list, now| {
        for r in time_rows_for_clock.0.borrow().iter() {
            apply_row(r, now);
        }
    });

    list.upcast()
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

    // ── lead_label (leave-by countdown) ────────────────────────────────────--

    #[test]
    fn lead_label_zero_walk_is_plain_relative() {
        let now = at(16, 0, 0);
        // Falls back to the existing departs-in label; never unreachable.
        assert_eq!(
            lead_label(now, at(16, 7, 0), 0),
            ("7 min".to_string(), false)
        );
        assert_eq!(
            lead_label(now, at(16, 0, 30), 0),
            ("now".to_string(), false)
        );
    }

    #[test]
    fn lead_label_counts_down_to_leave_time() {
        let now = at(16, 0, 0);
        // 14 min out, 10 min walk → 4 min of slack before you must leave.
        assert_eq!(
            lead_label(now, at(16, 14, 0), 10),
            ("4 min".to_string(), false)
        );
    }

    #[test]
    fn lead_label_one_minute_slack() {
        let now = at(16, 0, 0);
        // 11 min out, 10 min walk → 1 min slack.
        assert_eq!(
            lead_label(now, at(16, 11, 0), 10),
            ("1 min".to_string(), false)
        );
    }

    #[test]
    fn lead_label_zero_slack_is_leave_now_but_still_reachable() {
        let now = at(16, 0, 0);
        // Exactly the walk window: leave this instant, still catchable (not faded).
        assert_eq!(
            lead_label(now, at(16, 10, 0), 10),
            ("now".to_string(), false)
        );
    }

    #[test]
    fn lead_label_negative_slack_is_unreachable() {
        let now = at(16, 0, 0);
        // 3 min out, 10 min walk → already missed: "now" + faded.
        assert_eq!(lead_label(now, at(16, 3, 0), 10), ("now".to_string(), true));
    }

    // ── departed prune ─────────────────────────────────────────────────────--

    #[test]
    fn departed_hides_only_after_grace() {
        let train = at(16, 0, 0);
        // Future and on-the-dot trains stay visible.
        assert!(!departed(at(15, 59, 0), train));
        assert!(!departed(at(16, 0, 0), train));
        // Within the 30 s grace → still shown; past it → hidden.
        assert!(!departed(at(16, 0, 30), train));
        assert!(departed(at(16, 0, 31), train));
    }
}
