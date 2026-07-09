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
//! Up to [`MAX_VISIBLE_NONCRITICAL`] non-critical toasts render
//! individually. Additional non-critical toasts collapse into a
//! synthetic "+N more" card that opens the Notifications drawer on
//! click. Critical-urgency toasts always render individually and don't
//! count toward the cap. The notifications service itself does not
//! queue; it tracks the live set, so the cap is consumer-side only.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

use hytte::futures_signals::map_ref;
use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::dnd;
use hytte::services::niri;
use hytte::services::notifications::{self, Notification, NotificationImage, Urgency};
use hytte::services::notifications_mute;
use hytte::ui::{Anchor, Margin, layer_window};

use crate::components::notif_actions;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum number of non-critical toasts rendered as individual cards.
/// Additional non-critical notifications collapse into a synthetic
/// "+N more" overflow card. Critical-urgency toasts always render
/// individually and do not count toward this cap.
const MAX_VISIBLE_NONCRITICAL: usize = 4;

// ── Per-monitor toast view ────────────────────────────────────────────────────

struct ToastView {
    window: gtk::Window,
    vbox: gtk::Box,
    monitor: Monitor,
    card_map: RefCell<HashMap<u32, gtk::Widget>>,
    overflow_card: RefCell<Option<gtk::Widget>>,
    suppressed_during_dnd: RefCell<HashSet<u32>>,
}

// ── Thread-local window storage ───────────────────────────────────────────────

thread_local! {
    /// Mounted toast surfaces keyed by `Monitor.connector()`. Each
    /// entry owns its layer-shell window and the per-window state.
    static TOAST_WINDOWS: RefCell<HashMap<String, ToastView>> =
        RefCell::new(HashMap::new());

    /// Most recent focused-output name from
    /// [`hytte::services::niri::focused_output`]. Routes incoming
    /// notification batches to the matching window.
    static FOCUSED_OUTPUT: RefCell<Option<String>> = const { RefCell::new(None) };

    /// Set after the first `install()` call so module-level
    /// subscriptions wire exactly once across all per-monitor mounts.
    static SUBS_INSTALLED: Cell<bool> = const { Cell::new(false) };
}

// ── Public entry-point ────────────────────────────────────────────────────────

/// Build the toast layer-shell window for `monitor`, register it in
/// [`TOAST_WINDOWS`] keyed by the monitor's connector name, and lazily
/// install module-level subscriptions on the first call. Subsequent
/// calls register additional per-monitor surfaces; subscriptions wire
/// exactly once.
pub fn install(monitor: &Monitor) {
    let connector = match monitor.connector() {
        Some(c) if !c.is_empty() => c,
        _ => {
            tracing::debug!("notifications::install: monitor has no connector; skipping");
            return;
        }
    };

    let view = build_toast_view(monitor);
    TOAST_WINDOWS.with(|map| map.borrow_mut().insert(connector, view));

    if !SUBS_INSTALLED.with(Cell::get) {
        SUBS_INSTALLED.with(|c| c.set(true));
        install_subscriptions();
    }
}

/// Close every toast surface and drop the per-monitor entries. Called before
/// rebuilding on monitor hot-plug so a vanished output's `ToastView` doesn't
/// linger in [`TOAST_WINDOWS`] — otherwise `route_emission`'s
/// `map.values().next()` fallback could route every toast into a dead surface.
///
/// The module-level subscriptions (focused-output + the combined toast signal)
/// are left running: they route by connector on each emission, so a fresh
/// `install` re-keys the map and they self-heal. `SUBS_INSTALLED` therefore
/// stays set — subscriptions wire exactly once for the process lifetime.
pub fn close_all() {
    TOAST_WINDOWS.with(|map| {
        for (_, view) in map.borrow_mut().drain() {
            view.window.close();
        }
    });
}

// ── Subscriptions + routing ───────────────────────────────────────────────────

/// Wires the focused-output cell and the combined notification signal.
/// Runs exactly once across all per-monitor [`install`] calls (gated by
/// [`SUBS_INSTALLED`]).
fn install_subscriptions() {
    // niri::focused_output() → FOCUSED_OUTPUT
    glib::MainContext::default().spawn_local(niri::focused_output().for_each(|out| {
        FOCUSED_OUTPUT.with(|c| *c.borrow_mut() = out);
        std::future::ready(())
    }));

    // Combined (notifications, dnd, muted) signal → route_emission.
    let toast_signal = map_ref! {
        let notifs = notifications::active(),
        let dnd_on = dnd::enabled(),
        let muted = notifications_mute::muted_apps() => {
            (notifs.clone(), *dnd_on, muted.clone())
        }
    };
    glib::MainContext::default().spawn_local(toast_signal.for_each(
        |(notifs, dnd_on, muted): (Vec<Notification>, bool, HashSet<String>)| {
            route_emission(&notifs, dnd_on, &muted);
            std::future::ready(())
        },
    ));
}

