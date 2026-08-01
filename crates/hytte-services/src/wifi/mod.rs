//! Wi-Fi station tracking + network list, backed by either iwd or
//! `NetworkManager` depending on which daemon is running on the host.
//!
//! The backend is probed at startup via [`crate::wifi_backend::probe_backend`],
//! inside the service's own supervised task and retried while the probe is
//! *inconclusive* (see [`probe_until_conclusive`]), so a system bus that is
//! still coming up no longer latches "no wireless backend" for the process
//! lifetime (#613). Widgets are backend-agnostic — they only see the public
//! types and signal accessors exported from this module.
//!
//! # Public API
//!
//! ```ignore
//! // Register once at startup:
//! .with(wifi::service())
//!
//! // Subscribe in widgets:
//! wifi::station() -> impl Signal<Item = Option<Station>>
//! wifi::networks() -> impl Signal<Item = Vec<WifiNetwork>>
//! wifi::active_prompt() -> impl Signal<Item = Option<PromptRequest>>
//!
//! // Fire-and-forget commands:
//! wifi::scan();
//! wifi::connect_network(path);
//! wifi::disconnect();
//! wifi::submit_prompt(id, &secrets);   // one value per PromptRequest::secret_keys
//! wifi::cancel_prompt(id);
//! ```

mod agent;
mod client;
mod nm_agent;
mod parse;
mod types;
mod watcher;

use futures_channel::oneshot;
use futures_signals::signal::{Mutable, Signal};
use hytte_bus::BusKind;
use hytte_reactive::{Service, registry, runtime, spawn_supervised};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, RwLock};

use crate::retry;
// `ProbeError` is no longer named outside the tests: since #646 the policy is
// generic over the `Result`, so only `probe_backend`'s own signature mentions it.
use crate::wifi_backend::BackendChoice;

// ── Public re-exports ─────────────────────────────────────────────────────────

pub use crate::wifi_nm::{VpnProfile, WiredProfile};
pub use types::{Adapter, PromptKind, PromptRequest, Station, StationState, WifiNetwork};

// ── Station path cache ────────────────────────────────────────────────────────

/// Filled by the watcher on station discovery; read by command helpers.
/// Uses an `RwLock` so a new station path (USB dongle swap) can be written.
static STATION_PATH: OnceLock<Arc<tokio::sync::RwLock<String>>> = OnceLock::new();

fn station_path_store() -> &'static Arc<tokio::sync::RwLock<String>> {
    STATION_PATH.get_or_init(|| Arc::new(tokio::sync::RwLock::new(String::new())))
}

pub(super) async fn get_station_path() -> String {
    station_path_store().read().await.clone()
}

pub(super) async fn set_station_path(path: &str) {
    *station_path_store().write().await = path.to_string();
}

/// Filled by the watcher on adapter discovery; read by command helpers.
static ADAPTER_PATH: OnceLock<Arc<tokio::sync::RwLock<String>>> = OnceLock::new();

fn adapter_path_store() -> &'static Arc<tokio::sync::RwLock<String>> {
    ADAPTER_PATH.get_or_init(|| Arc::new(tokio::sync::RwLock::new(String::new())))
}

pub(super) async fn current_adapter_path() -> String {
    adapter_path_store().read().await.clone()
}

pub(super) async fn set_current_adapter_path(path: &str) {
    *adapter_path_store().write().await = path.to_string();
}

// ── Agent waiter map (module-level OnceLock for public API access) ────────────

/// Pending prompt waiters: request id → the oneshot the overlay resolves.
///
/// The payload is a `Vec<String>` — **one value per
/// [`PromptRequest::secret_keys`] entry, in that order** — not a single
/// string, because a `GetSecrets` round can ask for several secrets at once
/// and NM expects them all back in one reply. The single-secret case is simply
/// a one-element vector.
pub(super) type WaitersMap =
    Arc<AsyncMutex<HashMap<u64, oneshot::Sender<Result<Vec<String>, String>>>>>;

static WAITERS: OnceLock<WaitersMap> = OnceLock::new();

fn waiters() -> Option<&'static WaitersMap> {
    WAITERS.get()
}

pub(super) static NEXT_ID: AtomicU64 = AtomicU64::new(1);

// ── Backend discriminant (stored in WifiHandles) ──────────────────────────────

/// Which daemon is backing the Wi-Fi service on this host.
///
/// Cloneable so command functions can read it from the registry (GTK thread)
/// and then use it inside a spawned tokio task.
#[derive(Clone)]
pub(crate) enum WifiBackend {
    /// iwd is managing the radio. Commands use iwd D-Bus paths.
    Iwd,
    /// `NetworkManager` is managing the radio.
    /// The inner `Arc<RwLock<String>>` stores the current NM Wi-Fi device path
    /// (e.g. `/org/freedesktop/NetworkManager/Devices/3`), written by the
    /// watcher task once the device is discovered and updated if it changes.
    NetworkManager(Arc<RwLock<String>>),
    /// Neither backend was detected; commands are no-ops.
    None,
}

// ── Service handle ────────────────────────────────────────────────────────────

