//! `org.freedesktop.Notifications` daemon — registers on the session bus and
//! exposes a reactive signal of live notifications for the shell to render.
//!
//! # Usage note
//!
//! Only one process on the session bus may own `org.freedesktop.Notifications`
//! at a time. Disable mako, dunst, or any other notification daemon before
//! starting trollshell, otherwise the name acquisition will fail and the
//! service will keep retrying.
//!
//! That failure is otherwise entirely silent from the shell's point of view —
//! `Notify` calls simply land in the other daemon and nothing ever reaches
//! [`active()`]. [`ownership()`] exposes the live [`OwnState`] so a consumer
//! can say so; `trollshell`'s bell chip binds it (#747).

use futures_signals::signal::{Mutable, Signal};
use hytte_bus::{OwnNameSignal, OwnState};
use hytte_reactive::{Service, registry, runtime, shared};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::task::AbortHandle;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;

// ── Cross-thread shared handle ────────────────────────────────────────────────
//
// `hytte_reactive::registry` is a thread-local — initialised on the GTK main
// thread, empty on hytte-tokio worker threads. Public mutators below
// (`dismiss`, `clear_history`) are called from BOTH threads:
//   - GTK: widget click handlers (toast click, action button, "Clear all").
//   - hytte-tokio: the auto-expire timer in `notify()` and the iface's
//     `close_notification` method, both of which run on the bus connection's
//     worker.
//
// Using `registry::with` from a hytte-tokio thread silently no-ops (no
// handles → early return), which is the root cause of the "history is empty
// / clear-all does nothing" bug: the auto-expire path never reaches the
// `history.insert(...)` line. The cross-thread-safe alternative is
// `hytte_reactive::shared` (the process-global registry mirror): `Service::start`
// publishes this bag with `shared::insert`, and any thread reads it back via
// `shared::get::<NotificationsShared>()` — `Mutable<T>`, `Arc<AtomicU32>`, and
// `Mutex<…>` are all `Send + Sync`.
struct NotificationsShared {
    active: Mutable<Vec<Notification>>,
    history: Mutable<Vec<HistoryEntry>>,
    /// Shared id counter — the **same** `Arc<AtomicU32>` the D-Bus `Notify`
    /// handler (`NotificationsIface::next_id`) allocates from. Stored here so
    /// `post_local`, which runs on a hytte-tokio worker and can only reach the
    /// shared registry (the thread-local counter is GTK-thread-only), draws ids
    /// from that one monotonic sequence — a locally-posted toast can never
    /// collide with a `Notify`-allocated id.
    next_id: Arc<AtomicU32>,
    /// Ownership handle used to emit D-Bus signals directly on the owned
    /// connection without a round-trip method call.
    ownership: OwnNameSignal,
    /// Local-dispatch callbacks for locally-posted notification actions,
    /// keyed by `(notification id, action key)`. Populated by
    /// [`post_local_with_actions`], consumed (removed + run) by
    /// [`invoke_action`] on a matching `(id, key)`, and swept for a whole
    /// `id` by [`dismiss`] whenever that notification closes — by any path
    /// (auto-expiry, user dismiss, or `CloseNotification`) — so a
    /// never-clicked action doesn't linger forever. Externally-posted
    /// (D-Bus `Notify`) notifications never populate this map, so this
    /// field has zero effect on their behaviour. A plain `std::sync::Mutex`
    /// is fine here for the same reason as `POST_LOCAL_SEEN`: every
    /// critical section is non-async map surgery.
    local_actions: Mutex<LocalActionMap>,
    /// Hover-pausable per-id expiry timers (#567). Keyed by notification id,
    /// each entry pairs the pure [`TimerState`] bookkeeping with the live
    /// `sleep → dismiss` task's abort handle. One entry per **live**
    /// notification — sticky ones included, carrying `timeout: None` and no
    /// abort handle, because the hover count has to survive a sticky phase
    /// (#619). Reached from BOTH the GTK main thread (the overlay's motion
    /// controller calls [`pause_expiry`] / [`resume_expiry`]) and hytte-tokio
    /// (the sleep task, and [`dismiss`]'s cleanup) — a plain `std::sync::Mutex`
    /// suffices for the same reason as `local_actions`: every critical section
    /// is non-async map surgery.
    timers: Mutex<TimerMap>,
}

// ── Public data shapes ────────────────────────────────────────────────────────

/// Urgency level of a notification.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

/// A single action attached to a notification.
#[derive(Clone, Debug)]
pub struct Action {
    /// Action key used in the `ActionInvoked` D-Bus signal.
    pub key: String,
    /// User-facing button label.
    pub label: String,
}

/// An image attached to a notification.
///
/// Priority (highest first): `image-data` hint → `image-path` hint →
/// `app_icon` argument.
#[derive(Clone, Debug)]
pub enum NotificationImage {
    /// In-memory raw image from the `image-data` (or legacy `icon_data`) hint.
    ///
    /// Bytes are straight (non-premultiplied) RGBA or RGB depending on
    /// `has_alpha`. Pass `rowstride` directly to `gdk::MemoryTexture::new`
    /// — it may exceed `width * channels` due to alignment padding.
    Raw {
        width: i32,
        height: i32,
        rowstride: i32,
        has_alpha: bool,
        channels: i32,
        data: Vec<u8>,
    },
    /// File path or `file://` URL.
    Path(String),
    /// Named icon from the icon theme (taken from the `app_icon` argument).
    IconName(String),
}

/// A single live notification.
#[derive(Clone, Debug)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
    /// Resolved timeout: `Some(Duration)` for finite, `None` for sticky.
    ///
    /// Mapping from `expire_timeout`, per the freedesktop notification spec:
    /// - `<0` (e.g. `-1`) → `Some(5s)` (server default)
    /// - `0`             → `None` (sticky / never expires)
    /// - `>0`            → `Some(millis)` (as requested)
    pub timeout: Option<Duration>,
    /// Action buttons to display below the notification body.
    pub actions: Vec<Action>,
    /// Image to display on the notification card (thumbnail).
    pub image: Option<NotificationImage>,
    /// When the notification was first shown (Unix seconds).
    pub created_at: u64,
}

/// A dismissed notification kept in the in-memory history ring (capped at 100).
#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub id: u32,
    pub app_name: String,
    pub app_icon: String,
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
    pub image: Option<NotificationImage>,
    /// Action buttons that were attached to the original notification, kept
    /// in history so the drawer can render clickable buttons that re-invoke
    /// them via `invoke_action`.
    pub actions: Vec<Action>,
    /// Why the notification was closed: 1=expired, 2=dismissed, 3=closed-by-call, 4=undefined.
    pub reason: u32,
    /// When the notification was first shown (Unix seconds).
    pub created_at: u64,
    /// When the notification was dismissed (Unix seconds).
    pub dismissed_at: u64,
}

// ── Service handle ────────────────────────────────────────────────────────────

/// Service handle returned by [`service()`].
#[doc(hidden)]
pub struct NotificationsHandles {
    pub(crate) active: Mutable<Vec<Notification>>,
    /// Shared counter for allocating notification IDs. Stored here so that
    /// `NotificationsIface` clones across reconnects share the same sequence.
    pub(crate) _next_id: Arc<AtomicU32>,
    pub(crate) history: Mutable<Vec<HistoryEntry>>,
    /// The `own_name` task's live [`OwnState`], published by [`ownership()`]
    /// so a lost name race is visible in the UI rather than presenting as
    /// "notifications just stopped working" (#747, #653). Also what keeps the
    /// `own_name` task owning `org.freedesktop.Notifications` for the process
    /// lifetime — dropping the last handle would end it.
    ///
    /// Held as the [`OwnNameSignal`] itself since #750; before that it was a
    /// `Mutable<OwnState>` mirror plus a parked forwarding task, because
    /// `OwnNameSignal::signal_cloned` captured `&self` and so could not
    /// satisfy `bind`'s `S: 'static` bound. It carries `+ use<>` now.
    ownership: OwnNameSignal,
}

// ── Service entry-point ───────────────────────────────────────────────────────

/// Marker type for the notifications daemon service.
pub struct NotificationsService;

impl Service for NotificationsService {
    type Handles = NotificationsHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let active = Mutable::new(Vec::new());
        let next_id = Arc::new(AtomicU32::new(1));
        let history = Mutable::new(Vec::new());

        let iface = NotificationsIface {
            active: active.clone(),
            next_id: next_id.clone(),
            history: history.clone(),
        };

        // Own the well-known name + mount the interface. The bus layer handles
        // connection lifecycle, RequestName retries, and per-owner back-off if
        // mako/dunst is camping the name.
        //
        // The OwnNameSignal is stored both in NotificationsHandles (process
        // lifetime keep-alive) and in the shared registry (so dismiss/invoke_action
        // can emit signals directly without a round-trip D-Bus call).
        let ownership =
            hytte_bus::own_name(hytte_bus::BusKind::Session, "org.freedesktop.Notifications")
                .at_path("/org/freedesktop/Notifications", iface)
                .start();

        // Populate the cross-thread shared handle so `dismiss` / `clear_history`
        // can find these Mutables when called from a hytte-tokio worker (the
        // thread-local registry is GTK-only). Calling Service::start a second
        // time would `set` fail silently — services are registered once.
        shared::insert(NotificationsShared {
            active: active.clone(),
            history: history.clone(),
            next_id: next_id.clone(),
            ownership: ownership.clone(),
            local_actions: Mutex::new(HashMap::new()),
            timers: Mutex::new(HashMap::new()),
        });

