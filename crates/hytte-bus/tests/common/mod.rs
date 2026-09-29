//! Spawn an isolated `dbus-daemon` for one test.
//!
//! Each test gets a fresh broker so tests don't interfere with each other
//! and don't depend on the host's session bus. The daemon is killed when
//! the returned `BusGuard` is dropped.
//!
//! Skips with a clear `panic!("dbus-daemon not on PATH")` if the binary
//! is missing — surface the dependency loudly rather than silently.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use zbus::Connection;
use zbus::connection::Builder;

/// Upper bound on how long the ephemeral `dbus-daemon` is allowed to take to
/// print its listen address before a test gives up on it.
///
/// **This is a liveness guard, not a latency assertion.** No test in this
/// crate claims that `dbus-daemon` *should* start and print its address
/// within any particular time — that would be a meaningless thing to assert
/// about a spawned subprocess under load. The only job of this budget is to
/// stop a broker that never starts from wedging the suite (and therefore
/// `nix flake check`) forever; a genuinely broken daemon still fails the
/// test, just later rather than sooner.
///
/// This lives in the shared `common` module, so it is the single gate that
/// every one of `hytte-bus`'s ten `tests/*.rs` files passes through — each
/// spawns its own broker via [`ephemeral_bus`]. These tests run inside
/// `nix flake check`, which in the same invocation also builds two
/// `nixosTest` VMs plus the full workspace clippy and package builds — CPU
/// contention is the normal condition, not an edge case. A tight budget here
/// buys nothing (nobody is measuring startup latency) and costs false reds
/// (#676/#678: a markdown-only PR tripped a 3-second sibling budget in
/// `hytte-services` under load). If you're looking at this thinking "30
/// seconds seems excessive for a local subprocess to print a line" — it is,
/// for the happy path, and that's the point: this number is sized against
/// worst-case CI contention, not typical-case latency. Tightening it does
/// not strengthen any assertion in this module; it only makes the suite
/// flake more often under load. If you want faster failure signal for a
/// real hang, run the test locally — CI's job is to not lie.
const DBUS_DAEMON_STARTUP_BUDGET: Duration = Duration::from_secs(30);