/// Shared mutable state held in the service registry.
#[doc(hidden)]
pub struct WifiHandles {
    pub(crate) station: Mutable<Option<Station>>,
    pub(crate) networks: Mutable<Vec<WifiNetwork>>,
    pub(crate) prompts: Mutable<Option<PromptRequest>>,
    pub(crate) adapter: Mutable<Option<Adapter>>,
    /// Saved wired (ethernet) profiles. Only populated by the NM backend;
    /// stays empty for iwd / no-backend (iwd doesn't manage ethernet).
    pub(crate) wired: Mutable<Vec<WiredProfile>>,
    /// Saved VPN profiles. Only populated by the NM backend; stays empty for
    /// iwd / no-backend (iwd doesn't manage VPNs). Distinct from the poll-only
    /// `vpn::tunnels()` live-tunnel detection — these are NM connection profiles.
    pub(crate) vpn: Mutable<Vec<VpnProfile>>,
    /// iwd name-ownership signal; stays empty unless the iwd backend is chosen.
    ///
    /// A `OnceLock` rather than a plain `Option` because the backend probe now
    /// concludes *after* `start()` returns (see [`probe_until_conclusive`]), so
    /// the handle is parked here by the probe task. Set at most once — the
    /// verdict is still latched for the process lifetime (re-entrant switching
    /// is #633).
    _ownership: Arc<OnceLock<hytte_bus::OwnNameSignal>>,
    /// NM secret-agent export handle; stays empty unless the NM backend is
    /// chosen. Held only to keep the exported agent object (and its re-mount
    /// task) alive for the service's lifetime. `OnceLock` for the same reason
    /// as `_ownership`.
    _nm_agent: Arc<OnceLock<hytte_bus::ExportHandle>>,
    /// The committed backend. Starts at [`WifiBackend::None`] and is set once
    /// the probe reaches a conclusive verdict; commands issued before that see
    /// no backend, which is the same window that already exists between
    /// `start()` and the watcher's first discovery.
    pub(crate) backend: Mutable<WifiBackend>,
}

impl Default for WifiHandles {
    fn default() -> Self {
        // We can't call own_name here without the runtime; ownership is set
        // in Service::start. Use a placeholder that gets replaced immediately.
        // This is never called in practice — start() constructs WifiHandles directly.
        unreachable!("WifiHandles must be constructed via Service::start")
    }
}

// ── Service marker ────────────────────────────────────────────────────────────

/// The Wi-Fi service marker type — pass to `App::with`.
pub struct WifiService;

pub(super) const AGENT_PATH: &str = "/mov/vibec0re/trollshell/iwd_agent";
const ANCHOR_NAME: &str = "mov.vibec0re.trollshell.iwd-agent";

// ── Backend selection: retry while inconclusive, commit once conclusive ───────
//
// The verdict is latched for the process lifetime — `Service::start` spawns a
// *different* watcher per verdict — so committing to a wrong one is expensive.
// Before #613 an **inconclusive** probe ([`ProbeError`], "I could not ask") was
// collapsed straight into [`BackendChoice::None`] ("the bus answered and nobody
// is there"), which disabled Wi-Fi for the whole session; only a shell restart
// brought it back (#607).
//
// The fix is to keep asking until the answer means something. Retrying can't
// happen on the calling thread — `Service::start` runs on the GTK main thread,
// so a `block_on` retry loop there would freeze shell startup — so the probe
// and the per-verdict branch both moved inside the service's own supervised
// task, the shape `networkd.rs` already uses.
//
// Picking up a daemon that appears *after* a conclusive verdict, or switching
// backends at runtime, is out of scope here: it needs a cancellation primitive
// that does not exist yet (#633).
//
// The retry *mechanism* — schedule, budget, the pure decision — lives in
// `crate::retry`, shared with `networkd`'s startup seed since #646. What stays
// here is the *judgement*: which probe outcomes are worth asking again about,
// and why the shipped budget is unbounded.

/// The policy this service runs: **unbounded** retry with capped backoff.
///
/// Attempt 1 is immediate; each retry waits 0.5s doubled per elapsed attempt
/// and clamped at 8s, so the schedule settles into one probe every 8s forever.
/// Wall-clock cost per attempt is *not* zero and is not worth predicting: on a
/// healthy bus a probe is sub-millisecond, but the failure mode being retried
/// is exactly the one where it is slow — [`crate::wifi_backend::probe_backend`]
/// makes two sequential `hytte_bus::call`s, each with a 25s timeout and
/// [`hytte_bus::RetryPolicy::Once`] (a different, per-call policy — not this
/// one), so a down socket costs roughly 10s per attempt (fast
/// fail plus a 5s reconnect wait, twice) and a wedged peer up to ~50s (two full
/// timeouts, which `is_transient_zbus_error` does not classify as transient, so
/// there is no retry).
///
/// **Why unbounded.** `networkd.rs` already decided this same question the
/// other way round and its reasoning transfers wholesale: picking the inert
/// verdict "leaves the link list permanently empty even once the bus recovers
/// … the cost if NM genuinely never appears is a periodic warn, which beats an
/// inert service". Both Wi-Fi watchers self-heal the same way — `wifi_nm.rs`'s
/// discovery retries every 5s and `watcher.rs` has its `'discovery` loop — so
/// there is nothing to gain by stopping. A bounded give-up would leave the user
/// in precisely #607's state (Wi-Fi gone, restart the only cure) whenever the
/// bus takes longer than the bound: the bug this fix closes, on a longer fuse.
/// The bound cannot be justified by its own give-up being logged, either — that
/// argues for a bound from the visibility of the bound being wrong.
///
/// Visibility is preserved without the latch: **every** attempt logs at
/// `error!`, deliberately and not merely every Nth, because the backoff caps at
/// 8s — so a genuinely dead bus is loud in the journal forever rather than
/// silently polled.
///
/// **The policy is one field to change.** A bounded policy is
/// `max_attempts: Some(n)` and nothing else; `networkd`'s startup refresh
/// answered the same question the same way on #621, and since #646 the two
/// subsystems share [`retry::Policy`] rather than a hand-kept cross-reference.
///
/// The schedule's invariants — a nonzero first delay, a ceiling that is
/// actually reached — are asserted over *every* shipped policy in
/// `retry`'s tests (#665), not here.
pub(crate) const PROBE_RETRY: retry::Policy = retry::Policy {
    max_attempts: None,
    initial: Duration::from_millis(500),
    max_backoff: Duration::from_secs(8),
};

