//! Native trollshell lock screen.
//!
//! When `screensaver::is_locked()` emits `true`, mounts a layer-shell
//! window per monitor on `Layer::Overlay` with `KeyboardMode::Exclusive`.
//! The first installed monitor gets the password entry + clock; secondary
//! monitors get a clock-only black-cover. PAM authentication runs on a
//! `spawn_blocking` worker; on success `screensaver::handle_unlock_success()`
//! flips the signal and clears the surfaces.
//!
//! # Limitations
//!
//! - Hot-plug / monitor disconnect while locked is not handled. If the
//!   primary monitor is unplugged mid-lock, no other monitor inherits
//!   the entry. v0.3 polish.
//! - Wallpaper-blur is not implemented; the lock root has a 0.95-alpha
//!   `@window_bg_color` background which lets a small amount of
//!   wallpaper bleed through for visual continuity.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use hytte::futures_signals::signal::SignalExt;
use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::{clock, screensaver};
use hytte::ui::layer_window;

use hytte_pam::{authenticate, PamError, Zeroizing};

thread_local! {
    static LOCK_SURFACES: RefCell<HashMap<String, LockSurface>> =
        RefCell::new(HashMap::new());
    static SUBS_INSTALLED: Cell<bool> = const { Cell::new(false) };
}

struct LockSurface {
    window: gtk::Window,
    primary: Option<PrimaryUi>,
}

struct PrimaryUi {
    entry: gtk::PasswordEntry,
    error_label: gtk::Label,
    spinner: gtk::Spinner,
    submit_btn: gtk::Button,
    #[allow(dead_code)] // reserved for future shake-on-lock-state-change
    card: gtk::Box,
}

pub fn install(monitors: &[Monitor]) {
    if monitors.is_empty() {
        tracing::warn!("lock_screen::install called with no monitors");
        return;
    }

    for (idx, monitor) in monitors.iter().enumerate() {
        let connector = match monitor.connector() {
            Some(c) if !c.is_empty() => c,
            _ => continue,
        };
        let primary = idx == 0;
        let surface = build_lock_surface(monitor, primary);
        LOCK_SURFACES.with(|map| map.borrow_mut().insert(connector, surface));
    }

    if !SUBS_INSTALLED.with(Cell::get) {
        SUBS_INSTALLED.with(|c| c.set(true));
        install_lock_subscription();
    }
}

#[allow(clippy::too_many_lines)]
fn build_lock_surface(monitor: &Monitor, primary: bool) -> LockSurface {
    let window = layer_window(monitor)
        .layer(Layer::Overlay)
        .anchor(Anchor::Top)
        .anchor(Anchor::Bottom)
        .anchor(Anchor::Left)
        .anchor(Anchor::Right)
        .exclusive(false)
        .keyboard_mode(KeyboardMode::Exclusive)
        .namespace("hytte-lock")
        .build();
    window.add_css_class("ts-lock-root");
    window.set_visible(false);

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.set_valign(gtk::Align::Center);
    outer.set_halign(gtk::Align::Center);

    let card = gtk::Box::new(gtk::Orientation::Vertical, 16);
    card.add_css_class("ts-lock-card");
    card.set_halign(gtk::Align::Center);

    let clock_label = gtk::Label::new(None);
    clock_label.add_css_class("ts-lock-clock");
    clock_label.set_xalign(0.5);
    bind(
        clock::now().map(|dt| dt.format("%H:%M").to_string()),
        &clock_label,
        |w, t| w.set_text(&t),
    );
    card.append(&clock_label);

    let date_label = gtk::Label::new(None);
    date_label.add_css_class("ts-lock-date");
    date_label.set_xalign(0.5);
    bind(
        clock::now().map(|dt| dt.format("%A, %B %-d").to_string()),
        &date_label,
        |w, t| w.set_text(&t),
    );
    card.append(&date_label);

    let primary_ui = if primary {
        let user_label = gtk::Label::new(Some(&current_username()));
        user_label.add_css_class("ts-lock-user");
        user_label.set_xalign(0.5);
        card.append(&user_label);

        let entry = gtk::PasswordEntry::new();
        entry.set_show_peek_icon(false);
        entry.add_css_class("ts-lock-entry");
        entry.set_width_chars(28);
        card.append(&entry);

        let error_label = gtk::Label::new(None);
        error_label.add_css_class("ts-lock-error");
        error_label.set_xalign(0.5);
        error_label.set_visible(false);
        card.append(&error_label);

        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);
        card.append(&spinner);

        let submit_btn = gtk::Button::with_label("Authenticate");
        submit_btn.add_css_class("suggested-action");
        submit_btn.set_halign(gtk::Align::Center);
        card.append(&submit_btn);

        let entry_for_submit = entry.clone();
        let card_for_submit = card.clone();
        let error_for_submit = error_label.clone();
        let spinner_for_submit = spinner.clone();
        let submit_btn_for_submit = submit_btn.clone();
        let submit = move || {
            submit_password(
                &entry_for_submit,
                &card_for_submit,
                &error_for_submit,
                &spinner_for_submit,
                &submit_btn_for_submit,
            );
        };

        let submit_for_enter = submit.clone();
        entry.connect_activate(move |_| submit_for_enter());

        let submit_for_btn = submit;
        submit_btn.connect_clicked(move |_| submit_for_btn());

        Some(PrimaryUi {
            entry,
            error_label,
            spinner,
            submit_btn,
            card: card.clone(),
        })
    } else {
        None
    };

    outer.append(&card);
    window.set_child(Some(&outer));

    LockSurface {
        window,
        primary: primary_ui,
    }
}

