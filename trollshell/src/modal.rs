use std::cell::RefCell;
use std::collections::HashMap;

use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::gtk::{self, gdk, glib, graphene, prelude::*};
use hytte::prelude::*;
use hytte::services::calendar;
use hytte::services::clipboard;
use hytte::services::notifications;
use hytte::ui::{layer_window, Anchor, Layer, LayerEdge, LayerShell, Margin};

/// Drawer's max content width (`AdwClamp.maximum_size` in `components::layout::finish_page`).
/// Used to clamp the per-trigger margin so the card never falls off-screen left.
const DRAWER_MAX_WIDTH: i32 = 680;


#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Page {
    Media,
    Network,
    Vpn,
    Connections,
    Bluetooth,
    Stats,
    Audio,
    Power,
    PowerMenu,
    Notifications,
    Appearance,
    Displays,
    Clipboard,
    Calendar,
    Settings,
}

impl Page {
    fn stack_name(self) -> &'static str {
        match self {
            Self::Media => "media",
            Self::Network => "network",
            Self::Vpn => "vpn",
            Self::Connections => "connections",
            Self::Bluetooth => "bluetooth",
            Self::Stats => "stats",
            Self::Audio => "audio",
            Self::Power => "power",
            Self::PowerMenu => "power-menu",
            Self::Notifications => "notifications",
            Self::Appearance => "appearance",
            Self::Displays => "displays",
            Self::Clipboard => "clipboard",
            Self::Calendar => "calendar",
            Self::Settings => "settings",
        }
    }
}

/// Per-monitor drawer handle. Internally owns the layer-shell window, a
/// `GtkRevealer` that slides the card out of the bar's bottom edge, and a
/// persistent fullscreen click-catcher that's toggled alongside the drawer.
struct ModalPanel {
    window: gtk::Window,
    revealer: gtk::Revealer,
    stack: gtk::Stack,
    current: RefCell<Option<Page>>,
    #[allow(dead_code)]
    monitor: Monitor,
    catcher: gtk::Window,
    /// Emits `true` while the drawer is open (between `show_panel` and the
    /// retract animation finishing). Consumers — e.g. the bar — bind CSS
    /// classes to this so the seam between bar and drawer can restyle.
    open_state: Mutable<bool>,
}

thread_local! {
    static PANELS: RefCell<HashMap<String, ModalPanel>> = RefCell::new(HashMap::new());
    /// Per-connector drawer-open state, decoupled from `ModalPanel` lifetime
    /// so subscribers (OSD, bar CSS) can wire up before `install` runs and
    /// survive bar rebuilds on hot-plug.
    static DRAWER_OPEN: RefCell<HashMap<String, Mutable<bool>>> = RefCell::new(HashMap::new());
}

fn drawer_open_state(key: &str) -> Mutable<bool> {
    DRAWER_OPEN.with(|map| {
        map.borrow_mut()
            .entry(key.to_string())
            .or_insert_with(|| Mutable::new(false))
            .clone()
    })
}

fn monitor_key(m: &Monitor) -> String {
    m.connector()
        .unwrap_or_else(|| format!("monitor:{:p}", m.gdk()))
}

