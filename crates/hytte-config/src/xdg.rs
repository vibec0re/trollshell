//! Where a subsystem's config layers and its state file live (#868).
//!
//! Three directories, and the whole point of the phase is that they are three
//! and not one:
//!
//! ```text
//! $XDG_CONFIG_DIRS/trollshell/<subsystem>.toml   base, nix-written, read-only
//! $XDG_CONFIG_HOME/trollshell/<subsystem>.toml   overlay, yours, never touched by nix
//! $XDG_STATE_HOME/trollshell/<subsystem>.toml    state, shell-written, never yours
//! ```
//!
//! # Why `XDG_CONFIG_DIRS` and not a new variable
//!
//! `XDG_CONFIG_DIRS` *is* the standard search path for exactly this layering,
//! so home-manager can put a store path on it via `home.sessionVariables` and
//! write the base there, and a system-wide `/etc/xdg` install works the same
//! way with no extra wiring. A rebuild can never clobber a hand edit (nix
//! writes only into the store path), and a hand edit can never block a rebuild
//! (nix never reads the overlay). #866 settled this; `trollshell`'s
//! `plugin_launcher` already resolves `plugins.json` the same way, so the
//! convention is not new to the repo — what *is* new here is that the layers
//! are **merged** rather than first-existing-wins.
//!
//! # Precedence, and why the base dirs are reversed
//!
//! The XDG spec orders `XDG_CONFIG_DIRS` **most important first**. [`merge_all`]
//! (see [`crate::merge`]) applies layers left to right with the later one
//! winning, so [`Env::config_layers`] hands back the search path *reversed* —
//! lowest precedence first — with the overlay last. Getting that backwards
//! would silently make the least important system directory beat the most
//! important one, which is why [`Env::config_layers`] is a pure function over an
//! explicit [`Env`] and has a test that names three directories and checks the
//! order.
//!
//! # Testability
//!
//! Every rule here is a method on [`Env`], a plain struct of the four variables
//! that matter. Tests construct one literally, so none of them can be
//! perturbed by (or perturb) the developer's real environment;
//! [`Env::from_process`] is the only function that reads the process
//! environment at all, and the free functions at the bottom of this module are
//! its one-line wrappers.

use std::path::PathBuf;

/// Directory, under each XDG base, that every trollshell file lives in.
pub const APP_DIR: &str = "trollshell";

/// Fallback for an unset/empty `XDG_CONFIG_DIRS`, per the XDG base directory
/// spec.
const DEFAULT_CONFIG_DIRS: &str = "/etc/xdg";

/// The four environment variables that decide where config and state live.
///
/// A value is treated as unset when it is absent **or empty** — the spec says
/// an empty value means "use the default", and an empty `XDG_CONFIG_HOME` that
/// resolved to a bare relative path would put config in the process's working
/// directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Env {
    /// `$HOME` — the fallback base for both `XDG_CONFIG_HOME` and
    /// `XDG_STATE_HOME`.
    pub home: Option<String>,
    /// `$XDG_CONFIG_HOME`, defaulting to `$HOME/.config`.
    pub config_home: Option<String>,
    /// `$XDG_CONFIG_DIRS`, colon-separated, most important first; defaulting
    /// to `/etc/xdg`.
    pub config_dirs: Option<String>,
    /// `$XDG_STATE_HOME`, defaulting to `$HOME/.local/state`.
    pub state_home: Option<String>,
}

/// `value` unless it is `None` or empty.
fn nonempty(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|v| !v.is_empty())
}

impl Env {
    /// Read the live process environment. The **only** function in this module
    /// that does; everything else is a pure method on the result.
    #[must_use]
    pub fn from_process() -> Self {
        let var = |name: &str| std::env::var(name).ok();
        Self {
            home: var("HOME"),
            config_home: var("XDG_CONFIG_HOME"),
            config_dirs: var("XDG_CONFIG_DIRS"),
            state_home: var("XDG_STATE_HOME"),
        }
    }

    /// `$XDG_CONFIG_HOME`, else `$HOME/.config`. `None` when neither is set.
    #[must_use]
    pub fn config_home(&self) -> Option<PathBuf> {
        nonempty(self.config_home.as_ref()).map_or_else(
            || nonempty(self.home.as_ref()).map(|h| PathBuf::from(h).join(".config")),
            |dir| Some(PathBuf::from(dir)),
        )
    }

