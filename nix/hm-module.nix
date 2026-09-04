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
  #
  # `target` (#707) is the systemd user target each launched plugin unit binds
  # to (`PartOf=`) — the SAME value the shell's own unit below binds to. The
  # launcher used to hardcode graphical-session.target, so any session that set
  # `systemd.target` (the documented niri-session.target example, which is what
  # etc/ ships) had the shell on one target and its plugins on another, and
  # session teardown reached them out of step. The key is optional on the shell
  # side: a plugins.json without it falls back to graphical-session.target, so
  # an older module's file (and the NixOS module's, which has no shell unit and
  # so no such option) keeps working unchanged. It is only emitted here when it
  # differs from that same fallback (optionalAttrs, mirroring
  # trollshellSessionEnv below) — a default-configured session's plugins.json
  # therefore stays byte-identical to the pre-#707 one, so upgrading recycles
  # no already-running plugin (#813 item 2 fixed this key being emitted
  # unconditionally, which silently defeated that invariant).
  pluginsState = builtins.toJSON (
    {
      version = 1;
      plugins = lib.mapAttrs (_: plugin: {
        exec = lib.getExe plugin.package;
        inherit (plugin) env secrets;
        enabled = plugin.enable;
      }) cfg.plugins;
    }
    // (lib.optionalAttrs (cfg.systemd.target != "graphical-session.target") {
      target = cfg.systemd.target;
    })
  );

  # Night light (#222, #577): the wlsunset user unit's ExecStart.
  #
  # Coordinates are NOT baked in here any more. Nix-eval time cannot consult a
  # runtime daemon, which is exactly why the geoclue seeding was deferred and
  # why the Night light switch was a silent no-op unless you hand-wrote lat/lon.
  # The shell resolves them at toggle time instead (nightlight.rs: live location
  # fix -> the static options below -> refuse to start) and writes the argument
  # vector to ~/.config/trollshell/wlsunset.args, one argument per line — the
  # same handoff the Appearance picker uses for swaybg.args, read back below.
  #
  # Temperatures stay here: they have no runtime source.
  #
  # The args file is preferred over the static coordinates rather than merged
  # with them, so a hand-started unit (no shell running, no args file) still
  # behaves exactly as it did before #577: configured coords run, and with none
  # configured the unit prints a hint and exits 0 rather than handing wlsunset
  # a location it does not have.
  nl = cfg.nightlight;
  nlGeoConfigured = nl.latitude != null && nl.longitude != null;
  # Quoted for the inner `sh`; systemd expands the %h specifier and passes the
  # rest of the single-quoted script through untouched ($a / $@ are not words of
  # their own, so systemd leaves them for sh — same as the swaybg unit below).
  nlArgsFile = "\"%h/.config/trollshell/wlsunset.args\"";
  # Read the args file a line at a time into the positional args.
  nlReadArgs = "set --; while IFS= read -r a; do set -- \"$@\" \"$a\"; done < ${nlArgsFile}";
  nlFallback =
    if nlGeoConfigured then
      "set -- -l ${toString nl.latitude} -L ${toString nl.longitude}"
    else
      "echo \"wlsunset: no coordinates — start the Night light toggle from trollshell (it seeds them from your location), or set programs.trollshell.nightlight.{latitude,longitude}\" >&2; exit 0";
  nlExecStart = "${pkgs.bash}/bin/sh -c 'if [ -s ${nlArgsFile} ]; then ${nlReadArgs}; else ${nlFallback}; fi; exec ${pkgs.wlsunset}/bin/wlsunset -t ${toString nl.nightTemp} -T ${toString nl.dayTemp} \"$@\"'";

  # ── The two LLM backend units (#694) ────────────────────────────────────────
  # Both are per-user daemons with no upstream home-manager module, so both get
  # a plain user unit below — the same treatment swaybg and wlsunset get, and
  # the reason neither has a NixOS half (nix/nixos-module.nix asserts instead).
  cb = cfg.claudeBridge;
  pb = cfg.petBrain;

  # Where the pet brain's GGUF lives. Resolved here rather than as the option's
  # own default because module-common.nix is shared with the NixOS module, where
  # `config.home` does not exist to point at.
  pbModel =
    if pb.model != null then
      pb.model
    else
      "${config.home.homeDirectory}/.local/share/trollshell-pet/brain.gguf";

  # The ordering invariant between the bridge's per-request budget and its
  # client's global request timeout: the bridge's has to expire FIRST, so a slow
  # turn reaches the plugin as a clean 504 it can fall back from rather than as a
  # connection torn mid-read. It is documented at
  # crates/hytte-claude-bridge/src/bridge.rs:38-39 and
  # crates/hytte-ai-providers/src/lib.rs's DEFAULT_TIMEOUT, i.e. as prose in two
  # crates that nobody re-reads while editing their nix config. With both halves
  # declared in one config we can check it at eval time instead (#694).
  #
  # Only the pet has a client-side knob (#699, landed in #711). It parses exactly
  # like the bridge's own: unset, blank, unparsable or 0 all fall back to the
  # compiled default — so the regex match below mirrors that rather than
  # `lib.toInt`-ing a value that would throw on "" or "soon".
  aiProvidersDefaultTimeoutSecs = 10;
  petTimeoutRaw = cfg.plugins.pet.env.PET_LLM_TIMEOUT_SECS or null;
  petTimeoutSecs =
    if petTimeoutRaw != null && builtins.match "[1-9][0-9]*" petTimeoutRaw != null then
      lib.toInt petTimeoutRaw
    else
      aiProvidersDefaultTimeoutSecs;

  # The env the option surface renders to, built once and fed to BOTH
  # home.sessionVariables (login shells, `cargo run` from a terminal) and the
  # trollshell unit's Environment= below. The systemd user manager never
  # sources hm-session-vars.sh, so a unit-less copy means every one of these
  # options silently no-ops under systemd.enable (#568: stats.layout = "split"
  # had no effect). Each var is set only when its option is non-null;
  # optionalAttrs + // keeps them independent (mkIf on a whole attrset would
  # force an all-or-nothing block).
  trollshellSessionEnv = {
    # Stats-drawer layout (#508). Always set (not optionalAttrs): the value
    # is a total enum whose default equals the shell's own runtime default,
    # so exporting it explicitly is harmless and keeps the session env
    # self-describing.
    TROLLSHELL_STATS_LAYOUT = cfg.stats.layout;
  }
  // (lib.optionalAttrs (cfg.weather.fallbackCity != null) {
    TROLLSHELL_WEATHER_CITY = cfg.weather.fallbackCity;
  })
  // (lib.optionalAttrs (cfg.ownerName != null) {
    # Desktop owner's name (#696/#813), read by pet and caw's LLM personas
    # through the shared hytte_ai_providers::owner() resolver. Unset (not an
    # empty string) means both fall back to their neutral "your human"
    # default.
    TROLLSHELL_OWNER = cfg.ownerName;
  })
  // (lib.optionalAttrs (cfg.wallpaper.reloadCommand != null) {
    # Appearance picker reload command; null = the shell's swaybg default.
    TROLLSHELL_WALLPAPER_RELOAD_CMD = cfg.wallpaper.reloadCommand;
  })
  // (lib.optionalAttrs cfg.recorder.audioByDefault {
    # Arm the record chip's audio capture at session start (#403). Only set
    # when opted in — unset reads as off, and the env var is the override,
    # so Settings still flips it live during a session.
    TROLLSHELL_RECORD_AUDIO = "1";
  })
  // (lib.optionalAttrs nlGeoConfigured {
    # Static night-light coordinates (#577). The shell prefers a live location
    # fix and only falls back to these, so they are the override for a session
    # with no GeoClue2 — see nightlight.rs. Both are set together or not at all
    # (nlGeoConfigured); a half-configured pair is useless to wlsunset.
    TROLLSHELL_NIGHTLIGHT_LATITUDE = toString nl.latitude;
    TROLLSHELL_NIGHTLIGHT_LONGITUDE = toString nl.longitude;
  });
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
    the bundled swaybg wallpaper user unit standalone, which reads its
    argument list from ~/.config/trollshell/swaybg.args (the Appearance drawer
    page writes it — per-output images and rotation supported). Gently deprecated in favor of
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

        # See trollshellSessionEnv in the let above — shared with the unit's
        # Environment= so shells and the service agree.
        home.sessionVariables = trollshellSessionEnv;

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
            # The option-rendered session env, delivered where the process
            # actually reads it — the user manager never sources
            # hm-session-vars.sh (#568). Each assignment is quoted whole so a
            # value with spaces (wallpaper.reloadCommand) survives systemd's
            # Environment= word splitting; embedded double quotes in a value
            # are not supported.
            Environment = lib.mapAttrsToList (name: value: "\"${name}=${value}\"") trollshellSessionEnv;
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
      #
      # Writing the file is only half of "declarative", though (#695): the units
      # are *transient*, created by the shell at runtime, so there is no unit
      # file for home-manager to diff or restart — activation used to stop here
      # and a changed env/package/enable stayed frozen in the running plugin
      # until the next login, silently. So after the write, poke the running
      # shell's Control endpoint to reconcile (ReloadPlugins re-reads
      # plugins.json and starts/stops/restarts to match). Notes:
      #   * run as a NixOS module, activation happens in home-manager-<user>
      #     .service, which has no DBUS_SESSION_BUS_ADDRESS — hence the same
      #     XDG_RUNTIME_DIR prelude home-manager's own startServices uses.
      #   * it must be a hard no-op at boot / on a non-graphical switch, hence
      #     `|| true`: no shell running is the normal case, not an error (the
      #     shell reconciles on its own next start).
      (lib.mkIf (cfg.plugins != { }) {
        xdg.configFile."trollshell/plugins.json".text = pluginsState;

        home.activation.trollshellReloadPlugins = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
          export XDG_RUNTIME_DIR="''${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
          export DBUS_SESSION_BUS_ADDRESS="''${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}"
          ''${DRY_RUN_CMD:-} ${pkgs.systemd}/bin/busctl --user --quiet \
            call mov.vibec0re.trollshell.Control /mov/vibec0re/trollshell/Control \
            mov.vibec0re.trollshell.Control ReloadPlugins || true
        '';
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

      # wlsunset — night light (#222, #577). No home-manager module exists, so a
      # plain user unit mirroring swaybg. The shell toggles it on demand via
      # `systemctl --user start|stop wlsunset.service` (nightlight.rs), so there
      # is deliberately NO Install/WantedBy — it defaults to inactive and the
      # Appearance drawer's Night light switch brings it up. Always declared (so
      # the toggle target exists) and, since #577, always usable: the shell seeds
      # the coordinates into wlsunset.args before starting it, and only refuses
      # to start when it could resolve none at all.
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
            # The no-coordinates branch exits 0, so on-failure never loops it.
            Restart = "on-failure";
            RestartSec = 2;
          };
          # No Install section — the shell starts/stops it (see comment above).
        };
      }

      # hytte-claude-bridge (#584/#694/#866) — the keyless loopback
      # OpenAI-compatible shim over headless Claude Code.
      #
      # It used to be a hand-declared user unit here, mirroring swaybg/wlsunset.
      # Since #866 the daemon speaks the widget-plugin protocol itself (it paints
      # a status chip; crates/hytte-claude-bridge/src/plugin.rs), so it is
      # declared as a PLUGIN ENTRY instead and the shell's own launcher spawns it
      # as a transient trollshell-plugin-claude-bridge.service
      # (trollshell/src/plugin_launcher.rs). That is Annika's call on #866, and it
      # buys three things a bespoke unit could not: the control-center's Plugins
      # tab can stop/start it, #695's reconcile applies an edited config to a
      # running session, and #392's keyring injection covers its API key.
      #
      # etc/systemd/user/trollshell-claude-bridge.service stays as the reference
      # for hand-installed (non-home-manager) deployments — retiring it is a
      # separate follow-up, and the launcher deliberately leaves a static unit
      # of the same name alone.
      #
      # Writing into `programs.trollshell.plugins` rather than straight into the
      # rendered JSON is what keeps the entry overridable: the values below are
      # `lib.mkDefault`, so `plugins.claude-bridge.env.CLAUDE_BRIDGE_THINKING`
      # (or a `PATH=` for a `claude` outside the user manager's PATH) is set the
      # ordinary way, which is the replacement for the pre-#866
      # `systemd.user.services.…Service.Environment` escape hatch.
      #
      # ── THE ONE THING THAT CHANGED BEHAVIOUR ────────────────────────────────
      # The retired unit carried `UnsetEnvironment=` for the four billing
      # redirects. A transient unit has no such setting, so that scrub does not
      # exist on this path. What remains is the bridge's own startup refusal
      # (crates/hytte-claude-bridge/src/envguard.rs): in the two `claude` modes it
      # exits, naming the offending variable, rather than quietly billing metered
      # credits — the belt without the braces, and still a loud failure. It is
      # also why `secrets` below is declared ONLY in `api` mode: injecting
      # ANTHROPIC_API_KEY into a `claude` mode would stop the bridge starting at
      # all (#752).
      (lib.mkIf cb.enable {
        programs.trollshell.plugins.claude-bridge = {
          enable = lib.mkDefault true;
          package = lib.mkDefault cb.package;
          env = lib.mapAttrs (_: lib.mkDefault) (
            {
              RUST_LOG = "hytte_claude_bridge=info";
              CLAUDE_BRIDGE_MODE = cb.mode;
              CLAUDE_BRIDGE_PORT = toString cb.port;
              CLAUDE_BRIDGE_TIMEOUT_SECS = toString cb.timeoutSeconds;
              # Belt-and-braces dummy key. Nothing in the bridge reads it (it is
              # keyless and validates no bearer at all); the copy that actually
              # prevents a leak is the one on the CONSUMING plugin, because
              # `hytte_ai_providers::load_key` runs in the plugin's process and
              # checks $OPENROUTER_API_KEY before ~/.config/trollshell/
              # openrouter.key. Set it there too:
              # `plugins.pet.env.OPENROUTER_API_KEY = "local-bridge";`.
              OPENROUTER_API_KEY = "local-bridge";
            }
            // (lib.optionalAttrs (cb.model != null) { CLAUDE_BRIDGE_MODEL = cb.model; })
          );
          # The `api` mode is the only one for which an ANTHROPIC_API_KEY is a
          # credential rather than a startup refusal — see the block above and
          # claudeBridge.mode's description. `optionals`, not `mkIf`, because a
          # listOf definition concatenates: a user adding their own slot to this
          # plugin keeps it either way.
          secrets = lib.optionals (cb.mode == "api") [ "anthropic" ];
        };

        # See petTimeoutSecs in the `let` above. Only asserted when a `pet` is
        # declared in the same config — with no pet there is no client budget for
        # nix to compare against, and asserting against the compiled 10s default
        # would be a false positive for a bridge consumed by something else.
        assertions = [
          {
            assertion = !(cfg.plugins ? pet) || cb.timeoutSeconds < petTimeoutSecs;
            message = ''
              programs.trollshell.claudeBridge.timeoutSeconds is
              ${toString cb.timeoutSeconds}, which is not strictly less than the
              pet plugin's own request timeout (${toString petTimeoutSecs}s, from
              programs.trollshell.plugins.pet.env.PET_LLM_TIMEOUT_SECS — or
              hytte_ai_providers::DEFAULT_TIMEOUT when that is unset, blank or
              unparsable). The bridge's per-request budget has to expire first,
              so a slow turn comes back to the pet as a 504 it can fall back
              from instead of tearing the connection mid-read
              (crates/hytte-claude-bridge/src/bridge.rs). Either lower
              claudeBridge.timeoutSeconds, or raise
              programs.trollshell.plugins.pet.env.PET_LLM_TIMEOUT_SECS above it
              (#699/#711).
            '';
          }
        ];
      })

      # trollshell-pet-brain (#276/#694) — the local llama-server the pet talks
      # to when its PET_LLM_URL points here instead of at the claude bridge.
      # Ported from etc/systemd/user/trollshell-pet-brain.service, which stays
      # the reference for hand-installed deployments.
      (lib.mkIf pb.enable {
        systemd.user.services.trollshell-pet-brain = {
          Unit = {
            Description = "llama-server brain for the trollshell pet plugin";
            PartOf = [ cfg.systemd.target ];
            After = [ cfg.systemd.target ];
            # The GGUF is a runtime download, not part of the closure (see the
            # petBrain.enable description) — gate the unit on the file existing
            # instead of letting llama-server crash-loop against Restart=
            # on-failure until it is fetched. Same idiom as swaybg's args file.
            ConditionPathExists = pbModel;
          };
          Service = {
            Type = "simple";
            # `getExe'` by binary name, not `getExe`: llama-cpp's mainProgram is
            # the CLI, not the server. The model path is shell-escaped because
            # systemd splits ExecStart on whitespace and it is the one component
            # a user can point at a directory with a space in it; the store path
            # and the numbers cannot contain one.
            ExecStart = lib.concatStringsSep " " (
              [
                (lib.getExe' pb.package "llama-server")
                "--model"
                (lib.escapeShellArg pbModel)
                "--port"
                (toString pb.port)
              ]
              # One list entry is one argv token — escaped, so an entry with a
              # space stays a single argument instead of being re-split by
              # systemd into two.
              ++ map lib.escapeShellArg pb.extraArgs
            );
            Restart = "on-failure";
            RestartSec = 5;
          };
          Install.WantedBy = [ cfg.systemd.target ];
        };
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
