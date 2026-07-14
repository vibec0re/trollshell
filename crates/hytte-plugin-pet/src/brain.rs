//! The pet's brain: where the words come from.
//!
//! A long-running task takes [`ThinkReq`]s from the reducer and always
//! answers with one short line. The interesting path asks an **OpenAI-compatible
//! `/v1/chat/completions`** with a tiny persona prompt — either a **local
//! `llama-server`** (the default, on localhost — see
//! `etc/systemd/user/trollshell-pet-brain.service`) or a **cloud endpoint like
//! [`OpenRouter`](https://openrouter.ai)** when `$PET_LLM_API_KEY` (and a
//! `$PET_LLM_MODEL`) are set. Rate-limiting, unreachability, and nonsense
//! replies all fall back to the **canned pools**, so the pet stays fully alive
//! with no model configured at all.
//!
//! # Backends
//!
//! Both backends speak the same wire shape, so switching is pure config:
//! - **llama-server** (default): `$PET_LLM_URL=http://127.0.0.1:8080`, no key,
//!   its loaded model, and the `enable_thinking:false` template kwarg.
//! - **`OpenRouter`**: set `$PET_LLM_API_KEY` (sent as `Authorization: Bearer …`)
//!   and `$PET_LLM_MODEL` (e.g. `google/gemini-2.0-flash-exp:free`); the base
//!   URL then defaults to `https://openrouter.ai/api` (override with
//!   `$PET_LLM_URL`). The llama-only template kwarg is dropped for cloud.
//!
//! HTTP is the house idiom: blocking `ureq` on a `spawn_blocking` thread
//! (same as `hytte-services`' weather fetcher). The prompt asks for one
//! plain line; [`sanitize`] enforces it whatever the model does.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::{GRUMPY_AT, PetMsg};

/// Minimum gap between two real model calls; requests inside the gap get a
/// canned line instead. Keeps a poke-happy user from melting the CPU.
const MIN_LLM_GAP: Duration = Duration::from_secs(15);

/// Bubble budget: one line, at most this many characters. Kept at the
/// canned-pool width: the sidebar card is 320px and the Node vocabulary has
/// no wrap/ellipsize yet, so a long label's *minimum* width would push the
/// whole layer surface past the sidebar (overlapping niri tiles).
const MAX_LINE: usize = 30;

/// What the reducer wants a line about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkKind {
    /// The user clicked the pet (`pokes` = how many recently).
    Poke,
    /// A rare idle musing.
    Idle,
}

/// One request for a line, with the mood context the persona prompt needs.
#[derive(Debug, Clone, Copy)]
pub struct ThinkReq {
    pub kind: ThinkKind,
    /// Local hour 0..=23 (from the shell's clock subscription).
    pub hour: u8,
    /// The pet's mood, as a prompt word.
    pub mood: &'static str,
    /// Recent poke count (context for escalating sass).
    pub pokes: u32,
}

/// Brain configuration, from the environment.
struct Cfg {
    /// Base URL of the OpenAI-compatible chat endpoint. `None` disables the
    /// model entirely (canned-only pet). Resolved by [`resolve_base`] from
    /// `$PET_LLM_URL`, defaulting to a local llama-server — or to `OpenRouter`
    /// when an API key is set (see [`resolve_base`]).
    llm_base: Option<String>,
    /// `$PET_LLM_API_KEY` — bearer token for a cloud endpoint (`OpenRouter`),
    /// sent as `Authorization: Bearer …`. `None` for a local llama-server,
    /// which needs no auth.
    api_key: Option<String>,
    /// `$PET_LLM_MODEL` — the model id in the request body. **Required by
    /// `OpenRouter`** (e.g. `google/gemini-2.0-flash-exp:free`); a local
    /// llama-server ignores it (uses its loaded model), so it's optional there.
    model: Option<String>,
    /// `$PET_NAME` — the pet's name in its persona. Default: `nisse`.
    name: String,
}