        NotificationsHandles {
            active,
            _next_id: next_id,
            history,
            ownership,
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the notifications service to register with the hytte runtime.
#[must_use]
pub fn service() -> NotificationsService {
    NotificationsService
}

/// Signal that emits the current list of live notifications.
pub fn active() -> impl Signal<Item = Vec<Notification>> {
    registry::with(|r| {
        r.get::<NotificationsHandles>()
            .expect("notifications::service() not registered")
            .active
            .signal_cloned()
    })
}

/// Signal that emits the history of dismissed notifications, newest-first,
/// capped at 100 entries.
pub fn history() -> impl Signal<Item = Vec<HistoryEntry>> {
    registry::with(|r| {
        r.get::<NotificationsHandles>()
            .expect("notifications::service() not registered")
            .history
            .signal_cloned()
    })
}

/// Signal of the shell's hold on the `org.freedesktop.Notifications` bus name.
///
/// `org.freedesktop.Notifications` is a **session singleton**: exactly one
/// process may own it. Lose that race to mako, dunst, or a second copy of the
/// shell and every `Notify` on the session goes to the winner — [`active()`]
/// stays empty forever and no error is ever raised at us. Nothing about that is
/// distinguishable, from the UI, from "nobody sent a notification", which is
/// what made #653 worth filing: the condition was observable only in the
/// journal, and only at a log level the deployed shell filters out (#746).
///
/// Consumers should read the states as:
///
/// * [`OwnState::Owned`] — working.
/// * [`OwnState::Acquiring`] / [`OwnState::Lost`] — **in flight, say nothing**.
///   `own_name` re-requests 250 ms after a loss and only latches
///   `PermanentlyTaken` after several consecutive losses to the *same* holder,
///   so a bus blip or a reconnect passes through these two routinely. Warning
///   on them would flap.
/// * [`OwnState::PermanentlyTaken`] / [`OwnState::Denied`] — the service is
///   **inert**; this is the pair worth telling the user about.
///
/// Self-clearing: the ownership task re-attempts the moment `NameOwnerChanged`
/// reports the name released, so quitting the rival daemon flips this back to
/// `Owned` without a shell restart.
pub fn ownership() -> impl Signal<Item = OwnState> {
    registry::with(|r| {
        r.get::<NotificationsHandles>()
            .expect("notifications::service() not registered")
            .ownership
            .signal_cloned()
    })
}

/// Clear the in-memory notification history.
pub fn clear_history() {
    registry::with(|r| {
        r.get::<NotificationsHandles>()
            .expect("notifications::service() not registered")
            .history
            .set(Vec::new());
    });
}

/// Dismiss notification `id` with the given reason.
///
/// Reason codes: 1 = expired, 2 = dismissed-by-user, 3 = closed-by-call,
/// 4 = undefined.
///
/// Removes the notification from the active list, pushes it to history, and
/// emits `NotificationClosed` on the session bus.
///
/// Safe to call from any thread (GTK widget handlers, hytte-tokio workers,
/// the auto-expire timer in `notify()`). The local mutation runs
/// synchronously on the calling thread; the bus signal emit is spawned onto
/// the runtime.
pub fn dismiss(id: u32, reason: u32) {
    // Local state mutation. Synchronous — Mutable is thread-safe.
    if let Some(shared) = shared::get::<NotificationsShared>() {
        let removed = {
            let mut list = shared.active.lock_mut();
            let pos = list.iter().position(|n| n.id == id);
            pos.map(|i| list.remove(i))
        };
        if let Some(n) = removed {
            let dismissed_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let entry = HistoryEntry {
                id: n.id,
                app_name: n.app_name,
                app_icon: n.app_icon,
                summary: n.summary,
                body: n.body,
                urgency: n.urgency,
                image: n.image,
                actions: n.actions,
                reason,
                created_at: n.created_at,
                dismissed_at,
            };
            let mut hist = shared.history.lock_mut();
            hist.insert(0, entry);
            hist.truncate(100);
        }

        // Drop any local-dispatch callback still registered for `id` — this
        // close path (expiry, user dismiss, or `close_notification`'s call
        // into `dismiss`) means the action can never be invoked again. A
        // cheap no-op for externally-posted notifications, which never
        // populate this map.
        if let Ok(mut map) = shared.local_actions.lock() {
            clear_local_actions(&mut map, id);
        }

        // Cancel and drop any armed expiry timer for this id. Every close path
        // (auto-expiry, user dismiss, `CloseNotification`) funnels through
        // `dismiss`, so this is the one place the timer bookkeeping is
        // reclaimed — no leaked entry or orphaned sleep task (#567). Runs
        // unconditionally (like the `local_actions` sweep above): a
        // `close_notification` that already pulled the notification out of the
        // active list still needs its timer torn down here.
        if let Ok(mut timers) = shared.timers.lock() {
            clear_timer(&mut timers, id);
        }
    }

    // Emit the NotificationClosed signal directly on the owned connection.
    if let Some(shared) = shared::get::<NotificationsShared>() {
        let ownership = shared.ownership.clone();
        runtime::handle().spawn(async move {
            let result = ownership
                .emit("/org/freedesktop/Notifications", |emitter| async move {
                    NotificationsIface::notification_closed(&emitter, id, reason).await
                })
                .await;
            if let Err(e) = result {
                tracing::warn!(error = %e, id, reason, "NotificationClosed emit failed");
            }
        });
    }
}

/// Dismiss every currently-active notification (reason 2 = dismissed-by-user)
/// and move them all into history.
///
/// Used by the notifications drawer page on open so the bell counter goes to
/// zero when the user views them. Critical-urgency entries are dismissed too
/// — their actions remain accessible from the history page.
///
/// Safe to call from any thread; each id is forwarded to [`dismiss`], which
/// is itself thread-safe.
pub fn dismiss_all() {
    let Some(shared) = shared::get::<NotificationsShared>() else {
        return;
    };
    // Snapshot ids first so we don't hold the read guard across a write
    // (`dismiss` takes its own `lock_mut`).
    let ids: Vec<u32> = shared.active.lock_ref().iter().map(|n| n.id).collect();
    for id in ids {
        dismiss(id, 2);
    }
}

/// Invoke action `action_key` on notification `id`.
///
/// # Local dispatch (locally-posted notifications)
///
/// If `(id, action_key)` has a callback registered via
/// [`post_local_with_actions`], that callback is run **instead of** the
/// D-Bus broadcast below — not in addition to it. A locally-posted
/// notification has no external subscriber that could react to
/// `ActionInvoked` (the shell posted it to itself), so there is nothing for
/// the broadcast to accomplish beyond a wasted round-trip; the callback IS
/// the complete reaction. See [`LocalActionCallback`] for the thread
/// contract the callback runs under.
///
/// # D-Bus broadcast (externally-posted notifications)
///
/// With no local callback registered — always true for notifications an
/// external app posted via the `Notify` D-Bus method — this emits
/// `ActionInvoked` on the session bus exactly as before: a broadcast the
/// originating app filters by `id` and reacts to (e.g. opens its window,
/// posts a reply form). This path, and therefore external notifications'
/// behaviour, is unchanged by local dispatch.
///
/// Both the toast widget and the history page wire action buttons to this.
pub fn invoke_action(id: u32, action_key: &str) {
    if let Some(shared) = shared::get::<NotificationsShared>()
        && let Ok(mut map) = shared.local_actions.lock()
        && let Some(callback) = take_local_action(&mut map, id, action_key)
    {
        drop(map);
        callback();
        return;
    }

    let action_key = action_key.to_string();
    if let Some(shared) = shared::get::<NotificationsShared>() {
        let ownership = shared.ownership.clone();
        runtime::handle().spawn(async move {
            let key_for_log = action_key.clone();
            let result = ownership
                .emit("/org/freedesktop/Notifications", |emitter| async move {
                    NotificationsIface::action_invoked(&emitter, id, &action_key).await
                })
                .await;
            if let Err(e) = result {
                tracing::warn!(error = %e, id, action_key = key_for_log, "ActionInvoked emit failed");
            }
        });
    }
}

// ── Local self-posting ────────────────────────────────────────────────────────

/// A locally-dispatched notification action: a button key/label pair (same
/// shape as [`Action`]) plus the callback [`invoke_action`] runs when that
/// button is clicked. Built with [`LocalAction::new`] and passed to
/// [`post_local_with_actions`].
pub struct LocalAction {
    /// Action key. Must be unique among the actions passed to the same
    /// [`post_local_with_actions`] call — it's the lookup key
    /// [`invoke_action`] uses to find this callback.
    key: String,
    /// User-facing button label, rendered by the same toast/history action
    /// buttons an externally-posted [`Action`] would use.
    label: String,
    /// Run once, on the thread that calls [`invoke_action`] for this
    /// `(id, key)` — see [`LocalActionCallback`] for the full contract.
    callback: LocalActionCallback,
}

impl LocalAction {
    /// Build a local action. `callback` runs at most once — see
    /// [`LocalActionCallback`]'s doc for the thread it runs on and why it
    /// must be `Send`.
    #[must_use]
    pub fn new(
        key: impl Into<String>,
        label: impl Into<String>,
        callback: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            callback: Box::new(callback),
        }
    }
}

/// Callback type backing [`LocalAction`].
///
/// # Thread contract
///
/// The callback runs synchronously, **inline**, on whichever thread calls
/// [`invoke_action`] for the `(id, key)` it was registered under — there is
/// no dispatch/spawn in between. In practice that is always the GTK main
/// thread today: the only two `invoke_action` call sites are button click
/// handlers (`trollshell/src/overlays/notifications.rs`'s toast action row,
/// and the drawer history page), and callbacks may therefore freely touch
/// GTK/GDK/GIO (`gdk::Display::default()`, `gio::AppInfo::…`, …) — as the
/// screenshot toast's Open/Copy callbacks do.
///
/// The `Send` bound exists for **storage**, not dispatch: registered
/// callbacks live in [`NotificationsShared::local_actions`], a static also
/// reached from hytte-tokio worker threads (the auto-expire timer, the
/// D-Bus `close_notification` method), both of which only ever *drop* an
/// unclicked callback via [`clear_local_actions`] — never call it. `Send`
/// makes that cross-thread drop legal; it does not promise (and nothing
/// requires) that the closure itself is safe to *run* off the GTK thread.
/// A useful corollary: since GTK/GDK objects are not `Send`, a `Send`
/// closure structurally cannot hold one across the boundary — it can only
/// capture plain owned data (e.g. a screenshot's file path `String`) and
/// resolve the GTK/GDK/GIO handle fresh when it actually runs.
pub type LocalActionCallback = Box<dyn FnOnce() + Send + 'static>;

/// Map backing [`NotificationsShared::local_actions`]; see that field's doc.
type LocalActionMap = HashMap<(u32, String), LocalActionCallback>;

/// Register `callback` for `(id, key)`. Pure map mutation, factored out of
/// [`post_local_with_actions`] so registration can be unit-tested without
/// the process-global shared registry.
fn register_local_action(
    map: &mut LocalActionMap,
    id: u32,
    key: String,
    callback: LocalActionCallback,
) {
    map.insert((id, key), callback);
}

/// Remove and return the callback registered for `(id, key)`, if any. Pure
/// map mutation, factored out of [`invoke_action`] for unit-testing.
fn take_local_action(map: &mut LocalActionMap, id: u32, key: &str) -> Option<LocalActionCallback> {
    map.remove(&(id, key.to_string()))
}

/// Remove every callback registered for `id`, regardless of key. Pure map
/// mutation, factored out of [`dismiss`] for unit-testing.
fn clear_local_actions(map: &mut LocalActionMap, id: u32) {
    map.retain(|(nid, _), _| *nid != id);
}

// ── Hover-pausable expiry timers (#567) ───────────────────────────────────────
//
// Each finite-timeout notification arms a tokio `sleep(dur) → dismiss(id, 1)`
// task. It used to be fire-and-forget; now it's cancellable so a toast can
// hold its countdown while the pointer hovers it (parity with mako/dunst).
//
// The bookkeeping splits in two, mirroring the file's hytte-tokio vs GTK split:
//   - `TimerState` is pure — just the numbers (resolved timeout, hover count,
//     deadline, recorded remainder). Its start/pause/resume transitions are
//     unit-tested in the hermetic bucket with explicit `Instant`s, no runtime
//     needed.
//   - `TimerEntry` pairs that state with the live task's `AbortHandle`; the
//     effect functions (`set_expiry` / `pause_expiry` / `resume_expiry`)
//     translate a `TimerAction` into a spawn/abort against it.
//
// **One entry per live notification, sticky or not** (#619). A sticky
// notification used to have no entry at all, which made the hover-count the
// overlay maintains unrecordable: `pause_expiry` found nothing, so the widget's
// `holds_hover` flag and `TimerState::hover_count` — two records of the one
// fact "this toast is hovered" — diverged for the whole sticky phase, and a
// `replaces_id` re-post that flipped sticky → finite armed a countdown behind a
// stationary pointer. The entry is therefore created by the first `set_expiry`
// for an id and destroyed only by [`clear_timer`] from [`dismiss`] (the funnel
// for every close path); "sticky" is a `timeout: None` *inside* the state, not
// the absence of the state.

/// Minimum duration re-armed when the last hover leaves. A toast must not
/// vanish the instant the pointer leaves, so the resumed sleep is floored here
/// even if only a sliver remained when the hover began.
const MIN_RESUME: Duration = Duration::from_secs(1);

/// What the effect layer should do after a pure [`TimerState`] transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerAction {
    /// (Re-)arm a fresh sleep of this duration and store its abort handle.
    Arm(Duration),
    /// Abort the currently-armed sleep, if any.
    Abort,
    /// No effect.
    Nothing,
}

