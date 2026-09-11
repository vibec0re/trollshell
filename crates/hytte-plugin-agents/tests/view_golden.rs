//! Render-tree goldens: the exact `View` the plugin projects, committed to
//! git (spec §12's "`View` render-tree goldens").
//!
//! # Why a debug dump and not a msgpack hex
//!
//! `hytte-plugin-proto`'s own golden suite pins **encoder bytes** — its job is
//! to catch an `rmp-serde` upgrade shifting the wire, so opaque hex is the
//! right artifact there. These goldens have a different job: they pin the
//! **tree shape** — which node, which id, which class, which string — so that
//! a change to the row's layout shows up as a diff a human reads in review.
//! A `{:#?}` of the `View` renders *every* field of every node, so nothing can
//! move without moving the golden, and the diff says what moved.
//!
//! # Regenerating
//!
//! Only for an **intentional** layout change — never to make a red run pass:
//!
//! ```sh
//! cargo test -p hytte-plugin-agents --test view_golden -- --ignored --nocapture regenerate
//! ```
//!
//! It rewrites every `tests/fixtures/view_*.txt` from the table below and then
//! deliberately fails, so a regeneration run can never pass silently. Read
//! `git diff` before committing: a change you cannot explain from the source
//! change is exactly the regression this suite exists to catch.

use hytte_plugin::{Input, Plugin, cmd_channel};
use hytte_plugin_agents::Agents;
use hytte_plugin_agents::config::AgentsConfig;
use hytte_plugin_agents::hive::client::HiveError;
use hytte_plugin_agents::hive::wire::{AgentStatusRow, Response, VersionMismatch};
use hytte_plugin_agents::poll::Msg;
use std::path::{Path, PathBuf};

// ── fixtures ─────────────────────────────────────────────────────────────────

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn roster(fixture: &str) -> Vec<AgentStatusRow> {
    let path = fixtures_dir().join(fixture);
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing fixture {} ({e})", path.display()));
    let resp: Response = serde_json::from_str(body.trim()).expect("the fixture decodes");
    resp.agent_statuses.expect("a roster")
}

fn config(body: &str) -> AgentsConfig {
    hytte_config::subsystem::assemble::<AgentsConfig>(&[(
        PathBuf::from("overlay.toml"),
        body.to_owned(),
    )])
    .expect("the golden's config assembles")
    .config
}

/// A fixed clock, so every age label in a golden is deterministic.
/// `2026-09-07T12:45:00Z` — five minutes after the grouped fixture's
/// `status_set_at`, so the panel renders a real "5m ago" rather than
/// "just now".
const GOLDEN_NOW: i64 = 1_788_785_100;

fn seed(now_unix: i64) -> Agents {
    // The receiver is dropped immediately: no golden clicks a button that
    // queues a frame, and `CmdSender::send` on a dropped receiver is a
    // documented no-op the SDK tells callers to ignore.
    let (tx, _rx) = cmd_channel();
    let mut m = Agents::with_cmds(tx);
    m.update(Input::Snapshot(hytte_plugin::proto::StateSnapshot {
        clock: Some(hytte_plugin::proto::ClockState {
            unix: now_unix,
            iso: "2026-09-07T12:45:00Z".to_owned(),
        }),
    }));
    m
}

// ── the scenario table ───────────────────────────────────────────────────────

/// Every golden, by name. Each builds a model and returns it; the test both
/// checks and (when regenerating) rewrites `view_<name>.txt`.
fn scenarios() -> Vec<(&'static str, Agents)> {
    vec![
        ("connecting", seed(GOLDEN_NOW)),
        ("unreachable", {
            let mut m = seed(GOLDEN_NOW);
            m.update(Input::App(Msg::Status(Err(HiveError::Unreachable {
                reason: "permission denied — needs `hive-admin` group (re-login after adding)"
                    .to_owned(),
            }))));
            m
        }),
        // A hive that is up and saying no — visibly different chrome from
        // "no hive", which is the whole point of the state existing.
        ("hive_error", {
            let mut m = seed(GOLDEN_NOW);
            m.update(Input::App(Msg::Status(Err(HiveError::Refused {
                reason: "agent \"ghost\" is not managed by this hive".to_owned(),
            }))));
            m
        }),
        ("incompatible", {
            let mut m = seed(GOLDEN_NOW);
            m.update(Input::App(Msg::Status(Err(HiveError::Version(
                VersionMismatch {
                    theirs: 99,
                    ours: 1,
                },
            )))));
            m
        }),
        ("empty", {
            let mut m = seed(GOLDEN_NOW);
            m.update(Input::App(Msg::Status(Ok(Vec::new()))));
            m
        }),
        // The five §6.2 precedence rows in one card, ungrouped — so the header
        // suppression rule is visible in the same golden.
        ("precedence", {
            let mut m = seed(GOLDEN_NOW);
            m.update(Input::App(Msg::Status(Ok(roster(
                "agent_status_precedence.json",
            )))));
            m
        }),
        // Three agents across two projects plus an unlabelled one — the
        // grouping golden §12 asks for, headers and all.
        ("grouped", {
            let mut m = seed(GOLDEN_NOW);
            m.update(Input::App(Msg::Config(Box::new(config(
                "[display.trollshell-choom]\nlabel = \"choom\"\nicon = \"starred-symbolic\"\nproject = \"viberoot\"\n\n[display.nixos-choom]\nproject = \"nixos\"\n",
            )))));
            m.update(Input::App(Msg::Status(Ok(roster(
                "agent_status_grouped.json",
            )))));
            m
        }),
        // Twelve agents in one ungrouped hive — @kaesaecracker's shape, and
        // the case the compact row exists for. Pinned so a regression back to
        // two-line rows shows up as a diff, not as a screenshot round trip.
        ("twelve_ungrouped", {
            let mut m = seed(GOLDEN_NOW);
            m.update(Input::App(Msg::Status(Ok(many_agents(12)))));
            m
        }),
        // The same roster with one agent selected — the §6.4 panel in full.
        ("panel_selected", {
            let mut m = seed(GOLDEN_NOW);
            m.update(Input::App(Msg::Status(Ok(roster(
                "agent_status_grouped.json",
            )))));
            m.update(Input::App(Msg::Urls(Box::new(
                hytte_plugin_agents::hive::wire::HiveUrls {
                    domain: Some("hive.local".to_owned()),
                    home: Some("https://hive.local/".to_owned()),
                },
            ))));
            m.update(Input::event(
                "edit:trollshell-choom",
                hytte_plugin::proto::EventKind::Click,
            ));
            m
        }),
    ]
}

