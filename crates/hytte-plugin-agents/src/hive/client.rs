//! One request/response round trip over `/run/hyperhive/host.sock` (spec §5.3).
//!
//! **One connection per request**, which is `hivectl`'s own model
//! (`hivectl/src/client.rs:50-77`): connect, write one JSON line, read one
//! JSON line, drop the socket. At a two-second cadence a persistent connection
//! buys nothing and complicates the unreachable path.
//!
//! # No socket, no crash
//!
//! An absent path, `ECONNREFUSED` and `EACCES` all resolve to one
//! [`HiveError::Unreachable`], carrying a short operator-facing reason. The
//! caller renders a single "no hive" row and keeps its cadence: it never
//! panics, never exits, never busy-loops. A plugin that exited would be
//! restarted by its transient unit and flap; parking is correct.
//!
//! The permission case is the one worth naming, because it reads as "the
//! daemon is down" and is not: the socket is `0660 root:hive-admin` behind a
//! `0751` runtime dir (`nix/host-modules/hive-c0re/default.nix:390-398`), so a
//! non-member — or a *member* whose shell predates the group grant — gets
//! `EACCES` on connect.

use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;

use super::wire::{Request, Response, VersionMismatch, check_version};

/// How long one round trip may take before the poll gives up on it.
///
/// A wedged daemon must not stall the poll loop forever — the loop is what
/// keeps the row honest, and a hung request would freeze the last-good state
/// on screen with no way to tell it apart from a healthy quiet hive. Generous
/// against the 2 s default cadence, because the failure it guards is a hang,
/// not slowness.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a round trip did not produce a usable answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HiveError {
    /// The socket could not be reached at all. `reason` is short enough to
    /// ellipsize into a sidebar row.
    Unreachable {
        /// An operator-facing summary — the fix, not the errno.
        reason: String,
    },
    /// The socket answered, but not with something this build can read.
    Protocol {
        /// What went wrong parsing the line.
        reason: String,
    },
    /// The daemon speaks a wire version newer than this build (spec §5.2).
    Version(VersionMismatch),
    /// The daemon answered `ok: false`.
    Refused {
        /// The daemon's own `error` string, or a stand-in when it sent none.
        reason: String,
    },
}

impl std::fmt::Display for HiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable { reason } | Self::Protocol { reason } | Self::Refused { reason } => {
                f.write_str(reason)
            }
            Self::Version(VersionMismatch { theirs, ours }) => {
                write!(f, "hive protocol v{theirs}, plugin speaks v{ours}")
            }
        }
    }
}

/// Turn a connect `io::ErrorKind` into a row-sized reason that names the fix.
///
/// Deliberately terser than `hivectl`'s equivalent
/// (`hivectl/src/client.rs:26-47`): that one is a paragraph on a terminal,
/// this one has to fit an ellipsized sidebar line. The panel repeats the
/// socket path in full.
#[must_use]
pub fn connect_reason(kind: ErrorKind) -> String {
    match kind {
        ErrorKind::PermissionDenied => {
            "permission denied — needs `hive-admin` group (re-login after adding)".to_owned()
        }
        ErrorKind::NotFound => "no socket — hive-c0re is not running".to_owned(),
        ErrorKind::ConnectionRefused => "nothing listening — hive-c0re is down".to_owned(),
        _ => format!("cannot reach the hive ({kind:?})"),
    }
}

/// One round trip: connect, write `req` as a JSON line, read one line back.
///
/// # Errors
/// [`HiveError::Unreachable`] when the socket cannot be dialed or the round
/// trip exceeds [`REQUEST_TIMEOUT`]; [`HiveError::Protocol`] for a truncated
/// or unparseable answer; [`HiveError::Version`] when the daemon is newer than
/// this build; [`HiveError::Refused`] when the daemon answers `ok: false`.
pub async fn request(socket: &Path, req: &Request) -> Result<Response, HiveError> {
    match tokio::time::timeout(REQUEST_TIMEOUT, round_trip(socket, req)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(HiveError::Unreachable {
            reason: format!("hive did not answer within {}s", REQUEST_TIMEOUT.as_secs()),
        }),
    }
}

async fn round_trip(socket: &Path, req: &Request) -> Result<Response, HiveError> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|e| HiveError::Unreachable {
            reason: connect_reason(e.kind()),
        })?;
    let (read, mut write) = stream.into_split();

    let mut payload = serde_json::to_string(req).map_err(|e| HiveError::Protocol {
        reason: format!("could not encode {req:?}: {e}"),
    })?;
    payload.push('\n');
    write
        .write_all(payload.as_bytes())
        .await
        .map_err(|e| HiveError::Unreachable {
            reason: format!("write failed: {e}"),
        })?;
    write.flush().await.map_err(|e| HiveError::Unreachable {
        reason: format!("flush failed: {e}"),
    })?;

    let mut reader = BufReader::new(read);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| HiveError::Unreachable {
            reason: format!("read failed: {e}"),
        })?;
    if line.trim().is_empty() {
        return Err(HiveError::Protocol {
            reason: "hive closed the connection without answering".to_owned(),
        });
    }
    let resp: Response = serde_json::from_str(line.trim()).map_err(|e| HiveError::Protocol {
        reason: format!("unparseable answer: {e}"),
    })?;

    // The version check runs BEFORE `ok` is consulted: a response this build
    // may be misreading is not a response whose `ok` flag can be trusted.
    check_version(resp.version).map_err(HiveError::Version)?;

    if resp.ok {
        Ok(resp)
    } else {
        Err(HiveError::Refused {
            reason: resp
                .error
                .clone()
                .unwrap_or_else(|| "hive refused the request".to_owned()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{HiveError, connect_reason};
    use crate::hive::wire::{HOST_SOCK_VERSION, VersionMismatch};
    use std::io::ErrorKind;

    /// EACCES is an operator-side fix and must not blame the daemon — the
    /// same split `hivectl` makes, compressed to row width.
    #[test]
    fn permission_denied_names_the_group_not_the_daemon() {
        let r = connect_reason(ErrorKind::PermissionDenied);
        assert!(r.contains("hive-admin"), "{r}");
        assert!(!r.contains("not running"), "{r}");
    }

    #[test]
    fn a_missing_socket_blames_the_daemon_not_the_operator() {
        let r = connect_reason(ErrorKind::NotFound);
        assert!(r.contains("not running"), "{r}");
        assert!(!r.contains("hive-admin"), "{r}");
    }

    #[test]
    fn a_refused_socket_points_at_the_service() {
        let r = connect_reason(ErrorKind::ConnectionRefused);
        assert!(r.contains("nothing listening"), "{r}");
    }

    /// The version row's text is what the operator reads; spec §5.2 pins its
    /// shape ("hive protocol vN, plugin speaks vM").
    #[test]
    fn a_version_mismatch_renders_both_numbers() {
        let e = HiveError::Version(VersionMismatch {
            theirs: 9,
            ours: HOST_SOCK_VERSION,
        });
        let s = e.to_string();
        assert!(s.contains("v9"), "{s}");
        assert!(s.contains(&format!("v{HOST_SOCK_VERSION}")), "{s}");
    }
}
