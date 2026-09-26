//! `$XDG_CONFIG_HOME/trollshell/plugin-settings.toml` — the values behind the
//! settings plugins declare in their manifest (#1410).
//!
//! A plugin names the environment variables it reads
//! (`hytte_plugin_proto::manifest::Setting`), the control-center's Plugins tab
//! draws a form from that, and what a person saves lands here, one table per
//! plugin **instance id** — the `programs.trollshell.plugins.<id>` attribute
//! name, not the binary, because one binary can run twice (`stats` on the bar
//! and on the sidebar):
//!
//! ```toml
//! [vibectl]
//! V1BECTL_SCREENS = "/home/annika/.config/v1bectl/screens.kdl"
//! V1BECTL_SERVER = "root1-h32.v01d.dfdx.de:31337"
//!
//! [stats-bar]
//! STATS_PER_CORE = true
//! ```
//!
//! The shell's plugin launcher reads it at every launch and passes each value
//! as that variable; the control-center writes it. Two programs, one file —
//! the #640 reason this lives in a GTK-free leaf both of them link, rather than
//! as a reader in one and a writer in the other.
//!
//! # Config, not state
//!
//! #866 decision 3 put what the *shell* writes on a toggle under
//! `$XDG_STATE_HOME` and what a *person* chooses under `$XDG_CONFIG_HOME`. A
//! plugin setting is a choice, so this is config ([`crate::xdg::Env::overlay_path`]),
//! it is hand-editable, and the writer preserves every byte it did not have to
//! change ([`apply`]) — comments, other plugins' tables, key order, quoting.
//!
//! There is no base layer. nix's half of the same knob is the plugin's own
//! `programs.trollshell.plugins.<id>.env`, which already reaches the launcher
//! through `plugins.json` and **wins** over this file on the same variable;
//! the Plugins tab shows such a row read-only as "set in nix". So this module
//! reads one file and merges nothing.
//!
//! # Values are environment text
//!
//! Every value reaches the plugin as a string. A person may still write the
//! natural TOML type — the control-center writes a switch as a boolean and a
//! number as an integer — and [`scalar_text`] is the one spelling of each
//! scalar as environment text (`true`/`false`, decimal). An array or a table
//! has no such spelling, so it is dropped with a warning, costing that key and
//! nothing else — and so is a value no environment can carry
//! ([`value_refusal`]: a NUL byte, or more than [`MAX_VALUE_BYTES`]), which
//! would otherwise make the plugin's whole launch fail (#1415 review M3).
//!
//! This module does not decide which variable names are acceptable: the
//! launcher applies the proto's `Setting::env_refusal` to every key it reads,
//! because the file is hand-editable and must not become a way to set
//! `LD_PRELOAD`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::file::{self, Durability};
use crate::xdg;

/// The file's subsystem name: `plugin-settings.toml` under
/// `$XDG_CONFIG_HOME/trollshell/`.
pub const SUBSYSTEM: &str = "plugin-settings";

/// One plugin's values: environment variable → the text it is passed as.
pub type Values = BTreeMap<String, String>;

/// Every plugin's values, by instance id.
pub type AllValues = BTreeMap<String, Values>;

/// The longest value a setting may carry, in bytes: 32 KiB.
///
/// A value reaches `systemd-run` as an environment string, and the kernel
/// refuses to `execve` with any single one longer than `MAX_ARG_STRLEN`
/// (128 KiB), failing the plugin's **whole** launch rather than the one key.
/// A quarter of that leaves room for the name and for everything else in the
/// environment, and is still far more than any path or address needs.
pub const MAX_VALUE_BYTES: usize = 32 * 1024;

