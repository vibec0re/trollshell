//! Dialling an `OpenAI`-compatible endpoint over a **Unix-domain socket**
//! (#993), so a provider can name a same-uid socket instead of a loopback port.
//!
//! # Why this exists
//!
//! `hytte-claude-bridge` serves an unauthenticated route that spends the
//! owner's Claude subscription (real money in `CLAUDE_BRIDGE_MODE=api`). On
//! `127.0.0.1:8787` that was **uid-blind**: every other local account, and any
//! container sharing the host network namespace, could reach it and be billed
//! to whoever runs the shell. TCP loopback carries no file mode. A Unix socket
//! does — `0600` inside a `0700` directory under `$XDG_RUNTIME_DIR` is exactly
//! the boundary #956 settled for the plugin socket — so the bridge moved, and
//! the client had to learn how to dial one.
//!
//! # The URL shape
//!
//! [`Provider::base_url`](crate::Provider::base_url) stays one string, so no
//! consumer needed a code change: `pet` and `caw` pass whatever
//! `$PET_LLM_URL`/`$CAW_LLM_URL` holds straight through. A `unix://` prefix
//! (or its `http+unix://` alias) switches the transport:
//!
//! ```text
//! unix:///run/user/1000/trollshell/claude-bridge.sock   ← an explicit path
//! unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock ← the portable spelling
//! ```
//!
//! The second is [`BRIDGE_BASE_URL`], and it is what the nix module documents.
//! `$XDG_RUNTIME_DIR` cannot be known at nix evaluation time (it is
//! `/run/user/<uid>`, minted by logind at login), so the **one** token this
//! parser expands is that variable, resolved in the plugin's own process where
//! systemd has already set it. Nothing else is expanded: this is not a shell.
//!
//! An unset `$XDG_RUNTIME_DIR` is an [`Err`] naming the variable, never a
//! silent fall back to TCP — falling back would re-open the very hole this
//! closes.
//!
//! # How the transport is built
//!
//! `ureq` keeps its connector/resolver seams in `ureq::unversioned`, which is
//! public but **exempt from semver** (minor bumps may break it). That is the
//! deliberate trade: plugging a [`UnixStream`] in under ureq keeps the request
//! bytes, the `http_status_as_error(false)` behaviour and the global timeout
//! **identical** to the TCP path — a hand-rolled HTTP client would have been
//! a second dialect to keep in step with `hytte-claude-bridge`'s parser. The
//! cost is a compile error on a future ureq minor bump, which CI catches; the
//! version is pinned in `Cargo.lock` either way. No new resolved package: this
//! module adds no dependency at all.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ureq::Error;
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport,
};

/// Directory under `$XDG_RUNTIME_DIR` holding the bridge socket. The **same**
/// `trollshell` directory the plugin host socket lives in
/// (`hytte_plugin_proto::SOCKET_DIR`), deliberately: one `0700` directory per
/// user for every trollshell-owned socket, created by whichever of them starts
/// first.
pub const BRIDGE_SOCKET_DIR: &str = "trollshell";

/// The bridge socket's file name inside [`BRIDGE_SOCKET_DIR`].
pub const BRIDGE_SOCKET_FILE: &str = "claude-bridge.sock";

/// The canonical `base_url` for `hytte-claude-bridge`, in the portable
/// spelling — the value the nix module documents and a hand-written
/// `plugins.json` should carry.
///
/// `$XDG_RUNTIME_DIR` is expanded in the **consuming plugin's** process (see
/// the module docs), so this one string works for every uid.
/// `bridge_url_resolves_to_the_bridge_socket_path` pins it against
/// [`bridge_socket_path`], which is what the bridge itself binds.
pub const BRIDGE_BASE_URL: &str = "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock";

/// The scheme that routes a provider over a Unix socket.
pub const UNIX_SCHEME: &str = "unix://";

/// Alias for [`UNIX_SCHEME`], for people who have met the `http+unix` spelling
/// elsewhere. Identical behaviour.
const UNIX_SCHEME_ALIAS: &str = "http+unix://";

