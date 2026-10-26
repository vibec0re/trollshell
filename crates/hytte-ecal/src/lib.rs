//! Safe wrappers around the minimal libecal / libedataserver / libical-glib
//! subset we need to drive Evolution Data Server task lists. Reads + writes
//! work against ANY backend (local, CalDAV, Google, EWS, …) — libecal does
//! the per-backend translation.
//!
//! ## Threading
//!
//! Every public method is **sync** and blocks. EDS spawns its own threads
//! internally; libecal's GMainContext usage means a process-wide
//! `GMainContext` must be iterated for some operations (notably async
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
//!   it transfers to us; `take_error` reads and frees it at most once.
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

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::marker::PhantomData;
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
/// reflects the series origin — trust `start_unix`/`end_unix`, not it). An
/// occurrence someone edited on its own (a detached instance, carrying
/// `RECURRENCE-ID`) comes back **once**, as that component, in place of the
/// master's occurrence it replaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventInstance {
    /// iCalendar serialisation of the component this occurrence belongs to.
    ///
    /// **Shared, not cloned** (#1179): every occurrence of one series points
    /// at the same allocation, because the string is identical across them by
    /// construction — it is the *master* component's serialisation, the same
    /// bytes for occurrence 1 and occurrence 60 000. (A detached instance
    /// carries its own component's, and the occurrences a
    /// `RANGE=THISANDFUTURE` instance reschedules share that one's.) It is an
    /// [`Arc`] rather than an `Rc` because expansion runs on the EDS worker
    /// thread and the instances are handed to the UI thread.
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
        self.sources_by_extension(sys::E_SOURCE_EXTENSION_CALENDAR)
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
/// dropping calls `g_object_unref`. Borrow via `Self::raw` for sub-
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
        self.has_extension_c(&c)
    }

    /// [`has_extension`](Self::has_extension) for a name that is already
    /// NUL-terminated — the form the `E_SOURCE_EXTENSION_*` constants take, so
    /// an in-crate caller skips the `CString` round-trip. The single call site
    /// of `e_source_has_extension`, so its `unsafe` block is written once.
    fn has_extension_c(&self, extension_name: &CStr) -> bool {
        // SAFETY: P1/P3, and `extension_name` is a live `&CStr` (NUL-terminated
        // by construction) that outlives the call.
        let r = unsafe { sys::e_source_has_extension(self.raw, extension_name.as_ptr()) };
        r != 0
    }

    /// The colour **the user picked** for this calendar — the `[Calendar]
    /// Color=` key of the `.source` keyfile, which Evolution and GNOME Calendar
    /// write when someone chooses a colour. The shell tints its event rows with
    /// it instead of the hash-derived palette (#1223 item 1).
    ///
    /// `None` when the source carries no `Calendar` extension, when the stored
    /// value isn't a `#rrggbb` / `#rrggbbaa` hex triple, **and — the case that
    /// is not obvious — when EDS is reporting its own default rather than a
    /// choice.** There is no "unset" to read at this layer:
    /// `ESourceSelectable`'s `color` property is a *construct* property with
    /// `default-value="#62a0ea"`, so every calendar source answers that string
    /// from the moment it exists, keyfile key or not. This method reports that
    /// exact value as no colour (see [`EDS_DEFAULT_CALENDAR_COLOR`]), so a
    /// caller's own fallback still applies to a calendar nobody has coloured —
    /// without it, every unconfigured calendar on screen would collapse to one
    /// GNOME blue and the caller's palette would be dead code.
    ///
    /// The cost of that, stated plainly: a user who deliberately picks
    /// `#62a0ea` is indistinguishable from one who never picked anything, and
    /// gets the palette. That is the right trade — the palette colour is a
    /// perfectly good dot, where the alternative loses every distinction
    /// between untouched calendars.
    ///
    /// EDS stores whatever GTK's colour parser accepted, so a named colour
    /// (`red`), an `rgb(…)` form, or a short `#rgb` is possible and is
    /// deliberately dropped rather than handed to a caller that expects hex —
    /// conservative but never wrong. Nothing here panics on a malformed value.
    #[must_use]
    pub fn color(&self) -> Option<String> {
        // This guard is **load-bearing**, not belt-and-braces.
        // `e_source_get_extension` is not a query: EDS documents it as
        // *creating* the extension when the source doesn't carry one, and its
        // return is not nullable (see the `sys` declaration). Without asking
        // first, `color()` would instantiate an `ESourceCalendar` on every task
        // list, address book and memo source it is ever called on.
        if !self.has_extension_c(sys::E_SOURCE_EXTENSION_CALENDAR) {
            return None;
        }
        // SAFETY: P1/P3, and the name is a `'static` NUL-terminated `&CStr`.
        // The returned `ESourceExtension*` is a borrow owned by this `ESource`
        // (see its `sys` declaration) — valid for this `&self` borrow, and
        // never unref'd here.
        let ext = unsafe {
            sys::e_source_get_extension(self.raw, sys::E_SOURCE_EXTENSION_CALENDAR.as_ptr())
        };
        // Defensive only, and unreachable here: `e_source_get_extension`
        // answers NULL (after a `g_critical`) solely for an extension *name* no
        // subclass registered, and `Calendar` is one libedataserver defines
        // itself. It is **not** what covers "this source has no `[Calendar]`
        // group" — the `has_extension_c` guard above is.
        if ext.is_null() {
            return None;
        }
        // SAFETY: `ext` is the non-null extension just checked, alive for this
        // borrow of `self` (P1/P3); the `Calendar` group instantiates an
        // `ESourceSelectable`, so the cast names the subclass that accessor
        // expects. `_dup_color` copies the value under the extension's own
        // property lock and **transfers the copy to us** — so the pointer below
        // is ours alone, and freeing it is this function's job.
        let raw =
            unsafe { sys::e_source_selectable_dup_color(ext.cast::<sys::ESourceSelectable>()) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: `raw` is non-null (checked) and NUL-terminated, as
        // `_dup_color`'s `gchar*` is; the copy — including the replacement
        // characters `to_string_lossy` allocates for a non-UTF-8 byte — is
        // taken here, before the free below, and `raw` is never read after it.
        let value = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: the GLib-allocated copy `_dup_color` transferred to us,
        // freed exactly once on every path out of this function (the two early
        // returns above are both before the allocation exists) and never read
        // after.
        unsafe { sys::g_free(raw.cast::<c_void>()) }
        user_picked_color(value)
    }

    pub(crate) fn raw(&self) -> *mut sys::ESource {
        self.raw
    }
}

/// EDS's own default for an [`sys::ESourceSelectable`]'s colour — GNOME blue.
///
/// **Not a colour anybody picked.** `ESourceSelectable`'s `color` is a
/// *construct* property with `default-value="#62a0ea"` (EDS 3.60.2's
/// `EDataServer-1.2.gir`, `<property name="color" … construct="1"
/// default-value="#62a0ea">`), so every calendar source carries it from
/// construction whether or not its `.source` keyfile has a `[Calendar] Color=`
/// key. `e_source_selectable_dup_color` therefore answers this string rather
/// than `NULL` for an uncoloured calendar: at the FFI layer, "unset" does not
/// exist.
///
/// Confirmed end to end in the `eds-nixos-test` VM (run 35331529534, EDS
/// 3.60.2): a seeded calendar with a `[Calendar]` group and **no** `Color=`
/// printed `color=#62a0ea`, as did EDS's auto-provisioned `Personal`.
/// libedataserver exposes no C constant for it — the literal appears in the
/// GIR's `default-value` and in `libedataserver-1.2.so`'s strings, and a
/// `grep -i 62a0ea` over `include/evolution-data-server/` finds nothing — which
/// is why this crate carries its own copy, and why an EDS release that changes
/// the default must redden that VM assertion rather than silently pass.
///
/// [`Source::color`] reports this value as [`None`]; see that method for the
/// reasoning and for what it costs.
pub const EDS_DEFAULT_CALENDAR_COLOR: &str = "#62a0ea";

/// The filter [`Source::color`] applies to whatever string EDS hands back:
/// `Some` only for a hex triple somebody actually chose.
///
/// Two rejections, both `None` so the caller's own fallback applies — a
/// spelling that isn't `#rrggbb`/`#rrggbbaa` ([`is_hex_color`]), and EDS's
/// construct-time default ([`EDS_DEFAULT_CALENDAR_COLOR`], compared
/// case-insensitively because EDS stores the user's spelling verbatim and
/// `#62A0EA` is the same colour).
///
/// Pure and total, and a separate function for exactly that reason: the FFI
/// read above needs a live EDS, this does not, so this is where both decisions
/// are tested. Dropping the default compare here is what the VM's
/// `color=(none)` assertion catches in the other direction.
fn user_picked_color(value: String) -> Option<String> {
    if !is_hex_color(&value) {
        return None;
    }
    if value.eq_ignore_ascii_case(EDS_DEFAULT_CALENDAR_COLOR) {
        return None;
    }
    Some(value)
}

