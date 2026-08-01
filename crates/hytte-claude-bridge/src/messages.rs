//! The Anthropic **Messages API** client — #584's other half (#730).
//!
//! #584 scoped two backends. Path 1, the subscription route through
//! `hive-claude`, shipped in #666 and is what [`crate::backend::Subscription`]
//! runs. Path 2 — this module — is the lightweight client for somebody who
//! would rather pay per token with an API key than route through a Claude Code
//! subscription. It lands *inside* [`crate::backend::Reprompt`], exactly where
//! `backend.rs` said it would: that path already keeps the conversation in the
//! bridge's own record and re-prompts with the whole thing each turn, which is
//! the stateless shape `POST /v1/messages` wants.
//!
//! # It changes nothing above it
//!
//! The bridge still serves one loopback route, `POST /v1/chat/completions`, and
//! still answers the exact single-choice envelope `hytte_ai_providers::chat`
//! parses. Pet and caw consume the bridge purely as a `Provider` base URL, so
//! **neither needs a code change** — this backend is picked with
//! `CLAUDE_BRIDGE_MODE=api` on the bridge's own unit and is invisible to them.
//!
//! # Why the translation is not symmetric
//!
//! The `OpenAI` shape and the Messages shape disagree in three places, and each
//! disagreement is a decision rather than a mapping:
//!
//! - **`system`** is a *role* in the `OpenAI` transcript and a *top-level
//!   parameter* in the Messages API. Every `system` turn is lifted out and
//!   joined; what remains is the `messages` array.
//! - **`max_tokens`** is optional for `OpenAI` and **required** here, so the
//!   client's value is honoured when it sends one and [`DEFAULT_MAX_TOKENS`]
//!   stands in when it does not. This is the one knob the CLI paths could only
//!   ever approximate (see the crate docs) and that this path enforces for real.
//! - **`temperature` is never forwarded.** Not laziness: `temperature`, `top_p`
//!   and `top_k` are *rejected with a 400* on the current models (Opus 5,
//!   Opus 4.8/4.7, Sonnet 5, Fable 5). Passing the client's value through would
//!   turn every pet tick into a bad request.
//!
//! # Thinking is off by default, and that is load-bearing
//!
//! `max_tokens` bounds thinking **plus** answer text. The consumers here ask
//! for ~256 tokens of kaomoji quip, and thinking is *on by default* on Claude
//! Opus 5 — so a request that left it alone would spend the whole budget
//! reasoning and return an empty answer. The bridge therefore sends
//! `thinking: {"type": "disabled"}` unless told otherwise
//! (`CLAUDE_BRIDGE_THINKING`, see [`Thinking`]).
//!
//! # The key
//!
//! Loaded exactly the way the rest of the workspace loads keys — see
//! [`load_key`]. The bridge **refuses to start** in this mode without one
//! rather than binding a port it cannot serve; `main.rs` owns that check.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::backend::{MAX_ERROR_CHARS, truncate};
use crate::http::Failure;
use crate::session::OVERFLOW_STATUS;
use crate::wire::Message;

/// The one endpoint this client speaks.
const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

/// The `anthropic-version` header every request carries. Pinned rather than
/// tracked: the request and response shapes this module builds are the ones
/// this version defines.
pub const API_VERSION: &str = "2023-06-01";

/// The model used when `$CLAUDE_BRIDGE_MODEL` is unset.
///
/// Unlike the CLI paths — where an empty model means "whatever the Claude Code
/// CLI defaults to" — `POST /v1/messages` requires an explicit `model`, so this
/// path needs a concrete id.
pub const DEFAULT_MODEL: &str = "claude-opus-5";

/// `max_tokens` when the client sends none. The Messages API requires the
/// field; `hytte_ai_providers::chat` always sends one, so this is a backstop
/// for a different `OpenAI` client rather than the normal path.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Where a human is told to put the key when the API rejects theirs.
const KEY_PATH_HINT: &str = "~/.config/trollshell/anthropic.key";

/// TCP handshake budget. Separate from the request budget and deliberately not
/// configurable — this is the connect, never model latency (same split as
/// `hytte_ai_providers::chat`).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Longest error-body excerpt kept when the API's JSON error shape can't be
/// parsed.
const MAX_RAW_BODY_CHARS: usize = 200;

