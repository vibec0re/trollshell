//! The **effective** mount, and why this plugin reads a variable the SDK
//! already read.
//!
//! `hytte_plugin::run` resolves `HYTTE_PLUGIN_MOUNT` (#1159) and applies it to
//! the `Register` frame, deliberately without telling the plugin: "the plugin's
//! own code is not consulted and never sees the override" is the feature, not
//! an oversight — a plugin that could branch on its placement would be a plugin
//! whose placement is no longer a pure deployment decision.
//!
//! This plugin needs the answer anyway, for exactly one thing: **which table of
//! `stats.toml` it reads** (`[bar]` versus `[sidebar]`, `config::Family`). That
//! is a plugin-side decision — it is this crate's schema — but it depends on a
//! launch-side fact. So the variable is read here, from this process's own
//! environment, and the manifest's mount is the fallback.
//!
//! Reading it twice is safe in the only way that matters: [`effective`] is
//! **strictly less permissive than the SDK's parser cannot be**, because the
//! SDK has already refused the launch outright for any value it could not
//! parse. By the time anything here runs, `HYTTE_PLUGIN_MOUNT` is either unset
//! or a valid wire mount name — the `None` arm below is unreachable in a live
//! process and exists so this function is total and testable.
//!
//! The variable is spelled as a **literal** here rather than imported: the
//! SDK's `MOUNT_ENV` is private, and importing it would not be better anyway —
//! this is a second reader of a documented contract (`docs/plugin-env.md`), the
//! same relationship `nix/hm-module.nix` has to it.

use hytte_plugin::proto::Mount;

/// The launch-time placement variable, as `docs/plugin-env.md` documents it and
/// `hytte_plugin::run` reads it.
pub const MOUNT_ENV: &str = "HYTTE_PLUGIN_MOUNT";

/// The mount this instance actually registered on: the launch override when one
/// is set and parses, else the manifest's own.
///
/// `lookup` is injected rather than read from the process because
/// `unsafe_code = "forbid"` rules out `std::env::set_var` (an `unsafe fn` in
/// edition 2024), so a test that drove the real environment could not exist at
/// all — the same reason `hytte_config::subsystem::Subsystem::resolve` takes
/// one.
#[must_use]
pub fn effective(manifest: Mount, lookup: &dyn Fn(&str) -> Option<String>) -> Mount {
    lookup(MOUNT_ENV)
        .as_deref()
        .map(str::trim)
        .and_then(Mount::from_wire_name)
        .unwrap_or(manifest)
}

/// [`effective`] against the real environment — the one place this crate reads
/// one. A non-UTF-8 value comes back `None` from `var` and so falls to the
/// manifest, which is unreachable for the reason the module doc gives: the SDK
/// refuses such a launch before this runs.
#[must_use]
pub fn effective_from_env(manifest: Mount) -> Mount {
    effective(manifest, &|key| std::env::var(key).ok())
}

#[cfg(test)]
mod tests {
    use super::{MOUNT_ENV, effective};
    use hytte_plugin::proto::Mount;

    /// The lookup answers `value` for `MOUNT_ENV` and nothing for anything
    /// else, asserting which variable was asked for from a **literal** rather
    /// than from the const — so a renamed const cannot pass here by agreeing
    /// with itself.
    fn with(value: Option<&str>) -> impl Fn(&str) -> Option<String> + use<'_> {
        move |key| {
            assert_eq!(
                key, "HYTTE_PLUGIN_MOUNT",
                "the effective mount must come from the documented variable",
            );
            value.map(str::to_owned)
        }
    }

    /// Unset → the manifest's own mount, which is the single-instance case
    /// every plugin in the tree is on today.
    #[test]
    fn an_unset_variable_leaves_the_manifest_mount() {
        for mount in Mount::ALL {
            assert_eq!(effective(mount, &with(None)), mount);
        }
    }

    /// Every one of the nine wire names resolves to itself — including the one
    /// the manifest already asked for, so "the override was read" is not
    /// confused with "the manifest happened to agree".
    #[test]
    fn every_wire_name_resolves_to_its_own_mount() {
        for mount in Mount::ALL {
            assert_eq!(
                effective(Mount::SidebarRightTop, &with(Some(mount.wire_name()))),
                mount,
                "{}",
                mount.wire_name(),
            );
        }
    }

    /// Surrounding whitespace is trimmed, matching the SDK's own parser — a
    /// stray space in a Nix string must not silently move this instance onto
    /// the other table.
    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(
            effective(Mount::SidebarTop, &with(Some("  BarCenter\t"))),
            Mount::BarCenter,
        );
    }

    /// An unparseable value falls back to the manifest — the arm the module doc
    /// calls unreachable in a live process, because `hytte_plugin::run` exits
    /// non-zero on exactly these values before a session ever starts. It is
    /// pinned anyway: "unreachable" is a claim about the SDK, and this function
    /// has to be total either way.
    #[test]
    fn an_unparseable_value_falls_back_to_the_manifest() {
        for bad in ["", "   ", "sidebarrighttop", "SidebarRight", "right"] {
            assert_eq!(
                effective(Mount::SidebarRightTop, &with(Some(bad))),
                Mount::SidebarRightTop,
                "{bad:?}",
            );
        }
        assert_eq!(MOUNT_ENV, "HYTTE_PLUGIN_MOUNT");
    }
}
