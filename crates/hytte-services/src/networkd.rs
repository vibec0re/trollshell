//! Link state from systemd-networkd (`org.freedesktop.network1`).
//!
//! Polls the Manager's `ListLinks` once at startup and then whenever
//! `StateChanged` fires on the Manager, falling back to a 5-second timer so
//! newly-appeared links (hot-plug) never stall longer than 5 s.
//!
//! All D-Bus I/O goes through [`hytte_bus::call`] and [`hytte_bus::signals`]
//! so the shared connection supervisor handles reconnects automatically.
//!
//! # Backend selection (issue #80)
//!
//! systemd-networkd is **not** the link manager on every host — a
//! `NetworkManager`-managed desktop runs no networkd at all, so `ListLinks`
//! errors with `ServiceUnknown` and the panel's "All links" list stays empty.
//! At startup we probe (via [`crate::wifi_backend::probe_backend`]-style
//! `ListNames`/`ListActivatableNames`) whether networkd is actually present and
//! produces links; if it isn't and **`NetworkManager` is**, the link list is
//! sourced from NM over D-Bus instead (see [`crate::networkd_nm`]), feeding the
//! *same* [`Link`] list the panel already renders. This mirrors the #96 Wi-Fi
//! `NetworkManager` backend. No `/sys` scraping (rejected on #80/#91).
//!
//! # "Nothing to ask" is not "nothing there" (issue #608)
//!
//! Because the link list can be sourced from a daemon that isn't running, the
//! *absence* of a reading needs a representation of its own — otherwise every
//! consumer reads "no primary link" as "offline" and "no links" as "no
//! interfaces", which on a host with neither daemon is a negative claim built
//! out of a question nobody answered. [`LinkSource`] carries that distinction
//! alongside [`links`]/[`primary`], which keep their existing types.
//!
//! # A transient startup failure is not an absent daemon (issue #621)
//!
//! Once the probe has elected [`LinkBackend::Networkd`], a first `refresh` that
//! fails cannot mean "networkd isn't here" — the probe just established that it
//! is. It means the bus hiccuped in the microseconds since. That failure is
//! therefore retried, unboundedly and audibly, rather than ending the task:
//! see [`STARTUP_REFRESH_RETRY`].

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_bus::{BusKind, call, signals};
use hytte_reactive::{Service, registry, spawn_supervised};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use crate::retry;
use crate::wifi_backend::BackendChoice;

const NETWORKD_NAME: &str = "org.freedesktop.network1";
const MANAGER_PATH: &str = "/org/freedesktop/network1";
const MANAGER_IFACE: &str = "org.freedesktop.network1.Manager";
const LINK_IFACE: &str = "org.freedesktop.network1.Link";

pub struct NetworkdService;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OperationalState {
    #[default]
    Missing,
    Off,
    NoCarrier,
    Dormant,
    DegradedCarrier,
    Carrier,
    Degraded,
    EnslavedRouting,
    Routable,
    Unknown,
}

impl OperationalState {
    fn parse(s: &str) -> Self {
        match s {
            "missing" => Self::Missing,
            "off" => Self::Off,
            "no-carrier" => Self::NoCarrier,
            "dormant" => Self::Dormant,
            "degraded-carrier" => Self::DegradedCarrier,
            "carrier" => Self::Carrier,
            "degraded" => Self::Degraded,
            "enslaved" => Self::EnslavedRouting,
            "routable" => Self::Routable,
            _ => Self::Unknown,
        }
    }

