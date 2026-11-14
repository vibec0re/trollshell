//! Main iwd watcher task: discovery loop, D-Bus event pump, and signal
//! subscription management.

use futures_signals::signal::Mutable;
use futures_util::{Stream, StreamExt};
use hytte_bus::{BusKind, SignalItem, SignalSubscription};
use std::collections::HashMap;
use std::time::Duration;
use zbus::zvariant::OwnedValue;

use super::client::{
    discover_station, is_already_registered, iwd_has_owner, refresh_networks, refresh_station,
    register_iwd_agent,
};
use super::parse::{
    IWD_ADAPTER_IFACE, adapter_path_from_station, apply_adapter_props_delta,
    apply_station_props_delta, refresh_adapter_from_managed, refresh_station_from_managed,
    station_removed_from_event,
};
use super::types::{Adapter, PromptRequest, Station, StationState, WifiNetwork};
use super::{AGENT_PATH, IwdPaths};
use crate::handshake::Handshake;
use crate::retry;

// ── Subscriptions ─────────────────────────────────────────────────────────────

/// The bus's `NameOwnerChanged`, every name, filtered in code by
/// [`iwd_owner_change`]. The `BlueZ` agent watches its daemon the same way.
fn name_owner_changed() -> SignalSubscription {
    hytte_bus::signals(BusKind::System, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .signal("NameOwnerChanged")
        .start()
}

/// One of iwd's `ObjectManager` signals (`InterfacesAdded` or
/// `InterfacesRemoved`) on its root object.
fn object_manager_signal(signal: &str) -> SignalSubscription {
    hytte_bus::signals(BusKind::System, "net.connman.iwd")
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .signal(signal)
        .start()
}

/// `PropertiesChanged` on the station object.
fn station_props_signal(station_path: &str) -> SignalSubscription {
    hytte_bus::signals(BusKind::System, "net.connman.iwd")
        .at_path(station_path.to_string())
        .iface("org.freedesktop.DBus.Properties")
        .signal("PropertiesChanged")
        .start()
}

/// The streams [`pump_iwd_events`] reads, all four with their markers, and
/// the station's subscription itself. The first three are receivers taken
/// for one discovery from subscriptions [`run_wifi_watcher`] holds for its
/// whole life. The station's subscription is made per discovery, since its
/// path comes from the snapshot.
struct IwdStreams<'a, O, E, H> {
    /// iwd's owner: [`name_owner_changed`].
    owner: &'a mut O,
    /// `InterfacesAdded` on iwd's root object.
    added: &'a mut O,
    /// `InterfacesRemoved` on iwd's root object.
    removed: &'a mut O,
    /// `PropertiesChanged` on the station.
    station: &'a mut E,
    /// The station's subscription, for its handshake: the pump reads the
    /// station again once the bus has its match rule.
    station_rule: &'a H,
}

/// The calls to iwd the pump makes on a schedule of its own, injected so a
/// test can drive the pump without a bus.
struct IwdCalls<R, S> {
    /// `RegisterAgent` ([`register_iwd_agent`]), retried until iwd takes it
    /// ([`register_agent_until_accepted`]).
    register: R,
    /// The station read again once its rule is in place ([`reread_station`]).
    reread_station: S,
}

/// How long discovery waits before trying again when iwd did not answer, or
/// had no station yet.
const DISCOVERY_RETRY: Duration = Duration::from_secs(5);

/// What one `NameOwnerChanged` item means for the iwd watcher.
#[derive(Debug, PartialEq, Eq)]
enum IwdOwner {
    /// `net.connman.iwd` has no owner any more: iwd exited.
    Lost,
    /// `net.connman.iwd` has a new owner: an iwd that started after the one
    /// our agent registered with.
    Appeared,
    /// The subscription reported a hole (`Resubscribed`/`Lagged`). An iwd
    /// restart may have gone unseen. A `Resubscribed` is also how a reconnect
    /// of our own system-bus connection shows up here, and a reconnect gives
    /// this process a new unique name, the one iwd keys our agent to.
    Hole,
}

/// Classify one item from [`IwdStreams::owner`]. `None` for any name other
/// than iwd's.
fn iwd_owner_change(item: &SignalItem) -> Option<IwdOwner> {
    match item {
        SignalItem::Event(evt) => {
            let (name, _old, new) = evt
                .body
                .body()
                .deserialize::<(String, String, String)>()
                .ok()?;
            if name != crate::wifi_backend::IWD_BUS_NAME {
                return None;
            }
            Some(if new.is_empty() {
                IwdOwner::Lost
            } else {
                IwdOwner::Appeared
            })
        }
        SignalItem::Resubscribed | SignalItem::Lagged { .. } => Some(IwdOwner::Hole),
    }
}

/// What [`pump_iwd_events`] does about an [`IwdOwner`] change.
#[derive(Debug, PartialEq, Eq)]
struct OwnerReaction {
    /// Clear station, networks, prompt and adapter, as for a removed station.
    clear: bool,
    /// Return to `'discovery`. That re-reads iwd's objects, re-subscribes and
    /// calls `RegisterAgent` again.
    rediscover: bool,
}

/// The policy for iwd changing hands.
///
/// The agent registration is the reason this exists. iwd forgets every agent
/// when it exits, and the watcher used to register once per discovery pass.
/// The pump only went back to discovery on an `InterfacesRemoved` for the
/// station, which an iwd that crashes or is killed never sends. After such
/// a restart the panel kept working from signals, but iwd had no agent: the
/// next network that needed a passphrase failed with `NoAgent` and no
/// prompt.
///
/// - **Appeared**: rediscover. Not a `RegisterAgent` on the spot: iwd takes
///   its bus name before it initialises its modules, the agent manager
///   among them (`dbus_ready` then `nl80211_appeared` in `src/main.c`), so a
///   call right at the name change can find no `AgentManager`. Discovery
///   registers only once a `Station` exists, and by then the agent manager
///   exists too. The old daemon's state is cleared first.
/// - **Hole**: rediscover too: the registration may be gone with our old
///   unique name. The state is kept, because iwd itself may be fine and
///   discovery refreshes it in place.
/// - **Lost**: clear the state and wait here for the new owner. Rediscovering
///   now would be a call to `net.connman.iwd`, and iwd is D-Bus-activatable
///   (`SystemdService=iwd.service`), so that would restart an iwd that was
///   stopped on purpose.
fn owner_reaction(change: &IwdOwner) -> OwnerReaction {
    match change {
        IwdOwner::Lost => OwnerReaction {
            clear: true,
            rediscover: false,
        },
        IwdOwner::Appeared => OwnerReaction {
            clear: true,
            rediscover: true,
        },
        IwdOwner::Hole => OwnerReaction {
            clear: false,
            rediscover: true,
        },
    }
}

// ── Waiting for iwd ───────────────────────────────────────────────────────────

/// How often [`wait_for_iwd`] asks the bus again without having been woken.
/// It is a backstop; the wake is the `NameOwnerChanged` for iwd's new owner.
/// `bluetooth`'s `BLUEZ_RECHECK` is the same number for the same wait.
const IWD_RECHECK: Duration = Duration::from_secs(10);

/// Wait until iwd's name has an owner, without starting iwd (#1551).
///
/// This asks the bus ([`iwd_has_owner`]), never iwd. Any call to
/// `net.connman.iwd` D-Bus-activates `iwd.service`, so rediscovering as soon
/// as the pump returned turned a deliberate `systemctl stop iwd` into a
/// restart whenever the stopping iwd removed its station first, as a clean
/// stop does. `bluetooth`'s `wait_for_bluez` is the same wait for bluetoothd.
///
/// It reads `owner`, a receiver on the watcher's own subscription, rather
/// than opening a subscription: that one's match rule is long in place, and
/// the owner change that wakes this is consumed here instead of reaching the
/// next pump as a change that predates its snapshot.
///
/// `gone` clears what the watcher published, and runs once the bus says iwd
/// has no owner, before the wait (#1654). The pump clears it when it sees
/// iwd leave, but an iwd that leaves after a pump returned on a hole, or
/// inside the hole, is seen by no pump: until #1654 the panel kept the old
/// station while iwd was down.
async fn wait_for_iwd<S>(owner: &mut S, gone: impl FnOnce())
where
    S: Stream<Item = SignalItem> + Unpin,
{
    wait_for_owner(owner, iwd_has_owner, IWD_RECHECK, gone).await;
}

