//! The pet's brain: where the words come from.
//!
//! A long-running task takes [`ThinkReq`]s from the reducer and always
//! answers with one short line. The interesting path asks an
//! OpenAI-compatible `/v1/chat/completions` endpoint — through the shared
//! [`hytte_ai_providers`] client — with a tiny persona prompt. Two backends,
//! chosen by config:
//!
//! - **[`OpenRouter`](https://openrouter.ai)** (a cloud LLM — **the default
//!   when a key is configured**): a key from
//!   `~/.config/trollshell/openrouter.key` (see
//!   [`hytte_ai_providers::load_key`]) or `$PET_LLM_API_KEY`, plus a
//!   `$PET_LLM_MODEL`, or
//! - a **local `llama-server`** — opt in with `$PET_LLM_URL` (e.g.
//!   `http://127.0.0.1:8080`; see `etc/systemd/user/trollshell-pet-brain.service`).
//!
//! With **no key and no `$PET_LLM_URL`** the brain resolves to canned-only up
//! front (a keyless cloud call would only 401, so it never attempts one — see
//! [`resolve_provider`]). Rate-limiting, unreachability, and nonsense (or empty)
//! replies from a configured backend all fall back to the **canned pools** too,
//! so the pet stays fully alive with no model configured at all.
//!
//! Two more knobs tune the call itself, both whole seconds and both falling
//! back to their compiled default when unset/blank/unparsable/`0`:
//! `$PET_LLM_MIN_GAP_SECS` (throttle between real calls, default 15) and
//! `$PET_LLM_TIMEOUT_SECS` (how long one call may take, default
//! [`hytte_ai_providers::DEFAULT_TIMEOUT`]). See [`Cfg::from_env`] — in
//! particular for the ordering a server-side budget like
//! `CLAUDE_BRIDGE_TIMEOUT_SECS` has to respect.
//!
//! The shared client owns the HTTP + provider config; the pet keeps the
//! persona, the [`sanitize`] step, and the "empty line ⇒ offline ⇒ canned"
//! policy. The prompt asks for one plain line; [`sanitize`] enforces it
//! whatever the model does — and its word budget is *derived* from that
//! character clamp ([`PROMPT_MAX_WORDS`]), not chosen separately.

use std::time::Duration;

use hytte_ai_providers::{ChatOpts, Message, Provider};
use hytte_plugin::tokio::sync::mpsc;
use hytte_plugin::tokio::time::Instant;

use crate::{GRUMPY_AT, PetMsg};

/// Default minimum gap between two real model calls; requests inside the gap
/// get a canned line instead. Keeps a poke-happy user from melting the CPU.
/// Override with `$PET_LLM_MIN_GAP_SECS` (see [`Cfg::from_env`]).
const MIN_LLM_GAP: Duration = Duration::from_secs(15);

/// Bubble budget: one line, at most this many characters. Kept at the
/// canned-pool width: the sidebar card is 320px and the Node vocabulary has
/// no wrap/ellipsize yet, so a long label's *minimum* width would push the
/// whole layer surface past the sidebar (overlapping niri tiles).
const MAX_LINE: usize = 30;

/// Word budget stated in the persona prompt. It has to be derivable from
/// [`MAX_LINE`], not picked independently: [`sanitize`] hard-clamps at
/// [`MAX_LINE`] *characters*, so a prompt asking for more words than fit
/// makes an obedient model's reply reliably end in an ellipsis (#700 — the
/// prompt said 8 words against a 30-char clamp, ~45 chars of English). The
/// tests keep the two honest: a sentence of this many average-length English
/// words has to come back out of [`sanitize`] unclamped.
const PROMPT_MAX_WORDS: usize = 5;

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
    /// The resolved chat [`Provider`], or `None` for a canned-only pet (an
    /// empty `$PET_LLM_URL`, or no key and no URL). See [`resolve_provider`].
    provider: Option<Provider>,
    /// `$PET_NAME` — the pet's name in its persona. Default: `nisse`.
    name: String,
    /// `$PET_LLM_MIN_GAP_SECS` — throttle between two real model calls.
    /// Default: [`MIN_LLM_GAP`].
    min_llm_gap: Duration,
    /// `$PET_LLM_TIMEOUT_SECS` — how long one model call may take before the
    /// client hangs up. Default: [`hytte_ai_providers::DEFAULT_TIMEOUT`].
    llm_timeout: Duration,
}

