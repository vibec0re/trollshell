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
//! - [`sample`] — the `/proc` and `/sys` reads (through `hytte-sensors`, the
//!   shell's own samplers) and the visibility-gated task that drives them.
//! - [`card`] — the geometry, the per-core lamp ramp, the sidebar card and the
//!   bar chips.
//! - [`panel`] — the drawer page a chip click opens (#1251).
//! - [`mod@format`] — the byte and `used / total` strings the native Stats page
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

mod card;
mod config;
mod format;
mod panel;
mod plugin;
mod sample;

fn main() -> ! {
    // `hytte-config` warns through `tracing` when `stats.toml` carries a key
    // this schema does not know, and one line per key whose *value* nothing
    // accepts — the per-key tolerance is only useful if the reader hears about
    // it. With no subscriber installed those are silent no-ops, so the whole
    // diagnostic half of the config layering would be enforced and then thrown
    // away. One subscriber, matching `hytte-plugin-agents`'; stderr is the
    // journal for a systemd user unit.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("hytte_plugin_stats=info,hytte_config=warn")
            }),
        )
        .with_writer(std::io::stderr)
        .init();

    hytte_plugin::run::<plugin::Stats>()
}
