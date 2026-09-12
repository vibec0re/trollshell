//! Layer-shell **consent prompt** overlay (#487 phase 1b).
//!
//! Unlike the other overlays in this family, this one isn't driven by a service
//! signal — it is raised on demand by the plugin effect broker
//! ([`crate::plugins`]) when a plugin emits `Effect::RequestConsent` (the
//! motivating consumer is the `infobroker` data broker asking a human to approve
//! a local AI agent's data request). [`request`] shows a centered, focus-grabbing
//! card on niri's focused output — *"⟨agent⟩ wants: ⟨scope⟩ from ⟨datasource⟩"* —
//! and routes the choice back to the requesting plugin as
//! `HostMsg::ConsentDecision` over its outbound channel.
//!
//! # Two cards, and the difference is what silence means (#947 P3)
//!
//! The effect carries a [`ConsentChoices`], and this module is the only place
//! that turns it into widgets:
//!
//! | [`ConsentChoices`] | buttons | 60 s / `Esc` | keyboard default |
//! | --- | --- | --- | --- |
//! | `Grant` (default) | Allow once / This session / Always / Deny | [`ConsentDecision::Deny`] | `Always` |
//! | `Approval` | **Approve / Deny** | **nothing is sent** | none |
//!
//! `Grant` is #487 unchanged, down to the byte: `GRANT_CARD` is the same four
//! labels in the same order with the same style classes, and `golden_card_*`
//! pins that.
//!
//! `Approval` (spec §6.5) is for a one-shot decision on a queued item — a
//! hyperhive approval — where "This session" and "Always" would name a standing
//! grant the requester cannot honour. Its silence rule is the load-bearing half:
//! an unanswered prompt sends **no** decision, because a `Deny` there resolves
//! the item durably on the far side and "nobody was at the screen" is not
//! evidence for that. The requester keeps the item pending and re-raises on
//! demand. See [`ConsentChoices::unanswered`], which is where that rule lives so
//! both ends read it from one place.
//!
//! For the same reason the `Approval` card focuses **no** button: the surface
//! takes the keyboard exclusively, so a stray `Return` must not be able to
//! answer it either way. `Esc` still dismisses (sending nothing), which is the
//! deliberate no-op the card wants a key for.
//!
//! **Bounded (60 s).** Every prompt tears down after [`PROMPT_TIMEOUT`], so a
//! wedged UI never leaves a card on screen; whether that teardown *sends* a
//! decision is the table above. Every prompt resolves **exactly once** — the
//! first of a button click, `Esc`, or the timeout wins and cancels the rest.
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
use hytte_plugin_proto::{ConsentChoices, ConsentDecision, HostMsg};
use tokio::sync::mpsc;

/// How long a prompt stays up unanswered before it tears itself down (#487).
/// Matches the proto's documented 60 s bound — the broker holds its own,
/// slightly longer, fallback so a live shell's decision always wins the race.
///
/// What that teardown *sends* is [`ConsentChoices::unanswered`]'s call, not
/// this constant's: `Deny` for a grant, nothing at all for an approval.
const PROMPT_TIMEOUT: Duration = Duration::from_mins(1);

/// One button on a consent card: the label the human reads, the decision a
/// click sends, and the style class that colours it.
///
/// A table rather than four hand-built widgets, because the *card* is the thing
/// two choice sets disagree about and a table is the thing a test can compare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Choice {
    /// The button's label.
    pub(crate) label: &'static str,
    /// What a click on it routes back to the requesting plugin.
    pub(crate) decision: ConsentDecision,
    /// A libadwaita style class, or `""` for a plain button.
    pub(crate) class: &'static str,
}

/// #487's four-button grant card, in its original order and styling.
///
/// Order is left-to-right as appended, and `Deny` leads deliberately: it sits
/// furthest from the pointer's resting place on the affirmative end.
const GRANT_CARD: &[Choice] = &[
    Choice {
        label: "Deny",
        decision: ConsentDecision::Deny,
        class: "destructive-action",
    },
    Choice {
        label: "Allow once",
        decision: ConsentDecision::AllowOnce,
        class: "",
    },
    Choice {
        label: "This session",
        decision: ConsentDecision::AllowSession,
        class: "",
    },
    Choice {
        label: "Always",
        decision: ConsentDecision::AllowAlways,
        class: "suggested-action",
    },
];

