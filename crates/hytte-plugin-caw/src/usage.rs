//! The briefing's **Claude usage** ingredient (#1262 item 3) — the consumer
//! half of `hytte-claude-bridge`'s `claude-usage` datasource.
//!
//! The morning-briefing spec reserved this line — "#320 (Claude usage number)
//! slots in as one more optional ingredient line once its source exists"
//! (`docs/superpowers/specs/2026-07-23-caw-morning-briefing-design.md`) — and
//! the source now exists: the bridge polls `GET /api/oauth/usage` every five
//! minutes for its bar chip. caw does **not** poll it too. The OAuth token
//! stays read by exactly one process on the box, and the briefing quotes the
//! same numbers the chip is showing, because it asks the chip.
//!
//! # How this ingredient differs from the other three
//!
//! - **weather / departures** are caw's own one-shot HTTPS fetches at briefing
//!   time ([`crate::ingredients`]).
//! - **calendar** arrives as a host *push* caw subscribes to
//!   ([`StateKey::CalendarUpcoming`](hytte_plugin::proto::StateKey::CalendarUpcoming)).
//! - **usage** is a *pull from another plugin*: the host-routed datasource
//!   protocol (#509), the first time caw uses it. Still a one-shot at briefing
//!   time, which is the spec's stated rule — the always-on poll stays in the
//!   one process that already has it.
//!
//! The round trip is three hops through `main.rs`, because a plugin's only
//! outbound verb is an `Effect` returned from `update`: the briefing loop asks
//! (`CawMsg::NeedUsage`), `update` emits
//! [`Effect::DatasourceQuery`](hytte_plugin::proto::Effect::DatasourceQuery),
//! and the answer comes back to `update` as `Input::DatasourceResult`, which
//! relays it down the existing command lane (`CawCmd::Usage`) — the same lane
//! shape the calendar and lock pushes already ride.
//!
//! **The query always resolves.** The host synthesizes a failure for a missing
//! provider (immediately), a scope the provider never declared, or a provider
//! that does not answer inside its own 10 s bound, so "the bridge is not
//! running" costs the line and never the briefing.
//!
//! # The contract
//!
//! Opaque JSON at the proto layer, so the schema is the provider's and is
//! documented on both ends. This is caw's **own tolerant mirror** of it — not a
//! shared type — for the reason `hytte-plugin-agents` owns a mirror of
//! hyperhive's wire: caw cannot link a daemon that pulls in `hive-claude`, and
//! a briefing plugin has no business growing that closure. What keeps the two
//! honest is a literal: the `RECORDED` frame below is byte-for-byte the one
//! `hytte-claude-bridge`'s `datasource::tests::the_ready_payload_is_this_exact_frame`
//! records, so a one-sided rename of the datasource, of a field, or of the
//! shape reddens here.
//!
//! Unknown fields are ignored (an added field is not a breaking change), but an
//! unknown `version` is **refused** rather than guessed at: nobody reads a
//! speech bubble's stderr, and a wrong number in caw's voice is worse than no
//! line at all.

use serde::Deserialize;

/// The datasource id caw queries — the bridge's `provides` entry.
pub(crate) const DATASOURCE_ID: &str = "claude-usage";

/// The scope caw queries — the bridge's only one.
pub(crate) const SCOPE_CURRENT: &str = "current";

/// The payload schema generation caw understands. A frame stamped higher than
/// this is dropped (see the module docs).
pub(crate) const CONTRACT_VERSION: u32 = 1;

/// The request payload. The provider ignores it (it serves one thing), but the
/// wire wants *some* JSON object and an empty one is what the infobroker sends
/// its own parameterless queries as.
pub(crate) const NO_PARAMS: &str = "{}";

/// How many rows the briefing line will print before it stops. Two is the
/// ordinary shape (the five-hour bucket and the weekly one) and also what the
/// bridge's own chip shows; a third would push the bubble's other sentences
/// out of an eight-row speech box for a number nobody asked about.
const MAX_ROWS: usize = 2;

/// The wire frame — caw's tolerant mirror. No `deny_unknown_fields`: a field
/// the bridge adds must not blank this line.
#[derive(Debug, Deserialize)]
struct Snapshot {
    version: u32,
    #[serde(default)]
    limits: Vec<LimitRow>,
}

/// One rate-limit row as the bridge publishes it. Only the three fields this
/// line prints are modelled; `kind`, `label`, `severity`, `resets_at` and
/// `resets_in` ride past unread, which is the point of not sharing a type.
#[derive(Debug, Deserialize)]
struct LimitRow {
    #[serde(default)]
    short: String,
    #[serde(default)]
    percent: f64,
    /// Whether the bucket is counting. The provider publishes every row and
    /// leaves the presentation call here; a bucket that is not counting is not
    /// news.
    #[serde(default = "yes")]
    active: bool,
}

/// `active`'s default when the key is absent — a row with no flag is still a
/// row, the same reading the bridge's own `Limit::active` takes.
fn yes() -> bool {
    true
}

/// The briefing-shaped reading: the rows worth saying, already rendered.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UsageBrief {
    /// `"5h 37%, 7d 62%"` — no leading label, so the composer and the facts
    /// block can each frame it in their own voice.
    pub rows: String,
}

