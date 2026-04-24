# trollshell + hytte: Design Spec

**Date:** 2026-04-24
**Status:** Approved (pending implementation plan)

## Summary

`trollshell` is a personal Wayland desktop shell for the Niri compositor, built on `hytte` — a Rust library for composing GTK4 + libadwaita + gtk4-layer-shell desktop shells. `hytte` is library-first: the consumer (the shell binary) writes plain Rust to wire up bars, popups, services, and bindings. No external config DSL, no QML-style hot reload; instead, `hytte` connects to existing system daemons so iterating on the shell binary does not lose system state.

## Naming

- **`trollshell`** — the consumer binary; the personal desktop shell.
- **`hytte`** — the library crate (umbrella). "Hytte" is Norwegian for hut/cabin: a small hut you build shells in.

## Goals

- Personal-first: `trollshell` is the primary consumer, but `hytte` is general enough for someone else to build a different shell on top.
- Composable, not configurable: the consumer writes Rust, not config files.
- Robust to dev iteration: `cargo run` restarts of the shell do not lose system state (network, bluetooth, mpris, etc.).
- "Batteries included" coverage: bar widgets for clock, workspaces, mpris, tray, network, bluetooth, volume, brightness, notifications.
- Niri compositor first.

## Non-goals (v1)

- Hot-reload of shell code without restart (Rust dylib hot-swap is fragile; `cargo run` restart is fine because services persist in system daemons).
- Multi-compositor abstraction (Niri only; trait extraction comes later if a second backend is added).
- Configuration via external DSL / Lua / QML.
- Lockscreen, OSD, or compositor in `hytte`.

## Architecture

### Tech stack

- Rust (stable)
- `gtk4-rs` + `libadwaita-rs`
- `gtk4-layer-shell` for layer-shell windows
- `niri-ipc` for compositor IPC
- `zbus` for DBus
- `tokio` (multi-thread) for async runtime
- `futures-signals` for reactive primitives
- `glib::MainContext::spawn_local` to bridge tokio tasks to the GTK main loop

### Crate layout

```
trollshell-workspace/
├── Cargo.toml                    # workspace
├── crates/
│   ├── hytte-ui/                 # App, Bar, Panel, Popup, LayerWindow
│   ├── hytte-reactive/           # bind(), Service trait, gtk↔futures-signals bridge
│   ├── hytte-services/           # service modules (see below)
│   └── hytte/                    # umbrella re-export crate
└── trollshell/                   # binary
    ├── Cargo.toml
    ├── src/main.rs
    └── style.css
```

Dependency graph:

- `hytte-ui` → `hytte-reactive`
- `hytte-services` → `hytte-reactive`
- `hytte` re-exports all three
- `trollshell` → `hytte`

### State model: thread-local registry + tokio backend

The "shared object problem" in Rust (`Arc<Mutex<...>>` threaded through every widget) is avoided by a strict separation between **handles** and **work**:

1. Service handles (`Mutable<State>` from `futures-signals`) live in a thread-local registry on the GTK main thread.
2. Service I/O (DBus, sockets) runs on a multi-threaded tokio runtime on a separate thread pool.
3. Tokio tasks call `mutable.set(new_state)` directly — `Mutable` is `Send + Sync`.
4. Widgets bind to signals via `bind(signal, &widget, |w, v| w.set_x(v))`, which spawns the apply-loop on `glib::MainContext` (GTK main thread).

The consumer never sees an `Arc`, `Mutex`, or service handle — only free functions returning `impl Signal`.

### Reactive flow

```
┌─ gtk main thread ─────────────────────────────────┐
│  thread-local REGISTRY                            │
│    networkd.primary  : Mutable<LinkState>  ◄─┐    │
│    bluetooth.adapter : Mutable<BtState>    ◄─┼─┐  │
│  widgets: bind(networkd::primary(), ...)     │ │  │
└──────────────────────────────────────────────┼─┼──┘
                                    glib::idle │ │
┌─ tokio runtime (separate thread pool) ───────┼─┼──┐
│  zbus task: networkd DBus signals ───────────┘ │  │
│  zbus task: BlueZ DBus signals ────────────────┘  │
│  niri-ipc task: socket reads                      │
│  mpris task: per-player tracking                  │
└───────────────────────────────────────────────────┘
```

