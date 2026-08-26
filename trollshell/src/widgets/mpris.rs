//! Center-of-bar MPRIS media controls widget.
//!
//! Shows prev / play-pause / next icon buttons and an "artist – title" label.
//! Hidden when no player is active. Buttons disable when the player's
//! corresponding `can_*` flag is false. The play/pause button icon toggles
//! based on the player's playback status.
//!
//! Clicking the label (not the transport buttons) toggles the Media page in
//! the modal panel.
//!
//! ## Three visual states
//!
//! - **No player** — container hidden entirely.
//! - **Player, full row doesn't fit** — *narrow mode*: show only the `mini`
//!   icon button; all transport controls and the title label are hidden.
//!   Clicking `mini` opens the Media panel.
//! - **Player, full row fits** — *full mode*: show transport controls and
//!   title label; `mini` is hidden.
//!
//! ## How "fits" is decided (the narrow-vs-full trigger)
//!
//! The widget is told how much room it may take and renders to it. It measures
//! **itself** and nothing else:
//!
//! ```text
//!   budget       = components::center_budget::signal(monitor)   ← flows in
//!   full_natural = Σ natural widths of the full-mode chips (prev/play/next/label)
//!   full  ⇔  full_natural ≤ budget      (+ EXPAND_HEADROOM when expanding)
//! ```
//!
//! Playback status is deliberately **not** an input: expand and collapse are
//! purely about whether there is room on the bar (Annika's standing correction
//! on #838). Nor is anything about the widget's neighbours, its parent chain, or
//! the bar window — all of that moved to `components::center_budget`, which owns
//! every geometry observation and publishes one damped number.
//!
//! ### Why this is not the old feedback loop (#838, three iterations)
//!
//! The pre-#838 widget measured its neighbours from inside its own layout
//! reaction, which is a loop, and the two failures were the two ways a loop can
//! fail: it froze (the watchers hooked allocated widths that stop moving once
//! `CenterBox` has squeezed the start child), then — once #842 fed it live niri
//! triggers — it blinked, because those triggers fire on every window *title*
//! change and title noise moved the measurement across the fit boundary several
//! times a second. Both were fixed by deleting the loop, not by adding a third
//! guard on top of it.
//!
//! What is left here is a pure function of two inputs, and **neither input can
//! be moved by this widget's own output**:
//!
//! - `budget` is computed from the bar allocation and the neighbour clusters'
//!   *natural* widths, none of which depend on what the centre slot renders, and
//!   it is damped at the source so sub-threshold jitter never arrives.
//! - `full_natural` is a self-measurement, and self-measurement is not feedback.
//!   It is summed from the individual chips' own `measure()` results, which are
//!   **independent of the chip's current `visible` flag** — `gtk_widget_measure`
//!   reports a widget's own size request whether or not its parent is currently
//!   laying it out. That invariant is what lets us compute the would-be full
//!   width while narrow mode has those chips hidden, and it is load-bearing: if
//!   it ever stopped holding, collapsing would shrink `full_natural`, which
//!   *would* be a loop. Keep it in mind before making the chips measure through
//!   their container.
//!
//! With both inputs mode-independent, applying the rule twice with the same
//! inputs gives the same answer, so the mode cannot ping-pong on its own.
//!
//! ### Expand headroom
//!
//! [`EXPAND_HEADROOM`] (the same single `center_budget::JITTER_PX` tunable, in
//! its second documented role) keeps the collapse and expand thresholds from
//! coinciding: we collapse as soon as the row no longer fits, but expand only
//! with that much room to spare.
//!
//! The source damping makes this unnecessary for *budget* movement — that noise
//! is already gone before it arrives. It is kept for the one input damping does
//! not cover: `full_natural` moves with the **title text** (the label is capped
//! at `max_width_chars`, but within the cap it tracks the string). A title whose
//! full row lands within a pixel of the budget would otherwise flip the mode on
//! every track change. That is one flip per genuine input change, not the #838
//! self-sustaining blink — but it is still visible churn for no gain, and the
//! cost of suppressing it is being narrow when up to `EXPAND_HEADROOM` px of
//! unused room was available.

use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;

use hytte::gtk::{self, gdk, prelude::*};
use hytte::prelude::*;
use hytte::services::mpris::{self, Player};

use crate::components::center_budget;
use crate::components::mpris_controls::{bind_transport_button, play_pause_icon};

