//! Appearance / wallpaper drawer panel.
//!
//! The wallpaper picker spans three dimensions (#546), all persisted by the
//! `wallpaper` service to `~/.config/trollshell/wallpaper.json`:
//!
//! - **All displays** — the default image applied to every output.
//! - **Per-display overrides** — one image per connected output (reactively
//!   listed from niri's output topology), falling back to the default.
//! - **Time of day** — a rotation toggle plus a morning/day/evening/night
//!   image; when on it drives all displays on a fixed schedule.
//! - **Clear wallpaper** — an explicit reset back to no background. Disabled
//!   under a custom reload backend (`reloadCommand` / awww), which is
//!   single-image and can't be told "no wallpaper" — the button would be a
//!   silent no-op there (see [`wallpaper::has_custom_reload_backend`]).
//!
//! The user picks a file with `gtk::FileDialog`; the service rewrites its state
//! file, re-derives the swaybg arguments, and restarts (or, on clear, stops)
//! the swaybg unit so the change takes effect immediately.
//!
//! Also home to the **Night light** toggle (color temperature) — an appearance
//! concern that flips the zero-state `wlsunset` user unit via the `nightlight`
//! service. Config (lat/lon + day/night temps) lives in the nix module. With no
//! configured coordinates the toggle resolves them from a live location fix,
//! which can take seconds, so the row renders the service's tri-state
//! (`nightlight::state()`) rather than a bool: see [`build_display_group`].

use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self, gio};
use hytte::prelude::*;
use hytte::services::displays::{self, Output};
use hytte::services::nightlight;
use hytte::services::wallpaper::{self, Slot};

use crate::components::layout::{finish_page, page_box};
use crate::components::reactive_list::reactive_list;

pub fn panel_appearance() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    column.append(&build_wallpaper_group());
    column.append(&build_per_display_group());
    column.append(&build_rotation_group());
    column.append(&build_display_group());
    finish_page(&column)
}

/// The "Wallpaper" group: the all-displays default plus the explicit
/// "Clear wallpaper" reset. Both rows are static, so their order is stable.
fn build_wallpaper_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Wallpaper").build();

    let default_row = image_row(
        "All displays",
        wallpaper::default_path,
        |cur| cur.map_or_else(|| "Not set".to_string(), |p| wallpaper_basename(&p)),
        Rc::new(|path| wallpaper::set_default(&path)),
        Some(Rc::new(wallpaper::clear_default)),
    );
    group.add(&default_row);

    // Explicit reset to no wallpaper (#546) — clears the default, every
    // per-output override, and rotation in one go, then stops the swaybg unit.
    let clear_row = adw::ActionRow::builder()
        .title("Clear wallpaper")
        .subtitle("Remove the background from every display")
        .build();
    clear_row.set_subtitle_lines(0);
    let clear = gtk::Button::with_label("Clear");
    clear.set_valign(gtk::Align::Center);
    clear.add_css_class("flat");
    clear.add_css_class("destructive-action");
    if wallpaper::has_custom_reload_backend() {
        // A custom reload backend (reloadCommand / awww) is single-image and
        // driven only by "here's the new image" — it can't be told "no
        // wallpaper", so a clear would be a silent no-op (the daemon keeps
        // painting the last image). Disable the button rather than pretend it
        // works. The tooltip goes on the row: an insensitive button eats no
        // pointer events, so its own tooltip would never show.
        clear.set_sensitive(false);
        clear_row.set_tooltip_text(Some(
            "Clearing isn't available with a custom wallpaper backend \u{2014} \
             remove the wallpaper from your daemon instead",
        ));
        clear_row.add_suffix(&clear);
    } else {
        clear.connect_clicked(|_| wallpaper::clear());
        clear_row.add_suffix(&clear);
        clear_row.set_activatable_widget(Some(&clear));
    }
    group.add(&clear_row);

    group
}

/// The "Per-display" group: one override row per connected output, rebuilt on
/// niri topology change. Each row falls back to the all-displays default.
fn build_per_display_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Per-display")
        .description("Override the wallpaper on individual monitors")
        .build();

    reactive_list(
        &group,
        displays::outputs(),
        build_output_row,
        Some(|| {
            adw::ActionRow::builder()
                .title("No displays detected")
                .subtitle("Waiting for niri\u{2026}")
                .activatable(false)
                .build()
        }),
    );

    group
}

fn build_output_row(o: &Output) -> adw::ActionRow {
    let name = o.name.clone();
    let title = display_title(o);

    let name_for_sig = name.clone();
    let name_for_pick = name.clone();
    let name_for_clear = name.clone();
    let row = image_row(
        &title,
        move || {
            let n = name_for_sig.clone();
            wallpaper::state().map(move |s| s.outputs.get(&n).cloned())
        },
        |cur| cur.map_or_else(|| "Using default".to_string(), |p| wallpaper_basename(&p)),
        Rc::new(move |path| wallpaper::set_output(&name_for_pick, &path)),
        Some(Rc::new(move || wallpaper::clear_output(&name_for_clear))),
    );

    // Connector chip, matching the Displays panel's look.
    let prefix = gtk::Label::new(Some(&name));
    prefix.add_css_class("ts-display-connector");
    prefix.add_css_class("monospace");
    prefix.set_valign(gtk::Align::Center);
    row.add_prefix(&prefix);

    row
}

