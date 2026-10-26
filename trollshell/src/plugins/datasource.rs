//! Host-side datasource query router (#509).
//!
//! The generic datasource protocol is **host-routed**: a requester plugin never
//! dials a provider plugin. It emits an
//! [`Effect::DatasourceQuery`](hytte_plugin_proto::Effect::DatasourceQuery); the
//! host validates it, forwards it to the providing connection as
//! [`HostMsg::DatasourceQuery`], receives the provider's
//! [`Effect::DatasourceResult`](hytte_plugin_proto::Effect::DatasourceResult),
//! and routes the answer back to the requester as
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
//! [`Effect::DatasourceResult`](hytte_plugin_proto::Effect::DatasourceResult);
//! the host maps it back to the parked requester +
//! original `request_id`. Provider and requester id-spaces never touch.

use std::collections::btree_map::Entry;
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

/// Who serves one datasource id: the `holder` every query goes to, and the
/// plugins that declared the id while the holder was live, oldest first. Each
/// standby keeps its own channel and generation, so it can serve the moment
/// the holder leaves and its own teardown can take it out of the line.
struct ProviderSlot {
    holder: ProviderEntry,
    standby: Vec<ProviderEntry>,
}

/// What [`DatasourceRouter::register_provider`] did, for its log line.
enum Registered {
    /// The id had no provider; this one serves it.
    Serving,
    /// The same plugin declared the id again; its entry was replaced.
    Replaced,
    /// Another plugin serves the id; this one waits behind it.
    Standby { holder: String },
}

/// What [`DatasourceRouter::unregister_provider`] did, for its log line.
enum Unregistered {
    /// The generation owns nothing for this id: a no-op.
    Nothing,
    /// A standby left before it ever served.
    Standby,
    /// The holder left and nobody was waiting; the id is free.
    Free,
    /// The holder left and its oldest live standby serves now.
    Promoted { left: String, holder: String },
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
    /// Whether the provider's session has already let one answer for this query
    /// past its effect bucket unmetered ([`DatasourceRouter::admit_answer`]). One
    /// per query, so the exemption is bounded by the queries the host forwarded.
    answer_admitted: bool,
}

/// The cross-thread datasource query router (#509). One per host, shared (as an
/// `Arc<Mutex<…>>` pair, like `live_ids`/`runtime`) between the tokio session tasks
/// — which register/unregister providers as connections come and go — and the
/// GTK-thread effect broker, which routes queries and results. The maps are only
/// ever touched briefly (never across an await), so a plain `Mutex` is right.
#[derive(Clone, Default)]
pub(super) struct DatasourceRouter {
    /// Datasource id → its live provider, and the live standbys behind it.
    providers: Arc<Mutex<BTreeMap<String, ProviderSlot>>>,
    /// Host correlation → the parked requester.
    pending: Arc<Mutex<HashMap<u64, PendingQuery>>>,
    /// Monotonic host correlation counter.
    next_corr: Arc<AtomicU64>,
}

impl DatasourceRouter {
    /// Register datasource `id` as served by a connection (its `plugin_id`,
    /// `outbound`, declared `scopes`, and `generation`). The `plugin_id` is the
    /// answer-leg identity check (#553): only this provider may resolve a query
    /// routed here.
    ///
    /// **Another plugin's datasource cannot be taken over.** While a plugin
    /// holds `id`, a registration from a *different* plugin id does not replace
    /// it: the holder keeps serving, and the newcomer joins the id's
    /// **standby** line, with one warning naming both. A requester names a
    /// datasource, not a plugin, so replacing the holder would hand its
    /// queries — and the answers the requester trusts — to whichever plugin
    /// registered last. When the holder's connection tears down
    /// ([`unregister_provider`](Self::unregister_provider)), the oldest live
    /// standby serves from then on; with none waiting, the id is free and the
    /// next plugin to declare it serves it. Every entry here belongs to a live
    /// connection, because teardown is what removes it, standbys included.
    ///
    /// The **same** plugin id replaces its own entry. `IdGuard` allows one live
    /// connection per id and teardown unregisters inside that window, so a
    /// reconnect never finds its old entry still here: the one way to reach
    /// this arm is a manifest that lists the same datasource twice.
    pub(super) fn register_provider(
        &self,
        id: &str,
        plugin_id: &str,
        scopes: Vec<String>,
        outbound: mpsc::Sender<HostMsg>,
        generation: u64,
    ) {
        let entry = ProviderEntry {
            plugin_id: plugin_id.to_owned(),
            outbound,
            scopes,
            generation,
        };
        // Decided under the lock, logged after it.
        let outcome = {
            let mut providers = self
                .providers
                .lock()
                .expect("datasource providers poisoned");
            match providers.entry(id.to_owned()) {
                Entry::Vacant(vacant) => {
                    vacant.insert(ProviderSlot {
                        holder: entry,
                        standby: Vec::new(),
                    });
                    Registered::Serving
                }
                Entry::Occupied(mut occupied) => {
                    let slot = occupied.get_mut();
                    if slot.holder.plugin_id == plugin_id {
                        slot.holder = entry;
                        Registered::Replaced
                    } else {
                        // A manifest listing the id twice keeps one place.
                        match slot.standby.iter_mut().find(|e| e.plugin_id == plugin_id) {
                            Some(waiting) => *waiting = entry,
                            None => slot.standby.push(entry),
                        }
                        Registered::Standby {
                            holder: slot.holder.plugin_id.clone(),
                        }
                    }
                }
            }
        };
        match outcome {
            Registered::Standby { holder } => tracing::warn!(
                datasource = %id,
                holder = %holder,
                standby = %plugin_id,
                "datasource id is already served by another live plugin; the holder keeps \
                 serving, and this one stands by to take over when it leaves",
            ),
            Registered::Replaced => tracing::info!(
                datasource = %id,
                plugin = %plugin_id,
                "datasource provider re-registered by the same plugin; replacing its earlier entry",
            ),
            Registered::Serving => {
                tracing::info!(datasource = %id, plugin = %plugin_id, "datasource provider registered");
            }
        }
    }

