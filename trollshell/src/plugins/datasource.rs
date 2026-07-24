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

/// A connected datasource provider (#509): the providing connection's plugin id,
/// the outbound channel to its connection, the scopes it declared for this
/// datasource, and the connection generation — so teardown removes only an entry
/// this same connection still owns (the #278 generation guard the region mailboxes
/// use).
struct ProviderEntry {
    /// The providing connection's plugin id (#553): stamped onto every query routed
    /// here (see [`PendingQuery::provider_id`]) so the result leg can verify an
    /// answer came back from *this* provider, not another provider-capable plugin
    /// echoing a guessed correlation.
    plugin_id: String,
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
    /// The plugin id of the provider this query was routed to (#553). Only that
    /// provider may resolve this correlation: a [`DatasourceResult`] echoing the
    /// correlation from any *other* plugin is dropped, not delivered — the host, not
    /// the guessable correlation counter, is the identity authority.
    ///
    /// [`DatasourceResult`]: hytte_plugin_proto::Effect::DatasourceResult
    provider_id: String,
    /// Abort handle for this query's armed timeout task (#553). Aborted the moment
    /// the query resolves, so a successful answer leaves no timer lingering the full
    /// [`QUERY_TIMEOUT`] — the #544 [`tokio::time::timeout`] parity, adapted to the
    /// router's decoupled request/answer legs (the two never share one future to
    /// wrap, so an abort handle stands in for cancelling the timer).
    timeout: tokio::task::AbortHandle,
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
    /// Register datasource `id` as served by a connection (its `plugin_id`,
    /// `outbound`, declared `scopes`, and `generation`). The `plugin_id` is the
    /// answer-leg identity check (#553): only this provider may resolve a query
    /// routed here. Latest-wins on a duplicate id from a *different* provider
    /// (logged) — teardown's generation guard keeps a stale provider from evicting a
    /// live successor.
    pub(super) fn register_provider(
        &self,
        id: &str,
        plugin_id: &str,
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
                    plugin_id: plugin_id.to_owned(),
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
            // Look up the provider's plugin id + outbound + scopes, cloning out of
            // the lock so nothing is held across an await.
            let found = {
                let guard = providers.lock().expect("datasource providers poisoned");
                guard
                    .get(&provider)
                    .map(|e| (e.plugin_id.clone(), e.outbound.clone(), e.scopes.clone()))
            };
            let Some((provider_id, provider_out, scopes)) = found else {
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
            // Arm the timeout *before* parking, so its abort handle can ride in the
            // `PendingQuery` and be cancelled the instant the query resolves (#553) —
            // a successful answer no longer leaves this task sleeping out the full
            // bound. The task sleeps immediately, so it never races the `insert` below.
            let timeout = arm_query_timeout(pending.clone(), corr);
            pending.lock().expect("datasource pending poisoned").insert(
                corr,
                PendingQuery {
                    requester: requester.clone(),
                    request_id,
                    provider_id,
                    timeout,
                },
            );
            tracing::info!(
                requester = %requester_id, %provider, %scope, corr, request_id,
                "datasource query routed to provider",
            );
            // Forward to the provider under the opaque host correlation. A closed
            // outbound means the provider is tearing down — synthesize NotFound (and
            // abort the timeout we just armed, so it doesn't linger on a dead query).
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
                if let Some(p) = pending
                    .lock()
                    .expect("datasource pending poisoned")
                    .remove(&corr)
                {
                    p.timeout.abort();
                }
                fail(
                    &requester,
                    request_id,
                    DatasourceError::NotFound,
                    format!("provider '{provider}' disconnected before answering"),
                )
                .await;
            }
        });
    }

    /// Route a provider's [`Effect::DatasourceResult`](hytte_plugin_proto::Effect::DatasourceResult)
    /// (#509) back to the parked requester, keyed by the opaque host correlation the
    /// provider echoed. `responder_id` is the plugin id of the connection that
    /// emitted the result: only the provider the query was **routed to** may resolve
    /// its correlation (#553). A result whose `responder_id` doesn't match the parked
    /// `provider_id` is a cross-provider forgery — a different provider-capable plugin
    /// echoing a guessed correlation to inject or cancel another plugin's answer — so
    /// it is dropped and audited **without** removing the entry, leaving the genuine
    /// provider's later answer free to resolve it. A correlation with no parked entry
    /// (already timed out, or a bogus/duplicate echo) is likewise logged and dropped.
    /// Runs on the GTK broker thread; the async send is offloaded to the runtime.
    pub(super) fn deliver_result(
        &self,
        corr: u64,
        responder_id: String,
        outcome: DatasourceOutcome,
    ) {
        let pending = self.pending.clone();
        hytte::reactive::runtime::handle().spawn(async move {
            let taken = {
                let mut guard = pending.lock().expect("datasource pending poisoned");
                // Peek the routed-to provider id first (cloned so the borrow is
                // released before the conditional `remove`); resolve only on a match.
                match guard.get(&corr).map(|p| p.provider_id.clone()) {
                    Some(routed_to) if routed_to == responder_id => guard.remove(&corr),
                    Some(routed_to) => {
                        tracing::warn!(
                            corr,
                            responder = %responder_id,
                            routed_to = %routed_to,
                            "datasource result from a plugin that is not the routed-to provider; dropped",
                        );
                        None
                    }
                    None => {
                        tracing::debug!(
                            corr,
                            responder = %responder_id,
                            "datasource result for an unknown/expired query; dropped",
                        );
                        None
                    }
                }
            };
            if let Some(p) = taken {
                // The query resolved — cancel its armed timeout so no timer lingers
                // (#553; a no-op if the timeout already fired and reaped the entry).
                p.timeout.abort();
                let _ = p
                    .requester
                    .send(HostMsg::DatasourceResult {
                        request_id: p.request_id,
                        outcome,
                    })
                    .await;
            }
        });
    }
}

