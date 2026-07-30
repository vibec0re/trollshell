//! Drawer panel for the active MPRIS player — art + metadata + transport
//! controls + seek bar. Backed by `hytte::services::mpris` which surfaces
//! whichever player is currently top-priority.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use hytte::futures_signals::map_ref;
use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::mpris;

use crate::components::cast;
use crate::components::format::fmt_us;
use crate::components::layout::{finish_page, page_grid, section, toggle_class};
use crate::components::mpris_controls::{bind_transport_button, play_pause_icon};
use crate::scale::scale;

/// Labels + buttons that the bind closure updates on every emission.
#[derive(Clone)]
struct InfoWidgets {
    title: gtk::Label,
    artist: gtk::Label,
    album: gtk::Label,
    prev_btn: gtk::Button,
    play_pause_btn: gtk::Button,
    next_btn: gtk::Button,
    seek: gtk::Scale,
    pos: gtk::Label,
    len: gtk::Label,
}

/// Mutable per-render state shared between the bind closure and the
/// click/seek handlers. `bus` is its own `Rc<RefCell<…>>` so the shared
/// transport-button helper in `components::mpris_controls` can hold a
/// clone without taking the whole `PlayerState`.
#[derive(Default)]
struct PlayerState {
    bus: Rc<RefCell<Option<String>>>,
    track_id: RefCell<Option<String>>,
    length_us: Cell<u64>,
    last_art_url: RefCell<String>,
}

pub fn panel_media() -> gtk::Widget {
    let grid = page_grid();
    grid.set_column_homogeneous(false);

    // Source switcher spans both columns at the top; shown only with >=2
    // players (it wires its own reactive visibility).
    let switcher = build_switcher();
    grid.attach(&switcher, 0, 0, 2, 1);

    let art_image = build_art_panel(&grid);
    let info = build_info_panel();
    grid.attach(&info.section, 1, 1, 1, 1);

    let state = Rc::new(PlayerState::default());
    wire_transport_buttons(&info.widgets, &state);
    wire_player_bind(&info.widgets, &art_image, &state);
    wire_seek(&info.widgets.seek, &state);

    finish_page(&grid)
}

/// Per-render switcher chips: `bus_name` (`None` = the "Auto" chip) paired
/// with its toggle button, so the chip-state bind can restyle without a
/// full rebuild.
type SwitcherChips = Rc<RefCell<Vec<(Option<String>, gtk::ToggleButton)>>>;

/// CSS class carried by the chip whose player the user explicitly *pinned*
/// (via `mpris::select_player`), on top of the pressed state every active chip
/// gets. Lets a deliberate pin read differently from a merely heuristic pick
/// — see `assets/trollshell/style.css`.
const PINNED_CLASS: &str = "ts-media-source-pinned";

/// The two bus names the chip styling is a function of, as last emitted.
///
/// Kept in an `Rc<RefCell<…>>` shared with the roster-rebuild bind so chips
/// built by a rebuild get their state applied straight away: the rebuild and
/// the chip-state bind are independent apply-loops with no ordering guarantee
/// between them, so a rebuild can't rely on the state bind having painted its
/// brand-new buttons.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Selection {
    /// `mpris::selected_player()` — the bus the user pinned, if any.
    selected: Option<String>,
    /// `mpris::active_player()`'s bus — the pinned one when the pin is live,
    /// otherwise whatever the service's heuristic picked.
    active: Option<String>,
}

type SelectionCell = Rc<RefCell<Selection>>;

/// How a single source chip should render for a given [`Selection`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ChipState {
    /// The `gtk::ToggleButton` pressed state.
    pressed: bool,
    /// Whether the chip additionally carries [`PINNED_CLASS`].
    pinned: bool,
}

