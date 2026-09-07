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
//! Two facts have to hold, and this file pins both:
//!
//! 1. **GTK's legacy search-path lookup** — an image file sitting *directly* in
//!    a search-path directory becomes that icon name, with no `index.theme` and
//!    no `hicolor/scalable/…` tree. That is the whole reason the flat layout
//!    `assets/trollshell/icons/` already has is usable as-is. It is documented
//!    behaviour, but if a GTK release ever drops it the chip silently renders
//!    `image-missing` on Annika's bar and nothing else goes red.
//! 2. **The shell installs that directory** — appended, so a bundled file can
//!    never shadow an Adwaita name.
//!
//! ## Why the fixture is a temp dir and not `assets/trollshell/icons/`
//!
//! Because the CI sandbox does not have that directory. `nix/package.nix`'s
//! crane source filter deliberately strips `assets/` (bar one stylesheet) so an
//! icon edit can't invalidate the expensive Rust compile (#133) — and
//! `checks.system-tests` in `flake.nix` inherits that same filtered `src`. A
//! test that read the real icon dir would therefore pass here and fail in
//! `nix flake check`, or (worse) be quietly written to skip there, which is
//! where it most needs to run.
//!
//! So fact 1 is proved against a **fixture** written into a `TempDir` under the
//! icon's real name, and fact 2 against the search path itself — neither needs
//! `assets/` to exist. That the shipped file is really *in* the shipped
//! directory is asserted where the shipping happens: `nix/package.nix`'s
//! `trollshell-assets` derivation `test -f`s it, so a rename or a deletion
//! fails `nix build .#trollshell` (and `nix flake check`, which builds it)
//! rather than surfacing as a blank chip.
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
//! Both the fixture and `install_icon_search_path` mutate process-global state
//! (the display's one `GtkIconTheme`), and `#[gtk::test]` shares a single GTK
//! thread across every test in a binary *in an unspecified order* — so a second
//! test here would race the before/after assertions into whichever half
//! happened to run first. Everything lives in one function, in order.
//!
//! ## Falsification
//!
//! The test carries its own: it asserts the name does **not** resolve before the
//! search path is added, so gutting `install_icon_search_path` (or pointing it
//! at the wrong directory) fails the wiring half rather than passing vacuously.

#![cfg(feature = "system-tests")]

use hytte::gtk;
use hytte::gtk::gdk;
// `gio::File::path()` on the resolved `IconPaintable` lives on `FileExt`.
use hytte::gtk::prelude::*;
use trollshell::assets;

/// The name the claude-bridge chip puts on the wire
/// (`crates/hytte-claude-bridge/src/plugin.rs`'s `CLAUDE_ICON`), and therefore
/// the basename the shipped SVG must have. Spelled out here rather than
/// imported: the plugin is a separate binary crate that this one deliberately
/// does not link, and the *string* is the contract between them.
const CLAUDE_ICON: &str = "claude-symbolic";

/// A stand-in for the shipped glyph — the assertions below are about *name
/// resolution*, not pixels, so a filled square is enough. Deliberately shaped
/// like the real one where it matters: a `<path>` with a `fill` (what GTK's
/// symbolic recolour stylesheet overrides) and an explicit size.
// `r##"…"##`, not `r#"…"#`: the `fill="#e5e5e7"` inside contains `"#`, which
// would close a single-hash raw string mid-attribute.
const FIXTURE_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16"><path d="M3 3h10v10H3z" fill="#e5e5e7"/></svg>"##;

#[gtk::test]
fn a_flat_svg_in_a_search_path_dir_resolves_by_name_and_the_shell_adds_that_dir() {
    let display = gdk::Display::default().expect("a display (run under xvfb-run)");
    let theme = gtk::IconTheme::for_display(&display);

    assert!(
        !theme.has_icon(CLAUDE_ICON),
        "precondition: {CLAUDE_ICON} is not an Adwaita/hicolor name — if this \
         ever fires, every assertion below stops proving anything"
    );

    // ── Fact 1: the legacy flat lookup, and the `-symbolic` recolour contract ──
    let fixture_dir = tempfile::tempdir().expect("a scratch dir");
    let fixture = fixture_dir.path().join(format!("{CLAUDE_ICON}.svg"));
    std::fs::write(&fixture, FIXTURE_SVG).expect("write the fixture glyph");
    theme.add_search_path(fixture_dir.path());

    assert!(
        theme.has_icon(CLAUDE_ICON),
        "a bare {CLAUDE_ICON}.svg sitting directly in a search-path dir must \
         resolve as that icon name — no index.theme, no hicolor tree"
    );

    let paintable = theme.lookup_icon(
        CLAUDE_ICON,
        &[],
        16,
        1,
        gtk::TextDirection::None,
        gtk::IconLookupFlags::empty(),
    );
    let resolved = paintable
        .file()
        .and_then(|f| f.path())
        .expect("the lookup resolved to a real file, not the missing-icon fallback");
    // Canonicalized on both sides: on this platform a temp dir is routinely
    // reached through a symlink, and `Path::starts_with` is purely lexical.
    assert_eq!(
        std::fs::canonicalize(&resolved).ok(),
        std::fs::canonicalize(&fixture).ok(),
        "resolved to {resolved:?}, not the fixture"
    );

    // The `-symbolic` suffix is not decoration: it is what puts the glyph on
    // GTK's recolouring path, so it inks with the CSS `color` of the chip like
    // its Adwaita neighbours instead of rendering at the SVG's own fill. Naming
    // the shipped file plain `claude.svg` would still resolve above and quietly
    // lose this.
    assert!(
        paintable.is_symbolic(),
        "a -symbolic name must recolour with the theme"
    );

    // ── Fact 2: the shell installs its own icon dir, appended ─────────────────
    let search_before = theme.search_path();
    assets::install_icon_search_path();
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
    assert_eq!(
        search_after.last().map(std::path::PathBuf::as_path),
        Some(assets::icons_dir().as_path()),
        "the appended entry is the shell's bundled icon dir, and it is last — \
         so a bundled file can never shadow an Adwaita name"
    );
}
