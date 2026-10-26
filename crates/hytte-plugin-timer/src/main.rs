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
//! # The face — `display::SevenSeg` (#356/#376, on the #884 seam)
//!
//! The countdown is a seven-segment `MM:SS` readout. The bar **chip** wraps it
//! in a clickable button; clicking it opens the plugin's own drawer **panel**
//! (#349 PR2), which holds a second, identical readout above the entry, the
//! presets, and pause/reset. The SDK auto-subscribes the desktop accent (#376),
//! so the lit segments tint to the shell accent out of the box.
//!
//! Since #884 both readouts go through [`hytte_plugin::display::SevenSeg`]
//! instead of calling the raster kit by hand. One code path, two hosts: against
//! a shell that advertises the preem vocabulary in `HostMsg::Hello` the readout
//! ships as a typed `Node::Preem` the shell draws itself; against one that
//! doesn't it CPU-rasterises to exactly the [`Node::Pixels`] that
//! `preem::seven_seg(…).into_node(…)` produced before — byte for byte, which
//! the tests below pin.
//!
//! Nothing in `view` branches on which host is on the other end, and this
//! plugin has no `advance` to call at all: a seven-segment readout is pure, so
//! its whole state is the `MM:SS` string handed to
//! [`node`](hytte_plugin::display::SevenSeg::node). The countdown itself stays
//! the plugin's own — it is *data*, not animation, so it travels as the
//! widget's state on every change rather than as something the shell integrates.
//!
//! # The clock lives in the plugin — the host stays stateless
//!
//! State lives here (#195). A running countdown holds a **wall-clock deadline**
//! ([`SystemTime`]) and every tick derives `remaining` from `deadline - now`,
//! saturating at zero. The 1 Hz [`sources`](Plugin::sources) interval is only a
//! repaint cadence: it runs on `CLOCK_MONOTONIC`, which does not advance across
//! suspend, so counting its ticks made a 25-minute timer with 20 minutes of
//! suspend in the middle fire 20 minutes late. A paused timer stores
//! `remaining`, not a deadline, and resume sets a fresh deadline. If the wall
//! clock steps backwards (NTP) the deadline is rebased onto the new clock, so
//! the readout never grows. Time is injected (`*_at(now)` methods), which is
//! what lets the tests simulate suspend and clock steps without sleeping.
//!
//! The SDK builds the model fresh on every session and `systemctl --user
//! restart trollshell` reconnects the *same* plugin process, so `init` would
//! otherwise turn a running pomodoro into an idle 25:00. The model is therefore
//! mirrored into a process-level holder (`HELD`) after every update and
//! re-read by `init`. The SDK has no state seam and no other plugin carries
//! state across sessions; a process-level holder is enough because the process
//! outlives the host connection, and it is simpler than persisting to
//! `$XDG_STATE_HOME` (a plugin restart loses it, which is acceptable). A
//! deadline that passed during the gap fires its notification on the first
//! tick after reconnect. The reducer is pure and panic-free, which is what
//! makes it the plugin's whole correctness signal (see the `tests` module).
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

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use hytte_plugin::display::{SevenSeg, StyleName};
use hytte_plugin::proto::{Capability, Dir, Effect, EventKind, Manifest, Mount, Node, Page};
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View, tick_stream};

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
/// VFD field reads well as a small bar chip. A wire [`StyleName`] since #884 —
/// the skin travels to a preem-speaking shell as a *name*, never as resolved
/// colors, which is what lets the shell re-tint the readout live (#396).
const STYLE: StyleName = StyleName::Vfd;

/// The timer's own message: a single 1 Hz heartbeat from [`Plugin::sources`].
///
/// `Clone` is required by [`tick_stream`], which emits a clone each period.
#[derive(Clone, Debug)]
enum TimerMsg {
    /// One second elapsed — advance a running countdown.
    Tick,
}