### Service architecture: system-daemon-as-state-store

`hytte` services are thin async clients to existing persistent system daemons (systemd-networkd, BlueZ, PipeWire, MPRIS players, iwd, …). Persistent state lives in those daemons, not in `hytte`. Restarting `trollshell` reconnects without state loss — the system daemons keep running across shell restarts.

**Exception: notifications.** `org.freedesktop.Notifications` is a singleton DBus service per session, so `hytte_services::notifications` registers as the daemon itself. When running `trollshell`, the user must disable mako/dunst.

**Future upgrade path (not v1):** a sidecar `hytted` process for state we do want to persist across shell restarts beyond what system daemons track (dismissed-notification history, custom shell-state, counters). Out of scope for v1.

## Public API surface

### `hytte-ui`

```rust
pub struct App;
pub struct AppBuilder;

impl App {
    pub fn new(app_id: &str) -> AppBuilder;
}
impl AppBuilder {
    pub fn with<S: Service>(self, s: S) -> Self;
    pub fn with_user_style(self, css_path: impl AsRef<Path>) -> Self;
    pub fn run<F: FnOnce(&App)>(self, body: F) -> Result<()>;
}
impl App {
    pub fn monitors(&self) -> Vec<Monitor>;
    pub fn monitors_changed(&self) -> impl Signal<Item = Vec<Monitor>>;
}

pub enum Edge { Top, Bottom, Left, Right }

pub struct Bar;
impl Bar {
    pub fn new(monitor: &Monitor) -> Self;
    pub fn edge(self, e: Edge) -> Self;
    pub fn left  (self, ws: impl IntoIterator<Item = gtk::Widget>) -> Self;
    pub fn center(self, ws: impl IntoIterator<Item = gtk::Widget>) -> Self;
    pub fn right (self, ws: impl IntoIterator<Item = gtk::Widget>) -> Self;
    pub fn margin(self, m: Margin) -> Self;
    pub fn exclusive(self, on: bool) -> Self;
    pub fn show(self) -> BarHandle;
}

pub struct Panel;        // generic layer-shell window
pub struct Popup;        // anchored ephemeral popup
pub struct LayerWindow;  // raw primitive
```

**Multi-monitor:** the consumer iterates `app.monitors()` and listens to `app.monitors_changed()` to handle hot-plug. No `on_all_monitors()` helper — explicit is preferred.

### `hytte-reactive`

```rust
pub fn bind<S, W, F>(signal: S, widget: &W, apply: F)
where
    S: Signal + 'static,
    W: IsA<gtk::Widget>,
    F: Fn(&W, S::Item) + 'static;

pub fn bind_class<S>(signal: S, widget: &impl IsA<gtk::Widget>, class: &str)
where S: Signal<Item = bool> + 'static;

pub fn bind_visible<S>(signal: S, widget: &impl IsA<gtk::Widget>)
where S: Signal<Item = bool> + 'static;

pub fn bind_text<S>(signal: S, label: &gtk::Label)
where S: Signal<Item: AsRef<str>> + 'static;

pub trait Service: 'static {
    fn start(self: Box<Self>, rt: &tokio::runtime::Handle) -> Box<dyn Any>;
}
```

### `hytte-services`

Consistent pattern per service module:

- `pub fn service() -> impl Service` — registered via `App::with(...)`.
- Free functions returning `impl Signal<Item = ...>` for subscribable state.
- Free functions for fire-and-forget commands (e.g. `niri::focus_workspace(id)`).

Modules in v1:

| Module          | Backend                                                        |
|-----------------|----------------------------------------------------------------|
| `clock`         | `glib` timer + `chrono::Local`                                 |
| `niri`          | `niri-ipc` socket                                              |
| `mpris`         | `org.mpris.MediaPlayer2.*` via zbus                            |
| `tray`          | StatusNotifierItem host (zbus)                                 |
| `networkd`      | `org.freedesktop.network1` (systemd-networkd, zbus)            |
| `resolved`      | `org.freedesktop.resolve1` (systemd-resolved, zbus)            |
| `wifi`          | `net.connman.iwd` (zbus)                                       |
| `bluetooth`     | `org.bluez` (zbus)                                             |
| `pipewire`      | `pipewire-rs`                                                  |
| `notifications` | own daemon, registers `org.freedesktop.Notifications`          |
| `brightness`    | logind `org.freedesktop.login1.Session.SetBrightness`          |
| `upower`        | `org.freedesktop.UPower` (zbus)                                |