/// Probe for the Wi-Fi backend, retrying only while the probe is
/// *inconclusive*, and return the verdict to commit to.
///
/// `None` means the policy told us to stop asking before the bus ever answered
/// — the service stays inert, and the give-up is logged at `error!`. Under the
/// shipped unbounded [`PROBE_RETRY`] that cannot happen; this returns only once
/// it has a real verdict.
///
/// Every inconclusive attempt is logged at `error!` and a successful retry logs
/// a `RECOVERED` line naming the attempt count: #609's diagnostic exists to
/// *measure* how often this fires, and a retry that healed quietly would trade
/// a visible permanent bug for an invisible intermittent one.
async fn probe_until_conclusive(policy: retry::Policy) -> Option<BackendChoice> {
    let mut attempt: u32 = 1;
    loop {
        let outcome = crate::wifi_backend::probe_backend().await;
        // Rendered up front so the log arms below can name the failure. Empty
        // on `Ok`, which only ever reaches the `Proceed` arm.
        let reason = outcome
            .as_ref()
            .err()
            .map_or_else(String::new, ToString::to_string);

        // The verdict is weighed *here*, not inside the policy (#646). All
        // [`retry::Policy::step`] sees is the `Ok`/`Err` split — and for this
        // probe that split already is the distinction that matters: any answer
        // is conclusive and commits, **including `Ok(BackendChoice::None)`**
        // ("the bus replied and neither daemon is present"), while `ProbeError`
        // ("I could not ask") is the only retryable outcome. Retrying a real
        // `None` would be a bug of its own — an endless poll on a host that
        // simply has no Wi-Fi daemon — and committing an `Err` as `None` was
        // #613 itself.
        let step = policy.step(&outcome, attempt);
        match step {
            retry::Step::Proceed => {
                // `Proceed` is returned for `Ok` and nothing else, so the
                // verdict is right here in the probe's own `Result`. Reading it
                // off `outcome` rather than out of the step is what keeps the
                // policy generic — and means no code path can conjure a
                // `BackendChoice` out of an error.
                let choice = outcome.expect("retry::Step::Proceed is returned only for Ok");
                // The recovery wording is gated on the *verdict*, not just on
                // having retried: a host that genuinely runs no Wi-Fi daemon
                // recovers into `None`, and telling that user "Wi-Fi is coming
                // up" would be a false line in the very diagnostic this fix
                // exists to keep trustworthy.
                if attempt == 1 {
                    tracing::info!(?choice, "wifi: selected backend");
                } else if matches!(choice, BackendChoice::None) {
                    tracing::warn!(
                        attempts = attempt,
                        "wifi: backend probe RECOVERED — the system bus is answering again, and \
                         the verdict is that NO Wi-Fi daemon is present. That is a real finding, \
                         unlike the earlier 'could not ask'; the service stays inert, correctly \
                         (issue #613)."
                    );
                } else {
                    tracing::warn!(
                        attempts = attempt,
                        ?choice,
                        "wifi: backend probe RECOVERED — an earlier attempt could not reach the \
                         system bus, and a retry has now returned a real verdict. Wi-Fi is \
                         coming up without a shell restart (issue #613)."
                    );
                }
                return Some(choice);
            }
            retry::Step::Retry { after } => {
                // Logged on *every* attempt, not every Nth: the backoff caps at
                // 8s, so a permanently dead bus stays loud here rather than
                // degrading into a silent poll. See `PROBE_RETRY`.
                tracing::error!(
                    attempt,
                    retry_in_secs = after.as_secs_f64(),
                    error = %reason,
                    "wifi: backend probe was INCONCLUSIVE — the system bus could not be queried, \
                     so it is unknown whether a Wi-Fi daemon is running. This is NOT a finding \
                     that the host has no wireless hardware. Retrying until it answers; this \
                     line repeating means the bus is still unreachable, and a \
                     `backend probe RECOVERED` line follows once it heals (issues #607, #613)."
                );
                tokio::time::sleep(after).await;
                attempt += 1;
            }
            retry::Step::GiveUp => {
                tracing::error!(
                    attempts = attempt,
                    error = %reason,
                    "wifi: backend probe STILL INCONCLUSIVE after every retry — giving up and \
                     starting inert. Wi-Fi will not work this session; this is NOT a finding \
                     that the host has no wireless hardware. Run \
                     `systemctl --user restart trollshell` once the system bus is up to \
                     re-probe (issues #607, #613)."
                );
                return None;
            }
        }
    }
}

/// The state slots a backend watcher writes into.
///
/// Grouped so the per-backend start helpers below take three arguments instead
/// of eight, and so the probe task can carry all of them across a restart with
/// one clone.
#[derive(Clone)]
struct WatchTargets {
    station: Mutable<Option<Station>>,
    networks: Mutable<Vec<WifiNetwork>>,
    prompts: Mutable<Option<PromptRequest>>,
    adapter: Mutable<Option<Adapter>>,
    wired: Mutable<Vec<WiredProfile>>,
    vpn: Mutable<Vec<VpnProfile>>,
}

/// Commit to the iwd backend: own the agent name and start the iwd watcher.
///
/// Parks the ownership handle in `ownership_out` so it outlives this call (the
/// probe task returns once it has committed).
fn start_iwd(
    targets: WatchTargets,
    waiters: WaitersMap,
    ownership_out: &OnceLock<hytte_bus::OwnNameSignal>,
) -> WifiBackend {
    // Mount the iwd Agent on the SYSTEM bus (same as iwd's AgentManager). iwd
    // records our system-bus unique name when we call RegisterAgent, then
    // issues RequestPassphrase callbacks on the system bus.
    let agent = agent::IwdAgent {
        prompts: targets.prompts.clone(),
        waiters,
    };
    let own = hytte_bus::own_name(BusKind::System, ANCHOR_NAME)
        .at_path(AGENT_PATH, agent)
        .start();
    // Discarding a rejected `OwnNameSignal` is genuinely inert: it has no
    // `Drop`, so dropping it drops read access to the ownership state and
    // nothing else — the ownership task it spawned keeps running regardless.
    // Contrast `ExportHandle` in `start_network_manager`, whose drop is
    // destructive and is therefore guarded.
    let _ = ownership_out.set(own);

    spawn_supervised("wifi", move || {
        watcher::run_wifi_watcher(
            targets.station.clone(),
            targets.networks.clone(),
            targets.prompts.clone(),
            targets.adapter.clone(),
        )
    });

    WifiBackend::Iwd
}

