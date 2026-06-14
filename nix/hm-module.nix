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

  # Optional session-integration extras (the pieces documented under etc/). Each
  # is off by default and, where home-manager already ships a module for the
  # tool, drives that module with mkDefault values so you can still override any
  # individual setting. niri keybinds/session live in KDL and have no clean
  # home-manager path, so they stay manual — see etc/niri/.
  options.programs.trollshell.fuzzel.enable = lib.mkEnableOption ''
    the bundled fuzzel launcher config via home-manager's programs.fuzzel.
    Bind Mod+D to `fuzzel` in niri yourself (etc/niri/binds.kdl)'';

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
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
      }

      # fuzzel — config-only launcher (niri spawns it on a chord, so no unit).
      # Goes through programs.fuzzel so every key stays individually overridable.
      (lib.mkIf cfg.fuzzel.enable {
        programs.fuzzel = {
          enable = lib.mkDefault true;
          settings = {
            main = {
              # Quoted so the trailing space in the prompt survives.
              prompt = lib.mkDefault ''"> "'';
              width = lib.mkDefault 40;
              lines = lib.mkDefault 10;
              horizontal-pad = lib.mkDefault 20;
              vertical-pad = lib.mkDefault 10;
              inner-pad = lib.mkDefault 8;
              fields = lib.mkDefault "filename,name,generic,comment";
              match-mode = lib.mkDefault "fuzzy";
            };
            border = {
              radius = lib.mkDefault 12;
              width = lib.mkDefault 2;
            };
          };
        };
      })
    ]
  );
}
