//! The bridge's **plugin face** 🌉 — a bar chip reporting what the daemon is
//! doing, and (since #1236) what is left of the Claude account's rate limits,
//! plus a drawer panel with the whole list (issues #866, #957, #1236).
//!
//! # Two hats, one binary — and the HTTP hat is the one that matters
//!
//! This crate keeps its original job: serve `POST /v1/chat/completions` on a
//! same-uid socket (#993) so pet and caw can ride a Claude subscription
//! unchanged. #866 adds
//! a second hat — the daemon now *also* speaks the widget-plugin protocol, so it
//! rides `programs.trollshell.plugins` (and therefore the launcher, the
//! control-center's Plugins tab, and #392's keyring injection) instead of
//! needing its own hand-declared systemd unit. That is exactly the shape
//! `hytte-plugin-infobroker` already has: a real daemon that happens to paint a
//! chip.
//!
//! It differs from the infobroker in **which duty owns the process**, and the
//! difference is deliberate. The infobroker starts its socket server from
//! [`Plugin::sources`], so its server's life is one plugin session. The bridge
//! must not do that: its clients are other plugins making paid/metered calls,
//! and an HTTP endpoint that only exists while the *shell* is up would turn
//! every shell restart into a wave of 502s in pet and caw. So `main` binds and
//! serves the listener on its own multi-thread runtime **before** entering the
//! SDK's [`run`](hytte_plugin::run) loop, and the SDK's dial/backoff then
//! governs only the chip. If `XDG_RUNTIME_DIR` is unset there is no host socket
//! to dial at all, and `main` parks on the HTTP runtime rather than exiting —
//! the API stays up with no chip.
//!
//! The **usage poll** ([`crate::usage`]) lives on that same HTTP runtime, for
//! the same reason: the numbers keep arriving while the shell is down, and the
//! chip's tick reads whatever the last poll left on the board.
//!
//! The two runtimes never share anything but [`crate::status`]'s atomics and
//! [`crate::usage`]'s board.
//!
//! # What the chip says
//!
//! The Claude glyph, a health glyph, the mode, an optional key glyph, coarse
//! counts, and then one compact level meter per **active** rate limit (at most
//! [`MAX_CHIP_METERS`], in the server's own order — session and the weekly
//! bucket, today):
//!
//! ```text
//! [✳] [✓] api [🔑] 12/1 [▮▮▮▮▮▯] [▮▮▯▯▯▯]
//! ```
//!
//! - the **Claude glyph** ([`CLAUDE_ICON`]) leads, so the pill is identifiable as
//!   *this* daemon's before anyone parses the rest (#957, Annika's ask);
//! - the **glyph** is the last request's outcome (nothing served yet / 2xx / not);
//! - the **mode** is `sub` / `rep` / `api` — which backend is answering;
//! - the **key glyph** appears only when the bridge holds an outbound credential
//!   of its own, i.e. in `api` mode. Its *absence* is the informative case: it
//!   means no key is held and `claude` owns the subscription session. The key
//!   itself never reaches this module — [`crate::status::Startup::keyed`] is a
//!   boolean;
//! - the **counts** are `<2xx>/<not-2xx>`, hidden until something has been served;
//! - the **meters** are `limits[]`, coloured by the server's own `severity`, each
//!   with its own hover. They are the one part of the chip that can be *absent*:
//!   no login, an expired token, a network hiccup, or an account the endpoint
//!   reports no active limits for all render as no meters — and the chip itself
//!   never disappears, because the health readout is still true.
//!
//! # And what the hover says
//!
//! All of it, in words — [`tooltip`] on the root box:
//!
//! ```text
//! Claude bridge · subscription · 18 served, 0 failed
//! ```
//!
//! This is the #957 fix. Mara ran the chip for weeks reading `sub 18/0` without
//! knowing what it meant, and she was right to: every part of it is an
//! abbreviation whose key lived only in this module doc. A **failed** usage poll
//! appends its one-line sentence on a second line, which is where "usage stale —
//! run `claude` once to refresh the login" is said; a *successful* poll adds
//! nothing there, because the meters and their own hovers say it better.
//!
//! Each meter carries its own tooltip — `Session (5 h): 80% — resets in 2 h
//! 15 min` — on the [`Node::Box`] wrapping it, since
//! [`Node::Preem`](hytte_plugin::proto::Node::Preem) carries no tooltip field of
//! its own. GTK resolves a hover against the deepest widget under the pointer,
//! so a meter's hover wins over the root's wherever the pointer is actually on
//! one.
//!
//! # The panel (#1236)
//!
//! Clicking the chip emits `Effect::OpenPage(Page::PluginSelf)` and the host
//! opens [`panel`] in the drawer: **every** row in `limits[]`, active or not
//! (inactive ones greyed and said so), each with its label, a full-width bar,
//! the percent, the reset time as both an absolute UTC stamp and a "in 2 h
//! 15 min", and the account's extra-usage allowance when it is enabled. The
//! header carries the last-fetched time, or the failure sentence when the last
//! poll produced nothing **and there is nothing left over from a previous
//! one** — since #1283, a failed poll that still has [`usage::Report::last_ok`]
//! numbers renders that same list, with the failure sentence appended as a
//! footer rather than replacing it (see [`failure_footer`]).
//!
//! This is why the manifest now requests
//! [`Capability::OpenPage`](hytte_plugin::proto::Capability::OpenPage) — the one
//! capability it asks for. It still brokers no other effect, subscribes to no
//! host state, and provides no datasource.
//!
//! # Rendering whatever the server sends (Mara, #1236)
//!
//! Nothing here is keyed on a limit name. The rows are the list the endpoint
//! returned; the label comes from [`usage::humanise_kind`], which falls through
//! to the raw kind; the colour comes from [`severity_role`] /
//! [`severity_class`], which fall through to "normal". A bucket Anthropic adds
//! renders on the next poll with no code change, and one they rename does not
//! blank the chip.
//!
//! # A sidebar mount gets a card instead (#1280 P1)
//!
//! Annika's #1280 triage put a second surface on the roadmap — one card per
//! Claude subscription — and settled the scope for this slice on the issue
//! (2026-09-15): "just wrap it in a card so it looks better … we'll get back
//! to the multisubscription stuff again later". So this adds no second
//! socket and no config file, only a mount-family switch —
//! `hytte-plugin-stats`'s `Mount::is_bar` split, applied here the same way. A
//! launch whose *effective* mount (`HYTTE_PLUGIN_MOUNT`, resolved a second
//! time here — see [`effective_mount`] — because the SDK deliberately never
//! tells a plugin its own override) is one of the three bar regions gets
//! exactly what shipped before: the chip and its drawer panel, unchanged. Any
//! of the six sidebar mounts instead gets [`card`]: the same title-plus-rows
//! [`usage_card`] container the drawer panel now uses too, so "wrap it in a
//! card" improves both surfaces from one function. The title is
//! [`DEFAULT_TITLE`] unless `CLAUDE_BRIDGE_LABEL` is set to something
//! non-blank — free, since the process already has its own environment, and
//! deliberately not a second `/api/oauth/profile` request (the triage's P2,
//! still open). Neither switch is live-reloadable; both are read once at
//! [`Plugin::init`], the same "a deployment decision, not a config value"
//! argument `hytte-plugin-stats` makes for its own family split.

use std::time::Duration;

use hytte_plugin::display::{AccentRole, LedStrip, StyleName};
use hytte_plugin::proto::{Capability, Dir, Effect, EventKind, Manifest, Mount, Node, Page};
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View, tick_stream};

use crate::Mode;
use crate::status::{self, Last, Startup, Status};
use crate::usage::{self, ExtraUsage, Limit, Outcome, Report, Usage, UsageError};

/// Stable plugin id — the host's mount-slot key, and the `<id>` in the
/// `trollshell-plugin-<id>.service` transient unit the launcher spawns.
const PLUGIN_ID: &str = "claude-bridge";

/// The chip's inner box — the node every glyph and meter hangs off, and the
/// carrier of the whole-pill tooltip.
const ROOT_ID: &str = "claude-bridge-root";

/// The chip button. Clicking it opens the drawer panel (#1236); before that the
/// chip was deliberately inert.
const CHIP_BTN: &str = "claude-bridge-chip";

/// The drawer panel's root — the tree's own top-level id, unchanged by the
/// #1280 P1 card wrapper (see [`usage_card`]): the wrapper's one new node is
/// nested *inside* this id, not above it.
const PANEL_ROOT_ID: &str = "claude-bridge-panel";

/// The list [`usage_card`] nests inside [`PANEL_ROOT_ID`] — the node the
/// wrapper adds. Everything that rendered directly under `PANEL_ROOT_ID`
/// before #1280 P1 (the header, every row, the footer) renders here instead,
/// with every one of *its own* ids untouched.
const PANEL_LIST_ID: &str = "claude-bridge-panel-list";

/// The sidebar card's root (#1280 P1) — the mount-family sibling of
/// [`PANEL_ROOT_ID`], shown instead of the panel on a sidebar mount and never
/// alongside it (see [`Plugin::view`]).
const CARD_ROOT_ID: &str = "claude-bridge-card";

/// The list [`usage_card`] nests inside [`CARD_ROOT_ID`] — [`PANEL_LIST_ID`]'s
/// sidebar sibling.
const CARD_LIST_ID: &str = "claude-bridge-card-list";

/// The card class both surfaces carry (#1280 P1) — Annika's "just wrap it in a
/// card so it looks better": the drawer panel's own surface (`.ts-drawer-content`)
/// otherwise ships no card treatment of its own, and the sidebar host wrapper
/// (`.ts-plugin-card`, #319) deliberately ships no padding, following the
/// `ts-agents-card`/stats naming a plugin's own card class already uses for
/// the same reason.
const CARD_CLASS: &str = "ts-usage-card";

/// The Claude glyph the chip leads with (#957): the eight-spoked asterisk `✳`
/// the `claude` CLI prompts with — a generic dingbat, deliberately **not**
/// Anthropic's wordmark or logo.
///
/// Unlike every other icon this plugin names, it is not an Adwaita symbolic: it
/// ships with the *shell*, as `assets/trollshell/icons/claude-symbolic.svg`, and
/// resolves because the shell puts that directory on its `GtkIconTheme` search
/// path (`trollshell::assets::install_icon_search_path`). A host that hasn't
/// done so — or a plugin run against some other shell — renders `image-missing`
/// here and loses nothing else: the health glyph, mode and counts are
/// independent nodes.
const CLAUDE_ICON: &str = "claude-symbolic";

/// Where the manifest mounts when the launch says nothing — today's only
/// deployed shape, and unaffected by #1280 P1: nothing changes here unless an
/// operator sets [`MOUNT_ENV`] to a sidebar mount.
const DEFAULT_MOUNT: Mount = Mount::BarRight;

/// The launch-time placement variable, as `docs/plugin-env.md` documents it and
/// `hytte_plugin::run` reads it.
///
/// `hytte_plugin::run` already resolves this for the host-facing `Register`
/// frame and deliberately never tells the plugin (see `hytte-plugin-stats`'s
/// `mount` module for the full "why read it twice" rationale) — so a plugin
/// that needs its own placement-dependent decision, here chip-vs-card, reads
/// its own copy. The variable is spelled as a literal rather than imported:
/// the SDK's own constant is private, and this is a second, independent
/// reader of the same documented contract.
const MOUNT_ENV: &str = "HYTTE_PLUGIN_MOUNT";

/// The env var that lets an operator label the usage surfaces' title without
/// a second `/api/oauth/profile` request — the triage's P2, deliberately
/// deferred (Annika, #1280, 2026-09-15: "just wrap it in a card so it looks
/// better … we'll get back to the multisubscription stuff again later").
const LABEL_ENV: &str = "CLAUDE_BRIDGE_LABEL";

/// The title both the sidebar card and the drawer panel head with, absent an
/// operator-set [`LABEL_ENV`].
const DEFAULT_TITLE: &str = "Claude usage";

/// The mount this instance actually registered on: the launch override when
/// one is set and parses, else `manifest_mount`.
///
/// `lookup` is injected rather than read from the process because
/// `unsafe_code = "forbid"` rules out `std::env::set_var` (an `unsafe fn` in
/// edition 2024), so a test that drove the real environment could not exist
/// at all — the same reason `hytte-plugin-stats`'s `mount::effective` takes
/// one, and the same shape.
///
/// Reading it twice is safe in the only way that matters: this is **strictly
/// less permissive than the SDK's parser can be**, because the SDK has
/// already refused the launch outright for any value it could not parse. By
/// the time anything here runs, `HYTTE_PLUGIN_MOUNT` is either unset or a
/// valid wire mount name — the fallback-to-`manifest_mount` arm below is
/// unreachable in a live process and exists so this function is total and
/// testable (`hytte-plugin-stats::mount`'s own doc makes the same claim for
/// the identical shape; #1315's review found it survived only in this
/// crate's test docstrings, not on the function itself).
fn effective_mount(manifest_mount: Mount, lookup: &dyn Fn(&str) -> Option<String>) -> Mount {
    lookup(MOUNT_ENV)
        .as_deref()
        .map(str::trim)
        .and_then(Mount::from_wire_name)
        .unwrap_or(manifest_mount)
}

/// The title an operator asked for: [`LABEL_ENV`], trimmed, when it is set and
/// non-blank, else [`DEFAULT_TITLE`]. Pure, for the same testability reason
/// [`effective_mount`] takes an injected `lookup`.
fn card_title(lookup: &dyn Fn(&str) -> Option<String>) -> String {
    lookup(LABEL_ENV)
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| DEFAULT_TITLE.to_owned(), str::to_owned)
}

