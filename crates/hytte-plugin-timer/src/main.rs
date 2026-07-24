//! `hytte-plugin-timer` — a pomodoro / kitchen timer for the trollshell bar 🍅
//! (issue #406), and the first customer of the [`Effect::Notify`] toast effect.
//!
//! An out-of-process widget plugin on the `hytte-plugin` SDK: pure TEA — the
//! model below, a 1 Hz tick, an [`update`](Plugin::update) reducer, and a
//! `view`/`panel` pair. All transport (dial/backoff, the `Register` handshake,
//! liveness, render dedup, reconnection) lives in the [`hytte_plugin`] runtime
//! behind the one-line `main`; systemd's `Restart=on-failure` is the outer
//! supervisor.
//!
//! # The face — `preem::seven_seg` (#356/#376)
//!
//! The countdown is a seven-segment `MM:SS` readout, rendered into a
//! [`Node::Pixels`] the host paints crisp (nearest-neighbor) and aspect-locked.
//! The SDK auto-subscribes the desktop accent (#376), so the lit digits tint to
//! the shell accent out of the box. The bar **chip** wraps that readout in a
//! clickable button; clicking it opens the plugin's own drawer **panel** (#349
//! PR2) which holds the entry, the presets, and pause/reset.
//!
//! # The clock lives in the plugin — the host stays stateless
//!
//! State lives here (#195): the running countdown is `remaining` seconds,
//! decremented by the plugin's own 1 Hz [`sources`](Plugin::sources) tick — so a
//! running timer keeps counting whether or not its chip is on screen, and a host
//! restart simply re-seeds a fresh idle timer (nothing to persist). The reducer
//! is pure and panic-free, which is what makes it the plugin's whole correctness
//! signal (see the `tests` module) since the live host isn't reachable here.
//!
//! # Input — `Node::Entry` + `Submitted` (#357)
//!
//! Type a duration into the panel entry and press Enter: `25` / `25m` (25
//! minutes), `5:00` (5 minutes), `90s`, `1:30:00`, or `pomo` — all parsed in
//! [`parse_duration`], pure TEA, no wall clock.
//!
//! # At zero — `Effect::Notify` (#406)
//!
//! When the countdown reaches `00:00` the reducer emits exactly one
//! [`Effect::Notify`], which the host posts as a toast through its own
//! notification daemon — the payoff of the additive effect this crate ships
//! alongside.

use std::time::Duration;

use hytte_plugin::preem::{DisplayStyle, seven_seg};
use hytte_plugin::proto::{Capability, Dir, Effect, EventKind, Manifest, Mount, Node, Page};
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View};
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::IntervalStream;

/// Stable plugin id — the host's mount-region ownership key, the audit-log
/// subject, and the notification app name.
const PLUGIN_ID: &str = "timer";

// Node ids. The chip button opens the panel; the `Pixels`/`Entry`/`Button` ids
// make each re-render swap props in place instead of rebuilding the widget.
const ROOT_ID: &str = "timer-root";
const CHIP_BTN: &str = "timer-chip";
const SEG_ID: &str = "timer-seg";
const PANEL_ROOT_ID: &str = "timer-panel";
const PANEL_SEG_ID: &str = "timer-panel-seg";
const ENTRY_ID: &str = "timer-entry";
const PRESET_25: &str = "timer-preset-25";
const PRESET_5: &str = "timer-preset-5";
const PRESET_15: &str = "timer-preset-15";
const PAUSE_ID: &str = "timer-pause";
const RESET_ID: &str = "timer-reset";

/// The countdown cadence — one decrement per second.
const TICK: Duration = Duration::from_secs(1);
/// The pomodoro preset (and the `pomo` keyword), in seconds.
const POMODORO_SECS: u32 = 25 * 60;
/// The short-break preset, in seconds.
const SHORT_BREAK_SECS: u32 = 5 * 60;
/// The long-break preset, in seconds.
const LONG_BREAK_SECS: u32 = 15 * 60;
/// Clamp for a parsed/preset duration, so a fat-fingered entry can't overflow
/// the readout or the tick math. 24 h is far past any real kitchen timer.
const MAX_SECS: u32 = 24 * 60 * 60;
/// The 7seg skin. Its lit ink is accent-tinted by the SDK (#376); the near-black
/// VFD field reads well as a small bar chip.
const STYLE: DisplayStyle = DisplayStyle::Vfd;

