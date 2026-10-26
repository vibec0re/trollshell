//! `hytte-infobroker` — the broker's command-line client (issue #487, phase 1a).
//!
//! The only client of the broker socket, and the binary the skill folder's
//! agents shell out to. Three subcommands over the boring JSON-lines wire
//! ([`hytte_plugin_infobroker::wire`]):
//!
//! ```text
//! hytte-infobroker auth --agent <name> [--datasource <ds>]  # → `export HYTTE_INFOBROKER_TOKEN=…`
//! hytte-infobroker get <datasource> [--limit N] # uses $HYTTE_INFOBROKER_TOKEN → JSON
//! hytte-infobroker grants list                  # the durable grants (introspection)
//! ```
//!
//! `<datasource>` is `departures` / `weather` / `calendar` (#509/#484). `auth`
//! asks for one of them (`departures` when `--datasource` is left out), and the
//! human's consent prompt names it: a token minted by an "Allow once" / "This
//! session" answer reads that datasource and no other, so an agent that needs
//! two asks twice.
//!
//! The auth line is meant to be `eval`'d:
//! `eval "$(hytte-infobroker auth --agent claude)"`. Blocking std sockets only —
//! no async runtime, so the CLI stays a fast, tiny binary.
//!
//! Argument parsing is `clap` (#1116, following @kaesaecracker's recommendation
//! on that thread) rather than hand-rolled, so nix can generate shell
//! completions from the same `Command` at build time — see the hidden
//! `completions <shell>` subcommand below, which `nix/plugin.nix`'s
//! `installShellCompletion` call invokes.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use clap::{CommandFactory as _, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};

use hytte_plugin_infobroker::broker::CONSENT_PARK_TIMEOUT;
use hytte_plugin_infobroker::paths;
use hytte_plugin_infobroker::wire::{
    DATASOURCE_CALENDAR, DATASOURCE_DEPARTURES, DATASOURCE_WEATHER, Request, Response,
};

/// The environment variable carrying the session token, injected by `auth` and
/// read by `get`.
const ENV_TOKEN: &str = "HYTTE_INFOBROKER_TOKEN";

/// How long [`call`] waits for the broker's one reply line before giving up.
///
/// The longest a working broker legitimately takes is an `auth` parked on the
/// human: the shell's card answers within 60 s, and the broker's own backstop
/// ([`CONSENT_PARK_TIMEOUT`], 65 s) answers a card that never does. This is
/// that backstop plus 15 s of slack, so a slow-but-alive broker always answers
/// first. Without any bound (#1489 follow-up audit, L2) a wedged broker — one
/// that accepted the connection and then never wrote — hung the CLI, and with
/// it the agent that shelled out to it, forever.
const REPLY_TIMEOUT: Duration = CONSENT_PARK_TIMEOUT.saturating_add(Duration::from_secs(15));

const _: () = assert!(REPLY_TIMEOUT.as_secs() > CONSENT_PARK_TIMEOUT.as_secs());

/// The agent-flow example shown in `--help`.
const LONG_ABOUT: &str = "hytte-infobroker — the trollshell data broker CLI (issue #487)\n\
\n\
Typical agent flow:\n\
    eval \"$(hytte-infobroker auth --agent claude --datasource departures)\"\n\
    hytte-infobroker get departures --limit 5";

#[derive(Parser, Debug)]
#[command(name = "hytte-infobroker", version, long_about = LONG_ABOUT)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Mint a session token (prints an `export HYTTE_INFOBROKER_TOKEN=…` line
    /// meant to be `eval`'d)
    Auth {
        /// The agent name the minted token identifies as
        #[arg(long)]
        agent: String,
        /// The datasource to ask for — what the consent prompt names, and all
        /// a once/session token it mints can read [default: departures]
        #[arg(long)]
        datasource: Option<Datasource>,
    },
    /// Fetch scoped data (needs the env token from a prior `auth`)
    Get {
        /// Which datasource to fetch
        datasource: Datasource,
        /// Cap the number of rows (ignored by `weather`, a single reading)
        #[arg(long)]
        limit: Option<usize>,
    },
    /// The durable grants (introspection)
    Grants {
        #[command(subcommand)]
        action: GrantsAction,
    },
    /// Print a shell completion script (invoked by nix's
    /// `installShellCompletion`, not meant for a human to type)
    #[command(hide = true)]
    Completions {
        /// Which shell's script to print
        shell: Shell,
    },
}

#[derive(Subcommand, Debug)]
enum GrantsAction {
    /// List the durable grants
    List,
}

/// `get`'s datasource argument — the three the broker serves. Variant names
/// map to `wire`'s `DATASOURCE_*` constants via [`Datasource::as_wire`],
/// spelled out explicitly rather than leaned on clap's kebab-case renderer
/// agreeing by coincidence, the same "written out, not derived" call the
/// golden-layout proportions make elsewhere in this workspace.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Datasource {
    Departures,
    Weather,
    Calendar,
}

