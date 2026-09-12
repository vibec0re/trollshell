//! Shared OpenAI-compatible chat client for hytte plugins.
//!
//! This is the house blocking chat client: a [`Provider`] (base URL + optional
//! API key + optional model) plus [`chat`], which POSTs to
//! `{base_url}/v1/chat/completions` and returns the first choice's **raw**
//! content. Callers own their own prompt, persona, and output sanitization —
//! this crate speaks only the wire protocol and holds no opinion about the
//! text either way (an empty or nonsense reply comes back verbatim; the caller
//! decides what to do with it).
//!
//! Two presets cover the current backends:
//! - [`Provider::llama`] — a local `llama-server` (no key; also flips on the
//!   llama-only `enable_thinking:false` template kwarg, so a reasoning model
//!   doesn't burn its whole budget on hidden reasoning and return nothing).
//! - [`Provider::openrouter`] — the [`OpenRouter`](https://openrouter.ai) cloud
//!   endpoint (bearer-authenticated; the model id is required).
//!
//! Keys never live in git or a systemd unit: [`load_key`] reads
//! `$XDG_CONFIG_HOME/trollshell/{name}.key` (falling back to
//! `$HOME/.config/trollshell/{name}.key`), with a `{NAME}_API_KEY` env override
//! for CI/testing.
//!
//! One prompt-shaped thing does live here rather than in a plugin: [`owner`],
//! the session-wide `$TROLLSHELL_OWNER` resolver every persona uses to refer to
//! whoever is running the shell, with the neutral [`DEFAULT_OWNER`] fallback.
//! It sits next to [`load_key`] because both of the LLM plugins (`pet`, `caw`)
//! already depend on this crate and both need exactly one spelling of that var
//! and one fallback (#696/#706).
//!
//! HTTP is the house idiom: blocking `ureq`, meant to run on a
//! `spawn_blocking` thread (same as `hytte-services`' weather fetcher). The
//! whole request is bounded by [`ChatOpts::timeout`] ([`DEFAULT_TIMEOUT`]
//! unless the caller raises it) — read that constant before wiring a backend
//! that has a per-request budget of its own; the two have to be ordered.
//!
//! A base URL may also name a **Unix socket** (`unix://…`, #993) — that is how
//! `hytte-claude-bridge` is reached since it stopped listening on a uid-blind
//! loopback port. Same HTTP, same request bytes, different socket; see [`unix`]
//! for the URL shape and [`BRIDGE_BASE_URL`] for the canonical value.

mod unix;

pub use unix::{
    BRIDGE_BASE_URL, BRIDGE_SOCKET_DIR, BRIDGE_SOCKET_FILE, UNIX_SCHEME, bridge_socket_path,
    bridge_socket_path_in,
};

use std::path::{Path, PathBuf};
use std::time::Duration;

/// The one route this crate speaks, appended to every provider's base URL.
pub const ROUTE: &str = "/v1/chat/completions";

/// Default global budget for one [`chat`] round trip — connect, send **and**
/// read, not just the read.
///
/// **Ordering invariant:** a *server*-side per-request budget must stay
/// strictly **under** whatever [`ChatOpts::timeout`] the calling plugin
/// resolves to, so a slow turn reaches the caller as a clean
/// application-level error it can fall back from rather than as a connection
/// torn mid-read. `hytte-claude-bridge` is the live example: its
/// `CLAUDE_BRIDGE_TIMEOUT_SECS` (default 8s) is documented against this 10s
/// default, so widening the bridge's budget means raising the **client's**
/// timeout first (`PET_LLM_TIMEOUT_SECS` and friends) and only then the
/// bridge's — never the other way round.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// An OpenAI-compatible chat provider: where to POST and how to authenticate.
#[derive(Debug, Clone)]
pub struct Provider {
    /// Base URL of the endpoint; `/v1/chat/completions` is appended (a trailing
    /// slash is tolerated — `llama-server` 404s on `//v1/...`).
    pub base_url: String,
    /// Bearer token, sent as `Authorization: Bearer …` when `Some`. `None` for
    /// a local `llama-server` (needs no auth), which also flips on the
    /// llama-only `enable_thinking` template kwarg.
    pub api_key: Option<String>,
    /// Model id, sent in the request body when `Some`. Required by cloud
    /// endpoints (`OpenRouter`); a local `llama-server` ignores it (uses its
    /// loaded model), so it's optional there.
    pub model: Option<String>,
    /// A stable identifier for *this caller*, sent as `OpenAI`'s `user` field
    /// when `Some` and omitted entirely when `None` (#704).
    ///
    /// The spec defines it as an end-user identifier, and most endpoints treat
    /// it as an abuse-tracking hint they may ignore. `hytte-claude-bridge` puts
    /// it to work: without it the bridge has to *guess* which conversation a
    /// request continues by hashing the transcript, so two plugins whose
    /// prompts happen to agree land in one `claude` session — which #693
    /// measured forking silently under concurrent use. With it, each plugin
    /// names itself and gets its own.
    ///
    /// Set it to something stable and distinct per caller; the plugin id is the
    /// natural choice (unique, and it survives a restart). Leaving it `None`
    /// keeps the pre-#704 behaviour exactly.
    pub user: Option<String>,
}

