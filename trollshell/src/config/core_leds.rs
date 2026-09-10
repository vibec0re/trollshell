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
//! and the unusable-value line names the key and the file itself.
//!
//! # Two spellings, one parser
//!
//! The file's values are spelt exactly as the environment variables accepted
//! them — `rows` included, since [`Rows`] takes the word `"rect"` as readily as
//! the variable did. It additionally takes the TOML integer `0` for the same
//! automatic rectangle, which is what [`CoreLedsConfig::DEFAULT_TOML`] states,
//! and [`rows_spelling`] maps that back onto the variable's vocabulary so
//! **one** parser decides both — [`CoreLedsConfig::parsed`] is the only path
//! from raw spelling to [`CoreLeds`], and [`CoreLedsConfig::validate`] is that
//! same call with the value discarded, so "what the file rejects" and "what
//! the variable rejects" cannot drift.
//!
//! # Live reload
//!
//! [`Watcher`] polls every layer's [`stamp`] on [`CONFIG_POLL_INTERVAL`] — the
//! `places.toml` idiom (`hytte_services::places::watch_config`), a single
//! `stat` per layer per tick, re-reading only when a stamp actually moves. A
//! reload that fails to parse or validate **keeps the last good file layer**
//! and warns; a deleted layer falls back to the layer below it (and, with
//! nothing left, to the built-in defaults); a reload never re-announces a
//! deprecated variable, and the variable keeps winning across reloads.
//!
//! The config is loaded **once** per process, in [`startup`]; the watcher only
//! stamps. Two loads at startup would double every diagnostic and open a race
//! against an edit landing between them — see [`Watcher::observing`].

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::prelude::Service;
use hytte::reactive::{registry, spawn_supervised};
use hytte_config::subsystem::{self, ConfigError, Subsystem};
use hytte_config::xdg;
use hytte_preem::{ColorMap, DisplayStyle, Fill};

use super::{warn_deprecated_env, warn_unusable_env};

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
    /// What both accept, phrased to read after "expected" and after "it
    /// accepts" — the two sentences it appears in
    /// ([`crate::config::unusable_env_message`] and
    /// [`crate::config::deprecation_message`]) plus [`InvalidValue`]'s.
    expected: &'static str,
}

const STYLE: Knob = Knob {
    var: "TROLLSHELL_CORE_LEDS_STYLE",
    key: "style",
    expected: "one of vfd/lcd/oled/crt",
};
const COLOR: Knob = Knob {
    var: "TROLLSHELL_CORE_LEDS_COLOR",
    key: "color",
    expected: "one of style/rainbow/transpride/heat, or an #rrggbb literal",
};
const ROWS: Knob = Knob {
    var: "TROLLSHELL_CORE_LEDS_ROWS",
    key: "rows",
    // Both file spellings of the automatic shape, and the cap, stated in the
    // one place a user reads about this key outside `DEFAULT_TOML` — which,
    // until nix renders a base file, exists nowhere on disk (#1040 F5).
    expected: "\"rect\" (or 0) for the automatic rectangle, or a row count from 1 to 64",
};
const FILL: Knob = Knob {
    var: "TROLLSHELL_CORE_LEDS_FILL",
    key: "fill",
    expected: "one of spare/blank",
};

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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CoreLedsConfig {
    style: String,
    color: String,
    rows: Rows,
    fill: String,
}