impl Datasource {
    fn as_wire(self) -> &'static str {
        match self {
            Self::Departures => DATASOURCE_DEPARTURES,
            Self::Weather => DATASOURCE_WEATHER,
            Self::Calendar => DATASOURCE_CALENDAR,
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(e) => return exit_for_parse_error(&e),
    };
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hytte-infobroker: {e}");
            ExitCode::FAILURE
        }
    }
}

/// clap's own exit code for a usage error is 2; this CLI's contract predates
/// clap and treats any misuse the same as any other broker error — exit 1 —
/// so map it down rather than let switching parsers move the number a script
/// might check. `--help`/`--version` (clap's "display" error kinds) keep
/// their 0.
fn exit_for_parse_error(e: &clap::Error) -> ExitCode {
    let _ = e.print();
    if e.exit_code() == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        // No subcommand: print usage and succeed, same as the pre-clap
        // `None | Some("--help" | "-h" | "help")` arm.
        None => {
            Cli::command()
                .print_help()
                .map_err(|e| format!("printing help: {e}"))?;
            println!();
            Ok(())
        }
        Some(Command::Auth { agent, datasource }) => cmd_auth(&agent, datasource),
        Some(Command::Get { datasource, limit }) => cmd_get(datasource, limit),
        Some(Command::Grants {
            action: GrantsAction::List,
        }) => cmd_grants(),
        Some(Command::Completions { shell }) => {
            print!("{}", render_completions(shell));
            Ok(())
        }
    }
}

/// `auth --agent <name> [--datasource <ds>]` → an eval-able export line on
/// stdout (a human note on stderr), or the denial (with its how-to-grant hint)
/// as an error. An omitted `--datasource` leaves the field off the wire, so the
/// broker's own default (departures) applies — one definition, not two.
fn cmd_auth(agent: &str, datasource: Option<Datasource>) -> Result<(), String> {
    let resp = call(&Request::Auth {
        agent: agent.to_owned(),
        datasource: datasource.map(|d| d.as_wire().to_owned()),
    })?;
    if !resp.ok {
        return Err(deny_message(&resp));
    }
    let token = resp
        .token
        .ok_or("auth: broker returned ok without a token")?;
    // stdout: the one line meant for `eval` (nothing else, so eval stays clean).
    println!("export {ENV_TOKEN}={token}");
    // stderr: the human-facing note.
    if let Some(exp) = resp.expires_unix {
        let asked = datasource.map_or(DATASOURCE_DEPARTURES, Datasource::as_wire);
        eprintln!(
            "hytte-infobroker: session token for '{agent}' ({asked}) minted (expires at unix \
             {exp}); eval the line above to use it."
        );
    }
    Ok(())
}

/// `get <datasource> [--limit N]` → the scoped data as pretty JSON on stdout,
/// using the env token. The datasource must be one the broker serves —
/// `departures` / `weather` (each answered by its provider plugin over the host's
/// query protocol) or `calendar` (the host-fed live copy). `--limit` caps the row
/// datasources (`departures` / `calendar`); `weather` is a single reading and
/// ignores it.
fn cmd_get(datasource: Datasource, limit: Option<usize>) -> Result<(), String> {
    let token = std::env::var(ENV_TOKEN).map_err(|_| {
        format!(
            "get: {ENV_TOKEN} not set — run `eval \"$(hytte-infobroker auth --agent <name>)\"` first"
        )
    })?;
    let resp = call(&Request::Get {
        token,
        datasource: datasource.as_wire().to_owned(),
        limit,
    })?;
    if !resp.ok {
        return Err(deny_message(&resp));
    }
    // Each `get` populates exactly one payload field (the broker shapes it per
    // datasource); print that one as pretty JSON. A row datasource with no rows
    // prints as `[]`; weather is a single object.
    let json = match datasource {
        Datasource::Weather => {
            let reading = resp
                .weather
                .ok_or("get: broker returned ok without a weather reading")?;
            serde_json::to_string_pretty(&reading)
        }
        Datasource::Calendar => serde_json::to_string_pretty(&resp.calendar.unwrap_or_default()),
        Datasource::Departures => {
            serde_json::to_string_pretty(&resp.departures.unwrap_or_default())
        }
    }
    .map_err(|e| format!("encoding output: {e}"))?;
    println!("{json}");
    Ok(())
}

