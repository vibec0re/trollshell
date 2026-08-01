//! Request handling: routing, load shedding, single-flight, and the title map
//! that ties the two backends to one conversation identity.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::net::TcpStream;
use tokio::sync::{Semaphore, watch};

use crate::backend::{Backend, Conversation as _, Turn};
use crate::http::{self, Failure, Head};
use crate::session::{self, Rotation, Titles};
use crate::wire::{ChatRequest, ChatResponse, ErrorBody, Message};

/// Concurrent `claude` turns. Two, because this is one person's desktop and a
/// third simultaneous spawn is a symptom, not a workload.
const PERMITS: usize = 2;

/// How many conversations the title map remembers.
const TITLE_CAPACITY: usize = 256;

/// The shared outcome of one in-flight turn, handed to every duplicate caller.
type Shared = Option<Arc<Result<String, Failure>>>;

/// Everything one process needs to answer requests.
pub struct Bridge {
    backend: Backend,
    titles: Mutex<Titles>,
    permits: Semaphore,
    inflight: Mutex<HashMap<u64, watch::Receiver<Shared>>>,
    /// Serialises the session-rotation path (#667) — the decision **and** the
    /// turn that mints the replacement.
    ///
    /// Never taken on the healthy path, and a rotation happens about once per
    /// context window (~10³ turns), so it costs nothing. It exists because two
    /// requests really can overflow the same session at once: [`PERMITS`] is 2,
    /// and single-flight only collapses *identical* transcripts, so pet's tick
    /// and a manual poke are two different keys resolving to one title. Without
    /// the lock both would compute the same successor, both find it missing,
    /// and both `Attach::Create` it — two rival sessions under one title, which
    /// `--resume` would then pick between arbitrarily. Under it, the second one
    /// re-resolves, sees the replacement already recorded, and joins it.
    rotation: tokio::sync::Mutex<()>,
    budget: Duration,
}

impl Bridge {
    /// Build a bridge over `backend`, allowing `budget` per request.
    ///
    /// `budget` must stay **under** the client's global timeout
    /// (`hytte-ai-providers` uses 10s), so a slow turn reaches the caller as a
    /// clean 504 it can fall back from rather than as a connection-level
    /// failure mid-read.
    #[must_use]
    pub fn new(backend: Backend, budget: Duration) -> Self {
        Self {
            backend,
            titles: Mutex::new(Titles::new(TITLE_CAPACITY)),
            permits: Semaphore::new(PERMITS),
            inflight: Mutex::new(HashMap::new()),
            rotation: tokio::sync::Mutex::new(()),
            budget,
        }
    }

    /// Answer one parsed request. Returns the status and the JSON body.
    pub async fn handle(&self, head: &Head, body: &[u8]) -> (u16, Vec<u8>) {
        if head.path != http::ROUTE {
            return error_response(
                404,
                &format!(
                    "no route {}; this bridge serves only POST {}",
                    head.path,
                    http::ROUTE
                ),
            );
        }
        if head.method != "POST" {
            return error_response(
                405,
                &format!("{} is not allowed here; use POST", head.method),
            );
        }
        let req: ChatRequest = match serde_json::from_slice(body) {
            Ok(req) => req,
            Err(e) => return error_response(400, &format!("bad request body: {e}")),
        };
        if req.messages.is_empty() {
            return error_response(400, "`messages` must not be empty");
        }
        // The sampling knobs, named in the journal rather than silently
        // dropped. `max_tokens` now rides down to the backend — the Messages
        // API requires it and honours it (#730), while both `claude` arms still
        // ignore it because the CLI exposes no such flag. `temperature` and the
        // llama-only template kwarg are still accepted-and-ignored everywhere:
        // the CLI has no sampling knob, and the Messages API *rejects*
        // `temperature` outright on the current models.
        tracing::debug!(
            max_tokens = ?req.max_tokens,
            temperature = ?req.temperature,
            enable_thinking = ?req.chat_template_kwargs.and_then(|k| k.enable_thinking),
            "accepted the sampling knobs",
        );

        match self.complete(&req.messages, req.max_tokens).await {
            Ok(text) => ok_response(text, req.model, &req.messages),
            Err(f) => {
                tracing::warn!(status = f.status, message = %f.message, "request failed");
                error_response(f.status, &f.message)
            }
        }
    }

