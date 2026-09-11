//! #1024 (a #1004 second-pass follow-up, N3): pins the two `serve`-level
//! mechanisms the review's own throwaway harness proved but that never landed
//! in the tree — the reviewer measured that deleting either one left the
//! *unit* test suite green, because the internal `take_socket` tests drive
//! the decision with a local `owned` parameter, which cannot see whether
//! `serve` actually threads the process-wide listener through at all.
//!
//! #1059 added scenario D on the same harness for a different `serve`-level
//! property: that a synchronous step inside `serve` does not stop the
//! current-thread runtime `serve` is spawned onto in production.
//!
//! Every scenario needs a real second OS process, for two independent reasons:
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

use std::fmt::Write as _;
use std::io::Read as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hytte_plugin_infobroker::broker::{GrantView, serve_with_grant_loader, serve_with_shutdown};
use hytte_plugin_infobroker::grants::{Grant, GrantStore, to_toml};
use hytte_plugin_infobroker::paths::{GRANTS_FILE, SOCKET_FILE, STATE_DIR};
use hytte_plugin_infobroker::{BrokerMsg, BrokerSnapshot, Cmd, serve};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

/// Set (to any value) only on the re-exec'd child that is meant to actually
/// run one scenario's `_inner` test; see the module doc.
const SCENARIO_MARKER: &str = "INFOBROKER_TEST_SCENARIO";

/// #1024 review M1: printed by each `_inner` test (via `--nocapture`, already
/// passed) only after it reaches the end of its real scenario body, i.e. only
/// on the path that actually ran the scenario and passed. `run_inner` asserts
/// this string is present in the child's stdout as well as `out.status.success()`
/// — the exit-code check alone stays green when a renamed `_inner` fn leaves
/// `run_inner`'s literal name stale (0 tests match the filter, `0 passed`,
/// exit 0) or when `in_scenario_child()` never sees the marker (the `_inner`
/// test returns immediately, exit 0). Neither of those can print this line.
const SCENARIO_OK_PREFIX: &str = "SCENARIO_OK ";

/// #1024 review L4: upper bound on how long a re-exec'd scenario child may
/// run before `run_inner` gives up and kills it. Every regression this file
/// models today fails inside its own internal deadlines (at most
/// `WRITE_PARK_GIVE_UP` = 10 s) — this is a backstop against a *future*
/// regression that wedges an `_inner` test before it ever reaches one of
/// those, so the test binary (and CI) fails in bounded time instead of
/// hanging until the job's own outer limit.
const CHILD_GIVE_UP: Duration = Duration::from_secs(30);

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
/// `{"op":"grants"}` response is comfortably larger than any default Linux
/// UDS socket buffer (`wmem_default`/`rmem_default` = 212 992 B on a typical
/// kernel), so a client that never reads it genuinely blocks the write
/// instead of the whole reply sliding into kernel slack and returning
/// instantly regardless of any bound.
///
/// #1024 review New-1: was `80_000` (~7.6 MB of JSON), which left scenario
/// C's session-1/session-2 seed bounds (`GIVE_UP`/`WRITE_PARK_GIVE_UP`) on as
/// little as 0.83× margin under ~2x CPU oversubscription — `GrantStore::load`
/// parses this file synchronously, twice per scenario run (once per
/// session), and that cost is pure CPU, so it scales straight with
/// contention. `ubuntu-latest` is 4 vCPU and this suite runs twice per `nix
/// flake check` alongside two `nixosTest` VMs and the package build's
/// `doCheck`, i.e. genuinely oversubscribed. Measured (112 burners on 64
/// cores): `8_000` (~760 KB, still ≈2× the combined default UDS buffers, so
/// the write still genuinely blocks) restores the margin to 7.63×/5.73× on
/// the two bounds while keeping mutation (c) RED and the suite green.
const HUGE_GRANT_COUNT: usize = 8_000;

/// Scenario D (#1059): how long the injected synchronous grant loader blocks.
/// Twenty times [`STARVED_TIMER`], so the two arms of the mutation are never
/// in doubt — on the fixed tree the timer fires at ~[`STARVED_TIMER`], on the
/// mutated one it cannot fire before this elapses.
const SLOW_LOAD: Duration = Duration::from_secs(2);

/// Scenario D: the concurrent timer whose punctuality is the property under
/// test — the stand-in for the broker's own `REQUEST_TIMEOUT` /
/// `CONSENT_PARK_TIMEOUT` / `QUERY_PARK_TIMEOUT` bounds and the SDK's clock
/// pump, all of which live on the same current-thread runtime as `serve`.
const STARVED_TIMER: Duration = Duration::from_millis(100);

/// Scenario D: how far past [`STARVED_TIMER`] the timer may land before the
/// test calls it starved.
///
/// Measured on the fixed tree, 20 runs of this scenario under 64 CPU burners
/// on 64 cores: the timer fired at 100.99–108.94 ms, i.e. an overshoot of
/// 1.0–8.9 ms — comfortably inside the "~50 ms" #1059 asks for. This constant
/// is nonetheless 150 ms, ~17× that worst observation, for the reason
/// `HUGE_GRANT_COUNT`'s note gives: CI is `ubuntu-latest`'s 4 vCPU running
/// this suite twice per `nix flake check` alongside two `nixosTest` VMs, where
/// a thread wakeup is a great deal less punctual than it is here, and a
/// wall-clock bound this test does not need to be tight is not worth a flake.
/// It costs the mutation nothing: the discriminating value is [`SLOW_LOAD`] =
/// 2 s, 8× this whole bound (measured under mutation (e): 2.000328324 s).
///
/// The wall-clock check is in any case the *second* assertion. The first —
/// the loader must still be mid-flight when the timer fires — is a pure
/// ordering property with no clock in it, and it is the one that cannot be
/// satisfied by a slow-but-not-starved runtime.
const STARVED_TIMER_SLACK: Duration = Duration::from_millis(150);

// ── Shared harness ──────────────────────────────────────────────────────────

