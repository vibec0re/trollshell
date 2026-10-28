//! Request handling: routing, load shedding, single-flight, and the title map
//! that ties the two backends to one conversation identity.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{Semaphore, watch};
use tokio::time::Instant;

use crate::backend::{Backend, Conversation as _, REAP_GRACE, Turn, expired};
use crate::http::{self, Failure, Head};
use crate::session::{self, Key, Rotation, Titles};
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
    inflight: Mutex<HashMap<Key, watch::Receiver<Shared>>>,
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
    /// failure mid-read. A `claude` child still running at the budget is
    /// killed and reaped before that 504 is sent, which can add up to
    /// [`REAP_GRACE`] — so it is the sum that has to fit.
    ///
    /// `retired_store` is where the session retirements survive a restart
    /// (#855) — `None` keeps the whole title map in memory, which is what the
    /// tests below want and what the pre-#855 bridge did. It is passed in
    /// rather than resolved here so the state dir stays `main`'s single
    /// decision (`CLAUDE_BRIDGE_STATE_DIR`), and so nothing in this module has
    /// to reach for the environment to be testable.
    #[must_use]
    pub fn new(backend: Backend, budget: Duration, retired_store: Option<PathBuf>) -> Self {
        Self {
            backend,
            titles: Mutex::new(match retired_store {
                Some(store) => Titles::persisted(TITLE_CAPACITY, store),
                None => Titles::new(TITLE_CAPACITY),
            }),
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
            user = ?req.user,
            "accepted the sampling knobs",
        );

        match self
            .complete(&req.messages, req.user.as_deref(), req.max_tokens)
            .await
        {
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
    /// The turn belongs to the **leader**, the first of the identical requests
    /// to arrive. If the leader's request is dropped mid-turn — its client hung
    /// up, say — the turn goes with it, and every follower gets a 503 rather
    /// than wait for an answer that is no longer coming, even a follower whose
    /// own client is still connected.
    ///
    /// The key is the transcript **and the caller's identity**, deliberately:
    /// `max_tokens` is a cap on the answer, not part of the question, and keying
    /// on it would split two otherwise-identical in-flight requests into two
    /// paid turns — while two *different* callers (#704) are two conversations
    /// in two sessions, so sharing one answer between them would undo at the
    /// reply what the identity separated at the session. A request with no
    /// `user` keys exactly as it did before #704.
    async fn complete(
        &self,
        messages: &[Message],
        user: Option<&str>,
        max_tokens: Option<u32>,
    ) -> Result<String, Failure> {
        let key = session::flight_key(messages, user);
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
                // Removes the map entry however this turn ends — answered, or
                // dropped mid-turn by a client that hangs up
                // (`serve_connection` watches for it, #1550), the request
                // backstop, a panic, or shutdown — so no ending can wedge the
                // key. A drop takes `tx` with it, which is what tells the
                // followers (see above).
                let _guard = FlightGuard {
                    inflight: &self.inflight,
                    key,
                };
                let outcome = self.run_turn(messages, user, max_tokens).await;
                let _ = tx.send(Some(Arc::new(outcome.clone())));
                outcome
            }
        }
    }

    /// Run one turn: shed load, derive the identity, spend the budget, record.
    async fn run_turn(
        &self,
        messages: &[Message],
        user: Option<&str>,
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
            .resolve(messages, user);
        tracing::debug!(title = %title, turns = messages.len(), "answering");

        // ONE budget for the whole request, rotation included: the client's own
        // global timeout does not restart just because the bridge retried.
        //
        // It is a *deadline* handed down, not a timeout wrapped around
        // `answer`, and that difference is the fix for a hung `claude` being
        // orphaned: a wrapper's only move is to drop the future, and the
        // `hive-claude` turn sets no `kill_on_drop` on its child — so a drop
        // can at best kill the child (`backend::run_until`'s guard does, #1550)
        // and never wait for it, releasing this permit and the title lock
        // before it is reaped. With the deadline below, the backend kills and
        // reaps its own child, so by the time `answer` returns there is nothing
        // left running, and only then does `_permit` drop.
        let deadline = Instant::now() + self.budget;
        // The backstop. Every wait inside `answer` is bounded by `deadline`
        // itself, and the one that owns a child gives it at most `REAP_GRACE`
        // past it to die, so this cannot fire while they hold — it is here so
        // that an unbounded await added later costs one dropped turn — its
        // child killed but not waited for — said loudly, rather than a permit
        // wedged forever.
        let backstop = deadline + REAP_GRACE * 2;
        match tokio::time::timeout_at(
            backstop,
            self.answer(messages, delta, &title, user, max_tokens, deadline),
        )
        .await
        {
            Ok(reply) => reply,
            Err(_elapsed) => {
                tracing::error!(
                    title = %title,
                    budget_s = self.budget.as_secs(),
                    "a turn outlived its deadline by more than the reap grace — something in it \
                     is not deadline-bounded; any claude child it held was killed on the drop \
                     but is left to tokio to reap",
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
        user: Option<&str>,
        max_tokens: Option<u32>,
        deadline: Instant,
    ) -> Result<String, Failure> {
        let failure = match self
            .respond(messages, delta, title, max_tokens, deadline)
            .await
        {
            Ok(reply) => return Ok(self.record(title, messages, user, reply)),
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
        // Waiting for it is bounded by the same deadline as everything else in
        // the request; the holder's own turn is, too.
        let Ok(_rotating) = tokio::time::timeout_at(deadline, self.rotation.lock()).await else {
            return Err(expired());
        };
        let Some(next) = self.retire(messages, user, title) else {
            return Err(failure);
        };
        let reply = self
            .respond(messages, delta, &next, max_tokens, deadline)
            .await?;
        Ok(self.record(&next, messages, user, reply))
    }

    /// One backend turn under `title`.
    async fn respond(
        &self,
        messages: &[Message],
        delta: &Message,
        title: &str,
        max_tokens: Option<u32>,
        deadline: Instant,
    ) -> Result<String, Failure> {
        self.backend
            .respond(Turn {
                title,
                transcript: messages,
                delta,
                max_tokens,
                deadline,
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
    fn retire(&self, messages: &[Message], user: Option<&str>, title: &str) -> Option<String> {
        let mut titles = self.titles.lock().unwrap_or_else(PoisonError::into_inner);
        // Re-resolve under the lock: a request that raced us into the same
        // overflow may already have retired this session, and riding its
        // replacement is exactly what stops us minting a rival.
        let current = titles.resolve(messages, user);
        if current != title {
            tracing::info!(
                from = %title,
                to = %current,
                "session already retired by a concurrent request; joining its replacement",
            );
            return Some(current);
        }
        let next = session::next_title(&current)?;
        titles.rotate_to(messages, user, &next);
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
    fn record(
        &self,
        title: &str,
        messages: &[Message],
        user: Option<&str>,
        reply: String,
    ) -> String {
        self.titles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remember(title, messages, user, &reply);
        reply
    }
}

/// Removes an in-flight entry on drop, cancellation included.
struct FlightGuard<'a> {
    inflight: &'a Mutex<HashMap<Key, watch::Receiver<Shared>>>,
    key: Key,
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

/// What the status board is told about a request whose client hung up before
/// its answer: nginx's "client closed request". It never goes on the wire, and
/// only the `delivered: false` beside it decides the board's classification —
/// an error, as a hang-up has always counted (it used to surface as the failed
/// write of an answer nobody read).
const HUNG_UP: u16 = 499;

/// Read one request off `stream`, answer it, close.
///
/// This is also where the answer is counted for the plugin face's status chip
/// (#866) — at the connection boundary rather than inside [`Bridge::handle`], so
/// a malformed request the parser rejects is counted too. The tally is the one
/// thing done here rather than in [`serve`], so the unit tests below, which
/// drive whole connections through `serve`, never touch the process-global
/// board.
///
/// A [`UnixStream`](tokio::net::UnixStream) rather than any [`http::Stream`]
/// since #1550, because [`http::hung_up`] reads the socket's own readiness,
/// which only a socket has. The request and the response still go through the
/// generic [`http::read_request`] and [`http::write_response`], so the bytes on
/// the wire are the ones #993 settled.
///
/// Three things are deliberately **not** counted as served:
///
/// - a connection that closed before sending a byte — no request was made, so
///   it gets no response and no tally. That is a second bridge's startup probe
///   ([`crate::socket::socket_in_use`]), which used to paint a warning on the
///   incumbent's chip every time a duplicate start was refused;
/// - a 2xx whose response never reached the client. The tally is taken *after*
///   the bounded write (#1481) rather than before it, and an answer the client
///   did not get is, from where the client sits, a failure — it fell back;
/// - a request whose client hung up before its answer (see [`serve`]), counted
///   as an undelivered [`HUNG_UP`] for the same reason.
pub async fn serve_connection(bridge: &Bridge, stream: tokio::net::UnixStream) {
    if let Some((status, delivered)) = serve(bridge, stream).await {
        crate::status::record(status, delivered);
    }
}

/// Everything [`serve_connection`] does except the tally: `None` for a peer
/// that sent no request, otherwise the status the board should hear and
/// whether the response reached the client.
///
/// **A client that hangs up stops its turn** (#1550). While [`Bridge::handle`]
/// runs, [`http::hung_up`] watches the connection, and if the client goes away
/// first the handler is dropped and nothing is written. Dropping the handler
/// drops the turn, through the same path as any other dropped turn:
/// `backend::run_until`'s guard sends the `claude` child `SIGKILL` at once,
/// then the title lock and the request permit are released as the futures
/// holding them drop, and the reap is left to tokio — kill first, release
/// second, the order #1623's review found safe for the next `--resume`. A
/// request still queued, for its title or behind its single-flight leader, has
/// spawned nothing and only leaves the queue. Before this nothing watched the
/// socket, so a turn whose client had left ran on — spending the owner's
/// subscription — until its deadline killed it.
///
/// A client that only **half-closed** (shut its write side after the request)
/// is still reading, keeps its turn and gets its answer: `hung_up` waits for
/// the peer to be gone, which a half-close is not.
///
/// `biased` towards the watch, so a client that has already gone when the
/// request is parsed is noticed before the handler's first poll can spawn
/// anything.
async fn serve(bridge: &Bridge, mut stream: tokio::net::UnixStream) -> Option<(u16, bool)> {
    let (status, body) = match http::read_request(&mut stream).await {
        Ok(Some((head, body))) => tokio::select! {
            biased;
            () = http::hung_up(&stream) => {
                tracing::info!("the client hung up before its answer; dropped its request");
                return Some((HUNG_UP, false));
            }
            answer = bridge.handle(&head, &body) => answer,
        },
        Ok(None) => return None,
        Err(f) => error_response(f.status, &f.message),
    };
    let delivered = http::write_response(&mut stream, status, &body).await;
    Some((status, delivered))
}

#[cfg(test)]
mod tests {
    use super::{Bridge, error_response, ok_response};
    use crate::backend::{Backend, Reprompt, Subscription};
    use crate::http::Head;
    use crate::wire::Message;
    use std::time::Duration;

    /// A bridge whose backend is never reached — every test below short-circuits
    /// before any `claude` spawn.
    ///
    /// `None` for the retired store: these tests touch no disk, and the store's
    /// own behaviour is pinned in `crate::retired` and `crate::session`.
    fn bridge() -> Bridge {
        Bridge::new(
            Backend::Reprompt(Reprompt::new(hive_claude::Config::default())),
            Duration::from_secs(8),
            None,
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
        let title = bridge
            .titles
            .lock()
            .expect("unpoisoned")
            .resolve(&convo, None);

        let next = bridge.retire(&convo, None, &title).expect("rotatable");
        assert_eq!(next, format!("{title}-g1"));
        // The next poke — a different newest message, same conversation — must
        // land in the replacement rather than back in the session that just
        // overflowed.
        let later = vec![msg("system", "you are a cat"), msg("user", "poke again")];
        assert_eq!(
            bridge
                .titles
                .lock()
                .expect("unpoisoned")
                .resolve(&later, None),
            next
        );
    }

    /// The same recovery must work for a conversation that named itself (#704):
    /// an identity title fills up exactly like a hash-derived one, so if it
    /// could not rotate, opting in would trade a silent collision for a
    /// permanent 413.
    #[test]
    fn an_identified_conversation_retires_to_a_successor_too() {
        let bridge = bridge();
        let convo = vec![msg("system", "you are a cat"), msg("user", "poke")];
        let user = Some("pet");
        let title = bridge
            .titles
            .lock()
            .expect("unpoisoned")
            .resolve(&convo, user);

        let next = bridge.retire(&convo, user, &title).expect("rotatable");
        assert_eq!(next, format!("{title}-g1"));
        // A wholly different transcript under the same identity is the same
        // conversation, and must land in the replacement.
        let later = vec![
            msg("system", "you are a cat"),
            msg("user", "something else"),
        ];
        assert_eq!(
            bridge
                .titles
                .lock()
                .expect("unpoisoned")
                .resolve(&later, user),
            next
        );
        // …and the identity-less conversation with that same transcript is
        // untouched by any of it.
        assert_ne!(
            bridge
                .titles
                .lock()
                .expect("unpoisoned")
                .resolve(&convo, None),
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
        let title = bridge
            .titles
            .lock()
            .expect("unpoisoned")
            .resolve(&convo, None);

        let first = bridge.retire(&convo, None, &title).expect("rotatable");
        // The loser of the race still holds the pre-rotation title.
        let second = bridge.retire(&convo, None, &title).expect("rotatable");
        assert_eq!(second, first, "the race minted a second session");
        assert_eq!(
            bridge
                .titles
                .lock()
                .expect("unpoisoned")
                .resolve(&convo, None),
            first
        );
    }

    // ---- a hung claude is reaped before its permit and title are released ---

    /// A subscription bridge whose `claude` is `backend::stand_in`'s silent
    /// hang, writing its traces into `dir`.
    fn hung_bridge(dir: &std::path::Path, budget: Duration) -> Bridge {
        let config = hive_claude::Config {
            program: Some(crate::backend::stand_in::hung_claude(dir)),
            ..hive_claude::Config::default()
        };
        Bridge::new(
            Backend::Subscription(Subscription::new(config)),
            budget,
            None,
        )
    }

    /// One request under the identity `user` (#704), so two calls with the
    /// same `user` and different `text` are one conversation — one title, one
    /// title lock — but two flight keys, and single-flight does not merge them.
    async fn ask(bridge: &Bridge, user: &str, text: &str) -> u16 {
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": text}],
            "user": user,
        })
        .to_string();
        bridge
            .handle(&head("POST", crate::http::ROUTE), body.as_bytes())
            .await
            .0
    }

    /// **The title lock outlives the child, never the reverse.** Two turns of
    /// one conversation: the first hangs; the second queues on its title.
    ///
    /// Before the fix the request budget dropped the first turn at the
    /// deadline, which released its title lock with its `claude` still
    /// running — so the second turn `--resume`d a session that the orphan
    /// still had open, the transcript fork #693's lock exists to prevent.
    /// Pinned here two ways: the first child is out of `/proc` (killed **and**
    /// reaped) the moment its 504 is back, and the second child — which can
    /// only start once the lock is free — found no trace of it when it did.
    #[tokio::test]
    async fn a_hung_turn_is_reaped_before_its_title_is_released() {
        use crate::backend::stand_in;
        use std::cell::Cell;

        let dir = tempfile::tempdir().expect("tempdir");
        let bridge = hung_bridge(dir.path(), Duration::from_secs(2));
        let first_pid = Cell::new(None);

        let first = async {
            let status = ask(&bridge, "pet", "one").await;
            let pid = first_pid.get().expect("the first child started");
            assert!(
                !stand_in::present(pid),
                "the first child ({pid}) is still in /proc after its 504",
            );
            status
        };
        let second = async {
            let [pid] = stand_in::wait_for(dir.path(), 1).await[..] else {
                panic!("one child before the second turn is sent");
            };
            first_pid.set(Some(pid));
            // Halfway into the first turn's budget: sent while its child is
            // certainly still running, and early enough that this turn's own
            // deadline leaves its child a whole second to start once the lock
            // frees, however loaded the machine running the suite.
            tokio::time::sleep(Duration::from_secs(1)).await;
            ask(&bridge, "pet", "two").await
        };
        let (first, second) = tokio::join!(first, second);

        assert_eq!((first, second), (504, 504));
        let pids = stand_in::started(dir.path());
        let first = first_pid.get().expect("set");
        let [second] = pids[..]
            .iter()
            .copied()
            .filter(|&p| p != first)
            .collect::<Vec<_>>()[..]
        else {
            panic!("the second turn never started a child: {pids:?}");
        };
        assert!(
            !stand_in::saw(dir.path(), second).contains(&first),
            "the second turn's claude started while the first's was still in /proc",
        );
        assert_eq!(bridge.permits.available_permits(), super::PERMITS);
    }

    /// **The permit bounds live children again.** Fill both permits with hung
    /// turns, then keep knocking with a third until one is free: the third
    /// child must find fewer than [`PERMITS`](super::PERMITS) others still in
    /// `/proc` when it starts.
    ///
    /// Before the fix a permit came back at the deadline while its child slept
    /// on, so the third turn started beside **both** earlier children — and a
    /// steady stream of stalled turns grew the population without bound.
    #[tokio::test]
    async fn hung_turns_never_leave_more_live_children_than_permits() {
        use crate::backend::stand_in;

        let dir = tempfile::tempdir().expect("tempdir");
        // Two seconds, so every child — the third included, which starts only
        // once a permit frees — has room to reach its marker before its kill.
        let bridge = hung_bridge(dir.path(), Duration::from_secs(2));
        let earlier = std::cell::RefCell::new(Vec::new());

        let third = async {
            *earlier.borrow_mut() = stand_in::wait_for(dir.path(), 2).await;
            loop {
                let status = ask(&bridge, "caw", "hi").await;
                if status != 503 {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let statuses = tokio::join!(ask(&bridge, "pet", "hi"), ask(&bridge, "cat", "hi"), third);

        assert_eq!(statuses, (504, 504, 504));
        let earlier = earlier.into_inner();
        let pids = stand_in::started(dir.path());
        let [third] = pids[..]
            .iter()
            .copied()
            .filter(|p| !earlier.contains(p))
            .collect::<Vec<_>>()[..]
        else {
            panic!("the third turn never started a child: {pids:?} after {earlier:?}");
        };
        let alongside = stand_in::saw(dir.path(), third);
        assert!(
            alongside.len() < super::PERMITS,
            "the third claude started beside {alongside:?}: more than {} children at once",
            super::PERMITS,
        );
        assert_eq!(bridge.permits.available_permits(), super::PERMITS);
    }

    // ---- a client that hangs up stops its turn (#1550) ----------------------

    /// A budget nobody reaches in these tests, so that only a hang-up can end
    /// a hung turn early — and long enough that [`REAP_SLACK`] is "well before
    /// the deadline" even on a loaded runner.
    const HANG_UP_BUDGET: Duration = Duration::from_secs(20);

    /// How long a child may take to leave `/proc` — killed **and** reaped —
    /// after its client hung up: generous for a loaded runner, and still a
    /// quarter of [`HANG_UP_BUDGET`].
    const REAP_SLACK: Duration = Duration::from_secs(5);

    /// What `backend::KillOnDrop` says when it fires: the same fragment as
    /// `backend.rs`'s own `ABANDONED`, so a reworded line turns the positive
    /// counts below red rather than the zeroes silently vacuous.
    const ABANDONED: &str = "claude's turn was dropped before it finished";

    /// A stand-in `claude` that reads its prompt, leaves the `<pid>.pid`
    /// marker [`stand_in::started`](crate::backend::stand_in::started) looks
    /// for, and then answers only once a `go` file appears beside it — so a
    /// test can hold the turn open across whatever it does to the client, and
    /// see the turn survive it.
    const GATED: &str = r#"#!/bin/sh
dir="${0%/*}"
cat > /dev/null
: > "$dir/$$.pid"
while [ ! -e "$dir/go" ]; do sleep 0.05; done
echo '{"type":"result","subtype":"success","is_error":false,"result":"answered"}'
"#;

    /// A subscription bridge running `program` as its `claude`.
    fn bridge_running(program: std::path::PathBuf) -> Bridge {
        let config = hive_claude::Config {
            program: Some(program),
            ..hive_claude::Config::default()
        };
        Bridge::new(
            Backend::Subscription(Subscription::new(config)),
            HANG_UP_BUDGET,
            None,
        )
    }

    /// The bytes one `chat()` call by `user` puts on the socket — the
    /// `ureq`-shaped head `http.rs`'s parser tests pin.
    fn request(user: &str, text: &str) -> Vec<u8> {
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": text}],
            "user": user,
        })
        .to_string();
        format!(
            "POST {} HTTP/1.1\r\nhost: localhost\r\ncontent-type: application/json\r\n\
             content-length: {}\r\n\r\n{body}",
            crate::http::ROUTE,
            body.len(),
        )
        .into_bytes()
    }

    /// How many times `captured` heard the abandoned-turn guard.
    fn abandoned(captured: &hytte_config::test_support::Captured) -> usize {
        captured
            .warnings()
            .iter()
            .filter(|message| message.contains(ABANDONED))
            .count()
    }

    /// Wait, bounded by [`REAP_SLACK`] from `since`, until `pid` is out of
    /// `/proc`: killed **and** reaped. A poll, because the reap of a dropped
    /// turn's child falls to tokio's orphan reaper on a later `SIGCHLD`.
    async fn gone_within_slack(pid: u32, since: tokio::time::Instant, after: &str) {
        while crate::backend::stand_in::present(pid) {
            assert!(
                since.elapsed() < REAP_SLACK,
                "the child ({pid}) is still in /proc {REAP_SLACK:?} after {after} — running on \
                 until its deadline, or killed but never reaped",
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// **A client that hangs up takes its turn with it (#1550).** The client
    /// sends a request over a real Unix socket, then closes it outright while
    /// `claude` is still working: the child is killed and reaped well before
    /// the deadline, and the turn's title lock and request permit come back.
    ///
    /// Before the fix `serve_connection` awaited the answer without watching
    /// the socket, so the stand-in here slept on — on the owner's
    /// subscription — until the 20 s deadline killed it (#1623's review
    /// measured the same thing at a 3 s budget).
    ///
    /// The title lock is proved by a second turn of the same conversation (same
    /// `user`, so the same title, but a different transcript, so single-flight
    /// does not merge them): sent while the first runs, it cannot start a child
    /// until the first lets go, and it must start one once the first client
    /// hangs up. It is then hung up in turn, so both dropped turns are counted.
    #[tokio::test]
    async fn a_client_that_hangs_up_mid_turn_stops_its_turn() {
        use crate::backend::stand_in;
        use std::cell::Cell;
        use tokio::io::AsyncWriteExt as _;
        use tokio::net::UnixStream;
        use tokio::time::Instant;

        let dir = tempfile::tempdir().expect("tempdir");
        let bridge = hung_bridge(dir.path(), HANG_UP_BUDGET);
        let (captured, _scope) = hytte_config::test_support::capture();
        let (mut first, first_end) = UnixStream::pair().expect("a socket pair");
        let (mut second, second_end) = UnixStream::pair().expect("a socket pair");
        let hung_up_at = Cell::new(None);
        let first_served_at = Cell::new(None);

        let served_first = async {
            let ending = super::serve(&bridge, first_end).await;
            first_served_at.set(Some(Instant::now()));
            ending
        };
        let clients = async {
            first.write_all(&request("pet", "one")).await.expect("send");
            let [first_pid] = stand_in::wait_for(dir.path(), 1).await[..] else {
                panic!("one child before the second turn is sent");
            };
            second
                .write_all(&request("pet", "two"))
                .await
                .expect("send");
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                stand_in::started(dir.path()),
                [first_pid],
                "the second turn started beside the first: their title is not locked",
            );

            drop(first);
            let hung_up = Instant::now();
            hung_up_at.set(Some(hung_up));
            gone_within_slack(first_pid, hung_up, "its client hung up").await;

            let pids = stand_in::wait_for(dir.path(), 2).await;
            let [second_pid] = pids[..]
                .iter()
                .copied()
                .filter(|&p| p != first_pid)
                .collect::<Vec<_>>()[..]
            else {
                panic!("the second turn never got the title: {pids:?}");
            };
            drop(second);
            gone_within_slack(second_pid, Instant::now(), "its client hung up").await;
        };
        let (first_ending, second_ending, ()) =
            tokio::join!(served_first, super::serve(&bridge, second_end), clients);

        assert_eq!(first_ending, Some((super::HUNG_UP, false)));
        assert_eq!(second_ending, Some((super::HUNG_UP, false)));
        let (Some(hung_up), Some(served)) = (hung_up_at.get(), first_served_at.get()) else {
            panic!("both instants are set once the join is done");
        };
        assert!(
            served.duration_since(hung_up) < REAP_SLACK,
            "the first request was served {:?} after its client hung up, not at once",
            served.duration_since(hung_up),
        );
        assert_eq!(bridge.permits.available_permits(), super::PERMITS);
        assert!(
            bridge.inflight.lock().expect("unpoisoned").is_empty(),
            "a dropped turn left its single-flight entry behind",
        );
        assert_eq!(
            abandoned(&captured),
            2,
            "each dropped turn's kill was not reported exactly once",
        );
    }

    /// **A half-closed client keeps its turn.** A client may shut down its
    /// write side once the request is out and still read the answer — and a
    /// read on the bridge's end then returns 0 exactly as it would for a
    /// client that has gone. The watch must not take that for a hang-up.
    ///
    /// The gated stand-in holds the turn open for a second after the
    /// half-close — ten of the watch's polls — and must still be running at
    /// the end of it; then it answers, and the client reads the whole 200.
    #[tokio::test]
    async fn a_half_closed_client_keeps_its_turn_and_gets_the_answer() {
        use crate::backend::stand_in;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::UnixStream;

        let dir = tempfile::tempdir().expect("tempdir");
        let program = dir.path().join("gated-claude");
        stand_in::install(&program, GATED);
        let bridge = bridge_running(program);
        let (captured, _scope) = hytte_config::test_support::capture();
        let (mut client, server) = UnixStream::pair().expect("a socket pair");

        let client_side = async {
            client.write_all(&request("pet", "hi")).await.expect("send");
            let [pid] = stand_in::wait_for(dir.path(), 1).await[..] else {
                panic!("one child");
            };
            client.shutdown().await.expect("shut down the write half");
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert!(
                stand_in::present(pid),
                "the half-closed client's turn was dropped: its child ({pid}) is gone",
            );
            std::fs::write(dir.path().join("go"), b"").expect("release the stand-in");
            let mut reply = Vec::new();
            client
                .read_to_end(&mut reply)
                .await
                .expect("read the answer to EOF");
            String::from_utf8(reply).expect("an ASCII head and a JSON body")
        };
        let (ending, reply) = tokio::join!(super::serve(&bridge, server), client_side);

        assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"), "{reply:?}");
        assert!(reply.contains("answered"), "{reply:?}");
        assert_eq!(ending, Some((200, true)));
        assert_eq!(
            abandoned(&captured),
            0,
            "the half-closed client's turn was reported as abandoned",
        );
    }

    /// The positive control: a client that stays connected gets its answer,
    /// and nothing is dropped or killed on the way.
    ///
    /// The zero is only worth something if this capture would have heard the
    /// guard, so the same capture then watches a client that does hang up and
    /// must count exactly one.
    #[tokio::test]
    async fn a_client_that_stays_connected_gets_its_answer_and_trips_no_guard() {
        use crate::backend::stand_in;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::UnixStream;

        let dir = tempfile::tempdir().expect("tempdir");
        let (captured, _scope) = hytte_config::test_support::capture();

        let answering = bridge_running(stand_in::answering_claude(dir.path()));
        let (mut client, server) = UnixStream::pair().expect("a socket pair");
        let client_side = async {
            client.write_all(&request("pet", "hi")).await.expect("send");
            let mut reply = Vec::new();
            client
                .read_to_end(&mut reply)
                .await
                .expect("read the answer to EOF");
            String::from_utf8(reply).expect("an ASCII head and a JSON body")
        };
        let (ending, reply) = tokio::join!(super::serve(&answering, server), client_side);
        assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"), "{reply:?}");
        assert!(reply.contains("answered"), "{reply:?}");
        assert_eq!(ending, Some((200, true)));
        assert_eq!(
            abandoned(&captured),
            0,
            "a connected client's answered turn was reported as abandoned",
        );

        let hung = hung_bridge(dir.path(), HANG_UP_BUDGET);
        let (mut client, server) = UnixStream::pair().expect("a socket pair");
        let client_side = async {
            client.write_all(&request("pet", "hi")).await.expect("send");
            stand_in::wait_for(dir.path(), 1).await;
            drop(client);
        };
        let (ending, ()) = tokio::join!(super::serve(&hung, server), client_side);
        assert_eq!(ending, Some((super::HUNG_UP, false)));
        assert_eq!(
            abandoned(&captured),
            1,
            "this capture does not hear the guard, so the zero above proves nothing",
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
