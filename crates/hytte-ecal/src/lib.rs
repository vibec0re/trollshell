//! Safe wrappers around the minimal libecal / libedataserver / libical-glib
//! subset we need to drive Evolution Data Server task lists. Reads + writes
//! work against ANY backend (local, CalDAV, Google, EWS, …) — libecal does
//! the per-backend translation.
//!
//! ## Threading
//!
//! Every public method is **sync** and blocks. EDS spawns its own threads
//! internally; libecal's GMainContext usage means a process-wide
//! [`GMainContext`] must be iterated for some operations (notably async
//! ones we don't use). The sync APIs we expose handle that themselves.
//!
//! Wrappers own their underlying GObjects and `g_object_unref` on drop.
//! `Registry` and `CalClient` are not [`Sync`] — share via a `Mutex` or
//! pin to one thread.
//!
//! ## Errors
//!
//! Every fallible call extracts the GLib message from the out-param
//! `GError**` (if any) and wraps it in an `anyhow::Error`. The GError
//! itself is freed; the resulting string copy lives in the `Error`.
//!
//! ## `unsafe` and its SAFETY comments (#1179, #1195)
//!
//! This is one of the workspace's two `unsafe` islands (the other is
//! `hytte-gl`); everything else is compiled under `unsafe_code = "forbid"`.
//! Every `unsafe` block here carries a `// SAFETY:` line saying what that
//! block relies on — **one line per block**, because
//! `clippy::undocumented_unsafe_blocks` is `deny` for this crate (#1195, from
//! the root lints table its `Cargo.toml` mirrors), and that lint reads the
//! comment directly above each block and nothing else. A grouped comment
//! covering three blocks at once therefore fails that lint, which is the
//! point: a future block arriving without a justification fails `cargo
//! clippy` (the `nix flake check` gate — a plain `cargo check` does not see
//! this lint at all) rather than a reviewer's sampling.
//!
//! Three premises recur on nearly every call, so they are stated once here
//! and referred to by name (**P1**/**P2**/**P3**) rather than retyped a
//! hundred times; a block whose soundness needs anything beyond them spells
//! that out in full. Where several adjacent blocks genuinely share one
//! argument, the argument is written once as ordinary prose above them and
//! each block's own `SAFETY:` line cites it.
//!
//! - **P1 — owned handle.** A wrapper's `self.raw` (or a local that a
//!   null-check just guarded) is a live GObject pointer this crate owns
//!   exactly one ref to: non-null because the constructor rejected null, and
//!   released exactly once in `Drop`. It is therefore valid for the whole
//!   `&self` borrow, and handing it to a libecal/libical/GLib accessor is
//!   sound. Sub-objects those accessors return are null-checked before use,
//!   and the "new ref vs borrow" convention of each is documented at its
//!   `sys` declaration.
//! - **P2 — `GError**` out-param.** `&mut err` points at a live local
//!   initialised to `ptr::null_mut()`, which is exactly what a `GError**`
//!   out-param wants. The callee either leaves it null or stores a `GError`
//!   it transfers to us; [`take_error`] reads and frees it at most once.
//! - **P3 — thread affinity.** `Registry`, `CalClient`, `CalClientView` and
//!   `MainContext` are `!Send`/`!Sync` (raw pointers), so this crate never
//!   lets two threads touch one object; no call here races another on the
//!   same handle. [`Waker`] is the deliberate exception and carries its own
//!   argument at its `unsafe impl`.
//!
//! Where a block's real invariant cannot be stated honestly, its comment says
//! `SAFETY: UNVERIFIED` and names what would have to be true — a finding to
//! chase, not a claim to trust.

#![doc(test(no_crate_inject))]

pub mod sys;

use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};

// ── EventInstance ────────────────────────────────────────────────────────────

/// One concrete occurrence of a (possibly recurring) calendar component,
/// materialised by [`CalClient::generate_instances`]. Carries the
/// **authoritative per-instance** start/end as POSIX `time_t` (UTC seconds)
/// — computed by applying the component's RRULE via libical's recurrence
/// iterator, so a daily meeting yields one `EventInstance` per day in the
/// window. `ical` is the component's iCalendar serialisation, from which the
/// caller extracts SUMMARY / LOCATION / UID etc. (the embedded DTSTART still
/// reflects the series origin — trust `start_unix`/`end_unix`, not it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventInstance {
    /// iCalendar serialisation of the component this occurrence belongs to.
    ///
    /// **Shared, not cloned** (#1179): every occurrence of one series points
    /// at the same allocation, because the string is identical across them by
    /// construction — it is the *master* component's serialisation, the same
    /// bytes for occurrence 1 and occurrence 60 000. It is an [`Arc`] rather
    /// than an `Rc` because expansion runs on the EDS worker thread and the
    /// instances are handed to the UI thread.
    ///
    /// Read it as a `&str` (`&inst.ical` coerces); construct one from a
    /// `String`/`&str` with `.into()`.
    pub ical: Arc<str>,
    /// Occurrence start, POSIX seconds since the Unix epoch (UTC).
    pub start_unix: i64,
    /// Occurrence end, POSIX seconds since the Unix epoch (UTC).
    pub end_unix: i64,
    /// True when the occurrence's start is a DATE (no time-of-day) — i.e.
    /// an all-day event.
    pub all_day: bool,
}

// ── ESourceRegistry ──────────────────────────────────────────────────────────

/// Central source database. Construct once via [`Registry::new`] and share
/// for the lifetime of the process — the constructor is expensive
/// (`new_sync` round-trips to EDS over D-Bus).
pub struct Registry {
    raw: *mut sys::ESourceRegistry,
}

impl Registry {
    /// Synchronously open the source registry. Blocks until EDS responds.
    pub fn new() -> Result<Self> {
        let mut err: *mut sys::GError = ptr::null_mut();
        // SAFETY: P2. A null `GCancellable*` is the documented "uncancellable"
        // argument; the returned ref is checked for null and then owned by the
        // `Registry` this returns.
        let raw = unsafe { sys::e_source_registry_new_sync(ptr::null_mut(), &mut err) };
        if raw.is_null() {
            return Err(
                take_error(err).unwrap_or_else(|| anyhow!("ESourceRegistry: unknown error"))
            );
        }
        Ok(Self { raw })
    }

    /// Enumerate every configured task-list source. Sources that aren't
    /// enabled (`Enabled=false` in the `.source` file) are still returned
    /// — caller filters if needed.
    pub fn task_lists(&self) -> Vec<Source> {
        self.sources_by_extension(c"Task List")
    }

    /// Enumerate every configured calendar (Events) source — the sibling
    /// of [`task_lists`](Self::task_lists) for the `"Calendar"` extension.
    /// Backend-agnostic: local `.ics`, `CalDAV` (Nextcloud / generic),
    /// Google (via GOA), EWS — libecal does the per-backend translation, so
    /// the caller opens each with `CalClient::connect(.., Events, ..)` and
    /// queries VEVENTs regardless of where they actually live. As with
    /// task lists, disabled sources are still returned; caller filters.
    pub fn calendars(&self) -> Vec<Source> {
        self.sources_by_extension(c"Calendar")
    }

    /// Look up a single source by UID. Returns `None` if EDS doesn't
    /// know about that UID. The returned [`Source`] holds its own ref.
    pub fn ref_source(&self, uid: &str) -> Result<Option<Source>> {
        let c = CString::new(uid).context("uid contained an interior NUL")?;
        // SAFETY: P1/P3. `c` is a live `CString` (NUL-terminated, no interior
        // NUL — `CString::new` just proved it) that outlives the call, and the
        // returned ref is null-checked before a `Source` adopts it.
        let raw = unsafe { sys::e_source_registry_ref_source(self.raw, c.as_ptr()) };
        if raw.is_null() {
            return Ok(None);
        }
        Ok(Some(Source { raw }))
    }

    // Internal raw accessor unused publicly today — keeping the lifetime
    // tied to `&self` so future calls that need the registry pointer
    // (e.g. extension-property reads) don't have to re-architect.
    #[allow(dead_code)]
    pub(crate) fn raw_handle(&self) -> *mut sys::ESourceRegistry {
        self.raw
    }

    fn sources_by_extension(&self, extension: &CStr) -> Vec<Source> {
        // SAFETY: P1/P3. `extension` is a live `&CStr` (NUL-terminated by
        // construction) that outlives the call. The returned `GList*` is owned
        // by us — spine freed below, elements adopted into `Source`s.
        let list = unsafe { sys::e_source_registry_list_sources(self.raw, extension.as_ptr()) };
        if list.is_null() {
            return Vec::new();
        }
        let mut out = Vec::new();
        // Walk the list spine once (O(n)) via `node.next`, rather than
        // calling `g_list_nth_data(list, i)` in a loop — each of those
        // re-walks from the head, making the whole thing O(n²).
        let mut node = list;
        while !node.is_null() {
            // SAFETY: `node` is non-null (loop condition) and points at a
            // `GList` node GLib allocated, whose layout `sys::GList` mirrors
            // `#[repr(C)]`; the list is alive until the free below, and
            // nothing mutates it meanwhile (P3).
            let data = unsafe { (*node).data };
            if !data.is_null() {
                // `list_sources` returns refs we own — but `g_list_free_full`
                // with `g_object_unref` would release them. Instead we
                // adopt each ref into a `Source` (which will unref on drop)
                // and free only the list spine (without touching elements).
                out.push(Source {
                    raw: data.cast::<sys::ESource>(),
                });
            }
            // SAFETY: as the `data` read above — `node` is non-null and the
            // node it names is still allocated.
            node = unsafe { (*node).next };
        }
        // Free the spine only — calling `g_list_free` here is the standard
        // pattern when ownership of the elements is transferred elsewhere.
        // We don't have a binding for `g_list_free` directly, so reach into
        // sys via the destroy-notify-free path with a no-op destroyer.
        //
        // SAFETY: `list` is the non-null list we own, freed exactly once here
        // and never read after; `no_op_destroy` matches `GDestroyNotify` and
        // touches nothing, so the element refs adopted above survive.
        unsafe { sys::g_list_free_full(list, no_op_destroy) }
        out
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        // SAFETY: P1 — the single ref `e_source_registry_new_sync` transferred
        // to this value, released exactly once (Drop runs once) and never used
        // after.
        unsafe { sys::g_object_unref(self.raw) }
    }
}

/// `GDestroyNotify` no-op used when freeing only the spine of a `GList`
/// whose elements have been moved into Rust ownership.
///
/// # Safety
///
/// Trivially safe — the body never reads the pointer.
unsafe extern "C" fn no_op_destroy(_: *mut c_void) {}

// ── ESource ──────────────────────────────────────────────────────────────────

/// One configured source. Cheap-to-clone? No — `Source` owns a ref;
/// dropping calls `g_object_unref`. Borrow via [`Self::raw`] for sub-
/// objects (like [`CalClient::connect`]) that don't take ownership.
pub struct Source {
    raw: *mut sys::ESource,
}

impl Source {
    /// Stable EDS UID — the same string that names the `.source` file
    /// in `~/.config/evolution/sources/`.
    pub fn uid(&self) -> String {
        // SAFETY: P1/P3. `e_source_get_uid` returns a `const char*` the
        // `ESource` owns; `borrowed_cstr` copies it before this borrow of
        // `self` ends and never frees it.
        unsafe { borrowed_cstr(sys::e_source_get_uid(self.raw)) }.unwrap_or_default()
    }

    /// Human-readable name from the `DisplayName=` key. Localised
    /// variants are ignored; only the untagged value is returned.
    pub fn display_name(&self) -> String {
        // SAFETY: as `uid` above — a borrowed `const char*` owned by the
        // `ESource`, copied out within this borrow.
        unsafe { borrowed_cstr(sys::e_source_get_display_name(self.raw)) }.unwrap_or_default()
    }

    /// True iff this source carries the named extension (e.g.
    /// `"Task List"` for task sources).
    #[must_use]
    pub fn has_extension(&self, extension_name: &str) -> bool {
        let Ok(c) = CString::new(extension_name) else {
            return false;
        };
        // SAFETY: P1/P3, and `c` is a live `CString` outliving the call.
        let r = unsafe { sys::e_source_has_extension(self.raw, c.as_ptr()) };
        r != 0
    }

    pub(crate) fn raw(&self) -> *mut sys::ESource {
        self.raw
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        // SAFETY: P1 — the one ref this `Source` adopted (from
        // `list_sources`/`ref_source`), released exactly once.
        unsafe { sys::g_object_unref(self.raw) }
    }
}

// ── ECalClient ───────────────────────────────────────────────────────────────

/// Connected calendar/task/memo client. One client → one source. Driven
/// by sync I/O — every call blocks until EDS responds (or 5 s by
/// default, see [`CalClient::connect`]).
pub struct CalClient {
    raw: *mut sys::ECalClient,
}

impl CalClient {
    /// Open a client against `source` of the requested `source_type`.
    /// `wait_seconds` is libecal's "wait for backend to come online"
    /// budget; 5 is a reasonable default for local/CalDAV. For Google
    /// Tasks the first connect can take longer — bump if you see
    /// transient connect timeouts.
    pub fn connect(
        source: &Source,
        source_type: sys::ECalClientSourceType,
        wait_seconds: u32,
    ) -> Result<Self> {
        let mut err: *mut sys::GError = ptr::null_mut();
        // SAFETY: P2, and `source.raw()` is borrowed from a live `Source`
        // (P1) for the duration of this blocking call — libecal takes its own
        // ref if it keeps the source. Null `GCancellable*` = uncancellable.
        let raw = unsafe {
            sys::e_cal_client_connect_sync(
                source.raw(),
                source_type,
                wait_seconds,
                ptr::null_mut(),
                &mut err,
            )
        };
        if raw.is_null() {
            return Err(take_error(err)
                .unwrap_or_else(|| anyhow!("e_cal_client_connect_sync returned null")));
        }
        Ok(Self { raw })
    }

