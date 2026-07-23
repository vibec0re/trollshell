//! `hytte-plugin-pet` — a tiny kaomoji cat for the trollshell sidebar 🐾
//! (issue #276, option 1: the sidebar companion).
//!
//! An out-of-process widget plugin on the `hytte-plugin` SDK (#279): pure
//! TEA — the model below, a 4-second tick, and a view of exactly one face
//! and (sometimes) one speech bubble, sharing a single horizontal row so the
//! card stays compact (#313). **Click the face to poke it.**
//!
//! # Face (`face.rs`)
//!
//! The face is a **procedural color LCD** (#284): each tick renders the mood's
//! expression + a blink cycle into a 128×128 RGBA8 buffer, drawn as a
//! [`Node::Pixels`] the host upscales nearest-neighbor for the chunky-pixel look
//! (bezel in CSS, `.pet-face` / `.pet-lcd`). The speech bubble is rendered the
//! same way — the voice speaks in a hand-rolled 5×7 pixel font (`font.rs`, #304),
//! so it comes out 8-bit chunky too. Set **`TROLLSHELL_PET_KAOMOJI=1`** to fall
//! back to the original kaomoji `Label` face *and* the plain-text `Label` bubble.
//!
//! # Moods
//!
//! Derived, never stored: poking excites it, poke-spam makes it grumpy
//! (it's a cat), night hours (from the shell's `Clock` subscription — the
//! only wire state it uses) make it sleepy, and while its brain is working
//! it looks pensive. Each mood has its own little frame loop.
//!
//! # The brain (`brain.rs`)
//!
//! Thoughts — poke reactions and rare idle musings — come from a local
//! `llama-server` (MiniCPM5-1B or anything else OpenAI-compatible on
//! localhost; see `etc/systemd/user/trollshell-pet-brain.service`), heavily
//! rate-limited, with **canned lines whenever the model is missing, slow,
//! or rate-limited** — the pet is fully functional with no LLM at all.
//! Following the shell's daemon-as-state-store stance, the model runs as
//! its own user service; this process is a thin client.
//!
//! Environment: `PET_NAME` (default `nisse`), `PET_LLM_URL` (default
//! `http://127.0.0.1:8080`; set empty to run canned-only).
//!
//! Note: the pet mounts `SidebarBottom` with `order = -1`, so it perches
//! **above** the departures board (which mounts the same region unordered,
//! sorting as 0) — the host's region machinery (#293) sorts co-mounted cards by
//! `(order, id)` ascending and a lower order renders higher (#303).

mod brain;
mod face;
mod font;

use std::time::Duration;

use brain::{ThinkKind, ThinkReq};
use hytte_plugin::proto::{Effect, EventKind, Manifest, Mount, Node, StateKey};
use hytte_plugin::tokio_stream::wrappers::UnboundedReceiverStream;
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View};
use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::IntervalStream;

/// Animation / housekeeping cadence.
const TICK: Duration = Duration::from_secs(4);
/// How many ticks a speech bubble lingers (~20 s).
const BUBBLE_TTL: u32 = 5;
/// Recent pokes at which the cat has HAD IT (voice and face agree).
pub(crate) const GRUMPY_AT: u32 = 5;
/// Idle-thought odds: 1 in this many ticks (~every 5 minutes).
const IDLE_ODDS: u64 = 75;
/// The face button's node id — the poke target.
const FACE_ID: &str = "pet-face";

/// Messages from the pet's own sources (the tick stream + the brain).
#[derive(Debug)]
pub(crate) enum PetMsg {
    /// Animation/housekeeping heartbeat.
    Tick,
    /// The brain produced one bubble line.
    Thought(String),
}

