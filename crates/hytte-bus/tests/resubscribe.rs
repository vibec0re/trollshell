#![cfg(feature = "system-tests")]
//! Reconnect **detection**, and what a consumer can see of it (#1173).
//!
//! Every other reconnect test in this crate drives
//! `simulate_disconnect_for_test`, which clears the cached connection and wakes
//! the supervisor *itself*. That makes it useless for testing detection: it
//! performs by hand exactly the step the code under test is supposed to
//! perform, so deleting `with_conn`'s invalidate-and-notify tail leaves the
//! whole suite green. Nothing in this file calls it (grep: zero hits). Each
//! test here kills a real `dbus-daemon` and requires a primitive to notice on
//! its own.

use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{PropState, ProxyState, SignalItem, property_with, proxy_with, signals_with};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

mod common;
use common::{ephemeral_bus, restart_on_same_address};

// ── A trivial peer to subscribe to ───────────────────────────────────────────

const COUNTER_NAME: &str = "mov.vibec0re.test.Counter";
const COUNTER_PATH: &str = "/mov/vibec0re/test/Counter";

struct Counter {
    value: u32,
}

#[zbus::interface(name = "mov.vibec0re.test.Counter")]
impl Counter {
    #[zbus(property)]
    fn value(&self) -> u32 {
        self.value
    }
}

/// Serve [`Counter`] at [`COUNTER_PATH`] on `address`. Keep the returned
/// connection alive for as long as the interface must stay reachable.
async fn serve_counter(address: &str, value: u32) -> zbus::Connection {
    zbus::connection::Builder::address(address)
        .expect("parse bus address")
        .name(COUNTER_NAME)
        .expect("request Counter well-known name")
        .serve_at(COUNTER_PATH, Counter { value })
        .expect("serve Counter interface")
        .build()
        .await
        .expect("build Counter server connection")
}

fn counter_property(shared: &SharedConnection) -> hytte_bus::PropertySignal<u32> {
    property_with::<u32>(shared, COUNTER_NAME)
        .at_path(COUNTER_PATH)
        .iface(COUNTER_NAME)
        .name("Value")
        .start()
}

/// Poll `stream` until `want` matches, or fail with `what`.
///
/// Deadline-polled with a generous budget rather than a single `timeout`: these
/// tests restart a subprocess and wait on a supervisor, under a CI run that is
/// also building two nixosTest VMs. The budget is a liveness guard, not a
/// latency assertion — same reasoning as `common/mod.rs`'s startup budget
/// (#676/#678).
async fn wait_for<S, F>(stream: &mut S, budget: Duration, what: &str, mut want: F) -> S::Item
where
    S: futures_util::Stream + Unpin,
    F: FnMut(&S::Item) -> bool,
{
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(50), stream.next()).await
            && want(&item)
        {
            return item;
        }
    }
    panic!("timed out waiting for {what}");
}

// ── Item 1: a lone property subscription is enough to drive detection ────────

/// **The headline.** A `property()` subscription, and nothing else, must notice
/// that the bus died and come back when it returns.
///
/// This is the test #1189's design note says cannot pass today, and why: until
/// #1173 `subscribe_properties_changed` pulled the connection out of
/// `with_conn` through an infallible `Ok(conn)` and issued the real `AddMatch`
/// *outside* the closure, so a dead connection produced a retry that failed
/// forever without ever reaching `with_conn`'s error arm. Nothing invalidated
/// the cached connection, the supervisor was never woken, and recovery was
/// parasitic on some *other* primitive on the same bus happening to make a
/// fallible call — which a process whose only use of a bus is `property()`
/// never makes.
///
/// So: no `call()` probe (grep this file), no `simulate_disconnect_for_test`.
/// The only thing this test does by hand is *arm* the replacement connection
/// the supervisor would otherwise open by reading `$DBUS_SESSION_BUS_ADDRESS`
/// — a variable no test here can safely repoint, since mutating it needs
/// `unsafe`, forbidden workspace-wide. Clearing the dead connection and waking
/// the supervisor are left entirely to the code under test.
///
/// The restarted daemon serves a **different** value, so the recovery
/// assertion cannot be satisfied by a replayed `Loaded(42)`: reaching
/// `Loaded(7)` requires a fresh `Get` on a fresh connection.
#[tokio::test(flavor = "multi_thread")]
async fn a_lone_property_subscription_detects_a_dead_daemon_and_recovers() {
    let (conn_a, guard_a) = ephemeral_bus().await;
    let address = guard_a.address.clone();
    let _server_a = serve_counter(&address, 42).await;

    let shared = SharedConnection::for_test_session(conn_a);
    shared.spawn_supervisor_for_test();

    let prop = counter_property(&shared);
    let mut stream = prop.signal().to_stream();
    wait_for(
        &mut stream,
        Duration::from_secs(20),
        "the initial Loaded(42)",
        |s| matches!(s, PropState::Loaded(42)),
    )
    .await;

    // Kill daemon A and boot a fresh one on the identical socket path. The
    // cached connection now points at a dead peer and nothing has said so.
    let (conn_b, _guard_b) = restart_on_same_address(guard_a).await;
    let _server_b = serve_counter(&address, 7).await;

    // Arm recovery — and only recovery.
    shared.arm_reconnect_for_test(conn_b);

    wait_for(
        &mut stream,
        Duration::from_mins(1),
        "Loaded(7) after a real detect-and-reconnect",
        |s| matches!(s, PropState::Loaded(7)),
    )
    .await;

    assert!(
        shared.epoch() > 1,
        "the supervisor never installed the armed connection, so nothing \
         detected the dead one"
    );
}