/// The shape check behind [`user_picked_color`]: `true` only for a `#` followed
/// by exactly 6 (`#rrggbb`) or 8 (`#rrggbbaa`) ASCII hex digits.
///
/// Pure and total — the FFI read above needs a live EDS, this does not, so it
/// is where the "is this a colour?" decision is tested. Deliberately strict:
/// the one consumer (the shell's calendar rows) parses the string as a colour
/// and falls back to its palette for anything it can't use, and a value that
/// only *some* parsers accept is better dropped at the source than carried to
/// the surface that has to guess.
fn is_hex_color(s: &str) -> bool {
    let Some(hex) = s.strip_prefix('#') else {
        return false;
    };
    matches!(hex.len(), 6 | 8) && hex.bytes().all(|b| b.is_ascii_hexdigit())
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
    /// Implementation: we fetch every component (`#t`) and expand each master
    /// with libical's **core recurrence iterator**
    /// (`i_cal_recur_iterator_new` / `_next`) over the window — the engine
    /// the higher-level `e_cal_*_generate_instances_*` helpers wrap. Driving
    /// the iterator directly keeps expansion a pure function of the components
    /// we already hold, independent of EDS backend state. A master's `EXDATE`
    /// properties (cancelled occurrences) are excluded and its `RDATE`
    /// properties (extra one-off occurrences) added — see `expand_component`.
    ///
    /// **Detached instances.** `#t` does not return masters only: an
    /// occurrence someone edited on its own ("move just this week's meeting to
    /// Wednesday") is stored as a second component with the master's `UID`
    /// and a `RECURRENCE-ID` naming the occurrence it replaces, and EDS hands
    /// it back as a sibling of its master. It is reconciled with the master
    /// the way `e_cal_client_generate_instances` does it: the master's
    /// occurrence at that recurrence id is suppressed, and the detached
    /// component stands in for it at its own time — shown iff that time
    /// overlaps the window, whichever side of the window the original
    /// occurrence was on. See `expand_components`.
    pub fn generate_instances(&self, start_unix: i64, end_unix: i64) -> Result<Vec<EventInstance>> {
        // Fetch every component, masters and detached instances alike. We
        // need the live `ICalComponent*` (not the iCal string) to expand, so
        // we walk the GSList ourselves rather than going through
        // `get_object_strings`.
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

        // A master can only be expanded once every detached instance of it is
        // known, so collect the whole list before expanding any of it.
        let mut comps: Vec<*mut sys::ICalComponent> = Vec::new();
        let mut node = out_list;
        while !node.is_null() {
            // SAFETY: `node` is non-null (loop condition) and names a node of
            // the list we own until the free below (`sys::GSList` mirrors
            // glib's layout `#[repr(C)]`; nothing mutates it, P3).
            let comp = unsafe { (*node).data }.cast::<sys::ICalComponent>();
            if !comp.is_null() {
                comps.push(comp);
            }
            // SAFETY: as the `data` read above.
            node = unsafe { (*node).next };
        }

        let mut out: Vec<EventInstance> = Vec::new();
        // SAFETY: `expand_components`' contract is a slice of live, borrowed
        // `ICalComponent*`s — each is a non-null element of the list we own,
        // alive until the free below, and the callee frees nothing it does
        // not itself create.
        unsafe { expand_components(&comps, start_unix, end_unix, &mut out) }

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
/// releases at most the iterator it was given (never `rule`/`dtstart`) and, on
/// a rebuild, hands back a new owned iterator in `*iter`; the caller releases
/// whatever `*iter` holds when done, exactly once, with `g_object_unref`.
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
        sys::I_CAL_HOURLY_RECURRENCE
            | sys::I_CAL_MINUTELY_RECURRENCE
            | sys::I_CAL_SECONDLY_RECURRENCE
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
    // guarantees and has not been released yet — one GObject ref, owned by
    // the caller and handed to us here. Dropping it finalizes the wrapper,
    // whose finalizer frees the native iterator (`sys::ICalRecurIterator`
    // says why this is `g_object_unref` and never `i_cal_recur_iterator_free`).
    // Releasing it here (and replacing it below) is what makes "false ⇒
    // iterate from DTSTART as before" true unconditionally rather than only
    // for the `COUNT` refusal.
    unsafe { sys::g_object_unref(*iter) }
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
/// …and then the series' **detached instances** (`overrides`, gathered from
/// its sibling components by [`expand_components`]): an occurrence whose
/// recurrence id — its own start, as the master defines it — is overridden is
/// not emitted here, because the detached component is emitted in its place;
/// and an occurrence after a `RANGE=THISANDFUTURE` override is rescheduled by
/// it ([`FutureOverride`]). Since a rescheduled occurrence may come from
/// outside the window, the series is expanded over the window widened by the
/// furthest such shift ([`SeriesOverrides::reach`]); what is *emitted* is
/// still filtered against the real window.
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
    overrides: Option<&SeriesOverrides>,
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
    // SAFETY: `comp` is live by this function's contract, which is all
    // `component_duration` asks.
    let duration = unsafe { component_duration(comp, dtstart_unix) };

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

    // The recurrence ids to expand over: the window, widened by however far
    // a `THISANDFUTURE` override moves occurrences, so one rescheduled into
    // the window from outside it is still generated. Zero for every series
    // without such an override, which is nearly all of them.
    let (reach_before, reach_after) = overrides.map_or((0, 0), SeriesOverrides::reach);
    let ids_from = start_unix.saturating_sub(reach_before);
    let ids_until = end_unix.saturating_add(reach_after);

    let mut emit = Emitter::new(
        ical,
        exdates,
        duration,
        all_day,
        (start_unix, end_unix),
        overrides,
    );

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
            // whole life — `rule` and `dtstart` are released only after the
            // iterator itself is released below, which is what libical's
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
                    skip_iterator_to_window(&mut iter, rule, dtstart, dtstart_unix, ids_from);
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
                        if occ_unix >= ids_until {
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
                    // replacement `skip_iterator_to_window` built — and its
                    // one GObject ref is released exactly once here and the
                    // iterator never stepped after. The wrapper's finalizer
                    // frees the native iterator; `i_cal_recur_iterator_free`
                    // would free only that and leak the wrapper
                    // (`sys::ICalRecurIterator`).
                    unsafe { sys::g_object_unref(iter) }
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
/// applying — in this order — EXDATE exclusion, the series' detached
/// instances, the query window, dedup, and the #1179 work budget. Split out of
/// [`expand_component`] so the budget lives in one readable place (and so that
/// function stays under clippy's `too_many_lines`); it holds no raw pointer
/// and needs no `unsafe`.
struct Emitter<'a> {
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
    /// What the series' detached instances do to its occurrences; `None` for
    /// a series that has none (and for a detached instance's own emission).
    overrides: Option<&'a SeriesOverrides>,
    /// Whichever of [`MAX_OCCURRENCES_PER_COMPONENT`] and
    /// [`EXPANSION_BYTES_BUDGET`] binds first for this component's body size.
    max_occurrences: usize,
}

impl<'a> Emitter<'a> {
    /// `window` is the query window `[start, end)` in UTC seconds.
    fn new(
        ical: Arc<str>,
        exdates: HashSet<i64>,
        duration: i64,
        all_day: bool,
        (window_start, window_end): (i64, i64),
        overrides: Option<&'a SeriesOverrides>,
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
            overrides,
            max_occurrences,
        }
    }

    /// Push the occurrence whose recurrence id (its start as the series
    /// defines it) is `occ_unix`, if it survives the filters.
    ///
    /// Returns `false` **only** once the work budget is spent, so a caller
    /// stops iterating instead of spinning the recurrence iterator for
    /// occurrences it would discard. A merely filtered-out occurrence (EXDATE,
    /// detached, outside the window, duplicate) returns `true`: it consumed no
    /// budget.
    fn emit(&mut self, out: &mut Vec<EventInstance>, occ_unix: i64) -> bool {
        // EXDATE excludes; the window bounds the rest. An occurrence is kept
        // when it starts before the window end and its end is at/after the
        // window start (so it overlaps the window).
        if self.exdates.contains(&occ_unix) {
            return true;
        }
        // Copied out of `self` so the borrow it hands back is `'a`, not one
        // of `self` — `self.emitted` is mutated below while it is held.
        let overrides = self.overrides;
        // A detached instance stands in for this occurrence; it is emitted on
        // its own (`expand_components`), at its own time, so emitting this one
        // too would show the event twice.
        if overrides.is_some_and(|o| o.replaced.contains(&occ_unix)) {
            return true;
        }
        // A `THISANDFUTURE` override before it reschedules it.
        let rescheduled = overrides.and_then(|o| o.future_for(occ_unix));
        let (start_unix, duration, all_day) = match rescheduled {
            Some(f) => (occ_unix.saturating_add(f.shift), f.duration, f.all_day),
            None => (occ_unix, self.duration, self.all_day),
        };
        let end_unix = start_unix.saturating_add(duration);
        if start_unix >= self.window_end || end_unix < self.window_start {
            return true;
        }
        if self.emitted.contains(&(start_unix, end_unix)) {
            return true;
        }
        if self.emitted.len() >= self.max_occurrences {
            return false;
        }
        self.emitted.insert((start_unix, end_unix));
        out.push(EventInstance {
            ical: Arc::clone(rescheduled.map_or(&self.ical, |f| &f.ical)),
            start_unix,
            end_unix,
            all_day,
        });
        true
    }
}

// ── Detached recurrence instances ────────────────────────────────────────────
//
// RFC 5545 §3.8.4.4: an occurrence of a recurring event edited on its own —
// moved, retitled, cancelled — is stored as a second component with the
// series' `UID` and a `RECURRENCE-ID` naming the occurrence it replaces. EDS
// hands it back from `get_object_list_sync` as a *sibling* of the master, not
// inside it (the CalDAV backend's cache keys components by `(uid, rid)`), so
// nothing about the master says which of its occurrences are overridden.
// Expanding each component on its own — what this crate did before — shows a
// weekly meeting moved from Tuesday 10:00 to Wednesday 11:00 at **both**
// times. `expand_components` reconciles the two the way EDS's own
// `e_cal_client_generate_instances` (`e-cal-client.c`, `generate_instances` /
// `process_detached_instances`) does:
//
// - the master's occurrence whose recurrence id matches is not emitted;
// - the detached component is emitted once, at its **own** DTSTART/DTEND, and
//   never expanded (an RRULE copied onto it by a client is ignored);
// - each is filtered against the window on its own times, so an occurrence
//   moved out of the window disappears from it and one moved in appears —
//   whichever side the original was on;
// - a detached component whose master is not in the list (or has no UID)
//   still shows, standing alone.
//
// One thing EDS does not do is applied too: a `RANGE=THISANDFUTURE` override
// also reschedules every later occurrence of its series, as RFC 5545
// §3.8.4.4 specifies ("all subsequent instances are also rescheduled by the
// same time difference … [and] modified to have this same duration"), taking
// its properties as well — see `FutureOverride`. EDS, Thunderbird and Nextcloud
// all *split* a series rather than write one, so this is rare; it is handled
// so that, when one does arrive, the rescheduled occurrences are not shown at
// their old times.

/// A detached recurrence instance, read once, up front, by
/// [`expand_components`] — the master needs to know its recurrence id before
/// it is expanded, and emitting it afterwards then needs no FFI.
struct Detached {
    /// The series it belongs to. `None` (a component with no `UID`) overrides
    /// nothing and is just shown.
    uid: Option<String>,
    /// The instant its `RECURRENCE-ID` names — the master occurrence it
    /// replaces — normalised exactly as that occurrence is (through
    /// [`ical_time_to_unix`], with the `TZID` resolved as DTSTART's is), so
    /// the two compare equal. `None` if unreadable: it then overrides nothing.
    recurrence_id: Option<i64>,
    /// `RANGE=THISANDFUTURE` on the `RECURRENCE-ID`.
    this_and_future: bool,
    /// Its own DTSTART, `None` if undatable (it is then not shown, though it
    /// still suppresses the occurrence it replaces).
    start: Option<i64>,
    /// Its own `DTEND − DTSTART` (0 if absent).
    duration: i64,
    all_day: bool,
    /// Its own serialisation — what the calendar shows for it.
    ical: Arc<str>,
}

/// What one series' detached instances do to its master's occurrences, keyed
/// by recurrence id.
#[derive(Default)]
struct SeriesOverrides {
    /// The recurrence id of every detached instance of the series, whatever
    /// its `RANGE`: the master's occurrence there is not emitted, because the
    /// detached component is.
    replaced: HashSet<i64>,
    /// The series' `RANGE=THISANDFUTURE` instances, sorted by recurrence id.
    future: Vec<FutureOverride>,
}

/// A `RANGE=THISANDFUTURE` detached instance's effect on the master's
/// occurrences **after** it (RFC 5545 §3.8.4.4): each is moved by the same
/// time difference the detached instance was, takes its duration, and — as
/// Evolution shows it — its properties. "After" is by recurrence id, and a
/// later detached instance of either range wins over it ("subsequent
/// instances defined in separate components are not impacted").
struct FutureOverride {
    recurrence_id: i64,
    /// Detached DTSTART − recurrence id.
    shift: i64,
    duration: i64,
    all_day: bool,
    ical: Arc<str>,
}

impl SeriesOverrides {
    /// The `THISANDFUTURE` instance governing the occurrence with recurrence
    /// id `occ`: the latest one strictly before it. One *at* `occ` is in
    /// `replaced`, which the caller checks first.
    fn future_for(&self, occ: i64) -> Option<&FutureOverride> {
        let before = self.future.partition_point(|f| f.recurrence_id < occ);
        before.checked_sub(1).map(|i| &self.future[i])
    }

    /// How far before and after the window the master's recurrence ids must
    /// be expanded for every occurrence a `THISANDFUTURE` override moves *into*
    /// the window to be found: one moved later by `shift` comes from up to
    /// `shift + duration` before the window start (it still overlaps the
    /// window if its new end does), one moved earlier from up to `-shift`
    /// after the window end. `(0, 0)` without such an override.
    fn reach(&self) -> (i64, i64) {
        let before = self
            .future
            .iter()
            .map(|f| f.shift.saturating_add(f.duration))
            .max()
            .unwrap_or(0)
            .max(0);
        let after = self
            .future
            .iter()
            .map(|f| f.shift.saturating_neg())
            .max()
            .unwrap_or(0)
            .max(0);
        (before, after)
    }
}

/// Expand every component `get_object_list_sync` handed back — masters, and
/// the detached instances that override some of their occurrences — over
/// `[start_unix, end_unix)`, pushing each occurrence into `out`. See the
/// "Detached recurrence instances" notes above for the reconciliation.
///
/// Two passes: the first reads every component's `RECURRENCE-ID` and indexes
/// the detached ones by `UID`, because a master may come before or after its
/// detached instances in the list; the second expands each master with its
/// series' overrides ([`expand_component`]) and emits each detached instance
/// once, at its own time.
///
/// # Safety
///
/// Every pointer in `comps` must be a live `ICalComponent*` (a VEVENT) for
/// the whole call. Borrowed — none is freed here.
unsafe fn expand_components(
    comps: &[*mut sys::ICalComponent],
    start_unix: i64,
    end_unix: i64,
    out: &mut Vec<EventInstance>,
) {
    let detached: Vec<Option<Detached>> = comps
        .iter()
        // SAFETY: each `comp` is live and borrowed for this call (this
        // function's contract), which is all `read_detached` asks.
        .map(|&comp| unsafe { read_detached(comp) })
        .collect();

    let mut series: HashMap<&str, SeriesOverrides> = HashMap::new();
    for d in detached.iter().flatten() {
        let (Some(uid), Some(recurrence_id)) = (d.uid.as_deref(), d.recurrence_id) else {
            continue;
        };
        let overrides = series.entry(uid).or_default();
        overrides.replaced.insert(recurrence_id);
        if let (true, Some(start)) = (d.this_and_future, d.start) {
            overrides.future.push(FutureOverride {
                recurrence_id,
                shift: start.saturating_sub(recurrence_id),
                duration: d.duration,
                all_day: d.all_day,
                ical: Arc::clone(&d.ical),
            });
        }
    }
    for overrides in series.values_mut() {
        overrides.future.sort_by_key(|f| f.recurrence_id);
    }

    for (&comp, detached) in comps.iter().zip(&detached) {
        if let Some(d) = detached {
            emit_detached(d, start_unix, end_unix, out);
        } else {
            // SAFETY: `comp` is live (this function's contract); the UID is
            // copied out before the borrow ends.
            let uid = unsafe { component_uid(comp) };
            let overrides = uid.as_deref().and_then(|uid| series.get(uid));
            // SAFETY: `comp` is a live, borrowed VEVENT — exactly
            // `expand_component`'s contract — and that callee frees nothing it
            // did not create.
            unsafe { expand_component(comp, start_unix, end_unix, overrides, out) }
        }
    }
}

/// Emit a detached instance once, at its own time, if that overlaps the
/// window. It is never expanded: a `RECURRENCE-ID` component is one occurrence
/// by definition, even if a client copied the master's RRULE onto it.
fn emit_detached(d: &Detached, start_unix: i64, end_unix: i64, out: &mut Vec<EventInstance>) {
    let Some(start) = d.start else {
        return;
    };
    Emitter::new(
        Arc::clone(&d.ical),
        HashSet::new(),
        d.duration,
        d.all_day,
        (start_unix, end_unix),
        None,
    )
    .emit(out, start);
}

/// The [`Detached`] reading of `comp` if it carries a `RECURRENCE-ID`; `None`
/// for a master (no `RECURRENCE-ID` property at all).
///
/// # Safety
///
/// `comp` must be a live `ICalComponent*`. Borrowed — never freed here. Every
/// libical object this creates is released before it returns.
unsafe fn read_detached(comp: *mut sys::ICalComponent) -> Option<Detached> {
    // SAFETY: `comp` is live (this function's contract) and
    // `I_CAL_RECURRENCEID_PROPERTY` is libical's discriminant for the
    // property; a **new** property ref (or null), released below.
    let prop =
        unsafe { sys::i_cal_component_get_first_property(comp, sys::I_CAL_RECURRENCEID_PROPERTY) };
    if prop.is_null() {
        return None;
    }
    // Resolved against `comp` exactly as DTSTART is, so a `TZID`'d id compares
    // equal to the zoned occurrence it names (see the `sys` declaration for
    // why not `i_cal_component_get_recurrenceid`).
    //
    // SAFETY: `prop` is the non-null property just taken and `comp` the live
    // component it belongs to; both are borrowed, and the time comes back as
    // a **new** ref (or null), released below.
    let rid_time = unsafe { sys::i_cal_property_get_datetime_with_component(prop, comp) };
    // SAFETY: null or a live borrow of the ref just taken.
    let recurrence_id = unsafe { ical_time_to_unix(rid_time) };
    if !rid_time.is_null() {
        // SAFETY: that new ref, released exactly once; only the `i64` read out
        // of it is used afterwards.
        unsafe { sys::g_object_unref(rid_time) }
    }
    // SAFETY: `prop` is still the live property we hold, and
    // `I_CAL_RANGE_PARAMETER` is libical's `ICalParameterKind` for `RANGE=`;
    // a **new** wrapper ref (or null), released below.
    let range =
        unsafe { sys::i_cal_property_get_first_parameter(prop, sys::I_CAL_RANGE_PARAMETER) };
    let this_and_future = !range.is_null()
        // SAFETY: short-circuit — reached only for the non-null parameter just
        // taken, read without taking ownership; the `c_int` is compared, never
        // transmuted.
        && unsafe { sys::i_cal_parameter_get_range(range.cast_const()) }
            == sys::I_CAL_RANGE_THISANDFUTURE;
    if !range.is_null() {
        // SAFETY: the wrapper ref taken above, released exactly once (the
        // native parameter is the property's and is not freed by this).
        unsafe { sys::g_object_unref(range) }
    }
    // SAFETY: the property ref taken at the top, released exactly once, after
    // everything read from it.
    unsafe { sys::g_object_unref(prop) }

    // SAFETY: `comp` is live; a **new** `ICalTime` ref (or null), released
    // below.
    let dtstart = unsafe { sys::i_cal_component_get_dtstart(comp) };
    // SAFETY: null or a live borrow of the ref just taken.
    let start = unsafe { ical_time_to_unix(dtstart) };
    // SAFETY: as above.
    let all_day = unsafe { ical_time_is_date(dtstart) };
    if !dtstart.is_null() {
        // SAFETY: that new ref, released exactly once and not used again.
        unsafe { sys::g_object_unref(dtstart) }
    }
    // SAFETY: `comp` is live, which is all `component_duration` asks.
    let duration = start.map_or(0, |s| unsafe { component_duration(comp, s) });

    Some(Detached {
        // SAFETY: `comp` is live; the UID is copied out.
        uid: unsafe { component_uid(comp) },
        recurrence_id,
        this_and_future,
        start,
        duration,
        all_day,
        // SAFETY: `comp` is live; `component_ical_string` borrows and frees
        // nothing.
        ical: Arc::from(unsafe { component_ical_string(comp) }),
    })
}

/// `DTEND − DTSTART` of `comp` in seconds, given its DTSTART as
/// `dtstart_unix`; 0 when DTEND is absent, undatable or before DTSTART (the
/// calendar service fabricates a UI duration for a zero-length instance).
/// libical's DTEND accessor already derives the end from a `DURATION`
/// property when there is no `DTEND`.
///
/// # Safety
///
/// `comp` must be a live `ICalComponent*`. Borrowed — never freed here.
unsafe fn component_duration(comp: *mut sys::ICalComponent, dtstart_unix: i64) -> i64 {
    // SAFETY: `comp` is live (this function's contract); this returns a new
    // `ICalTime` ref (or null), released below.
    let dtend = unsafe { sys::i_cal_component_get_dtend(comp) };
    // SAFETY: null or a live borrow of the ref just taken.
    let duration = match unsafe { ical_time_to_unix(dtend) } {
        Some(e) if e >= dtstart_unix => e - dtstart_unix,
        _ => 0,
    };
    if !dtend.is_null() {
        // SAFETY: the new ref taken above, released exactly once; `dtend` is
        // not used again.
        unsafe { sys::g_object_unref(dtend) }
    }
    duration
}

/// The `UID` of `comp`, copied out; `None` if it has none.
///
/// # Safety
///
/// `comp` must be a live `ICalComponent*`. Borrowed — never freed here.
unsafe fn component_uid(comp: *mut sys::ICalComponent) -> Option<String> {
    // SAFETY: `comp` is live (this function's contract); the accessor returns
    // a `const char*` the component owns (transfer none), which
    // `borrowed_cstr` copies before this borrow ends and never frees.
    unsafe { borrowed_cstr(sys::i_cal_component_get_uid(comp)) }
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
/// expands the first VEVENT it finds, and returns the occurrences. For a
/// calendar whose VEVENTs must be expanded together (a master and its
/// detached instances), use [`expand_calendar_for_test`].
pub fn expand_ical_for_test(
    ical: &str,
    start_unix: i64,
    end_unix: i64,
) -> Result<Vec<EventInstance>> {
    let comp = parse_vevent(ical)?;
    let mut out = Vec::new();
    // SAFETY: `comp.raw` is the live VEVENT `parse_vevent` just produced and
    // this scope owns until the `drop` below — exactly the borrowed, live
    // `ICalComponent*` `expand_components` requires of each element.
    unsafe { expand_components(&[comp.raw], start_unix, end_unix, &mut out) }
    drop(comp);
    Ok(out)
}

/// [`expand_ical_for_test`] for a whole VCALENDAR: **every** VEVENT in `ical`
/// is expanded together, through the same reconciliation
/// [`CalClient::generate_instances`] applies to the components EDS hands
/// back — so a recurring master and its detached instances (same `UID`, one
/// carrying `RECURRENCE-ID`) come out as a calendar shows them, not each on
/// its own. A bare VEVENT is a calendar of one.
pub fn expand_calendar_for_test(
    ical: &str,
    start_unix: i64,
    end_unix: i64,
) -> Result<Vec<EventInstance>> {
    let calendar = parse_vevents(ical)?;
    let comps: Vec<*mut sys::ICalComponent> = calendar.events.iter().map(|c| c.raw).collect();
    let mut out = Vec::new();
    // SAFETY: every element is the `raw` of a `Component` `calendar` owns
    // until the `drop` below — live, borrowed `ICalComponent*`s, which is
    // `expand_components`' contract.
    unsafe { expand_components(&comps, start_unix, end_unix, &mut out) }
    drop(calendar);
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
/// - **UTC (`…Z`):** already absolute; libical converts it with the UTC
///   singleton, which is also the zone such a time carries.
/// - **Resolved zone (a `TZID` whose `VTIMEZONE` libical could resolve —
///   registered on the component, or a builtin like `Europe/Berlin` or
///   `/freeassociation.sourceforge.net/Europe/Berlin`):** read the wall clock
///   in the time's **own** zone (#522), not as UTC. Since #1590 it is read by
///   [`wall_clock_to_unix`], RFC 5545 §3.3.5's rule, over that zone's UTC →
///   offset map ([`IcalZone`]): the first occurrence in a fall-back fold, the
///   offset before the gap in a spring-forward gap. libical's own conversion
///   (`i_cal_time_as_timet_with_zone` with that zone) gets both edges wrong,
///   taking the later instant in a fold and the post-gap offset in a gap, so
///   it is used only if the rule finds no instant at all (a zone whose
///   offsets chrono cannot hold).
/// - **Genuinely floating DATE-TIME:** interpret the wall-clock fields in the
///   **local** system zone via chrono's `Local` (DST-correct), not UTC (#388).
///   A wall clock inside a DST fold or gap still names one instant, by RFC
///   5545 §3.3.5's rule (see [`WallClock::to_unix_in`]).
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

    if is_utc {
        // A `…Z` time: already absolute, and its own zone (when it carries
        // one) is this same UTC singleton, so libical's conversion has no
        // choice to make. Checked before the resolved-zone branch below so a
        // UTC time never pays for the zone sampling there.
        //
        // SAFETY: as the DATE branch above — a nullary getter for libical's
        // borrowed, never-unref'd process-wide singleton.
        let utc = unsafe { sys::i_cal_timezone_get_utc_timezone() };
        // SAFETY: as the DATE branch above — a read-only conversion of the
        // live borrow, with the singleton as the source zone.
        return Some(unsafe { sys::i_cal_time_as_timet_with_zone(tt_const, utc.cast_const()) });
    }

    if has_own_zone {
        // A DATE-TIME whose `TZID`'s `VTIMEZONE` libical could resolve
        // (registered on the component, or a builtin like `Europe/Berlin`).
        // Its wall clock is read in that zone, never as UTC (#522: reading it
        // as UTC put a `TZID=Europe/Berlin` 12:30 at 14:30 CEST), and by RFC
        // 5545 §3.3.5's rule on a DST transition (#1590), which libical's own
        // `i_cal_time_as_timet_with_zone(tt, own_zone)` does not follow: it
        // reads a fold's wall clock as the later instant and a gap's with the
        // post-gap offset. So only the zone's UTC → offset map is taken from
        // libical (`IcalZone`), and `wall_clock_to_unix`, the rule a floating
        // time is read by, does the rest.
        //
        // SAFETY: `own_zone` is non-null (this branch) and is a borrow `tt`
        // holds, so it lives at least as long as `tt`, which is live for this
        // whole call (this function's contract). The `IcalZone` is used only
        // inside this branch, i.e. within that borrow.
        let zone = unsafe { IcalZone::new(own_zone) };
        // SAFETY: `WallClock::from_ical` requires a non-null live `ICalTime*`;
        // `tt` is non-null (checked at the top) and live for this borrow.
        let wall = unsafe { WallClock::from_ical(tt) };
        return wall.to_unix_in(&zone).or_else(|| {
            // No instant by the rule: the zone has an offset chrono cannot
            // hold (a `VTIMEZONE` past ±24 h), or the fields are out of range.
            // libical's own answer is what this branch returned before #1590,
            // which beats dropping the event.
            //
            // SAFETY: `tt_const` is the live borrow from above and `own_zone`
            // the non-null zone it holds, never unref'd. Read-only.
            Some(unsafe { sys::i_cal_time_as_timet_with_zone(tt_const, own_zone.cast_const()) })
        });
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

    /// Interpret these fields as a wall-clock time in the **local** system
    /// zone and return POSIX UTC seconds — [`to_unix_in`](Self::to_unix_in)
    /// with chrono's `Local`.
    fn to_local_unix(self) -> Option<i64> {
        self.to_unix_in(&chrono::Local)
    }

    /// Interpret these fields as a wall-clock time in `tz` and return POSIX
    /// UTC seconds. Parameterised over the zone so the DST edges below are
    /// testable without touching the process's `TZ`; production only ever
    /// passes `Local` ([`to_local_unix`](Self::to_local_unix)).
    ///
    /// A wall clock that falls on a DST transition still names exactly one
    /// instant, by the rule RFC 5545 §3.3.5 gives for a `TZID` time (applied
    /// here to a floating one, which is read in the viewer's zone, #388):
    ///
    /// - **Fall-back fold** (the local time occurs twice — 02:30 on
    ///   2026-10-25 in `Europe/Berlin`): the **first** occurrence, i.e. the
    ///   earlier instant, still on the pre-transition offset.
    /// - **Spring-forward gap** (the local time never occurs — 02:30 on
    ///   2026-03-29): read with **the UTC offset in effect before the gap**,
    ///   which lands it as far past the gap as it was into it (03:30 CEST).
    ///
    /// Before this the conversion took chrono's `.single()`, which is `None`
    /// on both edges, and `None` is fatal one level up: an undatable
    /// `DTSTART` drops the whole event, and an undatable occurrence reads as
    /// "series exhausted" in `expand_component`'s iterator loop, so a
    /// floating 02:30 daily series silently ended at the first transition.
    ///
    /// `None` now means only that the fields do not name a calendar date and
    /// time at all (a field out of range).
    fn to_unix_in<Tz: chrono::TimeZone>(self, tz: &Tz) -> Option<i64> {
        use chrono::{NaiveDate, NaiveTime};

        let month = u32::try_from(self.month).ok()?;
        let day = u32::try_from(self.day).ok()?;
        let hour = u32::try_from(self.hour).ok()?;
        let minute = u32::try_from(self.minute).ok()?;
        let second = u32::try_from(self.second).ok()?;

        let date = NaiveDate::from_ymd_opt(self.year, month, day)?;
        let time = NaiveTime::from_hms_opt(hour, minute, second)?;
        wall_clock_to_unix(tz, date.and_time(time))
    }
}

/// How far [`offset_before_gap`] searches back for the last local time that
/// exists before a spring-forward gap, and in what steps. Real gaps are 30
/// minutes (Lord Howe Island) to an hour; the outlier is a whole day (Samoa
/// skipped 2011-12-30 crossing the date line), which 48 hours covers twice
/// over. The first existing time the search finds lies less than one step
/// before the gap, so it carries the gap's pre-transition offset unless a
/// second transition falls inside that same half hour.
const GAP_SEARCH_STEP_MINUTES: i64 = 30;
const GAP_SEARCH_STEPS: i64 = 48 * 60 / GAP_SEARCH_STEP_MINUTES;

/// Map a wall-clock time in `tz` to POSIX UTC seconds by the rule RFC 5545
/// §3.3.5 gives for a `TZID` time. A floating iCalendar time is read in the
/// viewer's zone (#388), so this is the rule for it too:
///
/// - **Fall-back fold** (the local time occurs twice, e.g. 02:30 on
///   2026-10-25 in `Europe/Berlin`): the **first** occurrence, i.e. the
///   earlier instant, still on the pre-transition offset.
/// - **Spring-forward gap** (the local time never occurs, e.g. 02:30 on
///   2026-03-29): read with **the UTC offset in effect before the gap**,
///   which lands it as far past the gap as it was into it (03:30 CEST).
///
/// Of a fold's two candidates, only the ones that read back as `naive`
/// count. chrono 0.4.44's Unix `Local` counts a fold's **upper bound** as part
/// of the fold (`offset/local/tz_info/timezone.rs:249-250`, `rule.rs:265-266`
/// and `277-278`, and on the negative-DST branches `rule.rs:302-303` and
/// `330-331`, which `Europe/Dublin`'s rule takes). At exactly 03:00 on Berlin's
/// fall-back day it answers
/// `Ambiguous(03:00 +01:00, 03:00 +02:00)`. The second of those is the
/// transition instant, 01:00Z, which reads **02:00**, so taking the earlier
/// instant put a floating 03:00 an hour early (#1579). If neither candidate
/// reads back, the time is treated as a gap.
///
/// Public so `hytte-services`' task list reads a floating `DUE`, and a
/// `TZID` one in its own zone (#1588), by the same rule the calendar reads a
/// floating `DTSTART` by (#1554), not a copy of it.
///
/// `None` only when no offset before a gap can be found within 48 hours, or
/// the result does not fit chrono's range.
pub fn wall_clock_to_unix<Tz: chrono::TimeZone>(
    tz: &Tz,
    naive: chrono::NaiveDateTime,
) -> Option<i64> {
    use chrono::{MappedLocalTime, Offset as _};

    let reads_back = |dt: &chrono::DateTime<Tz>| dt.with_timezone(tz).naive_local() == naive;
    let resolved = match tz.from_local_datetime(&naive) {
        // Taken as is, even where it does not read back: `Local` answers
        // `Single(pre-gap offset)` at a gap's first instant, and that instant
        // is exactly what the gap rule below would return.
        MappedLocalTime::Single(dt) => return Some(dt.timestamp()),
        // The first occurrence is the earlier *instant*, picked by value and
        // not by position. chrono documents the pair as `(earliest, latest)`
        // (and `MappedLocalTime::earliest` just takes the first), but its
        // Unix `Local` (0.4.44, `tz_info`'s `find_local_time_type_from_local`)
        // orders it by ascending UTC offset, which for a fall-back fold is
        // `(latest, earliest)`. Measured under `TZ=Europe/Berlin`:
        // `.earliest()` reads 02:30 on 2026-10-25 as 01:30Z, the CET repeat.
        MappedLocalTime::Ambiguous(a, b) => match (reads_back(&a), reads_back(&b)) {
            (true, true) => Some(a.timestamp().min(b.timestamp())),
            (true, false) => Some(a.timestamp()),
            (false, true) => Some(b.timestamp()),
            (false, false) => None,
        },
        MappedLocalTime::None => None,
    };
    resolved.or_else(|| {
        let before = offset_before_gap(tz, naive)?;
        naive
            .checked_sub_offset(before.fix())
            .map(|utc| utc.and_utc().timestamp())
    })
}

/// The UTC offset in effect just before the spring-forward gap `naive` falls
/// in: the offset of the latest local time before it that exists, searched
/// back in [`GAP_SEARCH_STEP_MINUTES`] steps. Should that time itself be
/// ambiguous (a fold just before the gap), the offset of its *later* instant
/// is the one the clock showed when the gap began — the smaller offset, since
/// the instant is the wall clock minus the offset. Picked by value, for the
/// reason [`wall_clock_to_unix`] gives.
///
/// Only an offset whose instant reads back as the probed time counts, as in
/// [`wall_clock_to_unix`] (#1579). On chrono's own zones this filter changes
/// no answer: at a fold's upper bound the candidate that does not read back
/// is the larger, pre-transition offset, which the smaller-offset rule never
/// picks. It is there so this search never trusts a candidate
/// [`wall_clock_to_unix`] would reject.
fn offset_before_gap<Tz: chrono::TimeZone>(
    tz: &Tz,
    naive: chrono::NaiveDateTime,
) -> Option<Tz::Offset> {
    use chrono::{MappedLocalTime, Offset as _};

    (1..=GAP_SEARCH_STEPS).find_map(|step| {
        let probe =
            naive.checked_sub_signed(chrono::TimeDelta::minutes(step * GAP_SEARCH_STEP_MINUTES))?;
        let reads_back = |offset: &Tz::Offset| {
            probe.checked_sub_offset(offset.fix()).is_some_and(|utc| {
                utc.checked_add_offset(tz.offset_from_utc_datetime(&utc).fix()) == Some(probe)
            })
        };
        match tz.offset_from_local_datetime(&probe) {
            MappedLocalTime::Single(offset) => Some(offset),
            MappedLocalTime::Ambiguous(a, b) => match (reads_back(&a), reads_back(&b)) {
                (true, true) if b.fix().local_minus_utc() < a.fix().local_minus_utc() => Some(b),
                (true, _) => Some(a),
                (false, true) => Some(b),
                (false, false) => None,
            },
            MappedLocalTime::None => None,
        }
    })
}

/// How far either side of a wall clock [`local_offsets`] samples a zone's
/// offset: one day. Every instant a wall clock can name lies within a real
/// UTC offset of it (−12 h to +14 h), so between the two samples.
const LOCAL_OFFSET_SAMPLE_SECS: i64 = 86_400;

/// The UTC offsets under which the wall clock `wall` names an instant in a
/// zone known only by its UTC → offset map, `offset_at`. `wall` is the wall
/// clock as seconds since the epoch, read as if it were UTC; offsets are in
/// seconds east of UTC.
///
/// An offset `o` is valid for `wall` when the instant `wall − o` really has
/// offset `o`. The candidates are the offsets a day before and a day after:
/// with at most one transition in those two days, every instant `wall` can
/// name has one of them. (#1585's sweep of every chrono-tz zone, 1970–2100,
/// found no two transitions within 48 hours.) So:
///
/// - both valid is a **fold**: `Ambiguous`, earlier instant (larger offset)
///   first, the order chrono documents;
/// - one valid is `Single`;
/// - neither is a **gap**: `None`.
///
/// Unlike chrono 0.4.44's Unix `Local` (#1579), a fold's upper bound is not in
/// the fold here: only the offset that reads back there is valid.
fn local_offsets(
    wall: i64,
    offset_at: impl Fn(i64) -> Option<i32>,
) -> chrono::MappedLocalTime<i32> {
    use chrono::MappedLocalTime;

    let valid = |off: &i32| wall.checked_sub(i64::from(*off)).and_then(&offset_at) == Some(*off);
    let before = wall
        .checked_sub(LOCAL_OFFSET_SAMPLE_SECS)
        .and_then(&offset_at);
    let after = wall
        .checked_add(LOCAL_OFFSET_SAMPLE_SECS)
        .and_then(&offset_at)
        .filter(|off| Some(*off) != before);
    match (before.filter(valid), after.filter(valid)) {
        (Some(a), Some(b)) => MappedLocalTime::Ambiguous(a.max(b), a.min(b)),
        (Some(off), None) | (None, Some(off)) => MappedLocalTime::Single(off),
        (None, None) => MappedLocalTime::None,
    }
}

/// A libical zone (the one a resolved `TZID` time carries) seen through
/// chrono's [`TimeZone`](chrono::TimeZone), so [`wall_clock_to_unix`] can read
/// a zoned wall clock by RFC 5545 §3.3.5 (#1590).
///
/// Only libical's **UTC → offset** map is used
/// ([`sys::i_cal_timezone_get_utc_offset_of_utc_time`]). It is a function:
/// every instant has one offset. The local → UTC direction is derived from
/// it by [`local_offsets`] instead of asked of libical, whose own answer
/// (`icaltimezone_get_utc_offset`, behind
/// [`sys::i_cal_time_as_timet_with_zone`]) takes the later instant in a fold
/// and the post-gap offset in a gap.
///
/// Holds raw pointers, so it is constructed only by [`IcalZone::new`], whose
/// contract is that `raw` stays live for `'a`. The lifetime keeps the value
/// (and every [`IcalZoneOffset`] chrono derives from it) inside that borrow.
#[derive(Clone, Copy, Debug)]
struct IcalZone<'a> {
    /// The zone. Borrowed from libical, never unref'd.
    raw: *mut sys::ICalTimezone,
    /// libical's UTC singleton, the zone an instant is built in before its
    /// offset is asked for. Borrowed, never unref'd.
    utc: *mut sys::ICalTimezone,
    _zone: PhantomData<&'a sys::ICalTimezone>,
}

/// The offset an [`IcalZone`] hands chrono. It carries its zone because
/// chrono rebuilds a `TimeZone` from an offset alone
/// (`TimeZone::from_offset`).
#[derive(Clone, Copy, Debug)]
struct IcalZoneOffset<'a> {
    zone: IcalZone<'a>,
    fix: chrono::FixedOffset,
}

impl chrono::Offset for IcalZoneOffset<'_> {
    fn fix(&self) -> chrono::FixedOffset {
        self.fix
    }
}

