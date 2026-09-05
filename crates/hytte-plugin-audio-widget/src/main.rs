//! `hytte-plugin-audio-widget` — an audio-reactive trollshell sidebar card 🎚️
//! (issue #506, "preem extra audio widget").
//!
//! An out-of-process widget plugin on the `hytte-plugin` SDK that composes four
//! preem raster widgets off the host's [`StateKey::AudioSpectrum`] push (#405,
//! perceptual-dBFS levels since #504) into one sidebar card, over a transport
//! row — the front panel of a y2k USB MP3 player (#840):
//!
//! - a **dot-matrix marquee** (a scrolling ticker banner, see the dot-matrix
//!   note below on why it isn't the track title yet),
//! - the **time readout** — a second, static dot-matrix line showing
//!   `elapsed/total` off the [`NowPlaying`] digest's timing fields (#840),
//! - a **16-band spectrum scope** (`spectrum::scope_tile`, mirroring the
//!   preem-demo's tile),
//! - the **LED peak/level strip** ([`hytte_plugin::display::LedStrip`], the kit
//!   widget this issue adds): a row of discrete LEDs lighting with the overall
//!   level, topped by a peak-hold dot that floats and decays, and
//! - the **transport row** (#840): chunky prev / play-pause / next buttons that
//!   drive the host's *active* player.
//!
//! # Three of the four displays speak preem state now (#884)
//!
//! The marquee, the time readout and the LED strip go through
//! [`hytte_plugin::display`] rather than calling the raster kit by hand: against
//! a shell that advertises the preem vocabulary in `HostMsg::Hello` each ships
//! as a typed `Node::Preem` the shell draws and animates; against one that
//! doesn't each CPU-rasterises to exactly the `Node::Pixels` it produced before,
//! byte for byte (pinned by the tests below). Nothing in `view` branches on
//! which host is on the other end.
//!
//! The **scroll offset moves shell-side**: the wire carries the banner text and
//! a speed in dots per second, never a position, so a marquee showing an
//! unchanged title sends *one* frame when the title changes instead of ~43 a
//! second forever. [`MARQUEE_SPEED_DPS`] is the same rate
//! [`MARQUEE_STEP`]-per-[`TICK`] gives plugin-side, so the two hosts scroll at
//! one visual rate from one number (`the_two_scroll_rates_agree` pins it).
//!
//! The **meters' ballistics stay plugin-side**, and that is deliberate rather
//! than an omission. Both LED values are attack-on-push / release-on-tick
//! envelopes over the raw audio, and the wire has one number for the level: a
//! *level* envelope has no wire form at all, and the strip's declarable
//! [`peak_hold`](hytte_plugin::display::LedStrip::peak_hold) folds the level it
//! is *given*, which here is already the fast-released bar value rather than the
//! raw peak the dot follows. Declaring it would therefore change what the dot
//! holds. So the card keeps both [`PeakHold`]s and states the two resulting
//! numbers, which is byte-exact against an old shell and still replaces a
//! ~25 KB RGBA buffer per frame with two floats against a new one.
//!
//! The **spectrum scope stays on the escape hatch**: `spectrum::scope_tile` is a
//! hand-drawn 16-band bar tile, not the kit's glow-trace `Scope`, and the
//! vocabulary has no word for it — exactly what `Frame::into_node` is for.
//!
//! # Transport — zero new protocol (#840)
//!
//! The card was display-only until #840; the controls needed nothing new on the
//! wire. [`Effect::Media`] and its three [`MediaAction`]s already existed and the
//! host's effect broker already resolves them onto the active player the same way
//! the bar chip does (#648) — the wire vocabulary is deliberately
//! player-agnostic, so the card names no player and can't name the wrong one. All
//! this plugin adds is [`Capability::Media`] in the manifest (the grant the host
//! enforces the effects against), three [`Node::Button`]s, and a click match. The
//! play-pause glyph mirrors [`NowPlaying::playing`], so the button shows what the
//! *player* is doing, not what was last clicked.
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
//! # The time readout parks the host's poller too (#840)
//!
//! `position_us` is sampled by a 250 ms `Position` poller in the shell's mpris
//! service, gated since #228 on "is a consumer on screen". This card is now a
//! second consumer, so the shell ORs the sidebar-open aggregate into that gate
//! (`trollshell/src/main.rs`) — the sidebar's own [`SlotVisible`] signal, the
//! same one this card parks on below. Closing the sidebar therefore parks both
//! ends at once: no polling, and no digest pushes to render.
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

use std::time::Duration;

use hytte_plugin::display::{DotMatrix, LedStrip, Marquee, StyleName};
use hytte_plugin::preem::PeakHold;
use hytte_plugin::proto::{
    Capability, Dir, Effect, EventKind, Manifest, MediaAction, Mount, Node, NowPlaying,
    SPECTRUM_BINS, StateKey,
};
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View, tick_stream};

