//! The reducer, driven with no host and no hive.
//!
//! `Plugin` has no transport surface, so every one of these builds a model
//! with [`hytte_plugin::cmd_channel`] — the shape that crate's own docs name
//! for exactly this — folds inputs into it, and reads back the effects it
//! returned plus the frames it queued on the command lane.
//!
//! The command lane is where the interesting assertions live: a click's
//! *visible* result is a re-render, but its *consequential* result is the JSON
//! line that reaches `host.sock`, so these pin the bytes.

use hytte_plugin::proto::{Effect, EventKind, Page};
use hytte_plugin::{CmdReceiver, Input, Plugin, cmd_channel};
use hytte_plugin_agents::Agents;
use hytte_plugin_agents::hive::client::HiveError;
use hytte_plugin_agents::hive::wire::{AgentStatusRow, Response, VersionMismatch};
use hytte_plugin_agents::model::{Hive, Status};
use hytte_plugin_agents::poll::{Cmd, Msg};

// ── harness ──────────────────────────────────────────────────────────────────

fn model() -> (Agents, CmdReceiver<Cmd>) {
    let (tx, rx) = cmd_channel();
    (Agents::with_cmds(tx), rx)
}

/// Decode a `tests/fixtures/*.json` line into the roster it carries — the
/// same bytes `fake_socket.rs` puts through the real client, so the reducer
/// and the wire tests cannot drift apart.
fn roster(fixture: &str) -> Vec<AgentStatusRow> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing fixture {} ({e})", path.display()));
    let resp: Response = serde_json::from_str(body.trim()).expect("the fixture decodes");
    resp.agent_statuses.expect("the fixture carries a roster")
}

fn status(rows: Vec<AgentStatusRow>) -> Input<Msg> {
    Input::App(Msg::Status(Ok(rows)))
}

fn click(id: &str) -> Input<Msg> {
    Input::Event {
        node: id.to_owned(),
        kind: EventKind::Click,
    }
}

/// Every frame queued on the command lane so far, as the JSON lines they will
/// become on the socket.
fn lines(rx: &mut CmdReceiver<Cmd>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        if let Cmd::Send(req) = cmd {
            out.push(serde_json::to_string(&req).expect("a Request serializes"));
        }
    }
    out
}

/// Every command queued, including the visibility ones.
fn cmds(rx: &mut CmdReceiver<Cmd>) -> Vec<Cmd> {
    let mut out = Vec::new();
    while let Ok(cmd) = rx.try_recv() {
        out.push(cmd);
    }
    out
}

// ── folding a poll ───────────────────────────────────────────────────────────

#[test]
fn a_poll_becomes_a_roster_and_a_failure_becomes_one_parked_row() {
    let (mut m, _rx) = model();
    assert_eq!(m.hive, Hive::Connecting);

    m.update(status(roster("agent_status_grouped.json")));
    assert_eq!(m.hive.agents().len(), 3);
    assert_eq!(m.hive.agents()[0].name.as_str(), "trollshell-choom");

    m.update(Input::App(Msg::Status(Err(HiveError::Unreachable {
        reason: "no socket — hive-c0re is not running".to_owned(),
    }))));
    assert_eq!(
        m.hive,
        Hive::Unreachable {
            reason: "no socket — hive-c0re is not running".to_owned()
        }
    );
    assert!(m.hive.agents().is_empty());
}

/// A version mismatch gets its **own** state, not the generic unreachable one:
/// "the hive is down" and "I refuse to read this hive" are different things
/// for an operator to see.
#[test]
fn a_version_mismatch_gets_its_own_state() {
    let (mut m, _rx) = model();
    m.update(Input::App(Msg::Status(Err(HiveError::Version(
        VersionMismatch {
            theirs: 99,
            ours: 1,
        },
    )))));
    assert_eq!(
        m.hive,
        Hive::Incompatible(VersionMismatch {
            theirs: 99,
            ours: 1
        })
    );
}

