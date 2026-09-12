//! Bundled asset path resolution.
//!
//! Resolution order, highest priority first — the same three-tier shape
//! [`crate::revision`] uses:
//!
//! 1. `TROLLSHELL_DATA_DIR` env at runtime (override, e.g. for testing).
//! 2. `TROLLSHELL_DATA_DIR` env at compile time (set by the Nix derivation
//!    to `$out/share/trollshell`).
//! 3. `CARGO_MANIFEST_DIR` (dev fallback — the asset sources live in the
//!    top-level `assets/trollshell/` dir, i.e. `../assets/trollshell`
//!    relative to this crate's `Cargo.toml`).
//!
//! An empty value at either env tier is treated as unset rather than
//! propagated, mirroring [`crate::revision`]'s "never empty" contract.

use std::path::PathBuf;

use hytte::gtk;
use hytte::gtk::gdk;

/// Dev fallback: the asset sources live next to this crate's `Cargo.toml`.
const MANIFEST_FALLBACK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../assets/trollshell");

/// Pick the effective base directory across the three tiers. Split out as a
/// pure function for the same reason [`crate::revision::resolve`] is: the
/// runtime tier can't be driven from a test (the workspace forbids `unsafe`,
/// so `std::env::set_var` is unavailable) and the compile-time tier is baked
/// in before any test runs.
fn resolve_with(runtime: Option<&str>, compile_time: Option<&str>) -> PathBuf {
    let base = runtime
        .filter(|s| !s.is_empty())
        .or_else(|| compile_time.filter(|s| !s.is_empty()))
        .unwrap_or(MANIFEST_FALLBACK);
    PathBuf::from(base)
}

#[must_use]
pub fn path(rel: &str) -> PathBuf {
    let runtime = std::env::var("TROLLSHELL_DATA_DIR").ok();
    resolve_with(runtime.as_deref(), option_env!("TROLLSHELL_DATA_DIR")).join(rel)
}

/// The bundled icon directory — `icons/` under the resolved data dir.
#[must_use]
pub fn icons_dir() -> PathBuf {
    path("icons")
}

/// Put [`icons_dir`] on the display's `GtkIconTheme` search path, so a bundled
/// icon resolves **by name** and not only by file path (#957).
///
/// The native chips reach these files directly (`gtk::Image::from_file(
/// assets::path("icons/cpu.svg"))`), which needs no theme at all. A widget
/// plugin cannot: `Node::Icon` carries a *themed name*, never pixels — that is
/// the whole point of the variant — so the only way a plugin can paint one of
/// the shell's own glyphs is for the shell to teach its icon theme where they
/// live. The claude-bridge chip's `claude-symbolic` is the first (#957).
///
/// No `index.theme` and no `hicolor/scalable/...` tree is involved: GTK keeps
/// the legacy lookup where *an image file sitting directly in a search-path
/// directory becomes that icon name*, which is exactly the flat layout
/// `assets/trollshell/icons/` already has. The path is **appended**, so a
/// bundled file can never shadow an Adwaita icon of the same name — the theme
/// proper is searched first.
///
/// Must run after GTK is initialized (it needs a `gdk::Display`); a missing
/// display is a no-op rather than a panic, so a headless caller degrades to
/// "bundled names don't resolve" instead of dying.
pub fn install_icon_search_path() {
    if let Some(display) = gdk::Display::default() {
        gtk::IconTheme::for_display(&display).add_search_path(icons_dir());
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{MANIFEST_FALLBACK, resolve_with};

    #[test]
    fn runtime_tier_wins_when_set() {
        assert_eq!(
            resolve_with(Some("/runtime/dir"), Some("/compiled/dir")),
            PathBuf::from("/runtime/dir")
        );
    }

    #[test]
    fn unset_or_empty_runtime_falls_through_to_compiled() {
        assert_eq!(
            resolve_with(None, Some("/compiled/dir")),
            PathBuf::from("/compiled/dir")
        );
        assert_eq!(
            resolve_with(Some(""), Some("/compiled/dir")),
            PathBuf::from("/compiled/dir")
        );
    }

    /// Mirrors `revision::tests::empty_compile_time_bake_still_yields_dev`: an
    /// empty value at either upper tier must not propagate an empty base — it
    /// falls all the way to the manifest-dir dev fallback.
    #[test]
    fn empty_or_unset_both_tiers_fall_through_to_manifest_fallback() {
        assert_eq!(resolve_with(None, None), PathBuf::from(MANIFEST_FALLBACK));
        assert_eq!(
            resolve_with(Some(""), Some("")),
            PathBuf::from(MANIFEST_FALLBACK)
        );
    }

    /// The end-to-end shape contract every caller ([`super::path`],
    /// [`super::icons_dir`]) relies on: whichever tier wins, `rel` is joined
    /// onto the resolved base unchanged. Env-independent (true regardless of
    /// what `TROLLSHELL_DATA_DIR` happens to be in the environment running
    /// this test), unlike `revision::revision_is_never_empty`'s equivalent.
    #[test]
    fn path_ends_with_the_requested_rel() {
        assert!(super::path("icons/cpu.svg").ends_with("icons/cpu.svg"));
        assert!(super::path("style.css").ends_with("style.css"));
    }
}
