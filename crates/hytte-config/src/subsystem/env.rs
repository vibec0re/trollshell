//! The environment layered **over** a subsystem's config file, the one
//! deprecation line a migrated variable produces while it is still read
//! (#866 step 2, hoisted out of the `core-leds.toml` pilot by #1044), and the
//! one line a variable produces once it no longer is (step 3, #1041).
//!
//! #866 settled three steps for each of the 43 environment variables
//! trollshell's configuration used to live in. Step 2 is [`key`]: the
//! variable is still read and still **wins**, but a set one warns **once at
//! startup** naming the file and the key it moves to. Step 3 is [`removed`]:
//! a subsystem's own decision, made once its deprecation window has closed —
//! the variable is no longer read for its *value* at all, only checked for
//! *presence*, so a set one still warns once but the file (or the built-in
//! default) always wins. `core-leds` was the first to reach it (#1041 item
//! 3); every other subsystem still calls [`key`] until its own window closes.
//!
//! # Resolution order, per key
//!
//! 1. the **environment variable**, when it is set *and* parses — with one
//!    deprecation warning at startup ([`Deprecations`]);
//! 2. the **config layers**, merged by [`crate::merge`]'s four rules;
//! 3. [`crate::subsystem::Subsystem::DEFAULT_TOML`], the bottom merge layer —
//!    so a missing file behaves exactly like an unset variable did, and says
//!    nothing about it.
//!
//! A **set but unparseable** variable warns once and falls through to the layer
//! below. It does not win, and it does not take anything down. Exactly one
//! line, not two (#1040 F9): the deprecation announcement is made only for a
//! value that actually parsed, and the unusable-value line names the key and
//! the file itself.
//!
//! # Why there is no `env_override(&[(var, key, parser)])`
//!
//! #1044 proposed a slice of homogeneous triples and the second review pass
//! measured that it cannot exist: the pilot's four parsers return
//! `DisplayStyle`, `ColorMap`, `Option<usize>` and `Fill`, and a homogeneous
//! `&[…]` cannot express four different `T`s. The two escapes on the table
//! were "a trait per key" and "a closure returning `Result<Field,
//! InvalidValue>` over an enum" — an enum every subsystem would then have to
//! declare, unpack four times, and keep in sync with its own resolved struct.
//!
//! So the generic piece is the **per-key** function [`key`], generic in `T`,
//! and a subsystem's `resolve` is one call per knob. That is four lines of
//! fan-out per family rather than an enum plus four `match` arms, and it is
//! the shape the pilot measured. It is also why [`key`]'s parser returns
//! `Result<T, &str>` rather than `Result<T, super::InvalidValue>`: the `Err`
//! carries the raw spelling, and the environment path deliberately never
//! produces an [`InvalidValue`](super::InvalidValue) — that type renders a
//! value **as TOML** with the *file* vocabulary, and a shell variable is
//! neither (#1040 V4 split the two vocabularies precisely because sharing one
//! string made the variable's own line self-contradictory).

use crate::xdg;

/// One migrated knob: the environment variable that used to carry it, the
/// config key that carries it now, and the vocabulary each accepts.
///
/// A table rather than ad-hoc string literals because every message about a
/// knob — the deprecation line, the unrecognised-value warning, the per-key
/// rejection — has to name the same three things.
pub struct EnvKnob {
    /// The deprecated environment variable.
    pub var: &'static str,
    /// The config key it moved to.
    pub key: &'static str,
    /// What the **variable** accepts, phrased to read after "expected" in
    /// [`unusable_env_message`].
    pub env_accepts: &'static str,
    /// What the **file key** accepts, phrased to read after "it accepts" in
    /// [`deprecation_message`] and after "expected" in
    /// [`InvalidValue`](super::InvalidValue)'s `Display`.
    ///
    /// Separate from [`Self::env_accepts`] because #1040 V4 measured that they
    /// genuinely differ wherever a key has a TOML spelling the variable never
    /// took (the pilot's `rows = 0` for the automatic rectangle): one shared
    /// string made the *variable*'s own line read
    /// "`0` … is not valid; expected "rect" (or 0)". Where the two agree,
    /// [`EnvKnob::same`] states the vocabulary once.
    pub file_accepts: &'static str,
}