/// #1024 review New-7: a byte cap alongside the line cap. With the
/// `{snap:?}`-formatted full-snapshot dumps gone (L3) nothing today emits a
/// single line long enough to blow this up on its own, but capping only
/// lines made that true by accident, not by construction — a future
/// regression that prints one very long line would sail straight through the
/// line cap.
const TAIL_MAX_BYTES: usize = 4096;

/// The head's own byte cap, the counterpart to [`TAIL_MAX_BYTES`] (#1059 item
/// 2). Smaller because the head exists to carry a `_inner` test's panic
/// message and its `assertion failed` block, not a transcript.
const HEAD_MAX_BYTES: usize = 2048;

/// How many of the FIRST lines of a child's captured output survive truncation
/// (#1059 item 2). See [`head_and_tail`].
const HEAD_LINES: usize = 10;

/// How many of the LAST lines survive truncation. `HEAD_LINES + TAIL_LINES`
/// is the 40 the pre-#1059 tail-only helper kept, so a short child's output
/// is reported exactly as it was.
const TAIL_LINES: usize = 30;

/// Which end of a string a byte cap keeps.
#[derive(Clone, Copy)]
enum Keep {
    /// Keep the first bytes, drop the rest (the head half).
    Start,
    /// Keep the last bytes, drop what precedes them (the tail half).
    End,
}

/// At most `max` bytes of `s` taken from the end `keep` names, split on a char
/// boundary, plus whether anything was actually dropped.
fn clamp_bytes(s: &str, max: usize, keep: Keep) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    match keep {
        Keep::Start => {
            let mut end = max;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            (&s[..end], true)
        }
        Keep::End => {
            let mut start = s.len() - max;
            while !s.is_char_boundary(start) {
                start += 1;
            }
            (&s[start..], true)
        }
    }
}

/// The first [`HEAD_LINES`] and last [`TAIL_LINES`] lines of `s`, each further
/// byte-capped ([`HEAD_MAX_BYTES`] / [`TAIL_MAX_BYTES`]), with a
/// byte/line-count header and an explicit elision marker whenever anything is
/// dropped. Returns `s` verbatim when nothing needs trimming.
///
/// #1024 review L3 (the tail half): a failing scenario's child can legitimately
/// print megabytes (a `{:?}`-formatted `BrokerSnapshot` holding
/// `HUGE_GRANT_COUNT` grants used to do exactly that), and dumping all of it
/// into a panic message is how one failing run put 6.6 MB into a CI log.
///
/// #1059 item 2 (the head half): keeping *only* the tail loses the one line a
/// red is diagnosed from as soon as backtraces are on. The devShell sets
/// `RUST_BACKTRACE=1` (`nix/devshell.nix`), a panicking `_inner` test's
/// backtrace is ~10 KB, and libtest prints the panic message *before* it —
/// so the message fell off the front of a 40-line / 4 KiB tail and the
/// developer got a stack of `core::panicking` frames with no reason attached
/// (measured on the #1033 tree: `grep -c "another info broker"` over a
/// deliberately reddened scenario B = 0 with backtraces on, 2 with them off).
/// CI, which does not set the variable, never saw it — so this is a dev-loop
/// fix, not a CI one.
fn head_and_tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let head_n = HEAD_LINES.min(lines.len());
    let tail_n = TAIL_LINES.min(lines.len() - head_n);
    let elided = lines.len() - head_n - tail_n;

    let head_src = lines[..head_n].join("\n");
    let tail_src = lines[lines.len() - tail_n..].join("\n");
    let (head, head_cut) = clamp_bytes(&head_src, HEAD_MAX_BYTES, Keep::Start);
    let (tail, tail_cut) = clamp_bytes(&tail_src, TAIL_MAX_BYTES, Keep::End);

    if elided == 0 && !head_cut && !tail_cut {
        return s.to_owned();
    }
    let mut out = format!(
        "[{} bytes, {} lines total — showing the first {head_n} and last {tail_n} lines]\n",
        s.len(),
        lines.len(),
    );
    out.push_str(head);
    if head_cut {
        out.push_str("\n… head truncated at a byte cap …");
    }
    if elided > 0 {
        let noun = if elided == 1 { "line" } else { "lines" };
        write!(out, "\n… {elided} {noun} elided …").expect("writing to a String cannot fail");
    }
    if tail_n > 0 {
        out.push('\n');
        if tail_cut {
            out.push_str("… tail truncated at a byte cap …\n");
        }
        out.push_str(tail);
    }
    out
}

/// #1059 item 2, the property the head half exists for: a panic message that a
/// long backtrace has pushed 197 lines from the end still reaches the panic
/// text `run_inner` prints.
///
/// RED under the pre-#1059 helper (`tail_lines(s, 40)`, tail-only): line 3 of
/// 200 is not in the last 40, so it could not appear in the output at all —
/// which is exactly what `RUST_BACKTRACE=1` (set by `nix/devshell.nix`) did to
/// every failing scenario in the dev loop.
///
/// Also RED under a head-only mutation (drop the tail half): the last line
/// must still be there too.
#[test]
fn head_and_tail_keeps_a_panic_line_a_backtrace_pushed_off_the_end() {
    const PANIC_LINE: &str =
        "thread 'main' panicked at tests/serve_socket_handover.rs:1: another info broker";
    let mut lines = vec![
        "running 1 test".to_owned(),
        String::new(),
        PANIC_LINE.to_owned(),
    ];
    // The shape libtest prints *after* the message: ~10 KB of frames.
    for i in 0..197 {
        lines.push(format!(
            "  {i:>3}: 0x00007f0000000000 - core::panicking::panic_fmt::h{i:016x}"
        ));
    }
    let input = lines.join("\n");
    assert_eq!(input.lines().count(), 200, "the fixture must be 200 lines");

    let out = head_and_tail(&input);
    assert!(
        out.contains(PANIC_LINE),
        "the panic line (line 3 of 200) must survive truncation — RED tail-only:\n{out}",
    );
    assert!(
        out.contains(lines.last().expect("the fixture is non-empty").as_str()),
        "the last line must survive too — RED head-only:\n{out}",
    );
    assert!(
        !out.contains(lines[100].as_str()),
        "the middle must genuinely be dropped, not merely reordered:\n{out}",
    );
    assert!(
        out.contains("lines elided"),
        "an elision must be announced, so nobody reads the join as contiguous output:\n{out}",
    );
    assert!(
        out.len() < input.len(),
        "truncation must actually shrink the output ({} vs {})",
        out.len(),
        input.len(),
    );
}

