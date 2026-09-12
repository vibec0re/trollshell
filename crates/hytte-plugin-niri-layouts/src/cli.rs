//! Argument parsing for the standalone hat.
//!
//! `clap` (#1116, following @kaesaecracker's recommendation on the thread),
//! not hand-rolled any more: nix generates shell completions from the same
//! `Command` tree at build time — see the hidden `completions <shell>`
//! subcommand below, which `nix/plugin.nix`'s `installShellCompletion` call
//! invokes.
//!
//! The grammar is still deliberately tiny, and **no arguments means the
//! plugin session** — that is the hinge the two hats turn on, preserved here
//! as `Cli::command` being an `Option`: a `None` is the plugin hat, never a
//! parse failure.

use clap::{CommandFactory as _, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};

use crate::layout::Layout;

/// The binary's own name, for error lines (`argv[0]` would print the nix store
/// path).
pub(crate) const BIN: &str = "hytte-plugin-niri-layouts";

/// Named the breakpoint/proportions as literal text for the humans reading
/// `--help` or a niri bind file, same as before clap: `hardcoded_breakpoint_
/// strings_track_the_constant` (`layout.rs`) reads this constant back to
/// check it still names [`crate::layout::GOLDEN_BREAKPOINT`] correctly, so
/// keep the "2560" spelling if this text changes.
pub(crate) const USAGE: &str = "\
hytte-plugin-niri-layouts — column layouts for the focused niri workspace (issue #1019)

