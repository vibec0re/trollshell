//! Raw `extern "C"` declarations for the libecal / libedataserver /
//! libical-glib subset we drive. Everything in here is `unsafe` to call;
//! the safe surface lives in `lib.rs`.
//!
//! All four libraries are linked via `build.rs` (pkg-config). Type
//! aliases use opaque `c_void` for GObject pointers — we don't need
//! field access, only pointer identity + the methods listed here.

use std::ffi::{c_char, c_int, c_uint, c_void};

// ── Opaque GObject types ─────────────────────────────────────────────────────

/// `ESourceRegistry *` — central source lookup. Created via
/// [`e_source_registry_new_sync`]; release with [`g_object_unref`].
pub type ESourceRegistry = c_void;

/// `ESource *` — handle to one configured account/source. Returned by
/// [`e_source_registry_list_sources`] and [`e_source_registry_ref_source`];
/// release each with [`g_object_unref`].
pub type ESource = c_void;

/// `EClient *` / `ECalClient *` — calendar/task/memo client. Connect
/// via [`e_cal_client_connect_sync`]; release with [`g_object_unref`].
pub type ECalClient = c_void;

/// `ICalComponent *` — parsed iCalendar component (VCALENDAR, VTODO,
/// VEVENT…). Some accessors return borrows, others must be unref'd via
/// [`g_object_unref`] — see each function's note.
pub type ICalComponent = c_void;

/// `GError *` — out-param for fallible operations. We always init it to
/// `null` and free it via [`g_error_free`] if a call sets it.
#[repr(C)]
pub struct GError {
    pub domain: u32,
    pub code: c_int,
    pub message: *mut c_char,
}

/// `GList *` of `ESource *`. We iterate via [`g_list_next`] and free the
/// list (NOT its elements — those have their own refcount) with
/// [`g_list_free_full`] passing [`g_object_unref`] as the destroyer.
pub type GList = c_void;

/// `GSList *` of `ICalComponent *`. Same iteration pattern as `GList`.
pub type GSList = c_void;

/// `GCancellable *` — pass null for "uncancellable". We don't expose
/// cancellation yet.
pub type GCancellable = c_void;

// ── Enum mirrors ─────────────────────────────────────────────────────────────

/// Mirror of `ECalClientSourceType` from `e-cal-enums.h`. Repr is C-int
/// because the C enum has no explicit `: type` annotation.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ECalClientSourceType {
    Events = 0,
    Tasks = 1,
    Memos = 2,
}

/// Mirror of `ECalObjModType`. Bitflags-ish but we only ever pass
/// `OnlyThis` for the simple non-recurring task case.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ECalObjModType {
    All = 0x07,
}

/// Mirror of `ECalOperationFlags`. We only use NONE.
pub const E_CAL_OPERATION_FLAG_NONE: c_uint = 0;

// ── libedataserver: ESourceRegistry + ESource ────────────────────────────────

#[link(name = "edataserver-1.2")]
unsafe extern "C" {
    pub fn e_source_registry_new_sync(
        cancellable: *mut GCancellable,
        error: *mut *mut GError,
    ) -> *mut ESourceRegistry;

    pub fn e_source_registry_list_sources(
        registry: *mut ESourceRegistry,
        extension_name: *const c_char,
    ) -> *mut GList;

    pub fn e_source_registry_ref_source(
        registry: *mut ESourceRegistry,
        uid: *const c_char,
    ) -> *mut ESource;

    pub fn e_source_get_uid(source: *mut ESource) -> *const c_char;
    pub fn e_source_get_display_name(source: *mut ESource) -> *const c_char;
    pub fn e_source_has_extension(source: *mut ESource, extension_name: *const c_char) -> c_int;
}

// ── libecal: ECalClient ─────────────────────────────────────────────────────