/// `n` running agents named `agent-0…`, in one ungrouped hive.
fn many_agents(n: usize) -> Vec<AgentStatusRow> {
    (0..n)
        .map(|i| AgentStatusRow {
            name: format!("agent-{i}"),
            running: true,
            status_text: Some(format!("working on task {i} of a rather long description")),
            ..AgentStatusRow::default()
        })
        .collect()
}

/// Goldens rendered from a **node function** rather than from `Plugin::view`.
///
/// `View`-level goldens cannot reach the bounded panel shape: `Plugin::view`
/// goes through `panel_ctx`, which resolves `Node::Scrolled`'s vocabulary
/// negotiation from `hytte_plugin::nodes::host_speaks_scrolled()`, and the
/// setter behind that — `display::set_negotiated` — is `pub(crate)`, so a test
/// **outside `hytte-plugin`** cannot raise it (its own tests do, four times).
/// The negotiation is not the plugin's to fake either.
///
/// `view::panel` is parameterised on the resolved number, though, so calling it
/// directly with `viewport_px: PANEL_VIEWPORT_PX` pins the shape a shell at or
/// past #969 actually renders — which is otherwise pinned only by a unit test's
/// `matches!`, leaving the artifact a human reads in review showing a fallback
/// no current host produces (#963 review, MED-3).
fn node_scenarios() -> Vec<(&'static str, hytte_plugin::proto::Node)> {
    let cfg = config("[display.trollshell-choom]\nlabel = \"choom\"\n");
    let hive = {
        let mut m = seed(GOLDEN_NOW);
        m.update(Input::App(Msg::Status(Ok(roster(
            "agent_status_grouped.json",
        )))));
        m.hive
    };
    let urls = hytte_plugin_agents::hive::wire::HiveUrls {
        domain: Some("hive.local".to_owned()),
        home: Some("https://hive.local/".to_owned()),
    };
    let ctx = |viewport_px| hytte_plugin_agents::view::PanelContext {
        now_unix: GOLDEN_NOW,
        last_poll_unix: Some(GOLDEN_NOW - 60),
        socket: "/run/hive/host.sock",
        urls: Some(&urls),
        viewport_px,
    };
    let name = hytte_plugin_agents::model::AgentName::parse("trollshell-choom").expect("legal");

    vec![
        // The shape a shell at or past #969 renders: the body inside a bounded
        // viewport.
        (
            "panel_bounded",
            hytte_plugin_agents::view::panel(
                &hive,
                &cfg,
                None,
                ctx(hytte_plugin_agents::view::PANEL_VIEWPORT_PX),
            ),
        ),
        // A selection the roster cannot resolve — the MED-1 shape: a notice, a
        // way back, and the overview underneath it.
        (
            "panel_stale_selection",
            hytte_plugin_agents::view::panel(
                &hytte_plugin_agents::model::Hive::Unreachable {
                    reason: "connection refused".to_owned(),
                },
                &cfg,
                Some(&name),
                ctx(hytte_plugin_agents::view::PANEL_VIEWPORT_PX),
            ),
        ),
        // One pill per state, each as its own artifact (Annika's v1 card,
        // 2026-09-11). The card-level goldens above already contain rows in
        // every state, but they contain a whole card around them: these three
        // are the **row**, so the thing a reviewer diffs when the pill changes
        // is forty lines rather than four hundred, and the one thing that
        // differs between them — the state glyph on line 2 and which verb the
        // lifecycle button offers — is the whole file.
        (
            "row_running",
            pill(AgentStatusRow {
                name: "trollshell-choom".to_owned(),
                running: true,
                active_model: Some("claude-opus-4-6".to_owned()),
                status_text: Some("Clauding…".to_owned()),
                url: Some("https://hive.local/agent/trollshell-choom/".to_owned()),
                ..AgentStatusRow::default()
            }),
        ),
        (
            "row_paused",
            pill(AgentStatusRow {
                name: "trollshell-choom".to_owned(),
                running: true,
                paused: true,
                active_model: Some("claude-sonnet-4-6".to_owned()),
                status_text: Some("Clauding…".to_owned()),
                ..AgentStatusRow::default()
            }),
        ),
        (
            "row_stopped",
            pill(AgentStatusRow {
                name: "trollshell-choom".to_owned(),
                running: false,
                needs_update: true,
                active_model: Some("opus-5.2-20262981923899321898".to_owned()),
                ..AgentStatusRow::default()
            }),
        ),
    ]
}

