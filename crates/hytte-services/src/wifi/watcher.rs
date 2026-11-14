//! Main iwd watcher task: discovery loop, D-Bus event pump, and signal
//! subscription management.

use futures_signals::signal::Mutable;
use futures_util::{Stream, StreamExt};
use hytte_bus::{BusKind, SignalItem, SignalSubscription};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::time::Duration;
use zbus::zvariant::OwnedValue;

use super::client::{
    discover_station, is_already_registered, is_no_owner, iwd_has_owner, refresh_adapter,
    refresh_networks, refresh_station, register_iwd_agent,
};
use super::parse::{
    Discovered, IWD_ADAPTER_IFACE, IWD_STATION_IFACE, Removed, adds_station_or_adapter,
    apply_adapter_props_delta, apply_station_props_delta, refresh_station_from_managed,
    removed_from_event,
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

/// `PropertiesChanged` on one of iwd's objects: the station, or the adapter.
/// The match is on the exact path, and iwd emits each object's changes at its
/// own: the adapter's at `/net/connman/iwd/<phy>` (`wiphy_get_path`, iwd 3.12
/// `src/wiphy.c:489-498`), the station's at the netdev's
/// `/net/connman/iwd/<phy>/<ifindex>` (`netdev_get_path`, `src/netdev.c:450-460`).
fn properties_signal(path: &str) -> SignalSubscription {
    hytte_bus::signals(BusKind::System, "net.connman.iwd")
        .at_path(path.to_string())
        .iface("org.freedesktop.DBus.Properties")
        .signal("PropertiesChanged")
        .start()
}

/// One published object's `PropertiesChanged`: its path, the receiver, read
/// with its markers, and the subscription, for its handshake: the pump reads
/// the object again once the bus has its match rule.
struct PropsWatch<'a, E, H> {
    /// The object's path, as the snapshot named it.
    path: &'a str,
    /// The receiver.
    items: &'a mut E,
    /// The subscription.
    rule: &'a H,
}

/// The streams [`pump_iwd_events`] reads, all with their markers. The first
/// three are receivers taken for one discovery from subscriptions
/// [`run_wifi_watcher`] holds for its whole life. The station's and the
/// adapter's are made per discovery, since their paths come from the
/// snapshot, and each is `None` when the snapshot had no such object.
struct IwdStreams<'a, O, E, H> {
    /// iwd's owner: [`name_owner_changed`].
    owner: &'a mut O,
    /// `InterfacesAdded` on iwd's root object.
    added: &'a mut O,
    /// `InterfacesRemoved` on iwd's root object.
    removed: &'a mut O,
    /// The station's `PropertiesChanged`.
    station: Option<PropsWatch<'a, E, H>>,
    /// The adapter's `PropertiesChanged` (#1656). Its own subscription, since
    /// iwd emits the adapter's changes at the adapter's path
    /// ([`properties_signal`]): the station's never carried `Powered`.
    adapter: Option<PropsWatch<'a, E, H>>,
}

/// `Some` when all three parts are.
fn props_watch<'a, E, H>(
    path: Option<&'a str>,
    items: Option<&'a mut E>,
    rule: Option<&'a H>,
) -> Option<PropsWatch<'a, E, H>> {
    Some(PropsWatch {
        path: path?,
        items: items?,
        rule: rule?,
    })
}

/// The calls to iwd the pump makes on a schedule of its own, injected so a
/// test can drive the pump without a bus.
struct IwdCalls<R, S, A> {
    /// `RegisterAgent` ([`register_iwd_agent`]), retried until iwd takes it
    /// ([`register_agent_until_accepted`]).
    register: R,
    /// The station read again once its rule is in place ([`reread_station`]).
    reread_station: S,
    /// The adapter read again once its rule is in place, or after one of its
    /// signals did not decode ([`refresh_adapter`]).
    reread_adapter: A,
}

