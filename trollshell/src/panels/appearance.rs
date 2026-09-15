//! Appearance / wallpaper drawer panel.
//!
//! The wallpaper picker spans three dimensions (#546), all persisted by the
//! `wallpaper` service to `$XDG_STATE_HOME/trollshell/wallpaper.toml` (#1226;
//! migrated once out of the legacy `~/.config/trollshell/wallpaper.json`):
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
//! Since #1308, a **Scaling** row beside the "All displays" picker sets
//! swaybg's `-m` mode (Fill / Fit / Center / Tile / Stretch) — one mode for
//! every screen, insensitive with nothing configured to scale (which
//! `default`/`outputs`/`rotation` can each make true — see
//! [`anything_configured_to_scale`]) or under a custom reload backend, same
//! as the Clear button above. See [`scaling_row`].
//!
//! The user picks a file with `gtk::FileDialog`; the service rewrites its state
//! file, re-derives the swaybg arguments, and restarts (or, on clear, stops)
//! the swaybg unit so the change takes effect immediately.
//!
//! Also home to the **Night light** toggle (color temperature) — an appearance
//! concern that flips the zero-state `wlsunset` user unit via the `nightlight`
//! service. Config (lat/lon + day/night temps) lives in the nix module. With no
//! configured coordinates the toggle resolves them from a live location fix,
//! which can take seconds, so the row renders the service's `Pending<bool>`
//! (`nightlight::state()`) rather than a bare bool: see
//! [`build_display_group`].

use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self, gio};
use hytte::prelude::*;
use hytte::services::displays::{self, Output};
use hytte::services::nightlight;
use hytte::services::wallpaper::{self, Mode, Slot};

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

/// Whether swaybg is (or would be) painting anything at all: a default
/// image, a per-output override, or a rotation with *any* slot configured
/// (#1314 review MED-1).
///
/// Any slot, not the one active this hour — `build_rotation_group` writes
/// only `rotation.*` for a rotation-only setup (no `default`, no `outputs`),
/// and `wallpaper::swaybg_args` renders whichever slot resolves for the
/// current hour, so gating on "the render is non-empty right now" would make
/// the Scaling row flicker sensitive/insensitive with the clock the moment a
/// rotation with, say, only `evening` set crossed into a different slot.
fn anything_configured_to_scale(s: &wallpaper::WallpaperState) -> bool {
    s.default.is_some()
        || !s.outputs.is_empty()
        || (s.rotation.enabled
            && Slot::ALL
                .iter()
                .any(|slot| s.rotation.image(*slot).is_some()))
}

