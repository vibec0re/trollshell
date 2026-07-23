{
  lib,
  runCommand,
  makeWrapper,
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
  # crane's default cleanCargoSource keeps only .rs/.toml/.lock; on top of that
  # we keep ONLY the assets the *compile* genuinely reads:
  #   - tests/fixtures — include_str!'d by the internals suite (doCheck runs
  #     `cargo test` in the sandbox).
  #   - assets/hytte-ui/style.css — hytte-ui's DEFAULT_STYLESHEET fallback
  #     (crates/hytte-ui/src/app.rs) include_str!'s this one file at compile
  #     time, so it must be present even though the rest of `assets/` isn't.
  # No OTHER stylesheets/icons are kept: everything else in `assets/` is
  # loaded from disk at runtime — the binary resolves them via the
  # makeWrapper env (TROLLSHELL_DATA_DIR / HYTTE_UI_DATA_DIR → the `assets`
  # derivation below), and dev falls back to the compile-time
  # CARGO_MANIFEST_DIR path. Keeping `assets/` (bar this one file) out of the
  # crane src filter means editing an icon or any other stylesheet doesn't
  # invalidate the expensive Rust build — only the trivial `assets`
  # derivation + the wrapper rebuild (#133).
  src = lib.cleanSourceWith {
    src = ../.;
    name = "trollshell-source";
    filter =
      path: type:
      (craneLib.filterCargoSources path type)
      || (lib.hasInfix "/tests/fixtures/" path)
      || (lib.hasSuffix "assets/hytte-ui/style.css" path);
  };

  # Standalone assets derivation: depends ONLY on the asset files, so editing a
  # stylesheet or icon rebuilds just this (cheap) derivation + the wrapper, not
  # the binary. Mara's call: an env wrapper over this, not a symlinkJoin.
  #   $out/share/trollshell/{style.css,icons/}  → TROLLSHELL_DATA_DIR
  #   $out/share/hytte-ui/style.css             → HYTTE_UI_DATA_DIR
  assets = runCommand "trollshell-assets" { } ''
    mkdir -p $out/share/trollshell $out/share/hytte-ui
    cp -r ${../assets/trollshell/icons} $out/share/trollshell/icons
    cp ${../assets/trollshell/style.css} $out/share/trollshell/style.css
    cp ${../assets/hytte-ui/style.css} $out/share/hytte-ui/style.css
  '';

  # Pulled out of commonArgs so the dev shell can reuse the exact same deps via
  # passthru.devInputs — crane appends its own build-orchestration hooks to the
  # final derivation's nativeBuildInputs, which spam "cargoVendorDir not set"
  # warnings when inherited into a shell, so the shell takes these raw lists.
  nativeBuildInputs = [
    pkg-config
    wrapGAppsHook4
    # Sets LIBCLANG_PATH + a complete BINDGEN_EXTRA_CLANG_ARGS so the bindgen
    # consumer (pipewire-sys/libspa-sys) finds libclang and the libc /
    # clang resource headers in the sandbox.
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

  # Args shared between the deps-only build (cached on Cargo.lock changes only)
  # and the final crate build. The bindgen consumer (pipewire-sys/libspa-sys)
  # runs during the deps build, so bindgenHook (which populates
  # LIBCLANG_PATH + BINDGEN_EXTRA_CLANG_ARGS) has to apply there too.
  commonArgs = {
    pname = "trollshell";
    version = "0.1.0";
    inherit src nativeBuildInputs buildInputs;

    # strictDeps stays off (crane's default): the bindgen build scripts read the
    # pipewire headers from buildInputs, simplest with one shared include path.

    # Workspace has multiple crates; only build the trollshell binary.
    cargoExtraArgs = "-p trollshell";
    # Run the hermetic internals suite as part of the build. The real-system
    # tests (dbus-daemon + display server) sit behind the `system-tests` cargo
    # feature, which we deliberately don't enable here, so the default workspace
    # `cargo test` needs no live daemons and runs cleanly in the sandbox.
    # cargoExtraArgs scopes the *build* to trollshell; --workspace broadens the
    # *test* run to every member crate's internals.
    doCheck = true;
    cargoTestExtraArgs = "--workspace";

    # No compile-time TROLLSHELL_DATA_DIR / HYTTE_UI_DATA_DIR here: both are
    # injected at *runtime* by the makeWrapper wrapping below, pointing at the
    # standalone `assets` derivation. Keeping them out of the build env is what
    # decouples the assets from the (expensive) Rust compile (#133). The dev
    # `cargo run` path stays covered by the in-crate compile-time fallbacks —
    # both assets.rs (trollshell) and app.rs (hytte-ui) fall back to their
    # crate's CARGO_MANIFEST_DIR when the runtime env is unset.

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

  # The actual Rust binary. Crucially it does NOT reference `assets`: nothing
  # about its inputs or build env mentions an asset path, so its drvPath is
  # invariant under asset edits. wrapGAppsHook4 still wraps it for GTK/GSettings
  # env. The asset env is layered on *outside* this derivation (see below) so
  # the expensive compile stays decoupled from the cheap assets (#133).
  unwrapped = craneLib.buildPackage (
    commonArgs
    // {
      inherit cargoArtifacts;

      # Raw input lists for the dev shell to reuse without crane's build hooks.
      passthru.devInputs = { inherit nativeBuildInputs buildInputs; };
      passthru.commonArgs = commonArgs;
      passthru.cargoArtifacts = cargoArtifacts;

      meta = {
        description = "hytte-based Wayland desktop shell (unwrapped — no asset env)";
        homepage = "https://github.com/vibec0re/trollshell/";
        license = lib.licenses.mpl20;
        platforms = lib.platforms.linux;
        mainProgram = "trollshell";
      };
    }
  );
in
# Final package = a thin wrapper that injects the runtime asset paths via
# makeWrapper (Mara: env wrapper, NOT symlinkJoin). It depends on `unwrapped`
# and `assets`; an asset edit rebuilds `assets` + re-runs this trivial wrapper,
# but `unwrapped.drvPath` is unchanged, so the Rust binary is NOT recompiled.
# We copy/symlink the rest of `unwrapped`'s outputs through and re-wrap only the
# binary, so consumers (mainProgram, share/) keep working.
runCommand "trollshell-0.1.0"
  {
    nativeBuildInputs = [ makeWrapper ];

    # Preserve the consumed passthru/meta from the inner build so flake.nix
    # (commonArgs/cargoArtifacts) and the dev shell (devInputs) keep resolving,
    # and `nix run` still finds the main program.
    passthru = unwrapped.passthru // {
      inherit unwrapped assets;
    };
    meta = unwrapped.meta // {
      description = "hytte-based Wayland desktop shell";
    };
  }
  ''
    mkdir -p $out/bin
    # Re-link everything except bin/ from the inner output so e.g. share/ stays
    # reachable, then wrap the binary with the runtime asset env. The asset
    # paths come from `assets`, so editing an asset never touches `unwrapped`.
    for d in ${unwrapped}/*; do
      name="$(basename "$d")"
      [ "$name" = "bin" ] && continue
      ln -s "$d" "$out/$name"
    done
    makeWrapper ${unwrapped}/bin/trollshell $out/bin/trollshell \
      --set TROLLSHELL_DATA_DIR "${assets}/share/trollshell" \
      --set HYTTE_UI_DATA_DIR "${assets}/share/hytte-ui"
  ''