/// Upper bound on a **single** raw `zbus::Proxy::call` issued by a test in this
/// crate, and on the retry loop such a call sits in. (#1011, #1423)
///
/// ## Why a raw call must never be awaited unbounded here
///
/// A zbus method call carries no reply timeout unless the connection was built
/// with one, and `Builder`'s `method_timeout` defaults to `None`. So a call
/// whose reply never arrives does not fail — it parks the awaiting task
/// **forever**. And a reply can genuinely never arrive: zbus's object-server
/// dispatch task is the only consumer of inbound `MethodCall` messages, and a
/// call the socket reader hands over before that task has registered its
/// `msg_type=MethodCall, destination=<unique name>` match rule matches no
/// receiver and is **dropped with no reply at all**. There is no arm in
/// `zbus::Connection` that synthesises an `UnknownObject`/`UnknownMethod`
/// error for an unhandled call.
///
/// That is what #1011 was, and it cost five `nix flake check` runs ~51 minutes
/// of silence apiece before the job timeout killed them: `tests/export.rs`
/// captured a unique name, called `Hello` on it, lost the race, and the
/// surrounding liveness loop — which re-checks its own deadline only *between*
/// iterations — could never reach that deadline. The last thing CI printed was
/// libtest's `test export_unmounts_on_handle_drop has been running for over 60
/// seconds`.
///
/// ## Why it outlives the fix for that race
///
/// Since #1423 there is no window for a call to land in. Every connection this
/// suite hands a `SharedConnection` comes from [`connect`], which goes through
/// `hytte-bus`'s `build_pooled` — the same function production connections go
/// through — and that stages a placeholder interface so zbus's own barrier
/// applies: the socket reader does not start until the dispatch task is
/// registered. (Before #1423, `connection.rs`'s `begin_dispatching` only
/// *narrowed* it; the #1011 campaign still lost the first `Hello` in 94 of 900
/// runs on this suite's constructor, and every one was answered on the next
/// attempt.)
///
/// This bound stays anyway, because what it guards is not that one mechanism
/// but **any** call that goes unanswered: a regression of #1423's barrier (the
/// control tree in #1423's campaign is exactly that), a server a test builds
/// without [`connect`], or a broker that wedges. Unbounded, each of those is
/// #1011's signature again — a silent binary and a job timeout. Bounded, it is
/// [`PROBE_BUDGET`] and a failure that names the call.
///
/// A call that gets no reply is still **retried** rather than failed, and what
/// that arm absorbs now is a reply that is merely slow under CI contention, not
/// a swallowed one. That is what lets [`CALL_BUDGET`] be short. It is
/// emphatically **not** a latency assertion — nothing here claims a D-Bus call
/// *should* answer within a second (a healthy run answers in ~1 ms).
/// [`PROBE_BUDGET`] — 20 attempts' worth — is what decides that something is
/// actually broken. Tightening `PROBE_BUDGET` is what would buy false reds
/// under CI contention; tightening `CALL_BUDGET` only costs extra retries. See
/// [`DBUS_DAEMON_STARTUP_BUDGET`] above for the same distinction stated at
/// length.
///
/// Because this loop retries, it does **not** notice a dropped call — which is
/// right for a liveness guard and wrong for a test of the barrier.
/// `tests/ready.rs` is where "no call is ever dropped" is asserted, with its own
/// budget and no retry on silence.
///
/// ## See also
///
/// This is the third place in the tree that writes this zbus mechanism down.
/// `hytte-bus`'s own `connection.rs`, at `build_pooled`, has the barrier, the
/// zbus 5.14.0 line numbers and what `begin_dispatching` measured before it;
/// `hytte-services`' `wifi/nm_agent.rs` (`mount_and_proxy`, #714/#743/#756)
/// derived the mechanism first and stages its agent through the same barrier.
/// All three point at each other, because three uncoordinated transcriptions of
/// one upstream behaviour is how drift starts.
#[allow(dead_code)] // not every test binary that pulls in `common` makes raw calls
pub const CALL_BUDGET: Duration = Duration::from_secs(1);

/// Upper bound on a whole liveness loop built out of [`CALL_BUDGET`] calls —
/// the deadline that decides a peer is really not answering. See
/// [`CALL_BUDGET`] for why the two are sized against each other.
#[allow(dead_code)] // not every test binary that pulls in `common` makes raw calls
pub const PROBE_BUDGET: Duration = Duration::from_secs(20);

/// Error names the **broker** generates on its own, without the call ever
/// reaching the connection we are probing.
///
/// [`answers_a_method_call`] exists to tell "our dispatch task replied" apart
/// from "nobody replied". An error reply from the peer proves the first; these
/// two do not, because `dbus-daemon` answers them itself when the destination
/// name has no owner. Accepting them would let the probe pass on a connection
/// that is not dispatching at all.
pub const BROKER_GENERATED_ERRORS: [&str; 2] = [
    "org.freedesktop.DBus.Error.ServiceUnknown",
    "org.freedesktop.DBus.Error.NameHasNoOwner",
];