/// `grants list` → one grant per line (agent, datasource, scope, decision).
/// A `hint` on the reply — `grants.toml` could not be read, so this is the
/// broker's in-memory set rather than the file's (#1489 follow-up audit, M3) —
/// goes to stderr first, so an empty list cannot read as "no grants".
fn cmd_grants() -> Result<(), String> {
    let resp = call(&Request::Grants)?;
    if !resp.ok {
        return Err(deny_message(&resp));
    }
    if let Some(hint) = &resp.hint {
        eprintln!("hytte-infobroker: warning: {hint}");
    }
    let grants = resp.grants.unwrap_or_default();
    if grants.is_empty() {
        println!("(no grants — edit grants.toml or use the infobroker panel's Allow)");
        return Ok(());
    }
    for g in &grants {
        println!("{}\t{}\t{}\t{}", g.agent, g.datasource, g.scope, g.decision);
    }
    Ok(())
}

/// Dial the broker socket, send one request line, read one response line.
fn call(req: &Request) -> Result<Response, String> {
    let path =
        paths::socket_path().ok_or("XDG_RUNTIME_DIR not set — is this a desktop session?")?;
    call_at(&path, req, REPLY_TIMEOUT)
}

/// [`call`] against an explicit socket and reply bound — the seam a test
/// drives with a scratch socket and a short `reply_timeout`.
fn call_at(path: &Path, req: &Request, reply_timeout: Duration) -> Result<Response, String> {
    let stream = UnixStream::connect(path).map_err(|e| {
        format!(
            "cannot reach the broker at {} ({e}) — is the infobroker plugin running?",
            path.display()
        )
    })?;
    stream
        .set_read_timeout(Some(reply_timeout))
        .map_err(|e| format!("setting the reply timeout: {e}"))?;

    let mut line = serde_json::to_string(req).map_err(|e| format!("encoding request: {e}"))?;
    line.push('\n');
    (&stream)
        .write_all(line.as_bytes())
        .map_err(|e| format!("sending request: {e}"))?;

    let mut reader = BufReader::new(&stream);
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).map_err(|e| {
        if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            format!(
                "the broker at {} did not answer within {} s — it may be wedged; restarting the \
                 infobroker plugin should clear it",
                path.display(),
                reply_timeout.as_secs()
            )
        } else {
            format!("reading response: {e}")
        }
    })?;
    let resp_line = resp_line.trim();
    if resp_line.is_empty() {
        return Err("the broker closed the connection without a response".to_owned());
    }
    serde_json::from_str(resp_line).map_err(|e| format!("decoding response: {e}"))
}

/// Render a denied [`Response`] as a CLI error: the reason plus, when present,
/// the actionable how-to-grant hint on its own indented line.
fn deny_message(resp: &Response) -> String {
    let error = resp
        .error
        .clone()
        .unwrap_or_else(|| "request denied".to_owned());
    match &resp.hint {
        Some(hint) => format!("{error}\n  hint: {hint}"),
        None => error,
    }
}

