#![cfg(feature = "system-tests")]

mod common;

use common::ephemeral_bus;
use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{PropState, property_with};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

struct Counter {
    value: u32,
}

#[zbus::interface(name = "mov.vibec0re.test.Counter")]
impl Counter {
    #[zbus(property)]
    fn value(&self) -> u32 {
        self.value
    }

    #[zbus(property)]
    fn set_value(&mut self, v: u32) {
        self.value = v;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cold_start_emits_loading_then_loaded() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();

    // Stand up a server that exposes the Counter interface.
    let _server = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("mov.vibec0re.test.Counter")
        .unwrap()
        .serve_at("/mov/vibec0re/test/Counter", Counter { value: 7 })
        .unwrap()
        .build()
        .await
        .unwrap();

    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let prop = property_with::<u32>(&shared, "mov.vibec0re.test.Counter")
        .at_path("/mov/vibec0re/test/Counter")
        .iface("mov.vibec0re.test.Counter")
        .name("Value")
        .start();

    let mut stream = prop.signal().to_stream();
    let mut saw_loading = false;
    let mut saw_loaded = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && saw_loaded.is_none() {
        let next = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
        if let Ok(Some(state)) = next {
            match state {
                PropState::Loading => saw_loading = true,
                PropState::Loaded(v) => saw_loaded = Some(v),
                PropState::Stale(_) => {}
            }
        }
    }

    assert!(saw_loading, "expected at least one Loading emission");
    assert_eq!(saw_loaded, Some(7));
}

#[tokio::test(flavor = "multi_thread")]
async fn properties_changed_emits_loaded_with_new_value() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();

    let server = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("mov.vibec0re.test.Counter")
        .unwrap()
        .serve_at("/mov/vibec0re/test/Counter", Counter { value: 1 })
        .unwrap()
        .build()
        .await
        .unwrap();

    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let prop = property_with::<u32>(&shared, "mov.vibec0re.test.Counter")
        .at_path("/mov/vibec0re/test/Counter")
        .iface("mov.vibec0re.test.Counter")
        .name("Value")
        .start();
    let mut stream = prop.signal().to_stream();

    // Drain initial Loading + Loaded(1).
    let mut current = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && !matches!(current, Some(1)) {
        if let Ok(Some(PropState::Loaded(v))) =
            tokio::time::timeout(Duration::from_millis(50), stream.next()).await
        {
            current = Some(v);
        }
    }
    assert_eq!(current, Some(1));

    // No extra wait needed here: the tracker subscribes to PropertiesChanged
    // BEFORE issuing its initial Get (#429, see property.rs's run_property),
    // and that subscribe is a synchronous round-trip (AddMatch + reply). So by
    // the time we've already observed Loaded(1) above — which can only be set
    // *after* the Get completes — the subscription is provably live. Emitting
    // now instead of guessing a fixed delay is the readiness signal.
    //
    // Mutate the server-side property and emit PropertiesChanged.
    let iface_ref = server
        .object_server()
        .interface::<_, Counter>("/mov/vibec0re/test/Counter")
        .await
        .unwrap();
    {
        let mut iface = iface_ref.get_mut().await;
        iface.set_value(99);
        iface
            .value_changed(iface_ref.signal_emitter())
            .await
            .unwrap();
    }

