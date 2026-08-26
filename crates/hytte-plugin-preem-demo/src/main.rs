//! `hytte-plugin-preem-demo` — the showcase plugin for the SDK's **preem
//! raster kit** (issue #356; `hytte_plugin::preem`).
//!
//! One sidebar card cycling every kit widget through every skin: a
//! seven-segment **HH:MM clock**, a **dot-matrix ticker** stepping one char
//! per second, a **scrolling marquee** panning a pixel window across a
//! pre-rendered strip, and an **8bit textbox**, all rotating VFD → LCD → OLED
//! every [`STYLE_SECS`] seconds. Below them a real **oscilloscope**
//! ([`Scope`], #556/#397) sweeps the 16-band `AudioSpectrum` push as a
//! glow-trace waveform over a graticule, with real phosphor decay — a silent
//! sink flatlines on the axis while the old trail keeps ghosting. Below *that*
//! a **needle gauge** ([`Gauge`], #397) steps between readings and swings to
//! each one on a real damped oscillator: the pointer overshoots, bounces once,
//! and settles, smearing while it moves. Tapping the clock advances the skin
//! immediately. It doubles as the kit's visual regression harness and the
//! copy-from reference for plugin authors.
//!
//! # The scope and the gauge are the card's two stateful widgets
//!
//! Every other widget is a pure function of the snapshot, but the scope carries
//! a phosphor buffer across frames, so it lives in the model: each host
//! heartbeat (the ~1 Hz `Clock` snapshot) [`advance`](Scope::advance)s it one
//! sweep with the latest bands, and `view` [`render`](Scope::render)s the
//! current trace. A frame-timer plugin would advance per frame for a fast
//! sweep; at the demo's 1 Hz heartbeat the lush decay is on full display (you
//! watch the ghost linger for several seconds).
//!
//! The gauge carries *physics* rather than pixels — a deflection and a velocity
//! — and is advanced the same way, by [`GAUGE_SLOWMO_DT`] of needle time per
//! heartbeat rather than the full second, so a swing plays out over several
//! renders instead of completing between two of them. Its target steps through
//! [`GAUGE_STOPS`] every [`GAUGE_HOLD_SECS`], which is a pure function of the
//! snapshot clock like everything else here.
//!
//! # …and the scope alone is the card's **parked** widget (#422)
//!
//! Being stateful is also the only reason this card subscribes
//! [`SlotVisible`](Input::SlotVisible) at all: the scope is the one widget that
//! parks. While the sidebar is closed no spectrum push arrives (the shell drops
//! the `PipeWire` tap once nothing on-screen wants it, #565/#583), yet the
//! 1 Hz heartbeat keeps coming: left
//! ungated, every tick would re-stamp the *same* frozen bands, saturating the
//! phosphor into a solid constant waveform that then needs several sweeps to
//! decay once the card is looked at again. So the heartbeat's sweep is gated on
//! visibility and the hide edge [`clear`](Scope::clear)s the phosphor — a
//! reopened card starts from a dark screen and re-derives from the next sweep.
//!
//! Everything else deliberately keeps running while hidden: the clock, ticker,
//! marquee, skin rotation **and the gauge** are pure functions of the snapshot,
//! so parking them would buy nothing and would make the reopened card show a
//! *stale time* — the very failure the scope's park exists to avoid. The gauge
//! is the instructive case: it is stateful like the scope, but its input is
//! derived rather than pushed, so it is the *input*, not the statefulness, that
//! decides whether a widget must park. ([`Needle::settle`] is there for a gauge
//! whose reading really does come from outside.)
//!
//! [`Needle::settle`]: hytte_plugin::preem::Needle::settle
//!
//! # Shape — The Elm Architecture, purely host-driven
//!
//! Like `hytte-plugin-clock-demo`, everything below is the pure TEA core.
//! There are **no sources and no timers**: the shell's `Clock` subscription
//! re-snapshots every second, and both the ticker step and the style
//! rotation are pure functions of the snapshot's unix time (plus a click
//! offset) — so `view` stays deterministic and unit-testable, and the
//! runtime's render dedup sees a fresh tree each second without the plugin
//! owning any cadence (the kit renders frames; it owns no clock).
//!
//! # Sizing — the #313 lesson
//!
//! Every widget is sized via its **buffer dimensions** (a `Pixels` node's
//! natural size), not shell CSS: the 7seg clock renders 188 px wide, the
//! 11-char ticker 268 px, the marquee window [`MARQUEE_WINDOW_PX`] wide, the
//! 22-column ×2 textbox 274 px, and the scope's and gauge's defaults 288 px each
//! — all inside the sidebar card's ~296 px content width.

