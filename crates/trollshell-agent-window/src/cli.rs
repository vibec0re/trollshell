//! The window's command line, and the application id it picks from it.
//!
//! Hand-parsed rather than `clap`, and it has to stay that way: hyperhive's
//! `Ident` admits a **leading hyphen** (`-leading-hyphen` is a legal agent
//! name), and this parser takes the token after `--agent` unconditionally,
//! without re-examining it for a `-`. `clap` would read that name as a flag.
//! The rule is asserted from both ends — `hytte_plugin_agents::window`'s
//! `a_leading_hyphen_name_is_its_own_argv_element` and
//! `the_plugins_own_argv_parses_on_both_tabs` here, which feeds the
//! *real* builder's output through the *real* parser. Both are named in plain
//! code rather than linked: they live in `#[cfg(test)]` modules, which rustdoc
//! never has in scope, so a link would be a `broken_intra_doc_links` error
//! under `checks.rustdoc`'s `-D warnings` (#1328).
//!
//! Two shapes, not one, since
//! [#1306](https://github.com/vibec0re/trollshell/issues/1306): one window for
//! one agent ([`Args`]) and the fan-out that opens all of them
//! ([`Invocation::OpenAll`]). They are an enum rather than an `Option<AgentName>`
//! because they do not share a `main` — the fan-out registers no
//! `GApplication`, builds no window and exits, so "which one is this" has to be
//! answered before an application id can even be derived.

use hytte_plugin_agents::model::AgentName;

/// The app-id prefix. One id **per agent** (see [`app_id`]).
pub const APP_ID_PREFIX: &str = "mov.vibec0re.trollshell.AgentWindow";

/// Which tab the window opens on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    /// hyperhive's agent page in the `WebKitGTK` view.
    #[default]
    Agent,
    /// Our own settings page — what the card's pen opens (Annika on #947,
    /// 2026-09-11 07:43Z).
    Settings,
}

impl Tab {
    /// The `adw::ViewStack` child name, which is also the `--tab` word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Settings => "settings",
        }
    }

    /// Parse one `--tab` value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "agent" => Some(Self::Agent),
            "settings" => Some(Self::Settings),
            _ => None,
        }
    }
}

/// What the window was asked to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Args {
    /// The agent, validated against hyperhive's own `Ident` charset before it
    /// reaches a socket, a window title or an app id.
    pub agent: AgentName,
    /// The tab to open on.
    pub tab: Tab,
}

/// What this process was asked to be.
///
/// Both arms are reachable from the same binary because the fan-out's whole
/// job is to launch the other arm N times — one binary, one `PATH` entry, one
/// nix slice, and no way for the two halves to disagree about the argv that
/// joins them (`hytte_plugin_agents::window::argv` builds it for both the
/// plugin and [`crate::open_all`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invocation {
    /// One agent's companion window — #950's mode, and the default.
    Window(Args),
    /// [#1306](https://github.com/vibec0re/trollshell/issues/1306): open one
    /// window per **running** agent on a fresh niri workspace, then exit.
    /// Carries nothing — the roster comes off `host.sock` and the workspace
    /// off `$NIRI_SOCKET`, both read at run time.
    OpenAll,
}

impl Invocation {
    /// The window arm's arguments, for a caller that has already established
    /// which arm it is holding.
    #[must_use]
    pub fn window(self) -> Option<Args> {
        match self {
            Self::Window(args) => Some(args),
            Self::OpenAll => None,
        }
    }
}

/// Why a command line could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// No `--agent` at all.
    MissingAgent,
    /// `--open-all` was combined with a flag that describes **one** window.
    ///
    /// Refused rather than resolved in either direction: silently ignoring
    /// `--agent` would open every agent when one was asked for, and silently
    /// ignoring `--open-all` would open one when every was.
    OpenAllWithWindowFlag(String),
    /// A flag that takes a value did not get one.
    MissingValue(String),
    /// `--agent` was given a name hyperhive would refuse.
    BadAgent(String),
    /// `--tab` was given a word this window has no page for.
    BadTab(String),
    /// An argument this window does not know.
    Unknown(String),
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingAgent => f.write_str("no --agent given"),
            Self::OpenAllWithWindowFlag(flag) => write!(
                f,
                "--open-all opens every running agent's window and cannot be combined with {flag}"
            ),
            Self::MissingValue(flag) => write!(f, "{flag} needs a value"),
            Self::BadAgent(raw) => write!(
                f,
                "{raw:?} is not a legal agent name (hyperhive's Ident: a-z, 0-9 and -)"
            ),
            Self::BadTab(raw) => write!(f, "no such tab: {raw:?} (agent | settings)"),
            Self::Unknown(arg) => write!(f, "unknown argument {arg:?}"),
        }
    }
}

