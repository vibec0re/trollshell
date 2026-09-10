# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust workspace with two layers:

- **`hytte`** — a library-first toolkit for composing GTK4 + libadwaita + `gtk4-layer-shell` Wayland desktop shells. Split across `crates/hytte-*`.
- **`trollshell`** — the personal shell binary built on `hytte`, targeting the **Niri** compositor.

"Composable, not configurable": there is no config DSL. The shell is wired up in plain Rust in `trollshell/src/main.rs`. **Design lives in GitHub discussions, epics and issues** — a feature is specced on its thread (Annika, Discussion #1063, 2026-09-10), and a build issue links the epic or discussion it came from. `docs/superpowers/{specs,plans}/` is the **April 2026 archive** of the original design (`2026-04-24-hytte-trollshell-design.md`) and the early version specs; consult it for the intent of the subsystems that date from then, but do not add to it — a new design goes on a discussion or an epic, never into that folder.

## Build / run / test

**You must work inside the Nix devShell.** `.envrc` is `use flake` (direnv); if direnv isn't active, run `nix develop` first. The devShell sets env that the build and runtime both require:

- `LD_LIBRARY_PATH`/`LIBCLANG_PATH` so the bindgen consumer (pipewire-sys/libspa-sys) can load libclang. Outside the shell, the build panics with _"a libclang shared library is not loaded on this thread."_
- `XDG_DATA_DIRS` + `GSETTINGS_SCHEMA_DIR` so GTK finds Adwaita symbolic icons and GSettings schemas. Outside the shell, most bar icons render as `image-missing`.

```sh
cargo build --release -p trollshell          # build the binary
cargo run -p trollshell                       # run it (needs a live Niri session: connects to $NIRI_SOCKET)
RUST_LOG=hytte_services=debug,trollshell=debug cargo run -p trollshell   # with logs
nix build                                     # build the packaged binary (.#trollshell)
```

`trollshell` is a real Wayland shell — running it meaningfully requires being **inside a Niri session**. Layer-shell surfaces and most services need live system daemons. (Locking is delegated to `swaylock`/logind, not an in-shell lock screen — see "Deployment & session integration" below.)

### Faster inner loop (devShell only)

Link time dominates the tail of every incremental build (heavy native deps). The devShell wires the **mold** linker by default via `RUSTFLAGS = "-C link-arg=-fuse-ld=mold"` (see `nix/devshell.nix`) — nothing to do, `cargo build`/`clippy`/`test` just link faster. This is deliberately **devShell-only**: the packaged crane build (`nix/package.nix`) has no mold in its sandbox, so a repo-level `.cargo/config.toml` linker setting would break `nix build .#trollshell`.

**sccache** (also in the devShell) caches rustc artifacts across worktrees/branches — handy for the review workflow. It's opt-in to keep the default `cargo` path unsurprising:

```sh
export RUSTC_WRAPPER=sccache        # then build as usual; `sccache --show-stats` to inspect
```

For quick feedback while iterating, `cargo clippy -p <crate> --lib` is much faster than the full `cargo clippy --workspace --all-targets` gate.

### Tests

Tests split into two buckets via the `system-tests` cargo feature (defined in
`hytte-bus`, `hytte-reactive`, `hytte-services`, `hytte-ui`). **Internals**
(pure logic) run by default; **real-system** tests (those needing a
`dbus-daemon` or a display server) are gated behind the feature so the default
run stays hermetic.

```sh
cargo test                                       # internals only — hermetic, no system deps
cargo test --workspace --features system-tests   # + real-system (dbus-daemon + display)
cargo test -p hytte-services clock               # tests matching a name
xvfb-run cargo test --features system-tests -p hytte-ui   # display tests headless
```

- The real-system tests carry `#[cfg(feature = "system-tests")]` (whole-file
  for integration tests, on the `mod tests` for the GTK unit tests) rather than
  `#[ignore]`, so the default `cargo test` doesn't even compile them.
- `hytte-bus`'s system tests **spawn a real `dbus-daemon`** (one ephemeral
  broker per test; must be on `PATH`). They don't touch the host session bus.
