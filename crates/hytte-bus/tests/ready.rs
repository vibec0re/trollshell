#![cfg(feature = "system-tests")]
//! #1423: every pooled connection is built with the `mov.vibec0re.hytte.Ready`
//! placeholder staged on its builder, so zbus's object server is dispatching
//! before anything is read from the connection's socket, and no inbound method
//! call can be dropped in between.
//!
//! Two tests, for two different claims:
//!
//! * [`a_call_fired_the_moment_the_unique_name_appears_is_answered`] — the
//!   window is closed. A peer races a call at every new connection, the
//!   connection's runtime is held busy until that call is in its socket — the
//!   schedule under which the old `begin_dispatching` lost it — and the object
//!   the call is for is exported either at once or only after it is answered.
//! * [`the_pooled_connections_serve_ready_on_both_buses`] — the placeholder is
//!   on the connections production actually builds: the real `session()` and
//!   `system()` singletons, on their first connect and after a reconnect,
//!   reached from a re-exec'd child of this binary.

mod common;

use common::{CALL_BUDGET, PROBE_BUDGET, ephemeral_bus};
use futures_util::StreamExt as _;
use hytte_bus::BusKind;
use hytte_bus::export_object_with;
use hytte_bus::test_support::{SharedConnection, pooled};
use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use tokio::sync::mpsc;
use zbus::connection::Builder;
use zbus::message::{Message, Type};
use zbus::names::{BusName, UniqueName};
use zbus::zvariant::OwnedValue;

// ── The race ──────────────────────────────────────────────────────────────────

/// How many fresh connections the peer races per run: half with the export
/// started before the call lands, half after (see [`exports_first`]).
const RACE_ITERATIONS: u32 = 20;

/// How long the raced call may go unanswered before it counts as **dropped**.
///
/// Not a latency budget: a dropped call is never answered, so any bound finds
/// it, and this one is sized so a merely slow reply under CI contention cannot
/// be mistaken for one. There is deliberately no retry on silence here — unlike
/// `common::CALL_BUDGET`'s loops, whose job is liveness — because silence is
/// the exact thing this test exists to catch.
const DROP_BUDGET: Duration = Duration::from_secs(10);

/// Where the connection under test exports [`Raced`].
const RACED_PATH: &str = "/mov/vibec0re/test/Raced";
const RACED_IFACE: &str = "mov.vibec0re.test.Raced";

#[derive(Clone)]
struct Raced;

#[zbus::interface(name = "mov.vibec0re.test.Raced")]
impl Raced {
    #[allow(clippy::unused_self)]
    fn hello(&self) -> String {
        "world".to_string()
    }
}

/// Whether round `round` starts its export **before** the raced call lands
/// (odd rounds) or only once that call has been answered (even rounds).
///
/// The two shapes catch the two ways the window can come back:
///
/// * **Export first** is the shape the issue names — a peer calls an object
///   the connection exports immediately after it is built. It catches the tree
///   before #1423, where `begin_dispatching` kicked the dispatch task onto the
///   connection's own runtime: held still, that task is queued behind the
///   socket reader, which reads the call first and drops it.
/// * **Export after** leaves nothing mounted when the call lands, so only the
///   way the connection was *built* can have given it a consumer. It catches
///   the barrier removed with nothing put back. The export-first shape only
///   catches that by luck: there the export's supervisor, on the hytte
///   runtime, creates the object server while the owner is held, and it
///   usually wins (measured with `test_support::connect` bypassing
///   `build_pooled`: red in 5 of 40 runs with export-first rounds alone).
///
/// In both shapes the fixed tree answers every call: `Hello` if the export was
/// already mounted, `UnknownObject` from the connection itself if not.
const fn exports_first(round: u32) -> bool {
    round % 2 == 1
}

