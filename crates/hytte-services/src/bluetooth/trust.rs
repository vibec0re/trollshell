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
//! The shell's `Pair` and the agent hold **one** set: the service builds
//! both from `BluetoothShared::with_agent`, and `pair_device` pairs through
//! the published state's set or not at all (#1611, LOW-1). A private set on
//! either side would bring back the "Allow …?" prompt right after the user
//! pairs a headset.
//!
//! Each entry is tagged with where it came from ([`Origin`]). A newer
//! pairing for the same device replaces the older one's entry
//! (generations), so a retry is never ended by the attempt before it, with
//! one exception: an accepted prompt never replaces the entry of a `Pair`
//! the shell is running (#1611, NIT-1). That prompt is usually the
//! confirmation of the very same `Pair`, and its entry would end after
//! [`PAIRING_WINDOW`] while `Pair` can run for minutes; the `Pair` entry
//! already covers the device until the call returns.
//!
//! **Trusted first, then out of the set**, on both paths: whenever a device
//! leaves the set after a pairing succeeded, bluetoothd has already answered
//! the `Set Trusted` (#1611, LOW-2). That alone does not close the race, since
//! the agent checks the set only after its caller check's round trip to the
//! bus: a request bluetoothd sent before it applied the `Set` can reach the
//! check after the entry is gone. So when the set misses, the agent reads
//! the device's `Trusted` from bluetoothd before it prompts
//! ([`DeviceBus::trusted`]), and a device that is trusted by then is let
//! through. A set miss means the entry ended before the check, so the `Set`
//! was answered before the read is sent: a device whose pairing succeeded,
//! and whose `Set Trusted` bluetoothd took, is found in one or the other
//! whatever the timing.
//!
//! **A new pairing is a new decision.** If the user set a device to Untrust
//! in its menu and later pairs it again (accepting the device's pairing
//! prompt, or choosing Pair here), it ends up trusted again. Accepting a
//! pairing means trusting the device, whichever side starts it and whatever
//! was chosen for it before. To keep a device untrusted, reject its pairing,
//! or set it to Untrust again afterwards.
//!
//! A re-pairing the user accepts on a device that is **already paired** is
//! trusted at the watcher's first look at the device list, since the list
//! already shows the device `Paired` from its old bond, even if the new
//! pairing then fails (#1611, NIT-2). That is harmless today: a failed
//! re-pairing keeps the old bond, so bluetoothd does not return the device to
//! the temporary state, a spoofer whose re-pairing fails gets no link key and
//! so nothing the trust would let it use, and `BlueZ`'s default
//! `JustWorksRepairing=never` refuses a Just Works re-pairing before any
//! prompt (`src/main.c`, `src/device.c`). It does mean that a device set to
//! Untrust becomes trusted again after an accepted re-pairing that failed.
//!
//! `AuthorizeService` itself still saves nothing: Allow answers one request.

use super::{PairOutcome, pair_outcome};
use futures_util::future::BoxFuture;
use hytte_bus::{BusError, BusKind, RetryPolicy};
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

/// How long the agent waits for bluetoothd's own `Trusted` before it prompts
/// anyway ([`DeviceBus::trusted`]). bluetoothd answers a property read at
/// once; this bounds a bluetoothd that has stopped answering, well inside
/// the 60 s it gives the user to answer the prompt that follows.
const TRUSTED_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Where a [`Pairings`] entry came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    /// The shell's own `Pair` call ([`pair_then_trust`]). The entry lasts as
    /// long as the call.
    Pair,
    /// A pairing prompt the user accepted ([`accepted`]). The entry lasts
    /// until the snapshot shows the device paired or loses it, or until
    /// [`PAIRING_WINDOW`] has passed.
    Accepted,
}

/// One device's entry in [`Pairings`]: the pairing that put it there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    generation: u64,
    origin: Origin,
}

/// The devices whose pairing the user started or accepted and that have not
/// finished, each under the generation and the [`Origin`] of the pairing
/// that put it there. While a device is here, the agent lets its
/// `AuthorizeService` through.
#[derive(Clone, Default)]
pub(crate) struct Pairings {
    entries: Arc<Mutex<HashMap<String, Entry>>>,
    next: Arc<AtomicU64>,
}

