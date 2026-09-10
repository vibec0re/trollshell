//! `hytte-plugin-agents` — hyperhive agents as trollshell sidebar rows
//! (issue #947, phase P1; spec
//! `docs/superpowers/specs/2026-09-07-agentic-desktop-design.md`).
//!
//! One row per hyperhive agent — `(icon) name (status-icon) (pause)
//! (chevron)` over the harness's own status line, grouped by multi-repo
//! project, with the chevron unfolding that agent's details **inside the
//! card** — plus a drawer page carrying the hive overview, the full roster and
//! the selected agent's flags, deployment and per-agent start/stop. The hive
//! is the
//! backend; trollshell is a client, and the hive stays the single source of
//! truth for agent state (the system-daemon-as-state-store rule,
//! `docs/superpowers/specs/2026-04-24-hytte-trollshell-design.md:95`).
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
//!   detached `RunCommand`. P1's primary click opens this plugin's own panel
//!   instead; what does not change across that swap is that it never touches
//!   `SetPaused`.
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
