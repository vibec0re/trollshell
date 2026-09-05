//! `hytte-plugin-preem-demo` — the showcase plugin for the SDK's **preem
//! display widgets** (issues #356 and #884; `hytte_plugin::display`).
//!
//! One sidebar card cycling every widget through every skin: a seven-segment
//! **HH:MM clock**, a **dot-matrix ticker** stepping one char per second, a
//! **scrolling marquee**, and an **8bit textbox**, all rotating VFD → LCD →
//! OLED → CRT every [`STYLE_SECS`] seconds — the last of those being the kit's
//! one *pass* rather than a skin, so it puts scanlines and curved-glass falloff
//! over every widget on the card at once (#397). Below them a real
//! **oscilloscope** ([`Scope`], #556/#397) sweeps the 16-band `AudioSpectrum`
//! push as a glow-trace waveform over a graticule, with real phosphor decay — a
//! silent sink flatlines on the axis while the old trail keeps ghosting. Below
//! *that* a **needle gauge** ([`Gauge`], #397) steps between readings and swings
//! to each one on a real damped oscillator: the pointer overshoots, bounces
//! once, and settles, smearing while it moves. Below *that*, two **`HH:MM:SS`
//! boards** ([`FlipBoard`], #397) showing the same clock through the kit's two
//! change mechanisms: a **split-flap** board whose cards hinge down and ripple
//! left to right, and a **nixie** readout whose outgoing cathode fades under the
//! incoming one's strike. Tapping the clock advances the skin immediately.
//!
//! It doubles as the visual regression harness and the copy-from reference for
//! plugin authors — which since #884 means it is the reference for **one code
//! path, two hosts**, not just for the raster kit.
//!
//! # The #884 migration: what this card does and does not know
//!
//! Every widget here comes from [`hytte_plugin::display`] rather than the raw
//! kit, so the card ships **typed state nodes** to a shell that speaks the preem
//! vocabulary and **CPU-rasterised pixels** to one that doesn't — with no branch
//! anywhere below. There is exactly one rule to see in the code:
//!
//! - **State setters** ([`Gauge::set_target`], [`FlipBoard::set_text`],
//!   [`Scope::push`]) and the `node(…)` calls in `view` are unconditional.
//! - **`advance(dt)`** is unconditional *too* — the SDK makes it a no-op once
//!   the host speaks preem, because the shell owns the needle spring, the
//!   phosphor, the flip clocks and the scroll offset there.
//!
//! So against a preem-speaking shell this card allocates no framebuffers and
//! runs no physics at all, and its animation is smooth at the shell's frame rate
//! instead of stepping at the host's ~1 Hz heartbeat. Against an older shell it
//! behaves exactly as it did before.
//!
//! # The stateful widgets, and why they are slow-motion in raster mode
//!
//! The scope carries a phosphor buffer, the gauge carries a deflection and a
//! velocity, and the two boards carry per-cell clocks — so they live in the
//! model and are advanced on the host heartbeat.
//!
//! They are advanced by a *fraction* of the elapsed second
//! ([`GAUGE_SLOWMO_DT`], [`FLIP_SLOWMO_DT`]) rather than the whole of it. That
//! is a **raster-mode concession**: this card has no frame timer, only the
//! shell's ~1 Hz `Clock` snapshot, so advancing by the real second would land
//! every animation between two renders and the mechanism would never be seen.
//! Those constants therefore have no effect at all once the host speaks preem —
//! the shell's own pump animates at frame rate — which is exactly the point of
//! the seam.
//!
//! The **marquee is the exception, and the instructive one**: its offset is not
//! integrated at all but *stated* from the snapshot's unix time
//! ([`MARQUEE_STEP_DOTS`], via [`Marquee::set_scroll_dots`]), so the raster
//! scroll keeps the wall-clock phase this card has always had. Integrating it
//! looked equivalent — the rate matches exactly — but made the pan
//! session-relative: every reconnect restarting at column 0, and a dropped
//! heartbeat losing ground the old code caught up. Where a widget's state is a
//! pure function of the clock, say it; only genuine physics needs a tick.
//!
//! # …and the scope alone is the card's **parked** widget (#422)
//!
//! Being stateful is also the only reason this card subscribes
//! [`SlotVisible`](Input::SlotVisible): the scope is the one widget that parks.
//! While the sidebar is closed no spectrum push arrives (the shell drops the
//! `PipeWire` tap once nothing on-screen wants it, #565/#583), yet the 1 Hz
//! heartbeat keeps coming: left ungated, every tick would re-stamp the *same*
//! frozen bands, saturating the phosphor into a solid constant waveform that
//! then needs several sweeps to decay once the card is looked at again. So the
//! heartbeat's sweep is gated on visibility and the hide edge
//! [`clear`](Scope::clear)s the phosphor.
//!
//! Worth noting for #884: in **state** mode the park is redundant. Re-stating
//! frozen bands emits an identical state node, which the runtime dedups, so no
//! frame reaches the shell and nothing saturates; the shell's own decay fades
//! the trace out honestly. The gate is kept because it is still load-bearing for
//! the raster path, and because a plugin should not have to reason about which
//! host it is talking to — which is the whole promise.
//!
//! Everything else deliberately keeps running while hidden: the clock, ticker,
//! marquee, boards, skin rotation **and the gauge** are pure functions of the
//! snapshot, so parking them would buy nothing and would make the reopened card
//! show a *stale time* — the very failure the scope's park exists to avoid. The
//! gauge is the instructive case: it is stateful like the scope, but its input
//! is derived rather than pushed, so it is the *input*, not the statefulness,
//! that decides whether a widget must park.
//!
//! # Shape — The Elm Architecture, purely host-driven
//!
//! Like `hytte-plugin-clock-demo`, everything below is the pure TEA core. There
//! are **no sources and no timers**: the shell's `Clock` subscription
//! re-snapshots every second, and both the ticker step and the style rotation
//! are pure functions of the snapshot's unix time (plus a click offset) — so
//! `view` stays deterministic and unit-testable, and the runtime's render dedup
//! sees a fresh tree each second without the plugin owning any cadence.
//!
//! # Sizing — the #313 lesson
//!
//! Every widget is sized via its **buffer dimensions** (which in state mode
//! become the config the shell sizes from), not shell CSS: the 7seg clock
//! renders 188 px wide, the 11-char ticker 268 px, the marquee window
//! [`MARQUEE_WINDOW_PX`] wide, the 22-column ×2 textbox 274 px, the scope's and
//! gauge's defaults 288 px each, and the two 8-cell boards 260 px — all inside
//! the sidebar card's ~296 px content width.

