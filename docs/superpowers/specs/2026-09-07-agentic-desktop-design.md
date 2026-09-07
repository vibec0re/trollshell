# The agentic desktop: hyperhive-backed agent rows, attach, and the Agents tab

**Date:** 2026-09-07
**Status:** Proposed — this is the veto window. No code is written before Annika has read it.
**Issues:** #947 (epic), #948 (the contract), #949 (desktop profile), #950 (attach), #951 (non-root attach), #952 (per-agent cage options), #953 (detached `RunCommand`)

## 1. Summary

A sidebar plugin renders one row per hyperhive agent — `(icon) name (status-icon)`
over `status (pause) (edit)` — and a click opens a terminal attached to that
agent's Claude Code session. The hive is the backend; trollshell is a client.

The plugin is `hytte-plugin-agents`, an **in-tree** plugin binary in the
`hytte-plugin-infobroker` / `hytte-claude-bridge` two-hats shape. It speaks
`host.sock`'s JSON lines itself through a small typed module, because this
workspace has no git dependencies (#757) and hyperhive publishes no client crate
yet (#948). It links no hyperhive code, and the shell links no hive code at all —
the only shell-side change this epic needs is #953.

Everything about the cage stays in the hive. trollshell owns exactly three things:
how to reach the socket, how often to ask, and which terminal to launch.

## 2. Decisions (Annika, 2026-09-07)

Recorded from the #947 / #950 / #951 / #952 threads as decisions, not questions:

1. **hyperhive first.** "I think I'd like to have this hyperhive first if possible.
   Why reinvent wheel." No podman cage; the earlier podman recommendation is
   withdrawn.
2. **Local hive first.** "We go with local hive first; remote hive / swarm support
   later" (#951). That fixes the hive-side order — #949 → #950 → #951 → #952 — and
   moves #948's gateway HTTP/SSE transport to a later phase.
3. **Claude Code only for v1.** "Agents: Focus for now on Claude Code. Other agents
   irrelevant for now" (#950). Per-agent CLI-flag / env overrides are Phase II
   (#952's Phase II section).
4. **The terminal spawns independent of the shell.** "Program should spawn
   independent of shell" — #953 is the in-tree mechanism.
5. **Status is whatever the agent harness says**, "worst case what it sets as title
   in the terminal".
6. **Placement A: in-tree.** "We keep the plugin and integration close to our chest,
   choom. At least for now. Maybe once this all stable we can extract and move to
   either hyperhive project or dedicated project."
7. **tmux is up for discussion** (#951). Section 7 presents the four options with a
   recommendation; it does not decide.

## 3. Goals and non-goals

**Goals**

- One sidebar row per agent, with live status and a working pause button.
- Click a row → a terminal, attached to that agent, that survives a shell restart.
- The desktop reads the hive; the hive stays the single source of truth for agent
  state (the system-daemon-as-state-store rule the canonical design sets out,
  `docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md:95`).
- Nothing crashes, and nothing spins, when there is no hive.

**Non-goals (v1)**

- No podman cage, and no second cage backend of any kind.
- No second runtime — no Pi, no OpenCode. Claude Code only.
- No in-shell VTE. A terminal is an external emulator, launched detached; the
  canonical non-goals already exclude the shell owning a PTY in spirit
  (`docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md:23-28`).
- No remote hive. The transport is the local unix socket only; the gateway
  HTTP + SSE path is a later phase of #948.
- No editing hive config from the desktop. The v1 Agents tab is read-only; a write
  path needs #952 first.
- No new shell-side surface beyond #953 — no new `Control` methods, no new services.

## 4. Vocabulary

- **agent** — a hyperhive-managed identity with a name, a persona, a state dir and
  a turn loop. The hive's `AgentStatusRow.name` (`hive-sh4re/src/container.rs:35`).
- **cage** — the agent's container. hyperhive builds these as `nixos-container`s
  named `h-<agent>`; everything about the cage (mounts, user, capabilities) is hive
  config, never desktop config.
- **session** — Claude Code's on-disk session under
  `~/.claude/projects/<cwd-slug>/<uuid>.jsonl`, resumed **by title**. The hive's
  harness keys every turn on one constant title, `hive-session` by default,
  overridable with `HIVE_SESSION_TITLE`
  (`docs/turn-loop/claude-invocation.md:72-77`). This is the persistent thing —
  not a terminal.
- **attach** — opening that session interactively from the desktop. Which mechanism
  is section 7's open decision.
- **the row** — the plugin's rendering of one agent.

## 5. Architecture — `hytte-plugin-agents`

### 5.1 Which duty owns the process

`hytte-claude-bridge` and `hytte-plugin-infobroker` differ in exactly this, and the
difference is deliberate: the infobroker starts its socket server from `sources()`,
so the server lives one plugin session; the bridge binds and spawns its HTTP
listener on its own runtime **before** handing the main thread to
`hytte_plugin::run` (`crates/hytte-claude-bridge/src/main.rs:444-470`).

`hytte-plugin-agents` is the **infobroker** shape, not the bridge's. It serves
nobody but the shell: with no shell there is nothing to render, so a poll loop that
outlived the session would burn socket round-trips for no reader. The hive client
starts from `sources()` and dies with the session, exactly like
`hytte_plugin_infobroker::serve` (`crates/hytte-plugin-infobroker/src/plugin.rs:116-119`).

### 5.2 The `hive` module — a typed mirror of the wire

One in-tree module, `hive::wire`, mirroring only what the rows need:

| in-tree type     | mirrors                                                                       | fields used                                                                                        |
| ---------------- | ----------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| `Request`        | `HostRequest` (`hive-host-sock/src/lib.rs:117-119`, `#[serde(tag = "cmd")]`)  | `List`, `AgentStatus`, `SetPaused { name, paused }`, `Start { scope }`, `Stop { scope, graceful }` |
| `Scope`          | `LifecycleScope` (`hive-host-sock/src/lib.rs:468-485`)                        | `agent_names` only — see section 11                                                                |
| `Response`       | `HostResponse` (`hive-host-sock/src/lib.rs:521-569`)                          | `ok`, `error`, `agents`, `agent_statuses`                                                          |
| `AgentStatusRow` | `hive_sh4re::container::AgentStatusRow` (`hive-sh4re/src/container.rs:33-66`) | `name`, `running`, `failed`, `needs_update`, `needs_login`, `paused`, `parent`                     |

Rules for the mirror:

- Every struct tolerates unknown keys, so a hive that grows a field does not break
  the desktop.
- Nothing is mirrored that the rows do not render. `queued_dags`, `nodes`,
  `approvals`, `quota`, the whole matrix half — omitted. A smaller mirror is a
  smaller thing to keep in sync, and the extraction (section 13) lifts it out whole.
- `pending_reminders` is deliberately **not** mirrored: the hive stubs it to `0`
  unconditionally (`hive-c0re/src/server.rs:378-384`), so rendering it would be
  rendering a lie.
- **Drift check.** Today there is no version on the wire; #948 asks for one
  ("a versioned schema we can code against and detect drift on" — Annika: "typ
  ultra preem"). When it lands, the client reads it once on connect and, on a major
  mismatch, renders a single "hive protocol vN, plugin speaks vM" row instead of
  guessing. Until then the drift detector is the fixture suite in section 12.

### 5.3 Dialing the socket

- **Path**: `/run/hyperhive/host.sock`, overridable from `agents.toml`
  (`nix/host-modules/hive-c0re/default.nix:390`, `hivectl/src/client.rs:2`).
- **Permissions**: the socket is `0660 root:hive-admin` and its directory `0751`
  (`nix/host-modules/hive-c0re/default.nix:390-398`). The desktop user must be in
  `hive-admin`, granted hive-side by
  `services.hyperhive.c0re.adminUsers = [ "annika" ]`
  (`nix/host-modules/hive-c0re/options.nix:419-432`). That option's own docs call it
  "a real privilege grant … the admin socket is _full_ hive control"
  (`options.nix:429-431`) — see section 11.
- **No socket, no crash.** Absent path, `ECONNREFUSED` and `EACCES` all resolve to
  one model state: `Hive::Unreachable { reason }`. The plugin renders a single row —
  "no hive" plus the reason, ellipsized — and keeps its poll cadence. It never
  panics, never exits, never busy-loops. A plugin that exits would be restarted by
  its transient unit and flap; parking is correct.
- **Reconnect**: one connection per request, which is `hivectl`'s own model. At a
  5 s cadence a persistent connection buys nothing and complicates the unreachable
  path.

### 5.4 Polling until `Subscribe` exists

`host.sock` is poll-only: there is no subscribe verb
(`hive-host-sock/src/lib.rs:117-432`). So v1 polls `AgentStatus` on a cadence
(`tick_stream`, `crates/hytte-plugin/src/lib.rs:399`), default 5 s, from
`agents.toml`.

Two mitigations, both free:

- The plugin mounts in the **sidebar**, so it subscribes `StateKey::SlotVisible` and
  parks the poll while the sidebar is closed — exactly the hook that key exists for
  (`crates/hytte-plugin-proto/src/manifest.rs:25-39`). A bar mount would not get
  this, which is one reason the row list is a sidebar card.
- A poll answering identically re-renders to an identical `View`, which the SDK
  dedups before it reaches the wire, so a quiet hive costs one socket round-trip and
  zero frames.

#948's `Subscribe` verb replaces the cadence with a stream and this section
collapses to "read events". The shape to ask for already exists on the HTTP side:
`/api/dashboard/stream` emits `DashboardEvent`s
(`hive-c0re/src/dashboard_events.rs:13-15`) and already takes a `kinds` filter on
its query string (`hive-c0re/src/dashboard/state_snapshot.rs:668,687`) — so
"`Subscribe { kinds }` over the line-framed socket" is the same idea on the
transport we already speak.

### 5.5 How it is launched

Like every other plugin: `programs.trollshell.plugins.agents` in the home-manager
module (`nix/module-common.nix:608`), rendered into `trollshell/plugins.json`
(`nix/hm-module.nix:42-49,325`), launched by the declarative launcher as a transient
`trollshell-plugin-agents` user unit via `systemd-run --user --collect --unit=…`
(`trollshell/src/plugin_launcher.rs:497-517`). Start/stop from the control-center's
Plugins tab works with no new code. It declares no secret slots — it holds no
credentials (section 11).

### 5.6 What it does not do

- It does not spawn, destroy, rebuild or deploy agents. `Spawn` and `Destroy` are
  provisioning verbs with approval semantics; the sidebar is a status surface.
- It does not link `hytte-services`, `hytte-ui`, GTK, or any hive crate.
- It does not read or write hive config files.
- It adds no `Control` D-Bus methods (`trollshell/src/control.rs:116-118`) and no
  shell-side module. The one shell change in this epic is #953.

## 6. The rows

### 6.1 The Node tree for one row

Annika's mock is two lines. `ListBox` materializes as a **selection-less** list
(`crates/hytte-plugin-proto/src/wire.rs:141-150`), so there is no row-activate
event — every interaction is a `Button`, whose `id` is required and is the click
target (`wire.rs:249-255`).

```text
ListBox { classes: ["ts-agents-list"], children: [
  Box { dir: Vertical, spacing: 2, classes: ["ts-agent-row"], children: [
    Row { classes: ["ts-agent-head"], children: [
      Icon   { name: "<runtime icon>", classes: ["ts-agent-runtime"] },
      Button { id: "attach:<name>", classes: ["flat", "ts-agent-name"],
               child: Label { text: "<display name>" } },
      Spacer,
      Icon   { name: "<status icon>", classes: ["ts-agent-state", "<state class>"] },
    ]},
    Row { classes: ["ts-agent-foot"], children: [
      Text   { text: "<status text>", ellipsize: true, classes: ["dim-label"] },
      Spacer,
      Button { id: "pause:<name>", classes: ["flat"],
               child: Icon { name: "media-playback-pause-symbolic" } },
      Button { id: "edit:<name>", classes: ["flat"],
               child: Icon { name: "document-edit-symbolic" } },
    ]},
  ]},
  … one per agent
]}
```

Every node in that tree already exists in the vocabulary — `Box` (`wire.rs:125`),
`Row` (`wire.rs:137`), `ListBox` (`wire.rs:146`), `Label` (`wire.rs:152`), `Text`
(`wire.rs:179`), `Icon` (`wire.rs:189`), `Button` (`wire.rs:250`), `Spacer`
(`wire.rs:343`). **No proto change, no `VOCAB` bump.** Mount is `Mount::SidebarTop`
(`crates/hytte-plugin-proto/src/manifest.rs:194-202`).

### 6.2 Status mapping

`AgentStatusRow`'s flags are explicitly orthogonal, not a state machine
(`hive-sh4re/src/container.rs:39-41`), so the row picks **one** primary state by
strict precedence and renders `needs_update` as a secondary badge:

| precedence | condition      | icon                                 | text               | class          |
| ---------- | -------------- | ------------------------------------ | ------------------ | -------------- |
| 1          | `failed`       | `dialog-error-symbolic`              | failed             | `error`        |
| 2          | `needs_login`  | `dialog-password-symbolic`           | needs login        | `warning`      |
| 3          | `paused`       | `media-playback-pause-symbolic`      | paused             | `dim-label`    |
| 4          | `!running`     | `media-playback-stop-symbolic`       | stopped            | `dim-label`    |
| 5          | `running`      | `media-playback-start-symbolic`      | the harness's text | `accent`       |
| badge      | `needs_update` | `software-update-available-symbolic` | (tooltip only)     | `ts-agent-upd` |

Row 5's "harness's text" is the one field that **is not on the wire today**. The
free-text status `set_status` writes lands in `state/hyperhive-status`
(`docs/agent-lifecycle/persistence.md:274-278`), is read by
`container_view::read_agent_status_live` (`hive-c0re/src/container_view.rs:277`),
and is surfaced on the _agent_ socket's `GetAgentMeta`
(`hive-c0re/src/socket_server/mod.rs:414-415`) and the dashboard — but
`handle_agent_status` drops it when it projects `ContainerView` onto
`AgentStatusRow` (`hive-c0re/src/server.rs:367-388`). `active_model`
(`hive-c0re/src/container_view.rs:65`) is dropped there too.

**This is the highest-value ask in #948**: put `status_text`, `status_set_at` and
`active_model` on `AgentStatusRow`. Until then row 5 reads "running" and Annika's
decision 5 is only half-honoured. The plugin renders "running" and does not invent
a substitute.

### 6.3 The three interactions

| gesture        | button id       | v1 behaviour                                                                                                                                |
| -------------- | --------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| click the name | `attach:<name>` | attach (section 7) — a detached `RunCommand`, needs #953 and #951                                                                           |
| click pause    | `pause:<name>`  | `SetPaused { name, paused: !paused }` (`hive-host-sock/src/lib.rs:160`); optimistic flip, reconciled by the next poll                       |
| click edit     | `edit:<name>`   | open the agent's hive page (`Urls` → `/agent/<name>/`), else reveal the config repo path in an `Expander`. **Real editing waits for #952.** |

Pause is the one write v1 does, and it is the safest one in the vocabulary: the
hive's own docs describe it as "a single marker write … applies immediately and
works on a stopped container too" (`hive-host-sock/src/lib.rs:152-158`). It is
idempotent both ways, so a double-click is harmless.

### 6.4 The panel

`Page::PluginSelf` (`crates/hytte-plugin-proto/src/effect.rs:36-43`) via
`View::panel` (`crates/hytte-plugin/src/lib.rs:602-624`), following #487's settled
preference that status/config UI rides the plugin's own panel before it grows
control-center surface. Contents, for the selected agent:

- the full flag set as a readable list, including `needs_update` and `deployed_sha`;
- `parent`, so the topology is visible;
- the hive's reachability state and the socket path in use;
- the last poll's age;
- start / stop buttons, scoped to that one agent (section 11's rule);
- a link row: the agent page URL, the config repo path.

## 7. Attach — the #950 decision table

Four options. **Nothing here is decided** — Annika reopened tmux on #951.

| option                                                  | needs from the hive                                        | root?                           | survives shell restart | sees the loop's transcript       | remote-capable               |
| ------------------------------------------------------- | ---------------------------------------------------------- | ------------------------------- | ---------------------- | -------------------------------- | ---------------------------- |
| **1. tmux in the cage**                                 | a `tty` agent kind + a supervised tmux session (#950, new) | yes, unless #951                | yes (in the cage)      | no — a second, parallel `claude` | no                           |
| **2. pause + `claude --resume <title>` via `choom`** ⭐ | #951 only; `choom --resume` already ships                  | **no**, with #951's polkit rule | yes (own terminal)     | **yes** — same session file      | via `ssh`                    |
| **3. the per-agent web page**                           | nothing — it exists today                                  | no                              | yes (a browser)        | rendered stream, not a tty       | **yes**, through the gateway |
| **4. a harness-owned PTY on `web.sock`**                | the real attach primitive (#951 option 2, largest change)  | no                              | yes                    | yes                              | yes                          |

**Recommended default: option 2** — recommended, not decided.

Why it is the cheapest thing that satisfies decisions 4 and 5:

- `hivectl agent <name> choom` already does the interactive-claude-in-the-cage
  shape, and already takes `--resume <value>`, which passes straight through to
  `claude --resume` (`docs/tools/hivectl.md:213-245`). Nothing is invented.
- The harness's session title is constant and knowable — `hive-session`, or
  `HIVE_SESSION_TITLE` (`docs/turn-loop/claude-invocation.md:72-77`) — so
  `choom --resume hive-session` lands the operator in **the agent's own
  transcript**, and the loop's next turn sees what the human did.
- `choom` already reproduces the harness's environment: the agent user, the state
  dir as cwd, and `--settings` / `--mcp-config` / `--system-prompt-file`
  (`docs/tools/hivectl.md:247-268`).
- It needs no tmux, no PTY, no new socket verb, and no in-shell terminal.

The one hazard, stated plainly: because the title is constant, resuming it **does**
collide with the live harness — precisely the case the hive's own note excludes for
a _blank_ choom ("it won't carry our title",
`docs/turn-loop/claude-invocation.md:83-84`). So option 2 is **pause-first, always**:

```text
SetPaused { name, paused: true }   →  wait one poll for paused == true
RunCommand { detached: true, argv: [<terminal…>, "hivectl", "agent", <name>,
                                    "choom", "--resume", <title>] }
on EffectResult (launch ok)        →  the row shows "paused · attached"
on terminal exit                   →  SetPaused { name, paused: false }
```

Unpause-on-exit is the piece #953 must not lose. A detached launch reports launch
success only, not exit status (#953's own proposal), so v1 unpauses on the
operator's next click of the pause button and the row makes that state obvious. If
#953 can cheaply surface "the transient unit stopped" without re-parenting the
child, the plugin unpauses automatically; if not, manual unpause is the honest v1
and the row says so.

The terminal is a config key — an argv prefix, defaulting to Annika's emulator:

```toml
terminal = ["alacritty", "-e"]
```

## 8. Status source

In Annika's order, and only these:

1. **The harness**, via `AgentStatusRow` plus the free-text `set_status` string —
   the flags today, the text once #948 puts it on the row (section 6.2).
2. **The terminal title** only ever matters _inside_ an attached terminal, where
   Claude Code sets it itself. trollshell never scrapes it: the plugin does not own
   the terminal's PTY, and after #953 the terminal is not even its child. This is
   the floor Annika named, and the emulator satisfies it, not us.

`Effect::Notify` (`crates/hytte-plugin-proto/src/effect.rs:106-117`, cap
`Capability::Notify`, `crates/hytte-plugin-proto/src/manifest.rs:120-122`) fires on
**edges only**, never on a level: an agent flipping into `failed` or `needs_login`,
or — once the text is on the wire — into a status the plugin's config marks as
waiting. Edge-only matters: a hive with one wedged agent must not toast every 5 s.
The plugin holds the previous poll's flags per agent and diffs.

## 9. Config — what trollshell owns

An `agents` subsystem through `hytte-config`
(`crates/hytte-config/src/subsystem.rs:79-96`), which buys the XDG layering
(`crates/hytte-config/src/xdg.rs:130-156`), the four merge rules including `_unset`
(`crates/hytte-config/src/merge.rs:34-48`), unknown-key warnings rather than
failures, and the format-preserving writer — all from a `NAME` and a `DEFAULT_TOML`.

```toml
# ~/.config/trollshell/agents.toml
socket = "/run/hyperhive/host.sock"   # override only
poll_seconds = 5
terminal = ["alacritty", "-e"]
attach = "choom-resume"               # section 7; "web" and "choom-blank" also valid
session_title = "hive-session"        # matches HIVE_SESSION_TITLE on the hive

[display.trollshell-choom]
label = "choom"
icon = "starred-symbolic"
```

That is the **whole** list. Everything else — mounts, mapped user, container
capabilities, forge project, skill folders, model, effort level, backend env file —
is hive config and is **never** duplicated here. The hive already owns the
Claude-Code knobs as module options: `hyperhive.model`
(`nix/agent-modules/agent-service.nix:16`), `availableModels` (`:37`),
`effortLevel` (`:65`), `backendEnvironmentFile` (`:131`). The desktop reads them; it
does not keep a second copy that can drift.

The roster is the hive's, not the file's: `[display.<name>]` sections only decorate
agents the hive reports. A section naming an unknown agent is inert and warned, not
an error — the merge layer's unknown-key rule.

## 10. Control-center Agents tab (phase 2)

Same shape as the Plugins tab after #887/#943: an `AdwBreakpointBin` over an
`AdwNavigationSplitView` — split panes wide, push navigation narrow, one widget tree
for both, and a detail pane that is _retargeted_ rather than rebuilt so the poll
stays invisible (`crates/trollshell-control-center/src/plugins_tab.rs:10-45`). It is
added to the `ViewStack` next to Plugins, Places and AI Keys
(`crates/trollshell-control-center/src/main.rs:95-118`).

**v1 is read-only.** Per agent: the flags, `deployed_sha`, `parent`, the hive's
`model` / `effortLevel` / mounts / user / project as the hive reports them, and links
to the agent page and config repo. Read-only is not a placeholder — it is what the
hive supports: edits today are a forge PR plus an operator approval
(`ApprovalKind::MergeConfigPr`), with no field-level write.

Two candidate write paths, both downstream of #952:

| write path                                      | pros                                                                          | cons                                                                                    |
| ----------------------------------------------- | ----------------------------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| **edit via forge PR + approval**                | the hive's real model today; auditable; nothing new on the socket             | a two-step round trip for a checkbox; needs a forge, which #949's desktop profile drops |
| **`SetAgentOptions` on the desktop profile** ⭐ | one verb, immediate; matches "the operator talking to themselves" on a laptop | new hive surface (#952); no audit trail; diverges from the swarm's approval discipline  |

Recommended: `SetAgentOptions`, **for the desktop profile only** — a hive with no
forge (#949) cannot use the PR path at all, so the desktop profile needs the verb
regardless. Not decided; it is downstream of #952 either way.

## 11. Trust boundary

The socket is full hive control — "spawn / kill / destroy / deploy"
(`nix/host-modules/hive-c0re/options.nix:429-431`). Three rules follow.

**One: the scope footgun.** `LifecycleScope::is_everything` treats an all-false scope
as **everything** (`hive-host-sock/src/lib.rs:487-495`), so a `Start {}` with a
defaulted scope starts the entire hive. The in-tree mirror's `Scope` therefore has
**no `Default` impl** and no all-false constructor: the only way to build one is
`Scope::agent(name)`, which fills `agent_names`. A unit test asserts every serialized
`Start` / `Stop` frame carries a non-empty `agent_names`.

**Two: `RunCommand` launches a fixed argv shape, never a shell.** The plugin holds
`Capability::RunCommand` (`crates/hytte-plugin-proto/src/manifest.rs:116-117`), the
higher-trust, separately granted capability. It emits exactly one argv shape:

```text
[ …terminal-prefix…, "hivectl", "agent", <name>, "choom", "--resume", <title> ]
```

`<name>` is echoed from the hive's own `AgentStatusRow.name`, never from the config
file, and is validated against `[A-Za-z0-9_-]+` before it reaches the argv. There is
no `sh -c`, no shell metacharacter path, and no plugin-supplied program name outside
the configured terminal prefix. The effect broker already audits every `RunCommand`
by kind (`trollshell/src/plugins/effects.rs:514`).

**Three: no secrets cross the plugin.** It holds no token, declares no secret slot,
and reads no credential. Its entire authority is the desktop user's `hive-admin`
membership, granted hive-side by `adminUsers`
(`nix/host-modules/hive-c0re/options.nix:419-432`) and enforced by the socket's
`0660 root:hive-admin` mode (`nix/host-modules/hive-c0re/default.nix:390-396`). A
user outside the group gets `EACCES` and the "no hive" row — the correct
unprivileged outcome, not a bug.

**An aside #953 should fold in.** The problem is worse than #953's body states. A
`RunCommand` child is not merely in the shell's cgroup: `execute_command` awaits it
under a 10-second `RUN_COMMAND_TIMEOUT` with `kill_on_drop(true)`
(`trollshell/src/plugins/effects.rs:352,400-401`). A terminal launched through the
effect today is **killed after ten seconds**, restart or no restart. #953's
`detached: true` path must therefore bypass the timeout and the await entirely, not
merely re-parent the child.

## 12. Testing and CI

Everything below is hermetic and rides the existing gates (`cargo test`,
`nix flake check`, `cargo clippy --workspace --all-targets --features system-tests`).

| check                                                                                     | where                                           |
| ----------------------------------------------------------------------------------------- | ----------------------------------------------- |
| `Request` serializes to the exact `{"cmd":"agent_status"}` line shape, per verb           | `hive::wire` unit test                          |
| recorded real `HostResponse` JSON lines decode into the mirror (fixture files)            | `hive::wire` fixture test                       |
| a response carrying **unknown** fields still decodes (forward drift)                      | `hive::wire` fixture test                       |
| every `Start` / `Stop` frame the plugin can emit has a non-empty `agent_names` (rule 1)   | `hive::wire` unit test                          |
| flags → (icon, text, class) for all 32 flag combinations, precedence pinned               | plugin unit test                                |
| `View` render-tree goldens: unreachable, empty hive, running, paused, failed, needs-login | plugin golden test                              |
| a click on `pause:<name>` emits exactly one `SetPaused` with the flipped bool             | plugin reducer test                             |
| the attach argv is the fixed shape, and a rejected name never reaches it                  | plugin reducer test                             |
| `Notify` fires on the flag **edge**, not the level (two identical polls → one toast)      | plugin reducer test                             |
| an absent / refused / permission-denied socket parks and re-polls, never exits            | fake-socket integration test (`system-tests`)   |
| the detached `RunCommand` argv is `systemd-run`-wrapped and never awaited                 | #953, `trollshell/src/plugins/effects.rs` tests |
| a detached child outlives the effect broker being dropped                                 | #953, `system-tests`                            |

The fixtures are the drift detector until #948 gives a version. They are **recorded
from a live hive**, checked in verbatim, and a hive-side wire change fails them —
deliberately, because a hand-written fixture would only prove the mirror agrees with
itself. Recording them is the one step that needs Annika's machine.

Falsification, per the house rule: each mechanism must have a test that goes red when
the mechanism is deleted. Delete the edge diff in `Notify` and the two-identical-polls
test must fail. Delete the name validation and the attach-argv test must fail. Give
`Scope` a `Default` and the scope test must fail.

**Live-verify** (for `docs/live-verify.md` when this ships) — only Annika's glass can
settle these: the rows render legibly in the sidebar at her scale; the pause button
actually parks a live agent; `hivectl … choom --resume hive-session` lands in the
agent's own transcript and the loop's next turn sees it; the launched Alacritty
survives `systemctl --user restart trollshell`; a `needs_login` flip raises exactly
one toast.

## 13. Phases

| phase | what                                                                                                                                             | blocked on                                                                                                    |
| ----- | ------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------- |
| P0    | this spec                                                                                                                                        | Annika's veto                                                                                                 |
| P1    | #953 (detached `RunCommand`, including the timeout fix) + `hytte-plugin-agents`: wire mirror, rows, pause, panel                                 | nothing in-tree; dev against a fake socket, live against a reachable hive — which for a laptop means **#949** |
| P2    | control-center **Agents** tab, read-only, adaptive drill-down                                                                                    | P1                                                                                                            |
| P3    | attach, per whichever section 7 option wins                                                                                                      | **#950** (options 1 and 4) + **#951**                                                                         |
| P4    | edit — real per-agent options                                                                                                                    | **#952**                                                                                                      |
| later | remote hive: #948's gateway HTTP + SSE transport; `Subscribe` replacing the poll; extraction to hyperhive or a dedicated project; other runtimes | **#948**, and decision 6's "once this all stable"                                                             |

P1 is buildable **today** against a fake socket, and that is the point of the fixture
suite: the plugin can be finished, tested and reviewed before a hive exists on the
laptop. It just cannot be _live-verified_ until #949.

## 14. Open questions

1. **Attach mechanism** — section 7's four options: tmux, pause + `claude --resume`, the per-agent web page, or a harness-owned PTY? (Recommendation: option 2.)
2. **#953 now or later** — still unanswered on #947; P1 cannot ship without it.
3. **Edit surface** — the plugin's own panel (the #487 groove) or a control-center tab, once #952 makes editing real? Both eventually, but which first?
4. **Plugin name** — `agents`, `hive`, or `choom`? It becomes the crate name, the unit name (`trollshell-plugin-<id>`) and the `plugins.<id>` key, so it is awkward to change later.
5. **Is `trollshell-choom` the first agent**, and does it live on the laptop hive or the existing swarm?
6. **Auto-unpause** — should attach pause the loop automatically, or refuse to attach while the loop runs and make the operator pause first?
7. **Notify policy** — toast on `needs_login` and `failed` only, or also on a harness status the config marks "waiting for you", once #948 puts the text on the row?

## 15. References

- hyperhive checkout: `/home/annika/viberoot/hyperhive` (every hive pointer above).
- Host socket protocol: `hive-host-sock/src/lib.rs`, `hive-host-sock/README.md`.
- Agent status row: `hive-sh4re/src/container.rs:33-66`; its projection at
  `hive-c0re/src/server.rs:367-388`.
- Attach today: `docs/tools/hivectl.md:213-268`.
- Session identity: `docs/turn-loop/claude-invocation.md:4-9,72-87`.
- Event push: `hive-c0re/src/dashboard_events.rs:13-15`,
  `hive-c0re/src/dashboard/state_snapshot.rs:668,687`.
- Forge is mandatory today (why #949 exists): `nix/host-modules/deploy.nix:152-155`.
- Plugin vocabulary: `crates/hytte-plugin-proto/src/{wire.rs,effect.rs,manifest.rs}`.
- The two-hats precedent: `crates/hytte-plugin-infobroker/src/plugin.rs`,
  `crates/hytte-claude-bridge/src/main.rs:444-470`.
- Launcher and control surface: `trollshell/src/plugin_launcher.rs`,
  `trollshell/src/control.rs`, `nix/module-common.nix:608`.
- Config layering: `crates/hytte-config/src/{subsystem.rs,xdg.rs,merge.rs}`.
- The canonical design's non-goals:
  `docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md:23-28`.
