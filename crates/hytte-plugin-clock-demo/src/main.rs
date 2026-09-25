//! `hytte-plugin-clock-demo` — the reference out-of-process widget plugin for
//! trollshell's "frontend B" plugin architecture (issue #35; on the #266 wire
//! protocol, the #272 host transport, and the #275 `hytte-plugin` runtime) —
//! and, since #1388, the **two-instance demo** as well.
//!
//! # First: the reference plugin
//!
//! It is the **end-to-end proof** that a plugin can live outside the shell,
//! link **no GTK** (only [`hytte_plugin`] — not even tokio directly), and
//! drive a real widget over a Unix socket. Left where its manifest puts it, it
//! renders a clock card into the shell's [`Mount::SidebarTop`] slot and, when
//! that card is clicked, asks the host to open the plugin's own page —
//! exercising the render path, the state-subscription path, and the
//! event→effect round-trip in one demo.
//!
//! It is still the reference for the **TEA shape** a new plugin author reads
//! first: a manifest, `init`, `update`, `view` and a one-line `main`, and
//! nothing else. What changed with #1408 is the card: it used to be the
//! plainest surface in the tree (a timestamp label over a "Power menu" button)
//! and it is now a preem clock, so the reference also shows the **display
//! seam** — a stateful widget and two pure ones side by side, one code path
//! for both hosts. See *The sidebar card* below.
//!
//! # Second: one binary, two surfaces
//!
//! The same binary also renders a **bar chip**: a compact `HH:MM`
//! seven-segment readout. Which of the two a process draws is decided by the
//! **family of its effective mount** — [`Mount::is_bar`] over
//! [`hytte_plugin::effective_mount`] (#1317), i.e. by the `HYTTE_PLUGIN_MOUNT`
//! the launch set (#1159) and nothing else. There is no flag, no env knob of
//! this crate's own, and no second config file.
//!
//! Both surfaces open the **same page** on a click
//! (`Effect::OpenPage(Page::PluginSelf)`); where it opens is the host's call,
//! made on the producing plugin's mount (#1010) — the drawer for the chip, the
//! centered dialog for the card. The page is one builder both arms publish.
//!
//! Running both at once is therefore a **deployment** shape, not a packaging
//! one: two `programs.trollshell.plugins.<id>` entries pointing at this one
//! package, the second with `mount = "BarCenter"`. The attribute name is the
//! launch id, and the module renders `HYTTE_PLUGIN_ID` for it automatically
//! (#1250/#1284), because the host allows exactly one live connection per
//! plugin id.
//!
//! Until #1388 those two surfaces were two crates — `hytte-plugin-clock-demo`
//! and `hytte-plugin-bar-clock-demo` — which is the same plugin twice over
//! (Annika on #1163: "since the plugins can be launched multiple times it makes
//! no sense to have a bar-clock-demo and a clock-demo"). `hytte-plugin-stats`
//! (#1250/#1251) and `hytte-claude-bridge` (#1315) are the same split; this is
//! the smallest possible statement of it.
//!
//! # Shape — The Elm Architecture, and nothing else
//!
//! Everything below is the pure TEA core: a model ([`ClockDemo`]) plus
//! [`update`](Plugin::update) / [`view`](Plugin::view). All transport —
//! dialing `$XDG_RUNTIME_DIR/trollshell/plugin.sock` with bounded backoff,
//! the `Register` handshake, liveness, render dedup, reconnection — lives in
//! the [`hytte_plugin`] runtime behind the one-line `main`. systemd's
//! `Restart=on-failure` is the outer supervisor for genuine process failures.
//!
//! The manifest is **per binary**, not per instance, so it declares the union
//! of what the two surfaces need: one mount (the sidebar default a launch may
//! override), one subscription (`Clock`), and one capability
//! ([`Capability::OpenPage`], which both arms use, each for this plugin's own
//! page).
//!
//! `update`, `view`, the `HH:MM`, date and seconds projections **and the split
//! itself** are unit-tested below — that is the demo's main correctness
//! signal, since the live host isn't reachable here; the session loop itself
//! is covered by `hytte-plugin`'s own tests.
//!
//! # The sidebar card: a preem clock (#1408)
//!
//! Annika's ask on #1163 was "a real sidebar or bar (depending where mounted)
//! plugin for fancy preem clock". #1388 built the *depending where mounted*
//! half; the card is the other half. It is one [`Node::Button`] — the whole
//! card is the click target, opening the page — holding three rows, every one
//! of them a [`hytte_plugin::display`] widget:
//!
//! - **`HH:MM` on a split-flap board** ([`FlipBoard`], [`Mechanism::SplitFlap`]).
//!   Since #1155 the shell draws it on the GPU, so the minute flip animates on
//!   glass: the plugin states the new face once and the shell runs the fold on
//!   its own frame clock.
//! - **A dot-matrix date line** ([`DotMatrix`]), `FRI 25 SEP`: weekday, day of
//!   the month, month.
//! - **A seconds sweep** ([`LedStrip`]), filling over the minute.
//!
//! All three come from the one host `Clock` state, like the chip — this
//! process never reads the system clock. The date is the ISO timestamp's
//! **local** date, never a projection of `unix`: `unix` is UTC, so for the
//! first two hours of every day at `+02:00` it still names yesterday.
//!
//! The "Power menu" button the card used to carry is gone: the power menu has
//! its own chip and keybind, and a clock card is not where anyone looks for it.
//!
//! ## Why there is a seconds sweep: the host pushes `Clock` every second
//!
//! A seconds readout is only honest if a second's worth of change reaches the
//! plugin, so this was measured rather than assumed. `hytte-services`' clock
//! service re-reads the wall clock on a 1 s `glib` timeout; the shell's plugin
//! host publishes every tick of it on a watch channel (`plugins/mod.rs`'s clock
//! pump → `pump::set_clock`), and each session's `snapshot_task` pushes a
//! `StateSnapshot` on every change of that channel. So a snapshot arrives about
//! once a second, carrying seconds in its timestamp. Had it been once a minute
//! there would be no sweep: a "seconds" readout that moved once a minute would
//! be a lie.
//!
//! The timeout is not aligned to the wall-clock second and drifts by its own
//! dispatch latency, so now and then a second is read twice or skipped. At
//! [`SWEEP_SECS_PER_LED`] seconds a segment that never shows.
//!
//! [`SWEEP_LEDS`] is 20 because it is the finest **even** division of the
//! minute whose natural width still fits the card: a strip is `11n + 5` px
//! wide, so 60 segments would be 665 px and 30 would be 335 px against the
//! card's ~296. The count is **1-based** ([`sweep_lit`]): the segment the
//! second hand is in is lit, and every one before it, so the strip is full for
//! the last three seconds and drops back to one as the minute flips.
//!
//! ## One skin, and the board in raster mode
//!
//! Every widget on the card wears [`CARD_SKIN`] (VFD), the skin the chip's
//! readout wears, so the two instances of this plugin read as one device family
//! when both are on screen.
//!
//! The board is re-stated on every snapshot and then **settled**. `settle` only
//! lands the plugin-side cards, which exist for the raster arm alone (the
//! shell owns the flip in state mode), and this plugin has no frame timer to
//! animate them on — so against a shell that does not speak preem the new
//! minute appears at once instead of sticking on the old card, with no branch
//! on which host is on the other end.
//!
//! ## Sizing
//!
//! A sidebar card's preem widgets are **height-for-width** (#1387): each one
//! stretches to the card's width and takes the height its aspect ratio gives
//! it. So a widget's natural size sets only its resolution and its proportions
//! — and it is kept inside the ~296 px card anyway (the #313 lesson), because a
//! natural width past the card widens the sidebar. At these metrics the board
//! is 246×66 px, the date line 244×36 and the sweep 225×24.
//!
//! # The bar chip is also the bar-side showcase for the preem display seam
//!
//! The chip's `HH:MM` is a [`hytte_plugin::display::SevenSeg`] readout rather
//! than a `Node::Label`, which makes this the bar-mounted half of the #884
//! acceptance pair (`hytte-plugin-preem-demo` is the sidebar-card half). Same
//! one code path, two hosts: against a shell that advertises the preem
//! vocabulary in `HostMsg::Hello` the chip goes out as a typed `Node::Preem`
//! the shell draws; against one that doesn't it CPU-rasterises to the
//! `Node::Pixels` a hand-written `preem::seven_seg(…).into_node(…)` would have
//! produced — byte for byte, which the tests below pin. The card makes the same
//! promise for its three widgets, and the tests pin that too.
//!
//! Nothing in `view` branches on which host is on the other end, and this chip
//! has no `advance` to call at all: a seven-segment readout is pure, so its
//! whole state is the string handed to `node`. That is the cheapest possible
//! migration shape, and it is the one most of the remaining bundled plugins
//! have.
//!
//! The page both surfaces open stays plain GTK labels: an RFC3339 timestamp
//! and a raw unix count are text, not a retro readout, and the contrast is the
//! point — the seam is opt-in per widget, not a mode the plugin enters.
//!
//! ## What a snapshot costs, stated (#898 review R5, corrected by #1409's)
//!
//! **Every snapshot puts a frame on the wire, on both surfaces.** The runtime
//! dedups on the whole `View`, tree and page together, and both surfaces
//! publish the page, whose first label is the host's timestamp — seconds and
//! nanoseconds included — so the view differs every second even while the
//! `HH:MM` holds. Measured over 120 one-second ticks (#1409 review), 120 of 120
//! frames went out on each surface in each mode:
//!
//! | | preem host (state) | old host (raster) |
//! |---|---|---|
//! | chip | 509 B | 53 KB (the 188×70 readout) |
//! | card | 785 B | ~122 KB (the three buffers above) |
//!
//! So against a shell that does not speak preem, each surface re-sends its
//! rasterised pixels about once a second. That is the honest price of a pixel
//! surface beside a per-second page, and it is left as it stands on purpose:
//! the shipped shell speaks preem, and a cache keyed on the reading would need
//! interior mutability in `view(&self)`, making the reference plugin less
//! readable than the thing it references.

