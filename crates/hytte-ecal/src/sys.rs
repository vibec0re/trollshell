//! Raw `extern "C"` declarations for the libecal / libedataserver /
//! libical-glib subset we drive. Everything in here is `unsafe` to call;
//! the safe surface lives in `lib.rs`.
//!
//! All four libraries are linked via `build.rs` (pkg-config). Type
//! aliases use opaque `c_void` for GObject pointers — we don't need
//! field access, only pointer identity + the methods listed here.

use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void};

/// `time_t` — POSIX calendar seconds since the Unix epoch. On every target
/// we build for (Linux x86_64/aarch64, glibc/musl) this is a signed 64-bit
/// integer; we model it as `i64` to match. Used by libical's
/// [`i_cal_time_as_timet_with_zone`] / [`i_cal_time_new_from_timet_with_zone`].
pub type TimeT = i64;

/// `gboolean` — GLib's C-int boolean (`0` = FALSE, non-zero = TRUE). Used
/// for the recurrence callback's return value.
pub type GBoolean = c_int;

/// `gulong` — the handler id [`g_signal_connect_data`] returns. GLib defines
/// `gulong` as the C `unsigned long`; on every target we build for (LP64
/// Linux) that is 64-bit, modelled here as [`c_ulong`].
pub type GULong = c_ulong;

/// `GCallback` — the generic, signature-erased function pointer GLib's signal
/// machinery stores. [`g_signal_connect_data`] takes a `GCallback`; the real
/// per-signal trampoline is cast to this at the call site. Modelled as an
/// `extern "C"` fn so transmuting a concrete trampoline into it is sound.
pub type GCallback = unsafe extern "C" fn();

/// `GClosureNotify` — invoked by GLib when the closure backing a connected
/// handler is finalised (i.e. once the handler is disconnected / the emitting
/// object is destroyed). We use it to drop the boxed Rust callback that backed
/// the trampoline's `user_data`, so the closure owns its callback for exactly
/// its lifetime. `closure` is opaque to us.
pub type GClosureNotify = unsafe extern "C" fn(data: *mut c_void, closure: *mut GClosure);

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

/// `ICalTime *` — a libical broken-down time value: a component's DTSTART /
/// DTEND, or one occurrence from the recurrence iterator. Released with
/// [`g_object_unref`] for the ones the accessors/iterator hand us (they
/// return new refs).
pub type ICalTime = c_void;

/// `ICalTimezone *` — a libical timezone. We only ever use the process-wide
/// UTC singleton ([`i_cal_timezone_get_utc_timezone`]), which is owned by
/// libical and must never be unref'd.
pub type ICalTimezone = c_void;

/// `ICalProperty *` — one property of a component (e.g. an RRULE). Returned
/// borrowed-or-owned per accessor; we `g_object_unref` the ones we own.
pub type ICalProperty = c_void;

/// `ICalRecurrence *` — a parsed RRULE value. Released with [`g_object_unref`].
pub type ICalRecurrence = c_void;

/// `ICalRecurIterator *` — libical's core recurrence iterator. Created via
/// [`i_cal_recur_iterator_new`]; freed with [`i_cal_recur_iterator_free`].
pub type ICalRecurIterator = c_void;

/// `ECalClientView *` — a live, push-based query result. Created via
/// [`e_cal_client_get_view_sync`]; emits the `objects-added` /
/// `objects-modified` / `objects-removed` GObject signals as the backend
/// changes. Release with [`g_object_unref`].
pub type ECalClientView = c_void;

/// `GMainContext *` — a GLib event-loop context. We create a private one per
/// EDS worker thread, push it thread-default, and iterate it so the view's
/// signals dispatch on that thread. Released with [`g_main_context_unref`].
pub type GMainContext = c_void;

/// `GClosure *` — opaque; only ever received (never read) by a
/// [`GClosureNotify`] callback. We never touch its fields.
pub type GClosure = c_void;

