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
//! [`Reprompt`] is where the thread's *other* cut landed too (#730): its
//! [`Engine`] chooses between the one-off `claude` subprocess and a real
//! `/v1/messages` API client ([`crate::messages`]), because the record-plus-
//! re-prompt shape is exactly the stateless one that path wants. Selected with
//! `CLAUDE_BRIDGE_MODE=api`. The record, the bound, and the head-pinning are
//! shared by both engines — only "who answers a composed message list" differs.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use hive_claude::{Attach, Claude, Config, Error as ClaudeError, Sink};
use serde_json::Value;

use crate::http::Failure;
use crate::messages::Client as MessagesClient;
use crate::session::{AttachKind, OVERFLOW_STATUS, prompt_for, render_transcript};
use crate::wire::Message;

/// Longest error text handed back to the client. `Error::Exit` carries up to
/// twenty stderr lines, which would swamp the pet's one-line log.
pub const MAX_ERROR_CHARS: usize = 300;

/// Longest conversation the [`Reprompt`] record keeps. Past this the oldest
/// turns are dropped, but the head (normally the system/persona message) is
/// pinned so the conversation doesn't lose its instructions.
///
/// It is also the **cost** bound on the API engine (#730): a stateless
/// re-prompt re-sends the whole record every turn, so this is what stops a
/// long-lived pet conversation growing its per-tick bill without limit.
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
    /// The client's `max_tokens`, carried down rather than dropped at the HTTP
    /// layer.
    ///
    /// The Claude Code CLI exposes no such flag, so both `claude` arms ignore
    /// it — that is the "at best approximated" the crate docs promise. The
    /// Messages API **requires** it, so [`crate::messages`] is the one arm that
    /// can honour it, and does.
    pub max_tokens: Option<u32>,
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
    /// Serialises same-title turns (#693).
    ///
    /// Claude Code does **not** serialise two concurrent `--resume`s of one
    /// session — measured, not assumed. Both runs exit 0 with plausible text,
    /// and the session's JSONL is left with one uuid parenting two divergent
    /// children: the transcript becomes a tree, and every later resume
    /// resolves against an ambiguous state. Nothing errors, which is what
    /// makes it worth a lock rather than a retry.
    turns: TitleLocks,
}

impl Subscription {
    /// A subscription backend running `claude` per `config`.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            turns: TitleLocks::default(),
        }
    }
}

impl Conversation for Subscription {
    async fn respond(&self, turn: Turn<'_>) -> Result<String, Failure> {
        resume_then_create(turn, &self.turns, |attach, prompt| async move {
            let sink = TextSink::default();
            match Claude::run(&self.config, &attach, &prompt, &sink).await {
                Ok(()) => sink.into_text().map_err(RunError::Failed),
                Err(ClaudeError::SessionNotFound) => Err(RunError::NoSession),
                Err(e) => Err(RunError::Failed(map_error(&e))),
            }
        })
        .await
    }
}

/// Why one attach attempt produced no answer.
///
/// Only [`RunError::NoSession`] is a *routing* outcome; everything else is
/// already the status the client should see. It exists so the resume-then-
/// create sequence can be written once over a seam — spawning a real `claude`
/// to test a lock would be neither hermetic nor free.
#[derive(Debug)]
enum RunError {
    /// The title resolved to no session; the caller may create it.
    NoSession,
    /// Anything else, already mapped for the client.
    Failed(Failure),
}

/// Answer one turn under `turn.title`: resume its session, and create one if
/// there is none — with **both** steps under that title's lock.
///
/// The lock spanning the whole sequence rather than just the resume is the
/// half of #693 that is easy to miss. Two concurrent *first* turns would each
/// miss the resume and each `Attach::Create`, leaving two session files under
/// one title — and `hive-claude` picks between those by readdir order, which
/// it warns about but cannot fix (`store.rs`: "multiple session files share
/// this title … not deterministic"; `Attach::Create`: "Nothing enforces
/// uniqueness"). Guarding only the resume would close the transcript fork and
/// open a rarer, nastier duplicate-session bug in its place.
///
/// `run` is the seam: in production it is `Claude::run` plus a [`TextSink`].
async fn resume_then_create<F, Fut>(
    turn: Turn<'_>,
    locks: &TitleLocks,
    run: F,
) -> Result<String, Failure>
where
    F: Fn(Attach, String) -> Fut,
    Fut: Future<Output = Result<String, RunError>>,
{
    let _serialised = locks.acquire(turn.title).await;

    // Resume first — always. The session may exist from an earlier turn, an
    // earlier bridge process, or an identical earlier request; in every one
    // of those cases it already holds the prefix, so it must receive ONLY
    // the newest message.
    let resume_prompt = prompt_for(AttachKind::Resume, turn.transcript, turn.delta);
    match run(Attach::Resume(turn.title.to_owned()), resume_prompt).await {
        Ok(text) => return Ok(text),
        Err(RunError::NoSession) => {
            tracing::debug!(title = turn.title, "no such session; creating");
        }
        Err(RunError::Failed(failure)) => return Err(failure),
    }

    // Only a session that does not exist yet gets the whole transcript —
    // there is nothing in it to duplicate.
    let create_prompt = prompt_for(AttachKind::Create, turn.transcript, turn.delta);
    run(Attach::Create(turn.title.to_owned()), create_prompt)
        .await
        .map_err(|e| match e {
            RunError::NoSession => map_error(&ClaudeError::SessionNotFound),
            RunError::Failed(failure) => failure,
        })
}

