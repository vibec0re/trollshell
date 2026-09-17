//! `hytte-plugin-stats` — system stats as an out-of-process trollshell plugin
//! (issue #1250, P1 of epic #1248; settled on Discussion #1235).
//!
//! One binary, meant to run **twice**: the full thing as bar chips plus its own
//! drawer page (P2, #1251) and a compact CPU + GPU card in the right sidebar
//! (P1, #1250). The two launches differ in exactly two environment variables —
//! `HYTTE_PLUGIN_MOUNT` (where the card mounts, #1159) and `HYTTE_PLUGIN_ID`
//! (what it registers as, #1250, without which the host rejects the second
//! connection as a duplicate) — and in the table of `stats.toml` they therefore
//! read. See `docs/plugin-env.md`.
//!
//! # The shape
//!
//! - [`config`] — `stats.toml`: `[bar]` and `[sidebar]`, over `hytte-config`'s
//!   `Subsystem` layering, with per-key tolerance.
//! - [`plugin::settings_from`] — this instance's effective mount
//!   (`hytte_plugin::effective_mount_from`, graduated into the SDK by #1317)
//!   decides which of the two tables above it reads.
//! - `sample` — the `/proc` and `/sys` reads (through `hytte-sensors`, the
//!   shell's own samplers) and the visibility-gated task that drives them.
//! - `card` — the geometry, the per-core lamp ramp, the sidebar card and the
//!   bar chips.
//! - `panel` — the drawer page a chip click opens (#1251).
//! - `format` — the byte and `used / total` strings the native Stats page
//!   prints, mirrored.
//! - [`plugin`] — the TEA core: manifest, `update`, `view`.
//!
//! Every meter either surface draws is a `Node::Preem` widget: the per-core
//! lamp row as a `DotMatrix`, the package temperature as a `SevenSeg`, the GPU
//! load as a `Gauge`, the load history as a `Scope`, memory and swap as
//! `LedStrip`s. The SDK's `display` wrappers decide per render whether those go
//! out as typed state (a preem-speaking shell draws them on the GPU) or as
//! CPU-rasterised pixels (an older one), so this plugin has one code path for
//! both.
//!
//! # Four chips, not five
//!
//! The bar instance mirrors four of the shell's five native stats chips —
//! `ts-cpu`, `ts-memory`, `ts-disk`, `ts-gpu`. The fifth, `ts-services`, counts
//! **failed systemd units** (`hytte_services::systemd::failed_units`, a
//! system-bus client) and **flapping shell tasks**
//! (`hytte_reactive::health::signal`, the shell's own supervised-task
//! supervisor). Neither is reachable from a plugin process — there is no
//! `StateKey` for either, a plugin never links `hytte-services`, and the task
//! supervisor is in-process state that exists nowhere else — so inventing a
//! transport for it would be a bigger decision than this phase. Epic #1248's P3
//! already says that chip and card stay native. See the PR for #1251.
//!
//! # Why this is a library as well as a binary (#888 P0)
//!
//! `trollshell-control-center` renders a settings form from
//! [`config::SCHEMA`], and it reaches this crate the way it already reaches
//! `hytte-plugin-agents`: as a **library** (#947 P4's precedent). A plugin
//! crate that links `hytte-config` cannot have its consts moved down into
//! `hytte-config-families` — that leaf would then depend on a crate that
//! depends on it — so they stay here and the companion app composes them in at
//! the top of the graph.
//!
//! The split is `hytte-plugin-agents`' exactly: everything is here, and
//! `main.rs` is the subscriber plus one call into the SDK.

// Only what a *consumer* of this library needs is public: `config` for #888
// P1's settings form, `plugin` for the binary beside this file. The four
// rendering modules stay crate-private, which is exactly what they were while
// this crate was bin-only — publishing them would export a plugin's internal
// geometry as API, and rustdoc would then (rightly) complain about every doc
// link they make to a private helper.
mod card;
pub mod config;
mod format;
mod panel;
pub mod plugin;
mod sample;
