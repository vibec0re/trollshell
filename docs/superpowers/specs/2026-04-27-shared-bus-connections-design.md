# `hytte-bus` — shared D-Bus capability layer

**Status:** design
**Date:** 2026-04-27
**Author:** Claude (with annika)
**Predecessors:** `2026-04-26-v0.2.6-robustness-design.md`, `2026-04-26-v0.2.8-lock-screen-design.md`.

## Goal

Eliminate the per-service D-Bus connection storm that exhausts `dbus-broker`'s file-descriptor budget after ~10–60 minutes of trollshell uptime, taking down every D-Bus client on the user session bus (dconf, gvfs, wireplumber, swaync, xdg-desktop-portal, GTK applications) along with trollshell itself.

The fix is a new `hytte-bus` crate that hides `zbus::Connection` behind a small set of capability primitives. All trollshell services share two long-lived `Connection`s (one per bus kind), supervised centrally with bounded backoff. Per-service code declares intent ("own this name", "subscribe to this signal", "call this method") and the bus layer handles connection management, reconnect, and permanent-failure detection uniformly.

## Symptom and root cause

**Symptom (BUGS.md, observed 2026-04-27):**
- After ~10 minutes of `cargo run -p trollshell`, the shell crashes.
- All GTK applications die simultaneously.
- `ps aux` shows `niri-session.target` shutting down.
- After restart, GNOME settings (accent color, etc.) are gone.

**Root cause** (confirmed via `journal.log` capture, 2026-04-27 10:27:08):

```
apr 27 10:27:08 blackforge dbus-broker-launch[71404]: ERROR sockopt_get_peerpidfd
    @ ../dbus-broker-37/src/util/sockopt.c +244: Too many open files
apr 27 10:27:08 blackforge systemd[71231]: Got disconnect on API bus.
apr 27 10:27:08 blackforge dconf-DEBUG: D-Bus connection closed,
    invalidating cache: Underlying GIOStream returned 0 bytes on an async read
```

Six trollshell services each maintain their own `Connection::session()` / `Connection::system()` inside flat-rate retry loops (`tokio::time::sleep(Duration::from_secs(2))` for `notifications` and `wifi`; `from_secs(5)` for `polkit`, `screensaver`, `power_profiles`, `bluetooth`). When their preconditions fail (mako already owns the Notifications name, iwd is not running, no logind session for the cargo-run process, etc.), each service opens a fresh `Connection` per retry. dbus-broker calls `getsockopt(SO_PEERPIDFD)` on every new peer to allocate a pidfd, consuming a file descriptor in the broker process; eventually the broker hits `EMFILE`, exits with status 1, and every D-Bus client on the session bus loses its connection at once. dconf was mid-cache-write, hence the GNOME-settings symptom.

The 10-minute timing is incidental — it is the time required for the leak to fill the broker's `LimitNOFILE` budget at ~30 connection attempts per minute. It does not correlate to swayidle's idle/suspend timers.

## Scope

### In scope

- New workspace member `crates/hytte-bus/` providing a process-wide shared connection per `BusKind` (Session, System) with a single supervisor task per bus.
- Five capability primitives (`own_name`, `signals`, `call`, `property`, `proxy`) that hide `zbus::Connection` from consumers and integrate with the futures-signals reactive layer already used throughout `hytte-services`.
- Migration of every D-Bus consumer in `crates/hytte-services/src/*.rs` to the new primitives, in the staged order documented under "Migration".
- Removal of the direct `zbus` dependency from `hytte-services/Cargo.toml` (transitive only via `hytte-bus`).
- Unit tests against an in-process ephemeral session bus for each primitive.

### Out of scope

