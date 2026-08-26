//! `hytte-plugin-audio-widget` — an audio-reactive trollshell sidebar card 🎚️
//! (issue #506, "preem extra audio widget").
//!
//! An out-of-process widget plugin on the `hytte-plugin` SDK that composes three
//! preem raster widgets off the host's [`StateKey::AudioSpectrum`] push (#405,
//! perceptual-dBFS levels since #504) into one sidebar card:
//!
//! - a **dot-matrix marquee** (a scrolling ticker banner, see the dot-matrix
//!   note below on why it isn't the track title yet),
//! - a **16-band spectrum scope** (`spectrum::scope_tile`, mirroring the
//!   preem-demo's tile), and
//! - the **LED peak/level strip** ([`hytte_plugin::preem::LedStrip`], the kit
//!   widget this issue adds): a row of discrete LEDs lighting with the overall
//!   level, topped by a peak-hold dot that floats and decays.
//!
//! # Shape — TEA, self-driven frame timer
//!
//! The kit owns no clock, so a ~20 Hz [`sources`](Plugin::sources) frame timer
//! drives the animation cadence: each [`Tick`](Msg::Tick) advances the marquee
//! scroll and *releases* the meters (the spectrum bars, the LED bar, and the
//! peak-hold dot all fall by a fixed rate), while each
//! [`AudioSpectrum`](Input::AudioSpectrum) push *attacks* them (raising each to
//! the fresh level). That attack-on-push / release-on-tick envelope gives the
//! meters their VU ballistics and, crucially, makes them settle to rest on their
//! own when the audio stops — the display never freezes mid-bar. The two LED
//! values are both [`PeakHold`]s: a fast-releasing one for the bar level and a
//! slow-releasing one for the dot.
//!
//! # `SlotVisible` gating (#288/#422)
//!
//! A sidebar card, so [`SlotVisible`](Input::SlotVisible) tracks the sidebar
//! opening/closing. The frame timer's work and the spectrum folding are both
//! gated on it: while the sidebar is hidden, ticks and pushes are no-ops, so the
//! view never changes and the runtime's render dedup sends nothing — the card
//! parks while nobody is looking, exactly the pattern the SDK documents for a
//! sidebar-mounted poller.
//!
//! The hide edge additionally **resets the meters to rest** (#422). Gating alone
//! freezes them at whatever was playing when the sidebar closed, and because the
//! release lives on the (also gated) tick, nothing walks them back down while
//! parked — so the card would re-appear showing a loud spectrum from minutes ago
//! and only crawl out of it over the ~1 s release once ticks resume. That stale
//! first frame is exactly what the shell's own tap gating makes *likelier*: the
//! host tears the `PipeWire` capture down while nothing on-screen wants it
//! (#565/#583), so a reopen waits on a node rebuild plus a format renegotiation
//! before the first real push lands. Parking at rest makes the reopened card
//! honest — all-zero is what an unfed meter actually knows — and it re-derives
//! from the next push, exactly as a fresh (re)connect does.
//!
//! # Dot-matrix data source — the live track (#528), with a banner fallback
//!
//! The marquee scrolls the **current track** (`title — artist`) whenever the
//! host reports a *playing* player, off the [`StateKey::NowPlaying`] push (#528):
//! the shell projects its own `hytte_services::mpris` active-player state onto a
//! GTK-free `NowPlaying { title, artist, playing }` the same way #405 projected
//! the spectrum — so this out-of-process plugin never touches the session bus.
//! When nothing is playing (paused / stopped / no player, or no title) it falls
//! back to the pre-#528 decorative banner, flipping between a "vibing" line and a
//! "silence" line off the one always-reachable datum, the peak level. (#529
//! shipped only the banner as a placeholder; #528 is the follow-up it flagged.)

mod spectrum;

use std::cell::RefCell;
use std::time::Duration;

use hytte_plugin::preem::{DisplayStyle, Frame, LedStrip, Marquee, MarqueeStrip, PeakHold};
use hytte_plugin::proto::{
    Capability, Dir, Effect, Manifest, Mount, Node, NowPlaying, SPECTRUM_BINS, StateKey,
};
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View, tick_stream};

/// Stable plugin id — the host's mount-slot ownership key and audit-log subject.
const PLUGIN_ID: &str = "audio-widget";

