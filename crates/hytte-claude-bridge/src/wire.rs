//! The OpenAI-compatible wire shapes — deliberately the *narrowest* pair that
//! satisfies the one client this bridge has.
//!
//! **Requests** accept exactly what `hytte_ai_providers::chat` sends
//! (`crates/hytte-ai-providers/src/lib.rs`): `model`, `messages`, `max_tokens`,
//! `temperature`, llama's `chat_template_kwargs.enable_thinking`, and — since
//! #704 — the spec's own `user`, which is the one field here that changes what
//! the bridge *does* rather than merely being tolerated. Unknown fields are
//! tolerated and ignored (serde's default), so a slightly different `OpenAI`
//! client still gets an answer rather than a 400.
//!
//! **Responses** return exactly what that `chat()` parses back: the first
//! choice's `message.content`. The rest of the envelope (`id`, `object`,
//! `created`, `model`, `finish_reason`) is there so the body is recognisable to
//! a human reading a `curl`, not because anything reads it. There is
//! deliberately **no `usage` block, no streaming, and no tool calls** — see the
//! crate docs.

use serde::{Deserialize, Serialize};

/// One chat message, in the `OpenAI` wire shape.
///
/// `content` is a plain `String` because that is what `hytte-ai-providers`
/// serialises; the spec's array-of-parts form is rejected as a bad request
/// rather than half-supported.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    /// An `assistant`-role message — used to reconstruct what the *next*
    /// request's transcript prefix will look like (see [`crate::session`]).
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_owned(),
            content: content.into(),
        }
    }
}

/// `POST /v1/chat/completions` request body.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatRequest {
    /// Echoed back in the response envelope; it does **not** select a model —
    /// the model is the bridge's own `$CLAUDE_BRIDGE_MODEL` (a client picking
    /// the model would let a plugin spend arbitrary subscription budget).
    #[serde(default)]
    pub model: Option<String>,
    pub messages: Vec<Message>,
    /// Accepted and only *approximated*: the Claude Code CLI has no
    /// `--max-tokens`, so this cannot be enforced. Kept in the shape so a
    /// client sending it isn't rejected.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Accepted and ignored — headless `claude` exposes no sampling knob.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// llama-server's template kwarg (`enable_thinking`). Tolerated and
    /// ignored; a keyless `Provider` sends it, so rejecting it would break the
    /// exact client this exists for.
    #[serde(default)]
    pub chat_template_kwargs: Option<TemplateKwargs>,
    /// Caller-supplied session identity (#704) — `OpenAI`'s own `user` field,
    /// which the spec already defines as a stable per-caller identifier, put
    /// to work here as the conversation handle the bridge otherwise has to
    /// *guess* from the transcript.
    ///
    /// Absent → the content-hash fallback in [`crate::session`], which is what
    /// every pre-#704 client does and what this crate did before #704. Present
    /// → the title is derived from it instead, in a namespace disjoint from
    /// the hash-derived one (see [`crate::session::Key`]).
    #[serde(default)]
    pub user: Option<String>,
}

/// The llama-only template kwargs object. Parsed so it can be *ignored*
/// explicitly rather than by accident.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct TemplateKwargs {
    #[serde(default)]
    pub enable_thinking: Option<bool>,
}

/// `POST /v1/chat/completions` response body.
#[derive(Debug, Serialize)]
pub struct ChatResponse {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub choices: Vec<Choice>,
}

/// One completion choice. Only index 0 is ever produced.
#[derive(Debug, Serialize)]
pub struct Choice {
    pub index: u32,
    pub message: Message,
    pub finish_reason: &'static str,
}

impl ChatResponse {
    /// Wrap `content` in the single-choice envelope `chat()` knows how to read.
    /// `created` is a unix timestamp; `id` is derived from it plus the reply so
    /// two responses don't share an id.
    pub fn single(content: String, model: Option<String>, created: u64, id_seed: u64) -> Self {
        Self {
            id: format!("chatcmpl-{id_seed:016x}"),
            object: "chat.completion",
            created,
            model,
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: "assistant".to_owned(),
                    content,
                },
                finish_reason: "stop",
            }],
        }
    }
}

/// The error body shape `OpenAI` clients expect. `hytte-ai-providers` builds its
/// `Err(String)` out of the raw body text on a non-2xx, so the `message` here
/// is what shows up in the pet's / caw's log line.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