/// One async mutex per conversation title, minted on demand and retired when
/// its last holder releases it.
///
/// **Per title, not global.** The title *is* the conversation's identity (see
/// [`crate::session`]), so a global lock would park the pet's turn behind
/// caw's for no reason — a latency regression on the common case, in service
/// of a hazard that only exists when two turns share a title.
///
/// The outer `std` mutex is only ever held for one map lookup, never across an
/// await; the inner `tokio` one is held across a whole turn, which is exactly
/// why it has to be `tokio`'s.
#[derive(Debug, Default)]
struct TitleLocks {
    by_title: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl TitleLocks {
    /// Wait for exclusive use of `title`.
    async fn acquire(&self, title: &str) -> TitleGuard<'_> {
        let mutex = {
            let mut by_title = self.by_title.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(by_title.entry(title.to_owned()).or_default())
        };
        TitleGuard {
            locks: self,
            title: title.to_owned(),
            // Owned rather than borrowed: the guard outlives the local `Arc`,
            // and an owned guard keeps its mutex alive by itself.
            held: Some(mutex.lock_owned().await),
        }
    }

    /// How many titles hold a lock right now. Only the invariant that this
    /// falls back to zero is interesting, which makes it a test's business.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_title
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// Exclusive use of one title, released on drop.
#[derive(Debug)]
struct TitleGuard<'a> {
    locks: &'a TitleLocks,
    title: String,
    /// An `Option` purely so [`Drop`] can release the mutex *before* deciding
    /// whether its map entry is still wanted.
    held: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for TitleGuard<'_> {
    fn drop(&mut self) {
        // Release first: an owned guard holds a strong reference of its own, so
        // the count below could never fall to one while this still held it.
        drop(self.held.take());
        let mut by_title = self
            .locks
            .by_title
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // The cleanup race, and why a strong count is the right question: a
        // task that has already claimed this title holds a reference of its own
        // — it clones the `Arc` under this same `std` mutex, *before* it starts
        // waiting — so a count of one proves the map is the sole owner and
        // nobody can be parked on it. Retiring an entry out from under a waiter
        // would be worse than leaking it: the waiter would hold a mutex the map
        // no longer names, and the next arrival would mint a second one and
        // fail to exclude it.
        if by_title
            .get(&self.title)
            .is_some_and(|mutex| Arc::strong_count(mutex) == 1)
        {
            by_title.remove(&self.title);
        }
    }
}

/// The re-prompting path: the bridge holds the conversation, the thing that
/// answers holds nothing.
///
/// Which thing answers is [`Engine`]. Everything *around* it — the bounded,
/// head-pinned record and the compose/remember cycle — is shared, because that
/// is the part the stateless shape is made of; swapping a subprocess for an
/// HTTP call changes nothing about the conversation's identity or its bound.
#[derive(Debug)]
pub struct Reprompt {
    engine: Engine,
    records: Mutex<Records>,
}

/// Who answers one composed message list.
#[derive(Debug)]
enum Engine {
    /// A one-off `claude --print` session per turn — no `--resume`, no
    /// `--name`, so claude persists no titled session for the bridge to depend
    /// on. Rides the subscription; holds no key.
    ///
    /// Boxed because `hive-claude` 0.1.0's `Config` grew past the point where
    /// `clippy::large_enum_variant` tolerates inlining it (288 bytes against
    /// [`Engine::Api`]'s 72). One `Engine` exists per process, so the
    /// indirection costs nothing that matters.
    Cli(Box<Config>),
    /// The Anthropic Messages API, billed to an API key (#730). No subprocess,
    /// no `claude` CLI, no `hive-claude` session state at all.
    Api(MessagesClient),
}

