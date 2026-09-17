//! Layer-shell **plugin dialog** overlay (#1010): a centered surface on the
//! focused output holding one plugin's own page.
//!
//! Raised by the effect broker ([`crate::plugins`]) when a **sidebar-mounted**
//! plugin emits `Effect::OpenPage(Page::PluginSelf)`. A bar chip's page keeps
//! opening the drawer beneath the bar, unchanged — see
//! `plugins::effects::page_surface`, which is where that decision is written
//! down. The plugin does not change a line to appear here, and the wire
//! vocabulary does not change: the routing is host-side, on the mount the host
//! already knew.
//!
//! Raised by Mara live-verifying #963, twice: the agents card sits bottom-left
//! in the sidebar and its rows opened a panel top-right in the drawer — *"its
//! very weird the panel opens in the top right after clicking bottom left"*.
//!
//! # Two shipped shapes, nothing new invented
//!
//! * The **window** is the drawer's (`modal.rs`'s `install`): one fullscreen
//!   layer surface whose `gtk::Overlay` *main* child is a transparent
//!   click-catcher (`.ts-modal-catcher`) and whose *overlay* child is a centered
//!   positioner carrying the card. That is how the drawer already gets "a click
//!   outside dismisses" with **no dimming** — the catcher paints nothing. Its
//!   cost is the drawer's cost today: while the dialog is up, pointer events on
//!   that output go to the catcher rather than through it.
//!
//!   The one difference from the drawer is [`Layer::Overlay`] +
//!   [`KeyboardMode::Exclusive`] instead of `Layer::Top` + `OnDemand` (the
//!   secret prompt's and the consent card's choice), so `Esc` always lands and a
//!   plugin `Entry` inside the page gets the keys.
//! * The **body** is the drawer's plugin child — [`plugins::plugin_dialog_slot`]
//!   builds the very tree `plugins::plugin_panel_slot` does, over the dialog's
//!   **own** selection (`dialog_panel_id`) rather than the drawer's. Same
//!   reconciler, same event routing to the plugin's live connection, same #903
//!   destroy story.
//!
//! # What "modal" means here
//!
//! Keyboard-exclusive while it is up, and any outside click closes it. That is
//! all. There is **no backdrop and no dimming** — Annika on #1010, 2026-09-10:
//! *"Let's keep it simple 4 now <3 no dimming <3"*. Other windows stay visible;
//! they are not clickable *through* the catcher while the dialog is up, exactly
//! as with the drawer. When the surface unmaps, niri hands keyboard focus back
//! to the previous toplevel, the way it does after the secret prompt closes.
//!
//! The sidebar stays up underneath (Annika's *"default 🤷‍♀️"* on §8, 2026-09-17):
//! the catcher covers it like everything else, and the first outside click
//! closes the dialog only.
//!
//! # One window, on the focused output
//!
//! Not one per monitor: [`install`] keeps a connector → [`Monitor`] map and
//! [`open_on_focused`] builds the single window on niri's focused output, the
//! `consent.rs` shape (#499/#517). Opening a second plugin's page while one is
//! up swaps the selection in place; opening on a *different* output rebuilds the
//! one window there. Never two windows.
//!
//! CSS hooks (`ts-`-prefixed, matching the prompt/consent overlays' shape):
//! - window root: `.ts-dialog` (transparent, like `.ts-prompt`)
//! - click-catcher: `.ts-modal-catcher` (the drawer's own transparent rule)
//! - card: `.ts-dialog-card`
//! - header row: `.ts-dialog-header`, its plugin-id label `.ts-dialog-title`
//!
//! The plugin's tree is reached through the existing `.ts-plugin-panel`
//! **descendant** rules, so plugin styling does not fork between drawer and
//! dialog.

use std::cell::RefCell;
use std::collections::HashMap;

use hytte::adw;
use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::ui::{Anchor, Layer, LayerShell, layer_window};
use hytte_plugin_proto::Mount;

use crate::overlays::sidebar::SIDEBAR_WIDTH;
use crate::plugins;
use crate::scale::scale;

// ── Tunables ──────────────────────────────────────────────────────────────────

/// The card's `AdwClamp` cap, in CSS px before [`scale`] — roughly twice the
/// sidebar card width a page opened from a sidebar card was drawn beside.
///
/// A cap rather than a fixed width: it also ends the "a long URL widens the
/// surface" class for good (#967), because the clamp stops the page's natural
/// width from reaching the surface at all.
const DIALOG_MAX_WIDTH: i32 = SIDEBAR_WIDTH * 2;

