//! Golden pin on the **first file** a settings form creates for a family the
//! operator has never configured (#1370), and the one invariant that has to
//! hold for every family in the tree: a commented default states nothing.
//!
//! Two halves, and they answer different questions.
//!
//! The first is a byte pin in `places_byte_identical.rs`'s shape — one report
//! of every observable step of a form's life against a fixture family, as
//! literal expected text, reached through the **public** API only. The writer
//! here has the same two permanent editors `places` has (the operator in
//! `$EDITOR`, the control center's generic form) and the same requirement:
//! what it writes is a file a person reads. Unlike the `places` golden, these
//! bytes were *authored* rather than captured — there is no earlier revision
//! that produced them, since #1365 wrote the one-line file this replaces — so
//! they are pinned here to be reviewed once and defended afterwards.
//!
//! The second is the invariant, discovered rather than listed: every
//! `DEFAULT_TOML` literal in the workspace is found by scanning the source
//! tree, and each one's commented render must parse to an **empty** document
//! and round-trip back to the original bytes. A `hytte-config` test cannot
//! reach the families themselves — `hytte-config-families`,
//! `hytte-plugin-agents` and `hytte-plugin-stats` all depend on *this* crate,
//! so a dev-dependency back would be a cycle that drags a plugin SDK into this
//! crate's test graph — and a hand-copied list of families rots the day a
//! fifth one lands. So the text is read out of the sources the same way
//! `nix/lint-glsl.py` reads `GLSL_HEADER` out of `gl_surface.rs`: the scan is
//! the coverage, and `REQUIRED` below is the floor that makes a rename fail
//! loudly rather than silently reducing it.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use hytte_config::subsystem::{self, InvalidValue, Subsystem, commented_default};

// ── Half one: the byte pin ──────────────────────────────────────────────────

/// A documented default in the shape the four real ones share: a file
/// preamble, a top-level key, a table with its own comment block, and an
/// array — enough for every ordering rule the writer has.
const FIXTURE_TOML: &str = r#"# The fixture family (#1370).
#
# Every line of this preamble must survive every save, and none of it may
# state a key.

# Whether the strip is lit at all.
enabled = true

[core]
# Strip colour, any CSS name.
color = "amber"
brightness = 3
palette = ["amber", "rust"]
"#;

#[derive(serde::Deserialize)]
struct Fixture {}

