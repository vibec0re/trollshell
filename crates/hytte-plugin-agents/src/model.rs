//! The plugin's model: what the hive is doing, and how one row reads.
//!
//! Everything here is pure and clock-free — the reducer folds inputs into it
//! and [`crate::view`] projects it. That is what makes the status precedence
//! and the grouping testable without a socket, a host, or a wall clock.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::AgentsConfig;
use crate::hive::wire::{AgentStatusRow, Approval, ApprovalStatus, VersionMismatch};

/// A hive-reported agent name that has passed the whitelist.
///
/// Spec §11 rule two: `<name>` is echoed from the hive's own
/// `AgentStatusRow.name`, never from the config file, and is validated before
/// it reaches anything that concatenates it — the button ids the row builds
/// (`pause:<name>`), the request frames, and in P2 the `RunCommand` argv.
///
/// **The charset is hyperhive's own `Ident`, not spec §11.2's.** The spec
/// names `[A-Za-z0-9_-]+`; `Ident::parse` accepts only `[a-z0-9-]`, 1..=63
/// bytes (`hive-types/src/lib.rs:95-110`). The looser one is not merely
/// redundant, it is *wrong in a way that only shows up as a failed write*:
/// `SetPaused`/`Restart` type their `name` as `Ident`, so a name this
/// accepted but `Ident` does not — `Agent_9`, `AGENT`, `snake_case` — would
/// be rendered as a row with live buttons whose every click the daemon
/// refuses at deserialize time. Matching the source of truth means a row that
/// renders is a row that can be driven.
///
/// The same charset is what spec §11.2 makes the guard on P2's `choom` argv,
/// so tightening it now is also the cheapest time to do it. Note for that
/// phase: `[a-z0-9-]` still admits a **leading hyphen** (`--rm` is a legal
/// `Ident`), which is an argv concern shared with upstream rather than
/// something this type can fix — an argv builder must pass `--` or an
/// explicit `--agent=<name>`, never bare interpolation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentName(String);

impl AgentName {
    /// The hive's own cap (`Ident::MAX_LEN`, `hive-types/src/lib.rs:87`).
    pub const MAX_LEN: usize = 63;

    /// Validate a name off the wire, by hyperhive's own `Ident` rule. `None`
    /// rejects it — the caller drops the row and logs, rather than rendering
    /// something the daemon would refuse to be addressed by.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        if s.is_empty() || s.len() > Self::MAX_LEN {
            return None;
        }
        // `Ident::parse`'s exact predicate, `hive-types/src/lib.rs:105-107`.
        if !s
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return None;
        }
        Some(Self(s.to_owned()))
    }

    /// The validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The one state a row renders, collapsed from the hive's orthogonal flags by
/// strict precedence (spec §6.2).
///
/// `AgentStatusRow`'s flags are explicitly *not* a state machine
/// (`hive-sh4re/src/container.rs:26-30`): any combination is meaningful, a
/// stopped agent can be paused and need an update. So the row picks one
/// primary state here and renders `needs_update` as a separate badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// 1. The container's unit exhausted its bounded restarts.
    Failed,
    /// 2. No live claude session; parked on the operator's re-auth flow.
    NeedsLogin,
    /// 3. The turn loop is parked by the harness pause marker.
    Paused,
    /// 4. Not running, and none of the above.
    Stopped,
    /// 5. Running — the harness's own text is the second line.
    Running,
}

impl Status {
    /// Every state, in precedence order — the table spec §6.2 pins.
    pub const ALL: [Status; 5] = [
        Status::Failed,
        Status::NeedsLogin,
        Status::Paused,
        Status::Stopped,
        Status::Running,
    ];

    /// Collapse one row's flags. **Strict precedence, first match wins.**
    #[must_use]
    pub fn of(row: &AgentStatusRow) -> Self {
        if row.failed {
            Self::Failed
        } else if row.needs_login {
            Self::NeedsLogin
        } else if row.paused {
            Self::Paused
        } else if row.running {
            Self::Running
        } else {
            Self::Stopped
        }
    }

    /// The symbolic icon for the trailing state badge.
    #[must_use]
    pub fn icon(self) -> &'static str {
        match self {
            Self::Failed => "dialog-error-symbolic",
            Self::NeedsLogin => "dialog-password-symbolic",
            Self::Paused => "media-playback-pause-symbolic",
            Self::Stopped => "media-playback-stop-symbolic",
            Self::Running => "media-playback-start-symbolic",
        }
    }

    /// The state's own word. For [`Status::Running`] this is the fallback the
    /// row shows when a running agent has set no `status_text` — see
    /// [`Agent::status_line`].
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::NeedsLogin => "needs login",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::Running => "running",
        }
    }

    /// The style class carried on the state icon.
    #[must_use]
    pub fn class(self) -> &'static str {
        match self {
            Self::Failed => "error",
            Self::NeedsLogin => "warning",
            Self::Paused | Self::Stopped => "dim-label",
            Self::Running => "accent",
        }
    }
}

