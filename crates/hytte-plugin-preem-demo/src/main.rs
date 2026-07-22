//! `hytte-plugin-preem-demo` — the showcase plugin for the SDK's **preem
//! raster kit** (issue #356; `hytte_plugin::preem`).
//!
//! One sidebar card cycling every kit widget through every skin: a
//! seven-segment **HH:MM clock**, a **dot-matrix ticker** stepping one char
//! per second, a **scrolling marquee** panning a pixel window across a
//! pre-rendered strip, and an **8bit textbox**, all rotating VFD → LCD → OLED
//! every [`STYLE_SECS`] seconds. Tapping the clock advances the skin
//! immediately. It doubles as the kit's visual regression harness and the
//! copy-from reference for plugin authors.
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
//! 22-column ×2 textbox 274 px — all inside the sidebar card's ~296 px content
//! width.

use hytte_plugin::preem::{DisplayStyle, Marquee, TextBox, dot_matrix, seven_seg};
use hytte_plugin::proto::{Dir, Effect, EventKind, Manifest, Mount, Node, StateKey};
use hytte_plugin::{CmdSender, Input, Plugin};

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

/// Seconds each skin holds before the rotation advances.
const STYLE_SECS: i64 = 10;
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
#[derive(Debug, PartialEq, Eq)]
struct PreemDemo {
    /// `"HH:MM"` from the host clock's ISO timestamp (`"--:--"` until the
    /// first snapshot lands — all-ghost dashes on the readout).
    hhmm: String,
    /// Latest unix seconds; drives the ticker step and the skin rotation.
    unix: i64,
    /// Click offset into the skin rotation (tapping the clock bumps it).
    style_bump: u32,
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

    /// Subscribes to `Clock`, mounts `SidebarTop` under the clock demo
    /// (`order = 1`; unordered co-mounts sort as 0 — #303). No capabilities:
    /// the demo asks nothing of the shell.
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, Mount::SidebarTop).with_order(1);
        m.subscribes = vec![StateKey::Clock];
        m
    }

    /// All-ghost placeholders until the first snapshot lands (the runtime
    /// renders this seed immediately, so the card mounts right away).
    fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
        Self {
            hhmm: "--:--".to_owned(),
            unix: 0,
            style_bump: 0,
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
            }
            // Tapping the clock advances the skin rotation by one.
            Input::Event { node, kind } => {
                if node == CYCLE_BTN && matches!(kind, EventKind::Click) {
                    self.style_bump = self.style_bump.wrapping_add(1);
                }
            }
            // No RunCommand is issued, and the card keeps cycling whether
            // or not anyone is looking (rendering is snapshot-driven and
            // cheap) — both pushes are no-ops.
            Input::EffectResult { .. } | Input::SlotVisible(_) => {}
            // `Msg = Infallible`: there are no app messages to receive.
            Input::App(never) => match never {},
        }
        Vec::new()
    }

    /// One vertical card: the pokeable 7seg clock, the ticker, the scrolling
    /// marquee, the textbox — all wearing the same skin — and a dim hint line.
    /// Every `Pixels` buffer satisfies the host's `len == w * h * 4` invariant
    /// by kit construction.
    fn view(&self) -> Node {
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
                Node::Label {
                    id: None,
                    text: "tap the clock to switch skins".to_owned(),
                    classes: vec!["dim-label".to_owned()],
                },
            ],
        }
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
        ClockState, EventKind, Node, PluginMsg, StateSnapshot, decode, encode,
    };
    use hytte_plugin::{Input, Plugin};

    fn fresh() -> PreemDemo {
        PreemDemo::init(hytte_plugin::cmd_channel().0)
    }

    fn snapshot(iso: &str, unix: i64) -> Input<std::convert::Infallible> {
        Input::Snapshot(StateSnapshot {
            clock: Some(ClockState {
                iso: iso.to_owned(),
                unix,
            }),
        })
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
            let tree = m.view();
            let bufs = pixels_of(&tree);
            assert_eq!(bufs.len(), 4, "clock + ticker + marquee + textbox");
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
        let render = PluginMsg::Render {
            tree: m.view(),
            panel: m.panel(),
            effects: Vec::new(),
        };
        let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
        assert_eq!(render, back);
    }
}
