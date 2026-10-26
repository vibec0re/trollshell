//! Main iwd watcher task: discovery loop, D-Bus event pump, and signal
//! subscription management.

use futures_signals::signal::Mutable;
use futures_util::StreamExt;
use hytte_bus::{BusKind, SignalItem};
use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

use super::client::{discover_station, refresh_networks, refresh_station, register_iwd_agent};
use super::parse::{
    adapter_path_from_station, apply_adapter_props_delta, apply_station_props_delta,
    refresh_adapter_from_managed, refresh_station_from_managed, station_removed_from_event,
};
use super::types::{Adapter, PromptRequest, Station, WifiNetwork};
use super::{AGENT_PATH, IwdPaths};

// ── Subscription bundle ───────────────────────────────────────────────────────

struct IwdSubs {
    station_props: hytte_bus::SignalSubscription,
    added: hytte_bus::SignalSubscription,
    removed: hytte_bus::SignalSubscription,
    /// The bus's `NameOwnerChanged`, every name, filtered in code by
    /// [`iwd_owner_change`]. The `BlueZ` agent watches its daemon the same
    /// way.
    owner: hytte_bus::SignalSubscription,
}

fn subscribe_iwd_events(station_path: &zbus::zvariant::OwnedObjectPath) -> IwdSubs {
    let owner = hytte_bus::signals(BusKind::System, "org.freedesktop.DBus")
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .signal("NameOwnerChanged")
        .start();
    let station_props = hytte_bus::signals(BusKind::System, "net.connman.iwd")
        .at_path(station_path.as_str().to_string())
        .iface("org.freedesktop.DBus.Properties")
        .signal("PropertiesChanged")
        .start();
    let added = hytte_bus::signals(BusKind::System, "net.connman.iwd")
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .signal("InterfacesAdded")
        .start();
    let removed = hytte_bus::signals(BusKind::System, "net.connman.iwd")
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .signal("InterfacesRemoved")
        .start();
    IwdSubs {
        station_props,
        added,
        removed,
        owner,
    }
}

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

// ── Main watcher task ─────────────────────────────────────────────────────────

/// `paths` is the committed iwd backend's own path cache — passed in rather than
/// reached for through a process-global, so that a backend switch retires this
/// watcher's paths along with the watcher (#633; see [`IwdPaths`]).
pub(super) async fn run_wifi_watcher(
    station_mutable: Mutable<Option<Station>>,
    networks_mutable: Mutable<Vec<WifiNetwork>>,
    prompts_mutable: Mutable<Option<PromptRequest>>,
    adapter_mutable: Mutable<Option<Adapter>>,
    paths: IwdPaths,
) {
    'discovery: loop {
        let Some((managed, station_path)) = discover_station().await else {
            continue 'discovery;
        };

        paths.set_station_path(station_path.as_str()).await;
        let adapter_path = adapter_path_from_station(station_path.as_str());
        if !adapter_path.is_empty() {
            paths.set_adapter_path(&adapter_path).await;
        }
        tracing::info!(path = station_path.as_str(), "wifi station found");

        refresh_adapter_from_managed(&managed, station_path.as_str(), &adapter_mutable);
        refresh_station_from_managed(&managed, &station_path, &station_mutable);
        refresh_networks(station_path.as_str(), &station_mutable, &networks_mutable).await;

        let subs = subscribe_iwd_events(&station_path);

        match register_iwd_agent(AGENT_PATH).await {
            Ok(()) => tracing::info!("hytte iwd agent registered"),
            Err(e) => tracing::warn!(error = %e, "iwd RegisterAgent failed"),
        }

        let station_path_str = station_path.as_str().to_string();
        // Returns when the station was removed or iwd changed hands — falls
        // through to re-discover (and re-register the agent) on the next
        // iteration of 'discovery.
        pump_iwd_events(
            subs,
            &station_path_str,
            &station_mutable,
            &networks_mutable,
            &prompts_mutable,
            &adapter_mutable,
        )
        .await;
    }
}

// ── Event pump ────────────────────────────────────────────────────────────────

/// Drive the iwd event loop. Returns when the watcher needs to restart
/// discovery: the station was removed, or iwd changed hands (see
/// [`owner_reaction`]).
async fn pump_iwd_events(
    subs: IwdSubs,
    station_path_str: &str,
    station_mutable: &Mutable<Option<Station>>,
    networks_mutable: &Mutable<Vec<WifiNetwork>>,
    prompts_mutable: &Mutable<Option<PromptRequest>>,
    adapter_mutable: &Mutable<Option<Adapter>>,
) {
    let mut station_events = subs.station_props.events();
    let mut added_events = subs.added.events();
    let mut removed_events = subs.removed.events();
    let mut owner_items = subs.owner.items();

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
    use super::{IwdOwner, OwnerReaction, iwd_owner_change, owner_reaction};
    use hytte_bus::{SignalEvent, SignalItem};

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