/// The #947 P3 two-button card: **Approve / Deny**, same rhythm as the grant
/// card (destructive first, affirmative last, one suggested action).
///
/// `Approve` sends [`ConsentDecision::AllowOnce`] — the one variant that means
/// "this request and no future one", which is exactly what spec §6.5 wants and
/// the only affirmative that does not imply a standing grant the requester
/// would have to invent a policy for. A requester that maps *every* `Allow*` to
/// its one-shot action (as the agents plugin does) therefore reads the same
/// answer from either card.
const APPROVAL_CARD: &[Choice] = &[
    Choice {
        label: "Deny",
        decision: ConsentDecision::Deny,
        class: "destructive-action",
    },
    Choice {
        label: "Approve",
        decision: ConsentDecision::AllowOnce,
        class: "suggested-action",
    },
];

/// The buttons a choice set draws, left to right.
pub(crate) const fn card(choices: ConsentChoices) -> &'static [Choice] {
    match choices {
        ConsentChoices::Grant => GRANT_CARD,
        ConsentChoices::Approval => APPROVAL_CARD,
    }
}

/// Which button (as an index into [`card`]) takes the keyboard focus when the
/// prompt appears, if any.
///
/// `Grant` focuses the **least-destructive** choice — #487's rule, so a
/// keyboard user cannot fall onto `Deny`. `Approval` focuses **nothing**: both
/// of its buttons are consequential (one merges a config change, the other
/// resolves the request negatively), and the surface holds the keyboard
/// exclusively, so any default would let a stray `Return` from another window
/// answer it. `Esc` remains the keyboard's only reach into that card, and it
/// sends nothing.
pub(crate) const fn keyboard_default(choices: ConsentChoices) -> Option<usize> {
    match choices {
        // The last entry, `Always` — see `GRANT_CARD`.
        ConsentChoices::Grant => Some(GRANT_CARD.len() - 1),
        ConsentChoices::Approval => None,
    }
}

/// The card's bold first line.
pub(crate) const fn title_for(choices: ConsentChoices) -> &'static str {
    match choices {
        ConsentChoices::Grant => "Consent request",
        ConsentChoices::Approval => "Approval request",
    }
}

/// The primary ask, computed by the plugin and assembled here.
///
/// `"⟨agent⟩ wants: ⟨scope⟩ from ⟨datasource⟩"` is #487's sentence and stays
/// exactly that whenever a datasource is named. A requester with no datasource
/// to name — an approval is about an item, not a data source — leaves the field
/// empty and gets the sentence without its trailing clause, rather than a
/// dangling `from `.
pub(crate) fn ask_line(agent: &str, datasource: &str, scope: &str) -> String {
    if datasource.is_empty() {
        format!("{agent} wants: {scope}")
    } else {
        format!("{agent} wants: {scope} from {datasource}")
    }
}

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
    // Tail-expression `insert` + an outer `drop`, for uniformity with the other
    // three `install` sites in this sweep (#643). **The weakest one:** the
    // displaced value is a `Monitor`, whose drop is a `GdkMonitor` refcount
    // decrement — that emits nothing, so unlike `sidebar`/`frame`/`osd` there is
    // no plausible re-entrant path even in principle. Converted anyway so the
    // shape is consistent and nobody has to re-derive which of the four were
    // "the real ones"; the cost is one `drop(…)`.
    drop(MONITORS.with(|m| m.borrow_mut().insert(connector, monitor.clone())));
}

/// Close any live prompt and forget the mounted monitors before a hot-plug
/// rebuild — only the per-monitor map and the window are torn down (the
/// focused-output tracker is the host's), so the re-install re-keys cleanly,
/// mirroring `overlays::osd::close_all`.
pub fn close_all() {
    // Bind the taken window before acting on it: the `if let` scrutinee's
    // `RefMut` temporary stays alive for the whole then-block (Rust 2024
    // only changed when it drops relative to an `else` branch, not this),
    // so a GTK call made directly inside the `if let` would hold the borrow
    // across it (#631) — a latent reentrancy hazard if `close()` ever
    // emits synchronously.
    let taken = CONSENT_WINDOW.with(|w| w.borrow_mut().take());
    if let Some(window) = taken {
        window.close();
    }
    MONITORS.with(|m| m.borrow_mut().clear());
}