/// §11 rule two: a name that fails the whitelist never becomes a row, and the
/// legitimate rows around it survive.
///
/// Falsification: drop the `AgentName::parse` guard in `fold_status` and the
/// roster grows to two, putting `../etc/passwd` into a node id.
#[test]
fn a_row_whose_name_fails_the_whitelist_is_dropped_not_rendered() {
    let (mut m, _rx) = model();
    let rows = roster("agent_status_bad_name.json");
    assert_eq!(rows.len(), 2, "the fixture carries one good and one bad");
    m.update(status(rows));
    let agents = m.hive.agents();
    assert_eq!(agents.len(), 1, "the illegal name must not become a row");
    assert_eq!(agents[0].name.as_str(), "good");
}

/// The §6.2 precedence fixture, folded end to end: five agents, five states,
/// in the hive's own order.
#[test]
fn the_precedence_fixture_renders_one_state_per_row() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_precedence.json")));
    let got: Vec<(&str, Status)> = m
        .hive
        .agents()
        .iter()
        .map(|a| (a.name.as_str(), a.status()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("wedged", Status::Failed),
            ("locked-out", Status::NeedsLogin),
            ("parked", Status::Paused),
            ("off", Status::Stopped),
            ("busy", Status::Running),
        ]
    );
    // The harness text is the running row's second line, verbatim.
    assert_eq!(m.hive.agents()[4].status_line(), "running the P1 gate");
    // …and the badge is orthogonal: `off` is stopped AND needs an update.
    assert!(m.hive.agents()[3].needs_update());
}

// ── the pause button ─────────────────────────────────────────────────────────

/// A click on `pause:<name>` emits exactly one `SetPaused` with the flipped
/// bool, and the row flips optimistically while it is in flight.
#[test]
fn a_pause_click_emits_one_set_paused_with_the_flipped_bool() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));

    // `trollshell-choom` is running and unpaused → the click asks to pause.
    let fx = m.update(click("pause:trollshell-choom"));
    assert!(fx.is_empty(), "pause is a command, not a shell effect");
    assert_eq!(
        lines(&mut rx),
        vec![r#"{"cmd":"set_paused","name":"trollshell-choom","paused":true}"#.to_owned()]
    );
    assert!(
        m.hive.agent(&name("trollshell-choom")).unwrap().paused(),
        "the row flips optimistically"
    );

    // `nixos-choom` is already paused → the click asks to resume.
    m.update(click("pause:nixos-choom"));
    assert_eq!(
        lines(&mut rx),
        vec![r#"{"cmd":"set_paused","name":"nixos-choom","paused":false}"#.to_owned()]
    );
}

/// The optimistic flip lasts exactly until the next poll answers — **whatever
/// it says**. A write the hive refused must not leave a lie on screen.
///
/// Falsification: make `fold_status` preserve `pending_paused` when the poll
/// disagrees, and the second half of this test fails with the row still
/// claiming "paused" over a hive that says otherwise.
#[test]
fn an_optimistic_flip_is_cleared_by_the_next_poll_even_when_it_disagrees() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    m.update(click("pause:trollshell-choom"));
    let _ = lines(&mut rx);
    assert!(m.hive.agent(&name("trollshell-choom")).unwrap().paused());

    // The hive answers with the SAME roster — i.e. the write did not land.
    m.update(status(roster("agent_status_grouped.json")));
    assert!(
        !m.hive.agent(&name("trollshell-choom")).unwrap().paused(),
        "the row must reconcile to the hive, not to the click"
    );
}

/// A click on an agent the roster does not carry is inert — no frame, no
/// panic. (The id came back over a socket; it is not trusted.)
#[test]
fn a_click_on_an_unknown_or_illegal_agent_sends_nothing() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    for id in [
        "pause:ghost",
        "pause:../etc/passwd",
        "pause:has space",
        "start:ghost-but-illegal!",
        "nonsense",
        "",
    ] {
        m.update(click(id));
    }
    assert!(lines(&mut rx).is_empty());
}

// ── the primary click ────────────────────────────────────────────────────────

