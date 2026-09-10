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
//! It does not win, and it does not take anything down.
//!
//! # Two spellings, one parser
//!
//! The file's values are spelt exactly as the environment variables accepted
//! them, with one deliberate exception: `rows` is a TOML integer, and `0`
//! spells the automatic wide rectangle that `TROLLSHELL_CORE_LEDS_ROWS` spelt
//! `rect`. [`rows_spelling`] maps the integer back onto the variable's
//! vocabulary so **one** parser decides both — [`CoreLedsConfig::parsed`] is
//! the only path from raw spelling to [`CoreLeds`], and
//! [`CoreLedsConfig::validate`] is that same call with the value discarded, so
//! "what the file rejects" and "what the variable rejects" cannot drift.
//!
//! # Live reload
//!
//! [`Watcher`] polls every layer's mtime on [`CONFIG_POLL_INTERVAL`] — the
//! `places.toml` idiom (`hytte_services::places::watch_config`), a single
//! `stat` per layer per tick, re-reading only when a stamp actually moves. A
//! reload that fails to parse or validate **keeps the last good file layer**
//! and warns; a reload never re-announces a deprecated variable, and the
//! variable keeps winning across reloads.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::prelude::Service;
use hytte::reactive::{registry, spawn_supervised};
use hytte_config::subsystem::{self, ConfigError, Subsystem};
use hytte_config::xdg;
use hytte_preem::{ColorMap, DisplayStyle, Fill};

use super::warn_deprecated_env;

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

/// Parse a `rows` value: `rect` for the automatic shape, or a positive row
/// count. `0` is rejected rather than silently clamped — as a *count* it is a
/// typo, not an intent, which is why the file spells "automatic" as the
/// integer `0` and this parser never sees it (see [`rows_spelling`]).
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
        Ok(n) if n > 0 => Ok(Some(n)),
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

/// The file's integer `rows` in the environment variable's vocabulary: `0`
/// (and, through `#[serde(default)]`, an absent key) is `rect`.
///
/// This is the *whole* translation between the two spellings, and it exists so
/// [`parse_core_leds_rows`] stays the single judge of a row count. A negative
/// integer renders as `-3` and is rejected by that parser, naming the value the
/// user actually wrote.
fn rows_spelling(rows: i64) -> String {
    if rows == 0 {
        "rect".to_string()
    } else {
        rows.to_string()
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
    /// What both accept, as it appears in a diagnostic.
    expected: &'static str,
}

const STYLE: Knob = Knob {
    var: "TROLLSHELL_CORE_LEDS_STYLE",
    key: "style",
    expected: "vfd/lcd/oled/crt",
};
const COLOR: Knob = Knob {
    var: "TROLLSHELL_CORE_LEDS_COLOR",
    key: "color",
    expected: "style/rainbow/transpride/heat/#rrggbb",
};
const ROWS: Knob = Knob {
    var: "TROLLSHELL_CORE_LEDS_ROWS",
    key: "rows",
    expected: "rect (0 in the file) for the automatic rectangle, or a positive row count",
};
const FILL: Knob = Knob {
    var: "TROLLSHELL_CORE_LEDS_FILL",
    key: "fill",
    expected: "spare/blank",
};

// ── The file schema ──────────────────────────────────────────────────────────

/// Why a `core-leds.toml` value was rejected.
///
/// Carries the key, the spelling the user wrote and the vocabulary that was
/// expected, so the journal line is actionable without opening the source.
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
        write!(f, "{key} = {value} is not one of: {expected}")
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
    rows: i64,
    fill: String,
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
            rows: 0,
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
        let rows = rows_spelling(self.rows);
        Ok(CoreLeds {
            style: parse_core_leds_style(&self.style)
                .map_err(|bad| InvalidValue::of(&STYLE, bad))?,
            color: parse_core_leds_color(&self.color)
                .map_err(|bad| InvalidValue::of(&COLOR, bad))?,
            rows: parse_core_leds_rows(&rows).map_err(|bad| InvalidValue::of(&ROWS, bad))?,
            fill: parse_core_leds_fill(&self.fill).map_err(|bad| InvalidValue::of(&FILL, bad))?,
        })
    }
}

