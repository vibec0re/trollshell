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
}

// ── Public entry-point ────────────────────────────────────────────────────────

/// Build and subscribe the prompt overlay for the given monitor.  Idempotent
/// in practice — called once from `main.rs` before the GTK loop starts.
pub fn install(monitor: &Monitor) {
    let monitor = monitor.clone();
    glib::MainContext::default().spawn_local(wifi::active_prompt().for_each(move |prompt| {
        match prompt {
            Some(req) => show_prompt(&monitor, req),
            None => close_prompt(),
        }
        std::future::ready(())
    }));
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn close_prompt() {
    PROMPT_WINDOW.with(|slot: &RefCell<Option<gtk::Window>>| {
        if let Some(w) = slot.borrow_mut().take() {
            w.close();
        }
    });
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

    let title = gtk::Label::new(Some(&format!("Connect to {}", req.ssid)));
    title.add_css_class("ts-prompt-title");
    title.set_xalign(0.0);
    vbox.append(&title);

    if !req.security.is_empty() {
        let subtitle = gtk::Label::new(Some(&format!("Security: {}", req.security)));
        subtitle.add_css_class("ts-prompt-subtitle");
        subtitle.set_xalign(0.0);
        vbox.append(&subtitle);
    }

    let entry = gtk::PasswordEntry::new();
    entry.set_show_peek_icon(true);
    vbox.append(&entry);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(4);
    let cancel_btn = gtk::Button::with_label("Cancel");
    let connect_btn = gtk::Button::with_label("Connect");
    connect_btn.add_css_class("suggested-action");
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

    // ── Enter in entry → submit ───────────────────────────────────────────────

    let entry_for_activate = entry.clone();
    let id_for_activate = req.id;
    entry.connect_activate(move |_| {
        let text = entry_for_activate.text().to_string();
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
