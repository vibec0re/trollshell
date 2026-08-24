//! The bar's tell for a well-known D-Bus name the shell could not take (#747).
//!
//! Not a chip of its own — a shared pure helper for the two chips that back
//! onto a **session-singleton** bus name, `widgets::notif_indicator`
//! (`org.freedesktop.Notifications`) and `widgets::tray`
//! (`org.kde.StatusNotifierWatcher`).
//!
//! # Why this exists
//!
//! `hytte_bus::own_name` has always published an [`OwnState`] per owned name,
//! and until #747 every caller in the tree dropped it as `_ownership`. So the
//! one failure mode these two services actually have in the field — someone
//! else got the name first — had no user-visible expression at all. Running
//! trollshell with mako installed means notifications silently do not work:
//! the bell chip sits there looking healthy, `notifications::active()` stays
//! empty, and nothing anywhere says why. The journal is not a fallback here:
//! the deployed shell filters below `error` (#746), and a user hitting this is
//! not reading logs — they are looking at the bar.
//!
//! # Why a bar tell rather than a toast or an OSD
//!
//! * A self-notification is circular for the notifications case — the daemon
//!   that lost the race is the one being asked to draw the complaint.
//! * A toast or OSD fires **once**, at login. The symptom shows up much later,
//!   the first time some app tries to notify you. By then the transient has
//!   long expired and the user is back to a mystery.
//! * The chip is already in the bar, already bound to a signal, and is exactly
//!   the thing the user looks at when wondering where their notifications
//!   went. Binding the ownership signal to it makes the tell last precisely as
//!   long as the condition, and disappear by itself when the rival exits (the
//!   ownership task re-acquires on `NameOwnerChanged`, no restart needed).
//!
//! # Which states get a tell
//!
//! Only [`OwnState::PermanentlyTaken`] and [`OwnState::Denied`].
//! [`OwnState::Acquiring`] and [`OwnState::Lost`] are in-flight by design —
//! `own_name` re-requests 250 ms after a loss and only latches
//! `PermanentlyTaken` after several consecutive losses to the *same* holder, so
//! every ordinary reconnect blip passes through them. Warning on those would
//! flap the bar on a healthy system; the debounce is already done for us, in
//! the bus layer, and this module's job is just not to undo it.

use hytte::bus::{OwnState, UNKNOWN_HOLDER};

/// What losing one specific well-known name costs the user, in words.
///
/// One `const` per contended name lives beside the chip that renders it, so
/// the copy stays next to the thing it describes.
pub(crate) struct Subject {
    /// Lead clause, already agreeing with its own verb — e.g.
    /// `"Notifications are not being delivered"`.
    pub headline: &'static str,
    /// The well-known name the shell could not take.
    pub bus_name: &'static str,
    /// Who plausibly took it, phrased for someone who has never heard of a bus
    /// name — e.g. `"another notification daemon (mako, dunst, …)"`.
    pub rival: &'static str,
}

/// The symbolic icon a chip swaps to while its name is contended. Adwaita
/// ships it, so it needs no bundled asset and no stylesheet rule — the icon
/// swap *is* the visual tell, and the string below is the diagnosis behind it.
pub(crate) const WARN_ICON: &str = "dialog-warning-symbolic";

/// Map an ownership state to the tooltip that explains it, or `None` when
/// there is nothing to tell the user.
///
/// Pure — no GTK, no bus. This is the whole state → message mapping, and the
/// only part of #747 that is testable without a second notification daemon
/// running on the session bus.
pub(crate) fn notice(state: &OwnState, subject: &Subject) -> Option<String> {
    match state {
        // Owned: working. Acquiring / Lost: in flight — see the module docs on
        // why these two must stay silent.
        OwnState::Owned | OwnState::Acquiring | OwnState::Lost { .. } => None,
        OwnState::PermanentlyTaken { current_owner } => Some(format!(
            "{}: {} already owns {}{}.\nQuit it and trollshell takes the name back on its own.",
            subject.headline,
            subject.rival,
            subject.bus_name,
            holder_clause(current_owner),
        )),
        // Deliberately does NOT blame a rival: nobody holds the name in this
        // state, the broker's policy simply refuses it to this user. Naming
        // mako here would send someone hunting a process that does not exist.
        OwnState::Denied => Some(format!(
            "{}: the message bus refused trollshell the name {}.\nThis needs a D-Bus policy change, not a running program stopped.",
            subject.headline, subject.bus_name,
        )),
    }
}

