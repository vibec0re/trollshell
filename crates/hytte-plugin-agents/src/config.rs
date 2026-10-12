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
//! # A bad value costs its own key, and only its own key (#1451)
//!
//! The file is read in two shapes, on the precedent the other three families
//! set (`core-leds`, `workspaces`, `stats`): [`AgentsFile`] is the file **as
//! written**, every field a raw [`toml::Value`] so serde has nothing to reject
//! (#1040 T1), and [`AgentsConfig`] is the **resolved** value every reader
//! consumes. [`AgentsFile::parsed`] is the one judge between the two, and it
//! judges key by key — so a typo'd `[display.argus].label` costs that label,
//! never the `socket` and `poll_seconds` beside it, which is what it used to
//! cost (#963 review, LOW-1).
//!
//! The names run the other way round from the precedents' (there,
//! `StatsConfig` is the *file* form and `Stats` the resolved one), and that is
//! deliberate: `AgentsConfig` was already the type three crates — this one,
//! `trollshell-agent-window` and `trollshell-control-center` — read as "the
//! config", built by hand in their tests and handed around by value. The file
//! form is the newcomer, so the newcomer took the new name.
//!
//! # The roster is the hive's, not the file's
//!
//! `[display.<name>]` sections only *decorate* agents the hive reports. A
//! section naming an agent that does not exist is inert — it decorates
//! nothing — rather than conjuring a row.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::time::Duration;

use hytte_config::schema::{Family, Field, Kind, Schema};
use hytte_config::subsystem::env::EnvKnob;
use hytte_config::subsystem::{InvalidValue, Subsystem, keep, spelling};
use serde::de::{self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
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
pub const DEFAULT_TOML: &str = r#"# trollshell — the hyperhive agents sidebar (issue #947).
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

// ── The schema (#888 P0 §2a/§3) ──────────────────────────────────────────────

/// What the leaves of `agents.toml` are, for a settings form that has this
/// crate as a library and the shell nowhere in sight.
///
/// It sits **here**, beside the [`DEFAULT_TOML`] it describes, rather than in
/// `hytte-config-families` with the two shell-owned families: this crate links
/// `hytte-config`, so the leaf crate cannot depend on it without a cycle
/// (#888's erratum to §3). The control center already links this crate as a
/// library (#947 P4) and composes the schema in there.
///
/// `display` is a [`Kind::Map`] — a table whose keys are hive agent names this
/// schema cannot know — so it renders **read-only** in v1 and its three rows
/// are checked per entry (spec §1; editing a map is P2). It is also why
/// [`DEFAULT_TOML`] stating none of it is not a mismatch; see
/// [`hytte_config::schema::verify`]'s collection exemption.
pub const SCHEMA: Schema = Schema {
    family: NAME,
    fields: &[
        Field {
            path: "socket",
            // "Absolute" is the half a `Kind` cannot carry, and
            // `AgentsFile::parsed` still enforces it — per key since #1451
            // (`parse_socket`), no longer for the whole file. What this buys
            // is the row refusing a *blank* before the save, which is the
            // other half of the same rule.
            kind: Kind::Text { blank_ok: false },
            doc: "The hive's host admin socket — an absolute path.",
        },
        Field {
            path: "poll_seconds",
            kind: Kind::Int {
                min: MIN_POLL_SECONDS.cast_signed(),
                max: MAX_POLL_SECONDS.cast_signed(),
                // A plain spin row: no words beside the number. `parse_poll`
                // reads the key's `spelling`, so it also takes a quoted
                // integer (`"5"`, `"+5"`, `"0005"`) exactly as `stats`' does;
                // the row only ever writes a bare one.
                also: &[],
            },
            doc: "Seconds between AgentStatus polls while the sidebar is open.",
        },
        Field {
            path: "display",
            kind: Kind::Map(DISPLAY_FIELDS),
            doc: "Per-agent display overrides, keyed exactly as the hive names the agent.",
        },
    ],
};

/// This family, for a settings form to compose into its list (#1360 review,
/// MEDIUM 4).
///
/// [`Family`] lives in `hytte_config::schema`, not in `hytte-config-families`,
/// precisely so this line can exist: that leaf carries only the two *shell*
/// families and cannot depend on this crate. Without it the control center —
/// which already links this crate as a library (#947 P4) — would hand-write
/// the same three fields as a struct literal nothing sweeps.
pub const FAMILY: Family = Family {
    name: NAME,
    schema: &SCHEMA,
    default_toml: DEFAULT_TOML,
};

/// One `[display.<name>]` entry — [`Display`]'s three optional keys.
///
/// All three are `blank_ok: false`, which is a **deliberate narrowing** rather
/// than a mirror of the reader (#1360 review, LOW 8): `label_for`/`icon_for`/
/// `project_for` each `.filter(|s| !s.trim().is_empty())`, so a blank loads
/// as absent, and nix's `nullOr str` will happily render one. A *row* that
/// accepted a blank would be offering the operator a value that means "unset"
/// and does not look like it — the row's own **reset** is how you spell that,
/// and it removes the key instead of writing an empty string nobody can see.
const DISPLAY_FIELDS: &[Field] = &[
    Field {
        path: "label",
        kind: Kind::Text { blank_ok: false },
        doc: "What the row calls this agent; absent renders the hive's own name.",
    },
    Field {
        path: "icon",
        kind: Kind::Text { blank_ok: false },
        doc: "The leading symbolic icon.",
    },
    Field {
        path: "project",
        kind: Kind::Text { blank_ok: false },
        doc: "The group header this row sits under.",
    },
];

/// One agent's display overrides — a `[display.<name>]` table, resolved.
///
/// Nothing deserialises this type directly: the file form's `display` is a
/// raw value, read one key at a time by [`AgentsFile::parsed`], which is what
/// keeps a wrong-typed `label` away from serde's whole-file verdict (#1451).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Display {
    /// What the row calls this agent. `None` renders the hive's own name.
    pub label: Option<String>,
    /// The leading symbolic icon. `None` renders [`DEFAULT_RUNTIME_ICON`].
    pub icon: Option<String>,
    /// The group header this row sits under (spec §6.3). `None` lands the
    /// agent in the ungrouped bucket.
    pub project: Option<String>,
}