/// The only variable this parser expands, in its two ordinary spellings.
const RUNTIME_DIR_TOKENS: [&str; 2] = ["${XDG_RUNTIME_DIR}", "$XDG_RUNTIME_DIR"];

/// Authority put in the request line and `Host:` header when dialling a
/// socket. A Unix socket has no host, and the bridge routes on the path alone
/// — but HTTP/1.1 requires *something*, and ureq requires a resolvable-looking
/// URI to build a request from at all.
const UNIX_AUTHORITY: &str = "localhost";

/// The well-known bridge socket,
/// `$XDG_RUNTIME_DIR/`[`BRIDGE_SOCKET_DIR`]`/`[`BRIDGE_SOCKET_FILE`] — or
/// `None` if `XDG_RUNTIME_DIR` is unset.
///
/// Same-user-only by spec, with no fallback anywhere else: this is the one
/// definition `hytte-claude-bridge` binds and every client dials, so the two
/// ends cannot drift (the `hytte_plugin_proto::socket_path` shape, for the same
/// reason).
#[must_use]
pub fn bridge_socket_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(bridge_socket_path_in(Path::new(&base)))
}

/// [`bridge_socket_path`] with the runtime dir injected, so both ends can be
/// tested against a temp dir without mutating the process environment
/// (`unsafe` under edition 2024, which this crate forbids).
#[must_use]
pub fn bridge_socket_path_in(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(BRIDGE_SOCKET_DIR).join(BRIDGE_SOCKET_FILE)
}

/// The socket path a `unix://` base URL names, or `None` for an ordinary
/// `http(s)://` one.
///
/// `Some(Err(_))` is a URL that *is* a socket URL and could not be resolved —
/// an unset `$XDG_RUNTIME_DIR`, or a relative path. Never a TCP fallback.
pub(crate) fn socket_target(base_url: &str) -> Option<Result<PathBuf, String>> {
    let raw = base_url
        .strip_prefix(UNIX_SCHEME)
        .or_else(|| base_url.strip_prefix(UNIX_SCHEME_ALIAS))?;
    Some(resolve_socket_path(
        raw,
        std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .as_deref(),
    ))
}

/// Core of [`socket_target`] with the runtime dir injected, so the expansion is
/// unit-testable without touching the process environment.
pub(crate) fn resolve_socket_path(
    raw: &str,
    runtime_dir: Option<&Path>,
) -> Result<PathBuf, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(format!(
            "{UNIX_SCHEME} URL names no socket path; use {BRIDGE_BASE_URL}"
        ));
    }
    for token in RUNTIME_DIR_TOKENS {
        if let Some(rest) = trimmed.strip_prefix(token) {
            let Some(dir) = runtime_dir else {
                return Err(format!(
                    "{token} is unset, so {raw} names no socket — the bridge is \
                     reachable only over its same-uid socket, and there is no \
                     loopback fallback (#993)"
                ));
            };
            let rest = rest.trim_start_matches('/');
            return Ok(if rest.is_empty() {
                dir.to_path_buf()
            } else {
                dir.join(rest)
            });
        }
    }
    if !trimmed.starts_with('/') {
        return Err(format!(
            "{UNIX_SCHEME} needs an absolute socket path (or a \
             $XDG_RUNTIME_DIR-relative one), got {raw}"
        ));
    }
    Ok(PathBuf::from(trimmed))
}

/// The request URL used over a socket: the path is all the bridge routes on,
/// and the authority is a placeholder that is never resolved.
pub(crate) fn request_url(route: &str) -> String {
    format!("http://{UNIX_AUTHORITY}{route}")
}

/// Build an agent that dials `path` for every request `config` makes.
pub(crate) fn agent(config: Config, path: PathBuf) -> ureq::Agent {
    ureq::Agent::with_parts(config, UnixConnector { path }, UnixResolver)
}

// ── The ureq seams ───────────────────────────────────────────────────────────

/// Opens [`UnixTransport`]s to one fixed socket path, ignoring the resolved
/// addresses entirely.
#[derive(Debug)]
struct UnixConnector {
    path: PathBuf,
}

