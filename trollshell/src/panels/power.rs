//! Drawer panel for battery + brightness — battery state, power profile,
//! brightness slider, and keep-awake toggle.

use std::cell::Cell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::brightness;
use hytte::services::screensaver;
use hytte::services::upower::{self, Battery, BatteryState};

use crate::components::format::fmt_dur;
use crate::components::layout::{boxed_list, finish_page, page_box, section};
use crate::components::power_profile::build_power_profile_expander;

/// App name written into the sentinel screensaver inhibitor that the
/// keep-awake toggle creates. Matching against this (plus
/// [`KEEP_AWAKE_REASON`]) lets every per-monitor drawer instance find and
/// release the inhibitor regardless of which one created it.
const KEEP_AWAKE_APP: &str = "trollshell";

/// Reason string written into the sentinel inhibitor. Must be unique enough
/// that a third-party app is not accidentally mistaken for our toggle.
const KEEP_AWAKE_REASON: &str = "Keep awake (manual)";

pub fn panel_power() -> gtk::Widget {
    let column = page_box();

    let battery = section("Battery");

    let battery_group = adw::PreferencesGroup::new();
    let battery_row = adw::ActionRow::builder().title("Charge").build();
    bind(
        upower::battery().map(|b: Battery| describe_battery(&b)),
        &battery_row,
        |row, text| row.set_subtitle(&text),
    );
    let pct_lbl = gtk::Label::new(None);
    pct_lbl.add_css_class("dim-label");
    bind_text(
        upower::battery().map(|b: Battery| format!("{:.0}%", b.percentage)),
        &pct_lbl,
    );
    battery_row.add_suffix(&pct_lbl);
    battery_group.add(&battery_row);
    battery_group.add(&build_power_profile_expander());
    battery.append(&battery_group);
    column.append(&battery);

    let bright = section("Brightness");
    bright.append(&build_brightness_row());
    column.append(&bright);

    let keep_awake = section("Keep awake");
    keep_awake.append(&build_keep_awake_group());
    column.append(&keep_awake);

    finish_page(&column)
}

/// Adwaita-flavoured brightness control: icon + slider + live percentage,
/// inside a single boxed-list row so it visually matches the battery panel.
fn build_brightness_row() -> gtk::ListBox {
    let list = boxed_list();

    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);

    let inner = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    inner.set_margin_start(12);
    inner.set_margin_end(12);
    inner.set_margin_top(8);
    inner.set_margin_bottom(8);

    let icon = gtk::Image::from_icon_name("display-brightness-symbolic");
    icon.add_css_class("dim-label");
    inner.append(&icon);

    let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.05, 1.0, 0.05);
    slider.set_draw_value(false);
    slider.set_hexpand(true);

    bind_two_way(
        brightness::current(),
        &slider,
        |s, b| {
            if let Some(b) = b {
                s.set_value(b.level);
            }
            s.set_sensitive(b.is_some());
        },
        |s| s.connect_value_changed(|s| brightness::set(s.value())),
    );
    inner.append(&slider);

    let pct_lbl = gtk::Label::new(None);
    pct_lbl.add_css_class("dim-label");
    pct_lbl.set_width_chars(4);
    pct_lbl.set_xalign(1.0);
    bind_text(
        brightness::current().map(|b| match b {
            Some(b) => format!("{:.0}%", b.level * 100.0),
            None => "—".to_string(),
        }),
        &pct_lbl,
    );
    inner.append(&pct_lbl);

    row.set_child(Some(&inner));
    list.append(&row);
    list
}