impl Provider {
    /// The [`OpenRouter`](https://openrouter.ai) cloud preset: base
    /// `https://openrouter.ai/api`, `model` set, and the key loaded from
    /// `~/.config/trollshell/openrouter.key` via [`load_key`] (may be `None`
    /// if no key is configured).
    #[must_use]
    pub fn openrouter(model: impl Into<String>) -> Self {
        Self {
            base_url: "https://openrouter.ai/api".to_owned(),
            api_key: load_key("openrouter"),
            model: Some(model.into()),
            user: None,
        }
    }

    /// A local `llama-server` preset at `base_url`: no key, no explicit model
    /// (the server uses its loaded model).
    #[must_use]
    pub fn llama(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: None,
            model: None,
            user: None,
        }
    }

    /// Name this caller, so the endpoint can tell its conversation from
    /// anyone else's (#704). See [`Provider::user`].
    #[must_use]
    pub fn as_user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }
}

/// One chat message (owned). Field names are the `OpenAI` wire names, so it
/// serializes straight into the request body.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    /// A `system`-role message.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_owned(),
            content: content.into(),
        }
    }

    /// A `user`-role message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_owned(),
            content: content.into(),
        }
    }
}

/// Sampling knobs for [`chat`], plus the request budget.
#[derive(Debug, Clone, Copy)]
pub struct ChatOpts {
    /// Upper bound on generated tokens.
    pub max_tokens: u32,
    /// Sampling temperature.
    pub temperature: f32,
    /// Global budget for the whole request (ureq's `timeout_global`: connect +
    /// send + read), [`DEFAULT_TIMEOUT`] by default. Raise it for a backend
    /// whose latency legitimately exceeds it — a cold `claude --print` turn
    /// through `hytte-claude-bridge`, a large local model — and mind the
    /// ordering invariant on [`DEFAULT_TIMEOUT`]. The 2s connect timeout is
    /// separate and not configurable: that's the TCP handshake, never model
    /// latency.
    pub timeout: Duration,
}