/// One agent, as the rows hold it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    /// The validated name — the addressing key for every button and request.
    pub name: AgentName,
    /// The hive's own row, verbatim.
    pub row: AgentStatusRow,
    /// A pause the operator asked for that the hive has not confirmed yet
    /// (spec §6.3: "optimistic flip, reconciled by the next poll"). Cleared
    /// the moment the poll agrees with it.
    pub pending_paused: Option<bool>,
}

impl Agent {
    /// The row's effective paused state: the optimistic flip while one is in
    /// flight, otherwise the hive's.
    #[must_use]
    pub fn paused(&self) -> bool {
        self.pending_paused.unwrap_or(self.row.paused)
    }

    /// The state the row renders, honouring the optimistic pause flip.
    #[must_use]
    pub fn status(&self) -> Status {
        let mut row = self.row.clone();
        row.paused = self.paused();
        Status::of(&row)
    }

    /// The row's second line: the harness's own text for a running agent,
    /// otherwise the state's word.
    ///
    /// `status_text` is `None` when unset **or when the container is not
    /// running** (`hive-sh4re/src/container.rs:76-79`), so precedence rows 1-4
    /// already cover every case where it is absent and row 5 falls back only
    /// when a running agent has set no status.
    #[must_use]
    pub fn status_line(&self) -> &str {
        let status = self.status();
        if status == Status::Running {
            self.row
                .status_text
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| status.text())
        } else {
            status.text()
        }
    }

    /// Whether the update badge shows.
    #[must_use]
    pub fn needs_update(&self) -> bool {
        self.row.needs_update
    }
}

/// The hive's approval queue, filtered to what is still waiting on a human and
/// ordered oldest first (#947 P3, spec §6.5).
///
/// A type rather than a bare `Vec<Approval>` because four call sites ask it
/// four different questions — "how many badges does this row wear", "which one
/// does a badge click raise", "which one has never been prompted", "is this
/// decision still answerable" — and each of those answers has to agree about
/// *ordering* and about *what counts as waiting*, or the badge and the prompt
/// would disagree about which approval is the oldest.
///
/// So both rules are established once, in [`PendingApprovals::new`], and every
/// method below reads them as an invariant rather than re-deriving them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PendingApprovals {
    /// Status-[`Pending`](ApprovalStatus::Pending) rows, ascending by id.
    queue: Vec<Approval>,
}

impl PendingApprovals {
    /// Take a raw `Pending` answer and keep what is still waiting on a human,
    /// oldest first.
    ///
    /// Two decisions, both here rather than at the call sites:
    ///
    /// - **Filter on the status, not on the verb.** `Pending`'s answer is
    ///   whatever the daemon's store returns, so an `Approved` row arriving in
    ///   it must not raise a prompt for a decision somebody already made. An
    ///   [`Unknown`](ApprovalStatus::Unknown) status is *not* waiting either: a
    ///   build that cannot name a state must not offer to resolve it.
    /// - **Order by id.** The hive's ids are monotonic per queue insert, so
    ///   ascending id *is* oldest-first — and it is a total order over `i64`,
    ///   where `requested_at` is a free-text timestamp the mirror deliberately
    ///   keeps unparsed (the [`AgentStatusRow::status_set_at`] rule).
    #[must_use]
    pub fn new(queue: Vec<Approval>) -> Self {
        let mut queue: Vec<Approval> = queue
            .into_iter()
            .filter(|a| a.status == ApprovalStatus::Pending)
            .collect();
        queue.sort_by_key(|a| a.id);
        Self { queue }
    }

    /// The whole queue, oldest first.
    #[must_use]
    pub fn all(&self) -> &[Approval] {
        &self.queue
    }

    /// Whether `id` is still waiting on a human.
    ///
    /// The question every decision path asks before acting: an approval that
    /// left the queue between the prompt and the answer (the operator used the
    /// dashboard, or `hivectl`) must be dropped, not re-decided — spec §6.5's
    /// "dropped with a debug line, not an error".
    #[must_use]
    pub fn contains(&self, id: i64) -> bool {
        self.queue.iter().any(|a| a.id == id)
    }