/// The timer's whole state — rebuilt on every (re)connect from `HELD`. While
/// running, `deadline` is the truth and `remaining` its per-tick projection;
/// while paused or idle, `remaining` is the truth. `total` is the last-set
/// duration so **Reset** can restore it.
#[derive(Debug, PartialEq, Eq)]
struct Timer {
    /// Seconds left on the countdown, as last derived from `deadline`.
    remaining: u32,
    /// The duration last set (entry or preset), restored by [`Timer::reset`].
    total: u32,
    /// Whether the countdown is advancing.
    running: bool,
    /// Wall-clock instant the countdown reaches zero; `Some` only while
    /// `running`.
    deadline: Option<SystemTime>,
    /// The `now` of the last tick/start/resume, to notice a backwards clock
    /// step. `Some` only while `running`.
    last_seen: Option<SystemTime>,
    /// The `MM:SS` readout the chip and the panel both draw (#884). Config
    /// only — a seven-segment strip is pure, so this carries no animation state
    /// and needs no `advance`; the reading is handed to `node` at render time.
    /// One instance for both call sites: the two nodes differ in nothing but
    /// their id, which is what keys each to its own renderer host-side.
    seg: SevenSeg,
}

/// The persistent part of a [`Timer`] — what survives a reconnect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Saved {
    /// See [`Timer::remaining`].
    remaining: u32,
    /// See [`Timer::total`].
    total: u32,
    /// See [`Timer::running`].
    running: bool,
    /// See [`Timer::deadline`].
    deadline: Option<SystemTime>,
    /// See [`Timer::last_seen`].
    last_seen: Option<SystemTime>,
}

/// A process-level slot holding the timer across sessions.
type Holder = Mutex<Option<Saved>>;

/// The production holder: the plugin process outlives each host connection.
static HELD: Holder = Mutex::new(None);

