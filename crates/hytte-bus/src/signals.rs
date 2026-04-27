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
}

/// Context passed to `run_subscription` to avoid too-many-arguments lint.
struct RunCtx {
    shared: SharedConnection,
    dest: String,
    path: String,
    iface: String,
    signal_name: String,
    tx: tokio::sync::broadcast::Sender<Arc<SignalEvent>>,
    missed: Mutable<u64>,
    missed_counter: Arc<AtomicU64>,
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
    pub fn start(self) -> SignalSubscription {
        let (tx, _) = tokio::sync::broadcast::channel(64);
        let missed = Mutable::new(0u64);
        let missed_counter = Arc::new(AtomicU64::new(0));
        let sub = SignalSubscription {
            inner: Arc::new(SubInner {
                sender: tx.clone(),
                missed: missed.clone(),
                missed_counter: missed_counter.clone(),
            }),
        };
        let ctx = RunCtx {
            shared: self.shared.clone(),
            dest: self.destination,
            path: self.path,
            iface: self.iface,
            signal_name: self.signal,
            tx,
            missed,
            missed_counter,
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

async fn run_subscription(ctx: RunCtx) {
    use futures_signals::signal::SignalExt;

    let RunCtx {
        shared,
        dest,
        path,
        iface,
        signal_name,
        tx,
        missed,
        missed_counter,
    } = ctx;

    let mut first_iteration = true;
    loop {
        if !first_iteration {
            let n = missed_counter.fetch_add(1, Ordering::AcqRel) + 1;
            missed.set(n);
        }
        first_iteration = false;

        // Snapshot the current epoch and grab the connection. We use `with_conn`
        // only to obtain the connection handle — the actual stream runs outside
        // of the closure so that epoch changes (reconnects) can abort it.
        let current_epoch = shared.epoch();
        let conn_result = shared
            .with_conn(|conn| async move { Ok(conn) })
            .await;

        let conn = match conn_result {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "signal subscription: no connection, will retry"
                );
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
        };

        // Build the proxy and subscribe to the signal.
        let stream_result = async {
            let proxy = zbus::Proxy::new(&conn, dest.as_str(), path.as_str(), iface.as_str()).await?;
            proxy.receive_signal(signal_name.as_str()).await
        }
        .await;

        let mut stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    dest,
                    path,
                    iface,
                    signal_name,
                    "signal subscription: receive_signal failed, will retry"
                );
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
        };

        // Watch for epoch changes so we can abort when the bus reconnects.
        let mut epoch_stream = shared.epoch_signal().signal_cloned().to_stream();

        // Consume signal emissions until either the stream ends or the epoch advances.
        loop {
            tokio::select! {
                msg = stream.next() => {
                    if let Some(msg) = msg {
                        let event = SignalEvent {
                            body: msg.clone(),
                            sender: msg.header().sender().map(ToString::to_string),
                            timestamp: SystemTime::now(),
                        };
                        let _ = tx.send(Arc::new(event));
                    } else {
                        // Stream ended (connection dropped or signal interface disappeared).
                        tracing::debug!(
                            dest,
                            path,
                            iface,
                            signal_name,
                            "signal stream ended; will re-subscribe"
                        );
                        break;
                    }
                }
                epoch_update = epoch_stream.next() => {
                    if let Some(new_epoch) = epoch_update
                        && new_epoch > current_epoch
                    {
                        // The bus reconnected — re-subscribe on the new connection.
                        tracing::debug!(
                            dest,
                            path,
                            iface,
                            signal_name,
                            new_epoch,
                            "epoch advanced; re-subscribing"
                        );
                        break;
                    }
                }
            }
        }
        // Brief pause before re-subscribing to avoid a tight loop when the bus
        // is cycling rapidly.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
