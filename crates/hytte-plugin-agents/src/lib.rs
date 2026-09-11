//! `hytte-plugin-agents` — hyperhive agents as trollshell sidebar rows
//! (issue #947, phase P1; spec
//! `docs/superpowers/specs/2026-09-07-agentic-desktop-design.md`).
//!
//! One **pill** per hyperhive agent, two lines and nothing else —
//! `[icon] [Name] [Model]` with `[start|stop] [edit]` on the right, over the
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
//! | §6.1's row is `(icon) name (status) (pause) (chevron)` + details in place | `[icon] [Name] [Model]` + `[start\|stop] [edit]`, status glyph on line 2, no chevron and no unfold | her mock; the unfolded detail is the drawer page now |
//! | §6.1's row click opens something | the row is **not** a click target at all | her click target is #950's `WebView`, which does not exist yet; opening the drawer instead would train the wrong surface |
//! | §6.4's page is "the selected agent's **full** detail" | the same pill, the flags, the agent's link — no `deployed` / `parent` / `status set` | the three rows she named; `model` survives as line 1's chip with the full id on its hover |
//!
//! Two things the v1 spec asks for are **not** in this crate and cannot be:
//! the row click's destination is #950, and the edit button's destination is
//! [#1010](https://github.com/vibec0re/trollshell/issues/1010)'s modal dialog —
//! a decision about which host surface a plugin page mounts on, still waiting
//! on one answer from Annika (modal to the shell, or to the session). The edit
//! button emits `OpenPage(PluginSelf)` either way, so it needs no change when
//! that lands.
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
//! - **approvals** (spec §6.5) — phase P3, and deliberately so: the row must
//!   be trustworthy before it is allowed to raise a modal that approves a
//!   config change. The plugin therefore declares no
//!   [`Capability::Consent`](hytte_plugin::proto::Capability::Consent).
//! - **triggers** (§6.6) and the **control-center Agents tab** (§10).
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

pub use plugin::{Agents, PLUGIN_ID};
