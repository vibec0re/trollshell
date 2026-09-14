//! Launching the agent's **companion window** — P2 of #947, tracked by
//! [#950](https://github.com/vibec0re/trollshell/issues/950).
//!
//! The window is `trollshell-agent-window`, a separate GTK4 + libadwaita
//! binary on the `trollshell-control-center` precedent: our chrome (header,
//! live status, start/stop/pause, a settings tab) around a `WebKitGTK` view of
//! hyperhive's own agent page. It is **never linked into this plugin** — the
//! plugin only asks the host to launch it, with
//! [`Effect::launch`](hytte_plugin::proto::Effect::launch) (#953's detached
//! mode), so the window outlives a `trollshell.service` restart and is not in
//! the shell's cgroup.
//!
//! # Why the fallback is decided *here* and not on the `EffectResult`
//!
//! The obvious shape — launch, and fall back when the host says the launch
//! failed — **cannot work**, and the host says so: on the systemd path
//! `systemd-run` returns as soon as the user manager takes the start job, so a
//! program that is not on `PATH` still produces
//! `ok: true, output: "launched unit …"` and the unit fails at exec where no
//! one is listening (`trollshell/src/plugins/effects.rs`'s `start_detached`;
//! only the no-user-manager *fallback* path ever sees an `ENOENT`). A launch
//! outcome is therefore not evidence that the window exists.
//!
//! So the plugin resolves the binary itself, once, with [`Probe`], and picks
//! the route before it emits anything. A desktop that has the agents plugin
//! but not the window — an out-of-tree install, or `plugins.agents` enabled
//! with `agentWindow.enable = false` — keeps the P1 behaviour (the browser for
//! the agent page, the drawer page for the pen) instead of a click that
//! silently does nothing.

use std::path::Path;

/// The companion window's binary name, as `nix/agent-window.nix` installs it
/// and as the manifest's `RunCommand` capability will spawn it.
pub const BINARY: &str = "trollshell-agent-window";

/// Which agent the window is for.
pub const ARG_AGENT: &str = "--agent";

/// Which tab it opens on. Omitted for [`Tab::Agent`] — see [`argv`].
pub const ARG_TAB: &str = "--tab";

/// The compositor's own CLI, which is how the workspace gets focused
/// ([#1306](https://github.com/vibec0re/trollshell/issues/1306)).
///
/// A plugin cannot ask the compositor anything — it has no `$NIRI_SOCKET`
/// client and no business growing one — so the focus rides the capability it
/// already has: one more detached `RunCommand`, of niri's own command-line
/// tool. That is also why the *dynamic* variant @kaesaecracker raised (find
/// the first empty workspace and rename it) is **not** here: choosing one
/// needs niri's workspace list, and reading that list is the shell's job, not
/// a plugin's.
pub const NIRI_BINARY: &str = "niri";

/// `niri msg action focus-workspace <workspace>` — the focus the fan-out
/// emits before its launches.
///
/// The name travels as its **own** argv element, for
/// [`argv`]'s reason and one sharper: niri's command line is `clap`, so a
/// workspace name starting with `-` would be read as a flag. This builder does
/// not guard that — `crate::config::AgentsConfig::workspace` does, before the
/// name ever reaches here, which is where a bad value can cost its own key
/// instead of producing an argv nobody can explain.
#[must_use]
pub fn niri_focus_argv(workspace: &str) -> Vec<String> {
    vec![
        NIRI_BINARY.to_owned(),
        "msg".to_owned(),
        "action".to_owned(),
        "focus-workspace".to_owned(),
        workspace.to_owned(),
    ]
}

/// The window's two tabs, as its `--tab` argument spells them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    /// hyperhive's agent page in the `WebKitGTK` view — the window's default,
    /// and what the agent-page link opens.
    Agent,
    /// The agent's settings, which the card's **pen** opens (Annika on
    /// [#947](https://github.com/vibec0re/trollshell/issues/947), 2026-09-11
    /// 07:43Z: "the card's edit button opens that window on its settings
    /// tab", so an agent has one surface).
    Settings,
}

impl Tab {
    /// The word on the command line.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Settings => "settings",
        }
    }
}

