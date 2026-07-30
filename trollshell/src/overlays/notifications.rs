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
//!
//! # Card lifetime
//!
//! A mounted card is reused across emissions unless the notification it
//! renders actually changed ([`card_content_eq`]), and a card that must be
//! swapped carries its hover hold to the replacement ([`replace_card`]).
//! Both exist because destroying a card releases the hover-pause hold
//! [`attach_hover_pause`] took, and a pointer that never moves generates no
//! crossing event to take it again (#593).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use hytte::futures_signals::map_ref;
use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::dnd;
use hytte::services::notifications::{self, Action, Notification, NotificationImage, Urgency};
use hytte::services::notifications_mute;
use hytte::ui::{Anchor, Margin, layer_window};

use crate::components::{focused_output, notif_actions};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum number of non-critical toasts rendered as individual cards.
/// Additional non-critical notifications collapse into a synthetic
/// "+N more" overflow card. Critical-urgency toasts always render
/// individually and do not count toward this cap.
const MAX_VISIBLE_NONCRITICAL: usize = 4;

/// Maximum rendered lines for a toast's body text (#352). Without a cap, a
/// long or multi-paragraph body wraps into an unbounded vertical stack and
/// stretches the toast popover to full-screen height. Paired with
/// [`clamp_lines`] — see its doc comment for why `Label::set_lines` alone
/// isn't sufficient.
const BODY_MAX_LINES: usize = 5;

/// Maximum rendered lines for a toast's summary text (#352). See
/// [`BODY_MAX_LINES`].
const SUMMARY_MAX_LINES: usize = 2;

// ── Per-monitor toast view ────────────────────────────────────────────────────

struct ToastView {
    window: gtk::Window,
    vbox: gtk::Box,
    monitor: Monitor,
    card_map: RefCell<HashMap<u32, CardEntry>>,
    overflow_card: RefCell<Option<gtk::Widget>>,
    suppressed_during_dnd: RefCell<HashSet<u32>>,
}

/// A mounted toast card, the notification snapshot it was built from, and the
/// hover-hold flag its motion controller drives.
///
/// The snapshot is what makes [`apply_emission`]'s "has anything actually
/// changed?" test possible; the flag is what lets a genuine rebuild carry the
/// service-side hover hold across the swap. Both are #593.
struct CardEntry {
    widget: gtk::Widget,
    /// The notification this card was rendered from — compared against the
    /// incoming one by [`card_content_eq`].
    notif: Notification,
    /// Shared with the card's `EventControllerMotion` / `connect_unmap`
    /// handlers: true exactly while this card holds one service-side hover
    /// hold on `notif.id`. See [`attach_hover_pause`].
    holds_hover: Rc<Cell<bool>>,
}

// ── Thread-local window storage ───────────────────────────────────────────────

