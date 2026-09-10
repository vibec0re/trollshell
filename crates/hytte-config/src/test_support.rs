//! Test-only plumbing every test binary in the workspace can reach (#1044).
//!
//! Three things live here, and each of them existed in two or three
//! incompatible copies before this module did:
//!
//! - **the global default subscriber** ([`ensure_global_default`]) — the
//!   process-wide fix for the `tracing` callsite trap #1022 filed and PR #1043
//!   settled;
//! - **the capture harness** ([`capture`], [`Captured`]) — a thread-local
//!   `Subscriber` that records the events a test asserts on;
//! - **the scratch overlay** ([`Overlay`]) — a config layer file whose mtime a
//!   test controls to the second, which is what makes the `subsystem::watch`
//!   poller assertable at all.
//!
//! # Why this is a cargo feature rather than `#[cfg(test)]`
//!
//! `#[cfg(test)]` is per-crate: a `pub(crate)` helper in `hytte-config` is
//! invisible to `trollshell`'s test binary and to `hytte-reactive`'s, which is
//! precisely how the tree ended up with three implementations of the same fix,
//! two of them weaker than the third. A cargo feature crosses the crate
//! boundary, so every test binary installs the *same* thing, and the module is
//! compiled out of every non-test build — `test-support` is enabled by
//! dev-dependencies only, so a release `hytte-config` rlib contains no
//! `set_global_default` call at all.
//!
//! It is additionally `#[cfg(any(test, feature = "test-support"))]` so this
//! crate's own `cargo test` gets it without having to enable its own feature,
//! which is what lets [`crate::subsystem::assemble`]'s `#[cfg(test)]` hook keep
//! working unchanged.
//!
//! # Why a callsite can go silently unobserved, and why warming callsites
//! up cannot close it (#1022)
//!
//! `tracing-core` caches a callsite's `Interest` **process-wide** the first
//! time it fires. That first-ever registration (`DefaultCallsite::register`,
//! `tracing-core = 0.1.36` `src/callsite.rs:319`, reached the first time any
//! macro at that source location runs anywhere in the process) computes the
//! cached value via `DISPATCHERS.rebuilder()` (`src/callsite.rs:544-548`),
//! which — whenever at most one `Dispatch` is currently live anywhere in the
//! process — takes the `Rebuilder::JustOne` shortcut (`src/callsite.rs:561-565`):
//! its `for_each` calls `dispatcher::get_default(f)`, i.e. "whatever *the
//! registering thread's own* ambient default is," not the full live-dispatcher
//! list. If that thread has no subscriber installed at that exact moment, the
//! callsite is cached `Interest::never()` for the rest of the process: once
//! *registered*, a later hit on *any* thread only re-reads the cached value
//! rather than recomputing it.
//!
//! A `tracing::warn!` inside a **generic** function is one `static` shared by
//! every instantiation, not one per monomorphization — a `static`'s address
//! cannot depend on a type parameter — so `assemble::<A>` and `assemble::<B>`
//! race the *same* callsites, and there is no per-type isolation to rely on.
//!
//! Warming callsites up first (this crate's original attempt, and #1020's and
//! #1032's shape) only moves *which* thread can lose that race; it cannot make
//! a bare-thread registration impossible, because the warm-up itself has to
//! touch each callsite from *some* thread. Measured on unpatched main:
//! **8/500** whole-binary runs under load across three different tests;
//! warming up before installing the capture `Dispatch`, **2/300**; warming up
//! inside the guarded scope, **3/500** — a reduction, never a closure.
//!
//! # The fix: make a subscriber-less thread impossible
//!
//! [`ensure_global_default`] installs [`AlwaysInterested`] — a trivial,
//! unconditionally-`enabled` `Subscriber` — as the **process-wide global
//! default**, exactly once. Once installed, `dispatcher::get_default`'s
//! fallback (`Entered::current`: `Some(default) => default, None =>
//! get_global()`) means a **truly bare** thread resolves the
//! `Rebuilder::JustOne` path above to `AlwaysInterested` instead of
//! `Dispatch::none()`, so a first-ever registration in that exact scenario can
//! no longer cache `Interest::never()`.
//!
//! Separately, and this is the half that survives even when the global slot is
//! already taken: `Dispatch::new` **registers** the dispatcher, and
//! `has_just_one` is recomputed inside `register_dispatch` as
//! `dispatchers.len() <= 1` after pruning dead ones (`:551-558`). A dispatcher
//! that never dies therefore switches the `JustOne` fast path off for the rest
//! of the process, so every later rebuild iterates the live list — which always
//! includes ours, whose `register_callsite` is `Interest::always()`, and
//! `Interest::and` degrades a disagreement to `sometimes`, never to `never`.
//!
//! # Losing the global slot is a weaker fix, not a panic (#1040 T5 / #1044)
//!
//! The tree had two shapes of this. `hytte-config`'s (#1043) took the global
//! default and `assert!`ed it had won; the trollshell config tests' (#1040 fix
//! round 2) never touched the global slot and only parked a permanently
//! registered `Dispatch`. The parked-`Dispatch` shape is strictly weaker — it
//! cannot cover `Rebuilder::JustOne`'s ambient fallback, only switch the fast
//! path off once a second `Dispatch` has registered — but it is never wrong,
//! and it leaves the process's single global slot free.
//!
//! So [`install_global_default`] does both, in that order: it builds the
//! `Dispatch` (which registers it), parks a strong reference so the registrar's
//! `Weak` can never die, and *then* offers it for the global slot. Winning
//! gives the strong fix; losing gives the weak one and an
//! [`Installed::Registered`] return value rather than a panic. A caller whose
//! subscriber must actually *receive* events (`hytte-reactive`'s supervisor
//! error counter) asserts on that return value — **outside** its own `Once`,
//! which is the other half of #1043's finding F2: an `.expect()` *inside*
//! `call_once` poisons the `Once`, and then every other test in the run fails
//! with "Once instance has previously been poisoned" instead of the one message
//! that explains anything (measured: 31 of 32 failures).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