    /// Coarse priority used to pick a "primary" link (highest wins).
    pub(crate) fn priority(self) -> u8 {
        match self {
            Self::Routable => 5,
            Self::Degraded => 4,
            Self::EnslavedRouting => 3,
            Self::Carrier | Self::DegradedCarrier => 2,
            Self::Dormant => 1,
            _ => 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct LinkAddress {
    pub addr: IpAddr,
    pub prefix_len: u8,
}

#[derive(Clone, Debug, Default)]
pub struct Link {
    pub idx: i32,
    pub name: String,
    pub operational: OperationalState,
    pub addresses: Vec<LinkAddress>,
    pub gateway_v4: Option<Ipv4Addr>,
    pub gateway_v6: Option<Ipv6Addr>,
    pub routes: Vec<RouteSummary>,
}

#[derive(Clone, Debug)]
pub struct RouteSummary {
    pub destination: IpAddr,
    pub prefix_len: u8,
    pub gateway: Option<IpAddr>,
    pub family: i32,
}

/// Whether any link manager is actually answering — the difference between
/// *unknown* and *known-absent* (issue #608).
///
/// [`links`] and [`primary`] cannot carry this themselves. An empty link list
/// and a `None` primary each mean two different things: "the manager answered
/// and there is no routable link / no interfaces", and "there is no manager to
/// answer". Only the first deserves the word *Offline*. #607's screenshot is
/// what the conflation looks like — the Connection card reporting "Offline / 0
/// interface(s)" directly above six live interfaces moving traffic, because the
/// traffic card reads the kernel while this service reads D-Bus.
///
/// A **separate signal** rather than a third state on [`primary`], so consumers
/// that only want the link — the bar chip, the address rows — keep their
/// `Option<Link>` untouched and only the callers that render an *absence* have
/// to ask. That is the one difference from `nightlight`'s `NightlightState`,
/// which folded its pending state into the signal the switch already bound to
/// because there was exactly one consumer; here there are several, and most of
/// them do not care.
///
/// It is also **not** [`LinkBackend`]. That is the *decision* of which daemon to
/// run a watcher against, and it is deliberately optimistic: an inconclusive
/// probe still picks `NetworkManager` so the watcher can self-heal (#607). This
/// is the *observation*, and it only names a source once that source has
/// actually produced a reading — so an optimistic guess can never launder itself
/// into a claim about the host.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LinkSource {
    /// Nothing has answered (yet). The startup probe may still be running, or
    /// it may have picked a backend that has produced no reading — including
    /// the optimistic `NetworkManager` fallback of an inconclusive probe, and a
    /// manager that has stopped answering since. An empty [`links`] or a `None`
    /// [`primary`] here means *not asked*, not *not there*.
    #[default]
    Unknown,
    /// systemd-networkd answered; its view of the host is what we render.
    Networkd,
    /// `NetworkManager` answered; ditto.
    NetworkManager,
    /// The bus answered and neither systemd-networkd nor `NetworkManager` is
    /// available: this host has no link manager. A *positive* finding, in the
    /// spirit of [`crate::wifi_backend::BackendChoice::None`] and
    /// [`crate::geoclue::LocationState::Unavailable`] — though note it is a fact
    /// about the *managers*, not about the kernel's interfaces, which may well
    /// be up and routing.
    Unavailable,
}

impl LinkSource {
    /// Whether a link manager is currently answering — i.e. whether an empty
    /// [`links`] or a `None` [`primary`] may be rendered as a statement about
    /// the host ("Offline", "0 interfaces") instead of as "we don't know".
    ///
    /// [`Self::Unavailable`] answers **false**: knowing that nothing manages the
    /// links tells you nothing about whether the host is online, which is
    /// precisely #608's complaint.
    #[must_use]
    pub fn is_answering(self) -> bool {
        matches!(self, Self::Networkd | Self::NetworkManager)
    }
}

#[doc(hidden)]
pub struct NetworkdHandles {
    pub(crate) links: Mutable<Vec<Link>>,
    pub(crate) primary: Mutable<Option<Link>>,
    /// Whether anything is answering for the two above — see [`LinkSource`].
    /// Written by the backend probe and promoted/retracted by every refresh.
    pub(crate) source: Mutable<LinkSource>,
}

impl Default for NetworkdHandles {
    fn default() -> Self {
        Self {
            links: Mutable::new(Vec::new()),
            primary: Mutable::new(None),
            // Nothing has been asked yet, and that is exactly what this says.
            source: Mutable::new(LinkSource::Unknown),
        }
    }
}

/// Which daemon should source the interface/link list on this host.
///
/// Probed once at startup. systemd-networkd is preferred when it is actually
/// managing links (its `ListLinks` returns a non-empty list); otherwise, if
/// `NetworkManager` is present, NM provides the list (issue #80).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LinkBackend {
    /// systemd-networkd (`org.freedesktop.network1`) is the active manager.
    Networkd,
    /// `NetworkManager` (`org.freedesktop.NetworkManager`) sources the links.
    NetworkManager,
    /// Neither is usable; the link list stays empty (panel shows nothing, as
    /// it does today).
    None,
}

impl LinkBackend {
    /// What is *known* about the link source the moment this verdict is reached,
    /// before any watcher has read anything.
    ///
    /// Only [`LinkBackend::None`] is knowledge — the bus answered and there is
    /// no manager, so [`LinkSource::Unavailable`] can be published immediately.
    /// The two named backends are a *plan*, and one of them is reached by an
    /// explicitly optimistic fallback (#607), so they publish nothing until a
    /// read actually lands: `refresh` is what promotes them to a named
    /// [`LinkSource`].
    fn initial_source(self) -> LinkSource {
        match self {
            Self::None => LinkSource::Unavailable,
            Self::Networkd | Self::NetworkManager => LinkSource::Unknown,
        }
    }
}

/// Decide which backend should source the link list.
///
/// Prefers networkd **only when it actually has links** — on a
/// `NetworkManager`-managed host `ListLinks` either errors (`ServiceUnknown`)
/// or returns an empty list, in which case we fall through to `NetworkManager` if
/// the `org.freedesktop.NetworkManager` bus name is present (mirroring
/// [`crate::wifi_backend::probe_backend`]). When the bus confirms neither is
/// available we return [`LinkBackend::None`] and the service stays inert.
///
/// A probe that *fails* is not the same as one reporting no `NetworkManager`
/// (#607): the selection is made once and never revisited, so an inconclusive
/// probe falls back to whichever source can still recover on its own rather
/// than to [`LinkBackend::None`], which would latch an empty link list.
async fn probe_link_backend() -> LinkBackend {
    // Does networkd have any links to show? `read_networkd_links` errors when
    // the network1 name isn't on the bus; an Ok-but-empty result means networkd
    // is running but not managing anything (e.g. NM-managed box where networkd
    // is installed-but-idle). Either way, prefer NM if it can fill the list.
    let networkd_has_links = match read_networkd_links().await {
        Ok(links) if !links.is_empty() => return LinkBackend::Networkd,
        Ok(_) => {
            tracing::info!("networkd present but no links; checking NetworkManager");
            true
        }
        Err(e) => {
            tracing::info!(error = ?e, "networkd unreachable; checking NetworkManager");
            false
        }
    };

    match crate::wifi_backend::probe_backend().await {
        Ok(BackendChoice::NetworkManager) => LinkBackend::NetworkManager,
        Ok(_) if networkd_has_links => {
            // networkd answered (just with no links yet) and NM isn't present —
            // keep networkd as the source so its listen loop's periodic refresh
            // picks up interfaces as they enrol.
            LinkBackend::Networkd
        }
        // The bus answered: neither networkd nor NetworkManager is usable.
        Ok(_) => LinkBackend::None,
        Err(e) if networkd_has_links => {
            // Inconclusive probe (#607) — we do NOT know that NM is absent, so
            // we must not read the failure as a negative. networkd at least
            // answered, and its listen loop re-polls, so prefer it.
            tracing::warn!(
                error = %e,
                "networkd: NetworkManager probe was inconclusive; keeping networkd as the \
                 link source since it answered"
            );
            LinkBackend::Networkd
        }
        Err(e) => {
            // Neither source has been ruled in *or out*. Picking `None` here is
            // what latched #607: it leaves the link list permanently empty even
            // once the bus recovers. The NM watcher re-polls every 5s, so it
            // self-heals if NM does turn up; the cost if NM genuinely never
            // appears is a periodic `GetDevices failed` warn, which beats an
            // inert service.
            tracing::error!(
                error = %e,
                "networkd: NetworkManager probe was INCONCLUSIVE and networkd is unreachable \
                 — this is not evidence that NetworkManager is absent. Sourcing links from \
                 NetworkManager anyway; its watcher re-polls and will recover if the bus \
                 comes back (issue #607)."
            );
            LinkBackend::NetworkManager
        }
    }
}

// ── Startup refresh retry (issue #621) ───────────────────────────────────────
//
// The retry *mechanism* — schedule, attempt budget, the pure `Proceed`/`Retry`/
// `GiveUp` decision — lives in `crate::retry`, shared with `wifi`'s backend
// probe since #646. The two paths retry the same class of transient system-bus
// failure, and someone debugging one while reading the journal of the other must
// not find two different behaviours; before #646 that was kept true by a pair of
// doc comments pointing at each other by hand.
//
// What stays here is the *judgement*: why this policy is unbounded, and what
// each attempt logs.

/// The policy the [`LinkBackend::Networkd`] arm runs: **unbounded** retry with
/// capped backoff.
///
/// Attempt 1 is immediate; each retry waits 0.5s doubled per elapsed attempt and
/// clamped at 8s, so the schedule settles into one refresh every 8s for as long
/// as networkd stays unreachable. The numbers match `wifi`'s `PROBE_RETRY`
/// (#634) on purpose — same failure class, same journal cadence.
///
/// **Why unbounded.** This file already answered the question just above, in
/// [`probe_link_backend`]'s inconclusive-probe arm: picking the inert verdict
/// "leaves the link list permanently empty even once the bus recovers … the cost
/// if NM genuinely never appears is a periodic warn, which beats an inert
/// service". The same reasoning applies here, and more directly: a bound means
/// that whenever networkd takes longer than the bound to come up, the user lands
/// in exactly the state #621 reports — the panel stuck on "no link manager has
/// answered yet" with `systemctl --user restart trollshell` the only cure. That
/// is the bug this closes, reintroduced on a longer fuse. A bound cannot be
/// justified by its own give-up being logged, either: that argues for having a
/// bound from the visibility of the bound being wrong.
///
/// **Visibility is preserved without the latch.** Every failed attempt logs at
/// `error!` and a retry that finally lands logs a `RECOVERED` line carrying the
/// attempt count — a quiet self-heal would trade a visible permanent bug for an
/// invisible intermittent one, which is worse for exactly the reporter's
/// situation (nothing in the UI hinted anything had failed). Logging every
/// attempt rather than every Nth is safe *because* the backoff caps at 8s: a
/// permanently dead networkd stays loud in the journal instead of degrading into
/// a silent poll, and it cannot become the 2s flood the old comment feared.
///
/// **The policy is one field to change.** A bounded policy is
/// `max_attempts: Some(n)` and nothing else; `the_shipped_policy_never_goes_inert`
/// asserts the shipped default is not.
///
/// Unlike `wifi`'s probe this path has no verdict to weigh before consulting the
/// policy: a `refresh` either read the links or could not, and "could not" is
/// never a fact about the host here — the backend was already elected.
///
/// The schedule's own invariants — a nonzero first delay, a ceiling that is
/// actually reached — are asserted over *every* shipped policy in `retry`'s
/// tests (#665), not here.
pub(crate) const STARTUP_REFRESH_RETRY: retry::Policy = retry::Policy {
    max_attempts: None,
    initial: Duration::from_millis(500),
    max_backoff: Duration::from_secs(8),
};

/// Seed the link list from networkd, retrying while [`refresh`] fails.
///
/// Returns `true` once a refresh has landed — i.e. once it is worth entering the
/// `listen` loop. `false` means the policy stopped asking first, which the
/// shipped unbounded [`STARTUP_REFRESH_RETRY`] never does.
async fn seed_links(
    policy: retry::Policy,
    links_out: &Mutable<Vec<Link>>,
    primary_out: &Mutable<Option<Link>>,
    source_out: &Mutable<LinkSource>,
) -> bool {
    let mut attempt: u32 = 1;
    loop {
        let outcome = refresh(links_out, primary_out, source_out).await;
        // Rendered up front so the log arms below can name the failure. Empty on
        // `Ok`, which only ever reaches the `Proceed` arm.
        let reason = outcome
            .as_ref()
            .err()
            .map_or_else(String::new, ToString::to_string);

        match policy.step(&outcome, attempt) {
            retry::Step::Proceed => {
                // Only announce a recovery if there was something to recover
                // from; the ordinary first-try success stays quiet.
                if attempt > 1 {
                    tracing::warn!(
                        attempts = attempt,
                        "networkd: startup refresh RECOVERED — an earlier attempt could not read \
                         networkd's links, and a retry has now succeeded. The link panel is \
                         populating without a shell restart (issue #621)."
                    );
                }
                return true;
            }
            retry::Step::Retry { after } => {
                // Logged on *every* attempt, not every Nth: the backoff caps at
                // 8s, so an unreachable networkd stays loud here rather than
                // degrading into a silent poll. See `STARTUP_REFRESH_RETRY`.
                tracing::error!(
                    attempt,
                    retry_in_secs = after.as_secs_f64(),
                    error = %reason,
                    "networkd: startup refresh FAILED — the backend probe already established \
                     that systemd-networkd is the link source on this host, so this is a \
                     transient read failure, NOT a finding that networkd is absent. Retrying \
                     until it answers; this line repeating means networkd is still unreachable, \
                     and a `startup refresh RECOVERED` line follows once it heals (issue #621)."
                );
                tokio::time::sleep(after).await;
                attempt += 1;
            }
            retry::Step::GiveUp => {
                tracing::error!(
                    attempts = attempt,
                    error = %reason,
                    "networkd: startup refresh STILL FAILING after every retry — giving up and \
                     staying inert. The link panel will report that nothing has answered for the \
                     rest of this session; run `systemctl --user restart trollshell` once \
                     networkd is up (issue #621)."
                );
                return false;
            }
        }
    }
}

impl Service for NetworkdService {
    type Handles = NetworkdHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = NetworkdHandles::default();
        let links_writer = handles.links.clone();
        let primary_writer = handles.primary.clone();
        let source_writer = handles.source.clone();

