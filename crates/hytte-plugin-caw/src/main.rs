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
//! - **Speech** (`speech.rs`): rendered in the **preem** raster kit's pixel
//!   font (#368) — a [`Node::Pixels`] in caw's violet palette, so her line
//!   reads like the rest of her (the LCD face, the VFD/dot-matrix screens)
//!   instead of the shell's TTF. Her stage direction (`action`) stays a dim
//!   italic real-font whisper for hierarchy.
//! - **Poke**: click her to get a little corvid reaction.
//! - **Idle**: when she hasn't expressed in a while she dozes off (she is, after
//!   all, an unbound zombie process).
//! - **Morning briefing** (#407, `briefing.rs`/`ingredients.rs`): once a day
//!   caw caws the news — weather + the first useful departure (calendar once
//!   the host shares it), composed through [`hytte_ai_providers`] in her voice
//!   (or a plain template, keyless), delivered sticky in the bubble until
//!   poked and mirrored as a toast
//!   ([`Effect::Notify`](hytte_plugin::proto::Effect::Notify)).
//!
//! Environment: `CAW_EXPRESSION_PATH` (default
//! `~/.local/state/caw/expression.json`) — the file opencaw writes; plus the
//! briefing knobs `CAW_BRIEFING_TIME` / `CAW_LLM_URL` / `CAW_LLM_MODEL` /
//! `CAW_LLM_API_KEY` (see `briefing.rs` and the systemd unit's comments).

mod briefing;
mod expression;
mod face;
mod ingredients;
mod speech;

use std::time::Duration;

