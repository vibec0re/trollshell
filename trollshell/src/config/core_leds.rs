//! `core-leds.toml` — how the Stats drawer's per-core LED panel (#857) is
//! dressed, and the **first live subsystem** of the #866/#868 config layering
//! (#869).
//!
//! # Why this one is the pilot
//!
//! Four knobs, brand new, entirely self-contained, and actively being fiddled
//! with — so the payoff of a file over an environment variable (edit, save,
//! watch the panel re-skin; no shell restart) is felt on the first try. Small
//! enough to throw away and redo if the shape turns out wrong, which is the
//! point of piloting before committing the other ~9 subsystems.
//!
//! # Resolution order, per key
//!
//! 1. the **environment variable**, when it is set *and* parses — with one
//!    deprecation warning at startup naming the file key it moves to
//!    ([`Deprecations`]);
//! 2. the **config layers**: `$XDG_CONFIG_DIRS/trollshell/core-leds.toml`
//!    (nix's base) then `$XDG_CONFIG_HOME/trollshell/core-leds.toml` (yours),
//!    merged by [`hytte_config::merge`]'s four rules;
//! 3. [`CoreLedsConfig::DEFAULT_TOML`] — the documented built-in default,
//!    which is the bottom merge layer, so a missing file behaves exactly like
//!    an unset variable did and says nothing about it.
//!
//! A **set but unparseable** variable keeps the pre-#869 behaviour: one
//! `warn!` naming the accepted values, then fall through to the layer below.
//! It does not win, and it does not take anything down. Exactly one line, not
//! two — the deprecation announcement is made only for a value that parsed,
//! and the unusable-value line names the key and the file itself. Both lines
//! are startup-only: a process's environment is fixed at `exec`, so a reload
//! has nothing new to say about one.
//!
//! # A bad *value* costs its own key, and only its own key
//!
//! Every key is judged on its own in [`CoreLedsConfig::parsed`]: the keys that
//! parse apply, each key that does not falls back to the built-in default with
//! one warning naming it, and [`CoreLedsConfig::validate`] is `Infallible` so
//! nothing in this schema can be a whole-file rejection (#1040 V1). What is
//! still whole-file is a layer that is not TOML **at all** — an unterminated
//! string, an integer literal too large for TOML's `i64` — which at startup
//! degrades to the built-in defaults with a loud `error!` and on a reload
//! keeps the last good file.
//!
//! # Two spellings, one parser
//!
//! The file's values are spelt exactly as the environment variables accepted
//! them — `rows` included, since [`Rows`] takes the word `"rect"` as readily as
//! the variable did. It additionally takes the TOML integer `0` for the same
//! automatic rectangle, which is what [`CoreLedsConfig::DEFAULT_TOML`] states,
//! and [`rows_spelling`] maps that back onto the variable's vocabulary so
//! **one** parser decides both — [`CoreLedsConfig::parsed`] is the only path
//! from raw spelling to [`CoreLeds`], so "what the file rejects" and "what the
//! variable rejects" cannot drift. The one deliberate divergence is that `0`,
//! which the variable never took, so the two vocabularies are stated
//! separately on the [`Knob`] (#1040 V4).
//!
//! # Live reload
//!
//! [`Watcher`] polls every layer's [`stamp`] on [`CONFIG_POLL_INTERVAL`] — the
//! `places.toml` idiom (`hytte_services::places::watch_config`), a single
//! `stat` per layer per tick, re-reading only when a stamp actually moves. A
//! reload of a layer that is not TOML **keeps the last good file layer** and
//! warns; a deleted layer falls back to the layer below it (and, with nothing
//! left, to the built-in defaults); a reload never re-announces a deprecated
//! variable, and the variable keeps winning across reloads.
//!
//! The config is stamped and then loaded **once** per process, in [`boot`];
//! the watcher does neither. Two loads at startup would double every
//! diagnostic, and stamping *after* the load loses an edit made in between
//! permanently — see [`Watcher::stamping_before`].

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::prelude::Service;
use hytte::reactive::{registry, spawn_supervised};
use hytte_config::subsystem::{self, ConfigError, Subsystem};
use hytte_config::xdg;
use hytte_preem::{ColorMap, DisplayStyle, Fill};

use super::{warn_deprecated_env, warn_rejected_value, warn_unusable_env};

// ── The resolved dressing ────────────────────────────────────────────────────

/// How the per-core LED panel is dressed (#857).
///
/// The resolved, parsed form — what the rasteriser consumes.
/// [`CoreLedsConfig`] is the *file* form, whose keys are raw strings and
/// integers so an unrecognised value can be reported rather than silently
/// substituted.
///
/// The skin and the colour map are **independent axes** (see the `hytte-preem`
/// `color_map` docs): `style = "crt"` with `color = "heat"` gives heat-mapped
/// lamps *through* the tube's scanlines, not one instead of the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoreLeds {
    /// The kit skin — the panel's physical character.
    pub style: DisplayStyle,
    /// The colour axis — what colour each lamp lights in.
    pub color: ColorMap,
    /// A pinned row count, or `None` for the automatic `rect` shape.
    pub rows: Option<usize>,
    /// What a ragged last row's leftover slots look like.
    pub fill: Fill,
}

impl Default for CoreLeds {
    fn default() -> Self {
        Self {
            // VFD: near-black field with a phosphor halo off every lit lamp —
            // the "blinken lichten" look, and the skin whose glow reads best
            // against the drawer's dark card.
            style: DisplayStyle::Vfd,
            // Heat: the panel's whole job is "which core is busy", and a
            // level-driven ramp answers that at a glance in a way a single ink
            // cannot. `color = "style"` is one line away for anyone who wants
            // the accent-tinted single ink back.
            color: ColorMap::Heat,
            rows: None,
            fill: Fill::Spare,
        }
    }
}

// ── The parsers: one vocabulary for both the file and the variable ───────────

/// Parse a `style` value. An unrecognised one returns `Err(raw)` so the caller
/// can name it.
fn parse_core_leds_style(raw: &str) -> Result<DisplayStyle, &str> {
    DisplayStyle::ALL
        .into_iter()
        .find(|s| s.name() == raw)
        .ok_or(raw)
}

/// Parse a `color` value: one of the kit's named maps, or an `#rrggbb` /
/// `rrggbb` literal for Annika's `(r, g, b)` option.
fn parse_core_leds_color(raw: &str) -> Result<ColorMap, &str> {
    if let Some(map) = ColorMap::ALL.into_iter().find(|m| m.name() == raw) {
        return Ok(map);
    }
    parse_hex_rgb(raw).ok_or(raw)
}

/// `#rrggbb` or bare `rrggbb` → a [`ColorMap::Rgb`]. Case-insensitive; any
/// other length or a non-hex digit is `None`.
fn parse_hex_rgb(raw: &str) -> Option<ColorMap> {
    let hex = raw.strip_prefix('#').unwrap_or(raw);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    Some(ColorMap::Rgb(byte(0)?, byte(2)?, byte(4)?))
}

/// The largest row count a pin may ask for.
///
/// Sized off what the panel can physically show, not off a round number. A
/// lamp row costs `CELL + GAP` = 11 buffer px (`hytte_preem::led_matrix`), the
/// panel's on-screen budget is `stats::CORE_PANEL_MAX_H` = 104 logical px and
/// the upscale factor never goes below 1× — so **9** rows already fill the
/// budget box, and past that every extra row is letterboxed back down and
/// resampled, which is the one thing the fixed dot grid exists to avoid
/// (#839/#843). The automatic shape's own appetite is smaller still: it takes
/// `⌈√cores / 2⌉` rows, which is 4 on a 64-thread box, 12 at 512 and **32** on
/// a hypothetical 4096-thread one.
///
/// 64 is therefore twice the most any real machine's automatic shape would
/// want and seven times what the budget box can render at 1× — deliberately
/// generous, because a pinned row count is the user's call to make even when
/// it is a *worse* shape than the automatic one. What it is not is unbounded:
/// `rows = 10000000` asks for a 7 GB frame (an allocation abort inside the
/// `bind` apply closure on the GTK main thread) and `rows = 9223372036854775807`
/// overflows `LedMatrix::height()`. Since #869 that value arrives from a file
/// the user edits live, where a stray digit is one keystroke — the same
/// argument `0` is already rejected on.
const MAX_ROWS: usize = 64;

/// Parse a `rows` value: `rect` for the automatic shape, or a row count in
/// `1..=MAX_ROWS`. `0` is rejected rather than silently clamped — as a *count*
/// it is a typo, not an intent — and so is anything past [`MAX_ROWS`], for the
/// reasons that constant documents.
///
/// This is the **single judge** of a row count: the file's integer spelling is
/// rendered back into this vocabulary by [`rows_spelling`] before it gets here,
/// so the cap and the rejections apply identically to
/// `TROLLSHELL_CORE_LEDS_ROWS` and to `rows =` in the file. That is the
/// property the pilot exists to prove, used.
///
/// `rect` is Annika's word for it from #857, and since her second pass on the
/// same issue ("rectangle for led view would be still more preem tho") it now
/// keeps its promise: the automatic shape is `LedMatrix::wide`'s wide
/// rectangle rather than the near-square panel #861 shipped. Still automatic
/// rather than a pinned default row count, because a fixed `rows = 4` reads
/// well at 64 cores and absurdly at 4.
fn parse_core_leds_rows(raw: &str) -> Result<Option<usize>, &str> {
    if raw == "rect" {
        return Ok(None);
    }
    match raw.parse::<usize>() {
        Ok(n) if (1..=MAX_ROWS).contains(&n) => Ok(Some(n)),
        _ => Err(raw),
    }
}

/// Parse a `fill` value: `spare` or `blank`.
fn parse_core_leds_fill(raw: &str) -> Result<Fill, &str> {
    match raw {
        "spare" => Ok(Fill::Spare),
        "blank" => Ok(Fill::Blank),
        other => Err(other),
    }
}

/// The file's `rows` value in the environment variable's vocabulary: the
/// integer `0` (and, through `#[serde(default)]`, an absent key) is `rect`,
/// and a string is the variable's own spelling already.
///
/// This is the *whole* translation between the two spellings, and it exists so
/// [`parse_core_leds_rows`] stays the single judge of a row count. A negative
/// integer renders as `-3` and is rejected by that parser, naming the value the
/// user actually wrote.
fn rows_spelling(rows: &Rows) -> String {
    match rows {
        Rows::Count(0) => "rect".to_string(),
        Rows::Count(n) => n.to_string(),
        Rows::Word(word) => word.clone(),
        // A value of neither TOML type — `rows = true`, `rows = 4.0`. Rendered
        // as the TOML it is and handed to the same judge, which rejects it and
        // names it; see [`Rows::Other`].
        Rows::Other(value) => value.to_string(),
    }
}

/// The same value **as the user wrote it in TOML**, for a diagnostic: an
/// integer bare, a string quoted (#1040 F11).
///
/// [`InvalidValue`] is only ever built on the file path, so quoting is a
/// straight improvement there — `style = "plasma"` is what the user has in
/// front of them, `style = plasma` is not TOML at all. The environment path
/// never produces one: an unusable variable gets
/// [`crate::config::warn_unusable_env`], which quotes with backticks because a
/// shell variable is not TOML either.
fn rows_as_written(rows: &Rows) -> String {
    match rows {
        Rows::Count(n) => n.to_string(),
        Rows::Word(word) => format!("{word:?}"),
        // `toml::Value`'s `Display` *is* its TOML rendering — `true`, `4.0`,
        // `[1]` — which is the whole reason the catch-all carries a
        // `toml::Value` rather than a `serde::de::IgnoredAny`.
        Rows::Other(value) => value.to_string(),
    }
}

// ── The knob table ───────────────────────────────────────────────────────────

/// One migrated knob: the environment variable that used to carry it, the
/// config key that carries it now, and the vocabulary both accept.
///
/// A table rather than four ad-hoc string literals because every message about
/// a knob — the deprecation line, the unrecognised-value warning, the
/// validation error — has to name the same three things, and the next nine
/// subsystems copy this shape.
struct Knob {
    /// The deprecated environment variable.
    var: &'static str,
    /// The `core-leds.toml` key it moved to.
    key: &'static str,
    /// What the **variable** accepts, phrased to read after "expected" in
    /// [`crate::config::unusable_env_message`].
    env_accepts: &'static str,
    /// What the **file key** accepts, phrased to read after "it accepts" in
    /// [`crate::config::deprecation_message`] and after "expected" in
    /// [`InvalidValue`]'s.
    ///
    /// Separate from [`Self::env_accepts`] for one key only, and #1040 V4 is
    /// why it has to be: `rows` is the pilot's single translation point
    /// between the two vocabularies (TOML's `0` is the file's spelling of the
    /// word `rect`, and the variable never took a `0`), so one shared string
    /// made the *variable*'s own line self-contradictory —
    /// "`0` … is not valid; expected "rect" (or 0)". Where the two agree,
    /// [`Knob::same`] states the vocabulary once.
    file_accepts: &'static str,
}

impl Knob {
    /// A knob whose file spelling and variable spelling are identical — the
    /// normal case, and the one a family-#2 author should expect to be in.
    const fn same(var: &'static str, key: &'static str, accepts: &'static str) -> Self {
        Self {
            var,
            key,
            env_accepts: accepts,
            file_accepts: accepts,
        }
    }
}