        spawn_supervised("networkd", move || {
            let links_writer = links_writer.clone();
            let primary_writer = primary_writer.clone();
            let source_writer = source_writer.clone();
            async move {
                let backend = probe_link_backend().await;
                // Publish what the verdict alone establishes — for the two named
                // backends that is nothing at all, since one of them is an
                // optimistic fallback. See `LinkBackend::initial_source`.
                source_writer.set_neq(backend.initial_source());
                match backend {
                    LinkBackend::NetworkManager => {
                        tracing::info!(
                            "networkd: sourcing link list from NetworkManager (networkd not managing)"
                        );
                        crate::networkd_nm::run_nm_links_watcher(
                            links_writer,
                            primary_writer,
                            source_writer,
                        )
                        .await;
                    }
                    LinkBackend::Networkd => {
                        // Seed the list, retrying until networkd answers. A failure
                        // here is NOT evidence that networkd is absent: this arm is
                        // reached only after `probe_link_backend` established that
                        // networkd IS the backend, so a refresh failing microseconds
                        // later is transient by construction — networkd restarting, a
                        // D-Bus hiccup, `ServiceUnknown` mid `systemctl restart
                        // systemd-networkd`. The genuinely-absent case is
                        // `LinkBackend::None`, handled below.
                        //
                        // This used to `return` on the first failure, which latched
                        // `LinkSource::Unknown` — "no link manager has answered yet" —
                        // for the rest of the process lifetime, curable only by
                        // restarting the shell (#621). See `STARTUP_REFRESH_RETRY` for
                        // why the retry is unbounded and why it stays audible.
                        if !seed_links(
                            STARTUP_REFRESH_RETRY,
                            &links_writer,
                            &primary_writer,
                            &source_writer,
                        )
                        .await
                        {
                            return;
                        }
                        // The 2s retry below is untouched, but not for the reason
                        // this comment used to give (#665): it does *not* only
                        // cover a `listen` that had been established and dropped.
                        // `listen` opens with its own initial `refresh`, so this
                        // loop also covers a `listen` that was never established
                        // — i.e. a networkd that died between the seed above and
                        // the subscription below.
                        //
                        // That is still a milder failure than #621: the seed has
                        // already published `LinkSource::Networkd`, so a later
                        // death leaves the link list *stale*, not indeterminate,
                        // and it self-heals when networkd returns. The
                        // observability gap is real though — a post-seed recovery
                        // shows up as this loop's `warn!` going quiet, **not** as
                        // a `startup refresh RECOVERED` line, so grepping for
                        // that line will not evidence a real self-heal. This
                        // used to be an uncapped flat 2s cadence; #646's second
                        // half moved it onto `retry::RECONNECT_RETRY`, resetting
                        // the attempt count after a `listen` that stayed up at
                        // least `retry::RECONNECT_RESET_AFTER` so a merely-flaky
                        // networkd doesn't ratchet to the 30s ceiling and stay
                        // there. #806 moved the counter itself into
                        // `retry::ReconnectBackoff`: the reset/read/advance
                        // ordering it encodes was hand-rolled here (and in three
                        // sibling loops), and hand-rolled wrong — the reset
                        // landed a cycle late, so a run that stayed healthy for
                        // hours still reconnected at the ratcheted delay. All
                        // this loop owns now is the clock and the `warn!`.
                        let mut backoff = retry::ReconnectBackoff::new();
                        loop {
                            let started = std::time::Instant::now();
                            let outcome =
                                listen(&links_writer, &primary_writer, &source_writer).await;
                            let delay = backoff.delay_after_run(started.elapsed());
                            match outcome {
                                Ok(()) => {
                                    tracing::warn!(?delay, "networkd stream ended, reconnecting");
                                }
                                Err(e) => {
                                    tracing::warn!(?delay, error = ?e, "networkd error, reconnecting");
                                }
                            }
                            tokio::time::sleep(delay).await;
                        }
                    }
                    LinkBackend::None => {
                        // A positive finding, already published above as
                        // `LinkSource::Unavailable`: consumers can say "no link
                        // manager" instead of rendering the empty list as "Offline".
                        tracing::info!("networkd: no link backend available; service inert");
                    }
                }
            }
        });