use hytte_plugin::display::{DotMatrix, FlipBoard, LedStrip, Mechanism, SevenSeg, StyleName};
use hytte_plugin::proto::{
    Capability, Dir, Effect, EventKind, Manifest, Mount, Node, Page, StateKey,
};
use hytte_plugin::{CmdSender, Input, Plugin, View};

/// Stable plugin id — the host's mount-slot ownership key and audit-log
/// subject, and what a **second** instance of this binary must be given a
/// different one of (`HYTTE_PLUGIN_ID`, #1250), since the host allows one live
/// connection per id.
const PLUGIN_ID: &str = "clock-demo";

/// Where this plugin mounts when the launch says nothing.
///
/// [`Mount::SidebarTop`] because this is first of all the **reference**
/// plugin, and the card is the surface a new author meets first. It stopped
/// being the plainest surface in the tree with #1408 — it is three preem
/// widgets inside one button now — so what it shows beyond the TEA shape is
/// the display seam, which is the next thing such an author needs. A bar
/// instance is the deliberate opt-in of a `mount = "BarCenter"` on its own
/// `programs.trollshell.plugins` entry.
const DEFAULT_MOUNT: Mount = Mount::SidebarTop;

/// Node ids for the **sidebar card** — constants, never derived from the
/// model, because the three preem ids are the host reconciler's keys (#882's
/// `preem_id` rule, as for [`CHIP_TIME_ID`]): an id that changed between
/// renders would rebuild the renderer, and the board would lose its flip
/// mid-fold.
///
/// [`CARD_BTN`] is both the tree's root and its click target: the whole card is
/// one button.
const CARD_BTN: &str = "clock-demo-card";
/// The vertical `Box` inside the card button, holding the three rows.
const FACE_ID: &str = "clock-demo-face";
/// The split-flap `HH:MM` board.
const TIME_ID: &str = "clock-demo-time";
/// The dot-matrix date line.
const DATE_ID: &str = "clock-demo-date";
/// The seconds sweep.
const SECONDS_ID: &str = "clock-demo-seconds";

/// Node ids for the **bar chip** and the drawer page it opens — disjoint from
/// the card's, so one `update` can tell the two surfaces' click targets apart
/// without a second dispatch table.
///
/// [`CHIP_TIME_ID`] keys the host reconciler onto the *same* preem renderer
/// instance across renders (#882's `preem_id` rule), which is what lets the
/// shell own the widget's continuity in state mode and swap the texture in
/// place in raster mode.
const CHIP_ID: &str = "clock-demo-chip";
const CHIP_TIME_ID: &str = "clock-demo-chip-time";
/// The clickable chip button — its `Click` opens the plugin's own page.
const CHIP_BTN: &str = "clock-demo-chip-btn";
/// Page ids: the page root, the full ISO timestamp and the unix seconds. The
/// page belongs to both surfaces since #1408; the ids predate that.
const PAGE_ID: &str = "clock-demo-page";
const PAGE_ISO_ID: &str = "clock-demo-page-iso";
const PAGE_UNIX_ID: &str = "clock-demo-page-unix";

/// The all-dash face the chip's readout wears before the first snapshot lands,
/// and whenever the host's timestamp doesn't project to an `HH:MM` — see
/// [`clock_face`]. The card's board wears it too: `-` and `:` are both on the
/// split-flap drum.
const NO_CLOCK: &str = "--:--";

/// The date line's placeholder, in the same ten cells as a real date so the
/// line keeps its proportions (and so its height, #1387) before the first
/// snapshot and for any timestamp that names no real date.
const NO_DATE: &str = "--- -- ---";

/// The skin every widget on the **card** wears: VFD, a near-black field with a
/// phosphor halo off every lit segment.
///
/// Chosen to match the chip rather than to contrast with it: the chip's
/// seven-segment readout is VFD (the skin the timer's bar readout wears), so
/// with both instances on screen the bar and the sidebar show one device
/// family. A constant, not a knob — the demo has no config file, and a clock
/// that re-skins itself rebuilds its renderers and loses the fold in flight.
const CARD_SKIN: StyleName = StyleName::Vfd;

/// Cells on the card's board: exactly `HH:MM`.
const FACE_CELLS: u32 = 5;
/// The board's logical pixels per font pixel (even, as the kit requires).
///
/// A board is `glyph_px × (8 × cells + 1)` px wide at scale 1 — 246 px here,
/// the widest even pitch inside the card. The card stretches it to its own
/// width either way (see the crate docs on sizing), so what this buys is the
/// resolution the fold is drawn at, not a bigger clock.
const FACE_GLYPH_PX: u32 = 6;
/// The board's baked-in integer upscale: none. [`FACE_GLYPH_PX`] already
/// carries the size, and a logical pixel per buffer pixel keeps the falling
/// card's edge and the hinge slot one pixel fine.
const FACE_SCALE: u32 = 1;
/// The date line's dot pitch: ten cells at 4 px are `4 × (6 × 10 + 1)` = 244 px,
/// the board's width to within two pixels.
const DATE_DOT_PX: u32 = 4;
/// Segments on the seconds sweep — see the crate docs for why 20.
const SWEEP_LEDS: u16 = 20;
/// Seconds each sweep segment stands for.
const SWEEP_SECS_PER_LED: u16 = 60 / SWEEP_LEDS;
const _: () = assert!(
    SWEEP_LEDS * SWEEP_SECS_PER_LED == 60,
    "the sweep divides the minute evenly",
);

/// Weekday abbreviations, Sunday first — the order [`weekday`] counts in.
const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
/// Month abbreviations, January first.
const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];

/// The plugin's entire state. Lives here — the host never stores or
/// round-trips it; it is rebuilt on every (re)connect and re-derived from the
/// next snapshot.
///
/// `PartialEq` but not `Eq`: [`FlipBoard`] and [`LedStrip`] carry `f32`s.
/// Nothing compares whole models for equality outside the tests anyway.
#[derive(Debug, PartialEq)]
struct ClockDemo {
    /// Latest ISO-8601 local timestamp from the host's clock subscription —
    /// the one source of the time, the date and the seconds on both surfaces.
    iso: String,
    /// Latest unix seconds (kept to show the full projected `ClockState` on
    /// the page). Deliberately **not** a source for the date line: it is UTC.
    unix: i64,
    /// Which surface this instance is, resolved once at [`Plugin::init`] from
    /// the launch's effective mount. Not a knob and not derived from anything
    /// in the model: a process is a chip or a card for its whole life.
    is_bar: bool,
    /// The chip's seven-segment readout (#884). Config only — a seven-segment
    /// strip is pure, so this carries no animation state and needs no
    /// `advance`; the text is handed to `node` at render time.
    ///
    /// Held by both arms rather than only the bar one: it is a style name in a
    /// struct, the model stays one shape, and a sidebar instance simply never
    /// renders it. The card's three widgets below are held the same way, for
    /// the same reason.
    seg: SevenSeg,
    /// The card's split-flap `HH:MM` board (#1408). **Stateful**: the flip
    /// clocks behind each card live here for the raster arm, so the face is
    /// stated in [`update`](Plugin::update) (see [`ClockDemo::restate_card`]),
    /// not handed to `node` like the pure widgets' text.
    flap: FlipBoard,
    /// The card's dot-matrix date line. Pure, like [`seg`](Self::seg): its
    /// text ([`date_line`]) is a `view` argument.
    date: DotMatrix,
    /// The card's seconds sweep. Its level is state, stated in `update` beside
    /// the board's face; it declares no peak-hold, so it has no animation of
    /// its own to advance.
    sweep: LedStrip,
}

/// Project an RFC3339 timestamp (`2026-07-11T15:49:00+02:00`) to the compact
/// `HH:MM` a bar chip shows. Panic-free over any host-sent value: a string
/// without a `T`, or one too short after it, degrades to the raw input rather
/// than slicing out of bounds.
fn short_time(iso: &str) -> String {
    match iso.find('T') {
        // `T` + `HH:MM` is 5 chars; `get` returns `None` (→ fall back) if the
        // string is truncated there, so this never panics on a bad boundary.
        Some(t) => iso.get(t + 1..t + 6).unwrap_or(iso).to_owned(),
        None => iso.to_owned(),
    }
}

/// [`short_time`] narrowed to what the seven-segment chip can actually draw:
/// anything that isn't a literal `HH:MM` falls back to [`NO_CLOCK`].
///
/// The narrowing came with #884 and it is the one thing the widget swap
/// genuinely changed. As a `Node::Label` a malformed timestamp was merely ugly;
/// a seven-segment strip lays its whole message out on one line at 40 px a
/// character, so a passthrough of the raw ISO string would be a ~1000 px chip
/// in the bar. It also covers the pre-snapshot seed (`"—"`, which has no glyph
/// on a seven-segment drum at all) with the all-dash face a real readout shows
/// when it has no reading.
fn clock_face(iso: &str) -> String {
    let face = short_time(iso);
    let b = face.as_bytes();
    let is_hhmm = b.len() == 5
        && b[2] == b':'
        && b[..2].iter().all(u8::is_ascii_digit)
        && b[3..].iter().all(u8::is_ascii_digit);
    if is_hhmm { face } else { NO_CLOCK.to_owned() }
}