/// Commit to the `NetworkManager` backend: start the NM watcher and export the
/// secret agent, parking its handle in `nm_agent_out`.
fn start_network_manager(
    targets: WatchTargets,
    waiters: WaitersMap,
    nm_agent_out: &OnceLock<hytte_bus::ExportHandle>,
) -> WifiBackend {
    let device_path_store: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
    let store = Arc::clone(&device_path_store);
    let prompts = targets.prompts.clone();
    spawn_supervised("wifi", move || {
        crate::wifi_nm::run_nm_wifi_watcher(
            targets.station.clone(),
            targets.networks.clone(),
            targets.adapter.clone(),
            targets.wired.clone(),
            targets.vpn.clone(),
            store.clone(),
        )
    });

    // Mount the NM SecretAgent on the SYSTEM bus and register it with NM's
    // AgentManager. Unlike the iwd agent, NM secret agents do NOT own a
    // well-known name — NM records our system connection's unique name and
    // calls GetSecrets back on it, so we export the object name-lessly (no
    // system-bus policy entry needed). Export first, then register, so the
    // object is present before NM can call back.
    let nm_agent = nm_agent::NmAgent { prompts, waiters };
    let export =
        hytte_bus::export_object(BusKind::System, crate::wifi_nm::NM_AGENT_PATH).start(nm_agent);
    if let Err(duplicate) = nm_agent_out.set(export) {
        // Not reachable today — the decide-task returns once it has committed
        // and `spawn_supervised` does not restart a clean completion, so this
        // runs at most once. Guarded anyway because the failure is silent and
        // security-adjacent: dropping the rejected handle runs
        // `ExportHandle::drop`, whose *path-keyed* `unmount(NM_AGENT_PATH)`
        // takes down the interface the surviving export mounted, while that
        // surviving handle never notices. NM would still hold our secret-agent
        // registration, so `GetSecrets` starts failing and Wi-Fi/VPN passphrase
        // prompts stop appearing — with nothing logged above `debug`.
        tracing::error!(
            path = crate::wifi_nm::NM_AGENT_PATH,
            "wifi_nm: the NM secret agent was exported twice; dropping the duplicate unmounts \
             the live agent, so passphrase prompts will stop working until the shell is \
             restarted. This is a bug in the backend-commit path — please report it."
        );
        drop(duplicate);
    }

    runtime::handle().spawn(async {
        // Give the export a moment to mount on the live connection before
        // registering, so NM never calls back before the object exists.
        tokio::time::sleep(Duration::from_millis(500)).await;
        match crate::wifi_nm::register_nm_agent().await {
            Ok(()) => tracing::info!("wifi_nm: secret agent registered with NM"),
            Err(e) => {
                tracing::warn!(error = %e, "wifi_nm: secret agent registration failed");
            }
        }
    });

    WifiBackend::NetworkManager(device_path_store)
}

impl Service for WifiService {
    type Handles = WifiHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        // Initialise the WAITERS map once so public API functions can reach it.
        let waiters_arc: WaitersMap = Arc::new(AsyncMutex::new(HashMap::new()));
        let _ = WAITERS.set(waiters_arc.clone());

        let targets = WatchTargets {
            station: Mutable::new(None),
            networks: Mutable::new(Vec::new()),
            prompts: Mutable::new(None),
            adapter: Mutable::new(None),
            wired: Mutable::new(Vec::new()),
            vpn: Mutable::new(Vec::new()),
        };
        let backend_mutable: Mutable<WifiBackend> = Mutable::new(WifiBackend::None);
        let ownership_slot: Arc<OnceLock<hytte_bus::OwnNameSignal>> = Arc::new(OnceLock::new());
        let nm_agent_slot: Arc<OnceLock<hytte_bus::ExportHandle>> = Arc::new(OnceLock::new());

        // One supervised task owns the whole decision: probe (retrying while
        // inconclusive), then commit to exactly one backend. It runs off the
        // GTK main thread, so `start()` returns immediately and the shell never
        // blocks on the bus. Same shape as `networkd::NetworkdService::start`.
        //
        // The task returns once it has committed — `spawn_supervised` takes a
        // clean completion at face value and does not restart it, so a backend
        // is never started twice. The per-backend watchers keep their own
        // independent supervision.
        {
            let targets = targets.clone();
            let backend_m = backend_mutable.clone();
            let waiters_m = waiters_arc;
            let ownership_out = Arc::clone(&ownership_slot);
            let nm_agent_out = Arc::clone(&nm_agent_slot);

            spawn_supervised("wifi-backend", move || {
                let targets = targets.clone();
                let backend_m = backend_m.clone();
                let waiters_m = waiters_m.clone();
                let ownership_out = Arc::clone(&ownership_out);
                let nm_agent_out = Arc::clone(&nm_agent_out);

                async move {
                    let Some(choice) = probe_until_conclusive(PROBE_RETRY).await else {
                        // Give-up already logged at `error!`; stay inert.
                        return;
                    };

                    let backend = match choice {
                        BackendChoice::Iwd => start_iwd(targets, waiters_m, &ownership_out),
                        BackendChoice::NetworkManager => {
                            start_network_manager(targets, waiters_m, &nm_agent_out)
                        }
                        BackendChoice::None => {
                            // A positive finding: the bus answered and neither
                            // daemon is present. An *inconclusive* probe never
                            // lands here — it either recovered on a retry or gave
                            // up above (#613).
                            tracing::warn!("wifi: no Wi-Fi backend present — service is inactive");
                            WifiBackend::None
                        }
                    };

                    // Published last, so a command can never see a backend whose
                    // watcher has not been spawned yet.
                    backend_m.set(backend);
                }
            });
        }