impl EnvKnob {
    /// A knob whose file spelling and variable spelling are identical — the
    /// normal case, and the one a new subsystem should expect to be in.
    #[must_use]
    pub const fn same(var: &'static str, key: &'static str, accepts: &'static str) -> Self {
        Self {
            var,
            key,
            env_accepts: accepts,
            file_accepts: accepts,
        }
    }
}

/// Whether this resolution is the startup one (which announces every
/// deprecated variable that is set) or a reload (which announces nothing).
///
/// An explicit parameter rather than a `Once` latch: the once-ness is then a
/// property of the two call sites — a subsystem's `boot` announces, its
/// `watch` does not — which a test can drive directly, instead of
/// process-global state the second test in a binary can no longer observe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Deprecations {
    /// Warn once per set variable, naming the file key it moves to.
    Announce,
    /// Say nothing: this environment was already announced at startup.
    Silent,
}

/// The one message a deprecated-but-still-honoured environment variable
/// produces.
///
/// A function rather than a literal at the call site for the reason
/// `subsystem`'s `MALFORMED_UNSET_MESSAGE` is a `const`: a test selects on the
/// **exact** rendered line, so a `contains("deprecated")` filter cannot turn
/// green-and-blind the day somebody rewords it — including a negative test,
/// whose whole job is to observe that an *unset* variable warns about nothing.
/// The *content* is pinned separately, against a literal string, because
/// building the expectation by calling this function would assert it against
/// itself (#1040 F4, mutation X3).
///
/// The sentence form (rather than a static message plus fields) is deliberate:
/// the line has to read as an instruction in `journalctl -f` without the reader
/// assembling it out of structured fields. The fields are emitted as well, for
/// anything that wants to filter on them.
///
/// `accepts` is the knob's **file** vocabulary — this line's whole job is to
/// tell the reader what to type in the file it names, so where the two
/// vocabularies differ it is the file's that belongs here (#1040 V4/T3). It is
/// part of the sentence rather than a field for the same reason (#1040 F5): the
/// only place a subsystem explains its spellings is `DEFAULT_TOML`, which —
/// until nix renders a base file — exists nowhere on disk for the reader to
/// open. A line that tells a user to move a value into a file has to tell them
/// what the file accepts, or they will guess.
#[must_use]
pub fn deprecation_message(var: &str, key: &str, file: &str, accepts: &str) -> String {
    format!("{var} is deprecated; set `{key}` in {file} — it accepts {accepts}")
}

/// The **one** line a set-but-unusable deprecated variable produces.
///
/// One line rather than two (#1040 F9): before this, an unrecognised value
/// produced both the deprecation sentence and a separate unrecognised-value
/// warning, and the reader got two half instructions to assemble. This says all
/// of it once — what was set, what was expected, what happens instead, and
/// where the value belongs now.
///
/// Emitted **once at startup**, exactly like [`deprecation_message`] and
/// through the same [`Deprecations`] gate (#1040 V7). It used to be emitted on
/// every reload as well, on the argument that "an unusable variable is still
/// unusable after an edit" — true, but so is the environment: a process's
/// variables are fixed at `exec` and nothing in this workspace calls `set_var`
/// (it is an `unsafe fn` and the workspace forbids unsafe), so there is no
/// outage that can *end* and nothing a repeat could tell the reader. It cost
/// one extra journal line per poll for the life of the shell.
///
/// `accepts` here is the knob's **variable** vocabulary, not its file one: they
/// differ wherever a key has a TOML spelling the variable never took (#1040
/// V4), and this sentence is about what the *variable* would have had to say.
#[must_use]
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
/// than a hard-coded `~/.config/…`, because the whole point of the line is that
/// the reader can open the file it names. The literal is the fallback for the
/// one case where no path resolves at all — neither `$XDG_CONFIG_HOME` nor a
/// usable `$HOME` — where naming the conventional location is still more useful
/// than saying nothing.
///
/// The `Env` is a parameter for the reason every rule in [`xdg`] takes one: the
/// process environment is not a thing a test may set (`std::env::set_var` is an
/// `unsafe fn` and this workspace forbids `unsafe`), so a resolution asserted
/// against `Env::from_process()` can only be asserted against itself. With an
/// `Env` in hand the result is pinnable against a literal path (#1040 F4,
/// mutation X2).
#[must_use]
pub fn overlay_display_in(env: &xdg::Env, subsystem: &str) -> String {
    env.overlay_path(subsystem).map_or_else(
        || format!("~/.config/{}/{subsystem}.toml", xdg::APP_DIR),
        |path| path.display().to_string(),
    )
}

