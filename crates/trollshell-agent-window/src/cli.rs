//! The window's command line, and the application id it picks from it.
//!
//! Hand-parsed rather than `clap`: two flags, both required-shaped, and the
//! workspace has no `clap` in `Cargo.lock` — adding one to read `--agent`
//! would be a resolved package per flag.

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

/// Why a command line could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// No `--agent` at all.
    MissingAgent,
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
pub const USAGE: &str = "usage: trollshell-agent-window --agent <name> [--tab agent|settings]";

/// Parse an argv **without** its program name.
///
/// # Errors
/// [`Invalid`], one variant per way a command line can be unusable. Every one
/// of them is a sentence an operator can act on, because this binary is
/// normally launched by the agents plugin and a human only ever types it when
/// something already went wrong.
pub fn parse<I, S>(args: I) -> Result<Args, Invalid>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut agent = None;
    let mut tab = Tab::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_ref() {
            "--agent" => {
                let raw = it
                    .next()
                    .ok_or_else(|| Invalid::MissingValue("--agent".to_owned()))?;
                agent = Some(
                    AgentName::parse(raw.as_ref())
                        .ok_or_else(|| Invalid::BadAgent(raw.as_ref().to_owned()))?,
                );
            }
            "--tab" => {
                let raw = it
                    .next()
                    .ok_or_else(|| Invalid::MissingValue("--tab".to_owned()))?;
                tab = Tab::parse(raw.as_ref())
                    .ok_or_else(|| Invalid::BadTab(raw.as_ref().to_owned()))?;
            }
            other => return Err(Invalid::Unknown(other.to_owned())),
        }
    }
    Ok(Args {
        agent: agent.ok_or(Invalid::MissingAgent)?,
        tab,
    })
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
    use super::{APP_ID_PREFIX, Args, Invalid, Tab, app_id, parse};
    use hytte_plugin_agents::model::AgentName;

    fn name(s: &str) -> AgentName {
        AgentName::parse(s).expect("a legal test name")
    }

    /// The launch the agents plugin emits for a row click — `--agent` alone —
    /// lands on the agent page.
    #[test]
    fn the_plain_launch_opens_the_agent_page() {
        assert_eq!(
            parse(["--agent", "trollshell-choom"]),
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
            parse(["--agent", "stray", "--tab", "settings"]).map(|a| a.tab),
            Ok(Tab::Settings)
        );
        assert_eq!(
            parse(["--agent", "stray", "--tab", "agent"]).map(|a| a.tab),
            Ok(Tab::Agent)
        );
    }

    /// Exactly the argv `hytte_plugin_agents::window::argv` builds parses —
    /// asserted against **that function**, not against a copy of its output,
    /// so a change on either side breaks this rather than shipping a launcher
    /// the window cannot read.
    ///
    /// Mutation (verified red): rename a flag on either side.
    #[test]
    fn the_plugins_own_argv_parses_on_both_tabs() {
        use hytte_plugin_agents::window;
        for (tab, expected) in [
            (window::Tab::Agent, Tab::Agent),
            (window::Tab::Settings, Tab::Settings),
        ] {
            let argv = window::argv("trollshell-choom", tab);
            assert_eq!(argv[0], "trollshell-agent-window", "the binary name");
            let parsed = parse(&argv[1..]).expect("the plugin's own argv parses");
            assert_eq!(parsed.agent.as_str(), "trollshell-choom");
            assert_eq!(parsed.tab, expected);
        }
    }

    /// Every way a command line can be wrong says which way, because a human
    /// only types this when something already went wrong.
    #[test]
    fn a_bad_command_line_names_what_is_wrong() {
        assert_eq!(parse::<_, &str>([]), Err(Invalid::MissingAgent));
        assert_eq!(
            parse(["--agent"]),
            Err(Invalid::MissingValue("--agent".to_owned()))
        );
        assert_eq!(
            parse(["--agent", "stray", "--tab"]),
            Err(Invalid::MissingValue("--tab".to_owned()))
        );
        assert_eq!(
            parse(["--agent", "../etc/passwd"]),
            Err(Invalid::BadAgent("../etc/passwd".to_owned())),
            "the §11 whitelist runs before the name reaches a socket or a title"
        );
        assert_eq!(
            parse(["--agent", "stray", "--tab", "stats"]),
            Err(Invalid::BadTab("stats".to_owned()))
        );
        assert_eq!(
            parse(["--verbose"]),
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

    /// **The dedup decision**: one id per agent, the same one every time.
    ///
    /// This is the whole of "a second launch for the same agent focuses the
    /// existing one" that is ours — the rest is `GApplication`'s, which keys its
    /// single-instance registration on exactly this string.
    ///
    /// Mutation (verified red): put anything per-launch in the id (a pid, a
    /// timestamp) and the stability assertion goes red; drop the agent from it
    /// and the distinctness one does.
    #[test]
    fn one_app_id_per_agent_stable_across_launches() {
        assert_eq!(app_id(&name("stray")), app_id(&name("stray")));
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
    /// Mutation (verified red): drop the leading-letter prefix and the
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