/// Headroom (px) required to expand narrow→full, beyond the bare full natural
/// width. Collapsing full→narrow needs no headroom; the asymmetry is what keeps
/// a row parked at the boundary from flipping on every title change.
///
/// Deliberately the *same* number as the budget's republish threshold rather
/// than a second knob: both encode "widths this close are not meaningfully
/// different", one applied to the neighbours' widths and one to our own. See the
/// "Expand headroom" section above for why this survives the source damping.
const EXPAND_HEADROOM: i32 = center_budget::JITTER_PX;

/// All child widgets of the MPRIS container, bundled so they can be passed
/// around without hitting the `too_many_arguments` clippy limit.
struct Chips {
    prev_btn: gtk::Button,
    play_pause_btn: gtk::Button,
    next_btn: gtk::Button,
    label: gtk::Label,
    mini: gtk::Button,
}

/// Build the MPRIS center-cluster widget.
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    container.add_css_class("ts-mpris");

    let chips = Chips {
        prev_btn: icon_button("media-skip-backward-symbolic"),
        play_pause_btn: icon_button("media-playback-start-symbolic"),
        next_btn: icon_button("media-skip-forward-symbolic"),
        label: build_clickable_label(monitor),
        mini: build_mini_button(monitor),
    };

    container.append(&chips.prev_btn);
    container.append(&chips.play_pause_btn);
    container.append(&chips.next_btn);
    container.append(&chips.label);
    container.append(&chips.mini);

    let current_bus: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    bind_transport_button(&chips.prev_btn, &current_bus, mpris::previous);
    bind_transport_button(&chips.play_pause_btn, &current_bus, mpris::play_pause);
    bind_transport_button(&chips.next_btn, &current_bus, mpris::next);

    wire_visibility_and_state(monitor, &container, chips, &current_bus);

    container.set_visible(false);
    container.upcast()
}

fn icon_button(icon: &str) -> gtk::Button {
    let btn = gtk::Button::new();
    btn.set_child(Some(&gtk::Image::from_icon_name(icon)));
    btn
}

/// A compact single-icon button shown in *narrow* mode (no room for the full
/// row). Clicking it opens the Media panel — same destination as the
/// full-mode label.
fn build_mini_button(monitor: &Monitor) -> gtk::Button {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-mpris-mini");
    btn.set_child(Some(&gtk::Image::from_icon_name(
        "audio-x-generic-symbolic",
    )));
    let monitor = monitor.clone();
    btn.connect_clicked(move |btn| {
        crate::modal::toggle(&monitor, crate::modal::Page::Media, btn);
    });
    btn
}

fn build_clickable_label(monitor: &Monitor) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(60);

    let gesture = gtk::GestureClick::new();
    gesture.set_button(gdk::BUTTON_PRIMARY);
    let monitor = monitor.clone();
    let label_for_anchor = label.clone();
    gesture.connect_pressed(move |gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        crate::modal::toggle(&monitor, crate::modal::Page::Media, &label_for_anchor);
    });
    label.add_controller(gesture);
    label
}

/// Mode bookkeeping shared between the two subscriptions that can change the
/// presentation (the player signal and the budget signal), so each has the
/// other's latest input and the current mode when it re-decides.
#[derive(Clone)]
struct ModeState {
    /// `true` ⇒ full row shown, `false` ⇒ narrow (mini) shown.
    full: Rc<Cell<bool>>,
    /// `true` while a player exists (i.e. the widget is not hidden).
    has_player: Rc<Cell<bool>>,
    /// Latest centre-slot budget from `components::center_budget`, or `None`
    /// while the bar geometry is unrealised. Cached rather than re-read because
    /// the player signal re-decides too, and it has no budget of its own.
    budget: Rc<Cell<Option<i32>>>,
}

impl ModeState {
    fn new() -> Self {
        Self {
            full: Rc::new(Cell::new(true)),
            has_player: Rc::new(Cell::new(false)),
            budget: Rc::new(Cell::new(None)),
        }
    }
}

