//! `hytte-claude-bridge` — an OpenAI-compatible face on headless Claude Code,
//! served on loopback so the existing hytte LLM plugins can ride a Claude
//! subscription **with zero changes to pet or caw** (`Provider` is already just
//! a base URL). Issue #584.
//!
//! One route, `POST /v1/chat/completions`, bound to `127.0.0.1:8787` — loopback
//! only, never `0.0.0.0`, because this runs on somebody's personal credentials.
//! (Port 8080 belongs to `trollshell-pet-brain.service`'s llama-server.)
//!
//! # It is also a hytte plugin (#866)
//!
//! Since #866 the daemon wears a second hat: it speaks the widget-plugin
//! protocol and paints a small status chip on the bar, following
//! `hytte-plugin-infobroker`'s two-hats-in-one-binary shape. That is what lets
//! it ride `programs.trollshell.plugins` — the launcher, the control-center's
//! Plugins tab, and #392's keyring injection — instead of needing its own
//! hand-declared systemd unit. See [`plugin`] for the chip and for **why the
//! HTTP listener is bound and spawned before the SDK loop is entered**: the API
//! is the primary duty and must not depend on the shell being up.
//!
//! # The INBOUND side is KEYLESS — and that is a correctness requirement
//!
//! The bridge validates **no bearer token at all** on the route it serves. It
//! cannot, and here is the precise reason: `hytte-plugin-pet`'s `brain.rs`
//! resolves its key as
//!
//! ```text
//! hytte_ai_providers::load_key("openrouter").or_else(|| env "PET_LLM_API_KEY")
//! ```
//!
//! — the shared `openrouter` key file (and its `OPENROUTER_API_KEY` env
//! override) is consulted **before** the pet's own variable. So a pet pointed
//! at this bridge sends whatever key it happens to have, and a bridge demanding
//! its *own* token would 401 every single request, forever. Keyless inbound is
//! the only shape that works.
//!
//! The corresponding control lives in the unit, not here:
//! `etc/systemd/user/trollshell-claude-bridge.service` sets a dummy
//! `OPENROUTER_API_KEY=local-bridge`. `load_key_from` checks the env override
//! *before* the key file, so that dummy value is what stops a real cloud key
//! being shipped to a loopback port. It is a security control; treat it as one.
//!
//! Loopback-only binding is the other half of that: with no inbound auth,
//! reachability *is* the authorization boundary. That matters *more* in
//! `CLAUDE_BRIDGE_MODE=api`, where anything that can reach the port spends real
//! money — which is exactly why the IP is hard-coded and not configurable.
//!
//! # Outbound, the bridge holds a key only in `api` mode
//!
//! The two `claude` modes hold no credentials of their own; `claude` owns the
//! subscription session. The `api` mode ([`messages`], #730) is the exception
//! by design: it is *for* people who would rather pay per token, so it loads an
//! Anthropic API key and refuses to start without one.
//!
//! # Deliberately unimplemented
//!
//! Only what `hytte_ai_providers::chat` sends is accepted, and only what it
//! parses is returned. There is **no streaming**, **no `usage` block**, and
//! **no tool calls**. `temperature` is ignored on every path — the Claude Code
//! CLI has no such flag and the Messages API *rejects* it on the current models
//! — and `max_tokens` is approximated by the `claude` paths and honoured for
//! real by `api`. Inventing surface beyond the one client would be surface
//! nobody has ever exercised.
//!
//! # Sessions retire themselves (subscription only)
//!
//! On the subscription path a conversation rides one persisted claude session,
//! which is what keeps its prompt prefix byte-stable and the cache warm — and
//! which also means it grows every turn and nothing prunes it. When it finally
//! overflows, the bridge retires that session and continues the conversation in
//! a fresh one, generation by generation (`hytte-bridge-<hash>`, `…-g1`, `…-g2`)
//! — see [`session`]'s module docs. It is **logged at `warn`**, because a
//! rotation is the one event that explains a cache-hit rate falling off a cliff,
//! and the old session is left on disk rather than deleted.
//!
//! The retirement survives a restart (#855): it is recorded in
//! `CLAUDE_BRIDGE_STATE_DIR`/[`retired::FILE`], because a bridge that forgot it
//! would resolve an already-rotated conversation back to the session that had
//! overflowed and spend one visibly failed turn rediscovering that — on every
//! `systemctl --user restart`. A missing or damaged file is never fatal; see
//! [`retired`].
//!
//! # Environment
//!
//! | variable | default | meaning |
//! | --- | --- | --- |
//! | `CLAUDE_BRIDGE_PORT` | `8787` | loopback port (the address is not configurable) |
//! | `CLAUDE_BRIDGE_MODEL` | unset | the model; empty leaves claude's own default, or [`messages::DEFAULT_MODEL`] in `api` mode |
//! | `CLAUDE_BRIDGE_MODE` | `subscription` | `subscription` (persisted session), `reprompt`, or `api` (#730) |
//! | `CLAUDE_BRIDGE_TIMEOUT_SECS` | `8` | per-request budget; must stay under the client's 10s |
//! | `CLAUDE_BRIDGE_STATE_DIR` | `$XDG_STATE_HOME/hytte-claude-bridge` | the child's cwd, which is what scopes claude's on-disk sessions (`claude` modes only), and where the retired-session map is kept (#855) |
//! | `CLAUDE_BRIDGE_THINKING` | `disabled` | `api` mode only: `disabled`, `adaptive`, or `auto` — see [`messages::Thinking`] |
//! | `ANTHROPIC_API_KEY` | unset | `api` mode only: it outranks `~/.config/trollshell/anthropic.key`. In the `claude` modes it is a **startup refusal** (see [`envguard`]) |
//!
//! # Where `ANTHROPIC_API_KEY` comes from, after #866
//!
//! Under the **static unit** (`etc/systemd/user/trollshell-claude-bridge.service`,
//! still the reference for hand-installed deployments) the variable is scrubbed
//! by `UnsetEnvironment=`, so `~/.config/trollshell/anthropic.key` is the only
//! source that works there (#752).
//!
//! Under the **launcher** the shape is different by design. A transient unit
//! spawned by `systemd-run` carries no `UnsetEnvironment=`, so that scrub does
//! not exist on this path; what remains is [`envguard`]'s startup refusal, which
//! names the offending variable in `systemctl status`. The home-manager module
//! therefore declares the `anthropic` secret slot **only** when the configured
//! mode is `api` — in the two `claude` modes an injected key is a refusal, not a
//! credential — and the launcher then injects the keyring's key as
//! `ANTHROPIC_API_KEY`, which the env-first precedence above picks up. The key
//! file keeps working unchanged behind it.

