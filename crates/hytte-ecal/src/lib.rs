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

#![doc(test(no_crate_inject))]

pub mod sys;

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;

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
    pub ical: String,
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
            node = unsafe { (*node).next };
        }
        // Free the spine only — calling `g_list_free` here is the standard
        // pattern when ownership of the elements is transferred elsewhere.
        // We don't have a binding for `g_list_free` directly, so reach into
        // sys via the destroy-notify-free path with a no-op destroyer.
        unsafe { sys::g_list_free_full(list, no_op_destroy) }
        out
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
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
        unsafe { borrowed_cstr(sys::e_source_get_uid(self.raw)) }.unwrap_or_default()
    }

    /// Human-readable name from the `DisplayName=` key. Localised
    /// variants are ignored; only the untagged value is returned.
    pub fn display_name(&self) -> String {
        unsafe { borrowed_cstr(sys::e_source_get_display_name(self.raw)) }.unwrap_or_default()
    }

    /// True iff this source carries the named extension (e.g.
    /// `"Task List"` for task sources).
    #[must_use]
    pub fn has_extension(&self, extension_name: &str) -> bool {
        let Ok(c) = CString::new(extension_name) else {
            return false;
        };
        let r = unsafe { sys::e_source_has_extension(self.raw, c.as_ptr()) };
        r != 0
    }

    pub(crate) fn raw(&self) -> *mut sys::ESource {
        self.raw
    }
}

impl Drop for Source {
    fn drop(&mut self) {
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
        let s = unsafe { CStr::from_ptr(out_uid) }
            .to_string_lossy()
            .into_owned();
        unsafe { sys::g_free(out_uid.cast::<c_void>()) }
        Ok(s)
    }

    /// Replace an existing object. `ical` must include the same UID as
    /// the object on the server. Non-recurring tasks pass
    /// [`sys::ECalObjModType::All`] for the mod-type.
    pub fn modify_from_ical(&self, ical: &str) -> Result<()> {
        let comp = parse_component(ical)?;
        let mut err: *mut sys::GError = ptr::null_mut();
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
                let domain = unsafe { (*err).domain };
                let code = unsafe { (*err).code };
                let not_found_domain = unsafe { sys::e_cal_client_error_quark() };
                if domain == not_found_domain && code == sys::E_CAL_CLIENT_ERROR_OBJECT_NOT_FOUND {
                    unsafe { sys::g_error_free(err) }
                    return Ok(None);
                }
            }
            return Err(take_error(err).unwrap_or_else(|| anyhow!("get_object_sync failed")));
        }
        if out.is_null() {
            return Ok(None);
        }
        let s_ptr = unsafe { sys::i_cal_component_as_ical_string(out) };
        let s = if s_ptr.is_null() {
            String::new()
        } else {
            let s = unsafe { CStr::from_ptr(s_ptr) }
                .to_string_lossy()
                .into_owned();
            unsafe { sys::g_free(s_ptr.cast::<c_void>()) }
            s
        };
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
            let data = unsafe { (*node).data }.cast::<sys::ICalComponent>();
            if !data.is_null() {
                let s_ptr = unsafe { sys::i_cal_component_as_ical_string(data) };
                if !s_ptr.is_null() {
                    let s = unsafe { CStr::from_ptr(s_ptr) }
                        .to_string_lossy()
                        .into_owned();
                    unsafe { sys::g_free(s_ptr.cast::<c_void>()) }
                    out.push(s);
                }
            }
            node = unsafe { (*node).next };
        }
        // Free the list AND each ICalComponent — list_sync passes
        // ownership of every element to the caller.
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
    /// The window is the only bound on expansion: a `FREQ=DAILY` series with
    /// no `UNTIL`/`COUNT` is naturally capped by the range you pass, never
    /// expanded unboundedly.
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
            let comp = unsafe { (*node).data }.cast::<sys::ICalComponent>();
            if !comp.is_null() {
                unsafe { expand_component(comp, start_unix, end_unix, &mut out) }
            }
            node = unsafe { (*node).next };
        }

        // Free the list AND each ICalComponent — list_sync transferred
        // ownership of every element to us.
        if !out_list.is_null() {
            unsafe { sys::g_slist_free_full(out_list, sys::g_object_unref_destroy_notify) }
        }
        Ok(out)
    }
}

