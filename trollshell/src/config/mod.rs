//! The shell's own **config-file** subsystems: `~/.config/trollshell/*.toml`
//! read through `hytte_config`'s layering (#866 / #868).
//!
//! # What lives here, and what does not
//!
//! `hytte-config` owns the *mechanism* — the XDG search path, the four merge
//! rules, the [`Subsystem`](hytte_config::subsystem::Subsystem) trait, the
//! per-key tolerance ([`parsed`](hytte_config::subsystem::Subsystem::parsed) /
//! [`keep`](hytte_config::subsystem::keep) /
//! [`InvalidValue`](hytte_config::subsystem::InvalidValue)), the environment
//! overlay and its deprecation lines
//! ([`subsystem::env`](hytte_config::subsystem::env)), the live-reload poller
//! ([`subsystem::watch`](hytte_config::subsystem::watch)) and the
//! format-preserving writer. This module owns the shell's *schemas*: one module
//! per subsystem, each declaring a type, a file name and a documented
//! `DEFAULT_TOML`.
//!
//! It did not start that way. #869's pilot grew all of the above **in the
//! shell** — deliberately, because a shape that has only been designed is not
//! worth generalising — and #1044 hoisted it down into `hytte-config` once
//! the pilot had four review passes' worth of evidence behind it. What is left
//! here is the part that is genuinely per-subsystem, which is the whole point:
//! family #2 is a declaration plus a wiring line.
//!
//! They are here rather than in a shared leaf crate because the *schemas* are
//! the shell's. #888 (schema-derived forms in the control-center's Plugins tab)
//! is the thread that would move a schema down into a crate both the shell and
//! the companion app can link.
//!
//! # Two conventions the pilot writes down for the nine subsystems after it
//!
//! **`_unset = ["key"]` is reserved.** TOML has no null, so #868 spells the
//! "unset a key an underlying layer set" half of the scalar merge rule as an
//! array of key names under the literal key `_unset`
//! ([`hytte_config::merge::UNSET_KEY`]). It is an invention, and it means a key
//! genuinely named `_unset` cannot be a schema key in any subsystem.
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
//! `core_leds.rs` is the worked example for every step below; read it alongside
//! this list rather than instead of it (#1040 fix round 4 F4). Steps 5–8 got
//! considerably shorter with #1044's hoist — what used to be ~510 lines of
//! scaffolding per family is now four `env::key` calls and two constructor
//! lines.
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
//!    base file), `type Resolved` (the parsed form the rest of the shell
//!    consumes), and `type Error = Infallible` unless the keys genuinely
//!    constrain each other — `Subsystem::validate`'s doc shows how a cross-key
//!    rule composes without a second parser.
//! 4. One `const EnvKnob` per migrated variable:
//!    [`EnvKnob::same`](hytte_config::subsystem::env::EnvKnob::same)`(var, key,
//!    accepts)`, or the four-field form where the file spelling and the
//!    variable spelling differ.
//! 5. `fn parsed(&self) -> (Resolved, Vec<InvalidValue>)` — the **single**
//!    judge: [`spelling`](hytte_config::subsystem::spelling) per key,
//!    [`keep`](hytte_config::subsystem::keep) per key, and no `?`, so a bad key
//!    costs its own key and nothing else.
//! 6. `fn resolve(layered, lookup, announce)` — one
//!    [`env::key`](hytte_config::subsystem::env::key) call per knob, so a set
//!    variable wins and announces once and an unusable one costs exactly one
//!    line. `lookup` is injected: `std::env::set_var` is `unsafe` and this
//!    workspace forbids `unsafe_code`, so nothing else could drive it in a test
//!    anyway. Omit it entirely for a subsystem with no migrated variables — the
//!    trait's default is "there is no environment to layer".
//! 7. Nothing. Startup is
//!    [`watch::boot`](hytte_config::subsystem::watch::boot)`::<Config>(paths,
//!    lookup)`: it stamps the layers, loads them **once**, and resolves the
//!    environment announcing every set variable — the stamp-before-load order
//!    lives in the constructor so no call site can get it wrong (#1040 V2).
//! 8. `impl Service` — `start` calls `boot`, then `spawn_supervised` over
//!    [`watch::poll_loop`](hytte_config::subsystem::watch::poll_loop). Hold
//!    `paths`/`lookup`/`interval` as fields on the `Service` struct itself, so
//!    `start` is what a test drives rather than a replica of it (#1040 V3).
//! 9. `.with(config::<name>::service())` in `main.rs`, plus a `signal()`
//!    accessor that `.expect()`s the registration.
//! 10. A `docs/live-verify.md` block: none of the reload behaviour is
//!     verifiable on CI.
//!
//! # The deprecation window
//!
//! #866 settled three steps, and the pilot is step 2 for its four variables:
//! the environment variable is still read and still **wins**, but a set one
//! warns **once at startup** naming the file and the key it moves to. Step 3 —
//! removing the variable — is a later decision, not this one. The three
//! sentences that window is made of, and the once-per-startup gate they go
//! through, live in [`hytte_config::subsystem::env`].

pub mod core_leds;
pub mod workspaces;
