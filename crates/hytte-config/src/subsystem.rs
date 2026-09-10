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
//! use hytte_config::subsystem::{InvalidValue, Subsystem, keep, load_or_default, spelling};
//! use hytte_config::subsystem::env::EnvKnob;
//!
//! // Every field is a raw `toml::Value` — anything narrower hands the verdict
//! // to serde, and serde's verdict is whole-file (#1040 T1).
//! #[derive(serde::Serialize, serde::Deserialize)]
//! #[serde(default)]
//! struct CoreLeds {
//!     color: toml::Value,
//! }
//!
//! impl Default for CoreLeds {
//!     fn default() -> Self { Self { color: "amber".into() } }
//! }
//!
//! const COLOR: EnvKnob = EnvKnob::same("TROLLSHELL_LED_COLOR", "color", "a CSS colour name");
//!
//! impl Subsystem for CoreLeds {
//!     const NAME: &'static str = "core-leds";
//!     const DEFAULT_TOML: &'static str = "# the core LED strip\ncolor = \"amber\"\n";
//!     type Error = std::convert::Infallible;
//!     type Resolved = String;
//!     fn validate(&self) -> Result<(), Self::Error> { Ok(()) }
//!     fn parsed(&self) -> (String, Vec<InvalidValue>) {
//!         let mut rejected = Vec::new();
//!         let raw = spelling(&self.color);
//!         let color = keep(
//!             (!raw.is_empty()).then_some(raw).ok_or_else(|| InvalidValue::of(&COLOR, &self.color)),
//!             String::default(),
//!             &mut rejected,
//!         );
//!         (color, rejected)
//!     }
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

pub mod env;
#[cfg(feature = "watch")]
pub mod watch;

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

    /// The **resolved** form of this config: what the rest of the shell
    /// consumes, after every raw spelling has been judged.
    ///
    /// Separate from `Self` because the two have different jobs. `Self` is the
    /// file as written, and every field of it should be a raw
    /// [`toml::Value`] — anything narrower, a `String` included, hands the
    /// verdict to serde, and serde's verdict is **whole-file** (#1040 T1).
    /// `Resolved` is the parsed value, where a key is a `DisplayStyle` or a
    /// `Duration` rather than a spelling.
    ///
    /// `Default` is required because a load that fails outright degrades to it
    /// ([`initial_load`]) and because a *single* rejected key falls back to its
    /// own default ([`keep`]). `PartialEq` is what lets `watch::Watcher::poll`
    /// republish only on a real change. `Send + Sync + 'static` is what lets a
    /// `Mutable<Self::Resolved>` cross onto the runtime — required
    /// unconditionally, rather than only under the `watch` feature, so that
    /// enabling live reload can never turn into a trait-bound error in a
    /// subsystem that compiled fine without it.
    type Resolved: Clone + Default + PartialEq + Send + Sync + 'static;

    /// Semantic checks the schema cannot express — the equivalent of
    /// `places::validate`'s latitude bounds and duplicate names.
    ///
    /// Runs after deserialisation on load, and again before a save, so a
    /// config that would be rejected on read is never written.
    ///
    /// A failure here is a **whole-file** rejection: [`assemble`] maps it onto
    /// [`ConfigError::Invalid`], the load returns no config at all, and the
    /// caller degrades to the built-in defaults or keeps the last good file.
    /// That is the right contract for keys that genuinely constrain each other
    /// and the wrong one for independent look-and-feel values — one typo would
    /// revert every other key (#1040 V1). Per-key judgement belongs in
    /// [`Self::parsed`], and `Infallible` is then the honest `Error`.
    ///
    /// # How a cross-key rule composes without a second parser (#1040 T4)
    ///
    /// `validate` sees the **raw** config while [`Self::parsed`] produces the
    /// resolved values — but `parsed` takes `&self`, so the rule is stated over
    /// resolved values by calling it:
    ///
    /// ```text
    /// type Error = MyError;
    /// fn validate(&self) -> Result<(), MyError> {
    ///     let (resolved, _rejected) = self.parsed();
    ///     // …the cross-key rule: e.g. reject a `min` above a `max`, which no
    ///     // single key can be judged on…
    /// }
    /// ```
    ///
    /// One parser still, and the per-key warnings still come out exactly once,
    /// because they are emitted in [`load_layer`] rather than in `parsed` —
    /// which is why `parsed` returns its rejections instead of logging them.
    /// The `_rejected` half is deliberately available there too: a rule
    /// evaluated over a key that fell back to its built-in default can say so
    /// rather than pretending the user asked for the default.
    ///
    /// # Errors
    /// Whatever the subsystem considers unusable.
    fn validate(&self) -> Result<(), Self::Error>;

    /// The file's raw spellings as [`Self::Resolved`], beside **every** key
    /// that did not parse.
    ///
    /// The single judge: nothing else may turn a spelling into a value, which
    /// is what keeps "what the file rejects" and "what the deprecated variable
    /// rejects" from drifting.
    ///
    /// # A bad value costs its own key, and only its own key (#1040 V1)
    ///
    /// This returns a value **and** a list, not a `Result`, and that is the
    /// whole shape of the rule. An implementation that `?`s on the first bad
    /// key and hands the error to [`Self::validate`] gets a *message* naming
    /// one key and an *effect* that drops all of them. Use [`spelling`] per key
    /// and [`keep`] per key, and write no `?`.
    ///
    /// The per-key fallback is the **built-in default** for that key, not the
    /// merged layer underneath it: [`crate::merge`] merges the layers before
    /// anything is parsed, so by the time a value is judged there is no
    /// provenance left to fall back through. [`rejected_value_message`] says so
    /// in as many words.
    fn parsed(&self) -> (Self::Resolved, Vec<InvalidValue>);

    /// The environment layered over `layered`, key by key — the merged file
    /// value is the fallback for every knob the environment does not carry.
    ///
    /// One [`env::key`] call per migrated variable, so a set variable wins and
    /// announces once and an unusable one costs exactly one line. See
    /// [`env`]'s module doc for why this is a hand-written fan-out rather than
    /// a table of homogeneous triples.
    ///
    /// The default is "there is no environment to layer" — the right answer for
    /// any subsystem that was never spelt as `TROLLSHELL_*` variables, and the
    /// reason a new family declares nothing here. `lookup` is injected rather
    /// than read from the process: `unsafe_code = "forbid"` rules out
    /// `std::env::set_var` (an `unsafe fn` in edition 2024), so a test that
    /// drove the real environment could not exist at all.
    #[must_use]
    fn resolve(
        layered: Self::Resolved,
        _lookup: &dyn Fn(&str) -> Option<String>,
        _announce: env::Deprecations,
    ) -> Self::Resolved {
        layered
    }
}