/// [`wait_for_iwd`] with the bus question and the re-check interval injected,
/// so a test can drive it without a bus under paused time.
///
/// Asks `has_owner`, and returns on the first `Ok(true)`. Otherwise waits for
/// the next sign that iwd may be back ([`next_iwd_owner`]) or `recheck`,
/// whichever comes first, and asks again. A failed question is asked again
/// the same way.
///
/// `gone` runs on the first `Ok(false)`, before the wait, and never on a
/// failed question: that one says nothing about iwd. An iwd that has an owner
/// at the first question, the healthy rediscovery, never runs it.
async fn wait_for_owner<S, H, F>(
    owner: &mut S,
    has_owner: H,
    recheck: Duration,
    gone: impl FnOnce(),
) where
    S: Stream<Item = SignalItem> + Unpin,
    H: Fn() -> F,
    F: Future<Output = Result<bool, hytte_bus::BusError>>,
{
    let mut gone = Some(gone);
    loop {
        match has_owner().await {
            Ok(true) => return,
            Ok(false) => {
                if let Some(gone) = gone.take() {
                    tracing::info!("wifi: iwd is not running; waiting for it to return");
                    gone();
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "wifi: NameHasOwner(net.connman.iwd) failed; asking again");
            }
        }
        let _ = tokio::time::timeout(recheck, next_iwd_owner(owner)).await;
    }
}

/// Resolve on the next sign that iwd may have an owner again: its name
/// gaining one, or a marker, since an owner change may hide behind it. A
/// stream that ends never resolves, so the caller's re-check paces it instead
/// of letting it spin.
async fn next_iwd_owner<S>(owner: &mut S)
where
    S: Stream<Item = SignalItem> + Unpin,
{
    while let Some(item) = owner.next().await {
        match iwd_owner_change(&item) {
            Some(IwdOwner::Appeared | IwdOwner::Hole) => return,
            Some(IwdOwner::Lost) | None => {}
        }
    }
    std::future::pending::<()>().await;
}

// ── Agent registration ────────────────────────────────────────────────────────

/// Register our agent with iwd, retrying a refusal until iwd takes it (#1551).
/// Returns once it has.
///
/// Until #1551 a failed `RegisterAgent` was `warn!`ed and not retried until
/// iwd next changed hands, and until then iwd had no agent: a network that
/// needed a passphrase failed with `NoAgent` and no prompt. The retry runs on
/// [`super::nm_registrar::REGISTER_RETRY`], the `NetworkManager` agent's
/// ramp, for its reason: a budget that runs out leaves the shell deaf for the
/// rest of the session, so the attempts are unbounded and the *delay* is
/// bounded, at 30 s. A permanent refusal is one `warn!`, then `debug!` until
/// it heals ([`retry::FailureLatch`]).
///
/// iwd's `AlreadyExists` counts as registered ([`is_already_registered`]):
/// it is what iwd answers when it still holds this connection's agent.
///
/// `register` and `policy` are injected so a test can script the answers and
/// count attempts under paused time.
async fn register_agent_until_accepted<R, F>(register: R, policy: retry::Policy)
where
    R: Fn() -> F,
    F: Future<Output = Result<(), hytte_bus::BusError>>,
{
    let mut latch = retry::FailureLatch::new();
    // 1-based, and counts the attempt that produced the outcome being
    // weighed, per `retry::Policy::step`.
    let mut attempt: u32 = 1;
    loop {
        let outcome = match register().await {
            Err(e) if is_already_registered(&e) => {
                tracing::debug!("iwd already holds this connection's agent");
                Ok(())
            }
            other => other,
        };
        let verdict = policy.step(&outcome, attempt);
        log_registration(&outcome, latch.record(outcome.is_ok()), attempt, verdict);
        match verdict {
            retry::Step::Retry { after } => {
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(after).await;
            }
            // `GiveUp` is unreachable under the unbounded ramp.
            retry::Step::Proceed | retry::Step::GiveUp => return,
        }
    }
}

/// What one registration attempt is worth saying, given the ones before it.
/// `nm_registrar`'s `log_attempt` is the same cadence for the NM agent.
fn log_registration(
    outcome: &Result<(), hytte_bus::BusError>,
    report: retry::Report,
    attempt: u32,
    verdict: retry::Step,
) {
    let error = outcome
        .as_ref()
        .err()
        .map_or_else(String::new, ToString::to_string);
    let retry_in_secs = match verdict {
        retry::Step::Retry { after } => after.as_secs_f64(),
        retry::Step::Proceed | retry::Step::GiveUp => 0.0,
    };
    match report {
        retry::Report::Quiet => tracing::info!("hytte iwd agent registered"),
        retry::Report::Recovered => tracing::info!(
            failed_attempts = attempt.saturating_sub(1),
            "hytte iwd agent registered after earlier attempts failed"
        ),
        retry::Report::Opened => tracing::warn!(
            attempt,
            retry_in_secs,
            %error,
            "iwd RegisterAgent failed — iwd cannot ask this shell for a passphrase until it \
             succeeds. Retrying with backoff, and again whenever iwd changes hands; this line \
             does not repeat while the failure does"
        ),
        retry::Report::Repeating => tracing::debug!(
            attempt,
            retry_in_secs,
            %error,
            "iwd RegisterAgent still failing"
        ),
    }
}

// ── Main watcher task ─────────────────────────────────────────────────────────

/// Clear everything the watcher publishes about iwd: the station, its
/// networks, a pending prompt and the adapter.
///
/// The pump does this when it sees iwd leave, a new iwd take its name or the
/// station removed. Since #1654 a discovery pass does it too, when it finds
/// iwd's name without an owner ([`wait_for_owner`]) or iwd without a station:
/// either can happen while no pump is running, after one returned on a hole,
/// and nothing else would clear what the last pass published.
///
/// A value that is already clear is left alone, so a pass that keeps
/// finding no station (the radio is off, and discovery retries every
/// [`DISCOVERY_RETRY`]) does not wake the panel each time.
fn clear_published(
    station: &Mutable<Option<Station>>,
    networks: &Mutable<Vec<WifiNetwork>>,
    prompts: &Mutable<Option<PromptRequest>>,
    adapter: &Mutable<Option<Adapter>>,
) {
    if station.lock_ref().is_some() {
        station.set(None);
    }
    if !networks.lock_ref().is_empty() {
        networks.set(Vec::new());
    }
    if prompts.lock_ref().is_some() {
        prompts.set(None);
    }
    if adapter.lock_ref().is_some() {
        adapter.set(None);
    }
}

/// What [`open_discovery`] hands the rest of a discovery pass.
struct Opened<O, E, T> {
    /// The pass's owner receiver, after the wait (if any) read it.
    owner: O,
    /// The pass's `InterfacesAdded` receiver.
    added: E,
    /// The pass's `InterfacesRemoved` receiver.
    removed: E,
    /// The snapshot: [`discover_station`]'s answer in production.
    snapshot: T,
}