/// ` (held by :1.42)`, or empty when the holder could not be established.
///
/// `own_name` reports [`UNKNOWN_HOLDER`] when `GetNameOwner` lost the race with
/// the holder — a routine outcome, not an error. Printing that placeholder
/// verbatim would read as a literal process name to anyone who has not read
/// `hytte-bus`, so the clause is dropped instead.
fn holder_clause(current_owner: &str) -> String {
    if current_owner == UNKNOWN_HOLDER || current_owner.is_empty() {
        String::new()
    } else {
        format!(" (held by {current_owner})")
    }
}

#[cfg(test)]
mod tests {
    use super::{Subject, notice};
    use hytte::bus::{OwnState, UNKNOWN_HOLDER};

    const SUBJECT: Subject = Subject {
        headline: "Notifications are not being delivered",
        bus_name: "org.freedesktop.Notifications",
        rival: "another notification daemon (mako, dunst, …)",
    };

    #[test]
    fn owning_the_name_says_nothing() {
        assert_eq!(notice(&OwnState::Owned, &SUBJECT), None);
    }

    #[test]
    fn acquiring_says_nothing() {
        assert_eq!(notice(&OwnState::Acquiring, &SUBJECT), None);
    }

    /// The debounce guarantee. `own_name` passes through `Lost` on every bus
    /// blip and every reconnect, transient or not, and only latches
    /// `PermanentlyTaken` after N consecutive losses to one holder. If `Lost`
    /// produced a tell, a healthy shell would flash a warning at the user
    /// every time the session bus hiccuped.
    #[test]
    fn a_loss_still_being_retried_says_nothing() {
        for transient in [true, false] {
            let state = OwnState::Lost {
                transient,
                prev_owner: Some(":1.42".to_string()),
            };
            assert_eq!(
                notice(&state, &SUBJECT),
                None,
                "a Lost that own_name is still retrying must not reach the bar (transient={transient})"
            );
        }
    }

    #[test]
    fn a_camped_name_names_the_holder_and_the_remedy() {
        let state = OwnState::PermanentlyTaken {
            current_owner: ":1.42".to_string(),
        };
        let msg = notice(&state, &SUBJECT).expect("a camped name must produce a tell");
        assert!(
            msg.contains("Notifications are not being delivered"),
            "{msg}"
        );
        assert!(msg.contains("mako, dunst"), "{msg}");
        assert!(msg.contains("org.freedesktop.Notifications"), "{msg}");
        assert!(msg.contains("(held by :1.42)"), "{msg}");
        assert!(msg.contains("takes the name back on its own"), "{msg}");
    }

    /// `GetNameOwner` routinely loses the race with a holder that releases the
    /// name right after refusing us, so `UNKNOWN_HOLDER` is a normal value —
    /// but it is `hytte-bus` jargon and must never reach a tooltip.
    #[test]
    fn an_unattributable_holder_is_omitted_rather_than_printed() {
        let state = OwnState::PermanentlyTaken {
            current_owner: UNKNOWN_HOLDER.to_string(),
        };
        let msg = notice(&state, &SUBJECT).expect("a camped name must produce a tell");
        assert!(!msg.contains(UNKNOWN_HOLDER), "{msg}");
        assert!(!msg.contains("held by"), "{msg}");
        // The rest of the diagnosis still has to survive the omission.
        assert!(msg.contains("org.freedesktop.Notifications"), "{msg}");
    }

    #[test]
    fn an_empty_holder_is_omitted_too() {
        let state = OwnState::PermanentlyTaken {
            current_owner: String::new(),
        };
        let msg = notice(&state, &SUBJECT).expect("a camped name must produce a tell");
        assert!(!msg.contains("held by"), "{msg}");
    }

    /// `Denied` has no rival process. Sending the user to kill mako when the
    /// real fix is a bus policy would be worse than saying nothing.
    #[test]
    fn denied_blames_policy_not_a_rival_daemon() {
        let msg = notice(&OwnState::Denied, &SUBJECT).expect("Denied must produce a tell");
        assert!(msg.contains("policy"), "{msg}");
        assert!(!msg.contains("mako"), "{msg}");
        assert!(!msg.contains("held by"), "{msg}");
        assert!(msg.contains("org.freedesktop.Notifications"), "{msg}");
    }

    /// The copy is per-name, not baked into the mapping.
    #[test]
    fn the_subject_supplies_every_name_specific_word() {
        const TRAY: Subject = Subject {
            headline: "The system tray is not receiving items",
            bus_name: "org.kde.StatusNotifierWatcher",
            rival: "another status-notifier host",
        };
        let state = OwnState::PermanentlyTaken {
            current_owner: ":1.7".to_string(),
        };
        let msg = notice(&state, &TRAY).expect("a camped name must produce a tell");
        assert!(
            msg.contains("The system tray is not receiving items"),
            "{msg}"
        );
        assert!(msg.contains("org.kde.StatusNotifierWatcher"), "{msg}");
        assert!(!msg.contains("Notifications"), "{msg}");
    }
}