/// [`overlay_display_in`] against the process environment — what every
/// production log line resolves.
#[must_use]
pub fn overlay_display(subsystem: &str) -> String {
    overlay_display_in(&xdg::Env::from_process(), subsystem)
}

/// Warn that `var` has moved into `subsystem`'s config file under `key`.
///
/// Call this **once per set variable at startup** — not per read, and not on a
/// config reload. A live reload re-resolves the same environment (the variable
/// still wins), so warning there would repeat the line every few seconds for as
/// long as the shell runs; see [`Deprecations`].
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

/// Resolve one knob: the environment variable if it is set, otherwise
/// `fallback` (the merged file value).
///
/// A set variable **wins**, and warns that it is deprecated. A set variable
/// that does not parse warns about the value and falls through to `fallback` —
/// the pre-migration behaviour, except that "the layer below" is now the config
/// file rather than a hard-coded default.
///
/// **One warning, not two** (#1040 F9). The announcement is made *after* the
/// parse and only for a value that actually parsed: a set-but-unusable variable
/// gets [`warn_unusable_env`] instead, a single line that already names the key
/// and the file the value should move to. Announcing first would have handed
/// the reader two half instructions for one mistake.
///
/// Both lines are gated on `announce` (#1040 V7) — see
/// [`unusable_env_message`] for why the unusable one is too.
pub fn key<'a, T>(
    subsystem: &str,
    knob: &EnvKnob,
    raw: Option<&'a str>,
    parse: impl FnOnce(&'a str) -> Result<T, &'a str>,
    fallback: T,
    announce: Deprecations,
) -> T {
    let Some(raw) = raw else { return fallback };
    match parse(raw) {
        Ok(value) => {
            if announce == Deprecations::Announce {
                // The *file* vocabulary: this line's job is to tell the reader
                // what to type in the file it names (#1040 T3).
                warn_deprecated_env(subsystem, knob.var, knob.key, knob.file_accepts);
            }
            value
        }
        Err(bad) => {
            if announce == Deprecations::Announce {
                // The *variable* vocabulary: this line is about what the
                // variable would have had to say, and a key's file spelling can
                // include a word the variable never took (#1040 V4).
                warn_unusable_env(subsystem, knob.var, bad, knob.key, knob.env_accepts);
            }
            fallback
        }
    }
}

/// The **one** line a variable produces once its subsystem's deprecation
/// window has closed (#1041 step 3): it is no longer read for its value at
/// all, so there is nothing left to say about what it was set *to* — only
/// that it does nothing now, and where the value belongs instead.
///
/// Deliberately not [`deprecation_message`] reworded in place: a subsystem
/// that has reached step 3 and one still mid-window (calling [`key`]) can
/// coexist in the same journal — `core-leds` reached it in #1041 while every
/// other subsystem was still on step 2 — and a reader filtering on "is
/// deprecated" vs. "does nothing any more" needs the two to read as genuinely
/// different instructions, not one message two subsystems happen to share.
///
/// `accepts` is the knob's **file** vocabulary, for the same reason
/// [`deprecation_message`]'s is: this line's job is to tell the reader what
/// to type in the file it names.
#[must_use]
pub fn removed_message(var: &str, key: &str, file: &str, accepts: &str) -> String {
    format!("{var} does nothing any more; set `{key}` in {file} instead — it accepts {accepts}")
}

