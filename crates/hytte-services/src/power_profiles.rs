//! Power profiles via `power-profiles-daemon`.
//!
//! Subscribes to `net.hadess.PowerProfiles` on the system bus. Emits a flat
//! [`PowerProfilesState`] every time `ActiveProfile` or `Profiles`
//! properties change.
//!
//! When power-profiles-daemon is not on the bus, both properties stay in
//! `Loading` and the emitted state is the default (empty `available`).
//! UI hides itself when `available.is_empty()`.

use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_bus::{BusKind, PropState, call, property};
use hytte_reactive::{Service, registry, runtime, spawn_supervised};
use std::collections::HashMap;
use zbus::zvariant::{OwnedValue, Value};

#[derive(Clone, Debug, Default)]
pub struct PowerProfilesState {
    pub active: String,
    pub available: Vec<String>,
}

#[doc(hidden)]
pub struct PowerProfilesHandles {
    pub(crate) state: Mutable<PowerProfilesState>,
}

impl Default for PowerProfilesHandles {
    fn default() -> Self {
        Self {
            state: Mutable::new(PowerProfilesState::default()),
        }
    }
}

pub struct PowerProfilesService;

impl Service for PowerProfilesService {
    type Handles = PowerProfilesHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = PowerProfilesHandles::default();
        let writer = handles.state.clone();

        // Two parallel property subscriptions; we coalesce them into the
        // emitted PowerProfilesState by holding the last-known value of
        // each side and re-publishing on every change.
        let active_signal = property::<String>(BusKind::System, CANONICAL_NAME)
            .at_path(CANONICAL_PATH)
            .iface(CANONICAL_NAME)
            .name("ActiveProfile")
            .start();

        let profiles_signal =
            property::<Vec<HashMap<String, OwnedValue>>>(BusKind::System, CANONICAL_NAME)
                .at_path(CANONICAL_PATH)
                .iface(CANONICAL_NAME)
                .name("Profiles")
                .start();

        let active_writer = writer.clone();
        let profiles_writer = writer.clone();

        spawn_supervised("power_profiles", move || {
            let active_signal = active_signal.clone();
            let active_writer = active_writer.clone();
            async move {
                active_signal
                    .signal()
                    .for_each(move |state| {
                        let active = match state {
                            PropState::Loaded(v) | PropState::Stale(v) => v,
                            PropState::Loading => String::new(),
                        };
                        active_writer.lock_mut().active = active;
                        std::future::ready(())
                    })
                    .await;
            }
        });

        spawn_supervised("power_profiles", move || {
            let profiles_signal = profiles_signal.clone();
            let profiles_writer = profiles_writer.clone();
            async move {
                profiles_signal
                    .signal()
                    .for_each(move |state| {
                        let raw = match state {
                            PropState::Loaded(v) | PropState::Stale(v) => v,
                            PropState::Loading => Vec::new(),
                        };
                        let available: Vec<String> = raw
                            .into_iter()
                            .filter_map(|m| {
                                m.get("Profile")
                                    .and_then(|v| v.try_clone().ok())
                                    .and_then(|v| String::try_from(v).ok())
                            })
                            .collect();
                        profiles_writer.lock_mut().available = available;
                        std::future::ready(())
                    })
                    .await;
            }
        });

        handles
    }
}

#[must_use]
pub fn service() -> PowerProfilesService {
    PowerProfilesService
}

pub fn state() -> impl Signal<Item = PowerProfilesState> {
    registry::with(|r| {
        r.get::<PowerProfilesHandles>()
            .expect("power_profiles::service() not registered")
            .state
            .signal_cloned()
    })
}

pub fn set_active(profile: &str) {
    let profile = profile.to_string();
    runtime::handle().spawn(async move {
        let value = Value::from(profile.as_str()).try_to_owned().ok();
        let Some(value) = value else {
            tracing::warn!(profile, "power_profiles set_active: failed to wrap Value");
            return;
        };
        let result = call(BusKind::System, CANONICAL_NAME)
            .at_path(CANONICAL_PATH)
            .iface("org.freedesktop.DBus.Properties")
            .method("Set")
            .args((CANONICAL_NAME, "ActiveProfile", value))
            .send::<()>()
            .await;
        if let Err(e) = result {
            tracing::warn!(error = %e, profile, "power_profiles set_active failed");
        }
    });
}

#[must_use]
pub fn humanize_profile(name: &str) -> String {
    match name {
        "performance" => "Performance".to_string(),
        "balanced" => "Balanced".to_string(),
        "power-saver" => "Power saver".to_string(),
        other => other.to_string(),
    }
}

const CANONICAL_NAME: &str = "net.hadess.PowerProfiles";
const CANONICAL_PATH: &str = "/net/hadess/PowerProfiles";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_known_profiles() {
        assert_eq!(humanize_profile("performance"), "Performance");
        assert_eq!(humanize_profile("balanced"), "Balanced");
        assert_eq!(humanize_profile("power-saver"), "Power saver");
    }

    #[test]
    fn humanize_unknown_profile_passes_through() {
        assert_eq!(humanize_profile("custom-fast"), "custom-fast");
    }
}
