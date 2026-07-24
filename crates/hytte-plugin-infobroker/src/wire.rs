//! The broker socket's boring JSON-lines protocol.
//!
//! One request object per line in, one response object per line out — that's the
//! whole wire (the CLI is the only client, so it stays deliberately dull; the
//! same schema is written up for agents in the skill folder's `SKILL.md`).
//! Requests are an externally-tagged `op` enum; responses are one flat struct
//! with an `ok` flag plus optional payload fields, so a new field is additive
//! and an old client just ignores what it doesn't read.
//!
//! ```text
//! → {"op":"auth","agent":"claude"}
//! ← {"ok":true,"token":"…","expires_unix":1750000000,"agent":"claude"}
//!
//! → {"op":"get","token":"…","datasource":"departures","limit":5}
//! ← {"ok":true,"datasource":"departures","departures":[ … ]}
//!
//! → {"op":"grants"}
//! ← {"ok":true,"grants":[{"agent":"claude","datasource":"departures", … }]}
//!
//! ← {"ok":false,"error":"…","hint":"…"}          (any denied request)
//! ```

use serde::{Deserialize, Serialize};

/// The departures datasource (phase 1a). Named on the wire so the vocabulary is
/// ready for more without a schema change.
pub const DATASOURCE_DEPARTURES: &str = "departures";

/// The calendar datasource (#484/#509 item 4). Unlike departures — which the
/// broker fetches itself — the broker keeps a **live copy** fed by the shell's
/// host push (the broker plugin subscribes `CalendarUpcoming` and forwards it
/// down the command lane), and `get calendar` serves that copy under the normal
/// grant flow. EDS lives in the shell, so an out-of-process broker can't read it
/// directly; the host push is the bridge.
pub const DATASOURCE_CALENDAR: &str = "calendar";

/// One request line from the CLI. Externally tagged on `op` so adding an op is
/// additive and an unknown op fails to parse loudly rather than silently.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Mint a session token for `agent` (identity = the token). Granted silently
    /// iff an `always` grant already covers the agent; otherwise denied.
    Auth { agent: String },
    /// Fetch scoped data from `datasource`, authenticated by a prior `auth`
    /// token. `limit` caps the returned rows (host clamps; `None` = a sane
    /// default).
    Get {
        token: String,
        datasource: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    /// List the durable grants (read-only; no token needed — it's local
    /// introspection for the human, mirrors the panel).
    Grants,
}

/// One scoped departure row in a [`Response`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepartureOut {
    /// Line label, e.g. `"S9"`.
    pub line: String,
    /// Destination, e.g. `"Spandau"`.
    pub direction: String,
    /// Local `HH:MM` of the actual departure.
    pub hhmm: String,
    /// Whole minutes from now until it departs (`0` = now/imminent).
    pub in_minutes: i64,
    /// Lateness in minutes (`0` on time; only lateness is surfaced).
    pub delay_minutes: i64,
    /// `true` for a cancelled run.
    pub cancelled: bool,
}

/// One upcoming calendar event in a [`Response`] (`get calendar` ok, #484). The
/// broker's SDK-free mirror of the shell's wire `UpcomingEvent`; the broker
/// plugin maps the proto type onto this at the boundary (as it maps consent
/// decisions), so the library never links the plugin proto. Times are Unix
/// seconds — the agent formats them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEntry {
    pub start_unix: i64,
    pub end_unix: i64,
    pub title: String,
    pub calendar: String,
}

/// One grant row in a `grants` [`Response`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantOut {
    pub agent: String,
    pub datasource: String,
    pub scope: String,
    /// `"always"` or `"deny"`.
    pub decision: String,
}

/// One response line. `ok` is the only required field; the rest are populated
/// per op and skipped when absent, so the JSON stays tight.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    /// A one-line failure reason (present iff `!ok`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// An actionable next step for the human (e.g. how to grant). Present on the
    /// consent denials the human should act on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// The minted session token (`auth` ok).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The token's absolute expiry, unix seconds (`auth` ok).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_unix: Option<i64>,
    /// The resolved agent (`auth` ok — echoes what the token identifies as).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Which datasource a `get` answered for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datasource: Option<String>,
    /// The scoped departures (`get departures` ok).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub departures: Option<Vec<DepartureOut>>,
    /// The upcoming calendar events (`get calendar` ok, #484).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar: Option<Vec<CalendarEntry>>,
    /// The durable grants (`grants` ok).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grants: Option<Vec<GrantOut>>,
}

