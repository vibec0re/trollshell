//! A pairing the user agreed to makes the device `Trusted` once it has
//! **succeeded**, whichever side started it (#1603 review, MED-1 / MED-A).
//!
//! bluetoothd asks the agent (`AuthorizeService`) before an untrusted device
//! may use a service, and lets a `Trusted` one through without asking
//! (`process_auth_queue` in `src/adapter.c`, `BlueZ` 5.87). Nothing in
//! bluetoothd sets `Trusted` by itself: its only setter is the D-Bus property
//! (`device.c`). So the shell sets it, once a pairing the user agreed to has
//! happened, and only devices the user deliberately left untrusted ask.
//!
//! **Why not set it earlier.** bluetoothd runs the service authorizations it
//! queued during a pairing in the same function that completes the pairing
//! (`device_svc_resolved`), before a `Set Trusted` sent after it could land.
//! Setting `Trusted` before the pairing closes that race, but it costs too
//! much: a device that is trusted when its bonding fails is not returned to
//! the temporary state (`device_bonding_complete`, `device.c`), so every
//! failed attempt left a permanent row behind, and a shell that died
//! mid-pair left it stored as trusted.
//!
//! **What closes the race instead: [`Pairings`].** An in-memory set of the
//! devices whose pairing the user started or accepted and that have not
//! finished. While a device is in it, the agent lets its `AuthorizeService`
//! requests through without a prompt. bluetoothd sends them to the agent
//! anyway (it skips only trusted devices), so nothing in bluetoothd is
//! touched until the pairing succeeds. Nothing in the set outlives the
//! shell.
//!
//! - **The shell pairs a device** ([`pair_then_trust`]). The device enters
//!   the set before `Pair`. When `Pair` succeeds, `Trusted` is set, the
//!   device leaves the set, and it is connected. When `Pair` fails or is
//!   cancelled, it leaves the set and nothing is set. The `Pair` call's own
//!   deadline bounds how long it can stay.
//! - **The device pairs with us** ([`accepted`]). When the user accepts the
//!   agent's pairing prompt (confirmation, authorization, PIN or passkey), the
//!   device enters the set before the agent replies. As soon as the watcher's
//!   snapshot shows it `Paired`, `Trusted` is set and it leaves the set. It
//!   also leaves, with nothing set, when the snapshot loses it (removed, or
//!   bluetoothd gone) or after [`PAIRING_WINDOW`]. A rejected prompt never
//!   enters the set.
//!
//! A newer pairing for the same device replaces the older one's entry
//! (generations), so a retry is never ended by the attempt before it.
//!
//! **A new pairing is a new decision.** If the user set a device to Untrust
//! in its menu and later pairs it again (accepting the device's pairing
//! prompt, or choosing Pair here), it ends up trusted again. Accepting a
//! pairing means trusting the device, whichever side starts it and whatever
//! was chosen for it before. To keep a device untrusted, reject its pairing,
//! or set it to Untrust again afterwards.
//!
//! `AuthorizeService` itself still saves nothing: Allow answers one request.

use super::{PairOutcome, pair_outcome};
use futures_util::future::BoxFuture;
use hytte_bus::BusError;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a pairing the device started has, after the user accepted it,
/// to show up as `Paired` before its entry in [`Pairings`] is dropped.
///
/// bluetoothd's own agent timeout (`REQUEST_TIMEOUT`, 60 s, `src/agent.c`):
/// the scale bluetoothd itself gives one step of a pairing that waits on a
/// person. Once the user here has answered, what is left is the other side's
/// answer, the key exchange and the service discovery that starts a second
/// after bonding (`DISCOVERY_TIMER`, `device.c`), all well inside that. A
/// pairing slower than this still completes; its device is then simply left
/// untrusted and asks before using a service.
pub(super) const PAIRING_WINDOW: Duration = Duration::from_mins(1);