/// Warn that `var` does nothing any more, and name the config key that
/// replaces it.
///
/// Call this **once per set variable at startup**, exactly like
/// [`warn_deprecated_env`] — see that function's doc for why a reload must
/// never call it.
pub fn warn_removed_env(subsystem: &str, var: &str, key: &str, accepts: &str) {
    let file = overlay_display(subsystem);
    tracing::warn!(
        subsystem,
        var,
        key,
        accepts,
        file = %file,
        "{}",
        removed_message(var, key, &file, accepts)
    );
}

/// Resolve one knob once its subsystem's deprecation window has closed
/// (#1041 step 3): `raw` is checked only for **presence**, never parsed, and
/// `fallback` (the merged file value) always wins.
///
/// The counterpart to [`key`] with no `parse` argument and no `Result`: once
/// a variable does nothing, there is nothing left to attempt parsing, and no
/// "unusable value" case to distinguish from a usable one — every set value
/// gets the same one line, from [`removed_message`].
pub fn removed<T>(
    subsystem: &str,
    knob: &EnvKnob,
    raw: Option<&str>,
    fallback: T,
    announce: Deprecations,
) -> T {
    if raw.is_some() && announce == Deprecations::Announce {
        warn_removed_env(subsystem, knob.var, knob.key, knob.file_accepts);
    }
    fallback
}

/// The process environment, for a subsystem's production call site.
///
/// Every resolver takes its lookup as a parameter rather than reaching for the
/// process, because `unsafe_code = "forbid"` rules out `std::env::set_var` (it
/// is an `unsafe fn` in edition 2024): a test that drove the real environment
/// could not exist at all, and one that read it would depend on the developer's
/// shell. This is the one production value that gets passed in.
#[must_use]
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

#[cfg(test)]
mod tests {
    use super::{
        Deprecations, EnvKnob, deprecation_message, key, overlay_display_in, removed,
        removed_message, unusable_env_message,
    };
    use crate::test_support::capture;
    use crate::xdg;

    const STYLE: EnvKnob = EnvKnob::same(
        "TROLLSHELL_CORE_LEDS_STYLE",
        "style",
        "one of vfd/lcd/oled/crt",
    );

    /// A knob whose two vocabularies genuinely differ — [`EnvKnob::same`]'s
    /// three STYLE-shaped tests above cannot tell "picked `file_accepts`" from
    /// "picked the only string it has".
    const ROWS: EnvKnob = EnvKnob {
        var: "TROLLSHELL_CORE_LEDS_ROWS",
        key: "rows",
        env_accepts: "rect, or a row count",
        file_accepts: "0 or \"rect\", or a row count",
    };

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
    /// calling [`deprecation_message`] would assert the sentence against itself
    /// and stay green through any reword (#1040 F4, mutation X3).
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

    /// The path in the line is the **resolved overlay path**, asserted against a
    /// literal built from a controlled `$XDG_CONFIG_HOME`.
    ///
    /// **Red if `overlay_display_in` stops resolving** — returns a constant, or
    /// the `~/.config/…` fallback, or drops the `trollshell/` component (#1040
    /// F4, mutation X2). The point of naming the real path is that the reader
    /// can open the file; a line naming a path that does not exist is worse
    /// than one naming none.
    #[test]
    fn the_line_names_the_resolved_overlay_path() {
        assert_eq!(
            overlay_display_in(&env("/x/config"), "core-leds"),
            "/x/config/trollshell/core-leds.toml"
        );
    }

    /// With nowhere to resolve — no `$XDG_CONFIG_HOME`, no usable `$HOME` — the
    /// line names the conventional location rather than saying nothing.
    #[test]
    fn with_no_config_home_the_line_names_the_conventional_path() {
        assert_eq!(
            overlay_display_in(&xdg::Env::default(), "core-leds"),
            "~/.config/trollshell/core-leds.toml"
        );
    }