        WifiHandles {
            station: targets.station,
            networks: targets.networks,
            prompts: targets.prompts,
            adapter: targets.adapter,
            wired: targets.wired,
            vpn: targets.vpn,
            _ownership: ownership_slot,
            _nm_agent: nm_agent_slot,
            backend: backend_mutable,
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the Wi-Fi service to register with the hytte runtime.
#[must_use]
pub fn service() -> WifiService {
    WifiService
}

/// Signal that emits the current station state, or `None` when no adapter
/// is present or iwd is not running.
pub fn station() -> impl Signal<Item = Option<Station>> {
    registry::with(|r| {
        r.get::<WifiHandles>()
            .expect("wifi::service() not registered")
            .station
            .signal_cloned()
    })
}

/// Signal emitting the current Adapter snapshot, or `None` when no adapter
/// is present.
pub fn adapter() -> impl Signal<Item = Option<Adapter>> {
    registry::with(|r| {
        r.get::<WifiHandles>()
            .expect("wifi::service() not registered")
            .adapter
            .signal_cloned()
    })
}

/// Signal that emits the current list of visible networks (ordered by signal
/// strength as returned by `GetOrderedNetworks`).
pub fn networks() -> impl Signal<Item = Vec<WifiNetwork>> {
    registry::with(|r| {
        r.get::<WifiHandles>()
            .expect("wifi::service() not registered")
            .networks
            .signal_cloned()
    })
}

/// Signal emitting the saved wired (ethernet) NM connection profiles, sorted by
/// name. Empty on the iwd / no backend (only `NetworkManager` manages ethernet).
pub fn wired_profiles() -> impl Signal<Item = Vec<WiredProfile>> {
    registry::with(|r| {
        r.get::<WifiHandles>()
            .expect("wifi::service() not registered")
            .wired
            .signal_cloned()
    })
}

/// Signal emitting the saved VPN NM connection profiles, sorted by name. Empty
/// on the iwd / no backend (only `NetworkManager` manages VPN profiles).
///
/// Distinct from [`crate::vpn::tunnels`], which polls the live tunnel
/// interfaces — these are NM *connection profiles* the user can activate.
pub fn vpn_profiles() -> impl Signal<Item = Vec<VpnProfile>> {
    registry::with(|r| {
        r.get::<WifiHandles>()
            .expect("wifi::service() not registered")
            .vpn
            .signal_cloned()
    })
}

/// Read the active [`WifiBackend`] from the registry.
///
/// A snapshot of the current value: the backend is committed by the probe task
/// (see [`probe_until_conclusive`]), so this reports [`WifiBackend::None`] until
/// the probe concludes — the same window in which the watcher has not yet
/// discovered a station and every command already no-ops.
fn get_backend() -> WifiBackend {
    registry::with(|r| {
        r.get::<WifiHandles>()
            .expect("wifi::service() not registered")
            .backend
            .get_cloned()
    })
}

/// Fire-and-forget: trigger a Wi-Fi scan on the station.
pub fn scan() {
    match get_backend() {
        WifiBackend::Iwd => {
            runtime::handle().spawn(async move {
                let path = get_station_path().await;
                if path.is_empty() {
                    tracing::warn!("wifi::scan: no station path known");
                    return;
                }
                if let Err(e) = do_station_call(&path, "Scan").await {
                    tracing::warn!(error = %e, "wifi scan failed");
                }
            });
        }
        WifiBackend::NetworkManager(store) => {
            runtime::handle().spawn(async move {
                let path = store.read().await.clone();
                if path.is_empty() {
                    tracing::warn!("wifi::scan: NM device path not yet known");
                    return;
                }
                if let Err(e) = crate::wifi_nm::nm_scan(&path).await {
                    tracing::warn!(error = %e, "wifi scan (NM) failed");
                }
            });
        }
        WifiBackend::None => {
            tracing::warn!("wifi::scan: no backend available");
        }
    }
}

/// Whether the network at `network_path` in the current scan snapshot already
/// has stored credentials / a saved profile.
///
/// Read from the registry on the calling (GTK) thread — same as
/// [`get_backend`] — so the answer can be moved into the spawned task. Networks
/// that have dropped out of the snapshot report `false`.
fn network_is_known(network_path: &str) -> bool {
    registry::with(|r| {
        r.get::<WifiHandles>()
            .expect("wifi::service() not registered")
            .networks
            .lock_ref()
            .iter()
            .any(|n| n.path == network_path && n.known)
    })
}

/// Fire-and-forget: connect to the network at `network_path`.
///
/// For the iwd backend, `network_path` is an iwd `Network` object path.
/// For the `NetworkManager` backend, `network_path` is an NM `AccessPoint` object path.
///
/// The `NetworkManager` backend needs to know whether a profile already exists
/// for the target: joining a *new* network takes NM's
/// `AddAndActivateConnection` (which creates one), not `ActivateConnection`
/// (which can only select one). That flag comes from the scan snapshot the user
/// clicked — see `crate::wifi_nm::nm_connect`.
pub fn connect_network(network_path: &str) {
    let path = network_path.to_string();
    match get_backend() {
        WifiBackend::Iwd => {
            runtime::handle().spawn(async move {
                if let Err(e) = do_network_call(&path, "Connect").await {
                    tracing::warn!(error = %e, path, "wifi connect_network failed (may need agent)");
                    crate::notifications::post_local(
                        "Wi-Fi",
                        "Wi-Fi connection failed",
                        &e.to_string(),
                        crate::notifications::Urgency::Critical,
                    );
                }
            });
        }
        WifiBackend::NetworkManager(store) => {
            let known = network_is_known(network_path);
            runtime::handle().spawn(async move {
                let device_path = store.read().await.clone();
                if device_path.is_empty() {
                    tracing::warn!("wifi::connect_network: NM device path not yet known");
                    return;
                }
                if let Err(e) = crate::wifi_nm::nm_connect(&device_path, &path, known).await {
                    tracing::warn!(error = %e, path, "wifi connect_network (NM) failed");
                    crate::notifications::post_local(
                        "Wi-Fi",
                        "Wi-Fi connection failed",
                        &e.to_string(),
                        crate::notifications::Urgency::Critical,
                    );
                }
            });
        }
        WifiBackend::None => {
            tracing::warn!("wifi::connect_network: no backend available");
        }
    }
}

/// Fire-and-forget: disconnect from the current network.
pub fn disconnect() {
    match get_backend() {
        WifiBackend::Iwd => {
            runtime::handle().spawn(async move {
                let path = get_station_path().await;
                if path.is_empty() {
                    tracing::warn!("wifi::disconnect: no station path known");
                    return;
                }
                if let Err(e) = do_station_call(&path, "Disconnect").await {
                    tracing::warn!(error = %e, "wifi disconnect failed");
                    crate::notifications::post_local(
                        "Wi-Fi",
                        "Wi-Fi disconnect failed",
                        &e.to_string(),
                        crate::notifications::Urgency::Critical,
                    );
                }
            });
        }
        WifiBackend::NetworkManager(store) => {
            runtime::handle().spawn(async move {
                let path = store.read().await.clone();
                if path.is_empty() {
                    tracing::warn!("wifi::disconnect: NM device path not yet known");
                    return;
                }
                if let Err(e) = crate::wifi_nm::nm_disconnect(&path).await {
                    tracing::warn!(error = %e, "wifi disconnect (NM) failed");
                    crate::notifications::post_local(
                        "Wi-Fi",
                        "Wi-Fi disconnect failed",
                        &e.to_string(),
                        crate::notifications::Urgency::Critical,
                    );
                }
            });
        }
        WifiBackend::None => {
            tracing::warn!("wifi::disconnect: no backend available");
        }
    }
}

/// Fire-and-forget: set the radio powered state.
///
/// For iwd, sets `Powered` on the `Adapter1` object.
/// For `NetworkManager`, sets `WirelessEnabled` on the manager.
pub fn set_powered(on: bool) {
    match get_backend() {
        WifiBackend::Iwd => {
            runtime::handle().spawn(async move {
                let path = current_adapter_path().await;
                if path.is_empty() {
                    tracing::warn!("wifi::set_powered: no adapter path known");
                    return;
                }
                if let Err(e) = do_set_powered(&path, on).await {
                    tracing::warn!(error = %e, on, "wifi set_powered failed");
                }
            });
        }
        WifiBackend::NetworkManager(_) => {
            runtime::handle().spawn(async move {
                if let Err(e) = crate::wifi_nm::nm_set_powered(on).await {
                    tracing::warn!(error = %e, on, "wifi set_powered (NM) failed");
                }
            });
        }
        WifiBackend::None => {
            tracing::warn!("wifi::set_powered: no backend available");
        }
    }
}

/// Fire-and-forget: forget the network with the given path.
///
/// For iwd, calls `Forget` on the `KnownNetwork` object at `known_network_path`.
/// For `NetworkManager`, calls `Settings.Connection.Delete` on the saved
/// connection profile at `known_network_path` (the path the watcher records in
/// [`WifiNetwork::known_network_path`] from the saved-connection enumeration).
pub fn forget(known_network_path: &str) {
    let path = known_network_path.to_string();
    match get_backend() {
        WifiBackend::Iwd => {
            runtime::handle().spawn(async move {
                if let Err(e) = do_known_network_call(&path, "Forget").await {
                    tracing::warn!(error = %e, path, "wifi forget failed");
                }
            });
        }
        WifiBackend::NetworkManager(_) => {
            runtime::handle().spawn(async move {
                if path.is_empty() {
                    tracing::warn!("wifi::forget: NM connection path is empty");
                    return;
                }
                if let Err(e) = crate::wifi_nm::nm_forget(&path).await {
                    tracing::warn!(error = %e, path, "wifi forget (NM) failed");
                }
            });
        }
        WifiBackend::None => {
            tracing::warn!("wifi::forget: no backend available");
        }
    }
}

/// Fire-and-forget: activate the saved wired (ethernet) profile at
/// `connection_path` on `device_path` (NM `ActivateConnection`).
///
/// NM-only: wired profiles are surfaced solely by the `NetworkManager` backend,
/// so this is a no-op on iwd / no backend (there are never any wired profiles to
/// act on there).
pub fn wired_activate(connection_path: &str, device_path: &str) {
    let conn = connection_path.to_string();
    let dev = device_path.to_string();
    let WifiBackend::NetworkManager(_) = get_backend() else {
        tracing::warn!("wifi::wired_activate: NM backend not active");
        return;
    };
    if conn.is_empty() || dev.is_empty() {
        tracing::warn!("wifi::wired_activate: empty connection or device path");
        return;
    }
    runtime::handle().spawn(async move {
        if let Err(e) = crate::wifi_nm::nm_activate_connection(&conn, &dev).await {
            tracing::warn!(error = %e, conn, dev, "wired activate (NM) failed");
        }
    });
}

/// Fire-and-forget: deactivate the wired connection on `device_path`
/// (NM `Device.Disconnect`). NM-only (see [`wired_activate`]).
pub fn wired_deactivate(device_path: &str) {
    let dev = device_path.to_string();
    let WifiBackend::NetworkManager(_) = get_backend() else {
        tracing::warn!("wifi::wired_deactivate: NM backend not active");
        return;
    };
    if dev.is_empty() {
        tracing::warn!("wifi::wired_deactivate: empty device path");
        return;
    }
    runtime::handle().spawn(async move {
        if let Err(e) = crate::wifi_nm::nm_disconnect(&dev).await {
            tracing::warn!(error = %e, dev, "wired deactivate (NM) failed");
        }
    });
}

/// Fire-and-forget: forget (delete) the saved wired profile at
/// `connection_path` (NM `Settings.Connection.Delete`). NM-only (see
/// [`wired_activate`]).
pub fn wired_forget(connection_path: &str) {
    let conn = connection_path.to_string();
    let WifiBackend::NetworkManager(_) = get_backend() else {
        tracing::warn!("wifi::wired_forget: NM backend not active");
        return;
    };
    if conn.is_empty() {
        tracing::warn!("wifi::wired_forget: empty connection path");
        return;
    }
    runtime::handle().spawn(async move {
        if let Err(e) = crate::wifi_nm::nm_forget(&conn).await {
            tracing::warn!(error = %e, conn, "wired forget (NM) failed");
        }
    });
}

/// Fire-and-forget: activate the saved VPN profile at `connection_path`
/// (NM `ActivateConnection` with `"/"` for device + specific-object — a VPN
/// rides the primary connection, see [`crate::wifi_nm::nm_activate_vpn`]).
///
/// NM-only: VPN profiles are surfaced solely by the `NetworkManager` backend, so
/// this is a no-op on iwd / no backend. If the profile needs credentials NM
/// doesn't hold, our secret agent surfaces an interactive prompt.
pub fn vpn_activate(connection_path: &str) {
    let conn = connection_path.to_string();
    let WifiBackend::NetworkManager(_) = get_backend() else {
        tracing::warn!("wifi::vpn_activate: NM backend not active");
        return;
    };
    if conn.is_empty() {
        tracing::warn!("wifi::vpn_activate: empty connection path");
        return;
    }
    runtime::handle().spawn(async move {
        if let Err(e) = crate::wifi_nm::nm_activate_vpn(&conn).await {
            tracing::warn!(error = %e, conn, "vpn activate (NM) failed");
            crate::notifications::post_local(
                "VPN",
                "VPN connection failed",
                &e.to_string(),
                crate::notifications::Urgency::Critical,
            );
        }
    });
}

/// Fire-and-forget: deactivate the active VPN connection at
/// `active_connection_path` (NM `Manager.DeactivateConnection` — **not**
/// `Device.Disconnect`, which is wrong for a device-less VPN).
///
/// `active_connection_path` is the *active-connection* object path captured in
/// [`VpnProfile::active_connection_path`] while the profile is up. NM-only (see
/// [`vpn_activate`]).
pub fn vpn_deactivate(active_connection_path: &str) {
    let active = active_connection_path.to_string();
    let WifiBackend::NetworkManager(_) = get_backend() else {
        tracing::warn!("wifi::vpn_deactivate: NM backend not active");
        return;
    };
    if active.is_empty() {
        tracing::warn!("wifi::vpn_deactivate: empty active-connection path");
        return;
    }
    runtime::handle().spawn(async move {
        if let Err(e) = crate::wifi_nm::nm_deactivate_connection(&active).await {
            tracing::warn!(error = %e, active, "vpn deactivate (NM) failed");
            crate::notifications::post_local(
                "VPN",
                "VPN disconnect failed",
                &e.to_string(),
                crate::notifications::Urgency::Critical,
            );
        }
    });
}

/// Signal emitting `Some(PromptRequest)` when iwd needs a passphrase, `None`
/// otherwise.  Only one prompt can be active at a time — v0.6.1 serialises.
pub fn active_prompt() -> impl Signal<Item = Option<PromptRequest>> {
    registry::with(|r| {
        r.get::<WifiHandles>()
            .expect("wifi::service() not registered")
            .prompts
            .signal_cloned()
    })
}

/// Submit the secret(s) for the prompt with `id`.
///
/// `secrets` carries **one value per [`PromptRequest::secret_keys`] entry, in
/// the same order**. A Wi-Fi passphrase is a one-element slice; a VPN round
/// that asked for several secrets returns all of them here, so the agent can
/// answer `NetworkManager`'s whole `GetSecrets` request in a single reply
/// instead of leaving the rest unfilled for NM to re-ask or fail on.
///
/// Resolving the waiter *removes* it, so a second submit for the same `id` is
/// a silent no-op — the overlay's in-flight latch is the first line of defence
/// against a double submit, this is the second.
pub fn submit_prompt(id: u64, secrets: &[String]) {
    let secrets = secrets.to_vec();
    let Some(arc) = waiters() else { return };
    let arc = arc.clone();
    runtime::handle().spawn(async move {
        let mut map = arc.lock().await;
        if let Some(tx) = map.remove(&id) {
            let _ = tx.send(Ok(secrets));
        }
    });
}

/// Dismiss the prompt with `id` without submitting (signals `Error.Canceled`).
pub fn cancel_prompt(id: u64) {
    let Some(arc) = waiters() else { return };
    let arc = arc.clone();
    runtime::handle().spawn(async move {
        let mut map = arc.lock().await;
        if let Some(tx) = map.remove(&id) {
            let _ = tx.send(Err("cancelled".into()));
        }
    });
}

// ── Command helpers ───────────────────────────────────────────────────────────

async fn do_station_call(path: &str, method: &str) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, "net.connman.iwd")
        .at_path(path.to_string())
        .iface("net.connman.iwd.Station")
        .method(method)
        .args(())
        .send::<()>()
        .await
}