/// Raise a consent prompt on the focused output and route the human's choice back
/// to the requesting plugin over `outbound` as
/// [`HostMsg::ConsentDecision`](hytte_plugin_proto::HostMsg::ConsentDecision),
/// keyed by `request_id` (#487 phase 1b). GTK-main-thread only (the effect broker
/// runs there).
///
/// `choices` picks the card — see the module docs' table. If no output is
/// mounted there is nowhere to ask, and the fallback follows the same rule the
/// timeout does ([`ConsentChoices::unanswered`]): a grant is denied immediately
/// so the agent is never left hanging, an approval sends nothing and stays
/// pending on the requester's side.
// One cohesive overlay-construction function (card + buttons + Esc + the
// bounded-resolve wiring); the length is the widget count, not branching — like
// `osd::build_osd_view`, splitting it would scatter the paired setup for no gain.
#[allow(clippy::too_many_lines)]
pub fn request(
    request_id: u64,
    agent: &str,
    datasource: &str,
    scope: &str,
    detail: &str,
    choices: ConsentChoices,
    outbound: mpsc::Sender<HostMsg>,
) {
    // Supersede any prompt already up (rare — one knock is typically in
    // flight). Bind-then-act, same as `close_all` above (#631): a GTK call
    // inside the `if let` would otherwise hold the scrutinee's `RefMut`
    // across it.
    let superseded = CONSENT_WINDOW.with(|w| w.borrow_mut().take());
    if let Some(window) = superseded {
        window.close();
    }

    let Some(monitor) = focused_monitor() else {
        // No output to prompt on. A grant is denied straight away rather than
        // stranding the agent; an approval sends nothing, because the whole
        // point of `ConsentChoices::Approval` is that a decision nobody made is
        // not a decision (spec §6.5) — the requester's own badge is what
        // surfaces it instead.
        match choices.unanswered() {
            Some(decision) => {
                tracing::warn!(request_id, %agent, "consent prompt: no monitor to show on; denying");
                let _ = outbound.try_send(HostMsg::ConsentDecision {
                    request_id,
                    decision,
                });
            }
            None => {
                tracing::warn!(
                    request_id,
                    %agent,
                    "consent prompt: no monitor to show on; leaving the request unanswered"
                );
            }
        }
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

    let title = gtk::Label::new(Some(title_for(choices)));
    title.add_css_class("ts-consent-title");
    title.set_xalign(0.0);
    root.append(&title);

    // The primary ask, computed by the plugin: "⟨agent⟩ wants: ⟨scope⟩ from ⟨datasource⟩".
    let ask = gtk::Label::new(Some(&ask_line(agent, datasource, scope)));
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
    // A click, Esc, or the timeout all race to resolve; the first wins, closes
    // the window, and cancels the timer. `done` is the guard.
    //
    // `resolve` takes an `Option`: `Some(d)` sends `d` back to the plugin,
    // `None` tears the card down **silently**. Only the second exists because of
    // `ConsentChoices::Approval` — see the module docs — and routing both
    // through one closure is what keeps "exactly once" a single guard rather
    // than two that could both fire.
    let done: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let timeout: Rc<Cell<Option<glib::SourceId>>> = Rc::new(Cell::new(None));

    let resolve = {
        let done = done.clone();
        let timeout = timeout.clone();
        let window = window.clone();
        // Owned: the closure outlives this call (it is held by three GTK
        // handlers), so the borrowed `agent` cannot ride into it.
        let agent = agent.to_owned();
        move |decision: Option<ConsentDecision>| {
            if done.replace(true) {
                return; // already resolved by an earlier click / Esc / timeout
            }
            if let Some(id) = timeout.take() {
                id.remove();
            }
            // Route the answer back to the requesting plugin. A full/closed queue
            // (the plugin is being reaped) just drops it — the broker times out.
            if let Some(decision) = decision {
                let _ = outbound.try_send(HostMsg::ConsentDecision {
                    request_id,
                    decision,
                });
            } else {
                tracing::debug!(
                    request_id,
                    %agent,
                    "consent prompt dismissed unanswered; the requester keeps the request"
                );
            }
            window.close();
        }
    };

    // ── Buttons: whichever card `choices` named ───────────────────────────────
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);

    let widgets: Vec<gtk::Button> = card(choices)
        .iter()
        .map(|choice| {
            let btn = gtk::Button::with_label(choice.label);
            if !choice.class.is_empty() {
                btn.add_css_class(choice.class);
            }
            let resolve = resolve.clone();
            let decision = choice.decision;
            btn.connect_clicked(move |_| resolve(Some(decision)));
            buttons.append(&btn);
            btn
        })
        .collect();
    root.append(&buttons);

    window.set_child(Some(&root));

    // ── Esc → whatever "unanswered" means for this card ───────────────────────
    let key_ctrl = gtk::EventControllerKey::new();
    {
        let resolve = resolve.clone();
        key_ctrl.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                resolve(choices.unanswered());
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    window.add_controller(key_ctrl);

    // ── Bounded: 60 s of silence tears the card down ──────────────────────────
    {
        let resolve = resolve.clone();
        let id = glib::timeout_add_local_once(PROMPT_TIMEOUT, move || {
            resolve(choices.unanswered());
        });
        timeout.set(Some(id));
    }

    window.set_visible(true);
    window.present();
    // Focus the card's keyboard default, or explicitly nothing — see
    // `keyboard_default`. The `set_focus(None)` is not redundant: GTK focuses
    // the first focusable child of a freshly-presented window on its own, which
    // on the approval card would be `Deny`.
    match keyboard_default(choices).and_then(|i| widgets.get(i)) {
        Some(btn) => {
            btn.grab_focus();
        }
        // Disambiguated: `GtkWindowExt` and `RootExt` both carry `set_focus`
        // and both are in the prelude. `GtkWindowExt`'s is the one that also
        // updates the window's own focus bookkeeping.
        None => gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>),
    }

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
/// A card as one line per button, `label|decision|class`, plus the keyboard
/// default and the silence rule — the artifact the goldens compare.
///
/// A rendering of the *table* rather than of the widgets: [`request`] builds
/// the buttons by a straight `map` over [`card`] with no per-variant branch
/// left in it, so the table is the whole of what "which card" means. (The
/// widget half wants a display server, a layer-shell compositor and a mounted
/// `Monitor` to exist at all, which is exactly the wall
/// `tests/overlay_reentrancy.rs` documents.)
#[cfg(test)]
fn render_card(choices: ConsentChoices) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    for choice in card(choices) {
        let _ = writeln!(
            out,
            "{}|{:?}|{}",
            choice.label, choice.decision, choice.class
        );
    }
    let _ = writeln!(
        out,
        "keyboard_default={}",
        keyboard_default(choices).map_or_else(|| "none".to_owned(), |i| i.to_string())
    );
    let _ = writeln!(out, "unanswered={:?}", choices.unanswered());
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{ConsentChoices, ConsentDecision, ask_line, card, keyboard_default, render_card};

    /// #487's card, pinned literally. #947 P3 made the buttons table-driven and
    /// added a second card; this is the assertion that the first one did not
    /// move by so much as a label, an order, a style class or a decision.
    ///
    /// Falsification: swap two labels, drop `destructive-action`, or re-order
    /// the table, and this goes red with a diff naming the row.
    #[test]
    fn golden_card_grant_is_487_unchanged() {
        assert_eq!(
            render_card(ConsentChoices::Grant),
            "\
Deny|Deny|destructive-action
Allow once|AllowOnce|
This session|AllowSession|
Always|AllowAlways|suggested-action
keyboard_default=3
unanswered=Some(Deny)
"
        );
    }

    /// The #947 P3 card. Two buttons, `Approve` last and suggested, no keyboard
    /// default, and — the load-bearing line — **no** decision on silence.
    ///
    /// Falsification: give `ConsentChoices::Approval` a
    /// `Some(ConsentDecision::Deny)` silence rule (the "just deny it like the
    /// other card" regression) and the last line goes red; label it "Allow"
    /// and the second row does.
    #[test]
    fn golden_card_approval_is_two_buttons_and_answers_nothing_on_silence() {
        assert_eq!(
            render_card(ConsentChoices::Approval),
            "\
Deny|Deny|destructive-action
Approve|AllowOnce|suggested-action
keyboard_default=none
unanswered=None
"
        );
    }

    /// Every affirmative on either card is an `Allow*`, and every card offers
    /// exactly one `Deny` — the property a requester's decision mapping rests
    /// on (spec §6.5 maps *any* `Allow*` to one approve).
    #[test]
    fn every_card_has_exactly_one_deny_and_the_rest_are_allows() {
        for choices in [ConsentChoices::Grant, ConsentChoices::Approval] {
            let denies = card(choices)
                .iter()
                .filter(|c| c.decision == ConsentDecision::Deny)
                .count();
            assert_eq!(denies, 1, "{choices:?}");
            assert!(
                card(choices).len() >= 2,
                "{choices:?} must offer a yes and a no"
            );
        }
    }

    /// The approval card's silence rule is the one the keyboard must not be
    /// able to route around, so no button may hold the default focus.
    #[test]
    fn the_approval_card_focuses_no_button() {
        assert_eq!(keyboard_default(ConsentChoices::Approval), None);
        // …and the grant card's default is an `Allow*`, never `Deny` (#487).
        let idx = keyboard_default(ConsentChoices::Grant).expect("a default");
        assert_ne!(
            card(ConsentChoices::Grant)[idx].decision,
            ConsentDecision::Deny
        );
    }

    /// #487's sentence, unchanged whenever a datasource is named.
    #[test]
    fn the_ask_line_keeps_487s_sentence() {
        assert_eq!(
            ask_line("claude", "departures", "next"),
            "claude wants: next from departures"
        );
    }

    /// A requester with no datasource to name gets the sentence without a
    /// dangling `from ` — the approval case, where the ask is about an item.
    #[test]
    fn an_empty_datasource_drops_the_trailing_clause() {
        assert_eq!(
            ask_line("trollshell-choom", "", "merge a reviewed config PR"),
            "trollshell-choom wants: merge a reviewed config PR"
        );
    }
}
