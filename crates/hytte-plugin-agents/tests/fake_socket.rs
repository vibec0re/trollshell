//! The fake `host.sock`: a real `UnixListener` in a tempdir speaking the
//! recorded JSON lines under `tests/fixtures/`.
//!
//! This is the whole reason P1 is buildable today (spec §13): the plugin can
//! be finished, tested and reviewed before a hive exists on the laptop. Every
//! test here drives the **real** client — connect, one JSON line out, one JSON
//! line back — against a **real** unix socket; nothing is stubbed but the
//! daemon's answers.
//!
//! # Why these are not `system-tests`-gated
//!
//! Spec §12 files the fake-socket case under the `system-tests` bucket. It
//! does not belong there: that feature exists for tests needing a
//! `dbus-daemon` or a display server (`CLAUDE.md`, "Tests"), and a
//! `UnixListener` in a `TempDir` needs neither. Gating them would mean the
//! **drift detector does not run** during `nix build .#trollshell`'s
//! `doCheck`, which deliberately omits `system-tests` — i.e. exactly the run
//! where a wire change should go red. So they ride the default bucket, and
//! `cargo test -p hytte-plugin-agents` is hermetic.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hytte_plugin_agents::hive::client::{HiveError, request};
use hytte_plugin_agents::hive::wire::{HOST_SOCK_VERSION, Request, Scope, VersionMismatch};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

// ── fixtures ─────────────────────────────────────────────────────────────────

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing fixture {} ({e})", path.display()))
        .trim()
        .to_owned()
}

// ── the fake hive ────────────────────────────────────────────────────────────

/// A `host.sock` stand-in. Records every request line it is sent, and answers
/// each from a `cmd` → reply table.
struct FakeHive {
    /// Kept alive so the socket's directory outlives the listener.
    _dir: tempfile::TempDir,
    path: PathBuf,
    seen: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeHive {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeHive {
    /// Bind at `<tmp>/host.sock` and serve `replies`, keyed by the request's
    /// `cmd` tag. A `cmd` with no entry gets a bare success.
    fn serve(replies: HashMap<&'static str, String>) -> Self {
        let dir = tempfile::tempdir().expect("a tempdir");
        let path = dir.path().join("host.sock");
        let listener = UnixListener::bind(&path).expect("bind the fake host.sock");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn(accept_loop(listener, replies, Arc::clone(&seen)));
        Self {
            _dir: dir,
            path,
            seen,
            task,
        }
    }

    /// The socket path a client dials.
    fn path(&self) -> &Path {
        &self.path
    }

    /// Every request line the fake has been sent, in order.
    fn seen(&self) -> Vec<String> {
        self.seen
            .lock()
            .expect("the recorder is never poisoned")
            .clone()
    }
}

async fn accept_loop(
    listener: UnixListener,
    replies: HashMap<&'static str, String>,
    seen: Arc<Mutex<Vec<String>>>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        // One request/response per connection, exactly like the real daemon
        // (and exactly what the client's one-connection-per-request model
        // expects).
        serve_one(stream, &replies, &seen).await;
    }
}

async fn serve_one(
    stream: UnixStream,
    replies: &HashMap<&'static str, String>,
    seen: &Arc<Mutex<Vec<String>>>,
) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    if reader.read_line(&mut line).await.is_err() {
        return;
    }
    let line = line.trim().to_owned();
    let cmd = serde_json::from_str::<serde_json::Value>(&line)
        .ok()
        .and_then(|v| v.get("cmd").and_then(|c| c.as_str()).map(str::to_owned))
        .unwrap_or_default();
    seen.lock()
        .expect("the recorder is never poisoned")
        .push(line);

