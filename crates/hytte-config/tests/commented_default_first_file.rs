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
    // Its own comments are the operator's: the leading one belongs to the key
    // it was written above and must not slide down past a new line, and the
    // trailing one belongs to the value (#1380 review, MEDIUM 1).
    let hand_written = dir.path().join("hand-written.toml");
    std::fs::write(
        &hand_written,
        "# I keep the strip dim on purpose\nbrightness = 1 # mine\n",
    )
    .expect("seed the overlay");
    let patched = save(&hand_written, "enabled", Some(true.into()));

    // The same rule where it is easiest to get wrong: the comment sits above
    // the *first* item, which is exactly where the seed's own preamble sits,
    // so only the bytes can tell them apart.
    let annotated = dir.path().join("annotated.toml");
    std::fs::write(
        &annotated,
        "# I keep the strip dim on purpose\n[core]\nbrightness = 1\n",
    )
    .expect("seed the overlay");
    let annotated_saved = save(&annotated, "enabled", Some(false.into()));

    // …and on the removal path, where an unlifted removal correctly takes the
    // comment with the key it described.
    let annotated_reset = dir.path().join("annotated-reset.toml");
    std::fs::write(
        &annotated_reset,
        "# why I turned it off\nenabled = false\n\n[core]\nbrightness = 1\n",
    )
    .expect("seed the overlay");
    let annotated_after_reset = save(&annotated_reset, "enabled", None);

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
        ("an-operators-comment-above-the-first-key", annotated_saved),
        (
            "an-operators-comment-above-the-key-being-reset",
            annotated_after_reset,
        ),
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

# Your own settings go below this line. Edit them there rather than
# uncommenting the documentation above: a key stated twice is not valid
# TOML, and every later save would fail on it.

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

# Your own settings go below this line. Edit them there rather than
# uncommenting the documentation above: a key stated twice is not valid
# TOML, and every later save would fail on it.

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

# Your own settings go below this line. Edit them there rather than
# uncommenting the documentation above: a key stated twice is not valid
# TOML, and every later save would fail on it.

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

# Your own settings go below this line. Edit them there rather than
# uncommenting the documentation above: a key stated twice is not valid
# TOML, and every later save would fail on it.

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

# Your own settings go below this line. Edit them there rather than
# uncommenting the documentation above: a key stated twice is not valid
# TOML, and every later save would fail on it.

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

# Your own settings go below this line. Edit them there rather than
# uncommenting the documentation above: a key stated twice is not valid
# TOML, and every later save would fail on it.

[core]

═══ reset-with-no-file-at-all ═══
<no file>

═══ a-file-that-already-exists ═══
# I keep the strip dim on purpose
brightness = 1 # mine
enabled = true

═══ an-operators-comment-above-the-first-key ═══
enabled = false
# I keep the strip dim on purpose
[core]
brightness = 1

═══ an-operators-comment-above-the-key-being-reset ═══

[core]
brightness = 1

"#;

