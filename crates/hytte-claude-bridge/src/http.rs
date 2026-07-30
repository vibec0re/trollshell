//! The entire HTTP server — about a hundred lines over `httparse`.
//!
//! # Why not a framework
//!
//! There is exactly one route. `axum`, `hyper` and `tower` are **absent from
//! this workspace's `Cargo.lock`**, so adopting one for a single POST handler
//! would add roughly twenty resolved packages to a tree that otherwise has no
//! async HTTP server at all. `httparse` is *already* in the lock (pulled in by
//! `ureq-proto`), so promoting it to a direct dependency resolves no new
//! package.
//!
//! # What that costs, stated plainly
//!
//! This speaks the narrow dialect its one client speaks and nothing more:
//! HTTP/1.1, `Content-Length`-delimited request bodies (no `Transfer-Encoding:
//! chunked`), one request per connection, and `Connection: close` on every
//! response. `ureq` 3 — the only client, via `hytte_ai_providers::chat` — sends
//! exactly that. Anything else gets a 400 with a reason rather than a hang.

use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// The one route this bridge serves.
pub const ROUTE: &str = "/v1/chat/completions";

/// Largest request head accepted, in bytes. `ureq` sends a handful of short
/// headers; anything near this is a client bug or an attack.
const MAX_HEAD: usize = 16 * 1024;

/// Largest request body accepted, in bytes. caw's briefing prompt is the big
/// one and is orders of magnitude under this.
const MAX_BODY: usize = 1024 * 1024;

/// How long a single connection may take to deliver its full request. Short,
/// because the only client sends the whole thing in one shot.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// A request that cannot be answered, as the status and body text the client
/// will see.
///
/// `Clone` because the single-flight path shares one outcome with every waiter
/// (see `bridge.rs`).
#[derive(Debug, Clone)]
pub struct Failure {
    pub status: u16,
    pub message: String,
}

impl Failure {
    /// A failure with an explicit status.
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status, self.message)
    }
}

/// A parsed request head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub method: String,
    pub path: String,
    pub content_length: usize,
    /// Byte length of the head, i.e. the offset the body starts at.
    pub body_offset: usize,
}

/// Parse a request head out of `buf`.
///
/// `Ok(None)` means "incomplete, read more". Errors are terminal and carry the
/// status to answer with.
pub fn parse_head(buf: &[u8]) -> Result<Option<Head>, Failure> {
    if buf.len() > MAX_HEAD {
        return Err(Failure::new(431, "request head too large"));
    }
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    let body_offset = match req.parse(buf) {
        Ok(httparse::Status::Complete(n)) => n,
        Ok(httparse::Status::Partial) => return Ok(None),
        Err(e) => return Err(Failure::new(400, format!("malformed request: {e}"))),
    };

    let mut content_length = 0usize;
    for h in req.headers.iter() {
        if h.name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(Failure::new(
                400,
                "chunked transfer encoding is not supported; send Content-Length",
            ));
        }
        if h.name.eq_ignore_ascii_case("content-length") {
            let raw = std::str::from_utf8(h.value)
                .map_err(|_| Failure::new(400, "non-utf8 Content-Length"))?;
            content_length = raw
                .trim()
                .parse()
                .map_err(|_| Failure::new(400, "unparseable Content-Length"))?;
        }
    }
    if content_length > MAX_BODY {
        return Err(Failure::new(413, "request body too large"));
    }

    Ok(Some(Head {
        method: req.method.unwrap_or_default().to_owned(),
        // Query strings are not part of any route here, so the path is
        // compared bare.
        path: req
            .path
            .unwrap_or_default()
            .split('?')
            .next()
            .unwrap_or_default()
            .to_owned(),
        content_length,
        body_offset,
    }))
}