use hytte_plugin::preem::{DisplayStyle, Gauge, Marquee, Scope, TextBox, dot_matrix, seven_seg};
use hytte_plugin::proto::{Dir, Effect, EventKind, Manifest, Mount, Node, SPECTRUM_BINS, StateKey};
use hytte_plugin::{CmdSender, Input, Plugin, View};

/// Stable plugin id — the host's mount-slot ownership key.
const PLUGIN_ID: &str = "preem-demo";
/// Node ids. The clock button is the click target (cycle the skin); the
/// `Pixels` ids make each re-render swap its texture in place.
const ROOT_ID: &str = "preem-demo-root";
const CYCLE_BTN: &str = "preem-demo-cycle";
const SEG_ID: &str = "preem-demo-7seg";
const TICKER_ID: &str = "preem-demo-ticker";
const MARQUEE_ID: &str = "preem-demo-marquee";
const TEXT_ID: &str = "preem-demo-textbox";
const SCOPE_ID: &str = "preem-demo-scope";
const GAUGE_ID: &str = "preem-demo-gauge";

/// Seconds each skin holds before the rotation advances.
const STYLE_SECS: i64 = 10;

/// The readings the showcase gauge steps between, one every
/// [`GAUGE_HOLD_SECS`]. Deliberately mid-scale: a full-scale slam would peg the
/// needle against the dial's stop and hide the overshoot the widget exists for.
const GAUGE_STOPS: [f32; 6] = [10.0, 72.0, 38.0, 90.0, 22.0, 55.0];
/// Host heartbeats each reading holds before the next one is dialed in — long
/// enough that the needle has visibly settled before the next step.
const GAUGE_HOLD_SECS: i64 = 8;
/// Seconds of needle motion the gauge is advanced per host heartbeat.
///
/// **Slow motion, on purpose.** This card has no frame timer — its only cadence
/// is the shell's ~1 Hz `Clock` snapshot (see the crate docs) — so advancing by
/// the real elapsed second would land the needle settled every time and the
/// physics would never be visible. At this rate the eight heartbeats of a hold
/// window cover the whole 1.3 s response, so successive renders actually show
/// the kick, the overshoot, the bounce and the settle. A frame-timer plugin
/// passes its real elapsed seconds instead; the kit owns no clock either way.
const GAUGE_SLOWMO_DT: f32 = 0.16;
/// The ticker's marquee message; the view shows a [`TICKER_WINDOW`]-char
/// window that advances one char per second (wrapping around).
const TICKER: &str = "PREEM RASTER KIT ~ VFD / LCD / OLED ~ 7SEG DOT 8BIT ~ ";
/// Chars of [`TICKER`] visible at once: 11 dot-matrix cells = 268 px, the
/// widest that fits the ~296 px sidebar card.
const TICKER_WINDOW: usize = 11;
/// The marquee's message — wider than the window, so it scrolls (every char is
/// font-covered; see the [`demo_copy_is_fully_covered_by_the_font`] test).
///
/// [`demo_copy_is_fully_covered_by_the_font`]: tests::demo_copy_is_fully_covered_by_the_font
const MARQUEE_MSG: &str = "SCROLLING MARQUEE ~ DOT-MATRIX PIXEL TICKER ~ ";
/// The marquee window width in pixels — a wide bar-chip ticker that stays
/// within the ~296 px sidebar card.
const MARQUEE_WINDOW_PX: usize = 268;
/// Pixels the marquee pans per host snapshot. The shell re-snapshots ~1 Hz, so
/// the demo steps once a second; a frame-timer plugin would bump the offset
/// every frame for a smooth scroll (the kit owns no clock).
const MARQUEE_STEP_PX: usize = 6;
/// The textbox wrap width: 22 columns at ×2 scale = 274 px.
const TEXT_COLS: usize = 22;