    let reply = replies
        .get(cmd.as_str())
        .cloned()
        .unwrap_or_else(|| r#"{"version":1,"ok":true}"#.to_owned());
    let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
    let _ = write.flush().await;
}

fn replies(pairs: &[(&'static str, &str)]) -> HashMap<&'static str, String> {
    pairs
        .iter()
        .map(|(cmd, body)| (*cmd, (*body).to_owned()))
        .collect()
}

// ── the tests ────────────────────────────────────────────────────────────────

/// The end-to-end round trip: a real socket, a recorded answer, the real
/// client, and the mirror decoding the roster the hive would actually send.
#[tokio::test]
async fn a_recorded_agent_status_round_trips_through_the_real_client() {
    let hive = FakeHive::serve(replies(&[(
        "agent_status",
        &fixture("agent_status_grouped.json"),
    )]));

    let resp = request(hive.path(), &Request::AgentStatus)
        .await
        .expect("the fixture is a good answer");
    assert_eq!(resp.version, HOST_SOCK_VERSION);
    let rows = resp.agent_statuses.expect("a roster");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].name, "trollshell-choom");
    assert_eq!(rows[0].status_text.as_deref(), Some("reviewing PR #947"));
    assert_eq!(
        rows[0].status_set_at.as_deref(),
        Some("2026-09-07T12:34:56Z")
    );
    assert_eq!(rows[0].active_model.as_deref(), Some("claude-opus-4-6"));
    assert_eq!(
        rows[0].url.as_deref(),
        Some("https://hive.local/agent/trollshell-choom/")
    );
    assert!(rows[1].paused);
    assert!(rows[1].needs_update);
    assert_eq!(rows[1].parent.as_deref(), Some("trollshell-choom"));
    assert!(!rows[2].running);

    // And the wire the client actually put on the socket.
    assert_eq!(hive.seen(), vec![r#"{"cmd":"agent_status"}"#.to_owned()]);
}

/// `List` and `Urls` decode from their own recorded answers — the rest of the
/// verbs P1 mirrors, exercised against the same fake.
#[tokio::test]
async fn list_and_urls_decode_from_their_recorded_answers() {
    let hive = FakeHive::serve(replies(&[
        ("list", &fixture("list.json")),
        ("urls", &fixture("urls.json")),
    ]));

    let listed = request(hive.path(), &Request::List)
        .await
        .expect("List answers");
    assert_eq!(
        listed.agents.expect("names"),
        vec![
            "trollshell-choom".to_owned(),
            "nixos-choom".to_owned(),
            "stray".to_owned()
        ]
    );

    let urls = request(hive.path(), &Request::Urls)
        .await
        .expect("Urls answers")
        .urls
        .expect("a urls block");
    assert_eq!(urls.domain.as_deref(), Some("hive.local"));
    assert_eq!(urls.home.as_deref(), Some("https://hive.local/"));
}

/// Forward drift: a hive that grew fields — on the response, on the row, and
/// in shapes this build has no type for at all — still decodes.
///
/// Falsification: add `#[serde(deny_unknown_fields)]` to `Response` or
/// `AgentStatusRow` and this goes red.
#[tokio::test]
async fn a_hive_that_grew_fields_still_decodes() {
    let hive = FakeHive::serve(replies(&[(
        "agent_status",
        &fixture("agent_status_unknown_keys.json"),
    )]));
    let rows = request(hive.path(), &Request::AgentStatus)
        .await
        .expect("unknown keys must not fail the round trip")
        .agent_statuses
        .expect("a roster");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "drifted");
    assert_eq!(
        rows[0].status_text.as_deref(),
        Some("a hive that grew fields")
    );
}

/// A pre-version daemon (no `version` key) is read, not refused: every
/// mirrored field is `#[serde(default)]`, so an older subset decodes.
#[tokio::test]
async fn a_pre_version_daemon_is_read_not_refused() {
    let hive = FakeHive::serve(replies(&[(
        "agent_status",
        &fixture("agent_status_v0.json"),
    )]));
    let resp = request(hive.path(), &Request::AgentStatus)
        .await
        .expect("version 0 is older, not newer");
    assert_eq!(resp.version, 0);
    let rows = resp.agent_statuses.expect("a roster");
    assert_eq!(rows[0].name, "ancient");
    // Fields the old daemon never had read as their defaults, never as lies.
    assert!(!rows[0].paused);
    assert_eq!(rows[0].status_text, None);
    assert_eq!(rows[0].url, None);
}

/// A daemon **newer** than this build is refused rather than misread — spec
/// §5.2, "drift → refuse, not guess". Note the fixture's row says
/// `failed: true`: reading it would be the exact failure mode this guards, a
/// wedged agent presented as something else.
///
/// Falsification: relax `check_version` to always `Ok(())` and this goes red.
#[tokio::test]
async fn a_newer_daemon_is_refused_before_its_rows_are_read() {
    let hive = FakeHive::serve(replies(&[(
        "agent_status",
        &fixture("agent_status_v99.json"),
    )]));
    let err = request(hive.path(), &Request::AgentStatus)
        .await
        .expect_err("a newer wire must refuse");
    assert_eq!(
        err,
        HiveError::Version(VersionMismatch {
            theirs: 99,
            ours: HOST_SOCK_VERSION,
        })
    );
}

/// `ok: false` surfaces the daemon's own message, not a generic failure.
#[tokio::test]
async fn a_refused_request_carries_the_daemons_own_error() {
    let hive = FakeHive::serve(replies(&[("agent_status", &fixture("error.json"))]));
    let err = request(hive.path(), &Request::AgentStatus)
        .await
        .expect_err("ok:false is an error");
    match err {
        HiveError::Refused { reason } => {
            assert!(reason.contains("ghost"), "{reason}");
        }
        other => panic!("expected Refused, got {other:?}"),
    }
}

/// Spec §11 rule one, on the wire this time: the bytes the lifecycle verbs
/// actually put on the socket name exactly one agent.
///
/// Falsification: give `Scope` a `Default` and build `Start { scope:
/// Scope::default() }` here — the recorded line loses `agent_names` and the
/// assertion fails, which is the same frame that would have started the whole
/// hive.
#[tokio::test]
async fn the_control_verbs_put_their_pinned_bytes_on_the_socket() {
    let hive = FakeHive::serve(HashMap::new());

    for req in [
        Request::SetPaused {
            name: "trollshell-choom".to_owned(),
            paused: true,
        },
        Request::SetPaused {
            name: "trollshell-choom".to_owned(),
            paused: false,
        },
        Request::Start {
            scope: Scope::agent("trollshell-choom"),
        },
        Request::Stop {
            scope: Scope::agent("trollshell-choom"),
            graceful: true,
        },
    ] {
        request(hive.path(), &req).await.expect("the fake accepts");
    }

    assert_eq!(
        hive.seen(),
        vec![
            r#"{"cmd":"set_paused","name":"trollshell-choom","paused":true}"#.to_owned(),
            r#"{"cmd":"set_paused","name":"trollshell-choom","paused":false}"#.to_owned(),
            r#"{"cmd":"start","scope":{"agent_names":["trollshell-choom"]}}"#.to_owned(),
            r#"{"cmd":"stop","scope":{"agent_names":["trollshell-choom"]},"graceful":true}"#
                .to_owned(),
        ]
    );
}

/// No socket, no crash — and recovery when one appears. This is spec §5.3's
/// whole contract in one test: an absent path resolves to `Unreachable` with
/// an operator-facing reason, the client keeps working, and the *same* path
/// answers normally once a daemon binds it.
#[tokio::test]
async fn an_absent_socket_parks_and_the_same_path_recovers_when_it_appears() {
    let dir = tempfile::tempdir().expect("a tempdir");
    let path = dir.path().join("host.sock");

    // 1. Nothing there yet.
    let err = request(&path, &Request::AgentStatus)
        .await
        .expect_err("an absent socket cannot answer");
    match &err {
        HiveError::Unreachable { reason } => {
            assert!(reason.contains("not running"), "{reason}");
            assert!(
                !reason.contains("hive-admin"),
                "an absent socket must not blame the operator's group: {reason}"
            );
        }
        other => panic!("expected Unreachable, got {other:?}"),
    }

    // 2. The daemon comes up on the very same path.
    let listener = UnixListener::bind(&path).expect("bind after the fact");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let task = tokio::spawn(accept_loop(
        listener,
        replies(&[("agent_status", &fixture("agent_status_grouped.json"))]),
        seen,
    ));

    // 3. The client — which was never restarted — now gets a roster.
    let rows = request(&path, &Request::AgentStatus)
        .await
        .expect("the socket is live now")
        .agent_statuses
        .expect("a roster");
    assert_eq!(rows.len(), 3);
    task.abort();
}

/// A socket that exists but has nobody behind it is a *different* diagnosis
/// from an absent one, and the row must say the right thing.
#[tokio::test]
async fn a_dead_socket_file_blames_the_service_not_the_group() {
    let dir = tempfile::tempdir().expect("a tempdir");
    let path = dir.path().join("host.sock");
    {
        // Bind and immediately drop: the path stays, the listener does not.
        let listener = UnixListener::bind(&path).expect("bind");
        drop(listener);
    }
    // Some platforms unlink on drop; recreate the inode as a plain file if so,
    // which is the other way this looks in the wild (a stale path).
    if !path.exists() {
        std::fs::write(&path, b"").expect("leave a stale path behind");
    }

    let err = request(&path, &Request::AgentStatus)
        .await
        .expect_err("nobody is listening");
    match &err {
        HiveError::Unreachable { reason } => {
            assert!(
                !reason.contains("hive-admin"),
                "a dead socket is not a permission problem: {reason}"
            );
        }
        other => panic!("expected Unreachable, got {other:?}"),
    }
}

/// A daemon that accepts and then hangs must not wedge the poll loop: the
/// round trip is bounded by `REQUEST_TIMEOUT`.
///
/// Falsification: drop the `tokio::time::timeout` wrapper in
/// `client::request` and this test hangs instead of passing.
#[tokio::test(start_paused = true)]
async fn a_hung_daemon_times_out_instead_of_wedging_the_poll() {
    let dir = tempfile::tempdir().expect("a tempdir");
    let path = dir.path().join("host.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    // Accept, then never answer — and hold the connection so the client is
    // waiting on a read that will not complete, not on an EOF.
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });

    let err = request(&path, &Request::AgentStatus)
        .await
        .expect_err("a hung daemon must time out");
    match &err {
        HiveError::Unreachable { reason } => assert!(reason.contains("did not answer"), "{reason}"),
        other => panic!("expected Unreachable, got {other:?}"),
    }
    task.abort();
}

/// A daemon that reads the request and then closes cleanly, without
/// answering, is a **protocol** error — not a silently-empty roster, which is
/// the failure that would render "no agents" over a hive full of them.
///
/// (Its rude sibling — a reset mid-request — lands in `Unreachable` instead;
/// both park the row and neither crashes, which is the property that matters.
/// `a_reset_connection_parks_too` below covers that half.)
#[tokio::test]
async fn a_connection_closed_without_an_answer_is_a_protocol_error() {
    let dir = tempfile::tempdir().expect("a tempdir");
    let path = dir.path().join("host.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            // Read the request, then close the write half cleanly — the
            // client sees EOF, exactly as it would from a daemon that gave up
            // between reading and answering.
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            let mut line = String::new();
            let _ = reader.read_line(&mut line).await;
            let _ = write.shutdown().await;
        }
    });

    let err = request(&path, &Request::AgentStatus)
        .await
        .expect_err("an empty answer is not an answer");
    assert!(
        matches!(err, HiveError::Protocol { .. }),
        "expected Protocol, got {err:?}"
    );
    task.abort();
}

/// The rude close: the daemon vanishes before reading. It resolves to
/// `Unreachable`, which is the same "park the row and keep the cadence"
/// outcome — the point of this test is that it is an `Err`, never a panic and
/// never an empty success.
#[tokio::test]
async fn a_reset_connection_parks_too() {
    let dir = tempfile::tempdir().expect("a tempdir");
    let path = dir.path().join("host.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream); // accept, then hang up rudely
        }
    });

    let err = request(&path, &Request::AgentStatus)
        .await
        .expect_err("a reset is not an answer");
    assert!(
        matches!(
            err,
            HiveError::Unreachable { .. } | HiveError::Protocol { .. }
        ),
        "a hung-up socket must park, got {err:?}"
    );
    task.abort();
}