    /// How many approvals `agent` is waiting on — the badge's count.
    #[must_use]
    pub fn count_for(&self, agent: &str) -> usize {
        self.queue.iter().filter(|a| a.agent == agent).count()
    }

    /// The oldest approval `agent` is waiting on — what its badge click raises.
    #[must_use]
    pub fn oldest_for(&self, agent: &str) -> Option<&Approval> {
        self.queue.iter().find(|a| a.agent == agent)
    }

    /// The oldest approval no prompt has been raised for yet.
    ///
    /// `prompted` is the dedup set: an approval prompts **once** and then waits
    /// for a decision or a badge click, so a hive with one unanswered approval
    /// does not raise a modal every poll.
    #[must_use]
    pub fn oldest_unprompted<'a>(&'a self, prompted: &BTreeSet<i64>) -> Option<&'a Approval> {
        self.queue.iter().find(|a| !prompted.contains(&a.id))
    }
}

/// The badge icon for `needs_update` (spec §6.2's badge row).
pub const UPDATE_BADGE_ICON: &str = "software-update-available-symbolic";

/// The badge icon for a pending approval (#947 P3).
///
/// A **question**, not a warning: an approval is the hive asking, and the row's
/// warning vocabulary (`failed`, `needs_login`) is already spoken for by
/// [`Status`], which this badge must not be mistaken for.
pub const APPROVAL_BADGE_ICON: &str = "dialog-question-symbolic";

/// The approval badge's style class.
pub const APPROVAL_BADGE_CLASS: &str = "ts-agent-approvals";

/// The badge's style class.
pub const UPDATE_BADGE_CLASS: &str = "ts-agent-upd";

/// What the plugin knows about the hive right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hive {
    /// No poll has answered yet — the seed state, rendered as one quiet row so
    /// the card mounts immediately rather than flashing an error.
    Connecting,
    /// The socket could not be reached. One row, the reason ellipsized, and
    /// the cadence unchanged: absent path, `ECONNREFUSED` and `EACCES` all
    /// land here (spec §5.3, "no socket, no crash").
    Unreachable {
        /// The row's second line.
        reason: String,
    },
    /// The hive is **up and talking**, but its answer was unusable: an
    /// `ok: false` carrying the daemon's own error, or a line this build
    /// could not parse.
    ///
    /// Kept apart from [`Hive::Unreachable`] because the two send the
    /// operator to different places — "hive-c0re is not running" is a
    /// `systemctl` problem, `agent "ghost" is not managed by this hive` is
    /// not — and a status surface that mislabels which one it is has failed
    /// at the one job it has.
    Error {
        /// The daemon's own message, or the parse failure.
        reason: String,
    },
    /// The daemon speaks a wire version this build refuses to guess at
    /// (spec §5.2).
    Incompatible(VersionMismatch),
    /// The roster, as of the last successful poll.
    Up {
        /// Every agent the hive reported, in the hive's own order.
        agents: Vec<Agent>,
    },
}

impl Hive {
    /// The roster, or the empty slice for every non-`Up` state.
    #[must_use]
    pub fn agents(&self) -> &[Agent] {
        match self {
            Self::Up { agents } => agents,
            _ => &[],
        }
    }

    /// One agent by name.
    #[must_use]
    pub fn agent(&self, name: &AgentName) -> Option<&Agent> {
        self.agents().iter().find(|a| &a.name == name)
    }

    /// One agent by name, mutably.
    pub fn agent_mut(&mut self, name: &AgentName) -> Option<&mut Agent> {
        match self {
            Self::Up { agents } => agents.iter_mut().find(|a| &a.name == name),
            _ => None,
        }
    }
}

/// Which project groups the operator has explicitly expanded or collapsed,
/// keyed by [`Group::header`].
///
/// Absent means "use the default" — open unless every agent in the group is
/// stopped (see `view::group_open`). Only an actual click writes an entry, so
/// a hive that gains a busy agent in a group nobody has touched still opens
/// it, while a group somebody deliberately collapsed stays collapsed.
pub type ExpandedGroups = BTreeMap<String, bool>;

/// One group header plus the rows under it (spec §6.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group<'a> {
    /// The multi-repo project, or `None` for the ungrouped bucket.
    pub project: Option<&'a str>,
    /// The rows, in the hive's own order within the group.
    pub agents: Vec<&'a Agent>,
}

impl Group<'_> {
    /// The header text a rendered group shows.
    pub const UNGROUPED: &'static str = "ungrouped";

    /// This group's header text.
    #[must_use]
    pub fn header(&self) -> &str {
        self.project.unwrap_or(Self::UNGROUPED)
    }
}