impl Default for ChatOpts {
    fn default() -> Self {
        Self {
            max_tokens: 256,
            temperature: 0.7,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

// ── The wire ─────────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct ChatRequest<'a> {
    /// The model id, sent only when configured. Cloud endpoints require it; a
    /// `llama-server` ignores it, so it's omitted from the wire when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    messages: &'a [Message],
    max_tokens: u32,
    temperature: f32,
    /// `MiniCPM5` (and Qwen-family) templates honor this; without it a
    /// reasoning model burns the whole token budget on `reasoning_content` and
    /// `content` comes back empty. Sent local-only (a keyed cloud endpoint
    /// wouldn't know the kwarg and could reject the non-standard field);
    /// servers whose templates don't know it simply ignore it.
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<TemplateKwargs>,
    /// `OpenAI`'s caller identifier, from [`Provider::user`] (#704). Omitted
    /// from the wire entirely when unset, so a request from a caller that has
    /// not opted in is byte-identical to what this crate sent before.
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<&'a str>,
}

#[derive(serde::Serialize)]
struct TemplateKwargs {
    enable_thinking: bool,
}

#[derive(serde::Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(serde::Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(serde::Deserialize)]
struct ChatChoiceMessage {
    content: String,
}

/// POST a chat completion to `provider` and return the first choice's **raw**
/// content — which may be empty; the caller decides what to do with an empty
/// or nonsense reply.
///
/// Auth and the llama-only reasoning hint follow the provider's key:
/// `Authorization: Bearer …` + `X-Title: trollshell` when `api_key` is `Some`;
/// the `enable_thinking:false` template kwarg only when it's `None` (local
/// `llama-server`). `provider.model` is sent in the body when set, and
/// `provider.user` likewise (#704) — both are omitted from the wire when
/// unset, never sent as null.
///
/// A `unix://` base URL (#993) is dialled over a [`UnixStream`] instead of a
/// TCP socket — everything above the socket, request bytes included, is
/// unchanged. A socket URL that cannot be resolved is an `Err` naming why;
/// there is deliberately **no** fall back to a loopback port, because that is
/// the uid-blind reachability #993 closed. See [`unix`].
///
/// [`UnixStream`]: std::os::unix::net::UnixStream
///
/// Blocking — run it on a `spawn_blocking` thread. 2s connect timeout; the
/// whole round trip is bounded by [`ChatOpts::timeout`] ([`DEFAULT_TIMEOUT`]
/// by default).
pub fn chat(provider: &Provider, messages: &[Message], opts: &ChatOpts) -> Result<String, String> {
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(2)))
        .timeout_global(Some(opts.timeout))
        // Don't collapse a 4xx/5xx into a bare status error — we want to read
        // the endpoint's JSON error body (OpenRouter explains *why*: an invalid
        // or restricted model, an auth problem…) and surface it in the message.
        .http_status_as_error(false)
        .build();
    let (agent, url) = match unix::socket_target(&provider.base_url) {
        Some(Ok(path)) => (unix::agent(config, path), unix::request_url(ROUTE)),
        Some(Err(why)) => return Err(format!("bad {UNIX_SCHEME} base url: {why}")),
        None => (
            ureq::Agent::from(config),
            format!("{}{ROUTE}", provider.base_url.trim_end_matches('/')),
        ),
    };
    let body = ChatRequest {
        model: provider.model.as_deref(),
        messages,
        max_tokens: opts.max_tokens,
        temperature: opts.temperature,
        chat_template_kwargs: provider.api_key.is_none().then_some(TemplateKwargs {
            enable_thinking: false,
        }),
        user: provider.user.as_deref(),
    };
    let mut builder = agent.post(&url);
    if let Some(key) = &provider.api_key {
        builder = builder
            .header("Authorization", format!("Bearer {key}"))
            // `OpenRouter` attribution (harmless on any OpenAI-compatible server).
            .header("X-Title", "trollshell");
    }
    let mut resp = builder.send_json(&body).map_err(|e| format!("http: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        // Surface the endpoint's error body so callers see the real cause (a bad
        // model id 400s with `{"error":{"message":"…is not a valid model id"}}`,
        // etc.) instead of a bare status code. Trimmed so the pet's log line and
        // the fallback path stay sane.
        let body: String = resp
            .body_mut()
            .read_to_string()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let detail: String = body.chars().take(200).collect();
        return Err(format!("http {}: {detail}", status.as_u16()));
    }
    let parsed: ChatResponse = resp
        .body_mut()
        .read_json()
        .map_err(|e| format!("bad response body: {e}"))?;
    Ok(parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .unwrap_or_default())
}

// ── Key from file ────────────────────────────────────────────────────────────

/// Load an API key for `name` from `$XDG_CONFIG_HOME/trollshell/{name}.key`
/// (falling back to `$HOME/.config/trollshell/{name}.key`), read and trimmed;
/// a non-empty value ⇒ `Some`. The `{NAME}_API_KEY` env var (upper-cased
/// `name`, e.g. `OPENROUTER_API_KEY`) overrides the file when set — for
/// CI/testing. Never panics on a missing file.
#[must_use]
pub fn load_key(name: &str) -> Option<String> {
    let env_override = std::env::var(format!("{}_API_KEY", name.to_uppercase())).ok();
    load_key_from(name, env_override, config_dir())
}

/// `$XDG_CONFIG_HOME` (if set and non-empty) else `$HOME/.config`.
fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|x| !x.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

/// Refuse a key file that grants any access to group or other — the way
/// `ssh` refuses a loose private key (#1169). `mode & 0o077 != 0` covers
/// group/other read, write, *or* execute in one test, which is the same bit
/// group `ssh-keygen`/`sshd` check. Pure and path-injected so it's testable
/// against a real tempfile without touching the loader's I/O.
///
/// `pub` since #1169's review: `hytte-claude-bridge`'s own key loader
/// (`messages.rs`'s `load_key_from`, for `anthropic.key`) already depends on
/// this crate and calls this directly rather than re-deriving the same
/// predicate — one rule, not two copies that can drift.
pub fn check_key_file_permissions(path: &Path, mode: u32) -> Result<(), String> {
    let mode = mode & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "key file {} has mode {mode:03o}, readable/writable by group or other — \
             refusing to load it (chmod 600 it)",
            path.display(),
        ));
    }
    Ok(())
}