impl Subsystem for Fixture {
    const NAME: &'static str = "fixture";
    const DEFAULT_TOML: &'static str = FIXTURE_TOML;
    type Error = std::convert::Infallible;
    type Resolved = ();
    fn parsed(&self) -> ((), Vec<InvalidValue>) {
        ((), Vec::new())
    }
    fn validate(&self) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn save(path: &Path, key: &str, value: Option<toml_edit::Value>) -> String {
    subsystem::save_leaf_to_locked_unchecked::<Fixture>(path, key, value, &BTreeSet::new())
        .expect("the fixture family accepts every key below");
    std::fs::read_to_string(path).unwrap_or_else(|_| "<no file>\n".to_owned())
}

/// Every scenario, as `(name, observed output)`.
fn report() -> String {
    let dir = tempfile::tempdir().expect("tempdir");

    let form = dir.path().join("fixture.toml");
    let first = save(&form, "core.brightness", Some(7_i64.into()));
    let second = save(&form, "core.color", Some("lcd".into()));
    let third = save(&form, "core.brightness", Some(2_i64.into()));
    let fourth = save(&form, "enabled", Some(false.into()));
    let reset_one = save(&form, "core.brightness", None);
    let reset_rest = {
        save(&form, "core.color", None);
        save(&form, "enabled", None)
    };

    // A reset with no file to reset in creates none: the seed is a preamble
    // for a value that is about to be written, not a file to leave behind.
    let never_saved = dir.path().join("never-saved.toml");
    let reset_into_nothing = save(&never_saved, "enabled", None);

    // The seed is for a file that does not exist. One that does — a hand
    // written overlay, or the one-line file #1365 wrote — is patched as it
    // always was, and does not grow a preamble under the operator's feet.
    let hand_written = dir.path().join("hand-written.toml");
    std::fs::write(&hand_written, "brightness = 1 # mine\n").expect("seed the overlay");
    let patched = save(&hand_written, "enabled", Some(true.into()));

    let sections: Vec<(&str, String)> = vec![
        ("commented-default", commented_default(FIXTURE_TOML)),
        ("first-save-a-table-leaf", first),
        ("second-save-its-sibling", second),
        ("third-save-the-same-leaf", third),
        ("fourth-save-a-top-level-leaf", fourth),
        ("reset-the-table-leaf", reset_one),
        ("reset-everything-else", reset_rest),
        ("reset-with-no-file-at-all", reset_into_nothing),
        ("a-file-that-already-exists", patched),
    ];

    let mut out = String::new();
    for (name, body) in sections {
        writeln!(out, "═══ {name} ═══\n{body}").expect("writing to a String cannot fail");
    }
    out
}

/// What the writer produces, byte for byte.
///
/// Read it as a story: the operator opens a settings page for a family they
/// have never configured, changes one row, changes another, changes the first
/// one back, and then resets each row in turn. The commented preamble is in
/// front of them the whole way, and at no point does the file state a key
/// they did not set.
const GOLDEN: &str = r#"═══ commented-default ═══
# The fixture family (#1370).
#
# Every line of this preamble must survive every save, and none of it may
# state a key.

# Whether the strip is lit at all.
# enabled = true

# [core]
# Strip colour, any CSS name.
# color = "amber"
# brightness = 3
# palette = ["amber", "rust"]

═══ first-save-a-table-leaf ═══
# The fixture family (#1370).
#
# Every line of this preamble must survive every save, and none of it may
# state a key.

# Whether the strip is lit at all.
# enabled = true

# [core]
# Strip colour, any CSS name.
# color = "amber"
# brightness = 3
# palette = ["amber", "rust"]

[core]
brightness = 7

═══ second-save-its-sibling ═══
# The fixture family (#1370).
#
# Every line of this preamble must survive every save, and none of it may
# state a key.

# Whether the strip is lit at all.
# enabled = true

# [core]
# Strip colour, any CSS name.
# color = "amber"
# brightness = 3
# palette = ["amber", "rust"]

[core]
brightness = 7
color = "lcd"

═══ third-save-the-same-leaf ═══
# The fixture family (#1370).
#
# Every line of this preamble must survive every save, and none of it may
# state a key.

# Whether the strip is lit at all.
# enabled = true

# [core]
# Strip colour, any CSS name.
# color = "amber"
# brightness = 3
# palette = ["amber", "rust"]

[core]
brightness = 2
color = "lcd"

═══ fourth-save-a-top-level-leaf ═══
# The fixture family (#1370).
#
# Every line of this preamble must survive every save, and none of it may
# state a key.

# Whether the strip is lit at all.
# enabled = true

# [core]
# Strip colour, any CSS name.
# color = "amber"
# brightness = 3
# palette = ["amber", "rust"]

enabled = false
[core]
brightness = 2
color = "lcd"

═══ reset-the-table-leaf ═══
# The fixture family (#1370).
#
# Every line of this preamble must survive every save, and none of it may
# state a key.

# Whether the strip is lit at all.
# enabled = true

# [core]
# Strip colour, any CSS name.
# color = "amber"
# brightness = 3
# palette = ["amber", "rust"]

enabled = false
[core]
color = "lcd"

═══ reset-everything-else ═══
# The fixture family (#1370).
#
# Every line of this preamble must survive every save, and none of it may
# state a key.

# Whether the strip is lit at all.
# enabled = true

# [core]
# Strip colour, any CSS name.
# color = "amber"
# brightness = 3
# palette = ["amber", "rust"]

[core]

═══ reset-with-no-file-at-all ═══
<no file>

═══ a-file-that-already-exists ═══
brightness = 1 # mine
enabled = true

"#;

#[test]
fn the_first_file_is_the_documented_default_with_every_value_commented_out() {
    assert_eq!(report(), GOLDEN);
}

/// Nothing above states a key the operator did not set — asserted by parsing
/// each step rather than by reading it.
///
/// The pin above is bytes, which is what a person reads; this is the meaning,
/// which is what the loader reads. A render that lost a `#` would still look
/// like documentation in a diff and would put a value at the top of the
/// precedence order, which is the #1365 bug this whole file exists to keep
/// fixed.
#[test]
fn no_step_of_that_story_states_a_key_nobody_set() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fixture.toml");

    let stated = |body: &str| -> BTreeSet<String> {
        let table: toml::Table = body.parse().expect("every step is TOML");
        let mut out = BTreeSet::new();
        flatten(&table, "", &mut out);
        out
    };

    let after_first = save(&path, "core.brightness", Some(7_i64.into()));
    assert_eq!(
        stated(&after_first),
        BTreeSet::from(["core.brightness".to_owned()])
    );

    let after_second = save(&path, "enabled", Some(false.into()));
    assert_eq!(
        stated(&after_second),
        BTreeSet::from(["core.brightness".to_owned(), "enabled".to_owned()])
    );

    // `[core]` is a documented table, so the reset keeps its header — an
    // empty table, which states no leaf.
    let after_reset = save(&path, "core.brightness", None);
    assert_eq!(stated(&after_reset), BTreeSet::from(["enabled".to_owned()]));
}

fn flatten(table: &toml::Table, prefix: &str, out: &mut BTreeSet<String>) {
    for (key, value) in table {
        let path = format!("{prefix}{key}");
        match value {
            toml::Value::Table(sub) => flatten(sub, &format!("{path}."), out),
            _ => {
                out.insert(path);
            }
        }
    }
}

// ── Half two: every family in the tree ──────────────────────────────────────

