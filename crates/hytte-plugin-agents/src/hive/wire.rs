//! The typed mirror of hyperhive's `host.sock` wire (spec §5.2).
//!
//! This workspace has no git dependencies (#757) and hyperhive publishes no
//! client crate yet (#948), so the desktop speaks the protocol itself through
//! this module rather than linking `hive-host-sock`. Everything here mirrors
//! hyperhive `origin/main`:
//!
//! | in-tree type      | mirrors                                                            |
//! | ----------------- | ------------------------------------------------------------------ |
//! | [`Request`]       | `HostRequest`, `hive-host-sock/src/lib.rs:117-119`                  |
//! | [`Scope`]         | `LifecycleScope`, `hive-host-sock/src/lib.rs:468-485`               |
//! | [`Response`]      | `HostResponse`, `hive-host-sock/src/lib.rs:521-569`                 |
//! | [`AgentStatusRow`]| `hive_sh4re::container::AgentStatusRow`, `hive-sh4re/src/container.rs:34-96` |
//! | [`HiveUrls`]      | `HiveUrls`, `hive-host-sock/src/lib.rs:503-518`                     |
//!
//! # The three rules the mirror follows
//!
//! 1. **Every struct tolerates unknown keys.** No `deny_unknown_fields`
//!    anywhere, and every field carries `#[serde(default)]`, so a hive that
//!    grows a field — or one that predates a field this build knows — decodes
//!    rather than failing.
//! 2. **Nothing is mirrored that the rows do not render.** `queued_dags`,
//!    `nodes`, `quota`, `agent_exists`, the whole matrix half — omitted. A
//!    smaller mirror is a smaller thing to keep in sync.
//!    `pending_reminders` is deliberately **not** mirrored either: the hive
//!    stubs it to `0` unconditionally (`hive-c0re/src/server.rs:378-384`), so
//!    rendering it would be rendering a lie.
//! 3. **Drift refuses, it does not guess.** [`HOST_SOCK_VERSION`] is compiled
//!    in and every response is checked against it — see [`check_version`].
//!
//! P1 mirrors eight of #948's ten verbs — the seven request/response ones plus
//! `SubscribeAgentStatus` (hyperhive#4064, landed), which is mirrored but
//! deliberately unused: see [`Request::SubscribeAgentStatus`]. `Pending` /
//! `Approve` / `Deny` — the approval prompt, spec §6.5 — are phase P3 and are
//! deliberately absent: the row must be trustworthy before it is allowed to
//! raise a modal that approves a config change.

use serde::{Deserialize, Serialize};

/// The wire-schema version this build speaks, mirroring hyperhive's
/// `HOST_SOCK_VERSION` (`hive-host-sock/src/lib.rs:526`, currently `1`).
///
/// Bumped hive-side **only** for a breaking change — "removing, renaming, or
/// retyping an existing field"; adding a new optional field is not breaking
/// and needs no bump (that struct's own doc comment). Which is exactly why the
/// asymmetric rule in [`check_version`] is the right one.
pub const HOST_SOCK_VERSION: u32 = 1;

/// The hive's default host admin socket (`hive-host-sock/src/lib.rs:32`,
/// `nix/host-modules/hive-c0re/default.nix:390`). Overridable from
/// `agents.toml`.
pub const DEFAULT_SOCKET: &str = "/run/hyperhive/host.sock";

/// The daemon speaks a wire version this build cannot read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionMismatch {
    /// The `version` the daemon put on its response.
    pub theirs: u32,
    /// [`HOST_SOCK_VERSION`], what this build was compiled against.
    pub ours: u32,
}