/// Picks the [`ToastView`] on the focused output and forwards the
/// emission to [`apply_emission`]. Falls back to the first mounted view
/// when the focused output is unknown or absent from the map (covers
/// startup before the first niri focus event lands, and outputs that
/// trollshell hasn't mounted on).
fn route_emission(notifs: &[Notification], dnd_on: bool, muted: &HashSet<String>) {
    let target_name = FOCUSED_OUTPUT.with(|c| c.borrow().clone());
    TOAST_WINDOWS.with(|map| {
        let map = map.borrow();
        if map.is_empty() {
            return;
        }
        let view = target_name
            .as_ref()
            .and_then(|n| map.get(n))
            .or_else(|| map.values().next());
        if let Some(view) = view {
            apply_emission(view, notifs, dnd_on, muted);
        }
    });
}

/// Toast-management logic running against a single per-monitor view.
/// Applies DND + per-app-mute gates, GCs the suppressed set, partitions
/// critical vs non-critical, collapses the non-critical tail into an
/// overflow card, and toggles window visibility.
fn apply_emission(
    view: &ToastView,
    notifs: &[Notification],
    dnd_on: bool,
    muted: &HashSet<String>,
) {
    let mut map = view.card_map.borrow_mut();
    let mut suppressed = view.suppressed_during_dnd.borrow_mut();

    let visible = filter_visible(notifs, dnd_on, muted, &mut suppressed);
    gc_suppressed(notifs, &mut suppressed);
    let Partition {
        critical_visible,
        head_noncritical,
        tail_noncritical_count,
    } = partition_visible(&visible);

    // Build id sets.
    let new_ids: HashMap<u32, &Notification> = critical_visible
        .iter()
        .copied()
        .chain(head_noncritical.iter().copied())
        .map(|n| (n.id, n))
        .collect();
    let old_ids: Vec<u32> = map.keys().copied().collect();

    // Remove cards whose notifications have gone.
    for id in &old_ids {
        if !new_ids.contains_key(id)
            && let Some(card) = map.remove(id)
        {
            view.vbox.remove(&card);
        }
    }

    // Add or rebuild cards for each notification.
    for (id, notif) in &new_ids {
        // For v0.4.0 we rebuild on replaces_id updates (same id, new
        // content) — drop the old card and build fresh.
        if let Some(old_card) = map.remove(id) {
            view.vbox.remove(&old_card);
        }
        let card = build_card(notif);
        view.vbox.append(&card);
        map.insert(*id, card);
    }

    // Manage the overflow "+N more" card. Singleton, lives in
    // `view.overflow_card`. Removed when tail is empty; rebuilt when
    // tail count changes (so the label updates).
    {
        let mut slot = view.overflow_card.borrow_mut();
        if tail_noncritical_count == 0 {
            if let Some(card) = slot.take() {
                view.vbox.remove(&card);
            }
        } else {
            if let Some(card) = slot.take() {
                view.vbox.remove(&card);
            }
            let card = build_overflow_card(&view.monitor, tail_noncritical_count);
            view.vbox.append(&card);
            *slot = Some(card);
        }
    }

    // Show/hide window based on whether any cards are mounted.
    view.window
        .set_visible(!map.is_empty() || view.overflow_card.borrow().is_some());
}

struct Partition<'a> {
    critical_visible: Vec<&'a Notification>,
    head_noncritical: Vec<&'a Notification>,
    tail_noncritical_count: usize,
}

/// Apply DND + per-app-mute gates. Critical urgency always shows and never
/// touches the suppressed set. Non-critical notifications that arrive while
/// DND is on, or whose `app_name` is muted, are recorded in `suppressed` and
/// stay hidden even after the gate flips off — toggling DND off must NOT
/// unleash the backlog.
fn filter_visible<'a>(
    notifs: &'a [Notification],
    dnd_on: bool,
    muted: &HashSet<String>,
    suppressed: &mut HashSet<u32>,
) -> Vec<&'a Notification> {
    notifs
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
        .collect()
}

/// Drop suppressed ids whose notifications are no longer in the upstream
/// active list (dismissed or expired) — once gone upstream we'll never need
/// to suppress them again. O(N) per emit.
fn gc_suppressed(notifs: &[Notification], suppressed: &mut HashSet<u32>) {
    let active_ids: HashSet<u32> = notifs.iter().map(|n| n.id).collect();
    suppressed.retain(|id| active_ids.contains(id));
}

/// Split visible toasts into critical (always rendered individually) and the
/// non-critical head + collapsed tail count. Critical urgency never counts
/// toward the cap.
fn partition_visible<'a>(visible: &[&'a Notification]) -> Partition<'a> {
    let (critical_visible, noncritical_visible): (Vec<&Notification>, Vec<&Notification>) = visible
        .iter()
        .copied()
        .partition(|n| n.urgency == Urgency::Critical);
    let nc_head_start = noncritical_visible
        .len()
        .saturating_sub(MAX_VISIBLE_NONCRITICAL);
    let head_noncritical = noncritical_visible[nc_head_start..].to_vec();
    Partition {
        critical_visible,
        head_noncritical,
        tail_noncritical_count: nc_head_start,
    }
}

