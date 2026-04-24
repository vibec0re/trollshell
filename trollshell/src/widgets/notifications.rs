//! Layer-shell toast window for `org.freedesktop.Notifications`.
//!
//! Call [`install`] once after GTK initialises (before the main loop runs).
//! It builds a single `gtk::Window` pinned to the top-right corner of the
//! given monitor and subscribes to [`hytte::services::notifications::active`].
//!
//! The window is stored in a thread-local so it is never dropped.  Callers
//! do not hold a handle; the window stays alive for the process lifetime.

use std::cell::RefCell;
use std::collections::HashMap;

use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::notifications::{self, Notification, Urgency};
use hytte::ui::{layer_window, Anchor, Margin};

// ── Thread-local window storage ───────────────────────────────────────────────

thread_local! {
    static TOAST_WINDOW: RefCell<Option<gtk::Window>> = const { RefCell::new(None) };
}

// ── Public entry-point ────────────────────────────────────────────────────────

/// Build the toast layer-shell window for `monitor` and subscribe it to the
/// notifications active signal.  Idempotent: a second call on the same thread
/// replaces the stored window reference (harmless in practice — `install` is
/// called once).
pub fn install(monitor: &Monitor) {
    let window = layer_window(monitor)
        .layer(Layer::Top)
        .anchor(Anchor::Top)
        .anchor(Anchor::Right)
        .margin(Margin {
            top: 8,
            right: 8,
            bottom: 0,
            left: 0,
        })
        .namespace("hytte-toasts")
        .exclusive(false)
        .keyboard_mode(KeyboardMode::None)
        .build();

    // Container: vertically stacked cards.
    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
    vbox.add_css_class("ts-toasts");
    window.set_child(Some(&vbox));

    // Map of mounted card widgets keyed by notification id.
    let card_map: RefCell<HashMap<u32, gtk::Widget>> = RefCell::new(HashMap::new());

    // Weak refs so the signal closure doesn't hold the window alive (the
    // thread-local owns it).
    let window_weak = window.downgrade();
    let vbox_weak = vbox.downgrade();

    glib::MainContext::default().spawn_local(
        notifications::active()
            .for_each(move |notifs: Vec<Notification>| {
                let Some(window) = window_weak.upgrade() else {
                    return std::future::ready(());
                };
                let Some(vbox) = vbox_weak.upgrade() else {
                    return std::future::ready(());
                };

                let mut map = card_map.borrow_mut();

                // Build id sets.
                let new_ids: HashMap<u32, &Notification> =
                    notifs.iter().map(|n| (n.id, n)).collect();
                let old_ids: Vec<u32> = map.keys().copied().collect();

                // Remove cards whose notifications have gone.
                for id in &old_ids {
                    if !new_ids.contains_key(id) && let Some(card) = map.remove(id) {
                        vbox.remove(&card);
                    }
                }

                // Add or rebuild cards for each notification.
                for (id, notif) in &new_ids {
                    // For v0.4.0 we rebuild on replaces_id updates (same id,
                    // new content) — drop the old card and build fresh.
                    if let Some(old_card) = map.remove(id) {
                        vbox.remove(&old_card);
                    }
                    let card = build_card(notif);
                    vbox.append(&card);
                    map.insert(*id, card);
                }

                // Show/hide window based on whether any notifications are active.
                window.set_visible(!map.is_empty());

                std::future::ready(())
            }),
    );

    TOAST_WINDOW.with(|cell| {
        *cell.borrow_mut() = Some(window);
    });
}

// ── Card builder ──────────────────────────────────────────────────────────────

fn build_card(notif: &Notification) -> gtk::Widget {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 4);
    card.add_css_class("ts-toast");

    let urgency_class = match notif.urgency {
        Urgency::Low => "ts-toast-low",
        Urgency::Normal => "ts-toast-normal",
        Urgency::Critical => "ts-toast-critical",
    };
    card.add_css_class(urgency_class);

    // Header row: icon + app name.
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);

    if !notif.app_icon.is_empty() {
        let icon = gtk::Image::from_icon_name(&notif.app_icon);
        icon.set_pixel_size(16);
        header.append(&icon);
    }

    let app_label = gtk::Label::new(Some(&notif.app_name));
    app_label.add_css_class("ts-toast-app");
    app_label.set_xalign(0.0);
    app_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    app_label.set_hexpand(true);
    header.append(&app_label);

    card.append(&header);

    // Summary.
    if !notif.summary.is_empty() {
        let summary = gtk::Label::new(Some(&notif.summary));
        summary.add_css_class("ts-toast-summary");
        summary.set_xalign(0.0);
        summary.set_wrap(true);
        summary.set_max_width_chars(40);
        card.append(&summary);
    }

    // Body.
    if !notif.body.is_empty() {
        let body = gtk::Label::new(Some(&notif.body));
        body.add_css_class("ts-toast-body");
        body.set_xalign(0.0);
        body.set_wrap(true);
        body.set_max_width_chars(40);
        card.append(&body);
    }

    // Click anywhere on the card → dismiss (reason 2 = dismissed by user).
    let id = notif.id;
    let gesture = gtk::GestureClick::new();
    gesture.connect_pressed(move |gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        notifications::dismiss(id, 2);
    });
    card.add_controller(gesture);

    card.upcast()
}