// ── The global default subscriber ────────────────────────────────────────────

/// What happened to the subscriber a caller offered
/// [`install_global_default`].
///
/// Returned rather than asserted, because the two outcomes are both useful and
/// only the caller knows which one it needs: a caller that only wants callsite
/// interest kept alive is served by either, and a caller whose subscriber has
/// to *receive* the events is served by [`Self::GlobalDefault`] alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Installed {
    /// The subscriber won the process's single global-default slot: it is
    /// every subscriber-less thread's ambient dispatcher, and it receives every
    /// event that is not captured by a thread-local `set_default` first.
    GlobalDefault,
    /// Another global default was installed first. The subscriber is
    /// permanently *registered* — so it still keeps callsite `Interest` alive
    /// for the life of the process — but it is nobody's default and receives
    /// nothing.
    Registered,
}

/// A trivial, unconditionally-`enabled` [`tracing::Subscriber`] — see the
/// module doc for what it does and does not prove.
///
/// It enables everything, records nothing and is never any thread's chosen
/// default, so it can only ever *widen* callsite interest. The cost is that
/// `warn!` macros build their event on threads with no capture installed and
/// hand it here, where it is dropped.
pub struct AlwaysInterested;

impl tracing::Subscriber for AlwaysInterested {
    fn register_callsite(
        &self,
        _: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::always()
    }
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    // No thread-local capture is listening on the global default itself (every
    // real assertion goes through `capture`'s `set_default`, which shadows this
    // on its own thread), so there is nowhere for an event delivered here to
    // usefully go. Dropping it is harmless.
    fn event(&self, _: &tracing::Event<'_>) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Strong references to every `Dispatch` this module has registered, held for
/// the life of the process.
///
/// `tracing-core`'s registrar keeps only a `Weak`; a `Dispatch` that is dropped
/// is pruned on the next `register_dispatch`, and with it the `has_just_one`
/// suppression the module doc describes. The global-default slot holds its own
/// strong reference, so this only *has* to hold the losers — but it holds both,
/// because "the keepalive is whatever we registered" is one rule rather than
/// two.
static PARKED: Mutex<Vec<tracing::Dispatch>> = Mutex::new(Vec::new());

/// Register `subscriber` permanently, and offer it the process's global-default
/// slot.
///
/// Never panics and never fails: losing the slot is reported as
/// [`Installed::Registered`], not as an error, because a registered-but-not-
/// default dispatcher is still the weaker half of #1022's fix (module doc).
///
/// Call it **once per subscriber per process** — through a `OnceLock` whose
/// value is the returned [`Installed`], so every later caller sees the same
/// outcome without a second registration. [`ensure_global_default`] is that
/// wrapper for the common case.
pub fn install_global_default<S>(subscriber: S) -> Installed
where
    S: tracing::Subscriber + Send + Sync + 'static,
{
    // `Dispatch::new` is what calls `callsite::register_dispatch`, so the
    // keepalive effect is bought here, before the slot is even offered.
    let dispatch = tracing::Dispatch::new(subscriber);
    PARKED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(dispatch.clone());
    if tracing::dispatcher::set_global_default(dispatch).is_ok() {
        Installed::GlobalDefault
    } else {
        Installed::Registered
    }
}

/// Install [`AlwaysInterested`] for the whole test binary, exactly once.
///
/// Idempotent and cheap after the first call: the outcome is memoised in a
/// `OnceLock`, so the hundred-odd tests that funnel through
/// [`crate::subsystem::assemble`]'s `#[cfg(test)]` hook pay one atomic load
/// each.
///
/// The return value is normally ignored — this subscriber records nothing, so
/// [`Installed::Registered`] is a perfectly usable outcome and there is nothing
/// for a caller to do about it. It is returned rather than dropped so a test
/// that wants to *assert* the strong form can.
pub fn ensure_global_default() -> Installed {
    static INSTALLED: OnceLock<Installed> = OnceLock::new();
    *INSTALLED.get_or_init(|| install_global_default(AlwaysInterested))
}

// ── The capture harness ──────────────────────────────────────────────────────

/// One `tracing` event, flattened to what an assertion actually looks at.
#[derive(Clone, Debug)]
pub struct CapturedEvent {
    /// The event's level — the usual first filter (`WARN` vs `ERROR`).
    pub level: tracing::Level,
    /// The rendered message.
    pub message: String,
    /// Everything but the message, rendered.
    ///
    /// Kept rather than dropped since #1040 V10: the config subsystems
    /// advertise that their lines carry `subsystem`/`var`/`key`/`accepts`/`file`
    /// "for anything that wants to filter on them", and the trollshell copy of
    /// this harness used to throw them away — so nothing asserted a single one
    /// and the claim was unfalsifiable. This crate's copy always kept them;
    /// hoisting is what reconciles the two.
    pub fields: HashMap<String, String>,
}

/// Events recorded by [`capture`], in emission order.
///
/// Hand-rolled over [`tracing::Subscriber`] rather than assembled from
/// `tracing_subscriber`'s `Registry` + `Layer`, which is what
/// `hytte-services`' `hooks.rs` harness does: this crate is the GTK-free leaf
/// whose whole point is a short dependency list, and a capture that only ever
/// needs `event` does not justify widening it. That is also why `hooks.rs` is
/// deliberately **not** folded in here — it is a `Layer`, a different shape
/// with a different dependency, and it captures nothing this one does.
#[derive(Clone, Default)]
pub struct Captured {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl Captured {
    /// Everything recorded so far.
    #[must_use]
    pub fn events(&self) -> Vec<CapturedEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Every message recorded at `level`, in order — the denominator for an
    /// "exactly one line" assertion.
    #[must_use]
    pub fn messages_at(&self, level: tracing::Level) -> Vec<String> {
        self.events()
            .into_iter()
            .filter(|e| e.level == level)
            .map(|e| e.message)
            .collect()
    }

    /// [`Self::messages_at`] at `WARN`.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        self.messages_at(tracing::Level::WARN)
    }

    /// [`Self::messages_at`] at `ERROR`.
    #[must_use]
    pub fn errors(&self) -> Vec<String> {
        self.messages_at(tracing::Level::ERROR)
    }
}

impl tracing::Subscriber for Captured {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(CapturedEvent {
                level: *event.metadata().level(),
                message: visitor.message,
                fields: visitor.fields,
            });
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: HashMap<String, String>,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        // The message itself arrives here: `tracing` renders format args through
        // `Debug`, and `Arguments`' `Debug` is its `Display`. So do `%` sigils,
        // whose wrapper renders `Display` through `Debug`.
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = rendered;
        } else {
            self.fields.insert(field.name().to_string(), rendered);
        }
    }
}