/// The "Time of day" group: a rotation toggle plus one image per slot. The slot
/// rows dim while rotation is off.
fn build_rotation_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Time of day")
        .build();

    let toggle = adw::SwitchRow::builder()
        .title("Rotate by time of day")
        .subtitle("Switch the wallpaper on a morning / day / evening / night schedule")
        .build();
    bind_two_way(
        wallpaper::state().map(|s| s.rotation.enabled),
        &toggle,
        adw::SwitchRow::set_active,
        |r| r.connect_active_notify(|r| wallpaper::set_rotation_enabled(r.is_active())),
    );
    group.add(&toggle);

    for slot in Slot::ALL {
        let range = slot.range_label();
        let row = image_row(
            slot.label(),
            move || wallpaper::state().map(move |s| s.rotation.image(slot).map(str::to_string)),
            move |cur| match cur {
                Some(p) => format!("{} \u{00b7} {range}", wallpaper_basename(&p)),
                None => range.to_string(),
            },
            Rc::new(move |path| wallpaper::set_slot_image(slot, &path)),
            Some(Rc::new(move || wallpaper::clear_slot(slot))),
        );
        // Dim the slot rows while rotation is off — they don't apply then.
        bind(
            wallpaper::state().map(|s| s.rotation.enabled),
            &row,
            adw::ActionRow::set_sensitive,
        );
        group.add(&row);
    }

    group
}

/// Build an `ActionRow` whose subtitle reflects a signal of the currently-set
/// image, with a "Browse…" suffix (opens the picker, hands the path to
/// `on_pick`) and an optional clear button (sensitive only while an image is
/// set).
///
/// `make_signal` is a *factory* — it's called once per binding so the subtitle
/// and the clear button's sensitivity each get an independent subscription.
fn image_row<S>(
    title: &str,
    make_signal: impl Fn() -> S + 'static,
    subtitle: impl Fn(Option<String>) -> String + 'static,
    on_pick: Rc<dyn Fn(String)>,
    on_clear: Option<Rc<dyn Fn()>>,
) -> adw::ActionRow
where
    S: hytte::futures_signals::signal::Signal<Item = Option<String>> + 'static,
{
    let row = adw::ActionRow::builder().title(title).build();
    // Long file paths shouldn't push the modal wide; let the subtitle wrap.
    row.set_subtitle_lines(0);

    bind(make_signal(), &row, move |row, cur| {
        row.set_subtitle(&subtitle(cur));
    });

    if let Some(clear) = on_clear {
        let btn = gtk::Button::from_icon_name("edit-clear-symbolic");
        btn.set_valign(gtk::Align::Center);
        btn.add_css_class("flat");
        btn.set_tooltip_text(Some("Clear"));
        btn.connect_clicked(move |_| clear());
        // Nothing to clear when the image is unset.
        bind(
            make_signal().map(|c| c.is_some()),
            &btn,
            gtk::Button::set_sensitive,
        );
        row.add_suffix(&btn);
    }

    let browse = gtk::Button::with_label("Browse\u{2026}");
    browse.set_valign(gtk::Align::Center);
    browse.add_css_class("flat");
    browse.connect_clicked(move |_| {
        let on_pick = on_pick.clone();
        open_wallpaper_picker(move |path| on_pick(path));
    });
    row.add_suffix(&browse);
    row.set_activatable_widget(Some(&browse));

    row
}

/// The Night light row's resting subtitle — what the toggle is *for*.
const NIGHT_LIGHT_SUBTITLE: &str = "Warm the screen's color temperature after sunset";

/// Subtitle while a toggle-on is parked on a location fix
/// (`NightlightState::Resolving`). Zero-config night light resolves coordinates
/// from `GeoClue` at toggle time, and a cold fix can take several seconds during
/// which nothing on screen changes — long enough that "flip it on, see nothing,
/// flip it back off" is the reasonable reaction rather than an unusual one
/// (#597). Naming the thing being waited for is the point: "it's slow" is not
/// actionable, "it needs your location" is.
const NIGHT_LIGHT_RESOLVING_SUBTITLE: &str = "Waiting for a location fix\u{2026}";