/// #1033 third pass ("`tail_lines`' byte cap … has no test of its own"): one
/// enormous line trips no line cap at all, so only a byte cap can hold it.
/// A single line is entirely head (there are no lines left over for a tail),
/// so this is the head cap's test. RED if the head [`clamp_bytes`] is dropped.
#[test]
fn head_and_tail_byte_caps_one_enormous_line() {
    let huge = "x".repeat(HEAD_MAX_BYTES + TAIL_MAX_BYTES + 10_000);
    let out = head_and_tail(&huge);
    assert!(
        out.len() < huge.len(),
        "a single {}-byte line must still be capped ({} bytes out)",
        huge.len(),
        out.len(),
    );
    assert!(
        out.contains("head truncated at a byte cap"),
        "the head byte cap must announce itself:\n{out}",
    );
}

/// The tail cap's own test, and the reason it is separate: the single-line
/// fixture above leaves `tail_n == 0`, so it stays green with the tail
/// [`clamp_bytes`] deleted outright (measured — mutation (i) passed 3/3 until
/// this test existed). Only a fixture with an over-long line in *both* halves
/// exercises the two caps independently.
///
/// RED if either [`clamp_bytes`] call is dropped.
#[test]
fn head_and_tail_byte_caps_each_half_independently() {
    let mut lines = vec!["H".repeat(HEAD_MAX_BYTES + 1_000)];
    for i in 1..49 {
        lines.push(format!("filler line {i}"));
    }
    lines.push("T".repeat(TAIL_MAX_BYTES + 1_000));
    let input = lines.join("\n");
    assert_eq!(
        input.lines().count(),
        50,
        "the fixture must exceed HEAD_LINES + TAIL_LINES so both halves are real",
    );

    let out = head_and_tail(&input);
    assert!(
        out.contains("head truncated at a byte cap"),
        "the head half must be byte-capped:\n{}",
        &out[..out.len().min(200)],
    );
    assert!(
        out.contains("tail truncated at a byte cap"),
        "the tail half must be byte-capped too — RED under mutation (i)",
    );
    assert!(
        out.len() < HEAD_MAX_BYTES + TAIL_MAX_BYTES + 500,
        "the two caps plus the header/markers must bound the whole output, got {} bytes",
        out.len(),
    );
}

/// Output short enough on both axes comes back byte-for-byte, with no header —
/// the pre-#1059 helper's contract, preserved.
#[test]
fn head_and_tail_leaves_short_output_verbatim() {
    let short = "running 1 test\nSCENARIO_OK whatever\ntest result: ok.";
    assert_eq!(head_and_tail(short), short);
}

