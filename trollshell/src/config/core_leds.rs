//! `core-leds.toml` — how the Stats drawer's per-core LED panel (#857) is
//! dressed, and the **first live subsystem** of the #866/#868 config layering
//! (#869).
//!
//! # Why this one was the pilot
//!
//! Four knobs, brand new, entirely self-contained, and actively being fiddled
//! with — so the payoff of a file over an environment variable (edit, save,
//! watch the panel re-skin; no shell restart) is felt on the first try. Small
//! enough to throw away and redo if the shape turned out wrong, which is the
//! point of piloting before committing the other ~9 subsystems.
//!
//! The shape did not turn out wrong, and #1044 hoisted every generic piece of
//! it into `hytte_config::subsystem` — the per-key tolerance, the environment
//! overlay and its three sentences, the layer poller, the test harnesses. What
//! is left in this file is the schema, the documented default, the four
//! parsers, the knob table and the service wiring: the parts a family #2 has to
//! write for itself, and nothing else.
//!
//! # Resolution order, per key
//!
//! 1. the **environment variable**, when it is set *and* parses — with one
//!    deprecation warning at startup ([`Deprecations`]);
//! 2. the **config layers**: `$XDG_CONFIG_DIRS/trollshell/core-leds.toml`
//!    (nix's base) then `$XDG_CONFIG_HOME/trollshell/core-leds.toml` (yours),
//!    merged by [`hytte_config::merge`]'s four rules;
//! 3. [`CoreLedsConfig::DEFAULT_TOML`] — the documented built-in default,
//!    which is the bottom merge layer, so a missing file behaves exactly like
//!    an unset variable did and says nothing about it.
//!
//! A **set but unparseable** variable keeps the pre-#869 behaviour: one `warn!`
//! naming the accepted values, then fall through to the layer below. Exactly
//! one line, not two, and both lines are startup-only — see
//! [`hytte_config::subsystem::env`] for why.
//!
//! # A bad *value* costs its own key, and only its own key
//!
//! Every key is judged on its own in [`CoreLedsConfig::parsed`]: the keys that
//! parse apply, each key that does not falls back to the built-in default with
//! one warning naming it, and [`CoreLedsConfig::validate`] is `Infallible` so
//! nothing in this schema can be a whole-file rejection (#1040 V1). A wrong
//! TOML **type** is the same kind of mistake and gets the same treatment —
//! `style = 5` costs `style` and nothing else — which is why every schema field
//! is a raw [`toml::Value`] and not even a `String` (#1040 T1). What is still
//! whole-file is a layer that is not TOML **at all** — an unterminated string,
//! an integer literal too large for TOML's `i64` — which at startup degrades to
//! the built-in defaults with a loud `error!` and on a reload keeps the last
//! good file.
//!
//! # Two spellings, one parser
//!
//! The file's values are spelt exactly as the environment variables accepted
//! them — `rows` included, since the key takes the word `"rect"` as readily as
//! the variable did. It additionally takes the TOML integer `0` for the same
//! automatic rectangle, which is what [`CoreLedsConfig::DEFAULT_TOML`] states,
//! and [`rows_spelling`] maps that back onto the variable's vocabulary so
//! **one** parser decides both — [`CoreLedsConfig::parsed`] is the only path
//! from raw spelling to [`CoreLeds`], so "what the file rejects" and "what the
//! variable rejects" cannot drift. The one deliberate divergence is that `0`,
//! which the variable never took, so the two vocabularies are stated separately
//! on the [`EnvKnob`] (#1040 V4).
//!
//! # Live reload
//!
//! [`watch::Watcher`] polls every layer's change stamp on [`CONFIG_POLL_INTERVAL`]
//! on AC power, [`BATTERY_CONFIG_POLL_INTERVAL`] on battery (#1041/#1081) — a
//! `(mtime, content hash)` read per layer per tick (`watch`'s own doc explains
//! why a hash rather than a bare `stat`), re-parsing only when a stamp actually
//! moves. A reload of a layer that is not TOML keeps the last good file layer
//! and warns; a deleted layer falls back to the layer below it (and, with
//! nothing left, to the built-in defaults); a reload never re-announces a
//! deprecated variable, and the variable keeps winning across reloads. See
//! [`hytte_config::subsystem::watch`] for the three orderings that make that
//! true, and for the generic `CadenceSource`/`wait_cadence` mechanism this
//! subsystem's battery split rides — everything below is this subsystem's own
//! half: mapping "on battery or not" to a `Duration`.

use std::path::PathBuf;
use std::time::Duration;

use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::prelude::Service;
use hytte::reactive::{registry, spawn_supervised};
use hytte_config::subsystem::env::{self, Deprecations, EnvKnob};
use hytte_config::subsystem::watch::{self, CadenceSource, EnvLookup};
use hytte_config::subsystem::{InvalidValue, Subsystem, keep, spelling};
use hytte_config::xdg;
use hytte_preem::{ColorMap, DisplayStyle, Fill};

// ── Battery-aware reload cadence (#1041/#1081) ───────────────────────────────

/// How often the running shell re-checks the config layers on AC power.
/// `hytte_config::subsystem::watch::POLL_INTERVAL` — `places.toml`'s own AC
/// cadence — restated as a local name so [`cadence`] reads as this
/// subsystem's own choice rather than a reach into `watch`'s internals.
const CONFIG_POLL_INTERVAL: Duration = watch::POLL_INTERVAL;

/// Re-check cadence on battery power (#1041): five times the AC interval.
///
/// `places.toml`'s own `BATTERY_CONFIG_POLL_INTERVAL` stretches by only 3x
/// (#505) because a *location* change is the thing it is racing to notice. A
/// hand edit to `core-leds.toml` is rarer still — it is look-and-feel
/// fiddling, not something that changes underneath you — so a wider stretch
/// costs nothing in felt responsiveness: 15 s to notice a saved file is still
/// "a live reload, no restart", the promise this module's own doc makes,
/// while more than halving the idle wakeups a laptop takes on battery for a
/// file that, in the overwhelmingly common case, never changes for the life
/// of the session.
const BATTERY_CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Battery-aware poll cadence: [`BATTERY_CONFIG_POLL_INTERVAL`] while on
/// battery power, else [`CONFIG_POLL_INTERVAL`]. Pure so the on-battery →
/// interval mapping is unit-testable without a real `UPower` — mirrors
/// `hytte_services::places::cadence` exactly.
fn cadence(on_battery: bool) -> Duration {
    if on_battery {
        BATTERY_CONFIG_POLL_INTERVAL
    } else {
        CONFIG_POLL_INTERVAL
    }
}

/// Best-effort on-battery snapshot for the production call site.
///
/// A private wrapper around [`hytte::services::upower::on_battery_now`],
/// mirroring the shape every in-crate `hytte-services` poller already uses
/// (`places`, `wifiscan`, `netconn`, `app_usage` each have their own private
/// `fn on_battery() -> bool` over `upower::on_battery_snapshot`) — this is the
/// same one-line wrapper, just calling the cross-crate `pub` accessor #1041
/// added instead of the in-crate `pub(crate)` one, since `core_leds` lives in
/// the `trollshell` binary rather than in `hytte-services` itself. See that
/// accessor's doc for the full contract (degrades to AC — normal cadence —
/// whenever the true state isn't known, never to the slow one).
fn on_battery() -> bool {
    hytte::services::upower::on_battery_now()
}

/// How the watcher reads the current battery state (#1041).
///
/// A boxed `Fn` rather than a direct call to [`self::on_battery`], the same
/// reason [`EnvLookup`] is injected: a test drives a fake power state instead
/// of the real (or absent) `UPower` daemon.
type BatterySource = std::sync::Arc<dyn Fn() -> bool + Send + Sync>;