impl<'a> IcalZone<'a> {
    /// Wrap the libical zone `raw`.
    ///
    /// # Safety
    ///
    /// `raw` must be a non-null, live `ICalTimezone*` for all of `'a`.
    unsafe fn new(raw: *mut sys::ICalTimezone) -> Self {
        // SAFETY: `i_cal_timezone_get_utc_timezone` is nullary and returns
        // libical's process-wide singleton — borrowed, never unref'd.
        let utc = unsafe { sys::i_cal_timezone_get_utc_timezone() };
        Self {
            raw,
            utc,
            _zone: PhantomData,
        }
    }

    /// The offset this zone has at the instant `unix`, in seconds east of
    /// UTC. `None` if libical cannot build that instant, or if the offset is
    /// one chrono cannot hold (±24 h or more, which only a malformed
    /// `VTIMEZONE` can spell).
    fn offset_at(self, unix: i64) -> Option<i32> {
        // SAFETY: `self.utc` is libical's UTC singleton (`new`); the
        // constructor takes a plain `time_t`, a DATE flag (0: an instant) and
        // that borrowed zone, and returns a **new** `ICalTime` ref (or null),
        // released below.
        let tt = unsafe { sys::i_cal_time_new_from_timet_with_zone(unix, 0, self.utc) };
        if tt.is_null() {
            return None;
        }
        // SAFETY: `tt` is the live time just built; a read-only predicate.
        let is_null_time = unsafe { sys::i_cal_time_is_null_time(tt.cast_const()) } != 0;
        let secs = (!is_null_time).then(|| {
            // SAFETY: `self.raw` is live for `'a` (the contract of `new`, the
            // only constructor), and `self` exists only within `'a`; `tt` is
            // the live time built above. The call reads both, writes nothing
            // through the null `is_daylight` (an optional out-param) and
            // allocates nothing.
            unsafe { sys::i_cal_timezone_get_utc_offset_of_utc_time(self.raw, tt, ptr::null_mut()) }
        });
        // SAFETY: the new ref taken above, released exactly once and never
        // read after.
        unsafe { sys::g_object_unref(tt) }
        secs.filter(|secs| chrono::FixedOffset::east_opt(*secs).is_some())
    }

    /// The chrono offset for `secs` seconds east of UTC. An offset chrono
    /// cannot hold reads as UTC. [`IcalZone::offset_at`] never hands one out,
    /// so this can only see one through `offset_from_utc_datetime`'s fallback
    /// for an instant libical cannot build, and an instant read with it then
    /// fails to read back.
    fn offset(self, secs: i32) -> IcalZoneOffset<'a> {
        IcalZoneOffset {
            zone: self,
            fix: chrono::FixedOffset::east_opt(secs)
                .unwrap_or_else(|| chrono::Offset::fix(&chrono::Utc)),
        }
    }
}

impl<'a> chrono::TimeZone for IcalZone<'a> {
    type Offset = IcalZoneOffset<'a>;

    fn from_offset(offset: &IcalZoneOffset<'a>) -> Self {
        offset.zone
    }

    fn offset_from_local_date(
        &self,
        local: &chrono::NaiveDate,
    ) -> chrono::MappedLocalTime<IcalZoneOffset<'a>> {
        self.offset_from_local_datetime(&local.and_time(chrono::NaiveTime::MIN))
    }

    fn offset_from_local_datetime(
        &self,
        local: &chrono::NaiveDateTime,
    ) -> chrono::MappedLocalTime<IcalZoneOffset<'a>> {
        let zone = *self;
        local_offsets(local.and_utc().timestamp(), |unix| zone.offset_at(unix))
            .map(|secs| zone.offset(secs))
    }

    fn offset_from_utc_date(&self, utc: &chrono::NaiveDate) -> IcalZoneOffset<'a> {
        self.offset_from_utc_datetime(&utc.and_time(chrono::NaiveTime::MIN))
    }

    fn offset_from_utc_datetime(&self, utc: &chrono::NaiveDateTime) -> IcalZoneOffset<'a> {
        self.offset(self.offset_at(utc.and_utc().timestamp()).unwrap_or(0))
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

/// A private GLib `GMainContext`, pushed thread-default on construction so
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
    /// Break a `MainContext::iterate(true)` out of its block so the owning
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

/// Every VEVENT of a parsed iCalendar body, for [`expand_calendar_for_test`].
/// `events` is declared first so it drops first; each child wrapper also holds
/// its own ref on `_root` (libical-glib's owner ref), so the order is
/// belt-and-braces.
struct ParsedCalendar {
    events: Vec<Component>,
    /// Never read: the VCALENDAR the events were taken from, held so the
    /// parsed tree outlives their handles. `None` for a bare VEVENT body,
    /// which is its own single event.
    _root: Option<Component>,
}