mod backend;
mod bridge;
mod envguard;
mod http;
mod messages;
mod plugin;
mod retired;
mod session;
mod status;
mod wire;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use backend::{Backend, Reprompt, Subscription};
use bridge::Bridge;

/// The port the design settled on. 8080 is llama-server's.
const DEFAULT_PORT: u16 = 8787;

/// Per-request budget. Under `hytte-ai-providers`' 10s global timeout on
/// purpose: the client must see a clean 504 it can fall back from, not a
/// connection-level failure. Measured `claude -p` latency for pet-shaped
/// prompts was 4–6s; caw's briefing is larger and untested, and a clean failure
/// there is a good outcome (both consumers degrade to canned output on `Err`).
const DEFAULT_BUDGET: Duration = Duration::from_secs(8);

/// Which conversation implementation to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// A persisted, title-addressed `hive-claude` session; resumed turns carry
    /// only the delta.
    Subscription,
    /// A one-off `claude` session per turn, with the bridge holding the
    /// transcript.
    Reprompt,
    /// The Anthropic Messages API, billed to an API key (#730). Same
    /// bridge-holds-the-transcript shape as [`Mode::Reprompt`]; no `claude`
    /// subprocess is spawned at all.
    Api,
}

impl Mode {
    /// Parse `$CLAUDE_BRIDGE_MODE`. Anything unrecognised falls back to the
    /// default rather than refusing to start — a typo in a unit file should not
    /// take the bridge down — but it is **said out loud**, because silently
    /// running the subscription path when the operator asked for the metered
    /// one (or the reverse) is exactly the kind of billing surprise the rest of
    /// this crate goes to some length to prevent.
    fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim) {
            Some("reprompt") => Self::Reprompt,
            Some("api" | "api-key" | "messages") => Self::Api,
            Some(other) if !other.is_empty() && other != "subscription" => {
                tracing::warn!(
                    value = other,
                    "unrecognised CLAUDE_BRIDGE_MODE; running the default subscription path",
                );
                Self::Subscription
            }
            _ => Self::Subscription,
        }
    }

    /// Whether this mode spawns a `claude` child.
    ///
    /// The one thing that has to be asked about a mode from outside it: the
    /// billing guard ([`envguard`]), the state directory, and the `hive-claude`
    /// config all exist **for the child**, and none of them mean anything when
    /// there isn't one.
    fn spawns_claude(self) -> bool {
        matches!(self, Self::Subscription | Self::Reprompt)
    }
}