/// Build a [`hytte_config::subsystem::watch::CadenceSource`] from a
/// battery-state source.
///
/// The composition is its own function, rather than inlined at the one
/// production call site, so it has its own test
/// (`the_cadence_source_follows_an_injected_battery_flag`, #1041): the closure
/// this returns calls `on_battery` **every time it is called**, not once at
/// construction, which is what lets a live power-state flip reach a poll
/// that's already mid-wait (`watch::wait_cadence`'s whole reason to re-check
/// rather than sleep the target in one shot). A mutation that captured
/// `on_battery()`'s value here instead of inside the closure — "ignore the
/// signal" — would still pass every other test in this module and only shows
/// up as a poller stuck at whatever cadence was true at startup.
fn battery_cadence_source(on_battery: BatterySource) -> CadenceSource {
    std::sync::Arc::new(move || cadence(on_battery()))
}

// ── The resolved dressing ────────────────────────────────────────────────────

/// How the per-core LED panel is dressed (#857).
///
/// The resolved, parsed form — what the rasteriser consumes.
/// [`CoreLedsConfig`] is the *file* form, whose keys are raw `toml::Value`s so
/// an unrecognised value can be reported rather than silently substituted.
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
/// Sized off what the panel can physically show, not off a round number. A lamp
/// row costs `CELL + GAP` = 11 buffer px (`hytte_preem::led_matrix`), the
/// panel's on-screen budget is `stats::CORE_PANEL_MAX_H` = 104 logical px and
/// the upscale factor never goes below 1× — so **9** rows already fill the
/// budget box, and past that every extra row is letterboxed back down and
/// resampled, which is the one thing the fixed dot grid exists to avoid
/// (#839/#843). The automatic shape's own appetite is smaller still: it takes
/// `⌈√cores / 2⌉` rows, which is 4 on a 64-thread box, 12 at 512 and **32** on
/// a hypothetical 4096-thread one.
///
/// 64 is therefore twice the most any real machine's automatic shape would want
/// and seven times what the budget box can render at 1× — deliberately
/// generous, because a pinned row count is the user's call to make even when it
/// is a *worse* shape than the automatic one. What it is not is unbounded:
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
/// `TROLLSHELL_CORE_LEDS_ROWS` and to `rows =` in the file. That is the property
/// the pilot exists to prove, used.
///
/// `rect` is Annika's word for it from #857, and since her second pass on the
/// same issue ("rectangle for led view would be still more preem tho") it now
/// keeps its promise: the automatic shape is `LedMatrix::wide`'s wide rectangle
/// rather than the near-square panel #861 shipped. Still automatic rather than a
/// pinned default row count, because a fixed `rows = 4` reads well at 64 cores
/// and absurdly at 4.
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

/// [`spelling`] for `rows`, the one key whose file vocabulary has a word the
/// variable's does not: the TOML integer `0` (and, through
/// `#[serde(default)]`, an absent key) is `rect`.
///
/// This is the *whole* translation between the two spellings, and it exists so
/// [`parse_core_leds_rows`] stays the single judge of a row count. A negative
/// integer renders as `-3` and is rejected by that parser, naming the value the
/// user actually wrote; so does `true`, `4.0` or `[1]`, through [`spelling`].
fn rows_spelling(rows: &toml::Value) -> String {
    match rows {
        toml::Value::Integer(0) => "rect".to_string(),
        other => spelling(other),
    }
}

// ── The knob table ───────────────────────────────────────────────────────────

const STYLE: EnvKnob = EnvKnob::same(
    "TROLLSHELL_CORE_LEDS_STYLE",
    "style",
    "one of vfd/lcd/oled/crt",
);
const COLOR: EnvKnob = EnvKnob::same(
    "TROLLSHELL_CORE_LEDS_COLOR",
    "color",
    "one of style/rainbow/transpride/heat, or an #rrggbb literal",
);
const ROWS: EnvKnob = EnvKnob {
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
const FILL: EnvKnob = EnvKnob::same("TROLLSHELL_CORE_LEDS_FILL", "fill", "one of spare/blank");

// ── The file schema ──────────────────────────────────────────────────────────

/// `core-leds.toml`, as written.
///
/// # Every field is a raw [`toml::Value`], and that is the template rule
///
/// Not the parsed value, and not even a `String`: whatever type a schema field
/// has, **serde judges the value against it before any subsystem code runs, and
/// serde's verdict is whole-file**. `subsystem::assemble` maps a type mismatch
/// onto `ConfigError::Schema`, which discards every other key in the file with
/// it — the exact failure #1040 V1 was filed about, one layer further down.
///
/// A `String` field is *not* enough to escape that, which is what #1040 T1
/// measured: `style = 5` beside a perfectly good `color = "rainbow"` failed the
/// whole file with `invalid type: integer 5, expected a string`, and the panel
/// dropped to built-in defaults with a line that named no key at all.
/// `toml::Value` is the only field type that can hold every shape a TOML file
/// can put there, so it is the only one that leaves serde with nothing to
/// reject — after which [`Self::parsed`] is genuinely the single judge, and a
/// wrong *type* is a per-key rejection quoting the value exactly like a wrong
/// *value* (`a_wrong_typed_value_is_a_per_key_rejection_too`).
///
/// The residual whole-file cases are then exactly the ones where the file is
/// **not TOML**: an unterminated string, and an integer literal too large for
/// TOML's `i64` (`rows = 9223372036854775808`, a `ConfigError::Parse` from the
/// TOML lexer). Neither ever reaches serde, let alone this type; both are tested
/// (`a_file_that_is_not_toml_is_still_a_whole_file_error`).
///
/// So the rule a family-#2 author copies is one line — **type every schema field
/// `toml::Value` and judge it in `parsed()`** — and "raw" in that sentence means
/// `toml::Value`, not `String`. The cost is that the field type no longer
/// documents the key's shape; the documentation of a key's shape is
/// [`Self::DEFAULT_TOML`] and the [`EnvKnob`] vocabulary, both of which the user
/// actually reads, and neither of which a whole-file failure can be built out
/// of.
///
/// `#[serde(default)]` is on the **container**, so a key erased by an
/// `_unset = ["style"]` marker in the overlay falls back to this type's
/// documented default rather than to `toml::Value`'s own (which has none: it is
/// the container default that makes the field optional at all). There is
/// deliberately no `deny_unknown_fields`: merge rule 4 says an unknown key warns
/// and is ignored, and `subsystem::assemble` is what implements that.
// No `Eq`: a `toml::Value` can hold a float.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CoreLedsConfig {
    style: toml::Value,
    color: toml::Value,
    rows: toml::Value,
    fill: toml::Value,
}

