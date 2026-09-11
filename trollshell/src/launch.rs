//! The one `systemd-run --user` invocation builder.
//!
//! Three things in this shell start a program as a transient user unit, and
//! before #1071 phase 2 two of them built their own argv:
//!
//! | caller | what it launches |
//! | --- | --- |
//! | [`plugin_launcher`](crate::plugin_launcher) | a widget plugin, supervised, `PartOf=` the session target (#419) |
//! | [`plugins::effects`](crate::plugins::effects) | a plugin's detached `RunCommand`, in `trollshell-launch.slice` (#953) |
//! | [`panels::workspaces`](crate::panels::workspaces) | an app of a workspace stack, in `trollshell-ws-<name>.slice` (#1071) |
//!
//! Annika's call on #1071 (2026-09-10): *"You should consider generalizing on
//! this. Looks like this will be launching more than plugins."* So the flag
//! vocabulary lives here once, in [`Launch`], and the two pre-existing callers
//! render through it rather than beside it.
//!
//! # The one invariant this module exists to enforce (#984)
//!
//! **[`args`] is private and [`command`] is the only way out.**
//!
//! A secret must never reach `systemd-run`'s argv: `/proc/<pid>/cmdline` is
//! `0444` (any local user) while `/proc/<pid>/environ` is `0400` (owner only).
//! So a secret is passed as the **bare** `--setenv=<NAME>` form, and its value
//! travels through `systemd-run`'s *own* environment — `systemd-run(1)`: *"when
//! `=` and VALUE are omitted, the value of the variable with the same name in
//! the program environment will be used"*.
//!
//! Those two halves are one mechanism, and splitting them fails **silently**:
//! on systemd 260.2 a bare `--setenv=K` whose `K` is absent from `systemd-run`'s
//! environment sets the child's `K` to the empty string and exits 0, so a plugin
//! degrades to a blank key with nothing logged anywhere. #984 first stated the
//! pairing in prose, and measured that a revert to
//! `Command::new("systemd-run").args(<argv builder>)` compiled, ran, and left
//! every launcher test green. Hence the visibility rule: there is no reachable
//! path that produces an argv without the matching environment, and that revert
//! is a compile error rather than a passing test.
//!
//! Generalising had to carry that rule across intact, not merely re-describe
//! it. It does: [`args`] has no visibility modifier, so it is private to this
//! module, and `plugin_launcher`'s old `mod invocation` is gone rather than
//! duplicated.
//!
//! # Flag order
//!
//! One order serves every caller, and it is the order both pre-#1071 builders
//! already emitted — which is what lets their exact-argv pins
//! (`plugin_launcher`'s `systemd_run_args_pin_the_invocation`, `effects`'
//! `detached_launch_wraps_the_argv_in_a_systemd_run_service_unit`) stay
//! byte-identical through the move:
//!
//! ```text
//! --user --quiet --collect [--slice=S] --unit=U --description=D
//!     [--property=P]… [--setenv=K=V]… [--setenv=K]… -- argv…
//! ```
//!
//! The plugin launcher passes no slice and three properties; the detached
//! launcher passes a slice and no properties; neither carries both, so the two
//! old orders are the same order with one of the optional groups empty.

/// The program every launch runs. Injectable at the [`command`] call so a
/// hermetic test can point a launch at a recording stub (`effects`' #964
/// `the_dispatched_unit_reaches_the_systemd_run_argv` does exactly that)
/// without a user manager.
pub(crate) const SYSTEMD_RUN: &str = "systemd-run";