/// How often an accepted pairing's watcher looks at the snapshot. The
/// snapshot is in memory, so this costs no bus traffic.
const PAIRED_POLL: Duration = Duration::from_millis(250);

/// The devices whose pairing the user started or accepted and that have not
/// finished, each under the generation of the pairing that put it there.
/// While a device is here, the agent lets its `AuthorizeService` through.
#[derive(Clone, Default)]
pub(crate) struct Pairings {
    entries: Arc<Mutex<HashMap<String, u64>>>,
    next: Arc<AtomicU64>,
}

impl Pairings {
    /// `device` is being paired: replace whatever pairing it had with this
    /// one, and return this one's generation.
    pub(super) fn begin(&self, device: &str) -> u64 {
        let generation = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.lock().insert(device.to_owned(), generation);
        generation
    }

    /// The pairing `generation` of `device` is over: drop its entry, unless a
    /// newer pairing has replaced it.
    pub(super) fn end(&self, device: &str, generation: u64) {
        let mut entries = self.lock();
        if entries.get(device) == Some(&generation) {
            entries.remove(device);
        }
    }

    /// Is `generation` still `device`'s current pairing?
    fn is_current(&self, device: &str, generation: u64) -> bool {
        self.lock().get(device) == Some(&generation)
    }

    /// Is `device` being paired right now?
    pub(super) fn contains(&self, device: &str) -> bool {
        self.lock().contains_key(device)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, u64>> {
        // A panic while holding this lock leaves a map that is still whole.
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// What the watcher's snapshot shows about a device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Shown {
    pub(super) paired: bool,
    pub(super) trusted: bool,
}

/// The `org.bluez.Device1` calls pairing makes, and the snapshot it reads.
/// [`SystemBluez`] makes them on the system bus; the tests record them.
pub(super) trait DeviceBus: Send + Sync {
    fn set_trusted<'a>(&'a self, device: &'a str, on: bool) -> BoxFuture<'a, Result<(), BusError>>;
    fn pair<'a>(&'a self, device: &'a str) -> BoxFuture<'a, Result<(), BusError>>;
    fn connect<'a>(&'a self, device: &'a str) -> BoxFuture<'a, Result<(), BusError>>;
    /// The watcher's snapshot of `device`, `None` if it lists no such device.
    fn shown(&self, device: &str) -> Option<Shown>;
}

/// [`DeviceBus`] on the system bus, reading the service's own snapshot.
pub(super) struct SystemBluez;

impl DeviceBus for SystemBluez {
    fn set_trusted<'a>(&'a self, device: &'a str, on: bool) -> BoxFuture<'a, Result<(), BusError>> {
        Box::pin(super::do_set_device_bool(device, "Trusted", on))
    }

    fn pair<'a>(&'a self, device: &'a str) -> BoxFuture<'a, Result<(), BusError>> {
        Box::pin(super::do_device_call(device, "Pair"))
    }

    fn connect<'a>(&'a self, device: &'a str) -> BoxFuture<'a, Result<(), BusError>> {
        Box::pin(super::do_device_call(device, "Connect"))
    }

    fn shown(&self, device: &str) -> Option<Shown> {
        let shared = super::shared_state()?;
        let devices = shared.devices.lock_ref();
        devices.iter().find(|d| d.path == device).map(|d| Shown {
            paired: d.paired,
            trusted: d.trusted,
        })
    }
}

/// Pair `device` from the shell: in progress while `Pair` runs, trusted and
/// connected once it succeeds, untouched if it fails. Returns what the
/// `Pair` call came to, for [`super::pair_device`]'s toast.
pub(super) async fn pair_then_trust(
    bus: &impl DeviceBus,
    pairings: &Pairings,
    device: &str,
) -> PairOutcome {
    let generation = pairings.begin(device);
    let reply = bus.pair(device).await;
    let outcome = pair_outcome(reply, bus.shown(device).is_some_and(|s| s.paired));
    match &outcome {
        PairOutcome::Paired | PairOutcome::PairedDespite(_) => {
            // Trusted first, then out of the set: a request bluetoothd sends
            // in between finds the device in one or the other.
            if let Err(e) = bus.set_trusted(device, true).await {
                tracing::warn!(error = %e, device, "auto-trust after pair failed");
            }
            pairings.end(device, generation);
            if let Err(e) = bus.connect(device).await {
                tracing::warn!(error = %e, device, "auto-connect after pair failed");
            }
        }
        PairOutcome::Failed(_) => pairings.end(device, generation),
    }
    outcome
}

/// The user accepted `device`'s pairing prompt: put it in progress now,
/// before the agent replies, and trust it once the snapshot shows it paired
/// ([`trust_once_paired`]). Call it only for an accepted prompt; a rejected
/// one must not enter the set.
pub(super) fn accepted(bus: &Arc<dyn DeviceBus>, pairings: &Pairings, device: &str) {
    let generation = pairings.begin(device);
    tokio::spawn(trust_once_paired(
        Arc::clone(bus),
        pairings.clone(),
        device.to_owned(),
        generation,
        PAIRING_WINDOW,
    ));
}

/// Watch the snapshot until `device` is paired (trust it), is lost (removed,
/// or bluetoothd gone), or `window` runs out, and end pairing `generation`
/// either way. Stops at once when a newer pairing of the device replaces it.
///
/// A device the snapshot does not list yet is waited for: bluetoothd creates
/// an incoming device just before it asks the agent, and the watcher can see
/// it a moment later. Only a device seen and then gone counts as removed.
///
/// A device that is already paired when its new pairing is accepted (a
/// re-pairing) is trusted at the first look.
async fn trust_once_paired(
    bus: Arc<dyn DeviceBus>,
    pairings: Pairings,
    device: String,
    generation: u64,
    window: Duration,
) {
    let deadline = tokio::time::Instant::now() + window;
    let mut seen = false;
    loop {
        if !pairings.is_current(&device, generation) {
            return;
        }
        match bus.shown(&device) {
            Some(shown) if shown.paired => {
                if let Err(e) = bus.set_trusted(&device, true).await {
                    tracing::warn!(
                        error = %e,
                        device,
                        "could not trust a device whose pairing the user accepted; it will ask \
                         before using services"
                    );
                }
                pairings.end(&device, generation);
                return;
            }
            Some(_) => seen = true,
            None if seen => {
                tracing::debug!(device, "a device being paired went away; pairing ended");
                pairings.end(&device, generation);
                return;
            }
            None => {}
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::info!(
                device,
                "a pairing the user accepted did not complete in time; nothing was trusted"
            );
            pairings.end(&device, generation);
            return;
        }
        tokio::time::sleep(PAIRED_POLL).await;
    }
}

/// A recording [`DeviceBus`] for the tests here and in `agent.rs`.
#[cfg(test)]
pub(super) mod fake {
    use super::{BoxFuture, BusError, DeviceBus, Shown};
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;

    /// One call the code under test made.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(in super::super) enum Call {
        SetTrusted(bool),
        Pair,
        Connect,
    }

    /// Answers scripted per test; every call recorded in order.
    pub(in super::super) struct FakeBus {
        pub(in super::super) shown: Mutex<Option<Shown>>,
        pub(in super::super) pair_reply: Mutex<Option<Result<(), BusError>>>,
        /// When set, `Pair` answers only once this is notified.
        pub(in super::super) pair_gate: Mutex<Option<Arc<Notify>>>,
        pub(in super::super) calls: Mutex<Vec<Call>>,
    }

    impl FakeBus {
        /// A device the snapshot shows as neither paired nor trusted, whose
        /// `Pair` succeeds at once.
        pub(in super::super) fn new() -> Self {
            Self {
                shown: Mutex::new(Some(Shown::default())),
                pair_reply: Mutex::new(Some(Ok(()))),
                pair_gate: Mutex::new(None),
                calls: Mutex::new(Vec::new()),
            }
        }

        pub(in super::super) fn calls(&self) -> Vec<Call> {
            self.calls.lock().expect("calls").clone()
        }

        pub(in super::super) fn show(&self, shown: Option<Shown>) {
            *self.shown.lock().expect("shown") = shown;
        }

        fn record(&self, call: Call) {
            self.calls.lock().expect("calls").push(call);
        }
    }

    impl DeviceBus for FakeBus {
        fn set_trusted<'a>(
            &'a self,
            _device: &'a str,
            on: bool,
        ) -> BoxFuture<'a, Result<(), BusError>> {
            self.record(Call::SetTrusted(on));
            Box::pin(async { Ok(()) })
        }

