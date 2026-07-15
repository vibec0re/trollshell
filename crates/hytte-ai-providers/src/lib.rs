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
//! HTTP is the house idiom: blocking `ureq`, meant to run on a
//! `spawn_blocking` thread (same as `hytte-services`' weather fetcher).

use std::path::PathBuf;
use std::time::Duration;

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
        }
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

/// Sampling knobs for [`chat`].
#[derive(Debug, Clone, Copy)]
pub struct ChatOpts {
    /// Upper bound on generated tokens.
    pub max_tokens: u32,
    /// Sampling temperature.
    pub temperature: f32,
}

impl Default for ChatOpts {
    fn default() -> Self {
        Self {
            max_tokens: 256,
            temperature: 0.7,
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
/// `llama-server`). `provider.model` is sent in the body when set.
///
/// Blocking — run it on a `spawn_blocking` thread. 2s connect / 10s global
/// timeout.
pub fn chat(provider: &Provider, messages: &[Message], opts: &ChatOpts) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(2)))
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let url = format!(
        "{}/v1/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    let body = ChatRequest {
        model: provider.model.as_deref(),
        messages,
        max_tokens: opts.max_tokens,
        temperature: opts.temperature,
        chat_template_kwargs: provider.api_key.is_none().then_some(TemplateKwargs {
            enable_thinking: false,
        }),
    };
    let mut builder = agent.post(&url);
    if let Some(key) = &provider.api_key {
        builder = builder
            .header("Authorization", format!("Bearer {key}"))
            // `OpenRouter` attribution (harmless on any OpenAI-compatible server).
            .header("X-Title", "trollshell");
    }
    let mut resp = builder.send_json(&body).map_err(|e| format!("http: {e}"))?;
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

/// Core of [`load_key`] with the env override and config dir injected, so it's
/// unit-testable without mutating the process environment (which is `unsafe`
/// under edition 2024, and this crate forbids `unsafe`).
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
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

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

    #[test]
    fn chat_keyed_sends_bearer_model_title_and_drops_kwarg() {
        let (base, handle) = fake_server(r#"{"choices":[{"message":{"content":"hi there"}}]}"#);
        let provider = Provider {
            // Trailing slash → also asserts the `/v1/...` path stays clean.
            base_url: format!("{base}/"),
            api_key: Some("sk-test-123".to_owned()),
            model: Some("google/gemini".to_owned()),
        };
        let msgs = [Message::system("be brief"), Message::user("hey")];
        let out = chat(
            &provider,
            &msgs,
            &ChatOpts {
                max_tokens: 8,
                temperature: 0.5,
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
    }

    #[test]
    fn chat_returns_raw_content_even_when_empty() {
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

    #[test]
    fn load_key_reads_file_env_override_and_absent() {
        // A private temp config dir; no process-env mutation (that's unsafe
        // under edition 2024 and forbidden here) — inject the dir directly.
        let dir = std::env::temp_dir().join(format!("hytte-ai-providers-{}", std::process::id()));
        let ts = dir.join("trollshell");
        std::fs::create_dir_all(&ts).expect("mkdir");
        std::fs::write(ts.join("openrouter.key"), "  sk-file-abc\n").expect("write key");

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
        // Empty file → None.
        std::fs::write(ts.join("blank.key"), "  \n").expect("write blank");
        assert!(load_key_from("blank", None, Some(dir.clone())).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }
}