/// Pure per-notification expiry bookkeeping — holds no tokio types, so the
/// start/pause/resume math is testable in the hermetic bucket. The live task's
/// [`AbortHandle`] lives beside it in [`TimerEntry`].
///
/// Reachable combinations of the four numbers (`timeout`, `hover_count`,
/// `deadline`, and [`TimerEntry::abort`] alongside them):
///
/// | phase | `timeout` | `hover_count` | `deadline` / `abort` |
/// |---|---|---|---|
/// | finite post, unhovered | `Some(t)` | 0 | `Some` / `Some` — counting down |
/// | finite, hovered | `Some(t)` | ≥1 | `None` / `None` — `remaining` holds the remainder |
/// | finite re-post while hovered | `Some(t')` | ≥1 | `None` / `None` — `remaining` reset to `t'` |
/// | sticky, any hover state | `None` | ≥0 | `None` / `None` — nothing to count |
/// | last hover leaves, finite | `Some(t)` | 0 | `Some` / `Some` — armed from `remaining` |
/// | last hover leaves, sticky | `None` | 0 | `None` / `None` — stays sticky |
///
/// A freshly created entry is the sticky row: [`Default`] is the only
/// constructor, so a state carries a `timeout` only once [`start`] has given it
/// one. That is deliberate — `new(timeout)` used to duplicate `start`'s job and
/// left `Some(t)` / `hover_count: 0` / `deadline: None` constructible, a state
/// no transition can otherwise produce and which reads as "finite but not
/// counting" if anything ever observed it.
///
/// Unreachable by construction, and the four things every transition below
/// preserves: `timeout: Some` with `hover_count == 0` and no `deadline`;
/// `timeout: None` with an armed `deadline`; `hover_count > 0` with an armed
/// `deadline`; a `deadline` without its `abort` (or vice versa, save for the
/// inherent race where a sleep has fired but its `dismiss` — which drops the
/// whole entry — hasn't run yet).
///
/// [`start`]: TimerState::start
#[derive(Debug, Default)]
struct TimerState {
    /// Resolved timeout of the current post: `Some` finite, `None` sticky —
    /// same shape (and source) as [`Notification::timeout`]. Reset on every
    /// `replaces_id` re-post, in *either* direction, and the switch that
    /// decides whether a resume has anything to arm at all.
    timeout: Option<Duration>,
    /// Hover-enters not yet balanced by a leave. A finite countdown is armed
    /// only while this is 0; any positive value means at least one toast copy
    /// is hovered and expiry is held. Tracked for sticky notifications too, so
    /// a re-post that turns one finite inherits the hold instead of counting
    /// down behind a pointer that never moved (#619).
    hover_count: u32,
    /// Deadline of the currently-armed sleep: `Some` while armed, `None` while
    /// paused or sticky. Used to compute the remainder at the moment a hover
    /// pauses it.
    deadline: Option<Instant>,
    /// Remainder recorded at pause, re-armed (floored at [`MIN_RESUME`]) when
    /// the last hover leaves. Set to the full resolved timeout by every finite
    /// [`start`](TimerState::start), so a never-hovered timer resumes with its
    /// full duration; zero until the first `start`, and meaningless (as well as
    /// unread) while `timeout` is `None`, since a sticky state never arms.
    remaining: Duration,
}

impl TimerState {
    /// Adopt the timeout a (re-)post resolved to, restarting a finite countdown
    /// from the full duration.
    ///
    /// Three cases, and the hover count survives all three — it belongs to the
    /// mounted card, which a `replaces_id` re-post does not unmount:
    ///
    /// - **sticky** (`None`): drop the deadline and ask the effect layer to
    ///   abort whatever the post being replaced had armed. Nothing counts down
    ///   until some later post turns the notification finite again.
    /// - **finite, unhovered**: arm the full timeout immediately.
    /// - **finite, hovered**: stay paused, recording the fresh full timeout as
    ///   `remaining` so the last leave arms from it. A re-post must never fire
    ///   out from behind a hover — the whole point of #567.
    fn start(&mut self, timeout: Option<Duration>, now: Instant) -> TimerAction {
        self.timeout = timeout;
        let Some(timeout) = timeout else {
            self.deadline = None;
            return TimerAction::Abort;
        };
        self.remaining = timeout;
        if self.hover_count == 0 {
            self.deadline = Some(now + timeout);
            TimerAction::Arm(timeout)
        } else {
            self.deadline = None;
            TimerAction::Nothing
        }
    }

    /// A hover entered. Increments the count; the first hover pauses the
    /// countdown, recording how much time was left. Further hovers (other
    /// per-monitor toast copies) only bump the count so they can't fight.
    fn pause(&mut self, now: Instant) -> TimerAction {
        self.hover_count += 1;
        if self.hover_count == 1
            && let Some(deadline) = self.deadline.take()
        {
            self.remaining = deadline.saturating_duration_since(now);
            TimerAction::Abort
        } else {
            TimerAction::Nothing
        }
    }

    /// A hover left. A leave with no matching enter (count already 0) is
    /// clamped to a no-op so the count can't go negative and a live task is
    /// never stranded. Only the *last* hover leaving re-arms the countdown,
    /// floored at `floor` — and only if the notification is currently finite: a
    /// sticky one tracks the count purely so a later finite re-post inherits
    /// it, and must not sprout an expiry the moment the pointer leaves (#619).
    fn resume(&mut self, now: Instant, floor: Duration) -> TimerAction {
        if self.hover_count == 0 {
            return TimerAction::Nothing;
        }
        self.hover_count -= 1;
        if self.hover_count > 0 || self.timeout.is_none() {
            return TimerAction::Nothing;
        }
        let dur = self.resume_duration().max(floor);
        self.deadline = Some(now + dur);
        TimerAction::Arm(dur)
    }

    /// The single remainder-vs-restart switch (#567). `self.remaining` resumes
    /// the countdown where it paused — mako's behaviour, and our default. Swap
    /// to `self.timeout.unwrap_or(self.remaining)` to restart the full timeout
    /// on every un-hover. Only ever called with `self.timeout` finite.
    fn resume_duration(&self) -> Duration {
        self.remaining
    }
}

/// A [`TimerState`] paired with its live sleep task's abort handle. `abort` is
/// `Some` exactly while the countdown is armed, `None` while paused or sticky.
#[derive(Default)]
struct TimerEntry {
    state: TimerState,
    abort: Option<AbortHandle>,
}

/// Per-id expiry timers backing [`NotificationsShared::timers`].
type TimerMap = HashMap<u32, TimerEntry>;

/// Spawn the `sleep(dur) → dismiss(id, 1 = expired)` task and return its abort
/// handle. Runs on the shared runtime so it's callable from any thread.
fn spawn_expiry(id: u32, dur: Duration) -> AbortHandle {
    runtime::handle()
        .spawn(async move {
            tokio::time::sleep(dur).await;
            dismiss(id, 1); // 1 = expired
        })
        .abort_handle()
}

