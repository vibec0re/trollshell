# The two nixosTest probe examples (`probe`, `wifi_probe`) must never creep
# back into the consumer compile (#1257).
#
# `workspace` (nix/package.nix) is the single crane compile every shipped
# binary is sliced from — the shell, the control center, every plugin,
# hytte-infobroker, hytte-claude-bridge. Between #588 and #1257 the two
# nixosTest probe examples rode along inside IT, via a `postInstall` that ran
# two extra `cargo build … --example` invocations after the workspace build,
# on `cargoArtifactsBinOnly` — the cache `workspace` exists specifically to
# hold NO dev-dependency artifacts. Selecting an `--example` target unifies
# that package's own dev-dependencies into the build graph, so every single
# consumer of `workspace` paid to recompile hytte-ecal's and hytte-services'
# dev-dep closures from scratch. #1257 moved the two examples to their own
# derivation (`probes`, same file, built on `cargoArtifacts` — the dev-deps
# cache `checks.{clippy,system-tests,workspace-tests}` already share), so no
# package build reaches them any more.
#
# This is the pin against that regressing, in the `shell-has-no-web-engine`
# shape: no compile of its own beyond what `workspace` already builds for
# every other check, so it stays cheap.
#
# 1. `${workspace}/bin` must carry neither `probe` nor `wifi_probe` — the
#    direct, observable regression #1257 reported. Guarded by two existence
#    checks before the loop runs (review, 2026-09-13): `${workspace}/bin`
#    itself must exist, and `$workspaceDrv` must be a readable file — see
#    below for why the second one matters.
# 2. `workspace`'s own `.drv` — the raw ATerm file nix instantiates before
#    ever building it, not its build output — must not contain the literal
#    string `--example` or `--all-targets` (the other cargo spelling that
#    pulls example targets into a build; #1120's own history is `doCheck =
#    true`, which appends `--all-targets` to the check command, not a
#    hand-written `--example` flag — see (3) below for the direct pin on
#    that). A derivation's env vars (`buildPhaseCargoCommand`, `postInstall`,
#    …) are serialised verbatim into that file, so a plain `grep` catches
#    either spelling added ANYWHERE in `workspace`'s own definition, without
#    needing the `nix` CLI or a build. This is the "cannot creep back in"
#    half: (1) alone would miss a stray `--example`/`--all-targets` build
#    that failed to actually install a binary into `$out/bin`, or installed
#    it under a different name.
# 3. `workspace`'s own `.drv` must not carry `doCheck=1` (serialised as the
#    literal `"doCheck","1"` in the ATerm — confirmed against a real drv,
#    2026-09-13; `doCheck=false`/unset serialises as `"doCheck",""`). Review
#    (2026-09-13) found that flipping `nix/package.nix`'s `workspace.doCheck`
#    back to `true` reproduces the exact pre-#1115 regression — `cargo test
#    --workspace` builds every example plus the whole dev-dependency graph,
#    on `cargoArtifactsBinOnly` (the cache that holds none) — while tripping
#    NEITHER of the two checks above: no `--example` string exists (`cargo
#    test`'s default target selection needs none), and crane's
#    `installFromCargoBuildLogHook` only captures what the **build** phase's
#    JSON log names, so the example binaries `cargo test`'s **check** phase
#    compiles never reach `$out/bin` either. This arm reads the one drv field
#    that shape does carry.
#
# Both `$workspaceDrv` checks below matter because `grep` exits 2 (not 1) on
# a missing/unreadable file, which `if grep -q …; then` cannot tell apart
# from "read fine, no match" — `set -e` does not fire on a condition command
# — so a `workspace.drvPath` that stopped resolving to a real file would
# leave every `grep` arm silently unmatched and the whole check green
# (review, 2026-09-13). Guarding it explicitly, before any `grep` runs, turns
# that into a loud failure instead. Same reasoning for `${workspace}/bin`:
# a missing directory makes the `[ -e … ]` loop below vacuously true too.
#
# Falsify:
# - arm 1/2 (bin contents / --example / --all-targets): move one
#   `cargoWithProfile build … --example probe` line back into `workspace`'s
#   own `buildPhaseCargoCommand` or a `postInstall` — both halves go red.
# - arm 3 (doCheck): flip `workspace`'s `doCheck = false` to `doCheck = true`
#   in `nix/package.nix` and instantiate (no need to build):
#   `nix derivation show .#trollshell.passthru.workspace` shows `"doCheck":
#   "1"`, and this check goes red on the same drv.
# - unreadable-input guard: point `workspaceDrv` at a path that doesn't
#   exist and confirm the check fails loudly instead of passing.
# Restore afterwards.
{
  runCommand,
  # The single whole-workspace compile (nix/package.nix's `passthru.workspace`).
  workspace,
}:
runCommand "workspace-ships-no-probes"
  {
    # `unsafeDiscardOutputDependency`: we want `workspace`'s `.drv` FILE
    # itself as a plain input source (always present, no build required to
    # write it), not a dependency on any of its OUTPUTS — the plain
    # `workspace.drvPath` string carries the latter kind of context, which is
    # what a bare `${workspace.drvPath}` interpolation asks Nix to satisfy,
    # and Nix has nothing built yet to satisfy it with under `nix flake check
    # --no-build`/eval-only contexts. Discarding that context is the standard
    # idiom for reading a sibling derivation's `.drv` without forcing it to
    # build.
    workspaceDrv = builtins.unsafeDiscardOutputDependency workspace.drvPath;
    meta.description = "the two nixosTest probe examples never creep back into the workspace compile (#1257)";
  }
  ''
    fail=0

    # Unreadable/missing inputs must fail loudly, not pass silently — a
    # `grep` on a missing file exits 2, which `if grep -q …; then` cannot
    # tell apart from "no match" (see the file header, 2026-09-13 review).
    if [ ! -f "$workspaceDrv" ]; then
      echo "FAIL: workspace's .drv is not readable at '$workspaceDrv' — this check cannot verify anything"
      exit 1
    fi

    if [ ! -d "${workspace}/bin" ]; then
      echo "FAIL: '${workspace}/bin' does not exist — this check cannot verify anything"
      exit 1
    fi

    for bin in probe wifi_probe; do
      if [ -e "${workspace}/bin/$bin" ]; then
        echo "FAIL: \$out/bin/$bin exists in workspace — an --example build reached the consumer compile; see nix/package.nix's probes derivation (#1257)"
        fail=1
      fi
    done

    if grep -q -- '--example' "$workspaceDrv"; then
      echo "FAIL: workspace's own .drv references --example — see nix/package.nix's probes derivation (#1257)"
      fail=1
    fi

    if grep -q -- '--all-targets' "$workspaceDrv"; then
      echo "FAIL: workspace's own .drv references --all-targets — cargo build/test --all-targets compiles examples too; see nix/package.nix's probes derivation (#1257)"
      fail=1
    fi

    if grep -q '"doCheck","1"' "$workspaceDrv"; then
      echo "FAIL: workspace's .drv has doCheck=1 — cargo test builds every example and the whole dev-dep graph on the bin-only cache (#1115/#1257)"
      fail=1
    fi

    [ "$fail" = 0 ] || exit 1
    touch $out
  ''