- XML-driven proxy generation. zbus's `zbus_xml` / `zbus_macros` could replace hand-written proxy code, but each existing service has working proxy code and replacing it adds risk without solving the FD bug. Future iteration.
- Multi-process connection sharing. Each trollshell process opens its own pair of connections; this is fine because only one trollshell runs.
- A public `bus::supervisor_state()` API. Logging at `tracing::warn!` covers the current need; expose programmatically only when a real consumer requires it.
- A typed enum of every D-Bus error name. `BusError::Permanent { reason: String, dbus_name: Option<String> }` is honest about the open-ended wire.
- Replacement of the `Service` trait or the registry pattern in `hytte-reactive`. Bus primitives are consumed inside `Service::start()` impls; they do not change the registry.
- Decoupling `hytte-bus` from `hytte-reactive::runtime`. Every internal task uses `runtime::handle().spawn(...)`; threading a runtime argument through every builder is cost without benefit.
- Direct fix for "GNOME accent color disappears." This symptom is dconf losing its connection mid-write; fixing the FD storm fixes the cascade.
- Ergonomic shortcuts like `bus::own_session_name(...)`. The builder is short enough.
- Metrics (Prometheus counters, `bus::stats()`). `tracing` events are sufficient.

## Architecture

### Crate layout

```
crates/hytte-bus/
├── Cargo.toml          # deps: zbus, futures-signals, futures-util, tokio,
│                       #       thiserror, anyhow, hytte-reactive (for runtime::handle)
└── src/
    ├── lib.rs          # public re-exports
    ├── connection.rs   # SharedConnection: cached Connection per bus + reconnect supervisor
    ├── error.rs        # BusError (Transient { source } | Permanent { reason, dbus_name })
    ├── own.rs          # primitive #1: own_name + OwnState
    ├── signals.rs      # primitive #2: signals() builder + SignalSubscription
    ├── call.rs         # primitive #3: call() builder + send().await
    ├── property.rs     # primitive #4: property() builder + PropState
    └── proxy.rs        # primitive #5: BusProxy long-lived handle
```

Re-exported through the `hytte` umbrella as `hytte::bus`.

**Public surface:**

```rust
pub use hytte_bus::{
    BusError, BusKind,        // Session | System
    own_name, OwnState,
    signals, SignalSubscription, SignalEvent,
    call,
    property, PropState,
    proxy, BusProxy, ProxyState,
};
```

`SharedConnection` and the supervisor are private. Re-exporting `zbus::zvariant` types (e.g. `ObjectPath`, `Value`) is permitted where consumers need to construct typed args; that is data, not connection plumbing.

`hytte-services` keeps its existing module shape. Each service's `Service::start()` impl delegates D-Bus work to `hytte::bus::*` instead of holding `Connection`s. The `Service` trait, the Handles bag, and the public `pub fn` accessors (e.g. `notifications::active() -> impl Signal<...>`) are unchanged.

`hytte-services/Cargo.toml` adds `hytte-bus = { path = "../hytte-bus" }` and removes the direct `zbus` dependency (transitive only).

### Shared connection layer

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BusKind { Session, System }

pub(crate) struct SharedConnection {
    kind: BusKind,
    inner: Arc<tokio::sync::Mutex<Inner>>,
    epoch: Arc<AtomicU64>,           // bumped on every successful reconnect
    epoch_signal: Mutable<u64>,      // futures-signals view for primitives that subscribe
}

