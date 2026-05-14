//! Sidebar departures widget. Subscribes to
//! [`hytte::services::departures::current()`] and renders the current
//! eight S-Bahn departures as a vertical list. Relative time labels
//! re-render on every emission of [`hytte::services::clock::now()`].

use chrono::{DateTime, Local};
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::{clock, departures};
use hytte::services::departures::{delay_string, Departure};

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

/// Build one row widget for `d`. The time cell re-renders on every clock
/// tick by binding to `clock::now()`. The row's CSS classes encode line
/// and cancellation state so styling is purely declarative.
fn row(d: &Departure) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("ts-departure-row");
    if d.cancelled {
        row.add_css_class("ts-cancelled");
    }

    // Line badge.
    let badge = gtk::Label::new(Some(&d.line));
    badge.add_css_class("ts-line-badge");
    badge.add_css_class(&format!("ts-line-{}", d.line));
    badge.set_halign(gtk::Align::Start);
    row.append(&badge);

    // Direction (takes the slack).
    let direction = gtk::Label::new(Some(&d.direction));
    direction.add_css_class("ts-departure-direction");
    direction.set_halign(gtk::Align::Start);
    direction.set_hexpand(true);
    direction.set_xalign(0.0);
    direction.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.append(&direction);

    // Time cell — re-renders each clock tick.
    let time_lbl = gtk::Label::new(None);
    time_lbl.add_css_class("ts-departure-time");
    let actual = d.actual;
    bind(clock::now(), &time_lbl, move |lbl, now| {
        let rel = relative_label(now, actual);
        lbl.set_text(&format!("{rel} · {}", actual.format("%H:%M")));
    });
    row.append(&time_lbl);

    // Delay indicator (hidden when on time).
    if let Some(text) = delay_string(d.delay_minutes) {
        let delay = gtk::Label::new(Some(&text));
        delay.add_css_class("ts-departure-delay");
        row.append(&delay);
    }

    row.upcast()
}

fn loading_row() -> gtk::Widget {
    let lbl = gtk::Label::new(Some("loading departures…"));
    lbl.add_css_class("ts-departures-loading");
    lbl.set_halign(gtk::Align::Start);
    lbl.upcast()
}

fn empty_row() -> gtk::Widget {
    let lbl = gtk::Label::new(Some("no S-Bahn departures in the next 30 min"));
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