/// Everything read out of the environment at startup.
#[derive(Debug, Clone)]
struct Settings {
    port: u16,
    mode: Mode,
    model: String,
    thinking: messages::Thinking,
    budget: Duration,
    state_dir: PathBuf,
}

impl Settings {
    fn from_env() -> Self {
        Self {
            port: env_nonempty("CLAUDE_BRIDGE_PORT")
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_PORT),
            mode: Mode::parse(env_nonempty("CLAUDE_BRIDGE_MODE").as_deref()),
            model: env_nonempty("CLAUDE_BRIDGE_MODEL").unwrap_or_default(),
            thinking: messages::Thinking::parse(env_nonempty("CLAUDE_BRIDGE_THINKING").as_deref()),
            budget: env_nonempty("CLAUDE_BRIDGE_TIMEOUT_SECS")
                .and_then(|v| v.parse().ok())
                .filter(|s| *s > 0)
                .map_or(DEFAULT_BUDGET, Duration::from_secs),
            state_dir: state_dir(),
        }
    }

    /// The budget the *inner* client gets: one second under the request budget.
    ///
    /// Both backends need the same ordering for the same reason — whatever is
    /// doing the work must give up before the outer `tokio::time::timeout`
    /// does, or the outer one fires and the work carries on with nobody left to
    /// read it (a `claude` child with no `kill_on_drop`; a `spawn_blocking`
    /// HTTP request that cannot be cancelled).
    fn inner_budget(&self) -> Duration {
        self.budget
            .saturating_sub(Duration::from_secs(1))
            .max(Duration::from_secs(1))
    }

    /// The `hive-claude` invocation config.
    ///
    /// `idle_timeout` is [`Settings::inner_budget`], so a stalled turn is
    /// killed and reaped by the driver — yielding a typed `Error::IdleTimeout`
    /// — *before* the outer budget fires. The outer timeout remains as a
    /// backstop for a turn that is streaming but too slow; that path can leave
    /// the child running, which is logged rather than papered over.
    fn claude_config(&self) -> hive_claude::Config {
        hive_claude::Config {
            model: self.model_arg(),
            cwd: Some(self.state_dir.clone()),
            idle_timeout: Some(self.inner_budget()),
            ..hive_claude::Config::default()
        }
    }

    /// The `--model` flag for the CLI paths, or `None` to omit it and let
    /// `claude` pick its own default.
    ///
    /// An **empty** `$CLAUDE_BRIDGE_MODEL` has always meant "omit the flag":
    /// before #757 the driver itself keyed on `if !config.model.is_empty()`,
    /// so a `String` carried both the id and the absence. `hive-claude` 0.1.0
    /// moved that distinction into the type (`Option<String>`), which puts the
    /// contract here instead. `Some(self.model.clone())` would type-check and
    /// pass `--model ""` to the CLI — hence the emptiness check, and the test
    /// that pins it.
    fn model_arg(&self) -> Option<String> {
        (!self.model.is_empty()).then(|| self.model.clone())
    }

    /// The model id the `api` mode asks for. Unlike the CLI paths
    /// ([`Settings::model_arg`]), an empty `$CLAUDE_BRIDGE_MODEL` cannot mean
    /// "the callee's default" — the Messages API requires the field.
    fn api_model(&self) -> String {
        if self.model.is_empty() {
            messages::DEFAULT_MODEL.to_owned()
        } else {
            self.model.clone()
        }
    }
}