thread_local! {
    /// Mounted toast surfaces keyed by `Monitor.connector()`. Each
    /// entry owns its layer-shell window and the per-window state.
    static TOAST_WINDOWS: RefCell<HashMap<String, ToastView>> =
        RefCell::new(HashMap::new());

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

/// Wires the shared focused-output cache and the combined notification
/// signal. Runs exactly once across all per-monitor [`install`] calls
/// (gated by [`SUBS_INSTALLED`]).
fn install_subscriptions() {
    // components::focused_output is idempotent — see its docs.
    focused_output::install();

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
    let target_name = focused_output::current();
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
            && let Some(entry) = map.remove(id)
        {
            view.vbox.remove(&entry.widget);
        }
    }

    // Add, rebuild, or KEEP a card for each notification.
    //
    // Keeping an unchanged card mounted is load-bearing, not just an
    // optimisation (#593): destroying it fires `connect_unmap`, which releases
    // the hover hold `attach_hover_pause` took — and GTK4 only retargets
    // pointer focus while processing a crossing or motion event, so a
    // *stationary* pointer produces no `enter` on the replacement and the
    // pause is never re-established. Rebuilding every card on every emission
    // therefore let any unrelated notification arriving or expiring silently
    // resume the countdown under a toast the user was still reading.
    for (id, notif) in &new_ids {
        if map
            .get(id)
            .is_some_and(|entry| card_content_eq(&entry.notif, notif))
        {
            continue;
        }
        let entry = match map.remove(id) {
            // Same id, new content (a `replaces_id` update) — swap the card.
            Some(old) => replace_card(&view.vbox, &old, notif),
            None => mount_card(&view.vbox, notif),
        };
        map.insert(*id, entry);
    }

    // Manage the overflow "+N more" card. Singleton, lives in
    // `view.overflow_card`. Removed when tail is empty; rebuilt when
    // tail count changes (so the label updates).
    //
    // Unlike the notification cards above this one is still rebuilt
    // unconditionally: it holds no hover state (no timer to strand), and the
    // remove-then-append is what keeps it pinned below every card the loop
    // above may have appended this pass.
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

/// Pre-clamp `text` to at most `n` hard-newline-delimited lines, joined back
/// with `\n`. Required *in addition to* `Label::set_lines(n)`: Pango treats
/// each `\n` as a separate paragraph, and `set_lines(n)` only caps lines
/// produced by *wrapping* — it does not limit the number of hard-newline
/// paragraphs. Without this pre-clamp, a body with many `\n`s would still
/// blow up the toast popover height (the #126/#129 lesson, revisited for
/// toasts in #352).
fn clamp_lines(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}

/// Would [`build_card`] render `a` and `b` identically?
///
/// `Notification` deliberately isn't `PartialEq` — `NotificationImage::Raw`
/// carries a decoded pixel buffer, and `created_at` is stamped afresh by every
/// `replaces_id` re-post even when nothing visible changed — so this compares
/// exactly the fields [`build_card`] reads, and nothing else. **Keep the two in
/// sync**: a field newly rendered onto the card must be added here, or an
/// update that only touches it will leave a stale card mounted.
///
/// Deliberately excluded: `timeout` and `created_at` (never rendered on a
/// toast; a re-post's timeout is handed to the service-side expiry bookkeeping
/// by `notify` regardless of what the overlay does with the widget — re-arming
/// it, or leaving it held if this card is hovered, #619).
fn card_content_eq(a: &Notification, b: &Notification) -> bool {
    a.id == b.id
        && a.app_name == b.app_name
        && a.app_icon == b.app_icon
        && a.summary == b.summary
        && a.body == b.body
        && a.urgency == b.urgency
        && actions_eq(&a.actions, &b.actions)
        && image_eq(a.image.as_ref(), b.image.as_ref())
}

/// Structural equality for a notification's action list (`Action` isn't
/// `PartialEq` either). Order matters: the buttons are rendered in list order.
fn actions_eq(a: &[Action], b: &[Action]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.key == y.key && x.label == y.label)
}

/// Structural equality for a notification's optional image payload. `Raw`
/// compares the pixel buffer itself — a memcmp of a thumbnail-sized buffer, at
/// most once per live toast per emission, which is cheap next to rebuilding
/// the widget tree it would otherwise gate.
fn image_eq(a: Option<&NotificationImage>, b: Option<&NotificationImage>) -> bool {
    match (a, b) {
        (None, None) => true,
        (
            Some(NotificationImage::Raw {
                width: aw,
                height: ah,
                rowstride: ar,
                has_alpha: aa,
                channels: ac,
                data: ad,
            }),
            Some(NotificationImage::Raw {
                width: bw,
                height: bh,
                rowstride: br,
                has_alpha: ba,
                channels: bc,
                data: bd,
            }),
        ) => aw == bw && ah == bh && ar == br && aa == ba && ac == bc && ad == bd,
        (Some(NotificationImage::Path(x)), Some(NotificationImage::Path(y)))
        | (Some(NotificationImage::IconName(x)), Some(NotificationImage::IconName(y))) => x == y,
        _ => false,
    }
}

