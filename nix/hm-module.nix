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

  # Declarative plugin launch state (#419; the attr key is the plugin id).
  # programs.trollshell.plugins no longer emits one static
  # trollshell-plugin-<id> unit per entry (#350's original shape): the option
  # renders to a JSON state file the *shell* reads at startup — the host
  # launches each enabled plugin itself as a transient user unit via
  # `systemd-run --user` (trollshell/src/plugin_launcher.rs), which is also
  # where #392's secret injection hooks in at spawn. Every entry is written,
  # including enable = false ones ("enabled": false — declared but not
  # auto-launched), so a disabled plugin still lists in the control-center's
  # Plugins tab and can be started manually.
  pluginsState = builtins.toJSON {
    version = 1;
    plugins = lib.mapAttrs (_: plugin: {
      exec = lib.getExe plugin.package;
      inherit (plugin) env secrets;
      enabled = plugin.enable;
    }) cfg.plugins;
  };

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
  _file = "nix/hm-module.nix";

  # enable / package / weather.fallbackCity / wallpaper.* are declared in the
  # shared base.
  imports = [
    (import ./module-common.nix self)
    # #204 Phase 4: the idle → dim → lock → suspend pipeline is now native
    # (in-process in trollshell), so swayidle and its unit are retired. Keep the
    # removed option declared with a clear message rather than deleting it
    # silently, so a downstream config that still sets it gets a pointer instead
    # of a cryptic "option does not exist".
    (lib.mkRemovedOptionModule [ "programs" "trollshell" "swayidle" "enable" ] ''
      trollshell now runs the idle → dim → lock → suspend pipeline natively,
      in-process (#204); swayidle and its user unit have been retired. Remove
      this option — the native idle manager
      (crates/hytte-services/src/idle_notify.rs) replaces it and honors logind
      inhibitors (so the "Keep awake" toggle just works). See etc/README.md.
    '')
  ];

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
    all of the session-integration extras below (fuzzel, the wallpaper daemon,
    cliphist, portals) as a group. Each is set with mkDefault, so you can still
    flip an individual one off. The wallpaper daemon is the one selected by
    programs.trollshell.wallpaper.backend (default swaybg), not hardcoded — set
    backend = "none" to opt out of daemon management entirely'';

  options.programs.trollshell.fuzzel.enable = lib.mkEnableOption ''
    the bundled fuzzel launcher config via home-manager's programs.fuzzel.
    Bind Mod+D to `fuzzel` in niri yourself (etc/niri/binds.kdl)'';

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
          # Screen-recording flow (#403, crates/hytte-services/src/recorder.rs):
          # the bar's record-toggle chip spawns `wf-recorder` and picks a region
          # with `slurp` — both external tools the shell doesn't bundle. Neither
          # is behind a toggle (unlike enableSessionExtras' fuzzel/swayidle/etc.)
          # because the record chip is always present, the same reasoning as
          # wlsunset above; the screenshot flow provisions nothing to mirror
          # (niri captures its own screenshots — see the NixOS module's copy of
          # this comment). Missing binaries degrade gracefully — a logged
          # warning, no recording — but without this the feature silently does
          # nothing out of the box (#421).
          pkgs.wf-recorder
          pkgs.slurp
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
          })
          // (lib.optionalAttrs cfg.recorder.audioByDefault {
            # Arm the record chip's audio capture at session start (#403).
            # Only set when opted in — unset reads as off, and the env var is
            # the override, so Settings still flips it live during a session.
            TROLLSHELL_RECORD_AUDIO = "1";
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

      # Declarative plugins (#350, launch model #419): write the launch-state
      # file (built above) the shell reads at startup instead of generating
      # static units — the host runs each enabled plugin as a transient
      # trollshell-plugin-<id> user unit via `systemd-run --user`. Launched
      # plugins dial plugin.sock and register themselves as before; supervision
      # (Restart=on-failure) and session lifetime (PartOf=graphical-session
      # .target) ride on the transient unit. Only written when any plugin is
      # declared, so a plugin-less config grows no config file.
      (lib.mkIf (cfg.plugins != { }) {
        xdg.configFile."trollshell/plugins.json".text = pluginsState;
      })

      # Control-center companion app (#399): the external GTK settings/management
      # window (app-id mov.vibec0re.trollshell.ControlCenter) that speaks to the
      # shell's mov.vibec0re.trollshell.Control endpoint. Installed by default
      # into the user profile alongside the shell; its own toggle drops it.
      (lib.mkIf cfg.controlCenter.enable {
        home.packages = [ cfg.controlCenter.package ];
      })

      # Group switch: turn the whole extras bundle on, each via mkDefault so an
      # explicit per-feature `enable = false` still wins. The wallpaper daemon
      # is whichever wallpaper.backend selects (default swaybg) — backend picks
      # exactly one, so two daemons can never run at once.
      (lib.mkIf cfg.enableSessionExtras {
        programs.trollshell = {
          fuzzel.enable = lib.mkDefault true;
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

      # Idle pipeline: native since #204 Phase 4 — trollshell owns dim → lock →
      # suspend + before-sleep relock in-process (an ext-idle-notify-v1 client
      # gated on logind inhibitors), so there is no swayidle unit to wire here.
      # brightnessctl (for the dim step) is on PATH via the session; add it to
      # home.packages if your login shell doesn't already provide it.

      # swaybg — no home-manager module exists, so a plain user unit. It reads
      # the swaybg.args file the service renders at start; the Appearance drawer
      # page rewrites that file (per-output aware) and restarts this unit to
      # apply a new image. %h = home dir.
      (lib.mkIf cfg.swaybg.enable {
        systemd.user.services.swaybg = {
          Unit = {
            Description = "Wallpaper background via swaybg";
            Documentation = "man:swaybg(1)";
            PartOf = [ cfg.systemd.target ];
            After = [ cfg.systemd.target ];
            Requisite = [ cfg.systemd.target ];
            # Stay inactive until the Appearance picker has rendered a wallpaper —
            # the service writes swaybg.args (one swaybg arg per line) and removes
            # it on Clear, so its existence gates the unit.
            ConditionPathExists = "%h/.config/trollshell/swaybg.args";
          };
          Service = {
            Type = "simple";
            # Read swaybg.args a line at a time into the positional args (paths
            # with spaces survive), then exec swaybg with the full per-output
            # argument list the service derived.
            ExecStart = "${pkgs.bash}/bin/sh -c 'set --; while IFS= read -r a; do set -- \"$@\" \"$a\"; done < %h/.config/trollshell/swaybg.args; exec ${pkgs.swaybg}/bin/swaybg \"$@\"'";
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