/// Re-execute this test binary, filtered to exactly `inner_test_name`, with
/// `runtime_dir`/`state_dir` as `XDG_RUNTIME_DIR`/`XDG_STATE_HOME` and
/// [`SCENARIO_MARKER`] set.
///
/// Bounded (#1024 review L4): the child is polled with `try_wait` rather than
/// the blocking `Command::output()`, and killed if it outruns
/// [`CHILD_GIVE_UP`], so a future regression that wedges an `_inner` test
/// before its own internal deadlines fails this test binary in bounded time
/// instead of hanging it (and the CI job) forever. stdout/stderr are drained
/// on their own threads *while* polling, not collected only after exit —
/// collecting after exit would deadlock if the child ever writes more than a
/// pipe buffer's worth before this function notices it exited.
///
/// Panics (with the head and tail of the child's stdout+stderr — see
/// [`head_and_tail`]) on a non-zero exit, so a mutation shows up as a named test
/// failure rather than a silent skip. Also asserts (#1024 review M1) that the
/// child's stdout contains a [`SCENARIO_OK_PREFIX`] line naming this exact
/// `inner_test_name` — printed only once the `_inner` test has run its real
/// scenario body to completion, which `out.status.success()` alone cannot
/// distinguish from "the filter matched nothing" (a renamed `_inner` fn, with
/// `run_inner`'s literal name string left stale) or "the marker never reached
/// the child" (`in_scenario_child()` false, so the `_inner` test returns
/// immediately) — both exit 0 with no scenario ever having run.
fn run_inner(inner_test_name: &str, runtime_dir: &Path, state_dir: &Path) {
    let args = [
        "--exact",
        "--nocapture",
        "--test-threads=1",
        inner_test_name,
    ];
    // #1024 review New-6: the whole "exactly one `_inner` test per re-exec"
    // guarantee rests on `--exact` actually being in this argv — assert it
    // rather than only trusting the literal array above never drifts, since
    // a future refactor that builds `args` differently (e.g. conditionally)
    // could drop it silently.
    assert!(
        args.contains(&"--exact"),
        "run_inner must always re-exec with --exact, or a dropped filter could run more than \
         the intended _inner test (#1024 review New-6)",
    );
    let exe = std::env::current_exe().expect("this test binary's own path");
    let mut child = std::process::Command::new(exe)
        .args(args)
        .env(SCENARIO_MARKER, "1")
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_STATE_HOME", state_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn this test binary for a hermetic scenario");

    let mut stdout_pipe = child.stdout.take().expect("child's stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("child's stderr was piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + CHILD_GIVE_UP;
    let (status, timed_out) = loop {
        if let Some(status) = child
            .try_wait()
            .expect("poll the re-exec'd child's exit status")
        {
            break (status, false);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let status = child.wait().expect("wait for the killed child to reap it");
            break (status, true);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = String::from_utf8_lossy(
        &stdout_reader
            .join()
            .expect("join the stdout-draining thread"),
    )
    .into_owned();
    let stderr = String::from_utf8_lossy(
        &stderr_reader
            .join()
            .expect("join the stderr-draining thread"),
    )
    .into_owned();

    assert!(
        !timed_out,
        "{inner_test_name} did not exit within {CHILD_GIVE_UP:?} and was killed.\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        head_and_tail(&stdout),
        head_and_tail(&stderr),
    );
    assert!(
        status.success(),
        "{inner_test_name} failed.\n--- stdout ---\n{}\n--- stderr ---\n{}",
        head_and_tail(&stdout),
        head_and_tail(&stderr),
    );
    let marker = format!("{SCENARIO_OK_PREFIX}{inner_test_name}");
    assert!(
        stdout.contains(&marker),
        "{inner_test_name} exited 0 but its stdout never reported running the scenario body to \
         completion (looked for {marker:?}) — a renamed `_inner` fn (leaving run_inner's literal \
         name stale) or a marker env that never reached the child both make the scenario a \
         silent no-op with a green suite (#1024 review M1).\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        head_and_tail(&stdout),
        head_and_tail(&stderr),
    );
}

/// True only inside a scenario's re-exec'd child — see the module doc.
fn in_scenario_child() -> bool {
    std::env::var_os(SCENARIO_MARKER).is_some()
}

/// The bare path (module path stripped of the crate-name segment, `::f`
/// suffix stripped) of the function this macro is invoked inside — e.g.
/// inside `a_session_handover_survives_a_predecessor_parked_in_read`, this
/// expands to that exact string.
///
/// #1024 review New-5: each scenario's `_inner` test name used to be written
/// three times by hand (the `_inner` fn itself, the literal passed to
/// [`run_inner`], and the literal in the `_inner`'s own [`SCENARIO_OK_PREFIX`]
/// print) with nothing enforcing any of them agree with the *outer* test's
/// own identity. Mutation `swapname` — pointing scenario A's outer test at
/// scenario C's `_inner` name — passed green, because [`run_inner`]'s M1
/// check only ever proved the string it was given was internally
/// self-consistent with what the resulting child printed, never that the
/// caller had actually named *itself*. Deriving the name from the calling
/// function's own identity removes the literal (and the copy-paste) instead
/// of just deduplicating it.
macro_rules! this_fn_name {
    () => {{
        fn f() {}
        fn type_name_of<T>(_: T) -> &'static str {
            std::any::type_name::<T>()
        }
        let full = type_name_of(f);
        let full = &full[..full.len() - "::f".len()];
        // Every `#[tokio::test] async fn` body desugars to a closure, so an
        // item defined inside one (like `f` above) picks up a
        // "::{{closure}}" path segment for each level of that desugaring —
        // strip from the first one onward to get back to the enclosing
        // test fn's own name (confirmed by running this: without the strip,
        // the derived name was
        // "…the_next_sessions_seed::{{closure}}", not the bare fn name).
        let full = full.split("::{{closure}}").next().unwrap_or(full);
        // `type_name` is crate-qualified ("<crate>::<path>"); libtest's own
        // test names are not, so strip the leading crate-name segment.
        full.split_once("::").map_or(full, |(_, rest)| rest)
    }};
}

/// Assert (#1024 review New-3) that a bounded seed genuinely landed *inside*
/// `bound` by wall clock, not just that `tokio::time::timeout` didn't return
/// `Err`. `#[tokio::test]` builds a **current-thread** runtime by default,
/// and `serve`'s `GrantStore::load` parses `grants.toml` synchronously (no
/// `.await` inside it) — while that runs, the executor cannot poll any other
/// future on this thread, timer included, so a seed that arrives late can
/// still see the timeout wrapper return `Ok` (measured on the unmutated
/// tree: 7.310 s elapsed through a `timeout(GIVE_UP = 5s, ..)` that returned
/// `Ok`, no panic). [`spawn_serve`] fixes the root cause (moving `serve`'s
/// execution off this thread entirely, so the timer keeps running); this
/// assertion is the belt-and-suspenders check that a wedge is still caught
/// here, precisely and fast, rather than only by `run_inner`'s coarse
/// [`CHILD_GIVE_UP`] kill.
fn assert_seeded_within(started: Instant, bound: Duration, what: &str) {
    let elapsed = started.elapsed();
    assert!(
        elapsed < bound,
        "{what} took {elapsed:?}, at or past its {bound:?} bound on the happy path — a wedge \
         should be caught here, not deferred to run_inner's coarse {CHILD_GIVE_UP:?} kill \
         (#1024 review New-3)",
    );
}

/// Run [`serve`] on tokio's blocking thread pool via `Handle::block_on`,
/// rather than a plain `tokio::spawn` onto this test's own async worker
/// thread.
///
/// #1024 review New-3: `#[tokio::test]` builds a **current-thread** runtime,
/// so a plain `tokio::spawn(serve(..))` runs `serve` — including its
/// synchronous `GrantStore::load` TOML parse, which has no `.await` inside it
/// — on the SAME single OS thread this test's own future (every
/// `tokio::time::timeout` in this file included) is polled on. While that
/// parse runs, the thread can't poll anything else, so a `timeout` wrapping a
/// seed that arrives late can still observe `Ok` — measured on the unmutated
/// tree: 7.310 s elapsed through a `timeout(GIVE_UP = 5s, ..)` that returned
/// `Ok`, no panic, because `Timeout` polls its inner future first and by the
/// time the executor got back to the timer the inner future had already
/// resolved.
///
/// Enabling tokio's `rt-multi-thread` feature (`#[tokio::test(flavor =
/// "multi_thread", ..)]`) would fix that, but is out of #1024's lane — this
/// crate's `Cargo.toml` isn't in it — and isn't reachable from this crate's
/// own dependency graph anyway: confirmed by trying it, `cargo test -p
/// hytte-plugin-infobroker` alone then fails to compile ("the runtime flavor
/// `multi_thread` requires the `rt-multi-thread` feature"), even though it
/// would happen to compile under a full `cargo test --workspace` because
/// unrelated sibling crates (`hytte-bus`, `hytte-services`, …) request that
/// feature and Cargo unifies it in for that wider build.
///
/// `spawn_blocking` needs only the already-enabled `rt` feature and moves
/// `serve`'s entire execution — async and blocking parts alike — onto
/// tokio's separate blocking-pool thread, leaving this test's own single
/// worker thread free to keep polling its timers. `Handle::block_on` from
/// inside a `spawn_blocking` closure is the bridge tokio's own docs recommend
/// for running async code from a blocking context (and is *not* the
/// "`block_on` inside an async task" pattern that panics — the
/// blocking-pool closure is plain sync code, not itself a polled future).
fn spawn_serve(
    cmds: mpsc::UnboundedReceiver<Cmd>,
    out: mpsc::UnboundedSender<BrokerMsg>,
) -> tokio::task::JoinHandle<()> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || handle.block_on(serve(cmds, out)))
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
    // #1024 review New-6: guard against ever running the outer half inside a
    // re-exec'd child too (structural, not just "the re-exec always passes
    // --exact" — see `run_inner`'s New-6 assertion for that half).
    if in_scenario_child() {
        return;
    }
    let runtime_dir = tempfile::tempdir().expect("XDG_RUNTIME_DIR scratch dir");
    let state_dir = tempfile::tempdir().expect("XDG_STATE_HOME scratch dir");
    // #1024 review New-5: derived from this function's own name, not a
    // hand-typed literal that could silently name a different scenario.
    let inner_name = format!("{}_inner", this_fn_name!());
    run_inner(&inner_name, runtime_dir.path(), state_dir.path());
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
    let session1 = spawn_serve(cmds1_rx, out1_tx);

    let started1 = Instant::now();
    let snap1 = tokio::time::timeout(GIVE_UP, recv_update(&mut out1_rx))
        .await
        .expect("session 1 must seed within a bounded window, not hang the harness — #1024 L4")
        .expect("session 1's lane produced a snapshot");
    assert_seeded_within(started1, GIVE_UP, "session 1's seed");
    assert_eq!(
        snap1.notice, None,
        "session 1 must bind cleanly: {:?}",
        snap1.notice,
    );

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
    let session2 = spawn_serve(cmds2_rx, out2_tx);

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
    // #1024 review L2: this probe connection is never accepted by session 1
    // (it is busy with `parked_client`) and must not linger — left alive, it
    // sits in the kernel accept backlog until *session 2* accepts it first,
    // parking session 2 in `handle_conn` for the full `REQUEST_TIMEOUT` (5 s)
    // before `fresh_client` below can get an answer. Dropping it here (rather
    // than at the end of the test) is what keeps this test's wall time down
    // to session 1's own `PARK_SETTLE`/`BACKOFF_BASE`/`PROBE_WINDOW` sleeps.
    drop(still_accepting);

    // Unblock session 1's parked client so session 1 observes its closed lane
    // and returns, releasing the process-wide SOCKET guard for session 2.
    send_line(&mut parked_client, r#"{"op":"grants"}"#).await;
    let _ = read_line(&mut parked_client).await;

    let started2 = Instant::now();
    let snap2 = tokio::time::timeout(GIVE_UP, recv_update(&mut out2_rx))
        .await
        .expect("session 2 must seed within the bounded handover window — RED under mutation (a)")
        .expect("session 2's lane produced a snapshot");
    assert_seeded_within(started2, GIVE_UP, "session 2's seed");
    assert_eq!(
        snap2.notice, None,
        "session 2 must come up Kept, not stand down against its own predecessor: {:?}",
        snap2.notice,
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

    // #1024 review M1: printed only once every assertion above has passed —
    // `run_inner` requires this exact line in the parent process's view of
    // the child's stdout, so a no-op child (renamed `_inner` fn, or a marker
    // that never reached it) cannot pass silently.
    // #1024 review New-5: derived from this function's own name, matching
    // the outer test's derivation of the same name — see `this_fn_name!`.
    println!("{SCENARIO_OK_PREFIX}{}", this_fn_name!());
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
///
/// RED under mutation D (#1024 review M2) — `std::fs::remove_file(&sock)`
/// added to the `StoodDown` arm right after the notice is set: the rendered
/// notice alone doesn't notice this, because nothing in the `_inner` test
/// touches the socket path at all. This is #995's actual damage (the
/// infobroker unlinking a *live* incumbent's socket), just moved one
/// statement later than #1004's fix guards, so this test asserts on the
/// socket itself, in the parent, after the child returns: same inode, and
/// still accepting a real connection from `foreign` (held open the whole
/// time, never dropped until this check is done).
#[tokio::test]
async fn a_foreign_listener_gets_stood_down_with_the_notice_rendered() {
    // #1024 review New-6: see the sibling scenario's identical guard.
    if in_scenario_child() {
        return;
    }
    let runtime_dir = tempfile::tempdir().expect("XDG_RUNTIME_DIR scratch dir");
    let state_dir = tempfile::tempdir().expect("XDG_STATE_HOME scratch dir");
    let sock_path = runtime_dir.path().join(SOCKET_FILE);
    let foreign = tokio::net::UnixListener::bind(&sock_path).expect("bind the foreign listener");
    let ino_before = std::fs::metadata(&sock_path)
        .expect("stat the freshly bound foreign socket")
        .ino();

    // #1024 review New-5: derived from this function's own name — see
    // `this_fn_name!`'s doc.
    let inner_name = format!("{}_inner", this_fn_name!());
    run_inner(&inner_name, runtime_dir.path(), state_dir.path());

    // #1024 review M2: the notice (checked in `_inner`) only proves the panel
    // was told — it says nothing about whether the stood-down session left
    // the incumbent's socket alone. First: still the same inode at the same
    // path, not unlinked (mutation D fails here with an ENOENT `expect`, or a
    // changed inode if something rebinds).
    let ino_after = std::fs::metadata(&sock_path)
        .expect("the foreign socket must still exist at its original path after stand-down")
        .ino();
    assert_eq!(
        ino_before,
        ino_after,
        "the socket at {} must be the SAME inode after stand-down, not unlinked/re-created \
         (mutation D: remove_file in the StoodDown arm)",
        sock_path.display(),
    );

    // `bind_socket`'s OWN liveness probe (`socket_in_use`, src/broker.rs) is
    // exactly one connect-then-immediately-drop against this listener — the
    // stood-down child made it to decide to stand down in the first place
    // (see that function's doc comment: "the incumbent's accept loop reads
    // EOF and reaps it on the next poll"). A *real* incumbent's own `serve()`
    // accept loop would already have reaped that connection; `foreign` here
    // never runs one, so it is still sitting unaccepted in the kernel
    // backlog. Drain it (it reads EOF immediately, having never written
    // anything) before the genuine round trip below, so `foreign.accept()`
    // there pairs with the fresh client and not this stale probe.
    while let Ok(Ok((mut stale, _))) =
        tokio::time::timeout(Duration::from_millis(200), foreign.accept()).await
    {
        // #1024 review New-4: the same 200 ms bound as `accept()` above, on
        // the `read_line` too — this is parent-side, post-child code, and
        // today it can't actually hang (`run_inner` has already reaped the
        // child, so every backlog connection is a plain EOF), but it is the
        // one un-bounded I/O call left in a file that just added bounds
        // everywhere else, guarding against a future regression rather than
        // a live one.
        let drained = tokio::time::timeout(Duration::from_millis(200), read_line(&mut stale))
            .await
            .expect(
                "draining a backlog connection must not hang — a stale probe that sends nothing \
                 should EOF immediately (#1024 review New-4)",
            );
        assert!(
            drained.is_none(),
            "drained a backlog connection that sent data — expected only #995's silent, \
             write-nothing liveness probe",
        );
    }

    // Second: not just present, but genuinely still live — `foreign` (held
    // open this whole time, dropped only below) accepts a fresh connection
    // and a real byte flows through it.
    let round_trip = async {
        let (client_result, accept_result) =
            tokio::join!(UnixStream::connect(&sock_path), foreign.accept());
        let mut client = client_result.expect("connect to the still-live incumbent");
        let (mut accepted, _) = accept_result.expect("the incumbent listener must accept it");
        send_line(&mut client, "ping").await;
        read_line(&mut accepted).await
    };
    let line = tokio::time::timeout(Duration::from_secs(2), round_trip)
        .await
        .expect("connect + accept must complete quickly if the incumbent survived stand-down")
        .expect("the incumbent must receive what the client sends");
    assert_eq!(
        line.trim(),
        "ping",
        "the incumbent must actually receive what a fresh client sends: {line:?}",
    );

    // Kept alive across the whole child run and the checks above — dropped
    // only now.
    drop(foreign);
}

#[tokio::test]
async fn a_foreign_listener_gets_stood_down_with_the_notice_rendered_inner() {
    if !in_scenario_child() {
        return;
    }
    let (cmds_tx, cmds_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<BrokerMsg>();
    let session = spawn_serve(cmds_rx, out_tx);

    let started = Instant::now();
    let snap = tokio::time::timeout(GIVE_UP, recv_update(&mut out_rx))
        .await
        .expect("a stood-down session must still seed a panel snapshot")
        .expect("the lane produced a snapshot");
    assert_seeded_within(started, GIVE_UP, "the stood-down session's seed");

    let notice = snap.notice.expect(
        "a foreign live listener must produce a stand-down notice — RED under mutation (b)",
    );
    assert!(
        notice.contains("another info broker"),
        "the notice must explain the stand-down: {notice}",
    );

    drop(cmds_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), session).await;

    // #1024 review M1 — see the sibling scenario for why this line matters.
    // #1024 review New-5 — derived from this function's own name, matching
    // the outer test's derivation of the same name.
    println!("{SCENARIO_OK_PREFIX}{}", this_fn_name!());
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
    // #1024 review New-6: see the sibling scenario's identical guard.
    if in_scenario_child() {
        return;
    }
    let runtime_dir = tempfile::tempdir().expect("XDG_RUNTIME_DIR scratch dir");
    let state_dir = tempfile::tempdir().expect("XDG_STATE_HOME scratch dir");
    seed_large_grants(state_dir.path());
    // #1024 review New-5: derived from this function's own name — see
    // `this_fn_name!`'s doc.
    let inner_name = format!("{}_inner", this_fn_name!());
    run_inner(&inner_name, runtime_dir.path(), state_dir.path());
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
    let session1 = spawn_serve(cmds1_rx, out1_tx);

    let started1 = Instant::now();
    let snap1 = tokio::time::timeout(GIVE_UP, recv_update(&mut out1_rx))
        .await
        .expect("session 1 must seed within a bounded window, not hang the harness — #1024 L4")
        .expect("session 1's lane produced a snapshot");
    assert_seeded_within(started1, GIVE_UP, "session 1's seed");
    assert_eq!(
        snap1.notice, None,
        "session 1 must bind cleanly: {:?}",
        snap1.notice,
    );
    // #1064 review F1: nothing else pins `load_grants` to actually reading
    // `grants.toml` — a loader that stops reading it (and returns an empty
    // store) stayed green here before this assertion existed. Check both the
    // count (the file really was read) and a known row (the bytes that came
    // back are the ones `seed_large_grants` wrote, not just the right shape).
    assert_eq!(
        snap1.grants.len(),
        HUGE_GRANT_COUNT,
        "session 1 must actually honour the seeded grants.toml — RED under a \
         `load_grants` that stops reading the file",
    );
    assert!(
        snap1.grants.iter().any(|g| g.agent == "agent-000000"
            && g.datasource == "departures"
            && g.decision == "always"),
        "the seeded grants must be the ones grants.toml actually holds, not just \
         the right count: first = {:?}",
        snap1.grants.first(),
    );

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
    let session2 = spawn_serve(cmds2_rx, out2_tx);

    let started2 = Instant::now();
    let snap2 = tokio::time::timeout(WRITE_PARK_GIVE_UP, recv_update(&mut out2_rx))
        .await
        .expect(
            "session 2 must seed within write_response's bound — RED if that timeout is removed",
        )
        .expect("session 2's lane produced a snapshot");
    assert_seeded_within(started2, WRITE_PARK_GIVE_UP, "session 2's seed");
    assert_eq!(
        snap2.notice, None,
        "session 2 must come up Kept: {:?}",
        snap2.notice,
    );

    drop(cmds2_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), session1).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), session2).await;

    // Held open (never read) for the whole test on purpose — dropped only
    // now that both sessions are done with it.
    drop(stuck_client);

    // #1024 review M1 — see the first scenario for why this line matters.
    // #1024 review New-5 — derived from this function's own name, matching
    // the outer test's derivation of the same name.
    println!("{SCENARIO_OK_PREFIX}{}", this_fn_name!());
}

