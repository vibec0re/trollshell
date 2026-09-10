//! Resolving the OpenGL entry points.
//!
//! # Nothing exports the plain Khronos spellings
//!
//! `gl::load_with` asks for `glCreateShader`, `glTexStorage2D`, … — the
//! unprefixed Khronos names. **No object in a GTK4 process exports those as ELF
//! symbols**, and #1067 is the bill for assuming otherwise: this module asked
//! for them verbatim, got nothing on every machine, and so
//! [`Gl::current`](crate::Gl::current) always returned [`Error::Load`] and the
//! preem GL renderer never engaged once in its shipped life. Measured on
//! nixpkgs (libepoxy 1.5.10, libglvnd 1.7.0), inside a live GTK4 process with a
//! GLES 3.2 context current:
//!
//! * **libepoxy** exports 3403 `epoxy_gl*` symbols and **zero** `gl*` ones —
//!   and the `epoxy_` ones are `D`, i.e. *variables holding* a function
//!   pointer, not functions. Epoxy's public ABI is
//!   `extern PFNGLFOOPROC epoxy_glFoo;` plus a header `#define glFoo
//!   epoxy_glFoo`; the unprefixed spelling is a preprocessor artifact that
//!   never reaches a symbol table.
//! * The only GL-ish objects that reach the process's **global** namespace do
//!   so through `libgtk-4.so.1` → `libgstgl-1.0.so.0`, whose `DT_NEEDED`
//!   carries glvnd's `libEGL.so.1` and `libGLX.so.0`. Those export `egl*` and
//!   `glX*`; `libGLdispatch.so.0` behind them exports no plain `gl*` either.
//! * glvnd's `libGL.so.1` / `libGLESv2.so.2` / `libOpenGL.so.0` — the three
//!   that *do* export plain `gl*` — are never in the global namespace: nothing
//!   in the closure has a `DT_NEEDED` on them, and epoxy `dlopen`s them
//!   `RTLD_LOCAL`.
//!
//! So the entry points have to be *asked for* rather than looked up, and the
//! three sources below are that ask, in order.
//!
//! # 1. glvnd's `eglGetProcAddress` — the route that runs
//!
//! `dlopen("libEGL.so.1")` reaches glvnd's vendor-neutral EGL, which GTK's own
//! closure has already mapped (the `libgstgl` chain above), so this costs a
//! refcount and adds nothing to the package closure. Its `eglGetProcAddress`
//! hands back **`libGLdispatch` stubs**: each one reads the calling thread's
//! current dispatch table on every call and jumps to the vendor of whatever
//! context is current *then*. That is the property that makes one process-wide
//! [`gl::load_with`] correct even though a `GtkGLArea` gets one `GdkGLContext`
//! per area (#886) — the pointers are per-process, the binding they dispatch to
//! is per-context, and unlike the epoxy route below there is no rewrite
//! subtlety and no dependence on whether GTK has already called through
//! anything. GDK creates EGL contexts on both Wayland and X11
//! (`GdkX11GLContextEGL`), so one EGL entry point serves every display backend
//! we run on; `glXGetProcAddressARB` out of `libGLX.so.0` is deliberately
//! **not** tried, and should not be until a GLX context is actually observed.
//!
//! **This route cannot certify itself.** glvnd allocates a dispatch slot for
//! any `gl`-shaped name it is handed, so it answers non-null for entry points
//! the current vendor does not implement — measured, `glThisDoesNotExistAtAll`
//! resolves to a live stub. [`required_symbols_present`] therefore proves
//! nothing here (it stays because it is a real gate on the other two routes,
//! where a name that is absent really does resolve to null). What confirms the
//! table is live is the **caller's first real GL call**: `Gl::current()`'s
//! contract is that a context is current, the shell's next act is a draw, and
//! `hytte-ui`'s three `gl_surface` tests make real calls against a real driver.
//! No probe call is made from here on purpose: `load` memoizes its verdict for
//! the process, a glvnd stub invoked with no context current returns from a
//! no-op with an undefined result rather than a diagnosable one, and on the
//! epoxy route below a resolver that cannot find a provider `abort()`s the
//! process — so a mistimed probe would be either a lie or a crash, permanently.
//!
//! # 2. libepoxy's `epoxy_<name>` variables — the fallback
//!
//! For a process where glvnd is not mapped at all. Two traps live here, and
//! both are load-bearing:
//!
//! * `libloading::Symbol<T>` derefs to the **dlsym address** cast to `T`, not
//!   to the value at that address. `*found` on a `Symbol<*const c_void>` is
//!   therefore already the address *of* `epoxy_glFoo`, and handing that to
//!   `gl::load_with` points every entry point into `.data` — the first GL call
//!   is then a clean SIGSEGV (measured). Reading the pointer the variable holds
//!   needs one more deref, which is what [`Source::resolve`] does.
//! * **Which pointer gets captured is a function of when this runs.** Before
//!   anything calls through it, `epoxy_glFoo` holds
//!   `epoxy_glFoo_global_rewrite_ptr` — a resolver that re-resolves against the
//!   current context on each call and rewrites the global, which is the
//!   per-context behaviour we want. But if GTK's own renderer has already
//!   called through `epoxy_glFoo`, the variable holds a **context-specific**
//!   address by then and that is what gets captured. Harmless as the tree
//!   stands: `GtkGLArea` gives one `GdkGLContext` per area (#886) and every
//!   `GlSurface`/`ShaderSurface` draw runs under the context that GDK created
//!   for it against the same driver, so every candidate address belongs to the
//!   same vendor. It stops being harmless the day two contexts in one process
//!   come from different vendors (a multi-GPU/PRIME split, or a second GL
//!   implementation loaded alongside) — at which point this route must capture
//!   the resolver instead of the rewritten pointer, or be dropped for route 1.
//!
//! # 3. Plain names from the process image — last
//!
//! `dlopen(NULL)` over everything already linked, asking for the unprefixed
//! spellings. This is what a process that carries glvnd's `libGL.so.1` in its
//! global namespace (or a distribution that links a GL implementation straight
//! into GTK) would answer, and it is the only one of the three that needs no
//! library of its own. It resolves nothing on nixpkgs — which is precisely the
//! shipped bug — so it is kept as the honest last word rather than the
//! catch-all the old header claimed it was.