// ── Per-key tolerance: a bad value costs its own key ─────────────────────────

/// Why a config value was rejected.
///
/// Carries the key, the spelling the user wrote and the vocabulary that was
/// expected, so the journal line is actionable without opening the source.
///
/// `value` is the value **as TOML** — quoted for a string key, bare for an
/// integer one — so the line quotes back exactly the bytes in the file the
/// reader is about to open (#1040 F11/T1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidValue {
    key: &'static str,
    value: String,
    expected: &'static str,
}

impl InvalidValue {
    /// The offending value **as the user wrote it in TOML**: a string quoted,
    /// an integer bare, a float/boolean/array/table exactly as TOML spells it
    /// (#1040 F11/T1).
    ///
    /// One constructor for every key and every TOML type, because
    /// [`toml::Value`]'s `Display` *is* the TOML rendering — so the line quotes
    /// back what the file holds, whether the mistake was a wrong word or a wrong
    /// type. It is the **canonical** rendering rather than the source bytes,
    /// which shows on a hex integer: it comes back decimal (`0xff0000` →
    /// `16711680`). A TOML date comes back quoted too, but that is **not**
    /// `Display`'s doing — `toml::Value::Datetime`'s own `Display` is unquoted,
    /// measured — it is [`assemble`]'s `IntoDeserializer` round-trip that erases
    /// the date to a `String` before this constructor ever sees it (#1040 fix
    /// round 4 F3).
    ///
    /// The environment path never produces an `InvalidValue` at all: an unusable
    /// variable gets [`env::warn_unusable_env`], which quotes with backticks
    /// because a shell variable is not TOML either.
    #[must_use]
    pub fn of(knob: &env::EnvKnob, value: &toml::Value) -> Self {
        Self::written(knob, &value.to_string())
    }

    /// The offending value already rendered as TOML — the form a test states as
    /// a literal, so the rendering itself is pinned rather than compared against
    /// another call to [`Self::of`] (#1040 mutation N6, which was green until
    /// this existed).
    #[must_use]
    pub fn written(knob: &env::EnvKnob, value: &str) -> Self {
        Self {
            key: knob.key,
            value: value.to_string(),
            // The *file* vocabulary: this diagnostic is only ever produced on
            // the file path, and the two can differ (#1040 V4).
            expected: knob.file_accepts,
        }
    }

    /// The key that was rejected — what [`load_layer`]'s journal line names.
    #[must_use]
    pub fn key(&self) -> &'static str {
        self.key
    }
}

impl std::fmt::Display for InvalidValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            key,
            value,
            expected,
        } = self;
        write!(f, "{key} = {value} is not valid; expected {expected}")
    }
}

/// The raw spelling one key is judged in: a TOML string's own contents, and for
/// **every other TOML type** its rendering — `5`, `true`, `4.0`, `[1]`.
///
/// This is the half of #1040 T1 that turns a wrong *type* into a per-key
/// rejection instead of a whole-file one. When every schema field is a raw
/// [`toml::Value`], serde accepts whatever shape the file holds and the verdict
/// lands here; a rendering no parser takes comes back as an [`InvalidValue`]
/// naming the key and quoting the value, which is exactly the treatment a wrong
/// *word* gets. Nothing about a value's type is special-cased, because to a
/// schema nothing about it is special: a spelling either parses or it does not.
#[must_use]
pub fn spelling(value: &toml::Value) -> String {
    match value {
        toml::Value::String(text) => text.clone(),
        // `toml::Value`'s `Display` *is* its TOML rendering, which is the whole
        // reason a schema keeps values rather than strings: the diagnostic
        // quotes back the bytes the reader is about to open.
        other => other.to_string(),
    }
}

