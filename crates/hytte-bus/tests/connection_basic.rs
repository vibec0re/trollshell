#![cfg(feature = "system-tests")]

mod common;

use common::{PROBE_BUDGET, answers_a_method_call, ephemeral_bus};
use hytte_bus::BusKind;
use hytte_bus::test_support::SharedConnection;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn with_conn_returns_connection_on_healthy_bus() {
    let (test_conn, _guard) = ephemeral_bus().await;
    // Wraps the ephemeral connection directly via the test-only
    // `for_test_session` constructor — no `DBUS_SESSION_BUS_ADDRESS` mutation
    // (which `unsafe_code = "forbid"` rules out; see `connection.rs`'s note
    // by `simulate_disconnect_for_test`) — then asserts `with_conn` sees that
    // same connection (matching unique names).
    let shared = SharedConnection::for_test_session(test_conn.clone());
    let unique_name_via_shared: Option<String> = shared
        .with_conn(
            |c| async move { Ok::<_, zbus::Error>(c.unique_name().map(ToString::to_string)) },
        )
        .await
        .expect("with_conn returns Ok on healthy bus");

    let unique_name_direct = test_conn.unique_name().map(ToString::to_string);
    assert_eq!(unique_name_via_shared, unique_name_direct);
}

/// A `SharedConnection` must be able to answer an inbound method call from the
/// moment it exists — **before** anything is exported or owned on it (#1011).
///
/// zbus creates its object server lazily, and the dispatch task that object
/// server spawns is the only consumer of inbound `MethodCall` messages: it
/// subscribes by adding a `msg_type=MethodCall, destination=<unique name>`
/// match rule to the connection's internal routing table. Until that rule
/// exists an arriving call matches no receiver and zbus **drops it with no
/// reply at all** — there is no fallback arm in `zbus::Connection` that
/// synthesises an `UnknownObject`/`UnknownMethod` error for an unhandled call.
/// A zbus call has no reply timeout by default either, so the peer does not get
/// an error: it waits forever.
///
/// Before `connection.rs`'s `begin_dispatching`, the first thing to touch
/// `object_server()` on a hytte connection was whichever `export`/`own`
/// supervisor happened to mount first, on the hytte runtime, whenever it got
/// scheduled. This test asserts the window is not merely small but *not the
/// caller's problem*: nothing is exported here at all, so on a tree without
/// that call the object server is never created and the `Introspect` below is
/// answered by nobody — the assertion fails by name rather than hanging,
/// because the wait is bounded.
///
/// The probe is `common::answers_a_method_call` — shared with the
/// supervisor-path test below so the two cannot drift on what "answers" means.
///
/// This test pins the **test-support** call site, `connection.rs`'s `for_test`.
/// The sibling below pins the production one. Neither covers the other:
/// measured, deleting `for_test`'s `begin_dispatching` alone reds this test by
/// name and leaves the sibling green, and deleting `supervisor_loop`'s alone
/// does the reverse.
#[tokio::test(flavor = "multi_thread")]
async fn shared_connection_answers_method_calls_before_anything_is_exported() {
    let (test_conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();
    let unique = test_conn
        .unique_name()
        .expect("ephemeral connection has a unique name")
        .as_str()
        .to_string();

    // Nothing is ever exported or owned on this one. Bound rather than dropped:
    // the `SharedConnection` owns the connection being probed.
    let _shared = SharedConnection::for_test_session(test_conn);

    let (answered, unanswered) = answers_a_method_call(&address, &unique).await;
    assert!(
        answered,
        "a SharedConnection with nothing exported on it answered no method \
         call at all within {PROBE_BUDGET:?} ({unanswered} calls got no reply): \
         `for_test` did not start zbus's object-server dispatch task, so every \
         call addressed to this connection is dropped with no reply and its \
         caller hangs (#1011)"
    );
}

/// The same property, for the `begin_dispatching` call site that actually
/// **ships**: `connection.rs`'s `supervisor_loop`, which is where every
/// production `SharedConnection`'s connection is installed (`session()` /
/// `system()` → `for_kind` → `start` → this loop).
///
/// The sibling test above builds its subject with `for_test_session` and so
/// reaches `for_test`'s copy of the call and never the supervisor's. That
/// matters more than it looks: the supervisor is the path the PR for #1011
/// measured as an *improvement* (first `Hello` lost 3.9 % → 1.5 %) while
/// `for_test` is the one it measured as a *regression* (→ 10.4 %, absorbed by
/// the bounded retry) — so the suite pinned the worse path and left the shipped
/// one uncovered. Measured on this branch before this test existed: deleting
/// `supervisor_loop`'s `begin_dispatching` and keeping `for_test`'s left the
/// whole `hytte-bus` suite green, 92 passed / 0 failed, three runs in a row.
/// `begin_dispatching` is private, `fn`, and two of its three callers live in
/// `mod test_support`, so "this is test scaffolding, drop the odd one out" is a
/// live reading — and taking it would silently put production back on "the
/// first `export`/`own` mount starts the dispatch task", which is exactly the
/// racy path `control.rs`'s `Control` endpoint and `wifi/nm_agent.rs`'s secret
/// agent sit on.
///
/// `simulate_disconnect_for_test` is what drives `supervisor_loop`'s `Ok(conn)`
/// arm without a real bus outage (it pre-injects the replacement via
/// `INJECTED_CONN`, drops the cached connection and wakes the loop — the same
/// route `connection_reconnect.rs` already uses). The replacement is a
/// connection `for_test` never touched, and nothing is exported or owned on it,
/// so the probe below is a clean pin on the supervisor's call and on nothing
/// else.
#[tokio::test(flavor = "multi_thread")]
async fn a_supervisor_installed_connection_answers_method_calls() {
    let (test_conn, guard) = ephemeral_bus().await;
    let address = guard.address.clone();

    let shared = SharedConnection::for_test_session(test_conn);
    shared.spawn_supervisor_for_test();

    // The connection under test. `for_test`'s `begin_dispatching` ran on the
    // one above and never sees this one.
    let replacement = zbus::connection::Builder::address(address.as_str())
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("replacement connection");
    let unique = replacement
        .unique_name()
        .expect("replacement connection has a unique name")
        .as_str()
        .to_string();

    shared.simulate_disconnect_for_test(replacement).await;

    // Epoch 1 is `for_test`'s connection; epoch 2 is the supervisor's install.
    let deadline = tokio::time::Instant::now() + PROBE_BUDGET;
    while shared.epoch() < 2 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        shared.epoch() >= 2,
        "the supervisor never installed the injected replacement within \
         {PROBE_BUDGET:?} (epoch is still {}), so this test never reached the \
         call site it exists to pin",
        shared.epoch()
    );

    let (answered, unanswered) = answers_a_method_call(&address, &unique).await;
    assert!(
        answered,
        "the connection `supervisor_loop` installed answered no method call at \
         all within {PROBE_BUDGET:?} ({unanswered} calls got no reply): the \
         supervisor did not start zbus's object-server dispatch task, so every \
         call addressed to a production SharedConnection before its first \
         export/own mount is dropped with no reply and its caller — which has \
         no reply timeout — hangs forever (#1011)"
    );
}