        handles
    }
}

async fn listen(
    links_out: &Mutable<Vec<Link>>,
    primary_out: &Mutable<Option<Link>>,
    source_out: &Mutable<LinkSource>,
) -> Result<()> {
    // Subscribe to StateChanged on the Manager so we react quickly to
    // link state transitions.  Missed-emissions on reconnect trigger a
    // re-poll too, so we never miss a change across a D-Bus restart.
    let state_changed = signals(BusKind::System, NETWORKD_NAME)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .signal("StateChanged")
        .start();

    let mut events = state_changed.events();

    // Initial poll.
    refresh(links_out, primary_out, source_out).await?;

    // 5-second fallback timer — catches hot-plug when StateChanged is
    // not emitted (e.g. older networkd, or newly added links).
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Discard the immediate first tick (we already polled above).
    interval.tick().await;

    loop {
        tokio::select! {
            _ = events.next() => {
                tracing::debug!("networkd StateChanged; refreshing links");
                if let Err(e) = refresh(links_out, primary_out, source_out).await {
                    tracing::warn!(error = ?e, "networkd refresh after StateChanged failed");
                }
            }
            _ = interval.tick() => {
                if let Err(e) = refresh(links_out, primary_out, source_out).await {
                    tracing::warn!(error = ?e, "networkd periodic refresh failed");
                }
            }
        }
    }
}