/// Open one discovery pass, in the order #1551 needs, with every step
/// injected so a test can record the order (#1573 review: until then the
/// order lived only in [`run_wifi_watcher`]'s body, and moving the wait or a
/// receiver left the suite green).
///
/// 1. A fresh owner receiver (`take_owner`), taken before the wait reads it.
/// 2. Except on the watcher's first pass, the wait for iwd to have an owner
///    (`wait`), so a later discovery never D-Bus-activates a stopped iwd. The
///    first pass is the one call the backend probe may rely on to start iwd.
/// 3. Fresh iwd receivers (`take_iwd`), after the wait and before the
///    snapshot, so every change made while the snapshot is taken is queued.
/// 4. The wait for the bus to have every subscription's match rule
///    (`subscriptions`, `hytte-bus`'s handshake), after the receivers exist
///    and before the snapshot is asked for. A subscription installs its rule
///    from a spawned task, so on the watcher's first pass the snapshot could
///    otherwise be answered before the bus routed anything here. On a later
///    pass the rules are long in place and this costs nothing; after a
///    reconnect of our own bus connection it waits for the rebuilt rules of
///    every subscription whose task has seen the reconnect. One that has not
///    seen it yet still answers for its dead rule, and its `Resubscribed`
///    marker covers that pass instead ([`run_wifi_watcher`]). The wait is
///    made here, on the subscriptions themselves, so the caller cannot leave
///    it out.
/// 5. The snapshot (`discover`).
async fn open_discovery<O, E, T, H, W, WF, D, DF>(
    first: &mut bool,
    take_owner: impl FnOnce() -> O,
    wait: W,
    take_iwd: impl FnOnce() -> (E, E),
    subscriptions: [&H; 3],
    discover: D,
) -> Opened<O, E, T>
where
    H: Handshake,
    W: FnOnce(O) -> WF,
    WF: Future<Output = O>,
    D: FnOnce() -> DF,
    DF: Future<Output = T>,
{
    let mut owner = take_owner();
    if !std::mem::take(first) {
        owner = wait(owner).await;
    }
    let (added, removed) = take_iwd();
    for subscription in subscriptions {
        subscription.subscribed().await;
    }
    let snapshot = discover().await;
    Opened {
        owner,
        added,
        removed,
        snapshot,
    }
}

/// The parts of a station that a re-read can change: what
/// [`refresh_station`] sets.
fn station_view(station: Option<&Station>) -> Option<(StationState, bool, Option<String>)> {
    station.map(|st| (st.state, st.scanning, st.connected_network.clone()))
}

/// Read the station again, live, once the bus has its subscription's match
/// rule (#1551), and its networks too when the station had changed.
///
/// The station's path comes from the snapshot, so its subscription cannot be
/// made before the snapshot the way [`open_discovery`]'s three are; this read
/// is what closes the window instead. It is sent after the rule is in place,
/// so it supersedes the snapshot's view, and every change iwd makes after
/// answering it reaches the station's receiver. Until #1551's follow-up
/// nothing but the network list was read after the snapshot: a station
/// change in between (a connect finishing, a scan ending) stayed off screen
/// until the station changed again.
///
/// The networks are read again only when the station changed, which is when
/// the change this read stands for would have refreshed them
/// ([`handle_station_props_event`]). The pass has read them once already,
/// before the pump started, and that read takes one round trip per network.
async fn reread_station(
    station_path: &str,
    station_mutable: &Mutable<Option<Station>>,
    networks_mutable: &Mutable<Vec<WifiNetwork>>,
) {
    let before = station_view(station_mutable.lock_ref().as_ref());
    refresh_station(station_path, station_mutable).await;
    if station_view(station_mutable.lock_ref().as_ref()) != before {
        refresh_networks(station_path, station_mutable, networks_mutable).await;
    }
}

/// `paths` is the committed iwd backend's own path cache — passed in rather than
/// reached for through a process-global, so that a backend switch retires this
/// watcher's paths along with the watcher (#633; see [`IwdPaths`]).
///
/// iwd's owner and its `ObjectManager` signals are subscribed **once**, for the
/// watcher's whole life, and before its first snapshot (#1551). They used to be
/// subscribed per discovery, after the snapshot and the network read, so an
/// iwd restart in between was missed until the next owner change, and the
/// restarted iwd had no agent. Now an owner change made while a snapshot is
/// taken is queued for the pump that follows it, and a pump reads it after
/// the snapshot.
///
/// "Before its first snapshot" holds on the bus, not only in this function's
/// order: [`open_discovery`] waits for the bus to have all three match rules
/// (`hytte-bus`'s handshake) before it asks for a snapshot. That wait has no
/// deadline. A subscription to `net.connman.iwd` does not need iwd to be
/// running, so it does not hold up the first discovery, the one that may
/// start iwd; with the system bus itself down the watcher waits here, where
/// discovery would have failed and retried.
///
/// The handshake covers each subscription's **first** rule. A rule that dies
/// and is rebuilt is covered by its `Resubscribed` marker, which is why all
/// three are read with [`SignalSubscription::items`]: a marker on any of
/// them sends the pump back to discovery, keeping the state
/// ([`pump_iwd_events`]). That matters for a reconnect of our own system-bus
/// connection whose death one of the subscription tasks had not yet seen
/// when the next pass asked: that pass's handshake answers for the dead rule,
/// and its snapshot can predate the rebuilt one. The marker that rebuild
/// sends is what says to snapshot again (#1645 review).
///
/// The station's own `PropertiesChanged` follows the same rules on the
/// pass's own subscription: waited for, then the station read again live
/// ([`reread_station`]), and read with its markers, a marker on it sending
/// the pump back to discovery too. The wait is one arm of the pump rather
/// than a step before it (#1647 review), so the network list and the agent
/// registration never wait on the station's rule: a bus that refuses that
/// one `AddMatch` costs the station's live updates, as before #1551, and not
/// the network list and the passphrase prompts with them.
///
/// The subscriptions live for the watcher's life, but each discovery takes
/// **fresh receivers** from them: the owner's at the top of the pass, before
/// [`wait_for_iwd`] reads it, and iwd's two after that wait, right before the
/// snapshot. A broadcast receiver sees only what is sent after it exists, so
/// everything older than the pass is gone without being read, and the
/// snapshot supersedes it. The old receivers are dropped unread. Between
/// discoveries nothing reads them (the pump's post-removal pause, a retry's
/// [`DISCOVERY_RETRY`]), and iwd's per-network and per-BSS
/// `InterfacesAdded`/`InterfacesRemoved` around a radio toggle can overrun one
/// there; a receiver that is never polled again never reports that as a lag
/// (#1573 review). Draining them instead polled the overrun and logged a
/// `warn!` for it.
///
/// Only the watcher's first discovery calls iwd unasked: the backend probe
/// chose iwd, maybe as an activatable name only, and that first call is what
/// starts it, as before. Every later discovery waits for iwd to have an owner
/// first ([`wait_for_iwd`]).
///
/// A pass that finds iwd's name without an owner, or iwd without a station,
/// clears what an earlier pass published before it waits or retries
/// ([`clear_published`], #1654). Until then an iwd or a station that went
/// away while no pump was reading, or inside the hole that ended one, stayed
/// on the panel until iwd came back. A pass that could not read iwd keeps the
/// state, as a hole does, and so does the healthy rediscovery: its snapshot
/// refreshes the state in place.
pub(super) async fn run_wifi_watcher(
    station_mutable: Mutable<Option<Station>>,
    networks_mutable: Mutable<Vec<WifiNetwork>>,
    prompts_mutable: Mutable<Option<PromptRequest>>,
    adapter_mutable: Mutable<Option<Adapter>>,
    paths: IwdPaths,
) {
    let owner_sub = name_owner_changed();
    let added_sub = object_manager_signal("InterfacesAdded");
    let removed_sub = object_manager_signal("InterfacesRemoved");

    let mut first_discovery = true;
    let clear = || {
        clear_published(
            &station_mutable,
            &networks_mutable,
            &prompts_mutable,
            &adapter_mutable,
        );
    };

    'discovery: loop {
        let Opened {
            owner: mut owner_items,
            added: mut added_items,
            removed: mut removed_items,
            snapshot,
        } = open_discovery(
            &mut first_discovery,
            || owner_sub.items(),
            |mut owner| async move {
                wait_for_iwd(&mut owner, clear).await;
                owner
            },
            || (added_sub.items(), removed_sub.items()),
            [&owner_sub, &added_sub, &removed_sub],
            discover_station,
        )
        .await;
        let (managed, station_path) = match snapshot {
            Ok(Some(found)) => found,
            // iwd says it has no station: whatever an earlier pass published
            // is gone, whether the pump saw it go or not (#1654).
            Ok(None) => {
                clear();
                tokio::time::sleep(DISCOVERY_RETRY).await;
                continue 'discovery;
            }
            // iwd did not answer. That says nothing about its station, so
            // the state is kept, as for a hole, for the next pass to refresh
            // in place. An iwd that is gone is cleared by that pass's wait.
            Err(_) => {
                tokio::time::sleep(DISCOVERY_RETRY).await;
                continue 'discovery;
            }
        };
        let station_path_str = station_path.as_str().to_string();

        // The station's own properties, subscribed as soon as the snapshot
        // names the station. Nothing here waits for its rule: the pump does,
        // and reads the station again once it is in place (#1551, see
        // `reread_station`).
        let station_sub = station_props_signal(&station_path_str);
        let mut station_items = station_sub.items();

        paths.set_station_path(&station_path_str).await;
        let adapter_path = adapter_path_from_station(&station_path_str);
        if !adapter_path.is_empty() {
            paths.set_adapter_path(&adapter_path).await;
        }
        tracing::info!(path = station_path_str, "wifi station found");

        refresh_adapter_from_managed(&managed, &station_path_str, &adapter_mutable);
        refresh_station_from_managed(&managed, &station_path, &station_mutable);
        refresh_networks(&station_path_str, &station_mutable, &networks_mutable).await;

        // Returns when the station was removed or iwd changed hands — falls
        // through to re-discover (and re-register the agent) on the next
        // iteration of 'discovery.
        pump_iwd_events(
            IwdStreams {
                owner: &mut owner_items,
                added: &mut added_items,
                removed: &mut removed_items,
                station: &mut station_items,
                station_rule: &station_sub,
            },
            IwdCalls {
                register: || register_iwd_agent(AGENT_PATH),
                reread_station: || {
                    reread_station(&station_path_str, &station_mutable, &networks_mutable)
                },
            },
            &station_path_str,
            &station_mutable,
            &networks_mutable,
            &prompts_mutable,
            &adapter_mutable,
        )
        .await;
        drop(station_sub);
    }
}

