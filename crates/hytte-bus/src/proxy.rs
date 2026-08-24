//! Primitive #5 — long-lived proxy handle that survives reconnects.
//!
//! See spec section 3.5.

use crate::connection::SharedConnection;
use crate::error::BusError;
use crate::handle::HandleTracker;
use futures_signals::signal::{Mutable, Signal, SignalExt};
use futures_util::StreamExt;
use serde::{Serialize, de::DeserializeOwned};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use zbus::zvariant::Type;

/// Default per-call timeout, matching `call()`'s default and D-Bus
/// conventions. A call that receives no reply within this window surfaces a
/// (permanent) timeout error instead of hanging the caller's task forever.
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(25);

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
    /// Per-call timeout applied to every [`BusProxy::call`].
    timeout: Duration,
    /// Fired when the watcher task exits. Wrapped in a Mutex so it can be
    /// taken exactly once (for tests).
    task_done_rx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

// ── Public handle ─────────────────────────────────────────────────────────────

/// Handle on a live proxy that survives bus reconnects. Cloning is cheap and
/// does not cancel; dropping the last clone tears down the background watcher
/// task (push-based, via [`HandleTracker`]).
pub struct BusProxy {
    inner: Arc<ProxyInner>,
    tracker: Arc<HandleTracker>,
}

impl Clone for BusProxy {
    fn clone(&self) -> Self {
        self.tracker.inc();
        Self {
            inner: self.inner.clone(),
            tracker: self.tracker.clone(),
        }
    }
}

impl Drop for BusProxy {
    fn drop(&mut self) {
        // Wake the watcher on the last clone drop so it exits.
        self.tracker.dec();
    }
}

impl BusProxy {
    /// Returns a [`Signal`] that emits [`ProxyState`] transitions as the
    /// proxy's connection and peer-presence state changes.
    ///
    /// `+ use<>` for the reason `OwnNameSignal::signal_cloned` carries it
    /// (#750): without it the opaque type captures `&self` under Rust 2024's
    /// rules and can never be `'static`, so `hytte_reactive::bind` cannot take
    /// it. The body returns a `MutableSignalCloned`, which already is.
    pub fn liveness(&self) -> impl Signal<Item = ProxyState> + use<> {
        self.inner.liveness.signal_cloned()
    }

