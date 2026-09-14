//! `stats.toml` — **one file, two instances**.
//!
//! The #1248 ask is one binary running twice: the full thing in the top bar,
//! a compact CPU + GPU card in the right sidebar. Two launches therefore need
//! two answers to "which cards do I draw?", and the settled shape (Discussion
//! #1235) is that the file holds **both** and each instance picks its table by
//! its own **mount family** — `[bar]` for the three bar regions, `[sidebar]`
//! for the six sidebar ones. No flag, no second file, no per-instance
//! environment beyond the two the launch already carries
//! ([`HYTTE_PLUGIN_MOUNT`](crate::mount) and `HYTTE_PLUGIN_ID`).
//!
//! # What it rides
//!
//! The `stats` subsystem is a [`hytte_config::subsystem::Subsystem`], which
//! buys the whole #866/#868 layering from a `NAME` and a `DEFAULT_TOML`: the
//! `XDG_CONFIG_DIRS` → `XDG_CONFIG_HOME` search path, the four merge rules
//! (scalars overlay-wins, tables deep-merge, arrays replace, unknown keys
//! **warn**), and the format-preserving writer. `hytte-plugin-agents`'
//! `agents.toml` (#947 P1) is the precedent for a *plugin* linking
//! `hytte-config`: it is a GTK-free leaf library, not a runtime link to the
//! shell, which is the #640 argument that let the control center link it.
//!
//! # A bad value costs its own key, and only its own key
//!
//! Every key is judged on its own in [`StatsConfig::parsed`], so a typo'd
//! `poll_seconds` does not revert the five booleans beside it (#1040 V1) —
//! and neither does a wrong TOML **type**, which is why every schema field
//! below is a raw [`toml::Value`] rather than a `bool` or a `u64` (#1040 T1).
//! [`StatsConfig::validate`] is consequently [`Infallible`]: nothing in this
//! schema constrains anything else in it, so there is no whole-file rule to
//! state. What *is* still whole-file is a layer that is not TOML at all, which
//! degrades to the built-in defaults with a loud `error!`.
//!
//! # P1 renders the sidebar table only
//!
//! `[bar]` is declared, documented and parsed here, and **nothing reads it
//! yet**: P1 (#1250) ships the sidebar card, and the bar instance is P2
//! (#1251). It is declared now rather than later because the two tables are
//! one decision — "which instance shows what" — and a file that grew its
//! second half a release later would have had two shapes to migrate between.
//! [`Stats::for_family`] already answers for both, and is tested for all nine
//! mounts.

use std::convert::Infallible;
use std::time::Duration;

use hytte_config::subsystem::env::EnvKnob;
use hytte_config::subsystem::{InvalidValue, Subsystem, keep, spelling};
use hytte_plugin::proto::Mount;
use serde::{Deserialize, Serialize};

/// The subsystem's file stem: `~/.config/trollshell/stats.toml`.
pub const NAME: &str = "stats";

/// Lower bound on `poll_seconds`. A zero cadence would spin the sampler
/// against `/proc` as fast as the runtime will schedule it; one second is
/// already the cadence the shell's own `sensors` service ticks at, and the
/// rate every `/proc/stat` delta in this tree is computed over.
pub const MIN_POLL_SECONDS: u64 = 1;

/// Upper bound on `poll_seconds` — one minute. Past this the card is a
/// still life rather than a system monitor, and a typo'd `poll_seconds =
/// 6000` should be a named, warned key rather than a card that looks wedged.
pub const MAX_POLL_SECONDS: u64 = 60;

/// The cadence both tables ship with: one sample per second, matching the
/// shell's own `sensors` service.
pub const DEFAULT_POLL_SECONDS: u64 = 1;

/// What a boolean key accepts, phrased to read after "expected".
const BOOL: &str = "true or false";

/// What `poll_seconds` accepts, phrased to read after "expected".
const SECONDS: &str = "a whole number of seconds, 1..=60";

