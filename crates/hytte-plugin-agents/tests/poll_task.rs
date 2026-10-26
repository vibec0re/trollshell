//! The poll loop itself, driven against a real socket and a real config file.
//!
//! `tests/reducer.rs` covers the model and `tests/fake_socket.rs` covers one
//! round trip; neither ever executes [`poll_task_with`]. That left four
//! load-bearing behaviours (five, with the refusal send) untested — and a mechanism with no test that goes
//! red when it is deleted is exactly what spec §12 forbids:
//!
//! | mechanism | what it buys | test here |
//! | --- | --- | --- |
//! | the `Gate`'s visibility guard | §5.4's parking — the entire sidebar-closed power story | [`the_poll_parks_while_the_sidebar_is_closed_and_wakes_on_open`] |
//! | the seed poll before the loop | a card that is not "connecting…" until the sidebar is first opened | [`the_seed_poll_runs_before_any_visibility_edge`] |
//! | `ConfigSource::changed` | `agents.toml` live-reload — the `places` behaviour §9 promises | [`an_agents_toml_edit_is_picked_up_on_the_next_poll`] |
//! | `*urls_done = true` | one `Urls` round trip per session, not one per poll | [`urls_is_fetched_once_per_session_not_once_per_poll`] |
//! | the `Msg::WriteRefused` send | a refused write reaching the operator at all | [`a_refused_write_reaches_the_reducer_and_still_re_polls`] |
//!
//! Every one of these uses `#[tokio::test(start_paused = true)]`: the cadence
//! is the thing under test, so the virtual clock is what makes "ten cadences
//! went by and the socket was untouched" a statement about the loop rather
//! than about how long the test slept.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use hytte_config::test_support::{Captured, capture};
use hytte_plugin::cmd_channel;
use hytte_plugin_agents::config::AgentsConfig;
use hytte_plugin_agents::hive::wire::Request;
use hytte_plugin_agents::poll::{Cmd, ConfigSource, Msg, poll_task_with};
use tokio::sync::mpsc;

mod fake;
use fake::{FakeHive, fixture, replies};

/// Point a config at the fake's socket, on a one-second cadence so the paused
/// clock can step through several of them.
fn cfg_for(hive: &FakeHive) -> AgentsConfig {
    AgentsConfig {
        socket: hive.path().display().to_string(),
        poll_seconds: 1,
        ..AgentsConfig::default()
    }
}

/// Let every spawned task run to its next await point. One `yield_now` is not
/// always enough: the loop hops through `select!` → request → `send`, and each
/// hop is its own scheduling point.
async fn settle() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

