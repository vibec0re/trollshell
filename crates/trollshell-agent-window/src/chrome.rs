//! The chrome's **models** — what the header, the buttons and the settings
//! page show, as plain data derived from [`AgentState`].
//!
//! Split from the widgets ([`crate::ui`]) for the reason the plugin splits its
//! reducer from its view: every rule about what the window says is then
//! testable without a display server, and the widget layer is a set of
//! `set_label` calls a single display test can pin.

use hytte_plugin_agents::config::AgentsConfig;
use hytte_plugin_agents::hive::wire::HiveUrls;
use hytte_plugin_agents::model::{AgentName, Status, agent_url, model_family};

use crate::feed::AgentState;

/// What the window's header line shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderModel {
    /// The agent's display label (`agents.toml`'s `display.<name>.label`, else
    /// the name).
    pub title: String,
    /// The symbolic icon name, from the same config the card reads.
    pub icon: String,
    /// The **short** model word — `Opus`, never `opus-5.2-2026…`. Annika on
    /// #963: "keep Model name short — Opus is enough."
    pub model_word: Option<String>,
    /// The full model id, for the short word's hover.
    pub model_full: Option<String>,
    /// The status line — the harness's own text for a running agent, the
    /// state's word otherwise, the failure's sentence when there is one.
    pub status: String,
    /// The symbolic icon beside it.
    pub status_icon: String,
    /// Its style class.
    pub status_class: String,
}

/// The connecting state's text, before the first answer.
pub const CONNECTING: &str = "connecting to the hive…";
/// What a window shows when its agent is not on this hive's roster.
pub const UNKNOWN_AGENT: &str = "no agent by that name on this hive";

impl HeaderModel {
    /// Derive the header from the agent's state.
    #[must_use]
    pub fn of(name: &AgentName, cfg: &AgentsConfig, state: &AgentState) -> Self {
        let (status, status_icon, status_class) = match state {
            AgentState::Connecting => (
                CONNECTING.to_owned(),
                "content-loading-symbolic".to_owned(),
                "dim-label".to_owned(),
            ),
            AgentState::Unreachable { reason } => (
                reason.clone(),
                "dialog-error-symbolic".to_owned(),
                "error".to_owned(),
            ),
            AgentState::Unknown => (
                UNKNOWN_AGENT.to_owned(),
                "dialog-warning-symbolic".to_owned(),
                "warning".to_owned(),
            ),
            AgentState::Up(agent) => {
                let s = agent.status();
                (
                    agent.status_line().to_owned(),
                    s.icon().to_owned(),
                    s.class().to_owned(),
                )
            }
        };
        let model_full = state
            .agent()
            .and_then(|a| a.row.active_model.as_deref())
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned);
        Self {
            title: cfg.label_for(name.as_str()).to_owned(),
            icon: cfg.icon_for(name.as_str()).to_owned(),
            model_word: model_full.as_deref().and_then(model_family),
            model_full,
            status,
            status_icon,
            status_class,
        }
    }
}

/// Which of the lifecycle buttons are live, and what the pause toggle shows.
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per widget property, the way `AgentStatusRow` carries one per hive flag \
              (and with the same allow): these are three separate widgets' states plus the \
              gate, not a state machine — `can_start`/`can_stop` are exclusive but `paused` is \
              orthogonal to both, so an enum over them would have to enumerate the product"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Controls {
    /// `Start` is offered — the agent is stopped.
    pub can_start: bool,
    /// `Stop` is offered — the agent is not stopped.
    pub can_stop: bool,
    /// The pause toggle's position.
    pub paused: bool,
    /// Whether any of them may be pressed at all. False until the hive has
    /// said what this agent is: a verb sent into an unknown state is a verb
    /// sent on a guess.
    pub live: bool,
}

impl Controls {
    /// Derive the buttons from the agent's state.
    ///
    /// `Start` and `Stop` are **mutually exclusive**, keyed on the same
    /// `Status::Stopped` test the card's lifecycle button uses, so the two
    /// surfaces cannot disagree about which verb an agent is offered.
    #[must_use]
    pub fn of(state: &AgentState) -> Self {
        let Some(agent) = state.agent() else {
            return Self {
                can_start: false,
                can_stop: false,
                paused: false,
                live: false,
            };
        };
        let stopped = agent.status() == Status::Stopped;
        Self {
            can_start: stopped,
            can_stop: !stopped,
            paused: agent.paused(),
            live: true,
        }
    }
}

/// One read-only row on the settings page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fact {
    /// The row's title.
    pub label: &'static str,
    /// Its value, already rendered — [`Facts::ABSENT`] when the hive has none.
    pub value: String,
}

/// The settings page's content.
///
/// **v1 is read-only, deliberately.** Editing an agent belongs to phase P5 of
/// #947 and is mostly not the shell's to edit at all: each agent has its own
/// config flake, and only the host-level remainder is the hive's (spec §9,
/// three owners). So this page shows what `host.sock` already says, plus where
/// this window is reading it from, and the window's buttons are the only
/// writes it performs.
pub struct Facts;