#[test]
fn the_first_file_is_the_documented_default_with_every_value_commented_out() {
    // `DUMP_REPORT=<path> cargo test …` writes what the writer actually
    // produced, so a deliberate change to the fixture or the seed can be
    // re-pinned by reading the new report rather than by hand-editing 170
    // lines of expected text. It writes only where the caller points it.
    if let Ok(path) = std::env::var("DUMP_REPORT") {
        std::fs::write(path, report()).expect("the path DUMP_REPORT names is writable");
    }
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
    // Not a shipped family — the control center's own fixture family, whose
    // `Subsystem` impl goes through this same writer and whose documented
    // default its `#[gtk::test]`s assert against. It is in the floor because
    // it is one of the five the scan finds today, and a scan that quietly
    // found four would be the failure this list exists to prevent.
    "crates/trollshell-control-center/src/config_form.rs",
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

        // Against the *normalised* source, which is what the render
        // promises: one `\n` per line, no `\r`, a trailing newline whether
        // or not the literal had one (#1380 review, LOW 3). Comparing
        // against the literal's own bytes would red on a cosmetic
        // difference that says nothing about commenting.
        let text = normalised(&text);

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

/// The scan's own classifier, over every spelling it must tell apart
/// (#1380 review, MEDIUM 2).
///
/// Its whole job is to be **loud** about a family it cannot read, and the
/// version this replaces was silent: `plain = "…"` below produced no entry
/// and no complaint, so a fifth family spelt that way would have left the
/// scan reporting four and passing. The `Unreadable` arms are the assertion.
#[test]
fn the_scan_tells_a_family_it_can_read_from_one_it_cannot() {
    let source = concat!(
        "pub const DEFAULT_TOML: &str = r#\"# raw\nkey = 1\n\"#;\n",
        "pub const DEFAULT_TOML: &str = r##\"# hashed \"#\" inside\n\"##;\n",
        "pub(super) const DEFAULT_TOML: &str = r#\"# scoped\n\"#;\n",
        "pub const DEFAULT_TOML: &str = families::core_leds::DEFAULT_TOML;\n",
        // Not published, so not a family: an `impl Subsystem` member, whether
        // it aliases a real one or is a test fixture's inline string.
        "    const DEFAULT_TOML: &'static str = DEFAULT_TOML;\n",
        "    const DEFAULT_TOML: &'static str = F::FAMILY.default_toml;\n",
        "    const DEFAULT_TOML: &'static str = \"enabled = true\\n\";\n",
        // Published and unreadable: each of these must be reported.
        "pub const DEFAULT_TOML: &str = \"# plain\\nkey = 1\\n\";\n",
        "pub const DEFAULT_TOML: &str = concat!(HEAD, TAIL);\n",
        "pub const DEFAULT_TOML: &str = include_str!(\"default.toml\");\n",
        "/// A doc line mentioning DEFAULT_TOML: it is not a declaration.\n",
    );

    let decls = default_toml_decls(source);
    let shapes: Vec<&str> = decls
        .iter()
        .map(|decl| match decl {
            Decl::Text(_) => "text",
            Decl::Alias => "alias",
            Decl::Unreadable(_) => "unreadable",
        })
        .collect();
    assert_eq!(
        shapes,
        vec![
            "text",
            "text",
            "text",
            "alias",
            "unreadable",
            "unreadable",
            "unreadable"
        ],
        "classified: {decls:?}"
    );

    let Decl::Text(first) = &decls[0] else {
        panic!("the first is a raw string")
    };
    assert_eq!(first, "# raw\nkey = 1\n");
    let Decl::Text(hashed) = &decls[1] else {
        panic!("the second is a `r##` raw string")
    };
    assert_eq!(
        hashed, "# hashed \"#\" inside\n",
        "the `\"#` inside an r## literal does not end it"
    );
}

/// Every `DEFAULT_TOML` string literal in the workspace, as
/// `(path relative to the workspace root, its text)`.
///
/// Deliberately a source scan and not a dependency: see this file's own docs.
/// It matches a `DEFAULT_TOML` **declaration** — the name, a type ascription
/// and an `=` on one line — so it steps past every mention of the name in
/// prose without needing to know about any of them.
///
/// **Every declaration it sees is accounted for** (#1380 review, MEDIUM 2):
/// one that is not a raw string and not an alias is reported as unreadable
/// and fails the caller by name, rather than being skipped in silence. The
/// first cut recognised `r#"…"#` alone, so a family spelt with a plain
/// `"…"` literal, a `concat!` or an `include_str!` would have left the scan
/// finding four families, passing, and covering the new one not at all — a
/// scan that can go quiet is worth less than no scan.
fn documented_defaults() -> Vec<(String, String)> {
    let (found, unreadable) = scan_workspace();
    assert!(
        unreadable.is_empty(),
        "the scan found DEFAULT_TOML declarations it cannot read: {unreadable:?}\n\
         Spell a documented default as a raw string literal (`r#\"…\"#`) — every family \
         does — or teach `default_toml_decl` the spelling in the same commit, so this \
         check keeps covering every family in the tree."
    );
    found
}

/// The walk: `(readable declarations, ones that could not be read)`.
fn scan_workspace() -> (Vec<(String, String)>, Vec<String>) {
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
    let mut unreadable = Vec::new();
    for source in sources {
        let body = std::fs::read_to_string(&source).expect("a Rust source this walk just listed");
        let name = source
            .strip_prefix(&root)
            .expect("the walk started at the root")
            .to_string_lossy()
            .into_owned();
        for decl in default_toml_decls(&body) {
            match decl {
                Decl::Text(text) => out.push((name.clone(), text)),
                // `const DEFAULT_TOML: &'static str = DEFAULT_TOML;` — the
                // shell's families re-export the leaf crate's const, so the
                // text is covered where it is written. Not a gap.
                Decl::Alias => {}
                Decl::Unreadable(line) => unreadable.push(format!("{name}: {line}")),
            }
        }
    }
    out.sort();
    (out, unreadable)
}

/// What one `DEFAULT_TOML` declaration's right-hand side turned out to be.
#[derive(Debug)]
enum Decl {
    /// A raw string literal — the text of a documented default.
    Text(String),
    /// Another const by name or path: a re-export, whose text is scanned
    /// wherever it is actually written.
    Alias,
    /// Anything else, carrying the line so the failure can name it.
    Unreadable(String),
}

/// `text`'s lines, each terminated with exactly one `\n` — what
/// [`commented_default`] normalises to, so the round trip above is a
/// statement about commenting rather than about line endings (#1380 review,
/// LOW 3).
fn normalised(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 1);
    for line in text.lines() {
        out.push_str(line);
        out.push('\n');
    }
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

/// Every **published** `DEFAULT_TOML` declaration in one source, classified.
///
/// A declaration is `pub const DEFAULT_TOML: … = …`, at item position on one
/// line. Three things follow from that shape, and each is deliberate:
///
/// * `pub` (or `pub(…)`) is what tells a *family* from a restatement. Every
///   family publishes its documented default, because the schema walker, the
///   control center and these tests all reach it by path; an
///   `impl Subsystem`'s own `const DEFAULT_TOML` member is either an alias of
///   one of those or a test fixture's inline string, and neither is a source
///   of text this scan should be reading.
/// * the visibility and `const` must be the whole of the line before the
///   name, so a declaration quoted *inside a string* — as this file's own
///   self-test does — is not mistaken for one.
/// * whatever follows the `=` is **classified, not matched**, which is the
///   #1380 MEDIUM 2 fix: a spelling this scan does not understand is
///   reported rather than skipped.
fn default_toml_decls(body: &str) -> Vec<Decl> {
    const NAME: &str = "DEFAULT_TOML";
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find(NAME) {
        let line_start = rest[..at].rfind('\n').map_or(0, |nl| nl + 1);
        let prefix = &rest[line_start..at];
        let after_name = &rest[at + NAME.len()..];
        rest = after_name;

        if !is_published_const(prefix) {
            continue;
        }
        let line = after_name.split('\n').next().unwrap_or_default();
        let Some(eq) = line.find('=') else { continue };
        if !line[..eq].contains(':') {
            continue;
        }
        let whole_line = format!("{prefix}{NAME}{line}");

        let after = after_name[eq + 1..].trim_start();
        match raw_string(after) {
            Some((text, consumed)) => {
                out.push(Decl::Text(text));
                rest = &after[consumed..];
            }
            // A bare path (`other::DEFAULT_TOML;`) is a re-export, and the
            // text is scanned where it is written. Anything else — a plain
            // `"…"`, a `concat!`, an `include_str!` — this scan cannot read,
            // and says so instead of going quiet.
            None if is_path_alias(after) => out.push(Decl::Alias),
            None => out.push(Decl::Unreadable(whole_line.trim().to_owned())),
        }
    }
    out
}

/// Whether everything on the line before the name is exactly a visibility and
/// `const` — `pub const `, `pub(super) const `, `pub(crate) const `.
fn is_published_const(prefix: &str) -> bool {
    let Some(head) = prefix.trim().strip_suffix("const") else {
        return false;
    };
    let head = head.trim();
    head == "pub" || (head.starts_with("pub(") && head.ends_with(')'))
}

/// A Rust raw string literal at the head of `src`, as `(contents, bytes
/// consumed)`. Raw because a documented default is full of quotes; every
/// family in the tree is spelt this way.
fn raw_string(src: &str) -> Option<(String, usize)> {
    let hashes = src.strip_prefix('r')?;
    let count = hashes.len() - hashes.trim_start_matches('#').len();
    let open = hashes[count..].strip_prefix('"')?;
    let close = format!("\"{}", "#".repeat(count));
    let end = open.find(&close)?;
    let consumed = src.len() - open.len() + end + close.len();
    Some((open[..end].to_owned(), consumed))
}

/// Whether `src` starts with an identifier or `::` path that ends the
/// statement — i.e. this declaration is another const under a new name.
fn is_path_alias(src: &str) -> bool {
    let path: &str = src
        .split(';')
        .next()
        .map(str::trim_end)
        .unwrap_or_default();
    !path.is_empty()
        && src.len() > path.len() // a `;` really did follow
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}
