//! The `hytte-plugin-agents` binary — one line of plugin, like every other.
//!
//! The whole transport (dialing `$XDG_RUNTIME_DIR/trollshell/plugin.sock` with
//! bounded backoff, the `Register` handshake, liveness, render dedup,
//! reconnection) lives in [`hytte_plugin::run`]; systemd's `Restart=on-failure`
//! on the transient `trollshell-plugin-agents` unit is the outer supervisor.
//!
//! Everything this plugin actually does is in the library
//! ([`hytte_plugin_agents`]), which is what lets the fake-socket suite drive it
//! with no host and no hive.

fn main() -> ! {
    // `hytte-config` warns through `tracing` when `agents.toml` carries a key
    // this schema does not know (the fourth merge rule), and this crate warns
    // on a refused write or a dropped row. With no subscriber installed those
    // are silent no-ops, so the unknown-key rule would be enforced and then
    // thrown away — hence one subscriber, matching `hytte-claude-bridge`'s.
    // stderr is the journal for a systemd user unit.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("hytte_plugin_agents=info,hytte_config=warn")
            }),
        )
        .with_writer(std::io::stderr)
        .init();

    hytte_plugin::run::<hytte_plugin_agents::Agents>()
}