/// Poll networkd once and publish the result, including whether networkd
/// answered at all ([`LinkSource`]).
///
/// A failed read **retracts** the source to [`LinkSource::Unknown`] rather than
/// leaving the last reading looking authoritative: once nothing is answering, a
/// `None` primary is no longer evidence of being offline (#608). The link list
/// itself is left as it was — a stale reading is still a reading, and clearing it
/// would manufacture the "0 interface(s)" this change exists to stop.
async fn refresh(
    links_out: &Mutable<Vec<Link>>,
    primary_out: &Mutable<Option<Link>>,
    source_out: &Mutable<LinkSource>,
) -> Result<()> {
    let links = match read_networkd_links().await {
        Ok(links) => links,
        Err(e) => {
            source_out.set_neq(LinkSource::Unknown);
            return Err(e);
        }
    };
    let primary = links
        .iter()
        .max_by_key(|l| l.operational.priority())
        .filter(|l| l.operational.priority() > 0)
        .cloned();

    links_out.set(links);
    primary_out.set(primary);
    source_out.set_neq(LinkSource::Networkd);
    Ok(())
}

/// Read the link list from systemd-networkd's `ListLinks` + per-link
/// `Describe`.
///
/// Returns an error when the `org.freedesktop.network1` name is not on the bus
/// (e.g. networkd isn't running) — callers use that to fall back to the
/// `NetworkManager` source. There is no `/sys` fallback (rejected on #80/#91).
async fn read_networkd_links() -> Result<Vec<Link>> {
    // ListLinks returns array of (idx: i32, name: String, path: ObjectPath).
    let list: Vec<(i32, String, zbus::zvariant::OwnedObjectPath)> =
        call(BusKind::System, NETWORKD_NAME)
            .at_path(MANAGER_PATH)
            .iface(MANAGER_IFACE)
            .method("ListLinks")
            .args(())
            .send()
            .await
            .context("ListLinks")?;

    let mut out = Vec::with_capacity(list.len());
    for (idx, name, path) in list {
        let path_str = path.as_str().to_string();

        let describe_json: String = call(BusKind::System, NETWORKD_NAME)
            .at_path(path_str.clone())
            .iface(LINK_IFACE)
            .method("Describe")
            .args(())
            .send()
            .await
            .inspect_err(|e| tracing::warn!(error = ?e, link = %name, "networkd Describe failed; treating link as address-less"))
            .unwrap_or_default();

        // OperationalState is also in the Describe JSON, but older networkd
        // only exposes it as a property.  Read it directly so we always have it.
        let op_prop: String = call(BusKind::System, NETWORKD_NAME)
            .at_path(path_str.clone())
            .iface("org.freedesktop.DBus.Properties")
            .method("Get")
            .args((LINK_IFACE, "OperationalState"))
            .send::<zbus::zvariant::OwnedValue>()
            .await
            .ok()
            .and_then(|v| String::try_from(v).ok())
            .unwrap_or_default();

        // The `Describe` method returns a JSON blob; parse addresses & routes.
        let parsed = parse_describe(&describe_json)
            .inspect_err(|e| tracing::warn!(error = ?e, link = %name, "networkd Describe JSON parse failed; treating link as address-less"))
            .unwrap_or_default();

        out.push(Link {
            idx,
            name,
            operational: OperationalState::parse(&op_prop),
            addresses: parsed.addresses,
            gateway_v4: parsed.gateway_v4,
            gateway_v6: parsed.gateway_v6,
            routes: parsed.routes,
        });
    }
    Ok(out)
}