/// One transient-unit launch, as a request rather than an argv.
///
/// Every field is owned: a `Launch` is built by the caller, handed to
/// [`command`] once, and dropped. Nothing here is a builder chain — there are
/// three call sites in the tree and a struct literal reads better than
/// `.slice(None).properties(vec![])` at each of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Launch {
    /// `--unit=` — the transient unit's name. The caller owns uniqueness and
    /// validity; this module does not guess a name or validate one, because the
    /// three callers guard different things (a plugin id, a workspace name, a
    /// host-allocated sequence) and each already does so at its own boundary.
    pub unit: String,
    /// `--description=` — the human line in `systemctl --user status`.
    pub description: String,
    /// `--slice=`, when the unit belongs to a slice. `None` emits no flag at
    /// all rather than an empty one, which is what keeps the plugin launcher's
    /// argv unchanged.
    pub slice: Option<String>,
    /// `--property=` values, verbatim and in order (e.g.
    /// `"Restart=on-failure"`). A `Vec<String>` rather than a typed set: the
    /// vocabulary is systemd's, it is large, and the two callers that use it
    /// want three fixed entries between them.
    pub properties: Vec<String>,
    /// `--setenv=K=V` — values that may ride the world-readable argv because
    /// they are already world-readable (nix renders `plugins.json` `0444`) or
    /// carry no secret (the display/IPC variables a detached launch forwards).
    pub env: Vec<(String, String)>,
    /// Bare `--setenv=K`, with the value set on `systemd-run`'s **own**
    /// environment by [`command`]. The #984 channel — see the module doc.
    ///
    /// Emitted *after* [`Self::env`] on purpose: a bare `--setenv=K` replaces an
    /// earlier `--setenv=K=V`, so an injected secret keeps winning over a stale
    /// value declared inline. That collision is the shipping claude-bridge
    /// configuration (`nix/hm-module.nix`'s billing scrub declares
    /// `ANTHROPIC_API_KEY = ""` alongside the `anthropic` slot).
    pub secret_env: Vec<(String, String)>,
    /// Everything after `--`. The separator is unconditional, so an `argv[0]`
    /// of `--now` is passed through as the program rather than eaten as a
    /// `systemd-run` option.
    pub argv: Vec<String>,
}

/// The full `systemd-run` argv (sans the program itself). Pure — but **private**
/// (see the module doc): the only way to reach it is [`command`], which also
/// sets the matching environment.
///
/// - `--user`: the session manager, so the unit lands in the user's own tree.
/// - `--quiet`: no "Running as unit …" chatter on stderr.
/// - `--collect`: release the unit even when the program ends failed, so a
///   crash-looped name is never wedged waiting for a `reset-failed`.
/// - `--`: terminates option parsing before any caller-supplied argv.
fn args(launch: &Launch) -> Vec<String> {
    let mut args = vec![
        "--user".to_owned(),
        "--quiet".to_owned(),
        "--collect".to_owned(),
    ];
    if let Some(slice) = &launch.slice {
        args.push(format!("--slice={slice}"));
    }
    args.push(format!("--unit={}", launch.unit));
    args.push(format!("--description={}", launch.description));
    for property in &launch.properties {
        args.push(format!("--property={property}"));
    }
    for (k, v) in &launch.env {
        args.push(format!("--setenv={k}={v}"));
    }
    for (k, _) in &launch.secret_env {
        // Bare name only — the value rides `systemd-run`'s environment (#984).
        args.push(format!("--setenv={k}"));
    }
    args.push("--".to_owned());
    args.extend(launch.argv.iter().cloned());
    args
}

/// The `systemd-run` invocation for one launch: [`args`] as argv, plus every
/// [`Launch::secret_env`] pair set in the child's **environment**. The only item
/// this module exports that can produce an argv, by design.
///
/// `Command` inherits the shell's environment and adds these on top, which is
/// what `systemd-run` needs to resolve a bare `--setenv=<NAME>`. The values land
/// in `/proc/<pid>/environ` (`0400`, owner-only) rather than
/// `/proc/<pid>/cmdline` (`0444`, any local user).
///
/// The env loop is unconditional: a name that is **both** declared in
/// [`Launch::env`] and injected must still be set here, because the argv's bare
/// `--setenv=<NAME>` overrides the inline `--setenv=<NAME>=<declared>` that
/// precedes it. Skipping it for such a name would hand the child the declared
/// value's *replacement*, which is the empty string.
///
/// `program` is [`SYSTEMD_RUN`] in production; see that constant for why it is
/// a parameter.
pub(crate) fn command(program: &str, launch: &Launch) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args(launch));
    for (k, v) in &launch.secret_env {
        cmd.env(k, v);
    }
    cmd
}