/// What the connection under test did with the peer's first call.
#[derive(Debug)]
enum FirstCall {
    /// `Hello` answered: the export was already mounted.
    Served,
    /// An error reply *from the connection itself* — it dispatched the call and
    /// found nothing at [`RACED_PATH`] yet. Answered, which is the point.
    Refused(String),
    /// No reply within [`DROP_BUDGET`]: the call was read before anything could
    /// consume it, and zbus dropped it. #1011's failure.
    Dropped,
}

/// What the peer tells the owner as a round goes on.
#[derive(Debug)]
enum Verdict {
    /// The raced call was answered. An export-after round starts its export
    /// now.
    Answered,
    /// The export is served; the round is over. `true` if another follows.
    Settled(bool),
    /// A call was dropped: the test is failing, stop building connections.
    Stop,
}

/// The owner's side of the race: per round, build a pooled connection, maybe
/// export on it at once, then hold its runtime until the peer's call is in its
/// socket.
///
/// Runs on its **own current-thread runtime** on its own thread, so every task
/// zbus spawns for the connection — its socket reader and, before #1423, the
/// dispatch task `begin_dispatching` kicked — lands on a runtime this function
/// can hold still. The hold is a blocking `recv` on purpose: it is the schedule
/// the #1011 campaign measured on CI, where a starved runtime had not yet polled
/// the dispatch task when the call arrived (the first call lost 94 / 900 times
/// on this suite's constructor), made certain instead of occasional. On a
/// connection built without the barrier, the reader was spawned first and runs
/// first once the runtime is let go, reads the call, finds no consumer and drops
/// it. With the barrier, `build()` did not return until the dispatch task was
/// registered and the reader did not exist before that, so there is nothing for
/// the hold to expose.
fn own_and_export(
    address: &str,
    fired: &std_mpsc::Receiver<()>,
    mut verdicts: mpsc::UnboundedReceiver<Verdict>,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("owner runtime");
    runtime.block_on(async move {
        let mut keep = Vec::new();
        for round in 1..=RACE_ITERATIONS {
            // The production build path (`build_pooled`), then — in an
            // export-first round — straight into an export. Nothing between
            // them yields to this runtime.
            let conn = common::connect(address).await;
            let shared = SharedConnection::for_test_session(conn);
            let mut export =
                exports_first(round).then(|| export_object_with(&shared, RACED_PATH).start(Raced));

            // The hold. Blocking, not awaiting: this runtime must not run a
            // single task until the peer's call has reached our socket.
            fired
                .recv_timeout(PROBE_BUDGET)
                .expect("the peer never fired at this connection");

            let verdict = verdicts.recv().await.expect("the peer went away mid-race");
            if matches!(verdict, Verdict::Stop) {
                keep.push((shared, export));
                break;
            }
            assert!(
                matches!(verdict, Verdict::Answered),
                "round {round}: expected the peer's first verdict, got {verdict:?}"
            );
            if export.is_none() {
                export = Some(export_object_with(&shared, RACED_PATH).start(Raced));
            }

            let verdict = verdicts.recv().await.expect("the peer went away mid-race");
            keep.push((shared, export));
            match verdict {
                Verdict::Settled(true) => {}
                Verdict::Settled(false) | Verdict::Stop => break,
                Verdict::Answered => panic!("round {round}: the peer answered twice"),
            }
        }
        // Let the export supervisors unmount while this runtime still drives
        // the sockets they would write to.
        drop(keep);
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
}

/// Wait for the broker to announce a **new** unique name — the earliest moment
/// anything outside a connection can learn that it exists.
async fn next_new_unique_name(
    appeared: &mut zbus::fdo::NameOwnerChangedStream,
) -> UniqueName<'static> {
    loop {
        let signal = tokio::time::timeout(PROBE_BUDGET, appeared.next())
            .await
            .expect("no connection appeared on the bus")
            .expect("NameOwnerChanged stream ended");
        let args = signal.args().expect("NameOwnerChanged args");
        if let BusName::Unique(name) = args.name()
            && args.old_owner().is_none()
            && args.new_owner().is_some()
        {
            return name.to_owned();
        }
    }
}

/// Fire `Hello` at `target` the instant it appears, and report what came back.
///
/// The call is sent without awaiting its reply, then a `GetId` round trip to
/// the broker runs behind it: `dbus-daemon` handles one sender's messages in
/// order, so by the time `GetId` is answered the raced call has been routed to
/// `target`. Only then is the owner told to stop holding (`fired`). No sleep
/// decides that.
async fn race_one(
    peer: &zbus::Connection,
    dbus: &zbus::fdo::DBusProxy<'_>,
    target: &UniqueName<'_>,
    fired: &std_mpsc::Sender<()>,
) -> FirstCall {
    let mut replies = zbus::MessageStream::from(peer);
    let call = Message::method_call(RACED_PATH, "Hello")
        .expect("method call builder")
        .destination(target.as_str())
        .expect("destination")
        .interface(RACED_IFACE)
        .expect("interface")
        .build(&())
        .expect("build the raced call");
    let serial = call.primary_header().serial_num();
    peer.send(&call).await.expect("send the raced call");

    dbus.get_id()
        .await
        .expect("GetId: the ordering barrier behind the raced call");
    fired.send(()).expect("owner went away mid-race");

    let reply = tokio::time::timeout(DROP_BUDGET, async {
        while let Some(message) = replies.next().await {
            let message = message.expect("peer message stream");
            if message.header().reply_serial() == Some(serial) {
                return message;
            }
        }
        panic!("peer message stream ended before the raced call was answered");
    })
    .await;

    match reply {
        Err(_elapsed) => FirstCall::Dropped,
        Ok(message) if message.message_type() == Type::MethodReturn => {
            let body: String = message
                .body()
                .deserialize()
                .expect("Hello returns a string");
            assert_eq!(body, "world");
            FirstCall::Served
        }
        Ok(message) => {
            let header = message.header();
            let name = header
                .error_name()
                .map_or_else(|| "<no error name>".to_string(), ToString::to_string);
            // The broker answers for a destination that does not exist; that
            // would mean the connection vanished, not that it answered.
            assert!(
                !common::BROKER_GENERATED_ERRORS.contains(&name.as_str()),
                "the broker, not the connection, answered the raced call: {name}"
            );
            FirstCall::Refused(name)
        }
    }
}

/// After a first call that was answered, the export must actually be served:
/// the claim is "served, not dropped", not merely "answered with an error".
async fn hello_is_served(peer: &zbus::Connection, target: &UniqueName<'_>) -> bool {
    let deadline = tokio::time::Instant::now() + PROBE_BUDGET;
    while tokio::time::Instant::now() < deadline {
        let call = peer.call_method(
            Some(target.as_str()),
            RACED_PATH,
            Some(RACED_IFACE),
            "Hello",
            &(),
        );
        if let Ok(Ok(reply)) = tokio::time::timeout(CALL_BUDGET, call).await
            && reply
                .body()
                .deserialize::<String>()
                .is_ok_and(|s| s == "world")
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

/// A method call fired at a pooled connection the moment its unique name
/// appears on the bus is **answered**, every time, however late the
/// connection's runtime gets round to reading it (#1423).
///
/// Each of [`RACE_ITERATIONS`] rounds: the owner thread builds a connection
/// through `common::connect` (i.e. `build_pooled`, the production build path)
/// and wraps it in a `SharedConnection`; the peer, watching `NameOwnerChanged`,
/// fires `Hello` at the new unique name as soon as the broker announces it; and
/// the owner's runtime is held still until that call is in the connection's
/// socket (see [`own_and_export`]). The object the call is for is exported
/// either in the same breath as the build or only after the call is answered,
/// alternating ([`exports_first`] has why both). The call may land before the
/// export is mounted — then the connection answers `UnknownObject` and the
/// peer retries until it is served — but it must never go unanswered.
///
/// **Falsified** both ways [`exports_first`] names, as #1423's PR measured:
/// the tree before #1423 (`build_pooled` without its `serve_at`,
/// `begin_dispatching` put back) drops the raced call in round 1, every run;
/// and `test_support::connect` bypassing `build_pooled` with nothing put back
/// drops it by round 2, the first export-after round, in 40 runs of 40.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_fired_the_moment_the_unique_name_appears_is_answered() {
    let (_first, guard) = ephemeral_bus().await;
    let address = guard.address.clone();

    // The peer: a plain zbus client, subscribed to the broker's announcements
    // before any connection under test exists.
    let peer = Builder::address(address.as_str())
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("peer connection");
    let dbus = zbus::fdo::DBusProxy::new(&peer).await.expect("DBus proxy");
    let mut appeared = dbus
        .receive_name_owner_changed()
        .await
        .expect("subscribe to NameOwnerChanged");

    let (fired_tx, fired_rx) = std_mpsc::channel::<()>();
    let (verdict_tx, verdict_rx) = mpsc::unbounded_channel::<Verdict>();
    let owner_address = address.clone();
    let owner = std::thread::spawn(move || own_and_export(&owner_address, &fired_rx, verdict_rx));

    // First-call outcomes, for the failure message and the campaign line.
    let mut first_calls: Vec<(u32, String)> = Vec::new();
    for round in 1..=RACE_ITERATIONS {
        let target = next_new_unique_name(&mut appeared).await;
        let first = race_one(&peer, &dbus, &target, &fired_tx).await;
        let refused = match first {
            FirstCall::Dropped => {
                let _ = verdict_tx.send(Verdict::Stop);
                let _ = tokio::task::spawn_blocking(move || owner.join()).await;
                panic!(
                    "round {round} of {RACE_ITERATIONS} (export {}): a Hello fired at \
                     {target} the moment it appeared got no reply at all within \
                     {DROP_BUDGET:?}. The connection read it before anything could \
                     consume it and zbus dropped it — the window #1423's `Ready` \
                     barrier closes (#1011). Earlier rounds' first calls: {first_calls:?}",
                    if exports_first(round) {
                        "first"
                    } else {
                        "after"
                    },
                );
            }
            FirstCall::Served => {
                first_calls.push((round, "served".to_string()));
                false
            }
            FirstCall::Refused(name) => {
                first_calls.push((round, name));
                true
            }
        };
        verdict_tx
            .send(Verdict::Answered)
            .expect("owner went away mid-race");
        if refused {
            assert!(
                hello_is_served(&peer, &target).await,
                "round {round}: the connection answered the first call but never \
                 served the export within {PROBE_BUDGET:?}"
            );
        }
        verdict_tx
            .send(Verdict::Settled(round < RACE_ITERATIONS))
            .expect("owner went away mid-race");
    }

    tokio::task::spawn_blocking(move || owner.join())
        .await
        .expect("join the owner thread")
        .expect("the owner thread panicked");

    // Only visible under `--nocapture`: how the raced calls were answered, by
    // round shape — what a campaign reads to see the race was really run.
    let count = |first: bool, served: bool| {
        first_calls
            .iter()
            .filter(|(round, outcome)| {
                exports_first(*round) == first && (outcome == "served") == served
            })
            .count()
    };
    println!(
        "race: {RACE_ITERATIONS} rounds, none dropped; export first: served {}, refused {}; \
         export after: served {}, refused {}",
        count(true, true),
        count(true, false),
        count(false, true),
        count(false, false),
    );
}

// ── The placeholder, on the connections production builds ────────────────────

/// Set only on the re-exec'd child that runs
/// [`the_pooled_connections_serve_ready_on_both_buses_inner`].
const CHILD_ENV: &str = "HYTTE_BUS_READY_TEST_CHILD";

/// Printed by the child only once its whole body has run, so the parent can
/// tell "the scenario passed" from "`--exact` matched no test and libtest
/// reported `0 passed`, exit 0".
const CHILD_OK: &str = "ready-child-reached-the-end";

/// Upper bound on the whole child process. A liveness guard: a child that
/// hangs fails this test by name instead of parking the binary.
const CHILD_BUDGET: Duration = Duration::from_mins(2);

/// How long one introspection call on a fresh pooled connection may take. No
/// retry on silence: with the barrier the first call is answered, and without
/// the `Ready` interface staged — with nothing else exported in the child —
/// the connection has no object server at all, so a retry would only wait
/// longer for a reply that is never coming.
const INTROSPECT_BUDGET: Duration = Duration::from_secs(10);

/// **The real pooled connections, both buses, first connect and reconnect.**
///
/// The in-process tests build their connections with
/// `test_support::connect`, which shares `build_pooled` with production but
/// not the part that picks a bus: `open_connection`'s `Builder::session()` /
/// `Builder::system()`. Those read `DBUS_SESSION_BUS_ADDRESS` /
/// `DBUS_SYSTEM_BUS_ADDRESS`, and nothing in-process can point those at a test
/// bus — `std::env::set_var` is `unsafe` in edition 2024 and this workspace
/// forbids `unsafe_code`. So this re-execs its own test binary, filtered to one
/// inner test, with both variables set on the **child** via `Command::env`
/// (safe: controlling a child's environment needs no `unsafe`) — the
/// `hytte-plugin` runtime tests' shape. Two separate ephemeral daemons stand in
/// for the two buses, so a connection that went to the wrong one fails too.
///
/// The child reaches the process-wide singletons through
/// `test_support::pooled`, which is exactly what `export_object`, `own_name`
/// and the rest resolve to, and checks each one twice: after its first
/// connect, and after `reconnect_for_test` has made the supervisor open a
/// replacement through the same `open_connection` — the reconnect path #1423
/// had to cover.
///
/// **Falsified** three ways, each red on the child's own assertion and
/// surfaced here as a failed child: dropping `build_pooled`'s `serve_at`
/// (red at the first bus checked); making one arm of `open_connection` build
/// without `build_pooled` (red on that bus alone, session or system); and
/// bypassing `build_pooled` only when the supervisor reconnects (red "after a
/// reconnect").
#[tokio::test(flavor = "multi_thread")]
async fn the_pooled_connections_serve_ready_on_both_buses() {
    let (_session_conn, session) = ephemeral_bus().await;
    let (_system_conn, system) = ephemeral_bus().await;

    let inner = "the_pooled_connections_serve_ready_on_both_buses_inner";
    let args = ["--exact", "--nocapture", "--test-threads=1", inner];
    assert!(
        args.contains(&"--exact"),
        "the re-exec must stay filtered to exactly one inner test",
    );
    let exe = std::env::current_exe().expect("this test binary's own path");
    let child = tokio::process::Command::new(exe)
        .args(args)
        .env(CHILD_ENV, "1")
        // Spelled as literals: these are the variables zbus reads, not ours.
        .env("DBUS_SESSION_BUS_ADDRESS", &session.address)
        .env("DBUS_SYSTEM_BUS_ADDRESS", &system.address)
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(CHILD_BUDGET, child)
        .await
        .expect("the child did not finish within its budget")
        .expect("re-exec this test binary with both bus addresses set");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "the child scenario failed ({:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        out.status,
    );
    assert!(
        stdout.contains(CHILD_OK),
        "the child exited 0 without reaching the end of {inner} — a stale filter \
         matches no test and libtest still reports success\n\
         --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
    );
}

/// The scenario body of [`the_pooled_connections_serve_ready_on_both_buses`].
/// Does nothing at all unless the parent's marker is set: an ordinary run
/// discovers it like any other test, and without the parent's environment the
/// singletons it touches would connect to the host's own buses.
#[tokio::test(flavor = "multi_thread")]
async fn the_pooled_connections_serve_ready_on_both_buses_inner() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }

    for (kind, variable) in [
        (BusKind::Session, "DBUS_SESSION_BUS_ADDRESS"),
        (BusKind::System, "DBUS_SYSTEM_BUS_ADDRESS"),
    ] {
        let address = std::env::var(variable).expect("the parent sets both bus addresses");
        let shared = pooled(kind);

        let first = unique_name_at_epoch(&shared, 1).await;
        assert_ready_is_served(&address, &first, kind, "its first connect").await;

        shared.reconnect_for_test().await;
        let second = unique_name_at_epoch(&shared, 2).await;
        assert_ne!(
            first, second,
            "{kind:?}: the supervisor did not open a new connection"
        );
        assert_ready_is_served(&address, &second, kind, "a reconnect").await;
    }

    println!("{CHILD_OK}");
}