/// Take the parsed value, or record why it was rejected and take the built-in
/// default for that key.
///
/// The one statement [`Subsystem::parsed`] repeats per key, and the reason it
/// can be written with no `?` in it. A free `fn` rather than a closure because a
/// subsystem uses it at as many different `T`s as it has keys.
pub fn keep<T>(result: Result<T, InvalidValue>, default: T, rejected: &mut Vec<InvalidValue>) -> T {
    match result {
        Ok(value) => value,
        Err(invalid) => {
            rejected.push(invalid);
            default
        }
    }
}

/// The **one** line a rejected value for a *known file key* produces.
///
/// #1040 V1 is why it exists: a bad value no longer takes the whole file down,
/// so the reader has to be told which key was dropped **and** what happened to
/// it. `invalid` is [`InvalidValue`]'s own rendering of "what you wrote, and
/// what was expected"; this adds the consequence.
///
/// "the built-in default" rather than "the layer below" is precise, not vague:
/// the layers are merged *before* anything is parsed, so by the time a value is
/// judged there is no provenance left to fall back through. The key falls all
/// the way to [`Subsystem::DEFAULT_TOML`]'s value, which is what the sentence
/// says.
#[must_use]
pub fn rejected_value_message(invalid: &str) -> String {
    format!("{invalid} — ignoring this key and using the built-in default")
}

