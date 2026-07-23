//! `infobroker` — the broker's command-line client (issue #487, phase 1a).
//!
//! The only client of the broker socket, and the binary the skill folder's
//! agents shell out to. Three subcommands over the boring JSON-lines wire
//! ([`hytte_plugin_infobroker::wire`]):
//!
//! ```text
//! infobroker auth --agent <name>        # → `export HYTTE_INFOBROKER_TOKEN=…`
//! infobroker get departures [--limit N] # uses $HYTTE_INFOBROKER_TOKEN → JSON
//! infobroker grants list                # the durable grants (introspection)
//! ```
//!
//! The auth line is meant to be `eval`'d:
//! `eval "$(infobroker auth --agent claude)"`. Blocking std sockets only — no
//! async runtime, so the CLI stays a fast, tiny binary.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use hytte_plugin_infobroker::paths;
use hytte_plugin_infobroker::wire::{DATASOURCE_DEPARTURES, Request, Response};

/// The environment variable carrying the session token, injected by `auth` and
/// read by `get`.
const ENV_TOKEN: &str = "HYTTE_INFOBROKER_TOKEN";

const USAGE: &str = "\
infobroker — the trollshell data broker CLI (issue #487)

USAGE:
    infobroker auth --agent <name>          mint a session token (prints an
                                            `export HYTTE_INFOBROKER_TOKEN=…`
                                            line to eval)
    infobroker get departures [--limit N]   fetch scoped data (needs the env
                                            token from a prior auth)
    infobroker grants list                  list the durable grants

Typical agent flow:
    eval \"$(infobroker auth --agent claude)\"
    infobroker get departures --limit 5";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("infobroker: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("auth") => cmd_auth(&args[1..]),
        Some("get") => cmd_get(&args[1..]),
        Some("grants") => cmd_grants(&args[1..]),
        None | Some("--help" | "-h" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown command '{other}'\n\n{USAGE}")),
    }
}

/// `auth --agent <name>` → an eval-able export line on stdout (a human note on
/// stderr), or the denial (with its how-to-grant hint) as an error.
fn cmd_auth(args: &[String]) -> Result<(), String> {
    let agent = flag_value(args, "--agent")
        .ok_or("auth: missing --agent <name>")?
        .to_owned();
    let resp = call(&Request::Auth {
        agent: agent.clone(),
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
        eprintln!(
            "infobroker: session token for '{agent}' minted (expires at unix {exp}); \
             eval the line above to use it."
        );
    }
    Ok(())
}

/// `get departures [--limit N]` → the scoped departures as pretty JSON on
/// stdout, using the env token.
fn cmd_get(args: &[String]) -> Result<(), String> {
    let datasource = args
        .first()
        .ok_or("get: missing datasource (try `get departures`)")?;
    if datasource != DATASOURCE_DEPARTURES {
        return Err(format!(
            "get: unknown datasource '{datasource}' (phase 1a ships only '{DATASOURCE_DEPARTURES}')"
        ));
    }
    let limit = match flag_value(&args[1..], "--limit") {
        Some(v) => Some(
            v.parse::<usize>()
                .map_err(|_| "get: --limit must be a whole number".to_owned())?,
        ),
        None => None,
    };
    let token = std::env::var(ENV_TOKEN).map_err(|_| {
        format!("get: {ENV_TOKEN} not set — run `eval \"$(infobroker auth --agent <name>)\"` first")
    })?;
    let resp = call(&Request::Get {
        token,
        datasource: datasource.clone(),
        limit,
    })?;
    if !resp.ok {
        return Err(deny_message(&resp));
    }
    let rows = resp.departures.unwrap_or_default();
    let json = serde_json::to_string_pretty(&rows).map_err(|e| format!("encoding output: {e}"))?;
    println!("{json}");
    Ok(())
}

/// `grants list` → one grant per line (agent, datasource, scope, decision).
fn cmd_grants(_args: &[String]) -> Result<(), String> {
    let resp = call(&Request::Grants)?;
    if !resp.ok {
        return Err(deny_message(&resp));
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
    let stream = UnixStream::connect(&path).map_err(|e| {
        format!(
            "cannot reach the broker at {} ({e}) — is the infobroker plugin running?",
            path.display()
        )
    })?;

    let mut line = serde_json::to_string(req).map_err(|e| format!("encoding request: {e}"))?;
    line.push('\n');
    (&stream)
        .write_all(line.as_bytes())
        .map_err(|e| format!("sending request: {e}"))?;

    let mut reader = BufReader::new(&stream);
    let mut resp_line = String::new();
    reader
        .read_line(&mut resp_line)
        .map_err(|e| format!("reading response: {e}"))?;
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

/// Find `--flag <value>` or `--flag=value` in `args`. Returns the value slice.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let eq_prefix = format!("{flag}=");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().map(String::as_str);
        }
        if let Some(v) = a.strip_prefix(&eq_prefix) {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::flag_value;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn flag_value_reads_spaced_and_equals_forms() {
        let spaced = args(&["--agent", "claude"]);
        assert_eq!(flag_value(&spaced, "--agent"), Some("claude"));
        let equals = args(&["--agent=claude"]);
        assert_eq!(flag_value(&equals, "--agent"), Some("claude"));
    }

    #[test]
    fn flag_value_is_none_when_absent_or_dangling() {
        assert_eq!(flag_value(&args(&["--limit", "5"]), "--agent"), None);
        // A trailing flag with no value → None (not a panic).
        assert_eq!(flag_value(&args(&["--agent"]), "--agent"), None);
    }

    #[test]
    fn flag_value_finds_the_flag_among_others() {
        let a = args(&["departures", "--limit", "5"]);
        assert_eq!(flag_value(&a, "--limit"), Some("5"));
    }
}