    /// Single-flight: identical concurrent transcripts share one backend turn.
    ///
    /// pet's tick and a manual poke can land together on the same prompt, and
    /// paying twice for the same answer is pure waste — of a subscription that
    /// is rate-limited, and, on the API backend, of real money.
    ///
    /// The key is the transcript alone, deliberately: `max_tokens` is a cap on
    /// the answer, not part of the question, and keying on it would split two
    /// otherwise-identical in-flight requests into two paid turns.
    async fn complete(
        &self,
        messages: &[Message],
        max_tokens: Option<u32>,
    ) -> Result<String, Failure> {
        let key = session::transcript_key(messages);
        let leader = {
            let mut inflight = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(rx) = inflight.get(&key) {
                Err(rx.clone())
            } else {
                let (tx, rx) = watch::channel(None);
                inflight.insert(key, rx);
                Ok(tx)
            }
        };
        match leader {
            Err(rx) => follow(rx).await,
            Ok(tx) => {
                // Removes the map entry even if this task is cancelled
                // mid-turn, so a disconnected client can't wedge the key.
                let _guard = FlightGuard {
                    inflight: &self.inflight,
                    key,
                };
                let outcome = self.run_turn(messages, max_tokens).await;
                let _ = tx.send(Some(Arc::new(outcome.clone())));
                outcome
            }
        }
    }

    /// Run one turn: shed load, derive the identity, spend the budget, record.
    async fn run_turn(
        &self,
        messages: &[Message],
        max_tokens: Option<u32>,
    ) -> Result<String, Failure> {
        let Ok(_permit) = self.permits.try_acquire() else {
            return Err(Failure::new(
                503,
                format!("bridge busy: {PERMITS} turns already running"),
            ));
        };
        let Some((_prefix, delta)) = session::split_delta(messages) else {
            return Err(Failure::new(400, "`messages` must not be empty"));
        };
        let title = self
            .titles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resolve(messages);
        tracing::debug!(title = %title, turns = messages.len(), "answering");

        // ONE budget for the whole request, rotation included: the client's own
        // global timeout does not restart just because the bridge retried.
        match tokio::time::timeout(
            self.budget,
            self.answer(messages, delta, &title, max_tokens),
        )
        .await
        {
            Ok(reply) => reply,
            Err(_elapsed) => {
                // The driver's own idle watchdog sits a second under this and
                // kills the child on silence, so reaching here means the turn
                // was still streaming — `hive-claude` sets no `kill_on_drop`,
                // so that child may outlive this request. Said out loud rather
                // than papered over.
                tracing::warn!(
                    title = %title,
                    budget_s = self.budget.as_secs(),
                    "budget expired while claude was still streaming; the child may still be running",
                );
                Err(Failure::new(
                    504,
                    format!(
                        "no answer within the bridge's {}s budget",
                        self.budget.as_secs()
                    ),
                ))
            }
        }
    }

    /// Answer one turn, retiring the session first if it has filled up (#667).
    ///
    /// **Which path sends what** — the delta rule is untouched, and both arms
    /// of it are reached through the same `respond`:
    ///
    /// - the ordinary turn resumes `title` and sends **only the delta**;
    /// - a rotation names a session that has never been resumed, so the
    ///   backend's resume misses with `SessionNotFound` and its create arm
    ///   replays the **whole transcript** — the persona prefix the fresh
    ///   session needs in order to be the same character as the old one.
    ///
    /// At most **one** rotation per request, by construction: the retry calls
    /// `respond` and not this function. An overflow on the *replacement* means
    /// the client's own transcript does not fit, which no further rotation can
    /// fix, so it is reported.
    async fn answer(
        &self,
        messages: &[Message],
        delta: &Message,
        title: &str,
        max_tokens: Option<u32>,
    ) -> Result<String, Failure> {
        let failure = match self.respond(messages, delta, title, max_tokens).await {
            Ok(reply) => return Ok(self.record(title, messages, reply)),
            Err(f) => f,
        };
        if !self.backend.is_persisted() {
            return Err(failure);
        }
        // Pure, cheap, and the only outcome worth taking the rotation lock for.
        match session::rotation_for(&failure, title) {
            Rotation::Keep => return Err(failure),
            Rotation::Stuck => {
                tracing::error!(
                    title = %title,
                    "this session is past the context window and its title carries no generation \
                     the bridge can rotate; the conversation is stuck until it is deleted by hand",
                );
                return Err(failure);
            }
            Rotation::To(_) => {}
        }

        // Held across the replacement's first turn, not merely across the
        // decision — see the field docs. Only ever reached on an overflow.
        let _rotating = self.rotation.lock().await;
        let Some(next) = self.retire(messages, title) else {
            return Err(failure);
        };
        let reply = self.respond(messages, delta, &next, max_tokens).await?;
        Ok(self.record(&next, messages, reply))
    }

