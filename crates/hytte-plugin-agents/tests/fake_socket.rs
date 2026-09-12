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
use std::sync::{Arc, Mutex};

use hytte_plugin_agents::hive::client::{HiveError, REQUEST_TIMEOUT, request};
use hytte_plugin_agents::hive::wire::{
    ApprovalKind, ApprovalStatus, HOST_SOCK_VERSION, Request, Scope, VersionMismatch,
};
use hytte_plugin_agents::model::PendingApprovals;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixListener;

mod fake;
use fake::{FakeHive, accept_loop, fixture, replies};

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
/// round trip is bounded by `REQUEST_TIMEOUT`, and **by that budget**, not
/// merely "eventually".
///
/// Two things here are deliberate, and the earlier version of this test had
/// neither:
///
/// - The **outer** `tokio::time::timeout` is what makes deleting the client's
///   own timeout a *fast, named* red instead of a hang. A hang is not a red:
///   on CI it is a job timeout with no failing test name, and locally it is a
///   reviewer's lunch break.
/// - The **`elapsed` bounds** are what pin the budget. Under
///   `start_paused = true` the virtual clock auto-advances to whatever
///   deadline exists, so a test that only asserts "an error came back" passes
///   on a ten-year timeout just as happily as on five seconds.
///
/// Falsification: remove the `tokio::time::timeout` in `client::request` and
/// this fails, named, in hundredths of a second; widen `REQUEST_TIMEOUT` and
/// the `elapsed` assertion fails instead.
#[tokio::test(start_paused = true)]
async fn a_hung_daemon_times_out_on_its_stated_budget() {
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

    let start = tokio::time::Instant::now();
    let err = tokio::time::timeout(REQUEST_TIMEOUT * 4, request(&path, &Request::AgentStatus))
        .await
        .expect("the client must bound its own round trip, not rely on its caller")
        .expect_err("a hung daemon must time out");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= REQUEST_TIMEOUT && elapsed < REQUEST_TIMEOUT * 2,
        "the budget must be REQUEST_TIMEOUT, not merely 'eventually': {elapsed:?}"
    );

    match &err {
        HiveError::Unreachable { reason } => {
            assert!(reason.contains("did not answer"), "{reason}");
            assert!(
                reason.contains(&REQUEST_TIMEOUT.as_secs().to_string()),
                "the row should name the budget it gave up on: {reason}"
            );
        }
        other => panic!("expected Unreachable, got {other:?}"),
    }
    task.abort();
}

/// The budget's **value**, pinned — because the `elapsed` bounds above cannot
/// pin it.
///
/// Worth spelling out, because it is a trap: those bounds are stated
/// *relative to `REQUEST_TIMEOUT`*, so they move with the constant. Measured —
/// widening `REQUEST_TIMEOUT` from 5 s to 60 s leaves
/// `a_hung_daemon_times_out_on_its_stated_budget` **green**, because 60 s is
/// still "its stated budget". That test proves the client honours whatever
/// budget it declares; this one proves which budget that is.
///
/// The number matters against the poll cadence: long enough that a slow but
/// healthy round trip is not cut off, short enough that a wedged daemon costs
/// a couple of cadences rather than freezing the last-good roster on screen.
/// Changing it should be a deliberate edit with a new number here.
///
/// It also converts an absurd widening into an instant red: at ten years the
/// hung-daemon test does not fail, it *hangs* — tokio's timer wheel cannot
/// step a decade in one auto-advance — whereas this fails before any timer
/// runs.
#[test]
fn the_request_budget_is_five_seconds() {
    assert_eq!(
        REQUEST_TIMEOUT,
        std::time::Duration::from_secs(5),
        "the round-trip budget moved; update this and say why in the commit"
    );
}

