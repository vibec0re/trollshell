//! Wi-Fi backend probe: detects which daemon is managing Wi-Fi on this system.
//!
//! Queries `org.freedesktop.DBus.ListNames` on the system bus and returns
//! the backend to use. [`BackendChoice::NetworkManager`] is preferred when both
//! NM and iwd are present (most common desktop setup).

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
/// Returns [`BackendChoice::NetworkManager`] when both NM and iwd are present
/// (NM is the more common deployment). Falls back to
/// [`BackendChoice::Iwd`] when only iwd is present, or
/// [`BackendChoice::None`] when neither is found.
///
/// # Errors
///
/// Does not return an error — a bus failure is logged and treated as
/// [`BackendChoice::None`].
pub async fn probe_backend() -> BackendChoice {
    let names: Vec<String> = match hytte_bus::call("org.freedesktop.DBus")
        .bus(BusKind::System)
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .method("ListNames")
        .args(())
        .send::<Vec<String>>()
        .await
    {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "wifi_backend: ListNames failed");
            return BackendChoice::None;
        }
    };

    let has_nm = names.contains(&"org.freedesktop.NetworkManager".to_string());
    let has_iwd = names.contains(&"net.connman.iwd".to_string());

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

    /// Simulate what `probe_backend` does on the name list, without I/O.
    fn pick(names: &[&str]) -> BackendChoice {
        let has_nm = names.contains(&"org.freedesktop.NetworkManager");
        let has_iwd = names.contains(&"net.connman.iwd");
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
        assert_eq!(pick(&names), BackendChoice::NetworkManager);
    }

    #[test]
    fn falls_back_to_iwd_when_nm_absent() {
        let names = ["net.connman.iwd", "org.freedesktop.DBus"];
        assert_eq!(pick(&names), BackendChoice::Iwd);
    }

    #[test]
    fn returns_none_when_neither_present() {
        let names = ["org.freedesktop.DBus", "org.freedesktop.login1"];
        assert_eq!(pick(&names), BackendChoice::None);
    }
}
