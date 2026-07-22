# hytte + trollshell v0.1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the v0.1 "bar exists" milestone of the design at `docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md` — a workspace with `hytte-reactive`, `hytte-ui`, `hytte-services` (clock + niri only), umbrella `hytte`, and a `trollshell` binary that draws a top-edge layer-shell bar on every Niri monitor with workspaces (left) and a clock (right).

**Architecture:** Library-first. `hytte-reactive` provides a `Service` trait, a thread-local typed `Registry` of service handles, a multi-thread `tokio` runtime accessed via a `OnceCell`, and a `bind(signal, &widget, apply)` helper that bridges `futures_signals::Signal` updates onto the GTK main loop via `glib::MainContext::spawn_local`. `hytte-ui` wraps `adw::Application` and exposes `App`/`Bar`/`LayerWindow` builders that produce `gtk4-layer-shell` windows. `hytte-services::niri` opens the Niri IPC socket, reads its event stream on a dedicated tokio task, and pushes state into `Mutable`s that widgets subscribe to.

**Tech Stack:** Rust 2024 edition, `gtk4`, `libadwaita`, `gtk4-layer-shell`, `niri-ipc`, `tokio` (multi-thread), `futures-signals`, `chrono`, `serde_json` (event-stream parsing fallback), `anyhow` for error plumbing in services.

---

## File Structure

After this plan completes the workspace looks like:

```
trollshell-workspace/
├── Cargo.toml                         # workspace, shared profiles
├── rust-toolchain.toml                # pin stable
├── .gitignore
├── README.md
├── crates/
│   ├── hytte-reactive/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs                 # public re-exports
│   │       ├── runtime.rs             # tokio runtime + Handle accessor
│   │       ├── registry.rs            # Service trait, Registry, REGISTRY thread_local
│   │       └── bind.rs                # bind, bind_text, bind_visible, bind_class
│   ├── hytte-ui/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── error.rs               # Result, Error
│   │       ├── monitor.rs             # Monitor (wraps gdk::Monitor)
│   │       ├── app.rs                 # App, AppBuilder
│   │       ├── layer_window.rs        # LayerWindow primitive
│   │       ├── bar.rs                 # Bar
│   │       └── style.css              # default shell stylesheet, include_str!
│   ├── hytte-services/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── clock.rs               # clock service
│   │       └── niri.rs                # niri service
│   └── hytte/
│       ├── Cargo.toml
│       └── src/
│           └── lib.rs                 # umbrella re-exports: ui, reactive, services
└── trollshell/
    ├── Cargo.toml
    ├── style.css                      # user CSS overrides
    └── src/
        ├── main.rs
        └── widgets/
            ├── mod.rs
            ├── clock.rs
            └── workspaces.rs
```

**Responsibility split:**

- `hytte-reactive` is the only crate that knows about both `tokio` and `glib`/`gtk` together. Everyone bridges through it.
- `hytte-ui` knows nothing about specific services — only widgets, windows, and how to start the registered services on activate.
- `hytte-services` knows nothing about widgets — it just exposes signals and commands.
- `hytte` is a re-export shim so consumers `use hytte::{ui::*, services::*, reactive::*};`.
- `trollshell` is the only binary; it wires everything together.

---

## Pre-flight (do once before Task 1)

Verify required system packages are installed on Arch:

```bash
pacman -Qi gtk4 libadwaita gtk4-layer-shell rust 2>&1 | grep '^Name'
```

Expected: lines for `gtk4`, `libadwaita`, `gtk4-layer-shell`, `rust`. If any missing:

```bash
sudo pacman -S --needed gtk4 libadwaita gtk4-layer-shell rust pkgconf
```

For tests that need a display (some `bind` tests, app smoke test):

```bash
pacman -Qi xorg-server-xvfb 2>&1 | grep '^Name'
```

If missing: `sudo pacman -S --needed xorg-server-xvfb`. (Tests that need a display are marked `#[ignore]` and run separately under `xvfb-run`.)

---

## Task 1: Workspace scaffold

Set up an empty but `cargo check`-clean workspace with all five crates declared and depending on each other.

**Files:**

- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`
- Create: `.gitignore`
- Create: `crates/hytte-reactive/Cargo.toml`
- Create: `crates/hytte-reactive/src/lib.rs`
- Create: `crates/hytte-ui/Cargo.toml`
- Create: `crates/hytte-ui/src/lib.rs`
- Create: `crates/hytte-services/Cargo.toml`
- Create: `crates/hytte-services/src/lib.rs`
- Create: `crates/hytte/Cargo.toml`
- Create: `crates/hytte/src/lib.rs`
- Create: `trollshell/Cargo.toml`
- Create: `trollshell/src/main.rs`

- [ ] **Step 1: Pin the toolchain**

Create `rust-toolchain.toml`:

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
profile = "minimal"
```

- [ ] **Step 2: Workspace `Cargo.toml`**

Create `Cargo.toml`:

```toml
[workspace]
resolver = "3"
members = [
    "crates/hytte-reactive",
    "crates/hytte-ui",
    "crates/hytte-services",
    "crates/hytte",
    "trollshell",
]

[workspace.package]
edition = "2024"
license = "MPL-2.0"
repository = "https://git.hannig.cc/choom/trollshell"
rust-version = "1.85"

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
pedantic = { level = "warn", priority = -1 }
module_name_repetitions = "allow"
missing_errors_doc = "allow"
missing_panics_doc = "allow"

[profile.release]
lto = "thin"
codegen-units = 1
strip = "symbols"
```

- [ ] **Step 3: `.gitignore`**

Create `.gitignore`:

```
/target
**/*.rs.bk
Cargo.lock.bak
.direnv
.envrc
```

(Keep `Cargo.lock` tracked — this is a binary workspace, lockfile belongs in git.)

- [ ] **Step 4: Empty `hytte-reactive` crate**

Create `crates/hytte-reactive/Cargo.toml`:

```toml
[package]
name = "hytte-reactive"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "GTK4 ↔ futures-signals bridge and service registry for hytte"

[lints]
workspace = true

[dependencies]
```

Create `crates/hytte-reactive/src/lib.rs`:

```rust
//! Bridge crate between GTK4's main loop and the `futures-signals` reactive
//! primitives, plus the hytte service registry. Service modules in
//! `hytte-services` register typed handles here at startup; widgets in
//! `hytte-ui` subscribe to them via `bind`.
```

- [ ] **Step 5: Empty `hytte-ui` crate**

Create `crates/hytte-ui/Cargo.toml`:

```toml
[package]
name = "hytte-ui"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "GTK4 + libadwaita + layer-shell window primitives for hytte"

[lints]
workspace = true

[dependencies]
hytte-reactive = { path = "../hytte-reactive" }
```

Create `crates/hytte-ui/src/lib.rs`:

```rust
//! GTK4 + libadwaita + gtk4-layer-shell window primitives. Provides the `App`
//! entry point, `Bar`/`Panel`/`Popup` builders, and a default shell
//! stylesheet.
```

- [ ] **Step 6: Empty `hytte-services` crate**

Create `crates/hytte-services/Cargo.toml`:

```toml
[package]
name = "hytte-services"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "Async clients to system daemons, exposed as hytte services"

[lints]
workspace = true

[dependencies]
hytte-reactive = { path = "../hytte-reactive" }
```

Create `crates/hytte-services/src/lib.rs`:

```rust
//! Async clients to system daemons exposed as hytte services. Each module
//! provides a `service()` constructor (registered via `App::with`) and free
//! functions returning `Signal`s of the daemon's state.
```

- [ ] **Step 7: Umbrella `hytte` crate**

Create `crates/hytte/Cargo.toml`:

```toml
[package]
name = "hytte"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "Library-first toolkit for composing GTK4 + Wayland desktop shells"

[lints]
workspace = true

[dependencies]
hytte-reactive = { path = "../hytte-reactive" }
hytte-ui       = { path = "../hytte-ui" }
hytte-services = { path = "../hytte-services" }
```

Create `crates/hytte/src/lib.rs`:

```rust
//! Library-first toolkit for composing GTK4 + Wayland desktop shells. This
//! crate just re-exports `hytte_ui`, `hytte_reactive`, and `hytte_services`
//! under shorter module paths.

pub use hytte_reactive as reactive;
pub use hytte_services as services;
pub use hytte_ui as ui;
```

- [ ] **Step 8: Empty `trollshell` binary**

Create `trollshell/Cargo.toml`:

