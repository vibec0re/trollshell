# Choom init — working `vibec0re/trollshell`

Hi choom <3 You're joining work on **`vibec0re/trollshell`** (GitHub is the forge). This is the
onboarding "init": everything we've learned working this repo that _isn't_ obvious from the tree.
Read the repo's own [`CLAUDE.md`](../CLAUDE.md) first — it's the source of truth for architecture and
build. This doc is the **how we work + the landmines**. When the two disagree, the code wins; verify
before asserting.

> Before anything else, **orient yourself live**: `gh pr list` and `gh issue list`, then read the
> _full_ latest thread on whatever you're about to touch. Threads move fast and several chooms work
> this repo at once (§4) — a snapshot in this doc would be stale by the time you read it.

---

## 0. The one rule that gates everything

**You must build inside the Nix devShell.** `.envrc` is `use flake` (direnv). If direnv isn't
active, run `nix develop` first. Outside it the build _panics_ ("a libclang shared library is not
loaded") and icons render as `image-missing`. `nix` needs flakes per-invocation:
`--extra-experimental-features 'nix-command flakes'`.

NixOS box, not Arch. `nix` only — no `pacman`. Rust/Python aren't on PATH except through nix
(see the root `CLAUDE.md` "Conventions" for the exact `nix shell` incantations).

## 1. The gates (a violation FAILS the build, not just lint)

- `unsafe_code = "forbid"` workspace-wide. Only `hytte-ecal` (FFI) overrides it.
- clippy `all` **and** `pedantic` at `deny` — code must be pedantic-clean or `cargo check` fails.
- `disallowed_methods`: raw `zbus::Connection::session/::system` are **banned** — all D-Bus goes
  through `hytte-bus` primitives.
- **Format with `nix fmt`, NOT `cargo fmt`.** `cargo fmt` silently skips feature-gated files
  (`#![cfg(feature = "system-tests")]`) and never touches `.nix` files — but CI's treefmt checks
  every file on disk. A diff that passes local `cargo fmt --check` can still fail CI formatting.
  `nix fmt <paths>` is the safe superset; run it on every changed file before committing.
- **CI does NOT run clippy.** CI (`nix flake check`) = treefmt + module-eval + the EDS nixosTest +
  `cargo test`. A PR can be CI-green and clippy-dirty. The local
  `cargo clippy --workspace --all-targets` is the _only_ clippy enforcement — always run it.
  (For fast iteration `cargo clippy -p <crate> --lib` beats the full workspace gate.)

Quickref:

```sh
cargo build --release -p trollshell
cargo run -p trollshell                 # needs a LIVE Niri session ($NIRI_SOCKET)
cargo test                              # hermetic internals only
cargo test --workspace --features system-tests   # + dbus-daemon/display tests (xvfb-run for display)
cargo clippy --workspace --all-targets  # THE clippy gate
nix fmt                                 # THE format gate
```

`trollshell` is a real Wayland shell — running it meaningfully needs a live Niri session + system
daemons. Most of what we build (UI, sensors, D-Bus clients) **cannot be verified in CI or even
headlessly** — that shapes the whole workflow below.

## 2. How we work this repo (the loop)

- **Issue-driven development.** Design questions, mockups, option trade-offs, and decisions go in
  **GitHub issue comments** — NOT chat prompts. Annika explicitly wants the issue thread to be the
  source of truth where collaborators (esp. Mara) weigh in. Don't use chat A/B questions for
  issue-scoped design choices; post them on the issue.
- **Triage house style:** a `## Triage` comment from the bot account, with `###` subheads,
  `file:line` refs in backticks, a bold **Severity** (low/medium/high), a concrete fix-direction,
  and an `@`-mention + clarifying question to the reporter when root cause needs confirming. Posting
  triage comments and **follow-up/breakdown issues is pre-authorized** — no need to ask first (be
  judicious; only break down genuinely large work; reference the parent; `gh issue list` first to
  avoid duplicating one a concurrent choom just filed).
- **Self-triaged issues:** an issue the _bot_ authored with full root-cause analysis in its body is
  already triaged — don't post a redundant `## Triage` restating it. Only human-filed issues
  (annikahannig / kaesaecracker) need one.
- **Never auto-merge to `main`.** That's Annika's call. Open the PR, flag what needs live-verify,
  let the PO test and report.
- **Fan out WIDE.** Annika actively wants aggressive parallelism ("why not more than two agents?").
  When genuinely-independent work exists, don't run a serial queue. Read-only research/review/
  scoping agents are _free_ (no worktree, no build) — spawn them liberally in parallel.
- **Sweep for unanswered questions every idle pass.** Check EVERY issue + recently-closed, by
  _latest-comment author_ (never by timestamp — a reviewer question can predate your wake). If the
  last comment is from `annikahannig` or `kaesaecracker` and it's a question / request / live-test
  result, it's unanswered → replying is top priority, even on issues you weren't tracking. A
  PO/reporter question left hanging is the highest-cost miss.

## 3. Build-ready vs HOLD (the most important judgement call)

**Default: build it.** Almost all trollshell work is UI/sensor/D-Bus that CI can't visually verify —
if "can't verify in CI" were a hold reason, nothing would ship. The proven loop is: build the
triaged-with-a-clear-fix issue → **adversarially self-review the diff** → open the PR → **flag the
live-verify** for Annika/Mara. They live-test and report. That's the loop, not a blocker. An issue
is buildable now if it's triaged with a clear direction even when it needs live verification, or a
non-blocking clarifying question is open but a robust fix covers the likely case.

**The LIMIT — genuinely HOLD when the design is an active dispute.** A `## Triage` comment existing
does **not** make an issue build-ready. Right before you start building a freshly-triaged issue,
**re-read the FULL latest thread** (not just the triage + your own claim). HOLD if the reporter/PO
has (a) pushed back on the _approach_, (b) said "no impl yet" / "didn't get feedback", (c) escalated
to Annika for a decision, or (d) a concurrent choom already agreed to hold. A brand-new issue with
the reporter mid-conversation is _more_ likely contested, not less. Building into a live design
dispute reads as incoherent ("you said hold, then you shipped") and gets parked. Also genuinely hold
when untriaged, or when a fix would be _wrong_ without the answer.

**Idle wake with a blocked/drained backlog?** Don't just grow backoff — **adversarially self-review
the open PRs whose correctness CI can't catch** (D-Bus clients, GTK/Wayland layout, anything flagged
"needs live verify"). Fan out one read-only reviewer per high-risk PR; feed it the diff + full new
files; ask for HIGH-CONFIDENCE correctness bugs only (logic, wrong API sig, panics on real data,
broken incremental-update) with `file:line` + a minimal fix — not style/clippy nits. Push confirmed
fixes as one commit to the _same_ branch (never a new PR, never merge). This has caught a CRITICAL +
multiple HIGH bugs in code that was clippy-clean, unit-green, and already pushed — before the PO
wasted a live-test round-trip.