async fn do_network_call(path: &str, method: &str) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, "net.connman.iwd")
        .at_path(path.to_string())
        .iface("net.connman.iwd.Network")
        .method(method)
        .args(())
        .send::<()>()
        .await
}

async fn do_set_powered(adapter_path: &str, on: bool) -> Result<(), hytte_bus::BusError> {
    let value = zbus::zvariant::Value::from(on)
        .try_to_owned()
        .map_err(|e| hytte_bus::BusError::Permanent {
            reason: e.to_string(),
            dbus_name: None,
        })?;
    hytte_bus::call(BusKind::System, "net.connman.iwd")
        .at_path(adapter_path.to_string())
        .iface("org.freedesktop.DBus.Properties")
        .method("Set")
        .args(("net.connman.iwd.Adapter1", "Powered", value))
        .send::<()>()
        .await
}

async fn do_known_network_call(path: &str, method: &str) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(BusKind::System, "net.connman.iwd")
        .at_path(path.to_string())
        .iface("net.connman.iwd.KnownNetwork")
        .method(method)
        .args(())
        .send::<()>()
        .await
}

#[cfg(test)]
mod tests {
    use super::{BackendChoice, Duration, retry};
    use crate::wifi_backend::ProbeError;

    // Since #646 the retry *mechanism* — the doubling schedule, the attempt
    // budget, the give-up boundary — is tested once, in `crate::retry`, over the
    // shared type; so are the shipped constants' delay invariants (#665), over
    // *every* shipped policy rather than this one. What is asserted here is the
    // part that is specific to this probe: which outcomes count as an answer.

