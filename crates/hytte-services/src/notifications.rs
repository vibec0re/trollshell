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
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::task::AbortHandle;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};

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
    /// The daemon's live set: the live list, the id counter, the shell-local
    /// action callbacks and the expiry timers. This is the **one**
    /// [`NotificationsIface`] [`mount_live_set`] builds, and the bus serves a
    /// clone of this very value. Every handle in it is an `Arc` or a
    /// `Mutable`, so the D-Bus `Notify` handler, [`post_local`], [`close`] and
    /// the hover [`pause_expiry`] / [`resume_expiry`] all reach the same four
    /// (#1604). Before that, `Service::start` and an `iface()` view each
    /// assembled the handles by hand, and handing either one a fresh timer map
    /// left every test green while every expiry it armed would have been
    /// refused.
    iface: NotificationsIface,
    history: Mutable<Vec<HistoryEntry>>,
    /// Ownership handle used to emit D-Bus signals directly on the owned
    /// connection without a round-trip method call.
    ownership: OwnNameSignal,
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
/// Priority (highest first): `image-data` / `image_data` hint →
/// `image-path` / `image_path` hint → `app_icon` argument → legacy
/// `icon_data` hint — the spec's single-image order (see `parse_image`).
#[derive(Clone, Debug)]
pub enum NotificationImage {
    /// In-memory raw image from the `image-data` hint (or its deprecated
    /// spellings, `image_data` and `icon_data`).
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
    /// For a D-Bus `Notify`, mapped from `expire_timeout` and `urgency` per
    /// the freedesktop notification spec:
    /// - `<0` (e.g. `-1`) → `Some(5s)` (server default), except
    ///   [`Urgency::Critical`] → `None` (stays until dismissed)
    /// - `0`             → `None` (sticky / never expires)
    /// - `>0`            → `Some(millis)` (as requested, at any urgency)
    ///
    /// A [`post_local`] toast always carries the 5 s default, at every
    /// urgency (see [`post_local`]).
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
        let history = Mutable::new(Vec::new());

        // Own the well-known name + mount the interface. The bus layer handles
        // connection lifecycle, RequestName retries, and per-owner back-off if
        // mako/dunst is camping the name. The bus serves a clone of the one
        // live set, and `NotificationsShared` below keeps the set itself (see
        // `mount_live_set`).
        //
        // The OwnNameSignal is stored both in NotificationsHandles (process
        // lifetime keep-alive) and in the shared registry (so dismiss/invoke_action
        // can emit signals directly without a round-trip D-Bus call).
        let (iface, ownership) = mount_live_set(|served| {
            hytte_bus::own_name(hytte_bus::BusKind::Session, "org.freedesktop.Notifications")
                .at_path("/org/freedesktop/Notifications", served)
                .start()
        });

        // Populate the cross-thread shared handle so `dismiss` / `clear_history`
        // can find these Mutables when called from a hytte-tokio worker (the
        // thread-local registry is GTK-only). Calling Service::start a second
        // time would `set` fail silently — services are registered once.
        shared::insert(NotificationsShared {
            iface: iface.clone(),
            history: history.clone(),
            ownership: ownership.clone(),
        });

        NotificationsHandles {
            active: iface.active,
            _next_id: iface.next_id,
            history,
            ownership,
        }
    }
}

/// Build the daemon's one live set and hand a clone of it to `mount`, which
/// in production mounts it on the bus as the `org.freedesktop.Notifications`
/// object and returns the ownership handle. Returns the live set itself, for
/// [`NotificationsShared`], with whatever `mount` returned.
///
/// This is the only place the daemon's [`NotificationsIface`] is built, so it
/// is the only place the D-Bus `Notify` handler and the shared registry could
/// come to hold different handles (#1604). They cannot: the bus gets a clone,
/// and a clone shares every handle. `mount` is a parameter, like
/// [`apply_expiry`]'s `spawn`, so a test can keep the bus's clone instead of
/// mounting it, and drive both halves with no bus.
fn mount_live_set<M>(mount: impl FnOnce(NotificationsIface) -> M) -> (NotificationsIface, M) {
    let iface = NotificationsIface::new();
    let mounted = mount(iface.clone());
    (iface, mounted)
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
/// emits `NotificationClosed` on the session bus — **only if `id` is live**.
/// For any other id it is a no-op: no history entry, and above all no signal.
/// The spec invalidates an id "before the signal is sent", so a second
/// `NotificationClosed` for one id, or one for an id never issued, is a close
/// event for a notification that does not exist — which a client that
/// filters the broadcast by id (libnotify does) may take for one of its own.
/// Pre-fix both happened: `CloseNotification 999` emitted
/// `NotificationClosed(999, 3)`, and a click landing as the expiry timer fired
/// emitted the close twice, once per reason.
///
/// Every close path funnels through here — auto-expiry, the user, and the
/// D-Bus `CloseNotification` — so this one gate covers all of them.
///
/// Safe to call from any thread (GTK widget handlers, hytte-tokio workers,
/// the auto-expire timer in `notify()`). The local mutation runs
/// synchronously on the calling thread; the bus signal emit is spawned onto
/// the runtime.
pub fn dismiss(id: u32, reason: u32) {
    close(id, reason, None);
}

/// [`dismiss`], plus the one thing an expiry has to prove first: `expiry` is
/// the [`TimerEntry::arming`] the firing sleep was armed with, and the close
/// goes ahead only while that is still the entry's current arming (see
/// [`closable`]). `None` — the user, `CloseNotification` — closes
/// unconditionally.
fn close(id: u32, reason: u32, expiry: Option<u64>) {
    let Some(shared) = shared::get::<NotificationsShared>() else {
        return;
    };

    // The live-set half: the arming check, the removal and the timer entry's
    // reclaim, as one step under the `active` guard (see `remove_closable`).
    let Some(n) = shared.iface.remove_closable(id, expiry) else {
        tracing::debug!(
            id,
            reason,
            stale_expiry = expiry.is_some(),
            "close for an id that is not live, or by a superseded expiry; nothing to do"
        );
        return;
    };

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
    {
        let mut hist = shared.history.lock_mut();
        hist.insert(0, entry);
        hist.truncate(100);
    }

    // Drop any local-dispatch callback still registered for `id` — this
    // close path (expiry, user dismiss, or `close_notification`'s call
    // into `dismiss`) means the action can never be invoked again. A
    // cheap no-op for externally-posted notifications, which never
    // populate this map.
    if let Ok(mut map) = shared.iface.local_actions.lock() {
        clear_local_actions(&mut map, id);
    }

    // Emit the NotificationClosed signal directly on the owned connection.
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

/// Where `id` sits in the live list, if it is there to close; `None` makes
/// the close a no-op. The one gate on every side effect of [`dismiss`], the
/// `NotificationClosed` emit included — factored out so the gate itself is
/// unit-testable without the process-global shared registry or a bus.
///
/// An expiry (`expiry: Some(arming)`) must also still be `id`'s *current*
/// arming. `AbortHandle::abort` cannot stop a sleep task that is already past
/// its `sleep` and blocked on the `active` lock inside `close`: there is no
/// `.await` left in it for the abort to land on. Before this check, such a
/// task — superseded by a `replaces_id` re-post that held the lock, or by a
/// hover that paused it — went on to close whatever was live under the id
/// once the lock came free: the fresh replacement, with reason 1, right
/// after it appeared. Every re-post and every pause bumps the arming
/// ([`TimerEntry::disarm`]), so the superseded task's no longer matches; a
/// missing entry means the notification already closed.
fn closable(
    live: &[Notification],
    timers: &TimerMap,
    id: u32,
    expiry: Option<u64>,
) -> Option<usize> {
    if let Some(arming) = expiry
        && timers.get(&id).is_none_or(|entry| entry.arming != arming)
    {
        return None;
    }
    live.iter().position(|n| n.id == id)
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
    let ids: Vec<u32> = shared
        .iface
        .active
        .lock_ref()
        .iter()
        .map(|n| n.id)
        .collect();
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
        && let Ok(mut map) = shared.iface.local_actions.lock()
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
/// callbacks live in `NotificationsIface::local_actions`, held in the
/// process-global shared registry and reached from hytte-tokio worker threads
/// (the auto-expire timer, the D-Bus `close_notification` method), both of
/// which only ever *drop* an unclicked callback via `clear_local_actions` —
/// never call it. `Send` makes that cross-thread drop legal; it does not
/// promise (and nothing requires) that the closure itself is safe to *run*
/// off the GTK thread. A useful corollary: since GTK/GDK objects are not
/// `Send`, a `Send` closure structurally cannot hold one across the boundary
/// — it can only capture plain owned data (e.g. a screenshot's file path
/// `String`) and resolve the GTK/GDK/GIO handle fresh when it actually runs.
pub type LocalActionCallback = Box<dyn FnOnce() + Send + 'static>;

/// Map backing [`NotificationsIface::local_actions`]; see that field's doc.
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
/// inherent window where a sleep has fired but its close — which drops the
/// whole entry — hasn't run yet; harmless, because that close presents its
/// [`TimerEntry::arming`] and is refused if anything re-armed or paused the
/// entry in the meantime).
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
    /// Which arming of this entry the live sleep belongs to. Bumped by every
    /// [`disarm`](Self::disarm) — so by every re-post, every pausing hover and
    /// every re-arm — and handed to the task [`arm`](Self::arm) spawns, which
    /// presents it when it fires; [`closable`] refuses an expiry whose arming
    /// is no longer this one. `abort` alone cannot be the record: a task past
    /// its `sleep` has no `.await` left for an abort to land on. Wraps rather
    /// than overflows; 2⁶⁴ re-arms of one toast are not a concern.
    arming: u64,
}

impl TimerEntry {
    /// Cancel the armed sleep, if any, and supersede it — including a sleep
    /// that has already fired and whose close is still waiting on the
    /// `active` lock, which the abort cannot reach.
    fn disarm(&mut self) {
        if let Some(handle) = self.abort.take() {
            handle.abort();
        }
        self.arming = self.arming.wrapping_add(1);
    }

    /// Supersede whatever was armed and arm a fresh sleep through `spawn`,
    /// which receives the arming the task must present when it fires.
    fn arm(&mut self, spawn: impl FnOnce(u64) -> Option<AbortHandle>) {
        self.disarm();
        self.abort = spawn(self.arming);
    }
}

/// Per-id expiry timers backing [`NotificationsIface::timers`].
type TimerMap = HashMap<u32, TimerEntry>;

/// Spawn the `sleep(dur) → close(id, 1 = expired)` task for `arming` and
/// return its abort handle. Runs on the shared runtime so it's callable from
/// any thread. The close is refused if `arming` has been superseded by the
/// time it gets the `active` lock (see [`closable`]).
fn spawn_expiry(id: u32, dur: Duration, arming: u64) -> AbortHandle {
    runtime::handle()
        .spawn(async move {
            tokio::time::sleep(dur).await;
            close(id, 1, Some(arming)); // 1 = expired
        })
        .abort_handle()
}

/// Record `id`'s resolved timeout — `Some` finite, `None` sticky — creating the
/// bookkeeping entry on first post and re-arming (or standing down) the sleep on
/// every `replaces_id` re-post. The one entry point for "what is this
/// notification's timeout now"; [`close`] (behind [`dismiss`]) is the only
/// thing that removes an entry.
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
/// `timers` is the caller's `NotificationsIface::timers`: in production the
/// one live set's map, which [`close`] and the hover pause/resume read too
/// (see [`mount_live_set`]). It is passed in rather than looked up so a test
/// can drive the real `Notify` handler and then read the entry it armed
/// (#1542). Callable from any thread.
fn set_expiry(timers: &Mutex<TimerMap>, id: u32, timeout: Option<Duration>) {
    if let Ok(mut timers) = timers.lock() {
        apply_expiry(&mut timers, id, timeout, Instant::now(), |dur, arming| {
            Some(spawn_expiry(id, dur, arming))
        });
    }
}

/// Map-level half of [`set_expiry`]: create-or-update `id`'s entry for a
/// (re-)post that resolved to `timeout`, cancel and supersede whatever that
/// entry had armed, and — when the transition calls for a fresh countdown —
/// install the handle `spawn` returns for it. `spawn` gets the duration and
/// the [`TimerEntry::arming`] the task must present.
///
/// **Never removes an entry.** [`clear_timer`], from [`close`], is the only
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
    spawn: impl FnOnce(Duration, u64) -> Option<AbortHandle>,
) {
    let entry = timers.entry(id).or_default();
    // Cancel and supersede any task already armed for this id (a `replaces_id`
    // re-post) so a stale sleep can't dismiss the refreshed notification early
    // — or "expire" one that just turned sticky — even if it has already fired
    // and is waiting on the `active` guard this re-post holds. Unconditional
    // rather than driven off the returned action: it must cover `Nothing` too
    // (a re-post arriving while a hover is held), and disarming an
    // already-cleared entry is free.
    entry.disarm();
    if let TimerAction::Arm(dur) = entry.state.start(timeout, now) {
        entry.arm(|arming| spawn(dur, arming));
    }
}

/// Remove and cancel the expiry timer for `id`. Called from [`close`] — the
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
    shared.iface.pause_expiry(id);
}

