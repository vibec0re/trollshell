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
in
{
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
        # UI (hytte-ui/src/style.css, trollshell/style.css), JetBrains Mono /
        # Fira Code for the clock + workspace chips. Without them fontconfig
        # silently falls back and the bar renders in the wrong typeface.
        fonts.packages = [
          pkgs.inter
          pkgs.cantarell-fonts
          pkgs.jetbrains-mono
          pkgs.fira-code
        ];

        # Weather widget location fallback. Stays outside the recommended-
        # services switch on purpose: it's the manual alternative to geoclue,
        # so it must keep working when auto-location is turned off. weather
        # forward-geocodes this city when GeoClue2 is absent.
        environment.sessionVariables.TROLLSHELL_WEATHER_CITY = lib.mkIf (
          cfg.weather.fallbackCity != null
        ) cfg.weather.fallbackCity;

        # Wallpaper reload command for the Appearance picker. Defaults follow
        # wallpaper.backend: null for swaybg (restart swaybg.service — the
        # shell's built-in default) and none; `awww img {}` for awww. The chosen
        # path reaches the command as TROLLSHELL_WALLPAPER_PATH.
        environment.sessionVariables.TROLLSHELL_WALLPAPER_RELOAD_CMD = lib.mkIf (
          cfg.wallpaper.reloadCommand != null
        ) cfg.wallpaper.reloadCommand;
      }

      # Wallpaper daemon (NixOS side): only swaybg is managed here. swaybg is the
      # bundled default and reads the wallpaper.path file itself (the Appearance
      # picker restarts this unit). The awww backend's daemon is run by
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
          # Stay inactive until the Appearance picker has written a path —
          # otherwise `swaybg -i ""` fails and Restart loops on a fresh install.
          unitConfig.ConditionPathExists = "%h/.config/trollshell/wallpaper.path";
          serviceConfig = {
            Type = "simple";
            ExecStart = "${pkgs.bash}/bin/sh -c 'exec ${pkgs.swaybg}/bin/swaybg -i \"$(${pkgs.coreutils}/bin/cat %h/.config/trollshell/wallpaper.path)\" -m fill'";
            Restart = "on-failure";
            RestartSec = 2;
          };
        };
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
        environment.systemPackages = [
          pkgs.gnome-control-center
          (pkgs.writeShellScriptBin "trollshell-online-accounts" ''
            exec env XDG_CURRENT_DESKTOP=GNOME \
              ${pkgs.gnome-control-center}/bin/gnome-control-center online-accounts "$@"
          '')
        ];

        # System-bus policy: allow any user to own the two trollshell agent
        # names. BlueZ / iwd policies still gate the actual method ACLs; this
        # only grants the right to RequestName. Without it, hytte_bus::own_name
        # hits AccessDenied at the broker and parks the agent inert with one
        # info-level log (own.rs) — the bluetooth/wifi pairing agents just go
        # quiet, nothing crashes.
        services.dbus.packages = [
          (pkgs.writeTextDir "share/dbus-1/system.d/cc.hannig.trollshell.conf" ''
            <!DOCTYPE busconfig PUBLIC
              "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
              "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
            <busconfig>
              <policy context="default">
                <allow own="cc.hannig.trollshell.bluez-agent"/>
                <allow own="cc.hannig.trollshell.iwd-agent"/>
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
