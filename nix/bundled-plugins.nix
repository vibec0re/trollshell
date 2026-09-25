# The bundled widget plugins (#558), by crate = binary = flake-output name.
#
# One list, read by two sides that must agree (#1400):
#
#   - flake.nix (`bundledPluginNames`): each name is a `packages.<name>`
#     output (nix/plugin.nix, a `cp` of one binary out of the single
#     whole-workspace compile) and a `checks` build.
#   - nix/module-common.nix: each name, stripped of `hytte-plugin-`, is a
#     bundled plugin id — the default of `programs.trollshell.availablePlugins`
#     and the ids whose `plugins.<id>.package` defaults to that output.
#
# Its own file rather than a `let` in flake.nix because the modules only
# receive the flake's `self`, and a flake output carrying a plain list would
# be an output nix's own schema does not know.
#
# `hytte-plugin-proto` (the wire-protocol lib) and `hytte-plugin` (the SDK)
# are not plugins and are deliberately absent; so are `hytte-infobroker` (the
# broker's CLI, not a widget) and `hytte-claude-bridge` (driven by
# `programs.trollshell.claudeBridge`, and not named `hytte-plugin-*`).
[
  "hytte-plugin-agents"
  "hytte-plugin-audio-widget"
  "hytte-plugin-caw"
  # One binary meant to be listed twice in `programs.trollshell.plugins`
  # (#1388, on `hytte-plugin-stats`' shape below): the sidebar card its
  # manifest mounts, and — with `mount = "BarCenter"` — the bar chip that
  # used to be a second crate, `hytte-plugin-bar-clock-demo`.
  "hytte-plugin-clock-demo"
  "hytte-plugin-departures"
  "hytte-plugin-infobroker"
  "hytte-plugin-niri-layouts"
  "hytte-plugin-pet"
  "hytte-plugin-preem-demo"
  # One binary meant to be listed twice in `programs.trollshell.plugins`
  # (#1250): a bar instance and a right-sidebar one, distinguished by
  # `HYTTE_PLUGIN_ID` + `HYTTE_PLUGIN_MOUNT` on the launch. That is a
  # deployment shape, not a packaging one — there is still exactly one
  # slice here, and `plugins.<id>.package` points both entries at it.
  "hytte-plugin-stats"
  "hytte-plugin-terminal"
  "hytte-plugin-timer"
  "hytte-plugin-weather"
]