/// `msg_rx.recv()` with a deadline — **never a bare `.await`**.
///
/// A test that waits forever for a message a deleted mechanism no longer
/// sends does not go red, it hangs: on CI that is a job timeout with no
/// failing test name, and locally it is a reviewer's lunch break. That is the
/// same defect the review caught in the request-timeout test, and it applies
/// to every wait in this file — deleting the seed poll, measured, starves
/// three of these tests at once. Under `start_paused` the virtual clock
/// auto-advances whenever every task is idle, so a genuinely stuck loop trips
/// this in milliseconds of wall time.
async fn recv_soon(rx: &mut mpsc::UnboundedReceiver<Msg>, what: &str) -> Msg {
    tokio::time::timeout(Duration::from_mins(1), rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .unwrap_or_else(|| panic!("the poll task dropped its sender before {what}"))
}

/// The next message that is **not** a tick's incidental traffic.
///
/// A tick has emitted more than one message since #947 P3 put `Pending` on it,
/// and the one-shot `Urls` was always there — so "the next message is the one I
/// am asserting on" stopped being true, and a wait written that way now reads
/// whichever of a tick's messages happened to be at the head of the queue.
/// These tests are about *when the poller talks to the socket* and *what it
/// says about a write*, not about the order within a tick, so they skip the two
/// carriers and assert on the rest.
///
/// Deliberately **not** a blanket drain: `Status`, `Config` and `WriteRefused`
/// all come back, so a test still fails loudly when the wrong one arrives
/// rather than waiting out its deadline.
async fn recv_status_lane(rx: &mut mpsc::UnboundedReceiver<Msg>, what: &str) -> Msg {
    loop {
        match recv_soon(rx, what).await {
            Msg::Pending(_) | Msg::Urls(_) => {}
            other => return other,
        }
    }
}

/// **§5.4's parking.** While the sidebar is hidden the loop makes no round
/// trips at all; opening it polls immediately and resumes the cadence.
///
/// This is the mechanism the PR's own "one tension worth a decision" section
/// is *about*, and the one the sidebar-closed power story rests on entirely.
/// `reducer.rs`'s `the_visibility_edge_reaches_the_poll_task` only proves the
/// `Cmd` leaves the reducer; it says nothing about the task on the other end.
///
/// Falsification: drop the `, if open` guard from the `interval.tick()` arm
/// in `hytte_plugin::poll::Gate::next` — the loop has been the SDK's since
/// #1168 — and the "a parked poll makes no round trips" assertion goes red.
/// (It reds `hytte_plugin::poll`'s own parking tests too; this one is what
/// proves *this* plugin is actually wired to that gate.)
#[tokio::test(start_paused = true)]
async fn the_poll_parks_while_the_sidebar_is_closed_and_wakes_on_open() {
    let hive = FakeHive::serve(replies(&[(
        "agent_status",
        &fixture("agent_status_grouped.json"),
    )]));
    let (tx, rx) = cmd_channel();
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(poll_task_with(
        rx,
        msg_tx,
        cfg_for(&hive),
        ConfigSource::over(Vec::new()),
    ));

    assert!(matches!(
        recv_soon(&mut msg_rx, "the seed config").await,
        Msg::Config(_)
    ));
    assert!(matches!(
        recv_status_lane(&mut msg_rx, "a poll answer").await,
        Msg::Status(Ok(_))
    ));
    // Settle before the baseline: a successful poll is followed by the
    // one-shot `Urls` fetch, which lands *after* the `Msg::Status` this just
    // received. Counting before it completes would credit it to the park.
    settle().await;
    let after_seed = hive.seen().len();
    assert!(after_seed >= 1, "the seed poll happened");

    // Closed: ten cadences go by and the socket is untouched.
    for _ in 0..10 {
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
    }
    assert_eq!(
        hive.seen().len(),
        after_seed,
        "a parked poll must make no round trips"
    );

    // Open: an immediate poll…
    tx.send(Cmd::SetVisible(true)).expect("the lane is open");
    assert!(matches!(
        recv_status_lane(&mut msg_rx, "a poll answer").await,
        Msg::Status(Ok(_))
    ));
    settle().await;
    let after_open = hive.seen().len();
    assert!(after_open > after_seed, "opening polls immediately");

    // …then the cadence resumes.
    for _ in 0..3 {
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
    }
    assert!(
        hive.seen().len() > after_open,
        "the cadence must resume while visible"
    );

    // And closing parks it again.
    tx.send(Cmd::SetVisible(false)).expect("the lane is open");
    settle().await;
    let after_close = hive.seen().len();
    for _ in 0..5 {
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
    }
    assert_eq!(
        hive.seen().len(),
        after_close,
        "closing must park the poll again"
    );
    task.abort();
}

/// **The seed poll.** One round trip at task start, regardless of visibility —
/// so the card shows a roster on first open instead of "connecting…".
///
/// The `SlotVisible` the runtime seeds at register is whatever the sidebar
/// happens to be, usually closed, so without this the parking above would
/// mean the plugin never talks to the hive until someone opens the sidebar.
///
/// Falsification: delete the `poll_once` call before the loop in `poll.rs`
/// and this fails — **named and fast**, on `recv_soon`'s deadline rather than
/// by hanging. (Measured: it also reds the parking and reload tests, since a
/// missing seed starves their first `Msg::Status` too.)
#[tokio::test(start_paused = true)]
async fn the_seed_poll_runs_before_any_visibility_edge() {
    let hive = FakeHive::serve(replies(&[(
        "agent_status",
        &fixture("agent_status_grouped.json"),
    )]));
    let (_tx, rx) = cmd_channel();
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(poll_task_with(
        rx,
        msg_tx,
        cfg_for(&hive),
        ConfigSource::over(Vec::new()),
    ));

    assert!(matches!(
        recv_soon(&mut msg_rx, "the seed config").await,
        Msg::Config(_)
    ));
    // No `SetVisible` was ever sent, so this can only be the seed poll.
    match recv_soon(&mut msg_rx, "the seed poll, with the sidebar closed").await {
        Msg::Status(Ok(rows)) => assert_eq!(rows.len(), 3),
        other => panic!("expected a seeded roster, got {other:?}"),
    }
    // Settle so the rest of the seed tick lands: the status answer is sent
    // before the tick's remaining round trips finish, so reading `seen()` here
    // without settling would be a race that happens to pass.
    settle().await;
    assert_eq!(
        hive.seen(),
        vec![
            r#"{"cmd":"agent_status"}"#.to_owned(),
            // #947 P3 rides the same tick, in this order — the roster first, so
            // a dead hive costs one failed connect and not two.
            r#"{"cmd":"pending"}"#.to_owned(),
            r#"{"cmd":"urls"}"#.to_owned(),
        ],
        "the seed tick is one round of each read verb, and nothing else"
    );
    task.abort();
}

/// **`agents.toml` live-reload.** An edit to a watched layer is picked up on
/// the next poll and re-published to the reducer — the `places` behaviour §9
/// promises, without a plugin restart.
///
/// Falsification: make `ConfigSource::changed` return `false` unconditionally
/// and the second `Msg::Config` never arrives, failing on the timeout below.
#[tokio::test(start_paused = true)]
async fn an_agents_toml_edit_is_picked_up_on_the_next_poll() {
    let hive = FakeHive::serve(replies(&[(
        "agent_status",
        &fixture("agent_status_grouped.json"),
    )]));
    let layer_dir = tempfile::tempdir().expect("a tempdir");
    let layer: PathBuf = layer_dir.path().join("agents.toml");

    let (tx, rx) = cmd_channel();
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(poll_task_with(
        rx,
        msg_tx,
        cfg_for(&hive),
        ConfigSource::over(vec![layer.clone()]),
    ));

    // The seed config is the one that was injected.
    match recv_soon(&mut msg_rx, "the seed config").await {
        Msg::Config(cfg) => assert!(cfg.display.is_empty()),
        other => panic!("expected the seed config, got {other:?}"),
    }
    assert!(matches!(
        recv_status_lane(&mut msg_rx, "a poll answer").await,
        Msg::Status(Ok(_))
    ));

    // The operator edits the file while the sidebar is open.
    tx.send(Cmd::SetVisible(true)).expect("the lane is open");
    assert!(matches!(
        recv_status_lane(&mut msg_rx, "a poll answer").await,
        Msg::Status(Ok(_))
    ));
    std::fs::write(
        &layer,
        "[display.trollshell-choom]\nlabel = \"choom\"\nproject = \"viberoot\"\n",
    )
    .expect("write the overlay");

    // The next tick notices.
    let reloaded = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            tokio::time::advance(Duration::from_secs(1)).await;
            settle().await;
            // Drain the whole tick looking for the reload, rather than
            // inspecting only its first message: a tick emits a status and
            // (since #947 P3) an approval queue, and a leftover one of those
            // would otherwise be the message that got looked at while the
            // `Msg::Config` behind it was thrown away by the drain.
            while let Ok(msg) = msg_rx.try_recv() {
                if let Msg::Config(cfg) = msg {
                    return cfg;
                }
            }
        }
    })
    .await
    .expect("an edited layer must be re-read");

    assert_eq!(reloaded.label_for("trollshell-choom"), "choom");
    assert_eq!(reloaded.project_for("trollshell-choom"), Some("viberoot"));
    task.abort();
}

