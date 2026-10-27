//! Primitive #7 — answer an exported interface's calls only from the current
//! owner of a well-known name.
//!
//! An agent that a system daemon calls back on, such as `NetworkManager`'s
//! secret agent or `BlueZ`'s `org.bluez.Agent1`, is exported on a connection
//! that any peer on the bus can address by its unique name. The daemon reaches
//! it that way, and so can every other peer the bus policy lets through. What
//! the agent answers matters (a Wi-Fi secret, a pairing decision), so it
//! should answer the daemon and no one else.
//!
//! [`CallerCheck`] is that guard. When a call arrives, it asks the bus who
//! owns the daemon's well-known name **at that moment** (`GetNameOwner`) and
//! compares the answer with the call's sender. Nothing is cached: a daemon
//! that restarts comes back under a new unique name, and its next call is
//! checked against that name. The check fails closed. A sender that is not
//! the owner is refused, and so is a call with no sender or a name the bus
//! cannot resolve.
//!
//! The lookup is answered by the bus itself (`org.freedesktop.DBus`), never
//! by the name's owner, so it starts no D-Bus-activatable daemon.

use crate::call::{RetryPolicy, call_with};
use crate::connection::{BusKind, SharedConnection, for_kind};
use crate::error::BusError;

/// Who may call an exported interface: whoever owns one well-known name on
/// the bus when the call arrives. Built by [`caller_check`](crate::caller_check).
///
/// Cheap to clone, so an interface struct can carry one into every clone the
/// object server takes.
#[derive(Clone)]
pub struct CallerCheck {
    bus: Bus,
    name: String,
}

/// Where a [`CallerCheck`] asks who owns its name.
///
/// The pooled connection is looked up when a call is checked, not when the
/// check is built, so building one opens no connection. That keeps an
/// interface struct constructible in a hermetic test.
#[derive(Clone)]
enum Bus {
    /// The process-wide pooled connection for this bus.
    Pooled(BusKind),
    /// A connection the caller supplied (tests).
    Given(SharedConnection),
}

/// Why a [`CallerCheck`] refused a call.
#[derive(Debug, thiserror::Error)]
pub enum CallerRefused {
    /// The message names no sender. Every message a bus routes carries one,
    /// so this only happens on a peer-to-peer connection, where there is no
    /// owner to compare with either.
    #[error("the call carries no sender")]
    NoSender,
    /// The bus could not say who owns the name: it has no owner right now
    /// (`org.freedesktop.DBus.Error.NameHasNoOwner`), or the lookup failed.
    #[error("cannot tell who owns {name}: {error}")]
    OwnerUnknown {
        /// The well-known name the check is for.
        name: String,
        /// Why the lookup failed.
        #[source]
        error: BusError,
    },
    /// The call came from someone other than the name's current owner.
    #[error("{sender} is not the owner of {name} (owned by {owner})")]
    NotOwner {
        /// The well-known name the check is for.
        name: String,
        /// The unique name the call came from.
        sender: String,
        /// The unique name that owns `name` right now.
        owner: String,
    },
}

/// Build a [`CallerCheck`] that asks over `shared` instead of a pooled
/// connection. Production callers use [`caller_check`](crate::caller_check).
#[doc(hidden)]
#[must_use]
pub fn caller_check_with(shared: &SharedConnection, name: impl Into<String>) -> CallerCheck {
    CallerCheck {
        bus: Bus::Given(shared.clone()),
        name: name.into(),
    }
}

impl CallerCheck {
    /// A check against `name` on the pooled connection for `bus`.
    pub(crate) fn pooled(bus: BusKind, name: impl Into<String>) -> Self {
        Self {
            bus: Bus::Pooled(bus),
            name: name.into(),
        }
    }

    /// The well-known name whose owner this check admits.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Admit the call whose header this is if, and only if, its sender owns
    /// [`name`](Self::name) right now.
    ///
    /// Take the header with zbus's `#[zbus(header)]` method parameter.
    ///
    /// # Errors
    ///
    /// [`CallerRefused`] when the call must not be answered.
    pub async fn check(&self, header: &zbus::message::Header<'_>) -> Result<(), CallerRefused> {
        let sender = header.sender().map(ToString::to_string);
        self.check_sender(sender.as_deref()).await
    }

    /// [`check`](Self::check) for a sender already read out of its message.
    ///
    /// # Errors
    ///
    /// [`CallerRefused`] when `sender` is `None`, is not the owner, or the bus
    /// cannot say who the owner is.
    pub async fn check_sender(&self, sender: Option<&str>) -> Result<(), CallerRefused> {
        let Some(sender) = sender else {
            return Err(CallerRefused::NoSender);
        };
        let shared = match &self.bus {
            Bus::Pooled(kind) => for_kind(*kind).clone(),
            Bus::Given(shared) => shared.clone(),
        };
        // `Never`: the call being checked arrived on this connection. If the
        // connection is dropping, its reply has nowhere to go either, so
        // waiting out a reconnect only delays a refusal.
        let owner = call_with(&shared, "org.freedesktop.DBus")
            .at_path("/org/freedesktop/DBus")
            .iface("org.freedesktop.DBus")
            .method("GetNameOwner")
            .args((self.name.clone(),))
            .retry(RetryPolicy::Never)
            .send::<String>()
            .await;
        verdict(&self.name, sender, owner)
    }
}

/// The decision itself: admit `sender` only when it is `owner`, and refuse
/// when the owner could not be found out.
fn verdict(name: &str, sender: &str, owner: Result<String, BusError>) -> Result<(), CallerRefused> {
    match owner {
        Ok(owner) if owner == sender => Ok(()),
        Ok(owner) => Err(CallerRefused::NotOwner {
            name: name.to_owned(),
            sender: sender.to_owned(),
            owner,
        }),
        Err(error) => Err(CallerRefused::OwnerUnknown {
            name: name.to_owned(),
            error,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{CallerCheck, CallerRefused, verdict};
    use crate::{BusError, BusKind};

    const NM: &str = "org.freedesktop.NetworkManager";

    #[test]
    fn the_owner_is_admitted() {
        assert!(verdict(NM, ":1.7", Ok(":1.7".to_owned())).is_ok());
    }

    #[test]
    fn a_sender_that_is_not_the_owner_is_refused() {
        match verdict(NM, ":1.9", Ok(":1.7".to_owned())) {
            Err(CallerRefused::NotOwner {
                name,
                sender,
                owner,
            }) => {
                assert_eq!(name, NM);
                assert_eq!(sender, ":1.9");
                assert_eq!(owner, ":1.7");
            }
            other => panic!("expected NotOwner, got {other:?}"),
        }
    }

    /// Fails closed: with no owner to compare against, nobody is admitted.
    #[test]
    fn an_owner_that_cannot_be_found_out_refuses_the_call() {
        let no_owner = BusError::Permanent {
            reason: "Could not get owner of name".to_owned(),
            dbus_name: Some("org.freedesktop.DBus.Error.NameHasNoOwner".to_owned()),
        };
        assert!(matches!(
            verdict(NM, ":1.7", Err(no_owner)),
            Err(CallerRefused::OwnerUnknown { .. })
        ));
    }

    /// A call with no sender is refused before any lookup, so this needs no
    /// bus: the pooled connection is never resolved.
    #[tokio::test]
    async fn a_call_with_no_sender_is_refused() {
        let check = CallerCheck::pooled(BusKind::System, NM);
        assert!(matches!(
            check.check_sender(None).await,
            Err(CallerRefused::NoSender)
        ));
    }
}