/// Record `id`'s resolved timeout — `Some` finite, `None` sticky — creating the
/// bookkeeping entry on first post and re-arming (or standing down) the sleep on
/// every `replaces_id` re-post. The one entry point for "what is this
/// notification's timeout now"; [`dismiss`] is the only thing that removes an
/// entry.
///
/// Called synchronously by the D-Bus `Notify` handler / [`post_local`] **while
/// they hold the `active` write guard**, before the notification becomes
/// observable at all — so a live notification always has an entry by the time
/// any toast can render, let alone be hovered. That is what lets
/// [`pause_expiry`] / [`resume_expiry`] be the sole owners of the hover count
/// instead of sharing that fact with the widget (#619). The pre-#619 split of
/// this into `arm_expiry` + a `disarm_expiry` that *removed* the entry is
/// exactly how the two drifted: a sticky post left nothing for the overlay's
/// hold to land on, so the next finite re-post minted a `hover_count: 0` entry
/// under a hovered card.
///
/// Callable from any thread; it only touches the cross-thread shared registry.
fn set_expiry(id: u32, timeout: Option<Duration>) {
    let Some(shared) = shared::get::<NotificationsShared>() else {
        return;
    };
    if let Ok(mut timers) = shared.timers.lock() {
        apply_expiry(&mut timers, id, timeout, Instant::now(), |dur| {
            Some(spawn_expiry(id, dur))
        });
    }
}

/// Map-level half of [`set_expiry`]: create-or-update `id`'s entry for a
/// (re-)post that resolved to `timeout`, cancel whatever that entry had armed,
/// and — when the transition calls for a fresh countdown — install the handle
/// `spawn` returns for it.
///
/// **Never removes an entry.** [`clear_timer`], from [`dismiss`], is the only
/// remover; a sticky re-post stands the countdown down in place, keeping the
/// hover count a mounted card may still be holding. That is the whole of #619,
/// so it is asserted directly in the hermetic tests — which is also why the
/// spawn is a parameter rather than a call to [`spawn_expiry`]: it lets a test
/// drive the real map surgery, and observe what it would have armed, with no
/// tokio runtime in sight. Same reason `clear_timer` is factored out.
fn apply_expiry(
    timers: &mut TimerMap,
    id: u32,
    timeout: Option<Duration>,
    now: Instant,
    spawn: impl FnOnce(Duration) -> Option<AbortHandle>,
) {
    let entry = timers.entry(id).or_default();
    // Cancel any task already armed for this id (a `replaces_id` re-post) so a
    // stale sleep can't dismiss the refreshed notification early — or "expire"
    // one that just turned sticky. Unconditional rather than driven off the
    // returned action: it must cover `Nothing` too (a re-post arriving while a
    // hover is held), and aborting an already-cleared handle is free.
    if let Some(handle) = entry.abort.take() {
        handle.abort();
    }
    if let TimerAction::Arm(dur) = entry.state.start(timeout, now) {
        entry.abort = spawn(dur);
    }
}

/// Remove and cancel the expiry timer for `id`. Called from [`dismiss`] — the
/// funnel for every close path — so a closed toast leaves behind no armed task
/// or bookkeeping entry. The *only* place an entry is dropped: a notification
/// that merely turned sticky keeps its entry (and its hover count) via
/// [`set_expiry`]. Pure map surgery over the abort handle; factored out for
/// unit-testing.
fn clear_timer(timers: &mut TimerMap, id: u32) {
    if let Some(entry) = timers.remove(&id)
        && let Some(handle) = entry.abort
    {
        handle.abort();
    }
}

/// Pause notification `id`'s auto-expiry while the pointer hovers its toast
/// (#567). The first hovering copy cancels the sleep and records the remaining
/// time; further copies only bump the hover-count. Idempotent guarding lives
/// in the overlay (each card contributes at most one enter).
///
/// A **sticky** notification takes the hold too (#619) — there is no sleep to
/// cancel, but the count is what a later finite re-post reads to know it must
/// stay paused. The service is therefore the single record of "this toast is
/// hovered" for the notification's whole life, whatever its timeout does in
/// between.
///
/// Called from the GTK main thread (the toast's `EventControllerMotion`); the
/// cancelled task ran on hytte-tokio. No-op only after the notification has
/// closed (entry already cleared by [`dismiss`]), where there is nothing left to
/// hold.
pub fn pause_expiry(id: u32) {
    let Some(shared) = shared::get::<NotificationsShared>() else {
        return;
    };
    if let Ok(mut timers) = shared.timers.lock()
        && let Some(entry) = timers.get_mut(&id)
        && entry.state.pause(Instant::now()) == TimerAction::Abort
        && let Some(handle) = entry.abort.take()
    {
        handle.abort();
    }
}

/// Resume notification `id`'s auto-expiry when the pointer leaves its toast
/// (#567). Only the last hover leaving re-arms the countdown — with the
/// recorded remainder, floored at [`MIN_RESUME`] so the toast doesn't vanish
/// the instant the pointer leaves. A sticky notification releases its hold with
/// nothing armed (#619). See [`pause_expiry`] for the threading.
pub fn resume_expiry(id: u32) {
    let Some(shared) = shared::get::<NotificationsShared>() else {
        return;
    };
    if let Ok(mut timers) = shared.timers.lock()
        && let Some(entry) = timers.get_mut(&id)
        && let TimerAction::Arm(dur) = entry.state.resume(Instant::now(), MIN_RESUME)
    {
        // Invariant: `abort` is None here (the matching pause cleared it).
        // Cancel defensively regardless so a logic drift can never strand a
        // live task behind a freshly-armed one.
        if let Some(handle) = entry.abort.take() {
            handle.abort();
        }
        entry.abort = Some(spawn_expiry(id, dur));
    }
}

/// Key for the [`post_local`] rate-limiter: `(app_name, summary, body)`.
type RateLimitKey = (String, String, String);
/// Last-emit instants keyed by [`RateLimitKey`].
type RateLimitMap = HashMap<RateLimitKey, Instant>;

/// Rate-limit window for repeated identical [`post_local`] toasts.
///
/// A user-triggered command that keeps failing (e.g. a Wi-Fi connect a
/// flapping daemon retries) shouldn't stack up identical toasts — repeats of
/// the same `(app_name, summary, body)` inside this window are dropped.
const POST_LOCAL_RATE_LIMIT: Duration = Duration::from_secs(10);

/// Backing store for the [`post_local`] rate-limiter. A plain
/// `std::sync::Mutex` (not the async one) is fine — the critical section is a
/// map GC + lookup + insert with no `.await`.
static POST_LOCAL_SEEN: OnceLock<Mutex<RateLimitMap>> = OnceLock::new();

/// Pure rate-limit decision, factored out of [`rate_limit_allow`] so it can be
/// unit-tested without the process-global map. Returns `true` (and records
/// `now`) when `key` hasn't been seen within `window`; `false` for a repeat.
/// Opportunistically drops entries older than `window` so the map can't grow
/// unbounded.
fn rate_limit_check(
    seen: &mut RateLimitMap,
    key: RateLimitKey,
    now: Instant,
    window: Duration,
) -> bool {
    seen.retain(|_, ts| now.duration_since(*ts) < window);
    if seen
        .get(&key)
        .is_some_and(|ts| now.duration_since(*ts) < window)
    {
        return false;
    }
    seen.insert(key, now);
    true
}

/// Process-global wrapper over [`rate_limit_check`]. Fails open (allows the
/// toast) if the lock is poisoned — a stray duplicate beats a swallowed error.
fn rate_limit_allow(app_name: &str, summary: &str, body: &str) -> bool {
    let map = POST_LOCAL_SEEN.get_or_init(|| Mutex::new(RateLimitMap::new()));
    let Ok(mut guard) = map.lock() else {
        return true;
    };
    let key = (app_name.to_string(), summary.to_string(), body.to_string());
    rate_limit_check(&mut guard, key, Instant::now(), POST_LOCAL_RATE_LIMIT)
}

/// Inject a synthetic notification into the daemon's own active set, so the
/// shell can surface its **own** failures — a failed fire-and-forget command
/// (Wi-Fi/VPN connect, a screenshot save, …) — as a native toast with no D-Bus
/// round-trip. The toast renders exactly like an external `Notify` and
/// auto-expires after the server-default timeout via the same spawn/[`dismiss`]
/// pattern the D-Bus handler uses.
///
/// Callable from **any** thread: it only touches the cross-thread shared
/// registry (never the GTK-thread-local registry), so command helpers running
/// on the hytte-tokio runtime can post directly.
///
/// # Urgency / Do-Not-Disturb
///
/// Pass [`Urgency::Critical`] for **error-scope** self-toasts. The toast
/// overlay suppresses non-critical toasts while Do-Not-Disturb is on or the
/// posting app is muted; only `Critical` bypasses both gates. A failure the
/// user just triggered by clicking should not be silently eaten by DND, so the
/// swept Wi-Fi/VPN error arms all pass `Critical`.
///
/// # Rate-limiting
///
/// Identical `(app_name, summary, body)` toasts within
/// [`POST_LOCAL_RATE_LIMIT`] are dropped so a flapping daemon can't spam the
/// surface.
///
/// No-op if the notifications service isn't registered (e.g. headless tests) —
/// callers should keep their `tracing::warn!` as the durable record.
///
/// Plain-toast convenience wrapper over [`post_local_with_actions`] with no
/// actions attached; see that function to attach Open/Copy-style action
/// buttons with local callbacks.
pub fn post_local(app_name: &str, summary: &str, body: &str, urgency: Urgency) {
    post_local_with_actions(app_name, summary, body, urgency, Vec::new());
}