/// "Display" preferences group holding the Night light toggle. The switch's
/// `active` is driven by the daemon's authoritative state
/// (`nightlight::state()`), NOT local widget state — so any monitor's drawer
/// reflects the same toggle and a drawer rebuild never loses track. Flipping it
/// starts/stops the `wlsunset` user unit.
///
/// The state is a tri-state, and the row spends all three of it:
///
/// - the switch shows `is_on()`, which folds `Resolving` into **on** — the user
///   just put it there and pulling it back would be the "switch moves by itself"
///   bug (#594) in reverse;
/// - the spinner and subtitle show `is_resolving()`, which is the part that
///   makes the wait legible at all;
/// - the switch deliberately stays **sensitive** throughout. Greying it out
///   during the wait would swap a ten-second silent stall for a ten-second
///   locked one, and it would take away the escape hatch #595 exists to make
///   work: a toggle-off during `Resolving` supersedes the parked start.
///
/// Built as `ActionRow` + explicit `gtk::Switch` rather than `adw::SwitchRow`
/// for one reason: `AdwSwitchRow` adds its own switch as the first suffix, so
/// anything added afterwards lands to its *right*. The spinner belongs between
/// the title and the control (the `panels::bluetooth` scan-row shape), which
/// needs the suffixes added in that order.
fn build_display_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Display").build();

    let row = adw::ActionRow::builder()
        .title("Night light")
        .subtitle(NIGHT_LIGHT_SUBTITLE)
        .build();

    let spinner = gtk::Spinner::new();
    spinner.set_valign(gtk::Align::Center);
    // The bind below sets this on its first poll; starting hidden avoids a
    // frame of stopped spinner between construction and that poll.
    spinner.set_visible(false);
    row.add_suffix(&spinner);

    let switch = gtk::Switch::new();
    switch.set_valign(gtk::Align::Center);
    // Two-way: the authoritative signal drives `active` (the block prevents the
    // programmatic set_active from re-entering the handler); a user flip calls
    // set_enabled, which toggles the user unit off the GTK thread. The explicit
    // `is_on()` is the tri-state → bool mapping — see the fn docs.
    bind_two_way(
        nightlight::state(),
        &switch,
        |w, state| w.set_active(state.is_on()),
        |w| w.connect_active_notify(|w| nightlight::set_enabled(w.is_active())),
    );
    row.add_suffix(&switch);
    row.set_activatable_widget(Some(&switch));

    // One subscription drives both pending affordances, so the spinner and the
    // subtitle can never disagree about whether a fix is outstanding. The
    // closure holds the spinner strongly, but only the row holds the closure's
    // task alive, and the row owns the spinner — so teardown frees both.
    bind(nightlight::state(), &row, move |row, state| {
        let resolving = state.is_resolving();
        row.set_subtitle(if resolving {
            NIGHT_LIGHT_RESOLVING_SUBTITLE
        } else {
            NIGHT_LIGHT_SUBTITLE
        });
        spinner.set_spinning(resolving);
        spinner.set_visible(resolving);
    });

    group.add(&row);
    group
}

/// Title for a per-display row: make + model when EDID is informative, else the
/// bare connector name. Mirrors the Displays panel.
fn display_title(o: &Output) -> String {
    let trimmed = format!("{} {}", o.make.trim(), o.model.trim());
    if trimmed.trim().is_empty() {
        o.name.clone()
    } else {
        trimmed.trim().to_string()
    }
}

/// Last path component of a wallpaper path, with the original returned if
/// it has no separator (e.g. relative or already-bare filename).
fn wallpaper_basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |s| s.to_string_lossy().into_owned())
}

/// Open a `gtk::FileDialog` to pick a wallpaper image. On selection, hands the
/// absolute path to `on_pick`. Cancellation / error is logged at debug level.
fn open_wallpaper_picker(on_pick: impl Fn(String) + 'static) {
    let dialog = gtk::FileDialog::builder()
        .title("Select wallpaper")
        .modal(true)
        .build();

    // Filter to common still-image formats. swaybg renders PNG/JPEG and
    // (with the right build) GIF/PNM/etc.; the broad filter keeps us out
    // of the business of guessing exactly what swaybg supports today.
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Images"));
    for mime in ["image/png", "image/jpeg", "image/webp", "image/bmp"] {
        filter.add_mime_type(mime);
    }
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    dialog.set_filters(Some(&filters));
    dialog.set_default_filter(Some(&filter));

    // Deliberately unparented: our drawer is a gtk4-layer-shell surface, not
    // an xdg-toplevel, so there is no valid handle-export path for the
    // xdg-desktop-portal file chooser (or GTK's fallback) to anchor to it.
    // On some GTK/gdk-wayland builds that export aborts the whole process
    // instead of degrading gracefully — the shell-crashing bug in #379. A
    // slightly less-anchored dialog is a much better failure mode than
    // taking down the shell.
    dialog.open(
        None::<&gtk::Window>,
        gio::Cancellable::NONE,
        move |result| {
            match result {
                Ok(file) => {
                    if let Some(path) = file.path() {
                        on_pick(path.to_string_lossy().into_owned());
                    } else {
                        tracing::warn!("wallpaper picker: selection had no local path");
                    }
                }
                Err(e) => {
                    // gtk's "Dismissed by user" comes back as an error too —
                    // debug, not warn.
                    tracing::debug!(error = %e, "wallpaper picker: dismissed");
                }
            }
        },
    );
}
