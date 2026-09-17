//! The nix-rendered `places.toml` base layer, read back through the real
//! layered reader (#1227 item 2).
//!
//! `tests/fixtures/places-nix-rendered.toml` is a **recording** of what
//! `programs.trollshell.config.places` renders, pinned byte-for-byte on both
//! platforms by `checks.nixos-module-places-fixture` /
//! `checks.hm-module-places-fixture`. The pair is the point: those two checks
//! say nix still emits these bytes, and this file says
//! `hytte_config::places` still reads them — so a renderer change and a reader
//! change have to land in the same commit or one of the three goes red. It is
//! the `agents` shape (`crates/hytte-plugin-agents/tests/fixtures/`, #1237) one
//! family over, and `nix/treefmt.nix` excludes the fixture from taplo for the
//! same reason: whoever formats it is nix, not us.
//!
//! An integration test rather than a `mod tests` block in `places.rs`, because
//! what it exercises is the *file* — `include_str!` of a checked-in recording —
//! and because the unit tests deliberately drive `assemble_places` on
//! hand-written layer bodies instead.

use std::path::PathBuf;

use hytte_config::places::{self, ENDPOINT_KEY, PLACE_KEY};

/// The recording, at the path the two flake checks diff against.
const NIX_RENDERED: &str = include_str!("fixtures/places-nix-rendered.toml");

fn base_layer() -> Vec<(PathBuf, String)> {
    vec![(
        PathBuf::from("/etc/xdg/trollshell/places.toml"),
        NIX_RENDERED.to_owned(),
    )]
}

/// Everything the nix example set comes back through the real reader, with
/// nothing invented and nothing dropped.
#[test]
fn the_nix_rendered_fixture_round_trips_through_the_real_reader() {
    let loaded = places::assemble_places(&base_layer(), None);

    let names: Vec<&str> = loaded.places.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        ["Schöneweide", "Werkstatt"],
        "the list, in the order nix declared it — the first entry is the \
         provisional home before the first location fix"
    );

    let home = &loaded.places[0];
    assert!((home.lat - 52.4556).abs() < 1e-9);
    assert!((home.lon - 13.5085).abs() < 1e-9);
    assert!((home.radius_km - 12.0).abs() < 1e-9);
    assert_eq!(home.ssids, ["Kabelsalat", "FRITZ!Box 7590 XV"]);
    assert_eq!(home.match_min, 2);
    assert_eq!(home.station.as_deref(), Some("900192001"));
    assert_eq!(home.walk_minutes, 10);

    // The second entry sets none of the optional keys, which is what `prune`
    // dropping a `null` leaf has to look like on the way back in: defaults,
    // not empty strings or zeroes the schema would then write back.
    let work = &loaded.places[1];
    assert_eq!(work.station, None);
    assert!(work.ssids.is_empty());
    assert_eq!(work.lines, ["S8", "S9"]);
    assert_eq!(work.match_min, places::default_match_min());
    assert!((work.radius_km - places::default_radius_km()).abs() < 1e-9);

    assert_eq!(loaded.endpoint.as_deref(), Some("vbb"));
}

/// The `_locked` line is read as the lock it is — the half `lockedLeafPaths`
/// renders and the half both editors grey rows from.
#[test]
fn the_fixtures_locked_line_names_exactly_the_two_keys_nix_set() {
    let loaded = places::assemble_places(&base_layer(), None);

    assert_eq!(
        loaded.locked.iter().map(String::as_str).collect::<Vec<_>>(),
        [ENDPOINT_KEY, PLACE_KEY],
        "#1227: the lock names exactly the leaves the nix example set"
    );
    assert!(loaded.places_are_locked());
    assert!(loaded.endpoint_is_locked());
    assert!(
        loaded.lock_findings.is_empty(),
        "a well-formed marker naming keys its own layer sets complains about \
         nothing: {:?}",
        loaded.lock_findings
    );
}

/// …and an overlay is refused against it, once per key, with the base value
/// kept — the whole reason the marker is there.
#[test]
fn an_overlay_over_the_fixture_is_refused_and_reported() {
    let loaded = places::assemble_places(
        &base_layer(),
        Some((
            PathBuf::from("/home/annika/.config/trollshell/places.toml"),
            "[departures]\nendpoint = \"db\"\n\
             [[place]]\nname = \"Zuhause\"\nlat = 1.0\nlon = 2.0\n"
                .to_owned(),
        )),
    );

    assert_eq!(
        loaded
            .places
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        ["Schöneweide", "Werkstatt"]
    );
    assert_eq!(loaded.endpoint.as_deref(), Some("vbb"));
    assert_eq!(
        loaded
            .lock_findings
            .iter()
            .map(|f| f.key.as_str())
            .collect::<Vec<_>>(),
        [ENDPOINT_KEY, PLACE_KEY],
        "one report per key, both of them"
    );
}

/// A no-op save over the nix-rendered bytes changes **no line** — it only
/// moves one block.
///
/// This is the brief's item (c) answered with what is actually true.
/// `render_places` is a format-preserving patch rather than a re-render, so
/// every byte nix wrote survives it: the `_locked` line (the #1331 "a marker
/// survives the writer's stale-key sweep" property, one writer over), nix's
/// own within-table key order, its inline arrays, its `12.0` spelling, and the
/// `[departures]` table this model barely knows about. What it does **not**
/// preserve is that table's *position*: `toml_edit` re-emits a standalone
/// table after an array-of-tables that was declared below it, so nix's
/// alphabetical `[departures]`-then-`[[place]]` comes back as
/// `[[place]]`-then-`[departures]`.
///
/// That relocation is pre-existing writer behaviour, not something the base
/// layer introduced — the shipped `DEFAULT_CONFIG` already puts `[departures]`
/// last, which is why no test had met it — and it is cosmetic: nothing reads
/// `places.toml` positionally. It is pinned as a *sorted-line* equality rather
/// than waved at, so a change that actually dropped or rewrote a line still
/// reds here.
///
/// It is deliberately not a claim that `pkgs.formats.toml` and `toml_edit`
/// agree about rendering a document from scratch. They do not, which is
/// exactly why this writer patches — and why in production the writer never
/// runs over these bytes at all: nix's base layer is read-only, and the
/// `_locked` line above means a save is refused before it starts
/// (`places::check_unlocked`).
#[test]
fn a_no_op_save_over_the_nix_rendered_bytes_rewrites_no_line() {
    let parsed = places::parse_places(NIX_RENDERED).expect("the fixture parses");

    let rendered = places::render_places(NIX_RENDERED, &parsed).expect("the fixture re-renders");

    let sorted = |text: &str| {
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        lines.sort();
        lines
    };
    assert_eq!(
        sorted(&rendered),
        sorted(NIX_RENDERED),
        "a save that changes nothing must rewrite nothing"
    );
    assert_ne!(
        rendered, NIX_RENDERED,
        "if this is ever byte-equal the relocation stopped happening — tighten \
         this test to a plain byte comparison rather than leaving the weaker \
         one standing"
    );
    assert!(
        rendered.starts_with("_locked = [\"departures.endpoint\", \"place\"]\n"),
        "the marker survives the writer, verbatim and in place: {rendered}"
    );
    assert_eq!(
        places::parse_departures_endpoint(&rendered).expect("re-parses"),
        Some("vbb".to_owned())
    );
    assert_eq!(
        places::parse_places(&rendered).expect("re-parses"),
        parsed,
        "and so does every place"
    );
}