/// The timer's own message: a single 1 Hz heartbeat from [`Plugin::sources`].
#[derive(Debug)]
enum TimerMsg {
    /// One second elapsed — advance a running countdown.
    Tick,
}

/// The timer's whole state — rebuilt on every (re)connect (the host stores
/// nothing). `remaining` is the live countdown; `total` is the last-set duration
/// so **Reset** can restore it.
#[derive(Debug, PartialEq, Eq)]
struct Timer {
    /// Seconds left on the countdown (the "deadline", relative — decremented by
    /// the 1 Hz tick so the reducer stays pure).
    remaining: u32,
    /// The duration last set (entry or preset), restored by [`Timer::reset`].
    total: u32,
    /// Whether the countdown is advancing.
    running: bool,
}

/// Format a whole-second count as a `MM:SS` (or wider) readout for the 7seg.
fn fmt_mmss(secs: u32) -> String {
    let mins = secs / 60;
    let rem = secs % 60;
    format!("{mins:02}:{rem:02}")
}

/// Parse a user-typed duration into seconds, or `None` for nonsense. Pure and
/// panic-free over any input:
///
/// - `pomo` / `pomodoro` → 25 minutes,
/// - `M:SS` or `H:MM:SS` → clock form (`5:00` → 300, `1:30:00` → 5400),
/// - `<n>h` / `<n>m` / `<n>s` → hours / minutes / seconds,
/// - a bare number → **minutes** (kitchen-timer convention: `25` → 25 min).
///
/// The result is clamped to [`MAX_SECS`]; a component out of range (`5:99`) or a
/// non-numeric body is rejected.
fn parse_duration(input: &str) -> Option<u32> {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    if s == "pomo" || s == "pomodoro" {
        return Some(POMODORO_SECS);
    }
    let secs = if s.contains(':') {
        parse_clock(&s)?
    } else if let Some(n) = s.strip_suffix('h') {
        n.trim().parse::<u32>().ok()?.checked_mul(3600)?
    } else if let Some(n) = s.strip_suffix('m') {
        n.trim().parse::<u32>().ok()?.checked_mul(60)?
    } else if let Some(n) = s.strip_suffix('s') {
        n.trim().parse::<u32>().ok()?
    } else {
        // Bare number: minutes.
        s.parse::<u32>().ok()?.checked_mul(60)?
    };
    Some(secs.min(MAX_SECS))
}

/// Parse the colon clock forms `M:SS` and `H:MM:SS`. Minutes/seconds components
/// must be `< 60`; the leading field is unbounded (clamped by the caller).
fn parse_clock(s: &str) -> Option<u32> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [m, sec] => {
            let m: u32 = m.parse().ok()?;
            let sec: u32 = sec.parse().ok()?;
            if sec >= 60 {
                return None;
            }
            m.checked_mul(60)?.checked_add(sec)
        }
        [h, m, sec] => {
            let h: u32 = h.parse().ok()?;
            let m: u32 = m.parse().ok()?;
            let sec: u32 = sec.parse().ok()?;
            if m >= 60 || sec >= 60 {
                return None;
            }
            h.checked_mul(3600)?
                .checked_add(m.checked_mul(60)?)?
                .checked_add(sec)
        }
        _ => None,
    }
}

impl Timer {
    /// The countdown as a `MM:SS` string for the 7seg.
    fn mmss(&self) -> String {
        fmt_mmss(self.remaining)
    }

    /// Load and start a duration (clamped): what a preset click or a parsed
    /// entry does. A zero duration loads but doesn't run.
    fn start(&mut self, secs: u32) {
        let secs = secs.min(MAX_SECS);
        self.total = secs;
        self.remaining = secs;
        self.running = secs > 0;
    }

