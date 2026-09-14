//! `hytte-plugin-stats` — system stats as an out-of-process trollshell plugin
//! (issue #1250, P1 of epic #1248; settled on Discussion #1235).
//!
//! One binary, meant to run **twice**: the full thing as bar chips (P2, #1251)
//! and a compact CPU + GPU card in the right sidebar, which is what this phase
//! ships. The two launches differ in exactly two environment variables —
//! `HYTTE_PLUGIN_MOUNT` (where the card mounts, #1159) and `HYTTE_PLUGIN_ID`
//! (what it registers as, #1250, without which the host rejects the second
//! connection as a duplicate) — and in the table of `stats.toml` they therefore
//! read. See `docs/plugin-env.md`.
//!
//! # The shape
//!
//! - [`config`] — `stats.toml`: `[bar]` and `[sidebar]`, over `hytte-config`'s
//!   `Subsystem` layering, with per-key tolerance.
//! - [`mount`] — the effective mount, and why this plugin reads a variable the
//!   SDK has already read.
//! - [`sample`] — the `/proc` and `/sys` reads (through `hytte-sensors`, the
//!   shell's own samplers) and the visibility-gated task that drives them.
//! - [`card`] — the geometry, and the per-core lamp ramp.
//! - [`plugin`] — the TEA core: manifest, `update`, `view`.
//!
//! Everything the card draws is a `Node::Preem` widget: the per-core lamp row
//! as a `DotMatrix`, the package temperature as a `SevenSeg`, the GPU load as a
//! `Gauge`, the load history as a `Scope`. The SDK's `display` wrappers decide
//! per render whether those go out as typed state (a preem-speaking shell draws
//! them on the GPU) or as CPU-rasterised pixels (an older one), so this plugin
//! has one code path for both.

mod card;
mod config;
mod mount;
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
