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
  };
}