/// Does the connection whose unique name is `unique`, on the bus at `address`,
/// answer an inbound method call **at all**?
///
/// `Introspectable.Introspect` on `/` is the probe because zbus's object server
/// serves it from the root node with no interface mounted, so a reply proves
/// the dispatch task is live and proves nothing else.
///
/// Bounded and retried, exactly as [`CALL_BUDGET`] prescribes. On a connection
/// built by [`connect`] the first call is answered — #1423's barrier leaves no
/// window for it to be dropped in — so the retry only covers a slow reply. A
/// connection that never dispatches (one built without the barrier, with
/// nothing ever exported on it) costs the whole [`PROBE_BUDGET`] and returns
/// `false`.
///
/// Returns `(answered, unanswered)` — the second is how many calls got no reply
/// at all, for the caller's failure message.
///
/// This lives in `common` rather than in the test file so that every test that
/// asks "does this connection answer at all" means the same thing by it.
#[allow(dead_code)] // not every test binary that pulls in `common` probes a peer
pub async fn answers_a_method_call(address: &str, unique: &str) -> (bool, u32) {
    let client = Builder::address(address)
        .expect("parse ephemeral bus address")
        .build()
        .await
        .expect("client connection");
    let proxy = zbus::Proxy::new(&client, unique, "/", "org.freedesktop.DBus.Introspectable")
        .await
        .expect("client proxy");

    let deadline = tokio::time::Instant::now() + PROBE_BUDGET;
    let mut unanswered = 0u32;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(CALL_BUDGET, proxy.call::<_, _, String>("Introspect", &())).await
        {
            Ok(Ok(xml)) => {
                assert!(
                    xml.contains("org.freedesktop.DBus.Introspectable"),
                    "unexpected introspection reply: {xml}"
                );
                return (true, unanswered);
            }
            // An error reply *from the peer* is still proof its dispatch task
            // is live — it ran, and refused. Deliberately not `Ok(Err(_))`:
            // that would also accept a broker-generated `ServiceUnknown` (the
            // daemon answering for a destination that does not exist) and a
            // client-side transport error on the *probing* connection, neither
            // of which says anything about the peer. This is the one test whose
            // whole job is to tell those two apart.
            Ok(Err(zbus::Error::MethodError(name, _, _)))
                if !BROKER_GENERATED_ERRORS.contains(&name.as_str()) =>
            {
                return (true, unanswered);
            }
            // A broker-generated error, or a transport error on our own side:
            // not proof either way. Retry.
            Ok(Err(_)) => {}
            Err(_elapsed) => unanswered += 1,
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    (false, unanswered)
}

/// Upper bound on reaping the ephemeral `dbus-daemon` in [`BusGuard`]'s `Drop`.
///
/// #1011's diagnosis named **two** things in this crate's tests that can park a
/// test forever, and the raw `zbus::Proxy::call` that [`CALL_BUDGET`] covers was
/// only one of them. The other is this drop: `block_in_place` plus a nested
/// `block_on` on [`tokio::process::Child::wait`], with nothing bounding it.
///
/// Nothing reached it in the 2,400 instrumented runs the PR for #1011 measured,
/// which is why it is a backstop rather than a fix. But an unbounded wait here
/// would be silent in exactly the way the unbounded call was — the test body has
/// already finished, libtest has not yet printed a result, and CI shows nothing
/// at all — so leaving the suite's *other* forever-park unbounded would leave
/// #1011's signature reachable by a second door. Bounded, a reap that wedges
/// costs an abandoned child (already `SIGKILL`ed, and spawned with
/// `kill_on_drop(true)`) instead of the whole job's timeout.
///
/// ## The abandonment is silent in CI, and that is on purpose here
///
/// An earlier version of this doc promised the wedged case "costs one line on
/// stderr". It does not, where it matters: libtest captures `eprintln!`
/// per test and discards it for a *passing* test, and
/// `nix/checks/system-tests.nix` runs `cargo test` without `--nocapture`.
/// Measured with this budget mutated to 1 ns so the timeout arm fires on every
/// guard: 0 occurrences of the warning in a plain run, 10 under `--nocapture`,
/// suite green both times. The line is kept because it is what a developer
/// running the suite by hand with `--nocapture` sees, but it is **not** the
/// guard's signal in CI — the signal in CI is that the job finishes at all.
///
/// What *is* pinned, in both directions, is the bound itself:
/// `connection_basic.rs`'s
/// `a_reap_that_does_not_finish_within_its_budget_is_abandoned` drives
/// [`reap_within`] over a live child (budget hit → `false`, and it returns
/// rather than parking) and over a `SIGKILL`ed one (budget not hit → `true`).
/// Faking either answer, or flipping the seam's polarity, reds it by name. The
/// *magnitude* of this constant is deliberately not asserted — see that test's
/// doc for the measurement and why.
///
/// Sized like [`DBUS_DAEMON_STARTUP_BUDGET`] and for the same reason: it is a
/// liveness guard, not a latency assertion. Reaping a `SIGKILL`ed local process
/// takes microseconds; 30 s is headroom for a starved runner, not for a slow
/// path.
pub const DAEMON_REAP_BUDGET: Duration = Duration::from_secs(30);

/// Wait for `child` to be reaped, giving up after `budget`. Returns whether it
/// was reaped.
///
/// The seam every reap in this module goes through — [`BusGuard`]'s `Drop` and
/// [`restart_on_same_address`] — so the two cannot drift, and so
/// [`DAEMON_REAP_BUDGET`] has one place a test can exercise both of its arms.
/// See that constant for why the bound exists and what is and is not
/// observable when it fires.
pub async fn reap_within(child: &mut Child, budget: Duration) -> bool {
    tokio::time::timeout(budget, child.wait()).await.is_ok()
}

pub struct BusGuard {
    child: Option<Child>,
    tmp: TempDir,
    // Used by connection_reconnect.rs to open a replacement connection against
    // the same ephemeral bus; not all test binaries need it.
    #[allow(dead_code)]
    pub address: String,
}

impl Drop for BusGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // Send SIGKILL then block until the daemon is fully reaped, so the
            // TempDir (socket directory) is not removed before the process exits.
            // block_in_place suspends async scheduling on this thread, allowing
            // a nested block_on without the "cannot block inside async" panic.
            //
            // Bounded by DAEMON_REAP_BUDGET — see its doc: this is the second of
            // the two forever-parks #1011's diagnosis named, and an unbounded
            // wait here is silent in exactly the way the unbounded call was.
            let _ = child.start_kill();
            tokio::task::block_in_place(|| {
                let handle = tokio::runtime::Handle::current();
                // This call site's bound is guaranteed only by the tree-wide
                // invariant that `reap_within` is the sole caller of the raw
                // `Child::wait` future — enforced by grep, not by
                // connection_basic.rs's seam test, which pins `reap_within`
                // in isolation and cannot see this call site (#1392 re-review
                // LOW 1).
                if !handle.block_on(reap_within(&mut child, DAEMON_REAP_BUDGET)) {
                    // Only visible under `--nocapture`; see DAEMON_REAP_BUDGET.
                    eprintln!(
                        "common::BusGuard: the ephemeral dbus-daemon did not exit within \
                         {DAEMON_REAP_BUDGET:?} of SIGKILL; abandoning the reap rather than \
                         hanging the test (#1011)"
                    );
                }
            });
        }
    }
}