/// The demo's entire state — rebuilt on every (re)connect, re-derived from
/// the next snapshot.
// `Eq` is intentionally not derived: `bins` is `[f32; N]`, which is `PartialEq`
// but not `Eq`. Nothing compares the whole model for equality anyway.
#[derive(Debug, PartialEq)]
struct PreemDemo {
    /// `"HH:MM"` from the host clock's ISO timestamp (`"--:--"` until the
    /// first snapshot lands — all-ghost dashes on the readout).
    hhmm: String,
    /// Latest unix seconds; drives the ticker step and the skin rotation.
    unix: i64,
    /// Click offset into the skin rotation (tapping the clock bumps it).
    style_bump: u32,
    /// Latest 16-band audio spectrum off the default sink's monitor (#405),
    /// swept by the scope. All-zero until the first push lands (a flat
    /// baseline), so the trace shows even on a silent desktop.
    bins: [f32; SPECTRUM_BINS],
    /// The oscilloscope (#556): a stateful widget carrying its phosphor buffer
    /// across frames. Advanced one sweep per host heartbeat with [`Self::bins`]
    /// **while the card is on-screen**, rendered by `view` in the current skin.
    scope: Scope,
    /// The needle gauge (#397): the card's other stateful widget, and the one
    /// that carries *physics* rather than a pixel buffer. Its target steps
    /// through [`GAUGE_STOPS`] on the heartbeat and the needle swings to it —
    /// overshooting and settling — over the following ticks.
    ///
    /// Unlike the scope this deliberately does **not** park (#422): its input is
    /// a pure function of the snapshot, exactly like the clock and the ticker,
    /// so keeping it live means a reopened card reads the current value instead
    /// of replaying a stale swing.
    gauge: Gauge,
    /// Whether the sidebar (and so this card) is currently shown — the
    /// [`SlotVisible`](Input::SlotVisible) gate for the scope's sweep (#422).
    /// Seeded `false`; the host sends the real value at register.
    visible: bool,
}

/// The `HH:MM` slice of an RFC 3339 local timestamp
/// (`2026-07-16T15:49:00+02:00` → `15:49`), or `None` for anything that
/// doesn't look like one.
fn parse_hhmm(iso: &str) -> Option<&str> {
    let s = iso.get(11..16)?;
    let b = s.as_bytes();
    let digits = [0usize, 1, 3, 4].iter().all(|&i| b[i].is_ascii_digit());
    (digits && b[2] == b':').then_some(s)
}

impl PreemDemo {
    /// The skin currently on display: unix time rotates through
    /// [`DisplayStyle::ALL`] every [`STYLE_SECS`], and each click on the
    /// clock advances the rotation by one.
    fn style(&self) -> DisplayStyle {
        let slot = self.unix.div_euclid(STYLE_SECS) + i64::from(self.style_bump);
        let n = i64::try_from(DisplayStyle::ALL.len()).unwrap_or(1);
        let idx = usize::try_from(slot.rem_euclid(n)).unwrap_or(0);
        DisplayStyle::ALL[idx]
    }

    /// The ticker's visible window, advanced one char per second.
    fn ticker_window(&self) -> String {
        let chars: Vec<char> = TICKER.chars().collect();
        let len = i64::try_from(chars.len()).unwrap_or(1).max(1);
        let off = usize::try_from(self.unix.rem_euclid(len)).unwrap_or(0);
        chars.iter().cycle().skip(off).take(TICKER_WINDOW).collect()
    }

    /// The marquee's scroll offset, panning [`MARQUEE_STEP_PX`] px per second.
    /// [`MarqueeStrip::window`](hytte_plugin::preem::MarqueeStrip::window) wraps
    /// this modulo the strip period, so the raw (unbounded) counter is fine.
    fn marquee_offset(&self) -> usize {
        // Bound the seconds before scaling so the multiply can never overflow;
        // the window's own modulo makes the absolute value irrelevant.
        let secs = usize::try_from(self.unix.rem_euclid(1_000_000)).unwrap_or(0);
        secs.saturating_mul(MARQUEE_STEP_PX)
    }

    /// Park the scope (#422): forget the last bands and wipe the phosphor, so a
    /// reopened card starts from a dark screen instead of the trace — or, worse,
    /// the saturated constant waveform an ungated heartbeat would have stamped —
    /// left over from whenever the sidebar closed. Called on the `SlotVisible`
    /// falling edge; idempotent, so a repeat while already parked is harmless.
    fn park(&mut self) {
        self.bins = [0.0; SPECTRUM_BINS];
        self.scope.clear();
    }

    /// The reading the gauge is currently dialed to: [`GAUGE_STOPS`] stepped
    /// once every [`GAUGE_HOLD_SECS`] of host clock. A pure function of the
    /// snapshot, like the ticker window and the skin rotation.
    fn gauge_target(&self) -> f32 {
        let slot = self.unix.div_euclid(GAUGE_HOLD_SECS);
        let count = i64::try_from(GAUGE_STOPS.len()).unwrap_or(1);
        let index = usize::try_from(slot.rem_euclid(count)).unwrap_or(0);
        GAUGE_STOPS[index]
    }

    /// The textbox's line — names the current skin so the rotation is
    /// legible in all three widgets at once.
    fn textbox_line(style: DisplayStyle) -> String {
        format!(
            "8bit textbox in the {} skin - wraps, clips and never panics",
            style.name()
        )
    }
}

