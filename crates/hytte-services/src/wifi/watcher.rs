//! Main iwd watcher task: discovery loop, D-Bus event pump, and signal
//! subscription management.

use futures_signals::signal::Mutable;
use futures_util::{FutureExt as _, Stream, StreamExt};
use hytte_bus::{BusKind, SignalEvent, SignalItem, SignalSubscription};
use std::collections::HashMap;
use std::time::Duration;
use zbus::zvariant::OwnedValue;

use super::client::{
    discover_station, is_already_registered, iwd_has_owner, refresh_networks, refresh_station,
    register_iwd_agent,
};
use super::parse::{
    adapter_path_from_station, apply_adapter_props_delta, apply_station_props_delta,
    refresh_adapter_from_managed, refresh_station_from_managed, station_removed_from_event,
};
use super::types::{Adapter, PromptRequest, Station, WifiNetwork};
use super::{AGENT_PATH, IwdPaths};
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

/// The streams [`pump_iwd_events`] reads. The first three belong to
/// [`run_wifi_watcher`] and outlive any one pump; the station's is per
/// discovery, since its path comes from the snapshot.
struct IwdStreams<'a, O, E> {
    /// iwd's owner: [`name_owner_changed`], read with its markers.
    owner: &'a mut O,
    /// `InterfacesAdded` on iwd's root object.
    added: &'a mut E,
    /// `InterfacesRemoved` on iwd's root object.
    removed: &'a mut E,
    /// `PropertiesChanged` on the station.
    station: &'a mut E,
}

/// Discard whatever `stream` has queued, without waiting for more.
///
/// [`run_wifi_watcher`] does this right before each discovery: anything queued
/// by then is older than the snapshot that discovery takes, which supersedes
/// it. What arrives after this, while the snapshot is taken, stays queued for
/// the pump.
fn drain<S: Stream + Unpin>(stream: &mut S) {
    while let Some(Some(_)) = stream.next().now_or_never() {}
}

