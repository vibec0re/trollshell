# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust workspace with two layers:

- **`hytte`** — a library-first toolkit for composing GTK4 + libadwaita + `gtk4-layer-shell` Wayland desktop shells. Split across `crates/hytte-*`.
- **`trollshell`** — the personal shell binary built on `hytte`, targeting the **Niri** compositor.

"Composable, not configurable": there is no config DSL. The shell is wired up in plain Rust in `trollshell/src/main.rs`. The canonical design is `docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md`; every subsequent feature has a paired `docs/superpowers/specs/<date>-<feature>-design.md` (the why) and `docs/superpowers/plans/<date>-<feature>.md` (the how). Consult these before changing a subsystem — they are the source of truth for intent.

## Build / run / test

**You must work inside the Nix devShell.** `.envrc` is `use flake` (direnv); if direnv isn't active, run `nix develop` first. The devShell sets env that the build and runtime both require:
- `LD_LIBRARY_PATH`/`LIBCLANG_PATH` so the two bindgen consumers (`hytte-pam` via pam-sys, and pipewire-sys/libspa-sys) can load libclang. Outside the shell, the build panics with *"a libclang shared library is not loaded on this thread."*
- `XDG_DATA_DIRS` + `GSETTINGS_SCHEMA_DIR` so GTK finds Adwaita symbolic icons and GSettings schemas. Outside the shell, most bar icons render as `image-missing`.

```sh
cargo build --release -p trollshell          # build the binary
cargo run -p trollshell                       # run it (needs a live Niri session: connects to $NIRI_SOCKET)
RUST_LOG=hytte_services=debug,trollshell=debug cargo run -p trollshell   # with logs
nix build                                     # build the packaged binary (.#trollshell)
```

`trollshell` is a real Wayland shell — running it meaningfully requires being **inside a Niri session**. Layer-shell surfaces, the lock screen, and most services need live system daemons.

### Tests

```sh
cargo test                                    # whole workspace
cargo test -p hytte-bus --test signals        # one integration-test binary
cargo test -p hytte-services clock            # tests matching a name
```

- `hytte-bus` integration tests **spawn a real `dbus-daemon`** (one ephemeral broker per test; must be on `PATH`). They don't touch the host session bus.
- GTK-dependent tests are marked `#[ignore]` — they need a display server. The Nix package sets `doCheck = false` because tests touch live daemons.

### Lint — strict, treat as the gate

