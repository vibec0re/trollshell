//! `--open-all` as the **real binary**: `main`'s dispatch, `config::load`,
//! the roster read and every exit code this mode can produce
//! ([#1306](https://github.com/vibec0re/trollshell/issues/1306); added by the
//! #1390 review, MED 2).
//!
//! # Why a child process and not a call
//!
//! `open_all::open_all_with(&mut impl Compositor, &mut impl Spawner, …)` is
//! covered from every angle by `src/open_all.rs`'s unit tests, and
//! `tests/open_all.rs` covers the socket half. What neither can reach is the
//! **wrapper**: `open_all::run` — `config::load` → `roster` → `running_agents`
//! → the real `SocketCompositor`/`DetachedSpawner` → an exit code — and
//! `main`'s one-line `Ok(Invocation::OpenAll) => …` arm. Both were unpinned:
//! the review measured that replacing `run`'s tail with a bare `EXIT_OK`, and
//! separately never calling `run` at all, left the whole suite green. That is
//! the seam-extracted / wrapper-inert shape the tree has closed before
//! (#1321/#1322).
//!
//! The contract this feature leans on hardest lives there and nowhere else:
//! **nothing running is exit 0**, because a non-zero exit would make the
//! keybind in `etc/niri/binds.kdl` look broken.
//!
//! # Why this is hermetic
//!
//! Every child runs with `XDG_CONFIG_HOME` on a tempdir (so it reads *our*
//! `agents.toml` and never the operator's — the #1101 rule), `XDG_CONFIG_DIRS`
//! on an empty one, `NIRI_SOCKET` naming a path that does not exist, and
//! **`PATH` on an empty directory**. That last one is what makes the "a launch
//! was attempted and failed" case safe to assert: `DetachedSpawner` tries
//! `systemd-run` (not found), falls back to spawning the window directly (not
//! found either), and nothing is ever started on the machine running the
//! tests. No display is needed because `main` returns from the `--open-all`
//! arm before any `GApplication` exists.

mod fake;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use fake::FakeHive;

/// The binary under test. Cargo hands an integration test the path to its own
/// crate's `[[bin]]`, which is what makes this a test of `main` rather than of
/// a function `main` happens to call.
const BIN: &str = env!("CARGO_BIN_EXE_trollshell-agent-window");

/// Exit code for "the fan-out did what it was asked".
const EXIT_OK: i32 = 0;
/// Exit code for "the hive could not be read, or nothing launched".
const EXIT_FAILED: i32 = 1;
/// Exit code for a command line this binary cannot use (`main`'s `EXIT_USAGE`).
const EXIT_USAGE: i32 = 2;

/// Run the real binary with `args` in an environment that reaches nothing on
/// this machine. `socket`, when given, is written into an `agents.toml` the
/// child will read.
fn run(args: &[&str], socket: Option<&Path>) -> Output {
    let home = tempfile::tempdir().expect("a tempdir");
    let empty = tempfile::tempdir().expect("a tempdir");
    if let Some(socket) = socket {
        let dir = home.path().join("trollshell");
        std::fs::create_dir_all(&dir).expect("the config dir");
        std::fs::write(
            dir.join("agents.toml"),
            // `{:?}` on the path's string is a TOML basic string, and a
            // tempdir path carries nothing that needs escaping beyond it.
            format!("socket = {:?}\n", socket.display().to_string()),
        )
        .expect("agents.toml");
    }
    Command::new(BIN)
        .args(args)
        .env("XDG_CONFIG_HOME", home.path())
        .env("XDG_CONFIG_DIRS", empty.path())
        .env("XDG_STATE_HOME", empty.path())
        .env("HOME", home.path())
        // Empty on purpose: neither `systemd-run` nor the window binary can be
        // resolved, so a launch fails instead of starting something.
        .env("PATH", empty.path())
        .env("NIRI_SOCKET", empty.path().join("no-niri.sock"))
        .env("RUST_LOG", "info")
        .output()
        .expect("the binary runs")
}

/// Everything the child said, both streams.
///
/// **Both**, because they are genuinely split and it is easy to assert against
/// the wrong one: `main` installs `tracing_subscriber::fmt`, whose default
/// writer is `io::stdout`, so every `tracing::info!`/`error!` — the roster
/// failure, the "nothing to open" line, the per-window launch results — lands
/// on **stdout**, while `main`'s own usage line is an `eprintln!` on stderr.
/// (Writing these assertions is what surfaced that; the module doc and
/// `etc/niri/README.md` said "stderr" for the log lines and now say which is
/// which.)
fn said(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// **Nothing running is exit 0.** The whole reason the contract is written
/// down: a keybind whose command exits non-zero looks broken, and "the hive is
/// up and nobody is running" is an answer rather than a failure.
///
/// Falsification (verified red): return `EXIT_FAILED` from `run`'s
/// `agents.is_empty()` arm and this reds with `code: Some(1)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_roster_exits_zero_with_one_line() {
    let hive = FakeHive::script(&[r#"{"version":1,"ok":true,"agent_statuses":[]}"#]);
    let socket = hive.path().to_owned();
    let out = tokio::task::spawn_blocking(move || run(&["--open-all"], Some(&socket)))
        .await
        .expect("the child ran");

    assert_eq!(out.status.code(), Some(EXIT_OK), "{}", said(&out));
    assert!(
        said(&out).contains("nothing to open"),
        "the one line an operator gets: {}",
        said(&out)
    );
    // …and it asked the hive exactly what the card asks.
    let asked = hive.seen();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].contains("agent_status"), "{asked:?}");
}

