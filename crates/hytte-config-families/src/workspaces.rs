//! `workspaces.toml` — saved workspace stacks (#1071).
//!
//! The documented default and the schema; the parsing, the editing API and the
//! Workspaces drawer page stay in the shell
//! (`trollshell/src/config/workspaces.rs`), which re-exports both consts from
//! their old paths.
//!
//! # Everything here is a collection, and the default states none of it
//!
//! The shell ships no stacks, so [`DEFAULT_TOML`] is **comments only** — it
//! documents the shape in a commented block rather than stating an example
//! nobody asked for, because `render_overlay`'s stale sweep would then have a
//! key to round-trip and the first save would either keep or delete it.
//! [`hytte_config::schema::verify`] is built for that: a
//! [`Kind::List`]/[`Kind::Map`] field the default does not state is not a
//! mismatch, precisely so a family whose contents are the operator's can
//! document its shape without shipping one (see that module's docs).
//!
//! The consequence is that this schema's two fields are checked for their
//! *shape* and not against any value — there are no values. Both render
//! read-only in v1 (#888 spec §1); editing a map or a list is P2.

use hytte_config::schema::{Field, Kind, Schema};

use crate::Family;

/// The family, for [`crate::FAMILIES`].
pub const FAMILY: Family = Family {
    name: "workspaces",
    schema: &SCHEMA,
    default_toml: DEFAULT_TOML,
};

/// The two leaves of `workspaces.toml`, both collections.
pub const SCHEMA: Schema = Schema {
    family: "workspaces",
    fields: FIELDS,
};

const FIELDS: &[Field] = &[
    Field {
        path: "order",
        kind: Kind::List(&Kind::Text { blank_ok: false }),
        doc: "Card order, top to bottom within a screen's column.",
    },
    Field {
        path: "workspace",
        kind: Kind::Map(STACK_FIELDS),
        doc: "Saved stacks, keyed by workspace name.",
    },
];

/// One `[workspace.<name>]` table — `STACK_KEYS` in the shell, which is the
/// list `parse_stack` reports an unknown key against.
const STACK_FIELDS: &[Field] = &[
    Field {
        path: "monitor",
        kind: Kind::Text { blank_ok: false },
        doc: "The connector this stack lives on; absent means the focused screen.",
    },
    Field {
        path: "autostart",
        kind: Kind::Bool,
        doc: "Start this stack at login.",
    },
    Field {
        path: "layout",
        // `Layout::parse`'s match arms, in `Layout`'s own declaration order.
        kind: Kind::Choice {
            options: &["equal", "golden", "split", "none"],
        },
        doc: "The layout template applied once, after every app has launched.",
    },
    Field {
        path: "apps",
        kind: Kind::List(&APP),
        doc: "The stack's apps, in order — which is niri's column order.",
    },
];

/// One entry of an `apps` array. A [`Kind::Map`] used as a list element
/// describes the element table itself; see [`Kind::List`].
const APP: Kind = Kind::Map(APP_FIELDS);

/// `APP_KEYS` in the shell.
const APP_FIELDS: &[Field] = &[
    Field {
        path: "id",
        kind: Kind::Text { blank_ok: false },
        doc: "The app's desktop-entry id — what niri reports as a window's app_id.",
    },
    Field {
        path: "exec",
        kind: Kind::Text { blank_ok: false },
        doc: "Overrides the desktop entry's own command.",
    },
];

/// The documented default, and the bottom merge layer.
///
/// Comments only, deliberately — see this module's own docs.
pub const DEFAULT_TOML: &str = r#"# Saved workspace stacks (#1071).
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The walker, on the family it describes. See the module docs for what it
    /// can and cannot say about a family whose default states no values.
    #[test]
    fn the_schema_matches_the_documented_default() {
        hytte_config::schema::verify(&SCHEMA, DEFAULT_TOML)
            .expect("workspaces' schema and its documented default must agree");
    }

    /// The commented example in [`DEFAULT_TOML`] is the shape the schema
    /// describes, so **uncommenting it must verify** — which is the only way
    /// to check a collection field's contents against a family that ships
    /// none, and the check the walker's collection exemption would otherwise
    /// leave to P2.
    ///
    /// Red if a `STACK_FIELDS` / `APP_FIELDS` entry is renamed without the
    /// comment block moving with it: the uncommented block grows a key no
    /// field claims.
    #[test]
    fn the_commented_example_in_the_default_verifies_when_uncommented() {
        // The prose is a paragraph; an example is an *indented* block inside
        // it. There are two — the `_unset` one-liner and the full shape — so
        // the extraction starts at the "The shape:" lead-in, or it would
        // splice `order` into the `[workspace]` table the first block opens.
        let example: String = DEFAULT_TOML
            .lines()
            .skip_while(|line| !line.contains("The shape:"))
            .skip(1)
            .filter_map(|line| line.strip_prefix('#'))
            .map(|line| line.strip_prefix(' ').unwrap_or(line))
            .filter(|line| line.starts_with("    ") || line.trim().is_empty())
            .fold(String::new(), |mut out, line| {
                out.push_str(line.trim_start());
                out.push('\n');
                out
            });

        assert!(
            example.contains("[workspace.chat]")
                && example.contains(r#"order = ["chat", "dev"]"#)
                && !example.contains("_unset"),
            "the shape block, and only it, must be what was extracted: {example}"
        );
        hytte_config::schema::verify(&SCHEMA, &example)
            .expect("the documented example is the shape the schema describes");
    }
}
