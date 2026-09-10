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
//! A third shape gets the same treatment for the same reason: a
//! [`crate::merge::UNSET_KEY`] marker the merge cannot honour — `_unset =
//! "color"` rather than `_unset = ["color"]`. [`crate::merge`] detects it,
//! because that is where the marker is honoured; [`assemble`] warns about it,
//! because that is where the layer's *file name* is (#988). It is not a
//! `ConfigError`: the rest of the file is fine, and the user gets a named,
//! actionable line in the journal instead of a shell that will not start.
//!
//! A fourth shape is the same argument one level down, and the reason it needs
//! its own check: `_unset = ["colr"]` is *well* formed, so the third check says
//! nothing, and the marker is stripped before the schema is ever shown the
//! table, so rule 4 structurally cannot report the name inside it as unknown
//! either. [`crate::merge::inert_unset`] finds a name no layer sets and
//! [`assemble`] warns, same split, same reason (#1008).
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

/// The one message a malformed [`merge::UNSET_KEY`] produces.
///
/// A `const` rather than a literal in the macro so the tests can select on the
/// *exact* message instead of a substring: a filter that greps for `_unset`
/// turns green-and-blind the day somebody rewords the string, which is the
/// failure mode the negative test ("a well-formed marker warns about nothing")
/// exists to rule out. Interpolated into the `warn!` below, so the two cannot
/// drift.
const MALFORMED_UNSET_MESSAGE: &str = "_unset must be an array of key names; ignoring it";

/// The one message an inert [`merge::UNSET_KEY`] name produces.
///
/// A `const` for the same anti-drift reason as [`MALFORMED_UNSET_MESSAGE`]: the
/// tests select on it exactly, so the negative ones cannot go green-and-blind
/// on a reworded string.
const INERT_UNSET_MESSAGE: &str = "_unset names a key no config layer sets; it removes nothing";

/// How a layer is named in a diagnostic. `None` is [`Subsystem::DEFAULT_TOML`],
/// which is not a file — a complaint about *that* one is our bug, not the
/// user's, and saying so is the difference between "go fix your config" and
/// "go file an issue".
fn layer_name(path: Option<&Path>) -> String {
    path.map_or_else(
        || "the built-in default".to_string(),
        |path| path.display().to_string(),
    )
}