impl Plugin for PreemDemo {
    /// Purely host-driven: the clock snapshot is the only heartbeat.
    type Msg = std::convert::Infallible;
    /// Purely display: no I/O of its own, no commands.
    type Cmd = std::convert::Infallible;

    /// Subscribes to `Clock` (the heartbeat), `AudioSpectrum` (the scope tile's
    /// input, #405) and `SlotVisible` (the scope's park gate, #422), mounts
    /// `SidebarTop` under the clock demo (`order = 1`; unordered co-mounts sort
    /// as 0 — #303). No capabilities: the demo asks nothing of the shell.
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, Mount::SidebarTop).with_order(1);
        m.subscribes = vec![
            StateKey::Clock,
            StateKey::AudioSpectrum,
            StateKey::SlotVisible,
        ];
        m
    }

    /// All-ghost placeholders until the first snapshot lands (the runtime
    /// renders this seed immediately, so the card mounts right away).
    fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
        Self {
            hhmm: "--:--".to_owned(),
            unix: 0,
            style_bump: 0,
            bins: [0.0; SPECTRUM_BINS],
            scope: Scope::new(),
            gauge: Gauge::new().range(0.0, 100.0),
            visible: false,
        }
    }

    /// Fold one input into the model. Pure and panic-free over any
    /// host-sent value; re-rendering is the runtime's problem.
    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            // `clock` is optional on the wire (a startup snapshot may
            // arrive before the host's clock pump has published).
            Input::Snapshot(snapshot) => {
                if let Some(clock) = snapshot.clock {
                    if let Some(hhmm) = parse_hhmm(&clock.iso) {
                        hhmm.clone_into(&mut self.hhmm);
                    }
                    self.unix = clock.unix;
                }
                // The heartbeat is the scope's sweep tick: advance the phosphor
                // one frame with the latest bands (silence flatlines on the axis
                // while the old trail decays — the honest ghost). Gated on
                // visibility (#422): hidden, no fresh bands arrive, so sweeping
                // would just re-stamp the frozen ones into a saturated trace.
                if self.visible {
                    self.scope.advance(&self.bins);
                }
                // The gauge's own heartbeat: dial in the current reading and let
                // the needle move toward it. Ungated on visibility, unlike the
                // scope — see the field docs — and advanced in slow motion so
                // the swing is legible at a 1 Hz cadence (`GAUGE_SLOWMO_DT`).
                self.gauge.set_target(self.gauge_target());
                self.gauge.advance(GAUGE_SLOWMO_DT);
            }
            // Tapping the clock advances the skin rotation by one.
            Input::Event { node, kind } => {
                if node == CYCLE_BTN && matches!(kind, EventKind::Click) {
                    self.style_bump = self.style_bump.wrapping_add(1);
                }
            }
            // The audio-spectrum push (#405): store the latest bands; the scope
            // sweeps them on the next heartbeat tick. Ignored while parked —
            // the tap is refcounted across every subscriber (#559), so another
            // on-screen one can keep frames arriving at this hidden card.
            Input::AudioSpectrum(spectrum) => {
                if self.visible {
                    self.bins = spectrum.bins;
                }
            }
            // The scope's park gate (#422): going off-screen wipes the phosphor
            // so the reopen isn't a stale (or saturated) trace. The rest of the
            // card is a pure function of the snapshot and keeps cycling whether
            // or not anyone is looking — see the crate docs.
            Input::SlotVisible(v) => {
                if self.visible && !v {
                    self.park();
                }
                self.visible = v;
            }
            // No RunCommand is issued and nothing else is subscribed — the
            // remaining pushes are no-ops.
            Input::EffectResult { .. }
            | Input::ConsentDecision { .. }
            | Input::CalendarUpcoming(_)
            | Input::SessionLocked(_)
            | Input::NowPlaying(_)
            | Input::DatasourceQuery { .. }
            | Input::DatasourceResult { .. } => {}
            // `Msg = Infallible`: there are no app messages to receive.
            Input::App(never) => match never {},
        }
        Vec::new()
    }

    /// One vertical card: the pokeable 7seg clock, the ticker, the scrolling
    /// marquee, the textbox — all wearing the same skin — the audio
    /// oscilloscope, and a dim hint line. Every `Pixels` buffer satisfies the
    /// host's `len == w * h * 4` invariant by kit construction.
    fn view(&self) -> View {
        let style = self.style();
        let clock = seven_seg(&self.hhmm, style);
        let ticker = dot_matrix(&self.ticker_window(), style);
        let marquee = Marquee::new(style)
            .window_px(MARQUEE_WINDOW_PX)
            .render(MARQUEE_MSG)
            .window(self.marquee_offset());
        let textbox = TextBox::styled(style)
            .cols(TEXT_COLS)
            .scale(2)
            .render(&Self::textbox_line(style));
        // The scope carries its phosphor across frames; `advance` already ran on
        // the heartbeat, so `view` just renders the current trace in this skin.
        let scope = self.scope.render(style);
        // Likewise the gauge: `update` already moved the needle, so `view` only
        // draws where it currently points.
        let gauge = self.gauge.render(style);
        Node::Box {
            id: Some(ROOT_ID.to_owned()),
            dir: Dir::Vertical,
            spacing: 8,
            scroll: false,
            classes: Vec::new(),
            children: vec![
                Node::Button {
                    id: CYCLE_BTN.to_owned(),
                    classes: vec!["flat".to_owned()],
                    child: Box::new(clock.into_node(Some(SEG_ID), Vec::new())),
                },
                ticker.into_node(Some(TICKER_ID), Vec::new()),
                marquee.into_node(Some(MARQUEE_ID), Vec::new()),
                textbox.into_node(Some(TEXT_ID), Vec::new()),
                scope.into_node(Some(SCOPE_ID), Vec::new()),
                gauge.into_node(Some(GAUGE_ID), Vec::new()),
                Node::Label {
                    id: None,
                    text: "tap the clock to switch skins".to_owned(),
                    classes: vec!["dim-label".to_owned()],
                },
            ],
        }
        .into()
    }
}