// ── Scenario D: a slow synchronous step must not stop the runtime's timers ──

/// #1059 item 1: production runs `serve` under a plain `tokio::spawn` on the
/// SDK's **current-thread** runtime (`src/plugin.rs`'s `sources()` →
/// `hytte-plugin/src/runtime.rs`), so any synchronous step inside `serve`
/// holds the only thread there is and stalls every timer on it — the broker's
/// own request/consent/query bounds, the SDK's clock pump, and the SDK session
/// loop, together. Measured on #1024's tree, with `GrantStore::load` parsing a
/// large `grants.toml` inline: a 5 s `tokio::time::timeout` returning `Ok` at
/// 7.310 s.
///
/// So this scenario drives `serve` exactly the way production does — a plain
/// `tokio::spawn`, deliberately **not** [`spawn_serve`], which exists precisely
/// to keep the other three scenarios off this thread — with a grant loader
/// injected through `broker::serve_with_grant_loader` that blocks
/// synchronously for [`SLOW_LOAD`], and asserts a concurrent [`STARVED_TIMER`]
/// on the same runtime still fires on time.
///
/// RED under mutation (e) — `spawn_blocking(load_grants).await` in
/// `serve_with_grant_loader` replaced by a plain `load_grants()` call: the
/// timer then cannot be polled until the loader returns 2 s later, failing
/// both the ordering assertion and the wall-clock bound.
///
/// It needs the re-exec harness for the same reason its siblings do: `serve`
/// bails before it ever reaches the grant load when `XDG_RUNTIME_DIR` is
/// unset, and `std::env::set_var` is `unsafe` (see the module doc).
#[tokio::test]
async fn a_slow_grant_load_does_not_stall_the_sessions_timers() {
    // #1024 review New-6: see the sibling scenarios' identical guard.
    if in_scenario_child() {
        return;
    }
    let runtime_dir = tempfile::tempdir().expect("XDG_RUNTIME_DIR scratch dir");
    let state_dir = tempfile::tempdir().expect("XDG_STATE_HOME scratch dir");
    // #1024 review New-5: derived from this function's own name — see
    // `this_fn_name!`'s doc.
    let inner_name = format!("{}_inner", this_fn_name!());
    run_inner(&inner_name, runtime_dir.path(), state_dir.path());
}