// ── Toast view constructor ────────────────────────────────────────────────────

fn build_toast_view(monitor: &Monitor) -> ToastView {
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

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
    vbox.add_css_class("ts-toasts");
    window.set_child(Some(&vbox));

    ToastView {
        window,
        vbox,
        monitor: monitor.clone(),
        card_map: RefCell::new(HashMap::new()),
        overflow_card: RefCell::new(None),
        suppressed_during_dnd: RefCell::new(HashSet::new()),
    }
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
        icon.set_pixel_size(crate::scale::scale(16));
        header.append(&icon);
    }

    let app_label = gtk::Label::new(Some(&notif.app_name));
    app_label.add_css_class("ts-toast-app");
    app_label.set_xalign(0.0);
    app_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    app_label.set_hexpand(true);
    header.append(&app_label);

    // Explicit dismiss button. Load-bearing (not just symmetry): once
    // body-click is wired to invoke a `default` action instead of
    // dismissing (below), a default-carrying toast would otherwise have no
    // way to close it without waiting out the timeout.
    let dismiss_btn = gtk::Button::from_icon_name("window-close-symbolic");
    dismiss_btn.add_css_class("flat");
    dismiss_btn.add_css_class("ts-toast-dismiss");
    dismiss_btn.set_valign(gtk::Align::Center);
    dismiss_btn.set_tooltip_text(Some("Dismiss"));
    let dismiss_id = notif.id;
    dismiss_btn.connect_clicked(move |_| {
        notifications::dismiss(dismiss_id, 2);
    });
    header.append(&dismiss_btn);

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

    // Action buttons (rendered only when visible actions are present —
    // the reserved `default` action is excluded, see `notif_actions`).
    // Cap at 3 visible buttons so a chatty app (e.g. an "snooze 1m / 5m /
    // 15m / 1h" calendar reminder) can't blow out the toast width — the
    // rest stay accessible from the drawer history page.
    let mut visible = notif_actions::visible_actions(&notif.actions).peekable();
    if visible.peek().is_some() {
        let actions_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        actions_row.add_css_class("ts-toast-actions");
        for action in visible.take(3) {
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

    // Click anywhere on the card → invoke the reserved `default` action
    // (if the notification carries one) then dismiss (reason 2 =
    // dismissed by user); with no `default` action this is a plain
    // dismiss, same as before. Action/dismiss buttons consume their own
    // click events before it bubbles here.
    let id = notif.id;
    let default_key = notif_actions::default_action(&notif.actions).map(|a| a.key.clone());
    let gesture = gtk::GestureClick::new();
    gesture.connect_pressed(move |gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        if let Some(key) = &default_key {
            notifications::invoke_action(id, key);
        }
        notifications::dismiss(id, 2);
    });
    card.add_controller(gesture);

    card.upcast()
}

// ── Overflow card builder ─────────────────────────────────────────────────────

/// Synthetic "+N more notifications" card shown when more than
/// [`MAX_VISIBLE_NONCRITICAL`] non-critical toasts are active. Clicking
/// the card opens the Notifications drawer page on `monitor`.
fn build_overflow_card(monitor: &Monitor, count: usize) -> gtk::Widget {
    let card = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    card.add_css_class("ts-toast");
    card.add_css_class("ts-toast-overflow");

    let icon = gtk::Image::from_icon_name("preferences-system-notifications-symbolic");
    icon.set_pixel_size(crate::scale::scale(24));
    icon.add_css_class("ts-toast-image");
    card.append(&icon);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
    column.set_hexpand(true);

    let summary = gtk::Label::new(Some(&format!("+{count} more notifications")));
    summary.add_css_class("ts-toast-summary");
    summary.set_xalign(0.0);
    column.append(&summary);

    let body = gtk::Label::new(Some("Click to open Notifications"));
    body.add_css_class("ts-toast-body");
    body.set_xalign(0.0);
    column.append(&body);

    card.append(&column);

    // Open (not toggle) the drawer — clicking the overflow card while the
    // drawer is already open should NOT close it.
    let monitor_for_click = monitor.clone();
    let gesture = gtk::GestureClick::new();
    gesture.connect_pressed(move |gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        crate::modal::open(&monitor_for_click, crate::modal::Page::Notifications);
    });
    card.add_controller(gesture);

    card.upcast()
}

// ── Image widget builder ──────────────────────────────────────────────────────

fn build_image(image: &NotificationImage) -> gtk::Image {
    let img = gtk::Image::new();
    img.set_pixel_size(crate::scale::scale(48));
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