/// A horizontal row of selectable source chips: one per live MPRIS player
/// plus an "Auto" chip to revert to the heuristic. Rebuilt reactively from
/// `mpris::players()`; the whole row hides unless >=2 players are present.
/// Chip pressed/pinned state comes from [`chip_state`].
fn build_switcher() -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("ts-media-switcher");

    let chips: SwitcherChips = Rc::new(RefCell::new(Vec::new()));
    let selection: SelectionCell = Rc::new(RefCell::new(Selection::default()));

    // Rebuild the chip set only when the ROSTER (each player's bus_name +
    // display label) actually changes. `mpris::players()` re-emits ~4Hz from
    // the position poller, so binding the rebuild straight to it would tear
    // down and recreate every chip 4x/second. Project to the roster + dedupe
    // so a rebuild fires only on add/remove/relabel.
    let chips_for_build = chips.clone();
    let selection_for_build = selection.clone();
    bind(
        mpris::players()
            .map(|players| {
                players
                    .iter()
                    .map(|p| (p.bus_name.clone(), player_label(p)))
                    .collect::<Vec<(String, String)>>()
            })
            .dedupe_cloned(),
        &row,
        move |row, roster| {
            rebuild_switcher(row, &roster, &chips_for_build);
            apply_chip_states(&chips_for_build, &selection_for_build.borrow());
        },
    );

    // Mark the chips. This needs BOTH signals, and until #651 it only had one:
    // an `active_player()`-only bind can never leave "Auto" pressed, because
    // Auto's bus is `None` while the active bus is `Some(…)` whenever any
    // player exists — so every emission cleared Auto and the user watched
    // their own click pop straight back out. `mpris::selected_player()` is the
    // missing half; it is not a new capability, it landed in the very commit
    // that added this panel (0d7d2da) and simply had no subscriber. Combining
    // the two also separates a pinned source from a heuristically-active one,
    // which is the distinction #128 asked for.
    //
    // Deliberately NOT deduped. The chips are `ToggleButton`s, so a click
    // flips the pressed state before the model sees it; `select_player` writes
    // a plain `Mutable::set`, which notifies even when the value is unchanged,
    // and this re-apply is what puts the button back where the model says it
    // belongs. Clicking an already-pressed chip (very easy on "Auto") would
    // otherwise leave it stuck in the flipped state. The apply is a no-op-
    // guarded walk over at most a handful of chips, so re-running it on every
    // `players()` tick costs nothing worth optimising away.
    let combined = map_ref! {
        let selected = mpris::selected_player(),
        let active = mpris::active_player().map(|p| p.map(|p| p.bus_name)) => {
            Selection { selected: selected.clone(), active: active.clone() }
        }
    };
    let chips_for_state = chips.clone();
    let selection_for_state = selection.clone();
    bind(combined, &row, move |_, sel| {
        apply_chip_states(&chips_for_state, &sel);
        *selection_for_state.borrow_mut() = sel;
    });

    row.upcast()
}

/// Push `sel` onto every live chip: pressed state on the toggle, and
/// [`PINNED_CLASS`] on whichever one is actually pinned.
fn apply_chip_states(chips: &SwitcherChips, sel: &Selection) {
    for (bus, btn) in chips.borrow().iter() {
        let state = chip_state(
            sel.selected.as_deref(),
            sel.active.as_deref(),
            bus.as_deref(),
        );
        set_toggle_silently(btn, state.pressed);
        toggle_class(btn, PINNED_CLASS, state.pinned);
    }
}

/// Whether the pin in `selected` is actually in force.
///
/// `mpris::resolve_active` honours a pin only while that player is still in
/// the live roster and otherwise falls back to the heuristic, so the pin is in
/// force exactly when it equals the resolved active bus. A pin left behind by
/// a player that has since quit therefore reads as automatic — which is what
/// the service is genuinely doing.
fn effective_pin<'a>(selected: Option<&'a str>, active: Option<&str>) -> Option<&'a str> {
    match (selected, active) {
        (Some(s), Some(a)) if s == a => Some(s),
        _ => None,
    }
}

/// Pure derivation of one chip's visual state from the current selection.
///
/// `chip` is the chip's own bus name, `None` for the "Auto" chip. Auto is a
/// *mode*, not a source: it presses exactly when no pin is in force, and is
/// never itself "pinned". A player chip presses when it is the active source
/// and takes [`PINNED_CLASS`] only when it is the pinned one.
fn chip_state(selected: Option<&str>, active: Option<&str>, chip: Option<&str>) -> ChipState {
    let pin = effective_pin(selected, active);
    match chip {
        None => ChipState {
            pressed: pin.is_none(),
            pinned: false,
        },
        Some(bus) => ChipState {
            pressed: active == Some(bus),
            pinned: pin == Some(bus),
        },
    }
}