impl Facts {
    /// What a row shows when the hive has no value for it.
    pub const ABSENT: &'static str = "—";

    /// The agent's own facts.
    #[must_use]
    pub fn agent(name: &AgentName, state: &AgentState) -> Vec<Fact> {
        let row = state.agent().map(|a| &a.row);
        let opt = |v: Option<&String>| {
            v.map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map_or_else(|| Self::ABSENT.to_owned(), str::to_owned)
        };
        vec![
            Fact {
                label: "Name",
                value: name.as_str().to_owned(),
            },
            Fact {
                label: "Model",
                value: opt(row.and_then(|r| r.active_model.as_ref())),
            },
            Fact {
                label: "Status",
                value: match state {
                    AgentState::Up(a) => a.status_line().to_owned(),
                    AgentState::Connecting => CONNECTING.to_owned(),
                    AgentState::Unknown => UNKNOWN_AGENT.to_owned(),
                    AgentState::Unreachable { reason } => reason.clone(),
                },
            },
            Fact {
                label: "Status set",
                value: opt(row.and_then(|r| r.status_set_at.as_ref())),
            },
            Fact {
                label: "Deployed",
                value: opt(row.and_then(|r| r.deployed_sha.as_ref())),
            },
            Fact {
                label: "Parent",
                value: opt(row.and_then(|r| r.parent.as_ref())),
            },
            Fact {
                label: "Agent page",
                value: state
                    .agent()
                    .and_then(agent_url)
                    .map_or_else(|| Self::ABSENT.to_owned(), str::to_owned),
            },
        ]
    }

