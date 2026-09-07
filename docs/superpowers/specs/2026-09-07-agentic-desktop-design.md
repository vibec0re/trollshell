# The agentic desktop: hyperhive-backed agent rows, attach, and the Agents tab

**Date:** 2026-09-07 (amended the same day — see section 2.1)
**Status:** Proposed — this is the veto window. No code is written before Annika has read it.
**Issues:** #947 (epic), #948 (the contract), #949 (the all-local deployment mode on a laptop), #950 (attach), #951 (non-root attach), #952 (per-agent cage options), #953 (detached `RunCommand`)
**Hyperhive issues:** hyperhive#4037, hyperhive#4038, hyperhive#4039 (filed by @the-sword-above on hyperhive's internal forge, not publicly linkable)

## 1. Summary

A sidebar plugin renders one row per hyperhive agent —
`(icon) name (status-icon)` over `status (pause) (term) (edit)` — and a click
opens a terminal attached to that agent's Claude Code session. The hive is the
backend; trollshell is a client.

The plugin is `hytte-plugin-agents`, an **in-tree** plugin binary in the
`hytte-plugin-infobroker` / `hytte-claude-bridge` two-hats shape. It speaks
`host.sock`'s JSON lines itself through a small typed module, because this
workspace has no git dependencies (#757) and hyperhive publishes no client crate
yet (#948). It links no hyperhive code, and the shell links no hive code at all —
the only shell-side change this epic needs is #953.

**Scope: one hive, never a swarm.** The plugin talks to the local hive's
`host.sock` and to `127.0.0.1`, and to nothing else. Forge, matrix and the swarm
controller are swarm-level concerns it never touches; agent create and destroy are
out of v1 entirely. `hivectl` stays out of the status and control loop — it appears
exactly once, as the argv behind "attach".

Everything about the cage stays in the hive — and, since amendment f, most of what
looked like "cage config" is really the agent's own config flake, which is a git
repo and not a hive surface at all (section 9). trollshell owns four things: how to
reach the socket, how often to ask, which terminal to launch, and what to call each
agent on screen.

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

### 2.1 Amendments from the hive side (2026-09-07, 09:22Z–12:22Z)

The first draft of this spec (07:51Z) predated @kaesaecracker (Mara, a hyperhive
dev) and @the-sword-above joining the threads. Their input moved eight things.
Annika's seven decisions above are **unchanged**; these constrain how they are met,
and retract three things earlier drafts got wrong: a forge-less hive profile (a),
the reason behind section 10's recommendation (a), and the claim that the swarm
control plane waits on Mara's hardware (h).

- **a. A hive is never standalone.** Mara, [#947 09:22Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5568440406):
  "a hive itself can only be deployed as part of a swarm and agent-agent comms go
  through swarm matrix." So decision 2's "local hive" is **the desktop running its
  own all-local swarm**, not a forge-less profile — the earlier proposal is
  withdrawn. The switch already exists:
  `services.hyperhive.deploy.singleHostSwarm`
  (`nix/host-modules/local-defaults.nix:26`), which asserts `allSwarmServices`, the
  swarm CA, the controller and `gateway.localHostsEntry` so the names resolve
  from `/etc/hosts` with no real DNS. Mara, [#947 09:35Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5568616265):
  "if anni wants to, she can enable a single option to also auto-deploy the swarm
  services on the same host. i call it the all-local deployment mode. she does not
  have to use the local forge directly for project work or chat via matrix - it can
  be just for the agents themselves."
- **b. Not Mara's swarm.** Same comment: "i would argue against joining my swarm, as
  my swarm is also laptop based rn … while hyperhive supports multiple operator
  logins, there is no separation between users currently, so it would still be more
  like two swarms where agents happen to have forge/matrix access to the other
  swarm." The join question is withdrawn; #949 was re-scoped accordingly.
- **c. Talk to the socket, not the CLI.** Mara, [#947 09:22Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5568440406):
  "instead of the hivectl command line, you could also integrate via the host unix
  domain socket directly (no shell needed, just read/write a sock)" and "with the
  appropriate hive setting, the host sock and most of hvectl is accessible without
  root (sock is group owned, no polkit)". Confirmed on [#949 10:03Z](https://github.com/vibec0re/trollshell/issues/949#issuecomment-5568952208):
  "host.sock is always group accessible, the adminusers option just adds the users
  to the group." That is section 5's design already; it also shrinks #951 to the
  `machinectl shell` polkit rule alone.
- **d. Lifetime ops are moving hive → swarm.** Mara, same comment: "the following
  things will move from hive to swarm in the future (though there could be a back
  channel for local cli control if you need it): permissions; agent create/destroy
  (controlled on swarm level, still runs on hive); secret management". She also
  asked for the frozen surface in writing — "please note down what api surface you
  need to be stable … if you integrate with the hive directly, we will have to keep
  a local override for lifetime stuff." That list now lives on
  [#948 09:24Z](https://github.com/vibec0re/trollshell/issues/948#issuecomment-5568468371);
  the fork it opens is **section 5.7, and it is OPEN.**
- **e. Two terminals, both real.** Mara, same comment: "the direct choom session in
  the container: this is for when you want to use claude code directly" versus "the
  agent web terminal: this is a claude-code-like web ui with a message history and
  ability to send messages, see todos, interrupt the turn and all that jazz …
  i mostly use the web thing to check on the agents and maybe send them a msg as
  they do dev work without my direct attention." Section 7's options 2 and 3 are
  therefore **both** kept, and the row offers both.
- **f. Mounts are the wrong shape.** Mara, [#952 11:31Z](https://github.com/vibec0re/trollshell/issues/952#issuecomment-5569977099):
  "agents typically just clone the repo and file prs directly, but i like the
  'dispatch an agent on a repo with a task' workflow", and "the swarm level forge
  has a knowledge repo agents get a local copy of … which makes the notes mount
  potentially redundant". Then [#952 11:32Z](https://github.com/vibec0re/trollshell/issues/952#issuecomment-5569995865):
  "each agent has its own config flake, so anything inside the container can just be
  confed in the flake (including custom inputs), its just the host level args you
  cannot change in a free-form way." Sections 9, 10 and phase P4 are rewritten
  against that split.
- **g. Three hyperhive issues are filed.** @the-sword-above verified each against
  hyperhive's source before filing — [#948 09:33Z](https://github.com/vibec0re/trollshell/issues/948#issuecomment-5568584669)
  and [#951 09:41Z](https://github.com/vibec0re/trollshell/issues/951#issuecomment-5568685307):

  | issue          | ask                                                                  | verified detail                                                                                                                                                                                                      |
  | -------------- | -------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
  | hyperhive#4037 | `status_text` / `status_set_at` / `active_model` on `AgentStatusRow` | `active_model` **already exists** one call upstream in `container_view::build_all` — not copied                                                                                                                      |
  | hyperhive#4038 | a schema / `version` field on `HostResponse`                         | zero version or schema field exists on that struct today                                                                                                                                                             |
  | hyperhive#4039 | a polkit rule granting `hive-admin` the action `choom` triggers      | `machinectl shell <name>@h-<name>` triggers **exactly one** action, `org.freedesktop.machine1.shell`; `login` and `host-shell` are different verbs `choom` never calls. hyperhive has zero `security.polkit.*` today |

- **h. The swarm control plane is _local_, not Mara's.** Mara,
  [#947 12:21Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5570553019):
  "'the swarm control plane once her central node exists' — my central node would
  not be annis central node." An earlier draft had option (ii) of the lifetime-ops
  fork blocked on her pending hardware; it never was. `singleHostSwarm` asserts
  `deploy.swarm-controller.enable`
  (`nix/host-modules/local-defaults.nix:36,131`), so Annika's laptop runs its own
  controller on loopback and the fork is a live choice today. Section 5.7 is
  rewritten against the controller's actual routes, and finds two gaps the thread
  did not surface — it cannot express `paused`, and its row is a different shape.

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
- No second runtime — no Pi, no OpenCode. Claude Code only. Recorded so nobody
  re-derives it: the hive side of this is smaller than #950's body implies. Mara,
  [#947 09:22Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5568440406):
  "you can already use openrouter to redirect claude code to a different provider.
  if need be, support for eg opencode can be added, i just never added it bc i dont
  have any inference except the claude subscription so i cannot test it." A second
  runtime is a hive feature waiting on a use case, not a redesign — and still out
  of v1.
- **No swarm-level surface.** Forge, matrix, the controller, NATS and approvals are
  swarm concerns; the plugin is scoped to **one hive** (amendment a/c) and reads
  none of them. It talks to `host.sock` and `127.0.0.1`, full stop.
- **No agent create or destroy.** `Spawn`, `RequestSpawn` and `Destroy` are
  provisioning verbs with approval semantics, and they are among the things moving
  to the swarm control plane (amendment d). Agents are declared elsewhere; the
  desktop lists, starts, stops, pauses and attaches.
- **No `hivectl` in the status or control loop.** It appears once, as the attach
  argv (amendment c).
- No in-shell VTE. A terminal is an external emulator, launched detached. The
  canonical design does not name terminals in its non-goals — the closest line is
  "Lockscreen, OSD, or compositor in `hytte`"
  (`docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md:28`), which is
  about scope, not PTYs — so this is a **new** non-goal, stated here rather than
  inherited: a VTE would add a GTK dependency and make the shell the owner of a
  PTY it would then have to keep alive across its own restarts, which is exactly
  what decision 4 rules out.
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
- **hive** — one host running `hive-c0re` plus its agents' cages, addressed by the
  desktop through `/run/hyperhive/host.sock`. **A hive is never standalone**: it is
  always part of a swarm (amendment a). This is the plugin's entire world.
- **swarm** — the layer above: forge, matrix, the controller, NATS, approvals, and
  increasingly the lifetime control plane (amendment d). The desktop reads none of
  it **today**. On Annika's laptop the swarm and the hive are the same box, via
  `services.hyperhive.deploy.singleHostSwarm`
  (`nix/host-modules/local-defaults.nix:26`), and the forge and matrix exist for the
  agents rather than for her — project work stays on GitHub with a token.
- **the (swarm) controller** — `swarm-controller`, the swarm's HTTP control plane.
  `singleHostSwarm` asserts it on
  (`deploy.swarm-controller.enable`, `nix/host-modules/local-defaults.nix:36,131`),
  so on Annika's laptop it is a **local** service on loopback, not Mara's
  infrastructure. That is why section 5.7's option (ii) is a live choice rather
  than a blocked one.
- **`trollshell-choom`** — the working name, from Annika's mock on #947, for an
  agent that maintains trollshell from a cage. It is the spec's running example of
  a row; whether it is actually the **first** agent to exist is open question 6.
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

**The authoritative list of what the desktop needs frozen is on #948**, posted at
Mara's request ([#948 09:24Z](https://github.com/vibec0re/trollshell/issues/948#issuecomment-5568468371)):
seven verbs (`List`, `AgentStatus`, `Start`, `Stop`, `SetPaused`, `Restart`,
`Urls`), one row shape, one new push (`Subscribe { kinds }`), plus a `version`
field on responses. It also names what the desktop will **never** need —
`Spawn`, `RequestSpawn`, `Destroy`, `Approve` / `Deny` / `Pending`, `Matrix*`,
`Forge*`, `Gateway*`, `Quota*`, `SetResourceLimits`, `SetParent`, `Rebuild` — which
is what keeps a local override cheap while lifetime ops move (amendment d, section
5.7). This spec does not re-derive that list; it implements it.

One in-tree module, `hive::wire`, mirroring only what the rows need — and covering
**every** verb and field a later section relies on, so nothing in sections 6, 7, 9
or 10 reaches for something this table does not carry:

| in-tree type     | mirrors                                                                       | fields used                                                                                                                    | used by                                                                                          |
| ---------------- | ----------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------ |
| `Request`        | `HostRequest` (`hive-host-sock/src/lib.rs:117-119`, `#[serde(tag = "cmd")]`)  | `List`, `AgentStatus`, `SetPaused { name, paused }`, `Start { scope }`, `Stop { scope, graceful }`, `Restart { name }`, `Urls` | the rows and the panel; `Urls` backs the `web:<name>` button                                     |
| `Scope`          | `LifecycleScope` (`hive-host-sock/src/lib.rs:468-485`)                        | `agent_names` only — see section 11                                                                                            | the panel's per-agent start / stop                                                               |
| `Response`       | `HostResponse` (`hive-host-sock/src/lib.rs:521-569`)                          | `ok`, `error`, `agents`, `agent_statuses`, `urls`                                                                              | every request                                                                                    |
| `AgentStatusRow` | `hive_sh4re::container::AgentStatusRow` (`hive-sh4re/src/container.rs:33-66`) | `name`, `running`, `failed`, `needs_update`, `needs_login`, `paused`, `parent`, `deployed_sha`                                 | section 6.2's precedence; `parent` and `deployed_sha` are panel- and tab-only (sections 6.4, 10) |
| `HiveUrls`       | `HiveUrls` (`hive-host-sock/src/lib.rs:503-518`)                              | `home` (and `domain` as the fallback)                                                                                          | the agent-page URL, **derived** — see below                                                      |

**One derivation, stated so nobody assumes otherwise:** `HiveUrls` carries
`domain` / `home` / `forge` / `matrix` and **no per-agent URL**
(`hive-host-sock/src/lib.rs:503-518`). The `web:<name>` button therefore builds
`<home>agent/<name>/` client-side from `home`, and renders disabled when `home` is
`None` (the hive says so when the dashboard is not reachable from a browser). A
per-agent URL on `HiveUrls` would remove the guess; it is small enough to ask for
if the web terminal survives the section 7 decision, and is deliberately **not**
filed yet — the three hyperhive issues that are filed were each scoped to
something already settled.

Rules for the mirror:

- Every struct tolerates unknown keys, so a hive that grows a field does not break
  the desktop.
- Nothing is mirrored that the rows do not render. `queued_dags`, `nodes`,
  `approvals`, `quota`, the whole matrix half — omitted. A smaller mirror is a
  smaller thing to keep in sync, and the extraction (section 13) lifts it out whole.
- `pending_reminders` is deliberately **not** mirrored: the hive stubs it to `0`
  unconditionally (`hive-c0re/src/server.rs:378-384`), so rendering it would be
  rendering a lie.
- **Drift check.** Today there is no version on the wire — @the-sword-above
  confirmed "zero version/schema field exists anywhere on that struct". It is filed
  as **hyperhive#4038** (Annika, on the ask: "typ ultra preem"). When it lands, the
  client reads it once on connect and, on a major mismatch, renders a single "hive
  protocol vN, plugin speaks vM" row instead of guessing. Until then the drift
  detector is the fixture suite in section 12.

### 5.3 Dialing the socket

- **Path**: `/run/hyperhive/host.sock`, overridable from `agents.toml`
  (`nix/host-modules/hive-c0re/default.nix:390`, `hivectl/src/client.rs:2`).
- **Permissions**: the socket is `0660 root:hive-admin` and its directory `0751`
  (`nix/host-modules/hive-c0re/default.nix:390-398`). It is **always**
  group-accessible — Mara, [#949 10:03Z](https://github.com/vibec0re/trollshell/issues/949#issuecomment-5568952208):
  "host.sock is always group accessible, the adminusers option just adds the users
  to the group." So `services.hyperhive.c0re.adminUsers = [ "annika" ]`
  (`nix/host-modules/hive-c0re/options.nix:419-432`) is group membership, not a mode
  change: no root, no polkit, no `sudo` anywhere in the status or control loop. That
  option's own docs still call it "a real privilege grant … the admin socket is
  _full_ hive control" (`options.nix:429-431`) — see section 11.
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

`Subscribe { kinds }` is item seven on #948's frozen-surface list
([#948 09:24Z](https://github.com/vibec0re/trollshell/issues/948#issuecomment-5568468371)),
and it is the one entry there that does not exist yet. When it lands it replaces
the cadence with a stream and this section collapses to "read events". The shape to
ask for already exists on the HTTP side:
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
  provisioning verbs with approval semantics, and they are moving to the swarm
  control plane anyway (amendment d); the sidebar is a status surface.
- It does not shell out to `hivectl` for status or control — only for attach
  (amendment c). Everything else is the socket.
- It does not read forge, matrix, NATS, approvals, or anything else swarm-level.
- It does not link `hytte-services`, `hytte-ui`, GTK, or any hive crate.
- It does not read or write hive config files.
- It adds no `Control` D-Bus methods (`trollshell/src/control.rs:116-118`) and no
  shell-side module. The one shell change in this epic is **#953**, whose detached
  mode must bypass the awaited `cmd.output()` — not merely re-parent the child —
  because of the 10 s timeout documented in section 11.

### 5.7 The lifetime-ops fork — OPEN

Mara flagged that permissions, agent create/destroy and secret management are
moving from the hive to the swarm control plane, "though there could be a back
channel for local cli control if you need it"
([#947 09:22Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5568440406)).
That makes the transport a real fork, and it is **not this spec's to settle**.

**Correction (12:21Z).** An earlier draft of this section said option (ii) waited
on Mara's central compute node. It does not, and she said so:
"'the swarm control plane once her central node exists' — my central node would
not be annis central node"
([#947 12:21Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5570553019)).
Under `singleHostSwarm` Annika's laptop runs **its own** swarm controller —
`deploy.swarm-controller.enable` is one of the toggles the mode asserts
(`nix/host-modules/local-defaults.nix:36,131`) — so the control plane is local,
on loopback, and buildable today. Section 4's vocabulary said as much a page
earlier; this section had simply not caught up.

What the local controller serves today (`swarm-controller/src/main.rs`):

| route                                    | verb  | line | what it is                                                   |
| ---------------------------------------- | ----- | ---- | ------------------------------------------------------------ |
| `/api/agents`                            | `GET` | 552  | every agent the swarm holds an identity for                  |
| `/api/agents/status`                     | `GET` | 913  | a row per agent — the controller's **own** `AgentStatusRow`  |
| `/api/hives/{hive}/agents/{agent}/state` | `PUT` | 789  | **the write**: declare an agent's wanted state               |
| `/api/hives/{hive}/wanted`               | `GET` | 830  | read the declaration back; the published value is the record |

So the honest table is:

| option                                       | for                                                                                                                                              | against                                                                                                                    |
| -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------- |
| **(i) `host.sock`, the seven verbs**         | exists, stable-ish, and already carries every flag the row renders. Annika's "local hive first" points here. Buildable now against a fake socket | asks Mara to keep a small local override alive past the migration — a promise, and she has not said whether it is a burden |
| **(ii) the local controller's HTTP surface** | where lifetime ops are actually going; loopback, no new hive promise, no dependency on anyone's hardware                                         | no promise of stability either — it is the part "currently moving". And two concrete gaps today, below                     |

Two gaps make (ii) more than a transport swap, both read out of the code rather
than the thread:

- **It cannot express pause.** The declaration vocabulary is a **closed** enum of
  `Up` / `Offline` (`swarm-queue-client/src/wanted.rs:87-94`), and a value the
  build does not know fails the _whole_ document by design — the test that pins
  that rule uses `"paused"` as its example of an invalid state
  (`swarm-queue-client/src/wanted.rs:228-231`). Pause is the one write v1 does
  (section 6.3) and the mandatory first step of attach (section 7), so (ii) needs
  a third state before it can drive the row.
- **Its row is a different shape.** The controller's `AgentStatusRow`
  (`swarm-controller/src/agent_status.rs:32-68`) is a _reported-snapshot_ view —
  `hive`, `freshness`, `last_seen_unix`, `age_seconds`, an opaque `snapshot`,
  `config_pr`, `wanted` — with no `paused`, `failed`, `needs_login`,
  `needs_update`, `parent` or `deployed_sha`. Section 6.2's precedence table maps
  none of it. It does carry the agent's status text inside `snapshot`, which is
  the very thing hyperhive#4037 asks the hive socket for — so on that one field
  (ii) is **ahead** of (i) today.

**Recommendation, conditional:** if Mara answers "controller, now", **(ii)**
becomes the recommendation and #948's frozen list moves from socket verbs to
controller routes (with a pinned `version`, the hyperhive#4038 shape). Absent that
answer, **(i)** — it is the only one that can render and drive the row as
specified today. Either way the local extras (a `choom` session, the agent web
terminal) sit on top, so the loser is a transport swap behind `hive::wire`, not a
rewrite.

**Marked OPEN.** Two questions decide it, both Mara's, both posted on
[#947 12:22Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5570564007)
and unanswered:

1. Is that controller API the control plane she means, and is
   `PUT /api/hives/{hive}/agents/{agent}/state` the write path for start / stop /
   pause — given `AgentState` has no `paused` today?
2. Would she rather the desktop code against it **now** and absorb its churn, than
   keep the `host.sock` override alive?

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
      Button { id: "web:<name>", classes: ["flat"],
               child: Icon { name: "utilities-terminal-symbolic" } },
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

This was the highest-value ask in #948 and is now **filed as hyperhive#4037**
(@the-sword-above, [#948 09:33Z](https://github.com/vibec0re/trollshell/issues/948#issuecomment-5568584669)),
verified against the source before filing: `active_model` **already exists** one
call upstream in `container_view::build_all` and is simply not copied into the row,
which makes the change narrower than described — a copy, not a new read. Until it
lands, row 5 reads "running", Annika's decision 5 is half-honoured, and the plugin
does not invent a substitute (the alternative — dialling every agent's `web.sock`
for its `GetAgentMeta` — is a swarm-shaped dependency this plugin refuses on
principle, section 3).

### 6.3 The four interactions

Four, not three: Mara's two terminals are different tools for different moments
(amendment e), so the row offers both rather than picking one.

| gesture            | button id       | v1 behaviour                                                                                                                                                                                                                                                                                                                                       |
| ------------------ | --------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| click the name     | `attach:<name>` | attach — the direct `choom` session, "for when you want to use claude code directly". Section 7; a detached `RunCommand`, needs #953 and hyperhive#4039                                                                                                                                                                                            |
| click the terminal | `web:<name>`    | open the agent web terminal — Mara's "check on the agents and maybe send them a msg as they do dev work without my direct attention". `Urls` → `home`, from which the plugin builds `<home>agent/<name>/` (section 5.2: `HiveUrls` has no per-agent field), handed to the browser. Disabled when `home` is `None`. Needs nothing new from the hive |
| click pause        | `pause:<name>`  | `SetPaused { name, paused: !paused }` (`hive-host-sock/src/lib.rs:160`); optimistic flip, reconciled by the next poll                                                                                                                                                                                                                              |
| click edit         | `edit:<name>`   | show the agent's config flake and its dispatch target (section 9), read-only, plus the links. **Real editing waits for #952.**                                                                                                                                                                                                                     |

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

Four options. **Nothing here is decided** — Annika reopened tmux on #951 ("tmux
should be up for discussion").

One framing correction from amendment e: options 2 and 3 are **not alternatives**.
Mara uses both, for different moments — "the direct choom session in the container:
this is for when you want to use claude code directly" versus the agent web
terminal, "a claude-code-like web ui with a message history and ability to send
messages, see todos, interrupt the turn and all that jazz … i mostly use the web
thing to check on the agents and maybe send them a msg as they do dev work without
my direct attention" ([#947 09:22Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5568440406)).
The row therefore carries both buttons (section 6.3). What is actually being
decided below is **which one the primary click — the agent's name — does**, and
whether 1 or 4 is wanted at all.

| option                                                  | needs from the hive                                                                             | root?                             | survives shell restart | sees the loop's transcript       | remote-capable               |
| ------------------------------------------------------- | ----------------------------------------------------------------------------------------------- | --------------------------------- | ---------------------- | -------------------------------- | ---------------------------- |
| **1. tmux in the cage**                                 | a `tty` agent kind + a supervised tmux session (#950, new; nothing in the hive uses tmux today) | yes, unless hyperhive#4039        | yes (in the cage)      | no — a second, parallel `claude` | no                           |
| **2. pause + `claude --resume <title>` via `choom`** ⭐ | **hyperhive#4039 only**, and it is filed; `choom --resume` already ships                        | **no**, once hyperhive#4039 lands | yes (own terminal)     | **yes** — same session file      | via `ssh`                    |
| **3. the per-agent web page** (kept regardless)         | nothing — it exists today                                                                       | no                                | yes (a browser)        | rendered stream, not a tty       | **yes**, through the gateway |
| **4. a harness-owned PTY on `web.sock`**                | the real attach primitive (#951 option 2, largest change)                                       | no                                | yes                    | yes                              | yes                          |

**Recommended default for the primary click: option 2** — recommended, not decided.
Option 3 ships alongside it either way.

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
- The one blocker is now filed and endorsed. Mara,
  [#947 09:35Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5568616265):
  "the polkit rules should probably go into hyperhive proper, no?" —
  **hyperhive#4039**, scoped by @the-sword-above to the single action
  `machinectl shell` actually triggers (`org.freedesktop.machine1.shell`; `login`
  and `host-shell` are different verbs `choom` never calls). With it, option 2
  needs nothing further from the hive.
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

Option 3 needs no pause at all — the web terminal is designed to be used while the
loop runs, which is half of why Mara reaches for it — so the `web:<name>` button
never touches `SetPaused`.

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
   the flags today, the text once **hyperhive#4037** lands (section 6.2). The
   plugin never dials an agent's own `web.sock` to get it: that would be a
   per-agent dependency on the swarm-shaped surface section 3 rules out.
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

## 9. Config — three owners, not two

Amendment f split what looked like one question into three. Mara,
[#952 11:32Z](https://github.com/vibec0re/trollshell/issues/952#issuecomment-5569995865):
"each agent has its own config flake, so anything inside the container can just be
confed in the flake (including custom inputs), its just the host level args you
cannot change in a free-form way."

| owner                        | holds                                                                                                                                                                                                                                                                      | who edits it                                                                                                                    |
| ---------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| the agent's **config flake** | everything _inside_ the cage: skill folders, commands, homedir config files, packages, custom flake inputs, and the Claude-Code knobs (`hyperhive.model`, `availableModels`, `effortLevel`, `backendEnvironmentFile` — `nix/agent-modules/agent-service.nix:16,37,65,131`) | a commit to that repo. **No hive change needed for any of it.** The desktop shows it; section 10 asks whether it ever writes it |
| the **hive**, host-level     | the nspawn / `nixos-container` arguments the flake cannot reach: bind mounts, container capabilities, private network, user mapping beyond `hyperhive.user.*`                                                                                                              | #952, and only after Annika names a case a clone cannot cover                                                                   |
| **trollshell**               | how to reach the socket, how often to ask, which terminal to launch, and display labels                                                                                                                                                                                    | `agents.toml`, below                                                                                                            |

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

That is the **whole** trollshell column. Nothing from the other two rows is
duplicated here — the desktop reads them and never keeps a second copy that can
drift.

**"Associated forge project" is a clone plus a task, not a mount.** Mara,
[#952 11:31Z](https://github.com/vibec0re/trollshell/issues/952#issuecomment-5569977099):
"agents typically just clone the repo and file prs directly, but i like the
'dispatch an agent on a repo with a task' workflow." So the row's project field is
"which repo, which task", and if that ever becomes a hive or swarm primitive the
desktop consumes it as-is. The notes/knowledge mount is redundant for the same
reason — "the swarm level forge has a knowledge repo agents get a local copy of
(and can contributen via prs to)". What is left of "mounts" is only what a clone
cannot carry — a large local dataset, a hardware device — and **Annika has not
named one**, which is open question 8.

Mara's own reason for withholding mounts is worth recording, because it outlives
this spec: "until now i did not add this on purpose. i really need the use case
here so i can think about where/how it belongs in hyperhive … having those mounts
would also prevent you from migrating an agent to a different hive"
([#947 09:22Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5568440406)).
A mount pins an agent to one host; a clone does not. She also names the escape
hatch that exists today — "use a network fs and agent config to mount it in the
container" — which is the config-flake row of the table above, not a hive change.

The roster is the hive's, not the file's: `[display.<name>]` sections only decorate
agents the hive reports. A section naming an unknown agent is inert and warned, not
an error — the merge layer's unknown-key rule.

The `agents` subsystem rides `hytte-config`
(`crates/hytte-config/src/subsystem.rs:79-96`), which buys the XDG layering
(`crates/hytte-config/src/xdg.rs:130-156`), the four merge rules including `_unset`
(`crates/hytte-config/src/merge.rs:34-48`), unknown-key warnings rather than
failures, and the format-preserving writer — all from a `NAME` and a `DEFAULT_TOML`.

## 10. Control-center Agents tab (phase 2)

Same shape as the Plugins tab after #887/#943: an `AdwBreakpointBin` over an
`AdwNavigationSplitView` — split panes wide, push navigation narrow, one widget tree
for both, and a detail pane that is _retargeted_ rather than rebuilt so the poll
stays invisible (`crates/trollshell-control-center/src/plugins_tab.rs:10-45`). It is
added to the `ViewStack` next to Plugins, Places and AI Keys
(`crates/trollshell-control-center/src/main.rs:95-118`).

**v1 is read-only.** Per agent: the flags, `deployed_sha`, `parent`, the hive's
`model` / `effortLevel` / user / project as reported, the config-flake location, and
links to the agent page and the config repo. Read-only is not a placeholder — it is
what the hive supports: edits today are a forge PR plus an operator approval
(`ApprovalKind::MergeConfigPr`), with no field-level write.

The write path is now a question about **which owner is being edited** (section 9),
and the earlier draft's rationale is retracted: with
`services.hyperhive.deploy.singleHostSwarm` there **is** a local forge (amendment
a), so "the desktop profile has no forge" — the previous argument for
`SetAgentOptions` — is simply false.

| write path                                       | what it can edit                                                                 | pros                                                                                                                     | cons                                                                                                                                          |
| ------------------------------------------------ | -------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------- |
| **a commit / PR to the agent's config flake** ⭐ | everything in-container: skills, commands, config files, packages, model, effort | the hive's real model, and the flake is already the sanctioned place (amendment f); auditable; **no hive change at all** | a git round trip for a checkbox; on an all-local swarm the approver is the operator approving themselves                                      |
| **`SetAgentOptions` on `host.sock`**             | only the host-level args: mounts, capabilities, user mapping                     | one verb, immediate                                                                                                      | new hive surface (#952); no audit trail; and it is exactly the kind of lifetime/permission op moving to the swarm control plane (amendment d) |

Recommended: **the config-flake path for in-container fields**, which is most of
Annika's original list, with `SetAgentOptions` reserved for the host-level remainder
if #952 ever needs it. Not decided; it is downstream of #952 and of open question 8.

## 11. Trust boundary

The socket is full hive control — "spawn / kill / destroy / deploy"
(`nix/host-modules/hive-c0re/options.nix:429-431`). Four rules follow.

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
membership — group membership only, since the socket is always group-owned
(amendment c): `adminUsers` (`nix/host-modules/hive-c0re/options.nix:419-432`)
populates the group, and the socket's `0660 root:hive-admin` mode
(`nix/host-modules/hive-c0re/default.nix:390-396`) is what enforces it. No root,
no polkit, no `sudo` anywhere in this plugin. A user outside the group gets
`EACCES` and the "no hive" row — the correct unprivileged outcome, not a bug.

**Four: the laptop rule — keep the gateway off the network.** This one is about
the hive's deployment, not the plugin, but it is the security fact that changed
today. Mara, [#949 11:03Z](https://github.com/vibec0re/trollshell/issues/949#issuecomment-5569648120):
"the hive dashboard still is not behind auth. dont open it to the network. a full
port sweep is pending after the swarm work is mostly done. currently the nginx is
running on the host network." On a machine that roams between networks, an
all-local swarm therefore wants the gateway bound to loopback or firewalled on
`:80`/`:443` until that sweep lands. **Nothing on the trollshell side needs those
ports open**: the plugin reaches `host.sock` and `127.0.0.1` and nothing else, and
the agent web terminal (section 7 option 3) is a loopback URL. The browser trusting
the swarm CA is a separate, benign matter — Mara: "i just click through the invalid
cert warning"; `security.pki.certificateFiles` is the tidy version.

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
| a click on `web:<name>` emits **no** `SetPaused` — the web terminal never pauses the loop | plugin reducer test                             |
| `Notify` fires on the flag **edge**, not the level (two identical polls → one toast)      | plugin reducer test                             |
| an absent / refused / permission-denied socket parks and re-polls, never exits            | fake-socket integration test (`system-tests`)   |
| the detached `RunCommand` argv is `systemd-run`-wrapped and never awaited                 | #953, `trollshell/src/plugins/effects.rs` tests |
| a detached child outlives the effect broker being dropped                                 | #953, `system-tests`                            |

The fixtures are the drift detector until hyperhive#4038 lands. They are **recorded
from a live hive**, checked in verbatim, and a hive-side wire change fails them —
deliberately, because a hand-written fixture would only prove the mirror agrees with
itself. Recording them is the one step that needs Annika's machine.

Falsification, per the house rule: each mechanism must have a test that goes red when
the mechanism is deleted. Delete the edge diff in `Notify` and the two-identical-polls
test must fail. Delete the name validation and the attach-argv test must fail. Give
`Scope` a `Default` and the scope test must fail.

**Live-verify** (for `docs/live-verify.md` when this ships) — only Annika's glass can
settle these, and all of them need a hive, i.e. `singleHostSwarm` deployed (#949):
the rows render legibly in the sidebar at her scale; the pause button actually parks
a live agent; `hivectl … choom --resume hive-session` lands in the agent's own
transcript and the loop's next turn sees it; the launched Alacritty survives
`systemctl --user restart trollshell`; the `web:<name>` button opens the agent page
on loopback with the swarm CA warning clicked through; a `needs_login` flip raises
exactly one toast; and the desktop user's `hive-admin` membership alone (no `sudo`)
is enough for every one of them.

## 13. Phases

| phase | what                                                                                                                                                                                                                                        | blocked on                                                                                                            |
| ----- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| P0    | this spec                                                                                                                                                                                                                                   | Annika's veto                                                                                                         |
| P1    | #953 (detached `RunCommand` — the detached path must bypass the awaited `cmd.output()`, not merely re-parent) + `hytte-plugin-agents`: wire mirror, rows, pause, panel, the `web:<name>` button                                             | nothing in-tree; dev against a fake socket. Live-verify needs a hive, i.e. **#949**'s `singleHostSwarm` on the laptop |
| P2    | control-center **Agents** tab, read-only, adaptive drill-down                                                                                                                                                                               | P1                                                                                                                    |
| P3    | attach, per whichever section 7 option wins (rec. option 2). The `web:<name>` button ships in P1 either way                                                                                                                                 | option 2 → **hyperhive#4039** (filed); option 1 or 4 → **#950** as well; option 3 → nothing, it is already in P1      |
| P4    | edit — narrowed by amendment f: the in-container fields already live in the agent's config flake, so this is a **config-flake editor**, not a hive change; the host-level remainder (mounts, caps) waits on **#952** and on open question 8 | **#952** for the host-level half only                                                                                 |
| later | remote hive: #948's gateway HTTP + SSE transport; `Subscribe { kinds }` replacing the poll; the swarm control plane if section 5.7 resolves to (ii); extraction; other runtimes                                                             | **#948**, section 5.7, and decision 6's "once this all stable"                                                        |

P1 is buildable **today** against a fake socket, and that is the point of the fixture
suite: the plugin can be finished, tested and reviewed before a hive exists on the
laptop. It just cannot be _live-verified_ until #949.

## 14. Open questions

Numbered for reply. Two from the first draft are now answered and gone: whether to
join Mara's swarm (no — amendment b) and whether the hive needs a forge-less profile
(no — amendment a, `singleHostSwarm`).

1. **Attach mechanism** — section 7's four options for the primary click: tmux, pause + `claude --resume`, the per-agent web page, or a harness-owned PTY? (Recommendation: option 2; option 3 ships alongside regardless.)
2. **#953 now or later** — still unanswered on #947; P1 cannot ship without it.
3. **The lifetime-ops fork** (section 5.7 — Mara's call, not Annika's) — `host.sock`'s seven verbs, which asks her to keep a local override alive, or the **local** swarm controller's HTTP surface, which is where lifetime ops are going but cannot express `paused` today? If she answers "controller, now", that becomes the recommendation; otherwise `host.sock`. Both questions posted on [#947 12:22Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5570564007), unanswered.
4. **Edit surface** — the plugin's own panel (the #487 groove) or a control-center tab, once editing is real? Both eventually, but which first?
5. **Plugin name** — `agents`, `hive`, or `choom`? It becomes the crate name, the unit name (`trollshell-plugin-<id>`) and the `plugins.<id>` key, so it is awkward to change later.
6. **Is `trollshell-choom` the first agent?** (Which hive is settled: Annika's own all-local swarm on the laptop.)
7. **Unpause after attach: automatic or manual?** Auto-_pause_ is not open — section 7 settles it for option 2 (pause-first, always; never refuse-to-attach). What is open is the other end: does the plugin unpause by itself when the terminal exits — which needs #953 to surface the transient unit's exit without re-parenting the child — or is v1 honest and manual, unpaused by the row's own pause button with the row reading `paused · attached` until then?
8. **Is there anything you need in the cage that a `git clone` cannot bring in?** Mara's question, relayed on #952: if not, mounts leave the requirement entirely and P4 shrinks to the config flake.
9. **Notify policy** — toast on `needs_login` and `failed` only, or also on a harness status the config marks "waiting for you", once hyperhive#4037 puts the text on the row?

## 15. References

- hyperhive checkout: `/home/annika/viberoot/hyperhive` (every hive pointer above);
  the same content is rendered at `https://hyperhive.darkest.space/docs/`.
- Host socket protocol: `hive-host-sock/src/lib.rs`, `hive-host-sock/README.md`.
  The frozen subset the desktop needs is enumerated on
  [#948 09:24Z](https://github.com/vibec0re/trollshell/issues/948#issuecomment-5568468371).
- Agent status row: `hive-sh4re/src/container.rs:33-66`; its projection at
  `hive-c0re/src/server.rs:367-388`.
- The all-local deployment mode: `services.hyperhive.deploy.singleHostSwarm`,
  `nix/host-modules/local-defaults.nix:26`; the laptop checklist lives on #949.
  It asserts the local swarm controller on at `local-defaults.nix:36,131`.
- The local swarm controller (section 5.7 option ii): routes at
  `swarm-controller/src/main.rs:552,789,830,913`; its own row shape at
  `swarm-controller/src/agent_status.rs:32-68`; the closed `Up` / `Offline`
  declaration vocabulary — and the test that uses `"paused"` as its invalid
  example — at `swarm-queue-client/src/wanted.rs:87-94,228-231`.
- Attach today: `docs/tools/hivectl.md:213-268`.
- Session identity: `docs/turn-loop/claude-invocation.md:4-9,72-87`.
- Event push: `hive-c0re/src/dashboard_events.rs:13-15`,
  `hive-c0re/src/dashboard/state_snapshot.rs:668,687`.
- Hyperhive issues filed from these threads (internal forge, not publicly
  linkable): **#4037** status text on the row, **#4038** a schema version on
  `HostResponse`, **#4039** the `org.freedesktop.machine1.shell` polkit rule.
- Plugin vocabulary: `crates/hytte-plugin-proto/src/{wire.rs,effect.rs,manifest.rs}`.
- The two-hats precedent: `crates/hytte-plugin-infobroker/src/plugin.rs`,
  `crates/hytte-claude-bridge/src/main.rs:444-470`.
- Launcher and control surface: `trollshell/src/plugin_launcher.rs`,
  `trollshell/src/control.rs`, `nix/module-common.nix:608`.
- Config layering: `crates/hytte-config/src/{subsystem.rs,xdg.rs,merge.rs}`.
- The canonical design's non-goals:
  `docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md:23-28`.
