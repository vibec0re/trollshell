//! Primitive #4 — cached property reads with `PropertiesChanged` tracking.
//!
//! See spec section 3.4.

use crate::connection::SharedConnection;
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use zbus::zvariant::{OwnedValue, Value};

/// Three states of a tracked property.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropState<T> {
    /// Initial, before the first Get completes.
    Loading,
    /// Current authoritative value.
    Loaded(T),
    /// Last known value while the bus is reconnecting or the property is
    /// momentarily unavailable. UI can render this differently from
    /// `Loaded` (e.g. a dimmed CSS class).
    Stale(T),
}

/// Builder for a tracked D-Bus property.
pub struct PropertyBuilder<'a, T> {
    shared: &'a SharedConnection,
    destination: String,
    path: String,
    iface: String,
    name: String,
    _t: std::marker::PhantomData<T>,
}

/// Create a property-tracking builder for a remote D-Bus property.
///
/// Returns a [`futures_signals::signal::Signal`] that emits [`PropState`]
/// transitions as the property value changes.
///
/// # Example
/// ```ignore
/// let sig = property_with::<u32>(&shared, "org.example.Counter")
///     .at_path("/org/example/Counter")
///     .iface("org.example.Counter")
///     .name("Value")
///     .start();
/// ```
#[doc(hidden)]
#[must_use]
pub fn property_with<'a, T>(
    shared: &'a SharedConnection,
    destination: impl Into<String>,
) -> PropertyBuilder<'a, T>
where
    T: Clone
        + Send
        + 'static
        + TryFrom<OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<Value<'v>, Error = zbus::zvariant::Error>,
{
    PropertyBuilder {
        shared,
        destination: destination.into(),
        path: String::new(),
        iface: String::new(),
        name: String::new(),
        _t: std::marker::PhantomData,
    }
}

impl<T> PropertyBuilder<'_, T>
where
    T: Clone
        + Send
        + Sync
        + 'static
        + TryFrom<OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<Value<'v>, Error = zbus::zvariant::Error>,
{
    /// Set the object path.
    #[must_use]
    pub fn at_path(mut self, p: impl Into<String>) -> Self {
        self.path = p.into();
        self
    }

    /// Set the D-Bus interface name.
    #[must_use]
    pub fn iface(mut self, i: impl Into<String>) -> Self {
        self.iface = i.into();
        self
    }

    /// Set the property member name (case-sensitive, typically `PascalCase`).
    #[must_use]
    pub fn name(mut self, n: impl Into<String>) -> Self {
        self.name = n.into();
        self
    }

    /// Spawn the tracking task. Returns a signal whose value transitions
    /// through [`PropState::Loading`] → [`PropState::Loaded`] and
    /// [`PropState::Stale`] on reconnects.
    pub fn start(self) -> impl Signal<Item = PropState<T>> {
        let state: Mutable<PropState<T>> = Mutable::new(PropState::Loading);
        let writer = state.clone();
        let shared = self.shared.clone();
        let dest = self.destination;
        let path = self.path;
        let iface = self.iface;
        let name = self.name;
        hytte_reactive::runtime::handle().spawn(async move {
            run_property::<T>(shared, dest, path, iface, name, writer).await;
        });
        state.signal_cloned()
    }
}

// ── Context struct ────────────────────────────────────────────────────────────

struct PropCtx {
    shared: SharedConnection,
    dest: String,
    path: String,
    iface: String,
    name: String,
}

// ── Core property-tracking loop ──────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
async fn run_property<T>(
    shared: SharedConnection,
    dest: String,
    path: String,
    iface: String,
    name: String,
    writer: Mutable<PropState<T>>,
) where
    T: Clone
        + Send
        + Sync
        + 'static
        + TryFrom<OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<Value<'v>, Error = zbus::zvariant::Error>,
{
    let ctx = PropCtx { shared, dest, path, iface, name };
    let mut last: Option<T> = None;

    loop {
        // Mark state: Stale if we have a prior value, Loading otherwise.
        match &last {
            Some(v) => writer.set(PropState::Stale(v.clone())),
            None => writer.set(PropState::Loading),
        }

        // ── Cold Get ────────────────────────────────────────────────────────

        let get_result = ctx
            .shared
            .with_conn(|conn| {
                let dest = ctx.dest.clone();
                let path = ctx.path.clone();
                let iface = ctx.iface.clone();
                let name = ctx.name.clone();
                async move {
                    let props = zbus::fdo::PropertiesProxy::builder(&conn)
                        .destination(dest.as_str())?
                        .path(path.as_str())?
                        .build()
                        .await?;
                    let raw: OwnedValue = props
                        .get(iface.as_str().try_into()?, name.as_str())
                        .await
                        .map_err(zbus::Error::from)?;
                    let typed: T = raw
                        .try_into()
                        .map_err(|e: zbus::zvariant::Error| {
                            zbus::Error::Failure(e.to_string())
                        })?;
                    Ok(typed)
                }
            })
            .await;

        let initial = match get_result {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    dest = ctx.dest,
                    path = ctx.path,
                    iface = ctx.iface,
                    name = ctx.name,
                    "property Get failed; will retry"
                );
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
        };

        last = Some(initial.clone());
        writer.set(PropState::Loaded(initial));

        // ── PropertiesChanged listener ────────────────────────────────────

        // Grab the current connection to subscribe to `PropertiesChanged`.
        let conn_result = ctx
            .shared
            .with_conn(|conn| async move { Ok(conn) })
            .await;

        let conn = match conn_result {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    dest = ctx.dest,
                    "property: failed to get connection for PropertiesChanged; will retry"
                );
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
        };

        // Build `PropertiesProxy` and subscribe to `PropertiesChanged`.
        let subscribe_result = async {
            let props = zbus::fdo::PropertiesProxy::builder(&conn)
                .destination(ctx.dest.as_str())?
                .path(ctx.path.as_str())?
                .build()
                .await?;
            props.receive_properties_changed().await
        }
        .await;

        let mut changes = match subscribe_result {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    dest = ctx.dest,
                    "property: PropertiesChanged subscribe failed; will retry"
                );
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
        };

        // Drain the stream until it ends or an invalidation triggers a re-Get.
        while let Some(sig) = changes.next().await {
            let Ok(args) = sig.args() else { continue };

            if args.interface_name != ctx.iface.as_str() {
                continue;
            }

            // Check for an inline value in `changed_properties`.
            if let Some(raw) = args.changed_properties.get(ctx.name.as_str()) {
                let decode: Result<T, _> = T::try_from(raw.clone());
                match decode {
                    Ok(typed) => {
                        last = Some(typed.clone());
                        writer.set(PropState::Loaded(typed));
                    }
                    Err(e) => {
                        tracing::debug!(
                            error = %e,
                            name = ctx.name,
                            "PropertiesChanged: failed to decode value"
                        );
                    }
                }
            }

            // Invalidation: break to trigger a re-Get on the next loop iteration.
            if args
                .invalidated_properties
                .contains(&ctx.name.as_str())
            {
                break;
            }
        }

        // Stream ended (bus disconnect or invalidation) — loop to re-Get.
        // Brief pause before re-subscribing to avoid a tight loop.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