/// The argv [`command`] built, read back off the `Command` itself.
///
/// The assertion seam the two migrated callers' argv pins use. Reading it back
/// off the built command rather than exporting [`args`] is what keeps the #984
/// invariant intact *and* makes every argv assertion necessarily describe an
/// invocation that also carries the matching environment — strictly better than
/// pinning a pure builder a second launch path could sidestep.
#[cfg(test)]
pub(crate) fn argv_of(cmd: &tokio::process::Command) -> Vec<String> {
    cmd.as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// The variables `cmd` sets **explicitly** on the child — `get_envs` reports
/// only the delta over the inherited environment, which is exactly what the
/// #984 pins want — as owned strings sorted by name, since `get_envs`' own
/// order is unspecified.
#[cfg(test)]
pub(crate) fn envs_of(cmd: &tokio::process::Command) -> Vec<(String, Option<String>)> {
    let mut envs: Vec<(String, Option<String>)> = cmd
        .as_std()
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    envs.sort();
    envs
}

#[cfg(test)]
mod tests {
    use super::{Launch, SYSTEMD_RUN, argv_of, command, envs_of};

    fn argv(launch: &Launch) -> Vec<String> {
        argv_of(&command(SYSTEMD_RUN, launch))
    }

    /// The shared prefix and the separator, on the emptiest possible launch.
    #[test]
    fn the_fixed_flags_come_first_and_the_separator_always_comes_last() {
        let args = argv(&Launch {
            unit: "u.service".to_owned(),
            description: "d".to_owned(),
            argv: vec!["/bin/true".to_owned()],
            ..Launch::default()
        });
        assert_eq!(
            args,
            [
                "--user",
                "--quiet",
                "--collect",
                "--unit=u.service",
                "--description=d",
                "--",
                "/bin/true",
            ]
        );
    }

    /// `--slice=` is emitted only when there is one, and it precedes `--unit=`.
    ///
    /// Both halves are load-bearing: the plugin launcher passes no slice and its
    /// exact-argv pin has `--unit=` fourth, while the detached launcher passes
    /// one and its pin has `--slice=` fourth. Emitting an empty `--slice=` for
    /// `None`, or ordering it after `--unit=`, reds one of the two.
    #[test]
    fn a_slice_is_optional_and_sits_between_collect_and_unit() {
        let without = argv(&Launch {
            unit: "u.service".to_owned(),
            ..Launch::default()
        });
        assert!(
            !without.iter().any(|a| a.starts_with("--slice")),
            "no slice must emit no flag at all: {without:?}"
        );

        let with = argv(&Launch {
            unit: "u.service".to_owned(),
            slice: Some("s.slice".to_owned()),
            ..Launch::default()
        });
        let slice = with.iter().position(|a| a == "--slice=s.slice");
        assert_eq!(slice, Some(3), "{with:?}");
        assert_eq!(with.iter().position(|a| a == "--unit=u.service"), Some(4));
    }

    /// Properties render long-form, verbatim, in the order given.
    #[test]
    fn properties_render_in_order() {
        let args = argv(&Launch {
            unit: "u.service".to_owned(),
            properties: vec![
                "Restart=on-failure".to_owned(),
                "RestartSec=2".to_owned(),
                "PartOf=niri-session.target".to_owned(),
            ],
            ..Launch::default()
        });
        let rendered: Vec<&String> = args
            .iter()
            .filter(|a| a.starts_with("--property="))
            .collect();
        assert_eq!(
            rendered,
            [
                "--property=Restart=on-failure",
                "--property=RestartSec=2",
                "--property=PartOf=niri-session.target",
            ]
        );
    }

    /// #984, stated on the generalised builder: a `secret_env` value is in the
    /// child's environment and its **name only** is in the argv.
    ///
    /// This is the property the whole module-privacy rule exists for, restated
    /// here so it is pinned at the seam every caller now goes through rather
    /// than only at `plugin_launcher`'s.
    #[test]
    fn a_secret_reaches_the_environment_and_never_the_argv() {
        const SECRET: &str = "sk-do-not-put-me-in-argv";
        let launch = Launch {
            unit: "u.service".to_owned(),
            env: vec![("PET_NAME".to_owned(), "nisse".to_owned())],
            secret_env: vec![("OPENROUTER_API_KEY".to_owned(), SECRET.to_owned())],
            ..Launch::default()
        };
        let cmd = command(SYSTEMD_RUN, &launch);
        let args = argv_of(&cmd);

        assert!(
            args.contains(&"--setenv=OPENROUTER_API_KEY".to_owned()),
            "the bare form must name the variable: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.contains(SECRET)),
            "secret value found in argv: {args:?}"
        );
        assert_eq!(
            envs_of(&cmd),
            vec![("OPENROUTER_API_KEY".to_owned(), Some(SECRET.to_owned()))],
            "the value must be set on the systemd-run process's environment"
        );
        assert!(
            args.contains(&"--setenv=PET_NAME=nisse".to_owned()),
            "a declared value deliberately stays inline: {args:?}"
        );
    }

    /// The precedence half of #984: the bare form comes after the inline one,
    /// and the injected value is really set even when the same name is declared.
    ///
    /// A `command` that skipped `cmd.env` for a name already in `env` would pass
    /// an argv-order-only assertion while the child silently received `""`.
    #[test]
    fn an_injected_secret_still_overrides_a_declared_value() {
        let launch = Launch {
            unit: "u.service".to_owned(),
            env: vec![("OPENROUTER_API_KEY".to_owned(), "stale".to_owned())],
            secret_env: vec![("OPENROUTER_API_KEY".to_owned(), "fresh".to_owned())],
            ..Launch::default()
        };
        let cmd = command(SYSTEMD_RUN, &launch);
        let args = argv_of(&cmd);

        let declared = args
            .iter()
            .position(|a| a == "--setenv=OPENROUTER_API_KEY=stale")
            .expect("the declared value is still passed inline");
        let injected = args
            .iter()
            .position(|a| a == "--setenv=OPENROUTER_API_KEY")
            .expect("the injected secret is passed bare");
        assert!(declared < injected, "{args:?}");
        assert_eq!(
            envs_of(&cmd),
            vec![("OPENROUTER_API_KEY".to_owned(), Some("fresh".to_owned()))],
            "the injected value must be set even when the same name is declared"
        );
    }

    /// A caller-supplied argv that looks like options is passed through.
    #[test]
    fn the_separator_protects_an_argv_that_looks_like_flags() {
        let args = argv(&Launch {
            unit: "u.service".to_owned(),
            argv: vec!["--scope".to_owned(), "--wait".to_owned()],
            ..Launch::default()
        });
        let sep = args.iter().position(|a| a == "--").expect("separated");
        assert_eq!(
            &args[sep + 1..],
            &["--scope".to_owned(), "--wait".to_owned()]
        );
        assert!(
            args[..sep].iter().all(|a| a != "--scope" && a != "--wait"),
            "nothing before -- was contributed by the caller: {args:?}"
        );
    }

    /// The program is what `command` was asked for, so a hermetic test can point
    /// a launch at a recording stub.
    #[test]
    fn the_program_is_injectable() {
        let cmd = command("/tmp/stub.sh", &Launch::default());
        assert_eq!(cmd.as_std().get_program().to_string_lossy(), "/tmp/stub.sh");
        assert_eq!(
            command(SYSTEMD_RUN, &Launch::default())
                .as_std()
                .get_program()
                .to_string_lossy(),
            "systemd-run"
        );
    }
}