/// The argv a detached launch carries, for one agent and one tab.
///
/// [`Tab::Agent`] is spelled by **omitting** `--tab`, not by passing
/// `--tab agent`: the window's default tab is the agent page, so the plain
/// two-argument form is the one #950 names and the one a human types. The
/// window accepts `--tab agent` too (it is the same value its parser
/// defaults to), so nothing depends on which spelling arrives.
#[must_use]
pub fn argv(name: &str, tab: Tab) -> Vec<String> {
    let mut out = vec![BINARY.to_owned(), ARG_AGENT.to_owned(), name.to_owned()];
    if tab != Tab::Agent {
        out.push(ARG_TAB.to_owned());
        out.push(tab.as_str().to_owned());
    }
    out
}

/// Is [`BINARY`] on **this process's** `PATH`, as an executable file?
///
/// # The assumption, stated as an assumption (#1130 L4)
///
/// This is the *plugin's* environment. The launch resolves the program
/// somewhere else: in the **user manager's** environment on the
/// `systemd-run --user` path, and in **trollshell's** on the no-manager
/// fallback (`trollshell/src/plugins/effects.rs`'s `start_detached`). In the
/// normal deployment all three are the same environment — the plugin is itself
/// a `systemd-run --user` transient unit the shell started — and that is why
/// asking here answers the question the launch will ask.
///
/// It is an equality that **can** break, in both directions, and neither break
/// is observable from here: a legacy static unit under `etc/` with its own
/// `Environment=PATH=`, or a plugin started by hand from a shell, gives either
/// a false positive (a click that silently does nothing, because the unit
/// fails at exec where nobody is listening — the very case this design says it
/// cannot observe) or a false negative (a permanent browser fallback).
///
/// So [`Probe::available`]'s warning prints the `PATH` it actually searched:
/// an operator comparing that line with `systemctl --user show-environment`
/// can see the mismatch, which is the most this side can offer without
/// reaching into another process's environment.
#[must_use]
pub fn on_path(binary: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| is_executable(&dir.join(binary)))
}

/// The `PATH` [`on_path`] searched, for the warning to quote.
#[must_use]
fn searched_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "<unset>".to_owned())
}

/// An existing regular file with at least one execute bit.
///
/// Deliberately not `access(X_OK)`: this is a "would a launch find it" check
/// a fraction of a second before the launch, not a security decision, and the
/// mode bits are what `PATH` resolution itself looks at.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// What a missing binary costs, in the one sentence its warning adds.
///
/// A `&'static str` on the probe rather than a `match` inside it, because the
/// two consequences have nothing in common: one is "this click takes its
/// fallback route", the other is "the windows still open, just not where you
/// wanted them". Naming the consequence is the whole value of the warning —
/// "niri is not on PATH" on its own tells an operator nothing about what they
/// just saw happen.
const WINDOW_MISSING: &str =
    "agent pages open in the browser and the pen opens the drawer page instead (install it with \
     programs.trollshell.agentWindow.enable)";

/// See [`WINDOW_MISSING`]. #1306's half: the fan-out still opens every window,
/// it just cannot gather them first.
const NIRI_MISSING: &str =
    "\"Open terminals\" cannot focus the hive workspace first, so the agent windows open wherever \
     the compositor puts them (a niri window rule still gathers them — see \
     etc/niri/agent-windows.kdl)";

/// Whether a binary this plugin wants to launch is there — resolved **once**
/// per process, and complained about **once**.
///
/// Caching is not an optimisation: without it a desktop with no window would
/// scan `PATH` and log a warning on every click of every row, which is the
/// shape §8's edge detector exists to avoid elsewhere in this plugin. The
/// window is installed by the same nix build as the plugin, so it does not
/// appear mid-session; if it ever does, restarting the plugin's unit is the
/// (cheap, transient) way to re-resolve.
///
/// It became **two** probes with #1306 — the window, and `niri` for the
/// workspace focus — so the binary and its missing-consequence are fields
/// rather than the constants they used to be. Everything else is unchanged,
/// including [`Probe::fixed`]'s meaning as the window's test seam.
#[derive(Clone, Debug)]
pub struct Probe {
    /// Which program this probe is about — searched on `PATH`, and named in
    /// the warning.
    binary: &'static str,
    /// The sentence the warning adds: see [`WINDOW_MISSING`].
    missing: &'static str,
    /// How the binary is located. A plain `fn` pointer rather than a boxed
    /// closure: the only values it ever takes are [`on_path`] and the two
    /// constants below, so a pointer needs no allocation and keeps [`Probe`]
    /// `Clone` without a lifetime.
    ///
    /// Deliberately **not** `Copy`: [`Probe::warned`] is a latch, and a copy
    /// would silently give the copy a fresh one.
    lookup: fn(&'static str) -> bool,
    /// The resolved answer, `None` until the first click wants it.
    cached: Option<bool>,
    /// Whether the "not installed" warning has already been said.
    warned: bool,
}

impl Probe {
    /// The real probe: resolve [`BINARY`] against `PATH` on first use.
    #[must_use]
    pub fn path() -> Self {
        Self::resolving(BINARY, WINDOW_MISSING)
    }

