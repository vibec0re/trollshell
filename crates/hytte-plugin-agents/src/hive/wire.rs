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
//! | [`Scope`]         | `LifecycleScope`, `hive-host-sock/src/lib.rs:484-501`               |
//! | [`Response`]      | `HostResponse`, `hive-host-sock/src/lib.rs:546-604`                 |
//! | [`AgentStatusRow`]| `hive_sh4re::container::AgentStatusRow`, `hive-sh4re/src/container.rs:34-96` |
//! | [`HiveUrls`]      | `HiveUrls`, `hive-host-sock/src/lib.rs:519-534`                     |
//! | [`Approval`]      | `hive_sh4re::approvals::Approval`, `hive-sh4re/src/approvals.rs:14-38` |
//! | [`ApprovalKind`]  | `ApprovalKind`, `hive-sh4re/src/approvals.rs:45-69`                 |
//! | [`ApprovalStatus`]| `ApprovalStatus`, `hive-sh4re/src/approvals.rs:90-100`              |
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
//! P1 mirrored eight of #948's ten verbs — the seven request/response ones plus
//! `SubscribeAgentStatus` (hyperhive#4064, landed), which is mirrored but
//! deliberately unused: see [`Request::SubscribeAgentStatus`]. **#947 P3 adds
//! the remaining three** — [`Request::Pending`], [`Request::Approve`] and
//! [`Request::Deny`], spec §6.5's approval prompt — which P1 left out on
//! purpose, so the row was trustworthy before it was allowed to raise a modal
//! that approves a config change.
//!
//! # Rule 1, the enum half
//!
//! The approval row is the first mirrored struct with **enum-typed** fields
//! ([`ApprovalKind`], [`ApprovalStatus`]), and rule 1's forward-drift promise
//! has to survive that. serde's default for an unknown enum value is to fail
//! the whole `Approval`, which would fail the whole `Vec<Approval>`, which
//! would fail the poll — so one new `ApprovalKind` variant hive-side would
//! blank every badge on the card, including the approvals this build
//! understands perfectly well.
//!
//! Both therefore carry a `#[serde(untagged)] Unknown(String)` catch-all, and
//! the value survives as the string the hive sent, so the prompt can still say
//! *something* truthful about it. Nothing here matches on a kind in a way that
//! changes what the plugin *does* — a kind is human wording, a status is
//! "pending or not" — so an unknown value costs a nicer sentence, never a
//! decision.

use serde::{Deserialize, Serialize};

/// The wire-schema version this build speaks, mirroring hyperhive's
/// `HOST_SOCK_VERSION` (`hive-host-sock/src/lib.rs:543`, currently `1`).
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
///   `#[serde(default)]` on the field, `hive-host-sock/src/lib.rs:547-556`)
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
    /// (`hive-host-sock/src/lib.rs:208`). **This is P1's data path.**
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
    /// (`hive-host-sock/src/lib.rs:152-164`). "A single marker write …
    /// applies immediately and works on a stopped container too", and it is
    /// idempotent both ways, so a double-click is harmless.
    SetPaused {
        /// The agent, echoed from the hive's own [`AgentStatusRow::name`].
        name: String,
        /// `true` pauses, `false` resumes.
        paused: bool,
    },
    /// Start containers, scoped (`hive-host-sock/src/lib.rs:291-294`).
    Start {
        /// Always a single agent — see [`Scope`].
        scope: Scope,
    },
    /// Stop containers, scoped (`hive-host-sock/src/lib.rs:282-287`).
    Stop {
        /// Always a single agent — see [`Scope`].
        scope: Scope,
        /// Run the per-agent quiesce rather than a hard stop.
        graceful: bool,
    },
    /// Stop and start one container without rebuilding config
    /// (`hive-host-sock/src/lib.rs:151`).
    Restart {
        /// The agent, echoed from the hive's own [`AgentStatusRow::name`].
        name: String,
    },
    /// This hive's domain plus its browser-facing URLs
    /// (`hive-host-sock/src/lib.rs:232`).
    Urls,
    /// List the approval queue (`hive-host-sock/src/lib.rs:228-229`) — the
    /// answer is [`Response::approvals`]. **#947 P3's data path.**
    ///
    /// Despite its name the answer is not filtered to pending rows: the
    /// daemon's own store hands back what it hands back, so a reader that
    /// cares about "still waiting for a human" filters on
    /// [`ApprovalStatus::Pending`] itself rather than trusting the verb's
    /// name. [`crate::model::PendingApprovals::new`] is that filter, in one
    /// place — a model policy, not a wire shape, which is why it does not live
    /// on [`Response`].
    Pending,
    /// Approve one queued request by id; **the action runs immediately**
    /// (`hive-host-sock/src/lib.rs:230-231`).
    ///
    /// Not idempotent the way [`Request::SetPaused`] is — the far side runs a
    /// config merge, a spawn or a flake update off this one frame — which is
    /// why the plugin sends exactly one per human decision and drops its
    /// correlation entry before the frame goes out, rather than after.
    Approve {
        /// The approval's id, echoed from the hive's own [`Approval::id`].
        id: i64,
    },
    /// Deny one queued request by id (`hive-host-sock/src/lib.rs:232-233`).
    ///
    /// Only ever sent for a **click**: spec §6.5's rule that an unanswered
    /// prompt leaves the approval exactly as it was is enforced one layer up,
    /// in `hytte_plugin_proto::ConsentChoices::unanswered`.
    Deny {
        /// The approval's id, echoed from the hive's own [`Approval::id`].
        id: i64,
    },
}