/// Like [`post_local`], but attaches `actions` as clickable buttons on the
/// toast (same rendering path as an externally-posted notification's
/// actions — the toast/history UI doesn't distinguish local from external).
///
/// Each [`LocalAction`]'s callback is registered under the freshly-allocated
/// notification id and its key, so a click dispatches to that callback via
/// [`invoke_action`] rather than the D-Bus `ActionInvoked` broadcast — see
/// [`invoke_action`]'s "Local dispatch" doc section for why the broadcast is
/// skipped entirely rather than also firing.
///
/// Registered callbacks are swept if the toast closes unclicked (expiry,
/// dismiss, …) — see [`NotificationsShared::local_actions`].
///
/// No-op (actions dropped) if the notifications service isn't registered,
/// mirroring [`post_local`].
pub fn post_local_with_actions(
    app_name: &str,
    summary: &str,
    body: &str,
    urgency: Urgency,
    actions: Vec<LocalAction>,
) {
    let Some(shared) = shared::get::<NotificationsShared>() else {
        return;
    };

    if !rate_limit_allow(app_name, summary, body) {
        return;
    }

    // Allocate from the SAME atomic the D-Bus `Notify` handler uses — one
    // monotonic sequence, so this id can never collide with a `Notify`-issued
    // one (see `NotificationsShared::next_id`).
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);

    // Register each action's callback under (id, key) and build the plain
    // Action list the Notification carries for rendering. If the mutex is
    // poisoned, fail safe by posting the toast with no action buttons rather
    // than buttons that would dispatch to nothing.
    let mut ui_actions = Vec::with_capacity(actions.len());
    if let Ok(mut map) = shared.local_actions.lock() {
        for action in actions {
            let LocalAction {
                key,
                label,
                callback,
            } = action;
            ui_actions.push(Action {
                key: key.clone(),
                label,
            });
            register_local_action(&mut map, id, key, callback);
        }
    } else {
        tracing::warn!(
            id,
            "notifications: local_actions mutex poisoned, posting without actions"
        );
    }

    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let notification = Notification {
        id,
        app_name: app_name.to_string(),
        app_icon: String::new(),
        summary: summary.to_string(),
        body: strip_markup(body),
        urgency,
        timeout: Some(DEFAULT_TIMEOUT),
        actions: ui_actions,
        image: None,
        created_at,
    };

    // Arm the hover-pausable auto-dismiss timer, then publish — both under the
    // one `active` write guard, expiry first. See `notify` for why the order and
    // the shared guard are both load-bearing (#619).
    {
        let mut list = shared.active.lock_mut();
        set_expiry(id, Some(DEFAULT_TIMEOUT));
        list.push(notification);
    }
    tracing::debug!(id, app_name, summary, "local notification posted");
}

// ── D-Bus interface ───────────────────────────────────────────────────────────

#[derive(Clone)]
struct NotificationsIface {
    active: Mutable<Vec<Notification>>,
    next_id: Arc<AtomicU32>,
    history: Mutable<Vec<HistoryEntry>>,
}

impl NotificationsIface {
    /// Remove `id` from the active list and push it to history.
    /// Returns the removed notification if found.
    fn remove_from_active(&self, id: u32) -> Option<Notification> {
        let mut list = self.active.lock_mut();
        let pos = list.iter().position(|n| n.id == id)?;
        Some(list.remove(pos))
    }

    fn push_to_history(&self, n: Notification, reason: u32) {
        let dismissed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let entry = HistoryEntry {
            id: n.id,
            app_name: n.app_name,
            app_icon: n.app_icon,
            summary: n.summary,
            body: n.body,
            urgency: n.urgency,
            image: n.image,
            actions: n.actions,
            reason,
            created_at: n.created_at,
            dismissed_at,
        };
        let mut hist = self.history.lock_mut();
        hist.insert(0, entry);
        hist.truncate(100);
    }
}

// zbus's `#[interface]` macro requires every method to be `async fn` even
// when the body doesn't await. Allowing at the impl-block keeps the noise
// out of each method.
#[allow(clippy::unused_async)]
#[zbus::interface(name = "org.freedesktop.Notifications")]
impl NotificationsIface {
    /// Show a notification. Returns the notification id.
    ///
    /// `expire_timeout` (freedesktop notification spec):
    ///   - `<0` (e.g. `-1`) → use server default (5s)
    ///   - `0`             → notification is sticky (never auto-dismissed)
    ///   - `>0`            → milliseconds
    // Signature mirrors the `org.freedesktop.Notifications.Notify` D-Bus
    // method — wire shape is fixed, can't bundle args.
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        // Determine id: honour replaces_id or allocate a new one.
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        };

        // Parse urgency from hints (key "urgency", type u8).
        let urgency = hints
            .get("urgency")
            .and_then(|v| u8::try_from(v.try_clone().ok()?).ok())
            .map(|b| match b {
                0 => Urgency::Low,
                2 => Urgency::Critical,
                _ => Urgency::Normal,
            })
            .unwrap_or_default();

        // Resolve timeout (see `resolve_timeout`).
        let timeout = resolve_timeout(expire_timeout);

        // Strip any HTML-like markup from body conservatively.
        let body_clean = strip_markup(body);

        // Parse actions from the flat interleaved [key, label, ...] array.
        let parsed_actions = parse_actions(&actions);

        // Parse image from hints, falling back to app_icon.
        let image = parse_image(&hints, app_icon);

        // Record when this notification first appeared.
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let notification = Notification {
            id,
            app_name: app_name.to_string(),
            app_icon: app_icon.to_string(),
            summary: summary.to_string(),
            body: body_clean,
            urgency,
            timeout,
            actions: parsed_actions,
            image,
            created_at,
        };

        // Record the resolved timeout on the hover-pausable expiry bookkeeping,
        // then update the active list (replace in-place if same id, else push) —
        // both under the one `active` write guard, expiry FIRST.
        //
        // `set_expiry` arms the auto-dismiss sleep for a finite timeout; for a
        // sticky one (`timeout == None`) it stands the countdown down —
        // cancelling the stale sleep a `replaces_id` re-post left behind — while
        // keeping the entry, so a hover taken during the sticky phase is still
        // recorded (#619).
        //
        // The order is load-bearing in both directions. Dropping the write guard
        // is what wakes the GTK-side toast consumer, and a card appended to an
        // already-mapped toast surface can be hovered as soon as it is
        // allocated — no compositor round-trip to hide behind — so the entry has
        // to exist before the write is observable, or `pause_expiry` no-ops and
        // the card's hold is recorded nowhere: #619 again, by a narrower race.
        // Keeping `set_expiry` *inside* the guard rather than merely ahead of it
        // also stops a 1ms `expire_timeout` from running `dismiss` before the
        // notification is in the list at all, which would strand it on screen
        // with no timer. Both mutexes are taken in the `active` → `timers` order
        // `dismiss` uses, so the pair cannot invert.
        {
            let mut list = self.active.lock_mut();
            crate::notifications::set_expiry(id, timeout);
            if let Some(existing) = list.iter_mut().find(|n| n.id == id) {
                *existing = notification.clone();
            } else {
                list.push(notification.clone());
            }
        }

        tracing::debug!(id, app_name, summary, "notification added");

        id
    }

    /// Close a notification by id (reason 3 = closed by call).
    async fn close_notification(&self, id: u32) {
        if let Some(n) = self.remove_from_active(id) {
            self.push_to_history(n, 3);
        }
        crate::notifications::dismiss(id, 3);
    }

    /// Return the capabilities this server implements.
    #[allow(clippy::unused_self)]
    fn get_capabilities(&self) -> Vec<String> {
        vec![
            "body".to_string(),
            "icon-static".to_string(),
            "actions".to_string(),
        ]
    }

    /// Return server identification tuple.
    #[allow(clippy::unused_self)]
    fn get_server_information(&self) -> (&str, &str, &str, &str) {
        ("trollshell", "hannig.cc", "0.4.0", "1.2")
    }

    /// Emitted when a notification is closed.
    ///
    /// `reason` codes: 1 = expired, 2 = dismissed, 3 = closed-by-call,
    /// 4 = undefined.
    #[zbus(signal)]
    async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    /// Emitted when an action on a notification is invoked.
    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;
}

// ── Helper: resolve timeout ───────────────────────────────────────────────────

/// Server default applied when an app delegates the timeout (`expire_timeout
/// < 0`, conventionally `-1`).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolve a freedesktop `expire_timeout` into a finite auto-dismiss duration
/// (`Some`) or a sticky notification (`None`).
///
/// Per the [notification spec][spec]:
/// - `< 0` (conventionally `-1`) → server decides; we apply [`DEFAULT_TIMEOUT`].
/// - `0` → never expire (sticky).
/// - `> 0` → that many milliseconds.
///
/// Note the `0` and `-1` cases are easy to invert — `-1` ("you decide") is by
/// far the most common value (it's `notify-send`'s default), so mapping it to
/// sticky makes ordinary toasts pile up forever (see issue #15).
///
/// [spec]: https://specifications.freedesktop.org/notification-spec/latest/protocol.html
fn resolve_timeout(expire_timeout: i32) -> Option<Duration> {
    match expire_timeout.cmp(&0) {
        std::cmp::Ordering::Less => Some(DEFAULT_TIMEOUT),
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => {
            // expire_timeout > 0 here so the cast to u64 is safe.
            #[allow(clippy::cast_sign_loss)]
            Some(Duration::from_millis(expire_timeout as u64))
        }
    }
}

// ── Helper: parse actions ─────────────────────────────────────────────────────

/// Parse the flat interleaved `[key1, label1, key2, label2, ...]` actions
/// array from the `Notify` call into a `Vec<Action>`.
fn parse_actions(raw: &[String]) -> Vec<Action> {
    raw.chunks_exact(2)
        .map(|chunk| Action {
            key: chunk[0].clone(),
            label: chunk[1].clone(),
        })
        .collect()
}

// ── Helper: parse image ───────────────────────────────────────────────────────

/// Resolve the image for a notification.
///
/// Priority: `image-data` hint (or legacy `icon_data`) > `image-path` hint >
/// `app_icon` argument.
fn parse_image(hints: &HashMap<String, OwnedValue>, app_icon: &str) -> Option<NotificationImage> {
    // 1. Try image-data (modern key) then icon_data (legacy underscore key).
    for key in &["image-data", "icon_data"] {
        if let Some(val) = hints.get(*key)
            && let Some(img) = decode_image_data(val)
        {
            return Some(img);
        }
    }

    // 2. Try image-path hint.
    if let Some(val) = hints.get("image-path")
        && let Ok(path) = String::try_from(val.try_clone().ok()?)
        && !path.is_empty()
    {
        return Some(NotificationImage::Path(path));
    }

    // 3. Fall back to app_icon argument.
    if app_icon.is_empty() {
        return None;
    }
    if app_icon.starts_with("file://") || app_icon.starts_with('/') {
        return Some(NotificationImage::Path(app_icon.to_string()));
    }
    Some(NotificationImage::IconName(app_icon.to_string()))
}

/// Upper bound on any single image dimension handed to GTK — a sanity guard so
/// a bogus multi-gigapixel `image-data` payload can't allocate the world (real
/// notification thumbnails are at most a few hundred px). `32768` is well above
/// any legitimate icon yet keeps `rowstride * height` comfortably inside `i64`.
const MAX_IMAGE_DIM: i32 = 1 << 15;