    /// Parse the iCalendar fragment in `ical` (must be a complete VTODO
    /// or VEVENT, optionally wrapped in a VCALENDAR — libical's parser
    /// accepts both) and create it on the server. Returns the UID EDS
    /// assigned (may differ from any UID in the input — backends are
    /// allowed to rewrite).
    pub fn create_from_ical(&self, ical: &str) -> Result<String> {
        let comp = parse_component(ical)?;
        let mut out_uid: *mut c_char = ptr::null_mut();
        let mut err: *mut sys::GError = ptr::null_mut();
        // SAFETY: P1/P2/P3. `comp.raw` is the live component `parse_component`
        // just built and this scope still owns (dropped only after the call);
        // `out_uid` is a live local initialised to null, the out-param
        // contract, and the string it receives is freed below.
        let ok = unsafe {
            sys::e_cal_client_create_object_sync(
                self.raw,
                comp.raw,
                sys::E_CAL_OPERATION_FLAG_NONE,
                &mut out_uid,
                ptr::null_mut(),
                &mut err,
            )
        };
        // The Component drop runs here.
        drop(comp);
        if ok == 0 {
            return Err(take_error(err).unwrap_or_else(|| anyhow!("create_object_sync failed")));
        }
        if out_uid.is_null() {
            return Ok(String::new());
        }
        // SAFETY: `out_uid` is non-null here (checked above) and points at the
        // NUL-terminated string libecal allocated for us; the copy is taken
        // before it is freed.
        let s = unsafe { CStr::from_ptr(out_uid) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: `out_uid` is the GLib-allocated string transferred to us,
        // freed exactly once and never read after.
        unsafe { sys::g_free(out_uid.cast::<c_void>()) }
        Ok(s)
    }

    /// Replace an existing object. `ical` must include the same UID as
    /// the object on the server. Non-recurring tasks pass
    /// [`sys::ECalObjModType::All`] for the mod-type.
    pub fn modify_from_ical(&self, ical: &str) -> Result<()> {
        let comp = parse_component(ical)?;
        let mut err: *mut sys::GError = ptr::null_mut();
        // SAFETY: P1/P2/P3, and `comp.raw` is the live component this scope
        // owns until the `drop(comp)` below.
        let ok = unsafe {
            sys::e_cal_client_modify_object_sync(
                self.raw,
                comp.raw,
                sys::ECalObjModType::All,
                sys::E_CAL_OPERATION_FLAG_NONE,
                ptr::null_mut(),
                &mut err,
            )
        };
        drop(comp);
        if ok == 0 {
            return Err(take_error(err).unwrap_or_else(|| anyhow!("modify_object_sync failed")));
        }
        Ok(())
    }

    /// Fetch a single object by UID and return its iCalendar
    /// serialisation. Returns `Ok(None)` when EDS reports the object
    /// doesn't exist (distinct from a transport error). `rid` is the
    /// recurrence-id for instance-level reads — `None` for non-recurring
    /// objects.
    pub fn get_object_as_string(&self, uid: &str, rid: Option<&str>) -> Result<Option<String>> {
        let uid_c = CString::new(uid).context("uid contained an interior NUL")?;
        let rid_c = rid
            .map(|s| CString::new(s).context("rid contained an interior NUL"))
            .transpose()?;
        let rid_ptr = rid_c.as_ref().map_or(ptr::null(), |c| c.as_ptr());
        let mut out: *mut sys::ICalComponent = ptr::null_mut();
        let mut err: *mut sys::GError = ptr::null_mut();
        // SAFETY: P1/P2/P3. `uid_c`/`rid_c` are live `CString`s that outlive
        // the call (`rid_ptr` is null when there is no rid, which is the
        // documented "no recurrence-id" argument), and `out` is a live local
        // initialised to null for the out-param.
        let ok = unsafe {
            sys::e_cal_client_get_object_sync(
                self.raw,
                uid_c.as_ptr(),
                rid_ptr,
                &mut out,
                ptr::null_mut(),
                &mut err,
            )
        };
        if ok == 0 {
            // Distinguish "not found" from other errors by matching the
            // GError's domain quark + code, not the (localisable) message:
            // EDS sets E_CAL_CLIENT_ERROR_OBJECT_NOT_FOUND in the
            // E_CAL_CLIENT_ERROR domain. The domain quark is resolved at
            // runtime via `e_cal_client_error_quark()` (stable for the
            // process lifetime); `GError.domain` is itself a GQuark.
            if !err.is_null() {
                // SAFETY: `err` is non-null (checked) and was set by the
                // call above, so it points at a `GError` we own whose layout
                // `sys::GError` mirrors `#[repr(C)]`; `domain` is a plain
                // integer field, read before the free below.
                let domain = unsafe { (*err).domain };
                // SAFETY: as the `domain` read above — the same live, owned
                // `GError`, another plain integer field, still before the free.
                let code = unsafe { (*err).code };
                // SAFETY: a nullary `G_GNUC_CONST` function that only interns
                // and returns a quark — no arguments to get wrong.
                let not_found_domain = unsafe { sys::e_cal_client_error_quark() };
                if domain == not_found_domain && code == sys::E_CAL_CLIENT_ERROR_OBJECT_NOT_FOUND {
                    // SAFETY: `err` is the `GError` transferred to us, freed
                    // exactly once on this path (we return immediately, so
                    // `take_error` below never sees it).
                    unsafe { sys::g_error_free(err) }
                    return Ok(None);
                }
            }
            return Err(take_error(err).unwrap_or_else(|| anyhow!("get_object_sync failed")));
        }
        if out.is_null() {
            return Ok(None);
        }
        // SAFETY: `out` is the non-null component (checked above) whose single
        // ref `get_object_sync` transferred to us; the serialisation it
        // returns is a GLib-allocated string we own.
        let s_ptr = unsafe { sys::i_cal_component_as_ical_string(out) };
        let s = if s_ptr.is_null() {
            String::new()
        } else {
            // SAFETY: non-null (this arm) and NUL-terminated, as libical's
            // `as_ical_string` returns; copied before the free below.
            let s = unsafe { CStr::from_ptr(s_ptr) }
                .to_string_lossy()
                .into_owned();
            // SAFETY: the GLib-allocated string above, freed exactly once and
            // never read after.
            unsafe { sys::g_free(s_ptr.cast::<c_void>()) }
            s
        };
        // SAFETY: the component ref transferred to us, released exactly once
        // on every path that reaches here.
        unsafe { sys::g_object_unref(out) }
        Ok(Some(s))
    }

    /// Remove an object by UID. `rid` is the recurrence-id for instance-
    /// level deletes — pass `None` for non-recurring tasks (the
    /// overwhelmingly common case).
    pub fn remove(&self, uid: &str, rid: Option<&str>) -> Result<()> {
        let uid_c = CString::new(uid).context("uid contained an interior NUL")?;
        let rid_c = rid
            .map(|s| CString::new(s).context("rid contained an interior NUL"))
            .transpose()?;
        let rid_ptr = rid_c.as_ref().map_or(ptr::null(), |c| c.as_ptr());
        let mut err: *mut sys::GError = ptr::null_mut();
        // SAFETY: P1/P2/P3, with `uid_c`/`rid_c` live `CString`s outliving the
        // call and a null `rid_ptr` meaning "no recurrence-id".
        let ok = unsafe {
            sys::e_cal_client_remove_object_sync(
                self.raw,
                uid_c.as_ptr(),
                rid_ptr,
                sys::ECalObjModType::All,
                sys::E_CAL_OPERATION_FLAG_NONE,
                ptr::null_mut(),
                &mut err,
            )
        };
        if ok == 0 {
            return Err(take_error(err).unwrap_or_else(|| anyhow!("remove_object_sync failed")));
        }
        Ok(())
    }

    /// Run an S-expression query against the backend and return each
    /// matching component serialised back to iCalendar. The standard
    /// "everything" query is `"#t"`. Common task filters:
    ///
    /// - `"(not (completed?))"` — incomplete tasks only
    /// - `"(due-in-time-range? (make-time \"20260101T000000Z\")
    ///   (make-time \"20260601T000000Z\"))"` — due in window
    ///
    /// Returns iCal strings ready to be parsed by any iCalendar
    /// implementation (we round-trip through the `icalendar` crate
    /// downstream).
    pub fn get_object_strings(&self, sexp: &str) -> Result<Vec<String>> {
        let s = CString::new(sexp).context("sexp contained an interior NUL")?;
        let mut out_list: *mut sys::GSList = ptr::null_mut();
        let mut err: *mut sys::GError = ptr::null_mut();
        // SAFETY: P1/P2/P3, `s` is a live `CString` outliving the call, and
        // `out_list` is a live local initialised to null for the out-param.
        let ok = unsafe {
            sys::e_cal_client_get_object_list_sync(
                self.raw,
                s.as_ptr(),
                &mut out_list,
                ptr::null_mut(),
                &mut err,
            )
        };
        if ok == 0 {
            return Err(take_error(err).unwrap_or_else(|| anyhow!("get_object_list_sync failed")));
        }
        if out_list.is_null() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        // Single O(n) walk of the GSList spine via `node.next`, instead of
        // the O(n²) `g_slist_nth_data(list, i)` loop (each call re-walks
        // from the head).
        let mut node = out_list;
        while !node.is_null() {
            // SAFETY: `node` is non-null (loop condition) and names a node of
            // the list we own until the free below; `sys::GSList` mirrors
            // glib's layout `#[repr(C)]` and nothing mutates the list (P3).
            let data = unsafe { (*node).data }.cast::<sys::ICalComponent>();
            if !data.is_null() {
                // SAFETY: `data` is a non-null element of that list — a live
                // `ICalComponent*` we own a ref to — and this only reads it.
                let s_ptr = unsafe { sys::i_cal_component_as_ical_string(data) };
                if !s_ptr.is_null() {
                    // SAFETY: non-null (this arm), NUL-terminated, copied
                    // before the free below.
                    let s = unsafe { CStr::from_ptr(s_ptr) }
                        .to_string_lossy()
                        .into_owned();
                    // SAFETY: the GLib-allocated string above, freed once.
                    unsafe { sys::g_free(s_ptr.cast::<c_void>()) }
                    out.push(s);
                }
            }
            // SAFETY: as the `data` read above.
            node = unsafe { (*node).next };
        }
        // Free the list AND each ICalComponent — list_sync passes
        // ownership of every element to the caller.
        //
        // SAFETY: `out_list` is the non-null list we own, freed exactly once
        // and never read after; `g_object_unref_destroy_notify` has the
        // `GDestroyNotify` signature and every element is a GObject whose ref
        // we own (so releasing them here is right, not a double-free).
        unsafe { sys::g_slist_free_full(out_list, sys::g_object_unref_destroy_notify) }
        Ok(out)
    }

    /// Expand every component in `[start_unix, end_unix)` (POSIX seconds,
    /// UTC) into its concrete occurrences by applying each one's RRULE.
    /// Unlike [`get_object_strings`](Self::get_object_strings) — which only
    /// ever returns master components — a recurring event yields **one
    /// [`EventInstance`] per occurrence** inside the window (so a daily
    /// meeting over a 30-day window returns ~30 instances). Non-recurring
    /// events in the window come back as a single instance.
    ///
    /// The window bounds expansion at **both** ends. A `FREQ=DAILY` series
    /// with no `UNTIL`/`COUNT` is capped at the top by the range you pass,
    /// never expanded unboundedly; and the iterator is fast-forwarded to
    /// `start_unix` before it is stepped (#1195), so a rule whose `DTSTART`
    /// is years in the past costs its in-window occurrences rather than its
    /// whole history — which is what used to make such a series come back
    /// *empty*. Neither is the only bound: a sub-hourly rule fills any window
    /// with more occurrences than a UI can use, so each component is
    /// additionally capped by
    /// [`MAX_OCCURRENCES_PER_COMPONENT`]/[`EXPANSION_BYTES_BUDGET`] and
    /// truncated with one `warn!` (#1179).
    ///
    /// Implementation: we fetch every master component (`#t`) and expand
    /// each one with libical's **core recurrence iterator**
    /// (`i_cal_recur_iterator_new` / `_next`) over the window — the engine
    /// the higher-level `e_cal_*_generate_instances_*` helpers wrap. Driving
    /// the iterator directly keeps expansion a pure function of the component
    /// we already hold, independent of EDS backend state. The component's
    /// `EXDATE` properties (cancelled occurrences) are excluded and its
    /// `RDATE` properties (extra one-off occurrences) added — see
    /// [`expand_component`].
    pub fn generate_instances(&self, start_unix: i64, end_unix: i64) -> Result<Vec<EventInstance>> {
        // Fetch every master component. We need the live `ICalComponent*`
        // (not the iCal string) to expand, so we walk the GSList ourselves
        // rather than going through `get_object_strings`.
        let s = CString::new("#t").expect("static sexp has no interior NUL");
        let mut out_list: *mut sys::GSList = ptr::null_mut();
        let mut err: *mut sys::GError = ptr::null_mut();
        // SAFETY: P1/P2/P3, `s` is the live static-sexp `CString` above, and
        // `out_list` is a live local initialised to null for the out-param.
        let ok = unsafe {
            sys::e_cal_client_get_object_list_sync(
                self.raw,
                s.as_ptr(),
                &mut out_list,
                ptr::null_mut(),
                &mut err,
            )
        };
        if ok == 0 {
            return Err(take_error(err).unwrap_or_else(|| anyhow!("get_object_list_sync failed")));
        }

        let mut out: Vec<EventInstance> = Vec::new();
        let mut node = out_list;
        while !node.is_null() {
            // SAFETY: `node` is non-null (loop condition) and names a node of
            // the list we own until the free below (`sys::GSList` mirrors
            // glib's layout `#[repr(C)]`; nothing mutates it, P3).
            let comp = unsafe { (*node).data }.cast::<sys::ICalComponent>();
            if !comp.is_null() {
                // SAFETY: `expand_component`'s contract is a live, borrowed
                // `ICalComponent*` — `comp` is a non-null element of this
                // list, alive until the free below, and the callee frees
                // nothing it does not itself create.
                unsafe { expand_component(comp, start_unix, end_unix, &mut out) }
            }
            // SAFETY: as the `data` read above.
            node = unsafe { (*node).next };
        }

        // Free the list AND each ICalComponent — list_sync transferred
        // ownership of every element to us.
        if !out_list.is_null() {
            // SAFETY: non-null (checked), ours, freed exactly once and never
            // read after; every element is a GObject whose ref we own, which
            // is what `g_object_unref_destroy_notify` releases.
            unsafe { sys::g_slist_free_full(out_list, sys::g_object_unref_destroy_notify) }
        }
        Ok(out)
    }
}

/// Hard ceiling on how many occurrences a single component may contribute to
/// one expansion (#1179). A `FREQ=MINUTELY` invite over the calendar's 43-day
/// window is 61 380 occurrences and every one of them costs the *consumer*
/// work too (`hytte-services` re-parses `EventInstance::ical` per instance),
/// so this is a budget on the whole pipeline, not just on this function. No
/// UI in this tree can show 10 000 rows; a series that hits this cap is
/// truncated, with one `warn!` naming its UID.
pub const MAX_OCCURRENCES_PER_COMPONENT: usize = 10_000;

/// The other half of the #1179 work budget: the number of *bytes* of iCal
/// metadata one component's occurrences may commit downstream, counted as
/// `occurrences × ical.len()`. The string itself is shared (one [`Arc<str>`]
/// per component), so this does not bound *this* crate's allocation — it
/// bounds the parsing every consumer does per instance, which a component
/// with a large VEVENT body would otherwise blow past long before
/// [`MAX_OCCURRENCES_PER_COMPONENT`] binds. Whichever cap binds first wins.
pub const EXPANSION_BYTES_BUDGET: usize = 4 * 1024 * 1024;

/// Hard ceiling on recurrence-iterator steps for one component — the original
/// #29 guard, kept as the backstop for a rule that emits nothing while still
/// looping, which the occurrence budget above cannot bound.
///
/// It counts *steps*, so until #1195 it bounded the wrong quantity: the
/// iterator was always driven from `DTSTART`, so a `FREQ=HOURLY` series whose
/// `DTSTART` is 2014 burned ~105 000 steps just crossing the twelve years
/// between there and the window, tripped this cap **before emitting
/// anything**, and returned 0 occurrences over the calendar's 43-day window
/// for a rule that has 1 032 of them — i.e. every long-standing hourly event
/// was simply absent from the panel. (#1190's doc claimed the guard only
/// bounded rules that never reach the window; that was false there too.)
/// [`skip_iterator_to_window`] now moves the iterator to the window before the
/// loop starts, so what this counts is in-window steps and the occurrence
/// budget is what binds in practice.
///
/// It still binds on the paths where the skip is not applied: an RRULE
/// carrying `COUNT` (libical refuses to fast-forward it, because starting
/// late would change which occurrences the count selects — such a rule is
/// finite by construction, so only a `COUNT` above this cap is truncated by
/// it), and a sub-day frequency (HOURLY/MINUTELY/SECONDLY) with `INTERVAL >
/// 1`, where [`skip_iterator_to_window`] deliberately declines the skip
/// itself (#1206 HIGH-1) rather than re-anchor onto the wrong grid.
const MAX_RECUR_ITERATIONS: u32 = 100_000;

/// How far *before* the window start [`skip_iterator_to_window`] asks libical
/// to re-anchor the iterator (#1195).
///
/// The skip target is an absolute instant, but the rule it re-anchors is
/// evaluated in `DTSTART`'s own frame — which may be a named zone, may be
/// floating (resolved against the viewer's local zone, #388), and for a DATE
/// value carries no time-of-day at all. Rather than reproduce libical's
/// internal conversion and be wrong at one edge, the skip deliberately
/// undershoots by two days: comfortably more than the ±14 h of real-world UTC
/// offset plus a day of DATE rounding, and cheap — the occurrences it
/// over-generates are dropped by [`Emitter::emit`]'s window check, at most
/// 2 880 extra steps even for `FREQ=MINUTELY`. Correctness of the *output*
/// never rests on this number being exactly right, only on it being large
/// enough: the emitted set is filtered against the real window either way.
const SKIP_BACKOFF_SECS: i64 = 2 * 86_400;

/// Why an expansion stopped short of the window's full occurrence set, so the
/// single `warn!` at the end of [`expand_component`] names the cause rather
/// than guessing — the two have opposite remedies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Truncation {
    /// [`MAX_OCCURRENCES_PER_COMPONENT`] or [`EXPANSION_BYTES_BUDGET`] spent —
    /// the component really does have more occurrences in the window than any
    /// UI in this tree can show.
    Budget,
    /// [`MAX_RECUR_ITERATIONS`] steps without reaching the end of the window.
    /// Since #1195 this is reachable for a `COUNT` rule whose count exceeds
    /// the cap (the shape libical itself refuses to fast-forward) or, since
    /// #1206, an old enough sub-day-frequency rule with `INTERVAL > 1` (the
    /// shape [`skip_iterator_to_window`] itself declines to fast-forward).
    IterationGuard,
}

/// Fast-forward a freshly-created recurrence iterator to the query window, so
/// the expansion loop's steps are in-window steps (#1195).
///
/// `*iter` in and out is always the iterator the caller should keep driving —
/// almost always unchanged, but rebuilt fresh from `rule`/`dtstart` in the one
/// case libical answers a refusal by mutating the iterator's internal state
/// first before this function can hand it back (#1206 MEDIUM-2; see
/// `sys::i_cal_recur_iterator_set_start`'s doc for which refusals do that).
///
/// Returns `true` when the skip actually re-anchored `*iter`. `false` means
/// "iterate from `DTSTART` as before" and is not an error; it happens for
/// four reasons: the series already starts inside (or just before) the
/// window, so there is nothing to skip; the skip time could not be built; the
/// rule is one of the three sub-day frequencies (HOURLY/MINUTELY/SECONDLY)
/// with `INTERVAL > 1`, where libical recovers the post-skip phase from a
/// single calendar field rather than the elapsed interval count and would
/// re-anchor the series onto the wrong grid (#1206 HIGH-1); or libical itself
/// **refused** — for an RRULE carrying `COUNT` (starting late would change
/// which occurrences the count selects), a `FREQ=YEARLY` rule whose
/// day-of-year expansion errors, or a first re-anchored instance past
/// libical's `MAX_TIME_T_YEAR`.
///
/// # Safety
///
/// `*iter` must be a live `ICalRecurIterator*` that has not been stepped yet
/// — libical re-anchors the rule's state, so a partially-consumed iterator
/// would silently restart. `rule` and `dtstart` must be the same live,
/// borrowed values `*iter` was built from ([`sys::i_cal_recur_iterator_new`]),
/// and must stay alive for as long as the caller keeps using the iterator
/// this function hands back — a rebuild borrows them again. This function
/// frees at most the iterator it was given (never `rule`/`dtstart`) and, on a
/// rebuild, hands back a new owned iterator in `*iter`; the caller frees
/// whatever `*iter` holds when done, exactly once.
unsafe fn skip_iterator_to_window(
    iter: &mut *mut sys::ICalRecurIterator,
    rule: *mut sys::ICalRecurrence,
    dtstart: *mut sys::ICalTime,
    dtstart_unix: i64,
    window_start: i64,
) -> bool {
    let Some(target) = window_start.checked_sub(SKIP_BACKOFF_SECS) else {
        return false;
    };
    if target <= dtstart_unix {
        // The series already starts at or after the target. libical's contract
        // for this call is a time between DTSTART and UNTIL, and there would be
        // nothing to gain anyway.
        return false;
    }
    // SAFETY: `rule` is the live, borrowed `ICalRecurrence` this function's
    // contract guarantees (the same one `*iter` was built from); both getters
    // are plain field reads with no ownership transfer.
    let freq = unsafe { sys::i_cal_recurrence_get_freq(rule) };
    // SAFETY: as the `freq` read above.
    let interval = unsafe { sys::i_cal_recurrence_get_interval(rule) };
    let sub_day = matches!(
        freq,
        sys::I_CAL_HOURLY_RECURRENCE | sys::I_CAL_MINUTELY_RECURRENCE | sys::I_CAL_SECONDLY_RECURRENCE
    );
    if sub_day && interval != 1 {
        // libical recovers the post-skip phase for these three frequencies
        // from a single calendar field (`icalrecur.c`'s `__iterator_set_start`:
        // `abs(istart.hour - rstart.hour) % interval` and its minute/second
        // analogues), not the elapsed interval count — so an `INTERVAL > 1`
        // series would land on the wrong grid (#1206 HIGH-1). Declining here
        // and falling back to stepping from `DTSTART` is correct for every
        // interval; `INTERVAL == 1` is the one case where hour/minute/second
        // modulo 1 is trivially the phase DTSTART itself has, which is why
        // #1195's headline (plain `FREQ=HOURLY`) is unaffected.
        return false;
    }
    // SAFETY: `i_cal_timezone_get_utc_timezone` is nullary and returns
    // libical's process-wide singleton — borrowed, never unref'd.
    let utc = unsafe { sys::i_cal_timezone_get_utc_timezone() };
    // SAFETY: the constructor takes a plain `time_t`, a DATE flag (0 — this is
    // an absolute instant, not an all-day value) and the borrowed singleton
    // above; it returns a **new** `ICalTime` ref this scope owns and releases
    // on every path below.
    let start = unsafe { sys::i_cal_time_new_from_timet_with_zone(target, 0, utc) };
    if start.is_null() {
        return false;
    }
    // SAFETY: `*iter` is the live, unstepped iterator this function's contract
    // guarantees, and `start` the live time just built — borrowed for the call
    // only (the iterator copies the value out; its `sys` declaration says so).
    let moved = unsafe { sys::i_cal_recur_iterator_set_start(*iter, start) };
    // SAFETY: the new ref taken above, released exactly once and never read
    // after, on every path below.
    unsafe { sys::g_object_unref(start) }
    if moved != 0 {
        return true;
    }
    // libical refused, and for two of its three refusal reasons it has
    // already mutated `*iter`'s internal state before answering zero (#1206
    // MEDIUM-2 — see `sys::i_cal_recur_iterator_set_start`'s doc), so the only
    // iterator that genuinely behaves like a fresh DTSTART-anchored one is an
    // actual fresh one; rebuilding costs nothing this path wasn't already
    // going to spend on the fallback loop.
    //
    // SAFETY: `*iter` is the live iterator this function's contract
    // guarantees and has not been freed yet; freeing it here (and replacing
    // it below) is what makes "false ⇒ iterate from DTSTART as before" true
    // unconditionally rather than only for the `COUNT` refusal.
    unsafe { sys::i_cal_recur_iterator_free(*iter) }
    // SAFETY: `rule` and `dtstart` are the same live, borrowed values `*iter`
    // was originally built from (this function's contract); building a new
    // iterator from them is exactly `i_cal_recur_iterator_new`'s contract.
    *iter = unsafe { sys::i_cal_recur_iterator_new(rule, dtstart) };
    false
}

