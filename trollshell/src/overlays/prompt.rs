//! Layer-shell password prompt window driven by [`hytte::services::wifi::active_prompt`].
//!
//! Call [`install`] once on the primary monitor during startup — before the
//! main event loop runs.  The function subscribes to the `active_prompt`
//! signal; when iwd asks for a passphrase a centered, focus-grabbing overlay
//! appears with an SSID label and a password entry.  Submitting (Enter or
//! Connect) calls [`wifi::submit_prompt`]; dismissing (Escape or Cancel) calls
//! [`wifi::cancel_prompt`].

use std::cell::RefCell;

use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::wifi;
use hytte::ui::{Layer, layer_window};

// ── Thread-local window storage ───────────────────────────────────────────────

thread_local! {
    static PROMPT_WINDOW: RefCell<Option<gtk::Window>> = const { RefCell::new(None) };

    /// The `active_prompt` subscription handle. Stored so [`close_all`] (and a
    /// re-`install`) can abort the prior subscription before wiring a new one —
    /// otherwise each monitor hot-plug would leak a subscription, and several
    /// would fight over the single `PROMPT_WINDOW`.
    static PROMPT_SUB: RefCell<Option<glib::JoinHandle<()>>> = const { RefCell::new(None) };
}

// ── Public entry-point ────────────────────────────────────────────────────────

/// Build and subscribe the prompt overlay for the given monitor. Called from
/// `main.rs` inside the `monitors_changed` loop, targeting the current primary
/// output. Aborts any prior subscription first, so re-installing on a new
/// monitor set re-homes the prompt cleanly. `wifi::active_prompt()` replays its
/// current value on subscribe, so a prompt that was live when the previous
/// primary vanished re-presents on the new one.
pub fn install(monitor: &Monitor) {
    // Drop any prior subscription so we never run two against the shared
    // PROMPT_WINDOW. (main.rs calls close_all before the install loop; this
    // keeps install idempotent on its own too.)
    abort_subscription();

    let monitor = monitor.clone();
    let handle =
        glib::MainContext::default().spawn_local(wifi::active_prompt().for_each(move |prompt| {
            match prompt {
                Some(req) => show_prompt(&monitor, req),
                None => close_prompt(),
            }
            std::future::ready(())
        }));
    PROMPT_SUB.with(|s| *s.borrow_mut() = Some(handle));
}

/// Abort the `active_prompt` subscription and close any open prompt window.
/// Called before rebuilding on monitor hot-plug so the prompt doesn't route
/// into a dead surface and its subscription doesn't leak.
pub fn close_all() {
    abort_subscription();
    close_prompt();
}