/// Whole seconds from `now` until `deadline`, rounded up (so the readout shows
/// `00:01` until the deadline actually passes), saturating at zero.
fn secs_until(deadline: SystemTime, now: SystemTime) -> u32 {
    let Ok(left) = deadline.duration_since(now) else {
        return 0;
    };
    let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    u32::try_from(secs).unwrap_or(u32::MAX)
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

    /// A fresh idle pomodoro (25:00, not running).
    fn fresh() -> Self {
        Self {
            remaining: POMODORO_SECS,
            total: POMODORO_SECS,
            running: false,
            deadline: None,
            last_seen: None,
            seg: SevenSeg::new(STYLE),
        }
    }

    /// The model as `init` builds it: whatever `holder` kept from the previous
    /// session, else a fresh idle pomodoro.
    fn restore(holder: &Holder) -> Self {
        let saved = *holder.lock().unwrap_or_else(PoisonError::into_inner);
        match saved {
            Some(s) => Self {
                remaining: s.remaining,
                total: s.total,
                running: s.running,
                deadline: s.deadline,
                last_seen: s.last_seen,
                seg: SevenSeg::new(STYLE),
            },
            None => Self::fresh(),
        }
    }

    /// Mirror the persistent state into `holder`.
    fn remember(&self, holder: &Holder) {
        *holder.lock().unwrap_or_else(PoisonError::into_inner) = Some(Saved {
            remaining: self.remaining,
            total: self.total,
            running: self.running,
            deadline: self.deadline,
            last_seen: self.last_seen,
        });
    }

    /// Load and start a duration (clamped) at `now`: what a preset click or a
    /// parsed entry does. A zero duration loads but doesn't run.
    fn start_at(&mut self, secs: u32, now: SystemTime) {
        let secs = secs.min(MAX_SECS);
        self.total = secs;
        self.remaining = secs;
        self.run_from(now);
    }

    /// Arm the deadline `remaining` seconds after `now` if there is anything
    /// left to run, else stop.
    fn run_from(&mut self, now: SystemTime) {
        if self.remaining > 0 {
            self.running = true;
            self.deadline = Some(now + Duration::from_secs(u64::from(self.remaining)));
            self.last_seen = Some(now);
        } else {
            self.stop();
        }
    }

    /// Drop the running state (deadline and clock bookkeeping with it).
    fn stop(&mut self) {
        self.running = false;
        self.deadline = None;
        self.last_seen = None;
    }

    /// Pause a running countdown (freezing `remaining` as of `now`), or resume
    /// a paused one with a fresh deadline — a no-op once it has hit zero.
    fn toggle_pause_at(&mut self, now: SystemTime) {
        if self.running {
            self.sync(now);
            self.stop();
        } else if self.remaining > 0 {
            self.run_from(now);
        }
    }

    /// Stop and restore the last-set duration.
    fn reset(&mut self) {
        self.remaining = self.total;
        self.stop();
    }

    /// Re-derive `remaining` from the deadline at `now`. A wall clock that
    /// stepped backwards since the last look rebases the deadline onto the new
    /// clock instead (losing at most one tick), so `remaining` never grows.
    fn sync(&mut self, now: SystemTime) {
        let (Some(deadline), Some(last)) = (self.deadline, self.last_seen) else {
            return;
        };
        if now < last {
            self.deadline = Some(now + Duration::from_secs(u64::from(self.remaining)));
        } else {
            self.remaining = secs_until(deadline, now).min(self.remaining);
        }
        self.last_seen = Some(now);
    }

    /// The heartbeat: re-derive a running countdown from the wall clock and, on
    /// the tick that finds the deadline reached, stop and emit exactly one
    /// [`Effect::Notify`]. An idle (or already-finished) timer ticks to
    /// nothing.
    fn tick_at(&mut self, now: SystemTime) -> Vec<Effect> {
        if !self.running {
            return Vec::new();
        }
        self.sync(now);
        if self.remaining == 0 {
            self.stop();
            return vec![Effect::Notify {
                summary: "Timer done".to_owned(),
                body: format!("{} timer finished", fmt_mmss(self.total)),
            }];
        }
        Vec::new()
    }

    /// Fold one user interaction. The chip click asks the host to open this
    /// plugin's panel; the panel controls mutate the model.
    fn on_event(&mut self, node: &str, kind: &EventKind, now: SystemTime) -> Vec<Effect> {
        match (kind, node) {
            (EventKind::Click, CHIP_BTN) => return vec![Effect::OpenPage(Page::PluginSelf)],
            (EventKind::Click, PRESET_25) => self.start_at(POMODORO_SECS, now),
            (EventKind::Click, PRESET_5) => self.start_at(SHORT_BREAK_SECS, now),
            (EventKind::Click, PRESET_15) => self.start_at(LONG_BREAK_SECS, now),
            (EventKind::Click, PAUSE_ID) => self.toggle_pause_at(now),
            (EventKind::Click, RESET_ID) => self.reset(),
            (EventKind::Submitted { text }, ENTRY_ID) => {
                if let Some(secs) = parse_duration(text) {
                    self.start_at(secs, now);
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
            tooltip: None,
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
        let mut m =
            Manifest::new(PLUGIN_ID, Mount::BarRight).with_version(env!("CARGO_PKG_VERSION"));
        m.capabilities = vec![Capability::OpenPage, Capability::Notify];
        m
    }

    /// The model the previous session left in `HELD` (so a shell restart
    /// keeps a running countdown), else a fresh idle pomodoro (25:00, not
    /// running) — the seed render mounts the chip immediately. The command
    /// sender goes unused (`Cmd = Infallible`).
    fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
        Self::restore(&HELD)
    }

    /// A 1 Hz repaint cadence, created per session and dropped on disconnect;
    /// the countdown itself comes from the wall clock, not from counting these.
    /// The command receiver goes unused. [`tick_stream`] fires immediately then
    /// every second — the leading tick is a no-op on an idle timer and, after a
    /// reconnect, fires a deadline that passed meanwhile.
    fn sources(_cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        Some(Box::pin(tick_stream(TICK, TimerMsg::Tick)))
    }

    /// Fold one input. Pure and panic-free over any host-sent value; re-rendering
    /// is the runtime's problem (identical trees are deduped, effects force a
    /// send).
    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        let now = SystemTime::now();
        let effects = match input {
            Input::App(TimerMsg::Tick) => self.tick_at(now),
            Input::Event { node, kind, .. } => self.on_event(&node, &kind, now),
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
            | Input::NowPlaying(_)
            | Input::DatasourceQuery { .. }
            | Input::DatasourceResult { .. } => Vec::new(),
        };
        self.remember(&HELD);
        effects
    }

    /// The rendered [`View`] (#349). The bar **chip**: a clickable button
    /// wrapping the `MM:SS` 7seg readout — the host wraps it in its own
    /// `.ts-plugin-chip` pill and sizes the readout aspect-locked, so it fits
    /// the bar without a CSS px rule; a click opens the panel. The drawer
    /// **panel**: the big readout, the duration entry, the preset row
    /// (25 / 5 / 15), and the pause/reset row. Its root carries no
    /// `.card`/`.ts-plugin-*` class — the drawer supplies the chrome.
    ///
    /// The two [`SevenSeg::node`](hytte_plugin::display::SevenSeg::node) calls
    /// are the whole #884 seam: each lands as a typed `Node::Preem` or a
    /// rasterised `Node::Pixels` depending on what the host advertised, with no
    /// branch here. Neither carries a CSS class — a pixel readout brings its own
    /// font, and in state mode the shell resolves the skin from the style name.
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
                child: Box::new(self.seg.node(SEG_ID, &self.mmss())),
            }],
            tooltip: None,
        };
        let readout = self.seg.node(PANEL_SEG_ID, &self.mmss());
        let entry = Node::Entry {
            id: ENTRY_ID.to_owned(),
            text: String::new(),
            placeholder: "25m · 5:00 · pomo".to_owned(),
            classes: vec!["monospace".to_owned()],
        };
        let presets = Node::Row {
            spacing: 0,
            id: Some("timer-presets".to_owned()),
            classes: Vec::new(),
            children: vec![
                button(PRESET_25, "25"),
                button(PRESET_5, "5"),
                button(PRESET_15, "15"),
            ],
            tooltip: None,
        };
        let controls = Node::Row {
            spacing: 0,
            id: Some("timer-controls".to_owned()),
            classes: Vec::new(),
            children: vec![
                button(PAUSE_ID, if self.running { "Pause" } else { "Start" }),
                button(RESET_ID, "Reset"),
            ],
            tooltip: None,
        };
        View::new(chip).panel(Node::Box {
            id: Some(PANEL_ROOT_ID.to_owned()),
            dir: Dir::Vertical,
            spacing: 8,
            scroll: false,
            classes: Vec::new(),
            children: vec![readout, entry, presets, controls],
            tooltip: None,
        })
    }
}

