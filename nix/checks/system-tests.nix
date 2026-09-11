# Run the `system-tests` cargo-feature bucket (#232): the
# whole-file-`#![cfg(feature = "system-tests")]` integration tests
# in hytte-bus/hytte-reactive/hytte-ui, plus the `#[cfg(all(test,
# feature = "system-tests"))]` GTK unit-test modules in hytte-ui.
# These never compile anywhere else — neither the workspace compile
# (which since #1115 runs no `cargo test` at all, `nix/package.nix`)
# nor `checks.workspace-tests` above (deliberately without
# `--features system-tests`, to stay hermetic) enable this feature
# — so this is their only home. Built via
# `mkCargoDerivation` directly
# (rather than `craneLib.cargoTest`) because `cargoTest.nix`
# hardcodes `checkPhaseCargoCommand`, silently discarding any
# override — we need that command to wrap `cargo test` in
# `xvfb-run` for the GTK tests (hytte-ui's `app_smoke`/`bind`/
# widget-tree & multi-sparkline tests) to have a display.
# hytte-bus's tests spawn their own ephemeral `dbus-daemon`
# (crates/hytte-bus/tests/common/mod.rs), so that binary needs to
# be on PATH too — neither it nor `xvfb-run` are in the package's
# buildInputs, so both are supplied explicitly here. (Also in
# nix/devshell.nix's `packages`, #684, so the same command works
# locally — but this sandboxed check never sees the devShell.)
# Reuses the same cargoArtifacts as the package build/clippy: the
# `system-tests` feature is `[]` (no extra deps), so the cached
# dependency graph is unaffected — only the workspace members
# themselves (not covered by cargoArtifacts, which only caches
# true external deps) need recompiling against the extra feature.
#
# Split out of flake.nix (#1102) into its own `callPackage`-able file,
# mirroring how `packages` already lives under `nix/*.nix`. Takes exactly
# what the block closed over in flake.nix: `craneLib`, `pkgs`, and the two
# `trollshell.passthru.*` values (`commonArgs`/`cargoArtifacts`) it read off
# the package derivation there — passed in directly rather than the whole
# `trollshell` value, since those two fields are all this ever used.
{
  craneLib,
  pkgs,
  commonArgs,
  cargoArtifacts,
}:
craneLib.mkCargoDerivation (
  commonArgs
  // {
    pnameSuffix = "-system-tests";
    inherit cargoArtifacts;
    # `mesa` (llvmpipe) since #1036: gives the three GL-context
    # tests in `hytte-ui` (`gl_surface.rs`) a real, software
    # `GdkGLContext` under `xvfb-run`, so they run instead of
    # skipping. Verified in the #1036 spike
    # (https://github.com/vibec0re/trollshell/issues/1036#issuecomment-5620514934):
    # 54 of `mesa`'s 60 closure paths are already pulled in by
    # bindgen's clang/llvm dep (the shared bulk is `llvm-21.1.8-lib`),
    # so the marginal closure delta is 6 new paths / ~274 MiB, not
    # the full ~1057 MiB `mesa` closure.
    #
    # `systemd` since #1082: `systemd-run` is on `$PATH` here for the
    # `trollshell/src/plugins/tests.rs` detached-launch tests
    # (`plugin_launcher.rs`'s #419 launch path). The sandbox has no
    # `systemd --user` manager and no session bus, so `systemd-run`
    # always fails to connect and every detached launch takes the
    # direct-spawn fallback (`FallbackReason::NoUserManager`) — see
    # the doc comments on `detached_launch_falls_back_without_a_user_manager`
    # and its two siblings for which shape each one exercises there.
    nativeCheckInputs = [
      pkgs.dbus
      pkgs.xvfb-run
      pkgs.mesa
      pkgs.systemd
    ];
    doCheck = true;
    # Leaf/terminal check: nothing consumes its target dir. crane
    # defaults `doInstallCargoArtifacts = true`, which packs the whole
    # ~2.2GiB target dir into a `target.tar.zst` — and that pack step
    # was OOM-ing CI's disk ("No space left on device" / "zstd: error
    # 70" in the artifact-install after every test already passed),
    # systematically failing PRs on runners with tight disks. Turning
    # it off stops producing the tarball entirely (#530: less artifact
    # churn overall).
    doInstallCargoArtifacts = false;
    # No separate build step: `cargo test` compiles as part of the
    # check phase. `commonArgs.preBuild` (the libspa-sys writable-
    # vendor-dir workaround) still runs first via the standard
    # (now-empty) buildPhase, same as it does for the `clippy` and
    # `cargoTest`-shaped checks — so the check phase's compile
    # inherits a writable vendor dir.
    buildPhaseCargoCommand = "";
    # A fresh writable $HOME: GTK/glib want to write font/icon
    # caches, and default stdenv HOME is deliberately unwritable.
    # xvfb-run allocates its own virtual display, so no manual
    # Xvfb/DISPLAY wiring is needed. Call the real `cargo` binary
    # directly (not the `cargoWithProfile` shell helper) because
    # xvfb-run execs its argv directly rather than through a
    # shell, so a bash *function* wouldn't resolve — the plain
    # `cargo` binary is on PATH via mkCargoDerivation's own
    # nativeBuildInputs and env vars (CARGO_HOME, vendoring) are
    # inherited by the child process either way.
    preCheck = ''
      export HOME="$(mktemp -d)"
      export XDG_DATA_DIRS="${pkgs.adwaita-icon-theme}/share''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
      export TROLLSHELL_REQUIRE_ICON_THEME=1
      # llvmpipe (#1036): `__EGL_VENDOR_LIBRARY_FILENAMES` is the
      # load-bearing one — glvnd's default vendor dirs
      # (`/usr/share/glvnd/egl_vendor.d`,
      # `/run/opengl-driver/share/…`) don't exist in the sandbox, so
      # without it `eglInitialize` finds no vendor and the three
      # GL-context tests in `hytte-ui` (`gl_surface.rs`) skip.
      # `LIBGL_DRIVERS_PATH` points llvmpipe at its own `swrast_dri.so`.
      # Deliberately NOT exporting `LD_LIBRARY_PATH="${pkgs.mesa}/lib"`
      # here: `libEGL_mesa.so.0`'s own RUNPATH already carries
      # `${pkgs.mesa}/lib` absolutely, so it isn't load-bearing —
      # confirmed by re-running the three tests under llvmpipe
      # without it (exit 0, still 3/3 pass; PR #1077 review LOW-5).
      export LIBGL_ALWAYS_SOFTWARE=1
      export LIBGL_DRIVERS_PATH="${pkgs.mesa}/lib/dri"
      export __EGL_VENDOR_LIBRARY_FILENAMES="${pkgs.mesa}/share/glvnd/egl_vendor.d/50_mesa.json"
      # A skip is indistinguishable from a pass in captured output
      # (same reasoning as `TROLLSHELL_REQUIRE_ICON_THEME` above):
      # this build means the three tests to run for real, so a
      # missing/refused GL context must fail the check, not skip it.
      export TROLLSHELL_REQUIRE_GL=1
      # #1082, on the same precedent: `pkgs.systemd` above puts
      # `systemd-run` on `$PATH`, so
      # `detached_launch_falls_back_without_a_user_manager`'s own
      # "is systemd-run on PATH at all" probe must find it here — a
      # miss would mean this check's `nativeCheckInputs` regressed,
      # and a silent skip would hide exactly that. This variable's
      # reach is that one `assert!` — its two siblings
      # (`detached_launch_returns_at_once_…`,
      # `two_launches_with_one_effect_id_both_start`) have no skip
      # branch to gate: they run and pass regardless of whether
      # `systemd-run` is on `$PATH` at all, since
      # `assert_launched_then_clean_up` accepts and classifies
      # whichever `LaunchReport` fallback the sandbox produces
      # (`NoSystemdRun` or `NoUserManager`).
      export TROLLSHELL_REQUIRE_SYSTEMD_RUN=1
      # #1080, on the GL env above: `preem_gl_diff` (the #893 stage B
      # CPU/GL parity harness) runs through the same llvmpipe context
      # as the three `hytte-ui` GL tests. Under llvmpipe every case
      # has measured bit-exact since #1078 — `max |Δ| 0` of 255 on
      # every channel, all twelve cases — so `TROLLSHELL_PARITY_EXACT=1`
      # pins the harness to that zero for *this* run: a case that
      # clears the on-glass ceiling (mean 2 / p99 8 / max 32, #893)
      # but is not bit-exact still fails, named `FAIL(exact)`
      # (`trollshell/examples/preem_gl_diff.rs`). The ceiling itself
      # is untouched — a real driver still only has to clear it, not
      # match llvmpipe byte for byte. A case that only fails the
      # exact check still prints its own `PASS <case>` line before
      # the `FAIL(exact) <case>` one right after it (#1089 review,
      # INFO-2) — the exit code and the `-- summary --` block's
      # `PASS all N case(s)`/`FAIL M of N` line are the verdict; a
      # `grep '^PASS'` count over the transcript is not.
      #
      # This pin rides whatever Mesa `nixpkgs` resolves to today
      # (26.2.1 as of #1080) — it is not a hash-pinned llvmpipe
      # build. A `nix flake update` that moves Mesa can therefore red
      # this exact-mode check on a PR that never touched a shader,
      # and it will look exactly like a renderer regression (#1089
      # review, INFO-5). If that happens: re-measure on the new Mesa
      # first (the `docs/live-verify.md` headless recipe, or just
      # read this check's own `FAIL(exact)` numbers) before
      # suspecting the diff — and the fix is to re-measure, never to
      # raise the ceiling `#893`/`#1078` settled for real hardware.
      export TROLLSHELL_PARITY_EXACT=1
    '';
    checkPhaseCargoCommand = ''
      xvfb-run -a cargo test --workspace --locked --features system-tests

      # #1080: the CPU/GL parity harness (#893 stage B), built and run
      # in the same phase and the same llvmpipe env as the GL-context
      # tests above, so a shader or kit change that breaks parity
      # ships red here instead of shipping silently until someone
      # runs the `docs/live-verify.md` recipe by hand. The dedicated
      # `cargo build` below is a deliberate no-op, not a hedge
      # against an uncompiled example: `cargo test` already builds
      # every example (measured — deleting the binary and re-running
      # `cargo test -p trollshell --features system-tests --no-run`
      # alone puts it straight back), so a broken example fails on
      # the `cargo test` line above, not here (#1089 review, LOW-2).
      # What this line actually buys is independence from `cargo
      # test`'s own target selection: it guarantees the binary
      # exists at the exact path the `find` below expects, on this
      # line's own terms, rather than as a side effect this check
      # would silently lose the day `cargo test`'s target set ever
      # changes.
      #
      # `--workspace --features system-tests`, matching the `cargo
      # test` invocation above byte for byte, and deliberately not
      # `-p trollshell`: `hytte-ui`/`hytte-services`/`hytte-bus`/
      # `hytte-reactive` all carry their own `system-tests` feature,
      # which `--workspace` activates on every member that defines
      # it the same way the preceding `cargo test` did — `-p
      # trollshell` would only turn it on for `trollshell` itself,
      # leaving every dependency at a *different* (default) feature
      # set than what `cargo test` just built, and cargo would
      # recompile the whole graph a second time to reconcile them.
      # Measured in the sandbox: an earlier version of this line used
      # `-p trollshell` (no `--workspace`), and gtk4/hytte-*/
      # trollshell's own lib all rebuilt from scratch under the
      # mismatched feature set — an extra 1 minute 38 seconds
      # (`Finished … target(s) in 1m 38s`) that `--workspace` above
      # avoids entirely.
      #
      # `cargo run` has no `--workspace` (only `-p`), so run the
      # produced binary directly instead — the same `find … -print
      # -quit` idiom `nix/package.nix`'s `postInstall` uses to
      # harvest the `probe`/`wifi_probe` examples, for the same
      # reason (`-quit` avoids a `find | head` pipeline racing
      # stdenv's `set -eu -o pipefail`, and it doesn't assume
      # `CARGO_TARGET_DIR`).
      #
      # `PREEM_GL_DIFF_OUT` is the harness's own override point
      # (default `gates/`, the repo's scratch directory) — pointed at
      # `$out/parity` so the per-case `.gl.ppm`/`.cpu.ppm`/
      # `.delta.pgm` evidence lands in the check's own output and is
      # there on a green build (`result/parity`). On a *red* build
      # `$out` is never registered as a valid store path — measured
      # on nix 2.34.8, `--keep-failed` preserves the build's scratch
      # directory, not `$out`, and the partial `$out` written before
      # the failure is not reachable through it either (#1089
      # review, LOW-1) — so the durable record of a red run is the
      # `-L` transcript's `FAIL(exact) <case>`/`FAIL(ceiling) <case>`
      # lines (kept in CI's own log), not a store path.
      cargo build --workspace --locked --features system-tests --example preem_gl_diff
      exampleBin="$(find "''${CARGO_TARGET_DIR:-target}" -type f -name preem_gl_diff -path '*/examples/*' -print -quit)"
      if [ -z "$exampleBin" ]; then
        echo "ERROR: example binary 'preem_gl_diff' was not built." >&2
        exit 1
      fi
      mkdir -p "$out/parity"
      PREEM_GL_DIFF_OUT="$out/parity" xvfb-run -a "$exampleBin"
      # #1078 item 5 left this open for #1080 to decide ("Noted for
      # #1080 to decide what the CI wiring asserts"), and up to here
      # this line only asserted the harness's own exit status. That
      # misses one shape: a run that exercised **zero** cases still
      # prints `PASS all 0 case(s)` and exits 0 (#1089 review,
      # LOW-3) — no live path reaches that today (the case list is
      # the fixed 4 skins × 3 idle points and this line passes no
      # argv), but a future `--skins` regression that silently
      # empties the list would ship green through the exit code
      # alone. Assert the evidence instead of trusting the exit
      # code: exactly 12 cases means exactly 12 `.gl.ppm` files.
      gl_ppm_count="$(find "$out/parity" -maxdepth 1 -name '*.gl.ppm' -type f | wc -l)"
      if [ "$gl_ppm_count" -ne 12 ]; then
        echo "ERROR: preem_gl_diff wrote $gl_ppm_count *.gl.ppm file(s) in \$out/parity, expected 12 — a case-count regression, not a parity failure." >&2
        exit 1
      fi
    '';
  }
)
