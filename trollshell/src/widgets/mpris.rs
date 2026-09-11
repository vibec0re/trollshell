//! Bar MPRIS chip: prev / play-pause / next plus a fixed-width "artist – title"
//! label when the bar has room for them, a single mini icon button when it does
//! not, and nothing at all when no player is active. Both renditions open the
//! Media page on click — the label in the full row, the button in the mini one.
//!
//! ## Why a fixed title width, and why `AdwBreakpointBin` (#838)
//!
//! Four iterations tried to compute the fit in our own code and each failed the
//! same way — frozen watchers, then title-driven blinking, then a damped budget
//! that read the bar's width before the compositor's configure had landed. All
//! three are one bug: reconstructing, from outside the layout pass, a number
//! that only exists inside it. A fifth attempt used `AdwSqueezer`, which models
//! the problem exactly but has been deprecated since libadwaita 1.4.
//!
//! Annika's call on #838 settled both halves: *"deprecated is deprecated. Keep
//! things simple. Use fixed title length."* The second sentence is what makes
//! the supported migration path work. libadwaita points `AdwSqueezer` users at
//! `AdwBreakpointBin`, whose breakpoints fire on a **constant** `max-width`
//! condition — previously useless here, because the full row's width moved with
//! the track title. Pin the label to [`TITLE_CHARS`] and the row's width becomes
//! a constant, so the threshold becomes a constant too, and the supported widget
//! fits the problem.
//!
//! The decision still happens inside GTK's allocation pass, which is the only
//! place the true available width has ever existed. Nothing here watches, damps
//! or re-measures anything at runtime: the threshold is measured **once** from
//! the built row (see [`build_bin`]) and then frozen into the breakpoint.
//!
//! ### The one non-obvious part: freezing the child's size request
//!
//! A breakpoint that hides the full row would otherwise be a one-way door. With
//! a plain `GtkBox` holding both renditions, hiding the full row drops the box's
//! natural width to the mini chip's — so the bar hands the slot only that much
//! room, the breakpoint stays applied, and the chip can **never** expand again
//! no matter how much space appears. Measured, that is a fall from `(34, 294)`
//! to `(34, 34)`: permanently stuck, the iteration-1 failure wearing a new hat.
//!
//! So the request the bar sees has to be the same whichever rendition shows:
//! **minimum** the mini chip's width, so the bar may squeeze the slot;
//! **natural** the full row's, so the bar keeps offering enough room to expand
//! back into. That is all [`FrozenSize`] is — a `GtkLayoutManager` holding
//! those two constants, installed by [`build_bin`] on an `AdwBin` between the
//! breakpoint bin and the renditions box. It measures nothing at runtime and
//! reacts to nothing; it is a declaration, not a computation, which is what
//! separates it from the four iterations that tried to reconstruct the fit from
//! outside the layout pass. Request stability is exactly what makes the
//! feedback loop impossible, and it is asserted by
//! `the_request_is_the_same_whichever_rendition_shows`.
//!
//! ### Why a layout manager and not `set_size_request` (#838, #851, #854)
//!
//! Because `set_size_request` raises a widget's **minimum** — the natural only
//! follows because GTK clamps it up to the minimum (`gtk_widget_real_adjust_size_request`)
//! — and the minimum is the one number that must stay small here. In
//! libadwaita 1.9.3 (`src/adw-breakpoint-bin.c`):
//!
//! - `adw_breakpoint_bin_measure()` zeroes the **bin's** minimum once it has a
//!   breakpoint — `if (priv->breakpoints->len > 0) min = 0;` (lines 375–376).
//!   The child's own minimum is not touched.
//! - `allocate_child()` measures the child fresh (line 223), and when the bin's
//!   slot is narrower does `width = MAX (width, min_width)` before
//!   `gtk_widget_allocate` (lines 256–259): the child is laid out at its own
//!   minimum however narrow the bin's slot happens to be.
//! - `adw_breakpoint_bin_init()` calls
//!   `gtk_widget_set_overflow (GTK_WIDGET (self), GTK_OVERFLOW_HIDDEN)`
//!   (line 657), so the bin clips to its own allocation.
//!
//! "Collapsed" means, by the breakpoint's own condition, a bin width
//! `A <= full_px - 1`. A `set_size_request(full_px, -1)` pin on the renditions
//! box therefore put the child's minimum **strictly above the bin's width in
//! every collapsed allocation** — the one state this widget exists to produce.
//! The box was laid out `full_px` wide from the bin's left edge and clipped at
//! `A`, so everything inside it was positioned against a right edge that is not
//! on screen. #851 shipped that with `halign: End` on both renditions and the
//! chip simply vanished: measured, a 34 px chip at `x = 260..294` inside a
//! 147 px bin. #854 bought it back into the clip rectangle with
//! `halign: Start`, and that is where Annika found it on the seventh round —
//! *"expanded is right aligned in bar; icon left aligned — attached to variable
//! width window list. keep right plz"* — because `Start` pins the chip to the
//! left edge of a slot whose left edge is wherever the variable-width window
//! list happens to stop.
//!
//! What the widget wants from the bin is `adw_breakpoint_bin_set_natural_size()`,
//! which raises the natural and leaves the minimum alone. It exists, it does
//! exactly this, and it is **private** (`adw-breakpoint-bin-private.h`, absent
//! from the Rust bindings). [`FrozenSize`] is the public-API stand-in.
//!
//! With it, the bin's child reports a `mini_px` minimum, which is never above
//! the bin's own allocation, so `allocate_child()`'s `MAX` is a no-op and the
//! renditions box is laid out at **exactly the bin's width**. `halign: End`
//! then means the bin's own right edge — for both renditions, at every
//! allocation. There is no asymmetry left to document or to "tidy up": the two
//! renditions are aligned the same way, and the chip stays inside the clip
//! whatever the window list does. Asserted by
//! `both_renditions_hug_the_bins_right_edge` (two squeezed widths and one roomy
//! one, because a chip at a fixed offset inside a wider row coincides with the
//! bin's right edge at exactly one allocation) and by
//! `the_collapsed_chip_is_drawn_inside_the_bin`.
//!
//! It also retires the journal spam #856 recorded. `adw-breakpoint-bin.c:247`
//! warns `… exceeds AdwBreakpointBin width: requested N px, A px available`
//! whenever `min_width > width` — the child's measured minimum against the
//! bin's allocated width — which the pin made true at every collapsed
//! allocation and the shim makes false at all of them. Note that no test can
//! see this either way: the bin sets `block_warnings` around the first
//! allocation and the breakpoint-transition pass, so a test that presents one
//! window and allocates once never reaches the warning at all. The measurement
//! behind the claim is a deliberate *second* collapsed allocation under
//! `xvfb-run` with stderr grepped for the line — one line per re-allocation
//! with the pin, zero with the shim.
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