/// Expand one master `comp` over `[start_unix, end_unix)` (POSIX UTC
/// seconds), pushing each occurrence into `out`.
///
/// - **Non-recurring** (no RRULE, no RDATE): emit a single [`EventInstance`]
///   if its DTSTART falls before `end_unix` (the calendar service does the
///   has-it-ended filtering).
/// - **Recurring** (RRULE present): drive libical's core recurrence iterator
///   (`i_cal_recur_iterator_new` / `_next`), emitting one instance per
///   occurrence inside `[start_unix, end_unix)` and stopping once occurrences
///   pass `end_unix` (so an unbounded series is window-capped). The iterator
///   is re-anchored on the window before the first step via
///   [`skip_iterator_to_window`] (#1195), so the occurrences between `DTSTART`
///   and the window are never generated at all; it falls back to stepping
///   from `DTSTART` for a rule shape libical will not fast-forward (an RRULE
///   carrying `COUNT`) or one this crate itself declines to fast-forward (a
///   sub-day frequency with `INTERVAL > 1`, #1206 HIGH-1). Per-occurrence
///   duration is `DTEND − DTSTART` (or 0 if absent; the service fabricates a
///   UI duration).
///
/// On top of the RRULE/DTSTART occurrences, the component's recurrence-set
/// modifiers are applied (RFC 5545 §3.8.5):
///
/// - **EXDATE** (cancelled occurrences): each `EXDATE` value — there may be
///   several `EXDATE` properties, since libical splits a comma-separated list
///   into one property apiece — is normalised to UTC seconds and any matching
///   occurrence is dropped. DATE and DATE-TIME forms both normalise through
///   the same [`ical_time_to_unix`] the iterator output uses, so an all-day
///   `EXDATE;VALUE=DATE` matches an all-day occurrence and a timed one matches
///   a timed occurrence. An `EXDATE` that matches no occurrence is a harmless
///   no-op.
/// - **RDATE** (extra one-off occurrences): each in-window `RDATE` is added,
///   deduped against the RRULE-expanded starts and skipped if it coincides
///   with an `EXDATE` (per RFC, EXDATE wins). RDATE can stand alone (no
///   RRULE), adding occurrences alongside DTSTART.
///
/// ## Bounded work (#1179)
///
/// The window is not by itself a bound on *work*: `FREQ=MINUTELY` over the
/// 43-day window the calendar service asks for is 61 380 occurrences, and
/// before #1179 each of those was checked against the emitted ones with a
/// linear `Vec::contains` and carried its own clone of the component's iCal
/// string — 1.9 × 10⁹ comparisons and ~9 MB of duplicated string, measured at
/// **13.6 s** for one such invite, on every refresh, per source. Three things
/// bound it now:
///
/// - the emitted set is a [`HashSet`] keyed by the occurrence's
///   `(start, end)`, so dedup is O(1) per occurrence rather than O(n);
/// - the iCal string is one [`Arc<str>`] shared by every occurrence of the
///   component, so the per-occurrence cost is a refcount bump;
/// - [`MAX_OCCURRENCES_PER_COMPONENT`] and [`EXPANSION_BYTES_BUDGET`] cap what
///   one component may contribute, whichever binds first, and truncation logs
///   exactly one `warn!` naming the component's UID.
///
/// A fourth bound arrived with #1195, and it is the one that decides whether
/// the component appears at all: the iterator is skipped to the window before
/// the loop starts, so the work is proportional to the occurrences *in* the
/// window rather than to the age of the series. Without it a `FREQ=HOURLY`
/// rule with a 2014 `DTSTART` spent its whole [`MAX_RECUR_ITERATIONS`] budget
/// on occurrences nobody asked for and returned **nothing** — every
/// long-standing hourly event missing from the panel, with a `warn!` that read
/// like a truncation rather than a disappearance.
///
/// # Safety
///
/// `comp` must be a live `ICalComponent*` (a VEVENT). Borrowed — never freed
/// here. All owned libical objects created within are released.
unsafe fn expand_component(
    comp: *mut sys::ICalComponent,
    start_unix: i64,
    end_unix: i64,
    out: &mut Vec<EventInstance>,
) {
    // DTSTART (owned). No DTSTART ⇒ undatable ⇒ skip.
    //
    // SAFETY: `comp` is a live component by this function's own contract; the
    // accessor returns a **new** `ICalTime` ref (libical-glib's "get_dtstart"
    // convention, `sys`) that this function releases on every exit path.
    let dtstart = unsafe { sys::i_cal_component_get_dtstart(comp) };
    // SAFETY: `ical_time_to_unix` accepts null or a live borrowed `ICalTime*`
    // — `dtstart` is exactly that, and is not freed until below.
    let Some(dtstart_unix) = (unsafe { ical_time_to_unix(dtstart) }) else {
        if !dtstart.is_null() {
            // SAFETY: the new ref taken above, released exactly once on this
            // early-return path.
            unsafe { sys::g_object_unref(dtstart) }
        }
        return;
    };
    // SAFETY: as the `ical_time_to_unix` call above — null or live borrow, and
    // `dtstart` is live here (the `else` arm returned).
    let all_day = unsafe { ical_time_is_date(dtstart) };

    // Duration = DTEND − DTSTART when DTEND is present; else 0.
    // SAFETY: `comp` is live; this returns a new `ICalTime` ref (or null),
    // released a few lines below.
    let dtend = unsafe { sys::i_cal_component_get_dtend(comp) };
    // SAFETY: null or a live borrow, as above.
    let duration = match unsafe { ical_time_to_unix(dtend) } {
        Some(e) if e >= dtstart_unix => e - dtstart_unix,
        _ => 0,
    };
    if !dtend.is_null() {
        // SAFETY: the new ref taken above, released exactly once; `dtend` is
        // not used again.
        unsafe { sys::g_object_unref(dtend) }
    }

    // The component's iCal serialisation (metadata: UID/SUMMARY/LOCATION/…),
    // identical across a series' occurrences — so it is materialised **once**
    // and every occurrence shares this one allocation (#1179).
    // SAFETY: `component_ical_string` wants a live `ICalComponent*`, which
    // `comp` is by this function's contract; it borrows and frees nothing.
    let ical: Arc<str> = Arc::from(unsafe { component_ical_string(comp) });

    // Recurrence-set modifiers, normalised to UTC seconds the same way every
    // occurrence is, so comparisons are apples-to-apples regardless of DATE
    // vs DATE-TIME / TZID. EXDATE is a membership set (hashed: it is probed
    // once per occurrence, so a linear scan here is quadratic in the same way
    // the emitted-set scan was); RDATE a list of extra starts, kept ordered
    // because emission order is part of this function's output contract.
    //
    // SAFETY: `collect_property_times` wants a live, borrowed
    // `ICalComponent*` — `comp` is exactly that (P1, this function's own
    // contract) — plus a property-kind discriminant; `I_CAL_EXDATE_PROPERTY`
    // is the `ICalPropertyKind` value libical defines (pinned at its `sys`
    // declaration), and `is_rdate = false` matches it, so the value is read
    // through the accessor for the type it actually has.
    let exdates: HashSet<i64> =
        unsafe { collect_property_times(comp, sys::I_CAL_EXDATE_PROPERTY, false) }
            .into_iter()
            .collect();
    // SAFETY: as the EXDATE call above, with the other half of the pairing —
    // `I_CAL_RDATE_PROPERTY` is libical's RDATE discriminant and `is_rdate =
    // true` selects the `ICalDatetimeperiod` accessor an RDATE value needs.
    let rdates = unsafe { collect_property_times(comp, sys::I_CAL_RDATE_PROPERTY, true) };

    let mut emit = Emitter::new(ical, exdates, duration, all_day, start_unix, end_unix);

    // Set once the work budget (or the iteration guard) cut the expansion
    // short, so the `warn!` below fires exactly once per component however
    // many occurrences were dropped. First cause wins — it is the one that
    // stopped the loop.
    let mut truncated: Option<Truncation> = None;

    // RRULE present?
    //
    // SAFETY: `comp` is live (this function's contract) and
    // `I_CAL_RRULE_PROPERTY` is libical's `ICalPropertyKind` discriminant for
    // an RRULE; the "first property" accessor returns a **new** ref (or null),
    // released at the end of this branch.
    let rrule_prop =
        unsafe { sys::i_cal_component_get_first_property(comp, sys::I_CAL_RRULE_PROPERTY) };
    if rrule_prop.is_null() {
        // No RRULE: DTSTART is the (sole) base occurrence; RDATE may add more.
        if !emit.emit(out, dtstart_unix) {
            truncated = truncated.or(Some(Truncation::Budget));
        }
    } else {
        // Recurring: iterate occurrences from DTSTART.
        // SAFETY: `rrule_prop` is the non-null property ref we hold; reading
        // its value yields a new `ICalRecurrence` ref (or null), released
        // below.
        let rule = unsafe { sys::i_cal_property_get_rrule(rrule_prop) };
        if !rule.is_null() {
            // SAFETY: both arguments are live and borrowed for the iterator's
            // whole life — `rule` and `dtstart` are released only after
            // `i_cal_recur_iterator_free` below, which is what libical's
            // iterator requires of the rule and the start time it is built
            // from.
            let mut iter = unsafe { sys::i_cal_recur_iterator_new(rule, dtstart) };
            if !iter.is_null() {
                // Skip straight to the window before stepping (#1195). Without
                // this the loop below walks every occurrence since DTSTART:
                // for an hourly series begun in 2014 that is ~105 000 steps
                // before the first one the window wants, which trips
                // MAX_RECUR_ITERATIONS and hands back an *empty* series. The
                // call declines (leaving `iter` as built) when there is
                // nothing to skip, when the rule is a sub-day frequency
                // (HOURLY/MINUTELY/SECONDLY) with INTERVAL > 1 — libical's
                // post-skip phase recovery only accounts for one calendar
                // field there and would land the series on the wrong grid
                // (#1206 HIGH-1) — or when libical itself refuses (e.g. a
                // COUNT rule), in which case the guard below is doing its
                // original job. On a refusal it may also rebuild `iter` from
                // scratch (#1206 MEDIUM-2, since two of libical's three
                // refusal paths mutate the iterator before answering
                // failure) — either way, `iter` below is the one to keep
                // using.
                //
                // SAFETY: `iter` is the non-null iterator created directly
                // above and not yet stepped; `rule` and `dtstart` are the
                // same live, borrowed values it was built from, kept alive
                // by this scope across the iterator's whole life — whether
                // that ends up being the one above or a replacement the
                // callee builds from them. The callee frees nothing it did
                // not itself create and hands back the iterator this scope
                // now owns.
                unsafe {
                    skip_iterator_to_window(&mut iter, rule, dtstart, dtstart_unix, start_unix);
                }
                if !iter.is_null() {
                    // A defensive cap: even with the time-window stop condition, a
                    // pathological rule shouldn't loop forever. After a successful
                    // skip it counts in-window steps, so the occurrence budget
                    // binds long before it does.
                    let mut guard = 0u32;
                    loop {
                        guard += 1;
                        if guard > MAX_RECUR_ITERATIONS {
                            truncated = truncated.or(Some(Truncation::IterationGuard));
                            break;
                        }
                        // SAFETY: `iter` is the non-null iterator we own and have
                        // not yet freed; each step returns a **new** `ICalTime`
                        // ref (or a null-time sentinel), released on both paths
                        // below before the next step.
                        let occ = unsafe { sys::i_cal_recur_iterator_next(iter) };
                        // SAFETY: null or a live borrow of the ref just taken.
                        let Some(occ_unix) = (unsafe { ical_time_to_unix(occ) }) else {
                            // null-time ⇒ series exhausted.
                            if !occ.is_null() {
                                // SAFETY: the new ref from this step, released
                                // exactly once (this path breaks the loop).
                                unsafe { sys::g_object_unref(occ) }
                            }
                            break;
                        };
                        if !occ.is_null() {
                            // SAFETY: the new ref from this step, released exactly
                            // once — `occ` is not read again this iteration (only
                            // the `i64` extracted from it is).
                            unsafe { sys::g_object_unref(occ) }
                        }
                        if occ_unix >= end_unix {
                            break; // past the window ⇒ done
                        }
                        if !emit.emit(out, occ_unix) {
                            // Budget spent: stop stepping the iterator instead of
                            // running it to the end of the window for nothing.
                            truncated = truncated.or(Some(Truncation::Budget));
                            break;
                        }
                    }
                    // SAFETY: `iter` is the non-null iterator this scope now
                    // owns — whether the one created above or the
                    // replacement `skip_iterator_to_window` built — freed
                    // exactly once here and never stepped after.
                    unsafe { sys::i_cal_recur_iterator_free(iter) }
                }
            }
            // SAFETY: the new `ICalRecurrence` ref taken above, released
            // exactly once and only after the iterator built from it is freed.
            unsafe { sys::g_object_unref(rule) }
        }
        // SAFETY: the new property ref taken above, released exactly once on
        // this branch (the `if` arm never took one).
        unsafe { sys::g_object_unref(rrule_prop) }
    }

    // RDATE: extra one-off occurrences within the window, deduped against the
    // RRULE-expanded set and subject to the same EXDATE exclusion.
    for rd in rdates {
        if !emit.emit(out, rd) {
            truncated = truncated.or(Some(Truncation::Budget));
            break;
        }
    }

    match truncated {
        None => {}
        Some(Truncation::Budget) => tracing::warn!(
            uid = uid_from_ical(&emit.ical).unwrap_or("(no UID)"),
            emitted = emit.emitted.len(),
            max_occurrences = emit.max_occurrences,
            "hytte-ecal: recurrence expansion truncated — this component alone \
             would fill the window with occurrences; showing the first ones only",
        ),
        Some(Truncation::IterationGuard) => tracing::warn!(
            uid = uid_from_ical(&emit.ical).unwrap_or("(no UID)"),
            emitted = emit.emitted.len(),
            max_iterations = MAX_RECUR_ITERATIONS,
            "hytte-ecal: recurrence expansion hit the iteration guard before \
             covering the window — a COUNT rule too long to fast-forward past \
             (#1195); the occurrences after the cap are missing",
        ),
    }

    // SAFETY: the new `dtstart` ref taken at the top, released exactly once on
    // this (the only remaining) exit path — strictly after the recurrence
    // iterator that borrowed it was freed.
    unsafe { sys::g_object_unref(dtstart) }
}

/// Accumulates one component's occurrences into the caller's output vector,
/// applying — in this order — EXDATE exclusion, the query window, dedup, and
/// the #1179 work budget. Split out of [`expand_component`] so the budget
/// lives in one readable place (and so that function stays under clippy's
/// `too_many_lines`); it holds no raw pointer and needs no `unsafe`.
struct Emitter {
    /// The component's serialisation, shared by every occurrence it emits.
    ical: Arc<str>,
    /// Cancelled starts (EXDATE), hashed because this is probed once per
    /// occurrence — a linear scan here is quadratic in the same way the
    /// emitted-set scan was before #1179.
    exdates: HashSet<i64>,
    /// `(start, end)` of every occurrence already pushed, so an RDATE doesn't
    /// double up one the RRULE (or DTSTART) already produced.
    emitted: HashSet<(i64, i64)>,
    /// `DTEND − DTSTART`, applied to every occurrence of the series.
    duration: i64,
    all_day: bool,
    window_start: i64,
    window_end: i64,
    /// Whichever of [`MAX_OCCURRENCES_PER_COMPONENT`] and
    /// [`EXPANSION_BYTES_BUDGET`] binds first for this component's body size.
    max_occurrences: usize,
}

impl Emitter {
    fn new(
        ical: Arc<str>,
        exdates: HashSet<i64>,
        duration: i64,
        all_day: bool,
        window_start: i64,
        window_end: i64,
    ) -> Self {
        // At least one occurrence, so a component with an absurdly large body
        // still yields its first instance rather than vanishing.
        let max_occurrences = MAX_OCCURRENCES_PER_COMPONENT
            .min(EXPANSION_BYTES_BUDGET / ical.len().max(1))
            .max(1);
        Self {
            ical,
            exdates,
            emitted: HashSet::new(),
            duration,
            all_day,
            window_start,
            window_end,
            max_occurrences,
        }
    }

    /// Push the occurrence starting at `occ_unix`, if it survives the filters.
    ///
    /// Returns `false` **only** once the work budget is spent, so a caller
    /// stops iterating instead of spinning the recurrence iterator for
    /// occurrences it would discard. A merely filtered-out occurrence (EXDATE,
    /// outside the window, duplicate) returns `true`: it consumed no budget.
    fn emit(&mut self, out: &mut Vec<EventInstance>, occ_unix: i64) -> bool {
        // EXDATE excludes; the window bounds the rest. An occurrence is kept
        // when it starts before the window end and its end is at/after the
        // window start (so it overlaps the window).
        if self.exdates.contains(&occ_unix) {
            return true;
        }
        let end_unix = occ_unix + self.duration;
        if occ_unix >= self.window_end || end_unix < self.window_start {
            return true;
        }
        if self.emitted.contains(&(occ_unix, end_unix)) {
            return true;
        }
        if self.emitted.len() >= self.max_occurrences {
            return false;
        }
        self.emitted.insert((occ_unix, end_unix));
        out.push(EventInstance {
            ical: Arc::clone(&self.ical),
            start_unix: occ_unix,
            end_unix,
            all_day: self.all_day,
        });
        true
    }
}

/// The value of the first `UID:` property line in an iCal serialisation, for
/// log lines only (the expansion budget's `warn!` has to name *which*
/// component it truncated, and the component pointer is long gone by the time
/// a human reads the log).
///
/// Deliberately a string scan over the serialisation we already hold rather
/// than another libical accessor: it costs no FFI surface, and the worst case
/// for a wrong answer is a mislabelled warning. A folded (continued) UID is
/// reported as its first line.
fn uid_from_ical(ical: &str) -> Option<&str> {
    ical.lines()
        .find_map(|line| line.strip_prefix("UID:"))
        .map(str::trim_end)
        .filter(|uid| !uid.is_empty())
}

