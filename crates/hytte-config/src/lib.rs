//! The GTK-free half of trollshell's user configuration: how
//! `~/.config/trollshell/*` is read and written, and the `places.toml` model.
//!
//! # Why this is its own crate (#640)
//!
//! `places.toml` has **two** writers by design — the operator editing it in
//! `$EDITOR` (which #703 explicitly asked to keep) and the control center's
//! places editor. Two writers over one file must agree byte for byte on how it
//! is validated and rendered, or the "format-preserving" guarantee the issue
//! settled on is only true of whichever one happened to write last.
//!
//! The shell's own copy of that logic lived in `hytte-services`, and
//! `trollshell-control-center` cannot link `hytte-services`: it pulls `gtk`,
//! `pipewire` and `hytte-ecal`, i.e. libpipewire and evolution-data-server into
//! a settings app. So the shared half moved here, to a leaf crate that depends
//! on nothing but `serde`/`toml`/`toml_edit`/`tracing` — the same shape as
//! `hytte-plugin-proto` and `hytte-ai-providers`. `hytte-services::places` now
//! wraps this with the reactive/service layer, and the control center calls it
//! directly. One model, one validator, one writer.
//!
//! # The layering (#868)
//!
//! #866 settled that trollshell's user configuration moves off 43 environment
//! variables and onto per-subsystem TOML, written as a nix **base** that an
//! unmanaged **overlay** layers over, with **state** kept somewhere else
//! entirely. The four modules that make that possible are the second half of
//! this crate, and they are deliberately additive: `places` predates them and
//! goes through none of them, which `tests/places_byte_identical.rs` pins byte
//! for byte.
//!
//! ```text
//! $XDG_CONFIG_DIRS/trollshell/<subsystem>.toml   base, nix-written, read-only
//! $XDG_CONFIG_HOME/trollshell/<subsystem>.toml   overlay, yours
//! $XDG_STATE_HOME/trollshell/<subsystem>.toml    state, the shell's
//! $XDG_CONFIG_HOME/trollshell/*.key              secrets, unchanged
//! ```
//!
//! Secrets stay out of the TOML on purpose: #752 established that an API key
//! in the environment is a hazard, and a key in a file the control center can
//! edit would undo that. `*.key` files keep their own path (see
//! `hytte-ai-providers`), and nothing here reads or writes one.
//!
//! # Modules
//!
//! - [`file`](mod@file) — the `~/.config/trollshell/<name>` path/read/write boilerplate,
//!   including the workspace's single copy of the atomic tmp + `fsync` +
//!   `rename(2)` replacement (#733/#739).
//! - [`places`](mod@places) — the `places.toml` schema, its validation rules, and the
//!   format-preserving writer.
//! - [`xdg`](mod@xdg) — where the layers and the state file live, as pure
//!   functions over an explicit environment.
//! - [`merge`](mod@merge) — the four merge rules (scalars, tables, arrays,
//!   and the spelled-out "unset").
//! - [`subsystem`](mod@subsystem) — the schema shape: declare a type, a name
//!   and a documented default; inherit the reader, the validator harness and
//!   the format-preserving writer.
//! - [`state`](mod@state) — the `$XDG_STATE_HOME` writer.

pub mod file;
pub mod merge;
pub mod places;
pub mod state;
pub mod subsystem;
pub mod xdg;