use self::frozen_size::FrozenSize;
use crate::components::mpris_controls::{bind_transport_button, play_pause_icon};

/// Width of the "artist – title" label, in characters.
///
/// Set as **both** `width_chars` and `max_width_chars`, so the label is exactly
/// this wide whatever the track is called: short titles pad, long ones ellipsize.
/// That is what makes the full row a constant width, and hence the breakpoint
/// threshold a constant — see the module doc.
///
/// A taste knob: Annika asked for a fixed title length and left the number to
/// us, so this is a starting value and safe to retune. Changing it changes the
/// bar width at which the chip collapses, and nothing else.
const TITLE_CHARS: i32 = 24;

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

    let bin = build_bin(
        full_row.upcast_ref(),
        build_mini_button(monitor).upcast_ref(),
    );

    let current_bus: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    bind_transport_button(&chips.prev_btn, &current_bus, mpris::previous);
    bind_transport_button(&chips.play_pause_btn, &current_bus, mpris::play_pause);
    bind_transport_button(&chips.next_btn, &current_bus, mpris::next);

    wire_visibility_and_state(&bin, chips, &current_bus);

    bin.set_visible(false);
    bin.upcast()
}

/// Assemble the breakpoint bin over the two renditions.
///
/// Split out from [`widget`] so the geometry contract this rewrite rests on can
/// be asserted against a real widget tree without a service registry — see the
/// `gtk_tests` module at the bottom of this file.
///
/// The shape, and why each part of it:
///
/// - **Both renditions live in one `GtkBox`**, `full` first, and a single
///   [`adw::Breakpoint`] flips their `visible` properties. Property setters are
///   what `AdwBreakpoint` is for, and they are declarative: the breakpoint
///   restores the previous values itself when it stops applying, so there is no
///   apply/unapply handler to keep in sync.
/// - **`hexpand` and `halign(End)` on both renditions.** They are aligned the
///   same way on purpose — Annika's *"keep right plz"* (#838): the slot's right
///   edge is where the bar's right-hand status cluster begins, and that is
///   where the media control belongs in either rendition, with the space it
///   gives up falling into the bar's existing mid-gap rather than moving with
///   the window list. This only works because [`FrozenSize`] keeps the box's
///   *minimum* down, so the bin lays the box out at its own allocation instead
///   of at the full row's width — see the module doc's "why a layout manager
///   and not `set_size_request`". Asserted by
///   `both_renditions_hug_the_bins_right_edge`.
/// - **[`FrozenSize`] on an `AdwBin` between the bin and the box** — the
///   load-bearing part. See the module doc: without a rendition-independent
///   natural width, collapsing is a one-way door and the chip can never expand
///   again; without a small minimum, the collapsed box is laid out wider than
///   the bin and clipped.
/// - **`set_size_request` on the bin** — required, not optional: libadwaita
///   documents that *"adding a breakpoint to `AdwBreakpointBin` will result in it
///   having no minimum size"*, and that `width-request` and `height-request`
///   must always be set to the smallest size you want to support. Width is the
///   mini chip's, which is what gives the bar permission to squeeze this slot at
///   all; height is the taller rendition's. Omitting the height half is not
///   silent — libadwaita warns on every allocation.
///
/// The threshold is measured here, once, and frozen into the condition. The
/// measurement happens after the CSS class and the child tree are in place so
/// the shell stylesheet's button padding is already included, and it never runs
/// again — a fixed-width label means there is nothing left that could move it.
/// Should the runtime row ever exceed the measured width anyway, the label's
/// end-ellipsize absorbs it, so the failure mode is a slightly shorter title
/// rather than a clipped or overflowing row.
fn build_bin(full: &gtk::Widget, mini: &gtk::Widget) -> adw::BreakpointBin {
    for w in [full, mini] {
        w.set_hexpand(true);
        w.set_halign(gtk::Align::End);
    }

    let renditions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    renditions.append(full);
    renditions.append(mini);

    // The shim that freezes the request. It has no other job: `AdwBin` is the
    // smallest thing in the bindings that parents one child and carries a
    // layout manager.
    let shim = adw::Bin::new();
    shim.set_child(Some(&renditions));

    let bin = adw::BreakpointBin::new();
    bin.add_css_class("ts-mpris");
    bin.set_child(Some(&shim));

    // Measure before hiding anything, with the tree and the CSS class already
    // in place. Frozen from here on.
    let full_px = natural_width(full);
    let mini_px = natural_width(mini);
    let height_px = natural_height(full).max(natural_height(mini));

    mini.set_visible(false);
    shim.set_layout_manager(Some(FrozenSize::new(mini_px, full_px, height_px)));
    // Both axes: libadwaita warns at runtime ("does not have a minimum height,
    // set the 'height-request' property") if only one is given, because the
    // breakpoint strips the bin's minimum in *both* directions. The bar never
    // squeezes vertically, so the height floor is simply the taller rendition.
    bin.set_size_request(mini_px, height_px);

    // Applies at `full_px - 1` and below, i.e. exactly when the full row no
    // longer fits; an allocation of exactly `full_px` keeps the full row.
    let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        f64::from(full_px - 1),
        adw::LengthUnit::Px,
    ));
    breakpoint.add_setter(full, "visible", Some(&false.to_value()));
    breakpoint.add_setter(mini, "visible", Some(&true.to_value()));
    bin.add_breakpoint(breakpoint);

    bin
}

