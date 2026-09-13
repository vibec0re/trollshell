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
//! poll produced nothing.
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

use std::time::Duration;

use hytte_plugin::display::{AccentRole, LedStrip, StyleName};
use hytte_plugin::proto::{Capability, Dir, Effect, EventKind, Manifest, Mount, Node, Page};
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View, tick_stream};

use crate::Mode;
use crate::status::{self, Last, Status};
use crate::usage::{self, ExtraUsage, Limit, Outcome, Report, Usage};

/// Stable plugin id — the host's mount-slot key, and the `<id>` in the
/// `trollshell-plugin-<id>.service` transient unit the launcher spawns.
const PLUGIN_ID: &str = "claude-bridge";

/// The chip's inner box — the node every glyph and meter hangs off, and the
/// carrier of the whole-pill tooltip.
const ROOT_ID: &str = "claude-bridge-root";

/// The chip button. Clicking it opens the drawer panel (#1236); before that the
/// chip was deliberately inert.
const CHIP_BTN: &str = "claude-bridge-chip";

/// The drawer panel's root.
const PANEL_ROOT_ID: &str = "claude-bridge-panel";

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

    /// Mounts [`Mount::BarRight`] as a chip. Subscribes to nothing (the chip is
    /// driven by its own tick off local boards, not by host state) and requests
    /// exactly one capability — [`Capability::OpenPage`], for its own drawer
    /// panel (#1236).
    fn manifest() -> Manifest {
        let mut manifest = Manifest::new(PLUGIN_ID, Mount::BarRight);
        manifest.capabilities = vec![Capability::OpenPage];
        manifest
    }

    fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
        Self {
            status: status::snapshot(),
            usage: usage::latest(),
            usage_version: usage::version(),
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

    fn view(&self) -> View {
        let now = usage::now_unix();
        let report = self.usage.as_ref();
        View::new(chip(&self.status, report, now)).panel(panel(report, now))
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
/// A **failed** usage poll appends its sentence on a second line. A successful
/// one appends nothing — the meters carry their own hovers, and repeating them
/// here would put the same numbers in two places that can disagree by a tick.
fn tooltip(status: &Status, report: Option<&Report>) -> String {
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
    match report.and_then(Report::error) {
        Some(sentence) => format!("{head}\n{sentence}"),
        None => head,
    }
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
    if let Some(usage) = report.and_then(Report::usage) {
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
        tooltip: Some(tooltip(status, report)),
    };
    Node::Button {
        id: CHIP_BTN.to_owned(),
        classes: vec!["flat".to_owned()],
        child: Box::new(inner),
    }
}

/// The drawer panel's header: the title, and either how fresh the numbers are or
/// why there aren't any.
fn header(report: Option<&Report>, now: i64) -> Node {
    let freshness = match report {
        None => "not fetched yet".to_owned(),
        Some(report) => format!("updated {}", usage::humanise_since(now, report.at)),
    };
    titled_row(
        label("Claude usage", &["heading"]),
        label(&freshness, &["dim-label"]),
    )
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

/// The drawer page: the header, then **every** row the server sent, then the
/// extra-usage allowance when it is on.
///
/// Three states other than a list, each of which says what it knows rather than
/// showing an empty page: nothing polled yet, the poll failed (the sentence,
/// which is the only actionable thing there is), or the account genuinely
/// reported no limits.
fn panel(report: Option<&Report>, now: i64) -> Node {
    let mut children = vec![header(report, now)];
    match report.map(|report| &report.outcome) {
        None => children.push(label("fetching usage…", &["dim-label"])),
        Some(Outcome::Failed(error)) => children.push(label(&error.sentence(), &["warning"])),
        Some(Outcome::Ok(usage)) => {
            if usage.limits.is_empty() {
                children.push(label("this account reported no limits", &["dim-label"]));
            }
            for (index, limit) in usage.limits.iter().enumerate() {
                children.push(limit_row(index, limit, now));
            }
            if let Some(extra) = usage.extra_usage.as_ref().filter(|extra| extra.is_enabled) {
                children.push(extra_row(extra));
            }
        }
    }
    Node::Box {
        id: Some(PANEL_ROOT_ID.to_owned()),
        dir: Dir::Vertical,
        spacing: 12,
        scroll: false,
        classes: Vec::new(),
        children,
        tooltip: None,
    }
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
        BridgeChip, CHIP_BTN, CLAUDE_ICON, MAX_CHIP_METERS, PANEL_ROOT_ID, Tick, capped, chip,
        chip_limits, counts_label, health_icon, meter_tooltip, mode_label, mode_name, panel,
        severity_class, severity_role, tooltip,
    };
    use crate::Mode;
    use crate::status::{Last, Startup, Status};
    use crate::usage::{ExtraUsage, Limit, Outcome, Report, Usage, UsageError, parse_rfc3339};
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

    /// The captured three-row response, as a published report.
    fn captured_report() -> Report {
        Report {
            at: now() - 120,
            outcome: Outcome::Ok(Usage {
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
            }),
        }
    }

    /// Collect every `Label`/`Icon` payload in a tree, in render order — the
    /// chip is small enough that its full text is the assertion.
    fn texts(node: &Node) -> Vec<String> {
        match node {
            Node::Label { text, .. } => vec![text.clone()],
            Node::Icon { name, .. } => vec![name.clone()],
            Node::Button { child, .. } => texts(child),
            Node::Box { children, .. } | Node::Row { children, .. } => {
                children.iter().flat_map(texts).collect()
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
        let own = match node {
            Node::Box { tooltip, .. } | Node::Row { tooltip, .. } => tooltip.clone(),
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
    #[test]
    fn the_view_always_carries_a_panel() {
        for usage in [None, Some(captured_report())] {
            let model = BridgeChip {
                status: status(Mode::Subscription, false, 0, 0, Last::None),
                usage,
                usage_version: 0,
            };
            let view = model.view();
            assert!(view.panel.is_some());
            assert!(view.hidden_on.is_empty(), "one chip, every monitor");
        }
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
                bars(&panel(Some(&report), now())).len(),
                n,
                "{n} limits ⇒ {n} panel rows"
            );
        }
    }

    /// An **inactive** limit keeps its panel row and never takes a chip meter —
    /// the captured response's third row is exactly this case.
    #[test]
    fn an_inactive_limit_is_a_panel_row_but_never_a_chip_meter() {
        let report = captured_report();
        let meters = preems(&chip_state(
            &status(Mode::Subscription, false, 4, 0, Last::Ok),
            Some(&report),
        ));
        assert_eq!(meters.len(), 2, "two of the three rows are active");
        let ids: Vec<&str> = meters.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "claude-bridge-led-0-session",
                "claude-bridge-led-1-weekly_all"
            ],
            "ids are indexed AND named, so two rows of one kind cannot collide"
        );

        let rows = bars(&panel(Some(&report), now()));
        assert_eq!(rows.len(), 3, "the inactive row is dimmed, not dropped");
        assert!(
            texts(&panel(Some(&report), now()))
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
                Some(format!("{base}\n{}", error.sentence())),
                "{error:?} ⇒ the hover carries the one-line sentence"
            );
        }

        // Before the first poll there is nothing to say, so nothing is said —
        // the #957 tooltip is byte-identical to what it was before #1236.
        let tree = chip_state(&board, None);
        assert!(preems(&tree).is_empty());
        assert_eq!(root_tooltip(&tree), Some(base.to_owned()));
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
        };
        let tree = chip_state(&status(Mode::Api, true, 1, 0, Last::Ok), Some(&report));
        let rendered = format!("{:?}{:?}", texts(&tree), tooltips(&tree));
        assert!(!rendered.contains("sk-ant"), "{rendered}");
    }

    // ── The panel ────────────────────────────────────────────────────────────

    /// The panel is every row, in the server's order, with both halves of each
    /// reset time and a header saying how fresh the numbers are.
    #[test]
    fn the_panel_lists_every_row_with_both_halves_of_its_reset() {
        let report = captured_report();
        let tree = panel(Some(&report), now());
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
            Node::Box { id, .. } => assert_eq!(id.as_deref(), Some(PANEL_ROOT_ID)),
            other => panic!("the panel root is a box, got {other:?}"),
        }
    }

    /// The three not-a-list states each say what they know instead of showing an
    /// empty page.
    #[test]
    fn the_panel_explains_itself_when_there_is_no_list() {
        let text = texts(&panel(None, now()));
        assert!(text.contains(&"not fetched yet".to_owned()), "{text:?}");
        assert!(text.contains(&"fetching usage…".to_owned()), "{text:?}");

        let failed = Report {
            at: now() - 90,
            outcome: Outcome::Failed(UsageError::Unauthorized),
        };
        let text = texts(&panel(Some(&failed), now()));
        assert!(text.contains(&"updated 1 min ago".to_owned()), "{text:?}");
        assert!(
            text.contains(&"usage stale — run `claude` once to refresh the login".to_owned()),
            "{text:?}"
        );
        assert!(bars(&panel(Some(&failed), now())).is_empty());

        let empty = Report {
            at: now(),
            outcome: Outcome::Ok(Usage::default()),
        };
        let text = texts(&panel(Some(&empty), now()));
        assert!(
            text.contains(&"this account reported no limits".to_owned()),
            "{text:?}"
        );
    }

    /// `extra_usage` is a row **only** when the account has it enabled — which
    /// the captured response does not, and most accounts do not.
    #[test]
    fn the_extra_usage_row_appears_only_when_it_is_enabled() {
        let off = captured_report();
        assert!(
            !texts(&panel(Some(&off), now())).contains(&"Extra usage".to_owned()),
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
        };
        let text = texts(&panel(Some(&on), now()));
        assert!(text.contains(&"Extra usage".to_owned()), "{text:?}");
        assert!(text.contains(&"30%".to_owned()), "{text:?}");
        assert!(text.contains(&"15.00 of 50.00 USD".to_owned()), "{text:?}");
        assert_eq!(
            bars(&panel(Some(&on), now())),
            vec![(
                "claude-bridge-bar-extra".to_owned(),
                0.30,
                vec!["accent".to_owned()]
            )]
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
            tooltip(&status(Mode::Api, true, 0, 0, Last::None), None),
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
        assert_eq!(tooltip(&board, None), "Claude bridge · starting up");
        assert_eq!(
            root_tooltip(&chip_state(&board, None)).as_deref(),
            Some(tooltip(&board, None).as_str())
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
            let hover = tooltip(&board, None);
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

        let view = hytte_plugin::display::testing::with_render_mode(RenderMode::State, || {
            BridgeChip {
                status: status(Mode::Api, true, 1, 0, Last::Ok),
                usage: Some(captured_report()),
                usage_version: 1,
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
