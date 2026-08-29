//! Golden pin on `places.toml`'s **bytes**, captured before #868's layering
//! landed and asserted unchanged after it.
//!
//! `places.toml` has two permanent editors — the operator in `$EDITOR` and the
//! control center — and the whole reason [`hytte_config`] exists is that they
//! must agree byte for byte (#640/#703). #868 adds a layered reader, a merge
//! layer and a state writer *beside* that path; "strictly additive" is easy to
//! claim and hard to see in a diff, so this file pins the observable output of
//! every public entry point places has, as literal expected text.
//!
//! The expected values below were produced by running this exact file against
//! `origin/main` at `970dd20` — i.e. with none of #868's production code in the
//! tree — and pasting the captured report in. A later change that shifts a
//! byte of the reader, the writer, the normaliser, the error strings or the
//! config path fails here, whether or not it meant to.
//!
//! Deliberately an **integration** test: it can only reach the public API, so
//! it pins the surface the control center actually links against rather than
//! internals it cannot see.

use std::fmt::Write as _;

use hytte_config::places::{self, Place};

/// A file with everything the format-preserving writer promises to keep: a
/// preamble comment, per-key comments, hand-chosen key ordering inside a
/// `[[place]]`, a key the model has never heard of, and an unrelated
/// top-level table.
const FIXTURE: &str = r#"# A hand-annotated places.toml.
# Both lines of this preamble must survive every save.

[[place]]
# The office. Key order here is deliberate: station first.
station = "900110001"
name = "Office"
lat = 52.5
lon = 13.4
ssids = ["office-wifi", "neighbour-ap"]
match_min = 1
# A key this model has never heard of.
colour = "amber"
radius_km = 8.0

[[place]]
name = "Cabin"
lat = 60.1
lon = 10.7
radius_km = 25.0 # generous: no fingerprint out here

[unrelated]
kept = true
"#;

fn parsed() -> Vec<Place> {
    places::parse_places(FIXTURE).expect("the fixture parses")
}

fn rendered(places: &[Place]) -> String {
    places::render_places(FIXTURE, places).expect("the fixture re-renders")
}

