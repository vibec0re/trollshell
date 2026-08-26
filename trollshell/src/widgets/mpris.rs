//! Bar MPRIS chip: prev / play-pause / next plus an "artist – title" label
//! when the bar has room for them, a single mini icon button when it does not,
//! and nothing at all when no player is active. Both renditions open the Media
//! page on click — the label in the full row, the button in the mini one.
//!
//! ## Why this is an `AdwSqueezer` (#838)
//!
//! Four iterations tried to decide full-vs-mini in *our* code, and each failed
//! the same way: the widget measured its neighbours through its parent chain
//! and froze (its watchers hooked `notify::width`, a property `GtkWidget` does
//! not have, so none of them ever fired); then it was given live niri triggers
//! and blinked instead (window *titles* tick several times a second, and the
//! window list's natural width tracks them); then the whole measurement moved
//! into a bar-side `center_budget` helper that damped the noise and published
//! one "this is the space you can have, max" number — which still read the
//! bar's width before the compositor's configure had landed, so it was wrong
//! for exactly as long as a sidebar toggle took to round-trip. All three are
//! one bug: our code was reconstructing, from outside the layout pass, a
//! number that only exists *inside* it.
//!
//! [`adw::Squeezer`] is GTK's answer to that — a container query. It holds both
//! renditions as children and the **layout system** shows the first one that
//! fits the allocation it was actually given, decided in the allocation pass
//! where the true available width lives. So there is nothing here to watch, to
//! damp, or to keep in sync: no trigger can be forgotten (iterations 1 and 2)
//! and no reading can be stale (iteration 4), because there are no readings.
//!
//! Two properties make the blink structurally impossible rather than merely
//! guarded, and both are GTK's to enforce, not ours to remember:
//!
//! - A squeezer's own size *request* is the same whichever child it shows —
//!   minimum is the smallest child's minimum, natural the largest child's
//!   natural — so swapping children cannot re-trigger the layout pass that
//!   chose them. That is the feedback loop from iterations 1–3, gone by
//!   construction. Its price is that a collapsed chip still *requests* the full
//!   row's natural width: the space it gives up is not handed back to the
//!   window list. Handing it back is precisely what would close the loop again,
//!   so the reserved slot is the design, not an oversight — see
//!   [`build_squeezer`] for how the visible child is aligned inside it.
//! - In the crowded regime the window list sits at its minimum, which is stable
//!   against title noise, so the squeezer's slice is stable too. Only a narrow
//!   contested band can flip the child, and [`TRANSITION_MS`] of crossfade makes
//!   that rare flip read as intentional.
//!
//! Playback status is not an input to any of this: expand and collapse are
//! purely about whether there is room on the bar (Annika's standing correction
//! on #838). The container's visibility follows `mpris::active_player()` and
//! nothing else.

use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self, gdk};
use hytte::prelude::*;
use hytte::services::mpris::{self, Player};

use crate::components::mpris_controls::{bind_transport_button, play_pause_icon};

/// Crossfade duration (ms) between the full row and the mini chip.
///
/// The squeezer re-decides on every allocation, and in the crowded regime the
/// inputs hold still, so a flip is rare. Long enough that when one does happen
/// it reads as a deliberate reflow rather than a glitch; short enough not to
/// lag a sidebar toggle.
const TRANSITION_MS: u32 = 150;

/// The full row's player-driven children, bundled so they can be passed around
/// without hitting the `too_many_arguments` clippy limit.
struct Chips {
    prev_btn: gtk::Button,
    play_pause_btn: gtk::Button,
    next_btn: gtk::Button,
    label: gtk::Label,
}

