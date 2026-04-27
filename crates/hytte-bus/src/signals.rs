//! Primitive #2 — subscribe to D-Bus signals on a remote object.
//!
//! See spec section 3.2.

use crate::connection::SharedConnection;
use futures_signals::signal::Mutable;
use futures_util::StreamExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Handle on a live signal subscription. Cloning is cheap (Arc) and does
/// not cancel; dropping the last clone tears down the subscription.
#[derive(Clone)]
pub struct SignalSubscription {
    inner: Arc<SubInner>,
}

struct SubInner {
    sender: tokio::sync::broadcast::Sender<Arc<SignalEvent>>,
    missed: Mutable<u64>,
    missed_counter: Arc<AtomicU64>,
    /// A oneshot receiver that resolves when the internal task exits.
    /// Exposed via `task_done_receiver()` for integration tests.
    task_done_rx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl SignalSubscription {
    /// Stream of signal emissions. Each call to `events()` returns an
    /// independent receiver; backpressure is handled by zbus' broadcast
    /// channel (slow consumers may lag).
    pub fn events(&self) -> impl futures_util::Stream<Item = SignalEvent> + Unpin {
        let mut rx = self.inner.sender.subscribe();
        Box::pin(async_stream::stream! {
            while let Ok(evt) = rx.recv().await {
                yield (*evt).clone();
            }
        })
    }

    /// Counter that bumps every time the bus reconnected and we
    /// re-subscribed — i.e. some signals between disconnect and
    /// re-subscribe were lost. Consumers that need authoritative state
    /// should re-fetch when this signal fires.
    pub fn missed_emissions(&self) -> impl futures_signals::signal::Signal<Item = u64> {
        self.inner.missed.signal_cloned()
    }

    /// Read the current missed-emissions count directly. Useful in tests
    /// that need to poll the counter without setting up a reactive pipeline.
    #[must_use]
    pub fn missed_count(&self) -> u64 {
        self.inner.missed_counter.load(Ordering::Acquire)
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
    /// Weak reference to the subscription's inner state. When all
    /// `SignalSubscription` clones have been dropped, `upgrade()` returns
    /// `None`, which is the definitive signal that no consumer will ever call
    /// `events()` again and the task can exit.
    weak: std::sync::Weak<SubInner>,
    missed: Mutable<u64>,
    missed_counter: Arc<AtomicU64>,
    /// Fired when the task exits so integration tests can verify teardown.
    task_done_tx: tokio::sync::oneshot::Sender<()>,
}

/// Builder.
pub struct SignalsBuilder<'a> {
    shared: &'a SharedConnection,
    destination: String,
    path: String,
    iface: String,
    signal: String,
}

impl SignalsBuilder<'_> {
    /// Override which bus this builder targets. The default is determined by
    /// the constructor: [`signals`](crate::signals) uses the system bus.
    ///
    /// Overriding here replaces the `SharedConnection` with the corresponding
    /// global singleton.
    #[must_use]
    pub fn bus(self, kind: crate::BusKind) -> SignalsBuilder<'static> {
        SignalsBuilder {
            shared: match kind {
                crate::BusKind::Session => crate::connection::session(),
                crate::BusKind::System => crate::connection::system(),
            },
            destination: self.destination,
            path: self.path,
            iface: self.iface,
            signal: self.signal,
        }
    }

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
        let missed = Mutable::new(0u64);
        let missed_counter = Arc::new(AtomicU64::new(0));
        let (task_done_tx, task_done_rx) = tokio::sync::oneshot::channel::<()>();

        let inner = Arc::new(SubInner {
            sender: tx.clone(),
            missed: missed.clone(),
            missed_counter: missed_counter.clone(),
            task_done_rx: tokio::sync::Mutex::new(Some(task_done_rx)),
        });
        let weak = Arc::downgrade(&inner);
        let sub = SignalSubscription { inner };
        let ctx = RunCtx {
            shared: self.shared.clone(),
            dest: self.destination,
            path: self.path,
            iface: self.iface,
            signal_name: self.signal,
            tx,
            weak,
            missed,
            missed_counter,
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
pub fn signals_with(
    shared: &SharedConnection,
    destination: impl Into<String>,
) -> SignalsBuilder<'_> {
    SignalsBuilder {
        shared,
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
    /// Weak handle — `None` strong count means all subscription handles dropped.
    weak: &'a std::sync::Weak<SubInner>,
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
    let mut epoch_stream = dc.shared.epoch_signal().signal_cloned().to_stream();
    // Periodic wakeup so we notice when all subscription handles have been
    // dropped while the task is parked waiting for a signal that never arrives.
    let mut liveness = tokio::time::interval(std::time::Duration::from_millis(100));
    liveness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
            _ = liveness.tick() => {}
        }
        // After each select arm: exit if all subscription handles are gone.
        if dc.weak.upgrade().is_none() {
            tracing::debug!(dest = dc.dest, path = dc.path, iface = dc.iface,
                signal_name = dc.signal_name,
                "all subscribers dropped; exiting subscription task");
            return DrainOutcome::NoSubscribers;
        }
    }
}

async fn run_subscription(ctx: RunCtx) {
    let RunCtx { shared, dest, path, iface, signal_name, tx, weak, missed, missed_counter, task_done_tx } = ctx;

    let mut first_iteration = true;
    loop {
        // Exit cleanly if all handles have been dropped (checked at each reconnect boundary).
        if weak.upgrade().is_none() {
            tracing::debug!(dest, path, iface, signal_name,
                "all subscribers dropped; exiting subscription task");
            let _ = task_done_tx.send(());
            return;
        }

        if !first_iteration {
            let n = missed_counter.fetch_add(1, Ordering::AcqRel) + 1;
            missed.set(n);
        }
        first_iteration = false;

        let current_epoch = shared.epoch();
        let conn_result = shared.with_conn(|conn| async move { Ok(conn) }).await;
        let conn = match conn_result {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "signal subscription: no connection, will retry");
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
        };

        let stream_result = async {
            let proxy = zbus::Proxy::new(&conn, dest.as_str(), path.as_str(), iface.as_str()).await?;
            proxy.receive_signal(signal_name.as_str()).await
        }.await;

        let stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(error = %e, dest, path, iface, signal_name,
                    "signal subscription: receive_signal failed, will retry");
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
        };

        let outcome = drain_signal_stream(stream, DrainCtx {
            shared: &shared,
            current_epoch,
            tx: &tx,
            weak: &weak,
            dest: &dest,
            path: &path,
            iface: &iface,
            signal_name: &signal_name,
        }).await;

        if matches!(outcome, DrainOutcome::NoSubscribers) {
            let _ = task_done_tx.send(());
            return;
        }

        // Brief pause before re-subscribing to avoid a tight loop when the bus
        // is cycling rapidly.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
