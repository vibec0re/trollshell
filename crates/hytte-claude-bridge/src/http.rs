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

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, Interest};
use tokio::net::UnixStream;

/// Anything one request can be read off and one response written back to.
///
/// Generic since #993 only because the socket underneath changed from a
/// `TcpStream` to a `UnixStream`: **the bytes on it did not**. Keeping the
/// stream a parameter rather than swapping one concrete type for another is
/// what lets the parser tests below stay byte-for-byte the ones written against
/// `ureq`'s output, and lets a test drive a whole request/response over
/// `tokio::io::duplex` with no socket at all.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Stream for S {}

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

/// How long a single connection may take to *accept* its response — the
/// write-side twin of [`READ_TIMEOUT`], and short for the same reason: the only
/// client reads the whole reply to EOF the moment it arrives.
///
/// Without it a client that never reads, sent a reply larger than the
/// socket's send buffer, would park its connection task in `write_all` for the
/// life of the process — the request permit is already released by then, but
/// the task, the fd and the response buffer are not.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// How often [`hung_up`] looks at a connection whose request is being
/// answered — and so the longest a turn runs on after its client has gone: a
/// tenth of a second, against a request budget of about eight.
const HANG_UP_POLL: Duration = Duration::from_millis(100);

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
///
/// `Ok(None)` is a peer that closed **before sending a single byte** — not a
/// malformed request but no request at all, and the caller answers it with
/// nothing. That is exactly what a second bridge's startup probe
/// ([`crate::socket::socket_in_use`]) does to the incumbent: connect, learn the
/// socket is live, hang up. Reading it as a 400 used to count every such probe
/// against the chip's health (its `ERRORS` and its last-request warning) for a
/// request nobody sent. A peer that sends *some* bytes and then closes is still
/// the 400 it always was, and so is one that connects and then sits silent
/// until [`READ_TIMEOUT`] (a 408).
pub async fn read_request(stream: &mut impl Stream) -> Result<Option<(Head, Vec<u8>)>, Failure> {
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
                if buf.is_empty() {
                    return Ok(None);
                }
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
        Ok(Some((head, body)))
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

/// Write a response and close the write half, bounded by [`WRITE_TIMEOUT`].
/// Returns whether the client was handed the whole response.
///
/// Every outcome is the end of the connection — the client gets the reply or
/// it does not, and there is nobody left to report a failure to — so each is
/// one debug line, never an error. The `bool` is for the one reader that does
/// care: the status chip, which should not count an answer as served when the
/// client never got it (see `bridge::serve_connection`).
///
/// "Handed" means `write_all` finished: on a Unix stream socket the bytes are
/// then in the peer's receive buffer, `Content-Length` already tells it where
/// they end, and a failed `shutdown` after that costs the client nothing.
pub async fn write_response(stream: &mut impl Stream, status: u16, body: &[u8]) -> bool {
    let bytes = response_bytes(status, body);
    let write = async {
        if let Err(e) = stream.write_all(&bytes).await {
            tracing::debug!(error = %e, "response write failed");
            return false;
        }
        if let Err(e) = stream.shutdown().await {
            tracing::debug!(error = %e, "response shutdown failed");
        }
        true
    };
    tokio::time::timeout(WRITE_TIMEOUT, write)
        .await
        .unwrap_or_else(|_elapsed| {
            tracing::debug!(
                timeout_s = WRITE_TIMEOUT.as_secs(),
                "timed out writing the response; dropping the connection",
            );
            false
        })
}

/// Resolves once the client at the far end of `stream` has **gone**, and never
/// before (#1550).
///
/// "Gone" is the kernel's answer, not a guess from a read. When the peer of a
/// Unix stream socket closes its end (or shuts down both directions), Linux
/// marks *this* end shut for reading **and** writing, and `poll(2)` reports
/// `POLLHUP` — which tokio surfaces as
/// [`Ready::is_write_closed`](tokio::io::Ready::is_write_closed). A client that
/// shuts down only its **write** half after sending the request, and keeps
/// reading for the answer, marks this end shut for reading alone: `POLLRDHUP`,
/// and a read that returns 0, but no `POLLHUP`. That half-closed client keeps
/// its turn. A zero-length read looks the same in both cases, which is why this
/// never reads. It would have nothing to read anyway: [`read_request`] has
/// taken exactly the request's `Content-Length`, and the protocol is one
/// request per connection.
///
/// It **polls** the connection's readiness every [`HANG_UP_POLL`] rather than
/// awaiting the hang-up as an event, because no readiness a connected socket
/// can wait on fits. `READABLE` fires on a half-close too, and stays set after
/// it, since tokio never clears a closed state. `WRITABLE` is set the whole
/// time, and clearing it to wait for the next event would also park
/// [`write_response`] on an edge that never comes. The flags themselves are
/// kept current by the runtime whether or not anything waits on them, so
/// reading them costs a timer tick, and that tick bounds how long a turn
/// nobody will read runs on after its client has left.
///
/// Never resolves if the readiness cannot be read at all (the runtime is
/// shutting down): a turn is dropped for a hang-up and for nothing else.
pub async fn hung_up(stream: &UnixStream) {
    loop {
        match stream.ready(Interest::WRITABLE).await {
            Ok(ready) if ready.is_write_closed() => return,
            Ok(_) => tokio::time::sleep(HANG_UP_POLL).await,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "cannot watch the connection for a hang-up; answering it unwatched",
                );
                std::future::pending::<()>().await;
            }
        }
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
    use super::{ROUTE, WRITE_TIMEOUT, parse_head, read_request, response_bytes, write_response};

    /// A client that never reads cannot hold its connection task forever: a
    /// reply bigger than the pipe's buffer gives up after [`WRITE_TIMEOUT`]
    /// instead of parking in `write_all`. On a paused clock, so it costs no
    /// real time — and under an outer timeout, so a regression fails rather
    /// than hangs the suite.
    #[tokio::test(start_paused = true)]
    async fn a_client_that_never_reads_cannot_hold_the_write_forever() {
        // 64 bytes of buffer and a peer kept alive but never read from, so a
        // 4 KiB reply can only ever block.
        let (_client, mut server) = tokio::io::duplex(64);
        let started = tokio::time::Instant::now();
        let delivered = tokio::time::timeout(
            WRITE_TIMEOUT * 2,
            write_response(&mut server, 200, &[b'x'; 4096]),
        )
        .await
        .expect("the write gave up on its own");
        assert!(!delivered, "a reply the client never took is not delivered");
        let elapsed = started.elapsed();
        assert!(
            (WRITE_TIMEOUT..WRITE_TIMEOUT * 2).contains(&elapsed),
            "gave up after {elapsed:?}, not after the {WRITE_TIMEOUT:?} bound",
        );
    }

    /// …while a client that does read gets the whole reply, closed, at once.
    #[tokio::test(start_paused = true)]
    async fn a_reading_client_gets_the_whole_reply_and_eof() {
        use tokio::io::AsyncReadExt as _;

        let (mut client, mut server) = tokio::io::duplex(64);
        let body = [b'x'; 4096];
        let started = tokio::time::Instant::now();
        let mut got = Vec::new();
        let (delivered, read) = tokio::join!(
            write_response(&mut server, 200, &body),
            client.read_to_end(&mut got),
        );
        read.expect("read to EOF");
        assert!(delivered);
        assert_eq!(got, response_bytes(200, &body));
        assert!(
            started.elapsed() < WRITE_TIMEOUT,
            "no timeout on the happy path"
        );
    }

    /// A peer that hangs up before sending anything made no request: `None`,
    /// which the caller answers with nothing and does not count. This is the
    /// shape of a second bridge's startup probe (`socket::socket_in_use`),
    /// which used to land on the incumbent's chip as a 400.
    #[tokio::test]
    async fn a_peer_that_closes_before_any_byte_made_no_request() {
        let (client, mut server) = tokio::io::duplex(64);
        drop(client);
        assert!(
            read_request(&mut server)
                .await
                .expect("not a failure")
                .is_none()
        );
    }

    /// …but one byte is a request, and a request cut short is still the 400 it
    /// always was — the exemption is for *no* bytes, not for a short read.
    #[tokio::test]
    async fn a_peer_that_closes_mid_head_is_still_a_bad_request() {
        use tokio::io::AsyncWriteExt as _;

        let (mut client, mut server) = tokio::io::duplex(64);
        client
            .write_all(b"POST /v1/chat")
            .await
            .expect("write a partial head");
        drop(client);
        let failure = read_request(&mut server).await.expect_err("a short read");
        assert_eq!(failure.status, 400);
        assert!(failure.message.contains("mid-head"), "{}", failure.message);
    }

    /// The exact head `ureq` 3 sends for `chat()` — since #993 over a Unix
    /// socket, which changes only the `Host:` placeholder the client fills in
    /// (`hytte_ai_providers::unix`), never the request line or the framing.
    #[test]
    fn parses_a_ureq_shaped_request() {
        let raw = b"POST /v1/chat/completions HTTP/1.1\r\n\
                    host: localhost\r\n\
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
