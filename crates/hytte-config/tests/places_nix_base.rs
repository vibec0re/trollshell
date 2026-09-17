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
        Some(&(
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

/// A no-op save over the nix-rendered bytes rewrites **nothing at all** —
/// `[departures]` included.
///
/// This is the brief's item (c) answered with what is actually true, corrected
/// from what #1338 shipped it as. `render_places` is a format-preserving
/// patch rather than a re-render, so every byte nix wrote survives it: the
/// `_locked` line (the #1331 "a marker survives the writer's stale-key sweep"
/// property, one writer over), nix's own within-table key order, its inline
/// arrays, its `12.0` spelling — **and**, since #1339, the `[departures]`
/// table's *position* too. `render_places` used to restate each rebuilt
/// `[[place]]` table's document position as its own index in the array (`0,
/// 1, …`), which only matched the real file order when nothing else in the
/// document had a lower position than the array's first entry — a standalone
/// table declared *above* `[[place]]` does, so the array was forced back down
/// past it on every save, walking `[departures]` one block further down each
/// time (measured over this fixture, before the fix: `round 1` — after 1
/// place; `round 2` — after 2, settled; i.e. **N** byte-churning saves, N =
/// the place count, each moving the content hash `ConfigWatcher` polls — one
/// spurious reload apiece for the shell and the control center). The fix
/// (`space_tables`, `crates/hytte-config/src/places.rs`) reuses each rebuilt
/// table's *own* original position instead of inventing one, so a save that
/// reorders nothing hands every entry back exactly its own slot and
/// `[departures]` — never touched — never moves. See
/// `a_departures_first_edit_settles_in_the_one_save_that_makes_it` below for
/// the *edited* (not no-op) case, which is where "one save, not N" actually
/// bites.
///
/// Still pre-existing (the shipped `DEFAULT_CONFIG` puts `[departures]` last,
/// which is why no test had met this until nix started rendering it first —
/// alphabetically, `departures` sorts before `place`) and still not reachable
/// through the nix path in production: the `_locked` line above means
/// `check_unlocked` refuses the save before the writer ever runs
/// (`a_save_is_refused_while_nix_owns_the_list` in `places.rs`'s own tests).
/// The document this test's byte-equality is about is `render_places` itself,
/// called directly — the same call a hand-edited, *unlocked* overlay would
/// reach.
#[test]
fn a_no_op_save_over_the_nix_rendered_bytes_rewrites_no_line() {
    let parsed = places::parse_places(NIX_RENDERED).expect("the fixture parses");

    let rendered = places::render_places(NIX_RENDERED, &parsed).expect("the fixture re-renders");

    assert_eq!(
        rendered, NIX_RENDERED,
        "a save that changes nothing must rewrite nothing — [departures] included"
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

/// A real edit over a `[departures]`-first document settles in the **one**
/// save that makes it — not the N (N = place count) the pre-#1339 writer
/// needed before it stopped moving the table.
///
/// Unlike the no-op case above, this document is (deliberately) not the
/// locked nix fixture: it is the shape a hand-edited, *unlocked* overlay can
/// actually be — nothing stops an operator from writing `[departures]` above
/// their `[[place]]` blocks — and the edit is a real one (a changed
/// `walk_minutes`), so there is something for `render_places` to do besides
/// hand every table back its own bytes.
#[test]
fn a_departures_first_edit_settles_in_the_one_save_that_makes_it() {
    const HAND_EDITED: &str = "\
[departures]
endpoint = \"vbb\"

[[place]]
name = \"Schöneweide\"
lat = 52.4556
lon = 13.5085
walk_minutes = 5

[[place]]
name = \"Werkstatt\"
lat = 52.5163
lon = 13.4549
";

    let mut places = places::parse_places(HAND_EDITED).expect("the fixture parses");
    places[0].walk_minutes = 9;

    let round1 = places::render_places(HAND_EDITED, &places).expect("first save renders");
    let departures_at = |text: &str| text.find("[departures]").expect("departures is present");
    let first_place_at = |text: &str| text.find("[[place]]").expect("a place is present");
    assert!(
        departures_at(&round1) < first_place_at(&round1),
        "one save must be enough to keep [departures] above [[place]] — it \
         started there: {round1}"
    );
    assert!(
        round1.contains("walk_minutes = 9"),
        "and the actual edit still landed: {round1}"
    );

    // The convergence claim: a second save of round 1's own (now edited)
    // places over round 1's own bytes is a true no-op — settled in the one
    // round the edit itself took, not walking any further.
    let round2 = places::render_places(&round1, &places).expect("second save renders");
    assert_eq!(
        round2, round1,
        "settled after the one save the edit needed — no further churn"
    );
}