/// The usage line, printed on a bad command line.
pub const USAGE: &str = "usage: trollshell-agent-window --agent <name> [--tab agent|settings]\n   \
                         or: trollshell-agent-window --open-all";

/// Parse an argv **without** its program name.
///
/// # Errors
/// [`Invalid`], one variant per way a command line can be unusable. Every one
/// of them is a sentence an operator can act on, because this binary is
/// normally launched by the agents plugin and a human only ever types it when
/// something already went wrong.
pub fn parse<I, S>(args: I) -> Result<Invocation, Invalid>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut agent = None;
    let mut tab = Tab::default();
    let mut open_all = false;
    let mut window_flag: Option<String> = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_ref() {
            // #1306. A whole mode rather than a value, so it is parsed here
            // and the conflict checked once at the end — an `--open-all
            // --agent x` in either order is the same refusal.
            hytte_plugin_agents::window::ARG_OPEN_ALL => open_all = true,
            "--agent" => {
                window_flag.get_or_insert_with(|| "--agent".to_owned());
                let raw = it
                    .next()
                    .ok_or_else(|| Invalid::MissingValue("--agent".to_owned()))?;
                agent = Some(
                    AgentName::parse(raw.as_ref())
                        .ok_or_else(|| Invalid::BadAgent(raw.as_ref().to_owned()))?,
                );
            }
            "--tab" => {
                window_flag.get_or_insert_with(|| "--tab".to_owned());
                let raw = it
                    .next()
                    .ok_or_else(|| Invalid::MissingValue("--tab".to_owned()))?;
                tab = Tab::parse(raw.as_ref())
                    .ok_or_else(|| Invalid::BadTab(raw.as_ref().to_owned()))?;
            }
            other => return Err(Invalid::Unknown(other.to_owned())),
        }
    }
    if open_all {
        return match window_flag {
            Some(flag) => Err(Invalid::OpenAllWithWindowFlag(flag)),
            None => Ok(Invocation::OpenAll),
        };
    }
    Ok(Invocation::Window(Args {
        agent: agent.ok_or(Invalid::MissingAgent)?,
        tab,
    }))
}

/// The `GApplication` id for one agent — **one id per agent**, which is how
/// "one window per agent" is enforced.
///
/// # Why per-agent, and not one id with a window registry
///
/// `GApplication` already does single-instance for us: a second process with
/// the same id finds the primary instance over the session bus, hands it the
/// command line (this app sets
/// [`HANDLES_COMMAND_LINE`](gtk::gio::ApplicationFlags::HANDLES_COMMAND_LINE),
/// so `--tab settings` from the pen is *forwarded* rather than lost) and
/// exits. Keyed per agent, that machinery is the whole feature: a second
/// `--agent stray` presents the existing stray window, and a first
/// `--agent other` is simply a different application.
///
/// The alternative — one shared id plus a `HashMap<AgentName, Window>` — costs
/// a registry, a lifetime for it, and one process whose `WebKit` crash takes
/// every agent's window with it. It would buy one thing back: a single app-id
/// for niri window rules. That is recoverable with a prefix regex on
/// [`APP_ID_PREFIX`], which `docs/live-verify.md` spells out, so it is the
/// cheaper side of the trade.
///
/// # The mangling
///
/// `GLib` requires each dot-separated element to be ASCII alphanumeric plus
/// `-`/`_` and to start with a letter, where hyperhive's `Ident` allows a name
/// to start with a digit or a `-` (`[a-z0-9-]{1,63}`). So every character
/// outside `[A-Za-z0-9_]` becomes `_` and a leading non-letter is prefixed —
/// a **total** function, because a name that reached here already passed
/// [`AgentName::parse`] and must not be able to produce an id `GLib` rejects at
/// `Application::new`, which aborts.
///
/// Two agents can therefore share an id only by colliding under that mangling
/// (`a-b` and `a_b`), and `Ident` forbids `_`, so within one hive they cannot.
#[must_use]
pub fn app_id(agent: &AgentName) -> String {
    let mut suffix = String::with_capacity(agent.as_str().len() + 1);
    let mut chars = agent.as_str().chars();
    if let Some(first) = chars.next()
        && !first.is_ascii_alphabetic()
    {
        suffix.push('a');
    }
    for c in agent.as_str().chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            suffix.push(c);
        } else {
            suffix.push('_');
        }
    }
    format!("{APP_ID_PREFIX}.{suffix}")
}