    /// Pause a running countdown, or resume a paused one — a no-op once it has
    /// hit zero (nothing left to run).
    fn toggle_pause(&mut self) {
        if self.remaining > 0 {
            self.running = !self.running;
        }
    }

    /// Stop and restore the last-set duration.
    fn reset(&mut self) {
        self.remaining = self.total;
        self.running = false;
    }

    /// The 1 Hz heartbeat: advance a running countdown by one second and, on the
    /// tick that reaches zero, stop and emit exactly one [`Effect::Notify`]. An
    /// idle (or already-finished) timer ticks to nothing.
    fn tick(&mut self) -> Vec<Effect> {
        if !self.running || self.remaining == 0 {
            return Vec::new();
        }
        self.remaining -= 1;
        if self.remaining == 0 {
            self.running = false;
            return vec![Effect::Notify {
                summary: "Timer done".to_owned(),
                body: format!("{} timer finished", fmt_mmss(self.total)),
            }];
        }
        Vec::new()
    }

    /// Fold one user interaction. The chip click asks the host to open this
    /// plugin's panel; the panel controls mutate the model.
    fn on_event(&mut self, node: &str, kind: &EventKind) -> Vec<Effect> {
        match (kind, node) {
            (EventKind::Click, CHIP_BTN) => return vec![Effect::OpenPage(Page::PluginSelf)],
            (EventKind::Click, PRESET_25) => self.start(POMODORO_SECS),
            (EventKind::Click, PRESET_5) => self.start(SHORT_BREAK_SECS),
            (EventKind::Click, PRESET_15) => self.start(LONG_BREAK_SECS),
            (EventKind::Click, PAUSE_ID) => self.toggle_pause(),
            (EventKind::Click, RESET_ID) => self.reset(),
            (EventKind::Submitted { text }, ENTRY_ID) => {
                if let Some(secs) = parse_duration(text) {
                    self.start(secs);
                }
            }
            _ => {}
        }
        Vec::new()
    }
}

/// One panel button: an id'd [`Node::Button`] wrapping a plain text label.
fn button(id: &str, label: &str) -> Node {
    Node::Button {
        id: id.to_owned(),
        classes: Vec::new(),
        child: Box::new(Node::Label {
            id: None,
            text: label.to_owned(),
            classes: Vec::new(),
        }),
    }
}

impl Plugin for Timer {
    type Msg = TimerMsg;
    /// Purely local: the timer issues no I/O of its own, so it has no commands
    /// and ignores the command lane entirely.
    type Cmd = std::convert::Infallible;

