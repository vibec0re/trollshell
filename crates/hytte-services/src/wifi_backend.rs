//! Wi-Fi backend probe: detects which daemon is managing Wi-Fi on this system.
//!
//! Queries both `org.freedesktop.DBus.ListNames` (currently-owned names) and
//! `org.freedesktop.DBus.ListActivatableNames` (names that can be
//! socket-activated) on the system bus, and returns the backend to use.
//! [`BackendChoice::NetworkManager`] is preferred when both NM and iwd are
//! present (most common desktop setup).
//!
//! # "Nobody is there" is not "I could not ask" (issue #607)
//!
//! The service spawns a different watcher per verdict, so the first
//! **conclusive** verdict is latched for the lifetime of the process. That makes
//! it critical that a *failed* query can never masquerade as a *negative*
//! answer. (Since #613 the caller re-runs the probe while it is inconclusive,
//! and only commits on a `Ok(_)`; see [`crate::wifi`]'s `probe_until_conclusive`.)
//!
//! Before #607 both bus calls collapsed their errors into an empty name list,
//! so a single transient failure at startup (system bus not reachable yet,
//! pooled connection not established) was indistinguishable from "the bus
//! answered and neither daemon is there". The probe returned
//! [`BackendChoice::None`], no watcher was spawned, and Wi-Fi stayed dead for
//! the whole session with no in-UI signal.
//!
//! [`probe_backend`] therefore returns a `Result`: [`BackendChoice::None`] is
//! now a *positive* statement ("both queries answered; neither daemon is
//! present"), and [`ProbeError`] is the "I could not ask" case. A `Result`
//! rather than a `BackendChoice::Unknown` variant specifically because the
//! compiler then rejects `probe_backend().await == BackendChoice::NetworkManager`
//! — the exact expression shape that silently read a failure as "NM is absent".
//!
//! A *positive* hit still wins over a failed query: if `ListNames` fails but
//! `ListActivatableNames` names `NetworkManager`, that is a trustworthy yes.
//! Only a **negative** requires that both queries actually answered.
//!
//! Re-probing *after* a conclusive verdict — picking up a daemon that appears
//! later, or switching between iwd and `NetworkManager` at runtime — is
//! deliberately still unsolved: it needs a cancellation primitive that does not
//! exist yet. See #633.

use hytte_bus::BusKind;

const NM_BUS_NAME: &str = "org.freedesktop.NetworkManager";
const IWD_BUS_NAME: &str = "net.connman.iwd";

const DBUS_NAME: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_IFACE: &str = "org.freedesktop.DBus";

// ── Backend discriminant ──────────────────────────────────────────────────────

/// Which daemon should back the [`crate::wifi`] service on this host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendChoice {
    /// iwd (`net.connman.iwd`) owns the radio.
    Iwd,
    /// `NetworkManager` (`org.freedesktop.NetworkManager`) owns the radio.
    NetworkManager,
    /// The bus **answered** and neither daemon is present or activatable; the
    /// service will start in a no-op state. This is a positive finding — a
    /// probe that could not reach the bus yields [`ProbeError`] instead.
    None,
}

// ── Probe failure ─────────────────────────────────────────────────────────────

/// The probe could not reach a trustworthy verdict: at least one bus-name
/// query failed, and the list(s) that *did* answer named no known backend.
///
/// This is emphatically **not** [`BackendChoice::None`] — it means "I could
/// not ask", not "nobody is there". Callers must not treat it as evidence that
/// a Wi-Fi daemon is absent (issue #607).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProbeError {
    /// `ListNames` failed; `ListActivatableNames` answered but named no known
    /// backend. A daemon that is currently running would not be seen.
    ListNames(String),
    /// `ListActivatableNames` failed; `ListNames` answered but named no known
    /// backend. A socket-activatable daemon would not be seen.
    ListActivatableNames(String),
    /// Both queries failed — nothing at all is known about the bus.
    Both {
        /// The `ListNames` failure.
        list_names: String,
        /// The `ListActivatableNames` failure.
        list_activatable_names: String,
    },
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ListNames(e) => write!(
                f,
                "could not query the system bus for a Wi-Fi backend: ListNames failed ({e}); \
                 a running daemon would have been missed"
            ),
            Self::ListActivatableNames(e) => write!(
                f,
                "could not query the system bus for a Wi-Fi backend: ListActivatableNames \
                 failed ({e}); a socket-activatable daemon would have been missed"
            ),
            Self::Both {
                list_names,
                list_activatable_names,
            } => write!(
                f,
                "could not query the system bus for a Wi-Fi backend: ListNames failed \
                 ({list_names}) and ListActivatableNames failed ({list_activatable_names}); \
                 nothing is known about the bus"
            ),
        }
    }
}