impl Pairings {
    /// The shell starts pairing `device`: this pairing replaces whatever
    /// entry the device had, and its generation is returned.
    pub(super) fn begin_pair(&self, device: &str) -> u64 {
        let mut entries = self.lock();
        let generation = self.next_generation();
        entries.insert(
            device.to_owned(),
            Entry {
                generation,
                origin: Origin::Pair,
            },
        );
        generation
    }

    /// The user accepted `device`'s pairing prompt: this pairing replaces an
    /// earlier accepted one, and its generation is returned.
    ///
    /// `None` while the shell's own `Pair` of the device is running: that
    /// entry covers the device until `Pair` returns, and this one would end
    /// after [`PAIRING_WINDOW`], possibly with `Pair` still running (#1611,
    /// NIT-1). The prompt is then usually that `Pair`'s own confirmation.
    pub(super) fn begin_accepted(&self, device: &str) -> Option<u64> {
        let mut entries = self.lock();
        if entries
            .get(device)
            .is_some_and(|entry| entry.origin == Origin::Pair)
        {
            return None;
        }
        let generation = self.next_generation();
        entries.insert(
            device.to_owned(),
            Entry {
                generation,
                origin: Origin::Accepted,
            },
        );
        Some(generation)
    }

    /// The pairing `generation` of `device` is over: drop its entry, unless a
    /// newer pairing has replaced it.
    pub(super) fn end(&self, device: &str, generation: u64) {
        let mut entries = self.lock();
        if entries
            .get(device)
            .is_some_and(|entry| entry.generation == generation)
        {
            entries.remove(device);
        }
    }

    /// Is `generation` still `device`'s current pairing?
    fn is_current(&self, device: &str, generation: u64) -> bool {
        self.lock()
            .get(device)
            .is_some_and(|entry| entry.generation == generation)
    }

    /// Is `device` being paired right now?
    pub(super) fn contains(&self, device: &str) -> bool {
        self.lock().contains_key(device)
    }

    /// Where `device`'s current entry came from, if it has one.
    #[cfg(test)]
    pub(super) fn origin(&self, device: &str) -> Option<Origin> {
        self.lock().get(device).map(|entry| entry.origin)
    }