/// Drives the three-state MPRIS presentation:
///
/// - **No player** → container hidden; `current_bus` cleared.
/// - **Player, full row doesn't fit** (narrow) → container visible; only
///   `mini` shown; transport controls and label hidden. `current_bus` and
///   player state are still kept current so the full-mode controls work
///   immediately when there is room again.
/// - **Player, full row fits** (full) → container visible; transport controls
///   and label shown; `mini` hidden.
fn wire_visibility_and_state(
    monitor: &Monitor,
    container: &gtk::Box,
    chips: Chips,
    current_bus: &Rc<RefCell<Option<String>>>,
) {
    let chips = Rc::new(chips);
    let mode = ModeState::new();
    let current_bus = current_bus.clone();

    // Re-evaluate the fit on every player change and apply the chosen mode.
    {
        let chips = chips.clone();
        let mode = mode.clone();
        let current_bus = current_bus.clone();
        bind(
            mpris::active_player(),
            container,
            move |container, maybe_player: Option<Player>| match maybe_player {
                None => {
                    mode.has_player.set(false);
                    container.set_visible(false);
                    *current_bus.borrow_mut() = None;
                }
                Some(player) => {
                    mode.has_player.set(true);
                    *current_bus.borrow_mut() = Some(player.bus_name.clone());
                    apply_player_to_widgets(&player, &chips);
                    container.set_visible(true);
                    // The label text just changed, which is the one input to
                    // `full_mode_natural_width` that moves at runtime.
                    reevaluate(&chips, &mode);
                }
            },
        );
    }

    // The hint: "this is the space you can have, max" (#838). Every geometry
    // observation that used to live in this widget — bar width, the neighbour
    // clusters, the sidebar toggle, the niri window/workspace triggers — now
    // lives in `components::center_budget`, which damps the result before it
    // gets here. We just re-render to whatever number arrives.
    {
        let chips = chips.clone();
        let mode = mode.clone();
        bind(
            center_budget::signal(monitor),
            container,
            move |_container, budget: Option<i32>| {
                mode.budget.set(budget);
                reevaluate(&chips, &mode);
            },
        );
    }
}

/// Recompute the mode and apply it. Cheap and idempotent: if the chosen mode
/// equals the current one, nothing visible changes.
fn reevaluate(chips: &Chips, mode: &ModeState) {
    if !mode.has_player.get() {
        return;
    }
    let full = decide_full(
        mode.budget.get(),
        full_mode_natural_width(chips),
        mode.full.get(),
    );
    mode.full.set(full);
    apply_mode(chips, full);
}

/// The pure mode decision, split out from the one GTK read it needs so the
/// threshold asymmetry and the not-yet-realised case are unit-testable without
/// a live widget tree.
///
/// `budget` is the centre slot's max width in px, or `None` while the bar
/// geometry isn't realised yet; `full_natural` is the summed natural width of
/// the full-mode chips; `currently_full` selects the threshold. Returns `true`
/// for full, `false` for narrow.
///
/// Neither input depends on the mode this returns (see the module doc), and
/// neither depends on playback status — the rule is purely spatial (#838).
fn decide_full(budget: Option<i32>, full_natural: i32, currently_full: bool) -> bool {
    let Some(budget) = budget else {
        // No budget published yet (the bar isn't laid out). Keep the current
        // mode rather than guess; the first real budget settles it.
        return currently_full;
    };
    if currently_full {
        // Collapse to narrow only once the full row no longer fits.
        full_natural <= budget
    } else {
        // Expand back to full only with headroom beyond the bare fit, so a row
        // parked at the boundary doesn't flip on every title change.
        full_natural + EXPAND_HEADROOM <= budget
    }
}

/// Summed natural width of the full-mode chips (prev/play/next/label),
/// independent of their current `visible` flag — so we can compute the
/// would-be full width even while narrow mode has them hidden.
///
/// That independence is an invariant of `gtk_widget_measure`, and it is the
/// reason self-measurement here is not feedback: hiding the chips must not
/// change what they measure, or collapsing would shrink the very number that
/// decided to collapse. See the module doc.
fn full_mode_natural_width(chips: &Chips) -> i32 {
    let nat = |w: &gtk::Widget| w.measure(gtk::Orientation::Horizontal, -1).1;
    nat(chips.prev_btn.upcast_ref())
        + nat(chips.play_pause_btn.upcast_ref())
        + nat(chips.next_btn.upcast_ref())
        + nat(chips.label.upcast_ref())
}

