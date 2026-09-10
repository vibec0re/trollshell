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

use hytte_config::xdg;

/// The one message a deprecated-but-still-honoured environment variable
/// produces.
///
/// A function rather than a literal at the call site for the reason
/// `hytte_config::subsystem`'s `MALFORMED_UNSET_MESSAGE` is a `const`: the
/// tests select on the **exact** rendered line, so a `contains("deprecated")`
/// filter cannot turn green-and-blind the day somebody rewords it — including
/// the negative test, whose whole job is to observe that an *unset* variable
/// warns about nothing. The *content* is pinned separately, against a literal
/// string in [`tests::the_deprecation_sentence_is_this_exact_sentence`]:
/// building the expectation by calling this function would assert it against
/// itself, and `docs/live-verify.md` quotes the line verbatim for a human to
/// match (#1040 F4).
///
/// The sentence form (rather than a static message plus fields) is deliberate,
/// and it is the half of #869 that nine later subsystems copy: the line has to
/// read as an instruction in `journalctl -f` without the reader assembling it
/// out of structured fields. The fields are emitted as well, for anything that
/// wants to filter on them.
///
/// `accepts` is the knob's vocabulary, and it is part of the sentence rather
/// than a field for the same reason (#1040 F5): the only place `core-leds.toml`
/// explains that the automatic shape is spelt `0` *or* `"rect"` is
/// `DEFAULT_TOML`, which — until nix renders a base file — exists nowhere on
/// disk for the reader to open. The line that tells a user to move a value into
/// a file has to tell them what the file accepts, or they will guess.
pub fn deprecation_message(var: &str, key: &str, file: &str, accepts: &str) -> String {
    format!("{var} is deprecated; set `{key}` in {file} — it accepts {accepts}")
}

/// The **one** line a set-but-unusable deprecated variable produces.
///
/// One line rather than two (#1040 F9): before this, a
/// `TROLLSHELL_CORE_LEDS_STYLE=plasma` produced both the deprecation sentence
/// and a separate unrecognised-value warning, and the reader got two half
/// instructions to assemble. This says all of it once — what was set, what was
/// expected, what happens instead, and where the value belongs now.
///
/// It is emitted on a **reload** as well as at startup, unlike
/// [`deprecation_message`]: an unusable variable is still unusable after an
/// edit, and the line is a fact about the current environment rather than a
/// one-off announcement. That is also why it never says "deprecated" in the
/// once-only sense the [`core_leds::Deprecations`] latch guards — the
/// deprecation *announcement* stays exactly once per set variable.
pub fn unusable_env_message(
    var: &str,
    value: &str,
    key: &str,
    file: &str,
    accepts: &str,
) -> String {
    format!(
        "{var} is set to `{value}`, which is not valid; expected {accepts} — ignoring it and taking `{key}` from {file}"
    )
}

/// Where a subsystem's overlay lives, as a string fit for a log line, resolved
/// against an explicit [`xdg::Env`].
///
/// The **real** resolved path (`$XDG_CONFIG_HOME`, or `$HOME/.config`) rather
/// than a hard-coded `~/.config/…`, because the whole point of the line is
/// that the reader can open the file it names. The literal is the fallback for
/// the one case where no path resolves at all — neither `$XDG_CONFIG_HOME` nor
/// a usable `$HOME` — where naming the conventional location is still more
/// useful than saying nothing.
///
/// The `Env` is a parameter for the reason every rule in `xdg.rs` takes one:
/// the process environment is not a thing a test may set (`std::env::set_var`
/// is an `unsafe fn` and this workspace forbids `unsafe`), so a resolution
/// asserted against `Env::from_process()` can only be asserted against itself.
/// With an `Env` in hand the result is pinnable against a literal path
/// (#1040 F4).
fn overlay_display_in(env: &xdg::Env, subsystem: &str) -> String {
    env.overlay_path(subsystem).map_or_else(
        || format!("~/.config/{}/{subsystem}.toml", xdg::APP_DIR),
        |path| path.display().to_string(),
    )
}

/// [`overlay_display_in`] against the process environment — what every
/// production log line resolves.
fn overlay_display(subsystem: &str) -> String {
    overlay_display_in(&xdg::Env::from_process(), subsystem)
}

/// Warn that `var` has moved into `subsystem`'s config file under `key`.
///
/// Call this **once per set variable at startup** — not per read, and not on a
/// config reload. A live reload re-resolves the same environment (the variable
/// still wins), so warning there would repeat the line every few seconds for
/// as long as the shell runs; see [`core_leds::Deprecations`].
pub fn warn_deprecated_env(subsystem: &str, var: &str, key: &str, accepts: &str) {
    let file = overlay_display(subsystem);
    tracing::warn!(
        subsystem,
        var,
        key,
        accepts,
        file = %file,
        "{}",
        deprecation_message(var, key, &file, accepts)
    );
}

