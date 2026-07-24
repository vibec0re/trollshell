//! Layer-shell **consent prompt** overlay (#487 phase 1b).
//!
//! Unlike the other overlays in this family, this one isn't driven by a service
//! signal — it is raised on demand by the plugin effect broker
//! ([`crate::plugins`]) when a plugin emits `Effect::RequestConsent` (the
//! motivating consumer is the `infobroker` data broker asking a human to approve
//! a local AI agent's data request). [`request`] shows a centered, focus-grabbing
//! card on niri's focused output — *"⟨agent⟩ wants: ⟨scope⟩ from ⟨datasource⟩"* —
//! with **Allow once / This session / Always / Deny**, and routes the choice back
//! to the requesting plugin as `HostMsg::ConsentDecision` over its outbound
//! channel.
//!
//! **Bounded (60 s).** An unanswered prompt resolves to
//! [`ConsentDecision::Deny`] after [`PROMPT_TIMEOUT`], so a wedged UI can never
//! leave the requesting agent hanging (the broker independently applies its own
//! longer fallback). Every prompt resolves **exactly once** — the first of a
//! button click, `Esc`, or the timeout wins and tears down the rest.
//!
//! **Focused output.** [`install`] registers each monitor by connector;
//! [`request`] picks the monitor for niri's focused output via the shared
//! [`crate::components::focused_output`] cache (#496/#440/#517), falling back to
//! any mounted one.
//!
//! CSS hooks (`ts-`-prefixed, matching the prompt overlay's shape):
//! - window root: `.ts-consent`
//! - inner card: `.ts-consent-root`
//! - title: `.ts-consent-title`
//! - subtitle (⟨agent⟩ wants …): `.ts-consent-subtitle`
//! - detail line: `.ts-consent-detail`

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use crate::components::focused_output;

use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::ui::{Layer, layer_window};
use hytte_plugin_proto::{ConsentDecision, HostMsg};
use tokio::sync::mpsc;

/// How long a prompt stays up unanswered before it resolves to
/// [`ConsentDecision::Deny`] (#487). Matches the proto's documented 60 s bound —
/// the broker holds its own, slightly longer, fallback so a live shell's decision
/// always wins the race.
const PROMPT_TIMEOUT: Duration = Duration::from_mins(1);

thread_local! {
    /// The single live consent window, if any. A fresh [`request`] replaces it
    /// (the superseded one's requester falls back to the broker's own timeout).
    static CONSENT_WINDOW: RefCell<Option<gtk::Window>> = const { RefCell::new(None) };

    /// Mounted monitors keyed by `Monitor.connector()`, so [`request`] can build
    /// the prompt on niri's focused output. Re-keyed on each hot-plug via
    /// [`close_all`] + [`install`].
    static MONITORS: RefCell<HashMap<String, Monitor>> = RefCell::new(HashMap::new());
}

/// Register `monitor` as a candidate output for consent prompts. Called per
/// monitor from `main.rs`'s `monitors_changed` loop. The focused-output *tracker*
/// lives in the shared [`crate::components::focused_output`] cache (#496/#440/#517),
/// so this only maintains the connector→`Monitor` map [`request`] resolves against.
pub fn install(monitor: &Monitor) {
    let Some(connector) = monitor.connector().filter(|c| !c.is_empty()) else {
        tracing::debug!("consent::install: monitor has no connector name; skipping");
        return;
    };
    MONITORS.with(|m| {
        m.borrow_mut().insert(connector, monitor.clone());
    });
}

/// Close any live prompt and forget the mounted monitors before a hot-plug
/// rebuild — only the per-monitor map and the window are torn down (the
/// focused-output tracker is the host's), so the re-install re-keys cleanly,
/// mirroring `overlays::osd::close_all`.
pub fn close_all() {
    CONSENT_WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().take() {
            window.close();
        }
    });
    MONITORS.with(|m| m.borrow_mut().clear());
}