#[must_use]
pub fn service() -> NetworkdService {
    NetworkdService
}

pub fn links() -> impl Signal<Item = Vec<Link>> {
    registry::with(|r| {
        r.get::<NetworkdHandles>()
            .expect("networkd::service() not registered")
            .links
            .signal_cloned()
    })
}

pub fn primary() -> impl Signal<Item = Option<Link>> {
    registry::with(|r| {
        r.get::<NetworkdHandles>()
            .expect("networkd::service() not registered")
            .primary
            .signal_cloned()
    })
}

/// Signal of whether any link manager is answering — see [`LinkSource`].
///
/// The companion to [`links`] and [`primary`]: those say *what* the link picture
/// is, this says whether anybody answered the question. Bind to it anywhere an
/// empty answer would otherwise be rendered as a negative fact (#608).
pub fn link_source() -> impl Signal<Item = LinkSource> {
    registry::with(|r| {
        r.get::<NetworkdHandles>()
            .expect("networkd::service() not registered")
            .source
            .signal()
    })
}

#[derive(Default, serde::Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct DescribeLink {
    addresses: Vec<DescribeAddress>,
    route_data: Vec<DescribeRoute>,
}

#[derive(Default, serde::Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct DescribeAddress {
    family: i32,
    address: Vec<u8>,
    prefix_length: u8,
}

#[derive(Default, serde::Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct DescribeRoute {
    family: i32,
    destination: Vec<u8>,
    destination_prefix_length: u8,
    gateway: Option<Vec<u8>>,
}

#[derive(Debug, Default)]
pub(crate) struct ParsedDescribe {
    pub addresses: Vec<LinkAddress>,
    pub gateway_v4: Option<Ipv4Addr>,
    pub gateway_v6: Option<Ipv6Addr>,
    pub routes: Vec<RouteSummary>,
}

