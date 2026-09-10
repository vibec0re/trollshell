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
//! # How the next subsystem is declared (the pilot's shape, in ten steps)
//!
//! `core_leds.rs` is the worked example for every step below; read it
//! alongside this list rather than instead of it (#1040 fix round 4 F4).
//!
//! 1. `pub mod <name>;` here; one file `config/<name>.rs`.
//! 2. `struct <Name>Config` — **every field a raw `toml::Value`**,
//!    `#[serde(default)]` on the *container*, and a hand-written `Default`
//!    pinned equal to `DEFAULT_TOML`. Anything narrower — a `String`
//!    included — hands the verdict to serde, and serde's verdict is
//!    whole-file (#1040 T1).
//! 3. `impl Subsystem`: `NAME` (kebab-case — the *config file's* stem,
//!    `core-leds.toml`, not the `.rs` module's), a **commented**
//!    `DEFAULT_TOML` (the only place a key is documented until nix renders a
//!    base file), and `type Error = Infallible` unless the keys genuinely
//!    constrain each other — `CoreLedsConfig::validate`'s doc shows how a
//!    cross-key rule composes without a second parser.
//! 4. One `const Knob` per migrated variable: `Knob::same(var, key, accepts)`,
//!    or the four-field form where the file spelling and the variable
//!    spelling differ.
//! 5. `fn parsed(&self) -> (Resolved, Vec<InvalidValue>)` — the **single**
//!    judge (`Resolved` is the subsystem's own resolved-value type;
//!    `core_leds`'s is `CoreLeds`): `spelling()` per key, `keep()` per key,
//!    and no `?`, so a bad key costs its own key and nothing else.
//! 6. `fn resolve(layered, lookup, announce)` — one `env_key` call per knob,
//!    so a set variable wins and announces once and an unusable one costs
//!    exactly one line. `lookup` is injected: `std::env::set_var` is
//!    `unsafe` and this workspace forbids `unsafe_code`, so nothing else
//!    could drive it in a test anyway.
//! 7. `fn boot(paths, lookup)` — `Watcher::stamping_before(paths,
//!    initial_load)`, then one `Deprecations::Announce` resolution. The
//!    process's only load; the stamp-before-load order lives in the
//!    constructor so no call site can get it wrong.
//! 8. `impl Service` — `start` calls `boot`, then `spawn_supervised` over
//!    `watch`. Hold `paths`/`lookup`/`interval` as fields on the `Service`
//!    struct itself, so `start` is what a test drives rather than a replica
//!    of it.
//! 9. `.with(config::<name>::service())` in `main.rs`, plus a `signal()`
//!    accessor that `.expect()`s the registration.
//! 10. A `docs/live-verify.md` block: none of the reload behaviour is
//!     verifiable on CI.
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
/// `accepts` is the knob's **file** vocabulary — this line's whole job is to
/// tell the reader what to type in the file it names, so where the two
/// vocabularies differ it is the file's that belongs here (#1040 V4). It is
/// part of the sentence rather than a field for the same reason (#1040 F5):
/// the only place `core-leds.toml` explains that the automatic shape is spelt
/// `0` *or* `"rect"` is `DEFAULT_TOML`, which — until nix renders a base file —
/// exists nowhere on disk for the reader to open. The line that tells a user to
/// move a value into a file has to tell them what the file accepts, or they
/// will guess.
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
/// Emitted **once at startup**, exactly like [`deprecation_message`] and
/// through the same [`core_leds::Deprecations`] gate (#1040 V7). It used to be
/// emitted on every reload as well, on the argument that "an unusable variable
/// is still unusable after an edit" — true, but so is the environment: a
/// process's variables are fixed at `exec` and this shell never calls
/// `set_var` (it is an `unsafe fn` and this crate forbids unsafe), so there is
/// no outage that can *end* and nothing a repeat could tell the reader. It
/// cost one extra journal line per save for the life of the shell, against a
/// `live-verify.md` bullet that promises **exactly one**.
///
/// `accepts` here is the knob's **variable** vocabulary, not its file one:
/// they differ wherever a key has a TOML spelling the variable never took
/// (`rows = 0`, #1040 V4), and this sentence is about what the *variable*
/// would have had to say.
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

/// The **one** line a rejected value for a *known file key* produces.
///
/// The third sentence in the family, and the one #1040 V1 needed: a bad value
/// in the file no longer takes the whole file down, so the reader has to be
/// told which key was dropped **and** what happened to it. `invalid` is the
/// subsystem's own rendering of "what you wrote, and what was expected"
/// ([`core_leds::InvalidValue`]); this adds the consequence.
///
/// "the built-in default" rather than "the layer below" is precise, not vague:
/// the layers are merged *before* anything is parsed, so by the time a value
/// is judged there is no provenance left to fall back through. The key falls
/// all the way to [`hytte_config::subsystem::Subsystem::DEFAULT_TOML`]'s
/// value, which is what the sentence says.
pub fn rejected_value_message(invalid: &str) -> String {
    format!("{invalid} — ignoring this key and using the built-in default")
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

/// Warn that one **file key** held a value nothing accepts, and that the
/// built-in default is being used for it.
///
/// One line per rejected key, emitted where the file is loaded — so a file
/// with two typos says two things and a file with none says nothing. The
/// *rest* of the file still applies, which is the whole reason this line
/// exists (#1040 V1): before it, a single bad value was a whole-file
/// `ConfigError::Invalid` and the reader got one message naming one key while
/// every *other* key silently reverted too.
pub fn warn_rejected_value(subsystem: &str, key: &str, invalid: &str) {
    tracing::warn!(subsystem, key, "{}", rejected_value_message(invalid));
}

#[cfg(test)]
mod tests {
    use super::{
        deprecation_message, overlay_display_in, rejected_value_message, unusable_env_message,
    };
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

    /// The rejected-file-value sentence, as a literal.
    ///
    /// The consequence clause is the load-bearing half: #1040 V1 was a PR that
    /// *said* a bad value left the rest of the file applied while the code
    /// dropped the whole file, and a human reading the journal has no way to
    /// tell the two apart except by what this line claims. Building the
    /// expectation from [`rejected_value_message`] would assert it against
    /// itself; `docs/live-verify.md` quotes it verbatim.
    #[test]
    fn the_rejected_value_sentence_is_this_exact_sentence() {
        assert_eq!(
            rejected_value_message("rows = \"many\" is not valid; expected a row count"),
            "rows = \"many\" is not valid; expected a row count \
             — ignoring this key and using the built-in default"
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
