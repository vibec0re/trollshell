//! #1024 (a #1004 second-pass follow-up, N3): pins the two `serve`-level
//! mechanisms the review's own throwaway harness proved but that never landed
//! in the tree — the reviewer measured that deleting either one left the
//! *unit* test suite green, because the internal `take_socket` tests drive
//! the decision with a local `owned` parameter, which cannot see whether
//! `serve` actually threads the process-wide listener through at all.
//!
//! Both scenarios need a real second OS process, for two independent reasons:
//!
//! - [`broker::SOCKET`] and [`broker::STOOD_DOWN`] are process-wide statics
//!   (deliberately — see their doc comments in `src/broker.rs`), so two
//!   scenarios sharing one test binary process would contaminate each other's
//!   "process never probes its own listener" decision.
//! - `XDG_RUNTIME_DIR` has to be scoped per scenario too, and
//!   `std::env::set_var` is `unsafe` in edition 2024 (this workspace forbids
//!   `unsafe_code` outright — see the root `Cargo.toml`), so there is no safe
//!   in-process way to set it for only part of a test binary.
//!
//! So each scenario is a pair: a plain `#[tokio::test]` that re-executes this
//! same test binary (`std::env::current_exe`), filtered to exactly one
//! `_inner` test, with a scratch `XDG_RUNTIME_DIR`/`XDG_STATE_HOME` and a
//! marker env var set on the child via `Command::env` (a safe builder method
//! — no `unsafe` needed to control a *child's* environment). The `_inner`
//! test does nothing at all unless that marker is present, so a normal
//! top-level `cargo test` run — which discovers `_inner` tests too — doesn't
//! try to run them with no `XDG_RUNTIME_DIR` set up. This is the same shape
//! `trollshell/src/plugins/tests.rs` uses for
//! `detached_launch_falls_back_without_a_user_manager`.

use std::path::Path;
use std::time::Duration;

use hytte_plugin_infobroker::grants::{Grant, to_toml};
use hytte_plugin_infobroker::paths::{GRANTS_FILE, SOCKET_FILE, STATE_DIR};
use hytte_plugin_infobroker::{BrokerMsg, BrokerSnapshot, serve};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Set (to any value) only on the re-exec'd child that is meant to actually
/// run one scenario's `_inner` test; see the module doc.
const SCENARIO_MARKER: &str = "INFOBROKER_TEST_SCENARIO";

/// Mirrors the SDK's own redial backoff (`hytte-plugin::runtime::BACKOFF_BASE`
/// = 100 ms) — how soon the SDK starts session N+1 after session N's lane
/// closes. Named here rather than imported: this crate is deliberately
/// SDK-free (see `src/lib.rs`), and the value is public knowledge documented
/// on [`broker::SOCKET`] itself.
const BACKOFF_BASE: Duration = Duration::from_millis(100);

/// There is no in-process observable for "the server has called `accept()`
/// on this exact connection and is now blocked reading its request line" —
/// seeing that would mean instrumenting `broker::handle_conn`'s private
/// state, out of this issue's lane. A generous, named, timing-based sleep is
/// the same shape #1004's own review harness used to establish this
/// precondition (its PR description cites "the harness's 250 ms"). The
/// scheduler latency this is actually waiting out is single-digit
/// milliseconds even under the 16-burner load campaign (see the PR body);
/// 300 ms leaves ample headroom.
const PARK_SETTLE: Duration = Duration::from_millis(300);

/// How long session 2 is left running, with nothing unblocking session 1,
/// before the test moves on to unblock it. Long enough that an unsynchronized
/// probe (mutation (a): the process-wide mutex replaced by a per-session
/// local) has certainly already run and observed session 1's still-live
/// listener — that race is won in microseconds, not milliseconds, so this is
/// generous headroom, not a tight measurement.
const PROBE_WINDOW: Duration = Duration::from_millis(300);

/// Upper bound for session 2's seed snapshot to land once session 1 is
/// unblocked. The real cost here is milliseconds (a mutex handoff); this
/// exists so a genuine regression is a named test failure within a bounded
/// wall-clock time rather than a hang.
const GIVE_UP: Duration = Duration::from_secs(5);

/// Upper bound for scenario C's session 2 to seed while session 1 is stuck
/// inside a blocked `write_response`. Deliberately generous and *not* pinned
/// to `broker::WRITE_RESPONSE_TIMEOUT` (private, currently 2 s) — the
/// property under test is that the block is bounded at all, not its exact
/// duration, so a future retune of that constant doesn't require touching
/// this test too.
const WRITE_PARK_GIVE_UP: Duration = Duration::from_secs(10);

