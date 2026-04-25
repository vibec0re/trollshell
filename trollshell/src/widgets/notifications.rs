//! Layer-shell toast window for `org.freedesktop.Notifications`.
//!
//! Call [`install`] once after GTK initialises (before the main loop runs).
//! It builds a single `gtk::Window` pinned to the top-right corner of the
//! given monitor and subscribes to [`hytte::services::notifications::active`].
//!
//! The window is stored in a thread-local so it is never dropped.  Callers
//! do not hold a handle; the window stays alive for the process lifetime.
//!
//! # Do-Not-Disturb gating
//!
//! When [`hytte::services::dnd::enabled()`] is true, non-critical toasts are
//! suppressed at the bind site — the drawer history page still records them
//! via the underlying notifications service. Critical-urgency notifications
//! (`urgency=2`) BYPASS DND per freedesktop spec.
//!
//! # Per-app mute
//!
//! When an `app_name` is in [`notifications_mute::muted_apps()`], non-critical
//! toasts from that app are suppressed before they're shown (history still
//! records them). Critical urgency bypasses the per-app mute too. Like DND,
//! this is a "from-now-forward" gate — toggling an app's mute off does NOT
//! revive previously suppressed toasts.
//!
//! # Queue cap
//!
//! TODO(notif-followup): when bursts produce 5+ active toasts, show only the
//! latest 3 plus a synthetic "+N more" card that opens the drawer's
//! Notifications page on click. The current implementation renders one card
//! per active notification — fine for steady-state but visually noisy under
//! a fast burst. The notifications service itself does not queue; it tracks
//! the live set, so any cap is consumer-side.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use hytte::futures_signals::map_ref;
use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::dnd;
use hytte::services::notifications::{self, Notification, NotificationImage, Urgency};
use hytte::services::notifications_mute;
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

    // `suppressed_during_dnd` records notification ids that arrived while
    // DND was on. Toggling DND off does NOT revive them — DND is a
    // "from-now-forward" gate. Entries are dropped when the upstream
    // notification leaves the active list (dismissed or expired).
    let suppressed_during_dnd: RefCell<HashSet<u32>> = RefCell::new(HashSet::new());

    // Weak refs so the signal closure doesn't hold the window alive (the
    // thread-local owns it).
    let window_weak = window.downgrade();
    let vbox_weak = vbox.downgrade();

    // Combine the live notifications with the DND flag and the per-app mute
    // set. The visibility filter — "critical bypasses DND + mute, anything
    // suppressed once stays suppressed even after the gate flips off" —
    // runs inside the `for_each` callback below, where the suppressed-set
    // state lives.
    let toast_signal = map_ref! {
        let notifs = notifications::active(),
        let dnd_on = dnd::enabled(),
        let muted = notifications_mute::muted_apps() => {
            (notifs.clone(), *dnd_on, muted.clone())
        }
    };

    glib::MainContext::default().spawn_local(
        toast_signal
            .for_each(move |(notifs, dnd_on, muted): (Vec<Notification>, bool, HashSet<String>)| {
                let Some(window) = window_weak.upgrade() else {
                    return std::future::ready(());
                };
                let Some(vbox) = vbox_weak.upgrade() else {
                    return std::future::ready(());
                };

                let mut map = card_map.borrow_mut();
                let mut suppressed = suppressed_during_dnd.borrow_mut();

                // Apply DND + per-app-mute gates. Critical urgency always
                // shows and never touches the suppressed set. Non-critical
                // notifications that arrive while DND is on, or whose
                // app_name is in the muted set, are recorded in
                // `suppressed` and stay hidden even after DND is toggled
                // off / the app is unmuted — flipping a gate off must NOT
                // unleash the backlog.
                let visible: Vec<&Notification> = notifs
                    .iter()
                    .filter(|n| {
                        if n.urgency == Urgency::Critical {
                            return true;
                        }
                        if suppressed.contains(&n.id) {
                            return false;
                        }
                        if dnd_on || muted.contains(&n.app_name) {
                            suppressed.insert(n.id);
                            return false;
                        }
                        true
                    })
                    .collect();

                // GC the suppressed set: drop any ids whose notifications
                // are no longer in the upstream active list (dismissed or
                // expired). Once gone upstream, we'll never need to suppress
                // them again. O(N) per emit.
                let active_ids: HashSet<u32> = notifs.iter().map(|n| n.id).collect();
                suppressed.retain(|id| active_ids.contains(id));

                // Build id sets.
                let new_ids: HashMap<u32, &Notification> =
                    visible.iter().map(|n| (n.id, *n)).collect();
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
    // Outer card: horizontal — [image?] [text column]
    let card = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    card.add_css_class("ts-toast");

    let urgency_class = match notif.urgency {
        Urgency::Low => "ts-toast-low",
        Urgency::Normal => "ts-toast-normal",
        Urgency::Critical => "ts-toast-critical",
    };
    card.add_css_class(urgency_class);

    // Left column: thumbnail image (optional).
    if let Some(image) = &notif.image {
        let img = build_image(image);
        img.add_css_class("ts-toast-image");
        img.set_tooltip_text(Some(&notif.app_name));
        card.append(&img);
    }

    // Right column: app name header + summary + body + actions.
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.set_hexpand(true);

    // Header row: small app icon + app name.
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);

    if notif.image.is_none() && !notif.app_icon.is_empty() {
        // Only show the small header icon when there is no thumbnail.
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

    column.append(&header);

    // Summary.
    if !notif.summary.is_empty() {
        let summary = gtk::Label::new(Some(&notif.summary));
        summary.add_css_class("ts-toast-summary");
        summary.set_xalign(0.0);
        summary.set_wrap(true);
        summary.set_max_width_chars(40);
        column.append(&summary);
    }

    // Body.
    if !notif.body.is_empty() {
        let body = gtk::Label::new(Some(&notif.body));
        body.add_css_class("ts-toast-body");
        body.set_xalign(0.0);
        body.set_wrap(true);
        body.set_max_width_chars(40);
        column.append(&body);
    }

    // Action buttons (rendered only when actions are present). Cap at 3
    // visible buttons so a chatty app (e.g. an "snooze 1m / 5m / 15m / 1h"
    // calendar reminder) can't blow out the toast width — the rest stay
    // accessible from the drawer history page.
    if !notif.actions.is_empty() {
        let actions_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        actions_row.add_css_class("ts-toast-actions");
        for action in notif.actions.iter().take(3) {
            let btn = gtk::Button::with_label(&action.label);
            btn.add_css_class("ts-toast-action");
            let id = notif.id;
            let key = action.key.clone();
            btn.connect_clicked(move |_| {
                notifications::invoke_action(id, &key);
                notifications::dismiss(id, 2);
            });
            actions_row.append(&btn);
        }
        column.append(&actions_row);
    }

    card.append(&column);

    // Click anywhere on the card → dismiss (reason 2 = dismissed by user).
    // Action buttons consume their own click events before it bubbles here.
    let id = notif.id;
    let gesture = gtk::GestureClick::new();
    gesture.connect_pressed(move |gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        notifications::dismiss(id, 2);
    });
    card.add_controller(gesture);

    card.upcast()
}

// ── Image widget builder ──────────────────────────────────────────────────────

fn build_image(image: &NotificationImage) -> gtk::Image {
    let img = gtk::Image::new();
    img.set_pixel_size(48);
    match image {
        NotificationImage::Raw {
            width,
            height,
            rowstride,
            has_alpha,
            data,
            ..
        } => {
            let format = if *has_alpha {
                gdk::MemoryFormat::R8g8b8a8
            } else {
                gdk::MemoryFormat::R8g8b8
            };
            let bytes = gdk::glib::Bytes::from(data.as_slice());
            let stride = usize::try_from(*rowstride).unwrap_or(0);
            let texture = gdk::MemoryTexture::new(*width, *height, format, &bytes, stride);
            img.set_paintable(Some(texture.upcast_ref::<gdk::Paintable>()));
        }
        NotificationImage::Path(path_or_url) => {
            let path_str = path_or_url
                .strip_prefix("file://")
                .unwrap_or(path_or_url.as_str());
            match gdk::Texture::from_filename(path_str) {
                Ok(texture) => {
                    img.set_paintable(Some(texture.upcast_ref::<gdk::Paintable>()));
                }
                Err(_) => {
                    img.set_icon_name(Some("dialog-information-symbolic"));
                }
            }
        }
        NotificationImage::IconName(name) => {
            img.set_icon_name(Some(name));
        }
    }
    img
}
