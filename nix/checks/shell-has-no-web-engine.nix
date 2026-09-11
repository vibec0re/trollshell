# The shell's runtime closure must carry **no WebKitGTK 6.0** (#1130 M1).
#
# `crates/trollshell-agent-window` is the one workspace member that links a web
# engine, and it ships as its own package. But there is one crane compile for
# the whole workspace (#572/#587), so `webkitgtk_6_0` has to be in *that*
# derivation's `buildInputs` — and `buildInputs` was also the list every GTK
# slice handed to `wrapGAppsHook4`. `wrap-gapps-hook.sh` propagates
# `GI_TYPELIB_PATH` verbatim, webkitgtk ships three `-6.0.typelib` files, so
# the wrapper script named the store path and the shell grew a **167 MiB**
# runtime dependency on an engine it never loads. Measured, on the #1130 branch
# before the fix: `nix why-depends .#trollshell …webkitgtk-…abi=6.0` reported a
# DIRECT reference.
#
# nix/package.nix now splits `buildInputs` (what a wrapper sees) from
# `webInputs` (what the compile, the devShell and the agent-window slice see).
# This asserts the result where it can actually be observed — the closure —
# rather than trusting the split to stay right.
#
# **It names the ABI on purpose.** `webkitgtk-…abi=4.1` is already in the
# shell's closure through evolution-data-server, so a bare `webkitgtk` grep is
# red on `main` and would have to be "fixed" by loosening it, which is how a
# check stops meaning anything.
#
# `exportReferencesGraph` gives nix's own answer about the runtime closure, not
# a re-derivation of it: the file it writes is the store paths the output
# references, transitively.
{
  runCommand,
  # The wrapped shell package, i.e. what a `nixos-rebuild` would install.
  trollshell,
  # The companion app, wrapped the same way and with the same exposure.
  trollshell-control-center,
}:
runCommand "shell-has-no-web-engine"
  {
    exportReferencesGraph = [
      "shell-closure"
      trollshell
      "control-center-closure"
      trollshell-control-center
    ];
    meta.description = "the shell and the control center carry no WebKitGTK 6.0 at runtime (#1130)";
  }
  ''
    fail=0
    for closure in shell-closure control-center-closure; do
      if grep -q 'webkitgtk-[^ ]*abi=6\.0' "$closure"; then
        echo "FAIL: $closure carries WebKitGTK 6.0 — see nix/package.nix's webInputs split"
        grep -o '/nix/store/[^ ]*webkitgtk-[^ ]*abi=6\.0[^ ]*' "$closure" | sort -u
        fail=1
      fi
    done

    # …and the negative control: 4.1 IS expected (evolution-data-server pulls
    # it), so a grep that found nothing at all would mean the closure file is
    # not what this check thinks it is.
    if ! grep -q 'webkitgtk' shell-closure; then
      echo "FAIL: no webkitgtk of any ABI in the shell's closure — this check is reading the"
      echo "      wrong file, or evolution-data-server stopped pulling 4.1. Either way the"
      echo "      assertion above is no longer proving anything."
      fail=1
    fi

    [ "$fail" = 0 ] || exit 1
    touch $out
  ''
