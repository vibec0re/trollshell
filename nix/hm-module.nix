self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.trollshell;
in
{
  # enable / package / weather.fallbackCity are declared in the shared base.
  imports = [ (import ./module-common.nix self) ];

  # The systemd user service is home-manager-only, so it lives here.
  options.programs.trollshell.systemd = {
    enable = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Run trollshell as a systemd user service.";
    };

    target = lib.mkOption {
      type = lib.types.str;
      default = "graphical-session.target";
      example = "niri-session.target";
      description = "Systemd user target the shell service binds to.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    home.sessionVariables = lib.mkIf (cfg.weather.fallbackCity != null) {
      TROLLSHELL_WEATHER_CITY = cfg.weather.fallbackCity;
    };

    systemd.user.services.trollshell = lib.mkIf cfg.systemd.enable {
      Unit = {
        Description = "trollshell — bar, drawer, services";
        PartOf = [ cfg.systemd.target ];
        After = [ cfg.systemd.target ];
        Requisite = [ "graphical-session.target" ];
      };
      Service = {
        Type = "simple";
        ExecStart = lib.getExe cfg.package;
        Restart = "on-failure";
        RestartSec = 2;
        Slice = "session.slice";
      };
      Install.WantedBy = [ cfg.systemd.target ];
    };
  };
}