const STYLE: Knob = Knob::same(
    "TROLLSHELL_CORE_LEDS_STYLE",
    "style",
    "one of vfd/lcd/oled/crt",
);
const COLOR: Knob = Knob::same(
    "TROLLSHELL_CORE_LEDS_COLOR",
    "color",
    "one of style/rainbow/transpride/heat, or an #rrggbb literal",
);
const ROWS: Knob = Knob {
    var: "TROLLSHELL_CORE_LEDS_ROWS",
    key: "rows",
    // The variable never took a `0` — `TROLLSHELL_CORE_LEDS_ROWS=0` is
    // rejected — so its own line must not offer one (#1040 V4).
    env_accepts: "rect for the automatic rectangle, or a row count from 1 to 64",
    // Both file spellings of the automatic shape, and the cap, stated in the
    // one place a user reads about this key outside `DEFAULT_TOML` — which,
    // until nix renders a base file, exists nowhere on disk (#1040 F5).
    file_accepts: "0 or \"rect\" for the automatic rectangle, or a row count from 1 to 64",
};
const FILL: Knob = Knob::same("TROLLSHELL_CORE_LEDS_FILL", "fill", "one of spare/blank");

// ── The file schema ──────────────────────────────────────────────────────────

/// Why a `core-leds.toml` value was rejected.
///
/// Carries the key, the spelling the user wrote and the vocabulary that was
/// expected, so the journal line is actionable without opening the source.
///
/// `value` is the value **as TOML** — quoted for a string key, bare for an
/// integer one — so the line quotes back exactly the bytes in the file the
/// reader is about to open (#1040 F11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidValue {
    key: &'static str,
    value: String,
    expected: &'static str,
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

/// `core-leds.toml`, as written.
///
/// Every key is the raw spelling rather than the parsed value, because
/// [`Subsystem::validate`] has to be able to *report* an unrecognised one — a
/// `DisplayStyle` field with a custom `Deserialize` would fail the whole load
/// with serde's message instead, and #868's rule is that a known key with an
/// unusable value is named, not guessed at.
///
/// `#[serde(default)]` is on the **container**, so a key erased by an
/// `_unset = ["style"]` marker in the overlay falls back to this type's
/// documented default rather than to `String::default()` (which is `""`, a
/// value no parser accepts). There is deliberately no `deny_unknown_fields`:
/// merge rule 4 says an unknown key warns and is ignored, and
/// [`subsystem::assemble`] is what implements that.
// No `Eq`: [`Rows::Other`] carries a `toml::Value`, which can hold a float.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CoreLedsConfig {
    style: String,
    color: String,
    rows: Rows,
    fill: String,
}

/// The TOML spellings the `rows` key accepts — **and every other one**, so
/// that no `rows` value can take the file down with it.
///
/// `#[serde(untagged)]`, so `rows = 0` **and** `rows = "rect"` both
/// deserialize and both reach [`parse_core_leds_rows`], which stays the single
/// judge. That is not a convenience: `rect` is the word the deprecated
/// `TROLLSHELL_CORE_LEDS_ROWS` accepted and the word the deprecation line walks
/// a migrating user toward, so a file that rejected it broke #869's own
/// contract.
///
/// # Why there is a catch-all variant
///
/// Every *other* key here is a `String`, so serde accepts whatever the user
/// wrote and [`CoreLedsConfig::parsed`] judges it per key. `rows` is the one
/// key with a non-string schema, and without [`Self::Other`] serde would
/// decide its fate first: `rows = true`, `rows = 4.0`, `rows = [1]` all failed
/// as a **whole-file** `ConfigError::Schema` before the subsystem saw a byte,
/// taking every other key in the file down with the typo (#1040 V9). A
/// catch-all restores the invariant the pilot is meant to demonstrate — *a bad
/// value costs its own key and nothing else* — and it costs one variant.
///
/// The residual whole-file cases are now exactly the ones where the file is
/// **not TOML**: an unterminated string, and an integer literal too large for
/// TOML's `i64` (`rows = 9223372036854775808`, a `ConfigError::Parse` from the
/// TOML lexer). Neither ever reaches serde, let alone this type; both are
/// tested (`a_file_that_is_not_toml_is_still_a_whole_file_error`).
///
/// A family-#2 author copying this needs the rule, not the enum: **a schema
/// field whose type is narrower than "any TOML scalar" hands its verdict to
/// serde, and serde's verdict is whole-file.** Keep fields raw, or give them a
/// catch-all.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Rows {
    /// A TOML integer: `0` is the automatic rectangle, anything else is a row
    /// count for [`parse_core_leds_rows`] to judge.
    Count(i64),
    /// A TOML string: the environment variable's own vocabulary, handed to
    /// [`parse_core_leds_rows`] untouched.
    Word(String),
    /// Anything else the file said — kept as the TOML it was so the diagnostic
    /// can quote it back, then rejected per key like any other bad value.
    Other(toml::Value),
}

impl Default for CoreLedsConfig {
    /// The documented default in Rust form.
    ///
    /// Pinned equal to [`Self::DEFAULT_TOML`]'s parse *and* to
    /// [`CoreLeds::default`] by `the_documented_default_is_the_built_in_one`,
    /// so this cannot drift from either. It exists as a Rust value because
    /// `#[serde(default)]` needs one and because the resolver needs a
    /// panic-free fallback for the (our-bug) case where the built-in TOML
    /// itself stops parsing.
    fn default() -> Self {
        Self {
            style: DisplayStyle::Vfd.name().to_string(),
            color: ColorMap::Heat.name().to_string(),
            rows: Rows::Count(0),
            fill: "spare".to_string(),
        }
    }
}

impl CoreLedsConfig {
    /// The file's raw spellings as the resolved [`CoreLeds`], beside **every**
    /// key that did not parse.
    ///
    /// The one path from spelling to value: the resolver is this call with the
    /// environment layered on top, and nothing else may parse a
    /// `core-leds.toml` value.
    ///
    /// # A bad value costs its own key, and only its own key (#1040 V1)
    ///
    /// This returns a `CoreLeds` **and** a list, not a `Result`, and that is
    /// the whole shape of the fix. It used to `?` on the first bad key and
    /// hand the error to [`Subsystem::validate`], where
    /// `hytte_config::subsystem::assemble` turns any validation failure into a
    /// whole-file `ConfigError::Invalid`. The *message* named one key; the
    /// *effect* dropped all four. A file saying
    ///
    /// ```toml
    /// style = "crt"
    /// rows  = "many"
    /// ```
    ///
    /// rendered the stock VFD panel with `style` silently gone, while the
    /// journal talked only about `rows` — the reader's three good keys looked
    /// ignored for no reason. Now `style` applies, `rows` falls back to the
    /// built-in default, and exactly one line names `rows`.
    ///
    /// The per-key fallback is the **built-in default** for that key, not the
    /// merged layer underneath it: [`hytte_config::merge`] merges the layers
    /// before anything is parsed, so by the time a value is judged there is no
    /// provenance left to fall back through. That is what
    /// [`crate::config::rejected_value_message`] says in as many words.
    ///
    /// This is also the template half a family #2 copies, and the rule is
    /// short: **keep every schema field raw and judge it here.** A field typed
    /// as the parsed value hands its verdict to serde, and serde's verdict is
    /// whole-file (see [`Rows::Other`] for the one key that needed a catch-all
    /// to get out of that).
    fn parsed(&self) -> (CoreLeds, Vec<InvalidValue>) {
        /// Take the parsed value, or record why it was rejected and take the
        /// built-in default for that key. A free `fn` rather than a closure
        /// because it is used at four different `T`s.
        fn keep<T>(
            result: Result<T, InvalidValue>,
            default: T,
            rejected: &mut Vec<InvalidValue>,
        ) -> T {
            match result {
                Ok(value) => value,
                Err(invalid) => {
                    rejected.push(invalid);
                    default
                }
            }
        }

        let mut rejected = Vec::new();
        let fallback = CoreLeds::default();
        let rows = rows_spelling(&self.rows);
        let leds = CoreLeds {
            style: keep(
                parse_core_leds_style(&self.style).map_err(|bad| InvalidValue::of(&STYLE, bad)),
                fallback.style,
                &mut rejected,
            ),
            color: keep(
                parse_core_leds_color(&self.color).map_err(|bad| InvalidValue::of(&COLOR, bad)),
                fallback.color,
                &mut rejected,
            ),
            // The offending value is reported in the spelling the *file* uses,
            // not the one the parser was handed: `rows = 0` is spelt `rect`
            // going in, and echoing `rect` back at a user who wrote something
            // else would name a value that is nowhere in their file.
            rows: keep(
                parse_core_leds_rows(&rows)
                    .map_err(|_| InvalidValue::written(&ROWS, &rows_as_written(&self.rows))),
                fallback.rows,
                &mut rejected,
            ),
            fill: keep(
                parse_core_leds_fill(&self.fill).map_err(|bad| InvalidValue::of(&FILL, bad)),
                fallback.fill,
                &mut rejected,
            ),
        };
        (leds, rejected)
    }
}

impl InvalidValue {
    /// The offending value of a **string** key, rendered as the quoted TOML
    /// the user wrote.
    fn of(knob: &Knob, value: &str) -> Self {
        Self::written(knob, &format!("{value:?}"))
    }

    /// The offending value already rendered as TOML — for `rows`, whose two
    /// spellings quote differently ([`rows_as_written`]).
    fn written(knob: &Knob, value: &str) -> Self {
        Self {
            key: knob.key,
            value: value.to_string(),
            // The *file* vocabulary: this diagnostic is only ever produced on
            // the file path, and for `rows` the two differ (#1040 V4).
            expected: knob.file_accepts,
        }
    }
}

impl Subsystem for CoreLedsConfig {
    const NAME: &'static str = "core-leds";

    const DEFAULT_TOML: &'static str = r##"# The Stats drawer's per-core LED panel (#857): one lamp per CPU core, each
# lit to that core's load. Every key here is look-and-feel — none of it
# changes what is measured.
#
# This file is read live: save an edit and the panel re-skins within a few
# seconds, with no shell restart. It is also layered — a nix-written base
# under $XDG_CONFIG_DIRS, your own edits under $XDG_CONFIG_HOME — so a
# rebuild never clobbers a hand edit and a hand edit never blocks a rebuild.
# Delete a key to fall back to the value below; `_unset = ["style"]` erases a
# key an underlying layer set (TOML has no null, so this is how it is spelt).
#
# A value no parser accepts costs its own key and nothing else: that key takes
# the built-in default, one journal line names it, and every other key in the
# file still applies.

# The kit skin — the panel's physical character.
#   vfd         near-black field with a phosphor halo off every lit lamp
#   lcd         grey-green cells that ghost their unlit segments
#   oled        pure black field, no ghost
#   crt         scanline comb and a curved-glass vignette
style = "vfd"

# The colour axis, and it is *independent* of the skin: style = "crt" with
# color = "heat" gives heat-mapped lamps through the tube's scanlines.
#   heat        blue-to-red ramp by load — "which core is busy", at a glance
#   style       the skin's own single ink (the pre-#857 look)
#   rainbow     a hue sweep across the lamps
#   transpride  the flag's bands across the lamps
#   "#rrggbb"   one literal colour
color = "heat"

# Rows in the lamp matrix. 0 — or the word "rect", which is what the deprecated
# TROLLSHELL_CORE_LEDS_ROWS took, and which this key accepts too — is the
# automatic wide rectangle, picked from the core count (16x4 on a 64-thread
# box, 4x1 at 4 cores). Any number from 1 to 64 pins the row count instead and
# the columns fall out of it; past 64 a row is thinner than a pixel on screen,
# so it is rejected as the typo it almost certainly is.
rows = 0

# What a ragged last row's leftover slots look like. Only visible when the row
# count divides unevenly *and* the skin ghosts (vfd/lcd).
#   spare       unlit lamps fill the tail
#   blank       the tail is left bare
fill = "spare"
"##;

    /// **Nothing** can fail whole-file validation here, and the type says so.
    ///
    /// `hytte_config::subsystem::assemble` maps any `validate` error onto
    /// `ConfigError::Invalid`, which is a **whole-file** rejection — the load
    /// returns no config at all and the caller falls back to the built-in
    /// defaults (at startup) or keeps the last good file (on a reload). That
    /// is the right contract for a config whose keys are interdependent, and
    /// the wrong one for this pilot, whose four knobs are independent
    /// look-and-feel values: one typo would revert the other three (#1040 V1).
    ///
    /// So the judgement moves into [`CoreLedsConfig::parsed`], which reports
    /// every bad key and defaults *that key* — and `Infallible` is then the
    /// honest `Error`, exactly as this trait's own doc suggests ("when there
    /// is nothing the type system did not already catch"). A family #2 whose
    /// keys really do constrain each other should use a real error here; one
    /// whose keys are independent should copy this.
    type Error = std::convert::Infallible;

    /// See [`Self::Error`]: every value is judged per key in
    /// [`CoreLedsConfig::parsed`], so there is nothing left for the whole-file
    /// gate to reject.
    fn validate(&self) -> Result<(), Self::Error> {
        Ok(())
    }
}

// ── Resolution: the environment over the file, per key ───────────────────────

/// Whether this resolution is the startup one (which announces every
/// deprecated variable that is set) or a reload (which announces nothing).
///
/// An explicit parameter rather than a `Once` latch: the once-ness is then a
/// property of the two call sites — `start` announces, [`watch`] does not —
/// which a test can drive directly, instead of process-global state that the
/// second test in a binary can no longer observe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Deprecations {
    /// Warn once per set variable, naming the file key it moves to.
    Announce,
    /// Say nothing: this environment was already announced at startup.
    Silent,
}