/// The documented default, and the bottom merge layer.
///
/// Kept commented because it is the only place a key is explained, it is what
/// an operator sees when they first open their overlay, and it is parsed on
/// every load — so a syntax error in it fails any test that loads the
/// subsystem rather than surfacing in production.
const DEFAULT_TOML: &str = r#"# trollshell — system stats as a plugin (issue #1250, epic #1248).
#
# ONE file, TWO instances. The same binary can run twice — the full thing in
# the top bar, a compact CPU + GPU card in the right sidebar — and each launch
# reads the table for the mount family it was launched into:
#
#   [bar]       BarLeft, BarCenter, BarRight
#   [sidebar]   SidebarLead/Top/Bottom and their SidebarRight* twins
#
# Nothing here says which instance is which: the launch does, through
# HYTTE_PLUGIN_MOUNT (where it mounts) and HYTTE_PLUGIN_ID (what it calls
# itself, so the host does not reject the second connection as a duplicate).
# See docs/plugin-env.md.
#
# This file is layered — a nix-written base under $XDG_CONFIG_DIRS, your own
# edits under $XDG_CONFIG_HOME — so a rebuild never clobbers a hand edit and a
# hand edit never blocks a rebuild. Delete a key to fall back to the value
# below; `_unset = ["cpu"]` erases a key an underlying layer set (TOML has no
# null, so this is how it is spelt).
#
# A value no parser accepts costs its own key and nothing else: that key takes
# the built-in default, one journal line names it, and every other key in the
# file still applies.

# The right-sidebar card (#1250). This is what P1 draws.
[sidebar]
# The CPU half: the per-core lamp row, the load history trace and the package
# temperature all hang off this. With it off the card is GPU only.
cpu = true
# The per-core lamp row — one dot-matrix cell per logical core, brighter the
# busier the core. The BlinkenLichten row. Off gives a smaller card.
per_core = true
# The load-history trace (an oscilloscope sweep over the last couple of
# minutes of overall CPU load).
history = true
# The package temperature readout. Shows `--` when no hwmon sensor answers.
temperature = true
# The GPU half: a needle gauge on GPU load. It hides itself entirely when
# there is no GPU to read, so `true` on a GPU-less machine costs nothing.
gpu = true
# Seconds between samples while the card is on screen. The sampler parks
# completely while the sidebar is closed, so a closed sidebar costs nothing
# regardless of this value. 1..=60.
poll_seconds = 1

# The bar instance. DECLARED AND PARSED, NOT YET RENDERED: P1 (#1250) ships
# the sidebar card only, and the bar chips are P2 (#1251). Editing these keys
# today changes nothing you can see; they are here so the file has one shape
# rather than two.
[bar]
cpu = true
per_core = false
history = false
temperature = true
gpu = true
poll_seconds = 1
"#;

// ── The resolved form ────────────────────────────────────────────────────────

/// Which table an instance reads, decided by where it mounted.
///
/// Two families, not nine mounts: the six sidebar mounts differ in *which*
/// sidebar and *where in it*, which changes nothing about what a card should
/// contain, while bar-versus-sidebar changes everything (a slim inline chip
/// versus a card with room for a history trace). `Mount::is_bar` is the same
/// split the host itself uses to decide whether to run the visibility push for
/// a connection (#288/#422), so this is the wire's own line rather than a new
/// one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Family {
    /// `BarLeft` / `BarCenter` / `BarRight` → the `[bar]` table.
    Bar,
    /// Every `Sidebar*` mount → the `[sidebar]` table.
    Sidebar,
}

impl Family {
    /// The family a mount belongs to.
    #[must_use]
    pub fn of(mount: Mount) -> Self {
        if mount.is_bar() {
            Self::Bar
        } else {
            Self::Sidebar
        }
    }

    /// The TOML table this family reads — for diagnostics, so a log line can
    /// say which half of the file an instance is running on.
    #[must_use]
    pub fn table(self) -> &'static str {
        match self {
            Self::Bar => "bar",
            Self::Sidebar => "sidebar",
        }
    }
}

