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
  webkitgtk_6_0,
  gsettings-desktop-schemas,
  adwaita-icon-theme,
  hicolor-icon-theme,
  evolution-data-server,
  libical,
  gobject-introspection,
  openssl,
  pipewire,
  # Source revision string (#601), computed once in flake.nix from
  # `self.shortRev` / `self.dirtyShortRev`. Injected into the *wrapper* below,
  # never into the compile — see the `preFixup` note. Defaults so an
  # out-of-flake `callPackage ./nix/package.nix` still evaluates.
  revision ? "unknown",
}:
let
  # crane's default cleanCargoSource keeps only .rs/.toml/.lock; on top of that
  # we keep ONLY the assets the *compile* genuinely reads:
  #   - tests/fixtures — include_str!'d by the internals suite (the hermetic
  #     `cargo test --workspace` run). Since #1115 that run no longer happens
  #     on this derivation's own `doCheck` — it's `checks.workspace-tests`
  #     (flake.nix) now — but that check reuses this exact `src` via
  #     `commonArgs`, so the fixtures still have to ship here.
  #   - assets/hytte-ui/style.css — hytte-ui's DEFAULT_STYLESHEET fallback
  #     (crates/hytte-ui/src/app.rs) include_str!'s this one file at compile
  #     time, so it must be present even though the rest of `assets/` isn't.
  #   - *.vert / *.frag — the preem GL renderer's shaders (#893 stage B), which
  #     `trollshell/src/plugins/preem_gl/program.rs` include_str!'s. The
  #     extension names the stage, for glslangValidator and for a reader. They
  #     are deliberately under `src/` rather than `assets/` (the design spec
  #     says so) because they are compiled into the binary, not loaded at
  #     runtime — but crane's filter is by *extension*, so `src/` does not save
  #     them and without this clause every `nix build` fails on a missing file
  #     while `cargo build` passes locally (the #480/#446 trap, from the other
  #     side).
  #
  #     This list and `nix/lint-glsl.py`'s STAGES table must agree, or one of
  #     the two silently stops covering a file the other ships. `.glsl` used to
  #     be kept here speculatively and was *not* in that table, so the first
  #     `.glsl` file added would have shipped uncompiled — or, once the lint
  #     learned to see subdirectories, turned the check red with exit 2. A
  #     shared body added later goes in both places, with a decision about
  #     which stage(s) to compile it under.
  #
  #     Since #893 there are two more producers, both covered by the same
  #     extension clause and both scanned by the lint: `hytte-ui`'s shader-widget
  #     vertex stage (`crates/hytte-ui/src/shader_*.vert`) and plugins' widget
  #     fragment *bodies* (e.g. the preem demo's `shaders/spectrum.frag`), which
  #     are compiled with `SHADER_PREAMBLE` spliced in front because they do not
  #     compile alone.
  #
  #     **The lint's body scan is tree-wide, to match this clause exactly.** It
  #     took two tries to get there and both intermediate spellings shipped a
  #     hole: a literal one-directory list missed a second plugin's `shaders/`
  #     entirely, and a `crates/*/shaders` glob still missed
  #     `crates/<crate>/src/stray.frag` and `trollshell/shaders/stray2.frag` —
  #     both measured shipping green (#968 reviews L5 and its residual). Because
  #     this filter has *no* directory constraint, only a tree-wide scan agrees
  #     with it by construction; anything convention-shaped agrees by luck.
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
      || (lib.hasSuffix "assets/hytte-ui/style.css" path)
      || (lib.hasSuffix ".vert" path)
      || (lib.hasSuffix ".frag" path);
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

    # The claude-bridge chip names this glyph through the *icon theme* rather
    # than loading it by path (#957) — it is an out-of-process plugin, and
    # `Node::Icon` carries a name, never pixels. A rename or a deletion would
    # therefore surface as `image-missing` on the bar with nothing red anywhere,
    # and no Rust test can catch it: the crane source filter above strips
    # `assets/`, so `checks.system-tests` never sees this directory. Assert it
    # here, where the file is actually shipped — this derivation is trivial, so
    # the check costs nothing and couples nothing to the Rust compile.
    test -f $out/share/trollshell/icons/claude-symbolic.svg
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

  # `buildInputs` PLUS WebKitGTK 6.0 — `webkit6-sys`'s pkg-config lookup
  # (`webkitgtk-6.0.pc`), needed by exactly one workspace member, the #950
  # companion window (crates/trollshell-agent-window).
  #
  # **The split is load-bearing, and it is the fix for #1130's M1.** It has to
  # be in the *compile* inputs, because there is one `craneLib.buildPackage`
  # for the whole workspace (#572/#587) and that one compile builds this member
  # too. But `buildInputs` is also the list the GTK **slices** hand to
  # `wrapGAppsHook4`, and `wrap-gapps-hook.sh` propagates `GI_TYPELIB_PATH`
  # verbatim — webkitgtk ships `lib/girepository-1.0/{WebKit,JavaScriptCore,
  # WebKitWebProcessExtension}-6.0.typelib`, so it lands in that variable, so
  # the wrapper script *references the store path*, so it is a runtime
  # dependency of the wrapped binary. Measured on the #1130 branch before this
  # split: `nix why-depends .#trollshell …webkitgtk-…abi=6.0` reported a DIRECT
  # reference, and the marginal closure was **one path, 167 MiB**, on both the
  # shell and the control center — neither of which ever loads a web engine.
  #
  # So: the compile, the devShell and the agent-window slice take `webInputs`;
  # the `trollshell` slice, `nix/control-center.nix` and the two probes take
  # the plain `buildInputs`. `checks.shell-has-no-web-engine`
  # (nix/checks/shell-has-no-web-engine.nix) asserts the result against the
  # shell's real closure, naming the ABI — a bare `webkitgtk` grep is red on
  # `main` already, via evolution-data-server's 4.1.
  webInputs = buildInputs ++ [ webkitgtk_6_0 ];

  # Args shared between the deps-only build (cached on Cargo.lock changes only)
  # and the single workspace compile. The bindgen consumer (pipewire-sys/
  # libspa-sys) runs during the deps build, so bindgenHook (which populates
  # LIBCLANG_PATH + BINDGEN_EXTRA_CLANG_ARGS) has to apply there too.
  commonArgs = {
    pname = "trollshell";
    version = "0.1.0";
    inherit src nativeBuildInputs;
    # The compile needs every member's native deps; see `webInputs` above for
    # why the wrappers must not inherit this same list.
    buildInputs = webInputs;

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
    # #1115: `doCheck` is deliberately NOT set here any more. It used to be
    # `true` on this shared bundle, so both the deps stage below and the
    # workspace compile ran the whole hermetic internals suite — meaning a
    # consumer's `nix build .#trollshell` (or any other slice of `workspace`)
    # paid for it too, and the deps stage compiled the dev-dependency graph
    # just to feed that. Neither belongs to a package build: `nix flake
    # check` already builds the package (#449), and now runs the hermetic
    # suite as its own check instead — `checks.workspace-tests` in flake.nix.
    # Same shape hyperhive settled on for the same problem
    # (`hyperhive/nix/rust.nix:43-94`): two named `buildDepsOnly` caches, one
    # per audience, rather than one cache and a shared `doCheck`. See
    # `cargoArtifacts` and `cargoArtifactsBinOnly` below for the two, and
    # `workspace`'s own `doCheck = false` for the compile stage.

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

  # The external dependency closure for the CHECKS (clippy, `system-tests`,
  # and — since #1115 — `workspace-tests` in flake.nix), cached on Cargo.lock
  # changes only. Same `--workspace --locked` scope as `commonArgs`, so the
  # feature union matches whatever each check compiles against it.
  #
  # `craneLib.buildDepsOnly`'s own default `doCheck = true` is deliberately
  # left alone here (no override): it adds `--all-targets` to the check
  # command and a `cargo test --no-run`, so this cache compiles and caches
  # the dev-dependency graph and every test harness in the workspace — dead
  # weight for a plain compile, but exactly what a check that runs or lints
  # tests needs. `cargoArtifactsBinOnly` below is the OTHER audience: no
  # `doCheck`, no dev-deps, feeding `workspace`'s own compile instead.
  #
  # Same split hyperhive made for the same reason
  # (`hyperhive/nix/rust.nix:43-75`, `cargoArtifacts`): one cache per
  # audience, because a deploy never runs or links a test binary and
  # shouldn't pay to compile one.
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;

  # The external dependency closure for the PACKAGE build — the audience
  # `cargoArtifacts` above deliberately doesn't serve. `doCheck = false` here
  # drops `buildDepsOnly`'s default `--all-targets` + `cargo test --no-run`,
  # so dev-dependencies and test harnesses are never compiled for a consumer
  # build at all. Distinct `pname` (rather than sharing `commonArgs`'
  # `"trollshell"`) so the two caches are told apart in build logs and store
  # paths, not just in this file — same reasoning and the same distinct-name
  # convention as hyperhive's `cargoArtifactsBinOnly`
  # (`hyperhive/nix/rust.nix:77-94`).
  cargoArtifactsBinOnly = craneLib.buildDepsOnly (
    commonArgs
    // {
      pname = "trollshell-workspace-bin";
      doCheck = false;
    }
  );

  # THE workspace compile — the single cargo invocation that produces every
  # binary this flake ships (#572, implementing kaesaecracker's plan).
  #
  # Everything downstream (the shell, the control center, the 14 bundled widget
  # plugins, the hytte-infobroker CLI, and since #588 the two nixosTest probe
  # *examples*) is a *slice* of this one output: a `cp` of one binary out of
  # `$out/bin`, optionally wrapped. There is no second crane invocation anywhere
  # in the tree that compiles the default feature set, so there is no second
  # cargo fingerprint universe that can drift out of sync with this one. The
  # only other crane calls are `checks.{clippy,system-tests}`, which compile
  # `--features system-tests` — a genuinely different feature union that by
  # construction cannot be a slice of this build — and, since #1115,
  # `checks.workspace-tests`, which reuses this derivation's own `commonArgs`
  # (same feature union as this build) and the `cargoArtifacts` cache above
  # (NOT the `cargoArtifactsBinOnly` this compile uses — see both for why).
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
  # check phase — the capture never depended on whether a check phase ran at
  # all. That's why #1115 could turn `doCheck` off below without touching
  # this hook: it already fires at the end of the build phase, `runHook
  # postBuild`, regardless of `doCheck`.
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
      # `cargoArtifactsBinOnly`, NOT `cargoArtifacts` — this is the compile
      # stage the bin-only cache exists for (see both bindings above). Same
      # pairing as hyperhive's `workspaceBuild`
      # (`hyperhive/nix/packages/default.nix:61-68`): the deploy path takes
      # the no-test-graph cache and sets its own `doCheck = false`
      # (`buildPackage`'s own default is `args.doCheck or true`, so this has
      # to be explicit here even though `commonArgs` carries no `doCheck` of
      # its own any more).
      cargoArtifacts = cargoArtifactsBinOnly;
      doCheck = false;
      pname = "trollshell-workspace";
      dontWrapGApps = true;

      # The two nixosTest probes — `hytte-ecal`'s `probe` and `hytte-services`'
      # `wifi_probe` (nix/probe.nix, nix/wifi-probe.nix) — are `--example`
      # targets, and cargo's default `build` target selection is lib + bins, so
      # crane's installFromCargoBuildLog never sees them: they aren't in the
      # build phase's JSON log.
      #
      # Before #1115 this rode `doCheck = true` for free: `cargo test`'s
      # documented default target selection builds every example "to ensure
      # they compile", so the check phase's `cargo test --workspace --locked`
      # produced both binaries as a side effect of a dev-dependency compile
      # that was already happening for the hermetic suite. `doCheck` is now
      # `false` on this derivation (#1115) — there is no check phase here any
      # more for that side effect to ride — so build the two examples
      # explicitly instead.
      #
      # Scoped one crate at a time (`-p hytte-ecal --example probe`, then
      # `-p hytte-services --example wifi_probe`) rather than `--workspace
      # --examples`: selecting an example target unifies that *package's own*
      # dev-dependencies into the build graph (resolver v3), and scoping
      # avoids pulling every OTHER workspace member's dev-deps in too —
      # cheaper than the `cargo test --workspace` compile this replaces, and
      # this derivation sits on `cargoArtifactsBinOnly` (above), which has no
      # cached "dev-deps of every member" artifact for a wider `--workspace
      # --examples` build to matter less by matching anyway.
      # `cargoWithProfile` (crane's helper, sourced by `cargoHelperFunctionsHook`
      # into every phase of this derivation, not just build/check) keeps the
      # profile the same `--release` the rest of this derivation uses.
      #
      # `-print -quit` rather than the usual `… | head -1`: stdenv's setup.sh
      # runs the build script under `set -eu -o pipefail`, so a `find | head`
      # pipeline can abort the whole build on SIGPIPE once `head` closes the
      # pipe. `-quit` stops the traversal at the first hit instead, with no pipe
      # and no race — and it doesn't walk the rest of a multi-GiB target dir.
      postInstall = ''
        cargoWithProfile build --locked -p hytte-ecal --example probe
        cargoWithProfile build --locked -p hytte-services --example wifi_probe
        for example in probe wifi_probe; do
          exampleBin="$(find "''${CARGO_TARGET_DIR:-target}" -type f -name "$example" -path '*/examples/*' -print -quit)"
          if [ -z "$exampleBin" ]; then
            echo "ERROR: example binary '$example' was not built." >&2
            exit 1
          fi
          install -Dm755 "$exampleBin" "$out/bin/$example"
        done
      '';

      # The icon-theme test env (`every_icon_name_exists_in_the_adwaita_theme_on_the_search_path`,
      # crates/hytte-plugin-niri-layouts/src/plugin.rs) used to live here as a
      # `preCheck` (#1038 review MED-4) because `doCheck = true` ran the
      # hermetic suite on this very derivation. #1115 turned that off — there
      # is no check phase left here for a `preCheck` to gate — so the
      # equivalent env moved to `checks.workspace-tests` (flake.nix), which
      # runs that suite now.

      passthru = {
        inherit cargoArtifacts cargoArtifactsBinOnly commonArgs;
        # `buildInputs` is what a **wrapper** should see; `webInputs` adds
        # WebKitGTK and is for the compile, the devShell and the one slice that
        # ships a web engine. See `webInputs`' comment above — handing the
        # wrong one to `wrapGAppsHook4` is #1130's M1, and it costs 167 MiB of
        # closure on a binary that never loads it.
        devInputs = { inherit nativeBuildInputs buildInputs webInputs; };
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
  #
  # TROLLSHELL_REV (#601) rides the SAME wrapper for the same reason the asset
  # paths do — it must not reach the compile. A compile-time env would rewrite
  # `workspace`'s derivation hash on every single commit, forcing a full
  # ~40-minute workspace rebuild per revision and invalidating the artifact
  # every other package output slices from. Here it costs one `cp` + one
  # makeWrapper re-run. trollshell/src/revision.rs reads it at runtime.
  preFixup = ''
    gappsWrapperArgs+=(
      --set TROLLSHELL_DATA_DIR "${assets}/share/trollshell"
      --set HYTTE_UI_DATA_DIR "${assets}/share/hytte-ui"
      --set TROLLSHELL_REV "${revision}"
    )
  '';

  # `workspace` is what nix/plugin.nix, nix/control-center.nix and (since #588)
  # nix/{probe,wifi-probe}.nix slice their own binaries out of; `commonArgs` +
  # `cargoArtifacts` are what the leaf flake checks (clippy / system-tests /
  # since #1115 workspace-tests) reuse instead of `workspace` itself — clippy
  # and system-tests because they compile a different feature set (`--features
  # system-tests`) and so cannot be a slice of `workspace`, workspace-tests
  # because `workspace` sits on `cargoArtifactsBinOnly` (no dev-dependency
  # graph) rather than `cargoArtifacts` (which has one).
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