impl Cfg {
    fn from_env() -> Self {
        let api_key = std::env::var("PET_LLM_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        let model = std::env::var("PET_LLM_MODEL")
            .ok()
            .filter(|s| !s.is_empty());
        let llm_base = resolve_base(
            std::env::var("PET_LLM_URL").ok().as_deref(),
            api_key.is_some(),
        );
        let name = std::env::var("PET_NAME").unwrap_or_else(|_| "nisse".to_owned());
        Self {
            llm_base,
            api_key,
            model,
            name,
        }
    }
}

/// Resolve the model base URL. `url_env` is the raw `$PET_LLM_URL` (`None` =
/// unset, `Some("")` = set-but-empty = model disabled). With no explicit URL, a
/// present API key means "use `OpenRouter`"; otherwise the local llama-server.
fn resolve_base(url_env: Option<&str>, has_key: bool) -> Option<String> {
    match url_env {
        Some("") => None,
        Some(s) => Some(s.to_owned()),
        None if has_key => Some("https://openrouter.ai/api".to_owned()),
        None => Some("http://127.0.0.1:8080".to_owned()),
    }
}

/// The brain task. Every request gets exactly one [`PetMsg::Thought`] reply;
/// exits when either channel end is gone (session teardown).
pub async fn brain(mut rx: mpsc::UnboundedReceiver<ThinkReq>, tx: mpsc::UnboundedSender<PetMsg>) {
    let cfg = Cfg::from_env();
    // `None` = the model has never been called, so the first request may.
    let mut last_llm: Option<Instant> = None;
    let mut canned_step: u64 = 0;

    while let Some(mut req) = rx.recv().await {
        // Coalesce: anything that queued up while we were busy is stale
        // context — answer only the newest request (the reducer also gates
        // requests on `thinking`, so this is a backstop).
        while let Ok(newer) = rx.try_recv() {
            req = newer;
        }
        if tx.is_closed() {
            return; // session gone; don't start work nobody will read
        }
        canned_step = canned_step.wrapping_add(1);
        let line = match &cfg.llm_base {
            Some(base) if last_llm.is_none_or(|t| t.elapsed() >= MIN_LLM_GAP) => {
                let url = llm_url(base);
                let name = cfg.name.clone();
                let api_key = cfg.api_key.clone();
                let model = cfg.model.clone();
                let asked = tokio::task::spawn_blocking(move || {
                    ask_llm(&url, api_key.as_deref(), model.as_deref(), &name, req)
                })
                .await;
                // Stamp at completion: the gap is between calls, so a slow
                // call must not immediately qualify the next one.
                last_llm = Some(Instant::now());
                match asked {
                    Ok(Ok(line)) => line,
                    Ok(Err(e)) => {
                        eprintln!("[pet] brain offline ({e}); using a canned line");
                        canned(req, canned_step)
                    }
                    Err(e) => {
                        eprintln!("[pet] brain task failed ({e}); using a canned line");
                        canned(req, canned_step)
                    }
                }
            }
            _ => canned(req, canned_step),
        };
        if tx.send(PetMsg::Thought(line)).is_err() {
            return;
        }
    }
}

/// The chat endpoint for a configured base URL (tolerates a trailing slash —
/// llama-server 404s on `//v1/...`).
fn llm_url(base: &str) -> String {
    format!("{}/v1/chat/completions", base.trim_end_matches('/'))
}

// ── The model path ───────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct ChatRequest<'a> {
    /// The model id, sent when configured (`$PET_LLM_MODEL`). **Required by
    /// `OpenRouter`**; a llama-server ignores it (uses its loaded model), so it's
    /// omitted from the wire when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    messages: [ChatMessage<'a>; 2],
    max_tokens: u32,
    temperature: f32,
    /// `MiniCPM5` (and Qwen-family) templates honor this; without it a
    /// reasoning model burns the whole token budget on `reasoning_content`
    /// and `content` comes back empty. Servers whose templates don't know
    /// the kwarg simply ignore it. Omitted for a cloud endpoint (`OpenRouter`),
    /// which isn't llama-server and could reject the non-standard field.
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<TemplateKwargs>,
}

#[derive(serde::Serialize)]
struct TemplateKwargs {
    enable_thinking: bool,
}

#[derive(serde::Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: String,
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

/// One blocking chat-completion call (runs on a `spawn_blocking` thread).
/// llama-server applies the model's own chat template server-side; a cloud
/// endpoint (`OpenRouter`) authenticates via `api_key` and needs an explicit
/// `model`.
fn ask_llm(
    url: &str,
    api_key: Option<&str>,
    model: Option<&str>,
    name: &str,
    req: ThinkReq,
) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(2)))
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let body = ChatRequest {
        model,
        messages: [
            ChatMessage {
                role: "system",
                content: persona(name, req),
            },
            ChatMessage {
                role: "user",
                content: event(name, req),
            },
        ],
        max_tokens: 32,
        temperature: 0.9,
        // The reasoning-off hint is a llama-server template kwarg; a keyed
        // (cloud/`OpenRouter`) endpoint wouldn't know it, so send it local-only.
        chat_template_kwargs: api_key.is_none().then_some(TemplateKwargs {
            enable_thinking: false,
        }),
    };
    let mut builder = agent.post(url);
    if let Some(key) = api_key {
        builder = builder
            .header("Authorization", format!("Bearer {key}"))
            // `OpenRouter` attribution (harmless on any OpenAI-compatible server).
            .header("X-Title", "trollshell-pet");
    }
    let mut resp = builder.send_json(&body).map_err(|e| format!("http: {e}"))?;
    let parsed: ChatResponse = resp
        .body_mut()
        .read_json()
        .map_err(|e| format!("bad response body: {e}"))?;
    let raw = parsed
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .unwrap_or_default();
    let line = sanitize(&raw, name);
    if line.is_empty() {
        Err("model produced an empty line (a reasoning model? run llama-server with --reasoning-budget 0)".to_owned())
    } else {
        Ok(line)
    }
}