    // Expect the consumer signal to update to Loaded(99).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut updated = None;
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(PropState::Loaded(v))) =
            tokio::time::timeout(Duration::from_millis(50), stream.next()).await
        {
            updated = Some(v);
            break;
        }
    }
    assert_eq!(updated, Some(99));
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnect_emits_stale_then_loaded() {
    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();

    // Stand up a server exposing the Counter interface (value = 42).
    let _server = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name("mov.vibec0re.test.Counter")
        .unwrap()
        .serve_at("/mov/vibec0re/test/Counter", Counter { value: 42 })
        .unwrap()
        .build()
        .await
        .unwrap();

    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let prop = property_with::<u32>(&shared, "mov.vibec0re.test.Counter")
        .at_path("/mov/vibec0re/test/Counter")
        .iface("mov.vibec0re.test.Counter")
        .name("Value")
        .start();

    let mut stream = prop.signal().to_stream();

    // ── Step 1: drain until Loaded(42) ───────────────────────────────────────
    let mut saw_loaded_initial = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !saw_loaded_initial {
        if let Ok(Some(PropState::Loaded(42))) =
            tokio::time::timeout(Duration::from_millis(50), stream.next()).await
        {
            saw_loaded_initial = true;
        }
    }
    assert!(
        saw_loaded_initial,
        "did not observe Loaded(42) before simulated disconnect"
    );

    // No extra wait needed before triggering the disconnect: having already
    // observed Loaded(42) above proves the subscribe-before-Get round-trip
    // (#429) already completed, so the tracker is already past the point of
    // being able to miss a subsequent epoch bump (mirrors the reasoning in
    // properties_changed_emits_loaded_with_new_value).
    //
    // ── Step 2: open a replacement connection and simulate a disconnect ───────
    let replacement = zbus::connection::Builder::address(address.as_str())
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("open replacement connection to ephemeral bus");

    shared.simulate_disconnect_for_test(replacement).await;

    // ── Step 3: assert Stale(42) then Loaded(_) are both observed ────────────
    let mut saw_stale = false;
    let mut saw_loaded_after = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !(saw_stale && saw_loaded_after) {
        let next = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
        if let Ok(Some(state)) = next {
            match state {
                PropState::Stale(42) => saw_stale = true,
                PropState::Loaded(_) if saw_stale => saw_loaded_after = true,
                _ => {}
            }
        }
    }

    assert!(
        saw_stale,
        "property task did not emit Stale(42) after simulated disconnect"
    );
    assert!(
        saw_loaded_after,
        "property task did not emit Loaded(_) after Stale(42)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn task_exits_when_property_signal_dropped() {
    let (conn, _guard) = ephemeral_bus().await;
    let shared = SharedConnection::for_test_session(conn);

    // Start a property tracker. No server is needed — the task will park
    // in its retry loop, but that's fine: we only care that it exits when
    // the handle is dropped.
    let prop = property_with::<u32>(&shared, "mov.vibec0re.test.Counter")
        .at_path("/mov/vibec0re/test/Counter")
        .iface("mov.vibec0re.test.Counter")
        .name("Value")
        .start();

    // Grab the task-done receiver while we still hold the handle.
    let done_rx = prop
        .task_done_receiver()
        .await
        .expect("task_done_receiver should be Some on first call");

    // Let the task actually get scheduled at least once before we drop. This
    // is a courtesy, not a correctness requirement: `HandleTracker`'s count is
    // decremented independently of task scheduling (see hytte-bus's
    // handle.rs), so the task will observe `all_dropped()` on its very first
    // loop iteration even if we dropped before it ever ran. A couple of
    // scheduler yields are enough to exercise the "already running" path too,
    // without guessing a wall-clock delay.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Drop the handle — the task must detect this and exit.
    drop(prop);

    // The task must exit within 1 second.
    tokio::time::timeout(Duration::from_secs(1), done_rx)
        .await
        .expect("timeout: property task did not exit within 1s after handle dropped")
        .expect("task_done_tx was dropped without sending — task panicked?");
}

// ── #429: change during the initial-Get window must not be lost ───────────────
//
// The tracker must subscribe to `PropertiesChanged` BEFORE issuing the initial
// `Get`, so a change that lands while the Get is in flight is buffered by the
// live subscription and replayed, rather than emitted before the AddMatch and
// silently lost.
//
// `SlowCounter::value` sleeps, holding the tracker parked inside `cold_get`
// while the test emits `PropertiesChanged(Value = 99)`. Under the fixed order
// the subscription is already live and buffers the change (the drain then wins
// over the slightly-later `Loaded(1)` → converges to `Loaded(99)`). Under the
// old Get-then-subscribe order the change is emitted before the subscription
// exists and is lost, leaving the tracker stuck at `Loaded(1)`.
//
// `get_started` turns "safe to emit" from a guessed delay into a deterministic
// signal: `run_property` subscribes to `PropertiesChanged` *before* calling
// `cold_get` (property.rs:276, #429), so `value()` — the Get's server-side
// handler — can only start executing after that subscription is fully live.
// Firing the notification at the top of `value()`, before its own sleep, lets
// the test await exactly one instant at which a single emit is guaranteed to
// land inside the window, rather than re-emitting on a guess-and-poll loop.

struct SlowCounter {
    get_started: Arc<Notify>,
}

#[zbus::interface(name = "mov.vibec0re.test.SlowCounter")]
impl SlowCounter {
    #[zbus(property)]
    async fn value(&self) -> u32 {
        // Signal the test that the Get has landed — see `get_started` above —
        // then hold the Get open long enough for the test to emit a change
        // while the tracker is parked inside its initial cold_get.
        self.get_started.notify_one();
        tokio::time::sleep(Duration::from_millis(600)).await;
        1
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn change_during_initial_get_window_is_not_lost() {
    use std::borrow::Cow;
    use std::collections::HashMap;
    use zbus::names::InterfaceName;
    use zbus::object_server::SignalEmitter;
    use zbus::zvariant::Value;

    const PATH: &str = "/mov/vibec0re/test/SlowCounter";
    const IFACE: &str = "mov.vibec0re.test.SlowCounter";

    let (conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();

    let get_started = Arc::new(Notify::new());
    let server = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name(IFACE)
        .unwrap()
        .serve_at(
            PATH,
            SlowCounter {
                get_started: get_started.clone(),
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let shared = SharedConnection::for_test_session(conn);
    shared.spawn_supervisor_for_test();

    let prop = property_with::<u32>(&shared, IFACE)
        .at_path(PATH)
        .iface(IFACE)
        .name("Value")
        .start();
    let mut stream = prop.signal().to_stream();

    // Wait (bounded) for the tracker to reach the getter. By source order
    // (property.rs:276, #429) this can only fire after the PropertiesChanged
    // subscription is fully live, so it's a deterministic "safe to emit"
    // signal rather than a guessed delay — see the comment on `SlowCounter`.
    tokio::time::timeout(Duration::from_secs(2), get_started.notified())
        .await
        .expect("tracker never reached the initial Get — subscribe_properties_changed stalled?");

    // Emit exactly once. This is the regression assertion for #429: a single
    // change, guaranteed (not guessed) to land inside the 600ms Get window,
    // must be buffered and win over the Get's own `Loaded(1)`. No re-emitting
    // — if during-window buffering regressed, this change is lost for good
    // and the wait below must time out, not quietly succeed via post-Get live
    // delivery.
    let emitter = SignalEmitter::new(&server, PATH).unwrap();
    let mut changed: HashMap<&str, Value> = HashMap::new();
    changed.insert("Value", Value::from(99u32));
    zbus::fdo::Properties::properties_changed(
        &emitter,
        InterfaceName::try_from(IFACE).unwrap(),
        changed,
        Cow::Borrowed(&[]),
    )
    .await
    .expect("failed to emit PropertiesChanged");

    // Bounded wait for delivery (bus round-trip + tracker wake-up + the
    // remainder of the 600ms Get) — generous, but never unbounded.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut final_value = None;
    while tokio::time::Instant::now() < deadline && final_value != Some(99) {
        if let Ok(Some(PropState::Loaded(v))) =
            tokio::time::timeout(Duration::from_millis(200), stream.next()).await
        {
            final_value = Some(v);
        }
    }

    assert_eq!(
        final_value,
        Some(99),
        "a change emitted during the initial-Get window was lost (#429): \
         tracker did not converge to Loaded(99)"
    );
}

// ── #1222 item 1: the daemon dies while the cold Get is in flight ────────────
//
// `cold_get` wraps the initial `Get` in `with_conn` (property.rs's `cold_get`,
// this file's neighbour), which is the reconnect seam: a transient failure on
// that call is what clears the cached connection and wakes the supervisor
// (`connection.rs`'s `with_conn` tail). Nothing in this crate pinned the case
// where the daemon dies WHILE that specific Get is still outstanding —
// `reconnect_emits_stale_then_loaded` (above, :149) disconnects only *after*
// observing `Loaded(42)`; `change_during_initial_get_window_is_not_lost`
// (above, :310) races a `PropertiesChanged` signal against the Get, not a
// connection drop; `killed_and_restarted_daemon_is_detected_by_with_conn_itself`
// (`connection_reconnect.rs`:170) kills the daemon only after draining to the
// initial `Loaded(42)`. This test kills it *during* the first, still-pending
// Get.
//
// `WithholdingCounter::value` notifies `get_started` the instant the Get
// lands on the peer, then parks forever (`std::future::pending`) — there is
// nothing to "release" it to: once the daemon that would route its reply is
// dead, that reply can never reach the client on any connection, old or new,
// so there is no point modelling a release path that cannot deliver.
//
// The contract this pins (`PropState`'s own doc plus `cold_get`'s comment):
// once the daemon is restarted, the subscriber must converge to `Loaded` on
// the value the FRESH connection's Get answers with, and must never observe
// `Loaded`/`Stale` on 42 — the value the dead connection's Get would have
// answered with, had it ever gotten to reply. A `Stale` marking in between is
// allowed by the state machine but not required here: since the first Get
// never completes, `run_property`'s `last` never becomes `Some`, so the one
// `needs_mark` write already spent before this test starts touching the
// daemon (the initial `Loading`) is the only mark until recovery — there is
// no `last` value for an in-between `Stale` to carry.
//
// ── Why this uses two independent ephemeral buses, not `restart_on_same_
// address` ───────────────────────────────────────────────────────────────
//
// Recovery here has to go through a real `spawn_supervisor_for_test` +
// `arm_reconnect_for_test`, exactly like `tests/resubscribe.rs`'s
// `a_lone_property_subscription_detects_a_dead_daemon_and_recovers`, and for
// the same reason that test gives for not using
// `simulate_disconnect_for_test`: that helper clears the cache and wakes the
// supervisor *itself*, which would make this test pass even if `with_conn`'s
// own detection were deleted.
//
// But an in-flight `Get` is the *fastest possible* detector in this crate —
// it is already registered as a receiver on the connection's broadcast
// channel, so it observes the socket-reader's error directly, with no extra
// "resubscribe, discover the stream ended, and re-issue a fresh call" lap
// like every other reconnect test's detection path takes. Measured against
// `restart_on_same_address` (kill, wait for reap, spawn a new daemon on the
// same path, wait for its startup line, dial it) that speed is a liability:
// the supervisor can wake and lose the race to a real, unrelated session bus
// before this test ever reaches its own `arm_reconnect_for_test` call —
// `connection_reconnect.rs`'s neighbouring comment calls exactly this race
// out as "a trap this test has no reason to walk into", and it is not
// hypothetical here — a `nix develop` shell has a real, reachable
// `$DBUS_SESSION_BUS_ADDRESS`, and an earlier draft of this test lost that
// race reliably (the retried `Get` came back `ServiceUnknown` from that real
// bus instead of `Ok` from the intended replacement).
//
// So the replacement bus (server included) is stood up and armed BEFORE bus
// A's daemon is killed, which closes the race window by construction rather
// than by hoping the arm wins it: `arm_reconnect_for_test` only populates a
// side table the supervisor consults on its *next* wake, and nothing wakes
// the supervisor until the in-flight Get on bus A actually fails — which
// cannot happen before bus A's daemon dies, several lines below the arm.
struct WithholdingCounter {
    value: u32,
    get_started: Arc<Notify>,
}

#[zbus::interface(name = "mov.vibec0re.test.WithholdingCounter")]
impl WithholdingCounter {
    #[zbus(property)]
    async fn value(&self) -> u32 {
        // Prove the Get actually landed on the peer, then park forever: the
        // reply this would eventually send can never reach the client once
        // the daemon that routes it is dead, so nothing ever needs to release
        // this — see the module comment above.
        self.get_started.notify_one();
        std::future::pending::<()>().await;
        self.value
    }
}

/// Non-withholding counter for the replacement bus: answers immediately with
/// a value distinct from [`WithholdingCounter`]'s withheld 42, so reaching it
/// requires a fresh Get on the fresh connection rather than a replay of the
/// dead epoch's (never-sent) answer.
struct FreshCounter {
    value: u32,
}

#[zbus::interface(name = "mov.vibec0re.test.WithholdingCounter")]
impl FreshCounter {
    #[zbus(property)]
    fn value(&self) -> u32 {
        self.value
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn get_in_flight_when_the_daemon_dies_recovers_to_loaded_after_restart() {
    const IFACE: &str = "mov.vibec0re.test.WithholdingCounter";
    const PATH: &str = "/mov/vibec0re/test/WithholdingCounter";
    /// The dead epoch's withheld answer. Never sent, and must never be
    /// observed by the subscriber under test.
    const WITHHELD: u32 = 42;
    /// The replacement bus's answer. Distinct from `WITHHELD` on purpose —
    /// see the module comment.
    const FRESH: u32 = 43;

    // ── Bus A: where the Get will be genuinely outstanding when its daemon
    // dies ────────────────────────────────────────────────────────────────
    let (conn_a, guard_a) = ephemeral_bus().await;
    let address_a = guard_a.address.clone();

    let get_started = Arc::new(Notify::new());
    let _server_a = zbus::connection::Builder::address(address_a.as_str())
        .unwrap()
        .name(IFACE)
        .unwrap()
        .serve_at(
            PATH,
            WithholdingCounter {
                value: WITHHELD,
                get_started: get_started.clone(),
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let shared = SharedConnection::for_test_session(conn_a);
    shared.spawn_supervisor_for_test();

    let prop = property_with::<u32>(&shared, IFACE)
        .at_path(PATH)
        .iface(IFACE)
        .name("Value")
        .start();
    let mut stream = prop.signal().to_stream();

    // Wait (bounded) for the cold Get to actually land on the peer before
    // touching bus A — otherwise "kill it while the Get is outstanding"
    // could race a Get that hasn't been sent yet.
    tokio::time::timeout(Duration::from_secs(5), get_started.notified())
        .await
        .expect("the initial Get never reached the peer — is the tracker parked elsewhere?");

    // ── Bus B: the replacement, stood up and armed BEFORE bus A dies — see
    // the module comment for why this order is load-bearing ───────────────
    let (conn_b, guard_b) = ephemeral_bus().await;
    let address_b = guard_b.address.clone();
    let _server_b = zbus::connection::Builder::address(address_b.as_str())
        .unwrap()
        .name(IFACE)
        .unwrap()
        .serve_at(PATH, FreshCounter { value: FRESH })
        .unwrap()
        .build()
        .await
        .unwrap();
    shared.arm_reconnect_for_test(conn_b);

    // Now kill bus A's daemon, with the Get from above still outstanding,
    // parked and unanswered, on it.
    drop(guard_a);

    let mut saw_loading = false;
    let mut recovered = false;
    let deadline = tokio::time::Instant::now() + Duration::from_mins(1);
    while tokio::time::Instant::now() < deadline && !recovered {
        let next = tokio::time::timeout(Duration::from_millis(100), stream.next()).await;
        if let Ok(Some(state)) = next {
            match state {
                PropState::Loading => saw_loading = true,
                PropState::Loaded(WITHHELD) | PropState::Stale(WITHHELD) => panic!(
                    "observed the dead epoch's withheld answer ({WITHHELD}) after the daemon \
                     was killed with its Get still outstanding — a recovered subscription must \
                     only ever surface a fresh Get's answer, never a replay of the dead one"
                ),
                PropState::Loaded(FRESH) => recovered = true,
                _ => {}
            }
        }
    }

    assert!(
        saw_loading,
        "expected the initial Loading state to have been observed before the daemon died"
    );
    assert!(
        recovered,
        "property subscription never reached Loaded({FRESH}) after the daemon was killed with \
         its initial Get outstanding and a replacement bus armed"
    );
}