/// Whether an `image-data` payload is self-consistent enough to become a
/// `gdk::MemoryTexture` without tripping its size `g_return_val_if_fail` — which
/// the gtk-rs binding turns into a panic on the GTK main thread.
///
/// `image-data` is an arbitrary session-bus payload (any app's `Notify` hint),
/// so `width`/`height`/`rowstride`/`data.len()` must be cross-checked before the
/// widget hands them to the texture constructor.
///
/// `bytes_per_pixel` is the stride unit GDK actually reads — 4 for the
/// `has_alpha` RGBA format, 3 for RGB — so the buffer-length check matches the
/// real texture read rather than the (possibly inconsistent) wire `channels`
/// field. All arithmetic is done in `i64` so no product can overflow `i32`.
fn image_data_consistent(
    width: i32,
    height: i32,
    rowstride: i32,
    bytes_per_pixel: i32,
    data_len: usize,
) -> bool {
    if width <= 0 || height <= 0 || rowstride <= 0 || bytes_per_pixel <= 0 {
        return false;
    }
    if width > MAX_IMAGE_DIM || height > MAX_IMAGE_DIM {
        return false;
    }
    let min_row = i64::from(width) * i64::from(bytes_per_pixel);
    if i64::from(rowstride) < min_row {
        return false;
    }
    // GDK reads `rowstride` bytes for each of the first `height - 1` rows, then
    // `width * bytes_per_pixel` bytes of the final row.
    let needed = i64::from(rowstride) * i64::from(height - 1) + min_row;
    i64::try_from(data_len).is_ok_and(|len| len >= needed)
}

/// Decode an `image-data` / `icon_data` `OwnedValue` of type `(iiibiiay)`.
///
/// Returns `None` for a malformed payload (bad dimensions/stride or a buffer too
/// short for the declared geometry) so `parse_image` falls through to the
/// `image-path` / `app_icon` path rather than the shell panicking when the
/// widget builds the texture.
fn decode_image_data(val: &OwnedValue) -> Option<NotificationImage> {
    // The spec defines image-data as (iiibiiay):
    //   width: i32, height: i32, rowstride: i32, has_alpha: bool,
    //   bits_per_sample: i32, channels: i32, data: Vec<u8>
    type ImageTuple = (i32, i32, i32, bool, i32, i32, Vec<u8>);
    let cloned = val.try_clone().ok()?;
    let (width, height, rowstride, has_alpha, _bits_per_sample, channels, data) =
        ImageTuple::try_from(cloned).ok()?;
    // The widget builds the texture with a format chosen purely from
    // `has_alpha` (RGBA = 4 bytes/px, RGB = 3), so validate the buffer against
    // that stride unit — not the wire `channels` field, which a buggy or
    // hostile app may set inconsistently with `has_alpha`.
    let bytes_per_pixel = if has_alpha { 4 } else { 3 };
    if !image_data_consistent(width, height, rowstride, bytes_per_pixel, data.len()) {
        tracing::warn!(
            width,
            height,
            rowstride,
            has_alpha,
            channels,
            data_len = data.len(),
            "rejecting malformed notification image-data; falling back to image-path/app_icon"
        );
        return None;
    }
    Some(NotificationImage::Raw {
        width,
        height,
        rowstride,
        has_alpha,
        channels,
        data,
    })
}

// ── Helper: strip HTML-like markup ───────────────────────────────────────────