/// Resolve one knob: the environment variable if it is set, otherwise
/// `fallback` (the merged file value).
///
/// A set variable **wins**, and warns that it is deprecated. A set variable
/// that does not parse warns about the value and falls through to `fallback` —
/// the pre-#869 behaviour, except that "the layer below" is now the config
/// file rather than the hard-coded default.
///
/// **One warning, not two** (#1040 F9). The announcement is made *after* the
/// parse and only for a value that actually parsed: a set-but-unusable variable
/// gets [`crate::config::warn_unusable_env`] instead, a single line that
/// already names the key and the file the value should move to. Announcing
/// first would have handed the reader two half instructions for one mistake.
fn env_key<'a, T>(
    knob: &Knob,
    raw: Option<&'a str>,
    parse: impl FnOnce(&'a str) -> Result<T, &'a str>,
    fallback: T,
    announce: Deprecations,
) -> T {
    let Some(raw) = raw else { return fallback };
    match parse(raw) {
        Ok(value) => {
            if announce == Deprecations::Announce {
                // The *file* vocabulary: this line's job is to tell the reader
                // what to type in the file it names.
                warn_deprecated_env(CoreLedsConfig::NAME, knob.var, knob.key, knob.file_accepts);
            }
            value
        }
        Err(bad) => {
            // Gated on `announce` exactly like the line above (#1040 V7).
            // The earlier reading — "an unusable variable is still unusable
            // after a reload, so this is a fact rather than an announcement" —
            // was true and still produced a line every three seconds for the
            // life of the shell. A process's environment is fixed at `exec`
            // (nothing here calls `set_var`; it is an `unsafe fn`), so the
            // repeat could never carry news, and `live-verify.md` promises the
            // reader **exactly one**.
            if announce == Deprecations::Announce {
                // The *variable* vocabulary: this line is about what the
                // variable would have had to say, and for `rows` the file
                // accepts a spelling the variable never did (#1040 V4).
                warn_unusable_env(
                    CoreLedsConfig::NAME,
                    knob.var,
                    bad,
                    knob.key,
                    knob.env_accepts,
                );
            }
            fallback
        }
    }
}

/// The environment layered over `layered`, key by key — the merged file value
/// is the fallback for every knob the environment does not carry.
///
/// `lookup` is injected rather than read from the process: `unsafe_code =
/// "forbid"` rules out `std::env::set_var` (it is an `unsafe fn` in edition
/// 2024), so a test that drove the real environment could not exist at all,
/// and one that read it would depend on the developer's shell.
fn resolve(
    layered: CoreLeds,
    lookup: &dyn Fn(&str) -> Option<String>,
    announce: Deprecations,
) -> CoreLeds {
    let (style, color, rows, fill) = (
        lookup(STYLE.var),
        lookup(COLOR.var),
        lookup(ROWS.var),
        lookup(FILL.var),
    );
    CoreLeds {
        style: env_key(
            &STYLE,
            style.as_deref(),
            parse_core_leds_style,
            layered.style,
            announce,
        ),
        color: env_key(
            &COLOR,
            color.as_deref(),
            parse_core_leds_color,
            layered.color,
            announce,
        ),
        rows: env_key(
            &ROWS,
            rows.as_deref(),
            parse_core_leds_rows,
            layered.rows,
            announce,
        ),
        fill: env_key(
            &FILL,
            fill.as_deref(),
            parse_core_leds_fill,
            layered.fill,
            announce,
        ),
    }
}

/// The process environment, for the production call sites.
fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

// ── Live reload ──────────────────────────────────────────────────────────────

/// How often the running shell re-checks the config layers. Each tick is one
/// `stat` per layer (two, normally: the nix base and the overlay) on a cached
/// inode, so it stays snappy while you edit at no measurable idle cost; the
/// files are only re-read when a stamp actually moves.
///
/// `places.toml`'s AC cadence (`hytte_services::places::CONFIG_POLL_INTERVAL`).
/// It does *not* slow down on battery the way places does since #505: that
/// needs `upower::on_battery_snapshot`, which is `pub(crate)` to
/// `hytte-services` and so unreachable from the binary. Noted rather than
/// worked around — widening it is a `hytte-services` change and belongs to
/// whichever subsystem migration first needs it.
const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// A layer's change stamp — its last-modified time **and** its length — or
/// `None` when it does not exist (the normal case for the overlay) or cannot
/// be stat'd.
///
/// The length is not decoration (#1040 F8). An mtime-only stamp misses an edit
/// saved inside the same mtime granule as the poll's own `stat`, and misses it
/// **permanently**: the stamp is updated unconditionally, so the movement is
/// never seen again and the panel stays one save stale until the *next* edit.
/// Linux's ext4/btrfs/tmpfs carry nanoseconds, so on this shell's own machine
/// the window is theoretical — but the poller is the piece nine more subsystems
/// copy, and a coarse-granularity filesystem (a network mount, a FAT-formatted
/// stick someone points `XDG_CONFIG_DIRS` at) is not exotic. `places`'
/// `ConfigWatcher` predates this and is mtime-only; its test overlay sets the
/// mtime by hand precisely because of the same window.
///
/// A length is a cheap discriminator on the same `stat` call, not a second
/// syscall, and it catches the overwhelmingly common shape of a same-granule
/// edit (a value getting longer or shorter). It is not a hash: two edits that
/// land in one granule *and* keep the byte count identical are still missed,
/// which is the honest limit of stat-polling and the reason a real watch
/// (inotify) is the eventual answer rather than a finer stamp.
fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Every layer's [`stamp`], in path order — one `stat` per layer.
fn stamps_of(paths: &[PathBuf]) -> Vec<Option<(SystemTime, u64)>> {
    paths.iter().map(|p| stamp(p)).collect()
}

/// The merged file layer, with every rejected key reported and defaulted — no
/// environment.
///
/// The `Err` here is a file that is not usable **as a file**: not TOML, or a
/// layer that exists and cannot be read. A known key holding a value nothing
/// accepts is *not* one of those (#1040 V1) — it costs its own key, warns, and
/// the rest of the file loads.
fn load_layer(paths: &[PathBuf]) -> Result<CoreLeds, ConfigError> {
    let loaded = subsystem::load_from::<CoreLedsConfig>(paths)?;
    let (leds, rejected) = loaded.config.parsed();
    for invalid in &rejected {
        warn_rejected_value(CoreLedsConfig::NAME, invalid.key, &invalid.to_string());
    }
    Ok(leds)
}

/// Watches every `core-leds.toml` layer for live reload by polling their
/// mtimes, in the shape `hytte_config::places::ConfigWatcher` established.
///
/// Holds the last file layer that loaded cleanly, which is what a malformed
/// save keeps: the panel goes on rendering the last good skin rather than
/// snapping back to the built-in default the moment a hand edit is mid-word.
#[derive(Clone)]
struct Watcher {
    paths: Vec<PathBuf>,
    stamps: Vec<Option<(SystemTime, u64)>>,
    last_good: CoreLeds,
}

impl Watcher {
    /// **Stamp `paths`, then read them with `load`** — in that order, which is
    /// the entire contract of this constructor.
    ///
    /// The one load is `load`'s, and it is a parameter rather than a call to
    /// [`initial_load`] for two reasons. The first is #1040 F1: the constructor
    /// used to load *by itself* while `start` had already loaded, so one typo
    /// produced two `unknown key` warnings and one broken file two `config
    /// unusable` errors. The second is that the ordering is otherwise a rule
    /// nothing enforces — two statements in a caller, swappable without a
    /// single test noticing (measured: mutation V2a green against a test that
    /// had inlined the two lines itself). With the load handed in, the order
    /// lives *here*, in one place, and a test can hand in a `load` that edits
    /// the file on its way out and watch the next poll either see it or lose
    /// it.
    ///
    /// The order matters because the baseline has to predate the value. Stamp
    /// first and an edit landing in the window is read by this very load, with
    /// a stamp that predates it — so the next [`Self::poll`] sees the stamp
    /// move and re-reads, and the edit is one tick late at worst. Stamp *after*
    /// and the baseline already includes an edit the published value does not,
    /// and because `poll` updates its stamps unconditionally that edit is
    /// missed **forever**. Measured on the previous head: file said `crt`,
    /// panel said `lcd`, three polls, nothing.
    ///
    /// A supervised restart of [`watch`] resumes from the same baseline — the
    /// watcher is `Clone` and the factory hands out a copy — so an edit made
    /// during a panic-and-restart is picked up by the next poll rather than
    /// silently baselined away. `places::ConfigWatcher::new` splits the load
    /// out the same way; the stamp ordering is this pilot's addition.
    fn stamping_before(paths: Vec<PathBuf>, load: impl FnOnce(&[PathBuf]) -> CoreLeds) -> Self {
        let stamps = stamps_of(&paths);
        let last_good = load(&paths);
        Self {
            paths,
            stamps,
            last_good,
        }
    }

    /// The dressing this environment and the current file layers resolve to.
    fn resolved(
        &self,
        lookup: &dyn Fn(&str) -> Option<String>,
        announce: Deprecations,
    ) -> CoreLeds {
        resolve(self.last_good, lookup, announce)
    }