/// A short run of ASCII digits as a number, or `None` if any byte is not one.
/// Only ever handed fixed-width fields of at most four digits, which a `u16`
/// holds without overflow.
fn digits(field: &str) -> Option<u16> {
    field.bytes().try_fold(0u16, |acc, b| {
        b.is_ascii_digit().then(|| acc * 10 + u16::from(b - b'0'))
    })
}

/// Days in `month` (1-based) of `year`, Gregorian; `0` for a month that does
/// not exist, so no day is valid in it.
fn days_in_month(year: u16, month: u16) -> u16 {
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    }
}

/// The **local** calendar date an RFC3339 timestamp names, as
/// `(year, month, day)` — or `None` unless it opens with a real
/// `YYYY-MM-DD` followed by the `T` that starts the time.
///
/// Local because it is read off the timestamp's own date field, which the host
/// renders in the session's zone; `ClockState::unix` would give the UTC date
/// instead. A day the month does not have (`2026-02-29`) is `None`, not a
/// weekday computed for a date that never existed.
fn local_date(iso: &str) -> Option<(u16, u16, u16)> {
    let b = iso.as_bytes();
    if b.len() < 11 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let year = digits(iso.get(0..4)?)?;
    let month = digits(iso.get(5..7)?)?;
    let day = digits(iso.get(8..10)?)?;
    (1..=days_in_month(year, month))
        .contains(&day)
        .then_some((year, month, day))
}

/// Index into [`WEEKDAYS`] (Sunday = 0) of a date [`local_date`] validated.
///
/// Sakamoto's method. January and February count as the previous year's
/// thirteenth and fourteenth months, which the offset table folds in; the year
/// is shifted up by one whole 400-year Gregorian cycle first, which leaves
/// every weekday where it was and keeps year `0000`'s January out of negative
/// numbers.
fn weekday(year: u16, month: u16, day: u16) -> usize {
    const OFFSETS: [usize; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let y = usize::from(year) + 400 - usize::from(month < 3);
    (y + y / 4 - y / 100 + y / 400 + OFFSETS[usize::from(month) - 1] + usize::from(day)) % 7
}

/// The card's date line — `SAT 11 JUL` — from the host timestamp's local date,
/// or [`NO_DATE`] when it names none. The day is zero-padded so every real
/// date is the same ten cells wide.
fn date_line(iso: &str) -> String {
    local_date(iso)
        .and_then(|(year, month, day)| {
            let name = MONTHS.get(usize::from(month).checked_sub(1)?)?;
            let wday = WEEKDAYS.get(weekday(year, month, day))?;
            Some(format!("{wday} {day:02} {name}"))
        })
        .unwrap_or_else(|| NO_DATE.to_owned())
}

/// The seconds of an RFC3339 timestamp, or `None` unless its time reads as a
/// literal `HH:MM:SS` (the host's timestamps may carry a fraction after that;
/// it is ignored, as the board ignores the seconds).
fn seconds(iso: &str) -> Option<u16> {
    let t = iso.find('T')?;
    let hms = iso.get(t + 1..t + 9)?;
    let b = hms.as_bytes();
    if b[2] != b':' || b[5] != b':' {
        return None;
    }
    digits(hms.get(0..2)?)?;
    digits(hms.get(3..5)?)?;
    digits(hms.get(6..8)?)
}

/// How many sweep segments a timestamp lights: the segment its second falls in
/// and every one before it, so `1..=`[`SWEEP_LEDS`] — or `0`, all dark, when
/// the timestamp carries no readable seconds. A leap second's `60` lights the
/// last segment rather than running off the end.
fn sweep_lit(iso: &str) -> u16 {
    seconds(iso).map_or(0, |s| (s / SWEEP_SECS_PER_LED + 1).min(SWEEP_LEDS))
}

impl ClockDemo {
    /// The seed model for one launch, with the mount override read through an
    /// injected `lookup` rather than the process environment.
    ///
    /// This is the whole of [`Plugin::init`] — `init` does nothing but call it
    /// with the real `std::env::var` — which is deliberate, on
    /// `hytte-claude-bridge`'s `resolve_settings` precedent (#1315 review, MED
    /// 3): the pure half ([`hytte_plugin::effective_mount_from`]) is already
    /// covered by the SDK's own tests, so what a test here has to reach is the
    /// line that *threads* the answer into the model. With the composition
    /// living here, hardcoding `is_bar` goes red in the `tests` module's
    /// `the_launch_mount_picks_the_surface`.
    ///
    /// **That is not the whole of it**, and saying so was this function's own
    /// review finding (#1389): [`Plugin::init`]'s one line — the `lookup` it
    /// passes — is a second place the wiring lives, and no test built on this
    /// seam can reach it, because every one of them supplies its own `lookup`.
    /// Neutering `init` to `with_launch(&|_| None)` therefore left all 13
    /// tests green while shipping a bar instance that renders the sidebar
    /// card. `init_reads_the_real_process_environment` is what closes it: a
    /// re-exec'd child of the test binary with a real `HYTTE_PLUGIN_MOUNT` in
    /// its environment, calling `init` itself. The tree has closed this same
    /// hole three times (`hytte-plugin`'s `runtime`, `hytte-claude-bridge`'s
    /// `plugin`, `hytte-plugin-stats`' `plugin`), each as a review finding.
    ///
    /// `&dyn Fn` rather than a generic: `unsafe_code = "forbid"` rules out
    /// `std::env::set_var` (an `unsafe fn` in edition 2024), so a test cannot
    /// drive the real environment at all and this seam is the only way in.
    fn with_launch(lookup: &dyn Fn(&str) -> Option<String>) -> Self {
        let mut model = Self {
            // Placeholder time until the first snapshot lands (the runtime
            // renders this seed immediately, so the slot mounts right away).
            iso: "—".to_owned(),
            unix: 0,
            is_bar: hytte_plugin::effective_mount_from(DEFAULT_MOUNT, lookup).is_bar(),
            // VFD: the same skin the timer's bar readout wears, so the two
            // seven-segment chips in the bar match.
            seg: SevenSeg::new(StyleName::Vfd),
            flap: FlipBoard::new(CARD_SKIN, Mechanism::SplitFlap)
                .cells(FACE_CELLS)
                .glyph_px(FACE_GLYPH_PX)
                .scale(FACE_SCALE),
            date: DotMatrix::new(CARD_SKIN).dot_px(DATE_DOT_PX),
            sweep: LedStrip::new(CARD_SKIN).leds(u32::from(SWEEP_LEDS)),
        };
        // The seed card shows the dashes the chip does, rather than a blank
        // board: the placeholder `iso` projects to `NO_CLOCK`.
        model.restate_card();
        model
    }

    /// Point the card's two **stateful** widgets at the model's current
    /// timestamp: the board at its `HH:MM`, the sweep at its seconds. The date
    /// line needs no call — a dot-matrix strip is pure, so its text is a `view`
    /// argument.
    ///
    /// Called on every snapshot. Re-stating an unchanged face is inert (the kit
    /// leaves a cell alone when it is already showing its character), so the
    /// board moves once a minute. The `settle` is the raster arm's whole
    /// animation story — see the crate docs: it lands the plugin-side cards at
    /// once, because nothing in this process would ever advance them, and in
    /// state mode it touches nothing on the wire.
    fn restate_card(&mut self) {
        self.flap.set_text(&clock_face(&self.iso));
        self.flap.settle();
        self.sweep
            .set_level(f32::from(sweep_lit(&self.iso)) / f32::from(SWEEP_LEDS));
    }

    /// The effect a click on `node` produces, which depends on **which surface
    /// this instance renders**.
    ///
    /// Both surfaces now answer with the same effect, but it is still gated on
    /// [`is_bar`](Self) rather than on the node id alone. The two id sets are
    /// disjoint, so matching ids would give the same answer for every event the
    /// host can actually send — but then a click routed for the surface this
    /// process does *not* draw would still fire an effect, and the split would
    /// be a property of the id constants rather than of the instance. This way
    /// `view` and `update` state the same thing.
    fn on_click(&self, node: &str) -> Vec<Effect> {
        match (self.is_bar, node) {
            // Either surface opens the plugin's own page (#349 PR2 for the
            // chip, #1408 for the card). The host resolves `PluginSelf` to
            // *this* plugin's page by the effect's plugin id — no page name to
            // know — and picks the drawer or the dialog by our mount (#1010).
            (true, CHIP_BTN) | (false, CARD_BTN) => vec![Effect::OpenPage(Page::PluginSelf)],
            _ => Vec::new(),
        }
    }

    /// The page either surface's click opens: a vertical `Box` showing the full
    /// projected `ClockState` — the RFC3339 timestamp and the raw unix seconds —
    /// a tree distinct from both the compact chip and the card. Its root
    /// carries **no** `.card`/`.ts-plugin-*` class: the drawer and the dialog
    /// each supply their own chrome, so the page owns only its inner content.
    ///
    /// One builder for both arms (#1408): the card and the chip open the same
    /// page, so there is one definition of it for the two to agree on.
    fn page(&self) -> Node {
        Node::Box {
            id: Some(PAGE_ID.to_owned()),
            dir: Dir::Vertical,
            spacing: 6,
            scroll: false,
            classes: Vec::new(),
            children: vec![
                Node::Label {
                    id: Some(PAGE_ISO_ID.to_owned()),
                    text: self.iso.clone(),
                    classes: vec!["title-2".to_owned()],
                    tooltip: None,
                },
                Node::Label {
                    id: Some(PAGE_UNIX_ID.to_owned()),
                    text: format!("unix: {}", self.unix),
                    classes: vec!["dim-label".to_owned()],
                    tooltip: None,
                },
            ],
            tooltip: None,
        }
    }

    /// The **sidebar card** (#1408) and the page its click opens.
    ///
    /// The card is one flat [`Node::Button`] — the click target is the whole
    /// card, and `flat` (an Adwaita built-in) strips the button chrome so the
    /// host's own `.ts-plugin-card` treatment is the only card drawn — holding
    /// a vertical `Box` of the three rows: the split-flap `HH:MM`, the
    /// dot-matrix date line and the seconds sweep.
    ///
    /// Each row is one `node` call, the #884 seam: a typed `Node::Preem` to a
    /// host that advertised the preem vocabulary, a rasterised `Node::Pixels`
    /// otherwise, with no branch here. No CSS class on any of them, for the
    /// chip's reason — they are pixel surfaces with their font baked in.
    fn card(&self) -> View {
        let face = Node::Box {
            id: Some(FACE_ID.to_owned()),
            dir: Dir::Vertical,
            spacing: 6,
            scroll: false,
            classes: Vec::new(),
            children: vec![
                self.flap.node(TIME_ID),
                self.date.node(DATE_ID, &date_line(&self.iso)),
                self.sweep.node(SECONDS_ID),
            ],
            tooltip: None,
        };
        View::new(Node::Button {
            id: CARD_BTN.to_owned(),
            classes: vec!["flat".to_owned()],
            child: Box::new(face),
        })
        .panel(self.page())
    }

    /// The **bar chip** and the drawer page its click opens (#349).
    ///
    /// The chip — wrapped by the host in a `.ts-plugin-chip` pill — is a
    /// horizontal `Box` holding a [`Node::Button`] (the click target) whose
    /// child is the compact `HH:MM` seven-segment readout. The page is
    /// [`page`](Self::page), shared with the card.
    ///
    /// The one `node` call is the whole #884 seam: it lands as a typed
    /// `Node::Preem` or a rasterised `Node::Pixels` depending on what the host
    /// advertised, with no branch here. The readout carries no CSS class — a
    /// monospace/tabular font rule like `ts-clock` means nothing to a pixel
    /// surface with its own font baked in.
    fn chip(&self) -> View {
        let chip = Node::Box {
            id: Some(CHIP_ID.to_owned()),
            dir: Dir::Horizontal,
            spacing: 4,
            scroll: false,
            classes: Vec::new(),
            children: vec![Node::Button {
                id: CHIP_BTN.to_owned(),
                classes: Vec::new(),
                child: Box::new(self.seg.node(CHIP_TIME_ID, &clock_face(&self.iso))),
            }],
            tooltip: None,
        };
        View::new(chip).panel(self.page())
    }
}

impl Plugin for ClockDemo {
    /// Purely host-driven: no timers, no fetches, no self-generated messages.
    type Msg = std::convert::Infallible;

    /// Purely display: it issues no I/O of its own, so it has no commands and
    /// ignores the command lane entirely (see `hytte_plugin`'s *Commands*
    /// docs). `Infallible` = "no command can ever be constructed".
    type Cmd = std::convert::Infallible;

    /// Subscribes to `Clock`, mounts [`DEFAULT_MOUNT`], requests the
    /// [`Capability::OpenPage`] capability. `Manifest::new` stamps
    /// `proto = PROTO_VERSION`, which the host exact-matches at the handshake.
    ///
    /// One manifest for both surfaces: it is per binary, not per instance. The
    /// capability list is therefore the **union** of what the two arms use, and
    /// that union is one entry — the card and the chip each need `OpenPage`,
    /// for the same `Page::PluginSelf`. Still nothing else: no `RunCommand`, no
    /// `Notify`, no `OpenUri`. The tests assert the list is exactly
    /// `[OpenPage]` rather than that it lacks something, because the host
    /// auto-grants every manifest capability.
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, DEFAULT_MOUNT).with_version(env!("CARGO_PKG_VERSION"));
        m.subscribes = vec![StateKey::Clock];
        m.capabilities = vec![Capability::OpenPage];
        m
    }

    /// The seed model, with this launch's mount resolved once — see
    /// [`ClockDemo::with_launch`], which is the whole of this function. The
    /// command sender goes unused: this plugin only reads state and asks the
    /// host to open a page.
    fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
        Self::with_launch(&|key| std::env::var(key).ok())
    }

    /// Fold one input into the model. Pure and panic-free over any host-sent
    /// value — this is the testable heart of the plugin. Re-rendering is the
    /// runtime's problem (identical trees are deduped), so a snapshot without
    /// a clock simply changes nothing.
    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            // Subscribed-state snapshot: take the clock. `clock` is optional
            // on the wire (a startup snapshot may arrive before the host's
            // clock pump has published), so tolerate `None`.
            Input::Snapshot(snapshot) => {
                if let Some(clock) = snapshot.clock {
                    self.iso = clock.iso;
                    self.unix = clock.unix;
                    // The card's stateful widgets follow the timestamp here;
                    // everything else on either surface is derived in `view`.
                    self.restate_card();
                }
                Vec::new()
            }
            // Our only interactive node is this surface's button; what its
            // click opens is `on_click`'s business. The effect rides exactly
            // one render frame, so a clock tick never re-fires it.
            Input::Event {
                node,
                kind: EventKind::Click,
                ..
            } => self.on_click(&node),
            // Any other interaction, effect result, or sidebar-visibility push
            // (#288) is a no-op that never touches the view — no `RunCommand`
            // is issued (so no `EffectResult` is expected), and neither surface
            // has pollers to park. Listed rather than wildcarded so a new
            // host→plugin frame is a compile error here, which is the place to
            // decide whether this demo cares.
            Input::Event { .. }
            | Input::EffectResult { .. }
            | Input::SlotVisible(_)
            | Input::AudioSpectrum(_)
            | Input::ConsentDecision { .. }
            | Input::CalendarUpcoming(_)
            | Input::SessionLocked(_)
            | Input::NowPlaying(_)
            | Input::DatasourceQuery { .. }
            | Input::DatasourceResult { .. } => Vec::new(),
            // `Msg = Infallible`: there are no app messages to receive.
            Input::App(never) => match never {},
        }
    }

    /// Project the model into the declarative widget tree the host reconciles
    /// into GTK — **which** tree being the one decision this plugin makes about
    /// its own placement (#1388).
    ///
    /// A bar instance renders the chip and a sidebar instance the card; both
    /// publish the one page a click on either opens. `hytte-plugin-stats`'
    /// `Stats::view` and `hytte-claude-bridge`'s are the same split, for the
    /// same reason.
    fn view(&self) -> View {
        if self.is_bar {
            self.chip()
        } else {
            self.card()
        }
    }
}