/// One instance's resolved settings — what the card actually consults.
///
/// Five booleans, and `clippy::struct_excessive_bools` is allowed rather than
/// worked around: they are five **independent** switches over a file whose
/// whole job is to say which parts of a card to draw, and the usual remedy —
/// collapsing them into a bitflag or an enum — would make `[sidebar] gpu =
/// false` unspellable as one TOML key, which is the thing this type exists to
/// be.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Card {
    /// Draw the CPU half.
    pub cpu: bool,
    /// Draw the per-core lamp row (within the CPU half).
    pub per_core: bool,
    /// Draw the load-history trace (within the CPU half).
    pub history: bool,
    /// Draw the package-temperature readout (within the CPU half).
    pub temperature: bool,
    /// Draw the GPU half, when there is a GPU to draw.
    pub gpu: bool,
    /// Seconds between samples while the surface is on screen.
    pub poll: Duration,
}

impl Card {
    /// The `[sidebar]` defaults in Rust form — the compact card P1 draws.
    #[must_use]
    pub const fn sidebar_default() -> Self {
        Self {
            cpu: true,
            per_core: true,
            history: true,
            temperature: true,
            gpu: true,
            poll: Duration::from_secs(DEFAULT_POLL_SECONDS),
        }
    }

    /// The `[bar]` defaults in Rust form. A bar chip is a slim inline widget,
    /// so the two space-hungry elements — the lamp row and the history sweep —
    /// are off by default there. Nothing reads this until P2 (#1251).
    #[must_use]
    pub const fn bar_default() -> Self {
        Self {
            cpu: true,
            per_core: false,
            history: false,
            temperature: true,
            gpu: true,
            poll: Duration::from_secs(DEFAULT_POLL_SECONDS),
        }
    }
}

impl Default for Card {
    fn default() -> Self {
        Self::sidebar_default()
    }
}

/// Both tables, resolved — [`Subsystem::Resolved`] for [`StatsConfig`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stats {
    /// The `[bar]` table.
    pub bar: Card,
    /// The `[sidebar]` table.
    pub sidebar: Card,
}

impl Stats {
    /// The table this instance reads.
    ///
    /// The whole "one file, two instances" mechanism, in one function — which
    /// is why it is pure and tested against all nine mounts rather than
    /// inlined at its single call site.
    #[must_use]
    pub fn for_family(&self, family: Family) -> Card {
        match family {
            Family::Bar => self.bar,
            Family::Sidebar => self.sidebar,
        }
    }
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            bar: Card::bar_default(),
            sidebar: Card::sidebar_default(),
        }
    }
}

// ── The file form ────────────────────────────────────────────────────────────

/// One table as written, before any value is judged.
///
/// Every field is a raw [`toml::Value`]: that is what keeps serde's verdict —
/// which is **whole-file** — away from a single mistyped key (#1040 T1).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct CardFile {
    cpu: toml::Value,
    per_core: toml::Value,
    history: toml::Value,
    temperature: toml::Value,
    gpu: toml::Value,
    poll_seconds: toml::Value,
}

impl CardFile {
    /// The `[sidebar]` defaults as TOML values.
    fn sidebar_default() -> Self {
        Self::of(Card::sidebar_default())
    }

    /// The `[bar]` defaults as TOML values.
    fn bar_default() -> Self {
        Self::of(Card::bar_default())
    }

    /// A resolved [`Card`] written back out as raw spellings.
    ///
    /// One direction of travel, so the Rust-side defaults cannot drift from the
    /// file-side ones: both `…_default` constructors above go through here,
    /// and `the_shipped_default_parses_and_matches_the_rust_default` pins the
    /// result against [`DEFAULT_TOML`]'s own parse.
    fn of(card: Card) -> Self {
        // `as_secs` is a `u64` and TOML integers are `i64`. The conversion
        // cannot fail for anything this schema accepts (`1..=60`), which is all
        // the two `…_default` constructors ever hand it; the fallback is the
        // ceiling, spelled as a literal because the `const` assertion beside
        // `MAX_POLL_SECONDS` is what keeps them in step without a second cast.
        const CEILING: i64 = 60;
        const _: () = assert!(MAX_POLL_SECONDS == 60, "CEILING mirrors MAX_POLL_SECONDS");
        let secs = i64::try_from(card.poll.as_secs()).unwrap_or(CEILING);
        Self {
            cpu: card.cpu.into(),
            per_core: card.per_core.into(),
            history: card.history.into(),
            temperature: card.temperature.into(),
            gpu: card.gpu.into(),
            poll_seconds: secs.into(),
        }
    }

