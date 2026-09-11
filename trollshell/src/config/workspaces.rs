//! `workspaces.toml` — the saved workspace stacks (#1071 §4).
//!
//! A **stack** is the apps of one named niri workspace, the monitor it lives
//! on, and the layout template applied to it. The file is the only thing this
//! epic makes the shell own: everything else is niri's state or systemd's
//! (#1071 §8).
//!
//! ```toml
//! order = ["chat", "dev", "music"]      # cards top to bottom; arrays replace whole
//!
//! [workspace.chat]
//! monitor   = "DP-1"                    # optional; absent = the focused monitor
//! autostart = true
//! layout    = "golden"                  # equal | golden | split | none
//! apps = [
//!   { id = "org.mozilla.firefox" },
//!   { id = "Alacritty", exec = "alacritty -e weechat" },
//! ]
//! ```
//!
//! # Why a table keyed by name rather than an array of stacks
//!
//! #868's merge rules **deep-merge tables** and **replace arrays whole**. As a
//! table, a home-manager base can pin `chat` while the overlay adds `music` and
//! changes only `chat`'s layout; as an array, the overlay would have to restate
//! every stack to change one. `apps` and `order` stay arrays on purpose, for
//! the same reason read the other way: editing them *is* replacing them.
//!
//! Removing a base-pinned stack from the overlay is `_unset` (rule 1), and
//! because `_unset` is honoured in the table it appears in, it is spelt
//! `[workspace]` + `_unset = ["chat"]` — at the level that holds the stack,
//! not inside the stack.
//!
//! # Why the whole schema is two `Option<toml::Value>`s
//!
//! The pilot's rule (#1040 T1): a schema field typed anything narrower than
//! `toml::Value` hands the verdict to serde, and serde's verdict is
//! *whole-file*. One mistyped `autostart` would cost every other stack in the
//! file. So both keys arrive raw and [`WorkspacesConfig::parsed`] is the single
//! judge — per stack, and within a stack per key.
//!
//! `Option`, and `skip_serializing_if`, for a second reason that is specific to
//! this subsystem: **arrays replace whole**, so an overlay that carries `order`
//! at all overrides the base's order entirely. A Save that wrote `order = []`
//! because the schema has a default would silently discard a base-pinned card
//! order. With `Option` the writer emits a key only if the overlay already had
//! one, so phase 2's Save — which sets a name and nothing else (#1071 §3.7) —
//! cannot touch a key it was not asked about. [`DEFAULT_TOML`] is comments only
//! for the same reason, and because it is honest: the shell ships no stacks.
//!
//! [`DEFAULT_TOML`]: WorkspacesConfig::DEFAULT_TOML

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::prelude::Service;
use hytte::reactive::{registry, spawn_supervised};
use hytte::services::systemd::normalize_workspace_name;
use hytte_config::subsystem::env::{EnvKnob, process_env};
use hytte_config::subsystem::watch::{self, EnvLookup};
use hytte_config::subsystem::{ConfigError, InvalidValue, Subsystem, keep};
use hytte_config::xdg;

/// The layout template applied once, after every app of a stack has launched
/// (#1071 §3.4 step 4). The file has carried it since phase 2; phase 3 is what
/// spawns `hytte-plugin-niri-layouts apply <layout>` at the end of a Start.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Layout {
    /// Every column the same width.
    Equal,
    /// The golden-ratio pair `hytte-plugin-niri-layouts` applies (#1019/#1026).
    Golden,
    /// Two columns, split.
    Split,
    /// Leave niri's own layout alone.
    #[default]
    None,
}

impl Layout {
    /// The spelling this layout is written as in the file, and the argument
    /// `hytte-plugin-niri-layouts apply` takes.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Equal => "equal",
            Self::Golden => "golden",
            Self::Split => "split",
            Self::None => "none",
        }
    }

    fn parse(spelling: &str) -> Result<Self, ()> {
        match spelling {
            "equal" => Ok(Self::Equal),
            "golden" => Ok(Self::Golden),
            "split" => Ok(Self::Split),
            "none" => Ok(Self::None),
            _ => Err(()),
        }
    }
}

/// One app of a stack.
///
/// `id` is a **desktop entry id** — what niri reports as a window's `app_id`
/// (#1071 §3.2), so the same string identifies the entry to launch and the
/// window to recognise. `exec` overrides the entry's own `Exec`; resolving an
/// entry (and stripping its field codes) is phase 4.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackApp {
    pub id: String,
    pub exec: Option<String>,
}

/// One saved workspace stack.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Stack {
    /// The connector this stack's workspace lives on. `None` = the focused
    /// monitor at Start time (#1071 §3.1). There is no monitor *field* in the
    /// UI — a card is moved by dragging it between the page's monitor columns,
    /// which is what `set_stack_monitor` writes (§5).
    pub monitor: Option<String>,
    /// Start this stack at session start (#1071 §3.5), eagerly and once, after
    /// niri reports its outputs — `workspace_stacks::autostart_plan`.
    pub autostart: bool,
    pub layout: Layout,
    /// In stack order, which **is** niri's column order (#1071 §3.4 step 3).
    pub apps: Vec<StackApp>,
}

/// The file, resolved.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Workspaces {
    /// Card order, top to bottom within a monitor's column. A stack not named
    /// here still shows — after the named ones, by name — so a hand-written
    /// file that forgets to extend `order` does not hide a stack.
    pub order: Vec<String>,
    /// Stacks by name. `BTreeMap` so the "not in `order`" tail is stable.
    pub stacks: BTreeMap<String, Stack>,
}

impl Workspaces {
    /// Stack names in card order: everything `order` names (that exists),
    /// then everything else by name.
    #[must_use]
    pub fn names_in_order(&self) -> Vec<String> {
        // Deduped: `order = ["chat", "chat"]` is an ordinary copy-paste slip,
        // and the page builds one card per name returned — so without this it
        // draws two identical cards, each with its own ▶, and starting one
        // leaves the other showing Inactive.
        let mut seen = BTreeSet::new();
        let mut out: Vec<String> = self
            .order
            .iter()
            .filter(|name| self.stacks.contains_key(*name))
            .filter(|name| seen.insert((*name).clone()))
            .cloned()
            .collect();
        let tail: Vec<String> = self
            .stacks
            .keys()
            .filter(|name| !out.contains(name))
            .cloned()
            .collect();
        out.extend(tail);
        out
    }
}

// ── The knobs ────────────────────────────────────────────────────────────────
//
// `InvalidValue` is only constructible from an `EnvKnob`, which carries the key
// name and the "expected …" half of the one-line diagnostic. This subsystem has
// no migrated `TROLLSHELL_*` variables — it is new, so there is no deprecation
// window to run and `Subsystem::resolve`'s default ("there is no environment to
// layer") is the honest answer — so every `var` below is empty and never read.
//
// The `key` strings are *templates* (`workspace.*.layout`) because a stack's
// keys are per stack and `EnvKnob::key` is `&'static str`. The stack's name goes
// in the value instead, so the line still says which one:
//
//     workspace.*.layout = chat: "gilded" is not valid; expected equal, golden, split or none

const ORDER: EnvKnob = EnvKnob::same("", "order", "an array of workspace names");
const WORKSPACE: EnvKnob = EnvKnob::same("", "workspace", "a table of stacks keyed by name");
const STACK_NAME: EnvKnob = EnvKnob::same(
    "",
    "workspace.*",
    "a name of lowercase letters, digits and single interior dashes",
);
const STACK_KEY: EnvKnob = EnvKnob::same("", "workspace.*", "monitor, autostart, layout or apps");
const MONITOR: EnvKnob = EnvKnob::same("", "workspace.*.monitor", "a connector name");
const AUTOSTART: EnvKnob = EnvKnob::same("", "workspace.*.autostart", "true or false");
const LAYOUT: EnvKnob = EnvKnob::same("", "workspace.*.layout", "equal, golden, split or none");
const APPS: EnvKnob = EnvKnob::same(
    "",
    "workspace.*.apps",
    "an array of { id = \"…\", exec = \"…\" } tables, each with a non-blank id \
     and, if present, a non-blank exec",
);
const APP_KEY: EnvKnob = EnvKnob::same("", "workspace.*.apps[]", "id or exec");

/// The keys a stack table may carry. Anything else is reported, once, and
/// ignored — rule 4's spirit inside a raw value, which `serde_ignored` cannot
/// see into because the whole `workspace` key arrives as a `toml::Value`.
const STACK_KEYS: [&str; 4] = ["monitor", "autostart", "layout", "apps"];

/// The keys one entry of an `apps` array may carry. See [`unknown_app_keys`].
const APP_KEYS: [&str; 2] = ["id", "exec"];

// ── The schema ───────────────────────────────────────────────────────────────

/// `workspaces.toml` as written. See the module doc for why both fields are
/// `Option<toml::Value>`.
// No `Eq`: a `toml::Value` can hold a float.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct WorkspacesConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    order: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace: Option<toml::Value>,
}

impl Subsystem for WorkspacesConfig {
    const NAME: &'static str = "workspaces";

