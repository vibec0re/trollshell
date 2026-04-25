//! `org.freedesktop.Notifications` daemon — registers on the session bus and
//! exposes a reactive signal of live notifications for the shell to render.
//!
//! # Usage note
//!
//! Only one process on the session bus may own `org.freedesktop.Notifications`
//! at a time. Disable mako, dunst, or any other notification daemon before
//! starting trollshell, otherwise the name acquisition will fail and the
//! service will keep retrying every 2s.

use anyhow::{anyhow, Context, Result};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, runtime, Service};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;
use zbus::{Connection, fdo};

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
    /// Mapping from `expire_timeout`:
    /// - `0`  → `Some(5s)` (server default)
    /// - `<0` → `None` (sticky / never expires)
    /// - `>0` → `Some(millis)` (as requested)
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
    pub(crate) next_id: Arc<AtomicU32>,
    pub(crate) history: Mutable<Vec<HistoryEntry>>,
}

impl Default for NotificationsHandles {
    fn default() -> Self {
        Self {
            active: Mutable::new(Vec::new()),
            next_id: Arc::new(AtomicU32::new(1)),
            history: Mutable::new(Vec::new()),
        }
    }
}

// ── Service entry-point ───────────────────────────────────────────────────────

/// Marker type for the notifications daemon service.
pub struct NotificationsService;

impl Service for NotificationsService {
    type Handles = NotificationsHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = NotificationsHandles::default();
        let active_writer = handles.active.clone();
        let next_id = handles.next_id.clone();
        let history_writer = handles.history.clone();

        rt.spawn(async move {
            loop {
                match listen(&active_writer, &next_id, &history_writer).await {
                    Ok(()) => {
                        tracing::warn!(
                            "notifications daemon stream closed, reconnecting in 2s"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "notifications daemon error, reconnecting in 2s"
                        );
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });

        handles
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
/// Removes the notification from the active list and emits `NotificationClosed`
/// on the session bus.
pub fn dismiss(id: u32, reason: u32) {
    runtime::handle().spawn(async move {
        if let Err(e) = do_dismiss(id, reason).await {
            tracing::warn!(error = %e, id, reason, "notifications::dismiss failed");
        }
    });
}

async fn do_dismiss(id: u32, reason: u32) -> Result<()> {
    let handles = registry::with(|r| {
        r.get::<NotificationsHandles>()
            .map(|h| (h.active.clone(), h.history.clone()))
    });
    let Some((active, history)) = handles else {
        return Ok(());
    };
    // Remove from active list, capturing the notification for history.
    let removed = {
        let mut list = active.lock_mut();
        let pos = list.iter().position(|n| n.id == id);
        pos.map(|i| list.remove(i))
    };
    // Push to history if we found and removed the notification.
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
        let mut hist = history.lock_mut();
        hist.insert(0, entry);
        hist.truncate(100);
    }
    // Emit the D-Bus signal.
    let conn = Connection::session()
        .await
        .context("open session bus for dismiss")?;
    let emitter = SignalEmitter::new(&conn, "/org/freedesktop/Notifications")
        .context("create signal emitter")?;
    NotificationsIface::notification_closed(&emitter, id, reason)
        .await
        .context("emit NotificationClosed")?;
    Ok(())
}

/// Invoke action `action_key` on notification `id`.
///
/// Emits `ActionInvoked` on the session bus as a broadcast — the originating
/// app filters by `id` and reacts (e.g. opens its window, posts a reply form).
/// Both the toast widget and the history page wire action buttons to this.
pub fn invoke_action(id: u32, action_key: &str) {
    let action_key = action_key.to_string();
    runtime::handle().spawn(async move {
        if let Err(e) = do_invoke_action(id, &action_key).await {
            tracing::warn!(error = %e, id, action_key, "notifications::invoke_action failed");
        }
    });
}

async fn do_invoke_action(id: u32, action_key: &str) -> Result<()> {
    let conn = Connection::session()
        .await
        .context("open session bus for invoke_action")?;
    let emitter = SignalEmitter::new(&conn, "/org/freedesktop/Notifications")
        .context("create signal emitter")?;
    NotificationsIface::action_invoked(&emitter, id, action_key)
        .await
        .context("emit ActionInvoked")?;
    Ok(())
}

// ── Daemon shared state ───────────────────────────────────────────────────────

#[derive(Clone)]
struct State {
    active: Mutable<Vec<Notification>>,
    history: Mutable<Vec<HistoryEntry>>,
    next_id: Arc<AtomicU32>,
    conn: Connection,
}

impl State {
    fn new(
        active: Mutable<Vec<Notification>>,
        history: Mutable<Vec<HistoryEntry>>,
        next_id: Arc<AtomicU32>,
        conn: Connection,
    ) -> Self {
        Self {
            active,
            history,
            next_id,
            conn,
        }
    }

    async fn dismiss(&self, id: u32, reason: u32) {
        // Remove from active list, capturing the notification for history.
        let removed = {
            let mut list = self.active.lock_mut();
            let pos = list.iter().position(|n| n.id == id);
            pos.map(|i| list.remove(i))
        };
        // Push to history if we found and removed the notification.
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
            let mut hist = self.history.lock_mut();
            hist.insert(0, entry);
            hist.truncate(100);
        }
        // Emit the D-Bus signal.
        match SignalEmitter::new(&self.conn, "/org/freedesktop/Notifications") {
            Ok(emitter) => {
                if let Err(e) =
                    NotificationsIface::notification_closed(&emitter, id, reason).await
                {
                    tracing::warn!(error = %e, id, reason, "emit NotificationClosed failed");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "create emitter for dismiss failed");
            }
        }
    }
}

// ── D-Bus interface ───────────────────────────────────────────────────────────

struct NotificationsIface {
    state: State,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl NotificationsIface {
    /// Show a notification. Returns the notification id.
    ///
    /// `expire_timeout`:
    ///   - `0`  → use server default (5s)
    ///   - `<0` → notification is sticky (never auto-dismissed)
    ///   - `>0` → milliseconds
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::unused_async)]
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
            self.state.next_id.fetch_add(1, Ordering::Relaxed)
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

        // Resolve timeout.
        // expire_timeout: 0 → server default (5s), <0 → sticky, >0 → millis.
        let timeout = match expire_timeout.cmp(&0) {
            std::cmp::Ordering::Equal => Some(Duration::from_secs(5)),
            std::cmp::Ordering::Less => None,
            std::cmp::Ordering::Greater => {
                // expire_timeout > 0 here so the cast to u64 is safe.
                #[allow(clippy::cast_sign_loss)]
                Some(Duration::from_millis(expire_timeout as u64))
            }
        };

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
            let mut list = self.state.active.lock_mut();
            if let Some(existing) = list.iter_mut().find(|n| n.id == id) {
                *existing = notification.clone();
            } else {
                list.push(notification.clone());
            }
        }