    /// Mounts [`Mount::BarRight`] as a chip. Requests [`Capability::OpenPage`]
    /// (open its own panel, #349 PR2) and [`Capability::Notify`] (the toast at
    /// zero, #406). It subscribes no host state — its countdown is self-driven;
    /// the SDK adds the accent subscription (#376) on its behalf.
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, Mount::BarRight);
        m.capabilities = vec![Capability::OpenPage, Capability::Notify];
        m
    }

    /// A fresh idle pomodoro (25:00, not running) — the seed render mounts the
    /// chip immediately. The command sender goes unused (`Cmd = Infallible`).
    fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
        Self {
            remaining: POMODORO_SECS,
            total: POMODORO_SECS,
            running: false,
        }
    }

    /// A 1 Hz tick, created per session and dropped on disconnect. The command
    /// receiver goes unused. `tokio::time::interval` fires immediately then every
    /// second — the leading tick is a harmless no-op on an idle timer.
    fn sources(_cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        let ticks = IntervalStream::new(tokio::time::interval(TICK)).map(|_| TimerMsg::Tick);
        Some(Box::pin(ticks))
    }

    /// Fold one input. Pure and panic-free over any host-sent value; re-rendering
    /// is the runtime's problem (identical trees are deduped, effects force a
    /// send).
    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            Input::App(TimerMsg::Tick) => self.tick(),
            Input::Event { node, kind } => self.on_event(&node, &kind),
            // No host state is subscribed and no command is issued, so the
            // snapshot / effect-result / visibility pushes are all no-ops. The
            // countdown keeps ticking whether or not the chip is on screen.
            Input::Snapshot(_)
            | Input::EffectResult { .. }
            | Input::SlotVisible(_)
            | Input::AudioSpectrum(_)
            | Input::ConsentDecision { .. }
            | Input::CalendarUpcoming(_)
            | Input::SessionLocked(_)
            | Input::NowPlaying(_) => Vec::new(),
        }
    }

    /// The rendered [`View`] (#349). The bar **chip**: a clickable button
    /// wrapping the `MM:SS` 7seg readout — the host wraps it in its own
    /// `.ts-plugin-chip` pill and paints the `Pixels` aspect-locked, so the
    /// readout fits the bar without a CSS px rule; a click opens the panel.
    /// The drawer **panel**: the big readout, the duration entry, the preset
    /// row (25 / 5 / 15), and the pause/reset row. Its root carries no
    /// `.card`/`.ts-plugin-*` class — the drawer supplies the chrome.
    fn view(&self) -> View {
        let chip = Node::Box {
            id: Some(ROOT_ID.to_owned()),
            dir: Dir::Horizontal,
            spacing: 0,
            scroll: false,
            classes: Vec::new(),
            children: vec![Node::Button {
                id: CHIP_BTN.to_owned(),
                classes: vec!["flat".to_owned()],
                child: Box::new(seven_seg(&self.mmss(), STYLE).into_node(Some(SEG_ID), Vec::new())),
            }],
        };
        let readout = seven_seg(&self.mmss(), STYLE).into_node(Some(PANEL_SEG_ID), Vec::new());
        let entry = Node::Entry {
            id: ENTRY_ID.to_owned(),
            text: String::new(),
            placeholder: "25m · 5:00 · pomo".to_owned(),
            classes: vec!["monospace".to_owned()],
        };
        let presets = Node::Row {
            id: Some("timer-presets".to_owned()),
            classes: Vec::new(),
            children: vec![
                button(PRESET_25, "25"),
                button(PRESET_5, "5"),
                button(PRESET_15, "15"),
            ],
        };
        let controls = Node::Row {
            id: Some("timer-controls".to_owned()),
            classes: Vec::new(),
            children: vec![
                button(PAUSE_ID, if self.running { "Pause" } else { "Start" }),
                button(RESET_ID, "Reset"),
            ],
        };
        View::new(chip).panel(Node::Box {
            id: Some(PANEL_ROOT_ID.to_owned()),
            dir: Dir::Vertical,
            spacing: 8,
            scroll: false,
            classes: Vec::new(),
            children: vec![readout, entry, presets, controls],
        })
    }
}

fn main() {
    hytte_plugin::run::<Timer>();
}

#[cfg(test)]
mod tests {
    use super::{
        CHIP_BTN, ENTRY_ID, LONG_BREAK_SECS, PLUGIN_ID, POMODORO_SECS, PRESET_5, PRESET_25,
        SHORT_BREAK_SECS, Timer, parse_duration,
    };
    use hytte_plugin::proto::{
        Capability, Effect, EventKind, Manifest, Node, Page, PluginMsg, decode, encode,
    };
    use hytte_plugin::{Input, Plugin};

    /// A fresh model with a throwaway command sender — the timer issues no
    /// commands, so the lane goes unused.
    fn fresh() -> Timer {
        Timer::init(hytte_plugin::cmd_channel().0)
    }

    fn click(node: &str) -> Input<super::TimerMsg> {
        Input::Event {
            node: node.to_owned(),
            kind: EventKind::Click,
        }
    }

    fn submit(node: &str, text: &str) -> Input<super::TimerMsg> {
        Input::Event {
            node: node.to_owned(),
            kind: EventKind::Submitted {
                text: text.to_owned(),
            },
        }
    }

    fn tick(model: &mut Timer) -> Vec<Effect> {
        model.update(Input::App(super::TimerMsg::Tick))
    }