// Node ids. The `Pixels` ids make each re-render swap its texture in place
// instead of rebuilding the widget.
const ROOT_ID: &str = "audio-widget-root";
const MARQUEE_ID: &str = "audio-widget-marquee";
const SCOPE_ID: &str = "audio-widget-scope";
const LEDS_ID: &str = "audio-widget-leds";

/// The skin every widget wears. VFD (cyan glow on near-black) reads like a hi-fi
/// display; its lit ink is accent-tinted by the SDK (#376) so the card matches
/// the desktop out of the box.
const STYLE: DisplayStyle = DisplayStyle::Vfd;

/// The frame cadence: ~20 Hz, matching the spectrum push rate for a smooth
/// animation without outrunning the data.
const TICK: Duration = Duration::from_millis(50);

/// The marquee window width in px — a wide ticker inside the ~296 px card.
const MARQUEE_WINDOW_PX: usize = 268;
/// **Virtual pixels** (dots) the marquee pans per frame tick — one, the
/// smallest step the display has: ≈20 dots/s at the 20 Hz [`TICK`], which
/// reads as a classic ticker. The unit is the dot, not the buffer pixel
/// (#839): a step that isn't a whole dot is not expressible, which is what
/// keeps the text stepping grid-column to grid-column instead of smearing
/// across dot positions the way the pre-#839 3-px step did.
const MARQUEE_STEP: usize = 1;

/// Per-tick release (fall) rates, in level units — the meters' ballistics.
/// The spectrum bars fall in ~1 s, the LED bar in ~0.4 s (a snappy VU release),
/// and the peak-hold dot in ~1.7 s (it lingers, then drops).
const BIN_RELEASE: f32 = 0.05;
const LEVEL_RELEASE: f32 = 0.12;
const PEAK_RELEASE: f32 = 0.03;

/// Peak-hold level above which the card reads as "audio playing" — picks the
/// marquee banner. Read off the slow-release dot so it doesn't flicker on
/// transients (it lingers ~1.7 s past the last sound).
const ACTIVE_THRESHOLD: f32 = 0.03;

/// The banner while audio is playing (wider than the window, so it scrolls).
/// Not the track title — see the crate docs' dot-matrix note. Every char is
/// font-covered (asserted in tests).
const ACTIVE_MSG: &str = "~ NOW VIBING ~ TROLLSHELL AUDIO ~ ";
/// The banner while silent (fits the window, so it holds static).
const IDLE_MSG: &str = "- SILENCE -";

/// The plugin's own message: a single ~20 Hz heartbeat from [`Plugin::sources`].
/// `Clone` is required by [`tick_stream`], which emits a clone each period.
#[derive(Debug, Clone)]
enum Msg {
    /// One frame elapsed — advance the marquee and release the meters.
    Tick,
}

/// The rasterized marquee strip, cached by the exact text it was drawn from.
///
/// The preem [`Marquee`] API is render-**once**, then [`window`](MarqueeStrip::window)
/// per frame (`crates/hytte-plugin/src/preem/marquee.rs`): `render` rasterizes the
/// message into its font-space bitmap and paints the window's fixed ghost matrix
/// once, while `window` only clones that backdrop and lights the dots the offset
/// selects. But the SDK calls [`view`](AudioWidget::view)
/// on every input — ~43×/s while visible (the 20 Hz tick + ~23 Hz spectrum
/// push) — for marquee text that changes only when the track or the idle/active
/// state flips (#560). So the strip is rasterized once per distinct text and
/// merely windowed each frame.
#[derive(Debug, Default)]
struct MarqueeCache {
    /// The cached `(text, strip)`: the strip rasterized from that exact text.
    /// `None` before the first [`frame`](Self::frame).
    entry: Option<(String, MarqueeStrip)>,
    /// How many times the strip has been (re)rasterized — a test hook (see
    /// `the_marquee_strip_is_cached`) proving the strip re-renders **iff** the
    /// text changed. Not logical state, so it's `#[cfg(test)]`-only and never
    /// rides in the shipped widget.
    #[cfg(test)]
    renders: u64,
}

