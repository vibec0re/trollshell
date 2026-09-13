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
#    direct, observable regression #1257 reported.
# 2. `workspace`'s own `.drv` — the raw ATerm file nix instantiates before
#    ever building it, not its build output — must not contain the literal
#    string `--example`. A derivation's env vars (`buildPhaseCargoCommand`,
#    `postInstall`, …) are serialised verbatim into that file, so a plain
#    `grep` catches an `--example` build added ANYWHERE in `workspace`'s own
#    definition, without needing the `nix` CLI or a build. This is the
#    "cannot creep back in" half: (1) alone would miss a stray `--example`
#    build that failed to actually install a binary into `$out/bin`, or
#    installed it under a different name.
#
# Falsify: move one `cargoWithProfile build … --example probe` line back into
# `workspace`'s own `buildPhaseCargoCommand` or a `postInstall` — both halves
# of this check go red. Restore afterwards.
{
  runCommand,
  # The single whole-workspace compile (nix/package.nix's `passthru.workspace`).
  workspace,
}:
runCommand "workspace-ships-no-probes"
  {
    meta.description = "the two nixosTest probe examples never creep back into the workspace compile (#1257)";
  }
  ''
    fail=0

    for bin in probe wifi_probe; do
      if [ -e "${workspace}/bin/$bin" ]; then
        echo "FAIL: \$out/bin/$bin exists in workspace — an --example build reached the consumer compile; see nix/package.nix's probes derivation (#1257)"
        fail=1
      fi
    done

    if grep -q -- '--example' "${workspace.drvPath}"; then
      echo "FAIL: workspace's own .drv references --example — see nix/package.nix's probes derivation (#1257)"
      fail=1
    fi

    [ "$fail" = 0 ] || exit 1
    touch $out
  ''