fn main() {
    hytte_plugin::run::<ClockDemo>()
}

#[cfg(test)]
mod tests {
    use super::{
        CARD_BTN, CHIP_BTN, CHIP_ID, CHIP_TIME_ID, ClockDemo, DATE_ID, DEFAULT_MOUNT, FACE_ID,
        NO_CLOCK, NO_DATE, PAGE_ID, PLUGIN_ID, SECONDS_ID, TIME_ID, clock_face, date_line,
        short_time, sweep_lit,
    };
    use hytte_plugin::display::{Mechanism, RenderMode, StyleName, testing::with_render_mode};
    use hytte_plugin::preem::{self, DisplayStyle, seven_seg};
    use hytte_plugin::proto::preem::PreemWidget;
    use hytte_plugin::proto::{
        Capability, ClockState, Dir, Effect, EventKind, Mount, Node, Page, PluginMsg, StateKey,
        StateSnapshot, decode, encode,
    };
    use hytte_plugin::{Input, Plugin};

    /// The variable spelled as a **literal**, never through the SDK's own
    /// private `MOUNT_ENV` const: these tests are a statement about the
    /// documented launch contract (`docs/plugin-env.md`), and deriving the name
    /// from the thing under test would follow a rename straight past the
    /// deployment that did not get one.
    const MOUNT_ENV: &str = "HYTTE_PLUGIN_MOUNT";

    fn clock_snapshot(iso: &str, unix: i64) -> Input<std::convert::Infallible> {
        Input::Snapshot(StateSnapshot {
            clock: Some(ClockState {
                iso: iso.to_owned(),
                unix,
            }),
        })
    }

    /// A model as a launch that set [`MOUNT_ENV`] to `mount` would build it —
    /// the one seam these tests have on the split, since `std::env::set_var` is
    /// `unsafe` and the crate forbids unsafe.
    fn launched_at(mount: Mount) -> ClockDemo {
        ClockDemo::with_launch(&move |key| (key == MOUNT_ENV).then(|| mount.wire_name().to_owned()))
    }

    /// A fresh **sidebar** model: a launch that says nothing, so the manifest's
    /// own [`DEFAULT_MOUNT`] stands. The demo issues no commands, so no command
    /// lane is built at all.
    fn fresh() -> ClockDemo {
        ClockDemo::with_launch(&|_| None)
    }

    /// A fresh **bar** model — the same binary, launched into the bar.
    fn fresh_bar() -> ClockDemo {
        launched_at(Mount::BarCenter)
    }

    /// The id of a tree's root — the chip's `Box` or the card's `Button` —
    /// which is what tells the two surfaces apart without touching either
    /// one's contents (or, in raster mode, its pixel buffers).
    fn root_id(tree: &Node) -> Option<&str> {
        match tree {
            Node::Box { id, .. } => id.as_deref(),
            Node::Button { id, .. } => Some(id),
            _ => None,
        }
    }