/// Which containers a lifecycle verb targets — **always exactly one agent**.
///
/// This is spec §11's rule one, enforced by the type rather than by a
/// convention. Hyperhive's `LifecycleScope::is_everything` treats an all-false
/// scope as *everything* (`hive-host-sock/src/lib.rs:503-511`), so a `Start`
/// with a defaulted scope starts the entire hive — agents, CI, forge, gateway
/// and matrix.
///
/// Two things make that unrepresentable here:
///
/// - The mirror carries **only** `agent_names`. There is no `agents` / `ci` /
///   `forge` / `gateway` / `matrix` field to set, so "everything" cannot be
///   spelled. The omitted keys are `#[serde(default)]` on the hive's own
///   struct (`hive-host-sock/src/lib.rs:485-500`), so they arrive as `false`.
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
/// Mirrors `HostResponse` (`hive-host-sock/src/lib.rs:546-604`); only the
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
    /// `Pending` result — the approval queue (#947 P3), **verbatim**.
    ///
    /// Not filtered here, deliberately: despite the verb's name the daemon
    /// hands back whatever its store returns, and "which statuses are still
    /// actionable" is model policy rather than wire shape. That rule lives in
    /// exactly one place, [`crate::model::PendingApprovals::new`].
    ///
    /// `None` means *this response did not answer `Pending`* — distinct from
    /// `Some(vec![])`, an empty queue, which is what clears a badge.
    #[serde(default)]
    pub approvals: Option<Vec<Approval>>,
}

/// One row in the hive's approval queue.
///
/// Mirrors `hive_sh4re::approvals::Approval`
/// (`hive-sh4re/src/approvals.rs:14-38`). Mirror rule 2 applies: `commit_ref`,
/// `fetched_sha`, `resolved_at` and `note` are **not** carried. They are the
/// payload and the audit trail of a decision the desktop does not make — the
/// prompt says who asked, for what kind of action, and the manager's own
/// free-text description, and everything past that belongs on the dashboard.
/// Carrying `commit_ref` in particular would mean rendering a field whose
/// meaning is per-kind (a sha, a PR number, an inputs array, or empty — that
/// struct's own doc), i.e. four render paths for a string nobody acts on.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Approval {
    /// The queue id — what [`Request::Approve`] / [`Request::Deny`] name, and
    /// the plugin's dedup key.
    #[serde(default)]
    pub id: i64,
    /// The agent that asked. An `Ident` hive-side, a plain string on the wire
    /// (`hive-types/src/lib.rs:143-147`), and re-validated here through
    /// [`crate::model::AgentName`] before it reaches a node id, exactly as an
    /// [`AgentStatusRow::name`] is.
    #[serde(default)]
    pub agent: String,
    /// What granting it will do.
    ///
    /// `#[serde(default)]` mirrors the hive's own attribute on this field, so
    /// a row that omits the key reads as `MergeConfigPr` on both sides.
    #[serde(default)]
    pub kind: ApprovalKind,
    /// When it was submitted — RFC 3339 UTC on the wire.
    ///
    /// Kept as the **raw string** for [`AgentStatusRow::status_set_at`]'s
    /// reason: a timestamp this build cannot parse should cost one label, not
    /// the whole queue.
    #[serde(default)]
    pub requested_at: String,
    /// Where in its lifecycle the request is.
    #[serde(default)]
    pub status: ApprovalStatus,
    /// The manager's free-text description, attached at submission time
    /// (`hive-sh4re/src/approvals.rs:34-37`). The prompt's detail line.
    #[serde(default)]
    pub description: Option<String>,
}

