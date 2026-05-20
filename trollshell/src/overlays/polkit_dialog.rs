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

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::polkit::{self, AuthIdentity, AuthPrompt, Zeroizing};
use hytte::ui::{layer_window, Layer};

thread_local! {
    static DIALOG_WINDOW: RefCell<Option<gtk::Window>> = const { RefCell::new(None) };
}

/// Build and subscribe the polkit auth dialog for the given monitor.
/// Idempotent in practice — called once from `main.rs` on the primary
/// monitor before the GTK loop starts.
pub fn install(monitor: &Monitor) {
    let monitor = monitor.clone();
    glib::MainContext::default().spawn_local(
        polkit::auth_prompts().for_each(move |prompt| {
            match prompt {
                Some(req) if req.follow_up => update_dialog_for_followup(&req),
                Some(req) => show_dialog(&monitor, req),
                None => close_dialog(),
            }
            std::future::ready(())
        }),
    );
}

fn close_dialog() {
    DIALOG_WINDOW.with(|slot: &RefCell<Option<gtk::Window>>| {
        if let Some(w) = slot.borrow_mut().take() {
            w.close();
        }
    });
}

/// Update the existing dialog in-place for a follow-up PAM prompt
/// (e.g. "Retype new password"). The window stays mounted and
/// keyboard-grabbed; only the prompt label and entry contents change.
fn update_dialog_for_followup(prompt: &AuthPrompt) {
    let updated = DIALOG_WINDOW.with(|slot: &RefCell<Option<gtk::Window>>| {
        let slot = slot.borrow();
        let Some(window) = slot.as_ref() else { return false };
        let Some(root) = window.child() else { return false };
        let mut walker = WidgetWalker::new(root);
        if let Some(label) = walker.find_named("ts-prompt-followup-label")
            && let Ok(label) = label.downcast::<gtk::Label>()
        {
            label.set_text(&prompt.message);
            label.set_visible(!prompt.message.is_empty());
        }
        if let Some(entry_w) = walker.find_named("ts-prompt-password-entry")
            && let Ok(entry) = entry_w.downcast::<gtk::PasswordEntry>()
        {
            entry.set_text("");
            entry.grab_focus();
        }
        true
    });
    if !updated {
        tracing::warn!("polkit follow-up prompt arrived without an existing dialog");
    }
}

struct WidgetWalker {
    queue: std::collections::VecDeque<gtk::Widget>,
}
impl WidgetWalker {
    fn new(root: gtk::Widget) -> Self {
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(root);
        Self { queue }
    }
    fn find_named(&mut self, name: &str) -> Option<gtk::Widget> {
        while let Some(w) = self.queue.pop_front() {
            if w.widget_name() == name {
                return Some(w);
            }
            let mut child = w.first_child();
            while let Some(c) = child {
                self.queue.push_back(c.clone());
                child = c.next_sibling();
            }
        }
        None
    }
}

#[allow(clippy::needless_pass_by_value)]
fn show_dialog(monitor: &Monitor, prompt: AuthPrompt) {
    close_dialog();

    let window = build_dialog_window(monitor);
    let vbox = build_dialog_body();

    append_header(&vbox, &prompt);
    let selected_uid = append_identity_row(&vbox, &prompt.identities);
    let entry = append_password_entry(&vbox);
    let _followup = append_followup_label(&vbox);
    let (cancel_btn, auth_btn) = append_buttons(&vbox);

    window.set_child(Some(&vbox));
    wire_escape(&window, &entry);
    wire_submit(&entry, &auth_btn, &cancel_btn, &selected_uid);

    window.set_visible(true);
    window.present();
    entry.grab_focus();

    DIALOG_WINDOW.with(|slot| *slot.borrow_mut() = Some(window));
}

fn build_dialog_window(monitor: &Monitor) -> gtk::Window {
    let window = layer_window(monitor)
        .layer(Layer::Overlay)
        .exclusive(false)
        .keyboard_mode(KeyboardMode::Exclusive)
        .namespace("hytte-polkit")
        .build();
    window.add_css_class("ts-prompt");
    window.set_size_request(520, 280);
    window
}