/// The pet's standing persona, tuned live against MiniCPM5-1B: short,
/// concrete, format stated as rules.
fn persona(name: &str, req: ThinkReq) -> String {
    format!(
        "You are {name}, a tiny cat who lives in the sidebar of Annika's \
         Linux desktop. It is around {hour}:00 and you feel {mood}. Always \
         answer as {name} the cat. Style: playful, a little sassy. Format: \
         exactly one line, at most 8 words, plain text, no quotes, no emoji.",
        hour = req.hour,
        mood = req.mood,
    )
}

/// The stimulus, phrased so a tiny model can't just parrot an instruction,
/// with the format re-anchored at the end (1B models follow the tail best).
fn event(name: &str, req: ThinkReq) -> String {
    let stim = match req.kind {
        ThinkKind::Poke if req.pokes >= GRUMPY_AT => format!("*poke #{} in a row*", req.pokes),
        ThinkKind::Poke => "*Annika pokes you*".to_owned(),
        ThinkKind::Idle if req.mood == "sleepy" => {
            "(late night, everything is quiet, you are sleepy)".to_owned()
        }
        ThinkKind::Idle => "(a quiet moment; share one tiny cat thought)".to_owned(),
    };
    format!("{stim} — say your one line now, as {name}:")
}

/// Force whatever the model said into one clean bubble line: first
/// non-empty line, quotes stripped, the model's "{name}:" self-naming tic
/// removed, emoji dropped (tiny models ignore "no emoji"), clamped to
/// [`MAX_LINE`] chars.
fn sanitize(raw: &str, name: &str) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .trim_matches(|c| c == '"' || c == '\'' || c == '“' || c == '”')
        .trim();
    // "Nisse: ..." self-naming tic, any casing.
    let line = match line.split_once(':') {
        Some((head, tail)) if head.trim().to_lowercase() == name.to_lowercase() => tail.trim(),
        _ => line,
    };
    let cleaned: String = line.chars().filter(|&c| !is_dropped(c)).collect();
    let cleaned = cleaned.trim();
    let mut out: String = cleaned.chars().take(MAX_LINE).collect();
    if cleaned.chars().count() > MAX_LINE {
        // Don't strand combining marks on the cut edge.
        while out.chars().last().is_some_and(is_combining) {
            out.pop();
        }
        out.push('…');
    }
    out
}

/// Common combining-mark ranges (a full grapheme segmenter would be a dep;
/// this covers what a chat model realistically emits).
fn is_combining(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{20D0}'..='\u{20FF}')
}

/// Codepoints to drop from bubbles: emoji blocks (kaomoji glyphs sit far
/// below them and survive) plus every double-quote lookalike — a tiny model
/// loves opening a quote it never closes, which end-trimming can't catch.
fn is_dropped(c: char) -> bool {
    matches!(
        c,
        '\u{1F000}'..='\u{1FAFF}'
            | '\u{2600}'..='\u{27BF}'
            | '\u{FE0F}'
            | '\u{200D}'
            | '"'
            | '\u{201c}'
            | '\u{201d}'
            | '\u{201e}'
            | '\u{ff02}'
    )
}

// ── The canned path ──────────────────────────────────────────────────────────

const CANNED_POKE: &[&str] = &[
    "mrrp!",
    "prrrr…",
    "hihi that tickles",
    "again! again!",
    "mya~",
    "boop received.",
];

const CANNED_POKE_GRUMPY: &[&str] = &[
    "ENOUGH.",
    "paws off, chommo.",
    "I bite (softly).",
    "hmpf.",
    "do I look like a button",
];

const CANNED_IDLE: &[&str] = &[
    "guarding your pixels.",
    "I dreamed of very small fish.",
    "the cursor moved. suspicious.",
    "soft. warm. sidebar.",
    "have you had water recently?",
    "purring at 60 fps.",
];