impl Reprompt {
    /// A re-prompting backend running `claude` per `config`.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            engine: Engine::Cli(Box::new(config)),
            records: Mutex::new(Records::default()),
        }
    }

    /// A re-prompting backend answering through the Messages API instead of a
    /// `claude` subprocess (#730).
    #[must_use]
    pub fn with_api(client: MessagesClient) -> Self {
        Self {
            engine: Engine::Api(client),
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
        // The guard above is dropped before this await on purpose: a `Mutex`
        // held across an await point would make this future non-`Send`, which
        // the `Conversation` trait requires.
        let reply = match &self.engine {
            Engine::Cli(config) => {
                let sink = TextSink::default();
                Claude::run(
                    config,
                    &Attach::OneOff,
                    &render_transcript(&messages),
                    &sink,
                )
                .await
                .map_err(|e| map_error(&e))?;
                sink.into_text()?
            }
            Engine::Api(client) => client.respond(&messages, turn.max_tokens).await?,
        };
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
        // `program` is a `PathBuf` since #757's `hive-claude` upgrade: it may be
        // an absolute path rather than a `PATH`-resolved name, so it is
        // `.display()`ed rather than formatted directly.
        ClaudeError::Spawn { program, source } => (
            502,
            format!(
                "could not spawn `{}`: {source} (is the Claude Code CLI on PATH?)",
                program.display()
            ),
        ),
        other => (502, format!("claude: {other}")),
    };
    Failure::new(status, truncate(&message, MAX_ERROR_CHARS))
}

/// Clamp `text` to `max` chars (never mid-char).
#[must_use]
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let clamped: String = text.chars().take(max).collect();
    format!("{clamped}…")
}