    /// Reload and return the fresh dressing when some layer's mtime has moved
    /// *and* the result differs from `current`; otherwise `None`.
    ///
    /// A layer that stops **parsing as TOML** keeps [`Self::last_good`] and
    /// warns — once per edit rather than once per tick, because the stamp is
    /// taken before the load, so a file left malformed is not re-read until it
    /// is saved again. That is the mid-word save: half a string typed, the
    /// panel should not flicker.
    ///
    /// A layer that parses but holds a value nothing accepts is a *different*
    /// case since #1040 V1 and is **not** an error: the good keys apply, the
    /// bad key takes the built-in default, and one warning names it. The
    /// asymmetry is deliberate — `style = "cr` is a file caught mid-edit,
    /// `style = "crt"` beside `rows = "many"` is a finished file with one
    /// mistake in it, and reverting the other three keys to punish the fourth
    /// is what V1 was filed about.
    ///
    /// A layer that is **deleted** is a different case and is deliberately not
    /// treated as an error (#1040 F7): its stamp goes to `None`, the load
    /// succeeds over the layers that remain, and the panel goes back to the
    /// built-in defaults. Deleting a config file is an intent — "I want the
    /// stock look back" — not a mistake, and it is the only way to get the
    /// defaults back without hand-restoring every key. `live-verify.md` says so
    /// too; `a_deleted_file_falls_back_to_the_defaults` pins it.
    fn poll(
        &mut self,
        current: CoreLeds,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Option<CoreLeds> {
        let now = stamps_of(&self.paths);
        if now == self.stamps {
            return None;
        }
        self.stamps = now;
        match load_layer(&self.paths) {
            Ok(config) => self.last_good = config,
            Err(e) => tracing::warn!(
                subsystem = CoreLedsConfig::NAME,
                error = %e,
                "config changed but is unusable; keeping the last good one"
            ),
        }
        // Silent: the environment has not changed, and this runs every few
        // seconds for the life of the shell.
        let next = self.resolved(lookup, Deprecations::Silent);
        (next != current).then_some(next)
    }
}

/// Poll the layers and republish on a real change, so an edit reaches the
/// panel within `interval` without restarting the shell.
///
/// Takes the [`Watcher`] rather than building one: [`boot`] already stamped
/// and loaded, in that order, and re-doing either here would put back the
/// double diagnostic (#1040 F1) and the lost-edit window (#1040 V2). The
/// watcher is `Clone`, which is what lets this ride `spawn_supervised`'s `Fn`
/// factory — a restart resumes from the same baseline instead of re-stamping.
///
/// `lookup` and `interval` are parameters for the same reason `paths` is one:
/// the loop is then drivable in a test at a cadence a test can wait for, and
/// [`CoreLedsService`] is the single place production values are chosen.
async fn watch(
    leds: Mutable<CoreLeds>,
    mut watcher: Watcher,
    lookup: EnvLookup,
    interval: Duration,
) {
    loop {
        tokio::time::sleep(interval).await;
        if let Some(next) = watcher.poll(leds.get(), &*lookup) {
            tracing::info!(subsystem = CoreLedsConfig::NAME, "config changed; reloaded");
            leds.set(next);
        }
    }
}

/// The merged file layer, or the built-in default with a loud `error!`.
///
/// The **one** load in the process's life. A failure is survivable by design —
/// [`subsystem::load_or_default`]'s policy applied to explicit paths — because
/// a config file nobody can parse must be visible in the journal and must not
/// stop the shell from starting.
fn initial_load(paths: &[PathBuf]) -> CoreLeds {
    match load_layer(paths) {
        Ok(config) => config,
        Err(e) => {
            tracing::error!(
                subsystem = CoreLedsConfig::NAME,
                error = %e,
                "config unusable; falling back to the built-in default"
            );
            CoreLeds::default()
        }
    }
}

/// The process's **one** startup sequence: stamp the layers, load them once,
/// resolve the environment over them announcing every deprecated variable that
/// is set, and hand back the dressing to publish beside the poller that will
/// keep it fresh.
///
/// # Stamp, then load (#1040 V2)
///
/// The ordering lives in [`Watcher::stamping_before`] rather than in two
/// statements here, which is the point: as two statements it was a rule
/// nothing enforced, and a mutation that swapped them left the suite green.
/// There is no re-read loop on purpose — erring toward "poll again" is free,
/// and a stat-read-stat retry would only narrow a window that is already
/// harmless in that direction.
///
/// The watcher is seeded with the **file layer**, not the resolved dressing:
/// resolving is where the environment wins, and seeding with a value the
/// environment already overrode would make the file's own value unrecoverable
/// on the first reload.
///
/// This is the only place in production that passes
/// [`Deprecations::Announce`], and it takes `paths`/`lookup` rather than
/// reaching for the process so the real call site is drivable in a test
/// (#1040 F3/V3).
fn boot(paths: &[PathBuf], lookup: &dyn Fn(&str) -> Option<String>) -> (CoreLeds, Watcher) {
    let watcher = Watcher::stamping_before(paths.to_vec(), initial_load);
    let resolved = resolve(watcher.last_good, lookup, Deprecations::Announce);
    (resolved, watcher)
}

// ── The service ──────────────────────────────────────────────────────────────

/// The handle the Stats panel subscribes to.
pub struct CoreLedsHandles {
    leds: Mutable<CoreLeds>,
}

/// How a knob's deprecated environment variable is read.
///
/// A boxed `Fn` rather than a direct `std::env::var`, because `unsafe_code =
/// "forbid"` rules out `std::env::set_var` (an `unsafe fn` in edition 2024): a
/// test that drove the real environment could not exist at all, and one that
/// read it would depend on the developer's shell.
type EnvLookup = std::sync::Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The `core-leds.toml` service: one startup load, one announcing resolution,
/// and a supervised poller.
///
/// # Why it carries its three inputs instead of reaching for them
///
/// [`Service::start`] is what the process actually runs, and until #1040 V3 it
/// was the one function here with **zero** coverage — every test drove a
/// hand-rolled replica of it, so mutations that made `start` load twice, or
/// skip the announcing resolution entirely, left the suite green. Nobody would
/// ever have heard a deprecation line again and CI would not have noticed.
///
/// Holding `paths`, `lookup` and `interval` as fields means a test constructs
/// this over a scratch overlay and a fake environment and then calls the
/// **real** `start`. What is left unpinned is [`service`]'s three argument
/// expressions — deliberately, and that is as thin as this can get without
/// `set_var`: they are `xdg::config_layers`, `process_env` and one `const`,
/// each covered on its own elsewhere.
pub struct CoreLedsService {
    /// Layer paths, lowest precedence first.
    paths: Vec<PathBuf>,
    /// How a deprecated variable is read.
    lookup: EnvLookup,
    /// How often [`watch`] re-stats the layers.
    interval: Duration,
}

impl Service for CoreLedsService {
    type Handles = CoreLedsHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        // One stamp, one load, one announcing resolution — all inside `boot`,
        // in that order. Nothing here chooses a `Deprecations` or a stamp
        // ordering, so this call site cannot get either wrong.
        let (resolved, watcher) = boot(&self.paths, &*self.lookup);
        let leds = Mutable::new(resolved);
        let Self {
            lookup, interval, ..
        } = self;
        spawn_supervised("core-leds", {
            let leds = leds.clone();
            move || watch(leds.clone(), watcher.clone(), lookup.clone(), interval)
        });
        CoreLedsHandles { leds }
    }
}

pub fn service() -> CoreLedsService {
    CoreLedsService {
        paths: xdg::config_layers(CoreLedsConfig::NAME),
        lookup: std::sync::Arc::new(process_env),
        interval: CONFIG_POLL_INTERVAL,
    }
}

/// Signal of the per-core LED panel's dressing, re-firing on every live config
/// reload that actually changes something.
pub fn signal() -> impl Signal<Item = CoreLeds> {
    registry::with(|r| {
        r.get::<CoreLedsHandles>()
            .expect("config::core_leds::service() not registered")
            .leds
            .signal()
    })
}

#[cfg(test)]
mod tests {
    use super::{
        COLOR, CONFIG_POLL_INTERVAL, CoreLeds, CoreLedsConfig, CoreLedsService, Deprecations, FILL,
        InvalidValue, MAX_ROWS, Mutable, ROWS, Rows, STYLE, Service, Watcher, boot, initial_load,
        parse_core_leds_color, parse_core_leds_fill, parse_core_leds_rows, parse_core_leds_style,
        parse_hex_rgb, resolve, rows_spelling, watch,
    };
    use hytte_config::subsystem::{self, Subsystem};
    use hytte_preem::{ColorMap, DisplayStyle, Fill};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    /// An environment that is not the process's: the injection point every
    /// resolution test drives, since `std::env::set_var` is an `unsafe fn` and
    /// this workspace forbids `unsafe`.
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn no_env() -> impl Fn(&str) -> Option<String> {
        env(&[])
    }

    /// What the file's keys resolved to, whatever it also rejected — the
    /// half of [`CoreLedsConfig::parsed`] the panel renders.
    fn applied(config: &CoreLedsConfig) -> CoreLeds {
        config.parsed().0
    }

    /// Every key the file was rejected on, in schema order — the half a
    /// journal line is written for.
    ///
    /// This is what `validate()` used to return, and the difference is the
    /// point of #1040 V1: a `Vec` means "these keys took the built-in default
    /// and the others applied", where the old `Result` meant "the whole file
    /// is gone".
    fn rejections(config: &CoreLedsConfig) -> Vec<InvalidValue> {
        config.parsed().1
    }

    /// The one key a config was rejected on.
    ///
    /// # Panics
    /// If it was rejected on none, or on more than one — either way the test
    /// asking is asking the wrong question.
    fn only_rejection(config: &CoreLedsConfig) -> InvalidValue {
        let mut rejected = rejections(config);
        assert_eq!(rejected.len(), 1, "expected exactly one rejected key");
        rejected.remove(0)
    }

    // ── The documented default ──────────────────────────────────────────────

    /// **The pin the whole pilot rests on.** `DEFAULT_TOML` is the bottom
    /// merge layer, so a missing file must behave *exactly* as an unset
    /// environment variable did before #869 — which is only true if the
    /// documented TOML, the `serde` fallback and the pre-#869 hard-coded
    /// defaults are all the same three values.
    ///
    /// Red if any of the three moves without the others: reword a default in
    /// the TOML, change `CoreLedsConfig::default`, or change
    /// `CoreLeds::default`.
    #[test]
    fn the_documented_default_is_the_built_in_one() {
        let loaded = subsystem::assemble::<CoreLedsConfig>(&[]).expect("DEFAULT_TOML assembles");

        assert_eq!(
            loaded.config,
            CoreLedsConfig::default(),
            "DEFAULT_TOML and the serde fallback must be the same config"
        );
        assert_eq!(
            applied(&loaded.config),
            CoreLeds::default(),
            "…and both must parse to the dressing the env path produced when unset"
        );
        assert!(
            loaded.unknown_keys.is_empty(),
            "the documented default must not carry a key the schema does not know: {:?}",
            loaded.unknown_keys
        );
    }

    /// The format-preserving writer #888's settings forms will use works on
    /// this schema: saving the documented defaults back over the documented
    /// file is a **byte no-op**, and what it produces still reparses.
    ///
    /// Red if a schema key stops serialising (the writer would then treat it
    /// as the user's own key), or if `DEFAULT_TOML` states a value the writer
    /// disagrees with (it would rewrite that line).
    #[test]
    fn saving_the_defaults_over_the_documented_file_changes_nothing() {
        let body =
            subsystem::render_overlay(CoreLedsConfig::DEFAULT_TOML, &CoreLedsConfig::default())
                .expect("renders");

        assert_eq!(
            body,
            CoreLedsConfig::DEFAULT_TOML,
            "writing the documented defaults back must not touch a byte"
        );
        let back = subsystem::assemble::<CoreLedsConfig>(&[(PathBuf::from("/o.toml"), body)])
            .expect("reparses");
        assert_eq!(back.config, CoreLedsConfig::default());
    }

    // ── The two spellings agree ─────────────────────────────────────────────

    /// Every value the knobs accept, in both spellings, and every value they
    /// reject — asserted as *the same set*, because #869's contract is that
    /// the file rejects exactly what the variable rejected.
    #[test]
    fn the_file_rejects_exactly_what_the_env_parsers_reject() {
        // Accepted: every skin, by the kit's own `name()` vocabulary.
        for style in DisplayStyle::ALL {
            assert_eq!(parse_core_leds_style(style.name()), Ok(style));
            assert!(rejections(&with_style(style.name())).is_empty());
        }
        // Accepted: every named map, plus the `#rrggbb` literal.
        for map in ColorMap::ALL {
            assert_eq!(parse_core_leds_color(map.name()), Ok(map));
        }
        assert_eq!(
            parse_core_leds_color("#9b59b6"),
            Ok(ColorMap::Rgb(0x9b, 0x59, 0xb6))
        );
        assert_eq!(parse_core_leds_rows("rect"), Ok(None));
        assert_eq!(parse_core_leds_rows("3"), Ok(Some(3)));
        assert_eq!(parse_core_leds_fill("spare"), Ok(Fill::Spare));
        assert_eq!(parse_core_leds_fill("blank"), Ok(Fill::Blank));

        // Rejected by the parser, and therefore by `validate`, with the key
        // and the offending spelling named.
        assert_eq!(parse_core_leds_style("plasma"), Err("plasma"));
        assert_eq!(
            rejections(&with_style("plasma")),
            [InvalidValue::of(&STYLE, "plasma")]
        );

        assert_eq!(parse_core_leds_color("puce"), Err("puce"));
        // `rgb` is `ColorMap::name`'s output but not an input: it carries no
        // components, so accepting it would mean inventing a colour.
        assert_eq!(parse_core_leds_color("rgb"), Err("rgb"));
        assert_eq!(
            rejections(&with(|c| c.color = "puce".into())),
            [InvalidValue::of(&COLOR, "puce")]
        );

        assert_eq!(parse_core_leds_rows("0"), Err("0"), "0 rows is a typo");
        assert_eq!(parse_core_leds_rows("-2"), Err("-2"));
        assert_eq!(parse_core_leds_rows("many"), Err("many"));
        assert_eq!(
            rejections(&with(|c| c.rows = Rows::Count(-2))),
            [InvalidValue::written(&ROWS, "-2")],
            "a negative row count is rejected in the integer spelling too"
        );

        assert_eq!(parse_core_leds_fill("none"), Err("none"));
        assert_eq!(
            rejections(&with(|c| c.fill = "none".into())),
            [InvalidValue::of(&FILL, "none")]
        );
    }

    /// `rows` is **bounded**, and bounded in the single judge so both spellings
    /// inherit the same cap (#1040 F2).
    ///
    /// Unbounded, `rows = 10000000` asked the rasteriser for a 16 × 110 000 005
    /// px frame — a ~7 GB allocation, i.e. an abort, inside the `bind` apply
    /// closure on the GTK main thread — and `rows = 9223372036854775807`
    /// overflowed `LedMatrix::height()` (silently, in release, where
    /// `overflow-checks` is off). Since #869 that value arrives from a file
    /// edited live, where a stray digit is one keystroke.
    ///
    /// **Red if the cap is removed** from `parse_core_leds_rows`, on either
    /// path.
    #[test]
    fn a_row_count_is_capped_in_both_spellings() {
        let cap = i64::try_from(MAX_ROWS).expect("the cap fits an i64");

        // The boundary is accepted, in the variable's spelling and the file's.
        assert_eq!(
            parse_core_leds_rows(&MAX_ROWS.to_string()),
            Ok(Some(MAX_ROWS))
        );
        assert_eq!(
            applied(&with(|c| c.rows = Rows::Count(cap))).rows,
            Some(MAX_ROWS)
        );

        // The boundary + 1 is rejected, in both — with a *per-key* error that
        // names the key and the value, not a whole-file failure (#1040 F5).
        let over = MAX_ROWS + 1;
        assert_eq!(parse_core_leds_rows(&over.to_string()), Err("65"));
        assert_eq!(
            rejections(&with(|c| c.rows = Rows::Count(cap + 1))),
            [InvalidValue::written(&ROWS, "65")]
        );

        // …and so is every shape of absurd, including the two that used to
        // take the shell down.
        for absurd in ["10000000", "9223372036854775807", "18446744073709551616"] {
            assert_eq!(parse_core_leds_rows(absurd), Err(absurd), "{absurd} rows");
        }
        assert_eq!(
            rejections(&with(|c| c.rows = Rows::Count(i64::MAX))),
            [InvalidValue::written(&ROWS, "9223372036854775807")]
        );
    }

