self:
{
  config,
  options,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.trollshell;
  backend = cfg.wallpaper.backend;
  # The awww backend is driven through home-manager's `services.awww` module
  # (the swww successor; swww is deprecated). It's guarded on existence and
  # asserted clearly if absent (below) rather than branching on option layout —
  # a pre-0.12 channel that still only ships `services.swww` won't have it.

  # One trollshell-plugin-<id> user service per programs.trollshell.plugins
  # entry (#350/#355; the attr key is the plugin id), mirroring
  # etc/systemd/user/trollshell-plugin-pet.service. Entries with
  # enable = false are filtered out before any unit is generated.
  pluginServices = lib.mapAttrs' (
    id: plugin:
    lib.nameValuePair "trollshell-plugin-${id}" {
      Unit = {
        PartOf = [ "niri-session.target" ];
        After = [
          "graphical-session.target"
          "trollshell.service"
        ];
        Requisite = [ "graphical-session.target" ];
      };
      Service = {
        Type = "simple";
        ExecStart = lib.getExe plugin.package;
        Environment = lib.mapAttrsToList (name: value: "${name}=${value}") plugin.env;
        Restart = "on-failure";
        RestartSec = 2;
      };
      Install.WantedBy = [ "niri-session.target" ];
    }
  ) (lib.filterAttrs (_: p: p.enable) cfg.plugins);

  # Night light (#222): the wlsunset user unit's ExecStart. Geo mode needs both
  # lat and lon; while either is unset the unit is declared but inert (starting
  # it just prints a hint and exits 0, so the shell's toggle never loops a
  # misconfigured daemon). Coordinates are stringified with toString — pass them
  # as strings to avoid the trailing zeros nix renders for floats.
  # NOTE: geoclue lat/lon seeding is a deferred follow-up; v1 is static coords.
  nl = cfg.nightlight;
  nlGeoConfigured = nl.latitude != null && nl.longitude != null;
  nlExecStart =
    if nlGeoConfigured then
      "${pkgs.wlsunset}/bin/wlsunset -l ${toString nl.latitude} -L ${toString nl.longitude} -t ${toString nl.nightTemp} -T ${toString nl.dayTemp}"
    else
      "${pkgs.bash}/bin/sh -c 'echo \"wlsunset: programs.trollshell.nightlight.{latitude,longitude} are unset — set them to enable the Night light toggle\" >&2; exit 0'";