/// Conservatively strip `<...>` tags from notification body text.
///
/// The freedesktop spec allows a small HTML subset, but v0.4.0 renders
/// notifications as plain text.
fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_TIMEOUT, LocalActionMap, MIN_RESUME, RateLimitMap, TimerAction, TimerEntry,
        TimerMap, TimerState, apply_expiry, clear_local_actions, clear_timer,
        image_data_consistent, rate_limit_check, register_local_action, resolve_timeout,
        take_local_action,
    };
    use std::cell::Cell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    // ── image-data validation (the #425 panic guard) ─────────────────────────

    #[test]
    fn image_data_accepts_tightly_packed_rgba() {
        // 2×2 RGBA, no row padding: rowstride == width*4, len == rowstride*height.
        assert!(image_data_consistent(2, 2, 8, 4, 16));
        // 1×1 RGBA.
        assert!(image_data_consistent(1, 1, 4, 4, 4));
    }

    #[test]
    fn image_data_accepts_row_padding_and_rgb() {
        // Padded RGBA: rowstride 10 > width*4, need = 10*(2-1) + 2*4 = 18.
        assert!(image_data_consistent(2, 2, 10, 4, 18));
        assert!(image_data_consistent(2, 2, 10, 4, 19)); // extra trailing slack is fine
        // 3-byte RGB (has_alpha == false): rowstride == width*3.
        assert!(image_data_consistent(2, 2, 6, 3, 12));
    }

    #[test]
    fn image_data_rejects_short_buffer() {
        // 2×2 RGBA needs 16 bytes; 15 must be rejected, not fed to GTK.
        assert!(!image_data_consistent(2, 2, 8, 4, 15));
        // Padded row: need = 10*1 + 8 = 18; 17 is one short.
        assert!(!image_data_consistent(2, 2, 10, 4, 17));
    }

    #[test]
    fn image_data_rejects_bad_dimensions_and_stride() {
        assert!(!image_data_consistent(0, 2, 8, 4, 16)); // zero width
        assert!(!image_data_consistent(2, 0, 8, 4, 16)); // zero height
        assert!(!image_data_consistent(-4, 2, 8, 4, 16)); // negative width
        assert!(!image_data_consistent(2, -2, 8, 4, 16)); // negative height
        assert!(!image_data_consistent(2, 2, -8, 4, 16)); // negative rowstride
        assert!(!image_data_consistent(2, 2, 0, 4, 16)); // zero rowstride
        assert!(!image_data_consistent(2, 2, 7, 4, 16)); // rowstride < width*bpp
        assert!(!image_data_consistent(2, 2, 8, 0, 16)); // zero bytes/px
    }

    #[test]
    fn image_data_rejects_absurd_dimensions_without_overflow() {
        // Bogus giant dims must be rejected by the sane-bound guard, never panic.
        assert!(!image_data_consistent(i32::MAX, i32::MAX, i32::MAX, 4, 16));
        assert!(!image_data_consistent(1 << 20, 1, 1 << 22, 4, 8));
    }

    #[test]
    fn rate_limit_blocks_identical_repeats_within_window() {
        let mut seen = RateLimitMap::new();
        let window = Duration::from_secs(10);
        let t0 = Instant::now();
        let key = (
            "Wi-Fi".to_string(),
            "Wi-Fi connection failed".to_string(),
            "boom".to_string(),
        );

        // First occurrence: allowed.
        assert!(rate_limit_check(&mut seen, key.clone(), t0, window));
        // Immediate identical repeat: blocked.
        assert!(!rate_limit_check(&mut seen, key.clone(), t0, window));
        // Still inside the window: blocked.
        assert!(!rate_limit_check(
            &mut seen,
            key.clone(),
            t0 + Duration::from_secs(9),
            window,
        ));
        // Past the window: allowed again.
        assert!(rate_limit_check(
            &mut seen,
            key,
            t0 + Duration::from_secs(11),
            window,
        ));
    }

    #[test]
    fn rate_limit_is_per_key() {
        let mut seen = RateLimitMap::new();
        let window = Duration::from_secs(10);
        let t0 = Instant::now();
        let wifi = (
            "Wi-Fi".to_string(),
            "Wi-Fi connection failed".to_string(),
            String::new(),
        );
        let vpn = (
            "VPN".to_string(),
            "VPN connection failed".to_string(),
            String::new(),
        );

        assert!(rate_limit_check(&mut seen, wifi.clone(), t0, window));
        // A different key is independent and allowed at the same instant.
        assert!(rate_limit_check(&mut seen, vpn, t0, window));
        // The first key is still rate-limited.
        assert!(!rate_limit_check(&mut seen, wifi, t0, window));
    }

    #[test]
    fn shared_atomic_ids_never_collide_across_allocators() {
        // post_local (via SHARED) and the D-Bus Notify handler (via
        // NotificationsIface) both fetch_add on the SAME Arc<AtomicU32>, so
        // their id streams interleave without ever overlapping — the
        // collision-safety invariant this feature depends on.
        let counter = Arc::new(AtomicU32::new(1));
        let iface_side = counter.clone(); // NotificationsIface::next_id
        let shared_side = counter.clone(); // NotificationsShared::next_id

        let a = iface_side.fetch_add(1, Ordering::Relaxed); // a Notify
        let b = shared_side.fetch_add(1, Ordering::Relaxed); // a post_local
        let c = iface_side.fetch_add(1, Ordering::Relaxed); // another Notify
        assert_eq!((a, b, c), (1, 2, 3));
        assert!(a != b && b != c && a != c);
    }

    #[test]
    fn negative_timeout_uses_server_default() {
        // -1 ("you decide") is notify-send's default and the common case;
        // it must NOT be sticky (regression test for issue #15).
        assert_eq!(resolve_timeout(-1), Some(DEFAULT_TIMEOUT));
        assert_eq!(resolve_timeout(i32::MIN), Some(DEFAULT_TIMEOUT));
    }

    #[test]
    fn zero_timeout_is_sticky() {
        // Per spec, 0 means never expire.
        assert_eq!(resolve_timeout(0), None);
    }

    #[test]
    fn positive_timeout_is_exact_millis() {
        assert_eq!(resolve_timeout(1), Some(Duration::from_millis(1)));
        assert_eq!(resolve_timeout(2500), Some(Duration::from_millis(2500)));
        assert_eq!(
            resolve_timeout(i32::MAX),
            Some(Duration::from_millis(i32::MAX as u64))
        );
    }

    // ── Local action registry: insert / dispatch / cleanup ──────────────────

    #[test]
    fn local_action_take_removes_and_runs_callback() {
        let mut map = LocalActionMap::new();
        let ran = Arc::new(AtomicBool::new(false));
        let ran_writer = ran.clone();
        register_local_action(
            &mut map,
            1,
            "open".to_string(),
            Box::new(move || ran_writer.store(true, Ordering::SeqCst)),
        );
        assert_eq!(map.len(), 1);

        // Taken exactly once: removed from the map, and running it fires
        // the callback.
        let callback = take_local_action(&mut map, 1, "open").expect("callback registered");
        assert!(map.is_empty());
        assert!(!ran.load(Ordering::SeqCst));
        callback();
        assert!(ran.load(Ordering::SeqCst));
    }

    #[test]
    fn local_action_take_is_scoped_by_id_and_key() {
        let mut map = LocalActionMap::new();
        register_local_action(&mut map, 1, "open".to_string(), Box::new(|| {}));
        register_local_action(&mut map, 1, "copy".to_string(), Box::new(|| {}));
        register_local_action(&mut map, 2, "open".to_string(), Box::new(|| {}));

        // Wrong key: no match, nothing removed.
        assert!(take_local_action(&mut map, 1, "close").is_none());
        assert_eq!(map.len(), 3);

        // Same key, different id: no match either — ids don't collide.
        assert!(take_local_action(&mut map, 3, "open").is_none());
        assert_eq!(map.len(), 3);

        // Exact (id, key) match: removed, siblings untouched.
        assert!(take_local_action(&mut map, 1, "open").is_some());
        assert_eq!(map.len(), 2);
        assert!(map.contains_key(&(1, "copy".to_string())));
        assert!(map.contains_key(&(2, "open".to_string())));
    }

    #[test]
    fn clear_local_actions_drops_only_the_given_id() {
        let mut map = LocalActionMap::new();
        register_local_action(&mut map, 1, "open".to_string(), Box::new(|| {}));
        register_local_action(&mut map, 1, "copy".to_string(), Box::new(|| {}));
        register_local_action(&mut map, 2, "open".to_string(), Box::new(|| {}));

        // Simulates `dismiss(1, ..)`: both of id 1's callbacks go, id 2's
        // stays registered.
        clear_local_actions(&mut map, 1);

        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&(2, "open".to_string())));
    }

    #[test]
    fn clear_local_actions_on_unknown_id_is_a_no_op() {
        let mut map = LocalActionMap::new();
        register_local_action(&mut map, 1, "open".to_string(), Box::new(|| {}));

        clear_local_actions(&mut map, 999);

        assert_eq!(map.len(), 1);
    }

    // ── Hover-pausable expiry timers (#567, #619) ────────────────────────────
    //
    // All pure `TimerState` transitions — no runtime, explicit `Instant`s.
    // `start(Some(d), …)` is a finite post, `start(None, …)` a sticky one; the
    // effect layer ([`set_expiry`]) mirrors each `TimerAction` onto the live
    // sleep task, which needs a runtime and so isn't covered here.

    #[test]
    fn timer_arms_full_timeout_when_not_hovered() {
        let mut s = TimerState::default();
        let now = Instant::now();
        assert_eq!(
            s.start(Some(Duration::from_secs(5)), now),
            TimerAction::Arm(Duration::from_secs(5))
        );
        assert_eq!(s.deadline, Some(now + Duration::from_secs(5)));
        assert_eq!(s.hover_count, 0);
    }

    #[test]
    fn hover_pauses_and_records_remaining() {
        let mut s = TimerState::default();
        let t0 = Instant::now();
        s.start(Some(Duration::from_secs(5)), t0);
        // Pointer enters 2s in: pause the sleep, ~3s recorded as remaining.
        assert_eq!(s.pause(t0 + Duration::from_secs(2)), TimerAction::Abort);
        assert_eq!(s.deadline, None);
        assert_eq!(s.remaining, Duration::from_secs(3));
        assert_eq!(s.hover_count, 1);
    }

    #[test]
    fn resume_rearms_with_remaining_not_full_timeout() {
        // The #567 behaviour call: resume with the REMAINING time, not a fresh
        // full timeout (mako semantics). This is the test that pins the
        // `resume_duration` switch.
        let mut s = TimerState::default();
        let t0 = Instant::now();
        s.start(Some(Duration::from_secs(5)), t0);
        s.pause(t0 + Duration::from_secs(2)); // 3s left
        let leave = t0 + Duration::from_secs(9); // hovered a good while
        assert_eq!(
            s.resume(leave, MIN_RESUME),
            TimerAction::Arm(Duration::from_secs(3))
        );
        assert_eq!(s.deadline, Some(leave + Duration::from_secs(3)));
        assert_eq!(s.hover_count, 0);
    }

    #[test]
    fn resume_floors_tiny_remainder() {
        // Enter with only a sliver left → resume must floor to MIN_RESUME so
        // the toast doesn't vanish the instant the pointer leaves.
        let mut s = TimerState::default();
        let t0 = Instant::now();
        s.start(Some(Duration::from_secs(5)), t0);
        s.pause(t0 + Duration::from_millis(4900)); // 100ms left
        assert_eq!(s.remaining, Duration::from_millis(100));
        assert_eq!(
            s.resume(t0 + Duration::from_secs(30), MIN_RESUME),
            TimerAction::Arm(MIN_RESUME)
        );
    }

    #[test]
    fn multiple_hover_copies_only_last_leave_rearms() {
        // Two per-monitor toast copies both hovered: the hover-count guards
        // one copy leaving from re-arming while the other still holds it.
        let mut s = TimerState::default();
        let t0 = Instant::now();
        s.start(Some(Duration::from_secs(5)), t0);
        assert_eq!(s.pause(t0 + Duration::from_secs(1)), TimerAction::Abort); // copy A enters
        assert_eq!(s.pause(t0 + Duration::from_secs(1)), TimerAction::Nothing); // copy B enters
        assert_eq!(s.hover_count, 2);
        // Copy A leaves — still held by B, stay paused.
        assert_eq!(
            s.resume(t0 + Duration::from_secs(2), MIN_RESUME),
            TimerAction::Nothing
        );
        assert_eq!(s.deadline, None);
        // Copy B leaves — now the countdown re-arms.
        assert!(matches!(
            s.resume(t0 + Duration::from_secs(2), MIN_RESUME),
            TimerAction::Arm(_)
        ));
        assert_eq!(s.hover_count, 0);
    }

    #[test]
    fn stray_leave_without_enter_is_clamped_no_op() {
        // A leave with no matching enter (count already 0) must not underflow
        // and must not re-arm a fresh task over the live one.
        let mut s = TimerState::default();
        let t0 = Instant::now();
        s.start(Some(Duration::from_secs(5)), t0);
        let armed_deadline = s.deadline;
        assert_eq!(
            s.resume(t0 + Duration::from_secs(1), MIN_RESUME),
            TimerAction::Nothing
        );
        assert_eq!(s.hover_count, 0);
        assert_eq!(s.deadline, armed_deadline); // unchanged — no re-arm
    }

    #[test]
    fn replaces_id_restart_preserves_hover_pause() {
        // A `replaces_id` re-post while hovered must stay paused (not fire out
        // from behind the hover) and resume from the NEW full timeout.
        let mut s = TimerState::default();
        let t0 = Instant::now();
        s.start(Some(Duration::from_secs(5)), t0);
        s.pause(t0 + Duration::from_secs(1)); // hovered
        // Re-post with a longer timeout while hovered: stays paused.
        assert_eq!(
            s.start(Some(Duration::from_secs(10)), t0 + Duration::from_secs(2)),
            TimerAction::Nothing
        );
        assert_eq!(s.deadline, None);
        assert_eq!(s.hover_count, 1);
        // Leave: arm from the new full timeout (remaining was reset to it).
        let leave = t0 + Duration::from_secs(3);
        assert_eq!(
            s.resume(leave, MIN_RESUME),
            TimerAction::Arm(Duration::from_secs(10))
        );
    }

    #[test]
    fn hold_transfer_across_a_card_rebuild_never_unpauses() {
        // The overlay's #593 card swap, as pure state transitions: the
        // successor card's hold is taken BEFORE the outgoing card is unmapped,
        // so the count goes 1 → 2 → 1 and never touches zero. Nothing re-arms
        // mid-hover and the recorded remainder is not recomputed. (The widget
        // half — that a stationary pointer gets no fresh `enter` — isn't
        // testable without a real compositor.)
        //
        // This pins the count-**1** entry only. The count-0 entry the same swap
        // used to meet — because a sticky phase threw the entry away — is
        // `sticky_then_finite_repost_under_a_parked_pointer_stays_held` below.
        let mut s = TimerState::default();
        let t0 = Instant::now();
        s.start(Some(Duration::from_secs(5)), t0);
        assert_eq!(s.pause(t0 + Duration::from_secs(1)), TimerAction::Abort); // pointer enters
        assert_eq!(s.remaining, Duration::from_secs(4));

        // Successor's hold, taken while the old card is still mounted…
        assert_eq!(s.pause(t0 + Duration::from_secs(2)), TimerAction::Nothing);
        // …then the old card unmaps and releases its own.
        assert_eq!(
            s.resume(t0 + Duration::from_secs(2), MIN_RESUME),
            TimerAction::Nothing
        );
        assert_eq!(s.hover_count, 1);
        assert_eq!(s.deadline, None); // still paused
        assert_eq!(s.remaining, Duration::from_secs(4)); // remainder untouched

        // The successor's own leave is the one — and only one — that re-arms.
        assert_eq!(
            s.resume(t0 + Duration::from_secs(9), MIN_RESUME),
            TimerAction::Arm(Duration::from_secs(4))
        );
        assert_eq!(s.hover_count, 0);
    }

    #[test]
    fn sticky_post_holds_a_hover_with_nothing_armed() {
        // A sticky notification has an entry (#619) purely so the overlay's hold
        // has somewhere to land: the pause records the count and arms nothing,
        // and — the half that would otherwise be a new bug — the leave must not
        // conjure an expiry onto a notification that never expires.
        let mut s = TimerState::default();
        let t0 = Instant::now();
        assert_eq!(s.start(None, t0), TimerAction::Abort); // sticky post
        assert_eq!(s.pause(t0), TimerAction::Nothing);
        assert_eq!(s.hover_count, 1);
        assert_eq!(s.deadline, None);
        assert_eq!(
            s.resume(t0 + Duration::from_secs(30), MIN_RESUME),
            TimerAction::Nothing
        );
        assert_eq!(s.hover_count, 0);
        assert_eq!(s.deadline, None);
    }

    #[test]
    fn sticky_then_finite_repost_under_a_parked_pointer_stays_held() {
        // The #619 repro, end to end as state transitions:
        //   notify-send -t 0 "hold me"   → sticky post
        //   park the pointer on the toast → the overlay's hold
        //   notify-send -r <id> -t -1     → same id, now finite
        // Pre-fix the sticky post left no entry at all, so the hold was recorded
        // nowhere and the re-post minted a `hover_count: 0` entry that armed a
        // 5s sleep under a stationary pointer. The count now survives the whole
        // sequence, so the re-post has a hold to see and stays paused.
        //
        // The *entry-lifetime* half of that — that a sticky phase no longer
        // throws the state away — is what
        // `sticky_repost_keeps_the_entry_and_its_hover_count` pins, against the
        // real `TimerMap` surgery. This test starts from one long-lived state and
        // so assumes it.
        let mut s = TimerState::default();
        let t0 = Instant::now();
        assert_eq!(s.start(None, t0), TimerAction::Abort); // sticky post
        assert_eq!(s.pause(t0 + Duration::from_secs(1)), TimerAction::Nothing);
        assert_eq!(s.hover_count, 1);

        // The re-post flips sticky → finite while the pointer is parked.
        assert_eq!(
            s.start(Some(Duration::from_secs(5)), t0 + Duration::from_secs(2)),
            TimerAction::Nothing // NOT Arm — that was the bug
        );
        assert_eq!(s.timeout, Some(Duration::from_secs(5)));
        assert_eq!(s.hover_count, 1);
        assert_eq!(s.deadline, None);
        assert_eq!(s.remaining, Duration::from_secs(5));

        // …and the card rebuild the re-posted content triggers (#593's swap)
        // hands the hold over without ever passing through zero.
        assert_eq!(s.pause(t0 + Duration::from_secs(2)), TimerAction::Nothing);
        assert_eq!(
            s.resume(t0 + Duration::from_secs(2), MIN_RESUME),
            TimerAction::Nothing
        );
        assert_eq!(s.hover_count, 1);
        assert_eq!(s.deadline, None);

        // Only the pointer actually leaving starts the new 5s countdown.
        let leave = t0 + Duration::from_secs(30);
        assert_eq!(
            s.resume(leave, MIN_RESUME),
            TimerAction::Arm(Duration::from_secs(5))
        );
        assert_eq!(s.deadline, Some(leave + Duration::from_secs(5)));
    }

    #[test]
    fn finite_then_sticky_repost_keeps_the_hold_and_stays_sticky() {
        // The symmetric direction: a hovered *finite* notification re-posted
        // sticky. Pre-fix this ran `clear_timer` and discarded `hover_count`
        // while the card was still mounted and still holding, so the card's
        // eventual unmap released a hold the service had forgotten.
        let mut s = TimerState::default();
        let t0 = Instant::now();
        assert_eq!(
            s.start(Some(Duration::from_secs(5)), t0),
            TimerAction::Arm(Duration::from_secs(5))
        );
        assert_eq!(s.pause(t0 + Duration::from_secs(1)), TimerAction::Abort);
        assert_eq!(s.remaining, Duration::from_secs(4));

        // Re-posted sticky: stand the countdown down, keep the hold.
        assert_eq!(
            s.start(None, t0 + Duration::from_secs(2)),
            TimerAction::Abort
        );
        assert_eq!(s.timeout, None);
        assert_eq!(s.deadline, None);
        assert_eq!(s.hover_count, 1);

        // The hold is released with nothing armed — a sticky toast waits for a
        // dismiss, it does not expire 4s after the pointer leaves.
        assert_eq!(
            s.resume(t0 + Duration::from_secs(3), MIN_RESUME),
            TimerAction::Nothing
        );
        assert_eq!(s.hover_count, 0);
        assert_eq!(s.deadline, None);

        // A later finite re-post arms normally from the fresh full timeout.
        let repost = t0 + Duration::from_secs(9);
        assert_eq!(
            s.start(Some(Duration::from_secs(5)), repost),
            TimerAction::Arm(Duration::from_secs(5))
        );
        assert_eq!(s.deadline, Some(repost + Duration::from_secs(5)));
    }

    #[test]
    fn sticky_repost_stands_down_an_armed_unhovered_countdown() {
        // No hover involved: the sleep armed by the finite post being replaced
        // must not fire later and "expire" a now-sticky notification (the case
        // the old `disarm_expiry` existed for — preserved, minus the entry
        // removal), and a stray leave afterwards still can't arm anything.
        let mut s = TimerState::default();
        let t0 = Instant::now();
        s.start(Some(Duration::from_secs(5)), t0);
        assert_eq!(
            s.start(None, t0 + Duration::from_secs(1)),
            TimerAction::Abort
        );
        assert_eq!(s.deadline, None);
        assert_eq!(
            s.resume(t0 + Duration::from_secs(2), MIN_RESUME),
            TimerAction::Nothing
        );
        assert_eq!(s.hover_count, 0);
        assert_eq!(s.deadline, None);
    }

    // ── Effect layer: the entry's lifetime in the map (#619) ─────────────────
    //
    // The transitions above all run against ONE long-lived `TimerState`, so they
    // presuppose the property #619 was actually about: that the entry survives a
    // sticky phase. These drive the real `TimerMap` surgery instead. `spawn`
    // returns `None` (no `AbortHandle` without a runtime) and records what it was
    // asked to arm, which is the observable that matters.

    /// Collect what [`apply_expiry`] would have armed, without a runtime.
    fn armed_by(
        timers: &mut TimerMap,
        id: u32,
        timeout: Option<Duration>,
        now: Instant,
    ) -> Option<Duration> {
        let armed = Cell::new(None);
        apply_expiry(timers, id, timeout, now, |dur| {
            armed.set(Some(dur));
            None
        });
        armed.get()
    }

    #[test]
    fn sticky_repost_keeps_the_entry_and_its_hover_count() {
        // THE #619 regression pin, at the layer the bug lived in. `disarm_expiry`
        // used to `timers.remove(&id)` right here, which is what left the
        // overlay's hold with nothing to land on. Re-introducing any
        // `if timeout.is_none() { clear_timer(…); return; }` short-circuit must
        // fail this test — the state-transition tests above would all still pass.
        let mut map = TimerMap::new();
        let t0 = Instant::now();
        let five = Duration::from_secs(5);

        // Finite post: entry created, full timeout armed.
        assert_eq!(armed_by(&mut map, 7, Some(five), t0), Some(five));
        assert!(map.contains_key(&7));

        // The toast mounts and the pointer parks on it — the overlay's
        // `pause_expiry`, which is this same `get_mut` + `pause`.
        let entry = map.get_mut(&7).expect("entry just created");
        assert_eq!(
            entry.state.pause(t0 + Duration::from_secs(1)),
            TimerAction::Abort
        );

        // Re-posted sticky while held: nothing armed, and BOTH the entry and the
        // hold survive.
        assert_eq!(
            armed_by(&mut map, 7, None, t0 + Duration::from_secs(2)),
            None
        );
        assert!(
            map.contains_key(&7),
            "a sticky re-post must not drop the entry (#619)"
        );
        assert_eq!(
            map[&7].state.hover_count, 1,
            "the hover hold must survive a sticky phase (#619)"
        );

        // Re-posted finite again, pointer still parked: the carried count is what
        // stops this arming a countdown nobody can see run out.
        assert_eq!(
            armed_by(&mut map, 7, Some(five), t0 + Duration::from_secs(3)),
            None,
            "must not arm while a hover is held (#619)"
        );
        assert_eq!(map[&7].state.hover_count, 1);
        assert_eq!(map[&7].state.deadline, None);

        // Only the pointer leaving arms it — with the full re-posted timeout.
        let leave = t0 + Duration::from_secs(30);
        let entry = map.get_mut(&7).expect("entry still present");
        assert_eq!(
            entry.state.resume(leave, MIN_RESUME),
            TimerAction::Arm(five)
        );
    }

    #[test]
    fn sticky_first_post_still_creates_an_entry() {
        // A sticky notification never arms anything, but it still needs the entry
        // — that is the whole reason `set_expiry` is called for `None` at all
        // (#619). Pre-fix this path was `disarm_expiry` on an empty map: a no-op
        // that left `pause_expiry` nothing to find.
        let mut map = TimerMap::new();
        assert_eq!(armed_by(&mut map, 9, None, Instant::now()), None);
        assert!(
            map.contains_key(&9),
            "a sticky post needs an entry for the overlay's hold to land on (#619)"
        );
        assert_eq!(map[&9].state.timeout, None);
        assert_eq!(map[&9].state.hover_count, 0);
    }

    #[test]
    fn finite_repost_rearms_from_the_new_timeout_when_unhovered() {
        // The ordinary re-post path through the map, for contrast with the held
        // one above: no hover, so each post arms afresh and the entry is reused
        // rather than duplicated.
        let mut map = TimerMap::new();
        let t0 = Instant::now();
        assert_eq!(
            armed_by(&mut map, 3, Some(Duration::from_secs(5)), t0),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            armed_by(&mut map, 3, Some(Duration::from_secs(10)), t0),
            Some(Duration::from_secs(10))
        );
        assert_eq!(map.len(), 1);
        assert_eq!(map[&3].state.timeout, Some(Duration::from_secs(10)));
        assert_eq!(map[&3].state.deadline, Some(t0 + Duration::from_secs(10)));
    }

    #[test]
    fn clear_timer_removes_only_the_given_id() {
        // Cleanup on dismiss: the id's bookkeeping is gone, siblings untouched,
        // no leaked entry. (`abort: None` keeps this runtime-free.)
        let mut map = TimerMap::new();
        map.insert(7, TimerEntry::default());
        // A sticky entry is a real entry now (#619) and `dismiss` is the only
        // thing that drops one.
        map.insert(8, TimerEntry::default());
        clear_timer(&mut map, 7);
        assert!(!map.contains_key(&7));
        assert!(map.contains_key(&8));
        // Clearing an unknown id is a no-op.
        clear_timer(&mut map, 999);
        assert_eq!(map.len(), 1);
    }
}