use hytte_plugin::display::{
    AccentRole, DotMatrix, FlipBoard, Gauge, Marquee, Mechanism, Scope, SevenSeg, StyleName,
    TextBox,
};
use hytte_plugin::proto::{Dir, Effect, EventKind, Manifest, Mount, Node, SPECTRUM_BINS, StateKey};
use hytte_plugin::{CmdSender, Input, Plugin, View};

/// Stable plugin id — the host's mount-slot ownership key.
const PLUGIN_ID: &str = "preem-demo";
/// Node ids. The clock button is the click target (cycle the skin); the widget
/// ids keep the host reconciler on the *same* renderer instance across renders,
/// which is what preserves the phosphor, the needle's momentum and the flip
/// clocks in state mode (#882's `preem_id` rule) — and swaps the texture in
/// place in raster mode.
const ROOT_ID: &str = "preem-demo-root";
const CYCLE_BTN: &str = "preem-demo-cycle";
const SEG_ID: &str = "preem-demo-7seg";
const TICKER_ID: &str = "preem-demo-ticker";
const MARQUEE_ID: &str = "preem-demo-marquee";
const TEXT_ID: &str = "preem-demo-textbox";
const SCOPE_ID: &str = "preem-demo-scope";
const GAUGE_ID: &str = "preem-demo-gauge";
const FLAP_ID: &str = "preem-demo-flap";
const NIXIE_ID: &str = "preem-demo-nixie";
const ROLE_ID: &str = "preem-demo-role";
const PIN_ID: &str = "preem-demo-pin";

/// The pinned widget's ink (#885) — a color no theme resolves to, so "did it
/// re-tint?" is answerable on glass at a glance rather than by pixel-peeping.
///
/// Deliberately loud: the whole point of the pair below is that changing the
/// desktop accent moves one of them and not the other.
const PIN_INK: [u8; 4] = [0xff, 0x2d, 0x95, 0xff];

/// What the two ink probes read. Four cells each, so the pair fits the ~296 px
/// card side by side.
const ROLE_TEXT: &str = "ROLE";
const PIN_TEXT: &str = "PIN.";

/// Seconds each skin holds before the rotation advances.
const STYLE_SECS: i64 = 10;

/// The `HH:MM` placeholder the readout wears until the first snapshot lands.
const NO_CLOCK: &str = "--:--";
/// Cells on each [`FlipBoard`]: `HH:MM:SS`, 260 px wide at the kit's defaults.
const CLOCK_CELLS: u32 = 8;
/// Seconds of board time each [`FlipBoard`] is advanced per host heartbeat.
///
/// **Slow motion, on purpose**, and **raster-only**: the card has no frame timer
/// of its own, so advancing by the real elapsed second would land every card
/// between two renders and the mechanism would never be seen. At this rate a
/// fold spans three or four heartbeats. Once the host speaks preem this constant
/// stops mattering — `advance` is a no-op and the shell's pump runs the fold at
/// frame rate.
const FLIP_SLOWMO_DT: f32 = 0.12;