impl MarqueeCache {
    /// The `MARQUEE_WINDOW_PX`-wide marquee frame for `text` at scroll `offset`.
    /// Re-rasterizes the strip only when `text` differs from the last render;
    /// otherwise it just slides the window over the cached strip.
    fn frame(&mut self, text: &str, offset: usize) -> Frame {
        if self.entry.as_ref().is_none_or(|(t, _)| t.as_str() != text) {
            let strip = Marquee::new(STYLE)
                .window_px(MARQUEE_WINDOW_PX)
                .render(text);
            self.entry = Some((text.to_owned(), strip));
            #[cfg(test)]
            {
                self.renders += 1;
            }
        }
        self.entry
            .as_ref()
            .expect("populated above")
            .1
            .window(offset)
    }
}

/// The whole audio widget — rebuilt on every (re)connect (the host stores
/// nothing), re-derived from the next spectrum push.
#[derive(Debug)]
struct AudioWidget {
    /// Whether the sidebar (and so this card) is currently shown — the
    /// [`SlotVisible`](Input::SlotVisible) gate for the frame timer's work.
    visible: bool,
    /// Monotone animation frame counter; drives the marquee scroll offset.
    frame: u64,
    /// Displayed spectrum bands: attack on push, release on tick. All-zero until
    /// the first push (a flat baseline), so the scope shows even on silence.
    bins: [f32; SPECTRUM_BINS],
    /// The LED bar level — a fast-releasing peak follower of the overall level.
    level: PeakHold,
    /// The LED peak-hold dot — a slow-releasing peak-hold of the overall level.
    peak: PeakHold,
    /// The current-track digest off the host's now-playing push (#528). The
    /// marquee scrolls `title — artist` while `playing`, and falls back to the
    /// decorative banner otherwise (see [`AudioWidget::marquee_text`]).
    now_playing: NowPlaying,
    /// The rasterized-marquee cache (#560). `RefCell` because [`view`](Self::view)
    /// takes `&self` yet refills the cache lazily.
    marquee_cache: RefCell<MarqueeCache>,
}

// `PartialEq` is hand-written (not derived) to exclude `marquee_cache`: it's a
// pure derived-render cache, so two models with equal logical state must compare
// equal regardless of what either has cached — otherwise a stale/differing cache
// could leak into the SDK's `view() != last_view` dedup or model equality.
// (`Eq` stays underived: `bins` is `[f32; N]`, `PartialEq` but not `Eq`.)
impl PartialEq for AudioWidget {
    fn eq(&self, other: &Self) -> bool {
        self.visible == other.visible
            && self.frame == other.frame
            && self.bins == other.bins
            && self.level == other.level
            && self.peak == other.peak
            && self.now_playing == other.now_playing
    }
}

impl AudioWidget {
    /// Fold a fresh spectrum push in: attack the meters (raise each to the new
    /// level; the release lives on the tick).
    fn fold(&mut self, peak: f32, bins: &[f32; SPECTRUM_BINS]) {
        for (b, &n) in self.bins.iter_mut().zip(bins.iter()) {
            *b = b.max(n.clamp(0.0, 1.0));
        }
        self.level.push(peak);
        self.peak.push(peak);
    }

    /// Park the meters (#422): drop every displayed value straight back to rest,
    /// so the card re-appears at its `init` baseline instead of frozen on the
    /// last frame before the sidebar closed. Called on the `SlotVisible` falling
    /// edge only — a repeat while already parked is a no-op anyway, but keeping
    /// it to the edge says what it means.
    ///
    /// The marquee's [`now_playing`](Self::now_playing) is deliberately *not*
    /// reset: the host re-seeds the current track right after `SlotVisibility(true)`
    /// on the unpark edge (#542), so clearing it here would only insert one
    /// frame of "- SILENCE -" between the reopen and that re-seed. The meters
    /// have no such re-seed — they can only be re-derived from live audio.
    fn park(&mut self) {
        self.bins = [0.0; SPECTRUM_BINS];
        self.level.reset();
        self.peak.reset();
    }