/// Build the drawer for one monitor and mount it as a layer-shell window.
#[allow(clippy::too_many_lines)]
pub fn install(monitor: &Monitor) {
    let key = monitor_key(monitor);

    // Build the catcher FIRST so that within `Layer::Top` the catcher's
    // surface is committed before the drawer's. Within the same layer,
    // most Wayland compositors stack the most-recently-mapped surface on
    // top, so `show_panel` re-maps catcher → drawer on every open to keep
    // the drawer above its catcher.
    let catcher = build_catcher(monitor, key.clone());

    // Drawer and bar both live on `Layer::Top`. There's a 10 px gap below
    // the bar before the drawer starts, so the drawer reads as a separate
    // floating card rather than an extension of the bar. Bar stays on the
    // default Top layer so fullscreen apps can still cover it.
    let window = layer_window(monitor)
        .layer(Layer::Top)
        .anchor(Anchor::Top)
        .anchor(Anchor::Right)
        .margin(Margin {
            top: 59,
            right: 0,
            bottom: 0,
            left: 0,
        })
        .exclusive(false)
        .keyboard_mode(KeyboardMode::OnDemand)
        .namespace(format!("hytte-modal-{key}"))
        .build();
    window.add_css_class("ts-modal");
    // Ignore other layer-shell surfaces' exclusive zones so `margin.top` is
    // measured from the true screen edge, not from below the bar's reserved
    // zone. Without this, the bar's auto-exclusive-zone (≈59 px) stacks
    // with our margin and pushes the drawer ~60 px lower than intended.
    window.set_exclusive_zone(-1);
    // Content-driven sizing: the layer-shell surface auto-negotiates its
    // size from the visible page's natural request. AdwClamp inside each
    // page caps width at 680 (see `components::layout::finish_page`); height is the
    // page's natural height. The min-width floor (360) keeps very sparse
    // pages from collapsing to a sliver. niri honors the surface-size
    // commit when switching pages, so the modal grows/shrinks live.
    window.set_size_request(360, -1);

    // ESC → animated retract.
    let key_ctrl = gtk::EventControllerKey::new();
    let key_for_esc = key.clone();
    key_ctrl.connect_key_pressed(move |_, k, _, _| {
        if k == gdk::Key::Escape {
            retract_by_key(&key_for_esc);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_ctrl);

    // Revealer with SlideDown transition — the drawer "pulls out" of the
    // bar's bottom. Height animates automatically when the revealed child
    // (the stack) picks a different page with a different natural height.
    // `valign = Start` pins the revealer to the top of the 720-tall surface
    // so the card doesn't float in the middle of the transparent envelope.
    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    revealer.set_transition_duration(180);
    revealer.set_reveal_child(false);
    revealer.set_valign(gtk::Align::Start);
    revealer.set_vexpand(false);

    // Card — dark surface with rounded bottom corners.
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("ts-drawer");
    card.set_valign(gtk::Align::Start);
    card.set_vexpand(false);

    let stack = gtk::Stack::new();
    stack.set_vexpand(false);
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(140);
    stack.set_interpolate_size(true);
    // Disable homogeneous sizing (default true on both axes in GTK4) so the
    // stack reports the *visible* child's natural size, not the max across
    // all children. Without this, sparse pages (Calendar, PowerMenu) render
    // at the size of the largest mounted page (Stats / Audio).
    stack.set_hhomogeneous(false);
    stack.set_vhomogeneous(false);

    stack.add_named(&crate::panels::panel_media(), Some(Page::Media.stack_name()));
    stack.add_named(&crate::panels::panel_network(), Some(Page::Network.stack_name()));
    stack.add_named(&crate::panels::panel_vpn(), Some(Page::Vpn.stack_name()));
    stack.add_named(
        &crate::panels::panel_connections(),
        Some(Page::Connections.stack_name()),
    );
    stack.add_named(&crate::panels::panel_bluetooth(), Some(Page::Bluetooth.stack_name()));
    stack.add_named(&crate::panels::panel_stats(), Some(Page::Stats.stack_name()));
    stack.add_named(&crate::panels::panel_audio(), Some(Page::Audio.stack_name()));
    stack.add_named(&crate::panels::panel_power(), Some(Page::Power.stack_name()));
    stack.add_named(
        &crate::panels::panel_power_menu(),
        Some(Page::PowerMenu.stack_name()),
    );
    stack.add_named(
        &crate::panels::panel_notifications(),
        Some(Page::Notifications.stack_name()),
    );
    stack.add_named(
        &crate::panels::panel_appearance(),
        Some(Page::Appearance.stack_name()),
    );
    stack.add_named(
        &crate::panels::panel_displays(),
        Some(Page::Displays.stack_name()),
    );
    stack.add_named(
        &crate::panels::panel_clipboard(),
        Some(Page::Clipboard.stack_name()),
    );
    stack.add_named(
        &crate::panels::panel_calendar(),
        Some(Page::Calendar.stack_name()),
    );
    stack.add_named(
        &crate::panels::panel_settings(),
        Some(Page::Settings.stack_name()),
    );

    card.append(&stack);
    revealer.set_child(Some(&card));
    window.set_child(Some(&revealer));

    // When the retract animation finishes (child-revealed goes false),
    // hide both the drawer surface and the persistent catcher.
    let key_for_revealed = key.clone();
    revealer.connect_child_revealed_notify(move |r| {
        if !r.is_child_revealed() {
            PANELS.with(|panels| {
                if let Some(panel) = panels.borrow().get(&key_for_revealed) {
                    panel.window.set_visible(false);
                    panel.catcher.set_visible(false);
                    *panel.current.borrow_mut() = None;
                    panel.open_state.set(false);
                }
            });
        }
    });

    window.set_visible(false);

    PANELS.with(|panels| {
        panels.borrow_mut().insert(
            key.clone(),
            ModalPanel {
                window,
                revealer,
                stack,
                current: RefCell::new(None),
                monitor: monitor.clone(),
                catcher,
                open_state: drawer_open_state(&key),
            },
        );
    });
}

/// Close and remove the drawer for a monitor that has been unplugged.
#[allow(dead_code)]
pub fn uninstall(monitor: &Monitor) {
    let key = monitor_key(monitor);
    PANELS.with(|panels| {
        if let Some(panel) = panels.borrow_mut().remove(&key) {
            panel.catcher.close();
            panel.window.close();
        }
    });
}

/// Close and remove all drawers (called before rebuilding bars on hot-plug).
pub fn close_all() {
    PANELS.with(|panels| {
        for (_, panel) in panels.borrow_mut().drain() {
            panel.catcher.close();
            panel.window.close();
        }
    });
}

/// Swap every currently-open panel's visible page to `target`. Drawer pages
/// are built once, monitor-agnostically, so an in-page deep-link callback
/// (e.g. a Settings row → Wallpaper) doesn't have a handy `&Monitor`. This
/// helper walks the per-monitor panel set and:
/// - For each panel currently showing a page (`current.is_some()`), swaps
///   the stack child to `target` (crossfade + height) without retracting.
/// - For closed panels, does nothing — we only switch what's actually open.
///
/// If the user has no drawer open when this fires, the call is a no-op.
/// Future work could open `target` on the primary monitor in that case;
/// for v1 we treat the deep-link as "in this drawer, jump there".
pub fn switch_active(target: Page) {
    PANELS.with(|panels| {
        for panel in panels.borrow().values() {
            if panel.current.borrow().is_some() {
                panel.stack.set_visible_child_name(target.stack_name());
                *panel.current.borrow_mut() = Some(target);
                on_page_show(target);
            }
        }
    });
}

/// Begin the retract animation on every open drawer. Used by drawer-content
/// callbacks (e.g. the power-menu action rows) that don't carry a monitor
/// handle but want the drawer to close after their action fires.
pub fn dismiss_all() {
    PANELS.with(|panels| {
        for panel in panels.borrow().values() {
            panel.revealer.set_reveal_child(false);
        }
    });
}

/// Begin the retract animation. The notify-child-revealed handler finishes
/// the teardown (hiding the surface + closing the catcher) when it ends.
fn retract_by_key(key: &str) {
    PANELS.with(|panels| {
        if let Some(panel) = panels.borrow().get(key) {
            panel.revealer.set_reveal_child(false);
        }
    });
}

#[allow(dead_code)]
pub fn close(monitor: &Monitor) {
    retract_by_key(&monitor_key(monitor));
}

#[allow(dead_code)]
pub fn open(monitor: &Monitor, page: Page) {
    let key = monitor_key(monitor);
    PANELS.with(|panels| {
        let panels = panels.borrow();
        let Some(panel) = panels.get(&key) else {
            return;
        };
        // No bar chip context here (called from a notification toast click);
        // anchor the drawer flush with the screen's right edge.
        show_panel(panel, page, 0);
    });
}

/// Toggle the drawer on `monitor` to the given `page`, anchoring the
/// drawer's right edge under `trigger`'s right edge:
/// - Same page open → start retract.
/// - Different page open → swap stack child in place (crossfade + height);
///   the drawer surface keeps its existing position.
/// - Closed → reposition under `trigger`, present surface, reveal.
pub fn toggle(monitor: &Monitor, page: Page, trigger: &impl IsA<gtk::Widget>) {
    let key = monitor_key(monitor);
    PANELS.with(|panels| {
        let panels = panels.borrow();
        let Some(panel) = panels.get(&key) else {
            return;
        };
        let current = *panel.current.borrow();
        match current {
            Some(p) if p == page => {
                panel.revealer.set_reveal_child(false);
            }
            Some(_) => {
                panel.stack.set_visible_child_name(page.stack_name());
                *panel.current.borrow_mut() = Some(page);
                on_page_show(page);
            }
            None => {
                // Set the visible child first so `measure` reflects the
                // target page's natural width, not whatever was last shown.
                // `show_panel` re-sets it (idempotent).
                panel.stack.set_visible_child_name(page.stack_name());
                let margin_right =
                    margin_right_for_trigger(monitor, panel, trigger.upcast_ref());
                show_panel(panel, page, margin_right);
            }
        }
    });
}

/// Present the drawer on `page` at `margin_right` pixels from the screen's
/// right edge. Show the catcher before the drawer so that within
/// `Layer::Top` the drawer's surface commits most recently and stacks above
/// the catcher.
fn show_panel(panel: &ModalPanel, page: Page, margin_right: i32) {
    panel.stack.set_visible_child_name(page.stack_name());
    *panel.current.borrow_mut() = Some(page);
    panel.open_state.set(true);
    on_page_show(page);

    panel.window.set_margin(LayerEdge::Right, margin_right);

    panel.catcher.set_visible(true);
    panel.catcher.present();

    panel.window.set_visible(true);
    panel.window.present();
    panel.revealer.set_reveal_child(true);
}

/// Distance from the screen's right edge to where the drawer's right edge
/// should land so the drawer's horizontal center sits under `trigger`'s
/// center. Clamped to `[0, mon_w - drawer_width]` so the drawer can't fall
/// off either edge — for triggers near the screen's right or left edge
/// this collapses to flush-right or flush-left respectively.
///
/// Uses `panel.stack.measure(...)` so the offset matches the target page's
/// natural width; the caller must `set_visible_child_name` first.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn margin_right_for_trigger(
    monitor: &Monitor,
    panel: &ModalPanel,
    trigger: &gtk::Widget,
) -> i32 {
    let (mon_w, _) = monitor.size();
    let chip_center = trigger.root().and_then(|root| {
        let mid = graphene::Point::new(trigger.width() as f32 / 2.0, 0.0);
        trigger
            .compute_point(root.upcast_ref::<gtk::Widget>(), &mid)
            .map(|p| p.x() as i32)
    });
    let Some(chip_center) = chip_center else {
        return 0;
    };
    // Min-width floor matches `window.set_size_request(360, -1)` above; max
    // matches `AdwClamp.maximum_size` in `components::layout::finish_page`.
    // If `measure` returns 0 (unrealized on first open), clamp lifts to 360.
    let (_, nat_w, _, _) = panel.stack.measure(gtk::Orientation::Horizontal, -1);
    let drawer_w = nat_w.clamp(360, DRAWER_MAX_WIDTH);
    let desired = mon_w - chip_center - drawer_w / 2;
    let max = (mon_w - drawer_w).max(0);
    desired.clamp(0, max)
}

/// Signal that emits `true` while the drawer on `monitor` is open (the
/// retract animation hasn't completed yet), and `false` when it's closed.
/// Backed by [`DRAWER_OPEN`] so callers can subscribe before `install` has
/// run for this monitor — needed because the OSD and bar both wire up
/// during synchronous boot, while modal panels are built later inside the
/// `monitors_changed` task.
pub fn drawer_open_signal(monitor: &Monitor) -> impl Signal<Item = bool> + 'static {
    drawer_open_state(&monitor_key(monitor)).signal()
}