pub(crate) fn parse_describe(json: &str) -> anyhow::Result<ParsedDescribe> {
    let raw: DescribeLink = serde_json::from_str(json).context("parse Describe JSON")?;
    let mut out = ParsedDescribe::default();

    for a in raw.addresses {
        if let Some(addr) = bytes_to_ip(a.family, &a.address) {
            out.addresses.push(LinkAddress {
                addr,
                prefix_len: a.prefix_length,
            });
        }
    }

    for r in raw.route_data {
        let Some(dest) = bytes_to_ip(r.family, &r.destination) else {
            continue;
        };
        let gw = r.gateway.as_ref().and_then(|g| bytes_to_ip(r.family, g));
        let is_default = r.destination_prefix_length == 0
            && match dest {
                IpAddr::V4(v4) => v4.is_unspecified(),
                IpAddr::V6(v6) => v6.is_unspecified(),
            };
        if is_default {
            if let Some(IpAddr::V4(g4)) = gw {
                out.gateway_v4 = Some(g4);
            } else if let Some(IpAddr::V6(g6)) = gw {
                out.gateway_v6 = Some(g6);
            }
        }
        out.routes.push(RouteSummary {
            destination: dest,
            prefix_len: r.destination_prefix_length,
            gateway: gw,
            family: r.family,
        });
    }

    Ok(out)
}

