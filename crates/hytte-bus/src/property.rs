//! Primitive #4 — cached property reads with `PropertiesChanged` tracking.
//!
//! See spec section 3.4.

use crate::connection::SharedConnection;
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use std::sync::Arc;
use zbus::zvariant::{OwnedValue, Value};

/// Three states of a tracked property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropState<T> {
    /// Initial, before the first Get completes.
    Loading,
    /// Current authoritative value.
    Loaded(T),
    /// Last known value while the bus is reconnecting or the property is
    /// momentarily unavailable. UI can render this differently from
    /// `Loaded` (e.g. a dimmed CSS class).
    Stale(T),
}

// ── Inner state shared between the handle and the task ───────────────────────

struct PropertyInner<T> {
    state: Mutable<PropState<T>>,
    /// Fired when the internal task exits. Wrapped in a Mutex so it can be
    /// taken exactly once (for tests).
    task_done_rx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

// ── Public handle ─────────────────────────────────────────────────────────────

/// Handle on a live property-tracking task. Cloning is cheap (Arc) and does
/// not cancel; dropping the last clone tears down the background task.
#[derive(Clone)]
pub struct PropertySignal<T> {
    inner: Arc<PropertyInner<T>>,
}

impl<T: Clone + Send + Sync + 'static> PropertySignal<T> {
    /// Returns a signal that emits [`PropState`] transitions as the tracked
    /// D-Bus property changes.
    pub fn signal(&self) -> impl Signal<Item = PropState<T>> {
        self.inner.state.signal_cloned()
    }

    /// Take the oneshot receiver that fires when the internal tracking task
    /// exits. May only be called once per handle; returns `None` on subsequent
    /// calls. Intended for integration tests that need to verify the task
    /// actually shuts down when the handle is dropped.
    #[doc(hidden)]
    pub async fn task_done_receiver(&self) -> Option<tokio::sync::oneshot::Receiver<()>> {
        self.inner.task_done_rx.lock().await.take()
    }
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// Builder for a tracked D-Bus property.
pub struct PropertyBuilder<'a, T> {
    shared: &'a SharedConnection,
    destination: String,
    path: String,
    iface: String,
    name: String,
    _t: std::marker::PhantomData<T>,
}

/// Create a property-tracking builder for a remote D-Bus property.
///
/// Returns a [`PropertySignal`] handle. Call `.signal()` on the handle to
/// obtain a [`futures_signals::signal::Signal`] that emits [`PropState`]
/// transitions as the property value changes. Dropping the last clone of the
/// handle tears down the background task.
///
/// # Example
/// ```ignore
/// let prop = property_with::<u32>(&shared, "org.example.Counter")
///     .at_path("/org/example/Counter")
///     .iface("org.example.Counter")
///     .name("Value")
///     .start();
/// let mut stream = prop.signal().to_stream();
/// ```
#[doc(hidden)]
#[must_use]
pub fn property_with<'a, T>(
    shared: &'a SharedConnection,
    destination: impl Into<String>,
) -> PropertyBuilder<'a, T>
where
    T: Clone
        + Send
        + 'static
        + TryFrom<OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<Value<'v>, Error = zbus::zvariant::Error>,
{
    PropertyBuilder {
        shared,
        destination: destination.into(),
        path: String::new(),
        iface: String::new(),
        name: String::new(),
        _t: std::marker::PhantomData,
    }
}

impl<T> PropertyBuilder<'_, T>
where
    T: Clone
        + Send
        + Sync
        + 'static
        + TryFrom<OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<Value<'v>, Error = zbus::zvariant::Error>,
{
    /// Override which bus this builder targets. The default is determined by
    /// the constructor: [`property`](crate::property) uses the system bus.
    ///
    /// Overriding here replaces the `SharedConnection` with the corresponding
    /// global singleton.
    #[must_use]
    pub fn bus(self, kind: crate::BusKind) -> PropertyBuilder<'static, T> {
        PropertyBuilder {
            shared: match kind {
                crate::BusKind::Session => crate::connection::session(),
                crate::BusKind::System => crate::connection::system(),
            },
            destination: self.destination,
            path: self.path,
            iface: self.iface,
            name: self.name,
            _t: std::marker::PhantomData,
        }
    }

    /// Set the object path.
    #[must_use]
    pub fn at_path(mut self, p: impl Into<String>) -> Self {
        self.path = p.into();
        self
    }

    /// Set the D-Bus interface name.
    #[must_use]
    pub fn iface(mut self, i: impl Into<String>) -> Self {
        self.iface = i.into();
        self
    }

    /// Set the property member name (case-sensitive, typically `PascalCase`).
    #[must_use]
    pub fn name(mut self, n: impl Into<String>) -> Self {
        self.name = n.into();
        self
    }

    /// Spawn the tracking task. Returns a [`PropertySignal`] handle whose
    /// value transitions through [`PropState::Loading`] → [`PropState::Loaded`]
    /// and [`PropState::Stale`] on reconnects. Dropping the last clone of the
    /// handle tears down the background task.
    pub fn start(self) -> PropertySignal<T> {
        let (task_done_tx, task_done_rx) = tokio::sync::oneshot::channel::<()>();

        let inner = Arc::new(PropertyInner {
            state: Mutable::new(PropState::Loading),
            task_done_rx: tokio::sync::Mutex::new(Some(task_done_rx)),
        });
        let weak = Arc::downgrade(&inner);
        let writer = inner.state.clone();

        let shared = self.shared.clone();
        let dest = self.destination;
        let path = self.path;
        let iface = self.iface;
        let name = self.name;

        hytte_reactive::runtime::handle().spawn(async move {
            run_property::<T>(shared, dest, path, iface, name, writer, weak, task_done_tx).await;
        });

        PropertySignal { inner }
    }
}