/// The real process environment — the one place this module reads one, so
/// "what does this launch say?" has a single answer and every pure function
/// above takes it as a parameter.
fn env_lookup(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// Both of [`Plugin::init`]'s launch-dependent fields, `(is_bar, title)`, in
/// one function [`Plugin::init`] is the only caller of (#1315 review, MED 3).
///
/// `effective_mount` and `card_title` are well covered on their own — the
/// gap this closes is the two lines that actually *thread* them into the
/// model, which the review found unpinned: hardcoding either field in
/// `init` (`is_bar: true`, or `title: DEFAULT_TITLE.to_owned()`) left every
/// test green, because nothing exercised `init`'s own composition rather
/// than the pure halves it calls. Pulling that composition out here, with
/// `init` doing nothing but destructuring the result, means a test against
/// *this* function is a test against `init` — there is no second place for
/// the wiring to live that a test could miss (`hytte-plugin-stats::plugin`'s
/// `settings_from` is the same shape, for the same reason).
fn resolve_settings(
    manifest_mount: Mount,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> (bool, String) {
    (
        effective_mount(manifest_mount, lookup).is_bar(),
        card_title(lookup),
    )
}

/// How often the chip re-reads [`crate::status`] and [`crate::usage`]'s board.
///
/// A status readout, not a clock: 5 s is fast enough that a run of failures
/// shows up while somebody is still looking at it, and slow enough to cost
/// nothing. The SDK dedups identical trees, so a tick that changes nothing sends
/// no frame at all. Note this is **not** the usage poll's cadence
/// ([`usage::POLL_EVERY`], five minutes) — this tick only reads a board the
/// other runtime fills.
const POLL: Duration = Duration::from_secs(5);

/// How many meters the chip will carry, however many active limits the server
/// reports.
///
/// A bar chip sits in a shared region with every other chip and each meter is
/// ~70 px wide, so this is a width budget, not a judgement about which limits
/// matter: the **panel** shows all of them. Two is what today's response makes
/// meaningful anyway (a session bucket and a weekly one).
pub const MAX_CHIP_METERS: usize = 2;

/// Segments per chip meter. Six reads as a level at a glance and keeps one meter
/// near 70 px at the kit's 8 px cells; the exact number is the panel's job.
const CHIP_LEDS: u32 = 6;

/// The chip meters' skin. VFD's pale-cyan-on-near-black is the same readout skin
/// the timer chip uses, and its ink is what [`severity_role`] re-tints.
const CHIP_STYLE: StyleName = StyleName::Vfd;

/// The chip's only message: re-read the boards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tick;

/// The chip's whole model — the last reading of each board. Everything else
/// lives in [`crate::status`]'s atomics and [`crate::usage`]'s mutex, written by
/// the HTTP half.
struct BridgeChip {
    status: Status,
    /// The last usage report, or `None` before the first poll completes.
    usage: Option<Report>,
    /// The [`usage::version`] `usage` was read at, so a tick that has nothing new
    /// to read skips the lock and the clone — 59 ticks in 60, given a five-minute
    /// poll and a five-second tick.
    usage_version: u64,
    /// The mount family (#1280 P1): `true` on a bar mount, where
    /// [`Plugin::view`] paints the chip and its panel; `false` on a sidebar
    /// mount, where it paints the card ([`card`]) alone. Resolved once from
    /// the launch's [`effective_mount`] — a deployment decision, not
    /// something re-read every tick, exactly as `hytte-plugin-stats`'s
    /// `Family::of` is (and on the same `Mount::is_bar` split the host itself
    /// uses).
    is_bar: bool,
    /// The title both surfaces head with — [`card_title`]'s resolved value,
    /// read once for the same reason `is_bar` is.
    title: String,
}

impl BridgeChip {
    /// Re-read the usage board if anything has been published since the last
    /// read.
    fn refresh_usage(&mut self) {
        let version = usage::version();
        if version != self.usage_version {
            self.usage_version = version;
            self.usage = usage::latest();
        }
    }

    /// Fold one user interaction. The chip click is the only one there is.
    fn on_event(node: &str, kind: &EventKind) -> Vec<Effect> {
        match (kind, node) {
            (EventKind::Click, CHIP_BTN) => vec![Effect::OpenPage(Page::PluginSelf)],
            _ => Vec::new(),
        }
    }
}

impl Plugin for BridgeChip {
    type Msg = Tick;
    /// Purely a readout: it issues no I/O of its own, so it has no commands. The
    /// usage poll is `main`'s, on the HTTP runtime, not a plugin command.
    type Cmd = std::convert::Infallible;

    /// Mounts [`DEFAULT_MOUNT`] — unchanged from the plugin's first shape, and
    /// still the id/mount a launch that sets no `HYTTE_PLUGIN_MOUNT` gets.
    /// [`Plugin::view`] is what actually decides chip-vs-card, off the launch's
    /// *effective* mount (#1280 P1) rather than this one. Subscribes to
    /// nothing (both surfaces are driven by their own tick off local boards,
    /// not by host state) and requests exactly one capability —
    /// [`Capability::OpenPage`], for the drawer panel (#1236) a bar-family
    /// instance publishes; a sidebar-family instance declares it too (a
    /// manifest is per binary, not per instance, following
    /// `hytte-plugin-stats`'s precedent) and never uses it — its card is not a
    /// click target.
    fn manifest() -> Manifest {
        let mut manifest = Manifest::new(PLUGIN_ID, DEFAULT_MOUNT);
        manifest.capabilities = vec![Capability::OpenPage];
        manifest
    }

    fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
        let (is_bar, title) = resolve_settings(DEFAULT_MOUNT, &env_lookup);
        Self {
            status: status::snapshot(),
            usage: usage::latest(),
            usage_version: usage::version(),
            is_bar,
            title,
        }
    }

    /// The chip's own cadence. Note this is the *chip's* source, not the
    /// bridge's: the HTTP listener and the usage poll are spawned by `main` on a
    /// different runtime and outlive every plugin session (module docs).
    fn sources(_cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        Some(Box::pin(tick_stream(POLL, Tick)))
    }

    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            Input::App(Tick) => {
                self.status = status::snapshot();
                self.refresh_usage();
            }
            Input::Event { node, kind, .. } => return Self::on_event(&node, &kind),
            _ => {}
        }
        Vec::new()
    }

    /// The bar family renders the chip **and** publishes the drawer panel a
    /// click opens; the sidebar family renders #1280 P1's card and publishes
    /// no panel — a card nothing can click is not a surface the drawer would
    /// ever show (`hytte-plugin-stats`'s `Stats::view` is the same split, for
    /// the same reason).
    fn view(&self) -> View {
        let now = usage::now_unix();
        let report = self.usage.as_ref();
        if self.is_bar {
            View::new(chip(&self.status, report, now)).panel(panel(
                &self.title,
                &self.status,
                report,
                now,
            ))
        } else {
            View::new(card(&self.title, &self.status, report, now))
        }
    }
}

// ── Pure projections ─────────────────────────────────────────────────────────

/// The health glyph: how the most recently answered request went.
///
/// Adwaita symbolic names, so they resolve in the shell's forced `Adwaita` icon
/// theme. `content-loading-symbolic` is the honest "nothing has happened yet"
/// state — an `emblem-ok` before the first request would claim health nobody has
/// measured.
fn health_icon(last: Last) -> &'static str {
    match last {
        Last::None => "content-loading-symbolic",
        Last::Ok => "emblem-ok-symbolic",
        Last::Error => "dialog-warning-symbolic",
    }
}

/// The three-letter backend label. Short because this is a bar chip, and
/// distinct because "which backend am I paying for" is the one thing a glance
/// has to answer: `sub` rides the subscription, `rep` re-prompts a fresh
/// `claude` per turn, `api` spends metered credits.
fn mode_label(mode: Mode) -> &'static str {
    mode_words(mode).0
}

/// The same backend, spelled out for the tooltip — which has room for words the
/// chip does not.
fn mode_name(mode: Mode) -> &'static str {
    mode_words(mode).1
}

/// The chip's two renderings of a backend, `(short, spelled out)`, from **one**
/// match so they cannot drift: a new [`Mode`] variant gets both or neither, and
/// the tooltip can never explain a different mode than the label prints.
fn mode_words(mode: Mode) -> (&'static str, &'static str) {
    match mode {
        Mode::Subscription => ("sub", "subscription"),
        Mode::Reprompt => ("rep", "re-prompt"),
        Mode::Api => ("api", "API key"),
    }
}

/// The widest count the chip will print before switching to `999+`.
///
/// A bar chip has to keep a fixed-ish width — this one sits in a shared region
/// with every other chip — and a long-lived bridge answering a pet tick a minute
/// reaches five digits inside a day, which would quietly push its neighbours
/// around. Past a thousand the exact number is not what anyone is reading the
/// chip for anyway; the journal has it.
const COUNT_CAP: u64 = 999;

/// The coarse counts, `<2xx>/<not-2xx>` — or an empty string before anything has
/// been served, which the caller renders as *no label at all* rather than a
/// misleading `0/0`. Each side saturates at [`COUNT_CAP`] so the chip's width
/// stops growing.
fn counts_label(ok: u64, errors: u64) -> String {
    if ok == 0 && errors == 0 {
        return String::new();
    }
    format!("{}/{}", capped(ok), capped(errors))
}

/// One count, rendered at most four characters wide.
fn capped(n: u64) -> String {
    if n > COUNT_CAP {
        format!("{COUNT_CAP}+")
    } else {
        n.to_string()
    }
}

/// The **severity → ink** mapping for a chip meter, defaulting to "normal".
///
/// A [`AccentRole`] rather than a colour: the shell resolves the role against
/// the live theme on every render, so a desktop accent change re-tints the
/// meters with no wire traffic (#885/#396). An unknown severity word reads as
/// normal — the endpoint is undocumented and a new one must not blank the
/// meter.
pub fn severity_role(severity: &str) -> AccentRole {
    match severity.trim() {
        "warning" => AccentRole::Warning,
        "critical" => AccentRole::Error,
        _ => AccentRole::Accent,
    }
}

/// The **severity → CSS class** mapping for the panel's labels and bars.
///
/// The sibling of [`severity_role`], and deliberately from the same three words:
/// libadwaita's own `accent` / `warning` / `error` state classes, which the host
/// applies verbatim. `None` for a severity that maps to no class is not an
/// option here — "normal" wants the accent tint the meters get.
pub fn severity_class(severity: &str) -> &'static str {
    match severity.trim() {
        "warning" => "warning",
        "critical" => "error",
        _ => "accent",
    }
}

/// The meters the **chip** carries: the active limits, in the server's order,
/// capped at [`MAX_CHIP_METERS`].
///
/// Inactive rows are the panel's business — a bucket that is not counting is
/// worth a line in a list and is not worth a permanent 70 px of bar.
pub fn chip_limits(usage: &Usage) -> Vec<&Limit> {
    usage
        .limits
        .iter()
        .filter(|limit| limit.active())
        .take(MAX_CHIP_METERS)
        .collect()
}

/// The whole chip in one sentence, for the hover (#957).
///
/// The chip is a handful of 16 px glyphs plus a couple of wordless meters, so
/// the tooltip spells out every part it abbreviates: the backend by name, and
/// the counts as words.
///
/// The counts are the **raw** numbers, not [`counts_label`]'s `999+`-saturated
/// ones: the cap exists to stop the chip widening on the bar, and a tooltip has
/// no width to defend.
///
/// Lines are appended below the head for whichever of these is true, in
/// order: nothing at all, when there is nothing more specific to say — the
/// meters carry their own hovers, and repeating their numbers here would put
/// the same numbers in two places that can disagree by a tick; the failure
/// sentence alone, when the last poll failed and the numbers it is still
/// showing (if any) are not stale; the staleness stamp alone, on a *stale
/// success* — nothing more specific to say than since when; or **both**, on a
/// *stale failure* — the sentence first, the stamp second — so a sustained
/// 429 never drops "next try in N min" for "usage stale since …" the way a
/// stamp-wins rule would (#1254's N2 hover-contradicts-panel shape, and
/// exactly the sustained-429 case #1283 was filed for; #1285's review, MED).
/// [`Report::is_stale`] is judged past [`usage::STALE_AFTER`], and not only a
/// wedged poll reaches it: since #1283 a *healthy* poller backing off a
/// sustained run of 429s can too.
fn tooltip(status: &Status, report: Option<&Report>, now: i64) -> String {
    let head = match status.startup {
        // Same honesty as the `…` label: no mode has been settled yet, so name
        // none.
        None => "Claude bridge · starting up".to_owned(),
        Some(startup) => {
            let traffic = if status.ok == 0 && status.errors == 0 {
                "nothing served yet".to_owned()
            } else {
                format!("{} served, {} failed", status.ok, status.errors)
            };
            format!("Claude bridge · {} · {traffic}", mode_name(startup.mode))
        }
    };
    let second_line = match report {
        // A stale report — whether the *latest* poll succeeded or is still
        // failing on top of old-enough `last_ok` numbers (#1283). `numbers_at`
        // names when those numbers were actually fetched, which on a stale
        // failure is `last_ok`'s clock, not this attempt's own `at`.
        Some(report) if report.usage().is_some() && report.is_stale(now) => {
            let stamp = format!(
                "usage stale since {}",
                usage::format_utc(report.numbers_at().unwrap_or(report.at))
            );
            match usage_failure_sentence(status, report, now) {
                // A stale *failure*: the actionable sentence first, the
                // staleness stamp second — never dropping "next try in
                // N min" is the whole point (see the doc above).
                Some(sentence) => Some(format!("{sentence}\n{stamp}")),
                // A stale *success*: nothing more specific to say than since
                // when — today's wording, unchanged.
                None => Some(stamp),
            }
        }
        Some(report) => usage_failure_sentence(status, report, now),
        None => None,
    };
    match second_line {
        Some(line) => format!("{head}\n{line}"),
        None => head,
    }
}

/// The failure line for the tooltip, one mode adjustment applied.
///
/// Every arm but one reads [`UsageError::sentence`] verbatim. The exception:
/// [`UsageError::NoCredentials`] in [`Mode::Api`], where "run `claude` once to
/// sign in" is advice for a login this mode never needs — it never spawns
/// `claude` (module docs, `Mode::spawns_claude`) and may have no Claude Code
/// login on the box at all. The usage limits are a subscription-account
/// feature regardless of which backend is answering, so say that instead.
fn usage_failure_sentence(status: &Status, report: &Report, now: i64) -> Option<String> {
    let Outcome::Failed(error) = &report.outcome else {
        return None;
    };
    let is_api = matches!(
        status.startup,
        Some(Startup {
            mode: Mode::Api,
            ..
        })
    );
    if is_api && matches!(error, UsageError::NoCredentials(_)) {
        return Some(
            "usage limits are a subscription feature — no Claude Code login is expected in \
             API-key mode"
                .to_owned(),
        );
    }
    report.error(now)
}

