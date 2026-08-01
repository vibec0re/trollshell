//! One conversation abstraction, two native implementations.
//!
//! This is @kaesaecracker's cut from the #584 thread, and it is the reason the
//! bridge is not "two backends that happen to share a URL": both arms answer
//! the same [`Turn`] — a title, a transcript, and the newest message — and each
//! does the *native* thing with it rather than emulating the other.
//!
//! - [`Subscription`] rides a **persisted** `hive-claude` session addressed by
//!   title. Resume-then-create, and a resumed session receives **only the
//!   delta**, so the session's prompt prefix stays byte-stable turn over turn
//!   and Claude Code's prompt cache can actually hit. This is the default.
//! - [`Reprompt`] keeps the conversation in the **bridge's own record** and
//!   re-prompts a fresh one-off session with the whole thing each turn. No
//!   claude session state touches disk, at the cost of re-sending context.
//!   Selected with `CLAUDE_BRIDGE_MODE=reprompt`.
//!
//! When the bridge eventually grows a real `/v1/messages` API client (the other
//! half of the thread's cut), it lands inside [`Reprompt`] — the shape is
//! already the stateless-re-prompt one that path wants.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Mutex, PoisonError};

use hive_claude::{Attach, Claude, Config, Error as ClaudeError, Sink};
use serde_json::Value;

use crate::http::Failure;
use crate::session::{AttachKind, OVERFLOW_STATUS, prompt_for, render_transcript};
use crate::wire::Message;

/// Longest error text handed back to the client. `Error::Exit` carries up to
/// twenty stderr lines, which would swamp the pet's one-line log.
const MAX_ERROR_CHARS: usize = 300;

/// Longest conversation the [`Reprompt`] record keeps. Past this the oldest
/// turns are dropped, but the head (normally the system/persona message) is
/// pinned so the conversation doesn't lose its instructions.
const MAX_RECORD_MESSAGES: usize = 64;

/// How many conversations [`Reprompt`] keeps records for.
const MAX_RECORDS: usize = 64;

/// One turn to answer.
#[derive(Debug, Clone, Copy)]
pub struct Turn<'a> {
    /// The derived session title — the conversation's identity (see
    /// [`crate::session`]).
    pub title: &'a str,
    /// The full transcript the client sent, newest message last.
    pub transcript: &'a [Message],
    /// The newest message, i.e. the delta the session has not seen.
    pub delta: &'a Message,
}

/// The abstraction both backends implement.
pub trait Conversation {
    /// Answer one turn, or explain why not in terms the HTTP layer can send.
    fn respond(&self, turn: Turn<'_>) -> impl Future<Output = Result<String, Failure>> + Send;
}

/// The subscription path: a persisted, title-addressed `hive-claude` session.
#[derive(Debug)]
pub struct Subscription {
    config: Config,
}

impl Subscription {
    /// A subscription backend running `claude` per `config`.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self { config }
    }
}

impl Conversation for Subscription {
    async fn respond(&self, turn: Turn<'_>) -> Result<String, Failure> {
        // Resume first — always. The session may exist from an earlier turn, an
        // earlier bridge process, or an identical earlier request; in every one
        // of those cases it already holds the prefix, so it must receive ONLY
        // the newest message.
        let resume_prompt = prompt_for(AttachKind::Resume, turn.transcript, turn.delta);
        let sink = TextSink::default();
        match Claude::run(
            &self.config,
            &Attach::Resume(turn.title.to_owned()),
            &resume_prompt,
            &sink,
        )
        .await
        {
            Ok(()) => return sink.into_text(),
            Err(ClaudeError::SessionNotFound) => {
                tracing::debug!(title = turn.title, "no such session; creating");
            }
            Err(e) => return Err(map_error(&e)),
        }

        // Only a session that does not exist yet gets the whole transcript —
        // there is nothing in it to duplicate.
        let create_prompt = prompt_for(AttachKind::Create, turn.transcript, turn.delta);
        let sink = TextSink::default();
        Claude::run(
            &self.config,
            &Attach::Create(turn.title.to_owned()),
            &create_prompt,
            &sink,
        )
        .await
        .map_err(|e| map_error(&e))?;
        sink.into_text()
    }
}

/// The re-prompting path: the bridge holds the conversation, claude holds
/// nothing.
#[derive(Debug)]
pub struct Reprompt {
    config: Config,
    records: Mutex<Records>,
}

impl Reprompt {
    /// A re-prompting backend running `claude` per `config`.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            records: Mutex::new(Records::default()),
        }
    }
}