/// Read one complete request off `stream`, bounded by [`READ_TIMEOUT`].
pub async fn read_request(stream: &mut TcpStream) -> Result<(Head, Vec<u8>), Failure> {
    let read = async {
        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 4096];
        let head = loop {
            if let Some(head) = parse_head(&buf)? {
                break head;
            }
            let n = stream
                .read(&mut chunk)
                .await
                .map_err(|e| Failure::new(400, format!("read failed: {e}")))?;
            if n == 0 {
                return Err(Failure::new(400, "connection closed mid-head"));
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let want = head.body_offset + head.content_length;
        while buf.len() < want {
            let n = stream
                .read(&mut chunk)
                .await
                .map_err(|e| Failure::new(400, format!("read failed: {e}")))?;
            if n == 0 {
                return Err(Failure::new(400, "connection closed mid-body"));
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let body = buf[head.body_offset..want].to_vec();
        Ok((head, body))
    };
    match tokio::time::timeout(READ_TIMEOUT, read).await {
        Ok(result) => result,
        Err(_elapsed) => Err(Failure::new(408, "timed out reading the request")),
    }
}

/// Serialise a response. Always `Connection: close` — one request per
/// connection is the whole contract here.
#[must_use]
pub fn response_bytes(status: u16, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n",
        reason = reason(status),
        len = body.len(),
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

/// Write a response and close the write half.
pub async fn write_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
    let bytes = response_bytes(status, body);
    if let Err(e) = stream.write_all(&bytes).await {
        tracing::debug!(error = %e, "response write failed");
        return;
    }
    if let Err(e) = stream.shutdown().await {
        tracing::debug!(error = %e, "response shutdown failed");
    }
}

/// Reason phrase for the statuses this bridge emits.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::{ROUTE, parse_head, response_bytes};

    /// The exact head `ureq` 3 sends for `chat()`.
    #[test]
    fn parses_a_ureq_shaped_request() {
        let raw = b"POST /v1/chat/completions HTTP/1.1\r\n\
                    host: 127.0.0.1:8787\r\n\
                    content-type: application/json\r\n\
                    content-length: 17\r\n\
                    \r\n\
                    {\"messages\":[]}xx";
        let head = parse_head(raw).expect("valid").expect("complete");
        assert_eq!(head.method, "POST");
        assert_eq!(head.path, ROUTE);
        assert_eq!(head.content_length, 17);
        assert_eq!(&raw[head.body_offset..], b"{\"messages\":[]}xx");
    }

    /// A partial head is "read more", not an error.
    #[test]
    fn a_partial_head_is_incomplete_not_an_error() {
        let raw = b"POST /v1/chat/completions HTTP/1.1\r\nhost: x\r\n";
        assert!(parse_head(raw).expect("not an error").is_none());
    }

    /// A query string is stripped before routing.
    #[test]
    fn the_query_string_is_not_part_of_the_path() {
        let raw = b"POST /v1/chat/completions?debug=1 HTTP/1.1\r\ncontent-length: 0\r\n\r\n";
        let head = parse_head(raw).expect("valid").expect("complete");
        assert_eq!(head.path, ROUTE);
    }

    /// Chunked bodies are refused with a reason rather than mis-read as an
    /// empty body.
    #[test]
    fn chunked_requests_are_refused_explicitly() {
        let raw = b"POST /v1/chat/completions HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n";
        let err = parse_head(raw).expect_err("refused");
        assert_eq!(err.status, 400);
        assert!(err.message.contains("chunked"));
    }

    /// An oversized declared body is rejected before any of it is read.
    #[test]
    fn an_oversized_body_is_rejected_up_front() {
        let raw = b"POST /v1/chat/completions HTTP/1.1\r\ncontent-length: 99999999\r\n\r\n";
        assert_eq!(parse_head(raw).expect_err("refused").status, 413);
    }

    /// Garbage is a 400, not a panic.
    #[test]
    fn garbage_is_a_bad_request() {
        let err = parse_head(b"\x01\x02 not http at all\r\n\r\n").expect_err("refused");
        assert_eq!(err.status, 400);
    }

    /// Every response closes the connection — `ureq` reads to EOF.
    #[test]
    fn responses_declare_connection_close_and_a_correct_length() {
        let body = br#"{"ok":true}"#;
        let bytes = response_bytes(200, body);
        let text = String::from_utf8(bytes).expect("ascii head");
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(text.ends_with(r#"{"ok":true}"#));
    }

    /// Statuses the bridge actually emits carry real reason phrases.
    #[test]
    fn emitted_statuses_have_reason_phrases() {
        for (status, phrase) in [
            (400, "Bad Request"),
            (404, "Not Found"),
            (405, "Method Not Allowed"),
            (429, "Too Many Requests"),
            (502, "Bad Gateway"),
            (503, "Service Unavailable"),
            (504, "Gateway Timeout"),
        ] {
            let text = String::from_utf8(response_bytes(status, b"")).expect("ascii");
            assert!(
                text.starts_with(&format!("HTTP/1.1 {status} {phrase}\r\n")),
                "{status}"
            );
        }
    }
}