    /// The rejection line a bad file value produces, **as a literal**.
    ///
    /// Every other assertion about [`InvalidValue`] compares one
    /// `InvalidValue::of` against another, which is the same green-and-blind
    /// shape #1040 F4 caught in `deprecations()`: it cannot see the value being
    /// rendered as `style = plasma` (not TOML) instead of `style = "plasma"`
    /// (what is actually in the file), and it cannot see the vocabulary being
    /// reworded out from under the sentence it has to read inside.
    ///
    /// **Red if the value stops being quoted as the TOML it was written as**,
    /// if the integer arm starts quoting, or if the sentence is reworded.
    #[test]
    fn a_rejected_value_is_reported_as_the_toml_it_was_written_as() {
        assert_eq!(
            only_rejection(&with_style("plasma")).to_string(),
            "style = \"plasma\" is not valid; expected one of vfd/lcd/oled/crt",
            "a string key's value is quoted — `style = plasma` is not TOML at all"
        );
        assert_eq!(
            only_rejection(&with(|c| c.rows = Rows::Count(-2))).to_string(),
            "rows = -2 is not valid; expected 0 or \"rect\" for the \
             automatic rectangle, or a row count from 1 to 64",
            "…while an integer key's is bare"
        );
        assert_eq!(
            only_rejection(&with(|c| c.rows = Rows::Word("many".into()))).to_string(),
            "rows = \"many\" is not valid; expected 0 or \"rect\" for the \
             automatic rectangle, or a row count from 1 to 64",
            "…and `rows`' string arm quotes, because that is what the file says"
        );
    }

    /// The cap the diagnostic quotes is the cap the parser enforces.
    ///
    /// `ROWS`' two vocabularies spell the bound out for the reader ("a row
    /// count from 1 to 64") because that sentence is what a migrating user
    /// gets in the journal, and both are literals. **Red if [`MAX_ROWS`] moves
    /// without them.**
    #[test]
    fn the_diagnostic_quotes_the_cap_it_enforces() {
        for accepts in [ROWS.file_accepts, ROWS.env_accepts] {
            assert!(
                accepts.contains(&format!("1 to {MAX_ROWS}")),
                "the row cap {MAX_ROWS} is not the one {accepts:?} promises"
            );
        }
    }

    /// **The two vocabularies differ in exactly one spelling, and each line
    /// states its own** (#1040 V4).
    ///
    /// `rows` is the pilot's single translation point: TOML's `0` is the
    /// file's spelling of the word `rect`, and `TROLLSHELL_CORE_LEDS_ROWS=0`
    /// is rejected. One shared `expected` string therefore made the
    /// *variable*'s own rejection line contradict itself, verbatim:
    ///
    /// ```text
    /// TROLLSHELL_CORE_LEDS_ROWS is set to `0`, which is not valid;
    ///   expected "rect" (or 0) …
    /// ```
    ///
    /// **Red if the two are collapsed back into one string** — in either
    /// direction: sharing the file's wording puts `0` back in the variable's
    /// line, sharing the variable's takes the file's own spelling out of the
    /// deprecation line that is supposed to teach it.
    #[test]
    fn the_variable_line_does_not_offer_a_spelling_the_variable_rejects() {
        let (captured, _guard) = capture();
        let resolved = resolve(
            CoreLeds::default(),
            &env(&[("TROLLSHELL_CORE_LEDS_ROWS", "0")]),
            Deprecations::Announce,
        );

        assert_eq!(resolved.rows, None, "live control: `0` fell through");
        let line = warnings(&captured)
            .into_iter()
            .find(|w| w.starts_with("TROLLSHELL_CORE_LEDS_ROWS is set to `0`"))
            .expect("the unusable-variable line");
        assert!(
            !line.contains("or 0"),
            "the line that rejects `0` must not offer `0` back: {line}"
        );
        assert!(
            ROWS.file_accepts.contains("0 or \"rect\""),
            "…while the file, which does take it, says so: {:?}",
            ROWS.file_accepts
        );
    }

    /// `rows = 0` is the file's spelling of the variable's `rect`, and the
    /// translation is the *only* difference between the two vocabularies.
    ///
    /// Red if `rows_spelling` stops special-casing `0`: the row parser rejects
    /// `"0"`, so the built-in default would stop validating and
    /// `the_documented_default_is_the_built_in_one` would go red with it.
    #[test]
    fn zero_rows_is_the_files_spelling_of_rect() {
        assert_eq!(rows_spelling(&Rows::Count(0)), "rect");
        assert_eq!(rows_spelling(&Rows::Count(4)), "4");
        assert_eq!(rows_spelling(&Rows::Count(-2)), "-2");
        assert_eq!(rows_spelling(&Rows::Word("rect".into())), "rect");

        assert_eq!(
            applied(&with(|c| c.rows = Rows::Count(0))).rows,
            None,
            "0 is the automatic rectangle"
        );
        assert_eq!(applied(&with(|c| c.rows = Rows::Count(3))).rows, Some(3));
    }

    /// **`rows = "rect"` works in the file** — the word the deprecated variable
    /// took, and the word the deprecation line walks a migrating user toward
    /// (#1040 F5).
    ///
    /// Before the untagged [`Rows`], it was a `ConfigError::Schema`: a
    /// **whole-file** failure that discarded every other key in the file and
    /// dropped the panel to built-in defaults over the most likely migration
    /// typo there is. `rows = "4"` follows for free — the string arm is handed
    /// straight to the same single judge — and a word the judge does not know
    /// comes back as a named per-key error rather than a serde message.
    ///
    /// **Red if the `Word` arm is dropped**, or if `rows_spelling` stops
    /// handing a string through untouched.
    #[test]
    fn the_word_rect_is_a_file_spelling_too() {
        let rect = subsystem::assemble::<CoreLedsConfig>(&[(
            PathBuf::from("/o.toml"),
            "rows = \"rect\"\nstyle = \"crt\"\n".into(),
        )])
        .expect("a string `rows` assembles");
        let resolved = applied(&rect.config);
        assert_eq!(resolved.rows, None, "the word is the automatic rectangle");
        assert_eq!(
            resolved.style,
            DisplayStyle::Crt,
            "…and the keys beside it survived, which a whole-file error would not have"
        );

        assert_eq!(
            applied(&with(|c| c.rows = Rows::Word("4".into()))).rows,
            Some(4),
            "the string arm goes through the same single judge"
        );
        assert_eq!(
            rejections(&with(|c| c.rows = Rows::Word("many".into()))),
            [InvalidValue::written(&ROWS, "\"many\"")],
            "an unknown word is a named per-key error, not a serde type message"
        );
    }

    /// **A bad value costs its own key and nothing else** — the headline
    /// claim, asserted on the *effect* rather than on the message (#1040 V1).
    ///
    /// Until this round the claim was false. `parsed` `?`-ed on the first bad
    /// key, `validate` was that call, and `subsystem::assemble` turns a
    /// `validate` error into a whole-file `ConfigError::Invalid` — so this
    /// file resolved to the stock VFD/heat panel with `style = "crt"` silently
    /// gone, while the journal talked only about `rows`. Every assertion in
    /// the suite was about the message; nothing looked at what the panel got.
    ///
    /// **Red if the judgement moves back behind a `?`** — or if `validate`
    /// stops being `Infallible`, which is the same defect one level up.
    #[test]
    fn a_bad_value_leaves_every_other_key_applied() {
        let mut overlay = Overlay::new();
        overlay.write("style = \"crt\"\ncolor = \"rainbow\"\nrows = \"many\"\n");

        let (captured, _guard) = capture();
        let leds = initial_load(&overlay.layers());

        assert_eq!(leds.style, DisplayStyle::Crt, "the good keys apply");
        assert_eq!(leds.color, ColorMap::Rainbow, "…all of them");
        assert_eq!(
            leds.rows,
            CoreLeds::default().rows,
            "…and only the bad key falls back to the built-in default"
        );
        assert_eq!(
            warnings(&captured),
            [
                "rows = \"many\" is not valid; expected 0 or \"rect\" for the automatic \
              rectangle, or a row count from 1 to 64 — ignoring this key and using \
              the built-in default"
            ],
            "exactly one line, naming exactly the key that was dropped, and saying \
             what happened to it"
        );
        assert_eq!(
            errors(&captured),
            Vec::<String>::new(),
            "and no `config unusable` error: the file was usable"
        );
    }

    /// A `rows` of the wrong *type* is a per-key rejection too (#1040 V9).
    ///
    /// It used to be the one residual whole-file case: `serde` decided
    /// `rows = true` inside its own deserializer, before any subsystem code
    /// ran, so a `ConfigError::Schema` took the file with it. [`Rows::Other`]
    /// is the catch-all that gives the judgement back to
    /// [`CoreLedsConfig::parsed`], where it belongs — including `rows = 4.0`,
    /// the plausible float typo, which the old carve-out's doc did not even
    /// list.
    ///
    /// **Red if `Rows::Other` is dropped**: every case below goes back to
    /// being a `ConfigError::Schema` and `assemble` returns `Err`.
    #[test]
    fn a_rows_value_of_the_wrong_type_is_a_per_key_rejection() {
        for (body, written) in [
            ("rows = true\nstyle = \"crt\"\n", "true"),
            ("rows = 4.0\nstyle = \"crt\"\n", "4.0"),
            ("rows = [1]\nstyle = \"crt\"\n", "[1]"),
        ] {
            let loaded =
                subsystem::assemble::<CoreLedsConfig>(&[(PathBuf::from("/o.toml"), body.into())])
                    .unwrap_or_else(|e| panic!("{body:?} must still load: {e}"));

            assert_eq!(
                applied(&loaded.config).style,
                DisplayStyle::Crt,
                "{body:?}: the key beside it survived"
            );
            assert_eq!(
                only_rejection(&loaded.config),
                InvalidValue::written(&ROWS, written),
                "{body:?}: named, and quoted as the TOML it was written as"
            );
        }
    }

    /// The residual whole-file cases, **tested rather than pretended away**:
    /// a layer that is not TOML at all.
    ///
    /// These never reach `serde`, let alone this schema — the TOML parser
    /// rejects the bytes, and there is nothing per-key to say about a file
    /// whose keys cannot be told apart. Both are `ConfigError::Parse`, both
    /// keep the last good file on a reload, and the second one is the reason
    /// `rows = 9223372036854775808` behaves differently from
    /// `rows = 9223372036854775807` (which is a plain per-key rejection):
    /// TOML integers are `i64`, so the larger literal is a lexing failure.
    #[test]
    fn a_file_that_is_not_toml_is_still_a_whole_file_error() {
        for body in [
            "style = \"crt\n",
            "rows = 9223372036854775808\nstyle = \"crt\"\n",
        ] {
            let err =
                subsystem::assemble::<CoreLedsConfig>(&[(PathBuf::from("/o.toml"), body.into())])
                    .expect_err("not TOML");
            assert!(
                matches!(err, subsystem::ConfigError::Parse { .. }),
                "{body:?} must fail as a parse error, got {err}"
            );
        }
    }

    /// The four keys, each set to a non-default value, land where they should.
    /// A full-house case so a field wired to the wrong parser cannot hide
    /// behind a coincidental default.
    #[test]
    fn every_key_round_trips_from_the_file() {
        let body = "style = \"crt\"\ncolor = \"#9b59b6\"\nrows = 3\nfill = \"blank\"\n";
        let loaded =
            subsystem::assemble::<CoreLedsConfig>(&[(PathBuf::from("/o.toml"), body.into())])
                .expect("assembles");

        assert_eq!(
            applied(&loaded.config),
            CoreLeds {
                style: DisplayStyle::Crt,
                color: ColorMap::Rgb(0x9b, 0x59, 0xb6),
                rows: Some(3),
                fill: Fill::Blank,
            }
        );
    }

    /// The overlay beats the base, a key only the base states falls through,
    /// and a key neither states falls through to `DEFAULT_TOML` — #868's
    /// scalar rule, exercised on this schema rather than on the generic one.
    #[test]
    fn the_overlay_beats_the_base_which_beats_the_documented_default() {
        let layers = vec![
            (
                PathBuf::from("/base.toml"),
                "style = \"lcd\"\nrows = 8\n".into(),
            ),
            (PathBuf::from("/overlay.toml"), "rows = 3\n".into()),
        ];
        let loaded = subsystem::assemble::<CoreLedsConfig>(&layers).expect("assembles");
        let resolved = applied(&loaded.config);

        assert_eq!(resolved.rows, Some(3), "the overlay wins");
        assert_eq!(
            resolved.style,
            DisplayStyle::Lcd,
            "a key only the base states falls through the overlay"
        );
        assert_eq!(
            resolved.color,
            ColorMap::Heat,
            "and a key no file states comes from DEFAULT_TOML"
        );
    }

    /// `#rrggbb`: with or without the hash, either case, and nothing else — a
    /// short, long or non-hex string is rejected rather than silently
    /// truncated.
    #[test]
    fn hex_colours_parse_both_ways() {
        assert_eq!(
            parse_hex_rgb("#ff8800"),
            Some(ColorMap::Rgb(0xff, 0x88, 0x00))
        );
        assert_eq!(
            parse_hex_rgb("ff8800"),
            Some(ColorMap::Rgb(0xff, 0x88, 0x00))
        );
        assert_eq!(
            parse_hex_rgb("FF8800"),
            Some(ColorMap::Rgb(0xff, 0x88, 0x00))
        );
        assert_eq!(parse_hex_rgb("#fff"), None);
        assert_eq!(parse_hex_rgb("#ff88000"), None);
        assert_eq!(parse_hex_rgb("#gg8800"), None);
        assert_eq!(parse_hex_rgb(""), None);
    }

