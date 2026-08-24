//! Primitive #2 — subscribe to D-Bus signals on a remote object.
//!
//! See spec section 3.2.

use crate::connection::SharedConnection;
use crate::handle::HandleTracker;
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::SystemTime;

/// A single signal emission delivered to the consumer.
#[derive(Clone)]
pub struct SignalEvent {
    /// The raw zbus message; consumer calls `.body().deserialize::<T>()`
    /// to decode arguments.
    pub body: zbus::Message,
    /// Sender's unique name, if known.
    pub sender: Option<String>,
    /// Local clock when the event was received.
    pub timestamp: SystemTime,
}

/// Handle on a live signal subscription. Cloning is cheap and does not cancel;
/// dropping the last clone tears down the subscription (push-based, via
/// [`HandleTracker`]).
pub struct SignalSubscription {
    inner: Arc<SubInner>,
    tracker: Arc<HandleTracker>,
}

impl Clone for SignalSubscription {
    fn clone(&self) -> Self {
        self.tracker.inc();
        Self {
            inner: self.inner.clone(),
            tracker: self.tracker.clone(),
        }
    }
}

impl Drop for SignalSubscription {
    fn drop(&mut self) {
        // Wake the subscription task on the last clone drop so it exits.
        self.tracker.dec();
    }
}

struct SubInner {
    sender: tokio::sync::broadcast::Sender<Arc<SignalEvent>>,
    /// A oneshot receiver that resolves when the internal task exits.
    /// Exposed via `task_done_receiver()` for integration tests.
    task_done_rx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl SignalSubscription {
    /// Stream of signal emissions. Each call to `events()` returns an
    /// independent receiver; backpressure is handled by zbus' broadcast
    /// channel (slow consumers may lag).
    ///
    /// `+ use<>` for the reason `OwnNameSignal::signal_cloned` carries it
    /// (#750): without it the opaque type captures `&self` under Rust 2024's
    /// rules and is never `'static`, so it could not be spawned onto a task or
    /// stored. The stream owns its `broadcast::Receiver` and borrows nothing.
    pub fn events(&self) -> impl futures_util::Stream<Item = SignalEvent> + Unpin + use<> {
        use tokio::sync::broadcast::error::RecvError;
        let mut rx = self.inner.sender.subscribe();
        Box::pin(async_stream::stream! {
            loop {
                match rx.recv().await {
                    Ok(evt) => yield (*evt).clone(),
                    // A burst overflowed the broadcast channel and this
                    // consumer fell behind. The receiver is still usable — the
                    // next `recv()` yields the oldest event still buffered — so
                    // warn and keep going. Treating this as end-of-stream (the
                    // old `while let Ok`) permanently froze every downstream
                    // subscriber after an event burst (#428).
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(
                            missed = n,
                            "signal event consumer lagged behind the broadcast \
                             channel; dropped buffered events and continuing \
                             (only channel close ends the stream)"
                        );
                    }
                    // The sender was dropped — no more events will ever arrive.
                    Err(RecvError::Closed) => break,
                }
            }
        })
    }

    /// Take the oneshot receiver that fires when the internal subscription
    /// task exits. May only be called once per subscription; returns `None`
    /// on subsequent calls. Intended for integration tests that need to verify
    /// the task actually shuts down when the subscription is dropped.
    #[doc(hidden)]
    pub async fn task_done_receiver(&self) -> Option<tokio::sync::oneshot::Receiver<()>> {
        self.inner.task_done_rx.lock().await.take()
    }
}

/// Context passed to `run_subscription` to avoid too-many-arguments lint.
struct RunCtx {
    shared: SharedConnection,
    dest: String,
    path: String,
    iface: String,
    signal_name: String,
    tx: tokio::sync::broadcast::Sender<Arc<SignalEvent>>,
    /// Live-handle tracker. When every `SignalSubscription` clone has been
    /// dropped, `all_dropped()` becomes true — the definitive signal that no
    /// consumer will ever call `events()` again and the task can exit.
    tracker: Arc<HandleTracker>,
    /// Fired when the task exits so integration tests can verify teardown.
    task_done_tx: tokio::sync::oneshot::Sender<()>,
}

/// Builder.
pub struct SignalsBuilder {
    shared: SharedConnection,
    destination: String,
    path: String,
    iface: String,
    signal: String,
}