/// The Scaling row's sensitivity: usable exactly when swaybg is actually
/// painting something (`anything_configured_to_scale`) *and* no custom
/// reload backend has taken over (#1314 review MED-1/MED-2) — a `reload()`
/// with `TROLLSHELL_WALLPAPER_RELOAD_CMD` set never touches the swaybg unit
/// `swaybg.args` is written for, so a pick there would write state and
/// change nothing anyone reads. `custom_backend` is a plain `bool` — read
/// once via `wallpaper::has_custom_reload_backend()` at the call site,
/// since the env var is fixed for the session — rather than this function
/// reading the environment itself, so it stays a pure, directly testable
/// predicate over the same two facts the Wallpaper group's Clear button
/// already gates on.
fn scaling_is_usable(custom_backend: bool, s: &wallpaper::WallpaperState) -> bool {
    !custom_backend && anything_configured_to_scale(s)
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

    // Scaling (#1308): one mode for every screen (Annika's per-output
    // question is open on the issue), so it sits beside the picker rather
    // than under it. Sensitive only when swaybg is actually painting
    // something (#1314 review MED-1/MED-2): a rotation-only setup (no
    // `default`, no `outputs`) still renders, and a custom reload backend
    // never reads `swaybg.args` at all, so the row would be a live control
    // over nothing in either case.
    let custom_backend = wallpaper::has_custom_reload_backend();
    let scaling = scaling_row(
        wallpaper::mode(),
        wallpaper::state().map(move |s| scaling_is_usable(custom_backend, &s)),
        wallpaper::set_mode,
    );
    if custom_backend {
        // Same reasoning and the same wording shape as the Clear button's
        // tooltip below: a custom backend only ever receives the primary
        // image path, so a Scaling pick would write state, re-derive
        // `swaybg.args`, and change nothing anyone reads.
        scaling.set_tooltip_text(Some(
            "Scaling isn't available with a custom wallpaper backend \u{2014} \
             it only receives the image path",
        ));
    }
    group.add(&scaling);

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

/// The "Scaling" row (#1308): an `adw::ComboRow` over swaybg's five *scaling*
/// modes (`Mode::SCALING` — `solid_color` paints a color rather than scaling
/// an image, so it stays out of the menu). One mode for every screen: the
/// picker above is per-output, but scaling isn't asked to be (Annika's
/// per-output question is open on the issue).
///
/// `mode_signal` drives the selected item; `on_change` fires with the mode the
/// user just picked — `wallpaper::set_mode` in production, injected so a test
/// can capture it instead of writing the real state; `sensitive_signal` gates
/// the row (`anything_configured_to_scale` and the custom-backend check in
/// production — see `build_wallpaper_group`), the same way the Wallpaper
/// group's own Clear button is gated.
///
/// When the file holds a mode outside `Mode::SCALING` (today only
/// `solid_color`), the row appends a trailing item naming it rather than
/// silently showing "Fill" at index 0 — #1314 review LOW-1: `AdwComboRow`
/// only fires `notify::selected` on an actual index change, so if the row
/// merely *displayed* Fill while the state held something else, picking Fill
/// would select the already-selected index 0 and write nothing, leaving no
/// way out of the unlisted mode.
fn scaling_row(
    mode_signal: impl hytte::futures_signals::signal::Signal<Item = Mode> + 'static,
    sensitive_signal: impl hytte::futures_signals::signal::Signal<Item = bool> + 'static,
    on_change: impl Fn(Mode) + 'static,
) -> adw::ComboRow {
    let labels: Vec<&str> = Mode::SCALING.iter().map(|m| m.label()).collect();
    let base_len = u32::try_from(Mode::SCALING.len()).unwrap_or(0);
    let model = gtk::StringList::new(&labels);
    let row = adw::ComboRow::builder()
        .title("Scaling")
        .subtitle("How the image fills each display")
        .model(&model)
        .build();

    bind_two_way(
        mode_signal,
        &row,
        {
            let model = model.clone();
            move |row: &adw::ComboRow, mode| {
                let extra = model.n_items().saturating_sub(base_len);
                if let Some(idx) = Mode::SCALING.iter().position(|m| *m == mode) {
                    if extra > 0 {
                        model.splice(base_len, extra, &[]);
                    }
                    row.set_selected(u32::try_from(idx).unwrap_or(0));
                } else {
                    let placeholder = format!("{} (from file)", mode.label());
                    model.splice(base_len, extra, &[placeholder.as_str()]);
                    row.set_selected(base_len);
                }
            }
        },
        move |row| {
            row.connect_selected_notify(move |row| {
                let idx = usize::try_from(row.selected()).unwrap_or(0);
                // A selection at or past `Mode::SCALING`'s length is the
                // trailing placeholder above — it names the file's current
                // out-of-vocabulary value and isn't itself pickable, so only
                // a real entry writes.
                if let Some(mode) = Mode::SCALING.get(idx).copied() {
                    on_change(mode);
                }
            })
        },
    );

    bind(sensitive_signal, &row, adw::ComboRow::set_sensitive);

    row
}

/// The Night light row's resting subtitle — what the toggle is *for*.
const NIGHT_LIGHT_SUBTITLE: &str = "Warm the screen's color temperature after sunset";

/// Subtitle while a toggle-on is parked on a location fix (the service's state
/// is `Pending` there). Zero-config night light resolves coordinates
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
/// The state is a `Pending<bool>` (#599's shared model), and the row spends both
/// halves of it:
///
/// - the switch shows `displayed()`, which prefers the in-flight intent over the
///   daemon's reading — the user just put the switch there and pulling it back
///   would be the "switch moves by itself" bug (#594) in reverse;
/// - the spinner and subtitle show `is_pending()`, which is the part that makes
///   the wait legible at all;
/// - the switch deliberately stays **sensitive** throughout. Greying it out
///   during the wait would swap a ten-second silent stall for a ten-second
///   locked one, and it would take away the escape hatch #595 exists to make
///   work: a toggle-off during the wait supersedes the parked start.
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
    // `displayed()` is the `Pending<bool>` → bool mapping — see the fn docs.
    bind_two_way(
        nightlight::state(),
        &switch,
        |w, state| w.set_active(*state.displayed()),
        |w| w.connect_active_notify(|w| nightlight::set_enabled(w.is_active())),
    );
    row.add_suffix(&switch);
    row.set_activatable_widget(Some(&switch));

    // One subscription drives both pending affordances, so the spinner and the
    // subtitle can never disagree about whether a fix is outstanding. The
    // closure holds the spinner strongly, but only the row holds the closure's
    // task alive, and the row owns the spinner — so teardown frees both.
    bind(nightlight::state(), &row, move |row, state| {
        let resolving = state.is_pending();
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

/// #1308: `scaling_row` takes its signals and its write path as parameters —
/// the same seam `image_row`'s `on_pick`/`on_clear` already use — precisely so
/// it is testable against local `Mutable`s rather than the registered
/// `wallpaper` service, and never touches the real `$XDG_STATE_HOME`.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::{Mode, anything_configured_to_scale, scaling_is_usable, scaling_row};
    use hytte::adw::{self, prelude::*};
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk;
    use hytte::services::wallpaper::{Rotation, WallpaperState};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    // ── the sensitivity predicates (pure — no GTK needed) ──────────────────

    /// #1314 review MED-1: a rotation-only setup (no `default`, no
    /// `outputs`) still renders — `wallpaper::swaybg_args`'s own rotation
    /// branch resolves the active slot's image — so the row must be
    /// sensitive, and gating on `default`/`outputs` alone (the pre-fix
    /// predicate) missed it entirely.
    ///
    /// **Falsification:** go back to `s.default.is_some() ||
    /// !s.outputs.is_empty()` — `rotation_only` reds.
    #[test]
    fn rotation_only_setup_is_seen_as_configured() {
        let truly_empty = WallpaperState::default();
        assert!(
            !anything_configured_to_scale(&truly_empty),
            "nothing set anywhere \u{21d2} nothing to scale"
        );

        let rotation_only = WallpaperState {
            rotation: Rotation {
                enabled: true,
                evening: Some("/e.png".into()),
                ..Rotation::default()
            },
            ..WallpaperState::default()
        };
        assert!(
            anything_configured_to_scale(&rotation_only),
            "a rotation-only setup with no default and no per-output override \
             still paints a wallpaper, so the row must be sensitive"
        );
    }

    /// Rotation *enabled* with every slot unset renders nothing
    /// (`wallpaper::swaybg_args`'s own "on with nothing configured" case) —
    /// gating on the bare `enabled` flag rather than "any slot has an image"
    /// would wrongly light the row up for a session that just turned
    /// rotation on and hasn't picked any slot image yet.
    #[test]
    fn rotation_enabled_with_no_slot_images_is_still_nothing_to_scale() {
        let state = WallpaperState {
            rotation: Rotation {
                enabled: true,
                ..Rotation::default()
            },
            ..WallpaperState::default()
        };
        assert!(!anything_configured_to_scale(&state));
    }

    /// #1314 review MED-2: a custom reload backend never touches the swaybg
    /// unit `swaybg.args` is written for, so the row must be insensitive
    /// under one regardless of what the state otherwise holds — even a
    /// fully-configured default.
    ///
    /// **Falsification:** drop the `!custom_backend &&` half —
    /// `custom_backend_wins_even_with_a_default_set` reds.
    #[test]
    fn custom_backend_wins_even_with_a_default_set() {
        let state = WallpaperState {
            default: Some("/d.png".into()),
            ..WallpaperState::default()
        };
        assert!(
            scaling_is_usable(false, &state),
            "no custom backend, configured \u{21d2} usable"
        );
        assert!(
            !scaling_is_usable(true, &state),
            "a custom backend never reads swaybg.args, so it wins over an \
             otherwise-configured state"
        );
    }

    /// The row's selected item follows the mode signal, at construction and
    /// after a later change — the way every other row in this panel follows
    /// `wallpaper::state()`.
    ///
    /// **Falsification:** hardcode the apply arm's `row.set_selected(0)`
    /// instead of resolving `mode`'s position in `Mode::SCALING` — the row
    /// opens on Fill regardless of the signal and never moves on the second
    /// assertion.
    #[gtk::test]
    fn the_row_follows_the_mode_signal() {
        adw::init().expect("libadwaita init");
        let handle: Mutable<Mode> = Mutable::new(Mode::Tile);
        let row = scaling_row(handle.signal(), Mutable::new(true).signal(), |_| {});
        pump();

        assert_eq!(
            usize::try_from(row.selected()).expect("a selection"),
            Mode::SCALING
                .iter()
                .position(|m| *m == Mode::Tile)
                .expect("Tile is offered"),
            "the row must open on the signal's own value, not always Fill"
        );

        handle.set(Mode::Center);
        pump();
        assert_eq!(
            usize::try_from(row.selected()).expect("a selection"),
            Mode::SCALING
                .iter()
                .position(|m| *m == Mode::Center)
                .expect("Center is offered"),
            "a later signal change must move the row too"
        );
    }

    /// Selecting an item writes the mode through the injected seam — never
    /// `wallpaper::set_mode` directly, and so never the real
    /// `$XDG_STATE_HOME` (CLAUDE.md's tests-must-not-touch-real-XDG rule).
    ///
    /// **Falsification:** wire `connect_selected_notify` to a no-op (or to
    /// the real `wallpaper::set_mode`, which this test cannot observe) — the
    /// captured `Vec` stays empty.
    #[gtk::test]
    fn selecting_an_item_writes_the_state_through_the_seam() {
        adw::init().expect("libadwaita init");
        let picked: Rc<RefCell<Vec<Mode>>> = Rc::new(RefCell::new(Vec::new()));
        let row = scaling_row(
            Mutable::new(Mode::Fill).signal(),
            Mutable::new(true).signal(),
            {
                let picked = Rc::clone(&picked);
                move |mode| picked.borrow_mut().push(mode)
            },
        );
        pump();

        let tile_idx = Mode::SCALING
            .iter()
            .position(|m| *m == Mode::Tile)
            .expect("Tile is offered");
        row.set_selected(u32::try_from(tile_idx).unwrap());
        pump();

        assert_eq!(
            picked.borrow().as_slice(),
            [Mode::Tile],
            "picking an item must call on_change with exactly the mode picked"
        );
    }

    /// Insensitive with nothing set to scale, sensitive once something is —
    /// the same gate `image_row`'s inline Clear button uses.
    #[gtk::test]
    fn the_row_is_insensitive_with_nothing_to_scale() {
        adw::init().expect("libadwaita init");
        let has_wallpaper: Mutable<bool> = Mutable::new(false);
        let row = scaling_row(
            Mutable::new(Mode::Fill).signal(),
            has_wallpaper.signal(),
            |_| {},
        );
        pump();

        assert!(!row.is_sensitive(), "nothing set yet \u{21d2} insensitive");

        has_wallpaper.set(true);
        pump();
        assert!(
            row.is_sensitive(),
            "a wallpaper is now set \u{21d2} sensitive"
        );
    }

    // ── a mode outside the row's own vocabulary (#1314 review LOW-1) ───────

    /// A file holding `solid_color` must not silently render as "Fill" — it
    /// gets a named trailing placeholder instead, so the row never states a
    /// mode the state does not actually have.
    #[gtk::test]
    fn a_mode_outside_the_menu_gets_a_named_placeholder_instead_of_fill() {
        adw::init().expect("libadwaita init");
        let row = scaling_row(
            Mutable::new(Mode::SolidColor).signal(),
            Mutable::new(true).signal(),
            |_| {},
        );
        pump();

        let scaling_len = u32::try_from(Mode::SCALING.len()).unwrap();
        assert_eq!(
            row.selected(),
            scaling_len,
            "an out-of-vocabulary mode must select the trailing placeholder, \
             not silently sit on index 0 (\"Fill\")"
        );

        let model = row
            .model()
            .and_then(|m| m.downcast::<gtk::StringList>().ok())
            .expect("a StringList model");
        assert_eq!(model.n_items(), scaling_len + 1);
        assert_eq!(
            model.string(scaling_len).as_deref(),
            Some("Solid color (from file)"),
            "the placeholder must name the file's actual value"
        );
    }

    /// The whole point of the placeholder: picking a *real* entry while it
    /// is showing must actually write, and the placeholder must then drop.
    /// Before this fix, the row displayed "Fill" (index 0) while the state
    /// held `solid_color`, and re-picking "Fill" was a no-op — `AdwComboRow`
    /// only commits on an actual index change, and index 0 was already
    /// "selected" as far as the widget was concerned.
    ///
    /// **Falsification:** go back to `Mode::SCALING.iter().position(|m| *m
    /// == mode).unwrap_or(0)` with no placeholder — `row.set_selected(0)`
    /// is then a no-op (0 is already selected) and `picked` stays empty.
    #[gtk::test]
    fn picking_a_real_mode_from_the_placeholder_state_writes_through() {
        adw::init().expect("libadwaita init");
        let picked: Rc<RefCell<Vec<Mode>>> = Rc::new(RefCell::new(Vec::new()));
        let handle: Mutable<Mode> = Mutable::new(Mode::SolidColor);
        let row = scaling_row(handle.signal(), Mutable::new(true).signal(), {
            let picked = Rc::clone(&picked);
            move |mode| picked.borrow_mut().push(mode)
        });
        pump();

        let fill_idx = Mode::SCALING
            .iter()
            .position(|m| *m == Mode::Fill)
            .expect("Fill is offered");
        row.set_selected(u32::try_from(fill_idx).unwrap());
        pump();

        assert_eq!(
            picked.borrow().as_slice(),
            [Mode::Fill],
            "picking Fill while the placeholder was showing must write through"
        );

        // Production feeds the write above back through `wallpaper::set_mode`
        // → `wallpaper::mode()`'s own signal; simulate that round trip by
        // driving the same `handle` this row is bound to, and confirm the
        // placeholder is then dropped rather than left stranded.
        handle.set(Mode::Fill);
        pump();

        let model = row
            .model()
            .and_then(|m| m.downcast::<gtk::StringList>().ok())
            .expect("a StringList model");
        assert_eq!(
            model.n_items(),
            u32::try_from(Mode::SCALING.len()).unwrap(),
            "the placeholder must be dropped once the signal reflects a real mode"
        );
    }
}