/// The families this scan must find, relative to the workspace root.
///
/// The floor, not the list: whatever else the walk turns up is checked too,
/// so a fifth family is covered the day it lands. These four are named so a
/// rename or a move fails here instead of quietly shrinking the coverage to
/// nothing — the "0 passed means the filter matched nothing" failure.
///
/// `trollshell/src/config/*` is deliberately absent: those declare the shell's
/// `Subsystem` impls over `hytte-config-families`' consts
/// (`const DEFAULT_TOML: &'static str = DEFAULT_TOML;`) and carry no text of
/// their own.
const REQUIRED: &[&str] = &[
    "crates/hytte-config-families/src/core_leds.rs",
    "crates/hytte-config-families/src/workspaces.rs",
    "crates/hytte-plugin-agents/src/config.rs",
    "crates/hytte-plugin-stats/src/config.rs",
];

#[test]
fn every_documented_default_in_the_tree_renders_to_a_document_that_states_nothing() {
    let found = documented_defaults();
    let paths: BTreeSet<&str> = found.iter().map(|(name, _)| name.as_str()).collect();
    for required in REQUIRED {
        assert!(
            paths.contains(required),
            "the scan found no DEFAULT_TOML in {required}; it found {paths:?}"
        );
    }

    for (name, text) in &found {
        text.parse::<toml::Table>()
            .unwrap_or_else(|e| panic!("{name}'s DEFAULT_TOML is not TOML: {e}"));

        let rendered = commented_default(text);
        let back: toml::Table = rendered
            .parse()
            .unwrap_or_else(|e| panic!("{name}'s commented default is not TOML: {e}\n{rendered}"));
        assert!(
            back.is_empty(),
            "{name}'s commented default still states {:?}:\n{rendered}",
            back.keys().collect::<Vec<_>>()
        );
    }
}

#[test]
fn every_documented_default_in_the_tree_round_trips_through_the_render() {
    for (name, text) in documented_defaults() {
        let rendered = commented_default(&text);
        assert_eq!(
            rendered.lines().count(),
            text.lines().count(),
            "{name}: the render is line for line"
        );

        let mut back = String::new();
        for (original, line) in text.lines().zip(rendered.lines()) {
            let was = original.trim_start();
            if was.is_empty() || was.starts_with('#') {
                assert_eq!(line, original, "{name}: a comment line was rewritten");
                back.push_str(line);
            } else {
                let content = line.trim_start();
                back.push_str(&line[..line.len() - content.len()]);
                back.push_str(content.strip_prefix("# ").unwrap_or_else(|| {
                    panic!("{name}: a value line was left uncommented: {line:?}")
                }));
            }
            back.push('\n');
        }
        assert_eq!(back, text, "{name}: uncommenting gives the original back");
    }
}

/// Every `DEFAULT_TOML` string literal in the workspace, as
/// `(path relative to the workspace root, its text)`.
///
/// Deliberately a source scan and not a dependency: see this file's own docs.
/// It matches a `DEFAULT_TOML` **declaration** — the name, a type ascription
/// and an `=` on one line, then a raw string, which is what every family
/// writes (a documented default is full of quotes) — so it steps past both
/// the `const DEFAULT_TOML: &'static str = DEFAULT_TOML;` re-exports and
/// every mention of the name in prose without needing to know about either.
fn documented_defaults() -> Vec<(String, String)> {
    let root = workspace_root();
    let mut sources = Vec::new();
    collect_rust_sources(&root, &mut sources);
    assert!(
        sources.len() > 100,
        "the walk found only {} Rust sources under {}; it is looking in the wrong place",
        sources.len(),
        root.display()
    );

    let mut out = Vec::new();
    for source in sources {
        let body = std::fs::read_to_string(&source).expect("a Rust source this walk just listed");
        for text in raw_default_tomls(&body) {
            let name = source
                .strip_prefix(&root)
                .expect("the walk started at the root")
                .to_string_lossy()
                .into_owned();
            out.push((name, text));
        }
    }
    out.sort();
    out
}

/// This crate's manifest directory is `<root>/crates/hytte-config`.
fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .nth(2)
        .expect("crates/hytte-config has two ancestors")
        .to_path_buf()
}

fn collect_rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            // `target` and `result*` are build output, `.git`/`.claude` are
            // not source. Everything else is walked, so a family in a new
            // top-level directory is still found.
            if name == "target" || name.starts_with('.') || name.starts_with("result") {
                continue;
            }
            collect_rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The raw-string values of every `DEFAULT_TOML` declaration in one source.
fn raw_default_tomls(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find("DEFAULT_TOML") {
        rest = &rest[at + "DEFAULT_TOML".len()..];
        // Only a declaration, and only on the name's own line: `: <type> =`
        // then a raw string. Anything else — a re-export, a doc link, a
        // sentence in a comment — is skipped.
        let line = rest.split('\n').next().unwrap_or_default();
        let Some(eq) = line.find('=') else { continue };
        if !line[..eq].contains(':') || line[..eq].contains('"') {
            continue;
        }
        let after = rest[eq + 1..].trim_start();
        let Some(hashes) = after.strip_prefix('r') else {
            continue;
        };
        let count = hashes.len() - hashes.trim_start_matches('#').len();
        let Some(open) = hashes[count..].strip_prefix('"') else {
            continue;
        };
        let close = format!("\"{}", "#".repeat(count));
        let Some(end) = open.find(&close) else {
            continue;
        };
        out.push(open[..end].to_owned());
        rest = &open[end..];
    }
    out
}
