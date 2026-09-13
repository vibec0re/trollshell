//! `agents.toml` — the desktop's whole config column (spec §9).
//!
//! Amendment f split what looked like one question into three owners: the
//! agent's **config flake** holds everything inside the cage, the **hive**
//! holds the host-level nspawn arguments, and **trollshell** holds exactly
//! four things — how to reach the socket, how often to ask, which terminal to
//! launch, and what to call each agent on screen. Nothing from the other two
//! owners is duplicated here; the desktop reads them and never keeps a second
//! copy that can drift.
//!
//! P1 implements three of those four. `terminal` and `session_title` belong to
//! the §7.3 secondary action and `[chat]` to the §7.1 companion window, both
//! of which are phase P2 — and because the fourth merge rule is *unknown keys
//! warn, never fail* (`crates/hytte-config/src/subsystem.rs:43-53`), a file
//! that already carries them loads fine today and simply logs them.
//!
//! # What it rides
//!
//! The `agents` subsystem is a [`hytte_config::subsystem::Subsystem`]
//! (`crates/hytte-config/src/subsystem.rs:78-107`), which buys the whole #866
//! layering from a `NAME` and a `DEFAULT_TOML`: the
//! `XDG_CONFIG_DIRS` → `XDG_CONFIG_HOME` search path, the four merge rules
//! including `_unset`, unknown-key warnings rather than failures, and the
//! format-preserving writer. This is the first production consumer of that
//! module.
//!
//! # The roster is the hive's, not the file's
//!
//! `[display.<name>]` sections only *decorate* agents the hive reports. A
//! section naming an agent that does not exist is inert — it decorates
//! nothing — rather than conjuring a row.

use std::collections::BTreeMap;
use std::time::Duration;

use hytte_config::subsystem::Subsystem;
use serde::{Deserialize, Serialize};

/// The subsystem's file stem: `~/.config/trollshell/agents.toml`.
pub const NAME: &str = "agents";

/// Poll cadence when no file says otherwise.
///
/// Spec §5.4 wrote "default 5 s"; the later contract comment on
/// [#948](https://github.com/vibec0re/trollshell/issues/948) settled on "the
/// client polls `AgentStatus` on a 2 s cadence, which is fine for v1", which
/// is what P1 ships. Either way it is one key in this file, and it stops
/// mattering entirely once `Subscribe { kinds }` (hyperhive#4064) replaces the
/// cadence with a stream.
pub const DEFAULT_POLL_SECONDS: u64 = 2;

/// Lower bound on [`AgentsConfig::poll_seconds`]. A zero (or an accidental
/// sub-second) cadence would hammer the socket; the hive is a state store, not
/// a stream, and one round trip per second is already more than the rows need.
pub const MIN_POLL_SECONDS: u64 = 1;

/// Upper bound on [`AgentsConfig::poll_seconds`] — one hour. Past this the
/// rows are decoration, and a typo'd `poll_seconds = 86400000` should be an
/// error the operator sees rather than a plugin that appears wedged.
pub const MAX_POLL_SECONDS: u64 = 3600;

/// The documented default, and the bottom merge layer.
///
/// Kept commented because it is the only place a key is explained, it is what
/// an operator sees when they first open their overlay, and it is parsed on
/// every load — so a syntax error in it fails any test that loads the
/// subsystem rather than surfacing in production.
const DEFAULT_TOML: &str = r#"# trollshell — the hyperhive agents sidebar (issue #947).
#
# This file is the WHOLE desktop-side surface. Everything about a cage lives
# in the hive; everything inside a cage lives in that agent's own config
# flake, which is a git repo and not a desktop surface at all. What is left is
# below: how to reach the socket, how often to ask, and what to call each
# agent on screen.

# The hive's host admin socket. Override only — the default is hyperhive's
# own. It is 0660 root:hive-admin behind a 0751 dir, so reaching it needs
# membership in `hive-admin` (`services.hyperhive.adminUsers`), and nothing
# else: no root, no polkit, no sudo. Outside the group the rows show a single
# "no hive — permission denied" line, which is the correct unprivileged
# outcome rather than a bug.
socket = "/run/hyperhive/host.sock"

# Seconds between `AgentStatus` polls while the sidebar is open. The poll
# parks entirely while the sidebar is closed, so a closed sidebar costs
# nothing regardless of this value. 1..=3600.
poll_seconds = 2

