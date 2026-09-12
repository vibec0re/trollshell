//! Primitive #4 — cached property reads with `PropertiesChanged` tracking.
//!
//! See spec section 3.4.

use crate::BusError;
use crate::backoff::FailureStreak;
use crate::connection::SharedConnection;
use crate::handle::HandleTracker;
use futures_signals::signal::{Mutable, Signal, SignalExt as _};
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Duration;
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

/// Handle on a live property-tracking task. Cloning is cheap and does not
/// cancel; dropping the last clone tears down the background task (push-based,
/// via [`HandleTracker`]).
pub struct PropertySignal<T> {
    inner: Arc<PropertyInner<T>>,
    tracker: Arc<HandleTracker>,
}

impl<T> Clone for PropertySignal<T> {
    fn clone(&self) -> Self {
        self.tracker.inc();
        Self {
            inner: self.inner.clone(),
            tracker: self.tracker.clone(),
        }
    }
}

impl<T> Drop for PropertySignal<T> {
    fn drop(&mut self) {
        // Wake the tracking task on the last clone drop so it exits.
        self.tracker.dec();
    }
}

impl<T: Clone + Send + Sync + 'static> PropertySignal<T> {
    /// Returns a signal that emits [`PropState`] transitions as the tracked
    /// D-Bus property changes.
    ///
    /// `+ use<T>` for the reason `OwnNameSignal::signal_cloned` carries
    /// `+ use<>` (#750): without an opt-out the opaque type captures `&self`
    /// under Rust 2024's rules and can never be `'static`, so
    /// `hytte_reactive::bind` cannot take it. `T` still has to be captured —
    /// it is in the item type — but the borrow does not.
    pub fn signal(&self) -> impl Signal<Item = PropState<T>> + use<T> {
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
pub struct PropertyBuilder<T> {
    shared: SharedConnection,
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
pub fn property_with<T>(
    shared: &SharedConnection,
    destination: impl Into<String>,
) -> PropertyBuilder<T>
where
    T: Clone
        + Send
        + 'static
        + TryFrom<OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<Value<'v>, Error = zbus::zvariant::Error>,
{
    PropertyBuilder {
        shared: shared.clone(),
        destination: destination.into(),
        path: String::new(),
        iface: String::new(),
        name: String::new(),
        _t: std::marker::PhantomData,
    }
}

impl<T> PropertyBuilder<T>
where
    T: Clone
        + Send
        + Sync
        + 'static
        + TryFrom<OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<Value<'v>, Error = zbus::zvariant::Error>,
{
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

        let tracker = HandleTracker::new();
        let inner = Arc::new(PropertyInner {
            state: Mutable::new(PropState::Loading),
            task_done_rx: tokio::sync::Mutex::new(Some(task_done_rx)),
        });
        let writer = inner.state.clone();

        let ctx = PropCtx {
            shared: self.shared,
            dest: self.destination,
            path: self.path,
            iface: self.iface,
            name: self.name,
            tracker: tracker.clone(),
        };

        hytte_reactive::runtime::handle().spawn(async move {
            run_property::<T>(ctx, writer, task_done_tx).await;
        });

        PropertySignal { inner, tracker }
    }
}

// ── Context struct ────────────────────────────────────────────────────────────

struct PropCtx {
    shared: SharedConnection,
    dest: String,
    path: String,
    iface: String,
    name: String,
    /// Live-handle tracker: `all_dropped()` reports when every [`PropertySignal`]
    /// clone is gone, and `dropped()` is the push-based drop wakeup.
    tracker: Arc<HandleTracker>,
}

// ── Core property-tracking loop ──────────────────────────────────────────────

async fn run_property<T>(
    ctx: PropCtx,
    writer: Mutable<PropState<T>>,
    task_done_tx: tokio::sync::oneshot::Sender<()>,
) where
    T: Clone
        + Send
        + Sync
        + 'static
        + TryFrom<OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<Value<'v>, Error = zbus::zvariant::Error>,
{
    let mut last: Option<T> = None;
    let mut task_done_tx = Some(task_done_tx);
    // `true` while the pre-Get state for the current (re)connect cycle has
    // still to be published; cleared the moment it is, and set again by the
    // `Loaded` that ends the cycle. See the comment on the marking block below
    // for why a flag rather than an unconditional `set` (#1173).
    let mut needs_mark = true;
    // The crate's retry ramp, owned across the outer loop's iterations so a bus
    // that will not answer actually backs off instead of resetting to 250 ms
    // every time round. Cleared by a successful subscribe.
    let mut failures = FailureStreak::default();

    loop {
        if ctx.tracker.all_dropped() {
            tracing::debug!(
                dest = ctx.dest,
                path = ctx.path,
                iface = ctx.iface,
                name = ctx.name,
                "all property handles dropped; exiting task"
            );
            if let Some(tx) = task_done_tx.take() {
                let _ = tx.send(());
            }
            return;
        }

        // Mark the pre-Get state exactly once per (re)connect cycle — on the
        // edge, not on every attempt.
        //
        // The comment here has said "exactly once per (re)connect cycle" since
        // the block was written, but until #1173 nothing implemented it: the
        // `continue` below re-enters the outer loop on every failed subscribe,
        // so a property whose re-subscribe keeps failing (dead daemon, dead
        // connection) re-ran this `set` at the retry cadence forever.
        // `Mutable::set` notifies unconditionally — only `set_if_changed`
        // compares, and it needs `T: PartialEq`, which this type parameter does
        // not carry — so each of those was a real wakeup for every `bind`ing on
        // the signal, i.e. a GTK apply-loop running several times a second for
        // as long as the outage lasted, to re-apply a value that never changed.
        // The flag is what makes the sentence true.
        if needs_mark {
            match &last {
                Some(v) => writer.set(PropState::Stale(v.clone())),
                None => writer.set(PropState::Loading),
            }
            needs_mark = false;
        }

        // Subscribe to PropertiesChanged BEFORE the initial Get (#429). A change
        // emitted in the window between the Get and the AddMatch completing was
        // otherwise never seen, leaving `Loaded(stale)` until the property next
        // happened to change — a window that re-opens on every reconnect epoch.
        // With the subscription live first, any change during the Get is buffered
        // in `changes` and replayed by `drain_changes` below; because that drain
        // runs *after* the `Loaded(initial)` set, the buffered (newer) change
        // wins over the slightly-later Get — latest-wins, no clobber. This mirrors
        // own.rs (NameOwnerChanged before RequestName) and proxy.rs (NOC before
        // Live).
        let (mut changes, current_epoch) = match subscribe_properties_changed(&ctx).await {
            Ok(pair) => {
                failures.reset();
                pair
            }
            Err(e) => {
                crate::backoff::back_off_resubscribe(
                    &mut failures,
                    "property: PropertiesChanged subscribe",
                    &ctx.dest,
                    &e,
                )
                .await;
                continue;
            }
        };

        // Retry the cold Get until it succeeds. `None` means every handle was
        // dropped mid-retry and the task must exit; `retry_cold_get` has
        // already fired `task_done_tx`.
        let Some(initial) = retry_cold_get::<T>(&ctx, &mut task_done_tx).await else {
            return;
        };
        last = Some(initial.clone());
        writer.set(PropState::Loaded(initial));
        // The cycle closed with a value; the next disruption gets to mark once.
        needs_mark = true;

        let exited = drain_changes::<T>(
            &ctx,
            &mut last,
            &writer,
            &mut changes,
            current_epoch,
            &mut task_done_tx,
        )
        .await;
        if exited {
            return;
        }

        // Brief pause before re-subscribing to avoid a tight loop on bus
        // disconnect / invalidation.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// Retry the cold `Get` until it succeeds, backing off differently for
/// transient (bus mid-reconnect — retry promptly) versus permanent
/// (`ServiceUnknown`/`AccessDenied` — retry rarely, so a doomed call doesn't
/// hammer the bus at 2 Hz forever).
///
/// The caller's `PropertiesChanged` subscription stays live across every retry,
/// so a change landing mid-Get is buffered and still replayed by
/// `drain_changes` afterwards.
///
/// Returns `None` when every [`PropertySignal`] handle was dropped while
/// retrying — the caller must return, and `task_done_tx` has already been
/// fired here.
async fn retry_cold_get<T>(
    ctx: &PropCtx,
    task_done_tx: &mut Option<tokio::sync::oneshot::Sender<()>>,
) -> Option<T>
where
    T: Clone
        + Send
        + Sync
        + 'static
        + TryFrom<OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<Value<'v>, Error = zbus::zvariant::Error>,
{
    let mut perm_backoff = Duration::from_millis(500);
    let mut warned_permanent = false;
    loop {
        if ctx.tracker.all_dropped() {
            tracing::debug!(
                dest = ctx.dest,
                path = ctx.path,
                iface = ctx.iface,
                name = ctx.name,
                "all property handles dropped (Get retry); exiting task"
            );
            if let Some(tx) = task_done_tx.take() {
                let _ = tx.send(());
            }
            return None;
        }

        match cold_get::<T>(ctx).await {
            Ok(v) => return Some(v),
            Err(e) if e.is_transient() => {
                tracing::debug!(error = %e, dest = ctx.dest, path = ctx.path,
                    iface = ctx.iface, name = ctx.name,
                    "property Get failed (transient); will retry");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                if warned_permanent {
                    tracing::debug!(error = %e, dest = ctx.dest, path = ctx.path,
                        iface = ctx.iface, name = ctx.name,
                        retry_in_ms = perm_backoff.as_millis(),
                        "property Get still permanently failing");
                } else {
                    warned_permanent = true;
                    tracing::warn!(error = %e, dest = ctx.dest, path = ctx.path,
                        iface = ctx.iface, name = ctx.name,
                        "property Get permanently failing; backing off retries");
                }
                tokio::time::sleep(perm_backoff).await;
                perm_backoff = (perm_backoff * 2).min(Duration::from_mins(1));
            }
        }
    }
}

async fn cold_get<T>(ctx: &PropCtx) -> Result<T, BusError>
where
    T: Clone
        + Send
        + Sync
        + 'static
        + TryFrom<OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<Value<'v>, Error = zbus::zvariant::Error>,
{
    ctx.shared
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
                    .map_err(|e: zbus::zvariant::Error| zbus::Error::Failure(e.to_string()))?;
                Ok(typed)
            }
        })
        .await
}