/// Stable plugin id — the host's mount-slot ownership key and audit-log subject.
const PLUGIN_ID: &str = "audio-widget";

// Node ids. Each display's id keys the host reconciler onto the *same* renderer
// across renders — swapping a texture in place in raster mode, and (#882's
// `preem_id` rule) keeping the shell-owned scroll continuous in state mode.
const ROOT_ID: &str = "audio-widget-root";
const MARQUEE_ID: &str = "audio-widget-marquee";
const READOUT_ID: &str = "audio-widget-readout";
const SCOPE_ID: &str = "audio-widget-scope";
const LEDS_ID: &str = "audio-widget-leds";
const TRANSPORT_ID: &str = "audio-widget-transport";
// The three transport buttons. `Node::Button` requires an id — it *is* the click
// event target, and these are what [`transport_action`] matches on.
const PREV_ID: &str = "audio-widget-prev";
const PLAY_ID: &str = "audio-widget-play";
const NEXT_ID: &str = "audio-widget-next";

/// Transport glyphs — Adwaita's standard symbolic media icons, so the row
/// themes with the rest of the shell (the bar's own mpris chip draws the same
/// three) and recolors with the icon theme instead of shipping bespoke art.
const ICON_PREV: &str = "media-skip-backward-symbolic";
const ICON_PLAY: &str = "media-playback-start-symbolic";
const ICON_PAUSE: &str = "media-playback-pause-symbolic";
const ICON_NEXT: &str = "media-skip-forward-symbolic";

/// The skin every widget wears. VFD (cyan glow on near-black) reads like a hi-fi
/// display; its lit ink is accent-tinted by the SDK (#376) so the card matches
/// the desktop out of the box. A wire [`StyleName`] since #884 — the skin
/// travels to a preem-speaking shell as a *name*, never as resolved colors,
/// which is what lets the shell re-tint every display live (#396).
const STYLE: StyleName = StyleName::Vfd;

/// The frame cadence: ~20 Hz, matching the spectrum push rate for a smooth
/// animation without outrunning the data.
const TICK: Duration = Duration::from_millis(50);

/// The marquee window width in px — a wide ticker inside the ~296 px card.
/// A `u32` since #884: it is a wire config field now, not a kit builder arg.
const MARQUEE_WINDOW_PX: u32 = 268;
/// **Virtual pixels** (dots) the marquee pans per frame tick — one, the
/// smallest step the display has: ≈20 dots/s at the 20 Hz [`TICK`], which
/// reads as a classic ticker. The unit is the dot, not the buffer pixel
/// (#839): a step that isn't a whole dot is not expressible, which is what
/// keeps the text stepping grid-column to grid-column instead of smearing
/// across dot positions the way the pre-#839 3-px step did.
const MARQUEE_STEP: usize = 1;

/// The **same** scroll rate stated for a shell that owns the offset (#884), in
/// dots per second: [`MARQUEE_STEP`] dot per [`TICK`] is 1/0.05 s = 20 dots/s.
///
/// Two hosts, one visual rate from one number — the plugin integrates it via
/// [`AudioWidget::marquee_offset`] when it owns the offset, and the shell
/// integrates it on its own frame clock when it does.
/// `the_two_scroll_rates_agree` pins the arithmetic so a change to either
/// constant can't quietly desync them.
const MARQUEE_SPEED_DPS: f32 = 20.0;

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

/// The time readout's stand-in for a duration the player doesn't report (#840):
/// `mpris:length` is absent on live streams and plenty of web players, and a
/// `00:00` total would read as a zero-length track rather than an unknown one.
const UNKNOWN_TIME: &str = "--:--";
/// What [`mmss`] shows past its two-digit minute field — a saturation, not a
/// wrap, so an over-long track reads as "at least this much" instead of
/// restarting from `00:00`. The field stays two digits so the readout keeps a
/// fixed 11-char width and the card never reflows mid-track.
const MAX_TIME: &str = "99:59";

/// The plugin's own message: a single ~20 Hz heartbeat from [`Plugin::sources`].
/// `Clone` is required by [`tick_stream`], which emits a clone each period.
#[derive(Debug, Clone)]
enum Msg {
    /// One frame elapsed — advance the marquee and release the meters.
    Tick,
}

/// A microsecond duration as `MM:SS` (#840).
///
/// Minutes are **not** rolled over into hours: an hour-long track reads `65:00`,
/// which is what a y2k player's five-cell time field does and what keeps the
/// readout a fixed width. Past the two-digit field it saturates at [`MAX_TIME`]
/// rather than widening to `100:00` (which would reflow the card) or wrapping to
/// `00:00` (which would lie about the direction of travel).
fn mmss(us: u64) -> String {
    let secs = us / 1_000_000;
    let mins = secs / 60;
    if mins > 99 {
        return MAX_TIME.to_owned();
    }
    let rem = secs % 60;
    format!("{mins:02}:{rem:02}")
}

