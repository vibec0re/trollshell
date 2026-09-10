//! Argument parsing for the standalone hat.
//!
//! Hand-rolled, following `hytte-infobroker`'s CLI: the workspace carries no
//! argument-parsing dependency at all (there is no `clap` in
//! `[workspace.dependencies]`, and none in any member's manifest), and two
//! tokens do not justify introducing one.
//!
//! The grammar is deliberately tiny, and **no arguments means the plugin
//! session** — that is the hinge the two hats turn on.

use crate::layout::Layout;

/// The binary's own name, for error lines (`argv[0]` would print the nix store
/// path).
pub(crate) const BIN: &str = "hytte-plugin-niri-layouts";

pub(crate) const USAGE: &str = "\
hytte-plugin-niri-layouts — column layouts for the focused niri workspace (issue #1019)

USAGE:
    hytte-plugin-niri-layouts                 run as a trollshell widget plugin
                                              (dials the shell's plugin socket)
    hytte-plugin-niri-layouts apply <layout>  apply one layout and exit
    hytte-plugin-niri-layouts --help          this text

    <layout>: equal  — every column the same width, 1/n each
              golden — first column 61.8 %, every other column 38.2 %
              split  — every column 50 %

Counts are COLUMNS, not windows: niri widths are per column, so a stacked
column of three windows is one column.

As a niri bind (etc/niri/binds.kdl):
    Mod+Shift+E { spawn \"hytte-plugin-niri-layouts\" \"apply\" \"equal\"; }";

/// What the command line asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Invocation {
    /// No arguments: be an ordinary out-of-process widget plugin.
    Plugin,
    /// `apply <layout>`: talk to niri directly and exit.
    Apply(Layout),
    /// `--help` / `-h` / `help`.
    Help,
}

/// Parse `argv[1..]`.
pub(crate) fn parse(args: &[String]) -> Result<Invocation, String> {
    match args.first().map(String::as_str) {
        None => Ok(Invocation::Plugin),
        Some("--help" | "-h" | "help") => Ok(Invocation::Help),
        Some("apply") => match args.len() {
            1 => Err(format!(
                "apply: missing <layout> (one of {})",
                known_layouts()
            )),
            2 => Layout::from_id(&args[1])
                .map(Invocation::Apply)
                .ok_or_else(|| {
                    format!(
                        "apply: unknown layout '{}' (known: {})",
                        args[1],
                        known_layouts()
                    )
                }),
            _ => Err(format!("apply: unexpected extra argument '{}'", args[2])),
        },
        Some(other) => Err(format!("unknown command '{other}'")),
    }
}

fn known_layouts() -> String {
    Layout::ALL
        .iter()
        .map(|l| l.id())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::{Invocation, USAGE, parse};
    use crate::layout::Layout;

    fn args(argv: &[&str]) -> Vec<String> {
        argv.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn no_arguments_is_the_plugin_session() {
        assert_eq!(parse(&[]), Ok(Invocation::Plugin));
    }

    #[test]
    fn apply_takes_each_of_the_three_layouts() {
        for layout in Layout::ALL {
            assert_eq!(
                parse(&args(&["apply", layout.id()])),
                Ok(Invocation::Apply(layout)),
                "apply {}",
                layout.id()
            );
        }
    }

    #[test]
    fn help_is_recognised_three_ways() {
        for flag in ["--help", "-h", "help"] {
            assert_eq!(parse(&args(&[flag])), Ok(Invocation::Help), "{flag}");
        }
    }

    #[test]
    fn apply_without_a_layout_names_the_three() {
        let err = parse(&args(&["apply"])).expect_err("a layout is required");
        assert!(err.contains("equal"), "got {err:?}");
        assert!(err.contains("golden"), "got {err:?}");
        assert!(err.contains("split"), "got {err:?}");
    }

    #[test]
    fn an_unknown_layout_is_refused_and_echoed() {
        let err = parse(&args(&["apply", "fibonacci"])).expect_err("not a layout");
        assert!(err.contains("fibonacci"), "got {err:?}");
        assert!(
            err.contains("golden"),
            "and lists the ones that are: {err:?}"
        );
    }

    #[test]
    fn a_trailing_argument_is_refused_rather_than_ignored() {
        // Silently ignoring it would make `apply equal golden` look like it did
        // both.
        let err = parse(&args(&["apply", "equal", "golden"])).expect_err("one layout only");
        assert!(err.contains("golden"), "got {err:?}");
    }

    #[test]
    fn an_unknown_subcommand_is_refused_rather_than_starting_a_session() {
        let err = parse(&args(&["aply", "equal"])).expect_err("typo, not a plugin start");
        assert!(err.contains("aply"), "got {err:?}");
    }

    #[test]
    fn the_usage_text_documents_every_layout_and_both_hats() {
        for layout in Layout::ALL {
            assert!(USAGE.contains(layout.id()), "usage omits {}", layout.id());
        }
        assert!(USAGE.contains("apply"), "the CLI hat");
        assert!(USAGE.contains("widget plugin"), "the plugin hat");
    }
}