/// Why `value` cannot be passed to a plugin as an environment variable, or
/// `None` when it can.
///
/// One rule for both ends of the file (#1415 review M3): [`parse`] drops such
/// a value with a warning, the shell's launcher checks what it is about to
/// pass again, and the control-center refuses to save one. A NUL is valid
/// TOML (`"\u0000"`) but no environment string can hold it — the spawn fails
/// with `InvalidInput` — and an over-long value makes `execve` fail with
/// `E2BIG`; either would cost the plugin every other value with it.
#[must_use]
pub fn value_refusal(value: &str) -> Option<&'static str> {
    if value.contains('\0') {
        Some("it contains a NUL byte, which no environment variable can hold")
    } else if value.len() > MAX_VALUE_BYTES {
        Some("it is longer than 32 KiB")
    } else {
        None
    }
}

/// `$XDG_CONFIG_HOME/trollshell/plugin-settings.toml`, or `None` when neither
/// `$XDG_CONFIG_HOME` nor `$HOME` is set.
#[must_use]
pub fn path() -> Option<PathBuf> {
    xdg::overlay_path(SUBSYSTEM)
}

/// A TOML scalar as the environment text a plugin receives, or `None` for an
/// array or a table, which have no such spelling.
///
/// Strings pass through; integers and floats are written the way TOML writes
/// them; booleans are `true`/`false`; a datetime is its RFC 3339 text.
#[must_use]
pub fn scalar_text(value: &toml::Value) -> Option<String> {
    match value {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Integer(i) => Some(i.to_string()),
        toml::Value::Float(f) => Some(f.to_string()),
        toml::Value::Boolean(b) => Some(b.to_string()),
        toml::Value::Datetime(d) => Some(d.to_string()),
        toml::Value::Array(_) | toml::Value::Table(_) => None,
    }
}

/// Parse the file's text into every plugin's values.
///
/// Tolerant per key, strict per file. A top-level key that is not a table, or
/// a value that is not a scalar, is dropped with one warning and costs only
/// itself; text that is not TOML at all is an error, and [`load_at`] reads
/// that as "nothing set" rather than guessing.
///
/// # Errors
/// The parse error when `text` is not a TOML document.
pub fn parse(text: &str) -> Result<AllValues, toml::de::Error> {
    let table: toml::Table = toml::from_str(text)?;
    let mut all = AllValues::new();
    for (id, item) in table {
        let toml::Value::Table(entries) = item else {
            tracing::warn!(
                plugin = %id,
                "plugin-settings.toml: a plugin's settings are a [table]; this entry is ignored"
            );
            continue;
        };
        let mut values = Values::new();
        for (key, value) in entries {
            let Some(text) = scalar_text(&value) else {
                tracing::warn!(
                    plugin = %id,
                    %key,
                    "plugin-settings.toml: an array or table cannot be an environment value; ignored"
                );
                continue;
            };
            if let Some(reason) = value_refusal(&text) {
                tracing::warn!(
                    plugin = %id,
                    %key,
                    reason,
                    "plugin-settings.toml: this value cannot be passed to the plugin; ignored"
                );
                continue;
            }
            values.insert(key, text);
        }
        all.insert(id, values);
    }
    Ok(all)
}

/// Read and [`parse`] the file at `path`.
///
/// A missing file is "nothing set", silently. A file that cannot be read or
/// does not parse is also "nothing set", with a warning naming it: the
/// launcher must still start every plugin, and falling back to each plugin's
/// own defaults is the documented behaviour of an unset variable.
#[must_use]
pub fn load_at(path: &Path) -> AllValues {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return AllValues::new(),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "plugin-settings.toml unreadable; no plugin settings applied");
            return AllValues::new();
        }
    };
    parse(&text).unwrap_or_else(|e| {
        tracing::warn!(path = %path.display(), error = %e, "plugin-settings.toml does not parse; no plugin settings applied");
        AllValues::new()
    })
}