    /// Judge this table's six spellings, appending every rejection to
    /// `rejected` and falling back per key to `fallback`'s value for it.
    fn parsed(&self, knobs: &CardKnobs, fallback: Card, rejected: &mut Vec<InvalidValue>) -> Card {
        // Every rejection quotes `self.<key>` — the raw `toml::Value` — rather
        // than the spelling the parser was handed, so a wrong *type* reads like
        // any other bad value: `cpu = 5` is reported as `bar.cpu = 5`, not as a
        // serde message about an integer where a boolean was expected.
        Card {
            cpu: keep(
                parse_bool(&spelling(&self.cpu))
                    .map_err(|()| InvalidValue::of(&knobs.cpu, &self.cpu)),
                fallback.cpu,
                rejected,
            ),
            per_core: keep(
                parse_bool(&spelling(&self.per_core))
                    .map_err(|()| InvalidValue::of(&knobs.per_core, &self.per_core)),
                fallback.per_core,
                rejected,
            ),
            history: keep(
                parse_bool(&spelling(&self.history))
                    .map_err(|()| InvalidValue::of(&knobs.history, &self.history)),
                fallback.history,
                rejected,
            ),
            temperature: keep(
                parse_bool(&spelling(&self.temperature))
                    .map_err(|()| InvalidValue::of(&knobs.temperature, &self.temperature)),
                fallback.temperature,
                rejected,
            ),
            gpu: keep(
                parse_bool(&spelling(&self.gpu))
                    .map_err(|()| InvalidValue::of(&knobs.gpu, &self.gpu)),
                fallback.gpu,
                rejected,
            ),
            poll: keep(
                parse_poll(&spelling(&self.poll_seconds))
                    .map_err(|()| InvalidValue::of(&knobs.poll_seconds, &self.poll_seconds)),
                fallback.poll,
                rejected,
            ),
        }
    }
}

impl Default for CardFile {
    /// The sidebar shape — see [`StatsConfig`]'s field attributes for why the
    /// two tables have different per-field defaults and this one is only ever
    /// a type-level requirement.
    fn default() -> Self {
        Self::sidebar_default()
    }
}

/// `~/.config/trollshell/stats.toml`.
///
/// The two fields carry **per-field** serde defaults rather than relying on
/// [`CardFile`]'s own [`Default`], because the two tables genuinely default
/// differently (a bar chip has no room for a history sweep). In practice
/// neither is reached: [`DEFAULT_TOML`] is the bottom merge layer and states
/// every key of both tables, and the merge rule for tables is a *deep* merge,
/// so by the time serde sees a document every key is present. They are
/// declared correctly anyway, because "unreachable" is a property of the
/// current merge rules and not of this type.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StatsConfig {
    #[serde(default = "CardFile::bar_default")]
    bar: CardFile,
    #[serde(default = "CardFile::sidebar_default")]
    sidebar: CardFile,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            bar: CardFile::bar_default(),
            sidebar: CardFile::sidebar_default(),
        }
    }
}

/// `stats.toml` was never spelt as `TROLLSHELL_*` environment variables, so no
/// key here has one to deprecate and this crate never calls
/// [`env::key`](hytte_config::subsystem::env::key) or
/// [`env::removed`](hytte_config::subsystem::env::removed) — [`Subsystem::resolve`]
/// keeps its default "there is no environment to layer" body.
///
/// [`EnvKnob`] is nevertheless how [`InvalidValue`] names a key and states its
/// vocabulary: `InvalidValue::written` reads exactly two of the four fields
/// (`key` and `file_accepts`) and never touches `var`. So each knob below
/// carries an empty `var`, which is unreachable by construction rather than a
/// placeholder waiting to be filled in.
const fn knob(key: &'static str, accepts: &'static str) -> EnvKnob {
    EnvKnob {
        var: "",
        key,
        env_accepts: accepts,
        file_accepts: accepts,
    }
}

/// One table's six knobs. Two consts rather than a prefix spliced at runtime:
/// [`InvalidValue`]'s key is a `&'static str`, so `bar.cpu` and `sidebar.cpu`
/// have to be two literals — and a reader who greps a journal line for the key
/// it names then finds it.
struct CardKnobs {
    cpu: EnvKnob,
    per_core: EnvKnob,
    history: EnvKnob,
    temperature: EnvKnob,
    gpu: EnvKnob,
    poll_seconds: EnvKnob,
}