/// **A live reload keeps the file's socket past a bad display value** (#1451)
/// — the per-key rule pinned on the path that actually ships, not only on
/// `config.rs`'s own `assemble` seam: `ConfigSource::read` is what a running
/// plugin re-reads `agents.toml` through.
///
/// The edited file points the socket at the fake hive and carries one
/// wrong-typed `label`. Whole-file, that `label` failed the layer and the
/// reload published the built-in default instead — `/run/hyperhive/host.sock`,
/// the 2 s cadence, no decorations — so the rows silently went looking for a
/// different hive. Per key, it costs that one label.
///
/// Falsification: point `ConfigSource::read` back at a whole-file reader (give
/// `AgentsFile::validate` an error whenever `parsed` rejects anything) and the
/// socket assertion reds.
#[tokio::test(start_paused = true)]
async fn a_reload_with_a_bad_display_value_keeps_the_files_socket() {
    let hive = FakeHive::serve(replies(&[(
        "agent_status",
        &fixture("agent_status_grouped.json"),
    )]));
    let socket = hive.path().display().to_string();
    let layer_dir = tempfile::tempdir().expect("a tempdir");
    let layer: PathBuf = layer_dir.path().join("agents.toml");

    let (tx, rx) = cmd_channel();
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(poll_task_with(
        rx,
        msg_tx,
        cfg_for(&hive),
        ConfigSource::over(vec![layer.clone()]),
    ));
    assert!(matches!(
        recv_soon(&mut msg_rx, "the seed config").await,
        Msg::Config(_)
    ));
    tx.send(Cmd::SetVisible(true)).expect("the lane is open");
    assert!(matches!(
        recv_status_lane(&mut msg_rx, "a poll answer").await,
        Msg::Status(Ok(_))
    ));

    std::fs::write(
        &layer,
        format!(
            "socket = {}\npoll_seconds = 1\n\
             [display.trollshell-choom]\nlabel = 5\nproject = \"viberoot\"\n\
             [display.argus]\nlabel = \"Argus\"\n",
            toml::Value::from(socket.as_str())
        ),
    )
    .expect("write the overlay");

    let reloaded = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            tokio::time::advance(Duration::from_secs(1)).await;
            settle().await;
            while let Ok(msg) = msg_rx.try_recv() {
                if let Msg::Config(cfg) = msg {
                    return cfg;
                }
            }
        }
    })
    .await
    .expect("an edited layer must be re-read");

    assert_eq!(reloaded.socket, socket, "the file's socket survives");
    assert_eq!(reloaded.poll_seconds, 1, "…and so does its cadence");
    assert_eq!(
        reloaded.project_for("trollshell-choom"),
        Some("viberoot"),
        "the bad label's own entry keeps its good key"
    );
    assert_eq!(
        reloaded.label_for("trollshell-choom"),
        "trollshell-choom",
        "the bad label falls back to the hive's name"
    );
    assert_eq!(
        reloaded.label_for("argus"),
        "Argus",
        "another agent's entry applies"
    );
    task.abort();
}

