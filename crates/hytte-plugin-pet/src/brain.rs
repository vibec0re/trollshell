//! The pet's brain: where the words come from.
//!
//! A long-running task takes [`ThinkReq`]s from the reducer and always
//! answers with one short line. The interesting path asks a **local
//! `llama-server`** (an OpenAI-compatible `/v1/chat/completions` on
//! localhost — see `etc/systemd/user/trollshell-pet-brain.service`) with a
//! tiny persona prompt; rate-limiting, unreachability, and nonsense replies
//! all fall back to the **canned pools**, so the pet stays fully alive with
//! no model installed at all.
//!
//! HTTP is the house idiom: blocking `ureq` on a `spawn_blocking` thread
//! (same as `hytte-services`' weather fetcher). The prompt asks for one
//! plain line; [`sanitize`] enforces it whatever the model does.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::PetMsg;

/// Minimum gap between two real model calls; requests inside the gap get a
/// canned line instead. Keeps a poke-happy user from melting the CPU.
const MIN_LLM_GAP: Duration = Duration::from_secs(15);

/// Bubble budget: one line, at most this many characters.
const MAX_LINE: usize = 48;

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
    /// `$PET_LLM_URL` — base URL of a llama-server; empty disables the model
    /// entirely (canned-only pet). Default: `http://127.0.0.1:8080`.
    llm_base: Option<String>,
    /// `$PET_NAME` — the pet's name in its persona. Default: `nisse`.
    name: String,
}

impl Cfg {
    fn from_env() -> Self {
        let llm_base = match std::env::var("PET_LLM_URL") {
            Ok(s) if s.is_empty() => None,
            Ok(s) => Some(s),
            Err(_) => Some("http://127.0.0.1:8080".to_owned()),
        };
        let name = std::env::var("PET_NAME").unwrap_or_else(|_| "nisse".to_owned());
        Self { llm_base, name }
    }
}

/// The brain task. Every request gets exactly one [`PetMsg::Thought`] reply;
/// exits when either channel end is gone (session teardown).
pub async fn brain(mut rx: mpsc::UnboundedReceiver<ThinkReq>, tx: mpsc::UnboundedSender<PetMsg>) {
    let cfg = Cfg::from_env();
    // `None` = the model has never been called, so the first request may.
    let mut last_llm: Option<Instant> = None;
    let mut canned_step: u64 = 0;

    while let Some(req) = rx.recv().await {
        canned_step = canned_step.wrapping_add(1);
        let line = match &cfg.llm_base {
            Some(base) if last_llm.is_none_or(|t| t.elapsed() >= MIN_LLM_GAP) => {
                let url = format!("{base}/v1/chat/completions");
                let name = cfg.name.clone();
                last_llm = Some(Instant::now());
                let asked = tokio::task::spawn_blocking(move || ask_llm(&url, &name, req)).await;
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

// ── The model path ───────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct ChatRequest<'a> {
    messages: [ChatMessage<'a>; 2],
    max_tokens: u32,
    temperature: f32,
    /// `MiniCPM5` (and Qwen-family) templates honor this; without it a
    /// reasoning model burns the whole token budget on `reasoning_content`
    /// and `content` comes back empty. Servers whose templates don't know
    /// the kwarg simply ignore it.
    chat_template_kwargs: TemplateKwargs,
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
/// llama-server applies the model's own chat template server-side.
fn ask_llm(url: &str, name: &str, req: ThinkReq) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(2)))
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .into();
    let body = ChatRequest {
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
        chat_template_kwargs: TemplateKwargs {
            enable_thinking: false,
        },
    };
    let mut resp = agent
        .post(url)
        .send_json(&body)
        .map_err(|e| format!("http: {e}"))?;
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
        ThinkKind::Poke if req.pokes >= 4 => format!("*poke #{} in a row*", req.pokes),
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
    let line = line
        .strip_prefix(&format!("{name}:"))
        .or_else(|| line.strip_prefix(&format!("{name} :")))
        .unwrap_or(line)
        .trim();
    let cleaned: String = line.chars().filter(|&c| !is_dropped(c)).collect();
    let cleaned = cleaned.trim();
    let mut out: String = cleaned.chars().take(MAX_LINE).collect();
    if cleaned.chars().count() > MAX_LINE {
        out.push('…');
    }
    out
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
        ThinkKind::Poke if req.pokes >= 4 => CANNED_POKE_GRUMPY,
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
    fn sanitize_strips_the_self_naming_tic_and_emoji() {
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
    fn ask_llm_reports_unreachable_server() {
        // A port nothing listens on (bind-then-drop reserves then frees it).
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().expect("addr")
        };
        let err = ask_llm(
            &format!("http://{addr}/v1/chat/completions"),
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
        let base =
            std::env::var("PET_LLM_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_owned());
        let url = format!("{base}/v1/chat/completions");

        let poke = ask_llm(
            &url,
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