# Per-agent display overrides. Each section names an agent EXACTLY as the hive
# reports it; a section naming an unknown agent decorates nothing and is
# warned, not an error. All three keys are optional.
#
#   [display.trollshell-choom]
#   label = "choom"                # what the row calls it (default: the name)
#   icon = "starred-symbolic"      # the leading symbolic icon
#   project = "viberoot"           # the group header this row sits under
#
# `project` is the multi-repo workspace the agent's project repo sits under —
# the grouping Annika asked for ("Maybe grouped by <multi-repo-project>").
# An agent with no `project` falls into an "ungrouped" group rather than
# vanishing, and with only one group the header is suppressed entirely.
"#;

/// One agent's display overrides — the `[display.<name>]` table.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct Display {
    /// What the row calls this agent. `None` renders the hive's own name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The leading symbolic icon. `None` renders [`DEFAULT_RUNTIME_ICON`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The group header this row sits under (spec §6.3). `None` lands the
    /// agent in the ungrouped bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// The leading icon a row shows when `[display.<name>].icon` says nothing.
///
/// One constant rather than a per-runtime lookup: decision 3 is "Claude Code
/// only for v1", so there is exactly one runtime to draw and a table would be
/// speculative. When a second runtime exists the hive will say which
/// (`active_model` is already on the row) and this becomes a match.
pub const DEFAULT_RUNTIME_ICON: &str = "system-run-symbolic";

/// `~/.config/trollshell/agents.toml`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct AgentsConfig {
    /// The hive's host admin socket.
    #[serde(default = "default_socket")]
    pub socket: String,
    /// Seconds between `AgentStatus` polls while the sidebar is open.
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
    /// Per-agent display overrides, keyed by the hive's own agent name.
    ///
    /// # The workspace's first map-typed `Subsystem` field, decided on purpose
    ///
    /// `hytte_config::subsystem`'s #1088 rules (see its
    /// "A map is outside the rule; its entries are not" paragraph) asked the
    /// first author to type a map to decide what an *empty entry* means, rather
    /// than inherit it by accident. This is that decision, and it is **drop the
    /// entry**:
    ///
    /// ```toml
    /// [display.argus]     # nothing of `Display`'s in it
    /// ```
    ///
    /// loads as `display = {}` with **no** unknown-key warning. The map itself
    /// swallows the table (a map has no schema to descend into), and the entry
    /// is a `Display` the schema *does* walk, so an entry holding none of its
    /// three optional keys reads as absent and goes.
    ///
    /// That is right for this file rather than merely acceptable, because an
    /// all-`None` `Display` is a **no-op by construction**: `label_for`,
    /// `icon_for` and `project_for` each fall back to the same value whether
    /// the entry is absent or present-and-empty. Keeping it would put a row in
    /// the merged config that changes nothing and that the format-preserving
    /// writer would then have to round-trip. The observable consequence is
    /// confined to that: writing an empty `[display.x]` to reserve a slot does
    /// not reserve one, and re-saving the file drops the heading.
    ///
    /// Pinned by `an_empty_display_entry_is_dropped_and_a_populated_one_is_not`.
    #[serde(default)]
    pub display: BTreeMap<String, Display>,
}

fn default_socket() -> String {
    crate::hive::DEFAULT_SOCKET.to_owned()
}

const fn default_poll_seconds() -> u64 {
    DEFAULT_POLL_SECONDS
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            socket: default_socket(),
            poll_seconds: DEFAULT_POLL_SECONDS,
            display: BTreeMap::new(),
        }
    }
}

impl AgentsConfig {
    /// The poll cadence as a [`Duration`], clamped to the validated range.
    ///
    /// Clamps rather than trusting the field, because [`load_or_default`]
    /// degrades to the built-in default on a validation failure but a caller
    /// holding a hand-built value has bypassed [`Self::validate`] entirely.
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_seconds.clamp(MIN_POLL_SECONDS, MAX_POLL_SECONDS))
    }

    /// This agent's display overrides, or the empty set.
    #[must_use]
    pub fn display_for(&self, agent: &str) -> Option<&Display> {
        self.display.get(agent)
    }

    /// What the row calls `agent`.
    #[must_use]
    pub fn label_for<'a>(&'a self, agent: &'a str) -> &'a str {
        self.display_for(agent)
            .and_then(|d| d.label.as_deref())
            .filter(|l| !l.trim().is_empty())
            .unwrap_or(agent)
    }

    /// The leading icon for `agent`.
    #[must_use]
    pub fn icon_for(&self, agent: &str) -> &str {
        self.display_for(agent)
            .and_then(|d| d.icon.as_deref())
            .filter(|i| !i.trim().is_empty())
            .unwrap_or(DEFAULT_RUNTIME_ICON)
    }

    /// The group header `agent` sits under, or `None` for the ungrouped
    /// bucket (spec §6.3).
    #[must_use]
    pub fn project_for(&self, agent: &str) -> Option<&str> {
        self.display_for(agent)
            .and_then(|d| d.project.as_deref())
            .map(str::trim)
            .filter(|p| !p.is_empty())
    }
}