fn abort_subscription() {
    PROMPT_SUB.with(|s| {
        if let Some(handle) = s.borrow_mut().take() {
            handle.abort();
        }
    });
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn close_prompt() {
    // Bind the taken window before acting on it: the `if let` scrutinee's
    // `RefMut` temporary stays alive for the whole then-block (Rust 2024
    // only changed when it drops relative to an `else` branch, not this),
    // so a GTK call made directly inside the `if let` would hold the borrow
    // across it (#631) — a latent reentrancy hazard if `close()` ever
    // emits synchronously.
    let taken = PROMPT_WINDOW.with(|slot: &RefCell<Option<gtk::Window>>| slot.borrow_mut().take());
    if let Some(w) = taken {
        w.close();
    }
}

#[allow(clippy::needless_pass_by_value)]
fn show_prompt(monitor: &Monitor, req: wifi::PromptRequest) {
    // Ensure any previous prompt is gone before creating the new one.
    close_prompt();

    let window = layer_window(monitor)
        .layer(Layer::Overlay)
        .exclusive(false)
        .keyboard_mode(KeyboardMode::Exclusive)
        .namespace("hytte-prompt")
        .build();
    window.add_css_class("ts-prompt");
    // Extra room around the card so its drop-shadow isn't clipped by
    // the layer-shell surface edge.
    window.set_size_request(460, 260);

    // ── Layout ────────────────────────────────────────────────────────────────

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    vbox.add_css_class("ts-prompt-root");
    vbox.set_margin_start(18);
    vbox.set_margin_end(18);
    vbox.set_margin_top(18);
    vbox.set_margin_bottom(18);

    // Title + subtitle vary by prompt kind: a Wi-Fi passphrase ("Connect to
    // <SSID>" / "Security: psk") vs a VPN secret ("VPN password" / the
    // connection name). The VPN case has no security field.
    let title_text = match req.kind {
        wifi::PromptKind::VpnSecret => "VPN password".to_string(),
        wifi::PromptKind::WifiPassphrase => format!("Connect to {}", req.ssid),
    };
    let title = gtk::Label::new(Some(&title_text));
    title.add_css_class("ts-prompt-title");
    title.set_xalign(0.0);
    vbox.append(&title);

    let subtitle_text = match req.kind {
        // For a VPN the `ssid` field carries the connection name.
        wifi::PromptKind::VpnSecret => Some(req.ssid.clone()).filter(|s| !s.is_empty()),
        wifi::PromptKind::WifiPassphrase => {
            Some(format!("Security: {}", req.security)).filter(|_| !req.security.is_empty())
        }
    };
    if let Some(subtitle_text) = subtitle_text {
        let subtitle = gtk::Label::new(Some(&subtitle_text));
        subtitle.add_css_class("ts-prompt-subtitle");
        subtitle.set_xalign(0.0);
        vbox.append(&subtitle);
    }

    // A reopened prompt whose previously-submitted secret was rejected gets an
    // error-styled subtitle, so a retry doesn't read as a byte-identical,
    // indistinguishable re-ask.
    if req.prior_failure {
        let error_label = gtk::Label::new(Some("Authentication failed — check the passphrase"));
        error_label.add_css_class("ts-prompt-error");
        error_label.set_xalign(0.0);
        vbox.append(&error_label);
    }

    let entry = gtk::PasswordEntry::new();
    entry.set_show_peek_icon(true);
    if req.prior_failure {
        entry.add_css_class("error");
    }
    vbox.append(&entry);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(4);
    let cancel_btn = gtk::Button::with_label("Cancel");
    let connect_btn = gtk::Button::with_label("Connect");
    connect_btn.add_css_class("suggested-action");
    // Disabled until the entry holds text — an empty submit is otherwise a
    // silent no-op the agent just re-prompts for.
    connect_btn.set_sensitive(false);
    buttons.append(&cancel_btn);
    buttons.append(&connect_btn);
    vbox.append(&buttons);

    window.set_child(Some(&vbox));

    // ── ESC → cancel ──────────────────────────────────────────────────────────

    let key_ctrl = gtk::EventControllerKey::new();
    let id_for_esc = req.id;
    key_ctrl.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            wifi::cancel_prompt(id_for_esc);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_ctrl);

    // ── Entry text → Connect sensitivity ─────────────────────────────────────
    //
    // An empty submit is a silent no-op the agent just re-prompts for, so keep
    // Connect disabled (and Enter inert) until the entry holds text.

    let connect_btn_for_guard = connect_btn.clone();
    entry.connect_changed(move |e| {
        connect_btn_for_guard.set_sensitive(!e.text().is_empty());
    });

    // ── Enter in entry → submit ───────────────────────────────────────────────

    let entry_for_activate = entry.clone();
    let id_for_activate = req.id;
    entry.connect_activate(move |_| {
        let text = entry_for_activate.text().to_string();
        if text.is_empty() {
            return;
        }
        wifi::submit_prompt(id_for_activate, &text);
    });

    // ── Cancel button ─────────────────────────────────────────────────────────

    let id_for_cancel = req.id;
    cancel_btn.connect_clicked(move |_| {
        wifi::cancel_prompt(id_for_cancel);
    });

    // ── Connect button ────────────────────────────────────────────────────────

    let entry_for_connect = entry.clone();
    let id_for_connect = req.id;
    connect_btn.connect_clicked(move |_| {
        let text = entry_for_connect.text().to_string();
        if text.is_empty() {
            return;
        }
        wifi::submit_prompt(id_for_connect, &text);
    });

    // ── Show ──────────────────────────────────────────────────────────────────

    window.set_visible(true);
    window.present();
    entry.grab_focus();

    PROMPT_WINDOW.with(|slot: &RefCell<Option<gtk::Window>>| {
        *slot.borrow_mut() = Some(window);
    });
}