/// `ICAL_RRULE_PROPERTY` — the `ICalPropertyKind` discriminant for an RRULE
/// property (value 73 in `icalderivedproperty.h`). Passed to
/// [`i_cal_component_get_first_property`].
pub const I_CAL_RRULE_PROPERTY: c_int = 73;

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

    /// Synchronously create a live [`ECalClientView`] for the S-expression
    /// `sexp` (e.g. `"#t"` for "everything"). The view is created in the
    /// stopped state; call [`e_cal_client_view_start`] to begin receiving the
    /// `objects-added`/`-modified`/`-removed` signals. The signals are
    /// delivered on the thread-default [`GMainContext`] in effect *at the time
    /// of this call*, so we push a private context thread-default first. On
    /// success the out-param holds a new ref (release with [`g_object_unref`]).
    pub fn e_cal_client_get_view_sync(
        client: *mut ECalClient,
        sexp: *const c_char,
        out_view: *mut *mut ECalClientView,
        cancellable: *mut GCancellable,
        error: *mut *mut GError,
    ) -> c_int;

    /// Start delivering change notifications on a view returned (stopped) by
    /// [`e_cal_client_get_view_sync`]. Also triggers the initial population:
    /// `objects-added` fires once with every object currently matching the
    /// query.
    pub fn e_cal_client_view_start(view: *mut ECalClientView, error: *mut *mut GError);

    /// Stop delivering notifications on a view. Best-effort on teardown — we
    /// call it before unref'ing the view so EDS drops its D-Bus-side proxy
    /// promptly.
    pub fn e_cal_client_view_stop(view: *mut ECalClientView, error: *mut *mut GError);
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

    /// Convert a broken-down [`ICalTime`] to POSIX `time_t` (UTC seconds),
    /// interpreting the value as being in `zone`. We always pass the UTC
    /// singleton so DATE-TIME instances normalise to absolute UTC; for
    /// floating/DATE values libical anchors them to that zone. Borrows
    /// `tt`/`zone`; allocates nothing.
    pub fn i_cal_time_as_timet_with_zone(tt: *const ICalTime, zone: *const ICalTimezone) -> TimeT;

    /// True iff the [`ICalTime`] is a DATE (no time-of-day) — i.e. the
    /// instance is all-day. Returns a [`GBoolean`].
    pub fn i_cal_time_is_date(tt: *const ICalTime) -> GBoolean;

    /// True iff the [`ICalTime`] is the libical "null time" sentinel — a
    /// guard before trusting the start/end the callback hands us.
    pub fn i_cal_time_is_null_time(tt: *const ICalTime) -> GBoolean;

    /// Construct a new [`ICalTime`] from POSIX `time_t` (UTC seconds) in
    /// `zone`. `is_date` non-zero makes it a DATE (no time-of-day). The
    /// returned object is owned by the caller — release with
    /// [`g_object_unref`].
    pub fn i_cal_time_new_from_timet_with_zone(
        v: TimeT,
        is_date: c_int,
        zone: *mut ICalTimezone,
    ) -> *mut ICalTime;

    /// The component's DTSTART as a new [`ICalTime`] (owned — unref). For a
    /// recurring master this is the series origin; we feed it to the
    /// recurrence iterator.
    pub fn i_cal_component_get_dtstart(comp: *mut ICalComponent) -> *mut ICalTime;

    /// The component's DTEND as a new [`ICalTime`] (owned — unref), or a
    /// null-time if absent. Used to derive per-occurrence duration.
    pub fn i_cal_component_get_dtend(comp: *mut ICalComponent) -> *mut ICalTime;

    /// First property of `kind` on the component (e.g.
    /// [`I_CAL_RRULE_PROPERTY`]) as a new [`ICalProperty`] (owned — unref),
    /// or null if the component has none.
    pub fn i_cal_component_get_first_property(
        comp: *mut ICalComponent,
        kind: c_int,
    ) -> *mut ICalProperty;

    /// The RRULE value of an RRULE [`ICalProperty`] as a new
    /// [`ICalRecurrence`] (owned — unref).
    pub fn i_cal_property_get_rrule(prop: *mut ICalProperty) -> *mut ICalRecurrence;

    /// Create a libical recurrence iterator for `rule` anchored at
    /// `dtstart`. Owned — release with [`i_cal_recur_iterator_free`].
    /// Borrows `rule`/`dtstart`.
    pub fn i_cal_recur_iterator_new(
        rule: *mut ICalRecurrence,
        dtstart: *mut ICalTime,
    ) -> *mut ICalRecurIterator;

    /// Advance the iterator and return the next occurrence as a new
    /// [`ICalTime`] (owned — unref). Returns a null-time
    /// ([`i_cal_time_is_null_time`]) when the series is exhausted.
    pub fn i_cal_recur_iterator_next(iter: *mut ICalRecurIterator) -> *mut ICalTime;

    /// Free a recurrence iterator created by [`i_cal_recur_iterator_new`].
    pub fn i_cal_recur_iterator_free(iter: *mut ICalRecurIterator);
}

