//! Main iwd watcher task: discovery loop, D-Bus event pump, and signal
//! subscription management.

use futures_signals::signal::Mutable;
use futures_util::StreamExt;
use hytte_bus::BusKind;
use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

use super::client::{discover_station, refresh_networks, refresh_station, register_iwd_agent};
use super::parse::{
    adapter_path_from_station, apply_adapter_props_delta, apply_station_props_delta,
    refresh_adapter_from_managed, refresh_station_from_managed, station_removed_from_event,
};
use super::types::{Adapter, PromptRequest, Station, WifiNetwork};
use super::{AGENT_PATH, set_current_adapter_path, set_station_path};

// ── Subscription bundle ───────────────────────────────────────────────────────

struct IwdSubs {
    station_props: hytte_bus::SignalSubscription,
    added: hytte_bus::SignalSubscription,
    removed: hytte_bus::SignalSubscription,
}

fn subscribe_iwd_events(station_path: &zbus::zvariant::OwnedObjectPath) -> IwdSubs {
    let station_props = hytte_bus::signals("net.connman.iwd")
        .bus(BusKind::System)
        .at_path(station_path.as_str().to_string())
        .iface("org.freedesktop.DBus.Properties")
        .signal("PropertiesChanged")
        .start();
    let added = hytte_bus::signals("net.connman.iwd")
        .bus(BusKind::System)
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .signal("InterfacesAdded")
        .start();
    let removed = hytte_bus::signals("net.connman.iwd")
        .bus(BusKind::System)
        .at_path("/")
        .iface("org.freedesktop.DBus.ObjectManager")
        .signal("InterfacesRemoved")
        .start();
    IwdSubs {
        station_props,
        added,
        removed,
    }
}

// ── Main watcher task ─────────────────────────────────────────────────────────

pub(super) async fn run_wifi_watcher(
    station_mutable: Mutable<Option<Station>>,
    networks_mutable: Mutable<Vec<WifiNetwork>>,
    prompts_mutable: Mutable<Option<PromptRequest>>,
    adapter_mutable: Mutable<Option<Adapter>>,
) {
    'discovery: loop {
        let Some((managed, station_path)) = discover_station().await else {
            continue 'discovery;
        };

        set_station_path(station_path.as_str()).await;
        let adapter_path = adapter_path_from_station(station_path.as_str());
        if !adapter_path.is_empty() {
            set_current_adapter_path(&adapter_path).await;
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
        // Returns only when the station was removed — falls through to
        // re-discover on the next iteration of 'discovery.
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

/// Drive the iwd event loop. Returns when the station was removed and the
/// watcher needs to restart discovery.
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

    loop {
        tokio::select! {
            Some(evt) = station_events.next() => {
                let should_refresh = handle_station_props_event(
                    &evt, station_path_str, station_mutable, adapter_mutable,
                ).await;
                if should_refresh {
                    refresh_networks(station_path_str, station_mutable, networks_mutable).await;
                }
            }
            Some(_) = added_events.next() => {
                refresh_networks(station_path_str, station_mutable, networks_mutable).await;
            }
            Some(evt) = removed_events.next() => {
                if station_removed_from_event(&evt.body, station_path_str) {
                    tracing::warn!(path = station_path_str, "iwd station removed — rewatching");
                    station_mutable.set(None);
                    networks_mutable.set(Vec::new());
                    prompts_mutable.set(None);
                    adapter_mutable.set(None);
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    return;
                }
                refresh_networks(station_path_str, station_mutable, networks_mutable).await;
            }
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
