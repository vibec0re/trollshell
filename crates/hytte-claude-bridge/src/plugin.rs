//! The bridge's **plugin face** 🌉 — a tiny bar chip reporting what the daemon
//! is doing (issue #866, Annika's "yo make bridge hytte plugin").
//!
//! # Two hats, one binary — and the HTTP hat is the one that matters
//!
//! This crate keeps its original job: serve `POST /v1/chat/completions` on
//! loopback so pet and caw can ride a Claude subscription unchanged. #866 adds
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
//! The two runtimes never share anything but [`crate::status`]'s atomics.
//!
//! # What the chip says
//!
//! The Claude glyph, a health glyph, the mode, an optional key glyph, and coarse
//! counts:
//!
//! ```text
//! [✳] [✓] api [🔑] 12/1
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
//! - the **counts** are `<2xx>/<not-2xx>`, hidden until something has been served.
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
//! abbreviation whose key lived only in this module doc. The chip is
//! deliberately panel-less (see below), so there was nowhere on the shell to put
//! the legend — which is why #957 grew `tooltip` on the node vocabulary
//! ([`Node::Box`]/[`Label`](Node::Label)/[`Icon`](Node::Icon)) rather than giving
//! this one chip a drawer.
//!
//! # No panel, no capabilities
//!
//! Deliberately (#866 scope): the chip is a readout, so it requests no
//! [`Capability`](hytte_plugin::proto::Capability) at all and defines no drawer
//! panel. A settings/detail surface, if it is ever wanted, is a later pass — and
//! the chip cannot regress into one silently, because an effect whose capability
//! the manifest doesn't list is dropped by the host before it is brokered.

use std::time::Duration;

use hytte_plugin::proto::{Dir, Effect, Manifest, Mount, Node};
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View, tick_stream};

use crate::Mode;
use crate::status::{self, Last, Status};

/// Stable plugin id — the host's mount-slot key, and the `<id>` in the
/// `trollshell-plugin-<id>.service` transient unit the launcher spawns.
const PLUGIN_ID: &str = "claude-bridge";

/// The chip's root node id (the only id it assigns: nothing here is clickable).
const ROOT_ID: &str = "claude-bridge-root";

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

/// How often the chip re-reads [`crate::status`].
///
/// A status readout, not a clock: 5 s is fast enough that a run of failures
/// shows up while somebody is still looking at it, and slow enough to cost
/// nothing. The SDK dedups identical trees, so a tick that changes nothing sends
/// no frame at all.
const POLL: Duration = Duration::from_secs(5);

/// The chip's only message: re-read the status board.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tick;

/// The chip's whole model — the last board reading. Everything else lives in
/// [`crate::status`]'s atomics, written by the HTTP half.
struct BridgeChip {
    status: Status,
}

impl Plugin for BridgeChip {
    type Msg = Tick;
    /// Purely a readout: it issues no I/O of its own, so it has no commands.
    type Cmd = std::convert::Infallible;

    /// Mounts [`Mount::BarRight`] as a chip. Subscribes to nothing (the chip is
    /// driven by its own tick off local atomics, not by host state) and requests
    /// **no capabilities** — see the module docs.
    fn manifest() -> Manifest {
        Manifest::new(PLUGIN_ID, Mount::BarRight)
    }

    fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
        Self {
            status: status::snapshot(),
        }
    }

    /// The chip's own cadence. Note this is the *chip's* source, not the
    /// bridge's: the HTTP listener is spawned by `main` on a different runtime
    /// and outlives every plugin session (module docs).
    fn sources(_cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        Some(Box::pin(tick_stream(POLL, Tick)))
    }

    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        if let Input::App(Tick) = input {
            self.status = status::snapshot();
        }
        Vec::new()
    }

    fn view(&self) -> View {
        chip(&self.status).into()
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

/// The whole chip in one sentence, for the hover (#957).
///
/// The chip is a handful of 16px glyphs and, being deliberately panel-less
/// (#866), has nowhere else to explain them — `sub 18/0` was legible only to
/// someone who had read this module. So the tooltip spells out every part the
/// chip abbreviates: the backend by name, and the counts as words.
///
/// The counts are the **raw** numbers, not [`counts_label`]'s `999+`-saturated
/// ones: the cap exists to stop the chip widening on the bar, and a tooltip has
/// no width to defend.
fn tooltip(status: &Status) -> String {
    let Some(startup) = status.startup else {
        // Same honesty as the `…` label: no mode has been settled yet, so name
        // none.
        return "Claude bridge · starting up".to_owned();
    };
    let traffic = if status.ok == 0 && status.errors == 0 {
        "nothing served yet".to_owned()
    } else {
        format!("{} served, {} failed", status.ok, status.errors)
    };
    format!("Claude bridge · {} · {traffic}", mode_name(startup.mode))
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

/// Project the status board onto the bar chip. The host wraps this in its own
/// `.ts-plugin-chip` pill, so the root carries no chip/card class of its own.
///
/// Before `main` has published the startup facts the chip renders a single muted
/// ellipsis: the daemon is up but has not settled its backend yet, and inventing
/// a mode for that window would be a lie the chip is specifically there to
/// prevent.
fn chip(status: &Status) -> Node {
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
    Node::Box {
        id: Some(ROOT_ID.to_owned()),
        dir: Dir::Horizontal,
        spacing: 4,
        scroll: false,
        classes: Vec::new(),
        children,
        // On the root, so hovering anywhere on the pill answers the question —
        // the glyphs are 16px wide and nobody should have to find the right one.
        tooltip: Some(tooltip(status)),
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
        BridgeChip, CLAUDE_ICON, Tick, capped, chip, counts_label, health_icon, mode_label,
        mode_name, tooltip,
    };
    use crate::Mode;
    use crate::status::{Last, Startup, Status};
    use hytte_plugin::proto::{Manifest, Mount, Node, PluginMsg, decode, encode};
    use hytte_plugin::{Input, Plugin};

    fn status(mode: Mode, keyed: bool, ok: u64, errors: u64, last: Last) -> Status {
        Status {
            startup: Some(Startup { mode, keyed }),
            ok,
            errors,
            last,
        }
    }

    /// Collect every `Label`/`Icon` payload in a tree, in render order — the
    /// chip is small enough that its full text is the assertion.
    fn texts(node: &Node) -> Vec<String> {
        match node {
            Node::Label { text, .. } => vec![text.clone()],
            Node::Icon { name, .. } => vec![name.clone()],
            Node::Box { children, .. } | Node::Row { children, .. } => {
                children.iter().flat_map(texts).collect()
            }
            _ => Vec::new(),
        }
    }

    /// The hover text on the tree's root box — the chip's one tooltip (#957).
    fn root_tooltip(node: &Node) -> Option<&str> {
        match node {
            Node::Box { tooltip, .. } => tooltip.as_deref(),
            _ => None,
        }
    }

    /// The manifest is the whole of this plugin's host contract: a bar chip that
    /// asks for **nothing**. A capability creeping in here is a real change (the
    /// host cap-checks effects), so pin the empty lists.
    #[test]
    fn the_manifest_is_a_bar_chip_that_asks_for_nothing() {
        let m: Manifest = BridgeChip::manifest();
        assert_eq!(m.id, "claude-bridge");
        assert_eq!(m.mount, Mount::BarRight);
        assert!(m.mount.is_bar(), "it is a chip, not a sidebar card");
        assert!(
            m.capabilities.is_empty(),
            "the chip is a readout; it brokers no effects"
        );
        assert!(
            m.subscribes.is_empty(),
            "it ticks off its own status board, not host state"
        );
        assert!(m.provides.is_empty());
    }

    /// The chip is panel-less: #866 scope is a readout, not a settings surface.
    #[test]
    fn the_view_is_chip_only() {
        let mut model = BridgeChip {
            status: status(Mode::Subscription, false, 0, 0, Last::None),
        };
        assert!(model.view().panel.is_none());
        // …and a tick never asks the shell for anything.
        assert!(model.update(Input::App(Tick)).is_empty());
    }

    /// A subscription-mode bridge that has served nothing: loading glyph, `sub`,
    /// no key glyph (nothing is held — `claude` owns the session), no counts.
    #[test]
    fn a_fresh_subscription_bridge_shows_no_key_and_no_counts() {
        let tree = chip(&status(Mode::Subscription, false, 0, 0, Last::None));
        assert_eq!(
            texts(&tree),
            vec![CLAUDE_ICON, "content-loading-symbolic", "sub"]
        );
    }

    /// An `api`-mode bridge holding a key: the key glyph is present, and the
    /// counts appear once something has been served.
    #[test]
    fn a_keyed_api_bridge_shows_the_key_glyph_and_its_counts() {
        let tree = chip(&status(Mode::Api, true, 12, 1, Last::Ok));
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
            let tree = chip(&status(mode, false, 3, 0, Last::Ok));
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
        let tree = chip(&Status {
            startup: None,
            ok: 0,
            errors: 0,
            last: Last::None,
        });
        assert_eq!(
            texts(&tree),
            vec![CLAUDE_ICON, "content-loading-symbolic", "…"]
        );
    }

    /// The Claude glyph leads in **every** state — including the two the chip
    /// treats specially (nothing published yet; nothing served yet). Annika's
    /// ask on #957 was that the pill be identifiable at a glance, which it isn't
    /// if the identity only shows up once the daemon has settled.
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
        for board in &boards {
            let tree = chip(board);
            assert_eq!(
                texts(&tree).first().map(String::as_str),
                Some(CLAUDE_ICON),
                "{board:?} must still lead with the Claude glyph"
            );
        }
    }

    /// The #957 fix itself: hovering the pill spells out everything it
    /// abbreviates. Mode by name, counts as words, on the **root** box so any
    /// pixel of the chip answers.
    #[test]
    fn the_root_box_carries_the_spelled_out_tooltip() {
        let tree = chip(&status(Mode::Subscription, false, 18, 0, Last::Ok));
        assert_eq!(
            root_tooltip(&tree),
            Some("Claude bridge · subscription · 18 served, 0 failed"),
            "this is literally Mara's `sub 18/0`, in words"
        );
    }

    /// Before anything is served the tooltip says so, rather than printing the
    /// `0 served, 0 failed` the chip itself deliberately refuses to render.
    #[test]
    fn the_tooltip_says_nothing_served_yet_before_the_first_request() {
        assert_eq!(
            tooltip(&status(Mode::Api, true, 0, 0, Last::None)),
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
        assert_eq!(tooltip(&board), "Claude bridge · starting up");
        assert_eq!(root_tooltip(&chip(&board)).unwrap(), tooltip(&board));
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
            let hover = tooltip(&board);
            assert!(hover.contains(name), "{hover:?} must name {name}");
            // …and the chip is still printing the short form of that same mode.
            assert_eq!(texts(&chip(&board))[2], mode_label(mode));
        }
    }

    /// The chip saturates at `999+` to keep its width; the tooltip has no width
    /// to defend, so it reports the real numbers.
    #[test]
    fn the_tooltip_reports_the_uncapped_counts() {
        let hover = tooltip(&status(Mode::Reprompt, false, 86_400, 12_345, Last::Error));
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
    /// manifest and a `Render` of its chip both round-trip through the codec.
    #[test]
    fn register_and_render_frames_round_trip() {
        let reg = PluginMsg::Register {
            manifest: BridgeChip::manifest(),
        };
        let back: PluginMsg = decode(&encode(&reg)).expect("register frame decodes");
        assert_eq!(reg, back);

        let view = BridgeChip {
            status: status(Mode::Api, true, 1, 0, Last::Ok),
        }
        .view();
        let render = PluginMsg::Render {
            tree: view.tree,
            panel: view.panel,
            hidden_on: view.hidden_on,
            effects: vec![],
        };
        let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
        assert_eq!(render, back);
    }
}
