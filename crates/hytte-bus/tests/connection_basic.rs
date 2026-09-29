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

/// A connection built the way `hytte-bus` builds one must answer an inbound
/// method call from the moment it exists — **before** anything is exported or
/// owned on it (#1011, #1423).
///
/// zbus's object-server dispatch task is the only consumer of inbound
/// `MethodCall` messages, and a call that reaches the connection before that
/// task has registered its `msg_type=MethodCall, destination=<unique name>`
/// match rule matches no receiver and is **dropped with no reply at all** —
/// there is no fallback arm in `zbus::Connection` that synthesises an
/// `UnknownObject`/`UnknownMethod` error for an unhandled call. A zbus call has
/// no reply timeout by default either, so the peer does not get an error: it
/// waits forever.
///
/// Nothing is exported here at all, so nothing but the way the connection was
/// built can have created that task. The subject comes from `ephemeral_bus`,
/// i.e. `common::connect` → `hytte_bus::test_support::connect` →
/// `connection.rs`'s `build_pooled` — the function production's
/// `open_connection` calls too — whose `serve_at` of the `Ready` placeholder is
/// what makes zbus start the dispatch task, and wait for it, inside `build()`.
/// Remove that `serve_at` and the object server is never created: the
/// `Introspect` below is answered by nobody, and the assertion fails by name
/// rather than hanging, because the wait is bounded. That is a different
/// property from the one `tests/ready.rs` races: this one would stay red however
/// long the test waited.
///
/// Until #1423 the same property was held by `begin_dispatching` in `for_test`,
/// and a sibling test pinned its second copy in `supervisor_loop`. Both copies
/// are gone; the production connect path is pinned per bus, and across a
/// reconnect, by `tests/ready.rs`'s re-exec'd child against the real
/// `session()`/`system()` singletons.
///
/// The probe is `common::answers_a_method_call`, so "answers" means the same
/// thing here as everywhere else in the suite.
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
         the connection was not built through `build_pooled`'s `Ready` barrier, \
         so zbus never started its object-server dispatch task, every call \
         addressed to this connection is dropped with no reply, and its caller \
         hangs (#1011, #1423)"
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
