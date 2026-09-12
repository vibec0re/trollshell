//! `hytte-plugin-niri-layouts` — three one-click column layouts for the focused
//! niri workspace (issue #1019).
//!
//! `equal`, `golden` and `split` set the widths of the columns on the workspace
//! you are looking at, in one click or one keybind. niri does the rest: it owns
//! the gaps, the animation and the scrolling.
//!
//! # Two hats, one binary
//!
//! - **No arguments** → an ordinary out-of-process trollshell widget plugin: a
//!   [`Mount::BarRight`](hytte_plugin::proto::Mount::BarRight) chip of three
//!   Adwaita symbolic buttons, one per layout, shown **on each screen** only
//!   while that screen's active workspace holds more than one window (#1050).
//!   See [`plugin`] and [`watch`].
//! - **`apply <equal|golden|split>`** → apply that layout and exit, so the very
//!   same binary is a niri `spawn` bind with no shell involved:
//!
//!   ```text
//!   Mod+Shift+E { spawn "hytte-plugin-niri-layouts" "apply" "equal"; }
//!   ```
//!
//! That is free because the plugin talks to `$NIRI_SOCKET` **itself** rather
//! than routing widths through the shell. The wire's `Effect::Niri` only knows
//! `FocusWorkspace` / `FocusWindow`, so the shell route would have cost a proto
//! bump, a capability and a host handler for an action the shell itself never
//! needs — and it would have made the standalone hat impossible. Both hats call
//! the same [`niri::apply`], so they can never disagree about what a layout
//! means. Trust boundary is unchanged: a plugin already runs in the user's own
//! session and could always have spoken to niri.
//!
//! # Columns, not windows
//!
//! niri widths are **per column** — a stacked column has one width, and
//! `SetWindowWidth` on any of its tiles sets the whole column. So `n` below is
//! the number of columns: three windows stacked in one column beside one other
//! window is `n = 2`. (Issue #1019 question 3; this is the answer built here.)
//!
//! Windows are grouped by `Window.layout.pos_in_scrolling_layout` on the active
//! workspace of the **target** output — the screen whose chip was clicked
//! (#1050), or the focused one for a keybind, which is the only target the CLI
//! hat has. Floating windows are skipped, and so is
//! anything with no position in the scrolling layout — which is how a
//! fullscreen window reports itself. Each column gets one request, addressed to
//! its first tile, left to right. No tiled columns → no requests at all.
//!
//! # The three layouts, and the three answers
//!
//! | layout | proportion per column |
//! | --- | --- |
//! | `equal` | `1/n` each (so `n = 1` is full width) |
//! | `golden` | first `0.75`, **every other one** `0.25` — or `0.618` / `0.382` (the golden ratio) on a screen narrower than 2560 logical px (#1052) |
//! | `split` | `0.5` for every column |
//!
//! Those are **fractions**, which is the unit [`layout`] thinks in. niri's
//! `SizeChange::SetProportion` is a **percentage** of the working area, so
//! `niri::percent` scales at the one seam before the socket — `0.5` goes on the
//! wire as `50.0`, exactly what `niri msg action set-window-width 50%` writes.
//! Getting that wrong is invisible on glass (niri clamps a sub-1 % request to
//! the window's minimum width, so every layout still "resizes the columns"),
//! which is why the wire number is pinned against niri's own contract rather
//! than against the constants above.
//!
//! The triage on #1019 put three questions to Annika and built its own defaults
//! meanwhile; she answered all three ("scroll off to the side / n1 / preem"),
//! then looked at the result on glass and sent it back for a second round
//! (2026-09-10). What stands after both:
//!
//! - **Golden reads as A**: the first column takes its share and every column
//!   after it takes the narrow one, so a third and later column *scroll off to
//!   the right* — which is what the issue's `[====] [==] ( .... ) [==]` sketch
//!   draws. The **shares are 75/25** on Annika's ultrawide, not the 61.8/38.2
//!   the first cut derived from φ and not the 70/30 the round after it carried
//!   for a day: "hmm no choom was thinking more like 75 : 25 I guess",
//!   `[ wide 75% ] [ narrow ]`.
//!   [`Layout::proportions`](layout::Layout::proportions) is the only place any
//!   proportion is decided **for a given pair**; [`layout::golden_pair`] is the
//!   only place that pair is picked. #1052 (2026-09-10, "Can we make this
//!   adaptive?") restored 61.8/38.2 for screens under 2560 logical px, since
//!   the flat 75/25 left the narrow column too cramped to use on a laptop or
//!   1080p/1440p monitor — the number is per-screen, not a single global
//!   answer any more.
//! - **Three inline glyph buttons** on the chip, not one chip opening a panel.
//!   [`plugin`]'s `chip()` is the only place the arrangement lives; the button
//!   ids `update` keys off would carry over to a panel unchanged.
//! - **Adwaita symbolic icons**, not the preem pictograms round 1 built:
//!   "Using preem for icons ultra gonk idé typ. Was misunderstanding in the
//!   first place. […] Looks shit. Adwaita icons fine." Her original "preem <3"
//!   was agreement with the *column-counting* answer, not a request to
//!   rasterise the glyphs. [`Layout::icon`](layout::Layout::icon) holds the
//!   three names and the reason for each.
//! - **The chip hides below two windows**: "Only show when more than 1 window
//!   in workspace." There is no host niri state topic, so [`watch`] keeps the
//!   count itself off a second `$NIRI_SOCKET` connection — **per output**
//!   since #1050, so screen B's chip follows screen B's workspace rather than
//!   whichever screen holds keyboard focus. `View::hidden_on` carries that to
//!   the host, and a click carries its own screen back.
//!
//! Counting **columns rather than windows** was the third question as the triage
//! asked it, and stands as written — note that it is deliberately *not* the same
//! count as [`watch`]'s: a stacked column of two windows is one column to
//! [`layout::plan`] and two windows to the visibility rule.

mod cli;
mod layout;
mod niri;
mod plugin;
mod watch;

use clap::Parser as _;
use cli::{BIN, Cli, Command};
use plugin::{NiriLayouts, apply_from_cli};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(e) => return exit_for_parse_error(&e),
    };
    match cli.command {
        // The plugin hat: no subcommand. `run` never returns: it owns the
        // process from here, dialing the host socket and reconnecting
        // forever.
        None => hytte_plugin::run::<NiriLayouts>(),
        // The standalone hat. Same `apply` the chip's click worker runs (via
        // `apply_from_cli`, which is where the "a keybind names no screen"
        // decision lives and is tested), so a keybind and a click cannot drift.
        Some(Command::Apply { layout }) => {
            match apply_from_cli(&mut niri::SocketTransport, layout.into()) {
                // niri's own text, verbatim — the CLI has no toast to put it in.
                Some(error) => {
                    eprintln!("{BIN}: {error}");
                    ExitCode::FAILURE
                }
                None => ExitCode::SUCCESS,
            }
        }
        Some(Command::Completions { shell }) => {
            print!("{}", cli::render_completions(shell));
            ExitCode::SUCCESS
        }
    }
}

/// clap's own exit code for a usage error is 2; this CLI's contract predates
/// clap and treats any misuse as a plain failure — exit 1 — so map it down
/// rather than let switching parsers move the number a script might check.
/// `--help`/`--version`/`help` (clap's "display" error kinds) keep their 0.
fn exit_for_parse_error(e: &clap::Error) -> ExitCode {
    let _ = e.print();
    if e.exit_code() == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
