//! `hytte-plugin-terminal` — a self-contained retro **micro-terminal** demo
//! (issue #357; the last piece of the #354 "preem widgets" breakout).
//!
//! It composes the two pieces that just landed on `main` — the `Node::Entry`
//! text-input vocabulary (#363) and the SDK's **preem raster kit** (#362) — into
//! one sidebar card: a preem VFD "screen" showing a scrollback of submitted
//! lines, with a single-line entry below it. Type a line, press **Enter** → the
//! line is appended to the screen (with a `> ` prompt), the screen scrolls when
//! it fills, and the entry clears (leaning on #357's clear-after-submit echo, so
//! rendering `text: ""` again reliably empties the widget).
//!
//! # Local echo only — a hard non-goal
//!
//! This is a **demo of `Entry` + `preem` composed**, nothing more. It is *pure
//! local echo*: a submitted line is only ever painted back onto the screen. It
//! **never** executes a shell command, spawns a process, or evaluates input —
//! there is no `std::process`, no `RunCommand` effect, no capability requested
//! at all. Running commands is explicitly out of scope and a security non-goal.
//! The `> ` prompt and the start-up banner are cosmetic flavor, not a shell.
//!
//! # Shape — The Elm Architecture, host-driven
//!
//! Like `hytte-plugin-preem-demo`, everything below is the pure TEA core with no
//! sources and no timers. The model is a capped scrollback [`Vec<String>`] plus
//! the entry's echo text; [`update`](Plugin::update) folds a
//! [`Submitted`](EventKind::Submitted) event into the scrollback and clears the
//! entry; [`view`](Plugin::view) renders the composed screen buffer + the entry.
//! The runtime's render dedup does the rest.
//!
//! # The screen — one `Node::Pixels`, composed from the kit
//!
//! The preem [`TextBox`] word-wraps a *single* text flow (its
//! [`wrap`](hytte_plugin::preem::font::wrap) collapses newlines), so a
//! multi-line screen can't be one `TextBox`. Instead each visible row is a
//! fixed-width one-line `TextBox` blitted into a single screen [`Frame`] via the
//! kit's [`Frame::blit`], then upscaled — so the whole screen ships as **one**
//! `Node::Pixels`, bottom-anchored (newest line sits just above the entry) with
//! a small bezel. Long lines are truncated to one row with a trailing `…` by the
//! `TextBox`'s own `max_lines(1)` — no hand-rolled wrapping.

use hytte_plugin::preem::{DisplayStyle, Frame, TextBox, font};
use hytte_plugin::proto::{Dir, Effect, EventKind, Manifest, Mount, Node};
use hytte_plugin::{CmdSender, Input, Plugin};

/// Stable plugin id — the host's mount-slot ownership key and audit subject.
const PLUGIN_ID: &str = "terminal";
/// Node ids. The entry is the submit target; the screen `Pixels` id makes each
/// re-render swap its texture in place instead of rebuilding the widget.
const ROOT_ID: &str = "terminal-root";
const SCREEN_ID: &str = "terminal-screen";
const ENTRY_ID: &str = "terminal-entry";

/// The screen skin: VFD (pale cyan on near-black) reads as a classic phosphor
/// terminal. Fixed — the terminal doesn't rotate skins.
const STYLE: DisplayStyle = DisplayStyle::Vfd;
/// Character columns per screen row. `line_px(22) = 131` px; with the bezel and
/// the ×2 upscale the screen is 274 px wide — inside the ~296 px sidebar card.
const COLS: usize = 22;
/// Visible rows, which is also the scrollback cap: the screen *is* the
/// scrollback (there's no scroll-back UI), so older lines fall off the top.
const ROWS: usize = 8;
/// Screen bezel around the text block, in pre-scale pixels.
const PAD: usize = 3;
/// Integer upscale baked into the screen buffer (chunky, crisp pixels).
const SCALE: usize = 2;
/// The entry's greyed placeholder — phrased to avoid implying execution.
const PLACEHOLDER: &str = "type a line, press enter…";

