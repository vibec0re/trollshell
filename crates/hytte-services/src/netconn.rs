//! Per-process active-connections service.
//!
//! Polls `ss -tunpH` every 2s and parses the output into a flat list
//! of `Connection { proto, local, remote, state, pid, program }`. The
//! PID/program columns appear only for sockets owned by the running
//! user; trollshell does not run as root, so other users' sockets show
//! up with `pid = None` and the UI groups them at the bottom.
//!
//! Failures (`ss` missing, parse error) log once and the signal stays
//! at its last known value.

use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_reactive::{Service, registry, spawn_supervised};
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Proto {
    Tcp,
    Tcp6,
    Udp,
    Udp6,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnState {
    Established,
    Listen,
    TimeWait,
    Close,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Connection {
    pub proto: Proto,
    pub local: SocketAddr,
    pub remote: Option<SocketAddr>,
    pub state: ConnState,
    pub pid: Option<u32>,
    pub program: Option<String>,
}

// ── Parser ───────────────────────────────────────────────────────────────────

/// Parse a single line of `ss -tunpH`. Returns `None` for unparseable
/// lines (we'd rather lose one line than panic the poll loop).
///
/// Expected line format with -tunpH:
///   <netid> <state> <recv-q> <send-q> <local> <peer> [users:((..pid=N..))]
///
/// Where:
/// - netid is one of: tcp, udp
/// - state is one of: ESTAB, LISTEN, TIME-WAIT, CLOSE, UNCONN, ...
/// - local/peer are `IP:PORT` (or `[IPv6]:PORT`); peer = `*:*` for LISTEN
/// - users column is optional; absent for sockets we can't see the owner of
pub(crate) fn parse_ss_line(line: &str) -> Option<Connection> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Split off any `users:((...))` tail — it's space-delimited but the
    // contents include commas/quotes. We treat it as everything from
    // the first `users:` token onward.
    let (head, users_tail) = match trimmed.find("users:") {
        Some(idx) => (trimmed[..idx].trim_end(), Some(&trimmed[idx..])),
        None => (trimmed, None),
    };
    let cols: Vec<&str> = head.split_whitespace().collect();
    if cols.len() < 6 {
        return None;
    }
    let netid = cols[0];
    let state_str = cols[1];
    let local_str = cols[4];
    let peer_str = cols[5];

    let proto = match netid {
        "tcp" if local_str.contains('[') => Proto::Tcp6,
        "tcp" => Proto::Tcp,
        "udp" if local_str.contains('[') => Proto::Udp6,
        "udp" => Proto::Udp,
        _ => return None,
    };
    let state = match state_str {
        "ESTAB" => ConnState::Established,
        "LISTEN" => ConnState::Listen,
        "TIME-WAIT" => ConnState::TimeWait,
        "CLOSE" => ConnState::Close,
        _ => ConnState::Other,
    };
    let local = parse_addr(local_str)?;
    let remote = if peer_str == "*:*" || peer_str == "[::]:*" {
        None
    } else {
        parse_addr(peer_str)
    };
    let (pid, program) = users_tail
        .and_then(parse_users_field)
        .map_or((None, None), |(pid, prog)| (Some(pid), Some(prog)));

    Some(Connection {
        proto,
        local,
        remote,
        state,
        pid,
        program,
    })
}

/// Parse a single ss endpoint string: `1.2.3.4:80` or `[::1]:80` or
/// `0.0.0.0:22`. Wildcard-port `*` is rejected.
fn parse_addr(s: &str) -> Option<SocketAddr> {
    if s.contains('*') {
        return None;
    }
    s.parse::<SocketAddr>().ok()
}

/// Pulls `(pid, program)` out of a `users:(("name",pid=N,fd=M))`-style
/// tail. Returns the first entry; ss may list multiple but the first
/// is the canonical owner for our purposes.
fn parse_users_field(tail: &str) -> Option<(u32, String)> {
    // Strip leading `users:((` and trailing `))`.
    let inner = tail.strip_prefix("users:((")?.strip_suffix("))")?;
    // Now `inner` looks like: "name",pid=1234,fd=78  optionally repeated
    // separated by `),(`. Take only the first entry.
    let first_entry = inner.split("),(").next()?;
    let parts: Vec<&str> = first_entry.splitn(3, ',').collect();
    if parts.len() < 2 {
        return None;
    }
    let name_quoted = parts[0];
    let name = name_quoted.trim_matches('"').to_string();
    let pid_kv = parts[1];
    let pid_val = pid_kv.strip_prefix("pid=")?;
    let pid = pid_val.parse::<u32>().ok()?;
    Some((pid, name))
}

pub(crate) fn parse_ss_output(output: &str) -> Vec<Connection> {
    output.lines().filter_map(parse_ss_line).collect()
}

// ── Service ──────────────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct NetconnHandles {
    pub(crate) connections: Mutable<Vec<Connection>>,
    /// Gate for the `ss` poller. While `false`, the poll loop parks and forks
    /// nothing; flipping it back to `true` resumes sampling immediately (the
    /// loop `select!`s on this so reactivation isn't delayed a full tick).
    ///
    /// Defaults to `true` so the first sample is taken eagerly at startup —
    /// the list is then already populated the instant the drawer opens, and
    /// `set_active(false)` parks it once the binary reports the relevant
    /// panels are hidden. See `set_active`.
    pub(crate) active: Mutable<bool>,
}