/// Wait for `shared`'s supervisor to have installed connection number `epoch`,
/// and return that connection's unique name.
async fn unique_name_at_epoch(shared: &SharedConnection, epoch: u64) -> String {
    let deadline = tokio::time::Instant::now() + PROBE_BUDGET;
    while shared.epoch() < epoch && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        shared.epoch() >= epoch,
        "{:?}: the supervisor never reached epoch {epoch} within {PROBE_BUDGET:?} \
         (still {})",
        shared.kind(),
        shared.epoch()
    );
    shared
        .with_conn(|c| async move {
            Ok::<_, zbus::Error>(
                c.unique_name()
                    .expect("a bus connection has a unique name")
                    .to_string(),
            )
        })
        .await
        .expect("with_conn on a freshly installed connection")
}

/// `mov.vibec0re.hytte.Ready` is served at `/mov/vibec0re/hytte` on the
/// connection `unique`, on the bus at `address` — and has no methods, no
/// properties and no signals.
///
/// Both names are spelled as literals, not through `hytte_bus::READY_*`: this
/// is the bus surface an outside observer sees, and a test built from the same
/// constants would agree with a typo in them.
async fn assert_ready_is_served(address: &str, unique: &str, kind: BusKind, when: &str) {
    let client = Builder::address(address)
        .expect("parse bus address")
        .build()
        .await
        .expect("probe connection");

    let introspect = client.call_method(
        Some(unique),
        "/mov/vibec0re/hytte",
        Some("org.freedesktop.DBus.Introspectable"),
        "Introspect",
        &(),
    );
    let xml: String = tokio::time::timeout(INTROSPECT_BUDGET, introspect)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{kind:?} after {when}: Introspect on /mov/vibec0re/hytte got no reply at \
                 all within {INTROSPECT_BUDGET:?} — this pooled connection has no object \
                 server, so it was built without the `Ready` barrier (#1423)"
            )
        })
        .unwrap_or_else(|e| {
            panic!("{kind:?} after {when}: Introspect on /mov/vibec0re/hytte failed: {e}")
        })
        .body()
        .deserialize()
        .expect("Introspect returns a string");

    let open = "<interface name=\"mov.vibec0re.hytte.Ready\">";
    let start = xml.find(open).unwrap_or_else(|| {
        panic!(
            "{kind:?} after {when}: /mov/vibec0re/hytte does not serve \
             mov.vibec0re.hytte.Ready (#1423):\n{xml}"
        )
    });
    let body = &xml[start + open.len()..];
    let body = &body[..body.find("</interface>").expect("interface element closes")];
    for member in ["<method", "<property", "<signal"] {
        assert!(
            !body.contains(member),
            "{kind:?} after {when}: mov.vibec0re.hytte.Ready grew a {member}> member:\n{xml}"
        );
    }

    let get_all = client.call_method(
        Some(unique),
        "/mov/vibec0re/hytte",
        Some("org.freedesktop.DBus.Properties"),
        "GetAll",
        &("mov.vibec0re.hytte.Ready",),
    );
    let properties: HashMap<String, OwnedValue> = tokio::time::timeout(INTROSPECT_BUDGET, get_all)
        .await
        .expect("GetAll on the Ready placeholder got no reply")
        .expect("GetAll on the Ready placeholder")
        .body()
        .deserialize()
        .expect("GetAll returns a{sv}");
    assert!(
        properties.is_empty(),
        "{kind:?} after {when}: mov.vibec0re.hytte.Ready has properties: {properties:?}"
    );
}