/// Every scenario, as `(name, observed output)`.
fn report() -> String {
    let base = parsed();

    let mut edited = base.clone();
    edited[0].station = Some("900120005".into());
    edited[0].walk_minutes = 4;
    edited[0].ssids.push("third-ap".into());

    let mut unset = base.clone();
    unset[0].station = None;

    let added = places::added(&base, Place::new("Studio", 1.5, 2.5)).expect("add is valid");
    let removed = places::removed(&base, "office").expect("Office exists");
    let renamed = places::renamed(&base, "cabin", "Hytte").expect("Cabin exists");

    let messy = Place {
        name: "  Padded  ".into(),
        station: Some("   ".into()),
        ssids: vec![String::new(), "  ".into(), "keep".into()],
        lines: vec!["S1".into(), " ".into()],
        directions: vec![String::new()],
        ..Place::new("ignored", 1.0, 2.0)
    };

    let mut bad_lat = base.clone();
    bad_lat[0].lat = 500.0;
    let mut bad_lon = base.clone();
    bad_lon[0].lon = -181.0;
    let mut bad_radius = base.clone();
    bad_radius[0].radius_km = 0.0;
    let mut blank_name = base.clone();
    blank_name[0].name = "   ".into();
    let mut dup = base.clone();
    dup[1].name = " OFFICE ".into();

    // The whole-file save path, through the real atomic writer.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("places.toml");
    std::fs::write(&path, FIXTURE).expect("seed");
    places::save_to(&path, &base, edited.clone()).expect("save");
    let saved = std::fs::read_to_string(&path).expect("read back");

    // `$HOME`-relative config path: pinned so a later change cannot quietly
    // move `places.toml` onto an XDG search path.
    let config_path = temp_env::with_var("HOME", Some("/golden/home"), || {
        format!("{:?}", places::config_path())
    });

    let default_places = places::parse_places(places::DEFAULT_CONFIG).expect("default parses");
    let default_round_trip =
        places::render_places(places::DEFAULT_CONFIG, &default_places).expect("default re-renders");

    let sections: Vec<(&str, String)> = vec![
        ("parse", format!("{base:?}")),
        ("identity-render", rendered(&base)),
        ("edit-station-walk-ssid", rendered(&edited)),
        ("unset-station", rendered(&unset)),
        ("add-place", rendered(&added)),
        ("remove-first", rendered(&removed)),
        ("rename", rendered(&renamed)),
        ("normalize", format!("{:?}", places::normalize(messy))),
        (
            "validate-errors",
            [
                places::validate(&base).err(),
                places::validate(&bad_lat).err(),
                places::validate(&bad_lon).err(),
                places::validate(&bad_radius).err(),
                places::validate(&blank_name).err(),
                places::validate(&dup).err(),
            ]
            .iter()
            .map(|e| {
                e.as_ref()
                    .map_or_else(|| "ok".to_string(), ToString::to_string)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        ),
        ("save-to-bytes", saved),
        ("config-path", config_path),
        ("default-config-parse", format!("{default_places:?}")),
        (
            "default-config-round-trips-verbatim",
            format!("{}", default_round_trip == places::DEFAULT_CONFIG),
        ),
        (
            "builtin-default-matches-parse",
            format!("{}", places::builtin_default() == default_places),
        ),
    ];

    let mut out = String::new();
    for (name, body) in sections {
        writeln!(out, "═══ {name} ═══\n{body}").expect("writing to a String cannot fail");
    }
    out
}

/// The report as `origin/main` at `970dd20` produced it — captured *before*
/// #868's production code existed, then re-run against the tree that carries
/// it. Every byte here is observed, not authored.
const GOLDEN: &str = r#"═══ parse ═══
[Place { name: "Office", lat: 52.5, lon: 13.4, radius_km: 8.0, ssids: ["office-wifi", "neighbour-ap"], match_min: 1, station: Some("900110001"), walk_minutes: 0, lines: [], directions: [] }, Place { name: "Cabin", lat: 60.1, lon: 10.7, radius_km: 25.0, ssids: [], match_min: 2, station: None, walk_minutes: 0, lines: [], directions: [] }]
═══ identity-render ═══
# A hand-annotated places.toml.
# Both lines of this preamble must survive every save.

[[place]]
# The office. Key order here is deliberate: station first.
station = "900110001"
name = "Office"
lat = 52.5
lon = 13.4
ssids = ["office-wifi", "neighbour-ap"]
match_min = 1
# A key this model has never heard of.
colour = "amber"
radius_km = 8.0

[[place]]
name = "Cabin"
lat = 60.1
lon = 10.7
radius_km = 25.0 # generous: no fingerprint out here

[unrelated]
kept = true

═══ edit-station-walk-ssid ═══
# A hand-annotated places.toml.
# Both lines of this preamble must survive every save.

[[place]]
# The office. Key order here is deliberate: station first.
station = "900120005"
name = "Office"
lat = 52.5
lon = 13.4
ssids = ["office-wifi", "neighbour-ap", "third-ap"]
match_min = 1
# A key this model has never heard of.
colour = "amber"
radius_km = 8.0
walk_minutes = 4

[[place]]
name = "Cabin"
lat = 60.1
lon = 10.7
radius_km = 25.0 # generous: no fingerprint out here

[unrelated]
kept = true

═══ unset-station ═══
# A hand-annotated places.toml.
# Both lines of this preamble must survive every save.

[[place]]
name = "Office"
lat = 52.5
lon = 13.4
ssids = ["office-wifi", "neighbour-ap"]
match_min = 1
# A key this model has never heard of.
colour = "amber"
radius_km = 8.0

[[place]]
name = "Cabin"
lat = 60.1
lon = 10.7
radius_km = 25.0 # generous: no fingerprint out here

[unrelated]
kept = true

═══ add-place ═══
# A hand-annotated places.toml.
# Both lines of this preamble must survive every save.

[[place]]
# The office. Key order here is deliberate: station first.
station = "900110001"
name = "Office"
lat = 52.5
lon = 13.4
ssids = ["office-wifi", "neighbour-ap"]
match_min = 1
# A key this model has never heard of.
colour = "amber"
radius_km = 8.0

[[place]]
name = "Cabin"
lat = 60.1
lon = 10.7
radius_km = 25.0 # generous: no fingerprint out here

[[place]]
name = "Studio"
lat = 1.5
lon = 2.5
radius_km = 12.0
ssids = []
match_min = 2
walk_minutes = 0
lines = []
directions = []

[unrelated]
kept = true

═══ remove-first ═══
# A hand-annotated places.toml.
# Both lines of this preamble must survive every save.

[[place]]
name = "Cabin"
lat = 60.1
lon = 10.7
radius_km = 25.0 # generous: no fingerprint out here

[unrelated]
kept = true

═══ rename ═══
# A hand-annotated places.toml.
# Both lines of this preamble must survive every save.

[[place]]
# The office. Key order here is deliberate: station first.
station = "900110001"
name = "Office"
lat = 52.5
lon = 13.4
ssids = ["office-wifi", "neighbour-ap"]
match_min = 1
# A key this model has never heard of.
colour = "amber"
radius_km = 8.0

[[place]]
name = "Hytte"
lat = 60.1
lon = 10.7
radius_km = 25.0 # generous: no fingerprint out here

[unrelated]
kept = true

═══ normalize ═══
Place { name: "Padded", lat: 1.0, lon: 2.0, radius_km: 12.0, ssids: ["keep"], match_min: 2, station: None, walk_minutes: 0, lines: ["S1"], directions: [] }
═══ validate-errors ═══
ok
"Office": latitude 500 is outside -90..=90
"Office": longitude -181 is outside -180..=180
"Office": radius_km 0 must be positive
a place needs a name
a place named " OFFICE " already exists
═══ save-to-bytes ═══
# A hand-annotated places.toml.
# Both lines of this preamble must survive every save.

[[place]]
# The office. Key order here is deliberate: station first.
station = "900120005"
name = "Office"
lat = 52.5
lon = 13.4
ssids = ["office-wifi", "neighbour-ap", "third-ap"]
match_min = 1
# A key this model has never heard of.
colour = "amber"
radius_km = 8.0
walk_minutes = 4

[[place]]
name = "Cabin"
lat = 60.1
lon = 10.7
radius_km = 25.0 # generous: no fingerprint out here

[unrelated]
kept = true

═══ config-path ═══
Some("/golden/home/.config/trollshell/places.toml")
═══ default-config-parse ═══
[Place { name: "Schöneweide", lat: 52.4556, lon: 13.5085, radius_km: 12.0, ssids: [], match_min: 2, station: Some("900192001"), walk_minutes: 10, lines: [], directions: [] }]
═══ default-config-round-trips-verbatim ═══
true
═══ builtin-default-matches-parse ═══
true
"#;

/// The whole public `places` surface, pinned byte for byte.
///
/// Not a round-trip assertion (`assert_eq!(f(x), f(x))` proves nothing): the
/// right-hand side is a literal recorded from a *different revision* of the
/// production code, so any behavioural drift in the reader, the writer, the
/// normaliser, the validator's messages or the config path shows up as a diff.
#[test]
fn places_is_byte_identical_to_the_pre_layering_baseline() {
    assert_eq!(
        report(),
        GOLDEN,
        "places.toml's observable behaviour moved; #868 is supposed to be strictly additive"
    );
}
