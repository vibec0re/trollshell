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

  # Night light (#657): the four programs.trollshell.nightlight.* options are
  # declared in the shared base (module-common.nix) purely so they render in
  # one options doc, but they are home-manager-only — the wlsunset unit they
  # parameterise is only declared by nix/hm-module.nix, and this module has no
  # unit of its own for them to reach. Setting any of them here would
  # therefore evaluate cleanly and silently do nothing, so rather than let
  # that happen we assert below (matching the awww-channel assertion in
  # hm-module.nix — the standard NixOS module-system assertions mechanism,
  # not something bespoke). Compared against the option defaults declared in
  # module-common.nix, same idiom as `nlGeoConfigured` in hm-module.nix.
  nlConfigured =
    cfg.nightlight.latitude != null
    || cfg.nightlight.longitude != null
    || cfg.nightlight.dayTemp != 6500
    || cfg.nightlight.nightTemp != 4000;

  # Declarative plugin launch state (#419; the attr key is the plugin id).
  # programs.trollshell.plugins no longer emits one static
  # trollshell-plugin-<id> unit per entry (#350's original shape): the option
  # renders to a JSON state file the *shell* reads at startup — the host
  # launches each enabled plugin itself as a transient user unit via
  # `systemd-run --user` (trollshell/src/plugin_launcher.rs), which is also
  # where #392's secret injection hooks in at spawn. Every entry is written,
  # including enable = false ones ("enabled": false — declared but not
  # auto-launched), so a disabled plugin still lists in the control-center's
  # Plugins tab and can be started manually. Same JSON as the home-manager
  # module builds, minus its top-level "target" key (#707): that renders
  # `programs.trollshell.systemd.target`, a home-manager-only option — this
  # module ships no shell user unit of its own, so it has no session target to
  # bind plugins to. The key is optional on the shell side and its absence means
  # graphical-session.target, which is exactly what this module wants. Keep the
  # rest of the two in sync.
  pluginsState = builtins.toJSON {
    version = 1;
    plugins = lib.mapAttrs (_: plugin: {
      exec = lib.getExe plugin.package;
      inherit (plugin) env secrets;
      enabled = plugin.enable;
    }) cfg.plugins;
  };