impl Connector for UnixConnector {
    type Out = UnixTransport;

    fn connect(
        &self,
        details: &ConnectionDetails,
        _chained: Option<()>,
    ) -> Result<Option<Self::Out>, Error> {
        // `connect(2)` on a Unix socket completes without a round trip, so the
        // configured connect timeout has nothing to bound here; the global
        // budget still covers send and read below.
        let stream = UnixStream::connect(&self.path).map_err(Error::Io)?;
        let buffers = LazyBuffers::new(
            details.config.input_buffer_size(),
            details.config.output_buffer_size(),
        );
        Ok(Some(UnixTransport {
            stream,
            buffers,
            timeout_read: None,
            timeout_write: None,
        }))
    }
}

/// Hands ureq a fixed loopback address it never uses: the connector dials a
/// path, but ureq resolves before it connects and requires at least one
/// address.
#[derive(Debug)]
struct UnixResolver;

impl Resolver for UnixResolver {
    fn resolve(
        &self,
        _uri: &Uri,
        _config: &Config,
        _timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, Error> {
        let mut addrs = self.empty();
        addrs.push(SocketAddr::from((Ipv4Addr::LOCALHOST, 80)));
        Ok(addrs)
    }
}

/// HTTP/1.1 over a [`UnixStream`] — `TcpTransport` with the socket swapped and
/// `TCP_NODELAY` dropped (a Unix socket has no Nagle).
struct UnixTransport {
    stream: UnixStream,
    buffers: LazyBuffers,
    timeout_read: Option<Duration>,
    timeout_write: Option<Duration>,
}

impl std::fmt::Debug for UnixTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnixTransport")
            .field("peer", &self.stream.peer_addr().ok())
            .finish_non_exhaustive()
    }
}

/// Whether an io error is this socket's timeout expiring. A `SO_RCVTIMEO` /
/// `SO_SNDTIMEO` expiry surfaces as `WouldBlock` on unix and `TimedOut`
/// elsewhere; both mean the same thing, and ureq wants
/// [`Error::Timeout`] rather than an io error so the caller sees a budget
/// overrun instead of a torn connection.
fn timed_out(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

impl Transport for UnixTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        let want = timeout.not_zero().map(|d| *d);
        if want != self.timeout_write {
            self.stream.set_write_timeout(want).map_err(Error::Io)?;
            self.timeout_write = want;
        }
        let output = &self.buffers.output()[..amount];
        match self.stream.write_all(output) {
            Ok(()) => Ok(()),
            Err(e) if timed_out(&e) => Err(Error::Timeout(timeout.reason)),
            Err(e) => Err(Error::Io(e)),
        }
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        let want = timeout.not_zero().map(|d| *d);
        if want != self.timeout_read {
            self.stream.set_read_timeout(want).map_err(Error::Io)?;
            self.timeout_read = want;
        }
        let input = self.buffers.input_append_buf();
        let amount = match self.stream.read(input) {
            Ok(n) => n,
            Err(e) if timed_out(&e) => return Err(Error::Timeout(timeout.reason)),
            Err(e) => return Err(Error::Io(e)),
        };
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        probe(&mut self.stream).unwrap_or(false)
    }
}

/// Whether `stream` is still usable: nothing unread waiting (the peer sending
/// unsolicited bytes means it is out of step) and not closed.
fn probe(stream: &mut UnixStream) -> std::io::Result<bool> {
    stream.set_nonblocking(true)?;
    let mut buf = [0u8; 1];
    let open = match stream.read(&mut buf) {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
        // Bytes we never asked for, or a closed/errored socket.
        Ok(_) | Err(_) => false,
    };
    stream.set_nonblocking(false)?;
    Ok(open)
}

#[cfg(test)]
mod tests {
    use super::{
        BRIDGE_BASE_URL, BRIDGE_SOCKET_DIR, BRIDGE_SOCKET_FILE, bridge_socket_path_in, request_url,
        resolve_socket_path, socket_target,
    };
    use std::path::{Path, PathBuf};