    const DEFAULT_TOML: &'static str = r#"# Saved workspace stacks (#1071).
#
# A *stack* is the apps of one named niri workspace, the screen it lives on, and
# the layout template applied to it. The Workspaces drawer page writes this file
# when you Save a workspace, and reads it live — an edit here shows up without
# restarting the shell.
#
# This file is layered: a home-manager base under $XDG_CONFIG_DIRS, then your own
# overlay in $XDG_CONFIG_HOME/trollshell/workspaces.toml. Tables deep-merge, so a
# base can pin `chat` while this file adds `music`; arrays REPLACE, so stating
# `apps` or `order` here replaces the layer below's entirely. To drop a stack a
# base pinned, name it in an `_unset` array in the table that holds it:
#
#     [workspace]
#     _unset = ["chat"]
#
# A value no parser accepts costs its own key and nothing else: that key takes
# the built-in default, one journal line names it, and every other key — in this
# stack and in every other — still applies. A stack whose *name* cannot be a
# systemd slice is dropped whole, since it could never be started.
#
# The shell ships no stacks, so there is nothing to state below. The shape:
#
#     # Card order, top to bottom within a screen's column. A stack you leave
#     # out still shows, after the ordered ones, by name.
#     order = ["chat", "dev"]
#
#     [workspace.chat]
#     # The connector this stack lives on, as `niri msg outputs` names it.
#     # Leave it out to start on whichever screen is focused.
#     monitor   = "DP-1"
#     # Start this stack at login.
#     autostart = true
#     # Applied once, after every app has launched: equal | golden | split | none
#     layout    = "golden"
#     # In order — which is also the left-to-right column order in niri.
#     # `id` is the app's desktop-entry id, the same string niri reports as a
#     # window's app_id. `exec` overrides the entry's own command.
#     apps = [
#       { id = "org.mozilla.firefox" },
#       { id = "Alacritty", exec = "alacritty -e weechat" },
#     ]
#
# Names are also systemd slice names (`trollshell-ws-<name>.slice`), so they are
# lowercase letters, digits and single interior dashes — no leading, trailing or
# doubled dash, at most 32 characters. The Save field folds case for you and
# refuses anything else rather than rewriting it.
"#;

    type Error = std::convert::Infallible;

    type Resolved = Workspaces;

    /// See [`Self::Error`]: every value is judged per key in [`Self::parsed`],
    /// so there is nothing left for the whole-file gate to reject. A stack that
    /// cannot work is dropped with a line, not made to take the file down.
    fn validate(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn parsed(&self) -> (Workspaces, Vec<InvalidValue>) {
        let mut rejected = Vec::new();
        let order = self.order.as_ref().map_or_else(Vec::new, |value| {
            keep(parse_order(value), Vec::new(), &mut rejected)
        });
        let stacks = self
            .workspace
            .as_ref()
            .map_or_else(BTreeMap::new, |value| parse_stacks(value, &mut rejected));
        (Workspaces { order, stacks }, rejected)
    }
}

/// `order`, as workspace names.
///
/// Whole-key: the order decides card placement and the index a started
/// workspace is moved to (§3.6), nothing more, so a malformed one costs the
/// ordering and nothing else rather than being silently half-applied.
fn parse_order(value: &toml::Value) -> Result<Vec<String>, InvalidValue> {
    let bad = || InvalidValue::of(&ORDER, value);
    let array = value.as_array().ok_or_else(bad)?;
    array
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .and_then(normalize_workspace_name)
                .ok_or_else(bad)
        })
        .collect()
}

/// The `workspace` table, stack by stack.
///
/// Per **stack**, not whole-key: a file with three good stacks and one typo'd
/// name keeps the three. A stack's own keys are then judged per key, so a bad
/// `layout` costs `layout` and the stack still starts.
fn parse_stacks(value: &toml::Value, rejected: &mut Vec<InvalidValue>) -> BTreeMap<String, Stack> {
    let Some(table) = value.as_table() else {
        rejected.push(InvalidValue::of(&WORKSPACE, value));
        return BTreeMap::new();
    };
    let mut stacks = BTreeMap::new();
    for (raw_name, entry) in table {
        let Some(name) = normalize_workspace_name(raw_name) else {
            rejected.push(InvalidValue::written(&STACK_NAME, raw_name));
            continue;
        };
        let Some(fields) = entry.as_table() else {
            rejected.push(InvalidValue::written(
                &STACK_KEY,
                &format!("{name}: {entry}"),
            ));
            continue;
        };
        for unknown in fields.keys().filter(|k| !STACK_KEYS.contains(&k.as_str())) {
            rejected.push(InvalidValue::written(
                &STACK_KEY,
                &format!("{name}.{unknown}"),
            ));
        }
        // Two spellings that normalise to one name are two *table keys* and one
        // stack. `toml::Table` is a `BTreeMap`, so a bare `insert` would parse
        // `Chat` and then silently overwrite it with `chat` — one stack's
        // monitor, layout and apps gone with no line anywhere. First wins, and
        // the loser is named.
        if stacks.contains_key(&name) {
            rejected.push(InvalidValue::written(
                &STACK_NAME,
                &format!("{raw_name} (already declared as {name})"),
            ));
            continue;
        }
        stacks.insert(name.clone(), parse_stack(&name, fields, rejected));
    }
    stacks
}

fn parse_stack(name: &str, fields: &toml::Table, rejected: &mut Vec<InvalidValue>) -> Stack {
    let named = |knob: &EnvKnob, value: &toml::Value| {
        InvalidValue::written(knob, &format!("{name}: {value}"))
    };
    let monitor = fields.get("monitor").and_then(|value| {
        keep(
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| named(&MONITOR, value))
                .map(Some),
            None,
            rejected,
        )
    });
    let autostart = fields.get("autostart").is_some_and(|value| {
        keep(
            value.as_bool().ok_or_else(|| named(&AUTOSTART, value)),
            false,
            rejected,
        )
    });
    let layout = fields.get("layout").map_or(Layout::None, |value| {
        keep(
            value
                .as_str()
                .and_then(|s| Layout::parse(s).ok())
                .ok_or_else(|| named(&LAYOUT, value)),
            Layout::None,
            rejected,
        )
    });
    let apps = fields.get("apps").map_or_else(Vec::new, |value| {
        // Reported, not fatal: the entries still parse, so a typo'd key costs
        // one line and the stack keeps whatever the shell does understand.
        for unknown in unknown_app_keys(value) {
            rejected.push(InvalidValue::written(
                &APP_KEY,
                &format!("{name}.apps[{unknown}]"),
            ));
        }
        keep(
            parse_apps(value).map_err(|()| named(&APPS, value)),
            Vec::new(),
            rejected,
        )
    });
    Stack {
        monitor,
        autostart,
        layout,
        apps,
    }
}

/// `apps`, whole-key: the list is an ordered whole (it *is* niri's column
/// order), so half of it applied in the wrong order would be worse than none.
fn parse_apps(value: &toml::Value) -> Result<Vec<StackApp>, ()> {
    value
        .as_array()
        .ok_or(())?
        .iter()
        .map(|entry| {
            let table = entry.as_table().ok_or(())?;
            let id = table.get("id").and_then(toml::Value::as_str).ok_or(())?;
            if id.trim().is_empty() {
                return Err(());
            }
            let exec = match table.get("exec") {
                None => None,
                Some(value) => {
                    let exec = value.as_str().ok_or(())?;
                    // A blank override is not "no override": it splits to an
                    // empty argv and surfaces as a `systemd-run` error at Start,
                    // hours after the file was written. Refuse it here, where
                    // the line names the key.
                    if exec.trim().is_empty() {
                        return Err(());
                    }
                    Some(exec.to_owned())
                }
            };
            Ok(StackApp {
                id: id.to_owned(),
                exec,
            })
        })
        .collect()
}

/// The unknown keys inside a stack's `apps` entries, as `"<n>.<key>"`.
///
/// Rule 4 one level deeper than [`parse_stacks`] reaches. `workspace` arrives as
/// a single raw `toml::Value`, so `serde_ignored` cannot see into it at all —
/// and `exec` is precisely the key whose whole purpose is changing what runs, so
/// `{ id = "Alacritty", exce = "…" }` loading clean and launching the wrong
/// thing is the worst-shaped silence in the file.
///
/// Separate from [`parse_apps`] because a typo'd key is not a reason to drop the
/// whole array: the entries still parse, the unknown key is reported, and the
/// stack starts with whatever it does understand.
fn unknown_app_keys(value: &toml::Value) -> Vec<String> {
    let Some(array) = value.as_array() else {
        return Vec::new();
    };
    array
        .iter()
        .enumerate()
        .filter_map(|(n, entry)| Some((n, entry.as_table()?)))
        .flat_map(|(n, table)| {
            table
                .keys()
                .filter(|k| !APP_KEYS.contains(&k.as_str()))
                .map(move |k| format!("{n}.{k}"))
        })
        .collect()
}

// ── Saving (the drawer's Edit → Save, #1071 §3.7) ────────────────────────────

/// Add the stack `name` to the **overlay**, keeping everything else in the file
/// as written.
///
/// Reads the overlay's own layers — `DEFAULT_TOML` plus the user's file, *not*
/// the merged base — so a stack a home-manager base pinned is not copied down
/// into the overlay as a side effect of saving a different one. That is the
/// property `a_save_never_materialises_a_base_stack_into_the_overlay` holds.
/// `order` is left exactly as the overlay had it, including absent: writing it
/// would replace a base-pinned order wholesale (arrays replace), and a Save is
/// not a statement about card order. Dragging a card between the page's
/// monitor columns goes through `set_stack_monitor` for the same reason.
///
/// **Refuses an existing name.** A Save is how a workspace gets *created*, and
/// `stack_value` omits every defaulted key, so silently replacing would drop the
/// existing stack's monitor, layout and autostart and replace its apps wholesale
/// (arrays replace) — with no way back. Phase 4's edit form is where an existing
/// stack is changed; until it exists, refusing is the only honest answer.
///
/// # Errors
/// [`ConfigError::Invalid`] for an unusable or already-taken name,
/// [`ConfigError::NoOverlayPath`] when there is no `XDG_CONFIG_HOME` to write
/// to, plus whatever the reader and the format-preserving writer report.
pub fn save_stack(name: &str, stack: &Stack) -> Result<(), ConfigError> {
    let name = normalize_workspace_name(name)
        .ok_or_else(|| ConfigError::Invalid(format!("invalid workspace name: {name:?}")))?;
    // Against the **merged** view, not the overlay: a base-pinned name is taken
    // too, and `save_stack_to` reads only the overlay, so a collision with the
    // base would otherwise be invisible right up until the merge produced a
    // stack the user never described.
    let taken = hytte_config::subsystem::load_or_default::<WorkspacesConfig>()
        .is_some_and(|config| config.parsed().0.stacks.contains_key(&name));
    if taken {
        return Err(ConfigError::Invalid(format!(
            "a workspace called {name:?} already exists"
        )));
    }
    let path = xdg::overlay_path(WorkspacesConfig::NAME).ok_or(ConfigError::NoOverlayPath)?;
    save_stack_to(&path, &name, stack)
}

