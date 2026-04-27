//! systemd service — surfaces the current set of failed units via
//! `org.freedesktop.systemd1.Manager`. Signal-driven: subscribes to
//! `JobRemoved` and re-fetches `ListUnitsFiltered(["failed"])` on
//! each emission.
//!
//! Notes on systemd dbus:
//! - Uses the **system bus** (`org.freedesktop.systemd1` on the system bus
//!   is the system manager; `systemd --user` exposes the same name on the
//!   session bus but this service monitors the system manager).
//! - `Manager.Subscribe()` MUST be called for the daemon to start
//!   emitting signals to this client. Without it `JobRemoved` never
//!   fires.
//! - `JobRemoved` covers every unit transition (start/stop/restart
//!   complete) regardless of result, so it's a reasonable proxy for
//!   "the failed-unit set may have changed". Cheaper than per-unit
//!   `PropertiesChanged` subscriptions for the v0.2.5 fidelity.
//!
//! All D-Bus I/O goes through [`hytte_bus::call`] and [`hytte_bus::signals`]
//! so the shared connection supervisor handles reconnects automatically.
//!
//! # Public API
//!
//! ```ignore
//! .with(systemd::service())
//!
//! systemd::failed_units() -> impl Signal<Item = Vec<FailedUnit>>
//! ```

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_bus::{call, signals, BusKind};
use hytte_reactive::{registry, Service};
use std::time::Duration;

const SYSTEMD_NAME: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_IFACE: &str = "org.freedesktop.systemd1.Manager";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedUnit {
    pub name: String,
    pub description: String,
    pub sub_state: String,
}

#[doc(hidden)]
pub struct SystemdHandles {
    pub(crate) failed_units: Mutable<Vec<FailedUnit>>,
}

impl Default for SystemdHandles {
    fn default() -> Self {
        Self {
            failed_units: Mutable::new(Vec::new()),
        }
    }
}

pub struct SystemdService;

impl Service for SystemdService {
    type Handles = SystemdHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = SystemdHandles::default();
        let writer = handles.failed_units.clone();

        rt.spawn(async move {
            loop {
                match listen(&writer).await {
                    Ok(()) => tracing::warn!("systemd listen loop ended, retrying in 5s"),
                    Err(e) => tracing::warn!(error = %e, "systemd listen error, retrying in 5s"),
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        handles
    }
}

#[must_use]
pub fn service() -> SystemdService {
    SystemdService
}

pub fn failed_units() -> impl Signal<Item = Vec<FailedUnit>> {
    registry::with(|r| {
        r.get::<SystemdHandles>()
            .expect("systemd::service() not registered")
            .failed_units
            .signal_cloned()
    })
}

// ── Listen loop ───────────────────────────────────────────────────────────────

/// systemd `ListUnitsFiltered` reply tuple shape:
/// (`name`, `description`, `load_state`, `active_state`, `sub_state`, `follower`,
///  `object_path`, `job_id`, `job_type`, `job_object_path`).
type UnitTuple = (
    String,
    String,
    String,
    String,
    String,
    String,
    zbus::zvariant::OwnedObjectPath,
    u32,
    String,
    zbus::zvariant::OwnedObjectPath,
);

async fn listen(writer: &Mutable<Vec<FailedUnit>>) -> Result<()> {
    // REQUIRED: systemd only emits signals to clients that have called
    // Subscribe(). Without this, JobRemoved never fires.
    call(SYSTEMD_NAME)
        .bus(BusKind::System)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .method("Subscribe")
        .args(())
        .send::<()>()
        .await
        .context("Manager.Subscribe")?;

    // Initial fetch of failed units.
    refresh_failed(writer).await?;

    // Subscribe to JobRemoved so we re-fetch whenever a job completes
    // (which may change the failed-unit set).
    let job_removed = signals(SYSTEMD_NAME)
        .bus(BusKind::System)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .signal("JobRemoved")
        .start();

    let mut events = job_removed.events();

    while events.next().await.is_some() {
        if let Err(e) = refresh_failed(writer).await {
            tracing::warn!(error = %e, "systemd refresh after JobRemoved failed");
        }
    }
    Ok(())
}

async fn refresh_failed(writer: &Mutable<Vec<FailedUnit>>) -> Result<()> {
    let units: Vec<UnitTuple> = call(SYSTEMD_NAME)
        .bus(BusKind::System)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .method("ListUnitsFiltered")
        .args((vec!["failed".to_string()],))
        .send()
        .await
        .context("ListUnitsFiltered")?;

    writer.set(parse_units(units));
    Ok(())
}

pub(crate) fn parse_units(units: Vec<UnitTuple>) -> Vec<FailedUnit> {
    let mut out: Vec<FailedUnit> = units
        .into_iter()
        .map(|(name, description, _load, _active, sub_state, ..)| FailedUnit {
            name,
            description,
            sub_state,
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(s: &str) -> zbus::zvariant::OwnedObjectPath {
        zbus::zvariant::ObjectPath::try_from(s).unwrap().into()
    }

    fn unit(name: &str, desc: &str, sub: &str) -> UnitTuple {
        (
            name.to_string(),
            desc.to_string(),
            "loaded".to_string(),
            "failed".to_string(),
            sub.to_string(),
            String::new(),
            op("/org/freedesktop/systemd1/unit/dummy"),
            0,
            String::new(),
            op("/"),
        )
    }

    #[test]
    fn parse_units_extracts_name_description_sub_state() {
        let input = vec![unit("polkit.service", "Authorization Manager", "failed")];
        let out = parse_units(input);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "polkit.service");
        assert_eq!(out[0].description, "Authorization Manager");
        assert_eq!(out[0].sub_state, "failed");
    }

    #[test]
    fn parse_units_sorts_by_name() {
        let input = vec![
            unit("zzz.service", "z", "failed"),
            unit("aaa.service", "a", "failed"),
            unit("mmm.service", "m", "failed"),
        ];
        let out = parse_units(input);
        let names: Vec<&str> = out.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, vec!["aaa.service", "mmm.service", "zzz.service"]);
    }

    #[test]
    fn parse_units_empty_input_yields_empty_output() {
        let out = parse_units(Vec::new());
        assert!(out.is_empty());
    }
}