/// Full-screen transparent layer-shell window that closes the drawer on any
/// press. Built once at install time and kept alive for the panel's
/// lifetime; visibility tracks the drawer.
fn build_catcher(monitor: &Monitor, modal_key: String) -> gtk::Window {
    let win = layer_window(monitor)
        .layer(Layer::Top)
        .anchor(Anchor::Top)
        .anchor(Anchor::Bottom)
        .anchor(Anchor::Left)
        .anchor(Anchor::Right)
        .exclusive(false)
        .keyboard_mode(KeyboardMode::None)
        .namespace("hytte-modal-catcher")
        .build();
    win.add_css_class("ts-modal-catcher");

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content.set_hexpand(true);
    content.set_vexpand(true);
    win.set_child(Some(&content));

    let gesture = gtk::GestureClick::new();
    gesture.set_button(0);
    let modal_key_for_press = modal_key;
    gesture.connect_pressed(move |_, _, _, _| {
        retract_by_key(&modal_key_for_press);
    });
    content.add_controller(gesture);

    // Start hidden; `show_panel` toggles visibility alongside the drawer.
    win.set_visible(false);
    win
}

/// Per-page side-effects that should run whenever a page becomes visible
/// (initial open OR cross-fade swap from another page). Add a match arm
/// here when a new page needs an on-show fetch.
fn on_page_show(page: Page) {
    match page {
        Page::Clipboard => clipboard::refresh(),
        Page::Calendar => calendar::refresh(),
        // Opening the Notifications drawer = the user has seen them.
        // Dismiss all active toasts (move to history); the bell counter
        // bound to active.len() drops to zero.
        Page::Notifications => notifications::dismiss_all(),
        _ => {}
    }
}