fn main() {
    hytte_plugin::run::<PreemDemo>()
}

#[cfg(test)]
mod tests {
    use super::{CYCLE_BTN, PreemDemo, STYLE_SECS, parse_hhmm};
    use hytte_plugin::preem::{DisplayStyle, Marquee};
    use hytte_plugin::proto::{
        ClockState, EventKind, Node, PluginMsg, StateKey, StateSnapshot, decode, encode,
    };
    use hytte_plugin::{Input, Plugin};

    fn fresh() -> PreemDemo {
        PreemDemo::init(hytte_plugin::cmd_channel().0)
    }

    /// An on-screen card (the register-time `SlotVisible` seed already
    /// delivered) — the state the scope actually sweeps in (#422).
    fn shown() -> PreemDemo {
        let mut m = fresh();
        let _ = m.update(Input::SlotVisible(true));
        m
    }

    fn snapshot(iso: &str, unix: i64) -> Input<std::convert::Infallible> {
        Input::Snapshot(StateSnapshot {
            clock: Some(ClockState {
                iso: iso.to_owned(),
                unix,
            }),
        })
    }

    fn spectrum(bins: [f32; super::SPECTRUM_BINS]) -> Input<std::convert::Infallible> {
        Input::AudioSpectrum(hytte_plugin::proto::AudioSpectrum { peak: 0.9, bins })
    }