// ── Event pump ────────────────────────────────────────────────────────────────

/// Drive the iwd event loop. Returns when the watcher needs to restart
/// discovery: the station was removed, iwd changed hands (see
/// [`owner_reaction`]), or iwd's `InterfacesAdded`/`InterfacesRemoved`
/// receiver or the station's `PropertiesChanged` receiver reported a hole.
///
/// A hole there (`Resubscribed` or `Lagged`) is handled as a hole in the
/// owner's ([`IwdOwner::Hole`]): rediscover, keeping the state. An object
/// change was missed, and the one the pump cannot rebuild from later signals
/// is the station's own removal, so only a fresh snapshot settles it. That
/// also re-registers the agent, which iwd answers with `AlreadyExists` if it
/// still holds it. A hole in the station's receiver alone could be repaired
/// by re-reading the station, but rediscovering does that as well (the next
/// pass makes a fresh station subscription, waits for it and re-reads the
/// station), and keeps one rule for all four receivers. A `Resubscribed`
/// there comes with a reconnect of our own bus connection, which hands the
/// other three receivers a marker of their own anyway.
///
/// Two calls to iwd run as arms of the loop rather than before it, so
/// neither holds up the other or the events (`calls`, injected so a test can
/// drive the pump without a bus):
///
/// - **The agent registration** ([`register_iwd_agent`] in production),
///   retried until iwd takes it ([`register_agent_until_accepted`]). Events
///   keep flowing while it backs off.
/// - **The station re-read** ([`reread_station`] in production), once the
///   bus has the station subscription's match rule (`station_rule`'s
///   handshake). Until the #1647 review that wait came before the pump, so a
///   station rule the bus refused (a per-connection limit, a policy) left
///   iwd with no agent, answering `NoAgent` for every network that needs a
///   passphrase. Now it costs the station's live updates only. The read is
///   awaited inside its arm, so no event is applied while it is in flight
///   and it never overwrites a newer one; an older event applied after it is
///   an absolute value followed by every later change on the same receiver.
///
/// Both stop being polled once iwd is gone, like every other arm that would
/// call iwd.
///
/// The loop is `biased`, owner first, so an iwd exit that is already queued
/// wins over every queued arm that would call iwd: unbiased, a pass that
/// started with a `Lost` queued could send `RegisterAgent` or the station
/// re-read first, and a call to iwd's name with no owner D-Bus-activates the
/// iwd that was just stopped. The other arms cannot starve: each but the two
/// one-shot calls is a bus signal taking one bounded step per item, with a
/// bounded receiver whose overrun is a marker that ends the pass.
async fn pump_iwd_events<O, E, H, R, RF, S, SF>(
    streams: IwdStreams<'_, O, E, H>,
    calls: IwdCalls<R, S>,
    station_path_str: &str,
    station_mutable: &Mutable<Option<Station>>,
    networks_mutable: &Mutable<Vec<WifiNetwork>>,
    prompts_mutable: &Mutable<Option<PromptRequest>>,
    adapter_mutable: &Mutable<Option<Adapter>>,
) where
    O: Stream<Item = SignalItem> + Unpin,
    E: Stream<Item = SignalItem> + Unpin,
    H: Handshake,
    R: Fn() -> RF,
    RF: Future<Output = Result<(), hytte_bus::BusError>>,
    S: Fn() -> SF,
    SF: Future<Output = ()>,
{
    let IwdStreams {
        owner: owner_items,
        added: added_items,
        removed: removed_items,
        station: station_items,
        station_rule,
    } = streams;
    let IwdCalls {
        register,
        reread_station,
    } = calls;

    let mut registering = std::pin::pin!(register_agent_until_accepted(
        register,
        super::nm_registrar::REGISTER_RETRY,
    ));
    let mut registered = false;
    let mut station_installed = std::pin::pin!(station_rule.subscribed());
    let mut station_read = false;

    let clear = || {
        clear_published(
            station_mutable,
            networks_mutable,
            prompts_mutable,
            adapter_mutable,
        );
    };
    // Set once iwd has exited. From then on the other arms are not polled.
    // Anything still queued came from the dead daemon, and acting on it would
    // call `net.connman.iwd`, which D-Bus-activates a stopped iwd.
    let mut iwd_gone = false;

    loop {
        tokio::select! {
            // Owner first: see this function's doc.
            biased;

            Some(item) = owner_items.next() => {
                let Some(change) = iwd_owner_change(&item) else { continue; };
                let reaction = owner_reaction(&change);
                tracing::info!(?change, "iwd changed hands");
                if reaction.clear {
                    clear();
                }
                if reaction.rediscover {
                    return;
                }
                iwd_gone = change == IwdOwner::Lost;
            }
            () = &mut station_installed, if !station_read && !iwd_gone => {
                station_read = true;
                reread_station().await;
            }
            () = &mut registering, if !registered && !iwd_gone => {
                registered = true;
            }
            // A marker on the station's receiver: rediscover, keeping the
            // state, as for the other three (see this function's doc).
            Some(item) = station_items.next(), if !iwd_gone => {
                let SignalItem::Event(evt) = &item else {
                    tracing::info!(?item, "iwd station PropertiesChanged has a hole; rediscovering");
                    return;
                };
                let should_refresh = handle_station_props_event(
                    evt, station_path_str, station_mutable, adapter_mutable,
                ).await;
                if should_refresh {
                    refresh_networks(station_path_str, station_mutable, networks_mutable).await;
                }
            }
            // A marker on either `ObjectManager` receiver: rediscover, keeping
            // the state, as for `IwdOwner::Hole` (see this function's doc).
            // The removal first, so a queued removal of the station ends the
            // pass before a queued add calls the station that left.
            Some(item) = removed_items.next(), if !iwd_gone => {
                let SignalItem::Event(evt) = &item else {
                    tracing::info!(?item, "iwd InterfacesRemoved has a hole; rediscovering");
                    return;
                };
                if station_removed_from_event(&evt.body, station_path_str) {
                    tracing::warn!(path = station_path_str, "iwd station removed — rewatching");
                    clear();
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    return;
                }
                refresh_networks(station_path_str, station_mutable, networks_mutable).await;
            }
            Some(item) = added_items.next(), if !iwd_gone => {
                let SignalItem::Event(_) = &item else {
                    tracing::info!(?item, "iwd InterfacesAdded has a hole; rediscovering");
                    return;
                };
                refresh_networks(station_path_str, station_mutable, networks_mutable).await;
            }
            // Every stream has ended (or iwd is gone and the owner stream has
            // ended): nothing is left to wait on here, so start over.
            else => return,
        }
    }
}

