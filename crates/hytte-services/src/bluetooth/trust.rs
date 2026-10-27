//! Pairing a device makes it `Trusted`, whichever side starts the pairing
//! (#1603 review, MED-1).
//!
//! bluetoothd asks the agent (`AuthorizeService`) before an untrusted device
//! may use a service, and lets a `Trusted` one through without asking
//! (`process_auth_queue` in `src/adapter.c`, `BlueZ` 5.87). Nothing in
//! bluetoothd sets `Trusted` by itself: its only setter is the D-Bus property
//! (`device.c`). So the shell sets it, in the two places a user agrees to a
//! pairing, and only devices the user deliberately left untrusted ever ask:
//!
//! - **The shell pairs a device** ([`pair_trusting`]). `Trusted` goes on
//!   **before** `Pair`, because bluetoothd answers `Pair` and runs the
//!   service authorizations it queued during the pairing in the same
//!   function (`device_svc_resolved`), before a `Set Trusted` sent after the
//!   reply could arrive. A headset that opens a profile while it pairs would
//!   otherwise ask "Allow …?" right after the user agreed to pair it. If
//!   `Pair` fails or is cancelled, `Trusted` goes back off, unless the device
//!   was trusted already.
//! - **The device pairs with us** ([`trust_on_accept`]). When the user
//!   accepts the agent's pairing prompt (confirmation, authorization, PIN or
//!   passkey), `Trusted` goes on before the agent replies, for the same
//!   reason. bluetoothd is waiting on the agent asynchronously, so it serves
//!   the `Set` meanwhile. If the device is still not paired
//!   [`PAIRING_SETTLE`] later, `Trusted` goes back off. A rejected prompt
//!   sets nothing.
//!
//! `AuthorizeService` itself still saves nothing: Allow answers one request.

use super::{PairOutcome, pair_outcome};
use futures_util::future::BoxFuture;
use hytte_bus::{BusError, BusKind};
use std::sync::Arc;
use std::time::Duration;

/// How long a device the user agreed to pair from its side has to finish
/// pairing before its `Trusted` is taken back. Twice bluetoothd's own 60 s
/// agent timeout (`REQUEST_TIMEOUT`, `src/agent.c`), so a pairing still
/// waiting on the other side's confirmation is not cut short.
pub(super) const PAIRING_SETTLE: Duration = Duration::from_mins(2);

/// What the watcher's snapshot shows about a device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Shown {
    pub(super) paired: bool,
    pub(super) trusted: bool,
}

/// Whether a device is paired, asked of bluetoothd itself rather than read
/// from the snapshot (whose `Paired` arrives only once services resolve).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pairing {
    Paired,
    NotPaired,
    /// No answer: bluetoothd is not running, the device is gone, or the call
    /// failed. Leave `Trusted` as it is, since a call to `org.bluez` with no
    /// owner would start a bluetoothd that was stopped on purpose.
    Unknown,
}

/// The `org.bluez.Device1` calls pairing makes. [`SystemBluez`] makes them on
/// the system bus; the tests record them.
pub(super) trait DeviceBus: Send + Sync {
    fn set_trusted<'a>(&'a self, device: &'a str, on: bool) -> BoxFuture<'a, Result<(), BusError>>;
    fn pair<'a>(&'a self, device: &'a str) -> BoxFuture<'a, Result<(), BusError>>;
    fn connect<'a>(&'a self, device: &'a str) -> BoxFuture<'a, Result<(), BusError>>;
    fn pairing<'a>(&'a self, device: &'a str) -> BoxFuture<'a, Pairing>;
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