/// Height budget for the card's scroller, in CSS px before [`scale`].
///
/// Deliberately a constant rather than the drawer's measured
/// `BarGeometry::available_card_height`: this surface hangs off no bar and
/// reserves nothing, so there is no geometry to derive it from — and a centered
/// card that grows to the full height of a tall output is the shape #967 asked
/// the sidebar not to have either. Chosen a little above the drawer's
/// `MIN_CARD_HEIGHT` floor (240) so an ordinary page does not scroll at all.
const DIALOG_MAX_HEIGHT: i32 = 560;

/// The layer-shell namespace this surface announces itself to the compositor as
/// — a niri rule can match it, like `hytte-prompt` / `hytte-consent`.
const DIALOG_NAMESPACE: &str = "hytte-dialog";

// ── Thread-local state ────────────────────────────────────────────────────────

/// The live dialog: its window, plus the connector it was built on so a second
/// open on the *same* output can swap the selection instead of rebuilding.
struct LiveDialog {
    window: gtk::Window,
    connector: String,
}

thread_local! {
    /// The single live dialog window, if any. One globally — not one per
    /// monitor — like `consent.rs`'s prompt.
    static DIALOG: RefCell<Option<LiveDialog>> = const { RefCell::new(None) };

    /// Mounted monitors keyed by `Monitor.connector()`, so [`open_on_focused`]
    /// can build on niri's focused output. Re-keyed on each hot-plug via
    /// [`close_all`] + [`install`], exactly as `consent.rs` does.
    static MONITORS: RefCell<HashMap<String, Monitor>> = RefCell::new(HashMap::new());
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Register `monitor` as a candidate output for plugin dialogs. Called per
/// monitor from `main.rs`'s `monitors_changed` loop, like
/// [`crate::overlays::consent::install`]; the focused-output *tracker* lives in
/// the shared [`crate::components::focused_output`] cache, so this only
/// maintains the connector → [`Monitor`] map [`open_on_focused`] resolves
/// against.
pub fn install(monitor: &Monitor) {
    let Some(connector) = monitor.connector() else {
        tracing::debug!("dialog::install: monitor has no connector name; skipping");
        return;
    };
    // Tail-expression `insert` + an outer `drop`, the #643 shape the other four
    // `install` sites use: the displaced `Monitor`'s drop is a `GdkMonitor`
    // refcount decrement, which emits nothing, but the shape is uniform so
    // nobody has to re-derive which of them were "the real ones".
    drop(MONITORS.with(|m| m.borrow_mut().insert(connector, monitor.clone())));
}

/// Close any live dialog and forget the mounted monitors before a hot-plug
/// rebuild, mirroring `consent::close_all`. The re-install re-keys cleanly.
pub fn close_all() {
    close();
    MONITORS.with(|m| m.borrow_mut().clear());
}

/// Open `plugin_id`'s own page in the dialog on niri's focused output (`preferred`),
/// falling back to any mounted one. Called from the plugin effect broker when a
/// **sidebar-mounted** plugin emits `Effect::OpenPage(Page::PluginSelf)`.
/// GTK-main-thread only (the broker runs there).
///
/// `mount` is the producing plugin's mount, carried through for the
/// slot-visibility contribution (#1010 §4) — a page that is up must not let its
/// plugin park the poller feeding it.
///
/// Opening while a dialog is already up on the same output **swaps the
/// selection** (same plugin: a no-op re-publish); on a different output the one
/// window is rebuilt there. There is never a second window.
///
/// No-op with one `debug!` line when no monitor is mounted at all — there is
/// nowhere to show it, and the plugin is told nothing either way (`OpenPage` is
/// a one-way effect).
pub fn open_on_focused(preferred: Option<&str>, plugin_id: &str, mount: Mount) {
    let Some((connector, monitor)) = resolve_monitor(preferred) else {
        tracing::debug!(plugin = %plugin_id, "plugin dialog: no monitor to show on");
        return;
    };

    // Already up on this output → swap the selection in place. The window, its
    // catcher and its panel slot all stay; only the header text and the
    // dialog's selection change, so the slot's live subscription reconciles the
    // new plugin's tree without a second mount (and without releasing and
    // re-taking a preem scope it may still need).
    let same_output = DIALOG.with(|d| {
        d.borrow()
            .as_ref()
            .is_some_and(|live| live.connector == connector)
    });
    if same_output {
        set_title(plugin_id);
        publish_selection(plugin_id, mount);
        return;
    }

    // A different output (or nothing up): tear the old one down first, so
    // "one dialog at a time" is a property of this function rather than of
    // whoever calls it.
    close();
    show(&monitor, &connector, plugin_id);
    publish_selection(plugin_id, mount);
}

/// Dismiss the dialog: take the window down, clear the dialog's selection and
/// drop its slot-visibility contribution.
///
/// The single close path — `Esc`, the close button, a click on the catcher and
/// the `dialog-close` `GAction` all land here, so there is one place that
/// decides what dismissal *does* (the `consent.rs` `resolve` argument). A no-op
/// when nothing is up, which is what makes the `GAction` safe to bind blind.
///
/// Clearing **only** the dialog's own selection is the §2.1 contract: the
/// drawer's `active_panel_id` is never touched here, so a drawer showing another
/// plugin's page on another monitor keeps showing it.
pub fn close() {
    let taken = DIALOG.with(|d| d.borrow_mut().take());
    let was_open = taken.is_some();
    if let Some(live) = taken {
        // Bind-then-act (#631): a GTK call made inside the `if let` on a
        // `RefCell` scrutinee would hold the borrow across it.
        //
        // Closing destroys the panel slot inside, whose own `connect_destroy`
        // aborts its render subscription and releases the panel scope it was
        // showing — refcounted across children (#921), so a drawer showing the
        // same plugin keeps its renderer instances.
        live.window.close();
    }
    if was_open {
        crate::plugins::set_dialog_panel(None);
        crate::plugins::set_dialog_visibility(None);
    }
}

/// Whether a dialog is currently up. Read by [`gtk_tests`] and by nothing
/// shipped: the shell's own surfaces coordinate through the selection handle,
/// not through this.
#[cfg(all(test, feature = "system-tests"))]
fn is_open() -> bool {
    DIALOG.with(|d| d.borrow().is_some())
}

// ── Internals ─────────────────────────────────────────────────────────────────

/// The focused output's monitor, or any mounted one — `modal::open_on_focused`'s
/// fallback rule, so an unknown or absent focused output still shows the page
/// somewhere rather than silently dropping the click.
fn resolve_monitor(preferred: Option<&str>) -> Option<(String, Monitor)> {
    MONITORS.with(|m| {
        let monitors = m.borrow();
        preferred
            .and_then(|key| monitors.get_key_value(key))
            .or_else(|| monitors.iter().next())
            .map(|(key, monitor)| (key.clone(), monitor.clone()))
    })
}

/// Publish the dialog's selection into the plugin host: which plugin's panel the
/// dialog child renders, and the mount whose slot-visibility aggregate this
/// dialog holds up while it is on screen.
fn publish_selection(plugin_id: &str, mount: Mount) {
    crate::plugins::set_dialog_panel(Some(plugin_id));
    crate::plugins::set_dialog_visibility(Some(mount));
}

/// Build and present the one dialog window on `monitor`, showing `plugin_id`'s
/// page. The caller has already taken any previous window down.
fn show(monitor: &Monitor, connector: &str, plugin_id: &str) {
    let window = layer_window(monitor)
        .layer(Layer::Overlay)
        // All four edges, so the inner `Overlay` fills the screen and the
        // catcher really does catch every outside click — the drawer's
        // fullscreen-surface shape (`modal::build_drawer_window`).
        .anchor(Anchor::Top)
        .anchor(Anchor::Bottom)
        .anchor(Anchor::Left)
        .anchor(Anchor::Right)
        .exclusive(false)
        .keyboard_mode(KeyboardMode::Exclusive)
        .namespace(DIALOG_NAMESPACE)
        .build();
    window.add_css_class("ts-dialog");
    // Ignore other layer surfaces' exclusive zones so the card centers on the
    // true screen, not on whatever the bar left over (the drawer's `-1`).
    window.set_exclusive_zone(-1);

    window.set_child(Some(&build_root(plugin_id, &plugins::plugin_dialog_slot())));
    wire_escape(&window);

    window.set_visible(true);
    window.present();

    DIALOG.with(|d| {
        *d.borrow_mut() = Some(LiveDialog {
            window,
            connector: connector.to_owned(),
        });
    });
}

/// Which key presses dismiss the dialog: `Escape`, and nothing else.
///
/// A named predicate rather than a comparison inline in the handler so the rule
/// is pinnable on its own — the handler below is reachable from a test (see
/// [`gtk_tests`]), but only by emitting the signal, and a predicate that reads
/// as "`Esc` closes, every other key reaches the plugin's page" is worth being
/// able to state.
fn dismisses(key: gdk::Key) -> bool {
    key == gdk::Key::Escape
}

/// `Esc` dismisses, wherever the focus sits inside the surface — the prompt
/// overlay's controller, on the window so a plugin `Entry` holding focus does
/// not swallow it.
///
/// Every other key `Proceed`s, so typing into a plugin's `Entry` inside the page
/// works exactly as it does in the drawer.
fn wire_escape(window: &gtk::Window) {
    let key_ctrl = gtk::EventControllerKey::new();
    key_ctrl.connect_key_pressed(|_, key, _, _| {
        if dismisses(key) {
            close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_ctrl);
}

/// The dialog's whole widget tree: a `gtk::Overlay` whose main child is the
/// transparent click-catcher and whose overlay child is the centered card
/// (header + `body`).
///
/// Split out of [`show`] — and taking `body` as a parameter — for
/// `plugins::region::build_panel_child`'s reason: [`plugins::plugin_dialog_slot`]
/// `.expect()`s a registered `PluginHandles` out of the thread-local registry,
/// which a `#[gtk::test]` has no booted `App` to provide, and [`show`] itself
/// needs a live [`Monitor`] and a Wayland compositor to build a layer surface
/// against. Everything this function assembles is the part a test can drive, and
/// it is the part with a shape to get wrong.
fn build_root(plugin_id: &str, body: &impl IsA<gtk::Widget>) -> gtk::Overlay {
    // Transparent click-catching background: any press dismisses. The drawer's
    // class, so it inherits the one transparent rule rather than a second copy
    // of it.
    let catcher = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    catcher.add_css_class("ts-modal-catcher");
    catcher.set_hexpand(true);
    catcher.set_vexpand(true);
    let gesture = gtk::GestureClick::new();
    gesture.set_button(0);
    gesture.connect_pressed(|_, _, _, _| close());
    catcher.add_controller(gesture);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&catcher));
    overlay.add_overlay(&build_card(plugin_id, body));
    overlay
}

/// The card itself: header row over the page, clamped in width and bounded in
/// height, centered on the surface.
fn build_card(plugin_id: &str, body: &impl IsA<gtk::Widget>) -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.add_css_class("ts-dialog-card");
    column.append(&build_header(plugin_id));

    // The #967 shape: the page scrolls inside a bounded card instead of growing
    // the surface. `hscrollbar_policy = Never` propagates the child's minimum
    // width through (the clamp caps the *natural*), so a page with a wide
    // minimum is still readable rather than silently clipped.
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_width(true)
        .propagate_natural_height(true)
        .max_content_height(scale(DIALOG_MAX_HEIGHT))
        .child(body)
        .build();
    column.append(&scroller);

    let clamp = adw::Clamp::builder()
        .maximum_size(scale(DIALOG_MAX_WIDTH))
        .tightening_threshold(scale(DIALOG_MAX_WIDTH))
        .child(&column)
        .build();
    // Centered on the fullscreen surface — the whole point of the overlay
    // (`Align::Center` on both axes, where the drawer aligns to the bar's
    // corner). `Align::Center` also stops the card from expanding to fill.
    clamp.set_halign(gtk::Align::Center);
    clamp.set_valign(gtk::Align::Center);
    clamp.upcast()
}