/// Write the `dbus-daemon` session config listening on `address` into `tmp`,
/// returning the config file's path. Split out of [`ephemeral_bus`] so
/// [`restart_on_same_address`] (#1175) can reuse the identical config — same
/// address, same policy — for a *second* daemon boot in the same `TempDir`.
fn write_session_conf(tmp: &TempDir, address: &str) -> PathBuf {
    let config = tmp.path().join("session.conf");
    std::fs::write(
        &config,
        format!(
            r#"<?xml version="1.0"?>
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>{address}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#
        ),
    )
    .expect("write dbus-daemon config");
    config
}

/// Spawn `dbus-daemon` against `config` and block until it has printed its
/// listen address (proof it is up and accepting connections). Shared by
/// [`ephemeral_bus`] (first boot) and [`restart_on_same_address`] (#1175: a
/// second boot on the identical socket path, standing in for a real
/// system/session `dbus-daemon` restarting under its supervisor while the
/// address it's configured to listen on stays fixed).
async fn spawn_daemon(config: &Path) -> Child {
    let mut child = Command::new("dbus-daemon")
        .arg("--config-file")
        .arg(config)
        .arg("--print-address=1")
        .arg("--nofork")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn dbus-daemon — install package `dbus` if missing");

    // Read the printed address from stdout to confirm the daemon is up.
    let stdout = child.stdout.take().expect("dbus-daemon stdout");
    let mut lines = BufReader::new(stdout).lines();
    let printed = tokio::time::timeout(DBUS_DAEMON_STARTUP_BUDGET, lines.next_line())
        .await
        .expect("dbus-daemon address timeout")
        .expect("dbus-daemon read address")
        .expect("dbus-daemon closed stdout");
    assert!(
        printed.contains("unix:path="),
        "unexpected dbus-daemon address: {printed}"
    );

    child
}

