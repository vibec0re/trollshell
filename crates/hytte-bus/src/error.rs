//! Error types for the hytte-bus layer.
//!
//! `BusError` distinguishes transient (bus mid-reconnect) from permanent
//! (the operation itself failed) failure modes. The mapping from
//! `zbus::Error` to `BusError` lives here as the single source of truth.

use thiserror::Error;

/// Outcome of a bus operation that the consumer might want to handle
/// differently depending on whether the failure is transient (retry will
/// likely succeed once the supervisor reconnects) or permanent (the
/// operation will never succeed; consumer must decide how to surface it).
#[derive(Debug, Error)]
pub enum BusError {
    /// The bus connection was lost while the operation was in flight.
    /// `RetryPolicy::Once` (the `call` default) automatically retries this
    /// once after the supervisor re-establishes the connection.
    #[error("bus connection transient failure: {source}")]
    Transient {
        #[source]
        source: zbus::Error,
    },

    /// The operation itself failed in a way that retrying will not fix
    /// (`UnknownMethod`, type mismatch, peer rejected the args, etc.). The
    /// consumer must decide what to do.
    #[error("bus operation permanently failed: {reason}")]
    Permanent {
        /// Human-readable description.
        reason: String,
        /// Originating D-Bus error name (e.g. `org.freedesktop.DBus.Error.UnknownMethod`)
        /// when the underlying error carried one.
        dbus_name: Option<String>,
    },
}

impl BusError {
    /// Map a `zbus::Error` produced by an in-flight operation to a
    /// `BusError`. Connection-level failures (`InputOutput`, FDO
    /// `Disconnected`) become `Transient`; method-level failures become
    /// `Permanent`.
    ///
    /// Note: `zbus` 5.x removed the top-level `Disconnected` variant; the
    /// equivalent is `zbus::Error::FDO(Box<fdo::Error::Disconnected>)`.
    #[must_use]
    pub fn from_zbus(err: zbus::Error) -> Self {
        if is_transient_zbus_error(&err) {
            return Self::Transient { source: err };
        }
        match &err {
            zbus::Error::FDO(fdo_err) => Self::Permanent {
                reason: fdo_err.to_string(),
                dbus_name: None,
            },
            zbus::Error::MethodError(name, msg, _) => Self::Permanent {
                reason: msg.clone().unwrap_or_else(|| name.to_string()),
                dbus_name: Some(name.to_string()),
            },
            _ => Self::Permanent {
                reason: err.to_string(),
                dbus_name: None,
            },
        }
    }

    /// True if this error is a transient bus-level failure (and therefore
    /// a candidate for retry across a supervisor reconnect).
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient { .. })
    }
}

/// Returns `true` for `zbus::Error` variants that indicate a lost or
/// unavailable connection (transient; the supervisor should reconnect and
/// callers should retry). Single classification point — consumed by both
/// `BusError::from_zbus` and `SharedConnection::with_conn`.
///
/// In zbus 5.x the top-level `Disconnected` variant was removed; the
/// equivalent is `zbus::Error::FDO(Box<fdo::Error::Disconnected>)`.
pub(crate) fn is_transient_zbus_error(err: &zbus::Error) -> bool {
    match err {
        zbus::Error::InputOutput(_) => true,
        zbus::Error::FDO(fdo_err) => matches!(**fdo_err, zbus::fdo::Error::Disconnected(_)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn io_error_is_transient() {
        let io_err = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        let bus_err = BusError::from_zbus(zbus::Error::InputOutput(Arc::new(io_err)));
        assert!(bus_err.is_transient(), "InputOutput must be Transient");
    }

    #[test]
    fn fdo_disconnected_is_transient() {
        let fdo = zbus::fdo::Error::Disconnected("peer gone".to_owned());
        let bus_err = BusError::from_zbus(zbus::Error::FDO(Box::new(fdo)));
        assert!(bus_err.is_transient(), "FDO Disconnected must be Transient");
    }

    #[test]
    fn service_unknown_is_permanent() {
        // An absent daemon (e.g. UPower disabled) surfaces as ServiceUnknown.
        let raw = zbus::Error::FDO(Box::new(zbus::fdo::Error::ServiceUnknown(
            "org.freedesktop.UPower".to_owned(),
        )));
        assert!(
            !is_transient_zbus_error(&raw),
            "ServiceUnknown must not be treated as transient"
        );
        assert!(
            !BusError::from_zbus(raw).is_transient(),
            "ServiceUnknown must be Permanent"
        );
    }

    #[test]
    fn access_denied_is_permanent() {
        let raw = zbus::Error::FDO(Box::new(zbus::fdo::Error::AccessDenied(
            "not authorised".to_owned(),
        )));
        assert!(
            !BusError::from_zbus(raw).is_transient(),
            "AccessDenied must be Permanent"
        );
    }

    #[test]
    fn unknown_object_is_permanent() {
        let raw = zbus::Error::FDO(Box::new(zbus::fdo::Error::UnknownObject(
            "/no/such/object".to_owned(),
        )));
        assert!(
            !BusError::from_zbus(raw).is_transient(),
            "UnknownObject must be Permanent"
        );
    }

    #[test]
    fn method_error_is_permanent_with_dbus_name() {
        // Build a minimal method-call Message to satisfy the MethodError variant's
        // third field.  The message contents are not examined by from_zbus; only
        // the error name and detail string matter.
        let msg = zbus::Message::method_call("/", "Foo")
            .unwrap()
            .build(&())
            .unwrap();

        let raw = zbus::Error::MethodError(
            "org.freedesktop.DBus.Error.UnknownMethod"
                .try_into()
                .unwrap(),
            Some("no such method".to_owned()),
            msg,
        );
        let bus_err = BusError::from_zbus(raw);
        match bus_err {
            BusError::Permanent { reason, dbus_name } => {
                assert_eq!(reason, "no such method");
                assert_eq!(
                    dbus_name.as_deref(),
                    Some("org.freedesktop.DBus.Error.UnknownMethod")
                );
            }
            BusError::Transient { .. } => panic!("MethodError must be Permanent"),
        }
    }
}