    /// Call a D-Bus method on the remote object this proxy points to.
    ///
    /// `args` must be a tuple (even for a single argument: `(x,)`) or `()`.
    /// Returns `BusError::Transient` when the proxy is mid-reconnect.
    ///
    /// The call is bounded by the proxy's timeout (default 25 s, overridable
    /// with [`ProxyBuilder::timeout`]): a peer that never replies surfaces a
    /// (permanent) timeout error rather than hanging the caller's task forever.
    /// A transient (connection-level) failure additionally kicks the shared
    /// connection's reconnect path, so it is handled just like a `call()`
    /// failure instead of being swallowed on the cached proxy.
    pub async fn call<A, R>(&self, method: &str, args: A) -> Result<R, BusError>
    where
        A: Serialize + Type,
        R: DeserializeOwned + Type,
    {
        // Snapshot the epoch before the call so a transient failure only
        // invalidates the connection it was actually issued on.
        let epoch = self.inner.shared.epoch();

        // Clone the cheap `zbus::Proxy` handle out of the cache and release the
        // read lock immediately, so a slow call cannot block the watcher from
        // rebuilding the cache during a reconnect.
        let proxy = {
            let guard = self.inner.cached.read().await;
            guard.as_ref().cloned().ok_or_else(|| {
                let sentinel = zbus::Error::FDO(Box::new(zbus::fdo::Error::Disconnected(
                    "proxy mid-reconnect".into(),
                )));
                BusError::Transient { source: sentinel }
            })?
        };

        let fut = proxy.call::<_, _, R>(method, &args);
        let outcome = match tokio::time::timeout(self.inner.timeout, fut).await {
            Ok(inner) => inner,
            Err(_elapsed) => Err(zbus::Error::Failure("proxy call timeout".into())),
        };

        match outcome.map_err(BusError::from_zbus) {
            Ok(v) => Ok(v),
            Err(e) => {
                if e.is_transient() {
                    self.inner.shared.invalidate_if_epoch(epoch).await;
                }
                Err(e)
            }
        }
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
pub struct ProxyBuilder {
    shared: SharedConnection,
    destination: String,
    path: String,
    iface: String,
    timeout: Duration,
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
pub fn proxy_with(shared: &SharedConnection, destination: impl Into<String>) -> ProxyBuilder {
    ProxyBuilder {
        shared: shared.clone(),
        destination: destination.into(),
        path: String::new(),
        iface: String::new(),
        timeout: DEFAULT_CALL_TIMEOUT,
    }
}

impl ProxyBuilder {
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

    /// Override the per-call timeout applied to every [`BusProxy::call`]
    /// (default 25 s). Bound this lower for calls to peers that must stay
    /// responsive; a call exceeding it returns a timeout error.
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
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
        let tracker = HandleTracker::new();

        let inner = Arc::new(ProxyInner {
            shared: self.shared,
            destination: self.destination,
            path: self.path,
            iface: self.iface,
            cached: RwLock::new(None),
            liveness: Mutable::new(ProxyState::Reconnecting),
            timeout: self.timeout,
            task_done_rx: tokio::sync::Mutex::new(Some(task_done_rx)),
        });

        // Do the initial proxy build to verify connectivity and populate the
        // cache. The watcher sets liveness to `Live` AFTER it has also
        // established the `NameOwnerChanged` subscription, so callers waiting
        // on `liveness()` for `Live` are guaranteed the subscription is active.
        do_rebuild_proxy_cache(&inner).await?;

        // Spawn the watcher. Unlike the property/signals tasks, it holds a
        // strong `inner` (it dereferences the cached proxy / liveness fields);
        // the `HandleTracker` — not the Arc strong count — is the authority on
        // when every `BusProxy` clone has been dropped, so this does not leak.
        let task_inner = inner.clone();
        let task_tracker = tracker.clone();
        hytte_reactive::runtime::handle().spawn(async move {
            run_proxy_watcher(task_inner, task_tracker, task_done_tx).await;
        });

        Ok(BusProxy { inner, tracker })
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
async fn run_proxy_watcher(
    inner: Arc<ProxyInner>,
    tracker: Arc<HandleTracker>,
    task_done_tx: tokio::sync::oneshot::Sender<()>,
) {
    let mut first_iteration = true;
    let mut task_done_tx = Some(task_done_tx);
    let dest = inner.destination.clone();

    loop {
        if tracker.all_dropped() {
            tracing::debug!(%dest, "proxy watcher: all handles dropped; exiting");
            if let Some(tx) = task_done_tx.take() {
                let _ = tx.send(());
            }
            return;
        }

        let Some(mut stream) = subscribe_noc(&inner, &dest).await else {
            continue;
        };

        if !first_iteration && let Err(e) = do_rebuild_proxy_cache(&inner).await {
            tracing::debug!(error = %e, %dest,
                "proxy watcher: proxy rebuild failed; will retry");
            inner.liveness.set(ProxyState::Reconnecting);
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            continue;
        }
        first_iteration = false;

        let current_epoch = inner.shared.epoch();
        inner.liveness.set(ProxyState::Live);

        let exited = drain_noc_stream(
            &inner,
            &tracker,
            &dest,
            &mut stream,
            current_epoch,
            &mut task_done_tx,
        )
        .await;
        if exited {
            return;
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Build the NOC match rule and subscribe before emitting Live, so any NOC
/// signal fired after subscription (even between proxy-build and subscribe)
/// is buffered. Returns None on transient failure (caller should retry).
async fn subscribe_noc(inner: &Arc<ProxyInner>, dest: &str) -> Option<zbus::MessageStream> {
    let match_rule = match build_noc_match_rule(dest) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, %dest,
                "proxy watcher: failed to build match rule; retrying");
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            return None;
        }
    };

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

    match subscribe_result {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::debug!(error = %e, %dest,
                "proxy watcher: subscribe failed; will retry");
            inner.liveness.set(ProxyState::Reconnecting);
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            None
        }
    }
}

/// Returns `true` if the watcher should exit entirely (all handles dropped).
async fn drain_noc_stream(
    inner: &Arc<ProxyInner>,
    tracker: &HandleTracker,
    dest: &str,
    stream: &mut zbus::MessageStream,
    current_epoch: u64,
    task_done_tx: &mut Option<tokio::sync::oneshot::Sender<()>>,
) -> bool {
    let mut epoch_stream = inner.shared.epoch_signal().to_stream();

    loop {
        if tracker.all_dropped() {
            tracing::debug!(%dest, "proxy watcher: all handles dropped (inner loop); exiting");
            if let Some(tx) = task_done_tx.take() {
                let _ = tx.send(());
            }
            return true;
        }

        tokio::select! {
            maybe_msg = stream.next() => {
                if handle_noc_msg(maybe_msg, inner, dest).await {
                    return false;
                }
            }
            maybe_epoch = epoch_stream.next() => {
                if let Some(new_epoch) = maybe_epoch
                    && new_epoch > current_epoch
                {
                    tracing::debug!(%dest, new_epoch,
                        "proxy watcher: epoch advanced; rebuilding");
                    mark_reconnecting(inner).await;
                    return false;
                }
            }
            // The last BusProxy clone was dropped — loop back to the
            // all-dropped check at the top, which exits the task. This is the
            // push-based replacement for the old 100 ms liveness poll (and also
            // fixes the leak where a quiet, stable connection would never notice
            // a drop, since the watcher holds a strong `inner`).
            () = tracker.dropped() => {}
        }
    }
}

/// Returns `true` when the caller should break the drain loop.
async fn handle_noc_msg(
    maybe_msg: Option<Result<zbus::Message, zbus::Error>>,
    inner: &Arc<ProxyInner>,
    dest: &str,
) -> bool {
    match maybe_msg {
        None => {
            tracing::debug!(%dest, "proxy watcher: NOC stream ended; reconnecting");
            mark_reconnecting(inner).await;
            true
        }
        Some(Err(e)) => {
            tracing::debug!(error = %e, %dest,
                "proxy watcher: message error; reconnecting");
            mark_reconnecting(inner).await;
            true
        }
        Some(Ok(msg)) => {
            let body = msg.body();
            if let Ok((name, _old, new_owner)) = body.deserialize::<(String, String, String)>()
                && name == dest
            {
                if new_owner.is_empty() {
                    tracing::debug!(%dest, "proxy watcher: peer gone");
                    let mut cached = inner.cached.write().await;
                    *cached = None;
                    drop(cached);
                    inner.liveness.set(ProxyState::PeerGone);
                } else {
                    tracing::debug!(%dest, %new_owner,
                        "proxy watcher: peer back; rebuilding");
                    let _ = do_rebuild_proxy_cache(inner).await;
                    inner.liveness.set(ProxyState::Live);
                }
            }
            false
        }
    }
}

async fn mark_reconnecting(inner: &Arc<ProxyInner>) {
    let mut cached = inner.cached.write().await;
    *cached = None;
    drop(cached);
    inner.liveness.set(ProxyState::Reconnecting);
}
