# The agentic desktop: hyperhive-backed agent rows, attach, and the Agents tab

**Date:** 2026-09-07 (amended the same day — see section 2.1)
**Status:** Proposed — this is the veto window. No code is written before Annika has read it.
**Issues:** #947 (epic), #948 (the contract), #949 (the all-local deployment mode on a laptop), #950 (attach), #951 (non-root attach), #952 (per-agent cage options), #953 (detached `RunCommand`)
**Hyperhive issues:** hyperhive#4037 (**landed**), hyperhive#4038, hyperhive#4039 — filed by @the-sword-above on hyperhive's internal forge, not publicly linkable

## 1. Summary

A sidebar plugin renders one row per hyperhive agent —
`(icon) name (status-icon)` over `status (pause) (edit)`, grouped by multi-repo
project — and a click opens that agent's **chat surface in its own window**. The
hive is the backend; trollshell is a client. A terminal into the cage is the
secondary action, not the primary one (section 7).

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

Recorded from the #947 / #950 / #951 / #952 threads as decisions, not questions.
Decisions 8–12 arrived in the evening, after the hive-side amendments in 2.1, and
they are the ones that fixed the shape of section 7:

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
7. **tmux is up for discussion** (#951) — **superseded by decision 9**: an
   interactive terminal turned out not to be the requirement at all.
8. **The use case, in her words** ([#947 17:14Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5573834527)):

   > - Project structure: `/home/annika/<multi-repo-workspace>/<project-repos, usually in git, managed in github / gitlab forge>`
   > - Want able to start choom in a container and manage via sidebar plugin.
   > - Maybe grouped by `<multi-repo-project>`
   > - Haz right now up to 8 Chooms in one `<multi-repo-project>`, all manually spawned in tmux 😭
   > - Fine if they run in same container - as long as projects distinct - so claudes can communicate.
   > - We can e.g. use some hytte datasource plugin to trigger agent run: Eg: received gitlab email (hytte-plugin-inbox-imap) -> matches criteria -> "/idd" -> stdin(relevant choom)

   Mapped onto hyperhive's unit (Mara's model, #952), which is what
   decision 9 confirms fits:
   - **a choom = one hive agent** — its own container, its own config repo on
     the local forge, cloning the project repo. Not a tmux pane.
   - **"up to 8 Chooms in one project" = 8 agents**, grouped in the sidebar by
     the multi-repo workspace their project repo sits under (section 6.1).
     "so claudes can communicate" is the **broker**, not a shared shell — which
     is better, because it survives a restart and is addressable by name.
   - **triggers** — the IMAP → `/idd` idea is a broker `send` to the right
     agent, not stdin (section 6.6).
   - **`viberoot` stays her own dev cage** (`nixos/containers/viberoot.nix` in
     her nixos repo — an ephemeral container with `~/viberoot` bind-mounted and
     `claude-code` + `tmux` inside). Hive agents get their own containers; this
     spec does not touch it.

9. **A local forge is fine, and the terminal is not the point.**
   [#947 17:21Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5573896006):
   "oooh ok the config repos! Then ok local forge fine <3 … I'm not too invested
   in the terminal as chat client for agent soooo I mean custom gtk client I'm
   also game - as long as runs out of trollshell process". That settles section 7
   on **the agent's chat surface, in its own process** — not a terminal — and it
   is why the tmux fork briefly opened at 17:17Z was withdrawn at 17:23Z. The
   forge it accepts runs in its own persistent `hive-forge` nixos-container on
   the hive host (`nix/host-modules/hive-forge/default.nix:107-110,634-636`:
   `ephemeral = false`, state under
   `/var/lib/nixos-containers/hive-forge/var/lib/forgejo/`), so the all-local
   laptop mode is her `viberoot`-style containers plus one more.
10. **Notifications with options.**
    [#947 17:24Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5573917144):
    "hyperhive integration in trollshell will be ultra preem!! Typ notify user
    with options and stuff." Section 6.5 is that: hive approvals raised as an
    interactive consent prompt on the desktop.
11. **Feature parity is a hard constraint on the chat surface.**
    [#947 17:25Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5573926876):
    "well the gtk chat client would pretty much need to fully support the current
    hyperhive webui features ❤️". This is what rules a native GTK
    reimplementation out of v1 — see section 7.
12. **The companion embeds a WebView, on WebKitGTK.**
    [#947 17:31Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5573975653):
    "Ok :) preem thx choom <3 then let's do it like this 💯" — answering the
    WebView proposal, and her own Servo question one comment earlier. v1 is
    `webkitgtk_6_0`; Servo is a documented re-check, not a someday, and the engine
    sits behind a build feature so trying it costs nothing elsewhere. Section 7.1
    carries the reasoning and the pointers.

### 2.1 Amendments from the hive side (2026-09-07, 09:22Z–17:34Z)

The first draft of this spec (07:51Z) predated @kaesaecracker (Mara, a hyperhive
dev) and @the-sword-above joining the threads. Their input moved ten things.
Annika's decisions 1–7 above are **unchanged** and 8–11 came later; these constrain
how all of them are met, and retract five things earlier drafts got wrong: a
forge-less hive profile (a), the reason behind section 10's recommendation (a), the
claim that the swarm control plane waits on Mara's hardware (h), the claim that it
cannot express pause (i), and a proposal to abandon hyperhive for tmux (j).

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
  the fork it opened is **section 5.7, and it is now settled** (amendment i).
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

  | issue                                        | ask                                                                  | verified detail                                                                                                                                                                                                      |
  | -------------------------------------------- | -------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
  | hyperhive#4037 (**landed**, see amendment i) | `status_text` / `status_set_at` / `active_model` on `AgentStatusRow` | `active_model` **already existed** one call upstream in `container_view::build_all`, just uncopied — which is why this one shipped the same day                                                                      |
  | hyperhive#4038                               | a schema / `version` field on `HostResponse`                         | zero version or schema field exists on that struct today                                                                                                                                                             |
  | hyperhive#4039                               | a polkit rule granting `hive-admin` the action `choom` triggers      | `machinectl shell <name>@h-<name>` triggers **exactly one** action, `org.freedesktop.machine1.shell`; `login` and `host-shell` are different verbs `choom` never calls. hyperhive has zero `security.polkit.*` today |

- **h. The swarm control plane is _local_, not Mara's.** Mara,
  [#947 12:21Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5570553019):
  "'the swarm control plane once her central node exists' — my central node would
  not be annis central node." An earlier draft had option (ii) of the lifetime-ops
  fork blocked on her pending hardware; it never was. `singleHostSwarm` asserts
  `deploy.swarm-controller.enable`
  (`nix/host-modules/local-defaults.nix:36,131`), so Annika's laptop runs its own
  controller on loopback and the fork is a live choice today.
- **i. The fork is settled — `host.sock` now, the controller as the migration.**
  Mara, [#947 15:27Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5572796356):
  "everything will be controlled by swarm controller at some point, for now my
  understanding was to use host.sock which we just added the agent status to."
  Section 5.7 is closed accordingly, and two things follow. First,
  **hyperhive#4037 has landed** — `AgentStatusRow` on hyperhive `origin/main` now
  carries `status_text`, `status_set_at` and `active_model`
  (`hive-sh4re/src/container.rs:73,80,85`), so section 6.2's status line comes
  from `AgentStatus` with nothing pending. Second, a **retraction**: the amendment-h
  draft claimed the controller "cannot express pause". Mara,
  [#947 15:16Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5572677568):
  "swarm-queue-client is the swarm-hive com, not the host.sock". It was a category
  error, not a gap — see section 5.7.
- **j. A tmux backend was proposed and withdrawn inside ten minutes.** When
  Annika's use case landed at 17:14Z it read as "eight interactive chooms per
  project, in one container, on my own workspace dirs" — which fits hyperhive's
  unit badly (8 config repos, a mount #952 does not have, a tty agent kind #950
  does not have, and multi-agent-per-container the hive does not do), so at
  [17:17Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5573859194)
  I proposed dropping the hive for tmux inside her existing `viberoot` container.
  Her 17:21Z reply ("ok the config repos! Then ok local forge fine … not too
  invested in the terminal as chat client") removed the premise — the interactive
  terminal was never the requirement — and the proposal was
  [withdrawn at 17:23Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5573911134).
  **hyperhive stays; nothing in sections 5, 5.7 or 9 changed.** Recorded because
  the reasoning is worth keeping: if she ever does want N interactive chooms
  sharing one cage, that is a different product than this spec, and the tmux path
  is where it starts.

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
  it **today** (amendment i settles that it reads `host.sock` now and migrates to
  the controller later). On Annika's laptop the swarm and the hive are the same box, via
  `services.hyperhive.deploy.singleHostSwarm`
  (`nix/host-modules/local-defaults.nix:26`), and the forge and matrix exist for the
  agents rather than for her — project work stays on GitHub with a token.
- **the (swarm) controller** — `swarm-controller`, the swarm's HTTP control plane.
  `singleHostSwarm` asserts it on
  (`deploy.swarm-controller.enable`, `nix/host-modules/local-defaults.nix:36,131`),
  so on Annika's laptop it is a **local** service on loopback, not Mara's
  infrastructure. "Everything will be controlled by swarm controller at some
  point" (Mara, 15:27Z), so it is where this plugin migrates — but not yet, and
  section 5.7 says why.
- **`trollshell-choom`** — the working name, from Annika's mock on #947, for an
  agent that maintains trollshell from a cage. It is the spec's running example of
  a row; whether it is actually the **first** agent to exist is open question 3.
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
`Spawn`, `RequestSpawn`, `Destroy`, `Matrix*`, `Forge*`, `Gateway*`, `Quota*`,
`SetResourceLimits`, `SetParent`, `Rebuild` — which is what keeps a local override
cheap while lifetime ops move (amendment d, section 5.7). This spec does not
re-derive that list; it implements it.

**One addition to relay to #948**, from Annika's decision 10 (notify with
options): `Pending` / `Approve { id }` / `Deny { id }` were on that comment's
"never needed" line, and they are needed after all — section 6.5 turns hive
approvals into a desktop consent prompt. Ten verbs, not seven. Nothing else on
the list moves.

One in-tree module, `hive::wire`, mirroring only what the rows need — and covering
**every** verb and field a later section relies on, so nothing in sections 6, 7, 9
or 10 reaches for something this table does not carry:

| in-tree type     | mirrors                                                                                                  | fields used                                                                                                                                                                | used by                                                                                                              |
| ---------------- | -------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `Request`        | `HostRequest` (`hive-host-sock/src/lib.rs:117-119`, `#[serde(tag = "cmd")]`)                             | `List`, `AgentStatus`, `SetPaused { name, paused }`, `Start { scope }`, `Stop { scope, graceful }`, `Restart { name }`, `Urls`, `Pending`, `Approve { id }`, `Deny { id }` | the rows and the panel; `Urls` backs the chat companion's URL; the last three back the approval prompt (section 6.5) |
| `Scope`          | `LifecycleScope` (`hive-host-sock/src/lib.rs:468-485`)                                                   | `agent_names` only — see section 11                                                                                                                                        | the panel's per-agent start / stop                                                                                   |
| `Response`       | `HostResponse` (`hive-host-sock/src/lib.rs:521-569`)                                                     | `ok`, `error`, `agents`, `agent_statuses`, `urls`, `approvals`                                                                                                             | every request                                                                                                        |
| `AgentStatusRow` | `hive_sh4re::container::AgentStatusRow` (`hive-sh4re/src/container.rs:34-86` on hyperhive `origin/main`) | `name`, `running`, `failed`, `needs_update`, `needs_login`, `paused`, `parent`, `deployed_sha`, **`status_text`**, `status_set_at`, `active_model`                         | section 6.2's precedence and its status line; `parent` and `deployed_sha` are panel- and tab-only (sections 6.4, 10) |
| `Approval`       | `hive_sh4re::approvals::Approval` (`hive-sh4re/src/approvals.rs:14-34`)                                  | `id`, `agent`, `kind`, `requested_at`, and the free-text description                                                                                                       | section 6.5's prompt strings                                                                                         |
| `HiveUrls`       | `HiveUrls` (`hive-host-sock/src/lib.rs:503-518`)                                                         | `home` (and `domain` as the fallback)                                                                                                                                      | the agent-page URL, **derived** — see below                                                                          |

**One derivation, stated so nobody assumes otherwise:** `HiveUrls` carries
`domain` / `home` / `forge` / `matrix` and **no per-agent URL**
(`hive-host-sock/src/lib.rs:503-518`). The plugin therefore builds
`<home>agent/<name>/` client-side from `home`, hands that to the chat companion,
and greys the row's primary click when `home` is `None` (the hive says so when the
dashboard is not reachable from a browser). **This matters more after decision 9
than it did before** — the derived URL is now the primary interaction, not a
secondary button. With section 7.1 decided, this is now **an ask to file**: a
per-agent page URL on `HiveUrls` (or on the `AgentStatus` row) so the desktop stops
guessing a path scheme it does not own. It is scoped, non-breaking and settled,
which is the bar the other three hyperhive issues met.

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

### 5.7 The lifetime-ops fork — SETTLED (Mara, 15:27Z)

Mara flagged that permissions, agent create/destroy and secret management are
moving from the hive to the swarm control plane
([#947 09:22Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5568440406)),
which made the transport a real question for a while. It is now answered
([#947 15:27Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5572796356)):

> everything will be controlled by swarm controller at some point, for now my
> understanding was to use host.sock which we just added the agent status to

**So: `host.sock` now, the swarm controller as the migration.** The desktop
implements #948's seven verbs against the socket, exactly as sections 5.2–5.4
describe, and the controller becomes a transport swap behind `hive::wire` when
"at some point" arrives — not a rewrite, because the plugin's model is already
the row, not the wire. The migration's read surface is already there when it is
wanted: `GET /api/agents` and `GET /api/agents/status`
(`swarm-controller/src/main.rs:552,913`), served on loopback by the controller
`singleHostSwarm` runs on Annika's own laptop (section 4).

**A correction, and it was mine.** An earlier draft of this section argued that
the controller "cannot express pause", because `AgentState` is a closed
`Up` / `Offline` enum. That was a category error, and Mara said so —
"swarm-queue-client is the swarm-hive com, not the host.sock"
([#947 15:16Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5572677568)).
The `wanted` document is the **controller→hive** channel, not a client surface:
`/api/hives/{hive}/wanted` is a **`GET`** on the controller
(`swarm-controller/src/main.rs:829-830`), and nobody outside the swarm writes it.
And pause was never a swarm concept at all — it is the hive-local
`/harness/paused` marker, created and removed by `Coordinator::set_paused` for
`hivectl agent <name> pause|resume` and the dashboard toggle
(`docs/agent-lifecycle/persistence.md:234-251`), which is precisely what
`SetPaused` writes. There was no gap in the controller to report.

Nothing else in this spec changes as a result. Section 6.3's pause button, section
7's pause-before-attach, and section 11's scope rule were all written against
`host.sock` from the first draft.

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
      Button { id: "chat:<name>", classes: ["flat", "ts-agent-name"],
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
  … one per agent, under a Label group header per project (section 6.3)
]}
```

The `choom:<name>` secondary action (section 7.3) is a **context-menu** entry, not
a fourth button — the row is two lines and already carries three targets. The
vocabulary has no context-menu node today, so v1 renders it as a fourth `Button`
in the panel (section 6.4) rather than on the row, and a real per-row menu waits
for a vocabulary addition nobody has asked for yet.

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

Row 5's "harness's text" was the one field missing from the wire when this spec
was first written. **It has since landed** — hyperhive#4037 is merged, and
`AgentStatusRow` on hyperhive `origin/main` now carries `status_text`
(`hive-sh4re/src/container.rs:80`), `status_set_at` (`:85`) and `active_model`
(`:73`) alongside the flags. Mara, closing the transport question:
"host.sock which we just added the agent status to"
([#947 15:27Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5572796356)).

So Annika's decision 5 is honoured in full from `AgentStatus` alone: the row's
second line is `status_text` verbatim, and the plugin never dials an agent's own
`web.sock` for it. Two properties of the new field the row must respect, both
stated in its own doc comment:

- It is `None` when unset **or when the container is not running** — a stopped
  agent's on-disk status is a stale snapshot from before the stop, and the hive
  applies the same `read_agent_status_live` rule every other reader gets. So
  precedence rows 1–4 already cover every case where the text is absent, and row
  5 falls back to "running" only when a running agent has set no status.
- `status_set_at` is RFC 3339 UTC and is `None` exactly when `status_text` is.
  The panel (section 6.4) renders it as an age; the row does not — a timestamp on
  a two-line card is noise.

`active_model` is available for the panel and the Agents tab (sections 6.4, 10);
the row does not show it, for the same reason.

### 6.3 The four interactions

The primary click opens the chat surface (decision 9, section 7); the terminal
demotes to a context-menu entry. Mara's two surfaces both survive (amendment e) —
they just swapped places once Annika said the terminal was not the point.

| gesture        | button id      | v1 behaviour                                                                                                                                                          |
| -------------- | -------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| click the name | `chat:<name>`  | **primary** — open the agent's chat surface in its own window (section 7). A detached `RunCommand` launching the companion binary; needs #953. Never pauses the loop. |
| click pause    | `pause:<name>` | `SetPaused { name, paused: !paused }` (`hive-host-sock/src/lib.rs:160`); optimistic flip, reconciled by the next poll                                                 |
| click edit     | `edit:<name>`  | show the agent's config flake and its dispatch target (section 9), read-only, plus the links. **Real editing waits for #952.**                                        |
| context menu   | `choom:<name>` | **secondary** — pause, then `choom --resume` in a terminal (section 7.3). A context-menu entry, not a button: it is the rare path                                     |

Grouping (decision 8): rows are grouped by **multi-repo project** — the workspace
directory the agent's project repo sits under — with the group name as a header
row. An agent whose project is unknown falls into an "ungrouped" group rather than
being hidden. With one project the header is suppressed and the list looks exactly
as it does today.

Pause is the one write the rows do, and it is the safest one in the vocabulary:
the hive's own docs describe it as "a single marker write … applies immediately and
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

### 6.5 Approvals — "notify user with options and stuff"

Annika, [#947 17:24Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5573917144):
"hyperhive integration in trollshell will be ultra preem!! Typ notify user with
options and stuff." The hive already has exactly one thing shaped like that: the
approval queue. An agent proposing a config change parks an approval, and today
the only way to answer it is `hivectl` or the dashboard.

The desktop already has the matching primitive — #487 phase 1b's interactive
consent prompt. So the two are wired together with **no new vocabulary on either
side**:

```text
poll  Pending                              (hive-host-sock/src/lib.rs:228-229)
  → for each new approval id, the plugin emits
    Effect::RequestConsent { request_id, agent, datasource, scope, detail }
                                           (crates/hytte-plugin-proto/src/effect.rs:135-141)
  → the shell raises the prompt on the focused output, four choices
  → HostMsg::ConsentDecision comes back keyed by the same request_id
                                           (crates/hytte-plugin-proto/src/msg.rs:122-124)
  → the plugin sends Approve { id }  or  Deny { id }
                                           (hive-host-sock/src/lib.rs:230-233)
```

The prompt's strings come from the `Approval` itself — `agent`, `kind`,
`requested_at` and the free-text description
(`hive-sh4re/src/approvals.rs:14-34`) — so the host learns no hive domain, which
is the rule `RequestConsent` was designed around.

**One mapping decision, and it is deliberately lossy.** `ConsentDecision` has four
variants — `AllowOnce`, `AllowSession`, `AllowAlways`, `Deny`
(`crates/hytte-plugin-proto/src/effect.rs:242-251`) — but a hive approval is a
one-shot on a specific id. So **every `Allow*` maps to `Approve { id }` for that
one id**, and the plugin persists **no standing grant**. "Approve every future
config PR from this agent" is not something a consent prompt should be able to
grant, and the `Deny` timeout (60 s → deny) is the right default for an approval
too: an unanswered prompt leaves the approval pending, which is what it already
was.

An approval that disappears from `Pending` between the prompt and the answer (the
operator used `hivectl`, or another prompt won) is dropped with a debug line, not
an error — the same idempotence the pause button has.

### 6.6 Triggers — a datasource plugin waking an agent

Decision 8's last line: "received gitlab email (hytte-plugin-inbox-imap) → matches
criteria → `/idd` → stdin(relevant choom)". The shape is right; the delivery is
not stdin. A hive agent has no stdin an outsider can reach, and it does not need
one: the harness already wakes on **inbox messages through the broker**, which is
the same path a sibling agent uses to talk to it, and messages queue unacked while
an agent is paused rather than being lost.

So a trigger is: a datasource plugin (the #487 groove — its own binary, its own
socket, no shell change) matches an event and sends `/idd` to the named agent as a
broker message. The desktop's side of that is one plugin and no new shell surface.

**Deliberately not specified here:** which verb sends it. Sending into an agent's
inbox from outside the hive is not something this spec has verified a route for —
`POST /send` on the agent's own web socket is one candidate (section 7.2), a
`host.sock` verb would be another and does not exist today. That is a phase-4
question and belongs on #948 when it is actually wanted, not a guess in this
document.

## 7. Attach — DECIDED: the agent's own chat surface, out of process

**The question #950 asked is answered, and it was the wrong question.** It offered
four ways to open a _terminal_. Annika, [#947 17:21Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5573896006):

> thing is I'm not too invested in the terminal as chat client for agent soooo
> I mean custom gtk client I'm also game - as long as runs out of trollshell process

So the primary click opens the agent's **chat surface**, in its **own process**,
and a terminal is the secondary action. That is #950's option 3 — the per-agent
web UI — promoted from "kept alongside" to the answer. tmux (option 1) and a
harness-owned PTY (option 4) are **considered and dropped**: both exist to give a
better terminal, and a terminal is not what she wants. They stay only as a note
here in case a future use case revives them.

Consequences, in order:

- **hyperhive#4039 (the `machinectl shell` polkit rule) is no longer on the
  critical path.** It gates only the secondary action, so P1 and P3 no longer wait
  on it. It is still worth landing — the secondary action is real.
- `terminal = ["alacritty", "-e"]` stays in `agents.toml`, for that secondary
  action alone.
- The pause-before-attach dance (below) applies only to the `choom` path. The chat
  surface is designed to be used **while the loop runs** — that is the whole point
  of it — so the primary click never touches `SetPaused`.

### 7.1 The v1 shape — DECIDED: an embedded WebView, WebKitGTK

Annika added one hard constraint at
[#947 17:25Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5573926876):

> well the gtk chat client would pretty much need to fully support the current
> hyperhive webui features ❤️

That rules out a native GTK reimplementation for v1. The agent page already has
message history with tool-call rendering, the composer, interrupt, the inbox and
todos flyouts, model and effort pickers, ctx and cost badges, login flow, and a
stats page (`docs/web-ui/agent.md`) — and it keeps moving. Reimplementing that in
GTK widgets is a parity treadmill from day one, and it would be trollshell's job
to keep up with hyperhive's frontend forever.

**So instead** ([#947 17:26Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5573931084)),
and it satisfies "runs out of trollshell process" exactly:

> a small companion binary (control-center shape) that is a libadwaita window
> **embedding the agent's own web page** — WebKitGTK `WebView` pointed at
> `/agent/<name>/` on the local gateway, the swarm CA trusted programmatically so
> there is no click-through.

| property        | what it gives                                                                                                                                   |
| --------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| own process     | the `trollshell-control-center` shape: a separate windowed GTK4/libadwaita binary, launched **detached** per #953                               |
| own window      | its own title and app-id, so niri window rules can place and size it                                                                            |
| feature parity  | free and permanent — it _is_ the web UI, so decision 11 is met by construction                                                                  |
| dependency cost | WebKitGTK is heavy, and it lands **only in the companion**. The shell never links it, exactly as it never links GTK's web stack today           |
| engine          | `webkitgtk_6_0` via the `webkit6` gtk-rs crate 0.6.1, behind a **build feature** so a second engine can be tried without touching anything else |
| the CA          | the swarm CA is trusted programmatically in the WebView rather than clicked through (Mara's browser workflow, #949)                             |

**Decided.** Annika, [#947 17:31Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5573975653):
"Ok :) preem thx choom <3 then let's do it like this 💯".

**Engine for v1: WebKitGTK** — `webkitgtk_6_0` in nixpkgs, driven through the
`webkit6` gtk-rs crate 0.6.1 (2026-03): a versioned GTK4 `WebView` widget with no
coverage questions about the agent page.

**Servo is a re-check, not a someday.** Annika asked
([#947 17:28Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5573946212)):
"is servo ready for this?" — the answer, with pointers, is on
[#947 17:34Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5573996643):
`servo` 0.5.0 is a normal crate on crates.io with a `WebView` API modelled on
WebKitGTK's, but **embedding is the unfinished part** (the "improve the embedding
API" tracking issue servo/servo#27579 has been open since 2020; the only real GTK4
integration, `servo-gtk`, runs Servo in a **subprocess** by re-executing your
binary rather than in-process into a `GtkGLArea`, and its author's own verdict was
"not yet ready for production"; Verso, the project built to be the embeddable
WebView, was archived 2025-10), and **CSS Grid is the known web-platform gap**
(servo/servo#34479). Re-evaluate in **6–12 months** — the day a maintained crate
offers true in-process `GtkGLArea` rendering with input wired up, or #27579 closes
with a documented contract. The companion therefore keeps its engine behind a
**build feature**, so Servo can be tried without touching the plugin, the row, or
anything else in this spec.

Native pieces can replace parts of the embedded page later, one at a time, if a
reason appears — a keyboard shortcut the web UI cannot bind, a notification the
window should raise. That is an option the WebView keeps open; it is not a plan.

### 7.2 If a native piece is ever wanted — the API it would speak

Kept as a **note, not a plan.** The agent's HTTP surface is reachable without the
gateway: `hivectl agent <name> watch` already dials
`/run/hive-agent/<name>/web.sock` directly and speaks bare HTTP/1.1 for the SSE
stream (`docs/tools/hivectl.md:276-285`), which is the precedent an external
client would follow. The socket is `0666` inside a `0751` per-agent dir owned by
the agent's container uid (`docs/networking/gateway.md:107-109`).

Endpoints a native client would need, all verified in `docs/web-ui/agent.md`:

| purpose            | endpoint                                       | line |
| ------------------ | ---------------------------------------------- | ---- |
| history on load    | `GET /events/history` (replay buffer)          | 190  |
| live tail          | `GET /events/stream` (SSE)                     | 191  |
| send a message     | `POST /send`                                   | 281  |
| interrupt the turn | `POST /api/cancel` (SIGINT the in-flight turn) | 284  |
| cold-load snapshot | `GET /api/state`                               | 331  |
| todos flyout       | `GET /api/todos`, `POST /api/todos/mark-done`  | 340  |
| model / effort     | `POST /api/model`, `POST /api/effort`          | 291  |

**One pointer I could not verify:** an earlier draft of #950 cited `/api/op-send`
as the prompt endpoint. It does not appear anywhere in `docs/web-ui/agent.md`;
the send endpoint is `POST /send` (`:281`). Treat the earlier reference as wrong.

### 7.3 The secondary action — `choom` in a terminal

Unchanged from the previous draft, and demoted to a context-menu entry on the row:
for the rare "drive Claude Code myself" moment.

`hivectl agent <name> choom` already runs an interactive claude in the cage and
already takes `--resume <value>`, passed through to `claude --resume`
(`docs/tools/hivectl.md:213-245`). The harness's session title is a knowable
constant — `hive-session`, or `HIVE_SESSION_TITLE`
(`docs/turn-loop/claude-invocation.md:72-77`) — so `choom --resume hive-session`
lands the operator in the agent's own transcript, and `choom` reproduces the
harness's environment: the agent user, the state dir as cwd, and `--settings` /
`--mcp-config` / `--system-prompt-file` (`docs/tools/hivectl.md:247-268`).

Because the title is constant, resuming it **does** collide with a running turn —
the case the hive's own note excludes for a _blank_ choom ("it won't carry our
title", `docs/turn-loop/claude-invocation.md:83-84`). So this path is
**pause-first, always**:

```text
SetPaused { name, paused: true }   →  wait one poll for paused == true
RunCommand { detached: true, argv: [<terminal…>, "hivectl", "agent", <name>,
                                    "choom", "--resume", <title>] }
on EffectResult (launch ok)        →  the row shows "paused · attached"
on terminal exit                   →  SetPaused { name, paused: false }
```

Unpause-on-exit is the piece #953 must not lose. A detached launch reports launch
success only, not exit status (#953's own proposal), so v1 unpauses on the
operator's next click of the pause button and the row makes that state obvious —
open question 4.

### 7.4 Considered and dropped

**tmux in the cage** (#950 option 1) and **a harness-owned PTY on `web.sock`**
(#950 option 3's heavier sibling) both existed to make the _terminal_ better.
Decision 9 removed the terminal from the primary path, so both drop out: tmux
needs a `tty` agent kind the hive does not have and would run a second `claude`
nobody drives, and the PTY is the largest hive change of the four for a surface
the web UI already provides. Neither is refuted — they are simply answering a
question that is no longer being asked.

## 8. Status source

In Annika's order, and only these:

1. **The harness**, via `AgentStatusRow` plus the free-text `set_status` string —
   both on the row today, since **hyperhive#4037 landed** (section 6.2). The
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
terminal = ["alacritty", "-e"]        # the section 7.3 secondary action only
session_title = "hive-session"        # matches HIVE_SESSION_TITLE on the hive

[display.trollshell-choom]
label = "choom"
icon = "starred-symbolic"
project = "viberoot"                  # the group header this row sits under

# The chat companion's own prefs live here too, so one file is the whole
# desktop-side surface and the companion works while the shell is down —
# the same rule the Places tab follows for places.toml.
[chat]
width = 1100
height = 800
last_agent = "trollshell-choom"       # reopened by default when launched bare
```

The `attach = "choom-resume"` key from the previous draft is **gone**: section 7
settles which surface the primary click opens, so a config key choosing between
them would only let the file contradict the spec. What stays configurable is the
terminal for the secondary action.

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
named one**, which is open question 5.

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

## 10. Control-center Agents tab (phase 4)

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
if #952 ever needs it. Not decided; it is downstream of #952 and of open question 5.

## 11. Trust boundary

The socket is full hive control — "spawn / kill / destroy / deploy"
(`nix/host-modules/hive-c0re/options.nix:429-431`). Five rules follow.

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

**Three: the approval prompt needs `Capability::Consent`, and grants nothing
standing.** Section 6.5's flow means the plugin declares `Capability::Consent`
(`crates/hytte-plugin-proto/src/manifest.rs:133`) on top of `RunCommand`, `Notify`
and `OpenPage` — and that capability is also the #305 gate for receiving the
`ConsentDecision` push at all, so declaring it is not optional decoration. Two
constraints on how it is used: the plugin **never persists a grant** (every
`Allow*` is one `Approve { id }`, section 6.5), and the prompt's strings are
echoed from the hive's own `Approval` record rather than composed from anything
the plugin invents — a prompt that misdescribes what it is approving is the one
way this surface could do real harm. The 60 s timeout resolving to `Deny` leaves
the approval **pending**, never denied on the hive.

**Four: no secrets cross the plugin.** It holds no token, declares no secret slot,
and reads no credential. Its entire authority is the desktop user's `hive-admin`
membership — group membership only, since the socket is always group-owned
(amendment c): `adminUsers` (`nix/host-modules/hive-c0re/options.nix:419-432`)
populates the group, and the socket's `0660 root:hive-admin` mode
(`nix/host-modules/hive-c0re/default.nix:390-396`) is what enforces it. No root,
no polkit, no `sudo` anywhere in this plugin. A user outside the group gets
`EACCES` and the "no hive" row — the correct unprivileged outcome, not a bug.

**Five: the laptop rule — keep the gateway off the network.** This one is about
the hive's deployment, not the plugin, but it is the security fact that changed
today. Mara, [#949 11:03Z](https://github.com/vibec0re/trollshell/issues/949#issuecomment-5569648120):
"the hive dashboard still is not behind auth. dont open it to the network. a full
port sweep is pending after the swarm work is mostly done. currently the nginx is
running on the host network." On a machine that roams between networks, an
all-local swarm therefore wants the gateway bound to loopback or firewalled on
`:80`/`:443` until that sweep lands. **Nothing on the trollshell side needs those
ports open**: the plugin reaches `host.sock` and `127.0.0.1` and nothing else, and
the chat companion (section 7.1) points at the **same loopback gateway** — which is
why closing `:80`/`:443` to the network costs the desktop nothing at all. The
swarm CA is the one place the companion differs from a browser: rather than
Mara's "i just click through the invalid cert warning", it trusts the CA
programmatically in the WebView, so there is no click-through to train the
operator out of. `security.pki.certificateFiles` remains the host-wide tidy
version.

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

| check                                                                                                        | where                                           |
| ------------------------------------------------------------------------------------------------------------ | ----------------------------------------------- |
| `Request` serializes to the exact `{"cmd":"agent_status"}` line shape, per verb                              | `hive::wire` unit test                          |
| recorded real `HostResponse` JSON lines decode into the mirror (fixture files)                               | `hive::wire` fixture test                       |
| a response carrying **unknown** fields still decodes (forward drift)                                         | `hive::wire` fixture test                       |
| every `Start` / `Stop` frame the plugin can emit has a non-empty `agent_names` (rule 1)                      | `hive::wire` unit test                          |
| flags → (icon, text, class) for all 32 flag combinations, precedence pinned                                  | plugin unit test                                |
| `View` render-tree goldens: unreachable, empty hive, running, paused, failed, needs-login                    | plugin golden test                              |
| a click on `pause:<name>` emits exactly one `SetPaused` with the flipped bool                                | plugin reducer test                             |
| the `choom` argv is the fixed shape, and a rejected name never reaches it                                    | plugin reducer test                             |
| a click on `chat:<name>` emits **no** `SetPaused` — the chat surface never pauses the loop                   | plugin reducer test                             |
| a new `Pending` row raises exactly one `RequestConsent`, and a repeat poll of the same id raises none        | plugin reducer test                             |
| every `ConsentDecision::Allow*` maps to `Approve { id }` and persists no grant; `Deny` maps to `Deny { id }` | plugin reducer test                             |
| an approval that vanishes from `Pending` before the decision arrives is dropped, not errored                 | plugin reducer test                             |
| rows group by project, and an agent with no known project lands in "ungrouped" rather than vanishing         | `View` golden test                              |
| `Notify` fires on the flag **edge**, not the level (two identical polls → one toast)                         | plugin reducer test                             |
| an absent / refused / permission-denied socket parks and re-polls, never exits                               | fake-socket integration test (`system-tests`)   |
| the detached `RunCommand` argv is `systemd-run`-wrapped and never awaited                                    | #953, `trollshell/src/plugins/effects.rs` tests |
| a detached child outlives the effect broker being dropped                                                    | #953, `system-tests`                            |

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
`systemctl --user restart trollshell`; the chat companion opens the agent page on
loopback **with no certificate warning** (the CA trusted programmatically, section
7.1) and survives a shell restart the same way; a `needs_login` flip raises exactly
one toast; a real `Pending` approval raises the consent prompt with strings that
correctly describe what is being approved, and answering it actually resolves the
approval hive-side; and the desktop user's `hive-admin` membership alone (no
`sudo`) is enough for every one of them.

## 13. Phases

| phase | what                                                                                                                                                                                                                                                                                                      | blocked on                                                                                                                                                                                                                                 |
| ----- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| P0    | this spec                                                                                                                                                                                                                                                                                                 | Annika's veto                                                                                                                                                                                                                              |
| P1    | #953 (detached `RunCommand` — the detached path must bypass the awaited `cmd.output()`, not merely re-parent) + `hytte-plugin-agents`: wire mirror, rows grouped by project, pause, panel                                                                                                                 | nothing. The transport is settled (`host.sock`, section 5.7) and the status text has landed (hyperhive#4037), so P1 is dev-against-a-fake-socket today; only **live-verify** needs a hive, i.e. **#949**'s `singleHostSwarm` on the laptop |
| P2    | **the chat companion** (section 7.1) — the out-of-process window the primary click opens. Moved ahead of the Agents tab because it is the interaction Annika actually asked for; the tab is a settings surface                                                                                            | **#953** for the detached launch. Engine and shape are settled (section 7.1)                                                                                                                                                               |
| P3    | **approvals** (section 6.5) — poll `Pending`, raise the consent prompt, route `Approve` / `Deny` back                                                                                                                                                                                                     | P1. Deliberately **not** in P1: the row must be trustworthy before it is allowed to raise a modal that approves a config change, and the consent prompt is the one surface here that can do harm if it misdescribes what it is asking      |
| P4    | control-center **Agents** tab, read-only, adaptive drill-down                                                                                                                                                                                                                                             | P1                                                                                                                                                                                                                                         |
| P5    | edit — narrowed by amendment f: the in-container fields already live in the agent's config flake, so this is a **config-flake editor**, not a hive change; the host-level remainder (mounts, caps) waits on **#952** and on open question 5                                                               | **#952** for the host-level half only                                                                                                                                                                                                      |
| later | triggers (section 6.6 — a datasource plugin sending `/idd` through the broker; the send route is unspecified on purpose); the swarm-controller migration, "at some point" (section 5.7); remote hive over #948's gateway HTTP + SSE; `Subscribe { kinds }` replacing the poll; extraction; other runtimes | **#948**, Mara's "at some point", and decision 6's "once this all stable"                                                                                                                                                                  |

The `choom` secondary action (section 7.3) rides along with P2, since it is one
more entry in the same window's context menu — and it is the only thing in this
plan that wants **hyperhive#4039**, which is why nothing above blocks on it.

P1 is buildable **today** against a fake socket, and that is the point of the
fixture suite: the plugin can be finished, tested and reviewed before a hive exists
on the laptop. It just cannot be _live-verified_ until #949.

## 14. Open questions

Numbered for reply, and **all six are Annika's** — the hive side is done. Five that
earlier drafts listed are answered and gone: whether to join Mara's swarm (no —
amendment b), whether the hive needs a forge-less profile (no — amendment a), the
lifetime-ops transport (`host.sock` now, controller later — amendment i), the
attach mechanism (the chat surface, decision 9), and the engine behind it
(WebKitGTK — section 7.1, "let's do it like this 💯").

1. **#953 now or later** — still unanswered on #947; P1 cannot ship without it.
2. **Plugin name** — `agents`, `hive`, or `choom`? It becomes the crate name, the unit name (`trollshell-plugin-<id>`) and the `plugins.<id>` key, so it is awkward to change later. The companion binary needs a name too (`trollshell-agent-chat`?).
3. **Is `trollshell-choom` the first agent?** (Which hive is settled: Annika's own all-local swarm on the laptop.)
4. **Unpause after the `choom` path: automatic or manual?** Auto-_pause_ is not open — section 7.3 settles it. What is open is the other end: does the plugin unpause by itself when the terminal exits — which needs #953 to surface the transient unit's exit without re-parenting the child — or is v1 honest and manual, unpaused by the row's own pause button with the row reading `paused · attached` until then?
5. **Is there anything you need in the cage that a `git clone` cannot bring in?** Mara's question, relayed on #952: if not, mounts leave the requirement entirely and P5 shrinks to the config flake.
6. **Notify policy** — toast on `needs_login` and `failed` only, or also on a `status_text` the config marks "waiting for you", now that the text is on the row? (Distinct from section 6.5's approval prompt, which is a modal with buttons, not a toast.)

**Marked later, not asked now:** whether the chat companion should also reach
agents on a _remote_ hive through the gateway. It would — the URL is the only
thing that changes — but the remote-hive phase is behind #948's HTTP transport
and there is no reason to decide it before then.

## 15. References

- hyperhive checkout: `/home/annika/viberoot/hyperhive` (every hive pointer above);
  the same content is rendered at `https://hyperhive.darkest.space/docs/`.
- The chat companion's engine (section 7.1): `webkitgtk_6_0` in nixpkgs, the
  `webkit6` gtk-rs crate 0.6.1. Servo's state as of mid-2026, with links, is on
  [#947 17:34Z](https://github.com/vibec0re/trollshell/issues/947#issuecomment-5573996643)
  — `servo` 0.5.0 on crates.io, embedding-API tracking issue servo/servo#27579
  (open since 2020), CSS Grid gap servo/servo#34479, `servo-gtk`'s subprocess
  approach, Verso archived 2025-10.
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
