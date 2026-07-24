//! `hytte-plugin-infobroker` — the consent-gated data broker for local AI agents
//! (issue #487, phase 1a).
//!
//! Annika's framing (settled on the #487 thread): agent bridges are a **skill
//! folder** (a `SKILL.md` + a `bin/` CLI), *not* an MCP server; auth is a
//! **CLI-minted session token passed via the environment**; the status/config
//! UI **rides a plugin panel**. This crate is that broker, shipped as two
//! binaries over one shared library:
//!
//! - **`hytte-plugin-infobroker`** (`src/plugin.rs`) — a normal out-of-process
//!   [`hytte_plugin`] widget: a bar chip + its own drawer [`panel`] (the
//!   #349/#415 `View { tree, panel }` mechanism) listing grants, datasource
//!   status, and a recent-requests audit trail, with per-row revoke/allow
//!   buttons. It ALSO runs the broker socket server ([`broker::serve`]).
//! - **`hytte-infobroker`** (`src/cli.rs`) — the only client of the broker
//!   socket; the binary the skill folder's agents shell out to (`auth` → env
//!   token → scoped `get`s).
//!
//! The library is deliberately **SDK-free** (it never links `hytte_plugin`), so
//! the CLI stays lean and the pure logic — the grant store, the token TTL
//! machine, the wire parse, and the consent decisions — is hermetically
//! unit-testable without a socket, a host, or a wall clock.
//!
//! # The two halves (issue #487's core distinction)
//!
//! - **Grants are durable** ([`grants`]): a TOML store keyed
//!   `(agent × datasource × scope)` → `always`/`deny`, surviving restarts.
//! - **Tokens are ephemeral** ([`tokens`]): in-memory bearer "session cookies",
//!   12 h TTL, killed by an explicit revoke or a broker/shell restart.
//!
//! Auth mints a token **silently** when an `always` grant covers the agent, else
//! denies with a how-to-grant hint and an informational `Effect::Notify` toast
//! so the human sees the knock. Interactive Allow/Deny prompting is **phase 1b**
//! (the shell-side consent overlay, post-#419); this crate makes **zero
//! shell-side changes**.
//!
//! # The one datasource
//!
//! [`departures`] is a one-shot, consent-gated fetch of the next N catchable
//! S-Bahn departures — an acknowledged code dup of `hytte-plugin-departures`'
//! fetch path, to be deduped in phase 2 once the proto grows a `Datasource`
//! capability.

pub mod broker;
pub mod departures;
pub mod grants;
pub mod paths;
pub mod tokens;
pub mod wire;

pub use broker::{BrokerMsg, BrokerSnapshot, Cmd, Toast, serve};