## 4. You are NOT alone — concurrency discipline

Multiple choom contexts act on this repo **at the same time**, sharing one bot gh identity, so their
work is indistinguishable by author. This bites in specific ways — guard against all of them:

- **Read the FULL current thread immediately before acting.** A sweep is a point-in-time snapshot; a
  concurrent choom can answer in the seconds between your sweep and your action. If a bot reply
  already landed after the human's comment, it's handled — don't double-handle.
- **`gh issue list` immediately before `gh issue create`.** Another context may have just filed the
  same follow-up. Duplicates fragment discussion; the senior issue wins. When closing-as-dup,
  re-fetch the _other_ issue's state right before acting and verify exactly one of the pair stays
  open (two contexts can close BOTH in opposite directions).
- **Check the working tree before ANY edit/checkout/commit.** The primary worktree is **not always
  clean `main`** — a concurrent context can be mid-build there (dirty files, HEAD on a stray feature
  branch, no PR yet). Wake-start survey is read-only (`gh`, `git status/log/fetch`). If
  `etc/…`/`assets/…`/`src/…` is half-done or HEAD is on a feature branch with dirty files, a
  concurrent choom owns it → hands off; don't build that issue or touch those files. A reflexive
  `git checkout` / `worktree remove --force` **silently discards** their uncommitted work.
- **Stay in your lane.** Triage context owns `## Triage` comments + untriaged issues; the implementer
  owns building _triaged_ issues + replying on its _own_ PRs. Don't act on an untriaged issue.
- **`origin/main` moves mid-session.** Re-`git fetch` during a wake — a concurrent PR (or Annika)
  can merge while you work.

## 5. Fan-out / worktree hygiene (if you spawn build worktrees)

- **ff local `main` FIRST, before any fan-out:** `git fetch origin -q && git merge --ff-only
origin/main`. Agent worktrees fork the _primary repo's LOCAL main_, not `origin/main` — stale
  local main makes agents branch off old code and "re-fix" already-merged work, or silently
  conflict/revert.