use std::ffi::{CString, c_char, c_void};
use std::sync::OnceLock;

use crate::Error;

/// glvnd's vendor-neutral EGL, the first source. GTK's own closure maps it
/// (`libgtk-4.so.1` → `libgstgl-1.0.so.0` → here), so `dlopen` on this soname
/// is a refcount on an existing mapping rather than a filesystem search.
const GLVND_EGL_SONAME: &str = "libEGL.so.1";

/// Sonames tried for the libepoxy fallback, in order: the packaged soname, then
/// the development symlink for a build where only the `-dev` output is on the
/// loader path.
const EPOXY_SONAMES: [&str; 2] = ["libepoxy.so.0", "libepoxy.so"];

/// `eglGetProcAddress`'s C signature. EGL spells the argument `const char *`
/// and the result `void (*)(void)`; a `*const c_void` receives it fine, because
/// nothing here ever calls through it — [`gl::load_with`] stores it, and the
/// `gl` crate calls it through its own registry-generated signature.
type GetProcAddress = unsafe extern "C" fn(*const c_char) -> *const c_void;

/// One opened source of entry points, ready to answer [`gl::load_with`].
///
/// Split out as a type (rather than a `bool` inside one `resolve`) so each
/// route can be driven straight from a test with no GL context and no display —
/// see the tests at the bottom of this file, which is the coverage #1067 found
/// missing.
#[derive(Clone, Copy)]
enum Source {
    /// glvnd's `eglGetProcAddress`, returning per-thread-current-context
    /// dispatch stubs.
    Glvnd(GetProcAddress),
    /// libepoxy's `epoxy_<name>` function-pointer **variables**.
    Epoxy(&'static libloading::Library),
    /// Plain Khronos names, out of an object that exports them as functions.
    ///
    /// Carries its own [`describe`](Self::describe) label rather than a
    /// hardcoded one: production only ever builds this from `open(None)` (the
    /// process image), but the hermetic test also wraps a **libepoxy** handle
    /// in this variant to drive the refusal gate without a fourth source
    /// (#1070's review, M6) — a hardcoded "process image" label would name the
    /// wrong library in that refusal message.
    Plain(&'static libloading::Library, &'static str),
}

impl Source {
    /// One entry point out of this source, or null when it is not there.
    ///
    /// Null is the contract [`gl::load_with`] expects for a missing entry point
    /// — it leaves that command's stub in place, which is what
    /// [`required_symbols_present`] then detects. (On [`Source::Glvnd`] that
    /// detection is vacuous; see the module header.)
    fn resolve(self, symbol: &str) -> *const c_void {
        match self {
            Self::Glvnd(get_proc_address) => {
                let Ok(name) = CString::new(symbol) else {
                    return std::ptr::null();
                };
                // SAFETY: calling glvnd's `eglGetProcAddress` with a
                // NUL-terminated C string it only reads, through a signature
                // that matches EGL's own. The `CString` outlives the call. The
                // result is a pointer this function only *returns*; it is
                // called through the `gl` crate's registry-generated
                // signatures, which match the C ABI of the entry point they
                // name.
                unsafe { get_proc_address(name.as_ptr()) }
            }
            Self::Epoxy(library) => {
                let prefixed = format!("epoxy_{symbol}");
                // SAFETY: reading a symbol's address out of a loaded library. A
                // symbol that is absent yields `Err`, which becomes null rather
                // than a wild pointer.
                let Ok(found) = (unsafe { library.get::<*const c_void>(prefixed.as_bytes()) })
                else {
                    return std::ptr::null();
                };
                // `*found` is the address *of* the `epoxy_glFoo` variable, not
                // its value — see the module header's first epoxy trap.
                let variable: *const *const c_void = (*found).cast();
                // SAFETY: this crate cannot verify, at runtime, which ELF
                // symbol type `variable` actually is — `libloading`/`dlsym`
                // erase the object/function distinction, and only
                // `dladdr1(…, RTLD_DL_SYMENT)` reading `ELF64_ST_TYPE(st_info)`
                // could recover it (#1070's review, M3; not done here). The
                // read rests entirely on libepoxy's *published ABI*, an
                // external contract this crate cannot check, the same way
                // `hytte-ecal` trusts libecal's: `variable` is the address
                // dlsym reported for a symbol `library` defines, and epoxy's
                // public header declares every `epoxy_gl*` an initialised
                // `PFNGLFOOPROC` — a live, aligned, pointer-sized object in the
                // mapping, which `library` (leaked, `'static`) keeps mapped.
                // Reading it is one aligned load.
                let held = unsafe { *variable };
                // Cheap runtime guard for the one shape a broken ABI claim
                // would produce that a plain null check would miss: `held`
                // reading back as the address of the *variable itself* rather
                // than a distinct function pointer, which is what happens if
                // `epoxy_glFoo` turns out not to be the pointer-sized data
                // object the SAFETY comment above assumes. The hermetic test
                // asserts this half directly (`resolve must return the
                // pointer the epoxy_glGetString variable *holds*, not the
                // address of the variable itself`); this is the same check
                // made live, so a future libepoxy that violates the ABI
                // degrades this route to "resolved nothing, fall through to
                // Plain" rather than installing a wild pointer.
                if held.is_null() || held == *found {
                    return std::ptr::null();
                }
                held
            }
            Self::Plain(library, _) => {
                // SAFETY: as above — a plain dlsym, whose address is only ever
                // stored by `gl::load_with` and called through the `gl` crate's
                // own generated signatures.
                unsafe { library.get::<*const c_void>(symbol.as_bytes()) }
                    .map_or(std::ptr::null(), |found| *found)
            }
        }
    }

