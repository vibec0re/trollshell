//! Layer-shell modal driven by [`hytte::services::polkit::auth_prompts`].
//!
//! Polkit asks us to authenticate a privileged action: we present a
//! focused, keyboard-grabbing overlay with the action's message, the
//! identity to authenticate as (defaulting to the user's own uid when
//! present), and a masked password entry.  Submitting calls
//! [`polkit::respond_to_auth`] with `Some((password, uid))`; cancelling
//! (Escape or Cancel button) calls it with `None`.
//!
//! Mirrors the wifi password prompt window in `widgets::prompt`.

use std::cell::RefCell;

use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::polkit::{self, AuthPrompt};
use hytte::ui::{layer_window, Layer};

// ── Thread-local window storage ───────────────────────────────────────────────

thread_local! {
    static DIALOG_WINDOW: RefCell<Option<gtk::Window>> = const { RefCell::new(None) };
}

// ── Public entry-point ────────────────────────────────────────────────────────

/// Build and subscribe the polkit auth dialog for the given monitor.
/// Idempotent in practice — called once from `main.rs` on the primary
/// monitor before the GTK loop starts.
pub fn install(monitor: &Monitor) {
    let monitor = monitor.clone();
    glib::MainContext::default().spawn_local(
        polkit::auth_prompts().for_each(move |prompt| {
            match prompt {
                Some(req) => show_dialog(&monitor, req),
                None => close_dialog(),
            }
            std::future::ready(())
        }),
    );
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn close_dialog() {
    DIALOG_WINDOW.with(|slot: &RefCell<Option<gtk::Window>>| {
        if let Some(w) = slot.borrow_mut().take() {
            w.close();
        }
    });
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
fn show_dialog(monitor: &Monitor, prompt: AuthPrompt) {
    // Ensure any previous dialog is gone before creating the new one.
    close_dialog();

    let window = layer_window(monitor)
        .layer(Layer::Overlay)
        .exclusive(false)
        .keyboard_mode(KeyboardMode::Exclusive)
        .namespace("hytte-polkit")
        .build();
    window.add_css_class("ts-prompt");
    window.set_size_request(520, 280);

    // ── Layout ────────────────────────────────────────────────────────────────

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    vbox.add_css_class("ts-prompt-root");
    vbox.set_margin_start(18);
    vbox.set_margin_end(18);
    vbox.set_margin_top(18);
    vbox.set_margin_bottom(18);

    // Title — high-level "what's asking" line.  Action-id is shown as the
    // subtitle so the user can verify what's about to be authorised.
    let title = gtk::Label::new(Some("Authentication required"));
    title.add_css_class("ts-prompt-title");
    title.set_xalign(0.0);
    vbox.append(&title);

    if !prompt.action_id.is_empty() {
        let action = gtk::Label::new(Some(&prompt.action_id));
        action.add_css_class("ts-prompt-subtitle");
        action.set_xalign(0.0);
        action.set_wrap(true);
        vbox.append(&action);
    }

    if !prompt.message.is_empty() {
        let msg = gtk::Label::new(Some(&prompt.message));
        msg.set_xalign(0.0);
        msg.set_wrap(true);
        msg.set_margin_top(4);
        vbox.append(&msg);
    }

    // ── Identity selection ────────────────────────────────────────────────────
    //
    // When polkit offers a single identity (the common case — the user's
    // own uid) we show a static row.  When it offers multiple, we show a
    // DropDown with the local uid pre-selected by `auth_prompts`'s sort.

    let identities = prompt.identities.clone();
    let selected_uid: std::rc::Rc<std::cell::Cell<u32>> =
        std::rc::Rc::new(std::cell::Cell::new(
            identities.first().map_or(0, |id| id.uid),
        ));

    if identities.len() <= 1 {
        if let Some(only) = identities.first() {
            let row = gtk::Label::new(Some(&format!("Authenticate as {}", only.pretty_name)));
            row.set_xalign(0.0);
            row.add_css_class("ts-prompt-subtitle");
            row.set_margin_top(6);
            vbox.append(&row);
        }
    } else {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_margin_top(6);
        let lbl = gtk::Label::new(Some("Authenticate as:"));
        lbl.set_xalign(0.0);
        row.append(&lbl);
        let names: Vec<&str> = identities.iter().map(|i| i.pretty_name.as_str()).collect();
        let dropdown = gtk::DropDown::from_strings(&names);
        dropdown.set_selected(0);
        dropdown.set_hexpand(true);
        let selected_for_drop = selected_uid.clone();
        let identities_for_drop = identities.clone();
        dropdown.connect_selected_notify(move |dd| {
            let idx = dd.selected() as usize;
            if let Some(id) = identities_for_drop.get(idx) {
                selected_for_drop.set(id.uid);
            }
        });
        row.append(&dropdown);
        vbox.append(&row);
    }

    // ── Password entry ────────────────────────────────────────────────────────

    let entry = gtk::PasswordEntry::new();
    entry.set_show_peek_icon(true);
    entry.set_margin_top(8);
    vbox.append(&entry);

    // ── Buttons ───────────────────────────────────────────────────────────────

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(4);
    let cancel_btn = gtk::Button::with_label("Cancel");
    let auth_btn = gtk::Button::with_label("Authenticate");
    auth_btn.add_css_class("suggested-action");
    buttons.append(&cancel_btn);
    buttons.append(&auth_btn);
    vbox.append(&buttons);

    window.set_child(Some(&vbox));

    // ── ESC → cancel ──────────────────────────────────────────────────────────

    let key_ctrl = gtk::EventControllerKey::new();
    let entry_for_esc = entry.clone();
    key_ctrl.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            polkit::respond_to_auth(None);
            // Drop cleartext from the GtkEntry buffer before the dialog hides.
            entry_for_esc.set_text("");
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_ctrl);

    // ── Submit (Enter or Authenticate button) ─────────────────────────────────

    let submit = {
        let entry = entry.clone();
        let selected_uid = selected_uid.clone();
        move || {
            let text = entry.text().to_string();
            polkit::respond_to_auth(Some((text, selected_uid.get())));
            // Drop cleartext from the GtkEntry buffer immediately; the
            // dialog stays up until the helper round-trip resolves.
            entry.set_text("");
        }
    };

    let submit_for_activate = submit.clone();
    entry.connect_activate(move |_| submit_for_activate());
    let submit_for_button = submit.clone();
    auth_btn.connect_clicked(move |_| submit_for_button());

    // ── Cancel ────────────────────────────────────────────────────────────────

    let entry_for_cancel = entry.clone();
    cancel_btn.connect_clicked(move |_| {
        polkit::respond_to_auth(None);
        entry_for_cancel.set_text("");
    });

    // ── Show ──────────────────────────────────────────────────────────────────

    window.set_visible(true);
    window.present();
    entry.grab_focus();

    DIALOG_WINDOW.with(|slot: &RefCell<Option<gtk::Window>>| {
        *slot.borrow_mut() = Some(window);
    });
}