/// A `GtkLayoutManager` that reports one frozen size request and hands its
/// widget's whole allocation straight to its child.
///
/// The public-API stand-in for libadwaita's private
/// `adw_breakpoint_bin_set_natural_size()`: it raises a natural width without
/// raising the minimum, which `gtk_widget_set_size_request` cannot do and which
/// is the entire difference between a mini chip that hugs the bar's right
/// cluster and one that is clipped away or stranded on the left (#838). See the
/// module doc's "why a layout manager and not `set_size_request`".
///
/// It has no inputs. The three numbers are measured once from the built
/// renditions in [`build_bin`] and never read anything again, so this cannot
/// become a fifth attempt at computing the fit from outside the layout pass:
/// there is nothing here to recompute.
mod frozen_size {
    use hytte::gtk::glib;
    use hytte::gtk::subclass::prelude::ObjectSubclassIsExt;

    mod imp {
        use std::cell::Cell;

        use hytte::gtk::prelude::WidgetExt;
        use hytte::gtk::subclass::prelude::*;
        use hytte::gtk::{self, glib};

        /// `width` is `(minimum, natural)`; `height` is both bounds at once —
        /// the bar never squeezes this slot vertically.
        #[derive(Default)]
        pub struct FrozenSize {
            pub width: Cell<(i32, i32)>,
            pub height: Cell<i32>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for FrozenSize {
            const NAME: &'static str = "TsMprisFrozenSize";
            type Type = super::FrozenSize;
            type ParentType = gtk::LayoutManager;
        }