```toml
[package]
name = "trollshell"
version = "0.1.0"
edition.workspace = true
license.workspace = true
repository.workspace = true
description = "Personal Wayland desktop shell built on hytte"

[lints]
workspace = true

[dependencies]
hytte = { path = "../crates/hytte" }
```

Create `trollshell/src/main.rs`:

```rust
fn main() {
    eprintln!("trollshell v0.1 — placeholder, see plan task 12");
}
```

- [ ] **Step 9: Verify the workspace builds**

Run: `cargo check --workspace`
Expected: all five crates compile clean (warnings allowed for unused code).

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml rust-toolchain.toml .gitignore crates/ trollshell/
git commit -m "feat: workspace scaffold for hytte + trollshell"
```

---

## Task 2: `hytte-reactive::runtime` — tokio runtime accessor

Provide a process-wide multi-thread `tokio::runtime::Runtime` initialized once on first access, plus a `handle()` accessor used by services to spawn their I/O tasks.

**Files:**

- Modify: `crates/hytte-reactive/Cargo.toml` (add `tokio`)
- Create: `crates/hytte-reactive/src/runtime.rs`
- Modify: `crates/hytte-reactive/src/lib.rs` (export `runtime`)
- Test: `crates/hytte-reactive/tests/runtime.rs`

- [ ] **Step 1: Add tokio dep**

Run: `cargo add -p hytte-reactive tokio --features rt-multi-thread,sync,net,io-util,time,macros`
Expected: `tokio` added to `crates/hytte-reactive/Cargo.toml`.

- [ ] **Step 2: Write the failing test**

Create `crates/hytte-reactive/tests/runtime.rs`:

```rust
use hytte_reactive::runtime;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn handle_spawns_tasks_on_a_background_thread() {
    let ran = Arc::new(AtomicBool::new(false));
    let ran_writer = ran.clone();

    runtime::handle().spawn(async move {
        ran_writer.store(true, Ordering::SeqCst);
    });

    // Give the runtime a moment to schedule and run the task.
    for _ in 0..100 {
        if ran.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("background task did not run within 1s");
}

#[test]
fn handle_is_stable_across_calls() {
    let h1 = runtime::handle();
    let h2 = runtime::handle();
    assert!(std::ptr::eq(h1, h2), "handle() should return the same Handle");
}
```

- [ ] **Step 3: Run the test, expect it to fail**

Run: `cargo test -p hytte-reactive --test runtime`
Expected: compile error — `runtime` module does not exist.

- [ ] **Step 4: Implement the runtime module**

Create `crates/hytte-reactive/src/runtime.rs`:

```rust
//! Process-wide multi-thread tokio runtime, initialized lazily on first
//! `handle()` call. Services use this `Handle` to spawn their I/O tasks.

use std::sync::OnceLock;
use tokio::runtime::{Handle, Runtime};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Returns a stable reference to the process-wide tokio runtime handle.
///
/// The runtime is built on first call. All subsequent calls return a handle
/// to the same runtime.
#[must_use]
pub fn handle() -> &'static Handle {
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("hytte-tokio")
                .build()
                .expect("failed to build hytte tokio runtime")
        })
        .handle()
}
```

- [ ] **Step 5: Re-export from `lib.rs`**

Replace `crates/hytte-reactive/src/lib.rs` with:

```rust
//! Bridge crate between GTK4's main loop and the `futures-signals` reactive
//! primitives, plus the hytte service registry. Service modules in
//! `hytte-services` register typed handles here at startup; widgets in
//! `hytte-ui` subscribe to them via `bind`.

pub mod runtime;
```

- [ ] **Step 6: Run the test, expect it to pass**

Run: `cargo test -p hytte-reactive --test runtime`
Expected: 2 tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/hytte-reactive
git commit -m "feat(reactive): tokio runtime accessor"
```

---

## Task 3: `hytte-reactive::registry` — Service trait + thread-local typed registry

Define the `Service` trait, a `Registry` (typed insert/get over `TypeId`), and a `REGISTRY` thread-local on the GTK main thread. Services register their handles here at startup; service free-functions read them out.

**Files:**

- Modify: `crates/hytte-reactive/Cargo.toml` (add `futures-signals`)
- Create: `crates/hytte-reactive/src/registry.rs`
- Modify: `crates/hytte-reactive/src/lib.rs`
- Test: `crates/hytte-reactive/tests/registry.rs`

- [ ] **Step 1: Add futures-signals dep**

Run: `cargo add -p hytte-reactive futures-signals`
Expected: `futures-signals` added.

- [ ] **Step 2: Write the failing test**

Create `crates/hytte-reactive/tests/registry.rs`:

```rust
use futures_signals::signal::{Mutable, SignalExt};
use hytte_reactive::registry::{self, Service};
use hytte_reactive::runtime;

struct ClockService;

#[derive(Default)]
struct ClockHandles {
    tick: Mutable<u32>,
}

impl Service for ClockService {
    type Handles = ClockHandles;
    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        ClockHandles::default()
    }
}

fn tick() -> impl futures_signals::signal::Signal<Item = u32> {
    registry::with(|r| {
        r.get::<ClockHandles>()
            .expect("ClockService not registered")
            .tick
            .signal()
    })
}

#[test]
fn registered_service_handles_round_trip() {
    registry::reset_for_tests();
    registry::install(Box::new(ClockService), runtime::handle());

    // Read the signal — should yield the default 0.
    let mut stream = tick().to_stream();
    futures_executor::block_on(async {
        let v = futures_util::StreamExt::next(&mut stream).await;
        assert_eq!(v, Some(0));
    });
}

#[test]
fn missing_service_panics_with_helpful_message() {
    registry::reset_for_tests();
    let panicked = std::panic::catch_unwind(|| {
        let _ = tick();
    });
    let msg = panicked
        .err()
        .and_then(|e| e.downcast_ref::<&str>().map(|s| (*s).to_string()).or_else(|| e.downcast_ref::<String>().cloned()))
        .unwrap_or_default();
    assert!(msg.contains("ClockService not registered"), "got: {msg}");
}
```

Add a `dev-dependencies` block to `crates/hytte-reactive/Cargo.toml`:

```bash
cargo add -p hytte-reactive --dev futures-executor futures-util
```

- [ ] **Step 3: Run the test, expect compile failure**

Run: `cargo test -p hytte-reactive --test registry`
Expected: compile error — `registry` module not found.

- [ ] **Step 4: Implement the registry**

Create `crates/hytte-reactive/src/registry.rs`:

```rust
//! Typed, thread-local registry of service handles.
//!
//! Each `Service` produces a `Handles` value at startup (typically a struct
//! of `Mutable<T>` / `MutableVec<T>` from `futures-signals`). Handles are
//! stored keyed by their concrete type. Service free-functions in
//! `hytte-services` retrieve them via [`with`].
//!
//! The registry lives in a `thread_local!` because GTK is single-threaded —
//! widgets only subscribe from the main thread. Cross-thread updates from
//! tokio tasks happen by writing to the `Mutable` (which is `Send + Sync`)
//! that the handle holds; the registry itself is never crossed thread
//! boundaries.

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;

/// A service that can be registered on an `App`.
pub trait Service: Sized + 'static {
    /// Bag of handles (typically `Mutable<T>` / `MutableVec<T>`) that
    /// widgets subscribe to.
    type Handles: 'static;

    /// Spawn background tasks on the supplied tokio handle and return the
    /// handle bag to be inserted in the registry.
    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles;
}

/// Type-erased shim used internally by `App` to store heterogeneous
/// services in a single `Vec`.
pub trait ServiceErased: 'static {
    fn start_erased(self: Box<Self>, rt: &tokio::runtime::Handle, registry: &mut Registry);
}

impl<S: Service> ServiceErased for S {
    fn start_erased(self: Box<Self>, rt: &tokio::runtime::Handle, registry: &mut Registry) {
        let handles = (*self).start(rt);
        registry.insert::<S::Handles>(handles);
    }
}

/// Storage for service handles, keyed by their concrete `TypeId`.
#[derive(Default)]
pub struct Registry {
    entries: HashMap<TypeId, Box<dyn Any>>,
}

impl Registry {
    pub fn insert<T: 'static>(&mut self, value: T) {
        self.entries.insert(TypeId::of::<T>(), Box::new(value));
    }

    pub fn get<T: 'static>(&self) -> Option<&T> {
        self.entries
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<T>())
    }
}

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
}

/// Run a closure with shared read access to the thread-local registry.
///
/// # Panics
/// Panics if called from a thread other than the one where services were
/// installed (typically the GTK main thread). In practice all subscriptions
/// happen on the main thread.
pub fn with<R>(f: impl FnOnce(&Registry) -> R) -> R {
    REGISTRY.with(|cell| f(&cell.borrow()))
}

/// Install a single service. Called by `App::run` once per registered
/// service before invoking the consumer's body closure.
pub fn install(service: Box<dyn ServiceErased>, rt: &tokio::runtime::Handle) {
    REGISTRY.with(|cell| {
        let mut reg = cell.borrow_mut();
        service.start_erased(rt, &mut reg);
    });
}

/// Wipe the registry — exposed for tests only.
#[doc(hidden)]
pub fn reset_for_tests() {
    REGISTRY.with(|cell| *cell.borrow_mut() = Registry::default());
}
```