/// Render one shell's completion script for this binary's `Command` tree.
/// `render_completions`/`print_completions` split lets a test inspect the
/// bytes without capturing stdout.
fn render_completions(shell: Shell) -> String {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_owned();
    let mut buf: Vec<u8> = Vec::new();
    generate(shell, &mut cmd, name, &mut buf);
    String::from_utf8(buf).expect("clap_complete's generated script is always valid UTF-8")
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory as _, Parser as _};
    use clap_complete::Shell;

    use super::{Cli, Command, GrantsAction, render_completions};
    use super::{Duration, Request, call_at};

    /// L2: a broker that accepts the connection and never writes fails the
    /// call at the reply bound, with an error saying so, instead of hanging
    /// the CLI (and the agent that shelled out to it) forever.
    ///
    /// Mutation: drop the `set_read_timeout`, and this test hangs — which
    /// is the bug.
    #[test]
    fn a_broker_that_never_answers_times_out_instead_of_hanging() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("broker.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        // Accept and hold the connection without writing a byte.
        let holder = std::thread::spawn(move || listener.accept().map(|(stream, _)| stream));

        let started = std::time::Instant::now();
        let err = call_at(&path, &Request::Grants, Duration::from_millis(200))
            .expect_err("a silent broker is an error");
        assert!(err.contains("did not answer within"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "gave up at the bound, not later: {:?}",
            started.elapsed()
        );
        drop(holder.join());
    }

    /// L2, the other side: the bound changes nothing for a broker that
    /// answers.
    #[test]
    fn a_broker_that_answers_is_read_as_before() {
        use std::io::{BufRead as _, Write as _};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("broker.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut request = String::new();
            std::io::BufReader::new(&stream)
                .read_line(&mut request)
                .expect("read the request");
            (&stream)
                .write_all(b"{\"ok\":true,\"grants\":[]}\n")
                .expect("reply");
            request
        });
        let resp = call_at(&path, &Request::Grants, Duration::from_secs(5)).expect("answered");
        assert!(resp.ok);
        assert_eq!(resp.grants, Some(Vec::new()));
        assert_eq!(server.join().expect("server").trim(), r#"{"op":"grants"}"#);
    }

    fn parse(argv: &[&str]) -> Result<Cli, clap::Error> {
        let mut full = vec!["hytte-infobroker"];
        full.extend_from_slice(argv);
        Cli::try_parse_from(full)
    }

    #[test]
    fn no_arguments_parses_with_no_subcommand() {
        let cli = parse(&[]).expect("bare invocation parses");
        assert!(cli.command.is_none());
    }

    #[test]
    fn auth_requires_the_agent_flag() {
        assert!(parse(&["auth"]).is_err());
    }

    #[test]
    fn auth_takes_the_agent_flag_spaced_or_equals() {
        let spaced = parse(&["auth", "--agent", "claude"]).expect("spaced form parses");
        assert!(matches!(
            spaced.command,
            Some(Command::Auth { agent, datasource: None }) if agent == "claude"
        ));

        let equals = parse(&["auth", "--agent=claude"]).expect("equals form parses");
        assert!(matches!(
            equals.command,
            Some(Command::Auth { agent, datasource: None }) if agent == "claude"
        ));
    }

    /// `--datasource` is optional (absent = the broker's departures default)
    /// and takes exactly the three datasources `get` does.
    #[test]
    fn auth_takes_an_optional_datasource_from_the_known_three() {
        for (token, want) in [
            ("departures", "departures"),
            ("weather", "weather"),
            ("calendar", "calendar"),
        ] {
            let cli = parse(&["auth", "--agent", "claude", "--datasource", token])
                .unwrap_or_else(|e| panic!("{token}: {e}"));
            let Some(Command::Auth {
                datasource: Some(ds),
                ..
            }) = cli.command
            else {
                panic!("{token}: expected an auth naming a datasource");
            };
            assert_eq!(ds.as_wire(), want);
        }
        assert!(
            parse(&["auth", "--agent", "claude", "--datasource", "moonphase"]).is_err(),
            "an unknown datasource is refused before it reaches the broker"
        );
    }

    #[test]
    fn get_parses_each_known_datasource() {
        for token in ["departures", "weather", "calendar"] {
            parse(&["get", token]).unwrap_or_else(|e| panic!("{token}: {e}"));
        }
    }

    #[test]
    fn get_rejects_an_unknown_datasource() {
        let err = parse(&["get", "moonphase"]).expect_err("not a known datasource");
        let msg = err.to_string();
        assert!(msg.contains("departures"), "lists the known ones: {msg}");
    }

    #[test]
    fn get_parses_the_limit_flag() {
        let cli = parse(&["get", "departures", "--limit", "5"]).expect("parses");
        assert!(matches!(
            cli.command,
            Some(Command::Get { limit: Some(5), .. })
        ));
    }

    #[test]
    fn get_limit_must_be_a_whole_number() {
        assert!(parse(&["get", "departures", "--limit", "nope"]).is_err());
    }

    #[test]
    fn grants_list_parses() {
        let cli = parse(&["grants", "list"]).expect("parses");
        assert!(matches!(
            cli.command,
            Some(Command::Grants {
                action: GrantsAction::List
            })
        ));
    }

    #[test]
    fn grants_without_list_is_an_error() {
        // The documented surface is `grants list`; requiring the verb (rather
        // than silently accepting any/no trailing token, as the pre-clap
        // `cmd_grants` did) matches what `--help` actually promises.
        assert!(parse(&["grants"]).is_err());
    }

    #[test]
    fn an_unknown_subcommand_is_refused() {
        assert!(parse(&["frobnicate"]).is_err());
    }

    #[test]
    fn completions_parses_every_shell_but_stays_hidden() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            let cli = parse(&["completions", &shell.to_string()]).unwrap_or_else(|e| {
                panic!("completions {shell}: {e}");
            });
            assert!(matches!(cli.command, Some(Command::Completions { .. })));
        }
        // Checked against the `Command` tree's own hidden flag, not against
        // rendered `--help` text: `LONG_ABOUT` is free to *mention* the word
        // "completions" without that meaning the subcommand itself is listed.
        let cmd = Cli::command();
        let completions = cmd
            .get_subcommands()
            .find(|s| s.get_name() == "completions")
            .expect("a completions subcommand exists");
        assert!(
            completions.is_hide_set(),
            "completions subcommand must be hidden from --help"
        );
    }

    /// Falsifies a dropped subcommand: removing `auth`/`get`/`grants` from
    /// [`Command`] would no longer print its name here.
    #[test]
    fn bash_completions_name_the_binary_and_every_subcommand() {
        let script = render_completions(Shell::Bash);
        assert!(script.contains("hytte-infobroker"), "{script}");
        for word in ["auth", "get", "grants"] {
            assert!(
                script.contains(word),
                "bash completions missing '{word}':\n{script}"
            );
        }
    }
}