/// Drive ticks until a `Msg::Config` shows up, or `None` if none does in `secs`.
async fn next_config(rx: &mut mpsc::UnboundedReceiver<Msg>, secs: u64) -> Option<AgentsConfig> {
    tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            tokio::time::advance(Duration::from_secs(1)).await;
            settle().await;
            while let Ok(msg) = rx.try_recv() {
                if let Msg::Config(cfg) = msg {
                    return *cfg;
                }
            }
        }
    })
    .await
    .ok()
}

/// **A live reload of a corrupt `agents.toml` keeps the config in use**
/// (review of `main`, LOW) — a whole-file failure (not TOML) still reaches
/// `ConfigSource::read`'s `Err`, and used to publish the built-in default:
/// default socket, default cadence, `[display]` wiped, rows "no socket" until
/// the file was fixed. Now the running config stays, and fixing the file applies.
///
/// Falsification: make `ConfigSource::read` return `Some(AgentsConfig::default())`
/// on `Err` and the corrupt-step assertion reds.
#[tokio::test(start_paused = true)]
async fn a_corrupt_reload_keeps_the_running_config_until_the_file_is_fixed() {
    let hive = FakeHive::serve(replies(&[(
        "agent_status",
        &fixture("agent_status_grouped.json"),
    )]));
    let socket = hive.path().display().to_string();
    let good = |label: &str, secs: u64| {
        format!(
            "socket = {}\npoll_seconds = {secs}\n[display.argus]\nlabel = \"{label}\"\n",
            toml::Value::from(socket.as_str())
        )
    };
    let layer_dir = tempfile::tempdir().expect("a tempdir");
    let layer: PathBuf = layer_dir.path().join("agents.toml");

    let (tx, rx) = cmd_channel();
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(poll_task_with(
        rx,
        msg_tx,
        cfg_for(&hive),
        ConfigSource::over(vec![layer.clone()]),
    ));
    assert!(matches!(
        recv_soon(&mut msg_rx, "the seed config").await,
        Msg::Config(_)
    ));
    tx.send(Cmd::SetVisible(true)).expect("the lane is open");

    // A good file is picked up.
    write_at(&layer, &good("Argus", 3), 1_000);
    let running = next_config(&mut msg_rx, 30)
        .await
        .expect("the file applies");
    assert_eq!(running.socket, socket);
    assert_eq!(running.poll_seconds, 3);
    assert_eq!(running.label_for("argus"), "Argus");

    // Corrupt it: nothing is published (least of all the default).
    write_at(&layer, "socket = [unterminated\n", 2_000);
    assert!(
        next_config(&mut msg_rx, 30).await.is_none(),
        "a corrupt file must not publish a config"
    );

    // Fix it: the new values apply.
    write_at(&layer, &good("Argus II", 5), 3_000);
    let fixed = next_config(&mut msg_rx, 30).await.expect("the fix applies");
    assert_eq!(fixed.socket, socket);
    assert_eq!(fixed.poll_seconds, 5);
    assert_eq!(fixed.label_for("argus"), "Argus II");
    task.abort();
}