    /// One frame: advance the scroll and release every meter toward rest.
    fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        for b in &mut self.bins {
            *b = (*b - BIN_RELEASE).max(0.0);
        }
        self.level.decay();
        self.peak.decay();
    }

    /// Whether the card reads as "audio playing" (off the lingering peak-hold).
    fn active(&self) -> bool {
        self.peak.value() > ACTIVE_THRESHOLD
    }

    /// The marquee text (#528): the live track (`title — artist`, or just the
    /// title) while the host reports a *playing* player with a title, else the
    /// decorative banner off the peak level (the pre-#528 fallback). The trailing
    /// gap keeps the scroll from butting the wrap point against itself.
    fn marquee_text(&self) -> String {
        if self.now_playing.playing && !self.now_playing.title.trim().is_empty() {
            let title = self.now_playing.title.trim();
            let artist = self.now_playing.artist.trim();
            if artist.is_empty() {
                format!("{title}   ")
            } else {
                format!("{title} — {artist}   ")
            }
        } else if self.active() {
            ACTIVE_MSG.to_owned()
        } else {
            IDLE_MSG.to_owned()
        }
    }

    /// The marquee scroll offset, in whole dots.
    /// [`MarqueeStrip::window`](hytte_plugin::preem::MarqueeStrip::window)
    /// wraps it modulo the strip period, so the raw counter is fine; bound it
    /// before the multiply so the product can never overflow.
    fn marquee_offset(&self) -> usize {
        let n = usize::try_from(self.frame % 1_000_000).unwrap_or(0);
        n.saturating_mul(MARQUEE_STEP)
    }
}

impl Plugin for AudioWidget {
    type Msg = Msg;
    /// Purely display: no I/O of its own, no commands.
    type Cmd = std::convert::Infallible;

    /// Mounts [`Mount::SidebarTop`] (`order = 2`, so it sorts below the clock /
    /// preem demos if co-mounted). Subscribes [`StateKey::AudioSpectrum`] (the
    /// data), [`StateKey::SlotVisible`] (the park gate), and
    /// [`StateKey::NowPlaying`] (the marquee track, #528) — the last paired with
    /// [`Capability::NowPlaying`], the capability the host requires on top of the
    /// subscription. The SDK adds the accent subscription (#376) on its behalf.
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, Mount::SidebarTop).with_order(2);
        m.subscribes = vec![
            StateKey::AudioSpectrum,
            StateKey::SlotVisible,
            StateKey::NowPlaying,
        ];
        m.capabilities = vec![Capability::NowPlaying];
        m
    }

    /// A parked, silent card — the seed render mounts the slot immediately; the
    /// register-time [`SlotVisible`](Input::SlotVisible) seed and the first push
    /// bring it to life. The command sender goes unused (`Cmd = Infallible`).
    fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
        Self {
            visible: false,
            frame: 0,
            bins: [0.0; SPECTRUM_BINS],
            level: PeakHold::new(LEVEL_RELEASE),
            peak: PeakHold::new(PEAK_RELEASE),
            now_playing: NowPlaying::default(),
            marquee_cache: RefCell::default(),
        }
    }

    /// A ~20 Hz frame tick, created per session and dropped on disconnect. The
    /// command receiver goes unused. `tick_stream` fires immediately then every
    /// [`TICK`] — the leading tick is a harmless no-op while still parked.
    fn sources(_cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        Some(Box::pin(tick_stream(TICK, Msg::Tick)))
    }

    /// Fold one input. Pure and panic-free over any host-sent value; re-rendering
    /// is the runtime's problem (identical trees are deduped). Ticks and pushes
    /// are gated on visibility so the card parks while the sidebar is closed, and
    /// the hide edge resets the meters so the reopen isn't a stale frame (#422).
    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            // The visibility gate (#288), with the #422 park: going off-screen
            // drops the meters to rest rather than freezing them mid-bar.
            Input::SlotVisible(v) => {
                if self.visible && !v {
                    self.park();
                }
                self.visible = v;
            }
            // The audio-spectrum push (#405): fold it while visible; ignore it
            // while parked (the display re-derives from the next push on reopen).
            // Not dead code even though the host tears the tap down when nothing
            // on-screen wants it: the tap is refcounted across *all* subscribers
            // (#559), so another on-screen one — a bar-mounted plugin, say — keeps
            // frames flowing to this hidden card too.
            Input::AudioSpectrum(s) => {
                if self.visible {
                    self.fold(s.peak, &s.bins);
                }
            }
            // The frame heartbeat: animate + release only while visible.
            Input::App(Msg::Tick) => {
                if self.visible {
                    self.tick();
                }
            }
            // The now-playing push (#528): adopt it while visible so the marquee
            // shows the live track; ignore it while parked (the card re-derives
            // from the next push on reopen, like the spectrum).
            Input::NowPlaying(np) => {
                if self.visible {
                    self.now_playing = np;
                }
            }
            // No Clock subscription (empty snapshot), no interactive nodes, no
            // commands or consent, and the other domain pushes aren't subscribed —
            // all no-ops.
            Input::Snapshot(_)
            | Input::Event { .. }
            | Input::EffectResult { .. }
            | Input::ConsentDecision { .. }
            | Input::CalendarUpcoming(_)
            | Input::SessionLocked(_)
            | Input::DatasourceQuery { .. }
            | Input::DatasourceResult { .. } => {}
        }
        Vec::new()
    }

    /// One vertical card: the dot-matrix marquee, the spectrum scope, and the LED
    /// peak/level strip — all in the VFD skin. Every `Pixels` buffer satisfies the
    /// host's `len == w * h * 4` invariant by kit construction, and every widget
    /// fits the ~296 px sidebar content width. No `.card`/`.ts-plugin-*` class —
    /// the host's region wrapper supplies the card chrome.
    fn view(&self) -> View {
        // Render the marquee strip once per distinct text, then window it per
        // frame (#560) — see [`MarqueeCache`]. `borrow_mut` is uncontended: the
        // runtime never calls `view` reentrantly.
        let marquee = self
            .marquee_cache
            .borrow_mut()
            .frame(&self.marquee_text(), self.marquee_offset());
        let scope = spectrum::scope_tile(&self.bins);
        let leds = LedStrip::new(STYLE).render(self.level.value(), self.peak.value());
        Node::Box {
            id: Some(ROOT_ID.to_owned()),
            dir: Dir::Vertical,
            spacing: 8,
            scroll: false,
            classes: Vec::new(),
            children: vec![
                marquee.into_node(Some(MARQUEE_ID), Vec::new()),
                scope.into_node(Some(SCOPE_ID), Vec::new()),
                leds.into_node(Some(LEDS_ID), Vec::new()),
            ],
        }
        .into()
    }
}

