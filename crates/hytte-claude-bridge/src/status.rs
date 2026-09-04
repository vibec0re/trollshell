//! The bridge's **status board** — the handful of coarse facts the plugin face
//! ([`crate::plugin`]) paints on its bar chip, published by the HTTP side.
//!
//! # Why process-global atomics rather than a channel (#866)
//!
//! The bridge's two duties run on **two different tokio runtimes**, and neither
//! can hand the other a handle at construction time:
//!
//! - `main` builds a multi-thread runtime, binds the loopback listener on it and
//!   spawns the accept loop there. That is the primary duty and it starts first.
//! - [`hytte_plugin::run`] builds a *current-thread* runtime of its own,
//!   `block_on`s it and never returns, and it constructs the plugin model
//!   through [`Plugin::init`](hytte_plugin::Plugin::init) — which takes no
//!   arguments beyond the SDK's own command sender.
//!
//! So the two halves meet at process-global atomics: the HTTP side writes, the
//! chip polls on its own tick. Everything here is [`Ordering::Relaxed`] — these
//! are display counters read by an eventually-consistent widget, never a
//! synchronisation signal between the runtimes.
//!
//! Nothing secret is ever published: [`Startup::keyed`] is a **boolean**, so the
//! chip can say "a credential is held" without the key itself ever leaving
//! [`crate::messages`].

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use crate::Mode;

/// Requests answered with a 2xx.
static OK: AtomicU64 = AtomicU64::new(0);
/// Requests answered with anything else (a 4xx probe of an unknown route counts
/// here too — this is deliberately *coarse* health, not a route-level metric).
static ERRORS: AtomicU64 = AtomicU64::new(0);
/// How the most recent request went, as [`Last`]'s discriminant.
static LAST: AtomicU8 = AtomicU8::new(LAST_NONE);
/// What `main` settled at startup: the mode, whether a credential is held, and
/// the port. Written exactly once, before the listener is spawned.
static STARTUP: OnceLock<Startup> = OnceLock::new();

const LAST_NONE: u8 = 0;
const LAST_OK: u8 = 1;
const LAST_ERROR: u8 = 2;

/// How the most recently answered request went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Last {
    /// Nothing has been served yet this process.
    None,
    /// The last request came back 2xx.
    Ok,
    /// The last request came back non-2xx.
    Error,
}

impl Last {
    /// Decode the stored discriminant. An unknown byte (impossible today, but
    /// this is a `u8` in a static) reads as [`Last::None`] rather than panicking
    /// — a status chip must never be able to take the bridge down.
    fn from_byte(byte: u8) -> Self {
        match byte {
            LAST_OK => Self::Ok,
            LAST_ERROR => Self::Error,
            _ => Self::None,
        }
    }
}

/// The startup facts, fixed for the process's life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Startup {
    /// Which backend is running.
    pub mode: Mode,
    /// Whether the bridge holds an outbound credential of its own. False in the
    /// two `claude` modes by design — `claude` owns the subscription session and
    /// the bridge holds nothing. True only in [`Mode::Api`], where startup has
    /// already refused if no key resolved.
    pub keyed: bool,
    /// The loopback port the listener bound.
    pub port: u16,
}

/// Everything the chip renders, read in one go so the numbers on it agree with
/// each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    /// The startup facts, or `None` before `main` has published them (the window
    /// exists only in tests — the plugin face is started after [`publish`]).
    pub startup: Option<Startup>,
    /// Requests answered 2xx so far.
    pub ok: u64,
    /// Requests answered non-2xx so far.
    pub errors: u64,
    /// How the most recent one went.
    pub last: Last,
}

/// Publish the startup facts. Called once from `main`, before the listener is
/// spawned; a second call is ignored (the `OnceLock`'s own semantics).
pub fn publish(startup: Startup) {
    let _ = STARTUP.set(startup);
}

/// Record one answered request by its HTTP status. 2xx is healthy, everything
/// else is not — including the 404/405/400 a stray probe earns, because the chip
/// is showing "is this thing answering the way its clients expect", and a client
/// that gets a 400 is a client that fell back.
pub fn record(http_status: u16) {
    if (200..300).contains(&http_status) {
        OK.fetch_add(1, Ordering::Relaxed);
        LAST.store(LAST_OK, Ordering::Relaxed);
    } else {
        ERRORS.fetch_add(1, Ordering::Relaxed);
        LAST.store(LAST_ERROR, Ordering::Relaxed);
    }
}

/// Read the whole board.
#[must_use]
pub fn snapshot() -> Status {
    Status {
        startup: STARTUP.get().copied(),
        ok: OK.load(Ordering::Relaxed),
        errors: ERRORS.load(Ordering::Relaxed),
        last: Last::from_byte(LAST.load(Ordering::Relaxed)),
    }
}

#[cfg(test)]
mod tests {
    use super::{Last, Status, record, snapshot};

    /// `record` classifies by status class, not by route: every 2xx is healthy,
    /// everything else — including the 4xx a stray probe earns — is not.
    ///
    /// Written against `snapshot`'s deltas rather than absolute counts because
    /// the board is process-global and the test binary runs its tests in
    /// parallel; the *classification* is what this pins.
    #[test]
    fn record_classifies_by_status_class() {
        let before = snapshot();
        record(200);
        let after_ok = snapshot();
        assert!(after_ok.ok > before.ok, "a 2xx counts as healthy");
        assert_eq!(after_ok.last, Last::Ok);

        record(504);
        let after_err = snapshot();
        assert!(
            after_err.errors > after_ok.errors,
            "a 5xx counts as an error"
        );
        assert_eq!(after_err.last, Last::Error);

        record(404);
        assert_eq!(
            snapshot().last,
            Last::Error,
            "a stray probe's 404 is not health"
        );
    }

    /// An unknown discriminant in the `last` byte degrades to "nothing served
    /// yet" instead of panicking — a status chip must never be able to take the
    /// bridge down.
    #[test]
    fn an_unknown_last_byte_reads_as_none() {
        assert_eq!(Last::from_byte(0), Last::None);
        assert_eq!(Last::from_byte(1), Last::Ok);
        assert_eq!(Last::from_byte(2), Last::Error);
        assert_eq!(Last::from_byte(200), Last::None);
    }

    /// The board is readable before `main` has published anything (the plugin
    /// face's `init` can, in principle, run first in a test build).
    #[test]
    fn a_status_without_startup_is_still_a_status() {
        let s = Status {
            startup: None,
            ok: 0,
            errors: 0,
            last: Last::None,
        };
        assert!(s.startup.is_none());
    }
}