impl SignalsBuilder {
    /// Set the object path to listen on.
    #[must_use]
    pub fn at_path(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    /// Set the D-Bus interface name.
    #[must_use]
    pub fn iface(mut self, name: impl Into<String>) -> Self {
        self.iface = name.into();
        self
    }

    /// Set the signal member name to subscribe to.
    #[must_use]
    pub fn signal(mut self, name: impl Into<String>) -> Self {
        self.signal = name.into();
        self
    }

    /// Spawn the subscription task. Returns a [`SignalSubscription`] handle
    /// from which independent event streams can be obtained.
    #[must_use]
    pub fn start(self) -> SignalSubscription {
        let (tx, _) = tokio::sync::broadcast::channel(64);
        let (task_done_tx, task_done_rx) = tokio::sync::oneshot::channel::<()>();
        let tracker = HandleTracker::new();

        let inner = Arc::new(SubInner {
            sender: tx.clone(),
            task_done_rx: tokio::sync::Mutex::new(Some(task_done_rx)),
        });
        let sub = SignalSubscription {
            inner,
            tracker: tracker.clone(),
        };
        let ctx = RunCtx {
            shared: self.shared,
            dest: self.destination,
            path: self.path,
            iface: self.iface,
            signal_name: self.signal,
            tx,
            tracker,
            task_done_tx,
        };
        hytte_reactive::runtime::handle().spawn(async move {
            run_subscription(ctx).await;
        });
        sub
    }
}

/// Entry point for creating a signal subscription builder.
///
/// # Example
/// ```ignore
/// let sub = signals_with(&shared, "org.example.Service")
///     .at_path("/org/example/Service")
///     .iface("org.example.Service")
///     .signal("Changed")
///     .start();
/// ```
#[doc(hidden)]
#[must_use]
pub fn signals_with(shared: &SharedConnection, destination: impl Into<String>) -> SignalsBuilder {
    SignalsBuilder {
        shared: shared.clone(),
        destination: destination.into(),
        path: String::new(),
        iface: String::new(),
        signal: String::new(),
    }
}

/// Why `drain_signal_stream` returned.
enum DrainOutcome {
    /// All `SignalSubscription` handles dropped — task must exit.
    NoSubscribers,
    /// Stream ended or epoch advanced — outer loop should reconnect.
    Reconnect,
}

/// Drain context passed to `drain_signal_stream` to stay under the argument limit.
struct DrainCtx<'a> {
    shared: &'a SharedConnection,
    current_epoch: u64,
    tx: &'a tokio::sync::broadcast::Sender<Arc<SignalEvent>>,
    /// Live-handle tracker — `all_dropped()` means every subscription handle
    /// has been dropped. Also the push-based drop wakeup (`dropped()`).
    tracker: &'a HandleTracker,
    dest: &'a str,
    path: &'a str,
    iface: &'a str,
    signal_name: &'a str,
}

/// Consume one connected signal stream until it ends, the epoch advances,
/// or all subscription handles are dropped.
async fn drain_signal_stream(
    mut stream: zbus::proxy::SignalStream<'_>,
    dc: DrainCtx<'_>,
) -> DrainOutcome {
    use futures_signals::signal::SignalExt;
    let mut epoch_stream = dc.shared.epoch_signal().to_stream();

    loop {
        tokio::select! {
            msg = stream.next() => {
                if let Some(msg) = msg {
                    let event = SignalEvent {
                        body: msg.clone(),
                        sender: msg.header().sender().map(ToString::to_string),
                        timestamp: SystemTime::now(),
                    };
                    let _ = dc.tx.send(Arc::new(event));
                } else {
                    tracing::debug!(dest = dc.dest, path = dc.path, iface = dc.iface,
                        signal_name = dc.signal_name, "signal stream ended; will re-subscribe");
                    return DrainOutcome::Reconnect;
                }
            }
            epoch_update = epoch_stream.next() => {
                if let Some(new_epoch) = epoch_update && new_epoch > dc.current_epoch {
                    tracing::debug!(dest = dc.dest, path = dc.path, iface = dc.iface,
                        signal_name = dc.signal_name, new_epoch, "epoch advanced; re-subscribing");
                    return DrainOutcome::Reconnect;
                }
            }
            // The last subscription handle was dropped — fall through to the
            // all-dropped check below, which exits the task.
            () = dc.tracker.dropped() => {}
        }
        // After each select arm: exit if all subscription handles are gone.
        if dc.tracker.all_dropped() {
            tracing::debug!(
                dest = dc.dest,
                path = dc.path,
                iface = dc.iface,
                signal_name = dc.signal_name,
                "all subscribers dropped; exiting subscription task"
            );
            return DrainOutcome::NoSubscribers;
        }
    }
}