/// Map-level half of [`pause_expiry`], factored out (like [`apply_expiry`])
/// so a test drives the real surgery with no runtime.
fn apply_pause(timers: &mut TimerMap, id: u32, now: Instant) {
    if let Some(entry) = timers.get_mut(&id)
        && entry.state.pause(now) == TimerAction::Abort
    {
        // Superseding, not just aborting: a sleep that fired a moment before
        // the pointer arrived must not close the toast now under it.
        entry.disarm();
    }
}

/// Resume notification `id`'s auto-expiry when the pointer leaves its toast
/// (#567). Only the last hover leaving re-arms the countdown — with the
/// recorded remainder, floored at `MIN_RESUME` so the toast doesn't vanish
/// the instant the pointer leaves. A sticky notification releases its hold with
/// nothing armed (#619). See [`pause_expiry`] for the threading.
pub fn resume_expiry(id: u32) {
    let Some(shared) = shared::get::<NotificationsShared>() else {
        return;
    };
    shared.iface.resume_expiry(id);
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
/// Here `Critical` only bypasses those gates: a self-toast still expires after
/// the 5 s server default at every urgency. That differs from a D-Bus client's
/// `Critical` notification with `expire_timeout = -1`, which the `Notify`
/// handler keeps on screen until dismissed (#1542). The shell's own error
/// toasts were not part of that decision, so they keep expiring.
///
/// # Rate-limiting
///
/// Identical `(app_name, summary, body)` toasts within
/// `POST_LOCAL_RATE_LIMIT` are dropped so a flapping daemon can't spam the
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
/// dismiss, …) — see `NotificationsIface::local_actions`.
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

    shared
        .iface
        .post_local(app_name, summary, body, urgency, actions);
}

// ── D-Bus interface ───────────────────────────────────────────────────────────

/// The daemon's live set, and the mounted `org.freedesktop.Notifications`
/// object. It holds four handles: the live list (`active`), the id counter
/// (`next_id`), the shell-local action callbacks (`local_actions`) and the
/// per-id expiry timers (`timers`). Each is an `Arc` or a `Mutable`, so a
/// clone shares all four. [`mount_live_set`] builds one value: the bus serves
/// a clone of it and [`NotificationsShared`] holds the value itself, so the
/// `Notify` handler and everything that reaches the daemon through the shared
/// registry work on the same four handles (#1604).
///
/// Besides the D-Bus methods, it carries the shell's own post
/// ([`post_local`](Self::post_local)) and the live-set half of [`close`],
/// [`pause_expiry`] and [`resume_expiry`], in a plain `impl` block the
/// interface macro does not export.
///
/// The history is not here: closing goes through [`dismiss`] and the shared
/// registry, like every other close path, and that is where the history is
/// written. (The interface used to carry the history too, for a
/// `close_notification` that pulled the notification out itself before
/// calling `dismiss` — which is what left `dismiss` emitting
/// `NotificationClosed` for ids it had not removed, since from there it could
/// not tell a close it was finishing from one for an id that was never live.)
#[derive(Clone)]
struct NotificationsIface {
    /// The live notifications, in posting order: what the toast overlay
    /// renders, through [`active()`].
    active: Mutable<Vec<Notification>>,
    /// The id counter `Notify` and [`post_local`](Self::post_local) both
    /// allocate from: one monotonic sequence, so a locally-posted toast can
    /// never collide with a `Notify`-allocated id. `post_local` reaches it
    /// through the shared registry, since it runs on hytte-tokio workers too
    /// (the thread-local registry is GTK-thread-only).
    next_id: Arc<AtomicU32>,
    /// Local-dispatch callbacks for locally-posted notification actions,
    /// keyed by `(notification id, action key)`. Populated by
    /// [`post_local_with_actions`], consumed (removed + run) by
    /// [`invoke_action`] on a matching `(id, key)`, and swept for a whole
    /// `id` by [`dismiss`] whenever that notification closes — by any path
    /// (auto-expiry, user dismiss, or `CloseNotification`) — so a
    /// never-clicked action doesn't linger forever. Externally-posted
    /// (D-Bus `Notify`) notifications never populate this map; a `Notify`
    /// that replaces a live id only clears that id's callbacks from it (see
    /// [`forget_replaced_local_actions`]). A plain `std::sync::Mutex` is fine
    /// here for the same reason as `POST_LOCAL_SEEN`: every critical section
    /// is non-async map surgery.
    local_actions: Arc<Mutex<LocalActionMap>>,
    /// Hover-pausable per-id expiry timers (#567). Keyed by notification id,
    /// each entry pairs the pure [`TimerState`] bookkeeping with the live
    /// `sleep → dismiss` task's abort handle. One entry per **live**
    /// notification — sticky ones included, carrying `timeout: None` and no
    /// abort handle, because the hover count has to survive a sticky phase
    /// (#619). `Notify` and [`post_local`](Self::post_local) arm through it,
    /// and [`close`] and the hover [`pause_expiry`] / [`resume_expiry`] read
    /// it: one map, because there is one live set (#1542, #1604). Reached
    /// from BOTH the GTK main thread (the overlay's motion controller) and
    /// hytte-tokio (the `Notify` handler, the sleep task, and [`dismiss`]'s
    /// cleanup) — a plain `std::sync::Mutex` suffices for the same reason as
    /// `local_actions`: every critical section is non-async map surgery.
    timers: Arc<Mutex<TimerMap>>,
}

impl NotificationsIface {
    /// A live set over fresh, empty handles: no notifications, ids from 1, no
    /// callbacks, no timers. [`mount_live_set`] calls it once for the daemon;
    /// every other `NotificationsIface` there is a clone of that one.
    fn new() -> Self {
        Self {
            active: Mutable::new(Vec::new()),
            next_id: Arc::new(AtomicU32::new(1)),
            local_actions: Arc::new(Mutex::new(LocalActionMap::new())),
            timers: Arc::new(Mutex::new(TimerMap::new())),
        }
    }