    /// Walk a tree and collect every `Pixels` node's parts.
    fn pixels_of(node: &Node) -> Vec<(u32, u32, usize)> {
        let mut out = Vec::new();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            match n {
                Node::Pixels {
                    width,
                    height,
                    data,
                    ..
                } => out.push((*width, *height, data.len())),
                Node::Box { children, .. }
                | Node::Row { children, .. }
                | Node::ListBox { children, .. } => stack.extend(children.iter()),
                Node::Button { child, .. } => stack.push(child),
                _ => {}
            }
        }
        out
    }

    /// A snapshot updates the readout and the ticker clock.
    #[test]
    fn snapshot_updates_the_model() {
        let mut m = fresh();
        let fx = m.update(snapshot("2026-07-16T15:49:00+02:00", 1_752_672_540));
        assert!(fx.is_empty());
        assert_eq!(m.hhmm, "15:49");
        assert_eq!(m.unix, 1_752_672_540);
    }

    /// Garbage timestamps keep the placeholder instead of poisoning the
    /// readout (the 7seg renders unknown chars as blanks either way).
    #[test]
    fn nonsense_iso_keeps_the_placeholder() {
        assert_eq!(parse_hhmm("2026-07-16T15:49:00+02:00"), Some("15:49"));
        assert_eq!(parse_hhmm("short"), None);
        assert_eq!(parse_hhmm("2026-07-16Tno:pe:00+02:00"), None);
        assert_eq!(parse_hhmm(""), None);
        let mut m = fresh();
        let _ = m.update(snapshot("nope", 7));
        assert_eq!(m.hhmm, "--:--", "placeholder survives");
        assert_eq!(m.unix, 7, "but the unix clock still advances");
    }

    /// The skin rotates with unix time and a click advances it by one.
    #[test]
    fn style_rotates_with_time_and_clicks() {
        let mut m = fresh();
        let base = m.style();
        m.unix += STYLE_SECS;
        assert_ne!(m.style(), base, "the rotation advances every slot");
        m.unix -= STYLE_SECS;
        assert_eq!(m.style(), base);
        let fx = m.update(Input::Event {
            node: CYCLE_BTN.to_owned(),
            kind: EventKind::Click,
        });
        assert!(fx.is_empty(), "the demo asks nothing of the shell");
        assert_ne!(m.style(), base, "a tap advances the skin");
        // Three slots = a full lap through DisplayStyle::ALL.
        let lap = m.style();
        m.unix += STYLE_SECS * 3;
        assert_eq!(m.style(), lap);
    }

    /// A click on a node we don't own changes nothing.
    #[test]
    fn foreign_clicks_are_ignored() {
        let mut m = fresh();
        let before = m.view();
        let fx = m.update(Input::Event {
            node: "not-ours".to_owned(),
            kind: EventKind::Click,
        });
        assert!(fx.is_empty());
        assert_eq!(m.view(), before);
    }

    /// Every `Pixels` buffer in the view honors the host's
    /// `len == w * h * 4` invariant and the ~296 px sidebar content width
    /// (the #313 lesson) — across all skins and clock states.
    #[test]
    fn every_view_pixels_is_valid_and_fits_the_card() {
        let mut m = fresh();
        for step in 0..6 {
            let tree = m.view().tree;
            let bufs = pixels_of(&tree);
            assert_eq!(
                bufs.len(),
                6,
                "clock + ticker + marquee + textbox + scope + gauge"
            );
            for (w, h, len) in bufs {
                assert_eq!(len, (w as usize) * (h as usize) * 4);
                assert!(w > 0 && h > 0);
                assert!(w <= 296, "width {w} fits the sidebar card");
            }
            let _ = m.update(snapshot("2026-07-16T23:59:00+02:00", step * STYLE_SECS));
        }
    }

    /// The view is a pure function of the model, and each skin renders a
    /// visibly different card.
    #[test]
    fn view_is_deterministic_and_skins_differ() {
        let mut m = fresh();
        let _ = m.update(snapshot("2026-07-16T12:00:00+02:00", 100));
        assert_eq!(m.view(), m.view(), "view is pure");
        let a = m.style();
        let before = m.view();
        let _ = m.update(Input::Event {
            node: CYCLE_BTN.to_owned(),
            kind: EventKind::Click,
        });
        assert_ne!(m.style(), a);
        assert_ne!(m.view(), before, "a new skin renders a new card");
    }

    /// The ticker window advances with the clock and wraps around.
    #[test]
    fn ticker_steps_and_wraps() {
        let mut m = fresh();
        let at = |m: &PreemDemo| m.ticker_window();
        let w0 = at(&m);
        m.unix += 1;
        assert_ne!(at(&m), w0, "one second, one char");
        let len = i64::try_from(super::TICKER.chars().count()).unwrap();
        m.unix += len - 1;
        assert_eq!(at(&m), w0, "a full lap wraps to the same window");
        assert_eq!(w0.chars().count(), super::TICKER_WINDOW);
    }

    /// The marquee pans forward with the host clock (a live frame timer would
    /// bump the offset per frame), and the panned pixels actually move.
    #[test]
    fn marquee_pans_with_the_clock() {
        let mut m = fresh();
        let _ = m.update(snapshot("2026-07-16T00:00:03+02:00", 300));
        let o0 = m.marquee_offset();
        let _ = m.update(snapshot("2026-07-16T00:00:04+02:00", 301));
        let o1 = m.marquee_offset();
        assert_eq!(o1 - o0, super::MARQUEE_STEP_PX, "one second, one step");

        let strip = Marquee::new(m.style())
            .window_px(super::MARQUEE_WINDOW_PX)
            .render(super::MARQUEE_MSG);
        assert!(strip.scrolls(), "the demo message overflows the window");
        assert_ne!(
            strip.window(o0),
            strip.window(o1),
            "the scroll moves pixels"
        );
    }

    /// Every ticker/marquee/textbox char is covered by the kit font — no
    /// accidental notdef boxes in the demo's own copy.
    #[test]
    fn demo_copy_is_fully_covered_by_the_font() {
        for c in super::TICKER.chars().chain(super::MARQUEE_MSG.chars()) {
            assert!(
                hytte_plugin::preem::font::glyph(c).is_some(),
                "ticker/marquee char {c:?} has a glyph"
            );
        }
        for style in DisplayStyle::ALL {
            for c in PreemDemo::textbox_line(style).chars() {
                assert!(
                    hytte_plugin::preem::font::glyph(c).is_some(),
                    "textbox char {c:?} has a glyph"
                );
            }
        }
    }

    /// The audio-spectrum push updates the model's bands, and a loud band bends
    /// the scope trace off the axis versus silence (#405/#556).
    #[test]
    fn scope_reacts_to_audio_spectrum() {
        use hytte_plugin::preem::{DisplayStyle, Scope};
        // Silent baseline vs a single loud band → the trace renders differently.
        let mut quiet = Scope::new();
        quiet.advance(&[0.0; super::SPECTRUM_BINS]);
        let mut loud_bins = [0.0_f32; super::SPECTRUM_BINS];
        loud_bins[8] = 1.0;
        let mut loud = Scope::new();
        loud.advance(&loud_bins);
        assert_ne!(
            quiet.render(DisplayStyle::Vfd),
            loud.render(DisplayStyle::Vfd),
            "a loud band bends the trace"
        );

        // The push folds into the model, so the next heartbeat sweeps those bands.
        let mut m = shown();
        let fx = m.update(spectrum(loud_bins));
        assert!(fx.is_empty(), "the demo asks nothing of the shell");
        assert!((m.bins[8] - 1.0).abs() < 1e-6, "band 8 stored");
        assert!(m.bins[0].abs() < 1e-6, "quiet bands stay low");
    }

    /// The scope sweeps on the host heartbeat and ghosts honestly when the sink
    /// goes silent: the old trace decays across ticks rather than snapping to
    /// black or freezing (the #556 phosphor showcase). Compares the scope
    /// buffer directly so the marquee's own scroll doesn't confound it.
    #[test]
    fn scope_sweeps_on_the_heartbeat_with_a_decay_ghost() {
        use hytte_plugin::preem::DisplayStyle;
        let mut m = shown();
        // A loud band, then the heartbeat draws it.
        let mut loud = [0.0_f32; super::SPECTRUM_BINS];
        loud[2] = 1.0;
        let _ = m.update(spectrum(loud));
        let _ = m.update(snapshot("2026-07-16T00:00:00+02:00", 1));
        let lit = m.scope.render(DisplayStyle::Vfd);
        // Silence: the heartbeat keeps sweeping and the loud trail decays.
        let _ = m.update(spectrum([0.0; super::SPECTRUM_BINS]));
        let _ = m.update(snapshot("2026-07-16T00:00:01+02:00", 2));
        let ghost = m.scope.render(DisplayStyle::Vfd);
        assert_ne!(ghost, lit, "silence decays the loud trace to a ghost");
        let _ = m.update(snapshot("2026-07-16T00:00:02+02:00", 3));
        let ghost2 = m.scope.render(DisplayStyle::Vfd);
        assert_ne!(ghost2, ghost, "the ghost keeps fading each heartbeat");
    }

    /// The gauge steps between readings on the host clock, and the needle
    /// **overshoots** on the way — the #397 showcase, checked here at the card
    /// level rather than trusted.
    #[test]
    fn the_gauge_steps_readings_and_overshoots_on_the_way() {
        let mut m = fresh();
        // The stop table advances once per hold window, and wraps.
        let _ = m.update(snapshot("2026-07-16T00:00:00+02:00", 0));
        let first = m.gauge_target();
        let _ = m.update(snapshot(
            "2026-07-16T00:00:00+02:00",
            super::GAUGE_HOLD_SECS,
        ));
        assert!(
            (m.gauge_target() - first).abs() > f32::EPSILON,
            "a new hold window dials in a new reading"
        );
        let lap = i64::try_from(super::GAUGE_STOPS.len()).unwrap() * super::GAUGE_HOLD_SECS;
        let _ = m.update(snapshot("2026-07-16T00:00:00+02:00", lap));
        assert!((m.gauge_target() - first).abs() < 1.0e-6, "and wraps");

        // Held on one reading, the needle swings past it before settling — and
        // does so within the heartbeats the hold window actually gives it.
        let mut m = fresh();
        let mut peak = f32::MIN;
        for second in 0..super::GAUGE_HOLD_SECS {
            let _ = m.update(snapshot("2026-07-16T00:00:00+02:00", second));
            peak = peak.max(m.gauge.value());
        }
        let target = m.gauge_target();
        assert!(
            peak > target + 1.0,
            "the needle overshot {target} (peaked at {peak}) — physics, not a lerp"
        );
        assert!(m.gauge.is_settled(), "and settled inside the hold window");
    }

    /// The gauge deliberately does **not** park (#422): its reading is derived
    /// from the snapshot, so a hidden card keeps it current and a reopen shows
    /// the value rather than replaying a stale swing.
    #[test]
    fn a_hidden_card_keeps_the_gauge_current() {
        let mut m = shown();
        let _ = m.update(Input::SlotVisible(false));
        for second in 0..(super::GAUGE_HOLD_SECS * 6) {
            let _ = m.update(snapshot("2026-07-16T00:00:00+02:00", second));
        }
        let target = m.gauge_target();
        assert!(
            (m.gauge.value() - target).abs() < 1.0,
            "a hidden gauge still tracks its reading ({} vs {target})",
            m.gauge.value()
        );
        assert!(m.gauge.is_settled(), "and is not frozen mid-swing");
    }

    /// The manifest opts into the clock heartbeat, the audio spectrum, and the
    /// slot-visibility gate the scope parks on (#422).
    #[test]
    fn manifest_subscribes_clock_spectrum_and_visibility() {
        let m = PreemDemo::manifest();
        assert!(m.subscribes.contains(&StateKey::Clock));
        assert!(m.subscribes.contains(&StateKey::AudioSpectrum));
        assert!(
            m.subscribes.contains(&StateKey::SlotVisible),
            "the scope's park gate is opt-in per #305"
        );
    }

    /// A hidden card parks the scope (#422): the heartbeat stops sweeping, so a
    /// frozen band set can't saturate the phosphor, and the hide edge wipes what
    /// was already drawn — a reopen starts from a dark screen, not the trace from
    /// whenever the sidebar closed.
    #[test]
    fn a_hidden_card_parks_the_scope() {
        use hytte_plugin::preem::{DisplayStyle, Scope};
        let dark = Scope::new().render(DisplayStyle::Vfd);

        let mut m = shown();
        let mut loud = [0.0_f32; super::SPECTRUM_BINS];
        loud[2] = 1.0;
        let _ = m.update(spectrum(loud));
        let _ = m.update(snapshot("2026-07-16T00:00:00+02:00", 1));
        assert_ne!(m.scope.render(DisplayStyle::Vfd), dark, "a trace is drawn");

        // Hide: the phosphor is wiped and the bands forgotten.
        let _ = m.update(Input::SlotVisible(false));
        assert_eq!(
            m.scope.render(DisplayStyle::Vfd),
            dark,
            "hiding wipes the trace"
        );
        assert!(m.bins.iter().all(|&b| b <= 0.0), "and forgets the bands");

        // Hidden: heartbeats no longer sweep, and a late push (another
        // subscriber may hold the tap up) doesn't re-arm the bands.
        let _ = m.update(spectrum(loud));
        for s in 2..8 {
            let _ = m.update(snapshot("2026-07-16T00:00:00+02:00", s));
        }
        assert_eq!(
            m.scope.render(DisplayStyle::Vfd),
            dark,
            "a parked scope stays dark across heartbeats"
        );

        // Reopen: still dark until real data sweeps again.
        let _ = m.update(Input::SlotVisible(true));
        assert_eq!(
            m.scope.render(DisplayStyle::Vfd),
            dark,
            "the reopened card starts from a dark screen"
        );
        let _ = m.update(spectrum(loud));
        let _ = m.update(snapshot("2026-07-16T00:00:09+02:00", 9));
        assert_ne!(
            m.scope.render(DisplayStyle::Vfd),
            dark,
            "and re-derives from the next sweep"
        );
    }

    /// The rest of the card deliberately does **not** park: the clock, ticker,
    /// marquee and skin rotation are pure functions of the snapshot, so a hidden
    /// card keeps them current and a reopen never shows a stale time (#422).
    #[test]
    fn a_hidden_card_still_tracks_the_clock() {
        let mut m = shown();
        let _ = m.update(snapshot("2026-07-16T15:49:00+02:00", 1_752_672_540));
        let _ = m.update(Input::SlotVisible(false));
        let _ = m.update(snapshot("2026-07-16T15:50:00+02:00", 1_752_672_600));
        assert_eq!(m.hhmm, "15:50", "the readout stays current while hidden");
        assert_eq!(m.unix, 1_752_672_600, "and so does the rotation clock");
    }

    /// The frames built from this plugin's data are valid on the wire.
    #[test]
    fn register_and_render_frames_round_trip() {
        let reg = PluginMsg::Register {
            manifest: PreemDemo::manifest(),
        };
        let back: PluginMsg = decode(&encode(&reg)).expect("register frame decodes");
        assert_eq!(reg, back);

        let mut m = fresh();
        let _ = m.update(snapshot("2026-07-16T15:49:00+02:00", 42));
        let view = m.view();
        let render = PluginMsg::Render {
            tree: view.tree,
            panel: view.panel,
            effects: Vec::new(),
        };
        let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
        assert_eq!(render, back);
    }
}