        impl ObjectImpl for FrozenSize {}

        impl LayoutManagerImpl for FrozenSize {
            /// The frozen request. `for_size` is deliberately ignored: neither
            /// axis of this widget depends on the other.
            fn measure(
                &self,
                _widget: &gtk::Widget,
                orientation: gtk::Orientation,
                _for_size: i32,
            ) -> (i32, i32, i32, i32) {
                if orientation == gtk::Orientation::Horizontal {
                    let (min, nat) = self.width.get();
                    (min, nat, -1, -1)
                } else {
                    let height = self.height.get();
                    (height, height, -1, -1)
                }
            }

            /// Pass the allocation through untouched. `gtk_widget_allocate`
            /// applies the child's own margins and alignment, so a child that
            /// wants to sit against the right edge of what it is given still
            /// does — the point of the exercise being that what it is given is
            /// now the bin's own width.
            fn allocate(&self, widget: &gtk::Widget, width: i32, height: i32, baseline: i32) {
                if let Some(child) = widget.first_child() {
                    child.allocate(width, height, baseline, None);
                }
            }
        }
    }

    glib::wrapper! {
        /// See the module doc.
        pub struct FrozenSize(ObjectSubclass<imp::FrozenSize>)
            @extends hytte::gtk::LayoutManager;
    }