/// [`save_stack`] against an explicit overlay path, so the round trip is
/// testable without an `XDG_CONFIG_HOME`.
///
/// Does **not** repeat [`save_stack`]'s already-taken check: that one is stated
/// against the merged layers, which an explicit single path cannot see. This is
/// the writer; the guard is the caller's.
///
/// # Errors
/// As [`save_stack`], minus the already-taken case.
pub fn save_stack_to(path: &std::path::Path, name: &str, stack: &Stack) -> Result<(), ConfigError> {
    let existing = hytte_config::subsystem::load_from::<WorkspacesConfig>(&[path.to_path_buf()])?;
    let mut table = existing
        .config
        .workspace
        .as_ref()
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    table.insert(name.to_owned(), stack_value(stack));
    let next = WorkspacesConfig {
        order: existing.config.order.clone(),
        workspace: Some(toml::Value::Table(table)),
    };
    hytte_config::subsystem::save_overlay_to(path, &next)
}

/// Write the Edit sub-page's form back to the overlay (#1071 §5, phase 4).
///
/// This is the writer [`save_stack`] deliberately was not. Phase 2's Save is a
/// *creation* and refuses a name that already exists, because it had no form
/// with which to describe the stack it would otherwise be replacing — its own
/// doc says *"Phase 4's edit form is where an existing stack is changed"*. This
/// is that form's writer, so it replaces by design.
///
/// `previous` is the name the form opened on: `None` for an ephemeral card
/// (§3.7, where Save is what creates the entry) and `Some(old)` for a saved one.
/// When `old` differs from `name` this is a **rename** — see [`rename_within`]
/// for what moves.
///
/// Writing the whole `[workspace.<name>]` table is right here, and is exactly
/// what made it wrong for [`set_stack_monitor`]: a drag says nothing about a
/// stack's apps, so copying a home-manager base's values down into the overlay
/// would freeze them; a Save says something about **every** field, because the
/// user just looked at all of them in the form and pressed Save. What they saw
/// was the merged view, and what they get is that view pinned — which is the
/// only reading of "Save" that does not silently discard an edit.
///
/// # Errors
/// [`ConfigError::Invalid`] for an unusable name, a rename onto a name some
/// other stack already has, or a `workspace` key that is not a table;
/// [`ConfigError::NoOverlayPath`] when there is nowhere to write.
pub fn save_edit(previous: Option<&str>, name: &str, stack: &Stack) -> Result<(), ConfigError> {
    let path = xdg::overlay_path(WorkspacesConfig::NAME).ok_or(ConfigError::NoOverlayPath)?;
    // Everything that needs the **layers** is read here, not in the writer — the
    // same split `save_stack`/`save_stack_to` already established, and for a
    // reason that bit: `save_edit_to` used to run the taken-name guard itself
    // through `load_or_default`, which resolves the *process's* XDG search path
    // rather than the `path` argument. So the guard was unfalsifiable from the
    // tempdir tests (the one named for it asserted a tautology), and two of
    // those tests' behaviour depended on what happened to be in the developer's
    // real `~/.config/trollshell` — a test that can only fail on Annika's box.
    save_edit_to(&path, previous, name, stack, &edit_context(previous))
}

/// What only the layer stack can answer, gathered where reading it is allowed.
#[derive(Clone, Debug, Default)]
pub struct EditContext {
    /// Every stack name the **merged** layers currently hold. A base-pinned name
    /// is taken too, and only the merged view can see that.
    pub taken: BTreeSet<String>,
    /// A layer **below** the overlay defines the name being renamed away from.
    ///
    /// What decides whether the rename writes an `_unset` marker. It has to be
    /// answered here because the writer sees one file: `merge_into` honours the
    /// marker against the layer below, and the overlay cannot tell whether there
    /// *is* one.
    ///
    /// Writing the marker unconditionally would be sound but noisy — with no
    /// base layer at all (the ordinary single-file case) it names a key no layer
    /// has, which is exactly the **inert** marker #1008 added a warning for. So
    /// a rename in a plain setup writes no marker, and a rename over a base
    /// writes one.
    pub previous_is_inherited: bool,
}

/// [`EditContext`] from the process's XDG layers.
fn edit_context(previous: Option<&str>) -> EditContext {
    let taken: BTreeSet<String> = hytte_config::subsystem::load_or_default::<WorkspacesConfig>()
        .map(|config| config.parsed().0.stacks.into_keys().collect())
        .unwrap_or_default();

    // The layers below the writable one: `config_layers` puts the overlay last.
    let layers = xdg::config_layers(WorkspacesConfig::NAME);
    let base = layers.split_last().map(|(_, base)| base).unwrap_or(&[]);
    let previous_is_inherited = previous.is_some_and(|previous| {
        normalize_workspace_name(previous).is_some_and(|previous| {
            hytte_config::subsystem::load_from::<WorkspacesConfig>(base)
                .is_ok_and(|loaded| loaded.config.parsed().0.stacks.contains_key(&previous))
        })
    });

    EditContext {
        taken,
        previous_is_inherited,
    }
}

/// [`save_edit`] against an explicit overlay path, so the round trip is testable
/// without an `XDG_CONFIG_HOME`.
///
/// # Errors
/// As [`save_edit`].
pub fn save_edit_to(
    path: &std::path::Path,
    previous: Option<&str>,
    name: &str,
    stack: &Stack,
    context: &EditContext,
) -> Result<(), ConfigError> {
    let name = normalize_workspace_name(name)
        .ok_or_else(|| ConfigError::Invalid(format!("invalid workspace name: {name:?}")))?;
    let previous = previous
        .map(|p| {
            normalize_workspace_name(p)
                .ok_or_else(|| ConfigError::Invalid(format!("invalid workspace name: {p:?}")))
        })
        .transpose()?;

    let existing = hytte_config::subsystem::load_from::<WorkspacesConfig>(&[path.to_path_buf()])?;
    let mut table = workspace_table(&existing)?;
    let mut order = existing.config.order.clone();
    let renaming = previous.as_deref().is_some_and(|p| p != name);

    if renaming {
        // A rename onto a name that is already somebody else's would silently
        // eat that stack — `Table::insert` replaces. `taken` is the **merged**
        // view, handed in by the caller (see `save_edit`), because a base-pinned
        // name is taken too and this function must not go looking for one.
        if context.taken.contains(&name) {
            return Err(ConfigError::Invalid(format!(
                "a workspace called {name:?} already exists"
            )));
        }
    }

    if let Some(previous) = previous.as_deref().filter(|_| renaming) {
        table.remove(previous);
        order = rename_within(order.as_ref(), previous, &name);
        // …and, when a layer **below** this one defines it, *say* the old name
        // is gone — which removing it from the overlay does not (review
        // MEDIUM 4). §4 designs for a home-manager base pinning a stack, and
        // merge rule 1 is that absence is inheritance, never erasure: an overlay
        // that merely stops mentioning `chat` inherits the base's `chat` right
        // back, so one rename yields **two** cards. `_unset` is the spelling
        // TOML lacks a null for.
        //
        // Whether there *is* such a layer is the caller's to answer — see
        // `EditContext::previous_is_inherited`, and MEDIUM 3 for why this
        // function does not go looking.
        if context.previous_is_inherited {
            unset_in(&mut table, previous);
        }
    }

    // Whatever else is true, the name being written is **not** unset. Without
    // this, renaming away and back leaves an inert `_unset` marker naming a key
    // the same layer also sets — which #1008 warns about, correctly.
    clear_unset(&mut table, &name);

    table.insert(name, stack_value(stack));
    let next = WorkspacesConfig {
        order,
        workspace: Some(toml::Value::Table(table)),
    };
    hytte_config::subsystem::save_overlay_to(path, &next)
}

/// Add `name` to the `[workspace]` table's `_unset` array, creating it if need
/// be, without disturbing any name already there.
fn unset_in(table: &mut toml::Table, name: &str) {
    let mut names: Vec<toml::Value> = table
        .get(hytte_config::merge::UNSET_KEY)
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !names
        .iter()
        .any(|v| v.as_str().is_some_and(|n| n.eq_ignore_ascii_case(name)))
    {
        names.push(name.into());
    }
    table.insert(
        hytte_config::merge::UNSET_KEY.to_owned(),
        toml::Value::Array(names),
    );
}

/// Drop `name` from the `[workspace]` table's `_unset` array, removing the array
/// entirely once it is empty rather than leaving `_unset = []` behind.
fn clear_unset(table: &mut toml::Table, name: &str) {
    let Some(names) = table
        .get(hytte_config::merge::UNSET_KEY)
        .and_then(toml::Value::as_array)
    else {
        return;
    };
    let kept: Vec<toml::Value> = names
        .iter()
        .filter(|v| !v.as_str().is_some_and(|n| n.eq_ignore_ascii_case(name)))
        .cloned()
        .collect();
    if kept.len() == names.len() {
        return;
    }
    if kept.is_empty() {
        table.remove(hytte_config::merge::UNSET_KEY);
    } else {
        table.insert(
            hytte_config::merge::UNSET_KEY.to_owned(),
            toml::Value::Array(kept),
        );
    }
}

/// Rewrite the card order (#1071 §3.6/§5, phase 4) — the file half of dragging a
/// card up or down inside one monitor's column.
///
/// `names` is the **whole** page's order, every monitor's cards together, in the
/// order the drag left them. It has to be: `order` is one flat array across
/// every screen (§4), so writing only the dragged column's names would drop
/// every other column's.
///
/// # Errors
/// [`ConfigError::NoOverlayPath`] when there is nowhere to write, plus whatever
/// the reader and the format-preserving writer report.
pub fn set_order(names: &[String]) -> Result<(), ConfigError> {
    let path = xdg::overlay_path(WorkspacesConfig::NAME).ok_or(ConfigError::NoOverlayPath)?;
    set_order_to(&path, names)
}

