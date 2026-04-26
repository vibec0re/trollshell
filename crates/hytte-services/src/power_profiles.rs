//! Power profiles via `power-profiles-daemon`.
//!
//! Subscribes to `net.hadess.PowerProfiles` (canonical) on the system
//! bus with fallback to `org.freedesktop.UPower.PowerProfiles` (the
//! freedesktop alias newer builds also expose). Emits a flat
//! [`PowerProfilesState`] every time the daemon's `ActiveProfile` or
//! `Profiles` properties change.
//!
//! When neither name is on the bus the listen loop emits the default
//! (empty) state and re-probes every 30s. UI hides itself when
//! `available.is_empty()`.

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_reactive::{registry, runtime, Service};
use std::collections::HashMap;
use std::time::Duration;
use zbus::zvariant::OwnedValue;
use zbus::Connection;

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

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = PowerProfilesHandles::default();
        let writer = handles.state.clone();
        rt.spawn(async move {
            loop {
                match listen(&writer).await {
                    Ok(()) => tracing::debug!("power_profiles listen ended, retrying in 5s"),
                    Err(e) => tracing::warn!(error = %e, "power_profiles error, retrying in 5s"),
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
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
        if let Err(e) = do_set_active(&profile).await {
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
const FREEDESKTOP_NAME: &str = "org.freedesktop.UPower.PowerProfiles";
const FREEDESKTOP_PATH: &str = "/org/freedesktop/UPower/PowerProfiles";

async fn listen(writer: &Mutable<PowerProfilesState>) -> Result<()> {
    let conn = Connection::system().await.context("connect system bus")?;
    let Some(proxy) = build_proxy(&conn).await else {
        writer.set(PowerProfilesState::default());
        tokio::time::sleep(Duration::from_secs(30)).await;
        return Err(anyhow::anyhow!("power-profiles-daemon not on bus"));
    };

    refresh_state(&proxy, writer).await?;

    let mut props = proxy
        .receive_signal("PropertiesChanged")
        .await
        .context("subscribe PropertiesChanged")?;
    while props.next().await.is_some() {
        if let Err(e) = refresh_state(&proxy, writer).await {
            tracing::warn!(error = %e, "power_profiles refresh failed");
        }
    }
    Ok(())
}

async fn build_proxy(conn: &Connection) -> Option<zbus::Proxy<'static>> {
    if let Ok(p) = zbus::Proxy::new(conn, CANONICAL_NAME, CANONICAL_PATH, CANONICAL_NAME).await
        && probe(&p).await
    {
        return Some(p);
    }
    if let Ok(p) =
        zbus::Proxy::new(conn, FREEDESKTOP_NAME, FREEDESKTOP_PATH, FREEDESKTOP_NAME).await
        && probe(&p).await
    {
        return Some(p);
    }
    None
}

async fn probe(p: &zbus::Proxy<'_>) -> bool {
    p.get_property::<String>("ActiveProfile").await.is_ok()
}

async fn refresh_state(
    proxy: &zbus::Proxy<'_>,
    writer: &Mutable<PowerProfilesState>,
) -> Result<()> {
    let active: String = proxy.get_property("ActiveProfile").await.unwrap_or_default();

    let raw: Vec<HashMap<String, OwnedValue>> = proxy
        .get_property("Profiles")
        .await
        .unwrap_or_default();
    let available: Vec<String> = raw
        .into_iter()
        .filter_map(|m| {
            m.get("Profile")
                .and_then(|v| v.try_clone().ok())
                .and_then(|v| String::try_from(v).ok())
        })
        .collect();

    writer.set(PowerProfilesState { active, available });
    Ok(())
}

// ── Command channel with reconnect-on-IO ─────────────────────────────────────

static CMD_CONN: tokio::sync::Mutex<Option<Connection>> = tokio::sync::Mutex::const_new(None);

async fn cmd_conn() -> Result<Connection> {
    let mut guard = CMD_CONN.lock().await;
    if guard.is_none() {
        let fresh = Connection::system()
            .await
            .context("open shared power_profiles command connection")?;
        *guard = Some(fresh);
    }
    Ok(guard
        .as_ref()
        .expect("just stored Some")
        .clone())
}

async fn evict_cmd_conn() {
    *CMD_CONN.lock().await = None;
}

fn is_io_error(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<zbus::Error>()
            .is_some_and(|ze| matches!(ze, zbus::Error::InputOutput(_)))
    })
}

async fn do_set_active(profile: &str) -> Result<()> {
    if try_set_active_at(CANONICAL_NAME, CANONICAL_PATH, profile)
        .await
        .is_ok()
    {
        return Ok(());
    }
    try_set_active_at(FREEDESKTOP_NAME, FREEDESKTOP_PATH, profile).await
}

async fn try_set_active_at(name: &str, path: &str, profile: &str) -> Result<()> {
    let conn = cmd_conn().await?;
    let r = conn
        .call_method(
            Some(name),
            path,
            Some("org.freedesktop.DBus.Properties"),
            "Set",
            &(name, "ActiveProfile", zbus::zvariant::Value::from(profile)),
        )
        .await
        .with_context(|| format!("call Properties.Set ActiveProfile on {name}"));
    if let Err(ref e) = r && is_io_error(e) {
        evict_cmd_conn().await;
    }
    r.map(|_| ())
}

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
