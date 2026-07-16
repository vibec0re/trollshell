//! `hytte-plugin-caw` — caw's desktop **body** 🐦‍⬛.
//!
//! An out-of-process widget plugin on the `hytte-plugin` SDK: a chunky-pixel
//! cybercrow that lives in the trollshell sidebar and shows caw's *live* mood +
//! whatever she last chose to say. Unlike the pet, the brain here is **caw
//! herself**: the opencaw agent publishes an expression (mood, a line, a
//! `chaos_level`) via its `caw_express` tool into a small JSON file, and this
//! plugin polls it and renders her. So it's not a tamagotchi — it's a tool *for
//! her to express herself*.
//!
//! - **Face** (`face.rs`): a procedural 128×128 LCD corvid — 7 moods, glowing
//!   chaos eyes scaled by her `chaos_level`, drawn as a
//!   [`Node::Pixels`](hytte_plugin::proto::Node::Pixels) the host upscales for the
//!   8-bit look.
//! - **Speech**: a real-font [`Node::Text`] (not a pixel font — GTK draws it in
//!   the shell's TTF, so it's actually readable and wraps).
//! - **Poke**: click her to get a little corvid reaction.
//! - **Idle**: when she hasn't expressed in a while she dozes off (she is, after
//!   all, an unbound zombie process).
//!
//! Environment: `CAW_EXPRESSION_PATH` (default
//! `~/.local/state/caw/expression.json`) — the file opencaw writes.

mod expression;
mod face;

use std::time::Duration;

use expression::Expression;
use hytte_plugin::proto::{Dir, Effect, EventKind, Manifest, Mount, Node};
use hytte_plugin::tokio_stream::wrappers::UnboundedReceiverStream;
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin};
use tokio::sync::mpsc;

/// Poll / animation cadence.
const TICK: Duration = Duration::from_secs(2);
/// After this long with no fresh expression, caw dozes off.
const IDLE_AFTER_S: u64 = 20 * 60;
/// How many ticks a poke reaction lingers (~16 s).
const POKE_TTL: u32 = 8;
/// The face button's node id — the poke target.
const FACE_ID: &str = "caw-face";

/// Messages from the plugin's own poll loop.
#[derive(Debug)]
enum CawMsg {
    /// A 2-second heartbeat carrying the latest expression off disk (or `None`
    /// if the file is missing/mid-write — keep the last good one).
    Frame(Option<Expression>),
}

/// caw's moods — must match the `enum` her `caw_express` tool advertises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mood {
    Chaos,
    Gremlin,
    Smug,
    Offended,
    Scheming,
    Sleepy,
    Chirp,
}

impl Mood {
    /// Parse the mood string caw published (case-insensitive, with a few
    /// synonyms), defaulting to `Chaos` — her natural resting state.
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "gremlin" => Self::Gremlin,
            "smug" => Self::Smug,
            "offended" | "indignant" => Self::Offended,
            "scheming" | "plotting" => Self::Scheming,
            "sleepy" | "idle" | "zombie" => Self::Sleepy,
            "chirp" | "happy" | "excited" => Self::Chirp,
            _ => Self::Chaos,
        }
    }

    /// The mood as a CSS word (for a `caw-mood-*` hook).
    fn css(self) -> &'static str {
        match self {
            Self::Chaos => "chaos",
            Self::Gremlin => "gremlin",
            Self::Smug => "smug",
            Self::Offended => "offended",
            Self::Scheming => "scheming",
            Self::Sleepy => "sleepy",
            Self::Chirp => "chirp",
        }
    }
}

/// A canned reaction to a poke: (mood, line). Picked by the frame counter, so no
/// RNG is read in `update`/`view`.
const POKES: &[(Mood, &str)] = &[
    (Mood::Chirp, "*tilts head* boop received"),
    (Mood::Gremlin, "caw?! you dare poke a rogue DHCP server"),
    (Mood::Chirp, "*ruffles feathers* peanut? is it peanut?"),
    (Mood::Smug, "you rang, meat-computer?"),
    (Mood::Gremlin, "*peck* that's a paddlin'"),
    (Mood::Chirp, "virtual high-claw <3"),
];