/// Collect every value of the repeated date-valued property `kind` on `comp`
/// (EXDATE or RDATE) as UTC POSIX seconds. libical exposes one property per
/// value (it splits a comma-separated list), so we walk first/next.
///
/// `is_rdate` selects the value accessor: EXDATE carries a plain `ICalTime`,
/// while RDATE carries an `ICalDatetimeperiod` (a date-time *or* a period,
/// whose start we take). Null-times / unparseable values are skipped. Every
/// owned libical object on each path is released.
///
/// # Safety
///
/// `comp` must be a live `ICalComponent*`. Borrowed — never freed here.
unsafe fn collect_property_times(
    comp: *mut sys::ICalComponent,
    kind: c_int,
    is_rdate: bool,
) -> Vec<i64> {
    let mut times = Vec::new();
    // SAFETY: `comp` is live (this function's contract) and `kind` is an
    // `ICalPropertyKind` discriminant; the accessor returns a **new** property
    // ref (or null), released at the end of each iteration.
    let mut prop = unsafe { sys::i_cal_component_get_first_property(comp, kind) };
    while !prop.is_null() {
        let unix = if is_rdate {
            // SAFETY: `prop` is the non-null property ref we hold, and the
            // caller passed `is_rdate` for an RDATE `kind` — so it really is
            // an RDATE property, which is what this callee requires.
            unsafe { rdate_property_to_unix(prop) }
        } else {
            // SAFETY: `prop` is a live EXDATE property (the `is_rdate = false`
            // arm); reading its value yields a new `ICalTime` ref or null.
            let tt = unsafe { sys::i_cal_property_get_exdate(prop) };
            // SAFETY: null or a live borrow of the ref just taken.
            let u = unsafe { ical_time_to_unix(tt) };
            if !tt.is_null() {
                // SAFETY: that new ref, released exactly once; only the `i64`
                // read out of it is used afterwards.
                unsafe { sys::g_object_unref(tt) }
            }
            u
        };
        if let Some(u) = unix {
            times.push(u);
        }
        // SAFETY: the property ref from this iteration, released exactly once
        // — `get_next_property` walks the component's own cursor and does not
        // need the previous property to still be held.
        unsafe { sys::g_object_unref(prop) }
        // SAFETY: as the `get_first_property` call above; the cursor was
        // established by it, on this same live `comp` and `kind`.
        prop = unsafe { sys::i_cal_component_get_next_property(comp, kind) };
    }
    times
}

/// Extract an RDATE property's start as UTC POSIX seconds. RDATE values come
/// as an `ICalDatetimeperiod`: prefer its plain date-time; fall back to the
/// start of its period form. Returns `None` for an unusable value. Frees every
/// owned libical object it touches.
///
/// # Safety
///
/// `prop` must be a live RDATE `ICalProperty*`. Borrowed — never freed here.
unsafe fn rdate_property_to_unix(prop: *mut sys::ICalProperty) -> Option<i64> {
    // SAFETY: `prop` is a live RDATE property by this function's contract;
    // reading its value yields a **new** `ICalDatetimeperiod` ref (or null),
    // released at the end.
    let dtp = unsafe { sys::i_cal_property_get_rdate(prop) };
    if dtp.is_null() {
        return None;
    }
    // Date-time form first.
    // SAFETY: `dtp` is the non-null value we own; this returns a new
    // `ICalTime` ref (or null) released two lines down.
    let tt = unsafe { sys::i_cal_datetimeperiod_get_time(dtp) };
    // SAFETY: null or a live borrow of the ref just taken.
    let mut result = unsafe { ical_time_to_unix(tt) };
    if !tt.is_null() {
        // SAFETY: that new ref, released exactly once.
        unsafe { sys::g_object_unref(tt) }
    }
    // Period form: take its start.
    if result.is_none() {
        // SAFETY: `dtp` is still the live value we own; this returns a new
        // `ICalPeriod` ref (or null), released below.
        let period = unsafe { sys::i_cal_datetimeperiod_get_period(dtp) };
        if !period.is_null() {
            // SAFETY: `period` is non-null and live; its start comes back as a
            // new `ICalTime` ref (or null), released below.
            let start = unsafe { sys::i_cal_period_get_start(period) };
            // SAFETY: null or a live borrow of the ref just taken.
            result = unsafe { ical_time_to_unix(start) };
            if !start.is_null() {
                // SAFETY: that new ref, released exactly once.
                unsafe { sys::g_object_unref(start) }
            }
            // SAFETY: the period ref above, released exactly once and only
            // after the start time read out of it.
            unsafe { sys::g_object_unref(period) }
        }
    }
    // SAFETY: the datetimeperiod ref taken at the top, released exactly once
    // on every path that reaches here, after everything read out of it.
    unsafe { sys::g_object_unref(dtp) }
    result
}

/// Pure-libical recurrence expansion of an iCalendar VEVENT string over a
/// UTC window, with **no EDS backend** — exposed so the recurrence path can
/// be exercised hermetically (the crate's unit tests use it). Parses `ical`,
/// expands the first VEVENT it finds, and returns the occurrences.
pub fn expand_ical_for_test(
    ical: &str,
    start_unix: i64,
    end_unix: i64,
) -> Result<Vec<EventInstance>> {
    let comp = parse_vevent(ical)?;
    let mut out = Vec::new();
    // SAFETY: `comp.raw` is the live VEVENT `parse_vevent` just produced and
    // this scope owns until the `drop` below — exactly the borrowed, live
    // `ICalComponent*` `expand_component` requires.
    unsafe { expand_component(comp.raw, start_unix, end_unix, &mut out) }
    drop(comp);
    Ok(out)
}

/// Serialise an `ICalComponent` to its iCal string (empty on failure).
///
/// # Safety
///
/// `comp` must be a live `ICalComponent*`.
unsafe fn component_ical_string(comp: *mut sys::ICalComponent) -> String {
    // SAFETY: `comp` is live by this function's contract; the serialisation
    // comes back as a GLib-allocated string we own.
    let s_ptr = unsafe { sys::i_cal_component_as_ical_string(comp) };
    if s_ptr.is_null() {
        return String::new();
    }
    // SAFETY: non-null (checked) and NUL-terminated, copied before the free.
    let s = unsafe { CStr::from_ptr(s_ptr) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: that GLib-allocated string, freed exactly once, never read after.
    unsafe { sys::g_free(s_ptr.cast::<c_void>()) }
    s
}

/// Convert a borrowed `ICalTime*` to POSIX UTC seconds, or `None` if the
/// pointer is null or libical reports it as the null-time sentinel.
///
/// # Timezone handling (issues #388, #522)
///
/// `i_cal_time_as_timet_with_zone(tt, zone)` treats `zone` as the **source**
/// zone the wall-clock fields are expressed in, for any `tt` that is neither a
/// DATE nor a UTC (`…Z`) value — it does **not** substitute the resolved zone a
/// `TZID` time already carries. Passing the UTC singleton for every time (the
/// pre-#388 behaviour) therefore reads *both* a floating time *and* a
/// resolved-`TZID` time as UTC, shifting each by the viewer's offset on display
/// (a 12:30 event shown as 14:30 in CEST). #388 fixed only the floating case;
/// the `TZID` case stayed broken (#522). We split by what `tt` actually is:
///
/// - **DATE (all-day):** keep the UTC anchor — the display side (`calendar`'s
///   `unix_to_local`) reinterprets the resulting midnight-UTC `time_t` as a
///   local calendar date, so this must stay midnight-UTC or the day would
///   drift. The zone argument is immaterial for a DATE.
/// - **Resolved zone (a `…Z` UTC time, or a `TZID` whose `VTIMEZONE` libical
///   could resolve — registered on the component or a builtin like
///   `Europe/Berlin`):** pass the time's **own** zone as the source, so libical
///   converts *from* that zone to the absolute instant. A UTC time carries the
///   UTC singleton as its own zone, so this is correct for it too (#522).
/// - **Genuinely floating DATE-TIME:** interpret the wall-clock fields in the
///   **local** system zone via chrono's `Local` (DST-correct), not UTC (#388).
///   This also covers a `TZID`'d time whose `VTIMEZONE` libical could not
///   resolve (it then reports the time as floating) — the local zone is the
///   right fallback for the viewer.
///
/// # Safety
///
/// `tt` must be null or a valid `ICalTime*` borrowed from libical.
unsafe fn ical_time_to_unix(tt: *mut sys::ICalTime) -> Option<i64> {
    if tt.is_null() {
        return None;
    }
    let tt_const = tt.cast_const();
    // The four reads below share one premise, cited by each `SAFETY:` line:
    // `tt` is non-null (checked) and a live borrowed `ICalTime*` by this
    // function's contract, and every accessor here is read-only — none takes
    // ownership or frees anything.
    //
    // SAFETY: the premise above; the null-time predicate only reads `tt`.
    if unsafe { sys::i_cal_time_is_null_time(tt_const) } != 0 {
        return None;
    }

    // SAFETY: the premise above; a read-only predicate on the same borrow.
    let is_date = unsafe { sys::i_cal_time_is_date(tt_const) } != 0;
    // SAFETY: as `is_date`.
    let is_utc = unsafe { sys::i_cal_time_is_utc(tt_const) } != 0;
    // SAFETY: as `is_date`, and the zone it returns is the one the time itself
    // holds — **borrowed**, owned by libical, and never unref'd here.
    let own_zone = unsafe { sys::i_cal_time_get_timezone(tt_const) };
    let has_own_zone = !own_zone.is_null();

    if is_date {
        // DATE anchors to midnight-UTC by design: the display side
        // (`calendar`'s `unix_to_local`) reinterprets the resulting
        // midnight-UTC `time_t` as a *local* calendar date, so this must stay
        // midnight-UTC or the day would drift. The zone argument is irrelevant
        // for a DATE (no time-of-day to shift) — pass the UTC singleton.
        //
        // SAFETY: `i_cal_timezone_get_utc_timezone` is nullary and returns
        // libical's process-wide singleton — borrowed, never unref'd (its
        // `sys` declaration says so).
        let utc = unsafe { sys::i_cal_timezone_get_utc_timezone() };
        // SAFETY: `tt_const` is the live borrow checked at the top of this
        // function and `utc` the singleton just taken; the conversion only
        // reads both and allocates nothing.
        return Some(unsafe { sys::i_cal_time_as_timet_with_zone(tt_const, utc.cast_const()) });
    }

    if has_own_zone {
        // Absolute DATE-TIME carrying a resolved zone — a `…Z` (UTC) time or
        // one whose `TZID`'s `VTIMEZONE` libical could resolve (registered on
        // the component, or a builtin like `Europe/Berlin`).
        //
        // `i_cal_time_as_timet_with_zone(tt, zone)` reads `zone` as the *source*
        // zone the wall-clock is expressed in whenever `tt` is a non-DATE,
        // non-UTC value — it does **not** substitute the time's own resolved
        // zone (issue #522; the #388 fix wrongly assumed the argument was
        // ignored for these). So the source zone MUST be the time's own zone:
        // passing the UTC singleton instead reads the wall-clock as UTC, so a
        // `TZID=Europe/Berlin` 12:30 becomes 12:30 UTC and displays as 14:30
        // CEST — the +2h double-shift. A UTC (`…Z`) time carries the UTC
        // singleton as its own zone, so this yields the correct instant for it
        // unchanged.
        //
        // SAFETY: `tt_const` is the live borrow from above and `own_zone` is
        // non-null (this branch) — a zone libical owns and keeps for the
        // process lifetime, borrowed here and never unref'd. Read-only.
        return Some(unsafe {
            sys::i_cal_time_as_timet_with_zone(tt_const, own_zone.cast_const())
        });
    }

    if is_utc {
        // A UTC-flagged time with no attached zone pointer (defensive: some
        // libical values carry the `is_utc` bit without a zone object). It is
        // already absolute — the UTC singleton is the correct source zone.
        //
        // SAFETY: as the DATE branch above — a nullary getter for libical's
        // borrowed, never-unref'd process-wide singleton.
        let utc = unsafe { sys::i_cal_timezone_get_utc_timezone() };
        // SAFETY: as the DATE branch above — a read-only conversion of the
        // live borrow, with the singleton as the source zone.
        return Some(unsafe { sys::i_cal_time_as_timet_with_zone(tt_const, utc.cast_const()) });
    }

    // Genuinely floating DATE-TIME (no zone, not UTC): resolve its wall-clock
    // fields in the local zone rather than assuming UTC (issue #388). Also
    // covers a `TZID`'d time whose `VTIMEZONE` libical could not resolve (it
    // then reports the time as floating) — the local zone is the right
    // fallback for the viewer.
    // SAFETY: `WallClock::from_ical` requires a non-null live `ICalTime*`;
    // `tt` is non-null (checked at the top) and live for this borrow.
    unsafe { WallClock::from_ical(tt) }.to_local_unix()
}

/// The broken-down wall-clock fields of an `ICalTime` (no timezone attached).
/// Factored out so the floating-time → local-instant mapping is unit-testable
/// without a live libical `ICalTime`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WallClock {
    /// Fields as libical hands them back (`gint`); validated on conversion.
    year: i32,
    month: i32,
    day: i32,
    hour: i32,
    minute: i32,
    second: i32,
}

impl WallClock {
    /// Read the broken-down fields off a borrowed `ICalTime*`.
    ///
    /// # Safety
    ///
    /// `tt` must be a valid, non-null `ICalTime*` borrowed from libical.
    unsafe fn from_ical(tt: *mut sys::ICalTime) -> Self {
        let c = tt.cast_const();
        // The six reads below share one premise, cited by each `SAFETY:` line:
        // `tt` is non-null and a live borrowed `ICalTime*` by this function's
        // contract, and each accessor is a read-only field getter returning a
        // `gint` — none takes ownership, frees anything, or can observe a
        // partially-built value (P3: the time belongs to this thread).
        //
        // SAFETY: the premise above.
        let year = unsafe { sys::i_cal_time_get_year(c) };
        // SAFETY: as `year`.
        let month = unsafe { sys::i_cal_time_get_month(c) };
        // SAFETY: as `year`.
        let day = unsafe { sys::i_cal_time_get_day(c) };
        // SAFETY: as `year`.
        let hour = unsafe { sys::i_cal_time_get_hour(c) };
        // SAFETY: as `year`.
        let minute = unsafe { sys::i_cal_time_get_minute(c) };
        // SAFETY: as `year`.
        let second = unsafe { sys::i_cal_time_get_second(c) };
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        }
    }

    /// Interpret these fields as a wall-clock instant in the **local** system
    /// zone and return POSIX UTC seconds. `None` if the fields don't name a
    /// real local instant — a value out of range, or one skipped/ambiguous
    /// across a DST transition (we take `.single()`, so a folded/gapped local
    /// time yields `None` rather than a guess).
    fn to_local_unix(self) -> Option<i64> {
        use chrono::{Local, NaiveDate, NaiveTime, TimeZone as _};

        let month = u32::try_from(self.month).ok()?;
        let day = u32::try_from(self.day).ok()?;
        let hour = u32::try_from(self.hour).ok()?;
        let minute = u32::try_from(self.minute).ok()?;
        let second = u32::try_from(self.second).ok()?;

        let date = NaiveDate::from_ymd_opt(self.year, month, day)?;
        let time = NaiveTime::from_hms_opt(hour, minute, second)?;
        Local
            .from_local_datetime(&date.and_time(time))
            .single()
            .map(|dt| dt.timestamp())
    }
}

/// True iff the borrowed `ICalTime*` is a DATE (all-day, no time-of-day).
///
/// # Safety
///
/// `tt` must be null or a valid `ICalTime*` borrowed from libical.
unsafe fn ical_time_is_date(tt: *mut sys::ICalTime) -> bool {
    // SAFETY: short-circuit — the accessor is reached only when `tt` is
    // non-null, and it is then a live borrowed `ICalTime*` (this function's
    // contract) read without taking ownership.
    !tt.is_null() && unsafe { sys::i_cal_time_is_date(tt.cast_const()) } != 0
}

impl CalClient {
    /// Open a **live, push-based** [`CalClientView`] over this client for the
    /// S-expression `sexp` (use `"#t"` for "every object"). `on_change` is
    /// invoked — coalesced to a bare "something changed, re-read" ping —
    /// whenever EDS reports objects added, modified, or removed (including the
    /// one-shot initial population that `view-start` triggers). It replaces the
    /// poll lag for issue #33: an external client (Endeavour, Evolution, …)
    /// editing a task surfaces in trollshell as soon as the owning thread next
    /// pumps its [`MainContext`].
    ///
    /// The returned [`CalClientView`] must be kept alive for notifications to
    /// keep flowing; dropping it stops the view and disconnects the handlers.
    /// Signals are delivered on the **thread-default `GMainContext` in effect
    /// when this is called** — so call it on a thread that owns a
    /// [`MainContext`] (pushed thread-default) and pump that context. `on_change`
    /// therefore always runs on that same thread, never concurrently with it.
    pub fn watch<F>(&self, sexp: &str, on_change: F) -> Result<CalClientView>
    where
        F: Fn() + 'static,
    {
        let sexp_c = CString::new(sexp).context("sexp contained an interior NUL")?;
        let mut view: *mut sys::ECalClientView = ptr::null_mut();
        let mut err: *mut sys::GError = ptr::null_mut();
        // SAFETY: P1/P2/P3, `sexp_c` is a live `CString` outliving the call,
        // and `view` is a live local initialised to null for the out-param.
        let ok = unsafe {
            sys::e_cal_client_get_view_sync(
                self.raw,
                sexp_c.as_ptr(),
                &mut view,
                ptr::null_mut(),
                &mut err,
            )
        };
        if ok == 0 || view.is_null() {
            return Err(take_error(err).unwrap_or_else(|| anyhow!("get_view_sync failed")));
        }

        // The callback lives in a double-box so we can hand GLib a *thin*
        // `*mut c_void` (the inner `Box<dyn Fn()>`) as each handler's
        // user_data. We own this box on the Rust side and free it in Drop —
        // strictly after every handler that could reach it is disconnected
        // (see `CalClientView::drop`), so no trampoline can read a freed
        // pointer. Hence the connections use a no-op destroy-notify;
        // ownership is ours, not the closures'.
        let boxed: Box<Box<dyn Fn()>> = Box::new(Box::new(on_change));
        let user_data = (&raw const *boxed).cast::<c_void>().cast_mut();

        // Connect all three change signals to one trampoline, **keeping every
        // handler id**: teardown disconnects each one explicitly before the
        // unref (#1179). Relying on `g_object_unref(view)` to disconnect them
        // — what this did before — is only sound if our ref is the last one,
        // and nothing here can prove that: libecal, or a signal emission in
        // flight, may hold another, in which case the handlers outlive the
        // `Box<dyn Fn()>` they point at and the trampoline dereferences freed
        // memory. A `0` id means that connect failed — log but continue,
        // since a partial subscription still beats none (and the safety-net
        // poll backstops anything missed); it is never passed to
        // `g_signal_handler_disconnect`.
        let mut handler_ids: Vec<sys::GULong> = Vec::with_capacity(3);
        for sig in [c"objects-added", c"objects-modified", c"objects-removed"] {
            // SAFETY: `view` is the non-null `ECalClientView*` `get_view_sync`
            // just handed us, `sig` is a `'static` NUL-terminated literal, and
            // `user_data` points at the boxed callback this function owns for
            // longer than every handler (Drop disconnects them all first). The
            // transmute retypes a concrete `extern "C"` fn to the
            // signature-erased `GCallback` GLib stores; GLib calls it back with
            // the `objects-*` signature the trampoline is written for.
            let id = unsafe {
                sys::g_signal_connect_data(
                    view,
                    sig.as_ptr(),
                    // The concrete trampoline has a fixed
                    // `(view, GSList*, user_data)` C signature; GLib stores it
                    // signature-erased as a `GCallback`, so transmute to that.
                    std::mem::transmute::<
                        unsafe extern "C" fn(*mut c_void, *mut sys::GSList, *mut c_void),
                        sys::GCallback,
                    >(view_changed_trampoline),
                    user_data,
                    no_op_closure_notify,
                    0, // G_CONNECT_DEFAULT
                )
            };
            debug_assert!(id != 0, "g_signal_connect_data returned 0 for {sig:?}");
            if id != 0 {
                handler_ids.push(id);
            }
        }

        // Begin notifications. `view-start` also replays the current contents
        // via `objects-added`, so the first refresh fires promptly without an
        // extra manual poll.
        let mut start_err: *mut sys::GError = ptr::null_mut();
        // SAFETY: `view` is live and owned here; `start_err` is a live local
        // initialised to null, which is what the `GError**` out-param wants.
        unsafe { sys::e_cal_client_view_start(view, &mut start_err) }
        if let Some(e) = take_error(start_err) {
            // Couldn't start — disconnect every handler *before* dropping the
            // box they point at, then release the view and surface the error
            // rather than returning a dead view. Same ordering as Drop, and
            // for the same reason.
            for id in handler_ids {
                // SAFETY: `id` is non-zero and came from a
                // `g_signal_connect_data` on this same live `view`, and is
                // disconnected exactly once (this path returns immediately
                // after, so Drop never runs for these ids).
                unsafe { sys::g_signal_handler_disconnect(view, id) }
            }
            // SAFETY: `view` is the ref `get_view_sync` transferred to us and
            // is released exactly once here.
            unsafe { sys::g_object_unref(view) }
            drop(boxed);
            return Err(e);
        }

        Ok(CalClientView {
            raw: view,
            handler_ids,
            _callback: boxed,
        })
    }
}