        fn pair<'a>(&'a self, _device: &'a str) -> BoxFuture<'a, Result<(), BusError>> {
            self.record(Call::Pair);
            let reply = self
                .pair_reply
                .lock()
                .expect("pair_reply")
                .take()
                .expect("one Pair per scripted reply");
            let gate = self.pair_gate.lock().expect("pair_gate").clone();
            Box::pin(async move {
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                reply
            })
        }

        fn connect<'a>(&'a self, _device: &'a str) -> BoxFuture<'a, Result<(), BusError>> {
            self.record(Call::Connect);
            Box::pin(async { Ok(()) })
        }

        fn shown(&self, _device: &str) -> Option<Shown> {
            *self.shown.lock().expect("shown")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Call, FakeBus};
    use super::{DeviceBus, PAIRING_WINDOW, Pairings, Shown, accepted, pair_then_trust};
    use crate::bluetooth::PairOutcome;
    use hytte_bus::BusError;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Notify;

    const DEVICE: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";

    const PAIRED: Shown = Shown {
        paired: true,
        trusted: false,
    };

    fn refused(name: &str) -> BusError {
        BusError::Permanent {
            reason: name.rsplit('.').next().unwrap_or(name).to_owned(),
            dbus_name: Some(name.to_owned()),
        }
    }

