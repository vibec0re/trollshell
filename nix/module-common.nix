# The programs.trollshell options shared by the NixOS and home-manager modules,
# so enable/package/weather.fallbackCity stay declared in one place instead of
# drifting between two near-identical copies. Each module imports this and adds
# its own platform-specific options (geoclue system-side, systemd user service
# home-side) plus the matching config.
self:
{ lib, pkgs, ... }:
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

    wallpaper.reloadCommand = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
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

        Leave null to keep the default — restart the bundled swaybg user unit.
        Set it to drive a different daemon, e.g. swww/awww: `awww img {}`.
        Setting it also tells the home-manager `enableSessionExtras` bundle not
        to start swaybg, which would otherwise fight your daemon over the
        wallpaper layer.
      '';
    };
  };
}
