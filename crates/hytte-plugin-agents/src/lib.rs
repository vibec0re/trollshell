//! `hytte-plugin-agents` — hyperhive agents as trollshell sidebar rows
//! (issue #947, phases P1 and P3; spec
//! `docs/superpowers/specs/2026-09-07-agentic-desktop-design.md`).
//!
//! One **pill** per hyperhive agent, two lines and nothing else —
//! `[icon] [Name] [Model]` with `[start|stop]` on the right, over the
//! harness's own status line — grouped by multi-repo project, plus a drawer
//! page carrying the same pill for one agent, its flags, its link, and the
//! hive overview with the full roster. The hive is the backend; trollshell is
//! a client, and the hive stays the single source of truth for agent state
//! (the system-daemon-as-state-store rule,
//! `docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md:95`).
//!
//! # The spec rows this deviates from, and why (§6.1 / §6.4)
//!
//! **The card is Annika's, not §6.1's.** She respecified it on
//! [#963](https://github.com/vibec0re/trollshell/pull/963) on 2026-09-11 after
//! @kaesaecracker's second round of live screenshots — *"the view still looks
//! very cluttered … Maybe we should try one card / pill per agent … deployed…
//! parent… agent page — too much information to display! Let's keep this slick
//! two lines"* — and her mock is now the contract. `view.rs`'s module doc
//! carries it verbatim along with the table of what each removed thing became.
//! Three consequences worth naming here:
//!
//! | spec row | what ships | why |
//! | --- | --- | --- |
//! | §6.1's row is `(icon) name (status) (pause) (chevron)` + details in place | `[icon] [Name] [Model]` + `[start\|stop]`, status glyph on line 2, no chevron and no unfold | her mock; the unfolded detail was the drawer page, until #1282 item 3 retired its only door |
//! | §6.1's row click opens something | the row is **not** a click target at all | her click target is #950's `WebView`, which does not exist yet; opening the drawer instead would train the wrong surface |
//! | §6.4's page is "the selected agent's **full** detail" | the same pill, the flags, the agent's link — no `deployed` / `parent` / `status set` | the three rows she named; `model` survives as line 1's chip with the full id on its hover |
//!
//! **The v1 destination is [#950](https://github.com/vibec0re/trollshell/issues/950),
//! and it is not in this crate.** Annika settled it on #947 (2026-09-11
//! 07:43Z): the agent's companion window — a `WebKitGTK` view in a window the
//! shell owns, one per agent, with chrome that reads `host.sock` directly — is
//! the single surface for an agent. The **row click** opens it on the agent
//! page; the window's own settings tab is reached inside it. This crate's
//! drawer page was the placeholder for that settings tab, reached from the
//! pill's **edit button**, until #950 shipped the window; since
//! [#1282](https://github.com/vibec0re/trollshell/issues/1282) item 3 retired
//! that button, the drawer page is no longer a Settings placeholder for
//! anything — its one remaining door is the panel roster's own name button
//! (`select:`), which goes straight to it, unconditionally, rather than
//! trying the window first the way the pen did.
//!
//! [#1010](https://github.com/vibec0re/trollshell/issues/1010) shipped the
//! host's routing rule for the destination that *is* still `OpenPage`: this
//! crate's **in-shell page** (the title row's hive overview) opens in the
//! centered dialog rather than the drawer, because this card is
//! `Mount::Sidebar*` — with no change here, since the host routes on the mount
//! it already knew. What #1010 does **not** govern is the #950 `RunCommand`
//! route above: a separate GTK window is not a page.
//!
//! # What it links, and what it does not
//!
//! An out-of-process [`hytte_plugin`] widget in the `hytte-plugin-infobroker`
//! shape (spec §5.1): the hive client starts from
//! [`Plugin::sources`](hytte_plugin::Plugin::sources) and dies with the
//! session. It links **no hyperhive crate** — this workspace has no git
//! dependencies (#757) and hyperhive publishes no client crate yet (#948), so
//! [`hive::wire`] mirrors the protocol in-tree. It links no `hytte-services`,
//! no `hytte-ui`, no GTK. It adds **no shell-side surface at all**: no
//! `Control` D-Bus method, no service, no module.
//!
//! # Scope: one hive, never a swarm
//!
//! The plugin talks to the local hive's `host.sock` and to nothing else.
//! Forge, matrix, NATS and the swarm controller are swarm-level concerns it
//! never touches; agent create and destroy are out entirely (spec §3, §5.6).
//! `hivectl` appears nowhere in the status or control loop.
//!
//! # What P1 is, and what it is not
//!
//! P1 is the row: the wire mirror ([`hive`]), the poll ([`poll`]), the status
//! precedence and the grouping ([`model`]), the tree ([`view`]), the reducer
//! ([`plugin`]) and `agents.toml` ([`config`]). Deliberately **not** P1:
//!
//! - **attach / the chat companion** (spec §7) — phase P2, and it needs #953's
//!   detached `RunCommand`. Annika's v1 puts the agent's page in the
//!   `trollshell-webview` companion ([#950](https://github.com/vibec0re/trollshell/issues/950))
//!   instead, on the **row click**; until that exists the row answers to
//!   nothing and the two buttons are the whole interaction. The `agent page`
//!   link on the drawer page opens the same URL in the browser through
//!   [`Effect::OpenUri`](hytte_plugin::proto::Effect::OpenUri)
//!   ([#1045](https://github.com/vibec0re/trollshell/issues/1045)), which is
//!   the fallback once the `WebView` is there and the only way to follow it
//!   until then.
//! - **triggers** (§6.6) and the **control-center Agents tab** (§10).
//!
//! # Approvals (spec §6.5) — phase P3, and the one thing here that writes
//!
//! The hive's approval queue is polled on the same tick as the roster
//! ([`poll`]), and a newly-queued approval raises the shell's **consent
//! prompt** through
//! [`Effect::RequestConsent`](hytte_plugin::proto::Effect::RequestConsent) with
//! the two-button card
//! ([`ConsentChoices::Approval`](hytte_plugin::proto::ConsentChoices::Approval)):
//! Approve / Deny, because "This session" and "Always" are not answers to a
//! one-shot config merge. The human's answer goes back as exactly one
//! `Approve { id }` or `Deny { id }` — §6.5's deliberately lossy rule, where
//! **every** affirmative is a single approve and **no standing grant is ever
//! persisted**, which this plugin achieves by having nowhere to persist one.
//!
//! Three rules are worth stating here because they are what make a modal that
//! merges a config change acceptable at all:
//!
//! 1. **It prompts once.** An approval raises one card and then waits, until
//!    it leaves the queue or the operator asks again. A hive with one
//!    unanswered approval does not raise a modal every two seconds.
//! 2. **Silence decides nothing.** A card nobody answers — the 60 s timeout,
//!    `Esc`, no output to draw on — sends the hive *nothing*. The approval
//!    stays exactly as it was, the row keeps a badge counting what it is
//!    waiting on, and clicking the badge re-raises the oldest. Deny is only
//!    ever a click.
//! 3. **The queue is the truth.** A decision for an approval that left
//!    `Pending` in the meantime is dropped with a debug line, never re-applied.
//!
//! This is why [`Capability::Consent`](hytte_plugin::proto::Capability::Consent)
//! was withheld through P1 and P2: the row had to be trustworthy first
//! (spec §13).
//!
//! # Testing without a hive
//!
//! Everything here is testable against a **fake `host.sock`** — a real
//! `UnixListener` in a tempdir speaking the recorded JSON lines under
//! `tests/fixtures/` — which is what makes P1 buildable, testable and
//! reviewable before a hive exists on the laptop. Only *live-verify* needs
//! one (#949).

pub mod config;
pub mod hive;
pub mod model;
pub mod plugin;
pub mod poll;
pub mod view;
pub mod window;

pub use plugin::{Agents, PLUGIN_ID};
