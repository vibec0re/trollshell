//! The schema shape: what a subsystem declares, and what it gets for free
//! (#868).
//!
//! `places.toml` already had the three pieces a layered config needs — a
//! schema, validation, and a format-preserving writer so its two editors agree
//! byte for byte (#640/#703). Writing those three again per subsystem is the
//! expensive half of #866, and writing none of them means silent typos. This
//! module is the generalisation: a subsystem declares its type, its file name
//! and its documented default, and inherits the reader, the validator harness
//! and the writer.
//!
//! ```no_run
//! use hytte_config::subsystem::{Subsystem, load_or_default};
//!
//! #[derive(serde::Serialize, serde::Deserialize)]
//! struct CoreLeds {
//!     #[serde(default)]
//!     color: String,
//! }
//!
//! impl Subsystem for CoreLeds {
//!     const NAME: &'static str = "core-leds";
//!     const DEFAULT_TOML: &'static str = "# the core LED strip\ncolor = \"amber\"\n";
//!     type Error = std::convert::Infallible;
//!     fn validate(&self) -> Result<(), Self::Error> { Ok(()) }
//! }
//!
//! let leds: Option<CoreLeds> = load_or_default::<CoreLeds>();
//! ```
//!
//! # The layers, bottom to top
//!
//! 1. [`Subsystem::DEFAULT_TOML`] — the built-in default, **as TOML**, so the
//!    loader has one parse path and the documented defaults and the effective
//!    ones cannot drift. `places` established this and it is why its shipped
//!    default is parse-tested rather than mirrored in Rust.
//! 2. each `$XDG_CONFIG_DIRS/trollshell/<NAME>.toml`, least important first.
//! 3. `$XDG_CONFIG_HOME/trollshell/<NAME>.toml` — the overlay.
//!
//! Merged by [`crate::merge`]'s four rules; see [`crate::xdg`] for why the
//! base directories are reversed on the way in.
//!
//! # Unknown key vs. wrong type
//!
//! The fourth merge rule — *unknown keys warn, never fail* — is enforced here
//! rather than in [`crate::merge`], because "unknown" is a property of the
//! schema and the merge layer has never seen one. A key the type does not have
//! is collected into [`Loaded::unknown_keys`], logged, and otherwise ignored.
//! A key it *does* have, carrying a value of the wrong type, is a
//! [`ConfigError::Schema`]: the user asked for something specific and got
//! nothing, and silently substituting a default would be the invisible-failure
//! mode #641 taught this repo to avoid. Neither takes the shell down —
//! [`load_or_default`] degrades to the built-in default with a loud `error!`.
//!
//! # Why the writer patches instead of re-rendering
//!
//! Same reason as `places` (#703): once the control center can edit a file a
//! person also hand-edits, a save that re-renders eats every comment, every
//! hand-chosen key order, and every key the model does not know about — once,
//! silently, permanently. [`render_overlay`] therefore edits the parsed
//! document, assigning only the keys whose value actually moved.
//!
//! It is a *generalisation of the idea*, not a replacement for
//! [`crate::places::render_places`]: that one additionally aligns
//! `[[place]]` array-of-tables entries across an edit, which is specific to a
//! keyed collection and has no meaning for the flat `[section] key = value`
//! shape the env-migration subsystems have. `places` keeps its own writer and
//! is untouched by this module.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::de::IntoDeserializer as _;

use crate::file::{self, Durability};
use crate::{merge, xdg};

/// What a subsystem declares to get the reader, the validator and the writer.
pub trait Subsystem: serde::de::DeserializeOwned {
    /// File stem: the config is `<NAME>.toml` in each layer, and the state
    /// file (if any) is `<NAME>.toml` under `$XDG_STATE_HOME/trollshell`.
    /// Kebab-case, matching the subsystem's name in `programs.trollshell.*`.
    const NAME: &'static str;

    /// The documented default, **as TOML**, used as the bottom merge layer and
    /// as the seed for a first-ever overlay write.
    ///
    /// Keep it commented: it is the only place a key is explained, it is what
    /// a user sees when they first open their overlay, and it is parsed on
    /// every load, so a syntax error in it is caught by any test that loads
    /// the subsystem rather than in production.
    const DEFAULT_TOML: &'static str;

    /// This subsystem's validation error. `std::convert::Infallible` when
    /// there is nothing the type system did not already catch.
    type Error: std::fmt::Display;