/// The two TOML spellings the `rows` key accepts.
///
/// `#[serde(untagged)]`, so `rows = 0` **and** `rows = "rect"` both
/// deserialize and both reach [`parse_core_leds_rows`], which stays the single
/// judge. That is not a convenience: `rect` is the word the deprecated
/// `TROLLSHELL_CORE_LEDS_ROWS` accepted and the word the deprecation line walks
/// a migrating user toward, so a file that rejected it broke #869's own
/// contract — and rejected it as a **whole-file** `ConfigError::Schema`, taking
/// every other key in the file down with the typo and dropping the panel back
/// to built-in defaults (#1040 F5).
///
/// The residual whole-file case is a value that is neither an integer nor a
/// string — `rows = true`, `rows = [1]`. That is the price of a `serde`-shaped
/// schema and it is *tested* rather than pretended away
/// (`a_rows_value_of_the_wrong_type_is_still_a_whole_file_error`): there is no
/// per-key hook in `hytte_config::subsystem` for a type mismatch, since the
/// mismatch happens inside `serde`'s deserializer before any subsystem code
/// runs. Hoisting one is #1041's business, not this PR's. The two spellings a
/// user plausibly writes are both covered here.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Rows {
    /// A TOML integer: `0` is the automatic rectangle, anything else is a row
    /// count for [`parse_core_leds_rows`] to judge.
    Count(i64),
    /// A TOML string: the environment variable's own vocabulary, handed to
    /// [`parse_core_leds_rows`] untouched.
    Word(String),
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
    /// The file's raw spellings as the resolved [`CoreLeds`], or the first key
    /// that does not parse.
    ///
    /// The one path from spelling to value: [`Self::validate`] is this call
    /// with the value thrown away, and the resolver is this call with the
    /// environment layered on top. Nothing else may parse a `core-leds.toml`
    /// value.
    fn parsed(&self) -> Result<CoreLeds, InvalidValue> {
        let rows = rows_spelling(&self.rows);
        Ok(CoreLeds {
            style: parse_core_leds_style(&self.style)
                .map_err(|bad| InvalidValue::of(&STYLE, bad))?,
            color: parse_core_leds_color(&self.color)
                .map_err(|bad| InvalidValue::of(&COLOR, bad))?,
            // The offending value is reported in the spelling the *file* uses,
            // not the one the parser was handed: `rows = 0` is spelt `rect`
            // going in, and echoing `rect` back at a user who wrote something
            // else would name a value that is nowhere in their file.
            rows: parse_core_leds_rows(&rows)
                .map_err(|_| InvalidValue::written(&ROWS, &rows_as_written(&self.rows)))?,
            fill: parse_core_leds_fill(&self.fill).map_err(|bad| InvalidValue::of(&FILL, bad))?,
        })
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
            expected: knob.expected,
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

    type Error = InvalidValue;

    /// Every value goes through the same parser the environment variable used,
    /// so the file rejects exactly what the variable rejected.
    fn validate(&self) -> Result<(), Self::Error> {
        self.parsed().map(|_| ())
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
                warn_deprecated_env(CoreLedsConfig::NAME, knob.var, knob.key, knob.expected);
            }
            value
        }
        Err(bad) => {
            // Not gated on `announce`: an unusable variable is still unusable
            // after a reload, and this line is a fact about the environment
            // rather than the once-per-startup announcement the latch guards.
            warn_unusable_env(CoreLedsConfig::NAME, knob.var, bad, knob.key, knob.expected);
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
    lookup: &impl Fn(&str) -> Option<String>,
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

/// The merged, validated file layer — no environment.
fn load_layer(paths: &[PathBuf]) -> Result<CoreLeds, ConfigError> {
    let loaded = subsystem::load_from::<CoreLedsConfig>(paths)?;
    // `load_from` already ran `validate`, which *is* `parsed`, so this cannot
    // fail — mapped rather than unwrapped so the impossible case degrades to
    // the same "keep the last good one" path as a real error instead of
    // panicking in the shell's main loop.
    loaded
        .config
        .parsed()
        .map_err(|e| ConfigError::Invalid(e.to_string()))
}

/// Watches every `core-leds.toml` layer for live reload by polling their
/// mtimes, in the shape `hytte_config::places::ConfigWatcher` established.
///
/// Holds the last file layer that loaded cleanly, which is what a malformed
/// save keeps: the panel goes on rendering the last good skin rather than
/// snapping back to the built-in default the moment a hand edit is mid-word.
struct Watcher {
    paths: Vec<PathBuf>,
    stamps: Vec<Option<(SystemTime, u64)>>,
    last_good: CoreLeds,
}

impl Watcher {
    /// Start watching from *now*: the current stamps are the baseline, so the
    /// first [`poll`](Self::poll) reports only edits made after construction.
    ///
    /// **Stamp-only** — it does not load (#1040 F1). `last_good` is seeded from
    /// the caller's already-loaded value, because the process loads this config
    /// exactly once, in [`startup`]. Before this split, `start` loaded and then
    /// handed `watch` a constructor that loaded *again*: two `unknown key`
    /// warnings for one typo, two `config unusable` errors for one broken file,
    /// and — worse — a race, since an edit landing between the two loads was
    /// baselined into the poller's stamps while the published value came from
    /// the first load, leaving the panel one save stale until the next edit.
    /// `places::ConfigWatcher::new` splits the same way and for the same
    /// reason: it stamps, and `PlacesService::start` does the one load.
    ///
    /// A supervised restart of [`watch`] re-stamps from now and re-seeds from
    /// the same startup value. That is a panic path, the seed is only ever the
    /// *fallback* for a poll that has not fired yet, and the next save corrects
    /// it; reloading there would put the double-load back for the sake of an
    /// edge nobody is in.
    fn observing(paths: Vec<PathBuf>, last_good: CoreLeds) -> Self {
        let stamps = paths.iter().map(|p| stamp(p)).collect();
        Self {
            paths,
            stamps,
            last_good,
        }
    }

    /// The dressing this environment and the current file layers resolve to.
    fn resolved(
        &self,
        lookup: &impl Fn(&str) -> Option<String>,
        announce: Deprecations,
    ) -> CoreLeds {
        resolve(self.last_good, lookup, announce)
    }

    /// Reload and return the fresh dressing when some layer's mtime has moved
    /// *and* the result differs from `current`; otherwise `None`.
    ///
    /// A layer that stops parsing (or stops validating) keeps
    /// [`Self::last_good`] and warns — once per edit rather than once per tick,
    /// because the stamp is taken before the load, so a file left malformed is
    /// not re-read until it is saved again.
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
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Option<CoreLeds> {
        let now: Vec<Option<(SystemTime, u64)>> = self.paths.iter().map(|p| stamp(p)).collect();
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
/// panel within [`CONFIG_POLL_INTERVAL`] without restarting the shell.
///
/// `seed` is [`startup`]'s already-loaded file layer, threaded through rather
/// than re-loaded here — see [`Watcher::observing`]. It is `Copy`, which is
/// what lets this ride `spawn_supervised`'s `Fn` factory unchanged.
async fn watch(leds: Mutable<CoreLeds>, seed: CoreLeds) {
    let mut watcher = Watcher::observing(xdg::config_layers(CoreLedsConfig::NAME), seed);
    loop {
        tokio::time::sleep(CONFIG_POLL_INTERVAL).await;
        if let Some(next) = watcher.poll(leds.get(), &process_env) {
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

/// The process's **one** startup resolution: load the layers once, then resolve
/// the environment over them announcing every deprecated variable that is set.
///
/// Returns the file layer (the poller's seed) beside the resolved dressing,
/// because the two are different things and [`watch`] needs the former —
/// resolving is where the environment wins, and seeding the watcher with a
/// value the environment already overrode would make the *file's* value
/// unrecoverable on the first reload.
///
/// This is the only place in production that passes
/// [`Deprecations::Announce`], and it takes `lookup` rather than reading the
/// process environment so a test can drive the real call site (#1040 F3):
/// before, `Deprecations::Announce` sat inline in [`CoreLedsService::start`],
/// where flipping it to `Silent` left the whole suite green and nobody would
/// ever have heard a deprecation line again.
fn startup(paths: &[PathBuf], lookup: &impl Fn(&str) -> Option<String>) -> (CoreLeds, CoreLeds) {
    let layered = initial_load(paths);
    (layered, resolve(layered, lookup, Deprecations::Announce))
}

// ── The service ──────────────────────────────────────────────────────────────

/// The handle the Stats panel subscribes to.
pub struct CoreLedsHandles {
    leds: Mutable<CoreLeds>,
}

pub struct CoreLedsService;

impl Service for CoreLedsService {
    type Handles = CoreLedsHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        // One load, one announcing resolution — both inside `startup`, which
        // is also what a test drives. Nothing here chooses a `Deprecations`,
        // so this call site cannot get the once-ness wrong.
        let (layered, resolved) = startup(&xdg::config_layers(CoreLedsConfig::NAME), &process_env);
        let leds = Mutable::new(resolved);
        spawn_supervised("core-leds", {
            let leds = leds.clone();
            move || watch(leds.clone(), layered)
        });
        CoreLedsHandles { leds }
    }
}

pub fn service() -> CoreLedsService {
    CoreLedsService
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
        COLOR, CONFIG_POLL_INTERVAL, CoreLeds, CoreLedsConfig, Deprecations, FILL, InvalidValue,
        MAX_ROWS, ROWS, Rows, STYLE, Watcher, initial_load, parse_core_leds_color,
        parse_core_leds_fill, parse_core_leds_rows, parse_core_leds_style, parse_hex_rgb, resolve,
        rows_spelling, startup,
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
            loaded.config.parsed().expect("the default validates"),
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
            assert!(with_style(style.name()).validate().is_ok());
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
            with_style("plasma").validate(),
            Err(InvalidValue::of(&STYLE, "plasma"))
        );

        assert_eq!(parse_core_leds_color("puce"), Err("puce"));
        // `rgb` is `ColorMap::name`'s output but not an input: it carries no
        // components, so accepting it would mean inventing a colour.
        assert_eq!(parse_core_leds_color("rgb"), Err("rgb"));
        assert_eq!(
            with(|c| c.color = "puce".into()).validate(),
            Err(InvalidValue::of(&COLOR, "puce"))
        );

        assert_eq!(parse_core_leds_rows("0"), Err("0"), "0 rows is a typo");
        assert_eq!(parse_core_leds_rows("-2"), Err("-2"));
        assert_eq!(parse_core_leds_rows("many"), Err("many"));
        assert_eq!(
            with(|c| c.rows = Rows::Count(-2)).validate(),
            Err(InvalidValue::written(&ROWS, "-2")),
            "a negative row count is rejected in the integer spelling too"
        );

        assert_eq!(parse_core_leds_fill("none"), Err("none"));
        assert_eq!(
            with(|c| c.fill = "none".into()).validate(),
            Err(InvalidValue::of(&FILL, "none"))
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
            with(|c| c.rows = Rows::Count(cap))
                .parsed()
                .expect("valid")
                .rows,
            Some(MAX_ROWS)
        );

        // The boundary + 1 is rejected, in both — with a *per-key* error that
        // names the key and the value, not a whole-file failure (#1040 F5).
        let over = MAX_ROWS + 1;
        assert_eq!(parse_core_leds_rows(&over.to_string()), Err("65"));
        assert_eq!(
            with(|c| c.rows = Rows::Count(cap + 1)).validate(),
            Err(InvalidValue::written(&ROWS, "65"))
        );

        // …and so is every shape of absurd, including the two that used to
        // take the shell down.
        for absurd in ["10000000", "9223372036854775807", "18446744073709551616"] {
            assert_eq!(parse_core_leds_rows(absurd), Err(absurd), "{absurd} rows");
        }
        assert_eq!(
            with(|c| c.rows = Rows::Count(i64::MAX)).validate(),
            Err(InvalidValue::written(&ROWS, "9223372036854775807"))
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
            with_style("plasma")
                .validate()
                .expect_err("plasma is not a skin")
                .to_string(),
            "style = \"plasma\" is not valid; expected one of vfd/lcd/oled/crt",
            "a string key's value is quoted — `style = plasma` is not TOML at all"
        );
        assert_eq!(
            with(|c| c.rows = Rows::Count(-2))
                .validate()
                .expect_err("-2 is not a row count")
                .to_string(),
            "rows = -2 is not valid; expected \"rect\" (or 0) for the automatic \
             rectangle, or a row count from 1 to 64",
            "…while an integer key's is bare"
        );
        assert_eq!(
            with(|c| c.rows = Rows::Word("many".into()))
                .validate()
                .expect_err("many is not a row count")
                .to_string(),
            "rows = \"many\" is not valid; expected \"rect\" (or 0) for the automatic \
             rectangle, or a row count from 1 to 64",
            "…and `rows`' string arm quotes, because that is what the file says"
        );
    }

    /// The cap the diagnostic quotes is the cap the parser enforces.
    ///
    /// `ROWS.expected` spells the bound out for the reader ("a row count from
    /// 1 to 64") because that sentence is what a migrating user gets in the
    /// journal, and it is a literal. **Red if [`MAX_ROWS`] moves without it.**
    #[test]
    fn the_diagnostic_quotes_the_cap_it_enforces() {
        assert!(
            ROWS.expected.contains(&format!("1 to {MAX_ROWS}")),
            "the row cap {MAX_ROWS} is not the one {:?} promises",
            ROWS.expected
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
            with(|c| c.rows = Rows::Count(0))
                .parsed()
                .expect("valid")
                .rows,
            None,
            "0 is the automatic rectangle"
        );
        assert_eq!(
            with(|c| c.rows = Rows::Count(3))
                .parsed()
                .expect("valid")
                .rows,
            Some(3)
        );
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
        let resolved = rect.config.parsed().expect("valid");
        assert_eq!(resolved.rows, None, "the word is the automatic rectangle");
        assert_eq!(
            resolved.style,
            DisplayStyle::Crt,
            "…and the keys beside it survived, which a whole-file error would not have"
        );

        assert_eq!(
            with(|c| c.rows = Rows::Word("4".into()))
                .parsed()
                .expect("valid")
                .rows,
            Some(4),
            "the string arm goes through the same single judge"
        );
        assert_eq!(
            with(|c| c.rows = Rows::Word("many".into())).validate(),
            Err(InvalidValue::written(&ROWS, "\"many\"")),
            "an unknown word is a named per-key error, not a serde type message"
        );
    }

    /// The residual whole-file case, **tested rather than pretended away**: a
    /// `rows` that is neither an integer nor a string.
    ///
    /// `serde` decides this inside its own deserializer, before any subsystem
    /// code runs, and `hytte_config::subsystem` offers no per-key hook for a
    /// type mismatch — so `rows = true` is a `ConfigError::Schema` and takes
    /// the file with it. Documented on [`Rows`], flagged to #1041, and pinned
    /// here so the day a per-key hook exists this test is the thing that
    /// notices (#1040 F5).
    #[test]
    fn a_rows_value_of_the_wrong_type_is_still_a_whole_file_error() {
        let err = subsystem::assemble::<CoreLedsConfig>(&[(
            PathBuf::from("/o.toml"),
            "rows = true\nstyle = \"crt\"\n".into(),
        )])
        .expect_err("a boolean `rows` fits neither arm");

        assert!(
            matches!(err, subsystem::ConfigError::Schema(ref m) if m.contains("rows")),
            "the key must at least be named: {err}"
        );
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
            loaded.config.parsed().expect("valid"),
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
        let resolved = loaded.config.parsed().expect("valid");

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
            let mut visitor = MessageVisitor::default();
            event.record(&mut visitor);
            self.events
                .lock()
                .expect("not poisoned")
                .push(CapturedEvent {
                    level: *event.metadata().level(),
                    message: visitor.message,
                });
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[derive(Default)]
    struct MessageVisitor {
        message: String,
    }

    impl tracing::field::Visit for MessageVisitor {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                self.message = value.to_string();
            }
        }
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            // The message itself arrives here: `tracing` renders format args
            // through `Debug`, and `Arguments`' `Debug` is its `Display`.
            if field.name() == "message" {
                self.message = format!("{value:?}");
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
        let captured = Captured::default();
        let guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(captured.clone()));
        tracing::callsite::rebuild_interest_cache();
        (captured, guard)
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
            knob.expected,
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
    /// all, so a live shell does not repeat the line every few seconds.
    ///
    /// **Red if the reload path is switched to `Announce`.**
    #[test]
    fn a_silent_resolution_announces_nothing() {
        let (captured, _guard) = capture();

        resolve(
            CoreLeds::default(),
            &env(&[
                ("TROLLSHELL_CORE_LEDS_STYLE", "crt"),
                ("TROLLSHELL_CORE_LEDS_COLOR", "puce"),
            ]),
            Deprecations::Silent,
        );

        let events = captured.events();
        assert!(
            events.iter().any(|e| e
                .message
                .contains("TROLLSHELL_CORE_LEDS_COLOR is set to `puce`")),
            "live control: the capture must be observing this thread, got {events:?}"
        );
        assert!(
            deprecations(&captured).is_empty(),
            "a reload must not re-announce: {events:?}"
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
    /// **Red if `startup` stops announcing.**
    #[test]
    fn the_startup_resolution_announces() {
        let (captured, _guard) = capture();

        let (layered, resolved) = startup(&[], &env(&[("TROLLSHELL_CORE_LEDS_STYLE", "crt")]));

        assert_eq!(
            layered,
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

    /// The startup sequence, minus the tokio spawn: **one** load, then a
    /// stamp-only watcher seeded from it — what `CoreLedsService::start` does
    /// through [`startup`].
    fn watching(paths: Vec<PathBuf>) -> Watcher {
        let seed = initial_load(&paths);
        Watcher::observing(paths, seed)
    }

    /// The payoff: an edit while the shell runs re-resolves without a restart.
    ///
    /// **Red if `Watcher::poll` stops re-reading** (return `None` before the
    /// load, or drop the `self.last_good = config` assignment).
    #[test]
    fn a_changed_file_is_picked_up() {
        let mut overlay = Overlay::new();
        overlay.write("style = \"lcd\"\n");
        let mut watcher = watching(overlay.layers());
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
        let mut watcher = watching(overlay.layers());
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

    /// A malformed save keeps the last good config rather than snapping the
    /// panel back to the built-in default mid-edit.
    ///
    /// **Red if the `Err` arm of `Watcher::poll` overwrites `last_good`.**
    #[test]
    fn a_malformed_file_keeps_the_last_good_config() {
        let mut overlay = Overlay::new();
        overlay.write("style = \"crt\"\n");
        let mut watcher = watching(overlay.layers());
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

        // Valid TOML, invalid value — the other half of "unusable".
        overlay.write("style = \"plasma\"\n");
        assert_eq!(watcher.poll(good, &no_env()), None);
        assert_eq!(
            watcher.resolved(&no_env(), Deprecations::Silent),
            good,
            "a rejected value keeps the last good skin too"
        );

        // …and a repaired file is picked up again.
        overlay.write("style = \"oled\"\n");
        let next = watcher.poll(good, &no_env()).expect("repaired → reload");
        assert_eq!(next.style, DisplayStyle::Oled);
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
        let mut watcher = watching(overlay.layers());
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
        let mut watcher = watching(overlay.layers());
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

    /// An unknown key is loud and harmless (merge rule 4): the file still
    /// loads, and the keys around the typo still apply.
    #[test]
    fn an_unknown_key_does_not_break_the_load() {
        let body = "colour = \"rainbow\"\nstyle = \"crt\"\n";
        let loaded =
            subsystem::assemble::<CoreLedsConfig>(&[(PathBuf::from("/o.toml"), body.into())])
                .expect("assembles");

        assert_eq!(loaded.unknown_keys, ["colour"]);
        let resolved = loaded.config.parsed().expect("valid");
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
    /// `Watcher::observing` goes back to loading instead of taking a seed.
    #[test]
    fn a_startup_loads_the_config_exactly_once() {
        let mut overlay = Overlay::new();
        overlay.write("colour = \"rainbow\"\nstyle = \"crt\"\n");

        let (captured, _guard) = capture();
        let (layered, _resolved) = startup(&overlay.layers(), &no_env());
        let _watcher = Watcher::observing(overlay.layers(), layered);

        assert_eq!(
            warnings(&captured),
            ["unknown key in config; ignoring it"],
            "one unknown key, one warning — live-verify.md promises exactly this"
        );
        assert_eq!(
            layered.style,
            DisplayStyle::Crt,
            "live control: the load under observation actually happened"
        );
    }

    /// The same, for the error half: a file nothing can parse says so **once**.
    #[test]
    fn a_startup_reports_an_unusable_file_exactly_once() {
        let mut overlay = Overlay::new();
        overlay.write("style = \"plasma\"\n");

        let (captured, _guard) = capture();
        let (layered, _resolved) = startup(&overlay.layers(), &no_env());
        let _watcher = Watcher::observing(overlay.layers(), layered);

        assert_eq!(
            errors(&captured),
            ["config unusable; falling back to the built-in default"],
            "one broken file, one error"
        );
        assert_eq!(
            layered,
            CoreLeds::default(),
            "…and it degrades to the built-in default rather than taking the shell down"
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
        let mut watcher = watching(overlay.layers());
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
        let mut watcher = watching(overlay.layers());

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
        let mut watcher = watching(overlay.layers());
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