    /// Test policy: a small, explicit budget so the give-up boundary is easy to
    /// name. Deliberately not [`super::PROBE_RETRY`] — these tests assert the
    /// *decision*, not the shipped numbers, so tuning the policy does not
    /// redden them.
    fn bounded() -> retry::Policy {
        retry::Policy {
            max_attempts: Some(3),
            initial: Duration::from_millis(10),
            max_backoff: Duration::from_millis(40),
        }
    }

    /// The "I could not ask" outcome: both bus queries failed.
    fn inconclusive() -> Result<BackendChoice, ProbeError> {
        Err(ProbeError::Both {
            list_names: "bus not reachable".to_string(),
            list_activatable_names: "bus not reachable".to_string(),
        })
    }

    // ── A conclusive probe commits, on whatever attempt produced it ───────────

    #[test]
    fn ok_commits_on_the_first_attempt() {
        assert_eq!(
            bounded().step(&Ok::<_, ProbeError>(BackendChoice::NetworkManager), 1),
            retry::Step::Proceed
        );
        assert_eq!(
            bounded().step(&Ok::<_, ProbeError>(BackendChoice::Iwd), 1),
            retry::Step::Proceed
        );
    }

    /// `BackendChoice::None` is a *positive* finding — the bus replied and
    /// neither daemon is present. Retrying "nobody is there" would be a bug of
    /// its own (an endless poll on a host that simply has no Wi-Fi daemon), so
    /// it must commit exactly like the other two verdicts.
    #[test]
    fn ok_none_commits_and_does_not_retry() {
        assert_eq!(
            bounded().step(&Ok::<_, ProbeError>(BackendChoice::None), 1),
            retry::Step::Proceed
        );
    }