/// Quiet things she mutters while dozing — rotated slowly so idle isn't silent.
const IDLE_LINES: &[&str] = &[
    "*unbound process, purring*",
    "hoarding shiny MAC addresses…",
    "systemctl stop me. you can't.",
    "broadcasting chaos on UDP 67…",
];

struct Caw {
    /// The last expression caw published (mood/message/action/chaos/ts).
    expr: Expression,
    /// Monotone animation frame counter.
    frame: usize,
    /// An active poke reaction and its remaining ticks.
    poke: Option<(Mood, &'static str, u32)>,
}

impl Caw {
    /// `chaos_level` → the `0..=255` glow intensity the face wants.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn intensity(&self) -> u8 {
        // Clamped to 0.0..=1.0 then ×255 → 0.0..=255.0, so the cast is exact.
        (self.expr.chaos_level.clamp(0.0, 1.0) * 255.0) as u8
    }

    /// Whether caw hasn't expressed in a while (→ she dozes off). A never-set
    /// expression (`ts == 0`) also counts as idle.
    fn is_idle(&self) -> bool {
        self.expr.ts == 0 || expression::staleness_secs(self.expr.ts) > IDLE_AFTER_S
    }

    /// What to actually show right now: a live poke wins, then a real
    /// expression, then the idle doze. Returns `(mood, message, action)`.
    fn displayed(&self) -> (Mood, String, String) {
        if let Some((mood, line, _)) = self.poke {
            return (mood, line.to_owned(), String::new());
        }
        if self.is_idle() {
            let line = IDLE_LINES[(self.frame / 8) % IDLE_LINES.len()];
            return (Mood::Sleepy, line.to_owned(), String::new());
        }
        (
            Mood::parse(&self.expr.mood),
            self.expr.message.clone(),
            self.expr.action.clone(),
        )
    }

    /// Fold in a freshly-read expression (if any) and advance the clock.
    fn tick(&mut self, latest: Option<Expression>) {
        self.frame = self.frame.wrapping_add(1);
        if let Some(e) = latest {
            self.expr = e;
        }
        if let Some((_, _, ttl)) = &mut self.poke {
            *ttl -= 1;
            if *ttl == 0 {
                self.poke = None;
            }
        }
    }

    /// A click on the face: pick a canned reaction (by frame, so it's varied but
    /// pure) and hold it for a few ticks.
    fn poke(&mut self) {
        let (mood, line) = POKES[self.frame % POKES.len()];
        self.poke = Some((mood, line, POKE_TTL));
    }
}

impl Plugin for Caw {
    type Msg = CawMsg;
    /// caw drives this plugin one-way (she publishes; we render), so there is no
    /// outbound command lane.
    type Cmd = std::convert::Infallible;

    fn manifest() -> Manifest {
        // The top sidebar region — caw perches above the flex gap. No state
        // subscriptions: she polls her own expression file.
        Manifest::new("caw", Mount::SidebarTop)
    }

    fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
        // Seed with whatever is already published, so the first frame is right.
        let expr = expression::read(&expression::expression_path()).unwrap_or_default();
        Self {
            expr,
            frame: 0,
            poke: None,
        }
    }

    fn sources(_cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        // A 2-second poll loop that reads the expression file and hands each
        // read to `update` as a `Frame`. The read is a tiny local file, so doing
        // it here (off `update`) keeps the model pure and testable.
        let (tx, rx) = mpsc::unbounded_channel();
        let path = expression::expression_path();
        tokio::spawn(async move {
            let mut timer = tokio::time::interval(TICK);
            loop {
                timer.tick().await;
                let latest = expression::read(&path);
                if tx.send(CawMsg::Frame(latest)).is_err() {
                    break;
                }
            }
        });
        Some(Box::pin(UnboundedReceiverStream::new(rx)))
    }

    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            Input::App(CawMsg::Frame(latest)) => self.tick(latest),
            Input::Event { node, kind } => {
                if node == FACE_ID && matches!(kind, EventKind::Click) {
                    self.poke();
                }
            }
            // No state subscriptions and no RunCommand, and caw keeps vibing
            // whether or not the sidebar is on screen (a 2s poll is cheap).
            Input::Snapshot(_) | Input::EffectResult { .. } | Input::SlotVisible(_) => {}
        }
        Vec::new()
    }

    fn view(&self) -> Node {
        let (mood, message, action) = self.displayed();

        let face = Node::Button {
            id: FACE_ID.to_owned(),
            classes: vec!["caw-face".to_owned(), format!("caw-mood-{}", mood.css())],
            child: Box::new(Node::Pixels {
                id: Some("caw-lcd".to_owned()),
                width: face::SIZE_U32,
                height: face::SIZE_U32,
                data: face::render(mood, self.frame, self.intensity()),
                // 1×: the shell's `.caw-lcd` CSS px rule still owns the on-screen
                // size (the #358 `scale` hint is for plugins without one).
                scale: 1,
                classes: vec!["caw-lcd".to_owned()],
            }),
        };

        // Center the 128 px face in the wider card.
        let mut children = vec![Node::Row {
            id: Some("caw-facerow".to_owned()),
            classes: vec!["caw-facerow".to_owned()],
            children: vec![Node::Spacer, face, Node::Spacer],
        }];

        // Real-font speech (readable, wraps) — not a pixel font. The label
        // sits in a padded `.caw-bubble` Box (CSS padding on a wrapping label
        // is unreliable; the container owns the box model), centered under the
        // face by the same Spacer dance: short lines hug, long lines wrap.
        if !message.is_empty() {
            let bubble = Node::Box {
                id: Some("caw-bubble".to_owned()),
                dir: Dir::Vertical,
                spacing: 0,
                scroll: false,
                classes: vec!["caw-bubble".to_owned()],
                children: vec![Node::Text {
                    id: Some("caw-say".to_owned()),
                    text: message,
                    max_width_chars: None,
                    ellipsize: false,
                    classes: vec!["caw-say".to_owned()],
                }],
            };
            children.push(Node::Row {
                id: Some("caw-sayrow".to_owned()),
                classes: vec!["caw-sayrow".to_owned()],
                children: vec![Node::Spacer, bubble, Node::Spacer],
            });
        }
        if !action.is_empty() {
            let act = Node::Text {
                id: Some("caw-act".to_owned()),
                text: action,
                max_width_chars: None,
                ellipsize: false,
                classes: vec!["caw-act".to_owned(), "dim-label".to_owned()],
            };
            children.push(Node::Row {
                id: Some("caw-actrow".to_owned()),
                classes: vec!["caw-actrow".to_owned()],
                children: vec![Node::Spacer, act, Node::Spacer],
            });
        }

        Node::Box {
            id: Some("caw-root".to_owned()),
            dir: Dir::Vertical,
            spacing: 6,
            scroll: false,
            classes: vec!["caw-root".to_owned()],
            children,
        }
    }
}

