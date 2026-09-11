//! `trollshell-agent-window` — the per-agent **companion window**
//! ([#950](https://github.com/vibec0re/trollshell/issues/950), phase P2 of
//! [#947](https://github.com/vibec0re/trollshell/issues/947); spec
//! `docs/superpowers/specs/2026-09-07-agentic-desktop-design.md` §7).
//!
//! **Our chrome, their page.** The body is a `WebKitGTK` view of hyperhive's own
//! per-agent page with `?hide=header,input` appended; everything around it —
//! the header with the agent's icon, name, short model word and the **live
//! status read from `host.sock`**, start/stop/pause, and a settings tab — is
//! ours, and reads the hive directly. Annika settled the shape on #947
//! (2026-09-11 07:16Z): a "dedicated, shell-controlled webview, not the
//! browser — we control it: buttons, status, settings", and at 07:43Z that the
//! card's pen opens this window on its settings tab, so an agent has **one**
//! surface.
//!
//! It is the `trollshell-control-center` shape: a separate windowed
//! GTK4/libadwaita binary, **never linked into the shell**, launched out of
//! process by the agents plugin through #953's detached `RunCommand` so it
//! outlives a `trollshell.service` restart. `WebKitGTK` therefore lands only
//! here — the shell links no web engine.
//!
//! # Why the window never reads the page's DOM
//!
//! Mara's constraint (#947, 2026-09-11 09:59Z): hyperhive's agent terminal
//! page is slated for a swarm-level rewrite, so integration through its markup
//! would be fragile. The whole contract with the hive's frontend is therefore
//! one query parameter ([`page::HIDE_QUERY`]), and everything the chrome shows
//! comes from `host.sock` instead ([`feed`]). The rewrite cannot break this
//! window as long as the parameter survives it.
//!
//! # One window per agent
//!
//! The application id carries the agent ([`cli::app_id`]), so `GApplication`'s
//! own single-instance machinery *is* the feature: a second
//! `--agent <same name>` finds the running process, hands it the command line
//! (`HANDLES_COMMAND_LINE`, so the pen's `--tab settings` is honoured rather
//! than dropped) and exits, while a different agent is simply a different
//! application. See [`cli::app_id`] for the trade against one shared id.

pub mod chrome;
pub mod cli;
pub mod feed;
pub mod page;
pub mod tls;
pub mod ui;
pub mod webview;