    /// `$XDG_STATE_HOME`, else `$HOME/.local/state`. `None` when neither is set.
    #[must_use]
    pub fn state_home(&self) -> Option<PathBuf> {
        nonempty(self.state_home.as_ref()).map_or_else(
            || nonempty(self.home.as_ref()).map(|h| PathBuf::from(h).join(".local").join("state")),
            |dir| Some(PathBuf::from(dir)),
        )
    }

    /// `$XDG_CONFIG_DIRS`, split on `:`, **most important first** — i.e. still
    /// in the spec's own order. Empty entries are dropped (a `::` or a leading
    /// colon would otherwise name the working directory).
    #[must_use]
    pub fn config_dirs(&self) -> Vec<PathBuf> {
        nonempty(self.config_dirs.as_ref())
            .unwrap_or(DEFAULT_CONFIG_DIRS)
            .split(':')
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .collect()
    }

    /// Every `<subsystem>.toml` layer, **lowest precedence first**: the
    /// `XDG_CONFIG_DIRS` entries in reverse spec order, then the overlay.
    ///
    /// This is the order [`crate::merge::merge_all`] consumes, so a caller
    /// never has to think about which end wins. Paths are returned whether or
    /// not the file exists — the loader skips the missing ones.
    #[must_use]
    pub fn config_layers(&self, subsystem: &str) -> Vec<PathBuf> {
        let file = file_name(subsystem);
        let mut layers: Vec<PathBuf> = self
            .config_dirs()
            .into_iter()
            .rev()
            .map(|dir| dir.join(APP_DIR).join(&file))
            .collect();
        layers.extend(self.config_home().map(|dir| dir.join(APP_DIR).join(&file)));
        layers
    }

    /// The single writable config layer:
    /// `$XDG_CONFIG_HOME/trollshell/<subsystem>.toml`.
    ///
    /// Nothing in the workspace may write to a `XDG_CONFIG_DIRS` entry — those
    /// are nix's, and on NixOS they are literally a read-only store path.
    #[must_use]
    pub fn overlay_path(&self, subsystem: &str) -> Option<PathBuf> {
        self.config_home()
            .map(|dir| dir.join(APP_DIR).join(file_name(subsystem)))
    }

    /// `$XDG_STATE_HOME/trollshell/<subsystem>.toml` — what the shell writes
    /// on a toggle, and what the operator never edits.
    #[must_use]
    pub fn state_path(&self, subsystem: &str) -> Option<PathBuf> {
        self.state_home()
            .map(|dir| dir.join(APP_DIR).join(file_name(subsystem)))
    }
}

/// `<subsystem>.toml`.
///
/// `subsystem` is a compile-time constant on a [`crate::subsystem::Subsystem`]
/// impl, never user input, so this does not try to sanitise a name containing
/// a path separator — it would have to be written into the source to get here.
fn file_name(subsystem: &str) -> String {
    format!("{subsystem}.toml")
}

/// [`Env::config_layers`] against the process environment.
#[must_use]
pub fn config_layers(subsystem: &str) -> Vec<PathBuf> {
    Env::from_process().config_layers(subsystem)
}

/// [`Env::overlay_path`] against the process environment.
#[must_use]
pub fn overlay_path(subsystem: &str) -> Option<PathBuf> {
    Env::from_process().overlay_path(subsystem)
}