/// Like [`parse_vevent`], but every VEVENT, in document order: a bare VEVENT
/// body is a list of one, a VCALENDAR the list of its VEVENT children.
fn parse_vevents(ical: &str) -> Result<ParsedCalendar> {
    let c = CString::new(ical).context("ical body contained an interior NUL")?;
    // SAFETY: as `parse_component` — a live `CString` outliving the call, and
    // a new component ref (or null) adopted below.
    let raw = unsafe { sys::i_cal_parser_parse_string(c.as_ptr()) };
    if raw.is_null() {
        bail!("libical: failed to parse iCalendar body");
    }
    let root = Component { raw };
    // SAFETY: `root.raw` is that live component; `isa` only reads it and its
    // `c_int` result is compared, never transmuted.
    if unsafe { sys::i_cal_component_isa(root.raw) } == sys::I_CAL_VEVENT_COMPONENT {
        return Ok(ParsedCalendar {
            events: vec![root],
            _root: None,
        });
    }
    let mut events = Vec::new();
    // SAFETY: as the loop in `parse_component` — live component, a real
    // `ICalComponentKind` variant, a new ref or null.
    let mut child = unsafe {
        sys::i_cal_component_get_first_component(root.raw, sys::ICalComponentKind::Vevent)
    };
    while !child.is_null() {
        events.push(Component { raw: child });
        // SAFETY: the same live parent and kind, continuing the walk the
        // `get_first_component` call above began; a new ref or null.
        child = unsafe {
            sys::i_cal_component_get_next_component(root.raw, sys::ICalComponentKind::Vevent)
        };
    }
    if events.is_empty() {
        bail!("libical: parsed body had no VEVENT child");
    }
    Ok(ParsedCalendar {
        events,
        _root: Some(root),
    })
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
    use std::ffi::c_void;
    use std::ptr;

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

    // ── Calendar source colour (#1223 item 1) ─────────────────────────────
    //
    // `Source::color` itself needs a live EDS (the nixosTest probe covers
    // that end to end); the decision it makes about the string EDS hands
    // back is pure, and lives here.

    /// The two spellings EDS actually stores for a calendar colour.
    #[test]
    fn hex_color_accepts_rgb_and_rgba_triples() {
        assert!(super::is_hex_color("#ff8800"));
        assert!(super::is_hex_color("#FF8800"));
        assert!(super::is_hex_color("#3584e4"));
        assert!(super::is_hex_color("#ff8800ff"));
        assert!(super::is_hex_color("#00000000"));
    }

    /// Everything else is dropped rather than passed on: EDS stores whatever
    /// GTK's colour parser accepted, and the caller wants hex or nothing.
    #[test]
    fn hex_color_rejects_everything_else() {
        for bad in [
            "",           // key present but empty
            "#",          // just the marker
            "red",        // a named colour (GTK parses it; we don't)
            "#f80",       // the short form
            "#ff88",      // 4 digits
            "#ff88000",   // 7 digits
            "#ff8800fff", // 9 digits
            "#gg8800",    // non-hex digits
            "ff8800",     // no leading '#'
            "rgb(255,0,0)",
            " #ff8800", // leading space — EDS wrote it verbatim
            "#ff8800 ", // trailing space
        ] {
            assert!(!super::is_hex_color(bad), "should have rejected {bad:?}");
        }
    }

    /// EDS's construct-time default is reported as **no colour**, so the
    /// caller's own fallback still applies to a calendar nobody has coloured.
    ///
    /// This is the whole reason `Source::color` is not just the hex check:
    /// `ESourceSelectable`'s `color` is a construct property defaulting to
    /// `#62a0ea`, so the FFI has no "unset" to answer. Measured in the VM
    /// (run 35331529534, EDS 3.60.2): a seeded calendar with no `Color=` key
    /// printed `color=#62a0ea`. Without this compare every unconfigured
    /// calendar collapses to one blue dot and the caller's palette is dead
    /// code — a visible regression for anyone who never picked a colour.
    #[test]
    fn the_eds_default_colour_is_reported_as_no_colour() {
        assert_eq!(super::EDS_DEFAULT_CALENDAR_COLOR, "#62a0ea");
        assert_eq!(
            super::user_picked_color(super::EDS_DEFAULT_CALENDAR_COLOR.to_owned()),
            None,
            "EDS's own default is not a choice; it must fall through to the caller's palette"
        );
        // EDS stores the spelling verbatim, so the compare is case-insensitive.
        assert_eq!(super::user_picked_color("#62A0EA".to_owned()), None);
        assert_eq!(super::user_picked_color("#62a0EA".to_owned()), None);
    }

    /// …and a colour somebody actually picked survives, including one that is
    /// merely *near* the default. Only the exact default is swallowed.
    #[test]
    fn a_picked_colour_survives_the_default_filter() {
        assert_eq!(
            super::user_picked_color("#ff8800".to_owned()),
            Some("#ff8800".to_owned())
        );
        assert_eq!(
            super::user_picked_color("#3584e4".to_owned()),
            Some("#3584e4".to_owned())
        );
        // One digit away from the default, and with the default's own alpha
        // spelling — neither is the default.
        assert_eq!(
            super::user_picked_color("#62a0eb".to_owned()),
            Some("#62a0eb".to_owned())
        );
        assert_eq!(
            super::user_picked_color("#62a0eaff".to_owned()),
            Some("#62a0eaff".to_owned())
        );
        // The shape check still runs first.
        assert_eq!(super::user_picked_color("red".to_owned()), None);
        assert_eq!(super::user_picked_color(String::new()), None);
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

    // ── Floating times on a DST transition ────────────────────────────────
    //
    // `WallClock::to_unix_in` is the zone-parameterised half of
    // `to_local_unix`, so these drive it with a hermetic zone instead of
    // `Local`: the process's `TZ` is shared by every test thread, so pinning
    // `Europe/Berlin` there would race the rest of the suite (the reason
    // #1472's calendar tests do not do it either). The zone below implements
    // chrono's `TimeZone` contract the way `Local` does for a host in that
    // zone, so `to_unix_in` sees the same `Single`/`Ambiguous`/`None`.

    /// The offset a [`TwoOffsetZone`] hands chrono. It carries its zone
    /// because chrono rebuilds a `TimeZone` from an offset alone
    /// (`TimeZone::from_offset`).
    #[derive(Clone, Copy, Debug)]
    struct TwoOffsetZoneOffset {
        zone: TwoOffsetZone,
        fix: chrono::FixedOffset,
    }

    impl chrono::Offset for TwoOffsetZoneOffset {
        fn fix(&self) -> chrono::FixedOffset {
            self.fix
        }
    }

    /// A hermetic zone with one shifted stretch: `base` seconds east of UTC
    /// everywhere except the UTC instants `[from, until)`, where it is
    /// `shifted`. Enough to model a year of `Europe/Berlin`, or Samoa's 2011
    /// date-line jump.
    ///
    /// `closed_folds` makes the local lookup count a fold's upper bound as
    /// part of the fold, as chrono 0.4.44's Unix `Local` does
    /// (`tz_info/timezone.rs:249-250`): the wall clock the old offset reads at
    /// the transition instant answers `Ambiguous`, although only the
    /// new-offset candidate reads back there. Without it the zone excludes
    /// that bound, as chrono-tz does.
    #[derive(Clone, Copy, Debug)]
    struct TwoOffsetZone {
        base: i32,
        shifted: i32,
        from: i64,
        until: i64,
        closed_folds: bool,
    }

    impl TwoOffsetZone {
        /// `Europe/Berlin` in 2026: CET (+01:00), except CEST (+02:00) from
        /// 2026-03-29T01:00Z to 2026-10-25T01:00Z. So local 2026-03-29
        /// 02:00–02:59 never happens, and local 2026-10-25 02:00–02:59 happens
        /// twice.
        fn berlin_2026() -> Self {
            Self {
                base: 3_600,
                shifted: 7_200,
                from: utc(2026, 3, 29, 1, 0),
                until: utc(2026, 10, 25, 1, 0),
                closed_folds: false,
            }
        }

        /// `Pacific/Apia` across 2011-12-30, which it skipped: −10:00 until
        /// 2011-12-30T10:00Z (local midnight that night), +14:00 after. The
        /// whole local day 2011-12-30 never happened — a 24-hour gap.
        fn samoa_2011() -> Self {
            Self {
                base: -36_000,
                shifted: 50_400,
                from: utc(2011, 12, 30, 10, 0),
                until: i64::MAX,
                closed_folds: false,
            }
        }

        /// `Africa/Cairo` in 2026, which falls back at 24:00: EET (+02:00),
        /// except EEST (+03:00) from 2026-04-23T22:00Z (Friday 04-24 00:00
        /// local) to 2026-10-29T21:00Z (Thursday 10-29 24:00 EEST, which
        /// becomes 23:00 EET). So local 23:00–24:00 on 10-29 happens twice,
        /// and the fold's upper bound is 00:00 on 10-30.
        fn cairo_2026() -> Self {
            Self {
                base: 7_200,
                shifted: 10_800,
                from: utc(2026, 4, 23, 22, 0),
                until: utc(2026, 10, 29, 21, 0),
                closed_folds: false,
            }
        }

        /// `America/Santiago` in 2026, a southern zone that falls back at
        /// 24:00: −03:00 in summer, −04:00 from 2026-04-05T03:00Z (Saturday
        /// 04-04 24:00 −03, which becomes 23:00 −04) to 2026-09-06T04:00Z
        /// (Sunday 09-06 00:00 −04, which becomes 01:00 −03). Here the fold is
        /// at `from`, the gap at `until`.
        fn santiago_2026() -> Self {
            Self {
                base: -10_800,
                shifted: -14_400,
                from: utc(2026, 4, 5, 3, 0),
                until: utc(2026, 9, 6, 4, 0),
                closed_folds: false,
            }
        }

        /// `America/New_York` in 2026: EST (−05:00), except EDT (−04:00) from
        /// 2026-03-08T07:00Z to 2026-11-01T06:00Z. So local 2026-03-08
        /// 02:00–02:59 never happens, and local 2026-11-01 01:00–01:59
        /// happens twice.
        fn new_york_2026() -> Self {
            Self {
                base: -18_000,
                shifted: -14_400,
                from: utc(2026, 3, 8, 7, 0),
                until: utc(2026, 11, 1, 6, 0),
                closed_folds: false,
            }
        }

        /// This zone with a fold's upper bound counted as part of the fold,
        /// the way chrono's Unix `Local` counts it.
        fn with_closed_folds(self) -> Self {
            Self {
                closed_folds: true,
                ..self
            }
        }

        fn offset_at(self, utc_secs: i64) -> i32 {
            if (self.from..self.until).contains(&utc_secs) {
                self.shifted
            } else {
                self.base
            }
        }

        fn offset(self, secs_east: i32) -> TwoOffsetZoneOffset {
            TwoOffsetZoneOffset {
                zone: self,
                fix: chrono::FixedOffset::east_opt(secs_east).unwrap(),
            }
        }
    }

    impl chrono::TimeZone for TwoOffsetZone {
        type Offset = TwoOffsetZoneOffset;

        fn from_offset(offset: &TwoOffsetZoneOffset) -> Self {
            offset.zone
        }

        fn offset_from_local_date(
            &self,
            local: &chrono::NaiveDate,
        ) -> chrono::MappedLocalTime<TwoOffsetZoneOffset> {
            self.offset_from_local_datetime(&local.and_hms_opt(0, 0, 0).unwrap())
        }

        /// A wall clock is valid under an offset iff the instant it names
        /// under that offset is one where the zone really has that offset —
        /// none valid is a gap, both valid is a fold.
        ///
        /// A fold's pair is ordered by **ascending offset**, as chrono
        /// 0.4.44's Unix `Local` orders it — which for a fall-back fold puts
        /// the *later* instant first, not the `(earliest, latest)` chrono
        /// documents. So the fold test below pins that `to_unix_in` picks the
        /// earlier instant by value rather than trusting the pair's order.
        ///
        /// With `closed_folds`, a fold's upper bound answers `Ambiguous` as
        /// well, although only its smaller-offset candidate reads back there.
        fn offset_from_local_datetime(
            &self,
            local: &chrono::NaiveDateTime,
        ) -> chrono::MappedLocalTime<TwoOffsetZoneOffset> {
            let wall = local.and_utc().timestamp();
            let valid = |off: i32| self.offset_at(wall - i64::from(off)) == off;
            let (smaller, larger) = if self.shifted < self.base {
                (self.shifted, self.base)
            } else {
                (self.base, self.shifted)
            };
            // A fold's upper bound: what the old, larger offset reads at the
            // transition instant into the smaller one.
            let at_fold_top = self.closed_folds
                && [
                    (self.from, self.base, self.shifted),
                    (self.until, self.shifted, self.base),
                ]
                .into_iter()
                .any(|(at, before, after)| {
                    before > after && at.checked_add(i64::from(before)) == Some(wall)
                });
            match (valid(smaller), valid(larger)) {
                (true, true) => {
                    chrono::MappedLocalTime::Ambiguous(self.offset(smaller), self.offset(larger))
                }
                _ if at_fold_top => {
                    chrono::MappedLocalTime::Ambiguous(self.offset(smaller), self.offset(larger))
                }
                (true, false) => chrono::MappedLocalTime::Single(self.offset(smaller)),
                (false, true) => chrono::MappedLocalTime::Single(self.offset(larger)),
                (false, false) => chrono::MappedLocalTime::None,
            }
        }

        fn offset_from_utc_date(&self, utc: &chrono::NaiveDate) -> TwoOffsetZoneOffset {
            self.offset_from_utc_datetime(&utc.and_hms_opt(0, 0, 0).unwrap())
        }

        fn offset_from_utc_datetime(&self, utc: &chrono::NaiveDateTime) -> TwoOffsetZoneOffset {
            self.offset(self.offset_at(utc.and_utc().timestamp()))
        }
    }

    /// POSIX seconds of a UTC wall clock.
    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        use chrono::{TimeZone as _, Utc};
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .unwrap()
            .timestamp()
    }

    /// A floating wall clock at `year-month-day hour:minute:00`.
    fn wall(year: i32, month: i32, day: i32, hour: i32, minute: i32) -> super::WallClock {
        super::WallClock {
            year,
            month,
            day,
            hour,
            minute,
            second: 0,
        }
    }

    /// The wall clock a [`super::WallClock`] fixture names.
    fn naive_of(w: super::WallClock) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(
            w.year,
            u32::try_from(w.month).unwrap(),
            u32::try_from(w.day).unwrap(),
        )
        .unwrap()
        .and_hms_opt(
            u32::try_from(w.hour).unwrap(),
            u32::try_from(w.minute).unwrap(),
            u32::try_from(w.second).unwrap(),
        )
        .unwrap()
    }

    /// Fixture sanity for the two edge tests: chrono reports exactly the case
    /// each one is about, so a test zone that drifted into "always `Single`"
    /// could not make them pass vacuously.
    fn mapping(zone: TwoOffsetZone, w: super::WallClock) -> &'static str {
        use chrono::{MappedLocalTime, TimeZone as _};
        match zone.offset_from_local_datetime(&naive_of(w)) {
            MappedLocalTime::Single(_) => "single",
            MappedLocalTime::Ambiguous(..) => "fold",
            MappedLocalTime::None => "gap",
        }
    }

    /// Today's case (2026-10-25, the EU fall-back day): 02:30 happens twice in
    /// Berlin, and RFC 5545 §3.3.5 says the value is the **first** one — 02:30
    /// CEST, 00:30 UTC. The old `.single()` answered `None` here, which drops
    /// the event (or ends a floating series at this occurrence); chrono's own
    /// `.earliest()` would answer 01:30Z, the CET repeat, because the test
    /// zone orders the pair the way Unix `Local` does.
    #[test]
    fn a_floating_time_in_the_fall_back_fold_is_its_first_occurrence() {
        let zone = TwoOffsetZone::berlin_2026();
        let w = wall(2026, 10, 25, 2, 30);
        assert_eq!(mapping(zone, w), "fold", "fixture: 02:30 must be ambiguous");
        assert_eq!(
            w.to_unix_in(&zone),
            Some(utc(2026, 10, 25, 0, 30)),
            "02:30 on the fall-back day is 02:30 CEST (00:30Z), not the CET \
             repeat (01:30Z) and not dropped",
        );
    }

    /// The spring gap: 02:30 on 2026-03-29 never happens in Berlin. RFC 5545
    /// §3.3.5 reads it with the offset before the gap (+01:00), which is
    /// 01:30 UTC — 03:30 CEST, as far past the gap as it was into it.
    #[test]
    fn a_floating_time_in_the_spring_forward_gap_uses_the_offset_before_it() {
        let zone = TwoOffsetZone::berlin_2026();
        let w = wall(2026, 3, 29, 2, 30);
        assert_eq!(mapping(zone, w), "gap", "fixture: 02:30 must not exist");
        assert_eq!(
            w.to_unix_in(&zone),
            Some(utc(2026, 3, 29, 1, 30)),
            "02:30 in the gap is read at +01:00 (01:30Z = 03:30 CEST), not dropped",
        );
        // The gap's first minute is the transition instant itself.
        assert_eq!(
            wall(2026, 3, 29, 2, 0).to_unix_in(&zone),
            Some(utc(2026, 3, 29, 1, 0))
        );
    }

    /// The gap search reaches back far enough for the one gap that is a whole
    /// day: Samoa's 2011-12-30. Noon that day is read at −10:00, the offset the
    /// clock showed before it jumped (22:00 UTC).
    #[test]
    fn a_floating_time_in_a_whole_skipped_day_still_resolves() {
        let zone = TwoOffsetZone::samoa_2011();
        let w = wall(2011, 12, 30, 12, 0);
        assert_eq!(mapping(zone, w), "gap", "fixture: the day must not exist");
        assert_eq!(w.to_unix_in(&zone), Some(utc(2011, 12, 30, 22, 0)));
    }

    /// Everything around the two edges is unchanged: a wall clock that names
    /// one instant maps to exactly that instant, on either side of either
    /// transition and well away from both — and fields that name no date at
    /// all are still `None`.
    #[test]
    fn floating_times_off_a_transition_are_unchanged() {
        let zone = TwoOffsetZone::berlin_2026();
        for (w, expected) in [
            (wall(2026, 1, 15, 11, 30), utc(2026, 1, 15, 10, 30)), // CET
            (wall(2026, 7, 22, 11, 30), utc(2026, 7, 22, 9, 30)),  // CEST
            (wall(2026, 3, 29, 1, 59), utc(2026, 3, 29, 0, 59)),   // before the gap
            (wall(2026, 3, 29, 3, 0), utc(2026, 3, 29, 1, 0)),     // after it
            (wall(2026, 10, 25, 1, 59), utc(2026, 10, 24, 23, 59)), // before the fold
            (wall(2026, 10, 25, 3, 0), utc(2026, 10, 25, 2, 0)),   // after it
        ] {
            assert_eq!(mapping(zone, w), "single", "fixture: {w:?}");
            assert_eq!(w.to_unix_in(&zone), Some(expected), "{w:?}");
        }
        assert_eq!(wall(2026, 13, 1, 0, 0).to_unix_in(&zone), None);
        assert_eq!(wall(2026, 2, 30, 0, 0).to_unix_in(&zone), None);
        assert_eq!(wall(2026, 1, 1, 24, 0).to_unix_in(&zone), None);
    }

    /// The wall clock `instant` reads in `zone`, through the zone's own
    /// UTC-to-local map.
    fn reads(zone: TwoOffsetZone, instant: i64) -> chrono::NaiveDateTime {
        chrono::DateTime::from_timestamp(instant, 0)
            .unwrap()
            .with_timezone(&zone)
            .naive_local()
    }

    /// #1579: a fold's upper bound, on a zone that counts it as part of the
    /// fold the way chrono 0.4.44's Unix `Local` does. There the pair holds
    /// the transition instant, which reads an hour earlier, and taking the
    /// earlier instant put the time there. The wall clock happens once, and
    /// the answer is the instant that reads it.
    ///
    /// - Berlin, 03:00 on 2026-10-25 (the issue's case): 02:00Z, not 01:00Z.
    /// - Cairo, 00:00 on 2026-10-30, after a fall-back at 24:00: 22:00Z the
    ///   day before, not 21:00Z, which reads 23:00 on 10-29.
    /// - Santiago, 00:00 on 2026-04-05, the same at the zone's `from` end:
    ///   04:00Z, not 03:00Z.
    #[test]
    fn a_floating_time_at_a_folds_upper_bound_is_the_instant_that_reads_it() {
        for (zone, w, want, transition) in [
            (
                TwoOffsetZone::berlin_2026(),
                wall(2026, 10, 25, 3, 0),
                utc(2026, 10, 25, 2, 0),
                utc(2026, 10, 25, 1, 0),
            ),
            (
                TwoOffsetZone::cairo_2026(),
                wall(2026, 10, 30, 0, 0),
                utc(2026, 10, 29, 22, 0),
                utc(2026, 10, 29, 21, 0),
            ),
            (
                TwoOffsetZone::santiago_2026(),
                wall(2026, 4, 5, 0, 0),
                utc(2026, 4, 5, 4, 0),
                utc(2026, 4, 5, 3, 0),
            ),
        ] {
            let zone = zone.with_closed_folds();
            assert_eq!(
                mapping(zone, w),
                "fold",
                "fixture: {w:?} must answer the fold `Local` reports there"
            );
            assert_eq!(
                reads(zone, transition) + chrono::TimeDelta::hours(1),
                reads(zone, want),
                "fixture: the transition instant for {w:?} reads an hour earlier"
            );
            assert_eq!(reads(zone, want), naive_of(w), "fixture: {w:?}");
            assert_eq!(w.to_unix_in(&zone), Some(want), "{w:?}");
        }
    }

    /// The fold's inside and lower bound, and the gap, on the same
    /// closed-bound zones: still the first occurrence and the offset before
    /// the gap, as in the open-bound tests above.
    #[test]
    fn closed_fold_bounds_leave_the_rest_of_a_transition_alone() {
        let berlin = TwoOffsetZone::berlin_2026().with_closed_folds();
        let cairo = TwoOffsetZone::cairo_2026().with_closed_folds();
        let santiago = TwoOffsetZone::santiago_2026().with_closed_folds();
        for (zone, w, want) in [
            (berlin, wall(2026, 10, 25, 2, 0), utc(2026, 10, 25, 0, 0)),
            (berlin, wall(2026, 10, 25, 2, 30), utc(2026, 10, 25, 0, 30)),
            (berlin, wall(2026, 3, 29, 2, 30), utc(2026, 3, 29, 1, 30)),
            (cairo, wall(2026, 10, 29, 23, 30), utc(2026, 10, 29, 20, 30)),
            (santiago, wall(2026, 4, 4, 23, 30), utc(2026, 4, 5, 2, 30)),
            (santiago, wall(2026, 9, 6, 0, 30), utc(2026, 9, 6, 4, 30)),
        ] {
            assert_eq!(w.to_unix_in(&zone), Some(want), "{w:?}");
        }
    }

    const LOCAL_CHILD: &str = "HYTTE_1579_WALL_CLOCK_LOCAL_CHILD";
    const LOCAL_CHILD_OK: &str = "hytte-1579-wall-clock-local-child-reached-the-end";

    /// #1579 against the real `Local`, the zone production passes. This
    /// re-execs the test binary with a POSIX `TZ` string, which chrono parses
    /// itself, so no tzdata file is needed and the parallel suite's `TZ` is
    /// never touched (the pattern of `hytte-services`' calendar
    /// `local_midnight_local_tests`, #1577). Each row is a floating wall clock
    /// and the UTC instant it must resolve to; a row marked `once` names a
    /// time that exists, so the child also checks that the instant reads it
    /// back.
    #[test]
    fn a_floating_time_at_a_folds_upper_bound_resolves_in_local() {
        const BERLIN: &str = "CET-1CEST,M3.5.0,M10.5.0/3";
        const NEW_YORK: &str = "EST5EDT,M3.2.0,M11.1.0";
        const SANTIAGO: &str = "CLT4CLST,M9.1.6/24,M4.1.6/24";
        const CAIRO: &str = "EET-2EEST,M4.5.5/0,M10.5.4/24";
        for (tz, naive, want, kind) in [
            // The issue's case: 03:00 CET, not 01:00Z (which reads 02:00).
            (
                BERLIN,
                "2026-10-25T03:00:00",
                "2026-10-25T02:00:00+00:00",
                "once",
            ),
            // Inside the fold: the first occurrence, 02:30 CEST.
            (
                BERLIN,
                "2026-10-25T02:30:00",
                "2026-10-25T00:30:00+00:00",
                "once",
            ),
            // In the spring gap: the offset before it, i.e. 03:30 CEST.
            (
                BERLIN,
                "2026-03-29T02:30:00",
                "2026-03-29T01:30:00+00:00",
                "gap",
            ),
            // The US falls back at 02:00: 02:00 EST, not 06:00Z (01:00 EST).
            (
                NEW_YORK,
                "2026-11-01T02:00:00",
                "2026-11-01T07:00:00+00:00",
                "once",
            ),
            // Santiago falls back at Saturday 24:00 −03: Sunday 00:00 −04,
            // not 03:00Z (23:00 on Saturday).
            (
                SANTIAGO,
                "2026-04-05T00:00:00",
                "2026-04-05T04:00:00+00:00",
                "once",
            ),
            // Its September gap at 00:00, which this `Local` answers `None`.
            (
                SANTIAGO,
                "2026-09-06T00:30:00",
                "2026-09-06T04:30:00+00:00",
                "gap",
            ),
            // Cairo falls back at Thursday 24:00 +03: Friday 00:00 +02, not
            // 21:00Z (23:00 on Thursday).
            (
                CAIRO,
                "2026-10-30T00:00:00",
                "2026-10-29T22:00:00+00:00",
                "once",
            ),
        ] {
            let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "--nocapture",
                    "--test-threads=1",
                    "tests::wall_clock_local_child",
                ])
                .env("TZ", tz)
                .env(LOCAL_CHILD, format!("{naive} {want} {kind}"))
                .output()
                .expect("re-exec the test binary");
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                out.status.success() && stdout.contains(LOCAL_CHILD_OK),
                "TZ={tz} {naive}:\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// The child half of the test above; a no-op outside its child.
    #[test]
    fn wall_clock_local_child() {
        use chrono::{Datelike as _, Timelike as _};

        let Ok(arg) = std::env::var(LOCAL_CHILD) else {
            return;
        };
        let mut args = arg.split(' ');
        let (Some(naive), Some(want), Some(kind)) = (args.next(), args.next(), args.next()) else {
            panic!("`<naive> <want> <once|gap>`: {arg}");
        };
        let naive: chrono::NaiveDateTime = naive.parse().expect("a wall clock");
        let w = super::WallClock {
            year: naive.year(),
            month: i32::try_from(naive.month()).unwrap(),
            day: i32::try_from(naive.day()).unwrap(),
            hour: i32::try_from(naive.hour()).unwrap(),
            minute: i32::try_from(naive.minute()).unwrap(),
            second: i32::try_from(naive.second()).unwrap(),
        };
        let got = w
            .to_local_unix()
            .and_then(|unix| chrono::DateTime::from_timestamp(unix, 0))
            .expect("a floating time always resolves");
        assert_eq!(got.to_rfc3339(), want, "{naive}");
        if kind == "once" {
            // Read back through the zone's own UTC-to-local map.
            assert_eq!(
                got.with_timezone(&chrono::Local).naive_local(),
                naive,
                "{naive}: the instant reads another wall clock"
            );
        }
        println!("{LOCAL_CHILD_OK}");
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

    // ── `TZID` times on a DST transition (#1590 item 4) ───────────────────
    //
    // Until #1590 a resolved `TZID` time was converted by libical alone
    // (`i_cal_time_as_timet_with_zone` with the time's own zone), and libical
    // 3.0.20 answers both edges of a transition differently from RFC 5545
    // §3.3.5. Its `icaltimezone_get_utc_offset` (`icaltimezone.c:818-973`)
    // matches a wall time against each change at the change's *pre*-change
    // local time (`:882`), so:
    //
    // - in a **gap** it takes the post-gap offset, which puts the time an
    //   hour *before* the gap: 02:30 Berlin on 2026-03-29 becomes 00:30Z,
    //   which reads 01:30 CET. The RFC takes the offset before the gap: 01:30Z,
    //   which reads 03:30 CEST;
    // - in a **fold** its overlap branch (`:930-960`) keeps the
    //   post-transition change unless the time's `is_daylight` bit asks for
    //   the other one, and a parsed time never sets that bit. So it takes the
    //   *later* instant, where the RFC takes the first occurrence.
    //
    // The upper bound of a fold and of a gap it gets right. The rows below pin
    // libical's own answers (the probe) and the instants the expansion gives
    // now. Each row's RFC instant is also checked against #1585's
    // `wall_clock_to_unix` on the hermetic `TwoOffsetZone` model of the same
    // zone, so a `TZID` time and a floating one follow one rule.
    //
    // The inline-`VTIMEZONE` rows run everywhere. The builtin rows (a bare
    // `TZID=Europe/Berlin`, and the `/freeassociation.sourceforge.net/` form
    // Evolution writes) need libical to find the host's zoneinfo, which the
    // nix build sandbox does not have. There they are skipped with a message;
    // libical then reads the `TZID` as floating, which is a different path.

    /// An inline `America/New_York` `VTIMEZONE` (the 2007 US rules): EDT from
    /// the second Sunday of March at 02:00, EST from the first Sunday of
    /// November at 02:00. Self-contained, like [`BERLIN_VTIMEZONE`].
    const NEW_YORK_VTIMEZONE: &str = "BEGIN:VTIMEZONE\r\nTZID:America/New_York\r\n\
         BEGIN:DAYLIGHT\r\nTZOFFSETFROM:-0500\r\nTZOFFSETTO:-0400\r\nTZNAME:EDT\r\n\
         DTSTART:20070311T020000\r\nRRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=2SU\r\nEND:DAYLIGHT\r\n\
         BEGIN:STANDARD\r\nTZOFFSETFROM:-0400\r\nTZOFFSETTO:-0500\r\nTZNAME:EST\r\n\
         DTSTART:20071104T020000\r\nRRULE:FREQ=YEARLY;BYMONTH=11;BYDAY=1SU\r\nEND:STANDARD\r\n\
         END:VTIMEZONE\r\n";

    /// One wall clock on, or at the edge of, a DST transition.
    #[derive(Clone, Copy, Debug)]
    struct TzidEdge {
        /// What the model zone says the wall clock is: `"fold"`, `"gap"`, or
        /// `"single"` for a transition's upper bound, which happens once.
        kind: &'static str,
        wall: super::WallClock,
        /// RFC 5545 §3.3.5's instant: the first occurrence in a fold, the
        /// offset before the gap in a gap.
        rfc: i64,
        /// What libical 3.0.20 answers on its own, as measured.
        libical: i64,
        /// The hermetic model of the zone, the oracle for `rfc`.
        model: TwoOffsetZone,
    }

    /// A zone the rows cover: its inline `VTIMEZONE`, the location its
    /// builtin `TZID`s are spelled from, and its rows.
    struct EdgeZone {
        vtimezone: &'static str,
        location: &'static str,
        rows: Vec<TzidEdge>,
    }

    /// Berlin's two 2026 transitions, each at its lower bound, inside and at
    /// its upper bound: the fall-back fold 02:00–03:00 on 10-25 and the
    /// spring-forward gap 02:00–03:00 on 03-29. And New York's: the fold
    /// 01:00–02:00 on 11-01 and the gap 02:00–03:00 on 03-08.
    fn edge_zones() -> [EdgeZone; 2] {
        let berlin = TwoOffsetZone::berlin_2026();
        let new_york = TwoOffsetZone::new_york_2026();
        let row = |model, kind, wall, rfc, libical| TzidEdge {
            kind,
            wall,
            rfc,
            libical,
            model,
        };
        [
            EdgeZone {
                vtimezone: BERLIN_VTIMEZONE,
                location: "Europe/Berlin",
                rows: vec![
                    row(
                        berlin,
                        "fold",
                        wall(2026, 10, 25, 2, 0),
                        utc(2026, 10, 25, 0, 0),
                        utc(2026, 10, 25, 1, 0),
                    ),
                    row(
                        berlin,
                        "fold",
                        wall(2026, 10, 25, 2, 30),
                        utc(2026, 10, 25, 0, 30),
                        utc(2026, 10, 25, 1, 30),
                    ),
                    row(
                        berlin,
                        "single",
                        wall(2026, 10, 25, 3, 0),
                        utc(2026, 10, 25, 2, 0),
                        utc(2026, 10, 25, 2, 0),
                    ),
                    row(
                        berlin,
                        "gap",
                        wall(2026, 3, 29, 2, 0),
                        utc(2026, 3, 29, 1, 0),
                        utc(2026, 3, 29, 0, 0),
                    ),
                    row(
                        berlin,
                        "gap",
                        wall(2026, 3, 29, 2, 30),
                        utc(2026, 3, 29, 1, 30),
                        utc(2026, 3, 29, 0, 30),
                    ),
                    row(
                        berlin,
                        "single",
                        wall(2026, 3, 29, 3, 0),
                        utc(2026, 3, 29, 1, 0),
                        utc(2026, 3, 29, 1, 0),
                    ),
                ],
            },
            EdgeZone {
                vtimezone: NEW_YORK_VTIMEZONE,
                location: "America/New_York",
                rows: vec![
                    row(
                        new_york,
                        "fold",
                        wall(2026, 11, 1, 1, 0),
                        utc(2026, 11, 1, 5, 0),
                        utc(2026, 11, 1, 6, 0),
                    ),
                    row(
                        new_york,
                        "fold",
                        wall(2026, 11, 1, 1, 30),
                        utc(2026, 11, 1, 5, 30),
                        utc(2026, 11, 1, 6, 30),
                    ),
                    row(
                        new_york,
                        "single",
                        wall(2026, 11, 1, 2, 0),
                        utc(2026, 11, 1, 7, 0),
                        utc(2026, 11, 1, 7, 0),
                    ),
                    row(
                        new_york,
                        "gap",
                        wall(2026, 3, 8, 2, 30),
                        utc(2026, 3, 8, 7, 30),
                        utc(2026, 3, 8, 6, 30),
                    ),
                ],
            },
        ]
    }

    /// The builtin `TZID` spellings of `location` libical resolves: the bare
    /// location (Thunderbird, most CalDAV servers) and libical's own prefixed
    /// form (Evolution).
    fn builtin_tzids(location: &str) -> [String; 2] {
        [
            location.to_owned(),
            format!("/freeassociation.sourceforge.net/{location}"),
        ]
    }

    /// A wall clock as an iCalendar DATE-TIME value, `YYYYMMDDTHHMMSS`.
    fn ical_wall(w: super::WallClock) -> String {
        format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}",
            w.year, w.month, w.day, w.hour, w.minute, w.second
        )
    }

    /// A VCALENDAR holding one event that starts at `w` in `tzid`, with
    /// `vtimezone` inline (empty for a builtin lookup).
    fn tzid_event(vtimezone: &str, tzid: &str, w: super::WallClock) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//hytte-ecal-test//\r\n{vtimezone}\
             BEGIN:VEVENT\r\nUID:tzid-edge\r\nDTSTAMP:20260101T000000Z\r\n\
             DTSTART;TZID={tzid}:{}\r\nSUMMARY:Edge\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
            ical_wall(w)
        )
    }

    /// libical's **own** instant for the `DTSTART` of the event in `ical`:
    /// `icaltime_as_timet_with_zone` with the zone the time resolved to, the
    /// call `ical_time_to_unix` made for every resolved `TZID` time before
    /// #1590. `None` when libical left the `TZID` unresolved, i.e. the time
    /// reads as floating.
    fn libical_own_instant(ical: &str) -> Option<i64> {
        let comp = super::parse_vevent(ical).unwrap();
        // SAFETY: `comp.raw` is the live VEVENT just parsed, owned by `comp`
        // until this function returns; the accessor hands back a new
        // `ICalTime` ref (or null), released below.
        let tt = unsafe { sys::i_cal_component_get_dtstart(comp.raw) };
        assert!(!tt.is_null(), "the fixture has a DTSTART");
        // SAFETY: `tt` is the live time just taken; the zone is a borrow it
        // holds, never unref'd here.
        let zone = unsafe { sys::i_cal_time_get_timezone(tt.cast_const()) };
        let unix = (!zone.is_null()).then(|| {
            // SAFETY: `tt` and `zone` are the live borrows above; the
            // conversion only reads them.
            unsafe { sys::i_cal_time_as_timet_with_zone(tt.cast_const(), zone.cast_const()) }
        });
        // SAFETY: the new ref taken above, released exactly once and never
        // read after.
        unsafe { sys::g_object_unref(tt) }
        unix
    }

    /// The start the expansion gives the one event in `ical`.
    fn expanded_start(ical: &str) -> i64 {
        let inst = super::expand_ical_for_test(ical, Y2026_START, Y2026_END).unwrap();
        assert_eq!(inst.len(), 1, "one event, one instance:\n{ical}");
        inst[0].start_unix
    }

    /// Fixture sanity for a row: the model zone reports the case the row is
    /// about, and #1585's floating-time rule on that model gives the row's
    /// RFC instant. That rule is the oracle the `TZID` path is held to.
    fn check_edge_fixture(e: TzidEdge) {
        assert_eq!(mapping(e.model, e.wall), e.kind, "fixture: {:?}", e.wall);
        assert_eq!(
            e.wall.to_unix_in(&e.model),
            Some(e.rfc),
            "fixture: {:?}",
            e.wall
        );
        if e.kind == "single" {
            assert_eq!(e.libical, e.rfc, "fixture: {:?}", e.wall);
        } else {
            assert_ne!(e.libical, e.rfc, "fixture: {:?}", e.wall);
        }
    }

    /// The probe, kept as a test: what libical answers **on its own** for a
    /// resolved `TZID` time on a transition. It takes the later instant in a
    /// fold and the post-gap offset in a gap, on an inline `VTIMEZONE` and on
    /// the builtin table alike, which is why `ical_time_to_unix` no longer
    /// uses its answer. If a libical upgrade starts following RFC 5545 here,
    /// this test fails and says so; `IcalZone` is then redundant, though still
    /// correct.
    #[test]
    fn libical_alone_takes_the_later_instant_in_a_fold_and_the_post_gap_offset() {
        let mut builtin_rows = 0;
        for z in edge_zones() {
            for e in &z.rows {
                check_edge_fixture(*e);
                let inline = tzid_event(z.vtimezone, z.location, e.wall);
                assert_eq!(
                    libical_own_instant(&inline),
                    Some(e.libical),
                    "inline {} {:?}",
                    z.location,
                    e.wall
                );
                for tzid in builtin_tzids(z.location) {
                    let Some(got) = libical_own_instant(&tzid_event("", &tzid, e.wall)) else {
                        eprintln!("skipped: libical cannot resolve the builtin {tzid} here");
                        continue;
                    };
                    assert_eq!(got, e.libical, "builtin {tzid} {:?}", e.wall);
                    builtin_rows += 1;
                }
            }
        }
        eprintln!("builtin-zone rows checked: {builtin_rows}");
    }

    /// The fix: a `TZID` time on a transition expands to RFC 5545 §3.3.5's
    /// instant. In a fold, the first occurrence (Berlin 02:30 on 10-25 is
    /// 00:30Z, not libical's 01:30Z); in a gap, the offset before it (Berlin
    /// 02:30 on 03-29 is 01:30Z, not libical's 00:30Z); at an upper bound, the
    /// one instant that reads it.
    #[test]
    fn a_tzid_time_on_a_transition_resolves_by_rfc_5545() {
        for z in edge_zones() {
            for e in &z.rows {
                check_edge_fixture(*e);
                let ical = tzid_event(z.vtimezone, z.location, e.wall);
                assert_eq!(
                    expanded_start(&ical),
                    e.rfc,
                    "{} {:?} ({}; libical alone says {})",
                    z.location,
                    e.wall,
                    e.kind,
                    e.libical
                );
            }
        }
    }

    /// The same rows through libical's builtin zones, the shape EDS hands
    /// back: its components carry no `VTIMEZONE`, so a `TZID` resolves
    /// against the builtin table, by its bare location or by the prefixed
    /// form Evolution writes. Skipped (with a message) where libical finds no
    /// zoneinfo, as in the nix build sandbox.
    #[test]
    fn a_builtin_tzid_time_on_a_transition_resolves_by_rfc_5545() {
        let mut checked = 0;
        for z in edge_zones() {
            for e in &z.rows {
                for tzid in builtin_tzids(z.location) {
                    let ical = tzid_event("", &tzid, e.wall);
                    if libical_own_instant(&ical).is_none() {
                        eprintln!("skipped: libical cannot resolve the builtin {tzid} here");
                        continue;
                    }
                    assert_eq!(
                        expanded_start(&ical),
                        e.rfc,
                        "{tzid} {:?} ({}; libical alone says {})",
                        e.wall,
                        e.kind,
                        e.libical
                    );
                    checked += 1;
                }
            }
        }
        eprintln!("builtin-zone rows checked: {checked}");
    }

    /// `local_offsets` against the hermetic models' own UTC → offset maps: a
    /// fold answers both offsets (larger, i.e. earlier instant, first), a gap
    /// none, everything else one. A fold's upper bound answers one: unlike
    /// chrono's `Local` (`TwoOffsetZone::with_closed_folds`), only the offset
    /// that reads back counts. Samoa's skipped day is a 24-hour gap, the
    /// widest the one-day samples have to straddle.
    #[test]
    fn local_offsets_reads_a_zone_from_its_utc_offsets_alone() {
        // `MappedLocalTime` is an alias of this enum, and an alias cannot
        // import variants.
        use chrono::LocalResult::{Ambiguous, None as Gap, Single};

        let berlin = TwoOffsetZone::berlin_2026();
        let new_york = TwoOffsetZone::new_york_2026();
        let samoa = TwoOffsetZone::samoa_2011();
        for (zone, w, want) in [
            (berlin, wall(2026, 10, 25, 1, 59), Single(7_200)),
            (berlin, wall(2026, 10, 25, 2, 0), Ambiguous(7_200, 3_600)),
            (berlin, wall(2026, 10, 25, 2, 30), Ambiguous(7_200, 3_600)),
            (berlin, wall(2026, 10, 25, 3, 0), Single(3_600)),
            (berlin, wall(2026, 3, 29, 1, 59), Single(3_600)),
            (berlin, wall(2026, 3, 29, 2, 0), Gap),
            (berlin, wall(2026, 3, 29, 2, 30), Gap),
            (berlin, wall(2026, 3, 29, 3, 0), Single(7_200)),
            (berlin, wall(2026, 7, 1, 12, 0), Single(7_200)),
            (
                new_york,
                wall(2026, 11, 1, 1, 30),
                Ambiguous(-14_400, -18_000),
            ),
            (new_york, wall(2026, 11, 1, 2, 0), Single(-18_000)),
            (new_york, wall(2026, 3, 8, 2, 30), Gap),
            (samoa, wall(2011, 12, 29, 23, 30), Single(-36_000)),
            (samoa, wall(2011, 12, 30, 0, 0), Gap),
            (samoa, wall(2011, 12, 30, 23, 59), Gap),
            (samoa, wall(2011, 12, 31, 0, 0), Single(50_400)),
        ] {
            let got = super::local_offsets(naive_of(w).and_utc().timestamp(), |unix| {
                Some(zone.offset_at(unix))
            });
            assert_eq!(got, want, "{w:?}");
        }
        // A zone libical cannot sample at all has no instant for anything.
        assert_eq!(super::local_offsets(0, |_| None), Gap);
    }

    /// A `VTIMEZONE` whose offset chrono cannot hold (+25:00, which only a
    /// malformed one can spell) leaves the rule no instant. The event then
    /// keeps libical's own answer, as before #1590, rather than being dropped
    /// as undatable, and the out-of-range offset is never handed to chrono as
    /// some other offset.
    #[test]
    fn a_zone_chrono_cannot_hold_falls_back_to_libicals_answer() {
        const PLUS_25H: &str = "BEGIN:VTIMEZONE\r\nTZID:Test/Plus25\r\n\
             BEGIN:STANDARD\r\nTZOFFSETFROM:+2500\r\nTZOFFSETTO:+2500\r\nTZNAME:P25\r\n\
             DTSTART:19700101T000000\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\n";
        let ical = tzid_event(PLUS_25H, "Test/Plus25", wall(2026, 7, 1, 12, 0));
        let libical = libical_own_instant(&ical).expect("libical resolves the inline zone");
        assert_eq!(
            libical,
            utc(2026, 7, 1, 12, 0) - 25 * 3_600,
            "fixture: libical reads the zone as +25:00"
        );
        assert_eq!(expanded_start(&ical), libical);
    }

    /// An event from 02:30 to 03:30 on Berlin's fall-back day starts at the
    /// first 02:30 (00:30Z) and ends at 03:30 CET (02:30Z): two hours. Pins
    /// that `DTEND` takes the same conversion as `DTSTART`.
    #[test]
    fn an_event_across_a_fold_lasts_from_the_first_occurrence() {
        let ical = berlin_calendar_of(&["UID:fold-span\r\nDTSTAMP:20260101T000000Z\r\n\
             DTSTART;TZID=Europe/Berlin:20261025T023000\r\n\
             DTEND;TZID=Europe/Berlin:20261025T033000\r\nSUMMARY:Night shift\r\n"]);
        let inst = super::expand_calendar_for_test(&ical, Y2026_START, Y2026_END).unwrap();
        assert_eq!(inst.len(), 1);
        assert_eq!(inst[0].start_unix, utc(2026, 10, 25, 0, 30));
        assert_eq!(inst[0].end_unix, utc(2026, 10, 25, 2, 30));
    }

    /// A daily 02:30 Berlin series across both 2026 transitions. The
    /// occurrences the recurrence iterator lands in the gap and in the fold
    /// take the same instants a single event does, since every occurrence
    /// goes through `ical_time_to_unix`. Three shapes: one `COUNT` series per
    /// transition (stepped from `DTSTART`, since libical will not fast-forward
    /// a `COUNT` rule), and an unbounded series begun in 2020, which
    /// `skip_iterator_to_window` (#1195) re-anchors on the window first.
    ///
    /// The spring rows stop at the gap day: the occurrence after it is the
    /// iterator's own problem, not the conversion's (see
    /// `libical_iterator_carries_a_gap_days_shift_into_the_next_occurrence`).
    #[test]
    fn a_zoned_daily_series_takes_the_same_instants_on_transition_days() {
        let spring = berlin_calendar_of(&["UID:spring\r\nDTSTAMP:20260101T000000Z\r\n\
             DTSTART;TZID=Europe/Berlin:20260328T023000\r\n\
             DTEND;TZID=Europe/Berlin:20260328T033000\r\n\
             RRULE:FREQ=DAILY;COUNT=2\r\nSUMMARY:Spring\r\n"]);
        let spring_want = vec![
            utc(2026, 3, 28, 1, 30), // 02:30 CET
            utc(2026, 3, 29, 1, 30), // in the gap: 03:30 CEST
        ];
        let inst = super::expand_calendar_for_test(&spring, Y2026_START, Y2026_END).unwrap();
        assert_eq!(sorted_starts(&inst), spring_want);
        assert!(inst.iter().all(|e| e.end_unix - e.start_unix == 3_600));

        let autumn = berlin_calendar_of(&["UID:autumn\r\nDTSTAMP:20260101T000000Z\r\n\
             DTSTART;TZID=Europe/Berlin:20261024T023000\r\n\
             RRULE:FREQ=DAILY;COUNT=3\r\nSUMMARY:Autumn\r\n"]);
        let autumn_want = vec![
            utc(2026, 10, 24, 0, 30), // 02:30 CEST
            utc(2026, 10, 25, 0, 30), // in the fold: the first 02:30, CEST
            utc(2026, 10, 26, 1, 30), // 02:30 CET
        ];
        let inst = super::expand_calendar_for_test(&autumn, Y2026_START, Y2026_END).unwrap();
        assert_eq!(sorted_starts(&inst), autumn_want);

        let since_2020 = berlin_calendar_of(&["UID:old\r\nDTSTAMP:20200101T000000Z\r\n\
             DTSTART;TZID=Europe/Berlin:20200101T023000\r\n\
             RRULE:FREQ=DAILY\r\nSUMMARY:Old\r\n"]);
        let spring_window = (utc(2026, 3, 28, 0, 0), utc(2026, 3, 30, 0, 0));
        let inst =
            super::expand_calendar_for_test(&since_2020, spring_window.0, spring_window.1).unwrap();
        assert_eq!(sorted_starts(&inst), spring_want, "unbounded, spring");
        let autumn_window = (utc(2026, 10, 24, 0, 0), utc(2026, 10, 27, 0, 0));
        let inst =
            super::expand_calendar_for_test(&since_2020, autumn_window.0, autumn_window.1).unwrap();
        assert_eq!(sorted_starts(&inst), autumn_want, "unbounded, autumn");
    }

    /// An `EXDATE` naming the fold occurrence and a detached instance whose
    /// `RECURRENCE-ID` names the gap one match the occurrences they name.
    ///
    /// The gap half failed before #1590: libical's iterator hands the gap
    /// day's occurrence back as 03:30 (ICU moves the missing 02:30 forward),
    /// which libical reads as 01:30Z, while the `RECURRENCE-ID`'s 02:30 went
    /// through libical's gap answer, 00:30Z. The two never matched, so the
    /// moved occurrence showed twice. The fold half held either way (both
    /// sides read the same 02:30 the same way); it is here to catch a fix that
    /// reaches the occurrences but not `EXDATE`.
    #[test]
    fn an_exdate_or_recurrence_id_on_a_transition_still_names_its_occurrence() {
        let ical = berlin_calendar_of(&[
            "UID:daily\r\nDTSTAMP:20260101T000000Z\r\n\
             DTSTART;TZID=Europe/Berlin:20260101T023000\r\n\
             RRULE:FREQ=DAILY\r\n\
             EXDATE;TZID=Europe/Berlin:20261025T023000\r\nSUMMARY:Daily\r\n",
            "UID:daily\r\nDTSTAMP:20260101T000000Z\r\n\
             RECURRENCE-ID;TZID=Europe/Berlin:20260329T023000\r\n\
             DTSTART;TZID=Europe/Berlin:20260329T120000\r\nSUMMARY:Daily (moved)\r\n",
        ]);
        let spring =
            super::expand_calendar_for_test(&ical, utc(2026, 3, 28, 0, 0), utc(2026, 3, 30, 0, 0))
                .unwrap();
        assert_eq!(
            sorted_starts(&spring),
            vec![
                utc(2026, 3, 28, 1, 30),
                utc(2026, 3, 29, 10, 0), // the detached instance, 12:00 CEST
            ],
            "the gap occurrence is the one the detached instance replaces"
        );
        let autumn = super::expand_calendar_for_test(
            &ical,
            utc(2026, 10, 24, 0, 0),
            utc(2026, 10, 27, 0, 0),
        )
        .unwrap();
        assert_eq!(
            sorted_starts(&autumn),
            vec![utc(2026, 10, 24, 0, 30), utc(2026, 10, 26, 1, 30)],
            "the fold occurrence is the one EXDATE cancels"
        );
    }

    /// The wall clocks libical's recurrence iterator hands back for the first
    /// `n` occurrences of the one RRULE'd event in `ical`, before any
    /// conversion.
    fn libical_iterator_walls(ical: &str, n: usize) -> Vec<super::WallClock> {
        let comp = super::parse_vevent(ical).unwrap();
        // SAFETY: `comp.raw` is the live VEVENT just parsed, owned by `comp`
        // until this function returns; a new `ICalTime` ref (or null),
        // released below after the iterator that borrows it.
        let dtstart = unsafe { sys::i_cal_component_get_dtstart(comp.raw) };
        assert!(!dtstart.is_null(), "the fixture has a DTSTART");
        // SAFETY: the same live component, and libical's RRULE
        // `ICalPropertyKind` discriminant; a new property ref (or null),
        // released below.
        let prop =
            unsafe { sys::i_cal_component_get_first_property(comp.raw, sys::I_CAL_RRULE_PROPERTY) };
        assert!(!prop.is_null(), "the fixture has an RRULE");
        // SAFETY: `prop` is that non-null property; a new `ICalRecurrence`
        // ref, released below after the iterator built from it.
        let rule = unsafe { sys::i_cal_property_get_rrule(prop) };
        assert!(!rule.is_null(), "the fixture's RRULE parses");
        // SAFETY: `rule` and `dtstart` are live, and stay alive until after
        // the iterator is released below, as libical requires.
        let iter = unsafe { sys::i_cal_recur_iterator_new(rule, dtstart) };
        assert!(!iter.is_null(), "the iterator constructs");
        let walls = (0..n)
            .map(|_| {
                // SAFETY: `iter` is the live iterator above; each step
                // returns a new `ICalTime` ref, released below.
                let occ = unsafe { sys::i_cal_recur_iterator_next(iter) };
                assert!(!occ.is_null(), "the series is unbounded");
                // SAFETY: `occ` is the non-null, live time just taken.
                let w = unsafe { super::WallClock::from_ical(occ) };
                // SAFETY: that new ref, released exactly once.
                unsafe { sys::g_object_unref(occ) }
                w
            })
            .collect();
        // SAFETY: the iterator's one ref, released exactly once and never
        // stepped after, before the rule and start it borrows.
        unsafe { sys::g_object_unref(iter) }
        // SAFETY: the `ICalRecurrence` ref taken above, released exactly once.
        unsafe { sys::g_object_unref(rule) }
        // SAFETY: the property ref taken above, released exactly once.
        unsafe { sys::g_object_unref(prop) }
        // SAFETY: the `ICalTime` ref taken above, released exactly once, after
        // the iterator that borrowed it.
        unsafe { sys::g_object_unref(dtstart) }
        walls
    }

    /// Found while checking #1590's recurrence step, **not fixed here**:
    /// libical's recurrence iterator (built with ICU on this tree, so it
    /// steps a series on an ICU calendar in the `TZID`'s location) moves a
    /// gap day's missing 02:30 to 03:30, which is the right instant (RFC 5545
    /// §3.3.5 reads 02:30 there as 03:30 CEST). It then **keeps** 03:30 for
    /// the next occurrence: it sets the series' hour on the previous
    /// occurrence's date and then adds the period, and on the gap day that
    /// hour falls in the gap again (`icalrecur.c`'s `next_unit` sets the
    /// hour, `increment_monthday` is `ucal_add`). So a daily 02:30 Berlin
    /// series has its 2026-03-30 occurrence at 03:30 CEST (01:30Z) where the
    /// RFC has 02:30 CEST (00:30Z). Weekly and yearly series carry it the same
    /// way. The expansion converts whatever wall clock the iterator hands
    /// back, so it shows that occurrence an hour late.
    ///
    /// Evolution expands with this same iterator (EDS's `e-cal-recur.c`), so
    /// a `RECURRENCE-ID` or `EXDATE` it writes for that occurrence says 03:30
    /// as well. Replacing the iterator's time of day with `DTSTART`'s would
    /// fix the instant and break those matches, so the choice is left to the
    /// issue. This pins the iterator's behaviour, so a libical upgrade that
    /// changes it shows up here.
    #[test]
    fn libical_iterator_carries_a_gap_days_shift_into_the_next_occurrence() {
        let event = |dtstart: &str, rrule: &str| {
            berlin_calendar_of(&[&format!(
                "UID:carry\r\nDTSTAMP:20250101T000000Z\r\n\
                 DTSTART;TZID=Europe/Berlin:{dtstart}\r\nRRULE:{rrule}\r\nSUMMARY:Carry\r\n"
            )])
        };
        assert_eq!(
            libical_iterator_walls(&event("20260328T023000", "FREQ=DAILY"), 4),
            vec![
                wall(2026, 3, 28, 2, 30),
                wall(2026, 3, 29, 3, 30), // the gap day: the right instant
                wall(2026, 3, 30, 3, 30), // the carry: the RFC has 02:30
                wall(2026, 3, 31, 2, 30),
            ],
            "daily"
        );
        assert_eq!(
            libical_iterator_walls(&event("20260322T023000", "FREQ=WEEKLY"), 4),
            vec![
                wall(2026, 3, 22, 2, 30),
                wall(2026, 3, 29, 3, 30),
                wall(2026, 4, 5, 3, 30),
                wall(2026, 4, 12, 2, 30),
            ],
            "weekly"
        );
        assert_eq!(
            libical_iterator_walls(&event("20250329T023000", "FREQ=YEARLY"), 3),
            vec![
                wall(2025, 3, 29, 2, 30),
                wall(2026, 3, 29, 3, 30),
                wall(2027, 3, 29, 3, 30), // a Monday; 2027's gap is on the 28th
            ],
            "yearly"
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

    // ── Detached recurrence instances (RECURRENCE-ID) ─────────────────────
    //
    // A recurring event with one occurrence edited on its own comes back from
    // EDS as two components with one UID: the master, and a detached instance
    // carrying `RECURRENCE-ID`. `expand_calendar_for_test` expands every
    // VEVENT of a VCALENDAR together, through the same `expand_components`
    // `generate_instances` runs over what EDS hands back, so these drive the
    // production reconciliation — hermetically, from iCalendar text, with no
    // EDS. Fixtures carry the Berlin `VTIMEZONE` inline (the shape a synced
    // CalDAV calendar delivers) so the zoned recurrence ids resolve without
    // host zoneinfo.
    //
    // The series: "Weekly sync", Tuesdays 10:00–11:00 Berlin (08:00Z in
    // CEST) from 2026-06-02 — so June's occurrences are the 2nd, 9th, 16th,
    // 23rd and 30th.

    /// The weekly master every fixture below shares.
    const WEEKLY_MASTER: &str = "UID:weekly-1\r\nDTSTAMP:20260501T000000Z\r\n\
         DTSTART;TZID=Europe/Berlin:20260602T100000\r\n\
         DTEND;TZID=Europe/Berlin:20260602T110000\r\n\
         RRULE:FREQ=WEEKLY\r\nSUMMARY:Weekly sync\r\n";

    /// The usual edit: the 2026-06-09 occurrence moved to Wednesday the 10th,
    /// 11:00–12:00 Berlin, with its own title.
    const WEEKLY_MOVED_JUN9: &str = "UID:weekly-1\r\nDTSTAMP:20260601T000000Z\r\n\
         RECURRENCE-ID;TZID=Europe/Berlin:20260609T100000\r\n\
         DTSTART;TZID=Europe/Berlin:20260610T110000\r\n\
         DTEND;TZID=Europe/Berlin:20260610T120000\r\n\
         SEQUENCE:1\r\nSUMMARY:Weekly sync (moved)\r\n";

    /// A VCALENDAR with the inline Berlin `VTIMEZONE` and one VEVENT per body,
    /// in the order given.
    fn berlin_calendar_of(vevent_bodies: &[&str]) -> String {
        let mut ical = format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//hytte-ecal-test//\r\n{BERLIN_VTIMEZONE}"
        );
        for body in vevent_bodies {
            ical.push_str("BEGIN:VEVENT\r\n");
            ical.push_str(body);
            ical.push_str("END:VEVENT\r\n");
        }
        ical.push_str("END:VCALENDAR\r\n");
        ical
    }

    /// The instances' starts, sorted (emission order is per component, and
    /// these tests are about the set the calendar shows).
    fn sorted_starts(instances: &[super::EventInstance]) -> Vec<i64> {
        let mut starts: Vec<i64> = instances.iter().map(|e| e.start_unix).collect();
        starts.sort_unstable();
        starts
    }

    /// June's five Tuesdays at 08:00Z — the master alone.
    fn june_tuesdays() -> Vec<i64> {
        [2, 9, 16, 23, 30]
            .into_iter()
            .map(|day| utc(2026, 6, day, 8, 0))
            .collect()
    }

    /// Control: expanded on its own, the master is June's five Tuesdays — so
    /// anything the tests below remove is the detached instance's doing.
    #[test]
    fn the_weekly_master_alone_is_junes_five_tuesdays() {
        let ical = berlin_calendar_of(&[WEEKLY_MASTER]);
        let inst = super::expand_calendar_for_test(&ical, JUN_START, JUL_START).unwrap();
        assert_eq!(sorted_starts(&inst), june_tuesdays());
    }

    /// The finding, as a test: a weekly meeting moved from Tuesday 10:00 to
    /// Wednesday 11:00 must show **once**, at the new time. Before the fix the
    /// master and the detached instance were each expanded on their own, so
    /// the calendar showed it on both days — six instances, Jun 9 among them.
    #[test]
    fn a_moved_occurrence_shows_once_at_its_new_time() {
        let ical = berlin_calendar_of(&[WEEKLY_MASTER, WEEKLY_MOVED_JUN9]);
        let inst = super::expand_calendar_for_test(&ical, JUN_START, JUL_START).unwrap();

        let jun9 = utc(2026, 6, 9, 8, 0);
        let moved = utc(2026, 6, 10, 9, 0); // 11:00 CEST
        let mut expected: Vec<i64> = june_tuesdays().into_iter().filter(|&s| s != jun9).collect();
        expected.push(moved);
        expected.sort_unstable();
        assert_eq!(
            sorted_starts(&inst),
            expected,
            "the Jun 9 occurrence must be replaced by the Jun 10 one, not joined by it",
        );

        let shown = inst.iter().find(|e| e.start_unix == moved).unwrap();
        assert_eq!(
            shown.end_unix,
            moved + 3_600,
            "the detached instance's own DTEND"
        );
        assert!(
            shown.ical.contains("Weekly sync (moved)") && shown.ical.contains("RECURRENCE-ID"),
            "the moved occurrence carries the detached component, not the master",
        );
        assert!(
            inst.iter()
                .filter(|e| e.start_unix != moved)
                .all(|e| !e.ical.contains("RECURRENCE-ID")),
            "every other occurrence is the master's",
        );
    }

    /// EDS hands components back in no particular order; a detached instance
    /// listed before its master must reconcile the same way.
    #[test]
    fn a_detached_instance_listed_before_its_master_reconciles_the_same() {
        let master_first = berlin_calendar_of(&[WEEKLY_MASTER, WEEKLY_MOVED_JUN9]);
        let detached_first = berlin_calendar_of(&[WEEKLY_MOVED_JUN9, WEEKLY_MASTER]);
        let a = super::expand_calendar_for_test(&master_first, JUN_START, JUL_START).unwrap();
        let b = super::expand_calendar_for_test(&detached_first, JUN_START, JUL_START).unwrap();
        assert_eq!(sorted_starts(&a), sorted_starts(&b));
        assert_eq!(b.len(), 5);
    }

    /// The occurrence the override names is inside the window but it was moved
    /// out of it: the window shows neither — not the old time (suppressed) and
    /// not the new one (outside).
    #[test]
    fn an_occurrence_moved_out_of_the_window_leaves_it() {
        let moved_out = "UID:weekly-1\r\nDTSTAMP:20260601T000000Z\r\n\
             RECURRENCE-ID;TZID=Europe/Berlin:20260630T100000\r\n\
             DTSTART;TZID=Europe/Berlin:20260702T100000\r\n\
             DTEND;TZID=Europe/Berlin:20260702T110000\r\nSUMMARY:Weekly sync (July)\r\n";
        let ical = berlin_calendar_of(&[WEEKLY_MASTER, moved_out]);

        let june = super::expand_calendar_for_test(&ical, JUN_START, JUL_START).unwrap();
        assert_eq!(
            sorted_starts(&june),
            june_tuesdays()[..4],
            "Jun 30 left June; nothing replaces it inside the window",
        );

        // …and the window it was moved into does show it, once.
        let jul_8 = utc(2026, 7, 8, 0, 0);
        let early_july = super::expand_calendar_for_test(&ical, JUL_START, jul_8).unwrap();
        assert_eq!(
            sorted_starts(&early_july),
            vec![utc(2026, 7, 2, 8, 0), utc(2026, 7, 7, 8, 0)],
            "Jul 2 (the moved one) and the master's Jul 7",
        );
    }

    /// The other direction: the occurrence the override names is *outside* the
    /// window (the first Tuesday of July), but it was moved into it (Monday
    /// June 29). It shows in June once; July no longer has it.
    #[test]
    fn an_occurrence_moved_into_the_window_from_outside_appears_once() {
        let moved_in = "UID:weekly-1\r\nDTSTAMP:20260601T000000Z\r\n\
             RECURRENCE-ID;TZID=Europe/Berlin:20260707T100000\r\n\
             DTSTART;TZID=Europe/Berlin:20260629T150000\r\n\
             DTEND;TZID=Europe/Berlin:20260629T160000\r\nSUMMARY:Weekly sync (early)\r\n";
        let ical = berlin_calendar_of(&[WEEKLY_MASTER, moved_in]);

        let june = super::expand_calendar_for_test(&ical, JUN_START, JUL_START).unwrap();
        let mut expected = june_tuesdays();
        expected.push(utc(2026, 6, 29, 13, 0));
        expected.sort_unstable();
        assert_eq!(sorted_starts(&june), expected);

        let jul_8 = utc(2026, 7, 8, 0, 0);
        let early_july = super::expand_calendar_for_test(&ical, JUL_START, jul_8).unwrap();
        assert!(
            early_july.is_empty(),
            "Jul 7 was moved into June, so early July shows nothing — got {:?}",
            sorted_starts(&early_july),
        );
    }

    /// The recurrence id is matched as an **instant**, not as text: a server
    /// that writes it in UTC (`…Z`) for a master zoned in `Europe/Berlin` —
    /// common in the wild — still names the same occurrence.
    #[test]
    fn a_utc_recurrence_id_matches_a_zoned_master_occurrence() {
        let utc_rid = "UID:weekly-1\r\nDTSTAMP:20260601T000000Z\r\n\
             RECURRENCE-ID:20260609T080000Z\r\n\
             DTSTART;TZID=Europe/Berlin:20260610T110000\r\n\
             DTEND;TZID=Europe/Berlin:20260610T120000\r\nSUMMARY:Weekly sync (moved)\r\n";
        let ical = berlin_calendar_of(&[WEEKLY_MASTER, utc_rid]);
        let inst = super::expand_calendar_for_test(&ical, JUN_START, JUL_START).unwrap();
        assert_eq!(
            inst.len(),
            5,
            "replaced, not duplicated: {:?}",
            sorted_starts(&inst)
        );
        assert!(!sorted_starts(&inst).contains(&utc(2026, 6, 9, 8, 0)));
    }

    /// One occurrence cancelled the way some servers (Google among them) say
    /// it — a detached instance with `STATUS:CANCELLED` rather than an
    /// `EXDATE` on the master. The master's occurrence is suppressed and the
    /// one instance left at that time carries the cancelled component, which
    /// `hytte-services`' calendar drops; before the fix the master's copy, not
    /// cancelled, stayed on screen.
    #[test]
    fn a_cancelled_detached_instance_replaces_the_occurrence_it_cancels() {
        let cancelled = "UID:weekly-1\r\nDTSTAMP:20260601T000000Z\r\n\
             RECURRENCE-ID;TZID=Europe/Berlin:20260609T100000\r\n\
             DTSTART;TZID=Europe/Berlin:20260609T100000\r\n\
             DTEND;TZID=Europe/Berlin:20260609T110000\r\n\
             STATUS:CANCELLED\r\nSUMMARY:Weekly sync\r\n";
        let ical = berlin_calendar_of(&[WEEKLY_MASTER, cancelled]);
        let inst = super::expand_calendar_for_test(&ical, JUN_START, JUL_START).unwrap();

        let jun9 = utc(2026, 6, 9, 8, 0);
        let at_jun9: Vec<_> = inst.iter().filter(|e| e.start_unix == jun9).collect();
        assert_eq!(at_jun9.len(), 1, "one instance at Jun 9, not two");
        assert!(
            at_jun9[0].ical.contains("STATUS:CANCELLED"),
            "the instance left at Jun 9 is the cancelled one",
        );
        assert_eq!(sorted_starts(&inst), june_tuesdays());
    }

    /// A detached instance is one occurrence by definition. Some clients copy
    /// the master's RRULE onto it anyway; it must not grow a second series.
    #[test]
    fn a_detached_instance_carrying_an_rrule_is_not_expanded() {
        let with_rrule = "UID:weekly-1\r\nDTSTAMP:20260601T000000Z\r\n\
             RECURRENCE-ID;TZID=Europe/Berlin:20260609T100000\r\n\
             DTSTART;TZID=Europe/Berlin:20260610T110000\r\n\
             DTEND;TZID=Europe/Berlin:20260610T120000\r\n\
             RRULE:FREQ=WEEKLY\r\nSUMMARY:Weekly sync (moved)\r\n";
        let ical = berlin_calendar_of(&[WEEKLY_MASTER, with_rrule]);
        let inst = super::expand_calendar_for_test(&ical, JUN_START, JUL_START).unwrap();
        let detached = inst
            .iter()
            .filter(|e| e.ical.contains("RECURRENCE-ID"))
            .count();
        assert_eq!(detached, 1, "the detached instance shows once, unexpanded");
        assert_eq!(inst.len(), 5);
    }

    /// A detached instance whose master is not in the list (deleted, or not
    /// synced yet) still shows — EDS does the same — and suppresses nothing.
    #[test]
    fn a_detached_instance_without_its_master_still_shows() {
        let ical = berlin_calendar_of(&[WEEKLY_MOVED_JUN9]);
        let inst = super::expand_calendar_for_test(&ical, JUN_START, JUL_START).unwrap();
        assert_eq!(sorted_starts(&inst), vec![utc(2026, 6, 10, 9, 0)]);
    }

    /// The other series in the same calendar are untouched: a detached
    /// instance only ever suppresses an occurrence of its own `UID`.
    #[test]
    fn a_detached_instance_only_overrides_its_own_series() {
        let other = WEEKLY_MASTER
            .replace("UID:weekly-1", "UID:weekly-2")
            .replace("Weekly sync", "Other sync");
        let ical = berlin_calendar_of(&[WEEKLY_MASTER, &other, WEEKLY_MOVED_JUN9]);
        let inst = super::expand_calendar_for_test(&ical, JUN_START, JUL_START).unwrap();
        let other_starts: Vec<i64> = sorted_starts(
            &inst
                .iter()
                .filter(|e| e.ical.contains("UID:weekly-2"))
                .cloned()
                .collect::<Vec<_>>(),
        );
        assert_eq!(other_starts, june_tuesdays(), "weekly-2 keeps its Jun 9");
        assert_eq!(inst.len(), 10);
    }

    /// All-day series: a `VALUE=DATE` recurrence id matches the DATE
    /// occurrence it names (both anchor to midnight UTC), and the moved day
    /// keeps the all-day flag.
    #[test]
    fn an_all_day_occurrence_moved_to_another_day_shows_once() {
        let master = "UID:allday-1\r\nDTSTAMP:20260501T000000Z\r\n\
             DTSTART;VALUE=DATE:20260601\r\nDTEND;VALUE=DATE:20260602\r\n\
             RRULE:FREQ=DAILY;COUNT=5\r\nSUMMARY:Holiday\r\n";
        let moved = "UID:allday-1\r\nDTSTAMP:20260501T000000Z\r\n\
             RECURRENCE-ID;VALUE=DATE:20260603\r\n\
             DTSTART;VALUE=DATE:20260610\r\nDTEND;VALUE=DATE:20260611\r\n\
             SUMMARY:Holiday (moved)\r\n";
        let ical = berlin_calendar_of(&[master, moved]);
        let inst = super::expand_calendar_for_test(&ical, JUN_START, JUL_START).unwrap();
        let day = |d: u32| utc(2026, 6, d, 0, 0);
        assert_eq!(
            sorted_starts(&inst),
            vec![day(1), day(2), day(4), day(5), day(10)]
        );
        assert!(inst.iter().all(|e| e.all_day));
    }

    // RANGE=THISANDFUTURE: the detached instance also reschedules every later
    // occurrence (RFC 5545 §3.8.4.4). Rare in the wild — EDS, Thunderbird and
    // Nextcloud split the series instead — but when one arrives, the later
    // occurrences must not stay at their old time.

    /// From Jun 16 on, the meeting moves to Wednesdays 11:00–12:30: the
    /// override itself shows at its own time, and every later occurrence is
    /// moved by the same +25 h, with its 90-minute duration and its title.
    #[test]
    fn this_and_future_reschedules_every_later_occurrence() {
        let from_jun16 = "UID:weekly-1\r\nDTSTAMP:20260601T000000Z\r\n\
             RECURRENCE-ID;RANGE=THISANDFUTURE;TZID=Europe/Berlin:20260616T100000\r\n\
             DTSTART;TZID=Europe/Berlin:20260617T110000\r\n\
             DTEND;TZID=Europe/Berlin:20260617T123000\r\nSUMMARY:Weekly sync (new slot)\r\n";
        let ical = berlin_calendar_of(&[WEEKLY_MASTER, from_jun16]);
        let inst = super::expand_calendar_for_test(&ical, JUN_START, JUL_START).unwrap();

        // Jun 2 and 9 as before; Jun 17 and 24 in the new slot; the master's
        // Jun 30 moves to Jul 1, out of June.
        let wed = |d: u32| utc(2026, 6, d, 9, 0);
        assert_eq!(
            sorted_starts(&inst),
            vec![
                utc(2026, 6, 2, 8, 0),
                utc(2026, 6, 9, 8, 0),
                wed(17),
                wed(24)
            ],
        );
        let jun24 = inst.iter().find(|e| e.start_unix == wed(24)).unwrap();
        assert_eq!(
            jun24.end_unix - jun24.start_unix,
            5_400,
            "the override's duration"
        );
        assert!(
            jun24.ical.contains("Weekly sync (new slot)"),
            "and its properties"
        );
        let jun9 = inst
            .iter()
            .find(|e| e.start_unix == utc(2026, 6, 9, 8, 0))
            .unwrap();
        assert!(
            !jun9.ical.contains("new slot"),
            "occurrences before the override are the master's",
        );
    }

    /// A single-instance override *after* a `THISANDFUTURE` one is its own
    /// component and wins for its occurrence ("subsequent instances defined in
    /// separate components are not impacted").
    #[test]
    fn a_later_single_override_wins_over_this_and_future() {
        let from_jun16 = "UID:weekly-1\r\nDTSTAMP:20260601T000000Z\r\n\
             RECURRENCE-ID;RANGE=THISANDFUTURE;TZID=Europe/Berlin:20260616T100000\r\n\
             DTSTART;TZID=Europe/Berlin:20260617T110000\r\n\
             DTEND;TZID=Europe/Berlin:20260617T123000\r\nSUMMARY:Weekly sync (new slot)\r\n";
        let jun23_alone = "UID:weekly-1\r\nDTSTAMP:20260601T000000Z\r\n\
             RECURRENCE-ID;TZID=Europe/Berlin:20260623T100000\r\n\
             DTSTART;TZID=Europe/Berlin:20260626T090000\r\n\
             DTEND;TZID=Europe/Berlin:20260626T100000\r\nSUMMARY:Weekly sync (Friday)\r\n";
        let ical = berlin_calendar_of(&[WEEKLY_MASTER, from_jun16, jun23_alone]);
        let inst = super::expand_calendar_for_test(&ical, JUN_START, JUL_START).unwrap();
        assert_eq!(
            sorted_starts(&inst),
            vec![
                utc(2026, 6, 2, 8, 0),
                utc(2026, 6, 9, 8, 0),
                utc(2026, 6, 17, 9, 0),
                utc(2026, 6, 26, 7, 0),
            ],
            "Jun 23 is the Friday override, not the THISANDFUTURE Wednesday",
        );
    }

    /// An occurrence a `THISANDFUTURE` override moves *earlier* can come from
    /// past the window's end: the master's Jun 30 occurrence, moved a day
    /// back, belongs to a window that ends before Jun 30 starts. The iterator
    /// is run past the window end by the shift so it is still found.
    #[test]
    fn this_and_future_moved_earlier_pulls_an_occurrence_in_from_after_the_window() {
        let a_day_earlier = "UID:weekly-1\r\nDTSTAMP:20260601T000000Z\r\n\
             RECURRENCE-ID;RANGE=THISANDFUTURE;TZID=Europe/Berlin:20260616T100000\r\n\
             DTSTART;TZID=Europe/Berlin:20260615T100000\r\n\
             DTEND;TZID=Europe/Berlin:20260615T110000\r\nSUMMARY:Weekly sync (Mondays)\r\n";
        let ical = berlin_calendar_of(&[WEEKLY_MASTER, a_day_earlier]);
        let mon_jun29 = utc(2026, 6, 29, 0, 0);
        let tue_jun30 = utc(2026, 6, 30, 0, 0);
        let inst = super::expand_calendar_for_test(&ical, mon_jun29, tue_jun30).unwrap();
        assert_eq!(
            sorted_starts(&inst),
            vec![utc(2026, 6, 29, 8, 0)],
            "Jun 30's occurrence, rescheduled to Monday Jun 29",
        );
    }

    /// The other direction, through the #1195 skip: a series begun in January
    /// whose occurrences a `THISANDFUTURE` override moved five days *later*.
    /// The Sunday-June-28 occurrence's recurrence id is Tuesday the 23rd —
    /// five days before the window, past `SKIP_BACKOFF_SECS`, so the iterator
    /// must be re-anchored that much earlier for it to be generated at all.
    #[test]
    fn this_and_future_moved_later_reaches_back_past_the_skip_backoff() {
        let master = "UID:weekly-jan\r\nDTSTAMP:20260101T000000Z\r\n\
             DTSTART:20260106T080000Z\r\nDTEND:20260106T090000Z\r\n\
             RRULE:FREQ=WEEKLY\r\nSUMMARY:Since January\r\n";
        let five_days_later = "UID:weekly-jan\r\nDTSTAMP:20260101T000000Z\r\n\
             RECURRENCE-ID;RANGE=THISANDFUTURE:20260113T080000Z\r\n\
             DTSTART:20260118T080000Z\r\nDTEND:20260118T090000Z\r\n\
             SUMMARY:Since January (Sundays)\r\n";
        // Fixture sanity, checked at compile time: the shift must exceed the
        // skip's own backoff, or the backoff alone would find the occurrence.
        const { assert!(5 * 86_400 > super::SKIP_BACKOFF_SECS) };
        let ical = berlin_calendar_of(&[master, five_days_later]);
        let sun_jun28 = utc(2026, 6, 28, 0, 0);
        let mon_jun29 = utc(2026, 6, 29, 0, 0);
        let inst = super::expand_calendar_for_test(&ical, sun_jun28, mon_jun29).unwrap();
        assert_eq!(sorted_starts(&inst), vec![utc(2026, 6, 28, 8, 0)]);
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

    /// #1590 reads every zoned occurrence through `IcalZone`, which asks
    /// libical for an offset three times per ordinary occurrence (a sample a
    /// day either side and the one check), each through a freshly built
    /// `ICalTime`. That is the new per-occurrence cost on the zoned path, so
    /// the #1179 shape is pinned on it too: a `TZID=Europe/Berlin`
    /// `FREQ=MINUTELY` series over the same 43-day window, capped, in order,
    /// and well inside the same loose bound.
    #[test]
    fn a_zoned_minutely_series_expands_in_bounded_work() {
        use std::time::{Duration, Instant};

        let window_end = JUN_START + 43 * 86_400;
        let ical = berlin_calendar_of(&["UID:zoned-minutely\r\nDTSTAMP:20260601T090000Z\r\n\
             DTSTART;TZID=Europe/Berlin:20260601T110000\r\n\
             DTEND;TZID=Europe/Berlin:20260601T110500\r\n\
             SUMMARY:Tick\r\nRRULE:FREQ=MINUTELY\r\n"]);

        let started = Instant::now();
        let inst = super::expand_calendar_for_test(&ical, JUN_START, window_end).unwrap();
        let elapsed = started.elapsed();

        assert_eq!(inst.len(), super::MAX_OCCURRENCES_PER_COMPONENT);
        // 11:00 CEST is 09:00Z; one occurrence a minute from there.
        for (i, e) in inst.iter().enumerate() {
            let minute = i64::try_from(i).unwrap();
            assert_eq!(e.start_unix, ANCHOR_0900 + minute * 60, "occurrence {i}");
        }
        eprintln!("zoned minutely expansion: {elapsed:?}");
        assert!(
            elapsed < Duration::from_secs(5),
            "zoned expansion took {elapsed:?} for {} occurrences",
            inst.len()
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
        use chrono::{TimeZone as _, Utc};

        let ical = "BEGIN:VEVENT\r\nUID:minutely-i7\r\nDTSTAMP:20260101T090300Z\r\n\
                     DTSTART:20260101T090300Z\r\nDTEND:20260101T090800Z\r\n\
                     SUMMARY:Every 7 minutes\r\nRRULE:FREQ=MINUTELY;INTERVAL=7\r\n\
                     END:VEVENT\r\n";
        let inst = super::expand_ical_for_test(ical, JUN_START, WINDOW_43D_END).unwrap();

        assert!(
            !inst.is_empty(),
            "fixture must contribute occurrences to the window",
        );
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
    ///
    /// "Rebuilt" is read off a weak pointer on the original iterator rather
    /// than by comparing addresses: now that the original is really released
    /// (`g_object_unref`, not the leaking `i_cal_recur_iterator_free`), the
    /// allocator is free to hand its block straight back to the replacement.
    #[test]
    fn libical_refuses_to_skip_a_count_rule_and_accepts_one_without() {
        /// Build the iterator `expand_component` builds for a given RRULE and
        /// report what `skip_iterator_to_window` says about skipping it to
        /// June 2026, plus whether it rebuilt the iterator (#1206 MEDIUM-2).
        fn iterator_skips(recurrence_rule: &str) -> (bool, bool) {
            let fixture = RecurFixture::new(recurrence_rule);
            let mut iter = fixture.iterator();
            let mut original: *mut c_void = ptr::null_mut();
            // SAFETY: `iter` is the live iterator just built, and `original`
            // is a local of this function, which releases every iterator
            // before it returns.
            unsafe { watch_finalization(iter, &raw mut original) }

            // SAFETY: `iter` is the live iterator just built and not yet
            // stepped, and `fixture.rule`/`fixture.dtstart` are the same live
            // values it was built from — exactly this callee's contract.
            let skipped = unsafe {
                super::skip_iterator_to_window(
                    &mut iter,
                    fixture.rule,
                    fixture.dtstart,
                    0,
                    JUN_START,
                )
            };
            // The original was finalized iff the skip released it and built a
            // replacement.
            let rebuilt = original.is_null();

            let mut current: *mut c_void = ptr::null_mut();
            // SAFETY: `iter` is the live iterator this scope now owns (the
            // original or its replacement), and `current` outlives it.
            unsafe { watch_finalization(iter, &raw mut current) }
            // SAFETY: that iterator's one ref, released exactly once and never
            // stepped after; `fixture` (its rule and start) drops after this.
            unsafe { sys::g_object_unref(iter) }
            assert!(
                current.is_null() && original.is_null(),
                "releasing the iterator did not finalize it — it leaked",
            );
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
             caller happened to be holding — and must release the original",
        );
    }

    // ── Releasing the recurrence iterator ─────────────────────────────────
    //
    // An `ICalRecurIterator` is a GObject wrapper around libical's native
    // iterator, and the wrapper's finalizer is what frees the native half.
    // These pin that the crate's release (`g_object_unref`) finalizes it, and
    // that the libical-glib "destructor" it used before does not.

    /// The libical objects `expand_component` builds a recurrence iterator
    /// from — the parsed VEVENT, its `DTSTART`, its RRULE property and the
    /// parsed rule — for the tests that drive the iterator directly. Every
    /// iterator built from it must be released before it drops: an iterator
    /// borrows `rule` and `dtstart` for its whole life.
    struct RecurFixture {
        // Never read: held so the VEVENT outlives the values read out of it.
        _comp: super::Component,
        dtstart: *mut sys::ICalTime,
        prop: *mut sys::ICalProperty,
        rule: *mut sys::ICalRecurrence,
    }

    impl RecurFixture {
        /// A 2014-01-01T09:00Z series with the given `RRULE` value.
        fn new(recurrence_rule: &str) -> Self {
            let ical = format!(
                "BEGIN:VEVENT\r\nUID:probe\r\nDTSTAMP:20140101T090000Z\r\n\
                 DTSTART:20140101T090000Z\r\nDTEND:20140101T093000Z\r\n\
                 SUMMARY:Probe\r\nRRULE:{recurrence_rule}\r\nEND:VEVENT\r\n"
            );
            let comp = super::parse_vevent(&ical).unwrap();
            // SAFETY: `comp.raw` is the live VEVENT just parsed, owned by the
            // fixture; the accessor returns a **new** `ICalTime` ref, released
            // in `Drop`.
            let dtstart = unsafe { sys::i_cal_component_get_dtstart(comp.raw) };
            // SAFETY: the same live component and libical's RRULE
            // `ICalPropertyKind` discriminant; a **new** property ref (or
            // null), released in `Drop`.
            let prop = unsafe {
                sys::i_cal_component_get_first_property(comp.raw, sys::I_CAL_RRULE_PROPERTY)
            };
            assert!(!prop.is_null(), "fixture has an RRULE");
            // SAFETY: `prop` is that non-null property; reading its value
            // yields a **new** `ICalRecurrence` ref, released in `Drop`.
            let rule = unsafe { sys::i_cal_property_get_rrule(prop) };
            assert!(!rule.is_null(), "fixture's RRULE parses");
            Self {
                _comp: comp,
                dtstart,
                prop,
                rule,
            }
        }

        /// A fresh iterator over the fixture's rule — one owned GObject ref,
        /// which the caller releases before the fixture drops.
        fn iterator(&self) -> *mut sys::ICalRecurIterator {
            // SAFETY: `rule` and `dtstart` are the fixture's live values and
            // stay alive until it drops — after every iterator built here is
            // released, which is what libical requires of them.
            let iter = unsafe { sys::i_cal_recur_iterator_new(self.rule, self.dtstart) };
            assert!(!iter.is_null(), "iterator constructs");
            iter
        }
    }

    impl Drop for RecurFixture {
        fn drop(&mut self) {
            // SAFETY: the new `ICalRecurrence` ref taken in `new`, released
            // exactly once, after every iterator built from it.
            unsafe { sys::g_object_unref(self.rule) }
            // SAFETY: the new property ref taken in `new`, released exactly
            // once.
            unsafe { sys::g_object_unref(self.prop) }
            // SAFETY: the new `ICalTime` ref taken in `new`, released exactly
            // once, after every iterator that borrowed it. `_comp` drops after
            // this body.
            unsafe { sys::g_object_unref(self.dtstart) }
        }
    }

    /// Watch `obj` for finalization: GLib nulls `*slot` when the object is
    /// finalized (`g_object_add_weak_pointer`).
    ///
    /// # Safety
    ///
    /// `obj` must be a live GObject, and `slot` must point at a location that
    /// outlives it — or that is unregistered with
    /// `g_object_remove_weak_pointer` while the object is still alive.
    unsafe fn watch_finalization(obj: *mut c_void, slot: *mut *mut c_void) {
        // SAFETY: `slot` is valid for writes (this function's contract); it is
        // seeded with the object's address, which GLib nulls on finalize.
        unsafe { slot.write(obj) }
        // SAFETY: `obj` is live and `slot` outlives it, both by this
        // function's contract.
        unsafe { sys::g_object_add_weak_pointer(obj, slot) }
    }

    /// The release every iterator in this crate goes through: one
    /// `g_object_unref` finalizes the `ICalRecurIterator` wrapper, whose
    /// finalizer frees the native `icalrecur_iterator`. Before this the crate
    /// called `i_cal_recur_iterator_free` instead, which frees only the native
    /// half and leaked the wrapper once per recurring component per calendar
    /// refresh — see the next test.
    #[test]
    fn releasing_a_recurrence_iterator_finalizes_it() {
        let fixture = RecurFixture::new("FREQ=DAILY");
        let iter = fixture.iterator();
        // Step it once, as `expand_component` does, so the release is of an
        // iterator that has done real work.
        // SAFETY: `iter` is the live iterator just built; the step returns a
        // **new** `ICalTime` ref, released straight after.
        let occ = unsafe { sys::i_cal_recur_iterator_next(iter) };
        assert!(!occ.is_null(), "the daily rule has a first occurrence");
        // SAFETY: that new ref, released exactly once.
        unsafe { sys::g_object_unref(occ) }

        let mut alive: *mut c_void = ptr::null_mut();
        // SAFETY: `iter` is live and `alive` outlives it (released below).
        unsafe { watch_finalization(iter, &raw mut alive) }
        // SAFETY: the iterator's one ref, released exactly once; `fixture`
        // drops after this.
        unsafe { sys::g_object_unref(iter) }
        assert!(
            alive.is_null(),
            "g_object_unref left the iterator alive: something else holds a ref",
        );
    }

    /// The external fact behind the previous test, pinned rather than quoted
    /// from libical-glib's generator: `i_cal_recur_iterator_free` frees the
    /// native `icalrecur_iterator` and **leaves the GObject wrapper alive** —
    /// its whole body is `icalrecur_iterator_free(i_cal_object_get_native(it))`
    /// (`api/i-cal-recur-iterator.xml`, a `kind="destructor"` stub the GIR
    /// skips), so it neither drops the ref nor clears the native pointer, and a
    /// later `g_object_unref` would free the native iterator a second time.
    /// That is why `sys` does not bind it and this test declares it locally.
    ///
    /// The wrapper this test frees is leaked on purpose — releasing it would be
    /// that double free. If a future libical makes `_free` drop the ref too,
    /// this goes red; the crate's `g_object_unref` is still correct then, and
    /// `sys::ICalRecurIterator`'s doc is what needs updating.
    #[test]
    fn i_cal_recur_iterator_free_frees_the_native_iterator_but_leaks_the_wrapper() {
        #[link(name = "ical-glib")]
        unsafe extern "C" {
            fn i_cal_recur_iterator_free(iter: *mut sys::ICalRecurIterator);
        }

        let fixture = RecurFixture::new("FREQ=DAILY");
        let iter = fixture.iterator();
        let mut alive: *mut c_void = ptr::null_mut();
        // SAFETY: `iter` is live; `alive` is unregistered below while the
        // wrapper is still alive, or nulled by GLib if it was finalized.
        unsafe { watch_finalization(iter, &raw mut alive) }
        // SAFETY: `iter` is the live iterator just built, passed to the
        // libical-glib destructor exactly once and never used as an iterator
        // again (its native half is gone after this).
        unsafe { i_cal_recur_iterator_free(iter) }
        let wrapper_survived = !alive.is_null();
        if wrapper_survived {
            // SAFETY: the wrapper is still a live GObject (it was not
            // finalized — that is what `alive` still being set means), and
            // unregistering touches only its weak-ref data, never the freed
            // native iterator. Nothing references `alive` after this.
            unsafe { sys::g_object_remove_weak_pointer(iter, &raw mut alive) }
        }
        assert!(
            wrapper_survived,
            "i_cal_recur_iterator_free now finalizes the wrapper too; the \
             crate's g_object_unref release is still right, but the \
             `sys::ICalRecurIterator` doc's account of libical is stale",
        );
    }
}
