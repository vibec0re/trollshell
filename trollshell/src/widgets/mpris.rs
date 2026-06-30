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
//! Historically this was a blunt window-count heuristic (hide once the active
//! workspace had ≥2 windows). That was a poor proxy for the real problem: the
//! bar is a `GtkCenterBox`, and the right-anchored `end_pair` (which holds
//! this widget + the status cluster) can be **overlapped** by a wide left
//! cluster (the window-title list) because `CenterBox` doesn't shrink children
//! to avoid collision. Window count is a bad signal — two short titles fit
//! fine, one very long title doesn't.
//!
//! Instead we measure actual bar geometry (see [`decide_mode`]):
//!
//! ```text
//!   available    = bar_width − left_cluster_width − right_cluster_width − GAP
//!   full_natural = Σ natural widths of the full-mode chips (prev/play/next/label)
//!   fits  ⇔  full_natural ≤ available  (with hysteresis, see below)
//! ```
//!
//! `available` deliberately excludes *this* widget's own width: it subtracts
//! only the left and right clusters. Crucially it measures each cluster's
//! **natural** width, never its current *allocated* width — `GtkCenterBox`
//! shrinks the start child (the window-button cluster) toward its minimum when
//! the end pair, which holds this widget, is wide, so the left cluster's
//! *allocation* depends on whether mpris is currently full. Reading that
//! allocation would feed our own mode back into the decision and oscillate (the
//! flicker as the full row tries to overlap the window buttons). Natural widths
//! are mode-independent — the window-button labels are width-capped
//! (`window_list`'s `max_width_chars`), so the left natural is bounded — and
//! that independence is what actually keeps the decision from feeding back.
//!
//! `full_natural` is summed from the individual full-mode chips' own
//! `measure()` results, which are independent of the chip's current `visible`
//! flag — so we can compute the would-be full width even while narrow mode
//! currently has those chips hidden.
//!
//! ### Anti-flicker (hysteresis)
//!
//! Switching modes changes the center child's width, which can re-trigger a
//! size-allocate. To avoid oscillating at the boundary we apply hysteresis: we
//! collapse full→narrow as soon as `full_natural` exceeds `available`, but we
//! expand narrow→full only once there is `HYSTERESIS` px of *headroom* beyond
//! `full_natural`. The expand threshold therefore sits strictly above the
//! collapse threshold; a width parked exactly at the boundary stays put.
//! Recomputation is also deferred to an idle callback (never run inside the
//! allocate), so a mode change settles on the next frame rather than recursing
//! within the current layout pass.

use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;

use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::mpris::{self, Player};

use crate::components::mpris_controls::{bind_transport_button, play_pause_icon};

/// Horizontal breathing room (px) kept between the full mpris row and the left
/// window-title cluster, so the two never butt right up against each other.
const GAP: i32 = 12;

/// Headroom (px) required to expand narrow→full, beyond the bare full natural
/// width. Collapsing full→narrow needs no headroom. The asymmetry is the
/// hysteresis band that prevents flicker at the fit boundary.
const HYSTERESIS: i32 = 24;

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

    wire_visibility_and_state(&container, chips, &current_bus);

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

/// Mode bookkeeping shared between the player-state signal and the geometry
/// watchers, so both apply a consistent presentation and so [`decide_mode`]
/// can read the *current* mode for hysteresis.
#[derive(Clone)]
struct ModeState {
    /// `true` ⇒ full row shown, `false` ⇒ narrow (mini) shown.
    full: Rc<Cell<bool>>,
    /// `true` while a player exists (i.e. the widget is not hidden).
    has_player: Rc<Cell<bool>>,
}

impl ModeState {
    fn new() -> Self {
        Self {
            full: Rc::new(Cell::new(true)),
            has_player: Rc::new(Cell::new(false)),
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
                    reevaluate(container, &chips, &mode);
                }
            },
        );
    }

    // Re-evaluate the fit whenever the geometry that feeds the decision
    // changes. The bar window's width changes on monitor resize; the left
    // cluster's width changes as windows open/close or titles change; the
    // container's own allocation changes when either neighbour moves. We watch
    // the `width` property on each — `connect_notify_local("width", …)` fires
    // post-allocate, so reads are settled, and we defer the actual decision to
    // an idle callback so we never toggle visibility from inside a layout pass.
    install_geometry_watchers(container, &chips, &mode);
}

/// Recompute the fit and apply the resulting mode. Cheap and idempotent: if
/// the chosen mode equals the current one, nothing visible changes (so this is
/// safe to call from a width-notify without oscillating).
fn reevaluate(container: &gtk::Box, chips: &Chips, mode: &ModeState) {
    if !mode.has_player.get() {
        return;
    }
    let want_full = decide_mode(container, chips, mode.full.get());
    mode.full.set(want_full);
    apply_mode(chips, want_full);
}