/// The drift check (spec §5.2: "on a major mismatch, renders a single
/// 'hive protocol vN, plugin speaks vM' row instead of guessing").
///
/// Deliberately **asymmetric**, and the asymmetry is the whole point:
///
/// - `theirs <= ours` is accepted. A version bump means a field was removed,
///   renamed or retyped, so an *older* daemon can only be a subset of what
///   this build reads — every mirrored field is `#[serde(default)]`, so an
///   absent one decodes to its default and renders as "unknown", never as a
///   wrong value. `0` is the pre-version daemon (hyperhive's own
///   `#[serde(default)]` on the field, `hive-host-sock/src/lib.rs:532-540`)
///   and lands in the same bucket.
/// - `theirs > ours` is **refused**. A newer daemon may have retyped a field
///   this build reads, and a mis-read status flag is worse than no status at
///   all: it would show a wedged agent as healthy. hyperhive's own `hivectl`
///   warns and continues here (`hivectl/src/client.rs:83-93`) because a human
///   reads its stderr; nobody reads a sidebar row's stderr, so the desktop
///   refuses instead.
///
/// # Errors
/// [`VersionMismatch`] when the daemon is newer than this build.
pub fn check_version(theirs: u32) -> Result<(), VersionMismatch> {
    if theirs <= HOST_SOCK_VERSION {
        Ok(())
    } else {
        Err(VersionMismatch {
            theirs,
            ours: HOST_SOCK_VERSION,
        })
    }
}

/// A request on the host admin socket — one JSON object per line.
///
/// Mirrors `HostRequest` (`hive-host-sock/src/lib.rs:117-119`), including its
/// `#[serde(tag = "cmd", rename_all = "snake_case")]` tagging, so
/// [`Request::AgentStatus`] goes out as exactly `{"cmd":"agent_status"}`.
///
/// `Serialize` only, on purpose: the desktop never *receives* a request, and a
/// `Deserialize` impl would let a test round-trip through this type instead of
/// pinning the bytes that actually reach the hive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// List managed containers (`hive-host-sock/src/lib.rs:190`).
    List,
    /// The roster view: one [`AgentStatusRow`] per managed agent
    /// (`hive-host-sock/src/lib.rs:207`). **This is P1's data path.**
    AgentStatus,
    /// Turn the connection into a live push feed of agent-status changes —
    /// hyperhive#4064, **landed** (`hive-host-sock/src/lib.rs:209-224`). The
    /// one verb on this socket that answers with more than one response: the
    /// server acks with a bare success, then writes one further response per
    /// changed agent (`agent_statuses` always a single-row `Vec`) for as long
    /// as the client stays connected, and reads no further requests on it.
    ///
    /// **Mirrored, deliberately unused in P1.** The wire types are complete
    /// here so the mirror is not lying about what the socket can do, but the
    /// data path stays on [`Request::AgentStatus`] polling: switching it is
    /// the first follow-up after P1 merges, and it is a real change rather
    /// than a swap — a subscribed connection has to be held open, redialed,
    /// and (per the verb's own doc) *still* fall back to a fresh poll after a
    /// gap, because the feed is best-effort and "a slow reader can miss
    /// updates rather than back-pressuring the daemon". So the poll does not
    /// go away; it becomes the reconciler behind the stream.
    SubscribeAgentStatus,
    /// Park (or un-park) one agent's turn loop
    /// (`hive-host-sock/src/lib.rs:160-166`). "A single marker write …
    /// applies immediately and works on a stopped container too", and it is
    /// idempotent both ways, so a double-click is harmless.
    SetPaused {
        /// The agent, echoed from the hive's own [`AgentStatusRow::name`].
        name: String,
        /// `true` pauses, `false` resumes.
        paused: bool,
    },
    /// Start containers, scoped (`hive-host-sock/src/lib.rs:270-273`).
    Start {
        /// Always a single agent — see [`Scope`].
        scope: Scope,
    },
    /// Stop containers, scoped (`hive-host-sock/src/lib.rs:262-267`).
    Stop {
        /// Always a single agent — see [`Scope`].
        scope: Scope,
        /// Run the per-agent quiesce rather than a hard stop.
        graceful: bool,
    },
    /// Stop and start one container without rebuilding config
    /// (`hive-host-sock/src/lib.rs:144`).
    Restart {
        /// The agent, echoed from the hive's own [`AgentStatusRow::name`].
        name: String,
    },
    /// This hive's domain plus its browser-facing URLs
    /// (`hive-host-sock/src/lib.rs:220`).
    Urls,
}