/// The leading icon a row shows when `[display.<name>].icon` says nothing.
///
/// One constant rather than a per-runtime lookup: decision 3 is "Claude Code
/// only for v1", so there is exactly one runtime to draw and a table would be
/// speculative. When a second runtime exists the hive will say which
/// (`active_model` is already on the row) and this becomes a match.
pub const DEFAULT_RUNTIME_ICON: &str = "system-run-symbolic";

/// `~/.config/trollshell/agents.toml`, **resolved** — what every reader of the
/// file consumes: the plugin's rows, the companion window's header and the
/// control center's Agents tab.
///
/// [`AgentsFile`] is the file as written, and [`AgentsFile::parsed`] is the
/// only thing that turns one into this, key by key (#1451). A value built by
/// hand bypasses that judge entirely, which is why [`Self::poll_interval`]
/// still clamps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentsConfig {
    /// The hive's host admin socket — an absolute path.
    pub socket: String,
    /// Seconds between `AgentStatus` polls while the sidebar is open.
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
    /// resolves to `display = {}` with **no** rejection and no unknown-key
    /// warning. Since #1451 the file form's `display` is a raw value, and the
    /// decision is stated where the value is judged: [`AgentsFile::parsed`]
    /// drops an entry that resolved to [`Display::default`]. A literally empty
    /// entry is usually gone before that — the reader's own empty-table rule
    /// still sees inside an entry, because the file form's reader hands an
    /// unknown key there to serde as ignored — but an entry whose every key
    /// was *rejected* only meets the second check, and goes the same way: the
    /// rejections are what is reported, and what is left decorates nothing.
    ///
    /// That is right for this file rather than merely acceptable, because an
    /// all-`None` `Display` is a **no-op by construction**: `label_for`,
    /// `icon_for` and `project_for` each fall back to the same value whether
    /// the entry is absent or present-and-empty. Keeping it would put a row in
    /// the resolved config that changes nothing. The observable consequence is
    /// confined to that: writing an empty `[display.x]` to reserve a slot does
    /// not reserve one.
    ///
    /// Pinned by `an_empty_display_entry_is_dropped_and_a_populated_one_is_not`.
    pub display: BTreeMap<String, Display>,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            socket: crate::hive::DEFAULT_SOCKET.to_owned(),
            poll_seconds: DEFAULT_POLL_SECONDS,
            display: BTreeMap::new(),
        }
    }
}

impl AgentsConfig {
    /// The poll cadence as a [`Duration`], clamped to the validated range.
    ///
    /// Clamps rather than trusting the field, because the file's value has
    /// been bounded by [`AgentsFile::parsed`] but a caller holding a
    /// hand-built value has bypassed that judge entirely.
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

// ── The file form (#1451) ────────────────────────────────────────────────────

/// `agents.toml` **as written**, before any value is judged — the
/// [`Subsystem`] the #866 layering reads, and the source of every
/// [`AgentsConfig`].
///
/// Every field is a raw [`toml::Value`], because anything narrower hands the
/// verdict to serde and serde's verdict is **whole-file** (#1040 T1). Until
/// #1451 this was `AgentsConfig` itself, natively typed, so
///
/// ```toml
/// socket = "/srv/hive/host.sock"
/// poll_seconds = 9
/// [display.argus]
/// label = 5            # invalid type: integer `5`, expected a string
/// ```
///
/// failed the whole layer and `socket` **and** `poll_seconds` silently
/// reverted to their built-ins — #1040 V1's named anti-pattern ("one typo
/// would revert every other key"), reached through the softest key in the file
/// (#963 review, LOW-1). Now `label` is one warned line naming
/// `display.*.label` and the agent, and everything else in the file applies.
///
/// `display` is raw **whole**, not a map of typed entries, on `workspaces`'
/// precedent: a `BTreeMap<String, _>` would still hand serde the verdict on an
/// entry that is not a table (`display.argus = "oops"`) and on a `display`
/// that is not a table at all, and both of those would still be whole-file.
///
/// It is read by `raw_display` rather than by `toml::Value`'s own
/// `Deserialize`, for exactly one difference: a key inside an entry that
/// [`Display`] does not have is handed to serde as **ignored**, which is what
/// rule 4's unknown-key report listens for. So `[display.argus] colour = "x"`
/// is the ordinary `unknown key in config; ignoring it` line with
/// `key = "display.argus.colour"` — the agent in the key, and in
/// `Loaded::unknown_keys` — the same as before #1451, rather than a rejection
/// keyed by the `display.*` template (#1457 review, NIT 5).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct AgentsFile {
    socket: toml::Value,
    poll_seconds: toml::Value,
    #[serde(deserialize_with = "raw_display")]
    display: toml::Value,
}

/// `display` as a raw value, every shape kept for the judge — except that a
/// key inside an entry that [`DISPLAY_KEYS`] does not name is **ignored**
/// rather than kept, so `serde_ignored` reports it under its full dotted path
/// (see [`AgentsFile`]). A `display` or an entry that is not a table comes
/// through as the value it is, for the judge to reject on its own.
fn raw_display<'de, D: Deserializer<'de>>(deserializer: D) -> Result<toml::Value, D::Error> {
    deserializer.deserialize_any(Raw::Display)
}

/// Which table `raw_display`'s visitor is walking: the agent map, or one
/// agent's entry.
#[derive(Clone, Copy)]
enum Raw {
    /// `display` itself — every key is an agent name.
    Display,
    /// One `[display.<name>]` — every key should be one of [`DISPLAY_KEYS`].
    Entry,
}