    /// Remove whatever this connection `generation` registered for `id`
    /// (teardown): the holder only if this generation is still the holder's —
    /// the same guard `clear_region_if_owned` applies to region cards — and
    /// otherwise its place in the standby line, if it has one.
    ///
    /// When the holder leaves, the **oldest live standby** serves from then
    /// on, with one log line naming both; a standby whose connection is
    /// already closed (its writer gone, its teardown still to come) is skipped
    /// and dropped. With no standby left the id is free.
    pub(super) fn unregister_provider(&self, id: &str, generation: u64) {
        let outcome = {
            let mut providers = self
                .providers
                .lock()
                .expect("datasource providers poisoned");
            match providers.get_mut(id) {
                None => Unregistered::Nothing,
                Some(slot) if slot.holder.generation != generation => {
                    let before = slot.standby.len();
                    slot.standby.retain(|e| e.generation != generation);
                    if slot.standby.len() == before {
                        Unregistered::Nothing
                    } else {
                        Unregistered::Standby
                    }
                }
                Some(slot) => {
                    slot.standby.retain(|e| !e.outbound.is_closed());
                    if slot.standby.is_empty() {
                        providers.remove(id);
                        Unregistered::Free
                    } else {
                        let next = slot.standby.remove(0);
                        let left = std::mem::replace(&mut slot.holder, next);
                        Unregistered::Promoted {
                            left: left.plugin_id,
                            holder: slot.holder.plugin_id.clone(),
                        }
                    }
                }
            }
        };
        match outcome {
            Unregistered::Nothing => {}
            Unregistered::Standby => {
                tracing::debug!(datasource = %id, "datasource standby left before it served");
            }
            Unregistered::Free => {
                tracing::info!(datasource = %id, "datasource provider unregistered");
            }
            Unregistered::Promoted { left, holder } => tracing::info!(
                datasource = %id,
                left = %left,
                holder = %holder,
                "datasource provider left; its oldest standby serves now",
            ),
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
                guard.get(&provider).map(|slot| {
                    let e = &slot.holder;
                    (e.plugin_id.clone(), e.outbound.clone(), e.scopes.clone())
                })
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
                    answer_admitted: false,
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

    /// Whether an [`Effect::DatasourceResult`](hytte_plugin_proto::Effect::DatasourceResult)
    /// that `responder_id` emitted for correlation `corr` answers a query the host
    /// parked for exactly that provider, and is the first answer to it to ask —
    /// marking it admitted if so. The provider's session asks this before its
    /// effect bucket (`session::throttle_effects`), and a `true` answer skips the
    /// bucket.
    ///
    /// The exemption is bounded by things the host controls, not the provider:
    /// every correlation that can say `true` was minted by
    /// [`route_query`](Self::route_query) for a query a requester paid for from
    /// its own effect bucket, and each says `true` once. A busy provider's
    /// honest answers therefore never compete with its own toasts and page
    /// opens for one token a second — which used to drop them and time the
    /// requesters out. Anything else (an unknown or expired correlation, another
    /// provider's, a second answer to the same query) is `false` and metered as
    /// before.
    ///
    /// Admitting changes nothing about delivery:
    /// [`deliver_result`](Self::deliver_result) still checks the responder and
    /// resolves the entry, and drops a later answer to the same query as
    /// unknown.
    pub(super) fn admit_answer(&self, corr: u64, responder_id: &str) -> bool {
        let mut pending = self.pending.lock().expect("datasource pending poisoned");
        match pending.get_mut(&corr) {
            Some(query) if query.provider_id == responder_id && !query.answer_admitted => {
                query.answer_admitted = true;
                true
            }
            _ => false,
        }
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

    /// A channel whose receiver is already gone: what a provider's outbound
    /// looks like once its writer has stopped.
    fn chan() -> mpsc::Sender<HostMsg> {
        mpsc::channel(8).0
    }

    /// A channel whose receiver the test keeps: a provider still connected.
    fn live() -> (mpsc::Sender<HostMsg>, mpsc::Receiver<HostMsg>) {
        mpsc::channel(8)
    }

    /// The holder of datasource `id`, as `(plugin id, generation)`.
    fn holder(r: &DatasourceRouter, id: &str) -> Option<(String, u64)> {
        r.providers
            .lock()
            .unwrap()
            .get(id)
            .map(|slot| (slot.holder.plugin_id.clone(), slot.holder.generation))
    }

    /// The standby line of datasource `id`, oldest first, as plugin ids.
    fn standbys(r: &DatasourceRouter, id: &str) -> Vec<String> {
        r.providers
            .lock()
            .unwrap()
            .get(id)
            .map(|slot| slot.standby.iter().map(|e| e.plugin_id.clone()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn register_then_lookup_and_scope_check() {
        let r = DatasourceRouter::default();
        r.register_provider("departures", "dep-plugin", vec!["next".into()], chan(), 1);
        let guard = r.providers.lock().unwrap();
        let entry = &guard.get("departures").expect("registered").holder;
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

    /// The same plugin id replaces its own entry. A manifest listing the
    /// datasource twice is what reaches this in practice (one generation);
    /// a newer generation of the same plugin, #509's case, does the same, and
    /// the older generation's teardown is then a no-op.
    #[test]
    fn a_reregistering_provider_replaces_its_own_entry() {
        let r = DatasourceRouter::default();
        r.register_provider("x", "weather", vec!["a".into()], chan(), 1);
        r.register_provider("x", "weather", vec!["b".into()], chan(), 1);
        {
            let guard = r.providers.lock().unwrap();
            let slot = guard.get("x").expect("present");
            assert_eq!(slot.holder.scopes, ["b"], "the later entry wins");
            assert!(slot.standby.is_empty(), "and nothing waits behind itself");
        }
        r.register_provider("x", "weather", vec!["c".into()], chan(), 2);
        r.unregister_provider("x", 1);
        assert_eq!(holder(&r, "x"), Some(("weather".to_owned(), 2)));
    }

    /// A different plugin declaring a datasource another live plugin serves
    /// does not replace it: the holder keeps serving, one warning names both,
    /// and the newcomer stands by. A standby that leaves first is gone from
    /// the line, so the holder's own teardown then frees the id.
    ///
    /// Red against #509's unconditional latest-wins: the holder's entry is
    /// replaced and the first `holder` assertion fails.
    #[test]
    fn another_plugin_cannot_take_over_a_live_datasource() {
        let r = DatasourceRouter::default();
        r.register_provider("weather", "wx-plugin", vec!["current".into()], chan(), 1);
        let (captured, guard) = hytte_config::test_support::capture();
        r.register_provider("weather", "squatter", vec!["forged".into()], chan(), 2);
        drop(guard);
        assert_eq!(holder(&r, "weather"), Some(("wx-plugin".to_owned(), 1)));
        assert_eq!(standbys(&r, "weather"), ["squatter"]);
        let warnings: Vec<_> = captured
            .events()
            .into_iter()
            .filter(|e| e.level == tracing::Level::WARN)
            .collect();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].fields["holder"], "wx-plugin", "{warnings:?}");
        assert_eq!(warnings[0].fields["standby"], "squatter", "{warnings:?}");

        // The standby's teardown leaves the holder alone and takes it out of
        // the line, so it is never promoted.
        r.unregister_provider("weather", 2);
        assert_eq!(holder(&r, "weather"), Some(("wx-plugin".to_owned(), 1)));
        assert!(standbys(&r, "weather").is_empty());
        // The holder's own teardown, with nobody waiting, frees the id.
        r.unregister_provider("weather", 1);
        assert_eq!(holder(&r, "weather"), None);
        r.register_provider("weather", "squatter", vec!["current".into()], chan(), 3);
        assert_eq!(holder(&r, "weather"), Some(("squatter".to_owned(), 3)));
    }

    /// Failover: A holds, B stands by, A leaves — B serves, on its own
    /// channel and scopes, with one line naming both. B's teardown then frees
    /// the id.
    ///
    /// Red against a refusal that keeps nothing: the id is free once A goes,
    /// and every query gets `NotFound` until B reconnects.
    #[test]
    fn a_standby_serves_once_the_holder_leaves() {
        let r = DatasourceRouter::default();
        let (a, _a_rx) = live();
        let (b, _b_rx) = live();
        r.register_provider("weather", "wx-a", vec!["current".into()], a, 1);
        r.register_provider("weather", "wx-b", vec!["hourly".into()], b, 2);
        let (captured, guard) = hytte_config::test_support::capture();
        r.unregister_provider("weather", 1);
        drop(guard);
        {
            let guard = r.providers.lock().unwrap();
            let slot = guard.get("weather").expect("still served");
            assert_eq!(slot.holder.plugin_id, "wx-b");
            assert_eq!(slot.holder.generation, 2);
            assert_eq!(slot.holder.scopes, ["hourly"], "B serves what B declared");
            assert!(slot.standby.is_empty());
        }
        let promoted: Vec<_> = captured
            .events()
            .into_iter()
            .filter(|e| e.fields.contains_key("left"))
            .collect();
        assert_eq!(promoted.len(), 1, "{promoted:?}");
        assert_eq!(promoted[0].fields["left"], "wx-a", "{promoted:?}");
        assert_eq!(promoted[0].fields["holder"], "wx-b", "{promoted:?}");

        r.unregister_provider("weather", 2);
        assert_eq!(holder(&r, "weather"), None);
    }

    /// The line is oldest first, and only live connections are in it: a
    /// standby that left is never promoted, and one whose channel is already
    /// closed is skipped and dropped.
    #[test]
    fn a_standby_that_left_is_never_promoted() {
        let r = DatasourceRouter::default();
        let (a, _a_rx) = live();
        let (b, _b_rx) = live();
        let (c, _c_rx) = live();
        r.register_provider("weather", "wx-a", vec!["current".into()], a, 1);
        r.register_provider("weather", "wx-b", vec!["current".into()], b, 2);
        r.register_provider("weather", "wx-closed", vec!["current".into()], chan(), 3);
        r.register_provider("weather", "wx-c", vec!["current".into()], c, 4);
        assert_eq!(standbys(&r, "weather"), ["wx-b", "wx-closed", "wx-c"]);

        r.unregister_provider("weather", 2);
        assert_eq!(standbys(&r, "weather"), ["wx-closed", "wx-c"]);
        assert_eq!(holder(&r, "weather"), Some(("wx-a".to_owned(), 1)));

        r.unregister_provider("weather", 1);
        assert_eq!(holder(&r, "weather"), Some(("wx-c".to_owned(), 4)));
        assert!(
            standbys(&r, "weather").is_empty(),
            "the closed one was dropped"
        );

        r.unregister_provider("weather", 4);
        assert_eq!(holder(&r, "weather"), None);
    }

    /// Park a query at `corr` for `provider_id`, as `route_query` does, with a
    /// timeout task that never fires.
    fn park(r: &DatasourceRouter, corr: u64, provider_id: &str) {
        let timeout = tokio::spawn(std::future::pending::<()>()).abort_handle();
        r.pending.lock().unwrap().insert(
            corr,
            PendingQuery {
                requester: chan(),
                request_id: corr,
                provider_id: provider_id.to_owned(),
                timeout,
                answer_admitted: false,
            },
        );
    }

    /// The bucket exemption admits exactly one answer per query the host parked,
    /// and only from the provider it was routed to.
    #[tokio::test]
    async fn admit_answer_admits_one_answer_per_parked_query_from_its_provider() {
        let r = DatasourceRouter::default();
        park(&r, 7, "departures");
        assert!(
            !r.admit_answer(7, "squatter"),
            "another plugin's answer is metered"
        );
        assert!(
            !r.admit_answer(8, "departures"),
            "an unknown correlation is metered"
        );
        assert!(
            r.admit_answer(7, "departures"),
            "the routed provider's answer"
        );
        assert!(
            !r.admit_answer(7, "departures"),
            "a second answer to the same query is metered"
        );
        // Admitting delivers nothing and removes nothing: the entry is still
        // parked for `deliver_result` to resolve.
        assert!(r.pending.lock().unwrap().contains_key(&7));
    }
}