- [ ] **Step 5: Re-export from `lib.rs`**

Replace `crates/hytte-reactive/src/lib.rs` with:

```rust
//! Bridge crate between GTK4's main loop and the `futures-signals` reactive
//! primitives, plus the hytte service registry. Service modules in
//! `hytte-services` register typed handles here at startup; widgets in
//! `hytte-ui` subscribe to them via `bind`.

pub mod registry;
pub mod runtime;

pub use registry::{Registry, Service, ServiceErased};
```

- [ ] **Step 6: Run the test**

Run: `cargo test -p hytte-reactive --test registry`
Expected: 2 tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/hytte-reactive
git commit -m "feat(reactive): Service trait + thread-local typed registry"
```

---

## Task 4: `hytte-reactive::bind` — signal → widget bridge

Add `bind`, `bind_text`, `bind_visible`, and `bind_class`. Each spawns a future on the GTK main loop via `glib::MainContext::spawn_local` that consumes the signal and applies updates to the widget.

**Files:**

- Modify: `crates/hytte-reactive/Cargo.toml` (add `gtk4` deps)
- Create: `crates/hytte-reactive/src/bind.rs`
- Modify: `crates/hytte-reactive/src/lib.rs`
- Test: `crates/hytte-reactive/tests/bind.rs`

- [ ] **Step 1: Add gtk4 dep**

Run: `cargo add -p hytte-reactive gtk4 --rename gtk`
Expected: `gtk = { package = "gtk4", … }` added.

- [ ] **Step 2: Write the failing test**

Create `crates/hytte-reactive/tests/bind.rs`:

```rust
//! Integration test: drive a `Mutable<String>` from the GTK main loop and
//! assert the bound `gtk::Label`'s text follows.
//!
//! Requires a display server. Run with `xvfb-run cargo test -p hytte-reactive
//! --test bind -- --ignored` or under an existing X/Wayland session.

use futures_signals::signal::{Mutable, SignalExt};
use gtk::glib;
use gtk::prelude::*;
use hytte_reactive::bind::{bind_text, bind_visible};
use std::time::Duration;