/// One meter's hover: `Session (5 h): 80% — resets in 2 h 15 min`.
///
/// The relative half only — the absolute stamp is the panel's, which has room
/// for it.
fn meter_tooltip(limit: &Limit, now: i64) -> String {
    let head = format!(
        "{}: {}",
        usage::humanise_kind(&limit.kind),
        usage::percent_label(limit.percent)
    );
    match usage::reset_short(now, limit.resets_at.as_deref()) {
        Some(when) => format!("{head} — resets {when}"),
        None => head,
    }
}

/// One chip meter: a level strip, wrapped in a box that can carry the tooltip
/// [`Node::Preem`](hytte_plugin::proto::Node::Preem) has no field for.
///
/// The strip is built here rather than held in the model on purpose: this one
/// drives no animation the shell owns — no peak-hold is declared, so its entire
/// state is the level — and a widget with no animation to preserve is cheaper to
/// construct than to reconcile. Ids are indexed as well as named so two rows
/// that somehow share a `kind` cannot collapse onto one renderer instance
/// (`Node::Preem`'s id contract, #918).
fn meter(index: usize, limit: &Limit, now: i64) -> Node {
    let mut strip = LedStrip::new(CHIP_STYLE)
        .leds(CHIP_LEDS)
        .accent_role(severity_role(limit.severity()));
    // 0.0..=1.0 by construction (`Limit::fraction` clamps and absorbs NaN), so
    // the narrowing cannot lose anything that matters at six segments.
    #[allow(clippy::cast_possible_truncation)]
    strip.set_level(limit.fraction() as f32);
    Node::Box {
        id: Some(format!("claude-bridge-meter-{index}-{}", limit.kind)),
        dir: Dir::Horizontal,
        spacing: 0,
        scroll: false,
        classes: Vec::new(),
        children: vec![strip.node(&format!("claude-bridge-led-{index}-{}", limit.kind))],
        tooltip: Some(meter_tooltip(limit, now)),
    }
}

/// A plain label node.
fn label(text: &str, classes: &[&str]) -> Node {
    Node::Label {
        id: None,
        text: text.to_owned(),
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
        // The whole pill's hover lives on the root box (see `chip`); a tooltip
        // here would only shadow it for the pointer's exact position.
        tooltip: None,
    }
}

/// How many characters the title row shows before ellipsizing (#1315 review,
/// MED 2).
///
/// `CLAUDE_BRIDGE_LABEL` is operator-set and unbounded — before this it was a
/// bare `Node::Label`, whose natural width is its whole string with no cap at
/// all, and `Node::Label` cannot wrap or ellipsize on the wire. Measured under
/// `xvfb-run` in the real containers (`.ts-plugin-card` inside `AdwClamp`'s
/// 320 px `maximum_size`, `.ts-plugin-panel` > `.ts-plugin-canvas` for the
/// drawer), with the #1315 MED-1 `.ts-usage-card` padding already applied: an
/// **uncapped** title's minimum request equals its full string width (a
/// `GtkLabel` never shrinks below its own text), so a title past the design
/// width is the exact `AdwClamp` "a card whose own minimum exceeds the cap is
/// still allocated at its minimum" shape (`trollshell/src/overlays/sidebar.rs`)
/// — a 31-character label alone measured a 364 px minimum against the 320 px
/// design width, and the effect is **independent of length past the cap**: a
/// 300-character label measured the same 364 px minimum uncapped, once
/// ellipsized. An ellipsizing `Node::Text`'s minimum, by contrast, measured a
/// constant ~162 px at every `max_width_chars` tried (14 through 24) —
/// ellipsis is what makes the minimum small, not the cap value — so the
/// overflow mode this constant exists to prevent cannot recur at any N; **20**
/// is chosen for headroom rather than survival: it is the largest of the
/// measured candidates whose *natural* width still measured comfortably under
/// the 320 px design width (308 px, 12 px of margin) rather than saturating
/// against `AdwClamp`'s own cap (320 px, from N=22 up) — and it matches
/// `hytte-plugin-agents`'s `NAME_CHARS`, the same "one short sidebar-row label
/// beside a spacer" shape.
const TITLE_CHARS: i32 = 20;

/// The title row's own label: ellipsizing and capped at [`TITLE_CHARS`], with
/// its own full text as the hover (#1302's ask, #1315 review MED 2) — the
/// `hytte-plugin-agents::clipped` shape, with the tooltip set **explicitly**
/// rather than left to the host's ellipsize-with-no-tooltip default, so a
/// falsification (dropping the cap, or dropping the tooltip) reds in this
/// crate's own tests rather than depending on host behaviour this crate does
/// not exercise.
fn title_label(text: &str) -> Node {
    Node::Text {
        id: None,
        text: text.to_owned(),
        max_width_chars: Some(TITLE_CHARS),
        ellipsize: true,
        tooltip: Some(text.to_owned()),
        classes: vec!["heading".to_owned()],
    }
}

/// A symbolic icon node.
fn icon(name: &str, classes: &[&str]) -> Node {
    Node::Icon {
        id: None,
        name: name.to_owned(),
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
        tooltip: None,
    }
}

/// A horizontal row with a right-pinned trailing child.
fn titled_row(leading: Node, trailing: Node) -> Node {
    Node::Row {
        id: None,
        classes: Vec::new(),
        spacing: 6,
        children: vec![leading, Node::Spacer, trailing],
        tooltip: None,
    }
}

/// A full-width bar, `0.0..=1.0`, tinted by a severity class.
fn bar(id: String, fraction: f64, class: &str) -> Node {
    Node::Progress {
        id: Some(id),
        fraction: fraction.clamp(0.0, 1.0),
        classes: vec![class.to_owned()],
    }
}

/// Project the status board and the usage board onto the bar chip.
///
/// The host wraps this in its own `.ts-plugin-chip` pill; the `flat` button
/// inside it is what makes the pill clickable without drawing a second frame.
///
/// Before `main` has published the startup facts the chip renders a single muted
/// ellipsis: the daemon is up but has not settled its backend yet, and inventing
/// a mode for that window would be a lie the chip is specifically there to
/// prevent.
fn chip(status: &Status, report: Option<&Report>, now: i64) -> Node {
    // Annika's ask on #957: lead with the Claude glyph, so the pill reads as
    // "the Claude thing" before anyone tries to parse `sub 18/0`.
    let mut children = vec![icon(CLAUDE_ICON, &[]), icon(health_icon(status.last), &[])];
    match status.startup {
        None => children.push(label("…", &["dim-label"])),
        Some(startup) => {
            children.push(label(mode_label(startup.mode), &[]));
            if startup.keyed {
                // A held credential, never the credential itself.
                children.push(icon("dialog-password-symbolic", &["dim-label"]));
            }
            let counts = counts_label(status.ok, status.errors);
            if !counts.is_empty() {
                children.push(label(&counts, &["dim-label", "numeric"]));
            }
        }
    }
    // A stale report's meters are dropped, not just its tooltip line: a level
    // strip with no words on it is the one part of the chip that could read as
    // current when it is not (`tooltip` is where the "since <time>" text
    // lives).
    if let Some(usage) = report
        .filter(|report| !report.is_stale(now))
        .and_then(Report::usage)
    {
        for (index, limit) in chip_limits(usage).into_iter().enumerate() {
            children.push(meter(index, limit, now));
        }
    }
    let inner = Node::Box {
        id: Some(ROOT_ID.to_owned()),
        dir: Dir::Horizontal,
        spacing: 4,
        scroll: false,
        classes: Vec::new(),
        children,
        // On the inner box, so hovering anywhere on the pill that isn't a meter
        // answers the question — the glyphs are 16 px wide and nobody should
        // have to find the right one.
        tooltip: Some(tooltip(status, report, now)),
    };
    Node::Button {
        id: CHIP_BTN.to_owned(),
        classes: vec!["flat".to_owned()],
        child: Box::new(inner),
    }
}

/// The usage surface's header: `title`, and either how fresh the numbers are
/// or why there aren't any. Shared by [`panel`] and [`card`] (#1280 P1) — the
/// one place either surface's title is actually drawn, so the two can never
/// print different words for the same resolved title.
fn header(title: &str, report: Option<&Report>, now: i64) -> Node {
    let freshness = match report {
        None => "not fetched yet".to_owned(),
        // The numbers' own clock, not the attempt's — on a failure behind
        // fresh `last_ok` numbers those are different instants, and this
        // header draws the numbers, not the attempt (#1285's review, HIGH-1).
        // `numbers_at()` is `None` when there is nothing to show at all
        // (no `last_ok`), where `report.at` — the failed attempt's own
        // timestamp — is the only clock there is to name.
        Some(report) => format!(
            "updated {}",
            usage::humanise_since(now, report.numbers_at().unwrap_or(report.at))
        ),
    };
    titled_row(title_label(title), label(&freshness, &["dim-label"]))
}

/// One limit, as a drawer row: title + percent, a full-width bar, and a caption
/// carrying the group, the reset time both ways, and — for a bucket that is not
/// currently counting — the fact that it isn't.
///
/// Inactive rows are **dimmed, not dropped**: "the weekly window is not counting
/// yet" is information, and a list that silently omits rows is a list nobody can
/// trust to be the account's.
fn limit_row(index: usize, limit: &Limit, now: i64) -> Node {
    let class = severity_class(limit.severity());
    let active = limit.active();
    let mut title_classes: Vec<&str> = vec!["heading"];
    let mut value_classes: Vec<&str> = vec!["numeric"];
    if active {
        value_classes.push(class);
    } else {
        title_classes.push("dim-label");
        value_classes.push("dim-label");
    }

    let mut caption: Vec<String> = Vec::new();
    if let Some(group) = limit
        .group
        .as_deref()
        .map(str::trim)
        .filter(|g| !g.is_empty())
    {
        caption.push(group.to_owned());
    }
    if let Some(phrase) = usage::reset_phrase(now, limit.resets_at.as_deref()) {
        caption.push(phrase);
    }
    if !active {
        caption.push("not counting right now".to_owned());
    }

    let mut children = vec![
        titled_row(
            label(&usage::humanise_kind(&limit.kind), &title_classes),
            label(&usage::percent_label(limit.percent), &value_classes),
        ),
        bar(
            format!("claude-bridge-bar-{index}-{}", limit.kind),
            limit.fraction(),
            class,
        ),
    ];
    if !caption.is_empty() {
        children.push(label(&caption.join(" · "), &["dim-label"]));
    }
    Node::Box {
        id: Some(format!("claude-bridge-row-{index}-{}", limit.kind)),
        dir: Dir::Vertical,
        spacing: 2,
        scroll: false,
        classes: Vec::new(),
        children,
        tooltip: None,
    }
}

/// The extra-usage (pay-as-you-go overflow) row, rendered **only** when the
/// account has it enabled — which is the uncommon case, and the reason it is a
/// row rather than a permanent fixture.
fn extra_row(extra: &ExtraUsage) -> Node {
    let value = extra
        .utilization
        .map_or_else(|| "on".to_owned(), usage::percent_label);
    let mut children = vec![
        titled_row(
            label("Extra usage", &["heading"]),
            label(&value, &["numeric"]),
        ),
        bar(
            "claude-bridge-bar-extra".to_owned(),
            extra.utilization.unwrap_or(0.0) / 100.0,
            "accent",
        ),
    ];
    let currency = extra.currency.as_deref().unwrap_or("").trim().to_owned();
    let spend = match (extra.used_credits, extra.monthly_limit) {
        (Some(used), Some(limit)) => Some(format!("{used:.2} of {limit:.2} {currency}")),
        (Some(used), None) => Some(format!("{used:.2} {currency} used")),
        (None, Some(limit)) => Some(format!("{limit:.2} {currency} allowance")),
        (None, None) => None,
    };
    if let Some(spend) = spend {
        children.push(label(spend.trim(), &["dim-label"]));
    }
    Node::Box {
        id: Some("claude-bridge-row-extra".to_owned()),
        dir: Dir::Vertical,
        spacing: 2,
        scroll: false,
        classes: Vec::new(),
        children,
        tooltip: None,
    }
}

/// The failure line, when the *latest* poll failed — [`usage_failure_sentence`]'s
/// mode adjustment, the same one the chip hover applies, so the panel can
/// never contradict it (#1254's review, N2). Used both as [`panel`]'s only
/// content when there are no numbers at all, and as its footer under a list
/// still drawn from [`Report::last_ok`] (#1283) — a failure names itself
/// without ever blanking numbers that are still within [`Report::is_stale`]'s
/// window.
fn failure_footer(status: &Status, report: &Report, now: i64) -> Option<Node> {
    let Outcome::Failed(error) = &report.outcome else {
        return None;
    };
    let sentence =
        usage_failure_sentence(status, report, now).unwrap_or_else(|| error.sentence(now));
    Some(label(&sentence, &["warning"]))
}

/// **Every** row the server sent, then the extra-usage allowance when it is
/// on — the state-dependent body [`panel`] and [`card`] both draw from one
/// report, pulled out of the drawer panel's original, single implementation
/// verbatim by #1280 P1 so the sidebar card cannot draw a different list
/// from the same numbers.
///
/// States other than a list, each of which says what it knows rather than
/// showing an empty page: nothing polled yet; there are no numbers to show at
/// all (either the poll failed with no last-known-good numbers to fall back
/// on, or the numbers that do exist are too old to trust) — the sentence
/// and/or the staleness stamp are the only actionable things there are; or
/// the account genuinely reported no limits. Since #1283 a **failed** poll
/// that still has fresh-enough [`Report::last_ok`] numbers is a further
/// state, not a variant of the no-numbers one: the rows render exactly as a
/// success's would (from [`Report::usage`], which already resolves to
/// `last_ok` on a failure), with [`failure_footer`] appended below them
/// rather than replacing them.
///
/// The staleness gate mirrors [`chip`]'s — `report.usage().filter(|_|
/// !report.is_stale(now))` — so neither surface can ever disagree with the
/// chip about whether there is anything current to show. Before #1285's
/// review this function drew straight off `report.usage()` with no cutoff at
/// all, which was tolerable while the panel only ever rendered rows on
/// [`Outcome::Ok`], where the age it displayed *was* the numbers' age — but
/// #1283 is what put a failure path through here too, and a sustained 429 at
/// [`usage::MAX_BACKOFF`] is the ordinary case under it, not an edge: without
/// the gate this painted every bar at an hours-old value under an "updated
/// just now" header, one click below a chip that had already, correctly,
/// dropped its own meters.
fn usage_rows(status: &Status, report: Option<&Report>, now: i64) -> Vec<Node> {
    let mut children = Vec::new();
    match report {
        None => children.push(label("fetching usage…", &["dim-label"])),
        Some(report) => {
            if let Some(usage) = report.usage().filter(|_| !report.is_stale(now)) {
                if usage.limits.is_empty() {
                    children.push(label("this account reported no limits", &["dim-label"]));
                }
                for (index, limit) in usage.limits.iter().enumerate() {
                    children.push(limit_row(index, limit, now));
                }
                if let Some(extra) = usage.extra_usage.as_ref().filter(|extra| extra.is_enabled) {
                    children.push(extra_row(extra));
                }
                children.extend(failure_footer(status, report, now));
            } else {
                // `is_stale` is `false` with nothing to be stale about (no
                // `last_ok` at all) — the failure sentence alone covers that
                // case, same as always.
                if report.is_stale(now) {
                    children.push(label(
                        &format!(
                            "usage stale since {}",
                            usage::format_utc(report.numbers_at().unwrap_or(report.at))
                        ),
                        &["warning"],
                    ));
                }
                children.extend(failure_footer(status, report, now));
            }
        }
    }
    children
}

