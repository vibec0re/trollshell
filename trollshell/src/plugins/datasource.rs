//! Host-side datasource query router (#509).
//!
//! The generic datasource protocol is **host-routed**: a requester plugin never
//! dials a provider plugin. It emits an
//! [`Effect::DatasourceQuery`](hytte_plugin_proto::Effect::DatasourceQuery); the
//! host validates it, forwards it to the providing connection as
//! [`HostMsg::DatasourceQuery`], receives the provider's
//! [`Effect::DatasourceResult`], and routes the answer back to the requester as
//! [`HostMsg::DatasourceResult`]. The host stays the single policy chokepoint —
//! consistent with the capability enforcement (#436) and the audit log (#510)
//! every effect already flows through.
//!
//! This mirrors the [`Effect::RunCommand`](hytte_plugin_proto::Effect::RunCommand)
//! round-trip (#544): the GTK-thread broker ([`super::effects::broker_effect`])
//! calls into here, and the actual async send + timeout are offloaded to the
//! tokio runtime. The difference is that a datasource reply routes to a **different
//! connection** (the provider, then back to the requester), so the router is a
//! cross-thread shared registry rather than a per-connection `outbound`.
//!
//! ## Correlation
//!
//! The requester's `request_id` is its own token, echoed back to it verbatim. But
//! the provider must not see (or be able to collide on) another plugin's id-space,
//! so the host mints an **opaque host correlation** for each in-flight query and
//! forwards *that* to the provider. The provider echoes it in its
//! [`Effect::DatasourceResult`]; the host maps it back to the parked requester +
//! original `request_id`. Provider and requester id-spaces never touch.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hytte_plugin_proto::{DatasourceError, DatasourceOutcome, HostMsg};
use tokio::sync::mpsc;

/// How long the host waits for a provider to answer a forwarded query before it
/// synthesizes a [`DatasourceError::Timeout`] result to the requester (#509). Matches
/// the [`RunCommand`](super::effects) bound — a wedged provider never leaves a
/// requester hanging. Shortened under test so the timeout path runs fast.
#[cfg(not(test))]
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const QUERY_TIMEOUT: Duration = Duration::from_millis(500);

/// A connected datasource provider (#509): the outbound channel to its connection,
/// the scopes it declared for this datasource, and the connection generation — so
/// teardown removes only an entry this same connection still owns (the #278
/// generation guard the region mailboxes use).
struct ProviderEntry {
    outbound: mpsc::Sender<HostMsg>,
    scopes: Vec<String>,
    generation: u64,
}

/// A datasource query parked awaiting its provider's answer (#509): the requester's
/// outbound channel and the requester's own `request_id` — the host translates
/// between this and the opaque host correlation it forwarded to the provider.
struct PendingQuery {
    requester: mpsc::Sender<HostMsg>,
    request_id: u64,
}

/// The cross-thread datasource query router (#509). One per host, shared (as an
/// `Arc<Mutex<…>>` pair, like `live_ids`/`runtime`) between the tokio session tasks
/// — which register/unregister providers as connections come and go — and the
/// GTK-thread effect broker, which routes queries and results. The maps are only
/// ever touched briefly (never across an await), so a plain `Mutex` is right.
#[derive(Clone, Default)]
pub(super) struct DatasourceRouter {
    /// Datasource id → its live provider.
    providers: Arc<Mutex<BTreeMap<String, ProviderEntry>>>,
    /// Host correlation → the parked requester.
    pending: Arc<Mutex<HashMap<u64, PendingQuery>>>,
    /// Monotonic host correlation counter.
    next_corr: Arc<AtomicU64>,
}

