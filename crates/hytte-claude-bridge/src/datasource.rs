//! The **`claude-usage` datasource** (#1262 item 3): the bridge serving the two
//! quota percentages it already polls to any other plugin that asks.
//!
//! # Why this route and not a second poller
//!
//! caw's morning briefing reserved "#320 (Claude usage number) as one more
//! optional ingredient line once its source exists"
//! (`docs/superpowers/specs/2026-07-23-caw-morning-briefing-design.md`). The
//! source exists — [`crate::usage`] polls `GET /api/oauth/usage` every five
//! minutes — and the question #1262 put on its thread was whether caw should
//! poll the same endpoint itself. It should not, and the answer settled there
//! is this module: **the OAuth token stays read by exactly one process**, the
//! briefing gets the same numbers the chip shows, and the box grows no second
//! five-minute poll of an undocumented endpoint that spends a login.
//!
//! # The route
//!
//! The host's generic datasource protocol (#509), the same one
//! `hytte-plugin-weather` and `hytte-plugin-departures` already serve on and
//! the infobroker already queries through. There is **no registration
//! anywhere else** — not in the infobroker, not in nix: a provider is a
//! manifest that declares
//! [`Capability::DatasourceProvider`](hytte_plugin::proto::Capability::DatasourceProvider)
//! plus a [`ProvidedDatasource`](hytte_plugin::proto::ProvidedDatasource)
//! entry, and the host registers it by id for the life of the connection
//! (`trollshell/src/plugins/session.rs`). A requester names
//! [`DATASOURCE_ID`] + [`SCOPE_CURRENT`]; the host validates, forwards, and
//! routes the answer back.
//!
//! Note which half of the daemon serves it: the **plugin** hat, not the HTTP
//! one. The chip's session already holds the last [`Report`] off
//! [`crate::usage`]'s process-global board (the two-runtimes-meet-at-a-static
//! shape [`crate::status`] documents), so answering a query is a projection of
//! state already in hand — no fetch, no await, no lock held across anything.
//! A query that arrives while the bridge is running headless-but-unconnected
//! is simply never routed here: the host has no provider to forward it to and
//! synthesizes [`DatasourceError::NotFound`] itself.
//!
//! # The payload contract
//!
//! Opaque JSON at the proto layer — the schema is this provider's contract with
//! its requesters, documented *here* (the proto's own instruction), and pinned
//! byte-for-byte by `the_ready_payload_is_this_exact_frame` below. The
//! consumer's mirror is `hytte-plugin-caw`'s `usage.rs`, whose parse test
//! carries the **same literal frame**; that pair is what makes a one-sided
//! rename of [`DATASOURCE_ID`], of a field, or of the shape go red rather than
//! silent. Deliberately two mirrors and not one shared type: caw cannot link a
//! daemon that pulls in `hive-claude`, and the proto keeps datasource payloads
//! opaque on purpose (`hytte-plugin-agents`' hand-owned hyperhive mirror is the
//! same call for the same reason).
//!
//! ```json
//! {"version":1,"fetched_at":1789635540,"now":1789635600,
//!  "limits":[{"kind":"session","label":"Session (5 h)","short":"5h",
//!             "percent":37.0,"severity":"normal","active":true,
//!             "resets_at":"2026-09-17T11:00:00+00:00","resets_in":"in 2 h"}]}
//! ```
//!
//! Every row the server sent rides along, in the server's own order, with its
//! raw `kind` beside the two rendered labels — [`crate::usage`]'s "render
//! whatever the server sends" stance (Mara, #1236) carried onto the wire, so a
//! bucket Anthropic adds reaches a requester without a change here. `active` is
//! published rather than filtered on: which rows are worth a line is the
//! requester's presentation call, not the provider's.
//!
//! # Staleness is an answer, not a silence
//!
//! Three states produce **no numbers**, and each is reported as an explicit
//! [`DatasourceOutcome::Failed`] carrying [`DatasourceError::Provider`] and a
//! sentence — never as a dropped query the host would have to time out, and
//! never as a `Ready` frame of numbers nobody should trust:
//!
//! 1. nothing published yet (the board is empty — the process just started);
//! 2. a report with no numbers at all (every poll so far has failed) — the
//!    failure's own [`UsageError::sentence`](crate::usage::UsageError::sentence)
//!    is the message;
//! 3. numbers older than [`usage::STALE_AFTER`], judged by
//!    [`Report::is_stale`] exactly as the chip judges it. The chip and this
//!    datasource go stale on the same tick, off the same predicate, so the
//!    briefing can never quote a number the bar has already stopped trusting.

use hytte_plugin::proto::{DatasourceError, DatasourceOutcome};
use serde::Serialize;

use crate::usage::{self, Report};