/// The micro-terminal's entire state — rebuilt fresh on every (re)connect.
#[derive(Debug, PartialEq, Eq)]
struct Terminal {
    /// The screen's scrollback, capped at [`ROWS`] lines (oldest dropped).
    history: Vec<String>,
    /// The entry's echo text. Only ever the empty string here — the plugin
    /// tracks no keystrokes (v1 has no per-keystroke event), it just re-asserts
    /// `""` after each submit so the widget clears.
    entry: String,
}

impl Terminal {
    /// The start-up banner — plain scrollback lines, honest about what this is.
    fn banner() -> Vec<String> {
        vec![
            "HYTTE MICRO-TERMINAL".to_owned(),
            "local echo - no exec".to_owned(),
            String::new(),
        ]
    }

    /// Append `line` to the scrollback, dropping the oldest rows so the history
    /// never exceeds [`ROWS`] — the screen's scroll behaviour.
    fn push_line(&mut self, line: String) {
        self.history.push(line);
        if self.history.len() > ROWS {
            let overflow = self.history.len() - ROWS;
            self.history.drain(..overflow);
        }
    }

    /// Fold a submitted entry into the model: echo it to the screen behind a
    /// `> ` prompt, then clear the entry. Pure local echo — nothing runs.
    fn submit(&mut self, text: &str) {
        self.push_line(format!("> {text}"));
        self.entry.clear();
    }
}

/// Compose the scrollback into one screen [`Frame`]: each visible row is a
/// fixed-width one-line [`TextBox`] blitted into a bezelled, bg-filled buffer,
/// bottom-anchored so the newest line rests just above the entry. Returns a
/// buffer that satisfies the host's `len == w * h * 4` invariant by kit
/// construction (every [`Frame`] does).
fn render_screen(history: &[String]) -> Frame {
    // Sample the skin's field color from an empty one-cell render, so the bezel
    // and the inter-row gaps fill with the same background as the rows without
    // reaching for the kit's crate-private palette.
    let bg = TextBox::styled(STYLE)
        .cols(1)
        .pad(0)
        .corner(0)
        .fixed_width(true)
        .render("")
        .get(0, 0)
        .unwrap_or([0, 0, 0, 0xff]);

    let width = 2 * PAD + font::line_px(COLS);
    let height = 2 * PAD + ROWS * font::GLYPH_H + (ROWS - 1) * font::LINE_GAP;
    let mut screen = Frame::filled(width, height, bg);

    // Bottom-anchor: with fewer than ROWS lines the blank rows sit on top.
    let visible = history.len().min(ROWS);
    let start = ROWS - visible;
    let x = i32::try_from(PAD).unwrap_or(0);
    for (i, line) in history.iter().take(ROWS).enumerate() {
        let row = TextBox::styled(STYLE)
            .cols(COLS)
            .max_lines(1)
            .pad(0)
            .corner(0)
            .fixed_width(true)
            .render(line);
        let row_y = PAD + (start + i) * (font::GLYPH_H + font::LINE_GAP);
        screen.blit(&row, x, i32::try_from(row_y).unwrap_or(0));
    }

    screen.upscale(SCALE)
}

impl Plugin for Terminal {
    /// Purely host-driven: submit events are the only input that changes state.
    type Msg = std::convert::Infallible;
    /// Purely local: no I/O of its own, no commands, no shell effects.
    type Cmd = std::convert::Infallible;

    /// Mounts a `SidebarTop` card (bar mounts are dropped by the host in v1, so
    /// the terminal must be a sidebar card to render). `order = 2` sits it after
    /// the preem-demo showcase. No subscriptions and no capabilities: the plugin
    /// only ever reacts to submit events on its own entry, and asks the shell
    /// for nothing.
    fn manifest() -> Manifest {
        Manifest::new(PLUGIN_ID, Mount::SidebarTop).with_order(2)
    }