/// What to ask the model to do about extended thinking.
///
/// Set with `$CLAUDE_BRIDGE_THINKING`. The default is deliberate — see the
/// module docs — but it is not universally accepted, hence the other two arms:
/// Claude Fable 5 **rejects** an explicit `disabled` (thinking is always on
/// there), and on Claude Opus 5 `disabled` is only accepted at effort `high` or
/// below, which is the default this module relies on by never sending
/// `output_config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Thinking {
    /// `thinking: {"type": "disabled"}` — the default, so a small `max_tokens`
    /// buys answer text rather than reasoning.
    #[default]
    Disabled,
    /// `thinking: {"type": "adaptive"}` — the model decides. Needs a
    /// `max_tokens` large enough to hold reasoning *and* an answer.
    Adaptive,
    /// Send no `thinking` field at all. The per-model default applies, which is
    /// what a model that refuses an explicit `disabled` needs.
    Auto,
}

impl Thinking {
    /// Parse `$CLAUDE_BRIDGE_THINKING`. Anything unrecognised falls back to the
    /// default rather than refusing to start — same rule as
    /// `crate::Mode::parse`, and for the same reason.
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some("adaptive") => Self::Adaptive,
            Some("auto") => Self::Auto,
            _ => Self::Disabled,
        }
    }

    /// The `thinking` request field, or `None` for [`Thinking::Auto`].
    fn field(self) -> Option<Value> {
        let kind = match self {
            Self::Disabled => "disabled",
            Self::Adaptive => "adaptive",
            Self::Auto => return None,
        };
        Some(serde_json::json!({ "type": kind }))
    }
}

/// A keyed client for `POST /v1/messages`.
///
/// Blocking `ureq` under `spawn_blocking` — the house idiom (`hytte-services`'
/// weather fetcher, `hytte_ai_providers::chat`, the departures plugin), not a
/// second HTTP stack.
#[derive(Clone)]
pub struct Client {
    key: String,
    model: String,
    thinking: Thinking,
    budget: Duration,
}

// Hand-written so the key cannot reach a log line: `Backend` and `Reprompt`
// both derive `Debug`, and `main.rs` logs the settings at startup.
impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("model", &self.model)
            .field("thinking", &self.thinking)
            .field("budget", &self.budget)
            .field("key", &"<redacted>")
            .finish()
    }
}

impl Client {
    /// A client billing `key`, asking for `model`, bounded by `budget`.
    ///
    /// `budget` is the whole round trip (connect + send + read) and must stay
    /// **under** the bridge's own per-request budget: `spawn_blocking` is not
    /// cancellable, so the outer `tokio::time::timeout` firing first would
    /// leave this request running with nobody left to read it.
    #[must_use]
    pub fn new(key: String, model: String, thinking: Thinking, budget: Duration) -> Self {
        Self {
            key,
            model,
            thinking,
            budget,
        }
    }

    /// Answer one composed transcript.
    pub async fn respond(
        &self,
        messages: &[Message],
        max_tokens: Option<u32>,
    ) -> Result<String, Failure> {
        let body = build_request(&self.model, self.thinking, messages, max_tokens)?;
        let key = self.key.clone();
        let budget = self.budget;
        match tokio::task::spawn_blocking(move || post(&key, budget, &body)).await {
            Ok(Ok((status, text))) => interpret(status, &text),
            Ok(Err(failure)) => Err(failure),
            Err(e) => Err(Failure::new(
                502,
                format!("the Anthropic API request task did not finish: {e}"),
            )),
        }
    }
}

/// POST `body` and return the status plus the raw response text.
///
/// Errors here are transport-level only; a non-2xx is a perfectly good
/// `(status, text)` that [`map_status`] turns into a [`Failure`] with the
/// endpoint's own explanation in it.
fn post(key: &str, budget: Duration, body: &Value) -> Result<(u16, String), Failure> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(budget))
        // Read the endpoint's JSON error body rather than collapsing a 4xx into
        // a bare status — the message is what a human needs to fix the key or
        // the model id.
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .post(ENDPOINT)
        .header("x-api-key", key)
        .header("anthropic-version", API_VERSION)
        .send_json(body)
        .map_err(|e| {
            Failure::new(
                502,
                truncate(
                    &format!("could not reach the Anthropic API: {e}"),
                    MAX_ERROR_CHARS,
                ),
            )
        })?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    Ok((status, text))
}

/// Turn one raw HTTP outcome into an answer or a [`Failure`].
fn interpret(status: u16, text: &str) -> Result<String, Failure> {
    if (200..300).contains(&status) {
        parse_response(text)
    } else {
        Err(map_status(status, text))
    }
}

