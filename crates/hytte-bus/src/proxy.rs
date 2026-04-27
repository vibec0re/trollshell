//! Primitive #5 — long-lived proxy handle that survives reconnects.
//!
//! See spec section 3.5.

use crate::connection::SharedConnection;
use crate::error::BusError;
use futures_signals::signal::{Mutable, Signal, SignalExt};
use futures_util::StreamExt;
use serde::{Serialize, de::DeserializeOwned};
use std::sync::{Arc, Weak};
use tokio::sync::RwLock;
use zbus::zvariant::Type;

// ── Public state enum ─────────────────────────────────────────────────────────

/// Liveness of a long-lived proxy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyState {
    /// Proxy is connected and the peer holds the destination name.
    Live,
    /// Bus is mid-reconnect; calls will return `BusError::Transient`
    /// momentarily.
    Reconnecting,
    /// Peer's destination name has no owner. Distinct from bus disconnect:
    /// the bus is fine, the *peer* (e.g. spotify) quit.
    PeerGone,
}

// ── Inner state ────────────────────────────────────────────────────────────────

struct ProxyInner {
    shared: SharedConnection,
    destination: String,
    path: String,
    iface: String,
    /// Cached `'static` proxy. `None` before `build()` completes and whenever
    /// the proxy is mid-reconnect (`Reconnecting`) or the peer has gone
    /// (`PeerGone`). Cleared explicitly on those transitions so that `call()`
    /// fast-fails with `BusError::Transient` instead of attempting I/O on a
    /// dead connection.
    cached: RwLock<Option<zbus::Proxy<'static>>>,
    liveness: Mutable<ProxyState>,
    /// Fired when the watcher task exits. Wrapped in a Mutex so it can be
    /// taken exactly once (for tests).
    task_done_rx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

// ── Public handle ─────────────────────────────────────────────────────────────

/// Handle on a live proxy that survives bus reconnects. Cloning is cheap
/// (Arc) and does not cancel; dropping the last clone tears down the
/// background watcher task.
#[derive(Clone)]
pub struct BusProxy {
    inner: Arc<ProxyInner>,
}

impl BusProxy {
    /// Returns a [`Signal`] that emits [`ProxyState`] transitions as the
    /// proxy's connection and peer-presence state changes.
    pub fn liveness(&self) -> impl Signal<Item = ProxyState> {
        self.inner.liveness.signal_cloned()
    }

    /// Call a D-Bus method on the remote object this proxy points to.
    ///
    /// `args` must be a tuple (even for a single argument: `(x,)`) or `()`.
    /// Returns `BusError::Transient` when the proxy is mid-reconnect.
    pub async fn call<A, R>(&self, method: &str, args: A) -> Result<R, BusError>
    where
        A: Serialize + Type,
        R: DeserializeOwned + Type,
    {
        let guard = self.inner.cached.read().await;
        let proxy = guard.as_ref().ok_or_else(|| {
            let sentinel = zbus::Error::FDO(Box::new(zbus::fdo::Error::Disconnected(
                "proxy mid-reconnect".into(),
            )));
            BusError::Transient { source: sentinel }
        })?;
        proxy
            .call::<_, _, R>(method, &args)
            .await
            .map_err(BusError::from_zbus)
    }

    /// Take the oneshot receiver that fires when the internal watcher task
    /// exits. May only be called once per proxy; returns `None` on subsequent
    /// calls. Intended for integration tests that verify task teardown when all
    /// clones are dropped.
    #[doc(hidden)]
    pub async fn task_done_receiver(&self) -> Option<tokio::sync::oneshot::Receiver<()>> {
        self.inner.task_done_rx.lock().await.take()
    }
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// Builder for a long-lived [`BusProxy`].
#[must_use]
pub struct ProxyBuilder<'a> {
    shared: &'a SharedConnection,
    destination: String,
    path: String,
    iface: String,
}

/// Create a proxy builder for the given destination well-known name.
///
/// # Example
/// ```ignore
/// let proxy = proxy_with(&shared, "org.freedesktop.DBus")
///     .at_path("/org/freedesktop/DBus")
///     .iface("org.freedesktop.DBus")
///     .build().await?;
/// ```
#[doc(hidden)]
pub fn proxy_with(
    shared: &SharedConnection,
    destination: impl Into<String>,
) -> ProxyBuilder<'_> {
    ProxyBuilder {
        shared,
        destination: destination.into(),
        path: String::new(),
        iface: String::new(),
    }
}