/// Install a capturing [`tracing::Subscriber`] as **this thread's** ambient
/// default, and hand back the events it records plus the guard that keeps it
/// installed.
///
/// Drop the guard (or let it fall out of scope) to uninstall. A thread-local
/// default shadows the global one, so a capturing test's own events land in its
/// own [`Captured`] and never in [`AlwaysInterested`].
///
/// Calls [`ensure_global_default`] first — see the module doc for the mechanism
/// that exists to close (#1022): without a permanently-registered global
/// default, a callsite first touched on a subscriber-less thread anywhere in
/// the binary can cache `Interest::never()` for the rest of the process, which
/// would make this very capture blind to a `warn!` it should have seen,
/// depending on unrelated test scheduling.
#[must_use]
pub fn capture() -> (Captured, tracing::dispatcher::DefaultGuard) {
    ensure_global_default();
    let captured = Captured::default();
    let guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(captured.clone()));
    (captured, guard)
}

// ── The scratch config layer ─────────────────────────────────────────────────

/// A scratch config layer whose mtime the test controls.
///
/// The `subsystem::watch` poller's whole subject is *whether a stamp moved*, so
/// a harness that lets the filesystem pick the mtime cannot test it: a
/// sub-millisecond test rewrites the file inside one mtime granule and the
/// watcher correctly sees no change. Every mutation of this file therefore sets
/// the mtime explicitly, in whole seconds, from a counter the test advances.
pub struct Overlay {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    stamp: u64,
}

