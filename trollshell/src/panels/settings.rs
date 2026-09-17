//! Drawer panel exposing trollshell-wide preferences. v1 (minimal) covers:
//!
//! - Dark mode — delegated to `hytte::services::theme`, which fans out across
//!   GTK4/libadwaita, legacy GTK (gsettings + settings.ini), and Qt
//!   (`qt[56]ct.conf`). The switch **binds** `theme::current_signal()` rather
//!   than reading a snapshot at page mount: the service's value is seeded by
//!   an async `gsettings get`, so a one-shot read taken while building the
//!   page is `None` by construction, and `modal::ensure_page` caches this
//!   page for the life of the shell — which is how a light session ended up
//!   with a permanently wrong "Dark mode: on" switch (#1192 review, HIGH-1).
//!   While the seed is in flight the switch is insensitive (unknown, not
//!   wrong); it becomes live when the answer lands, and follows an external
//!   change too. Trollshell *is* the compositor session, so "follow system"
//!   is meaningless — if gsettings reads back `default` (externally set),
//!   the service surfaces Dark and the next user pick makes it canonical.
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
use hytte::futures_signals::signal::Signal;
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::dnd;
use hytte::services::power_profiles;
use hytte::services::recorder;
use hytte::services::screensaver;
use hytte::services::theme::{self, Theme};

use crate::companion::{self, Route};
use crate::components::deep_link_row::deep_link_row;
use crate::components::layout::{finish_page, page_box};
use crate::components::power_profile::build_power_profile_expander;

/// The Dark-mode switch, bound to `current` (`theme::current_signal` in
/// production).
///
/// `current` is a **factory**, not a signal: the switch needs two
/// independent subscriptions (one for `sensitive`, one two-way for
/// `active`) and a `Signal` is consumed by its first binding. That also
/// makes the whole widget testable against a local `Mutable` without the
/// process-global theme handle — see this module's gated `mod tests`.
///
/// - **`None` (seed in flight) → insensitive.** Unknown is rendered as
///   unknown; a switch the user can flip while the answer is unknown would
///   write a theme off a position nobody chose (#1192 review, HIGH-1).
/// - **`Some(theme)` → sensitive, `active == is_dark`.**
/// - `bind_two_way`, for the reason `keep_awake` / DND below are: the
///   authoritative signal drives `active`, and the two-way bind blocks the
///   handler while it does, so the programmatic `set_active` that lands
///   when the seed arrives cannot re-enter `theme::set` and fan a whole
///   `gsettings` write (plus a `theme-changed` hook) out at startup.
fn build_theme_switch<S>(current: impl Fn() -> S) -> gtk::Switch
where
    S: Signal<Item = Option<Theme>> + 'static,
{
    let theme_switch = gtk::Switch::new();
    theme_switch.set_valign(gtk::Align::Center);
    bind(
        current().map(|t| t.is_some()),
        &theme_switch,
        gtk::prelude::WidgetExt::set_sensitive,
    );
    bind_two_way(
        current(),
        &theme_switch,
        |sw, theme| {
            if let Some(theme) = theme {
                sw.set_active(matches!(theme, Theme::Dark));
            }
        },
        |w| {
            w.connect_active_notify(|sw| {
                theme::set(if sw.is_active() {
                    Theme::Dark
                } else {
                    Theme::Light
                });
            })
        },
    );
    theme_switch
}

pub fn panel_settings() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    // ── Appearance ────────────────────────────────────────────────────────
    let appearance = adw::PreferencesGroup::builder().title("Appearance").build();

    let theme_row = adw::ActionRow::builder().title("Dark mode").build();

    let theme_switch = build_theme_switch(theme::current_signal);
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

    column.append(&build_more_group());

    finish_page(&column)
}

/// The "More" group: deep-link rows to drawer pages that don't have a
/// dedicated bar chip. Each row swaps the currently-open drawer to the target
/// page via `modal::switch_active` (see modal.rs) so the user stays on the same
/// monitor's drawer surface; no `&Monitor` is plumbed through here.
///
/// A function rather than a block inside [`panel_settings`] because #1071's
/// fourth row pushed that function past `clippy::too_many_lines`, and this is
/// the one section of the page that is a plain list with no bindings.
fn build_more_group() -> adw::PreferencesGroup {
    let more = adw::PreferencesGroup::builder().title("More").build();

    // The three rows that were here before #1071 keep their positions: this
    // group is a list users navigate by muscle memory, and a new entry is not a
    // reason to move what they already know (#1096 review, LOW-3). Workspaces is
    // appended.
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
    // #1071's entry point. The bar keeps its numbered `[1 2 3 …]` switcher
    // (revision 5 of the epic, Annika 2026-09-10), so the Workspaces page gets
    // no chip of its own — this row and the `open-page workspaces` GAction
    // (`commands.rs`, wired for free off `Page::stack_name`) are how it opens.
    more.add(&deep_link_row(
        "Workspaces",
        Some("Saved workspace stacks per screen"),
        "view-grid-symbolic",
        crate::modal::Page::Workspaces,
    ));
    // #1304's entry point, appended last: Mara's placement question on the
    // issue ("bottom of the group, or the first row so it's one click from
    // the chip?") is still open, and the bottom is this group's own default
    // for a new row (#1096 review, LOW-3, quoted above) — unlike the four
    // rows above it, this one leaves the drawer entirely rather than
    // switching to another page in it, which `control_center_row`'s
    // `CONTROL_CENTER_TRAILING_ICON` (in place of `deep_link_row`'s
    // `go-next-symbolic`) is meant to say before it's clicked.
    more.add(&control_center_row());

    more
}

