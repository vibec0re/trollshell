//! The shell's own **config-file** subsystems: `~/.config/trollshell/*.toml`
//! read through `hytte_config`'s layering (#866 / #868), and the deprecation
//! shape every migrated environment variable warns in.
//!
//! # What lives here, and what does not
//!
//! `hytte-config` owns the *mechanism* — the XDG search path, the four merge
//! rules, the [`Subsystem`](hytte_config::subsystem::Subsystem) trait, the
//! format-preserving writer. This module owns the shell's *schemas*: one
//! module per subsystem, each declaring a type, a file name and a documented
//! `DEFAULT_TOML`.
//!
//! They are here rather than in a shared leaf crate because #869 is a pilot:
//! one subsystem, deliberately small enough to throw away and redo if the
//! shape turns out wrong. #888 (schema-derived forms in the control-center's
//! Plugins tab) is the thread that would move a schema down into a crate both
//! the shell and the companion app can link, and it should move a shape that
//! has been used rather than one that has only been designed.
//!
//! # Two conventions the pilot writes down for the nine subsystems after it
//!
//! **`_unset = ["key"]` is reserved.** TOML has no null, so #868 spells the
//! "unset a key an underlying layer set" half of the scalar merge rule as an
//! array of key names under the literal key `_unset`
//! ([`hytte_config::merge::UNSET_KEY`]). It is an invention, and it means a
//! key genuinely named `_unset` cannot be a schema key in any subsystem.
//!
//! **Config vs. state is a rule, not a list.** *Config is what nix could
//! write* — declarative, reproducible, checkable into home-manager. *State is
//! what only this machine has* — machine-local and not reproducible, which is
//! why a secret is state and not config. Config layers under
//! `$XDG_CONFIG_DIRS` + `$XDG_CONFIG_HOME`; state lives under
//! `$XDG_STATE_HOME` ([`hytte_config::state`]) and is written by the shell,
//! never by hand. `core-leds.toml` is pure config: no state, no secret.
//!
//! # The deprecation window
//!
//! #866 settled three steps, and the pilot is step 2 for its four variables:
//! the environment variable is still read and still **wins**, but a set one
//! warns **once at startup** naming the file and the key it moves to
//! ([`warn_deprecated_env`]). Step 3 — removing the variable — is a later
//! decision, not this one.

pub mod core_leds;

use std::path::Path;

use hytte_config::xdg;

/// The one message a deprecated-but-still-honoured environment variable
/// produces.
///
/// A function rather than a literal at the call site for the reason
/// `hytte_config::subsystem`'s `MALFORMED_UNSET_MESSAGE` is a `const`: the
/// tests select on the **exact** rendered line, so a `contains("deprecated")`
/// filter cannot turn green-and-blind the day somebody rewords it — including
/// the negative test, whose whole job is to observe that an *unset* variable
/// warns about nothing.
///
/// The sentence form (rather than a static message plus fields) is deliberate,
/// and it is the half of #869 that nine later subsystems copy: the line has to
/// read as an instruction in `journalctl -f` without the reader assembling it
/// out of structured fields. The fields are emitted as well, for anything that
/// wants to filter on them.
pub fn deprecation_message(var: &str, key: &str, file: &str) -> String {
    format!("{var} is deprecated; set `{key}` in {file}")
}

/// Where a subsystem's overlay lives, as a string fit for a log line.
///
/// The **real** resolved path (`$XDG_CONFIG_HOME`, or `$HOME/.config`) rather
/// than a hard-coded `~/.config/…`, because the whole point of the line is
/// that the reader can open the file it names. The literal is the fallback for
/// the one case where no path resolves at all — neither `$XDG_CONFIG_HOME` nor
/// a usable `$HOME` — where naming the conventional location is still more
/// useful than saying nothing.
fn overlay_display(subsystem: &str) -> String {
    xdg::overlay_path(subsystem).as_deref().map_or_else(
        || format!("~/.config/{}/{subsystem}.toml", xdg::APP_DIR),
        |path: &Path| path.display().to_string(),
    )
}

/// Warn that `var` has moved into `subsystem`'s config file under `key`.
///
/// Call this **once per set variable at startup** — not per read, and not on a
/// config reload. A live reload re-resolves the same environment (the variable
/// still wins), so warning there would repeat the line every few seconds for
/// as long as the shell runs; see [`core_leds::Deprecations`].
pub fn warn_deprecated_env(subsystem: &str, var: &str, key: &str) {
    let file = overlay_display(subsystem);
    tracing::warn!(
        subsystem,
        var,
        key,
        file = %file,
        "{}",
        deprecation_message(var, key, &file)
    );
}
