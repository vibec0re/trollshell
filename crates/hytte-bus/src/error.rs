//! Error types for the hytte-bus layer.
//!
//! `BusError` distinguishes transient (bus mid-reconnect) from permanent
//! (the operation itself failed) failure modes, and keeps a call that ran out
//! of its own deadline apart from both. The mapping from `zbus::Error` to
//! `BusError` lives here as the single source of truth.

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

    /// The call got no reply before its own deadline: the `timeout` on
    /// [`CallBuilder`](crate::CallBuilder) or
    /// [`ProxyBuilder`](crate::ProxyBuilder). It says nothing about what the
    /// peer did with the call, which may still be carried out.
    ///
    /// This is `hytte-bus`'s own timer only. The bus daemon's
    /// `org.freedesktop.DBus.Error.NoReply` is an answer from the bus, and
    /// arrives as [`Permanent`](Self::Permanent) with that `dbus_name`.
    ///
    /// Like `Permanent`, it is not transient: no [`RetryPolicy`] retries it.
    /// Until #1551 it *was* a `Permanent`, recognisable only by its `reason`
    /// string, so a caller that needed to tell "no answer" from "an error
    /// answer" had to match that string. Match the variant, or
    /// [`is_timeout`](Self::is_timeout).
    ///
    /// [`RetryPolicy`]: crate::RetryPolicy
    #[error("bus operation permanently failed: {reason}")]
    Timeout {
        /// Human-readable description. The variant, not this text, is what
        /// says the deadline ran out.
        reason: String,
    },
}

/// `reason` of the [`BusError::Timeout`] a `call()` answers when its deadline
/// runs out. The wording predates the variant and is kept, so `Display` (and
/// every toast and log line built from it) reads as it did.
const CALL_TIMEOUT: &str = "call timeout";

/// [`CALL_TIMEOUT`] for a [`BusProxy::call`](crate::BusProxy::call).
const PROXY_CALL_TIMEOUT: &str = "proxy call timeout";

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

    /// The [`Timeout`](Self::Timeout) a `call()` answers when its own
    /// deadline runs out.
    pub(crate) fn call_timeout() -> Self {
        Self::Timeout {
            reason: CALL_TIMEOUT.to_owned(),
        }
    }

    /// The [`Timeout`](Self::Timeout) a [`BusProxy::call`](crate::BusProxy::call)
    /// answers when the proxy's deadline runs out.
    pub(crate) fn proxy_call_timeout() -> Self {
        Self::Timeout {
            reason: PROXY_CALL_TIMEOUT.to_owned(),
        }
    }

    /// True if this error is a transient bus-level failure (and therefore
    /// a candidate for retry across a supervisor reconnect).
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient { .. })
    }

    /// True if the call got no reply before its own deadline
    /// ([`Timeout`](Self::Timeout)), rather than an answer that was an error.
    ///
    /// False for the bus daemon's `org.freedesktop.DBus.Error.NoReply`: see
    /// [`Timeout`](Self::Timeout) for why the two are kept apart.
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout { .. })
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
            other => panic!("MethodError must be Permanent, got {other:?}"),
        }
    }

    /// The deadline is a variant, not a string: a `zbus` `Failure` that reads
    /// exactly like it is still an ordinary `Permanent`, and the `Timeout`
    /// the call path builds is recognised by its type whatever it says.
    ///
    /// **Falsification:** make `is_timeout` match `Permanent` with a
    /// `reason` of `"call timeout"` (the string coupling #1551 removed) and
    /// the first assertion fails.
    #[test]
    fn a_timeout_is_its_variant_not_its_wording() {
        let lookalike = BusError::from_zbus(zbus::Error::Failure(CALL_TIMEOUT.into()));
        assert!(
            !lookalike.is_timeout(),
            "a zbus Failure is an answer, whatever it says: {lookalike:?}"
        );

        for timeout in [BusError::call_timeout(), BusError::proxy_call_timeout()] {
            assert!(timeout.is_timeout(), "{timeout:?}");
            assert!(
                !timeout.is_transient(),
                "a deadline is not a lost connection, so no RetryPolicy retries it"
            );
        }
    }

    /// The wording the deadline had while it was a `Permanent` (#1551): every
    /// toast and log line formats a `BusError` with `Display`, so the variant
    /// must not change what they read.
    #[test]
    fn a_timeout_displays_as_it_did_before_it_had_a_variant() {
        assert_eq!(
            BusError::call_timeout().to_string(),
            "bus operation permanently failed: call timeout"
        );
        assert_eq!(
            BusError::proxy_call_timeout().to_string(),
            "bus operation permanently failed: proxy call timeout"
        );
    }
}