impl Default for CoreLedsConfig {
    /// The documented default in Rust form.
    ///
    /// Pinned equal to [`Self::DEFAULT_TOML`]'s parse *and* to
    /// [`CoreLeds::default`] by `the_documented_default_is_the_built_in_one`, so
    /// this cannot drift from either. It exists as a Rust value because
    /// `#[serde(default)]` needs one and because the resolver needs a panic-free
    /// fallback for the (our-bug) case where the built-in TOML itself stops
    /// parsing.
    fn default() -> Self {
        Self {
            style: DisplayStyle::Vfd.name().into(),
            color: ColorMap::Heat.name().into(),
            rows: toml::Value::Integer(0),
            fill: "spare".into(),
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
    /// defaults (at startup) or keeps the last good file (on a reload). That is
    /// the right contract for a config whose keys are interdependent, and the
    /// wrong one for this pilot, whose four knobs are independent look-and-feel
    /// values: one typo would revert the other three (#1040 V1).
    ///
    /// So the judgement moves into [`Self::parsed`], which reports every bad key
    /// and defaults *that key* — and `Infallible` is then the honest `Error`,
    /// exactly as the trait's own doc suggests ("when there is nothing the type
    /// system did not already catch"). A family #2 whose keys really do
    /// constrain each other should use a real error here; one whose keys are
    /// independent should copy this. `Subsystem::validate`'s doc carries the
    /// worked example of the first shape (#1040 T4).
    type Error = std::convert::Infallible;

    type Resolved = CoreLeds;

    /// See [`Self::Error`]: every value is judged per key in [`Self::parsed`],
    /// so there is nothing left for the whole-file gate to reject.
    fn validate(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    /// The file's raw spellings as the resolved [`CoreLeds`], beside **every**
    /// key that did not parse.
    ///
    /// The one path from spelling to value: [`Self::resolve`] is this call with
    /// the environment layered on top, and nothing else may parse a
    /// `core-leds.toml` value.
    ///
    /// # A bad value costs its own key, and only its own key (#1040 V1)
    ///
    /// This returns a `CoreLeds` **and** a list, not a `Result`. It used to `?`
    /// on the first bad key and hand the error to [`Self::validate`], where
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
    fn parsed(&self) -> (CoreLeds, Vec<InvalidValue>) {
        let mut rejected = Vec::new();
        let fallback = CoreLeds::default();
        // The spelling each key is judged in. Every key is its raw value's
        // (`spelling`); `rows` is the pilot's one translation point, where the
        // TOML integer `0` becomes the variable's word `rect` (#1040 V4).
        let (style, color, rows, fill) = (
            spelling(&self.style),
            spelling(&self.color),
            rows_spelling(&self.rows),
            spelling(&self.fill),
        );
        // Every rejection quotes `self.<key>` — the raw `toml::Value` — rather
        // than the spelling the parser was handed: `rows = 0` is spelt `rect`
        // going in, and echoing `rect` back at a user who wrote something else
        // would name a value that is nowhere in their file. It is also what
        // makes a wrong *type* read like any other bad value (#1040 T1):
        // `style = 5` is reported as `style = 5`, not as a serde message about
        // an integer where a string was expected.
        let leds = CoreLeds {
            style: keep(
                parse_core_leds_style(&style).map_err(|_| InvalidValue::of(&STYLE, &self.style)),
                fallback.style,
                &mut rejected,
            ),
            color: keep(
                parse_core_leds_color(&color).map_err(|_| InvalidValue::of(&COLOR, &self.color)),
                fallback.color,
                &mut rejected,
            ),
            rows: keep(
                parse_core_leds_rows(&rows).map_err(|_| InvalidValue::of(&ROWS, &self.rows)),
                fallback.rows,
                &mut rejected,
            ),
            fill: keep(
                parse_core_leds_fill(&fill).map_err(|_| InvalidValue::of(&FILL, &self.fill)),
                fallback.fill,
                &mut rejected,
            ),
        };
        (leds, rejected)
    }

    /// The environment layered over `layered`, key by key — the merged file
    /// value is the fallback for every knob the environment does not carry.
    ///
    /// Four [`env::key`] calls, one per knob, which is the whole fan-out a
    /// family #2 writes: the four parsers return four different types, so a
    /// homogeneous table of `(var, key, parser)` triples cannot express them
    /// (see [`env`]'s module doc for the decision).
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
            style: env::key(
                Self::NAME,
                &STYLE,
                style.as_deref(),
                parse_core_leds_style,
                layered.style,
                announce,
            ),
            color: env::key(
                Self::NAME,
                &COLOR,
                color.as_deref(),
                parse_core_leds_color,
                layered.color,
                announce,
            ),
            rows: env::key(
                Self::NAME,
                &ROWS,
                rows.as_deref(),
                parse_core_leds_rows,
                layered.rows,
                announce,
            ),
            fill: env::key(
                Self::NAME,
                &FILL,
                fill.as_deref(),
                parse_core_leds_fill,
                layered.fill,
                announce,
            ),
        }
    }
}

// ── The service ──────────────────────────────────────────────────────────────

/// The handle the Stats panel subscribes to.
pub struct CoreLedsHandles {
    leds: Mutable<CoreLeds>,
}

/// The `core-leds.toml` service: one startup load, one announcing resolution,
/// and a supervised poller.
///
/// # Why it carries its three inputs instead of reaching for them
///
/// [`Service::start`] is what the process actually runs, and until #1040 V3 it
/// was the one function here with **zero** coverage — every test drove a
/// hand-rolled replica of it, so mutations that made `start` load twice, or skip
/// the announcing resolution entirely, left the suite green. Nobody would ever
/// have heard a deprecation line again and CI would not have noticed.
///
/// Holding `paths`, `lookup` and `on_battery` as fields means a test
/// constructs this over a scratch overlay and a fake environment and then
/// calls the **real** `start`. What is left unpinned is [`service`]'s three
/// argument expressions — deliberately, and that is as thin as this can get
/// without `set_var`: they are `xdg::config_layers`, `env::process_env` and
/// [`self::on_battery`], each covered on its own elsewhere.
pub struct CoreLedsService {
    /// Layer paths, lowest precedence first.
    paths: Vec<PathBuf>,
    /// How a deprecated variable is read.
    lookup: EnvLookup,
    /// How the poller reads the current battery state (#1041) — turned into a
    /// `CadenceSource` by [`battery_cadence_source`] in [`Self::start`], not
    /// stored as one directly, so this field stays the thing a test actually
    /// wants to inject (a battery flag, the same shape `lookup` is an
    /// environment).
    on_battery: BatterySource,
}

impl Service for CoreLedsService {
    type Handles = CoreLedsHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        // One stamp, one load, one announcing resolution — all inside `boot`,
        // in that order. Nothing here chooses a `Deprecations` or a stamp
        // ordering, so this call site cannot get either wrong.
        let (resolved, watcher) = watch::boot::<CoreLedsConfig>(&self.paths, &*self.lookup);
        let leds = Mutable::new(resolved);
        let Self {
            lookup, on_battery, ..
        } = self;
        let cadence = battery_cadence_source(on_battery);
        spawn_supervised("core-leds", {
            let leds = leds.clone();
            move || {
                watch::poll_loop(
                    leds.clone(),
                    watcher.clone(),
                    lookup.clone(),
                    cadence.clone(),
                )
            }
        });
        CoreLedsHandles { leds }
    }
}