#[link(name = "ecal-2.0")]
unsafe extern "C" {
    pub fn e_cal_client_connect_sync(
        source: *mut ESource,
        source_type: ECalClientSourceType,
        wait_for_connected_seconds: u32,
        cancellable: *mut GCancellable,
        error: *mut *mut GError,
    ) -> *mut ECalClient;

    pub fn e_cal_client_create_object_sync(
        client: *mut ECalClient,
        icalcomp: *mut ICalComponent,
        opflags: c_uint,
        out_uid: *mut *mut c_char,
        cancellable: *mut GCancellable,
        error: *mut *mut GError,
    ) -> c_int;

    pub fn e_cal_client_modify_object_sync(
        client: *mut ECalClient,
        icalcomp: *mut ICalComponent,
        mod_type: ECalObjModType,
        opflags: c_uint,
        cancellable: *mut GCancellable,
        error: *mut *mut GError,
    ) -> c_int;

    pub fn e_cal_client_remove_object_sync(
        client: *mut ECalClient,
        uid: *const c_char,
        rid: *const c_char,
        mod_type: ECalObjModType,
        opflags: c_uint,
        cancellable: *mut GCancellable,
        error: *mut *mut GError,
    ) -> c_int;

    pub fn e_cal_client_get_object_list_sync(
        client: *mut ECalClient,
        sexp: *const c_char,
        out_objects: *mut *mut GSList,
        cancellable: *mut GCancellable,
        error: *mut *mut GError,
    ) -> c_int;

    pub fn e_cal_client_get_object_sync(
        client: *mut ECalClient,
        uid: *const c_char,
        rid: *const c_char,
        out_icalcomp: *mut *mut ICalComponent,
        cancellable: *mut GCancellable,
        error: *mut *mut GError,
    ) -> c_int;
}

// ── libical-glib: ICalComponent + parser ─────────────────────────────────────

/// Component-kind enum values from `icalenums.h`. Listed in source order
/// so the numeric values match the C definitions — DO NOT reorder. We
/// only carry the kinds we actually use.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ICalComponentKind {
    No = 0,
    Any = 1,
    XRoot = 2,
    XAttach = 3,
    Vevent = 4,
    Vtodo = 5,
    Vjournal = 6,
    Vcalendar = 7,
}

#[link(name = "ical-glib")]
unsafe extern "C" {
    pub fn i_cal_parser_parse_string(s: *const c_char) -> *mut ICalComponent;
    pub fn i_cal_component_as_ical_string(comp: *mut ICalComponent) -> *mut c_char;
    pub fn i_cal_component_get_uid(comp: *mut ICalComponent) -> *const c_char;
    pub fn i_cal_component_get_first_component(
        parent: *mut ICalComponent,
        kind: ICalComponentKind,
    ) -> *mut ICalComponent;
    pub fn i_cal_component_isa(comp: *mut ICalComponent) -> ICalComponentKind;
}

// ── glib / gobject ──────────────────────────────────────────────────────────

#[link(name = "gobject-2.0")]
unsafe extern "C" {
    pub fn g_object_unref(obj: *mut c_void);
}

#[link(name = "glib-2.0")]
unsafe extern "C" {
    pub fn g_error_free(err: *mut GError);
    pub fn g_free(mem: *mut c_void);

    pub fn g_list_length(list: *mut GList) -> c_uint;
    pub fn g_list_nth_data(list: *mut GList, n: c_uint) -> *mut c_void;
    pub fn g_list_free_full(list: *mut GList, free_func: unsafe extern "C" fn(*mut c_void));

    pub fn g_slist_length(list: *mut GSList) -> c_uint;
    pub fn g_slist_nth_data(list: *mut GSList, n: c_uint) -> *mut c_void;
    pub fn g_slist_free_full(list: *mut GSList, free_func: unsafe extern "C" fn(*mut c_void));
}

/// Trampoline so we can pass `g_object_unref` as a `GDestroyNotify`
/// (which `g_list_free_full` expects). The signature `extern "C" fn(*mut
/// c_void)` matches what GLib wants.
///
/// # Safety
///
/// Caller (GLib) must pass either null or a valid GObject pointer with
/// a ref the trampoline owns; passing a non-GObject pointer is UB.
pub unsafe extern "C" fn g_object_unref_destroy_notify(obj: *mut c_void) {
    if obj.is_null() {
        return;
    }
    unsafe { g_object_unref(obj) }
}