/// Bucket the roster by `[display.<name>].project` (spec §6.3).
///
/// Named projects sort alphabetically; the ungrouped bucket always sorts
/// **last**, so a newly-added agent nobody has labelled yet appears at the
/// bottom rather than displacing the groups above it. An agent whose project
/// is unknown lands in that bucket rather than being hidden — losing a row
/// because nobody wrote a config section would be the worst possible failure
/// mode for a status surface.
#[must_use]
pub fn group<'a>(agents: &'a [Agent], cfg: &'a AgentsConfig) -> Vec<Group<'a>> {
    let mut named: BTreeMap<&'a str, Vec<&'a Agent>> = BTreeMap::new();
    let mut ungrouped: Vec<&'a Agent> = Vec::new();
    for agent in agents {
        match cfg.project_for(agent.name.as_str()) {
            Some(project) => named.entry(project).or_default().push(agent),
            None => ungrouped.push(agent),
        }
    }
    let mut groups: Vec<Group<'a>> = named
        .into_iter()
        .map(|(project, agents)| Group {
            project: Some(project),
            agents,
        })
        .collect();
    if !ungrouped.is_empty() {
        groups.push(Group {
            project: None,
            agents: ungrouped,
        });
    }
    groups
}

/// Whether the rendered list should draw group headers at all.
///
/// Spec §6.3: "With one project the header is suppressed and the list looks
/// exactly as it does today." One group — named or ungrouped — is one project.
#[must_use]
pub fn headers_wanted(groups: &[Group<'_>]) -> bool {
    groups.len() > 1
}

/// The agent's own page URL, **as the hive states it** — never derived.
///
/// Spec §5.2 had the desktop building `<home>agent/<name>/` client-side,
/// because `HiveUrls` carried no per-agent URL and the path scheme was a
/// guess the desktop did not own. hyperhive#4073 landed that field on the row
/// (`hive-sh4re/src/container.rs:87-95`) for exactly that reason — "so a
/// client never has to build this path itself" — so the derivation is gone
/// rather than kept as a fallback: a fallback would re-introduce the guess on
/// precisely the hives too old to have the field, which is the case it was
/// wrong for.
///
/// `None` when the hive's domain is unconfigured, which is exactly when a
/// link would be dead.
#[must_use]
pub fn agent_url(agent: &Agent) -> Option<&str> {
    agent
        .row
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
}

/// The model families this plugin knows how to shorten, each with the exact
/// spelling the chip renders.
///
/// A table rather than "title-case the token", because the canonical spelling
/// is the product's, not a transformation: `Opus`, not `OPUS` or `opus`.
const MODEL_FAMILIES: [(&str, &str); 3] =
    [("opus", "Opus"), ("sonnet", "Sonnet"), ("haiku", "Haiku")];

/// The model **family word** for the row's identity chip — never the dated id.
///
/// Annika on [#963](https://github.com/vibec0re/trollshell/pull/963),
/// 2026-09-11: *"keep Model name short — Opus is enough. Not
/// `opus-5.2-20262981923899321898`"*. The hive reports `active_model` as
/// whatever the harness is configured with, which is a full model id whose
/// tail is a build date, a context marker, or both. In 320 px of sidebar that
/// tail is noise: it is identical for every agent on the same release, and the
/// one word that actually differs between two rows is the family.
///
/// # The rule
///
/// Split on every non-alphanumeric character, then:
///
/// - if **any** token is a known family (case-insensitively), render that
///   family's canonical spelling. Scanning every token rather than only the
///   first is what makes `claude-opus-4-6` — the provider-prefixed spelling
///   the hive actually sends, see `tests/fixtures/agent_status_grouped.json` —
///   and a bare `opus-5.2-…` both read `Opus`.
/// - otherwise the **first** token, with its first character upper-cased and
///   the rest left exactly as written: `gpt-4o` → `Gpt`, `GLM-4.6` → `GLM`.
///   Deliberately not a rename table with a fallback of "unknown": a model
///   this plugin has never heard of should still show something the operator
///   recognises, and leaving the rest of the token alone is what keeps an
///   all-caps family name from being mangled into `Glm`.
///
/// `None` for an absent, empty or punctuation-only value — the caller then
/// draws no chip at all rather than an empty one.
///
/// # The word boundary is Unicode, not ASCII (#963 review, LOW-3)
///
/// The split is `!char::is_alphanumeric`, so a model id written in any script
/// still yields a first token and therefore a chip. It was
/// `!char::is_ascii_alphanumeric` until the review pointed out the failure mode
/// that hides behind: an id with no ASCII alphanumerics at all tokenises to
/// nothing, returns `None`, and the row renders **no chip** — not a fallback
/// word, not a placeholder, nothing — which reads as "this agent has no model"
/// rather than as "this build could not shorten the name". Today's roster is
/// all ASCII (`claude-*`, `gpt-*`, `GLM-*`) so nobody would have hit it, and
/// that is exactly why it was worth closing before somebody did.
///
/// The **family table** stays ASCII-cased (`eq_ignore_ascii_case`): its three
/// entries are ASCII product names, and Unicode case folding on a lookup that
/// can only ever match ASCII would be ceremony. A non-ASCII token simply does
/// not match a family and falls through to the first-token branch, where
/// `char::to_uppercase` *is* Unicode-correct.
///
/// Nothing is lost by shortening: the caller puts the **full** id on the
/// chip's hover, which is this file's standing idiom for a clipped string.
#[must_use]
pub fn model_family(raw: &str) -> Option<String> {
    let tokens: Vec<&str> = raw
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let first = *tokens.first()?;
    for token in &tokens {
        if let Some((_, canonical)) = MODEL_FAMILIES
            .iter()
            .find(|(needle, _)| token.eq_ignore_ascii_case(needle))
        {
            return Some((*canonical).to_owned());
        }
    }
    let mut chars = first.chars();
    let head = chars.next()?;
    Some(head.to_uppercase().chain(chars).collect())
}

#[cfg(test)]
mod tests {
    use super::{
        Agent, AgentName, Group, Hive, PendingApprovals, Status, agent_url, group, headers_wanted,
        model_family,
    };
    use crate::config::AgentsConfig;
    use crate::hive::wire::{AgentStatusRow, Approval, ApprovalKind, ApprovalStatus};
    use std::collections::BTreeSet;

    // ── #947 P3: the approval queue's two invariants ─────────────────────────

    fn queued(id: i64, agent: &str, status: ApprovalStatus) -> Approval {
        Approval {
            id,
            agent: agent.to_owned(),
            kind: ApprovalKind::MergeConfigPr,
            requested_at: String::new(),
            status,
            description: None,
        }
    }

    /// [`PendingApprovals::new`] is the one place "still waiting, oldest
    /// first" is decided, so both halves are pinned here.
    ///
    /// Falsification: drop the `sort_by_key` and the order assertion goes red;
    /// drop the status filter and the resolved rows appear — which is the bug
    /// that would raise a modal offering to approve something already approved.
    #[test]
    fn the_queue_keeps_only_what_is_waiting_and_orders_it_oldest_first() {
        let q = PendingApprovals::new(vec![
            queued(9, "argus", ApprovalStatus::Pending),
            queued(4, "argus", ApprovalStatus::Approved),
            queued(6, "argus", ApprovalStatus::Pending),
            queued(2, "argus", ApprovalStatus::Denied),
            queued(1, "argus", ApprovalStatus::Cancelled),
            queued(3, "argus", ApprovalStatus::Failed),
        ]);
        assert_eq!(
            q.all().iter().map(|a| a.id).collect::<Vec<_>>(),
            vec![6, 9],
            "only `pending`, ascending by id"
        );
    }

    /// An [`ApprovalStatus::Unknown`] is **not** waiting: a build that cannot
    /// name a state must not offer the operator a card that resolves it.
    #[test]
    fn an_unknown_status_is_not_treated_as_waiting() {
        let q = PendingApprovals::new(vec![
            queued(
                1,
                "argus",
                ApprovalStatus::Unknown("awaiting_quorum".to_owned()),
            ),
            queued(2, "argus", ApprovalStatus::Pending),
        ]);
        assert_eq!(q.all().iter().map(|a| a.id).collect::<Vec<_>>(), vec![2]);
    }

    /// The three questions the badge and the prompt ask, and they must agree:
    /// the count is per agent, the badge click's target is that agent's oldest,
    /// and the prompt's target is the oldest nobody has been asked about.
    #[test]
    fn the_queue_answers_per_agent_and_agrees_about_oldest() {
        let q = PendingApprovals::new(vec![
            queued(5, "argus", ApprovalStatus::Pending),
            queued(7, "bosun", ApprovalStatus::Pending),
            queued(9, "argus", ApprovalStatus::Pending),
        ]);
        assert_eq!(q.count_for("argus"), 2);
        assert_eq!(q.count_for("bosun"), 1);
        assert_eq!(q.count_for("nobody"), 0);
        assert_eq!(q.oldest_for("argus").map(|a| a.id), Some(5));
        assert_eq!(q.oldest_for("nobody"), None);
        assert!(q.contains(7) && !q.contains(8));

        let mut prompted = BTreeSet::new();
        assert_eq!(q.oldest_unprompted(&prompted).map(|a| a.id), Some(5));
        prompted.insert(5);
        assert_eq!(q.oldest_unprompted(&prompted).map(|a| a.id), Some(7));
        prompted.insert(7);
        prompted.insert(9);
        assert_eq!(q.oldest_unprompted(&prompted), None);
    }

    fn agent(name: &str, row: AgentStatusRow) -> Agent {
        Agent {
            name: AgentName::parse(name).expect("test names are legal"),
            row,
            pending_paused: None,
        }
    }

    #[allow(
        clippy::fn_params_excessive_bools,
        reason = "a fixture constructor for a four-bool wire row; naming each flag at the call site is what makes the precedence table readable"
    )]
    fn flags(failed: bool, needs_login: bool, paused: bool, running: bool) -> AgentStatusRow {
        AgentStatusRow {
            name: "a".to_owned(),
            running,
            failed,
            needs_login,
            paused,
            ..AgentStatusRow::default()
        }
    }

    /// Spec §6.2's table, over **all 32 flag combinations** of the five
    /// booleans the row carries. `needs_update` is deliberately in the sweep
    /// even though it never changes the primary state: that independence is
    /// exactly what a badge means, and a regression that let it win would be
    /// caught here rather than in a screenshot.
    ///
    /// Falsification: swap any two arms of `Status::of` and this goes red on
    /// the combination that distinguishes them.
    #[test]
    fn precedence_holds_over_all_thirty_two_flag_combinations() {
        let mut seen = 0_u32;
        for bits in 0..32_u32 {
            let (failed, needs_login, paused, running, needs_update) = (
                bits & 1 != 0,
                bits & 2 != 0,
                bits & 4 != 0,
                bits & 8 != 0,
                bits & 16 != 0,
            );
            let mut row = flags(failed, needs_login, paused, running);
            row.needs_update = needs_update;

            let expected = if failed {
                Status::Failed
            } else if needs_login {
                Status::NeedsLogin
            } else if paused {
                Status::Paused
            } else if running {
                Status::Running
            } else {
                Status::Stopped
            };
            assert_eq!(Status::of(&row), expected, "bits={bits:#07b}");
            assert_eq!(
                Status::of(&row).icon(),
                expected.icon(),
                "icon disagrees at bits={bits:#07b}"
            );
            // The badge is orthogonal to the primary state, always.
            assert_eq!(row.needs_update, needs_update);
            seen += 1;
        }
        assert_eq!(seen, 32, "the sweep must cover every combination");
    }

    /// The five precedence rows, spelled out one by one against the spec's
    /// table — icon, text and class together, so a partial edit cannot pass.
    #[test]
    fn each_precedence_row_matches_the_spec_table() {
        let table = [
            (
                Status::Failed,
                "dialog-error-symbolic",
                "failed",
                "error",
                flags(true, true, true, true),
            ),
            (
                Status::NeedsLogin,
                "dialog-password-symbolic",
                "needs login",
                "warning",
                flags(false, true, true, true),
            ),
            (
                Status::Paused,
                "media-playback-pause-symbolic",
                "paused",
                "dim-label",
                flags(false, false, true, true),
            ),
            (
                Status::Stopped,
                "media-playback-stop-symbolic",
                "stopped",
                "dim-label",
                flags(false, false, false, false),
            ),
            (
                Status::Running,
                "media-playback-start-symbolic",
                "running",
                "accent",
                flags(false, false, false, true),
            ),
        ];
        for (want, icon, text, class, row) in &table {
            assert_eq!(Status::of(row), *want, "{row:?}");
            assert_eq!(want.icon(), *icon);
            assert_eq!(want.text(), *text);
            assert_eq!(want.class(), *class);
        }
        assert_eq!(Status::ALL.len(), table.len(), "ALL must cover the table");
    }

    /// Precedence 3 beats 4: a paused-and-stopped agent reads "paused", not
    /// "stopped" — pause survives a stop hive-side
    /// (`hive-sh4re/src/container.rs:59-63`), so the row must say so.
    #[test]
    fn a_paused_stopped_agent_reads_paused() {
        assert_eq!(
            Status::of(&flags(false, false, true, false)),
            Status::Paused
        );
    }

    /// Row 5's second line is the harness's own text, verbatim.
    #[test]
    fn a_running_agent_shows_the_harness_text_and_falls_back_when_blank() {
        let mut row = flags(false, false, false, true);
        row.status_text = Some("reviewing PR #947".to_owned());
        assert_eq!(agent("a", row.clone()).status_line(), "reviewing PR #947");

        row.status_text = Some("   ".to_owned());
        assert_eq!(agent("a", row.clone()).status_line(), "running");

        row.status_text = None;
        assert_eq!(agent("a", row).status_line(), "running");
    }

    /// A stopped agent's `status_text` is a stale snapshot the hive itself
    /// withholds; even if one arrived, the row must show the state's word.
    #[test]
    fn a_non_running_agent_never_shows_a_stale_harness_text() {
        let mut row = flags(false, false, false, false);
        row.status_text = Some("was doing something".to_owned());
        assert_eq!(agent("a", row).status_line(), "stopped");
    }

    /// The optimistic pause flip drives the rendered state immediately, and
    /// the hive's own flag takes back over once the flip is cleared.
    #[test]
    fn an_optimistic_pause_flip_drives_the_rendered_state() {
        let mut a = agent("a", flags(false, false, false, true));
        assert_eq!(a.status(), Status::Running);
        a.pending_paused = Some(true);
        assert_eq!(a.status(), Status::Paused);
        assert!(a.paused());
        a.pending_paused = None;
        assert_eq!(a.status(), Status::Running);
    }

    /// The whitelist is hyperhive's `Ident`, byte for byte — including the
    /// three shapes spec §11.2's looser `[A-Za-z0-9_-]` would have admitted
    /// and the daemon then refuses at deserialize time.
    ///
    /// Falsification: widen the predicate back to `is_ascii_alphanumeric() ||
    /// b'-' || b'_'` and the three `refused_by_ident` assertions go red.
    #[test]
    fn names_off_the_wire_match_hyperhives_ident_exactly() {
        for ok in [
            "trollshell-choom",
            "a",
            "agent-9",
            "9",
            "-leading-hyphen", // legal Ident; a P2 argv concern, not this type's
            &"a".repeat(AgentName::MAX_LEN),
        ] {
            assert!(AgentName::parse(ok).is_some(), "should accept {ok:?}");
        }
        // Rejected by `Ident` and therefore by us — the §11.2 gap this closes.
        for refused_by_ident in ["Agent_9", "AGENT", "snake_case_agent"] {
            assert!(
                AgentName::parse(refused_by_ident).is_none(),
                "hyperhive's Ident refuses {refused_by_ident:?}, so must we"
            );
        }
        for bad in [
            "",
            "../etc/passwd",
            "has space",
            "colon:in:name",
            "dot.separated",
            "naïve",
            &"a".repeat(AgentName::MAX_LEN + 1),
        ] {
            assert!(AgentName::parse(bad).is_none(), "should reject {bad:?}");
        }
    }

    fn cfg_with_projects(pairs: &[(&str, &str)]) -> AgentsConfig {
        let mut cfg = AgentsConfig::default();
        for (agent, project) in pairs {
            cfg.display.insert(
                (*agent).to_owned(),
                crate::config::Display {
                    project: Some((*project).to_owned()),
                    ..crate::config::Display::default()
                },
            );
        }
        cfg
    }

    /// Grouping: named projects alphabetically, the ungrouped bucket last, and
    /// an unlabelled agent lands in it rather than vanishing.
    #[test]
    fn rows_group_by_project_and_the_unlabelled_land_in_ungrouped() {
        let agents = vec![
            agent("zeta", flags(false, false, false, true)),
            agent("alpha", flags(false, false, false, true)),
            agent("orphan", flags(false, false, false, true)),
        ];
        let cfg = cfg_with_projects(&[("zeta", "viberoot"), ("alpha", "nixos")]);
        let groups = group(&agents, &cfg);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].project, Some("nixos"));
        assert_eq!(groups[1].project, Some("viberoot"));
        assert_eq!(groups[2].project, None);
        assert_eq!(groups[2].header(), Group::UNGROUPED);
        assert_eq!(groups[2].agents.len(), 1);
        assert_eq!(groups[2].agents[0].name.as_str(), "orphan");
        // Nothing is lost on the way through.
        let rendered: usize = groups.iter().map(|g| g.agents.len()).sum();
        assert_eq!(rendered, agents.len());
    }

    /// Spec §6.3: with one project the header is suppressed.
    #[test]
    fn one_group_suppresses_the_header_and_two_do_not() {
        let agents = vec![
            agent("a", flags(false, false, false, true)),
            agent("b", flags(false, false, false, true)),
        ];
        let bare = AgentsConfig::default();
        let all_ungrouped = group(&agents, &bare);
        assert_eq!(all_ungrouped.len(), 1);
        assert!(!headers_wanted(&all_ungrouped));

        let one_named = cfg_with_projects(&[("a", "viberoot"), ("b", "viberoot")]);
        let single = group(&agents, &one_named);
        assert_eq!(single.len(), 1);
        assert!(!headers_wanted(&single));

        let split = cfg_with_projects(&[("a", "viberoot")]);
        let two = group(&agents, &split);
        assert_eq!(two.len(), 2);
        assert!(headers_wanted(&two));
    }

    /// The agent page URL is the hive's word and nothing else. Falsification:
    /// re-add a `<home>agent/<name>/` fallback and the last two assertions —
    /// a hive predating hyperhive#4073, and one with the domain unconfigured —
    /// both start producing a link the hive never promised.
    #[test]
    fn the_agent_url_is_the_hives_own_and_is_never_derived() {
        let mut a = agent("choom", flags(false, false, false, true));
        assert_eq!(agent_url(&a), None, "a row without a url has no link");

        a.row.url = Some("https://hive.local/agent/choom/".to_owned());
        assert_eq!(agent_url(&a), Some("https://hive.local/agent/choom/"));

        a.row.url = Some("   ".to_owned());
        assert_eq!(agent_url(&a), None, "a blank url is not a link");
    }

    /// The family word, from every spelling the hive is known to send and from
    /// the one Annika wrote out on #963.
    ///
    /// The date-suffixed case is hers verbatim, `[100m]` and all: the point of
    /// the rule is that an unbounded tail cannot reach the chip.
    ///
    /// Falsification: match only the **first** token and `claude-opus-4-6` —
    /// what the fixture actually carries — starts rendering `Claude`; split on
    /// `-` alone and `opus-5.2-…` renders `Opus` still but `anthropic/…` stops
    /// resolving.
    #[test]
    fn a_model_id_shortens_to_its_family_word() {
        for (raw, want) in [
            ("opus-5.2-20262981923899321898[100m]", "Opus"),
            ("claude-opus-4-6", "Opus"),
            ("claude-sonnet-4-6", "Sonnet"),
            ("anthropic/claude-3-5-haiku-20241022", "Haiku"),
            ("OPUS", "Opus"),
            ("Sonnet", "Sonnet"),
        ] {
            assert_eq!(
                model_family(raw).as_deref(),
                Some(want),
                "{raw} should read {want}"
            );
        }
    }

    /// A family this plugin has never heard of still shows something: the first
    /// token, first character upper-cased and the **rest left alone**, so an
    /// all-caps product name is not mangled.
    ///
    /// Falsification: lower-case the tail and `GLM-4.6` renders `Glm`;
    /// title-case the whole token and `gpt-4o` renders `Gpt4o`.
    #[test]
    fn an_unknown_model_falls_back_to_its_first_token() {
        assert_eq!(model_family("gpt-4o").as_deref(), Some("Gpt"));
        assert_eq!(model_family("GLM-4.6").as_deref(), Some("GLM"));
        assert_eq!(model_family("mistral").as_deref(), Some("Mistral"));
    }

    /// Nothing to shorten is no chip, not an empty one.
    ///
    /// Falsification: return `Some(String::new())` for the empty case and the
    /// card grows a blank pill between the name and the buttons.
    #[test]
    fn a_model_with_no_word_in_it_has_no_family() {
        assert_eq!(model_family(""), None);
        assert_eq!(model_family("   "), None);
        assert_eq!(model_family("---"), None);
    }

    /// A model id in any script still gets a chip — the word boundary is
    /// Unicode, not ASCII (#963 review, LOW-3).
    ///
    /// The first two would have returned `None` under the old
    /// `is_ascii_alphanumeric` split, rendering **no chip at all** and reading
    /// as "no model". The third is the mixed case: an ASCII family word inside
    /// a non-ASCII id still wins, because the family scan looks at every token.
    ///
    /// Falsification: put `is_ascii_alphanumeric` back and the first two go
    /// `None`; make the family scan look only at the first token and the third
    /// renders `Модель` instead of `Opus`.
    #[test]
    fn a_non_ascii_model_id_still_gets_a_chip() {
        assert_eq!(model_family("модель-4").as_deref(), Some("Модель"));
        assert_eq!(model_family("モデル").as_deref(), Some("モデル"));
        assert_eq!(model_family("модель-opus-4").as_deref(), Some("Opus"));
    }

    #[test]
    fn a_non_up_hive_has_no_agents() {
        assert!(Hive::Connecting.agents().is_empty());
        assert!(
            Hive::Unreachable {
                reason: "x".to_owned()
            }
            .agents()
            .is_empty()
        );
    }
}