/// What an approval, once granted, will trigger.
///
/// Mirrors `ApprovalKind` (`hive-sh4re/src/approvals.rs:45-69`), including its
/// `snake_case` rename — plus the [`Unknown`](ApprovalKind::Unknown) arm, which
/// hyperhive's own enum does not have and this one needs (see the module docs'
/// "rule 1, the enum half").
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// Create and start a new sub-agent container.
    Spawn,
    /// Create an agent's config repo and seed it from the template.
    InitConfig,
    /// `nix flake update` the meta flake and commit the lock changes.
    UpdateMetaInputs,
    /// Add a scheduled prompt to the broker queue.
    SchedulePrompt,
    /// Merge an operator-reviewed config PR — the hive's sole config-change
    /// flow, and its `#[default]` on both sides.
    #[default]
    MergeConfigPr,
    /// A kind this build has never heard of, kept verbatim.
    #[serde(untagged)]
    Unknown(String),
}

impl ApprovalKind {
    /// The kind as a sentence fragment, for the prompt's *"⟨agent⟩ wants:
    /// ⟨this⟩"* line.
    ///
    /// The plugin computes every human string the host renders (that is what
    /// keeps `RequestConsent` domain-free), so this is where the hive's
    /// vocabulary becomes English. An [`Unknown`](ApprovalKind::Unknown) reads
    /// as the hive's own token rather than as "unknown": the operator can act
    /// on `spawn_replica`, and cannot act on a shrug.
    #[must_use]
    pub fn human(&self) -> String {
        match self {
            Self::Spawn => "create and start a new agent".to_owned(),
            Self::InitConfig => "create this agent's config repo".to_owned(),
            Self::UpdateMetaInputs => "update the hive's flake inputs".to_owned(),
            Self::SchedulePrompt => "schedule a prompt".to_owned(),
            Self::MergeConfigPr => "merge a reviewed config PR".to_owned(),
            Self::Unknown(raw) => format!("perform `{raw}`"),
        }
    }
}

/// Where an approval is in its lifecycle.
///
/// Mirrors `ApprovalStatus` (`hive-sh4re/src/approvals.rs:90-100`), plus the
/// same [`Unknown`](ApprovalStatus::Unknown) arm [`ApprovalKind`] carries.
///
/// The default is [`Pending`](ApprovalStatus::Pending) only because
/// `#[serde(default)]` on [`Approval::status`] needs one and the hive's own
/// field has no default — a row that somehow omits the key is *unresolved* by
/// construction, and defaulting the other way would silently hide it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    /// Waiting on the operator. The only status that raises a prompt.
    #[default]
    Pending,
    /// The operator approved it.
    Approved,
    /// The operator denied it.
    Denied,
    /// It failed after approval.
    Failed,
    /// The manager withdrew it before the operator acted.
    Cancelled,
    /// A status this build has never heard of, kept verbatim.
    ///
    /// Deliberately **not** treated as pending: an unknown status is a state
    /// the hive grew, and a build that cannot name it must not raise a modal
    /// offering to resolve it.
    #[serde(untagged)]
    Unknown(String),
}