/// Why an `agents.toml` is unusable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// `socket` is empty or not an absolute path.
    Socket(String),
    /// `poll_seconds` is outside [`MIN_POLL_SECONDS`]..=[`MAX_POLL_SECONDS`].
    PollSeconds(u64),
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Socket(s) => write!(
                f,
                "socket must be an absolute path to the hive's host.sock, got {s:?}"
            ),
            Self::PollSeconds(n) => write!(
                f,
                "poll_seconds must be between {MIN_POLL_SECONDS} and {MAX_POLL_SECONDS}, got {n}"
            ),
        }
    }
}

impl Subsystem for AgentsConfig {
    const NAME: &'static str = NAME;
    const DEFAULT_TOML: &'static str = DEFAULT_TOML;
    type Error = Invalid;

    /// `agents.toml` has **no spellings**, so the resolved form is the file
    /// itself (#1044).
    ///
    /// [`Subsystem::Resolved`] exists to separate "what the file says" from
    /// "what that word means" — `core-leds.toml`'s `style = "crt"` becoming a
    /// `DisplayStyle`, a number of seconds becoming a `Duration`. Every key
    /// here is already its own value: `socket` is a path, `poll_seconds` is a
    /// count, and `[display.<name>]` is three optional strings that are
    /// rendered verbatim. There is no word to judge, so a distinct `Resolved`
    /// type would be this struct with the fields copied across and nothing
    /// done to them.
    ///
    /// The one derived value, [`AgentsConfig::poll_interval`], is a **clamp**
    /// rather than a parse: it cannot reject anything, only pull an
    /// out-of-range number back into the validated band, and it is already a
    /// method so a caller holding a hand-built config gets the same bound.
    ///
    /// # The residual, stated rather than implied
    ///
    /// The trait's doc asks for every field to be a raw `toml::Value` so that
    /// serde's **whole-file** verdict never decides a single key's fate
    /// (#1040 T1). These fields are natively typed instead, so
    /// `poll_seconds = "soon"` is a deserialisation failure for the whole
    /// layer, not one warned key — the same behaviour this subsystem has had
    /// since it was written, and the reason [`Invalid`] still carries
    /// whole-file rules.
    ///
    /// **And it is not only the structural keys** (#963 review, LOW-1). A
    /// **cosmetic per-agent** value reaches the same whole-file verdict, which
    /// is the case worth naming because it is the one nobody expects.
    /// Measured:
    ///
    /// ```toml
    /// socket = "/run/hyperhive/host.sock"
    /// poll_seconds = 9
    /// [display.argus]
    /// label = 5            # invalid type: integer `5`, expected a string
    /// ```
    ///
    /// …fails the layer, so `socket` **and** `poll_seconds` silently revert to
    /// their built-ins — a look-and-feel typo reverting the socket path is
    /// exactly #1040 V1's named anti-pattern ("one typo would revert every
    /// other key"), reached through the softest key in the file. Pinned by
    /// `a_bad_display_value_takes_the_whole_file_down`.
    ///
    /// Adopting the per-key shape means retyping the schema and is a change of
    /// contract, not a rebase; it is #947 follow-up work, and the LOW-1 case
    /// above is what scopes it — **retype `display` first**, since that is
    /// where a typo costs the most relative to what it buys.
    type Resolved = Self;

    fn validate(&self) -> Result<(), Self::Error> {
        if self.socket.trim().is_empty() || !self.socket.starts_with('/') {
            return Err(Invalid::Socket(self.socket.clone()));
        }
        if !(MIN_POLL_SECONDS..=MAX_POLL_SECONDS).contains(&self.poll_seconds) {
            return Err(Invalid::PollSeconds(self.poll_seconds));
        }
        Ok(())
    }