/// Which containers a lifecycle verb targets — **always exactly one agent**.
///
/// This is spec §11's rule one, enforced by the type rather than by a
/// convention. Hyperhive's `LifecycleScope::is_everything` treats an all-false
/// scope as *everything* (`hive-host-sock/src/lib.rs:487-495`), so a `Start`
/// with a defaulted scope starts the entire hive — agents, CI, forge, gateway
/// and matrix.
///
/// Two things make that unrepresentable here:
///
/// - The mirror carries **only** `agent_names`. There is no `agents` / `ci` /
///   `forge` / `gateway` / `matrix` field to set, so "everything" cannot be
///   spelled. The omitted keys are `#[serde(default)]` on the hive's own
///   struct (`hive-host-sock/src/lib.rs:466-484`), so they arrive as `false`.
/// - There is **no `Default` impl** and no all-false constructor.
///   [`Scope::agent`] is the only way to build one and it always fills
///   `agent_names` with exactly one non-empty name.
///
/// Should the hive ever drop those `#[serde(default)]`s, this frame is
/// *rejected* rather than silently widened — a loud failure, which is the
/// correct direction for this particular footgun.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Scope {
    /// Specific sub-agents by logical name (`--agent <name>`, repeatable).
    /// Private, and never empty: see the type docs.
    agent_names: Vec<String>,
}

impl Scope {
    /// The only constructor: scope a lifecycle verb to one named agent.
    #[must_use]
    pub fn agent(name: &str) -> Self {
        Self {
            agent_names: vec![name.to_owned()],
        }
    }

    /// The agents this scope names. Non-empty by construction — the accessor
    /// exists so the §11 rule-one test can assert that without reaching into
    /// the private field.
    #[must_use]
    pub fn agent_names(&self) -> &[String] {
        &self.agent_names
    }
}

/// A response on the host admin socket — one JSON object per line.
///
/// Mirrors `HostResponse` (`hive-host-sock/src/lib.rs:521-569`); only the
/// fields the rows and the panel render are carried (mirror rule 2).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Response {
    /// The wire-schema version this response was built against. `0` on a
    /// pre-version daemon — see [`check_version`].
    #[serde(default)]
    pub version: u32,
    /// Whether the request succeeded.
    #[serde(default)]
    pub ok: bool,
    /// The daemon's message when `ok` is false.
    #[serde(default)]
    pub error: Option<String>,
    /// `List` result — agent names.
    #[serde(default)]
    pub agents: Option<Vec<String>>,
    /// `Urls` result.
    #[serde(default)]
    pub urls: Option<HiveUrls>,
    /// `AgentStatus` result — the roster.
    #[serde(default)]
    pub agent_statuses: Option<Vec<AgentStatusRow>>,
}

/// This hive's canonical domain plus the browser-facing dashboard root.
///
/// Mirrors `HiveUrls` (`hive-host-sock/src/lib.rs:503-518`); `forge` and
/// `matrix` are swarm surfaces the plugin never reads (spec §3), so they are
/// not mirrored.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct HiveUrls {
    /// Canonical hive domain (`services.hyperhive.domain`).
    #[serde(default)]
    pub domain: Option<String>,
    /// Operator dashboard root (`https://<domain>/`). `None` when the
    /// dashboard is not reachable from a browser.
    #[serde(default)]
    pub home: Option<String>,
}