struct Inner {
    conn: Option<Connection>,         // None = currently broken/reconnecting
    backoff: Backoff,
}
```

Two process-wide singletons, one per `BusKind`, lazily initialized on first use, stored in `OnceCell<SharedConnection>` keyed by kind. Same shape as `runtime::handle()`. No registration in the service registry, no `bus::service()` call in `main.rs` — first use brings it up.

**Reconnect supervisor** (one per `SharedConnection`, spawned on first use):

1. Ensure `inner.conn` is `Some(_)`. If `None`, open `Connection::session()`/`system()`. On success, store, bump `epoch`, reset `backoff`, notify subscribers via `epoch_signal`. On failure, sleep `backoff.next()`, continue.
2. Wait for the connection's underlying socket to fail (subscribe to `NameOwnerChanged` on `org.freedesktop.DBus` and watch our own unique-name disappear; or observe `zbus::connection::executor()` task ending).
3. On disconnect: set `inner.conn = None`, bump `epoch`, sleep `backoff.next()`, loop to step 1.

**Backoff:** exponential, 250ms → 500ms → 1s → 2s → 4s → 8s → 16s → **30s cap**. Reset to 250ms on successful reconnect. The cap matters because feedback for "the bus is back" must be timely on a local D-Bus.

**Accessor used by primitives:**

```rust
pub(crate) async fn with_conn<F, R, Fut>(kind: BusKind, f: F) -> Result<R, BusError>
where F: FnOnce(Connection) -> Fut, Fut: Future<Output = Result<R, zbus::Error>>;
```

Pattern: grab the current `Connection` clone, call `f`, on `zbus::Error::Disconnected` (or the equivalent `Io`/`Failure` variants) mark the connection broken, signal the supervisor, return `BusError::Transient`. Other zbus errors map to `BusError::Permanent` or pass through.

**Why a supervisor and not lazy "open on demand":** lazy-on-demand is the current pattern and it caused the FD storm. The supervisor centralizes the "is the bus alive?" decision so trollshell never opens N concurrent connections when N services discover the bus is dead simultaneously. Only the supervisor calls `Connection::session()`; everyone else gets a clone or waits.

**Concurrency:** `Arc<tokio::sync::Mutex<Inner>>` (held across `.await`), `Mutable<u64>` for the epoch (cheap clone, lock-free reads). All primitives are async; nothing blocks the GTK thread.

### Primitive 1 — `own_name`

For `notifications`, `polkit` (auth agent), `screensaver` (`org.freedesktop.ScreenSaver`), `bluetooth` (pairing agent), `mpris`, and any future service that owns a well-known name and serves an interface.

```rust
pub fn own_name(name: impl Into<String>) -> OwnNameBuilder;

impl OwnNameBuilder {
    pub fn bus(self, kind: BusKind) -> Self;                  // default: Session
    pub fn at_path<I>(self, path: &str, iface: I) -> Self     // mount object before claiming;
        where I: zbus::interface::Interface + Send + 'static; //   callable many times → many objects
    pub fn replace_existing(self, yes: bool) -> Self;          // default: true
    pub fn permanent_after(self, n: u32) -> Self;              // default: 3 consecutive losses
                                                               //   to same owner → PermanentlyTaken
    pub fn start(self) -> impl Signal<Item = OwnState>;        // dropping all subscribers releases
}

pub enum OwnState {
    Acquiring,
    Owned,
    Lost { transient: bool, prev_owner: Option<String> },
    PermanentlyTaken { current_owner: String },
}
```

**Internal task (one per owned name):**

1. `epoch = current connection epoch`; `conn = SharedConnection.with_conn(kind).clone`.
2. Mount all objects via `.at(path, iface)`.
3. `RequestName(name, ReplaceExisting | DoNotQueue)`.
4. Observe `NameOwnerChanged` for this name.
5. Emit `OwnState::Owned`.
6. On owner-changed-away: emit `OwnState::Lost { prev_owner }`. Increment loss counter for that owner. If 3 consecutive losses to the same owner, emit `PermanentlyTaken { current_owner }`, sleep 5 min, reset counter. Otherwise exponential-backoff (250ms → 30s cap), goto step 2.
7. On bus epoch bump: drop proxy state, emit `Acquiring`, goto step 2.

**Why "3 consecutive losses to same owner" not "RequestName returned Exists":** because the bus layer uses `ReplaceExisting`, the first attempt usually succeeds. The actual mako-camping pattern is replace → mako auto-restart wins back → replace → mako wins back → … N times. After N, declare the name the owner's turf and back off to a 5-minute retry. `PermanentlyTaken` does not mean "never try again"; it means "stop hammering, retry rarely, surface state to the consumer."

### Primitive 2 — `signals`

For login1 Lock/Unlock subscriptions, ObjectManager `InterfacesAdded`/`Removed`, iwd watchers, networkd link state changes, upower property-change broadcasts, and any other "subscribe to signal X on object Y."

```rust
pub fn signals<'a>(destination: &'a str) -> SignalsBuilder<'a>;