/// Spec §6.3: the primary click never pauses the loop. In P1 it opens the
/// plugin's own panel; in P2 it becomes a detached `RunCommand` launching the
/// chat companion. **What must hold across that swap** is asserted here: no
/// `SetPaused` on the lane.
///
/// Falsification: add a `SetPaused` to the `chat:` arm and this goes red.
#[test]
fn the_primary_click_opens_the_panel_and_never_pauses_the_loop() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));

    let fx = m.update(click("chat:trollshell-choom"));
    assert_eq!(fx, vec![Effect::OpenPage(Page::PluginSelf)]);
    assert!(
        lines(&mut rx).is_empty(),
        "the chat surface is used while the loop RUNS — it must not pause it"
    );
    assert_eq!(
        m.selected
            .as_ref()
            .map(hytte_plugin_agents::model::AgentName::as_str),
        Some("trollshell-choom")
    );
}

/// The edit button is the same read-only detail in P1 (real editing waits for
/// #952) — and likewise sends no frame.
#[test]
fn the_edit_click_opens_the_panel_and_sends_no_frame() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    let fx = m.update(click("edit:stray"));
    assert_eq!(fx, vec![Effect::OpenPage(Page::PluginSelf)]);
    assert!(lines(&mut rx).is_empty());
    assert_eq!(
        m.selected
            .as_ref()
            .map(hytte_plugin_agents::model::AgentName::as_str),
        Some("stray")
    );

    m.update(click("agents-back"));
    assert_eq!(m.selected, None);
}

/// A selection whose agent leaves the roster falls back to the overview rather
/// than pinning a panel to something that no longer exists.
#[test]
fn a_selection_that_vanishes_falls_back_to_the_overview() {
    let (mut m, _rx) = model();
    m.update(status(roster("agent_status_grouped.json")));
    m.update(click("edit:stray"));
    assert!(m.selected.is_some());

    m.update(status(roster("agent_status_precedence.json")));
    assert_eq!(m.selected, None);
}

// ── the panel's start / stop ─────────────────────────────────────────────────

/// Spec §11 rule one, through the reducer: the frames a panel click produces
/// scope exactly one agent.
#[test]
fn the_panel_start_and_stop_scope_exactly_one_agent() {
    let (mut m, mut rx) = model();
    m.update(status(roster("agent_status_grouped.json")));

    m.update(click("start:stray"));
    m.update(click("stop:stray"));
    assert_eq!(
        lines(&mut rx),
        vec![
            r#"{"cmd":"start","scope":{"agent_names":["stray"]}}"#.to_owned(),
            r#"{"cmd":"stop","scope":{"agent_names":["stray"]},"graceful":true}"#.to_owned(),
        ]
    );
}

// ── §8's edge-triggered Notify ───────────────────────────────────────────────

fn row(name: &str, failed: bool, needs_login: bool) -> AgentStatusRow {
    AgentStatusRow {
        name: name.to_owned(),
        running: !failed,
        failed,
        needs_login,
        ..AgentStatusRow::default()
    }
}

/// Two identical polls raise **one** toast, not two — §8's "edge only, never a
/// level: a hive with one wedged agent must not toast every 5 s".
///
/// Falsification: replace the `prev_alarms` diff with a level check
/// (`if now.failed { … }`) and the second poll starts toasting too.
#[test]
fn notify_fires_on_the_edge_and_two_identical_polls_toast_once() {
    let (mut m, _rx) = model();

    // First sighting: healthy, and no toast (nothing has transitioned).
    assert!(m.update(status(vec![row("a", false, false)])).is_empty());

    // The edge.
    let fx = m.update(status(vec![row("a", true, false)]));
    assert_eq!(fx.len(), 1, "one toast on the edge");
    match &fx[0] {
        Effect::Notify { summary, body } => {
            assert!(summary.contains('a'), "{summary}");
            assert!(summary.contains("failed"), "{summary}");
            assert!(!body.is_empty());
        }
        other => panic!("expected Notify, got {other:?}"),
    }

    // The level. Same flags, three more polls, zero toasts.
    for _ in 0..3 {
        assert!(
            m.update(status(vec![row("a", true, false)])).is_empty(),
            "a steady failed agent must not re-toast"
        );
    }

    // Recovering and failing again is a NEW edge, so it toasts again.
    assert!(m.update(status(vec![row("a", false, false)])).is_empty());
    assert_eq!(m.update(status(vec![row("a", true, false)])).len(), 1);
}