impl Conversation for Reprompt {
    async fn respond(&self, turn: Turn<'_>) -> Result<String, Failure> {
        let messages = {
            let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
            records.compose(turn)
        };
        let sink = TextSink::default();
        // `Attach::OneOff` — no `--resume`, no `--name`, so claude persists no
        // titled session for the bridge to depend on. The record above is the
        // only continuity.
        Claude::run(
            &self.config,
            &Attach::OneOff,
            &render_transcript(&messages),
            &sink,
        )
        .await
        .map_err(|e| map_error(&e))?;
        let reply = sink.into_text()?;
        {
            let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
            records.remember(turn.title, messages, &reply);
        }
        Ok(reply)
    }
}

/// The bridge-side conversation store backing [`Reprompt`].
#[derive(Debug, Default)]
struct Records {
    by_title: HashMap<String, Vec<Message>>,
    order: VecDeque<String>,
}

impl Records {
    /// The message list to re-prompt with.
    ///
    /// Prefers the bridge's own record when it is at least as complete as the
    /// client's prefix — a client that drops assistant turns from its history
    /// (as a stateless prompt-stuffing client does) still gets continuity.
    /// Otherwise the client's transcript wins, because it knows something the
    /// record does not.
    fn compose(&self, turn: Turn<'_>) -> Vec<Message> {
        // Saturating: the HTTP layer rejects an empty `messages`, but a
        // panicking underflow in a daemon is not a defence worth relying on.
        let prefix_len = turn.transcript.len().saturating_sub(1);
        match self.by_title.get(turn.title) {
            Some(record) if record.len() >= prefix_len => {
                let mut composed = record.clone();
                composed.push(turn.delta.clone());
                composed
            }
            _ => turn.transcript.to_vec(),
        }
    }

    /// Record what was sent plus the reply it produced.
    fn remember(&mut self, title: &str, mut messages: Vec<Message>, reply: &str) {
        messages.push(Message::assistant(reply));
        // Drop from just after the head so the system/persona message stays.
        while messages.len() > MAX_RECORD_MESSAGES {
            messages.remove(1);
        }
        if self.by_title.insert(title.to_owned(), messages).is_none() {
            self.order.push_back(title.to_owned());
        }
        while self.order.len() > MAX_RECORDS {
            if let Some(evicted) = self.order.pop_front() {
                self.by_title.remove(&evicted);
            }
        }
    }
}

/// The runtime-selected backend. An enum rather than a `dyn` object because the
/// trait returns `impl Future` (not dyn-compatible) and there will only ever be
/// the two arms the design names.
#[derive(Debug)]
pub enum Backend {
    Subscription(Subscription),
    Reprompt(Reprompt),
}

impl Backend {
    /// Whether this backend's conversations accumulate in **claude's** own
    /// on-disk session state — the thing that grows without bound and that only
    /// a fresh title can retire (#667).
    ///
    /// [`Reprompt`]'s conversation lives in the bridge's own record, which is
    /// already bounded and head-pinned ([`MAX_RECORD_MESSAGES`]), so an
    /// overflow there means the *client's* transcript does not fit — which no
    /// rotation can fix, and which would spin the generation counter once per
    /// request if it were allowed to try.
    #[must_use]
    pub fn is_persisted(&self) -> bool {
        matches!(self, Self::Subscription(_))
    }
}

impl Conversation for Backend {
    async fn respond(&self, turn: Turn<'_>) -> Result<String, Failure> {
        match self {
            Self::Subscription(b) => b.respond(turn).await,
            Self::Reprompt(b) => b.respond(turn).await,
        }
    }
}

/// Collects a turn's answer out of the `stream-json` stream.
///
/// `hive_claude::Claude::run` reports only *how* a turn ended; the text itself
/// arrives through the [`Sink`]. The terminal `result` event carries claude's
/// final answer and is preferred; the concatenated `assistant` text blocks are
/// the fallback for a stream that ends without one.
#[derive(Debug, Default)]
struct TextSink {
    assistant: Mutex<String>,
    result: Mutex<Option<String>>,
}

impl TextSink {
    /// The collected answer, or a 502 if the stream carried no text at all.
    fn into_text(self) -> Result<String, Failure> {
        let result = self
            .result
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        let assistant = self
            .assistant
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        let text = result.filter(|s| !s.trim().is_empty()).unwrap_or(assistant);
        if text.trim().is_empty() {
            return Err(Failure::new(502, "claude produced no text"));
        }
        Ok(text)
    }
}