/// A roster with a running agent, and a desktop where **no launch can
/// succeed** (empty `PATH`): `run` counts the failures and exits non-zero.
///
/// This is the one case that reaches `run`'s tail — the
/// `if report.launched == 0 { EXIT_FAILED }` the review replaced with a bare
/// `EXIT_OK` to find the suite green. The empty-roster and absent-socket cases
/// both return before it.
///
/// Falsification (verified red): replace that tail with `EXIT_OK` and this
/// reds with `code: Some(0)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_where_no_window_could_be_launched_exits_nonzero() {
    let hive = FakeHive::script(&[
        r#"{"version":1,"ok":true,"agent_statuses":[{"name":"argus","running":true,"failed":false,"needs_login":false,"paused":false}]}"#,
    ]);
    let socket = hive.path().to_owned();
    let out = tokio::task::spawn_blocking(move || run(&["--open-all"], Some(&socket)))
        .await
        .expect("the child ran");

    assert_eq!(out.status.code(), Some(EXIT_FAILED), "{}", said(&out));
    assert!(
        said(&out).contains("could not be launched"),
        "the failure names the window: {}",
        said(&out)
    );
}

/// A hive that is not there exits 1, carrying the client's own sentence — the
/// arm an operator hits when `hive-c0re` is stopped.
///
/// Falsification: return `EXIT_OK` from `run`'s roster-error arm and this reds
/// on the code.
#[test]
fn an_absent_socket_exits_one_with_the_hives_own_reason() {
    let dir = tempfile::tempdir().expect("a tempdir");
    let out = run(&["--open-all"], Some(&dir.path().join("nothing-here.sock")));

    assert_eq!(out.status.code(), Some(EXIT_FAILED), "{}", said(&out));
    assert!(
        said(&out).contains("hive-c0re is not running"),
        "{}",
        said(&out)
    );
}

/// `main`'s **usage** arm, both ways in: an argument this binary does not
/// know, and `--open-all` combined with a flag that describes one window.
///
/// Each prints its own sentence **and** the usage block (`main.rs` prints
/// both; the PR body's first pasted block showed only the sentence — LOW 4),
/// and exits 2.
///
/// Falsification (verified red): return `open_all::run()`'s code from the
/// `Err` arm, or drop `cli::USAGE` from the `eprintln!`, and the matching
/// assertion goes.
#[test]
fn a_command_line_this_binary_cannot_use_exits_two_with_the_usage_block() {
    let unknown = run(&["--verbose"], None);
    assert_eq!(unknown.status.code(), Some(EXIT_USAGE));
    assert!(
        said(&unknown).contains(r#"unknown argument "--verbose""#),
        "{}",
        said(&unknown)
    );
    assert!(
        said(&unknown).contains("usage: trollshell-agent-window --agent <name>"),
        "the usage block comes with it: {}",
        said(&unknown)
    );

    let conflict = run(&["--open-all", "--agent", "stray"], None);
    assert_eq!(conflict.status.code(), Some(EXIT_USAGE));
    assert!(
        said(&conflict).contains("cannot be combined with --agent"),
        "{}",
        said(&conflict)
    );
    assert!(
        said(&conflict).contains("or: trollshell-agent-window --open-all"),
        "the usage block comes with it here too: {}",
        said(&conflict)
    );
}

/// `main` dispatches `--open-all` to `open_all::run` and **not** to the window
/// arm: no `GApplication` is registered, nothing waits for a display, and the
/// process exits on its own.
///
/// Asserted as "it answered a hive question rather than a usage one" —
/// `EXIT_FAILED` plus the roster sentence is only reachable through `run`.
///
/// Falsification (verified red): return `EXIT_USAGE` from `main`'s `OpenAll`
/// arm without calling `run`, and this reds on both the code and the sentence.
#[test]
fn main_hands_open_all_to_the_fan_out_and_never_builds_a_window() {
    let dir = tempfile::tempdir().expect("a tempdir");
    let socket: PathBuf = dir.path().join("nothing-here.sock");
    let out = run(&["--open-all"], Some(&socket));

    assert_ne!(
        out.status.code(),
        Some(EXIT_USAGE),
        "the fan-out arm was reached, not the usage one: {}",
        said(&out)
    );
    assert!(
        said(&out).contains(&socket.display().to_string()),
        "it read OUR agents.toml and dialled the socket in it: {}",
        said(&out)
    );
}