/// Spawn the timeout reaper for a query parked at `corr` and return its abort
/// handle (#553). It sleeps [`QUERY_TIMEOUT`], then — if the correlation is still
/// parked (the provider never answered) — removes it and synthesizes a
/// [`DatasourceError::Timeout`] to the requester. The handle rides in the
/// [`PendingQuery`] so a resolving answer can [`abort`](tokio::task::AbortHandle::abort)
/// it, leaving no timer lingering after success.
fn arm_query_timeout(
    pending: Arc<Mutex<HashMap<u64, PendingQuery>>>,
    corr: u64,
) -> tokio::task::AbortHandle {
    hytte::reactive::runtime::handle()
        .spawn(async move {
            tokio::time::sleep(QUERY_TIMEOUT).await;
            let taken = pending
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
        })
        .abort_handle()
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
        r.register_provider("departures", "dep-plugin", vec!["next".into()], chan(), 1);
        let guard = r.providers.lock().unwrap();
        let entry = guard.get("departures").expect("registered");
        assert_eq!(entry.plugin_id, "dep-plugin");
        assert!(entry.scopes.iter().any(|s| s == "next"));
        assert!(!entry.scopes.iter().any(|s| s == "history"));
    }

    #[test]
    fn unregister_only_removes_a_generation_match() {
        let r = DatasourceRouter::default();
        r.register_provider("weather", "wx-plugin", vec!["current".into()], chan(), 5);
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
        r.register_provider("x", "first", vec!["a".into()], chan(), 1);
        r.register_provider("x", "second", vec!["b".into()], chan(), 2);
        // The newer registration wins; the older generation's teardown is a no-op.
        r.unregister_provider("x", 1);
        let guard = r.providers.lock().unwrap();
        let entry = guard.get("x").expect("newer provider still present");
        assert_eq!(entry.generation, 2);
        assert!(entry.scopes.iter().any(|s| s == "b"));
    }
}