impl Sink for TextSink {
    fn on_event(&self, event: &Value) {
        match event.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let Some(blocks) = event.pointer("/message/content").and_then(Value::as_array)
                else {
                    return;
                };
                let mut out = self
                    .assistant
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(text) = block.get("text").and_then(Value::as_str)
                    {
                        out.push_str(text);
                    }
                }
            }
            Some("result") => {
                // `is_error` results carry claude's failure text, not an answer;
                // the driver's sentinels already turned those into an `Error`.
                if event.get("is_error").and_then(Value::as_bool) == Some(true) {
                    return;
                }
                if let Some(text) = event.get("result").and_then(Value::as_str) {
                    *self.result.lock().unwrap_or_else(PoisonError::into_inner) =
                        Some(text.to_owned());
                }
            }
            _ => {}
        }
    }

    fn on_stderr_line(&self, line: &str) {
        tracing::debug!(target: "hytte_claude_bridge::claude", "{line}");
    }
}

/// Map a driver error onto the status the client should see.
///
/// The typed sentinels are exactly why this crate depends on `hive-claude`
/// rather than sniffing stderr: `RateLimited` vs. `Spawn` vs. `AuthFailed` are
/// three very different operator actions, and classifying them by substring is
/// the kind of code nobody should own twice.
fn map_error(e: &ClaudeError) -> Failure {
    let (status, message) = match e {
        ClaudeError::RateLimited => (
            429,
            "claude: rate-limited or a usage/credit cap was reached".to_owned(),
        ),
        // The one failure a session rotation can fix (#667) — `bridge.rs`
        // triggers on exactly this status, so the constant is shared rather
        // than the number written twice.
        ClaudeError::PromptTooLong => (
            OVERFLOW_STATUS,
            "claude: the conversation exceeds the model's context window".to_owned(),
        ),
        ClaudeError::AuthFailed => (
            502,
            "claude is not authenticated — run `claude` and /login. \
             (The bridge holds no credentials of its own; it is keyless by design.)"
                .to_owned(),
        ),
        ClaudeError::IdleTimeout => (
            504,
            "claude produced no output within the bridge's budget".to_owned(),
        ),
        ClaudeError::SessionNotFound => (
            502,
            "claude could not resolve the session title even after creating it".to_owned(),
        ),
        ClaudeError::Spawn { program, source } => (
            502,
            format!("could not spawn `{program}`: {source} (is the Claude Code CLI on PATH?)"),
        ),
        other => (502, format!("claude: {other}")),
    };
    Failure::new(status, truncate(&message, MAX_ERROR_CHARS))
}

/// Clamp `text` to `max` chars (never mid-char).
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let clamped: String = text.chars().take(max).collect();
    format!("{clamped}…")
}

