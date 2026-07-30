//! Build revision resolution (#601) — "which commit is this shell?".
//!
//! A running `trollshell` had no way to report the source revision it was built
//! from: the Control endpoint's `Version` returns `CARGO_PKG_VERSION`, which has
//! been `0.1.0` since the first commit and cannot tell yesterday's build from
//! March's. That gap has twice cost a round-trip of investigating behaviour that
//! was already fixed but not yet deployed (#375, and the #566 report that
//! prompted #601).
//!
//! Resolution order, highest priority first — the same three-tier shape
//! [`crate::assets`] uses:
//!
//! 1. `TROLLSHELL_REV` env at **runtime**. This is the tier the packaged build
//!    actually uses: `nix/package.nix`'s `preFixup` bakes the flake's
//!    `self.shortRev` (or `self.dirtyShortRev`) into the `wrapGAppsHook4`
//!    wrapper. It is deliberately a *wrapper* env and not a compile-time one —
//!    a compile-time `TROLLSHELL_REV` would change the whole-workspace crane
//!    derivation's hash on every commit and force a full rebuild per revision.
//! 2. `TROLLSHELL_REV` env at **compile time**, for non-nix builds that want to
//!    stamp a revision in (`TROLLSHELL_REV=$(git rev-parse --short HEAD) cargo
//!    build`). Unset during the nix workspace compile, by design, so that tier
//!    simply falls through there.
//! 3. `"dev"` — a plain `cargo run`/`cargo build` with neither env set. Seeing
//!    `dev` from a deployed shell means it was not built by the nix package.
//!
//! An **empty** value at either env tier is treated as unset rather than
//! propagated, so the "never empty" contract holds for every input. That is not
//! hypothetical: the recipe in tier 2 above sets the variable to the empty
//! string (not unset) when run from a tarball checkout, where `git rev-parse`
//! prints nothing.

/// Collapse an empty revision to the dev placeholder.
///
/// `const` on purpose: [`COMPILED_REV`] needs this in a `const` initializer, and
/// a match *guard* (`Some(s) if !s.is_empty()`) is not legal there — whereas
/// `str::is_empty` is `const`.
const fn or_dev(rev: &str) -> &str {
    if rev.is_empty() { "dev" } else { rev }
}

/// Revision baked in at compile time, else the dev placeholder. See the module
/// docs for why this is normally `None` in the packaged build.
const COMPILED_REV: &str = match option_env!("TROLLSHELL_REV") {
    Some(s) => or_dev(s),
    None => "dev",
};

/// Pick the effective revision from the two env tiers. Split out as a pure
/// function so the empty-string cases at *both* tiers are unit-testable — the
/// runtime tier can't be driven from a test (the workspace forbids `unsafe`, so
/// `std::env::set_var`, unsafe since edition 2024, is unavailable) and the
/// compile-time tier is baked into a `const` before any test runs.
fn resolve<'a>(runtime: Option<&'a str>, compiled: &'a str) -> &'a str {
    match runtime {
        Some(rev) if !rev.is_empty() => rev,
        _ => or_dev(compiled),
    }
}

/// The source revision this binary was built from — a short git hash
/// (`34e3d96`), a dirty-tree hash (`34e3d96-dirty`), `"unknown"` for a non-git
/// source, or `"dev"` for an unstamped local build. Never empty.
///
/// # Caveat: the runtime tier is inherited by child processes
///
/// The packaged wrapper sets `TROLLSHELL_REV` in the *shell's own* environment,
/// and anything the shell forks inherits it — `gio::AppInfo::launch_default_for_uri`
/// (`main.rs`, [`crate::widgets::calendar`]) and the plugin `RunCommand` effect
/// (`plugins/effects.rs`). So a `cargo run` executed inside a
/// terminal that happens to be a *descendant of the shell* will hit the runtime
/// tier and confidently report the **deployed** revision instead of `dev` —
/// inverting the very fixed-vs-fixed-but-not-deployed diagnosis this exists for.
///
/// Known caveat, not a bug, and deliberately not "fixed" by scrubbing the
/// variable on spawn: in the normal setup terminals descend from niri rather
/// than from trollshell, plugins are launched via `systemd-run --user` and so do
/// not inherit it, and `TROLLSHELL_DATA_DIR` has carried the identical hazard
/// since #133 without biting. Diverging from that would be a larger design call.
/// It is written down here so a reader debugging a surprising hash knows the
/// mechanism exists.
#[must_use]
pub fn revision() -> String {
    let runtime = std::env::var("TROLLSHELL_REV").ok();
    resolve(runtime.as_deref(), COMPILED_REV).to_owned()
}

#[cfg(test)]
mod tests {
    use super::{COMPILED_REV, or_dev, resolve, revision};

    #[test]
    fn runtime_tier_wins_when_set() {
        assert_eq!(resolve(Some("34e3d96"), "baked"), "34e3d96");
        assert_eq!(resolve(Some("34e3d96-dirty"), "baked"), "34e3d96-dirty");
    }

    #[test]
    fn unset_or_empty_runtime_falls_through_to_compiled() {
        assert_eq!(resolve(None, "baked"), "baked");
        assert_eq!(resolve(Some(""), "baked"), "baked");
    }

    /// The regression this pins. Before this, only the *runtime* tier rejected
    /// the empty string; an empty **compile-time** bake sailed through and
    /// `revision()` returned `""`, contradicting its own "never empty" contract.
    /// Reachable from the recipe the module docs recommend —
    /// `TROLLSHELL_REV=$(git rev-parse --short HEAD) cargo build` from a tarball
    /// checkout sets the variable to empty rather than leaving it unset.
    #[test]
    fn empty_compile_time_bake_still_yields_dev() {
        assert_eq!(or_dev(""), "dev");
        assert_eq!(resolve(None, ""), "dev");
        assert_eq!(resolve(Some(""), ""), "dev");
    }

    /// The end-to-end contract every caller (the `Control.Revision` D-Bus
    /// method, any future UI surface) relies on: something always comes back.
    #[test]
    fn revision_is_never_empty() {
        assert!(!revision().is_empty());
        assert!(!COMPILED_REV.is_empty());
    }
}
