#![cfg(feature = "system-tests")]
//! The "subscribed" handshake (#1551): [`SignalSubscription::subscribed`]
//! resolves once the bus has answered the subscription's `AddMatch`, so an
//! emission made after it is delivered.
//!
//! `SignalsBuilder::start` installs the match rule from a spawned task and
//! returns at once. Without something to await, a caller that subscribes and
//! then reads the peer's state can lose whatever the peer emits before the
//! rule lands. These tests hold the handshake to the bus-side property,
//! against a real `dbus-daemon`, rather than to the call order.
//!
//! Nothing here waits by sleeping. Whether an emission was *lost* (as opposed
//! to merely late) is decided by a second emission, the [`SENTINEL`], sent
//! once the rule is known to be installed: the bus delivers one sender's
//! signals in order, so the first value the stream yields says whether the
//! one before the sentinel got through. The timeouts are liveness guards, so
//! a failure is a message instead of a hung binary.
//!
//! [`SignalSubscription::subscribed`]: hytte_bus::SignalSubscription::subscribed

mod common;

use common::{ephemeral_bus, restart_on_same_address};
use futures_util::{FutureExt, Stream, StreamExt};
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{SignalItem, SignalSubscription, signals_with};
use std::time::Duration;

const PEER: &str = "mov.vibec0re.test.Emitter";
const PATH: &str = "/mov/vibec0re/test/Emitter";
const IFACE: &str = "mov.vibec0re.test.Emitter";

/// Sent after an emission under test, once the rule is installed. Reading it
/// first means the emission before it never arrived.
const SENTINEL: u32 = u32::MAX;

/// How many fresh subscriptions each looped test makes. Every one installs a
/// rule of its own (a distinct member name), so every one is a real `AddMatch`
/// round trip and a real chance to lose the race.
const ROUNDS: u32 = 200;

/// Liveness guard for one await. Not a latency assertion: the suite runs
/// beside whole-workspace compiles in CI.
const BUDGET: Duration = Duration::from_secs(30);

/// A peer that owns [`PEER`] on the bus at `address`.
async fn peer(address: &str) -> zbus::Connection {
    zbus::connection::Builder::address(address)
        .expect("parse bus address")
        .name(PEER)
        .expect("request the peer's well-known name")
        .build()
        .await
        .expect("build the peer connection")
}

/// Emit `member(value)` from `peer`, as a broadcast.
async fn emit(peer: &zbus::Connection, member: &str, value: u32) {
    peer.emit_signal(None::<&str>, PATH, IFACE, member, &value)
        .await
        .expect("emit signal");
}

fn subscribe(shared: &SharedConnection, member: &str) -> SignalSubscription {
    signals_with(shared, PEER)
        .at_path(PATH)
        .iface(IFACE)
        .signal(member)
        .start()
}