/// Why a [`save_at`] wrote nothing.
#[derive(Debug)]
pub enum SaveError {
    /// The existing file could not be read.
    Read(std::io::Error),
    /// The existing file is not TOML. A hand edit left it broken, and
    /// rewriting it from scratch would throw that edit away, so the save is
    /// refused and the file left exactly as it is.
    Parse(toml_edit::TomlError),
    /// The atomic replace failed.
    Write(std::io::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Read(e) => write!(f, "could not read plugin-settings.toml: {e}"),
            SaveError::Parse(e) => write!(
                f,
                "plugin-settings.toml does not parse, so it was left as it is; fix it by hand first: {e}"
            ),
            SaveError::Write(e) => write!(f, "could not write plugin-settings.toml: {e}"),
        }
    }
}

impl std::error::Error for SaveError {}

/// Set or remove some of plugin `id`'s values in `text`, touching nothing
/// else, and return the new text.
///
/// Each change is `(variable, value)`: `Some` sets it, `None` removes it, and
/// so does a `Some` empty **string** — an emptied field means "unset", which
/// is what hands the variable back to the plugin's own default. A value equal
/// to the one already there is left alone byte for byte, quoting and trailing
/// comment included; a changed one keeps its position and its comment. A key
/// the change list does not name — one the person added by hand, or a
/// setting a newer plugin no longer declares — is never touched.
///
/// The plugin's table is created at the end of the document when it is
/// missing, is edited in place whether it is spelled `[id]` or
/// `id = { … }`, and is removed once it holds nothing.
///
/// # Errors
/// The parse error when `text` is not TOML; see [`SaveError::Parse`].
pub fn apply(
    text: &str,
    id: &str,
    changes: &[(String, Option<toml_edit::Value>)],
) -> Result<String, toml_edit::TomlError> {
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    let is_table = doc
        .get(id)
        .is_some_and(|item| item.as_table_like().is_some());
    if !is_table {
        let wants_something = changes.iter().any(|(_, v)| v.as_ref().is_some_and(is_set));
        if !wants_something {
            return Ok(doc.to_string());
        }
        // Absent, or something that is not a table (and so was never read as
        // settings anyway): a fresh standard table.
        doc.insert(id, toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let table = doc
        .get_mut(id)
        .and_then(toml_edit::Item::as_table_like_mut)
        .expect("ensured to be a table above");
    for (key, value) in changes {
        match value.as_ref().filter(|v| is_set(v)) {
            None => {
                table.remove(key);
            }
            Some(want) => match table.get_mut(key).and_then(toml_edit::Item::as_value_mut) {
                Some(have) if same_value(have, want) => {}
                Some(have) => {
                    let decor = have.decor().clone();
                    *have = want.clone();
                    *have.decor_mut() = decor;
                }
                None => {
                    table.insert(key, toml_edit::Item::Value(want.clone()));
                }
            },
        }
    }
    if table.is_empty() {
        doc.remove(id);
    }
    Ok(doc.to_string())
}

/// [`apply`] to the file at `path` and replace it atomically — a write only
/// when the text actually changed.
///
/// A missing file is an empty document. The write is
/// [`Durability::FsyncParent`], the `places.toml` choice: a person pressed
/// Save and was told it worked, so losing it to a power cut would be data
/// loss, not a lost toggle.
///
/// # Errors
/// See [`SaveError`]; the file is untouched on every error.
pub fn save_at(
    path: &Path,
    id: &str,
    changes: &[(String, Option<toml_edit::Value>)],
) -> Result<(), SaveError> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(SaveError::Read(e)),
    };
    let next = apply(&existing, id, changes).map_err(SaveError::Parse)?;
    if next == existing {
        return Ok(());
    }
    file::write_atomic(path, &next, Durability::FsyncParent).map_err(SaveError::Write)
}

/// Whether `value` sets anything: every value but an empty string does.
fn is_set(value: &toml_edit::Value) -> bool {
    value.as_str().is_none_or(|s| !s.is_empty())
}

