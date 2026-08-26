# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust workspace with two layers:

- **`hytte`** — a library-first toolkit for composing GTK4 + libadwaita + `gtk4-layer-shell` Wayland desktop shells. Split across `crates/hytte-*`.
- **`trollshell`** — the personal shell binary built on `hytte`, targeting the **Niri** compositor.

"Composable, not configurable": there is no config DSL. The shell is wired up in plain Rust in `trollshell/src/main.rs`. The canonical design is `docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md`; most subsequent features have a design spec under `docs/superpowers/specs/` (the why) and a plan under `docs/superpowers/plans/` (the how), named by feature or version — the literal same-stem `<date>-<feature>` pairing doesn't hold for all of them. Consult these before changing a subsystem — they are the source of truth for intent.

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
  `nix shell nixpkgs#python3 --command python3 nix/lint-bind-pins.py`. The
  script's header documents the deliberate carve-out (capturing a _different_
  widget is correct) and why it paren/brace-matches instead of using a regex —
  read it before changing it.

### Lint — strict, treat as the gate

The workspace lint config (`Cargo.toml`) is deliberately severe; a violation fails `cargo check`, not just clippy:

- `unsafe_code = "forbid"` workspace-wide. **Only `hytte-ecal`** overrides this (it's pure FFI; unsafety is confined to safe wrappers in its `lib.rs`).
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
hytte-config      → GTK-free leaf (serde/toml/toml_edit/tracing only): the `places.toml` schema + validation + its format-preserving `toml_edit` writer, plus the atomic `~/.config/trollshell/*` write helper (ex `hytte-services::config_file`, aliased back as `pub(crate) use hytte_config::file as config_file` so in-crate call sites still read `config_file::…`). Consumed by BOTH `hytte-services` and `trollshell-control-center` — the first crate the shell's service layer and the companion app share, which is exactly why it exists: `places.toml` has two editors (#640/#703, the file stays hand-editable) and they must agree byte for byte rather than each carrying its own serialisation path
hytte-ecal        → hand-written FFI to evolution-data-server (libecal); the ONLY crate allowed `unsafe`
hytte-ai-providers → shared OpenAI-compatible chat client + provider config + key-file loader, used by plugins that talk to an LLM (e.g. hytte-plugin-pet)
hytte             → umbrella: re-exports {bus, reactive, services, ui} + a `prelude`
trollshell        → the binary; depends on `hytte`
trollshell-control-center → separate windowed GTK4/libadwaita companion app (#390/#399); talks to the running shell over its own `Control` D-Bus endpoint (trollshell/src/control.rs), never linked into the shell. It cannot link `hytte-services` either (that would drag libpipewire + evolution-data-server into a settings app), but since #640 it does link `hytte-config` — a shared GTK-free *leaf library*, not a runtime link to the shell: its Places tab reads and writes `places.toml` directly, through the same writer `hytte-services` uses, so the editor keeps working while the shell is down and the shell's existing mtime poll picks a save up with no new D-Bus surface
hytte-claude-bridge → standalone GTK-free daemon (#666/#584); links nothing in this graph and nothing links it — no arrow in this diagram at all. Serves one loopback HTTP route (`POST /v1/chat/completions` on `127.0.0.1:8787`) that the LLM plugins (pet, caw) consume purely as a `Provider` base URL, so neither needed a code change. The sole consumer of `hive-claude`, which was the workspace's last git dependency until #757 moved it to crates.io (see "Lint" above).

— plugin side (#35 frontend B; out-of-process, NEVER links the shell):
hytte-plugin-proto → GTK-free wire protocol (node vocab, manifest, MessagePack framing, socket_path); language-neutral schema anchor, tokio optional
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
- `plugins/` — the out-of-process widget-plugin **host transport** (a module dir since #443, not a single file — `mod.rs` plus `listener.rs`/`session.rs`/`effects.rs`/`pump.rs`/etc.): the per-user socket listener, the GTK-side clock pump, and the effect broker (#35).
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