use expression::Expression;
use hytte_plugin::proto::{Capability, Dir, Effect, EventKind, Manifest, Mount, Node};
use hytte_plugin::tokio_stream::wrappers::UnboundedReceiverStream;
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View};
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
    /// Today's composed morning briefing (#407) and the unix second it landed
    /// (the reference for "a fresher expression takes over").
    Briefing { text: String, at_unix: u64 },
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
    /// Today's morning briefing (#407): the composed text and the unix second
    /// it landed. Sticky until poked — or until caw herself publishes a
    /// fresher expression (her live voice always outranks old news; this
    /// plugin exists for *her* to express herself).
    briefing: Option<(String, u64)>,
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

    /// What to actually show right now: a live poke wins, then a sticky
    /// morning briefing (#407), then a real expression, then the idle doze.
    /// Returns `(mood, message, action, is_briefing)` — the last flag picks the
    /// taller briefing bubble in `view`.
    fn displayed(&self) -> (Mood, String, String, bool) {
        if let Some((mood, line, _)) = self.poke {
            return (mood, line.to_owned(), String::new(), false);
        }
        if let Some((text, _)) = &self.briefing {
            return (
                Mood::Chirp,
                text.clone(),
                "*caws the morning news*".to_owned(),
                true,
            );
        }
        if self.is_idle() {
            let line = IDLE_LINES[(self.frame / 8) % IDLE_LINES.len()];
            return (Mood::Sleepy, line.to_owned(), String::new(), false);
        }
        (
            Mood::parse(&self.expr.mood),
            self.expr.message.clone(),
            self.expr.action.clone(),
            false,
        )
    }

    /// Fold in a freshly-read expression (if any) and advance the clock.
    fn tick(&mut self, latest: Option<Expression>) {
        self.frame = self.frame.wrapping_add(1);
        if let Some(e) = latest {
            self.expr = e;
        }
        // caw's live voice outranks the news: an expression published *after*
        // the briefing landed retires it (a pre-briefing one never does).
        if let Some((_, at)) = &self.briefing
            && self.expr.ts > *at
        {
            self.briefing = None;
        }
        if let Some((_, _, ttl)) = &mut self.poke {
            *ttl -= 1;
            if *ttl == 0 {
                self.poke = None;
            }
        }
    }

    /// A click on the face: dismiss a sticky briefing (that's its ack, #407)
    /// and pick a canned reaction (by frame, so it's varied but pure) to hold
    /// for a few ticks.
    fn poke(&mut self) {
        self.briefing = None;
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
        // subscriptions: she polls her own expression file. `Notify` (#406)
        // mirrors the morning briefing (#407) as a toast, so the news lands
        // even with the sidebar closed.
        let mut m = Manifest::new("caw", Mount::SidebarTop);
        m.capabilities = vec![Capability::Notify];
        m
    }

    fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
        // Seed with whatever is already published, so the first frame is right.
        let expr = expression::read(&expression::expression_path()).unwrap_or_default();
        Self {
            expr,
            frame: 0,
            poke: None,
            briefing: None,
        }
    }

    fn sources(_cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        // A 2-second poll loop that reads the expression file and hands each
        // read to `update` as a `Frame`. The read is a tiny local file, so doing
        // it here (off `update`) keeps the model pure and testable.
        //
        // The same heartbeat doubles as the morning-briefing trigger (#407):
        // when the configured local time comes due (once per date, stamped
        // through to disk *before* composing so nothing can re-caw), the
        // blocking gather + compose runs on a `spawn_blocking` thread and the
        // result re-enters `update` as a `Briefing`. The expression poll simply
        // waits out that once-a-day await.
        let (tx, rx) = mpsc::unbounded_channel();
        let path = expression::expression_path();
        tokio::spawn(async move {
            let cfg = briefing::Cfg::from_env();
            let mut stamp = briefing::Stamp::load();
            let mut timer = tokio::time::interval(TICK);
            loop {
                timer.tick().await;
                let latest = expression::read(&path);
                if tx.send(CawMsg::Frame(latest)).is_err() {
                    break;
                }
                let Some(at_mins) = cfg.time else { continue };
                let now = chrono::Local::now();
                if !briefing::is_due(
                    briefing::minutes_of_day(&now),
                    now.date_naive(),
                    at_mins,
                    stamp.last(),
                ) {
                    continue;
                }
                stamp.mark(now.date_naive());
                let provider = cfg.provider.clone();
                let text =
                    tokio::task::spawn_blocking(move || briefing::brief_now(provider.as_ref()))
                        .await
                        .unwrap_or_else(|e| {
                            eprintln!("[caw] briefing task failed: {e}");
                            "the briefing crashed mid-caw. classic.".to_owned()
                        });
                let at_unix = expression::now_unix();
                if tx.send(CawMsg::Briefing { text, at_unix }).is_err() {
                    break;
                }
            }
        });
        Some(Box::pin(UnboundedReceiverStream::new(rx)))
    }

    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            Input::App(CawMsg::Frame(latest)) => self.tick(latest),
            Input::App(CawMsg::Briefing { text, at_unix }) => {
                self.briefing = Some((text.clone(), at_unix));
                // Mirror the news as a toast (#407): the sidebar is usually
                // closed at 07:00, and trollshell is the notification daemon.
                return vec![Effect::Notify {
                    summary: "caw's morning news".to_owned(),
                    body: text,
                }];
            }
            Input::Event { node, kind } => {
                if node == FACE_ID && matches!(kind, EventKind::Click) {
                    self.poke();
                }
            }
            // No state subscriptions and no RunCommand, and caw keeps vibing
            // whether or not the sidebar is on screen (a 2s poll is cheap).
            Input::Snapshot(_)
            | Input::EffectResult { .. }
            | Input::SlotVisible(_)
            | Input::AudioSpectrum(_)
            | Input::ConsentDecision { .. } => {}
        }
        Vec::new()
    }

    fn view(&self) -> View {
        let (mood, message, action, is_briefing) = self.displayed();

        // `Frame::into_node` bakes the `Node::Pixels` (id/width/height/data and
        // the `scale: 1` the preem kit always wants) so caw can't re-introduce
        // the hand-set `scale` that red-carded main in #364. The shell's
        // `.caw-lcd` CSS px rule still owns the on-screen size.
        let face = Node::Button {
            id: FACE_ID.to_owned(),
            classes: vec!["caw-face".to_owned(), format!("caw-mood-{}", mood.css())],
            child: Box::new(
                face::render(mood, self.frame, self.intensity())
                    .into_node(Some("caw-lcd"), vec!["caw-lcd".to_owned()]),
            ),
        };

        // Center the 128 px face in the wider card.
        let mut children = vec![Node::Row {
            id: Some("caw-facerow".to_owned()),
            classes: vec!["caw-facerow".to_owned()],
            children: vec![Node::Spacer, face, Node::Spacer],
        }];

        // Preem pixel-font speech (#368): her line is a `Node::Pixels` in
        // caw's violet palette (see `speech.rs`), not a TTF label — so it reads
        // like her LCD face. Centered under the face by the same Spacer dance;
        // the box hugs short lines and wraps long ones (capped, `…`-marked).
        // The morning briefing (#407) rides the taller briefing box (and an
        // extra `caw-briefing` class hook) so the news fits uncut.
        if !message.is_empty() {
            let bubble = if is_briefing {
                speech::briefing_node(
                    &message,
                    "caw-say",
                    vec!["caw-say".to_owned(), "caw-briefing".to_owned()],
                )
            } else {
                speech::speech_node(&message, "caw-say", vec!["caw-say".to_owned()])
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
        .into()
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
        c.briefing = None;
        c
    }

    const NEWS: &str = "morning, meat-computer. 3° rain, high 8°. S9 in 12 — move, choom.";

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
        let (mood, msg, act, briefing) = c.displayed();
        assert_eq!(mood, Mood::Chaos);
        assert_eq!(msg, "Rogue DHCP mode engaged");
        assert_eq!(act, "*ruffles feathers*");
        assert!(!briefing);
    }

    #[test]
    fn a_stale_expression_dozes_off() {
        let mut c = caw();
        c.expr.ts = 1; // ancient → idle
        let (mood, _msg, _act, _) = c.displayed();
        assert_eq!(mood, Mood::Sleepy, "an old expression means she's dozing");
    }

    #[test]
    fn poke_shows_a_reaction_then_expires() {
        let mut c = caw();
        c.poke();
        assert!(c.poke.is_some());
        let (_, line, _, _) = c.displayed();
        assert!(
            POKES.iter().any(|(_, l)| *l == line),
            "a canned poke line shows"
        );
        for _ in 0..POKE_TTL {
            c.tick(None);
        }
        assert!(c.poke.is_none(), "the poke reaction expires");
    }

    // ── The morning briefing (#407) ──────────────────────────────────────────

    #[test]
    fn briefing_lands_sticky_and_mirrors_as_a_toast() {
        let mut c = caw();
        let fx = c.update(Input::App(CawMsg::Briefing {
            text: NEWS.to_owned(),
            at_unix: now(),
        }));
        // The toast mirror (#414's Effect::Notify) rides the same frame.
        assert!(
            matches!(
                fx.as_slice(),
                [Effect::Notify { summary, body }]
                    if summary == "caw's morning news" && body == NEWS
            ),
            "got {fx:?}"
        );
        let (mood, msg, act, briefing) = c.displayed();
        assert_eq!(mood, Mood::Chirp);
        assert_eq!(msg, NEWS);
        assert_eq!(act, "*caws the morning news*");
        assert!(briefing, "the view picks the taller briefing bubble");
        // Sticky: heartbeats (even past the idle horizon) don't clear it.
        for _ in 0..16 {
            c.tick(None);
        }
        assert_eq!(c.displayed().1, NEWS, "still cawing the news");
    }

    #[test]
    fn briefing_outranks_the_idle_doze() {
        let mut c = caw();
        c.expr.ts = 1; // ancient → she'd be dozing
        let _ = c.update(Input::App(CawMsg::Briefing {
            text: NEWS.to_owned(),
            at_unix: now(),
        }));
        let (mood, msg, _, _) = c.displayed();
        assert_eq!(mood, Mood::Chirp, "news beats the doze");
        assert_eq!(msg, NEWS);
    }

    #[test]
    fn poke_dismisses_the_briefing() {
        let mut c = caw();
        let _ = c.update(Input::App(CawMsg::Briefing {
            text: NEWS.to_owned(),
            at_unix: now(),
        }));
        let fx = c.update(Input::Event {
            node: FACE_ID.to_owned(),
            kind: EventKind::Click,
        });
        assert!(fx.is_empty(), "the ack is silent — no second toast");
        assert!(c.briefing.is_none(), "poked = read");
        // The poke reaction shows, and after it expires she's back to normal.
        for _ in 0..POKE_TTL {
            c.tick(None);
        }
        assert_eq!(c.displayed().1, "Rogue DHCP mode engaged");
    }

    #[test]
    fn a_fresher_expression_retires_the_briefing() {
        let mut c = caw();
        let briefed_at = now();
        let _ = c.update(Input::App(CawMsg::Briefing {
            text: NEWS.to_owned(),
            at_unix: briefed_at,
        }));
        // The same pre-briefing expression re-read every 2s does NOT clear it…
        c.tick(Some(Expression {
            ts: briefed_at.saturating_sub(60),
            ..c.expr.clone()
        }));
        assert!(c.briefing.is_some(), "old chatter can't bury the news");
        // …but a line she publishes *after* the briefing takes over: this
        // plugin is her voice first, a news desk second.
        c.tick(Some(Expression {
            mood: "smug".into(),
            message: "already read it, choom".into(),
            ts: briefed_at + 5,
            ..Expression::default()
        }));
        assert!(c.briefing.is_none());
        assert_eq!(c.displayed().1, "already read it, choom");
    }

    #[test]
    fn manifest_mounts_sidebar_top_and_requests_notify() {
        let m = Caw::manifest();
        assert_eq!(m.id, "caw");
        assert_eq!(m.mount, Mount::SidebarTop);
        assert!(
            m.subscribes.is_empty(),
            "caw polls her own file and opts into no host push (#305)"
        );
        assert_eq!(
            m.capabilities,
            vec![Capability::Notify],
            "the briefing toast (#407) is her only ask of the host"
        );
        m.check_proto()
            .expect("stamped with the current proto version");
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
        let Node::Box { dir, children, .. } = c.view().tree else {
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
        // Her message now renders in the preem pixel font (#368): a `caw-say`
        // Pixels node with a valid host buffer, centered in its Spacer row —
        // not a real-font Text label.
        let say = find_pixels(&children, "caw-say").expect("the speech is a Pixels node");
        let Node::Pixels {
            width: sw,
            height: sh,
            data: sd,
            ..
        } = say
        else {
            unreachable!("find_pixels only returns Pixels")
        };
        assert!(*sw > 0 && *sh > 0, "the speech buffer is non-degenerate");
        assert_eq!(
            sd.len(),
            *sw as usize * *sh as usize * 4,
            "the speech buffer satisfies the host's len == w*h*4"
        );
        // Her stage direction stays a dim italic real-font whisper (only the
        // spoken line went preem).
        assert!(
            children.iter().any(|n| has_text(n, "*ruffles feathers*")),
            "the action is still a real-font Text node"
        );
    }

    /// The first [`Node::Pixels`] with `id` anywhere under `nodes`.
    fn find_pixels<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Node> {
        let mut stack: Vec<&Node> = nodes.iter().collect();
        while let Some(n) = stack.pop() {
            match n {
                Node::Pixels { id: Some(i), .. } if i == id => return Some(n),
                Node::Box { children, .. } | Node::Row { children, .. } => {
                    stack.extend(children.iter());
                }
                Node::Button { child, .. } => stack.push(child),
                _ => {}
            }
        }
        None
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