    /// Where the entry points came from — the `source` field on the one
    /// `debug` line, so the on-glass check the spec asked for is a journal line
    /// rather than a guess (#1067: `RUST_LOG=hytte_gl=debug`).
    fn describe(self) -> &'static str {
        match self {
            Self::Glvnd(_) => "libEGL.so.1 (eglGetProcAddress)",
            Self::Epoxy(_) => "libepoxy (epoxy_* variables)",
            Self::Plain(_, name) => name,
        }
    }
}

/// Entry points whose absence means the rest of this crate cannot work. Checked
/// after the load so a half-resolved table becomes an [`Error::Load`] here
/// rather than a null call somewhere in a draw.
///
/// Deliberately spans all four families this crate uses — shader compilation,
/// texture storage, framebuffers, and the instanced draw — so on
/// [`Source::Epoxy`] and [`Source::Plain`], where this is a **real gate**, a
/// GLES 2-era dispatch table (which has none of `TexStorage2D` /
/// `DrawArraysInstanced` / `VertexArray`) is rejected up front.
///
/// **Vacuously true on [`Source::Glvnd`]** — the route that always wins on
/// this platform — which answers non-null for any `gl`-shaped name at all
/// (see the module header); this function cannot reject anything there. #1070's
/// review (M2) measured what a vendor missing one of these entry points does
/// instead: a glvnd dispatch stub for an unimplemented call *returns*, it does
/// not trap. `glCreateShader` answers `0`; `glGetShaderiv` on it is a no-op
/// that never writes `status`, so [`Program::compile`](crate::Program::compile)
/// degrades to [`Error::Compile`] with an **empty** driver log rather than an
/// abort; `glTexStorage2D` similarly no-ops with no GL error raised, leaving a
/// texture that exists with no storage. Low probability in this tree —
/// `set_allowed_apis(GLES)` plus GDK's own GLES 3.2 context negotiation fails
/// first on hardware that thin, which is the `area.error()` exit
/// `real_gl()` (`crates/hytte-ui/src/gl_surface.rs`) now names apart from a
/// loader failure — but it is a real gap this function does not close, and
/// the vacuity noted above is the only guarantee route 1 gets.
fn required_symbols_present() -> bool {
    gl::CreateShader::is_loaded()
        && gl::TexStorage2D::is_loaded()
        && gl::GenFramebuffers::is_loaded()
        && gl::GenVertexArrays::is_loaded()
        && gl::DrawArraysInstanced::is_loaded()
        && gl::BlendEquation::is_loaded()
}