/// The pet's whole head.
struct Pet {
    /// Local hour 0..=23, from clock snapshots.
    hour: u8,
    /// Monotone animation frame counter (wrapped per mood in `view`).
    frame: usize,
    /// Current speech bubble and its remaining ticks.
    bubble: Option<(String, u32)>,
    /// A brain request is in flight (pensive face until the thought lands).
    thinking: bool,
    /// Recent pokes; decays over ticks.
    recent_pokes: u32,
    /// Tick counter (drives poke decay).
    ticks: u64,
    /// xorshift state for the idle-thought dice.
    rng: u64,
    /// The command lane to the brain task (#280): `poke`/`think` enqueue a
    /// [`ThinkReq`] here from `update`, and the task the pet's `sources` spawn
    /// drains it. The runtime owns the channel per session.
    cmd_tx: CmdSender<ThinkReq>,
    /// Render the legacy kaomoji `Label` face instead of the LCD, when
    /// `TROLLSHELL_PET_KAOMOJI=1`. Read once at init so `view` stays pure.
    kaomoji_fallback: bool,
}

/// The pet's disposition — derived from state, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mood {
    Happy,
    Sleepy,
    Excited,
    Grumpy,
    Thinking,
}

impl Mood {
    /// The mood as a prompt/CSS word.
    fn word(self) -> &'static str {
        match self {
            Self::Happy => "happy",
            Self::Sleepy => "sleepy",
            Self::Excited => "excited",
            Self::Grumpy => "grumpy",
            Self::Thinking => "thinking",
        }
    }
}

/// The frame loop for each mood.
fn frames(mood: Mood) -> &'static [&'static str] {
    match mood {
        Mood::Happy => &["(=^･ω･^=)", "(=^‥^=)∫", "(=^･ω･^=)ﾉ", "(=^‥^=)"],
        Mood::Sleepy => &["(=￣ω￣=)", "(=￣ω￣=) z", "(=-ω-=) zZ", "(=-ω-=) zzZ"],
        Mood::Excited => &["ヽ(=^･ω･^=)ﾉ", "ฅ(=^･ω･^=)ฅ", "＼(=^‥^=)／"],
        Mood::Grumpy => &["(=｀ω´=)", "(=｀ｪ´=)", "(=￢_￢=)"],
        Mood::Thinking => &["(=・_・=)?", "(=・.・=)…", "(=・_・=)…?"],
    }
}

/// Night hours (bedtime cat).
fn is_night(hour: u8) -> bool {
    !(7..23).contains(&hour)
}

/// The hour out of an RFC 3339 local timestamp (`2026-07-11T15:49:00+02:00`).
fn parse_hour(iso: &str) -> Option<u8> {
    let h: u8 = iso.get(11..13)?.parse().ok()?;
    (h <= 23).then_some(h)
}

impl Pet {
    /// Disposition ignoring the in-flight-thought overlay — what the pet
    /// *feels* (and what the brain prompt should say).
    fn base_mood(&self) -> Mood {
        if self.recent_pokes >= GRUMPY_AT {
            Mood::Grumpy
        } else if self.recent_pokes >= 1 {
            Mood::Excited
        } else if is_night(self.hour) {
            Mood::Sleepy
        } else {
            Mood::Happy
        }
    }

    /// Disposition as shown: a working brain looks pensive.
    fn mood(&self) -> Mood {
        if self.thinking {
            Mood::Thinking
        } else {
            self.base_mood()
        }
    }

    /// One xorshift step; true with 1-in-`odds` probability.
    fn roll(&mut self, odds: u64) -> bool {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x.is_multiple_of(odds)
    }

    /// Ask the brain for a line (fire-and-forget; it always answers, canned
    /// if it must). Only marks `thinking` if the brain is actually there.
    fn think(&mut self, kind: ThinkKind) {
        let req = ThinkReq {
            kind,
            hour: self.hour,
            mood: self.base_mood().word(),
            pokes: self.recent_pokes,
        };
        if self.cmd_tx.send(req).is_ok() {
            self.thinking = true;
        }
    }