/// The readings the showcase gauge steps between, one every
/// [`GAUGE_HOLD_SECS`]. Deliberately mid-scale: a full-scale slam would peg the
/// needle against the dial's stop and hide the overshoot the widget exists for.
const GAUGE_STOPS: [f32; 6] = [10.0, 72.0, 38.0, 90.0, 22.0, 55.0];
/// Host heartbeats each reading holds before the next one is dialed in — long
/// enough that the needle has visibly settled before the next step.
const GAUGE_HOLD_SECS: i64 = 8;
/// Seconds of needle motion the gauge is advanced per host heartbeat.
///
/// **Slow motion, on purpose**, and **raster-only** — same reasoning as
/// [`FLIP_SLOWMO_DT`]. At this rate the eight heartbeats of a hold window cover
/// the whole 1.3 s response, so successive renders actually show the kick, the
/// overshoot, the bounce and the settle.
const GAUGE_SLOWMO_DT: f32 = 0.16;
/// The ticker's marquee message; the view shows a [`TICKER_WINDOW`]-char
/// window that advances one char per second (wrapping around).
const TICKER: &str = "PREEM RASTER KIT ~ VFD / LCD / OLED / CRT ~ 7SEG DOT 8BIT ~ ";
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
const MARQUEE_WINDOW_PX: u32 = 268;
/// **Virtual pixels** (dots) the marquee pans per wall-clock second in raster
/// mode — the unit the marquee scrolls in, where a sub-dot step is not
/// expressible (#839).
///
/// Unlike the gauge's and the boards' slow-motion constants, this is **not** an
/// integration step: the offset is a pure function of the snapshot's unix time
/// ([`PreemDemo::marquee_offset`]), stated through
/// [`Marquee::set_scroll_dots`], exactly as this card computed it before #884.
/// Integrating instead would have made the scroll session-relative — every
/// reconnect restarting at column 0, and a missed heartbeat slowing the pan
/// rather than being caught up — a visible change against an old shell, which
/// this PR promises not to make (#898 review R4).
const MARQUEE_STEP_DOTS: usize = 2;
/// The marquee's scroll speed in **dots per second**, as stated on the wire.
///
/// State mode only: the shell integrates it against its own frame clock. The
/// raster arm never reads it — [`MARQUEE_STEP_DOTS`] drives that, off the host
/// clock — so the card scrolls at 2 dots/s against an old shell (all a 1 Hz
/// cadence can express) and at this rate, smoothly, against a preem-speaking
/// one. 20 dots/s is the kit's own default and the rate the audio widget
/// already scrolls at.
const MARQUEE_SPEED_DPS: f32 = 20.0;
/// The textbox wrap width: 22 columns at ×2 scale = 274 px.
const TEXT_COLS: u32 = 22;