/// Build the MPRIS bar chip.
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let chips = Chips {
        prev_btn: icon_button("media-skip-backward-symbolic"),
        play_pause_btn: icon_button("media-playback-start-symbolic"),
        next_btn: icon_button("media-skip-forward-symbolic"),
        label: build_clickable_label(monitor),
    };

    let full_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    full_row.append(&chips.prev_btn);
    full_row.append(&chips.play_pause_btn);
    full_row.append(&chips.next_btn);
    full_row.append(&chips.label);

    let squeezer = build_squeezer(
        full_row.upcast_ref(),
        build_mini_button(monitor).upcast_ref(),
    );
    squeezer.add_css_class("ts-mpris");

    let current_bus: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    bind_transport_button(&chips.prev_btn, &current_bus, mpris::previous);
    bind_transport_button(&chips.play_pause_btn, &current_bus, mpris::play_pause);
    bind_transport_button(&chips.next_btn, &current_bus, mpris::next);

    wire_visibility_and_state(&squeezer, chips, &current_bus);

    squeezer.set_visible(false);
    squeezer.upcast()
}

/// Assemble the squeezer over the two renditions, `full` first (preferred).
///
/// Split out from [`widget`] so the geometry contract this whole rewrite rests
/// on can be asserted against a real widget tree without a service registry —
/// see the `gtk_tests` module at the bottom of this file.
///
/// The configuration, and why each part of it:
///
/// - **`allow_none(false)`** — never show *nothing*. The mini chip is the floor;
///   an empty bar slot with an active player would be a worse answer than a
///   cramped one.
/// - **`homogeneous(false)`** — the squeezer takes the visible child's height
///   rather than the tallest child's. (It has no effect along the squeeze axis,
///   where the request is always min-of-mins / max-of-naturals; that is the
///   anti-feedback property the module doc describes.)
/// - **`switch_threshold_policy(Natural)`** — switch as soon as the visible
///   child cannot get its *natural* size, not merely its minimum. The label
///   ellipsizes, so its minimum is about one ellipsis wide; under the `Minimum`
///   policy the full row would "fit" almost any allocation and render three
///   buttons next to a bare "…", and the mini chip would be unreachable in
///   practice. `Natural` reproduces the rule the previous four iterations were
///   all trying to implement — full row only if the whole row genuinely fits —
///   with GTK doing the measuring. Flip this to `Minimum` if an ellipsized
///   title is ever preferred over the chip; it is a one-line change.
/// - **`halign(End)` on both children** — the squeezer's allocation can exceed
///   the visible child's natural width (see the reserved-slot note in the module
///   doc), and the squeezer allocates its visible child the full width. Without
///   this the mini chip would stretch across the whole reserved slot. End-
///   aligning keeps both renditions snug against the right-hand status cluster
///   and lets the slack fall into the bar's existing mid gap.
///
/// ## DEPRECATION — the one open question on this rewrite
///
/// `AdwSqueezer` is deprecated as of **libadwaita 1.4** (still shipped and
/// functional in the 1.9.3 we build against), and the `adw` 0.9.1 bindings gate
/// that deprecation on the `v1_4` feature the workspace enables — so every call
/// below is a `-D warnings` failure without the `#[expect(deprecated)]` carried
/// here, on [`wire_visibility_and_state`], and on the two test functions that
/// drive a squeezer. libadwaita's migration guide points at
/// `AdwBreakpointBin` + `AdwBreakpoint`, which is **not** an equivalent here: a
/// breakpoint fires on an explicit `max-width: N px` condition, so it would
/// reintroduce exactly the hand-tuned pixel constant this rewrite exists to
/// delete — and N would have to equal the full row's natural width, which moves
/// with the track title. A squeezer asks "does the preferred child fit?" and
/// needs no constant at all. There is no non-deprecated widget with that
/// semantic, in GTK4 or in libadwaita.
///
/// So the trade is: a deprecated-but-present widget that models the problem
/// exactly, versus a supported one that does not. Keeping the suppression means
/// this chip needs revisiting if libadwaita ever removes the widget (a 2.0
/// event; none is announced).
#[expect(
    deprecated,
    reason = "AdwSqueezer is deprecated since libadwaita 1.4; see the DEPRECATION note above"
)]
fn build_squeezer(full: &gtk::Widget, mini: &gtk::Widget) -> adw::Squeezer {
    full.set_halign(gtk::Align::End);
    mini.set_halign(gtk::Align::End);

    let squeezer = adw::Squeezer::new();
    squeezer.set_homogeneous(false);
    squeezer.set_allow_none(false);
    squeezer.set_switch_threshold_policy(adw::FoldThresholdPolicy::Natural);
    squeezer.set_transition_type(adw::SqueezerTransitionType::Crossfade);
    squeezer.set_transition_duration(TRANSITION_MS);

    // Order is preference order: the squeezer shows the first child that fits.
    squeezer.add(full);
    squeezer.add(mini);
    squeezer
}