/// Decode `PropertiesChanged` body. Applies the delta directly for known
/// interfaces (Station/Adapter) to avoid a full `GetAll` round-trip, and
/// signals whether the caller should refresh the network list.
async fn handle_station_props_event(
    evt: &hytte_bus::SignalEvent,
    station_path_str: &str,
    station_mutable: &Mutable<Option<Station>>,
    adapter_mutable: &Mutable<Option<Adapter>>,
) -> bool {
    let Ok((iface, changed, invalidated)) =
        evt.body
            .body()
            .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
    else {
        // Can't decode — full refresh to be safe.
        refresh_station(station_path_str, station_mutable).await;
        return true;
    };

    if iface == "net.connman.iwd.Station" {
        // The invalidated list too: a disconnect's `ConnectedNetwork` is in
        // it (#1654). The network list is read after any station delta, so
        // its "Connected" row follows the station either way.
        apply_station_props_delta(&changed, &invalidated, station_mutable);
        true
    } else if iface == IWD_ADAPTER_IFACE {
        apply_adapter_props_delta(&changed, adapter_mutable);
        false
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Adapter, IWD_RECHECK, IwdCalls, IwdOwner, IwdStreams, Mutable, OwnerReaction, Station,
        StationState, handle_station_props_event, iwd_owner_change, next_iwd_owner, open_discovery,
        owner_reaction, pump_iwd_events, register_agent_until_accepted, wait_for_owner,
    };
    use crate::handshake::Handshake;
    use crate::retry;
    use futures_util::StreamExt;
    use hytte_bus::{BusError, SignalEvent, SignalItem};
    use std::cell::{Cell, RefCell};
    use std::collections::{HashMap, VecDeque};
    use std::time::Duration;

    /// A ramp of the retry policy's shape with its own numbers, so tuning the
    /// shipped `REGISTER_RETRY` cannot redden these tests.
    const TEST_RAMP: retry::Policy = retry::Policy {
        max_attempts: None,
        initial: Duration::from_secs(1),
        max_backoff: Duration::from_secs(4),
    };

    fn refused(name: &str) -> BusError {
        BusError::Permanent {
            reason: name.rsplit('.').next().unwrap_or(name).to_owned(),
            dbus_name: Some(name.to_owned()),
        }
    }

    /// A `RegisterAgent` that iwd refuses is tried again on the ramp until
    /// iwd takes it, rather than once (#1551).
    ///
    /// **Falsification:** return after the first attempt whatever its outcome
    /// (the pre-#1551 single `warn!`) and only one attempt is made.
    #[tokio::test(start_paused = true)]
    async fn a_refused_registration_is_retried_until_iwd_takes_it() {
        let answers = RefCell::new(VecDeque::from([
            Err(refused("org.freedesktop.DBus.Error.UnknownObject")),
            Err(refused("org.freedesktop.DBus.Error.NoReply")),
            Ok(()),
        ]));
        let attempts = Cell::new(0_u32);
        let started = tokio::time::Instant::now();

        tokio::time::timeout(
            Duration::from_mins(1),
            register_agent_until_accepted(
                || {
                    attempts.set(attempts.get() + 1);
                    let answer = answers.borrow_mut().pop_front().expect("scripted");
                    async move { answer }
                },
                TEST_RAMP,
            ),
        )
        .await
        .expect("the registration must end once iwd takes it");

        assert_eq!(attempts.get(), 3, "two refusals, then the one iwd took");
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(1 + 2),
            "the retries wait on the ramp"
        );
    }

    /// iwd answering `AlreadyExists` means it still holds this connection's
    /// agent: registered, so not retried.
    ///
    /// **Falsification:** drop the `is_already_registered` arm and the
    /// registration retries an agent iwd already has, forever.
    #[tokio::test(start_paused = true)]
    async fn iwd_already_holding_our_agent_counts_as_registered() {
        let attempts = Cell::new(0_u32);
        tokio::time::timeout(
            Duration::from_mins(1),
            register_agent_until_accepted(
                || {
                    attempts.set(attempts.get() + 1);
                    async { Err(refused("net.connman.iwd.AlreadyExists")) }
                },
                TEST_RAMP,
            ),
        )
        .await
        .expect("AlreadyExists must end the registration");
        assert_eq!(attempts.get(), 1);
    }

    /// Waiting for iwd asks the bus until iwd has an owner, and asks again
    /// after each re-check interval even if no wake comes. It never returns
    /// on a "no" (#1551).
    ///
    /// **Falsification:** return from `wait_for_owner` without asking (no
    /// gate) and no question is asked; return on `Ok(false)` and only one is.
    #[tokio::test(start_paused = true)]
    async fn waiting_for_iwd_asks_the_bus_until_it_has_an_owner() {
        let answers = RefCell::new(VecDeque::from([
            Ok(false),
            Err(refused("org.freedesktop.DBus.Error.Timeout")),
            Ok(true),
        ]));
        let asked = Cell::new(0_u32);
        let started = tokio::time::Instant::now();
        let mut owner = futures_util::stream::pending::<SignalItem>();

        wait_for_owner(
            &mut owner,
            || {
                asked.set(asked.get() + 1);
                let answer = answers.borrow_mut().pop_front().expect("scripted");
                async move { answer }
            },
            IWD_RECHECK,
            || {},
        )
        .await;

        assert_eq!(asked.get(), 3, "a no, a failure, then the yes");
        assert_eq!(
            started.elapsed(),
            IWD_RECHECK * 2,
            "with no wake, each re-ask waits out the re-check interval"
        );
    }

    /// The wait clears what the watcher published once the bus says iwd has
    /// no owner, before it waits, and only then (#1654). An iwd that left
    /// while no pump was reading used to stay on the panel until it came
    /// back. An owner at the first question is the healthy rediscovery, and
    /// a failed question says nothing about iwd: neither clears.
    ///
    /// **Falsification:** never run `gone` and the "no" rows fail; run it on
    /// a failed question, or before the first question, and the rows with
    /// no "no" fail; run it after the wait and the time assertion does; run
    /// it on every "no" and the row with two does.
    #[tokio::test(start_paused = true)]
    async fn waiting_for_iwd_clears_once_iwd_has_no_owner() {
        let failed = || Err(refused("org.freedesktop.DBus.Error.Timeout"));
        // The answers, and the question whose "no" clears, if any.
        let rows = [
            ("an owner at once", vec![Ok(true)], None),
            (
                "a failed question, then an owner",
                vec![failed(), Ok(true)],
                None,
            ),
            ("no owner, then one", vec![Ok(false), Ok(true)], Some(1)),
            (
                "no owner twice, then one",
                vec![Ok(false), Ok(false), Ok(true)],
                Some(1),
            ),
            (
                "a failure, then no owner",
                vec![failed(), Ok(false), Ok(true)],
                Some(2),
            ),
        ];
        for (case, answers, clears_after) in rows {
            let answers = RefCell::new(VecDeque::from(answers));
            let asked = Cell::new(0_u32);
            let cleared = RefCell::new(Vec::new());
            let started = tokio::time::Instant::now();
            let mut owner = futures_util::stream::pending::<SignalItem>();

            wait_for_owner(
                &mut owner,
                || {
                    asked.set(asked.get() + 1);
                    let answer = answers.borrow_mut().pop_front().expect("scripted");
                    async move { answer }
                },
                IWD_RECHECK,
                || cleared.borrow_mut().push((asked.get(), started.elapsed())),
            )
            .await;

            // Right after that question, before the re-check wait after it.
            let expected: Vec<(u32, Duration)> = clears_after
                .map(|question| (question, IWD_RECHECK * (question - 1)))
                .into_iter()
                .collect();
            assert_eq!(cleared.into_inner(), expected, "{case}");
        }
    }

    /// Waiting for iwd wakes on iwd's new owner, and only on that. Someone
    /// else's name coming or going, and iwd going away, do not count.
    ///
    /// **Falsification:** return on any `Event` in `next_iwd_owner` and the
    /// second half fails, because the wait ends on an unrelated name.
    #[tokio::test(start_paused = true)]
    async fn waiting_for_iwd_wakes_on_its_new_owner_only() {
        let noise = || {
            futures_util::stream::iter([
                noc_item("org.bluez", "", ":1.3"),
                noc_item("net.connman.iwd", ":1.4", ""),
                noc_item(":1.5", ":1.5", ""),
            ])
        };

        let mut items = noise()
            .chain(futures_util::stream::iter([noc_item(
                "net.connman.iwd",
                "",
                ":1.9",
            )]))
            .chain(futures_util::stream::pending());
        tokio::time::timeout(IWD_RECHECK, next_iwd_owner(&mut items))
            .await
            .expect("a new iwd owner must end the wait");

        let mut items = noise().chain(futures_util::stream::pending());
        assert!(
            tokio::time::timeout(IWD_RECHECK, next_iwd_owner(&mut items))
                .await
                .is_err(),
            "nothing here announces a new iwd"
        );
    }

    /// A marker ends the wait so the caller asks the bus again: the owner
    /// change it may hide could be the one being waited for. A stream that
    /// ends does not end the wait, so the re-check interval paces it instead
    /// of a busy loop.
    #[tokio::test(start_paused = true)]
    async fn waiting_for_iwd_rechecks_on_a_marker_and_idles_on_a_closed_stream() {
        let mut items = futures_util::stream::iter([SignalItem::Resubscribed])
            .chain(futures_util::stream::pending());
        tokio::time::timeout(IWD_RECHECK, next_iwd_owner(&mut items))
            .await
            .expect("a marker must end the wait");

        let mut items = futures_util::stream::empty::<SignalItem>();
        assert!(
            tokio::time::timeout(IWD_RECHECK, next_iwd_owner(&mut items))
                .await
                .is_err(),
            "a closed stream must leave the pacing to the re-check"
        );
    }

    /// A discovery pass opens in #1551's order: the owner receiver, then the
    /// wait for iwd (every pass but the watcher's first), then iwd's
    /// receivers, then the wait for every subscription's match rule, then
    /// the snapshot (#1573 review).
    ///
    /// **Falsification:** wait on the first pass too, skip the wait on a
    /// later one, take iwd's receivers before the wait, or take the snapshot
    /// before them, and the recorded order changes. So does dropping the
    /// `subscribed().await` loop from `open_discovery`, awaiting only some of
    /// the subscriptions, or moving the loop below the snapshot.
    #[tokio::test]
    async fn a_discovery_pass_opens_in_order_and_waits_except_the_first() {
        /// A subscription whose handshake is not ready until polled twice,
        /// so a wait that is created but not awaited to the end is caught.
        struct Recording<'a>(&'a RefCell<Vec<&'static str>>, &'static str);
        impl Handshake for Recording<'_> {
            fn subscribed(&self) -> impl Future<Output = ()> {
                let Self(log, name) = *self;
                async move {
                    tokio::task::yield_now().await;
                    log.borrow_mut().push(name);
                }
            }
        }

        let log = RefCell::new(Vec::new());
        let subscriptions = [
            &Recording(&log, "owner rule installed"),
            &Recording(&log, "added rule installed"),
            &Recording(&log, "removed rule installed"),
        ];
        let mut first = true;
        for _ in 0..2 {
            let opened = open_discovery(
                &mut first,
                || log.borrow_mut().push("owner receiver"),
                |owner| {
                    log.borrow_mut().push("wait for iwd");
                    async move { owner }
                },
                || {
                    log.borrow_mut().push("iwd receivers");
                    ((), ())
                },
                subscriptions,
                || {
                    log.borrow_mut().push("snapshot");
                    async { Some(()) }
                },
            )
            .await;
            assert_eq!(opened.snapshot, Some(()));
        }
        assert_eq!(
            log.into_inner(),
            [
                "owner receiver",
                "iwd receivers",
                "owner rule installed",
                "added rule installed",
                "removed rule installed",
                "snapshot",
                "owner receiver",
                "wait for iwd",
                "iwd receivers",
                "owner rule installed",
                "added rule installed",
                "removed rule installed",
                "snapshot",
            ]
        );
    }

    /// A station subscription whose match rule the test installs, by
    /// sending `true` on the watch it was made from.
    struct StationRule(tokio::sync::watch::Receiver<bool>);

    impl Handshake for StationRule {
        fn subscribed(&self) -> impl Future<Output = ()> {
            let mut installed = self.0.clone();
            async move {
                let _ = installed.wait_for(|installed| *installed).await;
            }
        }
    }

    /// A station subscription whose rule the bus never installs.
    struct Refused;

    impl Handshake for Refused {
        fn subscribed(&self) -> impl Future<Output = ()> {
            std::future::pending()
        }
    }

    /// A stream the test feeds item by item, which stays open while its
    /// sender lives.
    fn fed(
        rx: tokio::sync::mpsc::UnboundedReceiver<SignalItem>,
    ) -> futures_util::stream::LocalBoxStream<'static, SignalItem> {
        futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        })
        .boxed_local()
    }

    /// The pump registers the agent while the station's rule is not yet
    /// installed, reads the station again once it is, and reads it once
    /// (#1551; #1647 review). Until the review the station's rule was waited
    /// for before the pump started, so a rule the bus refused left iwd with
    /// no agent; before #1551 the station was never read again at all.
    ///
    /// **Falsification:** wait for the station's rule before the
    /// registration and "register" comes after it, or never; re-read the
    /// station without waiting for its rule and the re-read comes first;
    /// re-read it on every turn and it is logged twice.
    #[tokio::test]
    async fn the_station_is_read_again_once_its_rule_is_installed() {
        let log = RefCell::new(Vec::new());
        let (install, installed) = tokio::sync::watch::channel(false);
        let (owner_tx, owner_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut owner = fed(owner_rx);
        let quiet = || futures_util::stream::pending::<SignalItem>().boxed_local();
        let (mut added, mut removed, mut station) = (quiet(), quiet(), quiet());
        let station_rule = StationRule(installed);
        let (station_state, networks, prompts, adapter) = (
            Mutable::new(Some(Station::default())),
            Mutable::new(Vec::new()),
            Mutable::new(None),
            Mutable::new(Some(Adapter::default())),
        );

        let pump = pump_iwd_events(
            IwdStreams {
                owner: &mut owner,
                added: &mut added,
                removed: &mut removed,
                station: &mut station,
                station_rule: &station_rule,
            },
            IwdCalls {
                register: || {
                    log.borrow_mut().push("register");
                    std::future::pending::<Result<(), BusError>>()
                },
                reread_station: || {
                    log.borrow_mut().push("station re-read");
                    std::future::ready(())
                },
            },
            "/net/connman/iwd/0/3",
            &station_state,
            &networks,
            &prompts,
            &adapter,
        );
        let drive = async {
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                *log.borrow(),
                ["register"],
                "the registration goes out with the station's rule pending"
            );
            install.send_replace(true);
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
            assert_eq!(*log.borrow(), ["register", "station re-read"]);
            owner_tx
                .send(SignalItem::Resubscribed)
                .expect("the pump reads the owner");
        };
        tokio::time::timeout(
            Duration::from_secs(30),
            futures_util::future::join(pump, drive),
        )
        .await
        .expect("a hole in the owner's receiver ends the pump");
        assert_eq!(
            log.into_inner(),
            ["register", "station re-read"],
            "the station is read again once"
        );
    }

    /// iwd leaving with its `Lost` already queued when a pass's pump starts:
    /// the `Lost` is taken first, so neither the registration nor the
    /// station re-read calls iwd, whose name then has no owner, and a call to
    /// it would D-Bus-activate the iwd that was just stopped (#1647 review).
    ///
    /// **Falsification:** drop `biased;` from the pump's `select!`, or move
    /// the owner arm down, and some rounds call iwd.
    #[tokio::test(start_paused = true)]
    async fn a_queued_lost_comes_before_every_call_to_iwd() {
        for round in 0..32 {
            let calls = RefCell::new(Vec::new());
            let lost = noc_item("net.connman.iwd", ":1.7", "");
            let mut owner = futures_util::stream::iter([lost])
                .chain(futures_util::stream::pending())
                .boxed_local();
            let quiet = || futures_util::stream::pending::<SignalItem>().boxed_local();
            let (mut added, mut removed, mut station) = (quiet(), quiet(), quiet());
            let (_install, installed) = tokio::sync::watch::channel(true);
            let station_state = Mutable::new(Some(Station::default()));

            let pumped = tokio::time::timeout(
                Duration::from_secs(5),
                pump_iwd_events(
                    IwdStreams {
                        owner: &mut owner,
                        added: &mut added,
                        removed: &mut removed,
                        station: &mut station,
                        station_rule: &StationRule(installed),
                    },
                    IwdCalls {
                        register: || {
                            calls.borrow_mut().push("RegisterAgent");
                            std::future::ready(Ok(()))
                        },
                        reread_station: || {
                            calls.borrow_mut().push("station re-read");
                            std::future::ready(())
                        },
                    },
                    "/net/connman/iwd/0/3",
                    &station_state,
                    &Mutable::new(Vec::new()),
                    &Mutable::new(None),
                    &Mutable::new(Some(Adapter::default())),
                ),
            )
            .await;
            assert!(
                pumped.is_err(),
                "a lost iwd is waited for, not rediscovered"
            );
            assert!(
                calls.borrow().is_empty(),
                "round {round}: iwd was called after it left: {:?}",
                calls.borrow()
            );
            assert!(station_state.get_cloned().is_none(), "the state is cleared");
        }
    }

    /// A hole in iwd's `InterfacesAdded` or `InterfacesRemoved` receiver
    /// sends the pump back to discovery and keeps the state, as a hole in
    /// the owner's does (#1645 review). Those two used to be read with
    /// `events()`, which drops markers, so when a pass's handshake answered
    /// for a rule whose death its task had not seen yet, the rebuild's
    /// marker was filtered out and the snapshot that predated the new rule
    /// was never taken again.
    ///
    /// The station's `PropertiesChanged` receiver is held to the same rule
    /// since #1551's follow-up: it used to be read with `events()` too, so a
    /// station change lost behind a marker waited for the station's next one.
    ///
    /// Every other stream is pending and the registration never answers, so
    /// nothing in the pump reaches a bus.
    ///
    /// **Falsification:** `continue` on a marker in any of the three arms
    /// instead of returning (what `events()` amounted to) and the pump never
    /// ends; clear the state on one and the kept-state assertion fails.
    #[tokio::test(start_paused = true)]
    async fn a_hole_in_iwds_object_signals_rediscovers_and_keeps_the_state() {
        use futures_util::stream::{self, LocalBoxStream};

        for marker in [SignalItem::Resubscribed, SignalItem::Lagged { skipped: 3 }] {
            for on in [
                "InterfacesAdded",
                "InterfacesRemoved",
                "station PropertiesChanged",
            ] {
                let quiet =
                    || -> LocalBoxStream<'static, SignalItem> { stream::pending().boxed_local() };
                let stream_for = |name: &str| {
                    if name == on {
                        stream::iter([marker.clone()])
                            .chain(stream::pending())
                            .boxed_local()
                    } else {
                        quiet()
                    }
                };
                let mut owner = quiet();
                let mut added = stream_for("InterfacesAdded");
                let mut removed = stream_for("InterfacesRemoved");
                let mut station = stream_for("station PropertiesChanged");
                let station_state = Mutable::new(Some(Station::default()));
                let adapter_state = Mutable::new(Some(Adapter::default()));

                tokio::time::timeout(
                    Duration::from_mins(1),
                    pump_iwd_events(
                        IwdStreams {
                            owner: &mut owner,
                            added: &mut added,
                            removed: &mut removed,
                            station: &mut station,
                            station_rule: &Refused,
                        },
                        IwdCalls {
                            register: std::future::pending::<Result<(), BusError>>,
                            reread_station: std::future::pending::<()>,
                        },
                        "/net/connman/iwd/0/3",
                        &station_state,
                        &Mutable::new(Vec::new()),
                        &Mutable::new(None),
                        &adapter_state,
                    ),
                )
                .await
                .unwrap_or_else(|_| {
                    panic!("a {marker:?} on {on} must send the pump back to discovery")
                });
                assert!(
                    station_state.get_cloned().is_some() && adapter_state.get_cloned().is_some(),
                    "a {marker:?} on {on} must keep the state for discovery to refresh in place"
                );
            }
        }
    }

    fn noc_item(name: &str, old: &str, new: &str) -> SignalItem {
        let body = zbus::Message::signal(
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameOwnerChanged",
        )
        .expect("signal builder")
        .build(&(name, old, new))
        .expect("build NameOwnerChanged");
        SignalItem::Event(SignalEvent {
            body,
            sender: None,
            timestamp: std::time::SystemTime::now(),
        })
    }

    /// iwd's station `PropertiesChanged`, `(s, a{sv}, as)`, as its signal
    /// carries it.
    fn station_properties_changed(
        changed: &[(&str, zbus::zvariant::Value<'static>)],
        invalidated: &[&str],
    ) -> SignalEvent {
        let changed: HashMap<&str, &zbus::zvariant::Value<'_>> =
            changed.iter().map(|(k, x)| (*k, x)).collect();
        let body = zbus::Message::signal(
            STATION,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
        )
        .expect("signal builder")
        .build(&("net.connman.iwd.Station", changed, invalidated))
        .expect("build PropertiesChanged");
        // The handler falls back to a live `GetAll` on a body it cannot
        // decode, which would reach the host's system bus: refuse that here.
        assert!(
            body.body()
                .deserialize::<(
                    String,
                    HashMap<String, zbus::zvariant::OwnedValue>,
                    Vec<String>
                )>()
                .is_ok(),
            "the handler must decode this body without a bus"
        );
        SignalEvent {
            body,
            sender: None,
            timestamp: std::time::SystemTime::now(),
        }
    }

    const STATION: &str = "/net/connman/iwd/0/4";
    /// iwd's paths for "home" and "work" on `STATION`, both `psk`.
    const HOME: &str = "/net/connman/iwd/0/4/686f6d65_psk";
    const WORK: &str = "/net/connman/iwd/0/4/776f726b_psk";

    /// The pump's station handler hands the invalidated list on, and keeps
    /// the SSID only on the same network (#1654). This is the call the pump
    /// makes for every station `PropertiesChanged`; the rules themselves are
    /// pinned in `parse`'s tests. A disconnect, as iwd sends it, takes the
    /// network and its SSID; a move to another network takes the SSID until
    /// the network read the handler asks for names the new one.
    ///
    /// **Falsification:** pass the handler's invalidated list on as empty
    /// and the disconnect row keeps `HOME`; keep the SSID across a network
    /// change and the second row names "home"; answer `false` for a station
    /// delta and neither row reads the networks again.
    #[tokio::test]
    async fn the_station_handler_applies_a_disconnect_and_a_network_change() {
        let disconnect = station_properties_changed(
            &[("State", "disconnected".into())],
            &["ConnectedNetwork", "ConnectedAccessPoint"],
        );
        let work = zbus::zvariant::ObjectPath::try_from(WORK).expect("an object path");
        let another_network = station_properties_changed(&[("ConnectedNetwork", work.into())], &[]);
        let rows = [
            ("a disconnect", disconnect, StationState::Disconnected, None),
            (
                "another network",
                another_network,
                StationState::Connected,
                Some(WORK),
            ),
        ];
        for (case, evt, state, network) in rows {
            let published = Mutable::new(Some(Station {
                path: STATION.to_owned(),
                state: StationState::Connected,
                scanning: false,
                connected_network: Some(HOME.to_owned()),
                connected_ssid: Some("home".to_owned()),
            }));
            let adapter = Mutable::new(Some(Adapter::default()));

            let refresh = handle_station_props_event(&evt, STATION, &published, &adapter).await;

            assert!(refresh, "{case}: the network list is read again");
            let st = published.get_cloned().expect("a station");
            assert_eq!(st.state, state, "{case}");
            assert_eq!(st.connected_network.as_deref(), network, "{case}");
            assert_eq!(st.connected_ssid, None, "{case}");
        }
    }

    /// iwd's own name changing hands is read correctly, in both directions,
    /// and a replacement without a visible "lost" half counts as a new owner.
    #[test]
    fn iwd_changing_hands_is_classified() {
        assert_eq!(
            iwd_owner_change(&noc_item("net.connman.iwd", ":1.7", "")),
            Some(IwdOwner::Lost)
        );
        assert_eq!(
            iwd_owner_change(&noc_item("net.connman.iwd", "", ":1.8")),
            Some(IwdOwner::Appeared)
        );
        assert_eq!(
            iwd_owner_change(&noc_item("net.connman.iwd", ":1.7", ":1.8")),
            Some(IwdOwner::Appeared)
        );
    }

    /// Every other name on the system bus is ignored. The match rule has no
    /// `arg0` filter, so this is most of what arrives.
    #[test]
    fn other_names_are_ignored() {
        for name in [
            "org.freedesktop.NetworkManager",
            "net.connman.iwd.agent",
            "org.bluez",
            ":1.7",
        ] {
            assert_eq!(
                iwd_owner_change(&noc_item(name, "", ":1.8")),
                None,
                "{name}"
            );
        }
    }

    /// A hole in the subscription is a "maybe": an iwd restart, or our own
    /// reconnect, may hide behind it.
    #[test]
    fn a_hole_is_a_maybe() {
        assert_eq!(
            iwd_owner_change(&SignalItem::Resubscribed),
            Some(IwdOwner::Hole)
        );
        assert_eq!(
            iwd_owner_change(&SignalItem::Lagged { skipped: 2 }),
            Some(IwdOwner::Hole)
        );
    }

    /// A new iwd, or a hole that may hide one, sends the watcher back to
    /// discovery, which is where `RegisterAgent` is called. Before this the
    /// pump never left on an owner change, so a restarted iwd had no agent.
    /// iwd going away does not: rediscovering is a call to iwd, which would
    /// D-Bus-activate one that was stopped on purpose.
    ///
    /// **Falsification:** make `Appeared` stay in the pump (`rediscover:
    /// false`) and the re-registration assertion fails.
    #[test]
    fn a_new_iwd_is_registered_with_again_and_a_lost_one_is_not_woken() {
        assert_eq!(
            owner_reaction(&IwdOwner::Appeared),
            OwnerReaction {
                clear: true,
                rediscover: true
            },
            "a new iwd must get our agent registered again, and the old one's state dropped"
        );
        assert_eq!(
            owner_reaction(&IwdOwner::Hole),
            OwnerReaction {
                clear: false,
                rediscover: true
            },
            "a hole re-registers, and keeps a state that may still be right"
        );
        assert_eq!(
            owner_reaction(&IwdOwner::Lost),
            OwnerReaction {
                clear: true,
                rediscover: false
            },
            "a lost iwd clears the panel and is not called"
        );
    }

    /// `clear_published` empties all four slots, and wakes none that is
    /// already clear (#1654 review). The pump, the wait for iwd and a pass that
    /// finds no station all go through it, but the real-bus W6 tests read only
    /// the station and the adapter, so dropping the network list from it left
    /// every test green while the panel kept the old network's "Connected"
    /// row with iwd gone. The skip is what keeps the 5 s no-station retry (the
    /// radio off) from waking the panel on every pass.
    ///
    /// **Falsification:** drop any one slot from `clear_published` and the
    /// first half fails; set the four unconditionally (the pump's pre-#1654
    /// closure) and the second half sees a wake.
    #[test]
    fn clear_published_clears_every_slot_and_wakes_none_already_clear() {
        use futures_signals::signal::SignalExt;
        use futures_util::FutureExt;

        let network = super::WifiNetwork {
            path: "/net/connman/iwd/0/4/686f6d65_psk".to_owned(),
            ssid: "home".to_owned(),
            security: "psk".to_owned(),
            known: true,
            connected: true,
            signal_dbm: -50,
            known_network_path: None,
            ap_count: 1,
            active_signal_dbm: Some(-50),
        };
        let prompt = super::PromptRequest {
            id: 1,
            network_path: network.path.clone(),
            ssid: "home".to_owned(),
            security: "psk".to_owned(),
            kind: crate::wifi::PromptKind::WifiPassphrase,
            secret_keys: vec!["passphrase".to_owned()],
            prior_failure: false,
        };
        let station = Mutable::new(Some(Station::default()));
        let networks = Mutable::new(vec![network]);
        let prompts = Mutable::new(Some(prompt));
        let adapter = Mutable::new(Some(Adapter::default()));

        super::clear_published(&station, &networks, &prompts, &adapter);
        assert!(station.lock_ref().is_none(), "the station is kept");
        assert!(networks.lock_ref().is_empty(), "the network list is kept");
        assert!(prompts.lock_ref().is_none(), "the prompt is kept");
        assert!(adapter.lock_ref().is_none(), "the adapter is kept");

        let mut station_wakes = station.signal_cloned().to_stream();
        let mut network_wakes = networks.signal_cloned().to_stream();
        let mut prompt_wakes = prompts.signal_cloned().to_stream();
        let mut adapter_wakes = adapter.signal_cloned().to_stream();
        // A signal yields its current value first.
        assert!(station_wakes.next().now_or_never().is_some());
        assert!(network_wakes.next().now_or_never().is_some());
        assert!(prompt_wakes.next().now_or_never().is_some());
        assert!(adapter_wakes.next().now_or_never().is_some());

        super::clear_published(&station, &networks, &prompts, &adapter);
        assert!(
            station_wakes.next().now_or_never().is_none(),
            "the station woke"
        );
        assert!(
            network_wakes.next().now_or_never().is_none(),
            "the network list woke"
        );
        assert!(
            prompt_wakes.next().now_or_never().is_none(),
            "the prompt woke"
        );
        assert!(
            adapter_wakes.next().now_or_never().is_none(),
            "the adapter woke"
        );
    }
}

// The production `run_wifi_watcher`, through the process-global system
// connection, against a real `dbus-daemon` and a scripted iwd, in a re-exec'd
// child (#1647 review W1, W2). A child module, not a file under `tests/`,
// because `run_wifi_watcher` is private.
#[cfg(all(test, feature = "system-tests"))]
mod real_bus;
