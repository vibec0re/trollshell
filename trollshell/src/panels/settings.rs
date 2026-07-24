//! Drawer panel exposing trollshell-wide preferences. v1 (minimal) covers:
//!
//! - Dark mode — delegated to `hytte::services::theme`, which fans out across
//!   GTK4/libadwaita, legacy GTK (gsettings + settings.ini), and Qt
//!   (qt[56]ct.conf). The switch reads the current theme once at page mount
//!   and writes back on toggle; we do NOT live-track external changes.
//!   Trollshell *is* the compositor session, so "follow system" is
//!   meaningless — if gsettings reads back `default` (externally set), the
//!   service surfaces Dark and the next user pick makes it canonical.
//! - Keep awake (#513) — the on-demand "stop idle-locking" caffeine toggle.
//!   Duplicates the switch in `panel_power`'s "Keep awake" section (both drive
//!   `screensaver::set_keep_awake` / observe `screensaver::keep_awake()`, so
//!   they stay in sync). It lives here because the Settings chip is always in
//!   the bar, while the Power panel hides with the battery/brightness chips on
//!   desktops.
//! - Do Not Disturb — duplicates the toggle at the top of `panel_notifications`.
//!   Both bindings drive the same `dnd::set_enabled` setter and observe the
//!   same `dnd::enabled` signal, so they stay in sync.
//! - Record audio (#421) — whether the next screen recording captures audio
//!   (`wf-recorder --audio`). Session-only, like `recorder::state` itself;
//!   `TROLLSHELL_RECORD_AUDIO=1` only seeds the starting value.
//!
//! Future v1.x: bar/drawer layout, idle timeouts (the native idle manager's
//! thresholds are currently compile-time constants), accent color, notification
//! policy.

use hytte::adw::{self, prelude::*};
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::dnd;
use hytte::services::power_profiles;
use hytte::services::recorder;
use hytte::services::screensaver;

use crate::components::deep_link_row::deep_link_row;
use crate::components::layout::{finish_page, page_box};
use crate::components::power_profile::build_power_profile_expander;

pub fn panel_settings() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    // ── Appearance ────────────────────────────────────────────────────────
    let appearance = adw::PreferencesGroup::builder().title("Appearance").build();

    let theme_row = adw::ActionRow::builder().title("Dark mode").build();

    let theme_switch = gtk::Switch::new();
    theme_switch.set_valign(gtk::Align::Center);
    theme_switch.set_active(matches!(
        hytte::services::theme::current(),
        hytte::services::theme::Theme::Dark
    ));
    theme_switch.connect_active_notify(|sw| {
        hytte::services::theme::set(if sw.is_active() {
            hytte::services::theme::Theme::Dark
        } else {
            hytte::services::theme::Theme::Light
        });
    });
    theme_row.add_suffix(&theme_switch);
    theme_row.set_activatable_widget(Some(&theme_switch));
    appearance.add(&theme_row);

    column.append(&appearance);

    // ── Keep awake ────────────────────────────────────────────────────────
    // The on-demand "stop idle-locking" switch (#513). It duplicates the
    // caffeine toggle in `panel_power`'s "Keep awake" section: both drive
    // `screensaver::set_keep_awake` and observe the authoritative
    // `screensaver::keep_awake()` signal, so they stay in sync
    // (daemon-as-state-store; `set_keep_awake` is idempotent, so the mirrored
    // programmatic `set_active` can't thrash the logind fd). Surfaced here
    // because the Settings chip is always in the bar, whereas the Power panel
    // is reachable only via the battery / brightness chips — both of which
    // hide on desktops with no battery or backlight, leaving the toggle
    // otherwise unreachable there.
    let awake = adw::PreferencesGroup::builder()
        .title("Keep awake")
        .description("Stop the screen dimming or locking while idle")
        .build();

    let awake_row = adw::SwitchRow::builder().title("Keep awake").build();
    // Two-way: the authoritative signal drives `active` (block guards re-entry
    // of the programmatic set); a user flip calls the idempotent setter.
    bind_two_way(
        screensaver::keep_awake(),
        &awake_row,
        adw::SwitchRow::set_active,
        |r| r.connect_active_notify(|r| screensaver::set_keep_awake(r.is_active())),
    );
    // Subtitle: what else is holding the system awake (Firefox, mpv, screen
    // share, …), so an off toggle doesn't imply the screen will sleep. Reuses
    // the Power panel's helper for an identical live subtitle.
    bind(screensaver::other_inhibitors(), &awake_row, |r, others| {
        r.set_subtitle(&crate::panels::power::keep_awake_subtitle(&others));
    });
    awake.add(&awake_row);

    column.append(&awake);

    // ── Notifications ─────────────────────────────────────────────────────
    let notif = adw::PreferencesGroup::builder()
        .title("Notifications")
        .build();

    let dnd_row = adw::ActionRow::builder()
        .title("Do Not Disturb")
        .subtitle("Suppress non-critical toasts; history still records.")
        .build();
    let dnd_switch = gtk::Switch::new();
    dnd_switch.set_valign(gtk::Align::Center);
    bind_two_way(dnd::enabled(), &dnd_switch, gtk::Switch::set_active, |w| {
        w.connect_active_notify(|sw| dnd::set_enabled(sw.is_active()))
    });
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

    // ── Recording ─────────────────────────────────────────────────────────
    // Audio-capture toggle for the screen-recording chip (#421). Applies to
    // the *next* recording only — mirrors `recorder::set_audio_enabled`'s own
    // doc: it never touches a recording already in progress.
    let recording = adw::PreferencesGroup::builder().title("Recording").build();

    let audio_row = adw::ActionRow::builder()
        .title("Record audio")
        .subtitle("Capture audio on the next screen recording (wf-recorder --audio).")
        .build();
    let audio_switch = gtk::Switch::new();
    audio_switch.set_valign(gtk::Align::Center);
    bind_two_way(
        recorder::audio_enabled(),
        &audio_switch,
        gtk::Switch::set_active,
        |sw| sw.connect_active_notify(|sw| recorder::set_audio_enabled(sw.is_active())),
    );
    audio_row.add_suffix(&audio_switch);
    audio_row.set_activatable_widget(Some(&audio_switch));
    recording.add(&audio_row);

    column.append(&recording);

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