/// The [`MediaAction`] a click on `node` asks for, or `None` for any other node
/// (the card's displays are all click-inert). Pure, so the button-id → action
/// mapping is testable without a session.
fn transport_action(node: &str) -> Option<MediaAction> {
    match node {
        PREV_ID => Some(MediaAction::Previous),
        PLAY_ID => Some(MediaAction::PlayPause),
        NEXT_ID => Some(MediaAction::Next),
        _ => None,
    }
}

/// One transport button: an id'd [`Node::Button`] wrapping a symbolic icon.
///
/// `primary` picks the play-pause button's treatment — libadwaita's own
/// `suggested-action` fill against the other two's `flat`, so the big lit button
/// sits between two quiet ones the way a player's face plate does. Both wear
/// `circular`; every class here is a stock libadwaita/GTK token, since a plugin
/// cannot ship CSS of its own and an unmatched token is simply inert.
fn transport_button(id: &str, icon: &str, primary: bool) -> Node {
    let mut classes = vec!["circular".to_owned()];
    classes.push(if primary { "suggested-action" } else { "flat" }.to_owned());
    Node::Button {
        id: id.to_owned(),
        classes,
        child: Box::new(Node::Icon {
            id: None,
            name: icon.to_owned(),
            classes: Vec::new(),
        }),
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
    /// The scrolling ticker (#884). Holds the skin, the window and the scroll
    /// *rate*; its plugin-side offset is re-stated from [`Self::frame`] on every
    /// tick and is a no-op once the shell owns the scroll.
    marquee: Marquee,
    /// The `elapsed/total` time line (#884). Config only — a dot-matrix strip is
    /// pure, so its whole state is the text handed to `node`.
    readout: DotMatrix,
    /// The LED peak/level strip (#884). Config only: no
    /// [`peak_hold`](LedStrip::peak_hold) is declared, because both meters are
    /// plugin-side envelopes (see the crate docs), so `view` clones this and
    /// states the two numbers the model already holds.
    leds: LedStrip,
}

// `PartialEq` is hand-written (not derived) to exclude the three display
// widgets: `readout` and `leds` are immutable config, and `marquee` carries only
// the raster-side scroll offset, which is a pure function of `frame` (already
// compared). Two models with equal logical state must compare equal regardless,
// so no derived render state can leak into the SDK's `view() != last_view` dedup
// or into model equality — the same rule the pre-#884 marquee cache followed.
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
    ///
    /// The scroll is *stated*, not integrated (#884): this card's offset is a
    /// pure function of its frame counter, which is exactly the case
    /// [`Marquee::set_scroll_dots`] exists for. It is a no-op while the host
    /// speaks preem — the shell integrates [`MARQUEE_SPEED_DPS`] on its own
    /// frame clock there — so it is safe (and correct) to call unconditionally.
    fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        let dots = self.marquee_offset();
        self.marquee.set_scroll_dots(dots);
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

    /// The time readout's text (#840): `elapsed/total`, with [`UNKNOWN_TIME`]
    /// standing in for a duration the player doesn't report.
    ///
    /// Always both fields, always 11 chars — 268 px of dot matrix, the same
    /// width as the marquee window above it, and a width that never changes as a
    /// track plays or a duration arrives late. The separator is a bare `/`
    /// rather than a spaced ` / ` for exactly that budget: the spaced form is 13
    /// chars, 316 px, past the ~296 px sidebar card.
    fn readout_text(&self) -> String {
        let total = if self.now_playing.length_us == 0 {
            UNKNOWN_TIME.to_owned()
        } else {
            mmss(self.now_playing.length_us)
        };
        format!("{}/{total}", mmss(self.now_playing.position_us))
    }

    /// The play-pause button's glyph — pause while the host reports a *playing*
    /// player, play otherwise. Mirrors the player's state, not the last click,
    /// so a track paused from the bar chip or a keyboard key flips this too.
    fn play_icon(&self) -> &'static str {
        if self.now_playing.playing {
            ICON_PAUSE
        } else {
            ICON_PLAY
        }
    }

    /// The marquee scroll offset, in whole dots. The strip wraps it modulo its
    /// own period, so the raw counter is fine; bound it before the multiply so
    /// the product can never overflow.
    fn marquee_offset(&self) -> usize {
        let n = usize::try_from(self.frame % 1_000_000).unwrap_or(0);
        n.saturating_mul(MARQUEE_STEP)
    }
}

impl Plugin for AudioWidget {
    type Msg = Msg;
    /// No I/O of its own: the transport row rides the render frame's effects, so
    /// nothing needs the async command lane.
    type Cmd = std::convert::Infallible;