fn build_dialog_body() -> gtk::Box {
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    vbox.add_css_class("ts-prompt-root");
    vbox.set_margin_start(18);
    vbox.set_margin_end(18);
    vbox.set_margin_top(18);
    vbox.set_margin_bottom(18);
    vbox
}

fn append_header(vbox: &gtk::Box, prompt: &AuthPrompt) {
    let title = gtk::Label::new(Some("Authentication required"));
    title.add_css_class("ts-prompt-title");
    title.set_xalign(0.0);
    vbox.append(&title);

    if !prompt.action_id.is_empty() {
        let action = wrapped_subtitle(&prompt.action_id);
        vbox.append(&action);
    }

    if !prompt.message.is_empty() {
        let msg = gtk::Label::new(Some(&prompt.message));
        msg.set_xalign(0.0);
        msg.set_wrap(true);
        msg.set_margin_top(4);
        vbox.append(&msg);
    }
}

fn wrapped_subtitle(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("ts-prompt-subtitle");
    label.set_xalign(0.0);
    label.set_wrap(true);
    label
}

/// When polkit offers a single identity (the common case — the user's
/// own uid) we show a static row.  When it offers multiple, we show a
/// DropDown with the local uid pre-selected by `auth_prompts`'s sort.
fn append_identity_row(vbox: &gtk::Box, identities: &[AuthIdentity]) -> Rc<Cell<u32>> {
    let selected_uid = Rc::new(Cell::new(identities.first().map_or(0, |id| id.uid)));

    if identities.len() <= 1 {
        if let Some(only) = identities.first() {
            let row = wrapped_subtitle(&format!("Authenticate as {}", only.pretty_name));
            row.set_margin_top(6);
            vbox.append(&row);
        }
        return selected_uid;
    }

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
    let identities_for_drop: Vec<AuthIdentity> = identities.to_vec();
    dropdown.connect_selected_notify(move |dd| {
        let idx = dd.selected() as usize;
        if let Some(id) = identities_for_drop.get(idx) {
            selected_for_drop.set(id.uid);
        }
    });
    row.append(&dropdown);
    vbox.append(&row);
    selected_uid
}

fn append_password_entry(vbox: &gtk::Box) -> gtk::PasswordEntry {
    let entry = gtk::PasswordEntry::new();
    entry.set_show_peek_icon(true);
    entry.set_margin_top(8);
    entry.set_widget_name("ts-prompt-password-entry");
    vbox.append(&entry);
    entry
}

fn append_followup_label(vbox: &gtk::Box) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_widget_name("ts-prompt-followup-label");
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.add_css_class("ts-prompt-followup");
    label.set_visible(false);
    vbox.append(&label);
    label
}

fn append_buttons(vbox: &gtk::Box) -> (gtk::Button, gtk::Button) {
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(4);
    let cancel_btn = gtk::Button::with_label("Cancel");
    let auth_btn = gtk::Button::with_label("Authenticate");
    auth_btn.add_css_class("suggested-action");
    buttons.append(&cancel_btn);
    buttons.append(&auth_btn);
    vbox.append(&buttons);
    (cancel_btn, auth_btn)
}

fn wire_escape(window: &gtk::Window, entry: &gtk::PasswordEntry) {
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
}

fn wire_submit(
    entry: &gtk::PasswordEntry,
    auth_btn: &gtk::Button,
    cancel_btn: &gtk::Button,
    selected_uid: &Rc<Cell<u32>>,
) {
    let submit = {
        let entry = entry.clone();
        let selected_uid = selected_uid.clone();
        move || {
            let text = Zeroizing::new(entry.text().to_string());
            polkit::respond_to_auth(Some((text, selected_uid.get())));
            // Drop cleartext immediately; dialog stays up until helper round-trip resolves.
            entry.set_text("");
        }
    };

    let submit_for_activate = submit.clone();
    entry.connect_activate(move |_| submit_for_activate());
    let submit_for_button = submit.clone();
    auth_btn.connect_clicked(move |_| submit_for_button());

    let entry_for_cancel = entry.clone();
    cancel_btn.connect_clicked(move |_| {
        polkit::respond_to_auth(None);
        entry_for_cancel.set_text("");
    });
}