- The GTK-dependent system tests need a display server (`xvfb-run` works).
- `hytte-services`'s gated test round-trips the NetworkManager secret agent
  (`wifi::nm_agent`'s `GetSecrets`) against a real `dbus-daemon` too.
- The Nix package (`nix/package.nix`) sets `doCheck = true`: every
  `nix build .#trollshell` runs the hermetic internals suite
  (`cargo test --workspace`, deliberately **without** `system-tests`) as part
  of the build.

### Packaging (`nix/package.nix`)

`nix/package.nix` has **one** `craneLib.buildPackage` — `trollshell-workspace`, built `--workspace --locked` — producing every binary the flake ships. It sets `dontWrapGApps = true`, so its `$out/bin` holds raw, unwrapped ELFs. Every package output is a **slice** of that single derivation, not a second crane call: `nix/plugin.nix` is a `runCommand` that `install -Dm755`s one binary out of `${workspace}/bin/<name>` with no wrapping — not just the bundled widget plugins, but any GTK-free binary the workspace produces (the `hytte-infobroker` CLI, #562; the `hytte-claude-bridge` daemon, #666); the `trollshell` slice (in `nix/package.nix` itself) and `nix/control-center.nix` do the same copy plus a `wrapGAppsHook4` wrap over `workspace.passthru.devInputs.buildInputs`, so the GApps env matches what an in-place compile would have produced.

**Adding a new binary means adding a slice, not a `buildPackage` call.** Before #587 the package path ran 15 crane compile derivations — 13 of which existed purely to copy one binary out — because each `buildPackage` inherited `workspace`'s packed `target` dir as `cargoArtifacts` and hoped cargo would find everything fresh; measured, it didn't, and every one of them recompiled the workspace. #587 collapsed that to one compile plus plain `cp`s specifically so nobody adds a 16th crane call.

`doCheck = true` lives on the `workspace` derivation itself: `buildPackage` captures binaries out of cargo's JSON build log in a `postBuild` hook, which runs _before_ the check phase, so `cargo test --workspace` (hermetic, no `system-tests`) runs on every build that forces `workspace` — a cold plugin build runs the same suite `nix build .#trollshell` does, not a separate one. The deps stage (`craneLib.buildDepsOnly`) shares that same `--workspace --locked` scope; it was wrongly `-p trollshell` before #587, fingerprinting a different feature union than the `--workspace` compile and so caching a dependency graph the compile stage couldn't actually reuse.

The two nixosTest probe binaries (`nix/probe.nix`, `nix/wifi-probe.nix`, #589) are slices too, but — unlike the plugin/shell slices — they `wrapGAppsHook4`-wrap: the EDS VM test needs `GIO_EXTRA_MODULES` for dconf's GSettings backend, which only a GApps wrap injects. Model a new probe-shaped derivation on these two, not on `nix/plugin.nix`.

### CI (`nix flake check`)

Beyond the package build's `doCheck`, the flake's `checks` output
(`flake.nix`) gates a fair bit more than tests:

- `cargo clippy --workspace --all-targets --features system-tests -- -D warnings`
  (the `system-tests` feature is enabled here so the gated integration tests
  and GTK `mod tests` blocks are lint-checked too, not just compiled once and
  forgotten).
- `treefmt` formatting (`nix/treefmt.nix`) — this is what `nix fmt` runs
  locally.
- The full `system-tests` cargo-feature bucket, run for real
  (`cargo test --workspace --features system-tests` under `xvfb-run`, with a
  `dbus-daemon` on `PATH`) — this is the _only_ place those tests run, since
  the package build's `doCheck` deliberately skips them.
- `nixosModules.default` / `homeModules.default` module-eval checks (force the
  systemd units, session vars, and assertion predicates the modules generate).
- Two `nixosTest` VMs: `eds-nixos-test` (evolution-data-server / `hytte-ecal`
  end-to-end) and `wifi-nm-nixos-test` (the NetworkManager Wi-Fi backend
  against simulated `mac80211_hwsim` radios).
- Since #449: actually **building** `packages.{trollshell,trollshell-control-center}`
  as part of `checks` — before that, `nix flake check` could stay green while
  `nix build .#trollshell` (or the control-center) was broken, because `check`
  only builds what's listed under `checks`, not `packages`.
- `bind-pins` (#831): a source scan (`nix/lint-bind-pins.py`, run by a
  `runCommand` — no compile, so it goes red in seconds) that fails the build if
  a `bind*` call site discards the closure's own widget parameter and uses a
  captured strong clone of the same widget instead, which pins the widget for
  the binding's lifetime and defeats the `WeakRef` contract in
  `crates/hytte-reactive/src/bind.rs` (#224). Not a clippy lint because the
  pattern is cross-statement and repo-specific; not a unit test because ten of
  the twelve sites #831 found can't be constructed without a registered
  `Registry`. Run it by hand from the repo root with
  `nix shell nixpkgs#python3 --command python3 nix/lint-bind-pins.py` — the
  `nix shell` is not optional: **`python3` is deliberately not on the devShell
  PATH**, so a bare `python3 …` (or `nix develop --command python3 …`) is
  `command not found`, which a script run for its side effects can swallow. The
  script's header documents the deliberate carve-out (capturing a _different_
  widget is correct) and why it paren/brace-matches instead of using a regex —
  read it before changing it.
- `glsl` (#893 stage B): the same shape for the preem GL renderer's shaders —
  `nix/lint-glsl.py` assembles each `trollshell/src/plugins/preem_gl/*.{vert,frag}`
  the way `Program::compile` does (the `GLSL_HEADER` const is read out of
  `crates/hytte-ui/src/gl_surface.rs`, the blur's `BLUR_DIR` splice out of
  `program.rs`'s `concat!`) and runs `glslangValidator` over it. Nothing else in
  the tree looks inside those files — they are `include_str!`'d `&'static str`s
  until a driver compiles them, and no check here has one — so without this a
  typo'd identifier ships green and surfaces as a blank chip on glass. The
  design spec named **naga** for this row; measured, naga 26's GLSL frontend
  rejects the whole ES profile (`#version 300/310/320 es` all
  `InvalidVersion` + `InvalidProfile("es")`), so this is `glslang` instead —
  which costs zero `Cargo.lock` entries, since it is invoked as a binary at
  build time rather than linked into the shell. Run it by hand with
  `nix shell nixpkgs#python3 nixpkgs#glslang --command python3 nix/lint-glsl.py`
  — `glslang` is in the devShell, **`python3` is not** (deliberately), so the
  `nix shell` prefix is what makes this run at all. Since #893 it also compiles the **shader
  widget**'s vertex stage and every plugin-supplied fragment _body_ shipped in
  the tree (`crates/*/shaders/*.frag`, globbed rather than listed so a new
  plugin's bodies are covered the day they land), each with the
  `SHADER_PREAMBLE` interface declarations spliced in front — read out of
  `crates/hytte-ui/src/shader_surface.rs`, so a change to the published uniform
  contract changes what CI validates in the same commit. A plugin's _runtime_
  source is deliberately not validated anywhere; see the trust boundary below.
- Since #1036, the `system-tests` check's closure carries `mesa` (llvmpipe) and
  its `preCheck` exports the software-GL env plus `TROLLSHELL_REQUIRE_GL=1`,
  so the three GL-context tests in `hytte-ui` (`gl_surface.rs`) actually run
  under a real `GdkGLContext` there instead of skipping — `TROLLSHELL_REQUIRE_GL=1`
  turns a skip into a failure the same way `flake.nix`'s `preCheck` already
  uses `TROLLSHELL_REQUIRE_ICON_THEME` to do that for the icon-theme test.

### Lint — strict, treat as the gate

The workspace lint config (`Cargo.toml`) is deliberately severe; a violation fails `cargo check`, not just clippy:

- `unsafe_code = "forbid"` workspace-wide. **Only `hytte-ecal` and `hytte-gl`** override this — the two islands (FFI to libecal; OpenGL entry points), each confining its unsafety to safe wrappers and each hand-mirroring the root lints table because workspace-lints inheritance is all-or-nothing. Keep the three tables in sync.
- clippy `all` **and** `pedantic` at `deny`. Code must be pedantic-clean.
- `disallowed_methods`: `zbus::Connection::session`/`::system` are **banned** (see `clippy.toml`). All D-Bus access goes through the `hytte-bus` primitives, never a raw zbus connection.

```sh
cargo clippy --workspace --all-targets        # must be clean
cargo fmt --all
```

Edition 2024, MSRV 1.91 (`rust-version.workspace = true` in every member since #453 — wiring up the inheritance is what surfaced `clippy::incompatible_msrv` violations against the previously-fictional 1.85 and forced the bump; not independently CI-gated beyond that clippy check — the devShell/crane toolchain floats on nixpkgs' current rustc, ~1.95). The nix build and devShell use nixpkgs' rust toolchain (via crane); there is no `rust-toolchain.toml` pin.

Shared dependency versions/feature baselines live in the root `Cargo.toml`'s `[workspace.dependencies]`; members inherit with `dep.workspace = true` rather than hand-repinning (#453). Every dependency comes from crates.io — the workspace has **no** git dependencies. `hive-claude` (`crates/hytte-claude-bridge` only) was the last one: a forge rev pin (#666) that crane resolved at eval time via `builtins.fetchGit`, rev-reproducible but not hash-pinned/substitutable the way a locked flake input is, so a cold `nix flake check` depended on that forge being reachable. #757 took the published 0.1.0 instead, closing #671 — keep it that way, and reach for a flake input over a bare rev pin if a git dependency is ever unavoidable. `deny.toml` (repo root) holds a `cargo-deny` advisories+licenses config, run locally — it isn't wired into `nix flake check` (the advisory-db fetch needs network, which sandboxed nix builds don't have):

```sh
nix shell nixpkgs#cargo-deny --command cargo-deny check
```

## Architecture — the reactive core

The whole design avoids threading `Arc<Mutex<…>>` through widgets by splitting **handles** from **work**:

1. **Handles** — `Mutable<T>` / `MutableVec<T>` from `futures-signals` — live in a **thread-local `Registry`** on the GTK main thread (`hytte-reactive/src/registry.rs`), keyed by `TypeId`.
2. **Work** — D-Bus, sockets, PipeWire — runs on a **process-wide multi-thread tokio runtime** (`hytte-reactive/src/runtime.rs`, `runtime::handle()`).
3. tokio tasks update state by calling `mutable.set(…)` directly (`Mutable` is `Send + Sync`); the registry itself never crosses threads.
4. Widgets subscribe with `bind(signal, &widget, |w, v| …)` (and `bind_text`/`bind_visible`/`bind_class`/`bind_two_way`), which spawns an apply-loop on `glib::MainContext` (GTK main thread). See `hytte-reactive/src/bind.rs`.

The shell author never sees an `Arc`, `Mutex`, or a handle — only free functions returning `impl Signal`.

### The service pattern (every `hytte-services` module follows it)

```rust
pub struct FooService;            // implements hytte_reactive::Service
impl Service for FooService {
    type Handles = FooHandles;    // a struct of Mutable<…> fields
    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles { … }  // spawn tokio tasks, return handles
}
pub fn service() -> FooService { FooService }                 // registered via App::with(…)
pub fn some_state() -> impl Signal<Item = …> {                // subscribe accessor
    registry::with(|r| r.get::<FooHandles>().expect("foo::service() not registered").field.signal_cloned())
}
pub fn do_thing(arg: …) { … }                                 // fire-and-forget command (e.g. niri::focus_workspace)
```

`clock.rs` is the minimal example; `upower.rs` is the canonical D-Bus example (one `hytte_bus::property` subscription per field, each updating its slice of the shared state). Accessors `.expect(...)` if the service wasn't registered — so a widget pulling `foo::state()` requires `App::with(foo::service())` in `main.rs`.

### system-daemon-as-state-store

Services are **thin async clients to persistent system daemons** (systemd-networkd, NetworkManager, BlueZ, PipeWire, UPower, logind, niri-ipc, iwd, evolution-data-server, …). Persistent state lives in the daemon, not in `hytte`, so restarting `trollshell` during dev reconnects **without losing system state**. This is a core design constraint — keep new state in the daemon where possible. The network stack sources from **both** networkd (link/route state, `networkd.rs`) and NetworkManager (`networkd_nm.rs`, `wifi_nm.rs`), with an NM secret agent (`wifi/nm_agent.rs`) answering Wi-Fi/VPN `GetSecrets`.

**Exception:** `notifications` registers itself as the `org.freedesktop.Notifications` daemon (a session singleton), so any other notification daemon (mako/dunst) must be disabled.

### Crate graph

```
hytte-reactive   ← base: Service trait, Registry, tokio runtime, bind() helpers
  ↑   ↑   ↑
hytte-ui  hytte-services  hytte-bus
hytte-ui          → App/AppBuilder (wraps adw::Application), Bar, LayerWindow, Popup, Monitor; layer-shell; default stylesheet
hytte-bus         → shared D-Bus layer: call / property / proxy / signals / own_name builders over pooled session+system connections
hytte-services    → the service modules (clients to daemons)
hytte-config      → GTK-free leaf (serde/serde_ignored/toml/toml_edit/tracing only): the `places.toml` schema + validation + its format-preserving `toml_edit` writer, plus the atomic `~/.config/trollshell/*` write helper (ex `hytte-services::config_file`, aliased back as `pub(crate) use hytte_config::file as config_file` so in-crate call sites still read `config_file::…`). Consumed by BOTH `hytte-services` and `trollshell-control-center` — the first crate the shell's service layer and the companion app share, which is exactly why it exists: `places.toml` has two editors (#640/#703, the file stays hand-editable) and they must agree byte for byte rather than each carrying its own serialisation path. Since #868 it also holds the **config layering** #866 settled: `xdg` (the `XDG_CONFIG_DIRS` base → `XDG_CONFIG_HOME` overlay search path, plus the `XDG_STATE_HOME` path so state never shares a directory with config), `merge` (the four rules — scalars overlay-wins-when-present with `_unset` spelling the null TOML lacks, tables deep-merge, arrays **replace**, and unknown keys warn via `serde_ignored` rather than fail), `subsystem` (declare a type + a name + a documented `DEFAULT_TOML`, inherit the reader/validator/format-preserving writer) and `state`. Since #1044 `subsystem` also carries everything the `core-leds.toml` pilot (#869/#1040) had grown in the shell, so family #2 is a declaration plus a wiring line: the per-key tolerance on the trait itself (`type Resolved` + `fn parsed` returning `(Resolved, Vec<InvalidValue>)`, so a bad value costs its own key and not the whole file), `subsystem::env` (the `EnvKnob` table, the `Deprecations` gate and the three sentences a migrated `TROLLSHELL_*` variable produces — deliberately a per-key `env::key<T>` fan-out, not a table of homogeneous triples, which #1040's second review measured cannot exist), and `subsystem::watch` (the `(mtime, len)` layer poller, stamp-before-load). Two **cargo features**, both off by default and both there to keep the dependency list above true for the control-center: `watch` pulls `tokio` + `futures-signals` (the shell's dependency line enables it, the control-center's does not — so a settings app never grows a runtime it does not drive), and `test-support` exposes `test_support` (the process-wide tracing global default #1022 needs, the capture harness and the scratch-`Overlay`), reached only through **dev**-dependencies from `trollshell` and `hytte-reactive`, so no shipped rlib carries a `set_global_default`. `places` predates all of it and goes through none of it — `crates/hytte-config/tests/places_byte_identical.rs` pins its bytes against a recording taken from `origin/main` before the layering existed, so a change that moves the two editors apart fails there
hytte-ecal        → hand-written FFI to evolution-data-server (libecal); one of TWO crates allowed `unsafe`
hytte-gl          → the **second `unsafe` island** (#893 stage B), on the `hytte-ecal` precedent: `Cargo.toml` hand-mirrors the root lints table with `unsafe_code = "allow"` because workspace-lints inheritance is all-or-nothing. It exists because the workspace `forbid`s unsafe and *every* raw-GL binding marks each entry point `unsafe fn`, so nothing could issue a draw call — which is why stage A's `gl_probe` deliberately measures GTK's integration cost without touching GL. GTK-free and tiny: one safe RAII type per GL object (program with the driver's info log handed back, immutable-storage texture, FBO, VAO), plus blend/viewport/clear and two attribute-less draws. Adds **no** resolved package — `gl 0.14.0` is already in `Cargo.lock` via `gdk4`, `libloading` via `clang-sys` — and since #1067 resolves its entry points through **glvnd's `eglGetProcAddress`** (`dlopen("libEGL.so.1")`, already mapped by GTK's own closure via `libgstgl`), whose `libGLdispatch` stubs route to the vendor of the **thread-current** context on every call, so one process-wide `gl::load_with` serves every `GdkGLContext` (#886). libepoxy is only the **fallback**, and the reason #1067 existed: it exports 3403 `epoxy_gl*` *variables* (`D`, function pointers) and **zero** plain `gl*` functions — the unprefixed spelling is a `#define` in epoxy's header that never reaches a symbol table — so the original loader's plain-name lookup resolved nothing on any NixOS machine and the GL renderer silently fell back to the CPU kit for the whole life of #893 stage B. The epoxy route needs one extra deref (`libloading::Symbol<T>` is the dlsym *address*); plain names from the process image are the last resort. `crates/hytte-gl/src/loader.rs`'s tests pin all three routes hermetically — no display, no driver, no GL context — via a `gdk4-sys` **dev**-dependency taken purely for its link line, which is what maps libepoxy/libEGL into the test binary. Consumed by `hytte-ui`'s `gl_surface` only; `trollshell` reaches GL through that widget and links this crate only as a dev-dependency, for the `preem_gl_diff` parity harness
hytte-ai-providers → shared OpenAI-compatible chat client + provider config + key-file loader, used by plugins that talk to an LLM (e.g. hytte-plugin-pet)
hytte             → umbrella: re-exports {bus, reactive, services, ui} + a `prelude`
trollshell        → the binary; depends on `hytte` — plus, since #857, `hytte-preem` directly: the Stats drawer's per-core LED panel rasterises the kit in-process into a `hytte::ui::PixelSurface`. That is the kit, NOT the plugin SDK; the shell still never links `hytte-plugin`
trollshell-control-center → separate windowed GTK4/libadwaita companion app (#390/#399); talks to the running shell over its own `Control` D-Bus endpoint (trollshell/src/control.rs), never linked into the shell. It cannot link `hytte-services` either (that would drag libpipewire + evolution-data-server into a settings app), but since #640 it does link `hytte-config` — a shared GTK-free *leaf library*, not a runtime link to the shell: its Places tab reads and writes `places.toml` directly, through the same writer `hytte-services` uses, so the editor keeps working while the shell is down and the shell's existing mtime poll picks a save up with no new D-Bus surface
hytte-claude-bridge → GTK-free daemon (#666/#584) that **also wears the plugin hat** since #866 (Annika's call on that thread): it deps `hytte-plugin` — the one arrow out of this entry, pointing down into the plugin-side block below — while still nothing in the tree links *it*. Its primary duty is unchanged: one loopback HTTP route (`POST /v1/chat/completions` on `127.0.0.1:8787`) that the LLM plugins (pet, caw) consume purely as a `Provider` base URL, so neither needed a code change. The second hat is `hytte-plugin-infobroker`'s shape — a real daemon that also paints a chip — and it is what lets the bridge ride `programs.trollshell.plugins` (the launcher, the control-center's Plugins tab, #392's keyring injection) instead of a hand-declared systemd unit; `nix/hm-module.nix` renders `plugins.claude-bridge` and declares no `trollshell-claude-bridge` unit any more. **Where it differs from the infobroker is which duty owns the process, and that is deliberate**: the infobroker starts its socket server from `sources()`, so the server lives one plugin session; the bridge's clients are other plugins making paid calls, so `main` binds and spawns the HTTP listener on its own multi-thread runtime *before* handing the main thread to `hytte_plugin::run` (which builds a current-thread runtime and blocks forever — hence `main` is not `#[tokio::main]`). With no `XDG_RUNTIME_DIR` it parks on the HTTP runtime rather than letting the SDK exit the process. The two runtimes share only `src/status.rs`'s atomics. Linking the SDK drags no GUI closure (proto + hytte-preem + tokio) and added no `Cargo.lock` entry. Still the sole consumer of `hive-claude`, which was the workspace's last git dependency until #757 moved it to crates.io (see "Lint" above).

— plugin side (#35 frontend B; out-of-process, NEVER links the shell):
hytte-plugin-proto → GTK-free wire protocol (node vocab, manifest, MessagePack framing, socket_path); language-neutral schema anchor, tokio optional
hytte-preem        → GTK-free leaf: the retro raster kit (#356) — dot_matrix, marquee, seven_seg, textbox, led_strip, scope, gauge, split_flap, font, Frame, DisplayStyle. Pure `std` plus one `hytte-plugin-proto` dep for `Frame::into_node`. Lived inside `hytte-plugin` until #859; extracted for the `hytte-config` reason — the shell wants to rasterise with it too (#857) and should not have to link the plugin *client* SDK to do it. `hytte-plugin` re-exports it (`pub use hytte_preem as preem;`), so every `hytte_plugin::preem::…` path a plugin already wrote still resolves
hytte-plugin       → the Rust plugin runtime SDK over the proto: TEA `Plugin` trait + `run()` (dial/backoff, Register handshake, session loop, render dedup). A plugin binary deps THIS crate alone
hytte-plugin-clock-demo → the reference plugin: pure manifest/init/update/view + one-line main
hytte-plugin-pet   → the kaomoji cat (#276): clock-driven moods, pokeable, optional llama-server brain (thin ureq client; canned fallback)
…and 9 more plugin binaries (hytte-plugin-{audio-widget,bar-clock-demo,preem-demo,timer,terminal,caw,departures,weather,usage}) following the same shape, plus hytte-plugin-infobroker — the count drifts, so trust `ls crates/hytte-plugin-*` over this line
```

Shell code uses `use hytte::prelude::*;` (App, Bar, Edge, Monitor, bind*, Service, …) plus `hytte::gtk` / `hytte::adw` / `hytte::services::*`. Don't add direct deps on gtk/adw/futures-signals in the binary — go through the re-exports.

### `hytte-bus`

All D-Bus goes through here, never raw zbus. Connections are pooled singletons (lazy session + system), with reconnection handled in `connection.rs`. Builders: `call()`, `property()`, `proxy()`, `signals()`, `own_name()`, `export_object()` — each takes the target bus as an explicit first argument, e.g. `call(BusKind::System, "org.freedesktop.UPower")` (#447 retired the asymmetric per-builder defaults and the `.bus(…)` override; the bus is stated at the constructor so a call to a system daemon can never silently land on the session bus). Property/proxy subscriptions surface a `PropState`/`ProxyState` (`Loading` → `Loaded`/`Stale`) rather than blocking. zbus is still a direct dep of `hytte-services` only for `zvariant` data types and the `#[zbus::interface]` macros — not for constructing connections.

## The `trollshell` binary

`main.rs` builds the `App`, registering each service module with its own `.with(foo::service())` call (one line per service — see the `hytte::services::{…}` import list atop `main.rs` for the current roster; there's no count maintained here on purpose, it only rots), then in the body closure builds a `Bar` per monitor and installs overlays. **Multi-monitor is explicit**: iterate `app.monitors()` and react to `app.monitors_changed()` to rebuild bars on hot-plug (there is intentionally no `on_all_monitors` helper).

Source layout (each module has a consistent shape — match it when adding):

- `widgets/` — bar chips. Each `pub fn widget(monitor) -> gtk::Widget`, binds to service signals, and on click calls `modal::toggle(monitor, Page::…, &btn)`.
- `panels/` — drawer pages mounted into `modal.rs`'s per-monitor `gtk::Stack`. Each `pub fn panel_<name>() -> gtk::Widget`.
- `overlays/` — per-monitor layer-shell overlays (consent, frame, notifications, osd, prompt, sidebar). Each `pub fn install(…)` wires the overlay to a signal source.
- `modal.rs` — the slide-out drawer system (`Page` enum, per-monitor drawer window/revealer).
- `components/` — cross-cutting `pub(crate)` building blocks reused across panels.
- `assets.rs` — resolves bundled asset paths via `TROLLSHELL_DATA_DIR` (runtime env → compile-time env baked by Nix → `CARGO_MANIFEST_DIR` dev fallback). Asset sources live in the top-level `assets/` dir mirroring the runtime `share/` layout: `assets/trollshell/{style.css,icons/}` and `assets/hytte-ui/style.css`.
- `commands.rs` — `gio::ActionEntry`s registered on the `adw::Application` (`org.gtk.Actions`) so niri keybinds can drive drawer/power-menu/sidebar actions that are otherwise mouse-only (#219).
- `control.rs` — the `mov.vibec0re.trollshell.Control` D-Bus endpoint that `trollshell-control-center` (and future tabs) bind to; transport only, no UI (#390).
- `plugins/` — the out-of-process widget-plugin **host transport** (a module dir since #443, not a single file — `mod.rs` plus `listener.rs`/`session.rs`/`effects.rs`/`pump.rs`/etc.): the per-user socket listener, the GTK-side clock pump, and the effect broker (#35). Since #893 stage B it also holds `preem_gl/` — the GPU arm of `preem_render`: the `Scope` shader pipeline, its `*.vert`/`*.frag` (under `src/`, `include_str!`'d, with a matching clause in `nix/package.nix`'s crane filter — the filter is by extension, so `src/` alone does not save them, and the `glsl` flake check compiles them), the pure state→uniform mapping, and the `TROLLSHELL_PREEM_RENDERER=cpu` kill switch. GL is the **default**; the CPU kit is the fallback for a failed context or a kind with no GL arm. Since #893 it also holds `shader_map.rs` — the host half of the **shader widget** (`Node::Shader`: a plugin ships a fragment body plus a data buffer, the shell compiles the body once in `hytte-ui`'s `ShaderSurface` and per frame re-uploads only the buffer). Its trust boundary is **route 0, the socket itself** (spec §"Trust boundary for #893"): `Capability::Shader` is an ordinary auto-granted manifest capability, the enforced checks are a 16 KiB source cap / a 4 MiB data cap / the buffer-shape invariant / the capability, each degrading to the broken-widget placeholder with one warning — and there is deliberately **no runtime validator**, because naga cannot parse GLSL ES at all. Blast radius of a runaway shader is the whole shell; GL-only by design, no CPU arm.
- `plugin_launcher.rs` — the declarative plugin **launcher** (#419): reads the nix-written `trollshell/plugins.json` (XDG config, rendered from `programs.trollshell.plugins`) and launches each enabled plugin as a transient `trollshell-plugin-<id>` user unit via `systemd-run --user`; the control-center Plugins tab's start/stop (#348) routes through it, and its `extra_env` spawn hook is where #392's key injection rides. Hand-installed static units under `etc/` keep working (legacy fallback).
- `revision.rs` — resolves the build's git revision via `TROLLSHELL_REV` (runtime env → compile-time env → `"dev"` fallback), the same three-tier shape as `assets.rs` (#601). The nix side injects it **only** through the cheap wrapper slices' `preFixup` (`nix/package.nix`, `nix/control-center.nix`) — never as a compile-time env, which would rehash the single `workspace` crane compile on every commit. Surfaced over D-Bus as `Control.Revision`; whether it also gets a UI surface is still open.
- `scale.rs` — font-relative pixel scaling (`scale()`) for the handful of Rust-set sizes CSS `em` can't reach (#135).

### Conventions

- CSS classes: `hytte-*` come from the library default stylesheet (`assets/hytte-ui/style.css`); `ts-*` come from the binary's `assets/trollshell/style.css` (loaded as user style at higher priority).
- App ID `mov.vibec0re.trollshell`; D-Bus agent names `mov.vibec0re.trollshell.{bluez,iwd}-agent` (polkit is now a standalone external agent, not in-process). The NetworkManager secret agent (`wifi/nm_agent.rs`) registers with NM's `AgentManager` (no extra bus-name; `RegisterWithCapabilities`) rather than owning a `mov.vibec0re.*` name.
- Logging via `tracing`.

## Deployment & session integration

`etc/` holds the full Niri-session config the shell expects (systemd user units incl. `trollshell.service` and `niri-session.target`, niri keybinds, kanshi display profiles, cliphist) — see `etc/README.md`. The idle → dim → lock → suspend pipeline is **native** (in-process; `crates/hytte-services/src/idle_notify.rs`, an `ext-idle-notify-v1` client gated on logind inhibitors — #204 retired swayidle), so there is no idle daemon unit. trollshell ships no in-shell lock screen; locking is delegated to `swaylock`, driven by the native idle timer / logind's `Lock` signal — see `etc/README.md`'s "Idle & screen locking" section for wiring `swaylock`'s own PAM stack. The flake exposes `nixosModules.default` (`programs.trollshell.enable`), which installs the package. Everything else it pulls in — the system-bus policy permitting the two agent names (`bluez`/`iwd`), the polkit-gnome user service, UPower, power-profiles-daemon, and geoclue2 — sits behind `programs.trollshell.enableRecommendedServices` (default `true`); each is `mkDefault` so an explicit `enable = false;` still wins. With the switch off, the chips that back onto a missing daemon hide themselves (battery on `BatteryState::Unknown`, the power-profile group on empty `available`, weather falls back to `TROLLSHELL_WEATHER_CITY`) and the bluez/iwd agents park inert on `AccessDenied`. A `homeModules.default` (home-manager) runs the shell as a user service and shares the same option base (`nix/module-common.nix`).

## Known gotchas

- **Niri fullscreen detection:** `WindowLayoutsChanged` is the _only_ niri-ipc event that fires on a fullscreen toggle (`WindowsChanged`/`WindowOpenedOrChanged` do not). The frame overlay relies on this.
- **Icons render as `image-missing`** if you run outside the devShell, or if the icon theme isn't forced — `main.rs` calls `set_gtk_icon_theme_name("Adwaita")` to work around GSettings schemas not being visible under `cargo run`.