#[cfg(test)]
pub(crate) mod test_tracing {
    //! Test-only tracing plumbing (#1022), shared by every `capture()`-style
    //! harness in this crate — not just [`subsystem`]'s.
    //!
    //! [`subsystem::assemble`] is the only production code that reaches in here
    //! (through its own `#[cfg(test)]` hook), but the fix has to live somewhere
    //! every test module can reach: at the time of PR #1043's review (F3), the
    //! old home — `pub(in crate::subsystem)` inside `subsystem::tests` — left
    //! `file`/`xdg`/`state`/`merge`/`places`'s own tests unable to call it even
    //! though they run in the same lib-test binary and share the same
    //! process-wide callsite registry, with ten-plus `warn!`/`error!` sites of
    //! their own (e.g. `subsystem::load_or_default`'s error arm, reachable
    //! without ever entering `assemble`). Latent today — every existing
    //! `capture()` call happens to route through `assemble`/`assembled` — but the
    //! first `capture()`-shaped harness added to `file.rs` or `places.rs` would
    //! have had #1022 back for its own callsites, with nothing obvious to call.
    //! A second harness should call [`ensure_global_default`] from here, not
    //! re-declare its own `Once`.
    //!
    //! # Why a callsite can go silently unobserved, and why warming callsites
    //! up cannot close it (#1022)
    //!
    //! `tracing-core` caches a callsite's `Interest` **process-wide** the
    //! first time it fires. That first-ever registration
    //! (`DefaultCallsite::register`, `tracing-core = 0.1.36`
    //! `src/callsite.rs:319`, reached the first time any macro at that source
    //! location runs anywhere in the process) computes the cached value via
    //! `DISPATCHERS.rebuilder()` (`src/callsite.rs:544-548`), which —
    //! whenever at most one `Dispatch` is currently live anywhere in the
    //! process — takes the `Rebuilder::JustOne` shortcut
    //! (`src/callsite.rs:561-565`): its `for_each` calls
    //! `dispatcher::get_default(f)`, i.e. "whatever *the registering thread's
    //! own* ambient default is," not the full live-dispatcher list. If that
    //! thread has no subscriber installed at that exact moment, the callsite
    //! is cached `Interest::never()` for the rest of the process: once
    //! *registered*, a later hit on *any* thread only re-reads the cached
    //! value rather than recomputing it.
    //!
    //! `subsystem::assemble<S: Subsystem>` is generic, but every
    //! `tracing::warn!` inside it expands to a `static` item local to the
    //! function body — and a `static` inside a generic function is **not**
    //! monomorphized; Rust emits exactly one instance, shared by every
    //! instantiation, because a `static`'s address cannot depend on a type
    //! parameter. So `assemble::<Leds>` (used by `subsystem::tests::assembled`,
    //! the helper most of that module's tests call) and the one-off
    //! `Subsystem` impls its "built-in default" tests define locally all
    //! share the *same* three callsites — there is no per-type isolation to
    //! rely on.
    //!
    //! `subsystem::tests` has tests that call `assemble`/`assembled` with
    //! **no** subscriber installed at all —
    //! `an_unknown_key_is_reported_and_does_not_fail_the_load` and
    //! `an_unknown_key_inside_an_optional_table_is_named_and_kept` among
    //! them, both by design; neither needs [`capture`](crate::subsystem)
    //! since neither asserts on a `warn!`. Either can win a shared callsite's
    //! first-ever registration race on its own bare thread. Whether that
    //! poisons the callsite for a *different*, `capture()`-using test depends
    //! on scheduling: `Dispatch::new`'s construction
    //! (`tracing_core::callsite::register_dispatch`, `src/dispatcher.rs:479`
    //! → `src/callsite.rs:484-487`) unconditionally rebuilds every
    //! *already-registered* callsite against every currently-live dispatcher,
    //! which un-poisons it — but only if that callsite is already in the
    //! registry by the time some `capture()`-holding test's own
    //! `Dispatch::new` runs. Warming callsites up first (this crate's
    //! original attempt here, and #1020's and #1032's shape) only moves
    //! *which* thread can lose that race; it cannot make a bare-thread
    //! registration impossible, because the warm-up itself has to touch each
    //! callsite from *some* thread, and an unrelated unguarded test can
    //! always touch the same shared callsite first. Measured: main
    //! (unpatched) **8/500** whole-binary runs under load across three
    //! different tests; warming up before installing the capture `Dispatch`
    //! (matching #1020/#1032), **2/300**; warming up *inside* the guarded
    //! scope instead, **3/500** — a reduction, never a closure, because
    //! neither shape touches the actual defect: a subscriber-less thread can
    //! exist in this binary at all.
    //!
    //! # The fix: make a subscriber-less thread impossible
    //!
    //! [`ensure_global_default`] installs [`AlwaysInterested`] — a trivial,
    //! unconditionally-`enabled` `Subscriber` — as the **process-wide global
    //! default**, exactly once, from `subsystem::tests::capture` and
    //! `subsystem::tests::assembled`, *and* from `subsystem::assemble` itself
    //! under `#[cfg(test)]`: several tests call `assemble::<SomeLocalType>`
    //! directly, past both of the other two, so `assemble` is the one call
    //! site every path actually funnels through (compiled out entirely for
    //! `load`'s real, non-test callers — see `assemble`'s own doc). Once
    //! installed, `dispatcher::get_default`'s fallback (`src/dispatcher.rs`,
    //! `Entered::current`: `Some(default) => default, None => get_global()`)
    //! means a **truly bare** thread — no `set_default` anywhere in its call
    //! stack, and no other `Dispatch` concurrently live in the process —
    //! resolves the `Rebuilder::JustOne` path above to `AlwaysInterested`
    //! instead of `Dispatch::none()`, so a first-ever registration in that
    //! exact scenario can no longer cache `Interest::never()`. That is the
    //! scenario `subsystem::tests::a_bare_thread_touching_a_callsite_while_only_the_global_is_live`
    //! exercises directly.
    //!
    //! **That is not, however, the path
    //! `subsystem::tests::a_bare_thread_that_never_calls_capture_still_sees_the_global_default`
    //! (the original #1022 regression test) actually takes** — its assertion
    //! message used to credit `JustOne` too (PR #1043 review, finding F1).
    //! That test calls `capture()` — which installs a second, ephemeral
    //! `Dispatch` via `set_default` — *before* the bare thread's first touch.
    //! By the time that touch runs, two dispatchers are concurrently live
    //! (the permanent global plus capture's own), so `has_just_one` is
    //! `false` and `rebuilder()` returns `Rebuilder::Read`, the full-registrar
    //! path, not `JustOne`. What actually saves that test is the *fold* over
    //! that full list (`Interest::and`, `tracing-core`'s `subscriber.rs`):
    //! it can never land on `never()` any more, because our
    //! permanently-registered global unconditionally reports
    //! `Interest::always()` for every callsite, and `and` only produces
    //! `never()` when *every* entry says `never()`. So the invariant that
    //! test actually demonstrates is "**a permanently-registered dispatcher
    //! exists**", not "`JustOne`'s ambient fallback resolved to it" — the
    //! narrower `JustOne` claim is real, but it is the *other* test's job.
    //! Confirmed by mutation: swapping [`AlwaysInterested`] for a clone whose
    //! `register_callsite` returns `Interest::never()` (i.e.
    //! `tracing::subscriber::NoSubscriber`'s own default) leaves both tests
    //! green — `always()` is belt-and-braces for the genuine `JustOne`
    //! window, not what closes #1022 on its own.
    //!
    //! `capture`'s own `set_default` still shadows the global default on its
    //! thread (thread-local beats global), so a capturing test's own events
    //! land in its own `Captured` exactly as before — the global default
    //! only ever matters for *registration*, never for where an
    //! actually-`enabled` event's payload goes.
    //!
    //! `warm_up_callsites` (both shapes) is gone: with a live global default
    //! always present, pre-touching callsites buys nothing.