/// `adw::PreferencesGroup` holding the keep-awake `adw::SwitchRow`.
///
/// The switch is driven by `screensaver::inhibitors()`: it is **on** iff the
/// sentinel inhibitor (`KEEP_AWAKE_APP` / `KEEP_AWAKE_REASON`) is present in
/// the daemon's list.  The toggle handler calls `screensaver::inhibit` /
/// `screensaver::uninhibit` to write to the daemon.
///
/// ## Feedback-loop prevention
///
/// `bind_two_way` blocks the `connect_active_notify` handler while it is
/// executing a signal-driven `set_active` call. This prevents the loop:
/// signal → `set_active(true)` → handler fires → `inhibit()` → signal → …
///
/// ## Multi-monitor consistency
///
/// The uninhibit cookie is sourced from `screensaver::inhibitors()` (the
/// daemon's authoritative state) rather than the `u32` returned by
/// `screensaver::inhibit`. This means any monitor's drawer instance can
/// release the inhibitor, and a drawer rebuild or `panel_power()` re-call
/// never loses track of the active cookie.
fn build_keep_awake_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    let row = adw::SwitchRow::builder().title("Keep awake").build();

    // Tracks the sentinel inhibitor's cookie, populated from the daemon's
    // inhibitors() signal. `Cell` is sufficient: single-threaded GTK main
    // loop, interior mutability, no need for `RefCell`.
    let cookie: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));

    // Bind 1 — update cookie cell + external-inhibitor subtitle together on
    // every inhibitors() emission. One subscription keeps both values in the
    // same event-loop turn, ensuring the cookie is always consistent with
    // what the subtitle reports.
    {
        let cookie = cookie.clone();
        bind(screensaver::inhibitors(), &row, move |r, list| {
            cookie.set(
                list.iter()
                    .find(|i| i.application == KEEP_AWAKE_APP && i.reason == KEEP_AWAKE_REASON)
                    .map(|i| i.cookie),
            );
            let external: Vec<&str> = list
                .iter()
                .filter(|i| !(i.application == KEEP_AWAKE_APP && i.reason == KEEP_AWAKE_REASON))
                .map(|i| i.application.as_str())
                .collect();
            if external.is_empty() {
                r.set_subtitle("");
            } else {
                r.set_subtitle(&format!("Also awake: {}", external.join(", ")));
            }
        });
    }

    // Bind 2 — two-way: signal drives `set_active`; user interaction calls
    // inhibit/uninhibit. bind_two_way blocks the `active-notify` handler
    // across every signal-driven `set_active`, so the feedback loop cannot
    // occur (see function-level doc).
    {
        let cookie = cookie.clone();
        bind_two_way(
            screensaver::inhibitors().map(|list| {
                list.iter()
                    .any(|i| i.application == KEEP_AWAKE_APP && i.reason == KEEP_AWAKE_REASON)
            }),
            &row,
            adw::SwitchRow::set_active,
            move |w| {
                w.connect_active_notify(move |r| {
                    if r.is_active() {
                        // Only inhibit if the sentinel isn't already live.
                        // In normal operation cookie is None here (bind_two_way
                        // blocks this handler during signal-driven set_active),
                        // but the guard makes the idempotent path explicit.
                        if cookie.get().is_none() {
                            // Intentionally discard the return value: the
                            // cookie is sourced from inhibitors() to stay
                            // correct across drawer rebuilds (see doc above).
                            let _ = screensaver::inhibit(KEEP_AWAKE_APP, KEEP_AWAKE_REASON);
                        }
                    } else if let Some(c) = cookie.get() {
                        screensaver::uninhibit(c);
                    }
                })
            },
        );
    }

    group.add(&row);
    group
}

fn describe_battery(b: &Battery) -> String {
    let state = match b.state {
        BatteryState::Charging => "Charging",
        BatteryState::Discharging => "Discharging",
        BatteryState::Empty => "Empty",
        BatteryState::FullyCharged => "Fully charged",
        BatteryState::PendingCharge => "Pending charge",
        BatteryState::PendingDischarge => "Pending discharge",
        BatteryState::Unknown => "Unknown",
    };
    let remaining = match b.state {
        BatteryState::Discharging => b.time_to_empty.map(|d| fmt_dur(d, "until empty")),
        BatteryState::Charging => b.time_to_full.map(|d| fmt_dur(d, "until full")),
        _ => None,
    };
    match remaining {
        Some(r) => format!("{state} \u{2014} {r}"),
        None => state.to_string(),
    }
}