/// Build a card for `notif` and append it to `vbox`.
fn mount_card(vbox: &gtk::Box, notif: &Notification) -> CardEntry {
    let entry = build_card(notif);
    vbox.append(&entry.widget);
    entry
}

/// Swap `old`'s card out for a freshly built one (same id, new content).
///
/// Two things the naive remove-then-append doesn't do:
///
/// - **Carries the hover hold** (#593). If `old` holds one, a *second* hold is
///   taken before `old` is unmapped, so its `connect_unmap` teardown drops the
///   service-side count from two back to one rather than to zero, the countdown
///   never re-arms mid-read, and the fresh card inherits the hold (seeded into
///   its `holds_hover`, so its eventual `leave`/`unmap` balances it exactly
///   once). Waiting for a crossing event to re-establish the pause is precisely
///   what fails under a pointer that doesn't move.
///
///   That "two, not zero" rests on the service keeping one timer entry — and its
///   hover count — for the whole life of a notification, sticky or finite
///   (#619). It does not hold on its own: before #619 a sticky phase deleted the
///   entry, so `old.holds_hover` could be true against a count of zero and this
///   swap's own `resume` re-armed the countdown under a parked pointer.
///
///   Still open (**#626**): if the notification is *closed and re-posted* under
///   the same id while this card is mounted and holding, the entry is torn down
///   and recreated with a count of zero, and the carried hold then runs
///   0 → 1 → 0 and arms — the #619 symptom by a different route. Closing that
///   needs the hold to name the entry generation it was taken against, which is
///   more than this swap can decide locally. A close with no re-post is fine:
///   the entry stays gone, both calls no-op, and there is nothing left to expire.
/// - **Keeps the card's place** in the stack, rather than sending an updated
///   toast to the bottom on every re-post.
///
/// Residual case, deliberately accepted: if the replacement card is small
/// enough that the pointer no longer lies over it, no `leave` will ever come
/// and the inherited hold lasts until the card unmaps — i.e. the toast waits
/// for a dismiss instead of expiring. Bounded (the `connect_unmap` teardown
/// still balances the count) and strictly the safer failure of the two.
fn replace_card(vbox: &gtk::Box, old: &CardEntry, notif: &Notification) -> CardEntry {
    let carried = old.holds_hover.get();
    if carried {
        notifications::pause_expiry(notif.id);
    }
    let after = old.widget.prev_sibling();
    // Fires `old`'s `connect_unmap` → `resume_expiry`, balancing the hold
    // `old` itself took (never the one taken just above).
    vbox.remove(&old.widget);

    let entry = build_card(notif);
    entry.holds_hover.set(carried);
    vbox.insert_child_after(&entry.widget, after.as_ref());
    entry
}

