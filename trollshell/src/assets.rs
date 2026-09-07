//! Bundled asset path resolution.
//!
//! Resolution order, highest priority first:
//!
//! 1. `TROLLSHELL_DATA_DIR` env at runtime (override, e.g. for testing).
//! 2. `TROLLSHELL_DATA_DIR` env at compile time (set by the Nix derivation
//!    to `$out/share/trollshell`).
//! 3. `CARGO_MANIFEST_DIR` (dev fallback — the asset sources live in the
//!    top-level `assets/trollshell/` dir, i.e. `../assets/trollshell`
//!    relative to this crate's `Cargo.toml`).

use std::path::PathBuf;

use hytte::gtk;
use hytte::gtk::gdk;

const COMPILED_BASE: &str = match option_env!("TROLLSHELL_DATA_DIR") {
    Some(s) => s,
    None => concat!(env!("CARGO_MANIFEST_DIR"), "/../assets/trollshell"),
};

#[must_use]
pub fn path(rel: &str) -> PathBuf {
    let base = std::env::var("TROLLSHELL_DATA_DIR").unwrap_or_else(|_| COMPILED_BASE.to_string());
    PathBuf::from(base).join(rel)
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