    fn shared(bus: FakeBus) -> (Arc<FakeBus>, Arc<dyn DeviceBus>) {
        let bus = Arc::new(bus);
        let dynamic: Arc<dyn DeviceBus> = bus.clone();
        (bus, dynamic)
    }

    /// Let paused time run `secs` seconds and every task due in it.
    async fn after(secs: u64) {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    /// A pairing that fails or is cancelled leaves bluetoothd's `Trusted`
    /// alone, so bluetoothd can return the device to the temporary state
    /// and forget it, and leaves nothing in progress.
    ///
    /// **Falsification:** set `Trusted` before `Pair` (the previous
    /// design), or forget the `end` in the `Failed` arm, and this fails.
    #[tokio::test]
    async fn a_failed_pairing_leaves_trust_untouched_and_ends_the_pairing() {
        for error in [
            "org.bluez.Error.AuthenticationFailed",
            "org.bluez.Error.AuthenticationCanceled",
            "org.bluez.Error.AuthenticationRejected",
            "org.bluez.Error.ConnectionAttemptFailed",
        ] {
            let bus = FakeBus::new();
            *bus.pair_reply.lock().unwrap() = Some(Err(refused(error)));
            let pairings = Pairings::default();
            let outcome = pair_then_trust(&bus, &pairings, DEVICE).await;
            assert!(matches!(outcome, PairOutcome::Failed(_)), "{error}");
            assert_eq!(bus.calls(), vec![Call::Pair], "{error}");
            assert!(!pairings.contains(DEVICE), "{error}: still in progress");
        }
    }

    /// The device is in progress for as long as `Pair` runs, and only once
    /// `Pair` succeeds is it trusted, then out of the set, then connected.
    ///
    /// **Falsification:** drop the `begin` (nothing in progress during
    /// `Pair`), the `set_trusted` (left untrusted) or the `end` (left in
    /// progress) and this fails.
    #[tokio::test]
    async fn a_shell_pairing_is_in_progress_until_it_succeeds_then_trusted() {
        let bus = FakeBus::new();
        let gate = Arc::new(Notify::new());
        *bus.pair_gate.lock().unwrap() = Some(gate.clone());
        let bus = Arc::new(bus);
        let pairings = Pairings::default();

        let pairing = tokio::spawn({
            let bus = bus.clone();
            let pairings = pairings.clone();
            async move { pair_then_trust(&*bus, &pairings, DEVICE).await }
        });
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert!(pairings.contains(DEVICE), "in progress while Pair runs");
        assert_eq!(bus.calls(), vec![Call::Pair], "nothing trusted yet");

        gate.notify_one();
        let outcome = pairing.await.expect("pairing task");
        assert!(matches!(outcome, PairOutcome::Paired));
        assert_eq!(
            bus.calls(),
            vec![Call::Pair, Call::SetTrusted(true), Call::Connect]
        );
        assert!(!pairings.contains(DEVICE), "no longer in progress");
    }

    /// bluetoothd's "already bonded" counts as paired: trusted and connected.
    #[tokio::test]
    async fn already_bonded_counts_as_success() {
        let bus = FakeBus::new();
        *bus.pair_reply.lock().unwrap() = Some(Err(refused("org.bluez.Error.AlreadyExists")));
        let pairings = Pairings::default();
        assert!(matches!(
            pair_then_trust(&bus, &pairings, DEVICE).await,
            PairOutcome::PairedDespite(_)
        ));
        assert_eq!(
            bus.calls(),
            vec![Call::Pair, Call::SetTrusted(true), Call::Connect]
        );
        assert!(!pairings.contains(DEVICE));
    }

    /// An accepted pairing is in progress until the snapshot shows the device
    /// paired; then, and only then, it is trusted and leaves the set.
    ///
    /// **Falsification:** trust at once instead of waiting for `Paired`, or
    /// never trust, and this fails.
    #[tokio::test(start_paused = true)]
    async fn an_accepted_pairing_is_trusted_once_the_device_shows_paired() {
        let (bus, dynamic) = shared(FakeBus::new());
        let pairings = Pairings::default();
        accepted(&dynamic, &pairings, DEVICE);
        assert!(pairings.contains(DEVICE), "in progress before the reply");

        after(5).await;
        assert!(bus.calls().is_empty(), "nothing trusted before it pairs");
        assert!(pairings.contains(DEVICE));

        bus.show(Some(PAIRED));
        after(1).await;
        assert_eq!(bus.calls(), vec![Call::SetTrusted(true)]);
        assert!(!pairings.contains(DEVICE));
    }

    /// An accepted pairing that never completes ends after the window with
    /// nothing set.
    ///
    /// **Falsification:** drop the deadline and it stays in progress for
    /// good.
    #[tokio::test(start_paused = true)]
    async fn an_accepted_pairing_that_never_completes_ends_without_trust() {
        let (bus, dynamic) = shared(FakeBus::new());
        let pairings = Pairings::default();
        accepted(&dynamic, &pairings, DEVICE);

        after(PAIRING_WINDOW.as_secs() - 1).await;
        assert!(pairings.contains(DEVICE), "not yet");
        after(2).await;
        assert!(!pairings.contains(DEVICE), "the window has passed");
        assert!(bus.calls().is_empty());
    }

    /// A device removed while it is being paired (or bluetoothd going away,
    /// which empties the snapshot) ends the pairing with nothing set.
    ///
    /// **Falsification:** ignore a device that went away and it stays in
    /// progress until the window passes.
    #[tokio::test(start_paused = true)]
    async fn a_device_removed_mid_pairing_ends_it_without_trust() {
        let (bus, dynamic) = shared(FakeBus::new());
        let pairings = Pairings::default();
        accepted(&dynamic, &pairings, DEVICE);
        after(1).await;
        bus.show(None);
        after(1).await;
        assert!(!pairings.contains(DEVICE));
        assert!(bus.calls().is_empty());
    }

    /// A device the snapshot does not list yet (bluetoothd created it just
    /// before asking the agent) is waited for, not taken for removed.
    ///
    /// **Falsification:** treat "not listed" as removed from the start and
    /// the pairing ends before the device shows up.
    #[tokio::test(start_paused = true)]
    async fn a_device_not_listed_yet_is_waited_for() {
        let bus = FakeBus::new();
        bus.show(None);
        let (bus, dynamic) = shared(bus);
        let pairings = Pairings::default();
        accepted(&dynamic, &pairings, DEVICE);
        after(2).await;
        assert!(pairings.contains(DEVICE));

        bus.show(Some(PAIRED));
        after(1).await;
        assert_eq!(bus.calls(), vec![Call::SetTrusted(true)]);
        assert!(!pairings.contains(DEVICE));
    }

    /// LOW-A, first case: the device's first pairing fails, it tries again
    /// after a while, the user accepts again, and the second one completes.
    /// The device ends trusted.
    #[tokio::test(start_paused = true)]
    async fn a_device_that_retries_after_a_failed_pairing_ends_trusted() {
        let (bus, dynamic) = shared(FakeBus::new());
        let pairings = Pairings::default();
        accepted(&dynamic, &pairings, DEVICE);
        after(100).await;
        assert!(!pairings.contains(DEVICE), "the first attempt has lapsed");

        accepted(&dynamic, &pairings, DEVICE);
        after(25).await;
        bus.show(Some(PAIRED));
        after(1).await;
        assert_eq!(bus.calls(), vec![Call::SetTrusted(true)]);
        assert!(!pairings.contains(DEVICE));
    }

    /// LOW-A, second case: the user clicks Pair while an incoming pairing is
    /// still pending, and the shell's `Pair` outlasts the incoming one's
    /// window. The older pairing's deadline must not end the newer one, and
    /// the device ends trusted.
    ///
    /// **Falsification:** drop the generations (`is_current` always true and
    /// `end` removing whatever entry is there) and the incoming pairing's
    /// deadline ends the shell's pairing while its `Pair` is still running.
    #[tokio::test(start_paused = true)]
    async fn clicking_pair_during_an_incoming_pairing_ends_trusted() {
        let bus = FakeBus::new();
        let gate = Arc::new(Notify::new());
        *bus.pair_gate.lock().unwrap() = Some(gate.clone());
        let (bus, dynamic) = shared(bus);
        let pairings = Pairings::default();

        accepted(&dynamic, &pairings, DEVICE);
        after(30).await;
        let pairing = tokio::spawn({
            let bus = bus.clone();
            let pairings = pairings.clone();
            async move { pair_then_trust(&*bus, &pairings, DEVICE).await }
        });
        after(PAIRING_WINDOW.as_secs()).await;
        assert!(
            pairings.contains(DEVICE),
            "the shell's pairing is still running"
        );

        bus.show(Some(PAIRED));
        gate.notify_one();
        let outcome = pairing.await.expect("pairing task");
        assert!(matches!(outcome, PairOutcome::Paired));
        after(1).await;
        assert_eq!(
            bus.calls(),
            vec![Call::Pair, Call::SetTrusted(true), Call::Connect]
        );
        assert!(!pairings.contains(DEVICE));
    }

    /// The set itself: a newer pairing's entry survives the older one ending.
    #[test]
    fn a_newer_pairing_outlives_the_older_one_ending() {
        let pairings = Pairings::default();
        let first = pairings.begin(DEVICE);
        let second = pairings.begin(DEVICE);
        pairings.end(DEVICE, first);
        assert!(pairings.contains(DEVICE));
        pairings.end(DEVICE, second);
        assert!(!pairings.contains(DEVICE));
    }
}