/// The `[bar]` table's keys, as a journal line spells them.
const BAR_KNOBS: CardKnobs = CardKnobs {
    cpu: knob("bar.cpu", BOOL),
    per_core: knob("bar.per_core", BOOL),
    history: knob("bar.history", BOOL),
    temperature: knob("bar.temperature", BOOL),
    gpu: knob("bar.gpu", BOOL),
    poll_seconds: knob("bar.poll_seconds", SECONDS),
};

/// The `[sidebar]` table's keys, as a journal line spells them.
const SIDEBAR_KNOBS: CardKnobs = CardKnobs {
    cpu: knob("sidebar.cpu", BOOL),
    per_core: knob("sidebar.per_core", BOOL),
    history: knob("sidebar.history", BOOL),
    temperature: knob("sidebar.temperature", BOOL),
    gpu: knob("sidebar.gpu", BOOL),
    poll_seconds: knob("sidebar.poll_seconds", SECONDS),
};

/// One boolean spelling. A quoted `"true"` is accepted for the same reason
/// `core-leds.toml` takes both `rows = 0` and `rows = "rect"`: [`spelling`]
/// renders every non-string TOML value and hands a string through unchanged, so
/// a spelling either parses or it does not and nothing about its TOML type is
/// special-cased (#1040 T1).
fn parse_bool(spelling: &str) -> Result<bool, ()> {
    spelling.parse::<bool>().map_err(|_| ())
}

/// One poll cadence, in whole seconds, inside [`MIN_POLL_SECONDS`]`..=`[`MAX_POLL_SECONDS`].
///
/// Bounded **here** rather than clamped at the call site: a clamp turns
/// `poll_seconds = 6000` into a silent 60, and the reader never learns their
/// file said something else. Parsing `u64` also rejects a negative or
/// fractional spelling without a separate branch — `-1` and `1.5` are simply
/// not `u64`s.
fn parse_poll(spelling: &str) -> Result<Duration, ()> {
    let secs: u64 = spelling.parse().map_err(|_| ())?;
    if (MIN_POLL_SECONDS..=MAX_POLL_SECONDS).contains(&secs) {
        Ok(Duration::from_secs(secs))
    } else {
        Err(())
    }
}

impl Subsystem for StatsConfig {
    const NAME: &'static str = NAME;
    const DEFAULT_TOML: &'static str = DEFAULT_TOML;

    /// Nothing in this schema constrains anything else in it: the six keys of
    /// a table are independent look-and-feel switches plus one bounded number,
    /// and each is judged on its own in [`Self::parsed`]. A whole-file rule
    /// would be the #1040 V1 anti-pattern — one typo reverting every other key
    /// — so the honest `Error` is [`Infallible`].
    type Error = Infallible;

    type Resolved = Stats;

    fn validate(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn parsed(&self) -> (Stats, Vec<InvalidValue>) {
        let mut rejected = Vec::new();
        let stats = Stats {
            bar: self
                .bar
                .parsed(&BAR_KNOBS, Card::bar_default(), &mut rejected),
            sidebar: self
                .sidebar
                .parsed(&SIDEBAR_KNOBS, Card::sidebar_default(), &mut rejected),
        };
        (stats, rejected)
    }
}

/// Load `stats.toml` from the process environment's XDG search path, with one
/// journal line per rejected key, degrading to the built-in defaults (with a
/// loud `error!`) on a layer that is not TOML at all.
///
/// [`hytte_config::subsystem::initial_load`] rather than `load_or_default`
/// because the card consumes [`Stats`], the *resolved* form — and because
/// `initial_load` is where the per-key rejections are actually emitted.
#[must_use]
pub fn load() -> Stats {
    hytte_config::subsystem::initial_load::<StatsConfig>(&hytte_config::xdg::config_layers(NAME))
}

#[cfg(test)]
mod tests {
    use super::{
        BOOL, Card, CardFile, DEFAULT_POLL_SECONDS, Family, MAX_POLL_SECONDS, MIN_POLL_SECONDS,
        SECONDS, Stats, StatsConfig, knob, parse_bool, parse_poll,
    };
    use hytte_config::subsystem::{Subsystem as _, assemble, load_from};
    use hytte_plugin::proto::Mount;
    use std::path::PathBuf;
    use std::time::Duration;

