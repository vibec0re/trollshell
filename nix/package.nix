{
  lib,
  stdenv,
  runCommand,
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
  # The per-binary slice derivations below reuse them too, so the GApps wrapper
  # env they produce is byte-identical to what the compile stage would have
  # produced in-place.
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
  # and the single workspace compile. The bindgen consumer (pipewire-sys/
  # libspa-sys) runs during the deps build, so bindgenHook (which populates
  # LIBCLANG_PATH + BINDGEN_EXTRA_CLANG_ARGS) has to apply there too.
  commonArgs = {
    pname = "trollshell";
    version = "0.1.0";
    inherit src nativeBuildInputs buildInputs;

    # strictDeps stays off (crane's default): the bindgen build scripts read the
    # pipewire headers from buildInputs, simplest with one shared include path.

    # ONE cargo scope for every stage (#572). Cargo derives each dependency's
    # feature set from the UNION of the packages built in one invocation and
    # fingerprints each artifact on that exact set, so a `-p trollshell` stage
    # and a `--workspace` stage disagree about shared deps and cannot reuse each
    # other's target dir. Before #572 the deps stage was `-p trollshell` while
    # the workspace stage was `--workspace`, so the deps cache was largely dead
    # weight; stating the scope once here keeps every stage feature-identical.
    cargoExtraArgs = "--workspace --locked";
    # Run the hermetic internals suite as part of the build. The real-system
    # tests (dbus-daemon + display server) sit behind the `system-tests` cargo
    # feature, which we deliberately don't enable here, so the default workspace
    # `cargo test` needs no live daemons and runs cleanly in the sandbox.
    # `cargoExtraArgs` already scopes the test run to `--workspace`, so no
    # separate `cargoTestExtraArgs` is needed.
    doCheck = true;
    cargoTestExtraArgs = "";

    # No compile-time TROLLSHELL_DATA_DIR / HYTTE_UI_DATA_DIR here: both are
    # injected at *runtime* by the wrapper below, pointing at the standalone
    # `assets` derivation. Keeping them out of the build env is what decouples
    # the assets from the (expensive) Rust compile (#133). The dev `cargo run`
    # path stays covered by the in-crate compile-time fallbacks — both assets.rs
    # (trollshell) and app.rs (hytte-ui) fall back to their crate's
    # CARGO_MANIFEST_DIR when the runtime env is unset.

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
    # runs for both the deps build and the workspace build (NIX_BUILD_TOP is the
    # same /build in each), so the source path matches and cargo reuses the
    # cached libspa artifact instead of recompiling it read-only.
    #
    # `--preserve=timestamps` is LOAD-BEARING for artifact reuse (#530). The
    # vendored sources in the nix store carry the normalised mtime (1 s past the
    # epoch); a plain `cp` would instead stamp the copy with the *current* build
    # time. Cargo's build-script staleness check compares each vendored source's
    # mtime against the (inherited, mtime-1) build-script output — so a
    # current-time copy makes EVERY build-script crate (proc-macro2, libc,
    # serde, the *-sys crates, …) look newer than its cached output and rebuild,
    # cascading into ~the whole graph. Preserving the epoch mtime keeps
    # source-vs-output equal, so cargo reuses the inherited artifacts. (chmod
    # still adds u+w for the libspa bindgen scratch writes — writability and
    # timestamps are independent.)
    preBuild = ''
      writableVendor="$NIX_BUILD_TOP/writable-vendor"
      cp -rL --preserve=timestamps --no-preserve=mode,ownership "$cargoVendorDir" "$writableVendor"
      chmod -R u+w "$writableVendor"
      substituteInPlace "$CARGO_HOME/config.toml" \
        --replace-fail "$cargoVendorDir" "$writableVendor"
    '';
  };

  # The external dependency closure, cached on Cargo.lock changes only. Same
  # `--workspace --locked` scope as the workspace build above it, so the feature
  # union matches and the compile stage actually inherits these artifacts. The
  # deps stage compiles dummy workspace crates, so running their (nonexistent)
  # tests would be pure overhead — `--no-run` still compiles + caches the
  # dev-dependency graph, which is the point.
  cargoArtifacts = craneLib.buildDepsOnly (commonArgs // { cargoTestExtraArgs = "--no-run"; });

  # THE workspace compile — the single cargo invocation that produces every
  # binary this flake ships (#572, implementing kaesaecracker's plan).
  #
  # Everything downstream (the shell, the control center, the 12 bundled widget
  # plugins, the hytte-infobroker CLI, and since #588 the two nixosTest probe
  # *examples*) is a *slice* of this one output: a `cp` of one binary out of
  # `$out/bin`, optionally wrapped. There is no second crane invocation anywhere
  # in the tree that compiles the default feature set, so there is no second
  # cargo fingerprint universe that can drift out of sync with this one. The
  # only crane calls left are `checks.{clippy,system-tests}`, which compile
  # `--features system-tests` — a genuinely different feature union that by
  # construction cannot be a slice of this build.
  #
  # History: #530 introduced an intermediate `cargoBuild` whose packed `target`
  # dir was inherited as `cargoArtifacts` by a `buildPackage` per binary, on the
  # theory that each would find its binary already built and do "little more
  # than link + install". #572 measured that and found it false — every consumer
  # recompiled the workspace, so 12 plugin packages meant 12 workspace compiles
  # (~40 min apiece locally, and ~40 min of extra parallel CI work per run since
  # #561 wired them all into `checks`). Inheriting a warm `target` dir across
  # derivations is a cache *hope*; slicing one output is a guarantee.
  #
  # `buildPackage` captures the binaries from cargo's JSON build log in a
  # `postBuild` hook (crane's installFromCargoBuildLogHook), i.e. BEFORE the
  # check phase — so hosting `doCheck` here cannot clobber what gets installed,
  # and the dev-dependency feature unification `cargo test` triggers is
  # harmless because nothing downstream compiles anything.
  #
  # `dontWrapGApps` keeps `$out/bin` raw, unwrapped ELFs. The GTK apps
  # (trollshell, trollshell-control-center) get wrapped in their own slice
  # derivations below / in control-center.nix; the plugins are GTK-free by
  # design (a plugin ships a declarative widget tree over hytte-plugin-proto and
  # the *host* renders it — crates/hytte-plugin/README) and stay unwrapped, so
  # they never drag the Adwaita/GSettings closure at runtime.
  workspace = craneLib.buildPackage (
    commonArgs
    // {
      inherit cargoArtifacts;
      pname = "trollshell-workspace";
      dontWrapGApps = true;

      # The two nixosTest probes — `hytte-ecal`'s `probe` and `hytte-services`'
      # `wifi_probe` (nix/probe.nix, nix/wifi-probe.nix) — are `--example`
      # targets, and cargo's default `build` target selection is lib + bins, so
      # crane's installFromCargoBuildLog never sees them: they aren't in the
      # build phase's JSON log. They are nonetheless already compiled in this
      # very derivation. `cargo test`'s documented default target selection
      # builds every example "to ensure they compile", and `doCheck = true`
      # above runs `cargo test --workspace --locked` in this same target dir
      # under the same release profile. The check phase runs before
      # installPhase (stdenv's phase order), so by the time this hook fires both
      # binaries are sitting in `target/release/examples/`. Copy them into
      # `$out/bin` alongside the declared bins so nix/{probe,wifi-probe}.nix can
      # slice them out like every other package output (#588).
      #
      # Deliberately NOT `cargoBuildExtraArgs = "--bins --examples"`, which
      # would be the obvious explicit spelling: selecting an example target
      # makes cargo unify that package's *dev*-dependencies into the build graph
      # (resolver v3), producing a THIRD feature configuration that neither the
      # deps stage's `cargo build --workspace` (no dev-deps) nor its `cargo test
      # --no-run` (dev-deps of *every* member) cached. hytte-reactive's dev
      # `tokio = { features = ["test-util"] }` alone is enough to make the two
      # unions differ, so tokio — and the whole graph beneath it — would
      # recompile a third time. Riding the check phase costs zero extra
      # compilation, which is the entire point of #572/#588.
      # `-print -quit` rather than the usual `… | head -1`: stdenv's setup.sh
      # runs the build script under `set -eu -o pipefail`, so a `find | head`
      # pipeline can abort the whole build on SIGPIPE once `head` closes the
      # pipe. `-quit` stops the traversal at the first hit instead, with no pipe
      # and no race — and it doesn't walk the rest of a multi-GiB target dir.
      postInstall = ''
        for example in probe wifi_probe; do
          exampleBin="$(find "''${CARGO_TARGET_DIR:-target}" -type f -name "$example" -path '*/examples/*' -print -quit)"
          if [ -z "$exampleBin" ]; then
            echo "ERROR: example binary '$example' was not built." >&2
            echo "The workspace build installs the nixosTest probe examples out of" >&2
            echo "the check phase's target dir; that requires doCheck = true." >&2
            exit 1
          fi
          install -Dm755 "$exampleBin" "$out/bin/$example"
        done
      '';

      passthru = {
        inherit cargoArtifacts commonArgs;
        devInputs = { inherit nativeBuildInputs buildInputs; };
      };

      meta = {
        description = "trollshell workspace compile — every binary this flake ships (#572)";
        homepage = "https://github.com/vibec0re/trollshell/";
        license = lib.licenses.mpl20;
        platforms = lib.platforms.linux;
      };
    }
  );
in
# The shell package = one binary sliced out of `workspace`, wrapped once with
# both the GApplication environment (wrapGAppsHook4, from the same buildInputs
# the compile used, so the wrapper env is unchanged) and the runtime asset paths
# (Mara: env wrapper, NOT symlinkJoin). This derivation depends on `workspace`
# and `assets`; an asset edit rebuilds `assets` + re-runs this trivial wrapper,
# but `workspace.drvPath` is unchanged, so nothing is recompiled (#133).
stdenv.mkDerivation {
  pname = "trollshell";
  version = "0.1.0";

  dontUnpack = true;
  dontConfigure = true;
  dontBuild = true;

  nativeBuildInputs = [ wrapGAppsHook4 ];
  inherit buildInputs;

  installPhase = ''
    runHook preInstall
    install -Dm755 ${workspace}/bin/trollshell "$out/bin/trollshell"
    runHook postInstall
  '';

  # wrapGAppsHook4's fixup wraps $out/bin/trollshell with the GApplication
  # schema/icon/typelib env; append the asset paths to the same wrapper rather
  # than layering a second makeWrapper on top of it.
  preFixup = ''
    gappsWrapperArgs+=(
      --set TROLLSHELL_DATA_DIR "${assets}/share/trollshell"
      --set HYTTE_UI_DATA_DIR "${assets}/share/hytte-ui"
    )
  '';

  # `workspace` is what nix/plugin.nix, nix/control-center.nix and (since #588)
  # nix/{probe,wifi-probe}.nix slice their own binaries out of; `commonArgs` +
  # `cargoArtifacts` are what the leaf flake checks (clippy / system-tests)
  # reuse, since they compile a different feature set (`--features
  # system-tests`) and so cannot be a slice of `workspace`.
  passthru = workspace.passthru // {
    inherit workspace assets;
  };

  meta = {
    description = "hytte-based Wayland desktop shell";
    homepage = "https://github.com/vibec0re/trollshell/";
    license = lib.licenses.mpl20;
    platforms = lib.platforms.linux;
    mainProgram = "trollshell";
  };
}
