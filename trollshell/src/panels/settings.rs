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

use hytte::adw::{self, prelude::*};
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::dnd;

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
