use std::cell::RefCell;
use std::collections::HashMap;

use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::calendar;
use hytte::services::clipboard;
use hytte::ui::{layer_window, Anchor, Layer, LayerShell, Margin};

use crate::widgets::pages;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Page {
    Media,
    Network,
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

    // Drawer and bar both live on `Layer::Top`; the drawer butts flush up
    // against the bar's bottom (no overlap) so there's no z-order conflict
    // at the seam to worry about. Bar stays on the default Top layer so
    // fullscreen apps can still cover it.
    let window = layer_window(monitor)
        .layer(Layer::Top)
        .anchor(Anchor::Top)
        .anchor(Anchor::Right)
        .margin(Margin {
            top: 49,
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
    // page caps width at 680 (see `pages::finish_page`); height is the
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

    stack.add_named(&pages::page_media(), Some(Page::Media.stack_name()));
    stack.add_named(&pages::page_network(), Some(Page::Network.stack_name()));
    stack.add_named(&pages::page_bluetooth(), Some(Page::Bluetooth.stack_name()));
    stack.add_named(&pages::page_stats(), Some(Page::Stats.stack_name()));
    stack.add_named(&pages::page_audio(), Some(Page::Audio.stack_name()));
    stack.add_named(&pages::page_power(), Some(Page::Power.stack_name()));
    stack.add_named(
        &pages::page_power_menu(),
        Some(Page::PowerMenu.stack_name()),
    );
    stack.add_named(
        &pages::page_notifications(),
        Some(Page::Notifications.stack_name()),
    );
    stack.add_named(
        &pages::page_appearance(),
        Some(Page::Appearance.stack_name()),
    );
    stack.add_named(
        &pages::page_displays(),
        Some(Page::Displays.stack_name()),
    );
    stack.add_named(
        &pages::page_clipboard(),
        Some(Page::Clipboard.stack_name()),
    );
    stack.add_named(
        &pages::page_calendar(),
        Some(Page::Calendar.stack_name()),
    );
    stack.add_named(
        &pages::page_settings(),
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
                open_state: Mutable::new(false),
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
        show_panel(panel, &key, page);
    });
}

/// Toggle the drawer on `monitor` to the given `page`:
/// - Same page open → start retract.
/// - Different page open → swap stack child in place (crossfade + height).
/// - Closed → present surface and reveal.
pub fn toggle(monitor: &Monitor, page: Page) {
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
                show_panel(panel, &key, page);
            }
        }
    });
}

/// Present the drawer on `page`. Show the catcher before the drawer so
/// that within `Layer::Top` the drawer's surface commits most recently
/// and stacks above the catcher.
fn show_panel(panel: &ModalPanel, _key: &str, page: Page) {
    panel.stack.set_visible_child_name(page.stack_name());
    *panel.current.borrow_mut() = Some(page);
    panel.open_state.set(true);
    on_page_show(page);

    panel.catcher.set_visible(true);
    panel.catcher.present();

    panel.window.set_visible(true);
    panel.window.present();
    panel.revealer.set_reveal_child(true);
}

/// Signal that emits `true` while the drawer on `monitor` is open (the
/// retract animation hasn't completed yet), and `false` when it's closed.
/// Returns `None` if `install` hasn't been called for this monitor yet.
pub fn drawer_open_signal(monitor: &Monitor) -> Option<impl Signal<Item = bool> + 'static> {
    let key = monitor_key(monitor);
    PANELS.with(|panels| {
        panels
            .borrow()
            .get(&key)
            .map(|panel| panel.open_state.signal())
    })
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
        _ => {}
    }
}