/// `$VAR`, if set to something non-empty.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Where the `claude` child runs.
///
/// A dedicated directory rather than the user's home or a repo: Claude Code
/// derives its per-project session directory from the cwd *and* reads that
/// directory's project config, so running in a scratch dir keeps a pet tick
/// from dragging in whatever `CLAUDE.md`, hooks and MCP servers happen to live
/// where the unit was started.
fn state_dir() -> PathBuf {
    if let Some(explicit) = env_nonempty("CLAUDE_BRIDGE_STATE_DIR") {
        return PathBuf::from(explicit);
    }
    env_nonempty("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env_nonempty("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("hytte-claude-bridge")
}

/// The listen address. The IP is **hard-coded loopback** — only the port is
/// configurable, so no environment mistake can expose an unauthenticated
/// endpoint on a LAN.
fn bind_addr(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

/// Everything that has to succeed before the bridge can answer a request: the
/// backend, the retired-session map, and the bound loopback listener.
struct Serving {
    listener: tokio::net::TcpListener,
    bridge: Arc<Bridge>,
    /// Whether the bridge holds an outbound credential of its own — true only in
    /// [`Mode::Api`], which has already refused to start if no key resolved. The
    /// value the chip paints its key glyph from; the key itself never leaves
    /// [`messages`].
    keyed: bool,
}

/// Bring the HTTP half up: run the billing guard where it applies, build the
/// backend, and bind the loopback listener. `None` on any failure — each one is
/// logged here, and `main` turns it into [`ExitCode::FAILURE`].
async fn start(settings: &Settings) -> Option<Serving> {
    // Fail closed before anything else — but only where the guard means
    // anything. It exists to stop the `claude` **child** being redirected onto
    // metered credits behind the operator's back; in `api` mode there is no
    // child and metered billing is the whole point, so `ANTHROPIC_API_KEY` is
    // the credential rather than the offence. Scoping it here is the one place
    // that distinction lives.
    if settings.mode.spawns_claude() {
        let offenders = envguard::offenders_in_env();
        if !offenders.is_empty() {
            tracing::error!("{}", envguard::refusal(&offenders));
            return None;
        }
        // The state dir is the child's cwd, and holds the retired-session map
        // (#855) — which creates the dir itself when it writes, so this is
        // still only needed for the child.
        if let Err(e) = std::fs::create_dir_all(&settings.state_dir) {
            tracing::error!(dir = %settings.state_dir.display(), error = %e, "could not create the state dir");
            return None;
        }
    }

    let mut keyed = false;
    let backend = match settings.mode {
        Mode::Subscription => Backend::Subscription(Subscription::new(settings.claude_config())),
        Mode::Reprompt => Backend::Reprompt(Reprompt::new(settings.claude_config())),
        Mode::Api => {
            // Refuse rather than bind: a bridge that starts without a key
            // advertises a backend it cannot serve, and every pet tick would
            // come back a 502 that looks like the plugin's fault.
            let Some(key) = messages::load_key() else {
                tracing::error!("{}", messages::missing_key_refusal());
                return None;
            };
            keyed = true;
            Backend::Reprompt(Reprompt::with_api(messages::Client::new(
                key,
                settings.api_model(),
                settings.thinking,
                settings.inner_budget(),
            )))
        }
    };
    // The retired-session map lives beside the child's sessions, in the same
    // state dir, and is loaded here so a conversation that had already rotated
    // resumes where it left off rather than replaying its overflow (#855).
    // Passed in every mode: only the subscription backend ever *rotates*, so
    // the other two simply never write it.
    let bridge = Arc::new(Bridge::new(
        backend,
        settings.budget,
        Some(settings.state_dir.join(retired::FILE)),
    ));

    let addr = bind_addr(settings.port);
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!(%addr, error = %e, "could not bind");
            return None;
        }
    };
    let billing = if settings.mode.spawns_claude() {
        "claude subscription (no key held)"
    } else {
        "metered Anthropic API credits (keyed)"
    };
    tracing::info!(
        %addr,
        mode = ?settings.mode,
        model = %if settings.mode.spawns_claude() {
            if settings.model.is_empty() { "<claude default>".to_owned() } else { settings.model.clone() }
        } else {
            settings.api_model()
        },
        budget_s = settings.budget.as_secs(),
        state_dir = %settings.state_dir.display(),
        billing,
        "hytte-claude-bridge listening (no inbound auth; loopback only)",
    );

    Some(Serving {
        listener,
        bridge,
        keyed,
    })
}