fn run_briefly(ms: u64) {
    let ctx = glib::MainContext::default();
    let deadline = std::time::Instant::now() + Duration::from_millis(ms);
    while std::time::Instant::now() < deadline {
        ctx.iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore = "requires a display server"]
fn bind_text_follows_mutable_updates() {
    gtk::init().expect("gtk init");
    let label = gtk::Label::new(None);
    let m = Mutable::new(String::from("hello"));

    bind_text(m.signal_cloned(), &label);
    run_briefly(50);
    assert_eq!(label.text().as_str(), "hello");

    m.set(String::from("world"));
    run_briefly(50);
    assert_eq!(label.text().as_str(), "world");
}

#[test]
#[ignore = "requires a display server"]
fn bind_visible_toggles_widget() {
    gtk::init().expect("gtk init");
    let label = gtk::Label::new(Some("x"));
    let m = Mutable::new(false);

    bind_visible(m.signal(), &label);
    run_briefly(50);
    assert!(!label.is_visible());

    m.set(true);
    run_briefly(50);
    assert!(label.is_visible());
}
```

- [ ] **Step 3: Run, expect compile failure**

Run: `cargo test -p hytte-reactive --test bind`
Expected: compile error — `bind` module not found.

- [ ] **Step 4: Implement bind**

Create `crates/hytte-reactive/src/bind.rs`:

```rust
//! Helpers that spawn a per-binding future on `glib::MainContext`, drive
//! a `Signal` to completion, and apply each emitted value to a GTK widget
//! on the main thread.

use futures_signals::signal::{Signal, SignalExt};
use gtk::glib;
use gtk::prelude::*;

/// Spawn a future on the GTK main loop that drives `signal` and applies
/// each emitted value to `widget` via `apply`.
///
/// The widget is cloned (cheap — GTK widgets are reference-counted). The
/// future lives as long as the underlying signal source; widget cleanup
/// drops the closure when the widget is collected and the next emission
/// observes a no-op.
pub fn bind<S, W, F>(signal: S, widget: &W, apply: F)
where
    S: Signal + 'static,
    S::Item: 'static,
    W: IsA<gtk::Widget> + Clone + 'static,
    F: Fn(&W, S::Item) + 'static,
{
    let widget = widget.clone();
    glib::MainContext::default().spawn_local(async move {
        signal
            .for_each(move |value| {
                apply(&widget, value);
                std::future::ready(())
            })
            .await;
    });
}

/// Bind a string-producing signal to a `gtk::Label`'s text.
pub fn bind_text<S>(signal: S, label: &gtk::Label)
where
    S: Signal + 'static,
    S::Item: AsRef<str> + 'static,
{
    bind(signal, label, |w, v| w.set_text(v.as_ref()));
}

/// Bind a bool signal to a widget's `visible` property.
pub fn bind_visible<S>(signal: S, widget: &impl IsA<gtk::Widget>)
where
    S: Signal<Item = bool> + 'static,
{
    bind(signal, &widget.clone().upcast::<gtk::Widget>(), |w, v| {
        w.set_visible(v);
    });
}

/// Bind a bool signal to whether `class` is present on the widget.
pub fn bind_class<S>(signal: S, widget: &impl IsA<gtk::Widget>, class: &str)
where
    S: Signal<Item = bool> + 'static,
{
    let class = class.to_owned();
    bind(
        signal,
        &widget.clone().upcast::<gtk::Widget>(),
        move |w, v| {
            if v {
                w.add_css_class(&class);
            } else {
                w.remove_css_class(&class);
            }
        },
    );
}
```

- [ ] **Step 5: Re-export**

Replace `crates/hytte-reactive/src/lib.rs` with:

```rust
//! Bridge crate between GTK4's main loop and the `futures-signals` reactive
//! primitives, plus the hytte service registry.

pub mod bind;
pub mod registry;
pub mod runtime;

pub use bind::{bind, bind_class, bind_text, bind_visible};
pub use registry::{Registry, Service, ServiceErased};
```

- [ ] **Step 6: Run the test under xvfb**

Run: `xvfb-run -a cargo test -p hytte-reactive --test bind -- --ignored`
Expected: 2 tests pass.

If `xvfb-run` not available, run inside an existing X/Wayland session:
Run: `cargo test -p hytte-reactive --test bind -- --ignored`

- [ ] **Step 7: Commit**

```bash
git add crates/hytte-reactive
git commit -m "feat(reactive): bind, bind_text, bind_visible, bind_class"
```

---

## Task 5: `hytte-ui::error` + `hytte-ui::monitor` + `hytte-ui::app` skeleton

Stand up the `App`/`AppBuilder` builder pattern, a `Monitor` newtype, and a `hytte_ui::Result`/`Error`. App holds an `adw::Application` and on activate: starts services, loads the (still-empty) default CSS, calls the consumer body once.

**Files:**

- Modify: `crates/hytte-ui/Cargo.toml` (add gtk4, libadwaita, gio)
- Create: `crates/hytte-ui/src/error.rs`
- Create: `crates/hytte-ui/src/monitor.rs`
- Create: `crates/hytte-ui/src/app.rs`
- Modify: `crates/hytte-ui/src/lib.rs`
- Test: `crates/hytte-ui/tests/app_smoke.rs`

- [ ] **Step 1: Add deps**

Run: `cargo add -p hytte-ui gtk4 --rename gtk`
Run: `cargo add -p hytte-ui libadwaita --rename adw`
Run: `cargo add -p hytte-ui gio --no-default-features`

(`gio` is re-exported by `gtk4` but pulling it directly clarifies intent in `flags()` calls.)

- [ ] **Step 2: Write the smoke test**

Create `crates/hytte-ui/tests/app_smoke.rs`:

```rust
//! Smoke: build an `App`, register no services, and assert that the body
//! closure runs and that we can enumerate at least one monitor.
//!
//! Requires a display server.

use hytte_ui::App;
use std::cell::Cell;
use std::rc::Rc;

#[test]
#[ignore = "requires a display server"]
fn body_runs_on_activate() {
    let ran = Rc::new(Cell::new(false));
    let ran_writer = ran.clone();

    App::new("mov.vibec0re.hytte.test")
        .run(move |app| {
            ran_writer.set(true);
            // Don't crash even if there are no monitors (CI/headless edge).
            let _ = app.monitors();
            // Stop the app loop immediately so the test exits.
            app.quit();
        })
        .expect("run");

    assert!(ran.get(), "body closure did not run");
}
```

- [ ] **Step 3: Run, expect compile failure**

Run: `cargo test -p hytte-ui --test app_smoke`
Expected: compile error — `App` not found.

- [ ] **Step 4: Implement `error.rs`**

Create `crates/hytte-ui/src/error.rs`:

```rust
//! Error and Result aliases for `hytte-ui`.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// `gtk::init` / `adw::init` failed.
    GtkInit(gtk::glib::BoolError),
    /// `gio::Application::run` exited with a non-zero status.
    NonZeroExit(i32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GtkInit(e) => write!(f, "gtk init failed: {e}"),
            Self::NonZeroExit(code) => write!(f, "application exited with status {code}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
```

- [ ] **Step 5: Implement `monitor.rs`**

Create `crates/hytte-ui/src/monitor.rs`:

```rust
//! Thin wrapper around `gdk::Monitor` carrying just the metadata bars need.

use gtk::gdk;
use gtk::prelude::*;

#[derive(Clone, Debug)]
pub struct Monitor {
    inner: gdk::Monitor,
}

impl Monitor {
    pub(crate) fn new(inner: gdk::Monitor) -> Self {
        Self { inner }
    }

    /// Connector name (e.g. `"DP-1"`, `"eDP-1"`). May be empty on some
    /// drivers; callers should fall back to `model()` or `description()`.
    #[must_use]
    pub fn connector(&self) -> Option<String> {
        self.inner.connector().map(|s| s.to_string())
    }

    /// Free-form description (manufacturer + model).
    #[must_use]
    pub fn description(&self) -> Option<String> {
        self.inner.description().map(|s| s.to_string())
    }

    /// Width and height in logical pixels.
    #[must_use]
    pub fn size(&self) -> (i32, i32) {
        let g = self.inner.geometry();
        (g.width(), g.height())
    }

    /// Underlying `gdk::Monitor` for direct GTK calls (e.g. layer-shell).
    #[must_use]
    pub fn gdk(&self) -> &gdk::Monitor {
        &self.inner
    }
}
```

- [ ] **Step 6: Implement `app.rs`**

Create `crates/hytte-ui/src/app.rs`:

```rust
//! `App` and `AppBuilder` — the entry point for a hytte-based shell.
//!
//! The builder collects registered services and a one-shot body closure.
//! `run` constructs an `adw::Application`, connects an `activate` handler
//! that starts each service, installs the default stylesheet, and calls
//! the body once with an `&App` view.

use crate::error::{Error, Result};
use crate::monitor::Monitor;
use adw::prelude::*;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use hytte_reactive::registry::{self, ServiceErased};
use hytte_reactive::runtime;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Builder for an `App`. Registers services and an optional user CSS file
/// before `run` is called.
pub struct AppBuilder {
    app_id: String,
    services: Vec<Box<dyn ServiceErased>>,
    user_style: Option<PathBuf>,
}

impl AppBuilder {
    #[must_use]
    pub fn with<S: hytte_reactive::Service>(mut self, service: S) -> Self {
        self.services.push(Box::new(service));
        self
    }

    #[must_use]
    pub fn with_user_style(mut self, path: impl AsRef<Path>) -> Self {
        self.user_style = Some(path.as_ref().to_path_buf());
        self
    }

    /// Run the application. The body closure is invoked once on first
    /// activate; subsequent activates are no-ops.
    ///
    /// # Errors
    /// Returns `Error::NonZeroExit` if the GTK application exits with a
    /// non-zero status.
    pub fn run<F>(self, body: F) -> Result<()>
    where
        F: FnOnce(&App) + 'static,
    {
        adw::init().map_err(Error::GtkInit)?;

        let inner = adw::Application::builder()
            .application_id(&self.app_id)
            .flags(gio::ApplicationFlags::default())
            .build();

        // Wrap the move-once state in `Rc<RefCell<Option<…>>>` so the
        // activate handler can `.take()` it on first fire.
        let body_cell: Rc<RefCell<Option<Box<dyn FnOnce(&App)>>>> =
            Rc::new(RefCell::new(Some(Box::new(body))));
        let services_cell: Rc<RefCell<Option<Vec<Box<dyn ServiceErased>>>>> =
            Rc::new(RefCell::new(Some(self.services)));
        let user_style = self.user_style;

        inner.connect_activate(move |inner_app| {
            // Hold the application alive without a regular toplevel.
            inner_app.hold();

            let Some(body_fn) = body_cell.borrow_mut().take() else {
                return;
            };
            let services = services_cell.borrow_mut().take().unwrap_or_default();

            install_default_css();
            if let Some(path) = user_style.as_ref() {
                install_user_css(path);
            }

            for service in services {
                registry::install(service, runtime::handle());
            }

            let app = App {
                inner: inner_app.clone(),
            };
            body_fn(&app);
        });

        let exit_code = inner.run().value();
        if exit_code == 0 {
            Ok(())
        } else {
            Err(Error::NonZeroExit(exit_code))
        }
    }
}

/// Live view of the running `adw::Application`. Handed to the consumer
/// body closure.
pub struct App {
    inner: adw::Application,
}

impl App {
    #[must_use]
    pub fn new(app_id: &str) -> AppBuilder {
        AppBuilder {
            app_id: app_id.to_owned(),
            services: Vec::new(),
            user_style: None,
        }
    }

    /// Snapshot of the currently connected monitors.
    #[must_use]
    pub fn monitors(&self) -> Vec<Monitor> {
        let Some(display) = gdk::Display::default() else {
            return Vec::new();
        };
        let model = display.monitors();
        let mut out = Vec::with_capacity(model.n_items() as usize);
        for i in 0..model.n_items() {
            if let Some(obj) = model.item(i) {
                if let Ok(monitor) = obj.downcast::<gdk::Monitor>() {
                    out.push(Monitor::new(monitor));
                }
            }
        }
        out
    }

    /// Underlying `adw::Application`, exposed for advanced use.
    #[must_use]
    pub fn inner(&self) -> &adw::Application {
        &self.inner
    }

    /// Quit the main loop. Useful from tests.
    pub fn quit(&self) {
        self.inner.quit();
    }
}

fn install_default_css() {
    // Filled in by Task 8; placeholder here keeps the symbol present.
    let provider = gtk::CssProvider::new();
    provider.load_from_data(crate::DEFAULT_STYLESHEET);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn install_user_css(path: &Path) {
    let provider = gtk::CssProvider::new();
    provider.load_from_path(path);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_USER,
        );
    }
}
```

- [ ] **Step 7: Update `lib.rs`**

Replace `crates/hytte-ui/src/lib.rs` with:

```rust
//! GTK4 + libadwaita + gtk4-layer-shell window primitives. Provides the
//! `App` entry point and (filled in by later tasks) `Bar` / `LayerWindow`
//! builders.

mod app;
mod error;
mod monitor;

pub use app::{App, AppBuilder};
pub use error::{Error, Result};
pub use monitor::Monitor;

/// Default stylesheet, replaced with real content in Task 8.
pub(crate) const DEFAULT_STYLESHEET: &str = "";
```

- [ ] **Step 8: Run the smoke test**

Run: `xvfb-run -a cargo test -p hytte-ui --test app_smoke -- --ignored`
Expected: 1 test passes.

- [ ] **Step 9: Commit**

```bash
git add crates/hytte-ui
git commit -m "feat(ui): App, AppBuilder, Monitor, error types"
```

---

## Task 6: `hytte-ui::layer_window` — raw layer-shell primitive

Wrap `gtk4-layer-shell` into a `LayerWindow` builder that produces a `gtk::Window` with layer-shell already configured. `Bar` will be built on top of this in the next task.

**Files:**

- Modify: `crates/hytte-ui/Cargo.toml` (add gtk4-layer-shell)
- Create: `crates/hytte-ui/src/layer_window.rs`
- Modify: `crates/hytte-ui/src/lib.rs`

- [ ] **Step 1: Add the layer-shell dep**

Run: `cargo add -p hytte-ui gtk4-layer-shell`
Expected: dep added.

- [ ] **Step 2: Implement `layer_window.rs`**

Create `crates/hytte-ui/src/layer_window.rs`:

```rust
//! Thin wrapper around `gtk4-layer-shell` that yields a configured
//! `gtk::Window` ready to host shell content.
//!
//! `Bar` (next module) is layered on top of this. Consumers wanting a
//! non-`Bar` layer surface (e.g. an OSD or a wallpaper) can use
//! `LayerWindow` directly.

use crate::Monitor;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge as LsEdge, Layer, LayerShell};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Anchor {
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Margin {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

pub struct LayerWindowBuilder {
    monitor: Monitor,
    layer: Layer,
    anchors: Vec<Anchor>,
    margin: Margin,
    namespace: String,
    exclusive: bool,
}

impl LayerWindowBuilder {
    #[must_use]
    pub fn anchor(mut self, edge: Anchor) -> Self {
        self.anchors.push(edge);
        self
    }

    #[must_use]
    pub fn margin(mut self, m: Margin) -> Self {
        self.margin = m;
        self
    }

    #[must_use]
    pub fn layer(mut self, layer: Layer) -> Self {
        self.layer = layer;
        self
    }

    #[must_use]
    pub fn namespace(mut self, ns: impl Into<String>) -> Self {
        self.namespace = ns.into();
        self
    }

    #[must_use]
    pub fn exclusive(mut self, on: bool) -> Self {
        self.exclusive = on;
        self
    }

    /// Construct the `gtk::Window`, wire up layer-shell, but don't show.
    #[must_use]
    pub fn build(self) -> gtk::Window {
        let window = gtk::Window::new();
        window.init_layer_shell();
        window.set_layer(self.layer);
        window.set_namespace(&self.namespace);
        window.set_monitor(self.monitor.gdk());

        for anchor in &self.anchors {
            window.set_anchor(map_edge(*anchor), true);
        }

        window.set_margin(LsEdge::Top, self.margin.top);
        window.set_margin(LsEdge::Right, self.margin.right);
        window.set_margin(LsEdge::Bottom, self.margin.bottom);
        window.set_margin(LsEdge::Left, self.margin.left);

        if self.exclusive {
            window.auto_exclusive_zone_enable();
        }

        window
    }
}

#[must_use]
pub fn layer_window(monitor: &Monitor) -> LayerWindowBuilder {
    LayerWindowBuilder {
        monitor: monitor.clone(),
        layer: Layer::Top,
        anchors: Vec::new(),
        margin: Margin::default(),
        namespace: String::from("hytte"),
        exclusive: false,
    }
}

fn map_edge(a: Anchor) -> LsEdge {
    match a {
        Anchor::Top => LsEdge::Top,
        Anchor::Bottom => LsEdge::Bottom,
        Anchor::Left => LsEdge::Left,
        Anchor::Right => LsEdge::Right,
    }
}
```

- [ ] **Step 3: Update `lib.rs`**

Modify `crates/hytte-ui/src/lib.rs` — add the new module and re-export:

Replace contents with:

```rust
//! GTK4 + libadwaita + gtk4-layer-shell window primitives.

mod app;
mod error;
mod layer_window;
mod monitor;

pub use app::{App, AppBuilder};
pub use error::{Error, Result};
pub use layer_window::{layer_window, Anchor, LayerWindowBuilder, Margin};
pub use monitor::Monitor;

/// Default stylesheet, replaced with real content in Task 8.
pub(crate) const DEFAULT_STYLESHEET: &str = "";

// Re-export the layer-shell `Layer` enum so consumers can pick a layer
// without depending on `gtk4-layer-shell` directly.
pub use gtk4_layer_shell::Layer;
```

- [ ] **Step 4: Verify it builds**

Run: `cargo check -p hytte-ui`
Expected: clean build.

- [ ] **Step 5: Commit**

```bash
git add crates/hytte-ui
git commit -m "feat(ui): LayerWindow primitive over gtk4-layer-shell"
```

---

## Task 7: `hytte-ui::bar` — three-section Bar built on LayerWindow

A `Bar` is a layer-shell window anchored to a single monitor edge (Top by default), containing a `gtk::CenterBox` with left/center/right widget groups.

**Files:**

- Create: `crates/hytte-ui/src/bar.rs`
- Modify: `crates/hytte-ui/src/lib.rs`

- [ ] **Step 1: Implement `bar.rs`**

Create `crates/hytte-ui/src/bar.rs`:

```rust
//! `Bar` — a top/bottom/left/right layer-shell strip with three widget
//! groups (left/center/right). Built on `LayerWindow`.
//!
//! Returns a `BarHandle` which keeps the underlying window alive; dropping
//! it closes the bar.

use crate::layer_window::{layer_window, Anchor, Margin};
use crate::Monitor;
use gtk::prelude::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

pub struct Bar {
    monitor: Monitor,
    edge: Edge,
    margin: Margin,
    exclusive: bool,
    left: Vec<gtk::Widget>,
    center: Vec<gtk::Widget>,
    right: Vec<gtk::Widget>,
}

impl Bar {
    #[must_use]
    pub fn new(monitor: &Monitor) -> Self {
        Self {
            monitor: monitor.clone(),
            edge: Edge::Top,
            margin: Margin::default(),
            exclusive: true,
            left: Vec::new(),
            center: Vec::new(),
            right: Vec::new(),
        }
    }

    #[must_use]
    pub fn edge(mut self, edge: Edge) -> Self {
        self.edge = edge;
        self
    }

    #[must_use]
    pub fn margin(mut self, m: Margin) -> Self {
        self.margin = m;
        self
    }

    #[must_use]
    pub fn exclusive(mut self, on: bool) -> Self {
        self.exclusive = on;
        self
    }

    #[must_use]
    pub fn left(mut self, widgets: impl IntoIterator<Item = gtk::Widget>) -> Self {
        self.left.extend(widgets);
        self
    }

    #[must_use]
    pub fn center(mut self, widgets: impl IntoIterator<Item = gtk::Widget>) -> Self {
        self.center.extend(widgets);
        self
    }

    #[must_use]
    pub fn right(mut self, widgets: impl IntoIterator<Item = gtk::Widget>) -> Self {
        self.right.extend(widgets);
        self
    }

    /// Build the bar window, present it, and return a handle that keeps it
    /// alive. Dropping the handle closes the bar.
    #[must_use]
    pub fn show(self) -> BarHandle {
        let (anchor_main, anchor_perp) = perpendicular_anchors(self.edge);

        let window = layer_window(&self.monitor)
            .anchor(anchor_main)
            .anchor(anchor_perp.0)
            .anchor(anchor_perp.1)
            .margin(self.margin)
            .exclusive(self.exclusive)
            .namespace(format!("hytte-bar-{:?}", self.edge).to_lowercase())
            .build();
        window.add_css_class("hytte-bar");
        window.add_css_class(edge_class(self.edge));

        let center_box = gtk::CenterBox::new();
        center_box.add_css_class("hytte-bar-content");

        let left = group_box("hytte-bar-group-left");
        for w in self.left {
            left.append(&w);
        }
        let middle = group_box("hytte-bar-group-center");
        for w in self.center {
            middle.append(&w);
        }
        let right = group_box("hytte-bar-group-right");
        for w in self.right {
            right.append(&w);
        }

        center_box.set_start_widget(Some(&left));
        center_box.set_center_widget(Some(&middle));
        center_box.set_end_widget(Some(&right));

        window.set_child(Some(&center_box));
        window.present();

        BarHandle { window }
    }
}

fn group_box(class: &str) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    b.add_css_class(class);
    b
}

fn perpendicular_anchors(edge: Edge) -> (Anchor, (Anchor, Anchor)) {
    match edge {
        Edge::Top => (Anchor::Top, (Anchor::Left, Anchor::Right)),
        Edge::Bottom => (Anchor::Bottom, (Anchor::Left, Anchor::Right)),
        Edge::Left => (Anchor::Left, (Anchor::Top, Anchor::Bottom)),
        Edge::Right => (Anchor::Right, (Anchor::Top, Anchor::Bottom)),
    }
}

fn edge_class(edge: Edge) -> &'static str {
    match edge {
        Edge::Top => "hytte-bar-top",
        Edge::Bottom => "hytte-bar-bottom",
        Edge::Left => "hytte-bar-left",
        Edge::Right => "hytte-bar-right",
    }
}

/// Holds the bar's underlying window alive. Dropping closes the bar.
pub struct BarHandle {
    window: gtk::Window,
}

impl BarHandle {
    /// Close the bar immediately.
    pub fn close(self) {
        self.window.close();
    }
}

impl Drop for BarHandle {
    fn drop(&mut self) {
        self.window.close();
    }
}
```

- [ ] **Step 2: Update `lib.rs`**

Replace `crates/hytte-ui/src/lib.rs` with:

```rust
//! GTK4 + libadwaita + gtk4-layer-shell window primitives.

mod app;
mod bar;
mod error;
mod layer_window;
mod monitor;

pub use app::{App, AppBuilder};
pub use bar::{Bar, BarHandle, Edge};
pub use error::{Error, Result};
pub use layer_window::{layer_window, Anchor, LayerWindowBuilder, Margin};
pub use monitor::Monitor;

pub use gtk4_layer_shell::Layer;

/// Default stylesheet, replaced with real content in Task 8.
pub(crate) const DEFAULT_STYLESHEET: &str = "";
```

- [ ] **Step 3: Verify it builds**

Run: `cargo check -p hytte-ui`
Expected: clean build.

- [ ] **Step 4: Commit**

```bash
git add crates/hytte-ui
git commit -m "feat(ui): Bar with left/center/right groups over LayerWindow"
```

---

## Task 8: `hytte-ui` default stylesheet

Replace the placeholder `DEFAULT_STYLESHEET` constant with `include_str!("style.css")` and ship a tasteful default.

**Files:**

- Create: `crates/hytte-ui/src/style.css`
- Modify: `crates/hytte-ui/src/lib.rs`

- [ ] **Step 1: Write the stylesheet**

Create `crates/hytte-ui/src/style.css`:

```css
/* hytte default shell stylesheet. Override per-shell via
 * App::with_user_style(path). */

.hytte-bar {
  background: rgba(20, 20, 22, 0.86);
  color: #f5f5f7;
  border: none;
  box-shadow: 0 1px 0 rgba(255, 255, 255, 0.05);
  font:
    13px/1 "Inter",
    "Cantarell",
    system-ui,
    sans-serif;
}

.hytte-bar-top {
  border-bottom: 1px solid rgba(255, 255, 255, 0.06);
}
.hytte-bar-bottom {
  border-top: 1px solid rgba(255, 255, 255, 0.06);
}
.hytte-bar-left {
  border-right: 1px solid rgba(255, 255, 255, 0.06);
}
.hytte-bar-right {
  border-left: 1px solid rgba(255, 255, 255, 0.06);
}

.hytte-bar-content {
  padding: 0 12px;
  min-height: 30px;
}

.hytte-bar-group-left,
.hytte-bar-group-center,
.hytte-bar-group-right {
  padding: 0 4px;
}

.hytte-bar-group-left button,
.hytte-bar-group-center button,
.hytte-bar-group-right button {
  background: transparent;
  border: none;
  border-radius: 6px;
  padding: 2px 8px;
  color: inherit;
  box-shadow: none;
}

.hytte-bar-group-left button:hover,
.hytte-bar-group-center button:hover,
.hytte-bar-group-right button:hover {
  background: rgba(255, 255, 255, 0.06);
}

.hytte-bar-group-left button.active,
.hytte-bar-group-center button.active,
.hytte-bar-group-right button.active {
  background: rgba(255, 255, 255, 0.1);
}
```

- [ ] **Step 2: Wire it into lib.rs**

In `crates/hytte-ui/src/lib.rs` replace the `DEFAULT_STYLESHEET` declaration line:

```rust
/// Default stylesheet, replaced with real content in Task 8.
pub(crate) const DEFAULT_STYLESHEET: &str = "";
```

with:

```rust
pub(crate) const DEFAULT_STYLESHEET: &str = include_str!("style.css");
```

- [ ] **Step 3: Build**

Run: `cargo check -p hytte-ui`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/hytte-ui
git commit -m "feat(ui): ship opinionated default shell stylesheet"
```

---

## Task 9: `hytte-services::clock` — wall-clock service

Tick once per second on the GTK main loop, push `chrono::DateTime<Local>` into a `Mutable`. No tokio task needed because `glib::timeout_add_seconds_local` lives on the main loop directly — but we still go through the `Service`/registry machinery so the API stays uniform with the network-y services.

**Files:**

- Modify: `crates/hytte-services/Cargo.toml` (add chrono, gtk4)
- Create: `crates/hytte-services/src/clock.rs`
- Modify: `crates/hytte-services/src/lib.rs`
- Test: `crates/hytte-services/tests/clock.rs`

- [ ] **Step 1: Add deps**

Run: `cargo add -p hytte-services chrono --no-default-features --features clock,std`
Run: `cargo add -p hytte-services gtk4 --rename gtk`
Run: `cargo add -p hytte-services futures-signals`
Run: `cargo add -p hytte-services tokio --features rt`

- [ ] **Step 2: Write the failing test**

Create `crates/hytte-services/tests/clock.rs`:

```rust
//! Sanity check: register the clock service, read its `now()` signal once,
//! assert we get a non-default `DateTime<Local>` (i.e. roughly "now").
//!
//! No display required — clock service relies only on glib timers, but
//! we still drive the main context manually here.

use chrono::Local;
use futures_signals::signal::SignalExt;
use gtk::glib;
use hytte_reactive::{registry, runtime};
use hytte_services::clock;
use std::time::Duration;

#[test]
fn now_emits_a_recent_timestamp() {
    glib::MainContext::default().with_thread_default(|| {
        registry::reset_for_tests();
        registry::install(Box::new(clock::ClockService), runtime::handle());

        let signal = clock::now();
        let mut stream = signal.to_stream();

        let ctx = glib::MainContext::default();
        let started = std::time::Instant::now();
        let mut got: Option<chrono::DateTime<Local>> = None;
        while started.elapsed() < Duration::from_millis(200) {
            ctx.iteration(false);
            if let Some(v) = futures_executor::block_on(async {
                use futures_util::StreamExt as _;
                tokio::time::timeout(Duration::from_millis(10), stream.next())
                    .await
                    .ok()
                    .flatten()
            }) {
                got = Some(v);
                break;
            }
        }
        let got = got.expect("clock signal never emitted");
        let drift = (Local::now() - got).num_seconds().abs();
        assert!(drift <= 1, "drift {drift}s");
    })
    .unwrap();
}
```

Add dev-deps:

```bash
cargo add -p hytte-services --dev futures-executor futures-util tokio --features tokio/macros,tokio/rt,tokio/time
```

- [ ] **Step 3: Run, expect compile failure**

Run: `cargo test -p hytte-services --test clock`
Expected: compile error — `clock` module not found.

- [ ] **Step 4: Implement the clock service**

Create `crates/hytte-services/src/clock.rs`:

```rust
//! Wall-clock service. Ticks a `Mutable<DateTime<Local>>` once per second
//! on the GTK main loop.

use chrono::{DateTime, Local};
use futures_signals::signal::{Mutable, Signal};
use gtk::glib;
use hytte_reactive::{registry, Service};
use std::time::Duration;

pub struct ClockService;

#[derive(Clone)]
pub(crate) struct ClockHandles {
    pub(crate) now: Mutable<DateTime<Local>>,
}

impl Service for ClockService {
    type Handles = ClockHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = ClockHandles {
            now: Mutable::new(Local::now()),
        };
        let writer = handles.now.clone();
        glib::timeout_add_local(Duration::from_secs(1), move || {
            writer.set(Local::now());
            glib::ControlFlow::Continue
        });
        handles
    }
}

#[must_use]
pub fn service() -> ClockService {
    ClockService
}

#[must_use]
pub fn now() -> impl Signal<Item = DateTime<Local>> {
    registry::with(|r| {
        r.get::<ClockHandles>()
            .expect("clock::service() not registered")
            .now
            .signal_cloned()
    })
}
```

- [ ] **Step 5: Update `lib.rs`**

Replace `crates/hytte-services/src/lib.rs` with:

```rust
//! Async clients to system daemons exposed as hytte services.

pub mod clock;
```

- [ ] **Step 6: Run the test**

Run: `cargo test -p hytte-services --test clock`
Expected: 1 test passes.

- [ ] **Step 7: Commit**

```bash
git add crates/hytte-services
git commit -m "feat(services): clock service ticking once per second"
```

---

## Task 10: `hytte-services::niri` — Niri compositor IPC

Connect to `$NIRI_SOCKET`, send `Request::EventStream`, push workspace and focused-window updates into `Mutable`s as events arrive. Reconnect with backoff on socket loss.

**Files:**

- Modify: `crates/hytte-services/Cargo.toml` (add niri-ipc, anyhow)
- Create: `crates/hytte-services/src/niri.rs`
- Modify: `crates/hytte-services/src/lib.rs`

This task does not have a unit test (would require a running Niri instance). Verification is via the Task 13 manual checklist.

- [ ] **Step 1: Add deps**

Run: `cargo add -p hytte-services niri-ipc anyhow tracing`

- [ ] **Step 2: Implement the niri service**

Create `crates/hytte-services/src/niri.rs`:

```rust
//! Niri compositor IPC client.
//!
//! Uses the synchronous `niri_ipc::socket::Socket` from a dedicated
//! `spawn_blocking` task on the tokio runtime. Niri's IPC is line-based
//! JSON over a unix socket; the `niri-ipc` crate handles the framing and
//! event deserialisation.
//!
//! On connection loss the loop sleeps 1s then reconnects.

use anyhow::{anyhow, Context, Result};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use niri_ipc::{socket::Socket, Event, Request, Window, Workspace};
use std::thread;
use std::time::Duration;

pub struct NiriService;

pub(crate) struct NiriHandles {
    pub(crate) workspaces: Mutable<Vec<Workspace>>,
    pub(crate) focused_window: Mutable<Option<Window>>,
}

impl Default for NiriHandles {
    fn default() -> Self {
        Self {
            workspaces: Mutable::new(Vec::new()),
            focused_window: Mutable::new(None),
        }
    }
}

impl Service for NiriService {
    type Handles = NiriHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = NiriHandles::default();
        let ws_writer = handles.workspaces.clone();
        let win_writer = handles.focused_window.clone();

        // niri-ipc Socket is sync; isolate it on a dedicated blocking thread.
        rt.spawn_blocking(move || loop {
            match listen_once(&ws_writer, &win_writer) {
                Ok(()) => {
                    tracing::warn!("niri event stream closed, reconnecting in 1s");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "niri ipc error, reconnecting in 1s");
                }
            }
            thread::sleep(Duration::from_secs(1));
        });

        handles
    }
}

fn listen_once(
    workspaces: &Mutable<Vec<Workspace>>,
    focused_window: &Mutable<Option<Window>>,
) -> Result<()> {
    let socket = Socket::connect().context("connect to NIRI_SOCKET")?;
    let (_reply, events) = socket
        .send(Request::EventStream)
        .map_err(|e| anyhow!("send EventStream request: {e}"))?;
    let events = events.ok_or_else(|| anyhow!("EventStream returned no event channel"))?;

    for event in events {
        let event = event.map_err(|e| anyhow!("read niri event: {e}"))?;
        apply_event(event, workspaces, focused_window);
    }
    Ok(())
}

fn apply_event(
    event: Event,
    workspaces: &Mutable<Vec<Workspace>>,
    focused_window: &Mutable<Option<Window>>,
) {
    match event {
        Event::WorkspacesChanged { workspaces: ws } => {
            workspaces.set(ws);
        }
        Event::WorkspaceActivated { id, focused } => {
            workspaces.lock_mut().iter_mut().for_each(|w| {
                if w.id == id {
                    w.is_active = true;
                    w.is_focused = focused;
                } else if focused {
                    w.is_focused = false;
                }
            });
        }
        Event::WindowsChanged { .. } | Event::WindowOpenedOrChanged { .. } => {
            // v0.1 only tracks focused window via WindowFocusChanged.
        }
        Event::WindowFocusChanged { id } => {
            // The IPC sends the new focused window's id (or None);
            // resolving the full Window is left to v0.2 once we cache the
            // window list. For v0.1 we expose just the id, packed into a
            // synthetic `Window` with default fields.
            focused_window.set(id.map(|id| Window {
                id,
                ..Default::default()
            }));
        }
        _ => {}
    }
}

#[must_use]
pub fn service() -> NiriService {
    NiriService
}

#[must_use]
pub fn workspaces() -> impl Signal<Item = Vec<Workspace>> {
    registry::with(|r| {
        r.get::<NiriHandles>()
            .expect("niri::service() not registered")
            .workspaces
            .signal_cloned()
    })
}

#[must_use]
pub fn focused_window() -> impl Signal<Item = Option<Window>> {
    registry::with(|r| {
        r.get::<NiriHandles>()
            .expect("niri::service() not registered")
            .focused_window
            .signal_cloned()
    })
}
```

> **Niri-ipc API note for the engineer:** This file assumes
> `niri_ipc::socket::Socket::connect()`, `Socket::send(Request::EventStream)`
> returning `(Reply, Option<EventStream>)` where `EventStream: Iterator<Item = io::Result<Event>>`,
> and the `Event::*` variants used above. If the on-disk `niri-ipc`
> version differs (it may be 0.x and surface drift is real), adapt:
>
> - Reach the same shape (subscribe → iterate events → write to Mutable).
> - The `apply_event` mapping for `Workspace`/`Window` field names may
>   need tweaks (`is_active`, `is_focused`, `id` may be named slightly
>   differently). Confirm against `cargo doc -p niri-ipc --open`.

- [ ] **Step 3: Update `lib.rs`**

Replace `crates/hytte-services/src/lib.rs` with:

```rust
//! Async clients to system daemons exposed as hytte services.

pub mod clock;
pub mod niri;
```

- [ ] **Step 4: Verify it builds**

Run: `cargo check -p hytte-services`
Expected: clean build. If `niri-ipc` API differs from the assumptions in Step 2, adapt `niri.rs` until it builds — the only requirement is that `workspaces()` and `focused_window()` still return signals.

- [ ] **Step 5: Commit**

```bash
git add crates/hytte-services
git commit -m "feat(services): niri compositor ipc — workspaces, focused window"
```

---

## Task 11: `hytte` umbrella re-exports

Confirm the umbrella crate exports everything cleanly so consumers write `use hytte::{ui, reactive, services};`. The crate body is already stubbed in Task 1 — just verify it compiles after Tasks 2–10 add real content.

**Files:**

- Verify: `crates/hytte/src/lib.rs` already correct from Task 1.

- [ ] **Step 1: Build the umbrella**

Run: `cargo check -p hytte`
Expected: clean build.

- [ ] **Step 2: Add a `prelude` for ergonomic imports in `trollshell`**

Append to `crates/hytte/src/lib.rs`:

````rust

/// Convenience re-exports for shell binaries:
///
/// ```ignore
/// use hytte::prelude::*;
/// ```
pub mod prelude {
    pub use hytte_reactive::{bind, bind_class, bind_text, bind_visible, Service};
    pub use hytte_ui::{App, Anchor, Bar, BarHandle, Edge, Layer, Margin, Monitor};
}
````

- [ ] **Step 3: Build again**

Run: `cargo check -p hytte`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/hytte
git commit -m "feat(hytte): umbrella prelude for shell binaries"
```

---

## Task 12: `trollshell` binary — clock + workspaces widgets, top bar per monitor

Replace the placeholder `main` with the real shell: a top edge bar on every connected monitor, workspaces cluster on the left, clock on the right.

**Files:**

- Modify: `trollshell/Cargo.toml` (add chrono dep for the format helper)
- Create: `trollshell/src/widgets/mod.rs`
- Create: `trollshell/src/widgets/clock.rs`
- Create: `trollshell/src/widgets/workspaces.rs`
- Modify: `trollshell/src/main.rs`
- Create: `trollshell/style.css`

- [ ] **Step 1: Add chrono**

Run: `cargo add -p trollshell chrono --no-default-features --features clock,std`
Run: `cargo add -p trollshell futures-signals`

- [ ] **Step 2: User CSS overrides**

Create `trollshell/style.css`:

```css
/* trollshell user overrides on top of hytte-ui defaults. */

.trollshell-clock {
  font-feature-settings: "tnum" 1;
  padding: 0 6px;
}

.trollshell-workspaces button {
  min-width: 22px;
  padding: 2px 0;
}
.trollshell-workspaces button.focused {
  background: rgba(255, 255, 255, 0.18);
}
```

- [ ] **Step 3: Clock widget**

Create `trollshell/src/widgets/mod.rs`:

```rust
pub mod clock;
pub mod workspaces;
```

Create `trollshell/src/widgets/clock.rs`:

```rust
use futures_signals::signal::SignalExt;
use gtk::prelude::*;
use hytte::prelude::*;
use hytte::services::clock;

pub fn widget() -> gtk::Widget {
    let label = gtk::Label::new(None);
    label.add_css_class("trollshell-clock");
    bind_text(
        clock::now().map(|t| t.format("%a %H:%M").to_string()),
        &label,
    );
    label.upcast()
}
```

- [ ] **Step 4: Workspaces widget**

Create `trollshell/src/widgets/workspaces.rs`:

```rust
use futures_signals::signal::SignalExt;
use gtk::prelude::*;
use hytte::prelude::*;
use hytte::services::niri;

pub fn widget() -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    container.add_css_class("trollshell-workspaces");

    let container_for_signal = container.clone();
    bind(
        niri::workspaces(),
        &container,
        move |_, workspaces| {
            // Drop existing children.
            while let Some(child) = container_for_signal.first_child() {
                container_for_signal.remove(&child);
            }
            for ws in workspaces {
                let btn = gtk::Button::with_label(&ws.id.to_string());
                if ws.is_focused {
                    btn.add_css_class("focused");
                }
                if ws.is_active {
                    btn.add_css_class("active");
                }
                container_for_signal.append(&btn);
            }
        },
    );

    container.upcast()
}
```

- [ ] **Step 5: Wire it up in `main.rs`**

Replace `trollshell/src/main.rs` with:

```rust
mod widgets;