    /// A relative `$XDG_CONFIG_HOME` is invalid per the XDG spec and this crate
    /// ignores it (#985) — so the line must fall back rather than name a path
    /// relative to the shell's working directory. The same gate `config_layers`
    /// goes through, on the path the line *names*.
    #[test]
    fn a_relative_config_home_falls_back_like_an_unset_one() {
        assert_eq!(
            overlay_display_in(&env("relative/config"), "core-leds"),
            "~/.config/trollshell/core-leds.toml"
        );
    }

    /// An unset variable resolves to the file value and says **nothing** — the
    /// negative that makes "one line per set variable" meaningful.
    #[test]
    fn an_unset_variable_takes_the_fallback_silently() {
        let (captured, _guard) = capture();

        let resolved = key(
            "core-leds",
            &STYLE,
            None,
            |raw: &str| Ok::<&str, &str>(raw),
            "from-the-file",
            Deprecations::Announce,
        );

        assert_eq!(resolved, "from-the-file");
        assert_eq!(captured.warnings(), Vec::<String>::new());
    }

    /// A set variable that parses **wins** and announces once, in the **file**
    /// vocabulary (#1040 T3, mutation R12 — swapping in `env_accepts` was green
    /// until a test looked at which string came out).
    #[test]
    fn a_set_variable_wins_and_announces_the_file_vocabulary() {
        const ROWS: EnvKnob = EnvKnob {
            var: "TROLLSHELL_CORE_LEDS_ROWS",
            key: "rows",
            env_accepts: "rect, or a row count",
            file_accepts: "0 or \"rect\", or a row count",
        };
        let (captured, _guard) = capture();

        let resolved = key(
            "core-leds",
            &ROWS,
            Some("4"),
            |raw: &str| Ok::<&str, &str>(raw),
            "from-the-file",
            Deprecations::Announce,
        );

        assert_eq!(resolved, "4", "a set, parsing variable wins");
        let warned = captured.warnings();
        assert_eq!(warned.len(), 1, "exactly one line: {warned:#?}");
        assert!(
            warned[0].contains(ROWS.file_accepts),
            "the deprecation line teaches the *file* spelling: {warned:#?}"
        );
        assert!(
            !warned[0].contains(&format!("expected {}", ROWS.env_accepts)),
            "…and is not the unusable-value line: {warned:#?}"
        );
    }

    /// A set-but-unusable variable falls through, and costs **one** line, not a
    /// deprecation line plus an unrecognised-value line (#1040 F9, mutation
    /// Z4/N5).
    #[test]
    fn an_unusable_variable_falls_through_with_exactly_one_line() {
        let (captured, _guard) = capture();

        let resolved = key(
            "core-leds",
            &STYLE,
            Some("plasma"),
            |raw: &str| Err::<&str, &str>(raw),
            "from-the-file",
            Deprecations::Announce,
        );

        assert_eq!(resolved, "from-the-file", "it does not win");
        let warned = captured.warnings();
        assert_eq!(warned.len(), 1, "one line, not two: {warned:#?}");
        assert!(
            warned[0].contains("is set to `plasma`, which is not valid"),
            "…and it is the unusable-value line: {warned:#?}"
        );
    }

    /// `Silent` says nothing at all, for either outcome — the reload gate
    /// (#1040 M2/V7, mutation R13: ungating the unusable line put a warning back
    /// on every poll for the life of the shell).
    #[test]
    fn a_silent_resolution_announces_nothing_for_either_outcome() {
        let (captured, _guard) = capture();

        let good = key(
            "core-leds",
            &STYLE,
            Some("crt"),
            |raw: &str| Ok::<&str, &str>(raw),
            "from-the-file",
            Deprecations::Silent,
        );
        let bad = key(
            "core-leds",
            &STYLE,
            Some("plasma"),
            |raw: &str| Err::<&str, &str>(raw),
            "from-the-file",
            Deprecations::Silent,
        );

        assert_eq!(good, "crt", "the variable still wins");
        assert_eq!(bad, "from-the-file", "…and an unusable one still does not");
        assert_eq!(captured.warnings(), Vec::<String>::new());
    }