/// The trailing icon on the Control Center row, said to mean "activating this
/// leaves the drawer for a separate window" — a named constant, not a string
/// literal at the call site, because the icon that reads that way and the
/// icon that actually exists in the Adwaita theme this ships are not the same
/// name: `external-link-symbolic` (the first pick) does not exist in
/// `adwaita-icon-theme`/`libadwaita`/`gtk4` and rendered as `image-missing`
/// (#1305 review MED-1, caught by `tests::the_control_center_row_is_insensitive_with_a_subtitle_only_when_the_route_is_missing`'s
/// `has_icon` assertion below). `window-new-symbolic` is the stock "opens in
/// a separate window" glyph and does exist.
const CONTROL_CENTER_TRAILING_ICON: &str = "window-new-symbolic";

/// The "Control Center" row (#1304): starts, or focuses, the
/// `trollshell-control-center` companion app. Resolves its launch [`Route`]
/// when the row is built — `modal::ensure_page` caches this whole page for
/// the life of the shell (this file's module doc), so a row built while the
/// binary was missing stays insensitive until the shell restarts, **even
/// though a fresh install needs no restart on the resolver's own side**: the
/// `open-control-center` `GAction` (`commands.rs`) re-resolves on every
/// activation, so the keybind starts working immediately while this cached
/// row does not — see `companion`'s module doc for that asymmetry stated in
/// full (correcting an earlier, false claim here that neither route could
/// appear or disappear without a restart at all).
fn control_center_row() -> adw::ActionRow {
    build_control_center_row(companion::resolve())
}

/// [`control_center_row`]'s testable half: everything after the route is
/// already known, over an injected [`Route`] — the seam
/// [`tests::the_control_center_row_is_insensitive_with_a_subtitle_only_when_the_route_is_missing`]
/// uses. Delegates to [`build_control_center_row_with`] with the real
/// [`crate::modal::dismiss_all`] as the "close the drawer" side effect;
/// production never calls the `_with` form directly.
fn build_control_center_row(route: Route) -> adw::ActionRow {
    build_control_center_row_with(route, crate::modal::dismiss_all)
}

/// [`build_control_center_row`] with the drawer-dismiss side effect also
/// injected, so [`tests::activating_the_row_dismisses_the_drawer`] can observe
/// it fired without a live `modal` panel (`modal`'s panel set is a private
/// thread-local `gtk::Window` registry that a bare test thread never
/// populates — see `tests/modal_deeplink.rs`'s module doc for the same
/// constraint on `modal::switch_active`).
fn build_control_center_row_with(route: Route, dismiss: impl Fn() + 'static) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("Control Center")
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name("preferences-system-symbolic"));
    // Every other row in this group swaps the drawer to another page; this
    // one leaves it for a separate window entirely, hence a different
    // trailing icon than `deep_link_row`'s `go-next-symbolic`.
    row.add_suffix(&gtk::Image::from_icon_name(CONTROL_CENTER_TRAILING_ICON));

    if matches!(route, Route::Missing) {
        row.set_sensitive(false);
        row.set_subtitle("trollshell-control-center is not installed");
        return row;
    }

    row.connect_activated(move |_| {
        companion::launch(&route);
        // The row's action leaves the drawer for a separate window, so there
        // is nothing left in it to switch to — dismiss rather than
        // `modal::switch_active`, the same call the power-menu rows and a
        // clipboard entry's own activation make to close the drawer after an
        // action that isn't a navigation to another page in it.
        dismiss();
    });
    row
}