    /// The banner screen + empty entry, rendered immediately so the card mounts
    /// before any interaction. `cmds` is unused (`Cmd = Infallible`).
    fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
        Self {
            history: Self::banner(),
            entry: String::new(),
        }
    }

    /// Fold one input into the model. The only state-changing input is a
    /// [`Submitted`](EventKind::Submitted) on our own entry; everything else is
    /// a no-op. Never returns an effect — this plugin drives nothing in the
    /// shell.
    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            Input::Event { node, kind } => {
                if node == ENTRY_ID
                    && let EventKind::Submitted { text } = kind
                {
                    self.submit(&text);
                }
            }
            // No subscriptions, no commands, no visibility gating — all no-ops.
            Input::Snapshot(_) | Input::EffectResult { .. } | Input::SlotVisible(_) => {}
            // `Msg = Infallible`: there are no app messages to receive.
            Input::App(never) => match never {},
        }
        Vec::new()
    }

    /// One vertical card: the composed preem screen over the entry line. The
    /// entry re-asserts `text: ""` every render so the host's clear-after-submit
    /// echo empties it after each Enter.
    fn view(&self) -> Node {
        Node::Box {
            id: Some(ROOT_ID.to_owned()),
            dir: Dir::Vertical,
            spacing: 6,
            scroll: false,
            classes: Vec::new(),
            children: vec![
                render_screen(&self.history).into_node(Some(SCREEN_ID), Vec::new()),
                Node::Entry {
                    id: ENTRY_ID.to_owned(),
                    text: self.entry.clone(),
                    placeholder: PLACEHOLDER.to_owned(),
                    classes: vec!["monospace".to_owned()],
                },
            ],
        }
    }
}

fn main() {
    hytte_plugin::run::<Terminal>();
}

#[cfg(test)]
mod tests {
    use super::{COLS, ENTRY_ID, PLACEHOLDER, ROWS, Terminal, render_screen};
    use hytte_plugin::proto::{EventKind, Node, PluginMsg, decode, encode};
    use hytte_plugin::{Input, Plugin};

    fn fresh() -> Terminal {
        Terminal::init(hytte_plugin::cmd_channel().0)
    }

    fn submit(text: &str) -> Input<std::convert::Infallible> {
        Input::Event {
            node: ENTRY_ID.to_owned(),
            kind: EventKind::Submitted {
                text: text.to_owned(),
            },
        }
    }