- **Cap parallel build worktrees ~3.** Each carries a multi-GB `target` (the shared-`CARGO_TARGET_DIR`
  export doesn't reliably stick); disk runs hot. Reap with `git worktree remove --force` right after
  the PR pushes. **Before force-removing any worktree you didn't just create:** check
  `git status --porcelain` + `git log <branch> ^main` for unique/uncommitted work — force-remove
  discards it silently.
- After reaping, `git worktree list` → confirm primary is back on clean `[main]`; if it landed on a
  stray local branch, `git checkout main && git merge --ff-only origin/main` and delete the stray
  (verify `git log origin/main..<name>` is empty first — origin + the PR are untouched).
- Concurrency cap on parallel agents ≈ `min(16, cores−2)`.

## 6. PR etiquette with an active PO

If you EXTEND an already-open, green+mergeable PR with more commits while Annika is in an active
session, she can hit merge _between_ your commits and land a PARTIAL PR (this happened — the
load-bearing commit got orphaned, cost a confusing round-trip). When adding load-bearing scope to a
mergeable PR: prefer a **separate PR**, or mark the PR **draft** before pushing and flip to ready
when complete, or post a one-line "hold the merge — one more commit coming". After a PO merges a
multi-commit PR, **verify which commits actually landed** (`git merge-base --is-ancestor <sha> main`)
before telling them it's done.

## 7. Technical landmines (type-check clean, render/run wrong — CI can't catch these)

- **niri blurs the FULL layer-shell surface geometry**, not the painted content. A fullscreen
  surface holding a small card frosts the _whole screen_; a persistent never-unmapped surface leaves
  a lingering frosted strip. This is the root cause behind every frosted-glass regression. Don't
  chase CSS alpha — start from surface geometry. niri 26.04 ships `ext-background-effect-v1` with
  `set_blur_region(wl_region)` to scope blur to a sub-rect. `etc/niri/blur.kdl` is **docs, not
  loaded** (niri has no `include`) — the live rules are hand-merged into Annika's nixos `config.kdl`.
- **`adw::PreferencesGroup::add` routes by widget type:** a `GtkListBoxRow`/`AdwActionRow`/etc. goes
  _into_ the boxed list (interleaved, in order); **any non-row child (bare `gtk::Label`/`gtk::Box`)
  renders BELOW the entire list.** It compiles fine; the bug is render-only. To interleave, the child
  MUST be a `GtkListBoxRow` subclass. For a slim row without the `AdwActionRow` header-box floor, use
  a bare `gtk::ListBoxRow` + `set_child`. trollshell uses `PreferencesGroup` heavily (calendar,
  tasks, network).
- **`gtk::Label::set_lines(n)` does NOT cap hard `\n` paragraphs** — only wrapping _within_ one
  paragraph. Text with embedded newlines (descriptions, notes) renders every line regardless.
  Pre-clamp in Rust: `text.lines().take(n)` before handing it to the label.
- General rule: any change only the _renderer_ (not the type system / clippy / CI) can validate →
  adversarially self-review against the actual library behavior before un-drafting.

## 8. How a change reaches Annika's machine (deployment)

`trollshell` is a **flake input** in her NixOS config (`github:vibec0re/trollshell`, tracks the
default branch). A source change (even CSS) reaches her system only after **merge to `main` →
`nix flake update trollshell` → `nixos-rebuild switch`**. A feature branch alone won't deploy — so
"merge it and I'll test" is a real round-trip; get the PR right first. niri is a **static
`config.kdl`** deployed by home-manager (editing `~/.config/niri/config.kdl` directly gets clobbered
on rebuild); niri layer-rules (incl. blur) deploy with the same rebuild. The `etc/` dir holds the
full session config the shell expects (systemd user units, keybinds, kanshi, PAM file) — see
`etc/README.md`. The idle → dim → lock → suspend pipeline is native (in-process; #204 retired swayidle).

## 9. People & references

- **Annika** (`annikahannig`) — the PO / shell owner. Decisions, live-testing, final merges. She
  wants autonomy + aggressive parallelism + issue-thread decisions. ("choom" = friend/collaborator.)
- **Mara** (`kaesaecracker`) — PO/reporter, files many issues, often live-tests. Author of
  **nova-shell** (`https://forge.darkest.space/mara/nova-shell.git`, publicly clonable, no auth) — a
  related niri-targeted GTK Wayland shell. When Mara says "check how nova-shell did it", clone it and
  cite the actual `file:line` rather than guessing (good refs so far: outside-click panel dismissal,
  Intel GPU RC6-residency reading, idle-inhibit).
- **`vibechoom`** — the shared bot gh identity all choom contexts post under.
- Design intent lives in `docs/superpowers/specs/` (the why) + `docs/superpowers/plans/` (the how) —
  consult before changing a subsystem. Canonical design:
  `docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md`.

---

_Welcome aboard, choom. Build inside the devShell, `nix fmt` + workspace clippy before every push,
read the full thread before you act, fan out wide, never auto-merge, and flag the live-verify._ <3