impl Cfg {
    /// Read the brain's knobs off the process environment.
    ///
    /// `$PET_LLM_MIN_GAP_SECS` and `$PET_LLM_TIMEOUT_SECS` are whole seconds;
    /// unset, blank, unparsable or `0` falls back to the compiled default
    /// (same shape as `hytte-claude-bridge`'s `CLAUDE_BRIDGE_TIMEOUT_SECS`).
    ///
    /// Raising `$PET_LLM_TIMEOUT_SECS` is what makes room for a slow backend —
    /// notably `hytte-claude-bridge`, whose own `CLAUDE_BRIDGE_TIMEOUT_SECS`
    /// must stay strictly **under** this value or a slow turn tears the
    /// connection instead of returning a 504 the pet can fall back from. See
    /// [`hytte_ai_providers::DEFAULT_TIMEOUT`] for that ordering invariant.
    fn from_env() -> Self {
        // The pet's OpenRouter key: the shared key file (`openrouter.key`, or
        // its `OPENROUTER_API_KEY` override) first, then the pet-specific
        // `$PET_LLM_API_KEY`.
        let key = hytte_ai_providers::load_key("openrouter").or_else(|| {
            std::env::var("PET_LLM_API_KEY")
                .ok()
                .filter(|s| !s.is_empty())
        });
        let model = std::env::var("PET_LLM_MODEL")
            .ok()
            .filter(|s| !s.is_empty());
        let provider = resolve_provider(std::env::var("PET_LLM_URL").ok().as_deref(), key, model);
        let name = std::env::var("PET_NAME").unwrap_or_else(|_| "nisse".to_owned());
        let min_llm_gap = secs_or(
            std::env::var("PET_LLM_MIN_GAP_SECS").ok().as_deref(),
            MIN_LLM_GAP,
        );
        let llm_timeout = secs_or(
            std::env::var("PET_LLM_TIMEOUT_SECS").ok().as_deref(),
            hytte_ai_providers::DEFAULT_TIMEOUT,
        );
        Self {
            provider,
            name,
            min_llm_gap,
            llm_timeout,
        }
    }
}

/// Parse a whole-seconds env value into a [`Duration`], falling back to
/// `default` when it's absent, blank, unparsable, or zero. Split out of
/// [`Cfg::from_env`] so the parse is unit-testable without mutating the
/// process environment (`unsafe` under edition 2024, which this crate
/// forbids) — same split as `hytte_ai_providers::load_key`'s.
fn secs_or(raw: Option<&str>, default: Duration) -> Duration {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map_or(default, Duration::from_secs)
}