/// Tear down and rebuild the switcher chips for the current roster (one
/// `(bus_name, label)` per live player).
fn rebuild_switcher(row: &gtk::Box, roster: &[(String, String)], chips: &SwitcherChips) {
    // Hidden unless there is an actual choice to make.
    row.set_visible(roster.len() >= 2);

    while let Some(child) = row.first_child() {
        row.remove(&child);
    }
    let mut new_chips = Vec::with_capacity(roster.len() + 1);

    // "Auto" chip — revert to the heuristic.
    let auto = source_chip("Auto");
    auto.connect_clicked(|_| mpris::select_player(None));
    row.append(&auto);
    new_chips.push((None, auto));

    // One chip per player, labelled by identity (fallbacks below).
    for (bus, label) in roster {
        let chip = source_chip(label);
        let bus_for_click = bus.clone();
        chip.connect_clicked(move |_| mpris::select_player(Some(bus_for_click.clone())));
        row.append(&chip);
        new_chips.push((Some(bus.clone()), chip));
    }

    *chips.borrow_mut() = new_chips;
}

/// Label for a source chip: `identity`, falling back to `title`, then
/// `bus_name`.
fn player_label(player: &mpris::Player) -> String {
    if !player.identity.is_empty() {
        player.identity.clone()
    } else if !player.title.is_empty() {
        player.title.clone()
    } else {
        player.bus_name.clone()
    }
}

fn source_chip(label: &str) -> gtk::ToggleButton {
    let btn = gtk::ToggleButton::with_label(label);
    btn.add_css_class("ts-media-source");
    btn
}

/// Set a chip's active (pressed) state. `set_active` only emits `toggled`,
/// not `clicked` (our selection handler is on `clicked`), so this purely
/// restyles the chip and never re-fires selection. Guarded to a no-op when
/// already in the desired state.
fn set_toggle_silently(btn: &gtk::ToggleButton, active: bool) {
    if btn.is_active() != active {
        btn.set_active(active);
    }
}

fn build_art_panel(grid: &gtk::Grid) -> gtk::Image {
    let art_box = section("");
    art_box.set_size_request(scale(220), scale(220));
    art_box.add_css_class("ts-media-art");
    let art_image = gtk::Image::new();
    art_image.set_icon_name(Some("audio-x-generic-symbolic"));
    art_image.set_pixel_size(scale(200));
    art_box.append(&art_image);
    grid.attach(&art_box, 0, 1, 1, 1);
    art_image
}

struct InfoPanel {
    section: gtk::Box,
    widgets: InfoWidgets,
}

fn build_info_panel() -> InfoPanel {
    let section = section("Now Playing");

    let title = ellipsized_label("ts-media-title", 40);
    title.set_text("\u{2014}");
    let artist = ellipsized_label("ts-media-artist", 40);
    let album = ellipsized_label("ts-media-album", 40);
    section.append(&title);
    section.append(&artist);
    section.append(&album);

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    controls.set_margin_top(8);
    let prev_btn = gtk::Button::from_icon_name("media-skip-backward-symbolic");
    let play_pause_btn = gtk::Button::from_icon_name("media-playback-start-symbolic");
    let next_btn = gtk::Button::from_icon_name("media-skip-forward-symbolic");
    controls.append(&prev_btn);
    controls.append(&play_pause_btn);
    controls.append(&next_btn);
    section.append(&controls);

    let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.001);
    seek.set_draw_value(false);
    seek.set_hexpand(true);
    section.append(&seek);

    let time_line = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let pos = gtk::Label::new(Some("0:00"));
    pos.set_xalign(0.0);
    pos.set_hexpand(true);
    let len = gtk::Label::new(Some("0:00"));
    len.set_xalign(1.0);
    len.set_hexpand(true);
    time_line.append(&pos);
    time_line.append(&len);
    section.append(&time_line);

    InfoPanel {
        section,
        widgets: InfoWidgets {
            title,
            artist,
            album,
            prev_btn,
            play_pause_btn,
            next_btn,
            seek,
            pos,
            len,
        },
    }
}