    // ── The split (#1388) ───────────────────────────────────────────────────

    /// **The pin on the fold**: one binary, two surfaces, chosen by the
    /// family of the launch's effective mount and by nothing else.
    ///
    /// Swept over all nine mounts rather than one of each, with `Mount::is_bar`
    /// — the wire's own family line — as the oracle, so a mount added to the
    /// vocabulary is covered the day it lands. The chain under test is the
    /// whole one: `HYTTE_PLUGIN_MOUNT` → [`hytte_plugin::effective_mount_from`]
    /// → the model's `is_bar` → which tree `view` returns.
    ///
    /// Since #1408 both surfaces publish the page, so the root id is the whole
    /// of the distinction; the page itself is pinned by
    /// `the_card_opens_the_chips_page`.
    ///
    /// Falsified before shipping: swapping `view`'s two arms turns this red on
    /// the first mount it checks.
    #[test]
    fn the_launch_mount_picks_the_surface() {
        for mount in Mount::ALL {
            let model = launched_at(mount);
            // State mode: nothing in either tree is a pixel buffer here, so a
            // failure prints ids rather than 52 KB of RGBA.
            let view = with_render_mode(RenderMode::State, || model.view());
            let expected = if mount.is_bar() { CHIP_ID } else { CARD_BTN };
            assert_eq!(root_id(&view.tree), Some(expected), "{mount:?}");
            assert!(
                view.panel.is_some(),
                "either surface publishes the page its click opens ({mount:?})",
            );
        }

        // …and a launch that says nothing at all is the card: this is first of
        // all the reference plugin, and `DEFAULT_MOUNT` is what its manifest
        // ships with.
        assert!(!DEFAULT_MOUNT.is_bar());
        let seed = with_render_mode(RenderMode::State, || fresh().view());
        assert_eq!(root_id(&seed.tree), Some(CARD_BTN));
    }

    /// Set (to any value) only on the re-exec'd child that actually runs
    /// [`init_reads_the_real_process_environment_inner`] — the same marker
    /// shape as `hytte-plugin::runtime`'s `MOUNT_ENV_CHILD` and
    /// `hytte-claude-bridge`'s `INIT_ENV_CHILD`, and for the same reason: an
    /// ordinary `cargo test` run discovers the inner test like any other and
    /// must not try to run its scenario with no launch environment set.
    const INIT_ENV_CHILD: &str = "HYTTE_PLUGIN_CLOCK_DEMO_INIT_TEST_CHILD";

    /// Printed by the child only once its scenario has run to completion and
    /// passed, so the parent can tell "the scenario passed" from "the
    /// `--exact` filter matched no test and libtest still reports `0 passed`,
    /// exit 0" — the failure mode a renamed inner test produces.
    const INIT_ENV_CHILD_OK: &str = "init-env-child-reached-the-end";

    /// **[`Plugin::init`] itself reads the real process environment** — the
    /// one production line every other test in this module routes around
    /// (#1389 review, HIGH 1).
    ///
    /// The whole suite reaches the model through [`ClockDemo::with_launch`]
    /// with a `lookup` of its own, so `init`'s `&|key| std::env::var(key).ok()`
    /// is never executed by it: neutering that line to `&|_| None` left 13
    /// tests, clippy and the whole of `nix flake check` green while shipping a
    /// **bar instance that renders the sidebar card** — a unit that starts
    /// cleanly, stays running and says nothing. The tree has closed this exact
    /// seam-wrapper hole three times before, each as a review finding
    /// (`hytte-plugin`'s `the_mount_env_var_reaches_the_register_frame`,
    /// `hytte-claude-bridge`'s `init_reaches_the_view_with_a_real_process_environment`,
    /// `hytte-plugin-stats`' `settings_reads_the_real_process_environment`);
    /// this is the fourth and it is the one #1388 asked for by name, since the
    /// ask was "exactly the way `hytte-plugin-stats` does it".
    ///
    /// A **child process** rather than a `set_var`: `unsafe_code = "forbid"`
    /// makes `std::env::set_var` (an `unsafe fn` in edition 2024) unspellable
    /// here, so the only way to hand this process's own `getenv` a value is to
    /// be a different process — `Command::env` is the safe builder that does
    /// it.
    ///
    /// **Two children, because they catch different mutations.** The override
    /// child sets `HYTTE_PLUGIN_MOUNT=BarCenter` and wants the chip:
    /// `BarCenter` rather than a sidebar name precisely because
    /// [`DEFAULT_MOUNT`] is a sidebar mount, so a fully neutered `init`
    /// (falling back to the manifest) and a working one give *different*
    /// answers — the inner test asserts that premise before anything else.
    /// The neutral child removes the variable and wants the card, which is
    /// what catches the opposite mutation (an `init` that hands over a
    /// constant `Some("BarCenter")`, which the override child would happily
    /// pass) and is also the one place this crate proves its shipped default
    /// survives `init` unmolested, in the same real-process harness.
    #[test]
    fn init_reads_the_real_process_environment() {
        let inner = "tests::init_reads_the_real_process_environment_inner";
        let args = ["--exact", "--nocapture", "--test-threads=1", inner];
        assert!(
            args.contains(&"--exact"),
            "the re-exec must stay filtered to exactly one inner test",
        );
        let exe = std::env::current_exe().expect("this test binary's own path");

        let overridden = std::process::Command::new(&exe)
            .args(args)
            .env(INIT_ENV_CHILD, "1")
            .env(MOUNT_ENV, "BarCenter")
            .output()
            .expect("re-exec this test binary with the mount override set");
        assert_init_child_reached_the_end(&overridden, inner, "override");

        // `env_remove`, not merely "unset": a child inherits the parent's
        // environment, so a developer running `cargo test` from a shell that
        // happens to export `HYTTE_PLUGIN_MOUNT` would otherwise get a neutral
        // child that is not neutral.
        let neutral = std::process::Command::new(&exe)
            .args(args)
            .env(INIT_ENV_CHILD, "1")
            .env_remove(MOUNT_ENV)
            .output()
            .expect("re-exec this test binary with no mount override");
        assert_init_child_reached_the_end(&neutral, inner, "neutral");
    }

    /// A child scenario both exited 0 **and** reached its own end marker.
    ///
    /// The second half is not redundant: `--exact` against a renamed inner
    /// test matches nothing, libtest reports `0 passed` and exits 0, and the
    /// whole pin goes inert — the failure mode both precedents call out.
    fn assert_init_child_reached_the_end(out: &std::process::Output, inner: &str, which: &str) {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "the {which} child scenario failed ({:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            out.status,
        );
        assert!(
            stdout.contains(INIT_ENV_CHILD_OK),
            "the {which} child exited 0 without reaching the end of {inner} — a stale \
             filter matches no test and libtest still reports success\n\
             --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        );
    }

    /// The scenario body of [`init_reads_the_real_process_environment`]. Does
    /// nothing at all unless the parent's marker is set, so an ordinary
    /// `cargo test` run — which discovers it like any other test — does not
    /// try to run it with no launch environment set up for it.
    #[test]
    fn init_reads_the_real_process_environment_inner() {
        if std::env::var_os(INIT_ENV_CHILD).is_none() {
            return;
        }
        // The premise the parent's choice of override rests on, asserted
        // rather than assumed (`hytte-plugin-stats`' copy of this, #1327
        // review LOW 1): if `DEFAULT_MOUNT` ever moved to a bar region — not a
        // hypothetical on a plugin whose whole point is running on both
        // families — a neutered `init` and a working one would agree on the
        // override child and this test would quietly stop catching anything.
        assert!(
            !DEFAULT_MOUNT.is_bar(),
            "test setup: the override's family must differ from DEFAULT_MOUNT's own, \
             or a neutered init() and a working one give the same answer",
        );

        let (tx, _rx) = hytte_plugin::cmd_channel();
        // `init`, not `with_launch`: reaching the seam's wrapper is the entire
        // point of being a separate process.
        let model = ClockDemo::init(tx);
        let view = with_render_mode(RenderMode::State, || model.view());

        match std::env::var(MOUNT_ENV).ok().as_deref() {
            Some("BarCenter") => {
                assert_eq!(
                    root_id(&view.tree),
                    Some(CHIP_ID),
                    "init must resolve the surface from the REAL environment",
                );
            }
            None => {
                assert_eq!(
                    root_id(&view.tree),
                    Some(CARD_BTN),
                    "with nothing set, init must land on the manifest's own mount",
                );
            }
            other => panic!("unexpected child environment: {MOUNT_ENV}={other:?}"),
        }
        assert!(view.panel.is_some(), "either surface publishes its page");
        println!("{INIT_ENV_CHILD_OK}");
    }

    /// The other half of the split: an instance answers only the click target
    /// it actually renders, so `update` and `view` state the same thing.
    #[test]
    fn each_surface_answers_only_its_own_button() {
        let mut bar = fresh_bar();
        assert_eq!(
            bar.update(Input::event(CHIP_BTN, EventKind::Click)),
            vec![Effect::OpenPage(Page::PluginSelf)],
            "the chip opens this plugin's own page",
        );
        assert!(
            bar.update(Input::event(CARD_BTN, EventKind::Click))
                .is_empty(),
            "a bar instance never renders the card",
        );

        let mut card = fresh();
        assert_eq!(
            card.update(Input::event(CARD_BTN, EventKind::Click)),
            vec![Effect::OpenPage(Page::PluginSelf)],
            "the card opens this plugin's own page",
        );
        assert!(
            card.update(Input::event(CHIP_BTN, EventKind::Click))
                .is_empty(),
            "a sidebar instance never renders the chip",
        );
    }

