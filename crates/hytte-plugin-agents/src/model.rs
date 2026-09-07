//! The plugin's model: what the hive is doing, and how one row reads.
//!
//! Everything here is pure and clock-free — the reducer folds inputs into it
//! and [`crate::view`] projects it. That is what makes the status precedence
//! and the grouping testable without a socket, a host, or a wall clock.

use std::collections::BTreeMap;

use crate::config::AgentsConfig;
use crate::hive::wire::{AgentStatusRow, VersionMismatch};

/// A hive-reported agent name that has passed the whitelist.
///
/// Spec §11 rule two: `<name>` is echoed from the hive's own
/// `AgentStatusRow.name`, never from the config file, and is validated before
/// it reaches anything that concatenates it — the button ids the row builds
/// (`pause:<name>`), the request frames, and in P2 the `RunCommand` argv.
///
/// The whitelist the spec names is `[A-Za-z0-9_-]+`. The hive's own `Ident` is
/// *stricter* (`[a-z0-9-]`, 63 bytes max — `hive-types/src/lib.rs:96-107`), so
/// nothing this accepts can surprise it; the extra breadth only means a hive
/// that widens its own charset does not silently drop rows here. The 63-byte
/// cap is kept because it is the hive's own (`Ident::MAX_LEN`) and it stops an
/// unbounded wire string from becoming an unbounded node id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgentName(String);

impl AgentName {
    /// The hive's own cap (`Ident::MAX_LEN`, `hive-types/src/lib.rs:87`).
    pub const MAX_LEN: usize = 63;

    /// Validate a name off the wire. `None` rejects it — the caller drops the
    /// row and logs, rather than rendering something it cannot address.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        if s.is_empty() || s.len() > Self::MAX_LEN {
            return None;
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
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

/// The badge icon for `needs_update` (spec §6.2's badge row).
pub const UPDATE_BADGE_ICON: &str = "software-update-available-symbolic";

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

#[cfg(test)]
mod tests {
    use super::{Agent, AgentName, Group, Hive, Status, agent_url, group, headers_wanted};
    use crate::config::AgentsConfig;
    use crate::hive::wire::AgentStatusRow;

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

    #[test]
    fn names_off_the_wire_are_whitelisted() {
        assert!(AgentName::parse("trollshell-choom").is_some());
        assert!(AgentName::parse("Agent_9").is_some());
        assert!(AgentName::parse("").is_none());
        assert!(AgentName::parse("../etc/passwd").is_none());
        assert!(AgentName::parse("has space").is_none());
        assert!(AgentName::parse("colon:in:name").is_none());
        assert!(AgentName::parse(&"a".repeat(AgentName::MAX_LEN)).is_some());
        assert!(AgentName::parse(&"a".repeat(AgentName::MAX_LEN + 1)).is_none());
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