Feature-flagged opt-in modules:

| Module     | Backend                                              |
|------------|------------------------------------------------------|
| `weather`  | `libgweather` via gobject-introspection bindings     |
| `calendar` | `evolution-data-server` (zbus)                       |
| `accounts` | `gnome-online-accounts` (zbus)                       |
| `location` | `GeoClue2` (zbus)                                    |

## Styling

`hytte-ui` ships opinionated default CSS for shell contexts (transparent bars, rounded popups, tasteful spacing) loaded via `gtk::CssProvider` at `STYLE_PROVIDER_PRIORITY_APPLICATION`. The consumer can layer additional CSS via `App::with_user_style(path)` at higher priority for per-shell overrides.

## Example consumer code

```rust
use hytte::ui::{App, Bar, Edge};
use hytte::services::{niri, mpris, tray, networkd, bluetooth, pipewire};

fn main() -> hytte::Result<()> {
    App::new("cc.hannig.trollshell")
        .with(niri::service())
        .with(networkd::service())
        .with(bluetooth::service())
        .with(mpris::service())
        .with(tray::service())
        .with(pipewire::service())
        .with_user_style("style.css")
        .run(|app| {
            for monitor in app.monitors() {
                Bar::new(&monitor)
                    .edge(Edge::Top)
                    .exclusive(true)
                    .left([workspaces_widget()])
                    .center([mpris_widget()])
                    .right([tray_widget(), quick_settings(), clock_widget()])
                    .show();
            }
        })
}
```

Hot-plug example:

```rust
use futures_signals::signal::SignalExt;

app.monitors_changed().for_each(|monitors| {
    // tear down old bars, build new ones for current set
}).await;
```

## MVP slicing

Each version is independently usable on real hardware.

### v0.1 — "bar exists"

- `hytte-ui`: `App`, `Bar`, `LayerWindow`
- `hytte-reactive`: `bind`, `bind_text`, `bind_visible`, `bind_class`
- `hytte-services`: `clock`, `niri`
- `trollshell`: bar with clock + workspaces

### v0.2 — "system is visible" (read-only indicators)

- `hytte-ui`: `Popup`, `Panel`
- `hytte-services`: `pipewire` (read), `networkd`, `resolved`, `bluetooth` (read), `upower`
- `trollshell`: right-cluster indicators, popup on click

### v0.3 — "interaction + media + tray"

- `hytte-services`: `mpris`, `tray` (SNI host — single hardest piece), `wifi`, `brightness`
- `trollshell`: media widget (center), tray, quick-settings popup with toggles/sliders

### v0.4 — "notifications"

- `hytte-services::notifications` registered as `org.freedesktop.Notifications` daemon
- `trollshell`: toasts + history popup

### v0.5+ (opt-in)

- `weather`, `calendar`, `accounts`, `location`

## Testing

- Each `hytte-services` module: unit tests with a mock zbus connection for state-translation logic (zbus has fake-bus support).
- `hytte-reactive::bind`: integration tests under `gtk::test_init` with a synthetic `Mutable` source, asserting widget state after main-loop tick.
- `hytte-ui` widgets: smoke tests instantiating bars on a headless GTK setup. Layer-shell requires a real Wayland compositor — defer end-to-end to manual testing on Niri.
- `trollshell`: manual on real hardware. CI runs `cargo check --all-targets --all-features` and `cargo clippy -- -D warnings`.

## Risks and unknowns

- **Tray (StatusNotifierItem host)** is the riskiest piece. GTK4 has no native tray support and the SNI protocol is finicky. Prototype before v0.3 plan locks scope.
- **`Service::start` return-type ergonomics**: `Box<dyn Any>` works but consumers of the registry must `downcast_ref::<NetworkHandles>()`. Plan to evaluate a typed handle-bag if friction shows up.
- **`hytte-ui::App` base class**: likely wraps `adw::Application` rather than `gtk::Application` for Adwaita styling — confirm during v0.1 implementation.
- **PipeWire bindings**: `pipewire-rs` API ergonomics may push us to wrap `pw-cli`/dbus instead. Re-evaluate at v0.2.