    // ── `removed`: step 3, the deprecation window closed (#1041) ────────────

    /// The removed-variable sentence, as a literal, for the same reason the
    /// deprecation and unusable-value sentences above are: `docs/live-verify.md`
    /// quotes it and a subsystem's own reword must travel there in the same
    /// commit.
    #[test]
    fn the_removed_sentence_is_this_exact_sentence() {
        assert_eq!(
            removed_message(
                "TROLLSHELL_CORE_LEDS_STYLE",
                "style",
                "/x/core-leds.toml",
                "one of vfd/lcd/oled/crt"
            ),
            "TROLLSHELL_CORE_LEDS_STYLE does nothing any more; set `style` in \
             /x/core-leds.toml instead — it accepts one of vfd/lcd/oled/crt"
        );
    }

    /// An unset variable is silent — the negative that makes "one line per set
    /// variable" meaningful, exactly as it is for [`key`].
    #[test]
    fn an_unset_variable_is_silent_once_removed() {
        let (captured, _guard) = capture();

        let resolved = removed(
            "core-leds",
            &STYLE,
            None,
            "from-the-file",
            Deprecations::Announce,
        );

        assert_eq!(resolved, "from-the-file");
        assert_eq!(captured.warnings(), Vec::<String>::new());
    }

    /// **A set variable never wins, whatever it says** — the whole point of
    /// step 3. `removed` takes no `parse` argument at all: there is no code
    /// path left that could read `raw`'s content, so this is really pinning
    /// the signature as much as the behaviour. A `key`-shaped regression that
    /// smuggled the value back in through `raw` would fail this the moment the
    /// variable and the fallback disagree.
    #[test]
    fn a_set_variable_never_wins_once_removed() {
        let (captured, _guard) = capture();

        let resolved = removed(
            "core-leds",
            &STYLE,
            Some("crt"),
            "from-the-file",
            Deprecations::Announce,
        );

        assert_eq!(resolved, "from-the-file", "the file always wins now");
        assert_eq!(captured.warnings().len(), 1, "…but it still costs one line");
    }

    /// The one line names the **file** vocabulary, not the variable's — the
    /// same split [`key`]'s deprecation line makes, and for the same reason:
    /// this line's job is to tell the reader what to type in the file it
    /// names. `ROWS` is the knob whose two vocabularies actually differ, so a
    /// swap to `env_accepts` is observable here where it would not be on
    /// [`EnvKnob::same`]'s `STYLE`.
    #[test]
    fn the_removed_line_teaches_the_file_vocabulary() {
        let (captured, _guard) = capture();

        removed(
            "core-leds",
            &ROWS,
            Some("rect"),
            Some(4_usize),
            Deprecations::Announce,
        );

        let warned = captured.warnings();
        assert_eq!(warned.len(), 1, "{warned:#?}");
        assert!(
            warned[0].contains(ROWS.file_accepts),
            "the removed line teaches the *file* spelling: {warned:#?}"
        );
        assert!(
            !warned[0].contains(ROWS.env_accepts) || ROWS.env_accepts == ROWS.file_accepts,
            "…and not the variable's, which differs here: {warned:#?}"
        );
    }

    /// `Silent` — what a reload passes — says nothing at all, exactly as it
    /// does for [`key`]: the variable is fixed for the life of the process, so
    /// a repeat could never carry news.
    #[test]
    fn a_silent_resolution_announces_nothing_once_removed() {
        let (captured, _guard) = capture();

        let resolved = removed(
            "core-leds",
            &STYLE,
            Some("crt"),
            "from-the-file",
            Deprecations::Silent,
        );

        assert_eq!(resolved, "from-the-file");
        assert_eq!(captured.warnings(), Vec::<String>::new());
    }
}