    /// A click on the face. While a thought is already in flight the poke
    /// still registers on the mood, but no new request queues up — stale
    /// replies would only clobber the fresh line (and it's a cat: when it's
    /// busy, it ignores you).
    fn poke(&mut self) {
        self.recent_pokes = self.recent_pokes.saturating_add(1);
        if !self.thinking {
            self.think(ThinkKind::Poke);
        }
    }

    /// The 4-second heartbeat: animate, decay, expire, occasionally muse.
    fn tick(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
        self.frame = self.frame.wrapping_add(1);
        if self.ticks.is_multiple_of(2) {
            self.recent_pokes = self.recent_pokes.saturating_sub(1);
        }
        if let Some((_, ttl)) = &mut self.bubble {
            *ttl -= 1;
            if *ttl == 0 {
                self.bubble = None;
            }
        }
        if self.bubble.is_none() && !self.thinking && self.roll(IDLE_ODDS) {
            self.think(ThinkKind::Idle);
        }
    }
}

impl Plugin for Pet {
    type Msg = PetMsg;
    /// Outbound: the pet asks its own brain for lines — that's plugin I/O, not
    /// a shell effect, so it rides the command lane (#280), not `update`'s
    /// return.
    type Cmd = ThinkReq;

    fn manifest() -> Manifest {
        // SidebarBottom, `order = -1`: the pet perches *above* the departures
        // board, which mounts the same region with no order (sorts as 0) — the
        // region sorts `(order, id)` ascending, lower renders higher (#303).
        let mut m = Manifest::new("pet", Mount::SidebarBottom).with_order(-1);
        m.subscribes = vec![StateKey::Clock];
        m
    }

    fn init(cmds: CmdSender<Self::Cmd>) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            hour: 12,
            frame: 0,
            bubble: None,
            thinking: false,
            recent_pokes: 0,
            ticks: 0,
            rng: (now.as_secs() ^ u64::from(now.subsec_nanos())).max(1),
            cmd_tx: cmds,
            kaomoji_fallback: std::env::var("TROLLSHELL_PET_KAOMOJI")
                .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")),
        }
    }

    fn sources(cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        // The brain task owns both directions of the pet's own I/O: it drains
        // `cmds` (the command lane, filled from `update`) and re-emits each
        // reply as a `PetMsg::Thought` on the app-message stream below.
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        tokio::spawn(brain::brain(cmds, msg_tx));
        let ticks = IntervalStream::new(tokio::time::interval(TICK)).map(|_| PetMsg::Tick);
        let thoughts = UnboundedReceiverStream::new(msg_rx);
        Some(Box::pin(ticks.merge(thoughts)))
    }

    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            Input::App(PetMsg::Tick) => self.tick(),
            Input::App(PetMsg::Thought(line)) => {
                self.thinking = false;
                if !line.is_empty() {
                    self.bubble = Some((line, BUBBLE_TTL));
                }
            }
            Input::Snapshot(snapshot) => {
                if let Some(h) = snapshot.clock.and_then(|c| parse_hour(&c.iso)) {
                    self.hour = h;
                }
            }
            Input::Event { node, kind } => {
                if node == FACE_ID && matches!(kind, EventKind::Click) {
                    self.poke();
                }
            }
            // EffectResult is unused (no RunCommand is issued), and the cat keeps
            // purring whether or not you're looking (its ticks are cheap and its
            // liveliness is the point) — so it ignores the sidebar-visibility push
            // (#288) rather than napping while hidden. Both are no-ops.
            Input::EffectResult { .. } | Input::SlotVisible(_) => {}
        }
        Vec::new()
    }

    fn view(&self) -> View {
        let mood = self.mood();
        // The face: an LCD `Pixels` surface by default, or the legacy kaomoji
        // `Label` under `TROLLSHELL_PET_KAOMOJI=1`. Either way it is the same
        // `Button(FACE_ID)` poke target — clicks are unchanged.
        let face_child = if self.kaomoji_fallback {
            let loops = frames(mood);
            let kao = loops[self.frame % loops.len()];
            Node::Label {
                id: None,
                text: kao.to_owned(),
                classes: vec!["pet-kao".to_owned()],
            }
        } else {
            Node::Pixels {
                id: Some("pet-lcd".to_owned()),
                width: face::SIZE_U32,
                height: face::SIZE_U32,
                data: face::render(mood, self.frame),
                // 1×: the shell's `.pet-lcd` CSS px rule still owns the on-screen
                // size (the #358 `scale` hint is for plugins without one).
                scale: 1,
                classes: vec!["pet-lcd".to_owned()],
            }
        };
        let mut children = vec![Node::Button {
            id: FACE_ID.to_owned(),
            classes: vec!["pet-face".to_owned(), format!("pet-mood-{}", mood.word())],
            child: Box::new(face_child),
        }];
        if let Some((line, _)) = &self.bubble {
            // The voice speaks in chunky pixel-font by default (#304), or as the
            // old kaomoji `Label` under `TROLLSHELL_PET_KAOMOJI=1`. Both keep the
            // `pet-bubble` class (the Label styles from it; the Pixels honors its
            // layout — the host's PixelSurface ignores CSS backgrounds).
            let bubble = if self.kaomoji_fallback {
                Node::Label {
                    id: None,
                    text: line.clone(),
                    classes: vec!["pet-bubble".to_owned()],
                }
            } else {
                font::bubble_node(line, "pet-bubble", vec!["pet-bubble".to_owned()])
            };
            children.push(bubble);
        }
        // The face and the bubble share one horizontal row (#313): the face
        // leads at its natural 128 px (a `Row` packs children at their natural
        // width, so the LCD no longer fills the ~296 px card and aspect-locks
        // into a ~300 px tower), and the bubble sits to its right in the
        // remaining slot. With no bubble the row collapses to just the face —
        // the leading face stays put either way.
        Node::Row {
            id: Some("pet-root".to_owned()),
            classes: vec!["pet-root".to_owned()],
            children,
        }
        .into()
    }
}