/// The demo's entire state — rebuilt on every (re)connect, re-derived from
/// the next snapshot.
// `Eq` is intentionally not derived: `bins` is `[f32; N]`, which is `PartialEq`
// but not `Eq`. Nothing compares the whole model for equality anyway.
//
// `PartialEq` is derived, as it was before #884 — the three wrappers that had
// dropped it now carry it again (#898 review R3), which is the property this
// model exists to demonstrate: swapping the raw kit for `display` must not cost
// a plugin its derives.
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
    /// The seven-segment `HH:MM` clock — pure, so its text is a `view` argument.
    seg: SevenSeg,
    /// The dot-matrix ticker — pure, likewise.
    ticker: DotMatrix,
    /// The scrolling marquee. Its **offset** is stateful (shell-owned in state
    /// mode, plugin-owned in raster mode), so the widget lives in the model even
    /// though its text is a `view` argument.
    marquee: Marquee,
    /// The 8bit textbox — pure.
    textbox: TextBox,
    /// The oscilloscope (#556): stateful, carrying a phosphor buffer in raster
    /// mode and a sample batch on the wire in state mode. Swept once per host
    /// heartbeat with [`Self::bins`] **while the card is on-screen**.
    scope: Scope,
    /// The needle gauge (#397): the card's other stateful widget, and the one
    /// that carries *physics* rather than a pixel buffer. Its target steps
    /// through [`GAUGE_STOPS`] on the heartbeat and the needle swings to it.
    ///
    /// Unlike the scope this deliberately does **not** park (#422): its input is
    /// a pure function of the snapshot, exactly like the clock and the ticker,
    /// so keeping it live means a reopened card reads the current value instead
    /// of replaying a stale swing.
    gauge: Gauge,
    /// The split-flap board (#397): the same `HH:MM:SS` the 7seg shows, on the
    /// airport-board mechanism. Stateful like the gauge, and like it deliberately
    /// **not** parked — its content is a pure function of the snapshot.
    flap: FlipBoard,
    /// The nixie readout (#397): the same clock again, so the card contrasts
    /// the two change mechanisms side by side on identical content.
    nixie: FlipBoard,
    /// The **role-tinted** half of the #885 ink pair: a dot matrix asking for
    /// [`AccentRole::Success`], so the shell resolves it against the live theme
    /// and it re-tints the moment the desktop accent or color scheme moves — no
    /// restart, no frame on the wire.
    role: DotMatrix,
    /// The **pinned** half: the same widget with an explicit
    /// [`PIN_INK`](crate::PIN_INK), which is deliberately excluded from that
    /// re-tint. Side by side with `role` these two *are* the demo of #885: change
    /// the accent with the card open and exactly one of them moves.
    pin: DotMatrix,
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
    /// [`StyleName::ALL`] every [`STYLE_SECS`], and each click on the clock
    /// advances the rotation by one.
    fn style(&self) -> StyleName {
        let slot = self.unix.div_euclid(STYLE_SECS) + i64::from(self.style_bump);
        let n = i64::try_from(StyleName::ALL.len()).unwrap_or(1);
        let idx = usize::try_from(slot.rem_euclid(n)).unwrap_or(0);
        StyleName::ALL[idx]
    }

    /// Point every widget at `style`.
    ///
    /// On the wire this is a **config** change, so a preem-speaking shell
    /// rebuilds each renderer instance — which resets the animation it owns.
    /// That is the vocabulary's rule ("a plugin that jitters a config field
    /// kills its own animation") and this card breaks it deliberately, once
    /// every [`STYLE_SECS`], because showing every widget in every skin is the
    /// whole job. A real plugin picks a skin and leaves it alone.
    fn restyle(&mut self, style: StyleName) {
        self.seg.style(style);
        self.ticker.style(style);
        self.marquee.style(style);
        self.textbox.style(style);
        self.scope.style(style);
        self.gauge.style(style);
        self.flap.style(style);
        self.nixie.style(style);
        self.role.style(style);
        self.pin.style(style);
    }

    /// The ticker's visible window, advanced one char per second.
    fn ticker_window(&self) -> String {
        let chars: Vec<char> = TICKER.chars().collect();
        let len = i64::try_from(chars.len()).unwrap_or(1).max(1);
        let off = usize::try_from(self.unix.rem_euclid(len)).unwrap_or(0);
        chars.iter().cycle().skip(off).take(TICKER_WINDOW).collect()
    }

    /// The marquee's scroll offset, panning [`MARQUEE_STEP_DOTS`] dots per
    /// second of wall clock — a pure function of the snapshot, like the ticker
    /// and the skin rotation.
    ///
    /// Byte-for-byte the pre-#884 projection, restored in the #898 review
    /// round: `MarqueeStrip::window` wraps it modulo the strip period, so the
    /// raw (unbounded) counter is fine, and the bound below only keeps the
    /// multiply from overflowing.
    fn marquee_offset(&self) -> usize {
        let secs = usize::try_from(self.unix.rem_euclid(1_000_000)).unwrap_or(0);
        secs.saturating_mul(MARQUEE_STEP_DOTS)
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

    /// The face both [`FlipBoard`]s show: `HH:MM:SS`, its seconds taken from
    /// the snapshot's unix time so a card changes on every heartbeat — which is
    /// what puts the retarget rule on display. All dashes until the first
    /// snapshot lands, matching the 7seg readout's placeholder.
    fn clock_face(&self) -> String {
        if self.hhmm == NO_CLOCK {
            return "--:--:--".to_owned();
        }
        format!("{}:{:02}", self.hhmm, self.unix.rem_euclid(60))
    }

    /// The textbox's line — names the current skin so the rotation is
    /// legible in all three widgets at once.
    fn textbox_line(style: StyleName) -> String {
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
    ///
    /// The #884 vocabulary negotiation needs no declaration here:
    /// `Manifest::new` stamps the `vocab`/`vocab_max` pair for every plugin.
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
        let style = StyleName::ALL[0];
        Self {
            hhmm: NO_CLOCK.to_owned(),
            unix: 0,
            style_bump: 0,
            bins: [0.0; SPECTRUM_BINS],
            seg: SevenSeg::new(style),
            ticker: DotMatrix::new(style),
            marquee: Marquee::new(style)
                .window_px(MARQUEE_WINDOW_PX)
                .speed_dots_per_sec(MARQUEE_SPEED_DPS),
            textbox: TextBox::new(style).cols(TEXT_COLS).scale(2),
            scope: Scope::new(style),
            gauge: Gauge::new(style).range(0.0, 100.0),
            flap: FlipBoard::new(style, Mechanism::SplitFlap).cells(CLOCK_CELLS),
            nixie: FlipBoard::new(style, Mechanism::Nixie).cells(CLOCK_CELLS),
            role: DotMatrix::new(style).accent_role(AccentRole::Success),
            pin: DotMatrix::new(style).ink(PIN_INK),
            visible: false,
        }
    }

    /// Fold one input into the model. Pure and panic-free over any host-sent
    /// value; re-rendering is the runtime's problem.
    ///
    /// Note that nothing here branches on which host is on the other end. Every
    /// `set_*`/`push` states what the widget should show; every `advance` is the
    /// raster-mode tick the SDK drops once the shell owns the animation.
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
                let style = self.style();
                self.restyle(style);
                // The heartbeat is the scope's sweep tick: state the latest
                // bands (silence flatlines on the axis while the old trail
                // decays — the honest ghost). Gated on visibility (#422); see
                // the crate docs on why that gate is raster-only in effect.
                if self.visible {
                    let bins = self.bins;
                    self.scope.push(&bins);
                }
                // The gauge's own heartbeat: dial in the current reading and let
                // the needle move toward it. Ungated on visibility, unlike the
                // scope — see the field docs.
                self.gauge.set_target(self.gauge_target());
                self.gauge.advance(GAUGE_SLOWMO_DT);
                // The boards' heartbeat: re-state the clock (unchanged cards
                // are untouched, so only the seconds card usually moves) and
                // advance the mechanism. Ungated for the gauge's reason.
                let face = self.clock_face();
                self.flap.set_text(&face);
                self.flap.advance(FLIP_SLOWMO_DT);
                self.nixie.set_text(&face);
                self.nixie.advance(FLIP_SLOWMO_DT);
                // …and the marquee's: state where the strip has panned to.
                // Absolute, not an increment, so the phase tracks the wall
                // clock and a clockless snapshot (or a missed heartbeat) leaves
                // it exactly where the time says it should be.
                self.marquee.set_scroll_dots(self.marquee_offset());
            }
            // Tapping the clock advances the skin rotation by one.
            Input::Event { node, kind } => {
                if node == CYCLE_BTN && matches!(kind, EventKind::Click) {
                    self.style_bump = self.style_bump.wrapping_add(1);
                    let style = self.style();
                    self.restyle(style);
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
    /// marquee, the textbox — all wearing the same skin — the ink pair, the audio
    /// oscilloscope, the needle gauge, the split-flap and nixie boards, and a
    /// dim hint line.
    ///
    /// Every `node(…)` here is one call that lands as a typed `Node::Preem` or a
    /// rasterised `Node::Pixels` depending on what the host advertised; a
    /// `Pixels` buffer satisfies the host's `len == w * h * 4` invariant by kit
    /// construction either way.
    fn view(&self) -> View {
        let style = self.style();
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
                    child: Box::new(self.seg.node(SEG_ID, &self.hhmm)),
                },
                self.ticker.node(TICKER_ID, &self.ticker_window()),
                self.marquee.node(MARQUEE_ID, MARQUEE_MSG),
                self.textbox.node(TEXT_ID, &Self::textbox_line(style)),
                self.scope.node(SCOPE_ID),
                self.gauge.node(GAUGE_ID),
                self.flap.node(FLAP_ID),
                self.nixie.node(NIXIE_ID),
                // #885, on glass: same widget, same skin, two ink sources. The
                // left one names a semantic role and follows the theme; the
                // right one pins `PIN_INK` and does not. Change the desktop
                // accent with this card open and exactly one of them moves.
                Node::Box {
                    id: None,
                    dir: Dir::Horizontal,
                    spacing: 8,
                    scroll: false,
                    classes: Vec::new(),
                    children: vec![
                        self.role.node(ROLE_ID, ROLE_TEXT),
                        self.pin.node(PIN_ID, PIN_TEXT),
                    ],
                },
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
    use hytte_plugin::display::testing::with_render_mode;
    use hytte_plugin::display::{Marquee, RenderMode, StyleName, display_style};
    use hytte_plugin::proto::preem::PreemWidget;
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

    /// Walk a tree and collect every `Preem` node's `(id, widget kind)`.
    fn preem_of(node: &Node) -> Vec<(String, &'static str)> {
        let mut out = Vec::new();
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            match n {
                Node::Preem { id, widget, .. } => {
                    out.push((id.clone().unwrap_or_default(), widget.kind()));
                }
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
        // One slot per style = a full lap through StyleName::ALL. Off the
        // list's own length, so adding a skin (#397's CRT was the fourth) can
        // never leave this asserting a stale lap.
        let lap = m.style();
        m.unix += STYLE_SECS * i64::try_from(StyleName::ALL.len()).unwrap();
        assert_eq!(m.style(), lap);
    }

    /// The rotation reaches **every** skin, the CRT pass included (#397): a
    /// full lap of slots visits each of `StyleName::ALL` exactly once, so
    /// there is no per-skin wiring in the demo that could quietly skip one.
    #[test]
    fn the_rotation_visits_every_skin_including_the_crt() {
        let mut m = fresh();
        let seen: Vec<StyleName> = (0..StyleName::ALL.len())
            .map(|_| {
                let style = m.style();
                m.unix += STYLE_SECS;
                style
            })
            .collect();
        for style in StyleName::ALL {
            assert!(seen.contains(&style), "the rotation never shows {style:?}");
        }
        assert!(
            seen.contains(&StyleName::Crt),
            "the CRT pass is on the card"
        );
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
        assert!(m.view() == before, "the tree is unchanged");
    }

    /// Against a host that never advertises the preem vocabulary — the default
    /// for these tests, and every shell before #883 — the card is exactly what
    /// it was: one rasterised `Pixels` buffer per widget, each honoring the
    /// host's `len == w * h * 4` invariant and the ~296 px sidebar content width
    /// (the #313 lesson), across all skins and clock states.
    #[test]
    fn against_an_old_shell_the_card_is_still_pixel_buffers() {
        let mut m = fresh();
        for step in 0..6 {
            let tree = m.view().tree;
            let bufs = pixels_of(&tree);
            assert_eq!(
                bufs.len(),
                10,
                "clock + ticker + marquee + textbox + scope + gauge + flap + nixie + the ink pair"
            );
            assert!(
                preem_of(&tree).is_empty(),
                "no advertisement, no state nodes"
            );
            for (w, h, len) in bufs {
                assert_eq!(len, (w as usize) * (h as usize) * 4);
                assert!(w > 0 && h > 0);
                assert!(w <= 296, "width {w} fits the sidebar card");
            }
            let _ = m.update(snapshot("2026-07-16T23:59:00+02:00", step * STYLE_SECS));
        }
    }

    /// …and against a shell that does advertise it, the *same* card is typed
    /// state nodes instead — same ids, same order, no rasterisation. This is the
    /// #884 acceptance showcase: one `view()`, two wire shapes. The #885 ink pair
    /// rides it unchanged: a pinned ink is a *state* node like any other, since
    /// what travels is still a style reference and not a rendered pixel.
    ///
    /// Drives the SDK's recorded generation directly rather than through a
    /// session; the session-level negotiation is `hytte-plugin`'s own test.
    #[test]
    fn against_a_preem_shell_the_same_card_is_state_nodes() {
        let mut m = fresh();
        let _ = m.update(snapshot("2026-07-16T15:49:00+02:00", 1_752_672_540));

        let tree = with_render_mode(RenderMode::State, || m.view().tree);

        assert!(
            pixels_of(&tree).is_empty(),
            "a preem-speaking shell gets no rasterised buffers at all"
        );
        assert_eq!(
            preem_of(&tree),
            vec![
                // `pixels_of`/`preem_of` walk depth-first off a stack, so the
                // card's children come back reversed — the assertion is on the
                // set and the ids, not on paint order. The ink pair is the last
                // row and so comes back first, its own two children reversed
                // with it.
                ("preem-demo-pin".to_owned(), "dot-matrix"),
                ("preem-demo-role".to_owned(), "dot-matrix"),
                ("preem-demo-nixie".to_owned(), "flip-board"),
                ("preem-demo-flap".to_owned(), "flip-board"),
                ("preem-demo-gauge".to_owned(), "gauge"),
                ("preem-demo-scope".to_owned(), "scope"),
                ("preem-demo-textbox".to_owned(), "text-box"),
                ("preem-demo-marquee".to_owned(), "marquee"),
                ("preem-demo-ticker".to_owned(), "dot-matrix"),
                ("preem-demo-7seg".to_owned(), "seven-seg"),
            ],
            "every widget on the card has a typed node with a stable id",
        );
    }

    /// The state nodes carry the readings the card actually derived — a tree of
    /// eight defaults would satisfy the shape assertion above while showing
    /// nothing.
    #[test]
    fn the_state_nodes_carry_the_card_s_own_readings() {
        let mut m = fresh();
        let _ = m.update(snapshot("2026-07-16T15:49:00+02:00", 1_752_672_540));
        let face = m.clock_face();
        let target = m.gauge_target();

        let tree = with_render_mode(RenderMode::State, || m.view().tree);

        let mut seen_seg = false;
        let mut seen_gauge = false;
        let mut stack = vec![&tree];
        while let Some(n) = stack.pop() {
            match n {
                Node::Preem { widget, .. } => match widget.as_ref() {
                    PreemWidget::SevenSeg { state, .. } => {
                        assert_eq!(state.text, "15:49", "the clock's own reading");
                        seen_seg = true;
                    }
                    PreemWidget::Gauge { state, config } => {
                        assert!((state.target - target).abs() < f32::EPSILON);
                        assert!((config.range.high - 100.0).abs() < f32::EPSILON);
                        seen_gauge = true;
                    }
                    PreemWidget::FlipBoard { state, config } => {
                        assert_eq!(state.text, face, "both boards show the clock face");
                        assert_eq!(config.cells, super::CLOCK_CELLS);
                    }
                    _ => {}
                },
                Node::Box { children, .. } => stack.extend(children.iter()),
                Node::Button { child, .. } => stack.push(child),
                _ => {}
            }
        }
        assert!(seen_seg && seen_gauge, "both widgets were on the card");
    }

    /// The view is a pure function of the model, and each skin renders a
    /// visibly different card.
    #[test]
    fn view_is_deterministic_and_skins_differ() {
        let mut m = fresh();
        let _ = m.update(snapshot("2026-07-16T12:00:00+02:00", 100));
        assert!(m.view() == m.view(), "view is pure");
        let a = m.style();
        let before = m.view();
        let _ = m.update(Input::Event {
            node: CYCLE_BTN.to_owned(),
            kind: EventKind::Click,
        });
        assert_ne!(m.style(), a);
        assert!(m.view() != before, "a new skin renders a new card");
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

    /// The marquee pans forward with the host clock in raster mode, at the same
    /// 2 dots per second it panned at before #884, on the same **wall-clock
    /// phase** — the offset is a pure function of `unix`, not a session-relative
    /// accumulation (#898 review R4) — and the panned pixels actually move.
    #[test]
    fn marquee_pans_with_the_clock() {
        let mut m = fresh();
        let _ = m.update(snapshot("2026-07-16T00:00:03+02:00", 300));
        let o0 = m.marquee.scroll_dots();
        let _ = m.update(snapshot("2026-07-16T00:00:04+02:00", 301));
        let o1 = m.marquee.scroll_dots();
        assert_eq!(o1 - o0, 2, "one heartbeat, two dots — the pre-#884 pace");

        // The phase, not just the rate: a *fresh* model handed the same
        // snapshot lands on the same column, which is what a reconnect gets and
        // what an integrating accumulator could not deliver.
        assert_eq!(o1, 301 * super::MARQUEE_STEP_DOTS, "the pre-#884 offset");
        let mut reconnected = fresh();
        let _ = reconnected.update(snapshot("2026-07-16T00:00:04+02:00", 301));
        assert_eq!(
            reconnected.marquee.scroll_dots(),
            o1,
            "a reconnect resumes the scroll where the clock says, not at 0",
        );

        // …and a snapshot the host sent without a clock does not pan it (the
        // offset is stated from `unix`, which did not move).
        let _ = m.update(Input::Snapshot(StateSnapshot::default()));
        assert_eq!(m.marquee.scroll_dots(), o1, "a clockless snapshot is inert");

        let strip = hytte_plugin::preem::Marquee::new(display_style(m.style()))
            .window_px(usize::try_from(super::MARQUEE_WINDOW_PX).unwrap())
            .render(super::MARQUEE_MSG);
        assert!(strip.scrolls(), "the demo message overflows the window");
        assert!(
            strip.window(o0) != strip.window(o1),
            "the scroll moves pixels"
        );
    }

    /// …and in state mode the plugin does not pan at all: the shell owns the
    /// offset, so the card's marquee node is identical across heartbeats even
    /// though it is visibly scrolling on screen.
    #[test]
    fn the_marquee_stops_panning_once_the_shell_owns_it() {
        let mut quiet = Marquee::new(StyleName::Vfd).window_px(super::MARQUEE_WINDOW_PX);
        let before = quiet.node_in(RenderMode::State, "mq", Vec::new(), super::MARQUEE_MSG);
        for beat in 0..50 {
            // Both ways a plugin can move a marquee, and neither reaches the
            // plugin-side offset while the shell owns it.
            quiet.advance_in(RenderMode::State, 1.0);
            quiet.set_scroll_dots_in(RenderMode::State, beat * super::MARQUEE_STEP_DOTS);
        }
        assert_eq!(quiet.scroll_dots(), 0, "the plugin never ticked");
        assert_eq!(
            quiet.node_in(RenderMode::State, "mq", Vec::new(), super::MARQUEE_MSG),
            before,
            "so fifty heartbeats put nothing on the wire",
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
        for style in StyleName::ALL {
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
        assert!(
            quiet.render(DisplayStyle::Vfd) != loud.render(DisplayStyle::Vfd),
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
    /// black or freezing (the #556 phosphor showcase). Compares the scope's own
    /// node so the marquee's scroll doesn't confound it.
    #[test]
    fn scope_sweeps_on_the_heartbeat_with_a_decay_ghost() {
        let mut m = shown();
        let trace = |m: &PreemDemo| m.scope.node_in(RenderMode::Raster, "sc", Vec::new());
        // A loud band, then the heartbeat draws it.
        let mut loud = [0.0_f32; super::SPECTRUM_BINS];
        loud[2] = 1.0;
        let _ = m.update(spectrum(loud));
        let _ = m.update(snapshot("2026-07-16T00:00:00+02:00", 1));
        let lit = trace(&m);
        // Silence: the heartbeat keeps sweeping and the loud trail decays.
        let _ = m.update(spectrum([0.0; super::SPECTRUM_BINS]));
        let _ = m.update(snapshot("2026-07-16T00:00:01+02:00", 2));
        let ghost = trace(&m);
        assert!(ghost != lit, "silence decays the loud trace to a ghost");
        let _ = m.update(snapshot("2026-07-16T00:00:02+02:00", 3));
        assert!(trace(&m) != ghost, "the ghost keeps fading each heartbeat");
    }

    /// The gauge steps between readings on the host clock, and the needle
    /// **overshoots** on the way — the #397 showcase, checked here at the card
    /// level rather than trusted. (Raster mode: in state mode the shell runs
    /// this same spring off the emitted target.)
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

    /// The boards track the clock face and **animate** getting there: a
    /// heartbeat that changes the seconds card leaves the board mid-transition
    /// rather than snapped onto the new time (the `FLIP_SLOWMO_DT` showcase),
    /// and the unchanged cards are not disturbed.
    #[test]
    fn the_boards_step_the_clock_and_animate_getting_there() {
        let mut m = fresh();
        assert_eq!(m.clock_face(), "--:--:--", "dashes before the first clock");

        let _ = m.update(snapshot("2026-07-16T15:49:00+02:00", 1_752_672_540));
        assert_eq!(m.clock_face(), "15:49:00");
        // The whole face was new, so both boards are still on their way.
        assert!(!m.flap.is_settled() && !m.nixie.is_settled());
        assert_eq!(m.flap.target(), "15:49:00", "aimed at the current time");
        assert_eq!(m.nixie.target(), "15:49:00");

        // Hold the same face for a while: re-stating it is inert, so the boards
        // simply run their clocks out and land.
        for _ in 0..12 {
            let _ = m.update(snapshot("2026-07-16T15:49:00+02:00", 1_752_672_540));
        }
        assert!(m.flap.is_settled(), "a held face lands");
        let landed = m.view();
        let _ = m.update(snapshot("2026-07-16T15:49:01+02:00", 1_752_672_541));
        assert_eq!(m.flap.target(), "15:49:01");
        assert!(!m.flap.is_settled(), "and the next second is mid-flip");
        assert!(m.view() != landed, "which the card actually shows");
    }

    /// The boards deliberately do **not** park (#422), for the gauge's reason:
    /// their content is a pure function of the snapshot, so a hidden card keeps
    /// them current and a reopen shows the time rather than a stale flip.
    #[test]
    fn a_hidden_card_keeps_the_boards_current() {
        let mut m = shown();
        let _ = m.update(Input::SlotVisible(false));
        for second in 0..30 {
            let _ = m.update(snapshot(
                "2026-07-16T15:49:00+02:00",
                1_752_672_540 + second,
            ));
        }
        assert_eq!(
            m.flap.target(),
            m.clock_face(),
            "a hidden split-flap board still tracks the clock"
        );
        assert_eq!(m.nixie.target(), m.clock_face());
    }

    /// Every char the boards can be asked to show is on the kit's drum, so the
    /// demo never renders a notdef card — dashes included.
    #[test]
    fn the_clock_face_stays_on_the_drum() {
        use hytte_plugin::preem::CHARSET;
        let mut m = fresh();
        let mut faces = vec![m.clock_face()];
        for second in [0_i64, 7, 59, 1_752_672_540] {
            let _ = m.update(snapshot("2026-07-16T15:49:00+02:00", second));
            faces.push(m.clock_face());
        }
        for face in faces {
            assert_eq!(
                face.chars().count(),
                usize::try_from(super::CLOCK_CELLS).unwrap(),
                "{face:?}"
            );
            for c in face.chars() {
                assert!(CHARSET.contains(c), "clock-face char {c:?} is on the drum");
            }
        }
    }

    /// A hidden card parks the scope (#422): the heartbeat stops sweeping, so a
    /// frozen band set can't saturate the phosphor, and the hide edge wipes what
    /// was already drawn — a reopen starts from a dark screen, not the trace from
    /// whenever the sidebar closed.
    #[test]
    fn a_hidden_card_parks_the_scope() {
        let trace = |m: &PreemDemo| m.scope.node_in(RenderMode::Raster, "sc", Vec::new());
        let dark = trace(&fresh());

        let mut m = shown();
        let mut loud = [0.0_f32; super::SPECTRUM_BINS];
        loud[2] = 1.0;
        let _ = m.update(spectrum(loud));
        let _ = m.update(snapshot("2026-07-16T00:00:00+02:00", 1));
        assert!(trace(&m) != dark, "a trace is drawn");

        // Hide: the phosphor is wiped and the bands forgotten.
        let _ = m.update(Input::SlotVisible(false));
        assert!(trace(&m) == dark, "hiding wipes the trace");
        assert!(m.bins.iter().all(|&b| b <= 0.0), "and forgets the bands");

        // Hidden: heartbeats no longer sweep, and a late push (another
        // subscriber may hold the tap up) doesn't re-arm the bands.
        let _ = m.update(spectrum(loud));
        for s in 2..8 {
            let _ = m.update(snapshot("2026-07-16T00:00:00+02:00", s));
        }
        assert!(
            trace(&m) == dark,
            "a parked scope stays dark across heartbeats"
        );

        // Reopen: still dark until real data sweeps again.
        let _ = m.update(Input::SlotVisible(true));
        assert!(
            trace(&m) == dark,
            "the reopened card starts from a dark screen"
        );
        let _ = m.update(spectrum(loud));
        let _ = m.update(snapshot("2026-07-16T00:00:09+02:00", 9));
        assert!(trace(&m) != dark, "and re-derives from the next sweep");
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
        assert!(
            m.negotiates_vocab(),
            "and `Manifest::new` stamps the #882 negotiation pair for free"
        );
    }

    /// The frames built from this plugin's data are valid on the wire — in
    /// **both** shapes, since the whole point is that the same `view()` produces
    /// either.
    #[test]
    fn register_and_render_frames_round_trip_in_both_modes() {
        let reg = PluginMsg::Register {
            manifest: PreemDemo::manifest(),
        };
        let back: PluginMsg = decode(&encode(&reg)).expect("register frame decodes");
        assert_eq!(reg, back);

        let mut m = fresh();
        let _ = m.update(snapshot("2026-07-16T15:49:00+02:00", 42));
        for mode in [RenderMode::Raster, RenderMode::State] {
            let view = with_render_mode(mode, || m.view());
            let render = PluginMsg::Render {
                tree: view.tree,
                panel: view.panel,
                effects: Vec::new(),
            };
            let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
            assert!(render == back, "{mode:?} round-trips");
        }
    }
}
