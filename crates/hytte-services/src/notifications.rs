//! `org.freedesktop.Notifications` daemon — registers on the session bus and
//! exposes a reactive signal of live notifications for the shell to render.
//!
//! # Usage note
//!
//! Only one process on the session bus may own `org.freedesktop.Notifications`
//! at a time. Disable mako, dunst, or any other notification daemon before
//! starting trollshell, otherwise the name acquisition will fail and the
//! service will keep retrying.

use futures_signals::signal::{Mutable, Signal};
use hytte_bus::OwnNameSignal;
use hytte_reactive::{Service, registry, runtime};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
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
// `history.insert(...)` line. A static `OnceLock` populated by
// `Service::start` is the cross-thread-safe alternative — `Mutable<T>` and
// `Arc<AtomicU32>` are both `Send + Sync`, so storing them in a static is
// safe.
struct NotificationsShared {
    active: Mutable<Vec<Notification>>,
    history: Mutable<Vec<HistoryEntry>>,
    /// Shared id counter — the **same** `Arc<AtomicU32>` the D-Bus `Notify`
    /// handler (`NotificationsIface::next_id`) allocates from. Stored here so
    /// `post_local`, which runs on a hytte-tokio worker and can only reach
    /// `SHARED` (the registry counter is GTK-thread-only), draws ids from that
    /// one monotonic sequence — a locally-posted toast can therefore never
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
}

static SHARED: OnceLock<NotificationsShared> = OnceLock::new();

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
    /// Kept alive so the `own_name` task continues owning
    /// `org.freedesktop.Notifications` for the process lifetime.
    _ownership: OwnNameSignal,
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
        // lifetime keep-alive) and in SHARED (so dismiss/invoke_action can
        // emit signals directly without a round-trip D-Bus call).
        let ownership = hytte_bus::own_name("org.freedesktop.Notifications")
            .at_path("/org/freedesktop/Notifications", iface)
            .start();

        // Populate the cross-thread shared handle so `dismiss` / `clear_history`
        // can find these Mutables when called from a hytte-tokio worker (the
        // thread-local registry is GTK-only). Calling Service::start a second
        // time would `set` fail silently — services are registered once.
        let _ = SHARED.set(NotificationsShared {
            active: active.clone(),
            history: history.clone(),
            next_id: next_id.clone(),
            ownership: ownership.clone(),
            local_actions: Mutex::new(HashMap::new()),
        });

        NotificationsHandles {
            active,
            _next_id: next_id,
            history,
            _ownership: ownership,
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
    if let Some(shared) = SHARED.get() {
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
    }

    // Emit the NotificationClosed signal directly on the owned connection.
    if let Some(shared) = SHARED.get() {
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
    let Some(shared) = SHARED.get() else {
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
    if let Some(shared) = SHARED.get()
        && let Ok(mut map) = shared.local_actions.lock()
        && let Some(callback) = take_local_action(&mut map, id, action_key)
    {
        drop(map);
        callback();
        return;
    }

    let action_key = action_key.to_string();
    if let Some(shared) = SHARED.get() {
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
/// the process-global `SHARED` state.
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
/// Callable from **any** thread: it only touches the cross-thread `SHARED`
/// handle (never the GTK-thread-local registry), so command helpers running on
/// the hytte-tokio runtime can post directly.
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
    let Some(shared) = SHARED.get() else {
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

    shared.active.lock_mut().push(notification);
    tracing::debug!(id, app_name, summary, "local notification posted");

    // Auto-dismiss after the server-default timeout, mirroring `notify()`.
    runtime::handle().spawn(async move {
        tokio::time::sleep(DEFAULT_TIMEOUT).await;
        dismiss(id, 1); // 1 = expired
    });
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

        // Update active list: replace in-place if same id, else push.
        {
            let mut list = self.active.lock_mut();
            if let Some(existing) = list.iter_mut().find(|n| n.id == id) {
                *existing = notification.clone();
            } else {
                list.push(notification.clone());
            }
        }

        tracing::debug!(id, app_name, summary, "notification added");

        // Schedule auto-dismiss if this notification has a finite timeout.
        if let Some(dur) = timeout {
            tokio::spawn(async move {
                tokio::time::sleep(dur).await;
                crate::notifications::dismiss(id, 1); // 1 = expired
            });
        }

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

/// Decode an `image-data` / `icon_data` `OwnedValue` of type `(iiibiiay)`.
fn decode_image_data(val: &OwnedValue) -> Option<NotificationImage> {
    // The spec defines image-data as (iiibiiay):
    //   width: i32, height: i32, rowstride: i32, has_alpha: bool,
    //   bits_per_sample: i32, channels: i32, data: Vec<u8>
    type ImageTuple = (i32, i32, i32, bool, i32, i32, Vec<u8>);
    let cloned = val.try_clone().ok()?;
    let (width, height, rowstride, has_alpha, _bits_per_sample, channels, data) =
        ImageTuple::try_from(cloned).ok()?;
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
        DEFAULT_TIMEOUT, LocalActionMap, RateLimitMap, clear_local_actions, rate_limit_check,
        register_local_action, resolve_timeout, take_local_action,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::{Duration, Instant};

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
}