/// Whether two values are the same TOML value, whatever their spelling.
fn same_value(a: &toml_edit::Value, b: &toml_edit::Value) -> bool {
    use toml_edit::Value as V;
    match (a, b) {
        (V::String(a), V::String(b)) => a.value() == b.value(),
        (V::Integer(a), V::Integer(b)) => a.value() == b.value(),
        (V::Float(a), V::Float(b)) => a.value().to_bits() == b.value().to_bits(),
        (V::Boolean(a), V::Boolean(b)) => a.value() == b.value(),
        (V::Datetime(a), V::Datetime(b)) => a.value() == b.value(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(key: &str, value: impl Into<toml_edit::Value>) -> (String, Option<toml_edit::Value>) {
        (key.to_owned(), Some(value.into()))
    }

    fn unset(key: &str) -> (String, Option<toml_edit::Value>) {
        (key.to_owned(), None)
    }

    // ── reading ──────────────────────────────────────────────────────────

    #[test]
    fn scalars_become_environment_text() {
        let all = parse("[p]\nS = \"x y\"\nI = -3\nF = 1.5\nB = true\nD = 2026-09-26T10:00:00Z\n")
            .expect("parses");
        let p = &all["p"];
        assert_eq!(p["S"], "x y");
        assert_eq!(p["I"], "-3");
        assert_eq!(p["F"], "1.5");
        assert_eq!(p["B"], "true");
        assert_eq!(p["D"], "2026-09-26T10:00:00Z");
    }

    #[test]
    fn a_non_scalar_costs_only_its_own_key() {
        let all =
            parse("stray = 1\n[p]\nA = [1, 2]\nT = { x = 1 }\nOK = \"kept\"\n").expect("parses");
        assert!(
            !all.contains_key("stray"),
            "a top-level scalar is not a plugin"
        );
        assert_eq!(
            all["p"],
            Values::from([("OK".to_owned(), "kept".to_owned())])
        );
    }

    /// #1415 review M3: a NUL (valid TOML) or an over-long value costs its
    /// own key, never its neighbours — the launcher would otherwise fail the
    /// plugin's whole spawn.
    #[test]
    fn a_value_no_environment_can_carry_costs_only_its_own_key() {
        let long = "x".repeat(MAX_VALUE_BYTES + 1);
        let text = format!("[p]\nNUL = \"a\\u0000b\"\nLONG = \"{long}\"\nOK = \"fine\"\n");
        assert_eq!(
            parse(&text).expect("parses")["p"],
            Values::from([("OK".to_owned(), "fine".to_owned())])
        );
        let at_cap = "x".repeat(MAX_VALUE_BYTES);
        assert_eq!(value_refusal(&at_cap), None, "the cap is inclusive");
        assert!(value_refusal(&long).is_some());
        assert!(value_refusal("a\0b").is_some());
        assert_eq!(value_refusal(""), None);
    }

    #[test]
    fn text_that_is_not_toml_is_an_error_and_loads_as_nothing() {
        assert!(parse("[p\nA = ").is_err());
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings.toml");
        std::fs::write(&path, "[p\nA = ").expect("write");
        assert!(load_at(&path).is_empty());
    }

    #[test]
    fn a_missing_file_is_nothing_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load_at(&dir.path().join("absent.toml")).is_empty());
    }

    #[test]
    fn the_path_is_the_config_overlay_not_state() {
        let env = xdg::Env {
            home: Some("/home/u".into()),
            config_home: Some("/cfg".into()),
            config_dirs: None,
            state_home: Some("/state".into()),
        };
        assert_eq!(
            env.overlay_path(SUBSYSTEM),
            Some(PathBuf::from("/cfg/trollshell/plugin-settings.toml"))
        );
    }

    // ── writing ──────────────────────────────────────────────────────────

    #[test]
    fn a_first_save_creates_the_plugins_table() {
        let out = apply("", "vibectl", &[set("V1BECTL_SCREENS", "/s.kdl")]).expect("ok");
        assert_eq!(out, "[vibectl]\nV1BECTL_SCREENS = \"/s.kdl\"\n");
        assert_eq!(
            parse(&out).expect("parses")["vibectl"]["V1BECTL_SCREENS"],
            "/s.kdl"
        );
    }

    #[test]
    fn everything_the_change_does_not_name_keeps_its_bytes() {
        let before = "\
# my plugin settings
[pet]
PET_NAME = 'nisse'   # literal quotes, on purpose

[vibectl]
# where the layout lives
V1BECTL_SCREENS = \"/old.kdl\" # comment stays
HAND_ADDED = \"x\"
";
        let after = apply(before, "vibectl", &[set("V1BECTL_SCREENS", "/new.kdl")]).expect("ok");
        assert_eq!(
            after,
            before.replace("\"/old.kdl\"", "\"/new.kdl\""),
            "only the one value's text moved"
        );
    }

    #[test]
    fn an_unchanged_value_keeps_its_spelling() {
        let before = "[pet]\nPET_NAME = 'nisse' # hi\nN = 0x10\n";
        let after =
            apply(before, "pet", &[set("PET_NAME", "nisse"), set("N", 16_i64)]).expect("ok");
        assert_eq!(after, before);
    }

    #[test]
    fn typed_values_are_written_as_their_toml_type() {
        let out = apply(
            "",
            "stats-bar",
            &[set("STATS_PER_CORE", true), set("STATS_COLUMNS", 4_i64)],
        )
        .expect("ok");
        assert_eq!(
            out,
            "[stats-bar]\nSTATS_PER_CORE = true\nSTATS_COLUMNS = 4\n"
        );
    }

    #[test]
    fn none_and_an_empty_string_both_remove_the_key() {
        let before = "[p]\nA = \"1\"\nB = \"2\"\nC = \"3\"\n";
        let after = apply(before, "p", &[unset("A"), set("B", "")]).expect("ok");
        assert_eq!(after, "[p]\nC = \"3\"\n");
    }

    #[test]
    fn a_table_left_empty_is_removed() {
        let before = "[other]\nX = \"1\"\n\n[p]\nA = \"1\"\n";
        let after = apply(before, "p", &[unset("A")]).expect("ok");
        assert_eq!(after, "[other]\nX = \"1\"\n");
    }

    #[test]
    fn clearing_a_plugin_with_no_table_writes_nothing_new() {
        let before = "[other]\nX = \"1\"\n";
        assert_eq!(
            apply(before, "p", &[unset("A"), set("B", "")]).expect("ok"),
            before
        );
    }

    #[test]
    fn an_inline_table_is_edited_in_place() {
        let before = "p = { A = \"1\", B = \"2\" }\n";
        let after = apply(before, "p", &[set("A", "9")]).expect("ok");
        assert_eq!(parse(&after).expect("parses")["p"]["A"], "9");
        assert!(after.starts_with("p = {"), "still inline: {after}");
    }

    #[test]
    fn a_broken_file_is_refused_and_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("plugin-settings.toml");
        std::fs::write(&path, "[p\nA = ").expect("write");
        let err = save_at(&path, "p", &[set("A", "1")]).expect_err("refused");
        assert!(matches!(err, SaveError::Parse(_)), "{err}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "[p\nA = ");
    }

    #[test]
    fn save_round_trips_through_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("plugin-settings.toml");
        save_at(
            &path,
            "vibectl",
            &[set("V1BECTL_SCREENS", "/s.kdl"), set("V1BECTL_DEBUG", true)],
        )
        .expect("saved");
        let all = load_at(&path);
        assert_eq!(all["vibectl"]["V1BECTL_SCREENS"], "/s.kdl");
        assert_eq!(all["vibectl"]["V1BECTL_DEBUG"], "true");

        // A no-op save does not rewrite the file.
        let before = std::fs::metadata(&path)
            .expect("stat")
            .modified()
            .expect("mtime");
        std::thread::sleep(std::time::Duration::from_millis(20));
        save_at(&path, "vibectl", &[set("V1BECTL_SCREENS", "/s.kdl")]).expect("saved");
        let after = std::fs::metadata(&path)
            .expect("stat")
            .modified()
            .expect("mtime");
        assert_eq!(before, after, "an unchanged save writes nothing");
    }
}