    /// The body of [`post_local_with_actions`], past its registry lookup and
    /// rate limit: post the shell's own toast into this live set and return
    /// its id. A method here rather than inline there so a test can drive it
    /// against its own handles and read the timer it armed, as the `Notify`
    /// tests do. Not part of the D-Bus interface: this `impl` block carries no
    /// `#[zbus::interface]`.
    fn post_local(
        &self,
        app_name: &str,
        summary: &str,
        body: &str,
        urgency: Urgency,
        actions: Vec<LocalAction>,
    ) -> u32 {
        // Allocate from the SAME atomic the D-Bus `Notify` handler uses — one
        // monotonic sequence, so this id can never collide with a
        // `Notify`-issued one (see `NotificationsIface::next_id`).
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        // Register each action's callback under (id, key) and build the plain
        // Action list the Notification carries for rendering. If the mutex is
        // poisoned, fail safe by posting the toast with no action buttons
        // rather than buttons that would dispatch to nothing.
        let mut ui_actions = Vec::with_capacity(actions.len());
        if let Ok(mut map) = self.local_actions.lock() {
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

        // The shell's own toasts expire after the default at every urgency,
        // `Critical` included: here `Critical` is the DND/mute bypass, not a
        // client leaving the timeout to us (see `post_local`). One binding, so
        // the field and the timer cannot disagree.
        let timeout = Some(DEFAULT_TIMEOUT);

        let notification = Notification {
            id,
            app_name: app_name.to_string(),
            app_icon: String::new(),
            summary: summary.to_string(),
            body: strip_markup(body),
            urgency,
            timeout,
            actions: ui_actions,
            image: None,
            created_at,
        };

        // Arm the hover-pausable auto-dismiss timer, then publish — both under
        // the one `active` write guard, expiry first. See `notify` for why the
        // order and the shared guard are both load-bearing (#619).
        {
            let mut list = self.active.lock_mut();
            set_expiry(&self.timers, id, timeout);
            list.push(notification);
        }
        tracing::debug!(id, app_name, summary, "local notification posted");
        id
    }

    /// [`close`]'s step on this live set: remove `id` from the live list and
    /// reclaim its timer entry, if [`closable`] lets the close go ahead, and
    /// hand the removed notification back. `None` makes the close a no-op.
    fn remove_closable(&self, id: u32, expiry: Option<u64>) -> Option<Notification> {
        // Local state mutation. Synchronous — Mutable is thread-safe. The
        // position is read through `Deref` and only a real removal goes through
        // `DerefMut`, so a no-op close does not wake the toast consumer.
        //
        // `timers` is taken *inside* the `active` guard — the `active` → `timers`
        // order `notify` / `post_local` already use for `set_expiry` — so the
        // arming check and the removal are one step against every re-post: a
        // re-post either ran entirely before this (and its re-arm makes a stale
        // expiry's arming not current) or runs entirely after (and finds the id
        // closed, so allocates fresh). The entry is reclaimed in the same step —
        // no leaked entry or orphaned sleep task (#567) — and only on a real
        // close: an entry exists exactly while its notification is live
        // (`set_expiry` creates it under the guard that publishes the
        // notification) and a non-live id is never re-created (`notify_id`), so a
        // no-op close has nothing to reclaim. A poisoned `timers` lock is still
        // read: every critical section on it is plain map surgery, and refusing
        // here would leave every toast on screen forever.
        let mut list = self.active.lock_mut();
        let mut timers = self.timers.lock().unwrap_or_else(PoisonError::into_inner);
        closable(&list, &timers, id, expiry).map(|at| {
            clear_timer(&mut timers, id);
            list.remove(at)
        })
    }

    /// [`pause_expiry`]'s work on this live set's timers.
    fn pause_expiry(&self, id: u32) {
        if let Ok(mut timers) = self.timers.lock() {
            apply_pause(&mut timers, id, Instant::now());
        }
    }

    /// [`resume_expiry`]'s work on this live set's timers.
    fn resume_expiry(&self, id: u32) {
        if let Ok(mut timers) = self.timers.lock()
            && let Some(entry) = timers.get_mut(&id)
            && let TimerAction::Arm(dur) = entry.state.resume(Instant::now(), MIN_RESUME)
        {
            // Invariant: `abort` is None here (the matching pause cleared it).
            // `arm` supersedes defensively regardless, so a logic drift can never
            // strand a live task behind a freshly-armed one.
            entry.arm(|arming| Some(spawn_expiry(id, dur, arming)));
        }
    }
}

// zbus's `#[interface]` macro requires every method to be `async fn` even
// when the body doesn't await, and to take `&self` even when it reaches no
// state through it (`close_notification` calls the free `dismiss`; the
// capability/identity handlers return constants). Allowing at the impl-block
// keeps the noise out of each method — `screensaver.rs`'s and `control.rs`'s
// shape.
#[allow(clippy::unused_async, clippy::unused_self)]
#[zbus::interface(name = "org.freedesktop.Notifications")]
impl NotificationsIface {
    /// Show a notification. Returns the notification id.
    ///
    /// `expire_timeout` (freedesktop notification spec; see `resolve_timeout`):
    ///   - `<0` (e.g. `-1`) → use server default: 5s, or sticky for a
    ///     `Critical` urgency
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
        // Parse urgency from hints (key "urgency"; see `parse_urgency`).
        let urgency = parse_urgency(&hints);

        // Resolve timeout (see `resolve_timeout`). It needs the urgency: a
        // `Critical` notification that leaves the timeout to us never expires.
        let timeout = resolve_timeout(urgency, expire_timeout);

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

        // Decide the id, record the resolved timeout on the hover-pausable
        // expiry bookkeeping, then update the active list (replace in-place if
        // the id is live, else push) — all under the one `active` write guard,
        // in that order.
        //
        // The id is decided *inside* the guard because it depends on what is
        // live (see `notify_id`): decided outside, the notification `replaces_id`
        // names could expire between the check and the write, and the re-post
        // would land as a new notification under an id that is no longer live.
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
        // `close` uses for its arming check, so the pair cannot invert.
        let id = {
            let mut list = self.active.lock_mut();
            // A replace of a live id keeps the id, so it must also drop any
            // shell-local callback registered under it. Lock order is
            // `active` -> `local_actions`; nothing takes them the other way.
            if let Ok(mut map) = self.local_actions.lock() {
                forget_replaced_local_actions(&mut map, &list, replaces_id);
            }
            let id = notify_id(&list, replaces_id, &self.next_id);
            set_expiry(&self.timers, id, timeout);
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
            if let Some(existing) = list.iter_mut().find(|n| n.id == id) {
                *existing = notification;
            } else {
                list.push(notification);
            }
            id
        };

        tracing::debug!(id, replaces_id, app_name, summary, "notification added");

        id
    }

    /// Close a notification by id (reason 3 = closed by call).
    ///
    /// All of it is [`dismiss`]'s, which emits `NotificationClosed` only for an
    /// id that was live. For any other id this replies success and emits
    /// nothing — mako's behaviour. The spec asks for "an empty D-Bus Error
    /// message" there instead; that is a client-visible change (libnotify
    /// surfaces it as a `GError` from `notify_notification_close`) left for a
    /// separate decision.
    async fn close_notification(&self, id: u32) {
        crate::notifications::dismiss(id, 3);
    }

    /// Return the capabilities this server implements.
    fn get_capabilities(&self) -> Vec<String> {
        vec![
            "body".to_string(),
            "icon-static".to_string(),
            "actions".to_string(),
        ]
    }