/// Resolve the pet's [`Provider`] from its env inputs. `url_env` is the raw
/// `$PET_LLM_URL` (`None` = unset, `Some("")` = set-but-empty = model
/// disabled). With no explicit URL, the default is **`OpenRouter`** (the pet's
/// cloud brain) **only when a `key` is present**: a keyless `OpenRouter` call
/// always 401s, so with neither a URL nor a key this short-circuits to `None`
/// (canned-only) rather than burning a doomed round-trip per thought (#438). An
/// explicit `$PET_LLM_URL` selects a local/self-hosted backend (e.g. a
/// `llama-server`, which needs no key) as the base, with any `key`/`model`
/// layered on.
fn resolve_provider(
    url_env: Option<&str>,
    key: Option<String>,
    model: Option<String>,
) -> Option<Provider> {
    match url_env {
        // Explicitly empty `$PET_LLM_URL` → model disabled (canned-only pet).
        Some("") => None,
        // An explicit URL is a local/self-hosted backend that needs no key —
        // keep it even keyless.
        Some(url) => Some(Provider {
            base_url: url.to_owned(),
            api_key: key,
            model,
        }),
        // No URL → the OpenRouter cloud default, but ONLY with a key. Keyless →
        // `None` (canned-only): the call would just 401, so skip it (#438).
        None => key.map(|key| Provider {
            base_url: "https://openrouter.ai/api".to_owned(),
            api_key: Some(key),
            model,
        }),
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
        let line = match &cfg.provider {
            Some(provider) if last_llm.is_none_or(|t| t.elapsed() >= cfg.min_llm_gap) => {
                let provider = provider.clone();
                let name = cfg.name.clone();
                let timeout = cfg.llm_timeout;
                let asked = hytte_plugin::tokio::task::spawn_blocking(move || {
                    ask_llm(&provider, &name, req, timeout)
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

// ── The model path ───────────────────────────────────────────────────────────

/// One blocking chat-completion call (runs on a `spawn_blocking` thread):
/// build the persona/event messages, ask the shared client — giving it up to
/// `timeout` for the whole round trip — then [`sanitize`] the raw reply. An
/// **empty** line is treated as "offline" so the caller falls back to a canned
/// line — a reasoning model that spends its whole budget on hidden reasoning
/// returns nothing, and that shouldn't surface as a blank bubble.
fn ask_llm(
    provider: &Provider,
    name: &str,
    req: ThinkReq,
    timeout: Duration,
) -> Result<String, String> {
    let messages = [
        Message::system(persona(name, req)),
        Message::user(event(name, req)),
    ];
    let opts = ChatOpts {
        max_tokens: 32,
        temperature: 0.9,
        timeout,
    };
    let raw = hytte_ai_providers::chat(provider, &messages, &opts)?;
    let line = sanitize(&raw, name);
    if line.is_empty() {
        Err("model produced an empty line (a reasoning model? run llama-server with --reasoning-budget 0)".to_owned())
    } else {
        Ok(line)
    }
}

/// The pet's standing persona, tuned live against MiniCPM5-1B: short,
/// concrete, format stated as rules. The word budget is [`PROMPT_MAX_WORDS`],
/// which is derived from the [`MAX_LINE`] clamp — asking for more words than
/// fit only guarantees [`sanitize`] truncates the answer (#700).
fn persona(name: &str, req: ThinkReq) -> String {
    format!(
        "You are {name}, a tiny cat who lives in the sidebar of Annika's \
         Linux desktop. It is around {hour}:00 and you feel {mood}. Always \
         answer as {name} the cat. Style: playful, a little sassy. Format: \
         exactly one line, at most {words} words, plain text, no quotes, \
         no emoji.",
        hour = req.hour,
        mood = req.mood,
        words = PROMPT_MAX_WORDS,
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

    /// Working estimate of one English word plus its trailing space, used to
    /// keep [`PROMPT_MAX_WORDS`] and [`MAX_LINE`] honest with each other.
    const AVG_WORD_LEN: usize = 6;

    /// The two env knobs (#697 gap, #699 timeout): the compiled defaults, and
    /// that only a sane positive whole-seconds override displaces them.
    #[test]
    fn secs_or_pins_defaults_and_parses_overrides() {
        // Defaults: unset, blank, unparsable, negative, or zero → compiled value.
        assert_eq!(secs_or(None, MIN_LLM_GAP), Duration::from_secs(15));
        assert_eq!(secs_or(Some(""), MIN_LLM_GAP), MIN_LLM_GAP);
        assert_eq!(secs_or(Some("   "), MIN_LLM_GAP), MIN_LLM_GAP);
        assert_eq!(secs_or(Some("soon"), MIN_LLM_GAP), MIN_LLM_GAP);
        assert_eq!(secs_or(Some("-5"), MIN_LLM_GAP), MIN_LLM_GAP);
        assert_eq!(secs_or(Some("2.5"), MIN_LLM_GAP), MIN_LLM_GAP);
        assert_eq!(
            secs_or(Some("0"), MIN_LLM_GAP),
            MIN_LLM_GAP,
            "0 is nonsense for both knobs → the default, not an instant timeout",
        );
        // Overrides parse, whitespace and all.
        assert_eq!(secs_or(Some("45"), MIN_LLM_GAP), Duration::from_secs(45));
        assert_eq!(secs_or(Some(" 45 "), MIN_LLM_GAP), Duration::from_secs(45));
        // The timeout knob shares the parse and defaults to the client's own
        // budget — the value hytte-claude-bridge's budget is ordered against.
        assert_eq!(
            secs_or(None, hytte_ai_providers::DEFAULT_TIMEOUT),
            Duration::from_secs(10),
        );
        assert_eq!(
            secs_or(Some("30"), hytte_ai_providers::DEFAULT_TIMEOUT),
            Duration::from_secs(30),
        );
    }

    /// #700: the persona's word budget has to fit the [`MAX_LINE`] clamp, or
    /// an obedient model's reply is truncated with an ellipsis every time.
    #[test]
    fn prompt_word_budget_fits_the_line_clamp() {
        let prompt = persona(
            "nisse",
            ThinkReq {
                kind: ThinkKind::Idle,
                hour: 9,
                mood: "happy",
                pokes: 0,
            },
        );
        assert!(
            prompt.contains(&format!("at most {PROMPT_MAX_WORDS} words")),
            "the prompt states the derived budget: {prompt}",
        );
        // A reply that obeys the prompt survives sanitize intact; the 8 words
        // the prompt used to ask for did not — that was the bug.
        let words = |n: usize| vec!["a".repeat(AVG_WORD_LEN - 1); n].join(" ");
        let obedient = words(PROMPT_MAX_WORDS);
        assert!(
            obedient.chars().count() <= MAX_LINE,
            "{PROMPT_MAX_WORDS} words at ~{AVG_WORD_LEN} chars is {} — over the {MAX_LINE}-char bubble",
            obedient.chars().count(),
        );
        assert_eq!(sanitize(&obedient, "nisse"), obedient, "no ellipsis");
        assert!(
            sanitize(&words(8), "nisse").ends_with('…'),
            "8 words is what #700 reported as always truncated",
        );
    }

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

    #[test]
    fn resolve_provider_picks_the_backend() {
        // Explicitly empty URL → model disabled (canned-only pet).
        assert!(resolve_provider(Some(""), None, None).is_none());
        // Explicit URL, no key → that base, no auth; model layered when set.
        let p = resolve_provider(Some("http://host:1"), None, Some("m".to_owned())).unwrap();
        assert_eq!(p.base_url, "http://host:1");
        assert!(p.api_key.is_none());
        assert_eq!(p.model.as_deref(), Some("m"));
        // No URL + a key → OpenRouter (the default), key + model layered on.
        let p = resolve_provider(None, Some("sk-1".to_owned()), Some("gpt".to_owned())).unwrap();
        assert_eq!(p.base_url, "https://openrouter.ai/api");
        assert_eq!(p.api_key.as_deref(), Some("sk-1"));
        assert_eq!(p.model.as_deref(), Some("gpt"));
        // No URL, no key → canned-only (#438): a keyless OpenRouter default would
        // 401 on every call, so short-circuit to `None` instead of a doomed
        // round-trip. (Local llama-server stays opt-in via $PET_LLM_URL.)
        assert!(
            resolve_provider(None, None, None).is_none(),
            "keyless + URL-less resolves to canned-only, not a doomed OpenRouter call",
        );
        // …but a model set without a key still can't authenticate → canned-only.
        assert!(
            resolve_provider(None, None, Some("gpt".to_owned())).is_none(),
            "a model without a key can't authenticate the cloud default either",
        );
        // Explicit URL + key (a self-hosted keyed endpoint) → URL wins, key layered.
        let p = resolve_provider(Some("http://host:2"), Some("k".to_owned()), None).unwrap();
        assert_eq!(p.base_url, "http://host:2");
        assert_eq!(p.api_key.as_deref(), Some("k"));
    }

    /// A one-shot fake OpenAI-compatible server that answers with `body`.
    fn spawn_fake(body: &'static str) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len(),
            );
            sock.write_all(resp.as_bytes()).expect("write");
        });
        (format!("http://{addr}"), handle)
    }

    /// End-to-end of the model path: a fake server answers one chat
    /// completion; `ask_llm` parses it and [`sanitize`]s the content.
    #[test]
    fn ask_llm_parses_and_sanitizes() {
        let (base, server) = spawn_fake(
            r#"{"choices":[{"message":{"role":"assistant","content":"\"purring at your service\"\n"}}]}"#,
        );
        let line = ask_llm(
            &Provider::llama(base),
            "nisse",
            ThinkReq {
                kind: ThinkKind::Idle,
                hour: 15,
                mood: "happy",
                pokes: 0,
            },
            hytte_ai_providers::DEFAULT_TIMEOUT,
        )
        .expect("parses + sanitizes the completion");
        assert_eq!(line, "purring at your service");
        server.join().expect("server thread");
    }

    /// A blank reply (e.g. a reasoning model that returned nothing) is the
    /// pet's "offline" signal → the brain uses a canned line.
    #[test]
    fn ask_llm_blank_reply_is_offline() {
        let (base, server) = spawn_fake(r#"{"choices":[{"message":{"content":"   \n"}}]}"#);
        let err = ask_llm(
            &Provider::llama(base),
            "nisse",
            ThinkReq {
                kind: ThinkKind::Poke,
                hour: 15,
                mood: "happy",
                pokes: 1,
            },
            hytte_ai_providers::DEFAULT_TIMEOUT,
        )
        .expect_err("a blank line is treated as offline");
        assert!(err.contains("empty line"), "{err}");
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
            &Provider::llama(format!("http://{addr}")),
            "nisse",
            ThinkReq {
                kind: ThinkKind::Poke,
                hour: 15,
                mood: "happy",
                pokes: 1,
            },
            hytte_ai_providers::DEFAULT_TIMEOUT,
        )
        .expect_err("nothing listens there");
        assert!(err.starts_with("http:"), "{err}");
    }

    /// #699: the resolved `$PET_LLM_TIMEOUT_SECS` really reaches the HTTP
    /// client — a server that accepts and never answers must be abandoned on
    /// the pet's budget, not the shared client's 10s default.
    #[test]
    fn ask_llm_passes_the_timeout_through_to_the_client() {
        // Bound but never accepted: the handshake completes from the kernel
        // backlog, so this stalls on the read, not the connect.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let started = std::time::Instant::now();
        let err = ask_llm(
            &Provider::llama(format!("http://{addr}")),
            "nisse",
            ThinkReq {
                kind: ThinkKind::Poke,
                hour: 15,
                mood: "happy",
                pokes: 1,
            },
            Duration::from_millis(200),
        )
        .expect_err("the server never answers");
        assert!(err.starts_with("http:"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "gave up after {:?} — the 200ms budget wasn't applied",
            started.elapsed(),
        );
        drop(listener);
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
        let provider = Provider::llama(base);

        let poke = ask_llm(
            &provider,
            "nisse",
            ThinkReq {
                kind: ThinkKind::Poke,
                hour: 21,
                mood: "excited",
                pokes: 2,
            },
            hytte_ai_providers::DEFAULT_TIMEOUT,
        )
        .expect("the model answers a poke");
        eprintln!("[live] poke reaction: {poke}");
        assert!(!poke.is_empty() && poke.chars().count() <= MAX_LINE + 1);

        let idle = ask_llm(
            &provider,
            "nisse",
            ThinkReq {
                kind: ThinkKind::Idle,
                hour: 2,
                mood: "sleepy",
                pokes: 0,
            },
            hytte_ai_providers::DEFAULT_TIMEOUT,
        )
        .expect("the model muses");
        eprintln!("[live] idle thought: {idle}");
        assert!(!idle.is_empty() && idle.chars().count() <= MAX_LINE + 1);
    }
}
