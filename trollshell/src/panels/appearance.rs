//! Appearance / wallpaper drawer panel.
//!
//! v1: a single image, applied to every output. The user picks a file with
//! `gtk::FileDialog`; the wallpaper service writes the path to
//! `~/.config/trollshell/wallpaper.path` and restarts `swaybg.service` so
//! the change takes effect immediately.
//!
//! Per-output (per-monitor) wallpaper, time-of-day rotation, and an explicit
//! "Clear" button are deliberately deferred. See `etc/wallpaper/README.md`.
//!
//! Also home to the **Night light** toggle (color temperature) — an appearance
//! concern that flips the zero-state `wlsunset` user unit via the `nightlight`
//! service. Config (lat/lon + day/night temps) lives in the nix module.

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self, gio};
use hytte::prelude::*;
use hytte::services::{nightlight, wallpaper};

use crate::components::layout::{finish_page, page_box};

pub fn panel_appearance() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    let group = adw::PreferencesGroup::builder().title("Wallpaper").build();

    let row = adw::ActionRow::builder().title("Image").build();
    bind(
        wallpaper::current_path().map(|p| match p {
            Some(p) => wallpaper_basename(&p),
            None => "Not set".to_string(),
        }),
        &row,
        |row, text| row.set_subtitle(&text),
    );
    // Long file paths shouldn't push the modal wide; let the subtitle wrap.
    row.set_subtitle_lines(0);

    let browse = gtk::Button::with_label("Browse\u{2026}");
    browse.set_valign(gtk::Align::Center);
    browse.add_css_class("flat");
    let browse_for_handler = browse.clone();
    browse.connect_clicked(move |_| {
        open_wallpaper_picker(&browse_for_handler);
    });
    row.add_suffix(&browse);
    row.set_activatable_widget(Some(&browse));
    group.add(&row);

    column.append(&group);
    column.append(&build_display_group());
    finish_page(&column)
}

/// "Display" preferences group holding the Night light toggle. The switch's
/// `active` is driven by the daemon's authoritative state
/// (`nightlight::enabled()`), NOT local widget state — so any monitor's drawer
/// reflects the same toggle and a drawer rebuild never loses track. Flipping it
/// starts/stops the `wlsunset` user unit.
fn build_display_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Display").build();

    let row = adw::SwitchRow::builder()
        .title("Night light")
        .subtitle("Warm the screen's color temperature after sunset")
        .build();

    // Two-way: the authoritative signal drives `active` (the block prevents the
    // programmatic set_active from re-entering the handler); a user flip calls
    // set_enabled, which toggles the user unit off the GTK thread.
    bind_two_way(
        nightlight::enabled(),
        &row,
        adw::SwitchRow::set_active,
        |r| r.connect_active_notify(|r| nightlight::set_enabled(r.is_active())),
    );

    group.add(&row);
    group
}

/// Last path component of a wallpaper path, with the original returned if
/// it has no separator (e.g. relative or already-bare filename).
fn wallpaper_basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |s| s.to_string_lossy().into_owned())
}

/// Open a `gtk::FileDialog` to pick a wallpaper image. On selection, hands
/// the absolute path to the wallpaper service (which persists + restarts
/// the swaybg unit). Cancellation / error is logged at debug level.
fn open_wallpaper_picker(parent_widget: &gtk::Button) {
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

    // Resolve the parent window so the dialog is modal-anchored to the
    // drawer's layer-shell surface; without it the dialog is parentless
    // and may not receive focus correctly under niri.
    let parent = parent_widget
        .root()
        .and_then(|root| root.downcast::<gtk::Window>().ok());

    dialog.open(parent.as_ref(), gio::Cancellable::NONE, |result| {
        match result {
            Ok(file) => {
                if let Some(path) = file.path() {
                    let s = path.to_string_lossy().into_owned();
                    wallpaper::set_path(&s);
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
    });
}