    /// One backend turn under `title`.
    async fn respond(
        &self,
        messages: &[Message],
        delta: &Message,
        title: &str,
        max_tokens: Option<u32>,
    ) -> Result<String, Failure> {
        self.backend
            .respond(Turn {
                title,
                transcript: messages,
                delta,
                max_tokens,
            })
            .await
    }

    /// Record that `title`'s session is retired, and return the title this
    /// conversation continues under. `None` if it turned out not to be
    /// rotatable after all.
    ///
    /// Callers must hold [`Bridge::rotation`]. Synchronous on purpose: the
    /// retirement lands **before** the replacement turn runs, so a request
    /// whose budget expires mid-retry — the likely outcome, since an overflow
    /// is only discovered after claude has posted the oversized prompt — still
    /// leaves the *next* request pointed at the fresh session instead of back
    /// at the full one. A rotation that only became durable on success would
    /// leave the bridge exactly as stuck as it is today.
    fn retire(&self, messages: &[Message], title: &str) -> Option<String> {
        let mut titles = self.titles.lock().unwrap_or_else(PoisonError::into_inner);
        // Re-resolve under the lock: a request that raced us into the same
        // overflow may already have retired this session, and riding its
        // replacement is exactly what stops us minting a rival.
        let current = titles.resolve(messages);
        if current != title {
            tracing::info!(
                from = %title,
                to = %current,
                "session already retired by a concurrent request; joining its replacement",
            );
            return Some(current);
        }
        let next = session::next_title(&current)?;
        titles.rotate_to(messages, &next);
        tracing::warn!(
            from = %title,
            to = %next,
            turns = messages.len(),
            "claude session is past the context window: retiring it and continuing in a fresh \
             session, which replays the transcript (its prompt cache starts cold)",
        );
        Some(next)
    }

    /// Remember the title this conversation answered under, so its next turn
    /// resolves to the same session.
    fn record(&self, title: &str, messages: &[Message], reply: String) -> String {
        self.titles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remember(title, messages, &reply);
        reply
    }
}

/// Removes an in-flight entry on drop, cancellation included.
struct FlightGuard<'a> {
    inflight: &'a Mutex<HashMap<u64, watch::Receiver<Shared>>>,
    key: u64,
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        self.inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
    }
}

/// Wait for the leader's outcome.
async fn follow(mut rx: watch::Receiver<Shared>) -> Result<String, Failure> {
    loop {
        let current = rx.borrow_and_update().clone();
        if let Some(shared) = current {
            return (*shared).clone();
        }
        if rx.changed().await.is_err() {
            return Err(Failure::new(
                503,
                "the identical in-flight request ended without a result",
            ));
        }
    }
}

/// Seconds since the unix epoch, saturating rather than panicking on a clock
/// before 1970.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A 200 carrying the single-choice envelope.
fn ok_response(text: String, model: Option<String>, messages: &[Message]) -> (u16, Vec<u8>) {
    let created = unix_now();
    let seed = session::transcript_key(messages) ^ created;
    let body = ChatResponse::single(text, model, created, seed);
    match serde_json::to_vec(&body) {
        Ok(bytes) => (200, bytes),
        Err(e) => error_response(502, &format!("could not encode the response: {e}")),
    }
}