/// The datasource id a requester names. Kebab-case after the plugin id
/// (`claude-bridge`), alongside the tree's existing `weather` / `departures`.
pub const DATASOURCE_ID: &str = "claude-usage";

/// The one scope this provider serves: the latest reading the bridge holds —
/// the same word `hytte-plugin-weather` uses for the same meaning.
pub const SCOPE_CURRENT: &str = "current";

/// The payload's schema generation. Bumped only by a **breaking** change:
/// unknown fields are ignored on the consumer side, so an added field is not
/// one. A requester that does not recognise the number it reads must render
/// nothing rather than guess (`hytte-plugin-agents`' rule for hyperhive's wire,
/// for the same reason — nobody reads a briefing line's stderr).
pub const CONTRACT_VERSION: u32 = 1;

/// The `claude-usage` payload — see the module docs for the contract.
#[derive(Debug, Serialize)]
struct Snapshot<'a> {
    /// [`CONTRACT_VERSION`].
    version: u32,
    /// Unix seconds the numbers were actually fetched at
    /// ([`Report::numbers_at`]) — *not* the moment of the last attempt, so a
    /// requester can age them the way [`Report::is_stale`] does.
    fetched_at: i64,
    /// Unix seconds this answer was composed at — the reference `resets_in` is
    /// relative to.
    now: i64,
    /// Every rate-limit row the server sent, in the server's own order.
    limits: Vec<LimitRow<'a>>,
}

/// One rate-limit row on the wire.
#[derive(Debug, Serialize)]
struct LimitRow<'a> {
    /// The server's own name for the bucket, unaltered.
    kind: &'a str,
    /// [`usage::humanise_kind`] — the long label the panel prints.
    label: String,
    /// [`usage::short_kind`] — the glance-sized one.
    short: String,
    /// Percent of the bucket consumed, `0.0..=100.0`. Clamped and
    /// finite-guarded exactly as [`usage::percent_label`] clamps for the
    /// meters, so a requester never has to defend against a `null` (which is
    /// what a non-finite `f64` would serialise as) or a nonsense magnitude.
    percent: f64,
    /// [`usage::Limit::severity`], defaulted — `normal` / `warning` /
    /// `critical`, or whatever unknown word the server chose.
    severity: &'a str,
    /// [`usage::Limit::active`], defaulted: whether the bucket is counting.
    active: bool,
    /// The raw RFC 3339 reset stamp, when the row carried one.
    resets_at: Option<&'a str>,
    /// [`usage::humanise_until`] against `now`, when the stamp parsed — so a
    /// requester with no date library still has something to print.
    resets_in: Option<String>,
}