/// [`Env::state_path`] against the process environment.
#[must_use]
pub fn state_path(subsystem: &str) -> Option<PathBuf> {
    Env::from_process().state_path(subsystem)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An environment with every variable set, so no test accidentally leans
    /// on a default it did not mean to exercise.
    fn env() -> Env {
        Env {
            home: Some("/home/annika".into()),
            config_home: Some("/home/annika/.config".into()),
            config_dirs: Some("/etc/xdg".into()),
            state_home: Some("/home/annika/.local/state".into()),
        }
    }

    fn strs(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
    }

    /// The rule the whole module exists for: base first, overlay last, and
    /// `XDG_CONFIG_DIRS`' own order reversed on the way, because the spec
    /// orders it most-important-first while the merge applies last-wins.
    ///
    /// Drop the `.rev()` in [`Env::config_layers`] and this goes red: the
    /// nix-written store path would end up beating `/etc/xdg` the wrong way
    /// round.
    #[test]
    fn layers_run_lowest_precedence_first_with_the_overlay_last() {
        let env = Env {
            config_dirs: Some("/nix/store/aaa-trollshell-config:/etc/xdg".into()),
            ..env()
        };

        assert_eq!(
            strs(&env.config_layers("core-leds")),
            [
                // Least important of the two base dirs, so applied first…
                "/etc/xdg/trollshell/core-leds.toml",
                // …then the more important one overrides it…
                "/nix/store/aaa-trollshell-config/trollshell/core-leds.toml",
                // …and the user's overlay overrides everything.
                "/home/annika/.config/trollshell/core-leds.toml",
            ]
        );
    }

    #[test]
    fn the_overlay_is_the_last_layer_and_the_only_writable_one() {
        let env = env();
        let layers = env.config_layers("core-leds");

        assert_eq!(
            layers.last(),
            env.overlay_path("core-leds").as_ref(),
            "the writable path must be the highest-precedence layer"
        );
    }

    /// #866's third decision, and the reason this phase exists at all: state
    /// does not live beside config. Point `state_home` at `config_home` in
    /// [`Env::state_home`] and this fails.
    #[test]
    fn state_never_shares_a_directory_with_config() {
        let env = env();
        let config = env.overlay_path("dnd").expect("config home resolves");
        let state = env.state_path("dnd").expect("state home resolves");

        assert_ne!(config, state);
        assert_ne!(
            config.parent(),
            state.parent(),
            "same filename in the same directory would make the two indistinguishable"
        );
        assert_eq!(
            strs(&[config, state]),
            [
                "/home/annika/.config/trollshell/dnd.toml",
                "/home/annika/.local/state/trollshell/dnd.toml",
            ]
        );
    }

    #[test]
    fn unset_variables_fall_back_to_the_xdg_defaults() {
        let env = Env {
            home: Some("/home/annika".into()),
            ..Env::default()
        };

        assert_eq!(
            strs(&env.config_layers("weather")),
            [
                "/etc/xdg/trollshell/weather.toml",
                "/home/annika/.config/trollshell/weather.toml",
            ]
        );
        assert_eq!(
            env.state_path("weather").map(|p| p.display().to_string()),
            Some("/home/annika/.local/state/trollshell/weather.toml".into())
        );
    }

    /// An empty variable means "use the default" — not "use the working
    /// directory", which is what a naive `unwrap_or_default` would give.
    #[test]
    fn empty_variables_are_treated_as_unset() {
        let env = Env {
            home: Some("/home/annika".into()),
            config_home: Some(String::new()),
            config_dirs: Some(String::new()),
            state_home: Some(String::new()),
        };

        assert_eq!(
            strs(&env.config_layers("pet")),
            [
                "/etc/xdg/trollshell/pet.toml",
                "/home/annika/.config/trollshell/pet.toml",
            ]
        );
        assert_eq!(
            env.state_path("pet").map(|p| p.display().to_string()),
            Some("/home/annika/.local/state/trollshell/pet.toml".into())
        );
    }

    /// A stray `::` or trailing colon must not name the process's working
    /// directory as a config source.
    #[test]
    fn blank_config_dirs_entries_are_dropped() {
        let env = Env {
            config_dirs: Some(":/etc/xdg::/usr/local/etc/xdg:".into()),
            ..env()
        };

        assert_eq!(
            strs(&env.config_layers("caw")),
            [
                "/usr/local/etc/xdg/trollshell/caw.toml",
                "/etc/xdg/trollshell/caw.toml",
                "/home/annika/.config/trollshell/caw.toml",
            ]
        );
    }

    /// No `$HOME` and no `$XDG_CONFIG_HOME`: there is still a base to read,
    /// but nowhere to write. The loader must degrade rather than invent a path.
    #[test]
    fn without_a_home_there_is_a_base_layer_but_no_overlay() {
        let env = Env {
            config_dirs: Some("/etc/xdg".into()),
            ..Env::default()
        };

        assert_eq!(
            strs(&env.config_layers("usage")),
            ["/etc/xdg/trollshell/usage.toml"]
        );
        assert_eq!(env.overlay_path("usage"), None);
        assert_eq!(env.state_path("usage"), None);
    }
}
