//! Primitive #2 — subscribe to D-Bus signals on a remote object.
//!
//! See spec section 3.2.

use crate::backoff::FailureStreak;
use crate::connection::SharedConnection;
use crate::handle::HandleTracker;
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::SystemTime;

/// A single signal emission delivered to the consumer.
#[derive(Clone, Debug)]
pub struct SignalEvent {
    /// The raw zbus message; consumer calls `.body().deserialize::<T>()`
    /// to decode arguments.
    pub body: zbus::Message,
    /// Sender's unique name, if known.
    pub sender: Option<String>,
    /// Local clock when the event was received.
    pub timestamp: SystemTime,
}

/// One item on a subscription's stream: an emission, or the notice that the
/// subscription behind it was rebuilt.
///
/// **Why an in-stream marker rather than a side channel** (#1173, closing the
/// half of #433 that was closed by *deleting* `missed_emissions` instead of
/// wiring it). Between a subscription dying and its replacement going up, every
/// emission the peer made is gone: the broker had no match rule to route it
/// through, and D-Bus replays nothing. A consumer whose state is a fold over
/// those emissions — `upower`, `mpris`, `bluetooth`, `networkd_nm`, … — is
/// silently wrong from that point until the peer happens to emit again, which
/// for a `PropertiesChanged` fold can be never. The one correct reaction is to
/// re-read the state the fold was tracking, and the consumer is the only party
/// that knows what that is.
///
/// A marker on the *same broadcast channel as the events* is totally ordered
/// with them: everything a consumer reads after it came from the new
/// `AddMatch`, and everything before it from the old one. A separate `Signal`
/// or counter would deliver the same fact out of band, so a consumer could
/// re-`Get` and then apply an event it had already folded in, or fold a new
/// event before learning that its history had a hole — the ordering would have
/// to be re-established by the consumer, once per consumer.
///
/// It is ignorable by default: [`SignalSubscription::events`] keeps its
/// `SignalEvent` item type and drops markers, so every existing consumer
/// compiles and behaves exactly as before. [`SignalSubscription::items`] is the
/// opt-in.
///
/// Two variants carry a hole in a consumer's history — [`Self::Resubscribed`]
/// (the *sender* side: the subscription itself was rebuilt) and
/// [`Self::Lagged`] (the *receiver* side: this particular consumer fell behind
/// the broadcast channel and the runtime dropped buffered items to catch it
/// up). They are kept distinct rather than folded into one variant so a
/// journal can still tell "the subscription was rebuilt" apart from "this
/// consumer specifically fell behind" — but every consumer that reacts to
/// [`Self::Resubscribed`] must react to [`Self::Lagged`] the same way: both
/// mean re-read whatever state this subscription feeds.
#[derive(Clone, Debug)]
pub enum SignalItem {
    /// One signal emission from the peer.
    Event(SignalEvent),
    /// The subscription was torn down and rebuilt — bus reconnect, broker
    /// restart, or the stream simply ending. Emissions made while it was down
    /// were never delivered and never will be; re-read whatever state this
    /// subscription feeds.
    ///
    /// Emitted only for a *re*-subscription: the first one has no history to
    /// have missed.
    ///
    /// This is one of two variants that report a hole in a consumer's
    /// history — see [`Self::Lagged`] for the other, and react to both the
    /// same way.
    Resubscribed,
    /// This receiver fell behind the broadcast channel and the runtime
    /// dropped `skipped` buffered items before it could see them — same
    /// underlying condition as [`Self::Resubscribed`] (emissions that will
    /// never be delivered), just discovered on the receiving end instead of
    /// the sending end. React to it the same way: re-read whatever state this
    /// subscription feeds.
    ///
    /// Before this variant existed, a lag silently swallowed any
    /// `Resubscribed` marker it overran (the marker is just another item in
    /// the same buffer), so a consumer folding emissions into state carried a
    /// stale fold with no notice at all.
    Lagged {
        /// Number of buffered items the broadcast channel dropped before this
        /// receiver could see them. Mirrors
        /// `tokio::sync::broadcast::error::RecvError::Lagged`'s count.
        skipped: u64,
    },
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
    sender: tokio::sync::broadcast::Sender<Arc<SignalItem>>,
    /// A oneshot receiver that resolves when the internal task exits.
    /// Exposed via `task_done_receiver()` for integration tests.
    task_done_rx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl SignalSubscription {
    /// Stream of signal emissions. Each call to `events()` returns an
    /// independent receiver; backpressure is handled by zbus' broadcast
    /// channel (slow consumers may lag).
    ///
    /// [`SignalItem::Resubscribed`] and [`SignalItem::Lagged`] markers are
    /// dropped here — this is the ignore-by-default half of #1173. Use
    /// [`Self::items`] to see them.
    ///
    /// `+ use<>` for the reason `OwnNameSignal::signal_cloned` carries it
    /// (#750): without it the opaque type captures `&self` under Rust 2024's
    /// rules and is never `'static`, so it could not be spawned onto a task or
    /// stored. The stream owns its `broadcast::Receiver` and borrows nothing.
    pub fn events(&self) -> impl futures_util::Stream<Item = SignalEvent> + Unpin + use<> {
        let mut items = self.items();
        Box::pin(async_stream::stream! {
            while let Some(item) = items.next().await {
                if let SignalItem::Event(evt) = item {
                    yield evt;
                }
            }
        })
    }

    /// Stream of emissions **and** [`SignalItem::Resubscribed`] /
    /// [`SignalItem::Lagged`] markers, in the order the subscription task
    /// produced them (for `Lagged`, the order this receiver observed them —
    /// see below). Each call returns an independent receiver.
    ///
    /// Reach for this when the consumer folds emissions into state that goes
    /// stale if any are missed — see [`SignalItem`] for the argument. A
    /// consumer that just reacts to each emission in isolation wants
    /// [`Self::events`].
    pub fn items(&self) -> impl futures_util::Stream<Item = SignalItem> + Unpin + use<> {
        use tokio::sync::broadcast::error::RecvError;
        let mut rx = self.inner.sender.subscribe();
        Box::pin(async_stream::stream! {
            loop {
                match rx.recv().await {
                    Ok(item) => yield (*item).clone(),
                    // A burst overflowed the broadcast channel and this
                    // consumer fell behind. The receiver is still usable — the
                    // next `recv()` yields the oldest event still buffered —
                    // so warn and keep going (the old `while let Ok` treated
                    // this as end-of-stream and permanently froze every
                    // downstream subscriber after an event burst, #428).
                    //
                    // A lag can overrun a buffered `Resubscribed` marker the
                    // same way it overruns an event — the marker is just
                    // another item in the same channel — so without a
                    // `Lagged` item of its own, a fold-over-emissions consumer
                    // would carry a stale fold with no notice at all. Yield
                    // one so `events()` can keep filtering by variant
                    // (unaffected: it only ever extracts `Event`) while
                    // `items()` reports the hole either way it opened.
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(
                            missed = n,
                            "signal event consumer lagged behind the broadcast \
                             channel; dropped buffered events and continuing \
                             (only channel close ends the stream)"
                        );
                        yield SignalItem::Lagged { skipped: n };
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
    tx: tokio::sync::broadcast::Sender<Arc<SignalItem>>,
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
    tx: &'a tokio::sync::broadcast::Sender<Arc<SignalItem>>,
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
    mut stream: zbus::proxy::SignalStream<'static>,
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
                    let _ = dc.tx.send(Arc::new(SignalItem::Event(event)));
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

    // The crate's retry ramp, owned across the outer loop's iterations so a bus
    // that will not answer actually backs off instead of resetting to 250 ms
    // every time round. Cleared by a successful subscribe.
    let mut failures = FailureStreak::default();
    // Whether a subscription has ever been established, so the `Resubscribed`
    // marker is emitted for the re-subscriptions only.
    let mut subscribed_before = false;

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

        // Build the proxy and issue the `AddMatch` **inside** `with_conn`
        // (#1173), rather than pulling the connection out through an
        // infallible `Ok(conn)` and subscribing on it afterwards.
        //
        // `Connection::add_match` answers `InputOutput(BrokenPipe)` the moment
        // its socket reader task has died, and that is precisely the error
        // `with_conn`'s tail is watching for: it clears the cached connection
        // and wakes the supervisor. Subscribing outside the closure — as this
        // did until #1173 — turned a dead connection into a silent retry
        // forever, because nothing ever invalidated the cache. `own.rs` and
        // `proxy.rs` always subscribed inside the closure; this is the same
        // shape.
        //
        // `new_owned` (not `new`) because the resulting stream outlives the
        // closure: it takes the `Connection` by value and owned name strings,
        // so the `Proxy` is `'static` and nothing borrows a local. The
        // `signal_name` goes in as an owned `String` for the same reason —
        // `receive_signal`'s lifetime parameter comes from the member name, not
        // from `&self`.
        //
        // Capture epoch AFTER with_conn returns so that current_epoch reflects
        // the epoch under which the subscription was actually built. Capturing
        // it before with_conn would race against the supervisor's first connect
        // (which bumps epoch 0 → 1), causing drain_signal_stream to see an
        // immediate epoch advance and needlessly tear down and rebuild the
        // subscription on cold start before any genuine reconnect has occurred.
        let stream_result: Result<zbus::proxy::SignalStream<'static>, crate::BusError> = shared
            .with_conn(|conn| {
                let dest = dest.clone();
                let path = path.clone();
                let iface = iface.clone();
                let signal_name = signal_name.clone();
                async move {
                    let proxy = zbus::Proxy::new_owned(conn, dest, path, iface).await?;
                    proxy.receive_signal(signal_name).await
                }
            })
            .await;
        let current_epoch = shared.epoch();

        let stream = match stream_result {
            Ok(s) => {
                failures.reset();
                // Tell consumers the subscription behind them is a new one, so
                // a fold over emissions can re-read the state it was tracking
                // (#1173). Deliberately *after* the subscribe succeeded and
                // *before* any event from it is pushed, so the marker and the
                // events it precedes are one ordered sequence on one channel.
                // Never on the first subscription — there is no history to have
                // missed. `send` failing means nobody is listening, which is
                // fine.
                if subscribed_before {
                    let _ = tx.send(Arc::new(SignalItem::Resubscribed));
                }
                subscribed_before = true;
                s
            }
            Err(e) => {
                crate::backoff::back_off_resubscribe(
                    &mut failures,
                    "signals: receive_signal",
                    &dest,
                    &e,
                )
                .await;
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
    use super::{SignalEvent, SignalItem, SignalSubscription, SubInner};
    use futures_util::StreamExt;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};
    use tokio::sync::broadcast;

    fn event(value: u32) -> Arc<SignalItem> {
        let body = zbus::Message::signal("/t", "t.I", "Ping")
            .expect("signal builder")
            .build(&value)
            .expect("build signal message");
        Arc::new(SignalItem::Event(SignalEvent {
            body,
            sender: None,
            timestamp: SystemTime::now(),
        }))
    }

    fn subscription(tx: broadcast::Sender<Arc<SignalItem>>) -> SignalSubscription {
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
        let (tx, _keep) = broadcast::channel::<Arc<SignalItem>>(4);
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

    /// The other end of the #428 arm, read from the opposite direction: a
    /// `Lagged` must not silently swallow a buffered `Resubscribed` marker.
    ///
    /// Capacity 2, one marker then ten events with nothing draining in
    /// between: the receiver falls behind before it ever polls, so its first
    /// `recv()` answers `Lagged` having skipped the marker along with most of
    /// the events. Before the fix, `items()` only warned and kept going —
    /// `markers` came back 0 here, which is exactly the "the consumer lagged
    /// past the `Resubscribed` marker and was never told to re-read" hole
    /// `mpris::watch_properties` depends on `items()` to report.
    #[tokio::test]
    async fn lagged_yields_a_re_read_item() {
        let (tx, _keep) = broadcast::channel::<Arc<SignalItem>>(2);
        let sub = subscription(tx.clone());
        let mut items = sub.items();

        let _ = tx.send(Arc::new(SignalItem::Resubscribed));
        for i in 0..10u32 {
            let _ = tx.send(event(i));
        }

        let mut events_seen = 0u32;
        let mut markers = 0u32;
        loop {
            match tokio::time::timeout(Duration::from_millis(50), items.next()).await {
                Ok(Some(SignalItem::Event(_))) => events_seen += 1,
                Ok(Some(SignalItem::Resubscribed | SignalItem::Lagged { .. })) => markers += 1,
                // `Ok(None)`: the sender was dropped. `Err(_)`: nothing more
                // within the timeout, i.e. the channel is drained. Either way
                // there is nothing left to read.
                Ok(None) | Err(_) => break,
            }
        }

        assert!(events_seen > 0, "expected at least the buffered survivors");
        assert_eq!(
            markers, 1,
            "the consumer lagged past the Resubscribed marker and was never \
             told to re-read (#1197 review)"
        );
    }
}