/// Core of [`load_key`] with the env override and config dir injected, so it's
/// unit-testable without mutating the process environment (which is `unsafe`
/// under edition 2024, and this crate forbids `unsafe`).
///
/// The happy path — a `0600`-or-tighter file, or no file at all (the ordinary
/// "no key configured" case) — is byte-identical to before #1169. What's new
/// is the middle case: a key file that *exists* but is readable by group or
/// other is refused rather than read, with [`check_key_file_permissions`]'s
/// message printed to stderr — this crate carries no logging dependency (and
/// #1169 adds none), so a plain `eprintln!` is the loudest warning available
/// without moving `Cargo.lock`.
fn load_key_from(
    name: &str,
    env_override: Option<String>,
    config_dir: Option<PathBuf>,
) -> Option<String> {
    if let Some(v) = env_override {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    let path = config_dir?.join("trollshell").join(format!("{name}.key"));
    if let Ok(meta) = std::fs::metadata(&path) {
        use std::os::unix::fs::PermissionsExt as _;
        if let Err(e) = check_key_file_permissions(&path, meta.permissions().mode()) {
            eprintln!("hytte-ai-providers: {e}");
            return None;
        }
    }
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

// ── The desktop owner ────────────────────────────────────────────────────────

/// Neutral stand-in for the desktop owner when `$TROLLSHELL_OWNER` is unset.
///
/// A persona that needs a possessive ("…lives in the sidebar of X's Linux
/// desktop") must have *something* to say, and the two wrong answers are both
/// on record: hardcoding a specific person's name (#696/#706 — not every
/// deployment's owner is named Annika) and guessing one from `$USER`/GECOS
/// (a login name is not what a human wants to be called). So: a neutral
/// phrase, and the owner opts in to a real one.
pub const DEFAULT_OWNER: &str = "your human";

/// Resolve the desktop owner's name from `$TROLLSHELL_OWNER`, falling back to
/// [`DEFAULT_OWNER`].
///
/// One session-wide variable shared by every plugin persona that refers to
/// whoever is running the shell — `pet`'s cat and `caw`'s crow both read it
/// through here, so there is exactly one spelling of the knob and one
/// fallback. Reads the process environment, so resolve it **once** at config
/// time (`Cfg::from_env` and friends), not per prompt.
#[must_use]
pub fn owner() -> String {
    owner_or(std::env::var("TROLLSHELL_OWNER").ok().as_deref())
}

/// Core of [`owner`] with the raw env value injected, so a caller can unit-test
/// its prompts against a chosen owner without mutating the process environment
/// (`unsafe` under edition 2024, which this crate forbids) — the same split as
/// [`load_key`]/`load_key_from`. Trimmed, and blank or whitespace-only counts
/// as unset.
#[must_use]
pub fn owner_or(raw: Option<&str>) -> String {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| DEFAULT_OWNER.to_owned(), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    use std::sync::{Mutex, PoisonError};

    /// Every test that binds an ephemeral port (`fake_server`,
    /// `fake_server_status`, or the bind-then-drop dead-port trick) takes this
    /// for its whole body. `TcpListener::bind("127.0.0.1:0")` hands out
    /// whatever port the kernel currently considers free; cargo runs these
    /// tests as parallel threads in one process, so without serialization a
    /// port one test just released can be recycled straight into another
    /// test's listener while both are mid-flight — see #716. A poisoned
    /// guard (some *other* test panicked while holding it) must not cascade
    /// into every later socket test failing on a misleading poison error, so
    /// unwrap through the poison rather than propagating it — mirrors
    /// `hytte_reactive::shared`'s `TEST_LOCK`.
    static TEST_SOCKETS: Mutex<()> = Mutex::new(());

    /// Find `needle` in `buf`.
    fn window_pos(buf: &[u8], needle: &[u8]) -> Option<usize> {
        buf.windows(needle.len()).position(|w| w == needle)
    }

    /// Parse a `Content-Length` from raw header text (0 if absent).
    fn content_length(head: &str) -> usize {
        for line in head.lines() {
            if let Some((k, v)) = line.split_once(':')
                && k.trim().eq_ignore_ascii_case("content-length")
            {
                return v.trim().parse().unwrap_or(0);
            }
        }
        0
    }

    /// Read a full HTTP request (headers + the declared body) off `sock`.
    fn capture_request(sock: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            if let Some(hdr_end) = window_pos(&buf, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..hdr_end]);
                if buf.len() >= hdr_end + 4 + content_length(&head) {
                    break;
                }
            }
            let n = sock.read(&mut tmp).expect("read request");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Split a raw request into `(head, body)` at the blank line.
    fn split_request(raw: &str) -> (&str, &str) {
        raw.split_once("\r\n\r\n").unwrap_or((raw, ""))
    }

    /// A one-shot fake OpenAI-compatible server: captures the request and
    /// replies with `resp_body`. Returns `(base_url, handle→raw request)`.
    fn fake_server(resp_body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let raw = capture_request(&mut sock);
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{resp_body}",
                resp_body.len(),
            );
            sock.write_all(resp.as_bytes()).expect("write response");
            raw
        });
        (format!("http://{addr}"), handle)
    }

    /// Like [`fake_server`] but replies with a non-200 `status` line + `body`.
    fn fake_server_status(
        status: &'static str,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let _ = capture_request(&mut sock);
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len(),
            );
            sock.write_all(resp.as_bytes()).expect("write response");
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn chat_surfaces_endpoint_error_body_on_non_2xx() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        // A bad/restricted model 400s with a JSON error body; `chat` must return
        // that cause, not a bare status code (the OpenRouter debugging story).
        let (base, handle) = fake_server_status(
            "400 Bad Request",
            r#"{"error":{"message":"google/nope is not a valid model id","code":400}}"#,
        );
        let provider = Provider {
            base_url: base,
            api_key: Some("sk-x".to_owned()),
            model: Some("google/nope".to_owned()),
            user: None,
        };
        let err = chat(&provider, &[Message::user("hi")], &ChatOpts::default())
            .expect_err("a non-2xx is an error");
        assert!(err.contains("400"), "status in the error: {err}");
        assert!(
            err.contains("not a valid model id"),
            "endpoint body surfaced: {err}"
        );
        handle.join().expect("server thread");
    }

    #[test]
    fn chat_keyed_sends_bearer_model_title_and_drops_kwarg() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let (base, handle) = fake_server(r#"{"choices":[{"message":{"content":"hi there"}}]}"#);
        let provider = Provider {
            // Trailing slash → also asserts the `/v1/...` path stays clean.
            base_url: format!("{base}/"),
            api_key: Some("sk-test-123".to_owned()),
            model: Some("google/gemini".to_owned()),
            user: None,
        };
        let msgs = [Message::system("be brief"), Message::user("hey")];
        let out = chat(
            &provider,
            &msgs,
            &ChatOpts {
                max_tokens: 8,
                temperature: 0.5,
                ..ChatOpts::default()
            },
        )
        .expect("chat succeeds");
        assert_eq!(out, "hi there");

        let raw = handle.join().expect("server thread");
        let (head, body) = split_request(&raw);
        assert!(
            head.starts_with("POST /v1/chat/completions "),
            "trailing slash tolerated: {head:?}"
        );
        // ureq lower-cases header *names* (values keep their case).
        let head_lc = head.to_ascii_lowercase();
        assert!(
            head_lc.contains("authorization: bearer sk-test-123"),
            "{head:?}"
        );
        assert!(head_lc.contains("x-title: trollshell"), "{head:?}");
        // ureq pretty-prints the JSON body — assert on parsed values, not text.
        let json: serde_json::Value = serde_json::from_str(body).expect("body is json");
        assert_eq!(json["model"], "google/gemini");
        assert_eq!(json["max_tokens"], 8);
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][0]["content"], "be brief");
        assert_eq!(json["messages"][1]["role"], "user");
        assert_eq!(json["messages"][1]["content"], "hey");
        assert!(
            json.get("chat_template_kwargs").is_none(),
            "keyed → the llama-only kwarg is dropped: {json}"
        );
    }

    #[test]
    fn chat_local_sends_kwarg_and_no_auth_no_model() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let (base, handle) = fake_server(r#"{"choices":[{"message":{"content":"purr"}}]}"#);
        let provider = Provider::llama(base);
        let out =
            chat(&provider, &[Message::user("hey")], &ChatOpts::default()).expect("chat succeeds");
        assert_eq!(out, "purr");

        let raw = handle.join().expect("server thread");
        let (head, body) = split_request(&raw);
        assert!(
            !head.to_ascii_lowercase().contains("authorization"),
            "local → no auth header: {head:?}"
        );
        let json: serde_json::Value = serde_json::from_str(body).expect("body is json");
        assert!(
            json.get("model").is_none(),
            "llama → no model in body: {json}"
        );
        assert_eq!(
            json["chat_template_kwargs"]["enable_thinking"], false,
            "local → the reasoning-off kwarg is sent: {json}"
        );
        assert!(
            json.get("user").is_none(),
            "no identity → no `user` on the wire at all (#704): {json}"
        );
    }

    /// A caller that has not named itself must send a body byte-identical to
    /// the pre-#704 one: `user` **absent**, not `null`. `hytte-claude-bridge`
    /// reads an absent field as "fall back to the content hash", and a `null`
    /// would parse the same way here but is a needless wire change that a
    /// stricter `OpenAI`-compatible endpoint could reject (#704).
    #[test]
    fn chat_omits_the_user_field_entirely_when_unset() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let (base, handle) = fake_server(r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        chat(
            &Provider::llama(base),
            &[Message::user("hey")],
            &ChatOpts::default(),
        )
        .expect("chat succeeds");

        let raw = handle.join().expect("server thread");
        let (_head, body) = split_request(&raw);
        assert!(
            !body.contains("user\":"),
            "the key must not appear at all, null included: {body}"
        );
    }

    /// …and a caller that *has* named itself sends it verbatim, which is the
    /// only thing carrying the identity to the bridge (#704).
    #[test]
    fn chat_sends_the_user_field_when_set() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let (base, handle) = fake_server(r#"{"choices":[{"message":{"content":"ok"}}]}"#);
        chat(
            &Provider::llama(base).as_user("pet"),
            &[Message::user("hey")],
            &ChatOpts::default(),
        )
        .expect("chat succeeds");

        let raw = handle.join().expect("server thread");
        let (_head, body) = split_request(&raw);
        let json: serde_json::Value = serde_json::from_str(body).expect("body is json");
        assert_eq!(json["user"], "pet");
    }

    #[test]
    fn chat_returns_raw_content_even_when_empty() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        let (base, handle) = fake_server(r#"{"choices":[{"message":{"content":""}}]}"#);
        let out = chat(
            &Provider::llama(base),
            &[Message::user("x")],
            &ChatOpts::default(),
        )
        .expect("chat succeeds");
        assert_eq!(
            out, "",
            "empty content comes back verbatim — the caller decides"
        );
        handle.join().expect("server thread");
    }

    #[test]
    fn chat_reports_unreachable_server() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        // A port nothing listens on (bind-then-drop reserves then frees it).
        let addr = {
            let l = TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr")
        };
        let err = chat(
            &Provider::llama(format!("http://{addr}")),
            &[Message::user("x")],
            &ChatOpts::default(),
        )
        .expect_err("nothing listens there");
        assert!(err.starts_with("http:"), "{err}");
    }

    /// The default budget is the number every server-side budget is written
    /// against — `hytte-claude-bridge`'s `DEFAULT_BUDGET` (8s) and its
    /// `the_default_budget_is_under_the_clients_global_timeout` test both
    /// hardcode this 10s. Moving it here silently would break that ordering.
    #[test]
    fn the_default_request_budget_is_ten_seconds() {
        assert_eq!(DEFAULT_TIMEOUT, Duration::from_secs(10));
        assert_eq!(ChatOpts::default().timeout, DEFAULT_TIMEOUT);
    }

    /// `ChatOpts::timeout` is really wired to the agent, not just carried:
    /// a server that accepts the connection (kernel backlog) and never answers
    /// must trip the *global* budget, not hang for the 10s default.
    #[test]
    fn chat_honours_the_configured_request_timeout() {
        let _guard = TEST_SOCKETS.lock().unwrap_or_else(PoisonError::into_inner);
        // Bound but never accepted: the handshake completes from the backlog,
        // so this is a read stall, not a connect failure.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let started = std::time::Instant::now();
        let err = chat(
            &Provider::llama(format!("http://{addr}")),
            &[Message::user("x")],
            &ChatOpts {
                timeout: Duration::from_millis(200),
                ..ChatOpts::default()
            },
        )
        .expect_err("the server never answers");
        assert!(err.starts_with("http:"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "gave up after {:?} — the configured 200ms budget wasn't applied",
            started.elapsed(),
        );
        drop(listener);
    }

    /// A private temp directory for one test's socket, named after the test so
    /// two cannot collide. Unix socket paths are capped at ~108 bytes, so this
    /// stays short. No `tempfile` dev-dependency — same hand-rolled shape the
    /// key-file test below uses.
    fn socket_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hytte-ai-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// A one-shot fake `OpenAI`-compatible server **on a Unix socket**: the
    /// twin of [`fake_server`], captures the request and replies with
    /// `resp_body`. Returns `(base_url, handle→raw request)`.
    fn fake_unix_server(
        dir: &Path,
        resp_body: &'static str,
    ) -> (String, std::thread::JoinHandle<String>) {
        let path = dir.join("bridge.sock");
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let raw = capture_unix_request(&mut sock);
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{resp_body}",
                resp_body.len(),
            );
            sock.write_all(resp.as_bytes()).expect("write response");
            raw
        });
        (format!("unix://{}", path.display()), handle)
    }

    /// [`capture_request`] for a `UnixStream` (the helper above is typed to
    /// `TcpStream`; the logic is the same bytes either way).
    fn capture_unix_request(sock: &mut std::os::unix::net::UnixStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            if let Some(hdr_end) = window_pos(&buf, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..hdr_end]);
                if buf.len() >= hdr_end + 4 + content_length(&head) {
                    break;
                }
            }
            let n = sock.read(&mut tmp).expect("read request");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// **The #993 round trip.** A `unix://` base URL must complete a real chat
    /// completion over a real Unix socket, sending the *same* request the TCP
    /// path sends — same route, same JSON body — because
    /// `hytte-claude-bridge`'s parser is written against exactly those bytes.
    ///
    /// This is also the falsifier for "route the socket URL over TCP anyway":
    /// there is no host to connect to in a `unix://` URL, so that mutation
    /// cannot produce a reply at all.
    #[test]
    fn chat_round_trips_over_a_unix_socket() {
        let dir = socket_dir("roundtrip");
        let (base, handle) =
            fake_unix_server(&dir, r#"{"choices":[{"message":{"content":"meow"}}]}"#);
        let provider = Provider {
            base_url: base,
            api_key: Some("local-bridge".to_owned()),
            model: None,
            user: Some("pet".to_owned()),
        };
        let out = chat(&provider, &[Message::user("hey")], &ChatOpts::default())
            .expect("chat succeeds over the socket");
        assert_eq!(out, "meow");

        let raw = handle.join().expect("server thread");
        let (head, body) = split_request(&raw);
        assert!(
            head.starts_with("POST /v1/chat/completions "),
            "the route is unchanged over a socket: {head:?}"
        );
        let head_lc = head.to_ascii_lowercase();
        assert!(
            head_lc.contains("host: localhost"),
            "a socket has no host; the placeholder authority is what ships: {head:?}"
        );
        let json: serde_json::Value = serde_json::from_str(body).expect("body is json");
        assert_eq!(json["messages"][0]["content"], "hey");
        assert_eq!(json["user"], "pet");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A socket URL that names nothing resolvable is an **error**, never a
    /// quiet fall back to a loopback port — the fallback would re-open the
    /// uid-blind hole #993 closed.
    #[test]
    fn chat_refuses_an_unresolvable_socket_url_instead_of_falling_back() {
        let err = chat(
            &Provider::llama("unix://trollshell/relative.sock"),
            &[Message::user("x")],
            &ChatOpts::default(),
        )
        .expect_err("a relative socket path names nothing");
        assert!(err.contains("unix://"), "{err}");
        assert!(err.contains("absolute"), "{err}");
    }

    /// A socket path with nothing listening is a clean transport error the
    /// caller can fall back from, exactly like a refused TCP connect.
    #[test]
    fn chat_reports_an_absent_socket() {
        let dir = socket_dir("absent");
        let err = chat(
            &Provider::llama(format!("unix://{}", dir.join("nobody.sock").display())),
            &[Message::user("x")],
            &ChatOpts::default(),
        )
        .expect_err("nothing is listening there");
        assert!(err.starts_with("http:"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_key_reads_file_env_override_and_absent() {
        // A private temp config dir; no process-env mutation (that's unsafe
        // under edition 2024 and forbidden here) — inject the dir directly.
        let dir = std::env::temp_dir().join(format!("hytte-ai-providers-{}", std::process::id()));
        let ts = dir.join("trollshell");
        std::fs::create_dir_all(&ts).expect("mkdir");
        std::fs::write(ts.join("openrouter.key"), "  sk-file-abc\n").expect("write key");
        // #1169: the happy path needs an explicit 0600, not whatever the
        // ambient umask left it at — this test's whole claim is "a properly
        // permissioned file loads", so it must not depend on the environment.
        chmod(&ts.join("openrouter.key"), 0o600);

        // File read + trimmed.
        assert_eq!(
            load_key_from("openrouter", None, Some(dir.clone())).as_deref(),
            Some("sk-file-abc"),
        );
        // Env override wins over the file (and is trimmed).
        assert_eq!(
            load_key_from(
                "openrouter",
                Some("  sk-env-9 ".to_owned()),
                Some(dir.clone())
            )
            .as_deref(),
            Some("sk-env-9"),
        );
        // A blank override falls through to the file.
        assert_eq!(
            load_key_from("openrouter", Some("   ".to_owned()), Some(dir.clone())).as_deref(),
            Some("sk-file-abc"),
        );
        // Missing file, no override → None.
        assert!(load_key_from("absent", None, Some(dir.clone())).is_none());
        // Empty file → None (0600 so it's the blank content being refused,
        // not the permission check landing first).
        std::fs::write(ts.join("blank.key"), "  \n").expect("write blank");
        chmod(&ts.join("blank.key"), 0o600);
        assert!(load_key_from("blank", None, Some(dir.clone())).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A tiny `chmod` wrapper so the tests read as intent, not
    /// `PermissionsExt` boilerplate at every call site.
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    /// **The check itself** (#1169): `0600`/`0400`/`0000` are fine (no
    /// group/other bit set); anything that sets a group or other bit —
    /// world-readable `0644` included — is refused, with the file's path and
    /// its mode named in the message so a human can act on it without
    /// guessing which file or what to `chmod`.
    #[test]
    fn check_key_file_permissions_refuses_group_or_other_access() {
        let dir =
            std::env::temp_dir().join(format!("hytte-ai-providers-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("openrouter.key");
        std::fs::write(&path, "sk-x").expect("write key");

        for ok_mode in [0o600, 0o400, 0o000] {
            chmod(&path, ok_mode);
            assert!(
                check_key_file_permissions(&path, ok_mode).is_ok(),
                "mode {ok_mode:03o} grants nothing to group/other and must be accepted",
            );
        }
        for bad_mode in [0o644, 0o640, 0o604, 0o755, 0o666] {
            chmod(&path, bad_mode);
            let err = check_key_file_permissions(&path, bad_mode)
                .expect_err("group/other access must be refused");
            assert!(
                err.contains(&path.display().to_string()),
                "the error must name the file: {err}",
            );
            assert!(
                err.contains(&format!("{bad_mode:03o}")),
                "the error must name the mode: {err}",
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// **End to end through the loader**: a world-readable `0644` key file is
    /// refused by [`load_key_from`] itself, not just the pure checker — the
    /// live-verify claim in the PR ("a world-readable key silently loaded
    /// before, now it doesn't") is exactly this behaviour.
    ///
    /// Falsification: delete the `check_key_file_permissions` call out of
    /// `load_key_from` and this test goes red — `sk-loose` loads clean.
    #[test]
    fn load_key_from_refuses_a_world_readable_file() {
        let dir =
            std::env::temp_dir().join(format!("hytte-ai-providers-loose-{}", std::process::id()));
        let ts = dir.join("trollshell");
        std::fs::create_dir_all(&ts).expect("mkdir");
        let path = ts.join("openrouter.key");
        std::fs::write(&path, "sk-loose").expect("write key");
        chmod(&path, 0o644);

        assert!(
            load_key_from("openrouter", None, Some(dir.clone())).is_none(),
            "a world-readable key file must be refused, not loaded",
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The shared owner resolver (#696/#706): an explicit `$TROLLSHELL_OWNER`
    /// wins after trimming, anything blank counts as unset, and the fallback
    /// is neutral rather than a specific person.
    #[test]
    fn owner_or_prefers_the_explicit_value_and_falls_back_when_unset_or_blank() {
        assert_eq!(owner_or(Some("kaesaecracker")), "kaesaecracker");
        assert_eq!(owner_or(Some("  Mara  ")), "Mara", "trimmed");
        assert_eq!(owner_or(None), DEFAULT_OWNER);
        assert_eq!(owner_or(Some("")), DEFAULT_OWNER);
        assert_eq!(owner_or(Some("   ")), DEFAULT_OWNER);
        assert!(
            !DEFAULT_OWNER.to_lowercase().contains("annika"),
            "the shared fallback must never be one particular person: {DEFAULT_OWNER}",
        );
    }
}