impl ProxyBuilder<'_> {
    /// Override which bus this builder targets. The default is determined by
    /// the constructor: [`proxy`](crate::proxy) uses the system bus.
    ///
    /// Overriding here replaces the `SharedConnection` with the corresponding
    /// global singleton.
    pub fn bus(self, kind: crate::BusKind) -> ProxyBuilder<'static> {
        ProxyBuilder {
            shared: match kind {
                crate::BusKind::Session => crate::connection::session(),
                crate::BusKind::System => crate::connection::system(),
            },
            destination: self.destination,
            path: self.path,
            iface: self.iface,
        }
    }

    /// Set the object path.
    pub fn at_path(mut self, p: impl Into<String>) -> Self {
        self.path = p.into();
        self
    }

    /// Set the D-Bus interface name.
    pub fn iface(mut self, i: impl Into<String>) -> Self {
        self.iface = i.into();
        self
    }

    /// Build the proxy. Connects to the current bus epoch, builds a cached
    /// `zbus::Proxy`, and spawns a watcher task. The proxy starts in
    /// `Reconnecting` state; the watcher transitions it to `Live` once it has
    /// subscribed to `NameOwnerChanged` on the bus.
    ///
    /// # Errors
    /// Returns `BusError` if the initial connection or proxy construction fails.
    pub async fn build(self) -> Result<BusProxy, BusError> {
        let (task_done_tx, task_done_rx) = tokio::sync::oneshot::channel::<()>();

        let inner = Arc::new(ProxyInner {
            shared: self.shared.clone(),
            destination: self.destination,
            path: self.path,
            iface: self.iface,
            cached: RwLock::new(None),
            liveness: Mutable::new(ProxyState::Reconnecting),
            task_done_rx: tokio::sync::Mutex::new(Some(task_done_rx)),
        });

        // Do the initial proxy build to verify connectivity and populate the
        // cache. The watcher sets liveness to `Live` AFTER it has also
        // established the `NameOwnerChanged` subscription, so callers waiting
        // on `liveness()` for `Live` are guaranteed the subscription is active.
        do_rebuild_proxy_cache(&inner).await?;

        // Spawn the watcher that reacts to NameOwnerChanged + epoch advances.
        let watch_inner = Arc::downgrade(&inner);
        hytte_reactive::runtime::handle().spawn(async move {
            run_proxy_watcher(watch_inner, task_done_tx).await;
        });

        Ok(BusProxy { inner })
    }
}

// ── Proxy cache rebuild helper ────────────────────────────────────────────────

/// Build or rebuild the cached `zbus::Proxy<'static>`. Does NOT set liveness —
/// the watcher does that AFTER subscribing to `NameOwnerChanged` to avoid a
/// race where the caller sees `Live` before the subscription is established.
async fn do_rebuild_proxy_cache(inner: &Arc<ProxyInner>) -> Result<(), BusError> {
    let dest = inner.destination.clone();
    let path = inner.path.clone();
    let iface = inner.iface.clone();

    let new_proxy = inner
        .shared
        .with_conn(|conn| {
            let dest = dest.clone();
            let path = path.clone();
            let iface = iface.clone();
            async move {
                // `new_owned` takes the connection by value and 'static
                // destination/path/interface strings, yielding a Proxy<'static>.
                zbus::Proxy::new_owned(conn, dest, path, iface).await
            }
        })
        .await?;

    let mut cached = inner.cached.write().await;
    *cached = Some(new_proxy);
    Ok(())
}

// ── Watcher task ──────────────────────────────────────────────────────────────

/// Build the `NameOwnerChanged` match rule for the given destination name,
/// filtered by arg0 (the service name).
fn build_noc_match_rule(dest: &str) -> Result<zbus::OwnedMatchRule, zbus::Error> {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")
        .map_err(|e| zbus::Error::Failure(e.to_string()))?
        .path("/org/freedesktop/DBus")
        .map_err(|e| zbus::Error::Failure(e.to_string()))?
        .interface("org.freedesktop.DBus")
        .map_err(|e| zbus::Error::Failure(e.to_string()))?
        .member("NameOwnerChanged")
        .map_err(|e| zbus::Error::Failure(e.to_string()))?
        .arg(0, dest)
        .map_err(|e| zbus::Error::Failure(e.to_string()))?
        .build();
    Ok(rule.into())
}