    /// The parse table — the reducer's testable heart (#357). `25m`→1500,
    /// `5:00`→300, and the whole vocabulary.
    #[test]
    fn parse_duration_covers_the_vocabulary() {
        assert_eq!(parse_duration("25m"), Some(1500));
        assert_eq!(parse_duration("5:00"), Some(300));
        assert_eq!(parse_duration("25"), Some(1500), "bare number is minutes");
        assert_eq!(parse_duration("pomo"), Some(POMODORO_SECS));
        assert_eq!(parse_duration("pomodoro"), Some(POMODORO_SECS));
        assert_eq!(parse_duration("90s"), Some(90));
        assert_eq!(parse_duration("1h"), Some(3600));
        assert_eq!(parse_duration("1:30:00"), Some(5400));
        assert_eq!(parse_duration("  10m  "), Some(600), "surrounding space ok");
        assert_eq!(
            parse_duration("POMO"),
            Some(POMODORO_SECS),
            "case-insensitive"
        );
    }

    /// Garbage never panics and never yields a duration.
    #[test]
    fn parse_duration_rejects_nonsense() {
        for bad in [
            "", "   ", "abc", "5:99", "1:60:00", "m", ":", "5:", "-3", "5m5",
        ] {
            assert_eq!(parse_duration(bad), None, "{bad:?} must not parse");
        }
    }

    /// A tick decrements a running countdown; hitting zero stops it and yields a
    /// single `Notify` effect (#406, the payoff).
    #[test]
    fn a_tick_decrements_and_zero_notifies() {
        let mut m = fresh();
        m.start(3);
        assert_eq!((m.remaining, m.running), (3, true));

        assert!(tick(&mut m).is_empty());
        assert_eq!(m.remaining, 2);
        assert!(tick(&mut m).is_empty());
        assert_eq!(m.remaining, 1);

        // The tick that reaches zero stops the timer and emits exactly one Notify.
        let fx = tick(&mut m);
        assert_eq!(m.remaining, 0);
        assert!(!m.running, "a finished timer stops");
        assert_eq!(
            fx,
            vec![Effect::Notify {
                summary: "Timer done".to_owned(),
                body: "00:03 timer finished".to_owned(),
            }]
        );

        // Ticking a finished timer does nothing more (no repeat Notify).
        assert!(tick(&mut m).is_empty());
        assert_eq!(m.remaining, 0);
    }

    /// An idle (paused / not-yet-started) timer ignores ticks.
    #[test]
    fn an_idle_timer_ignores_ticks() {
        let mut m = fresh();
        assert!(!m.running);
        let before = m.remaining;
        assert!(tick(&mut m).is_empty());
        assert_eq!(m.remaining, before, "an idle timer holds its readout");
    }

    /// Parsing `25m` into seconds runs a 1500 s countdown, and the entry text is
    /// never echoed back (so it clears after submit).
    #[test]
    fn submitting_an_entry_starts_the_countdown() {
        let mut m = fresh();
        let fx = m.update(submit(ENTRY_ID, "5:00"));
        assert!(fx.is_empty());
        assert_eq!((m.remaining, m.total, m.running), (300, 300, true));

        // The rendered entry always shows "" — it clears after a submit and
        // never fights in-progress typing (the echo-prop contract).
        let Some(Node::Box { children, .. }) = m.view().panel else {
            panic!("panel is a Box");
        };
        let entry = children
            .iter()
            .find(|n| matches!(n, Node::Entry { .. }))
            .expect("panel has an entry");
        let Node::Entry { text, .. } = entry else {
            unreachable!()
        };
        assert_eq!(text, "", "the entry clears after submit");
    }

    /// A submit that doesn't parse leaves the timer untouched.
    #[test]
    fn a_bad_entry_is_ignored() {
        let mut m = fresh();
        let before = (m.remaining, m.total, m.running);
        let fx = m.update(submit(ENTRY_ID, "nope"));
        assert!(fx.is_empty());
        assert_eq!((m.remaining, m.total, m.running), before);
    }