fn build_card(notif: &Notification) -> CardEntry {
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

    // Summary. See `clamp_lines` (#352) for why both a hard-newline
    // pre-clamp and `set_lines` are needed.
    if !notif.summary.is_empty() {
        let clamped = clamp_lines(&notif.summary, SUMMARY_MAX_LINES);
        let summary = gtk::Label::new(Some(&clamped));
        summary.add_css_class("ts-toast-summary");
        summary.set_xalign(0.0);
        summary.set_wrap(true);
        summary.set_max_width_chars(40);
        summary.set_lines(i32::try_from(SUMMARY_MAX_LINES).unwrap_or(i32::MAX));
        summary.set_ellipsize(gtk::pango::EllipsizeMode::End);
        column.append(&summary);
    }

    // Body. See `clamp_lines` (#352).
    if !notif.body.is_empty() {
        let clamped = clamp_lines(&notif.body, BODY_MAX_LINES);
        let body = gtk::Label::new(Some(&clamped));
        body.add_css_class("ts-toast-body");
        body.set_xalign(0.0);
        body.set_wrap(true);
        body.set_max_width_chars(40);
        body.set_lines(i32::try_from(BODY_MAX_LINES).unwrap_or(i32::MAX));
        body.set_ellipsize(gtk::pango::EllipsizeMode::End);
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

    // Hover-pause the auto-expiry countdown while the pointer is over the card.
    let holds_hover = attach_hover_pause(&card, notif.id);

    CardEntry {
        widget: card.upcast(),
        notif: notif.clone(),
        holds_hover,
    }
}

/// Wire hover-pause of a toast's auto-expiry onto `card` (#567). While the
/// pointer is over the card, the service holds the expiry timer; on leave it
/// resumes with the remaining time.
///
/// `holds_enter` clamps THIS card's contribution to the service-side
/// hover-count to at most one, so repeated enter/leave signals (or a stray
/// leave) can't drift the count, and multiple per-monitor toast copies each
/// contribute independently. The `connect_unmap` teardown balances the count
/// if the card is removed while still hovered — GTK does not guarantee a
/// `leave` on unmap, which would otherwise strand the timer paused forever.
/// A sticky notification takes and releases the hold like any other: there is no
/// countdown to pause, but the service still records the count, so a `replaces_id`
/// re-post that turns the notification finite inherits the hold instead of
/// arming behind a pointer that never moved (#619).
///
/// Returns the `holds_enter` flag so a card swap can read it and seed the
/// successor's — see [`replace_card`] (#593). Setting it to `true` on a fresh
/// card asserts "this card already owns a hold", which is why the swap takes
/// that hold explicitly before tearing the old card down.
fn attach_hover_pause(card: &gtk::Box, id: u32) -> Rc<Cell<bool>> {
    let holds_enter = Rc::new(Cell::new(false));

    let motion = gtk::EventControllerMotion::new();
    {
        let holds = holds_enter.clone();
        motion.connect_enter(move |_, _, _| {
            if !holds.replace(true) {
                notifications::pause_expiry(id);
            }
        });
    }
    {
        let holds = holds_enter.clone();
        motion.connect_leave(move |_| {
            if holds.replace(false) {
                notifications::resume_expiry(id);
            }
        });
    }
    card.add_controller(motion);

    {
        let holds = holds_enter.clone();
        card.connect_unmap(move |_| {
            if holds.replace(false) {
                notifications::resume_expiry(id);
            }
        });
    }

    holds_enter
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Action, Notification, NotificationImage, Urgency};
    use super::{actions_eq, card_content_eq, clamp_lines, image_eq};

    /// A plain notification to mutate one field of per test.
    fn notif() -> Notification {
        Notification {
            id: 7,
            app_name: "Fractal".to_string(),
            app_icon: "im-message".to_string(),
            summary: "Mara".to_string(),
            body: "check how nova-shell did it".to_string(),
            urgency: Urgency::Normal,
            timeout: Some(Duration::from_secs(5)),
            actions: vec![Action {
                key: "reply".to_string(),
                label: "Reply".to_string(),
            }],
            image: Some(NotificationImage::IconName("avatar".to_string())),
            created_at: 1_000,
        }
    }

    #[test]
    fn clamp_lines_passes_short_text_through() {
        assert_eq!(clamp_lines("one line", 5), "one line");
    }

    #[test]
    fn clamp_lines_caps_hard_newlines() {
        // The #126/#129 case: many `\n`-separated paragraphs must be
        // truncated by the pre-clamp itself, since `set_lines` alone only
        // caps *wrapped* lines, not hard-newline paragraphs.
        let body = "one\ntwo\nthree\nfour\nfive\nsix\nseven";
        assert_eq!(clamp_lines(body, 3), "one\ntwo\nthree");
    }

    #[test]
    fn clamp_lines_exact_count_unchanged() {
        assert_eq!(clamp_lines("a\nb", 2), "a\nb");
    }

    #[test]
    fn clamp_lines_zero_yields_empty() {
        assert_eq!(clamp_lines("a\nb\nc", 0), "");
    }

    #[test]
    fn clamp_lines_empty_input() {
        assert_eq!(clamp_lines("", 5), "");
    }

    // ── card_content_eq — the "may this card stay mounted?" test (#593) ──────

    #[test]
    fn identical_notifications_keep_the_card() {
        assert!(card_content_eq(&notif(), &notif()));
    }

    /// A named one-field edit to a [`notif`], for
    /// [`every_rendered_field_forces_a_rebuild`].
    type Mutation = (&'static str, fn(&mut Notification));

    #[test]
    fn every_rendered_field_forces_a_rebuild() {
        // One case per field `build_card` reads. If a new field starts being
        // rendered, it belongs both in `card_content_eq` and here.
        let mutations: Vec<Mutation> = vec![
            ("id", |n| n.id = 8),
            ("app_name", |n| n.app_name = "Nheko".to_string()),
            ("app_icon", |n| n.app_icon = "mail-unread".to_string()),
            ("summary", |n| n.summary = "Annika".to_string()),
            ("body", |n| n.body = "ship it".to_string()),
            ("urgency", |n| n.urgency = Urgency::Critical),
            ("actions", |n| n.actions.clear()),
            ("image", |n| n.image = None),
        ];
        for (field, mutate) in mutations {
            let base = notif();
            let mut changed = notif();
            mutate(&mut changed);
            assert!(
                !card_content_eq(&base, &changed),
                "changing {field} must rebuild the card"
            );
        }
    }

    #[test]
    fn unrendered_fields_do_not_force_a_rebuild() {
        // `created_at` is re-stamped by every `replaces_id` re-post and
        // `timeout` is the service's business — neither reaches the widget
        // tree, so neither may cost the card (and with it the hover hold).
        let base = notif();
        let mut resent = notif();
        resent.created_at = 9_999;
        resent.timeout = Some(Duration::from_secs(30));
        assert!(card_content_eq(&base, &resent));
    }

    #[test]
    fn action_label_and_order_changes_rebuild() {
        let base = notif();

        let mut relabelled = notif();
        relabelled.actions[0].label = "Answer".to_string();
        assert!(!card_content_eq(&base, &relabelled));

        let two = |a: &str, b: &str| {
            vec![
                Action {
                    key: a.to_string(),
                    label: a.to_string(),
                },
                Action {
                    key: b.to_string(),
                    label: b.to_string(),
                },
            ]
        };
        assert!(actions_eq(&two("a", "b"), &two("a", "b")));
        assert!(!actions_eq(&two("a", "b"), &two("b", "a")));
        assert!(!actions_eq(&two("a", "b"), &[]));
    }

    #[test]
    fn image_equality_covers_variants_and_payload() {
        let raw = |data: Vec<u8>| NotificationImage::Raw {
            width: 2,
            height: 1,
            rowstride: 8,
            has_alpha: true,
            channels: 4,
            data,
        };
        let bytes = vec![1, 2, 3, 4, 5, 6, 7, 8];

        assert!(image_eq(
            Some(&raw(bytes.clone())),
            Some(&raw(bytes.clone()))
        ));
        // Same dimensions, different pixels — a progress-bar thumbnail that
        // only redraws its fill must still rebuild the card.
        assert!(!image_eq(Some(&raw(bytes.clone())), Some(&raw(vec![9; 8]))));
        assert!(image_eq(None, None));
        assert!(!image_eq(Some(&raw(bytes)), None));

        let path = NotificationImage::Path("/tmp/a.png".to_string());
        let icon = NotificationImage::IconName("/tmp/a.png".to_string());
        assert!(image_eq(Some(&path), Some(&path.clone())));
        // Same string, different variant — one is loaded from disk, the other
        // from the icon theme.
        assert!(!image_eq(Some(&path), Some(&icon)));
    }
}
