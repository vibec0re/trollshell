//! Wi-Fi backend probe: detects which daemon is managing Wi-Fi on this system.
//!
//! Queries both `org.freedesktop.DBus.ListNames` (currently-owned names) and
//! `org.freedesktop.DBus.ListActivatableNames` (names that can be
//! socket-activated) on the system bus, and returns the backend to use.
//! [`BackendChoice::NetworkManager`] is preferred when both NM and iwd are
//! present (most common desktop setup).

use hytte_bus::BusKind;

// ── Backend discriminant ──────────────────────────────────────────────────────

/// Which daemon should back the [`crate::wifi`] service on this host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendChoice {
    /// iwd (`net.connman.iwd`) owns the radio.
    Iwd,
    /// `NetworkManager` (`org.freedesktop.NetworkManager`) owns the radio.
    NetworkManager,
    /// Neither daemon is running; the service will start in a no-op state.
    None,
}

// ── Runtime probe ─────────────────────────────────────────────────────────────

/// Probe the system bus for known Wi-Fi backend daemons.
///
/// Queries both `ListNames` (currently-owned bus names) and
/// `ListActivatableNames` (socket-activatable names) so that a daemon which is
/// still initialising or socket-activated at boot is not missed. A daemon is
/// considered present if it appears in **either** list.
///
/// Returns [`BackendChoice::NetworkManager`] when both NM and iwd are present
/// (NM is the more common deployment). Falls back to
/// [`BackendChoice::Iwd`] when only iwd is present, or
/// [`BackendChoice::None`] when neither is found.
///
/// # Errors
///
/// Does not return an error — any bus failure is logged and treated as an
/// empty name list; the other call's result still contributes.
pub async fn probe_backend() -> BackendChoice {
    let owned: Vec<String> = hytte_bus::call(BusKind::System, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("ListNames")
        .args(())
        .send::<Vec<String>>()
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "wifi_backend: ListNames failed");
            Vec::new()
        });

    let activatable: Vec<String> = hytte_bus::call(BusKind::System, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("ListActivatableNames")
        .args(())
        .send::<Vec<String>>()
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "wifi_backend: ListActivatableNames failed");
            Vec::new()
        });

    let has =
        |name: &str| owned.contains(&name.to_string()) || activatable.contains(&name.to_string());
    let has_nm = has("org.freedesktop.NetworkManager");
    let has_iwd = has("net.connman.iwd");

    if has_nm {
        BackendChoice::NetworkManager
    } else if has_iwd {
        BackendChoice::Iwd
    } else {
        BackendChoice::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_choice_variants_distinct() {
        assert_ne!(BackendChoice::Iwd, BackendChoice::NetworkManager);
        assert_ne!(BackendChoice::Iwd, BackendChoice::None);
        assert_ne!(BackendChoice::NetworkManager, BackendChoice::None);
    }

    /// Simulate what `probe_backend` does on two name lists (owned + activatable),
    /// without I/O.
    fn pick(owned: &[&str], activatable: &[&str]) -> BackendChoice {
        let has = |name: &str| owned.contains(&name) || activatable.contains(&name);
        let has_nm = has("org.freedesktop.NetworkManager");
        let has_iwd = has("net.connman.iwd");
        if has_nm {
            BackendChoice::NetworkManager
        } else if has_iwd {
            BackendChoice::Iwd
        } else {
            BackendChoice::None
        }
    }

    #[test]
    fn prefers_nm_when_both_present() {
        let names = [
            "org.freedesktop.NetworkManager",
            "net.connman.iwd",
            "org.freedesktop.DBus",
        ];
        assert_eq!(pick(&names, &[]), BackendChoice::NetworkManager);
    }

    #[test]
    fn falls_back_to_iwd_when_nm_absent() {
        let names = ["net.connman.iwd", "org.freedesktop.DBus"];
        assert_eq!(pick(&names, &[]), BackendChoice::Iwd);
    }

    #[test]
    fn returns_none_when_neither_present() {
        let names = ["org.freedesktop.DBus", "org.freedesktop.login1"];
        assert_eq!(pick(&names, &[]), BackendChoice::None);
    }

    #[test]
    fn detects_nm_in_activatable_only() {
        // NM is socket-activated — not yet in owned names.
        let owned = ["org.freedesktop.DBus"];
        let activatable = ["org.freedesktop.NetworkManager", "net.connman.iwd"];
        assert_eq!(pick(&owned, &activatable), BackendChoice::NetworkManager);
    }

    #[test]
    fn detects_iwd_in_activatable_only() {
        let owned = ["org.freedesktop.DBus"];
        let activatable = ["net.connman.iwd"];
        assert_eq!(pick(&owned, &activatable), BackendChoice::Iwd);
    }

    #[test]
    fn prefers_nm_when_nm_activatable_iwd_owned() {
        // NM only activatable, iwd already running — NM still takes priority.
        let owned = ["net.connman.iwd", "org.freedesktop.DBus"];
        let activatable = ["org.freedesktop.NetworkManager"];
        assert_eq!(pick(&owned, &activatable), BackendChoice::NetworkManager);
    }
}