/// Show the chips for the chosen mode. Idempotent.
fn apply_mode(chips: &Chips, full: bool) {
    chips.prev_btn.set_visible(full);
    chips.play_pause_btn.set_visible(full);
    chips.next_btn.set_visible(full);
    chips.label.set_visible(full);
    chips.mini.set_visible(!full);
}

fn apply_player_to_widgets(player: &Player, chips: &Chips) {
    let text = if player.artists.is_empty() {
        player.title.clone()
    } else {
        format!("{} \u{2013} {}", player.artists, player.title)
    };
    chips.label.set_text(&text);
    chips.label.set_tooltip_text(Some(&text));

    chips.prev_btn.set_sensitive(player.can_go_previous);
    chips.play_pause_btn.set_sensitive(player.can_play_pause);
    chips.next_btn.set_sensitive(player.can_go_next);

    let icon_name = play_pause_icon(player.status);
    if let Some(img) = chips.play_pause_btn.child().and_downcast::<gtk::Image>() {
        img.set_icon_name(Some(icon_name));
    }
}

#[cfg(test)]
mod tests {
    use super::{EXPAND_HEADROOM, decide_full};

    /// The published-budget case — the one the live bar spends all its time in;
    /// the budget is only `None` before the bar's first allocation.
    fn fits(budget: i32, full_natural: i32, currently_full: bool) -> bool {
        decide_full(Some(budget), full_natural, currently_full)
    }

    #[test]
    fn full_holds_until_the_row_no_longer_fits() {
        // In full mode we collapse only once the row exceeds the budget.
        assert!(fits(100, 100, true), "an exact fit stays full");
        assert!(fits(100, 99, true), "room to spare stays full");
        assert!(!fits(100, 101, true), "one px over collapses to narrow");
    }

    #[test]
    fn narrow_expands_only_with_headroom() {
        // From narrow we need EXPAND_HEADROOM px of room beyond the bare fit.
        assert!(!fits(100, 100, false), "a bare fit is not enough to expand");
        assert!(
            !fits(100, 100 - EXPAND_HEADROOM + 1, false),
            "one px short of the headroom stays narrow"
        );
        assert!(
            fits(100, 100 - EXPAND_HEADROOM, false),
            "exactly EXPAND_HEADROOM of room expands to full"
        );
    }

    #[test]
    fn a_row_at_the_boundary_does_not_ping_pong() {
        // With budget == full_natural the asymmetric thresholds keep the current
        // mode: full stays full, narrow stays narrow. The budget no longer moves
        // in response to this answer (see the module doc), so this is
        // belt-and-braces against title-driven `full_natural` movement, not
        // against a feedback loop.
        assert!(fits(200, 200, true), "full holds at the boundary");
        assert!(
            !fits(200, 200, false),
            "narrow does not expand at the boundary"
        );
    }

    #[test]
    fn the_rule_is_spatial_at_both_edges() {
        // #838: with room the row expands, without room it collapses, and
        // nothing else is consulted — the decision has no other input to
        // consult. Playback status is not a parameter of this function.
        assert!(
            fits(10_000, 100, false),
            "a generous budget expands a narrow chip"
        );
        assert!(
            !fits(100, 10_000, true),
            "a tight budget collapses a full row"
        );
    }

    #[test]
    fn applying_the_rule_twice_is_a_fixed_point() {
        // The anti-blink property, stated as a test: with the inputs held still
        // the mode settles after one application and stays there. #842's failure
        // was that the inputs did *not* hold still — title noise moved them
        // several times a second — which is why the damping now lives upstream
        // in `components::center_budget` instead of being re-guarded here.
        for (budget, natural) in [(400, 100), (100, 400), (200, 200), (100, 100)] {
            for start in [true, false] {
                let once = fits(budget, natural, start);
                let twice = fits(budget, natural, once);
                assert_eq!(
                    once, twice,
                    "budget={budget} natural={natural} start={start}"
                );
            }
        }
    }

    #[test]
    fn no_budget_yet_keeps_the_current_mode() {
        // `budget == None` means nothing has been published for this bar yet (it
        // isn't laid out). Guessing a width would flicker, so hold the current
        // mode; the first real budget settles it. `center_budget` also never
        // publishes `None` over a real budget, so this state is only ever the
        // initial one.
        assert!(
            decide_full(None, 100, true),
            "full is kept until a budget arrives"
        );
        assert!(
            !decide_full(None, 100, false),
            "narrow is kept until a budget arrives"
        );
    }
}