/// Load the GL entry points **once** for the process.
///
/// Idempotent: the first call does the work and every later one replays its
/// verdict, so a second `GtkGLArea` realizing does not re-`dlopen` anything.
pub(crate) fn load() -> Result<(), Error> {
    static LOADED: OnceLock<Result<&'static str, String>> = OnceLock::new();
    match LOADED.get_or_init(load_once) {
        Ok(_) => Ok(()),
        Err(message) => Err(Error::Load {
            message: message.clone(),
        }),
    }
}

/// The body [`load`] memoizes: the three sources of the module header, in
/// order, with every failure kept so the error names each path that was tried.
///
/// `Ok` carries [`Source::describe`] for the route that won — the same string
/// the `debug` line reports, and what lets the tests below assert *which*
/// source answered rather than only that one did. Idempotent, so a test may
/// call it directly without going through [`load`]'s memo.
fn load_once() -> Result<&'static str, String> {
    let mut attempts = Vec::new();

    match glvnd() {
        Ok(source) => {
            if let Some(won) = install(source, &mut attempts) {
                return Ok(won);
            }
        }
        Err(why) => attempts.push(format!("{GLVND_EGL_SONAME}: {why}")),
    }

    for soname in EPOXY_SONAMES {
        match open(Some(soname)) {
            Ok(library) => {
                if let Some(won) = install(Source::Epoxy(library), &mut attempts) {
                    return Ok(won);
                }
            }
            Err(why) => attempts.push(format!("{soname}: {why}")),
        }
    }

    match open(None) {
        Ok(library) => {
            if let Some(won) = install(
                Source::Plain(library, "process image (plain names)"),
                &mut attempts,
            ) {
                return Ok(won);
            }
        }
        Err(why) => attempts.push(format!("process image: {why}")),
    }

    Err(attempts.join("; "))
}

/// The closure [`install`] hands to [`gl::load_with`], named rather than
/// written inline at the call site.
///
/// #1070's review (M1) found the hermetic test could not see a `resolve` that
/// **ignores its own argument** — three separate mutations of exactly that
/// shape (this closure discarding `symbol`, [`Source::Glvnd::resolve`]
/// discarding it, [`Source::Epoxy::resolve`] discarding it) left every test
/// green, because the old test only ever probed `resolve("glGetString")` and
/// a discarding mutant answers that one name correctly while silently binding
/// **every** other entry point to `glGetString`'s dispatch stub —
/// `required_symbols_present` cannot tell, since it only checks non-null-ness.
/// Naming this closure gives the test one place, reachable through
/// [`install`]'s actual seam, to assert `hand_off("glGetString") !=
/// hand_off("glTexStorage2D")` through — see the test module below. It does
/// not close the gap completely: a mutant that bypasses this function and
/// re-inlines a discarding closure at the `gl::load_with` call site is still
/// invisible to any test that only calls `resolve` or `loader_for` directly.
/// That is mutation testing's ordinary "delete the mechanism, not perturb it"
/// limit, not a bug in this fix.
fn loader_for(source: Source) -> impl FnMut(&'static str) -> *const c_void {
    move |symbol| source.resolve(symbol)
}