impl<'a> SignalsBuilder<'a> {
    pub fn bus(self, kind: BusKind) -> Self;                      // default: System
    pub fn at_path(self, path: impl Into<ObjectPath<'a>>) -> Self;
    pub fn iface(self, name: &'a str) -> Self;
    pub fn signal(self, name: &'a str) -> Self;
    pub fn args(self, matcher: ArgMatcher) -> Self;               // optional cheap broker-side filter
    pub fn start(self) -> SignalSubscription;
}

pub struct SignalSubscription { … }

impl SignalSubscription {
    pub fn events(&self) -> impl Stream<Item = SignalEvent>;       // each emission, decoded as Message
    pub fn missed_emissions(&self) -> impl Signal<Item = u64>;     // bumped if bus was down
}

pub struct SignalEvent {
    pub body: zbus::Message,
    pub sender: Option<String>,
    pub timestamp: SystemTime,
}
```

**Internal task (one per subscription):**

1. `epoch = SharedConnection.epoch()`; build `Proxy::new(conn, dest, path, iface)`; obtain `proxy.receive_signal(signal)`.
2. Forward events to the consumer's channel.
3. On `stream.next() == None` or epoch change: bump `missed_emissions`, drop the proxy, await new epoch via `epoch_signal`, goto step 1.

**Why `missed_emissions` is exposed:** for state-bearing watchers (login1 Lock/Unlock, networkd link state), missing a signal during a reconnect window means the local view is stale. Consumers can watch `missed_emissions` and re-fetch authoritative state:

```rust
let sub = bus::signals("org.freedesktop.login1")
    .at_path(&session_path)
    .iface("org.freedesktop.login1.Session")
    .signal("Lock")
    .start();

glib::MainContext::default().spawn_local(sub.events().for_each(|_| {
    is_locked.set(true);
    std::future::ready(())
}));

glib::MainContext::default().spawn_local(sub.missed_emissions().for_each(|_| async {
    if let Ok(locked) = bus::call(LOGIN1, &session_path, IFACE, "GetLockedHint")
        .send::<bool>().await {
        is_locked.set(locked);
    }
}));
```

This is the correct pattern for "subscription that survives reconnects without lying about state." Today no service does this.

**`SignalsBuilder` accepts `'a` lifetimes** so common cases (string literals) do not allocate; `start()` consumes them and stores owned copies internally.

**Drop semantics:** dropping the `SignalSubscription` cancels the internal task and tears down the match rule (best-effort `RemoveMatch`). Cloning the handle is cheap (Arc) and does not cancel.

### Primitive 3 — `call`

For one-shot method calls. Today every such call opens its own `Connection::session()`/`system()`, makes the call, drops the connection. Single largest source of FD churn.

```rust
pub fn call<'a>(destination: &'a str) -> CallBuilder<'a>;

impl<'a> CallBuilder<'a> {
    pub fn bus(self, kind: BusKind) -> Self;                    // default: Session
    pub fn at_path(self, path: impl Into<ObjectPath<'a>>) -> Self;
    pub fn iface(self, name: &'a str) -> Self;
    pub fn method(self, name: &'a str) -> Self;
    pub fn args<A: Serialize + Type>(self, args: A) -> Self;
    pub fn timeout(self, dur: Duration) -> Self;                // default: 25s (D-Bus default)
    pub fn retry(self, on_transient: RetryPolicy) -> Self;      // default: Once

    pub async fn send<R: DeserializeOwned + Type>(self) -> Result<R, BusError>;
    pub fn fire_and_forget(self);                                // spawn on runtime, log on err
}

pub enum RetryPolicy {
    Never,                          // surface BusError::Transient immediately
    Once,                           // retry once if first attempt got Transient (bus reconnecting)
    Backoff { max_attempts: u32 },  // exponential backoff, then give up
}
```

**Why `Once` is the default:** the only legitimate transient on a local D-Bus is "bus was mid-reconnect." Letting `with_conn` await the supervisor's re-establish lets the second attempt see a fresh connection. More than one retry implies a deeper problem the consumer should know about.

**Internal flow:**

```
fn send():
    epoch_at_start = epoch
    result = SharedConnection.with_conn(kind, |conn| async {
        proxy.call(method, args).timeout(timeout).await
    })
    match (result, retry policy, epoch_changed):
        Ok                                 → return Ok
        Err(Transient), Once, fresh epoch  → retry once
        Err(Transient), Backoff            → exponential, capped
        Err(Permanent)                     → return immediately
```

**`fire_and_forget()`:** for sync contexts that don't care about the reply (button click handlers, swayidle SIGCONT, ActionInvoked emission). Spawns on `runtime::handle()`, logs `tracing::warn!` on error. Replaces the existing copy-pasted `runtime::handle().spawn(async move { if let Err(e) = … })` boilerplate.

### Primitive 4 — `property`

For upower battery percentage, networkd link state, power-profiles current profile, bluetooth adapter properties — any "give me current value as a Signal, keep it fresh, signal staleness during reconnect."

```rust
pub fn property<'a, T>(destination: &'a str) -> PropertyBuilder<'a, T>
where T: DeserializeOwned + Type + Clone + Send + 'static;

impl<'a, T> PropertyBuilder<'a, T> {
    pub fn bus(self, kind: BusKind) -> Self;
    pub fn at_path(self, path: impl Into<ObjectPath<'a>>) -> Self;
    pub fn iface(self, name: &'a str) -> Self;
    pub fn name(self, name: &'a str) -> Self;
    pub fn start(self) -> impl Signal<Item = PropState<T>>;
}

pub enum PropState<T> {
    Loading,            // initial, before first Get completes
    Loaded(T),          // current authoritative value
    Stale(T),           // last known value while bus is reconnecting / property gone
}
```

**Why three states (not `Option<T>`):** `Loading` and `Stale(T)` are different things to a UI. A bar widget rendering battery: `Loading` → skeleton; `Loaded(50%)` → render `50%`; `Stale(50%)` → render `50%` with a dimmed CSS class. Honest UI state.

**Internal task (one per property subscription):**

1. `epoch_at_start = epoch`; `conn = SharedConnection.with_conn(kind)`.
2. On cold start, emit `Loading`; on reconnect, emit `Stale(last)`.
3. `Properties.Get(iface, name)` → emit `Loaded(v)`.
4. Subscribe `PropertiesChanged` for this iface.
5. For each event: if `changed_properties` contains our name, emit `Loaded(new)`; if `invalidated_properties` contains our name, re-`Get`, emit `Loaded`.
6. On epoch bump: emit `Stale(last)`, goto step 2.
7. On `Get` failure: emit `Stale(last)` if available else stay `Loading`; backoff; retry from step 1.

**Coalescing:** if two properties on the same iface are subscribed (common — networkd "OperationalState" plus "AddressState"), the supervisor opens **one** `PropertiesChanged` subscription and fans out internally. Cache keyed by `(bus, dest, path, iface)`, lazy creation, dropped when the last subscriber goes away.

**`GetAll`-style "watch the whole bag"** is not implemented in v1. Add `bus::properties(...)` only when a real consumer needs it; today every widget watches one or two specific names.

### Primitive 5 — `proxy`

Escape hatch for "many calls against the same remote object." Real users: `tray` (StatusNotifierWatcher's per-item proxies), `mpris` (per-player proxies), `bluetooth` (per-device proxies).

```rust
pub fn proxy<'a>(destination: impl Into<Cow<'a, str>>) -> ProxyBuilder<'a>;

impl<'a> ProxyBuilder<'a> {
    pub fn bus(self, kind: BusKind) -> Self;
    pub fn at_path(self, path: impl Into<ObjectPath<'a>>) -> Self;
    pub fn iface(self, name: &'a str) -> Self;
    pub async fn build(self) -> Result<BusProxy, BusError>;   // initial connect; errors if peer unreachable
}

#[derive(Clone)]                    // cheap, Arc-backed; share freely
pub struct BusProxy { … }

impl BusProxy {
    pub async fn call<A, R>(&self, method: &str, args: A) -> Result<R, BusError>
        where A: Serialize + Type, R: DeserializeOwned + Type;

    pub fn fire_and_forget(&self, method: &str, args: impl …);

    pub async fn get_property<R: DeserializeOwned + Type>(&self, name: &str)
        -> Result<R, BusError>;

    pub async fn set_property<V: Serialize + Type>(&self, name: &str, val: V)
        -> Result<(), BusError>;

    pub fn signals(&self, name: &str) -> SignalSubscription;   // == bus::signals(...) scoped to this proxy

    pub fn liveness(&self) -> impl Signal<Item = ProxyState>;
}

pub enum ProxyState {
    Live,
    Reconnecting,        // bus is mid-reconnect; calls will queue or fail Transient
    PeerGone,            // remote peer (e.g. spotify) disappeared; distinct from bus down
}
```

**Internal:** `BusProxy` is `Arc<Inner>` where `Inner` holds `(BusKind, dest, path, iface)` plus a `tokio::sync::RwLock<Option<zbus::Proxy<'static>>>`. The supervisor's `epoch_signal` drives a small task per `BusProxy`:

1. On epoch bump, take the write lock, drop the old proxy, rebuild against the new connection. Emit `Reconnecting` → `Live`.
2. On `NameOwnerChanged`-away for the destination, emit `PeerGone` and stop trying.
3. On `NameOwnerChanged`-back for the destination, rebuild and emit `Live`.

**Why `ProxyState` distinguishes `PeerGone` from `Reconnecting`:** "Spotify quit" is a UI event (mpris widget hides). "Bus is briefly reconnecting" is invisible (widget keeps last state). Today `mpris` and `tray` muddle this; making it explicit cleans both up.

**Drop:** dropping the last `BusProxy` clone unsubscribes its `NameOwnerChanged` listener and stops the per-proxy task. Cheap clones let services hand `BusProxy` to widgets without lifecycle worry.

**Interplay with `bus::call`:** for one-shots, prefer `bus::call(...)` — do not spin up a `BusProxy` for a single invocation. Documented in module docs.

## Reconnect and permanent-failure semantics

### Reconnect contract

The `SharedConnection` supervisor is the authoritative source of "is the bus alive." Nothing else opens, watches, or replaces a `Connection`. Primitives observe via `epoch_signal`.

Behavior on bus reconnect (epoch bump), per primitive:

| Primitive | Behavior |
|---|---|
| `own_name` | emit `Acquiring`, re-mount objects, re-`RequestName`, on success emit `Owned` |
| `signals` | drop subscription, bump `missed_emissions`, re-subscribe — events between disconnect and re-subscribe are lost; the consumer is informed |
| `call` | retry policy applies (default `Once` retries against the new connection) |
| `property` | emit `Stale(last)`, re-`Get`, on success emit `Loaded(new)` |
| `proxy` | emit `Reconnecting`, rebuild proxy, on success emit `Live` |

**The "events are lost" rule for `signals` is non-negotiable:** D-Bus guarantees nothing about delivery during disconnect. The primitive cannot fake delivery. Consumers that need authoritative state across reconnects must watch `missed_emissions()` and re-fetch. Documented in the `signals` module docs.

**Backoff is owned by the supervisor.** A primitive never sleeps in a retry loop — it awaits the supervisor's epoch signal. This is the architectural property that prevents the FD storm: exactly one entity in the process opens connections, and it has bounded backoff.

**Connection-level vs. method errors:** the `with_conn` accessor inspects `zbus::Error` and routes:
- `Disconnected`, `Io`, `Failure(InputOutput*)` → `BusError::Transient` + signal supervisor to reconnect.
- `MethodError`, `InvalidReply`, `Unmarshal` → `BusError::Permanent` (or pass through).

This taxonomy lives in `error.rs` as the single mapping site.

### Permanent failure modes

Four recognized permanent-failure modes:

1. **`PermanentlyTaken`** (`own_name` only) — N consecutive `RequestName` losses to the same owner. Background retry every 5 min; consumer sees `OwnState::PermanentlyTaken { current_owner }`.
2. **`PeerGone`** (`proxy` only) — `NameOwnerChanged` says the destination's name has no owner. Distinct from bus disconnect; bus is fine, peer quit. Re-armed `NameOwnerChanged` subscription recovers when peer returns.
3. **`Permanent` `BusError` on a `call`** — the method itself returned an error. Bubbles to consumer as `Err(BusError::Permanent { reason, dbus_name })`. No retry, no backoff.
4. **No bus at all** — supervisor stays in connect loop with capped backoff. State-bearing primitives stay at their "no bus" variant (`OwnState::Acquiring`, `PropState::Loading`, `ProxyState::Reconnecting`); `SignalSubscription`'s `events()` stream simply produces nothing during the gap (and `missed_emissions` does not bump until the supervisor successfully reconnects and re-establishes the subscription, since "missed" is only knowable in retrospect).

**Explicitly not implemented:**
- Circuit-breaker beyond `PermanentlyTaken` for `own_name`. For `signals` and `property`, "remote object doesn't exist" loops forever — but with bounded backoff and **no FD allocation per attempt**, harmless.
- `BusError::Transient` exposure to the `call` consumer when `Once` retries succeeds. Consumer sees `Ok`. `Transient` is internal noise unless the consumer asks for `Never`.
- `bus::supervisor_state()` public signal in v1.

### Logging contract

- `tracing::debug!` — "subscribed", "released", "epoch bumped, re-establishing"
- `tracing::warn!` — "lost name to <owner>", "method call failed (Permanent)", "GetAll failed during cold start"
- `tracing::error!` — only the supervisor, only when `Connection::session/system` itself has been failing for >60s with the same error class

This kills the existing 2-second log spam. After: supervisor logs once at `warn` on disconnect, once at `info` on reconnect; per-primitive state changes log at `debug`. Quiet by default.

## Migration

### Ordering

Each phase is a separate commit cluster. Trollshell compiles, runs, and passes tests at every checkpoint.

**Phase 1 — `hytte-bus` crate scaffold** (no consumers): workspace member, `lib.rs`, `error.rs`, `connection.rs`. Unit tests against an in-process bus.

**Phase 2 — Five primitives implemented and tested, no consumers yet.** `own.rs`, `signals.rs`, `call.rs`, `property.rs`, `proxy.rs`. Each fully tested against an in-process zbus session.

**Phase 3 — Smoke-test migration: `resolved`.** Smallest, simplest D-Bus consumer — system bus only, no name ownership, no signal subscriptions, no property cache, one or two method calls against systemd-resolved. Exercises `bus::call(...)` end-to-end. If the API does not fit `resolved` cleanly, the design is wrong; better to find out in one file than five.

**Phase 4 — Loud offenders (the actual bug fix):**
- `notifications` (`own_name` + `signals` + `call`)
- `wifi` (`signals` + `call`)
- `polkit` (`own_name` + `signals` + `call`)
- `screensaver` (`own_name` + `signals` + `call`) — login1 + ScreenSaver iface
- `power_profiles` (`property` + `signals`)

Checkpoint: trollshell can run for 24h without `dbus-broker` `EMFILE`.

**Phase 5 — Remaining services:**
- `bluetooth` (`proxy` + `signals` + `property` + `own_name` for pairing agent)
- `mpris` (`proxy` + `signals`)
- `tray` (`own_name` + `proxy` fan-out)
- `networkd` (`signals` + `property`)
- `upower` (`property` + `signals`)
- `brightness` (`call` only)
- `systemd` (`call` + `signals`)

Checkpoint: zero `Connection::session()` / `Connection::system()` calls outside `hytte-bus`.

**Phase 6 — Cleanup:** remove `zbus` from `hytte-services/Cargo.toml`. Add a clippy `disallowed_methods` rule listing `zbus::Connection::session` / `system` to prevent regression. Done after Phase 5 has soaked for a week.

### Backwards compatibility

None required. `hytte` is pre-1.0. `hytte-services`'s public API is the `pub fn` accessors — those signatures **do not change**. The internal `Service::start()` impls change. The `Service` trait stays. Handles structs stay (some gain a new `daemon_state: Mutable<OwnState>` field, additive).

The one consumer-facing additive change: `notifications.rs`, `polkit.rs`, `screensaver.rs` etc. each gain a `pub fn daemon_state() -> impl Signal<Item = OwnState>` accessor so widgets can render "notifications: PermanentlyTaken (mako)" if they want. Old widgets keep working.

### Risk and rollback per phase

- **Phase 2:** primitive API may not fit a consumer we have not migrated yet. Mitigation: ordering puts the hardest consumer (`bluetooth`, all five primitives) in Phase 5; if its needs surface API changes, only Phase 5 PRs reshuffle.
- **Phase 4:** a migrated service silently regresses behavior (e.g. swayidle no longer paused on inhibit). Mitigation: each service's commit message includes a "before/after manual test" with an exact reproduction.
- **Rollback:** every phase is one commit cluster on its own branch; `git revert` is clean because nothing earlier depends on later phases. Phase 6 is the only irreversible one; defer until Phase 5 has soaked for a week.

## Testing

### In-process bus harness

`crates/hytte-bus/tests/common/mod.rs`:

```rust
pub async fn ephemeral_bus() -> (zbus::Connection, BusGuard) {
    // spawn dbus-daemon --session --print-address with a temp config that
    // disables the user policy, capture the address, set it as
    // DBUS_SESSION_BUS_ADDRESS for the duration of the test, return a guard
    // that kills the daemon on Drop.
}
```

Each test gets a fresh, isolated dbus-daemon (or `dbus-broker --address tmp`) so tests do not interfere and do not depend on the host's session bus. Same trick that systemd's own tests use.

`SharedConnection` exposes `pub(crate) fn for_test(addr: &str) -> Self` so tests can inject the ephemeral address; production code uses `Connection::session()`/`system()`.

### Per-primitive coverage

- **`own_name`:** acquire → owned, owner-stolen → `Lost` → re-acquired, three losses to same owner → `PermanentlyTaken`.
- **`signals`:** emission delivered, supervisor reconnect → `missed_emissions` bumped, late subscriber gets only future events.
- **`call`:** success, `MethodError` → `Permanent`, timeout, retry-`Once` across simulated reconnect.
- **`property`:** cold start → `Loading` → `Loaded`, `PropertiesChanged` → `Loaded`, supervisor reconnect → `Stale` → `Loaded`.
- **`proxy`:** liveness `Live`, peer `NameOwnerChanged` → `PeerGone`, peer back → `Live`, bus reconnect → `Reconnecting` → `Live`.

Reconnect tests force a disconnect by killing the dbus-daemon child mid-test and spinning up a replacement at the same address.

### Migrated service verification

Existing `crates/hytte-services/tests/clock.rs` is the precedent — it tests behavior, not internals. We do not add new D-Bus integration tests inside `hytte-services`; the bus primitives are already tested. Existing service tests remain unchanged. New service-level tests would be pure logic tests (e.g. "given an `OwnState::PermanentlyTaken` signal, the daemon_state mutable holds the right value") — additive.

### Soak verification (release gate, not CI)

The bug being fixed manifests over hours. Unit tests cannot prove the fix. End-to-end verification: 24h soak with `lsof -p $(pidof trollshell) | wc -l` monitored for stability and `dbus-broker` uptime monitored for restarts. Documented as a release gate in this design, not as a CI check.

## Appendix — illustrative migration: `notifications`

Today, the loop in `crates/hytte-services/src/notifications.rs:147-164`:

```rust
rt.spawn(async move {
    loop {
        match listen(&active_writer, &next_id, &history_writer).await {
            Ok(()) => tracing::warn!("notifications daemon stream closed, reconnecting in 2s"),
            Err(e) => tracing::warn!(error = %e, "notifications daemon error, reconnecting in 2s"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
});
```

After:

```rust
let state = bus::own_name("org.freedesktop.Notifications")
    .at_path("/org/freedesktop/Notifications", NotificationsIface { … })
    .start();

handles.daemon_state = state.clone();   // additive; UI can subscribe if it wants
```

The 51-minute disaster becomes ~3 connection attempts in the worst case. The `listen()` body is no longer needed — the interface methods are served directly by the `own_name` machinery; signal emissions (`NotificationClosed`, `ActionInvoked`) move from `do_dismiss`/`do_invoke_action` to `bus::call(...).fire_and_forget()`.