/// `needs_login` is its own edge, with its own wording — and both flags
/// flipping at once raise both toasts.
#[test]
fn needs_login_is_its_own_edge_and_both_can_fire_together() {
    let (mut m, _rx) = model();
    m.update(status(vec![row("a", false, false)]));

    let fx = m.update(status(vec![row("a", false, true)]));
    assert_eq!(fx.len(), 1);
    match &fx[0] {
        Effect::Notify { summary, .. } => assert!(summary.contains("needs login"), "{summary}"),
        other => panic!("expected Notify, got {other:?}"),
    }

    m.update(status(vec![row("a", false, false)]));
    let both = m.update(status(vec![row("a", true, true)]));
    assert_eq!(both.len(), 2, "two independent edges, two toasts");
}

/// A first sighting is not a transition — otherwise every reconnect would
/// toast the whole roster, which is the louder bug (§8's edge rule).
#[test]
fn a_first_sighting_of_an_already_failed_agent_is_silent() {
    let (mut m, _rx) = model();
    assert!(
        m.update(status(vec![row("a", true, true)])).is_empty(),
        "a wedged agent seen for the first time must not toast"
    );
}

/// The toast names the agent by its **display label** when one is configured,
/// because that is what the row shows.
#[test]
fn the_toast_uses_the_configured_display_label() {
    let (mut m, _rx) = model();
    m.update(Input::App(Msg::Config(Box::new(toml_config(
        "[display.a]\nlabel = \"choom\"\n",
    )))));
    m.update(status(vec![row("a", false, false)]));
    let fx = m.update(status(vec![row("a", true, false)]));
    match &fx[0] {
        Effect::Notify { summary, .. } => assert!(summary.starts_with("choom"), "{summary}"),
        other => panic!("expected Notify, got {other:?}"),
    }
}

fn toml_config(body: &str) -> hytte_plugin_agents::config::AgentsConfig {
    hytte_config::subsystem::assemble::<hytte_plugin_agents::config::AgentsConfig>(&[(
        std::path::PathBuf::from("overlay.toml"),
        body.to_owned(),
    )])
    .expect("the test config assembles")
    .config
}

// ── visibility ───────────────────────────────────────────────────────────────

/// The visibility push is forwarded down the lane so the poll task can park —
/// the #305 gate that makes `StateKey::SlotVisible` worth subscribing.
///
/// Falsification: drop the `Input::SlotVisible` arm and the poller never
/// parks (and never wakes on open).
#[test]
fn the_visibility_edge_reaches_the_poll_task() {
    let (mut m, mut rx) = model();
    m.update(Input::SlotVisible(true));
    m.update(Input::SlotVisible(false));
    assert_eq!(
        cmds(&mut rx),
        vec![Cmd::SetVisible(true), Cmd::SetVisible(false)]
    );
}

/// The manifest is the plugin's whole trust declaration, so it is pinned:
/// `OpenPage` + `Notify` and nothing else. **`RunCommand` is P2 and `Consent`
/// is P3** — a P1 that quietly declared either would be granted an authority
/// the row has not earned yet (spec §11 rules two and three, §13).
#[test]
fn the_manifest_declares_exactly_two_capabilities_and_no_secrets() {
    use hytte_plugin::proto::{Capability, Mount, StateKey};
    let m = Agents::manifest();
    assert_eq!(m.id, "agents");
    assert_eq!(m.mount, Mount::SidebarTop);
    assert_eq!(
        m.capabilities,
        vec![Capability::OpenPage, Capability::Notify]
    );
    assert!(!m.capabilities.contains(&Capability::RunCommand));
    assert!(!m.capabilities.contains(&Capability::Consent));
    assert_eq!(
        m.subscribes,
        vec![StateKey::Clock, StateKey::SlotVisible],
        "SlotVisible is the #305 gate for the park; Clock drives the panel ages"
    );
    assert!(m.provides.is_empty(), "this plugin serves no datasource");
}

fn name(s: &str) -> hytte_plugin_agents::model::AgentName {
    hytte_plugin_agents::model::AgentName::parse(s).expect("a legal test name")
}
