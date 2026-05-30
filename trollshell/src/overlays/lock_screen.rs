//! Native trollshell lock screen — `ext-session-lock-v1`.
//!
//! When `screensaver::is_locked()` flips to `true`, we create a fresh
//! `gtk4_session_lock::Instance`, request a lock from the compositor,
//! and mount one `gtk::Window` per monitor as the compositor advertises
//! them via `connect_monitor` (the `v1_2` feature). That callback fires
//! for every monitor present at lock-time *and* for any output
//! hot-plugged while the session stays locked — the spec requires a
//! lock surface on every output or the compositor blanks it rather
//! than revealing the desktop. The first monitor to surface gets the
//! password entry + clock; subsequent monitors get a clock-only black
//! cover.
//!
//! PAM authentication runs on `spawn_blocking`; on success the worker
//! calls `screensaver::handle_unlock_success()`, which flips
//! `is_locked` back to `false`. This subscription sees the change and
//! calls `instance.unlock()`; the compositor confirms with `unlocked`
//! and our handler drops the `Instance` (and with it the per-monitor
//! windows).
//!
//! # Why ext-session-lock-v1 and not `Layer::Overlay`
//!
//! `Layer::Overlay` + `KeyboardMode::Exclusive` is enough to *display*
//! a lock screen, but cannot enforce the security property. If
//! trollshell segfaults, OOMs, or panics while locked, layer-shell
//! windows die with the process and the desktop becomes visible
//! again. Under `ext-session-lock-v1` the compositor keeps the
//! session locked even if the client dies — outputs are blanked to a
//! solid color until a fresh lock client takes over. Input isolation,
//! multi-monitor coverage, and hot-plug behavior are all
//! compositor-enforced rather than racy best-effort.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use hytte::futures_signals::signal::SignalExt;
use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::{clock, screensaver};
use hytte::ui::session_lock;

use hytte_pam::{authenticate, PamError, Zeroizing};

thread_local! {
    static ACTIVE_LOCK: RefCell<Option<session_lock::Instance>> =
        const { RefCell::new(None) };
    static SUBS_INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// Wire up the lock-screen subscription. Idempotent — safe to call
/// once at startup. Subscribes to `screensaver::is_locked()` on the
/// GTK main loop; the actual `Instance` is created/destroyed each
/// lock cycle.
pub fn install() {
    if !session_lock::is_supported() {
        tracing::error!(
            "compositor does not advertise ext-session-lock-v1; \
             lock screen disabled. niri ships this since v0.1.0."
        );
        return;
    }
    if SUBS_INSTALLED.with(Cell::get) {
        return;
    }
    SUBS_INSTALLED.with(|c| c.set(true));

    glib::MainContext::default().spawn_local(
        screensaver::is_locked().for_each(|locked| {
            if locked {
                start_lock();
            } else {
                end_lock();
            }
            std::future::ready(())
        }),
    );
}

fn start_lock() {
    if ACTIVE_LOCK.with(|c| c.borrow().is_some()) {
        return;
    }

    let instance = session_lock::Instance::new();

    instance.connect_locked(|_| tracing::info!("session locked"));
    instance.connect_failed(|_| {
        tracing::error!("ext-session-lock-v1 lock request failed");
        ACTIVE_LOCK.with(|c| c.borrow_mut().take());
        // Reset the signal so the next lock attempt isn't a silent no-op.
        screensaver::handle_unlock_success();
    });
    instance.connect_unlocked(|_| {
        tracing::info!("session unlocked");
        ACTIVE_LOCK.with(|c| c.borrow_mut().take());
    });

    // `connect_monitor` fires once per existing output at lock-time and
    // once per hot-plugged output thereafter. First fire owns the
    // primary UI (password entry); the rest get a clock-only cover.
    let primary_assigned = Rc::new(Cell::new(false));
    instance.connect_monitor(move |inst, monitor| {
        let is_primary = !primary_assigned.replace(true);
        let window = build_lock_window(is_primary);
        inst.assign_window_to_monitor(&window, monitor);
        // No present() — session-lock manages surface lifecycle.
    });

    if !instance.lock() {
        tracing::warn!("session_lock instance refused immediate lock");
    }

    ACTIVE_LOCK.with(|c| *c.borrow_mut() = Some(instance));
}

fn end_lock() {
    let inst = ACTIVE_LOCK.with(|c| c.borrow().clone());
    if let Some(inst) = inst {
        inst.unlock();
        // connect_unlocked drops ACTIVE_LOCK once the compositor confirms.
    }
}

fn build_lock_window(primary: bool) -> gtk::Window {
    let window = gtk::Window::new();
    window.add_css_class("ts-lock-root");

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.set_valign(gtk::Align::Center);
    outer.set_halign(gtk::Align::Center);

    let card = gtk::Box::new(gtk::Orientation::Vertical, 16);
    card.add_css_class("ts-lock-card");
    card.set_halign(gtk::Align::Center);

    append_clock_and_date(&card);
    if primary {
        build_primary_ui(&card);
    }

    outer.append(&card);
    window.set_child(Some(&outer));
    window
}

fn append_clock_and_date(card: &gtk::Box) {
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
}

fn build_primary_ui(card: &gtk::Box) {
    let user_label = gtk::Label::new(Some(&current_username()));
    user_label.add_css_class("ts-lock-user");
    user_label.set_xalign(0.5);
    card.append(&user_label);

    let entry = gtk::PasswordEntry::new();
    // No peek icon: prevents shoulder-surf reveal on a lock screen.
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

    wire_submit(&entry, card, &error_label, &spinner, &submit_btn);

    // Grab focus once realized so the user can start typing immediately.
    let entry_for_focus = entry.clone();
    glib::idle_add_local_once(move || {
        entry_for_focus.grab_focus();
    });
}

fn wire_submit(
    entry: &gtk::PasswordEntry,
    card: &gtk::Box,
    error_label: &gtk::Label,
    spinner: &gtk::Spinner,
    submit_btn: &gtk::Button,
) {
    let entry_c = entry.clone();
    let card_c = card.clone();
    let error_c = error_label.clone();
    let spinner_c = spinner.clone();
    let btn_c = submit_btn.clone();
    let submit = move || submit_password(&entry_c, &card_c, &error_c, &spinner_c, &btn_c);

    let submit_for_enter = submit.clone();
    entry.connect_activate(move |_| submit_for_enter());
    let submit_for_btn = submit;
    submit_btn.connect_clicked(move |_| submit_for_btn());
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
        // Route through the shared hytte runtime — the GTK main thread
        // has no tokio Handle::current(), so bare spawn_blocking would
        // panic on first submit.
        let result = hytte::reactive::runtime::handle()
            .spawn_blocking(move || authenticate("trollshell", &username, password))
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