impl std::error::Error for ProbeError {}

// ── Pure decision core ────────────────────────────────────────────────────────

/// Turn the two name-query outcomes into a verdict, without I/O.
///
/// `Err(reason)` for either argument means *that query failed*. A positive hit
/// in whichever list answered is trustworthy on its own; a negative is only
/// trustworthy when **both** queries answered.
fn decide(
    owned: Result<&[String], &str>,
    activatable: Result<&[String], &str>,
) -> Result<BackendChoice, ProbeError> {
    let listed = |name: &str| {
        let seen =
            |list: Result<&[String], &str>| list.is_ok_and(|got| got.iter().any(|n| n == name));
        seen(owned) || seen(activatable)
    };

    // NM wins when both are present (the more common deployment).
    if listed(NM_BUS_NAME) {
        return Ok(BackendChoice::NetworkManager);
    }
    if listed(IWD_BUS_NAME) {
        return Ok(BackendChoice::Iwd);
    }

    // Nothing found. Only claim "no backend" if we actually got both answers.
    match (owned, activatable) {
        (Ok(_), Ok(_)) => Ok(BackendChoice::None),
        (Err(e), Ok(_)) => Err(ProbeError::ListNames(e.to_owned())),
        (Ok(_), Err(e)) => Err(ProbeError::ListActivatableNames(e.to_owned())),
        (Err(owned_err), Err(activatable_err)) => Err(ProbeError::Both {
            list_names: owned_err.to_owned(),
            list_activatable_names: activatable_err.to_owned(),
        }),
    }
}

// ── Runtime probe ─────────────────────────────────────────────────────────────

/// Run one `org.freedesktop.DBus` name-listing method on the system bus.
async fn list_bus_names(method: &'static str) -> Result<Vec<String>, String> {
    hytte_bus::call(BusKind::System, DBUS_NAME)
        .at_path(DBUS_PATH)
        .iface(DBUS_IFACE)
        .method(method)
        .args(())
        .send::<Vec<String>>()
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, method, "wifi_backend: bus name query failed");
            e.to_string()
        })
}

