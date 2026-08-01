//! Layer-shell secret prompt window driven by [`hytte::services::wifi::active_prompt`].
//!
//! Call [`install`] once on the primary monitor during startup — before the
//! main event loop runs.  The function subscribes to the `active_prompt`
//! signal; when iwd or `NetworkManager` asks for a secret a centered,
//! focus-grabbing overlay appears with a title and one password entry per
//! requested secret.  Submitting (Enter or Connect) calls
//! [`wifi::submit_prompt`] with every collected value; dismissing (Escape or
//! Cancel) calls [`wifi::cancel_prompt`].
//!
//! ## One prompt per `GetSecrets` round
//!
//! [`wifi::PromptRequest::secret_keys`] is an ordered, non-empty list, and the
//! overlay returns exactly one value per entry. Almost always that is a single
//! key — a Wi-Fi passphrase, or a VPN with one password — which renders as the
//! historical single unlabelled entry. When `NetworkManager` asks for several
//! secrets in one round (a VPN wanting a password *and* a one-time code), all
//! of them are collected in this one dialog so the agent can answer the whole
//! request in a single reply (#652). Sequential prompts would not do: NM
//! re-asks for, or fails the activation over, any secret it requested and did
//! not get back, so a partial answer is a failed round rather than a slower one.
//!
//! ## In-flight state
//!
//! On submit the form latches into a busy state — every entry insensitive,
//! Connect insensitive, a spinner shown — so a click visibly registers and a
//! second submit can't race the first. **Cancel stays live throughout** and the
//! latch is bounded by [`SUBMIT_TIMEOUT`], so the prompt can never strand the
//! user in a permanently busy window.
//!
//! What the latch deliberately does *not* claim is that the connection is being
//! established. Both agents clear `active_prompt` to `None` the instant the
//! secret is handed to the daemon, not when the association resolves, so this
//! window is normally torn down within milliseconds of Connect and the busy
//! state is invisible on the happy path — by design. Covering the seconds-long
//! association would mean keeping a keyboard-exclusive layer surface on screen
//! long after the user finished typing, driven by a signal
//! (`StationState::Connecting`) that does not exist for the VPN path at all.
//! The panel-side "Connecting…" in `panels/network/wifi.rs` remains the tell
//! for that phase.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::wifi;
use hytte::ui::{Layer, layer_window};

// ── Tunables ──────────────────────────────────────────────────────────────────

/// Upper bound on the post-submit in-flight state.
///
/// The window is normally closed by the daemon clearing `active_prompt` within
/// milliseconds, so this timer almost never fires. It exists so that a wedged
/// or never-resolved handshake degrades to a usable form the user can retry
/// from, rather than a dead dialog with everything greyed out — a prompt stuck
/// busy forever would be worse than no busy state at all.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(20);

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

/// Dismiss the prompt: tell the daemon (if it is still waiting) and take the
/// window down immediately.
///
/// Closing locally rather than waiting for `active_prompt` to go `None` is what
/// keeps Cancel responsive **while the form is busy**: by then the waiter has
/// already been consumed by the submit, so `cancel_prompt` is a no-op and
/// nothing else would ever close the window on the user's behalf until the
/// daemon got round to it. The already-submitted secret is on its way to the
/// daemon regardless — dismissing the window does not, and cannot, recall it.
fn dismiss(id: u64) {
    wifi::cancel_prompt(id);
    close_prompt();
}