    // ── Resolution: the environment wins, per key ───────────────────────────

    /// A set variable beats the file, and only for its own key.
    ///
    /// **Red if the env-wins branch is deleted** from `env_key` (the file's
    /// `lcd` would come back instead of the variable's `crt`).
    #[test]
    fn a_set_variable_beats_the_file() {
        let file = CoreLeds {
            style: DisplayStyle::Lcd,
            color: ColorMap::Rainbow,
            rows: Some(8),
            fill: Fill::Blank,
        };

        let resolved = resolve(
            file,
            &env(&[("TROLLSHELL_CORE_LEDS_STYLE", "crt")]),
            Deprecations::Silent,
        );

        assert_eq!(resolved.style, DisplayStyle::Crt, "the variable wins");
        assert_eq!(
            (resolved.color, resolved.rows, resolved.fill),
            (file.color, file.rows, file.fill),
            "…and only for its own key: the other three stay the file's"
        );
    }

    /// An unset variable resolves to the file's value, not to the built-in
    /// default — the regression that would make the whole file inert.
    #[test]
    fn an_unset_variable_leaves_the_file_alone() {
        let file = CoreLeds {
            style: DisplayStyle::Oled,
            color: ColorMap::TransPride,
            rows: Some(2),
            fill: Fill::Blank,
        };

        assert_eq!(resolve(file, &no_env(), Deprecations::Silent), file);
    }

    /// A set-but-unparseable variable does **not** win: it warns and falls
    /// through to the file, which is the pre-#869 behaviour with the config
    /// file in the place the hard-coded default used to occupy.
    #[test]
    fn an_unparseable_variable_falls_through_to_the_file() {
        let file = CoreLeds {
            style: DisplayStyle::Oled,
            ..CoreLeds::default()
        };

        let resolved = resolve(
            file,
            &env(&[("TROLLSHELL_CORE_LEDS_STYLE", "plasma")]),
            Deprecations::Silent,
        );

        assert_eq!(resolved.style, DisplayStyle::Oled);
    }

    // ── The deprecation warning ─────────────────────────────────────────────

    /// Captured `tracing` events, in the shape `hytte_config::subsystem`'s own
    /// tests use: a shared buffer, a field visitor, and a thread-local
    /// `set_default` guard so one test's subscriber cannot leak into another's.
    #[derive(Clone, Default)]
    struct Captured {
        events: std::sync::Arc<std::sync::Mutex<Vec<CapturedEvent>>>,
    }