fn bytes_to_ip(family: i32, bytes: &[u8]) -> Option<IpAddr> {
    match (family, bytes.len()) {
        (2, 4) => Some(IpAddr::V4(Ipv4Addr::new(
            bytes[0], bytes[1], bytes[2], bytes[3],
        ))),
        (10, 16) => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(bytes);
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- LinkSource (#608) ---

    #[test]
    fn only_a_daemon_that_answered_licenses_a_negative_claim() {
        // The whole point: `is_answering()` is the permission to render an empty
        // reading as a fact. Both non-answering states must withhold it.
        assert!(LinkSource::Networkd.is_answering());
        assert!(LinkSource::NetworkManager.is_answering());
        assert!(!LinkSource::Unknown.is_answering());
        assert!(
            !LinkSource::Unavailable.is_answering(),
            "knowing nothing manages the links says nothing about being online"
        );
    }

    #[test]
    fn a_service_that_has_read_nothing_claims_nothing() {
        // The default a freshly-started (or never-registered) service publishes.
        // If this were anything but `Unknown`, the panel would assert a state
        // before the probe had even run.
        assert_eq!(LinkSource::default(), LinkSource::Unknown);
        assert!(!LinkSource::default().is_answering());
        assert_eq!(NetworkdHandles::default().source.get(), LinkSource::Unknown);
    }

    #[test]
    fn only_the_no_backend_verdict_is_knowledge_on_its_own() {
        // `LinkBackend` is a plan, not an observation. Only "the bus answered and
        // neither daemon is there" can be published before a read lands.
        assert_eq!(
            LinkBackend::None.initial_source(),
            LinkSource::Unavailable,
            "a confirmed absence of managers is a positive finding"
        );
        assert_eq!(LinkBackend::Networkd.initial_source(), LinkSource::Unknown);
        assert_eq!(
            LinkBackend::NetworkManager.initial_source(),
            LinkSource::Unknown,
            "the NM verdict can come from an inconclusive probe (#607), so it \
             must not promote itself into a claim about the host"
        );
    }

    #[test]
    fn no_verdict_can_publish_a_named_source_before_a_read() {
        // Stated as an invariant over every variant, so a future backend added to
        // `LinkBackend` cannot quietly start claiming to answer at probe time.
        for backend in [
            LinkBackend::Networkd,
            LinkBackend::NetworkManager,
            LinkBackend::None,
        ] {
            assert!(
                !backend.initial_source().is_answering(),
                "{backend:?} claimed to be answering before reading anything"
            );
        }
    }

    // --- startup refresh retry (#621) ---
    //
    // Since #646 the retry *mechanism* — the doubling schedule, the attempt
    // budget, the give-up boundary — is tested once, in `crate::retry`, over the
    // shared type; so are the shipped constants' delay invariants (#665), over
    // *every* shipped policy rather than just this one. What is asserted here is
    // this seed's own claim: a failed refresh retries rather than ending the
    // task.

    /// A *bounded* policy with tiny delays, so the give-up path stays reachable
    /// and named. Deliberately not [`STARTUP_REFRESH_RETRY`] — these tests
    /// assert the decision; the shipped shape is asserted separately below.
    fn bounded() -> retry::Policy {
        retry::Policy {
            max_attempts: Some(3),
            initial: Duration::from_millis(10),
            max_backoff: Duration::from_millis(40),
        }
    }

    fn failed() -> Result<()> {
        Err(anyhow::anyhow!(
            "networkd went away between the probe and the seed"
        ))
    }

    #[test]
    fn a_refresh_that_landed_proceeds_immediately() {
        // A local binding rather than a helper fn: a function that always
        // returns `Ok` trips `clippy::unnecessary_wraps`, and the wrapper is the
        // whole point here — `step` decides on the `Result`, not on a bool.
        let landed: Result<()> = Ok(());
        assert_eq!(bounded().step(&landed, 1), retry::Step::Proceed);
        // Success ends the retrying whenever it arrives, not just first time.
        assert_eq!(bounded().step(&landed, 3), retry::Step::Proceed);
        assert_eq!(bounded().step(&landed, u32::MAX), retry::Step::Proceed);
    }

    #[test]
    fn a_failed_startup_refresh_retries_instead_of_ending_the_task() {
        // The regression: this used to `return`, latching `LinkSource::Unknown`
        // for the process lifetime. A failure must schedule another attempt.
        assert_eq!(
            bounded().step(&failed(), 1),
            retry::Step::Retry {
                after: Duration::from_millis(10)
            },
            "the first failed seed must retry, not go inert (#621)"
        );
        assert_eq!(
            bounded().step(&failed(), 2),
            retry::Step::Retry {
                after: Duration::from_millis(20)
            }
        );
    }

    /// The delays are deliberately **not** asserted here, and deliberately never
    /// against `STARTUP_REFRESH_RETRY.backoff(attempt)` — that is the expression
    /// `step` computes internally, so comparing against it pins only that `step`
    /// calls `backoff`, not that `backoff` produces a sane number. That
    /// tautology is what #665 filed: with it, `initial: Duration::ZERO` kept the
    /// whole suite green while turning the retry into a tight `error!` flood.
    /// `crate::retry`'s shipped-policy tests carry the real assertions, for this
    /// constant and `wifi::PROBE_RETRY` alike.
    #[test]
    fn the_shipped_policy_never_goes_inert() {
        // The decision on #621: unbounded. A bound would put the user back in
        // the reported state — panel stuck on "nothing has answered", restart
        // the only cure — whenever the bus takes longer than the bound.
        assert_eq!(
            STARTUP_REFRESH_RETRY.max_attempts, None,
            "the shipped startup-refresh policy must never give up; see \
             STARTUP_REFRESH_RETRY's docs and #621"
        );
        for attempt in [1_u32, 2, 8, 1_000, u32::MAX] {
            assert!(
                matches!(
                    STARTUP_REFRESH_RETRY.step(&failed(), attempt),
                    retry::Step::Retry { .. }
                ),
                "attempt {attempt}: a failed seed ended the task instead of retrying"
            );
        }
    }

    // --- parse_describe ---

    const SAMPLE_DESCRIBE: &str = r#"{
        "Index": 3,
        "Name": "wlp1s0",
        "OperationalState": "routable",
        "Addresses": [
            {"Family": 2, "Address": [192, 168, 1, 42], "PrefixLength": 24}
        ],
        "RouteData": [
            {
                "Family": 2,
                "Destination": [0, 0, 0, 0],
                "DestinationPrefixLength": 0,
                "Gateway": [192, 168, 1, 1]
            },
            {
                "Family": 2,
                "Destination": [192, 168, 1, 0],
                "DestinationPrefixLength": 24
            }
        ]
    }"#;

    #[test]
    fn parses_describe_json_minimal() {
        let parsed = parse_describe(SAMPLE_DESCRIBE).expect("parse");
        assert_eq!(parsed.addresses.len(), 1);
        assert_eq!(
            parsed.addresses[0].addr,
            IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 42))
        );
        assert_eq!(parsed.addresses[0].prefix_len, 24);
        assert_eq!(
            parsed.gateway_v4,
            Some(std::net::Ipv4Addr::new(192, 168, 1, 1))
        );
        assert_eq!(parsed.gateway_v6, None);
        assert_eq!(parsed.routes.len(), 2);
    }

    #[test]
    fn handles_unknown_fields() {
        let json = r#"{
            "Index": 1,
            "FutureField": "anything",
            "Addresses": [{"Family": 2, "Address": [10, 0, 0, 1], "PrefixLength": 8, "ExtraJunk": 99}]
        }"#;
        let parsed = parse_describe(json).expect("parse");
        assert_eq!(parsed.addresses.len(), 1);
    }

    #[test]
    fn default_route_populates_gateway_v4() {
        let json = r#"{
            "RouteData": [
                {"Family": 2, "Destination": [10, 0, 0, 0], "DestinationPrefixLength": 8, "Gateway": [10, 0, 0, 1]},
                {"Family": 2, "Destination": [0, 0, 0, 0], "DestinationPrefixLength": 0, "Gateway": [192, 168, 0, 1]}
            ]
        }"#;
        let parsed = parse_describe(json).expect("parse");
        assert_eq!(
            parsed.gateway_v4,
            Some(std::net::Ipv4Addr::new(192, 168, 0, 1))
        );
    }
}
