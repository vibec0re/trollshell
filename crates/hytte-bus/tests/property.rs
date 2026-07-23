#![cfg(feature = "system-tests")]

mod common;

use common::ephemeral_bus;
use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use hytte_bus::test_support::SharedConnection;
use hytte_bus::{PropState, property_with};
use std::time::Duration;

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

struct SlowCounter;

#[zbus::interface(name = "mov.vibec0re.test.SlowCounter")]
impl SlowCounter {
    #[zbus(property)]
    async fn value(&self) -> u32 {
        // Hold the Get open long enough for the test to emit a change while the
        // tracker is parked inside its initial cold_get.
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

    let server = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name(IFACE)
        .unwrap()
        .serve_at(PATH, SlowCounter)
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

    // Rather than guessing a single delay by which the tracker's subscribe
    // (AddMatch) has landed, keep re-emitting PropertiesChanged(Value = 99)
    // while polling for delivery. `subscribe_properties_changed` always
    // completes before `cold_get` is even entered (source-level invariant,
    // #429) — SlowCounter's 600ms-sleeping `value()` getter only starts
    // *after* that Get request is dispatched — so once the tracker task has
    // been scheduled at all, every retry from here on is emitted after the
    // subscription is live and gets buffered/replayed regardless of exactly
    // how long that took on this runner. The loop below both re-emits and
    // polls for the resulting Loaded(99), so it early-exits the moment the
    // race window has been positively exercised and observed.
    let emitter = SignalEmitter::new(&server, PATH).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut final_value = None;
    while tokio::time::Instant::now() < deadline && final_value != Some(99) {
        let mut changed: HashMap<&str, Value> = HashMap::new();
        changed.insert("Value", Value::from(99u32));
        let _ = zbus::fdo::Properties::properties_changed(
            &emitter,
            InterfaceName::try_from(IFACE).unwrap(),
            changed,
            Cow::Borrowed(&[]),
        )
        .await;

        let poll_deadline = tokio::time::Instant::now() + Duration::from_millis(50);
        while tokio::time::Instant::now() < poll_deadline {
            if let Ok(Some(PropState::Loaded(v))) =
                tokio::time::timeout(Duration::from_millis(20), stream.next()).await
            {
                final_value = Some(v);
                if v == 99 {
                    break;
                }
            }
        }
    }

    assert_eq!(
        final_value,
        Some(99),
        "a change emitted during the initial-Get window was lost (#429): \
         tracker did not converge to Loaded(99)"
    );
}