/// [`set_order`] against an explicit overlay path, so the round trip is testable
/// without an `XDG_CONFIG_HOME`.
///
/// Touches `order` and nothing else — not the `[workspace.*]` tables, not a
/// comment, not a byte of whitespace anywhere else — because it goes through the
/// same `toml_edit` patch [`set_stack_monitor_to`] does.
///
/// # Errors
/// Whatever the reader and the format-preserving writer report.
pub fn set_order_to(path: &std::path::Path, names: &[String]) -> Result<(), ConfigError> {
    let existing = hytte_config::subsystem::load_from::<WorkspacesConfig>(&[path.to_path_buf()])?;
    let order = toml::Value::Array(names.iter().map(|n| n.clone().into()).collect());
    let next = WorkspacesConfig {
        order: Some(order),
        workspace: existing.config.workspace.clone(),
    };
    hytte_config::subsystem::save_overlay_to(path, &next)
}

/// The overlay's `[workspace]` table, or an error if it is something else.
///
/// Shared by the two writers that rewrite inside it. **Present but not a table**
/// is a hand-edit slip (`workspace = "chat"`), and an `unwrap_or_default()`
/// there hands back a fresh empty table and quietly overwrites whatever the user
/// wrote (#1106 review LOW 8).
fn workspace_table(
    existing: &hytte_config::subsystem::Loaded<WorkspacesConfig>,
) -> Result<toml::Table, ConfigError> {
    match existing.config.workspace.as_ref() {
        None => Ok(toml::Table::new()),
        Some(toml::Value::Table(table)) => Ok(table.clone()),
        Some(other) => Err(ConfigError::Invalid(format!(
            "workspace is {other}, not a table of stacks; not rewriting it"
        ))),
    }
}

/// `order` with `from` replaced by `to`, **in place**.
///
/// In place rather than removed-and-appended: a rename is not a reordering, and
/// a renamed card that jumped to the bottom of its column would be a second,
/// invisible edit the user did not make.
///
/// Pure, and stated over the raw `toml::Value` because that is what the overlay
/// holds. Two shapes pass through untouched, and both matter: an **absent**
/// `order` stays absent — writing one would replace a base-pinned card order
/// wholesale, since arrays replace (`merge.rs`) — and a **non-array** `order` (a
/// hand-edit slip) is returned as it was, so this writer never destroys a value
/// it cannot read.
fn rename_within(order: Option<&toml::Value>, from: &str, to: &str) -> Option<toml::Value> {
    let Some(toml::Value::Array(items)) = order else {
        return order.cloned();
    };
    Some(toml::Value::Array(
        items
            .iter()
            .map(|item| match item.as_str() {
                Some(name) if name.eq_ignore_ascii_case(from) => to.into(),
                _ => item.clone(),
            })
            .collect(),
    ))
}

/// Record that the stack `name` lives on the connector `monitor` — the file
/// half of #1071 §5's drag between monitor columns (phase 3).
///
/// # Errors
/// [`ConfigError::NoOverlayPath`] when there is nowhere to write, plus whatever
/// [`set_stack_monitor_to`] returns.
pub fn set_stack_monitor(name: &str, monitor: &str) -> Result<(), ConfigError> {
    let path = xdg::overlay_path(WorkspacesConfig::NAME).ok_or(ConfigError::NoOverlayPath)?;
    set_stack_monitor_to(&path, name, monitor)
}

/// [`set_stack_monitor`] against an explicit overlay path, so the round trip is
/// testable without an `XDG_CONFIG_HOME`.
///
/// # Why this is not [`save_stack_to`] with a changed `monitor`
///
/// Two reasons, and both are about *not writing keys nobody asked about*.
///
/// * `save_stack_to` writes [`stack_value`] — the **whole** stack. Handed the
///   merged view of a stack a home-manager base pinned, it would copy that
///   base's `apps`, `layout` and `autostart` down into the overlay, where they
///   stop tracking the base forever. Dragging a card is not a statement about
///   any of those.
/// * `save_stack_to` is also what a *Save* calls, and a Save is a creation: it
///   may refuse an existing name (its caller does). A drag is the opposite —
///   it only ever changes a stack that already exists, including one that
///   exists only in the base, which is why this happily creates the overlay's
///   `[workspace.<name>]` table when there is none.
///
/// Everything else in the file — comments, key order, the other stacks, this
/// stack's own other keys — is left byte for byte as it was, because the write
/// goes through the same `toml_edit` patch `save_overlay_to` uses.
///
/// # Errors
/// [`ConfigError::Invalid`] for a name that is not a usable workspace name or
/// for an existing `[workspace.<name>]` entry that is not a table, plus
/// whatever the reader and the format-preserving writer report.
pub fn set_stack_monitor_to(
    path: &std::path::Path,
    name: &str,
    monitor: &str,
) -> Result<(), ConfigError> {
    let name = normalize_workspace_name(name)
        .ok_or_else(|| ConfigError::Invalid(format!("invalid workspace name: {name:?}")))?;
    let existing = hytte_config::subsystem::load_from::<WorkspacesConfig>(&[path.to_path_buf()])?;
    // Absent is fine — that is a first save. **Present but not a table** is a
    // hand-edit slip (`workspace = "chat"`), and an `unwrap_or_default()` there
    // would hand back a fresh empty table and quietly overwrite whatever the
    // user wrote. The per-stack guard below refuses exactly this shape one
    // level down; the outer one has to as well, or the careful guard only
    // covers the case the careless one has already destroyed (#1106 review
    // LOW 8). Shared with `save_edit_to`, which needs the same answer.
    let mut table = workspace_table(&existing)?;
    // A stack the overlay has never mentioned gets a fresh table with one key;
    // a stack it has gets that one key changed and keeps the rest.
    let entry = table
        .entry(name.clone())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let fields = entry.as_table_mut().ok_or_else(|| {
        ConfigError::Invalid(format!("workspace.{name} is not a table; not rewriting it"))
    })?;
    fields.insert("monitor".to_owned(), monitor.into());

    let next = WorkspacesConfig {
        order: existing.config.order.clone(),
        workspace: Some(toml::Value::Table(table)),
    };
    hytte_config::subsystem::save_overlay_to(path, &next)
}

/// One stack as the TOML it is written as.
///
/// Only the keys that carry information: a stack with no monitor, no autostart
/// and no layout writes three fewer lines than one with all three, which is
/// what keeps phase 2's Save — a name and the live apps, nothing else (#1071
/// §3.7) — from filling the file with defaults the user never chose.
fn stack_value(stack: &Stack) -> toml::Value {
    let mut table = toml::Table::new();
    if let Some(monitor) = &stack.monitor {
        table.insert("monitor".to_owned(), monitor.clone().into());
    }
    if stack.autostart {
        table.insert("autostart".to_owned(), true.into());
    }
    if stack.layout != Layout::None {
        table.insert("layout".to_owned(), stack.layout.name().into());
    }
    if !stack.apps.is_empty() {
        let apps: Vec<toml::Value> = stack
            .apps
            .iter()
            .map(|app| {
                let mut entry = toml::Table::new();
                entry.insert("id".to_owned(), app.id.clone().into());
                if let Some(exec) = &app.exec {
                    entry.insert("exec".to_owned(), exec.clone().into());
                }
                toml::Value::Table(entry)
            })
            .collect();
        table.insert("apps".to_owned(), toml::Value::Array(apps));
    }
    toml::Value::Table(table)
}

// ── The service ──────────────────────────────────────────────────────────────

/// The handle the Workspaces page subscribes to.
pub struct WorkspacesHandles {
    stacks: Mutable<Workspaces>,
}

pub struct WorkspacesService {
    /// Layer paths, lowest precedence first.
    paths: Vec<PathBuf>,
    /// How the (empty) environment layer is read — injected rather than read
    /// from the process because `unsafe_code = "forbid"` rules out
    /// `std::env::set_var`, so a test could not drive the real environment.
    lookup: EnvLookup,
}

impl Service for WorkspacesService {
    type Handles = WorkspacesHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let (resolved, watcher) = watch::boot::<WorkspacesConfig>(&self.paths, &*self.lookup);
        let stacks = Mutable::new(resolved);
        let Self { lookup, .. } = self;
        let cadence = watch::constant(watch::POLL_INTERVAL);
        spawn_supervised("workspaces-config", {
            let stacks = stacks.clone();
            move || {
                watch::poll_loop(
                    stacks.clone(),
                    watcher.clone(),
                    lookup.clone(),
                    cadence.clone(),
                )
            }
        });
        WorkspacesHandles { stacks }
    }
}

#[must_use]
pub fn service() -> WorkspacesService {
    WorkspacesService {
        paths: xdg::config_layers(WorkspacesConfig::NAME),
        lookup: std::sync::Arc::new(process_env),
    }
}

/// Signal of the saved stacks, re-firing on every live reload that actually
/// changes something — including the drawer's own Save, which the poller picks
/// up the same way it picks up a hand edit (#1040's pilot shape; no new D-Bus
/// surface, and the file stays the single source of truth).
pub fn signal() -> impl Signal<Item = Workspaces> {
    registry::with(|r| {
        r.get::<WorkspacesHandles>()
            .expect("config::workspaces::service() not registered")
            .stacks
            .signal_cloned()
    })
}

