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

/// Revision baked in at compile time, else the dev placeholder. See the module
/// docs for why this is normally `None` in the packaged build.
const COMPILED_REV: &str = match option_env!("TROLLSHELL_REV") {
    Some(s) => s,
    None => "dev",
};

/// The source revision this binary was built from — a short git hash
/// (`34e3d96`), a dirty-tree hash (`34e3d96-dirty`), `"unknown"` for a non-git
/// source, or `"dev"` for an unstamped local build. Never empty.
#[must_use]
pub fn revision() -> String {
    std::env::var("TROLLSHELL_REV")
        .ok()
        .filter(|rev| !rev.is_empty())
        .unwrap_or_else(|| COMPILED_REV.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{COMPILED_REV, revision};

    /// The point of the fallback chain is that *something* always comes back —
    /// a caller (the `Control.Revision` D-Bus method, any future UI surface)
    /// must never have to render an empty string. Deliberately env-free: the
    /// workspace forbids `unsafe`, so `std::env::set_var` (unsafe since edition
    /// 2024) is not available to drive the runtime tier from a test.
    #[test]
    fn revision_is_never_empty() {
        assert!(!revision().is_empty());
        assert!(!COMPILED_REV.is_empty());
    }
}