        tracing::debug!(id, app_name, summary, "notification added");

        // Schedule auto-dismiss if this notification has a finite timeout.
        if let Some(dur) = timeout {
            let state = self.state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(dur).await;
                state.dismiss(id, 1).await; // 1 = expired
            });
        }

        id
    }

    /// Close a notification by id (reason 3 = closed by call).
    async fn close_notification(&self, id: u32) {
        self.state.dismiss(id, 3).await;
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

// ── Main listen loop ──────────────────────────────────────────────────────────

async fn listen(
    active: &Mutable<Vec<Notification>>,
    next_id: &Arc<AtomicU32>,
    history: &Mutable<Vec<HistoryEntry>>,
) -> Result<()> {
    let conn = Connection::session()
        .await
        .context("connect session bus")?;

    let state = State::new(active.clone(), history.clone(), next_id.clone(), conn.clone());
    let iface = NotificationsIface {
        state: state.clone(),
    };

    conn.object_server()
        .at("/org/freedesktop/Notifications", iface)
        .await
        .context("register /org/freedesktop/Notifications")?;

    let dbus = fdo::DBusProxy::new(&conn)
        .await
        .context("create DBusProxy")?;

    let flags = fdo::RequestNameFlags::ReplaceExisting | fdo::RequestNameFlags::DoNotQueue;
    let reply = dbus
        .request_name(
            "org.freedesktop.Notifications".try_into().unwrap(),
            flags,
        )
        .await
        .context("request_name org.freedesktop.Notifications")?;

    if reply != fdo::RequestNameReply::PrimaryOwner && reply != fdo::RequestNameReply::AlreadyOwner
    {
        return Err(anyhow!(
            "could not acquire org.freedesktop.Notifications: {reply:?}. \
             Disable mako/dunst or any other notification daemon first."
        ));
    }

    tracing::info!("org.freedesktop.Notifications acquired");

    // Park indefinitely; all work is driven by incoming D-Bus method calls.
    std::future::pending::<()>().await;
    Ok(())
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