/// The stacks as of right now, not as a signal.
///
/// For a click handler, which needs one answer rather than a subscription — and
/// needs it *at click time*: a card carries only a name, so the stack is read
/// back here rather than captured when the card was built. The file is
/// live-reloaded and the drawer can sit open across an edit, so a captured
/// `Stack` could launch a stale app list.
///
/// GTK thread only, like every other registry accessor.
#[must_use]
pub fn current() -> Workspaces {
    registry::with(|r| {
        r.get::<WorkspacesHandles>()
            .expect("config::workspaces::service() not registered")
            .stacks
            .get_cloned()
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ConfigError, EditContext, Layout, Stack, StackApp, Workspaces, WorkspacesConfig,
        save_edit_to, save_stack_to, set_order_to, set_stack_monitor_to, stack_value,
    };
    use hytte_config::subsystem::{self, Subsystem};
    use hytte_config::test_support::capture;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn layers(bodies: &[(&str, &str)]) -> Vec<(PathBuf, String)> {
        bodies
            .iter()
            .map(|(path, body)| (PathBuf::from(path), (*body).to_owned()))
            .collect()
    }

    fn load(bodies: &[(&str, &str)]) -> Workspaces {
        subsystem::assemble::<WorkspacesConfig>(&layers(bodies))
            .expect("assembles")
            .config
            .parsed()
            .0
    }

    fn rejections(bodies: &[(&str, &str)]) -> Vec<String> {
        subsystem::assemble::<WorkspacesConfig>(&layers(bodies))
            .expect("assembles")
            .config
            .parsed()
            .1
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    const CHAT: &str = r#"
[workspace.chat]
monitor = "DP-1"
autostart = true
layout = "golden"
apps = [
  { id = "org.mozilla.firefox" },
  { id = "Alacritty", exec = "alacritty -e weechat" },
]
"#;

    /// The pin the whole subsystem rests on: what the documented file says and
    /// what the serde fallback produces are the same config.
    #[test]
    fn the_documented_default_is_the_built_in_one() {
        let loaded = subsystem::assemble::<WorkspacesConfig>(&[]).expect("DEFAULT_TOML assembles");
        assert_eq!(loaded.config, WorkspacesConfig::default());
        assert_eq!(
            loaded.config.parsed().0,
            Workspaces::default(),
            "the shell ships no stacks"
        );
        assert!(loaded.unknown_keys.is_empty(), "{:?}", loaded.unknown_keys);
        assert!(loaded.config.parsed().1.is_empty());
    }

    /// §7 row 1a: a base stack the overlay never mentions survives, because
    /// tables deep-merge (#868 rule 2).
    ///
    /// Falsified by making `workspace` an array of stacks — arrays replace, so
    /// `music` would take `chat` with it.
    #[test]
    fn an_overlay_adds_a_stack_without_dropping_the_bases() {
        let ws = load(&[
            ("/base.toml", CHAT),
            ("/overlay.toml", "[workspace.music]\nlayout = \"equal\"\n"),
        ]);
        assert_eq!(
            ws.stacks.keys().collect::<Vec<_>>(),
            ["chat", "music"],
            "the base's stack is still here"
        );
        assert_eq!(ws.stacks["chat"].layout, Layout::Golden);
        assert_eq!(ws.stacks["music"].layout, Layout::Equal);
    }

    /// …and a key of a base stack the overlay does restate wins, key by key,
    /// without disturbing that stack's other keys.
    #[test]
    fn an_overlay_changes_one_key_of_a_base_stack() {
        let ws = load(&[
            ("/base.toml", CHAT),
            ("/overlay.toml", "[workspace.chat]\nlayout = \"split\"\n"),
        ]);
        let chat = &ws.stacks["chat"];
        assert_eq!(chat.layout, Layout::Split, "the overlay wins");
        assert_eq!(
            chat.monitor.as_deref(),
            Some("DP-1"),
            "a key the overlay is silent about falls through"
        );
        assert_eq!(chat.apps.len(), 2, "…including the apps array");
    }

    /// §7 row 1b: `apps` and `order` **replace whole** (#868 rule 3), rather
    /// than appending or zipping element-wise.
    ///
    /// Falsified by a merge that appended: `chat` would keep three apps and the
    /// order would keep a tail.
    #[test]
    fn apps_and_order_replace_rather_than_merging() {
        let ws = load(&[
            (
                "/base.toml",
                "order = [\"chat\", \"dev\", \"music\"]\n[workspace.chat]\napps = [{ id = \"a\" }, { id = \"b\" }]\n",
            ),
            (
                "/overlay.toml",
                "order = [\"music\"]\n[workspace.chat]\napps = [{ id = \"c\" }]\n",
            ),
        ]);
        assert_eq!(
            ws.stacks["chat"].apps,
            [StackApp {
                id: "c".to_owned(),
                exec: None
            }],
            "the overlay's apps are the whole answer, with no tail from the base"
        );
        assert_eq!(ws.order, ["music"], "and so is the overlay's order");
    }

    /// §7 row 1c: `_unset` removes a base-pinned stack.
    ///
    /// Spelt in the table that *holds* the stack (`[workspace]`), since `_unset`
    /// is honoured per table — which is the part of #868's rule 1 a reader is
    /// most likely to get wrong, and why this asserts the sibling-keeps case
    /// too.
    #[test]
    fn unset_removes_a_base_stack_and_leaves_its_siblings() {
        let base = format!("{CHAT}\n[workspace.dev]\nlayout = \"equal\"\n");
        let ws = load(&[
            ("/base.toml", &base),
            ("/overlay.toml", "[workspace]\n_unset = [\"chat\"]\n"),
        ]);
        assert_eq!(
            ws.stacks.keys().collect::<Vec<_>>(),
            ["dev"],
            "chat is gone, dev is not"
        );
    }

    /// §7 row 1d: an unknown key warns and changes nothing — at the top level,
    /// where `serde_ignored` sees it…
    #[test]
    fn an_unknown_top_level_key_warns_and_does_not_break_the_load() {
        let loaded = subsystem::assemble::<WorkspacesConfig>(&layers(&[(
            "/o.toml",
            "oder = [\"chat\"]\n[workspace.chat]\nlayout = \"equal\"\n",
        )]))
        .expect("assembles");
        assert_eq!(loaded.unknown_keys, ["oder"]);
        let ws = loaded.config.parsed().0;
        assert!(ws.order.is_empty(), "the typo did nothing");
        assert_eq!(ws.stacks["chat"].layout, Layout::Equal, "…to anything else");
    }

    /// …and *inside* a stack, where it does not.
    ///
    /// `workspace` arrives as one raw `toml::Value`, so `serde_ignored` cannot
    /// look in. Without this the file's deepest and most likely typo — a
    /// misspelled stack key — would be silently ignored with nothing logged.
    #[test]
    fn an_unknown_key_inside_a_stack_is_reported_too() {
        let lines = rejections(&[(
            "/o.toml",
            "[workspace.chat]\nlayuot = \"golden\"\nautostart = true\n",
        )]);
        assert_eq!(
            lines,
            ["workspace.* = chat.layuot is not valid; expected monitor, autostart, layout or apps"]
        );
        let ws = load(&[(
            "/o.toml",
            "[workspace.chat]\nlayuot = \"golden\"\nautostart = true\n",
        )]);
        assert!(ws.stacks["chat"].autostart, "the good key still applies");
        assert_eq!(ws.stacks["chat"].layout, Layout::None);
    }

    /// A bad value costs its own key, and only its own key (#1040 V1) — stated
    /// across *stacks* as well as within one, which is the shape this schema
    /// adds over the pilot's four scalars.
    #[test]
    fn a_bad_value_costs_its_key_not_the_stack_and_not_the_file() {
        let body = "[workspace.chat]\nlayout = \"gilded\"\nmonitor = \"DP-1\"\n\
                    [workspace.dev]\nlayout = \"split\"\n";
        let ws = load(&[("/o.toml", body)]);
        assert_eq!(ws.stacks["chat"].layout, Layout::None, "only this key fell");
        assert_eq!(
            ws.stacks["chat"].monitor.as_deref(),
            Some("DP-1"),
            "its sibling applies"
        );
        assert_eq!(
            ws.stacks["dev"].layout,
            Layout::Split,
            "and the other stack is untouched"
        );
        assert_eq!(
            rejections(&[("/o.toml", body)]),
            ["workspace.*.layout = chat: \"gilded\" is not valid; \
                 expected equal, golden, split or none"],
            "one line, naming the stack and the key"
        );
    }

    /// A stack whose **name** cannot be a systemd slice is dropped whole rather
    /// than costing a key: there is no unit template it could ever start under.
    /// Its siblings survive.
    #[test]
    fn a_stack_with_an_unusable_name_is_dropped_and_its_siblings_are_not() {
        let body = "[workspace.\"chat--dev\"]\nlayout = \"equal\"\n\
                    [workspace.ok]\nlayout = \"split\"\n";
        let ws = load(&[("/o.toml", body)]);
        assert_eq!(ws.stacks.keys().collect::<Vec<_>>(), ["ok"]);
        assert_eq!(
            rejections(&[("/o.toml", body)]),
            [
                "workspace.* = chat--dev is not valid; expected a name of lowercase \
                 letters, digits and single interior dashes"
            ]
        );
    }

    /// A name that only differs by case is folded, not refused — niri matches
    /// workspace names case-insensitively, so `Chat` and `chat` are one
    /// workspace and must be one stack.
    #[test]
    fn a_stack_name_is_folded_to_the_spelling_niri_would_match() {
        let ws = load(&[("/o.toml", "[workspace.Chat]\nlayout = \"equal\"\n")]);
        assert_eq!(ws.stacks.keys().collect::<Vec<_>>(), ["chat"]);
        assert_eq!(ws.order, Vec::<String>::new());

        let ordered = load(&[("/o.toml", "order = [\"Chat\"]\n[workspace.chat]\n")]);
        assert_eq!(ordered.order, ["chat"], "order is folded the same way");
    }

    /// Card order: everything `order` names, then everything else by name — so
    /// a hand-written file that forgets to extend `order` never hides a stack.
    #[test]
    fn names_in_order_puts_the_unordered_tail_last() {
        let ws = load(&[(
            "/o.toml",
            "order = [\"music\", \"gone\", \"chat\"]\n\
             [workspace.chat]\n[workspace.music]\n[workspace.zzz]\n[workspace.aaa]\n",
        )]);
        assert_eq!(
            ws.names_in_order(),
            ["music", "chat", "aaa", "zzz"],
            "ordered first (a name for no stack is skipped), then the rest by name"
        );
    }

    /// §7 row 1e, half one: saving the documented defaults back over the
    /// documented file is a **byte no-op**.
    #[test]
    fn saving_the_defaults_over_the_documented_file_changes_nothing() {
        let body =
            subsystem::render_overlay(WorkspacesConfig::DEFAULT_TOML, &WorkspacesConfig::default())
                .expect("renders");
        assert_eq!(body, WorkspacesConfig::DEFAULT_TOML);
    }

    /// §7 row 1e, half two — and the half that matters for the drawer: saving a
    /// **real** file back unchanged is byte-identical, comments and all.
    ///
    /// The stack table is the interesting part: a writer that re-serialised the
    /// `workspace` value wholesale would reflow every stack and drop the user's
    /// comments, which is precisely the churn the format-preserving writer
    /// exists to avoid.
    #[test]
    fn a_no_change_save_is_byte_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        let body = "# my stacks\n\
                    order = [\"chat\"]\n\
                    \n\
                    # the one I actually use\n\
                    [workspace.chat]\n\
                    monitor = \"DP-1\"\n\
                    layout = \"golden\"\n\
                    apps = [{ id = \"Alacritty\" }]\n";
        std::fs::write(&path, body).expect("writes");

        // Read the stack back out and save exactly it — the "Save with nothing
        // changed" the drawer's Edit → Save performs on an untouched form.
        let loaded =
            subsystem::load_from::<WorkspacesConfig>(std::slice::from_ref(&path)).expect("loads");
        let stack = loaded.config.parsed().0.stacks["chat"].clone();
        save_stack_to(&path, "chat", &stack).expect("saves");

        assert_eq!(
            std::fs::read_to_string(&path).expect("reads back"),
            body,
            "a no-change save must not touch a byte"
        );
    }

    /// A Save adds its stack and leaves everything else in the file alone —
    /// including `order`, which it must not invent.
    ///
    /// Writing `order` would replace a base-pinned one wholesale (arrays
    /// replace), and a Save is not a statement about card order. Falsified by
    /// dropping the `Option` from the schema: a defaulted `order` serialises on
    /// every save.
    #[test]
    fn a_save_adds_its_stack_and_invents_no_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        std::fs::write(&path, "# mine\n[workspace.chat]\nlayout = \"golden\"\n").expect("writes");

        save_stack_to(
            &path,
            "dev",
            &Stack {
                apps: vec![StackApp {
                    id: "Alacritty".to_owned(),
                    exec: None,
                }],
                ..Stack::default()
            },
        )
        .expect("saves");

        let body = std::fs::read_to_string(&path).expect("reads back");
        assert!(
            body.contains("# mine"),
            "the user's comment survives: {body}"
        );
        assert!(body.contains("[workspace.chat]"), "{body}");
        assert!(
            !body.contains("order"),
            "a Save must not invent an order key: {body}"
        );

        let ws = subsystem::load_from::<WorkspacesConfig>(&[path])
            .expect("reloads")
            .config
            .parsed()
            .0;
        assert_eq!(ws.stacks["chat"].layout, Layout::Golden, "untouched");
        assert_eq!(ws.stacks["dev"].apps.len(), 1, "and the new one is there");
        assert!(ws.order.is_empty());
    }

    /// A saved stack writes only the keys that carry information, so phase 2's
    /// Save — a name and the live apps (#1071 §3.7) — does not fill the file
    /// with defaults the user never chose.
    #[test]
    fn a_default_stack_writes_no_keys_but_its_apps() {
        let bare = stack_value(&Stack::default());
        assert_eq!(bare.as_table().expect("a table").len(), 0);

        let full = stack_value(&Stack {
            monitor: Some("DP-1".to_owned()),
            autostart: true,
            layout: Layout::Split,
            apps: vec![StackApp {
                id: "a".to_owned(),
                exec: Some("a --now".to_owned()),
            }],
        });
        let table = full.as_table().expect("a table");
        assert_eq!(table.len(), 4);
        assert_eq!(table["layout"].as_str(), Some("split"));
        assert_eq!(
            table["apps"].as_array().expect("an array")[0]["exec"].as_str(),
            Some("a --now")
        );
    }

    /// The per-key lines really reach the journal, once — they are emitted by
    /// `load_layer`, not by `parsed`, so a `validate` that consulted `parsed`
    /// could not double them.
    #[test]
    fn a_rejected_key_is_logged_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        std::fs::write(&path, "[workspace.chat]\nlayout = \"gilded\"\n").expect("writes");

        let (captured, _guard) = capture();
        let ws = subsystem::initial_load::<WorkspacesConfig>(&[path]);

        assert_eq!(ws.stacks["chat"].layout, Layout::None);
        assert_eq!(
            captured.warnings(),
            ["workspace.*.layout = chat: \"gilded\" is not valid; \
                 expected equal, golden, split or none — ignoring this key and \
                 using the built-in default"]
        );
        assert!(captured.errors().is_empty(), "the file was usable");
    }

    /// **MEDIUM-4.** Two table keys that normalise to one name are a collision,
    /// not a silent overwrite.
    ///
    /// `toml::Table` is a `BTreeMap`, so `Chat` parses first and a bare
    /// `insert` then replaces it with `chat` — one stack's monitor, layout and
    /// apps gone with nothing logged. First wins, and the loser is named.
    ///
    /// **Mutation:** drop the `contains_key` guard → the second assertion reds
    /// (no line) and the first reds too (`chat` takes the later value).
    #[test]
    fn two_spellings_of_one_name_collide_rather_than_overwrite() {
        let body = "[workspace.Chat]\nlayout = \"equal\"\n\
                    [workspace.chat]\nlayout = \"split\"\n";
        let ws = load(&[("/o.toml", body)]);
        assert_eq!(ws.stacks.keys().collect::<Vec<_>>(), ["chat"]);
        assert_eq!(
            ws.stacks["chat"].layout,
            Layout::Equal,
            "the first spelling wins, rather than being silently replaced"
        );
        assert_eq!(
            rejections(&[("/o.toml", body)]),
            [
                "workspace.* = chat (already declared as chat) is not valid; expected a name \
                 of lowercase letters, digits and single interior dashes"
            ],
            "and the loser is named"
        );
    }

    /// **MEDIUM-6.** A duplicated `order` entry draws one card, not two.
    ///
    /// The page builds one card per name `names_in_order` returns, so a
    /// copy-paste slip in `order` used to give two identical cards, each with
    /// its own ▶.
    ///
    /// **Mutation:** drop the `seen.insert` filter → reds.
    #[test]
    fn a_duplicated_order_entry_does_not_duplicate_the_card() {
        let ws = load(&[(
            "/o.toml",
            "order = [\"chat\", \"chat\", \"dev\"]\n[workspace.chat]\n[workspace.dev]\n",
        )]);
        assert_eq!(ws.names_in_order(), ["chat", "dev"]);
    }

    /// **LOW.** An unknown key inside an `apps` entry is reported.
    ///
    /// Rule 4 one level deeper than the stack table: `workspace` arrives as one
    /// raw value, so `serde_ignored` cannot see in at all — and `exec` is the
    /// key whose whole purpose is changing what runs, so `exce` loading clean
    /// and launching the wrong thing is the worst-shaped silence in the file.
    ///
    /// Reported, **not** fatal: the entries still parse and the stack keeps
    /// what the shell does understand.
    #[test]
    fn an_unknown_key_inside_an_apps_entry_is_reported() {
        let body = "[workspace.chat]\n\
                    apps = [{ id = \"Alacritty\", exce = \"alacritty -e weechat\" }]\n";
        assert_eq!(
            rejections(&[("/o.toml", body)]),
            ["workspace.*.apps[] = chat.apps[0.exce] is not valid; expected id or exec"]
        );
        let ws = load(&[("/o.toml", body)]);
        assert_eq!(
            ws.stacks["chat"].apps,
            [StackApp {
                id: "Alacritty".to_owned(),
                exec: None
            }],
            "the entry still loads — it is the override that was lost, and said so"
        );
    }

    /// **LOW.** A blank `exec` or `id` is refused at load, where the line names
    /// the key, rather than becoming an empty argv and surfacing as a
    /// `systemd-run` failure at Start.
    #[test]
    fn a_blank_id_or_exec_is_refused_at_load() {
        for body in [
            "[workspace.chat]\napps = [{ id = \"Alacritty\", exec = \"   \" }]\n",
            "[workspace.chat]\napps = [{ id = \"  \" }]\n",
        ] {
            let ws = load(&[("/o.toml", body)]);
            assert!(
                ws.stacks["chat"].apps.is_empty(),
                "{body:?} should not produce a launchable app"
            );
            assert_eq!(
                rejections(&[("/o.toml", body)]).len(),
                1,
                "exactly one line, for the apps key: {body:?}"
            );
        }
    }

    /// **MEDIUM-5.** A Save refuses a name that is already a stack rather than
    /// replacing it.
    ///
    /// `stack_value` omits every defaulted key and arrays replace whole, so an
    /// unconditional insert would drop the existing stack's monitor, layout and
    /// autostart *and* replace its apps — with no way back. Phase 4's edit form
    /// is where an existing stack is changed.
    ///
    /// Stated on `save_stack_to`'s caller-side guard through the page's own
    /// check and on `save_stack`'s merged-layer check; here it is the writer's
    /// behaviour that is pinned — it still *writes*, because the guard is
    /// deliberately the caller's (see its doc).
    #[test]
    fn the_writer_is_unguarded_and_the_guard_is_stated_where_the_layers_are() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        std::fs::write(&path, "[workspace.chat]\nlayout = \"golden\"\n").expect("writes");

        // The writer replaces, by design — it cannot see the base layer, so it
        // is not the place to decide.
        save_stack_to(&path, "chat", &Stack::default()).expect("writes");
        let ws = subsystem::load_from::<WorkspacesConfig>(std::slice::from_ref(&path))
            .expect("reloads")
            .config
            .parsed()
            .0;
        assert_eq!(ws.stacks["chat"].layout, Layout::None);
    }

    /// **LOW.** A Save reads only the **overlay**, so a base-pinned stack is
    /// never materialised into it as a side effect of saving a different one.
    ///
    /// **Mutation:** point `save_stack_to`'s `load_from` at both layers → `base`
    /// appears in the written file and this reds. Every other save test uses a
    /// single layer, so nothing else could see it.
    #[test]
    fn a_save_never_materialises_a_base_stack_into_the_overlay() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("base.toml");
        let overlay = dir.path().join("workspaces.toml");
        std::fs::write(&base, "[workspace.base]\nlayout = \"golden\"\n").expect("writes");
        std::fs::write(&overlay, "# mine\n").expect("writes");

        save_stack_to(&overlay, "mine", &Stack::default()).expect("saves");

        let written = std::fs::read_to_string(&overlay).expect("reads back");
        assert!(written.contains("[workspace.mine]"), "{written}");
        assert!(
            !written.contains("base"),
            "the base layer's stack must not be copied into the overlay: {written}"
        );
        // …and the merged view still has both, which is the point.
        let merged = subsystem::load_from::<WorkspacesConfig>(&[base, overlay])
            .expect("merges")
            .config
            .parsed()
            .0;
        assert_eq!(merged.stacks.keys().collect::<Vec<_>>(), ["base", "mine"]);
    }

    // ── #1071 §5: the drag between monitor columns ───────────────────────────

    /// Dragging a card to another screen rewrites **one key** and leaves every
    /// other byte of the file alone — comments, key order, the other stacks.
    ///
    /// Falsified by routing the drag through `save_stack_to` with a modified
    /// `Stack`: `stack_value` re-emits the whole table, so `dev`'s own
    /// `# scratch` comment and `chat`'s hand-written `apps` spelling would both
    /// be reflowed.
    #[test]
    fn a_monitor_rewrite_touches_one_key_and_no_other_byte() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        let body = "# my stacks\n\
                    order = [\"chat\", \"dev\"]\n\
                    \n\
                    # the one I actually use\n\
                    [workspace.chat]\n\
                    monitor = \"DP-1\"\n\
                    layout = \"golden\"\n\
                    apps = [{ id = \"Alacritty\" }]\n\
                    \n\
                    # scratch\n\
                    [workspace.dev]\n\
                    autostart = true\n";
        std::fs::write(&path, body).expect("writes");

        set_stack_monitor_to(&path, "chat", "HDMI-A-1").expect("rewrites");

        assert_eq!(
            std::fs::read_to_string(&path).expect("reads back"),
            body.replace("monitor = \"DP-1\"", "monitor = \"HDMI-A-1\""),
            "one key changed, every other byte as it was"
        );
    }

    /// A stack with no `monitor` yet gains one — and, more to the point, a
    /// stack the **overlay** has never mentioned (it lives in the
    /// home-manager base) gets an overlay table with that one key, rather than
    /// having the base's whole stack copied down into the overlay where it
    /// would stop tracking the base forever.
    #[test]
    fn a_monitor_rewrite_never_materialises_a_base_stack() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("base.toml");
        let overlay = dir.path().join("workspaces.toml");
        std::fs::write(
            &base,
            "[workspace.chat]\nlayout = \"golden\"\napps = [{ id = \"Alacritty\" }]\n",
        )
        .expect("writes base");
        std::fs::write(&overlay, "# mine\n").expect("writes overlay");

        set_stack_monitor_to(&overlay, "chat", "HDMI-A-1").expect("rewrites");

        let written = std::fs::read_to_string(&overlay).expect("reads back");
        assert!(written.contains("[workspace.chat]"), "{written}");
        assert!(written.contains("monitor = \"HDMI-A-1\""), "{written}");
        assert!(
            !written.contains("golden") && !written.contains("Alacritty"),
            "the base's own keys stay in the base: {written}"
        );
        // …and the merge still produces the base's keys plus the new monitor.
        let merged = subsystem::load_from::<WorkspacesConfig>(&[base, overlay])
            .expect("loads")
            .config
            .parsed()
            .0;
        assert_eq!(merged.stacks["chat"].monitor.as_deref(), Some("HDMI-A-1"));
        assert_eq!(merged.stacks["chat"].layout, Layout::Golden);
        assert_eq!(merged.stacks["chat"].apps.len(), 1);
    }

    /// **Review LOW 8.** A `workspace` key that is not a table at all is a
    /// hand-edit slip, and the writer refuses rather than replacing it — the
    /// same treatment the per-stack guard one level down already gave.
    ///
    /// Without this the `unwrap_or_default()` handed back a fresh empty table
    /// and the user's value was gone, which is the one outcome a
    /// format-preserving writer exists to prevent.
    #[test]
    fn a_monitor_rewrite_refuses_a_workspace_key_that_is_not_a_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        let body = "# mine\nworkspace = \"chat\"\n";
        std::fs::write(&path, body).expect("writes");

        let err = set_stack_monitor_to(&path, "chat", "DP-1").expect_err("refuses");
        assert!(err.to_string().contains("not a table"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("reads back"),
            body,
            "the user's value is still there, byte for byte"
        );
    }

    /// A name that could never be a workspace is refused before anything is
    /// written — the same guard `save_stack` has, for the same reason: the
    /// name is also a systemd slice name.
    #[test]
    fn a_monitor_rewrite_refuses_an_unusable_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        std::fs::write(&path, "# mine\n").expect("writes");

        set_stack_monitor_to(&path, "chat--dev", "DP-1").expect_err("a doubled dash is refused");
        assert_eq!(
            std::fs::read_to_string(&path).expect("reads back"),
            "# mine\n",
            "nothing was written"
        );
        // Case is folded rather than refused, the way niri matches names.
        set_stack_monitor_to(&path, "Chat", "DP-1").expect("folds");
        assert!(
            std::fs::read_to_string(&path)
                .expect("reads back")
                .contains("[workspace.chat]"),
            "the folded name is the one written"
        );
    }

    // ── #1071 §5, phase 4: the Edit form's writers ───────────────────────────
    //
    // Every one of these drives an **explicit path** under a `tempfile::tempdir`.
    // The `xdg::overlay_path` wrappers (`save_edit`, `set_order`) are never
    // reached from a test, which is what keeps the suite off the developer's
    // real `~/.config/trollshell` — the same rule phase 2 carved the `Ops` seam
    // for, stated here as a convention because these writers have no seam and
    // need none (one write, no ordering to falsify).

    /// A file with an order and two stacks, for the reorder/rename rows.
    fn two_stacks(path: &std::path::Path) -> &'static str {
        let body = "# my stacks\n\
                    order = [\"chat\", \"dev\"]\n\
                    \n\
                    [workspace.chat]\n\
                    monitor = \"DP-1\"\n\
                    apps = [{ id = \"Alacritty\" }]\n\
                    \n\
                    [workspace.dev]\n\
                    layout = \"golden\"\n\
                    apps = [{ id = \"code\" }]\n";
        std::fs::write(path, body).expect("writes");
        body
    }

    /// The stack a Save writes, read back out of the file.
    fn stack_in(path: &std::path::Path, name: &str) -> Option<Stack> {
        let loaded =
            subsystem::load_from::<WorkspacesConfig>(std::slice::from_ref(&path.to_path_buf()))
                .expect("loads");
        loaded.config.parsed().0.stacks.get(name).cloned()
    }

    /// §5's Save replaces an existing stack — which is exactly what `save_stack`
    /// refuses to do, and why this writer exists.
    #[test]
    fn an_edit_save_replaces_the_stack_it_opened_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        two_stacks(&path);

        let edited = Stack {
            monitor: Some("DP-1".to_owned()),
            autostart: true,
            layout: Layout::Split,
            apps: vec![
                StackApp {
                    id: "Alacritty".to_owned(),
                    exec: Some("alacritty -e weechat".to_owned()),
                },
                StackApp {
                    id: "org.mozilla.firefox".to_owned(),
                    exec: None,
                },
            ],
        };
        save_edit_to(&path, Some("chat"), "chat", &edited, &EditContext::default()).expect("saves");

        assert_eq!(stack_in(&path, "chat").as_ref(), Some(&edited));
        let body = std::fs::read_to_string(&path).expect("reads back");
        assert!(body.contains("# my stacks"), "the comment survives: {body}");
        assert!(
            body.contains("[workspace.dev]") && body.contains("golden"),
            "the other stack is untouched: {body}"
        );
    }

    /// **A no-change Save is byte-identical** (#1071 §7 / the brief).
    ///
    /// The form opens on the merged view and Save writes it back; pressing Save
    /// without touching anything must not produce a diff, or every open-and-look
    /// churns the user's file.
    ///
    /// Falsified by having `save_edit_to` write a defaulted key (`autostart =
    /// false`) — `stack_value` omits every default precisely so this holds.
    #[test]
    fn a_no_change_edit_save_is_byte_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        let body = two_stacks(&path);

        let stack = stack_in(&path, "dev").expect("the stack is there");
        save_edit_to(&path, Some("dev"), "dev", &stack, &EditContext::default()).expect("saves");

        assert_eq!(
            std::fs::read_to_string(&path).expect("reads back"),
            body,
            "a no-change save must not touch a byte"
        );
    }

    /// **Cancel writes nothing** — stated where it can be checked.
    ///
    /// Cancel is the *absence* of a call, so the assertion is that the file is
    /// byte-identical after the form has been opened and the writers have not
    /// been called. That is weak on its own, which is why the GTK test
    /// (`panels::workspace_edit`) drives the actual Cancel button; what this
    /// pins is the other half — that merely *reading* a stack out to seed a
    /// form does not itself rewrite the file.
    ///
    /// Falsified by a `save_edit_to` on the Cancel path: the byte compare reds.
    #[test]
    fn opening_a_form_and_cancelling_leaves_the_file_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        let body = two_stacks(&path);

        // Everything the form's seeding does.
        let seeded = stack_in(&path, "chat").expect("the stack is there");
        drop(seeded);

        assert_eq!(std::fs::read_to_string(&path).expect("reads back"), body);
    }

    /// A rename moves the entry **and** its place in `order`, in place — a
    /// rename is not a reordering.
    #[test]
    fn a_rename_moves_the_entry_and_its_slot_in_the_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        two_stacks(&path);

        let stack = stack_in(&path, "chat").expect("the stack is there");
        save_edit_to(&path, Some("chat"), "talk", &stack, &EditContext::default()).expect("renames");

        let loaded =
            subsystem::load_from::<WorkspacesConfig>(std::slice::from_ref(&path)).expect("loads");
        let parsed = loaded.config.parsed().0;
        assert!(!parsed.stacks.contains_key("chat"), "the old key survived");
        assert_eq!(parsed.stacks.get("talk"), Some(&stack));
        assert_eq!(
            parsed.order,
            ["talk".to_owned(), "dev".to_owned()],
            "the renamed card jumped in the order instead of keeping its slot"
        );
    }

    /// A rename onto a name another stack already has is refused rather than
    /// silently eating that stack (`Table::insert` replaces).
    ///
    /// **Review MEDIUM 3**: this used to assert
    /// `after == before || after.contains("[workspace.dev]")`, which is true on
    /// either branch — so the one thing the test is named for was not pinned at
    /// all. It could not be: `save_edit_to` ran the guard itself through
    /// `load_or_default`, which resolves the **process's** XDG path rather than
    /// the `path` argument, so a tempdir test could neither set the taken set
    /// nor predict it. (It also made two tests here depend on what is in the
    /// developer's real `~/.config/trollshell` — `a_rename_moves_the_entry_…`
    /// renames to `talk`, and would have failed on a machine with a stack of
    /// that name. CI has no such file, so it would only ever break on Annika's
    /// box.)
    ///
    /// The guard is the caller's now — `save_edit` computes the merged set and
    /// hands it in — so this asserts unconditionally.
    ///
    /// **The mutation**: deleting the `taken.contains` check reds it, which it
    /// could not before.
    #[test]
    fn a_rename_onto_a_taken_name_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        let body = two_stacks(&path);

        let stack = stack_in(&path, "chat").expect("the stack is there");
        let taken = EditContext {
            taken: ["dev".to_owned()].into_iter().collect(),
            previous_is_inherited: false,
        };
        let err = save_edit_to(&path, Some("chat"), "dev", &stack, &taken).expect_err("refused");
        assert!(matches!(err, ConfigError::Invalid(_)), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("reads back"),
            body,
            "a refused rename still rewrote the file"
        );

        // …and with the name genuinely free it goes through, so the refusal is
        // the guard rather than the writer simply not working.
        save_edit_to(&path, Some("chat"), "talk", &stack, &EditContext::default()).expect("renames");
        assert!(stack_in(&path, "talk").is_some());
    }

    /// **Review MEDIUM 4**: renaming a stack a **base layer** pins must say the
    /// old name is gone, or one rename yields two cards.
    ///
    /// `save_edit_to` removes the old key from the *overlay*, and merge rule 1
    /// is that absence is inheritance, never erasure (§4 is explicit: removing a
    /// base-pinned stack takes `_unset`). So without the marker the merged view
    /// holds both — `talk` from the overlay and `chat` straight back from the
    /// base, now also dropped out of `order` by `rename_within` and so landing
    /// in `names_in_order`'s unordered tail.
    ///
    /// **The mutation**: deleting the `unset_in` call reds this — the merged
    /// view comes back with two stacks.
    #[test]
    fn renaming_a_base_pinned_stack_unsets_the_old_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("base.toml");
        let overlay = dir.path().join("workspaces.toml");
        // A base a home-manager module might render, and an overlay that so far
        // only carries a `monitor` a drag wrote — the partial-overlay case the
        // review calls out alongside the fully-pinned one.
        std::fs::write(
            &base,
            "order = [\"chat\"]\n\
             [workspace.chat]\n\
             layout = \"golden\"\n\
             apps = [{ id = \"Alacritty\" }]\n",
        )
        .expect("writes");
        std::fs::write(&overlay, "[workspace.chat]\nmonitor = \"DP-1\"\n").expect("writes");

        let merged = |()| {
            hytte_config::subsystem::load_from::<WorkspacesConfig>(&[
                base.clone(),
                overlay.clone(),
            ])
            .expect("loads")
            .config
            .parsed()
            .0
        };
        assert!(merged(()).stacks.contains_key("chat"), "the base pins it");

        let renamed = merged(()).stacks["chat"].clone();
        save_edit_to(
            &overlay,
            Some("chat"),
            "talk",
            &renamed,
            // What `edit_context` answers for this file pair: the base defines
            // `chat`, so the overlay has to say it is gone.
            &EditContext {
                taken: BTreeSet::new(),
                previous_is_inherited: true,
            },
        )
        .expect("renames");

        let after = merged(());
        assert_eq!(
            after.stacks.keys().collect::<Vec<_>>(),
            ["talk"],
            "the base-pinned stack came back, so one rename made two cards"
        );
        assert_eq!(after.stacks["talk"], renamed, "and it kept its contents");
        assert!(
            std::fs::read_to_string(&overlay)
                .expect("reads")
                .contains("_unset"),
            "the overlay must spell the removal; TOML has no null"
        );
    }

    /// The marker names in the overlay's `[workspace]` table, or `None`.
    fn unset_names(path: &std::path::Path) -> Option<Vec<String>> {
        let body = std::fs::read_to_string(path).expect("reads");
        let doc: toml::Table = body.parse().expect("parses");
        Some(
            doc.get("workspace")?
                .as_table()?
                .get(hytte_config::merge::UNSET_KEY)?
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
        )
    }

    /// A rename with **no base layer** writes no marker at all.
    ///
    /// `_unset` naming a key no layer has is precisely the *inert* marker #1008
    /// added a warning for, and the ordinary single-file setup would produce one
    /// on every rename. That is why the decision is the caller's rather than an
    /// unconditional write — see `EditContext::previous_is_inherited`.
    #[test]
    fn a_rename_with_nothing_below_it_writes_no_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        two_stacks(&path);

        let stack = stack_in(&path, "chat").expect("the stack is there");
        save_edit_to(&path, Some("chat"), "talk", &stack, &EditContext::default())
            .expect("renames");

        assert_eq!(
            unset_names(&path),
            None,
            "an inert marker was written: {}",
            std::fs::read_to_string(&path).expect("reads")
        );
        assert!(stack_in(&path, "talk").is_some());
        assert!(stack_in(&path, "chat").is_none());
    }

    /// …and renaming back clears the marker rather than leaving an **inert** one
    /// naming a key this same layer sets (#1008 warns about exactly that).
    #[test]
    fn renaming_back_clears_the_marker_it_wrote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        two_stacks(&path);
        let inherited = EditContext {
            taken: BTreeSet::new(),
            previous_is_inherited: true,
        };

        let stack = stack_in(&path, "chat").expect("the stack is there");
        save_edit_to(&path, Some("chat"), "talk", &stack, &inherited).expect("renames");
        assert_eq!(
            unset_names(&path).as_deref(),
            Some(["chat".to_owned()].as_slice()),
            "the first rename records the removal"
        );

        save_edit_to(&path, Some("talk"), "chat", &stack, &inherited).expect("renames back");
        let body = std::fs::read_to_string(&path).expect("reads");
        let marked = unset_names(&path).unwrap_or_default();
        assert!(
            !marked.iter().any(|n| n == "chat"),
            "the marker still names `chat`, which this same layer now sets — \
             that is the inert case: {body}"
        );
        assert!(
            marked.iter().any(|n| n == "talk"),
            "…and it must now name `talk` instead: {body}"
        );
        assert!(stack_in(&path, "chat").is_some(), "{body}");
        assert!(stack_in(&path, "talk").is_none(), "{body}");
    }

    /// An invalid name never reaches the file.
    #[test]
    fn an_edit_save_refuses_an_unusable_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        let body = two_stacks(&path);

        let err =
            save_edit_to(&path, Some("chat"), "chat--dev", &Stack::default(), &EditContext::default())
                .expect_err("refused");
        assert!(matches!(err, ConfigError::Invalid(_)), "{err}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("reads back"),
            body,
            "a refused name still rewrote the file"
        );
    }

    /// §3.6's drag: `set_order_to` rewrites **`order` and nothing else** — not a
    /// stack table, not a comment, not a byte of anything else.
    ///
    /// Falsified by re-emitting the whole document (the mutation `#1106` used
    /// for `set_stack_monitor_to`, here for the order).
    #[test]
    fn an_order_rewrite_touches_one_key_and_no_other_byte() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        let body = two_stacks(&path);

        set_order_to(&path, &["dev".to_owned(), "chat".to_owned()]).expect("writes");

        let after = std::fs::read_to_string(&path).expect("reads back");
        assert_ne!(after, body, "nothing was written at all");
        assert!(
            after.contains("# my stacks"),
            "the comment survives: {after}"
        );
        assert!(
            after.contains("monitor = \"DP-1\"") && after.contains("layout = \"golden\""),
            "a stack table was rewritten: {after}"
        );

        let loaded =
            subsystem::load_from::<WorkspacesConfig>(std::slice::from_ref(&path)).expect("loads");
        assert_eq!(
            loaded.config.parsed().0.order,
            ["dev".to_owned(), "chat".to_owned()]
        );

        // The diff really is one key: put it back and the bytes return.
        set_order_to(&path, &["chat".to_owned(), "dev".to_owned()]).expect("writes");
        assert_eq!(
            std::fs::read_to_string(&path).expect("reads back"),
            body,
            "the order rewrite is not reversible, so it touched something else"
        );
    }

    /// An ephemeral card's Save creates the entry and still invents no `order`
    /// key — arrays replace whole, so writing one would discard a base-pinned
    /// card order the user never asked to change.
    #[test]
    fn an_ephemeral_save_creates_the_entry_without_inventing_an_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workspaces.toml");
        std::fs::write(&path, "# mine\n[workspace.chat]\nlayout = \"golden\"\n").expect("writes");

        save_edit_to(
            &path,
            None,
            "music",
            &Stack {
                monitor: Some("HDMI-A-1".to_owned()),
                apps: vec![StackApp {
                    id: "spotify".to_owned(),
                    exec: Some("/nix/store/x/bin/spotify".to_owned()),
                }],
                ..Stack::default()
            },
            &EditContext::default(),
        )
        .expect("saves");

        let body = std::fs::read_to_string(&path).expect("reads back");
        assert!(body.contains("# mine"), "{body}");
        assert!(body.contains("[workspace.chat]"), "{body}");
        assert!(
            !body.contains("order"),
            "an ephemeral Save must not invent an order key: {body}"
        );
        let saved = stack_in(&path, "music").expect("created");
        assert_eq!(saved.monitor.as_deref(), Some("HDMI-A-1"));
        assert_eq!(
            saved.apps[0].exec.as_deref(),
            Some("/nix/store/x/bin/spotify"),
            "§3.7's command line for an app with no desktop entry was dropped"
        );
    }
}