async fn run_subscription(ctx: RunCtx) {
    let RunCtx {
        shared,
        dest,
        path,
        iface,
        signal_name,
        tx,
        tracker,
        task_done_tx,
    } = ctx;

    loop {
        // Exit cleanly if all handles have been dropped (checked at each reconnect boundary).
        if tracker.all_dropped() {
            tracing::debug!(
                dest,
                path,
                iface,
                signal_name,
                "all subscribers dropped; exiting subscription task"
            );
            let _ = task_done_tx.send(());
            return;
        }

        let conn_result = shared.with_conn(|conn| async move { Ok(conn) }).await;
        // Capture epoch AFTER with_conn returns so that current_epoch reflects
        // the epoch under which the subscription was actually built. Capturing
        // it before with_conn would race against the supervisor's first connect
        // (which bumps epoch 0 → 1), causing drain_signal_stream to see an
        // immediate epoch advance and needlessly tear down and rebuild the
        // subscription on cold start before any genuine reconnect has occurred.
        let current_epoch = shared.epoch();
        let conn = match conn_result {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "signal subscription: no connection, will retry");
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
        };

        let stream_result = async {
            let proxy =
                zbus::Proxy::new(&conn, dest.as_str(), path.as_str(), iface.as_str()).await?;
            proxy.receive_signal(signal_name.as_str()).await
        }
        .await;

        let stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, dest, path, iface, signal_name,
                    "signal subscription: receive_signal failed, will retry");
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
        };

        let outcome = drain_signal_stream(
            stream,
            DrainCtx {
                shared: &shared,
                current_epoch,
                tx: &tx,
                tracker: &tracker,
                dest: &dest,
                path: &path,
                iface: &iface,
                signal_name: &signal_name,
            },
        )
        .await;

        if matches!(outcome, DrainOutcome::NoSubscribers) {
            let _ = task_done_tx.send(());
            return;
        }

        // Brief pause before re-subscribing to avoid a tight loop when the bus
        // is cycling rapidly.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{SignalEvent, SignalSubscription, SubInner};
    use futures_util::StreamExt;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};
    use tokio::sync::broadcast;

    fn event(value: u32) -> Arc<SignalEvent> {
        let body = zbus::Message::signal("/t", "t.I", "Ping")
            .expect("signal builder")
            .build(&value)
            .expect("build signal message");
        Arc::new(SignalEvent {
            body,
            sender: None,
            timestamp: SystemTime::now(),
        })
    }

    fn subscription(tx: broadcast::Sender<Arc<SignalEvent>>) -> SignalSubscription {
        let (_done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        SignalSubscription {
            inner: Arc::new(SubInner {
                sender: tx,
                task_done_rx: tokio::sync::Mutex::new(Some(done_rx)),
            }),
            tracker: crate::handle::HandleTracker::new(),
        }
    }

    fn value_of(evt: &SignalEvent) -> u32 {
        evt.body.body().deserialize().expect("decode signal body")
    }

    /// Regression for #428: a broadcast `Lagged(n)` must NOT end the stream.
    /// Before the fix, `events()` used `while let Ok(..)`, so the first
    /// `Lagged` after an overflow terminated the stream and permanently froze
    /// every downstream subscriber.
    #[tokio::test]
    async fn events_survive_broadcast_lag() {
        let (tx, _keep) = broadcast::channel::<Arc<SignalEvent>>(4);
        let sub = subscription(tx.clone());
        let mut events = sub.events();

        // Overflow the capacity-4 channel without consuming: the receiver now
        // lags, so the next internal `recv()` returns `Lagged`.
        for i in 0..8u32 {
            let _ = tx.send(event(i));
        }

        // First delivery: the implementation swallows `Lagged` and yields the
        // oldest still-buffered event instead of terminating.
        let first = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .expect("stream must not end on broadcast lag (#428)")
            .expect("stream yielded None after lag — #428 regression");
        // Oldest events were dropped; the first survivor is one of the newest 4.
        assert!(
            value_of(&first) >= 4,
            "expected a post-lag survivor value, got {}",
            value_of(&first)
        );

        // The stream keeps working: a subsequent send is delivered.
        let _ = tx.send(event(100));
        let mut saw_100 = false;
        for _ in 0..16 {
            match tokio::time::timeout(Duration::from_millis(200), events.next()).await {
                Ok(Some(evt)) => {
                    if value_of(&evt) == 100 {
                        saw_100 = true;
                        break;
                    }
                }
                Ok(None) => panic!("stream ended after lag — #428 regression"),
                Err(_) => break,
            }
        }
        assert!(saw_100, "stream did not deliver the post-lag event");
    }
}