/// Answer one `claude-usage` query from the board's last [`Report`].
///
/// Pure: `report` is what [`crate::usage::latest`] handed the chip and `now`
/// is [`crate::usage::now_unix`], both injected so the whole contract — the
/// bytes and the three staleness refusals — is testable without a clock or a
/// poll. See the module docs for which state produces which outcome.
pub fn answer(report: Option<&Report>, now: i64) -> DatasourceOutcome {
    let refuse = |message: String| DatasourceOutcome::Failed {
        error: DatasourceError::Provider,
        message,
    };
    let Some(report) = report else {
        return refuse("no usage poll has completed yet".to_owned());
    };
    let Some(usage) = report.usage() else {
        // Every poll so far has failed and there is nothing carried forward —
        // the poll's own sentence is a better message than anything invented
        // here, and it is the one the chip's tooltip is showing.
        return refuse(
            report
                .error(now)
                .unwrap_or_else(|| "no usage numbers available".to_owned()),
        );
    };
    if report.is_stale(now) {
        let age = report.numbers_at().map_or_else(
            || "some time".to_owned(),
            |at| usage::humanise_since(now, at),
        );
        return refuse(format!("usage stale — the last numbers were fetched {age}"));
    }
    let fetched_at = report.numbers_at().unwrap_or(now);
    let snapshot = Snapshot {
        version: CONTRACT_VERSION,
        fetched_at,
        now,
        limits: usage
            .limits
            .iter()
            .map(|limit| LimitRow {
                kind: limit.kind.as_str(),
                label: usage::humanise_kind(&limit.kind),
                short: usage::short_kind(&limit.kind),
                percent: if limit.percent.is_finite() {
                    limit.percent.clamp(0.0, 100.0)
                } else {
                    0.0
                },
                severity: limit.severity(),
                active: limit.active(),
                resets_at: limit.resets_at.as_deref(),
                resets_in: usage::reset_short(now, limit.resets_at.as_deref()),
            })
            .collect(),
    };
    match serde_json::to_string(&snapshot) {
        Ok(payload) => DatasourceOutcome::Ready(payload),
        // Unreachable in practice (every field is a plain scalar or a string),
        // but a provider that panicked here would take the bridge's chip down
        // with it — the same stance the rest of this crate takes about a
        // readout never being able to kill the daemon.
        Err(e) => refuse(format!("serialize claude usage: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{CONTRACT_VERSION, DATASOURCE_ID, SCOPE_CURRENT, answer};
    use crate::usage::{Limit, Outcome, Report, STALE_AFTER, Usage, UsageError};
    use hytte_plugin::proto::{DatasourceError, DatasourceOutcome};

    /// The moment every fixture here is fetched at. A fixed literal, never
    /// `now_unix()`: #1262 item 2 is the sibling issue about exactly that
    /// mistake (a fixture timestamp judged against the real clock passes only
    /// while its assertions are staleness-insensitive, and these are not).
    /// `2026-09-17T08:59:00Z`; every `answer` below is composed at
    /// `FETCHED_AT + 60`, i.e. exactly 09:00:00Z, which is what makes the
    /// recorded frame's two `resets_in` strings land on the round `in 2 h` /
    /// `in 4 d` spellings rather than on something only arithmetic can check.
    const FETCHED_AT: i64 = 1_789_635_540;

    /// Two rows shaped like the measured response: an integer `percent`, an
    /// RFC 3339 `resets_at`, `severity` present on one row and absent on the
    /// other, plus one inactive row so `active` is exercised in both states.
    fn usage() -> Usage {
        Usage {
            limits: vec![
                Limit {
                    kind: "session".to_owned(),
                    group: Some("session".to_owned()),
                    percent: 37.0,
                    severity_raw: None,
                    resets_at: Some("2026-09-17T11:00:00+00:00".to_owned()),
                    is_active: Some(true),
                },
                Limit {
                    kind: "weekly_all".to_owned(),
                    group: Some("weekly".to_owned()),
                    percent: 62.0,
                    severity_raw: Some("warning".to_owned()),
                    resets_at: Some("2026-09-21T09:00:00+00:00".to_owned()),
                    is_active: Some(true),
                },
            ],
            extra_usage: None,
        }
    }

    fn ok_report() -> Report {
        Report {
            at: FETCHED_AT,
            outcome: Outcome::Ok(usage()),
            last_ok: Some((FETCHED_AT, usage())),
        }
    }

    fn ready(outcome: &DatasourceOutcome) -> &str {
        match outcome {
            DatasourceOutcome::Ready(payload) => payload,
            DatasourceOutcome::Failed { error, message } => {
                panic!("expected Ready, got Failed({error:?}): {message}")
            }
        }
    }

    /// **The recorded frame** (#1026's rule: pin the bytes, not a round-trip
    /// through the same structs that produced them). `hytte-plugin-caw`'s
    /// `usage::tests::the_recorded_bridge_frame_parses_into_a_brief` parses
    /// this exact string — copy either one and the other is the falsification.
    const RECORDED: &str = concat!(
        r#"{"version":1,"fetched_at":1789635540,"now":1789635600,"limits":["#,
        r#"{"kind":"session","label":"Session (5 h)","short":"5h","percent":37.0,"#,
        r#""severity":"normal","active":true,"resets_at":"2026-09-17T11:00:00+00:00","#,
        r#""resets_in":"in 2 h"},"#,
        r#"{"kind":"weekly_all","label":"Weekly (all)","short":"7d","percent":62.0,"#,
        r#""severity":"warning","active":true,"resets_at":"2026-09-21T09:00:00+00:00","#,
        r#""resets_in":"in 4 d"}]}"#,
    );

    #[test]
    fn the_ready_payload_is_this_exact_frame() {
        let outcome = answer(Some(&ok_report()), FETCHED_AT + 60);
        assert_eq!(ready(&outcome), RECORDED);
    }

    #[test]
    fn the_id_and_scope_are_the_names_the_consumer_queries() {
        // Literals, not the constants restated: a rename has to show up on
        // both sides of the route, and this is this side's half.
        assert_eq!(DATASOURCE_ID, "claude-usage");
        assert_eq!(SCOPE_CURRENT, "current");
        assert_eq!(CONTRACT_VERSION, 1);
    }

    #[test]
    fn an_empty_board_refuses_with_a_sentence() {
        let DatasourceOutcome::Failed { error, message } = answer(None, FETCHED_AT) else {
            panic!("an empty board must not answer Ready");
        };
        assert_eq!(error, DatasourceError::Provider);
        assert!(message.contains("no usage poll"), "{message}");
    }

    #[test]
    fn a_report_with_no_numbers_refuses_with_the_polls_own_sentence() {
        let report = Report {
            at: FETCHED_AT,
            outcome: Outcome::Failed(UsageError::Unauthorized),
            last_ok: None,
        };
        let DatasourceOutcome::Failed { error, message } = answer(Some(&report), FETCHED_AT) else {
            panic!("a report with no numbers must not answer Ready");
        };
        assert_eq!(error, DatasourceError::Provider);
        assert_eq!(message, UsageError::Unauthorized.sentence(FETCHED_AT));
    }

    #[test]
    fn numbers_past_stale_after_refuse_instead_of_publishing() {
        let stale_by = i64::try_from(STALE_AFTER.as_secs()).unwrap() + 1;
        let now = FETCHED_AT + stale_by;
        let report = ok_report();
        assert!(report.is_stale(now), "the fixture must actually be stale");
        let DatasourceOutcome::Failed { error, message } = answer(Some(&report), now) else {
            panic!("stale numbers must never be published as Ready");
        };
        assert_eq!(error, DatasourceError::Provider);
        assert!(message.contains("usage stale"), "{message}");
        // …and one second inside the window still answers with numbers, so the
        // test above is pinning the boundary and not merely "it refuses".
        let fresh = FETCHED_AT + stale_by - 2;
        assert!(matches!(
            answer(Some(&report), fresh),
            DatasourceOutcome::Ready(_)
        ));
    }

    #[test]
    fn carried_forward_numbers_are_served_while_the_last_attempt_failed() {
        // #1283's carry-forward: a transient failure keeps the chip's meters
        // alive, so it keeps the briefing's line alive too — up to the same
        // staleness window, judged off `last_ok`'s stamp.
        let report = Report {
            at: FETCHED_AT + 300,
            outcome: Outcome::Failed(UsageError::Http(500)),
            last_ok: Some((FETCHED_AT, usage())),
        };
        let outcome = answer(Some(&report), FETCHED_AT + 300);
        let payload = ready(&outcome);
        assert!(
            payload.contains(r#""fetched_at":1789635540"#),
            "the stamp must be the numbers', not the failed attempt's: {payload}"
        );
        assert!(
            payload.contains(r#""now":1789635840"#),
            "…while `now` is still the moment the answer was composed: {payload}"
        );
    }

    #[test]
    fn a_bucket_nobody_hardcoded_still_reaches_the_wire() {
        let report = Report {
            at: FETCHED_AT,
            outcome: Outcome::Ok(Usage {
                limits: vec![Limit {
                    kind: "lunar_cycle".to_owned(),
                    percent: 5.0,
                    ..Limit::default()
                }],
                extra_usage: None,
            }),
            last_ok: None,
        };
        let outcome = answer(Some(&report), FETCHED_AT);
        let payload = ready(&outcome);
        assert!(payload.contains(r#""kind":"lunar_cycle""#), "{payload}");
        assert!(payload.contains(r#""short":"lunar cycle""#), "{payload}");
        // No severity, no `is_active`, no reset stamp: the defaults ride out.
        assert!(payload.contains(r#""severity":"normal""#), "{payload}");
        assert!(payload.contains(r#""active":true"#), "{payload}");
        assert!(payload.contains(r#""resets_at":null"#), "{payload}");
        assert!(payload.contains(r#""resets_in":null"#), "{payload}");
    }

    #[test]
    fn a_non_finite_percent_never_reaches_the_wire_as_null() {
        let report = Report {
            at: FETCHED_AT,
            outcome: Outcome::Ok(Usage {
                limits: vec![
                    Limit {
                        kind: "session".to_owned(),
                        percent: f64::NAN,
                        ..Limit::default()
                    },
                    Limit {
                        kind: "weekly_all".to_owned(),
                        percent: 140.0,
                        ..Limit::default()
                    },
                ],
                extra_usage: None,
            }),
            last_ok: None,
        };
        let payload = ready(&answer(Some(&report), FETCHED_AT)).to_owned();
        assert!(!payload.contains("null,\"severity\""), "{payload}");
        assert!(payload.contains(r#""percent":0.0"#), "{payload}");
        assert!(payload.contains(r#""percent":100.0"#), "{payload}");
    }

    #[test]
    fn an_inactive_bucket_is_published_rather_than_filtered_out() {
        let report = Report {
            at: FETCHED_AT,
            outcome: Outcome::Ok(Usage {
                limits: vec![Limit {
                    kind: "weekly_scoped".to_owned(),
                    percent: 12.0,
                    is_active: Some(false),
                    ..Limit::default()
                }],
                extra_usage: None,
            }),
            last_ok: None,
        };
        let payload = ready(&answer(Some(&report), FETCHED_AT)).to_owned();
        assert!(payload.contains(r#""active":false"#), "{payload}");
        assert!(payload.contains(r#""short":"7d scoped""#), "{payload}");
    }
}