/// How many `always` grants to pre-seed for scenario C: enough that a
/// `{"op":"grants"}` response is several MB of JSON, comfortably larger than
/// any default Linux UDS socket buffer, so a client that never reads it
/// genuinely blocks the write instead of the whole reply sliding into kernel
/// slack and returning instantly regardless of any bound.
const HUGE_GRANT_COUNT: usize = 80_000;

// ── Shared harness ──────────────────────────────────────────────────────────

/// Re-execute this test binary, filtered to exactly `inner_test_name`, with
/// `runtime_dir`/`state_dir` as `XDG_RUNTIME_DIR`/`XDG_STATE_HOME` and
/// [`SCENARIO_MARKER`] set. Panics with the child's stdout+stderr on a
/// non-zero exit, so a mutation shows up as a named test failure rather than
/// a silent skip.
fn run_inner(inner_test_name: &str, runtime_dir: &Path, state_dir: &Path) {
    let exe = std::env::current_exe().expect("this test binary's own path");
    let out = std::process::Command::new(exe)
        .args([
            "--exact",
            "--nocapture",
            "--test-threads=1",
            inner_test_name,
        ])
        .env(SCENARIO_MARKER, "1")
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_STATE_HOME", state_dir)
        .output()
        .expect("re-exec this test binary for a hermetic scenario");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "{inner_test_name} failed.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
    );
}

/// True only inside a scenario's re-exec'd child — see the module doc.
fn in_scenario_child() -> bool {
    std::env::var_os(SCENARIO_MARKER).is_some()
}

/// Wait for the next [`BrokerMsg::Update`], skipping any interleaved
/// `RequestConsent`/`Query` message. None of these scenarios trigger either.
async fn recv_update(out: &mut mpsc::UnboundedReceiver<BrokerMsg>) -> Option<BrokerSnapshot> {
    loop {
        match out.recv().await? {
            BrokerMsg::Update { snapshot, .. } => return Some(snapshot),
            BrokerMsg::RequestConsent(_) | BrokerMsg::Query(_) => {}
        }
    }
}

/// Write one request line (the wire's JSON-lines framing).
async fn send_line(stream: &mut UnixStream, line: &str) {
    stream
        .write_all(line.as_bytes())
        .await
        .expect("write the request body");
    stream.write_all(b"\n").await.expect("write the newline");
}

/// Read one response line, or `None` on a clean EOF.
async fn read_line(stream: &mut UnixStream) -> Option<String> {
    let mut reader = tokio::io::BufReader::new(stream);
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .await
        .expect("read the response line");
    (n > 0).then_some(line)
}

/// Write `HUGE_GRANT_COUNT` `always` grants to `state_dir`'s `grants.toml`
/// (scenario C's oversized-response setup), via the crate's own public
/// `Grant`/`to_toml` round-trip rather than hand-rolled TOML text.
fn seed_large_grants(state_dir: &Path) {
    let grants: Vec<Grant> = (0..HUGE_GRANT_COUNT)
        .map(|i| Grant::always(format!("agent-{i:06}"), "departures"))
        .collect();
    let toml_text = to_toml(&grants).expect("encode the scratch grants.toml");
    let dir = state_dir.join(STATE_DIR);
    std::fs::create_dir_all(&dir).expect("create the scratch state dir");
    std::fs::write(dir.join(GRANTS_FILE), toml_text).expect("write the scratch grants.toml");
}

// ── Scenario A: a session handover while the predecessor is parked ─────────

/// #1024 / #995 M1: the exact `systemctl --user restart trollshell` shape —
/// session 1 binds, a client connects and parks `handle_conn` (never sends
/// its request line), session 1's `cmds` lane closes while it is still
/// parked, and session 2 starts within the SDK's `BACKOFF_BASE`. Session 2
/// must come up `Kept` (never having probed its own predecessor) and the
/// socket must stay accepting across the handover.
///
/// RED under mutation (a) — `SOCKET.lock().await` replaced by a per-session
/// local: session 2 then probes while session 1's listener is still alive,
/// reads that as a foreign broker, and stands down against itself (the exact
/// #995 bug this test exists to pin).
#[tokio::test]
async fn a_session_handover_survives_a_predecessor_parked_in_read() {
    let runtime_dir = tempfile::tempdir().expect("XDG_RUNTIME_DIR scratch dir");
    let state_dir = tempfile::tempdir().expect("XDG_STATE_HOME scratch dir");
    run_inner(
        "a_session_handover_survives_a_predecessor_parked_in_read_inner",
        runtime_dir.path(),
        state_dir.path(),
    );
}