/// This hive's canonical domain plus the browser-facing dashboard root.
///
/// Mirrors `HiveUrls` (`hive-host-sock/src/lib.rs:503-518`); `matrix` is a
/// swarm surface nothing in this workspace reads (spec §3), so it is not
/// mirrored.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct HiveUrls {
    /// Canonical hive domain (`services.hyperhive.domain`).
    #[serde(default)]
    pub domain: Option<String>,
    /// Operator dashboard root (`https://<domain>/`). `None` when the
    /// dashboard is not reachable from a browser.
    #[serde(default)]
    pub home: Option<String>,
    /// The forge's browser URL (`HIVE_FORGE_PUBLIC_URL`) — where an agent's
    /// **config repo** lives. `None` on a direct-port forge deploy, and on
    /// any hive whose gateway does not publish one.
    ///
    /// # Why a swarm URL is mirrored after all (#947 P4)
    ///
    /// Spec §3 keeps the *forge* out of this plugin's control loop, and that
    /// still holds: nothing dials it, nothing authenticates to it, and no
    /// verb here targets it. What #947 P4's control-center tab needs is one
    /// **link destination** — spec §10's "links to the agent page and the
    /// config repo", narrowed on the epic's P4 note to "the config repo
    /// **where `HiveUrls` names one**". That clause is only satisfiable if
    /// the mirror carries the key, and the alternative — deriving
    /// `https://forge.<domain>/` from [`HiveUrls::domain`] — is precisely the
    /// client-side guess [`crate::model::agent_url`] documents hyperhive#4073
    /// as having retired.
    ///
    /// So it is read the way every other optional URL here is: rendered when
    /// present, and the row simply absent when it is not.
    #[serde(default)]
    pub forge: Option<String>,
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
        AgentStatusRow, Approval, ApprovalKind, ApprovalStatus, HOST_SOCK_VERSION, Request,
        Response, Scope, VersionMismatch, check_version,
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
        // #947 P3's three, cited from `hive-host-sock/src/lib.rs:228-233`.
        assert_eq!(line(&Request::Pending), r#"{"cmd":"pending"}"#);
        assert_eq!(
            line(&Request::Approve { id: 42 }),
            r#"{"cmd":"approve","id":42}"#
        );
        assert_eq!(line(&Request::Deny { id: 42 }), r#"{"cmd":"deny","id":42}"#);
    }

    /// The id is `i64` on both sides, so a queue that has run past `u32` — or a
    /// hive that ever hands back a negative sentinel — still addresses the row
    /// it means. Pinned because a `u32` here would compile, pass every
    /// small-number test, and silently refuse the 4-billionth approval.
    #[test]
    fn an_approval_id_is_a_full_width_signed_integer_on_the_wire() {
        assert_eq!(
            line(&Request::Approve { id: i64::MAX }),
            r#"{"cmd":"approve","id":9223372036854775807}"#
        );
        assert_eq!(line(&Request::Deny { id: -1 }), r#"{"cmd":"deny","id":-1}"#);
    }

    /// Spec §11 rule one. `Scope` has no `Default` and no all-false
    /// constructor, so this walks every lifecycle frame the crate can build
    /// and asserts the serialized `agent_names` is present and non-empty —
    /// the property that stops a defaulted scope from meaning "the whole
    /// hive" (`hive-host-sock/src/lib.rs:503-511`).
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

    // ── #947 P3: the approval queue ──────────────────────────────────────────

    /// A `Pending` answer carrying every field the mirror reads, in the shape
    /// `HostResponse::pending` builds (`hive-host-sock/src/lib.rs:599-604`)
    /// around `hive_sh4re::approvals::Approval`.
    #[test]
    fn a_pending_answer_decodes_the_whole_row() {
        let raw = r#"{"version":1,"ok":true,"approvals":[{"id":7,"agent":"trollshell-choom","kind":"merge_config_pr","commit_ref":"12","fetched_sha":"deadbeefcafe","requested_at":"2026-09-12T09:15:00Z","status":"pending","description":"bump the meta flake"}]}"#;
        let resp: Response = serde_json::from_str(raw).expect("decodes");
        let queue = resp.approvals.clone().expect("a queue");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id, 7);
        assert_eq!(queue[0].agent, "trollshell-choom");
        assert_eq!(queue[0].kind, ApprovalKind::MergeConfigPr);
        assert_eq!(queue[0].status, ApprovalStatus::Pending);
        assert_eq!(queue[0].requested_at, "2026-09-12T09:15:00Z");
        assert_eq!(queue[0].description.as_deref(), Some("bump the meta flake"));
        // `commit_ref`/`fetched_sha` are deliberately unmirrored (rule 2) and
        // their presence must not fail the decode.
    }

    /// Rule 1, the enum half. A hive that grows an `ApprovalKind` or an
    /// `ApprovalStatus` must cost that row its *wording*, never the queue.
    ///
    /// Falsification: delete either `#[serde(untagged)] Unknown(String)` arm —
    /// the strict-enum version this mirror could have been — and `from_str`
    /// fails the whole `Vec<Approval>`, so both of the first two assertions go
    /// red at the `expect`.
    #[test]
    fn an_unknown_kind_or_status_costs_its_own_row_and_nothing_else() {
        let raw = r#"{"version":1,"ok":true,"approvals":[
            {"id":1,"agent":"a","kind":"teleport_agent","requested_at":"2026-09-12T09:00:00Z","status":"pending"},
            {"id":2,"agent":"a","kind":"merge_config_pr","requested_at":"2026-09-12T09:01:00Z","status":"awaiting_quorum"},
            {"id":3,"agent":"a","kind":"spawn","requested_at":"2026-09-12T09:02:00Z","status":"pending"}
        ]}"#;
        let resp: Response = serde_json::from_str(raw).expect("an unknown enum value is not fatal");
        let queue = resp.approvals.clone().expect("a queue");
        assert_eq!(queue.len(), 3, "no row is dropped");
        assert_eq!(
            queue[0].kind,
            ApprovalKind::Unknown("teleport_agent".to_owned())
        );
        assert_eq!(
            queue[1].status,
            ApprovalStatus::Unknown("awaiting_quorum".to_owned())
        );

        // The unknown kind still says something an operator can act on.
        assert_eq!(queue[0].kind.human(), "perform `teleport_agent`");
    }

    /// The `kind` key is `#[serde(default)]` on the hive's own struct
    /// (`hive-sh4re/src/approvals.rs:17-18`), so a row that omits it must read
    /// as `MergeConfigPr` here too — the mirror agreeing with the source of
    /// truth about a default, not inventing its own.
    #[test]
    fn an_omitted_kind_defaults_the_way_the_hives_own_struct_does() {
        let row: Approval = serde_json::from_str(
            r#"{"id":1,"agent":"a","requested_at":"2026-09-12T09:00:00Z","status":"pending"}"#,
        )
        .expect("decodes");
        assert_eq!(row.kind, ApprovalKind::MergeConfigPr);
    }

    /// A response to a verb that carries no queue leaves `approvals` absent,
    /// which must read as "not an answer to `Pending`" rather than as an empty
    /// queue that clears every badge.
    #[test]
    fn a_response_without_approvals_is_not_an_empty_queue() {
        let resp: Response = serde_json::from_str(r#"{"version":1,"ok":true}"#).expect("decodes");
        assert_eq!(resp.approvals, None);
    }

    /// Every kind renders a distinct, non-empty sentence fragment — the
    /// prompt's *"⟨agent⟩ wants: ⟨this⟩"* line, so two kinds reading alike
    /// would make two different asks indistinguishable on screen.
    #[test]
    fn every_kind_has_its_own_human_wording() {
        let kinds = [
            ApprovalKind::Spawn,
            ApprovalKind::InitConfig,
            ApprovalKind::UpdateMetaInputs,
            ApprovalKind::SchedulePrompt,
            ApprovalKind::MergeConfigPr,
        ];
        let mut seen: Vec<String> = kinds.iter().map(ApprovalKind::human).collect();
        assert!(seen.iter().all(|s| !s.is_empty()));
        seen.sort();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before, "two kinds read alike");
    }
}
