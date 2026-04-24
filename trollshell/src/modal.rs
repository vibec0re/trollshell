use std::cell::RefCell;
use std::collections::HashMap;

use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::ui::{layer_window, Anchor, Margin};

use crate::widgets::pages;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Page {
    Media,
    Network,
    Bluetooth,
    Stats,
    Audio,
    Power,
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
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Media => "Media",
            Self::Network => "Network",
            Self::Bluetooth => "Bluetooth",
            Self::Stats => "System",
            Self::Audio => "Audio",
            Self::Power => "Power",
        }
    }
}

/// Per-monitor modal panel handle. Internally owns the layer-shell window.
struct ModalPanel {
    window: gtk::Window,
    stack: gtk::Stack,
    title_label: gtk::Label,
    /// Which page is currently visible; None means the window is hidden.
    current: RefCell<Option<Page>>,
}

thread_local! {
    static PANELS: RefCell<HashMap<String, ModalPanel>> = RefCell::new(HashMap::new());
}

fn monitor_key(m: &Monitor) -> String {
    m.connector()
        .unwrap_or_else(|| format!("monitor:{:p}", m.gdk()))
}

/// Build the modal for one monitor and mount it as a layer-shell window.
pub fn install(monitor: &Monitor) {
    let key = monitor_key(monitor);

    let window = layer_window(monitor)
        .anchor(Anchor::Top)
        .anchor(Anchor::Right)
        .margin(Margin {
            top: 8,
            right: 8,
            bottom: 0,
            left: 0,
        })
        .exclusive(false)
        .keyboard_mode(KeyboardMode::OnDemand)
        .namespace(format!("hytte-modal-{key}"))
        .build();
    window.add_css_class("ts-modal");
    window.set_size_request(420, 520);

    // ESC → close.
    let key_ctrl = gtk::EventControllerKey::new();
    let window_for_esc = window.downgrade();
    key_ctrl.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            if let Some(w) = window_for_esc.upgrade() {
                w.set_visible(false);
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_ctrl);

    // Root vertical box.
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    vbox.add_css_class("ts-modal-root");

    // Header: title label + close button.
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.add_css_class("ts-modal-header");

    let title_label = gtk::Label::new(Some("\u{2014}"));
    title_label.add_css_class("ts-modal-title");
    title_label.set_xalign(0.0);
    title_label.set_hexpand(true);
    header.append(&title_label);

    let close_btn = gtk::Button::from_icon_name("window-close-symbolic");
    close_btn.add_css_class("ts-modal-close");
    let key_for_close = key.clone();
    close_btn.connect_clicked(move |_| close_by_key(&key_for_close));
    header.append(&close_btn);

    vbox.append(&header);

    // Page stack.
    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(120);

    stack.add_titled(
        &pages::page_media(),
        Some(Page::Media.stack_name()),
        Page::Media.title(),
    );
    stack.add_titled(
        &pages::page_network(),
        Some(Page::Network.stack_name()),
        Page::Network.title(),
    );
    stack.add_titled(
        &pages::page_bluetooth(),
        Some(Page::Bluetooth.stack_name()),
        Page::Bluetooth.title(),
    );
    stack.add_titled(
        &pages::page_stats(),
        Some(Page::Stats.stack_name()),
        Page::Stats.title(),
    );
    stack.add_titled(
        &pages::page_audio(),
        Some(Page::Audio.stack_name()),
        Page::Audio.title(),
    );
    stack.add_titled(
        &pages::page_power(),
        Some(Page::Power.stack_name()),
        Page::Power.title(),
    );

    vbox.append(&stack);
    window.set_child(Some(&vbox));

    // Initially hidden.
    window.set_visible(false);

    // When hidden (via any path), clear the current page so the next toggle
    // re-opens rather than thinking a page is already visible.
    let key_for_hide = key.clone();
    window.connect_hide(move |_| {
        PANELS.with(|panels| {
            if let Some(panel) = panels.borrow().get(&key_for_hide) {
                *panel.current.borrow_mut() = None;
            }
        });
    });

    PANELS.with(|panels| {
        panels.borrow_mut().insert(
            key.clone(),
            ModalPanel {
                window,
                stack,
                title_label,
                current: RefCell::new(None),
            },
        );
    });
}

/// Close and remove the modal for a monitor that has been unplugged.
#[allow(dead_code)]
pub fn uninstall(monitor: &Monitor) {
    let key = monitor_key(monitor);
    PANELS.with(|panels| {
        if let Some(panel) = panels.borrow_mut().remove(&key) {
            panel.window.close();
        }
    });
}

/// Close and remove all modals (called before rebuilding bars on hot-plug).
pub fn close_all() {
    PANELS.with(|panels| {
        for (_, panel) in panels.borrow_mut().drain() {
            panel.window.close();
        }
    });
}

fn close_by_key(key: &str) {
    PANELS.with(|panels| {
        if let Some(panel) = panels.borrow().get(key) {
            panel.window.set_visible(false);
        }
    });
}

#[allow(dead_code)]
pub fn close(monitor: &Monitor) {
    close_by_key(&monitor_key(monitor));
}

#[allow(dead_code)]
pub fn open(monitor: &Monitor, page: Page) {
    let key = monitor_key(monitor);
    PANELS.with(|panels| {
        let panels = panels.borrow();
        let Some(panel) = panels.get(&key) else {
            return;
        };
        panel.stack.set_visible_child_name(page.stack_name());
        panel.title_label.set_text(page.title());
        *panel.current.borrow_mut() = Some(page);
        panel.window.set_visible(true);
        panel.window.present();
    });
}

/// Toggle the modal on `monitor` to the given `page`:
/// - Same page open → close.
/// - Different page open → switch to target page.
/// - Closed → open on target page.
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
                // Same page — close.
                panel.window.set_visible(false);
            }
            Some(_) => {
                // Different page — switch.
                panel.stack.set_visible_child_name(page.stack_name());
                panel.title_label.set_text(page.title());
                *panel.current.borrow_mut() = Some(page);
            }
            None => {
                // Closed → open.
                panel.stack.set_visible_child_name(page.stack_name());
                panel.title_label.set_text(page.title());
                *panel.current.borrow_mut() = Some(page);
                panel.window.set_visible(true);
                panel.window.present();
            }
        }
    });
}