    /// Return server identification tuple.
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

// ── Helper: resolve id ────────────────────────────────────────────────────────

/// The id a `Notify` call's notification takes: `replaces_id` when it names a
/// notification that is **live** in `live`, a fresh one from `next_id`
/// otherwise — `0` ("replace nothing") included.
///
/// The spec says "if `replaces_id` is not 0, the returned value is the same
/// value as `replaces_id`", but that sentence presupposes something to replace.
/// Honouring it for an id that is not live is how two notifications came to
/// share one: `Notify(replaces_id = 5000)` returned 5000 for something this
/// daemon never issued and left the counter where it was, so when the counter
/// later reached 5000 a different app's notification replaced the first in
/// place — inheriting its timer entry — and each sender read the other's
/// `NotificationClosed` as its own. A `replaces_id` naming a notification that
/// already *closed* (the common case: a progress update arriving just after
/// the toast expired) is the same bug in miniature, minus the counter.
///
/// mako makes the same call ("either we had no `replaces_id`, or the id given
/// was invalid. Either way, make a new notification"), and libnotify adopts
/// whatever id `Notify` returns, so a client's later update finds the fresh
/// one. Must be called under the `active` write guard the result is written
/// back under: the answer is only as current as `live`.
fn notify_id(live: &[Notification], replaces_id: u32, next_id: &AtomicU32) -> u32 {
    if replaces_live(live, replaces_id) {
        replaces_id
    } else {
        next_id.fetch_add(1, Ordering::Relaxed)
    }
}

/// Whether `replaces_id` names a notification that is live in `live`, i.e. a
/// `Notify` carrying it lands as a replace and keeps the id.
fn replaces_live(live: &[Notification], replaces_id: u32) -> bool {
    replaces_id != 0 && live.iter().any(|n| n.id == replaces_id)
}

/// A replace lands on a live id: forget any shell-local callbacks registered
/// for it. A D-Bus client can name one of the shell's own `post_local` toasts
/// in `replaces_id`; the replacement's actions are the client's, so a click on
/// a key the shell also registered (`open`, `copy`) must broadcast
/// `ActionInvoked`, not run the shell's callback. Close clears these too (see
/// `dismiss`); a replace never closes. Pure map mutation, so it is
/// unit-testable without the process-global registry.
fn forget_replaced_local_actions(
    map: &mut LocalActionMap,
    live: &[Notification],
    replaces_id: u32,
) {
    if replaces_live(live, replaces_id) {
        clear_local_actions(map, replaces_id);
    }
}

// ── Helper: resolve timeout ───────────────────────────────────────────────────

/// The server default timeout. On D-Bus `Notify`, it applies when a client
/// leaves the timeout to us (`expire_timeout < 0`, conventionally `-1`) for a
/// `Low` or `Normal` urgency notification; a `Critical` one that does so gets
/// no timeout at all (see [`resolve_timeout`]). The shell's own toasts
/// ([`post_local`]) carry it at every urgency, `Critical` included.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolve a D-Bus `Notify` call's `expire_timeout` and urgency into a finite
/// auto-dismiss duration (`Some`) or a sticky notification (`None`). This is
/// the whole expiry rule **for `Notify`**: the handler stores the result on
/// [`Notification::timeout`] and hands the same value to [`set_expiry`],
/// which arms (or stands down) the auto-dismiss sleep. The shell's own toasts
/// do not come through here; [`post_local`] gives them [`DEFAULT_TIMEOUT`] at
/// every urgency.
///
/// Per the [notification spec][spec]:
/// - `< 0` (conventionally `-1`) → server decides. The spec lets that "vary
///   for the type of notification", and says critical notifications "should
///   not automatically expire" ([urgency levels][urgency]). So
///   [`Urgency::Critical`] → `None`: it stays until the user dismisses it,
///   or the client closes or replaces it. Every other urgency →
///   [`DEFAULT_TIMEOUT`].
/// - `0` → never expire (sticky), at any urgency.
/// - `> 0` → that many milliseconds, at any urgency. A client that names a
///   timeout for a critical notification gets the timeout it named.
///
/// Note the `0` and `-1` cases are easy to invert — `-1` ("you decide") is by
/// far the most common value (it's `notify-send`'s default), so mapping it to
/// sticky makes ordinary toasts pile up forever (see issue #15). Only the
/// critical arm of `-1` is sticky (#1542).
///
/// [spec]: https://specifications.freedesktop.org/notification-spec/latest/protocol.html
/// [urgency]: https://specifications.freedesktop.org/notification-spec/latest/urgency-levels.html
fn resolve_timeout(urgency: Urgency, expire_timeout: i32) -> Option<Duration> {
    match expire_timeout.cmp(&0) {
        std::cmp::Ordering::Less if urgency == Urgency::Critical => None,
        std::cmp::Ordering::Less => Some(DEFAULT_TIMEOUT),
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => {
            // expire_timeout > 0 here so the cast to u64 is safe.
            #[allow(clippy::cast_sign_loss)]
            Some(Duration::from_millis(expire_timeout as u64))
        }
    }
}

// ── Helper: parse urgency ─────────────────────────────────────────────────────

/// Read the `urgency` hint of a `Notify` call: `0` → [`Urgency::Low`], `2` →
/// [`Urgency::Critical`], and anything else (no hint, `1`, an out-of-range
/// number, a non-integer) → [`Urgency::Normal`].
///
/// The spec types the hint as a BYTE, but clients send other integer widths:
/// older Chromium builds sent a `uint32` (current Chromium sends the byte),
/// and an unannotated number in `GVariant` text
/// (`gdbus call … "{'urgency': <2>}"`) or a plain `int` from dbus-python is an
/// `int32`. mako reads `y`, `u` and `i` for that reason. Read as a byte only,
/// those came in as `Normal`: no DND bypass and, since #1542, no "stays until
/// dismissed" either. So every integer width is read by its value.
///
/// Out-of-range numbers are not clamped: a `3` or a `u32::MAX` stays `Normal`,
/// as a byte `3` always has. Clamping would turn a malformed hint into
/// `Critical`, which bypasses DND and mute and never expires. (mako does not
/// clamp either; it keeps the raw number, which then matches no urgency.)
fn parse_urgency(hints: &HashMap<String, OwnedValue>) -> Urgency {
    let level = hints.get("urgency").and_then(|v| match &**v {
        Value::U8(n) => Some(i64::from(*n)),
        Value::I16(n) => Some(i64::from(*n)),
        Value::U16(n) => Some(i64::from(*n)),
        Value::I32(n) => Some(i64::from(*n)),
        Value::U32(n) => Some(i64::from(*n)),
        Value::I64(n) => Some(*n),
        Value::U64(n) => i64::try_from(*n).ok(),
        _ => None,
    });
    match level {
        Some(0) => Urgency::Low,
        Some(2) => Urgency::Critical,
        _ => Urgency::Normal,
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
/// The spec's single-image order ("Icons and Images"), with each hint's
/// deprecated spec-1.1 spelling straight after its own:
///
/// 1. `image-data`, then `image_data` (1.1);
/// 2. `image-path`, then `image_path` (1.1);
/// 3. the `app_icon` argument;
/// 4. `icon_data` (pre-1.1), last — the spec keeps it only "for compatibility",
///    *after* `app_icon`.
///
/// The toast shows one picture (the overlay falls back to `app_icon` only
/// when this is `None`), so the single-image order is the one that applies.
/// Before this, `image_data` was not read at all — a 1.1 client's image was
/// silently dropped — and `icon_data` outranked both `image-path` and
/// `app_icon`. A hint that is present but unusable (a malformed raw image, an
/// empty or non-string path) falls through to the next one. Spec 1.2 lets
/// `image-path` name a themed icon as well as a file, so it takes the same
/// path-or-icon-name rule as `app_icon` ([`app_icon_image`]).
fn parse_image(hints: &HashMap<String, OwnedValue>, app_icon: &str) -> Option<NotificationImage> {
    let raw = |key: &str| hints.get(key).and_then(decode_image_data);
    let path = |key: &str| {
        hints
            .get(key)
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            .and_then(|p| app_icon_image(&p))
    };
    raw("image-data")
        .or_else(|| raw("image_data"))
        .or_else(|| path("image-path"))
        .or_else(|| path("image_path"))
        .or_else(|| app_icon_image(app_icon))
        .or_else(|| raw("icon_data"))
}

/// The `app_icon` argument as an image: a path or `file://` URL, else a
/// themed icon name; `None` when empty.
fn app_icon_image(app_icon: &str) -> Option<NotificationImage> {
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

/// Decode an `image-data` / `image_data` / `icon_data` `OwnedValue` of type
/// `(iiibiiay)`.
///
/// Returns `None` for a malformed payload (bad dimensions/stride or a buffer too
/// short for the declared geometry) so `parse_image` falls through to the next
/// hint in its order rather than the shell panicking when the widget builds
/// the texture.
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
            "rejecting malformed notification image data; falling back to the next image hint"
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

/// Turn a notification body into the plain text the shell renders: strip the
/// tags of the spec's markup subset, decode XML character references, and
/// keep everything else — a `<` or `&` that starts neither — literally.
///
/// # Why strip at all
///
/// [`GetCapabilities`](NotificationsIface::get_capabilities) does **not**
/// advertise `body-markup`, so by the letter of the spec the body is plain
/// text and the client should have stripped its own markup. Real clients send
/// it anyway — mako and dunst both advertise `body-markup`, so apps written
/// against them assume it — and the spec designs its tags "in a way that
/// allows them to be stripped out". So tags go; their text stays.
///
/// A `<` is a tag only when it is followed by an optional `/`, an ASCII
/// letter, and then a `>` before any further `<` — the shape every tag in the
/// spec's subset (`b`, `i`, `u`, `a`, `img`) and every tag a client actually
/// sends has. Anything else is text. The pre-fix stripper entered "in a tag"
/// on *any* `<` and left only on a `>`, so `notify-send t "errors < 5 in
/// foo.rs"` rendered `errors ` and silently dropped the rest of the body.
///
/// # Why decode entities too
///
/// A client that sends markup escapes its text for markup — that is what
/// makes it markup — so a body of `<b>Tom &amp; Jerry</b>` *means* "Tom &
/// Jerry". Stripping its tags but not its entities would render `Tom &amp;
/// Jerry`: half a decode, strictly worse for exactly the clients this
/// stripper exists for. The five predefined XML entities are decoded, and so
/// are numeric character references, because `g_markup_escape_text` —
/// glib's escaper, the one most C/Python clients here reach for — writes an
/// apostrophe as `&#39;`, so "it's" arrives as `it&#39;s`. The cost is a
/// plain-text body that *literally* contains `&amp;` rendering as `&`, which
/// is the rarer of the two by far. A bare `&` (`Fish & Chips`), an unknown
/// name (`&nbsp;`), and a reference to no valid character (`&#0;`,
/// `&#xD800;`) all stay exactly as sent.
///
/// One pass, so a decoded `&lt;b&gt;` becomes the text `<b>` and is never
/// re-read as a tag. Linear in the input: a tag scan stops at the next `<`,
/// so no two scans overlap, and an entity scan is bounded by
/// [`MAX_ENTITY_NAME`].
fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find(['<', '&']) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(len) = tag_len(rest) {
            rest = &rest[len..];
        } else if let Some((ch, len)) = char_reference(rest) {
            out.push(ch);
            rest = &rest[len..];
        } else {
            // A `<` or `&` that starts neither — text. Both are one byte.
            out.push_str(&rest[..1]);
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Byte length of the tag `s` starts with, or `None` if `s` does not start
/// with one — see [`strip_markup`] for the shape that counts.
fn tag_len(s: &str) -> Option<usize> {
    let inner = s.strip_prefix('<')?;
    let name = inner.strip_prefix('/').unwrap_or(inner);
    if !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let end = inner.find(['<', '>'])?;
    (inner.as_bytes()[end] == b'>').then_some(1 + end + 1)
}

/// Longest name between `&` and `;` that [`char_reference`] will look at —
/// `#x10FFFF`, the longest valid reference, is 8. Bounds the scan, so a body
/// of a million `&`s with no `;` costs a million short looks rather than a
/// quadratic walk on the D-Bus handler.
const MAX_ENTITY_NAME: usize = 10;

/// The character the reference `s` starts with stands for, and the
/// reference's byte length; `None` if `s` does not start with one we decode
/// (see [`strip_markup`]).
fn char_reference(s: &str) -> Option<(char, usize)> {
    let body = s.strip_prefix('&')?;
    let semi = body
        .bytes()
        .take(MAX_ENTITY_NAME + 1)
        .position(|b| b == b';')?;
    let name = &body[..semi];
    let ch = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => {
            let number = name.strip_prefix('#')?;
            let (digits, radix) = match number.strip_prefix(['x', 'X']) {
                Some(hex) => (hex, 16),
                None => (number, 10),
            };
            // `from_str_radix` alone would take a leading `+`.
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return None;
            }
            let code = u32::from_str_radix(digits, radix).ok()?;
            char::from_u32(code).filter(|&c| c != '\0')?
        }
    };
    Some((ch, 1 + semi + 1))
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_TIMEOUT, HashMap, LocalActionMap, MIN_RESUME, Mutable, Notification,
        NotificationImage, NotificationsIface, OwnedValue, RateLimitMap, TimerAction, TimerEntry,
        TimerMap, TimerState, Urgency, apply_expiry, apply_pause, clear_local_actions, clear_timer,
        closable, forget_replaced_local_actions, image_data_consistent, mount_live_set, notify_id,
        parse_image, parse_urgency, rate_limit_check, register_local_action, resolve_timeout,
        strip_markup, take_local_action,
    };
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use zbus::zvariant::{Str, Structure, Value};

    // ── Image hint precedence ────────────────────────────────────────────────

    /// An `(iiibiiay)` raw-image hint: `side`×`side` RGBA, tightly packed. The
    /// side length is how a test tells which hint won; `side = 0` is a
    /// malformed payload `decode_image_data` rejects.
    fn raw_hint(side: i32) -> OwnedValue {
        let len = usize::try_from(side * side * 4).expect("non-negative side");
        let tuple = (side, side, side * 4, true, 8_i32, 4_i32, vec![0_u8; len]);
        OwnedValue::try_from(Value::from(Structure::from(tuple))).expect("no fds in an image")
    }

    fn str_hint(s: &str) -> OwnedValue {
        OwnedValue::from(Str::from(s))
    }

    fn hints(pairs: &[(&str, OwnedValue)]) -> HashMap<String, OwnedValue> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.try_clone().expect("no fds")))
            .collect()
    }

    /// Which hint `parse_image` picked, as something `assert_eq!` can print.
    fn picked(hints: &HashMap<String, OwnedValue>, app_icon: &str) -> String {
        match parse_image(hints, app_icon) {
            None => "none".to_string(),
            Some(NotificationImage::Raw { width, .. }) => format!("raw {width}"),
            Some(NotificationImage::Path(p)) => format!("path {p}"),
            Some(NotificationImage::IconName(n)) => format!("icon {n}"),
        }
    }

    #[test]
    fn image_data_spec_1_1_spelling_is_read() {
        // The reported gap: a spec-1.1 client's `image_data` was ignored and
        // the toast showed its app icon (or nothing) instead.
        let h = hints(&[("image_data", raw_hint(2))]);
        assert_eq!(picked(&h, ""), "raw 2");
        assert_eq!(picked(&h, "firefox"), "raw 2");
        let h = hints(&[("image_path", str_hint("/tmp/b.png"))]);
        assert_eq!(picked(&h, "firefox"), "path /tmp/b.png");
    }

    #[test]
    fn image_path_hint_may_name_a_themed_icon() {
        // Spec 1.2: `image-path` is "a URI or a name in an icon theme", the
        // same as `app_icon`. A bare name is an icon, a `/` path or
        // `file://` URI stays a file.
        for key in ["image-path", "image_path"] {
            let h = hints(&[(key, str_hint("mail-unread"))]);
            assert_eq!(picked(&h, "firefox"), "icon mail-unread");
            let h = hints(&[(key, str_hint("file:///tmp/a.png"))]);
            assert_eq!(picked(&h, "firefox"), "path file:///tmp/a.png");
            let h = hints(&[(key, str_hint("/tmp/a.png"))]);
            assert_eq!(picked(&h, "firefox"), "path /tmp/a.png");
        }
    }

    #[test]
    fn image_hints_follow_the_spec_order() {
        // Peel the winner off one at a time, from all five down to none:
        // image-data > image_data > image-path > image_path > app_icon >
        // icon_data.
        let mut all = vec![
            ("image-data", raw_hint(1)),
            ("image_data", raw_hint(2)),
            ("image-path", str_hint("/tmp/a.png")),
            ("image_path", str_hint("/tmp/b.png")),
            ("icon_data", raw_hint(3)),
        ];
        let expected = ["raw 1", "raw 2", "path /tmp/a.png", "path /tmp/b.png"];
        for want in expected {
            assert_eq!(picked(&hints(&all), "firefox"), want);
            all.remove(0);
        }
        // Only `icon_data` left: the spec ranks it *below* `app_icon`…
        assert_eq!(picked(&hints(&all), "firefox"), "icon firefox");
        // …so it is shown only when there is no `app_icon` either.
        assert_eq!(picked(&hints(&all), ""), "raw 3");
        assert_eq!(picked(&hints(&[]), ""), "none");
    }

    #[test]
    fn an_unusable_image_hint_falls_through_to_the_next() {
        // Malformed raw data, an empty path, a non-string path: each costs
        // only its own rank.
        let h = hints(&[("image-data", raw_hint(0)), ("image_data", raw_hint(2))]);
        assert_eq!(picked(&h, ""), "raw 2");
        let h = hints(&[
            ("image-path", str_hint("")),
            ("image_path", OwnedValue::from(7_u32)),
        ]);
        assert_eq!(picked(&h, "/usr/share/a.svg"), "path /usr/share/a.svg");
    }

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

    // ── Body markup stripping ────────────────────────────────────────────────

    #[test]
    fn strip_markup_keeps_a_bare_less_than_and_the_rest_of_the_body() {
        // The reported repro: the old stripper entered "in a tag" here and
        // never left, so the body rendered as `errors `.
        assert_eq!(strip_markup("errors < 5 in foo.rs"), "errors < 5 in foo.rs");
        // Not a tag either: no letter after the `<`, or no `>` to close it.
        assert_eq!(strip_markup("a <5 b"), "a <5 b");
        assert_eq!(strip_markup("see <foo"), "see <foo");
        assert_eq!(strip_markup("a </ b"), "a </ b");
        assert_eq!(strip_markup("x < y > z"), "x < y > z");
        assert_eq!(strip_markup("trailing <"), "trailing <");
    }

    #[test]
    fn strip_markup_strips_the_spec_subset_and_keeps_its_text() {
        assert_eq!(
            strip_markup("<b>bold</b>, <i>it</i>, <u>un</u>"),
            "bold, it, un"
        );
        assert_eq!(
            strip_markup(r#"see <a href="https://example.org/?q=1">the page</a>"#),
            "see the page"
        );
        assert_eq!(
            strip_markup(r#"pic: <img src="/tmp/a.png" alt="a"/>!"#),
            "pic: !"
        );
        assert_eq!(strip_markup("line<br/>line"), "lineline");
    }

    #[test]
    fn strip_markup_a_tag_like_run_interrupted_by_another_lt_is_text() {
        // `<b ` reaches no `>` before the next `<`, so it is text; the `<i>`
        // after it is still a tag.
        assert_eq!(strip_markup("a <b <i>x</i>"), "a <b x");
    }

    #[test]
    fn strip_markup_leaves_a_literal_ampersand_alone() {
        assert_eq!(strip_markup("Fish & Chips"), "Fish & Chips");
        assert_eq!(strip_markup("a && b; c"), "a && b; c");
        assert_eq!(strip_markup("trailing &"), "trailing &");
        // Unknown names are not ours to guess at.
        assert_eq!(strip_markup("a&nbsp;b"), "a&nbsp;b");
    }

    #[test]
    fn strip_markup_decodes_the_xml_entities() {
        assert_eq!(strip_markup("Fish &amp; Chips"), "Fish & Chips");
        assert_eq!(
            strip_markup("<b>Tom &amp; Jerry</b>"),
            "Tom & Jerry",
            "a markup client's escaped text must come out as the text it meant"
        );
        assert_eq!(strip_markup("&lt; &gt; &quot; &apos; &amp;"), "< > \" ' &");
    }

    #[test]
    fn strip_markup_decodes_numeric_references() {
        // `g_markup_escape_text`'s apostrophe.
        assert_eq!(strip_markup("it&#39;s"), "it's");
        assert_eq!(strip_markup("&#x27;&#X27;&#x1F600;"), "''\u{1F600}");
        // Not references to a character: kept as sent.
        for raw in [
            "&#0;",
            "&#xD800;",
            "&#x110000;",
            "&#+65;",
            "&#x;",
            "&#;",
            "&#65",
        ] {
            assert_eq!(strip_markup(raw), raw);
        }
    }

    #[test]
    fn strip_markup_never_reads_a_decoded_lt_as_a_tag() {
        // The escaped form of a tag is text the sender wanted shown.
        assert_eq!(strip_markup("&lt;b&gt;hi&lt;/b&gt;"), "<b>hi</b>");
    }

    #[test]
    fn capabilities_do_not_advertise_body_markup() {
        // `strip_markup`'s whole argument starts here: without `body-markup`
        // the body is plain text by the spec, and stripping is a courtesy to
        // clients that send markup regardless. Advertising it would turn that
        // courtesy into a promise to *render* markup, which nothing does.
        let caps = NotificationsIface::new().get_capabilities();
        assert!(caps.iter().any(|c| c == "body"), "{caps:?}");
        assert!(!caps.iter().any(|c| c == "body-markup"), "{caps:?}");
    }

    #[test]
    fn strip_markup_passes_plain_and_non_ascii_text_through() {
        assert_eq!(strip_markup(""), "");
        assert_eq!(strip_markup("plain text"), "plain text");
        assert_eq!(strip_markup("Grüße <b>ü</b> → ok"), "Grüße ü → ok");
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
        // `post_local`, on the half the shared registry holds, and the D-Bus
        // `Notify` handler, on the bus's half, draw from one counter, so
        // their ids interleave without ever colliding. Two toasts sharing an
        // id would let a close, replace, hover or click on one land on the
        // other. Both posts go through the real allocators, on the two halves
        // `Service::start` builds (`live_set`, below).
        let (kept, served) = live_set();
        let a = post_notify(&served, 0, None, -1); // a Notify
        let b = kept.post_local(
            "Wi-Fi",
            "Connection failed",
            "",
            Urgency::Normal,
            Vec::new(),
        );
        let c = post_notify(&served, 0, None, -1); // another Notify
        assert_eq!((a, b, c), (1, 2, 3));
    }

    // ── Notify id resolution: `replaces_id` is honoured only when live ──────

    /// A live notification with `id` and nothing else worth reading.
    fn live(id: u32) -> Notification {
        Notification {
            id,
            app_name: String::new(),
            app_icon: String::new(),
            summary: String::new(),
            body: String::new(),
            urgency: Urgency::Normal,
            timeout: Some(DEFAULT_TIMEOUT),
            actions: Vec::new(),
            image: None,
            created_at: 0,
        }
    }

    #[test]
    fn replaces_id_zero_allocates_fresh_ids() {
        let counter = AtomicU32::new(1);
        assert_eq!(notify_id(&[], 0, &counter), 1);
        assert_eq!(notify_id(&[live(1)], 0, &counter), 2);
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn replaces_id_of_a_live_notification_keeps_its_id() {
        let counter = AtomicU32::new(4);
        let list = [live(1), live(3)];
        assert_eq!(notify_id(&list, 3, &counter), 3);
        assert_eq!(
            counter.load(Ordering::Relaxed),
            4,
            "an in-place replace must not burn a counter value"
        );
    }

    #[test]
    fn replaces_id_never_issued_gets_a_fresh_id_and_cannot_collide_later() {
        // The reported repro: `Notify(app, 5000, …)` used to return 5000 —
        // never issued — without moving the counter, so the 5000th ordinary
        // notification replaced it in place and shared its timer entry.
        let counter = AtomicU32::new(2); // id 1 was issued, and is live
        let mut list = vec![live(1)];
        let bogus = notify_id(&list, 5000, &counter);
        assert_eq!(bogus, 2, "a non-live replaces_id must allocate, not echo");
        list.push(live(bogus));

        // Walk the counter all the way past 5000: nothing it hands out may
        // equal an id that is already live.
        counter.store(4999, Ordering::Relaxed);
        for _ in 0..3 {
            let next = notify_id(&list, 0, &counter);
            assert!(
                list.iter().all(|n| n.id != next),
                "fresh id {next} collides with a live notification"
            );
            list.push(live(next));
        }
    }

    #[test]
    fn replaces_id_of_a_closed_notification_gets_a_fresh_id() {
        // A progress update arriving just after its toast expired: id 7 was
        // real, but it is gone, so the update is a new notification.
        let counter = AtomicU32::new(8);
        assert_eq!(notify_id(&[live(6)], 7, &counter), 8);
    }

    // ── Close gate: `NotificationClosed` only for an id that was live ───────
    //
    // `dismiss` returns before its history push, its sweeps and its emit
    // whenever `closable` says `None`, so these pin what reaches the bus.

    #[test]
    fn closing_an_id_that_was_never_live_is_a_no_op() {
        // `gdbus call … CloseNotification 999` used to emit
        // `NotificationClosed(999, 3)` for an id this daemon never issued.
        let list = [live(1), live(2)];
        assert_eq!(closable(&list, &TimerMap::new(), 999, None), None);
        assert_eq!(closable(&[], &TimerMap::new(), 1, None), None);
    }

    #[test]
    fn a_live_id_is_closable_where_it_sits() {
        let list = [live(1), live(2), live(3)];
        assert_eq!(closable(&list, &TimerMap::new(), 2, None), Some(1));
    }

    #[test]
    fn a_second_close_of_one_id_is_a_no_op() {
        // A click landing as the expiry timer fires: `dismiss(id, 2)` and
        // `dismiss(id, 1)` both ran their emit, so the sender saw the one
        // notification close twice, with two different reasons.
        let mut list = vec![live(1), live(2)];
        let at = closable(&list, &TimerMap::new(), 2, None).expect("live on the first close");
        list.remove(at);
        assert_eq!(
            closable(&list, &TimerMap::new(), 2, None),
            None,
            "the second close must emit nothing"
        );
        assert_eq!(
            closable(&list, &TimerMap::new(), 1, None),
            Some(0),
            "a sibling is untouched"
        );
    }

    const ALL_URGENCIES: [Urgency; 3] = [Urgency::Low, Urgency::Normal, Urgency::Critical];

    /// The four cases #1542 settled, with literal durations rather than
    /// [`DEFAULT_TIMEOUT`], so a change to the constant shows up here too.
    #[test]
    fn timeout_rule_by_urgency() {
        let cases = [
            // A critical that leaves the timeout to us never expires.
            (Urgency::Critical, -1, None),
            // A critical with a timeout of its own gets exactly that.
            (Urgency::Critical, 3000, Some(Duration::from_secs(3))),
            // Anything else that leaves it to us gets the 5 s default.
            (Urgency::Normal, -1, Some(Duration::from_secs(5))),
            // 0 means never, whatever the urgency.
            (Urgency::Low, 0, None),
        ];
        for (urgency, expire_timeout, want) in cases {
            assert_eq!(
                resolve_timeout(urgency, expire_timeout),
                want,
                "({urgency:?}, {expire_timeout})"
            );
        }
    }

    #[test]
    fn negative_timeout_uses_server_default_below_critical() {
        // -1 ("you decide") is notify-send's default and the common case;
        // it must NOT be sticky (regression test for issue #15).
        for urgency in [Urgency::Low, Urgency::Normal] {
            assert_eq!(resolve_timeout(urgency, -1), Some(DEFAULT_TIMEOUT));
            assert_eq!(resolve_timeout(urgency, i32::MIN), Some(DEFAULT_TIMEOUT));
        }
    }

    #[test]
    fn negative_timeout_is_sticky_for_critical() {
        // Any negative value delegates to the server, not just -1.
        assert_eq!(resolve_timeout(Urgency::Critical, -1), None);
        assert_eq!(resolve_timeout(Urgency::Critical, i32::MIN), None);
    }

    #[test]
    fn zero_timeout_is_sticky() {
        // Per spec, 0 means never expire.
        for urgency in ALL_URGENCIES {
            assert_eq!(resolve_timeout(urgency, 0), None, "{urgency:?}");
        }
    }

    #[test]
    fn positive_timeout_is_exact_millis() {
        // A timeout the client names is honoured at every urgency, critical
        // included.
        for urgency in ALL_URGENCIES {
            assert_eq!(
                resolve_timeout(urgency, 1),
                Some(Duration::from_millis(1)),
                "{urgency:?}"
            );
            assert_eq!(
                resolve_timeout(urgency, 2500),
                Some(Duration::from_millis(2500)),
                "{urgency:?}"
            );
            assert_eq!(
                resolve_timeout(urgency, i32::MAX),
                Some(Duration::from_millis(i32::MAX as u64)),
                "{urgency:?}"
            );
        }
    }

    // ── What a post arms: the timer, not only the field (#1542) ─────────────
    //
    // `Notification::timeout` is read by nothing on screen; what closes a
    // toast is the sleep `set_expiry` arms. So these drive the real `Notify`
    // handler / `post_local` against handles of their own and read the
    // **timer entry** back. A finite post spawns a real sleep on the hytte
    // runtime; when it fires, its `close` finds no `NotificationsShared`
    // registered in a test binary and does nothing.

    /// What one post left for `id`: the timeout on the published notification,
    /// the timeout its timer entry holds, and whether that timer is counting
    /// down (a deadline **and** a live sleep).
    #[derive(Debug, PartialEq)]
    struct Posted {
        field: Option<Duration>,
        timer: Option<Duration>,
        counting_down: bool,
    }

    impl Posted {
        fn of(iface: &NotificationsIface, id: u32) -> Self {
            let field = iface
                .active
                .lock_ref()
                .iter()
                .find(|n| n.id == id)
                .expect("published")
                .timeout;
            let timers = iface.timers.lock().expect("not poisoned");
            let entry = timers
                .get(&id)
                .expect("every live notification has a timer entry");
            assert_eq!(
                entry.state.deadline.is_some(),
                entry.abort.is_some(),
                "a deadline and a live sleep come and go together"
            );
            Self {
                field,
                timer: entry.state.timeout,
                counting_down: entry.abort.is_some(),
            }
        }

        /// Stays until dismissed: no timeout anywhere, nothing counting down.
        const STAYS: Self = Self {
            field: None,
            timer: None,
            counting_down: false,
        };

        /// Expires after `secs`, with the sleep that does it armed.
        fn expires_after(secs: u64) -> Self {
            Self {
                field: Some(Duration::from_secs(secs)),
                timer: Some(Duration::from_secs(secs)),
                counting_down: true,
            }
        }

        /// Expires after `secs` once released, but held by a hover: nothing
        /// counting down.
        fn held(secs: u64) -> Self {
            Self {
                counting_down: false,
                ..Self::expires_after(secs)
            }
        }
    }

    /// Drive the real `Notify` handler; `urgency` is the hint's value, `None`
    /// for no hint.
    fn post_notify(
        iface: &NotificationsIface,
        replaces_id: u32,
        urgency: Option<OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let hint_map = urgency
            .map(|u| hints(&[("urgency", u)]))
            .unwrap_or_default();
        futures_executor::block_on(iface.notify(
            "client",
            replaces_id,
            "",
            "summary",
            "",
            Vec::new(),
            hint_map,
            expire_timeout,
        ))
    }

    fn urgency_of(iface: &NotificationsIface, id: u32) -> Urgency {
        iface
            .active
            .lock_ref()
            .iter()
            .find(|n| n.id == id)
            .expect("published")
            .urgency
    }

    /// The `Notify` handler hands [`set_expiry`] the timeout resolved at the
    /// urgency it parsed, on a fresh post and on every replace. The pure
    /// `resolve_timeout` tests above cannot see a handler that arms the timer
    /// differently; this one reads the timer.
    #[test]
    fn notify_arms_the_timer_at_the_hinted_urgency() {
        let iface = NotificationsIface::new();

        let critical = post_notify(&iface, 0, Some(OwnedValue::from(2_u8)), -1);
        assert_eq!(urgency_of(&iface, critical), Urgency::Critical);
        assert_eq!(
            Posted::of(&iface, critical),
            Posted::STAYS,
            "a critical with the default timeout arms no timer"
        );

        let critical_3s = post_notify(&iface, 0, Some(OwnedValue::from(2_u8)), 3000);
        assert_eq!(Posted::of(&iface, critical_3s), Posted::expires_after(3));

        let normal = post_notify(&iface, 0, None, -1);
        assert_eq!(urgency_of(&iface, normal), Urgency::Normal);
        assert_eq!(Posted::of(&iface, normal), Posted::expires_after(5));

        let low_sticky = post_notify(&iface, 0, Some(OwnedValue::from(0_u8)), 0);
        assert_eq!(urgency_of(&iface, low_sticky), Urgency::Low);
        assert_eq!(Posted::of(&iface, low_sticky), Posted::STAYS);

        // The client replaces the normal toast with a critical one: the 5 s
        // sleep it had armed is stood down, and nothing counts down.
        let id = post_notify(&iface, normal, Some(OwnedValue::from(2_u8)), -1);
        assert_eq!(id, normal, "a replace of a live id keeps the id");
        assert_eq!(
            Posted::of(&iface, normal),
            Posted::STAYS,
            "Normal → Critical stands the countdown down"
        );

        // And the sticky critical replaced by a normal one expires again.
        let id = post_notify(&iface, critical, Some(OwnedValue::from(1_u8)), -1);
        assert_eq!(id, critical, "a replace of a live id keeps the id");
        assert_eq!(
            Posted::of(&iface, critical),
            Posted::expires_after(5),
            "Critical → Normal arms the default"
        );
    }

    /// A client that sends the urgency as `uint32` (older Chromium builds) or
    /// `int32` (`gdbus call`'s text form, dbus-python's plain `int`) gets the
    /// critical rule too: the hint is read by value, not only as a byte.
    #[test]
    fn notify_reads_a_uint32_or_int32_urgency() {
        let iface = NotificationsIface::new();
        for hint in [OwnedValue::from(2_u32), OwnedValue::from(2_i32)] {
            let shown = format!("{hint:?}");
            let id = post_notify(&iface, 0, Some(hint), -1);
            assert_eq!(urgency_of(&iface, id), Urgency::Critical, "{shown}");
            assert_eq!(Posted::of(&iface, id), Posted::STAYS, "{shown}");
        }
    }

    #[test]
    fn parse_urgency_reads_every_integer_width_by_value() {
        let cases = [
            (OwnedValue::from(2_u8), Urgency::Critical),
            (OwnedValue::from(2_i16), Urgency::Critical),
            (OwnedValue::from(2_u16), Urgency::Critical),
            (OwnedValue::from(2_i32), Urgency::Critical),
            (OwnedValue::from(2_u32), Urgency::Critical),
            (OwnedValue::from(2_i64), Urgency::Critical),
            (OwnedValue::from(2_u64), Urgency::Critical),
            (OwnedValue::from(0_u8), Urgency::Low),
            (OwnedValue::from(0_u32), Urgency::Low),
            (OwnedValue::from(0_i32), Urgency::Low),
            (OwnedValue::from(1_u8), Urgency::Normal),
            (OwnedValue::from(1_u32), Urgency::Normal),
            // Out of range is Normal, not clamped into Critical or Low.
            (OwnedValue::from(3_u8), Urgency::Normal),
            (OwnedValue::from(255_u8), Urgency::Normal),
            (OwnedValue::from(3_u32), Urgency::Normal),
            (OwnedValue::from(u32::MAX), Urgency::Normal),
            (OwnedValue::from(-1_i32), Urgency::Normal),
            (OwnedValue::from(i64::MIN), Urgency::Normal),
            (OwnedValue::from(u64::MAX), Urgency::Normal),
            // Not an integer at all.
            (str_hint("2"), Urgency::Normal),
            (OwnedValue::from(true), Urgency::Normal),
        ];
        for (value, want) in cases {
            let shown = format!("{value:?}");
            assert_eq!(
                parse_urgency(&hints(&[("urgency", value)])),
                want,
                "{shown}"
            );
        }
        assert_eq!(parse_urgency(&HashMap::new()), Urgency::Normal, "no hint");
    }

    /// The shell's own toasts keep the 5 s default at every urgency, a
    /// `Critical` one included (the owner decision #1542 left open; see
    /// `post_local`). Driven through the real post, reading its timer.
    #[test]
    fn post_local_arms_the_default_timer_at_every_urgency() {
        let iface = NotificationsIface::new();
        for urgency in ALL_URGENCIES {
            let id = iface.post_local("Wi-Fi", "Wi-Fi connection failed", "", urgency, Vec::new());
            assert_eq!(urgency_of(&iface, id), urgency);
            assert_eq!(
                Posted::of(&iface, id),
                Posted::expires_after(5),
                "{urgency:?}"
            );
        }
    }

    // ── One live set: the bus and the shared side share one timer map (#1604)
    //
    // `Service::start` takes both halves from `mount_live_set`: the bus serves
    // one, and `NotificationsShared` keeps the other, which is how `close`,
    // the hover `pause_expiry` / `resume_expiry` and `post_local` reach the
    // daemon. These take the same two halves and cross them: arm through one,
    // hold, expire and close through the other, and read the result back
    // through the first. If the halves held different timer maps, every
    // expiry would be refused and every hold would land nowhere, while every
    // test above stayed green (M14/M15 in the #1599 review).

    /// The two halves `Service::start` builds: the live set the shared
    /// registry keeps, and the clone the bus serves.
    fn live_set() -> (NotificationsIface, NotificationsIface) {
        mount_live_set(|served| served)
    }

    /// The arming the live sleep for `id` presents when it fires, read from
    /// the timer map `iface` holds.
    fn arming_of(iface: &NotificationsIface, id: u32) -> u64 {
        iface
            .timers
            .lock()
            .expect("not poisoned")
            .get(&id)
            .expect("a live notification has a timer entry")
            .arming
    }

    fn has_timer(iface: &NotificationsIface, id: u32) -> bool {
        iface.timers.lock().expect("not poisoned").contains_key(&id)
    }

    /// The two halves share every handle, not only the two the crossing
    /// tests below happen to read (#1609 review, LOW-1): a fresh `next_id` or
    /// `local_actions` on the bus's half leaves those green. The destructure
    /// names every field with no `..`, so a field added later is a compile
    /// error here until it is checked too. That matters because a plain-value
    /// field would be deep-copied by `#[derive(Clone)]`, leaving the bus a
    /// copy of its own.
    #[test]
    fn the_two_halves_share_every_handle() {
        let (kept, served) = live_set();
        let NotificationsIface {
            active,
            next_id,
            local_actions,
            timers,
        } = &served;
        assert!(Arc::ptr_eq(next_id, &kept.next_id), "next_id");
        assert!(
            Arc::ptr_eq(local_actions, &kept.local_actions),
            "local_actions"
        );
        assert!(Arc::ptr_eq(timers, &kept.timers), "timers");
        // `Mutable` has no `ptr_eq`: write through one half, read the other.
        active.lock_mut().push(live(7));
        assert!(kept.active.lock_ref().iter().any(|n| n.id == 7), "active");
    }

    /// Arm through the bus's `Notify`, then hold, release, expire and close
    /// through the shared side, which is how the overlay's hover, the sleep
    /// and a click reach the daemon.
    #[test]
    fn a_notify_on_the_bus_is_held_and_closed_by_the_shared_side() {
        let (kept, served) = live_set();

        let id = post_notify(&served, 0, None, -1);
        assert_eq!(Posted::of(&served, id), Posted::expires_after(5));

        // The pointer enters the toast, then leaves it.
        kept.pause_expiry(id);
        assert_eq!(
            Posted::of(&served, id),
            Posted::held(5),
            "a hover on the shared side holds the countdown `Notify` armed"
        );
        kept.resume_expiry(id);
        assert!(
            Posted::of(&served, id).counting_down,
            "and the pointer leaving re-arms it"
        );

        // The sleep fires and presents the arming it was given.
        let fired = arming_of(&served, id);
        assert_eq!(
            kept.remove_closable(id, Some(fired)).map(|n| n.id),
            Some(id),
            "the shared side's close accepts the expiry `Notify` armed"
        );
        assert!(
            !has_timer(&served, id),
            "and reclaims the timer entry `Notify` made"
        );

        // A click on a sticky one: the entry `Notify` made goes with it.
        let sticky = post_notify(&served, 0, None, 0);
        assert_eq!(Posted::of(&served, sticky), Posted::STAYS);
        assert_eq!(
            kept.remove_closable(sticky, None).map(|n| n.id),
            Some(sticky)
        );
        assert!(
            !has_timer(&served, sticky),
            "no timer entry outlives its toast"
        );
        assert!(served.active.lock_ref().is_empty());
    }

    /// Arm through `post_local`, which posts through the half the shared
    /// registry keeps, then read and act from the bus's side: the bus sees
    /// the timer `post_local` armed, and a client's `Notify` replacing the
    /// shell's toast by id lands on that same entry, so a hover held on the
    /// shared side holds the replacement too (#619).
    #[test]
    fn a_post_local_toast_is_seen_and_replaced_on_the_bus() {
        let (kept, served) = live_set();

        // Its own expiry: the bus side sees the timer, and the sleep's
        // arming closes the toast.
        let expiring = kept.post_local(
            "Wi-Fi",
            "Connection failed",
            "",
            Urgency::Critical,
            Vec::new(),
        );
        assert_eq!(
            Posted::of(&served, expiring),
            Posted::expires_after(5),
            "the bus side sees the timer `post_local` armed"
        );
        let fired = arming_of(&served, expiring);
        assert_eq!(
            kept.remove_closable(expiring, Some(fired)).map(|n| n.id),
            Some(expiring),
            "the expiry `post_local` armed closes its toast"
        );
        assert!(!has_timer(&served, expiring));

        // Held on the shared side, then replaced by a client with a 3 s
        // timeout: the replace stays held, at the timeout it names.
        let replaced = kept.post_local(
            "VPN",
            "Connection failed",
            "",
            Urgency::Critical,
            Vec::new(),
        );
        kept.pause_expiry(replaced);
        assert_eq!(
            post_notify(&served, replaced, None, 3000),
            replaced,
            "a replace of a live id keeps the id"
        );
        assert_eq!(
            Posted::of(&served, replaced),
            Posted::held(3),
            "the replace lands on the entry `post_local` armed, hold and all"
        );
        kept.resume_expiry(replaced);
        assert!(Posted::of(&served, replaced).counting_down);
        assert_eq!(
            kept.remove_closable(replaced, None).map(|n| n.id),
            Some(replaced)
        );
        assert!(
            !has_timer(&kept, replaced) && !has_timer(&served, replaced),
            "no timer entry outlives its toast"
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
    fn replace_of_a_live_id_forgets_its_local_actions() {
        let mut map = LocalActionMap::new();
        register_local_action(&mut map, 1, "open".to_string(), Box::new(|| {}));
        register_local_action(&mut map, 1, "copy".to_string(), Box::new(|| {}));
        register_local_action(&mut map, 2, "open".to_string(), Box::new(|| {}));

        forget_replaced_local_actions(&mut map, &[live(1), live(2)], 1);

        // id 1's callbacks are gone, so a click broadcasts `ActionInvoked`;
        // id 2's stay.
        assert!(take_local_action(&mut map, 1, "open").is_none());
        assert!(take_local_action(&mut map, 1, "copy").is_none());
        assert!(map.contains_key(&(2, "open".to_string())));
    }

    #[test]
    fn replace_of_a_non_live_or_zero_id_keeps_local_actions() {
        let mut map = LocalActionMap::new();
        register_local_action(&mut map, 1, "open".to_string(), Box::new(|| {}));

        // 0 = replace nothing; 5 is not live: neither touches id 1.
        forget_replaced_local_actions(&mut map, &[live(1)], 0);
        forget_replaced_local_actions(&mut map, &[live(1)], 5);

        assert!(map.contains_key(&(1, "open".to_string())));
    }

    /// The `Notify` handler itself, not just the helper: a client's
    /// `replaces_id` naming a live shell-local toast retires that toast's
    /// callbacks, so the replacement's `open` broadcasts `ActionInvoked`
    /// instead of running the shell's. Driven through the real `notify`
    /// (#1572 review): its other side effect, `set_expiry`, arms in this
    /// test's own timer map.
    #[test]
    fn notify_replacing_a_live_local_toast_forgets_its_callbacks() {
        let local_actions = Arc::new(Mutex::new(LocalActionMap::new()));
        {
            let mut map = local_actions.lock().expect("fresh mutex");
            register_local_action(&mut map, 1, "open".to_string(), Box::new(|| {}));
            register_local_action(&mut map, 2, "open".to_string(), Box::new(|| {}));
        }
        let iface = NotificationsIface {
            active: Mutable::new(vec![live(1), live(2)]),
            next_id: Arc::new(AtomicU32::new(3)),
            local_actions: Arc::clone(&local_actions),
            timers: Arc::new(Mutex::new(TimerMap::new())),
        };

        let id = futures_executor::block_on(iface.notify(
            "client",
            1,
            "",
            "replaced",
            "",
            vec!["open".to_string(), "Open".to_string()],
            HashMap::new(),
            -1,
        ));

        assert_eq!(id, 1, "a replace of a live id keeps the id");
        let mut map = local_actions.lock().expect("not poisoned");
        assert!(
            take_local_action(&mut map, 1, "open").is_none(),
            "the replaced shell toast's callback must not run on the client's click"
        );
        assert!(
            map.contains_key(&(2, "open".to_string())),
            "another toast's callback is untouched"
        );
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

    /// Collect what [`apply_expiry`] would have armed — the duration, and the
    /// arming the task would present when it fires — without a runtime.
    fn armed_with(
        timers: &mut TimerMap,
        id: u32,
        timeout: Option<Duration>,
        now: Instant,
    ) -> Option<(Duration, u64)> {
        let armed = Cell::new(None);
        apply_expiry(timers, id, timeout, now, |dur, arming| {
            armed.set(Some((dur, arming)));
            None
        });
        armed.get()
    }

    /// The duration half of [`armed_with`].
    fn armed_by(
        timers: &mut TimerMap,
        id: u32,
        timeout: Option<Duration>,
        now: Instant,
    ) -> Option<Duration> {
        armed_with(timers, id, timeout, now).map(|(dur, _)| dur)
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

    // ── Superseded expiries: a fired sleep must still be the current arming ─
    //
    // `abort()` cannot reach a sleep task that is past its `sleep` and blocked
    // on the `active` lock inside `close` — there is no `.await` left for it to
    // land on — so the arming the task presents is what decides.

    #[test]
    fn a_superseded_expiry_cannot_close_the_replacement() {
        // The race: id 7's sleep fires and its close blocks on `active`, held
        // by a `Notify(replaces_id = 7)`. Pre-fix the close ran as soon as the
        // re-post released the lock and expired the brand-new replacement,
        // reason 1, the moment it appeared.
        let mut map = TimerMap::new();
        let t0 = Instant::now();
        let five = Duration::from_secs(5);
        let (_, fired) = armed_with(&mut map, 7, Some(five), t0).expect("a finite post arms");
        // The re-post, under the guard the fired close is waiting on.
        let (_, fresh) =
            armed_with(&mut map, 7, Some(five), t0 + five).expect("a finite re-post re-arms");
        let list = [live(7)];
        assert_eq!(
            closable(&list, &map, 7, Some(fired)),
            None,
            "the superseded expiry must not close the replacement"
        );
        assert_eq!(
            closable(&list, &map, 7, Some(fresh)),
            Some(0),
            "the replacement's own expiry still closes it"
        );
    }

    #[test]
    fn a_sticky_repost_supersedes_a_fired_expiry() {
        // Same race, re-posted sticky: nothing is re-armed, and the fired
        // sleep still must not "expire" a notification that no longer expires.
        let mut map = TimerMap::new();
        let t0 = Instant::now();
        let (_, fired) = armed_with(&mut map, 7, Some(Duration::from_secs(5)), t0).expect("arms");
        assert_eq!(armed_with(&mut map, 7, None, t0), None);
        assert_eq!(closable(&[live(7)], &map, 7, Some(fired)), None);
    }

    #[test]
    fn an_expiry_superseded_by_a_hover_cannot_close() {
        // The pointer lands as the sleep fires: the hover's pause must win, or
        // the toast closes out from under the pointer that is holding it.
        let mut map = TimerMap::new();
        let t0 = Instant::now();
        let five = Duration::from_secs(5);
        let (_, fired) = armed_with(&mut map, 7, Some(five), t0).expect("arms");
        apply_pause(&mut map, 7, t0 + five);
        assert_eq!(map[&7].state.hover_count, 1);
        assert_eq!(closable(&[live(7)], &map, 7, Some(fired)), None);
    }

    #[test]
    fn an_expiry_for_a_closed_notification_is_a_no_op() {
        // No entry: another path already closed it and reclaimed the timer.
        assert_eq!(closable(&[], &TimerMap::new(), 7, Some(0)), None);
    }

    #[test]
    fn a_user_or_bus_close_ignores_the_arming() {
        // Only an expiry has anything to prove; a click or `CloseNotification`
        // closes whatever is live under the id.
        let mut map = TimerMap::new();
        let t0 = Instant::now();
        armed_with(&mut map, 7, Some(Duration::from_secs(5)), t0);
        armed_with(&mut map, 7, Some(Duration::from_secs(5)), t0);
        assert_eq!(closable(&[live(7)], &map, 7, None), Some(0));
    }
}