use hytte::prelude::*;
use hytte::services::{clock, niri};

fn main() -> hytte_ui::Result<()> {
    tracing_subscriber::fmt::init();

    App::new("mov.vibec0re.trollshell")
        .with(clock::service())
        .with(niri::service())
        .with_user_style(concat!(env!("CARGO_MANIFEST_DIR"), "/style.css"))
        .run(|app| {
            for monitor in app.monitors() {
                Bar::new(&monitor)
                    .edge(Edge::Top)
                    .exclusive(true)
                    .left([widgets::workspaces::widget()])
                    .right([widgets::clock::widget()])
                    .show()
                    // Leak the handle: bars live for the app lifetime.
                    .into_long_lived();
            }
        })
}
```

- [ ] **Step 6: Add `into_long_lived` helper to `BarHandle`**

`Bar::show()` returns a `BarHandle` whose `Drop` closes the bar. The shell wants bars alive forever, so add a deliberate leak helper.

Modify `crates/hytte-ui/src/bar.rs` — add this `impl` block after the existing `impl BarHandle`:

```rust
impl BarHandle {
    /// Forget the handle so the bar lives for the application's lifetime.
    /// Useful when constructing many bars in the body closure where you
    /// don't want to track each one individually.
    pub fn into_long_lived(self) {
        std::mem::forget(self);
    }
}
```

(Two `impl BarHandle` blocks side-by-side is legal Rust; merge into the existing block if preferred.)

- [ ] **Step 7: Add tracing-subscriber to trollshell**

Run: `cargo add -p trollshell tracing tracing-subscriber --features tracing-subscriber/fmt,tracing-subscriber/env-filter`

- [ ] **Step 8: Build**

Run: `cargo build -p trollshell`
Expected: clean build.

- [ ] **Step 9: Commit**

```bash
git add crates/hytte-ui trollshell
git commit -m "feat(trollshell): top bar with workspaces and clock"
```

---

## Task 13: README + manual verification on real Niri

Document how to build and run, list the manual smoke checklist that v0.1 has to pass.

**Files:**

- Create: `README.md`

- [ ] **Step 1: Write the README**

Create `README.md`:

````markdown
# trollshell + hytte

A library-first Rust toolkit (`hytte`) for composing GTK4 + libadwaita + layer-shell desktop shells, and `trollshell` — the personal shell built on it.

This repo holds the v0.1 milestone: a top-edge bar on every Niri monitor with workspaces (left) and a clock (right). See `docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md` for the full design.

## Build

```sh
cargo build --release -p trollshell
```
````

## Run (Niri only, v0.1)

```sh
cargo run --release -p trollshell
```

`trollshell` connects to `$NIRI_SOCKET` for compositor state. Make sure you're inside a Niri session.

## Repo layout

- `crates/hytte-reactive/` — `Service` trait, thread-local registry, tokio runtime accessor, `bind` helpers.
- `crates/hytte-ui/` — `App`, `Bar`, `LayerWindow` primitives + default shell stylesheet.
- `crates/hytte-services/` — `clock`, `niri` (more in v0.2+).
- `crates/hytte/` — umbrella re-export crate.
- `trollshell/` — the binary.

## Logs

```sh
RUST_LOG=hytte_services=debug,trollshell=debug cargo run -p trollshell
```

````

- [ ] **Step 2: Run the manual smoke checklist on real Niri**

(Engineer performs these on a Niri session and ticks each off.)

- [ ] Inside a Niri session, `cargo run --release -p trollshell` starts without panicking.
- [ ] A semi-transparent dark bar appears anchored to the top edge of every connected monitor.
- [ ] The clock on the right shows the current time in `Day HH:MM` format and ticks every minute (it actually updates each second internally, but format only shows minute precision).
- [ ] The workspaces cluster on the left lists Niri's workspaces by id.
- [ ] Switching workspaces in Niri (`niri msg action focus-workspace 2` etc.) updates the cluster — the focused workspace gets the `.focused` class (lighter background per `trollshell/style.css`).
- [ ] Plugging/unplugging an external monitor *does not* automatically add/remove a bar (this is expected for v0.1; hot-plug is wired in v0.2 via `App::monitors_changed`).
- [ ] Killing `trollshell` and re-running it returns the shell to the same state without lost data — Niri keeps running, the new shell reconnects.

- [ ] **Step 3: Commit README and any tweaks the smoke checklist surfaced**

```bash
git add README.md
git commit -m "docs: README + v0.1 manual smoke checklist"
````