/// `common::DAEMON_REAP_BUDGET` — the bound on `BusGuard`'s `Drop` and on
/// `restart_on_same_address`, i.e. the suite's two reaps of the ephemeral
/// `dbus-daemon` — must actually govern, in both directions.
///
/// It had no observable effect before this. libtest captures `eprintln!` per
/// test and discards it for a *passing* test, and `nix/checks/system-tests.nix`
/// runs `cargo test` with no `--nocapture`, so the warning the timeout arm
/// prints reaches nobody in the one place it is meant to be read: measured with
/// the budget mutated to 1 ns, a run that abandoned **every** reap printed 0
/// warning lines and passed 92/92, and 1 ns was as green as 30 s.
///
/// So the bound gets a seam, `common::reap_within`, and both of its arms get an
/// assertion:
///
/// * budget hit — a live `sleep 60` is **not** reaped within 50 ms, and the
///   call *returns* rather than parking. Mutating `reap_within` to `true` (the
///   "bound removed, answer faked" shape) reds this one, as does flipping its
///   `is_ok()` to `is_err()`. Deleting the `tokio::time::timeout` outright
///   makes it park forever rather than red, which is the honest shape: an
///   unbounded reap *is* a hang, and a hang is what #1011 was.
/// * budget not hit — the same child, after `SIGKILL`, **is** reaped within the
///   real `DAEMON_REAP_BUDGET`. Mutating `reap_within` to `false` reds this one.
///
/// What this deliberately does **not** pin is the constant's magnitude.
/// Measured, three runs: `DAEMON_REAP_BUDGET = 1 ns` leaves this test green,
/// because `tokio::time::timeout` polls the inner future before it observes an
/// already-elapsed deadline and an already-`SIGKILL`ed local child is reaped
/// inside that first poll. That is not a gap — the number is a liveness guard
/// sized against a starved runner, exactly like `DBUS_DAEMON_STARTUP_BUDGET`,
/// and asserting a magnitude would be asserting a latency nobody claims. What
/// the suite lacked and now has is a test that reds when the *bound* stops
/// governing.
///
/// No `dbus-daemon` is involved: the guard's bound is about reaping a child
/// process, and `sleep` exercises it honestly without depending on a broker.
#[tokio::test(flavor = "multi_thread")]
async fn a_reap_that_does_not_finish_within_its_budget_is_abandoned() {
    let mut slow = tokio::process::Command::new("sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .expect("spawn `sleep` — coreutils is on PATH in the devShell and the check sandbox");

    assert!(
        !common::reap_within(&mut slow, Duration::from_millis(50)).await,
        "a live child was reported reaped within 50 ms: `reap_within`'s budget \
         is not governing, so `BusGuard::drop` would wait for a wedged \
         dbus-daemon forever — the second of #1011's two forever-parks"
    );

    let _ = slow.start_kill();
    assert!(
        common::reap_within(&mut slow, common::DAEMON_REAP_BUDGET).await,
        "a SIGKILLed child was not reaped within {:?}: the guard's budget is \
         too tight to cover the case it exists for, so every teardown would \
         abandon its daemon",
        common::DAEMON_REAP_BUDGET
    );
}

// Verify the public API surface: BusKind is accessible from outside the crate.
const _: fn() = || {
    let _ = BusKind::Session;
};