/// Decide whether the full row fits, given the current mode (for hysteresis).
///
/// Returns `true` for full mode, `false` for narrow.
///
/// `available` is the bar width minus the left and right clusters and a small
/// gap; `full_natural` is the summed natural width of the full-mode chips.
/// Neither quantity depends on the mpris widget's *current* mode, so the
/// decision can't feed back into itself.
fn decide_mode(container: &gtk::Box, chips: &Chips, currently_full: bool) -> bool {
    let Some(available) = available_width(container) else {
        // Geometry not realised yet (no root / zero allocation). Keep the
        // current mode rather than guess; a later width-notify settles it.
        return currently_full;
    };
    let full_natural = full_mode_natural_width(chips);

    if currently_full {
        // Collapse to narrow only once the full row no longer fits.
        full_natural <= available
    } else {
        // Expand back to full only with headroom beyond the bare fit, so a
        // width parked at the boundary doesn't ping-pong.
        full_natural + HYSTERESIS <= available
    }
}

/// Horizontal space (px) the mpris widget may occupy: bar width minus the left
/// and right clusters minus a small gap. `None` if the bar geometry isn't
/// realised yet.
///
/// The clusters are measured by their **natural** width, never their current
/// *allocated* width: `GtkCenterBox` squeezes the start (window-button) cluster
/// toward its minimum when the end pair — which holds this widget — is wide, so
/// the left cluster's allocation depends on whether mpris is full. Reading it
/// would feed our own mode back into the decision and flicker. The `bar_width`
/// is read from the `CenterBox` allocation because that tracks the monitor, not
/// our mode.
fn available_width(container: &gtk::Box) -> Option<i32> {
    // Parent chain: container → middle(Box) → end_pair(Box) → CenterBox → win.
    let middle = container.parent()?;
    let end_pair = middle.parent()?;
    let center_box = end_pair.parent()?.downcast::<gtk::CenterBox>().ok()?;

    let bar_width = center_box.width();
    if bar_width <= 0 {
        return None;
    }

    // Left cluster = the CenterBox start widget (window-title list etc.).
    let left_width = center_box.start_widget().map_or(0, |w| natural_width(&w));

    // Right cluster = the sibling of `middle` inside `end_pair` (the status
    // groups). Its width is independent of our mode.
    let right_width = end_pair
        .last_child()
        .filter(|w| *w != middle)
        .map_or(0, |w| natural_width(&w));

    Some((bar_width - left_width - right_width - GAP).max(0))
}

/// A widget's natural width — mode-independent, unlike its allocated width
/// (which `GtkCenterBox` perturbs in response to the mpris mode; see
/// [`available_width`]). Used for the neighbour clusters so the fit decision
/// never reads an allocation our own toggle feeds back into.
fn natural_width(w: &gtk::Widget) -> i32 {
    w.measure(gtk::Orientation::Horizontal, -1).1
}

/// Summed natural width of the full-mode chips (prev/play/next/label),
/// independent of their current `visible` flag — so we can compute the
/// would-be full width even while narrow mode has them hidden.
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

/// Watch the geometry inputs and re-evaluate the fit when any changes. We hook
/// the `width` notify on the bar window, the left cluster, and the container
/// itself; the decision is deferred to idle so we never mutate visibility
/// mid-allocate.
fn install_geometry_watchers(container: &gtk::Box, chips: &Rc<Chips>, mode: &ModeState) {
    // A coalescing idle-scheduled re-evaluation: many width notifies can fire
    // in one frame; collapse them to a single deferred decision.
    let pending = Rc::new(Cell::new(false));
    let schedule = {
        let container = container.clone();
        let chips = chips.clone();
        let mode = mode.clone();
        let pending = pending.clone();
        move || {
            if pending.replace(true) {
                return;
            }
            let container = container.clone();
            let chips = chips.clone();
            let mode = mode.clone();
            let pending = pending.clone();
            glib::idle_add_local_once(move || {
                pending.set(false);
                reevaluate(&container, &chips, &mode);
            });
        }
    };

    // Container's own width (changes when either neighbour moves).
    {
        let schedule = schedule.clone();
        container.connect_notify_local(Some("width"), move |_, _| schedule());
    }

    // Defer the bar-window / left-cluster hooks until the widget is realised
    // and its parent chain (and the bar root) exists. `connect_realize` fires
    // once the widget joins a mapped window.
    let schedule_for_realize = schedule;
    container.connect_realize(move |container| {
        watch_neighbours(container, &schedule_for_realize);
        // Realisation itself is a good moment to settle the initial mode.
        schedule_for_realize();
    });
}

/// On realise, hook the bar window and left cluster widths. Done here (not at
/// build time) because the parent chain only exists once the widget is added
/// to a realised bar.
fn watch_neighbours(container: &gtk::Box, schedule: &(impl Fn() + Clone + 'static)) {
    // The bar window's default size changes on monitor resize / exclusive-zone
    // changes; hook its `default-width`. (The container's own `width` notify is
    // the primary watcher — this just catches resizes that don't immediately
    // re-allocate the center child.) Downcast to `Window` so we only subscribe
    // to a property we know exists.
    if let Some(win) = container.root().and_downcast::<gtk::Window>() {
        let schedule = schedule.clone();
        win.connect_notify_local(Some("default-width"), move |_, _| schedule());
    }

    // Left cluster: its width grows/shrinks as windows open/close. Reach it
    // via the CenterBox start widget.
    if let Some(middle) = container.parent()
        && let Some(end_pair) = middle.parent()
        && let Some(center_box) = end_pair.parent().and_downcast::<gtk::CenterBox>()
        && let Some(left) = center_box.start_widget()
    {
        let schedule = schedule.clone();
        left.connect_notify_local(Some("width"), move |_, _| schedule());
    }
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