in
{
  _file = "nix/nixos-module.nix";

  # enable / package / weather.fallbackCity / wallpaper.* are declared in the
  # shared base.
  imports = [ (import ./module-common.nix self) ];

  options.programs.trollshell.enableRecommendedServices = lib.mkOption {
    type = lib.types.bool;
    default = true;
    description = ''
      Enable the system daemons trollshell's optional chips lean on (UPower,
      power-profiles-daemon, geoclue, plus the polkit agent and the agent-name
      D-Bus policy). Each is still individually overridable; this is a master
      switch for the lot. Turning it off leaves a working bar — the chips that
      back onto a missing daemon simply hide themselves.
    '';
  };

  options.programs.trollshell.enableRecommendedSoftware = lib.mkOption {
    type = lib.types.bool;
    default = true;
    description = ''
      Install the optional GNOME desktop apps that pair with the GOA →
      evolution-data-server stack trollshell reads: GNOME Calendar (a
      read/write UI for the same calendars trollshell's Calendar page shows
      read-only), GNOME Tasks (Endeavour, for the EDS task lists), and GNOME
      Contacts. Kept separate from `enableRecommendedServices` because these
      are heavier GUI apps rather than daemons — turn this off to keep the
      services + the `gnome-control-center` account-add UI while dropping the
      apps. (`gnome-control-center` itself stays under the services switch,
      since it's the essential way to add an account.)
    '';
  };

  # geoclue is system-only, so it lives here rather than in the shared base.
  options.programs.trollshell.weather.geoclue = {
    enable = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Enable geoclue2 auto-location for the weather widget (and future location-aware features).";
    };

    providerUrl = lib.mkOption {
      type = lib.types.str;
      default = "https://api.beacondb.net/v1/geolocate";
      description = ''
        WiFi-positioning backend for geoclue. Mozilla Location
        Service (geoclue's historical default) shut down in 2024;
        beaconDB is the community successor.
      '';
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        environment.systemPackages = [ cfg.package ];

        # Fonts the stylesheets name explicitly: Inter + Cantarell for the bar
        # UI (assets/hytte-ui/style.css, assets/trollshell/style.css), JetBrains Mono /
        # Fira Code for the clock + workspace chips. Without them fontconfig
        # silently falls back and the bar renders in the wrong typeface.
        fonts.packages = [
          pkgs.inter
          pkgs.cantarell-fonts
          pkgs.jetbrains-mono
          pkgs.fira-code
        ];

        # Stats-drawer layout (#508). Always exported (not mkIf): the value is a
        # total enum whose default equals the shell's own runtime default
        # ("combined"), so setting it explicitly is harmless and keeps the
        # session env self-describing.
        environment.sessionVariables.TROLLSHELL_STATS_LAYOUT = cfg.stats.layout;

        # Weather widget location fallback. Stays outside the recommended-
        # services switch on purpose: it's the manual alternative to geoclue,
        # so it must keep working when auto-location is turned off. weather
        # forward-geocodes this city when GeoClue2 is absent.
        environment.sessionVariables.TROLLSHELL_WEATHER_CITY = lib.mkIf (
          cfg.weather.fallbackCity != null
        ) cfg.weather.fallbackCity;

        # Desktop owner's name (#696/#813), session-wide so pet and caw's LLM
        # personas both pick it up without per-plugin env wiring. null means
        # "unset entirely" — both plugins fall back to their neutral "your
        # human" default (`hytte_ai_providers::owner()`), and an empty string
        # would be noise (owner_or treats blank the same as unset anyway).
        environment.sessionVariables.TROLLSHELL_OWNER = lib.mkIf (
          cfg.ownerName != null
        ) cfg.ownerName;

        # Wallpaper reload command for the Appearance picker. Defaults follow
        # wallpaper.backend: null for swaybg (restart swaybg.service — the
        # shell's built-in default) and none; `awww img {}` for awww. The chosen
        # path reaches the command as TROLLSHELL_WALLPAPER_PATH.
        environment.sessionVariables.TROLLSHELL_WALLPAPER_RELOAD_CMD = lib.mkIf (
          cfg.wallpaper.reloadCommand != null
        ) cfg.wallpaper.reloadCommand;

        # Screen-recording audio default (#403): arm the record chip's audio
        # toggle at session start. mkIf so it's only exported when opted in —
        # unset reads as off, and the env var is the override (Settings still
        # flips it live during a session).
        environment.sessionVariables.TROLLSHELL_RECORD_AUDIO = lib.mkIf cfg.recorder.audioByDefault "1";
      }

      # Wallpaper daemon (NixOS side): only swaybg is managed here. swaybg is the
      # bundled default and reads the swaybg.args file the service renders (the
      # Appearance picker restarts this unit). The awww backend's daemon is run by
      # home-manager's services.awww — a NixOS-only user without home-manager
      # runs the awww daemon themselves — so for awww the NixOS module just
      # exports the reload command above. backend = "none" manages nothing.
      # Also skip swaybg when an explicit reloadCommand is set: that's the
      # pre-enum way to drive another daemon, and a config that set it but not
      # `backend` (default "swaybg") must not get swaybg started over its daemon.
      (lib.mkIf (backend == "swaybg" && cfg.wallpaper.reloadCommand == null) {
        systemd.user.services.swaybg = {
          description = "trollshell wallpaper daemon (swaybg)";
          wantedBy = [ "graphical-session.target" ];
          partOf = [ "graphical-session.target" ];
          after = [ "graphical-session.target" ];
          # Stay inactive until the Appearance picker has rendered a wallpaper —
          # the service writes swaybg.args (one swaybg arg per line, per-output
          # aware) and removes it on Clear, so its existence gates the unit.
          unitConfig.ConditionPathExists = "%h/.config/trollshell/swaybg.args";
          serviceConfig = {
            Type = "simple";
            # Read swaybg.args a line at a time into the positional args (paths
            # with spaces survive), then exec swaybg with the full per-output
            # argument list the service derived.
            ExecStart = "${pkgs.bash}/bin/sh -c 'set --; while IFS= read -r a; do set -- \"$@\" \"$a\"; done < %h/.config/trollshell/swaybg.args; exec ${pkgs.swaybg}/bin/swaybg \"$@\"'";
            Restart = "on-failure";
            RestartSec = 2;
          };
        };
      })
      # Declarative plugins (#350, launch model #419): write the launch-state
      # file (built above) under /etc/xdg — the default $XDG_CONFIG_DIRS entry
      # the shell's launcher falls back to — instead of generating static
      # units; the host runs each enabled plugin as a transient
      # trollshell-plugin-<id> user unit via `systemd-run --user`. A per-user
      # home-manager file ($XDG_CONFIG_HOME/trollshell/plugins.json) fully
      # shadows this system one (first existing file wins whole, no merge).
      # Note `programs.trollshell.plugins` set at NixOS system level is a
      # *separate* declaration from any home-manager per-user
      # `programs.trollshell.plugins` (home-manager.sharedModules below only
      # shares the module definition, not config values) — set it wherever
      # you actually run the shell.
      (lib.mkIf (cfg.plugins != { }) {
        environment.etc."xdg/trollshell/plugins.json".text = pluginsState;
      })

      # Night light (#657): see nlConfigured above. Assert rather than let the
      # four nightlight.* options evaluate to a no-op — the NixOS module
      # declares no wlsunset package or unit, so nothing here ever reads
      # them. A clear failure at eval time beats a bar switch that silently
      # does nothing at runtime.
      (lib.mkIf nlConfigured {
        assertions = [
          {
            assertion = false;
            message = ''
              programs.trollshell.nightlight.* is set, but night light
              (wlsunset) is currently implemented only by the home-manager
              module — nix/nixos-module.nix declares no wlsunset package or
              systemd user unit, so this setting has no effect here: the
              shell's Night light switch still renders (the service starts
              unconditionally) and does nothing when toggled. See #657. Move
              this configuration to your home-manager
              programs.trollshell.nightlight config instead, or drop it.
            '';
          }
        ];
      })

      # The two LLM backend units (#694): programs.trollshell.claudeBridge.* and
      # .petBrain.* are declared in the shared base (module-common.nix) so they
      # render in one options doc, but — like nightlight above — they are
      # home-manager-only. nix/hm-module.nix declares both user units; this
      # module declares neither, and a NixOS-only deployment has no per-user
      # `claude` login state (nor a per-user model download) for them to drive.
      # So assert rather than let `enable = true` evaluate cleanly and start
      # nothing.
      #
      # Unconditional, not behind an `lib.mkIf`, so the flake's `nixos-module`
      # check forces both predicates on every run — see the note at that check
      # about `mkIf`-guarded assertions being invisible to it (#681). Only
      # `enable` is inspected: setting `port` or `model` without enabling is
      # inert here the same way it is anywhere else.
      {
        assertions = [
          {
            assertion = !cfg.claudeBridge.enable;
            message = ''
              programs.trollshell.claudeBridge.enable is set, but the
              hytte-claude-bridge user service is currently implemented only by
              the home-manager module — nix/nixos-module.nix declares no unit
              for it, so this setting has no effect here and nothing would
              listen on the loopback port your plugins point at. The bridge
              drives a per-user, already-logged-in `claude` CLI, which a
              system-level module has no handle on. See #694. Move this to your
              home-manager programs.trollshell.claudeBridge config, or install
              the unit by hand from
              etc/systemd/user/trollshell-claude-bridge.service.
            '';
          }
          {
            assertion = !cfg.petBrain.enable;
            message = ''
              programs.trollshell.petBrain.enable is set, but the
              trollshell-pet-brain user service (llama-server) is currently
              implemented only by the home-manager module —
              nix/nixos-module.nix declares no unit for it, so this setting has
              no effect here. The brain loads a GGUF out of a per-user
              directory, which is home-manager's half of the world. See #694.
              Move this to your home-manager programs.trollshell.petBrain
              config, or install the unit by hand from
              etc/systemd/user/trollshell-pet-brain.service.
            '';
          }
        ];
      }

      # Control-center companion app (#399): the external GTK settings/management
      # window (app-id mov.vibec0re.trollshell.ControlCenter) that speaks to the
      # shell's mov.vibec0re.trollshell.Control endpoint. Installed by default
      # alongside the shell; its own toggle drops it. Kept as a self-contained
      # mkMerge branch so it doesn't collide with the systemPackages edits in the
      # recommended-services block below.
      (lib.mkIf cfg.controlCenter.enable {
        environment.systemPackages = [ cfg.controlCenter.package ];
      })

      # The recommended-but-optional system daemons trollshell's chips lean
      # on, grouped behind the master switch. Each chip hides itself when its
      # daemon is missing, so dropping the lot still leaves a working bar.
      # Per-daemon mkDefault keeps an explicit `services.<d>.enable = false;`
      # in user config intact even while the switch is on.
      (lib.mkIf cfg.enableRecommendedServices {
        # UPower drives the battery chip + plug/unplug OSDs. Without it the
        # chip stays hidden (BatteryState::Unknown) and the five property
        # subscriptions sit in PropState::Loading forever.
        services.upower.enable = lib.mkDefault true;

        # power-profiles-daemon (net.hadess.PowerProfiles) feeds the
        # power-profile selector. Without it ActiveProfile + Profiles stay in
        # PropState::Loading; the profile group hides itself (panels gate on
        # available.is_empty()) so the drawer stays clean.
        services.power-profiles-daemon.enable = lib.mkDefault true;

        # geoclue2 auto-locates the weather widget (and future location-aware
        # features). Without it the geoclue service times out and falls back to
        # TROLLSHELL_WEATHER_CITY; with neither, weather shows a "set a city"
        # hint rather than breaking. Still individually gated by its own toggle.
        services.geoclue2 = lib.mkIf cfg.weather.geoclue.enable {
          enable = lib.mkDefault true;
          # MLS is dead — point geoclue's wifi backend at beaconDB.
          geoProviderUrl = lib.mkDefault cfg.weather.geoclue.providerUrl;
          # Let trollshell's geoclue client (DesktopId "trollshell")
          # request location.
          appConfig.trollshell = {
            isAllowed = true;
            isSystem = false;
          };
        };

        # The GNOME Online Accounts → evolution-data-server stack that feeds
        # the calendar + tasks drawer pages. trollshell has no in-shell
        # account UI by design (system-daemon-as-state-store): EDS *is* the
        # account store, and hytte-ecal is a thin read-only client of it
        # (crates/hytte-ecal/src/lib.rs). The flow is:
        #   Settings → Online Accounts (gnome-control-center) adds a
        #   Google/iCloud/CalDAV account → GOA writes an EDS source →
        #   evolution-data-server syncs it into the local .ics cache →
        #   hytte-ecal reads it → calendar/tasks populate.
        # Without this stack a fresh NixOS install has no way to acquire an
        # account, so those panels sit in their empty state. mkDefault per
        # service so an explicit `services.gnome.<svc>.enable = false;` still
        # wins even with the master switch on.

        # GOA daemon (org.gnome.OnlineAccounts) — the account backend EDS
        # consumes. This is the "add an account" half of the flow.
        services.gnome.gnome-online-accounts.enable = lib.mkDefault true;

        # evolution-data-server provides the evolution-*-factory services and
        # the on-disk .ics/.vcf cache that hytte-ecal reads. (EDS's own module
        # already turns gnome-keyring on; we set it below explicitly too.)
        services.gnome.evolution-data-server.enable = lib.mkDefault true;

        # gnome-keyring is where GOA stores the OAuth tokens / CalDAV
        # passwords for the accounts you add; without it GOA can't persist
        # credentials and re-prompts (or fails) on every session.
        services.gnome.gnome-keyring.enable = lib.mkDefault true;

        # dconf is the GSettings backend gnome-control-center (and the GOA
        # panel) read/write their state through. A trollshell user typically
        # runs no full GNOME desktop-manager to enable it, so wire it here —
        # otherwise the Online Accounts panel can't persist its settings.
        programs.dconf.enable = lib.mkDefault true;

        # gnome-control-center is the actual UI to add accounts: its
        # "Online Accounts" panel (`gnome-control-center online-accounts`) is
        # where you sign in to Google/iCloud/CalDAV. It's a heavy dependency,
        # but without an account-adding UI the rest of the stack is inert.
        # Merged into the list rather than replacing the base systemPackages.
        #
        # `trollshell-online-accounts` is a thin launcher for that panel.
        # gnome-control-center hard-refuses to start unless XDG_CURRENT_DESKTOP
        # names GNOME or Unity ("Running gnome-control-center is only supported
        # under GNOME and Unity, exiting"); under a Niri session it's `niri`,
        # so the bare command bails. The wrapper spoofs the desktop just for
        # this invocation — bind it to a niri keybind, or run it by name.
        #
        # Spoofing GNOME also makes g-c-c show its *whole* sidebar, including
        # shell-only panels that read GSettings schemas only a real GNOME Shell
        # installs. On a Niri box those schemas are absent, so g-c-c reads a
        # value off a NULL schema and core-dumps — e.g. opening "Multitasking"
        # aborts with "Settings schema 'org.gnome.shell.app-switcher' is not
        # installed" → trace trap (#375). That panel reads
        # org.gnome.shell.app-switcher / .window-switcher (from gnome-shell) and
        # org.gnome.mutter (from mutter). We don't want the shell/compositor
        # themselves — only their compiled *schemas* — so prepend just those two
        # packages' schema dirs to XDG_DATA_DIRS for this launch; the shell-only
        # panels then resolve their schemas and render inert instead of aborting.
        #
        # Path form: GIO appends `glib-2.0/schemas` to each XDG_DATA_DIRS entry,
        # and nixpkgs relocates schemas to `share/gsettings-schemas/<name>/`
        # (NOT bare `share/glib-2.0/schemas`) — the same layout the devShell
        # points GLib at (nix/devshell.nix). So each entry is the package's
        # `.../gsettings-schemas/<name>` dir. g-c-c's own wrapGAppsHook wrapper
        # only *prefixes* XDG_DATA_DIRS, so these tail entries survive to the
        # real binary. `$XDG_DATA_DIRS` is expanded by the shell before `env`
        # runs, preserving whatever the session already set.
        environment.systemPackages = [
          pkgs.gnome-control-center
          (pkgs.writeShellScriptBin "trollshell-online-accounts" ''
            exec env XDG_CURRENT_DESKTOP=GNOME \
              XDG_DATA_DIRS="${pkgs.gnome-shell}/share/gsettings-schemas/${pkgs.gnome-shell.name}:${pkgs.mutter}/share/gsettings-schemas/${pkgs.mutter.name}:$XDG_DATA_DIRS" \
              ${pkgs.gnome-control-center}/bin/gnome-control-center online-accounts "$@"
          '')

          # Screen-recording flow (#403, crates/hytte-services/src/recorder.rs):
          # the bar's record-toggle chip spawns `wf-recorder` and picks a region
          # with `slurp`. Neither is provisioned elsewhere — unlike screenshots,
          # which go through niri's own compositor-native screenshot UI
          # (niri::screenshot in hytte-services), there is no existing dep to
          # mirror here. Missing binaries degrade gracefully (a logged warning,
          # no recording) rather than crashing, but without this the feature
          # silently does nothing out of the box (#421).
          pkgs.wf-recorder
          pkgs.slurp
        ];

        # evolution-alarm-notify (#402): EDS's own alarm daemon. It watches
        # every source's calendar for VALARM triggers and posts standard
        # org.freedesktop.Notifications toasts — which trollshell (as the
        # daemon, crates/hytte-services/src/notifications.rs) renders like any
        # other notification. Zero application code: daemon-as-state-store
        # extends to alarm bookkeeping too, so snooze/dismiss/recurring-event
        # expansion stay in EDS's own battle-tested state machine rather than
        # a reimplementation in hytte-ecal (see #402's Option A vs B).
        #
        # evolution-data-server itself ships this exact unit at
        # share/systemd/user/evolution-alarm-notify.service, but NixOS has no
        # systemd.user.packages import mechanism (unlike systemd.packages for
        # system units, which services.gnome.evolution-data-server's own
        # module already uses two entries up), so the package's unit is never
        # picked up on its own — wire it here, matching upstream's Type/BusName
        # so systemd tracks readiness the same way evolution itself would.
        # Gated on evolution-data-server specifically (not just the master
        # switch): the daemon has nothing to watch without it. mkDefault on
        # `enable` so `systemd.user.services.evolution-alarm-notify.enable =
        # false;` still wins even with both switches on.
        systemd.user.services.evolution-alarm-notify =
          lib.mkIf config.services.gnome.evolution-data-server.enable
            {
              enable = lib.mkDefault true;
              description = "Event and Task Reminders (evolution-alarm-notify)";
              wantedBy = [ "graphical-session.target" ];
              partOf = [ "graphical-session.target" ];
              after = [ "graphical-session.target" ];
              serviceConfig = {
                Type = "dbus";
                BusName = "org.gnome.Evolution-alarm-notify";
                ExecStart = "${pkgs.evolution-data-server}/libexec/evolution-data-server/evolution-alarm-notify";
                Restart = "on-failure";
                RestartSec = 2;
              };
            };

        # System-bus policy: allow any user to own the two trollshell agent
        # names. BlueZ / iwd policies still gate the actual method ACLs; this
        # only grants the right to RequestName. Without it, hytte_bus::own_name
        # hits AccessDenied at the broker and parks the agent inert with one
        # info-level log (own.rs) — the bluetooth/wifi pairing agents just go
        # quiet, nothing crashes.
        #
        # NB: the NetworkManager secret agent (issue #99) is deliberately NOT
        # listed here. NM secret agents do not own a well-known name — NM
        # records the registering connection's *unique* name and calls
        # GetSecrets back on it (hytte_bus::export_object mounts the object
        # name-lessly on the shared system connection). NM's own bundled
        # system-bus policy already lets a console user register a secret agent,
        # so no extra <allow own=...> entry is required.
        services.dbus.packages = [
          (pkgs.writeTextDir "share/dbus-1/system.d/mov.vibec0re.trollshell.conf" ''
            <!DOCTYPE busconfig PUBLIC
              "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
              "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
            <busconfig>
              <policy context="default">
                <allow own="mov.vibec0re.trollshell.bluez-agent"/>
                <allow own="mov.vibec0re.trollshell.iwd-agent"/>
              </policy>
            </busconfig>
          '')
        ];

        # Polkit authentication agent. trollshell no longer ships its own
        # in-process agent; run the standard standalone polkit-gnome agent as a
        # user service bound to the graphical session. Without it the bar still
        # runs — only polkit-mediated privilege prompts lose their GUI. Swap
        # polkit_gnome for another agent (mate-polkit, hyprpolkitagent, …) by
        # overriding this unit's ExecStart.
        systemd.user.services.polkit-gnome-authentication-agent-1 = {
          description = "polkit-gnome authentication agent";
          wantedBy = [ "graphical-session.target" ];
          partOf = [ "graphical-session.target" ];
          after = [ "graphical-session.target" ];
          serviceConfig = {
            Type = "simple";
            ExecStart = "${pkgs.polkit_gnome}/libexec/polkit-gnome-authentication-agent-1";
            Restart = "on-failure";
            RestartSec = 1;
            TimeoutStopSec = 10;
          };
        };

        # xdg-desktop-portal for niri (#375), mirroring the home-manager
        # module's programs.trollshell.portals.enable (nix/hm-module.nix). The
        # FileChooser portal is what the Appearance "Browse…" wallpaper picker —
        # and gnome-control-center's own file dialogs — open through; without a
        # backend the picker has nothing to talk to. #380 already stopped the
        # picker from *crashing* the shell (it opens the FileDialog unparented);
        # this wires an actual portal backend so it *works* out of the box on a
        # NixOS-module deployment. Home-manager users get this from
        # programs.trollshell.portals.enable; on a NixOS-only host (no
        # home-manager) it was wired nowhere. Same routing as
        # etc/xdg-desktop-portal/niri-portals.conf. mkDefault throughout so an
        # explicit override still wins.
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

        # xdg-desktop-portal picks its per-desktop config (the niri-portals.conf
        # rendered above) by matching XDG_CURRENT_DESKTOP, and the GNOME backend
        # only activates when GNOME is in the list. niri advertises itself as
        # `niri`, so export `niri:GNOME` for the session so both the niri
        # portal config and the gnome backend engage (see
        # etc/xdg-desktop-portal/README.md). The home-manager module leaves this
        # to the user (its portals option doc says so); the NixOS module sets it
        # here. mkDefault so a hand-set value wins.
        environment.sessionVariables.XDG_CURRENT_DESKTOP = lib.mkDefault "niri:GNOME";
      })
      # Optional GNOME desktop apps that complement the GOA/EDS stack: a real
      # calendar UI (trollshell's own Calendar page is read-only), plus task
      # and contact managers for the lists GOA provisions. Gated separately
      # from the services so a minimal install can keep the daemons + the
      # account-add UI without pulling in the heavier GUI apps.
      (lib.mkIf cfg.enableRecommendedSoftware {
        environment.systemPackages = [
          # Read/write calendar UI over the same EDS sources trollshell's
          # Calendar page reads — the way to actually create/edit events,
          # which the shell's read-only page can't.
          pkgs.gnome-calendar
          # GNOME Tasks (Endeavour): full UI for the EDS task lists the
          # trollshell Tasks page surfaces.
          pkgs.endeavour
          # GNOME Contacts: GOA also syncs contacts; this views/edits them
          # (trollshell doesn't surface contacts itself).
          pkgs.gnome-contacts
        ];
      })
      # When the home-manager NixOS module is in use, register trollshell's HM
      # module as a shared module so per-user `programs.trollshell` config is
      # available everywhere (the user service starts once a user also sets
      # programs.trollshell.enable = true in their home-manager config).
      (lib.optionalAttrs (options ? home-manager) {
        home-manager.sharedModules = [ self.homeModules.default ];
      })
    ]
  );
}