#[cfg(test)]
mod tests {
    use super::{Records, RunError, TextSink, TitleLocks, Turn, map_error, truncate};
    use crate::http::Failure;
    use crate::wire::Message;
    use hive_claude::{Attach, Error as ClaudeError, Sink as _};
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// How long one fake turn stays inside the seam, in scheduler yields. On a
    /// current-thread runtime a yield hands control to the other half of the
    /// race at exactly the points a real subprocess await would, which is what
    /// makes the races below deterministic rather than hopeful.
    const YIELDS: usize = 8;

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.to_owned(),
            content: content.to_owned(),
        }
    }

    /// A turn over `transcript` whose delta is its newest message.
    fn turn<'a>(title: &'a str, transcript: &'a [Message]) -> Turn<'a> {
        Turn {
            title,
            transcript,
            delta: transcript.last().expect("non-empty"),
            max_tokens: None,
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
    /// retire. **Both** reprompt engines are non-persisted — the API one holds
    /// no claude session state at all, so a rotation there would spin the
    /// generation counter once per request and fix nothing (#667).
    #[test]
    fn only_the_subscription_backend_rotates() {
        use super::{Backend, Reprompt, Subscription};
        use crate::messages::{Client as MessagesClient, Thinking};
        use hive_claude::Config;
        use std::time::Duration;
        assert!(Backend::Subscription(Subscription::new(Config::default())).is_persisted());
        assert!(!Backend::Reprompt(Reprompt::new(Config::default())).is_persisted());
        let api = Reprompt::with_api(MessagesClient::new(
            "sk-ant-test".to_owned(),
            "claude-opus-5".to_owned(),
            Thinking::default(),
            Duration::from_secs(7),
        ));
        assert!(!Backend::Reprompt(api).is_persisted());
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
        assert_eq!(records.compose(turn("t", &transcript)), transcript);
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
        let composed = records.compose(turn("t", &transcript));
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

    // ---- #693: same-title turns must not run concurrently ------------------

    /// A stand-in for `Claude::run`: it records what it was asked to attach to,
    /// keeps its own idea of which sessions exist, and — the point — reports
    /// how many turns were ever inside it at once.
    #[derive(Debug, Default)]
    struct Fake {
        /// `("resume" | "create", title)`, in the order the fake saw them.
        calls: Mutex<Vec<(&'static str, String)>>,
        /// Sessions this fake has created; a resume misses until one exists.
        sessions: Mutex<HashSet<String>>,
        inside: AtomicUsize,
        /// The most turns ever inside the fake at one time. `1` is the whole
        /// claim of #693.
        peak: AtomicUsize,
    }

    impl Fake {
        /// One fake turn, wide enough for the other racer to get inside it.
        ///
        /// The state is read and written *after* the yields on purpose: that is
        /// what lets two unguarded first turns both observe "no such session"
        /// before either creates one.
        async fn run(&self, attach: Attach, _prompt: String) -> Result<String, RunError> {
            let inside = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(inside, Ordering::SeqCst);
            // A create replays the whole transcript where a resume sends only
            // the delta, so it really is the slower of the two. Modelled,
            // because it is what makes the resume-then-create window
            // observable at all: a lock covering only the resume lets the
            // loser's resume run *while* the winner is still inside its
            // create, and miss. With both arms equally fast the deterministic
            // interleaving below happens to hide that, which is a property of
            // the test rather than of the code.
            let stay = match &attach {
                Attach::Create(_) => YIELDS * 3,
                _ => YIELDS,
            };
            for _ in 0..stay {
                tokio::task::yield_now().await;
            }
            let outcome = match &attach {
                Attach::Resume(title) => {
                    self.record("resume", title);
                    if self.sessions.lock().expect("sessions").contains(title) {
                        Ok(format!("resumed {title}"))
                    } else {
                        Err(RunError::NoSession)
                    }
                }
                Attach::Create(title) => {
                    self.record("create", title);
                    self.sessions
                        .lock()
                        .expect("sessions")
                        .insert(title.to_owned());
                    Ok(format!("created {title}"))
                }
                other => panic!("the subscription path never attaches as {other:?}"),
            };
            self.inside.fetch_sub(1, Ordering::SeqCst);
            outcome
        }

        fn record(&self, kind: &'static str, title: &str) {
            self.calls
                .lock()
                .expect("calls")
                .push((kind, title.to_owned()));
        }

        /// How many times the fake was asked to create `title`.
        fn creates(&self, title: &str) -> usize {
            self.calls
                .lock()
                .expect("calls")
                .iter()
                .filter(|(kind, seen)| *kind == "create" && seen == title)
                .count()
        }
    }

    /// Two concurrent turns on one title must not overlap.
    ///
    /// This is #693's measured failure: unguarded, both enter `Claude::run`,
    /// and claude forks the session's transcript into a tree — silently, with
    /// both callers getting a plausible answer. Both turns here take the
    /// **resume** arm, the shape that was actually reproduced.
    #[tokio::test]
    async fn concurrent_turns_on_one_title_never_overlap() {
        let locks = TitleLocks::default();
        let fake = Fake::default();
        fake.sessions
            .lock()
            .expect("sessions")
            .insert("t".to_owned());
        let transcript = vec![msg("system", "persona"), msg("user", "hi")];

        let (one, two) = tokio::join!(
            super::resume_then_create(turn("t", &transcript), &locks, |a, p| fake.run(a, p)),
            super::resume_then_create(turn("t", &transcript), &locks, |a, p| fake.run(a, p)),
        );

        assert_eq!(one.expect("answered"), "resumed t");
        assert_eq!(two.expect("answered"), "resumed t");
        assert_eq!(
            fake.peak.load(Ordering::SeqCst),
            1,
            "two turns were inside claude at once — the session forks here"
        );
        assert_eq!(locks.len(), 0, "the title's lock outlived its holders");
    }

    /// The half that is easy to miss: two concurrent **first** turns.
    ///
    /// Both miss the resume, so a lock covering only the resume would let both
    /// create — two session files under one title, which `--resume` then picks
    /// between by readdir order. Exactly one create, and the loser resumes the
    /// winner's session rather than minting a rival.
    #[tokio::test]
    async fn concurrent_first_turns_create_exactly_one_session() {
        let locks = TitleLocks::default();
        let fake = Fake::default();
        let transcript = vec![msg("system", "persona"), msg("user", "hi")];

        let (one, two) = tokio::join!(
            super::resume_then_create(turn("t", &transcript), &locks, |a, p| fake.run(a, p)),
            super::resume_then_create(turn("t", &transcript), &locks, |a, p| fake.run(a, p)),
        );

        one.expect("answered");
        two.expect("answered");
        assert_eq!(
            fake.creates("t"),
            1,
            "one title, two sessions on disk — `--resume` would pick by readdir order"
        );
        assert_eq!(
            *fake.calls.lock().expect("calls"),
            vec![
                ("resume", "t".to_owned()),
                ("create", "t".to_owned()),
                ("resume", "t".to_owned()),
            ],
            "the second turn should have joined the first's session"
        );
        assert_eq!(fake.sessions.lock().expect("sessions").len(), 1);
    }

    /// Per title, not global. A global lock would pass both tests above and
    /// still be wrong: it parks the pet behind caw on every request whose title
    /// differs, which is the common case.
    #[tokio::test]
    async fn turns_on_different_titles_still_run_concurrently() {
        let locks = TitleLocks::default();
        let fake = Fake::default();
        let transcript = vec![msg("user", "hi")];

        let (pet, caw) = tokio::join!(
            super::resume_then_create(turn("pet", &transcript), &locks, |a, p| fake.run(a, p)),
            super::resume_then_create(turn("caw", &transcript), &locks, |a, p| fake.run(a, p)),
        );

        pet.expect("answered");
        caw.expect("answered");
        assert_eq!(
            fake.peak.load(Ordering::SeqCst),
            2,
            "unrelated conversations were serialised against each other"
        );
    }

    /// The map-entry cleanup race: a release that another task is already
    /// waiting on must **not** retire the entry.
    ///
    /// If it did, the waiter would hold a mutex the map no longer names, and
    /// the next arrival would mint a second one and not exclude it — the exact
    /// overlap the lock exists to prevent. The waiter therefore has to still
    /// find its own entry in the map once it wakes.
    #[tokio::test]
    async fn releasing_a_contended_title_keeps_its_entry() {
        let locks = TitleLocks::default();
        let seen_by_waiter = Mutex::new(None);

        let holder = async {
            let guard = locks.acquire("t").await;
            // Let the waiter reach the wait queue before releasing.
            for _ in 0..YIELDS {
                tokio::task::yield_now().await;
            }
            drop(guard);
        };
        let waiter = async {
            let guard = locks.acquire("t").await;
            *seen_by_waiter.lock().expect("seen") = Some(locks.len());
            drop(guard);
        };
        tokio::join!(holder, waiter);

        assert_eq!(
            *seen_by_waiter.lock().expect("seen"),
            Some(1),
            "the waiter's entry was retired out from under it"
        );
        assert_eq!(locks.len(), 0, "the entry outlived its last holder");
    }

    /// The map is a cache of live contention, not a log of every title the
    /// process has ever answered under.
    #[tokio::test]
    async fn the_lock_map_does_not_accumulate_titles() {
        let locks = TitleLocks::default();
        for n in 0..200 {
            let guard = locks.acquire(&format!("t{n}")).await;
            assert_eq!(locks.len(), 1);
            drop(guard);
        }
        assert_eq!(locks.len(), 0);
    }

    /// A resume that lands never creates — the delta rule depends on it.
    #[tokio::test]
    async fn a_resumed_session_is_not_recreated() {
        let locks = TitleLocks::default();
        let fake = Fake::default();
        fake.sessions
            .lock()
            .expect("sessions")
            .insert("t".to_owned());
        let transcript = vec![msg("user", "hi")];

        let reply =
            super::resume_then_create(turn("t", &transcript), &locks, |a, p| fake.run(a, p))
                .await
                .expect("answered");

        assert_eq!(reply, "resumed t");
        assert_eq!(fake.creates("t"), 0);
    }

    /// A failure that is not `SessionNotFound` is reported, not retried as a
    /// create: retrying a rate limit or an auth failure that way would mint a
    /// rival session per failed turn.
    #[tokio::test]
    async fn a_non_session_failure_is_not_retried_as_a_create() {
        let locks = TitleLocks::default();
        let attempts = AtomicUsize::new(0);
        let transcript = vec![msg("user", "hi")];

        let out = super::resume_then_create(turn("t", &transcript), &locks, |_attach, _prompt| {
            attempts.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Err(RunError::Failed(Failure::new(429, "rate-limited"))))
        })
        .await;

        assert_eq!(out.expect_err("failed").status, 429);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    /// The tests above pin the interleaving deterministically on a
    /// current-thread runtime. This one hands it to a real scheduler instead:
    /// 256 rounds of 4 racers on 4 worker threads — 1024 turns whose ordering
    /// nobody controls — and both invariants have to hold in every round.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_race_stays_closed_on_a_multi_threaded_runtime() {
        const ROUNDS: usize = 256;
        const RACERS: usize = 4;

        let locks = Arc::new(TitleLocks::default());
        let fake = Arc::new(Fake::default());
        for round in 0..ROUNDS {
            let title = format!("t{round}");
            let mut racers = tokio::task::JoinSet::new();
            for _ in 0..RACERS {
                let locks = Arc::clone(&locks);
                let fake = Arc::clone(&fake);
                let title = title.clone();
                racers.spawn(async move {
                    let transcript = vec![msg("user", "hi")];
                    super::resume_then_create(turn(&title, &transcript), &locks, |a, p| {
                        fake.run(a, p)
                    })
                    .await
                });
            }
            while let Some(joined) = racers.join_next().await {
                joined.expect("no panic").expect("answered");
            }
            assert_eq!(fake.creates(&title), 1, "round {round} created twice");
        }
        assert_eq!(
            fake.peak.load(Ordering::SeqCst),
            1,
            "turns on one title overlapped"
        );
        assert_eq!(locks.len(), 0);
    }
}