    fn pairing<'a>(&'a self, device: &'a str) -> BoxFuture<'a, Pairing> {
        Box::pin(async move {
            // Ask the bus first, never `org.bluez`: a call to a bluetoothd
            // that is not running would start it (see `devices::wait_for_bluez`).
            if !matches!(super::devices::bluez_has_owner().await, Ok(true)) {
                return Pairing::Unknown;
            }
            let paired = hytte_bus::call(BusKind::System, "org.bluez")
                .at_path(device.to_owned())
                .iface("org.freedesktop.DBus.Properties")
                .method("Get")
                .args(("org.bluez.Device1".to_owned(), "Paired".to_owned()))
                .send::<zbus::zvariant::OwnedValue>()
                .await
                .ok()
                .and_then(|value| bool::try_from(value).ok());
            match paired {
                Some(true) => Pairing::Paired,
                Some(false) => Pairing::NotPaired,
                None => Pairing::Unknown,
            }
        })
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

/// Pair `device` from the shell, trusting it first; then connect it, or
/// take the trust back if the pairing did not happen. Returns what the
/// `Pair` call came to, for [`super::pair_device`]'s toast.
///
/// A device already trusted is left trusted whatever happens. When the
/// first `Set Trusted` fails, the pairing goes ahead and trust is set
/// after it instead, as before this existed.
pub(super) async fn pair_trusting(bus: &impl DeviceBus, device: &str) -> PairOutcome {
    let was_trusted = bus.shown(device).is_some_and(|s| s.trusted);
    let mut trusted = was_trusted;
    if !was_trusted {
        match bus.set_trusted(device, true).await {
            Ok(()) => trusted = true,
            Err(e) => tracing::warn!(
                error = %e,
                device,
                "could not trust the device before pairing; trusting it after instead"
            ),
        }
    }

    let reply = bus.pair(device).await;
    let outcome = pair_outcome(reply, bus.shown(device).is_some_and(|s| s.paired));
    match &outcome {
        PairOutcome::Paired | PairOutcome::PairedDespite(_) => {
            if !trusted && let Err(e) = bus.set_trusted(device, true).await {
                tracing::warn!(error = %e, device, "auto-trust after pair failed");
            }
            if let Err(e) = bus.connect(device).await {
                tracing::warn!(error = %e, device, "auto-connect after pair failed");
            }
        }
        PairOutcome::Failed(_) => {
            if trusted
                && !was_trusted
                && let Err(e) = bus.set_trusted(device, false).await
            {
                tracing::warn!(
                    error = %e,
                    device,
                    "could not take back the trust set for a pairing that failed"
                );
            }
        }
    }
    outcome
}

/// The user accepted `device`'s pairing prompt: trust it now, before the
/// agent replies, and take that back if the device is still not paired
/// [`PAIRING_SETTLE`] later. A device already trusted is left alone.
///
/// Call it only for an accepted prompt. A rejected one must set nothing.
pub(super) async fn trust_on_accept(bus: &Arc<dyn DeviceBus>, device: &str) {
    if bus.shown(device).is_some_and(|s| s.trusted) {
        return;
    }
    if let Err(e) = bus.set_trusted(device, true).await {
        tracing::warn!(
            error = %e,
            device,
            "could not trust a device whose pairing the user accepted; it will ask before using services"
        );
        return;
    }
    tokio::spawn(keep_trust_if_paired(
        Arc::clone(bus),
        device.to_owned(),
        PAIRING_SETTLE,
    ));
}

/// After `settle`, take `device`'s trust back unless it is paired by then.
async fn keep_trust_if_paired(bus: Arc<dyn DeviceBus>, device: String, settle: Duration) {
    tokio::time::sleep(settle).await;
    match bus.pairing(&device).await {
        Pairing::Paired | Pairing::Unknown => {}
        Pairing::NotPaired => {
            tracing::info!(
                device,
                "a pairing the user accepted did not complete; taking its trust back"
            );
            if let Err(e) = bus.set_trusted(&device, false).await {
                tracing::warn!(
                    error = %e,
                    device,
                    "could not take back the trust set for a pairing that did not complete"
                );
            }
        }
    }
}

/// A recording [`DeviceBus`] for the tests here and in `agent.rs`.
#[cfg(test)]
pub(super) mod fake {
    use super::{BoxFuture, BusError, DeviceBus, Pairing, Shown};
    use std::sync::Mutex;

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
        pub(in super::super) pairing: Mutex<Pairing>,
        pub(in super::super) calls: Mutex<Vec<Call>>,
    }

    impl FakeBus {
        /// A device the snapshot shows as neither paired nor trusted, whose
        /// `Pair` succeeds and which bluetoothd reports paired.
        pub(in super::super) fn new() -> Self {
            Self {
                shown: Mutex::new(Some(Shown::default())),
                pair_reply: Mutex::new(Some(Ok(()))),
                pairing: Mutex::new(Pairing::Paired),
                calls: Mutex::new(Vec::new()),
            }
        }

        pub(in super::super) fn calls(&self) -> Vec<Call> {
            self.calls.lock().expect("calls").clone()
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
                .expect("one Pair per test");
            Box::pin(async move { reply })
        }

        fn connect<'a>(&'a self, _device: &'a str) -> BoxFuture<'a, Result<(), BusError>> {
            self.record(Call::Connect);
            Box::pin(async { Ok(()) })
        }

        fn pairing<'a>(&'a self, _device: &'a str) -> BoxFuture<'a, Pairing> {
            let pairing = *self.pairing.lock().expect("pairing");
            Box::pin(async move { pairing })
        }

        fn shown(&self, _device: &str) -> Option<Shown> {
            *self.shown.lock().expect("shown")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{Call, FakeBus};
    use super::{DeviceBus, PAIRING_SETTLE, Pairing, Shown, pair_trusting, trust_on_accept};
    use crate::bluetooth::PairOutcome;
    use hytte_bus::BusError;
    use std::sync::Arc;
    use std::time::Duration;

    const DEVICE: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";

    fn refused(name: &str) -> BusError {
        BusError::Permanent {
            reason: name.rsplit('.').next().unwrap_or(name).to_owned(),
            dbus_name: Some(name.to_owned()),
        }
    }

    /// The shell trusts a device **before** asking bluetoothd to pair it, so
    /// a service the device opens during the pairing is not asked about;
    /// after a successful pair it connects, and sets nothing twice.
    ///
    /// **Falsification:** move the first `set_trusted` after `pair` (or drop
    /// it, leaving the old post-pair trust) and the order assertion fails.
    #[tokio::test]
    async fn the_shell_trusts_a_device_before_pairing_it() {
        let bus = FakeBus::new();
        let outcome = pair_trusting(&bus, DEVICE).await;
        assert!(matches!(outcome, PairOutcome::Paired));
        assert_eq!(
            bus.calls(),
            vec![Call::SetTrusted(true), Call::Pair, Call::Connect]
        );
    }

    /// A pairing that fails, including one the user cancelled, takes the
    /// trust back.
    ///
    /// **Falsification:** drop the revert in the `Failed` arm and the
    /// device is left trusted: the last call is `Pair`.
    #[tokio::test]
    async fn a_failed_or_cancelled_pairing_takes_the_trust_back() {
        for error in [
            "org.bluez.Error.AuthenticationFailed",
            "org.bluez.Error.AuthenticationCanceled",
            "org.bluez.Error.AuthenticationRejected",
        ] {
            let bus = FakeBus::new();
            *bus.pair_reply.lock().unwrap() = Some(Err(refused(error)));
            let outcome = pair_trusting(&bus, DEVICE).await;
            assert!(matches!(outcome, PairOutcome::Failed(_)), "{error}");
            assert_eq!(
                bus.calls(),
                vec![Call::SetTrusted(true), Call::Pair, Call::SetTrusted(false)],
                "{error}"
            );
        }
    }

    /// A device the user had trusted already stays trusted: nothing is set
    /// before the pairing, and a failure takes nothing back.
    #[tokio::test]
    async fn a_trusted_device_stays_trusted_whatever_the_pairing_does() {
        let bus = FakeBus::new();
        *bus.shown.lock().unwrap() = Some(Shown {
            paired: false,
            trusted: true,
        });
        *bus.pair_reply.lock().unwrap() =
            Some(Err(refused("org.bluez.Error.AuthenticationFailed")));
        assert!(matches!(
            pair_trusting(&bus, DEVICE).await,
            PairOutcome::Failed(_)
        ));
        assert_eq!(bus.calls(), vec![Call::Pair]);
    }

    /// bluetoothd's "already bonded" still counts as paired: trust stays,
    /// and the device connects.
    #[tokio::test]
    async fn already_bonded_keeps_the_trust_and_connects() {
        let bus = FakeBus::new();
        *bus.pair_reply.lock().unwrap() = Some(Err(refused("org.bluez.Error.AlreadyExists")));
        assert!(matches!(
            pair_trusting(&bus, DEVICE).await,
            PairOutcome::PairedDespite(_)
        ));
        assert_eq!(
            bus.calls(),
            vec![Call::SetTrusted(true), Call::Pair, Call::Connect]
        );
    }

    fn shared(bus: FakeBus) -> (Arc<FakeBus>, Arc<dyn DeviceBus>) {
        let bus = Arc::new(bus);
        let dynamic: Arc<dyn DeviceBus> = bus.clone();
        (bus, dynamic)
    }

    /// Run the tasks `trust_on_accept` spawned until they are idle, past
    /// [`PAIRING_SETTLE`] (paused time jumps the clock).
    async fn past_the_settle() {
        tokio::time::sleep(PAIRING_SETTLE + Duration::from_secs(1)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    /// Accepting a device's pairing trusts it at once, and a pairing that
    /// completes keeps it trusted.
    #[tokio::test(start_paused = true)]
    async fn an_accepted_pairing_trusts_the_device_and_keeps_it() {
        let (bus, dynamic) = shared(FakeBus::new());
        trust_on_accept(&dynamic, DEVICE).await;
        assert_eq!(
            bus.calls(),
            vec![Call::SetTrusted(true)],
            "trusted before the agent replies"
        );
        past_the_settle().await;
        assert_eq!(bus.calls(), vec![Call::SetTrusted(true)], "kept");
    }

    /// A pairing the user accepted that never completes gives the trust back
    /// once [`PAIRING_SETTLE`] has passed, and not before.
    ///
    /// **Falsification:** drop the `keep_trust_if_paired` task and the
    /// device stays trusted.
    #[tokio::test(start_paused = true)]
    async fn an_accepted_pairing_that_never_completes_takes_the_trust_back() {
        let bus = FakeBus::new();
        *bus.pairing.lock().unwrap() = Pairing::NotPaired;
        let (bus, dynamic) = shared(bus);
        trust_on_accept(&dynamic, DEVICE).await;

        tokio::time::sleep(PAIRING_SETTLE.saturating_sub(Duration::from_secs(1))).await;
        tokio::task::yield_now().await;
        assert_eq!(bus.calls(), vec![Call::SetTrusted(true)], "not yet");

        past_the_settle().await;
        assert_eq!(
            bus.calls(),
            vec![Call::SetTrusted(true), Call::SetTrusted(false)]
        );
    }

    /// When bluetoothd cannot be asked (it stopped, or the device is gone),
    /// nothing more is called.
    #[tokio::test(start_paused = true)]
    async fn an_unknown_outcome_leaves_the_trust_alone() {
        let bus = FakeBus::new();
        *bus.pairing.lock().unwrap() = Pairing::Unknown;
        let (bus, dynamic) = shared(bus);
        trust_on_accept(&dynamic, DEVICE).await;
        past_the_settle().await;
        assert_eq!(bus.calls(), vec![Call::SetTrusted(true)]);
    }

    /// A device already trusted is not touched, and so never reverted.
    #[tokio::test(start_paused = true)]
    async fn accepting_a_trusted_device_sets_nothing() {
        let bus = FakeBus::new();
        *bus.shown.lock().unwrap() = Some(Shown {
            paired: false,
            trusted: true,
        });
        *bus.pairing.lock().unwrap() = Pairing::NotPaired;
        let (bus, dynamic) = shared(bus);
        trust_on_accept(&dynamic, DEVICE).await;
        past_the_settle().await;
        assert!(bus.calls().is_empty());
    }
}