    /// Mounts [`Mount::SidebarBottom`] at `order = 1` (#840): the host sorts a
    /// region by `(order, id)` ascending, so the deck sits below the pet (`-1`)
    /// and the departures board (`0`), at the foot of the sidebar where Annika
    /// asked for it. Subscribes [`StateKey::AudioSpectrum`] (the meters),
    /// [`StateKey::SlotVisible`] (the park gate), and [`StateKey::NowPlaying`]
    /// (the marquee track and the time readout, #528/#840) — the last paired
    /// with [`Capability::NowPlaying`], the capability the host requires on top
    /// of the subscription. [`Capability::Media`] is the grant for the transport
    /// row's effects (#840); the host drops a `Media` effect from a plugin that
    /// didn't declare it. The SDK adds the accent subscription (#376) on its
    /// behalf.
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, Mount::SidebarBottom).with_order(1);
        m.subscribes = vec![
            StateKey::AudioSpectrum,
            StateKey::SlotVisible,
            StateKey::NowPlaying,
        ];
        m.capabilities = vec![Capability::NowPlaying, Capability::Media];
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
            marquee: Marquee::new(STYLE)
                .window_px(MARQUEE_WINDOW_PX)
                .speed_dots_per_sec(MARQUEE_SPEED_DPS),
            readout: DotMatrix::new(STYLE),
            leds: LedStrip::new(STYLE),
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
            // The now-playing push (#528, timing since #840): adopt it while
            // visible so the marquee shows the live track and the readout the
            // live position; ignore it while parked (the card re-derives from
            // the next push on reopen, like the spectrum).
            Input::NowPlaying(np) => {
                if self.visible {
                    self.now_playing = np;
                }
            }
            // A transport click (#840). Deliberately **not** visibility-gated:
            // a click can only arrive from a card the user can see and is
            // pointing at, so gating it could only ever drop a real one on a
            // stale `visible` — and unlike the pushes there is nothing to
            // re-derive later. The card holds no optimistic playing/paused
            // state either: the effect goes to the player, and the glyph
            // follows on the next `NowPlaying` push, so the button can never
            // disagree with the player.
            Input::Event { node, kind } => {
                if matches!(kind, EventKind::Click)
                    && let Some(action) = transport_action(&node)
                {
                    return vec![Effect::Media(action)];
                }
            }
            // No Clock subscription (empty snapshot), no commands or consent,
            // and the other domain pushes aren't subscribed — all no-ops.
            Input::Snapshot(_)
            | Input::EffectResult { .. }
            | Input::ConsentDecision { .. }
            | Input::CalendarUpcoming(_)
            | Input::SessionLocked(_)
            | Input::DatasourceQuery { .. }
            | Input::DatasourceResult { .. } => {}
        }
        Vec::new()
    }

    /// One vertical card: the dot-matrix marquee, the time readout, the spectrum
    /// scope and the LED peak/level strip — all in the VFD skin — over the
    /// transport row. Every widget fits the ~296 px sidebar content width, and
    /// no `.card`/`.ts-plugin-*` class rides here — the host's region wrapper
    /// supplies the card chrome.
    ///
    /// Three of the four displays lower through the #884 seam, landing as typed
    /// `Node::Preem` or rasterised `Node::Pixels` depending on what the host
    /// advertised, with no branch here; the spectrum tile is hand-drawn and
    /// stays on `Frame::into_node`. In raster mode every buffer satisfies the
    /// host's `len == w * h * 4` invariant by kit construction.
    fn view(&self) -> View {
        // The LED strip is a pure projection of the two meters the model already
        // holds, so `view` clones the configured strip and states them. Neither
        // setter animates anything: with no `peak_hold` declared the strip owns
        // no plugin-side physics, and the explicit peak is the true inter-frame
        // dot the slow-release `PeakHold` computed.
        let mut leds = self.leds.clone();
        leds.set_level(self.level.value());
        leds.set_peak(Some(self.peak.value()));
        let scope = spectrum::scope_tile(&self.bins);
        // Spacer-flanked so the three buttons centre under the displays whatever
        // the card's width — `Node::Box` (not `Node::Row`) for the `spacing`
        // field, which is what keeps the circular buttons from butting together.
        let transport = Node::Box {
            id: Some(TRANSPORT_ID.to_owned()),
            dir: Dir::Horizontal,
            spacing: 6,
            scroll: false,
            classes: Vec::new(),
            children: vec![
                Node::Spacer,
                transport_button(PREV_ID, ICON_PREV, false),
                transport_button(PLAY_ID, self.play_icon(), true),
                transport_button(NEXT_ID, ICON_NEXT, false),
                Node::Spacer,
            ],
        };
        Node::Box {
            id: Some(ROOT_ID.to_owned()),
            dir: Dir::Vertical,
            spacing: 8,
            scroll: false,
            classes: Vec::new(),
            children: vec![
                self.marquee.node(MARQUEE_ID, &self.marquee_text()),
                self.readout.node(READOUT_ID, &self.readout_text()),
                scope.into_node(Some(SCOPE_ID), Vec::new()),
                leds.node(LEDS_ID),
                transport,
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
    use super::{
        ACTIVE_MSG, AudioWidget, ICON_PAUSE, ICON_PLAY, IDLE_MSG, LEDS_ID, MARQUEE_ID,
        MARQUEE_SPEED_DPS, MARQUEE_STEP, MARQUEE_WINDOW_PX, MAX_TIME, Msg, NEXT_ID, PLAY_ID,
        PLUGIN_ID, PREV_ID, READOUT_ID, TICK, UNKNOWN_TIME, mmss, transport_action,
    };
    use hytte_plugin::display::{RenderMode, StyleName, testing::with_render_mode};
    // Aliased rather than imported by name: the raster-parity tests below build
    // the *kit* `Marquee`/`LedStrip` by hand, and the model holds the *display*
    // wrappers of the same names — `kit::` keeps which is which unmissable.
    use hytte_plugin::preem::{self as kit, DisplayStyle, font};
    use hytte_plugin::proto::preem::PreemWidget;
    use hytte_plugin::proto::{
        AudioSpectrum, Effect, EventKind, MediaAction, Mount, Node, PluginMsg, SPECTRUM_BINS,
        StateKey, decode, encode,
    };
    use hytte_plugin::{Input, Plugin};

    /// The kit window width the raster arm builds, as the kit's `usize`.
    fn window_px() -> usize {
        usize::try_from(MARQUEE_WINDOW_PX).expect("the window fits a usize")
    }

    /// Unwrap one migrated display's typed widget, checking the reconciler's key
    /// and that a preem-speaking host got no pixels for it (#884).
    fn preem<'a>(node: &'a Node, want_id: &str) -> &'a PreemWidget {
        match node {
            Node::Preem { id, widget, .. } => {
                assert_eq!(id.as_deref(), Some(want_id), "the reconciler's key");
                widget.as_ref()
            }
            Node::Pixels { .. } => panic!("a preem-speaking host must not get pixels"),
            other => panic!("expected Node::Preem, got {other:?}"),
        }
    }

    /// One second in microseconds — the unit `NowPlaying`'s timing fields carry.
    const SEC: u64 = 1_000_000;

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
    /// now-playing track (#528, paired with its capability), holds the transport
    /// grant (#840), and mounts at the foot of the sidebar below pet (`-1`) and
    /// departures (`0`).
    #[test]
    fn manifest_subscribes_spectrum_and_visibility() {
        use hytte_plugin::proto::Capability;
        let m = AudioWidget::manifest();
        assert_eq!(m.id, PLUGIN_ID);
        assert_eq!(m.mount, Mount::SidebarBottom, "the deck sits at the bottom");
        assert!(
            m.order.is_some_and(|o| o > 0),
            "sorts below departures (0) and pet (-1), which sort ascending"
        );
        assert!(m.subscribes.contains(&StateKey::AudioSpectrum));
        assert!(m.subscribes.contains(&StateKey::SlotVisible));
        assert!(
            m.subscribes.contains(&StateKey::NowPlaying),
            "opts into the now-playing track (#528)"
        );
        assert_eq!(
            m.capabilities,
            vec![Capability::NowPlaying, Capability::Media],
            "the now-playing push is capability-gated on top of the subscription, \
             and the transport row's effects need the Media grant (#840)"
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
        let strip = kit::Marquee::new(DisplayStyle::Vfd)
            .window_px(window_px())
            .render(ACTIVE_MSG);
        assert!(strip.scrolls(), "the active banner scrolls");
    }

    /// A now-playing push with no timing — the pre-#840 shape, which the timing
    /// fields' `0` default still spells.
    fn track(title: &str, artist: &str, playing: bool) -> Input<Msg> {
        timed_track(title, artist, playing, 0, 0)
    }

    /// A now-playing push carrying a position and a length (#840).
    fn timed_track(
        title: &str,
        artist: &str,
        playing: bool,
        position_us: u64,
        length_us: u64,
    ) -> Input<Msg> {
        Input::NowPlaying(hytte_plugin::proto::NowPlaying {
            title: title.to_owned(),
            artist: artist.to_owned(),
            playing,
            position_us,
            length_us,
        })
    }

    /// The now-playing push (#528) takes over the marquee while playing — title
    /// and artist — and releases back to the banner when it stops.
    #[test]
    fn now_playing_drives_the_marquee_over_the_banner() {
        let mut m = shown();
        // A loud push would otherwise pick the active banner…
        let _ = m.update(spectrum(0.9, 0, 0.9));
        assert_eq!(m.marquee_text(), ACTIVE_MSG);
        // …but a playing track wins.
        m.update(track("Chrome Rain", "Choom", true));
        assert!(
            m.marquee_text().starts_with("Chrome Rain — Choom"),
            "playing track scrolls title — artist: {}",
            m.marquee_text()
        );
        // Title only (no artist).
        m.update(track("Untitled", "", true));
        assert!(m.marquee_text().starts_with("Untitled"));
        assert!(!m.marquee_text().contains('—'), "no dash without an artist");
        // Paused → back to the banner off the peak level.
        m.update(track("Chrome Rain", "Choom", false));
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
        let mut m = fresh(); // never shown → parked
        let before = m.view();
        m.update(track("Chrome Rain", "Choom", true));
        assert_eq!(
            m.view(),
            before,
            "a parked card ignores the now-playing push"
        );
    }

    /// `MM:SS` zero-pads both fields, counts an hour-plus track in plain minutes
    /// rather than rolling into an hours field, and saturates instead of widening
    /// or wrapping past the two-digit minute field (#840).
    #[test]
    fn mmss_pads_rolls_over_and_saturates() {
        assert_eq!(mmss(0), "00:00", "the start of a track");
        assert_eq!(mmss(9 * SEC), "00:09", "seconds zero-pad");
        assert_eq!(mmss(65 * SEC), "01:05", "minutes zero-pad");
        assert_eq!(mmss(599 * SEC), "09:59");
        // An hour-long track keeps counting minutes — no hours field.
        assert_eq!(mmss(3600 * SEC), "60:00", "an hour reads as 60 minutes");
        assert_eq!(mmss(3661 * SEC), "61:01");
        assert_eq!(mmss(5999 * SEC), "99:59", "the last exact value");
        // Past two digits it saturates, so the field never widens or wraps.
        assert_eq!(mmss(6000 * SEC), MAX_TIME, "100 minutes saturates");
        assert_eq!(mmss(u64::MAX), MAX_TIME, "and so does anything absurd");
        // Sub-second remainders truncate down, never round up into the next tick.
        assert_eq!(mmss(SEC - 1), "00:00");
        assert_eq!(mmss(2 * SEC - 1), "00:01");
    }

    /// The readout shows `elapsed/total`, stands `--:--` in for a duration the
    /// player doesn't report, and holds one fixed width either way — 11 chars, so
    /// the dot matrix is 268 px and the card never reflows (#840).
    #[test]
    fn the_readout_shows_elapsed_over_total() {
        let mut m = shown();
        assert_eq!(
            m.readout_text(),
            format!("00:00/{UNKNOWN_TIME}"),
            "an idle card reads zero over an unknown total"
        );

        // A player that reports no length keeps the placeholder while counting.
        m.update(timed_track("Chrome Rain", "Choom", true, 83 * SEC, 0));
        assert_eq!(m.readout_text(), format!("01:23/{UNKNOWN_TIME}"));

        // With a length, both halves are real times.
        m.update(timed_track(
            "Chrome Rain",
            "Choom",
            true,
            83 * SEC,
            296 * SEC,
        ));
        assert_eq!(m.readout_text(), "01:23/04:56");

        // Every state is the same width, which is what keeps the layout still.
        for (pos, len) in [
            (0, 0),
            (83 * SEC, 0),
            (83 * SEC, 296 * SEC),
            (u64::MAX, u64::MAX),
        ] {
            m.update(timed_track("t", "a", true, pos, len));
            assert_eq!(
                m.readout_text().chars().count(),
                11,
                "readout {:?} must stay 11 chars wide",
                m.readout_text()
            );
        }
    }

    /// Every char the readout can emit is font-covered — no notdef boxes in the
    /// time field (the digits, the `:`, the `/`, and the `--:--` placeholder).
    #[test]
    fn readout_chars_are_font_covered() {
        for c in "0123456789:/-".chars() {
            assert!(font::glyph(c).is_some(), "readout char {c:?} has a glyph");
        }
    }

    /// A click on a transport button asks the host to drive the active player
    /// (#840) — one `Effect::Media` per button, and nothing at all for a click on
    /// anything else or for a non-click event on a button.
    #[test]
    fn a_transport_click_asks_the_host_to_drive_the_player() {
        let mut m = shown();
        for (id, action) in [
            (PREV_ID, MediaAction::Previous),
            (PLAY_ID, MediaAction::PlayPause),
            (NEXT_ID, MediaAction::Next),
        ] {
            let fx = m.update(Input::Event {
                node: id.to_owned(),
                kind: EventKind::Click,
            });
            assert_eq!(fx, vec![Effect::Media(action)], "{id} drives {action:?}");
        }
        // The displays are click-inert…
        assert!(
            m.update(Input::Event {
                node: super::SCOPE_ID.to_owned(),
                kind: EventKind::Click,
            })
            .is_empty(),
            "a click on the scope asks for nothing"
        );
        // …and a non-click event on a button is not a press.
        assert!(
            m.update(Input::Event {
                node: PLAY_ID.to_owned(),
                kind: EventKind::Scroll { dx: 0.0, dy: 1.0 },
            })
            .is_empty(),
            "scrolling the play button is not a press"
        );
        // The mapping itself is total and refuses unknown ids.
        assert!(transport_action("nope").is_none());
    }

    /// The play-pause glyph mirrors the *player*, not the last click (#840): a
    /// push that says "playing" shows pause, and pausing from anywhere else
    /// flips it straight back.
    #[test]
    fn the_play_button_mirrors_the_player() {
        let mut m = shown();
        assert_eq!(m.play_icon(), ICON_PLAY, "nothing playing yet");
        m.update(track("Chrome Rain", "Choom", true));
        assert_eq!(m.play_icon(), ICON_PAUSE, "a playing track offers pause");
        // Clicking play-pause changes nothing on its own — the player answers.
        let _ = m.update(Input::Event {
            node: PLAY_ID.to_owned(),
            kind: EventKind::Click,
        });
        assert_eq!(
            m.play_icon(),
            ICON_PAUSE,
            "no optimistic flip: the glyph waits for the player"
        );
        m.update(track("Chrome Rain", "Choom", false));
        assert_eq!(m.play_icon(), ICON_PLAY, "and follows it when it pauses");
    }

    /// The scroll offset advances one step per frame tick and wraps the counter.
    #[test]
    fn the_marquee_scrolls_with_ticks() {
        let mut m = shown();
        let o0 = m.marquee_offset();
        tick(&mut m);
        assert_eq!(m.marquee_offset() - o0, MARQUEE_STEP, "one tick, one step");
    }

    /// The plugin's own scroll cadence and the rate it *states* for a
    /// shell-owned scroll are the same number (#884): [`MARQUEE_STEP`] dot per
    /// [`TICK`] is [`MARQUEE_SPEED_DPS`] dots per second, so the ticker moves at
    /// one visual rate whichever host is on the other end.
    ///
    /// Pinned because the two live in different constants and nothing else would
    /// notice them drifting apart — the raster arm never reads the speed, and
    /// the state arm never reads the step.
    #[test]
    fn the_two_scroll_rates_agree() {
        // In `f64` off integer sources, so the check itself carries no cast.
        let stated_per_tick = f64::from(MARQUEE_SPEED_DPS) * TICK.as_secs_f64();
        let step = u32::try_from(MARQUEE_STEP).expect("a small whole-dot step");
        assert!(
            (stated_per_tick - f64::from(step)).abs() < 1e-6,
            "the plugin steps {step} dot(s) per tick but tells the shell \
             {MARQUEE_SPEED_DPS} dots/s, which is {stated_per_tick} per tick",
        );
    }

    /// The migration's compat promise (#884): against a shell that does **not**
    /// advertise the preem vocabulary, the three migrated displays must reach
    /// the host as the exact same pixels the hand-written kit calls produced
    /// before the seam existed — byte for byte, at a **fixed advance sequence**,
    /// since the marquee's offset and the meters' ballistics are both time-driven.
    ///
    /// The sequence below is the card's real one: become visible, take a loud
    /// push (which attacks both meters and flips the banner to `ACTIVE_MSG`),
    /// then three frame ticks, which release the meters and walk the scroll
    /// three dots. Nothing here would hold if the config translation were even
    /// one field off.
    #[test]
    fn against_an_old_shell_the_three_displays_are_rasterised_kit_widgets() {
        let mut m = shown();
        let _ = m.update(timed_track(
            "Chrome Rain",
            "Choom",
            true,
            83 * SEC,
            214 * SEC,
        ));
        let _ = m.update(spectrum(0.9, 0, 0.9));
        for _ in 0..3 {
            tick(&mut m);
        }

        let tree = with_render_mode(RenderMode::Raster, || m.view().tree);
        let Node::Box { children, .. } = &tree else {
            panic!("the card root is a Box")
        };

        // The kit calls this card wrote by hand before #884, verbatim.
        let marquee = kit::Marquee::new(DisplayStyle::Vfd)
            .window_px(window_px())
            .render(&m.marquee_text())
            .window(m.marquee_offset())
            .into_node(Some(MARQUEE_ID), vec![]);
        let readout = kit::dot_matrix(&m.readout_text(), DisplayStyle::Vfd)
            .into_node(Some(READOUT_ID), vec![]);
        let leds = kit::LedStrip::new(DisplayStyle::Vfd)
            .render(m.level.value(), m.peak.value())
            .into_node(Some(LEDS_ID), vec![]);

        // `==` rather than `assert_eq!`: these carry a `Node::Pixels`, whose
        // `Debug` would dump the whole RGBA buffer into the failure output.
        assert!(
            children[0] == marquee,
            "the raster marquee must match the kit"
        );
        assert!(
            children[1] == readout,
            "the raster readout must match the kit"
        );
        assert!(
            children[3] == leds,
            "the raster LED strip must match the kit"
        );
        // …and the hand-drawn spectrum tile is untouched by the migration.
        assert!(
            children[2]
                == super::spectrum::scope_tile(&m.bins).into_node(Some(super::SCOPE_ID), vec![]),
            "the scope tile stays on the escape hatch",
        );
    }

    /// …and against a shell that advertises the vocabulary, the *same* `view`
    /// ships typed state nodes for those three — same ids, same readings, the
    /// skin as a name, the scroll rate stated rather than a position, and no
    /// pixels except the hand-drawn scope tile (#884).
    #[test]
    fn against_a_preem_shell_the_three_displays_are_state_nodes() {
        let mut m = shown();
        let _ = m.update(timed_track(
            "Chrome Rain",
            "Choom",
            true,
            83 * SEC,
            214 * SEC,
        ));
        let _ = m.update(spectrum(0.9, 0, 0.9));
        tick(&mut m);

        let tree = with_render_mode(RenderMode::State, || m.view().tree);
        let Node::Box { children, .. } = &tree else {
            panic!("the card root is a Box")
        };

        match preem(&children[0], MARQUEE_ID) {
            PreemWidget::Marquee { config, state } => {
                assert_eq!(state.text, m.marquee_text(), "the banner the shell scrolls");
                assert_eq!(config.window_px, MARQUEE_WINDOW_PX);
                assert!(
                    (config.speed_dots_per_sec - MARQUEE_SPEED_DPS).abs() < 1e-6,
                    "the wire carries a rate, and the shell owns the offset",
                );
                assert_eq!(config.style.style, StyleName::Vfd);
            }
            other => panic!("expected a marquee, got {other:?}"),
        }
        match preem(&children[1], READOUT_ID) {
            PreemWidget::DotMatrix { config, state } => {
                assert_eq!(state.text, m.readout_text(), "elapsed/total");
                assert_eq!(config.style.style, StyleName::Vfd);
            }
            other => panic!("expected a dot-matrix strip, got {other:?}"),
        }
        match preem(&children[3], LEDS_ID) {
            PreemWidget::LedStrip { config, state } => {
                assert!((state.level - m.level.value()).abs() < 1e-6, "the bar");
                assert_eq!(state.peak, Some(m.peak.value()), "the explicit dot");
                assert!(
                    config.peak_hold.is_none(),
                    "no shell-owned fall: both envelopes are the plugin's own",
                );
                assert_eq!(config.style.style, StyleName::Vfd);
            }
            other => panic!("expected an LED strip, got {other:?}"),
        }
        // The spectrum tile has no vocabulary word and stays pixels in both modes.
        assert!(
            matches!(children[2], Node::Pixels { .. }),
            "the hand-drawn scope tile is the escape hatch",
        );
    }

    /// The scroll the shell will own and the scroll the plugin runs are the same
    /// walk: after N ticks the marquee draws the window the raw counter selects,
    /// and a state-mode session never moves the plugin-side offset at all
    /// (`set_scroll_dots` is a no-op there) — so a reconnect to an old shell
    /// re-derives it from `frame` on the very next tick rather than resuming
    /// from a stale position.
    #[test]
    fn the_scroll_offset_is_raster_only_and_tracks_the_frame_counter() {
        let mut m = shown();
        with_render_mode(RenderMode::State, || {
            for _ in 0..5 {
                tick(&mut m);
            }
        });
        assert_eq!(m.marquee_offset(), 5, "the model still counts frames");
        assert_eq!(
            m.marquee.scroll_dots(),
            0,
            "but the shell owns the scroll, so nothing moved plugin-side",
        );

        // One raster tick re-states it from the counter — no drift carried over.
        tick(&mut m);
        assert_eq!(m.marquee.scroll_dots(), m.marquee_offset());
        assert_eq!(m.marquee.scroll_dots(), 6);
    }

    /// Every `Pixels` buffer honors the host's `len == w*h*4` seam and the ~296 px
    /// sidebar content width — across silent and loud states.
    ///
    /// Raster mode explicitly (#884): against a preem-speaking host three of the
    /// four displays carry no buffer at all, and this is the arm where the
    /// invariant has to hold.
    #[test]
    fn every_view_pixels_is_valid_and_fits_the_card() {
        let mut m = shown();
        for _ in 0..4 {
            let bufs = with_render_mode(RenderMode::Raster, || pixels_of(&m.view().tree));
            assert_eq!(bufs.len(), 4, "marquee + readout + scope + LED strip");
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