/// The header: the plugin **id**, dim, and a close button.
///
/// The id and not a display name, because there is no display name to use: the
/// manifest carries none and `SlotRender` does not either, and adding one is a
/// proto change this cut rules out (§9). The plugin's own panel tree carries its
/// title. Annika took the default on §8 (2026-09-17): *"default 🤷‍♀️"* — the
/// plugin id, dim.
fn build_header(plugin_id: &str) -> gtk::Box {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.add_css_class("ts-dialog-header");

    let title = gtk::Label::new(Some(plugin_id));
    title.add_css_class("ts-dialog-title");
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    header.append(&title);

    let close_btn = gtk::Button::from_icon_name("window-close-symbolic");
    close_btn.add_css_class("flat");
    close_btn.add_css_class("circular");
    close_btn.set_tooltip_text(Some("Close"));
    close_btn.connect_clicked(|_| close());
    header.append(&close_btn);

    header
}

/// Retitle the live dialog's header in place, for the swap-the-selection path.
///
/// Walks to the label rather than stashing a widget handle in the thread-local:
/// the alternative is a second strong clone of a widget the window already owns,
/// living for as long as the dialog does — the pin shape `nix/lint-bind-pins.py`
/// exists to catch. The walk is a fixed path over a tree this file builds three
/// functions up, and it is inert (no retitle, one `debug!`) if any hop fails.
fn set_title(plugin_id: &str) {
    let label = DIALOG.with(|d| {
        d.borrow()
            .as_ref()
            .and_then(|live| live.window.child())
            .and_then(|root| root.downcast::<gtk::Overlay>().ok())
            .and_then(|overlay| overlay.last_child())
            .and_then(|clamp| clamp.downcast::<adw::Clamp>().ok())
            .and_then(|clamp| clamp.child())
            .and_then(|column| column.first_child())
            .and_then(|header| header.first_child())
            .and_then(|title| title.downcast::<gtk::Label>().ok())
    });
    if let Some(label) = label {
        label.set_text(plugin_id);
    } else {
        tracing::debug!(
            plugin = %plugin_id,
            "plugin dialog: header label not found; title left as it was"
        );
    }
}

