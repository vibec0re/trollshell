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
//! The question goes to the bus on **the connection the call arrived on**,
//! which an interface method takes with zbus's `#[zbus(connection)]`
//! parameter. So the bus that names the owner is always the bus the sender's
//! name belongs to, whichever pooled connection or generation delivered the
//! call, and a test that mounts an agent on its own bus checks it exactly as
//! production does. The lookup is answered by the bus itself
//! (`org.freedesktop.DBus`), never by the name's owner, so it starts no
//! D-Bus-activatable daemon. It reuses a connection zbus handed the method; it
//! builds none.

use crate::error::BusError;
use std::time::Duration;

/// How long the owner lookup may take before the call is refused. The bus
/// driver answers `GetNameOwner` itself, so this only bounds a wedged
/// connection. It matches [`call()`](crate::call())'s default deadline, and it fits
/// inside the time both daemons give their agents: bluetoothd waits 60 s for
/// an `Agent1` reply (`REQUEST_TIMEOUT`, `src/agent.c`) and `NetworkManager`
/// 120 s for `GetSecrets`.
const OWNER_LOOKUP_TIMEOUT: Duration = Duration::from_secs(25);

/// Who may call an exported interface: whoever owns one well-known name on
/// the bus when the call arrives. Built by [`caller_check`](crate::caller_check).
///
/// Holds only the name, so it is cheap to clone into every copy of an
/// interface struct the object server takes, and building one touches no bus.
#[derive(Clone, Debug)]
pub struct CallerCheck {
    name: String,
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

impl CallerCheck {
    /// A check that admits the current owner of `name`.
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// The well-known name whose owner this check admits.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Admit the call whose header this is if, and only if, its sender owns
    /// [`name`](Self::name) right now, asking on `conn`, the connection the
    /// call arrived on.
    ///
    /// In an interface method, take `conn` with `#[zbus(connection)]` and the
    /// header with `#[zbus(header)]`.
    ///
    /// # Errors
    ///
    /// [`CallerRefused`] when the call must not be answered.
    pub async fn check(
        &self,
        conn: &zbus::Connection,
        header: &zbus::message::Header<'_>,
    ) -> Result<(), CallerRefused> {
        let sender = header.sender().map(ToString::to_string);
        self.check_sender(conn, sender.as_deref()).await
    }

    /// [`check`](Self::check) for a sender already read out of its message.
    ///
    /// # Errors
    ///
    /// [`CallerRefused`] when `sender` is `None`, is not the owner, or the bus
    /// cannot say who the owner is.
    pub async fn check_sender(
        &self,
        conn: &zbus::Connection,
        sender: Option<&str>,
    ) -> Result<(), CallerRefused> {
        let sender = require_sender(sender)?;
        let owner = name_owner(conn, &self.name).await;
        verdict(&self.name, sender, owner)
    }
}

/// The sender, or [`CallerRefused::NoSender`] before anything is asked.
fn require_sender(sender: Option<&str>) -> Result<&str, CallerRefused> {
    sender.ok_or(CallerRefused::NoSender)
}

/// Ask the bus on `conn` who owns `name`. A reply with no owner comes back
/// as the bus's `NameHasNoOwner` error, carried in [`BusError::Permanent`]'s
/// `dbus_name`.
async fn name_owner(conn: &zbus::Connection, name: &str) -> Result<String, BusError> {
    let args = (name,);
    let asked = conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "GetNameOwner",
        &args,
    );
    let reply = tokio::time::timeout(OWNER_LOOKUP_TIMEOUT, asked)
        .await
        .map_err(|_| zbus::Error::Failure("GetNameOwner timed out".into()))
        .and_then(|reply| reply)
        .map_err(BusError::from_zbus)?;
    reply
        .body()
        .deserialize::<String>()
        .map_err(BusError::from_zbus)
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
    use super::{CallerRefused, require_sender, verdict};
    use crate::BusError;

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

    /// A call with no sender is refused before any lookup.
    #[test]
    fn a_call_with_no_sender_is_refused() {
        assert!(matches!(require_sender(None), Err(CallerRefused::NoSender)));
        assert_eq!(require_sender(Some(":1.7")).ok(), Some(":1.7"));
    }
}