USAGE:
    hytte-plugin-niri-layouts                 run as a trollshell widget plugin
                                              (dials the shell's plugin socket)
    hytte-plugin-niri-layouts apply <layout>  apply one layout and exit
    hytte-plugin-niri-layouts --help          this text

    <layout>: equal  — every column the same width, 1/n each
              golden — first column 75 %, every other column 25 %, on the
                       target output's screen at 2560 logical px wide or
                       more; 61.8 % / 38.2 % (the golden cut) narrower than
                       that (#1052)
              split  — every column 50 %

Counts are COLUMNS, not windows: niri widths are per column, so a stacked
column of three windows is one column.

As a niri bind (etc/niri/binds.kdl):
    Mod+Alt+E { spawn \"hytte-plugin-niri-layouts\" \"apply\" \"equal\"; }";

/// Three one-click column layouts for the focused niri workspace.
///
/// With no subcommand, runs as an ordinary out-of-process trollshell widget
/// plugin (dials the shell's plugin socket). `apply <layout>` applies one
/// layout and exits instead, which is what a niri `spawn` bind invokes — see
/// `etc/niri/binds.kdl` for the wiring.
#[derive(Parser, Debug)]
#[command(name = "hytte-plugin-niri-layouts", version, long_about = USAGE)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// Apply one layout to the focused workspace's columns and exit
    Apply {
        /// equal (1/n each) | golden (75/25, or 61.8/38.2 under 2560 logical
        /// px, #1052) | split (50/50)
        layout: LayoutArg,
    },
    /// Print a shell completion script (invoked by nix's
    /// `installShellCompletion`, not meant for a human to type)
    #[command(hide = true)]
    Completions {
        /// Which shell's script to print
        shell: Shell,
    },
}

/// A `clap`-facing mirror of [`Layout`]. Kept as its own type rather than
/// deriving `ValueEnum` on `Layout` itself — `layout.rs` is pure planning
/// logic with no argument-parsing concern, so this is where that concern
/// lives instead. Variant names map 1:1 via `From`, and clap's default
/// kebab-case rendering (`Equal` → `"equal"`, etc.) matches
/// [`Layout::id`](crate::layout::Layout::id)'s own strings, which the tests
/// below pin.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum LayoutArg {
    Equal,
    Golden,
    Split,
}

impl From<LayoutArg> for Layout {
    fn from(arg: LayoutArg) -> Self {
        match arg {
            LayoutArg::Equal => Self::Equal,
            LayoutArg::Golden => Self::Golden,
            LayoutArg::Split => Self::Split,
        }
    }
}

/// Render one shell's completion script for this binary's `Command` tree.
/// Split from printing so a test can inspect the bytes without capturing
/// stdout.
pub(crate) fn render_completions(shell: Shell) -> String {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_owned();
    let mut buf: Vec<u8> = Vec::new();
    generate(shell, &mut cmd, name, &mut buf);
    String::from_utf8(buf).expect("clap_complete's generated script is always valid UTF-8")
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory as _, Parser as _};
    use clap_complete::Shell;

    use super::{Cli, Command, USAGE, render_completions};
    use crate::layout::Layout;

    fn parse(argv: &[&str]) -> Result<Cli, clap::Error> {
        let mut full = vec!["hytte-plugin-niri-layouts"];
        full.extend_from_slice(argv);
        Cli::try_parse_from(full)
    }

    #[test]
    fn no_arguments_is_the_plugin_session() {
        let cli = parse(&[]).expect("bare invocation parses");
        assert!(cli.command.is_none());
    }

    #[test]
    fn apply_takes_each_of_the_three_layouts() {
        for layout in Layout::ALL {
            let cli = parse(&["apply", layout.id()])
                .unwrap_or_else(|e| panic!("apply {}: {e}", layout.id()));
            let Some(Command::Apply { layout: arg }) = cli.command else {
                panic!("apply {} did not parse as Apply", layout.id());
            };
            assert_eq!(Layout::from(arg), layout, "apply {}", layout.id());
        }
    }

    #[test]
    fn help_is_recognised_three_ways() {
        for flag in ["--help", "-h", "help"] {
            let err = parse(&[flag]).expect_err("help exits through the Err/DisplayHelp path");
            assert_eq!(
                err.exit_code(),
                0,
                "{flag} should be a zero-exit help display"
            );
        }
    }

    #[test]
    fn apply_without_a_layout_is_refused() {
        assert!(parse(&["apply"]).is_err());
    }

    #[test]
    fn an_unknown_layout_is_refused_and_echoed() {
        let err = parse(&["apply", "fibonacci"]).expect_err("not a layout");
        let msg = err.to_string();
        assert!(msg.contains("fibonacci"), "got {msg:?}");
        assert!(
            msg.contains("golden"),
            "and lists the ones that are: {msg:?}"
        );
    }

    #[test]
    fn a_trailing_argument_is_refused_rather_than_ignored() {
        // Silently ignoring it would make `apply equal golden` look like it did
        // both.
        let err = parse(&["apply", "equal", "golden"]).expect_err("one layout only");
        assert!(err.to_string().contains("golden"), "got {err}");
    }

    #[test]
    fn an_unknown_subcommand_is_refused_rather_than_starting_a_session() {
        let err = parse(&["aply", "equal"]).expect_err("typo, not a plugin start");
        assert!(err.to_string().contains("aply"), "got {err}");
    }

    #[test]
    fn the_usage_text_documents_every_layout_and_both_hats() {
        for layout in Layout::ALL {
            assert!(USAGE.contains(layout.id()), "usage omits {}", layout.id());
        }
        assert!(USAGE.contains("apply"), "the CLI hat");
        assert!(USAGE.contains("widget plugin"), "the plugin hat");
    }

    /// `--help` never dials niri (see the `None` arm in `main`), so it
    /// can't report which pair actually applies on the screen it's run on —
    /// it documents the rule instead, both pairs and the breakpoint between
    /// them (#1052).
    #[test]
    fn the_usage_text_names_both_golden_pairs_and_the_breakpoint() {
        assert!(
            USAGE.contains("75 %") && USAGE.contains("25 %"),
            "the wide pair"
        );
        assert!(
            USAGE.contains("61.8 %") && USAGE.contains("38.2 %"),
            "the golden cut"
        );
        assert!(USAGE.contains("2560"), "the breakpoint between them");
    }

    #[test]
    fn completions_parses_every_shell_but_stays_hidden() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            let cli = parse(&["completions", &shell.to_string()])
                .unwrap_or_else(|e| panic!("completions {shell}: {e}"));
            assert!(matches!(cli.command, Some(Command::Completions { .. })));
        }
        // Checked against the `Command` tree's own hidden flag, not against
        // rendered `--help` text: `USAGE` (this file's `long_about`) is free
        // to *mention* the word "completions" without that meaning the
        // subcommand itself is listed.
        let cmd = Cli::command();
        let completions = cmd
            .get_subcommands()
            .find(|s| s.get_name() == "completions")
            .expect("a completions subcommand exists");
        assert!(
            completions.is_hide_set(),
            "completions subcommand must be hidden from --help"
        );
    }

    /// Falsifies a dropped subcommand: removing `apply` from [`Command`]
    /// would no longer print its name here.
    #[test]
    fn bash_completions_name_the_binary_and_every_subcommand() {
        let script = render_completions(Shell::Bash);
        assert!(script.contains("hytte-plugin-niri-layouts"), "{script}");
        assert!(
            script.contains("apply"),
            "bash completions missing 'apply':\n{script}"
        );
    }
}
