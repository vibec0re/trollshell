//! Primitive #6 — export a D-Bus interface at an object path **without** owning
//! a well-known name.
//!
//! Some D-Bus daemons (notably `NetworkManager`'s `AgentManager`) record the
//! *caller's unique connection name* when an agent registers and call back on
//! that unique name — they never look up a well-known name. For such agents,
//! owning a name (and the system-bus policy entry that would require) is both
//! unnecessary and slightly wrong: the agent just needs its object reachable on
//! the same shared connection the rest of the service already uses.
//!
//! [`own`](crate::own) couples object export to name ownership; this primitive
//! is the name-less counterpart. It mounts the interface on the current shared
//! connection and re-mounts it on every reconnect (tracking the connection
//! epoch), exactly like `own_name`'s mount-on-reconnect behaviour, so the agent
//! survives a bus blip.

use crate::connection::SharedConnection;
use futures_signals::signal::SignalExt as _;
use futures_util::StreamExt as _;
use std::sync::Arc;
use std::time::Duration;
use zbus::object_server::Interface;

/// A handle keeping an exported object alive. Dropping the last clone stops the
/// re-mount supervisor task. The object stays mounted on the live connection
/// until that connection is replaced, which is fine for process-lifetime agents
/// — hold this for as long as the agent should be served.
#[derive(Clone)]
pub struct ExportHandle {
    _keep_alive: Arc<()>,
}

/// Builder for [`export_object`](crate::export_object).
pub struct ExportBuilder {
    shared: SharedConnection,
    path: String,
}

/// Internal entry point taking a `SharedConnection` directly. Production callers
/// use [`export_object`](crate::export_object).
#[doc(hidden)]
#[must_use]
pub fn export_object_with(shared: &SharedConnection, path: impl Into<String>) -> ExportBuilder {
    ExportBuilder {
        shared: shared.clone(),
        path: path.into(),
    }
}

impl ExportBuilder {
    /// Override which bus to export on. The default is determined by the
    /// constructor: [`export_object`](crate::export_object) uses the system bus.
    #[must_use]
    pub fn bus(self, kind: crate::BusKind) -> ExportBuilder {
        ExportBuilder {
            shared: match kind {
                crate::BusKind::Session => crate::connection::session().clone(),
                crate::BusKind::System => crate::connection::system().clone(),
            },
            path: self.path,
        }
    }

    /// Mount `iface` at the configured object path and keep it mounted across
    /// reconnects. Returns an [`ExportHandle`]; hold it for as long as the
    /// object should be served. Spawns a supervisor task on the hytte runtime
    /// that re-mounts the interface whenever the connection epoch advances
    /// (i.e. after the supervisor re-establishes a dropped connection).
    ///
    /// `iface` must be `Clone` because the object server takes ownership on each
    /// mount; the clone is used when the connection is re-established.
    #[must_use]
    pub fn start<I>(self, iface: I) -> ExportHandle
    where
        I: Interface + Clone + Send + Sync + 'static,
    {
        let keep_alive = Arc::new(());
        let weak = Arc::downgrade(&keep_alive);
        let shared = self.shared;
        let path = self.path;

        hytte_reactive::runtime::handle().spawn(async move {
            let mut epoch_stream = shared.epoch_signal().to_stream();
            let mut last_epoch = u64::MAX;
            loop {
                // Stop once the caller has dropped every ExportHandle clone.
                if weak.upgrade().is_none() {
                    return;
                }

                let current = shared.epoch();
                if current != last_epoch {
                    let iface_clone = iface.clone();
                    let path_clone = path.clone();
                    let mounted = shared
                        .with_conn(|conn| async move {
                            // `at()` → Ok(true) on first mount, Ok(false) if an
                            // interface is already registered at this path. Both
                            // are success: a re-mount that finds the iface still
                            // present from a prior epoch is a no-op.
                            let _ = conn
                                .object_server()
                                .at(path_clone.as_str(), iface_clone)
                                .await?;
                            Ok(())
                        })
                        .await;
                    match mounted {
                        Ok(()) => {
                            tracing::debug!(path = %path, epoch = current, "exported object mounted");
                            last_epoch = current;
                        }
                        Err(e) => {
                            tracing::debug!(path = %path, error = %e, "export mount failed; retrying");
                            tokio::time::sleep(Duration::from_millis(250)).await;
                            continue;
                        }
                    }
                }

                // Wait for the next epoch change (reconnect) or a short timeout
                // so a dropped handle is noticed promptly.
                let _ = tokio::time::timeout(Duration::from_secs(30), epoch_stream.next()).await;
            }
        });

        ExportHandle {
            _keep_alive: keep_alive,
        }
    }
}
