//! Polled S-Bahn departures from S Schöneweide, sourced from
//! v6.bvg.transport.rest.
//!
//! A 15-minute tokio loop fetches the next 8 suburban-rail departures and
//! exposes them through a [`Mutable<DeparturesState>`]. Consumers subscribe
//! via [`current()`]. The sidebar's open-edge handler nudges [`refresh()`]
//! to keep the freshly-opened list current without waiting for the next
//! poll tick.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};
use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_reactive::{registry, Service};
use tokio::sync::Notify;

// ── Configuration ───────────────────────────────────────────────────────────

/// BVG/HAFAS station ID for "S Schöneweide". Stable; verified at:
/// `https://v6.bvg.transport.rest/locations?query=schöneweide`.
pub const SCHOENEWEIDE_ID: &str = "900180001";

/// Background poll cadence. The sidebar's open-edge handler additionally
/// kicks [`refresh()`] for an immediate fetch.
pub const POLL_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// After this much time elapses since the last successful fetch, a
/// continuing error transitions `Stale` → `Err` so the user sees the
/// list has gone cold.
pub const STALE_DROP_AFTER: Duration = Duration::from_secs(30 * 60);

/// How many departures to request and display.
pub const RESULTS: usize = 8;

pub const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(10);

// ── Public types ────────────────────────────────────────────────────────────

/// One upcoming S-Bahn departure, ready for rendering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Departure {
    /// Line label, e.g. `"S9"`.
    pub line: String,
    /// Destination string, e.g. `"Spandau"`.
    pub direction: String,
    /// Scheduled local departure time.
    pub planned: DateTime<Local>,
    /// Actual local departure time (= planned + delay).
    pub actual: DateTime<Local>,
    /// Lateness in minutes. `0` when on time; negative if early.
    pub delay_minutes: i64,
    /// `true` for explicitly cancelled rows.
    pub cancelled: bool,
    /// HAFAS trip identifier, stable across refreshes for a given run.
    pub trip_id: String,
}

/// The whole service surface, observed by the widget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeparturesState {
    /// Initial value before the first fetch returns.
    Loading,
    /// Most recent fetch succeeded; `at` is when it landed.
    Ok { at: DateTime<Local>, items: Vec<Departure> },
    /// A previous fetch succeeded and a later one failed; keep showing
    /// the prior list with a "stale" hint, up to `STALE_DROP_AFTER`.
    Stale { at: DateTime<Local>, items: Vec<Departure>, err: String },
    /// No usable data on hand and the latest fetch failed.
    Err { err: String },
}

impl Default for DeparturesState {
    fn default() -> Self {
        Self::Loading
    }
}