/// Expand one master `comp` over `[start_unix, end_unix)` (POSIX UTC
/// seconds), pushing each occurrence into `out`.
///
/// - **Non-recurring** (no RRULE, no RDATE): emit a single [`EventInstance`]
///   if its DTSTART falls before `end_unix` (the calendar service does the
///   has-it-ended filtering).
/// - **Recurring** (RRULE present): drive libical's core recurrence iterator
///   (`i_cal_recur_iterator_new` / `_next`) from DTSTART, emitting one
///   instance per occurrence inside `[start_unix, end_unix)` and stopping
///   once occurrences pass `end_unix` (so an unbounded series is window-
///   capped). Per-occurrence duration is `DTEND − DTSTART` (or 0 if absent;
///   the service fabricates a UI duration).
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
    let dtstart = unsafe { sys::i_cal_component_get_dtstart(comp) };
    let Some(dtstart_unix) = (unsafe { ical_time_to_unix(dtstart) }) else {
        if !dtstart.is_null() {
            unsafe { sys::g_object_unref(dtstart) }
        }
        return;
    };
    let all_day = unsafe { ical_time_is_date(dtstart) };

    // Duration = DTEND − DTSTART when DTEND is present; else 0.
    let dtend = unsafe { sys::i_cal_component_get_dtend(comp) };
    let duration = match unsafe { ical_time_to_unix(dtend) } {
        Some(e) if e >= dtstart_unix => e - dtstart_unix,
        _ => 0,
    };
    if !dtend.is_null() {
        unsafe { sys::g_object_unref(dtend) }
    }

    // The component's iCal serialisation (metadata: UID/SUMMARY/LOCATION/…),
    // identical across a series' occurrences.
    let ical = unsafe { component_ical_string(comp) };

    // Recurrence-set modifiers, normalised to UTC seconds the same way every
    // occurrence is, so comparisons are apples-to-apples regardless of DATE
    // vs DATE-TIME / TZID. EXDATE is a membership set; RDATE a list of extra
    // starts.
    let exdates = unsafe { collect_property_times(comp, sys::I_CAL_EXDATE_PROPERTY, false) };
    let rdates = unsafe { collect_property_times(comp, sys::I_CAL_RDATE_PROPERTY, true) };

    // `emitted` tracks occurrence starts we've already pushed, so RDATE
    // doesn't double up one the RRULE (or DTSTART) already produced.
    let mut emitted: Vec<i64> = Vec::new();
    let mut emit = |out: &mut Vec<EventInstance>, occ_unix: i64| {
        // EXDATE excludes; the window bounds the rest. An occurrence is kept
        // when it starts before the window end and its end is at/after the
        // window start (so it overlaps the window).
        if exdates.contains(&occ_unix) {
            return;
        }
        if occ_unix >= end_unix || occ_unix + duration < start_unix {
            return;
        }
        if emitted.contains(&occ_unix) {
            return;
        }
        emitted.push(occ_unix);
        out.push(EventInstance {
            ical: ical.clone(),
            start_unix: occ_unix,
            end_unix: occ_unix + duration,
            all_day,
        });
    };

    // RRULE present?
    let rrule_prop =
        unsafe { sys::i_cal_component_get_first_property(comp, sys::I_CAL_RRULE_PROPERTY) };
    if rrule_prop.is_null() {
        // No RRULE: DTSTART is the (sole) base occurrence; RDATE may add more.
        emit(out, dtstart_unix);
    } else {
        // Recurring: iterate occurrences from DTSTART.
        let rule = unsafe { sys::i_cal_property_get_rrule(rrule_prop) };
        if !rule.is_null() {
            let iter = unsafe { sys::i_cal_recur_iterator_new(rule, dtstart) };
            if !iter.is_null() {
                // A defensive cap: even with the time-window stop condition, a
                // pathological rule shouldn't loop forever.
                let mut guard = 0u32;
                loop {
                    guard += 1;
                    if guard > 100_000 {
                        break;
                    }
                    let occ = unsafe { sys::i_cal_recur_iterator_next(iter) };
                    let Some(occ_unix) = (unsafe { ical_time_to_unix(occ) }) else {
                        // null-time ⇒ series exhausted.
                        if !occ.is_null() {
                            unsafe { sys::g_object_unref(occ) }
                        }
                        break;
                    };
                    if !occ.is_null() {
                        unsafe { sys::g_object_unref(occ) }
                    }
                    if occ_unix >= end_unix {
                        break; // past the window ⇒ done
                    }
                    emit(out, occ_unix);
                }
                unsafe { sys::i_cal_recur_iterator_free(iter) }
            }
            unsafe { sys::g_object_unref(rule) }
        }
        unsafe { sys::g_object_unref(rrule_prop) }
    }

    // RDATE: extra one-off occurrences within the window, deduped against the
    // RRULE-expanded set and subject to the same EXDATE exclusion.
    for rd in rdates {
        emit(out, rd);
    }

    unsafe { sys::g_object_unref(dtstart) }
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
    let mut prop = unsafe { sys::i_cal_component_get_first_property(comp, kind) };
    while !prop.is_null() {
        let unix = if is_rdate {
            unsafe { rdate_property_to_unix(prop) }
        } else {
            let tt = unsafe { sys::i_cal_property_get_exdate(prop) };
            let u = unsafe { ical_time_to_unix(tt) };
            if !tt.is_null() {
                unsafe { sys::g_object_unref(tt) }
            }
            u
        };
        if let Some(u) = unix {
            times.push(u);
        }
        unsafe { sys::g_object_unref(prop) }
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
    let dtp = unsafe { sys::i_cal_property_get_rdate(prop) };
    if dtp.is_null() {
        return None;
    }
    // Date-time form first.
    let tt = unsafe { sys::i_cal_datetimeperiod_get_time(dtp) };
    let mut result = unsafe { ical_time_to_unix(tt) };
    if !tt.is_null() {
        unsafe { sys::g_object_unref(tt) }
    }
    // Period form: take its start.
    if result.is_none() {
        let period = unsafe { sys::i_cal_datetimeperiod_get_period(dtp) };
        if !period.is_null() {
            let start = unsafe { sys::i_cal_period_get_start(period) };
            result = unsafe { ical_time_to_unix(start) };
            if !start.is_null() {
                unsafe { sys::g_object_unref(start) }
            }
            unsafe { sys::g_object_unref(period) }
        }
    }
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
    let s_ptr = unsafe { sys::i_cal_component_as_ical_string(comp) };
    if s_ptr.is_null() {
        return String::new();
    }
    let s = unsafe { CStr::from_ptr(s_ptr) }
        .to_string_lossy()
        .into_owned();
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
    if unsafe { sys::i_cal_time_is_null_time(tt_const) } != 0 {
        return None;
    }

    let is_date = unsafe { sys::i_cal_time_is_date(tt_const) } != 0;
    let is_utc = unsafe { sys::i_cal_time_is_utc(tt_const) } != 0;
    let own_zone = unsafe { sys::i_cal_time_get_timezone(tt_const) };
    let has_own_zone = !own_zone.is_null();

    if is_date {
        // DATE anchors to midnight-UTC by design: the display side
        // (`calendar`'s `unix_to_local`) reinterprets the resulting
        // midnight-UTC `time_t` as a *local* calendar date, so this must stay
        // midnight-UTC or the day would drift. The zone argument is irrelevant
        // for a DATE (no time-of-day to shift) — pass the UTC singleton.
        let utc = unsafe { sys::i_cal_timezone_get_utc_timezone() };
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
        return Some(unsafe {
            sys::i_cal_time_as_timet_with_zone(tt_const, own_zone.cast_const())
        });
    }

    if is_utc {
        // A UTC-flagged time with no attached zone pointer (defensive: some
        // libical values carry the `is_utc` bit without a zone object). It is
        // already absolute — the UTC singleton is the correct source zone.
        let utc = unsafe { sys::i_cal_timezone_get_utc_timezone() };
        return Some(unsafe { sys::i_cal_time_as_timet_with_zone(tt_const, utc.cast_const()) });
    }

    // Genuinely floating DATE-TIME (no zone, not UTC): resolve its wall-clock
    // fields in the local zone rather than assuming UTC (issue #388). Also
    // covers a `TZID`'d time whose `VTIMEZONE` libical could not resolve (it
    // then reports the time as floating) — the local zone is the right
    // fallback for the viewer.
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
        Self {
            year: unsafe { sys::i_cal_time_get_year(c) },
            month: unsafe { sys::i_cal_time_get_month(c) },
            day: unsafe { sys::i_cal_time_get_day(c) },
            hour: unsafe { sys::i_cal_time_get_hour(c) },
            minute: unsafe { sys::i_cal_time_get_minute(c) },
            second: unsafe { sys::i_cal_time_get_second(c) },
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
        // strictly after the view is stopped + unref'd, so no in-flight
        // trampoline can read a freed pointer. Hence the connections use a
        // no-op destroy-notify; ownership is ours, not the closures'.
        let boxed: Box<Box<dyn Fn()>> = Box::new(Box::new(on_change));
        let user_data = (&raw const *boxed).cast::<c_void>().cast_mut();

        // Connect all three change signals to one trampoline. We don't track
        // the returned handler ids: teardown is `g_object_unref(view)` in
        // `CalClientView::drop`, which disconnects every handler on the object
        // automatically. A `0` id means a connect failed — log but continue,
        // since a partial subscription still beats none (and the safety-net
        // poll backstops anything missed).
        for sig in [c"objects-added", c"objects-modified", c"objects-removed"] {
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
        }

        // Begin notifications. `view-start` also replays the current contents
        // via `objects-added`, so the first refresh fires promptly without an
        // extra manual poll.
        let mut start_err: *mut sys::GError = ptr::null_mut();
        unsafe { sys::e_cal_client_view_start(view, &mut start_err) }
        if let Some(e) = take_error(start_err) {
            // Couldn't start — disconnect/free everything we just set up and
            // surface the error rather than returning a dead view.
            unsafe { sys::g_object_unref(view) }
            drop(boxed);
            return Err(e);
        }

        Ok(CalClientView {
            raw: view,
            _callback: boxed,
        })
    }
}