/// The `ts-usage-card` container Annika asked for on #1280 P1 ("just wrap it
/// in a card so it looks better") — one function, one shape, two callers
/// ([`panel`] and [`card`]), which differ only in which ids they carry and
/// which title they show, never in what is inside.
///
/// `root_id` becomes the returned tree's own top-level id — [`PANEL_ROOT_ID`]
/// for the drawer panel, unchanged from before this wrapper existed, so every
/// existing reader of "the panel's root" keeps working. `list_id` names the
/// one node this wrapper adds: the header-plus-rows list, nested one level
/// inside `root_id`. Nothing that rendered directly under a surface's root
/// before #1280 P1 changes its own id — the header and every row/bar id
/// [`usage_rows`] builds are exactly what they were; they are simply nested
/// one box deeper now, which the tree-walking test helpers (`texts`, `bars`,
/// `tooltips`) already recurse through.
fn usage_card(
    root_id: &str,
    list_id: &str,
    title: &str,
    status: &Status,
    report: Option<&Report>,
    now: i64,
) -> Node {
    let mut children = vec![header(title, report, now)];
    children.extend(usage_rows(status, report, now));
    let list = Node::Box {
        id: Some(list_id.to_owned()),
        dir: Dir::Vertical,
        spacing: 12,
        scroll: false,
        classes: Vec::new(),
        children,
        tooltip: None,
    };
    Node::Box {
        id: Some(root_id.to_owned()),
        dir: Dir::Vertical,
        spacing: 0,
        scroll: false,
        classes: vec![CARD_CLASS.to_owned()],
        children: vec![list],
        tooltip: None,
    }
}

/// The drawer page (#1236), now [`usage_card`]-wrapped (#1280 P1) so the
/// panel reads as a card instead of a bare list. Opened by
/// `Effect::OpenPage(Page::PluginSelf)` on a bar-family instance's chip
/// click; a sidebar-family instance never publishes this (see [`Plugin::view`]).
fn panel(title: &str, status: &Status, report: Option<&Report>, now: i64) -> Node {
    usage_card(PANEL_ROOT_ID, PANEL_LIST_ID, title, status, report, now)
}

/// The sidebar card (#1280 P1): every row [`panel`] would show, in the same
/// [`usage_card`] container, under its own title row and root id. Rendered
/// **instead of** the chip on a sidebar-family mount — never alongside it,
/// and never a click target (see [`Plugin::view`], [`Plugin::manifest`]).
fn card(title: &str, status: &Status, report: Option<&Report>, now: i64) -> Node {
    usage_card(CARD_ROOT_ID, CARD_LIST_ID, title, status, report, now)
}

// ── Entry points used by `main` ──────────────────────────────────────────────

/// Whether there is a host socket to dial at all. `main` uses this to decide
/// between running the plugin face (which owns the main thread forever) and
/// parking on the HTTP runtime — the API must not depend on the shell.
#[must_use]
pub fn host_socket_available() -> bool {
    hytte_plugin::proto::socket_path().is_some()
}

/// Hand the main thread to the SDK: dial the host socket with bounded backoff,
/// register, and paint the chip forever. Never returns.
pub fn run() -> ! {
    hytte_plugin::run::<BridgeChip>()
}

#[cfg(test)]
mod tests {
    use super::{
        BridgeChip, CARD_CLASS, CARD_LIST_ID, CARD_ROOT_ID, CHIP_BTN, CLAUDE_ICON, DEFAULT_MOUNT,
        DEFAULT_TITLE, LABEL_ENV, MAX_CHIP_METERS, MOUNT_ENV, PANEL_LIST_ID, PANEL_ROOT_ID,
        TITLE_CHARS, Tick, capped, card, card_title, chip, chip_limits, counts_label,
        effective_mount, health_icon, meter_tooltip, mode_label, mode_name, panel,
        resolve_settings, severity_class, severity_role, tooltip,
    };
    use crate::Mode;
    use crate::status::{Last, Startup, Status};
    use crate::usage::{
        self, ExtraUsage, Limit, Outcome, Report, Usage, UsageError, parse_rfc3339,
    };
    use hytte_plugin::display::{AccentRole, RenderMode};
    use hytte_plugin::proto::{
        Capability, Effect, EventKind, Manifest, Mount, Node, Page, PluginMsg, decode, encode,
        preem::PreemWidget,
    };
    use hytte_plugin::{Input, Plugin};

    /// A fixed `now` so every humanised string in these tests is a constant.
    /// `2026-09-13T11:35:00Z` is 2 h 15 min before the captured session reset.
    fn now() -> i64 {
        parse_rfc3339("2026-09-13T11:35:00Z").expect("a fixed now")
    }

    fn status(mode: Mode, keyed: bool, ok: u64, errors: u64, last: Last) -> Status {
        Status {
            startup: Some(Startup { mode, keyed }),
            ok,
            errors,
            last,
        }
    }

    /// A `Status` for panel tests that do not care which mode is answering —
    /// only [`usage_failure_sentence`]'s one special case (`NoCredentials` in
    /// `Mode::Api`) does, and those tests build their own.
    fn default_status() -> Status {
        status(Mode::Subscription, false, 0, 0, Last::None)
    }

    /// One limit row, spelled the way the endpoint spells them.
    fn limit(kind: &str, percent: f64, severity: &str, active: bool, resets: &str) -> Limit {
        Limit {
            kind: kind.to_owned(),
            group: Some(kind.split('_').next().unwrap_or(kind).to_owned()),
            percent,
            severity_raw: Some(severity.to_owned()),
            resets_at: Some(resets.to_owned()),
            is_active: Some(active),
        }
    }

    /// The captured three-row response.
    fn captured_usage() -> Usage {
        Usage {
            limits: vec![
                limit(
                    "session",
                    80.0,
                    "warning",
                    true,
                    "2026-09-13T13:50:00.101848+00:00",
                ),
                limit(
                    "weekly_all",
                    40.0,
                    "normal",
                    true,
                    "2026-09-17T15:00:00.101872+00:00",
                ),
                limit(
                    "weekly_scoped",
                    25.0,
                    "normal",
                    false,
                    "2026-09-17T15:00:00.102097+00:00",
                ),
            ],
            extra_usage: Some(ExtraUsage::default()),
        }
    }

    /// The captured response, as a published report.
    fn captured_report() -> Report {
        Report {
            at: now() - 120,
            outcome: Outcome::Ok(captured_usage()),
            last_ok: None,
        }
    }

    /// The captured response, anchored to the **real** clock instead of this
    /// module's fixed narrative `now()`.
    ///
    /// Every other fixture in this module is read through [`chip`]/[`panel`]
    /// with a `now` handed in explicitly, so the fixed narrative time is fine
    /// — but [`BridgeChip::view`] reads [`usage::now_unix`] itself, and cannot
    /// be handed a different clock. A report anchored to the narrative `now()`
    /// read through `view()` therefore ages by however long it has been since
    /// that timestamp was written, and eventually crosses `usage::STALE_AFTER`
    /// for real — #1254's review (N1) found exactly that latent shape in
    /// `the_view_always_carries_a_panel`, one test over from where it had
    /// already been fixed here. Use this wherever a fixture report is rendered
    /// through `view()`.
    fn fresh_report() -> Report {
        Report {
            at: usage::now_unix(),
            ..captured_report()
        }
    }

