{
  description = "trollshell — hytte-based Wayland desktop shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    # Only used by the flake checks (hm-module) to evaluate homeModules.default
    # against a real home-manager module set; not a runtime dependency.
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      treefmt-nix,
      crane,
      home-manager,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems =
        fn:
        nixpkgs.lib.genAttrs systems (
          system:
          fn rec {
            pkgs = import nixpkgs { inherit system; };
            # crane drives the build on nixpkgs' own rust toolchain.
            craneLib = crane.mkLib pkgs;
            treefmt-eval = treefmt-nix.lib.evalModule pkgs ./nix/treefmt.nix;
          }
        );
    in
    {
      packages = forAllSystems (
        { pkgs, craneLib, ... }:
        let
          trollshell = pkgs.callPackage ./nix/package.nix { inherit craneLib; };
        in
        {
          inherit trollshell;
          default = trollshell;
        }
      );

      devShells = forAllSystems (
        { pkgs, ... }:
        {
          default = import ./nix/devshell.nix {
            inherit pkgs;
            trollshell = self.packages.${pkgs.stdenv.hostPlatform.system}.trollshell;
          };
        }
      );

      formatter = forAllSystems ({ treefmt-eval, ... }: treefmt-eval.config.build.wrapper);

      checks = forAllSystems (
        {
          pkgs,
          treefmt-eval,
          craneLib,
          ...
        }:
        let
          system = pkgs.stdenv.hostPlatform.system;

          # A cheap stand-in for the real trollshell package so the module-eval
          # checks don't force a full Rust crate build just to type-check the
          # config bodies. It carries meta.mainProgram so `lib.getExe cfg.package`
          # (used by the systemd ExecStart) still resolves.
          stubPackage = pkgs.writeShellScriptBin "trollshell" "";

          # The hytte-ecal `probe` example binary + fixture sources (a
          # task-list and a calendar), for the eds-nixos-test below.
          probe = pkgs.callPackage ./nix/probe.nix { inherit craneLib; };
          # The hytte-services `wifi_probe` example binary, for the
          # wifi-nm-nixos-test below.
          wifiProbe = pkgs.callPackage ./nix/wifi-probe.nix { inherit craneLib; };
          taskSource = pkgs.writeText "test-tasks.source" ''
            [Data Source]
            DisplayName=Test Tasks
            Enabled=true

            [Task List]
            BackendName=local
          '';
          # A writable local calendar the probe seeds a FREQ=DAILY;COUNT=5
          # VEVENT into, then expands via generate_instances — exercising the
          # RRULE-expansion path (#29).
          calSource = pkgs.writeText "test-calendar.source" ''
            [Data Source]
            DisplayName=Test Calendar
            Enabled=true

            [Calendar]
            BackendName=local
          '';
        in
        {
          formatting = treefmt-eval.config.build.check self;

          # Evaluate homeModules.default against a real home-manager module set so
          # the config bodies (systemd user units, session vars, the swaybg gate,
          # the awww assertion) are actually forced — not just parsed. Builds a
          # trivial derivation that deepSeq's the config attrs that hold those
          # bodies, so a broken body fails the check rather than silently lurking.
          hm-module =
            let
              hm = home-manager.lib.homeManagerConfiguration {
                inherit pkgs;
                modules = [
                  self.homeModules.default
                  {
                    home = {
                      username = "alice";
                      homeDirectory = "/home/alice";
                      stateVersion = "24.11";
                      # `nixpkgs.follows = "nixpkgs"` means HM and nixpkgs ride
                      # the same unstable channel but report different release
                      # numbers; the check is module-eval coverage, not a
                      # release-matched deployment, so silence the warning.
                      enableNixpkgsReleaseCheck = false;
                    };
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      enableSessionExtras = true;
                      weather.fallbackCity = "Berlin";
                      systemd.target = "niri-session.target";
                    };
                  }
                ];
              };
              cfg = hm.config;
              # Force the module's config bodies: the systemd user units (incl.
              # the swaybg unit the extras bundle starts under the default
              # backend), the session vars, and every assertion's *predicate* (so
              # the awww-channel assertion in hm-module.nix actually runs). Only
              # the predicates, not the messages — a message is the lazy
              # explanation shown when an assertion fails, so forcing it would be
              # both pointless and prone to evaluating intentionally-deferred text.
              probe = builtins.deepSeq {
                userUnits = cfg.systemd.user.services;
                sessionVars = cfg.home.sessionVariables;
                assertionPredicates = map (a: a.assertion) cfg.assertions;
              } "ok";
            in
            pkgs.runCommand "trollshell-hm-module-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # Evaluate nixosModules.default the same way. Forces the module's own
          # config contributions — the swaybg/polkit user units, the session
          # vars, and the assertions — rather than system.build.toplevel, to keep
          # it light. A blanket `deepSeq` over the whole NixOS config blows the
          # call stack (systemPackages/systemd recurse through the full package
          # closure), so we force only the trollshell-relevant slices: attrNames
          # of the service sets (which still forces every mkIf/mkMerge branch to
          # decide membership) plus the swaybg unit's ExecStart string (the actual
          # body logic) and the assertions.
          nixos-module =
            let
              nixos = nixpkgs.lib.nixosSystem {
                inherit system;
                modules = [
                  self.nixosModules.default
                  {
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      weather.fallbackCity = "Berlin";
                    };
                    # Minimal stubs so the NixOS module set evaluates without a
                    # real machine: a bootloader, a root filesystem, and a state
                    # version. nixpkgs.hostPlatform is set via nixosSystem above.
                    boot.loader.grub.enable = false;
                    fileSystems."/" = {
                      device = "/dev/sda1";
                      fsType = "ext4";
                    };
                    system.stateVersion = "24.11";
                  }
                ];
              };
              cfg = nixos.config;
              # Force the trollshell module's own outputs without descending into
              # package store closures: the unit/var/package key sets, the swaybg
              # ExecStart body, the session vars, the dbus policy package count,
              # and every assertion's boolean. We force only the assertion
              # *predicates*, not their messages — NixOS ships assertions whose
              # `message` is lazy and only well-defined when the assertion fails
              # (e.g. the fileSystems topological-sort error), so deepSeq'ing all
              # messages would trip an unrelated internal assertion's message.
              probe = builtins.deepSeq {
                userUnits = builtins.attrNames cfg.systemd.user.services;
                swaybgExec = cfg.systemd.user.services.swaybg.serviceConfig.ExecStart;
                sessionVars = cfg.environment.sessionVariables;
                systemPackageCount = builtins.length cfg.environment.systemPackages;
                dbusPackageCount = builtins.length cfg.services.dbus.packages;
                assertionPredicates = map (a: a.assertion) cfg.assertions;
              } "ok";
            in
            pkgs.runCommand "trollshell-nixos-module-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # The "lean heavy on nix" counterpart to the Rust ephemeral-EDS
          # harness (#49): boot a real NixOS VM with evolution-data-server
          # configured declaratively, seed a fixture task list + calendar, and
          # run the hytte-ecal probe against it end-to-end. The probe also
          # creates a FREQ=DAILY;COUNT=5 VEVENT and expands it, so this gates
          # the RRULE-expansion fix for #29. Verified to run under TCG (no KVM
          # needed); GitHub's Linux runners have /dev/kvm for speed.
          eds-nixos-test = pkgs.testers.runNixOSTest {
            name = "eds-nixos-test";
            nodes.machine =
              { ... }:
              {
                users.users.alice = {
                  isNormalUser = true;
                  uid = 1000;
                };
                # Three-line EDS module: installs the package, wires its D-Bus
                # session activation service files, and its systemd user units.
                services.gnome.evolution-data-server.enable = true;
                programs.dconf.enable = true; # EDS GSettings backend
                services.gnome.gnome-keyring.enable = true; # EDS credential store
                environment.systemPackages = [ probe ];
                virtualisation.graphics = false;
              };
            testScript = ''
              machine.wait_for_unit("multi-user.target")

              # Seed the fixture task-list + calendar sources into alice's home.
              machine.succeed("mkdir -p /home/alice/.config/evolution/sources")
              machine.copy_from_host(
                  "${taskSource}",
                  "/home/alice/.config/evolution/sources/test-tasks.source",
              )
              machine.copy_from_host(
                  "${calSource}",
                  "/home/alice/.config/evolution/sources/test-calendar.source",
              )
              machine.succeed("chown -R alice:users /home/alice/.config")
              # Store-copied files land read-only (0444); EDS's source
              # registry rewrites .source files on first open to add runtime
              # keys, which fails ("Permission denied") on a read-only file —
              # benign for the auto-provisioned lists but it left the seeded
              # *calendar* unwritable, so creating an event on it failed. Make
              # the fixtures writable.
              machine.succeed("chmod -R u+w /home/alice/.config/evolution")

              # Bring up alice's user session (creates /run/user/1000/bus).
              machine.succeed("loginctl enable-linger alice")
              machine.wait_for_unit("user@1000.service")
              machine.wait_for_file("/run/user/1000/bus")

              # Run the probe as alice; EDS D-Bus-activates on first connect.
              schemas = "${pkgs.evolution-data-server}/share/gsettings-schemas"
              output = machine.wait_until_succeeds(
                  "su -s /bin/sh alice -c '"
                  + "export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus; "
                  + "export HOME=/home/alice XDG_RUNTIME_DIR=/run/user/1000; "
                  + "export GSETTINGS_SCHEMA_DIR=$(ls -d " + schemas + "/*/glib-2.0/schemas | head -1); "
                  + "probe'",
                  timeout=180,
              )
              # EDS auto-provisions a default "Personal" list, so the count
              # isn't 1 — assert our seeded fixture is enumerated and the FFI
              # create/remove roundtrip actually worked.
              assert "Test Tasks" in output, output
              assert "created uid: hytte-ecal-probe-1" in output, output
              assert "removed hytte-ecal-probe-1" in output, output

              # Live view push (#33): the probe opens a CalClientView over the
              # task list, then from a *second* client connection (standing in
              # for Endeavour) creates + modifies a task. EDS must push the
              # objects-added/-modified notifications to the view — exercising
              # get_view_sync → view_start → the GObject signal trampoline →
              # the boxed Rust callback, pumped via a private GMainContext. A
              # missing push would bail the probe (so wait_until_succeeds would
              # fail); the explicit count line is the positive signal. The probe
              # watches whichever task list EDS lists first (ordering isn't
              # guaranteed — could be the auto-provisioned "Personal" or our
              # "Test Tasks"), so don't pin the name here.
              assert "watching '" in output, output
              assert "editor created uid: hytte-ecal-live-1" in output, output
              assert "editor modified uid: hytte-ecal-live-1" in output, output
              assert "live view push count:" in output, output
              # At least the create + modify pushes landed (initial population +
              # 2). Parse the final count and assert it advanced past the
              # initial-population baseline. (The match can't be None here — the
              # assert above already required the line — but guard it so the
              # test driver's type checker is satisfied.)
              import re
              m = re.search(r"live view push count: (\d+)", output)
              assert m is not None, output
              push_count = int(m.group(1))
              assert push_count >= 2, f"expected >=2 view pushes, got {push_count}: {output}"

              # Recurrence expansion (#29): the probe seeds a
              # FREQ=DAILY;COUNT=5 VEVENT and expands it over a one-month
              # window. All 5 occurrences must materialise — the whole point
              # of the fix (the old master-only path would surface just 1).
              assert "Test Calendar" in output, output
              assert "created recurring uid:" in output, output
              assert "recurring instance count: 5" in output, output
              assert "removed recurring" in output, output
            '';
          };

          # The "lean heavy on nix" harness for the NetworkManager Wi-Fi
          # backend (#96): boot a real NixOS VM with NetworkManager and a pair
          # of virtual Wi-Fi radios (mac80211_hwsim), then drive wifi_nm
          # end-to-end via the wifi_probe example — backend detection, device
          # discovery, a live RequestScan, and a state read. mac80211_hwsim
          # gives NM a real (simulated) wlan device so the whole D-Bus path
          # exercises against a live daemon, not a mock. Mirrors
          # eds-nixos-test; runs under TCG (no KVM needed).
          wifi-nm-nixos-test = pkgs.testers.runNixOSTest {
            name = "wifi-nm-nixos-test";
            nodes.machine =
              { ... }:
              {
                networking.networkmanager.enable = true;
                # Two virtual 802.11 radios; NM manages the resulting wlan
                # interfaces, giving the probe a real device + AP scan path.
                boot.kernelModules = [ "mac80211_hwsim" ];
                boot.extraModprobeConfig = "options mac80211_hwsim radios=2";
                environment.systemPackages = [ wifiProbe ];
                virtualisation.graphics = false;
              };
            testScript = ''
              machine.wait_for_unit("multi-user.target")
              machine.wait_for_unit("NetworkManager.service")
              # Wait until NM has a Wi-Fi device registered (hwsim + NM takeover).
              machine.wait_until_succeeds(
                  "nmcli -t -f DEVICE,TYPE device | grep ':wifi'", timeout=60
              )

              # Run the probe as root on the system bus — drives wifi_nm against
              # the live NetworkManager.
              output = machine.wait_until_succeeds("wifi_probe", timeout=180)
              assert "backend=NetworkManager" in output, output
              assert "device=" in output, output
              assert "scan=" in output, output
              assert "networks=" in output, output
            '';
          };
        }
      );

      # `import … self` (not a bare path) because the module's `package` option
      # defaults to self.packages.<system>.trollshell. A bare-path module would
      # instead build via `pkgs.callPackage ./package.nix`, which needs crane
      # (craneLib) wired into the consumer's nixpkgs; threading `self` reuses the
      # package we already built here rather than pushing that onto consumers.
      nixosModules.default = import ./nix/nixos-module.nix self;

      # Curried the same way and for the same reason as the NixOS module above.
      homeModules.default = import ./nix/hm-module.nix self;
      homeManagerModules.default = self.homeModules.default;
    };
}