    /// Assemble one overlay body over the built-in default — the seam every
    /// test here uses, which takes layer bodies as plain strings and so never
    /// touches the real `$XDG_CONFIG_HOME` / `$XDG_CONFIG_DIRS`.
    fn from_toml(body: &str) -> StatsConfig {
        assemble::<StatsConfig>(&[(PathBuf::from("overlay.toml"), body.to_owned())])
            .expect("layers assemble")
            .config
    }

    /// …and the resolved form of the same, with the rejections it produced.
    fn resolved(body: &str) -> (Stats, Vec<String>) {
        let config = from_toml(body);
        let (stats, rejected) = config.parsed();
        (stats, rejected.into_iter().map(|r| r.to_string()).collect())
    }

    /// The shipped default parses, validates, and produces exactly the values
    /// the Rust-side [`Stats::default`] claims — the documented default and
    /// the effective one cannot drift.
    #[test]
    fn the_shipped_default_parses_and_matches_the_rust_default() {
        let loaded = assemble::<StatsConfig>(&[]).expect("the built-in default assembles");
        assert!(loaded.unknown_keys.is_empty(), "{:?}", loaded.unknown_keys);
        loaded.config.validate().expect("the default validates");
        assert_eq!(
            loaded.config,
            StatsConfig::default(),
            "DEFAULT_TOML and StatsConfig::default() must be the same file",
        );
        let (stats, rejected) = loaded.config.parsed();
        assert!(rejected.is_empty(), "{rejected:?}");
        assert_eq!(
            stats,
            Stats::default(),
            "DEFAULT_TOML must resolve to Stats::default()",
        );
    }

    /// The two tables differ in the shipped default — which is the only reason
    /// the file has two of them. Stated as literals rather than by comparing
    /// the two `…_default()` constructors to each other, so a mutation that
    /// made them identical reds here.
    #[test]
    fn the_two_tables_ship_different_defaults() {
        let stats = Stats::default();
        assert!(
            stats.sidebar.per_core && stats.sidebar.history,
            "the sidebar card ships the lamp row and the history sweep",
        );
        assert!(
            !stats.bar.per_core && !stats.bar.history,
            "a bar chip has room for neither",
        );
        assert_eq!(stats.bar.poll, Duration::from_secs(DEFAULT_POLL_SECONDS));
        assert_eq!(
            stats.sidebar.poll,
            Duration::from_secs(DEFAULT_POLL_SECONDS)
        );
    }

    /// **The mechanism**: which table each of the nine mounts reads.
    ///
    /// Driven off `Mount::ALL` rather than a list written out here, so a tenth
    /// mount cannot land without a family — and asserted against
    /// `Mount::is_bar` spelled out per arm rather than called, so pointing
    /// `Family::of` at the wrong half of the enum reds.
    ///
    /// **Falsified** by deleting the family selection — having `for_family`
    /// answer `self.sidebar` unconditionally — which reds the three bar rows.
    #[test]
    fn every_mount_reads_the_table_for_its_family() {
        let stats = Stats::default();
        for mount in Mount::ALL {
            let want = match mount {
                Mount::BarLeft | Mount::BarCenter | Mount::BarRight => Family::Bar,
                Mount::SidebarLead
                | Mount::SidebarTop
                | Mount::SidebarBottom
                | Mount::SidebarRightLead
                | Mount::SidebarRightTop
                | Mount::SidebarRightBottom => Family::Sidebar,
            };
            assert_eq!(
                Family::of(mount),
                want,
                "{} belongs to the {} family",
                mount.wire_name(),
                want.table(),
            );
            assert_eq!(
                stats.for_family(Family::of(mount)),
                match want {
                    Family::Bar => stats.bar,
                    Family::Sidebar => stats.sidebar,
                },
                "{} must read the [{}] table",
                mount.wire_name(),
                want.table(),
            );
        }
    }