impl<'de> DeserializeSeed<'de> for Raw {
    type Value = toml::Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<toml::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Raw {
    type Value = toml::Value;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("any TOML value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<toml::Value, E> {
        Ok(toml::Value::Boolean(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<toml::Value, E> {
        Ok(toml::Value::Integer(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<toml::Value, E> {
        i64::try_from(v)
            .map(toml::Value::Integer)
            .map_err(E::custom)
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<toml::Value, E> {
        Ok(toml::Value::Float(v))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<toml::Value, E> {
        Ok(toml::Value::String(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<toml::Value, E> {
        Ok(toml::Value::String(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<toml::Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element::<toml::Value>()? {
            items.push(item);
        }
        Ok(toml::Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<toml::Value, A::Error> {
        let mut table = toml::Table::new();
        while let Some(key) = map.next_key::<String>()? {
            let value = match self {
                Self::Display => map.next_value_seed(Self::Entry)?,
                Self::Entry if DISPLAY_KEYS.contains(&key.as_str()) => {
                    map.next_value::<toml::Value>()?
                }
                Self::Entry => {
                    map.next_value::<IgnoredAny>()?;
                    continue;
                }
            };
            table.insert(key, value);
        }
        Ok(toml::Value::Table(table))
    }
}

impl Default for AgentsFile {
    /// [`AgentsConfig::default`]'s socket and cadence as TOML values, and no
    /// display overrides — pinned against [`DEFAULT_TOML`]'s own parse by
    /// `the_shipped_default_parses_and_matches_the_rust_default`.
    fn default() -> Self {
        Self {
            socket: crate::hive::DEFAULT_SOCKET.into(),
            poll_seconds: toml::Value::Integer(DEFAULT_POLL_SECONDS.cast_signed()),
            display: toml::Value::Table(toml::Table::new()),
        }
    }
}

// ── The knobs ────────────────────────────────────────────────────────────────
//
// `InvalidValue` is only constructible from an `EnvKnob`, which carries the key
// name and the "expected …" half of the one-line diagnostic. `agents.toml` was
// never spelt as `TROLLSHELL_*` variables, so there is no deprecation window to
// run, `Subsystem::resolve`'s default ("there is no environment to layer") is
// the honest answer, and every `var` below is empty and never read — the
// `workspaces`/`stats` argument.
//
// The `display` keys are *templates* (`display.*.label`) because an entry's
// keys are per agent and `EnvKnob::key` is `&'static str`. The agent's name
// goes in the value instead, so the line still says which one:
//
//     display.*.label = argus: 5 is not valid; expected a string

const SOCKET: EnvKnob = EnvKnob::same("", "socket", "an absolute path to the hive's host.sock");
const POLL_SECONDS: EnvKnob =
    EnvKnob::same("", "poll_seconds", "a whole number of seconds, 1..=3600");
// `POLL_SECONDS` restates the bounds in words, because a `const` cannot format
// them; this is what keeps the two in step.
const _: () = assert!(
    MIN_POLL_SECONDS == 1 && MAX_POLL_SECONDS == 3600,
    "POLL_SECONDS' wording mirrors MIN_POLL_SECONDS..=MAX_POLL_SECONDS"
);
const DISPLAY: EnvKnob = EnvKnob::same(
    "",
    "display",
    "a table of per-agent overrides keyed by agent name",
);
const DISPLAY_ENTRY: EnvKnob = EnvKnob::same("", "display.*", "a table of label, icon or project");
const LABEL: EnvKnob = EnvKnob::same("", "display.*.label", "a string");
const ICON: EnvKnob = EnvKnob::same("", "display.*.icon", "a string");
const PROJECT: EnvKnob = EnvKnob::same("", "display.*.project", "a string");

/// The keys one `[display.<name>]` entry may carry — [`DISPLAY_FIELDS`]' rows,
/// which `the_schemas_fields_are_the_serde_surface` holds them to. Anything
/// else in an entry is ignored by `raw_display`, which is what makes rule 4
/// report it.
const DISPLAY_KEYS: [&str; 3] = ["label", "icon", "project"];

impl Subsystem for AgentsFile {
    const NAME: &'static str = NAME;
    const DEFAULT_TOML: &'static str = DEFAULT_TOML;

    /// Nothing in this schema constrains anything else in it, so there is no
    /// whole-file rule left to state (#1451).
    ///
    /// `socket` and `poll_seconds` used to be judged here, and a relative
    /// `socket` then cost the whole merged file — `poll_seconds` and every
    /// `[display.*]` entry with it. Each constrains only itself, so each is
    /// judged in [`Self::parsed`] like any other key, and [`Infallible`] is
    /// the honest `Error`, as it is for `core-leds`, `workspaces` and `stats`.
    type Error = Infallible;

    /// [`AgentsConfig`] — see the module docs for why the names run the other
    /// way round from `stats`' and `workspaces`'.
    type Resolved = AgentsConfig;

    fn validate(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    /// The file's raw values as an [`AgentsConfig`], beside **every** key that
    /// did not parse.
    ///
    /// A rejected key falls back to its own built-in default — the default
    /// socket, the default cadence, or no override at all — and nothing else
    /// moves. No `?` anywhere, which is the trait's whole rule (#1040 V1).
    fn parsed(&self) -> (AgentsConfig, Vec<InvalidValue>) {
        let mut rejected = Vec::new();
        let fallback = AgentsConfig::default();
        let socket = keep(
            parse_socket(&self.socket).ok_or_else(|| InvalidValue::of(&SOCKET, &self.socket)),
            fallback.socket,
            &mut rejected,
        );
        let poll_seconds = keep(
            parse_poll(&spelling(&self.poll_seconds))
                .ok_or_else(|| InvalidValue::of(&POLL_SECONDS, &self.poll_seconds)),
            fallback.poll_seconds,
            &mut rejected,
        );
        let display = parse_display(&self.display, &mut rejected);
        (
            AgentsConfig {
                socket,
                poll_seconds,
                display,
            },
            rejected,
        )
    }
}

/// `socket`: a string naming an absolute path — the rule `validate` applied to
/// the whole file before #1451, now applied to this key alone.
///
/// Read with `as_str` rather than through [`spelling`]: a path is only ever a
/// string, and no other TOML type's rendering could start with `/` anyway.
fn parse_socket(value: &toml::Value) -> Option<String> {
    value
        .as_str()
        .filter(|path| path.starts_with('/'))
        .map(str::to_owned)
}

/// `poll_seconds`: a whole number of seconds inside
/// [`MIN_POLL_SECONDS`]`..=`[`MAX_POLL_SECONDS`].
///
/// Bounded **here** rather than clamped, on `stats`' argument: a clamp turns
/// `poll_seconds = 86400000` into a silent 3600, and the reader never learns
/// their file said something else ([`AgentsConfig::poll_interval`] still
/// clamps, for a value built by hand). Parsing a `u64` refuses a negative or
/// fractional spelling without a branch of its own. A quoted integer (`"5"`,
/// and since `u64::from_str` takes a leading `+` and leading zeros, `"+5"` and
/// `"0005"` too) is taken for the reason [`spelling`] gives — `stats`'
/// `parse_poll` takes the same set.
fn parse_poll(spelling: &str) -> Option<u64> {
    spelling
        .parse::<u64>()
        .ok()
        .filter(|seconds| (MIN_POLL_SECONDS..=MAX_POLL_SECONDS).contains(seconds))
}

/// The `display` table, entry by entry and, within an entry, key by key —
/// `workspaces.rs`' `parse_stacks`/`parse_stack`, one level shallower.
///
/// * A `display` that is not a table costs the map, and nothing else.
/// * An entry that is not a table costs that entry; its siblings apply.
/// * Inside an entry, a key of the wrong type costs that key and the entry's
///   other keys apply.
///
/// A key [`Display`] does not have never reaches here: `raw_display` left it
/// out and rule 4 reported it, under its own dotted path (see [`AgentsFile`]).
///
/// No agent *name* is judged: the roster is the hive's, so a name it does not
/// report decorates nothing rather than being an error (the module docs).
fn parse_display(
    value: &toml::Value,
    rejected: &mut Vec<InvalidValue>,
) -> BTreeMap<String, Display> {
    let Some(table) = value.as_table() else {
        rejected.push(InvalidValue::of(&DISPLAY, value));
        return BTreeMap::new();
    };
    let mut display = BTreeMap::new();
    for (agent, entry) in table {
        let Some(fields) = entry.as_table() else {
            rejected.push(InvalidValue::written(
                &DISPLAY_ENTRY,
                &format!("{agent}: {entry}"),
            ));
            continue;
        };
        let overrides = parse_entry(agent, fields, rejected);
        // The empty-entry decision `AgentsConfig::display` argues: an entry
        // that resolved to nothing decorates nothing, so it goes.
        if overrides != Display::default() {
            display.insert(agent.clone(), overrides);
        }
    }
    display
}

/// One `[display.<name>]` entry's three keys, each judged on its own.
///
/// A key is a string or it is rejected — `label = 5` is not quietly rendered
/// as `"5"`, because the operator meant *something* and a row reading `5` is
/// not obviously a mistake. A blank string is taken as written: the readers'
/// own `.filter` treats it as absent (see [`DISPLAY_FIELDS`]).
fn parse_entry(agent: &str, fields: &toml::Table, rejected: &mut Vec<InvalidValue>) -> Display {
    let mut text = |knob: &EnvKnob, key: &str| {
        fields.get(key).and_then(|value| {
            keep(
                value
                    .as_str()
                    .map(|text| Some(text.to_owned()))
                    .ok_or_else(|| InvalidValue::written(knob, &format!("{agent}: {value}"))),
                None,
                rejected,
            )
        })
    };
    Display {
        label: text(&LABEL, "label"),
        icon: text(&ICON, "icon"),
        project: text(&PROJECT, "project"),
    }
}

/// Load `agents.toml` from the process environment's XDG search path, with
/// one journal line per rejected key, degrading to the built-in defaults (with
/// a loud `error!`) only when a layer is unusable **as a file** — not TOML, or
/// unreadable.
///
/// [`hytte_config::subsystem::initial_load`] rather than `load_or_default`, on
/// `stats`' precedent: every reader consumes [`AgentsConfig`], the resolved
/// form, and `initial_load` is where the per-key rejections are emitted. Its
/// fallback is [`AgentsConfig::default`], which
/// `the_shipped_default_parses_and_matches_the_rust_default` pins to
/// [`DEFAULT_TOML`]'s own values.
#[must_use]
pub fn load() -> AgentsConfig {
    hytte_config::subsystem::initial_load::<AgentsFile>(&hytte_config::xdg::config_layers(NAME))
}

#[cfg(test)]
mod tests {
    use super::{
        AgentsConfig, AgentsFile, DEFAULT_POLL_SECONDS, DEFAULT_RUNTIME_ICON, DEFAULT_TOML,
        DISPLAY_KEYS, Display, FAMILY, Kind, MAX_POLL_SECONDS, MIN_POLL_SECONDS, NAME, SCHEMA,
    };
    use hytte_config::schema;
    use hytte_config::subsystem::{
        FindingKind, InvalidValue, Subsystem as _, assemble, assemble_base_layers,
    };
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    // ── The schema (#888 P0) ────────────────────────────────────────────────

    /// The exported [`FAMILY`] is this family, and it verifies — so a form
    /// composing it into its list gets the same sweep
    /// `hytte-config-families`' own `FAMILIES` gets (#1360 review, MEDIUM 4).
    #[test]
    fn the_exported_family_is_this_one_and_verifies() {
        assert_eq!(FAMILY.name, NAME);
        assert_eq!(FAMILY.schema, &SCHEMA);
        assert_eq!(FAMILY.default_toml, DEFAULT_TOML);
        FAMILY
            .verify()
            .expect("the exported family agrees with itself");
    }

    /// **The walker.** Every leaf the documented default states has a `Field`,
    /// every non-collection `Field` names a leaf it states, and every stated
    /// value is inside its declared `Kind`.
    #[test]
    fn the_schema_matches_the_documented_default() {
        schema::verify(&SCHEMA, DEFAULT_TOML)
            .expect("agents' schema and its documented default must agree");
    }

    /// The schema's field set is the **file form's** surface, both ways — top
    /// level and inside a `[display.<name>]` entry.
    ///
    /// This is the direction the walker structurally cannot see: `DEFAULT_TOML`
    /// leaves `[display.*]` commented out, so its keys are a strict subset of
    /// what the reader takes, and a key with no row would otherwise be
    /// invisible to every gate. It is the same comparison
    /// `nix/lint-config-vocab.py` makes textually against the nix option tree
    /// — measured there on a planted `pub chat_url: Option<String>` that left
    /// every other gate green.
    ///
    /// Since #1451 an entry's keys are no serde surface at all — the file
    /// form's `display` is a raw value — so the inside half holds the rows to
    /// [`DISPLAY_KEYS`], the list an unknown key is reported against, and then
    /// proves the judge reads each of them into its own [`Display`] field. The
    /// destructuring makes that exhaustive: a fourth `Display` field fails to
    /// compile here until it is read and has a row.
    #[test]
    fn the_schemas_fields_are_the_serde_surface() {
        let file = toml::Value::try_from(AgentsFile::default()).expect("serialises");
        let mut top: Vec<&str> = file
            .as_table()
            .expect("a table")
            .keys()
            .map(String::as_str)
            .collect();
        top.sort_unstable();
        let mut declared: Vec<&str> = SCHEMA.fields.iter().map(|field| field.path).collect();
        declared.sort_unstable();
        assert_eq!(declared, top, "one Field per file-form field, and no more");

        let Some(Kind::Map(rows)) = SCHEMA.field("display").map(|field| field.kind) else {
            panic!("`display` must be a map of per-agent entries");
        };
        assert_eq!(
            rows.iter().map(|field| field.path).collect::<Vec<_>>(),
            DISPLAY_KEYS,
            "one row per key an entry may carry, and no more"
        );

        let (cfg, rejected) = parse("[display.a]\nlabel = \"l\"\nicon = \"i\"\nproject = \"p\"\n");
        assert!(rejected.is_empty(), "{rejected:?}");
        let Display {
            label,
            icon,
            project,
        } = cfg.display_for("a").cloned().expect("the entry applies");
        assert_eq!(
            (label.as_deref(), icon.as_deref(), project.as_deref()),
            (Some("l"), Some("i"), Some("p")),
            "every key an entry may carry reaches its own field"
        );
    }

    /// `poll_seconds`' spin range is the range the judge enforces — the bound
    /// `nix/lint-config-vocab.py` already mirrors into the nix option, now
    /// mirrored into the form as well.
    #[test]
    fn the_schemas_cadence_range_is_the_parsers_own() {
        let Some(Kind::Int { min, max, also }) =
            SCHEMA.field("poll_seconds").map(|field| field.kind)
        else {
            panic!("`poll_seconds` must be a bounded integer");
        };
        assert!(
            also.is_empty(),
            "a plain spin row: the parser takes a number"
        );
        assert_eq!(min, MIN_POLL_SECONDS.cast_signed());
        assert_eq!(max, MAX_POLL_SECONDS.cast_signed());

        let at = |seconds: u64| parse(&format!("poll_seconds = {seconds}\n"));
        for inside in [MIN_POLL_SECONDS, MAX_POLL_SECONDS] {
            let (cfg, rejected) = at(inside);
            assert!(rejected.is_empty(), "{inside}: {rejected:?}");
            assert_eq!(cfg.poll_seconds, inside);
        }
        for outside in [MIN_POLL_SECONDS - 1, MAX_POLL_SECONDS + 1] {
            let (cfg, rejected) = at(outside);
            assert_eq!(keys(&rejected), ["poll_seconds"], "{outside}");
            assert_eq!(cfg.poll_seconds, DEFAULT_POLL_SECONDS, "{outside}");
        }
    }

    // ── The reader ──────────────────────────────────────────────────────────

    /// One overlay over [`DEFAULT_TOML`], assembled and judged.
    fn parse(body: &str) -> (AgentsConfig, Vec<InvalidValue>) {
        assemble::<AgentsFile>(&[(PathBuf::from("overlay.toml"), body.to_owned())])
            .expect("layers assemble")
            .config
            .parsed()
    }

    fn from_toml(body: &str) -> AgentsConfig {
        parse(body).0
    }

    /// Every rejection as its journal line renders it.
    fn lines(rejected: &[InvalidValue]) -> Vec<String> {
        rejected.iter().map(ToString::to_string).collect()
    }

    /// Every rejection's key.
    fn keys(rejected: &[InvalidValue]) -> Vec<&'static str> {
        rejected.iter().map(InvalidValue::key).collect()
    }

    /// The shipped default parses, judges clean, and produces exactly the
    /// values the Rust-side [`AgentsConfig::default`] claims — the documented
    /// default and the effective one cannot drift.
    #[test]
    fn the_shipped_default_parses_and_matches_the_rust_default() {
        let loaded = assemble::<AgentsFile>(&[]).expect("the built-in default assembles");
        assert_eq!(loaded.config, AgentsFile::default());
        assert!(loaded.unknown_keys.is_empty(), "{:?}", loaded.unknown_keys);
        let (resolved, rejected) = loaded.config.parsed();
        assert!(
            rejected.is_empty(),
            "the default is all valid: {rejected:?}"
        );
        assert_eq!(resolved, AgentsConfig::default());
    }

    /// #1227 item 1: the exact bytes `programs.trollshell.config.agents`
    /// renders (checked in at `tests/fixtures/agents-nix-rendered.toml`,
    /// pinned byte-for-byte against the live nix module by `flake.nix`'s
    /// `nixos-module-agents-fixture` check) parse through this crate's real
    /// [`Subsystem`](hytte_config::subsystem::Subsystem) reader — as a single
    /// base layer with an empty overlay, [`assemble`] with the fixture as the
    /// one layer beyond [`AgentsFile::DEFAULT_TOML`] — and produce exactly
    /// the typed values the nix example set, with nothing rejected. This never
    /// touches the real `$XDG_CONFIG_HOME`/`$XDG_CONFIG_DIRS`: [`assemble`]
    /// takes layer bodies as plain strings, the same seam every other test in
    /// this file uses.
    ///
    /// Since #1227 the fixture also carries the `_locked` list nix renders
    /// beside the values, so this test is where the wire between the two
    /// halves is checked: the marker must be stripped before the schema sees
    /// it (no `unknown_keys` entry, no field, no rejection), and
    /// [`hytte_config::subsystem::Loaded::locked`] must come back naming
    /// exactly the five leaves the nix example set. It goes in through
    /// [`assemble_base_layers`] rather than [`assemble`] because that is what
    /// it is — a nix-written base layer, with no overlay file beside it —
    /// and a lock binds the overlay, so a base read as one would pin nothing.
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
        let loaded = assemble_base_layers::<AgentsFile>(&[(path, body)])
            .expect("the nix-rendered fixture assembles");
        assert!(loaded.unknown_keys.is_empty(), "{:?}", loaded.unknown_keys);

        assert_eq!(
            loaded.locked.iter().map(String::as_str).collect::<Vec<_>>(),
            [
                "display.argus.icon",
                "display.trollshell-choom.label",
                "display.trollshell-choom.project",
                "poll_seconds",
                "socket",
            ],
            "#1227: the lock names exactly the leaves the nix example set"
        );
        assert!(
            loaded.lock_findings.is_empty(),
            "a base layer alone has nothing above it to refuse: {:?}",
            loaded.lock_findings
        );

        let (resolved, rejected) = loaded.config.parsed();
        assert!(
            rejected.is_empty(),
            "the fixture is all valid: {rejected:?}"
        );

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
            resolved,
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
        let loaded = assemble::<AgentsFile>(&[(
            PathBuf::from("overlay.toml"),
            "terminal = [\"alacritty\", \"-e\"]\nsession_title = \"hive-session\"\n\n[chat]\nwidth = 1100\n"
                .to_owned(),
        )])
        .expect("unknown keys never fail the load");
        assert_eq!(
            loaded.config.parsed(),
            (AgentsConfig::default(), Vec::new())
        );
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
    /// row label or an empty group header — and it is not a *rejection*: it
    /// is a string, and the readers' own filter is what makes it absent.
    #[test]
    fn blank_overrides_are_treated_as_absent() {
        let (cfg, rejected) = parse("[display.a]\nlabel = \"  \"\nicon = \"\"\nproject = \" \"\n");
        assert!(rejected.is_empty(), "{rejected:?}");
        assert_eq!(cfg.label_for("a"), "a");
        assert_eq!(cfg.icon_for("a"), DEFAULT_RUNTIME_ICON);
        assert_eq!(cfg.project_for("a"), None);
    }

    /// The two rules `validate` applied to the **whole file** before #1451 — a
    /// relative `socket`, an out-of-range cadence — now each cost their own key,
    /// fall back to that key's built-in, and are named once. The display entry
    /// beside them applies, which is the part that used to go with them.
    #[test]
    fn a_relative_socket_and_a_silly_cadence_cost_only_themselves() {
        let (cfg, rejected) = parse(concat!(
            "socket = \"host.sock\"\n",
            "poll_seconds = 0\n",
            "[display.argus]\n",
            "label = \"Argus\"\n",
        ));
        assert_eq!(cfg.socket, crate::hive::DEFAULT_SOCKET);
        assert_eq!(cfg.poll_seconds, DEFAULT_POLL_SECONDS);
        assert_eq!(
            cfg.label_for("argus"),
            "Argus",
            "the display entry beside them applies"
        );
        assert_eq!(
            lines(&rejected),
            [
                "socket = \"host.sock\" is not valid; expected an absolute path to the hive's host.sock",
                "poll_seconds = 0 is not valid; expected a whole number of seconds, 1..=3600",
            ]
        );

        let (huge, rejected) = parse(&format!("poll_seconds = {}\n", MAX_POLL_SECONDS + 1));
        assert_eq!(huge.poll_seconds, DEFAULT_POLL_SECONDS);
        assert_eq!(keys(&rejected), ["poll_seconds"]);

        let (blank, rejected) = parse("socket = \"\"\n");
        assert_eq!(blank.socket, crate::hive::DEFAULT_SOCKET);
        assert_eq!(keys(&rejected), ["socket"]);
    }

    /// `poll_interval` clamps even a value the judge would have rejected, so
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

    /// A key the schema DOES have, carrying the wrong TOML type, is **named** —
    /// silently substituting a default would be the invisible-failure mode
    /// #641 taught this repo to avoid — and since #1451 it costs that key
    /// alone. Before, it was a `ConfigError::Schema` for the layer and every
    /// other key reverted with it.
    ///
    /// The contrast with `phase_two_keys_are_warned_not_fatal` still holds,
    /// one level down: that is rule 4's `unknown_keys`, this is the judge's own
    /// rejection.
    #[test]
    fn a_known_key_of_the_wrong_type_costs_that_key_and_is_named() {
        let loaded = assemble::<AgentsFile>(&[(
            PathBuf::from("overlay.toml"),
            "socket = \"/srv/hive/host.sock\"\npoll_seconds = \"often\"\n".to_owned(),
        )])
        .expect("a wrong-typed known key no longer fails the layer");
        assert!(
            loaded.unknown_keys.is_empty(),
            "it is not an unknown key: {:?}",
            loaded.unknown_keys
        );
        let (cfg, rejected) = loaded.config.parsed();
        assert_eq!(
            cfg.socket, "/srv/hive/host.sock",
            "the key beside it applies"
        );
        assert_eq!(cfg.poll_seconds, DEFAULT_POLL_SECONDS);
        assert_eq!(
            lines(&rejected),
            ["poll_seconds = \"often\" is not valid; expected a whole number of seconds, 1..=3600"]
        );
    }

    /// **An empty `[display.x]` entry is dropped, on purpose** — the decision
    /// `hytte_config::subsystem`'s #1088 rules asked the workspace's first
    /// map-typed field to make. See [`AgentsConfig::display`] for the argument.
    ///
    /// Three cases in one, because the interesting part is that they differ:
    /// an empty map resolves to an empty map, an empty *entry* resolves to
    /// nothing and goes, and a populated entry survives untouched. None of the
    /// three warns or is rejected — an empty entry is not a mistake, it is a
    /// known key holding nothing.
    ///
    /// The literally-empty entry here is dropped twice over: by the reader's
    /// own #1088 empty-table rule (which sees inside an entry because
    /// `raw_display` hands it unknown keys as ignored) and by
    /// `parse_display`'s `Display::default()` guard. The guard alone is what
    /// drops an entry whose every key was *rejected*, which is
    /// `a_bad_display_on_one_agent_leaves_another_agents_alone`'s pin.
    #[test]
    fn an_empty_display_entry_is_dropped_and_a_populated_one_is_not() {
        let empty_map =
            assemble::<AgentsFile>(&[(PathBuf::from("overlay.toml"), "display = {}\n".to_owned())])
                .expect("an empty map assembles");
        assert!(
            empty_map.unknown_keys.is_empty(),
            "{:?}",
            empty_map.unknown_keys
        );
        let (resolved, rejected) = empty_map.config.parsed();
        assert!(resolved.display.is_empty());
        assert!(rejected.is_empty(), "{rejected:?}");

        let empty_entry = assemble::<AgentsFile>(&[(
            PathBuf::from("overlay.toml"),
            "[display.argus]\n".to_owned(),
        )])
        .expect("an empty entry assembles");
        assert!(
            empty_entry.unknown_keys.is_empty(),
            "…and it is not an unknown key: {:?}",
            empty_entry.unknown_keys
        );
        let (resolved, rejected) = empty_entry.config.parsed();
        assert!(
            resolved.display.is_empty(),
            "an entry holding none of `Display`'s keys resolves to nothing: {:?}",
            resolved.display
        );
        assert!(
            rejected.is_empty(),
            "…and nothing was rejected: {rejected:?}"
        );

        let populated = from_toml("[display.argus]\nlabel = \"a\"\n");
        assert_eq!(populated.label_for("argus"), "a");
    }

    /// **A bad value in a cosmetic per-agent key costs that key, and only that
    /// key** (#963 review, LOW-1; #1451) — the opposite pin of the
    /// `a_bad_display_value_takes_the_whole_file_down` that stood here, whose
    /// own doc named this test's arrival as its falsification.
    ///
    /// The file is that test's file, except that `socket` is not the built-in
    /// one (so keeping it is observable) and the entry carries a good `icon`
    /// beside the bad `label` (so "costs the key" is told apart from "costs the
    /// entry").
    ///
    /// Falsification: make `AgentsFile::validate` refuse whenever `parsed`
    /// rejects anything — the whole-file shape this replaced — and the
    /// `assemble` below fails; make `parse_entry` drop the whole entry on one
    /// bad key and the `icon` assertion reds.
    #[test]
    fn a_bad_display_value_costs_only_its_own_key() {
        let body = concat!(
            "socket = \"/srv/hive/host.sock\"\n",
            "poll_seconds = 9\n",
            "[display.argus]\n",
            "label = 5\n",
            "icon = \"starred-symbolic\"\n",
        );
        let loaded = assemble::<AgentsFile>(&[(PathBuf::from("overlay.toml"), body.to_owned())])
            .expect("a wrong-typed display value no longer fails the layer");
        let (cfg, rejected) = loaded.config.parsed();

        assert_eq!(
            cfg.socket, "/srv/hive/host.sock",
            "the socket is the file's"
        );
        assert_eq!(cfg.poll_seconds, 9, "…and so is the cadence");
        assert_ne!(
            cfg.poll_seconds,
            AgentsConfig::default().poll_seconds,
            "…which differs from the built-in, so a revert would be observable"
        );
        assert_eq!(
            cfg.icon_for("argus"),
            "starred-symbolic",
            "the entry's good key applies"
        );
        assert_eq!(
            cfg.label_for("argus"),
            "argus",
            "the bad one falls back to the hive's own name"
        );

        assert_eq!(
            keys(&rejected),
            ["display.*.label"],
            "one rejection, naming the key"
        );
        assert_eq!(
            lines(&rejected),
            ["display.*.label = argus: 5 is not valid; expected a string"],
            "…and the agent it was for"
        );
    }

    /// The other side of the same pin: a file whose every key is valid
    /// resolves every key, with nothing rejected — the per-key judge drops
    /// nothing it was not asked to.
    #[test]
    fn a_valid_display_still_applies() {
        let (cfg, rejected) = parse(concat!(
            "socket = \"/srv/hive/host.sock\"\n",
            "poll_seconds = 9\n",
            "[display.trollshell-choom]\n",
            "label = \"choom\"\n",
            "icon = \"starred-symbolic\"\n",
            "project = \"viberoot\"\n",
        ));
        assert!(rejected.is_empty(), "{rejected:?}");

        let mut display = BTreeMap::new();
        display.insert(
            "trollshell-choom".to_owned(),
            Display {
                label: Some("choom".to_owned()),
                icon: Some("starred-symbolic".to_owned()),
                project: Some("viberoot".to_owned()),
            },
        );
        assert_eq!(
            cfg,
            AgentsConfig {
                socket: "/srv/hive/host.sock".to_owned(),
                poll_seconds: 9,
                display,
            }
        );
    }

    /// A bad entry costs **its own agent's** decoration, never another
    /// agent's. `argus` has nothing valid left, so it decorates nothing and
    /// goes; `trollshell-choom` beside it applies whole.
    ///
    /// Falsification: make `parse_display` stop at the first entry with a
    /// rejection (`break` instead of carrying on) and the `trollshell-choom`
    /// assertions red.
    #[test]
    fn a_bad_display_on_one_agent_leaves_another_agents_alone() {
        let (cfg, rejected) = parse(concat!(
            "[display.argus]\n",
            "label = 5\n",
            "project = [\"viberoot\"]\n",
            "[display.trollshell-choom]\n",
            "label = \"choom\"\n",
            "project = \"viberoot\"\n",
        ));
        assert_eq!(cfg.label_for("trollshell-choom"), "choom");
        assert_eq!(cfg.project_for("trollshell-choom"), Some("viberoot"));
        assert_eq!(
            cfg.display_for("argus"),
            None,
            "an entry whose every key was rejected decorates nothing, so it goes"
        );
        assert_eq!(
            lines(&rejected),
            [
                "display.*.label = argus: 5 is not valid; expected a string",
                "display.*.project = argus: [\"viberoot\"] is not valid; expected a string",
            ]
        );
    }

    /// An entry that is not a table at all costs **that entry** — not the map
    /// it sits in, and not the file. This is the shape a `BTreeMap` of typed
    /// entries would still have handed to serde whole-file, and the reason
    /// `display` is raw all the way down.
    #[test]
    fn a_display_entry_that_is_not_a_table_costs_only_that_entry() {
        let (cfg, rejected) = parse(concat!(
            "poll_seconds = 9\n",
            "[display]\n",
            "argus = \"oops\"\n",
            "[display.trollshell-choom]\n",
            "label = \"choom\"\n",
        ));
        assert_eq!(cfg.poll_seconds, 9);
        assert_eq!(cfg.label_for("trollshell-choom"), "choom");
        assert_eq!(cfg.display_for("argus"), None);
        assert_eq!(
            lines(&rejected),
            [
                "display.* = argus: \"oops\" is not valid; expected a table of label, icon or project"
            ]
        );
    }

    /// A `display` that is not a table costs the map, and nothing else.
    #[test]
    fn a_display_that_is_not_a_table_costs_only_the_map() {
        let (cfg, rejected) =
            parse("socket = \"/srv/hive/host.sock\"\npoll_seconds = 9\ndisplay = 5\n");
        assert_eq!(cfg.socket, "/srv/hive/host.sock");
        assert_eq!(cfg.poll_seconds, 9);
        assert!(cfg.display.is_empty());
        assert_eq!(
            lines(&rejected),
            [
                "display = 5 is not valid; expected a table of per-agent overrides keyed by agent name"
            ]
        );
    }

    /// An unknown key **inside** an entry is rule 4's, exactly as it was
    /// before #1451: one `unknown_keys` entry under its **full** dotted path —
    /// the agent in the key, not the `display.*` template a rejection would
    /// carry (#1457 review, NIT 5) — and no rejection, because it is not a bad
    /// value for a key the schema has. The entry's good keys still apply.
    ///
    /// Falsification: have `raw_display` keep an unknown entry key as a
    /// `toml::Value` instead of ignoring it, and the `unknown_keys` assertion
    /// reds (empty) — the misspelt `lable` would then vanish with nothing
    /// logged.
    #[test]
    fn an_unknown_key_inside_a_display_entry_is_reported_and_its_siblings_apply() {
        let loaded = assemble::<AgentsFile>(&[(
            PathBuf::from("overlay.toml"),
            "[display.argus]\nlable = \"Argus\"\nicon = \"starred-symbolic\"\n".to_owned(),
        )])
        .expect("an unknown key never fails the load");
        assert_eq!(
            loaded.unknown_keys,
            ["display.argus.lable"],
            "rule 4, keyed by the agent"
        );
        let (cfg, rejected) = loaded.config.parsed();
        assert_eq!(cfg.icon_for("argus"), "starred-symbolic");
        assert_eq!(cfg.label_for("argus"), "argus");
        assert!(rejected.is_empty(), "not a rejected value: {rejected:?}");
    }

    /// A rejected `socket` costs the socket and nothing beside it — the sibling
    /// direction `a_relative_socket_and_a_silly_cadence_cost_only_themselves`
    /// cannot see, because there both keys are bad (#1457 review, LOW 2).
    #[test]
    fn a_bad_socket_costs_only_the_socket() {
        let (cfg, rejected) = parse(concat!(
            "socket = \"relative.sock\"\n",
            "poll_seconds = 9\n",
            "[display.argus]\n",
            "label = \"A\"\n",
        ));
        assert_eq!(cfg.socket, crate::hive::DEFAULT_SOCKET);
        assert_eq!(
            cfg.poll_seconds, 9,
            "the cadence beside a bad socket applies"
        );
        assert_eq!(cfg.label_for("argus"), "A");
        assert_eq!(keys(&rejected), ["socket"]);
    }

    /// **The merge rules, `_unset` and a nix `_locked` still compose with the
    /// per-key judge** now that the file form is raw (#1457 review, LOW 3) —
    /// three layers: a low base, a nix-written base that locks `socket` and
    /// one display leaf, and an overlay that tries to override both.
    ///
    /// The judge never sees `_locked` (the merge applies it before anything is
    /// deserialised), so there is no per-key lock check to delete; the
    /// mutation that reds this is the reader's own — fold the overlay in as a
    /// base in `hytte-config`'s `assemble_layers`, and the lock stops binding.
    #[test]
    fn locks_deep_merge_unset_and_per_key_compose_across_layers() {
        let loaded = assemble::<AgentsFile>(&[
            (
                PathBuf::from("/etc/xdg/low/trollshell/agents.toml"),
                "[display.argus]\nproject = \"base0\"\n\
                 [display.choom]\nlabel = \"C0\"\n\
                 [display.dropme]\nlabel = \"x\"\n"
                    .to_owned(),
            ),
            (
                PathBuf::from("/etc/xdg/nix/trollshell/agents.toml"),
                "_locked = [\"socket\", \"display.argus.label\"]\n\
                 socket = \"/srv/nix/host.sock\"\n\
                 [display.argus]\nlabel = \"Nix\"\nicon = \"nix-icon\"\n"
                    .to_owned(),
            ),
            (
                PathBuf::from("/home/u/.config/trollshell/agents.toml"),
                "socket = \"/srv/mine.sock\"\npoll_seconds = 9\n\
                 [display]\n_unset = [\"dropme\"]\n\
                 [display.argus]\nlabel = \"Mine\"\nproject = \"mine\"\n_unset = [\"icon\"]\n\
                 [display.choom]\nicon = 5\n"
                    .to_owned(),
            ),
        ])
        .expect("layers assemble");
        assert!(loaded.unknown_keys.is_empty(), "{:?}", loaded.unknown_keys);
        assert!(
            loaded.unset_findings.is_empty(),
            "{:?}",
            loaded.unset_findings
        );
        let mut shadowed: Vec<&str> = loaded
            .lock_findings
            .iter()
            .filter(|finding| finding.kind == FindingKind::ShadowedLockedKey)
            .map(|finding| finding.key.as_str())
            .collect();
        shadowed.sort_unstable();
        assert_eq!(
            shadowed,
            ["display.argus.label", "socket"],
            "each refused override reported exactly once: {:?}",
            loaded.lock_findings
        );

        let (cfg, rejected) = loaded.config.parsed();
        assert_eq!(
            cfg.socket, "/srv/nix/host.sock",
            "the lock binds the overlay"
        );
        assert_eq!(cfg.poll_seconds, 9, "rule 1: the overlay's scalar wins");
        assert_eq!(
            cfg.display_for("argus"),
            Some(&Display {
                label: Some("Nix".to_owned()),
                icon: None,
                project: Some("mine".to_owned()),
            }),
            "rule 2 per agent across three layers; `_unset` inside an entry; the lock inside an entry"
        );
        assert_eq!(
            cfg.display_for("choom"),
            Some(&Display {
                label: Some("C0".to_owned()),
                icon: None,
                project: None,
            }),
            "a bad overlay key costs itself; the base's sibling key survives"
        );
        assert_eq!(
            cfg.display_for("dropme"),
            None,
            "`[display] _unset` erases an entry"
        );
        assert_eq!(
            lines(&rejected),
            ["display.*.icon = choom: 5 is not valid; expected a string"]
        );
    }

    /// The merge's markers never reach the judge: an overlay's
    /// `[display] _unset = ["argus"]` erases a base layer's entry and is not
    /// itself reported as an agent called `_unset` — which it would be, were
    /// the merge to leave it in the raw table `parse_display` walks.
    #[test]
    fn an_unset_display_entry_is_erased_and_not_reported() {
        let loaded = assemble::<AgentsFile>(&[
            (
                PathBuf::from("/etc/xdg/trollshell/agents.toml"),
                "[display.argus]\nlabel = \"Argus\"\n[display.trollshell-choom]\nlabel = \"choom\"\n"
                    .to_owned(),
            ),
            (
                PathBuf::from("overlay.toml"),
                "[display]\n_unset = [\"argus\"]\n".to_owned(),
            ),
        ])
        .expect("layers assemble");
        let (cfg, rejected) = loaded.config.parsed();
        assert!(rejected.is_empty(), "{rejected:?}");
        assert_eq!(cfg.display_for("argus"), None);
        assert_eq!(cfg.label_for("trollshell-choom"), "choom");
    }
}