#[link(name = "ical-glib")]
unsafe extern "C" {
    /// The process-wide UTC [`ICalTimezone`] singleton. Owned by libical —
    /// never unref it. Passed to [`i_cal_time_as_timet_with_zone`].
    pub fn i_cal_timezone_get_utc_timezone() -> *mut ICalTimezone;
}

// ── glib / gobject ──────────────────────────────────────────────────────────

#[link(name = "gobject-2.0")]
unsafe extern "C" {
    pub fn g_object_unref(obj: *mut c_void);

    /// `g_signal_connect_data` — connect `c_handler` to the named signal on
    /// `instance`, threading `data` (our boxed Rust callback) through to every
    /// invocation and to `destroy_data` (called once, when the closure is
    /// finalised, so we can free `data`). Returns the handler id (a
    /// [`GULong`]); `0` means the connection failed. `connect_flags` of `0`
    /// (`G_CONNECT_DEFAULT`) is the "call before the default handler, no swap"
    /// behaviour we want. The real per-signal handler is cast to the
    /// signature-erased [`GCallback`] at the call site.
    pub fn g_signal_connect_data(
        instance: *mut c_void,
        detailed_signal: *const c_char,
        c_handler: GCallback,
        data: *mut c_void,
        destroy_data: GClosureNotify,
        connect_flags: c_uint,
    ) -> GULong;
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

    /// Create a fresh, private [`GMainContext`]. Owned — release with
    /// [`g_main_context_unref`].
    pub fn g_main_context_new() -> *mut GMainContext;

    /// Take an additional ref on a [`GMainContext`]. Thread-safe — used by the
    /// `Send`-able waker so it can outlive the owning `MainContext`.
    pub fn g_main_context_ref(context: *mut GMainContext) -> *mut GMainContext;

    /// Drop a ref taken via [`g_main_context_new`] / [`g_main_context_ref`].
    pub fn g_main_context_unref(context: *mut GMainContext);

    /// Make `context` the thread-default for the calling thread, so GObject
    /// signal sources (like the EDS view's) attach to it rather than the
    /// global default. Balanced by [`g_main_context_pop_thread_default`].
    pub fn g_main_context_push_thread_default(context: *mut GMainContext);

    /// Undo a [`g_main_context_push_thread_default`].
    pub fn g_main_context_pop_thread_default(context: *mut GMainContext);

    /// Run a single iteration of `context`. With `may_block` non-zero this
    /// blocks until a source is ready (a view signal arrives) or
    /// [`g_main_context_wakeup`] is called from another thread. Returns
    /// non-zero if any source was dispatched.
    pub fn g_main_context_iteration(context: *mut GMainContext, may_block: GBoolean) -> GBoolean;

    /// Wake a [`g_main_context_iteration`] that is blocking on `context`.
    /// Thread-safe — this is the one GMainContext call we make from *other*
    /// threads (the public API senders) to break the worker out of its block.
    pub fn g_main_context_wakeup(context: *mut GMainContext);
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