    impl FrozenSize {
        /// Freeze a request: `min_width` is the narrowest the bar may squeeze
        /// the widget to, `natural_width` the width it keeps asking for, and
        /// `height` both vertical bounds.
        pub fn new(min_width: i32, natural_width: i32, height: i32) -> Self {
            let this: Self = glib::Object::new();
            this.imp().width.set((min_width, natural_width));
            this.imp().height.set(height);
            this
        }
    }
}

/// A widget's natural width.
fn natural_width(w: &gtk::Widget) -> i32 {
    w.measure(gtk::Orientation::Horizontal, -1).1
}

/// A widget's natural height.
fn natural_height(w: &gtk::Widget) -> i32 {
    w.measure(gtk::Orientation::Vertical, -1).1
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

/// The title label, pinned to [`TITLE_CHARS`] wide in both directions so its
/// width never moves with the track name.
fn build_clickable_label(monitor: &Monitor) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_width_chars(TITLE_CHARS);
    label.set_max_width_chars(TITLE_CHARS);

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
///   breakpoint's job, every allocation pass.
///
/// The full row's chips stay current even while the mini chip is the visible
/// rendition, so the transport controls work the instant there is room again.
fn wire_visibility_and_state(
    bin: &adw::BreakpointBin,
    chips: Chips,
    current_bus: &Rc<RefCell<Option<String>>>,
) {
    let chips = Rc::new(chips);
    let current_bus = current_bus.clone();
    bind(
        mpris::active_player(),
        bin,
        move |bin, maybe_player: Option<Player>| match maybe_player {
            None => {
                bin.set_visible(false);
                *current_bus.borrow_mut() = None;
            }
            Some(player) => {
                *current_bus.borrow_mut() = Some(player.bus_name.clone());
                apply_player_to_widgets(&player, &chips);
                bin.set_visible(true);
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

/// The geometry contract, asserted rather than believed (#838).
///
/// The pure `decide_full` unit tests this replaces are gone with the machinery
/// they tested: there is no longer a decision function, because the decision is
/// GTK's. What is worth testing instead is that the hand-off to GTK is the one
/// the module doc claims — that the title width really is fixed, that the
/// request really is rendition-independent (the anti-stuck property), and that
/// a crowded bar really does flip the rendition.
///
/// Gated behind `system-tests` because instantiating widgets needs a display
/// (`xvfb-run`).
#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use super::{TITLE_CHARS, build_bin, icon_button, natural_width};
    use hytte::adw::{self, prelude::*};
    use hytte::gtk;

    /// Run the GTK main loop until it has nothing left to dispatch, so a
    /// queued resize/allocation actually happens.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// A title label built exactly as [`super::build_clickable_label`] builds
    /// it, minus the click gesture (which needs a live `Monitor`).
    fn title_label(text: &str) -> gtk::Label {
        let label = gtk::Label::new(Some(text));
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_width_chars(TITLE_CHARS);
        label.set_max_width_chars(TITLE_CHARS);
        label
    }

    /// The two renditions, minus everything that needs a live `Monitor` and the
    /// service registry. Geometry is what is under test, and geometry does not
    /// depend on either.
    fn renditions(title: &str) -> (gtk::Box, gtk::Button) {
        let full = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        full.append(&icon_button("media-skip-backward-symbolic"));
        full.append(&icon_button("media-playback-start-symbolic"));
        full.append(&icon_button("media-skip-forward-symbolic"));
        full.append(&title_label(title));

        let mini = gtk::Button::new();
        mini.set_child(Some(&gtk::Image::from_icon_name(
            "audio-x-generic-symbolic",
        )));
        (full, mini)
    }

    /// Which rendition the bin is currently showing: the `GtkBox` full row or
    /// the `GtkButton` mini chip.
    fn shown(bin: &adw::BreakpointBin) -> gtk::Widget {
        let shim = bin
            .child()
            .and_downcast::<adw::Bin>()
            .expect("bin child is the frozen-size shim");
        let renditions = shim
            .child()
            .and_downcast::<gtk::Box>()
            .expect("the shim's child is the renditions box");
        let mut child = renditions.first_child();
        while let Some(w) = child {
            if w.is_visible() {
                return w;
            }
            child = w.next_sibling();
        }
        panic!("no rendition is visible");
    }

    /// Annika's actual ask on #838: *"Use fixed title length."* The label is
    /// pinned in both directions, so a one-word track and a paragraph-length one
    /// produce the same row width. That constancy is the whole reason a single
    /// frozen breakpoint threshold is correct — if this fails, the threshold is
    /// measuring one title and being applied to another.
    #[gtk::test]
    fn the_row_width_does_not_move_with_the_title() {
        adw::init().expect("libadwaita init");
        let short = natural_width(renditions("Hi").0.upcast_ref());
        let long = natural_width(
            renditions("An Extremely Long Artist Name \u{2013} And A Longer Track Title Still")
                .0
                .upcast_ref(),
        );
        assert_eq!(
            short, long,
            "the full row must be the same width whatever the title says"
        );
    }

    /// The anti-stuck property, and the reason the renditions box carries a
    /// `set_size_request`. The bin must report the same size request whichever
    /// rendition is visible: minimum the mini chip's width (so the bar may
    /// squeeze the slot), natural the full row's (so the bar keeps offering
    /// enough room to expand back into).
    ///
    /// Falsified by deleting the `set_size_request` on the renditions box: the
    /// collapsed natural then drops to the mini chip's width, the bar stops
    /// offering more, and the breakpoint can never unapply. Measured, that is
    /// `(34, 294)` becoming `(34, 34)`.
    #[gtk::test]
    fn the_request_is_the_same_whichever_rendition_shows() {
        adw::init().expect("libadwaita init");
        let (full, mini) = renditions("Artist \u{2013} Title");
        let full_px = natural_width(full.upcast_ref());
        let mini_px = natural_width(mini.clone().upcast_ref());
        let bin = build_bin(full.upcast_ref(), mini.upcast_ref());

        let expanded = (
            bin.measure(gtk::Orientation::Horizontal, -1).0,
            bin.measure(gtk::Orientation::Horizontal, -1).1,
        );
        assert_eq!(
            expanded,
            (mini_px, full_px),
            "the bin's floor must be the mini chip and its ceiling the full row"
        );

        // Force the collapsed configuration the breakpoint would produce.
        full.set_visible(false);
        mini.set_visible(true);
        let collapsed = (
            bin.measure(gtk::Orientation::Horizontal, -1).0,
            bin.measure(gtk::Orientation::Horizontal, -1).1,
        );
        assert_eq!(
            collapsed, expanded,
            "the request must not change when the rendition does — a natural width that drops \
             on collapse means the chip can never expand again (#838)"
        );
    }

    /// The decision itself, made by GTK: an allocation that cannot hold the
    /// full row shows the mini chip; one with room shows the full row.
    #[gtk::test]
    fn a_tight_allocation_selects_the_mini_chip() {
        adw::init().expect("libadwaita init");
        let full_px = natural_width(renditions("Artist \u{2013} Title").0.upcast_ref());

        assert!(
            shown(&bin_at(full_px / 2)).is::<gtk::Button>(),
            "an allocation too small for the full row must select the mini chip"
        );
        assert!(
            shown(&bin_at(full_px + 100)).is::<gtk::Box>(),
            "an allocation with room to spare must select the full row"
        );
    }

    /// Put a fresh bin in a fresh window `width` px wide and let GTK allocate
    /// it. A window per case on purpose: `set_default_size` only takes effect
    /// before the window is mapped, so resizing one already-presented window
    /// silently measures the first size twice.
    fn bin_at(width: i32) -> adw::BreakpointBin {
        let (full, mini) = renditions("Artist \u{2013} Title");
        let bin = build_bin(full.upcast_ref(), mini.upcast_ref());
        let window = gtk::Window::new();
        window.set_child(Some(&bin));
        window.set_default_size(width, 40);
        window.present();
        pump();
        window.set_child(None::<&gtk::Widget>);
        window.destroy();
        bin
    }

    /// The placebo check (#838): the *bar's own shape* has to be able to
    /// squeeze this slot, or none of the above matters.
    ///
    /// `hytte_ui::Bar` builds `CenterBox[left, end_pair[centre, right]]` with
    /// **no centre child** — the "centre" group rides the end widget
    /// (`crates/hytte-ui/src/bar.rs`). A reasonable worry is that such a shape
    /// always hands `end_pair` its natural width and starves the start child
    /// instead, in which case the bin would never see a tight allocation. It
    /// does not: `GtkCenterBox` gives each child its minimum and then
    /// distributes what is left toward the naturals, so an over-subscribed bar
    /// shortens both sides.
    ///
    /// Asserted at both ends so it cannot pass by being stuck — which, given
    /// stuck is this widget's oldest failure mode, is the point.
    #[gtk::test]
    fn the_bar_shape_can_actually_squeeze_the_slot() {
        adw::init().expect("libadwaita init");

        // `hytte_ui::Bar::show()`'s tree, with deliberately tiny stand-ins for
        // the two clusters. The roomy case needs a window wider than the whole
        // tree's natural width, and the test harness' virtual display is only
        // 640 px (`xvfb-run -a` with no `-screen`, which is what `nix flake
        // check` uses too) — anything bigger gets clamped, and a clamped window
        // would measure as "crowded" in every case and pass the crowded
        // assertion for the wrong reason. The `roomy_width` check below turns
        // that into a loud failure if it ever happens anyway.
        let build_bar = || {
            let left = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let btn = gtk::Button::with_label("a window title");
            if let Some(l) = btn.child().and_downcast::<gtk::Label>() {
                l.set_ellipsize(gtk::pango::EllipsizeMode::End);
                l.set_max_width_chars(6);
            }
            left.append(&btn);

            let (full, mini) = renditions("Artist \u{2013} Title");
            // The real label is TITLE_CHARS wide; trim it here so the whole
            // tree still fits the display in the roomy case.
            if let Some(label) = full.last_child().and_downcast::<gtk::Label>() {
                label.set_width_chars(8);
                label.set_max_width_chars(8);
            }
            let bin = build_bin(full.upcast_ref(), mini.upcast_ref());

            let middle = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            middle.append(&bin);
            let right = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            for _ in 0..2 {
                right.append(&icon_button("audio-volume-high-symbolic"));
            }

            let end_pair = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            end_pair.append(&middle);
            end_pair.append(&right);

            let center_box = gtk::CenterBox::new();
            center_box.set_start_widget(Some(&left));
            center_box.set_end_widget(Some(&end_pair));
            (center_box, bin)
        };

        // Returns the chosen rendition *and* the width the bar actually got, so
        // a display too small to honour the request fails loudly rather than
        // masquerading as a crowded bar.
        let chosen_at = |bar_width: i32| {
            let (center_box, bin) = build_bar();
            let window = gtk::Window::new();
            window.set_child(Some(&center_box));
            window.set_default_size(bar_width, 40);
            window.present();
            pump();
            let chosen = shown(&bin);
            let got = center_box.width();
            window.destroy();
            (chosen, got)
        };

        let (bar_min, bar_nat) = {
            let (center_box, _) = build_bar();
            let (min, nat, _, _) = center_box.measure(gtk::Orientation::Horizontal, -1);
            (min, nat)
        };
        assert!(
            bar_nat > bar_min,
            "test setup: the bar tree must have room to be squeezed at all \
             (min={bar_min}, natural={bar_nat})"
        );

        let (roomy, roomy_width) = chosen_at(bar_nat + 200);
        assert!(
            roomy_width >= bar_nat,
            "test setup: the display clamped the roomy bar to {roomy_width} px, below the \
             {bar_nat} px this tree wants — the roomy case cannot be measured here"
        );
        assert!(
            roomy.is::<gtk::Box>(),
            "a bar wider than everything's natural width must show the full row, got a {}",
            roomy.type_()
        );

        // A bar squeezed to just above its own minimum: every cluster is
        // fighting for room, which is exactly Annika's crowded-workspace case.
        let (crowded, crowded_width) = chosen_at(bar_min + 40);
        assert!(
            crowded.is::<gtk::Button>(),
            "a crowded bar must squeeze this slot down to the mini chip — if this fails the \
             bin is never squeezed and the widget is a placebo (#838); \
             bar min={bar_min}, natural={bar_nat}, allocated={crowded_width}, got a {}",
            crowded.type_()
        );
    }

    /// Where the shown rendition actually lands inside the bin.
    ///
    /// Everything the two tests below assert has to be captured *while the
    /// window is still up*: `compute_bounds` and `pick` both stop meaning
    /// anything the moment the bin is unparented.
    struct Placement {
        /// Is the mini chip the rendition on screen?
        collapsed: bool,
        left: f64,
        top: f64,
        right: f64,
        bottom: f64,
        bin_w: f64,
        bin_h: f64,
        /// Did a click at the rendition's centre land on it?
        hit: bool,
        /// What `pick` returned there, for the failure message.
        picked: String,
    }

    impl Placement {
        /// Is the rendition inside the rectangle the bin paints and clips to?
        fn inside_the_bin(&self) -> bool {
            self.left >= 0.0
                && self.top >= 0.0
                && self.right <= self.bin_w
                && self.bottom <= self.bin_h
        }

        /// Distance from the rendition's right edge to the bin's — the number
        /// Annika's *"keep right plz"* is about.
        fn right_edge_gap(&self) -> f64 {
            (self.bin_w - self.right).abs()
        }

        /// The geometry, for a failure message.
        fn describe(&self) -> String {
            format!(
                "bounds x={}..{}, y={}..{} in a {}x{} bin",
                self.left, self.right, self.top, self.bottom, self.bin_w, self.bin_h
            )
        }
    }

    /// Present a fresh bin in a fresh window `width` px wide, let GTK allocate
    /// it, and measure where the chosen rendition ended up. One window per
    /// case, for the reason [`bin_at`] documents.
    fn placement_at(width: i32) -> Placement {
        let (full, mini) = renditions("Artist \u{2013} Title");
        let bin = build_bin(full.upcast_ref(), mini.upcast_ref());
        let window = gtk::Window::new();
        window.set_child(Some(&bin));
        window.set_default_size(width, 40);
        window.present();
        pump();

        let rendition = shown(&bin);
        let collapsed = rendition.is::<gtk::Button>();
        let bounds = rendition
            .compute_bounds(&bin)
            .expect("the shown rendition is a descendant of the bin");
        let (bin_w, bin_h) = (f64::from(bin.width()), f64::from(bin.height()));
        let (left, top) = (f64::from(bounds.x()), f64::from(bounds.y()));
        let (right, bottom) = (
            f64::from(bounds.x() + bounds.width()),
            f64::from(bounds.y() + bounds.height()),
        );
        let picked = bin.pick(
            f64::midpoint(left, right),
            f64::midpoint(top, bottom),
            gtk::PickFlags::DEFAULT,
        );
        let hit = picked
            .as_ref()
            .is_some_and(|w| *w == rendition || w.is_ancestor(&rendition));
        let picked = picked.map_or_else(|| "nothing".to_owned(), |w| w.type_().to_string());

        window.set_child(None::<&gtk::Widget>);
        window.destroy();

        Placement {
            collapsed,
            left,
            top,
            right,
            bottom,
            bin_w,
            bin_h,
            hit,
            picked,
        }
    }

    /// The bug the three tests above could not see (#838).
    ///
    /// [`shown`] reads `is_visible()`, and GTK's `visible` is orthogonal to
    /// *position* and to *clipping*: a widget can be `visible`, correctly
    /// selected by the breakpoint, and still be laid out entirely outside the
    /// rectangle its parent paints. That is precisely what #851 shipped — the
    /// mini chip was `visible` on every crowded bar and drawn nowhere, because
    /// `AdwBreakpointBin`
    ///
    /// - zeroes its **own** minimum once it has a breakpoint
    ///   (`adw_breakpoint_bin_measure`: `if (priv->breakpoints->len > 0) min = 0;`)
    ///   but never the child's,
    /// - allocates the child `MAX (width, min_width)` regardless of its own
    ///   slot (`allocate_child`), and
    /// - clips to its own allocation (`adw_breakpoint_bin_init`:
    ///   `gtk_widget_set_overflow (GTK_WIDGET (self), GTK_OVERFLOW_HIDDEN)`).
    ///
    /// So assert the rectangle, not the flag: at a tight allocation the shown
    /// rendition's bounds must lie inside the bin's own, and a click at its
    /// centre must actually land on it.
    #[gtk::test]
    fn the_collapsed_chip_is_drawn_inside_the_bin() {
        adw::init().expect("libadwaita init");
        let full_px = natural_width(renditions("Artist \u{2013} Title").0.upcast_ref());
        let width = full_px / 2;
        let placement = placement_at(width);

        assert!(
            placement.collapsed,
            "test setup: an allocation of {width} px must select the mini chip"
        );
        assert!(
            placement.inside_the_bin(),
            "the shown rendition is drawn outside the bin that clips it, so it is invisible and \
             unclickable on a crowded bar (#838): {}",
            placement.describe()
        );
        assert!(
            placement.hit,
            "a click at the centre of the shown rendition must land on it, got {} ({})",
            placement.picked,
            placement.describe()
        );
    }

    /// Annika's seventh round on #838: *"expanded is right aligned in bar; icon
    /// left aligned — attached to variable width window list. keep right plz"*.
    ///
    /// #854 bought its way back inside the clip rectangle with `halign: Start`
    /// on the mini chip, which parks the collapsed chip against whatever the
    /// window list leaves instead of against the bar's right-hand cluster. The
    /// fix is not a different alignment but a different **allocation**: the
    /// renditions box is laid out at the bin's own width (see the module doc's
    /// frozen-request shim), so `End` now means the bin's right edge rather
    /// than the pinned row's.
    ///
    /// Two squeezed widths, because one of them can be right for the wrong
    /// reason — a chip at a *fixed* offset inside a wider pinned row coincides
    /// with the bin's right edge at exactly one allocation. With
    /// `TITLE_CHARS = 24` these are #854's measured 147 px bin and a 254 px one.
    #[gtk::test]
    fn both_renditions_hug_the_bins_right_edge() {
        adw::init().expect("libadwaita init");
        let full_px = natural_width(renditions("Artist \u{2013} Title").0.upcast_ref());

        for width in [full_px / 2, full_px - 40] {
            let placement = placement_at(width);
            assert!(
                placement.collapsed,
                "test setup: an allocation of {width} px must select the mini chip"
            );
            assert!(
                placement.right_edge_gap() <= 1.0,
                "the collapsed chip must sit against the bar's right-hand cluster like the full \
                 row does, not against the variable-width window list (#838): its right edge is \
                 {} px from the bin's, {}",
                placement.right_edge_gap(),
                placement.describe()
            );
            assert!(
                placement.inside_the_bin(),
                "the collapsed chip is drawn outside the bin that clips it (#838): {}",
                placement.describe()
            );
            assert!(
                placement.hit,
                "a click at the centre of the collapsed chip must land on it, got {} ({})",
                placement.picked,
                placement.describe()
            );
        }

        let roomy = placement_at(full_px + 100);
        assert!(
            !roomy.collapsed,
            "test setup: an allocation of {} px must select the full row",
            full_px + 100
        );
        assert!(
            roomy.right_edge_gap() <= 1.0,
            "the full row must keep hugging the bin's right edge when there is room (#838): its \
             right edge is {} px from the bin's, {}",
            roomy.right_edge_gap(),
            roomy.describe()
        );
    }
}