impl Response {
    /// A denial with a reason and an actionable hint.
    #[must_use]
    pub fn denied(error: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
            hint: Some(hint.into()),
            ..Self::default()
        }
    }

    /// A denial with a reason but no actionable hint (a transient/technical
    /// failure the agent should just retry, e.g. an expired token).
    #[must_use]
    pub fn error(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
            ..Self::default()
        }
    }
}

/// Parse one request line. Returns a human-readable error (used verbatim in the
/// denial [`Response`]) rather than the raw serde message dressing.
///
/// # Errors
/// If `line` is not a JSON object matching a known [`Request`] op.
pub fn parse_request(line: &str) -> Result<Request, String> {
    serde_json::from_str(line).map_err(|e| format!("bad request: {e}"))
}

/// Serialize a response to a single JSON line (no trailing newline — the caller
/// frames it). Infallible in practice; a serialization error degrades to a
/// hand-written error line so the socket always answers *something*.
#[must_use]
pub fn encode_response(resp: &Response) -> String {
    serde_json::to_string(resp)
        .unwrap_or_else(|_| r#"{"ok":false,"error":"broker: response encode failed"}"#.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_auth_request() {
        let req = parse_request(r#"{"op":"auth","agent":"claude"}"#).expect("parses");
        assert_eq!(
            req,
            Request::Auth {
                agent: "claude".to_owned()
            }
        );
    }

    #[test]
    fn parse_get_request_with_and_without_limit() {
        let with =
            parse_request(r#"{"op":"get","token":"abc","datasource":"departures","limit":5}"#)
                .expect("parses");
        assert_eq!(
            with,
            Request::Get {
                token: "abc".to_owned(),
                datasource: "departures".to_owned(),
                limit: Some(5),
            }
        );
        // `limit` is optional and defaults to None (a sane server default).
        let without = parse_request(r#"{"op":"get","token":"abc","datasource":"departures"}"#)
            .expect("parses");
        assert_eq!(
            without,
            Request::Get {
                token: "abc".to_owned(),
                datasource: "departures".to_owned(),
                limit: None,
            }
        );
    }

    #[test]
    fn parse_grants_request() {
        assert_eq!(
            parse_request(r#"{"op":"grants"}"#).expect("parses"),
            Request::Grants
        );
    }

    #[test]
    fn parse_rejects_unknown_op_and_garbage() {
        assert!(
            parse_request(r#"{"op":"nope"}"#).is_err(),
            "unknown op is a loud error"
        );
        let err = parse_request("{not json").unwrap_err();
        assert!(err.starts_with("bad request:"), "got: {err}");
    }

    #[test]
    fn response_round_trips_and_skips_empty_fields() {
        let ok = Response {
            ok: true,
            token: Some("t".to_owned()),
            expires_unix: Some(42),
            agent: Some("claude".to_owned()),
            ..Response::default()
        };
        let line = encode_response(&ok);
        // Only the populated fields ride the wire.
        assert!(line.contains(r#""ok":true"#));
        assert!(line.contains(r#""token":"t""#));
        assert!(!line.contains("error"), "absent fields are skipped: {line}");
        assert!(!line.contains("departures"));
        // And it decodes back to the same struct.
        let back: Response = serde_json::from_str(&line).expect("decodes");
        assert_eq!(back, ok);
    }

    #[test]
    fn calendar_response_round_trips_and_names_its_datasource() {
        let resp = Response {
            ok: true,
            datasource: Some(DATASOURCE_CALENDAR.to_owned()),
            calendar: Some(vec![CalendarEntry {
                start_unix: 100,
                end_unix: 200,
                title: "standup".to_owned(),
                calendar: "Work".to_owned(),
            }]),
            ..Response::default()
        };
        let line = encode_response(&resp);
        assert!(line.contains(r#""datasource":"calendar""#), "{line}");
        assert!(line.contains(r#""title":"standup""#), "{line}");
        assert!(
            !line.contains("departures"),
            "unrelated fields skipped: {line}"
        );
        let back: Response = serde_json::from_str(&line).expect("decodes");
        assert_eq!(back, resp);
    }

    #[test]
    fn denied_carries_error_and_hint() {
        let d = Response::denied("no grant for agent 'claude'", "add a grant to grants.toml");
        assert!(!d.ok);
        assert_eq!(d.error.as_deref(), Some("no grant for agent 'claude'"));
        assert_eq!(d.hint.as_deref(), Some("add a grant to grants.toml"));
    }
}