/// Connect to `address` the way `hytte-bus` connects in production — through
/// [`hytte_bus::test_support::connect`], i.e. `build_pooled`, so the
/// connection serves the `mov.vibec0re.hytte.Ready` placeholder and its object
/// server is dispatching before it reads a single message (#1423).
///
/// Use this for every connection a test hands to a `SharedConnection` (the one
/// [`ephemeral_bus`] returns, a reconnect's replacement), so the suite exercises
/// the connections production builds. A *peer* — a server a test stands up to
/// be talked to, or a client that probes us — is not a `hytte-bus` connection
/// and stays a plain `zbus::connection::Builder`.
pub async fn connect(address: &str) -> Connection {
    hytte_bus::test_support::connect(address)
        .await
        .expect("connect to ephemeral bus")
}

/// Spawn a fresh dbus-daemon, return a connection to it plus a guard
/// that kills the daemon on drop.
pub async fn ephemeral_bus() -> (Connection, BusGuard) {
    let tmp = TempDir::new().expect("create tempdir for dbus-daemon");
    let socket: PathBuf = tmp.path().join("bus");
    let address = format!("unix:path={}", socket.display());

    let config = write_session_conf(&tmp, &address);
    let child = spawn_daemon(&config).await;
    let conn = connect(&address).await;

    (
        conn,
        BusGuard {
            child: Some(child),
            tmp,
            address,
        },
    )
}

/// Kill the `dbus-daemon` behind `guard` and boot a fresh one listening on
/// the **identical** socket path (#1175) — standing in for a real
/// system/session bus restarting under its supervisor while every consumer's
/// configured address stays the same. Any `zbus::Connection` opened against
/// the old daemon (including the one returned by the original
/// [`ephemeral_bus`] call) is now talking to a dead peer and stays that way —
/// this does not, and cannot, reach into an existing `Connection` and fix it
/// up. The `Connection` this returns is a brand new one, freshly dialled
/// against the new daemon; the returned `BusGuard` (same `TempDir`, new
/// child) replaces the caller's old guard, which must not be used again.
#[allow(dead_code)] // not every test binary that pulls in `common` needs a restart
pub async fn restart_on_same_address(mut guard: BusGuard) -> (Connection, BusGuard) {
    if let Some(mut child) = guard.child.take() {
        // Bounded by [`DAEMON_REAP_BUDGET`], through the same [`reap_within`]
        // seam as [`BusGuard`]'s `Drop`, and for the same reason: this is the
        // suite's *third* forever-park, the same call on the same kind of
        // child as the one bounded there, 120 lines up. It is awaited from
        // three test bodies (`connection_reconnect.rs`, `resubscribe.rs` x2),
        // so a park here produces precisely #1011's CI signature — `running 1
        // test`, no `test result:`, silence until the job timeout. Being a
        // plain `.await` rather than a nested `block_on` makes it harder to
        // notice, not safer.
        let _ = child.start_kill();
        // This call site's bound is guaranteed only by the tree-wide
        // invariant that `reap_within` is the sole caller of the raw
        // `Child::wait` future — enforced by grep, not by
        // connection_basic.rs's seam test, which pins `reap_within` in
        // isolation and cannot see this call site (#1392 re-review LOW 1).
        if !reap_within(&mut child, DAEMON_REAP_BUDGET).await {
            // Only visible under `--nocapture`; see DAEMON_REAP_BUDGET. The
            // stale-socket removal below is what lets the fresh daemon bind
            // the identical path even when the old one is still around.
            eprintln!(
                "common::restart_on_same_address: the old dbus-daemon did not exit within \
                 {DAEMON_REAP_BUDGET:?} of SIGKILL; abandoning the reap rather than hanging \
                 the test (#1011)"
            );
        }
    }

    // dbus-daemon doesn't reliably unlink its own socket file on a SIGKILL;
    // remove any stale one so the fresh daemon can bind the identical path
    // instead of failing with EADDRINUSE.
    if let Some(socket_path) = guard.address.strip_prefix("unix:path=") {
        let _ = std::fs::remove_file(socket_path);
    }

    let config = guard.tmp.path().join("session.conf");
    let child = spawn_daemon(&config).await;
    let conn = connect(&guard.address).await;

    guard.child = Some(child);
    (conn, guard)
}