    /// The #613 regression itself: an inconclusive probe must never commit to
    /// "no backend". That collapse is what latched Wi-Fi off for a whole
    /// session and made a shell restart the only cure (#607).
    ///
    /// Since #646 the type system carries half of this: `retry::Step::Proceed`
    /// has no payload, so `probe_until_conclusive` can only ever return a
    /// `BackendChoice` it read out of an `Ok` — there is no longer a code path
    /// that could manufacture `None` from a `ProbeError`. What is left to assert
    /// is that an inconclusive probe does not reach that arm at all.
    #[test]
    fn inconclusive_probe_never_commits_to_no_backend() {
        let policy = bounded();
        for attempt in 1..=12_u32 {
            let step = policy.step(&inconclusive(), attempt);
            assert_ne!(
                step,
                retry::Step::Proceed,
                "attempt {attempt}: an inconclusive probe was treated as an answer"
            );
            assert!(
                matches!(step, retry::Step::Retry { .. } | retry::Step::GiveUp),
                "attempt {attempt}: expected retry-or-give-up, got {step:?}"
            );
        }
    }

    // ── Policy shape ─────────────────────────────────────────────────────────

    /// The policy this service actually ships must be the unbounded one: a
    /// bounded give-up would re-create #607 (Wi-Fi gone, restart the only cure)
    /// whenever the bus takes longer than the bound. A bounded policy stays
    /// expressible, and `crate::retry`'s tests keep its give-up path covered.
    ///
    /// The delays are asserted in `crate::retry` over every shipped policy —
    /// deliberately not here, and deliberately never against
    /// `PROBE_RETRY.backoff(attempt)`, which is the expression `step` computes
    /// internally and therefore pins nothing (#665).
    #[test]
    fn the_shipped_policy_never_goes_inert() {
        assert_eq!(
            super::PROBE_RETRY.max_attempts,
            None,
            "the shipped probe policy must never give up; see PROBE_RETRY's docs and #613"
        );
        for attempt in [1_u32, 3, 100, u32::MAX] {
            assert!(
                matches!(
                    super::PROBE_RETRY.step(&inconclusive(), attempt),
                    retry::Step::Retry { .. }
                ),
                "attempt {attempt}: an inconclusive probe ended the task instead of retrying"
            );
        }
    }
}