// ── Context struct ────────────────────────────────────────────────────────────

struct PropCtx<T> {
    shared: SharedConnection,
    dest: String,
    path: String,
    iface: String,
    name: String,
    weak: std::sync::Weak<PropertyInner<T>>,
}

// ── Core property-tracking loop ──────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
async fn run_property<T>(
    shared: SharedConnection,
    dest: String,
    path: String,
    iface: String,
    name: String,
    writer: Mutable<PropState<T>>,
    weak: std::sync::Weak<PropertyInner<T>>,
    task_done_tx: tokio::sync::oneshot::Sender<()>,
) where
    T: Clone
        + Send
        + Sync
        + 'static
        + TryFrom<OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<Value<'v>, Error = zbus::zvariant::Error>,
{
    let ctx = PropCtx { shared, dest, path, iface, name, weak };
    let mut last: Option<T> = None;

    loop {
        // Exit cleanly if all handles have been dropped (checked at each
        // reconnect boundary — mirrors the signals primitive pattern).
        if ctx.weak.upgrade().is_none() {
            tracing::debug!(dest = ctx.dest, path = ctx.path, iface = ctx.iface,
                name = ctx.name, "all property handles dropped; exiting task");
            let _ = task_done_tx.send(());
            return;
        }

        // Mark state: Stale if we have a prior value, Loading otherwise.
        match &last {
            Some(v) => writer.set(PropState::Stale(v.clone())),
            None => writer.set(PropState::Loading),
        }

        // ── Cold Get ────────────────────────────────────────────────────────

        let get_result = ctx
            .shared
            .with_conn(|conn| {
                let dest = ctx.dest.clone();
                let path = ctx.path.clone();
                let iface = ctx.iface.clone();
                let name = ctx.name.clone();
                async move {
                    let props = zbus::fdo::PropertiesProxy::builder(&conn)
                        .destination(dest.as_str())?
                        .path(path.as_str())?
                        .build()
                        .await?;
                    let raw: OwnedValue = props
                        .get(iface.as_str().try_into()?, name.as_str())
                        .await
                        .map_err(zbus::Error::from)?;
                    let typed: T = raw
                        .try_into()
                        .map_err(|e: zbus::zvariant::Error| {
                            zbus::Error::Failure(e.to_string())
                        })?;
                    Ok(typed)
                }
            })
            .await;

        let initial = match get_result {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    dest = ctx.dest,
                    path = ctx.path,
                    iface = ctx.iface,
                    name = ctx.name,
                    "property Get failed; will retry"
                );
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
        };

        last = Some(initial.clone());
        writer.set(PropState::Loaded(initial));

        // ── PropertiesChanged listener ────────────────────────────────────

        // Grab the current connection to subscribe to `PropertiesChanged`.
        let conn_result = ctx
            .shared
            .with_conn(|conn| async move { Ok(conn) })
            .await;

        let conn = match conn_result {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    dest = ctx.dest,
                    "property: failed to get connection for PropertiesChanged; will retry"
                );
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
        };

        // Build `PropertiesProxy` and subscribe to `PropertiesChanged`.
        let subscribe_result = async {
            let props = zbus::fdo::PropertiesProxy::builder(&conn)
                .destination(ctx.dest.as_str())?
                .path(ctx.path.as_str())?
                .build()
                .await?;
            props.receive_properties_changed().await
        }
        .await;

        let mut changes = match subscribe_result {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    dest = ctx.dest,
                    "property: PropertiesChanged subscribe failed; will retry"
                );
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
        };

        // Drain the PropertiesChanged stream until it ends, an invalidation
        // triggers a re-Get, or all handles are dropped.
        //
        // A periodic liveness tick wakes the loop even when no D-Bus events
        // arrive so that we detect handle-drops promptly (same pattern as the
        // signals primitive).
        let mut liveness = tokio::time::interval(std::time::Duration::from_millis(100));
        liveness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        'changes: loop {
            // Check liveness before parking in select! — covers the case where
            // all handles were dropped while we were processing a prior event.
            if ctx.weak.upgrade().is_none() {
                tracing::debug!(dest = ctx.dest, path = ctx.path, iface = ctx.iface,
                    name = ctx.name,
                    "all property handles dropped (inner loop); exiting task");
                let _ = task_done_tx.send(());
                return;
            }

            tokio::select! {
                maybe_sig = changes.next() => {
                    let Some(sig) = maybe_sig else {
                        // Stream ended — reconnect.
                        break 'changes;
                    };
                    let Ok(args) = sig.args() else { continue };

                    if args.interface_name != ctx.iface.as_str() {
                        continue;
                    }

                    // Check for an inline value in `changed_properties`.
                    if let Some(raw) = args.changed_properties.get(ctx.name.as_str()) {
                        let decode: Result<T, _> = T::try_from(raw.clone());
                        match decode {
                            Ok(typed) => {
                                last = Some(typed.clone());
                                writer.set(PropState::Loaded(typed));
                            }
                            Err(e) => {
                                tracing::debug!(
                                    error = %e,
                                    name = ctx.name,
                                    "PropertiesChanged: failed to decode value"
                                );
                            }
                        }
                    }

                    // Invalidation: break to trigger a re-Get on the next loop iteration.
                    if args.invalidated_properties.contains(&ctx.name.as_str()) {
                        break 'changes;
                    }
                }
                _ = liveness.tick() => {
                    // Woke to check liveness — loop back to the upgrade check
                    // at the top of 'changes.
                }
            }
        }

        // Stream ended (bus disconnect or invalidation) — loop to re-Get.
        // Brief pause before re-subscribing to avoid a tight loop.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
