//! The **shell's** config families as data: a name, a documented default and
//! a [`Schema`] each, reachable without linking the shell (#888 P0 §3).
//!
//! # Why this is its own crate
//!
//! `trollshell-control-center` renders one row per config leaf from a
//! [`Schema`] (#888 P1). It cannot link `trollshell` — the shell is a binary,
//! and linking it would drag GTK's whole service layer into a settings app —
//! so the two shell-owned families' `DEFAULT_TOML` and `SCHEMA` consts live
//! here instead, in a GTK-free leaf that depends on nothing but
//! `hytte-config`. The shell re-exports both from their old module paths, so
//! its `impl Subsystem`, its tests and every reader of
//! `CoreLedsConfig::DEFAULT_TOML` are unchanged. This is #640's argument —
//! *two editors of one file must agree byte for byte, so the shared half is a
//! leaf library* — applied one level up, to the description of the file rather
//! than to the file's writer.
//!
//! # Two families, not four
//!
//! The four families #888 P0 writes a schema for are `core-leds`,
//! `workspaces`, `stats` and `agents`. Only the first two are here, and the
//! omission is structural rather than a staging decision: `hytte-plugin-stats`
//! and `hytte-plugin-agents` **link `hytte-config` themselves**, so a crate
//! this one depends on cannot depend back on them without a cycle. Their
//! consts therefore stay beside their own `impl Subsystem`, each with the same
//! `verify` test, and the control center — which already links
//! `hytte-plugin-agents` as a library (#947 P4) and gains
//! `hytte-plugin-stats` the same way — composes them into its own list at the
//! top of the dependency graph, where there is no cycle to have.
//!
//! [`FAMILIES`] is therefore *the shell's* families, and its doc says so
//! rather than pretending to be a registry of everything.

use hytte_config::schema::{Mismatch, Schema};

pub mod core_leds;
pub mod workspaces;

/// One config family, as the form reads it.
///
/// Not `#[non_exhaustive]`: every `Family` in the tree is a `const` struct
/// literal, written here and (for the two plugin families) in the plugin
/// crates, so sealing the literal would seal the feature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Family {
    /// [`hytte_config::subsystem::Subsystem::NAME`] — the config file's stem.
    pub name: &'static str,
    /// What the family's leaves are.
    pub schema: &'static Schema,
    /// [`hytte_config::subsystem::Subsystem::DEFAULT_TOML`] — the documented
    /// default, whose comments are what a form shows as a row's tooltip.
    pub default_toml: &'static str,
}

impl Family {
    /// [`hytte_config::schema::verify`] over this family's own two halves.
    ///
    /// # Errors
    /// As [`hytte_config::schema::verify`].
    pub fn verify(&self) -> Result<(), Vec<Mismatch>> {
        hytte_config::schema::verify(self.schema, self.default_toml)
    }
}

/// The **shell-owned** config families — see the crate docs for why the two
/// plugin-owned ones are not here and cannot be.
pub const FAMILIES: &[&Family] = &[&core_leds::FAMILY, &workspaces::FAMILY];

/// The shell-owned family called `name`, or `None`.
#[must_use]
pub fn family(name: &str) -> Option<&'static Family> {
    FAMILIES.iter().copied().find(|family| family.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every family this crate lists agrees with its own documented default.
    ///
    /// The per-family tests beside each `SCHEMA` are the ones that red with a
    /// useful name; this one is the sweep, so a family added to [`FAMILIES`]
    /// without a test of its own is still checked.
    #[test]
    fn every_listed_family_verifies() {
        for family in FAMILIES {
            family
                .verify()
                .unwrap_or_else(|e| panic!("{} does not match its default: {e:?}", family.name));
        }
    }

    /// A family's three halves name the same thing.
    #[test]
    fn a_familys_name_is_its_schemas_family() {
        for listed in FAMILIES {
            assert_eq!(listed.name, listed.schema.family);
            assert_eq!(family(listed.name).map(|f| f.name), Some(listed.name));
        }
    }

    #[test]
    fn the_two_shell_families_are_listed_and_nothing_else_is() {
        let names: Vec<&str> = FAMILIES.iter().map(|family| family.name).collect();
        assert_eq!(names, ["core-leds", "workspaces"]);
        assert!(
            family("stats").is_none() && family("agents").is_none(),
            "the plugin-owned families cannot be here — see the crate docs"
        );
    }
}