    /// An overlay that touches one table leaves the other alone — the deep
    /// merge, which is what lets a user turn the lamp row off in the sidebar
    /// without silently re-stating every `[bar]` key.
    #[test]
    fn an_overlay_on_one_table_leaves_the_other_at_its_default() {
        let (stats, rejected) = resolved("[sidebar]\nper_core = false\n");
        assert!(rejected.is_empty(), "{rejected:?}");
        assert!(!stats.sidebar.per_core, "the overlay wins for its own key");
        assert!(
            stats.sidebar.history,
            "…and the sibling keys in that table keep the default",
        );
        assert_eq!(
            stats.bar,
            Card::bar_default(),
            "…and the other table is untouched",
        );
    }

    /// **Per-key tolerance**: one unusable value costs its own key, names
    /// itself, and leaves every other key in the file applied (#1040 V1).
    ///
    /// **Falsified** by making `parsed` `?` on the first bad key (the two good
    /// keys then revert too), or by clamping `poll_seconds` instead of
    /// rejecting it (the rejection list comes back empty).
    #[test]
    fn one_bad_value_costs_its_own_key_and_nothing_else() {
        let (stats, rejected) =
            resolved("[sidebar]\npoll_seconds = 6000\nper_core = false\n[bar]\ncpu = \"maybe\"\n");
        assert_eq!(
            stats.sidebar.poll,
            Card::sidebar_default().poll,
            "a rejected key falls back to the BUILT-IN default for that key",
        );
        assert!(!stats.sidebar.per_core, "its sibling still applies");
        assert_eq!(stats.bar.cpu, Card::bar_default().cpu);
        assert!(stats.bar.temperature, "and the bar table's siblings too");
        assert_eq!(
            rejected.len(),
            2,
            "exactly two keys were rejected: {rejected:?}"
        );
        assert!(
            rejected
                .iter()
                .any(|r| r == "sidebar.poll_seconds = 6000 is not valid; expected a whole number of seconds, 1..=60"),
            "{rejected:?}",
        );
        assert!(
            rejected
                .iter()
                .any(|r| r == "bar.cpu = \"maybe\" is not valid; expected true or false"),
            "{rejected:?}",
        );
    }

    /// A wrong TOML **type** is the same kind of mistake and gets the same
    /// per-key treatment, which is why every schema field is a raw
    /// `toml::Value` (#1040 T1). Without that, serde would reject the whole
    /// layer and every key would revert.
    #[test]
    fn a_wrong_type_is_a_per_key_rejection_not_a_whole_file_one() {
        let (stats, rejected) = resolved("[sidebar]\ngpu = 5\ntemperature = false\n");
        assert!(stats.sidebar.gpu, "the bad key took its built-in default");
        assert!(
            !stats.sidebar.temperature,
            "the good key beside it still applied",
        );
        assert_eq!(rejected.len(), 1, "{rejected:?}");
        assert!(
            rejected[0].contains("sidebar.gpu = 5"),
            "the line quotes the value as the file spells it: {rejected:?}",
        );
    }

    /// An unknown key **warns** and loads (the fourth merge rule), rather than
    /// failing the file — so a `stats.toml` written against a later version
    /// still runs here.
    #[test]
    fn an_unknown_key_is_reported_and_does_not_fail_the_load() {
        let loaded = assemble::<StatsConfig>(&[(
            PathBuf::from("overlay.toml"),
            "[sidebar]\nfrobnicate = true\n".to_owned(),
        )])
        .expect("an unknown key must not fail the load");
        assert_eq!(loaded.unknown_keys, vec!["sidebar.frobnicate".to_owned()]);
        let (stats, rejected) = loaded.config.parsed();
        assert!(rejected.is_empty(), "{rejected:?}");
        assert_eq!(stats, Stats::default());
    }

    /// The bounds, from both sides, and the spellings `u64` parsing rejects
    /// for free.
    #[test]
    fn poll_seconds_is_bounded_at_both_ends() {
        assert_eq!(
            parse_poll(&MIN_POLL_SECONDS.to_string()),
            Ok(Duration::from_secs(MIN_POLL_SECONDS)),
        );
        assert_eq!(
            parse_poll(&MAX_POLL_SECONDS.to_string()),
            Ok(Duration::from_secs(MAX_POLL_SECONDS)),
        );
        for bad in ["0", "61", "-1", "1.5", "true", "", "  1", "1s"] {
            assert!(parse_poll(bad).is_err(), "{bad:?} must be refused");
        }
    }