/// Probe the system bus for known Wi-Fi backend daemons.
///
/// Queries both `ListNames` (currently-owned bus names) and
/// `ListActivatableNames` (socket-activatable names) so that a daemon which is
/// still initialising or socket-activated at boot is not missed. A daemon is
/// considered present if it appears in **either** list.
///
/// Returns [`BackendChoice::NetworkManager`] when both NM and iwd are present
/// (NM is the more common deployment), [`BackendChoice::Iwd`] when only iwd is
/// present, and [`BackendChoice::None`] when both queries answered and neither
/// daemon is there.
///
/// # Errors
///
/// Returns [`ProbeError`] when a bus query failed *and* the query that did
/// answer named no known backend — i.e. the probe could not establish whether
/// a daemon is present. Callers must not collapse this into "no backend"
/// (issue #607): a latched false negative disables Wi-Fi for the whole
/// process lifetime.
pub async fn probe_backend() -> Result<BackendChoice, ProbeError> {
    let owned = list_bus_names("ListNames").await;
    let activatable = list_bus_names("ListActivatableNames").await;

    let verdict = decide(
        owned.as_ref().map(Vec::as_slice).map_err(String::as_str),
        activatable
            .as_ref()
            .map(Vec::as_slice)
            .map_err(String::as_str),
    );

    match &verdict {
        Ok(choice) => tracing::debug!(?choice, "wifi_backend: probe verdict"),
        Err(e) => tracing::error!(
            error = %e,
            "wifi_backend: probe inconclusive — this is NOT the same as 'no Wi-Fi daemon \
             is present'; callers must not treat it as one"
        ),
    }

    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn backend_choice_variants_distinct() {
        assert_ne!(BackendChoice::Iwd, BackendChoice::NetworkManager);
        assert_ne!(BackendChoice::Iwd, BackendChoice::None);
        assert_ne!(BackendChoice::NetworkManager, BackendChoice::None);
    }

    // ── Both queries answered: the pre-#607 behaviour, unchanged ──────────────

    #[test]
    fn prefers_nm_when_both_present() {
        let owned = names(&[NM_BUS_NAME, IWD_BUS_NAME, DBUS_NAME]);
        assert_eq!(
            decide(Ok(&owned), Ok(&[])),
            Ok(BackendChoice::NetworkManager)
        );
    }

    #[test]
    fn falls_back_to_iwd_when_nm_absent() {
        let owned = names(&[IWD_BUS_NAME, DBUS_NAME]);
        assert_eq!(decide(Ok(&owned), Ok(&[])), Ok(BackendChoice::Iwd));
    }

    #[test]
    fn returns_none_when_neither_present() {
        let owned = names(&[DBUS_NAME, "org.freedesktop.login1"]);
        assert_eq!(decide(Ok(&owned), Ok(&[])), Ok(BackendChoice::None));
    }

    #[test]
    fn detects_nm_in_activatable_only() {
        // NM is socket-activated — not yet in owned names.
        let owned = names(&[DBUS_NAME]);
        let activatable = names(&[NM_BUS_NAME, IWD_BUS_NAME]);
        assert_eq!(
            decide(Ok(&owned), Ok(&activatable)),
            Ok(BackendChoice::NetworkManager)
        );
    }

    #[test]
    fn detects_iwd_in_activatable_only() {
        let owned = names(&[DBUS_NAME]);
        let activatable = names(&[IWD_BUS_NAME]);
        assert_eq!(decide(Ok(&owned), Ok(&activatable)), Ok(BackendChoice::Iwd));
    }

    #[test]
    fn prefers_nm_when_nm_activatable_iwd_owned() {
        // NM only activatable, iwd already running — NM still takes priority.
        let owned = names(&[IWD_BUS_NAME, DBUS_NAME]);
        let activatable = names(&[NM_BUS_NAME]);
        assert_eq!(
            decide(Ok(&owned), Ok(&activatable)),
            Ok(BackendChoice::NetworkManager)
        );
    }

    // ── A positive hit survives a failed sibling query ────────────────────────

    #[test]
    fn nm_in_owned_wins_even_if_activatable_query_failed() {
        let owned = names(&[NM_BUS_NAME, DBUS_NAME]);
        assert_eq!(
            decide(Ok(&owned), Err("connection reset")),
            Ok(BackendChoice::NetworkManager)
        );
    }

    #[test]
    fn iwd_in_activatable_wins_even_if_owned_query_failed() {
        let activatable = names(&[IWD_BUS_NAME]);
        assert_eq!(
            decide(Err("connection reset"), Ok(&activatable)),
            Ok(BackendChoice::Iwd)
        );
    }

    // ── #607: a failed query must never read as "no backend" ─────────────────

    #[test]
    fn failed_owned_query_with_no_hit_is_an_error_not_none() {
        let activatable = names(&[DBUS_NAME]);
        assert_eq!(
            decide(Err("bus not reachable"), Ok(&activatable)),
            Err(ProbeError::ListNames("bus not reachable".to_string()))
        );
    }

    #[test]
    fn failed_activatable_query_with_no_hit_is_an_error_not_none() {
        let owned = names(&[DBUS_NAME]);
        assert_eq!(
            decide(Ok(&owned), Err("bus not reachable")),
            Err(ProbeError::ListActivatableNames(
                "bus not reachable".to_string()
            ))
        );
    }

    #[test]
    fn both_queries_failing_is_an_error_not_none() {
        assert_eq!(
            decide(Err("no owned"), Err("no activatable")),
            Err(ProbeError::Both {
                list_names: "no owned".to_string(),
                list_activatable_names: "no activatable".to_string(),
            })
        );
    }

    /// The regression #607 turns on: a transient bus failure and a genuinely
    /// backend-free host must not produce the same value. Before the fix both
    /// collapsed to `BackendChoice::None`, and the caller latched that verdict
    /// for the process lifetime.
    #[test]
    fn transient_failure_is_distinguishable_from_a_backendless_host() {
        let empty = names(&[DBUS_NAME]);
        let backendless = decide(Ok(&empty), Ok(&empty));
        let transient = decide(Err("bus not reachable"), Err("bus not reachable"));

        assert_eq!(backendless, Ok(BackendChoice::None));
        assert!(transient.is_err());
        assert_ne!(backendless, transient);
    }

    #[test]
    fn probe_error_display_names_the_failed_query() {
        assert!(
            ProbeError::ListNames("boom".to_string())
                .to_string()
                .contains("ListNames failed (boom)")
        );
        assert!(
            ProbeError::ListActivatableNames("boom".to_string())
                .to_string()
                .contains("ListActivatableNames failed (boom)")
        );
        let both = ProbeError::Both {
            list_names: "a".to_string(),
            list_activatable_names: "b".to_string(),
        }
        .to_string();
        assert!(both.contains("ListNames failed (a)"));
        assert!(both.contains("ListActivatableNames failed (b)"));
    }
}