fn main() {
    hytte_plugin::run::<Pet>()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pet with a fixed seed + a probe on its brain-request channel. The
    /// command lane is normally the runtime's to build; here the test plays
    /// that role with [`cmd_channel`](hytte_plugin::cmd_channel), keeping the
    /// receiver to assert what `update` dispatched.
    fn pet() -> (Pet, CmdReceiver<ThinkReq>) {
        let (tx, rx) = hytte_plugin::cmd_channel();
        let mut p = Pet::init(tx);
        p.rng = 42;
        p.hour = 12;
        (p, rx)
    }

    #[test]
    fn hour_drives_the_resting_mood() {
        let (mut p, _rx) = pet();
        assert_eq!(p.mood(), Mood::Happy);
        p.hour = 2;
        assert_eq!(p.mood(), Mood::Sleepy);
        p.hour = 23;
        assert_eq!(p.mood(), Mood::Sleepy);
        p.hour = 7;
        assert_eq!(p.mood(), Mood::Happy);
    }

    #[test]
    fn snapshot_updates_the_hour() {
        let (mut p, _rx) = pet();
        let _ = p.update(Input::Snapshot(hytte_plugin::proto::StateSnapshot {
            clock: Some(hytte_plugin::proto::ClockState {
                iso: "2026-07-11T23:12:00+02:00".to_owned(),
                unix: 0,
            }),
        }));
        assert_eq!(p.hour, 23);
        assert_eq!(p.mood(), Mood::Sleepy);
    }

    #[test]
    fn parse_hour_rejects_nonsense() {
        assert_eq!(parse_hour("2026-07-11T09:00:00+02:00"), Some(9));
        assert_eq!(parse_hour("2026-07-11T99:00:00+02:00"), None);
        assert_eq!(parse_hour("short"), None);
        assert_eq!(parse_hour(""), None);
    }

    #[test]
    fn poke_requests_a_thought_and_looks_pensive() {
        let (mut p, mut rx) = pet();
        let fx = p.update(Input::Event {
            node: FACE_ID.to_owned(),
            kind: EventKind::Click,
        });
        assert!(fx.is_empty(), "the pet asks nothing of the shell");
        let req = rx.try_recv().expect("a brain request");
        assert_eq!(req.kind, ThinkKind::Poke);
        assert_eq!(req.mood, "excited");
        assert_eq!(p.mood(), Mood::Thinking);
    }

    #[test]
    fn poke_spam_turns_grumpy() {
        let (mut p, mut rx) = pet();
        let mut last = None;
        for _ in 0..GRUMPY_AT {
            p.poke();
            // The brain answers between pokes, so the next poke may ask again.
            if let Ok(req) = rx.try_recv() {
                last = Some(req);
                let _ = p.update(Input::App(PetMsg::Thought("!".to_owned())));
            }
        }
        let last = last.expect("requests were sent");
        assert_eq!(last.mood, "grumpy", "the final ask carries the spam mood");
        assert_eq!(last.pokes, GRUMPY_AT);
        p.thinking = false;
        assert_eq!(p.mood(), Mood::Grumpy);
    }

    #[test]
    fn a_thought_fills_the_bubble_then_expires() {
        let (mut p, _rx) = pet();
        let _ = p.update(Input::App(PetMsg::Thought("mrrp!".to_owned())));
        assert!(!p.thinking);
        assert_eq!(p.bubble.as_ref().map(|(s, _)| s.as_str()), Some("mrrp!"));
        let mut texts = Vec::new();
        for _ in 0..BUBBLE_TTL {
            texts.push(p.bubble.is_some());
            let _ = p.update(Input::App(PetMsg::Tick));
        }
        assert!(texts.iter().all(|t| *t), "bubble lives out its TTL");
        assert!(p.bubble.is_none(), "then it pops");
    }

    #[test]
    fn excitement_decays_back_to_the_resting_mood() {
        let (mut p, mut rx) = pet();
        p.poke();
        let _ = rx.try_recv();
        let _ = p.update(Input::App(PetMsg::Thought("!".to_owned())));
        assert_eq!(p.mood(), Mood::Excited);
        for _ in 0..4 {
            let _ = p.update(Input::App(PetMsg::Tick));
            // Swallow any idle thought so `thinking` never sticks.
            if rx.try_recv().is_ok() {
                let _ = p.update(Input::App(PetMsg::Thought(String::new())));
            }
        }
        assert_eq!(p.mood(), Mood::Happy);
    }

    #[test]
    fn frames_animate_and_wrap() {
        let (mut p, mut rx) = pet();
        let first = frames(p.mood())[p.frame % frames(p.mood()).len()];
        let _ = p.update(Input::App(PetMsg::Tick));
        if rx.try_recv().is_ok() {
            let _ = p.update(Input::App(PetMsg::Thought(String::new())));
        }
        let second = frames(p.mood())[p.frame % frames(p.mood()).len()];
        assert_ne!(first, second, "the face moves");
    }

    #[test]
    fn idle_thoughts_eventually_happen() {
        let (mut p, mut rx) = pet();
        let mut idles = 0;
        for _ in 0..2000 {
            let _ = p.update(Input::App(PetMsg::Tick));
            if let Ok(req) = rx.try_recv() {
                assert_eq!(req.kind, ThinkKind::Idle);
                idles += 1;
                // Answer so the pet goes back to musing eligibility.
                let _ = p.update(Input::App(PetMsg::Thought(String::new())));
            }
        }
        assert!(idles > 0, "a fixed seed muses at least once in 2000 ticks");
        assert!(idles < 200, "but not constantly ({idles})");
    }

    #[test]
    fn view_is_a_pokeable_face_with_optional_bubble() {
        let (mut p, _rx) = pet();
        // The root is a horizontal `Row` (#313): face-button first, bubble
        // second only when a line is up, absent (row collapses) at rest.
        let Node::Row { children, .. } = p.view().tree else {
            panic!("root is a row");
        };
        assert_eq!(children.len(), 1, "no bubble at rest");
        assert!(
            matches!(&children[0], Node::Button { id, .. } if id == FACE_ID),
            "the face leads the row and is the poke target"
        );
        let _ = p.update(Input::App(PetMsg::Thought("hej".to_owned())));
        let Node::Row { children, .. } = p.view().tree else {
            panic!("root is a row");
        };
        assert_eq!(children.len(), 2);
        // The default bubble is a chunky pixel-font Pixels surface (#304), whose
        // buffer honors the host's `len == w*h*4` invariant.
        let Node::Pixels {
            width,
            height,
            data,
            classes,
            ..
        } = &children[1]
        else {
            panic!("default bubble is a Pixels surface");
        };
        assert_eq!(
            data.len(),
            *width as usize * *height as usize * 4,
            "bubble buffer must satisfy the host's len == w*h*4 seam"
        );
        assert!(classes.iter().any(|c| c == "pet-bubble"));
    }

    #[test]
    fn the_kaomoji_fallback_bubble_is_a_plain_label() {
        let (mut p, _rx) = pet();
        p.kaomoji_fallback = true;
        let _ = p.update(Input::App(PetMsg::Thought("hej".to_owned())));
        let Node::Row { children, .. } = p.view().tree else {
            panic!("root is a row");
        };
        assert_eq!(children.len(), 2);
        assert!(
            matches!(&children[1], Node::Label { text, .. } if text == "hej"),
            "the fallback bubble label carries the thought"
        );
    }

    #[test]
    fn face_is_an_lcd_by_default_and_a_kaomoji_under_the_fallback() {
        // Default: the face child is a 128×128 RGBA8 Pixels surface whose buffer
        // honors the host's `len == w*h*4` invariant.
        let (mut p, _rx) = pet();
        p.kaomoji_fallback = false;
        let Node::Row { children, .. } = p.view().tree else {
            panic!("root is a row");
        };
        let Node::Button { id, child, .. } = &children[0] else {
            panic!("face is a button");
        };
        assert_eq!(id, FACE_ID, "the LCD keeps the same poke target");
        let Node::Pixels {
            width,
            height,
            data,
            ..
        } = &**child
        else {
            panic!("default face child is a Pixels surface");
        };
        assert_eq!((*width, *height), (128, 128));
        assert_eq!(
            data.len(),
            128 * 128 * 4,
            "buffer must satisfy the host's len == w*h*4 seam"
        );

        // Fallback: the kaomoji Label returns, still inside Button(FACE_ID).
        let (mut p, _rx2) = pet();
        p.kaomoji_fallback = true;
        let Node::Row { children, .. } = p.view().tree else {
            panic!("root is a row");
        };
        let Node::Button { child, .. } = &children[0] else {
            panic!("face is a button");
        };
        assert!(
            matches!(&**child, Node::Label { .. }),
            "fallback face is the kaomoji label"
        );
    }

    #[test]
    fn pokes_while_thinking_register_but_do_not_queue_requests() {
        let (mut p, mut rx) = pet();
        p.poke();
        assert!(rx.try_recv().is_ok(), "first poke asks the brain");
        p.poke();
        p.poke();
        assert!(
            rx.try_recv().is_err(),
            "further pokes while thinking queue nothing"
        );
        assert_eq!(p.recent_pokes, 3, "but they still count for the mood");
    }

    #[test]
    fn foreign_nodes_do_not_poke() {
        let (mut p, mut rx) = pet();
        let _ = p.update(Input::Event {
            node: "not-the-pet".to_owned(),
            kind: EventKind::Click,
        });
        assert!(rx.try_recv().is_err());
        assert_eq!(p.recent_pokes, 0);
    }
}