/// An error status with the OpenAI-shaped error body.
fn error_response(status: u16, message: &str) -> (u16, Vec<u8>) {
    let body = serde_json::to_vec(&ErrorBody::new(message))
        .unwrap_or_else(|_| br#"{"error":{"message":"internal","type":"bridge_error"}}"#.to_vec());
    (status, body)
}

/// Read one request off `stream`, answer it, close.
pub async fn serve_connection(bridge: &Bridge, mut stream: TcpStream) {
    let (status, body) = match http::read_request(&mut stream).await {
        Ok((head, body)) => bridge.handle(&head, &body).await,
        Err(f) => error_response(f.status, &f.message),
    };
    http::write_response(&mut stream, status, &body).await;
}

#[cfg(test)]
mod tests {
    use super::{Bridge, error_response, ok_response};
    use crate::backend::{Backend, Reprompt};
    use crate::http::Head;
    use crate::wire::Message;
    use std::time::Duration;

    /// A bridge whose backend is never reached — every test below short-circuits
    /// before any `claude` spawn.
    fn bridge() -> Bridge {
        Bridge::new(
            Backend::Reprompt(Reprompt::new(hive_claude::Config::default())),
            Duration::from_secs(8),
        )
    }

    fn head(method: &str, path: &str) -> Head {
        Head {
            method: method.to_owned(),
            path: path.to_owned(),
            content_length: 0,
            body_offset: 0,
        }
    }

    #[tokio::test]
    async fn an_unknown_route_is_a_404_naming_the_real_one() {
        let (status, body) = bridge()
            .handle(&head("POST", "/v1/completions"), b"{}")
            .await;
        assert_eq!(status, 404);
        assert!(String::from_utf8_lossy(&body).contains("/v1/chat/completions"));
    }

    #[tokio::test]
    async fn a_get_on_the_route_is_a_405() {
        let (status, _) = bridge()
            .handle(&head("GET", "/v1/chat/completions"), b"")
            .await;
        assert_eq!(status, 405);
    }

    #[tokio::test]
    async fn an_unparseable_body_is_a_400_with_the_reason() {
        let (status, body) = bridge()
            .handle(&head("POST", "/v1/chat/completions"), b"not json")
            .await;
        assert_eq!(status, 400);
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("bad request body"), "{text}");
    }

    #[tokio::test]
    async fn an_empty_messages_array_is_a_400() {
        let (status, body) = bridge()
            .handle(&head("POST", "/v1/chat/completions"), br#"{"messages":[]}"#)
            .await;
        assert_eq!(status, 400);
        assert!(String::from_utf8_lossy(&body).contains("must not be empty"));
    }

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.to_owned(),
            content: content.to_owned(),
        }
    }

    /// Retiring a full session points the conversation at a successor and makes
    /// that stick — the recovery from #667's permanent 413. Driven directly
    /// because nothing hermetic can push a real claude session over its context
    /// window; `session.rs` pins the decision itself.
    #[test]
    fn retiring_a_full_session_points_the_conversation_at_a_successor() {
        let bridge = bridge();
        let convo = vec![msg("system", "you are a cat"), msg("user", "poke")];
        let title = bridge.titles.lock().expect("unpoisoned").resolve(&convo);

        let next = bridge.retire(&convo, &title).expect("rotatable");
        assert_eq!(next, format!("{title}-g1"));
        // The next poke — a different newest message, same conversation — must
        // land in the replacement rather than back in the session that just
        // overflowed.
        let later = vec![msg("system", "you are a cat"), msg("user", "poke again")];
        assert_eq!(
            bridge.titles.lock().expect("unpoisoned").resolve(&later),
            next
        );
    }

    /// Two requests racing into the same overflow must end up in **one**
    /// replacement session: the second retirement joins the first rather than
    /// minting a rival that `--resume` would then pick between arbitrarily.
    /// (In the running bridge both calls are serialised by `Bridge::rotation`;
    /// this asserts the decision they make once serialised.)
    #[test]
    fn a_concurrent_retirement_joins_the_replacement_instead_of_minting_a_rival() {
        let bridge = bridge();
        let convo = vec![msg("system", "persona"), msg("user", "one")];
        let title = bridge.titles.lock().expect("unpoisoned").resolve(&convo);

        let first = bridge.retire(&convo, &title).expect("rotatable");
        // The loser of the race still holds the pre-rotation title.
        let second = bridge.retire(&convo, &title).expect("rotatable");
        assert_eq!(second, first, "the race minted a second session");
        assert_eq!(
            bridge.titles.lock().expect("unpoisoned").resolve(&convo),
            first
        );
    }

    /// Error bodies are the `OpenAI` error shape, so a client that parses errors
    /// sees a `message` where it expects one.
    #[test]
    fn error_bodies_are_openai_shaped() {
        let (status, body) = error_response(429, "slow down");
        assert_eq!(status, 429);
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(json["error"]["message"], "slow down");
        assert_eq!(json["error"]["type"], "bridge_error");
    }

    /// A success body carries the content at the one path `chat()` reads, and
    /// echoes the requested model.
    #[test]
    fn success_bodies_carry_the_content_and_echo_the_model() {
        let messages = vec![Message {
            role: "user".to_owned(),
            content: "hi".to_owned(),
        }];
        let (status, body) = ok_response("hello".to_owned(), Some("opus".to_owned()), &messages);
        assert_eq!(status, 200);
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(json["choices"][0]["message"]["content"], "hello");
        assert_eq!(json["model"], "opus");
    }
}