/// How long discovery waits before trying again when iwd did not answer.
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
    /// Clear everything the watcher publishes ([`clear_published`]).
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
async fn wait_for_iwd<S, G>(owner: &mut S, gone: impl FnOnce() -> G)
where
    S: Stream<Item = SignalItem> + Unpin,
    G: Future<Output = ()>,
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
async fn wait_for_owner<S, H, F, G>(
    owner: &mut S,
    has_owner: H,
    recheck: Duration,
    gone: impl FnOnce() -> G,
) where
    S: Stream<Item = SignalItem> + Unpin,
    H: Fn() -> F,
    F: Future<Output = Result<bool, hytte_bus::BusError>>,
    G: Future<Output = ()>,
{
    let mut gone = Some(gone);
    loop {
        match has_owner().await {
            Ok(true) => return,
            Ok(false) => {
                if let Some(gone) = gone.take() {
                    tracing::info!("wifi: iwd is not running; waiting for it to return");
                    gone().await;
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
/// The bus's own no-owner answer ([`is_no_owner`]) ends the registration
/// without a retry or a `warn!` (#1657 review). `RegisterAgent` is sent with
/// `no_auto_start()` ([`register_iwd_agent`]), so that answer means iwd has
/// left: its `Lost` reaches the pump, which clears and waits, and the pass
/// that the next iwd's `Appeared` starts registers with that iwd. Retrying
/// would only ask an empty name again until the pump read the `Lost`.
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
            Err(e) if is_no_owner(&e) => {
                tracing::debug!(
                    attempt,
                    "iwd left before taking the agent; the next iwd's pass registers"
                );
                return;
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

/// Clear everything the watcher publishes about iwd: the station's side
/// ([`clear_station_side`]) and the adapter.
///
/// The pump does this when it sees iwd leave, a new iwd take its name or the
/// adapter's object removed. Since #1654 a discovery pass does it too when it
/// finds iwd's name without an owner ([`wait_for_owner`]): that can happen
/// while no pump is running, after one returned on a hole, and nothing else
/// would clear what the last pass published.
///
/// A station that goes while the adapter stays clears only the station's side
/// (#1656): that is what powering the adapter off looks like, and the adapter
/// is what the panel's power switch turns it back on with.
///
/// A value that is already clear is left alone, so clearing a watcher that
/// has nothing published wakes no part of the panel.
///
/// The commands' paths go too, each before the panel's value it names
/// (#1659), so no command addresses what the panel no longer shows.
async fn clear_published(
    station: &Mutable<Option<Station>>,
    networks: &Mutable<Vec<WifiNetwork>>,
    prompts: &Mutable<Option<PromptRequest>>,
    adapter: &Mutable<Option<Adapter>>,
    paths: &IwdPaths,
) {
    clear_station_side(station, networks, prompts, paths).await;
    paths.set_adapter_path(None).await;
    if adapter.lock_ref().is_some() {
        adapter.set(None);
    }
}

/// Clear what the watcher publishes about iwd's station: the station, its
/// networks and a pending prompt, and the station's path the commands
/// address, first (#1659). The adapter and its path stay (#1656).
///
/// The pump does this when iwd removes the station's interface and not the
/// adapter's object, and a discovery pass does it when its snapshot has no
/// station: the adapter is powered off (or iwd has no netdev up), and the
/// pass publishes the snapshot's adapter instead. Until #1656 both cleared
/// the adapter too, so the panel read "No adapter" and its power switch went
/// insensitive with the radio off.
///
/// A value that is already clear is left alone, as in [`clear_published`]:
/// a pass that finds no station again, after a hole say, wakes nothing.
async fn clear_station_side(
    station: &Mutable<Option<Station>>,
    networks: &Mutable<Vec<WifiNetwork>>,
    prompts: &Mutable<Option<PromptRequest>>,
    paths: &IwdPaths,
) {
    paths.set_station_path(None).await;
    if station.lock_ref().is_some() {
        station.set(None);
    }
    if !networks.lock_ref().is_empty() {
        networks.set(Vec::new());
    }
    if prompts.lock_ref().is_some() {
        prompts.set(None);
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
///
/// A pass with no station has nothing to read, and its pump never asks.
async fn reread_station(
    station_path: Option<&str>,
    station_mutable: &Mutable<Option<Station>>,
    networks_mutable: &Mutable<Vec<WifiNetwork>>,
) {
    let Some(station_path) = station_path else {
        return;
    };
    let before = station_view(station_mutable.lock_ref().as_ref());
    refresh_station(station_path, station_mutable).await;
    if station_view(station_mutable.lock_ref().as_ref()) != before {
        refresh_networks(station_path, station_mutable, networks_mutable).await;
    }
}

/// Read the adapter again, live ([`refresh_adapter`]), once the bus has its
/// subscription's match rule (#1656): [`reread_station`]'s discipline. A
/// change to `Powered` between the snapshot and the rule would otherwise stay
/// off screen until the next one. A pass with no adapter has nothing to read,
/// and its pump never asks.
async fn reread_adapter(adapter_path: Option<&str>, adapter_mutable: &Mutable<Option<Adapter>>) {
    if let Some(adapter_path) = adapter_path {
        refresh_adapter(adapter_path, adapter_mutable).await;
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
/// The adapter's own `PropertiesChanged` is subscribed the same way, at the
/// adapter's path (#1656): waited for in the pump, then the adapter read
/// again ([`reread_adapter`]), and read with its markers. iwd emits the
/// adapter's `Powered` there and nowhere else, so until #1656 the panel never
/// saw the radio switch on or off while it watched a station.
///
/// The subscriptions live for the watcher's life, but each discovery takes
/// **fresh receivers** from them: the owner's at the top of the pass, before
/// [`wait_for_iwd`] reads it, and iwd's two after that wait, right before the
/// snapshot. A broadcast receiver sees only what is sent after it exists, so
/// everything older than the pass is gone without being read, and the
/// snapshot supersedes it. The old receivers are dropped unread. Between
/// discoveries nothing reads them (the pump's [`REMOVAL_PAUSE`] after the
/// adapter's removal and after the station's, a retry's [`DISCOVERY_RETRY`]),
/// and iwd's per-network and per-BSS `InterfacesAdded`/`InterfacesRemoved`
/// around a radio toggle can overrun one there; a receiver that is never
/// polled again never reports that as a lag (#1573 review). Draining them
/// instead polled the overrun and logged a `warn!` for it.
///
/// Only the watcher's first discovery calls iwd unasked: the backend probe
/// chose iwd, maybe as an activatable name only, and that first call is what
/// starts it, as before. Every later discovery waits for iwd to have an owner
/// first ([`wait_for_iwd`]).
///
/// A pass that finds iwd's name without an owner clears what an earlier pass
/// published before it waits ([`clear_published`], #1654). A pass whose
/// snapshot has no station clears the station's side
/// ([`clear_station_side`]) and publishes the snapshot's adapter, or clears
/// it when the snapshot has none (#1656). With more than one adapter that is
/// the one already published, while the snapshot still has it
/// ([`super::parse::adapter_in`], #1657 review), so the radio the panel just
/// switched off is the one its switch turns back on. With more than one
/// station a pass keeps the published one the same way
/// ([`super::parse::station_in`], #1659). Until #1654 an iwd or a
/// station that went away while no pump was reading, or inside the hole that
/// ended one, stayed on the panel until iwd came back; until #1656 a pass
/// with no station cleared the adapter too, so a powered-off radio could not
/// be switched back on from the panel. A pass that could not read iwd keeps
/// the state, as a hole does, and so does the healthy rediscovery: its
/// snapshot refreshes the state in place.
///
/// A pass with no station does not retry on a timer: its pump watches the
/// adapter and waits for iwd to add a station or an adapter
/// ([`pump_iwd_events`]). Until #1656 it slept [`DISCOVERY_RETRY`] and took
/// another snapshot, for as long as the radio was off.
///
/// `paths` gets the adapter's path from the adapter the pass publishes, with
/// a station or without one, so the power switch ([`super::set_powered`])
/// can reach a radio that was off when the shell started (#1656). It used to
/// be derived from the station's path, and a pass with no station set none.
/// Every clear of the watcher's clears the paths it covers, and a pass with
/// no adapter clears the adapter's (#1659).
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
            &paths,
        )
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
            // Read when the snapshot is asked for, after the wait's clear.
            || discover_station(&station_mutable, &adapter_mutable),
        )
        .await;
        // iwd did not answer. That says nothing about its station, so the
        // state is kept, as for a hole, for the next pass to refresh in
        // place. An iwd that is gone is cleared by that pass's wait.
        let Ok(Discovered {
            managed,
            station,
            adapter,
        }) = snapshot
        else {
            tokio::time::sleep(DISCOVERY_RETRY).await;
            continue 'discovery;
        };
        let station_path = station.as_ref().map(|p| p.as_str().to_owned());
        let adapter_path = adapter.as_ref().map(|a| a.path.clone());

        // Each object's own properties, subscribed as soon as the snapshot
        // names it. Nothing here waits for the rules: the pump does, and
        // reads each object again once its rule is in place (#1551, see
        // `reread_station`; #1656, `reread_adapter`).
        let station_sub = station_path.as_deref().map(properties_signal);
        let adapter_sub = adapter_path.as_deref().map(properties_signal);
        let mut station_items = station_sub.as_ref().map(SignalSubscription::items);
        let mut adapter_items = adapter_sub.as_ref().map(SignalSubscription::items);

        // The power switch addresses the adapter this pass publishes, with a
        // station or without one (#1656), and none when it publishes none
        // (#1659).
        paths.set_adapter_path(adapter_path.as_deref()).await;
        adapter_mutable.set_neq(adapter);
        if let Some(station) = &station {
            let path = station.as_str();
            paths.set_station_path(Some(path)).await;
            tracing::info!(path, "wifi station found");
            refresh_station_from_managed(&managed, station, &station_mutable);
            refresh_networks(path, &station_mutable, &networks_mutable).await;
        } else {
            // iwd answered and has no station: the adapter is powered off, or
            // iwd has no netdev up. Whatever station an earlier pass published
            // is gone, whether the pump saw it go or not (#1654); the adapter
            // is the snapshot's, published above (#1656).
            tracing::info!(adapter = ?adapter_path, "wifi: iwd has no station");
            clear_station_side(
                &station_mutable,
                &networks_mutable,
                &prompts_mutable,
                &paths,
            )
            .await;
        }

        // Returns when iwd changed hands, the station or the adapter was
        // removed, a pass with no station saw one (or an adapter) added, or a
        // receiver had a hole — falls through to re-discover (and re-register
        // the agent) on the next iteration of 'discovery.
        pump_iwd_events(
            IwdStreams {
                owner: &mut owner_items,
                added: &mut added_items,
                removed: &mut removed_items,
                station: props_watch(
                    station_path.as_deref(),
                    station_items.as_mut(),
                    station_sub.as_ref(),
                ),
                adapter: props_watch(
                    adapter_path.as_deref(),
                    adapter_items.as_mut(),
                    adapter_sub.as_ref(),
                ),
            },
            IwdCalls {
                register: || register_iwd_agent(AGENT_PATH),
                reread_station: || {
                    reread_station(station_path.as_deref(), &station_mutable, &networks_mutable)
                },
                reread_adapter: || reread_adapter(adapter_path.as_deref(), &adapter_mutable),
            },
            &station_mutable,
            &networks_mutable,
            &prompts_mutable,
            &adapter_mutable,
            &paths,
        )
        .await;
        drop(station_sub);
        drop(adapter_sub);
    }
}

// ── Event pump ────────────────────────────────────────────────────────────────

/// How long the pump waits after the station's interface or the adapter's
/// object is removed before it rediscovers: what it always waited after the
/// station's removal. It keeps the next pass's call to iwd from following the
/// removal at once: an iwd that is stopping can remove its objects before it
/// releases its name (#1551 saw it with the station), and [`wait_for_iwd`]
/// asks the bus whether the name still has an owner before that call.
const REMOVAL_PAUSE: Duration = Duration::from_secs(2);

/// `rule`'s handshake, or never when there is no rule.
async fn installed<H: Handshake>(rule: Option<&H>) {
    match rule {
        Some(rule) => rule.subscribed().await,
        None => std::future::pending().await,
    }
}

/// A watch's path, receiver and subscription, each `None` without a watch.
type WatchParts<'a, E, H> = (Option<&'a str>, Option<&'a mut E>, Option<&'a H>);

/// Take a watch apart, for the pump's arms to borrow its path, its receiver
/// and its subscription's handshake separately.
fn watch_parts<E, H>(watch: Option<PropsWatch<'_, E, H>>) -> WatchParts<'_, E, H> {
    match watch {
        Some(PropsWatch { path, items, rule }) => (Some(path), Some(items), Some(rule)),
        None => (None, None, None),
    }
}

/// The event a receiver yielded, or `None` for a marker, which is logged as
/// a hole in `what` and sends the pump back to discovery.
fn event_or_hole<'i>(item: &'i SignalItem, what: &str) -> Option<&'i hytte_bus::SignalEvent> {
    match item {
        SignalItem::Event(evt) => Some(evt),
        SignalItem::Resubscribed | SignalItem::Lagged { .. } => {
            tracing::info!(?item, "iwd {what} has a hole; rediscovering");
            None
        }
    }
}

/// The next item on `items`, or never when there are none.
async fn next_item<E>(items: &mut Option<&mut E>) -> Option<SignalItem>
where
    E: Stream<Item = SignalItem> + Unpin,
{
    match items {
        Some(items) => items.next().await,
        None => std::future::pending().await,
    }
}

/// Drive the iwd event loop. Returns when the watcher needs to restart
/// discovery: iwd changed hands (see [`owner_reaction`]), the published
/// station's interface or adapter's object was removed, a pass with no
/// station saw iwd add a station or an adapter, or any receiver reported a
/// hole.
///
/// **A station that goes keeps the adapter** (#1656). When iwd removes the
/// station's interface and not the adapter's object, which is what powering
/// the adapter off does, the pump clears the station's side
/// ([`clear_station_side`]) and leaves the adapter published, so the panel's
/// power switch stays live. Then it pauses ([`REMOVAL_PAUSE`]) and
/// rediscovers, as it always did there: the next snapshot says whether the
/// station is back, and with no station that pass publishes the snapshot's
/// adapter, `Powered` included. Until #1656 the removal cleared the adapter
/// too, and a pass with no station published none, so the power switch went
/// insensitive once the radio was off.
///
/// The removal ends the pass rather than leave it running without the
/// station (#1657 review). `InterfacesRemoved` and `InterfacesAdded` reach the
/// pump on two receivers, each fed by its own subscription task, so nothing
/// orders them: a station that flaps (a netdev down and straight back up) can
/// be read add first, and a pass that went on without the station would wait
/// for an add it had already read, with iwd connected and the panel empty.
/// The pump that did that parked the panel 13 times in 1000 flaps emitted in
/// iwd's own order. A snapshot cannot be overtaken that way.
///
/// A pass that **starts** with no station watches the adapter, its `Powered`
/// live both ways, and waits for an `InterfacesAdded` that brings a station
/// or an adapter ([`adds_station_or_adapter`]), then rediscovers: that is how
/// a radio switched back on gets its station published. Nothing in that state
/// calls iwd, and no timer wakes it. Until #1656 such a pass retried on a
/// timer.
///
/// The adapter's own removal clears everything, pauses ([`REMOVAL_PAUSE`])
/// and rediscovers: iwd removes the adapter only with the wiphy, so the
/// station is gone too, and a snapshot says whether another adapter is
/// there.
///
/// A hole (`Resubscribed` or `Lagged`) is handled as a hole in the owner's
/// ([`IwdOwner::Hole`]): rediscover, keeping the state. An object change was
/// missed, and the ones the pump cannot rebuild from later signals are an
/// object's own removal and addition, so only a fresh snapshot settles them.
/// That also re-registers the agent, which iwd answers with `AlreadyExists`
/// if it still holds it. A hole in the station's or the adapter's receiver
/// alone could be repaired by re-reading that object, but rediscovering does
/// that as well (the next pass makes fresh subscriptions, waits for them and
/// re-reads both), and keeps one rule for every receiver. A `Resubscribed`
/// there comes with a reconnect of our own bus connection, which hands the
/// other receivers a marker of their own anyway.
///
/// Three calls to iwd run beside the events rather than before them, so none
/// holds up another or the events (`calls`, injected so a test can drive the
/// pump without a bus):
///
/// - **The agent registration** ([`register_iwd_agent`] in production),
///   retried until iwd takes it ([`register_agent_until_accepted`]). Events
///   keep flowing while it backs off. It is polled beside the event loop, not
///   as one of its arms ([`beside_registration`]), so it keeps moving while a
///   handler awaits a call of its own. A pass with no station does not
///   register: iwd has nothing to ask a passphrase for, and the pass that
///   finds the station registers.
/// - **The station re-read** ([`reread_station`] in production), once the
///   bus has the station subscription's match rule (its handshake). Until
///   the #1647 review that wait came before the pump, so a station rule the
///   bus refused (a per-connection limit, a policy) left iwd with no agent,
///   answering `NoAgent` for every network that needs a passphrase. Now it
///   costs the station's live updates only.
/// - **The adapter re-read** ([`reread_adapter`] in production), the same
///   way on the adapter's rule (#1656), and again after an adapter signal
///   that does not decode.
///
/// Each re-read is awaited inside its arm, so no event is applied while it is
/// in flight and it never overwrites a newer one; an older event applied
/// after it is an absolute value followed by every later change on the same
/// receiver. Every call stops once the pump has read iwd's `Lost`: the
/// re-reads' arms are no longer polled, and the registration is dropped.
///
/// The loop is `biased`, owner first, and [`beside_registration`] polls it
/// before the registration, so an iwd exit that is already queued when the
/// loop is polled wins over every call to iwd: otherwise a pass that started
/// with a `Lost` queued could send `RegisterAgent` or a re-read first, and a
/// call to iwd's name with no owner D-Bus-activates the iwd that was just
/// stopped. A `Lost` that arrives while a handler awaits is not read until
/// the handler returns, and the registration keeps running meanwhile (that is
/// the point of running it beside the loop), so a retry can go out behind it
/// (#1657 review). That is why `RegisterAgent` is sent with `no_auto_start()`
/// ([`register_iwd_agent`]), and why the bus's no-owner answer ends the
/// registration instead of being retried ([`register_agent_until_accepted`]).
/// The other arms cannot starve: each but the one-shot re-reads is a bus
/// signal taking one bounded step per item, with a bounded receiver whose
/// overrun is a marker that ends the pass.
async fn pump_iwd_events<O, E, H, R, RF, S, SF, A, AF>(
    streams: IwdStreams<'_, O, E, H>,
    calls: IwdCalls<R, S, A>,
    station_mutable: &Mutable<Option<Station>>,
    networks_mutable: &Mutable<Vec<WifiNetwork>>,
    prompts_mutable: &Mutable<Option<PromptRequest>>,
    adapter_mutable: &Mutable<Option<Adapter>>,
    paths: &IwdPaths,
) where
    O: Stream<Item = SignalItem> + Unpin,
    E: Stream<Item = SignalItem> + Unpin,
    H: Handshake,
    R: Fn() -> RF,
    RF: Future<Output = Result<(), hytte_bus::BusError>>,
    S: Fn() -> SF,
    SF: Future<Output = ()>,
    A: Fn() -> AF,
    AF: Future<Output = ()>,
{
    // Set once iwd has exited. From then on nothing that would call iwd is
    // polled. Anything still queued came from the dead daemon, and acting on
    // it would call `net.connman.iwd`, which D-Bus-activates a stopped iwd.
    // An atomic only because the watcher's future must be `Send`: one task
    // reads and writes it.
    let iwd_gone = AtomicBool::new(false);
    // Nothing to register for without a station: see this function's doc.
    let registration = streams.station.is_some().then(|| {
        register_agent_until_accepted(calls.register, super::nm_registrar::REGISTER_RETRY)
    });
    let published = Published {
        station: station_mutable,
        networks: networks_mutable,
        prompts: prompts_mutable,
        adapter: adapter_mutable,
        paths,
    };
    let events = pump_events(
        streams,
        (calls.reread_station, calls.reread_adapter),
        published,
        &iwd_gone,
    );
    beside_registration(events, registration, &iwd_gone).await;
}

/// What the watcher publishes, and the paths the commands address it by,
/// borrowed for one pump.
#[derive(Clone, Copy)]
struct Published<'m> {
    station: &'m Mutable<Option<Station>>,
    networks: &'m Mutable<Vec<WifiNetwork>>,
    prompts: &'m Mutable<Option<PromptRequest>>,
    adapter: &'m Mutable<Option<Adapter>>,
    paths: &'m IwdPaths,
}

impl Published<'_> {
    /// [`clear_published`].
    async fn clear(self) {
        clear_published(
            self.station,
            self.networks,
            self.prompts,
            self.adapter,
            self.paths,
        )
        .await;
    }
}

/// Run the pump's event loop with the agent registration polled beside it
/// (#1656), until the loop returns. The registration is dropped once the pump
/// has read iwd's `Lost` or the registration is done, never left pending
/// unpolled.
///
/// It used to be one of the loop's arms, so it was not polled while another
/// arm's handler awaited a call of its own (a re-read, a network read), and a
/// call in flight holds its connection's resources between polls. The locks
/// on the way to the socket are `tokio::sync::Mutex`es (`hytte-bus`'s
/// `with_conn`, and zbus 5.14's write half under its `tokio` feature,
/// `abstractions/async_lock.rs`), which are fair: they hand the lock to the
/// first queued waiter whether or not that waiter is polled. A
/// `RegisterAgent` queued behind another caller could be handed a lock while
/// its arm sat unpolled, and the handler's own call then waited behind it:
/// #1656's W1 failed 3 of 40 runs that way, with neither call ever written,
/// once the adapter's subscription put more traffic on the connection at the
/// pump's start. Those runs ended at the re-read's 25 s deadline only because
/// they stalled on zbus's write lock, inside the call. `hytte_bus::call`'s
/// deadline covers the call's send and reply (`proxy.call`) and nothing
/// before it: `with_conn`'s lock and the proxy's construction have none
/// (#1657 review). A pending call's receiver on the connection's
/// method-return queue (8 deep, zbus `connection/mod.rs`) is the same hazard
/// for every reply on the connection, which is why a registration that iwd's
/// exit leaves pending is dropped rather than kept.
async fn beside_registration(
    events: impl Future<Output = ()>,
    registration: Option<impl Future<Output = ()>>,
    iwd_gone: &AtomicBool,
) {
    let mut events = std::pin::pin!(events);
    let mut registration = std::pin::pin!(registration);
    std::future::poll_fn(|cx| {
        // The events first, so a queued `Lost` is read before the
        // registration is polled (see `pump_iwd_events`' doc).
        if events.as_mut().poll(cx).is_ready() {
            return Poll::Ready(());
        }
        if iwd_gone.load(Ordering::Relaxed) {
            registration.set(None);
        }
        let done = registration
            .as_mut()
            .as_pin_mut()
            .is_some_and(|registration| registration.poll(cx).is_ready());
        if done {
            registration.set(None);
        }
        Poll::Pending
    })
    .await;
}

/// [`pump_iwd_events`]' event loop: every arm but the registration, which
/// runs beside it ([`beside_registration`]). Returns when the pump does; see
/// that function's doc for why each arm does what it does.
async fn pump_events<O, E, H, S, SF, A, AF>(
    streams: IwdStreams<'_, O, E, H>,
    (reread_station, reread_adapter): (S, A),
    published: Published<'_>,
    iwd_gone: &AtomicBool,
) where
    O: Stream<Item = SignalItem> + Unpin,
    E: Stream<Item = SignalItem> + Unpin,
    H: Handshake,
    S: Fn() -> SF,
    SF: Future<Output = ()>,
    A: Fn() -> AF,
    AF: Future<Output = ()>,
{
    let Published {
        station: station_mutable,
        networks: networks_mutable,
        prompts: prompts_mutable,
        adapter: adapter_mutable,
        paths,
    } = published;
    // What the pass publishes, by path, and the receivers for it. Either
    // object's removal ends the pass.
    let (station_path, mut station_items, station_rule) = watch_parts(streams.station);
    let (adapter_path, mut adapter_items, adapter_rule) = watch_parts(streams.adapter);
    let mut station_installed = std::pin::pin!(installed(station_rule));
    let mut station_read = false;
    let mut adapter_installed = std::pin::pin!(installed(adapter_rule));
    let mut adapter_read = false;

    loop {
        let gone = iwd_gone.load(Ordering::Relaxed);
        tokio::select! {
            // Owner first: see `pump_iwd_events`' doc.
            biased;

            Some(item) = streams.owner.next() => {
                let Some(change) = iwd_owner_change(&item) else { continue; };
                let reaction = owner_reaction(&change);
                tracing::info!(?change, "iwd changed hands");
                // Before the clear, which can wait for the paths' lock: the
                // registration beside this loop is polled meanwhile (#1661).
                iwd_gone.store(change == IwdOwner::Lost, Ordering::Relaxed);
                if reaction.clear {
                    published.clear().await;
                }
                if reaction.rediscover {
                    return;
                }
            }
            () = &mut station_installed, if station_path.is_some() && !station_read && !gone => {
                station_read = true;
                reread_station().await;
            }
            () = &mut adapter_installed, if adapter_path.is_some() && !adapter_read && !gone => {
                adapter_read = true;
                reread_adapter().await;
            }
            // A marker on any receiver below: rediscover, keeping the state,
            // as for `IwdOwner::Hole` (see `pump_iwd_events`' doc).
            Some(item) = next_item(&mut station_items), if !gone => {
                let Some(evt) = event_or_hole(&item, "station PropertiesChanged") else { return };
                let Some(path) = station_path else { continue; };
                handle_station_props_event(evt, path, station_mutable).await;
                refresh_networks(path, station_mutable, networks_mutable).await;
            }
            // The adapter's own (#1656).
            Some(item) = next_item(&mut adapter_items), if !gone => {
                let Some(evt) = event_or_hole(&item, "adapter PropertiesChanged") else { return };
                if !apply_adapter_event(evt, adapter_mutable) {
                    reread_adapter().await;
                }
            }
            // The removal first, so a queued removal of the station stops the
            // pass reading it before a queued add would read the station that
            // left.
            Some(item) = streams.removed.next(), if !gone => {
                let Some(evt) = event_or_hole(&item, "InterfacesRemoved") else { return };
                match removed_from_event(&evt.body, station_path, adapter_path) {
                    Removed::Adapter => {
                        tracing::warn!(path = adapter_path, "iwd adapter removed — rewatching");
                        published.clear().await;
                        tokio::time::sleep(REMOVAL_PAUSE).await;
                        return;
                    }
                    Removed::Station => {
                        tracing::info!(
                            path = station_path,
                            "iwd station removed; keeping the adapter and rewatching"
                        );
                        clear_station_side(station_mutable, networks_mutable, prompts_mutable, paths)
                            .await;
                        tokio::time::sleep(REMOVAL_PAUSE).await;
                        return;
                    }
                    Removed::Other => if let Some(path) = station_path {
                        refresh_networks(path, station_mutable, networks_mutable).await;
                    },
                }
            }
            Some(item) = streams.added.next(), if !gone => {
                let Some(evt) = event_or_hole(&item, "InterfacesAdded") else { return };
                match station_path {
                    Some(path) => refresh_networks(path, station_mutable, networks_mutable).await,
                    None if adds_station_or_adapter(&evt.body) => {
                        tracing::info!("iwd added a station or an adapter; rediscovering");
                        return;
                    }
                    None => {}
                }
            }
            // Every stream has ended (or iwd is gone and the owner stream has
            // ended): nothing is left to wait on here, so start over.
            else => return,
        }
    }
}

/// The `(interface, changed, invalidated)` body of a `PropertiesChanged`
/// signal, or `None` if it does not decode as one.
fn decode_properties_changed(
    msg: &zbus::Message,
) -> Option<(String, HashMap<String, OwnedValue>, Vec<String>)> {
    msg.body().deserialize().ok()
}

/// Decode a `PropertiesChanged` on the station's path. Applies a
/// `net.connman.iwd.Station` delta directly to avoid a full `GetAll`
/// round-trip. The caller reads the network list after every one, whatever
/// its interface.
///
/// The adapter's changes never arrive here: iwd emits them at the adapter's
/// path, which has a subscription of its own ([`apply_adapter_event`],
/// #1656). This handler used to have an arm for them that could not fire,
/// and the one case where it told the caller not to read the networks.
async fn handle_station_props_event(
    evt: &hytte_bus::SignalEvent,
    station_path_str: &str,
    station_mutable: &Mutable<Option<Station>>,
) {
    let Some((iface, changed, invalidated)) = decode_properties_changed(&evt.body) else {
        // Can't decode — full refresh to be safe.
        refresh_station(station_path_str, station_mutable).await;
        return;
    };

    if iface == IWD_STATION_IFACE {
        // The invalidated list too: a disconnect's `ConnectedNetwork` is in
        // it (#1654). The network list is read after any station delta, so
        // its "Connected" row follows the station either way.
        apply_station_props_delta(&changed, &invalidated, station_mutable);
    }
}

/// Apply a `PropertiesChanged` on the adapter's path (#1656): a
/// `net.connman.iwd.Adapter` delta, `Powered` included, which is how the
/// panel sees the radio switch off and back on (iwd 3.12 `wiphy_rfkill_cb`,
/// `src/wiphy.c:2492-2520`). A change to another interface there is
/// ignored. `false` when the body does not decode, and the caller reads the
/// adapter again.
fn apply_adapter_event(
    evt: &hytte_bus::SignalEvent,
    adapter_mutable: &Mutable<Option<Adapter>>,
) -> bool {
    let Some((iface, changed, _invalidated)) = decode_properties_changed(&evt.body) else {
        return false;
    };
    if iface == IWD_ADAPTER_IFACE {
        apply_adapter_props_delta(&changed, adapter_mutable);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{
        Adapter, IWD_RECHECK, IwdCalls, IwdOwner, IwdPaths, IwdStreams, Mutable, OwnerReaction,
        PropsWatch, REMOVAL_PAUSE, Station, StationState, apply_adapter_event,
        decode_properties_changed, handle_station_props_event, iwd_owner_change, next_iwd_owner,
        open_discovery, owner_reaction, pump_iwd_events, register_agent_until_accepted,
        wait_for_owner,
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

    /// The bus answering that nothing owns iwd's name ends the registration
    /// at once, even after a refusal opened the retry ramp (#1657 review):
    /// `RegisterAgent` is sent `no_auto_start()`, so iwd has left, and the
    /// next iwd's pass registers. No retry knocks on the empty name while the
    /// pump has yet to read the `Lost`.
    ///
    /// **Falsification:** drop the `is_no_owner` arm and the registration
    /// retries the empty name on the ramp until the timeout.
    #[tokio::test(start_paused = true)]
    async fn iwd_gone_from_the_bus_ends_the_registration() {
        let answers = RefCell::new(VecDeque::from([
            Err(refused("org.freedesktop.DBus.Error.NoReply")),
            Err(refused("org.freedesktop.DBus.Error.NameHasNoOwner")),
        ]));
        let attempts = Cell::new(0_u32);
        let started = tokio::time::Instant::now();
        tokio::time::timeout(
            Duration::from_mins(1),
            register_agent_until_accepted(
                || {
                    attempts.set(attempts.get() + 1);
                    let answer = answers.borrow_mut().pop_front().unwrap_or_else(|| {
                        Err(refused("org.freedesktop.DBus.Error.NameHasNoOwner"))
                    });
                    async move { answer }
                },
                TEST_RAMP,
            ),
        )
        .await
        .expect("a name with no owner must end the registration");
        assert_eq!(attempts.get(), 2, "one refusal, then the bus's no-owner");
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(1),
            "one step of the ramp"
        );
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
            || async {},
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
                || {
                    cleared.borrow_mut().push((asked.get(), started.elapsed()));
                    async {}
                },
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

        let paths = IwdPaths::default();
        let pump = pump_iwd_events(
            IwdStreams {
                owner: &mut owner,
                added: &mut added,
                removed: &mut removed,
                station: Some(PropsWatch {
                    path: "/net/connman/iwd/0/3",
                    items: &mut station,
                    rule: &station_rule,
                }),
                adapter: None,
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
                reread_adapter: || {
                    log.borrow_mut().push("adapter re-read");
                    std::future::ready(())
                },
            },
            &station_state,
            &networks,
            &prompts,
            &adapter,
            &paths,
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
            "the station is read again once, and a pass with no adapter reads none"
        );
    }

    /// The registration keeps moving while a handler awaits a call of its own
    /// (#1656). Here the station's re-read can only finish once the
    /// registration has been polled through its own call, the shape of zbus
    /// handing the socket's write lock to a queued `RegisterAgent`: with the
    /// registration as one of the loop's arms it was not polled while the
    /// re-read's arm awaited, and the two waited on each other (#1656's W1, 3
    /// of 40 runs, each until the re-read's 25 s deadline).
    ///
    /// **Falsification:** poll the registration as an arm of the event loop
    /// again (the pre-#1656 pump), or not at all, and the re-read never ends.
    #[tokio::test]
    async fn the_registration_moves_while_a_handler_awaits_a_call() {
        let (sent, arrived) = tokio::sync::oneshot::channel::<()>();
        let (sent, arrived) = (RefCell::new(Some(sent)), RefCell::new(Some(arrived)));
        let read = Cell::new(false);
        let (_install, installed) = tokio::sync::watch::channel(true);
        let station_rule = StationRule(installed);
        let (owner_tx, owner_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut owner = fed(owner_rx);
        let quiet = || futures_util::stream::pending::<SignalItem>().boxed_local();
        let (mut added, mut removed, mut station) = (quiet(), quiet(), quiet());
        let (station_state, networks, prompts, adapter) = (
            Mutable::new(Some(Station::default())),
            Mutable::new(Vec::new()),
            Mutable::new(None),
            Mutable::new(Some(Adapter::default())),
        );

        let paths = IwdPaths::default();
        let pump = pump_iwd_events(
            IwdStreams {
                owner: &mut owner,
                added: &mut added,
                removed: &mut removed,
                station: Some(PropsWatch {
                    path: STATION,
                    items: &mut station,
                    rule: &station_rule,
                }),
                adapter: None,
            },
            IwdCalls {
                register: || {
                    let sent = sent.borrow_mut().take();
                    async move {
                        // In flight: it finishes only if polled again.
                        tokio::task::yield_now().await;
                        if let Some(sent) = sent {
                            let _ = sent.send(());
                        }
                        Ok::<(), BusError>(())
                    }
                },
                reread_station: || {
                    let arrived = arrived.borrow_mut().take();
                    let read = &read;
                    async move {
                        if let Some(arrived) = arrived {
                            let _ = arrived.await;
                        }
                        read.set(true);
                    }
                },
                reread_adapter: || std::future::ready(()),
            },
            &station_state,
            &networks,
            &prompts,
            &adapter,
            &paths,
        );
        let drive = async {
            for _ in 0..16 {
                tokio::task::yield_now().await;
            }
            assert!(
                read.get(),
                "the re-read waited on a registration nobody polled"
            );
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
    }

    /// A pass with an adapter and no station (the radio is off) reads the
    /// adapter again once its rule is installed, and once, and does not
    /// register the agent: iwd has nothing to ask a passphrase for until the
    /// station is back, and the pass that finds it registers (#1656).
    ///
    /// **Falsification:** re-read the adapter without waiting for its rule
    /// and the re-read comes first; never re-read it and it is never logged;
    /// register without a station and "register" is logged.
    #[tokio::test]
    async fn the_adapter_is_read_again_once_its_rule_is_installed() {
        let log = RefCell::new(Vec::new());
        let (install, installed) = tokio::sync::watch::channel(false);
        let (owner_tx, owner_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut owner = fed(owner_rx);
        let quiet = || futures_util::stream::pending::<SignalItem>().boxed_local();
        let (mut added, mut removed, mut adapter_items) = (quiet(), quiet(), quiet());
        let adapter_rule = StationRule(installed);
        let adapter = Mutable::new(Some(powered_off_adapter()));
        let (station_state, networks, prompts) = (
            Mutable::new(None),
            Mutable::new(Vec::new()),
            Mutable::new(None),
        );

        let paths = IwdPaths::default();
        let pump = pump_iwd_events(
            IwdStreams {
                owner: &mut owner,
                added: &mut added,
                removed: &mut removed,
                station: None,
                adapter: Some(PropsWatch {
                    path: ADAPTER,
                    items: &mut adapter_items,
                    rule: &adapter_rule,
                }),
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
                reread_adapter: || {
                    log.borrow_mut().push("adapter re-read");
                    std::future::ready(())
                },
            },
            &station_state,
            &networks,
            &prompts,
            &adapter,
            &paths,
        );
        let drive = async {
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
            assert!(log.borrow().is_empty(), "{:?}", log.borrow());
            install.send_replace(true);
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
            assert_eq!(*log.borrow(), ["adapter re-read"]);
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
        assert_eq!(log.into_inner(), ["adapter re-read"]);
    }

    /// iwd leaving with its `Lost` already queued when a pass's pump starts:
    /// the `Lost` is taken first, so neither the registration nor a re-read
    /// calls iwd, whose name then has no owner, and a call to it would
    /// D-Bus-activate the iwd that was just stopped (#1647 review). The
    /// adapter's re-read (#1656) is held to it too.
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
            let (mut added, mut removed, mut station, mut adapter_items) =
                (quiet(), quiet(), quiet(), quiet());
            let (_install, installed) = tokio::sync::watch::channel(true);
            let station_state = Mutable::new(Some(Station::default()));
            let adapter = Mutable::new(Some(Adapter::default()));

            let pumped = tokio::time::timeout(
                Duration::from_secs(5),
                pump_iwd_events(
                    IwdStreams {
                        owner: &mut owner,
                        added: &mut added,
                        removed: &mut removed,
                        station: Some(PropsWatch {
                            path: "/net/connman/iwd/0/3",
                            items: &mut station,
                            rule: &StationRule(installed.clone()),
                        }),
                        adapter: Some(PropsWatch {
                            path: ADAPTER,
                            items: &mut adapter_items,
                            rule: &StationRule(installed),
                        }),
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
                        reread_adapter: || {
                            calls.borrow_mut().push("adapter re-read");
                            std::future::ready(())
                        },
                    },
                    &station_state,
                    &Mutable::new(Vec::new()),
                    &Mutable::new(None),
                    &adapter,
                    &IwdPaths::default(),
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

    /// iwd's `Lost` read while a command (`scan`, `disconnect`, `set_powered`)
    /// is reading one of the paths: since #1659 the clear takes the paths'
    /// write lock, so the `Lost` handler can wait there, and
    /// `beside_registration` polls the registration meanwhile. `iwd_gone` must
    /// be set before that wait, or the registration calls iwd after its `Lost`
    /// was read (#1661 review).
    ///
    /// **Falsification:** set `iwd_gone` after `published.clear().await` (the
    /// #1661 head) and `RegisterAgent` is called.
    #[tokio::test(start_paused = true)]
    async fn a_lost_read_while_a_command_reads_a_path_calls_iwd_no_more() {
        let calls = RefCell::new(Vec::new());
        let lost = noc_item("net.connman.iwd", ":1.7", "");
        let mut owner = futures_util::stream::iter([lost])
            .chain(futures_util::stream::pending())
            .boxed_local();
        let quiet = || futures_util::stream::pending::<SignalItem>().boxed_local();
        let (mut added, mut removed, mut station) = (quiet(), quiet(), quiet());
        let (_install, installed) = tokio::sync::watch::channel(false);
        let station_state = Mutable::new(Some(Station::default()));
        let adapter = Mutable::new(Some(Adapter::default()));
        let (networks, prompts) = (Mutable::new(Vec::new()), Mutable::new(None));
        let rule = StationRule(installed);
        let paths = IwdPaths::default();
        // A command's `paths.station_path()`, caught between its read lock
        // and its clone.
        let command = paths.station.clone().read_owned().await;

        let pump = tokio::time::timeout(
            Duration::from_secs(5),
            pump_iwd_events(
                IwdStreams {
                    owner: &mut owner,
                    added: &mut added,
                    removed: &mut removed,
                    station: Some(PropsWatch {
                        path: STATION,
                        items: &mut station,
                        rule: &rule,
                    }),
                    adapter: None,
                },
                IwdCalls {
                    register: || {
                        calls.borrow_mut().push("RegisterAgent");
                        std::future::ready(Ok(()))
                    },
                    reread_station: || std::future::ready(()),
                    reread_adapter: || std::future::ready(()),
                },
                &station_state,
                &networks,
                &prompts,
                &adapter,
                &paths,
            ),
        );
        let drive = async {
            tokio::task::yield_now().await;
            drop(command);
        };
        let (pumped, ()) = futures_util::future::join(pump, drive).await;
        assert!(pumped.is_err(), "a lost iwd is waited for, not rediscovered");
        assert!(
            calls.borrow().is_empty(),
            "iwd was called after its Lost was read: {:?}",
            calls.borrow()
        );
        assert!(station_state.get_cloned().is_none(), "the state is cleared");
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
    /// The adapter's (#1656) is held to it from the start.
    ///
    /// Every other stream is pending and the registration never answers, so
    /// nothing in the pump reaches a bus.
    ///
    /// **Falsification:** `continue` on a marker in any of the four arms
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
                "adapter PropertiesChanged",
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
                let mut adapter_items = stream_for("adapter PropertiesChanged");
                let station_state = Mutable::new(Some(Station::default()));
                let adapter_state = Mutable::new(Some(Adapter::default()));

                tokio::time::timeout(
                    Duration::from_mins(1),
                    pump_iwd_events(
                        IwdStreams {
                            owner: &mut owner,
                            added: &mut added,
                            removed: &mut removed,
                            station: Some(PropsWatch {
                                path: "/net/connman/iwd/0/3",
                                items: &mut station,
                                rule: &Refused,
                            }),
                            adapter: Some(PropsWatch {
                                path: ADAPTER,
                                items: &mut adapter_items,
                                rule: &Refused,
                            }),
                        },
                        IwdCalls {
                            register: std::future::pending::<Result<(), BusError>>,
                            reread_station: std::future::pending::<()>,
                            reread_adapter: std::future::pending::<()>,
                        },
                        &station_state,
                        &Mutable::new(Vec::new()),
                        &Mutable::new(None),
                        &adapter_state,
                        &IwdPaths::default(),
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
        // decode, which would reach the host's system bus: refuse that here,
        // through the handler's own decode.
        assert!(
            decode_properties_changed(&body).is_some(),
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
    /// the network read the pump makes next names the new one.
    ///
    /// **Falsification:** pass the handler's invalidated list on as empty
    /// and the disconnect row keeps `HOME`; keep the SSID across a network
    /// change and the second row names "home".
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
            handle_station_props_event(&evt, STATION, &published).await;
            let st = published.get_cloned().expect("a station");
            assert_eq!(st.state, state, "{case}");
            assert_eq!(st.connected_network.as_deref(), network, "{case}");
            assert_eq!(st.connected_ssid, None, "{case}");
        }
    }

    /// iwd's adapter, the station's parent (`wiphy_get_path`).
    const ADAPTER: &str = "/net/connman/iwd/0";

    fn powered_off_adapter() -> Adapter {
        Adapter {
            path: ADAPTER.to_owned(),
            powered: false,
            name: "phy0".to_owned(),
        }
    }

    fn event(body: zbus::Message) -> SignalEvent {
        SignalEvent {
            body,
            sender: None,
            timestamp: std::time::SystemTime::now(),
        }
    }

    /// A `PropertiesChanged` at `path` for `iface`, as iwd emits it.
    fn properties_changed_at(
        path: &str,
        iface: &str,
        changed: &[(&str, zbus::zvariant::Value<'static>)],
    ) -> SignalEvent {
        let changed: HashMap<&str, &zbus::zvariant::Value<'_>> =
            changed.iter().map(|(k, x)| (*k, x)).collect();
        let none: &[&str] = &[];
        event(
            zbus::Message::signal(path, "org.freedesktop.DBus.Properties", "PropertiesChanged")
                .expect("signal builder")
                .build(&(iface, changed, none))
                .expect("build PropertiesChanged"),
        )
    }

    /// `InterfacesRemoved` for `ifaces` at `path`, on iwd's root object.
    fn interfaces_removed(path: &str, ifaces: &[&str]) -> SignalItem {
        let path = zbus::zvariant::ObjectPath::try_from(path).expect("an object path");
        SignalItem::Event(event(
            zbus::Message::signal(
                "/",
                "org.freedesktop.DBus.ObjectManager",
                "InterfacesRemoved",
            )
            .expect("signal builder")
            .build(&(path, ifaces))
            .expect("build InterfacesRemoved"),
        ))
    }

    /// `InterfacesAdded` for `ifaces` (no properties) at `path`.
    fn interfaces_added(path: &str, ifaces: &[&str]) -> SignalItem {
        let path = zbus::zvariant::ObjectPath::try_from(path).expect("an object path");
        let ifaces: HashMap<&str, HashMap<&str, zbus::zvariant::Value<'_>>> =
            ifaces.iter().map(|i| (*i, HashMap::new())).collect();
        SignalItem::Event(event(
            zbus::Message::signal("/", "org.freedesktop.DBus.ObjectManager", "InterfacesAdded")
                .expect("signal builder")
                .build(&(path, ifaces))
                .expect("build InterfacesAdded"),
        ))
    }

    /// The adapter handler applies an adapter delta at the adapter's path,
    /// `Powered` both ways, ignores another interface's, and asks for a
    /// re-read of a body it cannot decode (#1656). Until #1656 nothing read
    /// the adapter's path at all.
    ///
    /// **Falsification:** ignore the delta and the first two rows keep the
    /// old `Powered`; apply any interface's and the `Device` row flips it;
    /// answer `true` for an undecodable body and its row fails.
    #[test]
    fn the_adapter_handler_applies_powered_and_ignores_other_interfaces() {
        let rows = [
            (
                "powered off",
                properties_changed_at(
                    ADAPTER,
                    "net.connman.iwd.Adapter",
                    &[("Powered", false.into())],
                ),
                true,
                Some(false),
            ),
            (
                "powered on",
                properties_changed_at(
                    ADAPTER,
                    "net.connman.iwd.Adapter",
                    &[("Powered", true.into())],
                ),
                false,
                Some(true),
            ),
            (
                "another interface",
                properties_changed_at(
                    ADAPTER,
                    "net.connman.iwd.Device",
                    &[("Powered", true.into())],
                ),
                false,
                Some(false),
            ),
            (
                "a body that is not PropertiesChanged's",
                event(
                    zbus::Message::signal(
                        ADAPTER,
                        "org.freedesktop.DBus.Properties",
                        "PropertiesChanged",
                    )
                    .expect("signal builder")
                    .build(&(7_u32,))
                    .expect("build"),
                ),
                false,
                None,
            ),
        ];
        for (case, evt, powered_before, powered_after) in rows {
            let adapter = Mutable::new(Some(Adapter {
                powered: powered_before,
                ..powered_off_adapter()
            }));
            let decoded = apply_adapter_event(&evt, &adapter);
            assert_eq!(decoded, powered_after.is_some(), "{case}");
            assert_eq!(
                adapter.get_cloned().map(|a| a.powered),
                Some(powered_after.unwrap_or(powered_before)),
                "{case}"
            );
        }
    }

    /// An adapter signal the pump cannot decode makes it read the adapter
    /// again, rather than leave `Powered` at whatever it was (#1656). The
    /// adapter's rule is refused here, so the read is the signal's and not
    /// the handshake's.
    ///
    /// **Falsification:** ignore `apply_adapter_event`'s answer at the call
    /// site and nothing is read.
    #[tokio::test(start_paused = true)]
    async fn an_adapter_signal_that_does_not_decode_reads_the_adapter_again() {
        let log = RefCell::new(Vec::new());
        let garbled = SignalItem::Event(event(
            zbus::Message::signal(
                ADAPTER,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
            )
            .expect("signal builder")
            .build(&(7_u32,))
            .expect("build"),
        ));
        let quiet = || futures_util::stream::pending::<SignalItem>().boxed_local();
        let (mut owner, mut added, mut removed) = (quiet(), quiet(), quiet());
        let mut adapter_items = futures_util::stream::iter([garbled])
            .chain(futures_util::stream::pending())
            .boxed_local();
        let adapter = Mutable::new(Some(powered_off_adapter()));
        let (station_state, networks, prompts) = (
            Mutable::new(None),
            Mutable::new(Vec::new()),
            Mutable::new(None),
        );

        let pumped = tokio::time::timeout(
            Duration::from_mins(1),
            pump_iwd_events(
                IwdStreams {
                    owner: &mut owner,
                    added: &mut added,
                    removed: &mut removed,
                    station: None,
                    adapter: Some(PropsWatch {
                        path: ADAPTER,
                        items: &mut adapter_items,
                        rule: &Refused,
                    }),
                },
                IwdCalls {
                    register: std::future::pending::<Result<(), BusError>>,
                    reread_station: std::future::pending::<()>,
                    reread_adapter: || {
                        log.borrow_mut().push("adapter re-read");
                        std::future::ready(())
                    },
                },
                &station_state,
                &networks,
                &prompts,
                &adapter,
                &IwdPaths::default(),
            ),
        )
        .await;
        assert!(pumped.is_err(), "nothing here ends the pass");
        assert_eq!(log.into_inner(), ["adapter re-read"]);
    }

    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    /// An adapter `PropertiesChanged` setting `Powered` to `on`.
    fn powered_to(on: bool) -> SignalItem {
        SignalItem::Event(properties_changed_at(
            ADAPTER,
            "net.connman.iwd.Adapter",
            &[("Powered", on.into())],
        ))
    }

    /// Powering the adapter off, as iwd does it: the station's interface is
    /// removed and the adapter's object stays. The pump clears the station's
    /// side, leaves the adapter published as it was, and returns after
    /// [`REMOVAL_PAUSE`], so the next snapshot says whether the station is
    /// back (#1656, #1657 review). The adapter is watched the whole time, as
    /// the panel's signal sees it: it is never cleared, not even for a
    /// moment, so the power switch never goes insensitive. Before the removal
    /// its `Powered` arrives live from its own receiver.
    ///
    /// Nothing here reaches a bus: the rules are refused, the registration
    /// never answers, and no event reads the networks.
    ///
    /// **Falsification:** clear the adapter on the station's removal and the
    /// watch sees it gone; keep the pass running without the station (the
    /// first #1657 head) and the pump never returns; drop the pause and the
    /// elapsed time is short; ignore the adapter's delta and `Powered` never
    /// moves; hand the station-side clear other paths than the pump's (#1659)
    /// and the station's path stays.
    #[tokio::test(start_paused = true)]
    async fn a_station_removal_keeps_the_adapter_and_rediscovers() {
        use futures_signals::signal::SignalExt;

        let paths = published_paths().await;
        let (removed_tx, removed_rx) = tokio::sync::mpsc::unbounded_channel();
        let (adapter_tx, adapter_rx) = tokio::sync::mpsc::unbounded_channel();
        let quiet = || futures_util::stream::pending::<SignalItem>().boxed_local();
        let (mut owner, mut added, mut station) = (quiet(), quiet(), quiet());
        let (mut removed, mut adapter_items) = (fed(removed_rx), fed(adapter_rx));
        let station_state = Mutable::new(Some(Station {
            path: STATION.to_owned(),
            ..Station::default()
        }));
        let (network, prompt) = network_and_prompt();
        let (networks, prompts) = (Mutable::new(vec![network]), Mutable::new(Some(prompt)));
        let adapter = Mutable::new(Some(Adapter {
            powered: true,
            ..powered_off_adapter()
        }));
        let returned = Cell::new(None);
        let seen = RefCell::new(Vec::new());

        let pump = async {
            pump_iwd_events(
                IwdStreams {
                    owner: &mut owner,
                    added: &mut added,
                    removed: &mut removed,
                    station: Some(PropsWatch {
                        path: STATION,
                        items: &mut station,
                        rule: &Refused,
                    }),
                    adapter: Some(PropsWatch {
                        path: ADAPTER,
                        items: &mut adapter_items,
                        rule: &Refused,
                    }),
                },
                IwdCalls {
                    register: std::future::pending::<Result<(), BusError>>,
                    reread_station: std::future::pending::<()>,
                    reread_adapter: std::future::pending::<()>,
                },
                &station_state,
                &networks,
                &prompts,
                &adapter,
                &paths,
            )
            .await;
            returned.set(Some(tokio::time::Instant::now()));
        };
        // Every value the adapter takes, as the panel's signal sees it.
        let watch = async {
            let mut values = adapter.signal_cloned().to_stream();
            while let Some(value) = values.next().await {
                seen.borrow_mut().push(value.map(|a| a.powered));
            }
        };
        let powered = || adapter.get_cloned().map(|a| a.powered);
        let drive = async {
            adapter_tx
                .send(powered_to(false))
                .expect("the pump reads it");
            settle().await;
            assert_eq!(powered(), Some(false), "the radio's Powered arrives live");

            let removal = tokio::time::Instant::now();
            removed_tx
                .send(interfaces_removed(STATION, &["net.connman.iwd.Station"]))
                .expect("the pump reads it");
            settle().await;
            assert!(station_state.get_cloned().is_none(), "the station is gone");
            assert!(networks.lock_ref().is_empty(), "its networks are gone");
            assert!(prompts.lock_ref().is_none(), "its prompt is gone");
            assert_eq!(powered(), Some(false), "the adapter stays, as it was");
            assert_eq!(paths_of(&paths).await, (String::new(), ADAPTER.to_owned()));
            assert!(returned.get().is_none(), "the pass ended before its pause");
            removal
        };
        let (watch, both) = (
            std::pin::pin!(watch),
            std::pin::pin!(futures_util::future::join(pump, drive)),
        );
        let ended = tokio::time::timeout(
            Duration::from_mins(1),
            futures_util::future::select(watch, both),
        )
        .await
        .expect("the station's removal sends the pump back to discovery");
        let futures_util::future::Either::Right((((), removal), _)) = ended else {
            unreachable!("the watch never ends");
        };
        assert_eq!(
            returned.get().map(|at| at - removal),
            Some(REMOVAL_PAUSE),
            "the pump rediscovers after the pause"
        );
        assert_eq!(
            *seen.borrow(),
            [Some(true), Some(false)],
            "the adapter left the panel, or took another value"
        );
    }

    /// A pass that starts with no station (the radio is off): the adapter's
    /// `Powered` arrives live, both ways, from its own receiver; an
    /// `InterfacesAdded` that brings no station or adapter is no reason to
    /// rediscover, and the station's return is (#1656). Until #1656 such a
    /// pass published no adapter and retried on a timer.
    ///
    /// **Falsification:** drop the adapter arm or ignore its delta and
    /// `Powered` never moves; rediscover on any `InterfacesAdded` and the
    /// pump returns on the known network; never rediscover on the station's
    /// return and the pump never returns.
    #[tokio::test(start_paused = true)]
    async fn a_pass_with_no_station_follows_powered_and_ends_on_the_stations_return() {
        let (added_tx, added_rx) = tokio::sync::mpsc::unbounded_channel();
        let (adapter_tx, adapter_rx) = tokio::sync::mpsc::unbounded_channel();
        let quiet = || futures_util::stream::pending::<SignalItem>().boxed_local();
        let (mut owner, mut removed) = (quiet(), quiet());
        let (mut added, mut adapter_items) = (fed(added_rx), fed(adapter_rx));
        let adapter = Mutable::new(Some(powered_off_adapter()));
        let returned = Cell::new(false);

        let pump = async {
            pump_iwd_events(
                IwdStreams {
                    owner: &mut owner,
                    added: &mut added,
                    removed: &mut removed,
                    station: None,
                    adapter: Some(PropsWatch {
                        path: ADAPTER,
                        items: &mut adapter_items,
                        rule: &Refused,
                    }),
                },
                IwdCalls {
                    register: std::future::pending::<Result<(), BusError>>,
                    reread_station: std::future::pending::<()>,
                    reread_adapter: std::future::pending::<()>,
                },
                &Mutable::new(None),
                &Mutable::new(Vec::new()),
                &Mutable::new(None),
                &adapter,
                &IwdPaths::default(),
            )
            .await;
            returned.set(true);
        };
        let powered = || adapter.get_cloned().map(|a| a.powered);
        let drive = async {
            let send = |tx: &tokio::sync::mpsc::UnboundedSender<SignalItem>, item| {
                tx.send(item).expect("the pump reads it");
            };
            send(&adapter_tx, powered_to(true));
            settle().await;
            assert_eq!(powered(), Some(true), "the radio is on");
            send(&adapter_tx, powered_to(false));
            settle().await;
            assert_eq!(powered(), Some(false), "the radio is off again");
            send(&adapter_tx, powered_to(true));
            settle().await;
            assert_eq!(powered(), Some(true), "the radio is on again");

            send(
                &added_tx,
                interfaces_added(
                    "/net/connman/iwd/686f6d65_psk",
                    &["net.connman.iwd.KnownNetwork"],
                ),
            );
            settle().await;
            assert!(!returned.get(), "a known network brings no station");
            send(
                &added_tx,
                interfaces_added(STATION, &["net.connman.iwd.Station"]),
            );
        };
        tokio::time::timeout(
            Duration::from_mins(1),
            futures_util::future::join(pump, drive),
        )
        .await
        .expect("the station's return sends the pump back to discovery");
        assert!(returned.get());
        assert_eq!(powered(), Some(true), "rediscovering keeps the state");
    }

    /// (#1657 review) a pass with no station still follows iwd changing
    /// hands. iwd leaving clears the adapter it published, so the switch
    /// does not stay live for an iwd that is gone, and the pass waits; a new
    /// iwd ends it, so the new one is snapshotted and registered with.
    ///
    /// **Falsification:** clear only the station's side on iwd's `Lost` and
    /// the adapter stays; hand the pump's clear other paths than its own
    /// (#1659) and the adapter's path does.
    #[tokio::test(start_paused = true)]
    async fn a_pass_with_no_station_clears_on_lost_and_ends_on_a_new_iwd() {
        let paths = published_paths().await;
        let (owner_tx, owner_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut owner = fed(owner_rx);
        let quiet = || futures_util::stream::pending::<SignalItem>().boxed_local();
        let (mut added, mut removed, mut adapter_items) = (quiet(), quiet(), quiet());
        let adapter = Mutable::new(Some(powered_off_adapter()));
        let (station_state, networks, prompts) = (
            Mutable::new(None),
            Mutable::new(Vec::new()),
            Mutable::new(None),
        );
        let returned = Cell::new(false);
        let pump = async {
            pump_iwd_events(
                IwdStreams {
                    owner: &mut owner,
                    added: &mut added,
                    removed: &mut removed,
                    station: None,
                    adapter: Some(PropsWatch {
                        path: ADAPTER,
                        items: &mut adapter_items,
                        rule: &Refused,
                    }),
                },
                IwdCalls {
                    register: std::future::pending::<Result<(), BusError>>,
                    reread_station: std::future::pending::<()>,
                    reread_adapter: std::future::pending::<()>,
                },
                &station_state,
                &networks,
                &prompts,
                &adapter,
                &paths,
            )
            .await;
            returned.set(true);
        };
        let drive = async {
            owner_tx
                .send(noc_item("net.connman.iwd", ":1.7", ""))
                .expect("the pump reads the owner");
            settle().await;
            assert!(
                adapter.get_cloned().is_none(),
                "iwd left and the panel kept its adapter"
            );
            assert_eq!(paths_of(&paths).await, (String::new(), String::new()));
            assert!(
                !returned.get(),
                "a lost iwd is waited for, not rediscovered"
            );
            owner_tx
                .send(noc_item("net.connman.iwd", "", ":1.8"))
                .expect("the pump reads the owner");
        };
        tokio::time::timeout(
            Duration::from_mins(1),
            futures_util::future::join(pump, drive),
        )
        .await
        .expect("a new iwd ends the pass");
        assert!(returned.get());
    }

    /// The adapter's own object removed: the wiphy is gone, and the station
    /// with it. Everything is cleared and the pump rediscovers after
    /// [`REMOVAL_PAUSE`], what the station's removal used to do (#1656).
    ///
    /// **Falsification:** treat it as the station's removal and the adapter
    /// stays; drop the pause and the elapsed time is short; hand this clear
    /// other paths than the pump's and the commands keep both (#1659).
    #[tokio::test(start_paused = true)]
    async fn the_adapters_removal_clears_everything_and_rediscovers() {
        let quiet = || futures_util::stream::pending::<SignalItem>().boxed_local();
        let (mut owner, mut added, mut adapter_items) = (quiet(), quiet(), quiet());
        let mut removed = futures_util::stream::iter([interfaces_removed(
            ADAPTER,
            &["net.connman.iwd.Adapter", "org.freedesktop.DBus.Properties"],
        )])
        .chain(futures_util::stream::pending())
        .boxed_local();
        let adapter = Mutable::new(Some(powered_off_adapter()));
        let paths = published_paths().await;
        let started = tokio::time::Instant::now();

        tokio::time::timeout(
            Duration::from_mins(1),
            pump_iwd_events(
                IwdStreams {
                    owner: &mut owner,
                    added: &mut added,
                    removed: &mut removed,
                    station: None,
                    adapter: Some(PropsWatch {
                        path: ADAPTER,
                        items: &mut adapter_items,
                        rule: &Refused,
                    }),
                },
                IwdCalls {
                    register: std::future::pending::<Result<(), BusError>>,
                    reread_station: std::future::pending::<()>,
                    reread_adapter: std::future::pending::<()>,
                },
                &Mutable::new(None),
                &Mutable::new(Vec::new()),
                &Mutable::new(None),
                &adapter,
                &paths,
            ),
        )
        .await
        .expect("the adapter's removal sends the pump back to discovery");
        assert!(adapter.get_cloned().is_none(), "the adapter is cleared");
        assert_eq!(paths_of(&paths).await, (String::new(), String::new()));
        assert_eq!(started.elapsed(), REMOVAL_PAUSE);
    }

    /// A pass with no station waits for iwd and nothing else: no timer
    /// ends it, and it calls nothing, the adapter's re-read aside, which
    /// waits on a rule refused here (#1656). Until #1656 such a pass slept
    /// five seconds and took another snapshot, for as long as the radio
    /// was off.
    ///
    /// **Falsification:** return from the pump on a timer and it ends;
    /// register without a station and the call is logged.
    #[tokio::test(start_paused = true)]
    async fn a_pass_with_no_station_waits_for_iwd_without_a_timer() {
        let calls = RefCell::new(Vec::new());
        let quiet = || futures_util::stream::pending::<SignalItem>().boxed_local();
        let (mut owner, mut added, mut removed, mut adapter_items) =
            (quiet(), quiet(), quiet(), quiet());
        let adapter = Mutable::new(Some(powered_off_adapter()));

        let pumped = tokio::time::timeout(
            Duration::from_hours(1),
            pump_iwd_events(
                IwdStreams {
                    owner: &mut owner,
                    added: &mut added,
                    removed: &mut removed,
                    station: None,
                    adapter: Some(PropsWatch {
                        path: ADAPTER,
                        items: &mut adapter_items,
                        rule: &Refused,
                    }),
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
                    reread_adapter: || {
                        calls.borrow_mut().push("adapter re-read");
                        std::future::ready(())
                    },
                },
                &Mutable::new(None),
                &Mutable::new(Vec::new()),
                &Mutable::new(None),
                &adapter,
                &IwdPaths::default(),
            ),
        )
        .await;
        assert!(pumped.is_err(), "the pass ended on its own");
        assert!(calls.borrow().is_empty(), "{:?}", calls.borrow());
        assert_eq!(adapter.get_cloned(), Some(powered_off_adapter()));
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

    /// A connected network row and a prompt for it, for the clear tests.
    fn network_and_prompt() -> (super::WifiNetwork, super::PromptRequest) {
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
        (network, prompt)
    }

    /// The commands' paths with [`STATION`] and [`ADAPTER`] published.
    async fn published_paths() -> IwdPaths {
        let paths = IwdPaths::default();
        paths.set_station_path(Some(STATION)).await;
        paths.set_adapter_path(Some(ADAPTER)).await;
        paths
    }

    /// The station's and the adapter's path the commands address.
    async fn paths_of(paths: &IwdPaths) -> (String, String) {
        (paths.station_path().await, paths.adapter_path().await)
    }

    /// `clear_published` empties all four slots, and wakes none that is
    /// already clear (#1654 review). The pump and the wait for iwd go through
    /// it, but the real-bus W6 tests read only the station and the adapter,
    /// so dropping the network list from it left every test green while the
    /// panel kept the old network's "Connected" row with iwd gone. The skip
    /// is what keeps a clear of a watcher with nothing published from waking
    /// the panel.
    ///
    /// **Falsification:** drop any one slot from `clear_published`, or keep
    /// either of the commands' paths (#1659), and the first half fails; set
    /// the four unconditionally (the pump's pre-#1654 closure) and the second
    /// half sees a wake.
    #[tokio::test]
    async fn clear_published_clears_every_slot_and_wakes_none_already_clear() {
        use futures_signals::signal::SignalExt;
        use futures_util::FutureExt;

        let (network, prompt) = network_and_prompt();
        let station = Mutable::new(Some(Station::default()));
        let networks = Mutable::new(vec![network]);
        let prompts = Mutable::new(Some(prompt));
        let adapter = Mutable::new(Some(Adapter::default()));
        let paths = published_paths().await;

        super::clear_published(&station, &networks, &prompts, &adapter, &paths).await;
        assert!(station.lock_ref().is_none(), "the station is kept");
        assert!(networks.lock_ref().is_empty(), "the network list is kept");
        assert!(prompts.lock_ref().is_none(), "the prompt is kept");
        assert!(adapter.lock_ref().is_none(), "the adapter is kept");
        assert_eq!(paths_of(&paths).await, (String::new(), String::new()));

        let mut station_wakes = station.signal_cloned().to_stream();
        let mut network_wakes = networks.signal_cloned().to_stream();
        let mut prompt_wakes = prompts.signal_cloned().to_stream();
        let mut adapter_wakes = adapter.signal_cloned().to_stream();
        // A signal yields its current value first.
        assert!(station_wakes.next().now_or_never().is_some());
        assert!(network_wakes.next().now_or_never().is_some());
        assert!(prompt_wakes.next().now_or_never().is_some());
        assert!(adapter_wakes.next().now_or_never().is_some());

        super::clear_published(&station, &networks, &prompts, &adapter, &paths).await;
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

    /// `clear_station_side` empties the station, its networks and a prompt,
    /// leaves the adapter as it was without waking it, and wakes nothing that
    /// is already clear (#1656). It is what a station's removal and a
    /// snapshot with no station clear: the adapter they keep is what the
    /// panel's power switch turns the radio back on with.
    ///
    /// The commands' paths go the same way (#1659): the station's is cleared,
    /// so `scan` and `disconnect` address no station that is gone, and the
    /// adapter's is kept, so the power switch can turn the radio back on.
    ///
    /// **Falsification:** clear the adapter or its path in it (the pre-#1656
    /// clear) and its assertion fails; drop a station-side slot or keep the
    /// station's path and its assertion does; set them unconditionally and
    /// the second half wakes.
    #[tokio::test]
    async fn clear_station_side_keeps_the_adapter_and_wakes_none_already_clear() {
        use futures_signals::signal::SignalExt;
        use futures_util::FutureExt;

        let (network, prompt) = network_and_prompt();
        let station = Mutable::new(Some(Station::default()));
        let networks = Mutable::new(vec![network]);
        let prompts = Mutable::new(Some(prompt));
        let adapter = Mutable::new(Some(powered_off_adapter()));
        let mut adapter_wakes = adapter.signal_cloned().to_stream();
        assert!(adapter_wakes.next().now_or_never().is_some());
        let paths = published_paths().await;

        super::clear_station_side(&station, &networks, &prompts, &paths).await;
        assert!(station.lock_ref().is_none(), "the station is kept");
        assert!(networks.lock_ref().is_empty(), "the network list is kept");
        assert!(prompts.lock_ref().is_none(), "the prompt is kept");
        assert_eq!(adapter.get_cloned(), Some(powered_off_adapter()));
        assert_eq!(paths_of(&paths).await, (String::new(), ADAPTER.to_owned()));
        assert!(
            adapter_wakes.next().now_or_never().is_none(),
            "the adapter woke"
        );

        let mut station_wakes = station.signal_cloned().to_stream();
        let mut network_wakes = networks.signal_cloned().to_stream();
        let mut prompt_wakes = prompts.signal_cloned().to_stream();
        assert!(station_wakes.next().now_or_never().is_some());
        assert!(network_wakes.next().now_or_never().is_some());
        assert!(prompt_wakes.next().now_or_never().is_some());

        super::clear_station_side(&station, &networks, &prompts, &paths).await;
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
    }
}

// The production `run_wifi_watcher`, through the process-global system
// connection, against a real `dbus-daemon` and a scripted iwd, in a re-exec'd
// child (#1647 review W1, W2). A child module, not a file under `tests/`,
// because `run_wifi_watcher` is private.
#[cfg(all(test, feature = "system-tests"))]
mod real_bus;