/// `EACCES` on a socket that exists and is listening — the one connect branch
/// `docs/plugin-env.md` and the live-verify list both lean on, and the one
/// most likely to be met in the wild (a `hive-admin` member whose shell
/// predates the group grant). Exercised against a **real** `UnixStream::connect`
/// rather than only through `connect_reason`'s unit test, because the mapping
/// only matters if this is the `ErrorKind` the kernel actually produces.
///
/// Skipped, not failed, when the test user can open a mode-0000 file anyway —
/// `root` and `CAP_DAC_OVERRIDE` bypass the permission bits, and a sandbox
/// that runs tests as root would otherwise fail this for a reason that has
/// nothing to do with the code.
#[tokio::test]
async fn a_permission_denied_socket_names_the_group_not_the_daemon() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("a tempdir");
    let path = dir.path().join("host.sock");
    let listener = UnixListener::bind(&path).expect("bind");
    let task = tokio::spawn(async move { while listener.accept().await.is_ok() {} });

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
        .expect("chmod the socket");

    match request(&path, &Request::AgentStatus).await {
        Err(HiveError::Unreachable { reason }) => {
            assert!(
                reason.contains("hive-admin"),
                "EACCES must name the group, not the daemon: {reason}"
            );
            assert!(!reason.contains("not running"), "{reason}");
        }
        // Root ignores the mode bits; that is the environment, not a bug.
        Ok(_) => eprintln!("skipped: this user can connect to a 0000 socket (root?)"),
        Err(other) => panic!("expected Unreachable(EACCES), got {other:?}"),
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

// ── #947 P3: the approval queue, end to end ──────────────────────────────────

/// The `Pending` round trip through the **real** client against a recorded
/// answer: the queue decodes, the resolved row is filtered out, and the
/// remaining two come back oldest first.
///
/// This is the socket-level half of what `reducer.rs` asserts on the model —
/// so a fixture that stopped decoding would go red here even if the reducer's
/// hand-built `Approval`s still compiled.
#[tokio::test]
async fn a_recorded_pending_queue_round_trips_through_the_real_client() {
    let hive = FakeHive::serve(replies(&[("pending", &fixture("pending.json"))]));

    let resp = request(hive.path(), &Request::Pending)
        .await
        .expect("the fixture is a good answer");
    assert_eq!(resp.version, HOST_SOCK_VERSION);
    // Three rows on the wire…
    assert_eq!(resp.approvals.as_ref().expect("a queue").len(), 3);
    // …two of which are still waiting, oldest first once the model applies
    // its one filter (the wire mirror deliberately does not).
    let pending = PendingApprovals::new(resp.approvals.clone().unwrap_or_default());
    let pending = pending.all();
    assert_eq!(
        pending.iter().map(|a| a.id).collect::<Vec<_>>(),
        vec![7, 8],
        "the `approved` row must not reach a prompt"
    );
    assert_eq!(pending[0].agent, "trollshell-choom");
    assert_eq!(pending[0].kind, ApprovalKind::MergeConfigPr);
    assert_eq!(
        pending[0].description.as_deref(),
        Some("bump the meta flake inputs")
    );
    assert_eq!(pending[1].kind, ApprovalKind::Spawn);
    assert_eq!(pending[1].description, None);

    assert_eq!(hive.seen(), vec![r#"{"cmd":"pending"}"#.to_owned()]);
}

/// An empty queue is a legitimate answer, distinct from "the verb was not
/// asked": `approvals: Some(vec![])`, not `None`. The plugin draws no badge
/// either way, but only one of the two clears a badge that was there.
#[tokio::test]
async fn an_empty_queue_is_an_answer_not_an_absence() {
    let hive = FakeHive::serve(replies(&[("pending", &fixture("pending_empty.json"))]));

    let resp = request(hive.path(), &Request::Pending)
        .await
        .expect("an empty queue is a good answer");
    assert_eq!(resp.approvals, Some(Vec::new()));
    assert!(
        PendingApprovals::new(resp.approvals.unwrap_or_default())
            .all()
            .is_empty()
    );
}

/// Forward drift over a real socket: a kind and a status this build has never
/// heard of decode rather than failing the queue, and the unknown *status* is
/// not treated as pending.
///
/// Falsification: drop either `#[serde(untagged)] Unknown(String)` arm in
/// `wire.rs` and this fails at `expect` with a `Protocol` error — i.e. the
/// whole card would have gone to its error row over one unrecognised word.
#[tokio::test]
async fn an_unrecognised_kind_or_status_still_decodes_over_the_socket() {
    let hive = FakeHive::serve(replies(&[("pending", &fixture("pending_unknown.json"))]));

    let resp = request(hive.path(), &Request::Pending)
        .await
        .expect("forward drift must not fail the round trip");
    let queue = resp.approvals.clone().expect("a queue");
    assert_eq!(queue.len(), 2);
    assert_eq!(
        queue[0].kind,
        ApprovalKind::Unknown("teleport_agent".to_owned())
    );
    assert_eq!(
        queue[1].status,
        ApprovalStatus::Unknown("awaiting_quorum".to_owned())
    );
    assert_eq!(
        PendingApprovals::new(resp.approvals.unwrap_or_default())
            .all()
            .iter()
            .map(|a| a.id)
            .collect::<Vec<_>>(),
        vec![11],
        "an unknown status is not waiting on anybody"
    );
}

/// The two write verbs reach the socket as the exact lines `hive-c0re` parses
/// (`hive-host-sock/src/lib.rs:230-233`), and a refusal comes back as
/// `HiveError::Refused` carrying the daemon's own words — which is what the
/// reducer turns into its "no silent loss" toast.
#[tokio::test]
async fn approve_and_deny_put_their_exact_lines_on_the_socket() {
    let hive = FakeHive::serve(replies(&[
        ("approve", r#"{"version":1,"ok":true}"#),
        ("deny", &fixture("error.json")),
    ]));

    request(hive.path(), &Request::Approve { id: 7 })
        .await
        .expect("the hive accepts");
    let err = request(hive.path(), &Request::Deny { id: 8 })
        .await
        .expect_err("the fixture refuses");
    assert!(
        matches!(err, HiveError::Refused { .. }),
        "expected Refused, got {err:?}"
    );

    assert_eq!(
        hive.seen(),
        vec![
            r#"{"cmd":"approve","id":7}"#.to_owned(),
            r#"{"cmd":"deny","id":8}"#.to_owned(),
        ]
    );
}