/// A [`serde_ignored`] path as the dotted key path the rest of this module
/// speaks in.
///
/// `serde_ignored` spells every hop through a wrapper — `Option`, a newtype
/// struct, a newtype variant — as a `?` segment, so a key the schema does not
/// know inside a `core: Option<Core>` arrives as `core.?.mystery` while
/// [`collect_paths`] and [`crate::merge::inert_unset`] produce `core.mystery`.
/// Left as-is that mismatch is not cosmetic: [`schema_paths`] subtracts one set
/// from the other, so the user's own key inside an *optional* table failed to
/// subtract and [`patch`]'s stale sweep deleted it (found while fixing #1008
/// shape 2, where the sweep now recurses into such a table instead of removing
/// it whole, and would otherwise have emptied it key by key).
///
/// A TOML key that is literally `?` would be folded away here. It has to be a
/// quoted key to exist at all, no schema in the workspace has one, and the
/// consequence is a key not removed rather than a key wrongly removed — the
/// same direction, for the same reason, as [`collect_paths`]'s dotted-key note.
fn dotted_key(path: &str) -> String {
    if !path.contains('?') {
        return path.to_owned();
    }
    path.split('.')
        .filter(|segment| *segment != "?")
        .collect::<Vec<_>>()
        .join(".")
}

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
    // Kept as two parallel vectors rather than pairs: the diagnostics below
    // need the file name beside each table, and `merge::inert_unset` needs the
    // tables as one slice because its question — "does *any* layer set this
    // key?" — is about the whole stack rather than about one layer at a time.
    let mut paths: Vec<Option<&Path>> = Vec::with_capacity(layers.len() + 1);
    let mut tables: Vec<toml::Table> = Vec::with_capacity(layers.len() + 1);
    paths.push(None);
    tables.push(parse_layer(S::DEFAULT_TOML, None)?);
    for (path, body) in layers {
        paths.push(Some(path.as_path()));
        tables.push(parse_layer(body, Some(path))?);
    }

    // #988: a `_unset` the merge cannot honour is dropped either way, so it
    // has to be *said* — otherwise the user gets the inherited value back with
    // no signal at all, which is the invisible failure the module docs above
    // argue against for every other key shape. Per layer and before the merge,
    // because after it every marker is gone and there is nothing left to
    // attribute to a file.
    for (path, table) in paths.iter().zip(&tables) {
        for bad in merge::malformed_unset(table) {
            tracing::warn!(
                subsystem = S::NAME,
                layer = %layer_name(*path),
                key = %bad.key,
                found = bad.found,
                "{MALFORMED_UNSET_MESSAGE}"
            );
        }
    }

    // #1008: the same argument one level down. A *well-formed* marker naming a
    // key nothing sets is the typo that actually costs the erasure, and it is
    // the one shape neither the check above nor rule 4 below can see — the
    // marker is stripped before the schema is shown the table, so the name
    // inside it never reaches `serde_ignored`.
    for inert in merge::inert_unset(&tables) {
        tracing::warn!(
            subsystem = S::NAME,
            layer = %layer_name(paths[inert.layer]),
            key = %inert.key,
            "{INERT_UNSET_MESSAGE}"
        );
    }

    let merged = merge::merge_all(tables);

    let mut unknown_keys = Vec::new();
    let config: S = serde_ignored::deserialize(merged.into_deserializer(), |path| {
        unknown_keys.push(dotted_key(&path.to_string()));
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

/// Every dotted key path in `table`, tables included — except the
/// [`merge::UNSET_KEY`] marker, at any depth.
///
/// The exclusion is by name, deliberately, rather than left to
/// [`serde_ignored`]: [`merge::merge_into`] strips the marker before the
/// schema ever sees the table, so `serde_ignored` structurally *cannot* report
/// it as ignored, and [`schema_paths`] would then count it as schema-owned. The
/// stale sweep in [`patch`] would go on to delete the user's explicit erasure
/// on the next save — silently, and with the erased key falling back to the
/// inherited value on the load after that (#990).
fn collect_paths(table: &toml::Table, prefix: &str, out: &mut BTreeSet<String>) {
    for (key, value) in table {
        if key == merge::UNSET_KEY {
            continue;
        }
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
        ignored.insert(dotted_key(&p.to_string()));
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
///
/// `table` is a [`toml_edit::TableLike`] rather than a [`toml_edit::Table`]
/// because since #1008 [`patch`] recurses into inline tables too, and that is
/// what the two decor rules here are about. Inside `{ a = 1 }` the space before
/// the `}` is the **last value's own suffix**, and a fresh value's absent decor
/// is what `toml_edit` fills in positionally, so:
///
/// * **Replacing** a value carries its suffix over unless that suffix holds a
///   comment — forcing it empty would render `{ a = 2}`, and in a standard
///   table the suffix is either empty already or the stale annotation this
///   deliberately drops. One rule, both spellings: keep the spacing, drop the
///   annotation.
/// * **Appending** leaves the new value's decor alone, so it inherits the
///   trailing `(" ", " ")` inside an inline table and the ordinary `(" ", "")`
///   in a standard one — and clears the previous last value's suffix when that
///   is pure whitespace, because it has just stopped being the last value and
///   its space would otherwise render as `{ a = 1 , b = 2 }`. Only an inline
///   table ever has whitespace there.
fn set_value(table: &mut dyn toml_edit::TableLike, key: &str, mut value: toml_edit::Value) {
    let Some(carried) = carried_suffix(table, key) else {
        if let Some(last) = last_whitespace_suffixed_key(table)
            && let Some(last_value) = table.get_mut(&last).and_then(toml_edit::Item::as_value_mut)
        {
            last_value.decor_mut().set_suffix("");
        }
        table.insert(key, toml_edit::Item::Value(value));
        return;
    };

    value.decor_mut().set_prefix(" ");
    value.decor_mut().set_suffix(carried);
    // Assigning through the existing `Item` rather than re-inserting the key is
    // what keeps the comment block above it; see this function's doc.
    if let Some(item) = table.get_mut(key) {
        *item = toml_edit::Item::Value(value);
    }
}

/// What [`set_value`] should put back after the value at `key`, or `None` when
/// there is no `key` here yet — which is how it tells an edit from an append.
///
/// `Some("")` covers both "nothing followed the old value" and "a comment did,
/// and it described a value that is about to stop existing".
fn carried_suffix(table: &dyn toml_edit::TableLike, key: &str) -> Option<String> {
    let item = table.get(key)?;
    Some(
        item.as_value()
            .and_then(|value| value.decor().suffix())
            .and_then(toml_edit::RawString::as_str)
            .filter(|raw| !raw.contains('#'))
            .unwrap_or_default()
            .to_owned(),
    )
}

/// The last key of `table`, if the value under it carries a non-empty
/// whitespace-only suffix: the space an inline table's final entry holds in
/// front of its `}`.
fn last_whitespace_suffixed_key(table: &dyn toml_edit::TableLike) -> Option<String> {
    let (key, item) = table.iter().last()?;
    let suffix = item
        .as_value()?
        .decor()
        .suffix()
        .and_then(toml_edit::RawString::as_str)?;
    (!suffix.is_empty() && suffix.chars().all(char::is_whitespace)).then(|| key.to_owned())
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
///
/// `doc` is a [`toml_edit::TableLike`], which is what makes the two levels
/// below work on a table the user spelled `core = { … }` as well as on one they
/// spelled `[core]`. Recursing into an inline table rather than replacing it is
/// both halves of #1008 shape 1: the table keeps its spelling (the user's
/// choice, not ours to normalise) and everything inside it that is not the
/// schema's — a [`merge::UNSET_KEY`] marker, a hand-added key — keeps its
/// bytes, the way it already did in a standard table.
fn patch(
    doc: &mut dyn toml_edit::TableLike,
    want: &toml::Table,
    have: &toml::Table,
    owned: Option<&BTreeSet<String>>,
    prefix: &str,
) {
    let empty = toml::Table::new();

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
            // A *table* the schema owns is not the same as its contents being
            // ours (#1008 shape 2). Sweep it with an empty `want`, which
            // removes the schema-owned keys inside it at every depth and
            // leaves a marker, an unknown key or an unrelated sub-table where
            // the user put them — comments and all. Only a table with nothing
            // of theirs left in it goes whole, which is the ordinary case and
            // the behaviour every other shape already had.
            let emptied = match doc
                .get_mut(&key)
                .and_then(toml_edit::Item::as_table_like_mut)
            {
                Some(sub_doc) => {
                    let sub_have = have
                        .get(&key)
                        .and_then(toml::Value::as_table)
                        .unwrap_or(&empty);
                    patch(
                        sub_doc,
                        &empty,
                        sub_have,
                        Some(owned),
                        &format!("{prefix}{key}."),
                    );
                    sub_doc.is_empty()
                }
                // A scalar, or an array of tables: nothing to keep back.
                None => true,
            };
            if emptied {
                doc.remove(&key);
            }
        }
    }

    for (key, value) in want {
        if let toml::Value::Table(sub_want) = value {
            if !doc.get(key).is_some_and(toml_edit::Item::is_table_like) {
                // A standard table inside a standard one, an inline table
                // inside an inline one: `TableLike::insert` converts on the way
                // in, so the new table is spelled the way its parent is.
                doc.insert(key, toml_edit::Item::Table(toml_edit::Table::new()));
            }
            let Some(sub_doc) = doc
                .get_mut(key)
                .and_then(toml_edit::Item::as_table_like_mut)
            else {
                continue;
            };
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
/// A [`crate::merge::UNSET_KEY`] marker survives a save. Not because
/// [`serde_ignored`] reports it as a key the schema does not know — it cannot,
/// the merge eats the marker before the schema is ever shown the table — but
/// because [`collect_paths`] excludes it by name, so the stale sweep never
/// counts it as schema-owned (#990).
///
/// It also stays *correct*, in both of the two cases there are. When the value
/// still carries the key that was unset, the save writes it back explicitly
/// and the merge applies removals before assignments, so the marker is
/// redundant rather than wrong. When the value no longer carries it — an
/// `Option` gone to `None`, which is the shape that made the erasure worth
/// spelling in the first place — the marker is the only thing holding the
/// erasure, and deleting it would silently restore the inherited value on the
/// next load.
///
/// **The two shapes that used to lose it (#1008), and what they do now.** Both
/// were about [`patch`] losing the *table* rather than the marker:
///
/// 1. **The table spelled inline.** `patch` used to recurse only into
///    [`toml_edit::Item::is_table`], which is false for
///    `core = { brightness = 7, _unset = ["label"], mystery = 42 }`, so the
///    inline table was replaced wholesale — taking the marker, and any key the
///    schema does not know, with it. That second half predated #990: it is the
///    older "unrelated keys survive" guarantee, and no fixture caught it
///    because they all use standard tables. It now recurses through
///    [`toml_edit::Item::as_table_like_mut`] and patches the inline table in
///    place. **The table stays inline**: how the user spells a table is theirs,
///    and a writer whose whole argument is "do not rewrite bytes you were not
///    asked to" has no business promoting it.
/// 2. **A schema field of type `Option<Table>` gone to `None`.** The stale
///    sweep matched the *table's* own path, which [`collect_paths`] correctly
///    still inserts (the table is schema-owned even though the marker inside it
///    is not), so `doc.remove` took the whole `[core]` block. It now sweeps the
///    keys the schema owns *out of* that table — recursively — and keeps
///    whatever is left: the marker, its comment, keys the schema does not know,
///    unrelated sub-tables. **A table left holding only those stays**, header
///    and all; a table left holding nothing is removed whole, which is the
///    ordinary case and is what every other shape already did.
///
/// The asymmetry in (2) is the point. "The schema owns this table" is a
/// statement about the keys the schema put there, not a licence over the block
/// the user typed around them — and the marker specifically must outlive the
/// value it erases, or the erasure it holds is undone by the next save (#990).
/// The cost of the choice is an empty-looking `[core]` left behind when the
/// user's only remaining line is a marker; the cost of the other choice is
/// silent data loss, so it is not close.
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

    // ── #990: a save keeps the user's `_unset` marker ────────────────────────

    /// The marker is the user's, never the schema's — at the root and at
    /// depth, in one document so a fix that only handles the top level cannot
    /// pass.
    ///
    /// Red before [`collect_paths`] excluded [`merge::UNSET_KEY`] by name:
    /// [`merge::merge_into`] strips the marker before `serde_ignored` can
    /// report it as ignored, so [`schema_paths`] counted both markers as
    /// schema-owned and [`patch`]'s stale sweep deleted them.
    #[test]
    fn a_save_keeps_the_users_unset_marker_at_every_depth() {
        let existing = "_unset = [\"enabled\"]\n\n[core]\n_unset = [\"label\"]\nbrightness = 7\n";
        let value = config_from(existing);

        let out = render_overlay(existing, &value).expect("renders");

        assert!(
            out.contains("_unset = [\"enabled\"]"),
            "the root marker must survive a save: {out}"
        );
        assert!(
            out.contains("_unset = [\"label\"]"),
            "and so must one inside a table: {out}"
        );
    }

    /// What the deletion actually costs, which is why #990 is not cosmetic: an
    /// erasure whose key the value no longer carries is held by the marker
    /// *alone*. Drop it on a save about something else and the inherited value
    /// comes back on the next load, silently.
    ///
    /// Red before the fix — the second load sees `label = Some("old")`.
    #[test]
    fn an_unset_marker_still_erases_after_a_save_of_an_unrelated_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().join("base.toml");
        let overlay = dir.path().join("core-leds.toml");
        std::fs::write(&base, "[core]\nlabel = \"old\"\n").expect("seed the base layer");
        std::fs::write(&overlay, "[core]\n_unset = [\"label\"]\nbrightness = 7\n")
            .expect("seed the overlay");
        let paths = [base, overlay.clone()];

        let mut value = load_from::<Leds>(&paths).expect("loads").config;
        assert_eq!(value.core.label, None, "the marker erases the base's label");

        // A save that has nothing to do with `label` at all.
        value.core.brightness = 5;
        save_overlay_to(&overlay, &value).expect("saves");
        let written = std::fs::read_to_string(&overlay).expect("read back");
        assert!(
            written.contains("_unset"),
            "the marker must survive the save: {written}"
        );

        let reloaded = load_from::<Leds>(&paths).expect("reloads").config;
        assert_eq!(reloaded.core.brightness, 5, "the save itself still took");
        assert_eq!(
            reloaded.core.label, None,
            "…and the erasure must still hold on the next load: {written}"
        );
    }

    // ── #988: a marker the merge cannot honour is said out loud ─────────────

    /// Captured `tracing` events, in the shape `hytte-services`' `hooks` tests
    /// use: a shared buffer, a field visitor, and a thread-local
    /// [`tracing::dispatcher::set_default`] guard so one test's subscriber
    /// cannot leak into another's.
    ///
    /// Hand-rolled over [`tracing::Subscriber`] rather than assembled from
    /// `tracing_subscriber`'s `Registry` + `Layer`, which is what `hooks` does:
    /// this crate is the GTK-free leaf whose whole point is a short dependency
    /// list (`serde`, `serde_ignored`, `toml`, `toml_edit`, `tracing`), and a
    /// capture that only ever needs `event` does not justify widening it.
    #[derive(Clone, Default)]
    struct Captured {
        events: std::sync::Arc<std::sync::Mutex<Vec<CapturedEvent>>>,
    }

    #[derive(Clone, Debug)]
    struct CapturedEvent {
        level: tracing::Level,
        message: String,
        fields: std::collections::HashMap<String, String>,
    }

    impl Captured {
        fn events(&self) -> Vec<CapturedEvent> {
            self.events.lock().expect("not poisoned").clone()
        }
    }

    impl tracing::Subscriber for Captured {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);
            self.events
                .lock()
                .expect("not poisoned")
                .push(CapturedEvent {
                    level: *event.metadata().level(),
                    message: visitor.message,
                    fields: visitor.fields,
                });
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[derive(Default)]
    struct FieldVisitor {
        message: String,
        fields: std::collections::HashMap<String, String>,
    }

    impl tracing::field::Visit for FieldVisitor {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                self.message = value.to_string();
            } else {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }
        }
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            // `%` sigils and the message itself arrive here; the wrappers
            // `tracing` uses render their `Display` form through `Debug`.
            let rendered = format!("{value:?}");
            if field.name() == "message" {
                self.message = rendered;
            } else {
                self.fields.insert(field.name().to_string(), rendered);
            }
        }
    }

    fn capture() -> (Captured, tracing::dispatcher::DefaultGuard) {
        let captured = Captured::default();
        let guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(captured.clone()));
        // Belt and braces, kept deliberately. The hazard is real — with zero
        // live dispatchers `tracing-core` caches a callsite's interest as
        // `never` for the whole binary (`callsite.rs`'s
        // `interest.unwrap_or_else(Interest::never)`) — but `Dispatch::new`
        // above already closes it: constructing a `Dispatch` registers it and
        // rebuilds interest for every registered callsite over the live list.
        // What this call covers is the narrower `Rebuilder::JustOne` path,
        // where with a single live dispatcher the rebuild degrades to
        // "whatever *this* thread's default is" — genuinely thread-sensitive,
        // and one line to insure against. It can only widen interest here
        // (`Captured::enabled` is unconditionally true), so it cannot poison a
        // sibling test.
        tracing::callsite::rebuild_interest_cache();
        (captured, guard)
    }

    /// The malformed-marker warnings, selected on the **exact** message rather
    /// than on a substring of it. A `contains("_unset")` filter would turn
    /// green-and-blind if the `warn!` were ever reworded — including the
    /// negative test below, whose whole job is to observe an absence.
    fn unset_warnings(captured: &Captured) -> Vec<CapturedEvent> {
        captured
            .events()
            .into_iter()
            .filter(|e| e.level == tracing::Level::WARN && e.message == MALFORMED_UNSET_MESSAGE)
            .collect()
    }

    /// A `_unset` the merge cannot honour removes nothing. Saying so is the
    /// whole fix — and it has to name the layer, because with three or four
    /// candidate files in play an unattributed complaint is close to useless.
    ///
    /// Red if the [`merge::malformed_unset`] loop in [`assemble`] goes away.
    #[test]
    fn a_malformed_unset_marker_warns_naming_the_layer() {
        let (captured, _guard) = capture();

        let loaded = assembled(&["[core]\n_unset = \"color\"\n"]);

        let warnings = unset_warnings(&captured);
        assert_eq!(warnings.len(), 1, "one marker, one warning: {warnings:#?}");
        let fields = &warnings[0].fields;
        assert_eq!(
            fields.get("layer").map(String::as_str),
            Some("/layer/0.toml"),
            "the file the user can open: {fields:#?}"
        );
        assert_eq!(fields.get("key").map(String::as_str), Some("core._unset"));
        assert_eq!(fields.get("found").map(String::as_str), Some("string"));
        assert_eq!(
            fields.get("subsystem").map(String::as_str),
            Some("core-leds")
        );

        assert_eq!(
            loaded.config.core.color, "amber",
            "the marker removed nothing, which is exactly what the warning is for"
        );
        assert!(
            loaded.unknown_keys.is_empty(),
            "and it is still stripped, so it is not an unknown key on top: {:?}",
            loaded.unknown_keys
        );
    }

    /// Every element that is not a key name is named, by index — a marker with
    /// one usable entry and two typos must not report just the first.
    #[test]
    fn every_non_key_name_element_of_an_unset_marker_is_warned_about() {
        let (captured, _guard) = capture();

        let loaded = assembled(&["[core]\n_unset = [\"color\", 3, true]\n"]);

        let warnings = unset_warnings(&captured);
        let named: Vec<(&str, &str)> = warnings
            .iter()
            .map(|e| {
                (
                    e.fields.get("key").map_or("", String::as_str),
                    e.fields.get("found").map_or("", String::as_str),
                )
            })
            .collect();
        assert_eq!(
            named,
            [("core._unset[1]", "integer"), ("core._unset[2]", "boolean")],
            "each offender, by index and type: {warnings:#?}"
        );

        assert_eq!(
            loaded.config.core.color, "",
            "the usable element is still honoured, so the default's amber is gone"
        );
    }

    /// The warning must be a signal, not noise on every file that uses the
    /// feature: a well-formed marker says nothing.
    ///
    /// The layer carries an unknown key alongside the marker purely as a
    /// **live control**. Asserting an absence against a capture that observed
    /// nothing at all is not an assertion — swap the thread's subscriber for
    /// `Dispatch::none()` and a bare "no `_unset` warning" test stays green
    /// while the two positive tests above go red. The control event proves the
    /// capture was wired at the moment the absence was observed.
    #[test]
    fn a_well_formed_unset_marker_is_not_warned_about() {
        let (captured, _guard) = capture();

        let loaded = assembled(&["[core]\n_unset = [\"color\"]\nnope = 1\n"]);

        let events = captured.events();
        assert!(
            events.iter().any(|e| e.level == tracing::Level::WARN
                && e.fields.get("key").map(String::as_str) == Some("core.nope")),
            "the control event must land, or the absence below proves nothing: {events:#?}"
        );
        assert!(
            unset_warnings(&captured).is_empty(),
            "a well-formed marker must draw no complaint: {events:#?}"
        );
        assert_eq!(loaded.config.core.color, "", "and it was honoured");
        assert_eq!(
            loaded.unknown_keys,
            ["core.nope"],
            "the control is the ordinary rule-4 path, unchanged"
        );
    }

    /// The other arm of [`layer_name`]: a malformed marker in
    /// [`Subsystem::DEFAULT_TOML`] is **our** bug, and the warning has to say
    /// so rather than name a file the user could go and edit.
    ///
    /// It is also the arm a future `DEFAULT_TOML` edit could start hitting
    /// silently, since no fixture on the happy path ever reaches it.
    #[test]
    fn a_malformed_marker_in_the_built_in_default_is_named_as_ours() {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct BadDefault {
            #[serde(default)]
            color: String,
        }

        impl Subsystem for BadDefault {
            const NAME: &'static str = "bad-default";
            const DEFAULT_TOML: &'static str = "_unset = \"color\"\ncolor = \"amber\"\n";
            type Error = std::convert::Infallible;
            fn validate(&self) -> Result<(), Self::Error> {
                Ok(())
            }
        }

        let (captured, _guard) = capture();

        let loaded = assemble::<BadDefault>(&[]).expect("assembles");

        let warnings = unset_warnings(&captured);
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        let fields = &warnings[0].fields;
        assert_eq!(
            fields.get("layer").map(String::as_str),
            Some("the built-in default"),
            "not a path — there is no file to send anyone to: {fields:#?}"
        );
        assert_eq!(fields.get("key").map(String::as_str), Some("_unset"));
        assert_eq!(fields.get("found").map(String::as_str), Some("string"));
        assert_eq!(loaded.config.color, "amber", "and it removed nothing");
    }

    // ── #1008: the two shapes where `patch` used to lose the marker ─────────
    //
    // Both were about `patch` losing the *table*, not the marker: one spelled
    // inline, one swept whole as a schema-owned `Option<Table>` gone to `None`.
    // They replace the two "…_today" tests #1006 left pinning the old
    // behaviour; `render_overlay`'s doc carries the full argument, including
    // why a table left holding nothing but the user's own lines stays.

    /// A subsystem whose table is optional, so a save can drop it entirely.
    /// Shared by the sweep tests below.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct OptTable {
        #[serde(default)]
        enabled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        core: Option<Core>,
    }

    impl Subsystem for OptTable {
        const NAME: &'static str = "opt-table";
        const DEFAULT_TOML: &'static str = "enabled = true\n";
        type Error = std::convert::Infallible;
        fn validate(&self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    /// **#1008 shape 1.** A table the user spelled inline is patched *in
    /// place*: [`patch`] recurses through
    /// [`toml_edit::Item::as_table_like_mut`], so the marker and every key the
    /// schema does not know keep their bytes — and the table keeps its
    /// spelling, which is the user's choice and not the writer's to normalise.
    ///
    /// The unknown-key half is the older guarantee
    /// `a_save_preserves_keys_and_tables_the_schema_does_not_know` states; that
    /// fixture only uses standard tables, which is why nothing caught it.
    ///
    /// Red if the recursion gate goes back to [`toml_edit::Item::is_table`]:
    /// the inline table is replaced by a fresh standard one and both halves go
    /// with it.
    #[test]
    fn a_save_of_an_inline_table_keeps_the_marker_and_unknown_keys() {
        let existing = "core = { brightness = 7, _unset = [\"label\"], mystery = 42 }\n";
        let mut value = config_from(existing);
        value.core.brightness = 5;

        let out = render_overlay(existing, &value).expect("renders");

        assert!(
            out.contains("_unset = [\"label\"]"),
            "the marker is the user's, wherever the table is spelled: {out}"
        );
        assert!(
            out.contains("mystery = 42"),
            "and so is a key the schema does not know: {out}"
        );
        assert!(
            out.starts_with("core = {"),
            "the table stays inline — the spelling is theirs: {out}"
        );
        assert!(
            !out.contains("[core]"),
            "…so it is never promoted to a standard table: {out}"
        );
        assert_eq!(
            config_from(&out).core.brightness,
            5,
            "and the save itself still takes"
        );

        // The whole rendering, so what a save through an inline table actually
        // produces is written down rather than inferred from four `contains`.
        // The three appended keys are the documented "a whole-value save pins
        // every key" behaviour, and the spacing around them is `set_value`'s
        // append rule: red if it stops clearing `mystery`'s trailing space,
        // which would render `42 , color`.
        assert_eq!(
            out,
            "core = { brightness = 5, _unset = [\"label\"], mystery = 42, \
             color = \"amber\", palette = [\"amber\", \"rust\"] }\nenabled = true\n"
        );
    }

    /// The fidelity half of shape 1, asserted on **bytes** rather than on
    /// `contains`: a save through an inline table must move the one value that
    /// changed and nothing else — not the comment above the key, not the one
    /// beside the closing brace, not the spacing inside the braces.
    ///
    /// The edited key is deliberately the **last** one in the table, because
    /// that space in front of the `}` is the previous value's own decor suffix:
    /// red if `set_value` forces the suffix empty (`brightness = 7}`), red if
    /// it stops normalising the prefix, and red if `patch` replaces the table.
    #[test]
    fn a_save_through_an_inline_table_moves_one_value_and_no_other_byte() {
        let existing = "# My LEDs.\nenabled = true\n\n\
             # the strip, spelled inline on purpose\n\
             core = { color = \"amber\", palette = [\"amber\", \"rust\"], mystery = 42, brightness = 3 } # hand-tuned\n";

        assert_eq!(
            render_overlay(existing, &config_from(existing)).expect("renders"),
            existing,
            "a no-op save must be a no-op here too, or the assertion below \
             proves nothing about the edit"
        );

        let mut value = config_from(existing);
        value.core.brightness = 7;

        assert_eq!(
            render_overlay(existing, &value).expect("renders"),
            existing.replace("brightness = 3", "brightness = 7"),
            "only the one key's bytes may move"
        );
    }

    /// **#1008 shape 2.** A schema field of type `Option<Table>` gone to `None`
    /// makes the stale sweep match the *table's* path — which [`collect_paths`]
    /// rightly still inserts, the table being schema-owned even though what the
    /// user wrote inside it is not. The sweep now recurses with an empty `want`
    /// and removes the schema's keys out of the block, leaving the marker, the
    /// comment above it, and the key the schema does not know.
    ///
    /// Red if the sweep goes back to `doc.remove`-ing the table item, and red
    /// if `dotted_key` stops folding `serde_ignored`'s `?` wrapper segment —
    /// without it `core.mystery` never subtracts out of the owned set and the
    /// recursion deletes it key by key instead of taking it whole.
    #[test]
    fn a_save_that_drops_an_optional_table_keeps_what_the_schema_does_not_own() {
        let existing = "enabled = true\n\n[core]\n# do not inherit the base's label\n_unset = [\"label\"]\nmystery = 1\nbrightness = 7\n";
        let value = OptTable {
            enabled: true,
            core: None,
        };

        let out = render_overlay(existing, &value).expect("renders");

        assert_eq!(
            out,
            "enabled = true\n\n[core]\n# do not inherit the base's label\n_unset = [\"label\"]\nmystery = 1\n",
            "the schema's `brightness` goes; the marker, its comment and the \
             user's own key stay, in a block that is now theirs alone"
        );
    }

    /// The other half of that decision, and the reason it is not simply "never
    /// remove a table": a block holding nothing but the schema's own keys still
    /// goes whole, header comment and all. That is the ordinary `Option<Table>`
    /// → `None` case and the behaviour every other shape already had.
    ///
    /// Red if the `emptied` check in the sweep goes away — an empty `[core]`
    /// header is left behind on every such save.
    #[test]
    fn a_save_that_drops_an_optional_table_with_nothing_of_the_users_in_it_removes_the_block() {
        let existing =
            "enabled = true\n\n# the strip\n[core]\nbrightness = 7\npalette = [\"amber\"]\n";
        let value = OptTable {
            enabled: true,
            core: None,
        };

        let out = render_overlay(existing, &value).expect("renders");

        assert_eq!(
            out, "enabled = true\n",
            "nothing of the user's was in it, so the block goes whole"
        );
    }

    /// `serde_ignored` spells the hop through an `Option` as a `?` segment, and
    /// both dotted paths this module hands out are key names. The reporting
    /// half is cosmetic; the [`schema_paths`] half is not — an unrecognised
    /// path fails to subtract, so the writer counts the user's own key as
    /// schema-owned and deletes it.
    ///
    /// Red if `dotted_key` stops folding `?` away: `unknown_keys` reads
    /// `core.?.mystery` and the save eats `mystery`.
    #[test]
    fn an_unknown_key_inside_an_optional_table_is_named_and_kept() {
        let existing = "enabled = true\n\n[core]\nmystery = 1\nbrightness = 7\n";

        let loaded = assemble::<OptTable>(&layers(&[existing])).expect("assembles");
        assert_eq!(
            loaded.unknown_keys,
            ["core.mystery"],
            "a dotted key name, not serde_ignored's `core.?.mystery`"
        );

        let value = OptTable {
            enabled: true,
            core: None,
        };
        let out = render_overlay(existing, &value).expect("renders");
        assert!(
            out.contains("mystery = 1"),
            "and a key the writer failed to recognise must never be deleted: {out}"
        );
    }

    // ── #1008: a marker that names nothing is said out loud ─────────────────

    /// The inert-marker warnings, selected on the **exact** message, for the
    /// same anti-drift reason [`unset_warnings`] is.
    fn inert_warnings(captured: &Captured) -> Vec<CapturedEvent> {
        captured
            .events()
            .into_iter()
            .filter(|e| e.level == tracing::Level::WARN && e.message == INERT_UNSET_MESSAGE)
            .collect()
    }

    /// The typo that actually costs the erasure, and the one shape neither of
    /// the other two checks can see: `_unset = ["colr"]` is well formed, so
    /// #988 says nothing, and the marker is stripped before `serde_ignored`
    /// runs, so rule 4 never gets shown the name inside it.
    ///
    /// The same marker carries a name that *is* set, which is both the live
    /// control (a capture that observed nothing would fail the count) and the
    /// noise guard: a marker naming a real key must draw no complaint.
    ///
    /// Red if the [`merge::inert_unset`] loop in [`assemble`] goes away.
    #[test]
    fn an_unset_marker_naming_a_key_no_layer_sets_is_warned_about() {
        let (captured, _guard) = capture();

        let loaded = assembled(&["[core]\n_unset = [\"colr\", \"color\"]\n"]);

        let warnings = inert_warnings(&captured);
        assert_eq!(
            warnings.len(),
            1,
            "one name matches nothing, one warning: {warnings:#?}"
        );
        let fields = &warnings[0].fields;
        assert_eq!(
            fields.get("key").map(String::as_str),
            Some("core.colr"),
            "the key to go and correct, not the marker's own path: {fields:#?}"
        );
        assert_eq!(
            fields.get("layer").map(String::as_str),
            Some("/layer/0.toml"),
            "the file the user can open: {fields:#?}"
        );
        assert_eq!(
            fields.get("subsystem").map(String::as_str),
            Some("core-leds")
        );

        assert_eq!(
            loaded.config.core.color, "",
            "`color` is a real key and really was erased, which is why it is \
             the control rather than a second complaint"
        );
    }

    /// The noise guard across layers: erasing a key a *lower* layer sets is the
    /// whole point of the feature and must stay silent.
    ///
    /// The unknown key beside the marker is the live control, the shape
    /// `a_well_formed_unset_marker_is_not_warned_about` established: an absence
    /// measured against a capture that observed nothing is not an assertion.
    ///
    /// Red if `merge::inert_unset` stops collecting the key paths of the other
    /// layers before deciding.
    #[test]
    fn an_unset_marker_that_erases_a_lower_layers_key_is_not_warned_about() {
        let (captured, _guard) = capture();

        let loaded = assembled(&[
            "[core]\nlabel = \"old\"\n",
            "[core]\n_unset = [\"label\"]\nnope = 1\n",
        ]);

        let events = captured.events();
        assert!(
            events.iter().any(|e| e.level == tracing::Level::WARN
                && e.fields.get("key").map(String::as_str) == Some("core.nope")),
            "the control event must land, or the absence below proves nothing: {events:#?}"
        );
        assert!(
            inert_warnings(&captured).is_empty(),
            "the marker names a key the base layer sets: {events:#?}"
        );
        assert_eq!(
            loaded.config.core.label, None,
            "…and it did erase it, which is what makes the silence correct"
        );
    }

    /// The other arm of [`layer_name`] for this warning: a marker naming
    /// nothing in [`Subsystem::DEFAULT_TOML`] is **our** typo, and the line has
    /// to say so rather than send the user to a file they did not write.
    #[test]
    fn an_inert_marker_in_the_built_in_default_is_named_as_ours() {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct InertDefault {
            #[serde(default)]
            color: String,
        }

        impl Subsystem for InertDefault {
            const NAME: &'static str = "inert-default";
            const DEFAULT_TOML: &'static str = "_unset = [\"colr\"]\ncolor = \"amber\"\n";
            type Error = std::convert::Infallible;
            fn validate(&self) -> Result<(), Self::Error> {
                Ok(())
            }
        }

        let (captured, _guard) = capture();

        let loaded = assemble::<InertDefault>(&[]).expect("assembles");

        let warnings = inert_warnings(&captured);
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        let fields = &warnings[0].fields;
        assert_eq!(
            fields.get("layer").map(String::as_str),
            Some("the built-in default"),
            "not a path — there is no file to send anyone to: {fields:#?}"
        );
        assert_eq!(fields.get("key").map(String::as_str), Some("colr"));
        assert_eq!(loaded.config.color, "amber", "and it removed nothing");
    }
}