/// Turn a daemon-side secret key into a field label: `"leap-password"` →
/// `"Leap password"`, `"Gateway Password"` → unchanged. Only shown when a
/// prompt collects more than one secret, so the single-secret Wi-Fi/VPN case
/// never renders `"Psk"` or `"Password"` above its entry.
fn field_label(key: &str) -> String {
    let spaced: String = key
        .chars()
        .map(|c| if c == '-' || c == '_' { ' ' } else { c })
        .collect();
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

/// Title, subtitle and (on a retry) the error line.
fn build_header(vbox: &gtk::Box, req: &wifi::PromptRequest) {
    // Title + subtitle vary by prompt kind: a Wi-Fi passphrase ("Connect to
    // <SSID>" / "Security: psk") vs a VPN secret ("VPN password" / the
    // connection name). The VPN case has no security field.
    let title_text = match req.kind {
        wifi::PromptKind::VpnSecret if req.secret_keys.len() > 1 => "VPN credentials".to_string(),
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
    // indistinguishable re-ask. NM does not say *which* secret it rejected, so
    // a multi-field round can only point at the set.
    if req.prior_failure {
        let error_text = match (req.kind, req.secret_keys.len()) {
            (wifi::PromptKind::VpnSecret, 1) => "Authentication failed — check the password",
            (wifi::PromptKind::VpnSecret, _) => "Authentication failed — check the credentials",
            (wifi::PromptKind::WifiPassphrase, _) => "Authentication failed — check the passphrase",
        };
        let error_label = gtk::Label::new(Some(error_text));
        error_label.add_css_class("ts-prompt-error");
        error_label.set_xalign(0.0);
        vbox.append(&error_label);
    }
}

/// One [`gtk::PasswordEntry`] per requested secret, in request order. A single
/// key renders bare (the pre-existing look); several get a label each, since
/// otherwise there is no way to tell which box wants the one-time code.
fn build_entries(vbox: &gtk::Box, req: &wifi::PromptRequest) -> Vec<gtk::PasswordEntry> {
    let labelled = req.secret_keys.len() > 1;
    req.secret_keys
        .iter()
        .map(|key| {
            if labelled {
                let label = gtk::Label::new(Some(&field_label(key)));
                label.add_css_class("ts-prompt-field-label");
                label.set_xalign(0.0);
                vbox.append(&label);
            }
            let entry = gtk::PasswordEntry::new();
            entry.set_show_peek_icon(true);
            if req.prior_failure {
                entry.add_css_class("error");
            }
            vbox.append(&entry);
            entry
        })
        .collect()
}

// ── The form + its in-flight latch ────────────────────────────────────────────

/// The interactive part of one prompt window: the fields plus the widgets whose
/// state tracks whether a submit is in flight.
struct Form {
    entries: Vec<gtk::PasswordEntry>,
    connect_btn: gtk::Button,
    spinner: gtk::Spinner,
    /// `true` between a submit and either the window closing or
    /// [`SUBMIT_TIMEOUT`] expiring. Guards against a second submit racing the
    /// first, which with several fields would otherwise be easy to trigger by
    /// hitting Enter twice.
    busy: Cell<bool>,
}

impl Form {
    /// `true` when every field holds text. An empty field is a failed round,
    /// not a shorter one — NM re-asks for whatever it didn't get back — so
    /// Connect stays disabled until all of them are filled.
    fn all_filled(&self) -> bool {
        self.entries.iter().all(|e| !e.text().is_empty())
    }

    /// Re-evaluate Connect's sensitivity after a keystroke.
    fn refresh_sensitivity(&self) {
        self.connect_btn
            .set_sensitive(!self.busy.get() && self.all_filled());
    }

    /// Enter or leave the in-flight state. Cancel is deliberately untouched: it
    /// must stay clickable so a busy prompt always has a way out.
    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        for entry in &self.entries {
            entry.set_sensitive(!busy);
        }
        self.spinner.set_spinning(busy);
        self.spinner.set_visible(busy);
        self.refresh_sensitivity();
    }

    /// Collect every field and hand the whole set to the agent.
    fn submit(self: &Rc<Self>, id: u64) {
        if self.busy.get() {
            return;
        }
        // Send the user to the first gap rather than submitting a partial set.
        if let Some(empty) = self.entries.iter().find(|e| e.text().is_empty()) {
            empty.grab_focus();
            return;
        }
        let values: Vec<String> = self.entries.iter().map(|e| e.text().to_string()).collect();
        self.set_busy(true);
        wifi::submit_prompt(id, &values);

        // Bounded in-flight state. Every *normal* exit is the window being torn
        // down — by `active_prompt` going `None` once the daemon takes the
        // secret, by a `CancelGetSecrets` doing the same, or by Cancel/Escape —
        // and `show_prompt` clears the latch on unmap for exactly those cases,
        // so this timer stays quiet through them. It covers only the abnormal
        // remainder (a handshake that never resolves, or a `submit_prompt` that
        // silently no-ops because the service was never registered): it
        // un-busies the form in place so the user can edit and retry.
        let form = Rc::clone(self);
        glib::timeout_add_local_once(SUBMIT_TIMEOUT, move || {
            if form.busy.get() {
                tracing::warn!(
                    id,
                    "secret prompt: no response {}s after submit — re-enabling the form",
                    SUBMIT_TIMEOUT.as_secs(),
                );
                form.set_busy(false);
                if let Some(first) = form.entries.first() {
                    first.grab_focus();
                }
            }
        });
    }
}

// ── Window construction ───────────────────────────────────────────────────────

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
    // the layer-shell surface edge. A multi-field prompt simply grows past
    // this minimum.
    window.set_size_request(460, 260);

    // ── Layout ────────────────────────────────────────────────────────────────

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 8);
    vbox.add_css_class("ts-prompt-root");
    vbox.set_margin_start(18);
    vbox.set_margin_end(18);
    vbox.set_margin_top(18);
    vbox.set_margin_bottom(18);

    build_header(&vbox, &req);
    let entries = build_entries(&vbox, &req);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(4);

    // In-flight spinner, left of the buttons. Starts hidden so there is never a
    // frame of stopped spinner (matching the `panels::appearance` idiom); no
    // CSS class, like the `panels::bluetooth` busy rows.
    let spinner = gtk::Spinner::new();
    spinner.set_valign(gtk::Align::Center);
    spinner.set_visible(false);
    buttons.append(&spinner);

    let cancel_btn = gtk::Button::with_label("Cancel");
    let connect_btn = gtk::Button::with_label("Connect");
    connect_btn.add_css_class("suggested-action");
    // Disabled until every entry holds text — a partial submit is otherwise a
    // silent no-op the agent just re-prompts for.
    connect_btn.set_sensitive(false);
    buttons.append(&cancel_btn);
    buttons.append(&connect_btn);
    vbox.append(&buttons);

    window.set_child(Some(&vbox));

    let form = Rc::new(Form {
        entries,
        connect_btn: connect_btn.clone(),
        spinner,
        busy: Cell::new(false),
    });

    // ── Teardown clears the latch ─────────────────────────────────────────────
    //
    // Every normal exit from the in-flight state is this window going away, and
    // the window going away does *not* by itself touch the form. Without this,
    // `Form::submit`'s safety timer would fire 20 s after every **successful**
    // submit, find `busy` still set, and warn about a prompt that resolved
    // perfectly — so the one path that is supposed to be silent would be the
    // noisiest. Clearing the flag on unmap (the `panels::power_menu` idiom)
    // leaves the timer for the genuinely-stuck case it exists for.
    let form_for_unmap = Rc::clone(&form);
    window.connect_unmap(move |_| form_for_unmap.busy.set(false));

    // ── ESC → cancel ──────────────────────────────────────────────────────────

    let key_ctrl = gtk::EventControllerKey::new();
    let id = req.id;
    key_ctrl.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            dismiss(id);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_ctrl);

    // ── Per-entry wiring: text → Connect sensitivity, Enter → submit ─────────

    for entry in &form.entries {
        let form_for_guard = Rc::clone(&form);
        entry.connect_changed(move |_| form_for_guard.refresh_sensitivity());

        let form_for_activate = Rc::clone(&form);
        entry.connect_activate(move |_| form_for_activate.submit(id));
    }

    // ── Buttons ───────────────────────────────────────────────────────────────

    cancel_btn.connect_clicked(move |_| dismiss(id));

    let form_for_connect = Rc::clone(&form);
    connect_btn.connect_clicked(move |_| form_for_connect.submit(id));

    // ── Show ──────────────────────────────────────────────────────────────────

    window.set_visible(true);
    window.present();
    if let Some(first) = form.entries.first() {
        first.grab_focus();
    }

    PROMPT_WINDOW.with(|slot: &RefCell<Option<gtk::Window>>| {
        *slot.borrow_mut() = Some(window);
    });
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::field_label;

    #[test]
    fn field_label_capitalises_a_bare_key() {
        assert_eq!(field_label("password"), "Password");
    }

    #[test]
    fn field_label_turns_separators_into_spaces() {
        assert_eq!(field_label("leap-password"), "Leap password");
        assert_eq!(field_label("group_password"), "Group password");
    }

    #[test]
    fn field_label_leaves_an_already_human_hint_alone() {
        // Several VPN plugins hint with prose ("Gateway Password"); rewriting
        // those would be worse than passing them through.
        assert_eq!(field_label("Gateway Password"), "Gateway Password");
    }

    #[test]
    fn field_label_handles_an_empty_key() {
        assert_eq!(field_label(""), "");
    }
}