// ── Item 2: Stale is an edge, not a heartbeat ────────────────────────────────

/// A property whose re-subscribe keeps failing must publish `Stale` **once**,
/// not once per retry.
///
/// `Mutable::set` notifies unconditionally — `set_if_changed` needs
/// `T: PartialEq`, which `PropState<T>`'s parameter does not carry — so each
/// re-mark was a real wakeup for every `bind`ing on the signal: a GTK
/// apply-loop running several times a second for the whole length of an
/// outage, re-applying a value that never changed. Before #1173 the outer loop
/// re-ran the marking block on every failed attempt; the comment above it had
/// claimed "exactly once per (re)connect cycle" since the day it was written.
///
/// The daemon is killed and never restarted, so the re-subscribe fails for the
/// rest of the test. Counting through the signal is sound here because the
/// pre-fix cadence (one `set` per retry) is far slower than a dedicated tokio
/// task polling the stream, so no emission is lost to
/// `MutableSignalCloned`'s latest-value coalescing; and coalescing can only
/// ever *reduce* a count, never inflate it past 1.
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_resubscribe_marks_stale_exactly_once() {
    /// Long enough that the retired flat 250 ms spin would publish ~20 `Stale`s
    /// inside it, and that the ramp has gone round several times.
    const WINDOW: Duration = Duration::from_secs(5);

    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();
    let _server = serve_counter(&address, 42).await;

    // No supervisor and no armed replacement: once the daemon is gone, every
    // re-subscribe fails for the rest of the test.
    let shared = SharedConnection::for_test_session(conn);

    let prop = counter_property(&shared);
    let mut warmup = prop.signal().to_stream();
    wait_for(
        &mut warmup,
        Duration::from_secs(20),
        "the initial Loaded(42)",
        |s| matches!(s, PropState::Loaded(42)),
    )
    .await;
    drop(warmup);

    // Count from a fresh subscriber: its first item is the current value
    // (`Loaded(42)`), and everything after it is a real transition.
    let stales = Arc::new(AtomicUsize::new(0));
    let counter = stales.clone();
    let mut counting = prop.signal().to_stream();
    let counting_task = tokio::spawn(async move {
        while let Some(state) = counting.next().await {
            if matches!(state, PropState::Stale(_)) {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    // Kill the daemon for good.
    drop(guard);

    tokio::time::sleep(WINDOW).await;
    counting_task.abort();

    assert_eq!(
        stales.load(Ordering::Relaxed),
        1,
        "a failing re-subscribe must publish Stale on the edge only; \
         {WINDOW:?} of retries produced this many emissions"
    );
}

// ── Item 3: Live is a claim about the cache, so only a rebuild may make it ───

/// A proxy whose rebuild fails must not announce `Live`.
///
/// `handle_noc_msg`'s peer-back arm discarded the rebuild's `Result` and set
/// `Live` regardless, so `liveness()` said "connected" while every
/// `BusProxy::call` returned `Transient` from the empty cache — the state is a
/// claim about the cache being populated, and nothing checked it.
///
/// Arranging a failing rebuild needs the connection the *watcher* reads from
/// to be gone while the connection its `NameOwnerChanged` stream rides stays
/// up. Those are separable: the stream holds its own handle on the
/// `zbus::Connection`, while `with_conn` reads `SharedConnection`'s cached one.
/// So the test drops the cached connection (`drop_connection_for_test`, no
/// supervisor to put it back) and then re-owns the peer's name, which delivers
/// a real peer-back NOC on the still-live stream.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_proxy_rebuild_does_not_announce_live() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();
    let server = serve_counter(&address, 42).await;

    // Keep the underlying zbus connection alive independently of
    // `SharedConnection`'s cache, so dropping the cache cannot take the
    // watcher's NOC stream down with it.
    let _keepalive = conn.clone();
    let shared = SharedConnection::for_test_session(conn);

    let proxy = proxy_with(&shared, COUNTER_NAME)
        .at_path(COUNTER_PATH)
        .iface(COUNTER_NAME)
        .build()
        .await
        .expect("build proxy against a live peer");

    let mut liveness = proxy.liveness().to_stream();
    wait_for(
        &mut liveness,
        Duration::from_secs(20),
        "the proxy to go Live",
        |s| matches!(s, ProxyState::Live),
    )
    .await;

    // The peer quits: NOC with an empty new owner, cache cleared, PeerGone.
    drop(server);
    wait_for(
        &mut liveness,
        Duration::from_secs(20),
        "PeerGone after the peer quit",
        |s| matches!(s, ProxyState::PeerGone),
    )
    .await;

    // Now take the shared connection away. No supervisor is running, so it
    // stays away and every rebuild from here on fails.
    shared.drop_connection_for_test().await;

    // The peer comes back — a real NOC on the watcher's still-live stream.
    let _server_again = serve_counter(&address, 42).await;

    let observed = wait_for(
        &mut liveness,
        Duration::from_secs(20),
        "the watcher to react to the peer coming back",
        |s| matches!(s, ProxyState::Live | ProxyState::Reconnecting),
    )
    .await;
    assert_eq!(
        observed,
        ProxyState::Reconnecting,
        "the peer came back but the cached proxy could not be rebuilt, so the \
         watcher must not claim Live — every call would return Transient"
    );
    assert!(
        proxy
            .call::<_, u32>("Whatever", ())
            .await
            .is_err_and(|e| e.is_transient()),
        "a proxy that is not Live must fail its calls transiently"
    );
}

// ── Item 4: the consumer can see that the subscription was rebuilt ───────────

/// `signals()` must tell a consumer when it re-subscribed.
///
/// Between a subscription dying and its replacement going up, every emission
/// the peer made is gone — the broker had no match rule to route it through,
/// and there is no replay. A consumer whose state is a fold over those
/// emissions (`upower`, `mpris`, `bluetooth`, …) is silently wrong from that
/// point on, and #433 item 2 was closed by *deleting* the `missed_emissions`
/// counter rather than by wiring anything to it.
///
/// The marker rides the same broadcast channel as the events, so it is totally
/// ordered with them: everything a consumer reads after it came from the new
/// `AddMatch`. `events()` keeps its item type and drops markers, so every
/// existing consumer compiles and behaves exactly as before; `items()` is the
/// opt-in that surfaces them.
#[tokio::test(flavor = "multi_thread")]
async fn a_resubscribe_reaches_the_consumer_as_a_marker() {
    let (conn_a, guard_a) = ephemeral_bus().await;
    let address = guard_a.address.clone();
    let _server_a = serve_counter(&address, 42).await;

    let shared = SharedConnection::for_test_session(conn_a);
    shared.spawn_supervisor_for_test();

    let sub = signals_with(&shared, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .signal("NameOwnerChanged")
        .start();
    let mut items = sub.items();

    // Give the first subscription a moment to be established: a marker is only
    // emitted for a *re*-subscribe, so a race here would make the assertion
    // below vacuous.
    let seen_before = Arc::new(AtomicUsize::new(0));
    {
        let seen = seen_before.clone();
        let mut probe = sub.items();
        tokio::spawn(async move {
            while let Some(item) = probe.next().await {
                if matches!(item, SignalItem::Resubscribed) {
                    seen.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        seen_before.load(Ordering::Relaxed),
        0,
        "the first subscription is not a re-subscription"
    );

    let (conn_b, _guard_b) = restart_on_same_address(guard_a).await;
    shared.arm_reconnect_for_test(conn_b);

    wait_for(
        &mut items,
        Duration::from_mins(1),
        "a Resubscribed marker after the broker restarted",
        |i| matches!(i, SignalItem::Resubscribed),
    )
    .await;
}
