//! The `hytte-plugin-stats` binary — one line of plugin, like every other.
//!
//! The whole transport (dialing `$XDG_RUNTIME_DIR/trollshell/plugin.sock` with
//! bounded backoff, the `Register` handshake, liveness, render dedup,
//! reconnection) lives in [`hytte_plugin::run`]; systemd's `Restart=on-failure`
//! on the transient `trollshell-plugin-stats` unit is the outer supervisor.
//!
//! Everything this plugin does is in the library ([`hytte_plugin_stats`]) —
//! the `hytte-plugin-agents` split, adopted by #888 P0 so
//! `trollshell-control-center` can read [`hytte_plugin_stats::config::SCHEMA`]
//! without linking a binary. The crate docs live there.

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

    hytte_plugin::run::<hytte_plugin_stats::plugin::Stats>()
}