/// Point [`gl::load_with`] at `source` and check the entry points this crate
/// needs actually resolved.
///
/// `Some(description)` when this source is the one; `None` when it resolved too
/// little and the caller should try the next, with the reason pushed onto
/// `attempts`.
fn install(source: Source, attempts: &mut Vec<String>) -> Option<&'static str> {
    gl::load_with(loader_for(source));
    if required_symbols_present() {
        tracing::debug!(source = source.describe(), "GL entry points resolved");
        Some(source.describe())
    } else {
        attempts.push(format!(
            "{}: loaded, but the GL 3.2-era entry points this crate needs are absent",
            source.describe()
        ));
        None
    }
}

/// Open glvnd's EGL and take its `eglGetProcAddress`.
fn glvnd() -> Result<Source, String> {
    let library = open(Some(GLVND_EGL_SONAME))?;
    // SAFETY: reading a symbol's address out of a loaded library and typing it
    // as EGL's own `eglGetProcAddress` signature — which is what glvnd's
    // `libEGL.so.1` defines under that name. An absent symbol yields `Err`
    // rather than a wild pointer. The library is leaked (`'static`), so the
    // function pointer cannot outlive its mapping.
    let symbol = unsafe { library.get::<GetProcAddress>(b"eglGetProcAddress") }
        .map_err(|err| err.to_string())?;
    Ok(Source::Glvnd(*symbol))
}

/// `dlopen` one library by soname (or the process image, for `None`), and leak
/// the handle.
///
/// Leaked deliberately, and this is the whole lifetime story: `gl::load_with`
/// stores raw pointers *into* this mapping in a process-global table, and there
/// is no unload hook that could invalidate them, so the handle has to outlive
/// every GL call the process will ever make. A `'static` leak is the honest
/// spelling of that; the alternative (a `OnceLock<Library>`) is the same
/// lifetime with more ceremony. At most three per process, ~a pointer each.
fn open(soname: Option<&str>) -> Result<&'static libloading::Library, String> {
    let library = match soname {
        // SAFETY: `Library::new` runs the library's initialisers, which is the
        // documented hazard. Both sonames named here are already mapped into
        // this process by GTK's own closure, so this is a refcount bump on an
        // existing mapping and no initialiser runs a second time. The name is
        // not caller-controlled — it is one of the three constants above.
        Some(name) => unsafe { libloading::Library::new(name) }.map_err(|err| err.to_string())?,
        // `dlopen(NULL)`: a handle on the process image, which maps nothing and
        // so has no initialiser hazard at all — hence the safe constructor.
        // Unix-only, which this whole tree is (Wayland/GTK4 on NixOS).
        None => libloading::Library::from(libloading::os::unix::Library::this()),
    };
    Ok(Box::leak(Box::new(library)))
}

#[cfg(test)]
mod tests {
    use super::{
        EPOXY_SONAMES, GLVND_EGL_SONAME, GetProcAddress, Source, install, load_once, loader_for,
        open,
    };

    /// Map GTK's GL closure into this test binary.
    ///
    /// The loader's two library sources are reached **by soname**, which only
    /// works in a process that already has them mapped — in the shell that is
    /// GTK's doing, and a bare `hytte-gl` test binary has neither on its
    /// `RUNPATH` (measured: every `dlopen` below returns "cannot open shared
    /// object file"). Referencing one `gdk4-sys` entry point puts `-lgtk-4` on
    /// this binary's link line, so `libgtk-4.so.1` → `libepoxy.so.0` and
    /// `libgtk-4.so.1` → `libgstgl-1.0.so.0` → `libEGL.so.1` are `DT_NEEDED`
    /// and mapped before `main`, reproducing exactly the condition the loader
    /// is written for. It is a **dev**-dependency: nothing GTK-shaped is linked
    /// into this crate as shipped, and it resolves no new package
    /// (`gdk4-sys 0.11.2` is already in `Cargo.lock` under `gtk4`).
    fn map_gtk_gl_closure() {
        std::hint::black_box(gdk4_sys::gdk_gl_context_get_type as *const ());
    }