#[tokio::test]
async fn a_slow_grant_load_does_not_stall_the_sessions_timers_inner() {
    if !in_scenario_child() {
        return;
    }
    let sock_path =
        hytte_plugin_infobroker::paths::socket_path().expect("XDG_RUNTIME_DIR set by the harness");

    // Two latches rather than one: `entered` proves `serve` actually reached
    // the grant load (without it, a `serve` that returned early would make the
    // punctuality assertions vacuously true), `left` is the ordering half of
    // the property — the timer must fire while the loader is still running.
    let entered = Arc::new(AtomicBool::new(false));
    let left = Arc::new(AtomicBool::new(false));

    let (cmds_tx, cmds_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<BrokerMsg>();
    let loader_entered = Arc::clone(&entered);
    let loader_left = Arc::clone(&left);
    // A PLAIN `tokio::spawn` on this test's current-thread runtime — the
    // production shape. Using `spawn_serve` here would move `serve` to the
    // blocking pool and make the whole scenario pass unconditionally.
    let session = tokio::spawn(serve_with_grant_loader(cmds_rx, out_tx, move || {
        loader_entered.store(true, Ordering::SeqCst);
        std::thread::sleep(SLOW_LOAD);
        loader_left.store(true, Ordering::SeqCst);
        GrantStore::from_grants(Vec::new())
    }));

    let started = Instant::now();
    let fired = tokio::time::timeout(STARVED_TIMER, std::future::pending::<()>()).await;
    let delay = started.elapsed();
    // Printed, not just asserted: `--nocapture` is already on for every
    // scenario child, and the whole point of the bounds below is a measured
    // number, so a future retune of `STARVED_TIMER_SLACK` starts from a fresh
    // observation rather than this comment.
    println!(
        "MEASURED a {STARVED_TIMER:?} timer fired after {delay:?} against a {SLOW_LOAD:?} load"
    );
    assert!(
        fired.is_err(),
        "a timeout over `pending` must elapse, never resolve — that would be the harness \
         breaking, not the code under test",
    );

    assert!(
        entered.load(Ordering::SeqCst),
        "the injected loader had not started {delay:?} in, so this test cannot tell a punctual \
         timer from a `serve` that never reached the grant load at all",
    );
    assert!(
        !left.load(Ordering::SeqCst),
        "the {STARVED_TIMER:?} timer fired only after the {SLOW_LOAD:?} loader had already \
         finished ({delay:?} elapsed) — the two never overlapped, i.e. `serve` ran the loader \
         on this very thread (RED under mutation (e): `spawn_blocking` removed)",
    );
    assert!(
        delay < STARVED_TIMER + STARVED_TIMER_SLACK,
        "a {STARVED_TIMER:?} timer took {delay:?} while `serve`'s {SLOW_LOAD:?} synchronous \
         grant load ran on the same current-thread runtime — the load is starving the session's \
         own timers (#1059)",
    );

    // The offload must not have cost the session anything: it still seeds the
    // panel once the loader returns, and it really is serving.
    let seed = tokio::time::timeout(SLOW_LOAD + GIVE_UP, recv_update(&mut out_rx))
        .await
        .expect("the session must still seed once the slow load finishes")
        .expect("the lane produced a snapshot");
    assert!(
        left.load(Ordering::SeqCst),
        "the seed must follow the loader, not race ahead of it — `serve` must still be using \
         the store the loader handed back",
    );
    assert_eq!(
        seed.notice, None,
        "the session must bind cleanly: {:?}",
        seed.notice,
    );

    let mut client = UnixStream::connect(&sock_path)
        .await
        .expect("connect to the session");
    send_line(&mut client, r#"{"op":"grants"}"#).await;
    let reply = read_line(&mut client)
        .await
        .expect("the session answers a real request");
    assert!(
        reply.contains("\"ok\":true"),
        "the offloaded load must leave a fully working broker behind: {reply}",
    );

    drop(cmds_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), session).await;

    // #1024 review M1 / New-5 — see the first scenario for why this line
    // matters and why the name is derived rather than typed.
    println!("{SCENARIO_OK_PREFIX}{}", this_fn_name!());
}

// ── Scenario E: the shutdown hook drains a queued grant save (#1092 review M2) ──

/// PR #1092's review (M2): `broker::serve_inner`'s shutdown arm —
/// `Some(ack) = recv_shutdown(&mut shutdown) => { state.grants.drain().await;
/// let _ = ack.send(()); break; }` — had no test at all; the reviewer
/// measured that guarding the arm off (`, if std::hint::black_box(false)`)
/// left the whole crate's unit suite green. The PR's own Scope note argued a
/// real drive needed a subprocess harness "out of scope here" — this file
/// already is that harness (#1024/#1059's scenarios above), so scenario E
/// just adds to it.
///
/// Drives [`serve_with_shutdown`] the production way (a plain `tokio::spawn`
/// on this test's current-thread runtime, matching `src/plugin.rs`'s
/// `sources()`): queues one real `Cmd::Allow` over the SDK-free public API,
/// waits for the panel snapshot that proves it landed in the broker's
/// in-memory state, then requests the drain over the shutdown oneshot and
/// awaits its ack. The ack is the synchronization point —
/// `GrantStore::drain` (`src/grants.rs`) `.await`s the writer task's own
/// `spawn_blocking` before `serve_inner` ever calls `ack.send(())` — so a
/// resolved `ack_rx` already proves the write finished; reloading
/// `grants.toml` from disk afterward confirms it is the *right* write, not
/// merely that some file now exists.
///
/// RED under the review's mutation (the arm guarded off): nothing ever
/// services `shutdown_rx`, so `ack_rx` never resolves — the bounded
/// `tokio::time::timeout` below turns that into a named failure rather than
/// a hang.
#[tokio::test]
async fn shutdown_drains_a_queued_grant_save() {
    // #1024 review New-6: see the sibling scenarios' identical guard.
    if in_scenario_child() {
        return;
    }
    let runtime_dir = tempfile::tempdir().expect("XDG_RUNTIME_DIR scratch dir");
    let state_dir = tempfile::tempdir().expect("XDG_STATE_HOME scratch dir");
    // #1024 review New-5: derived from this function's own name — see
    // `this_fn_name!`'s doc.
    let inner_name = format!("{}_inner", this_fn_name!());
    run_inner(&inner_name, runtime_dir.path(), state_dir.path());
}

#[tokio::test]
async fn shutdown_drains_a_queued_grant_save_inner() {
    if !in_scenario_child() {
        return;
    }
    let state_dir = std::env::var_os("XDG_STATE_HOME").expect("the harness sets XDG_STATE_HOME");
    let grants_path = Path::new(&state_dir).join(STATE_DIR).join(GRANTS_FILE);

    let (cmds_tx, cmds_rx) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<BrokerMsg>();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    // Production shape: a plain `tokio::spawn` on this current-thread
    // runtime, exactly like `src/plugin.rs`'s `sources()` spawns `serve`.
    let session = tokio::spawn(serve_with_shutdown(cmds_rx, out_tx, shutdown_rx));

    let seed = tokio::time::timeout(GIVE_UP, recv_update(&mut out_rx))
        .await
        .expect("the session must seed promptly")
        .expect("the lane produced a snapshot");
    assert_eq!(
        seed.notice, None,
        "the session must bind cleanly: {:?}",
        seed.notice
    );
    assert!(seed.grants.is_empty(), "starts with no grants");

    cmds_tx
        .send(Cmd::Allow {
            agent: "claude".to_owned(),
            datasource: "departures".to_owned(),
        })
        .expect("the broker task is still alive");

    let after_allow = tokio::time::timeout(GIVE_UP, recv_update(&mut out_rx))
        .await
        .expect("the Allow must produce a fresh snapshot")
        .expect("the lane produced a snapshot");
    assert_eq!(
        after_allow.grants,
        vec![GrantView {
            agent: "claude".to_owned(),
            datasource: "departures".to_owned(),
            decision: "always",
        }],
        "the Allow must be applied (and queued for a write) before the shutdown request is sent",
    );

    let (ack_tx, ack_rx) = oneshot::channel();
    shutdown_tx
        .send(ack_tx)
        .expect("the broker task is still alive to receive the shutdown request");
    tokio::time::timeout(GIVE_UP, ack_rx)
        .await
        .expect(
            "the shutdown ack must arrive within the bound — a guarded-off arm (#1092 review \
             M2) never answers at all, which this bound turns into a named failure instead of \
             a hang",
        )
        .expect("the broker task must not drop the ack sender without answering");

    // The ack having arrived is the proof `drain` already finished (see the
    // doc above) — no settle sleep, no poll.
    let on_disk = GrantStore::load(&grants_path).expect("drain must leave a well-formed file");
    assert_eq!(
        on_disk.grants(),
        &[Grant::always("claude", "departures")],
        "the queued Allow must have landed on disk before the ack fired",
    );

    let _ = tokio::time::timeout(Duration::from_secs(2), session).await;

    // #1024 review M1 / New-5 — see the first scenario for why this line
    // matters and why the name is derived rather than typed.
    println!("{SCENARIO_OK_PREFIX}{}", this_fn_name!());
}