/// Let `pause` pass, reading and discarding `owner` meanwhile.
///
/// The owner subscription matches every name on the system bus, and nothing
/// else reads it while discovery waits to retry. Left unread, a busy bus can
/// overrun its receiver, which `hytte-bus` logs as a lag. Whatever it carried
/// is superseded by the next discovery, which drains, asks the bus, and
/// re-reads iwd.
async fn pause_reading<S: Stream + Unpin>(owner: &mut S, pause: Duration) {
    let _ = tokio::time::timeout(pause, async {
        while owner.next().await.is_some() {}
        std::future::pending::<()>().await;
    })
    .await;
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

/// Classify one item from [`IwdSubs::owner`]. `None` for any name other
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
/// It reads `owner`, the watcher's own subscription, rather than opening one:
/// that subscription is long in place, and the owner change that wakes this
/// is consumed here instead of reaching the next pump as a change that
/// predates its snapshot.
async fn wait_for_iwd<S>(owner: &mut S)
where
    S: Stream<Item = SignalItem> + Unpin,
{
    wait_for_owner(owner, iwd_has_owner, IWD_RECHECK).await;
}

/// [`wait_for_iwd`] with the bus question and the re-check interval injected,
/// so a test can drive it without a bus under paused time.
///
/// Asks `has_owner`, and returns on the first `Ok(true)`. Otherwise waits for
/// the next sign that iwd may be back ([`next_iwd_owner`]) or `recheck`,
/// whichever comes first, and asks again. A failed question is asked again
/// the same way.
async fn wait_for_owner<S, H, F>(owner: &mut S, has_owner: H, recheck: Duration)
where
    S: Stream<Item = SignalItem> + Unpin,
    H: Fn() -> F,
    F: Future<Output = Result<bool, hytte_bus::BusError>>,
{
    let mut announced = false;
    loop {
        match has_owner().await {
            Ok(true) => return,
            Ok(false) if !announced => {
                announced = true;
                tracing::info!("wifi: iwd is not running; waiting for it to return");
            }
            Ok(false) => {}
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
/// the snapshot. Each discovery first drains what was queued before it
/// ([`drain`]), since its snapshot supersedes that.
///
/// Only the watcher's first discovery calls iwd unasked: the backend probe
/// chose iwd, maybe as an activatable name only, and that first call is what
/// starts it, as before. Every later discovery waits for iwd to have an owner
/// first ([`wait_for_iwd`]).
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
    // The receivers are taken now, not when a pump starts: a broadcast
    // receiver only sees what is sent after it exists.
    let mut owner_items = owner_sub.items();
    let mut added_events = added_sub.events();
    let mut removed_events = removed_sub.events();

    let mut first_discovery = true;

    'discovery: loop {
        drain(&mut owner_items);
        drain(&mut added_events);
        drain(&mut removed_events);
        if first_discovery {
            first_discovery = false;
        } else {
            wait_for_iwd(&mut owner_items).await;
        }

        let Some((managed, station_path)) = discover_station().await else {
            pause_reading(&mut owner_items, DISCOVERY_RETRY).await;
            continue 'discovery;
        };

        // The station's own properties, subscribed as soon as the snapshot
        // names the station, and so before the network read below. Its path
        // is not known before the snapshot, so a station change between the
        // snapshot and this line still waits for the station's next change.
        let station_sub = station_props_signal(station_path.as_str());
        let mut station_events = station_sub.events();

        paths.set_station_path(station_path.as_str()).await;
        let adapter_path = adapter_path_from_station(station_path.as_str());
        if !adapter_path.is_empty() {
            paths.set_adapter_path(&adapter_path).await;
        }
        tracing::info!(path = station_path.as_str(), "wifi station found");

        refresh_adapter_from_managed(&managed, station_path.as_str(), &adapter_mutable);
        refresh_station_from_managed(&managed, &station_path, &station_mutable);
        refresh_networks(station_path.as_str(), &station_mutable, &networks_mutable).await;

        let station_path_str = station_path.as_str().to_string();
        // Returns when the station was removed or iwd changed hands — falls
        // through to re-discover (and re-register the agent) on the next
        // iteration of 'discovery.
        pump_iwd_events(
            IwdStreams {
                owner: &mut owner_items,
                added: &mut added_events,
                removed: &mut removed_events,
                station: &mut station_events,
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
/// discovery: the station was removed, or iwd changed hands (see
/// [`owner_reaction`]).
///
/// It also registers our agent with iwd, and retries until iwd takes it
/// ([`register_agent_until_accepted`]). The registration is one arm of the
/// loop, so events keep flowing while it backs off, and it stops being polled
/// once iwd is gone, like every other arm that would call iwd.
async fn pump_iwd_events<O, E>(
    streams: IwdStreams<'_, O, E>,
    station_path_str: &str,
    station_mutable: &Mutable<Option<Station>>,
    networks_mutable: &Mutable<Vec<WifiNetwork>>,
    prompts_mutable: &Mutable<Option<PromptRequest>>,
    adapter_mutable: &Mutable<Option<Adapter>>,
) where
    O: Stream<Item = SignalItem> + Unpin,
    E: Stream<Item = SignalEvent> + Unpin,
{
    let IwdStreams {
        owner: owner_items,
        added: added_events,
        removed: removed_events,
        station: station_events,
    } = streams;

    let mut registering = std::pin::pin!(register_agent_until_accepted(
        || register_iwd_agent(AGENT_PATH),
        super::nm_registrar::REGISTER_RETRY,
    ));
    let mut registered = false;

    let clear = || {
        station_mutable.set(None);
        networks_mutable.set(Vec::new());
        prompts_mutable.set(None);
        adapter_mutable.set(None);
    };
    // Set once iwd has exited. From then on the other arms are not polled.
    // Anything still queued came from the dead daemon, and acting on it would
    // call `net.connman.iwd`, which D-Bus-activates a stopped iwd.
    let mut iwd_gone = false;

    loop {
        tokio::select! {
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
            () = &mut registering, if !registered && !iwd_gone => {
                registered = true;
            }
            Some(evt) = station_events.next(), if !iwd_gone => {
                let should_refresh = handle_station_props_event(
                    &evt, station_path_str, station_mutable, adapter_mutable,
                ).await;
                if should_refresh {
                    refresh_networks(station_path_str, station_mutable, networks_mutable).await;
                }
            }
            Some(_) = added_events.next(), if !iwd_gone => {
                refresh_networks(station_path_str, station_mutable, networks_mutable).await;
            }
            Some(evt) = removed_events.next(), if !iwd_gone => {
                if station_removed_from_event(&evt.body, station_path_str) {
                    tracing::warn!(path = station_path_str, "iwd station removed — rewatching");
                    clear();
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    return;
                }
                refresh_networks(station_path_str, station_mutable, networks_mutable).await;
            }
            // Every stream has ended (or iwd is gone and the owner stream has
            // ended): nothing is left to wait on here, so start over.
            else => return,
        }
    }
}

/// Decode `PropertiesChanged` body. Applies the delta directly for known
/// interfaces (Station/Adapter1) to avoid a full `GetAll` round-trip, and
/// signals whether the caller should refresh the network list.
async fn handle_station_props_event(
    evt: &hytte_bus::SignalEvent,
    station_path_str: &str,
    station_mutable: &Mutable<Option<Station>>,
    adapter_mutable: &Mutable<Option<Adapter>>,
) -> bool {
    let Ok((iface, changed, _)) = evt
        .body
        .body()
        .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
    else {
        // Can't decode — full refresh to be safe.
        refresh_station(station_path_str, station_mutable).await;
        return true;
    };

    if iface == "net.connman.iwd.Station" {
        apply_station_props_delta(&changed, station_mutable);
        true
    } else if iface == "net.connman.iwd.Adapter1" {
        apply_adapter_props_delta(&changed, adapter_mutable);
        false
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IWD_RECHECK, IwdOwner, OwnerReaction, drain, iwd_owner_change, next_iwd_owner,
        owner_reaction, register_agent_until_accepted, wait_for_owner,
    };
    use crate::retry;
    use futures_util::StreamExt;
    use hytte_bus::{BusError, SignalEvent, SignalItem};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
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
        )
        .await;

        assert_eq!(asked.get(), 3, "a no, a failure, then the yes");
        assert_eq!(
            started.elapsed(),
            IWD_RECHECK * 2,
            "with no wake, each re-ask waits out the re-check interval"
        );
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

    /// Draining discards what is queued and nothing that comes after it. That
    /// is what keeps an owner change made while a snapshot is taken for the
    /// pump, while one older than the snapshot is dropped.
    ///
    /// **Falsification:** make `drain` a no-op and the stale item comes out
    /// first.
    #[tokio::test]
    async fn draining_discards_only_what_is_already_queued() {
        let (tx, mut rx) = futures_channel::mpsc::unbounded::<&str>();
        tx.unbounded_send("before the snapshot").expect("open");
        tx.unbounded_send("also before").expect("open");

        drain(&mut rx);
        tx.unbounded_send("during the snapshot").expect("open");

        assert_eq!(rx.next().await, Some("during the snapshot"));
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
}