    /// The real probe for [`NIRI_BINARY`] (#1306).
    #[must_use]
    pub fn niri() -> Self {
        Self::resolving(NIRI_BINARY, NIRI_MISSING)
    }

    fn resolving(binary: &'static str, missing: &'static str) -> Self {
        Self {
            binary,
            missing,
            lookup: on_path,
            cached: None,
            warned: false,
        }
    }

    /// A probe pinned to one answer — the test seam, so a reducer test states
    /// which desktop it is describing instead of inheriting the machine's
    /// `PATH`.
    ///
    /// This one is the **window's**; [`Probe::fixed_niri`] is the compositor's.
    /// Two constructors rather than one taking a binary, so a test reads as the
    /// desktop it describes ("no window installed") rather than as a string.
    #[must_use]
    pub fn fixed(available: bool) -> Self {
        Self::pinned(BINARY, WINDOW_MISSING, available)
    }

    /// [`Probe::fixed`] for `niri` (#1306).
    #[must_use]
    pub fn fixed_niri(available: bool) -> Self {
        Self::pinned(NIRI_BINARY, NIRI_MISSING, available)
    }

    fn pinned(binary: &'static str, missing: &'static str, available: bool) -> Self {
        Self {
            binary,
            missing,
            lookup: if available { always } else { never },
            cached: Some(available),
            warned: false,
        }
    }

    /// Is the binary there? Resolves on first call and remembers.
    pub fn available(&mut self) -> bool {
        let lookup = self.lookup;
        let binary = self.binary;
        let found = *self.cached.get_or_insert_with(|| lookup(binary));
        if self.claim_warning(found) {
            // The searched `PATH` goes in the line because it is **this
            // process's**, and the launch will resolve somewhere else — see
            // [`on_path`]'s docs. An operator with a click that does nothing
            // can compare this against `systemctl --user show-environment`.
            tracing::warn!(
                binary = self.binary,
                searched_path = %searched_path(),
                missing = self.missing,
                "a program this plugin launches is not on its PATH. The launch would resolve it \
                 in the systemd user manager's environment, which is normally the same one — \
                 compare with `systemctl --user show-environment` if it is not"
            );
        }
        found
    }