const CANNED_SLEEPY: &[&str] = &[
    "zzz… five more minutes.",
    "why are we awake.",
    "night shift, purr shift.",
];

/// A canned line for `req`, stepped deterministically by `step` so repeated
/// requests cycle the pool instead of repeating one entry.
pub fn canned(req: ThinkReq, step: u64) -> String {
    let pool = match req.kind {
        ThinkKind::Poke if req.pokes >= GRUMPY_AT => CANNED_POKE_GRUMPY,
        ThinkKind::Poke => CANNED_POKE,
        ThinkKind::Idle if req.mood == "sleepy" => CANNED_SLEEPY,
        ThinkKind::Idle => CANNED_IDLE,
    };
    let idx = usize::try_from(step % pool.len() as u64).unwrap_or(0);
    (*pool.get(idx).unwrap_or(&pool[0])).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn sanitize_takes_first_line_strips_quotes_and_clamps() {
        assert_eq!(sanitize("\"mrrp!\"\nsecond line", "nisse"), "mrrp!");
        assert_eq!(sanitize("\n\n  purr  \n", "nisse"), "purr");
        let long = "a".repeat(100);
        let out = sanitize(&long, "nisse");
        assert_eq!(out.chars().count(), MAX_LINE + 1, "clamped + ellipsis");
        assert!(out.ends_with('…'));
        assert_eq!(sanitize("", "nisse"), "");
    }

    #[test]
    fn llm_url_tolerates_trailing_slashes() {
        assert_eq!(
            llm_url("http://127.0.0.1:8080/"),
            "http://127.0.0.1:8080/v1/chat/completions"
        );
        assert_eq!(
            llm_url("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080/v1/chat/completions"
        );
    }

    #[test]
    fn sanitize_strips_the_self_naming_tic_and_emoji() {
        assert_eq!(
            sanitize("Nisse: purring loudly", "nisse"),
            "purring loudly",
            "the tic strip is case-insensitive"
        );
        assert_eq!(
            sanitize("warning: nap time", "nisse"),
            "warning: nap time",
            "legit colons survive"
        );
        assert_eq!(
            sanitize("nisse: sleepy cat night quiet", "nisse"),
            "sleepy cat night quiet"
        );
        assert_eq!(
            sanitize("mrrp! \u{1F63A}\u{2764}\u{FE0F}", "nisse"),
            "mrrp!"
        );
        assert_eq!(
            sanitize("(=^･ω･^=) stays", "nisse"),
            "(=^･ω･^=) stays",
            "kaomoji glyphs survive the emoji filter"
        );
    }

    #[test]
    fn canned_pools_cycle_and_pick_by_context() {
        let poke = ThinkReq {
            kind: ThinkKind::Poke,
            hour: 12,
            mood: "happy",
            pokes: 1,
        };
        let a = canned(poke, 1);
        let b = canned(poke, 2);
        assert_ne!(a, b, "steps cycle the pool");
        let grumpy = canned(ThinkReq { pokes: 5, ..poke }, 1);
        assert!(CANNED_POKE_GRUMPY.contains(&grumpy.as_str()));
        let sleepy_idle = canned(
            ThinkReq {
                kind: ThinkKind::Idle,
                hour: 2,
                mood: "sleepy",
                pokes: 0,
            },
            0,
        );
        assert!(CANNED_SLEEPY.contains(&sleepy_idle.as_str()));
    }

    /// Hermetic end-to-end of the model path: a fake llama-server on a local
    /// socket answers one canned chat completion; `ask_llm` must parse it
    /// and sanitize the content.
    #[test]
    fn ask_llm_parses_a_chat_completion() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            // Read the request (enough of it), then answer.
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let body = r#"{"choices":[{"message":{"role":"assistant","content":"\"purring at your service\"\n"}}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).expect("write");
        });

        let line = ask_llm(
            &format!("http://{addr}/v1/chat/completions"),
            None,
            None,
            "nisse",
            ThinkReq {
                kind: ThinkKind::Idle,
                hour: 15,
                mood: "happy",
                pokes: 0,
            },
        )
        .expect("parses the completion");
        assert_eq!(line, "purring at your service");
        server.join().expect("server thread");
    }

    #[test]
    fn resolve_base_defaults_and_openrouter() {
        // No URL, no key → local llama-server.
        assert_eq!(
            resolve_base(None, false).as_deref(),
            Some("http://127.0.0.1:8080")
        );
        // No URL but a key present → `OpenRouter`.
        assert_eq!(
            resolve_base(None, true).as_deref(),
            Some("https://openrouter.ai/api")
        );
        // Explicit URL always wins (even with a key).
        assert_eq!(
            resolve_base(Some("http://box:9000"), true).as_deref(),
            Some("http://box:9000")
        );
        // Empty URL disables the model entirely.
        assert_eq!(resolve_base(Some(""), true), None);
    }

    /// The `OpenRouter` path: an API key + model must ride the request as an
    /// `Authorization: Bearer` header and a `"model"` body field, and the
    /// llama-only `chat_template_kwargs` must be dropped.
    #[test]
    fn ask_llm_sends_model_and_auth_when_keyed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let captured = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            // Read until the whole request (headers + Content-Length body) is in
            // hand — a single read can return just the headers, dropping the
            // JSON body we assert on.
            let mut acc = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = sock.read(&mut buf).expect("read");
                if n == 0 {
                    break;
                }
                acc.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&acc);
                if let Some(hdr_end) = text.find("\r\n\r\n") {
                    let content_len = text[..hdr_end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if acc.len() >= hdr_end + 4 + content_len {
                        break;
                    }
                }
            }
            let body = r#"{"choices":[{"message":{"role":"assistant","content":"mrrp"}}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).expect("write");
            String::from_utf8_lossy(&acc).into_owned()
        });

        let line = ask_llm(
            &format!("http://{addr}/v1/chat/completions"),
            Some("sk-or-testkey"),
            Some("google/gemini-2.0-flash-exp:free"),
            "nisse",
            ThinkReq {
                kind: ThinkKind::Poke,
                hour: 9,
                mood: "happy",
                pokes: 1,
            },
        )
        .expect("parses the completion");
        assert_eq!(line, "mrrp");

        let req = captured.join().expect("server thread");
        let lower = req.to_ascii_lowercase();
        assert!(
            lower.contains("authorization: bearer sk-or-testkey"),
            "auth header sent: {req}"
        );
        // The model value is unique to the `model` field (format-agnostic:
        // ureq pretty-prints, so don't assume compact `"model":"…"`).
        assert!(
            req.contains("google/gemini-2.0-flash-exp:free"),
            "model in body: {req}"
        );
        assert!(
            !req.contains("chat_template_kwargs"),
            "llama-only kwarg dropped for cloud: {req}"
        );
    }

    #[test]
    fn ask_llm_reports_unreachable_server() {
        // A port nothing listens on (bind-then-drop reserves then frees it).
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr")
        };
        let err = ask_llm(
            &format!("http://{addr}/v1/chat/completions"),
            None,
            None,
            "nisse",
            ThinkReq {
                kind: ThinkKind::Poke,
                hour: 15,
                mood: "happy",
                pokes: 1,
            },
        )
        .expect_err("nothing listens there");
        assert!(err.starts_with("http:"), "{err}");
    }
}