#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::build_theme_switch;
    use hytte::adw::{self, prelude::*};
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk;
    use hytte::services::theme::Theme;

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// The UI half of #1192's HIGH-1. The service half
    /// (`hytte_services::theme`'s `a_light_session_is_never_observed_as_dark`)
    /// proves the value arrives correctly; this proves the switch renders it
    /// correctly, including the state that used to be spelled as a confident
    /// `Theme::Dark`: unknown.
    ///
    /// **Falsification:** drop the `sensitive` binding and the first
    /// assertion fails; make the two-way apply run on `None` (e.g.
    /// `sw.set_active(matches!(theme, Some(Theme::Dark)))`) and a light
    /// session is indistinguishable from an unread one, which is the whole
    /// bug.
    #[gtk::test]
    fn the_theme_switch_is_insensitive_until_the_seed_lands() {
        adw::init().expect("libadwaita init");
        let handle: Mutable<Option<Theme>> = Mutable::new(None);
        let switch = build_theme_switch({
            let handle = handle.clone();
            move || handle.signal()
        });
        pump();

        assert!(
            !switch.is_sensitive(),
            "while the theme is unknown the switch must not be flippable — a \
             flip would write a theme off a position nobody chose",
        );

        handle.set(Some(Theme::Light));
        pump();
        assert!(
            switch.is_sensitive(),
            "the switch goes live once the seed lands",
        );
        assert!(
            !switch.is_active(),
            "a light session must show Dark mode OFF — this is the exact \
             reading that was wrong, permanently, before #1192's review",
        );

        handle.set(Some(Theme::Dark));
        pump();
        assert!(switch.is_active(), "a dark session shows Dark mode ON");
        assert!(switch.is_sensitive());
    }

    /// #1304: the Control Center row is honest about a missing binary
    /// (insensitive, subtitle naming it) and otherwise a plain, unadorned
    /// clickable row — the same "before the click, not after" rule
    /// `companion::resolve`'s module doc states for why the route can't be
    /// decided at launch time.
    ///
    /// Also pins #1305 review MED-1: the row's trailing icon must actually
    /// exist in the Adwaita theme this ships with — `set_gtk_icon_theme_name`
    /// forces the same theme `main.rs` forces for a real session, since
    /// nothing else on a bare test thread would (CLAUDE.md's `image-missing`
    /// gotcha). `external-link-symbolic`, the first pick, does not exist in
    /// `adwaita-icon-theme`/`libadwaita`/`gtk4` and rendered as
    /// `image-missing`; `window-new-symbolic` does.
    ///
    /// **Falsification:** drop the `row.set_sensitive(false)` call in
    /// `build_control_center_row`'s `Route::Missing` arm → this reds (the
    /// row stays sensitive while still showing the "not installed"
    /// subtitle, a broken affordance rather than an honest one). Revert
    /// `CONTROL_CENTER_TRAILING_ICON` to `"external-link-symbolic"` → the
    /// `has_icon` assertion reds.
    #[gtk::test]
    fn the_control_center_row_is_insensitive_with_a_subtitle_only_when_the_route_is_missing() {
        adw::init().expect("libadwaita init");

        if let Some(settings) = gtk::Settings::default() {
            settings.set_gtk_icon_theme_name(Some("Adwaita"));
        }
        let display = gtk::gdk::Display::default().expect("a display for a #[gtk::test]");
        assert!(
            gtk::IconTheme::for_display(&display).has_icon(super::CONTROL_CENTER_TRAILING_ICON),
            "{:?} must exist in the Adwaita theme this ships with, or the row's \
             \"this opens a separate window\" affordance renders as image-missing",
            super::CONTROL_CENTER_TRAILING_ICON
        );

        let missing = super::build_control_center_row(super::Route::Missing);
        assert!(
            !missing.is_sensitive(),
            "a missing control center must not be clickable"
        );
        assert_eq!(
            missing.subtitle().as_deref(),
            Some("trollshell-control-center is not installed"),
            "the row must name the missing binary rather than a silent no-op"
        );

        let binary = super::build_control_center_row(super::Route::Binary(
            std::path::PathBuf::from("/usr/bin/trollshell-control-center"),
        ));
        assert!(
            binary.is_sensitive(),
            "a resolved binary route must be clickable"
        );
        assert!(
            binary.subtitle().as_deref().is_none_or(str::is_empty),
            "a working route must carry no subtitle, got {:?}",
            binary.subtitle()
        );
    }

    /// #1305 review LOW: the triage's item 1 is "launches the companion
    /// **and closes the drawer**" — pinned here over
    /// `build_control_center_row_with`'s injected dismiss callback rather
    /// than a live `modal` panel, which a bare test thread never populates
    /// (see that function's doc). The row's `"activated"` signal is emitted
    /// directly (`emit_by_name`) — `AdwActionRow` defines it on the row
    /// itself, so it fires the same way whether or not the row is packed
    /// into a real `GtkListBox`.
    ///
    /// **Falsification:** delete the `dismiss();` call in
    /// `build_control_center_row_with`'s `connect_activated` closure → this
    /// reds, `dismissed` never becomes `true`.
    #[gtk::test]
    fn activating_the_row_dismisses_the_drawer() {
        adw::init().expect("libadwaita init");

        let dismissed = std::rc::Rc::new(std::cell::Cell::new(false));
        let dismissed_in = dismissed.clone();
        let row = super::build_control_center_row_with(
            super::Route::Binary(std::path::PathBuf::from(
                "/usr/bin/trollshell-control-center",
            )),
            move || dismissed_in.set(true),
        );

        row.emit_by_name::<()>("activated", &[]);

        assert!(
            dismissed.get(),
            "activating the row must close the drawer (dismiss was never called)"
        );
    }
}