fn ellipsized_label(css_class: &str, max_chars: i32) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.add_css_class(css_class);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(max_chars);
    label
}

fn wire_transport_buttons(w: &InfoWidgets, state: &Rc<PlayerState>) {
    bind_transport_button(&w.prev_btn, &state.bus, mpris::previous);
    bind_transport_button(&w.play_pause_btn, &state.bus, mpris::play_pause);
    bind_transport_button(&w.next_btn, &state.bus, mpris::next);
}

fn wire_player_bind(w: &InfoWidgets, art_image: &gtk::Image, state: &Rc<PlayerState>) {
    let w = w.clone();
    let state = state.clone();
    let art = art_image.clone();
    let title = w.title.clone();
    bind(
        mpris::active_player(),
        &title,
        move |_, maybe_player| match maybe_player {
            None => render_no_player(&w, &art, &state),
            Some(player) => render_player(&w, &art, &state, &player),
        },
    );
}

fn render_no_player(w: &InfoWidgets, art: &gtk::Image, state: &PlayerState) {
    *state.bus.borrow_mut() = None;
    *state.track_id.borrow_mut() = None;
    state.length_us.set(0);
    w.title.set_text("No player");
    w.artist.set_text("");
    w.album.set_text("");
    w.pos.set_text("0:00");
    w.len.set_text("0:00");
    art.set_paintable(None::<&gdk::Paintable>);
    art.set_icon_name(Some("audio-x-generic-symbolic"));
    art.set_pixel_size(scale(200));
    w.prev_btn.set_sensitive(false);
    w.play_pause_btn.set_sensitive(false);
    w.next_btn.set_sensitive(false);
}

fn render_player(w: &InfoWidgets, art: &gtk::Image, state: &PlayerState, player: &mpris::Player) {
    *state.bus.borrow_mut() = Some(player.bus_name.clone());
    (*state.track_id.borrow_mut()).clone_from(&player.track_id);
    state.length_us.set(player.length_us);

    w.title.set_text(&player.title);
    w.artist.set_text(&player.artists);
    w.album.set_text(&player.album);

    w.prev_btn.set_sensitive(player.can_go_previous);
    w.play_pause_btn.set_sensitive(player.can_play_pause);
    w.next_btn.set_sensitive(player.can_go_next);

    w.play_pause_btn
        .set_icon_name(play_pause_icon(player.status));

    w.pos.set_text(&fmt_us(player.position_us));
    w.len.set_text(&fmt_us(player.length_us));

    if *state.last_art_url.borrow() != player.art_url {
        (*state.last_art_url.borrow_mut()).clone_from(&player.art_url);
        spawn_art_fetch(art.clone(), player.art_url.clone());
    }
}

fn spawn_art_fetch(art: gtk::Image, url: String) {
    glib::MainContext::default().spawn_local(async move {
        let Some(bytes) = mpris::art_for_url(&url).await else {
            return;
        };
        let glib_bytes = glib::Bytes::from(&bytes);
        let Ok(texture) = gdk::Texture::from_bytes(&glib_bytes) else {
            return;
        };
        art.set_pixel_size(-1);
        art.set_paintable(Some(&texture));
        art.set_size_request(scale(200), scale(200));
    });
}

fn wire_seek(seek: &gtk::Scale, state: &Rc<PlayerState>) {
    let state_for_handler = state.clone();
    // Drag-safe: the mpris position poller writes this scale ~4×/s, which would
    // otherwise yank the thumb back mid-drag. `bind_two_way_drag_safe` suppresses
    // the poller's applies while the user is grabbing the slider (and briefly
    // after release) so the seek no longer fights the poller — see #445.
    bind_two_way_drag_safe(
        mpris::active_player().map(player_seek_fraction),
        seek,
        gtk::prelude::RangeExt::set_value,
        move |s| {
            let state = state_for_handler.clone();
            s.connect_value_changed(move |s| send_seek(s, &state))
        },
    );
}

fn player_seek_fraction(maybe: Option<mpris::Player>) -> f64 {
    let Some(p) = maybe else { return 0.0 };
    if p.length_us == 0 {
        return 0.0;
    }
    (cast::u64_to_f64(p.position_us) / cast::u64_to_f64(p.length_us)).clamp(0.0, 1.0)
}

