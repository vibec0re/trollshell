//! The poll loop itself, driven against a real socket and a real config file.
//!
//! `tests/reducer.rs` covers the model and `tests/fake_socket.rs` covers one
//! round trip; neither ever executes [`poll_task_with`]. That left four
//! load-bearing behaviours untested — and a mechanism with no test that goes
//! red when it is deleted is exactly what spec §12 forbids:
//!
//! | mechanism | what it buys | test here |
//! | --- | --- | --- |
//! | `, if visible` on the tick arm | §5.4's parking — the entire sidebar-closed power story | [`the_poll_parks_while_the_sidebar_is_closed_and_wakes_on_open`] |
//! | the seed poll before the loop | a card that is not "connecting…" until the sidebar is first opened | [`the_seed_poll_runs_before_any_visibility_edge`] |
//! | `ConfigSource::changed` | `agents.toml` live-reload — the `places` behaviour §9 promises | [`an_agents_toml_edit_is_picked_up_on_the_next_poll`] |
//! | `*urls_done = true` | one `Urls` round trip per session, not one per poll | [`urls_is_fetched_once_per_session_not_once_per_poll`] |
//!
//! Every one of these uses `#[tokio::test(start_paused = true)]`: the cadence
//! is the thing under test, so the virtual clock is what makes "ten cadences
//! went by and the socket was untouched" a statement about the loop rather
//! than about how long the test slept.

use std::path::PathBuf;
use std::time::Duration;

use hytte_plugin::cmd_channel;
use hytte_plugin_agents::config::AgentsConfig;
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

/// **§5.4's parking.** While the sidebar is hidden the loop makes no round
/// trips at all; opening it polls immediately and resumes the cadence.
///
/// This is the mechanism the PR's own "one tension worth a decision" section
/// is *about*, and the one the sidebar-closed power story rests on entirely.
/// `reducer.rs`'s `the_visibility_edge_reaches_the_poll_task` only proves the
/// `Cmd` leaves the reducer; it says nothing about the task on the other end.
///
/// Falsification: drop `, if visible` from the `interval.tick()` arm in
/// `poll.rs` and the "a parked poll makes no round trips" assertion goes red.
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

    assert!(matches!(msg_rx.recv().await, Some(Msg::Config(_))));
    assert!(matches!(msg_rx.recv().await, Some(Msg::Status(Ok(_)))));
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
    assert!(matches!(msg_rx.recv().await, Some(Msg::Status(Ok(_)))));
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
/// Falsification: delete the `poll_once` call before the loop in `poll.rs` and
/// the `Msg::Status` assertion below hangs, then fails on the outer timeout.
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

    assert!(matches!(msg_rx.recv().await, Some(Msg::Config(_))));
    // No `SetVisible` was ever sent, so this can only be the seed poll. The
    // outer timeout is what turns a deleted seed into a fast, named red rather
    // than a hang.
    let status = tokio::time::timeout(Duration::from_secs(30), msg_rx.recv())
        .await
        .expect("the seed poll must run with the sidebar closed");
    match status {
        Some(Msg::Status(Ok(rows))) => assert_eq!(rows.len(), 3),
        other => panic!("expected a seeded roster, got {other:?}"),
    }
    assert_eq!(hive.seen(), vec![r#"{"cmd":"agent_status"}"#.to_owned()]);
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
    match msg_rx.recv().await {
        Some(Msg::Config(cfg)) => assert!(cfg.display.is_empty()),
        other => panic!("expected the seed config, got {other:?}"),
    }
    assert!(matches!(msg_rx.recv().await, Some(Msg::Status(Ok(_)))));

    // The operator edits the file while the sidebar is open.
    tx.send(Cmd::SetVisible(true)).expect("the lane is open");
    assert!(matches!(msg_rx.recv().await, Some(Msg::Status(Ok(_)))));
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
            if let Ok(Msg::Config(cfg)) = msg_rx.try_recv() {
                return cfg;
            }
            while msg_rx.try_recv().is_ok() {}
        }
    })
    .await
    .expect("an edited layer must be re-read");

    assert_eq!(reloaded.label_for("trollshell-choom"), "choom");
    assert_eq!(reloaded.project_for("trollshell-choom"), Some("viberoot"));
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

    assert!(matches!(msg_rx.recv().await, Some(Msg::Config(_))));
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