// ── GTK integration tests (need a display → gated to `system-tests`) ─────────

/// The dialog's **shape**, driven through the same builders `show` mounts.
///
/// What is *not* here, and why: the layer-shell window itself. Building one
/// calls `gtk_layer_init_for_window`, which requires a Wayland compositor
/// speaking `zwlr_layer_shell_v1` — under `xvfb` (what CI has) it is a hard
/// error, not a degraded window. Nothing anywhere in this tree constructs a
/// layer surface in a test for that reason. So `show`'s four window properties
/// (`Layer::Overlay`, `KeyboardMode::Exclusive`, the namespace, the four
/// anchors) are **live-verify** items — `docs/live-verify.md`'s #1010 section
/// names them — and everything below the window is pinned here.
#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use super::{DIALOG, LiveDialog, build_root, close, is_open, set_title, wire_escape};
    use hytte::adw::{self, prelude::*};
    use hytte::gtk::glib::translate::IntoGlib;
    use hytte::gtk::{self, gdk};

    /// A stand-in for the plugin panel slot: the real one `.expect()`s a
    /// registered `PluginHandles`, which a `#[gtk::test]` has no booted `App`
    /// to provide (the `build_panel_child` split's argument, one level up).
    fn body() -> gtk::Label {
        gtk::Label::new(Some("page"))
    }

    /// The drawer's window shape, reproduced: the `gtk::Overlay`'s **main**
    /// child is the transparent catcher and its **overlay** child is the card.
    /// That ordering is the whole no-dimming mechanism — GTK paints overlay
    /// children above the main child, so the card is on top of a catcher that
    /// paints nothing, and no cross-surface restacking is asked of niri.
    ///
    /// **Falsification:** swap the two (`set_child` the card,
    /// `add_overlay` the catcher) → the catcher would paint over the page; the
    /// class assertions below go red.
    #[gtk::test]
    fn the_catcher_is_the_main_child_and_the_card_the_overlay_child() {
        adw::init().expect("libadwaita init");
        let root = build_root("demo", &body());

        let main = root.child().expect("an Overlay main child");
        assert!(
            main.has_css_class("ts-modal-catcher"),
            "the main child must be the transparent click-catcher"
        );

        let card = root.last_child().expect("an overlay child");
        assert!(
            card.downcast_ref::<adw::Clamp>().is_some(),
            "the overlay child must be the clamped card"
        );
    }

    /// The header is the plugin **id**, dim, plus a close button — §8's default,
    /// and the reason the id is not a display name is that there is none on the
    /// wire (§9).
    #[gtk::test]
    fn the_header_carries_the_plugin_id_and_a_close_button() {
        adw::init().expect("libadwaita init");
        let root = build_root("hytte-plugin-agents", &body());

        let header = header_of(&root);
        let title = header
            .first_child()
            .and_then(|w| w.downcast::<gtk::Label>().ok())
            .expect("a title label");
        assert_eq!(title.text(), "hytte-plugin-agents");
        assert!(title.has_css_class("ts-dialog-title"));

        let close_btn = header
            .last_child()
            .and_then(|w| w.downcast::<gtk::Button>().ok())
            .expect("a close button");
        assert!(
            close_btn.has_css_class("flat"),
            "the close button is the flat/circular header shape"
        );
    }

    /// Opening plugin Y while X is shown **swaps the content**: the header
    /// retitles in place and no second window is built.
    ///
    /// Drives [`set_title`] against a hand-built `LiveDialog` — the swap path
    /// `open_on_focused` takes when the focused output already has the dialog —
    /// because `open_on_focused` itself needs a layer surface.
    ///
    /// **Falsification:** make `set_title` a no-op → the second assertion reds.
    #[gtk::test]
    fn opening_a_second_plugin_retitles_in_place() {
        adw::init().expect("libadwaita init");
        let window = gtk::Window::new();
        window.set_child(Some(&build_root("first", &body())));
        DIALOG.with(|d| {
            *d.borrow_mut() = Some(LiveDialog {
                window: window.clone(),
                connector: "DP-1".to_owned(),
            });
        });

        set_title("second");

        let root = window
            .child()
            .and_then(|w| w.downcast::<gtk::Overlay>().ok())
            .expect("the dialog root");
        let title = header_of(&root)
            .first_child()
            .and_then(|w| w.downcast::<gtk::Label>().ok())
            .expect("a title label");
        assert_eq!(
            title.text(),
            "second",
            "a second open on the same output must retitle the live card, not build a window"
        );

        // Leave the thread-local clean for the next test on this thread —
        // `close()` reaches the plugin registry, which a `#[gtk::test]` has no
        // booted `App` for, so the window is dropped by hand.
        DIALOG.with(|d| *d.borrow_mut() = None);
        window.destroy();
    }

    /// [`close`] over an empty slot touches nothing and — crucially — does not
    /// reach the plugin registry, which is what makes the `dialog-close`
    /// `GAction` safe to bind blind and what keeps a stray `Esc` from panicking
    /// a shell whose dialog is already down.
    ///
    /// **Falsification:** drop the `was_open` guard in `close` so it always
    /// publishes → this panics on the unregistered `PluginHandles`.
    #[gtk::test]
    fn closing_with_nothing_up_is_inert() {
        adw::init().expect("libadwaita init");
        DIALOG.with(|d| *d.borrow_mut() = None);
        close();
        assert!(!is_open());
    }

    /// `Esc` dismisses and **stops** the press; every other key proceeds into
    /// the page, so a plugin `Entry` inside it still receives what is typed.
    ///
    /// Drives the shipped closure, not a copy of it: [`wire_escape`] is wired
    /// onto a plain `gtk::Window` and the controller's own `key-pressed` signal
    /// is emitted, which is the only way to reach a key handler without a
    /// compositor delivering a real event. `close()` runs for real on the
    /// `Escape` arm and is inert with nothing up
    /// (`closing_with_nothing_up_is_inert`), so no registry is needed.
    ///
    /// **Falsification:** make `dismisses` always `false` (Escape a no-op), or
    /// drop the `add_controller` call → the first assertion reds.
    #[gtk::test]
    fn escape_dismisses_and_every_other_key_reaches_the_page() {
        adw::init().expect("libadwaita init");
        DIALOG.with(|d| *d.borrow_mut() = None);

        let window = gtk::Window::new();
        wire_escape(&window);

        let controller = window
            .observe_controllers()
            .into_iter()
            .flatten()
            .find_map(|c| c.downcast::<gtk::EventControllerKey>().ok())
            .expect("wire_escape must leave a key controller on the window");

        assert!(
            press(&controller, gdk::Key::Escape),
            "Escape must dismiss the dialog and stop the press"
        );
        assert!(
            !press(&controller, gdk::Key::Return),
            "every other key must proceed into the plugin's page"
        );

        window.destroy();
    }

    /// Emit one `key-pressed` on `controller`, returning whether the handler
    /// claimed it (`Propagation::Stop`).
    fn press(controller: &gtk::EventControllerKey, key: gdk::Key) -> bool {
        controller.emit_by_name::<bool>(
            "key-pressed",
            &[&key.into_glib(), &0u32, &gdk::ModifierType::empty()],
        )
    }

    /// The header box inside a built root.
    fn header_of(root: &gtk::Overlay) -> gtk::Widget {
        root.last_child()
            .and_then(|w| w.downcast::<adw::Clamp>().ok())
            .and_then(|clamp| clamp.child())
            .and_then(|column| column.first_child())
            .expect("header row")
    }
}