    /// The boolean parser, including the quoted spelling it deliberately takes.
    #[test]
    fn the_boolean_parser_takes_both_spellings_and_nothing_else() {
        assert_eq!(parse_bool("true"), Ok(true));
        assert_eq!(parse_bool("false"), Ok(false));
        for bad in ["", "yes", "1", "0", "True", "on"] {
            assert!(parse_bool(bad).is_err(), "{bad:?} must be refused");
        }
    }

    /// **The file round trip, through real files in a tempdir** — never the
    /// real XDG directories (#1101: a test that saves through the real
    /// `$XDG_CONFIG_HOME` pollutes the user's config and self-neutralises on
    /// the second run).
    ///
    /// Two layers on disk, lowest precedence first, exactly the shape
    /// `xdg::config_layers` returns: a "nix-written" base and the user's
    /// overlay. The overlay wins per scalar, the base still supplies what the
    /// overlay omits, and the built-in default underlies both.
    #[test]
    fn two_layers_on_disk_merge_lowest_precedence_first() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let base = dir.path().join("base-stats.toml");
        let overlay = dir.path().join("overlay-stats.toml");
        std::fs::write(&base, "[sidebar]\nhistory = false\ngpu = false\n").expect("write base");
        std::fs::write(&overlay, "[sidebar]\ngpu = true\npoll_seconds = 5\n")
            .expect("write overlay");

        let loaded = load_from::<StatsConfig>(&[base, overlay]).expect("both layers load");
        assert!(loaded.unknown_keys.is_empty(), "{:?}", loaded.unknown_keys);
        let (stats, rejected) = loaded.config.parsed();
        assert!(rejected.is_empty(), "{rejected:?}");

        assert!(!stats.sidebar.history, "the base layer's key applies");
        assert!(stats.sidebar.gpu, "the overlay wins where both set a key");
        assert_eq!(stats.sidebar.poll, Duration::from_secs(5));
        assert!(
            stats.sidebar.per_core,
            "and DEFAULT_TOML underlies both for a key neither states",
        );
    }

    /// A layer that is not TOML **at all** is the one whole-file failure this
    /// schema still has, and `load_from` reports it rather than silently
    /// producing defaults — the caller (`initial_load`) is what degrades.
    #[test]
    fn a_layer_that_is_not_toml_is_a_whole_file_error() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let bad = dir.path().join("stats.toml");
        std::fs::write(&bad, "[sidebar\ncpu = true\n").expect("write");
        assert!(
            load_from::<StatsConfig>(&[bad]).is_err(),
            "an unterminated table header is not a per-key problem",
        );
    }

    /// The knob table's shape, which is what every rejection line is built
    /// from: the `var` field is empty by construction because this subsystem
    /// has no deprecated environment variable, and the two fields
    /// `InvalidValue` actually reads carry the key and its vocabulary.
    #[test]
    fn the_knobs_carry_no_environment_variable() {
        let k = knob("sidebar.cpu", BOOL);
        assert_eq!(k.var, "", "stats.toml was never an environment variable");
        assert_eq!(k.key, "sidebar.cpu");
        assert_eq!(k.file_accepts, BOOL);
        assert_eq!(knob("bar.poll_seconds", SECONDS).file_accepts, SECONDS);
    }

    /// `CardFile::of` is the single direction of travel between the resolved
    /// and file forms, so the two `…_default` pairs cannot drift.
    #[test]
    fn the_file_and_resolved_defaults_are_one_value() {
        let mut rejected = Vec::new();
        let bar =
            CardFile::bar_default().parsed(&super::BAR_KNOBS, Card::bar_default(), &mut rejected);
        let sidebar = CardFile::sidebar_default().parsed(
            &super::SIDEBAR_KNOBS,
            Card::sidebar_default(),
            &mut rejected,
        );
        assert!(rejected.is_empty(), "{rejected:?}");
        assert_eq!(bar, Card::bar_default());
        assert_eq!(sidebar, Card::sidebar_default());
    }
}