#[cfg(test)]
mod tests {
    use super::{Records, TextSink, Turn, map_error, truncate};
    use crate::wire::Message;
    use hive_claude::{Error as ClaudeError, Sink as _};
    use serde_json::json;

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.to_owned(),
            content: content.to_owned(),
        }
    }

    /// The terminal `result` event is the answer.
    #[test]
    fn the_result_event_supplies_the_answer() {
        let sink = TextSink::default();
        sink.on_event(&json!({"type": "system", "subtype": "init"}));
        sink.on_event(&json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "partial"}]}
        }));
        sink.on_event(&json!({"type": "result", "is_error": false, "result": "final answer"}));
        assert_eq!(sink.into_text().expect("text"), "final answer");
    }

    /// Without a `result` event the concatenated assistant text blocks stand in.
    #[test]
    fn assistant_blocks_are_the_fallback() {
        let sink = TextSink::default();
        sink.on_event(&json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "text", "text": "one "},
                {"type": "thinking", "thinking": "IGNORED"},
                {"type": "text", "text": "two"},
            ]}
        }));
        assert_eq!(sink.into_text().expect("text"), "one two");
    }

    /// An `is_error` result carries claude's failure text, not an answer.
    #[test]
    fn an_error_result_is_not_treated_as_an_answer() {
        let sink = TextSink::default();
        sink.on_event(&json!({"type": "result", "is_error": true, "result": "Prompt is too long"}));
        assert!(sink.into_text().is_err());
    }

    /// A stream with no text at all is a 502, not an empty 200 — an empty
    /// answer would reach the pet as a blank speech bubble.
    #[test]
    fn a_textless_stream_is_a_bad_gateway() {
        let sink = TextSink::default();
        sink.on_event(&json!({"type": "system", "subtype": "init"}));
        assert_eq!(sink.into_text().expect_err("no text").status, 502);
    }

    /// The rotation trigger, pinned end to end: the driver's typed overflow
    /// sentinel is the thing — and, of the statuses reachable from a turn, the
    /// only thing — that `bridge.rs` retires a session for (#667). If a second
    /// `ClaudeError` ever maps to this status it would start rotating sessions
    /// that are not full, so the exclusivity is asserted too.
    #[test]
    fn only_the_overflow_sentinel_triggers_a_rotation() {
        assert_eq!(
            map_error(&ClaudeError::PromptTooLong).status,
            crate::session::OVERFLOW_STATUS
        );
        for other in [
            ClaudeError::RateLimited,
            ClaudeError::AuthFailed,
            ClaudeError::IdleTimeout,
            ClaudeError::SessionNotFound,
        ] {
            assert_ne!(
                map_error(&other).status,
                crate::session::OVERFLOW_STATUS,
                "{other} would trigger a rotation"
            );
        }
    }

    /// Only the persisted-session backend accumulates state a rotation can
    /// retire.
    #[test]
    fn only_the_subscription_backend_rotates() {
        use super::{Backend, Reprompt, Subscription};
        use hive_claude::Config;
        assert!(Backend::Subscription(Subscription::new(Config::default())).is_persisted());
        assert!(!Backend::Reprompt(Reprompt::new(Config::default())).is_persisted());
    }

    /// Each typed sentinel maps to a distinct, actionable status.
    #[test]
    fn sentinels_map_to_distinct_statuses() {
        assert_eq!(map_error(&ClaudeError::RateLimited).status, 429);
        assert_eq!(map_error(&ClaudeError::PromptTooLong).status, 413);
        assert_eq!(map_error(&ClaudeError::AuthFailed).status, 502);
        assert_eq!(map_error(&ClaudeError::IdleTimeout).status, 504);
        assert_eq!(map_error(&ClaudeError::SessionNotFound).status, 502);
        assert!(
            map_error(&ClaudeError::AuthFailed)
                .message
                .contains("/login")
        );
    }

    /// A long `Error::Exit` stderr tail must not swamp the caller's log line.
    #[test]
    fn error_text_is_clamped() {
        let long = "x".repeat(5_000);
        assert!(truncate(&long, 300).chars().count() <= 301);
        assert_eq!(truncate("short", 300), "short");
        // Multi-byte input must not be split mid-char.
        assert_eq!(truncate("äöü", 2), "äö…");
    }

    /// With no record, the reprompt path sends the client's transcript verbatim.
    #[test]
    fn reprompt_falls_back_to_the_client_transcript() {
        let records = Records::default();
        let transcript = vec![msg("system", "persona"), msg("user", "hello")];
        let turn = Turn {
            title: "t",
            transcript: &transcript,
            delta: &transcript[1],
        };
        assert_eq!(records.compose(turn), transcript);
    }

    /// With a record, the reprompt path replays it plus the delta — including
    /// the assistant turns a stateless client dropped.
    #[test]
    fn reprompt_replays_its_own_record_plus_the_delta() {
        let mut records = Records::default();
        let turn1 = vec![msg("system", "persona"), msg("user", "hello")];
        records.remember("t", turn1.clone(), "hi there");

        // The client forgot the assistant turn and just appended a new question.
        let transcript = vec![msg("system", "persona"), msg("user", "again")];
        let composed = records.compose(Turn {
            title: "t",
            transcript: &transcript,
            delta: &transcript[1],
        });
        assert_eq!(
            composed,
            vec![
                msg("system", "persona"),
                msg("user", "hello"),
                msg("assistant", "hi there"),
                msg("user", "again"),
            ]
        );
    }

    /// A record is bounded and keeps its head (the persona) when it trims.
    #[test]
    fn records_are_bounded_and_pin_the_head() {
        let mut records = Records::default();
        let mut convo = vec![msg("system", "PERSONA")];
        for n in 0..200 {
            convo.push(msg("user", &format!("turn {n}")));
            records.remember("t", convo.clone(), "ok");
            convo = records.by_title["t"].clone();
        }
        let record = &records.by_title["t"];
        assert!(record.len() <= super::MAX_RECORD_MESSAGES);
        assert_eq!(record[0], msg("system", "PERSONA"));
    }

    /// The record map itself is bounded.
    #[test]
    fn the_record_map_is_bounded() {
        let mut records = Records::default();
        for n in 0..200 {
            records.remember(&format!("t{n}"), vec![msg("user", "hi")], "ok");
        }
        assert!(records.by_title.len() <= super::MAX_RECORDS);
    }
}
