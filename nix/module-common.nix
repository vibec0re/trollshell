# The programs.trollshell options shared by the NixOS and home-manager modules,
# so enable/package/weather.fallbackCity stay declared in one place instead of
# drifting between two near-identical copies. Each module imports this and adds
# its own platform-specific options (geoclue system-side, systemd user service
# home-side) plus the matching config.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
{
  options.programs.trollshell = {
    enable = lib.mkEnableOption "trollshell — hytte-based Wayland desktop shell";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.trollshell;
      defaultText = lib.literalExpression "trollshell.packages.\${system}.trollshell";
      description = "The trollshell package to install.";
    };

    weather.fallbackCity = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "Berlin";
      description = ''
        City the weather widget falls back to when geolocation is unavailable.
        Sets TROLLSHELL_WEATHER_CITY for the session. Leave null to rely on
        geoclue (enabled system-side by the NixOS module).
      '';
    };

    wallpaper.backend = lib.mkOption {
      type = lib.types.enum [
        "swaybg"
        "awww"
        "none"
      ];
      default = "swaybg";
      example = "awww";
      description = ''
        Which wallpaper daemon trollshell manages for the session, and what the
        Appearance picker tells to reload. One enum value drives the daemon
        wiring and the `reloadCommand` default together, so two daemons can never
        run at once — backend selection is structural, not an assertion.

        - `swaybg` (default — today's behavior): the bundled swaybg user unit;
          the Appearance picker restarts it (the shell's built-in default), so
          `reloadCommand` stays null.
        - `awww`: the swww successor (upstream renamed swww → awww at 0.12; swww
          itself is deprecated). Defaults `reloadCommand` to `awww img {}`. The
          daemon is run by home-manager's `services.awww`; the NixOS module only
          exports the reload command (a NixOS-only user without home-manager
          runs the awww daemon themselves).
        - `none`: manage no daemon at all — the Appearance picker only writes
          `~/.config/trollshell/wallpaper.path` (and runs `reloadCommand` if you
          set one yourself). Use this to wire your own daemon.

        The legacy `programs.trollshell.swaybg.enable` (home-manager) and a
        hand-set `wallpaper.reloadCommand` both still work and take precedence;
        they are gently deprecated in favor of this enum.
      '';
    };

    wallpaper.reloadCommand = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      # Per backend (`awww img {}` for awww). An option default, so any explicit
      # `reloadCommand = "…"` the user sets already wins over it (defaults are the
      # lowest merge priority). `{}` is the shell-quoted path, NOT a $VAR.
      default =
        {
          swaybg = null;
          awww = "awww img {}";
          none = null;
        }
        .${config.programs.trollshell.wallpaper.backend};
      defaultText = lib.literalExpression ''# per backend: null (swaybg/none) or "awww img {}" (awww)'';
      example = "awww img {}";
      description = ''
        Shell command the Appearance picker runs (via `sh -c`) after writing
        `~/.config/trollshell/wallpaper.path`, to tell your wallpaper daemon to
        reload. A `{}` in the command is replaced with the chosen path,
        shell-quoted. Sets TROLLSHELL_WALLPAPER_RELOAD_CMD for the session.

        Use the `{}` placeholder, not a `$VAR` reference: this value is delivered
        through sessionVariables, which expands `$`-references at login (before
        the path exists), so `awww img "$TROLLSHELL_WALLPAPER_PATH"` would expand
        to `awww img ""`. The path is still also exported as
        TROLLSHELL_WALLPAPER_PATH for daemons that read it directly.

        Defaults follow `wallpaper.backend`: null for `swaybg` (restart the
        bundled swaybg unit — the shell's built-in default) and `none`, and
        `awww img {}` for `awww`. Set it explicitly to override; an explicit
        value wins over the per-backend default.
      '';
    };

    niri.blurRules = lib.mkOption {
      type = lib.types.lines;
      readOnly = true;
      default = builtins.readFile (self + "/etc/niri/blur.kdl");
      description = ''
        The niri `layer-rule` blocks that enable trollshell's frosted-glass blur
        (bar / sidebar / drawer), sourced from the package's `etc/niri/blur.kdl`.
        niri has no `include`, so splice this into your niri config, e.g.:

            xdg.configFile."niri/config.kdl".text =
              myNiriConfigKdl + config.programs.trollshell.niri.blurRules;

        The sidebar/drawer rules use `xray true` (frost the wallpaper behind
        the overlay) for a consistent dark frost that matches the bar. Set a
        rule to `xray false` to frost the window behind it instead (pricier).
        Requires niri >= 26.04.
      '';
    };
  };
}