impl InvalidValue {
    fn of(knob: &Knob, value: &str) -> Self {
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

# Rows in the lamp matrix. 0 is the automatic wide rectangle, picked from the
# core count (16x4 on a 64-thread box, 4x1 at 4 cores) — this is the shape the
# retired TROLLSHELL_CORE_LEDS_ROWS spelt "rect". Any positive number pins the
# row count instead and the columns fall out of it.
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
fn env_key<'a, T>(
    knob: &Knob,
    raw: Option<&'a str>,
    parse: impl FnOnce(&'a str) -> Result<T, &'a str>,
    fallback: T,
    announce: Deprecations,
) -> T {
    let Some(raw) = raw else { return fallback };
    if announce == Deprecations::Announce {
        warn_deprecated_env(CoreLedsConfig::NAME, knob.var, knob.key);
    }
    match parse(raw) {
        Ok(value) => value,
        Err(bad) => {
            tracing::warn!(
                var = knob.var,
                value = bad,
                expected = knob.expected,
                "per-core LED panel option unrecognized; falling through to the config file",
            );
            fallback
        }
    }
}

/// The environment layered over `file`, key by key.
///
/// `lookup` is injected rather than read from the process: `unsafe_code =
/// "forbid"` rules out `std::env::set_var` (it is an `unsafe fn` in edition
/// 2024), so a test that drove the real environment could not exist at all,
/// and one that read it would depend on the developer's shell.
fn resolve(
    file: CoreLeds,
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
            file.style,
            announce,
        ),
        color: env_key(
            &COLOR,
            color.as_deref(),
            parse_core_leds_color,
            file.color,
            announce,
        ),
        rows: env_key(
            &ROWS,
            rows.as_deref(),
            parse_core_leds_rows,
            file.rows,
            announce,
        ),
        fill: env_key(
            &FILL,
            fill.as_deref(),
            parse_core_leds_fill,
            file.fill,
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

/// A layer's last-modified time, or `None` when it does not exist (the normal
/// case for the overlay) or cannot be stat'd.
fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
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
    stamps: Vec<Option<SystemTime>>,
    last_good: CoreLeds,
}

impl Watcher {
    /// Start watching from *now*: the current stamps are the baseline, so the
    /// first [`poll`](Self::poll) reports only edits made after construction.
    ///
    /// The initial load happens here too, and a failure is loud and survivable
    /// — the built-in default, with an `error!` naming the file and the reason,
    /// which is [`subsystem::load_or_default`]'s policy applied to explicit
    /// paths.
    fn observe(paths: Vec<PathBuf>) -> Self {
        let stamps = paths.iter().map(|p| mtime(p)).collect();
        let last_good = match load_layer(&paths) {
            Ok(config) => config,
            Err(e) => {
                tracing::error!(
                    subsystem = CoreLedsConfig::NAME,
                    error = %e,
                    "config unusable; falling back to the built-in default"
                );
                CoreLeds::default()
            }
        };
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
    fn poll(
        &mut self,
        current: CoreLeds,
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Option<CoreLeds> {
        let now: Vec<Option<SystemTime>> = self.paths.iter().map(|p| mtime(p)).collect();
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
async fn watch(leds: Mutable<CoreLeds>) {
    let mut watcher = Watcher::observe(xdg::config_layers(CoreLedsConfig::NAME));
    loop {
        tokio::time::sleep(CONFIG_POLL_INTERVAL).await;
        if let Some(next) = watcher.poll(leds.get(), &process_env) {
            tracing::info!(subsystem = CoreLedsConfig::NAME, "config changed; reloaded");
            leds.set(next);
        }
    }
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
        let watcher = Watcher::observe(xdg::config_layers(CoreLedsConfig::NAME));
        // The one announcing resolution in the process: every set variable
        // gets its deprecation line here, and nowhere else.
        let leds = Mutable::new(watcher.resolved(&process_env, Deprecations::Announce));
        spawn_supervised("core-leds", {
            let leds = leds.clone();
            move || watch(leds.clone())
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
        ROWS, STYLE, Watcher, parse_core_leds_color, parse_core_leds_fill, parse_core_leds_rows,
        parse_core_leds_style, parse_hex_rgb, resolve, rows_spelling,
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
            with(|c| c.rows = -2).validate(),
            Err(InvalidValue::of(&ROWS, "-2")),
            "a negative row count is rejected in the integer spelling too"
        );

        assert_eq!(parse_core_leds_fill("none"), Err("none"));
        assert_eq!(
            with(|c| c.fill = "none".into()).validate(),
            Err(InvalidValue::of(&FILL, "none"))
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
        assert_eq!(rows_spelling(0), "rect");
        assert_eq!(rows_spelling(4), "4");
        assert_eq!(rows_spelling(-2), "-2");

        assert_eq!(
            with(|c| c.rows = 0).parsed().expect("valid").rows,
            None,
            "0 is the automatic rectangle"
        );
        assert_eq!(with(|c| c.rows = 3).parsed().expect("valid").rows, Some(3));
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
        let file = crate::config::overlay_display(CoreLedsConfig::NAME);
        let expected: Vec<String> = [&STYLE, &COLOR, &ROWS, &FILL]
            .into_iter()
            .map(|knob| crate::config::deprecation_message(knob.var, knob.key, &file))
            .collect();
        captured
            .events()
            .into_iter()
            .filter(|e| e.level == tracing::Level::WARN && expected.contains(&e.message))
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
        let file = crate::config::overlay_display(CoreLedsConfig::NAME);
        assert_eq!(
            deprecations(&captured),
            vec![
                crate::config::deprecation_message(STYLE.var, STYLE.key, &file),
                crate::config::deprecation_message(FILL.var, FILL.key, &file),
            ],
            "one line per *set* variable, in knob order, and no line for the two unset ones"
        );
    }

    /// The other half, and the one that needs a **live control**: asserting an
    /// absence against a capture that observed nothing at all is not an
    /// assertion. The unparseable-value warning proves the capture was wired
    /// at the moment the absence was observed.
    #[test]
    fn an_unset_variable_announces_nothing() {
        let (captured, _guard) = capture();

        resolve(
            CoreLeds::default(),
            &env(&[("TROLLSHELL_CORE_LEDS_COLOR", "puce")]),
            Deprecations::Announce,
        );

        let events = captured.events();
        assert!(
            events
                .iter()
                .any(|e| e.message.contains("per-core LED panel option unrecognized")),
            "live control: the capture must be observing this thread, got {events:?}"
        );
        let file = crate::config::overlay_display(CoreLedsConfig::NAME);
        assert_eq!(
            deprecations(&captured),
            vec![crate::config::deprecation_message(
                COLOR.var, COLOR.key, &file
            )],
            "only the one variable that is actually set is announced"
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
            events
                .iter()
                .any(|e| e.message.contains("per-core LED panel option unrecognized")),
            "live control: the capture must be observing this thread, got {events:?}"
        );
        assert!(
            deprecations(&captured).is_empty(),
            "a reload must not re-announce: {events:?}"
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
            std::fs::write(&self.path, body).expect("write");
            self.stamp += 1;
            let file = std::fs::File::options()
                .write(true)
                .open(&self.path)
                .expect("open");
            file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(self.stamp))
                .expect("set mtime");
        }

        fn layers(&self) -> Vec<PathBuf> {
            vec![self.path.clone()]
        }
    }

    /// The payoff: an edit while the shell runs re-resolves without a restart.
    ///
    /// **Red if `Watcher::poll` stops re-reading** (return `None` before the
    /// load, or drop the `self.last_good = config` assignment).
    #[test]
    fn a_changed_file_is_picked_up() {
        let mut overlay = Overlay::new();
        overlay.write("style = \"lcd\"\n");
        let mut watcher = Watcher::observe(overlay.layers());
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
        let mut watcher = Watcher::observe(overlay.layers());
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
        let mut watcher = Watcher::observe(overlay.layers());
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
        let mut watcher = Watcher::observe(overlay.layers());
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