    /// **#1067.** The loader must resolve real entry points on this platform.
    /// Needs no GL context, no display and no driver: `dlopen` + `dlsym` is all
    /// of it, and it would have been red from the day this crate landed — the
    /// shipped loader asked every source for the plain Khronos spellings, which
    /// none of them export, so `Gl::current()` always failed and the preem GL
    /// renderer fell back to the CPU kit on every machine for the whole life of
    /// #893 stage B.
    ///
    /// `glGetString` is the name asked for throughout: it is GL 1.0, so every
    /// implementation of every profile carries it, and nothing here calls it.
    ///
    /// **One test function on purpose.** Sections 2 and 3 both drive
    /// [`gl::load_with`], which writes the `gl` crate's **process-global**
    /// entry-point table; cargo runs `#[test]`s on parallel threads, so as
    /// separate functions they would race each other over that table. Ordering
    /// them here also leaves the table holding the *working* glvnd pointers
    /// when the test returns rather than section 2's deliberate nulls.
    ///
    /// **Falsified** four ways, each of which reddens a different assertion:
    /// reverting [`Source::resolve`] to the plain-name lookup for every variant
    /// (what `origin/main` does); dropping the extra deref on the epoxy path
    /// (caught by the `.data` comparison, in a test that only reads pointers,
    /// instead of by a SIGSEGV in a draw); dropping the
    /// [`required_symbols_present`] gate out of [`install`] (section 2);
    /// rewiring [`load_once`] to a different source order (section 3). Since
    /// #1070's review (M1), also falsified by any of the three sites listed on
    /// [`loader_for`]'s own doc discarding the name they are handed — caught
    /// by the `hand_off`/`hand_off_epoxy` assertions right after sections 1
    /// and 2, which none of the four mutations above touch.
    #[test]
    fn the_loader_resolves_entry_points_on_this_platform() {
        map_gtk_gl_closure();

        // ---- 1. glvnd -------------------------------------------------------
        // Whether `GLVND_EGL_SONAME` is mapped here is an incidental nixpkgs
        // packaging fact (gtk4 built with the GStreamer media backend, whose
        // `libgstgl` is what pulls glvnd's libEGL onto this test binary's
        // closure — see the module header), not a property this crate
        // controls. A gtk4 repackaging that drops it takes route 1 off this
        // *test's* closure while the shell keeps working (route 2, epoxy,
        // wins instead) — so this precondition is a loud SKIP, not a fail
        // (#1070's review, M4). Every other assertion below still panics.
        let Ok(egl) = open(Some(GLVND_EGL_SONAME)) else {
            eprintln!(
                "SKIP the_loader_resolves_entry_points_on_this_platform: {GLVND_EGL_SONAME} is \
                 not mapped in this process. This is expected if this platform's gtk4 was built \
                 without the GStreamer media backend; the shell still resolves GL through route \
                 2 (libepoxy). If gtk4 on this platform is expected to carry glvnd's libEGL, \
                 that expectation has changed and this skip is the signal to look.",
            );
            return;
        };
        // SAFETY: as in `glvnd()` above — a dlsym typed as the signature glvnd
        // defines for it, never called through here.
        let get_proc_address = *unsafe { egl.get::<GetProcAddress>(b"eglGetProcAddress") }
            .expect("glvnd's libEGL defines eglGetProcAddress");
        let glvnd = Source::Glvnd(get_proc_address);
        assert!(
            !glvnd.resolve("glGetString").is_null(),
            "glvnd's eglGetProcAddress must answer for glGetString; a null here is the #1067 \
             failure — every entry point would be left as the `gl` crate's missing-fn stub",
        );

        // The measured property that makes `required_symbols_present()` no
        // liveness check on this route, pinned so a glvnd that starts
        // validating names is noticed rather than assumed: glvnd allocates a
        // dispatch slot for any `gl`-shaped spelling, implemented or not.
        assert!(
            !glvnd.resolve("glHytteNotARealEntryPoint").is_null(),
            "glvnd is documented here as answering non-null for names no vendor implements; if \
             that has changed, the module header's reasoning about liveness needs rewriting",
        );

        // #1070's review (M1): the assertions above call `Source::resolve`
        // directly, which cannot see a discard at the seam `install` actually
        // uses — `gl::load_with(loader_for(source))`. Asserting *through*
        // `loader_for` closes that gap: a `resolve`/`loader_for` that
        // discards its argument and always answers `glGetString`'s stub would
        // still pass every assertion above but fail this one.
        let mut hand_off = loader_for(glvnd);
        assert_ne!(
            hand_off("glGetString"),
            hand_off("glTexStorage2D"),
            "the resolver install hands to gl::load_with must pass each name through, not \
             collapse every entry point onto whichever name was asked for first",
        );

        // ---- 2. libepoxy ----------------------------------------------------
        let epoxy = open(Some(EPOXY_SONAMES[0])).unwrap_or_else(|why| {
            panic!(
                "{} must be mapped in a process linked against gtk-4: {why}",
                EPOXY_SONAMES[0]
            )
        });
        let resolved = Source::Epoxy(epoxy).resolve("glGetString");
        assert!(
            !resolved.is_null(),
            "libepoxy exports epoxy_glGetString; the prefixed lookup must find it",
        );

        // The extra deref, asserted rather than trusted. Without it `resolve`
        // hands back the address *of* the `epoxy_glGetString` variable — a
        // `.data` address, which `gl::load_with` would install as an entry
        // point and the first GL call would jump into. Comparing against that
        // address catches the mutation here, in a test that only reads
        // pointers, instead of in a SIGSEGV somewhere in a draw.
        // SAFETY: a dlsym for a symbol libepoxy defines; the address is only
        // compared, never dereferenced or called.
        let variable_address =
            *unsafe { epoxy.get::<*const std::ffi::c_void>(b"epoxy_glGetString") }
                .expect("libepoxy defines epoxy_glGetString");
        assert_ne!(
            resolved, variable_address,
            "resolve must return the pointer the epoxy_glGetString variable *holds*, not the \
             address of the variable itself (libloading's Symbol<T> derefs to the dlsym address)",
        );

        // Same seam as glvnd's, above (#1070's review, M1): asserted through
        // `loader_for` so a discard at the `install`/`gl::load_with` handoff
        // is caught even though the epoxy route prefixes the name first.
        let mut hand_off_epoxy = loader_for(Source::Epoxy(epoxy));
        assert_ne!(
            hand_off_epoxy("glGetString"),
            hand_off_epoxy("glTexStorage2D"),
            "same, for the epoxy_<name> prefix path",
        );

        // ---- 1c. plain names ------------------------------------------------
        // The shipped bug, pinned as a fact about this platform rather than a
        // guess: libepoxy exports no unprefixed `gl*` at all, so the route the
        // loader used to take for *every* source finds nothing here.
        //
        // This deliberately wraps the **libepoxy** handle in `Source::Plain`
        // rather than opening a fourth, genuinely-plain library — the point is
        // to drive `install`'s refusal gate with a source that resolves
        // nothing, and epoxy's handle is already open. Its label says so
        // (#1070's review, M6): production only ever builds `Source::Plain`
        // from the process image, and a hardcoded "process image" label here
        // would misname what this refusal is actually about.
        let plain_over_epoxy = Source::Plain(epoxy, "libepoxy (via Plain, refusal-gate test)");
        assert!(
            plain_over_epoxy.resolve("glGetString").is_null(),
            "libepoxy is documented here as exporting only epoxy_gl* variables; a non-null plain \
             glGetString would mean this platform changed and the module header is stale",
        );

        // ---- 2. the gate ----------------------------------------------------
        // `required_symbols_present` is what turns a source that resolved *too
        // little* into a fallthrough instead of a table of null pointers a draw
        // would call into. `plain_over_epoxy` is a source that answers null
        // for everything (asserted just above), so installing it must be
        // refused — and the refusal must name the source, since that string is
        // the whole of the `Error::Load` message anyone triages from.
        let mut attempts = Vec::new();
        assert!(
            install(plain_over_epoxy, &mut attempts).is_none(),
            "a source that resolves nothing must be refused, not installed",
        );
        assert!(
            attempts
                .last()
                .is_some_and(|why| why.starts_with(plain_over_epoxy.describe())),
            "the refusal must name which source it was: {attempts:?}",
        );

        // ---- 3. the whole chain ---------------------------------------------
        // End-to-end, and the assertion #1067 is actually about: the shipped
        // order must resolve on this platform, and glvnd must be what answers.
        // `load_once` is idempotent and deliberately called instead of `load`,
        // so this does not consume (or depend on) the process-wide memo.
        assert_eq!(
            load_once().as_deref(),
            Ok(Source::Glvnd(get_proc_address).describe()),
            "the loader must resolve, and route 1 (glvnd) must be the source that wins here",
        );
    }
}