#[cfg(test)]
mod tests {
    use super::{APP_ID_PREFIX, Args, Invalid, Invocation, Tab, USAGE, app_id, parse};
    use hytte_plugin_agents::model::AgentName;

    fn name(s: &str) -> AgentName {
        AgentName::parse(s).expect("a legal test name")
    }

    /// Parse, insisting on the one-window arm — the shape every #950-era
    /// assertion here describes.
    fn window<I, S>(args: I) -> Result<Args, Invalid>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        parse(args).map(|i| i.window().expect("the one-window arm"))
    }

    /// The launch the agents plugin emits for a row click — `--agent` alone —
    /// lands on the agent page.
    #[test]
    fn the_plain_launch_opens_the_agent_page() {
        assert_eq!(
            window(["--agent", "trollshell-choom"]),
            Ok(Args {
                agent: name("trollshell-choom"),
                tab: Tab::Agent,
            })
        );
    }

    /// The launch the **pen** emits lands on the settings page, and
    /// `--tab agent` — the spelling `window::argv` deliberately omits — is
    /// accepted too, so the two ends cannot drift into an unparseable pair.
    #[test]
    fn the_pens_launch_opens_the_settings_page() {
        assert_eq!(
            window(["--agent", "stray", "--tab", "settings"]).map(|a| a.tab),
            Ok(Tab::Settings)
        );
        assert_eq!(
            window(["--agent", "stray", "--tab", "agent"]).map(|a| a.tab),
            Ok(Tab::Agent)
        );
    }

    /// Exactly the argv `hytte_plugin_agents::window::argv` builds parses —
    /// asserted against **that function**, not against a copy of its output,
    /// so a change on either side breaks this rather than shipping a launcher
    /// the window cannot read.
    ///
    /// Mutation (verified red, #1130 review M10): rename a flag on either side.
    ///
    /// `"-leading-hyphen"` is the fixture `hytte-plugin-agents`' `AgentName`
    /// test marks as an argv concern (#1167): this parser takes the token after
    /// `--agent` unconditionally, so a legal name that starts with `-` is a
    /// name here and not a flag — asserted through the real builder so a
    /// change to either side (an `=`-joined form, a re-examined value) reds.
    #[test]
    fn the_plugins_own_argv_parses_on_both_tabs() {
        use hytte_plugin_agents::window;
        for name in ["trollshell-choom", "-leading-hyphen"] {
            for (tab, expected) in [
                (window::Tab::Agent, Tab::Agent),
                (window::Tab::Settings, Tab::Settings),
            ] {
                let argv = window::argv(name, tab);
                assert_eq!(argv[0], "trollshell-agent-window", "the binary name");
                let parsed = window(&argv[1..]).expect("the plugin's own argv parses");
                assert_eq!(parsed.agent.as_str(), name);
                assert_eq!(parsed.tab, expected);
            }
        }
    }

    /// #1306's fan-out is its own arm, and the **plugin's own builder** is
    /// what produces the argv this parses — asserted against
    /// `hytte_plugin_agents::window::open_all_argv` rather than against a copy
    /// of its output, for `the_plugins_own_argv_parses_on_both_tabs`' reason:
    /// the two ends are in different crates and a literal here would let one
    /// move.
    ///
    /// Falsification: rename the flag on either side and this reds.
    #[test]
    fn the_plugins_fan_out_argv_parses_as_the_open_all_arm() {
        let argv = hytte_plugin_agents::window::open_all_argv();
        assert_eq!(argv[0], "trollshell-agent-window", "the binary name");
        assert_eq!(parse(&argv[1..]), Ok(Invocation::OpenAll));
        assert_eq!(
            parse(["--open-all"]).map(Invocation::window),
            Ok(None),
            "the fan-out is not a window arm; `main` must not try to build one"
        );
    }

    /// `--open-all` and the one-window flags are refused together, in **both
    /// orders**, rather than one silently winning.
    ///
    /// Either resolution would be a surprise in the expensive direction: with
    /// `--agent` winning, a fan-out opens one window; with `--open-all`
    /// winning, a request for one agent opens the whole hive.
    ///
    /// Falsification: return `Ok(Invocation::OpenAll)` regardless of the
    /// window flags (or check the conflict only in one order) and this reds.
    #[test]
    fn the_fan_out_refuses_to_be_combined_with_a_one_window_flag() {
        for argv in [
            vec!["--open-all", "--agent", "stray"],
            vec!["--agent", "stray", "--open-all"],
        ] {
            assert_eq!(
                parse(&argv),
                Err(Invalid::OpenAllWithWindowFlag("--agent".to_owned())),
                "{argv:?}"
            );
        }
        assert_eq!(
            parse(["--open-all", "--tab", "settings"]),
            Err(Invalid::OpenAllWithWindowFlag("--tab".to_owned())),
        );
        assert!(
            Invalid::OpenAllWithWindowFlag("--agent".to_owned())
                .to_string()
                .contains("--agent"),
            "the sentence names the flag that clashed"
        );
        assert!(
            USAGE.contains("--open-all"),
            "…and the usage line offers the mode that was refused"
        );
    }

    /// Every way a command line can be wrong says which way, because a human
    /// only types this when something already went wrong.
    #[test]
    fn a_bad_command_line_names_what_is_wrong() {
        assert_eq!(window::<_, &str>([]), Err(Invalid::MissingAgent));
        assert_eq!(
            window(["--agent"]),
            Err(Invalid::MissingValue("--agent".to_owned()))
        );
        assert_eq!(
            window(["--agent", "stray", "--tab"]),
            Err(Invalid::MissingValue("--tab".to_owned()))
        );
        assert_eq!(
            window(["--agent", "../etc/passwd"]),
            Err(Invalid::BadAgent("../etc/passwd".to_owned())),
            "the §11 whitelist runs before the name reaches a socket or a title"
        );
        assert_eq!(
            window(["--agent", "stray", "--tab", "stats"]),
            Err(Invalid::BadTab("stats".to_owned()))
        );
        assert_eq!(
            window(["--verbose"]),
            Err(Invalid::Unknown("--verbose".to_owned()))
        );
        for e in [
            Invalid::MissingAgent,
            Invalid::BadAgent("x".to_owned()),
            Invalid::BadTab("x".to_owned()),
        ] {
            assert!(!e.to_string().is_empty());
        }
    }

    /// **The dedup decision**: the id is a pure function of the agent's name —
    /// the same string in every process, for ever.
    ///
    /// This is the whole of "a second launch for the same agent focuses the
    /// existing one" that is ours; the rest is `GApplication`'s, which keys its
    /// single-instance registration on exactly this string. So the assertion
    /// has to be the **exact** string, not two calls compared to each other:
    /// a per-launch component (a pid, a timestamp, a counter) is identical
    /// within one process and would sail through a self-comparison while
    /// breaking the feature entirely — measured, on the mutation below.
    ///
    /// Mutation (verified red, #1130 review M4): append `std::process::id()` — or anything else
    /// that is not the name — and the first assertion reds. (The
    /// self-comparison this test used to make stayed **green** on that
    /// mutation, which is why it is no longer the assertion.) Drop the agent
    /// from the id and the distinctness assertion reds.
    #[test]
    fn one_app_id_per_agent_stable_across_launches() {
        assert_eq!(
            app_id(&name("stray")),
            "mov.vibec0re.trollshell.AgentWindow.stray",
            "the id is the prefix and the mangled name, and nothing else — \
             anything per-launch here means a second window per launch"
        );
        assert_eq!(
            app_id(&name("trollshell-choom")),
            "mov.vibec0re.trollshell.AgentWindow.trollshell_choom"
        );
        assert_eq!(
            app_id(&name("9-lives")),
            "mov.vibec0re.trollshell.AgentWindow.a9_lives",
            "the leading-letter fix is part of the pure function too"
        );
        assert_ne!(app_id(&name("stray")), app_id(&name("nixos-choom")));
        assert!(app_id(&name("stray")).starts_with(APP_ID_PREFIX));
    }

    /// Every id this can produce is one `GLib` will accept — asked of **`GLib`**,
    /// over the whole `Ident` charset including the shapes that need the
    /// mangling (a leading digit, a leading and trailing hyphen, the 63-char
    /// maximum).
    ///
    /// `Application::new` aborts the process on an invalid id, so a name that
    /// passed `AgentName::parse` producing one would be a crash on click.
    ///
    /// Mutation (verified red, #1130 review M5): drop the leading-letter prefix and the
    /// `9-lives` case reds; drop the `-` → `_` mapping and every hyphenated
    /// name does.
    #[test]
    fn every_reachable_app_id_is_one_glib_accepts() {
        for raw in [
            "a",
            "stray",
            "trollshell-choom",
            "9-lives",
            "-leading-hyphen",
            "trailing-hyphen-",
            "0",
            &"z-9".repeat(21),
        ] {
            let n = AgentName::parse(raw).unwrap_or_else(|| panic!("{raw} is a legal Ident"));
            let id = app_id(&n);
            assert!(
                gtk::gio::Application::id_is_valid(&id),
                "{raw} produced the invalid app id {id}"
            );
        }
    }
}
