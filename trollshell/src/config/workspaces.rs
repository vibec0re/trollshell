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

use std::collections::BTreeMap;
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
/// (#1071 §3.4 step 4). Applying it is phase 3; the file carries it from phase
/// 2 so a stack saved now does not need re-editing then.
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
    /// UI — a card is moved by dragging it between columns (phase 3).
    pub monitor: Option<String>,
    /// Start this stack at session start (#1071 §3.5). Honoured in phase 3.
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
        let mut out: Vec<String> = self
            .order
            .iter()
            .filter(|name| self.stacks.contains_key(*name))
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
    "an array of { id = \"…\", exec = \"…\" } tables",
);

/// The keys a stack table may carry. Anything else is reported, once, and
/// ignored — rule 4's spirit inside a raw value, which `serde_ignored` cannot
/// see into because the whole `workspace` key arrives as a `toml::Value`.
const STACK_KEYS: [&str; 4] = ["monitor", "autostart", "layout", "apps"];

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
/// Whole-key: order is cosmetic (phase 3 acts on it), so a malformed one costs
/// the ordering and nothing else rather than being silently half-applied.
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
            if id.is_empty() {
                return Err(());
            }
            let exec = match table.get("exec") {
                None => None,
                Some(value) => Some(value.as_str().ok_or(())?.to_owned()),
            };
            Ok(StackApp {
                id: id.to_owned(),
                exec,
            })
        })
        .collect()
}

// ── Saving (the drawer's Edit → Save, #1071 §3.7) ────────────────────────────

/// Add or replace the stack `name` in the **overlay**, keeping everything else
/// in the file as written.
///
/// Reads the overlay's own layers — `DEFAULT_TOML` plus the user's file, *not*
/// the merged base — so a stack a home-manager base pinned is not copied down
/// into the overlay as a side effect of saving a different one. `order` is left
/// exactly as the overlay had it, including absent: writing it would replace a
/// base-pinned order wholesale (arrays replace), and card order is phase 3's.
///
/// # Errors
/// [`ConfigError::NoOverlayPath`] when there is no `XDG_CONFIG_HOME` to write
/// to, plus whatever the reader and the format-preserving writer report.
pub fn save_stack(name: &str, stack: &Stack) -> Result<(), ConfigError> {
    let name = normalize_workspace_name(name)
        .ok_or_else(|| ConfigError::Invalid(format!("invalid workspace name: {name:?}")))?;
    let path = xdg::overlay_path(WorkspacesConfig::NAME).ok_or(ConfigError::NoOverlayPath)?;
    save_stack_to(&path, &name, stack)
}

/// [`save_stack`] against an explicit overlay path, so the round trip is
/// testable without an `XDG_CONFIG_HOME`.
///
/// # Errors
/// As [`save_stack`].
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

#[cfg(test)]
mod tests {
    use super::{
        Layout, Stack, StackApp, Workspaces, WorkspacesConfig, save_stack_to, stack_value,
    };
    use hytte_config::subsystem::{self, Subsystem};
    use hytte_config::test_support::capture;
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
    /// replace), and card order is phase 3's. Falsified by dropping the
    /// `Option` from the schema: a defaulted `order` serialises on every save.
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
}