// ── Request: OpenAI transcript → Messages API body ───────────────────────────

/// Build the `POST /v1/messages` body for a composed transcript.
///
/// Pure, so the whole translation is testable without a key or a socket.
pub fn build_request(
    model: &str,
    thinking: Thinking,
    messages: &[Message],
    max_tokens: Option<u32>,
) -> Result<Value, Failure> {
    let (system, turns) = translate(messages);
    if turns.is_empty() {
        return Err(Failure::new(
            400,
            "the transcript carried no user or assistant turn to send",
        ));
    }
    let mut body = serde_json::Map::new();
    body.insert("model".to_owned(), Value::from(model));
    body.insert(
        "max_tokens".to_owned(),
        Value::from(max_tokens.filter(|m| *m > 0).unwrap_or(DEFAULT_MAX_TOKENS)),
    );
    if let Some(field) = thinking.field() {
        body.insert("thinking".to_owned(), field);
    }
    if let Some(system) = system {
        body.insert("system".to_owned(), Value::from(system));
    }
    body.insert(
        "messages".to_owned(),
        Value::Array(
            turns
                .into_iter()
                .map(|(role, content)| serde_json::json!({"role": role, "content": content}))
                .collect(),
        ),
    );
    Ok(Value::Object(body))
}

/// Split an `OpenAI` transcript into `(system, turns)`.
///
/// Three passes, each of which is one rule:
///
/// 1. **Lift and label.** `system` turns leave the array entirely; `user` and
///    `assistant` keep their role; any other role (a `tool` turn from a
///    different client, say) rides as a `user` turn labelled `[role]`, the same
///    convention [`crate::session::render_transcript`] uses so the CLI and API
///    paths read a foreign role identically. Empty content is dropped — the API
///    rejects an empty text block.
/// 2. **Fix the head.** The Messages API requires the first turn to be `user`;
///    a leading `assistant` is relabelled rather than dropped, so no content is
///    lost to a shape rule.
/// 3. **Merge runs.** Consecutive same-role turns are joined with a blank line.
fn translate(messages: &[Message]) -> (Option<String>, Vec<(&'static str, String)>) {
    let mut system: Vec<&str> = Vec::new();
    let mut turns: Vec<(&'static str, String)> = Vec::new();
    for m in messages {
        let content = m.content.trim();
        if content.is_empty() {
            continue;
        }
        match m.role.as_str() {
            "system" => system.push(content),
            "assistant" => turns.push(("assistant", content.to_owned())),
            "user" => turns.push(("user", content.to_owned())),
            other => turns.push(("user", format!("[{other}]\n{content}"))),
        }
    }

    if let Some((role, content)) = turns.first_mut()
        && *role == "assistant"
    {
        let relabelled = format!("[assistant]\n{content}");
        *role = "user";
        *content = relabelled;
    }

    let mut merged: Vec<(&'static str, String)> = Vec::with_capacity(turns.len());
    for (role, content) in turns {
        match merged.last_mut() {
            Some((last_role, last_content)) if *last_role == role => {
                last_content.push_str("\n\n");
                last_content.push_str(&content);
            }
            _ => merged.push((role, content)),
        }
    }

    let system = (!system.is_empty()).then(|| system.join("\n\n"));
    (system, merged)
}

// ── Response: Messages API body → the text the envelope carries ──────────────

/// The subset of the response this bridge reads. Unknown fields are ignored, so
/// a response carrying `usage`, `container`, or anything else still parses.
#[derive(Debug, Deserialize)]
struct ApiResponse {
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
}

/// One content block. Kept untyped-by-`type` on purpose: `thinking`,
/// `redacted_thinking` and every future block kind must be ignored rather than
/// rejected, and only `text` carries an answer.
#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

/// The API's error envelope.
#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    error: ApiErrorDetail,
}

/// The `error` object inside [`ApiErrorBody`].
#[derive(Debug, Deserialize)]
struct ApiErrorDetail {
    message: String,
}

/// The answer text out of a 2xx body, or a [`Failure`] explaining why there
/// isn't one.
pub fn parse_response(text: &str) -> Result<String, Failure> {
    let parsed: ApiResponse = serde_json::from_str(text).map_err(|e| {
        Failure::new(
            502,
            format!("the Anthropic API returned a body the bridge could not read: {e}"),
        )
    })?;
    // A refusal is a **200** carrying a `stop_reason` and (usually) no text, so
    // it has to be checked before the content is read — this is the one status
    // that looks like success and is not.
    if parsed.stop_reason.as_deref() == Some("refusal") {
        return Err(Failure::new(
            502,
            "the Anthropic API declined this request (stop_reason: refusal)",
        ));
    }
    if parsed.stop_reason.as_deref() == Some("max_tokens") {
        tracing::debug!("the answer hit max_tokens and is truncated");
    }
    let answer: String = parsed
        .content
        .iter()
        .filter(|b| b.kind == "text")
        .filter_map(|b| b.text.as_deref())
        .collect();
    if answer.trim().is_empty() {
        return Err(Failure::new(502, "the Anthropic API produced no text"));
    }
    Ok(answer)
}

/// Map a non-2xx onto the status the client should see, with the endpoint's own
/// message in the text.
///
/// The statuses line up with [`crate::backend::map_error`]'s so a plugin sees
/// one failure vocabulary whichever backend answered: 429 stays 429, an
/// overflow is [`OVERFLOW_STATUS`], and everything the operator has to fix is a
/// 502 that names the fix.
#[must_use]
pub fn map_status(status: u16, body: &str) -> Failure {
    let detail = api_error_message(body);
    let (mapped, message) = match status {
        // The API has no distinct error *type* for a context overflow — it is
        // an `invalid_request_error` like a dozen unrelated things — so the
        // message is the only signal. Narrow on purpose: on this backend
        // nothing rotates on it (`Reprompt` is not persisted), so the status is
        // a label for the human, not a trigger.
        400 if is_overflow(&detail) => (
            OVERFLOW_STATUS,
            format!("the conversation exceeds the model's context window: {detail}"),
        ),
        400 => (
            400,
            format!("the Anthropic API rejected the request: {detail}"),
        ),
        401 => (
            502,
            format!(
                "the Anthropic API rejected the key — put a working one in {KEY_PATH_HINT} \
                 (or set ANTHROPIC_API_KEY): {detail}"
            ),
        ),
        403 => (
            502,
            format!("this Anthropic API key is not permitted to do that: {detail}"),
        ),
        404 => (
            502,
            format!("no such model — check CLAUDE_BRIDGE_MODEL: {detail}"),
        ),
        413 => (
            OVERFLOW_STATUS,
            format!("the request is too large for the Anthropic API: {detail}"),
        ),
        429 => (
            429,
            format!(
                "the Anthropic API is rate-limiting the bridge, or the credit balance is exhausted: {detail}"
            ),
        ),
        s if s >= 500 => (
            502,
            format!("the Anthropic API is unavailable (HTTP {s}): {detail}"),
        ),
        s => (
            502,
            format!("the Anthropic API answered HTTP {s}: {detail}"),
        ),
    };
    Failure::new(mapped, truncate(&message, MAX_ERROR_CHARS))
}

/// Whether an error message describes a prompt that does not fit.
fn is_overflow(detail: &str) -> bool {
    detail.to_ascii_lowercase().contains("too long")
}

/// The endpoint's own explanation, or a clamped excerpt of whatever it sent.
fn api_error_message(body: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<ApiErrorBody>(body) {
        return parsed.error.message;
    }
    body.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_RAW_BODY_CHARS)
        .collect()
}

// ── The key ──────────────────────────────────────────────────────────────────

/// Load the Anthropic API key.
///
/// Deliberately the **same** shape `hytte_ai_providers::load_key("anthropic")`
/// would produce — `$XDG_CONFIG_HOME/trollshell/anthropic.key` (falling back to
/// `$HOME/.config/trollshell/anthropic.key`), trimmed, empty-is-unset, with an
/// `ANTHROPIC_API_KEY` env override for testing. Mirrored rather than called
/// because this crate deliberately links nothing else in the tree (see the
/// crate docs); the conventions are the contract, not the linkage.
///
/// One consequence worth stating: `ANTHROPIC_API_KEY` is the variable
/// [`crate::envguard`] refuses to start on — because it would silently move the
/// `claude` **child** onto metered credits. In this mode there is no child and
/// metered billing is the whole point, so the guard does not run and the same
/// variable becomes a legitimate override. The key **file** is the primary
/// path: the shipped unit's `UnsetEnvironment=` scrubs the env var.
#[must_use]
pub fn load_key() -> Option<String> {
    load_key_from(std::env::var("ANTHROPIC_API_KEY").ok(), config_dir())
}

/// `$XDG_CONFIG_HOME` (if set and non-empty) else `$HOME/.config`.
fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|x| !x.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
}