in
{
  # enable / package / weather.fallbackCity / wallpaper.* are declared in the
  # shared base.
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
    all of the session-integration extras below (fuzzel, swayidle, the
    wallpaper daemon, cliphist, portals) as a group. Each is set with mkDefault,
    so you can still flip an individual one off. The wallpaper daemon is the one
    selected by programs.trollshell.wallpaper.backend (default swaybg), not
    hardcoded — set backend = "none" to opt out of daemon management entirely'';

  options.programs.trollshell.fuzzel.enable = lib.mkEnableOption ''
    the bundled fuzzel launcher config via home-manager's programs.fuzzel.
    Bind Mod+D to `fuzzel` in niri yourself (etc/niri/binds.kdl)'';

  options.programs.trollshell.swayidle.enable = lib.mkEnableOption ''
    the idle pipeline (dim at 4 min, lock at 5, suspend at 10, lock before
    sleep) via home-manager's services.swayidle'';

  options.programs.trollshell.swaybg.enable = lib.mkEnableOption ''
    the bundled swaybg wallpaper user unit standalone, which reads the image
    path from ~/.config/trollshell/wallpaper.path (the Appearance drawer page
    writes it). Gently deprecated in favor of
    programs.trollshell.wallpaper.backend = "swaybg" + enableSessionExtras: the
    extras bundle now starts the swaybg unit when backend is "swaybg" (the
    default). This option is still honored — set it true to run the swaybg unit
    without the rest of the bundle. Two wallpaper daemons can never run at once
    because backend selects exactly one, but if you set this true AND
    backend = "awww", swaybg and awww would both start; don't.
    Leave it false unless you specifically want swaybg without the bundle'';

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
          # Night light daemon (#222): the wlsunset.service unit below drives it;
          # also on PATH so the user can invoke wlsunset directly.
          pkgs.wlsunset
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
            Requisite = [ cfg.systemd.target ];
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

      # Declarative out-of-tree plugins (#350): one trollshell-plugin-<id>
      # user service per programs.trollshell.plugins entry (built above).
      # Plugins dial plugin.sock and register themselves; this only wires
      # the unit — no shell-side spawn/supervise (trollshell/src/plugins.rs)
      # and no runtime load/unload (that's #348, out of scope here).
      { systemd.user.services = pluginServices; }

      # Group switch: turn the whole extras bundle on, each via mkDefault so an
      # explicit per-feature `enable = false` still wins. The wallpaper daemon
      # is whichever wallpaper.backend selects (default swaybg) — backend picks
      # exactly one, so two daemons can never run at once.
      (lib.mkIf cfg.enableSessionExtras {
        programs.trollshell = {
          fuzzel.enable = lib.mkDefault true;
          swayidle.enable = lib.mkDefault true;
          # Start the swaybg unit only when it's the chosen backend AND no
          # explicit reloadCommand is set. A hand-set reloadCommand is the
          # pre-enum way to drive another daemon (e.g. `awww img {}`); honoring
          # it here means an existing config that set reloadCommand but not
          # `backend` (which defaults to "swaybg") doesn't get swaybg started
          # alongside its own daemon. awww proper goes through services.awww.
          swaybg.enable = lib.mkIf (backend == "swaybg" && cfg.wallpaper.reloadCommand == null) (
            lib.mkDefault true
          );
          cliphist.enable = lib.mkDefault true;
          portals.enable = lib.mkDefault true;
        };
        # awww goes through home-manager's own services.awww module (which
        # manages the daemon unit + has its own package option). Guarded on the
        # module actually existing so an absent module surfaces as the clear
        # assertion below rather than a raw "option does not exist" error.
        services = lib.mkIf (backend == "awww" && options.services ? awww) {
          awww.enable = lib.mkDefault true;
        };
      })

      # Naming-wrinkle guard: the awww backend needs home-manager's
      # `services.awww` module, which only exists on 0.12+ channels (upstream
      # renamed swww → awww; swww is deprecated). We assert with a clear message
      # rather than silently branch on option layout. A pre-0.12 channel that
      # still only ships `services.swww` won't have `services.awww` — upgrade,
      # or use backend = "none" and wire the daemon yourself.
      (lib.mkIf (cfg.enableSessionExtras && backend == "awww") {
        assertions = [
          {
            assertion = options.services ? awww;
            message = ''
              programs.trollshell.wallpaper.backend = "awww", but your
              home-manager channel has no `services.awww` module to manage the
              daemon. awww is the 0.12+ successor of the now-deprecated swww;
              older channels ship it as `services.swww`. Upgrade home-manager to
              a channel with `services.awww`, or set
              programs.trollshell.wallpaper.backend = "none" and wire the daemon
              yourself.
            '';
          }
        ];
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
            Requisite = [ cfg.systemd.target ];
            # Stay inactive until the Appearance picker has written a wallpaper
            # path; otherwise ExecStart's `cat` yields empty, swaybg fails, and
            # Restart=on-failure loops it on a fresh install.
            ConditionPathExists = "%h/.config/trollshell/wallpaper.path";
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

      # wlsunset — night light (#222). No home-manager module exists, so a plain
      # user unit mirroring swaybg. The shell toggles it on demand via
      # `systemctl --user start|stop wlsunset.service` (nightlight.rs), so there
      # is deliberately NO Install/WantedBy — it defaults to inactive and the
      # Appearance drawer's Night light switch brings it up. Always declared (so
      # the toggle target exists); inert while lat/lon are unset (see nlExecStart)
      # rather than hard-failing evaluation. geoclue lat/lon seeding is deferred.
      {
        systemd.user.services.wlsunset = {
          Unit = {
            Description = "Night light (color temperature) via wlsunset";
            Documentation = "man:wlsunset(1)";
            PartOf = [ cfg.systemd.target ];
            After = [ cfg.systemd.target ];
            Requisite = [ cfg.systemd.target ];
          };
          Service = {
            Type = "simple";
            ExecStart = nlExecStart;
            # Only auto-restart the real daemon; the inert hint exits 0 and must
            # not loop.
            Restart = if nlGeoConfigured then "on-failure" else "no";
            RestartSec = 2;
          };
          # No Install section — the shell starts/stops it (see comment above).
        };
      }

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