/// Raise a consent prompt on the focused output and route the human's choice back
/// to the requesting plugin over `outbound` as
/// [`HostMsg::ConsentDecision`](hytte_plugin_proto::HostMsg::ConsentDecision),
/// keyed by `request_id` (#487 phase 1b). GTK-main-thread only (the effect broker
/// runs there). If no output is mounted, the request is denied immediately so the
/// agent is never left hanging.
// One cohesive overlay-construction function (card + four buttons + Esc + the
// bounded-resolve wiring); the length is the widget count, not branching — like
// `osd::build_osd_view`, splitting it would scatter the paired setup for no gain.
#[allow(clippy::too_many_lines)]
pub fn request(
    request_id: u64,
    agent: &str,
    datasource: &str,
    scope: &str,
    detail: &str,
    outbound: mpsc::Sender<HostMsg>,
) {
    // Supersede any prompt already up (rare — one knock is typically in flight).
    CONSENT_WINDOW.with(|w| {
        if let Some(window) = w.borrow_mut().take() {
            window.close();
        }
    });

    let Some(monitor) = focused_monitor() else {
        // No output to prompt on: deny straight away rather than strand the agent.
        tracing::warn!(request_id, %agent, "consent prompt: no monitor to show on; denying");
        let _ = outbound.try_send(HostMsg::ConsentDecision {
            request_id,
            decision: ConsentDecision::Deny,
        });
        return;
    };

    let window = layer_window(&monitor)
        .layer(Layer::Overlay)
        .exclusive(false)
        .keyboard_mode(KeyboardMode::Exclusive)
        .namespace("hytte-consent")
        .build();
    window.add_css_class("ts-consent");
    // Extra room so the card's drop-shadow isn't clipped by the surface edge.
    window.set_size_request(480, 300);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    root.add_css_class("ts-consent-root");
    root.set_margin_start(18);
    root.set_margin_end(18);
    root.set_margin_top(18);
    root.set_margin_bottom(18);

    let title = gtk::Label::new(Some("Consent request"));
    title.add_css_class("ts-consent-title");
    title.set_xalign(0.0);
    root.append(&title);

    // The primary ask, computed by the plugin: "⟨agent⟩ wants: ⟨scope⟩ from ⟨datasource⟩".
    let ask = gtk::Label::new(Some(&format!("{agent} wants: {scope} from {datasource}")));
    ask.add_css_class("ts-consent-subtitle");
    ask.set_xalign(0.0);
    ask.set_wrap(true);
    root.append(&ask);

    if !detail.is_empty() {
        let detail_label = gtk::Label::new(Some(detail));
        detail_label.add_css_class("ts-consent-detail");
        detail_label.set_xalign(0.0);
        detail_label.set_wrap(true);
        root.append(&detail_label);
    }

    // ── Resolve-exactly-once machinery ────────────────────────────────────────
    //
    // A click, Esc, or the timeout all race to resolve; the first wins, sends the
    // decision, closes the window, and cancels the timer. `done` is the guard.
    let done: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let timeout: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));

    let resolve = {
        let done = done.clone();
        let timeout = timeout.clone();
        let window = window.clone();
        move |decision: ConsentDecision| {
            if done.replace(true) {
                return; // already resolved by an earlier click / Esc / timeout
            }
            if let Some(id) = timeout.take() {
                id.remove();
            }
            // Route the answer back to the requesting plugin. A full/closed queue
            // (the plugin is being reaped) just drops it — the broker times out.
            let _ = outbound.try_send(HostMsg::ConsentDecision {
                request_id,
                decision,
            });
            window.close();
        }
    };

    // ── Buttons: Allow once / This session / Always / Deny ────────────────────
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);

    let deny_btn = gtk::Button::with_label("Deny");
    deny_btn.add_css_class("destructive-action");
    let once_btn = gtk::Button::with_label("Allow once");
    let session_btn = gtk::Button::with_label("This session");
    let always_btn = gtk::Button::with_label("Always");
    always_btn.add_css_class("suggested-action");

    for (btn, decision) in [
        (&deny_btn, ConsentDecision::Deny),
        (&once_btn, ConsentDecision::AllowOnce),
        (&session_btn, ConsentDecision::AllowSession),
        (&always_btn, ConsentDecision::AllowAlways),
    ] {
        let resolve = resolve.clone();
        btn.connect_clicked(move |_| resolve(decision));
        buttons.append(btn);
    }
    root.append(&buttons);

    window.set_child(Some(&root));

    // ── Esc → Deny ────────────────────────────────────────────────────────────
    let key_ctrl = gtk::EventControllerKey::new();
    {
        let resolve = resolve.clone();
        key_ctrl.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                resolve(ConsentDecision::Deny);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    window.add_controller(key_ctrl);

    // ── Bounded: unanswered → Deny after 60 s ─────────────────────────────────
    {
        let resolve = resolve.clone();
        let id = glib::timeout_add_local_once(PROMPT_TIMEOUT, move || {
            resolve(ConsentDecision::Deny);
        });
        timeout.set(Some(id));
    }

    window.set_visible(true);
    window.present();
    // Focus the least-destructive default so keyboard users don't accidentally
    // Deny; Esc is the explicit cancel.
    always_btn.grab_focus();

    CONSENT_WINDOW.with(|w| *w.borrow_mut() = Some(window));
}

/// The `Monitor` for niri's focused output, or any mounted one as a fallback
/// (niri startup / a just-vanished output), or `None` if none are mounted.
fn focused_monitor() -> Option<Monitor> {
    let focused = focused_output::current();
    MONITORS.with(|m| {
        let m = m.borrow();
        focused
            .as_ref()
            .and_then(|name| m.get(name))
            .or_else(|| m.values().next())
            .cloned()
    })
}
