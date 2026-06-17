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

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;

use anyhow::{Context as _, Result, anyhow, bail};

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
}

impl Drop for CalClient {
    fn drop(&mut self) {
        unsafe { sys::g_object_unref(self.raw) }
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
}