/// Long-running watcher: reacts to `NameOwnerChanged` signals for the
/// destination and to bus epoch advances. Exits when all `BusProxy` clones
/// are dropped.
///
/// Architecture:
/// 1. Subscribe to NOC (before setting Live — avoids a race where the caller
///    sees Live but the NOC subscription is not yet active).
/// 2. Rebuild the cached proxy if needed.
/// 3. Emit Live.
/// 4. Drain the NOC stream + epoch signal until the stream ends or epoch bumps.
/// 5. Set Reconnecting, loop.
#[allow(clippy::too_many_lines)]
async fn run_proxy_watcher(
    weak: Weak<ProxyInner>,
    task_done_tx: tokio::sync::oneshot::Sender<()>,
) {
    let mut first_iteration = true;

    loop {
        // Exit if all BusProxy handles have been dropped.
        let Some(inner) = weak.upgrade() else {
            tracing::debug!("proxy watcher: all handles dropped; exiting");
            let _ = task_done_tx.send(());
            return;
        };

        let dest = inner.destination.clone();

        // Build the NOC match rule (infallible in practice — destination names
        // were validated by the builder, but errors are handled gracefully).
        let match_rule = match build_noc_match_rule(&dest) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, %dest,
                    "proxy watcher: failed to build match rule; retrying");
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
        };

        // Subscribe to NameOwnerChanged FIRST, before emitting Live.
        // This ensures any NOC signal fired after the subscription is set up
        // (even if it arrived between proxy-build and subscribe) is buffered.
        let subscribe_result = inner
            .shared
            .with_conn(|conn| {
                let rule = match_rule.clone();
                async move {
                    let stream = zbus::MessageStream::for_match_rule(rule, &conn, None).await?;
                    Ok(stream)
                }
            })
            .await;

        let mut stream = match subscribe_result {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, %dest,
                    "proxy watcher: subscribe failed; will retry");
                inner.liveness.set(ProxyState::Reconnecting);
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
        };

        // On iterations after the first, rebuild the proxy cache against the
        // current (post-reconnect) connection.
        if !first_iteration && let Err(e) = do_rebuild_proxy_cache(&inner).await {
            tracing::debug!(error = %e, %dest,
                "proxy watcher: proxy rebuild failed; will retry");
            inner.liveness.set(ProxyState::Reconnecting);
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            continue;
        }
        first_iteration = false;

        // Record the epoch at the time we subscribed so we can detect a
        // bus reconnect that bumps the epoch.
        let current_epoch = inner.shared.epoch();
        let mut epoch_stream = inner.shared.epoch_signal().to_stream();

        // Now that we're subscribed and have a working proxy, emit Live.
        inner.liveness.set(ProxyState::Live);

        // Periodic liveness tick — lets us detect handle-drops while parked.
        let mut liveness_tick = tokio::time::interval(std::time::Duration::from_millis(100));
        liveness_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        'drain: loop {
            // Check if all handles have been dropped.
            if weak.upgrade().is_none() {
                tracing::debug!(%dest,
                    "proxy watcher: all handles dropped (inner loop); exiting");
                let _ = task_done_tx.send(());
                return;
            }

            tokio::select! {
                maybe_msg = stream.next() => {
                    match maybe_msg {
                        None => {
                            // Stream ended — bus disconnected or match rule removed.
                            tracing::debug!(%dest,
                                "proxy watcher: NOC stream ended; reconnecting");
                            {
                                let mut cached = inner.cached.write().await;
                                *cached = None;
                            }
                            inner.liveness.set(ProxyState::Reconnecting);
                            break 'drain;
                        }
                        Some(Ok(msg)) => {
                            // Deserialize the NameOwnerChanged body:
                            // (name, old_owner, new_owner).
                            let body = msg.body();
                            match body.deserialize::<(String, String, String)>() {
                                Ok((name, _old, new_owner)) if name == dest => {
                                    if new_owner.is_empty() {
                                        tracing::debug!(%dest,
                                            "proxy watcher: peer gone");
                                        {
                                            let mut cached = inner.cached.write().await;
                                            *cached = None;
                                        }
                                        inner.liveness.set(ProxyState::PeerGone);
                                    } else {
                                        tracing::debug!(%dest, %new_owner,
                                            "proxy watcher: peer back; rebuilding");
                                        let _ = do_rebuild_proxy_cache(&inner).await;
                                        inner.liveness.set(ProxyState::Live);
                                    }
                                }
                                _ => {} // ignore signals for other names or decode errors
                            }
                        }
                        Some(Err(e)) => {
                            tracing::debug!(error = %e, %dest,
                                "proxy watcher: message error; reconnecting");
                            {
                                let mut cached = inner.cached.write().await;
                                *cached = None;
                            }
                            inner.liveness.set(ProxyState::Reconnecting);
                            break 'drain;
                        }
                    }
                }
                maybe_epoch = epoch_stream.next() => {
                    if let Some(new_epoch) = maybe_epoch
                        && new_epoch > current_epoch
                    {
                        tracing::debug!(%dest, new_epoch,
                            "proxy watcher: epoch advanced; rebuilding");
                        {
                            let mut cached = inner.cached.write().await;
                            *cached = None;
                        }
                        inner.liveness.set(ProxyState::Reconnecting);
                        break 'drain;
                    }
                }
                _ = liveness_tick.tick() => {
                    // Liveness tick — loop back to the handle-drop check above.
                }
            }
        }

        // Brief pause before re-subscribing to avoid tight loops when the bus
        // is cycling rapidly.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