    /// One manifest for both surfaces: the sidebar default, the `Clock`
    /// subscription, and the **union** of the two arms' capabilities — which is
    /// the one entry both of them need.
    #[test]
    fn the_manifest_is_the_union_of_both_surfaces() {
        let m = ClockDemo::manifest();
        assert_eq!(m.id, PLUGIN_ID);
        assert_eq!(m.mount, DEFAULT_MOUNT, "the reference surface is the card");
        assert_eq!(m.subscribes, vec![StateKey::Clock]);
        assert_eq!(
            m.capabilities,
            vec![Capability::OpenPage],
            "exactly this, not merely `no RunCommand`: the host auto-grants \
             every capability a manifest names",
        );
    }

    // ── The sidebar card (#1408) ────────────────────────────────────────────

    /// What a preem-speaking host reads off the card: the board's `HH:MM`, the
    /// date line's text and the sweep's level, in that order — or a panic
    /// naming what arrived instead of the card's shape.
    fn card_readings(model: &ClockDemo) -> (String, String, f32) {
        let tree = with_render_mode(RenderMode::State, || model.view().tree);
        let Node::Button { child, .. } = &tree else {
            panic!("the card root is its click target, got {tree:?}")
        };
        let Node::Box { children, .. } = child.as_ref() else {
            panic!("the card button holds the face box, got {child:?}")
        };
        let [time, date, sweep] = children.as_slice() else {
            panic!("the face holds exactly three rows, got {children:?}")
        };
        let widget = |node: &Node| match node {
            Node::Preem { widget, .. } => widget.as_ref().clone(),
            other => panic!("expected Node::Preem, got {other:?}"),
        };
        let PreemWidget::FlipBoard { state: time, .. } = widget(time) else {
            panic!("the first row is the split-flap board")
        };
        let PreemWidget::DotMatrix { state: date, .. } = widget(date) else {
            panic!("the second row is the dot-matrix date line")
        };
        let PreemWidget::LedStrip { state: sweep, .. } = widget(sweep) else {
            panic!("the third row is the seconds sweep")
        };
        (time.text, date.text, sweep.level)
    }