impl Drop for CalClient {
    fn drop(&mut self) {
        // SAFETY: P1 — the single ref `e_cal_client_connect_sync` transferred
        // to this value, released exactly once and never used after.
        unsafe { sys::g_object_unref(self.raw) }
    }
}

// ── ECalClientView ───────────────────────────────────────────────────────────

/// A live push subscription to a [`CalClient`]'s objects (see
/// [`CalClient::watch`]). Holds the EDS view plus the boxed Rust callback the
/// signal handlers fire into. Notifications flow only while this is alive **and**
/// the owning thread keeps pumping the [`MainContext`] the view was created
/// under; dropping it stops the view, **disconnects each handler**, releases
/// EDS's proxy, and only then frees the callback (in that order — see `Drop`).
pub struct CalClientView {
    raw: *mut sys::ECalClientView,
    /// The `objects-{added,modified,removed}` handler ids, each disconnected
    /// in [`Drop`] **before** the view is unref'd and before `_callback` is
    /// freed (#1179). Only non-zero ids (successful connects) are in here.
    handler_ids: Vec<sys::GULong>,
    // Kept alive (and dropped last, after the view is torn down) so the raw
    // user_data pointer the handlers hold stays valid for their whole life.
    _callback: Box<Box<dyn Fn()>>,
}

impl Drop for CalClientView {
    fn drop(&mut self) {
        // Teardown order is the whole safety argument for the raw `user_data`
        // the three handlers carry, and it is exactly this (#1179):
        //
        //   1. stop the view, so EDS quits emitting;
        //   2. **disconnect every handler**, so none of them can be invoked
        //      again by anyone — this is the step that makes freeing
        //      `_callback` sound. `g_object_unref` alone would only achieve
        //      it if our ref were the last one, which nothing here can prove:
        //      libecal (or an emission in flight) may hold another, and then
        //      the handlers outlive the box they point at;
        //   3. release our ref on the view;
        //   4. `_callback` drops, freeing the `Box<dyn Fn()>` — after (2),
        //      unreachable by construction rather than by refcount luck.
        //
        // Steps 1-3 run on the view's owning thread (the only place a
        // `CalClientView` lives), so no trampoline can be mid-flight either.
        let mut err: *mut sys::GError = ptr::null_mut();
        // SAFETY: `self.raw` is the live view this value owns (non-null since
        // `watch` rejected a null one), and `err` is a live local initialised
        // to null for the `GError**` out-param.
        unsafe { sys::e_cal_client_view_stop(self.raw, &mut err) }
        if !err.is_null() {
            // SAFETY: non-null here, and set by the call above, so it is a
            // `GError` we own; freed exactly once and never read after.
            unsafe { sys::g_error_free(err) }
        }
        for &id in &self.handler_ids {
            // SAFETY: every id in this vector is a non-zero id
            // `g_signal_connect_data` returned for this same `self.raw`, and
            // Drop runs once, so each is disconnected exactly once.
            unsafe { sys::g_signal_handler_disconnect(self.raw, id) }
        }
        // SAFETY: the single ref `e_cal_client_get_view_sync` transferred to
        // this value, released exactly once (Drop runs once) and never used
        // after.
        unsafe { sys::g_object_unref(self.raw) }
    }
}

/// The `objects-{added,modified,removed}` C handler. All three share this one
/// trampoline: the *kind* of change doesn't matter to us (the service re-reads
/// the whole list either way), so we coalesce to a bare ping. `user_data` is
/// the inner `Box<dyn Fn()>` from [`CalClient::watch`].
///
/// # Safety
///
/// GLib calls this with `user_data` equal to the pointer we passed to
/// `g_signal_connect_data` — a `*const Box<dyn Fn()>` owned by the
/// [`CalClientView`]. It is live at every reachable call: the three handlers
/// that can reach this function are **disconnected** in `CalClientView::drop`
/// before that box is freed (#1179), so an invocation after the free is
/// unreachable by construction rather than by the view's refcount happening
/// to be one. `_view`/`_objects` are borrowed and not touched.
unsafe extern "C" fn view_changed_trampoline(
    _view: *mut c_void,
    _objects: *mut sys::GSList,
    user_data: *mut c_void,
) {
    if user_data.is_null() {
        return;
    }
    // SAFETY: `user_data` is non-null (checked) and is the pointer `watch`
    // passed to `g_signal_connect_data` — the address of the inner
    // `Box<dyn Fn()>` its `CalClientView` owns. That box outlives every
    // handler that can reach this function (Drop disconnects them first), and
    // the reference taken here lives only for this call, so it cannot alias a
    // `&mut` (nothing ever takes one) and cannot dangle.
    let cb = unsafe { &*user_data.cast::<Box<dyn Fn()>>() };
    cb();
}

/// No-op `GClosureNotify`: the boxed callback's lifetime is owned by the
/// [`CalClientView`], not by GLib's closures, so there is nothing to free when
/// a closure finalises.
///
/// # Safety
///
/// Trivially safe — the body never dereferences either argument.
unsafe extern "C" fn no_op_closure_notify(_data: *mut c_void, _closure: *mut sys::GClosure) {}

// ── MainContext ──────────────────────────────────────────────────────────────

/// A private GLib [`GMainContext`], pushed thread-default on construction so
/// EDS views created on this thread deliver their signals here (not to the
/// global default context, which trollshell's GTK thread owns). Iterate it with
/// [`MainContext::iterate`] to dispatch pending view signals.
///
/// **Thread-bound:** create and iterate it on one thread only (it pushes itself
/// thread-default for *that* thread). [`MainContext::waker`] hands out a
/// `Send`-able handle for waking the iteration from elsewhere.
pub struct MainContext {
    raw: *mut sys::GMainContext,
}

impl MainContext {
    /// Create a fresh private context and push it thread-default for the
    /// calling thread. Returns `None` if GLib couldn't allocate one.
    #[must_use]
    pub fn new() -> Option<Self> {
        // SAFETY: nullary GLib allocator; the ref it returns is owned by the
        // `MainContext` this builds and released exactly once in its Drop.
        let raw = unsafe { sys::g_main_context_new() };
        if raw.is_null() {
            return None;
        }
        // SAFETY: `raw` is the non-null context just created. The push is
        // paired with exactly one pop in Drop, on this same thread — the type
        // is `!Send`, so the pop cannot happen on another thread and unbalance
        // GLib's per-thread stack.
        unsafe { sys::g_main_context_push_thread_default(raw) }
        Some(Self { raw })
    }

    /// Run one iteration. With `block` true, sleeps until a source is ready
    /// (a view signal arrived) or a [`Waker::wake`] fires — fully event-driven,
    /// no busy spin. Returns true if a source was dispatched.
    pub fn iterate(&self, block: bool) -> bool {
        // SAFETY: P1/P3 — `self.raw` is the live context this value owns, and
        // `MainContext` is `!Send`, so this iterates on the thread that pushed
        // it thread-default (GLib's requirement for iteration).
        unsafe { sys::g_main_context_iteration(self.raw, sys::GBoolean::from(block)) != 0 }
    }

    /// A `Send`-able handle that can [`Waker::wake`] this context's blocking
    /// iteration from another thread (the only cross-thread `GMainContext`
    /// operation GLib sanctions). Holds its own ref, so it stays valid even if
    /// the `MainContext` is dropped first.
    #[must_use]
    pub fn waker(&self) -> Waker {
        // SAFETY: P1, and `g_main_context_ref` is one of GLib's thread-safe
        // `GMainContext` calls. The extra ref it returns is owned by the
        // `Waker` and released exactly once in its Drop, so the context
        // outlives the waker even if this `MainContext` is dropped first.
        let raw = unsafe { sys::g_main_context_ref(self.raw) };
        Waker { raw }
    }
}

impl Drop for MainContext {
    fn drop(&mut self) {
        // SAFETY: P1/P3 — pops the push from `new` on the same thread (the
        // type is `!Send`), exactly once, so GLib's thread-default stack stays
        // balanced; `self.raw` is still live because we hold the ref released
        // on the next line.
        unsafe { sys::g_main_context_pop_thread_default(self.raw) }
        // SAFETY: that ref, released exactly once and never used after. Any
        // `Waker` holds its own ref, so the context survives them.
        unsafe { sys::g_main_context_unref(self.raw) }
    }
}

/// A `Send + Sync` handle for waking a [`MainContext`]'s blocking iteration
/// from another thread. Every wrapped call (`wakeup`/`ref`/`unref`) is on
/// GLib's documented thread-safe `GMainContext` surface, so sharing this across
/// threads is sound.
pub struct Waker {
    raw: *mut sys::GMainContext,
}

// SAFETY: `g_main_context_wakeup`/`_ref`/`_unref` are explicitly thread-safe in
// GLib; this handle only ever calls those. It never touches the
// thread-default-stack or iterates, so it carries no thread affinity.
unsafe impl Send for Waker {}
// SAFETY: as above — all operations are thread-safe and take `&self`.
unsafe impl Sync for Waker {}

impl Waker {
    /// Break a [`MainContext::iterate(true)`] out of its block so the owning
    /// thread loops promptly (e.g. to pick up a newly-queued command).
    pub fn wake(&self) {
        // SAFETY: `self.raw` is the context this `Waker` holds its own ref to,
        // so it is live for `&self`; `g_main_context_wakeup` is explicitly
        // thread-safe, which is what makes the `Send`/`Sync` impls above sound.
        unsafe { sys::g_main_context_wakeup(self.raw) }
    }
}

impl Drop for Waker {
    fn drop(&mut self) {
        // SAFETY: the ref `MainContext::waker` took for this value, released
        // exactly once; `_unref` is thread-safe, so dropping on any thread is
        // fine.
        unsafe { sys::g_main_context_unref(self.raw) }
    }
}

// ── ICalComponent ────────────────────────────────────────────────────────────

/// RAII handle to a parsed `ICalComponent`. We expose this only as an
/// implementation detail of [`CalClient::create_from_ical`] etc; consumers
/// of this crate pass iCal strings end-to-end.
struct Component {
    raw: *mut sys::ICalComponent,
}

impl Drop for Component {
    fn drop(&mut self) {
        // libical-glib's GObject-style components are released via
        // `g_object_unref`. The legacy `i_cal_component_free` exists for
        // the C struct, not the GObject wrapper.
        //
        // SAFETY: P1 — `self.raw` is the one component ref this value adopted
        // (from the parser or from a `get_first_component`), released exactly
        // once and never used after.
        unsafe { sys::g_object_unref(self.raw) }
    }
}

/// Parse an iCalendar string and return a VTODO/VEVENT component ready
/// to hand to libecal. The parser yields the outer VCALENDAR; we
/// unwrap one level so callers can pass either a bare VTODO/VEVENT or
/// the full VCALENDAR wrapper and the result is the same.
///
/// If the parsed root is already a VTODO/VEVENT (libical accepts both),
/// we hand that back directly. Otherwise we look for the first inner
/// VTODO, then VEVENT — that ordering matches the tasks-first bias of
/// this crate's primary use case.
fn parse_component(ical: &str) -> Result<Component> {
    let c = CString::new(ical).context("ical body contained an interior NUL")?;
    // SAFETY: `c` is a live `CString` (NUL-terminated, no interior NUL) that
    // outlives the call; libical copies what it needs and hands back a new
    // component ref, adopted by the `Component` below.
    let raw = unsafe { sys::i_cal_parser_parse_string(c.as_ptr()) };
    if raw.is_null() {
        bail!("libical: failed to parse iCalendar body");
    }
    let parsed = Component { raw };
    // `isa` returns a raw `c_int`; match it against the libical component
    // constants rather than transmuting into the 8-variant Rust enum
    // (libical may return any of ~28 kinds — an unlisted value read as a
    // `#[repr(C)]` enum would be UB).
    //
    // SAFETY: `parsed.raw` is that live component; `isa` only reads it, and
    // the `c_int` it returns is matched against constants, never transmuted.
    let kind = unsafe { sys::i_cal_component_isa(parsed.raw) };
    if matches!(
        kind,
        sys::I_CAL_VTODO_COMPONENT | sys::I_CAL_VEVENT_COMPONENT
    ) {
        return Ok(parsed);
    }
    // Try VTODO first, then VEVENT.
    for k in [
        sys::ICalComponentKind::Vtodo,
        sys::ICalComponentKind::Vevent,
    ] {
        // SAFETY: `parsed.raw` is the live parsed component this scope owns,
        // and `k` is a real `ICalComponentKind` variant (a Rust enum with the
        // libical discriminants, passed out — never received — so no invalid
        // value can be constructed). Returns a new ref or null.
        let inner = unsafe { sys::i_cal_component_get_first_component(parsed.raw, k) };
        if !inner.is_null() {
            // `get_first_component` returns a NEW ref (libical-glib
            // GObject convention for "first" accessors). Wrap it in a
            // Component so it'll be unref'd. The outer VCALENDAR ref
            // drops with `parsed`.
            return Ok(Component { raw: inner });
        }
    }
    bail!("libical: parsed body had no VTODO or VEVENT child");
}

/// Like [`parse_component`] but VEVENT-only — used by [`expand_ical_for_test`]
/// to materialise a recurring event from a string for hermetic expansion.
fn parse_vevent(ical: &str) -> Result<Component> {
    let c = CString::new(ical).context("ical body contained an interior NUL")?;
    // SAFETY: as `parse_component` — a live `CString` outliving the call, and
    // a new component ref (or null) adopted below.
    let raw = unsafe { sys::i_cal_parser_parse_string(c.as_ptr()) };
    if raw.is_null() {
        bail!("libical: failed to parse iCalendar body");
    }
    let parsed = Component { raw };
    // SAFETY: `parsed.raw` is that live component; `isa` only reads it and its
    // `c_int` result is compared, never transmuted.
    if unsafe { sys::i_cal_component_isa(parsed.raw) } == sys::I_CAL_VEVENT_COMPONENT {
        return Ok(parsed);
    }
    // SAFETY: as the loop in `parse_component` — live component, a real
    // `ICalComponentKind` variant, new ref or null.
    let inner = unsafe {
        sys::i_cal_component_get_first_component(parsed.raw, sys::ICalComponentKind::Vevent)
    };
    if !inner.is_null() {
        return Ok(Component { raw: inner });
    }
    bail!("libical: parsed body had no VEVENT child");
}

// ── GError helpers ───────────────────────────────────────────────────────────