/// How many times the journal named `trollshell-choom`'s bad `label`.
fn label_lines(log: &Captured) -> usize {
    log.warnings()
        .iter()
        .filter(|w| w.contains("display.*.label = trollshell-choom: 5 is not valid"))
        .count()
}

/// Write `body` to `path` and set its mtime explicitly, so two edits can never
/// share an mtime granule and read as one.
fn write_at(path: &Path, body: &str, secs: u64) {
    std::fs::write(path, body).expect("write");
    std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open")
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
        .expect("set mtime");
}

/// **A live reload names a rejected key once per edit, not once per tick**
/// (#1457 review, MEDIUM 1) — the journal half of #1451 on the reload path.
///
/// `a_reload_with_a_bad_display_value_keeps_the_files_socket` pins the
/// reload's *values*; this pins that the rejection is *said*, and said once.
/// Twelve ticks after one edit give exactly one line (the reload only
/// re-reads when a stamp moved), and a second edit gives a second line — the
/// control that makes the first count a statement about ticks rather than a
/// capture that saw nothing.
///
/// Falsification (verified red): make `ConfigSource::read` silent
/// (`load_from` + `parsed().0`, no line) or whole-file (the built-in default
/// on any rejection) and the count is 0; make `ConfigSource::changed` always
/// `true` and it is one per tick.
#[tokio::test(start_paused = true)]
async fn a_live_reload_reports_a_bad_key_once_per_edit_not_once_per_tick() {
    let (log, _guard) = capture();
    let hive = FakeHive::serve(replies(&[(
        "agent_status",
        &fixture("agent_status_grouped.json"),
    )]));
    let socket = hive.path().display().to_string();
    let dir = tempfile::tempdir().expect("tempdir");
    let layer = dir.path().join("agents.toml");
    let body = format!(
        "socket = {}\npoll_seconds = 1\n[display.trollshell-choom]\nlabel = 5\n",
        toml::Value::from(socket.as_str())
    );

    let (tx, rx) = cmd_channel();
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<Msg>();
    let task = tokio::spawn(poll_task_with(
        rx,
        msg_tx,
        AgentsConfig {
            socket: socket.clone(),
            poll_seconds: 1,
            ..AgentsConfig::default()
        },
        ConfigSource::over(vec![layer.clone()]),
    ));
    tx.send(Cmd::SetVisible(true)).expect("the lane is open");
    settle().await;

    write_at(&layer, &body, 1_000_000);
    for _ in 0..12 {
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        while msg_rx.try_recv().is_ok() {}
    }
    assert_eq!(
        label_lines(&log),
        1,
        "one line for one edit across 12 ticks: {:?}",
        log.warnings()
    );
    assert!(
        !hive.seen().is_empty(),
        "the file's socket is the one polled"
    );

    // Control: a second edit (a new mtime) is a second line.
    write_at(&layer, &body, 1_000_001);
    for _ in 0..3 {
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        while msg_rx.try_recv().is_ok() {}
    }
    assert_eq!(label_lines(&log), 2, "{:?}", log.warnings());
    task.abort();
}