    /// The canonical env value every deployment carries must resolve to the
    /// exact path `hytte-claude-bridge` binds. These are the two ends of one
    /// wire contract; if they ever drift, every LLM plugin silently loses its
    /// brain with a connection error nobody can place.
    #[test]
    fn bridge_url_resolves_to_the_bridge_socket_path() {
        let runtime = Path::new("/run/user/1000");
        assert_eq!(
            resolve_socket_path(
                BRIDGE_BASE_URL.strip_prefix("unix://").expect("the scheme"),
                Some(runtime)
            )
            .expect("resolves"),
            bridge_socket_path_in(runtime),
        );
        assert_eq!(
            bridge_socket_path_in(runtime),
            PathBuf::from("/run/user/1000/trollshell/claude-bridge.sock"),
        );
        // …and the constants the bridge binds from are the ones in that path.
        assert_eq!(BRIDGE_SOCKET_DIR, "trollshell");
        assert_eq!(BRIDGE_SOCKET_FILE, "claude-bridge.sock");
    }

    /// An ordinary http(s) URL is not a socket URL — the remote providers are
    /// untouched by all of this.
    #[test]
    fn http_urls_are_not_socket_urls() {
        for url in [
            "http://127.0.0.1:8080",
            "https://openrouter.ai/api",
            "http://localhost:8787/",
        ] {
            assert!(socket_target(url).is_none(), "{url}");
        }
    }

    /// Both spellings of the scheme, and an explicit absolute path.
    #[test]
    fn both_scheme_spellings_name_an_absolute_path() {
        for url in [
            "unix:///run/user/7/trollshell/claude-bridge.sock",
            "http+unix:///run/user/7/trollshell/claude-bridge.sock",
        ] {
            let got = socket_target(url)
                .unwrap_or_else(|| panic!("{url} is a socket url"))
                .expect("resolves without any env");
            assert_eq!(
                got,
                PathBuf::from("/run/user/7/trollshell/claude-bridge.sock"),
                "{url}"
            );
        }
    }

    /// The `$XDG_RUNTIME_DIR` token is expanded in both spellings, and only at
    /// the front — this is not a shell.
    #[test]
    fn the_runtime_dir_token_is_expanded_in_both_spellings() {
        let runtime = Path::new("/run/user/1000");
        for raw in [
            "$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock",
            "${XDG_RUNTIME_DIR}/trollshell/claude-bridge.sock",
        ] {
            assert_eq!(
                resolve_socket_path(raw, Some(runtime)).expect("resolves"),
                PathBuf::from("/run/user/1000/trollshell/claude-bridge.sock"),
                "{raw}"
            );
        }
        // No other variable is expanded, and an embedded one stays literal.
        assert_eq!(
            resolve_socket_path("/run/$HOME/x.sock", Some(runtime)).expect("resolves"),
            PathBuf::from("/run/$HOME/x.sock"),
        );
    }

    /// **No silent TCP fallback.** With `$XDG_RUNTIME_DIR` unset there is no
    /// socket to dial and the client must say so — falling back to a loopback
    /// port is exactly the uid-blind hole #993 closed.
    #[test]
    fn an_unset_runtime_dir_is_an_error_naming_the_variable() {
        let err = resolve_socket_path("$XDG_RUNTIME_DIR/trollshell/x.sock", None)
            .expect_err("nothing to resolve against");
        assert!(err.contains("XDG_RUNTIME_DIR"), "{err}");
        // A relative path is a configuration mistake, not a cwd lookup.
        let err = resolve_socket_path("trollshell/x.sock", Some(Path::new("/run/user/1")))
            .expect_err("relative");
        assert!(err.contains("absolute"), "{err}");
        // An empty path names nothing.
        assert!(resolve_socket_path("", Some(Path::new("/run/user/1"))).is_err());
    }

    /// The request line carries only the route: a Unix socket has no host, and
    /// the placeholder authority must never leak a port or a path segment the
    /// bridge would then 404 on.
    #[test]
    fn the_request_url_is_the_route_under_a_placeholder_authority() {
        assert_eq!(
            request_url("/v1/chat/completions"),
            "http://localhost/v1/chat/completions"
        );
    }
}