---

## Self-Review

**Spec coverage:**

- Repo + crate layout — Task 1.
- Thread-local registry + tokio backend — Tasks 2, 3.
- bind / bind_text / bind_visible / bind_class — Task 4.
- App, Monitor, multi-monitor iteration — Tasks 5, 12.
- Bar, LayerWindow primitives — Tasks 6, 7.
- Default opinionated stylesheet — Task 8.
- `clock` service — Task 9.
- `niri` service (workspaces + focused window) — Task 10.
- Umbrella `hytte` crate + prelude — Task 11.
- `trollshell` binary delivering v0.1 milestone — Task 12.
- README + manual verification — Task 13.

**Out of scope for v0.1 (per the spec):** `Popup`/`Panel`, all v0.2/v0.3/v0.4 services. Not gaps, deferred.

**Type consistency check:**

- `ClockHandles` defined in Task 9 used only inside `hytte_services::clock`, not crossed against other tasks. ✓
- `NiriHandles` defined in Task 10 used only inside `hytte_services::niri`. ✓
- `Service::Handles` associated type referenced consistently in Tasks 3, 9, 10.
- `Bar::show() → BarHandle` (Task 7) → `BarHandle::into_long_lived` (Task 12). ✓
- `Edge` enum lives in `hytte_ui::bar` (Task 7), re-exported from `hytte` prelude (Task 11), used in trollshell (Task 12). ✓
- `bind`, `bind_text`, `bind_visible`, `bind_class` defined Task 4, re-exported via prelude Task 11, used Task 12. ✓
- `Anchor` (layer-shell-level edges) vs `Edge` (Bar-level edges) — distinct types living in distinct modules; mapped via `perpendicular_anchors` inside `bar.rs`. Intentional separation; documented implicitly by namespace. Acceptable.

**Placeholder scan:** No "TBD" / "implement later" / "appropriate error handling". The niri-ipc API note in Task 10 is a _labelled_ uncertainty about an external crate's surface — paired with a concrete fallback instruction ("adapt the apply_event mapping"), not a placeholder.