/// One agent's row in an `AgentStatus` result.
///
/// Mirrors `hive_sh4re::container::AgentStatusRow`
/// (`hive-sh4re/src/container.rs:34-96` on hyperhive `origin/main`). The flags
/// are explicitly orthogonal, not a state machine (`:26-30` on that file) —
/// [`crate::model::Status::of`] is what collapses them to one rendered state
/// by strict precedence.
#[allow(
    clippy::struct_excessive_bools,
    reason = "a flat wire projection of independent per-agent flags — the hive's own struct carries the same allow for the same reason"
)]
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct AgentStatusRow {
    /// Logical agent name (no `h-` prefix).
    pub name: String,
    /// Whether the container is currently running.
    #[serde(default)]
    pub running: bool,
    /// The container's unit is in systemd's `failed` state.
    #[serde(default)]
    pub failed: bool,
    /// Config commit is pending — a rebuild would change the locked rev.
    #[serde(default)]
    pub needs_update: bool,
    /// No live claude session; parked waiting for the operator's re-auth.
    #[serde(default)]
    pub needs_login: bool,
    /// The turn loop is parked (the harness pause marker is present).
    #[serde(default)]
    pub paused: bool,
    /// Parent in the topology tree. `None` marks a root-level agent.
    #[serde(default)]
    pub parent: Option<String>,
    /// First 12 chars of the sha the meta flake has locked for this agent.
    #[serde(default)]
    pub deployed_sha: Option<String>,
    /// The Claude model the harness is currently using.
    #[serde(default)]
    pub active_model: Option<String>,
    /// The agent's free-text status, set via the harness's `set_status` tool.
    /// `None` when unset **or when the container is not running** — a stopped
    /// agent's on-disk status is a stale snapshot from before the stop, and
    /// the hive applies the same `read_agent_status_live` rule to every
    /// reader (`hive-sh4re/src/container.rs:76-79`). Precedence rows 1-4 in
    /// [`crate::model::Status`] already cover every case where it is absent.
    #[serde(default)]
    pub status_text: Option<String>,
    /// When `status_text` was last set — RFC 3339 UTC on the wire
    /// (`hive-sh4re/src/container.rs:82-85`), `None` exactly when
    /// `status_text` is.
    ///
    /// Kept as the **raw string** rather than a `chrono::DateTime`, and
    /// parsed at render time: a timestamp this build cannot parse should cost
    /// one age label, not the whole roster, and a strongly-typed field would
    /// fail the entire `agent_statuses` decode on one malformed value.
    #[serde(default)]
    pub status_set_at: Option<String>,
    /// The agent's own web UI behind the gateway,
    /// `https://<hive-domain>/agent/<name>/` — **hyperhive#4073, landed**
    /// (`hive-sh4re/src/container.rs:87-95`). `None` when the hive's domain
    /// is unconfigured, the same condition [`HiveUrls::home`] is `None`
    /// under.
    ///
    /// This retires spec §5.2's "one derivation": the desktop no longer
    /// builds `<home>agent/<name>/` client-side, because the hive now says
    /// it — "present here (not just derivable from `HiveUrls`) so a client
    /// never has to build this path itself", per the field's own doc. P1 only
    /// *displays* it; opening it is the P2 companion (spec §7.1).
    ///
    /// Still `#[serde(default)]`: a hive predating #4073 omits the key, and a
    /// row that decodes on both sides of that change is what mirror rule 1 is
    /// for.
    #[serde(default)]
    pub url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        AgentStatusRow, HOST_SOCK_VERSION, Request, Response, Scope, VersionMismatch, check_version,
    };

    fn line(req: &Request) -> String {
        serde_json::to_string(req).expect("a Request always serializes")
    }

    /// Every verb's exact wire bytes, pinned. `host.sock` is
    /// `#[serde(tag = "cmd", rename_all = "snake_case")]`, so a renamed
    /// variant or a re-cased tag would silently stop being understood by the
    /// daemon; nothing else in this crate would notice.
    #[test]
    fn every_verb_serializes_to_its_pinned_line() {
        assert_eq!(line(&Request::List), r#"{"cmd":"list"}"#);
        assert_eq!(line(&Request::AgentStatus), r#"{"cmd":"agent_status"}"#);
        assert_eq!(
            line(&Request::SubscribeAgentStatus),
            r#"{"cmd":"subscribe_agent_status"}"#
        );
        assert_eq!(line(&Request::Urls), r#"{"cmd":"urls"}"#);
        assert_eq!(
            line(&Request::SetPaused {
                name: "trollshell-choom".to_owned(),
                paused: true,
            }),
            r#"{"cmd":"set_paused","name":"trollshell-choom","paused":true}"#
        );
        assert_eq!(
            line(&Request::Restart {
                name: "trollshell-choom".to_owned(),
            }),
            r#"{"cmd":"restart","name":"trollshell-choom"}"#
        );
        assert_eq!(
            line(&Request::Start {
                scope: Scope::agent("trollshell-choom"),
            }),
            r#"{"cmd":"start","scope":{"agent_names":["trollshell-choom"]}}"#
        );
        assert_eq!(
            line(&Request::Stop {
                scope: Scope::agent("trollshell-choom"),
                graceful: true,
            }),
            r#"{"cmd":"stop","scope":{"agent_names":["trollshell-choom"]},"graceful":true}"#
        );
    }

    /// Spec §11 rule one. `Scope` has no `Default` and no all-false
    /// constructor, so this walks every lifecycle frame the crate can build
    /// and asserts the serialized `agent_names` is present and non-empty —
    /// the property that stops a defaulted scope from meaning "the whole
    /// hive" (`hive-host-sock/src/lib.rs:487-495`).
    ///
    /// Falsification: give `Scope` a `Default` (or an `everything()`
    /// constructor) and add it to this table, and the assertion below fails.
    #[test]
    fn every_lifecycle_frame_names_exactly_one_agent() {
        let frames = [
            Request::Start {
                scope: Scope::agent("alpha"),
            },
            Request::Stop {
                scope: Scope::agent("alpha"),
                graceful: false,
            },
            Request::Stop {
                scope: Scope::agent("alpha"),
                graceful: true,
            },
        ];
        for frame in &frames {
            let value: serde_json::Value =
                serde_json::from_str(&line(frame)).expect("our own frame parses");
            let names = value
                .get("scope")
                .and_then(|s| s.get("agent_names"))
                .and_then(serde_json::Value::as_array)
                .unwrap_or_else(|| panic!("{frame:?} has no scope.agent_names"));
            assert_eq!(names.len(), 1, "{frame:?} must scope exactly one agent");
            assert_eq!(names[0], "alpha", "{frame:?}");
        }
    }

    /// The scope frame must NOT carry the class flags at all — omitting them
    /// is what makes "everything" unrepresentable, and their presence would
    /// mean someone re-added the fields.
    #[test]
    fn the_scope_frame_carries_no_class_flags() {
        let value: serde_json::Value = serde_json::from_str(&line(&Request::Start {
            scope: Scope::agent("alpha"),
        }))
        .expect("our own frame parses");
        let scope = value
            .get("scope")
            .expect("a scope")
            .as_object()
            .expect("an object");
        assert_eq!(
            scope.keys().collect::<Vec<_>>(),
            vec!["agent_names"],
            "the mirror must carry only agent_names"
        );
    }

    #[test]
    fn scope_agent_names_is_never_empty() {
        assert_eq!(Scope::agent("alpha").agent_names(), ["alpha".to_owned()]);
    }

    /// A response carrying keys this build has never heard of still decodes —
    /// mirror rule 1, the forward-drift guarantee.
    #[test]
    fn unknown_response_keys_are_tolerated() {
        let raw = r#"{"version":1,"ok":true,"agent_statuses":[{"name":"a","running":true,"failed":false,"needs_update":false,"needs_login":false,"pending_reminders":3,"paused":false,"brand_new_field":{"nested":[1,2]}}],"queued_dags":[7],"quota":[],"another_new_top_level":"hi"}"#;
        let resp: Response = serde_json::from_str(raw).expect("unknown keys must not fail");
        assert!(resp.ok);
        let rows = resp.agent_statuses.expect("a roster");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "a");
        assert!(rows[0].running);
    }

    /// A pre-version daemon (no `version` key at all) decodes as `0`, which
    /// [`check_version`] accepts.
    #[test]
    fn a_pre_version_daemon_decodes_as_zero_and_is_accepted() {
        let resp: Response =
            serde_json::from_str(r#"{"ok":true,"agents":["a"]}"#).expect("decodes");
        assert_eq!(resp.version, 0);
        assert_eq!(check_version(resp.version), Ok(()));
    }

    /// Drift refuses rather than guessing. Falsification: relax
    /// [`check_version`] to `Ok(())` and this goes red.
    #[test]
    fn a_newer_daemon_is_refused_and_an_equal_one_is_not() {
        assert_eq!(check_version(HOST_SOCK_VERSION), Ok(()));
        assert_eq!(
            check_version(HOST_SOCK_VERSION + 1),
            Err(VersionMismatch {
                theirs: HOST_SOCK_VERSION + 1,
                ours: HOST_SOCK_VERSION,
            })
        );
    }

    /// The per-row `url` (hyperhive#4073, landed) decodes when present and
    /// defaults to `None` when absent, so the mirror reads a hive from either
    /// side of that change.
    #[test]
    fn the_per_row_url_is_optional_on_both_sides_of_hyperhive_4073() {
        let without: AgentStatusRow = serde_json::from_str(
            r#"{"name":"a","running":true,"needs_update":false,"needs_login":false}"#,
        )
        .expect("decodes");
        assert_eq!(without.url, None);
        let with: AgentStatusRow = serde_json::from_str(
            r#"{"name":"a","running":true,"needs_update":false,"needs_login":false,"url":"https://hive.local/agent/a/"}"#,
        )
        .expect("decodes");
        assert_eq!(with.url.as_deref(), Some("https://hive.local/agent/a/"));
    }
}
