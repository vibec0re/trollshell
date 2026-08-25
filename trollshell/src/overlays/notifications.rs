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
//!
//! # Hover holds
//!
//! Which toast is hovered is a fact the overlay observes (GTK crossing events)
//! and the service records (a per-id hover count). [`HoldState`] is the whole of
//! the overlay's half, and the rule that keeps the two from drifting is stated
//! there: a card's claim on the count is **renewed against the entry as it
//! stands now** at the end of every emission, so entry identity never enters the
//! correctness argument (#626).

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
/// hover claim its motion controller drives.
///
/// The snapshot is what makes [`apply_emission`]'s "has anything actually
/// changed?" test possible (#593); the claim is what lets a genuine rebuild
/// carry the service-side hover hold across the swap (#593) and what
/// [`apply_emission`] renews at the end of every pass (#626).
struct CardEntry {
    widget: gtk::Widget,
    /// The notification this card was rendered from — compared against the
    /// incoming one by [`card_content_eq`].
    notif: Notification,
    /// This card's claim on the service-side hover count for `notif.id`,
    /// shared with its `EventControllerMotion` / `connect_unmap` handlers.
    /// See [`HoldState`].
    hold: CardHold,
}

// ── Thread-local window storage ───────────────────────────────────────────────

thread_local! {
    /// Mounted toast surfaces keyed by `Monitor.connector()`. Each
    /// entry owns its layer-shell window and the per-window state.
    ///
    /// `Rc<ToastView>`, not a bare `ToastView` (#643): [`route_emission`] has to
    /// hand a view to [`apply_emission`], which runs a long stretch of GTK work,
    /// and the only way to do that without holding this cell borrowed across all
    /// of it is to clone a handle out. Mirrors `overlays::osd`'s `OSDS`.
    static TOAST_WINDOWS: RefCell<HashMap<String, Rc<ToastView>>> =
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
    // **The semicolon rule** (#643). This `insert` returns the displaced
    // `Rc<ToastView>`, and it is safe *only* because it is the closure's tail
    // expression: the returned `Option` is moved out to the caller before the
    // `borrow_mut()` `RefMut` — a temporary of that same expression — drops, so
    // the displaced view runs its drop glue in the *outer* statement, with
    // `TOAST_WINDOWS` no longer borrowed. Putting a semicolon after
    // `insert(...)` inside the closure inverts that: the `Option` becomes a
    // statement temporary dropped *before* the `RefMut`, i.e. GObject unrefs
    // under a live borrow. Do not add one. (See the four `install` sites in
    // `sidebar`/`frame`/`osd`/`consent`, which had exactly that shape.)
    TOAST_WINDOWS.with(|map| map.borrow_mut().insert(connector, Rc::new(view)));

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
///
/// Tears down with `destroy()`, not `close()` (#632): a toast surface that
/// never showed a notification on this monitor is still unrealized, and
/// `close()` neither destroys an unrealized window nor drops GTK's internal
/// toplevel reference — only `destroy()` does, and it can't be vetoed by a
/// `close-request` handler.
pub fn close_all() {
    TOAST_WINDOWS.with(|map| {
        // `take()` moves the whole map out (leaving `Default`) and releases
        // the borrow inside the call, rather than holding a `drain()` RefMut
        // across every `destroy()` below (#631) — a borrow held across a GTK
        // call is a latent reentrancy hazard if any emission it triggers is
        // ever synchronous.
        for (_, view) in map.take() {
            view.window.destroy();
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
    // Resolve to an owned handle and let the borrow end at this `let` (#643).
    // `apply_emission` is nothing but GTK work — `remove`/`append`/`set_visible`
    // on the card stack — and `install`/`close_all` are the `borrow_mut()`
    // counterparties on this cell. A shared borrow held across all that would
    // panic on any re-entry, and a `BorrowMutError` unwinding through a glib
    // callback aborts the process rather than failing the update.
    let view = TOAST_WINDOWS.with(|map| {
        let map = map.borrow();
        target_name
            .as_ref()
            .and_then(|n| map.get(n))
            .or_else(|| map.values().next())
            .map(Rc::clone)
    });
    if let Some(view) = view {
        apply_emission(&view, notifs, dnd_on, muted);
    }
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
    // Take both cells for the duration of the rebuild rather than holding a
    // `RefMut` across it (#643). The named bindings this replaces stayed live
    // all the way down past `vbox.remove`, `replace_card`/`mount_card`, the
    // overflow `remove`/`append`, and the closing `set_visible` — spelling (2)
    // of the sweep, a `let`-bound borrow instead of a chained temporary, but
    // exactly the same hazard.
    //
    // This is the one site in the cluster where the tree *documents* the
    // emission as synchronous rather than leaving it unverified: the comment on
    // the card loop below, and `replace_card`'s own comment on
    // `vbox.remove(&old.widget)`, both state that unmapping a card fires its
    // `connect_unmap` → `resume_expiry`, and #593's keep-the-card-mounted design
    // is built on that being immediate. It doesn't panic today only because that
    // handler reaches for the expiry bookkeeping rather than for `card_map`.
    //
    // Trade-off, stated plainly: while this runs, the cells hold empty
    // `Default`s, so a re-entrant reader would see no cards rather than the live
    // set. That is the same trade #643's other take-and-restore conversions make
    // (`panels::stats`'s per-core bars, `panels::connections`' row vecs), and it
    // is strictly better than the alternative, which is not a wrong answer but a
    // `BorrowMutError` unwinding through a glib callback — a process abort.
    // Nothing re-enters via the driver itself: `route_emission` is pumped from a
    // `spawn_local`ed `for_each`, which cannot re-poll itself synchronously.
    let mut map = view.card_map.take();
    let mut suppressed = view.suppressed_during_dnd.take();

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

    // Renew every live hover claim against the timer entries **as they stand
    // now** (#626). This is the step that makes the widget-side record
    // un-losable rather than merely well-behaved: the loop above cannot tell a
    // surviving timer entry from one that was torn down and re-created with a
    // zero hover count since the last emission (a `CloseNotification` followed
    // by a re-post of the same id, both landing before GTK polled), and
    // `HoldState::resync` is what makes it not have to. See [`HoldState`].
    //
    // Runs over every mounted card, not just the ones this pass rebuilt: an
    // identical re-post keeps its card via `card_content_eq` above and so takes
    // the `continue` branch, yet its entry is just as replaceable.
    for entry in map.values() {
        entry.hold.resync();
    }

    // Manage the overflow "+N more" card. Singleton, lives in
    // `view.overflow_card`. Removed when tail is empty; rebuilt when
    // tail count changes (so the label updates).
    //
    // Unlike the notification cards above this one is still rebuilt
    // unconditionally: it holds no hover state (no timer to strand), and the
    // remove-then-append is what keeps it pinned below every card the loop
    // above may have appended this pass.
    //
    // Take-then-act here too (#643): the `let mut slot = ….borrow_mut()` this
    // replaces was held across the `remove()` and the `append()` below. Both
    // arms started with the same removal, so it is hoisted out.
    if let Some(card) = view.overflow_card.take() {
        view.vbox.remove(&card);
    }
    let overflow = if tail_noncritical_count == 0 {
        None
    } else {
        let card = build_overflow_card(&view.monitor, tail_noncritical_count);
        view.vbox.append(&card);
        Some(card)
    };

    // Decide visibility from the locals, *before* handing anything back: the
    // original spelled this `!map.is_empty() || view.overflow_card.borrow()
    // .is_some()`, an argument-position `Ref` that is a temporary of the whole
    // statement and so stayed alive across `set_visible` — spelling (4).
    let any_mounted = !map.is_empty() || overflow.is_some();

    // Store back. Each cell holds the empty `Default` its `take()` left behind
    // (`HashMap`/`HashSet`/`Option::None` allocate nothing until first use), so
    // these assignments drop nothing of consequence inside the borrow.
    *view.card_map.borrow_mut() = map;
    *view.suppressed_during_dnd.borrow_mut() = suppressed;
    *view.overflow_card.borrow_mut() = overflow;

    view.window.set_visible(any_mounted);
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
/// - **Carries the hover claim** (#593). If `old` holds one, it is *moved* onto
///   the replacement before `old` is unmapped, so `old`'s `connect_unmap`
///   teardown finds nothing to release and the swap makes **no service call at
///   all**. Waiting for a crossing event to re-establish the pause is precisely
///   what fails under a pointer that doesn't move.
///
///   A move, not the release/re-take pair this used to do (#626). That pair was
///   ordered take-then-release specifically so the service-side count went
///   1 → 2 → 1 instead of 1 → 0 → 1, since reaching zero with a finite timeout
///   arms a fresh countdown. But it only went 1 → 2 → 1 while the entry the
///   count lives in survived the swap; a `CloseNotification` plus a re-post of
///   the same id, both landing before GTK polled, replaced it with a fresh
///   `hover_count: 0` entry, and then the very same pair ran 0 → 1 → 0 and armed
///   under a parked pointer. Moving the claim removes the pair, and with it
///   every ordering question about an entry this function cannot see.
///
///   The claim the replacement inherits is then renewed against whatever entry
///   exists by [`apply_emission`]'s closing `resync` pass — which is what
///   actually re-establishes the hold when the entry *was* replaced. This
///   function deliberately decides nothing about that; see [`HoldState`].
/// - **Keeps the card's place** in the stack, rather than sending an updated
///   toast to the bottom on every re-post.
///
/// Residual case, deliberately accepted: if the replacement card is small
/// enough that the pointer no longer lies over it, no `leave` will ever come
/// and the inherited claim lasts until the card unmaps — i.e. the toast waits
/// for a dismiss instead of expiring. Bounded (the `connect_unmap` teardown
/// still balances the count) and strictly the safer failure of the two.
fn replace_card(vbox: &gtk::Box, old: &CardEntry, notif: &Notification) -> CardEntry {
    let after = old.widget.prev_sibling();
    let entry = build_card(notif);
    // Before the teardown, so the `connect_unmap` that `remove` fires below
    // sees a card with no claim left and stands down.
    entry.hold.adopt_from(&old.hold);
    vbox.remove(&old.widget);
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
    let hold = attach_hover_pause(&card, notif.id);

    CardEntry {
        widget: card.upcast(),
        notif: notif.clone(),
        hold,
    }
}

// ── Hover hold ────────────────────────────────────────────────────────────────

/// One service-side call the overlay makes on behalf of a hover claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoldCall {
    /// [`notifications::pause_expiry`].
    Pause,
    /// [`notifications::resume_expiry`].
    Resume,
}

/// What a [`HoldState`] transition asks the effect layer to do. Split out from
/// the transition so the transitions stay pure — testable with no display
/// server, no notifications service, and no tokio runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoldAction {
    /// Claim a hold this card did not have.
    Take,
    /// Give up the hold this card had.
    Release,
    /// Give the hold up and immediately re-claim it, within one GTK main-loop
    /// iteration. See [`HoldState::resync`].
    Renew,
    /// Nothing to do.
    Nothing,
}

impl HoldAction {
    /// The exact call sequence this action performs, in order.
    ///
    /// Both [`CardHold::apply`] and the state-machine tests drive this one list,
    /// so a test that pins an ordering pins the ordering the shell emits rather
    /// than a paraphrase of it.
    fn calls(self) -> &'static [HoldCall] {
        match self {
            Self::Take => &[HoldCall::Pause],
            Self::Release => &[HoldCall::Resume],
            Self::Renew => &[HoldCall::Resume, HoldCall::Pause],
            Self::Nothing => &[],
        }
    }
}

/// One toast card's claim on the service-side hover count for its notification
/// id — the entirety of the overlay's hover bookkeeping.
///
/// # Why a claim rather than a mirror
///
/// "This toast is hovered" is observed here (GTK crossing events) and recorded
/// there (`TimerState::hover_count`, one entry per live notification). Five
/// defects in a row (#567 → #593 → #596 → #619 → #626) were the same shape: the
/// two records had different lifetimes, so a widget-side `true` outlived — or
/// out-*lived-through* — the service-side entry it was taken against, and the
/// release it eventually issued landed somewhere it did not belong. The
/// arithmetic was never wrong; the addressing was.
///
/// So this type does not mirror the count. It records only that **this card
/// claims one unit of it**, and the overlay discharges that claim by
/// *renewing* it — [`resync`](Self::resync), run over every mounted card at the
/// end of every emission — rather than by remembering where it was first taken.
///
/// # Why renewal cannot lose the hold
///
/// [`HoldAction::Renew`] is `resume_expiry` immediately followed by
/// `pause_expiry`, and its postcondition is *"hover count ≥ 1 for this id and
/// nothing armed"* under **every** state the entry can be in, because:
///
/// - **Entry re-created since the claim was taken** (the #626 case: a
///   `CloseNotification` dropped it and a re-post minted a fresh
///   `hover_count: 0` one that armed). The `resume` is clamped to a no-op —
///   `TimerState::resume` returns `Nothing` at a count of zero — and the
///   `pause` then takes that fresh entry 0 → 1, aborting the countdown it
///   armed.
/// - **Entry survived, this is its only claim.** 1 → 0 arms, 0 → 1 aborts it
///   again. Observationally neutral: the re-armed duration is
///   `max(remaining, MIN_RESUME)`, the pause records that back as `remaining`,
///   and the eventual real leave arms `max(remaining, MIN_RESUME)` either way.
///   (The recorded remainder loses the microseconds between the two calls each
///   time, and the sleep task spawned by the first is aborted by the second
///   before it can tick — both bounded by that same `MIN_RESUME` floor.)
/// - **Entry survived, another monitor's copy also claims it.** 2 → 1 → 2, no
///   edge crossed, no effect whatsoever.
/// - **Entry gone and not re-posted.** Both calls find no entry and no-op.
///
/// None of those four cases needs to be distinguished, and the overlay could
/// not distinguish them anyway. That is the point: *entry identity is not part
/// of the correctness argument*, so it cannot be got wrong. The order is
/// load-bearing and is the only ordering that works — `pause`-then-`resume`
/// would take a fresh entry 0 → 1 → 0 and arm, which is exactly the bug.
///
/// A card swap ([`replace_card`]) correspondingly makes *no* service call: the
/// claim is moved with [`transfer`](Self::transfer), so no count edge is crossed
/// during the teardown, and the renewal pass re-establishes the hold afterwards.
///
/// # Residual
///
/// The window between the service arming a re-created entry (on the D-Bus
/// worker) and the renewal pass running (on the GTK main thread, next poll) is
/// not closed — a notification whose timeout is shorter than one main-loop
/// iteration could still expire inside it. Closing that needs the service to
/// carry the count across the close/re-post, which is not something the overlay
/// can decide.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct HoldState {
    /// True exactly while this card claims one unit of the service-side hover
    /// count for its notification id.
    held: bool,
}

impl HoldState {
    /// The pointer entered this card. Idempotent, so repeated `enter`s — or an
    /// `enter` on a card that inherited a claim via [`transfer`](Self::transfer)
    /// — never take a second hold.
    fn enter(self) -> (Self, HoldAction) {
        if self.held {
            (self, HoldAction::Nothing)
        } else {
            (Self { held: true }, HoldAction::Take)
        }
    }

    /// The pointer left this card, or the card unmapped. Both wire here and
    /// exactly one of them may act: GTK does not guarantee a `leave` before an
    /// unmap (which would strand the timer paused forever), nor an unmap
    /// without a preceding `leave` (which would drift the count).
    fn leave(self) -> (Self, HoldAction) {
        if self.held {
            (Self { held: false }, HoldAction::Release)
        } else {
            (self, HoldAction::Nothing)
        }
    }

    /// Renew a live claim against the timer entry as it stands *now*. The state
    /// is unchanged — this card still claims exactly one unit either way; only
    /// the entry that unit sits in may have been swapped out from under it. See
    /// the type doc for why this is total.
    fn resync(self) -> (Self, HoldAction) {
        let action = if self.held {
            HoldAction::Renew
        } else {
            HoldAction::Nothing
        };
        (self, action)
    }

    /// Move this claim from a card about to be torn down onto its replacement,
    /// returning `(predecessor, successor)`.
    ///
    /// One operation returning both halves, deliberately: a transfer that only
    /// cleared the predecessor would drop the claim, and one that only seeded
    /// the successor would double it — and both were expressible when this was
    /// a bare `Cell<bool>` read by [`replace_card`]. It emits no [`HoldAction`]
    /// because a move crosses no count edge.
    fn transfer(self) -> (Self, Self) {
        (Self { held: false }, self)
    }
}

/// Runtime handle to one card's [`HoldState`].
///
/// `Rc<Cell<…>>` because the card's motion controller, its `connect_unmap`
/// handler, and the [`CardEntry`] all drive the same claim; `id` rides along so
/// no caller has to re-supply it and mis-address a release.
#[derive(Clone)]
struct CardHold {
    id: u32,
    state: Rc<Cell<HoldState>>,
}

impl CardHold {
    fn new(id: u32) -> Self {
        Self {
            id,
            state: Rc::new(Cell::new(HoldState::default())),
        }
    }

    /// Run one pure transition and perform the calls it asks for.
    fn apply(&self, transition: fn(HoldState) -> (HoldState, HoldAction)) {
        let (next, action) = transition(self.state.get());
        self.state.set(next);
        for call in action.calls() {
            match call {
                HoldCall::Pause => notifications::pause_expiry(self.id),
                HoldCall::Resume => notifications::resume_expiry(self.id),
            }
        }
    }

    fn enter(&self) {
        self.apply(HoldState::enter);
    }

    fn leave(&self) {
        self.apply(HoldState::leave);
    }

    fn resync(&self) {
        self.apply(HoldState::resync);
    }

    /// Adopt `old`'s claim onto this (freshly built) card — see
    /// [`HoldState::transfer`]. Makes no service call by construction.
    fn adopt_from(&self, old: &Self) {
        let (cleared, mine) = old.state.get().transfer();
        old.state.set(cleared);
        self.state.set(mine);
    }
}

/// Wire hover-pause of a toast's auto-expiry onto `card` (#567). While the
/// pointer is over the card, the service holds the expiry timer; on leave it
/// resumes with the remaining time. A sticky notification takes and releases the
/// claim like any other: there is no countdown to pause, but the service still
/// records the count, so a `replaces_id` re-post that turns the notification
/// finite inherits the hold instead of arming behind a pointer that never
/// moved (#619).
///
/// Returns the claim so a card swap can move it to the successor
/// ([`replace_card`], #593) and so [`apply_emission`] can renew it (#626). All
/// three of the handlers wired here, and both of those callers, go through
/// [`HoldState`] — there is no other way to touch the count from the overlay.
fn attach_hover_pause(card: &gtk::Box, id: u32) -> CardHold {
    let hold = CardHold::new(id);

    let motion = gtk::EventControllerMotion::new();
    {
        let hold = hold.clone();
        motion.connect_enter(move |_, _, _| hold.enter());
    }
    {
        let hold = hold.clone();
        motion.connect_leave(move |_| hold.leave());
    }
    card.add_controller(motion);

    {
        let hold = hold.clone();
        card.connect_unmap(move |_| hold.leave());
    }

    hold
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
    use super::{HoldAction, HoldCall, HoldState};
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

    // ── The hover-claim state machine (#567/#593/#596/#619/#626) ─────────────

    /// A model of the service-side timer entry for **one** notification id.
    ///
    /// Not a copy of `hytte_services::notifications::TimerState`, and not an
    /// attempt to re-test it — the service's own hermetic bucket pins its
    /// arithmetic. This models exactly the clauses of that type's documented
    /// contract the overlay's call *ordering* rests on, so a regression in the
    /// ordering fails here:
    ///
    /// 1. `resume` at a hover count of zero is clamped to a no-op.
    /// 2. Only the *last* leave (the count reaching zero) arms, and only while
    ///    the notification is finite.
    /// 3. The *first* hover (the count reaching one) aborts whatever is armed.
    /// 4. A post creates the entry if absent and arms iff finite and unhovered;
    ///    a close removes it outright, hover count and all.
    ///
    /// Clause 4 is the whole of #626: the entry under a live claim can be
    /// destroyed and re-created with a zero count while the card holding that
    /// claim stays mounted and the pointer never moves.
    #[derive(Debug, Default)]
    struct TimerModel {
        entry: Option<ModelEntry>,
    }

    #[derive(Debug)]
    struct ModelEntry {
        finite: bool,
        hover_count: u32,
        armed: bool,
    }

    impl TimerModel {
        /// `Notify` → `set_expiry` → `apply_expiry`.
        fn post(&mut self, finite: bool) {
            let entry = self.entry.get_or_insert(ModelEntry {
                finite,
                hover_count: 0,
                armed: false,
            });
            entry.finite = finite;
            entry.armed = finite && entry.hover_count == 0;
        }

        /// `CloseNotification` (or an expiry firing) → `dismiss` →
        /// `clear_timer`.
        fn close(&mut self) {
            self.entry = None;
        }

        fn call(&mut self, call: HoldCall) {
            match call {
                HoldCall::Pause => {
                    if let Some(e) = &mut self.entry {
                        e.hover_count += 1;
                        if e.hover_count == 1 {
                            e.armed = false;
                        }
                    }
                }
                HoldCall::Resume => {
                    if let Some(e) = &mut self.entry {
                        if e.hover_count == 0 {
                            return; // clause 1
                        }
                        e.hover_count -= 1;
                        if e.hover_count == 0 && e.finite {
                            e.armed = true;
                        }
                    }
                }
            }
        }

        /// Will this notification expire on its own from here? The single
        /// user-visible question behind all five defects.
        fn armed(&self) -> bool {
            self.entry.as_ref().is_some_and(|e| e.armed)
        }

        fn hover_count(&self) -> u32 {
            self.entry.as_ref().map_or(0, |e| e.hover_count)
        }
    }

    /// A mounted card, driving a [`TimerModel`] through the real
    /// [`HoldState`] transitions and the real [`HoldAction::calls`] mapping —
    /// nothing about the effects is paraphrased here.
    #[derive(Debug, Default)]
    struct Card {
        hold: HoldState,
    }

    impl Card {
        fn apply(
            &mut self,
            timers: &mut TimerModel,
            transition: fn(HoldState) -> (HoldState, HoldAction),
        ) {
            let (next, action) = transition(self.hold);
            self.hold = next;
            for call in action.calls() {
                timers.call(*call);
            }
        }

        fn enter(&mut self, timers: &mut TimerModel) {
            self.apply(timers, HoldState::enter);
        }

        fn leave(&mut self, timers: &mut TimerModel) {
            self.apply(timers, HoldState::leave);
        }

        fn resync(&mut self, timers: &mut TimerModel) {
            self.apply(timers, HoldState::resync);
        }

        /// `replace_card`: build the successor, move the claim onto it, then
        /// unmap the predecessor — whose `connect_unmap` runs `leave`.
        fn rebuild(&mut self, timers: &mut TimerModel) -> Self {
            let (cleared, mine) = self.hold.transfer();
            self.hold = cleared;
            let successor = Self { hold: mine };
            self.leave(timers);
            successor
        }
    }

    #[test]
    fn enter_takes_exactly_one_hold() {
        let (held, action) = HoldState::default().enter();
        assert_eq!(action, HoldAction::Take);
        assert_eq!(held.enter(), (held, HoldAction::Nothing));
    }

    #[test]
    fn leave_releases_once_then_stands_down() {
        let (held, _) = HoldState::default().enter();
        let (idle, action) = held.leave();
        assert_eq!(action, HoldAction::Release);
        // The `connect_unmap` teardown following a `leave` — and a stray
        // `leave` with no matching `enter` — must not drift the count.
        assert_eq!(idle.leave(), (idle, HoldAction::Nothing));
    }

    #[test]
    fn resync_renews_only_a_live_claim() {
        let idle = HoldState::default();
        assert_eq!(idle.resync(), (idle, HoldAction::Nothing));
        let (held, _) = idle.enter();
        assert_eq!(held.resync(), (held, HoldAction::Renew));
    }

    #[test]
    fn transfer_moves_the_claim_whole() {
        let (held, _) = HoldState::default().enter();
        let (predecessor, successor) = held.transfer();
        assert_eq!(predecessor, HoldState::default());
        assert_eq!(successor, held);
        // The predecessor's `connect_unmap` now finds nothing to release, so
        // the swap crosses no count edge at all.
        assert_eq!(predecessor.leave().1, HoldAction::Nothing);

        // Transferring an unheld claim is just as total.
        let (predecessor, successor) = HoldState::default().transfer();
        assert_eq!(predecessor, HoldState::default());
        assert_eq!(successor, HoldState::default());
    }

    #[test]
    fn action_call_sequences_are_fixed() {
        assert_eq!(HoldAction::Take.calls(), &[HoldCall::Pause]);
        assert_eq!(HoldAction::Release.calls(), &[HoldCall::Resume]);
        // Order is load-bearing — see `renewing_pause_first_would_arm`.
        assert_eq!(
            HoldAction::Renew.calls(),
            &[HoldCall::Resume, HoldCall::Pause]
        );
        assert!(HoldAction::Nothing.calls().is_empty());
    }

    #[test]
    fn renewing_pause_first_would_arm() {
        // The negative control for `HoldAction::Renew`'s order. Against a
        // freshly re-created entry (hover count zero, counting down), pausing
        // before resuming runs 0 → 1 → 0 and re-arms — #626's exact failure,
        // and what the pre-#626 `replace_card` did.
        let mut timers = TimerModel::default();
        timers.post(true);
        timers.call(HoldCall::Pause);
        timers.call(HoldCall::Resume);
        assert!(timers.armed(), "pause-then-resume arms a fresh entry");

        // The shipped order, from the same state.
        let mut timers = TimerModel::default();
        timers.post(true);
        for call in HoldAction::Renew.calls() {
            timers.call(*call);
        }
        assert!(!timers.armed());
        assert_eq!(timers.hover_count(), 1);
    }

    #[test]
    fn close_and_repost_under_a_parked_pointer_keeps_the_toast() {
        // #626 itself. Finite post; the pointer parks on the toast.
        let mut timers = TimerModel::default();
        let mut card = Card::default();
        timers.post(true);
        card.enter(&mut timers);
        assert!(!timers.armed(), "a hovered toast must not count down");

        // `CloseNotification(id)` immediately followed by
        // `Notify(replaces_id = id)`, both handled on the D-Bus worker before
        // GTK polls: the entry the claim was taken against is gone, and a fresh
        // one is counting down in its place under a pointer that never moved.
        timers.close();
        timers.post(true);
        assert!(timers.armed());
        assert_eq!(
            timers.hover_count(),
            0,
            "the re-created entry knows nothing of the mounted card"
        );

        // GTK polls. Content changed, so the card is swapped...
        let mut card = card.rebuild(&mut timers);
        assert_eq!(
            timers.hover_count(),
            0,
            "a swap moves the claim and must make no service call"
        );
        // ...and `apply_emission`'s closing pass renews the claim.
        card.resync(&mut timers);

        assert!(
            !timers.armed(),
            "#626: the toast must not expire under a parked pointer"
        );
        assert_eq!(timers.hover_count(), 1);
    }

    #[test]
    fn close_and_identical_repost_keeps_the_toast() {
        // The variant `replace_card` alone would never see: `card_content_eq`
        // ignores `timeout` and `created_at`, so a close plus a byte-identical
        // re-post keeps the card mounted and takes `apply_emission`'s
        // `continue` branch — while the entry underneath is replaced just the
        // same. Only the emission-wide renewal pass covers this.
        let mut timers = TimerModel::default();
        let mut card = Card::default();
        timers.post(true);
        card.enter(&mut timers);

        timers.close();
        timers.post(true);
        assert!(timers.armed());

        card.resync(&mut timers);
        assert!(!timers.armed());
        assert_eq!(timers.hover_count(), 1);
    }

    #[test]
    fn plain_rebuild_under_a_parked_pointer_keeps_the_toast() {
        // #593, unchanged: a `replaces_id` re-post that changes the rendered
        // content rebuilds the card, and a stationary pointer sends no `enter`
        // to the successor.
        let mut timers = TimerModel::default();
        let mut card = Card::default();
        timers.post(true);
        card.enter(&mut timers);

        timers.post(true); // re-post; still hovered, so it stays paused
        assert!(!timers.armed());

        let mut card = card.rebuild(&mut timers);
        assert!(!timers.armed(), "the swap crosses no count edge");
        assert_eq!(timers.hover_count(), 1);

        card.resync(&mut timers);
        assert!(!timers.armed());
        assert_eq!(timers.hover_count(), 1, "renewal nets to zero on the count");
    }

    #[test]
    fn sticky_then_finite_repost_keeps_the_toast() {
        // #619: the notification is sticky while hovered, then a re-post turns
        // it finite. The entry — and its count — survives, so the re-post
        // inherits the hold rather than arming behind the pointer.
        let mut timers = TimerModel::default();
        let mut card = Card::default();
        timers.post(false);
        card.enter(&mut timers);
        assert!(!timers.armed(), "a sticky notification has nothing to arm");

        timers.post(true);
        card.resync(&mut timers);
        assert!(!timers.armed());
        assert_eq!(timers.hover_count(), 1);

        card.leave(&mut timers);
        assert!(timers.armed(), "the real leave is what finally arms it");
    }

    #[test]
    fn leaving_after_renewals_arms_exactly_once() {
        let mut timers = TimerModel::default();
        let mut card = Card::default();
        timers.post(true);
        card.enter(&mut timers);
        for _ in 0..5 {
            card.resync(&mut timers);
        }
        assert_eq!(
            timers.hover_count(),
            1,
            "renewal is idempotent on the count"
        );

        card.leave(&mut timers);
        assert!(timers.armed());
        assert_eq!(timers.hover_count(), 0);
        // The `connect_unmap` that follows the pointer leaving must not
        // double-release and strand a second countdown.
        card.leave(&mut timers);
        assert_eq!(timers.hover_count(), 0);
    }

    #[test]
    fn a_second_monitors_copy_renews_without_crossing_an_edge() {
        // Two per-monitor toast copies of one id: `hover_count` is the "is ANY
        // copy hovered" aggregate, so a renewal by one of them runs 2 → 1 → 2
        // and is a complete no-op.
        let mut timers = TimerModel::default();
        let (mut a, mut b) = (Card::default(), Card::default());
        timers.post(true);
        a.enter(&mut timers);
        b.enter(&mut timers);
        assert_eq!(timers.hover_count(), 2);

        a.resync(&mut timers);
        b.resync(&mut timers);
        assert_eq!(timers.hover_count(), 2);
        assert!(!timers.armed());

        a.leave(&mut timers);
        assert!(!timers.armed(), "one copy is still hovered");
        b.leave(&mut timers);
        assert!(timers.armed());
    }

    #[test]
    fn renewal_on_a_closed_notification_is_inert() {
        // A close with no re-post: there is no entry, both calls find nothing,
        // and the card's eventual unmap is equally harmless.
        let mut timers = TimerModel::default();
        let mut card = Card::default();
        timers.post(true);
        card.enter(&mut timers);
        timers.close();

        card.resync(&mut timers);
        card.leave(&mut timers);
        assert!(!timers.armed());
        assert_eq!(timers.hover_count(), 0);
    }
}

// ── Reentrancy regression tests ───────────────────────────────────────────────

/// The `RefCell`-across-a-GTK-call abort class (#674) for this file — #673's
/// headline site.
///
/// ## Why there is no production change alongside these tests
///
/// #758's survey recorded `apply_emission`/`route_emission` as unreachable
/// because their loop bodies lived inline inside `bind(…)` closures. That is no
/// longer true: both are plain private free functions taking their inputs
/// explicitly, in exactly the shape `widgets/workspaces.rs`'s
/// `update_workspaces` has, so a colocated test reaches them by module privacy
/// alone. Nothing in `apply_emission` was extracted, reordered, or reshaped to
/// make this file compile.
///
/// ## Why this needs `App::run` when `workspaces.rs`'s equivalent does not
///
/// `apply_emission` takes a `&ToastView`, and `ToastView` carries a `Monitor`
/// (`build_overflow_card`'s click target). `Monitor`'s only constructor is
/// `pub(crate)` to `hytte-ui`, so the sole way to obtain one from this crate is
/// `App::monitors` — which exists only inside a running `App::run` body.
/// `test_monitor` does that once, lazily, and caches the result; see its doc.
///
/// ## Why the probe is `unmap` and not `destroy`
///
/// `apply_emission`'s own comment names the synchronous emission this site
/// actually has: unmapping a card fires its `connect_unmap` → `resume_expiry`,
/// and #593's keep-the-card-mounted design is built on that being immediate.
/// So the probe rides `unmap`, fired from inside `vbox.remove()` itself —
/// earlier in the take window than any dispose, and the real pairing rather
/// than a contrived one.
///
/// A `destroy` probe was tried first and is *not* usable here: with the toast
/// window mapped, a notification card is still referenced after
/// `vbox.remove()` (it holds the focusable dismiss button), so it is not
/// disposed inside the loop and the handler never fires — the test would have
/// been green while covering nothing. Hence the explicit `is_mapped` assertion
/// in `seeded`: a card that never mapped emits no `unmap` either.
///
/// ## The guarantee, stated narrowly
///
/// Each test asserts one thing: a synchronous GTK emission that re-enters
/// `apply_emission` on the same view does **not** raise `BorrowMutError`.
/// That is *not* a claim that re-entrant emissions compose — they do not, and
/// deliberately so. The outer pass's closing write-back clobbers whatever the
/// inner pass stored, and the inner pass renders against cells holding empty
/// `Default`s (the trade-off `apply_emission`'s own comment states). What is
/// under test is only that the process survives, because the pre-#673 spelling
/// did not: a `BorrowMutError` unwinding out of a glib signal emission is a
/// "panic in a function that cannot unwind", i.e. `SIGABRT`, which takes the
/// whole shell down rather than dropping one toast (#663 hit this for real).
///
/// Production does not re-enter here today — `route_emission` is pumped from a
/// `spawn_local`ed `for_each`, which cannot re-poll itself synchronously — so
/// this is defence in depth against a *new* synchronous emission being
/// introduced into one of the loops, which is precisely the regression #674
/// wants caught.
///
/// Needs a real display server (`xvfb-run`), hence the `system-tests` gate.
#[cfg(all(test, feature = "system-tests"))]
mod reentrancy_tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashSet;
    use std::rc::Rc;
    use std::time::Duration;

    use hytte::gtk::{self, glib, prelude::*};
    use hytte::prelude::*;
    use hytte::services::notifications::{Notification, Urgency};

    use super::{MAX_VISIBLE_NONCRITICAL, ToastView, apply_emission, build_toast_view};

    thread_local! {
        /// The output every view in this module is built for, captured once by
        /// [`test_monitor`]. `Monitor` is `!Send` and this binary's
        /// `#[gtk::test]`s all share one thread, so a `thread_local!` is the
        /// natural home and needs no synchronisation.
        static TEST_MONITOR: RefCell<Option<Monitor>> = const { RefCell::new(None) };
    }

    /// A real `Monitor`, captured from a one-shot `App::run`.
    ///
    /// **Once, lazily, and cached** for two reasons. `App::run` has
    /// process-global side effects (`adw::init`, the dark colour scheme, the
    /// default stylesheet, a leaked application hold), and this is a unit-test
    /// binary shared with several hundred other tests — running it per test
    /// would repeat all of that per test. And `tests/overlay_reentrancy.rs`
    /// documents that a second `App::run` in one binary panics on duplicate
    /// service registration; that constraint does not bite here (this builder
    /// registers no services at all), but there is no reason to lean on it.
    ///
    /// `#[gtk::test]` runs every test in this binary on one thread, so the
    /// cache needs no locking and cannot be raced regardless of test order.
    fn test_monitor() -> Monitor {
        if let Some(monitor) = TEST_MONITOR.with(|cell| cell.borrow().clone()) {
            return monitor;
        }
        App::new("mov.vibec0re.trollshell.test.notifications-reentrancy")
            .run(|app| {
                let first = app.monitors().first().cloned();
                TEST_MONITOR.with(|cell| *cell.borrow_mut() = first);
                app.quit();
            })
            .expect("App::run");
        TEST_MONITOR
            .with(|cell| cell.borrow().clone())
            .expect("the display server must report at least one output; `xvfb-run` provides one")
    }

    /// A notification whose rendered fields are a pure function of `id` and
    /// `body`, so two calls with the same arguments compare equal under
    /// `card_content_eq` and the card is *kept* rather than rebuilt. Passing a
    /// different `body` for the same `id` is how a test reaches the rebuild arm.
    fn notif(id: u32, body: &str) -> Notification {
        Notification {
            id,
            app_name: "Fractal".to_owned(),
            app_icon: String::new(),
            summary: format!("toast {id}"),
            body: body.to_owned(),
            urgency: Urgency::Normal,
            timeout: Some(Duration::from_secs(5)),
            actions: Vec::new(),
            image: None,
            created_at: 1_000,
        }
    }

    /// No per-app mutes, DND off — every test here is about the borrow
    /// discipline, not the visibility gates.
    fn unmuted() -> HashSet<String> {
        HashSet::new()
    }

    /// `notif(1..=n, "a")`, the shape the overflow test needs on both sides of
    /// the cap.
    fn toasts(n: u32) -> Vec<Notification> {
        (1..=n).map(|id| notif(id, "a")).collect()
    }

    /// A view seeded by a real first emission, pumped until its cards are
    /// actually mapped.
    ///
    /// The pump is load-bearing, not hygiene: `apply_emission`'s closing
    /// `set_visible(true)` only *starts* the toplevel's realize/map, and a card
    /// that never mapped emits no `unmap` when it is removed — every test here
    /// would go green while exercising nothing. `Rc` because the reentrant
    /// handlers need a second handle, and because that is what `TOAST_WINDOWS`
    /// stores in production.
    fn seeded(initial: &[Notification]) -> Rc<ToastView> {
        let view = Rc::new(build_toast_view(&test_monitor()));
        apply_emission(&view, initial, false, &unmuted());
        let ctx = glib::MainContext::default();
        while ctx.iteration(false) {}
        view
    }

    /// The mounted card for `id`, asserted mapped so a test cannot pass
    /// vacuously on a probe that could never fire. See [`seeded`].
    fn mapped_card(view: &ToastView, id: u32) -> gtk::Widget {
        let card = view.card_map.borrow()[&id].widget.clone();
        assert!(
            card.is_mapped(),
            "toast {id}'s card must be mapped before the pass under test; an unmapped card emits \
             no `unmap` when it is removed, so the probe below would never fire"
        );
        card
    }

    /// Sorted ids currently tracked in `card_map`, i.e. what the last
    /// write-back left behind.
    fn card_ids(view: &ToastView) -> Vec<u32> {
        let mut ids: Vec<u32> = view.card_map.borrow().keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Arm `card`'s `unmap` to re-enter `apply_emission` on `view` exactly
    /// once, recording whether it fired while `in_outer` was set — i.e. while
    /// the outer pass was still on the stack.
    ///
    /// Once, because the reentrant pass unmaps cards of its own and an
    /// unguarded probe would recurse without bound.
    fn arm_reentry(
        card: &gtk::Widget,
        view: &Rc<ToastView>,
        in_outer: &Rc<Cell<bool>>,
        reentrant: Vec<Notification>,
    ) -> Rc<Cell<Option<bool>>> {
        let fired_inside = Rc::new(Cell::new(None::<bool>));
        let view = Rc::clone(view);
        let in_outer = Rc::clone(in_outer);
        let recorder = Rc::clone(&fired_inside);
        let armed = Cell::new(true);
        card.connect_unmap(move |_| {
            if !armed.replace(false) {
                return;
            }
            recorder.set(Some(in_outer.get()));
            apply_emission(&view, &reentrant, false, &unmuted());
        });
        fired_inside
    }

    /// Step 1 of the emission, the *removal* loop: `map.remove(id)` hands the
    /// entry out and `vbox.remove(&entry.widget)` unparents it, which unmaps
    /// the card and fires its `unmap` handlers **synchronously**, inside the
    /// loop and inside the window in which both cells are taken.
    ///
    /// Reverting `let mut map = view.card_map.take();` to the pre-#673
    /// `view.card_map.borrow_mut()` makes the reentrant call below hit a live
    /// `RefMut` and abort the binary. With `take()` the cell is free for the
    /// whole pass, so the inner call simply finds an empty map.
    #[gtk::test]
    fn apply_emission_tolerates_a_reentrant_pass_from_a_removed_cards_unmap() {
        let view = seeded(&[notif(1, "a"), notif(2, "b")]);
        assert_eq!(
            card_ids(&view),
            [1, 2],
            "both toasts must be mounted after the seeding pass"
        );

        // True only while the outer `apply_emission` is on the stack, so the
        // probe can record whether it ran inside the pass or was deferred.
        let in_outer = Rc::new(Cell::new(false));
        let card2 = mapped_card(&view, 2);
        let fired_inside = arm_reentry(&card2, &view, &in_outer, vec![notif(1, "a")]);

        in_outer.set(true);
        apply_emission(&view, &[notif(1, "a")], false, &unmuted());
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "the removed card's `unmap` must fire synchronously inside `apply_emission`; if GTK \
             ever defers it this test proves nothing about the borrow discipline — and #593's \
             hover-hold design would be broken too"
        );
        assert_eq!(
            card_ids(&view),
            [1],
            "the outer pass's write-back must still land: re-entry may not leave the cell holding \
             the inner pass's map or an empty one"
        );
    }

    /// Step 2 of the emission, the *rebuild* arm: a same-id notification whose
    /// rendered content changed takes `replace_card`, and `replace_card`'s own
    /// comment on `vbox.remove(&old.widget)` is where this site's
    /// synchronous-unmap claim is written down. The emission lands before the
    /// loop's `map.insert` has put the replacement back, so the reentrant pass
    /// sees the cell mid-diff.
    ///
    /// Same falsification as above: with `borrow_mut()` held the inner call
    /// aborts on `BorrowMutError`.
    #[gtk::test]
    fn apply_emission_does_not_hold_the_card_map_across_replace_card() {
        let view = seeded(&[notif(1, "before")]);

        let in_outer = Rc::new(Cell::new(false));
        let card1 = mapped_card(&view, 1);
        let fired_inside = arm_reentry(&card1, &view, &in_outer, vec![notif(1, "reentrant")]);

        // Same id (so the card is replaced, not removed), different body (so
        // `card_content_eq` fails and the rebuild arm actually runs).
        in_outer.set(true);
        apply_emission(&view, &[notif(1, "after")], false, &unmuted());
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "the superseded card's `unmap` must fire synchronously inside `replace_card`'s \
             removal; without it this test does not exercise the borrow at all"
        );
        assert_eq!(
            card_ids(&view),
            [1],
            "the replacement must still be tracked after a reentrant pass"
        );
    }

    /// The overflow slot — a *different cell*, with the same pre-#673 spelling:
    /// `let mut slot = view.overflow_card.borrow_mut();` held across the
    /// `vbox.remove()` that retires the old "+N more" card and the
    /// `vbox.append()` that would mount its replacement.
    ///
    /// Falsifiable independently of the two above: revert only the
    /// `view.overflow_card.take()` at the head of the overflow block and leave
    /// `card_map` on `take()`, and the reentrant pass gets all the way down to
    /// `view.overflow_card.take()` before it hits the live `RefMut` — so the
    /// `BorrowMutError` names this cell and no other.
    #[gtk::test]
    fn apply_emission_does_not_hold_the_overflow_slot_across_its_own_teardown() {
        // One more non-critical toast than renders individually, so the tail
        // collapses into an overflow card.
        let cap = u32::try_from(MAX_VISIBLE_NONCRITICAL).expect("the cap fits in a u32");
        let view = seeded(&toasts(cap + 1));
        let overflow = view
            .overflow_card
            .borrow()
            .clone()
            .expect("one toast over the cap must collapse into an overflow card");
        assert!(
            overflow.is_mapped(),
            "the overflow card must be mapped before the pass under test; an unmapped card emits \
             no `unmap` when it is removed"
        );

        let in_outer = Rc::new(Cell::new(false));
        let fired_inside = arm_reentry(&overflow, &view, &in_outer, vec![notif(1, "a")]);
        drop(overflow);

        // Back under the cap: the tail empties, so the overflow card is retired
        // and not rebuilt.
        in_outer.set(true);
        apply_emission(&view, &toasts(cap), false, &unmuted());
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "the retired overflow card's `unmap` must fire synchronously inside \
             `apply_emission`'s overflow block; if it does not, this test covers nothing"
        );
        assert!(
            view.overflow_card.borrow().is_none(),
            "the outer pass's write-back must still land: with the tail empty the slot ends None"
        );
    }
}