/// Parse a provider payload into the ingredient, or `None` when there is
/// nothing worth a line: an unparseable frame, a version from the future, or a
/// reading whose every row is inactive.
pub(crate) fn parse(payload: &str) -> Option<UsageBrief> {
    let snapshot: Snapshot = serde_json::from_str(payload).ok()?;
    if snapshot.version != CONTRACT_VERSION {
        eprintln!(
            "[caw] claude-usage frame is version {} and caw speaks {CONTRACT_VERSION}; \
             skipping the usage line",
            snapshot.version
        );
        return None;
    }
    let rows: Vec<String> = snapshot
        .limits
        .iter()
        .filter(|row| row.active)
        .filter(|row| !row.short.trim().is_empty())
        .take(MAX_ROWS)
        .map(|row| format!("{} {}", row.short.trim(), percent(row.percent)))
        .collect();
    if rows.is_empty() {
        return None;
    }
    Some(UsageBrief {
        rows: rows.join(", "),
    })
}

/// A whole-number percent, spelled exactly as the bridge's own
/// `usage::percent_label` spells it — clamp, round, print — so the bubble and
/// the bar can never disagree about a number by a digit. The provider already
/// clamps; this is the mirror's own guard, because a mirror that trusted the
/// other end would not be one.
fn percent(raw: f64) -> String {
    if raw.is_finite() {
        format!("{}%", raw.clamp(0.0, 100.0).round())
    } else {
        "0%".to_owned()
    }
}

/// **The recorded frame** — byte-for-byte the string `hytte-claude-bridge`'s
/// `datasource::tests::RECORDED` pins as the provider's output, and the whole
/// of the two mirrors' agreement: change one side's datasource id, field name,
/// or shape and exactly one of the two crates' tests goes red.
///
/// Module-level rather than inside `mod tests` so `main.rs`'s round-trip tests
/// can feed the same bytes through `update`'s `DatasourceResult` arm — one
/// recording, every consumer-side assertion.
#[cfg(test)]
pub(crate) const RECORDED: &str = concat!(
    r#"{"version":1,"fetched_at":1789635540,"now":1789635600,"limits":["#,
    r#"{"kind":"session","label":"Session (5 h)","short":"5h","percent":37.0,"#,
    r#""severity":"normal","active":true,"resets_at":"2026-09-17T11:00:00+00:00","#,
    r#""resets_in":"in 2 h"},"#,
    r#"{"kind":"weekly_all","label":"Weekly (all)","short":"7d","percent":62.0,"#,
    r#""severity":"warning","active":true,"resets_at":"2026-09-21T09:00:00+00:00","#,
    r#""resets_in":"in 4 d"}]}"#,
);

#[cfg(test)]
mod tests {
    use super::{CONTRACT_VERSION, DATASOURCE_ID, NO_PARAMS, RECORDED, SCOPE_CURRENT, parse};

    #[test]
    fn the_recorded_bridge_frame_parses_into_a_brief() {
        let brief = parse(RECORDED).expect("the recorded frame is a usable reading");
        assert_eq!(brief.rows, "5h 37%, 7d 62%");
    }

    #[test]
    fn the_id_and_scope_are_the_names_the_provider_declares() {
        // Literals, not the constants restated: this is the consumer's half of
        // the route's two-sided rename gate (the provider's half is
        // `hytte-claude-bridge`'s
        // `the_manifest_is_a_bar_chip_that_opens_its_panel_and_serves_its_usage`).
        assert_eq!(DATASOURCE_ID, "claude-usage");
        assert_eq!(SCOPE_CURRENT, "current");
        assert_eq!(CONTRACT_VERSION, 1);
        assert_eq!(NO_PARAMS, "{}");
    }

    #[test]
    fn a_frame_from_the_future_is_refused_rather_than_guessed_at() {
        let newer = RECORDED.replacen(r#""version":1"#, r#""version":2"#, 1);
        assert_eq!(parse(&newer), None);
    }

    #[test]
    fn garbage_and_an_empty_reading_are_both_no_line() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("not json"), None);
        assert_eq!(parse(r#"{"version":1,"limits":[]}"#), None);
        // Every row inactive: the provider publishes them, caw does not say
        // them.
        assert_eq!(
            parse(r#"{"version":1,"limits":[{"short":"5h","percent":9.0,"active":false}]}"#),
            None
        );
        // A row with no name to print is not a row either.
        assert_eq!(
            parse(r#"{"version":1,"limits":[{"short":"  ","percent":9.0}]}"#),
            None
        );
    }

    #[test]
    fn a_missing_active_flag_still_counts_as_a_row() {
        let brief = parse(r#"{"version":1,"limits":[{"short":"5h","percent":4.0}]}"#)
            .expect("a row with no flag is still a row");
        assert_eq!(brief.rows, "5h 4%");
    }

    #[test]
    fn percentages_round_and_a_third_bucket_is_left_off() {
        let brief = parse(
            r#"{"version":1,"limits":[
                 {"short":"5h","percent":37.4},
                 {"short":"7d","percent":61.5},
                 {"short":"30d","percent":3.0}]}"#,
        )
        .expect("three rows parse");
        assert_eq!(brief.rows, "5h 37%, 7d 62%", "two rows, rounded");
    }

    #[test]
    fn a_nonsense_percent_never_reaches_the_bubble() {
        // The provider clamps, but the mirror does not take its word for it.
        let brief = parse(
            r#"{"version":1,"limits":[{"short":"5h","percent":-4.0},{"short":"7d","percent":9001.0}]}"#,
        )
        .expect("two rows parse");
        assert_eq!(brief.rows, "5h 0%, 7d 100%");
    }

    #[test]
    fn unknown_fields_ride_past_unread() {
        let grown = RECORDED.replacen(
            r#""limits":["#,
            r#""tomorrows_field":{"nested":[1,2]},"limits":["#,
            1,
        );
        assert_eq!(
            parse(&grown).map(|b| b.rows),
            Some("5h 37%, 7d 62%".to_owned()),
            "an added field must not blank the line"
        );
    }
}
