//! Drawer panel exposing trollshell-wide preferences. v1 (minimal) covers two
//! knobs:
//!
//! - Theme (Light / Dark) — delegated to `hytte::services::theme`, which
//!   fans out across GTK4/libadwaita, legacy GTK (gsettings + settings.ini),
//!   and Qt (qt[56]ct.conf). The dropdown reads the current theme once at
//!   page mount and writes back on selection change; we do NOT live-track
//!   external changes. Trollshell *is* the compositor session, so "follow
//!   system" is meaningless — if gsettings reads back `default` (externally
//!   set), the service surfaces Dark and the next user pick makes it canonical.
//! - Do Not Disturb — duplicates the toggle at the top of `panel_notifications`.
//!   Both bindings drive the same `dnd::set_enabled` setter and observe the
//!   same `dnd::enabled` signal, so they stay in sync.
//!
//! Future v1.x: bar/drawer layout, idle timeouts (#28's swayidle is currently
//! hand-edited), accent color, notification policy.

use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::dnd;
use hytte::services::power_profiles::{self, humanize_profile};

use crate::components::deep_link_row::deep_link_row;
use crate::components::layout::{finish_page, page_box};

pub fn panel_settings() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    // ── Appearance ────────────────────────────────────────────────────────
    let appearance = adw::PreferencesGroup::builder().title("Appearance").build();

    let theme_row = adw::ActionRow::builder()
        .title("Theme")
        .subtitle("Light or dark.")
        .build();

    // Order: ["Light", "Dark"] — matches `theme_from_index` mapping.
    let theme_dropdown = gtk::DropDown::from_strings(&["Light", "Dark"]);
    theme_dropdown.set_valign(gtk::Align::Center);
    theme_dropdown.set_selected(theme_to_index(hytte::services::theme::current()));
    theme_dropdown.connect_selected_notify(|dd| {
        hytte::services::theme::set(theme_from_index(dd.selected()));
    });
    theme_row.add_suffix(&theme_dropdown);
    theme_row.set_activatable_widget(Some(&theme_dropdown));
    appearance.add(&theme_row);

    column.append(&appearance);

    // ── Notifications ─────────────────────────────────────────────────────
    let notif = adw::PreferencesGroup::builder().title("Notifications").build();

    let dnd_row = adw::ActionRow::builder()
        .title("Do Not Disturb")
        .subtitle("Suppress non-critical toasts; history still records.")
        .build();
    let dnd_switch = gtk::Switch::new();
    dnd_switch.set_valign(gtk::Align::Center);
    bind_two_way(
        dnd::enabled(),
        &dnd_switch,
        gtk::Switch::set_active,
        |w| w.connect_active_notify(|sw| dnd::set_enabled(sw.is_active())),
    );
    dnd_row.add_suffix(&dnd_switch);
    dnd_row.set_activatable_widget(Some(&dnd_switch));
    notif.add(&dnd_row);

    column.append(&notif);

    // ── Power ─────────────────────────────────────────────────────────────
    // Power profile (also surfaced in panel_power alongside battery +
    // brightness). Both bindings observe the same `power_profiles::state()`
    // signal so they stay in sync.
    let power_group = adw::PreferencesGroup::builder().title("Power").build();
    power_group.add(&build_power_profile_expander());
    // Hide the whole group when no power-profiles daemon is available
    // (e.g. desktops without `power-profiles-daemon`); same gate the
    // expander uses internally on its own visibility, but the group
    // header would otherwise hang there with no rows.
    bind(
        power_profiles::state().map(|s| !s.available.is_empty()),
        &power_group,
        gtk::prelude::WidgetExt::set_visible,
    );
    column.append(&power_group);

    // ── More ──────────────────────────────────────────────────────────────
    // Deep-link rows to drawer pages that don't have a dedicated bar chip.
    // Each row swaps the currently-open drawer to the target page via
    // `modal::switch_active` (see modal.rs) so the user stays on the same
    // monitor's drawer surface; no `&Monitor` is plumbed through here.
    let more = adw::PreferencesGroup::builder().title("More").build();

    more.add(&deep_link_row(
        "Wallpaper",
        Some("Pick a desktop background"),
        "preferences-desktop-wallpaper-symbolic",
        crate::modal::Page::Appearance,
    ));
    more.add(&deep_link_row(
        "Displays",
        Some("Output layout and resolution"),
        "video-display-symbolic",
        crate::modal::Page::Displays,
    ));
    more.add(&deep_link_row(
        "Clipboard history",
        Some("Recent copies from cliphist"),
        "edit-paste-symbolic",
        crate::modal::Page::Clipboard,
    ));

    column.append(&more);

    finish_page(&column)
}

/// Power-profile expander. Same shape as `panels::power::build_power_profile_expander`;
/// duplicated here so Settings can surface power profile alongside Theme +
/// Do-Not-Disturb as a top-level preference. Both expanders observe the same
/// signal and call the same setter, so flipping one updates the other in place.
fn build_power_profile_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder()
        .title("Power profile")
        .build();

    bind(
        power_profiles::state().map(|s| !s.available.is_empty()),
        &expander,
        gtk::prelude::WidgetExt::set_visible,
    );

    bind(
        power_profiles::state().map(|s| humanize_profile(&s.active)),
        &expander,
        |row, t| row.set_subtitle(&t),
    );

    let icon = gtk::Image::new();
    icon.set_valign(gtk::Align::Center);
    bind(
        power_profiles::state().map(|s| profile_icon_name(&s.active)),
        &icon,
        |w, name| w.set_icon_name(Some(name)),
    );
    expander.add_prefix(&icon);

    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let expander_for_bind = expander.clone();
    let rows_for_bind = rows_track.clone();
    bind(power_profiles::state(), &expander, move |_, state| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            expander_for_bind.remove(&row);
        }
        let mut new_rows = Vec::with_capacity(state.available.len());
        for profile in &state.available {
            let row = adw::ActionRow::builder()
                .title(humanize_profile(profile))
                .activatable(true)
                .build();
            if profile == &state.active {
                let check = gtk::Image::from_icon_name("object-select-symbolic");
                check.set_valign(gtk::Align::Center);
                row.add_suffix(&check);
            }
            let profile_owned = profile.clone();
            row.connect_activated(move |_| {
                power_profiles::set_active(&profile_owned);
            });
            expander_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    expander
}

fn profile_icon_name(active: &str) -> &'static str {
    match active {
        "performance" => "power-profile-performance-symbolic",
        "balanced" => "power-profile-balanced-symbolic",
        "power-saver" => "power-profile-power-saver-symbolic",
        _ => "system-run-symbolic",
    }
}

/// Theme dropdown index <-> `hytte::services::theme::Theme`. Order matches
/// the strings passed to `gtk::DropDown::from_strings` in `panel_settings`.
fn theme_from_index(i: u32) -> hytte::services::theme::Theme {
    match i {
        0 => hytte::services::theme::Theme::Light,
        _ => hytte::services::theme::Theme::Dark,
    }
}

fn theme_to_index(t: hytte::services::theme::Theme) -> u32 {
    match t {
        hytte::services::theme::Theme::Light => 0,
        hytte::services::theme::Theme::Dark => 1,
    }
}