    /// The presets load and start their durations.
    #[test]
    fn presets_start_their_durations() {
        let mut m = fresh();
        let _ = m.update(click(PRESET_5));
        assert_eq!((m.remaining, m.running), (SHORT_BREAK_SECS, true));
        let _ = m.update(click(PRESET_25));
        assert_eq!((m.remaining, m.running), (POMODORO_SECS, true));
        let _ = m.update(click(super::PRESET_15));
        assert_eq!((m.remaining, m.running), (LONG_BREAK_SECS, true));
    }

    /// Pause toggles a running countdown; reset stops and restores the total.
    #[test]
    fn pause_toggles_and_reset_restores() {
        let mut m = fresh();
        m.start(600);
        let _ = m.update(click(super::PAUSE_ID));
        assert!(!m.running, "pause halts");
        let _ = tick(&mut m);
        assert_eq!(m.remaining, 600, "paused timers don't advance");
        let _ = m.update(click(super::PAUSE_ID));
        assert!(m.running, "a second toggle resumes");

        let _ = tick(&mut m);
        assert_eq!(m.remaining, 599);
        let _ = m.update(click(super::RESET_ID));
        assert_eq!(
            (m.remaining, m.running),
            (600, false),
            "reset restores total"
        );
    }

    /// Clicking the chip asks the host to open this plugin's own panel.
    #[test]
    fn clicking_the_chip_opens_the_panel() {
        let mut m = fresh();
        let fx = m.update(click(CHIP_BTN));
        assert_eq!(fx, vec![Effect::OpenPage(Page::PluginSelf)]);
    }

    /// A click on a node we don't own changes nothing.
    #[test]
    fn foreign_clicks_are_ignored() {
        let mut m = fresh();
        let before = m.view();
        let fx = m.update(click("not-ours"));
        assert!(fx.is_empty());
        assert_eq!(m.view(), before);
    }

    /// The chip is a clickable 7seg `Pixels` strip whose buffer honors the host's
    /// `len == w*h*4` seam.
    #[test]
    fn the_chip_is_a_pokeable_seven_seg() {
        let m = fresh();
        let Node::Box { children, .. } = m.view().tree else {
            panic!("root is a box");
        };
        let Node::Button { id, child, .. } = &children[0] else {
            panic!("chip is a button");
        };
        assert_eq!(id, CHIP_BTN);
        let Node::Pixels {
            width,
            height,
            data,
            ..
        } = &**child
        else {
            panic!("the chip child is a Pixels 7seg");
        };
        assert_eq!(
            data.len(),
            *width as usize * *height as usize * 4,
            "buffer must satisfy the host's len == w*h*4 seam"
        );
    }

    /// The manifest requests exactly the caps the panel + toast need.
    #[test]
    fn manifest_requests_the_right_caps() {
        let m = Manifest::new(PLUGIN_ID, super::Mount::BarRight);
        let caps = Timer::manifest().capabilities;
        assert!(caps.contains(&Capability::OpenPage), "opens its own panel");
        assert!(caps.contains(&Capability::Notify), "alerts at zero");
        // (Sanity: `Manifest::new` alone requests none.)
        assert!(m.capabilities.is_empty());
    }

    /// The frames built from this plugin's data are valid on the wire — including
    /// a `Notify`-bearing render frame (Part 1's additive effect end-to-end).
    #[test]
    fn register_and_render_frames_round_trip() {
        let reg = PluginMsg::Register {
            manifest: Timer::manifest(),
        };
        let back: PluginMsg = decode(&encode(&reg)).expect("register frame decodes");
        assert_eq!(reg, back);

        let mut m = fresh();
        m.start(1);
        let effects = tick(&mut m);
        assert!(matches!(effects.as_slice(), [Effect::Notify { .. }]));
        let view = m.view();
        let render = PluginMsg::Render {
            tree: view.tree,
            panel: view.panel,
            effects,
        };
        let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
        assert_eq!(render, back);
    }
}