#[tokio::test]
async fn a_session_handover_survives_a_predecessor_parked_in_read_inner() {
    if !in_scenario_child() {
        return;
    }
    let sock_path =
        hytte_plugin_infobroker::paths::socket_path().expect("XDG_RUNTIME_DIR set by the harness");

    // Session 1: binds cleanly.
    let (cmds1_tx, cmds1_rx) = mpsc::unbounded_channel();
    let (out1_tx, mut out1_rx) = mpsc::unbounded_channel::<BrokerMsg>();
    let session1 = tokio::spawn(serve(cmds1_rx, out1_tx));

    let snap1 = recv_update(&mut out1_rx)
        .await
        .expect("session 1's lane produced a snapshot");
    assert_eq!(snap1.notice, None, "session 1 must bind cleanly: {snap1:?}");

    // A client connects and sends nothing: parks session 1's `handle_conn`
    // inside `REQUEST_TIMEOUT`.
    let mut parked_client = UnixStream::connect(&sock_path)
        .await
        .expect("connect to session 1");
    tokio::time::sleep(PARK_SETTLE).await;

    // Session 1's lane closes while the client is still parked.
    drop(cmds1_tx);
    tokio::time::sleep(BACKOFF_BASE).await;

    // Session 2 starts — within the SDK's own backoff, while session 1 is
    // still alive and holding the listener.
    let (cmds2_tx, cmds2_rx) = mpsc::unbounded_channel();
    let (out2_tx, mut out2_rx) = mpsc::unbounded_channel::<BrokerMsg>();
    let session2 = tokio::spawn(serve(cmds2_rx, out2_tx));

    // Give session 2 a window to attempt (and, under mutation (a), complete)
    // an unsynchronized probe of session 1's still-live listener before this
    // test unblocks session 1.
    tokio::time::sleep(PROBE_WINDOW).await;

    // The socket must keep accepting across the handover, even while session
    // 1 is still parked servicing the first client.
    let still_accepting =
        tokio::time::timeout(Duration::from_secs(1), UnixStream::connect(&sock_path)).await;
    assert!(
        matches!(still_accepting, Ok(Ok(_))),
        "the socket must keep accepting during the handover window: {still_accepting:?}",
    );

    // Unblock session 1's parked client so session 1 observes its closed lane
    // and returns, releasing the process-wide SOCKET guard for session 2.
    send_line(&mut parked_client, r#"{"op":"grants"}"#).await;
    let _ = read_line(&mut parked_client).await;

    let snap2 = tokio::time::timeout(GIVE_UP, recv_update(&mut out2_rx))
        .await
        .expect("session 2 must seed within the bounded handover window — RED under mutation (a)")
        .expect("session 2's lane produced a snapshot");
    assert_eq!(
        snap2.notice, None,
        "session 2 must come up Kept, not stand down against its own predecessor: {snap2:?}",
    );

    // And the socket really is live under session 2: a fresh client gets a
    // real answer.
    let mut fresh_client = UnixStream::connect(&sock_path)
        .await
        .expect("connect to session 2");
    send_line(&mut fresh_client, r#"{"op":"grants"}"#).await;
    let reply = read_line(&mut fresh_client)
        .await
        .expect("session 2 answers a real request");
    assert!(
        reply.contains("\"ok\":true"),
        "session 2 must answer a real request: {reply}",
    );

    drop(cmds2_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), session1).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), session2).await;
}

// ── Scenario B: a genuinely foreign owner ───────────────────────────────────

/// #1024 / #995: with no process-owned listener yet (a fresh process) and a
/// *real* foreign listener already bound on the socket path, `serve` must
/// stand down and the panel-bound [`BrokerSnapshot::notice`] must explain why.
///
/// RED under mutation (b) — `state.notice = Some(…)` deleted from `serve`'s
/// `StoodDown` arm: the decision is still correctly `StoodDown` (nothing
/// unlinks the foreign socket, per the existing `bind_socket` unit tests),
/// but nothing tells the panel, so this assertion fails.
#[tokio::test]
async fn a_foreign_listener_gets_stood_down_with_the_notice_rendered() {
    let runtime_dir = tempfile::tempdir().expect("XDG_RUNTIME_DIR scratch dir");
    let state_dir = tempfile::tempdir().expect("XDG_STATE_HOME scratch dir");
    let sock_path = runtime_dir.path().join(SOCKET_FILE);
    let foreign = tokio::net::UnixListener::bind(&sock_path).expect("bind the foreign listener");

    run_inner(
        "a_foreign_listener_gets_stood_down_with_the_notice_rendered_inner",
        runtime_dir.path(),
        state_dir.path(),
    );

    // Kept alive across the whole child run above — dropped only now, so the
    // child's probe always finds it live.
    drop(foreign);
}