    /// Find the single `Entry` node in a view tree.
    fn entry_of(node: &Node) -> Option<(&str, &str)> {
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            match n {
                Node::Entry {
                    text, placeholder, ..
                } => return Some((text, placeholder)),
                Node::Box { children, .. } => stack.extend(children.iter()),
                _ => {}
            }
        }
        None
    }

    /// Collect every `Pixels` node's `(width, height, data-len)`.
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
                Node::Box { children, .. } => stack.extend(children.iter()),
                _ => {}
            }
        }
        out
    }

    /// A submit echoes the line behind a `> ` prompt and clears the entry.
    #[test]
    fn submit_echoes_with_prompt_and_clears_entry() {
        let mut m = fresh();
        // Pretend the widget had some in-flight echo text to clear.
        m.entry = "stale".to_owned();
        let fx = m.update(submit("hello world"));
        assert!(fx.is_empty(), "the terminal drives nothing in the shell");
        assert_eq!(m.history.last().map(String::as_str), Some("> hello world"));
        assert_eq!(m.entry, "", "the entry is cleared after a submit");
    }

    /// The scrollback caps at ROWS lines, dropping the oldest first.
    #[test]
    fn scrollback_caps_and_drops_oldest() {
        let mut m = fresh();
        m.history.clear();
        for i in 0..(ROWS + 5) {
            m.push_line(format!("line {i}"));
        }
        assert_eq!(m.history.len(), ROWS, "history is capped at ROWS");
        assert_eq!(
            m.history.first().map(String::as_str),
            Some("line 5"),
            "the five oldest lines fell off the top"
        );
        assert_eq!(
            m.history.last().map(String::as_str),
            Some(&*format!("line {}", ROWS + 4))
        );
    }

    /// The init banner is present and honest, and the entry starts empty with
    /// the placeholder hint.
    #[test]
    fn init_shows_banner_and_empty_entry() {
        let m = fresh();
        assert_eq!(
            m.history.first().map(String::as_str),
            Some("HYTTE MICRO-TERMINAL")
        );
        assert!(
            m.history.iter().any(|l| l.contains("no exec")),
            "honest banner"
        );
        let view = m.view();
        let (text, placeholder) = entry_of(&view).expect("view has an entry");
        assert_eq!(text, "", "the entry starts empty");
        assert_eq!(placeholder, PLACEHOLDER);
    }

    /// After a submit the view's entry re-asserts empty text (the
    /// clear-after-submit echo), and the echoed line is on the screen.
    #[test]
    fn view_entry_is_cleared_after_submit() {
        let mut m = fresh();
        let _ = m.update(submit("abc"));
        let view = m.view();
        let (text, _) = entry_of(&view).expect("view has an entry");
        assert_eq!(text, "", "the rendered entry text is empty after a submit");
        assert_eq!(m.history.last().map(String::as_str), Some("> abc"));
    }

    /// A submit on a node we don't own changes nothing.
    #[test]
    fn foreign_events_are_ignored() {
        let mut m = fresh();
        let before = m.history.clone();
        let fx = m.update(Input::Event {
            node: "not-ours".to_owned(),
            kind: EventKind::Submitted {
                text: "boom".to_owned(),
            },
        });
        assert!(fx.is_empty());
        assert_eq!(m.history, before, "a foreign submit is ignored");
    }

    /// The screen buffer honors the host's `len == w * h * 4` invariant and
    /// fits the ~296 px sidebar card, across an empty screen, a partial one, and
    /// an overflowing one with an over-long line.
    #[test]
    fn screen_is_valid_and_fits_the_card() {
        let cases: Vec<Vec<String>> = vec![
            Vec::new(),
            vec!["> hi".to_owned()],
            (0..(ROWS + 3))
                .map(|i| format!("> line number {i} is quite a bit wider than {COLS} columns"))
                .collect(),
        ];
        for history in cases {
            let f = render_screen(&history);
            assert_eq!(f.data().len(), f.width() * f.height() * 4);
            assert!(f.width() > 0 && f.height() > 0);
            assert!(f.width() <= 296, "screen width {} fits the card", f.width());
        }
    }

    /// The view is a pure function of the model, and every `Pixels` buffer in it
    /// is valid — one screen node, exactly.
    #[test]
    fn view_is_deterministic_with_one_valid_screen() {
        let mut m = fresh();
        let _ = m.update(submit("mrrp"));
        assert_eq!(m.view(), m.view(), "view is pure");
        let bufs = pixels_of(&m.view());
        assert_eq!(bufs.len(), 1, "exactly one screen buffer");
        for (w, h, len) in bufs {
            assert_eq!(len, (w as usize) * (h as usize) * 4);
            assert!(w > 0 && h > 0 && w <= 296);
        }
    }

    /// The Register and Render frames this plugin builds round-trip on the wire.
    #[test]
    fn register_and_render_frames_round_trip() {
        let reg = PluginMsg::Register {
            manifest: Terminal::manifest(),
        };
        let back: PluginMsg = decode(&encode(&reg)).expect("register frame decodes");
        assert_eq!(reg, back);

        let mut m = fresh();
        let _ = m.update(submit("hej"));
        let render = PluginMsg::Render {
            tree: m.view(),
            panel: m.panel(),
            effects: Vec::new(),
        };
        let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
        assert_eq!(render, back);
    }
}
