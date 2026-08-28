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
//! How the prompts refer to the person at the keyboard is *not* a pet knob:
//! both the persona and the poke stimulus take the desktop owner from the
//! session-wide `$TROLLSHELL_OWNER`, resolved once per session through the
//! shared [`hytte_ai_providers::owner`] (neutral fallback, never a guess from
//! `$USER`) — the same resolver `caw` reads (#696/#706).
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
    /// `$TROLLSHELL_OWNER` — how the persona and the poke stimulus refer to
    /// whoever is running the shell, resolved through the shared
    /// [`hytte_ai_providers::owner`] (so `caw` reads the same variable, with
    /// the same neutral [`hytte_ai_providers::DEFAULT_OWNER`] fallback).
    /// Resolved **once** per session here rather than per prompt.
    owner: String,
    /// `$PET_LLM_MIN_GAP_SECS` — throttle between two real model calls.
    /// Default: [`MIN_LLM_GAP`].
    min_llm_gap: Duration,
    /// `$PET_LLM_TIMEOUT_SECS` — how long one model call may take before the
    /// client hangs up. Default: [`hytte_ai_providers::DEFAULT_TIMEOUT`].
    llm_timeout: Duration,
    /// `$PET_PERSONA` — a style/tone clause spliced into [`persona`]'s
    /// `Style:` line, replacing the default `"playful, a little sassy"`.
    /// The `{name}` interpolation and the trailing `Format:` rules
    /// (including [`PROMPT_MAX_WORDS`]) are **not** overridable through this
    /// knob — see [`persona`]'s doc comment for why. Default: `None` (the
    /// built-in style clause). Unset, empty, or whitespace-only all resolve
    /// to `None`.
    persona: Option<String>,
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
        let owner = hytte_ai_providers::owner();
        let min_llm_gap = secs_or(
            std::env::var("PET_LLM_MIN_GAP_SECS").ok().as_deref(),
            MIN_LLM_GAP,
        );
        let llm_timeout = secs_or(
            std::env::var("PET_LLM_TIMEOUT_SECS").ok().as_deref(),
            hytte_ai_providers::DEFAULT_TIMEOUT,
        );
        let persona = persona_or_none(std::env::var("PET_PERSONA").ok().as_deref());
        Self {
            provider,
            name,
            owner,
            min_llm_gap,
            llm_timeout,
            persona,
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

/// Resolve `$PET_PERSONA` into the spliced style clause, or `None` when
/// unset, empty, or whitespace-only (same "blank is unset" treatment as
/// [`secs_or`], split out for the same unit-testability reason). The
/// returned string is trimmed, since it's spliced directly into a sentence.
fn persona_or_none(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// The identity the pet sends as `OpenAI`'s `user` (#704), and the id it
/// registers under. One constant for both on purpose: the property the bridge
/// needs is that this string is stable across restarts and distinct from every
/// other caller's, and the manifest id is already exactly that.
///
/// It reaches `hytte-claude-bridge` as the conversation handle, so the pet's
/// session can never be the one caw is talking in — see [`Provider::user`].
/// Endpoints that are not the bridge treat it as the abuse-tracking hint the
/// spec describes, or ignore it.
///
/// **Must stay distinct from `hytte_plugin_caw::briefing::PLUGIN_ID`.** Two
/// plugins sharing an identity share one `claude` session, which is precisely
/// the cross-caller bleed the identity exists to prevent. No test can enforce
/// that: the two consts live in separate crates that do not depend on each
/// other, so each side can only assert against a hardcoded copy of the other's
/// value. Changing either one is a two-file change.
///
/// Changing it also orphans the pet's existing on-disk session — the bridge's
/// title is a digest of exactly this string.
pub const PLUGIN_ID: &str = "pet";

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
            user: Some(PLUGIN_ID.to_owned()),
        }),
        // No URL → the OpenRouter cloud default, but ONLY with a key. Keyless →
        // `None` (canned-only): the call would just 401, so skip it (#438).
        None => key.map(|key| Provider {
            base_url: "https://openrouter.ai/api".to_owned(),
            api_key: Some(key),
            model,
            user: Some(PLUGIN_ID.to_owned()),
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
                let owner = cfg.owner.clone();
                let timeout = cfg.llm_timeout;
                let persona_style = cfg.persona.clone();
                let asked = hytte_plugin::tokio::task::spawn_blocking(move || {
                    ask_llm(
                        &provider,
                        &name,
                        &owner,
                        req,
                        timeout,
                        persona_style.as_deref(),
                    )
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
    owner: &str,
    req: ThinkReq,
    timeout: Duration,
    persona_style: Option<&str>,
) -> Result<String, String> {
    let messages = prompt_messages(name, owner, req, persona_style);
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

/// The exact pair of messages one request carries: the standing
/// [`persona`] as the system message, the current [`event`] as the user one.
///
/// Split out of [`ask_llm`] so a test can assert on the pair a request would
/// actually send — specifically that the **system** half does not move when
/// the hour or the mood does, which is the property a resumed bridge session
/// depends on (see [`persona`]).
fn prompt_messages(
    name: &str,
    owner: &str,
    req: ThinkReq,
    persona_style: Option<&str>,
) -> [Message; 2] {
    [
        Message::system(persona(name, owner, persona_style)),
        Message::user(event(name, owner, req)),
    ]
}

/// The pet's standing persona, tuned live against MiniCPM5-1B: short,
/// concrete, format stated as rules. The word budget is [`PROMPT_MAX_WORDS`],
/// which is derived from the [`MAX_LINE`] clamp — asking for more words than
/// fit only guarantees [`sanitize`] truncates the answer (#700).
///
/// `style` is `$PET_PERSONA` (see [`Cfg::persona`]) — it splices in as the
/// `Style:` clause only, replacing the default `"playful, a little sassy"`.
/// The `{name}` interpolation and the trailing `Format:` rules stay fixed
/// regardless of `style`: they're what keeps a reply inside
/// [`PROMPT_MAX_WORDS`], and a persona that could drop them would silently
/// produce lines [`sanitize`] then chops mid-word (#698's decision — a
/// full-template override was considered and rejected for exactly this).
///
/// `owner` is the desktop owner, resolved once per session from
/// `$TROLLSHELL_OWNER` via the shared [`hytte_ai_providers::owner`] (see
/// [`Cfg::owner`]). It used to be hardcoded here as `"Annika's Linux
/// desktop"` — not every deployment's owner is named Annika (#696).
///
/// **Nothing time-varying may live here.** The hour and the mood used to be
/// interpolated into this string, and that was load-bearing by accident: it
/// changed the system message every tick, which changed the conversation key
/// `hytte-claude-bridge` derives from the prefix, which minted a fresh session
/// and re-sent the whole transcript. Once a caller names its own session
/// (#704, [`PLUGIN_ID`]) the key is constant, so every turn after the first
/// **resumes** and sends only the newest message — the system message is
/// whatever the session was created with, forever. Anything the model must be
/// told *again each turn* belongs in [`event`], which is the message that is
/// actually re-sent. The same holds for the bridge's reprompt backend, whose
/// `Records::compose` head-pins the first system message it ever recorded.
fn persona(name: &str, owner: &str, style: Option<&str>) -> String {
    let style = style.unwrap_or("playful, a little sassy");
    format!(
        "You are {name}, a tiny cat who lives in the sidebar of {owner}'s Linux \
         desktop. Always answer as {name} the cat. Style: {style}. Format: \
         exactly one line, at most {PROMPT_MAX_WORDS} words, plain text, no \
         quotes, no emoji."
    )
}

/// The stimulus, phrased so a tiny model can't just parrot an instruction,
/// with the format re-anchored at the end (1B models follow the tail best).
///
/// This is the **user** message on every request, so the poke branch is the
/// other half of #696: below [`GRUMPY_AT`] it names the person doing the
/// poking, and it named a hardcoded `"Annika"` until the shared
/// [`hytte_ai_providers::owner`] resolver landed. Above [`GRUMPY_AT`] the
/// stimulus counts pokes instead and mentions nobody — the bug only ever
/// showed on the first few clicks.
///
/// Being the message that is re-sent every turn is also why the **hour and the
/// mood** open it. They used to sit in [`persona`], where a resumed session
/// freezes them at whatever they were when it was created (see that function's
/// note); here they are the scene the stimulus happens in, restated on every
/// request, so `mood` — a headline feature (#284) — actually reaches the model
/// each tick. They lead rather than trail because the format anchor has to keep
/// the tail.
fn event(name: &str, owner: &str, req: ThinkReq) -> String {
    let stim = match req.kind {
        ThinkKind::Poke if req.pokes >= GRUMPY_AT => format!("*poke #{} in a row*", req.pokes),
        ThinkKind::Poke => format!("*{owner} pokes you*"),
        ThinkKind::Idle if req.mood == "sleepy" => "(late night, everything is quiet)".to_owned(),
        ThinkKind::Idle => "(a quiet moment; share one tiny cat thought)".to_owned(),
    };
    format!(
        "It is around {hour}:00 and you feel {mood}. {stim} — say your one \
         line now, as {name}:",
        hour = req.hour,
        mood = req.mood,
    )
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
    use hytte_ai_providers::DEFAULT_OWNER;
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

    /// #698: `$PET_PERSONA` resolution — unset/empty/whitespace-only all
    /// mean "no override", and a real value comes back trimmed.
    #[test]
    fn persona_or_none_treats_blank_as_unset() {
        assert_eq!(persona_or_none(None), None);
        assert_eq!(persona_or_none(Some("")), None);
        assert_eq!(persona_or_none(Some("   ")), None);
        assert_eq!(persona_or_none(Some("\t\n")), None);
        assert_eq!(
            persona_or_none(Some("gentle and philosophical")),
            Some("gentle and philosophical".to_owned()),
        );
        assert_eq!(
            persona_or_none(Some("  grumpy and terse  ")),
            Some("grumpy and terse".to_owned()),
            "spliced into a sentence, so the value is trimmed",
        );
    }

    /// #698: with no override the prompt keeps the built-in style clause;
    /// with one set, that clause — and only that clause — changes. The
    /// `{name}` interpolation and the `Format:` rules (including the derived
    /// [`PROMPT_MAX_WORDS`]) must survive either way.
    #[test]
    fn persona_splices_style_but_keeps_format_rules_fixed() {
        let default_prompt = persona("nisse", DEFAULT_OWNER, None);
        assert!(default_prompt.contains("Style: playful, a little sassy."));

        let custom_prompt = persona("nisse", DEFAULT_OWNER, Some("gentle and philosophical"));
        assert!(custom_prompt.contains("Style: gentle and philosophical."));
        assert!(
            !custom_prompt.contains("playful, a little sassy"),
            "the override replaces the default clause, it doesn't append to it",
        );

        // Everything outside the `Style:` clause is identical — in
        // particular the format rules a custom persona cannot touch.
        let format_rules = format!(
            "Format: exactly one line, at most {PROMPT_MAX_WORDS} words, \
             plain text, no quotes, no emoji."
        );
        assert!(default_prompt.contains(&format_rules));
        assert!(custom_prompt.contains(&format_rules));
        assert!(default_prompt.contains("You are nisse, a tiny cat"));
        assert!(custom_prompt.contains("You are nisse, a tiny cat"));

        // #696: the default no longer names a specific owner.
        assert!(
            !default_prompt.contains("Annika"),
            "the default persona must not hardcode an owner name: {default_prompt}",
        );
    }

    /// #696, persona half: the owner possessive comes from the resolved owner,
    /// and the neutral default names nobody in particular.
    #[test]
    fn persona_embeds_the_resolved_owner_and_never_a_hardcoded_name() {
        assert!(persona("nisse", "kaesaecracker", None).contains("kaesaecracker's Linux desktop"));
        let neutral = persona("nisse", DEFAULT_OWNER, None);
        assert!(neutral.contains("your human's Linux desktop"), "{neutral}");
        assert!(
            !neutral.to_lowercase().contains("annika"),
            "the default persona must not name a specific person: {neutral}",
        );
    }

    /// **The freeze regression.** The system message must not move when the
    /// hour or the mood does; the user message must.
    ///
    /// Since #704 the pet names its own session ([`PLUGIN_ID`]), so
    /// `hytte-claude-bridge` derives a constant title for it and every turn
    /// after the first **resumes**, re-sending only the newest message. A
    /// system message that carried `{hour}`/`{mood}` would therefore be frozen
    /// at session-creation time forever — start the bridge at 03:00 and the pet
    /// still believes it is 03:00 and sleepy next week. (Before #704 it worked
    /// only by accident: the changing system message churned the content hash,
    /// which minted a new session and re-rendered the transcript.) The same
    /// applies to the bridge's reprompt backend, whose `Records::compose`
    /// head-pins the first system message it recorded.
    ///
    /// This test fails the moment either value migrates back up.
    #[test]
    fn only_the_user_message_moves_with_the_hour_and_the_mood() {
        let at = |hour, mood| ThinkReq {
            kind: ThinkKind::Idle,
            hour,
            mood,
            pokes: 0,
        };
        let three_am = prompt_messages("nisse", "kaesaecracker", at(3, "sleepy"), None);
        let noon = prompt_messages("nisse", "kaesaecracker", at(12, "happy"), None);

        assert_eq!(three_am[0].role, "system");
        assert_eq!(three_am[1].role, "user");
        assert_eq!(
            three_am[0].content, noon[0].content,
            "the system message must be byte-identical across hours and moods — \
             a resumed session only ever hears the one it was created with",
        );
        assert_ne!(
            three_am[1].content, noon[1].content,
            "the user message is the only half re-sent on a resume, so it is \
             where the time-varying state has to live",
        );

        // Named explicitly, so a partial migration back is caught too.
        for frozen in ["3:00", "12:00", "sleepy", "happy"] {
            assert!(
                !three_am[0].content.contains(frozen) && !noon[0].content.contains(frozen),
                "the persona still carries {frozen:?}: {}",
                three_am[0].content,
            );
        }
        assert!(
            three_am[1].content.contains("3:00"),
            "{}",
            three_am[1].content
        );
        assert!(
            three_am[1].content.contains("sleepy"),
            "{}",
            three_am[1].content
        );
        assert!(noon[1].content.contains("12:00"), "{}", noon[1].content);
        assert!(noon[1].content.contains("happy"), "{}", noon[1].content);
    }

    /// #696, the half [`persona`]'s test could never have caught: the poke
    /// stimulus is the **user** message on every request, and below
    /// [`GRUMPY_AT`] it hardcoded `"*Annika pokes you*"` — so a configured
    /// brain was told the wrong person's name on every early click. It now
    /// carries the resolved owner; the grumpy branch counts pokes and names
    /// nobody, and neither idle branch mentions an owner at all.
    #[test]
    fn event_poke_names_the_resolved_owner_and_never_a_hardcoded_name() {
        let poke = |pokes| ThinkReq {
            kind: ThinkKind::Poke,
            hour: 14,
            mood: "happy",
            pokes,
        };

        let named = event("nisse", "kaesaecracker", poke(1));
        assert!(
            named.contains("*kaesaecracker pokes you*"),
            "the poker is the resolved owner: {named}",
        );
        let neutral = event("nisse", DEFAULT_OWNER, poke(1));
        assert!(
            neutral.contains("*your human pokes you*"),
            "unset `$TROLLSHELL_OWNER` → the neutral phrase: {neutral}",
        );
        assert!(
            !neutral.to_lowercase().contains("annika"),
            "the poke stimulus must not hardcode an owner name: {neutral}",
        );
        // The tail anchor the prompt relies on survives either way.
        assert!(
            named.ends_with("— say your one line now, as nisse:"),
            "{named}"
        );

        // Grumpy branch: pokes are counted, nobody is named.
        let grumpy = event("nisse", "kaesaecracker", poke(GRUMPY_AT));
        assert!(
            grumpy.contains(&format!("*poke #{GRUMPY_AT} in a row*")),
            "{grumpy}",
        );
        assert!(
            !grumpy.contains("kaesaecracker"),
            "the grumpy stimulus names nobody: {grumpy}",
        );

        // Idle branches carry no owner mention either.
        for mood in ["sleepy", "happy"] {
            let idle = event(
                "nisse",
                "kaesaecracker",
                ThinkReq {
                    kind: ThinkKind::Idle,
                    hour: 2,
                    mood,
                    pokes: 0,
                },
            );
            assert!(!idle.contains("kaesaecracker pokes"), "{idle}");
        }
    }

    /// #700: the persona's word budget has to fit the [`MAX_LINE`] clamp, or
    /// an obedient model's reply is truncated with an ellipsis every time.
    #[test]
    fn prompt_word_budget_fits_the_line_clamp() {
        let prompt = persona("nisse", DEFAULT_OWNER, None);
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

    /// Whichever backend it resolves to, the pet names itself (#704) — the
    /// bridge keys the conversation on this, so a provider that lost it would
    /// silently fall back to sharing a session by transcript luck.
    #[test]
    fn every_resolved_provider_carries_the_plugin_identity() {
        for p in [
            resolve_provider(Some("http://host:1"), None, None),
            resolve_provider(None, Some("sk-1".to_owned()), None),
        ] {
            assert_eq!(
                p.expect("resolves").user.as_deref(),
                Some(PLUGIN_ID),
                "the provider must carry the pet's identity",
            );
        }
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
            "kaesaecracker",
            ThinkReq {
                kind: ThinkKind::Idle,
                hour: 15,
                mood: "happy",
                pokes: 0,
            },
            hytte_ai_providers::DEFAULT_TIMEOUT,
            None,
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
            "kaesaecracker",
            ThinkReq {
                kind: ThinkKind::Poke,
                hour: 15,
                mood: "happy",
                pokes: 1,
            },
            hytte_ai_providers::DEFAULT_TIMEOUT,
            None,
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
            "kaesaecracker",
            ThinkReq {
                kind: ThinkKind::Poke,
                hour: 15,
                mood: "happy",
                pokes: 1,
            },
            hytte_ai_providers::DEFAULT_TIMEOUT,
            None,
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
            "kaesaecracker",
            ThinkReq {
                kind: ThinkKind::Poke,
                hour: 15,
                mood: "happy",
                pokes: 1,
            },
            Duration::from_millis(200),
            None,
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
        let owner = hytte_ai_providers::owner();

        let poke = ask_llm(
            &provider,
            "nisse",
            &owner,
            ThinkReq {
                kind: ThinkKind::Poke,
                hour: 21,
                mood: "excited",
                pokes: 2,
            },
            hytte_ai_providers::DEFAULT_TIMEOUT,
            None,
        )
        .expect("the model answers a poke");
        eprintln!("[live] poke reaction: {poke}");
        assert!(!poke.is_empty() && poke.chars().count() <= MAX_LINE + 1);

        let idle = ask_llm(
            &provider,
            "nisse",
            &owner,
            ThinkReq {
                kind: ThinkKind::Idle,
                hour: 2,
                mood: "sleepy",
                pokes: 0,
            },
            hytte_ai_providers::DEFAULT_TIMEOUT,
            None,
        )
        .expect("the model muses");
        eprintln!("[live] idle thought: {idle}");
        assert!(!idle.is_empty() && idle.chars().count() <= MAX_LINE + 1);
    }
}