/// Returns the `PropertiesChanged` stream and the epoch under which it was
/// built.
///
/// The `AddMatch` runs **inside** `with_conn` (#1173), not on a connection
/// fetched out of it. `Connection::add_match` answers
/// `InputOutput(BrokenPipe)` as soon as its socket reader task has died, and
/// that error is exactly what `with_conn`'s tail is watching for: it clears
/// the cached connection and wakes the supervisor. Until #1173 the subscribe
/// happened *outside* the closure — the `with_conn` above it only fetched the
/// connection through an infallible `Ok(conn)` — so a dead connection made
/// this retry forever while nothing invalidated the cache. Recovery was
/// parasitic on some *other* primitive on the same bus happening to issue a
/// fallible call; a process whose only use of a bus is `property()` never
/// reconnected at all. `own.rs` and `proxy.rs` always subscribed inside the
/// closure; this is now the same shape.
///
/// Epoch is captured *after* `with_conn` returns so a mid-build reconnect
/// doesn't leave us watching the wrong connection — same lesson as
/// signals.rs's cold-start fix, and the same ordering `proxy.rs`'s watcher
/// uses around `subscribe_noc`.
async fn subscribe_properties_changed(
    ctx: &PropCtx,
) -> Result<(zbus::fdo::PropertiesChangedStream, u64), BusError> {
    let subscribe_result = ctx
        .shared
        .with_conn(|conn| {
            let dest = ctx.dest.clone();
            let path = ctx.path.clone();
            async move {
                let props = zbus::fdo::PropertiesProxy::builder(&conn)
                    .destination(dest.as_str())?
                    .path(path.as_str())?
                    .build()
                    .await?;
                props.receive_properties_changed().await
            }
        })
        .await;

    let current_epoch = ctx.shared.epoch();

    subscribe_result.map(|s| (s, current_epoch))
}