fn main() {
    hytte_plugin::run::<AudioWidget>();
}

#[cfg(test)]
mod tests {
    use super::{ACTIVE_MSG, AudioWidget, IDLE_MSG, MARQUEE_STEP, Msg, PLUGIN_ID};
    use hytte_plugin::preem::font;
    use hytte_plugin::proto::{
        AudioSpectrum, Mount, Node, PluginMsg, SPECTRUM_BINS, StateKey, decode, encode,
    };
    use hytte_plugin::{Input, Plugin};

    fn fresh() -> AudioWidget {
        AudioWidget::init(hytte_plugin::cmd_channel().0)
    }

    /// A visible widget (the register-time `SlotVisible` seed already delivered).
    fn shown() -> AudioWidget {
        let mut m = fresh();
        m.update(Input::SlotVisible(true));
        m
    }

    fn spectrum(peak: f32, band: usize, level: f32) -> Input<Msg> {
        let mut bins = [0.0_f32; SPECTRUM_BINS];
        bins[band] = level;
        Input::AudioSpectrum(AudioSpectrum { peak, bins })
    }

    fn tick(m: &mut AudioWidget) {
        m.update(Input::App(Msg::Tick));
    }

    /// Walk a tree and collect every `Pixels` node's `(w, h, data.len())`.
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
                Node::Box { children, .. } | Node::Row { children, .. } => {
                    stack.extend(children.iter());
                }
                Node::Button { child, .. } => stack.push(child),
                _ => {}
            }
        }
        out
    }

    /// The manifest opts into the spectrum, the visibility gate, and the
    /// now-playing track (#528, paired with its capability), and mounts a sidebar
    /// card.
    #[test]
    fn manifest_subscribes_spectrum_and_visibility() {
        use hytte_plugin::proto::Capability;
        let m = AudioWidget::manifest();
        assert_eq!(m.id, PLUGIN_ID);
        assert_eq!(m.mount, Mount::SidebarTop);
        assert!(m.subscribes.contains(&StateKey::AudioSpectrum));
        assert!(m.subscribes.contains(&StateKey::SlotVisible));
        assert!(
            m.subscribes.contains(&StateKey::NowPlaying),
            "opts into the now-playing track (#528)"
        );
        assert_eq!(
            m.capabilities,
            vec![Capability::NowPlaying],
            "the now-playing push is capability-gated on top of the subscription"
        );
        m.check_proto()
            .expect("stamped with the current proto version");
    }

    /// A spectrum push attacks the meters, and ticks release them back toward
    /// rest — the attack-on-push / release-on-tick envelope.
    #[test]
    fn a_push_attacks_and_ticks_release() {
        let mut m = shown();
        let fx = m.update(spectrum(0.9, 4, 0.8));
        assert!(fx.is_empty(), "the card asks nothing of the shell");
        assert!(
            (m.peak.value() - 0.9).abs() < 1e-6,
            "the dot jumps to the peak"
        );
        assert!((m.level.value() - 0.9).abs() < 1e-6, "the bar jumps too");
        assert!((m.bins[4] - 0.8).abs() < 1e-6, "band 4 attacked");
        assert!(m.bins[0].abs() < 1e-6, "quiet bands stay low");

        // Release: quiet ticks lower every meter (the LED bar releases fastest).
        let (l0, p0, b0) = (m.level.value(), m.peak.value(), m.bins[4]);
        tick(&mut m);
        assert!(m.level.value() < l0, "the bar releases");
        assert!(m.peak.value() < p0, "the dot decays");
        assert!(m.bins[4] < b0, "the band falls");
        assert!(
            l0 - m.level.value() > p0 - m.peak.value(),
            "the bar releases faster than the peak-hold dot"
        );
    }

    /// The meters settle to exact rest after enough quiet ticks — no undershoot,
    /// no freeze mid-bar when the audio stops.
    #[test]
    fn the_meters_settle_to_rest() {
        let mut m = shown();
        let _ = m.update(spectrum(1.0, 0, 1.0));
        for _ in 0..200 {
            tick(&mut m);
        }
        // Every meter is clamped `>= 0.0`, so `<= 0.0` proves exact rest without
        // a float `==` (which clippy's `float_cmp` forbids).
        assert!(m.level.value() <= 0.0, "the LED bar rests at zero");
        assert!(m.peak.value() <= 0.0, "the peak-hold dot rests at zero");
        assert!(m.bins.iter().all(|&b| b <= 0.0), "all bands rest at zero");
        assert!(!m.active(), "a rested card reads as silent");
    }

    /// While the sidebar is hidden, pushes and ticks are no-ops — the card parks.
    #[test]
    fn a_hidden_card_parks() {
        let mut m = fresh(); // never made visible → parked
        assert!(!m.visible);
        let before = m.view();
        let _ = m.update(spectrum(1.0, 8, 1.0));
        tick(&mut m);
        assert_eq!(m.view(), before, "a parked card ignores pushes and ticks");
        // Once shown, it comes to life.
        m.update(Input::SlotVisible(true));
        let _ = m.update(spectrum(1.0, 8, 1.0));
        assert_ne!(m.view(), before, "a shown card folds the push");
    }

    /// Hiding the card parks the meters **at rest** (#422), so a reopen renders
    /// the baseline card rather than the loud frame it froze on — the stale first
    /// frame this issue is about. Gating alone wouldn't do it: the release lives
    /// on the (also gated) tick, so a frozen meter never walks itself down.
    #[test]
    fn hiding_parks_the_meters_at_rest() {
        let mut m = shown();
        let _ = m.update(spectrum(1.0, 8, 1.0));
        assert!(m.peak.value() > 0.5, "loud before the hide");
        let loud_view = m.view();

        m.update(Input::SlotVisible(false));
        assert!(m.bins.iter().all(|&b| b <= 0.0), "every band drops to rest");
        assert!(m.level.value() <= 0.0, "the LED bar drops to rest");
        assert!(m.peak.value() <= 0.0, "the peak-hold dot drops to rest");

        // Re-showing renders the baseline card, not the frozen loud one.
        m.update(Input::SlotVisible(true));
        assert_ne!(
            m.view(),
            loud_view,
            "a reopened card does not replay the pre-hide frame"
        );
        assert_eq!(
            m.view(),
            shown().view(),
            "it reopens exactly where a fresh card starts"
        );

        // And it still comes back to life on the next real push.
        let _ = m.update(spectrum(1.0, 8, 1.0));
        assert_eq!(m.view(), loud_view, "the next push re-derives the display");
    }

    /// The park is edge-driven but idempotent: a redundant `SlotVisible(false)`
    /// while already hidden changes nothing, and a hide that never saw audio is
    /// a no-op rather than a spurious re-render.
    #[test]
    fn parking_is_idempotent() {
        let mut m = fresh(); // never shown → already parked
        let before = m.view();
        m.update(Input::SlotVisible(false));
        assert_eq!(m.view(), before, "hiding an already-hidden card is a no-op");
        m.update(Input::SlotVisible(true));
        m.update(Input::SlotVisible(false));
        m.update(Input::SlotVisible(false));
        assert_eq!(m.view(), before, "repeated hides stay at rest");
    }

    /// The marquee falls back to the decorative banner on the audio-active state
    /// (off the lingering peak-hold) when nothing is playing, and only the active
    /// banner scrolls.
    #[test]
    fn the_marquee_reflects_audio_state() {
        let mut m = shown();
        assert!(!m.active(), "silent at rest");
        assert_eq!(m.marquee_text(), IDLE_MSG);
        let _ = m.update(spectrum(0.9, 0, 0.9));
        assert!(m.active(), "a loud push reads as playing");
        assert_eq!(m.marquee_text(), ACTIVE_MSG);
        // The active banner overflows the window (scrolls); a step moves pixels.
        let strip = hytte_plugin::preem::Marquee::new(super::STYLE)
            .window_px(super::MARQUEE_WINDOW_PX)
            .render(ACTIVE_MSG);
        assert!(strip.scrolls(), "the active banner scrolls");
    }

    /// The now-playing push (#528) takes over the marquee while playing — title
    /// and artist — and releases back to the banner when it stops.
    #[test]
    fn now_playing_drives_the_marquee_over_the_banner() {
        use hytte_plugin::proto::NowPlaying;
        let mut m = shown();
        // A loud push would otherwise pick the active banner…
        let _ = m.update(spectrum(0.9, 0, 0.9));
        assert_eq!(m.marquee_text(), ACTIVE_MSG);
        // …but a playing track wins.
        m.update(Input::NowPlaying(NowPlaying {
            title: "Chrome Rain".to_owned(),
            artist: "Choom".to_owned(),
            playing: true,
        }));
        assert!(
            m.marquee_text().starts_with("Chrome Rain — Choom"),
            "playing track scrolls title — artist: {}",
            m.marquee_text()
        );
        // Title only (no artist).
        m.update(Input::NowPlaying(NowPlaying {
            title: "Untitled".to_owned(),
            artist: String::new(),
            playing: true,
        }));
        assert!(m.marquee_text().starts_with("Untitled"));
        assert!(!m.marquee_text().contains('—'), "no dash without an artist");
        // Paused → back to the banner off the peak level.
        m.update(Input::NowPlaying(NowPlaying {
            title: "Chrome Rain".to_owned(),
            artist: "Choom".to_owned(),
            playing: false,
        }));
        assert_eq!(
            m.marquee_text(),
            ACTIVE_MSG,
            "a not-playing track falls back to the banner"
        );
    }

    /// While hidden the now-playing push is ignored (the card parks), exactly like
    /// the spectrum push.
    #[test]
    fn a_hidden_card_ignores_now_playing() {
        use hytte_plugin::proto::NowPlaying;
        let mut m = fresh(); // never shown → parked
        let before = m.view();
        m.update(Input::NowPlaying(NowPlaying {
            title: "Chrome Rain".to_owned(),
            artist: "Choom".to_owned(),
            playing: true,
        }));
        assert_eq!(
            m.view(),
            before,
            "a parked card ignores the now-playing push"
        );
    }

    /// The scroll offset advances one step per frame tick and wraps the counter.
    #[test]
    fn the_marquee_scrolls_with_ticks() {
        let mut m = shown();
        let o0 = m.marquee_offset();
        tick(&mut m);
        assert_eq!(m.marquee_offset() - o0, MARQUEE_STEP, "one tick, one step");
    }

    /// The marquee strip is rasterized **once per distinct text** and merely
    /// windowed per frame (#560): identical text over repeated `view()`s
    /// re-renders nothing, advancing the scroll offset re-renders nothing, and a
    /// text change (idle banner → active banner) re-rasterizes exactly once. The
    /// rasterization count is read off the widget's own cache — no SDK hook.
    #[test]
    fn the_marquee_strip_is_cached() {
        let mut m = shown();
        assert_eq!(m.marquee_text(), IDLE_MSG, "silent at rest");

        // First view rasterizes the idle banner.
        let _ = m.view();
        assert_eq!(
            m.marquee_cache.borrow().renders,
            1,
            "the first view rasterizes the strip"
        );

        // Same text over more views (offset unchanged) → no re-rasterization.
        let _ = m.view();
        let _ = m.view();
        assert_eq!(
            m.marquee_cache.borrow().renders,
            1,
            "identical text reuses the cached strip"
        );

        // A tick advances the scroll offset but not the text — still windowing,
        // never re-rasterizing.
        tick(&mut m);
        assert_eq!(m.marquee_text(), IDLE_MSG, "still silent, same text");
        let _ = m.view();
        assert_eq!(
            m.marquee_cache.borrow().renders,
            1,
            "advancing the window offset does not re-rasterize"
        );

        // A loud push flips the banner idle → active: exactly one re-rasterization.
        let _ = m.update(spectrum(0.9, 0, 0.9));
        assert_eq!(m.marquee_text(), ACTIVE_MSG, "a loud push reads as playing");
        let _ = m.view();
        assert_eq!(
            m.marquee_cache.borrow().renders,
            2,
            "a text change re-rasterizes the strip once"
        );

        // The active banner scrolls; ticking advances the offset and windows the
        // same strip — still no re-rasterization.
        tick(&mut m);
        assert_eq!(
            m.marquee_text(),
            ACTIVE_MSG,
            "the peak lingers, still active"
        );
        let _ = m.view();
        assert_eq!(
            m.marquee_cache.borrow().renders,
            2,
            "scrolling the active banner windows the cached strip"
        );
    }

    /// Every `Pixels` buffer honors the host's `len == w*h*4` seam and the ~296 px
    /// sidebar content width — across silent and loud states.
    #[test]
    fn every_view_pixels_is_valid_and_fits_the_card() {
        let mut m = shown();
        for _ in 0..4 {
            let bufs = pixels_of(&m.view().tree);
            assert_eq!(bufs.len(), 3, "marquee + scope + LED strip");
            for (w, h, len) in bufs {
                assert_eq!(len, (w as usize) * (h as usize) * 4);
                assert!(w > 0 && h > 0);
                assert!(w <= 296, "width {w} fits the sidebar card");
            }
            let _ = m.update(spectrum(0.7, 2, 0.6));
            tick(&mut m);
        }
    }

    /// The view is a pure function of the model, and a fresh push renders a new
    /// card (the meters actually moved).
    #[test]
    fn view_is_deterministic_and_reacts() {
        let mut m = shown();
        assert_eq!(m.view(), m.view(), "view is pure");
        let before = m.view();
        let _ = m.update(spectrum(1.0, 8, 1.0));
        assert_ne!(m.view(), before, "a loud push renders a new card");
    }

    /// Both marquee banners are fully covered by the kit font — no accidental
    /// notdef boxes.
    #[test]
    fn banners_are_font_covered() {
        for msg in [ACTIVE_MSG, IDLE_MSG] {
            for c in msg.chars() {
                assert!(font::glyph(c).is_some(), "banner char {c:?} has a glyph");
            }
        }
    }

    /// The frames built from this plugin's data are valid on the wire.
    #[test]
    fn register_and_render_frames_round_trip() {
        let reg = PluginMsg::Register {
            manifest: AudioWidget::manifest(),
        };
        let back: PluginMsg = decode(&encode(&reg)).expect("register frame decodes");
        assert_eq!(reg, back);

        let mut m = shown();
        let _ = m.update(spectrum(0.8, 5, 0.7));
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
