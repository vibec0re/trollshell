//! The placeholder interface every pooled connection is built with (#1423).
//!
//! [`Ready`] exists for one reason: zbus only waits for its object-server
//! dispatch task to be running before it starts reading the socket when the
//! connection is built with at least one interface
//! (`zbus-5.14.0/src/connection/builder.rs:464-485`). `hytte-bus` has no real
//! interface to stage there — a pooled connection is shared by every service,
//! and what gets exported on it is decided later, by whoever calls
//! [`export_object`](crate::export_object) or [`own_name`](crate::own_name) — so
//! it stages this one. `connection.rs`'s `build_pooled` has the whole argument.
//!
//! It is a real object on the bus, so it has a real name:
//! [`READY_INTERFACE`] at [`READY_PATH`], on every pooled connection a process
//! opens. The pools are lazy, so that is each bus the process actually uses:
//! the shell serves it on the session and the system bus, and the control
//! center, whose every `hytte-bus` call is on the session bus, on the session
//! bus only. It has no methods, no properties and no signals. Like every object
//! zbus serves, it also answers the standard
//! `org.freedesktop.DBus.Introspectable`, `Peer` and `Properties` interfaces —
//! and nothing more. It owns no bus name, so the system bus needs no policy
//! entry for it.

/// The D-Bus interface name of the placeholder object every `hytte-bus` pooled
/// connection serves at [`READY_PATH`] (#1423).
///
/// Nothing calls it and nothing should: it has no methods, properties or
/// signals. It exists so that zbus builds each pooled connection through its
/// `serve_at` path, which waits for the object server to be dispatching before
/// the first inbound message can be read. What it tells an observer is only
/// that this process's connection was built by `hytte-bus`; for example,
/// `busctl --user introspect <unique name> /mov/vibec0re/hytte` shows it.
///
/// The name belongs to the library, not to the shell, hence
/// `mov.vibec0re.hytte.*` rather than the shell's `mov.vibec0re.trollshell.*`.
/// It must stay equal to the literal in `Ready`'s `#[zbus::interface]`
/// attribute, which cannot name a const; a unit test pins the two together.
pub const READY_INTERFACE: &str = "mov.vibec0re.hytte.Ready";

/// The object path the [`READY_INTERFACE`] placeholder is served at, on every
/// pooled connection — session, system, or both, as the process uses them
/// (#1423).
///
/// Nothing calls it. Exporting an object claims no bus name, so this path adds
/// nothing a system-bus policy has to permit.
pub const READY_PATH: &str = "/mov/vibec0re/hytte";

/// The placeholder itself: an interface with no members, staged on every
/// pooled connection's builder by `connection.rs`'s `build_pooled`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ready;

// Keep the literal equal to `READY_INTERFACE`: the attribute takes a string
// literal only. `the_interface_name_is_the_published_constant` pins it.
#[zbus::interface(name = "mov.vibec0re.hytte.Ready")]
impl Ready {}

#[cfg(test)]
mod tests {
    use super::{READY_INTERFACE, READY_PATH, Ready};
    use zbus::object_server::Interface as _;
    use zbus::zvariant::ObjectPath;

    /// The macro's name and the published const are two spellings of one
    /// thing; if they drift, the docs name an interface that nothing serves.
    #[test]
    fn the_interface_name_is_the_published_constant() {
        assert_eq!(Ready::name().as_str(), READY_INTERFACE);
        // Spelled out once as a literal too, so a change to both at once is a
        // visible change to the bus surface rather than a silent one.
        assert_eq!(READY_INTERFACE, "mov.vibec0re.hytte.Ready");
    }

    /// `serve_at` parses the path; an invalid one would make every pooled
    /// connection fail to build.
    #[test]
    fn the_path_is_a_valid_object_path() {
        assert_eq!(READY_PATH, "/mov/vibec0re/hytte");
        ObjectPath::try_from(READY_PATH).expect("READY_PATH is a valid D-Bus object path");
    }

    /// No methods, no properties, no signals: the introspection zbus serves for
    /// it is the interface element and nothing inside it.
    #[test]
    fn the_interface_has_no_members() {
        let mut xml = String::new();
        Ready.introspect_to_writer(&mut xml, 0);
        let body: Vec<&str> = xml
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(
            body,
            [
                "<interface name=\"mov.vibec0re.hytte.Ready\">",
                "</interface>"
            ],
            "the placeholder interface grew a member:\n{xml}"
        );
    }
}