/// **A refused write reaches the reducer.** The hive answers `ok: false` to a
/// `set_paused`; the task must turn that into a `Msg::WriteRefused` carrying
/// the daemon's own words, and must still re-poll so the row un-sticks.
///
/// `reducer.rs` covers the other half — `Msg::WriteRefused` → exactly one
/// `Effect::Notify` — by feeding the message directly, which means it does
/// **not** cover the send. Measured: deleting the `msg_tx.send` in `poll.rs`
/// leaves the whole suite green without this test.
///
/// Falsification: delete that send and this fails on `recv_soon`'s deadline.
#[tokio::test(start_paused = true)]
async fn a_refused_write_reaches_the_reducer_and_still_re_polls() {
    let hive = FakeHive::serve(replies(&[
        ("agent_status", &fixture("agent_status_grouped.json")),
        ("set_paused", &fixture("error.json")),
    ]));
    let (tx, rx) = cmd_channel();
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(poll_task_with(
        rx,
        msg_tx,
        cfg_for(&hive),
        ConfigSource::over(Vec::new()),
    ));

    assert!(matches!(
        recv_soon(&mut msg_rx, "the seed config").await,
        Msg::Config(_)
    ));
    assert!(matches!(
        recv_status_lane(&mut msg_rx, "a poll answer").await,
        Msg::Status(Ok(_))
    ));
    settle().await;

    tx.send(Cmd::Send(Request::SetPaused {
        name: "trollshell-choom".to_owned(),
        paused: true,
    }))
    .expect("the lane is open");

    match recv_status_lane(&mut msg_rx, "the refusal").await {
        Msg::WriteRefused { request, reason } => {
            assert_eq!(
                request,
                Request::SetPaused {
                    name: "trollshell-choom".to_owned(),
                    paused: true,
                },
                "the refusal must carry the frame that was refused"
            );
            assert!(
                reason.contains("ghost"),
                "the hive's own words, not a stand-in: {reason}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    // …and the roster is re-read regardless, which is what un-sticks the row's
    // optimistic flip.
    assert!(matches!(
        recv_status_lane(&mut msg_rx, "the reconciling poll").await,
        Msg::Status(Ok(_))
    ));
    task.abort();
}

/// **The `urls_done` latch.** `Urls` is asked once per session, not once per
/// poll — it backs a link that changes about never, and a per-poll round trip
/// would double this plugin's socket traffic for nothing.
///
/// Falsification: never set `*urls_done = true` in `poll_once` and the
/// `urls == 1` assertion goes red (it climbs with the poll count).
#[tokio::test(start_paused = true)]
async fn urls_is_fetched_once_per_session_not_once_per_poll() {
    let hive = FakeHive::serve(replies(&[
        ("agent_status", &fixture("agent_status_grouped.json")),
        ("urls", &fixture("urls.json")),
    ]));
    let (tx, rx) = cmd_channel();
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(poll_task_with(
        rx,
        msg_tx,
        cfg_for(&hive),
        ConfigSource::over(Vec::new()),
    ));

    assert!(matches!(
        recv_soon(&mut msg_rx, "the seed config").await,
        Msg::Config(_)
    ));
    tx.send(Cmd::SetVisible(true)).expect("the lane is open");

    // Several cadences worth of polls.
    for _ in 0..8 {
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
    }
    while msg_rx.try_recv().is_ok() {}

    let seen = hive.seen();
    let statuses = seen
        .iter()
        .filter(|l| l.contains(r#""cmd":"agent_status""#))
        .count();
    let urls = seen
        .iter()
        .filter(|l| l.contains(r#""cmd":"urls""#))
        .count();
    assert!(statuses > 2, "several polls happened: {statuses}");
    assert_eq!(urls, 1, "Urls must be asked exactly once per session");
    task.abort();
}

/// **#1140's review, LOW-2.** A hive that is down costs **one** failed connect
/// per tick, not three.
///
/// The ordering's stated reason — `AgentStatus` first, `Pending` and `Urls`
/// skipped when it fails — was unpinned: replacing the `if !ok { return; }`
/// with `if false` left the suite green, and a down hive would then burn three
/// connect attempts every tick for as long as the sidebar is open.
///
/// Falsification: delete that early return and `seen()` grows past one, or —
/// since the fake is not even bound here — a `Msg::Pending` arrives where none
/// should.
#[tokio::test(start_paused = true)]
async fn a_failed_status_skips_the_rest_of_the_tick() {
    // A path with no socket behind it: every connect fails.
    let dir = tempfile::tempdir().expect("a tempdir");
    let cfg = AgentsConfig {
        socket: dir.path().join("absent.sock").display().to_string(),
        poll_seconds: 1,
        ..AgentsConfig::default()
    };
    let (_tx, rx) = cmd_channel();
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(poll_task_with(
        rx,
        msg_tx,
        cfg,
        ConfigSource::over(Vec::new()),
    ));

    assert!(matches!(
        recv_soon(&mut msg_rx, "the seed config").await,
        Msg::Config(_)
    ));
    match recv_soon(&mut msg_rx, "the seed poll's failure").await {
        Msg::Status(Err(_)) => {}
        other => panic!("expected a failed status, got {other:?}"),
    }

    // Nothing else from that tick: no `Pending`, no `Urls`.
    settle().await;
    assert!(
        msg_rx.try_recv().is_err(),
        "a failed status must end the tick — no `Pending`, no `Urls`"
    );
    task.abort();
}

/// The refusal lane (#1140's review, LOW-3): `Pending` answering `ok: false`
/// reaches the reducer as `Msg::Pending(Err(…))` rather than being swallowed,
/// so the badges it can no longer vouch for are dropped.
///
/// Falsification: go back to logging the error and dropping it, and the
/// `Msg::Pending(Err(_))` never arrives — this fails on `recv_soon`'s deadline.
#[tokio::test(start_paused = true)]
async fn a_refused_pending_reaches_the_reducer() {
    let hive = FakeHive::serve(replies(&[
        ("agent_status", &fixture("agent_status_grouped.json")),
        ("pending", &fixture("error.json")),
    ]));
    let (_tx, rx) = cmd_channel();
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(poll_task_with(
        rx,
        msg_tx,
        cfg_for(&hive),
        ConfigSource::over(Vec::new()),
    ));

    assert!(matches!(
        recv_soon(&mut msg_rx, "the seed config").await,
        Msg::Config(_)
    ));
    assert!(matches!(
        recv_soon(&mut msg_rx, "a poll answer").await,
        Msg::Status(Ok(_))
    ));
    match recv_soon(&mut msg_rx, "the refused approval queue").await {
        Msg::Pending(Err(e)) => assert!(e.to_string().contains("ghost"), "{e}"),
        other => panic!("expected a refused queue, got {other:?}"),
    }
    task.abort();
}