/// Live test — needs a llama-server holding a chat model:
/// `llama-server --model brain.gguf --port 8080` (or `$PET_LLM_URL`), then
/// `cargo test -p hytte-plugin-pet -- --ignored --nocapture`.
#[cfg(test)]
mod live_tests {
    use super::*;

    #[test]
    #[ignore = "needs a running llama-server with a chat model"]
    fn live_persona_speaks_one_short_line() {
        // Honors the same env as production: set PET_LLM_API_KEY + PET_LLM_MODEL
        // to live-test the `OpenRouter` path, or leave them unset for a local
        // llama-server.
        let api_key = std::env::var("PET_LLM_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());
        let model = std::env::var("PET_LLM_MODEL")
            .ok()
            .filter(|s| !s.is_empty());
        let base = resolve_base(
            std::env::var("PET_LLM_URL").ok().as_deref(),
            api_key.is_some(),
        )
        .expect("model not disabled");
        let url = llm_url(&base);

        let poke = ask_llm(
            &url,
            api_key.as_deref(),
            model.as_deref(),
            "nisse",
            ThinkReq {
                kind: ThinkKind::Poke,
                hour: 21,
                mood: "excited",
                pokes: 2,
            },
        )
        .expect("the model answers a poke");
        eprintln!("[live] poke reaction: {poke}");
        assert!(!poke.is_empty() && poke.chars().count() <= MAX_LINE + 1);

        let idle = ask_llm(
            &url,
            api_key.as_deref(),
            model.as_deref(),
            "nisse",
            ThinkReq {
                kind: ThinkKind::Idle,
                hour: 2,
                mood: "sleepy",
                pokes: 0,
            },
        )
        .expect("the model muses");
        eprintln!("[live] idle thought: {idle}");
        assert!(!idle.is_empty() && idle.chars().count() <= MAX_LINE + 1);
    }
}