/// Warn that `var` is set to something no parser accepts, and that the value
/// under `key` in the config file is being used instead.
///
/// The counterpart to [`warn_deprecated_env`], and deliberately *not* paired
/// with it: a variable that does not parse gets this line **only**, so the
/// unusable case costs one journal line rather than two (#1040 F9).
pub fn warn_unusable_env(subsystem: &str, var: &str, value: &str, key: &str, accepts: &str) {
    let file = overlay_display(subsystem);
    tracing::warn!(
        subsystem,
        var,
        value,
        key,
        accepts,
        file = %file,
        "{}",
        unusable_env_message(var, value, key, &file, accepts)
    );
}

#[cfg(test)]
mod tests {
    use super::{deprecation_message, overlay_display_in, unusable_env_message};
    use hytte_config::xdg;

    /// A controlled environment: one absolute `$XDG_CONFIG_HOME`, nothing else
    /// set, so every path below is a literal this test states in full.
    fn env(config_home: &str) -> xdg::Env {
        xdg::Env {
            config_home: Some(config_home.to_string()),
            ..xdg::Env::default()
        }
    }

    /// The deprecation sentence, **as a literal**.
    ///
    /// `docs/live-verify.md` quotes this line for a human to match against
    /// their journal, and #869 says in as many words "get the wording right
    /// here — nine subsystems will copy it". Building the expectation by
    /// calling [`deprecation_message`] would assert the sentence against
    /// itself and stay green through any reword (#1040 F4, mutation X3).
    ///
    /// **Red if the sentence is reworded** — deliberately, so the reword has to
    /// travel to `live-verify.md` and the nine copies in the same commit.
    #[test]
    fn the_deprecation_sentence_is_this_exact_sentence() {
        assert_eq!(
            deprecation_message(
                "TROLLSHELL_CORE_LEDS_STYLE",
                "style",
                "/x/core-leds.toml",
                "one of vfd/lcd/oled/crt"
            ),
            "TROLLSHELL_CORE_LEDS_STYLE is deprecated; set `style` in /x/core-leds.toml \
             — it accepts one of vfd/lcd/oled/crt"
        );
    }

    /// The unusable-value sentence, as a literal, for the same reason.
    ///
    /// It has to carry all four facts on its own, because it is the *only* line
    /// a set-but-unparseable variable produces (#1040 F9).
    #[test]
    fn the_unusable_value_sentence_is_this_exact_sentence() {
        assert_eq!(
            unusable_env_message(
                "TROLLSHELL_CORE_LEDS_STYLE",
                "plasma",
                "style",
                "/x/core-leds.toml",
                "one of vfd/lcd/oled/crt"
            ),
            "TROLLSHELL_CORE_LEDS_STYLE is set to `plasma`, which is not valid; \
             expected one of vfd/lcd/oled/crt — ignoring it and taking `style` \
             from /x/core-leds.toml"
        );
    }

    /// The path in the line is the **resolved overlay path**, asserted against
    /// a literal built from a controlled `$XDG_CONFIG_HOME`.
    ///
    /// **Red if `overlay_display_in` stops resolving** — returns a constant, or
    /// the `~/.config/…` fallback, or drops the `trollshell/` component
    /// (#1040 F4, mutation X2). The point of naming the real path is that the
    /// reader can open the file; a line naming a path that does not exist is
    /// worse than one naming none.
    #[test]
    fn the_line_names_the_resolved_overlay_path() {
        assert_eq!(
            overlay_display_in(&env("/x/config"), "core-leds"),
            "/x/config/trollshell/core-leds.toml"
        );
    }

    /// With nowhere to resolve — no `$XDG_CONFIG_HOME`, no usable `$HOME` —
    /// the line names the conventional location rather than saying nothing.
    #[test]
    fn with_no_config_home_the_line_names_the_conventional_path() {
        assert_eq!(
            overlay_display_in(&xdg::Env::default(), "core-leds"),
            "~/.config/trollshell/core-leds.toml"
        );
    }

    /// A relative `$XDG_CONFIG_HOME` is invalid per the XDG spec and
    /// `hytte-config` ignores it (#985) — so the line must fall back rather
    /// than name a path relative to the shell's working directory. The same
    /// gate `config_layers` goes through, on the path the line *names*.
    #[test]
    fn a_relative_config_home_falls_back_like_an_unset_one() {
        assert_eq!(
            overlay_display_in(&env("relative/config"), "core-leds"),
            "~/.config/trollshell/core-leds.toml"
        );
    }
}
