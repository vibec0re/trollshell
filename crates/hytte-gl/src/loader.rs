//! Resolving the OpenGL entry points.
//!
//! # Why libepoxy, and why by soname
//!
//! GTK already links **libepoxy** and uses it for every GL call it makes
//! itself, so the library is in the process before any of this runs.
//! `dlopen("libepoxy.so.0")` on an already-loaded soname returns a handle on
//! *that* mapping rather than searching the filesystem, so this costs a
//! refcount and reaches exactly the dispatch table GTK's own renderer uses.
//!
//! Epoxy's exported `gl*` symbols are **dispatch stubs**: each resolves the
//! real driver entry point lazily against whatever context is current on the
//! calling thread. That is the property that makes one process-wide
//! [`gl::load_with`] correct even though a `GtkGLArea` gets one `GdkGLContext`
//! per area (#886): the pointers this loads are per-process, and the *binding*
//! they dispatch to is per-context. A loader built on `eglGetProcAddress`
//! would not have that property and would have to be redone per context.
//!
//! The spec (§ "The hard problem") asked for this to be verified on glass;
//! [`load`] therefore reports which path resolved and leaves a
//! `debug`-level line naming it, and the sanity check below turns a partial
//! resolution into an error rather than a null-pointer call.
//!
//! # The two fallbacks
//!
//! 1. `libepoxy.so.0` — the packaged soname, the expected path.
//! 2. `libepoxy.so` — the development symlink, for a build where only the
//!    `-dev` output is on the loader path.
//! 3. The process image itself (`dlopen(NULL)`), which resolves against
//!    everything already linked — GTK's libepoxy included. This is the path
//!    that keeps working if a distribution ever links epoxy statically into
//!    GTK, and it is why a failure here is genuinely "this process has no GL",
//!    not "the soname moved".

use std::ffi::c_void;
use std::sync::OnceLock;

use crate::Error;

/// Sonames tried, in order, before falling back to the process image.
const EPOXY_SONAMES: [&str; 2] = ["libepoxy.so.0", "libepoxy.so"];

/// Entry points whose absence means the rest of this crate cannot work. Checked
/// after the load so a half-resolved table becomes an [`Error::Load`] here
/// rather than a null call somewhere in a draw.
///
/// Deliberately spans all four families this crate uses — shader compilation,
/// texture storage, framebuffers, and the instanced draw — so a GLES 2-era
/// dispatch table (which has none of `TexStorage2D` / `DrawArraysInstanced` /
/// `VertexArray`) is rejected up front.
fn required_symbols_present() -> bool {
    gl::CreateShader::is_loaded()
        && gl::TexStorage2D::is_loaded()
        && gl::GenFramebuffers::is_loaded()
        && gl::GenVertexArrays::is_loaded()
        && gl::DrawArraysInstanced::is_loaded()
        && gl::BlendEquation::is_loaded()
}

/// Where the entry points came from — reported once, so the on-glass check the
/// spec asked for is a journal line rather than a guess.
fn describe(source: &str) -> &str {
    source
}

/// Load the GL entry points **once** for the process, from libepoxy.
///
/// Idempotent: the first call does the work and every later one replays its
/// verdict, so a second `GtkGLArea` realizing does not re-`dlopen` anything.
pub(crate) fn load() -> Result<(), Error> {
    static LOADED: OnceLock<Result<(), String>> = OnceLock::new();
    LOADED
        .get_or_init(load_once)
        .clone()
        .map_err(|message| Error::Load { message })
}

/// The body [`load`] memoizes.
fn load_once() -> Result<(), String> {
    let mut attempts = Vec::new();
    for soname in EPOXY_SONAMES {
        match open(Some(soname)) {
            Ok(()) => {
                tracing::debug!(source = describe(soname), "GL entry points resolved");
                return Ok(());
            }
            Err(why) => attempts.push(format!("{soname}: {why}")),
        }
    }
    match open(None) {
        Ok(()) => {
            tracing::debug!(
                source = describe("process image"),
                "GL entry points resolved"
            );
            Ok(())
        }
        Err(why) => {
            attempts.push(format!("process image: {why}"));
            Err(attempts.join("; "))
        }
    }
}

/// `dlopen` one library (or the process image, for `None`), point
/// [`gl::load_with`] at it, and check that the entry points this crate needs
/// actually resolved.
fn open(soname: Option<&str>) -> Result<(), String> {
    let library = match soname {
        // SAFETY: `Library::new` runs the library's initialisers, which is the
        // documented hazard. libepoxy is already mapped into this process by
        // GTK, so this is a refcount bump on an existing mapping and no
        // initialiser runs a second time. The name is not caller-controlled —
        // it is one of the two constants above.
        Some(name) => unsafe { libloading::Library::new(name) }.map_err(|err| err.to_string())?,
        // `dlopen(NULL)`: a handle on the process image, which maps nothing and
        // so has no initialiser hazard at all — hence the safe constructor.
        // Unix-only, which this whole tree is (Wayland/GTK4 on NixOS).
        None => libloading::Library::from(libloading::os::unix::Library::this()),
    };

    // Leaked deliberately, and this is the whole lifetime story: `gl::load_with`
    // stores raw pointers *into* this mapping in a process-global table, and
    // there is no unload hook that could invalidate them, so the handle has to
    // outlive every GL call the process will ever make. A `'static` leak is the
    // honest spelling of that; the alternative (a `OnceLock<Library>`) is the
    // same lifetime with more ceremony. One leak per process, ~a pointer.
    let library: &'static libloading::Library = Box::leak(Box::new(library));
    gl::load_with(|symbol| resolve(library, symbol));

    if required_symbols_present() {
        Ok(())
    } else {
        Err("loaded, but the GL 3.2-era entry points this crate needs are absent".to_owned())
    }
}

/// One symbol out of `library`, or null when it is not there.
///
/// Null is the contract `gl::load_with` expects for a missing entry point — it
/// leaves that command's stub in place, which is what
/// [`required_symbols_present`] then detects.
fn resolve(library: &libloading::Library, symbol: &str) -> *const c_void {
    // SAFETY: reading a symbol's address out of a loaded library. The address
    // is only ever *stored* here (by `gl::load_with`); it is called through the
    // `gl` crate's own signatures, which are generated from the Khronos
    // registry and therefore match the C ABI of the symbol they name. A symbol
    // that is absent yields `Err`, which becomes a null pointer rather than a
    // wild one.
    unsafe {
        library
            .get::<*const c_void>(symbol.as_bytes())
            .map_or(std::ptr::null(), |found| *found)
    }
}