    fn next_generation(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
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
    /// bluetoothd's own `Trusted` for `device`, read from bluetoothd now
    /// rather than from the snapshot, which can lag it.
    ///
    /// The read goes to `org.bluez`, and a call to a name nobody owns
    /// D-Bus-activates its service, so call it only while bluetoothd is
    /// known to be running.
    fn trusted<'a>(&'a self, device: &'a str) -> BoxFuture<'a, Result<bool, BusError>>;
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

    fn trusted<'a>(&'a self, device: &'a str) -> BoxFuture<'a, Result<bool, BusError>> {
        Box::pin(read_trusted(device))
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

/// `Properties.Get(org.bluez.Device1, Trusted)` on the pooled system
/// connection, bounded by [`TRUSTED_READ_TIMEOUT`] and never retried: a read
/// that fails or is slow costs the caller a prompt, which is the safe answer.
async fn read_trusted(device: &str) -> Result<bool, BusError> {
    let value: zbus::zvariant::OwnedValue = hytte_bus::call(BusKind::System, "org.bluez")
        .at_path(device.to_owned())
        .iface("org.freedesktop.DBus.Properties")
        .method("Get")
        .args(("org.bluez.Device1".to_owned(), "Trusted".to_owned()))
        .timeout(TRUSTED_READ_TIMEOUT)
        .retry(RetryPolicy::Never)
        .send::<zbus::zvariant::OwnedValue>()
        .await?;
    bool::try_from(value).map_err(|e| BusError::Permanent {
        reason: format!("Device1.Trusted is not a boolean: {e}"),
        dbus_name: None,
    })
}

/// Pair `device` from the shell: in progress while `Pair` runs, trusted and
/// connected once it succeeds, untouched if it fails. Returns what the
/// `Pair` call came to, for [`super::pair_device`]'s toast.
pub(super) async fn pair_then_trust(
    bus: &impl DeviceBus,
    pairings: &Pairings,
    device: &str,
) -> PairOutcome {
    let generation = pairings.begin_pair(device);
    let reply = bus.pair(device).await;
    let outcome = pair_outcome(reply, bus.shown(device).is_some_and(|s| s.paired));
    match &outcome {
        PairOutcome::Paired | PairOutcome::PairedDespite(_) => {
            // Trusted first, then out of the set: the device leaves only once
            // bluetoothd has answered the `Set`, so the agent finds it either
            // in the set or trusted in bluetoothd (the module doc's LOW-2).
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
///
/// While the shell's own `Pair` of the device runs, this does nothing: that
/// `Pair` keeps the device in progress until it returns and trusts it if it
/// succeeded ([`Pairings::begin_accepted`]).
pub(super) fn accepted(bus: &Arc<dyn DeviceBus>, pairings: &Pairings, device: &str) {
    let Some(generation) = pairings.begin_accepted(device) else {
        tracing::debug!(
            device,
            "an accepted pairing prompt during the shell's own Pair; the Pair covers the device"
        );
        return;
    };
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
/// re-pairing) is trusted at the first look, even if the new pairing then
/// fails (the module doc says why that is harmless).
///
/// Here too the device is trusted first and leaves the set after: `end` runs
/// only once bluetoothd has answered the `Set`.
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
    use super::{BoxFuture, BusError, DeviceBus, Pairings, Shown};
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;

    /// One call the code under test made.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(in super::super) enum Call {
        SetTrusted(bool),
        Pair,
        Connect,
    }

    /// Answers scripted per test; every call recorded in order. A read of
    /// `Trusted` is not a call: it changes nothing, and is counted apart.
    pub(in super::super) struct FakeBus {
        pub(in super::super) shown: Mutex<Option<Shown>>,
        pub(in super::super) pair_reply: Mutex<Option<Result<(), BusError>>>,
        /// When set, `Pair` answers only once this is notified.
        pub(in super::super) pair_gate: Mutex<Option<Arc<Notify>>>,
        pub(in super::super) calls: Mutex<Vec<Call>>,
        /// bluetoothd's own `Trusted`, what a live read answers; `None` makes
        /// the read fail. A `SetTrusted` changes it, as bluetoothd's setter
        /// does before it replies.
        pub(in super::super) live_trusted: Mutex<Option<bool>>,
        /// How many live reads of `Trusted` were made.
        pub(in super::super) trusted_reads: Mutex<usize>,
        /// When set, each `SetTrusted` records in [`Self::in_set_at_trust`]
        /// whether the device was in this set at that moment.
        pub(in super::super) watched: Mutex<Option<Pairings>>,
        pub(in super::super) in_set_at_trust: Mutex<Vec<bool>>,
    }

    impl FakeBus {
        /// A device the snapshot shows as neither paired nor trusted, whose
        /// `Pair` succeeds at once and whose live `Trusted` reads `false`.
        pub(in super::super) fn new() -> Self {
            Self {
                shown: Mutex::new(Some(Shown::default())),
                pair_reply: Mutex::new(Some(Ok(()))),
                pair_gate: Mutex::new(None),
                calls: Mutex::new(Vec::new()),
                live_trusted: Mutex::new(Some(false)),
                trusted_reads: Mutex::new(0),
                watched: Mutex::new(None),
                in_set_at_trust: Mutex::new(Vec::new()),
            }
        }

        pub(in super::super) fn calls(&self) -> Vec<Call> {
            self.calls.lock().expect("calls").clone()
        }

        pub(in super::super) fn show(&self, shown: Option<Shown>) {
            *self.shown.lock().expect("shown") = shown;
        }

        /// From now on, record at each `SetTrusted` whether the device is in
        /// `pairings`.
        pub(in super::super) fn watch(&self, pairings: &Pairings) {
            *self.watched.lock().expect("watched") = Some(pairings.clone());
        }

        /// Whether the device was in the watched set at each `SetTrusted`.
        pub(in super::super) fn in_set_at_trust(&self) -> Vec<bool> {
            self.in_set_at_trust
                .lock()
                .expect("in_set_at_trust")
                .clone()
        }

        pub(in super::super) fn set_live_trusted(&self, trusted: Option<bool>) {
            *self.live_trusted.lock().expect("live_trusted") = trusted;
        }

        pub(in super::super) fn trusted_reads(&self) -> usize {
            *self.trusted_reads.lock().expect("trusted_reads")
        }

        fn record(&self, call: Call) {
            self.calls.lock().expect("calls").push(call);
        }
    }

    impl DeviceBus for FakeBus {
        fn set_trusted<'a>(
            &'a self,
            device: &'a str,
            on: bool,
        ) -> BoxFuture<'a, Result<(), BusError>> {
            self.record(Call::SetTrusted(on));
            if let Some(pairings) = self.watched.lock().expect("watched").as_ref() {
                self.in_set_at_trust
                    .lock()
                    .expect("in_set_at_trust")
                    .push(pairings.contains(device));
            }
            self.set_live_trusted(Some(on));
            Box::pin(async { Ok(()) })
        }

        fn trusted<'a>(&'a self, _device: &'a str) -> BoxFuture<'a, Result<bool, BusError>> {
            *self.trusted_reads.lock().expect("trusted_reads") += 1;
            let answer = self
                .live_trusted
                .lock()
                .expect("live_trusted")
                .ok_or_else(|| BusError::Permanent {
                    reason: "scripted read failure".to_owned(),
                    dbus_name: Some("org.freedesktop.DBus.Error.NoReply".to_owned()),
                });
            Box::pin(async move { answer })
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
    use super::{DeviceBus, Origin, PAIRING_WINDOW, Pairings, Shown, accepted, pair_then_trust};
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

    /// The set itself: a newer pairing's entry survives the older one ending,
    /// whichever kind each is (except an accept after a `Pair`, below).
    #[test]
    fn a_newer_pairing_outlives_the_older_one_ending() {
        let pairings = Pairings::default();
        let first = pairings.begin_pair(DEVICE);
        let second = pairings.begin_pair(DEVICE);
        pairings.end(DEVICE, first);
        assert!(pairings.contains(DEVICE));
        pairings.end(DEVICE, second);
        assert!(!pairings.contains(DEVICE));

        let first = pairings.begin_accepted(DEVICE).expect("no Pair running");
        let second = pairings.begin_accepted(DEVICE).expect("no Pair running");
        pairings.end(DEVICE, first);
        assert!(pairings.contains(DEVICE), "a retried accept outlives the first");
        pairings.end(DEVICE, second);
        assert!(!pairings.contains(DEVICE));

        let accepted = pairings.begin_accepted(DEVICE).expect("no Pair running");
        let pair = pairings.begin_pair(DEVICE);
        pairings.end(DEVICE, accepted);
        assert_eq!(
            pairings.origin(DEVICE),
            Some(Origin::Pair),
            "a Pair replaces an accepted entry, and outlives its end"
        );
        pairings.end(DEVICE, pair);
        assert!(!pairings.contains(DEVICE));
    }

    /// The set itself (#1611, NIT-1): an accept while the shell's `Pair` runs
    /// leaves the `Pair` entry as it is and starts nothing of its own.
    ///
    /// **Falsification:** let `begin_accepted` replace whatever entry is
    /// there, origin regardless, and the entry turns into an accepted one.
    #[test]
    fn an_accept_never_replaces_a_pair_entry() {
        let pairings = Pairings::default();
        let pair = pairings.begin_pair(DEVICE);
        assert_eq!(pairings.begin_accepted(DEVICE), None);
        assert_eq!(pairings.origin(DEVICE), Some(Origin::Pair));
        pairings.end(DEVICE, pair);
        assert!(!pairings.contains(DEVICE), "the Pair ends its own entry");
    }

    /// LOW-2 (#1611), the shell's path: when `pair_then_trust` sets `Trusted`,
    /// the device is still in the set, so a service request is covered by
    /// the set until bluetoothd has answered the `Set`, and by bluetoothd's
    /// own `Trusted` after.
    ///
    /// **Falsification:** in `pair_then_trust`, end the entry before the
    /// `set_trusted` call (the review's Q7) and the device is out of the set
    /// at the `Set`.
    #[tokio::test]
    async fn the_shell_trusts_before_it_ends_its_pairing() {
        let bus = FakeBus::new();
        let pairings = Pairings::default();
        bus.watch(&pairings);
        let outcome = pair_then_trust(&bus, &pairings, DEVICE).await;
        assert!(matches!(outcome, PairOutcome::Paired));
        assert_eq!(
            bus.calls(),
            vec![Call::Pair, Call::SetTrusted(true), Call::Connect]
        );
        assert_eq!(
            bus.in_set_at_trust(),
            vec![true],
            "the device must still be in the set when it is trusted"
        );
        assert!(!pairings.contains(DEVICE), "and out of it after");
    }

    /// LOW-2 (#1611), the watcher's path: when an accepted pairing's watcher
    /// sets `Trusted`, the device is still in the set.
    ///
    /// **Falsification:** in `trust_once_paired`, end the entry before the
    /// `set_trusted` call (the review's Q8) and the device is out of the set
    /// at the `Set`.
    #[tokio::test(start_paused = true)]
    async fn the_watcher_trusts_before_it_ends_its_pairing() {
        let (bus, dynamic) = shared(FakeBus::new());
        let pairings = Pairings::default();
        bus.watch(&pairings);
        accepted(&dynamic, &pairings, DEVICE);
        after(1).await;
        bus.show(Some(PAIRED));
        after(1).await;
        assert_eq!(bus.calls(), vec![Call::SetTrusted(true)]);
        assert_eq!(
            bus.in_set_at_trust(),
            vec![true],
            "the device must still be in the set when it is trusted"
        );
        assert!(!pairings.contains(DEVICE), "and out of it after");
    }

    /// NIT-1 (#1611): the user confirms the device's code during the shell's
    /// own `Pair`, and the other side takes longer than [`PAIRING_WINDOW`]
    /// after that. The device stays in progress for as long as `Pair` runs,
    /// and `Pair`'s success is what trusts it.
    ///
    /// The review's scratch probe: `Pair` at t=0, accept at t=5, and at
    /// t=69 the device had left the set with `Pair` still running.
    ///
    /// **Falsification:** let an accept replace a `Pair` entry
    /// (`begin_accepted` ignoring the origin) and the accept's window ends
    /// the shell's cover at t=65.
    #[tokio::test(start_paused = true)]
    async fn an_accept_during_the_shells_pair_does_not_shorten_it() {
        let bus = FakeBus::new();
        let gate = Arc::new(Notify::new());
        *bus.pair_gate.lock().unwrap() = Some(gate.clone());
        let (bus, dynamic) = shared(bus);
        let pairings = Pairings::default();

        let pairing = tokio::spawn({
            let bus = bus.clone();
            let pairings = pairings.clone();
            async move { pair_then_trust(&*bus, &pairings, DEVICE).await }
        });
        after(5).await;
        // The user confirms the code of the shell's own pairing.
        accepted(&dynamic, &pairings, DEVICE);
        after(PAIRING_WINDOW.as_secs() + 4).await;
        assert!(
            pairings.contains(DEVICE),
            "Pair is still running at t=69, so the device is still in progress"
        );
        assert_eq!(bus.calls(), vec![Call::Pair], "nothing trusted yet");

        bus.show(Some(PAIRED));
        gate.notify_one();
        let outcome = pairing.await.expect("pairing task");
        assert!(matches!(outcome, PairOutcome::Paired));
        after(1).await;
        assert_eq!(
            bus.calls(),
            vec![Call::Pair, Call::SetTrusted(true), Call::Connect],
            "trusted once, by the Pair"
        );
        assert!(!pairings.contains(DEVICE));
    }
}