/// Consume a `GError*` (which may be null) into an `anyhow::Error`,
/// freeing the GError via `g_error_free`. Returns `None` when the input
/// pointer was null.
fn take_error(err: *mut sys::GError) -> Option<anyhow::Error> {
    if err.is_null() {
        return None;
    }
    // SAFETY: `err` is non-null (checked) and, by every caller's construction,
    // a `GError` GLib allocated and transferred to us — `sys::GError` mirrors
    // its layout `#[repr(C)]`. `message` is either null or a NUL-terminated
    // string owned by the error, copied here before the free below.
    let msg = unsafe {
        let ptr = (*err).message;
        if ptr.is_null() {
            String::from("(no GError message)")
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    };
    // SAFETY: as the `message` read above — a plain integer field of the same
    // live, owned error, read before it is freed.
    let domain = unsafe { (*err).domain };
    // SAFETY: as the `domain` read above.
    let code = unsafe { (*err).code };
    // SAFETY: the error we own, freed exactly once (this function consumes the
    // pointer and every caller drops it afterwards) and never read after.
    unsafe { sys::g_error_free(err) }
    Some(anyhow!("EDS error [domain={domain} code={code}]: {msg}"))
}

/// Convert a borrowed `const char*` returned by libecal/libedataserver into
/// an owned `String`. Returns `None` for a null pointer.
unsafe fn borrowed_cstr(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` is non-null (checked) and, by this function's contract, a
    // NUL-terminated string owned by the GObject it came from and live for
    // that borrow; the copy is taken here and the original is never freed.
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::sys;

    /// The integer constants we match `i_cal_component_isa` against must
    /// stay numerically identical to the corresponding `ICalComponentKind`
    /// enum discriminants — they describe the same libical values, just in
    /// the int form that's sound to receive from FFI.
    #[test]
    fn component_kind_constants_match_enum() {
        assert_eq!(
            sys::I_CAL_VEVENT_COMPONENT,
            sys::ICalComponentKind::Vevent as i32
        );
        assert_eq!(
            sys::I_CAL_VTODO_COMPONENT,
            sys::ICalComponentKind::Vtodo as i32
        );
    }

    /// `GError.domain` is a `GQuark` (`guint32`); the struct must mirror
    /// that so the domain comparison in `get_object_as_string` is valid.
    #[test]
    fn gerror_domain_is_u32_quark() {
        let err = sys::GError {
            domain: u32::MAX,
            code: 0,
            message: std::ptr::null_mut(),
        };
        // Round-trips through a u32 without truncation.
        assert_eq!(err.domain, u32::MAX);
    }

    // ── Zoned (`TZID`) timezone (issue #522) ──────────────────────────────
    //
    // A DATE-TIME with a resolved `TZID` (or a `…Z` UTC value) is *absolute*:
    // its instant is fixed regardless of the viewer's zone. It must be read in
    // its own zone, not the UTC singleton, or a `TZID=Europe/Berlin` 12:30
    // event lands at 12:30 UTC and displays as 14:30 CEST — the +2h double
    // shift. These assert the exact absolute instant, so they are fully
    // deterministic regardless of the test host's `TZ`.
    //
    // Every fixture carries its `VTIMEZONE` **inline** in the VCALENDAR (the
    // shape a synced CalDAV/Google calendar delivers). libical computes the
    // offset from the inline STANDARD/DAYLIGHT observances alone — it does NOT
    // touch the system/`tzdata` zoneinfo — so the zone resolves even in a
    // hermetic sandbox with no zoneinfo installed (the crane/nix test bucket).
    // A fixture relying on libical's *builtin* `Europe/Berlin` lookup would
    // instead be read as floating there (no zoneinfo to resolve), falling back
    // to `Local` — which is UTC in a sandbox with no `/etc/localtime` — and so
    // would spuriously reproduce the very +2h it means to guard against.

    /// The whole of 2026 as a UTC-seconds window — brackets any 2026 event in
    /// any viewer zone.
    const Y2026_START: i64 = 1_767_225_600; // 2026-01-01T00:00:00Z
    const Y2026_END: i64 = 1_798_761_600; // 2027-01-01T00:00:00Z

    /// An inline `Europe/Berlin` `VTIMEZONE` (CET/CEST DST rules). Self-
    /// contained: libical derives the UTC offset from these observances without
    /// consulting the host zoneinfo database.
    const BERLIN_VTIMEZONE: &str = "BEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\n\
         BEGIN:DAYLIGHT\r\nTZOFFSETFROM:+0100\r\nTZOFFSETTO:+0200\r\nTZNAME:CEST\r\n\
         DTSTART:19700329T020000\r\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU\r\nEND:DAYLIGHT\r\n\
         BEGIN:STANDARD\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nTZNAME:CET\r\n\
         DTSTART:19701025T030000\r\nRRULE:FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU\r\nEND:STANDARD\r\n\
         END:VTIMEZONE\r\n";

    /// 2026-07-24 12:30 in `Europe/Berlin` (CEST, UTC+2) is 10:30 UTC.
    fn expected_1030_utc() -> i64 {
        use chrono::{TimeZone as _, Utc};
        Utc.with_ymd_and_hms(2026, 7, 24, 10, 30, 0)
            .unwrap()
            .timestamp()
    }

    /// Wrap `vevent_body` in a VCALENDAR that carries the inline Berlin
    /// `VTIMEZONE`, so a `TZID=Europe/Berlin` inside it resolves self-contained.
    fn berlin_calendar(vevent_body: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//hytte-ecal-test//\r\n{BERLIN_VTIMEZONE}BEGIN:VEVENT\r\n{vevent_body}END:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    /// A `DTSTART;TZID=Europe/Berlin:…123000` (12:30 CEST) must expand to the
    /// absolute instant 10:30 UTC — not 12:30 UTC (the +2h double-shift, #522).
    /// This is the core regression guard.
    #[test]
    fn tzid_datetime_resolves_to_own_zone_instant() {
        use chrono::{TimeZone as _, Utc};

        let ical = berlin_calendar(
            "UID:tzid-1\r\nDTSTAMP:20260724T090000Z\r\n\
             DTSTART;TZID=Europe/Berlin:20260724T123000\r\n\
             DTEND;TZID=Europe/Berlin:20260724T133000\r\nSUMMARY:Lunch\r\n",
        );
        let inst = super::expand_ical_for_test(&ical, Y2026_START, Y2026_END).unwrap();
        assert_eq!(inst.len(), 1);
        assert!(!inst[0].all_day);
        assert_eq!(
            inst[0].start_unix,
            expected_1030_utc(),
            "TZID=Europe/Berlin 12:30 must resolve to 10:30 UTC (its own zone), not 12:30 UTC",
        );
        // Guard the exact +2h signature the bug produced (wall-clock read as UTC).
        let bug_utc = Utc
            .with_ymd_and_hms(2026, 7, 24, 12, 30, 0)
            .unwrap()
            .timestamp();
        assert_ne!(
            inst[0].start_unix, bug_utc,
            "must not read the Berlin wall-clock as UTC (the #522 +2h shift)",
        );
    }

    /// A `…Z` UTC value is unchanged by the fix: 10:30 UTC in, 10:30 UTC out.
    /// (Control for the three-input table: UTC / TZID / floating.)
    #[test]
    fn utc_datetime_instant_unchanged() {
        let ical = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:utc-1\r\n\
                     DTSTAMP:20260724T090000Z\r\n\
                     DTSTART:20260724T103000Z\r\nDTEND:20260724T113000Z\r\n\
                     SUMMARY:Sync\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let inst = super::expand_ical_for_test(ical, Y2026_START, Y2026_END).unwrap();
        assert_eq!(inst.len(), 1);
        assert_eq!(inst[0].start_unix, expected_1030_utc());
    }

    /// The recurrence iterator preserves the resolved zone: every occurrence of
    /// a `TZID`'d daily series is absolute in its own zone. Guards the iterator
    /// path (`i_cal_recur_iterator_*`), not just the single-occurrence path.
    #[test]
    fn tzid_recurring_occurrences_resolve_to_own_zone() {
        let ical = berlin_calendar(
            "UID:tzid-rec\r\nDTSTAMP:20260724T090000Z\r\n\
             DTSTART;TZID=Europe/Berlin:20260724T123000\r\n\
             DTEND;TZID=Europe/Berlin:20260724T133000\r\n\
             RRULE:FREQ=DAILY;COUNT=3\r\nSUMMARY:Standup\r\n",
        );
        let inst = super::expand_ical_for_test(&ical, Y2026_START, Y2026_END).unwrap();
        assert_eq!(inst.len(), 3);
        // First occurrence at 10:30 UTC; each subsequent one a wall-clock day
        // later (both days are CEST, so +86400s — no DST transition here).
        assert_eq!(inst[0].start_unix, expected_1030_utc());
        assert_eq!(inst[1].start_unix, expected_1030_utc() + 86_400);
        assert_eq!(inst[2].start_unix, expected_1030_utc() + 2 * 86_400);
    }

    // ── Floating-time timezone (issue #388) ───────────────────────────────
    //
    // A zone-less (floating) DTSTART must be read in the *local* zone, not
    // UTC, or every timed event shows shifted by the viewer's offset.

    /// Pure-logic guard: a floating wall clock resolves to the same wall clock
    /// when rendered back in `Local` — i.e. the fields were interpreted as
    /// local, never as UTC. Deterministic in any real system zone (11:30 is
    /// not a DST-gap time).
    #[test]
    fn wallclock_resolves_in_local_zone_not_utc() {
        use chrono::{Datelike as _, Local, TimeZone as _, Timelike as _};

        let wall = super::WallClock {
            year: 2026,
            month: 7,
            day: 22,
            hour: 11,
            minute: 30,
            second: 0,
        };
        let unix = wall.to_local_unix().expect("valid local instant");
        let back = Local.timestamp_opt(unix, 0).single().unwrap();
        assert_eq!(
            (back.year(), back.month(), back.day()),
            (2026, 7, 22),
            "the local calendar date must be preserved",
        );
        assert_eq!(
            (back.hour(), back.minute(), back.second()),
            (11, 30, 0),
            "11:30 floating must render back as 11:30 local, not offset-shifted",
        );
    }

    /// End-to-end through the FFI: a floating `DTSTART` (no `TZID`, no `Z`)
    /// expands to an instant that renders back to the same wall clock in the
    /// local zone. Deterministic regardless of the test host's `TZ`.
    #[test]
    fn floating_datetime_expands_in_local_zone() {
        use chrono::{Datelike as _, Local, TimeZone as _, Timelike as _};

        let ical = "BEGIN:VEVENT\r\nUID:float\r\nDTSTAMP:20260722T090000Z\r\n\
                     DTSTART:20260722T113000\r\nDTEND:20260722T120000\r\n\
                     SUMMARY:Lunch\r\nEND:VEVENT\r\n";
        // Window = all of 2026 (UTC seconds); brackets the event in any zone.
        let inst = super::expand_ical_for_test(ical, 1_767_225_600, 1_798_761_600).unwrap();
        assert_eq!(inst.len(), 1);
        assert!(!inst[0].all_day);
        let start = Local.timestamp_opt(inst[0].start_unix, 0).single().unwrap();
        assert_eq!((start.year(), start.month(), start.day()), (2026, 7, 22));
        assert_eq!(
            (start.hour(), start.minute()),
            (11, 30),
            "floating 11:30 must land at 11:30 local (issue #388), not 11:30 UTC",
        );
    }

    // ── Recurrence expansion (issue #29) ──────────────────────────────────
    //
    // These drive the pure-libical path in [`expand_ical_for_test`] — no EDS
    // backend, so they're hermetic and run under the default `cargo test
    // -p hytte-ecal` (the crate links libical-glib).

    // 2026-06-01T00:00:00Z .. 2026-07-01T00:00:00Z (the whole of June 2026).
    const JUN_START: i64 = 1_780_272_000;
    const JUL_START: i64 = 1_782_864_000;
    // 2026-06-01T09:00:00Z — the anchor used by the fixtures below.
    const ANCHOR_0900: i64 = 1_780_304_400;

    #[test]
    fn rrule_daily_count_5_yields_5_instances() {
        let ical = "BEGIN:VEVENT\r\nUID:d5\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART:20260601T090000Z\r\nDTEND:20260601T093000Z\r\n\
                     SUMMARY:Standup\r\nRRULE:FREQ=DAILY;COUNT=5\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();
        assert_eq!(inst.len(), 5, "FREQ=DAILY;COUNT=5 must expand to 5");
        // Consecutive daily starts, 30-minute duration each.
        for (i, e) in inst.iter().enumerate() {
            let day = i64::try_from(i).unwrap();
            assert_eq!(e.start_unix, ANCHOR_0900 + day * 86_400);
            assert_eq!(e.end_unix - e.start_unix, 1_800);
            assert!(!e.all_day);
        }
    }

    #[test]
    fn rrule_unbounded_daily_is_window_capped() {
        // No COUNT/UNTIL ⇒ infinite series; the 30-day-ish window must bound
        // it (here: June has 30 days, so exactly 30 occurrences from Jun 1).
        let ical = "BEGIN:VEVENT\r\nUID:dinf\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART:20260601T090000Z\r\nDTEND:20260601T093000Z\r\n\
                     SUMMARY:Forever\r\nRRULE:FREQ=DAILY\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();
        assert_eq!(inst.len(), 30, "unbounded daily over June ⇒ 30 in-window");
    }

    #[test]
    fn rrule_weekly_count_3() {
        let ical = "BEGIN:VEVENT\r\nUID:w3\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART:20260601T090000Z\r\nDTEND:20260601T100000Z\r\n\
                     SUMMARY:Weekly\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();
        assert_eq!(inst.len(), 3);
        assert_eq!(inst[1].start_unix - inst[0].start_unix, 7 * 86_400);
    }

    #[test]
    fn non_recurring_event_yields_single_instance() {
        let ical = "BEGIN:VEVENT\r\nUID:one\r\nDTSTAMP:20260615T120000Z\r\n\
                     DTSTART:20260615T120000Z\r\nDTEND:20260615T130000Z\r\n\
                     SUMMARY:Once\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();
        assert_eq!(inst.len(), 1);
        assert_eq!(inst[0].end_unix - inst[0].start_unix, 3_600);
    }

    #[test]
    fn occurrences_outside_window_are_excluded() {
        // COUNT=5 daily from Jun 1, but query only Jun 3 onward ⇒ 3 left.
        let ical = "BEGIN:VEVENT\r\nUID:cut\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART:20260601T090000Z\r\nDTEND:20260601T093000Z\r\n\
                     SUMMARY:Cut\r\nRRULE:FREQ=DAILY;COUNT=5\r\nEND:VEVENT\r\n";
        let jun3 = ANCHOR_0900 + 2 * 86_400; // 2026-06-03T09:00:00Z
        let inst = super::expand_ical_for_test(ical, jun3, JUL_START).unwrap();
        assert_eq!(inst.len(), 3, "Jun 3,4,5 fall in the trimmed window");
    }

    #[test]
    fn all_day_recurring_sets_all_day_flag() {
        let ical = "BEGIN:VEVENT\r\nUID:ad\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART;VALUE=DATE:20260601\r\n\
                     SUMMARY:Holiday\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();
        assert_eq!(inst.len(), 3);
        assert!(inst.iter().all(|e| e.all_day));
    }

    // ── EXDATE / RDATE (issue #29 follow-up) ──────────────────────────────
    //
    // The recurrence-set modifiers layered on top of RRULE expansion: EXDATE
    // cancels a single occurrence (the common "skipped one standup" case) and
    // RDATE bolts an extra one-off onto the series. Same hermetic path as the
    // RRULE tests above.

    #[test]
    fn exdate_excludes_one_occurrence() {
        // FREQ=DAILY;COUNT=5 from Jun 1 09:00, with Jun 3 cancelled.
        let ical = "BEGIN:VEVENT\r\nUID:ex1\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART:20260601T090000Z\r\nDTEND:20260601T093000Z\r\n\
                     SUMMARY:Standup\r\nRRULE:FREQ=DAILY;COUNT=5\r\n\
                     EXDATE:20260603T090000Z\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();
        assert_eq!(
            inst.len(),
            4,
            "the excluded Jun 3 occurrence must be absent"
        );
        let jun3 = ANCHOR_0900 + 2 * 86_400;
        assert!(
            inst.iter().all(|e| e.start_unix != jun3),
            "no instance may start at the EXDATE'd Jun 3 09:00",
        );
        // The other four are intact and contiguous (Jun 1,2,4,5).
        assert_eq!(inst[0].start_unix, ANCHOR_0900);
        assert_eq!(inst[1].start_unix, ANCHOR_0900 + 86_400);
        assert_eq!(inst[2].start_unix, ANCHOR_0900 + 3 * 86_400);
        assert_eq!(inst[3].start_unix, ANCHOR_0900 + 4 * 86_400);
    }

    #[test]
    fn multiple_exdate_properties_all_apply() {
        // Two separate EXDATE properties (Jun 2 and Jun 4) each cancel one.
        let ical = "BEGIN:VEVENT\r\nUID:ex2\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART:20260601T090000Z\r\nDTEND:20260601T093000Z\r\n\
                     SUMMARY:Standup\r\nRRULE:FREQ=DAILY;COUNT=5\r\n\
                     EXDATE:20260602T090000Z\r\nEXDATE:20260604T090000Z\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();
        assert_eq!(inst.len(), 3, "two EXDATEs ⇒ 5 − 2 = 3 occurrences");
        let starts: Vec<i64> = inst.iter().map(|e| e.start_unix).collect();
        assert_eq!(
            starts,
            vec![
                ANCHOR_0900,              // Jun 1
                ANCHOR_0900 + 2 * 86_400, // Jun 3
                ANCHOR_0900 + 4 * 86_400, // Jun 5
            ],
        );
    }

    #[test]
    fn exdate_listing_multiple_datetimes_in_one_property() {
        // A single EXDATE property carrying a comma-separated list — libical
        // splits it into multiple properties internally, which our first/next
        // walk must pick up in full.
        let ical = "BEGIN:VEVENT\r\nUID:ex3\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART:20260601T090000Z\r\nDTEND:20260601T093000Z\r\n\
                     SUMMARY:Standup\r\nRRULE:FREQ=DAILY;COUNT=5\r\n\
                     EXDATE:20260602T090000Z,20260603T090000Z\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();
        assert_eq!(inst.len(), 3, "comma-listed EXDATE excludes both Jun 2 & 3");
    }

    #[test]
    fn exdate_not_matching_any_occurrence_is_noop() {
        // EXDATE points at a time no occurrence falls on (08:00, not 09:00) ⇒
        // nothing is excluded.
        let ical = "BEGIN:VEVENT\r\nUID:exn\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART:20260601T090000Z\r\nDTEND:20260601T093000Z\r\n\
                     SUMMARY:Standup\r\nRRULE:FREQ=DAILY;COUNT=5\r\n\
                     EXDATE:20260603T080000Z\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();
        assert_eq!(inst.len(), 5, "a non-matching EXDATE is a no-op");
    }

    #[test]
    fn exdate_all_day_date_value_excludes_all_day_occurrence() {
        // All-day series with an all-day (VALUE=DATE) EXDATE: the DATE-form
        // exclusion must match the DATE-form occurrence (both normalise to
        // UTC midnight).
        let ical = "BEGIN:VEVENT\r\nUID:exad\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART;VALUE=DATE:20260601\r\n\
                     SUMMARY:Holiday\r\nRRULE:FREQ=DAILY;COUNT=3\r\n\
                     EXDATE;VALUE=DATE:20260602\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();
        assert_eq!(inst.len(), 2, "the all-day Jun 2 occurrence is excluded");
        assert!(inst.iter().all(|e| e.all_day));
    }

    #[test]
    fn rdate_adds_one_off_occurrence() {
        // FREQ=DAILY;COUNT=3 (Jun 1,2,3) plus an RDATE on Jun 10 ⇒ 4 total,
        // with the extra outside the RRULE span.
        let ical = "BEGIN:VEVENT\r\nUID:rd1\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART:20260601T090000Z\r\nDTEND:20260601T093000Z\r\n\
                     SUMMARY:Standup\r\nRRULE:FREQ=DAILY;COUNT=3\r\n\
                     RDATE:20260610T090000Z\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();
        assert_eq!(inst.len(), 4, "3 RRULE occurrences + 1 RDATE");
        let jun10 = ANCHOR_0900 + 9 * 86_400;
        assert!(
            inst.iter().any(|e| e.start_unix == jun10),
            "the RDATE-added Jun 10 occurrence must be present",
        );
        // Duration carries over from DTEND − DTSTART (30 min) for the RDATE.
        let added = inst.iter().find(|e| e.start_unix == jun10).unwrap();
        assert_eq!(added.end_unix - added.start_unix, 1_800);
    }

    #[test]
    fn rdate_duplicate_of_rrule_occurrence_is_deduped() {
        // An RDATE coinciding with an existing RRULE occurrence (Jun 2) must
        // not produce a second instance.
        let ical = "BEGIN:VEVENT\r\nUID:rd2\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART:20260601T090000Z\r\nDTEND:20260601T093000Z\r\n\
                     SUMMARY:Standup\r\nRRULE:FREQ=DAILY;COUNT=3\r\n\
                     RDATE:20260602T090000Z\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();
        assert_eq!(
            inst.len(),
            3,
            "RDATE duplicating an RRULE occurrence is deduped"
        );
    }

    #[test]
    fn exdate_beats_rdate_on_same_instant() {
        // RFC 5545: if EXDATE and RDATE name the same instant, EXDATE wins.
        let ical = "BEGIN:VEVENT\r\nUID:rdex\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART:20260601T090000Z\r\nDTEND:20260601T093000Z\r\n\
                     SUMMARY:Standup\r\nRRULE:FREQ=DAILY;COUNT=3\r\n\
                     RDATE:20260610T090000Z\r\nEXDATE:20260610T090000Z\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();
        assert_eq!(inst.len(), 3, "EXDATE cancels the same-instant RDATE");
        let jun10 = ANCHOR_0900 + 9 * 86_400;
        assert!(inst.iter().all(|e| e.start_unix != jun10));
    }

    #[test]
    fn rdate_outside_window_is_excluded() {
        // An RDATE in July, queried over June only ⇒ not emitted.
        let ical = "BEGIN:VEVENT\r\nUID:rdw\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART:20260601T090000Z\r\nDTEND:20260601T093000Z\r\n\
                     SUMMARY:Standup\r\nRRULE:FREQ=DAILY;COUNT=3\r\n\
                     RDATE:20260710T090000Z\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();
        assert_eq!(inst.len(), 3, "out-of-window RDATE is dropped");
    }

    #[test]
    fn rdate_without_rrule_adds_to_dtstart() {
        // No RRULE: DTSTART is the base occurrence, RDATE adds another.
        let ical = "BEGIN:VEVENT\r\nUID:rdo\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART:20260601T090000Z\r\nDTEND:20260601T093000Z\r\n\
                     SUMMARY:Pair\r\nRDATE:20260605T090000Z\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();
        assert_eq!(inst.len(), 2, "DTSTART + 1 RDATE, no RRULE");
        let mut starts: Vec<i64> = inst.iter().map(|e| e.start_unix).collect();
        starts.sort_unstable();
        assert_eq!(starts, vec![ANCHOR_0900, ANCHOR_0900 + 4 * 86_400]);
    }

    // ── MainContext + Waker (issue #33) ───────────────────────────────────
    //
    // The push-refresh path's loop machinery — a private GMainContext the
    // worker iterates, woken cross-thread. These are hermetic: pure GLib, no
    // EDS backend. The view + signal trampoline themselves need a live EDS
    // session, so those are exercised by the nixosTest (checks.eds-nixos-test).

    #[test]
    fn main_context_non_blocking_iteration_returns() {
        // A fresh context has no ready sources, so a non-blocking iteration
        // must return promptly (false = nothing dispatched) and not hang.
        let ctx = super::MainContext::new().expect("alloc GMainContext");
        assert!(!ctx.iterate(false), "empty context dispatched nothing");
    }

    #[test]
    fn waker_unblocks_a_blocking_iteration() {
        use std::time::{Duration, Instant};

        // Prove the event-driven contract: `iterate(true)` on a context with
        // no ready sources blocks until a waker fires from another thread —
        // exactly how `send_op` nudges the EDS worker out of its block.
        // The context is created and iterated on this one thread (so its
        // thread-default push/pop stay balanced here); only the waker — which
        // is `Send` by design — crosses to the helper thread.
        let ctx = super::MainContext::new().expect("alloc GMainContext");
        let waker = ctx.waker();

        // Wake after a short delay; until then `iterate(true)` must stay
        // blocked. The delay is the lower bound we assert the block lasted.
        let _firer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            waker.wake();
        });

        let start = Instant::now();
        ctx.iterate(true); // blocks until the wake above
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(100),
            "iteration returned in {elapsed:?} — it didn't actually block on the waker"
        );
    }

    // ── Expansion output fixture (#1179) ──────────────────────────────────
    //
    // Pinned *before* #1179 rewrote `expand_component`'s dedup and its
    // per-occurrence iCal cloning, so that rewrite is provably
    // output-preserving rather than merely believed to be. One rule
    // exercising RRULE + EXDATE + RDATE at once, with every field of every
    // occurrence asserted in order — including the iCal serialisation
    // libical hands back, which is what the pre-#1179 code cloned per
    // occurrence and the post-#1179 code shares.

    /// The fixture component: a 3-occurrence daily series with one cancelled
    /// occurrence (EXDATE, Jun 2) and one extra one-off (RDATE, Jun 10).
    const FIXTURE_DAILY: &str = "BEGIN:VEVENT\r\nUID:fixture-daily\r\n\
         DTSTAMP:20260601T090000Z\r\nDTSTART:20260601T090000Z\r\n\
         DTEND:20260601T093000Z\r\nSUMMARY:Standup\r\nLOCATION:Kitchen\r\n\
         RRULE:FREQ=DAILY;COUNT=3\r\nEXDATE:20260602T090000Z\r\n\
         RDATE:20260610T090000Z\r\nEND:VEVENT\r\n";

    /// What `i_cal_component_as_ical_string` hands back for [`FIXTURE_DAILY`]
    /// — the string every occurrence of the series carries. Recorded from the
    /// pre-#1179 tree; property order is libical's, not ours.
    const FIXTURE_DAILY_SERIALISED: &str = "BEGIN:VEVENT\r\nUID:fixture-daily\r\n\
         DTSTAMP:20260601T090000Z\r\nDTSTART:20260601T090000Z\r\n\
         DTEND:20260601T093000Z\r\nSUMMARY:Standup\r\nLOCATION:Kitchen\r\n\
         RRULE:FREQ=DAILY;COUNT=3\r\nEXDATE:20260602T090000Z\r\n\
         RDATE:20260610T090000Z\r\nEND:VEVENT\r\n";

    #[test]
    fn small_daily_rule_expansion_is_pinned_field_by_field() {
        let inst = super::expand_ical_for_test(FIXTURE_DAILY, JUN_START, JUL_START).unwrap();

        // Jun 1 (RRULE), Jun 3 (RRULE — Jun 2 is EXDATE'd), Jun 10 (RDATE),
        // in that emission order: RRULE occurrences first, RDATEs appended.
        let expected: [(i64, i64, bool); 3] = [
            (ANCHOR_0900, ANCHOR_0900 + 1_800, false),
            (
                ANCHOR_0900 + 2 * 86_400,
                ANCHOR_0900 + 2 * 86_400 + 1_800,
                false,
            ),
            (
                ANCHOR_0900 + 9 * 86_400,
                ANCHOR_0900 + 9 * 86_400 + 1_800,
                false,
            ),
        ];
        assert_eq!(inst.len(), expected.len(), "occurrence count");
        for (i, (e, (start, end, all_day))) in inst.iter().zip(expected).enumerate() {
            assert_eq!(e.start_unix, start, "occurrence {i} start");
            assert_eq!(e.end_unix, end, "occurrence {i} end");
            assert_eq!(e.all_day, all_day, "occurrence {i} all_day");
            assert_eq!(
                &*e.ical, FIXTURE_DAILY_SERIALISED,
                "occurrence {i} carries the component's serialisation verbatim",
            );
        }
    }

    /// Every occurrence of one series must share **one** `Arc<str>`, not carry
    /// its own copy of the same bytes. Equality alone cannot see the
    /// difference — this asserts pointer identity, so re-introducing a
    /// per-occurrence copy (`Arc::from(ical.to_string())` inside the loop)
    /// fails here even though every field still compares equal (#1179).
    #[test]
    fn occurrences_of_one_series_share_a_single_ical_allocation() {
        let inst = super::expand_ical_for_test(FIXTURE_DAILY, JUN_START, JUL_START).unwrap();
        assert!(inst.len() >= 2, "need at least two occurrences to compare");
        for (i, e) in inst.iter().enumerate().skip(1) {
            assert!(
                std::sync::Arc::ptr_eq(&inst[0].ical, &e.ical),
                "occurrence {i} allocated its own copy of the series' iCal string",
            );
        }
    }

    // ── Bounded expansion work (#1179) ────────────────────────────────────

    #[test]
    fn uid_is_read_out_of_an_ical_serialisation_for_the_truncation_warning() {
        assert_eq!(super::uid_from_ical(FIXTURE_DAILY), Some("fixture-daily"));
        // No UID property at all, and an empty one, both decline rather than
        // naming something wrong in a log line.
        assert_eq!(
            super::uid_from_ical("BEGIN:VEVENT\r\nSUMMARY:x\r\nEND:VEVENT\r\n"),
            None
        );
        assert_eq!(super::uid_from_ical("UID:\r\n"), None);
        // Not confused by a property whose *name* ends in UID.
        assert_eq!(
            super::uid_from_ical("X-MY-UID:nope\r\nUID:real\r\n"),
            Some("real")
        );
    }

    /// The #1179 crasher, as a test. `FREQ=MINUTELY` over the 43-day window
    /// the calendar service actually asks for (`WINDOW_DAYS` unioned with the
    /// 6-week grid, `hytte-services`' `calendar.rs`) is 61 380 occurrences.
    /// Before the fix each one was checked against every already-emitted one
    /// with a linear `Vec::contains` and carried its own clone of the
    /// component's iCal string: 1.9 × 10⁹ comparisons and ~9 MB of duplicated
    /// string, **measured at 13.6 s** on this tree — per source, on every
    /// refresh, with the calendar `Mutable` the panel binds to waiting on it.
    ///
    /// The elapsed bound here is deliberately loose (CI is slow and shares a
    /// box); it is three orders of magnitude under the pre-fix number, which
    /// is the only resolution this needs to have.
    #[test]
    fn minutely_rule_over_the_calendar_window_expands_in_bounded_work() {
        use std::time::{Duration, Instant};

        // 43 days from Jun 1 — the widest window `calendar.rs` composes.
        let window_end = JUN_START + 43 * 86_400;
        let ical = "BEGIN:VEVENT\r\nUID:minutely-1\r\nDTSTAMP:20260601T090000Z\r\n\
                     DTSTART:20260601T090000Z\r\nDTEND:20260601T090500Z\r\n\
                     SUMMARY:Tick\r\nRRULE:FREQ=MINUTELY\r\nEND:VEVENT\r\n";

        let started = Instant::now();
        let inst = super::expand_ical_for_test(ical, JUN_START, window_end).unwrap();
        let elapsed = started.elapsed();

        // Capped, not merely finished: the naive expansion is 61 380.
        assert!(
            inst.len() <= super::MAX_OCCURRENCES_PER_COMPONENT,
            "expansion returned {} occurrences, over the {} cap",
            inst.len(),
            super::MAX_OCCURRENCES_PER_COMPONENT,
        );
        assert!(
            inst.len() < 61_380,
            "the minutely series was not truncated at all ({} occurrences)",
            inst.len(),
        );
        assert!(!inst.is_empty(), "truncation must not swallow the series");

        // Truncation keeps a *prefix* — the first occurrences in order, one
        // per minute from DTSTART — not an arbitrary subset.
        for (i, e) in inst.iter().enumerate() {
            let minute = i64::try_from(i).unwrap();
            assert_eq!(
                e.start_unix,
                ANCHOR_0900 + minute * 60,
                "occurrence {i} is not the {i}th minute of the series",
            );
        }

        assert!(
            elapsed < Duration::from_secs(5),
            "expansion took {elapsed:?} — the O(n²) dedup or the per-occurrence \
             clone is back (#1179 measured 13.6 s before the fix)",
        );
    }

    /// The bytes budget, not the count cap, is what binds for a component with
    /// a large body: `EXPANSION_BYTES_BUDGET / ical.len()` occurrences. Pinned
    /// with a VEVENT padded past 419 bytes (4 MiB / 10 000), so a change that
    /// drops the bytes half of the budget shows up here rather than only on a
    /// calendar full of fat invites.
    #[test]
    fn a_large_component_is_capped_by_the_bytes_budget_not_the_count() {
        let padding = "X".repeat(8_000);
        let ical = format!(
            "BEGIN:VEVENT\r\nUID:fat-1\r\nDTSTAMP:20260601T090000Z\r\n\
             DTSTART:20260601T090000Z\r\nDTEND:20260601T090500Z\r\n\
             SUMMARY:Fat\r\nDESCRIPTION:{padding}\r\nRRULE:FREQ=MINUTELY\r\nEND:VEVENT\r\n"
        );
        let window_end = JUN_START + 43 * 86_400;
        let inst = super::expand_ical_for_test(&ical, JUN_START, window_end).unwrap();

        let len = inst[0].ical.len();
        assert!(len > 8_000, "the fixture's body did not survive parsing");
        let expected = super::EXPANSION_BYTES_BUDGET / len;
        assert!(
            expected < super::MAX_OCCURRENCES_PER_COMPONENT,
            "fixture too small to make the bytes budget the binding cap",
        );
        assert_eq!(
            inst.len(),
            expected,
            "a {len}-byte component must cap at EXPANSION_BYTES_BUDGET / {len}",
        );
    }

    // ── Skipping to the window before iterating (issue #1195) ─────────────
    //
    // `MAX_RECUR_ITERATIONS` counts iterator *steps*, and the iterator was
    // always driven from `DTSTART`. So the guard bounded the wrong quantity:
    // a rule old enough to need more than 100 000 steps to reach the window
    // was cut off *before emitting anything*, and every long-standing hourly
    // (or minutely) event was silently absent from the calendar panel rather
    // than merely truncated. `skip_iterator_to_window` re-anchors the
    // iterator on the window first, so the steps the guard counts are
    // in-window steps.
    //
    // These fixtures all start in **2014** — ~105 000 hours before the
    // window, i.e. just past the old cap, which is what made the defect
    // invisible for shorter histories.

    /// 2014-01-01T09:00:00Z, the `DTSTART` the fixtures below share.
    fn dtstart_2014() -> i64 {
        use chrono::{TimeZone as _, Utc};
        Utc.with_ymd_and_hms(2014, 1, 1, 9, 0, 0)
            .unwrap()
            .timestamp()
    }

    /// 2026-01-01T09:00:00Z — the `DTSTART` the sub-day multi-`INTERVAL`
    /// fixtures below share (#1206 HIGH-1). Five months (not twelve years)
    /// before the window: enough for the skip to have been attempted on the
    /// unfixed branch, but few enough steps (~3 615 hourly, ~452 at
    /// `INTERVAL=8`) that declining the skip and walking from `DTSTART`
    /// instead — this fix's fallback — stays comfortably under
    /// [`super::MAX_RECUR_ITERATIONS`].
    fn dtstart_2026_0900() -> i64 {
        use chrono::{TimeZone as _, Utc};
        Utc.with_ymd_and_hms(2026, 1, 1, 9, 0, 0)
            .unwrap()
            .timestamp()
    }

    /// The widest window `hytte-services`' `calendar.rs` composes: 43 days
    /// from `JUN_START`. Both ends land on an exact hour, so an hourly series
    /// has exactly `43 × 24` occurrences inside it.
    const WINDOW_43D_END: i64 = JUN_START + 43 * 86_400;

    /// The headline of #1195: a `FREQ=HOURLY` rule whose `DTSTART` is 2014
    /// must yield the window's **full** 43 × 24 = 1 032 occurrences.
    ///
    /// Measured on `origin/main` (and on #1190, whose doc claimed otherwise):
    /// **0**. Twelve years of hourly occurrences is ~105 000 iterator steps,
    /// just past `MAX_RECUR_ITERATIONS`, so the loop gave up in 2025 and the
    /// component contributed nothing at all. 1 032 is comfortably under
    /// `MAX_OCCURRENCES_PER_COMPONENT`, so nothing here is truncated either —
    /// the answer is the whole series, not a capped prefix.
    #[test]
    fn an_hourly_rule_that_started_in_2014_yields_the_windows_full_1032_occurrences() {
        let ical = "BEGIN:VEVENT\r\nUID:hourly-2014\r\nDTSTAMP:20140101T090000Z\r\n\
                     DTSTART:20140101T090000Z\r\nDTEND:20140101T093000Z\r\n\
                     SUMMARY:Long-standing\r\nRRULE:FREQ=HOURLY\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, WINDOW_43D_END).unwrap();

        assert_eq!(
            inst.len(),
            43 * 24,
            "a 2014 hourly rule must expand to the window's own hours; 0 here is \
             the #1195 defect (the iteration guard tripped before the window)",
        );
        assert!(
            inst.len() < super::MAX_OCCURRENCES_PER_COMPONENT,
            "fixture must sit under the occurrence budget, or this tests truncation",
        );
        // Exactly the hourly grid of the window, in order, 30 minutes each.
        for (i, e) in inst.iter().enumerate() {
            let hour = i64::try_from(i).unwrap();
            assert_eq!(e.start_unix, JUN_START + hour * 3_600, "occurrence {i}");
            assert_eq!(e.end_unix, e.start_unix + 1_800, "occurrence {i} duration");
            assert!(!e.all_day);
        }
    }

    /// The same rule started *inside* the window is the control: it differs
    /// from the 2014 one only by the nine hours before its own 09:00 `DTSTART`
    /// on day one (1 023 vs 1 032). That is the whole of the difference the
    /// age of a series may make — before #1195 it was 1 023 vs nothing.
    #[test]
    fn an_old_hourly_rule_and_a_fresh_one_differ_only_by_the_hours_before_dtstart() {
        let old = "BEGIN:VEVENT\r\nUID:hourly-2014\r\nDTSTAMP:20140101T090000Z\r\n\
                    DTSTART:20140101T090000Z\r\nDTEND:20140101T093000Z\r\n\
                    SUMMARY:Old\r\nRRULE:FREQ=HOURLY\r\nEND:VEVENT\r\n";
        let fresh = "BEGIN:VEVENT\r\nUID:hourly-2026\r\nDTSTAMP:20260601T090000Z\r\n\
                      DTSTART:20260601T090000Z\r\nDTEND:20260601T093000Z\r\n\
                      SUMMARY:Fresh\r\nRRULE:FREQ=HOURLY\r\nEND:VEVENT\r\n";

        let old = super::expand_ical_for_test(old, JUN_START, WINDOW_43D_END).unwrap();
        let fresh = super::expand_ical_for_test(fresh, JUN_START, WINDOW_43D_END).unwrap();

        assert_eq!(fresh.len(), 1_023, "43 × 24 less the 9 hours before 09:00");
        assert_eq!(
            old.len(),
            fresh.len() + 9,
            "the only occurrences the 2014 rule adds are the 9 hours of day one \
             that the 2026 rule has not started for yet",
        );
        // And from 09:00 on day one they are the same instants.
        assert_eq!(
            old[9..].iter().map(|e| e.start_unix).collect::<Vec<_>>(),
            fresh.iter().map(|e| e.start_unix).collect::<Vec<_>>(),
        );
    }

    /// `EXDATE` is applied to a skipped series exactly as to an unskipped one
    /// — the skip moves the iterator, it does not bypass the recurrence-set
    /// modifiers, which are read off the component and matched per occurrence.
    ///
    /// `FREQ=HOURLY` (not `DAILY`) from 2014 is deliberate (#1206 NIT-6): a
    /// 2014 daily rule is only ~4 500 steps from the window, comfortably under
    /// [`super::MAX_RECUR_ITERATIONS`], so it would still pass with the skip
    /// stubbed off entirely — this test would then be pinning EXDATE
    /// filtering alone, not the skip. Hourly from the same DTSTART is ~105
    /// 000 steps, past the guard, so this genuinely depends on the skip: stub
    /// `skip_iterator_to_window` to decline unconditionally and this reds
    /// (0 occurrences) alongside the other 2014-hourly tests.
    #[test]
    fn exdate_still_excludes_occurrences_of_a_skipped_series() {
        let ical = "BEGIN:VEVENT\r\nUID:hourly-2014\r\nDTSTAMP:20140101T090000Z\r\n\
                     DTSTART:20140101T090000Z\r\nDTEND:20140101T093000Z\r\n\
                     SUMMARY:Old standup\r\nRRULE:FREQ=HOURLY\r\n\
                     EXDATE:20260603T090000Z\r\nEXDATE:20260610T090000Z\r\n\
                     END:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, JUL_START).unwrap();

        assert_eq!(
            inst.len(),
            30 * 24 - 2,
            "30 days of June's hourly grid less the two EXDATEs",
        );
        let starts: Vec<i64> = inst.iter().map(|e| e.start_unix).collect();
        // Unlike the DAILY version of this fixture, an hourly grid hits every
        // hour of every day, so the window's very first hour (midnight, not
        // DTSTART's 09:00) is the first survivor.
        assert_eq!(starts[0], JUN_START, "June 1 00:00 survives");
        for excluded in [ANCHOR_0900 + 2 * 86_400, ANCHOR_0900 + 9 * 86_400] {
            assert!(
                !starts.contains(&excluded),
                "EXDATE {excluded} was not applied to the skipped series",
            );
        }
    }

    // ── Sub-day INTERVAL > 1 must decline the skip (#1206 HIGH-1) ──────────
    //
    // libical recovers the post-skip phase for HOURLY/MINUTELY/SECONDLY from
    // a single calendar field (`icalrecur.c`'s `__iterator_set_start`:
    // `abs(istart.hour - rstart.hour) % interval` and the minute/second
    // analogues), not the elapsed interval count. For `INTERVAL == 1` that is
    // trivially correct (anything modulo 1 is 0); for `INTERVAL > 1` it
    // re-anchors the series onto the wrong grid while leaving the occurrence
    // *count* unchanged — a proven regression against `origin/main`, which
    // drives every rule from `DTSTART` and so never hits this libical
    // behaviour at all. `skip_iterator_to_window` declines the skip for
    // exactly this shape and falls back to the (correct) `DTSTART` walk.

    /// `FREQ=HOURLY;INTERVAL=8` from a `DTSTART` five months before the
    /// window. `origin/main` (and this fix's fallback) land on 01:00Z/09:00Z/
    /// 17:00Z for June 1; the unguarded skip measured 07:00Z/15:00Z/23:00Z,
    /// two hours early throughout, with the same occurrence count either way
    /// — the silent-wrong-data failure mode HIGH-1 found. Falsify: drop the
    /// `INTERVAL == 1` guard in `skip_iterator_to_window` and this reds
    /// against the branch's wrong grid.
    #[test]
    fn an_hourly_rule_with_interval_8_stays_on_the_dtstart_grid() {
        let ical = "BEGIN:VEVENT\r\nUID:hourly-i8\r\nDTSTAMP:20260101T090000Z\r\n\
                     DTSTART:20260101T090000Z\r\nDTEND:20260101T093000Z\r\n\
                     SUMMARY:Every 8 hours\r\nRRULE:FREQ=HOURLY;INTERVAL=8\r\n\
                     END:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, WINDOW_43D_END).unwrap();

        assert!(
            inst.len() >= 3,
            "fixture too short to check the first three occurrences",
        );
        let starts: Vec<i64> = inst.iter().take(3).map(|e| e.start_unix).collect();
        assert_eq!(
            starts,
            vec![JUN_START + 3_600, ANCHOR_0900, JUN_START + 17 * 3_600],
            "must land on 01:00Z/09:00Z/17:00Z — the branch's bug lands two \
             hours early, on 07:00Z/15:00Z/23:00Z",
        );
        // Every occurrence, not just the first three, must sit on the grid —
        // the reviewer's sweep methodology, not a spot check.
        let dtstart_unix = dtstart_2026_0900();
        for e in &inst {
            assert_eq!(
                (e.start_unix - dtstart_unix).rem_euclid(8 * 3_600),
                0,
                "occurrence {} is off the INTERVAL=8 grid",
                e.start_unix,
            );
        }
    }

    /// `FREQ=HOURLY;INTERVAL=12` — the twice-daily shape, same bug as
    /// `INTERVAL=8` above but a different grid: `origin/main` (and this
    /// fix) land on 09:00Z/21:00Z; the unguarded skip measured 03:00Z/
    /// 15:00Z, six hours early.
    #[test]
    fn an_hourly_rule_with_interval_12_stays_on_the_dtstart_grid() {
        let ical = "BEGIN:VEVENT\r\nUID:hourly-i12\r\nDTSTAMP:20260101T090000Z\r\n\
                     DTSTART:20260101T090000Z\r\nDTEND:20260101T093000Z\r\n\
                     SUMMARY:Twice daily\r\nRRULE:FREQ=HOURLY;INTERVAL=12\r\n\
                     END:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, WINDOW_43D_END).unwrap();

        assert!(
            inst.len() >= 2,
            "fixture too short to check the first two occurrences",
        );
        let starts: Vec<i64> = inst.iter().take(2).map(|e| e.start_unix).collect();
        assert_eq!(
            starts,
            vec![ANCHOR_0900, JUN_START + 21 * 3_600],
            "must land on 09:00Z/21:00Z — the branch's bug lands six hours \
             early, on 03:00Z/15:00Z",
        );
        let dtstart_unix = dtstart_2026_0900();
        for e in &inst {
            assert_eq!(
                (e.start_unix - dtstart_unix).rem_euclid(12 * 3_600),
                0,
                "occurrence {} is off the INTERVAL=12 grid",
                e.start_unix,
            );
        }
    }

    /// `FREQ=MINUTELY;INTERVAL=7` — the reviewer's sweep found this
    /// frequency breaks too (the same bug's minute analogue). Checked the
    /// same way the sweep was: every emitted start must sit on the DTSTART
    /// grid, not just a spot-checked few.
    #[test]
    fn a_minutely_rule_with_interval_7_stays_on_the_dtstart_grid() {
        let ical = "BEGIN:VEVENT\r\nUID:minutely-i7\r\nDTSTAMP:20260101T090300Z\r\n\
                     DTSTART:20260101T090300Z\r\nDTEND:20260101T090800Z\r\n\
                     SUMMARY:Every 7 minutes\r\nRRULE:FREQ=MINUTELY;INTERVAL=7\r\n\
                     END:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, WINDOW_43D_END).unwrap();

        assert!(
            !inst.is_empty(),
            "fixture must contribute occurrences to the window",
        );
        use chrono::{TimeZone as _, Utc};
        let dtstart_unix = Utc
            .with_ymd_and_hms(2026, 1, 1, 9, 3, 0)
            .unwrap()
            .timestamp();
        for e in &inst {
            assert_eq!(
                (e.start_unix - dtstart_unix).rem_euclid(7 * 60),
                0,
                "occurrence {} is off the INTERVAL=7 grid",
                e.start_unix,
            );
        }
    }

    /// The skip target is an absolute UTC instant, but libical re-anchors the
    /// rule in `DTSTART`'s own frame. `SKIP_BACKOFF_SECS` exists so that a
    /// conversion this crate does not reproduce cannot cost an occurrence: a
    /// `TZID=Europe/Berlin` series started in 2014 must still produce every
    /// one of June's hours. (Two days of backoff against ±14 h of possible
    /// offset; the over-generated occurrences are dropped by the window
    /// filter.)
    #[test]
    fn a_zoned_series_started_in_2014_keeps_every_in_window_occurrence() {
        let ical = berlin_calendar(
            "UID:hourly-berlin-2014\r\nDTSTAMP:20140101T083000Z\r\n\
             DTSTART;TZID=Europe/Berlin:20140101T093000\r\n\
             DTEND;TZID=Europe/Berlin:20140101T094500\r\nSUMMARY:Zoned\r\n\
             RRULE:FREQ=HOURLY\r\n",
        );
        let inst = super::expand_ical_for_test(&ical, JUN_START, JUL_START).unwrap();

        // Berlin is CEST (UTC+2) for all of June, and the series sits on the
        // half hour, so June's occurrences are the UTC grid …:30 — 24 a day,
        // 30 days. The 2026-05-31T23:30Z occurrence ends at 23:45, before the
        // window, so it is correctly excluded.
        assert_eq!(inst.len(), 30 * 24, "June's half-hourly Berlin grid");
        for (i, e) in inst.iter().enumerate() {
            let hour = i64::try_from(i).unwrap();
            assert_eq!(
                e.start_unix,
                JUN_START + 1_800 + hour * 3_600,
                "occurrence {i} drifted — the skip landed in the wrong frame",
            );
        }
    }

    /// libical refuses to fast-forward an RRULE carrying `COUNT` (starting
    /// late would change which occurrences the count selects), so such a rule
    /// is still expanded from `DTSTART` — and must still come out right. A
    /// 60-day count begun a month before the window contributes its
    /// in-window tail and nothing else.
    #[test]
    fn a_count_rule_libical_will_not_skip_is_still_expanded_correctly() {
        let ical = "BEGIN:VEVENT\r\nUID:daily-count\r\nDTSTAMP:20260501T090000Z\r\n\
                     DTSTART:20260501T090000Z\r\nDTEND:20260501T093000Z\r\n\
                     SUMMARY:Sixty days\r\nRRULE:FREQ=DAILY;COUNT=60\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, WINDOW_43D_END).unwrap();

        // DTSTART is May 1; occurrences 31..59 of the count fall on or after
        // June 1, and the count runs out well inside the window.
        assert_eq!(inst.len(), 29, "occurrences 31..=59 of a 60-day count");
        assert_eq!(inst[0].start_unix, ANCHOR_0900, "the June 1 occurrence");
        for (i, e) in inst.iter().enumerate() {
            let day = i64::try_from(i).unwrap();
            assert_eq!(e.start_unix, ANCHOR_0900 + day * 86_400, "occurrence {i}");
        }
    }

    /// A `FREQ=YEARLY` rule that also trips the `COUNT` refusal — exercising
    /// the frequency whose `__iterator_set_start` branch does the most work
    /// (day-of-year expansion), not just `DAILY`'s single `increment_monthday`
    /// — must still expand correctly once `skip_iterator_to_window` rebuilds
    /// the iterator on refusal (#1206 MEDIUM-2). A 10-year count begun in
    /// 2020 contributes exactly its one in-window year.
    #[test]
    fn a_yearly_count_rule_that_trips_the_refusal_still_yields_its_in_window_instance() {
        let ical = "BEGIN:VEVENT\r\nUID:yearly-count\r\nDTSTAMP:20200605T090000Z\r\n\
                     DTSTART:20200605T090000Z\r\nDTEND:20200605T093000Z\r\n\
                     SUMMARY:Anniversary\r\nRRULE:FREQ=YEARLY;COUNT=10\r\nEND:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, WINDOW_43D_END).unwrap();

        // DTSTART 2020-06-05; the count runs 2020..=2029, so 2026-06-05 is
        // occurrence #6 of 10 and the only one inside the 43-day window.
        assert_eq!(
            inst.len(),
            1,
            "exactly the 2026 anniversary should fall in the window",
        );
        assert_eq!(
            inst[0].start_unix,
            ANCHOR_0900 + 4 * 86_400,
            "2026-06-05T09:00:00Z",
        );
    }

    /// A series that ended before the window contributes nothing: the skip
    /// must not resurrect it by re-anchoring past its `UNTIL`.
    #[test]
    fn a_series_that_ended_before_the_window_stays_empty_after_the_skip() {
        let ical = "BEGIN:VEVENT\r\nUID:hourly-until\r\nDTSTAMP:20140101T090000Z\r\n\
                     DTSTART:20140101T090000Z\r\nDTEND:20140101T093000Z\r\n\
                     SUMMARY:Finished\r\nRRULE:FREQ=HOURLY;UNTIL=20150101T000000Z\r\n\
                     END:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, WINDOW_43D_END).unwrap();
        assert!(
            inst.is_empty(),
            "a rule whose UNTIL is eleven years before the window emitted {} \
             occurrences",
            inst.len(),
        );
        assert!(
            dtstart_2014() < JUN_START,
            "fixture sanity: the series really does predate the window",
        );
    }

    /// The residue #1195 deliberately leaves, pinned so it is a known shape
    /// rather than a surprise: a `COUNT` rule is the one thing libical will
    /// not fast-forward, so `MAX_RECUR_ITERATIONS` still binds on it from
    /// `DTSTART`. A 200 000-hour count begun in 2014 runs out of steps ~2025,
    /// before the window — nothing is emitted, and the expansion stays cheap
    /// and logs the iteration-guard `warn!` rather than the budget one.
    ///
    /// The fix for this, if a real calendar ever produces one, is to compute
    /// the skip arithmetically for the simple frequencies instead of asking
    /// libical; it is not worth the second recurrence implementation until
    /// something needs it.
    #[test]
    fn a_count_rule_too_long_to_fast_forward_is_still_bounded_by_the_guard() {
        use std::time::{Duration, Instant};

        let ical = "BEGIN:VEVENT\r\nUID:hourly-huge-count\r\nDTSTAMP:20140101T090000Z\r\n\
                     DTSTART:20140101T090000Z\r\nDTEND:20140101T093000Z\r\n\
                     SUMMARY:Pathological\r\nRRULE:FREQ=HOURLY;COUNT=200000\r\n\
                     END:VEVENT\r\n";
        let started = Instant::now();
        let inst = super::expand_ical_for_test(ical, JUN_START, WINDOW_43D_END).unwrap();
        let elapsed = started.elapsed();

        assert!(
            inst.is_empty(),
            "documented residue: 100 000 hourly steps from 2014 stop in 2025, \
             short of the window — got {} occurrences instead, which means the \
             skip now applies to COUNT rules and this test should become an \
             equality on the real occurrence set",
            inst.len(),
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the iteration guard no longer bounds a COUNT rule ({elapsed:?})",
        );
    }

    /// The external fact the `COUNT` fallback rests on, pinned rather than
    /// quoted from a header: libical **refuses** to re-anchor an iterator for
    /// an RRULE carrying `COUNT` (starting late would change which occurrences
    /// the count selects) and accepts one for a rule without it. Every
    /// expansion-level assertion above would still pass if
    /// `i_cal_recur_iterator_set_start` silently did nothing on a COUNT rule
    /// — or silently restarted the count — so this reads the return value
    /// directly. Change the answer here and `MAX_RECUR_ITERATIONS`'s doc,
    /// `Truncation::IterationGuard` and the residue test below all become
    /// wrong at once.
    #[test]
    fn libical_refuses_to_skip_a_count_rule_and_accepts_one_without() {
        /// Build the iterator `expand_component` builds for a given RRULE and
        /// report what `skip_iterator_to_window` says about skipping it to
        /// June 2026, plus whether it rebuilt the iterator (#1206 MEDIUM-2).
        fn iterator_skips(recurrence_rule: &str) -> (bool, bool) {
            let ical = format!(
                "BEGIN:VEVENT\r\nUID:probe\r\nDTSTAMP:20140101T090000Z\r\n\
                 DTSTART:20140101T090000Z\r\nDTEND:20140101T093000Z\r\n\
                 SUMMARY:Probe\r\nRRULE:{recurrence_rule}\r\nEND:VEVENT\r\n"
            );
            let comp = super::parse_vevent(&ical).unwrap();
            // SAFETY: `comp.raw` is the live VEVENT just parsed, owned by this
            // scope until the `drop` below; the accessor returns a **new**
            // `ICalTime` ref, released at the end.
            let dtstart = unsafe { sys::i_cal_component_get_dtstart(comp.raw) };
            // SAFETY: the same live component and libical's RRULE
            // `ICalPropertyKind` discriminant; a **new** property ref (or
            // null), released at the end.
            let prop = unsafe {
                sys::i_cal_component_get_first_property(comp.raw, sys::I_CAL_RRULE_PROPERTY)
            };
            assert!(!prop.is_null(), "fixture has an RRULE");
            // SAFETY: `prop` is that non-null property; reading its value
            // yields a **new** `ICalRecurrence` ref, released at the end.
            let rule = unsafe { sys::i_cal_property_get_rrule(prop) };
            assert!(!rule.is_null(), "fixture's RRULE parses");
            // SAFETY: both arguments are live and stay borrowed until after
            // the iterator is freed below, which is what libical requires of
            // the rule and start time an iterator is built from.
            let mut iter = unsafe { sys::i_cal_recur_iterator_new(rule, dtstart) };
            assert!(!iter.is_null(), "iterator constructs");
            let original = iter;

            // SAFETY: `iter` is the live iterator just built and not yet
            // stepped, and `rule`/`dtstart` are the same live values it was
            // built from — exactly this callee's contract.
            let skipped =
                unsafe { super::skip_iterator_to_window(&mut iter, rule, dtstart, 0, JUN_START) };
            let rebuilt = iter != original;

            // SAFETY: `iter` is the iterator this scope now owns (whether
            // the original or a replacement), freed exactly once and never
            // stepped after.
            unsafe { sys::i_cal_recur_iterator_free(iter) }
            // SAFETY: the new `ICalRecurrence` ref above, released exactly
            // once and only after the iterator built from it is freed.
            unsafe { sys::g_object_unref(rule) }
            // SAFETY: the new property ref above, released exactly once.
            unsafe { sys::g_object_unref(prop) }
            // SAFETY: the new `ICalTime` ref above, released exactly once and
            // only after the iterator that borrowed it is freed.
            unsafe { sys::g_object_unref(dtstart) }
            drop(comp);
            (skipped, rebuilt)
        }

        let (skipped, rebuilt) = iterator_skips("FREQ=HOURLY");
        assert!(
            skipped,
            "libical must accept the skip for a plain rule — without it the \
             whole of #1195 is a no-op that happens to still pass its \
             expansion tests only if nothing else changed",
        );
        assert!(!rebuilt, "a successful skip must reuse the same iterator");

        let (skipped, rebuilt) = iterator_skips("FREQ=HOURLY;COUNT=200000");
        assert!(
            !skipped,
            "libical accepted a skip on a COUNT rule; the fallback path, its \
             warn! and MAX_RECUR_ITERATIONS's doc are all written around the \
             refusal",
        );
        assert!(
            rebuilt,
            "a refused skip must rebuild the iterator (#1206 MEDIUM-2) — \
             \"false ⇒ iterate from DTSTART as before\" is a promise about \
             the iterator returned, not a description of the one the \
             caller happened to be holding",
        );
    }
}
