//! pkg-config linking for libecal-2.0 + libedataserver-1.2. The default
//! probe handles `-l` flags and search paths; we don't reference any
//! headers (FFI declarations are hand-written in `src/sys.rs`), so no
//! bindgen step is needed.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if let Err(e) = pkg_config::Config::new()
        .atleast_version("3.40")
        .probe("libecal-2.0")
    {
        // EDS isn't installed → the crate won't build. Surface a clear
        // message so the developer knows what to install (Arch:
        // `evolution-data-server`; Debian/Ubuntu: `libecal2.0-dev` etc.).
        panic!(
            "libecal-2.0 not found via pkg-config — install evolution-data-server's dev package.\n{e}",
        );
    }
    if let Err(e) = pkg_config::Config::new()
        .atleast_version("3.40")
        .probe("libedataserver-1.2")
    {
        panic!(
            "libedataserver-1.2 not found via pkg-config — install evolution-data-server's dev package.\n{e}",
        );
    }
    if let Err(e) = pkg_config::Config::new()
        .atleast_version("3.0")
        .probe("libical-glib")
    {
        panic!("libical-glib not found via pkg-config — usually shipped with libical.\n{e}");
    }
    // glib + gobject come transitively via libecal's pkg-config Requires
    // line, so probing them explicitly here would just duplicate -l flags.

    // Bake the EDS libexec dir (evolution-source-registry +
    // evolution-calendar-factory) so the opt-in `system-tests` harness can
    // spawn an ephemeral EDS. `exec_prefix` is empty under nix, so derive it
    // from `prefix`. Non-fatal: only the system tests read EDS_LIBEXEC_DIR.
    if let Ok(prefix) = pkg_config::get_variable("libecal-2.0", "prefix") {
        println!("cargo:rustc-env=EDS_LIBEXEC_DIR={prefix}/libexec");
    }
}