The workspace lint config (`Cargo.toml`) is deliberately severe; a violation fails `cargo check`, not just clippy:
- `unsafe_code = "forbid"` workspace-wide. **Only `hytte-ecal`** overrides this (it's pure FFI; unsafety is confined to safe wrappers in its `lib.rs`).
- clippy `all` **and** `pedantic` at `deny`. Code must be pedantic-clean.
- `disallowed_methods`: `zbus::Connection::session`/`::system` are **banned** (see `clippy.toml`). All D-Bus access goes through the `hytte-bus` primitives, never a raw zbus connection.

```sh
cargo clippy --workspace --all-targets        # must be clean
cargo fmt --all
```

Edition 2024, MSRV 1.85. The nix build and devShell use nixpkgs' rust toolchain (via crane); there is no `rust-toolchain.toml` pin.

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

Services are **thin async clients to persistent system daemons** (systemd-networkd, BlueZ, PipeWire, UPower, logind, niri-ipc, iwd, evolution-data-server, …). Persistent state lives in the daemon, not in `hytte`, so restarting `trollshell` during dev reconnects **without losing system state**. This is a core design constraint — keep new state in the daemon where possible.

**Exception:** `notifications` registers itself as the `org.freedesktop.Notifications` daemon (a session singleton), so any other notification daemon (mako/dunst) must be disabled.

### Crate graph

```
hytte-reactive   ← base: Service trait, Registry, tokio runtime, bind() helpers
  ↑   ↑   ↑
hytte-ui  hytte-services  hytte-bus
hytte-ui          → App/AppBuilder (wraps adw::Application), Bar, LayerWindow, Popup, Monitor; layer-shell + ext-session-lock-v1; default stylesheet
hytte-bus         → shared D-Bus layer: call / property / proxy / signals / own_name builders over pooled session+system connections
hytte-services    → the service modules (clients to daemons)
hytte-ecal        → hand-written FFI to evolution-data-server (libecal); the ONLY crate allowed `unsafe`
hytte-pam         → synchronous PAM auth for the lock screen
hytte             → umbrella: re-exports {bus, reactive, services, ui} + a `prelude`
trollshell        → the binary; depends on `hytte` + `hytte-pam`
```

Shell code uses `use hytte::prelude::*;` (App, Bar, Edge, Monitor, bind*, Service, …) plus `hytte::gtk` / `hytte::adw` / `hytte::services::*`. Don't add direct deps on gtk/adw/futures-signals in the binary — go through the re-exports.

### `hytte-bus`

All D-Bus goes through here, never raw zbus. Connections are pooled singletons (lazy session + system), with reconnection handled in `connection.rs`. Builders: `call()`, `property()`, `proxy()`, `signals()`, `own_name()` — **note their default bus differs** (e.g. `call`/`own_name` default to session; `property`/`signals`/`proxy` default to system) — override with `.bus(BusKind::…)`. Property/proxy subscriptions surface a `PropState`/`ProxyState` (`Loading` → `Loaded`/`Stale`) rather than blocking. zbus is still a direct dep of `hytte-services` only for `zvariant` data types and the `#[zbus::interface]` macros — not for constructing connections.

## The `trollshell` binary

`main.rs` builds the `App`, registers ~28 services with `.with(…)`, then in the body closure builds a `Bar` per monitor and installs overlays. **Multi-monitor is explicit**: iterate `app.monitors()` and react to `app.monitors_changed()` to rebuild bars on hot-plug (there is intentionally no `on_all_monitors` helper).

Source layout (each module has a consistent shape — match it when adding):
- `widgets/` — bar chips. Each `pub fn widget(monitor) -> gtk::Widget`, binds to service signals, and on click calls `modal::toggle(monitor, Page::…, &btn)`.
- `panels/` — drawer pages mounted into `modal.rs`'s per-monitor `gtk::Stack`. Each `pub fn panel_<name>() -> gtk::Widget`.
- `overlays/` — per-monitor layer-shell overlays (lock_screen, osd, notifications, frame, prompt, sidebar). Each `pub fn install(…)` wires the overlay to a signal source.
- `modal.rs` — the slide-out drawer system (`Page` enum, per-monitor drawer window/revealer).
- `components/` — cross-cutting `pub(crate)` building blocks reused across panels.
- `assets.rs` — resolves bundled asset paths via `TROLLSHELL_DATA_DIR` (runtime env → compile-time env baked by Nix → `CARGO_MANIFEST_DIR` dev fallback). Assets live in `trollshell/{icons,style.css}`.

### Conventions

- CSS classes: `hytte-*` come from the library default stylesheet (`hytte-ui/src/style.css`); `ts-*` come from the binary's `trollshell/style.css` (loaded as user style at higher priority).
- App ID `cc.hannig.trollshell`; D-Bus agent names `cc.hannig.trollshell.{bluez,iwd}-agent` (polkit is now a standalone external agent, not in-process).
- Logging via `tracing`.

## Deployment & session integration

`etc/` holds the full Niri-session config the shell expects (systemd user units incl. `trollshell.service` and `niri-session.target`, niri keybinds, swayidle idle/lock pipeline, kanshi display profiles, cliphist, the PAM service file) — see `etc/README.md`. The flake exposes `nixosModules.default` (`programs.trollshell.enable`) which, beyond installing the package, declares the `trollshell` PAM service, a system-bus policy permitting the three agent names, and enables UPower + power-profiles-daemon (the battery/power chips stay hidden without them).

## Known gotchas

- **Niri fullscreen detection:** `WindowLayoutsChanged` is the *only* niri-ipc event that fires on a fullscreen toggle (`WindowsChanged`/`WindowOpenedOrChanged` do not). The frame overlay relies on this.
- **Icons render as `image-missing`** if you run outside the devShell, or if the icon theme isn't forced — `main.rs` calls `set_gtk_icon_theme_name("Adwaita")` to work around GSettings schemas not being visible under `cargo run`.
- **bindgen 0.69 vs 0.72:** `hytte-pam` force-enables bindgen 0.69's `runtime` feature in its build-deps because the pipewire crates pull bindgen 0.72 and flip clang-sys to runtime linking workspace-wide; the two majors don't share features. Don't remove that otherwise-unused build-dep.