fn send_seek(scale: &gtk::Scale, state: &PlayerState) {
    let bus_opt = state.bus.borrow();
    let tid_opt = state.track_id.borrow();
    let (Some(b), Some(t)) = (bus_opt.as_ref(), tid_opt.as_ref()) else {
        return;
    };
    let length = state.length_us.get();
    if length == 0 {
        return;
    }
    let pos_fraction = scale.value().clamp(0.0, 1.0);
    let pos_us = cast::f64_to_i64_trunc(pos_fraction * cast::u64_to_f64(length));
    mpris::set_position(b, t, pos_us);
}

#[cfg(test)]
mod tests {
    use super::{ChipState, chip_state};

    const AUTO: Option<&str> = None;
    const A: Option<&str> = Some("org.mpris.MediaPlayer2.a");
    const B: Option<&str> = Some("org.mpris.MediaPlayer2.b");

    fn state(pressed: bool, pinned: bool) -> ChipState {
        ChipState { pressed, pinned }
    }

    #[test]
    fn auto_is_pressed_in_automatic_mode() {
        // Nothing pinned, `a` merely won the heuristic: Auto owns the pressed
        // state for the *mode*, and `a` is pressed as the active source.
        assert_eq!(chip_state(None, A, AUTO), state(true, false));
        assert_eq!(chip_state(None, A, A), state(true, false));
        assert_eq!(chip_state(None, A, B), state(false, false));
    }

    #[test]
    fn auto_is_pressed_with_no_players_at_all() {
        assert_eq!(chip_state(None, None, AUTO), state(true, false));
    }

    #[test]
    fn pinning_a_player_releases_auto_and_marks_the_pin() {
        // The mirror image of the automatic case above, and the half the old
        // `active_player()`-only bind could express. It could not express the
        // other half — Auto was force-cleared whenever any player existed —
        // which is what made clicking Auto look like a no-op (#651).
        assert_eq!(chip_state(A, A, AUTO), state(false, false));
        assert_eq!(chip_state(A, A, A), state(true, true));
        assert_eq!(chip_state(A, A, B), state(false, false));
    }

    #[test]
    fn only_the_pinned_chip_is_marked_pinned() {
        // `b` is active *because* it is pinned; `a` is neither.
        assert_eq!(chip_state(B, B, A), state(false, false));
        assert_eq!(chip_state(B, B, B), state(true, true));
    }

    #[test]
    fn a_stale_pin_reads_as_automatic() {
        // The pinned player quit, so mpris' `resolve_active` already reverted
        // to the heuristic (`b`). The chips must say the same thing: Auto
        // pressed, and `b` active but not pinned.
        assert_eq!(chip_state(A, B, AUTO), state(true, false));
        assert_eq!(chip_state(A, B, B), state(true, false));
        // …and likewise when the last player is gone entirely.
        assert_eq!(chip_state(A, None, AUTO), state(true, false));
    }

    #[test]
    fn auto_and_the_resolved_source_light_up_together() {
        // Deliberate, and the reason the two states need different visuals:
        // in automatic mode "Auto" reports the *mode* while the player chip
        // reports which source that mode resolved to, so both are pressed.
        // Pinning collapses that to a single pressed chip.
        assert!(chip_state(None, A, AUTO).pressed && chip_state(None, A, A).pressed);
        assert!(!chip_state(A, A, AUTO).pressed && chip_state(A, A, A).pressed);
    }

    #[test]
    fn at_most_one_chip_is_pinned_and_a_pinned_chip_is_always_pressed() {
        for selected in [None, A, B] {
            for active in [None, A, B] {
                let pinned: Vec<Option<&str>> = [AUTO, A, B]
                    .into_iter()
                    .filter(|chip| chip_state(selected, active, *chip).pinned)
                    .collect();
                assert!(
                    pinned.len() <= 1,
                    "selected={selected:?} active={active:?} pinned {pinned:?}"
                );
                for chip in pinned {
                    assert!(
                        chip_state(selected, active, chip).pressed,
                        "pinned chip {chip:?} must also render pressed"
                    );
                }
            }
        }
    }
}
