//! Request handling: routing, load shedding, single-flight, and the title map
//! that ties the two backends to one conversation identity.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::net::TcpStream;
use tokio::sync::{Semaphore, watch};

use crate::backend::{Backend, Conversation as _, Turn};
use crate::http::{self, Failure, Head};
use crate::session::{self, Titles};
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
        // The accepted-but-ignored knobs, named in the journal rather than
        // silently dropped: `claude` exposes no sampling or token-cap flag, so
        // tolerating these is the whole of the contract.
        tracing::debug!(
            max_tokens = ?req.max_tokens,
            temperature = ?req.temperature,
            enable_thinking = ?req.chat_template_kwargs.and_then(|k| k.enable_thinking),
            "accepted and ignoring the sampling knobs",
        );

        match self.complete(&req.messages).await {
            Ok(text) => ok_response(text, req.model, &req.messages),
            Err(f) => {
                tracing::warn!(status = f.status, message = %f.message, "request failed");
                error_response(f.status, &f.message)
            }
        }
    }

    /// Single-flight: identical concurrent transcripts share one `claude` turn.
    ///
    /// pet's tick and a manual poke can land together on the same prompt, and
    /// paying twice for the same answer is pure waste of a subscription that is
    /// rate-limited, not metered.
    async fn complete(&self, messages: &[Message]) -> Result<String, Failure> {
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
                let outcome = self.run_turn(messages).await;
                let _ = tx.send(Some(Arc::new(outcome.clone())));
                outcome
            }
        }
    }

    /// Run one turn: shed load, derive the identity, spend the budget, record.
    async fn run_turn(&self, messages: &[Message]) -> Result<String, Failure> {
        let Ok(_permit) = self.permits.try_acquire() else {
            return Err(Failure::new(
                503,
                format!("bridge busy: {PERMITS} claude turns already running"),
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

        let turn = Turn {
            title: &title,
            transcript: messages,
            delta,
        };
        let reply = match tokio::time::timeout(self.budget, self.backend.respond(turn)).await {
            Ok(reply) => reply?,
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
                return Err(Failure::new(
                    504,
                    format!(
                        "no answer within the bridge's {}s budget",
                        self.budget.as_secs()
                    ),
                ));
            }
        };
        self.titles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remember(&title, messages, &reply);
        Ok(reply)
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