impl DatasourceRouter {
    /// Register `id` as served by a connection (its `outbound`, declared `scopes`,
    /// and `generation`). Latest-wins on a duplicate id from a *different* provider
    /// (logged) — teardown's generation guard keeps a stale provider from evicting a
    /// live successor.
    pub(super) fn register_provider(
        &self,
        id: &str,
        scopes: Vec<String>,
        outbound: mpsc::Sender<HostMsg>,
        generation: u64,
    ) {
        let prev = self
            .providers
            .lock()
            .expect("datasource providers poisoned")
            .insert(
                id.to_owned(),
                ProviderEntry {
                    outbound,
                    scopes,
                    generation,
                },
            );
        if prev.is_some() {
            tracing::warn!(datasource = %id, "datasource id already had a provider; replacing (latest-wins)");
        } else {
            tracing::info!(datasource = %id, "datasource provider registered");
        }
    }

    /// Remove `id`'s provider iff it is still the one this `generation` registered
    /// (teardown). A fast-reconnect successor with a newer generation is never
    /// evicted — the same guard `clear_region_if_owned` applies to region cards.
    pub(super) fn unregister_provider(&self, id: &str, generation: u64) {
        let mut providers = self
            .providers
            .lock()
            .expect("datasource providers poisoned");
        if providers
            .get(id)
            .is_some_and(|e| e.generation == generation)
        {
            providers.remove(id);
            tracing::info!(datasource = %id, "datasource provider unregistered");
        }
    }

    /// Route a requester's [`Effect::DatasourceQuery`](hytte_plugin_proto::Effect::DatasourceQuery)
    /// (#509). Runs on the GTK broker thread; the lookup + async forward + timeout
    /// are offloaded to the tokio runtime. Synthesizes a
    /// [`DatasourceError::NotFound`] / [`DatasourceError::ScopeDenied`] result to the
    /// requester for a routing failure, or parks the query (keyed by a fresh host
    /// correlation) and forwards it to the provider. The requester never hangs: a
    /// wedged provider is caught by the [`QUERY_TIMEOUT`] arm.
    pub(super) fn route_query(
        &self,
        requester_id: String,
        request_id: u64,
        provider: String,
        scope: String,
        params: String,
        requester: mpsc::Sender<HostMsg>,
    ) {
        let providers = self.providers.clone();
        let pending = self.pending.clone();
        let next_corr = self.next_corr.clone();
        hytte::reactive::runtime::handle().spawn(async move {
            // Look up the provider's outbound + scopes, cloning out of the lock so
            // nothing is held across an await.
            let found = {
                let guard = providers.lock().expect("datasource providers poisoned");
                guard
                    .get(&provider)
                    .map(|e| (e.outbound.clone(), e.scopes.clone()))
            };
            let Some((provider_out, scopes)) = found else {
                tracing::info!(
                    requester = %requester_id, %provider, %scope,
                    "datasource query for an unprovided datasource; NotFound",
                );
                fail(
                    &requester,
                    request_id,
                    DatasourceError::NotFound,
                    format!("no connected provider for datasource '{provider}'"),
                )
                .await;
                return;
            };
            if !scopes.iter().any(|s| s == &scope) {
                tracing::info!(
                    requester = %requester_id, %provider, %scope,
                    "datasource query for a scope the provider does not serve; ScopeDenied",
                );
                fail(
                    &requester,
                    request_id,
                    DatasourceError::ScopeDenied,
                    format!("provider '{provider}' does not serve scope '{scope}'"),
                )
                .await;
                return;
            }
            let corr = next_corr.fetch_add(1, Ordering::Relaxed);
            pending.lock().expect("datasource pending poisoned").insert(
                corr,
                PendingQuery {
                    requester: requester.clone(),
                    request_id,
                },
            );
            tracing::info!(
                requester = %requester_id, %provider, %scope, corr, request_id,
                "datasource query routed to provider",
            );
            // Forward to the provider under the opaque host correlation. A closed
            // outbound means the provider is tearing down — synthesize NotFound.
            if provider_out
                .send(HostMsg::DatasourceQuery {
                    request_id: corr,
                    datasource: provider.clone(),
                    scope,
                    params,
                })
                .await
                .is_err()
            {
                pending
                    .lock()
                    .expect("datasource pending poisoned")
                    .remove(&corr);
                fail(
                    &requester,
                    request_id,
                    DatasourceError::NotFound,
                    format!("provider '{provider}' disconnected before answering"),
                )
                .await;
                return;
            }
            // Arm the timeout: if the correlation is still parked after the bound,
            // the provider never answered — reap it and fail the requester.
            let pending_to = pending.clone();
            hytte::reactive::runtime::handle().spawn(async move {
                tokio::time::sleep(QUERY_TIMEOUT).await;
                let taken = pending_to
                    .lock()
                    .expect("datasource pending poisoned")
                    .remove(&corr);
                if let Some(p) = taken {
                    tracing::warn!(
                        corr,
                        request_id = p.request_id,
                        "datasource query timed out; no provider answer"
                    );
                    fail(
                        &p.requester,
                        p.request_id,
                        DatasourceError::Timeout,
                        "provider did not answer within the query timeout".to_owned(),
                    )
                    .await;
                }
            });
        });
    }