impl Default for NetconnHandles {
    fn default() -> Self {
        Self {
            connections: Mutable::new(Vec::new()),
            active: Mutable::new(true),
        }
    }
}

pub struct NetconnService;

impl Service for NetconnService {
    type Handles = NetconnHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = NetconnHandles::default();
        let writer = handles.connections.clone();
        let active = handles.active.clone();
        spawn_supervised("netconn", move || {
            let writer = writer.clone();
            let active = active.clone();
            async move {
                poll_loop(writer, active).await;
            }
        });
        handles
    }
}

async fn poll_loop(writer: Mutable<Vec<Connection>>, active: Mutable<bool>) {
    loop {
        // Park (forking nothing) while gated inactive. `wait_for(true)` resolves
        // as soon as `set_active(true)` lands — `Mutable::signal()` replays the
        // current value on first poll, so if we've already been reactivated by
        // the time we get here it returns immediately, with no lost wakeup.
        // Reactivation is thus instant rather than waiting out a sleep tick.
        if !active.get() {
            let _ = active.signal().wait_for(true).await;
        }
        if let Some(out) = run_ss().await {
            let next = parse_ss_output(&out);
            // Avoid no-op re-emissions: the signal would still emit because
            // `set` always notifies, so compare and skip when unchanged.
            if writer.lock_ref().clone() != next {
                writer.set(next);
            }
        }
        // Sleep the inter-sample interval, but bail out early if we get gated
        // inactive mid-wait — no point holding the timer when parked. The
        // top-of-loop park then handles the resume edge.
        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(2)) => {}
            _ = active.signal().wait_for(false) => {}
        }
    }
}

async fn run_ss() -> Option<String> {
    let result = tokio::process::Command::new("ss")
        .args(["-tunpH"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await;
    match result {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "netconn: ss spawn failed");
            None
        }
    }
}

#[must_use]
pub fn service() -> NetconnService {
    NetconnService
}

pub fn connections() -> impl Signal<Item = Vec<Connection>> {
    registry::with(|r| {
        r.get::<NetconnHandles>()
            .expect("netconn::service() not registered")
            .connections
            .signal_cloned()
    })
}

/// Gate the `ss` poller: `true` resumes 2 s sampling (immediately taking a
/// fresh sample), `false` parks it so it forks nothing while the
/// Connections/Network drawer panels are hidden.
///
/// Fire-and-forget command: the binary wires the drawer-visibility signal to
/// this so the always-on poller idles when no one's looking (#50). A no-op
/// `set` to the same value is skipped to avoid spurious loop wakeups.
pub fn set_active(active: bool) {
    registry::with(|r| {
        let handle = &r
            .get::<NetconnHandles>()
            .expect("netconn::service() not registered")
            .active;
        if handle.get() != active {
            handle.set(active);
        }
    });
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn parse_ss_line_with_users_field() {
        let line =
            "tcp ESTAB 0 0 192.168.1.10:54321 1.2.3.4:443 users:((\"firefox\",pid=1234,fd=78))";
        let c = parse_ss_line(line).unwrap();
        assert_eq!(c.proto, Proto::Tcp);
        assert_eq!(c.state, ConnState::Established);
        assert_eq!(c.local.ip(), IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)));
        assert_eq!(c.local.port(), 54321);
        assert_eq!(c.remote.unwrap().port(), 443);
        assert_eq!(c.pid, Some(1234));
        assert_eq!(c.program.as_deref(), Some("firefox"));
    }

    #[test]
    fn parse_ss_line_without_users_field() {
        let line = "tcp ESTAB 0 0 10.0.0.5:42210 8.8.8.8:443";
        let c = parse_ss_line(line).unwrap();
        assert_eq!(c.proto, Proto::Tcp);
        assert!(c.pid.is_none());
        assert!(c.program.is_none());
    }

    #[test]
    fn parse_ss_line_listen_state_has_no_remote() {
        let line = "tcp LISTEN 0 4096 0.0.0.0:22 *:* users:((\"sshd\",pid=900,fd=3))";
        let c = parse_ss_line(line).unwrap();
        assert_eq!(c.state, ConnState::Listen);
        assert!(c.remote.is_none());
        assert_eq!(c.pid, Some(900));
    }

    #[test]
    fn parse_ss_line_v6() {
        let line = "tcp ESTAB 0 0 [2001:db8::1]:54321 [2001:db8::2]:443";
        let c = parse_ss_line(line).unwrap();
        assert_eq!(c.proto, Proto::Tcp6);
        assert_eq!(
            c.local.ip(),
            IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap())
        );
    }

    #[test]
    fn parse_ss_line_garbage_is_none() {
        assert!(parse_ss_line("").is_none());
        assert!(parse_ss_line("just one column").is_none());
        assert!(parse_ss_line("ipv9 ESTAB 0 0 a:1 b:2").is_none());
    }

    #[test]
    fn parse_ss_output_runs_through_lines() {
        let output = "tcp ESTAB 0 0 10.0.0.5:42210 8.8.8.8:443
udp UNCONN 0 0 10.0.0.5:42424 1.1.1.1:53 users:((\"systemd-resolved\",pid=500,fd=12))
garbage line that should be skipped
tcp LISTEN 0 4096 0.0.0.0:22 *:*";
        let cs = parse_ss_output(output);
        assert_eq!(cs.len(), 3);
    }
}