    /// A trivial, unconditionally-`enabled` [`tracing::Subscriber`] installed
    /// as the process-wide global default by [`ensure_global_default`] — see
    /// the module doc for why this exists and exactly what it does and
    /// doesn't prove.
    pub(crate) struct AlwaysInterested;

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
        // No thread-local capture is listening on the global default itself
        // (every real assertion goes through `capture`'s `set_default`,
        // which shadows this on its own thread), so there is nowhere for an
        // event delivered here to usefully go. Dropping it is harmless.
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Installs [`AlwaysInterested`] as the global default exactly once for
    /// the whole test binary. Called from `subsystem::tests::capture` and
    /// `subsystem::tests::assembled`, and — `pub(crate)`, since this is the
    /// one call every test module needs a path to — from `subsystem::assemble`
    /// itself under `#[cfg(test)]`, which is the call that actually makes
    /// every path unskippable: several tests call `assemble` directly, past
    /// both `capture` and `assembled`. `set_global_default` can only succeed
    /// once per process; nothing else in this crate or its dev-dependencies
    /// (`tempfile`, `temp-env`) calls it, so a second call racing this one is
    /// expected to find the `Once` already tripped, never to hit
    /// `set_global_default` itself a second time.
    ///
    /// # A poisoned `Once` versus the real reason (#1043 review, finding F2)
    ///
    /// The `Once`'s closure decides the outcome and stores it in `OK`; it
    /// never panics. The `assert!` that can panic runs *outside*
    /// `call_once`, on every call, not just the one that ran the closure —
    /// so every caller that loses the race sees the real reason, not
    /// `std::sync::Once`'s own "previously been poisoned" message. That
    /// matters here specifically because `ensure_global_default` is reached
    /// from `assemble`'s `#[cfg(test)]` hook, i.e. from nearly every test in
    /// this crate's lib-test binary (137 of them, at review time): an
    /// `.expect()` *inside* `call_once` would poison the `Once` the one time
    /// some foreign global won the race, and then **every other test that
    /// calls this function for the rest of that run** would fail with
    /// "Once instance has previously been poisoned" instead of the one
    /// message that actually explains anything.
    ///
    /// No dedicated regression test simulates that race in this binary:
    /// `set_global_default` succeeds at most once per process, and this
    /// function is reached by essentially every test here (directly or via
    /// `assemble`), so a test that deliberately won that race first would
    /// nondeterministically break every *other* test in the same run — Rust's
    /// default test harness runs tests in parallel, in no fixed order, so
    /// "run this test first" isn't a thing to rely on. Verified by hand
    /// instead, outside this crate's own suite, with a variant where a
    /// `tracing::subscriber::NoSubscriber` is installed via
    /// `set_global_default` immediately before the real call inside the
    /// `Once`: 31/32 failures read the poisoned-`Once` message before this
    /// fix, and after it every failure names the real cause (the `assert!`
    /// message above) instead.
    pub(crate) fn ensure_global_default() {
        static INSTALLED: std::sync::Once = std::sync::Once::new();
        static OK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        INSTALLED.call_once(|| {
            // `tracing::subscriber::set_global_default` wraps its argument in
            // `Dispatch::new` itself (`tracing-0.1.44`'s `subscriber.rs:42`),
            // so this alone is what fires `callsite::register_dispatch`. Not
            // `.expect()` here — see this function's doc for why deciding
            // inside `call_once` and asserting outside it is load-bearing.
            OK.store(
                tracing::subscriber::set_global_default(AlwaysInterested).is_ok(),
                std::sync::atomic::Ordering::SeqCst,
            );
        });
        assert!(
            OK.load(std::sync::atomic::Ordering::SeqCst),
            "another global default was installed first — #1022's fix is \
             inert; nothing else in this crate or its dev-dependencies \
             (`tempfile`, `temp-env`) is expected to call \
             `set_global_default`"
        );
    }
}
