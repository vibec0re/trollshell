//! Drawer panel for battery + brightness — battery state, power profile,
//! brightness slider.

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::brightness;
use hytte::services::fullscreen_inhibit;
use hytte::services::screensaver::{self, Inhibitor};
use hytte::services::upower::{self, Battery, BatteryState};

use crate::components::format::fmt_dur;
use crate::components::layout::{boxed_list, finish_page, page_box, section};
use crate::components::power_profile::build_power_profile_expander;

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

    let awake = section("Keep awake");
    awake.append(&build_keep_awake_row());
    column.append(&awake);

    finish_page(&column)
}

/// "Keep awake" caffeine toggle: an `adw::SwitchRow` whose state is derived
/// from the daemon's authoritative inhibitor list (`screensaver::keep_awake()`),
/// NOT local widget state — so any monitor's drawer reflects the same toggle
/// and a drawer rebuild never loses track (issue #270). Flipping it acquires /
/// releases a logind idle-inhibitor fd held in the screensaver service; the
/// native idle manager honors that inhibitor (via logind's `BlockInhibited`)
/// and skips dim/lock while it's held.
fn build_keep_awake_row() -> gtk::ListBox {
    let list = boxed_list();

    let row = adw::SwitchRow::builder().title("Keep awake").build();

    // Two-way: the authoritative signal drives `active` (block prevents the
    // programmatic set_active from re-entering the handler); a user flip calls
    // set_keep_awake, which is idempotent so mirrored state can't thrash the fd.
    bind_two_way(
        screensaver::keep_awake(),
        &row,
        adw::SwitchRow::set_active,
        |r| r.connect_active_notify(|r| screensaver::set_keep_awake(r.is_active())),
    );

    // Subtitle: what else is holding the system awake (Firefox, mpv, screen
    // share, …), so an off toggle doesn't imply the screen will sleep.
    bind(screensaver::other_inhibitors(), &row, |r, others| {
        r.set_subtitle(&keep_awake_subtitle(&others));
    });

    list.append(&row);
    list.append(&build_fullscreen_inhibit_row());
    list
}

/// "Keep awake when fullscreen" policy toggle (#404): an `adw::SwitchRow`
/// bound to the authoritative `fullscreen_inhibit::enabled()` flag (default
/// ON, persisted). While it's on, the shell holds a logind idle inhibitor for
/// as long as a window is genuinely fullscreen (movie/game/presentation), so
/// the native idle manager skips dim/lock/suspend — an automatic sibling of
/// the manual caffeine switch above. Turning it off lets fullscreen windows
/// dim/lock as normal (well-behaved players like mpv/Firefox still self-inhibit
/// on their own).
fn build_fullscreen_inhibit_row() -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Keep awake when fullscreen")
        .subtitle("Hold off dim/lock while a window is fullscreen")
        .build();

    // Two-way, same shape as the caffeine row: the authoritative signal drives
    // `active` (block guards re-entry); a user flip calls set_enabled, which is
    // idempotent so mirrored state can't thrash the policy or its fd.
    bind_two_way(
        fullscreen_inhibit::enabled(),
        &row,
        adw::SwitchRow::set_active,
        |r| r.connect_active_notify(|r| fullscreen_inhibit::set_enabled(r.is_active())),
    );

    row
}

/// Build the "Keep awake" subtitle from the external inhibitors: a deduped
/// "Also awake: …" app list, or default help text when nothing else holds it.
fn keep_awake_subtitle(others: &[Inhibitor]) -> String {
    let mut names: Vec<&str> = Vec::new();
    for i in others {
        let name = i.application.as_str();
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        "Prevent the screen from blanking or locking".to_string()
    } else {
        format!("Also awake: {}", names.join(", "))
    }
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