/// Core of [`load_key`] with the override and config dir injected, so it is
/// unit-testable without mutating the process environment (which is `unsafe`
/// under edition 2024, and this workspace forbids `unsafe`).
fn load_key_from(env_override: Option<String>, config_dir: Option<PathBuf>) -> Option<String> {
    if let Some(v) = env_override {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }
    let path = config_dir?.join("trollshell").join("anthropic.key");
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// The message printed before exiting when this mode has no key.
///
/// Fail closed, matching [`crate::envguard`]: a bridge that will not start is
/// loud, whereas one that binds 8787 and 502s every request looks like the
/// plugin is broken.
#[must_use]
pub fn missing_key_refusal() -> String {
    format!(
        "refusing to start: CLAUDE_BRIDGE_MODE=api needs an Anthropic API key and there is none.\n\
         Put one in {KEY_PATH_HINT} (or set ANTHROPIC_API_KEY), or drop the mode to run the \
         keyless subscription path instead.\n\
         Binding the port without a key would advertise a backend that answers every request \
         with a 502."
    )
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_MAX_TOKENS, DEFAULT_MODEL, Thinking, build_request, interpret, load_key_from,
        map_status, parse_response,
    };
    use crate::session::OVERFLOW_STATUS;
    use crate::wire::{ChatRequest, ChatResponse, Message};

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.to_owned(),
            content: content.to_owned(),
        }
    }

    fn build(messages: &[Message], max_tokens: Option<u32>) -> serde_json::Value {
        build_request(DEFAULT_MODEL, Thinking::default(), messages, max_tokens).expect("builds")
    }

    // ── request translation ──────────────────────────────────────────────

    /// **The system split.** `system` is a role in the `OpenAI` transcript and a
    /// top-level parameter here; leaving it in `messages` would send the persona
    /// as a user turn and change the character the pet speaks with.
    #[test]
    fn system_turns_are_lifted_out_of_the_messages_array() {
        let body = build(&[msg("system", "you are a cat"), msg("user", "poke")], None);
        assert_eq!(body["system"], "you are a cat");
        assert_eq!(body["messages"].as_array().expect("array").len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "poke");
    }

    /// Several `system` turns join rather than the last one winning — a client
    /// that appends a rule mid-conversation must not silently drop the persona.
    #[test]
    fn multiple_system_turns_are_joined() {
        let body = build(
            &[
                msg("system", "you are a cat"),
                msg("user", "poke"),
                msg("system", "be brief"),
                msg("user", "again"),
            ],
            None,
        );
        assert_eq!(body["system"], "you are a cat\n\nbe brief");
        // …and the two user turns are now adjacent, so they merge.
        assert_eq!(body["messages"].as_array().expect("array").len(), 1);
        assert_eq!(body["messages"][0]["content"], "poke\n\nagain");
    }

    /// A transcript with no `system` turn omits the field rather than sending an
    /// empty string.
    #[test]
    fn a_transcript_without_a_system_turn_omits_the_field() {
        let body = build(&[msg("user", "hi")], None);
        assert!(body.get("system").is_none(), "{body}");
    }

    /// The standard `OpenAI` loop round-trips: roles alternate and survive.
    #[test]
    fn an_alternating_transcript_survives_intact() {
        let body = build(
            &[
                msg("system", "persona"),
                msg("user", "one"),
                msg("assistant", "two"),
                msg("user", "three"),
            ],
            None,
        );
        let turns = body["messages"].as_array().expect("array");
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0]["role"], "user");
        assert_eq!(turns[0]["content"], "one");
        assert_eq!(turns[1]["role"], "assistant");
        assert_eq!(turns[2]["content"], "three");
    }

    /// A role neither the API nor this bridge knows rides as a labelled `user`
    /// turn — the same convention `session::render_transcript` uses, so a
    /// foreign role reads identically whichever backend answered.
    #[test]
    fn an_unknown_role_is_labelled_and_carried_as_a_user_turn() {
        let body = build(&[msg("user", "hi"), msg("tool", "42")], None);
        let turns = body["messages"].as_array().expect("array");
        assert_eq!(turns.len(), 1, "the labelled turn merges into the user run");
        assert_eq!(turns[0]["role"], "user");
        assert_eq!(turns[0]["content"], "hi\n\n[tool]\n42");
    }

    /// The API requires the first turn to be `user`. A leading `assistant` is
    /// relabelled, not dropped — losing content to a shape rule would be worse
    /// than a slightly odd prompt.
    #[test]
    fn a_leading_assistant_turn_is_relabelled_rather_than_dropped() {
        let body = build(&[msg("assistant", "MARKER"), msg("user", "then")], None);
        let turns = body["messages"].as_array().expect("array");
        assert_eq!(turns[0]["role"], "user");
        assert!(
            turns[0]["content"]
                .as_str()
                .expect("string")
                .contains("MARKER"),
            "{turns:?}"
        );
    }

    /// Empty turns are dropped: the API rejects an empty text block, so
    /// forwarding one would 400 the whole request over a stray blank message.
    #[test]
    fn blank_turns_are_dropped() {
        let body = build(
            &[msg("system", "  "), msg("user", "hi"), msg("assistant", "")],
            None,
        );
        assert!(body.get("system").is_none(), "{body}");
        assert_eq!(body["messages"].as_array().expect("array").len(), 1);
    }

    /// A transcript that is nothing but `system` turns has no turn to send. A
    /// clean 400 beats an API-side 400 the client can't act on.
    #[test]
    fn an_all_system_transcript_is_a_bad_request() {
        let err = build_request(
            DEFAULT_MODEL,
            Thinking::default(),
            &[msg("system", "persona")],
            None,
        )
        .expect_err("no turn to send");
        assert_eq!(err.status, 400);
    }

    /// `max_tokens` is **required** by the API. The client's value is honoured
    /// — this is the knob the CLI paths can only approximate.
    #[test]
    fn max_tokens_is_honoured_and_defaulted() {
        let convo = [msg("user", "hi")];
        assert_eq!(build(&convo, Some(256))["max_tokens"], 256);
        assert_eq!(build(&convo, None)["max_tokens"], DEFAULT_MAX_TOKENS);
        // A client asking for zero tokens would be an API-side 400; treat it as
        // "unset" rather than forwarding a guaranteed rejection.
        assert_eq!(build(&convo, Some(0))["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    /// **Thinking is off by default**, and that is what makes a 256-token
    /// budget buy an answer instead of reasoning the client never sees.
    #[test]
    fn thinking_is_disabled_by_default() {
        let body = build(&[msg("user", "hi")], Some(256));
        assert_eq!(body["thinking"]["type"], "disabled");
    }

    /// The other two arms exist for the models that refuse an explicit
    /// `disabled` (Fable 5) or where the operator wants reasoning.
    #[test]
    fn the_thinking_knob_selects_the_other_two_shapes() {
        let convo = [msg("user", "hi")];
        let adaptive =
            build_request(DEFAULT_MODEL, Thinking::Adaptive, &convo, None).expect("builds");
        assert_eq!(adaptive["thinking"]["type"], "adaptive");
        let auto = build_request(DEFAULT_MODEL, Thinking::Auto, &convo, None).expect("builds");
        assert!(
            auto.get("thinking").is_none(),
            "auto sends no thinking field: {auto}"
        );
    }

    /// `$CLAUDE_BRIDGE_THINKING` parsing, including the fall-back-to-default
    /// rule a typo in a unit file relies on.
    #[test]
    fn thinking_parsing_defaults_to_disabled() {
        assert_eq!(Thinking::parse(None), Thinking::Disabled);
        assert_eq!(Thinking::parse(Some("disabled")), Thinking::Disabled);
        assert_eq!(Thinking::parse(Some(" adaptive ")), Thinking::Adaptive);
        assert_eq!(Thinking::parse(Some("auto")), Thinking::Auto);
        assert_eq!(Thinking::parse(Some("nonsense")), Thinking::Disabled);
    }

    /// **`temperature` must never be forwarded.** It is rejected with a 400 on
    /// every current model, so passing the client's value through would turn
    /// each pet tick into a bad request. The accepted-and-ignored contract is
    /// the whole point.
    #[test]
    fn sampling_parameters_the_api_rejects_are_never_sent() {
        let body = build(&[msg("system", "persona"), msg("user", "hi")], Some(256));
        for banned in ["temperature", "top_p", "top_k", "chat_template_kwargs"] {
            assert!(body.get(banned).is_none(), "{banned} leaked: {body}");
        }
    }

    /// End to end on the request side, from the literal bytes
    /// `hytte_ai_providers::chat` sends for a keyless provider.
    #[test]
    fn the_body_the_pet_actually_sends_translates() {
        let raw = br#"{
            "messages": [
                {"role": "system", "content": "you are a cat"},
                {"role": "user", "content": "poke"}
            ],
            "max_tokens": 256,
            "temperature": 0.7,
            "chat_template_kwargs": {"enable_thinking": false}
        }"#;
        let req: ChatRequest = serde_json::from_slice(raw).expect("parses");
        let body = build(&req.messages, req.max_tokens);
        assert_eq!(body["model"], DEFAULT_MODEL);
        assert_eq!(body["max_tokens"], 256);
        assert_eq!(body["system"], "you are a cat");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "poke");
        assert!(body.get("temperature").is_none(), "{body}");
    }

    // ── response translation ─────────────────────────────────────────────

    /// The answer is the concatenated `text` blocks; every other block kind —
    /// `thinking` today, whatever ships next — is ignored rather than rejected.
    #[test]
    fn text_blocks_are_concatenated_and_other_blocks_ignored() {
        let raw = r#"{
            "id": "msg_01",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-5",
            "content": [
                {"type": "thinking", "thinking": "IGNORED"},
                {"type": "text", "text": "one "},
                {"type": "text", "text": "two"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 12, "output_tokens": 3}
        }"#;
        assert_eq!(parse_response(raw).expect("text"), "one two");
    }

    /// A refusal is an HTTP **200** with no answer in it — checking
    /// `stop_reason` before reading `content` is the whole reason this is not a
    /// one-liner.
    #[test]
    fn a_refusal_is_not_treated_as_an_answer() {
        let raw = r#"{"content":[],"stop_reason":"refusal",
                      "stop_details":{"type":"refusal","category":"cyber"}}"#;
        let err = parse_response(raw).expect_err("refused");
        assert_eq!(err.status, 502);
        assert!(err.message.contains("refusal"), "{}", err.message);
    }

    /// A textless 200 is a 502, not an empty 200 — an empty answer reaches the
    /// pet as a blank speech bubble.
    #[test]
    fn a_textless_response_is_a_bad_gateway() {
        let raw = r#"{"content":[{"type":"thinking","thinking":"…"}],"stop_reason":"end_turn"}"#;
        assert_eq!(parse_response(raw).expect_err("no text").status, 502);
    }

    /// A truncated answer is still an answer.
    #[test]
    fn a_max_tokens_truncation_still_yields_its_text() {
        let raw = r#"{"content":[{"type":"text","text":"half a th"}],"stop_reason":"max_tokens"}"#;
        assert_eq!(parse_response(raw).expect("text"), "half a th");
    }

    /// Garbage is a 502 with a reason, not a panic.
    #[test]
    fn an_unparseable_body_is_a_bad_gateway() {
        assert_eq!(parse_response("not json").expect_err("refused").status, 502);
    }

    /// The full round trip the plugins actually see: a real Messages API
    /// response becomes the single-choice envelope `chat()` parses.
    #[test]
    fn a_messages_response_becomes_the_envelope_the_client_parses() {
        let raw = r#"{
            "id": "msg_01XyZ",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-5",
            "content": [{"type": "text", "text": "(=^･ω･^=) mrrp"}],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {"input_tokens": 24, "output_tokens": 9}
        }"#;
        let text = interpret(200, raw).expect("answer");
        let envelope = ChatResponse::single(text, Some("claude-opus-5".to_owned()), 42, 7);
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&envelope).expect("serialises"))
                .expect("round-trips");
        assert_eq!(json["choices"][0]["message"]["content"], "(=^･ω･^=) mrrp");
        assert_eq!(json["choices"][0]["message"]["role"], "assistant");
        assert_eq!(json["object"], "chat.completion");
    }

    // ── error mapping ────────────────────────────────────────────────────

    /// Each status maps to something the operator can act on, with the API's own
    /// message carried through.
    #[test]
    fn statuses_map_to_actionable_failures() {
        let auth = map_status(
            401,
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
        );
        assert_eq!(auth.status, 502);
        assert!(auth.message.contains("anthropic.key"), "{}", auth.message);
        assert!(
            auth.message.contains("invalid x-api-key"),
            "{}",
            auth.message
        );

        assert_eq!(map_status(403, "{}").status, 502);
        let missing_model = map_status(
            404,
            r#"{"type":"error","error":{"type":"not_found_error","message":"model: nope"}}"#,
        );
        assert_eq!(missing_model.status, 502);
        assert!(
            missing_model.message.contains("CLAUDE_BRIDGE_MODEL"),
            "{}",
            missing_model.message
        );
        assert_eq!(map_status(429, "{}").status, 429);
        assert_eq!(map_status(500, "{}").status, 502);
        assert_eq!(map_status(529, "{}").status, 502);
        assert_eq!(map_status(418, "{}").status, 502);
    }

    /// A prompt that does not fit reports the **same** status the CLI paths
    /// report for the same condition, so a plugin sees one vocabulary.
    #[test]
    fn an_oversized_prompt_reports_the_shared_overflow_status() {
        let over = map_status(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error",
                "message":"prompt is too long: 1200000 tokens > 1000000 maximum"}}"#,
        );
        assert_eq!(over.status, OVERFLOW_STATUS);
        assert_eq!(map_status(413, "{}").status, OVERFLOW_STATUS);
        // …and an ordinary bad request is still a 400.
        let plain = map_status(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: required"}}"#,
        );
        assert_eq!(plain.status, 400);
    }

    /// A non-JSON error body still reaches the caller instead of a bare status.
    #[test]
    fn a_non_json_error_body_is_still_surfaced() {
        let f = map_status(502, "<html>  gateway\n  down </html>");
        assert!(f.message.contains("gateway down"), "{}", f.message);
    }

    /// Error text is clamped so a huge body cannot swamp the pet's log line.
    #[test]
    fn error_text_is_clamped() {
        let huge = format!(r#"{{"error":{{"message":"{}"}}}}"#, "x".repeat(5_000));
        assert!(map_status(400, &huge).message.chars().count() <= 301);
    }

    /// A 2xx goes to the parser and a non-2xx to the mapper — the one branch
    /// `respond` depends on.
    #[test]
    fn only_a_success_status_is_parsed_as_an_answer() {
        let ok = r#"{"content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn"}"#;
        assert_eq!(interpret(200, ok).expect("answer"), "hi");
        assert_eq!(interpret(429, "{}").expect_err("rate-limited").status, 429);
        // A 2xx that is not 200 is still a success — the API answers 200, but a
        // proxy in front of it need not.
        assert_eq!(interpret(299, ok).expect("answer"), "hi");
    }

    // ── the key ──────────────────────────────────────────────────────────

    /// The loader must behave exactly like `hytte_ai_providers::load_key` — env
    /// override first, then the trimmed key file, empty is unset — or the
    /// workspace grows a third key-loading style.
    #[test]
    fn key_loading_matches_the_workspace_convention() {
        let dir =
            std::env::temp_dir().join(format!("hytte-claude-bridge-key-{}", std::process::id()));
        let ts = dir.join("trollshell");
        std::fs::create_dir_all(&ts).expect("mkdir");
        std::fs::write(ts.join("anthropic.key"), "  sk-ant-file\n").expect("write key");

        assert_eq!(
            load_key_from(None, Some(dir.clone())).as_deref(),
            Some("sk-ant-file"),
            "file read and trimmed",
        );
        assert_eq!(
            load_key_from(Some("  sk-ant-env ".to_owned()), Some(dir.clone())).as_deref(),
            Some("sk-ant-env"),
            "env override wins",
        );
        assert_eq!(
            load_key_from(Some("   ".to_owned()), Some(dir.clone())).as_deref(),
            Some("sk-ant-file"),
            "a blank override falls through to the file",
        );
        std::fs::write(ts.join("anthropic.key"), "  \n").expect("blank the key");
        assert!(
            load_key_from(None, Some(dir.clone())).is_none(),
            "an empty key file is unset",
        );
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            load_key_from(None, Some(dir)).is_none(),
            "a missing key file is unset, not a panic",
        );
    }

    /// The key must never reach a log line: `main.rs` logs the settings and
    /// `Backend` derives `Debug` all the way down to this client.
    #[test]
    fn the_client_never_debug_prints_its_key() {
        let client = super::Client::new(
            "sk-ant-SECRET".to_owned(),
            DEFAULT_MODEL.to_owned(),
            Thinking::default(),
            std::time::Duration::from_secs(7),
        );
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("SECRET"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
        assert!(rendered.contains(DEFAULT_MODEL), "{rendered}");
    }

    /// The refusal names both ways to configure a key and says what refusing
    /// buys — it is the only thing a human will see in `systemctl status`.
    #[test]
    fn the_missing_key_refusal_names_both_sources() {
        let msg = super::missing_key_refusal();
        assert!(msg.contains("anthropic.key"), "{msg}");
        assert!(msg.contains("ANTHROPIC_API_KEY"), "{msg}");
        assert!(msg.contains("CLAUDE_BRIDGE_MODE=api"), "{msg}");
    }
}