/// Warn that one **file key** held a value nothing accepts, and that the
/// built-in default is being used for it.
///
/// One line per rejected key, emitted where the file is loaded — so a file with
/// two typos says two things and a file with none says nothing. The *rest* of
/// the file still applies, which is the whole reason this line exists (#1040
/// V1): before it, a single bad value was a whole-file [`ConfigError::Invalid`]
/// and the reader got one message naming one key while every *other* key
/// silently reverted too.
pub fn warn_rejected_value(subsystem: &str, key: &str, invalid: &str) {
    tracing::warn!(subsystem, key, "{}", rejected_value_message(invalid));
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
/// This is [`serde_ignored::Path`]'s own `Display`, with one difference: a hop
/// through a **wrapper** — `Option`, a newtype struct, a newtype variant —
/// contributes no segment. `Display` writes `?` for those, so a key the schema
/// does not know inside a `core: Option<Core>` arrives as `core.?.mystery`
/// while [`collect_paths`] and [`crate::merge::inert_unset`] produce
/// `core.mystery`. That mismatch is not cosmetic: [`schema_paths`] subtracts
/// one set from the other, so the user's own key inside an *optional* table
/// failed to subtract, counted as schema-owned, and [`patch`]'s stale sweep
/// deleted it.
///
/// It walks the enum rather than editing the rendered string precisely because
/// `Display` renders a wrapper hop and a **user's map key spelled `"?"`**
/// identically. Dropping `?` segments from the text folds that key to `""`,
/// which does *not* collide with the `?` [`collect_paths`] produces for it —
/// so instead of merely failing to remove the key, the sweep removes it,
/// "deleting somebody's key on uncertain information", which [`schema_paths`]
/// names as the one outcome this writer must never produce. Everything else,
/// including how a parent contributes `{parent}.` unless it is the root, is
/// [`serde_ignored`]'s rule, kept deliberately so the two spellings cannot
/// drift apart on some other exotic key.
fn dotted_key(path: &serde_ignored::Path<'_>) -> String {
    match unwrapped(path) {
        serde_ignored::Path::Seq { parent, index } => {
            format!("{}{index}", parent_prefix(parent))
        }
        serde_ignored::Path::Map { parent, key } => format!("{}{key}", parent_prefix(parent)),
        // `Root`, and the wrapper arms `unwrapped` has already peeled off.
        _ => String::new(),
    }
}

/// What a parent contributes in front of its child's segment: nothing at the
/// root, its own path and a `.` otherwise — [`serde_ignored`]'s `Display` rule,
/// asked *after* the wrappers are peeled so an `Option` hop cannot make an
/// empty prefix look like a non-empty one.
fn parent_prefix(parent: &serde_ignored::Path<'_>) -> String {
    match unwrapped(parent) {
        serde_ignored::Path::Root => String::new(),
        other => format!("{}.", dotted_key(other)),
    }
}

/// `path` with its wrapper hops peeled off. They say how the *type* is shaped —
/// an `Option`, a newtype — and never name anything a user wrote.
fn unwrapped<'a>(path: &'a serde_ignored::Path<'a>) -> &'a serde_ignored::Path<'a> {
    match path {
        serde_ignored::Path::Some { parent }
        | serde_ignored::Path::NewtypeStruct { parent }
        | serde_ignored::Path::NewtypeVariant { parent } => unwrapped(parent),
        other => other,
    }
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
    // #1022: several tests below call `assemble` directly — bypassing both
    // `capture` and the `assembled` test helper, the two call sites named in
    // `crate::test_support`'s own doc — because they assert on something
    // other than a `warn!` (an error variant, a key name, a round-tripped
    // byte string) and never needed a capture. They can still race a
    // *shared* callsite's first-ever registration against a
    // `capture()`-using test (every `tracing::warn!` in this generic function
    // is one `static` shared by every `S`, not one per monomorphization —
    // see `crate::test_support`'s doc). Compiled out entirely outside
    // `cargo test`, so this is invisible to `load`'s real, non-test callers.
    #[cfg(test)]
    crate::test_support::ensure_global_default();
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
        unknown_keys.push(dotted_key(&path));
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

/// The merged file layer as [`Subsystem::Resolved`], with every rejected key
/// reported and defaulted — **no environment**.
///
/// The `Err` here is a file that is not usable **as a file**: not TOML, or a
/// layer that exists and cannot be read. A known key holding a value nothing
/// accepts is *not* one of those (#1040 V1) — it costs its own key, warns, and
/// the rest of the file loads. Neither is a known key holding a value of the
/// wrong TOML *type* (#1040 T1), as long as the schema field is a raw
/// [`toml::Value`] and so leaves serde nothing to reject.
///
/// This is where the per-key warnings are emitted, rather than in
/// [`Subsystem::parsed`], so a subsystem may call `parsed` freely from
/// [`Subsystem::validate`] without doubling every line.
///
/// # Errors
/// As [`load_from`].
pub fn load_layer<S: Subsystem>(paths: &[PathBuf]) -> Result<S::Resolved, ConfigError> {
    let loaded = load_from::<S>(paths)?;
    let (resolved, rejected) = loaded.config.parsed();
    for invalid in &rejected {
        warn_rejected_value(S::NAME, invalid.key(), &invalid.to_string());
    }
    Ok(resolved)
}

/// [`load_layer`], degrading to `S::Resolved::default()` with a loud `error!`.
///
/// The **one** load in a subsystem's process lifetime, handed to
/// `watch::Watcher::stamping_before` so the stamp is taken first. A failure is
/// survivable by design — [`load_or_default`]'s policy applied to explicit
/// paths — because a config file nobody can parse must be visible in the journal
/// and must not stop the shell from starting.
#[must_use]
pub fn initial_load<S: Subsystem>(paths: &[PathBuf]) -> S::Resolved {
    match load_layer::<S>(paths) {
        Ok(config) => config,
        Err(e) => {
            tracing::error!(
                subsystem = S::NAME,
                error = %e,
                "config unusable; falling back to the built-in default"
            );
            S::Resolved::default()
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
        ignored.insert(dotted_key(&p));
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
///   its space would otherwise render as `{ a = 1 , b = 2 }`. An inline table
///   is the shape that has anything there; in a standard table the most that
///   rule can match is trailing whitespace on a line, in a table this save is
///   adding a key to anyway.
fn set_value(table: &mut dyn toml_edit::TableLike, key: &str, mut value: toml_edit::Value) {
    let Some(carried) = carried_suffix(table, key) else {
        close_up_for_append(table);
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

/// Make room at the end of `table` for a key about to be appended: whatever is
/// currently last stops being last, so the space it holds in front of an inline
/// table's `}` has to go or it renders as `{ a = 1 , b = 2 }`.
///
/// Called from **both** append paths — [`set_value`] for a scalar and [`patch`]
/// for a sub-table. Splitting them is what left `{ y = 4 , inner = { x = 7 } }`
/// on the table path (found by #1016's review).
fn close_up_for_append(table: &mut dyn toml_edit::TableLike) {
    if let Some(last) = last_whitespace_suffixed_key(table)
        && let Some(last_value) = table.get_mut(&last).and_then(toml_edit::Item::as_value_mut)
    {
        last_value.decor_mut().set_suffix("");
    }
}

/// Remove `key`, handing the space it held in front of an inline table's `}` to
/// whatever is last afterwards — the mirror of [`set_value`]'s carry-over rule,
/// for the one path that takes a key away instead of rewriting one. Without it
/// the sweep that #1008 shape 2 made reachable renders
/// `core = { _unset = ["label"]}`.
///
/// Self-selecting on the removed value's own suffix: only an inline table
/// normally has whitespace there, and a *comment* is never moved — it described
/// the value that is going away, the same reasoning as [`set_value`]'s. The one
/// thing it can also carry is trailing whitespace off a standard table's last
/// line, on a line the same save is already editing the table around.
fn remove_keeping_closing_space(table: &mut dyn toml_edit::TableLike, key: &str) {
    let was_last = table.iter().last().is_some_and(|(last, _)| last == key);
    let removed = table.remove(key);
    if !was_last {
        return;
    }

    let Some(space) = removed
        .as_ref()
        .and_then(toml_edit::Item::as_value)
        .and_then(|value| value.decor().suffix())
        .and_then(toml_edit::RawString::as_str)
        .filter(|raw| !raw.is_empty() && raw.chars().all(char::is_whitespace))
    else {
        return;
    };

    let Some(new_last) = table.iter().last().map(|(last, _)| last.to_owned()) else {
        return;
    };
    if let Some(value) = table
        .get_mut(&new_last)
        .and_then(toml_edit::Item::as_value_mut)
        && value
            .decor()
            .suffix()
            .and_then(toml_edit::RawString::as_str)
            == Some("")
    {
        value.decor_mut().set_suffix(space);
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
                remove_keeping_closing_space(doc, &key);
            }
        }
    }

    for (key, value) in want {
        if let toml::Value::Table(sub_want) = value {
            match doc.get(key).map(toml_edit::Item::is_table_like) {
                Some(true) => {}
                existing => {
                    // A standard table inside a standard one, an inline table
                    // inside an inline one: `TableLike::insert` converts on the
                    // way in, so the new table is spelled the way its parent
                    // is. When there was no key here at all this is an append
                    // like any other, and takes the same fix-up — replacing a
                    // key that *is* here is not, and must not touch a
                    // neighbour's bytes.
                    if existing.is_none() {
                        close_up_for_append(doc);
                    }
                    doc.insert(key, toml_edit::Item::Table(toml_edit::Table::new()));
                }
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
/// **What (2) preserves, precisely: the bytes, not the field's `None`.** The
/// marker, its comment and the keys the schema does not know survive the save,
/// and the keys the schema *does* own are gone and stay gone — that much is
/// pinned. The `Option` field's own `None`-ness does **not** survive a reload:
/// plain `serde` reads `Option<T>` as `None` only when the key is entirely
/// absent, and keeping the table for the user's lines is exactly what stops it
/// being absent. So `assemble` hands back `Some(<per-field defaults>)` — and
/// per-*field* `#[serde(default)]` means the field type's default, not the
/// struct's `Default` impl, so a save after that reload writes
/// `brightness = 0, color = ""` into the file rather than the documented
/// defaults. `an_erased_optional_table_kept_for_the_users_keys_reloads_as_some_defaults_today`
/// pins that as it behaves now, honestly and without `#[ignore]`.
///
/// Fixing it is a **reader** question, not a writer one — "a table with no
/// schema-owned key in it reads as absent for that layer" — which is why it is
/// **#1025** and not a patch to this function. Until that lands, an
/// `Option<Table>` field is a shape a subsystem author should reach for knowing
/// this.
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
        type Resolved = ();
        fn parsed(&self) -> ((), Vec<InvalidValue>) {
            ((), Vec::new())
        }
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

    /// Calls [`crate::test_support::ensure_global_default`] before touching
    /// `assemble` at all — this is the *other* required call site (see
    /// `capture`'s doc and `crate::test_support`'s): several tests call
    /// `assembled` directly, with no [`capture`] guard, so the global default
    /// has to be reachable from here too, or those tests are exactly the
    /// subscriber-less thread #1022 is about.
    fn assembled(bodies: &[&str]) -> Loaded<Leds> {
        crate::test_support::ensure_global_default();
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
            type Resolved = ();
            fn parsed(&self) -> ((), Vec<InvalidValue>) {
                ((), Vec::new())
            }
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

    // The capture harness itself now lives in `crate::test_support` (#1044):
    // this module's copy and the trollshell config tests' copy were the same
    // 100 lines twice over, and they had already drifted — one kept the
    // structured fields, the other threw them away (#1040 V10).
    use crate::test_support::{Captured, CapturedEvent, capture};

    /// The global default from #1022's fix must reach a thread that never
    /// calls [`capture`] or [`assembled`] at all — exactly the shape
    /// `an_unknown_key_is_reported_and_does_not_fail_the_load` and
    /// `an_unknown_key_inside_an_optional_table_is_named_and_kept` (elsewhere
    /// in this module) have always had: they call `assembled`/`assemble`
    /// directly, with no subscriber of their own, and before this fix could
    /// win the exact race this test constructs on purpose.
    ///
    /// # Ordering matters — this is not the same shape as the deleted
    /// "poison before any `Dispatch` exists" test
    ///
    /// [`capture`] is installed **first**, before the bare thread below ever
    /// touches the callsite — deliberately, so `capture`'s own `Dispatch::new`
    /// has already run and found nothing yet to fix (the callsite doesn't
    /// exist in the registry until the bare thread creates it). The old,
    /// pre-#1022 shape of this test called `capture` *after* the bare touch,
    /// which just re-exercises `Dispatch::new`'s unconditional full-registry
    /// rebuild — a real, still-true property, but not the one this fix adds,
    /// and it would pass even with [`crate::test_support::ensure_global_default`]
    /// deleted (I checked: it did, 0 of the mutation's failures were this
    /// test).
    ///
    /// # What actually saves this test is *not* `Rebuilder::JustOne` (PR
    /// #1043 review, finding F1)
    ///
    /// With `capture` installed first, two dispatchers are concurrently live
    /// by the time the bare thread below runs — the permanent global plus
    /// `capture`'s own ephemeral one — so `DISPATCHERS.has_just_one` is
    /// already `false` and the bare thread's first-ever registration takes
    /// `Rebuilder::Read` (the full live-registrar list), never
    /// `Rebuilder::JustOne`'s single-thread-ambient shortcut. What closes the
    /// race here is the *fold* over that full list: it can only land on
    /// `never()` if every entry says `never()`, and our permanently-registered
    /// global always says `always()`, so the folded result can't be
    /// `never()` either. See [`crate::test_support`]'s doc for the fold rule
    /// and for `a_bare_thread_touching_a_callsite_while_only_the_global_is_live`,
    /// the sibling test below that exercises the actual `JustOne` path this
    /// test's own assertion used to (wrongly) credit.
    ///
    /// The callsite here is private to this test (its own `tracing::warn!`
    /// invocation, at its own source line), so its first-ever registration is
    /// guaranteed to happen on the bare thread below, not on whichever thread
    /// some *other* test's shared callsite happens to race on.
    #[test]
    fn a_bare_thread_that_never_calls_capture_still_sees_the_global_default() {
        fn fire() {
            tracing::warn!(marker = "1022-bare-thread", "1022 bare-thread probe");
        }

        // Guarantee the global default is live before anything below runs —
        // exactly what every real `capture`/`assembled` call already does; a
        // test doesn't get to assume some *other* test won that race first.
        crate::test_support::ensure_global_default();

        // Install the capturing `Dispatch` *before* the callsite below has
        // ever been touched by anyone — its own construction has nothing
        // registered yet to rebuild, so it cannot be what saves this test.
        let (captured, _guard) = capture();

        // First-ever touch, on a bare thread with no `set_default` anywhere
        // in its stack — the scenario that used to cache `Interest::never()`
        // for whichever callsite got here first (#1022), with the capturing
        // thread's own fix-on-construction chance already spent on nothing.
        std::thread::spawn(fire).join().expect("bare thread");

        fire();
        assert!(
            captured
                .events()
                .iter()
                .any(|e| e.fields.get("marker").map(String::as_str) == Some("1022-bare-thread")),
            "a callsite first touched on a subscriber-less thread, after the \
             capturing Dispatch already existed, must still be observed: the \
             permanently-registered global default keeps the folded interest \
             off Interest::never() (Rebuilder::Read, not JustOne — see this \
             test's doc), so the first-ever touch can never cache never()"
        );
    }

    /// The narrower claim this module's other #1022 regression test doesn't
    /// actually exercise (PR #1043 review, finding F1): a **truly** bare
    /// registration, with no other `Dispatch` concurrently live anywhere in
    /// the process, so `DISPATCHERS.has_just_one` is still `true` and the
    /// callsite's first-ever registration takes `Rebuilder::JustOne`
    /// (`tracing-core = 0.1.36`'s `src/callsite.rs:561-565`) — whose
    /// `for_each` resolves the touching thread's ambient default via
    /// `dispatcher::get_default`, i.e. the global fallback this fix installs.
    ///
    /// `capture` is installed *after* the bare thread's touch here (the
    /// opposite order from the sibling test above) precisely so nothing else
    /// is registered yet when that touch happens. This makes the test **not
    /// discriminating on its own** against removing the fix entirely: with
    /// `ensure_global_default`'s body disabled, the callsite would cache
    /// `never()` on the bare touch, but `capture`'s own subsequent
    /// `Dispatch::new` unconditionally rebuilds every already-registered
    /// callsite (see `crate::test_support`'s doc) and would rescue it anyway
    /// — the same pre-existing mechanism that makes the *other* test's old,
    /// deleted ordering non-discriminating. What this test isolates instead
    /// is [`crate::test_support::AlwaysInterested`]'s `register_callsite`
    /// actually being consulted on the `JustOne` path: swap it for a
    /// subscriber whose `register_callsite` returns anything other than
    /// `Interest::always()` and this is the test that is supposed to notice.
    #[test]
    fn a_bare_thread_touching_a_callsite_while_only_the_global_is_live() {
        fn fire() {
            tracing::warn!(marker = "1043-justone", "JustOne-path probe");
        }

        crate::test_support::ensure_global_default();

        // Only the permanent global is live at this point (in isolation —
        // see the doc above on why this can't be guaranteed under the
        // default parallel test harness). First-ever touch, on a bare
        // thread, before any ephemeral `capture()` `Dispatch` exists
        // anywhere: the `Rebuilder::JustOne` shortcut is what resolves this
        // registration, not the full-list fold.
        std::thread::spawn(fire).join().expect("bare thread");

        let (captured, _guard) = capture();
        fire();
        assert!(
            captured
                .events()
                .iter()
                .any(|e| e.fields.get("marker").map(String::as_str) == Some("1043-justone")),
            "a callsite whose first-ever registration took the JustOne path \
             while only the global default was live must still be observed \
             once a real capture exists"
        );
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
            type Resolved = ();
            fn parsed(&self) -> ((), Vec<InvalidValue>) {
                ((), Vec::new())
            }
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
        type Resolved = ();
        fn parsed(&self) -> ((), Vec<InvalidValue>) {
            ((), Vec::new())
        }
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
    /// Red if `dotted_key` stops folding the wrapper hop away: `unknown_keys`
    /// reads `core.?.mystery` and the save eats `mystery`.
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

    /// The other side of the same fold, and the reason [`dotted_key`] walks
    /// [`serde_ignored::Path`] instead of editing its rendered string: a
    /// *user's* key spelled `"?"` renders exactly like a wrapper hop. Dropping
    /// `?` segments from the text folded it to `""`, which does not collide
    /// with the `?` [`collect_paths`] produces for the same key, so it stayed
    /// in the owned set and the stale sweep **deleted** it — the one outcome
    /// [`schema_paths`]'s doc says this writer must never produce.
    ///
    /// At the root and at depth, because the fold hit them differently: at
    /// depth `core.?` collapsed to `core`, naming a table the schema *owns*.
    ///
    /// The two costs are split into two tests deliberately — a single one would
    /// panic on whichever assertion came first and leave the other unproven,
    /// and it is the second that is the data loss. Both red on a `dotted_key`
    /// that matches on `?` in a rendered string (found by #1016's review).
    const WRAPPER_LOOKALIKE: [(&str, &str); 2] = [
        (
            "enabled = true\n\"?\" = 1\n\n[core]\ncolor = \"amber\"\nbrightness = 3\n",
            "?",
        ),
        (
            "enabled = true\n\n[core]\n\"?\" = 1\ncolor = \"amber\"\nbrightness = 3\n",
            "core.?",
        ),
    ];

    #[test]
    fn a_key_spelled_like_a_wrapper_hop_is_reported_under_its_own_name() {
        for (existing, expected) in WRAPPER_LOOKALIKE {
            let loaded = assemble::<Leds>(&layers(&[existing])).expect("assembles");
            assert_eq!(
                loaded.unknown_keys,
                [expected],
                "a `?` a user typed is a key, not a wrapper hop: {existing:?}"
            );
        }
    }

    #[test]
    fn a_key_spelled_like_a_wrapper_hop_survives_a_save() {
        for (existing, _) in WRAPPER_LOOKALIKE {
            let mut value = config_from(existing);
            value.core.brightness = 5;

            let out = render_overlay(existing, &value).expect("renders");

            assert!(
                out.contains("\"?\" = 1"),
                "the writer must never delete a key it did not recognise: {out}"
            );
            assert!(
                out.contains("brightness = 5"),
                "and the save itself still takes: {out}"
            );
        }
    }

    // ── #1016 review: two bytes the inline paths still moved ────────────────

    /// Appending a *sub-table* to an inline table went straight through
    /// `TableLike::insert`, skipping the fix-up [`set_value`]'s append branch
    /// does, and rendered `{ y = 4 , inner = { x = 7 } }`. Both paths now go
    /// through [`close_up_for_append`].
    ///
    /// Red on the stray space if the call in [`patch`] goes away.
    #[test]
    fn appending_a_sub_table_to_an_inline_table_does_not_leave_a_stray_space() {
        #[derive(Default, serde::Serialize, serde::Deserialize)]
        struct Inner {
            #[serde(default)]
            x: u8,
        }

        #[derive(Default, serde::Serialize, serde::Deserialize)]
        struct Outer {
            #[serde(default)]
            inner: Inner,
            #[serde(default)]
            y: u8,
        }

        #[derive(serde::Serialize, serde::Deserialize)]
        struct Nested {
            #[serde(default)]
            enabled: bool,
            #[serde(default)]
            core: Outer,
        }

        impl Subsystem for Nested {
            const NAME: &'static str = "nested";
            const DEFAULT_TOML: &'static str = "enabled = true\n";
            type Error = std::convert::Infallible;
            type Resolved = ();
            fn parsed(&self) -> ((), Vec<InvalidValue>) {
                ((), Vec::new())
            }
            fn validate(&self) -> Result<(), Self::Error> {
                Ok(())
            }
        }

        let existing = "enabled = true\ncore = { y = 4 }\n";
        let value = Nested {
            enabled: true,
            core: Outer {
                inner: Inner { x: 7 },
                y: 4,
            },
        };

        let out = render_overlay(existing, &value).expect("renders");

        assert_eq!(out, "enabled = true\ncore = { y = 4, inner = { x = 7 } }\n");
        assert_eq!(
            render_overlay(&out, &value).expect("renders again"),
            out,
            "and the result is a fixed point"
        );
    }

    /// Removing the **last** entry of an inline table took the space in front
    /// of the `}` with it, because that space was the removed value's own decor
    /// suffix — `core = { _unset = ["label"]}`. [`remove_keeping_closing_space`]
    /// hands it to whatever is last afterwards, mirroring [`set_value`]'s
    /// carry-over rule.
    ///
    /// The mirror fixture (marker last, so the removed key is not) is in the
    /// same test: it was already clean, and it is what hid the bug.
    ///
    /// Red on the missing space if the carry-over goes away.
    #[test]
    fn removing_the_last_entry_of_an_inline_table_keeps_its_closing_space() {
        let value = OptTable {
            enabled: true,
            core: None,
        };

        for existing in [
            "enabled = true\ncore = { _unset = [\"label\"], brightness = 7 }\n",
            "enabled = true\ncore = { brightness = 7, _unset = [\"label\"] }\n",
        ] {
            assert_eq!(
                render_overlay(existing, &value).expect("renders"),
                "enabled = true\ncore = { _unset = [\"label\"] }\n",
                "whichever end the schema's key sat at: {existing:?}"
            );
        }
    }

    /// **#1025, current behaviour.** What shape 2 preserves is the *bytes*, not
    /// the field's `None`. Keeping the table for the user's marker is exactly
    /// what stops the key being absent, and plain `serde` reads `Option<T>` as
    /// `None` only when it is — so the reload after the save hands back
    /// `Some(…)`, and because per-*field* `#[serde(default)]` uses the field
    /// type's default rather than `Core`'s own `Default` impl, the values are
    /// `0`/`""`/`[]` rather than the documented `3`/`"amber"`. The save after
    /// *that* writes them into the file.
    ///
    /// Closing the loop is what makes this visible: every other round-trip test
    /// here hand-builds the "after" value, so none of them ever asks what a
    /// reload of what was just written actually says.
    ///
    /// #1025 flips this — a table with no schema-owned key in it reads as
    /// absent for that layer — and the assertions below become `None` and a
    /// fixed point. It is a reader rule, not a writer one, which is why it is
    /// not fixed here. Red under exactly that mutation to `assemble`.
    #[test]
    fn an_erased_optional_table_kept_for_the_users_keys_reloads_as_some_defaults_today() {
        let existing = "enabled = true\ncore = { _unset = [\"label\"], brightness = 7 }\n";

        let erased = OptTable {
            enabled: true,
            core: None,
        };
        let saved = render_overlay(existing, &erased).expect("renders");
        assert_eq!(
            saved, "enabled = true\ncore = { _unset = [\"label\"] }\n",
            "the bytes half holds: the marker stays, the schema's key goes"
        );

        let reloaded = assemble::<OptTable>(&layers(&[&saved]))
            .expect("reloads")
            .config;
        let core = reloaded
            .core
            .as_ref()
            .expect("#1025 will flip this to `None`: the table is still there");
        assert_eq!(
            (core.brightness, core.color.as_str(), core.palette.len()),
            (0, "", 0),
            "#1025 will flip this too — and note these are the *field* types' \
             defaults, not Core::default()'s 3/\"amber\""
        );

        let again = render_overlay(&saved, &reloaded).expect("renders again");
        assert_eq!(
            again,
            "enabled = true\ncore = { _unset = [\"label\"], brightness = 0, color = \"\", palette = [] }\n",
            "#1025 will flip this to a fixed point; today the second save writes \
             the degenerate values back"
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
            type Resolved = ();
            fn parsed(&self) -> ((), Vec<InvalidValue>) {
                ((), Vec::new())
            }
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

    // ── #1040 V1: a bad value costs its own key ─────────────────────────────

    const STYLE: env::EnvKnob = env::EnvKnob::same(
        "TROLLSHELL_CORE_LEDS_STYLE",
        "style",
        "one of vfd/lcd/oled/crt",
    );
    const ROWS: env::EnvKnob = env::EnvKnob {
        var: "TROLLSHELL_CORE_LEDS_ROWS",
        key: "rows",
        env_accepts: "rect, or a row count",
        file_accepts: "0 or \"rect\", or a row count",
    };

    /// The rejected-file-value sentence, as a literal.
    ///
    /// The consequence clause is the load-bearing half: #1040 V1 was a PR that
    /// *said* a bad value left the rest of the file applied while the code
    /// dropped the whole file, and a human reading the journal has no way to
    /// tell the two apart except by what this line claims. Building the
    /// expectation from [`rejected_value_message`] would assert it against
    /// itself; `docs/live-verify.md` quotes it verbatim (#1040 mutation N9).
    #[test]
    fn the_rejected_value_sentence_is_this_exact_sentence() {
        assert_eq!(
            rejected_value_message("rows = \"many\" is not valid; expected a row count"),
            "rows = \"many\" is not valid; expected a row count \
             — ignoring this key and using the built-in default"
        );
    }

    /// [`InvalidValue`]'s own sentence, as a literal, and the **file**
    /// vocabulary in it (#1040 V4 — the diagnostic is only ever produced on the
    /// file path).
    #[test]
    fn the_invalid_value_sentence_is_this_exact_sentence() {
        assert_eq!(
            InvalidValue::written(&ROWS, "\"many\"").to_string(),
            "rows = \"many\" is not valid; expected 0 or \"rect\", or a row count"
        );
    }

    /// A value is quoted back **as TOML**: a string quoted, an integer bare, a
    /// hex integer in its canonical decimal rendering.
    ///
    /// **Red if `InvalidValue::of` stops going through `toml::Value`'s
    /// `Display`** (#1040 F11/T1, mutation N6 — which was green until the
    /// rendering was pinned against literals rather than against another call
    /// to `of`).
    #[test]
    fn a_rejected_value_is_quoted_back_as_the_toml_it_was_written_as() {
        let cases: [(toml::Value, &str); 5] = [
            ("plasma".into(), "\"plasma\""),
            (5.into(), "5"),
            (true.into(), "true"),
            (1.5.into(), "1.5"),
            (0xff_0000.into(), "16711680"),
        ];
        for (value, written) in cases {
            assert_eq!(
                InvalidValue::of(&STYLE, &value),
                InvalidValue::written(&STYLE, written),
                "{value} must be quoted back as {written}"
            );
        }
    }

    /// [`spelling`] is a string's *contents* and every other type's TOML
    /// rendering — the half of #1040 T1 that turns a wrong type into a per-key
    /// rejection rather than a whole-file one.
    #[test]
    fn a_spelling_is_the_string_itself_or_the_toml_rendering() {
        assert_eq!(
            spelling(&toml::Value::from("vfd")),
            "vfd",
            "no added quotes"
        );
        assert_eq!(spelling(&toml::Value::from(5)), "5");
        assert_eq!(spelling(&toml::Value::from(true)), "true");
        assert_eq!(spelling(&toml::Value::from(vec![1])), "[1]");
    }

    /// [`keep`] takes the value when there is one and records the rejection —
    /// **and takes that key's own default** — when there is not. One key's
    /// mistake, one key's cost (#1040 mutation R11).
    #[test]
    fn keep_records_a_rejection_and_defaults_only_that_key() {
        let mut rejected = Vec::new();

        let good = keep(Ok::<u8, InvalidValue>(7), 0, &mut rejected);
        let bad = keep(
            Err(InvalidValue::written(&ROWS, "\"many\"")),
            3,
            &mut rejected,
        );

        assert_eq!(good, 7, "a parsed value is taken as-is");
        assert_eq!(bad, 3, "…and a rejected one falls to the default handed in");
        assert_eq!(rejected, vec![InvalidValue::written(&ROWS, "\"many\"")]);
    }
}
