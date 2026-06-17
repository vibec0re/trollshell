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

/// Doubly-linked `GList` node. Modelled as `#[repr(C)]` (matching glib's
/// `struct _GList { gpointer data; GList *next; GList *prev; }`) so we can
/// walk `node.next` once per element — O(n) — instead of repeatedly
/// re-walking from the head via `g_list_nth_data` (which is O(n²)).
/// `g_list_next` is a C macro, not an exported symbol, so it can't be
/// bound directly; the node struct is the supported alternative.
///
/// We only ever read `data`/`next` and never construct one ourselves, so
/// the `prev` field is faithfully mirrored for layout but otherwise unused.
#[repr(C)]
pub struct GList {
    pub data: *mut c_void,
    pub next: *mut GList,
    pub prev: *mut GList,
}

/// Singly-linked `GSList` node. Mirror of glib's
/// `struct _GSList { gpointer data; GSList *next; }` — same O(n) walk
/// rationale as [`GList`].
#[repr(C)]
pub struct GSList {
    pub data: *mut c_void,
    pub next: *mut GSList,
}

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
///
/// This enum is only ever **passed into** libical (e.g.
/// [`i_cal_component_get_first_component`]) with values we construct
/// ourselves, so the `#[repr(C)]` enum is sound there. It must NOT be
/// used to *receive* a kind from libical: `i_cal_component_isa` can
/// return any of ~28 libical kinds, and materialising an out-of-range
/// value as this 8-variant enum is undefined behaviour. For that path,
/// [`i_cal_component_isa`] returns a raw `c_int` matched against the
/// [`I_CAL_*_COMPONENT`] constants below.
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

/// `ICAL_VEVENT_COMPONENT` — the integer discriminant `i_cal_component_isa`
/// returns for a VEVENT. Kept in sync with [`ICalComponentKind::Vevent`].
pub const I_CAL_VEVENT_COMPONENT: c_int = 4;

/// `ICAL_VTODO_COMPONENT` — the integer discriminant for a VTODO. Kept in
/// sync with [`ICalComponentKind::Vtodo`].
pub const I_CAL_VTODO_COMPONENT: c_int = 5;

#[link(name = "ical-glib")]
unsafe extern "C" {
    pub fn i_cal_parser_parse_string(s: *const c_char) -> *mut ICalComponent;
    pub fn i_cal_component_as_ical_string(comp: *mut ICalComponent) -> *mut c_char;
    pub fn i_cal_component_get_uid(comp: *mut ICalComponent) -> *const c_char;
    pub fn i_cal_component_get_first_component(
        parent: *mut ICalComponent,
        kind: ICalComponentKind,
    ) -> *mut ICalComponent;
    /// Returns the component's kind as a raw `ICalComponentKind` C enum
    /// value. Declared as `c_int` (not the [`ICalComponentKind`] Rust
    /// enum) because libical may return any of ~28 kinds — only a subset
    /// of which we model — and reading an unlisted value as a too-small
    /// `#[repr(C)]` enum is undefined behaviour. Callers match the int
    /// against [`I_CAL_VEVENT_COMPONENT`] / [`I_CAL_VTODO_COMPONENT`].
    pub fn i_cal_component_isa(comp: *mut ICalComponent) -> c_int;
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

    // Iteration is done by walking `node.next` on the `#[repr(C)]`
    // [`GList`]/[`GSList`] structs (O(n)); only the spine-free helpers are
    // bound. `g_list_next`/`g_slist_next` are C macros, not symbols.
    pub fn g_list_free_full(list: *mut GList, free_func: unsafe extern "C" fn(*mut c_void));
    pub fn g_slist_free_full(list: *mut GSList, free_func: unsafe extern "C" fn(*mut c_void));
}

#[link(name = "ecal-2.0")]
unsafe extern "C" {
    /// `e_cal_client_error_quark()` — the runtime `GQuark` identifying the
    /// `E_CAL_CLIENT_ERROR` GError domain. Compared against `GError.domain`
    /// (a `GQuark`, i.e. `guint32`) to classify errors by domain+code
    /// instead of i18n-fragile message substrings. `G_GNUC_CONST`: the
    /// returned quark is stable for the process lifetime.
    pub fn e_cal_client_error_quark() -> u32;
}

/// `E_CAL_CLIENT_ERROR_OBJECT_NOT_FOUND` — the `ECalClientError` code (in
/// the [`e_cal_client_error_quark`] domain) EDS sets when a requested
/// object UID doesn't exist. Second variant of the C enum, which starts
/// at 0, so the value is 1.
pub const E_CAL_CLIENT_ERROR_OBJECT_NOT_FOUND: c_int = 1;

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