impl Overlay {
    /// An empty scratch directory holding `<stem>.toml` — not yet created on
    /// disk, which is the "no overlay" case every subsystem's normal state is.
    ///
    /// # Panics
    /// If the scratch directory cannot be created.
    #[must_use]
    pub fn new(stem: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(format!("{stem}.toml"));
        Self {
            _dir: dir,
            path,
            stamp: 0,
        }
    }

    /// Write `body` and move the mtime forward by a whole second.
    ///
    /// # Panics
    /// If the file cannot be written or its mtime set.
    pub fn write(&mut self, body: &str) {
        self.put(body);
        self.stamp += 1;
        self.touch();
    }

    /// Write `body` **without** moving the mtime — the same-granule save an
    /// mtime-only watcher misses forever (#1040 F8), and the reason the stamp
    /// is a `(mtime, len)` pair rather than an mtime.
    ///
    /// # Panics
    /// If the file cannot be written or its mtime set.
    pub fn write_in_the_same_granule(&self, body: &str) {
        self.put(body);
        self.touch();
    }

    fn put(&self, body: &str) {
        std::fs::write(&self.path, body).expect("write");
    }

    fn touch(&self) {
        let file = std::fs::File::options()
            .write(true)
            .open(&self.path)
            .expect("open");
        file.set_modified(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(self.stamp),
        )
        .expect("set mtime");
    }

    /// Remove the file — the "I want the stock look back" intent, which is not
    /// an error (#1040 F7).
    ///
    /// # Panics
    /// If the file does not exist or cannot be removed.
    pub fn delete(&self) {
        std::fs::remove_file(&self.path).expect("delete");
    }

    /// The one layer path, in the shape `subsystem::watch::boot` takes.
    #[must_use]
    pub fn layers(&self) -> Vec<std::path::PathBuf> {
        vec![self.path.clone()]
    }

    /// Where the layer file is, for a test that needs to build a multi-layer
    /// path list around it.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}
