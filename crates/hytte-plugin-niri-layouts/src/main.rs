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
//!   glyph buttons, one per layout. See [`plugin`].
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
//! workspace of the focused output. Floating windows are skipped, and so is
//! anything with no position in the scrolling layout — which is how a
//! fullscreen window reports itself. Each column gets one request, addressed to
//! its first tile, left to right. No tiled columns → no requests at all.
//!
//! # The three layouts, and which #1019 question each answers
//!
//! | layout | proportion per column | question |
//! | --- | --- | --- |
//! | `equal` | `1/n` each (so `n = 1` is full width) | — |
//! | `golden` | first `0.618`, **every other one** `0.382` | **Q1, reading A** |
//! | `split` | `0.5` for every column | — |
//!
//! **Q1 (golden)** is answered with reading **A**: the first column takes 61.8 %
//! and every column after it takes 38.2 %, so a third and later column scroll
//! off to the right — which is what the issue's `[====] [==] ( .... ) [==]`
//! sketch draws. Reading B (the narrow columns *share* the remaining 38.2 % so
//! everything stays on screen) is a one-line change to
//! [`Layout::proportions`](layout::Layout::proportions), which is the only place
//! any proportion is decided.
//!
//! **Q2 (chip shape)** is answered with three inline glyph buttons, per the
//! triage. A single chip opening a panel would be a change to
//! [`plugin`]'s `chip()` alone; the button ids `update` keys off would carry
//! over unchanged.
//!
//! **Q3 (stacked columns)** is answered "columns", as above.
//!
//! None of the three is settled by Annika yet — each is built as the triage's
//! stated default, and each is deliberately confined to one function.

mod cli;
mod layout;
mod niri;
mod plugin;

use cli::{BIN, Invocation, USAGE};
use plugin::{NiriLayouts, apply_and_report};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args) {
        // The plugin hat. `run` never returns: it owns the process from here,
        // dialing the host socket and reconnecting forever.
        Ok(Invocation::Plugin) => hytte_plugin::run::<NiriLayouts>(),
        Ok(Invocation::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        // The standalone hat. Same `apply` the chip's click worker runs, so a
        // keybind and a click cannot drift.
        Ok(Invocation::Apply(layout)) => {
            match apply_and_report(&mut niri::SocketTransport, layout) {
                None => ExitCode::SUCCESS,
                Some(plugin::Msg::Failed(error)) => {
                    // niri's own text, verbatim — the CLI has no toast to put it in.
                    eprintln!("{BIN}: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        Err(error) => {
            eprintln!("{BIN}: {error}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}