    /// Collect every `Label`/`Icon` payload in a tree, in render order — the
    /// chip is small enough that its full text is the assertion.
    fn texts(node: &Node) -> Vec<String> {
        match node {
            // #1315 review MED 2: the title row's label is now an ellipsizing
            // `Node::Text` (see `title_label`), not a `Node::Label` — this
            // helper has to see both, or every text-content assertion in this
            // module would silently stop seeing the title at all.
            Node::Label { text, .. } | Node::Text { text, .. } => vec![text.clone()],
            Node::Icon { name, .. } => vec![name.clone()],
            Node::Button { child, .. } => texts(child),
            Node::Box { children, .. } | Node::Row { children, .. } => {
                children.iter().flat_map(texts).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Every `Node::Text` in a tree, `(text, max_width_chars, ellipsize,
    /// tooltip)` — the #1315 review MED 2 fields, since `texts()` alone
    /// collapses a `Text` down to its string and loses them.
    fn ellipsized_texts(node: &Node) -> Vec<(String, Option<i32>, bool, Option<String>)> {
        match node {
            Node::Text {
                text,
                max_width_chars,
                ellipsize,
                tooltip,
                ..
            } => vec![(text.clone(), *max_width_chars, *ellipsize, tooltip.clone())],
            Node::Button { child, .. } => ellipsized_texts(child),
            Node::Box { children, .. } | Node::Row { children, .. } => {
                children.iter().flat_map(ellipsized_texts).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Every preem node in a tree, `(id, widget)`, in render order.
    fn preems(node: &Node) -> Vec<(String, PreemWidget)> {
        match node {
            Node::Preem { id, widget, .. } => {
                vec![(id.clone().unwrap_or_default(), (**widget).clone())]
            }
            Node::Button { child, .. } => preems(child),
            Node::Box { children, .. } | Node::Row { children, .. } => {
                children.iter().flat_map(preems).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Every `Progress` node in a tree, `(id, fraction, classes)`.
    fn bars(node: &Node) -> Vec<(String, f64, Vec<String>)> {
        match node {
            Node::Progress {
                id,
                fraction,
                classes,
            } => vec![(id.clone().unwrap_or_default(), *fraction, classes.clone())],
            Node::Button { child, .. } => bars(child),
            Node::Box { children, .. } | Node::Row { children, .. } => {
                children.iter().flat_map(bars).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Every tooltip in a tree, in render order.
    fn tooltips(node: &Node) -> Vec<String> {
        // `Node::Text`'s own tooltip is `title_label`'s hover (#1315 review
        // MED 2) — the one leaf tooltip this crate ever sets deliberately
        // (every other label is `tooltip: None` on purpose, since the
        // whole-pill hover lives on the chip's root box; see `label`'s doc).
        let own = match node {
            Node::Box { tooltip, .. } | Node::Row { tooltip, .. } | Node::Text { tooltip, .. } => {
                tooltip.clone()
            }
            _ => None,
        };
        let children = match node {
            Node::Button { child, .. } => tooltips(child),
            Node::Box { children, .. } | Node::Row { children, .. } => {
                children.iter().flat_map(tooltips).collect()
            }
            _ => Vec::new(),
        };
        own.into_iter().chain(children).collect()
    }

    /// The hover text on the chip's inner box — the chip's whole-pill tooltip
    /// (#957), now reached through the click button that wraps it (#1236).
    fn root_tooltip(node: &Node) -> Option<String> {
        match node {
            Node::Button { child, .. } => root_tooltip(child),
            Node::Box { tooltip, .. } => tooltip.clone(),
            _ => None,
        }
    }

    /// The chip in **state** mode, where a meter is a `Node::Preem` — the mode a
    /// preem-speaking shell negotiates, and the one whose tree these tests read.
    fn chip_state(status: &Status, report: Option<&Report>) -> Node {
        hytte_plugin::display::testing::with_render_mode(RenderMode::State, || {
            chip(status, report, now())
        })
    }

    // ── The manifest ─────────────────────────────────────────────────────────

    /// The manifest is the whole of this plugin's host contract: a bar chip that
    /// asks for exactly one capability. A second one creeping in here is a real
    /// change (the host cap-checks effects), so pin the list.
    #[test]
    fn the_manifest_is_a_bar_chip_that_asks_only_to_open_its_own_panel() {
        let m: Manifest = BridgeChip::manifest();
        assert_eq!(m.id, "claude-bridge");
        assert_eq!(m.mount, Mount::BarRight);
        assert!(m.mount.is_bar(), "it is a chip, not a sidebar card");
        assert_eq!(
            m.capabilities,
            vec![Capability::OpenPage],
            "the drawer panel, and nothing else"
        );
        assert!(
            m.subscribes.is_empty(),
            "it ticks off its own boards, not host state"
        );
        assert!(m.provides.is_empty());
        // Sanity: `Manifest::new` alone requests none, so the line above is this
        // plugin's, not the constructor's.
        assert!(Manifest::new("x", Mount::BarRight).capabilities.is_empty());
    }

    /// Clicking the chip asks the host to open this plugin's own panel; a click
    /// on a node it does not own changes nothing.
    #[test]
    fn clicking_the_chip_opens_the_panel() {
        let mut model = BridgeChip {
            status: status(Mode::Subscription, false, 0, 0, Last::None),
            usage: Some(captured_report()),
            usage_version: 0,
            is_bar: true,
            title: DEFAULT_TITLE.to_owned(),
        };
        assert_eq!(
            model.update(Input::event(CHIP_BTN, EventKind::Click)),
            vec![Effect::OpenPage(Page::PluginSelf)]
        );
        assert!(
            model
                .update(Input::event("somebody-elses-button", EventKind::Click))
                .is_empty()
        );
        // …and a tick still never asks the shell for anything.
        assert!(model.update(Input::App(Tick)).is_empty());
    }

    /// The view now carries a panel — the #1236 change to #866's deliberate
    /// panel-less chip — in every state, including before the first poll, so a
    /// click always opens something that explains itself.
    ///
    /// Uses [`fresh_report`], not `captured_report()`: `.view()` reads the
    /// *real* clock (`usage::now_unix()`), so a report anchored to this
    /// module's fixed narrative `now()` renders through the staleness path
    /// once real time has drifted past `usage::STALE_AFTER` from that
    /// timestamp — which it already has. The last assertion is
    /// staleness-SENSITIVE (it reads through the very path a stale report
    /// would suppress), so the injection is load-bearing: falsify by putting
    /// `captured_report()` back in the `Some(…)` arm and this goes red.
    #[test]
    fn the_view_always_carries_a_panel() {
        for usage in [None, Some(fresh_report())] {
            let model = BridgeChip {
                status: status(Mode::Subscription, false, 0, 0, Last::None),
                usage,
                usage_version: 0,
                is_bar: true,
                title: DEFAULT_TITLE.to_owned(),
            };
            let view = model.view();
            assert!(view.panel.is_some());
            assert!(view.hidden_on.is_empty(), "one chip, every monitor");
        }

        let model = BridgeChip {
            status: status(Mode::Subscription, false, 1, 0, Last::Ok),
            usage: Some(fresh_report()),
            usage_version: 0,
            is_bar: true,
            title: DEFAULT_TITLE.to_owned(),
        };
        let hover = root_tooltip(&model.view().tree).expect("a hover");
        assert!(
            !hover.contains("usage stale since"),
            "a freshly-anchored report must not render through the staleness \
             path: {hover:?}"
        );
    }

    // ── #1280 P1: the mount family (bar chip vs. sidebar card) ───────────────

    /// **Golden — a bar mount's view is exactly the chip and its panel,
    /// unmoved by the #1280 P1 mount-family switch.** `chip()`/`panel()`'s own
    /// bodies are untouched by this change (item 1's "unchanged
    /// byte-for-byte"); this test proves `Plugin::view` still reaches them
    /// on a bar mount.
    ///
    /// Falsify by inverting the branches in `Plugin::view` (swap the
    /// `if self.is_bar { .. } else { .. }` arms): `view.tree` becomes the
    /// card's `Node::Box` instead of the chip's `Node::Button`, and the first
    /// `assert_eq!` reds.
    #[test]
    fn a_bar_mount_view_is_exactly_the_chip_and_its_panel() {
        let report = fresh_report();
        let model = BridgeChip {
            status: status(Mode::Api, true, 12, 1, Last::Ok),
            usage: Some(report.clone()),
            usage_version: 0,
            is_bar: true,
            title: DEFAULT_TITLE.to_owned(),
        };
        let view = model.view();
        let now = usage::now_unix();
        assert_eq!(
            view.tree,
            chip(&model.status, Some(&report), now),
            "a bar mount's tree is the chip, unmoved by the #1280 P1 switch"
        );
        assert_eq!(
            view.panel,
            Some(panel(DEFAULT_TITLE, &model.status, Some(&report), now)),
            "…and it still publishes the drawer panel"
        );
    }

    /// **Golden — a sidebar mount's view is exactly the card, and it
    /// publishes no panel.** The mirror of the test above: falsify the same
    /// way, from the other side — inverting `Plugin::view`'s branches makes
    /// `view.tree` the chip's `Node::Button` instead of the card's
    /// `Node::Box`, and `view.panel` stops being `None`.
    #[test]
    fn a_sidebar_mount_view_is_exactly_the_card_and_publishes_no_panel() {
        let report = fresh_report();
        let model = BridgeChip {
            status: status(Mode::Api, true, 12, 1, Last::Ok),
            usage: Some(report.clone()),
            usage_version: 0,
            is_bar: false,
            title: "Home account".to_owned(),
        };
        let view = model.view();
        let now = usage::now_unix();
        assert_eq!(
            view.tree,
            card("Home account", &model.status, Some(&report), now),
            "a sidebar mount's tree is the card"
        );
        assert!(
            view.panel.is_none(),
            "the card is not a click target — a sidebar instance publishes no panel"
        );
    }

    /// [`effective_mount`] falls back to the manifest's own mount when
    /// `HYTTE_PLUGIN_MOUNT` is unset — today's only deployed shape.
    #[test]
    fn effective_mount_falls_back_to_the_manifest_when_unset() {
        assert_eq!(effective_mount(DEFAULT_MOUNT, &|_| None), DEFAULT_MOUNT);
        assert_eq!(
            effective_mount(Mount::SidebarRightTop, &|_| None),
            Mount::SidebarRightTop
        );
    }

    /// [`effective_mount`] reads [`MOUNT_ENV`] — the documented variable,
    /// asserted as a literal rather than the constant so a rename of the
    /// constant alone cannot pass here by agreeing with itself — and a
    /// sidebar wire name switches the family away from a bar default.
    #[test]
    fn effective_mount_reads_the_documented_variable_and_switches_family() {
        let lookup = |key: &str| {
            assert_eq!(key, "HYTTE_PLUGIN_MOUNT");
            Some("SidebarRightTop".to_owned())
        };
        assert_eq!(
            effective_mount(DEFAULT_MOUNT, &lookup),
            Mount::SidebarRightTop
        );
        assert!(!effective_mount(DEFAULT_MOUNT, &lookup).is_bar());
        assert_eq!(MOUNT_ENV, "HYTTE_PLUGIN_MOUNT");
    }

    /// Surrounding whitespace is trimmed (matching the SDK's own parser), and
    /// an unparseable value falls back to the manifest rather than refusing —
    /// unlike the SDK's own `HYTTE_PLUGIN_MOUNT` parser, which refuses a
    /// launch outright, [`effective_mount`] must stay total: it exists so
    /// this plugin can make its *own* decision from a value the SDK has
    /// already accepted or the process has none of.
    #[test]
    fn effective_mount_trims_whitespace_and_falls_back_on_garbage() {
        assert_eq!(
            effective_mount(DEFAULT_MOUNT, &|_| Some("  SidebarTop\t".to_owned())),
            Mount::SidebarTop
        );
        for bad in ["", "   ", "barleft", "Bar-Left"] {
            assert_eq!(
                effective_mount(Mount::SidebarLead, &|_| Some(bad.to_owned())),
                Mount::SidebarLead,
                "{bad:?}"
            );
        }
    }

    /// [`card_title`] falls back to [`DEFAULT_TITLE`] when [`LABEL_ENV`] is
    /// unset or blank — the free, no-second-request default the #1280 triage
    /// settled on.
    #[test]
    fn card_title_falls_back_to_the_default_when_unset_or_blank() {
        for value in [None, Some(""), Some("   ")] {
            assert_eq!(
                card_title(&|_| value.map(str::to_owned)),
                DEFAULT_TITLE,
                "{value:?}"
            );
        }
    }

    /// [`card_title`] reads [`LABEL_ENV`] — the documented variable, again a
    /// literal rather than the constant — and trims it.
    #[test]
    fn card_title_uses_the_labelled_env_var_when_set_and_trims_it() {
        let lookup = |key: &str| {
            assert_eq!(key, "CLAUDE_BRIDGE_LABEL");
            Some("  Home account \t".to_owned())
        };
        assert_eq!(card_title(&lookup), "Home account");
        assert_eq!(LABEL_ENV, "CLAUDE_BRIDGE_LABEL");
    }

    /// **#1315 review MED 3** — the two lines that thread the launch
    /// environment into [`Plugin::init`]'s model are pinned here, not just
    /// [`effective_mount`]/[`card_title`] in isolation. `init` calls nothing
    /// but [`resolve_settings`] and destructures its result, so a test
    /// against this function is a test against `init`'s own wiring.
    ///
    /// Falsify by hardcoding either return value inside `resolve_settings`:
    ///
    /// ```text
    /// (true, card_title(lookup))                    // is_bar hardcoded
    ///   -> the first assert (`!is_bar`) reds: the sidebar override never
    ///      flips the family, so the card can never render on any mount.
    /// (effective_mount(..).is_bar(), DEFAULT_TITLE.to_owned())  // title hardcoded
    ///   -> the second assert (`title == "Home account"`) reds:
    ///      `CLAUDE_BRIDGE_LABEL` is silently ignored.
    /// ```
    ///
    /// Both arms are asserted **and** their opposite (the neutral, no-env
    /// case still resolves to the manifest's own bar mount and
    /// [`DEFAULT_TITLE`]), so this cannot pass by hardcoding either result
    /// to what the first half of the test expects.
    #[test]
    fn resolve_settings_wires_the_mount_override_and_the_label_through_to_init() {
        let overridden = |key: &str| match key {
            "HYTTE_PLUGIN_MOUNT" => Some("SidebarRightTop".to_owned()),
            "CLAUDE_BRIDGE_LABEL" => Some("Home account".to_owned()),
            _ => None,
        };
        let (is_bar, title) = resolve_settings(DEFAULT_MOUNT, &overridden);
        assert!(!is_bar, "a sidebar override must flip the family off bar");
        assert_eq!(title, "Home account");

        let (is_bar, title) = resolve_settings(DEFAULT_MOUNT, &|_| None);
        assert!(is_bar, "no override keeps the manifest's own bar mount");
        assert_eq!(title, DEFAULT_TITLE);
    }

    /// The sidebar card, for the captured three-row response: the title row,
    /// every row [`panel`] would show, the `ts-usage-card` class and its own
    /// root/list ids.
    ///
    /// Falsify by dropping [`CARD_CLASS`] from [`usage_card`]'s children: the
    /// `classes` assertion below reds.
    #[test]
    fn the_card_carries_the_title_and_every_row_a_captured_report_has() {
        let report = captured_report();
        let tree = card("Home account", &default_status(), Some(&report), now());
        let text = texts(&tree);
        assert_eq!(text.first().map(String::as_str), Some("Home account"));
        assert!(text.contains(&"Session (5 h)".to_owned()), "{text:?}");
        assert!(text.contains(&"Weekly (all)".to_owned()), "{text:?}");
        assert!(text.contains(&"Weekly (scoped)".to_owned()), "{text:?}");
        assert_eq!(bars(&tree).len(), 3, "one bar per limit row");
        match &tree {
            Node::Box {
                id,
                classes,
                children,
                ..
            } => {
                assert_eq!(id.as_deref(), Some(CARD_ROOT_ID));
                assert_eq!(classes, &vec![CARD_CLASS.to_owned()]);
                assert_eq!(children.len(), 1, "the wrapper's one new node: the list");
                match &children[0] {
                    Node::Box { id, .. } => assert_eq!(id.as_deref(), Some(CARD_LIST_ID)),
                    other => panic!("the card's list must be a box, got {other:?}"),
                }
            }
            other => panic!("the card root must be a box, got {other:?}"),
        }
    }

    /// **#1315 review MED 2** — a 31-character `CLAUDE_BRIDGE_LABEL` (an
    /// ordinary second-account name, e.g. `"Work subscription
    /// (claude-work)"`) already exceeded the sidebar's 320 px design width
    /// before this fix (measured: a plain, uncapped `Node::Label`'s minimum
    /// request equals its whole string, so `AdwClamp` allocates the card at
    /// that minimum rather than at its 320 px cap — see `TITLE_CHARS`'s own
    /// doc for the measured numbers). The title row is now [`title_label`]:
    /// capped and ellipsizing at [`TITLE_CHARS`], with the **full** string as
    /// the hover — [`card`] and [`panel`] both route through it via
    /// [`header`], so this is one assertion for both surfaces.
    ///
    /// The wire still carries the whole 300-character string — truncation to
    /// the ellipsis is the host's rendering job, not this plugin's — but the
    /// `max_width_chars`/`ellipsize` flags are what stop the overflow
    /// regardless of how long the operator's string is: a 300-char label
    /// measured the *same* ~162 px minimum under `xvfb-run` as a 31-char one
    /// once ellipsized, where the *uncapped* `Node::Label` this replaces grew
    /// its minimum with the string every time.
    ///
    /// Falsify by reverting [`title_label`] to `label(text, &["heading"])`:
    /// `ellipsized_texts` finds nothing and the first assertion panics.
    #[test]
    fn a_300_char_label_is_capped_and_ellipsized_with_the_full_text_as_hover() {
        let long = "x".repeat(300);
        let report = captured_report();

        for tree in [
            card(&long, &default_status(), Some(&report), now()),
            panel(&long, &default_status(), Some(&report), now()),
        ] {
            let titles = ellipsized_texts(&tree);
            assert_eq!(
                titles.len(),
                1,
                "exactly one ellipsizing node — the title row's label: {titles:?}"
            );
            let (text, max_width_chars, ellipsize, tooltip) = &titles[0];
            assert_eq!(
                text, &long,
                "the wire carries the operator's whole string — the host ellipsizes it"
            );
            assert_eq!(*max_width_chars, Some(TITLE_CHARS));
            assert!(*ellipsize, "without this flag a long cap still overflows");
            assert_eq!(
                tooltip.as_deref(),
                Some(long.as_str()),
                "the full label survives as the hover (#1302's ask)"
            );
        }
    }

    /// A failed poll with no last-known-good numbers still renders its
    /// footer **inside the card**, exactly as it does inside the panel — same
    /// [`usage_rows`], same [`failure_footer`].
    #[test]
    fn a_failed_report_still_renders_its_footer_inside_the_card() {
        let board = status(Mode::Subscription, false, 18, 0, Last::Ok);
        let report = Report {
            at: now() - 30,
            outcome: Outcome::Failed(UsageError::Unauthorized),
            last_ok: None,
        };
        let tree = card(DEFAULT_TITLE, &board, Some(&report), now());
        let text = texts(&tree);
        assert!(
            text.contains(&"usage stale — run `claude` once to refresh the login".to_owned()),
            "{text:?}"
        );
    }

    /// A report past [`usage::STALE_AFTER`] renders the same "usage stale
    /// since …" label and no bars inside the card that the panel already
    /// carries for the same input.
    #[test]
    fn a_stale_report_renders_the_staleness_label_inside_the_card() {
        let board = status(Mode::Subscription, false, 18, 0, Last::Ok);
        let ceiling = i64::try_from(usage::STALE_AFTER.as_secs()).expect("fits");
        let stale = Report {
            at: now() - ceiling - 1,
            ..captured_report()
        };
        let tree = card(DEFAULT_TITLE, &board, Some(&stale), now());
        let text = texts(&tree);
        assert!(
            text.iter().any(|t| t.starts_with("usage stale since")),
            "{text:?}"
        );
        assert!(
            bars(&tree).is_empty(),
            "stale numbers render no bars in the card either"
        );
    }

    /// **The panel wrapper is the same node shape as the card.** Both are
    /// [`usage_card`], so the same report draws the same rows in both — the
    /// bars in particular, whose ids [`usage_rows`] builds independently of
    /// which surface called it.
    ///
    /// Falsify by having [`card`] call [`usage_rows`] with a different
    /// argument than [`panel`] does (e.g. a report clamped to a different
    /// `now`): the `bars` equality below reds.
    #[test]
    fn the_panel_and_the_card_draw_the_same_rows_from_the_same_report() {
        let report = captured_report();
        let board = default_status();
        let panel_tree = panel(DEFAULT_TITLE, &board, Some(&report), now());
        let card_tree = card(DEFAULT_TITLE, &board, Some(&report), now());
        assert_eq!(
            bars(&panel_tree),
            bars(&card_tree),
            "one function, usage_card, draws both surfaces' rows"
        );
        assert!(texts(&panel_tree).contains(&"Claude usage".to_owned()));
        assert!(texts(&card_tree).contains(&"Claude usage".to_owned()));
    }

    // ── (e) The chip's meters ────────────────────────────────────────────────

    /// **N active limits ⇒ min(N, 2) meters on the chip, N rows in the panel.**
    ///
    /// The cap is a width budget for the bar, and the panel is where "all of
    /// them" lives. Falsify by rendering every active limit on the chip (drop
    /// the `.take(MAX_CHIP_METERS)` in `chip_limits`): the `n.min(2)` assertion
    /// goes red from three limits up.
    #[test]
    fn the_chip_caps_its_meters_where_the_panel_shows_every_row() {
        for n in 0_usize..5 {
            let usage = Usage {
                limits: (0..n)
                    .map(|i| {
                        limit(
                            &format!("bucket_{i}"),
                            f64::from(u32::try_from(i).unwrap_or(0)) * 10.0,
                            "normal",
                            true,
                            "2026-09-13T13:50:00Z",
                        )
                    })
                    .collect(),
                extra_usage: None,
            };
            let report = Report {
                at: now(),
                outcome: Outcome::Ok(usage.clone()),
                last_ok: None,
            };
            assert_eq!(
                chip_limits(&usage).len(),
                n.min(MAX_CHIP_METERS),
                "{n} active limits"
            );
            assert_eq!(
                preems(&chip_state(
                    &status(Mode::Subscription, false, 1, 0, Last::Ok),
                    Some(&report)
                ))
                .len(),
                n.min(MAX_CHIP_METERS),
                "{n} active limits ⇒ min(n, {MAX_CHIP_METERS}) meters"
            );
            assert_eq!(
                bars(&panel(
                    DEFAULT_TITLE,
                    &status(Mode::Subscription, false, 1, 0, Last::Ok),
                    Some(&report),
                    now()
                ))
                .len(),
                n,
                "{n} limits ⇒ {n} panel rows"
            );
        }
    }

    /// An **inactive** limit keeps its panel row and never takes a chip meter
    /// — with the inactive row placed **first**, the shape the real endpoint
    /// has actually sent on this account (the PR's own live-verify record:
    /// three rows on 2026-09-13, only one `is_active`).
    ///
    /// This is deliberately not the captured response's own order (inactive
    /// third): `.take(MAX_CHIP_METERS)` alone would remove that row too, so a
    /// mutation that deletes `chip_limits`'s `.filter(|limit| limit.active())`
    /// is invisible against that fixture — the meters come out identical
    /// either way. Leading with the inactive row makes the two lists differ:
    /// without the filter the first meter would be `weekly_scoped`, not
    /// `session`. Falsify by deleting that filter: red, on the id list below.
    #[test]
    fn an_inactive_limit_is_a_panel_row_but_never_a_chip_meter() {
        let usage = Usage {
            limits: vec![
                limit(
                    "weekly_scoped",
                    25.0,
                    "normal",
                    false,
                    "2026-09-17T15:00:00.102097+00:00",
                ),
                limit(
                    "session",
                    80.0,
                    "warning",
                    true,
                    "2026-09-13T13:50:00.101848+00:00",
                ),
                limit(
                    "weekly_all",
                    40.0,
                    "normal",
                    true,
                    "2026-09-17T15:00:00.101872+00:00",
                ),
            ],
            extra_usage: Some(ExtraUsage::default()),
        };
        let report = Report {
            at: now() - 120,
            outcome: Outcome::Ok(usage),
            last_ok: None,
        };
        let board = status(Mode::Subscription, false, 4, 0, Last::Ok);
        let meters = preems(&chip_state(&board, Some(&report)));
        assert_eq!(meters.len(), 2, "two of the three rows are active");
        let ids: Vec<&str> = meters.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "claude-bridge-led-0-session",
                "claude-bridge-led-1-weekly_all"
            ],
            "the leading, inactive row is skipped rather than masked by \
             `.take` — ids are indexed AND named, so two rows of one kind \
             cannot collide"
        );

        let rows = bars(&panel(DEFAULT_TITLE, &board, Some(&report), now()));
        assert_eq!(rows.len(), 3, "the inactive row is dimmed, not dropped");
        assert!(
            texts(&panel(DEFAULT_TITLE, &board, Some(&report), now()))
                .iter()
                .any(|t| t.contains("not counting right now")),
            "and it says why it is greyed"
        );
    }

    /// The meter levels are the percentages, and their ink is the server's
    /// severity — the **severity → role** table, walked through a real tree.
    #[test]
    fn the_meters_carry_the_level_and_the_severity_ink() {
        let report = captured_report();
        let meters = preems(&chip_state(
            &status(Mode::Api, true, 9, 0, Last::Ok),
            Some(&report),
        ));
        let levels: Vec<(f32, Option<AccentRole>)> = meters
            .iter()
            .map(|(_, widget)| match widget {
                PreemWidget::LedStrip { config, state } => (state.level, config.style.accent),
                other => panic!("a chip meter must be a level strip, got {other:?}"),
            })
            .collect();
        assert_eq!(
            levels,
            vec![
                (0.80, Some(AccentRole::Warning)),
                (0.40, Some(AccentRole::Accent)),
            ]
        );
    }

    /// **The severity mapping table**, both halves, from the same three words —
    /// and an unknown severity reads as normal rather than blanking the meter,
    /// because the endpoint is undocumented and will grow words.
    #[test]
    fn severity_maps_to_one_ink_and_one_class() {
        for (severity, role, class) in [
            ("normal", AccentRole::Accent, "accent"),
            ("warning", AccentRole::Warning, "warning"),
            ("critical", AccentRole::Error, "error"),
            (" warning ", AccentRole::Warning, "warning"),
            ("spicy", AccentRole::Accent, "accent"),
            ("", AccentRole::Accent, "accent"),
        ] {
            assert_eq!(severity_role(severity), role, "{severity:?}");
            assert_eq!(severity_class(severity), class, "{severity:?}");
        }
    }

    /// Each meter carries its own hover naming its bucket, its percentage and
    /// its reset — the chip is otherwise two wordless bars.
    #[test]
    fn each_meter_hovers_with_its_own_bucket_and_reset() {
        let report = captured_report();
        let hovers = tooltips(&chip_state(
            &status(Mode::Subscription, false, 4, 0, Last::Ok),
            Some(&report),
        ));
        assert!(
            hovers.contains(&"Session (5 h): 80% — resets in 2 h 15 min".to_owned()),
            "{hovers:?}"
        );
        assert!(
            hovers.contains(&"Weekly (all): 40% — resets in 4 d 3 h".to_owned()),
            "{hovers:?}"
        );
        // A row with no readable reset still hovers with what it does know.
        assert_eq!(
            meter_tooltip(
                &Limit {
                    kind: "session".to_owned(),
                    percent: 12.0,
                    ..Limit::default()
                },
                now()
            ),
            "Session (5 h): 12%"
        );
    }

    // ── The chip's unusable-usage states ─────────────────────────────────────

    /// **Usage unavailable ⇒ no meters, the chip stays, and the hover says
    /// why.** Every failure arm, plus the never-polled state.
    #[test]
    fn a_failed_poll_costs_the_meters_and_never_the_chip() {
        let board = status(Mode::Subscription, false, 18, 0, Last::Ok);
        let base = "Claude bridge · subscription · 18 served, 0 failed";

        for error in [
            UsageError::Unauthorized,
            UsageError::NoCredentials("/home/a/.claude/.credentials.json".into()),
            UsageError::Http(503),
            UsageError::Io("connection refused".to_owned()),
            UsageError::Parse("expected value at line 1".to_owned()),
        ] {
            let report = Report {
                at: now() - 30,
                outcome: Outcome::Failed(error.clone()),
                last_ok: None,
            };
            let tree = chip_state(&board, Some(&report));
            assert!(preems(&tree).is_empty(), "{error:?} ⇒ no meters");
            assert_eq!(
                texts(&tree),
                vec![CLAUDE_ICON, "emblem-ok-symbolic", "sub", "18/0"],
                "{error:?} ⇒ the chip itself is untouched"
            );
            assert_eq!(
                root_tooltip(&tree),
                Some(format!("{base}\n{}", error.sentence(now()))),
                "{error:?} ⇒ the hover carries the one-line sentence"
            );
        }

        // Before the first poll there is nothing to say, so nothing is said —
        // the #957 tooltip is byte-identical to what it was before #1236.
        let tree = chip_state(&board, None);
        assert!(preems(&tree).is_empty());
        assert_eq!(root_tooltip(&tree), Some(base.to_owned()));
    }

    /// **A wedged poll goes visibly stale, past [`usage::STALE_AFTER`].**
    ///
    /// The report is a *successful* one — the case with no error to already
    /// say so — and old enough that only a poll task that stopped publishing
    /// entirely explains it (an ordinary failure would have republished a
    /// fresh `at` on its own five-minute cadence). The chip drops the meters
    /// and the hover says since when, rather than keeping last hour's numbers
    /// on the bar looking current.
    #[test]
    fn a_wedged_poll_drops_its_meters_past_the_staleness_ceiling() {
        let board = status(Mode::Subscription, false, 18, 0, Last::Ok);
        let base = "Claude bridge · subscription · 18 served, 0 failed";
        let ceiling = i64::try_from(usage::STALE_AFTER.as_secs()).expect("fits");

        let fresh = Report {
            at: now() - ceiling,
            ..captured_report()
        };
        let tree = chip_state(&board, Some(&fresh));
        assert!(!preems(&tree).is_empty(), "still inside the ceiling");
        assert_eq!(
            root_tooltip(&tree),
            Some(base.to_owned()),
            "nothing extra yet"
        );

        let stale = Report {
            at: now() - ceiling - 1,
            ..captured_report()
        };
        let tree = chip_state(&board, Some(&stale));
        assert!(
            preems(&tree).is_empty(),
            "one second past the ceiling ⇒ no meters"
        );
        assert_eq!(
            root_tooltip(&tree),
            Some(format!(
                "{base}\nusage stale since {}",
                usage::format_utc(stale.at)
            )),
            "the hover names when the numbers stopped being current"
        );
    }

    /// **#1285's review, MED — a stale *failure*'s hover carries both lines,
    /// sentence first, stamp second**, never dropping "next try in N min"
    /// for "usage stale since …" the way a stamp-wins rule did before this
    /// fix (the #1254 N2 hover-contradicts-panel shape, resurfacing exactly
    /// in the sustained-429 case #1283 was filed for). A stale *success*
    /// keeps today's stamp-only wording — pinned already by
    /// `a_wedged_poll_drops_its_meters_past_the_staleness_ceiling` above.
    ///
    /// (Lifted from the review as
    /// `a_stale_failures_hover_dates_the_numbers_not_the_attempt`, adjusted
    /// to the combined wording item 3 settled on.)
    ///
    /// Falsify by reverting `tooltip`'s stale arm to always answer just the
    /// stamp (dropping `usage_failure_sentence` on the failure branch): the
    /// sentence line vanishes and this goes red. Falsify the stamp's own
    /// clock separately by reverting `numbers_at().unwrap_or(at)` there to
    /// plain `report.at`: `left: …stale since <fetched>` /
    /// `right: …stale since <now()>`.
    #[test]
    fn a_stale_failures_hover_dates_the_numbers_not_the_attempt() {
        let board = status(Mode::Subscription, false, 18, 0, Last::Ok);
        let ceiling = i64::try_from(usage::STALE_AFTER.as_secs()).expect("fits");
        let fetched = now() - ceiling - 1;
        let report = Report {
            at: now(),
            outcome: Outcome::Failed(UsageError::Http(429)),
            last_ok: Some((fetched, captured_usage())),
        };
        assert_eq!(
            root_tooltip(&chip_state(&board, Some(&report))),
            Some(format!(
                "Claude bridge · subscription · 18 served, 0 failed\n\
                 usage unavailable — the usage endpoint answered HTTP 429\n\
                 usage stale since {}",
                usage::format_utc(fetched)
            )),
            "the sentence stays first, and the stamp is last_ok's clock — \
             never the failed attempt's own `at`"
        );
    }

    // ── #1283: a failure keeps the last good numbers on the meters ──────────

    /// **Item 1(a): a failed poll after a success keeps the meters up**, drawn
    /// from `Report::last_ok`, with the failure's own sentence on the hover —
    /// the numbers and the explanation are not the same job.
    ///
    /// Falsify by reverting `Report::usage`'s `Failed` arm to ignore
    /// `last_ok` (`Outcome::Failed(_) => None`, the pre-#1283 shape): the
    /// meters assertion goes red.
    #[test]
    fn a_failed_poll_after_a_success_keeps_the_meters_and_still_names_the_failure() {
        let board = status(Mode::Subscription, false, 18, 0, Last::Ok);
        let base = "Claude bridge · subscription · 18 served, 0 failed";
        let report = Report {
            at: now() - 30,
            // Deadline = this attempt's `at` (now() - 30) + the 10 min wait
            // `next_wait` would have chosen — rendered at `now()` that is 570 s
            // left, which still rounds UP to "10 min" (#1285's review, NIT).
            outcome: Outcome::Failed(UsageError::RateLimited(now() - 30 + 600)),
            last_ok: Some((now() - 150, captured_usage())),
        };

        let tree = chip_state(&board, Some(&report));
        assert!(!preems(&tree).is_empty(), "the last-good meters stay up");
        assert_eq!(
            root_tooltip(&tree),
            Some(format!("{base}\nusage rate-limited — next try in 10 min")),
            "the hover still names the failure, on its own line"
        );

        let panel_text = texts(&panel(DEFAULT_TITLE, &board, Some(&report), now()));
        assert!(
            panel_text.contains(&"Session (5 h)".to_owned()),
            "the panel also keeps the last-good rows: {panel_text:?}"
        );
        assert!(
            panel_text.contains(&"usage rate-limited — next try in 10 min".to_owned()),
            "…with the failure as a footer, not a replacement: {panel_text:?}"
        );
    }

    /// **#1285's review, HIGH-1: the panel header dates the numbers on
    /// screen, not the failed attempt that produced no numbers.** A fresh
    /// 429 over 5-minute-old `last_ok` numbers must read "updated 5 min
    /// ago", never "updated just now" — the header is naming when the rows
    /// it is drawing were actually fetched.
    ///
    /// Falsify by reverting `header`'s `numbers_at().unwrap_or(at)` back to
    /// plain `report.at`: the second assertion goes red (`"updated just
    /// now"` instead of `"updated 5 min ago"`), while the no-`last_ok`
    /// failure case (`the_panel_explains_itself_when_there_is_no_list`)
    /// stays green either way — `numbers_at()` is `None` there, so both
    /// spellings agree.
    #[test]
    fn the_panel_header_dates_the_numbers_not_the_failed_attempt() {
        let board = status(Mode::Subscription, false, 18, 0, Last::Ok);
        let report = Report {
            at: now(),
            outcome: Outcome::Failed(UsageError::RateLimited(now() + 600)),
            last_ok: Some((now() - 300, captured_usage())),
        };
        let text = texts(&panel(DEFAULT_TITLE, &board, Some(&report), now()));
        assert!(
            text.contains(&"Session (5 h)".to_owned()),
            "rows from last_ok: {text:?}"
        );
        assert!(
            text.contains(&"updated 5 min ago".to_owned()),
            "the header must age the numbers on screen, not the failed attempt: {text:?}"
        );
    }

    /// **#1285's review, HIGH-2: the panel's staleness gate mirrors the
    /// chip's.** A four-hour-old `last_ok` behind a fresh 429 draws no rows
    /// and no bars at all — just the staleness stamp and the failure
    /// footer — exactly like the chip that has already, correctly, dropped
    /// its own meters one click above it.
    ///
    /// Falsify by reverting `panel`'s gate to plain `report.usage()` (no
    /// `.filter(|_| !report.is_stale(now))`): the first two assertions go
    /// red — `Session (5 h)` and its bar render under an "updated just
    /// now" header even though the numbers are four hours old.
    #[test]
    fn a_stale_last_ok_behind_a_fresh_429_renders_no_rows_the_stale_label_and_the_footer() {
        let board = status(Mode::Subscription, false, 18, 0, Last::Ok);
        let four_hours = 4 * 3_600;
        let report = Report {
            at: now(),
            outcome: Outcome::Failed(UsageError::RateLimited(now() + 1_800)),
            last_ok: Some((now() - four_hours, captured_usage())),
        };
        let tree = panel(DEFAULT_TITLE, &board, Some(&report), now());
        let text = texts(&tree);
        assert!(
            !text.contains(&"Session (5 h)".to_owned()),
            "no rows from four-hour-old numbers: {text:?}"
        );
        assert!(bars(&tree).is_empty(), "no bars either: {text:?}");
        assert!(
            text.contains(&format!(
                "usage stale since {}",
                usage::format_utc(now() - four_hours)
            )),
            "the panel names when the numbers stopped being current: {text:?}"
        );
        assert!(
            text.iter().any(|t| t.starts_with("usage rate-limited")),
            "…and still keeps the failure footer: {text:?}"
        );
    }

    /// **Item 1(b): once `last_ok` itself is past [`usage::STALE_AFTER`], the
    /// chip drops the meters — the same as any other stale report — even
    /// though the failure that produced this `Report` is fresh.**
    #[test]
    fn a_last_ok_older_than_the_ceiling_still_drops_the_meters() {
        let board = status(Mode::Subscription, false, 18, 0, Last::Ok);
        let ceiling = i64::try_from(usage::STALE_AFTER.as_secs()).expect("fits");
        let report = Report {
            at: now(),
            outcome: Outcome::Failed(UsageError::Http(429)),
            last_ok: Some((now() - ceiling - 1, captured_usage())),
        };
        let tree = chip_state(&board, Some(&report));
        assert!(
            preems(&tree).is_empty(),
            "last_ok is one second past the ceiling, even though `at` is now"
        );
    }

    /// **Item 1(c): a first poll that fails has no `last_ok`, and renders
    /// exactly as it always has** — this is the pre-existing
    /// `a_failed_poll_costs_the_meters_and_never_the_chip` case, named here
    /// once more so the #1283 build issue's three test items are each
    /// findable by name.
    #[test]
    fn a_first_poll_failure_has_no_last_ok_and_is_unchanged() {
        let board = status(Mode::Subscription, false, 0, 0, Last::None);
        let report = Report {
            at: now(),
            outcome: Outcome::Failed(UsageError::Unauthorized),
            last_ok: None,
        };
        assert!(preems(&chip_state(&board, Some(&report))).is_empty());
        assert!(
            bars(&panel(DEFAULT_TITLE, &board, Some(&report), now())).is_empty(),
            "no rows at all — nothing to fall back on"
        );
        assert!(
            texts(&panel(DEFAULT_TITLE, &board, Some(&report), now()))
                .contains(&"usage stale — run `claude` once to refresh the login".to_owned())
        );
    }

    /// **API-key mode has no login to sign in to.** `NoCredentials` in every
    /// other mode reads its own sentence — "run `claude` once to sign in" —
    /// but `api` mode never spawns `claude`, so that is advice for a login
    /// this bridge does not need. The usage limits belong to the box's Claude
    /// Code login regardless of which backend answers, so the hover says
    /// that instead.
    #[test]
    fn an_api_mode_bridge_gets_a_subscription_note_not_a_sign_in_prompt() {
        let error = UsageError::NoCredentials("/home/a/.claude/.credentials.json".into());
        let report = Report {
            at: now() - 30,
            outcome: Outcome::Failed(error.clone()),
            last_ok: None,
        };

        let api = status(Mode::Api, true, 9, 0, Last::Ok);
        let hover = root_tooltip(&chip_state(&api, Some(&report))).expect("a hover");
        assert!(
            hover.contains("usage limits are a subscription feature"),
            "{hover:?}"
        );
        assert!(
            !hover.contains("run `claude` once"),
            "api mode never spawns claude: {hover:?}"
        );

        // Every claude-spawning mode is unaffected — the sign-in prompt is
        // exactly right there.
        for mode in [Mode::Subscription, Mode::Reprompt] {
            let board = status(mode, false, 9, 0, Last::Ok);
            let hover = root_tooltip(&chip_state(&board, Some(&report))).expect("a hover");
            assert!(
                hover.contains(&error.sentence(now())),
                "{mode:?}: {hover:?}"
            );
        }
    }

    /// The chip hover's second line and the drawer panel's error sentence say
    /// the SAME thing for the same `Status`/error — both routed through
    /// [`usage_failure_sentence`], the one function that knows the mode
    /// adjustment, rather than the panel reading [`UsageError::sentence`] on
    /// its own (#1254's review, N2: an API-key bridge's hover said "usage
    /// limits are a subscription feature" while the panel one click beneath
    /// it still said "run `claude` once to sign in").
    ///
    /// One test per mode, each against the LITERAL wording for its mode —
    /// not `usage_failure_sentence`'s own answer. Deriving `expected` from
    /// that function (both surfaces under test call it) made the assertion
    /// `f(x) == f(x)`: it agreed with itself under a mutation that swapped
    /// the mode adjustment onto the wrong modes, so no test named which mode
    /// broke, contrary to what this doc claimed (#1278's review, F1).
    fn assert_panel_agrees_with_chip_hover(
        mode: Mode,
        keyed: bool,
        expect_subscription_note: bool,
    ) {
        let error = UsageError::NoCredentials("/home/a/.claude/.credentials.json".into());
        let report = Report {
            at: now() - 30,
            outcome: Outcome::Failed(error.clone()),
            last_ok: None,
        };
        let board = status(mode, keyed, 9, 0, Last::Ok);

        let hover = root_tooltip(&chip_state(&board, Some(&report))).expect("a hover");
        let panel_text = texts(&panel(DEFAULT_TITLE, &board, Some(&report), now()));

        if expect_subscription_note {
            assert!(
                hover.contains("usage limits are a subscription feature"),
                "{mode:?} hover: {hover:?}"
            );
            assert!(
                !hover.contains("run `claude` once"),
                "{mode:?} hover: api mode never spawns claude: {hover:?}"
            );
            assert!(
                panel_text
                    .iter()
                    .any(|t| t.contains("usage limits are a subscription feature")),
                "{mode:?} panel: {panel_text:?}"
            );
            assert!(
                !panel_text.iter().any(|t| t.contains("run `claude` once")),
                "{mode:?} panel: api mode never spawns claude: {panel_text:?}"
            );
        } else {
            assert!(
                hover.ends_with(&error.sentence(now())),
                "{mode:?} hover does not carry the failure sentence: {hover:?}"
            );
            assert!(
                panel_text.contains(&error.sentence(now())),
                "{mode:?} panel does not carry the failure sentence: {panel_text:?}"
            );
        }
    }

    #[test]
    fn the_panel_agrees_with_the_chip_hover_in_subscription_mode() {
        assert_panel_agrees_with_chip_hover(Mode::Subscription, false, false);
    }

    #[test]
    fn the_panel_agrees_with_the_chip_hover_in_reprompt_mode() {
        assert_panel_agrees_with_chip_hover(Mode::Reprompt, false, false);
    }

    /// The mode that actually differs: falsify by reverting `panel`'s
    /// threading (back to `report.error()`/`error.sentence()` with no
    /// `&Status`) — this test goes red, since the panel would then render
    /// `NoCredentials`'s literal "run `claude` once to sign in" instead of the
    /// subscription-feature note the hover gives in `api` mode.
    #[test]
    fn the_panel_agrees_with_the_chip_hover_in_api_mode() {
        assert_panel_agrees_with_chip_hover(Mode::Api, true, true);
    }

    /// No error text anywhere in the chip may carry a bearer token. The arms
    /// that could — the two that wrap borrowed text — are scrubbed in
    /// `usage::fetch`; this pins that the chip does not reintroduce one by, say,
    /// formatting a `Debug`.
    #[test]
    fn the_chip_never_renders_anything_token_shaped() {
        let report = Report {
            at: now(),
            outcome: Outcome::Failed(UsageError::Io("<redacted> refused".to_owned())),
            last_ok: None,
        };
        let tree = chip_state(&status(Mode::Api, true, 1, 0, Last::Ok), Some(&report));
        let rendered = format!("{:?}{:?}", texts(&tree), tooltips(&tree));
        assert!(!rendered.contains("sk-ant"), "{rendered}");
    }

    // ── The panel ────────────────────────────────────────────────────────────

    /// The panel is every row, in the server's order, with both halves of each
    /// reset time and a header saying how fresh the numbers are — now
    /// [`usage_card`]-wrapped (#1280 P1): the returned tree's own id is still
    /// [`PANEL_ROOT_ID`] (nothing that read "the panel's root" before this
    /// change reads anything different), it now carries [`CARD_CLASS`], and
    /// the one new node the wrapper adds nests everything this test already
    /// checked — the header text, the rows, the bars — one level inside,
    /// under [`PANEL_LIST_ID`]. Falsify by dropping `CARD_CLASS` from
    /// [`usage_card`]'s children: the class assertion below reds.
    #[test]
    fn the_panel_lists_every_row_with_both_halves_of_its_reset() {
        let report = captured_report();
        let tree = panel(DEFAULT_TITLE, &default_status(), Some(&report), now());
        let text = texts(&tree);
        assert_eq!(text.first().map(String::as_str), Some("Claude usage"));
        assert!(text.contains(&"updated 2 min ago".to_owned()), "{text:?}");
        assert!(text.contains(&"Session (5 h)".to_owned()), "{text:?}");
        assert!(text.contains(&"Weekly (all)".to_owned()), "{text:?}");
        assert!(text.contains(&"Weekly (scoped)".to_owned()), "{text:?}");
        assert!(
            text.contains(&"session · resets 2026-09-13 13:50 UTC · in 2 h 15 min".to_owned()),
            "{text:?}"
        );
        assert_eq!(
            bars(&tree),
            vec![
                (
                    "claude-bridge-bar-0-session".to_owned(),
                    0.80,
                    vec!["warning".to_owned()]
                ),
                (
                    "claude-bridge-bar-1-weekly_all".to_owned(),
                    0.40,
                    vec!["accent".to_owned()]
                ),
                (
                    "claude-bridge-bar-2-weekly_scoped".to_owned(),
                    0.25,
                    vec!["accent".to_owned()]
                ),
            ]
        );
        match &tree {
            Node::Box {
                id,
                classes,
                children,
                ..
            } => {
                assert_eq!(id.as_deref(), Some(PANEL_ROOT_ID));
                assert_eq!(
                    classes,
                    &vec![CARD_CLASS.to_owned()],
                    "the drawer panel is wrapped in the same card class as the sidebar card"
                );
                assert_eq!(children.len(), 1, "the wrapper's one new node: the list");
                match &children[0] {
                    Node::Box { id, .. } => assert_eq!(id.as_deref(), Some(PANEL_LIST_ID)),
                    other => panic!("the panel's list must be a box, got {other:?}"),
                }
            }
            other => panic!("the panel root is a box, got {other:?}"),
        }
    }

    /// The three not-a-list states each say what they know instead of showing an
    /// empty page.
    #[test]
    fn the_panel_explains_itself_when_there_is_no_list() {
        let board = default_status();
        let text = texts(&panel(DEFAULT_TITLE, &board, None, now()));
        assert!(text.contains(&"not fetched yet".to_owned()), "{text:?}");
        assert!(text.contains(&"fetching usage…".to_owned()), "{text:?}");

        let failed = Report {
            at: now() - 90,
            outcome: Outcome::Failed(UsageError::Unauthorized),
            last_ok: None,
        };
        let text = texts(&panel(DEFAULT_TITLE, &board, Some(&failed), now()));
        assert!(text.contains(&"updated 1 min ago".to_owned()), "{text:?}");
        assert!(
            text.contains(&"usage stale — run `claude` once to refresh the login".to_owned()),
            "{text:?}"
        );
        assert!(bars(&panel(DEFAULT_TITLE, &board, Some(&failed), now())).is_empty());

        let empty = Report {
            at: now(),
            outcome: Outcome::Ok(Usage::default()),
            last_ok: None,
        };
        let text = texts(&panel(DEFAULT_TITLE, &board, Some(&empty), now()));
        assert!(
            text.contains(&"this account reported no limits".to_owned()),
            "{text:?}"
        );
    }

    /// `extra_usage` is a row **only** when the account has it enabled — which
    /// the captured response does not, and most accounts do not.
    #[test]
    fn the_extra_usage_row_appears_only_when_it_is_enabled() {
        let board = default_status();
        let off = captured_report();
        assert!(
            !texts(&panel(DEFAULT_TITLE, &board, Some(&off), now()))
                .contains(&"Extra usage".to_owned()),
            "a disabled allowance is not a row"
        );

        let on = Report {
            at: now(),
            outcome: Outcome::Ok(Usage {
                limits: Vec::new(),
                extra_usage: Some(ExtraUsage {
                    is_enabled: true,
                    utilization: Some(30.0),
                    used_credits: Some(15.0),
                    monthly_limit: Some(50.0),
                    currency: Some("USD".to_owned()),
                }),
            }),
            last_ok: None,
        };
        let text = texts(&panel(DEFAULT_TITLE, &board, Some(&on), now()));
        assert!(text.contains(&"Extra usage".to_owned()), "{text:?}");
        assert!(text.contains(&"30%".to_owned()), "{text:?}");
        assert!(text.contains(&"15.00 of 50.00 USD".to_owned()), "{text:?}");
        assert_eq!(
            bars(&panel(DEFAULT_TITLE, &board, Some(&on), now())),
            vec![(
                "claude-bridge-bar-extra".to_owned(),
                0.30,
                vec!["accent".to_owned()]
            )]
        );
    }

    /// **#1285's review, LOW — "the footer appears only when the latest poll
    /// failed" was otherwise unpinned.** A successful report's panel names no
    /// failure at all.
    ///
    /// Falsify by having `failure_footer` answer `Some(…)` regardless of
    /// `report.outcome` (e.g. dropping its `let Outcome::Failed(error) = …
    /// else { return None; }` guard): this goes red.
    #[test]
    fn a_successful_panel_carries_no_failure_footer() {
        let board = status(Mode::Subscription, false, 9, 0, Last::Ok);
        let text = texts(&panel(
            DEFAULT_TITLE,
            &board,
            Some(&captured_report()),
            now(),
        ));
        assert!(
            !text.iter().any(|t| t.starts_with("usage ")),
            "a success names no failure: {text:?}"
        );
    }

    // ── The #866/#957 chip contract, unchanged ───────────────────────────────

    /// A subscription-mode bridge that has served nothing: loading glyph, `sub`,
    /// no key glyph (nothing is held — `claude` owns the session), no counts,
    /// and no meters (nothing polled yet).
    #[test]
    fn a_fresh_subscription_bridge_shows_no_key_and_no_counts() {
        let tree = chip_state(&status(Mode::Subscription, false, 0, 0, Last::None), None);
        assert_eq!(
            texts(&tree),
            vec![CLAUDE_ICON, "content-loading-symbolic", "sub"]
        );
    }

    /// An `api`-mode bridge holding a key: the key glyph is present, and the
    /// counts appear once something has been served.
    #[test]
    fn a_keyed_api_bridge_shows_the_key_glyph_and_its_counts() {
        let tree = chip_state(&status(Mode::Api, true, 12, 1, Last::Ok), None);
        assert_eq!(
            texts(&tree),
            vec![
                CLAUDE_ICON,
                "emblem-ok-symbolic",
                "api",
                "dialog-password-symbolic",
                "12/1",
            ]
        );
    }

    /// The absence of the key glyph is the load-bearing signal — it says "no
    /// credential is held here". A `claude` mode must never paint it, whatever
    /// the traffic looks like.
    #[test]
    fn a_claude_mode_never_paints_the_key_glyph() {
        for mode in [Mode::Subscription, Mode::Reprompt] {
            let tree = chip_state(&status(mode, false, 3, 0, Last::Ok), None);
            assert!(
                !texts(&tree).iter().any(|t| t.contains("password")),
                "{mode:?} holds no credential of its own"
            );
        }
    }

    /// Before `main` publishes the startup facts the chip says so, rather than
    /// inventing a mode.
    #[test]
    fn an_unpublished_status_renders_a_muted_placeholder() {
        let tree = chip_state(
            &Status {
                startup: None,
                ok: 0,
                errors: 0,
                last: Last::None,
            },
            None,
        );
        assert_eq!(
            texts(&tree),
            vec![CLAUDE_ICON, "content-loading-symbolic", "…"]
        );
    }

    /// The Claude glyph leads in **every** state — including the two the chip
    /// treats specially (nothing published yet; nothing served yet) and with the
    /// meters present. Annika's ask on #957 was that the pill be identifiable at
    /// a glance, which it isn't if the identity only shows up once the daemon has
    /// settled.
    #[test]
    fn the_claude_glyph_always_leads_the_chip() {
        let mut boards = vec![Status {
            startup: None,
            ok: 0,
            errors: 0,
            last: Last::None,
        }];
        for mode in [Mode::Subscription, Mode::Reprompt, Mode::Api] {
            for keyed in [false, true] {
                for last in [Last::None, Last::Ok, Last::Error] {
                    boards.push(status(mode, keyed, 7, 2, last));
                }
            }
        }
        let reports = [None, Some(captured_report())];
        for board in &boards {
            for report in &reports {
                let tree = chip_state(board, report.as_ref());
                assert_eq!(
                    texts(&tree).first().map(String::as_str),
                    Some(CLAUDE_ICON),
                    "{board:?} must still lead with the Claude glyph"
                );
            }
        }
    }

    /// The #957 fix itself: hovering the pill spells out everything it
    /// abbreviates. Mode by name, counts as words, on the box **inside** the
    /// click button so any pixel of the chip that isn't a meter answers.
    #[test]
    fn the_root_box_carries_the_spelled_out_tooltip() {
        let tree = chip_state(&status(Mode::Subscription, false, 18, 0, Last::Ok), None);
        assert_eq!(
            root_tooltip(&tree).as_deref(),
            Some("Claude bridge · subscription · 18 served, 0 failed"),
            "this is literally Mara's `sub 18/0`, in words"
        );
        // …and the chip is a button, so the hover survives being clickable.
        match &tree {
            Node::Button { id, classes, .. } => {
                assert_eq!(id, CHIP_BTN);
                assert_eq!(classes, &vec!["flat".to_owned()], "no second frame");
            }
            other => panic!("the chip root is the click button, got {other:?}"),
        }
    }

    /// Before anything is served the tooltip says so, rather than printing the
    /// `0 served, 0 failed` the chip itself deliberately refuses to render.
    #[test]
    fn the_tooltip_says_nothing_served_yet_before_the_first_request() {
        assert_eq!(
            tooltip(&status(Mode::Api, true, 0, 0, Last::None), None, now()),
            "Claude bridge · API key · nothing served yet"
        );
    }

    /// …and before `main` publishes the startup facts it names no mode at all —
    /// the same honesty as the `…` label.
    #[test]
    fn the_tooltip_names_no_mode_before_the_backend_is_settled() {
        let board = Status {
            startup: None,
            ok: 0,
            errors: 0,
            last: Last::None,
        };
        assert_eq!(tooltip(&board, None, now()), "Claude bridge · starting up");
        assert_eq!(
            root_tooltip(&chip_state(&board, None)).as_deref(),
            Some(tooltip(&board, None, now()).as_str())
        );
    }

    /// One source of truth: the tooltip's spelled-out mode and the chip's
    /// three-letter one come from the same match, so they can never describe
    /// different backends. Every mode gets a distinct, non-empty long name.
    #[test]
    fn the_tooltip_names_the_backend_the_label_abbreviates() {
        let modes = [Mode::Subscription, Mode::Reprompt, Mode::Api];
        let names = modes.map(mode_name);
        assert_eq!(names, ["subscription", "re-prompt", "API key"]);
        let mut sorted = names;
        sorted.sort_unstable();
        sorted
            .windows(2)
            .for_each(|w| assert_ne!(w[0], w[1], "names must not collide"));
        for (mode, name) in modes.into_iter().zip(names) {
            let board = status(mode, false, 1, 0, Last::Ok);
            let hover = tooltip(&board, None, now());
            assert!(hover.contains(name), "{hover:?} must name {name}");
            // …and the chip is still printing the short form of that same mode.
            assert_eq!(texts(&chip_state(&board, None))[2], mode_label(mode));
        }
    }

    /// The chip saturates at `999+` to keep its width; the tooltip has no width
    /// to defend, so it reports the real numbers.
    #[test]
    fn the_tooltip_reports_the_uncapped_counts() {
        let hover = tooltip(
            &status(Mode::Reprompt, false, 86_400, 12_345, Last::Error),
            None,
            now(),
        );
        assert_eq!(
            hover,
            "Claude bridge · re-prompt · 86400 served, 12345 failed"
        );
        assert_eq!(
            counts_label(86_400, 12_345),
            "999+/999+",
            "the chip still caps"
        );
    }

    #[test]
    fn the_health_glyph_follows_the_last_request() {
        assert_eq!(health_icon(Last::None), "content-loading-symbolic");
        assert_eq!(health_icon(Last::Ok), "emblem-ok-symbolic");
        assert_eq!(health_icon(Last::Error), "dialog-warning-symbolic");
    }

    /// Every mode gets a distinct label — "which backend am I paying for" is the
    /// question the chip exists to answer at a glance.
    #[test]
    fn every_mode_has_a_distinct_label() {
        let labels = [Mode::Subscription, Mode::Reprompt, Mode::Api].map(mode_label);
        assert_eq!(labels, ["sub", "rep", "api"]);
        let mut sorted = labels;
        sorted.sort_unstable();
        sorted
            .windows(2)
            .for_each(|w| assert_ne!(w[0], w[1], "labels must not collide"));
    }

    /// `0/0` is never rendered: an untouched bridge shows no counts at all.
    #[test]
    fn counts_are_hidden_until_something_has_been_served() {
        assert_eq!(counts_label(0, 0), "");
        assert_eq!(counts_label(1, 0), "1/0");
        assert_eq!(counts_label(0, 1), "0/1");
        assert_eq!(counts_label(9, 4), "9/4");
    }

    /// The chip's width stops growing: a bridge that has answered a pet tick a
    /// minute for a day would otherwise print five digits and shove its
    /// neighbours along the bar.
    #[test]
    fn counts_saturate_so_the_chip_cannot_widen_without_bound() {
        assert_eq!(
            counts_label(999, 0),
            "999/0",
            "the cap itself prints exactly"
        );
        assert_eq!(counts_label(1_000, 2), "999+/2");
        assert_eq!(counts_label(86_400, 12_345), "999+/999+");
        // Four characters is the widest either side can ever be.
        for n in [0, 1, 999, 1_000, u64::MAX] {
            assert!(capped(n).len() <= 4, "{n} rendered as {}", capped(n));
        }
    }

    /// The frames this plugin puts on the wire are valid: the `Register`
    /// manifest and a `Render` of its chip **and panel** both round-trip through
    /// the codec — including the preem meters, which are the newest thing on it.
    #[test]
    fn register_and_render_frames_round_trip() {
        let reg = PluginMsg::Register {
            manifest: BridgeChip::manifest(),
        };
        let back: PluginMsg = decode(&encode(&reg)).expect("register frame decodes");
        assert_eq!(reg, back);

        // `.view()` reads the *real* clock (`usage::now_unix()`), unlike
        // `chip_state`'s fixed `now()` — so the report has to be fresh against
        // real time, not the fixed narrative time the other tests use, or the
        // #1236 staleness ceiling drops the meters this test is asserting on.
        // `fresh_report()` is the one place that anchoring lives.
        let view = hytte_plugin::display::testing::with_render_mode(RenderMode::State, || {
            BridgeChip {
                status: status(Mode::Api, true, 1, 0, Last::Ok),
                usage: Some(fresh_report()),
                usage_version: 1,
                is_bar: true,
                title: DEFAULT_TITLE.to_owned(),
            }
            .view()
        });
        assert!(!preems(&view.tree).is_empty(), "the meters are on the wire");
        let render = PluginMsg::Render {
            tree: view.tree,
            panel: view.panel.map(Box::new),
            hidden_on: view.hidden_on,
            effects: vec![],
        };
        let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
        assert_eq!(render, back);
    }

    /// The **raster** arm is the one a shell that never advertised the preem
    /// vocabulary gets, and it must still produce meters — pixels rather than
    /// typed state, same count, same tooltips.
    #[test]
    fn a_preem_less_host_still_gets_its_meters_as_pixels() {
        let report = captured_report();
        let tree = hytte_plugin::display::testing::with_render_mode(RenderMode::Raster, || {
            chip(
                &status(Mode::Subscription, false, 1, 0, Last::Ok),
                Some(&report),
                now(),
            )
        });
        assert!(
            preems(&tree).is_empty(),
            "a raster host receives no typed preem nodes"
        );
        let hovers = tooltips(&tree);
        assert!(
            hovers
                .iter()
                .any(|hover| hover.starts_with("Session (5 h): 80%")),
            "{hovers:?}"
        );
    }
}
