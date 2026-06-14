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
  options.programs.trollshell.enableSessionExtras = lib.mkEnableOption ''
    all of the session-integration extras below (fuzzel, swayidle, swaybg,
    cliphist, portals) as a group. Each is set with mkDefault, so you can still
    flip an individual one off (e.g. programs.trollshell.swaybg.enable = false)'';

  options.programs.trollshell.fuzzel.enable = lib.mkEnableOption ''
    the bundled fuzzel launcher config via home-manager's programs.fuzzel.
    Bind Mod+D to `fuzzel` in niri yourself (etc/niri/binds.kdl)'';

  options.programs.trollshell.swayidle.enable = lib.mkEnableOption ''
    the idle pipeline (dim at 4 min, lock at 5, suspend at 10, lock before
    sleep) via home-manager's services.swayidle'';

  options.programs.trollshell.swaybg.enable = lib.mkEnableOption ''
    the swaybg wallpaper service. Reads the image path from
    ~/.config/trollshell/wallpaper.path, which the Appearance drawer page writes'';

  options.programs.trollshell.swww.enable = lib.mkEnableOption ''
    swww/awww as the wallpaper backend instead of swaybg. This only points
    `wallpaper.reloadCommand` at `awww img {}` (so the Appearance picker reloads
    via the daemon) and, because that sets reloadCommand, disables the bundled
    swaybg auto-start — the two never run together. It does NOT enable the
    daemon module for you: the home-manager option is `services.swww` before the
    upstream 0.12 rename and `services.awww` after, and trollshell can't know
    which one your home-manager channel defines, so you enable your own
    `services.awww`/`services.swww` (and adjust reloadCommand if your binary
    isn't `awww`)'';

  options.programs.trollshell.cliphist.enable = lib.mkEnableOption ''
    clipboard history (text + images) via home-manager's services.cliphist,
    which feeds the Clipboard drawer page'';

  options.programs.trollshell.portals.enable = lib.mkEnableOption ''
    the niri xdg-desktop-portal routing + backends via home-manager's
    xdg.portal. You still set XDG_CURRENT_DESKTOP=niri:GNOME yourself'';

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        # cfg.package plus the fonts the stylesheets name (Inter + Cantarell for
        # the bar UI, JetBrains Mono / Fira Code for the clock + workspace chips)
        # — the NixOS module puts these in fonts.packages; here they go through
        # the user profile, so fontconfig must be on to discover them.
        home.packages = [
          cfg.package
          pkgs.inter
          pkgs.cantarell-fonts
          pkgs.jetbrains-mono
          pkgs.fira-code
        ];
        fonts.fontconfig.enable = lib.mkDefault true;

        # Session vars, each set only when its option is non-null. optionalAttrs
        # + // keeps the two independent (mkIf on a whole attrset would force an
        # all-or-nothing block).
        home.sessionVariables =
          (lib.optionalAttrs (cfg.weather.fallbackCity != null) {
            TROLLSHELL_WEATHER_CITY = cfg.weather.fallbackCity;
          })
          // (lib.optionalAttrs (cfg.wallpaper.reloadCommand != null) {
            # Appearance picker reload command; null = the shell's swaybg default.
            TROLLSHELL_WALLPAPER_RELOAD_CMD = cfg.wallpaper.reloadCommand;
          });

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

      # Group switch: turn the whole extras bundle on, each via mkDefault so an
      # explicit per-feature `enable = false` still wins.
      (lib.mkIf cfg.enableSessionExtras {
        programs.trollshell = {
          fuzzel.enable = lib.mkDefault true;
          swayidle.enable = lib.mkDefault true;
          # Only auto-start swaybg when no custom reload command is set —
          # otherwise the bundle would launch swaybg alongside the swww/awww (or
          # other) daemon that reloadCommand points at, and the two fight over
          # the wallpaper layer.
          swaybg.enable = lib.mkDefault (cfg.wallpaper.reloadCommand == null);
          cliphist.enable = lib.mkDefault true;
          portals.enable = lib.mkDefault true;
        };
      })

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

      # swayidle — idle dim/lock/suspend pipeline via home-manager's module.
      # Commands use absolute store paths because swayidle's unit only puts a
      # shell on PATH. mkDefault keeps the timeouts/events overridable wholesale.
      (lib.mkIf cfg.swayidle.enable {
        services.swayidle = {
          enable = lib.mkDefault true;
          timeouts = lib.mkDefault [
            {
              timeout = 240;
              command = "${lib.getExe pkgs.brightnessctl} -s set 10%";
              resumeCommand = "${lib.getExe pkgs.brightnessctl} -r";
            }
            {
              timeout = 300;
              command = "${pkgs.systemd}/bin/loginctl lock-session";
            }
            {
              timeout = 600;
              command = "${pkgs.systemd}/bin/systemctl suspend";
            }
          ];
          # loginctl lock-session → logind Lock → trollshell's lock surface.
          events.before-sleep = lib.mkDefault "${pkgs.systemd}/bin/loginctl lock-session";
        };
      })

      # swaybg — no home-manager module exists, so a plain user unit. It reads
      # the wallpaper path at start; the Appearance drawer page rewrites that
      # file and restarts this unit to apply a new image. %h = home dir.
      (lib.mkIf cfg.swaybg.enable {
        systemd.user.services.swaybg = {
          Unit = {
            Description = "Wallpaper background via swaybg";
            Documentation = "man:swaybg(1)";
            PartOf = [ cfg.systemd.target ];
            After = [ cfg.systemd.target ];
            Requisite = [ "graphical-session.target" ];
          };
          Service = {
            Type = "simple";
            ExecStart = "${pkgs.bash}/bin/sh -c 'exec ${pkgs.swaybg}/bin/swaybg -i \"$(${pkgs.coreutils}/bin/cat %h/.config/trollshell/wallpaper.path)\" -m fill'";
            Restart = "on-failure";
            RestartSec = 2;
          };
          Install.WantedBy = [ cfg.systemd.target ];
        };
      })

      # swww/awww — backend selector, not a daemon unit. It only points the
      # Appearance picker's reload command at the daemon; because that sets
      # reloadCommand, the enableSessionExtras bundle stops auto-starting swaybg
      # (see the swaybg.enable mkDefault above), so the two never fight over the
      # wallpaper layer. mkDefault keeps an explicit reloadCommand override
      # winning. The daemon module itself (services.swww vs services.awww — the
      # 0.12 rename) is the user's to enable; trollshell can't pick the name for
      # them without breaking the other home-manager release.
      (lib.mkIf cfg.swww.enable {
        programs.trollshell.wallpaper.reloadCommand = lib.mkDefault "awww img {}";
      })

      # cliphist — clipboard history via home-manager's module. allowImages
      # defaults true, so this starts both the text and image wl-paste watchers.
      (lib.mkIf cfg.cliphist.enable {
        services.cliphist.enable = lib.mkDefault true;
      })

      # portals — niri's xdg-desktop-portal routing via home-manager's xdg.portal
      # module. config.niri is rendered to niri-portals.conf; extraPortals pulls
      # in the backends. mkDefault throughout keeps every piece overridable.
      (lib.mkIf cfg.portals.enable {
        xdg.portal = {
          enable = lib.mkDefault true;
          extraPortals = lib.mkDefault [
            pkgs.xdg-desktop-portal-gnome
            pkgs.xdg-desktop-portal-wlr
          ];
          config.niri = {
            default = lib.mkDefault [
              "gnome"
              "gtk"
            ];
            "org.freedesktop.impl.portal.FileChooser" = lib.mkDefault [
              "gnome"
              "gtk"
            ];
            "org.freedesktop.impl.portal.Settings" = lib.mkDefault [
              "gnome"
              "gtk"
            ];
            "org.freedesktop.impl.portal.Screenshot" = lib.mkDefault [ "wlr" ];
            "org.freedesktop.impl.portal.ScreenCast" = lib.mkDefault [ "wlr" ];
          };
        };
      })
    ]
  );
}
