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
use std::time::Duration;
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
}

// ── Service handle ────────────────────────────────────────────────────────────

/// Service handle returned by [`service()`].
#[doc(hidden)]
pub struct NotificationsHandles {
    pub(crate) active: Mutable<Vec<Notification>>,
    pub(crate) next_id: Arc<AtomicU32>,
}

impl Default for NotificationsHandles {
    fn default() -> Self {
        Self {
            active: Mutable::new(Vec::new()),
            next_id: Arc::new(AtomicU32::new(1)),
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

        rt.spawn(async move {
            loop {
                match listen(&active_writer, &next_id).await {
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
    let active = registry::with(|r| {
        r.get::<NotificationsHandles>()
            .map(|h| h.active.clone())
    });
    let Some(active) = active else {
        return Ok(());
    };
    // Remove from active list.
    {
        let mut list = active.lock_mut();
        list.retain(|n| n.id != id);
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
/// Emits `ActionInvoked` on the session bus. Trollshell v0.4.0 doesn't
/// render action buttons, but this API is here for v0.4.1's history popup.
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
    next_id: Arc<AtomicU32>,
    conn: Connection,
}

impl State {
    fn new(
        active: Mutable<Vec<Notification>>,
        next_id: Arc<AtomicU32>,
        conn: Connection,
    ) -> Self {
        Self {
            active,
            next_id,
            conn,
        }
    }

    async fn dismiss(&self, id: u32, reason: u32) {
        // Remove from active list.
        {
            let mut list = self.active.lock_mut();
            list.retain(|n| n.id != id);
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
        // `actions` is received from callers but not rendered in v0.4.0.
        let _ = actions;
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

        let notification = Notification {
            id,
            app_name: app_name.to_string(),
            app_icon: app_icon.to_string(),
            summary: summary.to_string(),
            body: body_clean,
            urgency,
            timeout,
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
        vec!["body".to_string(), "icon-static".to_string()]
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

async fn listen(active: &Mutable<Vec<Notification>>, next_id: &Arc<AtomicU32>) -> Result<()> {
    let conn = Connection::session()
        .await
        .context("connect session bus")?;

    let state = State::new(active.clone(), next_id.clone(), conn.clone());
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
