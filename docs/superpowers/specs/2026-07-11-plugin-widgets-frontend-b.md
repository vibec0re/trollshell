# Plugin widgets (frontend B): out-of-process TEA plugins over a Unix socket

**Date:** 2026-07-11
**Status:** Approved (stamped on #35 / #195; supersedes the issue-thread strawman)
**Issues:** #35 (feature), #195 (design), #199 (reconciler, merged)

## Summary

Widgets can be shipped as **out-of-process plugins** instead of being compiled
into the shell. A plugin is an autonomous process that speaks
[The Elm Architecture](https://guide.elm-lang.org/architecture/) to the host
over a Unix socket: it holds its own state, and on every re-render it pushes a
declarative widget **tree** (plus a list of shell **effects**) to the host. The
host is a **stateless render target** — it reconciles the tree into real GTK
widgets (the #199 reconciler), brokers the effects against the plugin's granted
capabilities, and pushes back user events and a subscribed subset of shell
state. Nothing about a plugin is linked into `trollshell`; "installing" one is
enabling a systemd user unit.

This spec promotes the design that was hashed out and stamped across #35/#195 to
a real spec, and it is the source of truth for the first implementation crate,
`hytte-plugin-proto` (PR 1). It stays deliberately aligned with that crate: the
types below _are_ the wire types.

## Background — what was ruled out, and why B

The brainstorm on #35 fanned four options; three collapsed into one architecture
and one was killed outright:

- **`.so` / dlopen plugins — ruled out.** No stable Rust ABI, GTK4 isn't
  ABI-stable across minors, `unsafe_code = "forbid"` workspace-wide, and the
  reactive `Registry` is `thread_local!` keyed by `TypeId` (a loaded `.so`
  can't resolve the host's service `TypeId`s). Also an explicit design non-goal.
- **Embedded Wayland compositor / cross-process pixel-sharing — ruled out.**
  Wayland object ids are per-connection, so one client cannot name another's
  surface; the only working pixel-share (shared buffer / PipeWire into a
  `GtkPicture`) is display-only, not clickable. The instant a plugin needs input
  you are hand-rolling a nested compositor (the `dockland` approach Annika
  already rejected).
- **The three viable frontends are the same architecture** — a declarative
  widget-tree → GTK reconciler on the host — with three ways to feed it:
  config/scripts (**D**), a socket process (**B**), a WASM module (**A**). The
  reconciler is the shared foundation, built first and merged as **#199**.

Annika chose **B** ("half a react native"): a separate process emitting a
tree/diff over a socket. Subsequent calls on the thread refined it into the
model below.

## The model

```
host:   (subscribed-state snapshot | user event | effect result) ── msg ──▶ plugin
plugin: reduce(local, msg) -> local';  view(local') -> (tree, effects) ──▶ host
host:   reconcile tree → GTK;  broker effects (capability-gated);  hold no plugin state
```

Stamped properties:

- **Plugin = autonomous process; host = stateless render target.** The reducer's
  state lives _in the plugin_. The host never stores or round-trips it. A plugin
  crash loses only transient UI state; it restarts and re-derives. (This settles
  the earlier host-persisted-blob fork → plugin-held.)
- **Full-duplex, not request/response.** The plugin re-renders and pushes a
  frame **whenever it wants** — a host-state change, an internal timer, an
  external fetch completing. "trollshell should always accept new view state."
- **Two kinds of effect.** _Shell_ effects (open a page, set volume, run a
  command, a niri action) are returned to the host and capability-brokered — the
  plugin never touches D-Bus/niri directly. The plugin's **own** external I/O
  (fetching its service) it just does in-process; that is not an effect and is
  never round-tripped through the host.
- **State subset, full snapshot, no deltas.** The plugin declares
  `subscribes: [StateKey]`; the host serializes **only** those, and re-sends the
  **full** subscribed subset on any change (latest-wins) — not per-key deltas.
  (Annika: "keep simple. app gets full subscribed state partial.") This is the
  perf knob: no firehose of unrelated state.

## Topology (the stamped connection model)

The strawman had the host _spawn_ plugins and pass a socket path via env. The
**systemd** decision killed that handshake. The approved topology (Annika 👍 on
#195):

- The host **listens** on one well-known socket:
  `$XDG_RUNTIME_DIR/trollshell/plugin.sock`, **same-user only** (the runtime dir
  is `0700`; the host sets restrictive perms on the socket too).
- Plugins are **systemd user units** that **dial in** and self-identify by
  sending `Register { manifest }` as their first frame.
- **Supervision is systemd's job.** A crash is just a disconnect;
  `Restart=on-failure` reconnects and the plugin re-`Register`s. Backoff,
  liveness (`WatchdogSec` and/or the protocol `Ping`/`Pong`) live in the unit.
- **"Enable a plugin" = enable its unit.** Discovery is "who connected", not a
  plugin directory the host scans.
- **Gating:** the host forwards state and keeps a plugin's slot live only while
  its mount is visible (reuse the sidebar open-signal) — no serialization when
  the drawer is closed.

The units and socket wiring land in `etc/` alongside the other session units;
they are **not** in scope for the proto crate (PR 1) and are sketched for PR 2+.

## Transport & framing

- **Encoding: `MessagePack`** (`rmp-serde`) — compact, fast, serde-native,
  language-agnostic. JSON was rejected (unfit for the per-frame hot path);
  bincode was rejected (Rust-framing-specific, bad for non-Rust plugins). A
  zero-copy schema format (Cap'n Proto/FlatBuffers) is the later escape hatch if
  a profile ever demands it.
- **Framing: length-prefixed.** Each frame is a 4-byte big-endian body length
  followed by that many `MessagePack` bytes. `hytte-plugin-proto` provides
  `encode`/`decode` (whole-frame) and, behind an optional `tokio` feature,
  `read_frame`/`write_frame` for streaming. A read-side `MAX_FRAME_LEN` (16 MiB)
  bounds memory against a hostile/buggy peer.

## Node vocabulary (frozen, from #199)

The closed 7-node set the reconciler already renders, mirrored GTK-free in
`wire::Node` (`Box` carries `scroll` explicitly, so `wire::Node → hytte_ui::Node`
is a trivial 1:1 the host does in PR 2):

```rust
enum Node {
    Box      { id: Option<NodeId>, dir: Dir, spacing: i32, scroll: bool, classes: Vec<Cls>, children: Vec<Node> },
    Label    { id: Option<NodeId>, text: String, classes: Vec<Cls> },
    Icon     { id: Option<NodeId>, name: String, classes: Vec<Cls> },   // themed icon NAME only, never pixels
    Button   { id: NodeId, classes: Vec<Cls>, child: Box<Node> },       // id required — the click target
    Progress { id: Option<NodeId>, fraction: f64, classes: Vec<Cls> },
    Revealer { id: Option<NodeId>, open: bool, child: Box<Node> },
    Separator{ classes: Vec<Cls> },
}
enum Dir { Horizontal, Vertical }
enum EventKind { Click, Scroll { dx: f64, dy: f64 } }   // v1; matches the reconciler (no Hover)
```

- `classes` are the existing CSS token contract (`ts-*` binary / `hytte-*`
  library) — plugins style via tokens, never raw CSS.
- Deliberately **not** in v1: arbitrary pixels/images, custom drawing, two-way
  inputs (text entry / slider). Bespoke interactive widgets (the calendar grid)
  stay hand-written Rust. The node set is additive: `Row`/`ListBox` and richer
  props (ellipsize/margins/tooltip) slot in when the first list-y plugin needs
  them.

## Message envelope

```rust
enum PluginMsg {                                     // plugin → host
    Register { manifest: Manifest },                 // first frame after dial-in
    Render   { tree: Node, effects: Vec<Effect> },   // the (tree, effects) frame — pushed anytime
    Log      { level: LogLevel, msg: String },
    Pong     { seq: u64 },
}
enum HostMsg {                                        // host → plugin
    StateSnapshot { snapshot: StateSnapshot },        // full subscribed subset, on change (no deltas)
    Event         { node: NodeId, kind: EventKind },  // user interaction on a rendered node
    EffectResult  { id: u64, outcome: EffectOutcome },// result of a brokered RunCommand
    Ping          { seq: u64 },
    Shutdown,
}
```

Effects are **bundled** on the `Render` frame (not a separate channel), so a
(tree, effects) frame is applied atomically. Most effects are fire-and-forget;
`RunCommand` returns an `EffectResult` correlated by `id` (Elm's `Cmd msg`).

## Manifest, capabilities, effects

```rust
struct Manifest {
    id: String,                    // stable id: audit-log subject + mount-slot key
    proto: u16,                    // exact-matched against PROTO_VERSION on Register
    subscribes: Vec<StateKey>,     // the state subset
    capabilities: Vec<Capability>, // requested shell caps
    mount: Mount,                  // SidebarTop | SidebarBottom | BarLeft | BarCenter | BarRight
}
enum StateKey { Clock }            // v1 starter; additive (battery, media, niri.workspaces, … later)
enum Capability { OpenPage, Niri, Media, Audio, RunCommand }
enum Effect {
    OpenPage(Page),
    Niri(NiriAction),              // FocusWorkspace{id} | FocusWindow{id}
    Media(MediaAction),            // PlayPause | Next | Previous
    Audio(AudioAction),            // SetVolume(f64) | ToggleMute
    RunCommand { id: u64, argv: Vec<String> },
}
```

**Capability model (stamped):** auto-grant from the manifest — this is a
personal shell and Annika owns the plugins — but **audit-log every brokered
effect** with the plugin id, and keep **`RunCommand` a separately-granted,
higher-trust cap**. The host maps each `Effect` to a real `do_thing`
(`modal::toggle`, `niri::focus_*`, `mpris::*`, the pipewire setters, a spawn)
and refuses any effect whose capability wasn't granted.

**Handshake:** plugin connects → `Register { manifest }` → host runs
`Manifest::check_proto` (exact match on `proto`, else drop) and grants caps →
host sends the initial `StateSnapshot` for the subscribed keys → plugin sends
its first `Render`. Thereafter both directions stream freely.

## State subset

```rust
struct StateSnapshot { clock: Option<ClockState> }     // a field per StateKey; Some iff subscribed
struct ClockState    { iso: String, unix: i64 }        // wire projection of clock::now(); no chrono/GTK
```

v1 carries `clock` only. Each `StateKey` maps 1:1 to a service accessor
(`clock::now()`, later `upower::battery()`, `mpris::active_player()`,
`niri::workspaces()`, …); the corresponding `StateSnapshot` field is populated
iff subscribed. The host coalesces bursts (latest-wins) and pushes the full
subscribed subset on change.

## Schema evolution (compat rules — done at birth)

Schema skew across independently-built plugins is the standing risk, so the
guards ship in the first crate rather than being retrofitted:

- **`PROTO_VERSION: u16` exact-matched on `Register`.** A plugin on a different
  proto is rejected at the handshake, never best-effort decoded.
- **Named-field encoding pinned** — bodies are always `rmp_serde::to_vec_named`
  (a map keyed by field name), never positional arrays. Unknown fields are then
  skipped on decode, which is what makes additive evolution safe.
- **Serde enum representation pinned to the default external tagging**
  (`{ "Variant": … }`, keyed by the variant _name_). Not internal/adjacent/
  untagged: appending a variant is only invisible to older code paths because
  tagging is name-based, not positional.

| Change                                                 | Proto    |
| ------------------------------------------------------ | -------- |
| Add an **optional struct field** (`#[serde(default)]`) | **same** |
| Append a **new enum variant** (name-tagged)            | **same** |
| **Rename / remove** a field or variant                 | **bump** |
| **Change a field's type or meaning**                   | **bump** |
| **Reorder** a tuple variant's elements                 | **bump** |

## Crate layout — `hytte-plugin-proto` (PR 1)

A new workspace member, **GTK-free / `hytte-ui`-free / host-free** (a plugin
author links only this + `serde`; the `tokio` framing helpers are optional):

```
crates/hytte-plugin-proto/
  src/lib.rs       # crate doc (topology, TEA, compat rules), PROTO_VERSION, re-exports
  src/wire.rs      # Node, Dir, EventKind, NodeId, Cls — mirror of hytte_ui, GTK-free
  src/msg.rs       # PluginMsg, HostMsg, LogLevel
  src/manifest.rs  # Manifest, StateKey, Capability, Mount, check_proto
  src/state.rs     # StateSnapshot, ClockState
  src/effect.rs    # Effect, Page, NiriAction, MediaAction, AudioAction, EffectOutcome
  src/codec.rs     # length-prefixed framing, ProtoError, encode/decode (+ tokio read_frame/write_frame)
  tests/proto.rs   # hermetic: round-trips, proto mismatch, framing errors, fwd/bwd compat
```

Dependencies: `serde` (derive) + `rmp-serde` only; optional `tokio` (io-util)
behind the `tokio` feature. Inherits the workspace lints (`unsafe_code =
"forbid"`, clippy pedantic-at-deny).

## Out of scope for PR 1 (the rest of the sprint)

- **PR 2 — host transport + `wire::Node → hytte_ui::Node` mapping + sidebar
  mount.** The host listens on the socket, drives the #199 reconciler from
  received `Render` frames, wires the reconciler's `on_event` back to outbound
  `Event` frames, and mounts a plugin's view in the sidebar.
- **Effect broker + audit log** — map `Effect` → `do_thing`, gate on caps, log.
- **State pump** — turn subscribed signal changes into `StateSnapshot` frames,
  coalesced, gated on mount visibility.
- **systemd units + socket wiring in `etc/`** — the supervision surface.
- **A reference `vibectl` plugin** — the end-to-end proof.

## Plugin runtime SDK — `hytte-plugin` (#275, post-sprint)

The reference plugin (PR 3) initially inlined ~150 lines of transport
scaffolding; `crates/hytte-plugin` extracts it as the plugin-side **Rust
runtime** so an author writes only the TEA core:

```rust
trait Plugin {
    type Msg;                                  // own-source messages; Infallible if none
    fn manifest() -> Manifest;
    fn init() -> Self;                         // per-session: state re-derives after reconnect
    fn sources() -> Option<MsgStream<Msg>>;    // self-driven re-renders (timer/fetch), default None
    fn update(&mut self, Input<Msg>) -> Vec<Effect>;
    fn view(&self) -> Node;
}
fn run<P: Plugin>() -> !                       // owns main: dial+backoff, handshake, session loop
```

Runtime decisions (deliberate, settled in #275):

- **`Ping`/`Shutdown` never reach the author** — the runtime answers `Pong`
  itself; `Shutdown` ≡ disconnect → redial with backoff (units run
  `Restart=on-failure`, so a clean exit would strand the plugin across a host
  restart; redialing rides it out).
- **Render dedup replaces "should I re-render"**: after every `update`, a
  `Render` frame goes out iff the tree changed since the last sent one or the
  update returned effects (effects force a send even for an identical tree).
- **Cancel-safety**: `read_frame` is cancel-safe only at frame boundaries, so
  the runtime never races it in `select!` — a reader task owns the read half
  and forwards whole frames over a channel (the host's reader/writer shape).
- **`socket_path()` lives in `hytte-plugin-proto`** (`topology.rs`): the path
  is part of the wire contract; host and SDK share the one definition.

`hytte-plugin-proto` stays the language-neutral schema anchor — a non-Rust
plugin still speaks the wire directly and reimplements this loop.

## Future

- More `StateKey`s (battery, media, net, niri workspaces/window, cpu, weather,
  power-profile) — additive.
- `Row`/`ListBox` nodes + richer props for list-y widgets — additive.
- **Frontend D** (config + scripts) and **frontend A** (WASM) as later feeds for
  the _same_ reconciler, if wanted.

## References

- #35 — the feature request + brainstorm (options, what was ruled out).
- #195 — the design thread (vocab/manifest/supervisor strawman, the stamped
  refinements, the topology 👍).
- #199 — the merged reconciler (`crates/hytte-ui/src/widget_tree.rs`); the
  frozen node vocabulary this protocol mirrors.
- `docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md` — the base
  design (the "composable, not configurable" non-goal this consciously relaxes
  to "config/plugins are an optional layer; native Rust widgets stay
  first-class").