async fn within<F: Future>(what: &str, fut: F) -> F::Output {
    tokio::time::timeout(BUDGET, fut)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// The next item on `items`.
async fn next_item<S>(items: &mut S, what: &str) -> SignalItem
where
    S: Stream<Item = SignalItem> + Unpin,
{
    within(what, items.next())
        .await
        .unwrap_or_else(|| panic!("the stream ended while waiting for {what}"))
}

/// The value of the next emission on `items`. A marker is a failure: no test
/// here expects one where it calls this.
async fn next_value<S>(items: &mut S, what: &str) -> u32
where
    S: Stream<Item = SignalItem> + Unpin,
{
    match next_item(items, what).await {
        SignalItem::Event(evt) => evt.body.body().deserialize().expect("decode the value"),
        other => panic!("expected an emission while waiting for {what}, got {other:?}"),
    }
}

/// **The property.** Take a receiver, await `subscribed()`, and an emission
/// made right after is delivered: every time, with no retry and no delay.
///
/// **Falsification:** make the subscription task report subscribed before
/// `receive_signal` has returned (move `live.send_replace(true)` above the
/// `with_conn` in `run_subscription`) and rounds lose their emission: the
/// first value read is the sentinel, or nothing arrives at all.
/// [`without_the_handshake_an_emission_right_after_start_is_lost`] is the
/// same loop minus the await, kept in the tree as the standing proof that
/// this loop can fail.
#[tokio::test(flavor = "multi_thread")]
async fn an_emission_after_subscribed_is_always_delivered() {
    let (conn, guard) = ephemeral_bus().await;
    let peer = peer(&guard.address).await;
    let shared = SharedConnection::for_test_session(conn);

    for round in 0..ROUNDS {
        let member = format!("Ping{round}");
        let sub = subscribe(&shared, &member);
        let mut items = sub.items();

        within("subscribed()", sub.subscribed()).await;
        emit(&peer, &member, round).await;
        emit(&peer, &member, SENTINEL).await;

        let first = next_value(&mut items, "the emission made after subscribed()").await;
        assert_eq!(
            first, round,
            "round {round}: the emission made right after subscribed() resolved was lost \
             (the sentinel arrived first), so subscribed() resolved before the bus had the \
             match rule"
        );
    }
}

/// **The control**, and the gap #1551 names. The same loop without the
/// handshake: emit right after `start()`, then await `subscribed()` and send
/// the sentinel. The rule is installed by a task that has several bus round
/// trips ahead of it, so the emission is routed before the rule exists and
/// the sentinel is what arrives.
///
/// It asserts that *some* round loses its emission, not that every round
/// does, because nothing orders the task against this thread. In practice
/// every round loses it.
///
/// This is what gives [`an_emission_after_subscribed_is_always_delivered`]
/// its detection power: the only difference between the two loops is where
/// the await sits. It also keeps the sentinel honest. If this ever reports
/// zero lost, either `start()` has become synchronous or the sentinel no
/// longer tells a lost emission from a delivered one.
#[tokio::test(flavor = "multi_thread")]
async fn without_the_handshake_an_emission_right_after_start_is_lost() {
    let (conn, guard) = ephemeral_bus().await;
    let peer = peer(&guard.address).await;
    let shared = SharedConnection::for_test_session(conn);

    let mut lost = 0u32;
    for round in 0..ROUNDS {
        let member = format!("Ping{round}");
        let sub = subscribe(&shared, &member);
        let mut items = sub.items();

        emit(&peer, &member, round).await;
        within("subscribed()", sub.subscribed()).await;
        emit(&peer, &member, SENTINEL).await;

        if next_value(&mut items, "the first value of a round").await == SENTINEL {
            lost += 1;
        }
    }

    assert!(
        lost > 0,
        "no emission made right after start() was lost in {ROUNDS} rounds: this control no \
         longer shows the race the handshake closes"
    );
}

/// The rule is installed against the *name*, so a destination nobody owns
/// does not hold the handshake up, and what the name's first owner emits is
/// delivered.
///
/// The iwd watcher depends on this: on its first discovery iwd may be an
/// activatable name with no owner, and the call that starts it comes after
/// the handshake. A `subscribed()` that waited for an owner would deadlock
/// that start.
#[tokio::test(flavor = "multi_thread")]
async fn subscribed_does_not_wait_for_the_destination_to_have_an_owner() {
    let (conn, guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);

    let sub = subscribe(&shared, "Ping");
    let mut items = sub.items();
    within("subscribed() with no owner", sub.subscribed()).await;

    let peer = peer(&guard.address).await;
    emit(&peer, "Ping", 7).await;
    assert_eq!(
        next_value(&mut items, "the first owner's emission").await,
        7
    );
}

/// "Subscribed" is about the current match rule, not a latch: once the task
/// has seen its bus die, `subscribed()` is pending until the replacement
/// rule is installed, and an emission made after it resolves again is
/// delivered, behind the `Resubscribed` marker for the rebuild.
///
/// No supervisor runs here, on purpose. With nothing to reconnect it, the
/// subscription stays down for as long as the test holds it there, so "the
/// task knows it has no rule" is a state the test can stand in rather than a
/// window it has to hit. The replacement connection is installed by hand
/// once the pending assertion is made.
///
/// **Falsification:** delete the `live.send_replace(false)` after the drain
/// in `run_subscription` (never re-arm) and the wait for the task to notice
/// times out. Were that wait removed too, `subscribed()` would answer for the
/// dead rule and the emission below would be routed before the new one.
#[tokio::test(flavor = "multi_thread")]
async fn after_the_bus_dies_subscribed_waits_for_the_replacement_rule() {
    let (conn_a, guard_a) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn_a);

    let sub = subscribe(&shared, "Ping");
    let mut items = sub.items();
    within("the first subscribed()", sub.subscribed()).await;

    // Kill the daemon and boot a fresh one on the same address. The pooled
    // connection is dead and nothing replaces it yet.
    let (conn_b, guard_b) = restart_on_same_address(guard_a).await;
    within(
        "the subscription task to notice its bus died",
        sub.unsubscribed_for_test(),
    )
    .await;
    assert!(
        sub.subscribed().now_or_never().is_none(),
        "subscribed() must be pending while the subscription has no match rule"
    );

    let peer = peer(&guard_b.address).await;
    shared.install_fresh_connection_for_test(conn_b).await;
    within("subscribed() on the new bus", sub.subscribed()).await;
    emit(&peer, "Ping", 7).await;

    // The marker for the rebuild is queued before the rebuild counts as
    // subscribed, so it is what a receiver taken earlier reads first.
    assert!(
        matches!(
            next_item(&mut items, "the Resubscribed marker").await,
            SignalItem::Resubscribed
        ),
        "the rebuild's marker must come before anything the new rule delivers"
    );
    assert_eq!(
        next_value(
            &mut items,
            "the emission made after the second subscribed()"
        )
        .await,
        7,
        "an emission made after subscribed() resolved on the new bus was lost"
    );
}