    /// Every key is already its value — see [`Subsystem::Resolved`] above.
    ///
    /// The rejection list is **always** empty, and that is a statement about
    /// this schema rather than a stub: nothing here turns a string into
    /// anything, so there is no per-key verdict to report. The keys that can
    /// still be wrong (`socket`, `poll_seconds`) constrain the file as a whole
    /// and are judged in [`Self::validate`], which is where the trait puts a
    /// whole-file rule.
    fn parsed(&self) -> (Self, Vec<hytte_config::subsystem::InvalidValue>) {
        (self.clone(), Vec::new())
    }
}

/// Load `agents.toml` from the process environment's XDG search path,
/// degrading to the documented default (with a loud `error!`) on any failure.
///
/// `None` only when [`DEFAULT_TOML`] itself does not parse or validate — a bug
/// in this crate, not in anyone's config — and the caller then falls back to
/// [`AgentsConfig::default`], which is the same values expressed in Rust.
#[must_use]
pub fn load() -> AgentsConfig {
    hytte_config::subsystem::load_or_default::<AgentsConfig>().unwrap_or_else(|| {
        tracing::error!("agents.toml: the built-in default does not load — this is a bug");
        AgentsConfig::default()
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AgentsConfig, DEFAULT_RUNTIME_ICON, Display, Invalid, MAX_POLL_SECONDS, MIN_POLL_SECONDS,
    };
    use hytte_config::subsystem::{Subsystem as _, assemble};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    fn from_toml(body: &str) -> AgentsConfig {
        assemble::<AgentsConfig>(&[(PathBuf::from("overlay.toml"), body.to_owned())])
            .expect("layers assemble")
            .config
    }

    /// The shipped default parses, validates, and produces exactly the values
    /// the Rust-side [`AgentsConfig::default`] claims — the documented default
    /// and the effective one cannot drift.
    #[test]
    fn the_shipped_default_parses_and_matches_the_rust_default() {
        let loaded = assemble::<AgentsConfig>(&[]).expect("the built-in default assembles");
        assert_eq!(loaded.config, AgentsConfig::default());
        assert!(loaded.unknown_keys.is_empty(), "{:?}", loaded.unknown_keys);
        loaded.config.validate().expect("the default validates");
    }

    /// #1227 item 1: the exact bytes `programs.trollshell.config.agents`
    /// renders (checked in at `tests/fixtures/agents-nix-rendered.toml`,
    /// pinned byte-for-byte against the live nix module by `flake.nix`'s
    /// `nixos-module-agents-fixture` check) parse through this crate's real
    /// [`Subsystem`] reader — as a single base layer with an empty overlay,
    /// [`assemble`] with the fixture as the one layer beyond
    /// [`AgentsConfig::DEFAULT_TOML`] — and produce exactly the typed values
    /// the nix example set. This never touches the real
    /// `$XDG_CONFIG_HOME`/`$XDG_CONFIG_DIRS`: [`assemble`] takes layer
    /// bodies as plain strings, the same seam every other test in this file
    /// uses.
    ///
    /// Falsification: change one key in the fixture without changing the
    /// nix example that produced it (or vice versa), and either this test
    /// or `nix build .#checks.x86_64-linux.nixos-module-agents-fixture`
    /// goes red — never both silently drifting the same way.
    #[test]
    fn nix_rendered_fixture_round_trips_through_the_real_reader() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agents-nix-rendered.toml");
        let body = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("missing fixture {} ({e})", path.display()));
        let loaded = assemble::<AgentsConfig>(&[(path, body)])
            .expect("the nix-rendered fixture assembles");
        assert!(loaded.unknown_keys.is_empty(), "{:?}", loaded.unknown_keys);
        loaded.config.validate().expect("the fixture validates");

        let mut display = BTreeMap::new();
        display.insert(
            "argus".to_owned(),
            Display {
                label: None,
                icon: Some("starred-symbolic".to_owned()),
                project: None,
            },
        );
        display.insert(
            "trollshell-choom".to_owned(),
            Display {
                label: Some("choom".to_owned()),
                icon: None,
                project: Some("viberoot".to_owned()),
            },
        );
        assert_eq!(
            loaded.config,
            AgentsConfig {
                socket: "/run/hyperhive/host.sock".to_owned(),
                poll_seconds: 5,
                display,
            }
        );
    }

    /// P2's keys (`terminal`, `session_title`, `[chat]`) are unknown to this
    /// schema and must WARN, not fail — an operator who wrote tomorrow's file
    /// today still gets working rows.
    #[test]
    fn phase_two_keys_are_warned_not_fatal() {
        let loaded = assemble::<AgentsConfig>(&[(
            PathBuf::from("overlay.toml"),
            "terminal = [\"alacritty\", \"-e\"]\nsession_title = \"hive-session\"\n\n[chat]\nwidth = 1100\n"
                .to_owned(),
        )])
        .expect("unknown keys never fail the load");
        assert_eq!(loaded.config, AgentsConfig::default());
        assert!(
            loaded.unknown_keys.iter().any(|k| k == "terminal"),
            "{:?}",
            loaded.unknown_keys
        );
        assert!(
            loaded.unknown_keys.iter().any(|k| k.starts_with("chat")),
            "{:?}",
            loaded.unknown_keys
        );
    }

    #[test]
    fn an_overlay_wins_per_scalar_and_leaves_the_rest_at_the_default() {
        let cfg = from_toml("poll_seconds = 7\n");
        assert_eq!(cfg.poll_seconds, 7);
        assert_eq!(cfg.socket, crate::hive::DEFAULT_SOCKET);
    }

    #[test]
    fn display_overrides_decorate_by_agent_name() {
        let cfg = from_toml(
            "[display.trollshell-choom]\nlabel = \"choom\"\nicon = \"starred-symbolic\"\nproject = \"viberoot\"\n",
        );
        assert_eq!(cfg.label_for("trollshell-choom"), "choom");
        assert_eq!(cfg.icon_for("trollshell-choom"), "starred-symbolic");
        assert_eq!(cfg.project_for("trollshell-choom"), Some("viberoot"));
        assert_eq!(
            cfg.display_for("trollshell-choom"),
            Some(&Display {
                label: Some("choom".to_owned()),
                icon: Some("starred-symbolic".to_owned()),
                project: Some("viberoot".to_owned()),
            })
        );
    }

    /// An undecorated agent falls back to its own name, the default icon and
    /// the ungrouped bucket — the roster is the hive's, not the file's.
    #[test]
    fn an_undecorated_agent_falls_back_everywhere() {
        let cfg = AgentsConfig::default();
        assert_eq!(cfg.label_for("alpha"), "alpha");
        assert_eq!(cfg.icon_for("alpha"), DEFAULT_RUNTIME_ICON);
        assert_eq!(cfg.project_for("alpha"), None);
    }

    /// A blank override is treated as absent rather than rendering an empty
    /// row label or an empty group header.
    #[test]
    fn blank_overrides_are_treated_as_absent() {
        let cfg = from_toml("[display.a]\nlabel = \"  \"\nicon = \"\"\nproject = \" \"\n");
        assert_eq!(cfg.label_for("a"), "a");
        assert_eq!(cfg.icon_for("a"), DEFAULT_RUNTIME_ICON);
        assert_eq!(cfg.project_for("a"), None);
    }

    #[test]
    fn validation_rejects_a_relative_socket_and_a_silly_cadence() {
        let relative = AgentsConfig {
            socket: "host.sock".to_owned(),
            ..AgentsConfig::default()
        };
        assert_eq!(
            relative.validate(),
            Err(Invalid::Socket("host.sock".to_owned()))
        );

        let zero = AgentsConfig {
            poll_seconds: 0,
            ..AgentsConfig::default()
        };
        assert_eq!(zero.validate(), Err(Invalid::PollSeconds(0)));

        let huge = AgentsConfig {
            poll_seconds: MAX_POLL_SECONDS + 1,
            ..AgentsConfig::default()
        };
        assert_eq!(
            huge.validate(),
            Err(Invalid::PollSeconds(MAX_POLL_SECONDS + 1))
        );
    }

    /// `poll_interval` clamps even a value `validate` would have rejected, so
    /// a hand-built config can never produce a zero-duration interval (which
    /// `tokio::time::interval` panics on).
    #[test]
    fn the_poll_interval_clamps_a_value_validation_would_have_rejected() {
        let mut cfg = AgentsConfig {
            poll_seconds: 0,
            ..AgentsConfig::default()
        };
        assert_eq!(cfg.poll_interval().as_secs(), MIN_POLL_SECONDS);
        cfg.poll_seconds = u64::MAX;
        assert_eq!(cfg.poll_interval().as_secs(), MAX_POLL_SECONDS);
    }

    /// A key the schema DOES have, carrying the wrong type, is a hard error —
    /// the user asked for something specific and silently substituting a
    /// default would be the invisible-failure mode #641 taught this repo to
    /// avoid. (The contrast with the unknown-key test above is the point.)
    #[test]
    fn a_known_key_of_the_wrong_type_is_an_error_not_a_warning() {
        let err = assemble::<AgentsConfig>(&[(
            PathBuf::from("overlay.toml"),
            "poll_seconds = \"often\"\n".to_owned(),
        )])
        .expect_err("a wrong-typed known key must fail");
        assert!(
            matches!(err, hytte_config::subsystem::ConfigError::Schema(_)),
            "{err:?}"
        );
    }

    /// **An empty `[display.x]` entry is dropped, on purpose** — the decision
    /// `hytte_config::subsystem`'s #1088 rules asked the workspace's first
    /// map-typed field to make. See [`AgentsConfig::display`] for the argument.
    ///
    /// Three cases in one, because the interesting part is that they differ:
    /// the map swallows its own empty table, an empty *entry* reads as absent
    /// and goes, and a populated entry survives untouched. None of the three
    /// warns — an empty entry is not an unknown key, it is a known one holding
    /// nothing.
    ///
    /// Falsification: make `Display` materialise an all-`None` entry (give it
    /// a `#[serde(default)]` non-`Option` field, say) and the second assertion
    /// reds; drop the `#[serde(default)]` from `display` and the first does.
    #[test]
    fn an_empty_display_entry_is_dropped_and_a_populated_one_is_not() {
        let empty_map = assemble::<AgentsConfig>(&[(
            PathBuf::from("overlay.toml"),
            "display = {}\n".to_owned(),
        )])
        .expect("an empty map assembles");
        assert!(empty_map.config.display.is_empty());
        assert!(
            empty_map.unknown_keys.is_empty(),
            "{:?}",
            empty_map.unknown_keys
        );

        let empty_entry = assemble::<AgentsConfig>(&[(
            PathBuf::from("overlay.toml"),
            "[display.argus]\n".to_owned(),
        )])
        .expect("an empty entry assembles");
        assert!(
            empty_entry.config.display.is_empty(),
            "an entry holding none of `Display`'s keys reads as absent: {:?}",
            empty_entry.config.display
        );
        assert!(
            empty_entry.unknown_keys.is_empty(),
            "…and it is not an unknown key: {:?}",
            empty_entry.unknown_keys
        );

        let populated = from_toml("[display.argus]\nlabel = \"a\"\n");
        assert_eq!(populated.label_for("argus"), "a");
    }

    /// **A bad value in a cosmetic per-agent key takes the whole file down**
    /// (#963 review, LOW-1) — the residual [`AgentsConfig::Resolved`] names.
    ///
    /// This is the case worth pinning rather than `poll_seconds`: nobody is
    /// surprised that a broken *structural* key fails the layer, but a typo in
    /// a display label silently reverting the socket path is #1040 V1's named
    /// anti-pattern reached through the softest key in the file. Pinning it
    /// makes the follow-up's scope ("retype `display` first") a fact rather
    /// than an opinion.
    ///
    /// Falsification: retype `display`'s values as `toml::Value` and give
    /// `parsed` a per-key verdict, and this reds — which is exactly the
    /// follow-up landing.
    #[test]
    fn a_bad_display_value_takes_the_whole_file_down() {
        let body = concat!(
            "socket = \"/run/hyperhive/host.sock\"\n",
            "poll_seconds = 9\n",
            "[display.argus]\n",
            "label = 5\n",
        );
        let err = assemble::<AgentsConfig>(&[(PathBuf::from("overlay.toml"), body.to_owned())])
            .expect_err("a wrong-typed display value must fail the layer");
        assert!(
            matches!(err, hytte_config::subsystem::ConfigError::Schema(_)),
            "{err:?}"
        );

        // …and the blast radius is the point: the two good keys are gone with
        // it, because `load_or_default` degrades the whole file.
        let good = from_toml("socket = \"/run/hyperhive/host.sock\"\npoll_seconds = 9\n");
        assert_eq!(good.poll_seconds, 9, "they load fine on their own");
        assert_ne!(
            good.poll_seconds,
            AgentsConfig::default().poll_seconds,
            "…and differ from the built-in, so reverting to it is observable"
        );
    }
}