    /// Route a provider's [`Effect::DatasourceResult`](hytte_plugin_proto::Effect::DatasourceResult)
    /// (#509) back to the parked requester, keyed by the opaque host correlation the
    /// provider echoed. A correlation with no parked entry (already timed out, or a
    /// bogus/duplicate echo) is logged and dropped. Runs on the GTK broker thread;
    /// the async send is offloaded to the runtime.
    pub(super) fn deliver_result(&self, corr: u64, outcome: DatasourceOutcome) {
        let pending = self.pending.clone();
        hytte::reactive::runtime::handle().spawn(async move {
            let taken = pending
                .lock()
                .expect("datasource pending poisoned")
                .remove(&corr);
            if let Some(p) = taken {
                let _ = p
                    .requester
                    .send(HostMsg::DatasourceResult {
                        request_id: p.request_id,
                        outcome,
                    })
                    .await;
            } else {
                tracing::debug!(
                    corr,
                    "datasource result for an unknown/expired query; dropped"
                );
            }
        });
    }
}

/// Send a synthesized [`DatasourceOutcome::Failed`] result back to a requester.
async fn fail(
    requester: &mpsc::Sender<HostMsg>,
    request_id: u64,
    error: DatasourceError,
    message: String,
) {
    let _ = requester
        .send(HostMsg::DatasourceResult {
            request_id,
            outcome: DatasourceOutcome::Failed { error, message },
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chan() -> mpsc::Sender<HostMsg> {
        mpsc::channel(8).0
    }

    #[test]
    fn register_then_lookup_and_scope_check() {
        let r = DatasourceRouter::default();
        r.register_provider("departures", vec!["next".into()], chan(), 1);
        let guard = r.providers.lock().unwrap();
        let entry = guard.get("departures").expect("registered");
        assert!(entry.scopes.iter().any(|s| s == "next"));
        assert!(!entry.scopes.iter().any(|s| s == "history"));
    }

    #[test]
    fn unregister_only_removes_a_generation_match() {
        let r = DatasourceRouter::default();
        r.register_provider("weather", vec!["current".into()], chan(), 5);
        // A stale generation (an already-superseded connection) must not evict it.
        r.unregister_provider("weather", 4);
        assert!(r.providers.lock().unwrap().contains_key("weather"));
        // The owning generation removes it.
        r.unregister_provider("weather", 5);
        assert!(!r.providers.lock().unwrap().contains_key("weather"));
    }

    #[test]
    fn latest_provider_wins_on_a_duplicate_id() {
        let r = DatasourceRouter::default();
        r.register_provider("x", vec!["a".into()], chan(), 1);
        r.register_provider("x", vec!["b".into()], chan(), 2);
        // The newer registration wins; the older generation's teardown is a no-op.
        r.unregister_provider("x", 1);
        let guard = r.providers.lock().unwrap();
        let entry = guard.get("x").expect("newer provider still present");
        assert_eq!(entry.generation, 2);
        assert!(entry.scopes.iter().any(|s| s == "b"));
    }
}