/// One agent's card row, alone — the smallest tree that is a whole pill.
///
/// Built through [`view::card`] rather than by calling the private `agent_row`,
/// because the row's wrappers (the dense `ListBox`, the group suppression) are
/// part of what the pill renders as; the golden is then the card with exactly
/// one row in it.
fn pill(row: AgentStatusRow) -> hytte_plugin::proto::Node {
    let mut m = seed(GOLDEN_NOW);
    m.update(Input::App(Msg::Status(Ok(vec![row]))));
    hytte_plugin_agents::plugin::card_of(&m)
}

fn golden_path(name: &str) -> PathBuf {
    fixtures_dir().join(format!("view_{name}.txt"))
}

fn render(model: &Agents) -> String {
    format!("{:#?}\n", model.view())
}

// ── the tests ────────────────────────────────────────────────────────────────

/// Every golden this suite owns, as `(name, rendered)` — the `View`-level ones
/// and the node-level ones in one list, so both are checked and regenerated by
/// exactly the same code.
fn all_goldens() -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = scenarios()
        .into_iter()
        .map(|(name, m)| (name, render(&m)))
        .collect();
    out.extend(
        node_scenarios()
            .into_iter()
            .map(|(name, node)| (name, format!("{node:#?}\n"))),
    );
    out
}

#[test]
fn render_trees_match_their_goldens() {
    let mut mismatches = Vec::new();
    for (name, got) in all_goldens() {
        let path = golden_path(name);
        let want = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "missing golden {} ({e}) — run `cargo test -p hytte-plugin-agents --test view_golden -- --ignored --nocapture regenerate` and commit the result",
                path.display()
            )
        });
        if got != want {
            mismatches.push(name);
            // Print the first differing line so a CI log says what moved
            // without dumping two whole trees.
            let first = want
                .lines()
                .zip(got.lines())
                .enumerate()
                .find(|(_, (w, g))| w != g);
            match first {
                Some((i, (w, g))) => {
                    eprintln!(
                        "view_{name}.txt line {}:\n  golden: {w}\n  actual: {g}",
                        i + 1
                    );
                }
                None => eprintln!("view_{name}.txt differs in length only"),
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "render trees drifted from their goldens: {mismatches:?} — if the change is intentional, \
         regenerate with `cargo test -p hytte-plugin-agents --test view_golden -- --ignored \
         --nocapture regenerate` and read the diff"
    );
}

/// Every scenario must actually differ from every other one — otherwise a
/// golden could be silently duplicated and stop testing anything.
#[test]
fn every_golden_scenario_renders_something_distinct() {
    let rendered = all_goldens();
    for (i, (a_name, a)) in rendered.iter().enumerate() {
        for (b_name, b) in rendered.iter().skip(i + 1) {
            assert_ne!(a, b, "{a_name} and {b_name} render identically");
        }
    }
}

/// The grouped golden must actually carry the group headers, and the
/// precedence one must not — the §6.3 suppression rule, asserted here as well
/// as pinned in the golden so a regenerate cannot quietly bless its loss.
#[test]
fn headers_appear_only_when_there_is_more_than_one_project() {
    let rendered: std::collections::HashMap<&str, String> = scenarios()
        .into_iter()
        .map(|(name, m)| (name, render(&m)))
        .collect();
    let grouped = &rendered["grouped"];
    assert!(grouped.contains("ts-agents-group"), "grouped needs headers");
    assert!(grouped.contains("viberoot"), "{grouped}");
    assert!(grouped.contains("nixos"), "{grouped}");
    assert!(
        grouped.contains("ungrouped"),
        "the unlabelled agent's bucket"
    );

    let precedence = &rendered["precedence"];
    assert!(
        !precedence.contains("ts-agents-group"),
        "one project must suppress the header"
    );
}

#[test]
#[ignore = "regenerates the committed goldens; run deliberately, then read the diff"]
fn regenerate() {
    for (name, rendered) in all_goldens() {
        let path = golden_path(name);
        std::fs::write(&path, rendered).expect("write the golden");
        println!("wrote {}", path.display());
    }
    panic!(
        "goldens regenerated — this failure is deliberate so a regeneration run can never pass \
         silently. Inspect `git diff crates/hytte-plugin-agents/tests/fixtures/view_*.txt` before \
         committing."
    );
}