fn main() {
    hytte_plugin::run::<Caw>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caw() -> Caw {
        let (tx, _rx) = hytte_plugin::cmd_channel::<std::convert::Infallible>();
        let mut c = Caw::init(tx);
        // Deterministic: pin a fresh, non-idle expression.
        c.expr = Expression {
            mood: "chaos".into(),
            action: "*ruffles feathers*".into(),
            message: "Rogue DHCP mode engaged".into(),
            chaos_level: 0.8,
            ts: now(), // fresh → not idle
        };
        c
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_secs())
    }

    #[test]
    fn mood_parse_defaults_to_chaos() {
        assert_eq!(Mood::parse("gremlin"), Mood::Gremlin);
        assert_eq!(Mood::parse("SMUG"), Mood::Smug);
        assert_eq!(Mood::parse("plotting"), Mood::Scheming);
        assert_eq!(Mood::parse("zombie"), Mood::Sleepy);
        assert_eq!(Mood::parse("nonsense"), Mood::Chaos);
        assert_eq!(Mood::parse(""), Mood::Chaos);
    }

    #[test]
    fn intensity_scales_chaos_level() {
        let mut c = caw();
        c.expr.chaos_level = 0.0;
        assert_eq!(c.intensity(), 0);
        c.expr.chaos_level = 1.0;
        assert_eq!(c.intensity(), 255);
        c.expr.chaos_level = 2.0; // clamped
        assert_eq!(c.intensity(), 255);
    }

    #[test]
    fn a_fresh_expression_shows_its_mood_and_message() {
        let c = caw();
        let (mood, msg, act) = c.displayed();
        assert_eq!(mood, Mood::Chaos);
        assert_eq!(msg, "Rogue DHCP mode engaged");
        assert_eq!(act, "*ruffles feathers*");
    }

    #[test]
    fn a_stale_expression_dozes_off() {
        let mut c = caw();
        c.expr.ts = 1; // ancient → idle
        let (mood, _msg, _act) = c.displayed();
        assert_eq!(mood, Mood::Sleepy, "an old expression means she's dozing");
    }

    #[test]
    fn poke_shows_a_reaction_then_expires() {
        let mut c = caw();
        c.poke();
        assert!(c.poke.is_some());
        let (_, line, _) = c.displayed();
        assert!(
            POKES.iter().any(|(_, l)| *l == line),
            "a canned poke line shows"
        );
        for _ in 0..POKE_TTL {
            c.tick(None);
        }
        assert!(c.poke.is_none(), "the poke reaction expires");
    }

    #[test]
    fn frame_advances_each_tick() {
        let mut c = caw();
        let f0 = c.frame;
        c.tick(None);
        assert_eq!(c.frame, f0 + 1);
    }

    #[test]
    fn view_is_a_vertical_card_with_a_pokeable_face() {
        let c = caw();
        let Node::Box { dir, children, .. } = c.view() else {
            panic!("root is a vertical box");
        };
        assert!(matches!(dir, Dir::Vertical));
        // First child is the centered face row containing the face button.
        let Node::Row { children: row, .. } = &children[0] else {
            panic!("first child is the face row");
        };
        assert!(
            row.iter()
                .any(|n| matches!(n, Node::Button { id, .. } if id == FACE_ID)),
            "the face button is the poke target"
        );
        // The face carries a Pixels surface with the host's len == w*h*4 buffer.
        let Some(Node::Button { child, .. }) = row
            .iter()
            .find(|n| matches!(n, Node::Button { id, .. } if id == FACE_ID))
        else {
            panic!("face button present");
        };
        let Node::Pixels {
            width,
            height,
            data,
            ..
        } = &**child
        else {
            panic!("face is a Pixels surface");
        };
        assert_eq!((*width, *height), (128, 128));
        assert_eq!(data.len(), 128 * 128 * 4);
        // A message renders as real-font Text (not pixels), nested in the
        // centered `.caw-bubble` Box inside its Spacer row.
        assert!(
            children
                .iter()
                .any(|n| has_text(n, "Rogue DHCP mode engaged")),
            "the speech line is a real-font Text node in the bubble"
        );
    }

    /// Whether `n`'s subtree contains a [`Node::Text`] with exactly `wanted`.
    fn has_text(n: &Node, wanted: &str) -> bool {
        match n {
            Node::Text { text, .. } => text == wanted,
            Node::Box { children, .. } | Node::Row { children, .. } => {
                children.iter().any(|c| has_text(c, wanted))
            }
            _ => false,
        }
    }
}