impl Drop for CalClient {
    fn drop(&mut self) {
        unsafe { sys::g_object_unref(self.raw) }
    }
}

// ── ECalClientView ───────────────────────────────────────────────────────────

/// A live push subscription to a [`CalClient`]'s objects (see
/// [`CalClient::watch`]). Holds the EDS view plus the boxed Rust callback the
/// signal handlers fire into. Notifications flow only while this is alive **and**
/// the owning thread keeps pumping the [`MainContext`] the view was created
/// under; dropping it stops the view, releases EDS's proxy, and finally frees
/// the callback.
pub struct CalClientView {
    raw: *mut sys::ECalClientView,
    // Kept alive (and dropped last, after the view is torn down) so the raw
    // user_data pointer the handlers hold stays valid for their whole life.
    _callback: Box<Box<dyn Fn()>>,
}

impl Drop for CalClientView {
    fn drop(&mut self) {
        // Stop first so EDS quits emitting, then unref. Both run on the view's
        // owning thread (the only place a `CalClientView` lives), so no
        // trampoline can be mid-flight against the callback we're about to
        // free when `_callback` drops right after this.
        let mut err: *mut sys::GError = ptr::null_mut();
        unsafe { sys::e_cal_client_view_stop(self.raw, &mut err) }
        if !err.is_null() {
            unsafe { sys::g_error_free(err) }
        }
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
/// `g_signal_connect_data` — a live `*const Box<dyn Fn()>` owned by the
/// [`CalClientView`] that is, by construction, still alive (it's torn down
/// strictly before that box is freed). `_view`/`_objects` are borrowed and not
/// touched.
unsafe extern "C" fn view_changed_trampoline(
    _view: *mut c_void,
    _objects: *mut sys::GSList,
    user_data: *mut c_void,
) {
    if user_data.is_null() {
        return;
    }
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
        let raw = unsafe { sys::g_main_context_new() };
        if raw.is_null() {
            return None;
        }
        unsafe { sys::g_main_context_push_thread_default(raw) }
        Some(Self { raw })
    }

    /// Run one iteration. With `block` true, sleeps until a source is ready
    /// (a view signal arrived) or a [`Waker::wake`] fires — fully event-driven,
    /// no busy spin. Returns true if a source was dispatched.
    pub fn iterate(&self, block: bool) -> bool {
        unsafe { sys::g_main_context_iteration(self.raw, sys::GBoolean::from(block)) != 0 }
    }

    /// A `Send`-able handle that can [`Waker::wake`] this context's blocking
    /// iteration from another thread (the only cross-thread `GMainContext`
    /// operation GLib sanctions). Holds its own ref, so it stays valid even if
    /// the `MainContext` is dropped first.
    #[must_use]
    pub fn waker(&self) -> Waker {
        let raw = unsafe { sys::g_main_context_ref(self.raw) };
        Waker { raw }
    }
}

impl Drop for MainContext {
    fn drop(&mut self) {
        unsafe { sys::g_main_context_pop_thread_default(self.raw) }
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
        unsafe { sys::g_main_context_wakeup(self.raw) }
    }
}

impl Drop for Waker {
    fn drop(&mut self) {
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
    let raw = unsafe { sys::i_cal_parser_parse_string(c.as_ptr()) };
    if raw.is_null() {
        bail!("libical: failed to parse iCalendar body");
    }
    let parsed = Component { raw };
    // `isa` returns a raw `c_int`; match it against the libical component
    // constants rather than transmuting into the 8-variant Rust enum
    // (libical may return any of ~28 kinds — an unlisted value read as a
    // `#[repr(C)]` enum would be UB).
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
    let raw = unsafe { sys::i_cal_parser_parse_string(c.as_ptr()) };
    if raw.is_null() {
        bail!("libical: failed to parse iCalendar body");
    }
    let parsed = Component { raw };
    if unsafe { sys::i_cal_component_isa(parsed.raw) } == sys::I_CAL_VEVENT_COMPONENT {
        return Ok(parsed);
    }
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
    let msg = unsafe {
        let ptr = (*err).message;
        if ptr.is_null() {
            String::from("(no GError message)")
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    };
    let domain = unsafe { (*err).domain };
    let code = unsafe { (*err).code };
    unsafe { sys::g_error_free(err) }
    Some(anyhow!("EDS error [domain={domain} code={code}]: {msg}"))
}

/// Convert a borrowed `const char*` returned by libecal/libedataserver into
/// an owned `String`. Returns `None` for a null pointer.
unsafe fn borrowed_cstr(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
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
}