    /// The core signal against **today's** shell: a snapshot updates the model,
    /// and a sidebar instance's `view` renders the exact card the host will
    /// reconcile — every row a rasterised kit buffer **byte-identical** to the
    /// one a plugin author would build with the raw kit by hand, inside the
    /// flat card button, with the page beside it.
    ///
    /// The kit is driven here with literal metrics rather than this crate's
    /// constants, so a changed board size, date pitch or segment count shows up
    /// as a failure to be looked at rather than following along silently. It
    /// also pins the raster arm's one behaviour of its own: the board has
    /// **landed** on the new face (the `settle` in `restate_card`); without it
    /// the kit would keep drawing the seed's dashes.
    #[test]
    fn snapshot_updates_model_and_renders_expected_tree() {
        let mut model = fresh();
        let effects = model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1_783_777_740));
        assert!(effects.is_empty());
        assert_eq!(model.iso, "2026-07-11T15:49:00+02:00");
        assert_eq!(model.unix, 1_783_777_740);

        let mut board = preem::FlipBoard::new(preem::Mechanism::SplitFlap)
            .cells(5)
            .glyph_px(6)
            .scale(1);
        board.set_text("15:49");
        board.settle();
        let expected = Node::Button {
            id: "clock-demo-card".to_owned(),
            classes: vec!["flat".to_owned()],
            child: Box::new(Node::Box {
                id: Some("clock-demo-face".to_owned()),
                dir: Dir::Vertical,
                spacing: 6,
                scroll: false,
                classes: vec![],
                children: vec![
                    board
                        .render(DisplayStyle::Vfd)
                        .into_node(Some("clock-demo-time"), vec![]),
                    preem::DotMatrix::new(DisplayStyle::Vfd)
                        .dot_px(4)
                        .render("SAT 11 JUL")
                        .into_node(Some("clock-demo-date"), vec![]),
                    // `:00` is the first three-second segment: one of 20 lit.
                    preem::LedStrip::new(DisplayStyle::Vfd)
                        .leds(20)
                        .render(1.0 / 20.0, 0.0)
                        .into_node(Some("clock-demo-seconds"), vec![]),
                ],
                tooltip: None,
            }),
        };
        // `==` rather than `assert_eq!`: the operands carry `Node::Pixels`,
        // whose own `Debug` would dump the whole RGBA buffer into the failure
        // output.
        let view = with_render_mode(RenderMode::Raster, || model.view());
        assert!(
            view.tree == expected,
            "the raster card must match the kit by hand"
        );
        assert_eq!(
            root_id(view.panel.as_ref().expect("the card publishes its page")),
            Some(PAGE_ID),
        );
    }

    /// Panic if any node in the subtree is a rasterised `Node::Pixels`. Names
    /// the id rather than printing the node, whose `Debug` is the whole buffer.
    fn assert_no_pixels(node: &Node) {
        match node {
            Node::Pixels { id, .. } => {
                panic!("a preem-speaking host must not get pixels ({id:?})")
            }
            Node::Button { child, .. } => assert_no_pixels(child),
            Node::Box { children, .. } => children.iter().for_each(assert_no_pixels),
            _ => {}
        }
    }

    /// …and against a shell that advertises the preem vocabulary, the *same*
    /// `view` ships three typed state nodes instead — the ids the reconciler
    /// keys on, the readings, the skin as a name — and **no pixels anywhere**
    /// in the tree (#884's promise, on the card's three widgets).
    #[test]
    fn against_a_preem_shell_the_card_is_state_nodes() {
        let mut model = fresh();
        model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1_783_777_740));

        let tree = with_render_mode(RenderMode::State, || model.view().tree);
        assert_no_pixels(&tree);

        let Node::Button { id, classes, child } = &tree else {
            panic!("the card root is its click target")
        };
        assert_eq!(id, CARD_BTN);
        assert_eq!(classes, &vec!["flat".to_owned()], "no button chrome");
        let Node::Box { children, .. } = child.as_ref() else {
            panic!("the card button holds the face box")
        };
        let [time, date, sweep] = children.as_slice() else {
            panic!("three rows")
        };

        let preem_of = |node: &Node, want: &str| match node {
            Node::Preem { id, widget, .. } => {
                assert_eq!(id.as_deref(), Some(want), "the reconciler's key");
                widget.as_ref().clone()
            }
            other => panic!("expected Node::Preem for {want}, got {other:?}"),
        };
        match preem_of(time, TIME_ID) {
            PreemWidget::FlipBoard { config, state } => {
                assert_eq!(state.text, "15:49", "the plugin's own reading");
                assert_eq!(config.mechanism, Mechanism::SplitFlap);
                assert_eq!(
                    (config.cells, config.glyph_px, config.scale),
                    (5, 6, 1),
                    "HH:MM, 246 px wide",
                );
                assert_eq!(
                    config.style.style,
                    StyleName::Vfd,
                    "the skin travels as a name, never as colors",
                );
            }
            other => panic!("expected a split-flap board, got {other:?}"),
        }
        match preem_of(date, DATE_ID) {
            PreemWidget::DotMatrix { config, state } => {
                assert_eq!(state.text, "SAT 11 JUL");
                assert_eq!(config.dot_px, 4);
                assert_eq!(config.style.style, StyleName::Vfd);
            }
            other => panic!("expected a dot-matrix line, got {other:?}"),
        }
        match preem_of(sweep, SECONDS_ID) {
            PreemWidget::LedStrip { config, state } => {
                assert!((state.level - 0.05).abs() < f32::EPSILON, "{}", state.level);
                assert_eq!(
                    state.peak, None,
                    "no peak dot: this is a sweep, not a meter"
                );
                assert_eq!(config.leds, 20);
                assert_eq!(config.peak_hold, None);
                assert_eq!(config.style.style, StyleName::Vfd);
            }
            other => panic!("expected an LED strip, got {other:?}"),
        }
    }

    /// Every reading on the card is the **pushed** `Clock` state's — the time,
    /// the date and the seconds all move with the snapshot and with nothing
    /// else, and the seed (before any snapshot) is the all-dash face.
    #[test]
    fn the_card_reads_time_date_and_seconds_off_the_pushed_clock() {
        let mut model = fresh();
        assert_eq!(
            card_readings(&model),
            (NO_CLOCK.to_owned(), NO_DATE.to_owned(), 0.0),
            "the seed shows dashes and a dark sweep, not a guess",
        );

        for (iso, face, date, lit) in [
            ("2026-07-11T15:49:00+02:00", "15:49", "SAT 11 JUL", 1_u16),
            ("2026-07-11T15:49:02+02:00", "15:49", "SAT 11 JUL", 1),
            ("2026-07-11T15:49:03+02:00", "15:49", "SAT 11 JUL", 2),
            ("2026-07-11T15:49:29.5+02:00", "15:49", "SAT 11 JUL", 10),
            ("2026-07-11T15:49:59+02:00", "15:49", "SAT 11 JUL", 20),
            ("2026-09-25T08:05:30+02:00", "08:05", "FRI 25 SEP", 11),
        ] {
            model.update(clock_snapshot(iso, 0));
            let (got_face, got_date, level) = card_readings(&model);
            assert_eq!(got_face, face, "{iso}");
            assert_eq!(got_date, date, "{iso}");
            assert!(
                (level - f32::from(lit) / 20.0).abs() < f32::EPSILON,
                "{iso}: {level} is not {lit} of 20 segments",
            );
        }
    }

    /// **The date is local, and it rolls over at local midnight** — the one
    /// reading on the card that `unix` would get wrong.
    ///
    /// Each pair is a real minute boundary with the host's matching `unix`
    /// value: at `+02:00`, `00:00:00` on the 12th is still 22:00 on the 11th in
    /// UTC, so a date line projected from `unix` stays on the 11th across the
    /// flip. Only the ISO timestamp's own date field moves. The year boundary
    /// is the same test with every field rolling at once.
    #[test]
    fn the_date_line_rolls_over_at_local_midnight() {
        let mut model = fresh();
        for [
            (before_iso, before_unix, before),
            (after_iso, after_unix, after),
        ] in [
            [
                ("2026-07-11T23:59:59+02:00", 1_783_807_199, "SAT 11 JUL"),
                ("2026-07-12T00:00:00+02:00", 1_783_807_200, "SUN 12 JUL"),
            ],
            [
                ("2026-12-31T23:59:59+01:00", 1_798_757_999, "THU 31 DEC"),
                ("2027-01-01T00:00:00+01:00", 1_798_758_000, "FRI 01 JAN"),
            ],
        ] {
            model.update(clock_snapshot(before_iso, before_unix));
            let (face, date, _) = card_readings(&model);
            assert_eq!((face.as_str(), date.as_str()), ("23:59", before));

            model.update(clock_snapshot(after_iso, after_unix));
            let (face, date, _) = card_readings(&model);
            assert_eq!(
                (face.as_str(), date.as_str()),
                ("00:00", after),
                "the board and the date line flip together at local midnight",
            );
        }
    }

    /// The date projection on its own: weekdays across the calendar's awkward
    /// corners, and the placeholder for everything that names no real date.
    #[test]
    fn the_date_line_names_real_dates_and_dashes_anything_else() {
        // Weekdays checked against `date -d … +%a`.
        assert_eq!(date_line("1970-01-01T00:00:00Z"), "THU 01 JAN");
        assert_eq!(date_line("2000-03-01T00:00:00Z"), "WED 01 MAR");
        assert_eq!(date_line("2026-01-05T12:00:00+01:00"), "MON 05 JAN");
        assert_eq!(date_line("2028-02-29T12:00:00+01:00"), "TUE 29 FEB");
        // A day the month does not have, in a leap-year-shaped trap.
        assert_eq!(date_line("2026-02-29T12:00:00+01:00"), NO_DATE);
        assert_eq!(date_line("2100-02-29T12:00:00Z"), NO_DATE);
        assert_eq!(date_line("2026-13-01T12:00:00Z"), NO_DATE);
        assert_eq!(date_line("2026-00-10T12:00:00Z"), NO_DATE);
        assert_eq!(date_line("2026-07-00T12:00:00Z"), NO_DATE);
        // The pre-snapshot seed, and shapes that are not a timestamp at all.
        assert_eq!(date_line("—"), NO_DATE);
        assert_eq!(date_line(""), NO_DATE);
        assert_eq!(date_line("2026-07-11"), NO_DATE);
        assert_eq!(date_line("2026/07/11T15:49:00Z"), NO_DATE);
        assert_eq!(date_line("2026-07-1１T15:49:00Z"), NO_DATE);
        // Every real date is the placeholder's width, so the line never
        // changes proportions (#1387's height-for-width) under a reading.
        assert_eq!(date_line("2026-07-11T15:49:00Z").len(), NO_DATE.len());
    }

    /// One date in every month, plus the 400-year leap rule and `weekday`'s
    /// year-0000 guard — each checked against `date -u -d … +'%a %d %b'`.
    ///
    /// The #1409 review's test: the one above reaches only six months, so a
    /// typo in `MONTHS` or in `weekday`'s offset table for any of the other
    /// six, a dropped `% 400` clause or a dropped `+ 400` shift all shipped
    /// green before it.
    #[test]
    fn the_date_line_names_every_month_and_the_400_year_rule() {
        for (iso, want) in [
            ("2026-01-15T12:00:00+01:00", "THU 15 JAN"),
            ("2026-02-15T12:00:00+01:00", "SUN 15 FEB"),
            ("2026-03-15T12:00:00+01:00", "SUN 15 MAR"),
            ("2026-04-15T12:00:00+02:00", "WED 15 APR"),
            ("2026-05-15T12:00:00+02:00", "FRI 15 MAY"),
            ("2026-06-15T12:00:00+02:00", "MON 15 JUN"),
            ("2026-07-15T12:00:00+02:00", "WED 15 JUL"),
            ("2026-08-15T12:00:00+02:00", "SAT 15 AUG"),
            ("2026-09-15T12:00:00+02:00", "TUE 15 SEP"),
            ("2026-10-15T12:00:00+02:00", "THU 15 OCT"),
            ("2026-11-15T12:00:00+01:00", "SUN 15 NOV"),
            ("2026-12-15T12:00:00+01:00", "TUE 15 DEC"),
            // divisible by 100 *and* 400: a leap year (2100 is the other half)
            ("2000-02-29T12:00:00+01:00", "TUE 29 FEB"),
            // `weekday`'s +400 shift: year 0000's January must not underflow
            ("0000-01-01T00:00:00Z", "SAT 01 JAN"),
        ] {
            assert_eq!(date_line(iso), want, "{iso}");
        }
    }

    /// The sweep's projection on its own: 1-based three-second segments, dark
    /// without readable seconds, and a leap second pinned to the last segment.
    #[test]
    fn the_sweep_lights_the_segment_the_second_is_in() {
        assert_eq!(sweep_lit("2026-07-11T15:49:00+02:00"), 1);
        assert_eq!(sweep_lit("2026-07-11T15:49:05+02:00"), 2);
        assert_eq!(sweep_lit("2026-07-11T15:49:57+02:00"), 20);
        assert_eq!(sweep_lit("2016-12-31T23:59:60Z"), 20, "a leap second");
        assert_eq!(sweep_lit("—"), 0);
        assert_eq!(sweep_lit("2026-07-11T15:49"), 0);
        assert_eq!(sweep_lit("2026-07-11T15:49-00Z"), 0);
        assert_eq!(sweep_lit("2026-07-11Txx:49:05Z"), 0);
        assert_eq!(sweep_lit("2026-07-11T15:49:0５Z"), 0);
    }

    /// **A click anywhere on the card opens this plugin's own page, and that is
    /// all it does** — exactly one `OpenPage(PluginSelf)`, not the power menu
    /// the card's old button opened, and nothing added beside it.
    #[test]
    fn the_card_click_opens_this_plugins_own_page_and_nothing_else() {
        let mut card = fresh();
        card.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1_783_777_740));
        let effects = card.update(Input::event(CARD_BTN, EventKind::Click));
        assert_eq!(effects, vec![Effect::OpenPage(Page::PluginSelf)]);
        assert!(
            !effects.contains(&Effect::OpenPage(Page::PowerMenu)),
            "the power menu is not this card's business any more",
        );
        // The rows inside the button are not click targets of their own.
        for row in [FACE_ID, TIME_ID, DATE_ID, SECONDS_ID] {
            assert!(
                card.update(Input::event(row, EventKind::Click)).is_empty(),
                "{row}",
            );
        }
        // …and a scroll over the card changes nothing either.
        let scroll = EventKind::Scroll { dx: 0.0, dy: 1.0 };
        assert!(card.update(Input::event(CARD_BTN, scroll)).is_empty());
    }

    /// The card's click lands on the same page the chip's does: one builder,
    /// so the drawer and the dialog show the same thing for the same clock.
    #[test]
    fn the_card_opens_the_chips_page() {
        let (mut card, mut chip) = (fresh(), fresh_bar());
        for model in [&mut card, &mut chip] {
            model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1_783_777_740));
        }
        for mode in [RenderMode::Raster, RenderMode::State] {
            let card_page = with_render_mode(mode, || card.view().panel);
            let chip_page = with_render_mode(mode, || chip.view().panel);
            assert!(card_page.is_some(), "{mode:?}");
            assert_eq!(card_page, chip_page, "{mode:?}");
        }
    }

    /// Every id in `node`'s subtree, in tree order.
    fn ids(node: &Node) -> Vec<String> {
        match node {
            Node::Button { id, child, .. } => {
                let mut out = vec![id.clone()];
                out.extend(ids(child));
                out
            }
            Node::Box { id, children, .. } => {
                let mut out: Vec<String> = id.iter().cloned().collect();
                out.extend(children.iter().flat_map(ids));
                out
            }
            Node::Preem { id, .. } | Node::Pixels { id, .. } => id.iter().cloned().collect(),
            _ => Vec::new(),
        }
    }

    /// **The card's node ids are stable across renders** — the precondition
    /// for the shell keeping one renderer per widget, and so for the minute
    /// flip animating at all: a board whose id moved between two renders is a
    /// new board, built at rest on the new face.
    ///
    /// Two renders a minute, a day and some seconds apart, in both modes,
    /// against the literal ids.
    #[test]
    fn the_card_node_ids_are_stable_across_renders() {
        let mut model = fresh();
        let expected = [
            "clock-demo-card",
            "clock-demo-face",
            "clock-demo-time",
            "clock-demo-date",
            "clock-demo-seconds",
        ];
        for mode in [RenderMode::State, RenderMode::Raster] {
            model.update(clock_snapshot("2026-07-11T23:59:07+02:00", 1_783_807_147));
            let first = ids(&with_render_mode(mode, || model.view().tree));
            model.update(clock_snapshot("2026-07-12T00:00:41+02:00", 1_783_807_241));
            let second = ids(&with_render_mode(mode, || model.view().tree));
            assert_eq!(first, expected, "{mode:?}");
            assert_eq!(second, first, "{mode:?}");
        }
    }

    /// A click on a node we don't own is ignored (no spurious effect).
    #[test]
    fn click_on_unknown_node_is_ignored() {
        let mut model = fresh();
        let effects = model.update(Input::event("not-ours", EventKind::Click));
        assert!(effects.is_empty());
    }

    // ── The bar chip ────────────────────────────────────────────────────────

    /// `short_time` projects RFC3339 → `HH:MM`, and degrades gracefully on any
    /// malformed input rather than panicking.
    #[test]
    fn short_time_extracts_hh_mm() {
        assert_eq!(short_time("2026-07-11T15:49:00+02:00"), "15:49");
        assert_eq!(short_time("2026-07-11T00:00:00Z"), "00:00");
        // No 'T' → raw passthrough (the "—" seed and any odd value survive).
        assert_eq!(short_time("—"), "—");
        assert_eq!(short_time("no-time-here"), "no-time-here");
        // Truncated after 'T' → passthrough, never an out-of-bounds slice.
        assert_eq!(short_time("2026-07-11T15"), "2026-07-11T15");
    }

    /// The chip's face is `HH:MM` when the host's timestamp projects to one and
    /// the all-dash placeholder otherwise — including the pre-snapshot seed,
    /// which has no seven-segment glyph at all (#884).
    #[test]
    fn the_chip_face_falls_back_to_dashes() {
        assert_eq!(clock_face("2026-07-11T15:49:00+02:00"), "15:49");
        assert_eq!(clock_face("2026-07-11T00:00:00Z"), "00:00");
        // The seed — one codepoint with no seven-segment glyph, so without the
        // fallback the chip is a single dark cell.
        assert_eq!(clock_face("—"), NO_CLOCK);
        // …and the shapes `short_time` passes through raw, which at 40 px a
        // character would be a chip several hundred pixels wide.
        assert_eq!(clock_face("no-time-here"), NO_CLOCK);
        assert_eq!(clock_face("2026-07-11T15"), NO_CLOCK);
        // Right length, wrong shape.
        assert_eq!(clock_face("2026-07-11Txx:xx:00Z"), NO_CLOCK);
        assert_eq!(clock_face("2026-07-11T15-49:00Z"), NO_CLOCK);
        // A non-ASCII digit is not a digit: `b[..2]` slices bytes, so this also
        // pins that the check can't be fooled into indexing a wide codepoint.
        assert_eq!(clock_face("2026-07-11T１5:49:00Z"), NO_CLOCK);
    }

    /// The core signal against **today's** shell (#884): a snapshot updates the
    /// model, and a bar instance's `view` renders the exact compact chip the
    /// host will reconcile — a rasterised seven-segment readout
    /// **byte-identical** to the `preem::seven_seg(…).into_node(…)` a plugin
    /// author writes by hand.
    ///
    /// That equality is the migration's compat promise: this chip must reach an
    /// un-advertising shell as the same pixels it would have had if #884 had
    /// never happened, and only comparing the buffers proves it.
    #[test]
    fn against_an_old_shell_the_chip_is_a_rasterised_seven_seg() {
        let mut model = fresh_bar();
        let effects = model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1_752_241_740));
        assert!(effects.is_empty());
        assert_eq!(model.iso, "2026-07-11T15:49:00+02:00");
        assert_eq!(model.unix, 1_752_241_740);

        let expected = Node::Box {
            id: Some("clock-demo-chip".to_owned()),
            dir: Dir::Horizontal,
            spacing: 4,
            scroll: false,
            classes: vec![],
            children: vec![Node::Button {
                id: "clock-demo-chip-btn".to_owned(),
                classes: vec![],
                child: Box::new(
                    seven_seg("15:49", DisplayStyle::Vfd).into_node(Some(CHIP_TIME_ID), vec![]),
                ),
            }],
            tooltip: None,
        };
        // `==` rather than `assert_eq!`: the operands carry a `Node::Pixels`,
        // whose own `Debug` would dump the whole RGBA buffer into the failure
        // output.
        let tree = with_render_mode(RenderMode::Raster, || model.view().tree);
        assert!(
            tree == expected,
            "the raster chip must match the kit by hand"
        );
    }

    /// …and against a shell that advertises the preem vocabulary, the *same*
    /// `view` ships the typed state node instead — same id, same reading, no
    /// pixels anywhere in the tree (#884).
    #[test]
    fn against_a_preem_shell_the_same_chip_is_a_state_node() {
        let mut model = fresh_bar();
        model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1_752_241_740));

        let tree = with_render_mode(RenderMode::State, || model.view().tree);
        let Node::Box { children, .. } = &tree else {
            panic!("the chip root is a Box")
        };
        let [Node::Button { child, .. }] = children.as_slice() else {
            panic!("the chip holds exactly the click target")
        };
        match child.as_ref() {
            Node::Preem { id, widget, .. } => {
                assert_eq!(id.as_deref(), Some(CHIP_TIME_ID), "the reconciler's key");
                match widget.as_ref() {
                    PreemWidget::SevenSeg { config, state } => {
                        assert_eq!(state.text, "15:49", "the plugin's own reading");
                        assert_eq!(
                            config.style.style,
                            StyleName::Vfd,
                            "the skin travels as a name, never as colors",
                        );
                    }
                    other => panic!("expected a seven-seg widget, got {other:?}"),
                }
            }
            Node::Pixels { .. } => panic!("a preem-speaking host must not get pixels"),
            other => panic!("expected Node::Preem, got {other:?}"),
        }
    }

    /// #349 PR2: a click on the chip button opens the plugin's own page, and
    /// that page projects the full `ClockState` (a tree distinct from the
    /// chip).
    #[test]
    fn the_chips_page_renders_the_full_clock() {
        let mut model = fresh_bar();
        model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1_752_241_740));

        let expected_panel = Node::Box {
            id: Some("clock-demo-page".to_owned()),
            dir: Dir::Vertical,
            spacing: 6,
            scroll: false,
            classes: vec![],
            children: vec![
                Node::Label {
                    id: Some("clock-demo-page-iso".to_owned()),
                    text: "2026-07-11T15:49:00+02:00".to_owned(),
                    classes: vec!["title-2".to_owned()],
                    tooltip: None,
                },
                Node::Label {
                    id: Some("clock-demo-page-unix".to_owned()),
                    text: "unix: 1752241740".to_owned(),
                    classes: vec!["dim-label".to_owned()],
                    tooltip: None,
                },
            ],
            tooltip: None,
        };
        // The page is plain GTK either way — the seam is per widget, not a mode
        // the plugin enters — so pin it in *both* render modes.
        for mode in [RenderMode::Raster, RenderMode::State] {
            let panel = with_render_mode(mode, || model.view().panel);
            assert_eq!(panel, Some(expected_panel.clone()), "{mode:?}");
        }
    }

    // ── Both surfaces ───────────────────────────────────────────────────────

    /// A snapshot whose `clock` is `None` (startup window) changes nothing on
    /// either surface — the runtime's tree dedup then sends no frame for it.
    #[test]
    fn snapshot_without_clock_changes_nothing() {
        for mut model in [fresh(), fresh_bar()] {
            let arm = if model.is_bar { "bar" } else { "sidebar" };
            for mode in [RenderMode::Raster, RenderMode::State] {
                let before = with_render_mode(mode, || model.view());
                let effects = model.update(Input::Snapshot(StateSnapshot::default()));
                assert!(effects.is_empty());
                // `==` rather than `assert_eq!`: in raster mode the chip's view
                // carries a `Node::Pixels`, whose `Debug` would dump the buffer.
                assert!(
                    with_render_mode(mode, || model.view()) == before,
                    "{arm} / {mode:?}",
                );
            }
        }
    }

    /// The `Register` frame built from this plugin's manifest is valid on the
    /// wire, and declares the #882 vocabulary negotiation — which is what makes
    /// the host send the `Hello` that unlocks the state arm above.
    ///
    /// One frame for both surfaces, because there is one manifest: an instance
    /// registers the same way whichever tree it goes on to render.
    #[test]
    fn the_register_frame_round_trips_and_negotiates() {
        let reg = PluginMsg::Register {
            manifest: ClockDemo::manifest(),
        };
        let back: PluginMsg = decode(&encode(&reg)).expect("register frame decodes");
        assert_eq!(reg, back);

        let PluginMsg::Register { manifest } = &reg else {
            panic!("built as a Register frame")
        };
        assert!(manifest.negotiates_vocab());
    }

    /// The `Render` frame each surface produces is valid on the wire, in both
    /// render modes (#884): the typed state node has to survive the codec
    /// exactly as the rasterised buffer already did, since that frame is the
    /// only thing the shell ever sees.
    #[test]
    fn both_surfaces_render_frames_round_trip_in_both_modes() {
        for mut model in [fresh(), fresh_bar()] {
            let arm = if model.is_bar { "bar" } else { "sidebar" };
            let _ = model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1));
            for mode in [RenderMode::Raster, RenderMode::State] {
                // Both arms' frames are panel-bearing: the chip or the card,
                // plus the page either one opens (#349, #1408).
                let view = with_render_mode(mode, || model.view());
                let render = PluginMsg::Render {
                    tree: view.tree,
                    panel: view.panel.map(Box::new),
                    hidden_on: view.hidden_on,
                    effects: vec![Effect::OpenPage(Page::PluginSelf)],
                };
                let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
                assert!(render == back, "{arm} / {mode:?}");
            }
        }
    }
}