    #[derive(Clone, Debug)]
    struct CapturedEvent {
        level: tracing::Level,
        message: String,
        /// Everything but the message, rendered.
        ///
        /// Kept rather than dropped since #1040 V10: the PR advertises that
        /// these lines carry `subsystem`/`var`/`key`/`accepts`/`file` "for
        /// anything that wants to filter on them", and this file's copy of the
        /// harness used to throw them away — so nothing asserted a single one
        /// and the claim was unfalsifiable. `hytte-config`'s copy always kept
        /// them; the divergence is what #1044's hoist has to reconcile.
        fields: HashMap<String, String>,
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
        fields: HashMap<String, String>,
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
            // The message itself arrives here: `tracing` renders format args
            // through `Debug`, and `Arguments`' `Debug` is its `Display`. So
            // do `%` sigils, whose wrapper renders `Display` through `Debug`.
            let rendered = format!("{value:?}");
            if field.name() == "message" {
                self.message = rendered;
            } else {
                self.fields.insert(field.name().to_string(), rendered);
            }
        }
    }

    /// Install `Captured` as this thread's subscriber, and rebuild the
    /// callsite interest cache.
    ///
    /// The rebuild is the repo's known `tracing` trap, not decoration: with
    /// zero live dispatchers `tracing-core` caches a callsite's interest as
    /// `never` for the whole binary, so a `warn!` first reached by some other
    /// test would be invisible here forever. `Dispatch::new` already rebuilds
    /// over the registered list; this covers the single-live-dispatcher
    /// `JustOne` path, which is thread-sensitive. It can only *widen* interest
    /// (`enabled` is unconditionally true), so it cannot poison a sibling.
    fn capture() -> (Captured, tracing::dispatcher::DefaultGuard) {
        keep_interest_alive();
        let captured = Captured::default();
        let guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(captured.clone()));
        tracing::callsite::rebuild_interest_cache();
        (captured, guard)
    }

    /// A `Dispatch` that stays registered for the life of the test binary and
    /// is interested in everything — the thing that makes every capture in
    /// this file deterministic.
    ///
    /// # The flake, and why `rebuild_interest_cache` alone does not close it
    ///
    /// `tracing-core` caches each callsite's `Interest` process-wide and
    /// rebuilds it through `callsite::Rebuilder`. That rebuilder has a fast
    /// path (`tracing-core-0.1.36/src/callsite.rs:544-573`): while
    /// `has_just_one` is set — one live registered `Dispatch`, which is the
    /// steady state of a test binary where captures come and go — it does
    /// **not** iterate the registered dispatchers at all. It calls
    /// `dispatcher::get_default`, i.e. *the rebuilding thread's own default*.
    /// A sibling test thread that has no default and first touches one of
    /// these callsites therefore registers it against `NoSubscriber`, whose
    /// `register_callsite` is `Interest::never()` — cached globally, so the
    /// `warn!` short-circuits and a capture on another thread observes
    /// nothing.
    ///
    /// Measured on this branch: **14 / 30** full `--features system-tests`
    /// runs failed `a_bad_value_leaves_every_other_key_applied` with
    /// `left: []`, with `rebuild_interest_cache()` in place and with the
    /// #1020 warm-up (touch the callsites before installing the subscriber)
    /// in place. Both are insurance against a race they cannot win: the
    /// poisoning thread is not this one, and it acts after the rebuild.
    ///
    /// # Why a keepalive does close it
    ///
    /// `has_just_one` is recomputed only inside `register_dispatch`, as
    /// `dispatchers.len() <= 1` after pruning dead ones (`:551-558`). A
    /// dispatch that never dies means the next registration always counts
    /// **two**, so the fast path is switched off for the rest of the process
    /// and every later rebuild — from any thread, with or without a default —
    /// iterates the live dispatchers, which always include this one. Its
    /// `register_callsite` is `Interest::always()`, and `Interest::and`
    /// (`subscriber.rs:658-664`) degrades a disagreement to `sometimes`, never
    /// to `never` — so a callsite can only ever end up `always` or
    /// `sometimes`, and `sometimes` consults the *emitting* thread's
    /// subscriber, which is the capture.
    ///
    /// It can only widen interest, exactly like the `rebuild_interest_cache()`
    /// above it: it enables nothing, records nothing, and is never any
    /// thread's default. The cost is that `warn!` macros build their event on
    /// threads with no subscriber and hand it to `NoSubscriber` — invisible,
    /// and confined to this test binary.
    ///
    /// No lock, and no discipline required from any other test — which is what
    /// #1020 asked for and what a shared mutex over every capture test would
    /// not give, since the poisoning thread need not be running a capture test
    /// at all.
    fn keep_interest_alive() {
        static KEEPALIVE: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();

        struct AlwaysInterested;
        impl tracing::Subscriber for AlwaysInterested {
            fn register_callsite(
                &self,
                _: &'static tracing::Metadata<'static>,
            ) -> tracing::subscriber::Interest {
                tracing::subscriber::Interest::always()
            }
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, _: &tracing::Event<'_>) {}
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }

        // `Dispatch::new` is what registers it; the `OnceLock` holds the only
        // strong reference, so the registrar's `Weak` never dies.
        KEEPALIVE.get_or_init(|| tracing::Dispatch::new(AlwaysInterested));
    }

    /// The deprecation lines, selected on the **exact** rendered message. A
    /// `contains("deprecated")` filter would turn green-and-blind the day the
    /// wording changes — including the negative test below, whose whole job is
    /// to observe an absence.
    fn deprecations(captured: &Captured) -> Vec<String> {
        let expected: Vec<String> = [&STYLE, &COLOR, &ROWS, &FILL]
            .into_iter()
            .map(announced)
            .collect();
        captured
            .events()
            .into_iter()
            .filter(|e| e.level == tracing::Level::WARN && expected.contains(&e.message))
            .map(|e| e.message)
            .collect()
    }

    /// The exact deprecation line one knob produces, against the process's own
    /// resolved overlay path.
    ///
    /// The *sentence* is pinned against a literal in `config::tests`; here the
    /// job is only to select the line out of a capture, which needs whatever
    /// path this machine resolves.
    fn announced(knob: &super::Knob) -> String {
        crate::config::deprecation_message(
            knob.var,
            knob.key,
            &crate::config::overlay_display(CoreLedsConfig::NAME),
            knob.file_accepts,
        )
    }

    /// Every warning the capture saw, whatever its shape — the denominator for
    /// "exactly one line".
    fn warnings(captured: &Captured) -> Vec<String> {
        captured
            .events()
            .into_iter()
            .filter(|e| e.level == tracing::Level::WARN)
            .map(|e| e.message)
            .collect()
    }

    /// Errors, likewise.
    fn errors(captured: &Captured) -> Vec<String> {
        captured
            .events()
            .into_iter()
            .filter(|e| e.level == tracing::Level::ERROR)
            .map(|e| e.message)
            .collect()
    }

    /// A set variable still wins, and says so **once**, naming the real
    /// overlay path and the key it moves to.
    ///
    /// Red if `warn_deprecated_env` stops being called, if the message is
    /// reworded in one place and not the other, or if the wrong key is named.
    #[test]
    fn a_set_variable_announces_its_deprecation_once() {
        let (captured, _guard) = capture();

        let resolved = resolve(
            CoreLeds::default(),
            &env(&[
                ("TROLLSHELL_CORE_LEDS_STYLE", "crt"),
                ("TROLLSHELL_CORE_LEDS_FILL", "blank"),
            ]),
            Deprecations::Announce,
        );

        assert_eq!(resolved.style, DisplayStyle::Crt, "the variable still wins");
        assert_eq!(
            deprecations(&captured),
            vec![announced(&STYLE), announced(&FILL)],
            "one line per *set* variable, in knob order, and no line for the two unset ones"
        );
    }

    /// The other half, and the one that needs a **live control**: asserting an
    /// absence against a capture that observed nothing at all is not an
    /// assertion. The unusable-value warning proves the capture was wired at
    /// the moment the absence was observed.
    ///
    /// It also pins **F9**: a set-but-unusable variable costs *one* line, not a
    /// deprecation line plus an unusable-value line. Red if `env_key` goes back
    /// to announcing before it parses.
    #[test]
    fn an_unset_variable_announces_nothing() {
        let (captured, _guard) = capture();

        resolve(
            CoreLeds::default(),
            &env(&[("TROLLSHELL_CORE_LEDS_COLOR", "puce")]),
            Deprecations::Announce,
        );

        let warned = warnings(&captured);
        assert_eq!(
            warned.len(),
            1,
            "one set-but-unusable variable is exactly one line: {warned:?}"
        );
        assert!(
            warned[0].starts_with("TROLLSHELL_CORE_LEDS_COLOR is set to `puce`"),
            "live control: the capture must be observing this thread, got {warned:?}"
        );
        assert!(
            warned[0].contains("`color`") && warned[0].contains("core-leds.toml"),
            "…and the one line has to carry the instruction the deprecation line would have: \
             {warned:?}"
        );
        assert!(
            deprecations(&captured).is_empty(),
            "a value nothing accepts is not also announced as a migration: {warned:?}"
        );
    }

    /// `Deprecations::Silent` — what every reload passes — says nothing at
    /// all, for **either** line, so a live shell does not repeat itself every
    /// few seconds. Both a variable that parsed and one that did not are set
    /// here, since since #1040 V7 the two are gated together.
    ///
    /// The live control is the resolution itself rather than a line that was
    /// observed: an absence-of-output test cannot use its own subject as
    /// evidence that the subject ran. `crt` reaching the result proves both
    /// variables were read on this thread.
    ///
    /// **Red if either warning stops being gated on `Deprecations`.**
    #[test]
    fn a_silent_resolution_announces_nothing() {
        let (captured, _guard) = capture();

        let resolved = resolve(
            CoreLeds::default(),
            &env(&[
                ("TROLLSHELL_CORE_LEDS_STYLE", "crt"),
                ("TROLLSHELL_CORE_LEDS_COLOR", "puce"),
            ]),
            Deprecations::Silent,
        );

        assert_eq!(
            resolved.style,
            DisplayStyle::Crt,
            "live control: the resolution under observation actually ran"
        );
        assert_eq!(
            resolved.color,
            CoreLeds::default().color,
            "…including the unusable half, which fell through"
        );
        assert_eq!(
            warnings(&captured),
            Vec::<String>::new(),
            "a reload says nothing: {:?}",
            captured.events()
        );
    }

    /// **The startup call site announces** — the one place in production that
    /// passes [`Deprecations::Announce`], driven through the function
    /// [`CoreLedsService::start`] delegates to (#1040 F3).
    ///
    /// Before this, flipping that `Announce` to `Silent` left the whole suite
    /// green (mutation X1): the `resolve`-level tests pass their own
    /// `Deprecations`, so none of them could see which one production chose.
    /// The reload side already had this cover
    /// (`a_reload_does_not_re_announce_a_pinned_variable`); this is the
    /// symmetric half, and "the once-ness is a property of the two call sites"
    /// only closes with both.
    ///
    /// **Red if `boot` stops announcing.**
    #[test]
    fn the_startup_resolution_announces() {
        let (captured, _guard) = capture();

        let (resolved, watcher) = boot(&[], &env(&[("TROLLSHELL_CORE_LEDS_STYLE", "crt")]));

        assert_eq!(
            watcher.last_good,
            CoreLeds::default(),
            "live control: with no layers the file half is the documented default"
        );
        assert_eq!(resolved.style, DisplayStyle::Crt, "…and the variable wins");
        assert_eq!(
            deprecations(&captured),
            vec![announced(&STYLE)],
            "the startup resolution announces exactly the set variable"
        );
    }

    // ── Live reload ─────────────────────────────────────────────────────────

    /// A scratch overlay whose mtime the test controls.
    struct Overlay {
        _dir: tempfile::TempDir,
        path: PathBuf,
        stamp: u64,
    }

    impl Overlay {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("core-leds.toml");
            Self {
                _dir: dir,
                path,
                stamp: 0,
            }
        }

        /// Write `body` and move the mtime forward by a whole second.
        ///
        /// Set explicitly rather than trusted to the filesystem: a
        /// sub-millisecond test would otherwise rewrite a file inside one
        /// mtime granule and the watcher would correctly see no change.
        fn write(&mut self, body: &str) {
            self.put(body);
            self.stamp += 1;
            self.touch();
        }

        /// Write `body` **without** moving the mtime — the same-granule save
        /// an mtime-only watcher misses forever (#1040 F8).
        fn write_in_the_same_granule(&self, body: &str) {
            self.put(body);
            self.touch();
        }

        fn put(&self, body: &str) {
            std::fs::write(&self.path, body).expect("write");
        }

        fn touch(&self) {
            let file = std::fs::File::options()
                .write(true)
                .open(&self.path)
                .expect("open");
            file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(self.stamp))
                .expect("set mtime");
        }

        fn delete(&self) {
            std::fs::remove_file(&self.path).expect("delete");
        }

        fn layers(&self) -> Vec<PathBuf> {
            vec![self.path.clone()]
        }
    }

    /// The watcher [`boot`] hands back for `paths`, with no environment.
    ///
    /// The production function itself since #1040 V3 — it used to be a
    /// hand-rolled replica of it, which is exactly why mutations to the real
    /// one survived.
    fn watching(paths: &[PathBuf]) -> Watcher {
        boot(paths, &no_env()).1
    }

    /// A [`CoreLedsService`] over a scratch overlay and a fake environment,
    /// polling at `interval` — so [`Service::start`], the function the process
    /// actually runs, is the thing under test rather than a copy of it
    /// (#1040 V3).
    fn service_over(
        paths: Vec<PathBuf>,
        lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
        interval: Duration,
    ) -> CoreLedsService {
        CoreLedsService {
            paths,
            lookup: std::sync::Arc::new(lookup),
            interval,
        }
    }

    /// An interval no test waits for: for the `start`-level tests, whose
    /// subject is the synchronous half and whose spawned poller must stay out
    /// of the way.
    const NEVER: Duration = Duration::from_hours(1);

    /// The payoff: an edit while the shell runs re-resolves without a restart.
    ///
    /// **Red if `Watcher::poll` stops re-reading** (return `None` before the
    /// load, or drop the `self.last_good = config` assignment).
    #[test]
    fn a_changed_file_is_picked_up() {
        let mut overlay = Overlay::new();
        overlay.write("style = \"lcd\"\n");
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&no_env(), Deprecations::Silent);
        assert_eq!(current.style, DisplayStyle::Lcd);

        // Unmoved mtime → nothing to do.
        assert_eq!(watcher.poll(current, &no_env()), None);

        overlay.write("style = \"crt\"\ncolor = \"rainbow\"\n");
        let next = watcher.poll(current, &no_env()).expect("changed → reload");

        assert_eq!(next.style, DisplayStyle::Crt);
        assert_eq!(next.color, ColorMap::Rainbow);
        // A touch that changes nothing must not churn the signal.
        overlay.write("style = \"crt\"\ncolor = \"rainbow\"\n");
        assert_eq!(watcher.poll(next, &no_env()), None);
    }

    /// A file that appears *after* startup is a change too — the overlay is
    /// absent on a fresh install, so `None → Some(mtime)` has to count.
    #[test]
    fn a_newly_created_file_is_picked_up() {
        let mut overlay = Overlay::new();
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&no_env(), Deprecations::Silent);
        assert_eq!(
            current,
            CoreLeds::default(),
            "a missing file is the default"
        );

        overlay.write("fill = \"blank\"\n");
        let next = watcher.poll(current, &no_env()).expect("created → reload");

        assert_eq!(next.fill, Fill::Blank);
    }

    /// A save caught **mid-edit** — bytes that are not TOML — keeps the last
    /// good config rather than snapping the panel back to the built-in default
    /// while a string is half typed.
    ///
    /// The sibling case, a file that *is* TOML with one unusable value, is
    /// deliberately **not** this: it is a finished file with one mistake in
    /// it, so the good keys apply and the bad key takes the built-in default
    /// (`a_bad_value_reloads_the_keys_beside_it` below, #1040 V1). The
    /// asymmetry is the whole judgement, so both halves are pinned.
    ///
    /// **Red if the `Err` arm of `Watcher::poll` overwrites `last_good`.**
    #[test]
    fn a_file_caught_mid_edit_keeps_the_last_good_config() {
        let mut overlay = Overlay::new();
        overlay.write("style = \"crt\"\n");
        let mut watcher = watching(&overlay.layers());
        let good = watcher.resolved(&no_env(), Deprecations::Silent);
        assert_eq!(good.style, DisplayStyle::Crt);

        // Not TOML at all.
        overlay.write("style = \"crt\n");
        assert_eq!(
            watcher.poll(good, &no_env()),
            None,
            "a parse error must publish nothing"
        );
        assert_eq!(watcher.resolved(&no_env(), Deprecations::Silent), good);

        // …and a repaired file is picked up again.
        overlay.write("style = \"oled\"\n");
        let next = watcher.poll(good, &no_env()).expect("repaired → reload");
        assert_eq!(next.style, DisplayStyle::Oled);
    }

    /// A **live save** with one bad value republishes the keys beside it
    /// (#1040 V1), rather than keeping the whole last-good skin.
    ///
    /// This is the reload half of `a_bad_value_leaves_every_other_key_applied`
    /// and the one a human actually meets: they add `color = "rainbow"` to a
    /// working file, fat-finger `rows` in the same save, and — before this
    /// round — got neither, with a journal line about `rows` only.
    ///
    /// **Red if a rejected value goes back to being a whole-file failure**:
    /// `poll` then returns `None` and the panel keeps the old colour.
    #[test]
    fn a_bad_value_reloads_the_keys_beside_it() {
        let mut overlay = Overlay::new();
        overlay.write("style = \"crt\"\n");
        let mut watcher = watching(&overlay.layers());
        let good = watcher.resolved(&no_env(), Deprecations::Silent);

        overlay.write("style = \"crt\"\ncolor = \"rainbow\"\nrows = \"many\"\n");
        let next = watcher
            .poll(good, &no_env())
            .expect("the good keys in the save must reach the panel");

        assert_eq!(next.color, ColorMap::Rainbow, "the key beside the typo");
        assert_eq!(next.style, DisplayStyle::Crt, "…and the one that was fine");
        assert_eq!(
            next.rows,
            CoreLeds::default().rows,
            "…while the typo alone falls back to the built-in default"
        );
    }

    /// An environment-pinned key stays pinned across a reload: the file moves,
    /// the variable still wins for its own key, and the *other* keys still
    /// track the file.
    ///
    /// **Red if the reload stops layering the environment** (it would publish
    /// the file's `lcd`).
    #[test]
    fn an_env_pinned_key_survives_a_reload() {
        let pinned = env(&[("TROLLSHELL_CORE_LEDS_STYLE", "crt")]);
        let mut overlay = Overlay::new();
        overlay.write("style = \"vfd\"\ncolor = \"heat\"\n");
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&pinned, Deprecations::Silent);
        assert_eq!(current.style, DisplayStyle::Crt);

        overlay.write("style = \"lcd\"\ncolor = \"rainbow\"\n");
        let next = watcher.poll(current, &pinned).expect("changed → reload");

        assert_eq!(
            next.style,
            DisplayStyle::Crt,
            "the environment still wins after a reload"
        );
        assert_eq!(
            next.color,
            ColorMap::Rainbow,
            "…while the keys it does not pin follow the file"
        );
    }

    /// A **reload** must not re-announce. The deprecation line is a startup
    /// event; the poll runs every few seconds for the life of the shell, so an
    /// announcing reload would fill the journal with the same line forever.
    ///
    /// The capture is installed *after* the startup resolution, so it observes
    /// the poll and nothing else. **Red if `Watcher::poll` passes
    /// `Deprecations::Announce`** — the call-site half of the latch, which the
    /// `resolve`-level test above cannot see.
    #[test]
    fn a_reload_does_not_re_announce_a_pinned_variable() {
        let pinned = env(&[("TROLLSHELL_CORE_LEDS_STYLE", "crt")]);
        let mut overlay = Overlay::new();
        overlay.write("color = \"heat\"\n");
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&pinned, Deprecations::Announce);

        let (captured, _guard) = capture();
        overlay.write("color = \"rainbow\"\n");
        let next = watcher.poll(current, &pinned).expect("changed → reload");

        assert_eq!(
            next.color,
            ColorMap::Rainbow,
            "live control: the reload under observation must actually have happened"
        );
        assert_eq!(
            next.style,
            DisplayStyle::Crt,
            "…with the variable still won"
        );
        assert!(
            deprecations(&captured).is_empty(),
            "a reload must not repeat the startup line: {:?}",
            captured.events()
        );
    }

    /// **A bad variable costs one line for the life of the shell**, not one
    /// per reload (#1040 V7).
    ///
    /// The unusable-value line used to be ungated, on the argument that "an
    /// unusable variable is still unusable after an edit". True, and still one
    /// extra journal line every three seconds forever — against a
    /// `live-verify.md` bullet that promises **exactly one** two lines after
    /// telling the reader that a repeating line means a bug. A process's
    /// environment cannot change under it, so the repeat could never carry
    /// news.
    ///
    /// **Red if the unusable-value warning stops being gated on
    /// `Deprecations`.**
    #[test]
    fn a_reload_does_not_repeat_the_unusable_variable_line() {
        let broken = env(&[("TROLLSHELL_CORE_LEDS_STYLE", "plasma")]);
        let mut overlay = Overlay::new();
        overlay.write("color = \"heat\"\n");
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&broken, Deprecations::Announce);

        let (captured, _guard) = capture();
        overlay.write("color = \"rainbow\"\n");
        let next = watcher.poll(current, &broken).expect("changed → reload");

        assert_eq!(
            next.color,
            ColorMap::Rainbow,
            "live control: the reload under observation must actually have happened"
        );
        assert_eq!(
            warnings(&captured),
            Vec::<String>::new(),
            "a reload says nothing about a variable that was already reported"
        );
    }

    /// An unknown key is loud and harmless (merge rule 4): the file still
    /// loads, and the keys around the typo still apply.
    #[test]
    fn an_unknown_key_does_not_break_the_load() {
        let body = "colour = \"rainbow\"\nstyle = \"crt\"\n";
        let loaded =
            subsystem::assemble::<CoreLedsConfig>(&[(PathBuf::from("/o.toml"), body.into())])
                .expect("assembles");

        assert_eq!(loaded.unknown_keys, ["colour"]);
        let resolved = applied(&loaded.config);
        assert_eq!(resolved.style, DisplayStyle::Crt);
        assert_eq!(resolved.color, ColorMap::Heat, "the typo did nothing");
    }

    /// **The whole startup sequence loads the config exactly once** — so one
    /// typo produces one diagnostic (#1040 F1).
    ///
    /// `start` used to load (through `Watcher::observe`) and then hand `watch`
    /// a constructor that loaded *again*: two `unknown key in config` warnings
    /// for one misspelt key, two `config unusable` errors for one broken file.
    /// That is not cosmetic — `docs/live-verify.md` promises a human **one**
    /// line for exactly this case, and the second load re-baselined the
    /// poller's stamps against a file that might have changed between the two,
    /// leaving the published value one save stale.
    ///
    /// **Red if anything on the startup path loads twice** — in particular if
    /// `Watcher::stamping_before` goes back to loading twice.
    #[test]
    fn a_startup_loads_the_config_exactly_once() {
        let mut overlay = Overlay::new();
        overlay.write("colour = \"rainbow\"\nstyle = \"crt\"\n");

        let (captured, _guard) = capture();
        let (_resolved, watcher) = boot(&overlay.layers(), &no_env());

        assert_eq!(
            warnings(&captured),
            ["unknown key in config; ignoring it"],
            "one unknown key, one warning — live-verify.md promises exactly this"
        );
        assert_eq!(
            watcher.last_good.style,
            DisplayStyle::Crt,
            "live control: the load under observation actually happened"
        );
    }

    /// The same, for the error half: a file nothing can parse says so **once**.
    #[test]
    fn a_startup_reports_an_unusable_file_exactly_once() {
        let mut overlay = Overlay::new();
        overlay.write("style = \"crt\n");

        let (captured, _guard) = capture();
        let (_resolved, watcher) = boot(&overlay.layers(), &no_env());

        assert_eq!(
            errors(&captured),
            ["config unusable; falling back to the built-in default"],
            "one broken file, one error"
        );
        assert_eq!(
            watcher.last_good,
            CoreLeds::default(),
            "…and it degrades to the built-in default rather than taking the shell down"
        );
    }

    // ── The service: what the process actually runs (#1040 V3) ──────────────

    /// **`Service::start` loads once and announces once** — the two properties
    /// F1 and F3 established, asserted at the call site that has them rather
    /// than at a replica of it.
    ///
    /// This is #1040 V3's whole point. Until it existed, `start` had *zero*
    /// coverage: making it call the startup sequence twice (mutation Z2), or
    /// skip it entirely and resolve `Silent` inline (Z3, F3's original defect
    /// one level up), left the suite at 562 passed. Nobody would ever have
    /// heard a deprecation line again and CI would not have noticed.
    ///
    /// **Red if `start` loads twice, announces twice, or stops announcing.**
    #[test]
    fn the_service_start_loads_once_and_announces_once() {
        let mut overlay = Overlay::new();
        overlay.write("colour = \"rainbow\"\nstyle = \"crt\"\n");
        let service = service_over(
            overlay.layers(),
            env(&[("TROLLSHELL_CORE_LEDS_FILL", "blank")]),
            NEVER,
        );

        let (captured, _guard) = capture();
        let handles = service.start(hytte::reactive::runtime::handle());

        assert_eq!(
            warnings(&captured),
            [
                "unknown key in config; ignoring it".to_string(),
                announced(&FILL),
            ],
            "one load and one announcing resolution, in that order"
        );
        assert_eq!(
            handles.leds.get(),
            CoreLeds {
                style: DisplayStyle::Crt,
                fill: Fill::Blank,
                ..CoreLeds::default()
            },
            "live control: the file's key and the variable's key both reached the handle"
        );
    }

    /// **Deleting the file means "give me the defaults back"** — the honest
    /// reading of a delete, and the only way to get the stock look without
    /// hand-restoring every key (#1040 F7).
    ///
    /// Deliberately *not* the malformed-file behaviour: a broken file is an
    /// edit in progress and keeps the last good skin, while a deleted one is an
    /// intent. Documented on [`Watcher::poll`] and in `live-verify.md`.
    #[test]
    fn a_deleted_file_falls_back_to_the_defaults() {
        let mut overlay = Overlay::new();
        overlay.write("style = \"crt\"\ncolor = \"rainbow\"\n");
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&no_env(), Deprecations::Silent);
        assert_eq!(current.style, DisplayStyle::Crt);

        overlay.delete();
        let next = watcher.poll(current, &no_env()).expect("deleted → reload");

        assert_eq!(
            next,
            CoreLeds::default(),
            "a delete is an intent, not a mistake: back to the documented defaults"
        );
    }

    /// **The watcher starts from *now*.** Its baseline is the layers' current
    /// stamps, so a file that has not moved since construction republishes
    /// nothing — even when the caller's `current` disagrees with it.
    ///
    /// The disagreement is what gives this test teeth: with every stamp
    /// baselined to `None` (mutation X5 — a watcher that thinks every layer
    /// just appeared) the first poll reloads and returns the file's value,
    /// which is not `current`. Asserted against a `current` that matches the
    /// file, this stayed green, which is why X5 survived the first round.
    #[test]
    fn a_watcher_starts_from_now() {
        let mut overlay = Overlay::new();
        overlay.write("style = \"lcd\"\n");
        let mut watcher = watching(&overlay.layers());

        assert_eq!(
            watcher.poll(CoreLeds::default(), &no_env()),
            None,
            "nothing has moved since construction, so there is nothing to republish"
        );
        assert_eq!(
            watcher.resolved(&no_env(), Deprecations::Silent).style,
            DisplayStyle::Lcd,
            "live control: the watcher really is watching a file that says lcd"
        );
    }

    /// An edit saved **inside the same mtime granule** as the last stat is
    /// still seen, because the stamp carries the file's length too
    /// (#1040 F8).
    ///
    /// An mtime-only stamp misses this one *permanently*, not merely late: the
    /// stamp is updated unconditionally, so the movement never shows up again.
    ///
    /// **Red if [`stamp`](super::stamp) drops the length.**
    #[test]
    fn an_edit_inside_one_mtime_granule_is_still_seen() {
        let mut overlay = Overlay::new();
        overlay.write("style = \"lcd\"\n");
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&no_env(), Deprecations::Silent);
        assert_eq!(current.style, DisplayStyle::Lcd);

        // Same mtime to the nanosecond, different bytes — and a different
        // byte count, which is the half the length in the stamp can see.
        overlay.write_in_the_same_granule("style = \"crt\"\ncolor = \"rainbow\"\n");
        let next = watcher
            .poll(current, &no_env())
            .expect("a same-granule save must not be invisible");

        assert_eq!(next.style, DisplayStyle::Crt);
        assert_eq!(next.color, ColorMap::Rainbow);
    }

    /// **An edit made after the load is not lost** (#1040 V2).
    ///
    /// The window is real and was measured on the previous head: the shell
    /// loaded and published at T1, the spawned poller stamped at T2 when a
    /// runtime worker first polled the task, and a save landing in between was
    /// baselined into the poller's stamps while the published value predated
    /// it. Because [`Watcher::poll`] updates its stamps unconditionally, that
    /// save was then missed **forever** — file said `crt`, panel said `lcd`,
    /// three polls, nothing.
    ///
    /// Deterministic rather than raced, and driven through the **production**
    /// constructor rather than a copy of it: the save is the last thing the
    /// injected `load` does, so it lands strictly after the read. With the
    /// stamp taken first (as [`Watcher::stamping_before`] does) the baseline
    /// predates the save, the next poll sees it and republishes. With the
    /// stamp taken after the load — the two lines swapped, which is all the
    /// bug ever was — the baseline already includes it, `poll` returns `None`,
    /// and the panel keeps `lcd` for the life of the shell.
    ///
    /// An earlier version of this test inlined those two lines itself and so
    /// asserted nothing about the code; mutation V2a was green against it.
    #[test]
    fn an_edit_landing_after_the_load_is_not_lost() {
        let overlay = std::cell::RefCell::new(Overlay::new());
        overlay.borrow_mut().write("style = \"lcd\"\n");
        let paths = overlay.borrow().layers();

        let mut watcher = Watcher::stamping_before(paths, |paths| {
            let leds = initial_load(paths);
            // Strictly after the read, strictly before the caller sees the
            // watcher: the window V2 is about.
            overlay.borrow_mut().write("style = \"crt\"\n");
            leds
        });

        let published = watcher.resolved(&no_env(), Deprecations::Silent);
        assert_eq!(
            published.style,
            DisplayStyle::Lcd,
            "live control: the load really did happen before the save"
        );
        assert_eq!(
            watcher.poll(published, &no_env()).map(|leds| leds.style),
            Some(DisplayStyle::Crt),
            "the save must reach the panel on the next poll, not never"
        );
    }

    /// **The watcher [`boot`] built is the one [`watch`] polls** — the loop
    /// itself, driven for real (#1040 V3, mutation Z1).
    ///
    /// `watch` is production code with no other coverage: every other reload
    /// test calls [`Watcher::poll`] by hand. A `watch` that rebuilt its own
    /// watcher (F1's exact defect, moved one function along) would re-load,
    /// double every diagnostic, re-open V2's race at full width — and, because
    /// it would rebuild from `xdg::config_layers`, watch the *process's* real
    /// layers instead of the ones it was handed, which is what this notices.
    ///
    /// Driven on the shared runtime at a 10 ms cadence rather than the
    /// production three seconds, which is why `watch` takes its interval as a
    /// parameter.
    #[test]
    fn the_watch_loop_republishes_the_watcher_it_was_given() {
        let mut overlay = Overlay::new();
        overlay.write("style = \"lcd\"\n");
        let (resolved, watcher) = boot(&overlay.layers(), &no_env());
        let leds = Mutable::new(resolved);
        assert_eq!(leds.get().style, DisplayStyle::Lcd, "live control");

        overlay.write("style = \"crt\"\ncolor = \"rainbow\"\n");

        let seen = hytte::reactive::runtime::handle().block_on({
            let leds = leds.clone();
            async move {
                let watching = watch(
                    leds.clone(),
                    watcher,
                    std::sync::Arc::new(no_env()),
                    Duration::from_millis(10),
                );
                tokio::select! {
                    () = watching => false,
                    republished = settles(&leds, DisplayStyle::Crt) => republished,
                }
            }
        });

        assert!(
            seen,
            "the save must reach the Mutable through the loop, got {:?}",
            leds.get()
        );
        assert_eq!(leds.get().color, ColorMap::Rainbow, "…whole, not partly");
    }

    /// Wait (up to ~3 s) for `leds` to carry `want`, so the loop test has a
    /// bound instead of a fixed sleep.
    async fn settles(leds: &Mutable<CoreLeds>, want: DisplayStyle) -> bool {
        for _ in 0..600 {
            if leds.get().style == want {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        false
    }

    /// **Two layers really are watched** — an edit to the *base* reloads live
    /// too, not just an edit to the overlay (#1040 V8).
    ///
    /// Every other watcher test uses `Overlay::layers`, which is one path, so
    /// nothing pinned that `Watcher` stats each layer rather than the last one
    /// (or the first). It also settles what `live-verify.md` should say: the
    /// base layer needs no restart either.
    #[test]
    fn an_edit_to_the_base_layer_reloads_too() {
        let mut base = Overlay::new();
        let mut overlay = Overlay::new();
        base.write("style = \"crt\"\nfill = \"blank\"\n");
        overlay.write("style = \"lcd\"\n");
        let layers = vec![base.path.clone(), overlay.path.clone()];

        let mut watcher = watching(&layers);
        let current = watcher.resolved(&no_env(), Deprecations::Silent);
        assert_eq!(current.style, DisplayStyle::Lcd, "the overlay wins");
        assert_eq!(
            current.fill,
            Fill::Blank,
            "…over the base it does not state"
        );

        base.write("style = \"crt\"\nfill = \"spare\"\ncolor = \"rainbow\"\n");
        let next = watcher
            .poll(current, &no_env())
            .expect("a base-layer edit is a change too");

        assert_eq!(next.color, ColorMap::Rainbow, "the base's new key applies");
        assert_eq!(next.fill, Fill::Spare, "…and its changed one");
        assert_eq!(
            next.style,
            DisplayStyle::Lcd,
            "…while the overlay still wins"
        );
    }

    /// The structured fields the PR body advertises are **actually emitted**
    /// (#1040 V10).
    ///
    /// The sentence is what a human reads; the fields are what a
    /// `journalctl`/`RUST_LOG` filter selects on, and until this round nothing
    /// asserted a single one — this file's copy of the capture harness dropped
    /// them on the floor, so the claim could not have been false in a way any
    /// test could see.
    #[test]
    fn the_deprecation_line_carries_its_fields() {
        let (captured, _guard) = capture();
        resolve(
            CoreLeds::default(),
            &env(&[("TROLLSHELL_CORE_LEDS_STYLE", "crt")]),
            Deprecations::Announce,
        );

        let event = captured
            .events()
            .into_iter()
            .find(|e| e.message == announced(&STYLE))
            .expect("the deprecation line");
        assert_eq!(
            event.fields.get("subsystem").map(String::as_str),
            Some("core-leds")
        );
        assert_eq!(
            event.fields.get("var").map(String::as_str),
            Some("TROLLSHELL_CORE_LEDS_STYLE")
        );
        assert_eq!(event.fields.get("key").map(String::as_str), Some("style"));
        assert_eq!(
            event.fields.get("accepts").map(String::as_str),
            Some(STYLE.file_accepts)
        );
        assert_eq!(
            event.fields.get("file").map(String::as_str),
            Some(crate::config::overlay_display(CoreLedsConfig::NAME).as_str())
        );
    }

    /// The poll cadence is `places.toml`'s, and short enough that an edit
    /// feels live rather than eventual.
    #[test]
    fn the_poll_interval_is_a_few_seconds() {
        assert_eq!(CONFIG_POLL_INTERVAL, Duration::from_secs(3));
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    fn with(f: impl FnOnce(&mut CoreLedsConfig)) -> CoreLedsConfig {
        let mut config = CoreLedsConfig::default();
        f(&mut config);
        config
    }

    fn with_style(style: &str) -> CoreLedsConfig {
        with(|c| c.style = style.to_string())
    }
}
