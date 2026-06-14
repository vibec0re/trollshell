{
  lib,
  craneLib,
  rustPlatform,
  pkg-config,
  wrapGAppsHook4,
  glib,
  gtk4,
  libadwaita,
  gtk4-layer-shell,
  gsettings-desktop-schemas,
  adwaita-icon-theme,
  hicolor-icon-theme,
  evolution-data-server,
  libical,
  gobject-introspection,
  openssl,
  pipewire,
}:
let
  # crane's default cleanCargoSource keeps only .rs/.toml/.lock; also keep the
  # stylesheets (hytte-ui/src/style.css is include_str!'d at compile time;
  # trollshell/style.css is copied into the output by postInstall) and the
  # trollshell icons that postInstall ships.
  src = lib.cleanSourceWith {
    src = ../.;
    name = "trollshell-source";
    filter =
      path: type:
      (craneLib.filterCargoSources path type)
      || (lib.hasSuffix ".css" path)
      || (lib.hasInfix "/trollshell/icons/" path);
  };

  # Args shared between the deps-only build (cached on Cargo.lock changes only)
  # and the final crate build. The bindgen consumers (hytte-pam via pam-sys,
  # pipewire-sys/libspa-sys) run during the deps build, so bindgenHook (which
  # populates LIBCLANG_PATH + BINDGEN_EXTRA_CLANG_ARGS) has to apply there too.
  commonArgs = {
    pname = "trollshell";
    version = "0.1.0";
    inherit src;

    # No strictDeps: the bindgen build scripts (pam-sys, pipewire-sys/libspa-sys)
    # run libclang against headers from buildInputs, which is simplest when host
    # and target deps share one include path — matching the old buildRustPackage.

    # Workspace has multiple crates; only build the trollshell binary.
    cargoExtraArgs = "-p trollshell";
    # Tests touch live system daemons (dbus, etc.); skip in the nix sandbox.
    doCheck = false;

    nativeBuildInputs = [
      pkg-config
      wrapGAppsHook4
      # Sets LIBCLANG_PATH + a complete BINDGEN_EXTRA_CLANG_ARGS so the bindgen
      # consumers (pam-sys, pipewire-sys/libspa-sys) find libclang and the libc
      # / clang resource headers in the sandbox.
      rustPlatform.bindgenHook
    ];

    buildInputs = [
      glib
      gtk4
      libadwaita
      gtk4-layer-shell
      gsettings-desktop-schemas
      adwaita-icon-theme
      hicolor-icon-theme

      evolution-data-server
      libical
      gobject-introspection

      openssl

      # libpipewire-0.3 + libspa-0.2 — pipewire-rs (libpipewire-sys /
      # libspa-sys) discovers headers + .so via pkg-config (.pc files
      # ship in the dev output and pkg-config is already in
      # nativeBuildInputs).
      pipewire
    ];

    # Baked into the binary at compile time; trollshell::assets reads
    # this with option_env! and falls back to CARGO_MANIFEST_DIR when
    # unset (the dev `cargo run` case).
    TROLLSHELL_DATA_DIR = "${placeholder "out"}/share/trollshell";

    # libspa-sys' bindgen uses clang_macro_fallback to constify cast macros like
    # SPA_ID_INVALID (`((uint32_t)0xffffffff)` in pipewire ≥ 1.6). The fallback
    # writes scratch files (.macro_eval.c, *.pch) into the crate's *source*
    # directory (its CWD). Cargo's vendored sources live read-only in the nix
    # store, so that write fails, the fallback silently bails, and the constant
    # vanishes — breaking the libspa build. Build from a writable copy of the
    # vendored sources so bindgen can scribble there.
    #
    # crane's vendor dir holds symlinks into per-crate read-only store paths, so
    # -L dereferences them into real files and chmod makes them writable. This
    # runs for both the deps build and the final build (NIX_BUILD_TOP is the
    # same /build in each), so the source path matches and cargo reuses the
    # cached libspa artifact instead of recompiling it read-only.
    preBuild = ''
      writableVendor="$NIX_BUILD_TOP/writable-vendor"
      cp -rL --no-preserve=mode,ownership "$cargoVendorDir" "$writableVendor"
      chmod -R u+w "$writableVendor"
      substituteInPlace "$CARGO_HOME/config.toml" \
        --replace-fail "$cargoVendorDir" "$writableVendor"
    '';
  };

  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
craneLib.buildPackage (
  commonArgs
  // {
    inherit cargoArtifacts;

    postInstall = ''
      mkdir -p $out/share/trollshell
      cp -r trollshell/icons $out/share/trollshell/
      cp trollshell/style.css $out/share/trollshell/
    '';

    meta = {
      description = "hytte-based Wayland desktop shell";
      homepage = "https://github.com/vibec0re/trollshell/";
      license = lib.licenses.mpl20;
      platforms = lib.platforms.linux;
      mainProgram = "trollshell";
    };
  }
)