fn main() {
    hytte_plugin::run::<Timer>();
}

#[cfg(test)]
mod tests {
    use super::{
        CHIP_BTN, ENTRY_ID, Holder, LONG_BREAK_SECS, PANEL_SEG_ID, PLUGIN_ID, POMODORO_SECS,
        PRESET_5, PRESET_25, SEG_ID, SHORT_BREAK_SECS, Timer, parse_duration,
    };
    use hytte_plugin::display::{RenderMode, StyleName, testing::with_render_mode};
    use hytte_plugin::preem::{DisplayStyle, seven_seg};
    use hytte_plugin::proto::preem::{AccentRole, PreemWidget};
    use hytte_plugin::proto::{
        Capability, Effect, EventKind, Manifest, Node, Page, PluginMsg, decode, encode,
    };
    use hytte_plugin::{Input, Plugin};
    use std::sync::Mutex;
    use std::time::{Duration, SystemTime};

    /// A fresh idle model, built without the process-level holder so parallel
    /// tests never see each other's state.
    fn fresh() -> Timer {
        Timer::fresh()
    }

    /// A fixed wall-clock origin, `s` seconds after which the tests read "now".
    fn at(s: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + s)
    }

    /// Start `secs` at the test origin.
    fn start(m: &mut Timer, secs: u32) {
        m.start_at(secs, at(0));
    }

    /// An isolated holder, standing in for `HELD`.
    fn holder() -> Holder {
        Mutex::new(None)
    }

    fn click(node: &str) -> Input<super::TimerMsg> {
        Input::event(node, EventKind::Click)
    }

    fn submit(node: &str, text: &str) -> Input<super::TimerMsg> {
        Input::event(
            node.to_owned(),
            EventKind::Submitted {
                text: text.to_owned(),
            },
        )
    }

    /// A tick as the wall clock reads `s` seconds after the origin.
    fn tick(model: &mut Timer, s: u64) -> Vec<Effect> {
        model.tick_at(at(s))
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
        start(&mut m, 3);
        assert_eq!((m.remaining, m.running), (3, true));

        assert!(tick(&mut m, 1).is_empty());
        assert_eq!(m.remaining, 2);
        assert!(tick(&mut m, 2).is_empty());
        assert_eq!(m.remaining, 1);

        // The tick that reaches zero stops the timer and emits exactly one Notify.
        let fx = tick(&mut m, 3);
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
        assert!(tick(&mut m, 4).is_empty());
        assert_eq!(m.remaining, 0);
    }

    /// An idle (paused / not-yet-started) timer ignores ticks.
    #[test]
    fn an_idle_timer_ignores_ticks() {
        let mut m = fresh();
        assert!(!m.running);
        let before = m.remaining;
        assert!(tick(&mut m, 5).is_empty());
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
        start(&mut m, 600);
        m.toggle_pause_at(at(0));
        assert!(!m.running, "pause halts");
        let _ = tick(&mut m, 30);
        assert_eq!(m.remaining, 600, "paused timers don't advance");
        m.toggle_pause_at(at(30));
        assert!(m.running, "a second toggle resumes");

        let _ = tick(&mut m, 31);
        assert_eq!(m.remaining, 599);
        let _ = m.update(click(super::RESET_ID));
        assert_eq!(
            (m.remaining, m.running),
            (600, false),
            "reset restores total"
        );
    }

    /// A deadline crossed while the process was suspended (the monotonic tick
    /// clock stood still) fires on the very next tick, not 20 minutes later.
    #[test]
    fn a_deadline_crossed_in_suspend_fires_on_the_next_tick() {
        let mut m = fresh();
        start(&mut m, 25 * 60);
        assert!(tick(&mut m, 5 * 60).is_empty());
        assert_eq!(m.remaining, 20 * 60);

        // Suspended for 25 minutes: the next tick sees the wall clock far past
        // the deadline.
        let fx = tick(&mut m, 30 * 60);
        assert_eq!(m.remaining, 0);
        assert!(!m.running);
        assert!(matches!(fx.as_slice(), [Effect::Notify { .. }]));
        assert!(tick(&mut m, 31 * 60).is_empty(), "exactly one Notify");

        // A suspend that ends before the deadline just shows the lost time.
        let mut m = fresh();
        start(&mut m, 25 * 60);
        assert!(tick(&mut m, 20 * 60).is_empty());
        assert_eq!(m.remaining, 5 * 60);
    }

    /// Time that passes while paused is not counted, and resume re-arms the
    /// deadline from the resume instant.
    #[test]
    fn pause_and_resume_across_a_gap() {
        let mut m = fresh();
        start(&mut m, 600);
        assert!(tick(&mut m, 100).is_empty());
        m.toggle_pause_at(at(100));
        assert_eq!((m.remaining, m.running, m.deadline), (500, false, None));

        // An hour passes (suspend included) while paused.
        let _ = tick(&mut m, 3700);
        assert_eq!(m.remaining, 500);
        m.toggle_pause_at(at(3700));
        assert_eq!((m.remaining, m.running), (500, true));
        assert!(tick(&mut m, 3700 + 499).is_empty());
        assert_eq!(m.remaining, 1);
        let fx = tick(&mut m, 3700 + 500);
        assert!(matches!(fx.as_slice(), [Effect::Notify { .. }]));
    }

    /// A wall clock stepped backwards never makes the readout grow past what it
    /// showed (or past the original duration); the countdown carries on from
    /// the new clock.
    #[test]
    fn a_backwards_clock_jump_never_grows_the_readout() {
        let mut m = fresh();
        start(&mut m, 600);
        assert!(tick(&mut m, 100).is_empty());
        assert_eq!(m.remaining, 500);

        // NTP steps the clock back an hour.
        let back = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - 3600);
        assert!(m.tick_at(back).is_empty());
        assert_eq!(m.remaining, 500, "no growth");
        assert!(m.remaining <= m.total);

        // And it keeps counting down on the new clock.
        assert!(m.tick_at(back + Duration::from_secs(10)).is_empty());
        assert_eq!(m.remaining, 490);
        let fx = m.tick_at(back + Duration::from_secs(500));
        assert!(matches!(fx.as_slice(), [Effect::Notify { .. }]));
    }

    /// A reconnect re-runs `init`: a running timer must survive it, and a
    /// deadline that passed in the gap fires on the first tick after.
    #[test]
    fn reinit_keeps_a_running_timer() {
        let held = holder();
        assert_eq!(Timer::restore(&held), Timer::fresh(), "empty holder: idle");

        let mut m = Timer::restore(&held);
        start(&mut m, 600);
        assert!(tick(&mut m, 60).is_empty());
        m.remember(&held);

        let mut again = Timer::restore(&held);
        assert_eq!(
            (again.remaining, again.total, again.running),
            (540, 600, true)
        );
        assert!(tick(&mut again, 120).is_empty());
        assert_eq!(again.remaining, 480);

        // The shell was down past the deadline.
        again.remember(&held);
        let mut late = Timer::restore(&held);
        let fx = tick(&mut late, 10_000);
        assert!(matches!(fx.as_slice(), [Effect::Notify { .. }]));

        // A paused timer survives too, without a deadline.
        let mut p = fresh();
        start(&mut p, 600);
        p.toggle_pause_at(at(10));
        p.remember(&held);
        let p = Timer::restore(&held);
        assert_eq!((p.remaining, p.running, p.deadline), (590, false, None));
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

    /// The migration's compat promise (#884): against a shell that does **not**
    /// advertise the preem vocabulary, both readouts must reach the host as the
    /// exact same pixels a hand-written `preem::seven_seg(…).into_node(…)`
    /// produced before the seam existed — chip *and* panel, byte for byte.
    ///
    /// Only comparing the buffers proves it, so this compares whole nodes: the
    /// id, the class list and every one of the RGBA bytes.
    #[test]
    fn against_an_old_shell_both_readouts_are_rasterised_seven_segs() {
        let mut m = fresh();
        // A face with a digit in every position, so a wrong glyph can't hide.
        start(&mut m, 23 * 60 + 45);
        let view = with_render_mode(RenderMode::Raster, || m.view());

        let Node::Box { children, .. } = &view.tree else {
            panic!("root is a box");
        };
        let [Node::Button { child, .. }] = children.as_slice() else {
            panic!("the chip holds exactly the click target");
        };
        // `==` rather than `assert_eq!`: these carry a `Node::Pixels`, whose
        // `Debug` would dump the whole RGBA buffer into the failure output.
        assert!(
            **child == seven_seg("23:45", DisplayStyle::Vfd).into_node(Some(SEG_ID), vec![]),
            "the raster chip must match the kit by hand",
        );

        let Some(Node::Box { children, .. }) = &view.panel else {
            panic!("the panel is a box");
        };
        assert!(
            children[0]
                == seven_seg("23:45", DisplayStyle::Vfd).into_node(Some(PANEL_SEG_ID), vec![]),
            "the raster panel readout must match the kit by hand",
        );
    }

    /// Unwrap one readout and assert it is the seven-seg state the shell will
    /// draw: the reconciler's key, the plugin's own reading, and the skin as a
    /// *name* rather than resolved colors.
    fn assert_seven_seg(node: &Node, want_id: &str, want_text: &str) {
        match node {
            Node::Preem { id, widget, .. } => {
                assert_eq!(id.as_deref(), Some(want_id), "the reconciler's key");
                match widget.as_ref() {
                    PreemWidget::SevenSeg { config, state } => {
                        assert_eq!(state.text, want_text, "the plugin's own reading");
                        assert_eq!(
                            config.style.style,
                            StyleName::Vfd,
                            "the skin travels as a name, never as colors",
                        );
                        // #914 LOW: without this, `.neutral()` (or any other
                        // role, or a pinned color) could be added to the
                        // readouts and every assertion here would stay green —
                        // the readout would just stop following the desktop
                        // accent on glass, which no test could see.
                        assert_eq!(
                            config.style.accent,
                            Some(AccentRole::Accent),
                            "the readout follows the desktop accent (#396), and says so on the wire",
                        );
                        assert_eq!(
                            (config.style.ink, config.style.field),
                            (None, None),
                            "…so it pins no color of its own",
                        );
                    }
                    other => panic!("expected a seven-seg widget, got {other:?}"),
                }
            }
            Node::Pixels { .. } => panic!("a preem-speaking host must not get pixels"),
            other => panic!("expected Node::Preem, got {other:?}"),
        }
    }

    /// …and against a shell that advertises the vocabulary, the *same* `view`
    /// ships typed state nodes instead — same ids, same reading, the skin as a
    /// name, and no pixels anywhere in either tree (#884).
    #[test]
    fn against_a_preem_shell_both_readouts_are_state_nodes() {
        let mut m = fresh();
        start(&mut m, 23 * 60 + 45);
        let view = with_render_mode(RenderMode::State, || m.view());

        let Node::Box { children, .. } = &view.tree else {
            panic!("root is a box");
        };
        let [Node::Button { child, .. }] = children.as_slice() else {
            panic!("the chip holds exactly the click target");
        };
        assert_seven_seg(child, SEG_ID, "23:45");

        let Some(Node::Box { children, .. }) = &view.panel else {
            panic!("the panel is a box");
        };
        assert_seven_seg(&children[0], PANEL_SEG_ID, "23:45");
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
        start(&mut m, 1);
        let effects = tick(&mut m, 1);
        assert!(matches!(effects.as_slice(), [Effect::Notify { .. }]));
        let view = m.view();
        let render = PluginMsg::Render {
            tree: view.tree,
            panel: view.panel.map(Box::new),
            hidden_on: view.hidden_on,
            effects,
        };
        let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
        assert_eq!(render, back);
    }
}