fn submit_password(
    entry: &gtk::PasswordEntry,
    card: &gtk::Box,
    error: &gtk::Label,
    spinner: &gtk::Spinner,
    submit_btn: &gtk::Button,
) {
    let password = Zeroizing::new(entry.text().to_string());
    entry.set_text("");
    error.set_visible(false);
    spinner.set_visible(true);
    spinner.set_spinning(true);
    submit_btn.set_sensitive(false);
    entry.set_sensitive(false);

    let username = current_username();
    let entry_for_done = entry.clone();
    let card_for_done = card.clone();
    let error_for_done = error.clone();
    let spinner_for_done = spinner.clone();
    let submit_for_done = submit_btn.clone();

    glib::MainContext::default().spawn_local(async move {
        let result = tokio::task::spawn_blocking(move || {
            authenticate("trollshell", &username, password)
        })
        .await
        .unwrap_or_else(|_| Err(PamError::Service("blocking task panicked".into())));

        spinner_for_done.set_spinning(false);
        spinner_for_done.set_visible(false);
        submit_for_done.set_sensitive(true);
        entry_for_done.set_sensitive(true);
        entry_for_done.grab_focus();

        match result {
            Ok(()) => screensaver::handle_unlock_success(),
            Err(PamError::AuthFailed) => {
                show_auth_error(&error_for_done, "Incorrect password");
                shake(&card_for_done);
            }
            Err(PamError::Service(msg)) => {
                tracing::warn!(error = %msg, "PAM service error");
                show_auth_error(&error_for_done, "Authentication unavailable");
            }
        }
    });
}

fn show_auth_error(label: &gtk::Label, text: &str) {
    label.set_text(text);
    label.set_visible(true);
}

fn shake(card: &gtk::Box) {
    card.add_css_class("ts-lock-shake");
    let card_for_clear = card.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(450), move || {
        card_for_clear.remove_css_class("ts-lock-shake");
    });
}

fn current_username() -> String {
    nix::unistd::User::from_uid(nix::unistd::Uid::current())
        .ok()
        .flatten()
        .map(|u| u.name)
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "user".to_string())
}

fn install_lock_subscription() {
    glib::MainContext::default().spawn_local(
        screensaver::is_locked().for_each(|locked| {
            LOCK_SURFACES.with(|map| {
                let map = map.borrow();
                if locked {
                    for surface in map.values() {
                        surface.window.set_visible(true);
                        if let Some(p) = surface.primary.as_ref() {
                            p.error_label.set_visible(false);
                            p.entry.set_text("");
                            p.spinner.set_spinning(false);
                            p.spinner.set_visible(false);
                            p.submit_btn.set_sensitive(true);
                            p.entry.set_sensitive(true);
                            p.entry.grab_focus();
                        }
                    }
                } else {
                    for surface in map.values() {
                        if let Some(p) = surface.primary.as_ref() {
                            p.entry.set_text("");
                            p.error_label.set_visible(false);
                        }
                        surface.window.set_visible(false);
                    }
                }
            });
            std::future::ready(())
        }),
    );
}