    /// Semantic checks the schema cannot express — the equivalent of
    /// `places::validate`'s latitude bounds and duplicate names.
    ///
    /// Runs after deserialisation on load, and again before a save, so a
    /// config that would be rejected on read is never written.
    ///
    /// # Errors
    /// Whatever the subsystem considers unusable.
    fn validate(&self) -> Result<(), Self::Error>;
}

/// A loaded subsystem config, plus what the load learned on the way.
#[derive(Clone, Debug)]
pub struct Loaded<S> {
    /// The merged, validated config.
    pub config: S,
    /// Layer files that existed and contributed, lowest precedence first.
    /// `DEFAULT_TOML` is not listed — it is not a file.
    pub sources: Vec<PathBuf>,
    /// Dotted paths of keys no layer's schema knows. Warned, never fatal;
    /// returned as well so a settings UI can surface them.
    pub unknown_keys: Vec<String>,
}

/// Why a layered load or save failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// A layer exists but is not valid TOML. `path` is `None` for
    /// [`Subsystem::DEFAULT_TOML`], which means the bug is ours.
    Parse {
        /// The offending layer, or `None` for the built-in default.
        path: Option<PathBuf>,
        /// The parser's message.
        message: String,
    },
    /// A layer exists but could not be read: permissions, or non-UTF-8.
    /// Deliberately not the same as "absent" — falling through to the layer
    /// below would present base behaviour as if it were the user's.
    Unreadable {
        /// The offending layer.
        path: PathBuf,
        /// The I/O error.
        message: String,
    },
    /// The merged config does not fit the schema — a known key with a value of
    /// the wrong type. An *unknown* key is not this; see the module docs.
    Schema(String),
    /// [`Subsystem::validate`] rejected the merged config.
    Invalid(String),
    /// Nowhere to write: neither `$XDG_CONFIG_HOME` nor `$HOME` is set.
    NoOverlayPath,
    /// The config could not be rendered back to TOML.
    Encode(String),
    /// The atomic write failed; the previous overlay is untouched.
    Write(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse { path, message } => match path {
                Some(path) => write!(f, "{} is not valid TOML: {message}", path.display()),
                None => write!(
                    f,
                    "the built-in default config is not valid TOML: {message}"
                ),
            },
            Self::Unreadable { path, message } => {
                write!(f, "{} could not be read ({message})", path.display())
            }
            Self::Schema(e) => write!(f, "config does not fit the schema: {e}"),
            Self::Invalid(e) => write!(f, "config is not usable: {e}"),
            Self::NoOverlayPath => write!(
                f,
                "cannot locate the config overlay: neither $XDG_CONFIG_HOME nor $HOME is set"
            ),
            Self::Encode(e) => write!(f, "could not render the config: {e}"),
            Self::Write(e) => write!(
                f,
                "could not write the config ({e}); the previous one is unchanged"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Parse one layer body, naming the file in the error.
fn parse_layer(body: &str, path: Option<&Path>) -> Result<toml::Table, ConfigError> {
    body.parse::<toml::Table>().map_err(|e| ConfigError::Parse {
        path: path.map(Path::to_path_buf),
        message: e.to_string(),
    })
}

/// The pure core of [`load`]: [`Subsystem::DEFAULT_TOML`] plus every layer
/// body (**lowest precedence first**), merged, checked against the schema and
/// validated. No I/O, so every rule above is unit-testable.
///
/// # Errors
/// [`ConfigError::Parse`] for a layer that is not TOML, [`ConfigError::Schema`]
/// for a known key of the wrong type, [`ConfigError::Invalid`] when
/// [`Subsystem::validate`] rejects the result. An *unknown* key is none of
/// these — it is warned and reported in [`Loaded::unknown_keys`].
pub fn assemble<S: Subsystem>(layers: &[(PathBuf, String)]) -> Result<Loaded<S>, ConfigError> {
    let mut tables = Vec::with_capacity(layers.len() + 1);
    tables.push(parse_layer(S::DEFAULT_TOML, None)?);
    for (path, body) in layers {
        tables.push(parse_layer(body, Some(path))?);
    }
    let merged = merge::merge_all(tables);

    let mut unknown_keys = Vec::new();
    let config: S = serde_ignored::deserialize(merged.into_deserializer(), |path| {
        unknown_keys.push(path.to_string());
    })
    .map_err(|e| ConfigError::Schema(e.to_string()))?;

    // Rule 4: loud, but never fatal. A typo must be visible and must not take
    // the shell down.
    for key in &unknown_keys {
        tracing::warn!(
            subsystem = S::NAME,
            key,
            "unknown key in config; ignoring it"
        );
    }

    config
        .validate()
        .map_err(|e| ConfigError::Invalid(e.to_string()))?;

    Ok(Loaded {
        config,
        sources: layers.iter().map(|(path, _)| path.clone()).collect(),
        unknown_keys,
    })
}

/// Read the layer files that exist, lowest precedence first.
///
/// A **missing** layer is not an error — most layers are absent most of the
/// time, and "no overlay" is the normal case. An **unreadable** one is,
/// because quietly dropping it would show base behaviour as though it were the
/// user's.
fn read_layers(paths: &[PathBuf]) -> Result<Vec<(PathBuf, String)>, ConfigError> {
    let mut out = Vec::new();
    for path in paths {
        match std::fs::read_to_string(path) {
            Ok(body) => out.push((path.clone(), body)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(ConfigError::Unreadable {
                    path: path.clone(),
                    message: e.to_string(),
                });
            }
        }
    }
    Ok(out)
}

/// [`assemble`] over explicit layer paths, lowest precedence first — the shape
/// [`crate::xdg::Env::config_layers`] returns. Missing paths are skipped.
///
/// # Errors
/// As [`assemble`], plus [`ConfigError::Unreadable`] for a layer that exists
/// but cannot be read.
pub fn load_from<S: Subsystem>(paths: &[PathBuf]) -> Result<Loaded<S>, ConfigError> {
    assemble::<S>(&read_layers(paths)?)
}

/// Load `S` from the process environment's XDG search path.
///
/// # Errors
/// As [`load_from`].
pub fn load<S: Subsystem>() -> Result<Loaded<S>, ConfigError> {
    load_from::<S>(&xdg::config_layers(S::NAME))
}

/// [`load`], degrading to [`Subsystem::DEFAULT_TOML`] alone on any failure,
/// with a loud `error!` naming what went wrong.
///
/// This is what a service wants: a broken config file must be visible in the
/// journal and must not stop the shell from starting. `None` only when the
/// built-in default itself does not parse or validate — a bug in the
/// subsystem, not in anyone's config.
#[must_use]
pub fn load_or_default<S: Subsystem>() -> Option<S> {
    match load::<S>() {
        Ok(loaded) => Some(loaded.config),
        Err(e) => {
            tracing::error!(
                subsystem = S::NAME,
                error = %e,
                "config unusable; falling back to the built-in default"
            );
            match assemble::<S>(&[]) {
                Ok(loaded) => Some(loaded.config),
                Err(e) => {
                    tracing::error!(
                        subsystem = S::NAME,
                        error = %e,
                        "the built-in default config is itself unusable"
                    );
                    None
                }
            }
        }
    }
}

// ── The writer: a format-preserving document patch ──────────────────────────

/// `value` as a TOML table.
fn wanted<S: Subsystem + serde::Serialize>(value: &S) -> Result<toml::Table, ConfigError> {
    match toml::Value::try_from(value) {
        Ok(toml::Value::Table(table)) => Ok(table),
        Ok(_) => Err(ConfigError::Encode(
            "a subsystem config must serialise to a table".into(),
        )),
        Err(e) => Err(ConfigError::Encode(e.to_string())),
    }
}

/// Every dotted key path in `table`, tables included.
fn collect_paths(table: &toml::Table, prefix: &str, out: &mut BTreeSet<String>) {
    for (key, value) in table {
        let path = format!("{prefix}{key}");
        if let toml::Value::Table(nested) = value {
            collect_paths(nested, &format!("{path}."), out);
        }
        out.insert(path);
    }
}

/// The dotted paths in `have` that `S`'s schema actually owns — everything
/// [`serde_ignored`] did *not* report as ignored.
///
/// The probe deserialises `have` layered over [`Subsystem::DEFAULT_TOML`], not
/// `have` alone: an overlay legitimately omits keys the schema requires, and
/// probing the bare file would then fail for every such subsystem and disable
/// removal entirely. The default contributes no ignored paths (its keys are
/// ours by construction), so the answer for `have`'s own keys is unchanged —
/// only the probe's ability to complete is.
///
/// `None` when even that does not deserialise, in which case the caller
/// removes nothing: an incomplete ignored-set would name a user's own key as
/// schema-owned and delete it, and deleting somebody's key on uncertain
/// information is the one outcome this writer must never produce.
///
/// A quoted TOML key containing a literal `.` would be indistinguishable from
/// a nesting separator here; no schema in the workspace has one, and the
/// consequence is a key not removed rather than a key wrongly removed.
fn schema_paths<S: Subsystem>(have: &toml::Table) -> Option<BTreeSet<String>> {
    let probe = merge::merge_all([parse_layer(S::DEFAULT_TOML, None).ok()?, have.clone()]);

    let mut ignored = BTreeSet::new();
    let parsed: Result<S, _> = serde_ignored::deserialize(probe.into_deserializer(), |p| {
        ignored.insert(p.to_string());
    });
    parsed.ok()?;

    let mut all = BTreeSet::new();
    collect_paths(have, "", &mut all);
    Some(all.difference(&ignored).cloned().collect())
}

/// Assign `value` to `key`, keeping the comment block written *above* the key
/// and dropping the trailing comment that annotated the old value.
///
/// Replacing the `Item` behind an existing key — rather than re-inserting the
/// key — is what preserves that block. `places::set_value` carries the same
/// two lines and the full argument for why the trailing comment goes (#641: a
/// stale `# S Schöneweide Bhf` beside a station id that is no longer
/// Schöneweide is worse than no comment). The duplication is deliberate:
/// `places`' writer is pinned byte for byte by
/// `tests/places_byte_identical.rs`, and sharing an implementation with a new
/// generic one is exactly how that pin would start moving.
fn set_value(table: &mut toml_edit::Table, key: &str, mut value: toml_edit::Value) {
    value.decor_mut().set_prefix(" ");
    value.decor_mut().set_suffix("");
    if let Some(existing) = table.get_mut(key) {
        *existing = toml_edit::Item::Value(value);
    } else {
        table.insert(key, toml_edit::Item::Value(value));
    }
}

/// `toml::Value` as a `toml_edit::Value`.
///
/// Hand-written rather than routed through `toml_edit::ser::ValueSerializer`:
/// that module is behind `toml_edit`'s `serde` feature, and turning the
/// feature on to convert between two representations of the same TOML would
/// widen the crate's dependency surface for nothing. Tables become *inline*
/// tables, which is the only thing a value position can hold; a `want` entry
/// that is a table is recursed into by [`patch`] and never reaches here.
fn to_edit(value: &toml::Value) -> toml_edit::Value {
    match value {
        toml::Value::String(v) => v.as_str().into(),
        toml::Value::Integer(v) => (*v).into(),
        toml::Value::Float(v) => (*v).into(),
        toml::Value::Boolean(v) => (*v).into(),
        toml::Value::Datetime(v) => (*v).into(),
        toml::Value::Array(items) => toml_edit::Value::Array(items.iter().map(to_edit).collect()),
        toml::Value::Table(table) => {
            let mut inline = toml_edit::InlineTable::new();
            for (key, nested) in table {
                inline.insert(key, to_edit(nested));
            }
            toml_edit::Value::InlineTable(inline)
        }
    }
}

/// One table level of the patch.
///
/// `have` is what the document currently parses to at this level, so an
/// unchanged key can be recognised and left untouched — formatting, inline
/// comment and all.
fn patch(
    doc: &mut toml_edit::Table,
    want: &toml::Table,
    have: &toml::Table,
    owned: Option<&BTreeSet<String>>,
    prefix: &str,
) {
    // Keys the schema owns but the value no longer carries: an `Option` gone
    // to `None`. Everything else in the document — a hand-added annotation, an
    // unrelated table — is not ours to delete.
    if let Some(owned) = owned {
        let stale: Vec<String> = doc
            .iter()
            .map(|(key, _)| key.to_string())
            .filter(|key| !want.contains_key(key) && owned.contains(&format!("{prefix}{key}")))
            .collect();
        for key in stale {
            doc.remove(&key);
        }
    }

    for (key, value) in want {
        if let toml::Value::Table(sub_want) = value {
            if !doc.get(key).is_some_and(toml_edit::Item::is_table) {
                doc.insert(key, toml_edit::Item::Table(toml_edit::Table::new()));
            }
            let Some(sub_doc) = doc.get_mut(key).and_then(toml_edit::Item::as_table_mut) else {
                continue;
            };
            let empty = toml::Table::new();
            let sub_have = have
                .get(key)
                .and_then(toml::Value::as_table)
                .unwrap_or(&empty);
            patch(
                sub_doc,
                sub_want,
                sub_have,
                owned,
                &format!("{prefix}{key}."),
            );
        } else if have.get(key) != Some(value) {
            // Only a key whose value actually moved is rewritten, so an
            // untouched one keeps its exact bytes.
            set_value(doc, key, to_edit(value));
        }
    }
}

/// Patch `value` into the document `existing` holds, rather than re-rendering
/// it. Pure, so both the fidelity and the round trip are unit-testable.
///
/// What survives: the preamble, per-key comments, hand-chosen key ordering,
/// unrelated tables, and any key the schema does not know about. What moves:
/// only the keys whose value actually changed, plus schema-owned keys the
/// value no longer carries.
///
/// A [`crate::merge::UNSET_KEY`] marker in the file is one of the keys the
/// schema does not know, so it survives a save — and stays correct, because a
/// save also writes the key it unset back explicitly, and the merge applies
/// the removal before the assignments. The marker is then redundant rather
/// than wrong.
///
/// # Errors
/// [`ConfigError::Encode`] when `existing` is not valid TOML — refusing rather
/// than replacing bytes we cannot account for — or when `value` does not
/// serialise to a table.
pub fn render_overlay<S: Subsystem + serde::Serialize>(
    existing: &str,
    value: &S,
) -> Result<String, ConfigError> {
    let want = wanted(value)?;
    let mut doc: toml_edit::DocumentMut = existing.parse().map_err(|e: toml_edit::TomlError| {
        ConfigError::Encode(format!("the file being replaced is not valid TOML: {e}"))
    })?;
    let have = parse_layer(existing, None).map_err(|e| ConfigError::Encode(e.to_string()))?;
    let owned = schema_paths::<S>(&have);

    patch(doc.as_table_mut(), &want, &have, owned.as_ref(), "");
    Ok(doc.to_string())
}

/// Write `value` to `path` as this subsystem's overlay, atomically.
///
/// Seeds a not-yet-existing file with [`Subsystem::DEFAULT_TOML`] before
/// patching, so a first save produces the documented, commented file rather
/// than a bare dump of values — the same discoverability `places` gives on
/// first run. Note the consequence: a whole-value save pins every key,
/// including ones that were inherited from a base layer.
///
/// Takes [`Durability::FsyncParent`]: this is user-authored data, written
/// rarely and acknowledged back to the user, so losing an acknowledged save to
/// a power cut would be data loss rather than a lost toggle.
///
/// # Errors
/// [`ConfigError::Invalid`] if `value` would not be accepted back,
/// [`ConfigError::Unreadable`] if the existing file cannot be read (refusing
/// rather than overwriting bytes we cannot account for), plus anything
/// [`render_overlay`] returns and [`ConfigError::Write`] for the replace.
pub fn save_overlay_to<S: Subsystem + serde::Serialize>(
    path: &Path,
    value: &S,
) -> Result<(), ConfigError> {
    value
        .validate()
        .map_err(|e| ConfigError::Invalid(e.to_string()))?;

    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => S::DEFAULT_TOML.to_string(),
        Err(e) => {
            return Err(ConfigError::Unreadable {
                path: path.to_path_buf(),
                message: e.to_string(),
            });
        }
    };

    let body = render_overlay(&existing, value)?;
    file::write_atomic(path, &body, Durability::FsyncParent)
        .map_err(|e| ConfigError::Write(e.to_string()))
}

/// [`save_overlay_to`] against `$XDG_CONFIG_HOME/trollshell/<NAME>.toml` — the
/// only config layer anything in this workspace may write to. The
/// `XDG_CONFIG_DIRS` layers are nix's, and on NixOS are a read-only store path.
///
/// # Errors
/// [`ConfigError::NoOverlayPath`] when there is nowhere to write, plus
/// anything [`save_overlay_to`] returns.
pub fn save_overlay<S: Subsystem + serde::Serialize>(value: &S) -> Result<(), ConfigError> {
    let path = xdg::overlay_path(S::NAME).ok_or(ConfigError::NoOverlayPath)?;
    save_overlay_to(&path, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A subsystem in the shape the env migration will produce: a documented
    /// TOML default, a nested table, an array, an optional key, and one
    /// semantic rule the type system cannot express.
    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Leds {
        #[serde(default)]
        enabled: bool,
        #[serde(default)]
        core: Core,
    }

    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Core {
        #[serde(default)]
        color: String,
        #[serde(default)]
        brightness: u8,
        #[serde(default)]
        palette: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    }

    impl Default for Core {
        fn default() -> Self {
            Self {
                color: "amber".into(),
                brightness: 3,
                palette: Vec::new(),
                label: None,
            }
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct TooBright(u8);

    impl std::fmt::Display for TooBright {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "brightness {} is above the maximum of 8", self.0)
        }
    }

    impl Subsystem for Leds {
        const NAME: &'static str = "core-leds";
        const DEFAULT_TOML: &'static str = DEFAULT;
        type Error = TooBright;
        fn validate(&self) -> Result<(), Self::Error> {
            if self.core.brightness > 8 {
                return Err(TooBright(self.core.brightness));
            }
            Ok(())
        }
    }

    const DEFAULT: &str = r#"# The per-core LED strip.
enabled = true

[core]
# Strip colour, any CSS name.
color = "amber"
brightness = 3
palette = ["amber", "rust"]
"#;

    fn layers(bodies: &[&str]) -> Vec<(PathBuf, String)> {
        bodies
            .iter()
            .enumerate()
            .map(|(i, body)| {
                (
                    PathBuf::from(format!("/layer/{i}.toml")),
                    (*body).to_string(),
                )
            })
            .collect()
    }

    fn assembled(bodies: &[&str]) -> Loaded<Leds> {
        assemble::<Leds>(&layers(bodies)).expect("assembles")
    }

    #[test]
    fn with_no_layers_the_documented_default_is_the_config() {
        let loaded = assembled(&[]);

        assert_eq!(loaded.config.core.color, "amber");
        assert_eq!(loaded.config.core.brightness, 3);
        assert!(loaded.config.enabled);
        assert!(loaded.sources.is_empty());
    }

    /// The whole layering, end to end: default, then a nix base, then the
    /// user's overlay, with each rule visible in one of the keys.
    #[test]
    fn the_overlay_beats_the_base_which_beats_the_documented_default() {
        let loaded = assembled(&[
            // base, written by nix
            "[core]\ncolor = \"cyan\"\nbrightness = 5\n",
            // overlay, the user's
            "[core]\nbrightness = 7\npalette = [\"teal\"]\n",
        ]);

        assert_eq!(loaded.config.core.brightness, 7, "overlay wins");
        assert_eq!(
            loaded.config.core.color, "cyan",
            "a key only the base states falls through the overlay"
        );
        assert_eq!(
            loaded.config.core.palette,
            ["teal"],
            "arrays replace: the default's two entries are gone"
        );
        assert!(loaded.config.enabled, "the default's own key still applies");
        assert_eq!(loaded.sources.len(), 2);
    }

    /// **Rule 4.** A typo is loud and harmless: reported, logged, and the rest
    /// of the config still loads.
    ///
    /// Red if `serde_ignored` is swapped back for a plain `toml::from_str`
    /// (nothing is reported), and red if the schema ever grows
    /// `deny_unknown_fields` (the whole load fails).
    #[test]
    fn an_unknown_key_is_reported_and_does_not_fail_the_load() {
        let loaded = assembled(&["[core]\ncolour = \"cyan\"\nbrightness = 7\n"]);

        assert_eq!(
            loaded.unknown_keys,
            ["core.colour"],
            "the typo is named, with its full path"
        );
        assert_eq!(
            loaded.config.core.brightness, 7,
            "the keys around the typo still apply"
        );
        assert_eq!(
            loaded.config.core.color, "amber",
            "and the misspelt one keeps its default"
        );
    }

    /// The other half of rule 4: a *known* key with the wrong type is not a
    /// typo, and is not silently defaulted.
    #[test]
    fn a_known_key_of_the_wrong_type_is_an_error_not_an_ignored_key() {
        let err = assemble::<Leds>(&layers(&["[core]\nbrightness = \"loud\"\n"]))
            .expect_err("a type error must surface");

        assert!(matches!(err, ConfigError::Schema(_)), "got {err:?}");
    }

    #[test]
    fn validate_runs_on_the_merged_result() {
        let err = assemble::<Leds>(&layers(&["[core]\nbrightness = 9\n"]))
            .expect_err("9 is above the maximum");

        assert_eq!(
            err.to_string(),
            "config is not usable: brightness 9 is above the maximum of 8"
        );
    }

    /// A broken layer says *which file* — with three or four candidate paths
    /// in play, an unattributed parse error is close to useless.
    #[test]
    fn a_layer_that_is_not_toml_names_the_file() {
        let err =
            assemble::<Leds>(&layers(&["this is not toml"])).expect_err("a broken layer fails");

        let ConfigError::Parse { path, .. } = &err else {
            panic!("expected a parse error, got {err:?}");
        };
        assert_eq!(path.as_deref(), Some(Path::new("/layer/0.toml")));
        assert!(
            err.to_string()
                .starts_with("/layer/0.toml is not valid TOML:"),
            "the message must lead with the path: {err}"
        );
    }

    #[test]
    fn load_from_skips_missing_layers_and_refuses_an_unreadable_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.toml");
        let present = dir.path().join("base.toml");
        std::fs::write(&present, "[core]\nbrightness = 6\n").expect("seed");

        let loaded = load_from::<Leds>(&[missing.clone(), present.clone()])
            .expect("a missing layer is fine");
        assert_eq!(loaded.config.core.brightness, 6);
        assert_eq!(loaded.sources, [present]);

        // A directory where a file should be is unreadable for every uid.
        let wedged = dir.path().join("wedged.toml");
        std::fs::create_dir(&wedged).expect("mkdir");
        let err =
            load_from::<Leds>(&[wedged]).expect_err("an unreadable layer must not be skipped");
        assert!(matches!(err, ConfigError::Unreadable { .. }), "got {err:?}");
    }

    // ── the writer ──────────────────────────────────────────────────────────

    const HAND_EDITED: &str = r#"# My LEDs.
enabled = true

[core]
# Strip colour, any CSS name.
color = "amber" # warm
brightness = 3
palette = ["amber", "rust"]
label = "desk"
mystery = 42

[unrelated]
kept = true
"#;

    fn config_from(text: &str) -> Leds {
        assemble::<Leds>(&layers(&[text]))
            .expect("assembles")
            .config
    }

    #[test]
    fn a_save_that_changes_nothing_changes_no_bytes() {
        let value = config_from(HAND_EDITED);

        assert_eq!(
            render_overlay(HAND_EDITED, &value).expect("renders"),
            HAND_EDITED,
            "a no-op save must be a no-op on disk"
        );
    }

    #[test]
    fn a_save_edits_one_key_and_leaves_every_other_byte_alone() {
        let mut value = config_from(HAND_EDITED);
        value.core.brightness = 7;

        let out = render_overlay(HAND_EDITED, &value).expect("renders");

        assert_eq!(
            out,
            HAND_EDITED.replace("brightness = 3", "brightness = 7"),
            "only the one key's bytes may move"
        );
    }

    #[test]
    fn a_save_keeps_the_key_comment_and_drops_the_stale_value_comment() {
        let mut value = config_from(HAND_EDITED);
        value.core.color = "cyan".into();

        let out = render_overlay(HAND_EDITED, &value).expect("renders");

        assert!(
            out.contains("# Strip colour, any CSS name.\ncolor = \"cyan\"\n"),
            "the comment above the key documents the field and survives; the one \
             beside the value described the old value and must not: {out}"
        );
    }

    /// A key the schema does not know, and a whole table it does not know,
    /// must survive a save — this is the guarantee that lets a file stay
    /// hand-editable while a GUI also writes it.
    #[test]
    fn a_save_preserves_keys_and_tables_the_schema_does_not_know() {
        let mut value = config_from(HAND_EDITED);
        value.core.brightness = 8;

        let out = render_overlay(HAND_EDITED, &value).expect("renders");

        assert!(out.contains("mystery = 42"), "{out}");
        assert!(out.contains("[unrelated]\nkept = true"), "{out}");
        assert!(out.starts_with("# My LEDs."), "{out}");
    }

    /// The `Option` case: a schema key the value no longer carries is removed,
    /// which is the one thing a pure "assign what you have" patcher cannot do.
    ///
    /// Red if the `stale` block in [`patch`] is deleted (`label` survives).
    #[test]
    fn a_save_removes_a_schema_key_the_value_no_longer_carries() {
        let mut value = config_from(HAND_EDITED);
        value.core.label = None;

        let out = render_overlay(HAND_EDITED, &value).expect("renders");

        assert!(!out.contains("label"), "the unset key must go: {out}");
        assert!(
            out.contains("mystery = 42"),
            "but a key we do not own must not be swept up with it: {out}"
        );
    }

    /// Removal only ever fires on information we trust. When the document does
    /// not deserialise, [`schema_paths`] returns `None` and nothing is deleted.
    #[test]
    fn a_document_that_does_not_deserialise_loses_no_keys() {
        // `brightness` is a string here, so the round trip fails outright.
        let broken = "[core]\nbrightness = \"loud\"\nlabel = \"desk\"\nmystery = 1\n";
        let mut value = config_from(HAND_EDITED);
        value.core.label = None;

        let out = render_overlay(broken, &value).expect("renders anyway");

        assert!(out.contains("mystery = 1"), "{out}");
        assert!(
            out.contains("label"),
            "an uncertain schema view must delete nothing: {out}"
        );
    }

    /// A schema with a **required** field, and an overlay that legitimately
    /// omits it. Removal must still work — probing the bare file would fail to
    /// deserialise here and [`schema_paths`] would give up, leaving `label`
    /// behind forever.
    #[test]
    fn removal_works_for_a_schema_whose_required_key_the_overlay_omits() {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Strict {
            // No `#[serde(default)]`: the default layer is the only thing that
            // supplies this when an overlay does not.
            name: String,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            label: Option<String>,
        }

        impl Subsystem for Strict {
            const NAME: &'static str = "strict";
            const DEFAULT_TOML: &'static str = "name = \"default\"\n";
            type Error = std::convert::Infallible;
            fn validate(&self) -> Result<(), Self::Error> {
                Ok(())
            }
        }

        let existing = "label = \"desk\"\nmystery = 1\n";
        let value = Strict {
            name: "default".into(),
            label: None,
        };

        let out = render_overlay(existing, &value).expect("renders");

        assert!(!out.contains("label"), "the schema key must go: {out}");
        assert!(
            out.contains("mystery = 1"),
            "the user's key must stay: {out}"
        );
    }

    #[test]
    fn a_save_creates_a_table_the_document_does_not_have_yet() {
        let mut value = config_from(HAND_EDITED);
        value.core.brightness = 5;

        let out = render_overlay("enabled = false\n", &value).expect("renders");

        let back = config_from(&out);
        assert_eq!(back.core.brightness, 5);
        assert_eq!(back.core.color, "amber");
    }

    #[test]
    fn a_saved_file_reloads_to_the_value_that_was_saved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("core-leds.toml");
        let mut value = config_from(HAND_EDITED);
        value.core.brightness = 6;
        value.core.palette = vec!["teal".into()];
        value.core.label = None;

        // No file yet: the documented default seeds it, so the first save is
        // still a commented, readable file.
        save_overlay_to(&path, &value).expect("saves");
        let seeded = std::fs::read_to_string(&path).expect("read back");
        assert!(seeded.contains("# The per-core LED strip."), "{seeded}");
        assert_eq!(
            load_from::<Leds>(std::slice::from_ref(&path))
                .expect("reloads")
                .config,
            value
        );

        // And a second save over the real file round-trips too.
        value.core.color = "cyan".into();
        save_overlay_to(&path, &value).expect("saves again");
        assert_eq!(
            load_from::<Leds>(std::slice::from_ref(&path))
                .expect("reloads")
                .config,
            value
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("dir").count(),
            1,
            "the atomic writer must leave no temp file behind"
        );
    }

    #[test]
    fn a_save_refuses_a_value_it_would_not_accept_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("core-leds.toml");
        let mut value = config_from(HAND_EDITED);
        value.core.brightness = 200;

        let err = save_overlay_to(&path, &value).expect_err("validate must gate the write");

        assert!(matches!(err, ConfigError::Invalid(_)), "got {err:?}");
        assert!(!path.exists(), "a rejected save must not create the file");
    }
}