#[tokio::test]
async fn a_foreign_listener_gets_stood_down_with_the_notice_rendered_inner() {
    if !in_scenario_child() {
        return;
    }
    let (cmds_tx, cmds_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<BrokerMsg>();
    let session = tokio::spawn(serve(cmds_rx, out_tx));

    let snap = tokio::time::timeout(GIVE_UP, recv_update(&mut out_rx))
        .await
        .expect("a stood-down session must still seed a panel snapshot")
        .expect("the lane produced a snapshot");

    let notice = snap.notice.expect(
        "a foreign live listener must produce a stand-down notice — RED under mutation (b)",
    );
    assert!(
        notice.contains("another info broker"),
        "the notice must explain the stand-down: {notice}",
    );

    drop(cmds_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), session).await;
}

// ── Scenario C: a client that never reads a large response ─────────────────

/// #1024 N4: a client that connects, sends a real request, and then never
/// reads its (deliberately huge) reply must not delay a *later* session's
/// seed beyond `write_response`'s own bound — the process-wide `SOCKET`
/// mutex means an unbounded write would otherwise park every subsequent
/// session right along with this one.
///
/// RED if `write_response`'s internal `tokio::time::timeout` is removed:
/// session 1 never returns, session 2 never seeds, and this test's own
/// [`WRITE_PARK_GIVE_UP`] safety net fires — a bounded failure, not a hang.
#[tokio::test]
async fn a_client_that_never_reads_does_not_delay_the_next_sessions_seed() {
    let runtime_dir = tempfile::tempdir().expect("XDG_RUNTIME_DIR scratch dir");
    let state_dir = tempfile::tempdir().expect("XDG_STATE_HOME scratch dir");
    seed_large_grants(state_dir.path());
    run_inner(
        "a_client_that_never_reads_does_not_delay_the_next_sessions_seed_inner",
        runtime_dir.path(),
        state_dir.path(),
    );
}

#[tokio::test]
async fn a_client_that_never_reads_does_not_delay_the_next_sessions_seed_inner() {
    if !in_scenario_child() {
        return;
    }
    let sock_path =
        hytte_plugin_infobroker::paths::socket_path().expect("XDG_RUNTIME_DIR set by the harness");

    let (cmds1_tx, cmds1_rx) = mpsc::unbounded_channel();
    let (out1_tx, mut out1_rx) = mpsc::unbounded_channel::<BrokerMsg>();
    let session1 = tokio::spawn(serve(cmds1_rx, out1_tx));

    let snap1 = recv_update(&mut out1_rx)
        .await
        .expect("session 1's lane produced a snapshot");
    assert_eq!(snap1.notice, None, "session 1 must bind cleanly: {snap1:?}");

    // A client connects, asks for the (huge, pre-seeded) grants list, and
    // then never reads the reply: `write_response` blocks on the client's
    // unread kernel buffer.
    let mut stuck_client = UnixStream::connect(&sock_path)
        .await
        .expect("connect to session 1");
    send_line(&mut stuck_client, r#"{"op":"grants"}"#).await;
    tokio::time::sleep(PARK_SETTLE).await;

    drop(cmds1_tx);
    tokio::time::sleep(BACKOFF_BASE).await;

    let (cmds2_tx, cmds2_rx) = mpsc::unbounded_channel();
    let (out2_tx, mut out2_rx) = mpsc::unbounded_channel::<BrokerMsg>();
    let session2 = tokio::spawn(serve(cmds2_rx, out2_tx));

    let snap2 = tokio::time::timeout(WRITE_PARK_GIVE_UP, recv_update(&mut out2_rx))
        .await
        .expect(
            "session 2 must seed within write_response's bound — RED if that timeout is removed",
        )
        .expect("session 2's lane produced a snapshot");
    assert_eq!(snap2.notice, None, "session 2 must come up Kept: {snap2:?}");

    drop(cmds2_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), session1).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), session2).await;

    // Held open (never read) for the whole test on purpose — dropped only
    // now that both sessions are done with it.
    drop(stuck_client);
}