pub fn service() -> CoreLedsService {
    CoreLedsService {
        paths: xdg::config_layers(CoreLedsConfig::NAME),
        lookup: std::sync::Arc::new(env::process_env),
        on_battery: std::sync::Arc::new(on_battery),
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
        BATTERY_CONFIG_POLL_INTERVAL, COLOR, CONFIG_POLL_INTERVAL, CoreLeds, CoreLedsConfig,
        CoreLedsService, FILL, MAX_ROWS, ROWS, STYLE, Service, battery_cadence_source, cadence,
        parse_core_leds_color, parse_core_leds_fill, parse_core_leds_rows, parse_core_leds_style,
        parse_hex_rgb, rows_spelling,
    };
    use hytte_config::subsystem::env::{self, Deprecations, EnvKnob};
    use hytte_config::subsystem::watch;
    use hytte_config::subsystem::{self, InvalidValue, Subsystem};
    use hytte_config::test_support::Overlay;
    use hytte_preem::{ColorMap, DisplayStyle, Fill};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    /// This subsystem's watcher — the generic `hytte_config` one at `S =
    /// CoreLedsConfig`. Aliased so every test below reads exactly as it did
    /// before #1044 hoisted the type out of this file.
    type Watcher = watch::Watcher<CoreLedsConfig>;

    /// [`watch::boot`] at this subsystem — stamp, load once, resolve
    /// announcing.
    fn boot(paths: &[PathBuf], lookup: &dyn Fn(&str) -> Option<String>) -> (CoreLeds, Watcher) {
        watch::boot::<CoreLedsConfig>(paths, lookup)
    }

    /// [`subsystem::initial_load`] at this subsystem.
    fn initial_load(paths: &[PathBuf]) -> CoreLeds {
        subsystem::initial_load::<CoreLedsConfig>(paths)
    }

    /// [`Subsystem::resolve`] at this subsystem — the environment layered over
    /// a merged file value.
    fn resolve(
        layered: CoreLeds,
        lookup: &dyn Fn(&str) -> Option<String>,
        announce: Deprecations,
    ) -> CoreLeds {
        CoreLedsConfig::resolve(layered, lookup, announce)
    }

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

    /// A table-valued key is a per-key rejection on the way **in** (#1040
    /// T1) — and on the way back **out**, today, it costs that key its
    /// documented comment block: `subsystem::patch` promotes a table `want`
    /// to a standard `[key]` table, which drops the key's prefix decor.
    /// Nothing saves a `CoreLedsConfig` yet; #888's forms will.
    ///
    /// **Pinned as it behaves, not as it should** — this is #1040 fix round
    /// 4's F2, and the real fix is `hytte_config`'s (#1044/#888): `patch`
    /// should write a table `want` over a non-table `have` as an inline
    /// value in place, preserving decor and position, the way it already
    /// renders a `toml::Value::Table` as an `InlineTable`
    /// (`subsystem.rs:681-687`). **Delete this test the day that lands** —
    /// it goes red the moment the comment count stops dropping.
    #[test]
    fn a_table_valued_key_loads_per_key_but_loses_its_comments_on_save_today() {
        let comments = |s: &str| s.lines().filter(|l| l.starts_with('#')).count();

        let loaded = subsystem::assemble::<CoreLedsConfig>(&[(
            PathBuf::from("/o.toml"),
            "color = { r = 255, g = 0, b = 0 }\nstyle = \"crt\"\n".into(),
        )])
        .expect("a table-valued key must not be a whole-file failure");
        assert_eq!(applied(&loaded.config).style, DisplayStyle::Crt);
        let saved = subsystem::render_overlay(CoreLedsConfig::DEFAULT_TOML, &loaded.config)
            .expect("renders");
        assert_eq!(
            (comments(CoreLedsConfig::DEFAULT_TOML), comments(&saved)),
            (36, 29),
            "a table-valued key costs its comment block on save, today"
        );

        // The control, so this cannot go green by the writer breaking outright.
        let scalar = subsystem::assemble::<CoreLedsConfig>(&[(
            PathBuf::from("/o.toml"),
            "color = 0xff0000\nstyle = \"crt\"\n".into(),
        )])
        .expect("loads");
        let kept = subsystem::render_overlay(CoreLedsConfig::DEFAULT_TOML, &scalar.config)
            .expect("renders");
        assert_eq!(
            comments(&kept),
            36,
            "a scalar of the wrong type keeps every comment"
        );
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
            [InvalidValue::written(&STYLE, "\"plasma\"")]
        );

        assert_eq!(parse_core_leds_color("puce"), Err("puce"));
        // `rgb` is `ColorMap::name`'s output but not an input: it carries no
        // components, so accepting it would mean inventing a colour.
        assert_eq!(parse_core_leds_color("rgb"), Err("rgb"));
        assert_eq!(
            rejections(&with(|c| c.color = "puce".into())),
            [InvalidValue::written(&COLOR, "\"puce\"")]
        );

        assert_eq!(parse_core_leds_rows("0"), Err("0"), "0 rows is a typo");
        assert_eq!(parse_core_leds_rows("-2"), Err("-2"));
        assert_eq!(parse_core_leds_rows("many"), Err("many"));
        assert_eq!(
            rejections(&with(|c| c.rows = toml::Value::Integer(-2))),
            [InvalidValue::written(&ROWS, "-2")],
            "a negative row count is rejected in the integer spelling too"
        );

        assert_eq!(parse_core_leds_fill("none"), Err("none"));
        assert_eq!(
            rejections(&with(|c| c.fill = "none".into())),
            [InvalidValue::written(&FILL, "\"none\"")]
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
            applied(&with(|c| c.rows = toml::Value::Integer(cap))).rows,
            Some(MAX_ROWS)
        );

        // The boundary + 1 is rejected, in both — with a *per-key* error that
        // names the key and the value, not a whole-file failure (#1040 F5).
        let over = MAX_ROWS + 1;
        assert_eq!(parse_core_leds_rows(&over.to_string()), Err("65"));
        assert_eq!(
            rejections(&with(|c| c.rows = toml::Value::Integer(cap + 1))),
            [InvalidValue::written(&ROWS, "65")]
        );

        // …and so is every shape of absurd, including the two that used to
        // take the shell down.
        for absurd in ["10000000", "9223372036854775807", "18446744073709551616"] {
            assert_eq!(parse_core_leds_rows(absurd), Err(absurd), "{absurd} rows");
        }
        assert_eq!(
            rejections(&with(|c| c.rows = toml::Value::Integer(i64::MAX))),
            [InvalidValue::written(&ROWS, "9223372036854775807")]
        );
    }

    /// The rejection line a bad file value produces, **as a literal**.
    ///
    /// Every other assertion about [`InvalidValue`] states the *rendering* as a
    /// literal but reads the vocabulary out of the [`Knob`]; only this one
    /// spells the whole sentence out. Without it the wording would be asserted
    /// against itself — the green-and-blind shape #1040 F4 caught in
    /// `deprecations()` — and nothing would see the vocabulary reworded out
    /// from under the sentence it has to read inside.
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
            only_rejection(&with(|c| c.rows = toml::Value::Integer(-2))).to_string(),
            "rows = -2 is not valid; expected 0 or \"rect\" for the \
             automatic rectangle, or a row count from 1 to 64",
            "…while an integer key's is bare"
        );
        assert_eq!(
            only_rejection(&with(|c| c.rows = toml::Value::String("many".into()))).to_string(),
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

        // The line the variable path produces is built from `env_accepts` …
        assert!(
            line.contains(ROWS.env_accepts),
            "the variable's line must be built from its own vocabulary: {line}"
        );
        // … and that vocabulary must not contain the `0` spelling anywhere.
        // A digit search rather than a substring one on purpose: `!contains(
        // "or 0")` passed the whole file vocabulary through unnoticed
        // (mutation V4a, green — it renders as "expected 0 or \"rect\" …",
        // which reads exactly as self-contradictory and contains no "or 0").
        // The variable's vocabulary is the one sentence here with no digit `0`
        // in it at all: "1 to 64" has none.
        assert!(
            !ROWS.env_accepts.contains('0'),
            "the vocabulary offered for a rejected `0` must not contain a `0`: {:?}",
            ROWS.env_accepts
        );
        // …while the file, which really does take it, says so — and the two
        // are genuinely different strings, not one shared by both paths.
        assert!(
            ROWS.file_accepts.contains("0 or \"rect\""),
            "the file vocabulary must teach the spelling the file accepts: {:?}",
            ROWS.file_accepts
        );
        assert_ne!(
            ROWS.env_accepts, ROWS.file_accepts,
            "this is the one knob whose two spellings differ; collapsing them is the bug"
        );
    }

    /// …and the **deprecation** line carries the *file* vocabulary, which is
    /// the other half of that split (#1040 T3).
    ///
    /// The test above covers the line an *unusable* variable produces. Nothing
    /// covered the line a **usable** one produces: swapping
    /// `knob.file_accepts` for `knob.env_accepts` at `env_key`'s announcing
    /// call site left the suite green at 585 (mutation R12), because no capture
    /// test set `TROLLSHELL_CORE_LEDS_ROWS` to a valid value — the other three
    /// knobs are [`Knob::same`], so for them the swap is a no-op.
    ///
    /// What it silently drops is the `0` spelling, from the one line on disk
    /// that teaches it: `DEFAULT_TOML` exists nowhere until nix renders a base
    /// file, so the deprecation line — which is *about* the file — is where a
    /// migrating user learns what the file takes. Measured, with
    /// `TROLLSHELL_CORE_LEDS_ROWS=rect`:
    ///
    /// ```text
    /// now:  … it accepts 0 or "rect" for the automatic rectangle, or a row count from 1 to 64
    /// R12:  … it accepts rect for the automatic rectangle, or a row count from 1 to 64
    /// ```
    ///
    /// **Red if the announcing call site takes the variable's vocabulary.**
    #[test]
    fn the_deprecation_line_teaches_the_file_spelling() {
        let (captured, _guard) = capture();
        let resolved = resolve(
            CoreLeds::default(),
            &env(&[("TROLLSHELL_CORE_LEDS_ROWS", "rect")]),
            Deprecations::Announce,
        );
        assert_eq!(resolved.rows, None, "live control: the variable was read");

        let line = warnings(&captured)
            .into_iter()
            .find(|w| w.starts_with("TROLLSHELL_CORE_LEDS_ROWS is deprecated"))
            .expect("the deprecation line for rows");

        // The `0` spelling, stated as the literal the file accepts — not
        // `ROWS.file_accepts`, which would assert the constant against itself
        // through the code path (the shape that made R12 green).
        assert!(line.contains("0 or \"rect\""), "{line}");
        assert!(
            line.ends_with(
                "— it accepts 0 or \"rect\" for the automatic rectangle, \
                 or a row count from 1 to 64"
            ),
            "the whole vocabulary, verbatim: {line}"
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
        assert_eq!(rows_spelling(&toml::Value::Integer(0)), "rect");
        assert_eq!(rows_spelling(&toml::Value::Integer(4)), "4");
        assert_eq!(rows_spelling(&toml::Value::Integer(-2)), "-2");
        assert_eq!(rows_spelling(&toml::Value::String("rect".into())), "rect");

        assert_eq!(
            applied(&with(|c| c.rows = toml::Value::Integer(0))).rows,
            None,
            "0 is the automatic rectangle"
        );
        assert_eq!(
            applied(&with(|c| c.rows = toml::Value::Integer(3))).rows,
            Some(3)
        );
    }

    /// **`rows = "rect"` works in the file** — the word the deprecated variable
    /// took, and the word the deprecation line walks a migrating user toward
    /// (#1040 F5).
    ///
    /// While `rows` was typed `i64`, it was a `ConfigError::Schema`: a
    /// **whole-file** failure that discarded every other key in the file and
    /// dropped the panel to built-in defaults over the most likely migration
    /// typo there is. `rows = "4"` follows for free — a TOML string is handed
    /// straight to the same single judge — and a word the judge does not know
    /// comes back as a named per-key error rather than a serde message.
    ///
    /// **Red if `rows` stops being a raw `toml::Value`**, or if [`spelling`]
    /// stops handing a string's own contents through untouched.
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
            applied(&with(|c| c.rows = toml::Value::String("4".into()))).rows,
            Some(4),
            "the string arm goes through the same single judge"
        );
        assert_eq!(
            rejections(&with(|c| c.rows = toml::Value::String("many".into()))),
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
        let mut overlay = overlay();
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
    /// It was the first residual whole-file case to be closed: `serde` decided
    /// `rows = true` inside its own deserializer, before any subsystem code
    /// ran, so a `ConfigError::Schema` took the file with it. Typing the field
    /// `toml::Value` gives the judgement back to [`CoreLedsConfig::parsed`],
    /// where it belongs — including `rows = 4.0`, the plausible float typo,
    /// which the old `i64` field's doc did not even list. The sibling test
    /// below does the same for the other three keys (#1040 T1).
    ///
    /// **Red if `rows` narrows back to a concrete type**: every case below
    /// goes back to being a `ConfigError::Schema` and `assemble` returns
    /// `Err`.
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

    /// **A wrong TOML *type* on any key costs that key and nothing else**
    /// (#1040 T1) — the same contract a wrong *value* has had since V1, and
    /// the last thing standing between the tree's claims and the code.
    ///
    /// Measured on the previous head, through the production `initial_load`:
    /// `style = 5` beside a perfectly good `color = "rainbow"` was
    /// `Schema("invalid type: integer 5, expected a string in `style`")` — a
    /// **whole-file** failure. The panel dropped to stock VFD/heat with the
    /// good `color` silently gone, and the single journal line was `config
    /// unusable; falling back to the built-in default`, naming no key at all.
    /// That is exactly the V1 experience, and five statements in the tree (three
    /// doc comments, the PR body, a `live-verify.md` bullet a human is asked to
    /// check on glass) said the residual whole-file class was "a file that is
    /// not TOML at all". This test is what makes them true.
    ///
    /// A `String` field was not enough, which is the template point worth
    /// copying: a `String` *is* the raw spelling, and serde still rejects every
    /// non-string value against it. `toml::Value` is the field type to reach
    /// for **not** because a hand-written `#[serde(untagged)]` catch-all
    /// cannot be made to accept every shape — measured (#1040 fix round 4
    /// F3), it can, datetime included, once it goes through the same
    /// `IntoDeserializer` round-trip `assemble` uses — but because
    /// `toml::Value` is strictly more general and leaves serde nothing to
    /// judge: one type, not an enum whose variant order some future TOML
    /// shape could still pick wrong.
    ///
    /// The `color = 0xff0000` row is the realistic one: a hex colour *is* a
    /// number, and TOML takes `0x…` as an integer. The `color = 1979-05-27`
    /// row is the one to read carefully — see
    /// `a_toml_date_arrives_as_a_string_not_a_datetime` for why it applies as
    /// the *string* `"1979-05-27"`, not a `toml::Value::Datetime`.
    ///
    /// **Red if any schema field narrows to a concrete type** — `style: String`
    /// alone turns the first three rows back into whole-file failures.
    #[test]
    fn a_wrong_typed_value_is_a_per_key_rejection_too() {
        let rainbow = CoreLeds {
            color: ColorMap::Rainbow,
            ..CoreLeds::default()
        };
        let crt = CoreLeds {
            style: DisplayStyle::Crt,
            ..CoreLeds::default()
        };
        for (body, applies, rejected) in [
            (
                "style = 5\ncolor = \"rainbow\"\n",
                rainbow,
                InvalidValue::written(&STYLE, "5"),
            ),
            (
                "style = true\ncolor = \"rainbow\"\n",
                rainbow,
                InvalidValue::written(&STYLE, "true"),
            ),
            (
                "style = [1]\ncolor = \"rainbow\"\n",
                rainbow,
                InvalidValue::written(&STYLE, "[1]"),
            ),
            (
                "fill = 1.5\ncolor = \"rainbow\"\n",
                rainbow,
                InvalidValue::written(&FILL, "1.5"),
            ),
            (
                "color = 0xff0000\nstyle = \"crt\"\n",
                crt,
                // TOML's own rendering of what the file holds: the lexer
                // takes `0xff0000` as the integer 16711680, and `Display` for
                // a `toml::Value` is canonical TOML, not the source bytes.
                InvalidValue::written(&COLOR, "16711680"),
            ),
            (
                "color = 1979-05-27\nstyle = \"crt\"\n",
                crt,
                // Not `Display` quoting a date (`toml::Value::Datetime`'s own
                // `Display` is unquoted, measured) — it is `assemble`'s
                // `IntoDeserializer` round-trip that erases the date to a
                // `String` before `parsed()` ever runs, so the field holds
                // the *string* `"1979-05-27"` by the time this constructor
                // sees it (#1040 fix round 4 F3). Pinned as it behaves rather
                // than wished into shape:
                // `a_toml_date_arrives_as_a_string_not_a_datetime` names the
                // mechanism, and the reader still sees the value they typed.
                InvalidValue::written(&COLOR, "\"1979-05-27\""),
            ),
        ] {
            let loaded =
                subsystem::assemble::<CoreLedsConfig>(&[(PathBuf::from("/o.toml"), body.into())])
                    .unwrap_or_else(|e| {
                        panic!("{body:?} must not be a whole-file failure, got {e}")
                    });

            assert_eq!(
                applied(&loaded.config),
                applies,
                "{body:?}: the key beside it applies, and only the bad key takes \
                 the built-in default"
            );
            assert_eq!(
                only_rejection(&loaded.config),
                rejected,
                "{body:?}: named, and quoted as the TOML the file holds — not a \
                 serde message about a type"
            );
        }
    }

    /// A TOML **date** does not reach a `toml::Value` field as a date
    /// (#1040 fix round 4 F3): `assemble`'s `IntoDeserializer` round-trip
    /// erases it to a `String` before [`CoreLedsConfig::parsed`] ever runs.
    /// `toml::Value::Datetime`'s own `Display` is *unquoted* — measured below
    /// — so the quoting in `a_wrong_typed_value_is_a_per_key_rejection_too`'s
    /// date row is the string's, not `Display`'s.
    ///
    /// Inert here — nothing in this schema takes a date — but a family #2
    /// with a genuine date key inherits the erasure, including a save that
    /// writes `key = "2026-01-01"` where the file said `key = 2026-01-01`,
    /// until #1044 fixes it.
    ///
    /// **Red if the erasure is ever closed** (e.g. `assemble` switches to a
    /// deserializer that preserves `toml::Value::Datetime` through the merge)
    /// — which is the day this test, and the doc sentences it backs, should
    /// be deleted rather than "fixed".
    #[test]
    fn a_toml_date_arrives_as_a_string_not_a_datetime() {
        let raw: toml::Table = "color = 1979-05-27\n".parse().expect("parses");
        assert!(
            matches!(raw.get("color"), Some(toml::Value::Datetime(_))),
            "the file holds a date"
        );
        assert_eq!(
            toml::Value::Datetime("1979-05-27".parse().expect("valid date")).to_string(),
            "1979-05-27",
            "toml::Value::Datetime's own Display is unquoted"
        );

        let loaded = subsystem::assemble::<CoreLedsConfig>(&[(
            PathBuf::from("/o.toml"),
            "color = 1979-05-27\nstyle = \"crt\"\n".into(),
        )])
        .expect("a date must not be a whole-file failure");
        assert_eq!(
            only_rejection(&loaded.config),
            InvalidValue::written(&COLOR, "\"1979-05-27\""),
            "the erasure happened before this constructor ever ran"
        );
        assert_eq!(
            applied(&loaded.config).style,
            DisplayStyle::Crt,
            "live control: the key beside it survived"
        );
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

    /// `_unset = ["style"]` is advertised at `DEFAULT_TOML`'s own top comment
    /// and by #868's merge rule 1 erases whatever a lower layer set — never
    /// exercised by a test on this schema until now (#1040 fix round 4 nit).
    ///
    /// Three shapes: alone, beside a sibling key that *is* set in the same
    /// layer, and over a base layer that set the erased key — in every case
    /// the key falls to `CoreLedsConfig`'s own `Default`, not to
    /// `toml::Value`'s (which has none), and with **zero** rejections and
    /// **zero** unknown keys, since `_unset` is a merge-time marker stripped
    /// before the schema ever sees the table.
    #[test]
    fn unset_erases_the_inherited_key_and_falls_to_the_built_in_default() {
        let alone = subsystem::assemble::<CoreLedsConfig>(&[(
            PathBuf::from("/o.toml"),
            "_unset = [\"style\"]\n".into(),
        )])
        .expect("assembles");
        assert_eq!(applied(&alone.config).style, DisplayStyle::Vfd);
        assert_eq!(applied(&alone.config).color, ColorMap::Heat);
        assert!(alone.unknown_keys.is_empty());
        assert!(rejections(&alone.config).is_empty());

        let beside = subsystem::assemble::<CoreLedsConfig>(&[(
            PathBuf::from("/o.toml"),
            "_unset = [\"style\"]\ncolor = \"rainbow\"\n".into(),
        )])
        .expect("assembles");
        assert_eq!(applied(&beside.config).style, DisplayStyle::Vfd);
        assert_eq!(applied(&beside.config).color, ColorMap::Rainbow);
        assert!(beside.unknown_keys.is_empty());
        assert!(rejections(&beside.config).is_empty());

        let over_base = subsystem::assemble::<CoreLedsConfig>(&[
            (PathBuf::from("/base.toml"), "style = \"crt\"\n".into()),
            (
                PathBuf::from("/overlay.toml"),
                "_unset = [\"style\"]\n".into(),
            ),
        ])
        .expect("assembles");
        assert_eq!(
            applied(&over_base.config).style,
            DisplayStyle::Vfd,
            "erases the base layer's value too, not just DEFAULT_TOML's"
        );
        assert!(over_base.unknown_keys.is_empty());
        assert!(rejections(&over_base.config).is_empty());
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
    // The capture harness is `hytte_config::test_support` since #1044. It was a
    // third verbatim copy of the same 100 lines — and had already drifted from
    // `hytte-config`'s, which kept the structured fields this one threw away
    // (#1040 V10). Its `capture()` installs the process-wide global default
    // (#1043's shape) with this file's keepalive `Dispatch` as the
    // never-panicking fallback, so the mechanism here is strictly stronger than
    // what it replaces — see that module's doc for the measurement.
    use hytte_config::test_support::{Captured, capture};

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
    fn announced(knob: &EnvKnob) -> String {
        env::deprecation_message(
            knob.var,
            knob.key,
            &env::overlay_display(CoreLedsConfig::NAME),
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
            *watcher.last_good(),
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
    ///
    /// The harness itself is `hytte_config::test_support::Overlay` since #1044:
    /// the tempdir + explicit-mtime + `write_in_the_same_granule` shape is the
    /// second-largest generic test asset the pilot grew, and family #2 should
    /// not rewrite it. This wrapper only supplies the file stem.
    fn overlay() -> Overlay {
        Overlay::new(CoreLedsConfig::NAME)
    }

    /// The watcher [`boot`] hands back for `paths`, with no environment.
    ///
    /// The production function itself since #1040 V3 — it used to be a
    /// hand-rolled replica of it, which is exactly why mutations to the real
    /// one survived.
    fn watching(paths: &[PathBuf]) -> Watcher {
        boot(paths, &no_env()).1
    }

    /// A [`CoreLedsService`] over a scratch overlay, a fake environment and an
    /// injected battery flag — so [`Service::start`], the function the process
    /// actually runs, is the thing under test rather than a copy of it
    /// (#1040 V3, battery flag added #1041).
    fn service_over(
        paths: Vec<PathBuf>,
        lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
        on_battery: impl Fn() -> bool + Send + Sync + 'static,
    ) -> CoreLedsService {
        CoreLedsService {
            paths,
            lookup: std::sync::Arc::new(lookup),
            on_battery: std::sync::Arc::new(on_battery),
        }
    }

    /// The payoff: an edit while the shell runs re-resolves without a restart.
    ///
    /// **Red if `Watcher::poll` stops re-reading** (return `None` before the
    /// load, or drop the `self.last_good = config` assignment).
    #[test]
    fn a_changed_file_is_picked_up() {
        let mut overlay = overlay();
        overlay.write("style = \"lcd\"\n");
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&no_env(), Deprecations::Silent);
        assert_eq!(current.style, DisplayStyle::Lcd);

        // Unmoved mtime → nothing to do.
        assert_eq!(watcher.poll(&current, &no_env()), None);

        overlay.write("style = \"crt\"\ncolor = \"rainbow\"\n");
        let next = watcher.poll(&current, &no_env()).expect("changed → reload");

        assert_eq!(next.style, DisplayStyle::Crt);
        assert_eq!(next.color, ColorMap::Rainbow);
        // A touch that changes nothing must not churn the signal.
        overlay.write("style = \"crt\"\ncolor = \"rainbow\"\n");
        assert_eq!(watcher.poll(&next, &no_env()), None);
    }

    /// A file that appears *after* startup is a change too — the overlay is
    /// absent on a fresh install, so `None → Some(mtime)` has to count.
    #[test]
    fn a_newly_created_file_is_picked_up() {
        let mut overlay = overlay();
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&no_env(), Deprecations::Silent);
        assert_eq!(
            current,
            CoreLeds::default(),
            "a missing file is the default"
        );

        overlay.write("fill = \"blank\"\n");
        let next = watcher.poll(&current, &no_env()).expect("created → reload");

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
        let mut overlay = overlay();
        overlay.write("style = \"crt\"\n");
        let mut watcher = watching(&overlay.layers());
        let good = watcher.resolved(&no_env(), Deprecations::Silent);
        assert_eq!(good.style, DisplayStyle::Crt);

        // Not TOML at all.
        overlay.write("style = \"crt\n");
        assert_eq!(
            watcher.poll(&good, &no_env()),
            None,
            "a parse error must publish nothing"
        );
        assert_eq!(watcher.resolved(&no_env(), Deprecations::Silent), good);

        // …and a repaired file is picked up again.
        overlay.write("style = \"oled\"\n");
        let next = watcher.poll(&good, &no_env()).expect("repaired → reload");
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
        let mut overlay = overlay();
        overlay.write("style = \"crt\"\n");
        let mut watcher = watching(&overlay.layers());
        let good = watcher.resolved(&no_env(), Deprecations::Silent);

        overlay.write("style = \"crt\"\ncolor = \"rainbow\"\nrows = \"many\"\n");
        let next = watcher
            .poll(&good, &no_env())
            .expect("the good keys in the save must reach the panel");

        assert_eq!(next.color, ColorMap::Rainbow, "the key beside the typo");
        assert_eq!(next.style, DisplayStyle::Crt, "…and the one that was fine");
        assert_eq!(
            next.rows,
            CoreLeds::default().rows,
            "…while the typo alone falls back to the built-in default"
        );
    }

    /// **One warning per save, not one per tick** (#1040 T2) — for the file
    /// that is not TOML.
    ///
    /// The mechanism is `Watcher::poll`'s unconditional `self.stamps = now;`:
    /// a layer whose stamp has been taken is not re-read until it moves again,
    /// so a file left malformed is read *once*. Deleting that one line left the
    /// whole suite green at 585 passed — the `(next != current)` dedup hides
    /// the republish, so nothing observable moves and only the journal does.
    /// Measured under R8: four polls, four warnings.
    ///
    /// In production that is a line every 3 s for the life of the shell, and
    /// it contradicts two of this PR's own promises in as many words —
    /// [`Watcher::poll`]'s doc ("once per edit rather than once per tick") and
    /// `live-verify.md`'s "the journal gets one warning per save (not one per
    /// poll)". It is also the single most-copied line in the file: `Watcher` is
    /// the generic poller nine subsystems inherit, and V7 was fixed precisely
    /// to stop a line repeating every three seconds.
    ///
    /// **Red if `poll` stops updating its stamps** (`left: 4, right: 1`).
    #[test]
    fn a_malformed_file_warns_once_per_save_not_once_per_tick() {
        let mut overlay = overlay();
        overlay.write("style = \"lcd\"\n");
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&no_env(), Deprecations::Silent);

        // Not TOML at all: the `Err` arm of `poll`, which warns and keeps the
        // last good file.
        overlay.write("style = \"cr\n");
        let (captured, _guard) = capture();
        for _ in 0..4 {
            let _ = watcher.poll(&current, &no_env());
        }

        assert_eq!(
            warnings(&captured).len(),
            1,
            "one line for the save, not one per poll: {:?}",
            warnings(&captured)
        );
    }

    /// The same, for the file that **is** TOML with one unusable value — the
    /// per-key line #1040 V1 added (#1040 T2).
    ///
    /// A separate test because it travels a different arm of `poll`: this one
    /// *succeeds*, updating `last_good` and republishing, and its warning comes
    /// out of `load_layer` rather than out of `poll`'s `Err`. Both are gated by
    /// the same stamp update, and a fix that only covered the malformed case
    /// would leave the more likely one — a finished file with one typo in it —
    /// talking forever.
    ///
    /// **Red if `poll` stops updating its stamps** (`left: 4, right: 1`).
    #[test]
    fn a_rejected_value_warns_once_per_save_not_once_per_tick() {
        let mut overlay = overlay();
        overlay.write("style = \"lcd\"\n");
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&no_env(), Deprecations::Silent);

        overlay.write("style = \"crt\"\nrows = \"many\"\n");
        let (captured, _guard) = capture();
        for _ in 0..4 {
            let _ = watcher.poll(&current, &no_env());
        }

        assert_eq!(
            warnings(&captured).len(),
            1,
            "one line for the save, not one per poll: {:?}",
            warnings(&captured)
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
        let mut overlay = overlay();
        overlay.write("style = \"vfd\"\ncolor = \"heat\"\n");
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&pinned, Deprecations::Silent);
        assert_eq!(current.style, DisplayStyle::Crt);

        overlay.write("style = \"lcd\"\ncolor = \"rainbow\"\n");
        let next = watcher.poll(&current, &pinned).expect("changed → reload");

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
        let mut overlay = overlay();
        overlay.write("color = \"heat\"\n");
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&pinned, Deprecations::Announce);

        let (captured, _guard) = capture();
        overlay.write("color = \"rainbow\"\n");
        let next = watcher.poll(&current, &pinned).expect("changed → reload");

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
        let mut overlay = overlay();
        overlay.write("color = \"heat\"\n");
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&broken, Deprecations::Announce);

        let (captured, _guard) = capture();
        overlay.write("color = \"rainbow\"\n");
        let next = watcher.poll(&current, &broken).expect("changed → reload");

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
        let mut overlay = overlay();
        overlay.write("colour = \"rainbow\"\nstyle = \"crt\"\n");

        let (captured, _guard) = capture();
        let (_resolved, watcher) = boot(&overlay.layers(), &no_env());

        assert_eq!(
            warnings(&captured),
            ["unknown key in config; ignoring it"],
            "one unknown key, one warning — live-verify.md promises exactly this"
        );
        assert_eq!(
            watcher.last_good().style,
            DisplayStyle::Crt,
            "live control: the load under observation actually happened"
        );
    }

    /// The same, for the error half: a file nothing can parse says so **once**.
    #[test]
    fn a_startup_reports_an_unusable_file_exactly_once() {
        let mut overlay = overlay();
        overlay.write("style = \"crt\n");

        let (captured, _guard) = capture();
        let (_resolved, watcher) = boot(&overlay.layers(), &no_env());

        assert_eq!(
            errors(&captured),
            ["config unusable; falling back to the built-in default"],
            "one broken file, one error"
        );
        assert_eq!(
            *watcher.last_good(),
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
        let mut overlay = overlay();
        overlay.write("colour = \"rainbow\"\nstyle = \"crt\"\n");
        // AC (`|| false`) → CONFIG_POLL_INTERVAL (3 s), already far longer
        // than the synchronous assertions below need — no separate "never"
        // escape hatch required now that the wait is battery-driven rather
        // than a bare `interval` field (#1041).
        let service = service_over(
            overlay.layers(),
            env(&[("TROLLSHELL_CORE_LEDS_FILL", "blank")]),
            || false,
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
        let mut overlay = overlay();
        overlay.write("style = \"crt\"\ncolor = \"rainbow\"\n");
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&no_env(), Deprecations::Silent);
        assert_eq!(current.style, DisplayStyle::Crt);

        overlay.delete();
        let next = watcher.poll(&current, &no_env()).expect("deleted → reload");

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
        let mut overlay = overlay();
        overlay.write("style = \"lcd\"\n");
        let mut watcher = watching(&overlay.layers());

        assert_eq!(
            watcher.poll(&CoreLeds::default(), &no_env()),
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
        let mut overlay = overlay();
        overlay.write("style = \"lcd\"\n");
        let mut watcher = watching(&overlay.layers());
        let current = watcher.resolved(&no_env(), Deprecations::Silent);
        assert_eq!(current.style, DisplayStyle::Lcd);

        // Same mtime to the nanosecond, different bytes — and a different
        // byte count, which is the half the length in the stamp can see.
        overlay.write_in_the_same_granule("style = \"crt\"\ncolor = \"rainbow\"\n");
        let next = watcher
            .poll(&current, &no_env())
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
        let overlay = std::cell::RefCell::new(overlay());
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
            watcher.poll(&published, &no_env()).map(|leds| leds.style),
            Some(DisplayStyle::Crt),
            "the save must reach the panel on the next poll, not never"
        );
    }

    // `the_watch_loop_republishes_the_watcher_it_was_given` (driving
    // `watch::poll_loop` for real, at a fast injected cadence) moved to
    // `hytte_config::subsystem::watch`'s own tests
    // (`a_fast_cadence_source_drives_the_loop`) when #1081's cadence work
    // landed on top of #1044's hoist: `poll_loop` is generic mechanism now,
    // not this subsystem's own function, so it is tested where it lives. This
    // subsystem's own coverage is the battery *mapping* — see
    // `cadence_is_config_poll_interval_on_ac` and its neighbours further down.

    /// **Two layers really are watched** — an edit to the *base* reloads live
    /// too, not just an edit to the overlay (#1040 V8).
    ///
    /// Every other watcher test uses `Overlay::layers`, which is one path, so
    /// nothing pinned that `Watcher` stats each layer rather than the last one
    /// (or the first). It also settles what `live-verify.md` should say: the
    /// base layer needs no restart either.
    #[test]
    fn an_edit_to_the_base_layer_reloads_too() {
        let mut base = overlay();
        let mut overlay = overlay();
        base.write("style = \"crt\"\nfill = \"blank\"\n");
        overlay.write("style = \"lcd\"\n");
        let layers = vec![base.path().to_path_buf(), overlay.path().to_path_buf()];

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
            .poll(&current, &no_env())
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
            Some(env::overlay_display(CoreLedsConfig::NAME).as_str())
        );
    }

    /// The poll cadence is `places.toml`'s, and short enough that an edit
    /// feels live rather than eventual.
    #[test]
    fn the_poll_interval_is_a_few_seconds() {
        assert_eq!(watch::POLL_INTERVAL, Duration::from_secs(3));
    }

    // ── Battery-aware cadence (#1041/#1081) ─────────────────────────────────
    //
    // `watch::poll_loop`'s own loop mechanics (the `CadenceSource`/
    // `wait_cadence` recheck stepping) are tested generically in
    // `hytte_config::subsystem::watch`. What is left here is this subsystem's
    // own half: mapping "on battery or not" to a `Duration`, and wiring that
    // mapping into the `CadenceSource` `Service::start` hands `poll_loop`.

    #[test]
    fn cadence_is_config_poll_interval_on_ac() {
        assert_eq!(cadence(false), CONFIG_POLL_INTERVAL);
    }

    #[test]
    fn cadence_stretches_on_battery() {
        assert_eq!(cadence(true), BATTERY_CONFIG_POLL_INTERVAL);
        assert!(BATTERY_CONFIG_POLL_INTERVAL > CONFIG_POLL_INTERVAL);
    }

    /// The documented number, pinned as a literal — `cadence_stretches_on_battery`'s
    /// `>` comparison alone would stay green against a mutation that widened
    /// the constant to something still bigger than 3 s, and
    /// `docs/live-verify.md` tells a human to expect **15 s**, not merely
    /// "longer than 3 s".
    #[test]
    fn the_battery_poll_interval_is_fifteen_seconds() {
        assert_eq!(BATTERY_CONFIG_POLL_INTERVAL, Duration::from_secs(15));
    }

    /// **The watcher's cadence signal switches with the battery signal**,
    /// live — not once at construction.
    ///
    /// `battery_cadence_source` is what turns [`CoreLedsService`]'s injected
    /// `on_battery` field into the `CadenceSource` `watch::poll_loop`'s loop
    /// actually polls, and the whole point is that it re-reads the flag on
    /// every call rather than snapshotting it when the closure is built. This
    /// drives the same `AtomicBool` a real
    /// `hytte::services::upower::on_battery_now` caller would see change under
    /// it mid-session, and checks both readings through the one source
    /// [`Service::start`] hands `poll_loop`.
    ///
    /// **Red if the signal is ignored** — a mutation that captured
    /// `on_battery()`'s value once outside the returned closure would pass
    /// the first assertion and fail the second, since flipping the flag
    /// afterwards would have no effect.
    #[test]
    fn the_cadence_source_follows_an_injected_battery_flag() {
        let on_battery = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let source = battery_cadence_source({
            let on_battery = on_battery.clone();
            std::sync::Arc::new(move || on_battery.load(std::sync::atomic::Ordering::Relaxed))
        });

        assert_eq!(source(), CONFIG_POLL_INTERVAL, "AC to start");
        on_battery.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            source(),
            BATTERY_CONFIG_POLL_INTERVAL,
            "…and the source re-reads the flag live, not once"
        );
    }

    // ── helpers ─────────────────────────────────────────────────────────────

    fn with(f: impl FnOnce(&mut CoreLedsConfig)) -> CoreLedsConfig {
        let mut config = CoreLedsConfig::default();
        f(&mut config);
        config
    }

    fn with_style(style: &str) -> CoreLedsConfig {
        with(|c| c.style = style.into())
    }
}