    /// Should *this* call say something? Takes the one warning the probe owes,
    /// so the second miss is silent.
    ///
    /// Split out of [`Probe::available`] so the latch is assertable: a
    /// `tracing::warn!` has no return value, and capturing it would need a
    /// process-global subscriber whose callsite `Interest` leaks into sibling
    /// tests (#991). This is the mechanism, and a test can consume it.
    fn claim_warning(&mut self, found: bool) -> bool {
        if found || self.warned {
            return false;
        }
        self.warned = true;
        true
    }
}

impl Default for Probe {
    fn default() -> Self {
        Self::path()
    }
}

/// [`Probe::fixed`]'s "installed" lookup. A named `fn` rather than a closure
/// so both arms of its `if` have the one `fn(&'static str) -> bool` type.
fn always(_binary: &'static str) -> bool {
    true
}

/// [`Probe::fixed`]'s "not installed" lookup. See [`always`].
fn never(_binary: &'static str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::{ARG_AGENT, ARG_TAB, BINARY, Probe, Tab, argv};

    /// The plain launch is `<binary> --agent <name>` and carries **no** tab
    /// argument — #950's own spelling.
    ///
    /// Falsification: add `--tab agent` to the [`Tab::Agent`] arm and the
    /// length assertion reds.
    #[test]
    fn the_agent_tab_is_spelled_by_omitting_the_flag() {
        assert_eq!(
            argv("trollshell-choom", Tab::Agent),
            vec![
                BINARY.to_owned(),
                ARG_AGENT.to_owned(),
                "trollshell-choom".to_owned(),
            ]
        );
    }

    /// The pen's launch appends `--tab settings`, and nothing else moves.
    ///
    /// Falsification: swap the two flags, or spell the tab anything but
    /// `settings`, and this reds on the exact vector.
    #[test]
    fn the_settings_tab_appends_the_flag_and_its_word() {
        assert_eq!(
            argv("nixos-choom", Tab::Settings),
            vec![
                BINARY.to_owned(),
                ARG_AGENT.to_owned(),
                "nixos-choom".to_owned(),
                ARG_TAB.to_owned(),
                "settings".to_owned(),
            ]
        );
    }

    /// The agent name is passed as **one** argument, never interpolated into
    /// a string a shell would re-split — there is no shell on the detached
    /// path, and this is what keeps it that way.
    #[test]
    fn the_name_is_its_own_argument() {
        let av = argv("a-b-c", Tab::Settings);
        assert_eq!(av.iter().filter(|a| *a == "a-b-c").count(), 1);
        assert!(av.iter().all(|a| !a.contains(' ')), "{av:?}");
    }

    /// A pinned probe answers without touching `PATH`, in both directions.
    #[test]
    fn a_fixed_probe_answers_what_it_was_pinned_to() {
        assert!(Probe::fixed(true).available());
        assert!(!Probe::fixed(false).available());
    }

    /// The absent-window warning is said **once**, not once per click — and
    /// [`Probe::available`] is what consumes it.
    ///
    /// Mutations, both verified red: delete `self.warned = true` from
    /// `claim_warning` (the second assertion then still claims a warning), or
    /// delete its `|| self.warned` guard (same). Deleting the `claim_warning`
    /// call from `available` reds the second assertion too, which is the half
    /// that pins *who* consumes it rather than merely that a latch exists.
    #[test]
    fn the_missing_window_is_complained_about_once() {
        let mut p = Probe::fixed(false);
        assert!(!p.available());
        assert!(
            !p.claim_warning(false),
            "available() already took the one warning this probe owes"
        );

        let mut fresh = Probe::fixed(false);
        assert!(
            fresh.claim_warning(false),
            "a probe that has not missed yet still owes one"
        );
        assert!(!fresh.claim_warning(false), "and only one");
    }

    /// An **installed** window never warns, however often it is asked.
    #[test]
    fn an_installed_window_is_never_complained_about() {
        let mut p = Probe::fixed(true);
        assert!(p.available());
        assert!(!p.claim_warning(true));
        assert!(!p.warned);
    }

    /// The `"-leading-hyphen"` fixture (`model.rs`'s `AgentName` test names it
    /// "a P2 argv concern, not this type's") is safe **because of the shape
    /// of the argv**, and this pins that shape byte for byte: the name travels
    /// as its own element after `--agent`, never joined with `=` and never
    /// preceded by anything that could read it as a flag.
    ///
    /// Why the shape matters: `trollshell-agent-window::cli::parse` is
    /// hand-rolled, not `clap`. On the token `"--agent"` it takes the *next*
    /// token as the value unconditionally, without re-examining it for a
    /// leading `-`, so a name starting with one is never reinterpreted as a
    /// flag — and `--agent=<name>` would in fact *break* it (no `=`-splitting
    /// arm; the joined token falls to its `Unknown` arm, verified red while
    /// this test was written). The parser side of the same contract is
    /// `trollshell-agent-window`'s `the_plugins_own_argv_parses_on_both_tabs`,
    /// which feeds this builder's output — this fixture included — through the
    /// real parser; it lives there rather than here because a dev-dependency
    /// from this crate back onto the window would link the web engine into
    /// this crate's test build.
    #[test]
    fn a_leading_hyphen_name_is_its_own_argv_element() {
        assert_eq!(
            argv("-leading-hyphen", Tab::Agent),
            [BINARY, ARG_AGENT, "-leading-hyphen"]
        );
        assert_eq!(
            argv("-leading-hyphen", Tab::Settings),
            [BINARY, ARG_AGENT, "-leading-hyphen", ARG_TAB, "settings"]
        );
    }

    /// The real probe resolves **once**: a second call does not re-scan.
    ///
    /// Falsification: drop the `cached` field (or re-run `lookup` every call)
    /// and the pinned-`cached` assertion reds.
    #[test]
    fn the_path_probe_caches_its_answer() {
        let mut p = Probe::path();
        assert!(
            p.cached.is_none(),
            "nothing is resolved before it is needed"
        );
        let first = p.available();
        assert_eq!(p.cached, Some(first), "the answer is remembered");
        // Pin the cache rather than PATH: flip it and the second call must
        // return the *cached* value, which proves the lookup did not re-run.
        p.cached = Some(!first);
        assert_eq!(p.available(), !first);
    }
}