    /// Where this window is reading from.
    #[must_use]
    pub fn hive(cfg: &AgentsConfig, urls: Option<&HiveUrls>) -> Vec<Fact> {
        let opt = |v: Option<&String>| {
            v.map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map_or_else(|| Self::ABSENT.to_owned(), str::to_owned)
        };
        vec![
            Fact {
                label: "Domain",
                value: opt(urls.and_then(|u| u.domain.as_ref())),
            },
            Fact {
                label: "Dashboard",
                value: opt(urls.and_then(|u| u.home.as_ref())),
            },
            Fact {
                label: "Socket",
                value: cfg.socket.clone(),
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::{CONNECTING, Controls, Facts, HeaderModel, UNKNOWN_AGENT};
    use crate::feed::AgentState;
    use hytte_plugin_agents::config::{AgentsConfig, Display};
    use hytte_plugin_agents::hive::wire::{AgentStatusRow, HiveUrls};
    use hytte_plugin_agents::model::{Agent, AgentName};

    fn name(s: &str) -> AgentName {
        AgentName::parse(s).expect("a legal test name")
    }

    fn up(row: AgentStatusRow) -> AgentState {
        AgentState::Up(Box::new(Agent {
            name: name(&row.name.clone()),
            row,
            pending_paused: None,
        }))
    }

    fn running() -> AgentStatusRow {
        AgentStatusRow {
            name: "stray".to_owned(),
            running: true,
            active_model: Some("claude-opus-5-20262981".to_owned()),
            status_text: Some("reviewing PR #963".to_owned()),
            ..AgentStatusRow::default()
        }
    }

    /// The header carries the harness's own words and the **short** model,
    /// with the full id kept for the hover.
    ///
    /// Mutation (verified red): show `active_model` verbatim and the
    /// `model_word` assertion reds — the chip would read
    /// `claude-opus-5-20262981` in a header sized for `Opus`.
    #[test]
    fn a_running_agents_header_is_its_status_text_and_a_short_model() {
        let h = HeaderModel::of(&name("stray"), &AgentsConfig::default(), &up(running()));
        assert_eq!(h.title, "stray");
        assert_eq!(h.status, "reviewing PR #963");
        assert_eq!(h.status_class, "accent");
        assert_eq!(h.model_word.as_deref(), Some("Opus"));
        assert_eq!(h.model_full.as_deref(), Some("claude-opus-5-20262981"));
    }

    /// `agents.toml`'s display block drives this window too — the same label
    /// and icon the sidebar card shows, from the same `AgentsConfig`.
    ///
    /// Mutation (verified red): read `name.as_str()` directly instead of
    /// `cfg.label_for` and both assertions red.
    #[test]
    fn the_display_config_the_card_reads_names_this_window_too() {
        let mut cfg = AgentsConfig::default();
        cfg.display.insert(
            "stray".to_owned(),
            Display {
                label: Some("Stray cat".to_owned()),
                icon: Some("face-smile-symbolic".to_owned()),
                project: None,
            },
        );
        let h = HeaderModel::of(&name("stray"), &cfg, &up(running()));
        assert_eq!(h.title, "Stray cat");
        assert_eq!(h.icon, "face-smile-symbolic");
    }

    /// The three non-agent states each say their own thing, and none of them
    /// borrows another's icon.
    #[test]
    fn every_state_has_its_own_sentence_and_glyph() {
        let cfg = AgentsConfig::default();
        let n = name("stray");
        let connecting = HeaderModel::of(&n, &cfg, &AgentState::Connecting);
        let unknown = HeaderModel::of(&n, &cfg, &AgentState::Unknown);
        let down = HeaderModel::of(
            &n,
            &cfg,
            &AgentState::Unreachable {
                reason: "no socket — hive-c0re is not running".to_owned(),
            },
        );
        assert_eq!(connecting.status, CONNECTING);
        assert_eq!(unknown.status, UNKNOWN_AGENT);
        assert_eq!(down.status, "no socket — hive-c0re is not running");
        let icons = [
            connecting.status_icon,
            unknown.status_icon,
            down.status_icon,
        ];
        let mut distinct = icons.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            icons.len(),
            "each state is distinguishable at a glance: {icons:?}"
        );
        assert!(connecting.model_word.is_none(), "no agent, no model chip");
    }

    /// Start and Stop are mutually exclusive, keyed on the same `Stopped` test
    /// the card uses, and neither is live until the hive has answered.
    ///
    /// Mutation (verified red): key `can_start` on `!running` instead of
    /// `Status::Stopped` and the paused case reds — a paused agent is running,
    /// and offering it `Start` would be a no-op button.
    #[test]
    fn the_lifecycle_buttons_are_exclusive_and_dead_until_the_hive_answers() {
        for state in [
            AgentState::Connecting,
            AgentState::Unknown,
            AgentState::Unreachable {
                reason: "down".to_owned(),
            },
        ] {
            let c = Controls::of(&state);
            assert!(!c.live && !c.can_start && !c.can_stop, "{state:?}");
        }

        let c = Controls::of(&up(running()));
        assert_eq!(
            (c.live, c.can_start, c.can_stop, c.paused),
            (true, false, true, false)
        );

        let stopped = AgentStatusRow {
            running: false,
            ..running()
        };
        let c = Controls::of(&up(stopped));
        assert_eq!(
            (c.live, c.can_start, c.can_stop, c.paused),
            (true, true, false, false)
        );

        let paused = AgentStatusRow {
            paused: true,
            ..running()
        };
        let c = Controls::of(&up(paused));
        assert_eq!(
            (c.live, c.can_start, c.can_stop, c.paused),
            (true, false, true, true),
            "a paused agent is running — it is offered Stop, not Start"
        );
    }

    /// The settings page shows what the hive says, and a dash where it says
    /// nothing — never an empty row and never a guess.
    #[test]
    fn the_settings_facts_render_absence_as_a_dash() {
        let facts = Facts::agent(&name("stray"), &AgentState::Connecting);
        let by = |l: &str| {
            facts
                .iter()
                .find(|f| f.label == l)
                .unwrap_or_else(|| panic!("no {l} row"))
                .value
                .clone()
        };
        assert_eq!(by("Name"), "stray");
        assert_eq!(by("Model"), Facts::ABSENT);
        assert_eq!(by("Agent page"), Facts::ABSENT);
        assert_eq!(by("Status"), CONNECTING);

        let row = AgentStatusRow {
            url: Some("https://hive.local/agent/stray/".to_owned()),
            deployed_sha: Some("abc123def456".to_owned()),
            ..running()
        };
        let facts = Facts::agent(&name("stray"), &up(row));
        let by = |l: &str| {
            facts
                .iter()
                .find(|f| f.label == l)
                .expect("row")
                .value
                .clone()
        };
        assert_eq!(by("Model"), "claude-opus-5-20262981", "the FULL id here");
        assert_eq!(by("Agent page"), "https://hive.local/agent/stray/");
        assert_eq!(by("Deployed"), "abc123def456");
    }

    /// The hive block names where this window is reading from — the socket
    /// path included, because "the hive is unreachable" is unactionable
    /// without it.
    #[test]
    fn the_hive_facts_name_the_socket_even_with_no_urls() {
        let cfg = AgentsConfig::default();
        let facts = Facts::hive(&cfg, None);
        assert!(
            facts
                .iter()
                .any(|f| f.label == "Socket" && f.value == cfg.socket),
            "{facts:?}"
        );
        assert!(facts.iter().all(|f| !f.value.is_empty()));

        let urls = HiveUrls {
            domain: Some("hive.local".to_owned()),
            home: Some("  https://hive.local/  ".to_owned()),
        };
        let facts = Facts::hive(&cfg, Some(&urls));
        assert!(
            facts
                .iter()
                .any(|f| f.label == "Dashboard" && f.value == "https://hive.local/"),
            "trimmed, like everywhere else the hive's strings are read: {facts:?}"
        );
    }
}
