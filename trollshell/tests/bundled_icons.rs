//! The shell's bundled icons must resolve **by name**, not only by file path
//! (#957).
//!
//! Every native chip loads its glyph with
//! `gtk::Image::from_file(assets::path("icons/cpu.svg"))`, which needs no icon
//! theme at all — so nothing in the tree ever exercised the theme for these
//! files, and nothing had to. A *widget plugin* cannot do that: `Node::Icon`
//! carries a themed **name** and never pixels, on purpose. So the claude-bridge
//! chip's `claude-symbolic` glyph (#957) is the first bundled icon that has to
//! come out of `GtkIconTheme`, and the only thing that makes it resolvable is
//! `assets::install_icon_search_path` putting `assets/trollshell/icons/` on the
//! theme's search path.
//!
//! That mechanism rests on a GTK behaviour this repo had no other test for: the
//! legacy lookup where an image file sitting *directly* in a search-path
//! directory becomes that icon name, with no `index.theme` and no
//! `hicolor/scalable/…` tree. If a GTK release ever drops it, the chip silently
//! renders `image-missing` on Annika's bar and nothing else goes red. This file
//! is that alarm.
//!
//! ## Why an integration test, and why gated
//!
//! `gtk::IconTheme::for_display` needs a real `GdkDisplay`, so this needs a
//! display server (`xvfb-run`) — hence the `system-tests` gate, like every other
//! file in this directory. It reaches `trollshell::assets` through the shadow
//! `lib` target `src/lib.rs` exists for (#674).
//!
//! ## One `#[gtk::test]`, deliberately
//!
//! `install_icon_search_path` mutates process-global state (the display's one
//! `GtkIconTheme`), and `#[gtk::test]` shares a single GTK thread across every
//! test in a binary *in an unspecified order* — so a second test here would race
//! the before/after assertion below into whichever half happened to run first.
//! Everything therefore lives in one function, in order.
//!
//! ## Falsification
//!
//! The test carries its own: it asserts the name does **not** resolve before
//! `install_icon_search_path` runs, so deleting that call's body (or pointing it
//! at the wrong directory) fails the *second* half rather than passing
//! vacuously.

#![cfg(feature = "system-tests")]

use hytte::gtk;
use hytte::gtk::gdk;
// `gio::File::path()` on the resolved `IconPaintable` lives on `FileExt`.
use hytte::gtk::prelude::*;
use trollshell::assets;

/// The name the claude-bridge chip puts on the wire
/// (`crates/hytte-claude-bridge/src/plugin.rs`'s `CLAUDE_ICON`). Spelled out
/// here rather than imported: the plugin is a separate binary crate that this
/// one deliberately does not link, and the *string* is the contract between
/// them.
const CLAUDE_ICON: &str = "claude-symbolic";

#[gtk::test]
fn the_bundled_claude_glyph_resolves_through_the_icon_theme() {
    let display = gdk::Display::default().expect("a display (run under xvfb-run)");
    let theme = gtk::IconTheme::for_display(&display);
    let search_before = theme.search_path();

    assert!(
        !theme.has_icon(CLAUDE_ICON),
        "precondition: {CLAUDE_ICON} is not an Adwaita/hicolor name — if this \
         ever fires, the assertion below stops proving anything"
    );

    assets::install_icon_search_path();

    assert!(
        theme.has_icon(CLAUDE_ICON),
        "the bundled icon dir is on the search path, so the flat \
         claude-symbolic.svg in it must resolve as an icon name"
    );

    let paintable = theme.lookup_icon(
        CLAUDE_ICON,
        &[],
        16,
        1,
        gtk::TextDirection::None,
        gtk::IconLookupFlags::empty(),
    );
    let path = paintable
        .file()
        .and_then(|f| f.path())
        .expect("the lookup resolved to a real file, not the missing-icon fallback");
    assert!(
        path.ends_with("claude-symbolic.svg"),
        "resolved to {path:?}, not the bundled SVG"
    );
    // Canonicalized on both sides: the dev-fallback data dir is
    // `<manifest>/../assets/trollshell`, and `Path::starts_with` is purely
    // lexical — it would not see through that `..` even though GTK hands back
    // the normalized path.
    let icons_dir = std::fs::canonicalize(assets::icons_dir()).expect("the bundled icon dir");
    let resolved = std::fs::canonicalize(&path).expect("the resolved icon file");
    assert!(
        resolved.starts_with(&icons_dir),
        "resolved to {resolved:?}, outside the shell's own icon dir {icons_dir:?}"
    );

    // The `-symbolic` suffix is not decoration: it is what puts the glyph on
    // GTK's recolouring path, so it inks with the CSS `color` of the chip like
    // its Adwaita neighbours instead of rendering at the SVG's own fill. A
    // rename to plain `claude.svg` would still resolve above and quietly lose
    // this.
    assert!(
        paintable.is_symbolic(),
        "the bundled glyph must recolour with the theme"
    );

    // The directory is **appended**, never set: every entry the theme already
    // searched stays ahead of it, so a bundled file can never shadow an Adwaita
    // name (the chip's other three glyphs are Adwaita symbolics). Asserted on
    // the search path itself rather than by looking up an Adwaita name, which
    // would depend on the icon themes installed in whatever sandbox this runs
    // in — the very fragility `main.rs`'s forced theme name works around.
    let search_after = theme.search_path();
    assert_eq!(
        search_after.len(),
        search_before.len() + 1,
        "exactly one entry added, and the existing path is not replaced"
    );
    assert_eq!(
        search_after[..search_before.len()],
        search_before[..],
        "…the entries the theme already had keep their order, and their priority"
    );
    let appended = search_after.last().expect("the appended entry");
    assert_eq!(
        std::fs::canonicalize(appended).ok(),
        Some(icons_dir),
        "the appended entry is the shell's bundled icon dir, and it is last"
    );
}