fn icon_button(icon: &str) -> gtk::Button {
    let btn = gtk::Button::new();
    btn.set_child(Some(&gtk::Image::from_icon_name(icon)));
    btn
}

/// A compact single-icon button — the rendition shown when the full row does
/// not fit. Clicking it opens the Media panel, the same destination as the
/// full row's label.
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

/// Drive the two things that are still ours to drive: whether the chip is on
/// the bar at all, and what the full row says.
///
/// - **No player** → hidden; `current_bus` cleared.
/// - **Player** → visible; transport sensitivity, play/pause glyph and label
///   text refreshed. Which rendition is shown is not decided here — that is the
///   squeezer's job, every allocation pass.
///
/// The full row's chips stay current even while the mini chip is the visible
/// rendition, so the transport controls work the instant there is room again.
#[expect(
    deprecated,
    reason = "AdwSqueezer is deprecated since libadwaita 1.4; see the DEPRECATION note on \
              build_squeezer"
)]
fn wire_visibility_and_state(
    squeezer: &adw::Squeezer,
    chips: Chips,
    current_bus: &Rc<RefCell<Option<String>>>,
) {
    let chips = Rc::new(chips);
    let current_bus = current_bus.clone();
    bind(
        mpris::active_player(),
        squeezer,
        move |squeezer, maybe_player: Option<Player>| match maybe_player {
            None => {
                squeezer.set_visible(false);
                *current_bus.borrow_mut() = None;
            }
            Some(player) => {
                *current_bus.borrow_mut() = Some(player.bus_name.clone());
                apply_player_to_widgets(&player, &chips);
                squeezer.set_visible(true);
            }
        },
    );
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

/// The squeezer's geometry contract, asserted rather than believed (#838).
///
/// The pure `decide_full` / `fits` unit tests this replaces are gone with the
/// machinery they tested: there is no longer a decision function to test,
/// because the decision is GTK's. What is worth testing instead is that the
/// hand-off to GTK is the one the module doc claims — that the squeezer's
/// minimum really is the mini chip's (so the chip, not the full row, is what
/// sets the bar's floor), and that a genuinely tight allocation really does
/// select the mini child.
///
/// Gated behind `system-tests` because instantiating widgets needs a display
/// (`xvfb-run`).
#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use super::{build_squeezer, icon_button};
    use hytte::adw::{self, prelude::*};
    use hytte::gtk;

    /// Run the GTK main loop until it has nothing left to dispatch, so a
    /// queued resize/allocation actually happens.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// The two renditions, minus everything that needs a live `Monitor` and the
    /// service registry (the click gestures and the player bindings). Geometry
    /// is what is under test, and geometry does not depend on either.
    fn renditions() -> (gtk::Box, gtk::Button) {
        let full = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        full.append(&icon_button("media-skip-backward-symbolic"));
        full.append(&icon_button("media-playback-start-symbolic"));
        full.append(&icon_button("media-skip-forward-symbolic"));
        let label = gtk::Label::new(Some(
            "A Very Long Artist Name \u{2013} An Equally Long Track Title",
        ));
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_max_width_chars(60);
        full.append(&label);

        let mini = gtk::Button::new();
        mini.set_child(Some(&gtk::Image::from_icon_name(
            "audio-x-generic-symbolic",
        )));
        (full, mini)
    }

    fn width_request(w: &impl IsA<gtk::Widget>) -> (i32, i32) {
        let (min, nat, _, _) = w.as_ref().measure(gtk::Orientation::Horizontal, -1);
        (min, nat)
    }

    /// The bar-side contract from #838's item 4: the squeezer's **minimum** is
    /// the mini chip's minimum, not the full row's. That is what lets the bar
    /// squeeze this slot at all — if the floor were the full row, the chip
    /// would be reserving room it may not have and the squeezer could never be
    /// squeezed. Its **natural** stays the full row's, which is the
    /// anti-feedback property: it does not change when the shown child does.
    #[gtk::test]
    fn the_floor_is_the_mini_chip_and_the_ceiling_is_the_full_row() {
        adw::init().expect("libadwaita init");
        let (full, mini) = renditions();
        let (full_min, full_nat) = width_request(&full);
        let (mini_min, _) = width_request(&mini);
        let squeezer = build_squeezer(full.upcast_ref(), mini.upcast_ref());
        let (sq_min, sq_nat) = width_request(&squeezer);

        assert!(
            full_min > mini_min,
            "test setup: the full row must genuinely be the wider rendition \
             (full_min={full_min}, mini_min={mini_min})"
        );
        assert_eq!(
            sq_min, mini_min,
            "the squeezer's minimum must be the mini chip's, so the bar can squeeze this slot"
        );
        assert_eq!(
            sq_nat, full_nat,
            "the squeezer's natural must be the full row's, so it asks for the room the full \
             rendition needs"
        );
    }

    /// The decision itself, made by GTK: give the squeezer an allocation that
    /// cannot hold the full row and it shows the mini chip; give it room and it
    /// shows the full row. This is the assertion the previous four iterations
    /// each tried to make true with watchers, budgets and hysteresis.
    #[gtk::test]
    #[expect(
        deprecated,
        reason = "AdwSqueezer is deprecated since libadwaita 1.4; see the DEPRECATION note on \
                  build_squeezer"
    )]
    fn a_tight_allocation_selects_the_mini_child() {
        adw::init().expect("libadwaita init");
        let (probe_full, probe_mini) = renditions();
        let (mini_min, _) = width_request(&probe_mini);
        let (_, full_nat) = width_request(&probe_full);

        let narrow = visible_child_at(mini_min + 8);
        assert!(
            narrow.is::<gtk::Button>(),
            "an allocation too small for the full row must select the mini chip, got a {}",
            narrow.type_()
        );

        let wide = visible_child_at(full_nat + 200);
        assert!(
            wide.is::<gtk::Box>(),
            "an allocation with room to spare must select the full row, got a {}",
            wide.type_()
        );
    }

    /// Put a fresh squeezer in a fresh window `width` px wide, let GTK allocate
    /// it, and report which rendition GTK chose.
    ///
    /// A window per case on purpose: `set_default_size` only takes effect
    /// before the window is mapped, so resizing one already-presented window
    /// silently measures the first size twice.
    #[expect(
        deprecated,
        reason = "AdwSqueezer is deprecated since libadwaita 1.4; see the DEPRECATION note on \
                  build_squeezer"
    )]
    fn visible_child_at(width: i32) -> gtk::Widget {
        let (full, mini) = renditions();
        let squeezer = build_squeezer(full.upcast_ref(), mini.upcast_ref());
        // No transition, so `visible_child` is settled the moment the
        // allocation is; the crossfade is a presentation detail and would
        // otherwise leave the outgoing child visible mid-animation.
        squeezer.set_transition_type(adw::SqueezerTransitionType::None);

        let window = gtk::Window::new();
        window.set_child(Some(&squeezer));
        window.set_default_size(width, 40);
        window.present();
        pump();

        let chosen = squeezer.visible_child().expect("allow_none(false)");
        window.destroy();
        chosen
    }
}