/// The `error` object inside [`ErrorBody`].
#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub message: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
}

impl ErrorBody {
    /// An error body carrying `message`.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            error: ErrorDetail {
                message: message.into(),
                kind: "bridge_error",
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ChatRequest, ChatResponse};

    /// The exact body `hytte_ai_providers::chat` sends for a keyless provider
    /// (no `model`, `chat_template_kwargs` present) must parse.
    #[test]
    fn parses_the_llama_shaped_body_the_pet_actually_sends() {
        let body = br#"{
            "messages": [
                {"role": "system", "content": "you are a cat"},
                {"role": "user", "content": "poke"}
            ],
            "max_tokens": 256,
            "temperature": 0.7,
            "chat_template_kwargs": {"enable_thinking": false}
        }"#;
        let req: ChatRequest = serde_json::from_slice(body).expect("parses");
        assert_eq!(req.model, None);
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[1].content, "poke");
        assert_eq!(req.max_tokens, Some(256));
        assert!(req.chat_template_kwargs.is_some());
        // The body a pre-#704 client sends carries no identity at all.
        assert_eq!(req.user, None);
    }

    /// A client that opts in to #704 sends `user`, and it must survive the
    /// parse — this is the only thing that carries the identity into
    /// `session.rs`.
    #[test]
    fn an_explicit_user_identity_is_parsed() {
        let body = br#"{"messages":[{"role":"user","content":"hi"}],"user":"pet"}"#;
        let req: ChatRequest = serde_json::from_slice(body).expect("parses");
        assert_eq!(req.user.as_deref(), Some("pet"));
    }

    /// An absent `user` deserialises to `None` rather than failing the request:
    /// the field is additive, so every body written before #704 must still
    /// parse (#704).
    #[test]
    fn an_absent_user_deserialises_to_none() {
        let body = br#"{"messages":[{"role":"user","content":"hi"}]}"#;
        let req: ChatRequest = serde_json::from_slice(body).expect("parses");
        assert_eq!(req.user, None);
    }

    /// A keyed provider sends `model` and no template kwargs.
    #[test]
    fn parses_the_keyed_shaped_body() {
        let body = br#"{"model":"anthropic/claude","messages":[{"role":"user","content":"hi"}],
                        "max_tokens":128,"temperature":0.2}"#;
        let req: ChatRequest = serde_json::from_slice(body).expect("parses");
        assert_eq!(req.model.as_deref(), Some("anthropic/claude"));
        assert!(req.chat_template_kwargs.is_none());
    }

    /// Fields outside the accepted subset must not be a 400 — tolerate and
    /// ignore, so a different `OpenAI` client still works.
    #[test]
    fn unknown_fields_are_tolerated() {
        let body = br#"{"messages":[{"role":"user","content":"hi"}],
                        "stream":true,"tools":[],"top_p":0.9,"seed":7}"#;
        let req: ChatRequest = serde_json::from_slice(body).expect("parses");
        assert_eq!(req.messages.len(), 1);
    }

    /// The spec's array-of-parts `content` is rejected rather than
    /// half-supported — no client of this bridge sends it.
    #[test]
    fn array_content_is_rejected() {
        let body = br#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
        assert!(serde_json::from_slice::<ChatRequest>(body).is_err());
    }

    /// The response must carry the one path `chat()` reads:
    /// `choices[0].message.content`.
    #[test]
    fn response_shape_is_what_chat_parses() {
        let resp = ChatResponse::single("meow".to_owned(), Some("sonnet".to_owned()), 42, 7);
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).expect("serialises"))
                .expect("round-trips");
        assert_eq!(json["choices"][0]["message"]["content"], "meow");
        assert_eq!(json["choices"][0]["message"]["role"], "assistant");
        assert_eq!(json["object"], "chat.completion");
        assert_eq!(json["created"], 42);
        assert_eq!(json["model"], "sonnet");
    }

    /// Deliberately unimplemented surface must be *absent*, not faked: a
    /// zeroed `usage` block would be a lie a caller could act on.
    #[test]
    fn response_carries_no_usage_block() {
        let resp = ChatResponse::single("meow".to_owned(), None, 42, 7);
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&resp).expect("serialises"))
                .expect("round-trips");
        assert!(json.get("usage").is_none());
        assert!(json.get("model").is_none());
    }
}