/// Answer connections forever. Spawned on the HTTP runtime by `main`, so it
/// keeps serving whatever the plugin face is doing — including while the shell
/// is down and the SDK is sitting in its dial backoff.
async fn accept_loop(listener: tokio::net::TcpListener, bridge: Arc<Bridge>) {
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let bridge = Arc::clone(&bridge);
                tokio::spawn(async move { bridge::serve_connection(&bridge, stream).await });
            }
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// The process's two duties, in priority order (#866).
///
/// `main` is **not** `#[tokio::main]` any more, and that is load-bearing:
/// [`hytte_plugin::run`] builds a current-thread runtime of its own and
/// `block_on`s it forever, which cannot happen from inside another runtime's
/// context. So this owns the HTTP runtime explicitly, gets the listener serving
/// on it, and only then hands the main thread to the SDK. `rt` is never dropped
/// (both tail paths diverge), so its worker threads — and the accept loop on
/// them — outlive every plugin session.
fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("hytte_claude_bridge=info")),
        )
        .init();

    let settings = Settings::from_env();

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "tokio runtime failed to build");
            return ExitCode::FAILURE;
        }
    };

    let Some(serving) = rt.block_on(start(&settings)) else {
        return ExitCode::FAILURE;
    };
    // Publish before the listener can record anything, so the chip never shows
    // counts against an unknown mode.
    status::publish(status::Startup {
        mode: settings.mode,
        keyed: serving.keyed,
    });
    rt.spawn(accept_loop(serving.listener, serving.bridge));

    // The chip is the secondary duty. With no `XDG_RUNTIME_DIR` there is no host
    // socket to dial *ever*, and the SDK would exit the process over it — which
    // would take the API down with it. Park on the HTTP runtime instead.
    if plugin::host_socket_available() {
        plugin::run();
    }
    tracing::warn!(
        "XDG_RUNTIME_DIR unset; no trollshell host socket to dial — serving HTTP with no bar chip"
    );
    rt.block_on(std::future::pending::<ExitCode>())
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_BUDGET, Mode, Settings, bind_addr};
    use std::time::Duration;

    /// The listen address must never be routable. Only the port is
    /// configurable; the IP is not reachable from the environment at all.
    #[test]
    fn the_bind_address_is_always_loopback() {
        for port in [8787u16, 1, 65535] {
            let addr = bind_addr(port);
            assert!(addr.ip().is_loopback(), "{addr} is not loopback");
            assert_eq!(addr.port(), port);
        }
    }

    /// The request budget must stay under `hytte-ai-providers`' 10s global
    /// timeout, or the client sees a torn connection instead of a clean error
    /// it can fall back from.
    #[test]
    fn the_default_budget_is_under_the_clients_global_timeout() {
        assert!(DEFAULT_BUDGET < Duration::from_secs(10));
    }

    /// Mode parsing: the persisted-session path is the default, and a typo in a
    /// unit file must not take the bridge down.
    #[test]
    fn mode_defaults_to_subscription() {
        assert_eq!(Mode::parse(None), Mode::Subscription);
        assert_eq!(Mode::parse(Some("subscription")), Mode::Subscription);
        assert_eq!(Mode::parse(Some("nonsense")), Mode::Subscription);
        assert_eq!(Mode::parse(Some(" reprompt ")), Mode::Reprompt);
    }

    /// The API-key backend rides the **existing** mode knob rather than a
    /// second selection mechanism (#730), and answers to the spellings somebody
    /// is plausibly going to type.
    #[test]
    fn the_api_backend_is_selected_through_the_same_mode_knob() {
        for spelling in ["api", " api ", "api-key", "messages"] {
            assert_eq!(Mode::parse(Some(spelling)), Mode::Api, "{spelling}");
        }
    }

    /// **The billing-guard scope.** `envguard` refuses to start on
    /// `ANTHROPIC_API_KEY` because it would silently move the `claude` child
    /// onto metered credits — but in `api` mode there is no child and that
    /// variable is the credential. Exactly the modes that spawn `claude` run
    /// the guard; if this ever flipped, the API backend could not be configured
    /// through its env override at all.
    #[test]
    fn only_the_claude_modes_run_the_billing_guard() {
        assert!(Mode::Subscription.spawns_claude());
        assert!(Mode::Reprompt.spawns_claude());
        assert!(!Mode::Api.spawns_claude());
    }

    /// The `api` mode needs a concrete model id — an empty
    /// `$CLAUDE_BRIDGE_MODEL` cannot mean "the callee's default" the way it
    /// does for the CLI, because the Messages API requires the field.
    #[test]
    fn the_api_mode_always_resolves_a_concrete_model() {
        let mut settings = Settings {
            port: 8787,
            mode: Mode::Api,
            model: String::new(),
            thinking: crate::messages::Thinking::default(),
            budget: DEFAULT_BUDGET,
            state_dir: std::path::PathBuf::from("/tmp"),
        };
        assert_eq!(settings.api_model(), crate::messages::DEFAULT_MODEL);
        settings.model = "claude-haiku-4-5".to_owned();
        assert_eq!(settings.api_model(), "claude-haiku-4-5");
    }

    /// **The CLI `--model` contract**, which #757's `hive-claude` upgrade moved
    /// out of the driver and into this crate. An empty `$CLAUDE_BRIDGE_MODEL`
    /// means "omit the flag and let `claude` choose" — the driver used to
    /// enforce that itself with `if !config.model.is_empty()`, and its
    /// `Option<String>` successor makes `Some("")`, i.e. a literal `--model ""`,
    /// expressible for the first time. This is the sibling of
    /// [`Settings::api_model`], which resolves the same empty value the other
    /// way because the Messages API has no such default.
    #[test]
    fn an_empty_model_omits_the_cli_flag() {
        let mut settings = Settings {
            port: 8787,
            mode: Mode::Subscription,
            model: String::new(),
            thinking: crate::messages::Thinking::default(),
            budget: DEFAULT_BUDGET,
            state_dir: std::path::PathBuf::from("/tmp"),
        };
        assert_eq!(settings.model_arg(), None);
        settings.model = "haiku".to_owned();
        assert_eq!(settings.model_arg(), Some("haiku".to_owned()));
    }

    /// The inner client's budget must be strictly under the outer one, or the
    /// outer timeout fires first and the work it was bounding runs on
    /// uncancelled.
    #[test]
    fn the_inner_budget_stays_under_the_request_budget() {
        let settings = Settings {
            port: 8787,
            mode: Mode::Api,
            model: String::new(),
            thinking: crate::messages::Thinking::default(),
            budget: DEFAULT_BUDGET,
            state_dir: std::path::PathBuf::from("/tmp"),
        };
        assert!(settings.inner_budget() < DEFAULT_BUDGET);
        // …and never zero, however small the outer budget gets.
        let tight = Settings {
            budget: Duration::from_secs(1),
            ..settings
        };
        assert_eq!(tight.inner_budget(), Duration::from_secs(1));
    }
}