/// Pump the `PropertiesChanged` stream until reconnect / invalidation / handle
/// drop. Returns `true` when all handles have been dropped and the watcher
/// should exit entirely. A drop-fired teardown `Notify` wakes the loop so we
/// detect handle-drops while parked (push-based; same pattern as the other
/// primitives). The epoch arm catches supervisor reconnects so we don't stay
/// stuck on the old connection's stream.
async fn drain_changes<T>(
    ctx: &PropCtx,
    last: &mut Option<T>,
    writer: &Mutable<PropState<T>>,
    changes: &mut zbus::fdo::PropertiesChangedStream,
    current_epoch: u64,
    task_done_tx: &mut Option<tokio::sync::oneshot::Sender<()>>,
) -> bool
where
    T: Clone
        + Send
        + Sync
        + 'static
        + TryFrom<OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<Value<'v>, Error = zbus::zvariant::Error>,
{
    let mut epoch_stream = ctx.shared.epoch_signal().to_stream();

    loop {
        if ctx.tracker.all_dropped() {
            tracing::debug!(
                dest = ctx.dest,
                path = ctx.path,
                iface = ctx.iface,
                name = ctx.name,
                "all property handles dropped (inner loop); exiting task"
            );
            if let Some(tx) = task_done_tx.take() {
                let _ = tx.send(());
            }
            return true;
        }

        tokio::select! {
            maybe_sig = changes.next() => {
                let Some(sig) = maybe_sig else { return false };
                if apply_change::<T>(ctx, &sig, last, writer) {
                    return false;
                }
            }
            epoch_update = epoch_stream.next() => {
                if let Some(new_epoch) = epoch_update && new_epoch > current_epoch {
                    tracing::debug!(dest = ctx.dest, path = ctx.path, iface = ctx.iface,
                        name = ctx.name, new_epoch,
                        "epoch advanced; breaking to re-Get on fresh connection");
                    return false;
                }
            }
            // The last handle was dropped — loop back to the all-dropped check
            // at the top, which exits the task.
            () = ctx.tracker.dropped() => {}
        }
    }
}

/// Apply one `PropertiesChanged` signal. Returns `true` when the caller should
/// break the drain loop (invalidation → re-Get on outer iteration).
fn apply_change<T>(
    ctx: &PropCtx,
    sig: &zbus::fdo::PropertiesChanged,
    last: &mut Option<T>,
    writer: &Mutable<PropState<T>>,
) -> bool
where
    T: Clone
        + Send
        + Sync
        + 'static
        + TryFrom<OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<Value<'v>, Error = zbus::zvariant::Error>,
{
    let Ok(args) = sig.args() else { return false };

    if args.interface_name != ctx.iface.as_str() {
        return false;
    }

    if let Some(raw) = args.changed_properties.get(ctx.name.as_str()) {
        match T::try_from(raw.clone()) {
            Ok(typed) => {
                *last = Some(typed.clone());
                writer.set(PropState::Loaded(typed));
            }
            Err(e) => {
                tracing::debug!(error = %e, name = ctx.name,
                    "PropertiesChanged: failed to decode value");
            }
        }
    }

    args.invalidated_properties.contains(&ctx.name.as_str())
}
