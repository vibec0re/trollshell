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
      # The 14 bundled widget plugins (#558), by crate = binary = flake-output
      # name. Each is packaged by nix/plugin.nix, which since #572 is a plain
      # `cp` of one already-compiled binary out of the single whole-workspace
      # compile (`trollshell.passthru.workspace`) — no cargo, no crane. Shared
      # between the `packages` output (per-plugin flake outputs, so
      # `programs.trollshell.plugins.<id>.package` has something in THIS flake to
      # point at) and the `checks` output (build coverage, #449), so the list
      # lives once. `hytte-plugin-proto` (the wire-protocol lib) and
      # `hytte-plugin` (the SDK) are not plugins and are deliberately absent.
      bundledPluginNames = [
        "hytte-plugin-agents"
        "hytte-plugin-audio-widget"
        "hytte-plugin-bar-clock-demo"
        "hytte-plugin-caw"
        "hytte-plugin-clock-demo"
        "hytte-plugin-departures"
        "hytte-plugin-infobroker"
        "hytte-plugin-niri-layouts"
        "hytte-plugin-pet"
        "hytte-plugin-preem-demo"
        "hytte-plugin-terminal"
        "hytte-plugin-timer"
        "hytte-plugin-usage"
        "hytte-plugin-weather"
      ];
      # The source revision this build came from (#601), threaded into the
      # *wrapped* binaries' runtime environment as `TROLLSHELL_REV` so a running
      # shell can answer "which commit am I?" — the question that has now cost
      # two rounds of investigating already-fixed behaviour (#375, #566).
      #
      # `self.shortRev` exists only on a CLEAN git tree; a dirty working tree
      # instead carries `self.dirtyShortRev` (e.g. "34e3d96-dirty"), and a
      # non-git source (a `path:` flake, a tarball) has neither — hence the
      # literal fallback. Verified against nix 2.34: the two attributes are
      # mutually exclusive, never both present.
      #
      # Deliberately NOT a compile-time env on the `workspace` derivation: that
      # would change the one expensive crane compile's hash on every commit and
      # invalidate the artifact every package output slices from (see
      # nix/package.nix). It is injected only by the cheap wrapper slices
      # (nix/package.nix's `preFixup`, nix/control-center.nix), which are a `cp`
      # plus a makeWrapper call. The Rust side (trollshell/src/revision.rs)
      # reads it at runtime and falls back to "dev" when unset, which is exactly
      # what a plain `cargo run` gets.
      revision = self.shortRev or (self.dirtyShortRev or "unknown");
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
          trollshell = pkgs.callPackage ./nix/package.nix { inherit craneLib revision; };
          # The single whole-workspace compile (#572). EVERY package output
          # below is a slice of this one derivation — a `cp` of one binary out
          # of its `$out/bin`, optionally wrapped — so there is exactly one
          # cargo invocation on the package path and nothing that can miss a
          # cross-derivation artifact cache. See nix/package.nix.
          workspace = trollshell.passthru.workspace;

          # The control-center companion app (#399): the workspace binary, GApps-
          # wrapped, plus a .desktop launcher. No cargo (#572).
          trollshell-control-center = pkgs.callPackage ./nix/control-center.nix {
            inherit workspace revision;
          };

          # The per-agent companion window (#950, P2 of the agentic desktop):
          # hyperhive's own agent page in a WebKitGTK view inside our chrome.
          # Same slice-and-wrap shape as the control center — it is a windowed
          # GTK app, and WebKit needs the GApplication env — but with no
          # .desktop item, since it is meaningless without `--agent <name>`
          # (nix/agent-window.nix says why). `programs.trollshell.agentWindow`
          # installs it; the agents plugin launches it off `PATH`.
          trollshell-agent-window = pkgs.callPackage ./nix/agent-window.nix {
            inherit workspace revision;
          };

          # Per-plugin flake packages (#558): `packages.hytte-plugin-<id>` for
          # each of the 14 bundled plugins. Generated from `bundledPluginNames`
          # (one attr each) rather than hand-written. Since #572 each is a `cp`
          # of one already-compiled binary out of `workspace` — no cargo, no
          # crane, no recompile.
          bundledPlugins = pkgs.lib.genAttrs bundledPluginNames (
            name: pkgs.callPackage ./nix/plugin.nix { inherit workspace name; }
          );

          # The `hytte-infobroker` CLI (#562): the #487 consent-gated broker's
          # second `[[bin]]` (crates/hytte-plugin-infobroker/Cargo.toml — a tool,
          # not a widget plugin), used by the etc/skills/infobroker agent-bridge
          # skill. Packaged via the same nix/plugin.nix derivation as the bundled
          # plugins above, but deliberately kept OUT of `bundledPluginNames` —
          # nothing in `programs.trollshell.plugins` should point at it; install
          # it with a plain `home.packages` entry instead (see the skill docs).
          hytte-infobroker = pkgs.callPackage ./nix/plugin.nix {
            inherit workspace;
            name = "hytte-infobroker";
            description = "trollshell consent-gated agent-bridge broker CLI (#487)";
          };

          # The `hytte-claude-bridge` daemon (#584): a keyless same-uid-socket shim
          # putting an OpenAI-compatible face on headless Claude Code. Since #866
          # it speaks the plugin protocol too (a status chip) and IS driven by
          # `programs.trollshell.plugins` — `nix/hm-module.nix` renders the
          # `claude-bridge` entry from `claudeBridge.*`, so nobody points
          # `plugins.<id>.package` at this by hand. It still stays out of
          # `bundledPluginNames`, which is a mechanical `hytte-plugin-<id>` →
          # package map and this binary is not named that way. GTK-free, so
          # nix/plugin.nix's unwrapped `cp` is exactly right; no wrapGAppsHook4
          # needed.
          hytte-claude-bridge = pkgs.callPackage ./nix/plugin.nix {
            inherit workspace;
            name = "hytte-claude-bridge";
            description = "trollshell keyless same-uid-socket OpenAI-compatible bridge to headless Claude Code (#584, #993)";
          };

          # Autogenerated `programs.trollshell.*` options reference (#533) +
          # the plugin env-knob reference (#573/#614), both rendered onto one
          # docs site. Its own file (nix/options-doc.nix) — see there for the
          # full rationale — so the `checks` output below can build it too
          # (#614) via the same `pkgs.callPackage` call rather than
          # duplicating the derivation body. Build + read with:
          #   nix build .#options-doc
          #   $BROWSER result/share/doc/trollshell/index.html
          options-doc = pkgs.callPackage ./nix/options-doc.nix { inherit self pkgs; };
        in
        {
          inherit
            trollshell
            trollshell-control-center
            trollshell-agent-window
            options-doc
            hytte-infobroker
            hytte-claude-bridge
            ;
          default = trollshell;
        }
        // bundledPlugins
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
          trollshell = pkgs.callPackage ./nix/package.nix { inherit craneLib revision; };
          # The single whole-workspace compile (#572), mirroring the `packages`
          # output above.
          workspace = trollshell.passthru.workspace;

          # The control-center companion app (#411), mirroring the `packages`
          # output above — a slice of `workspace`, no cargo of its own (#572).
          trollshell-control-center = pkgs.callPackage ./nix/control-center.nix {
            inherit workspace revision;
          };

          # The 14 per-plugin packages (#558), mirroring the `packages` output.
          # Merged into `checks` below so `nix flake check` actually *builds*
          # each one — the same reason #449 wired the two existing packages into
          # checks: flake check only builds what's listed here, so without this a
          # broken plugin package could stay green until someone ran `nix build
          # .#hytte-plugin-<id>`. Genuinely near-free since #572: each is a `cp`
          # out of the one `workspace` output every other check already forces.
          bundledPlugins = pkgs.lib.genAttrs bundledPluginNames (
            name: pkgs.callPackage ./nix/plugin.nix { inherit workspace name; }
          );

          # The `hytte-infobroker` CLI package (#562), mirroring the `packages`
          # output above. Wired into `checks` below for the same #449 reason as
          # `bundledPlugins`: without it, `nix flake check` could stay green
          # while `nix build .#hytte-infobroker` was actually broken.
          hytte-infobroker = pkgs.callPackage ./nix/plugin.nix {
            inherit workspace;
            name = "hytte-infobroker";
            description = "trollshell consent-gated agent-bridge broker CLI (#487)";
          };

          # The `hytte-claude-bridge` daemon package (#584), mirroring the
          # `packages` output above and wired into `checks` below for the same
          # #449 reason: without it, `nix flake check` could stay green while
          # `nix build .#hytte-claude-bridge` was broken. Until #757 it was also
          # the one place crane's *git*-dependency vendoring got exercised in CI:
          # `hive-claude` was a rev pin that `builtins.fetchGit` resolved at
          # eval, which a sandboxed build phase could not have done. It comes
          # from crates.io now, so nothing in this flake reaches a third-party
          # forge to evaluate any more (#671).
          hytte-claude-bridge = pkgs.callPackage ./nix/plugin.nix {
            inherit workspace;
            name = "hytte-claude-bridge";
            description = "trollshell keyless same-uid-socket OpenAI-compatible bridge to headless Claude Code (#584, #993)";
          };

          # A cheap stand-in for the real trollshell package so the module-eval
          # checks don't force a full Rust crate build just to type-check the
          # config bodies. It carries meta.mainProgram so `lib.getExe cfg.package`
          # (used by the systemd ExecStart) still resolves.
          stubPackage = pkgs.writeShellScriptBin "trollshell" "";

          # Stand-in plugin binary for the programs.trollshell.plugins
          # coverage in the two module-eval checks below (#350/#355).
          stubPlugin = pkgs.writeShellScriptBin "hytte-plugin-demo" "";

          # Stand-ins for the two LLM backend daemons (#694), so the hm-module
          # check below can turn both units on and force their bodies without
          # pulling a full workspace compile (hytte-claude-bridge) or llama-cpp
          # into the check's closure. Named after the binary each unit actually
          # invokes, so `lib.getExe` (bridge, via meta.mainProgram) and
          # `lib.getExe' … "llama-server"` (pet brain, by name) both resolve.
          stubClaudeBridge = pkgs.writeShellScriptBin "hytte-claude-bridge" "";
          stubLlamaCpp = pkgs.writeShellScriptBin "llama-server" "";

          # The hytte-ecal `probe` example binary + fixture sources (a
          # task-list and a calendar), for the eds-nixos-test below. Since #588
          # this is a slice of `workspace` — a `cp` + a GApps wrap, no cargo and
          # no crane — exactly like the plugin packages above. Before #588 it
          # was its own `buildDepsOnly` + `buildPackage` pair with its own `src`
          # filter, i.e. a second full dependency compile per cold flake check.
          probe = pkgs.callPackage ./nix/probe.nix { inherit workspace; };
          # The hytte-services `wifi_probe` example binary, for the
          # wifi-nm-nixos-test below. Same slice treatment (#588) — it was the
          # third full dependency compile.
          wifiProbe = pkgs.callPackage ./nix/wifi-probe.nix { inherit workspace; };
          taskSource = pkgs.writeText "test-tasks.source" ''
            [Data Source]
            DisplayName=Test Tasks
            Enabled=true

            [Task List]
            BackendName=local
          '';
          # A writable local calendar the probe seeds FREQ=DAILY;COUNT=5
          # VEVENTs into, then expands via generate_instances — exercising the
          # RRULE-expansion path (#29) and the EXDATE recurrence-set modifier
          # (the #29 follow-up: one series cancels a day via EXDATE).
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

          # `options-doc` (#533/#614): pure eval plus one tiny C binary
          # (cmark-gfm) — no crane, nothing expensive — so there's no cost
          # excuse for `options-doc.yml`'s actual render+copy pipeline to go
          # unbuilt on every PR. Mirrors the `packages` output's binding (its
          # own `let`, hence the separate `pkgs.callPackage` call rather than
          # an `inherit` — same reason `trollshell` etc. below are
          # recomputed here too instead of reused across outputs).
          options-doc = pkgs.callPackage ./nix/options-doc.nix { inherit self pkgs; };

          # `nix flake check` only *builds* the derivations listed in `checks`
          # — it does not build `packages` just because they're evaluable.
          # Without these entries, CI (which runs flake check, not
          # `nix build`) can stay green while `nix build .#trollshell` or
          # `.#trollshell-control-center` is actually broken — the release
          # profile, `nix/package.nix`'s src filter, the assets derivation,
          # and the wrapper derivations are never exercised. See #449. Same
          # argument extends to `options-doc` (#614, bound above — already
          # in this attrset, so it isn't repeated in this `inherit`): without
          # it here, a build break in it (a bad cmark-gfm flag, a quoting
          # bug) stays green and first shows up as a red Pages deploy after
          # merge.
          inherit
            trollshell
            trollshell-control-center
            hytte-infobroker
            hytte-claude-bridge
            ;

          # Lint the entire workspace with pedantic-clean Clippy. Reuses
          # cargoArtifacts from the package build so dependencies aren't
          # recompiled from scratch. Must stay green because the workspace
          # denies clippy::all + clippy::pedantic and forbids unsafe.
          #
          # `--features system-tests` pulls the whole-file-gated integration
          # tests (crates/hytte-{bus,reactive,ui}/tests/*.rs) and the gated
          # `mod` blocks (hytte-ui's widget_tests/gtk_tests) into the lint
          # pass too — without it they're invisible to clippy, the same gap
          # #232 found for `cargo test` itself. Verified clean locally
          # (`cargo clippy --workspace --all-targets --features system-tests
          # -- -D warnings`) before wiring this in.
          clippy = craneLib.cargoClippy (
            trollshell.passthru.commonArgs
            // {
              cargoArtifacts = trollshell.passthru.cargoArtifacts;
              # `commonArgs.cargoExtraArgs` already carries `--workspace
              # --locked` (#572), so only the lint-specific flags go here; the
              # effective command is unchanged.
              cargoClippyExtraArgs = "--all-targets --features system-tests -- -D warnings";
              # This is a leaf/terminal check — nothing chains off its target
              # dir as `cargoArtifacts` — so don't pack it. crane defaults
              # `doInstallCargoArtifacts = true`, which would tar the whole
              # (multi-GiB) target dir into $out for no consumer, burning build
              # time and disk. Same fix + reason as system-tests below.
              doInstallCargoArtifacts = false;
            }
          );

          # The static counterpart to `clippy`, for a defect clippy cannot see
          # (#831): a `bind*` call site that discards the closure's own widget
          # parameter and uses a captured strong clone of the same widget
          # instead, pinning it for the binding's lifetime and defeating the
          # `WeakRef` contract in crates/hytte-reactive/src/bind.rs:16-22
          # (#224).
          #
          # This exists because the defect has already recurred once by the
          # only other means available: #772 fixed four sites and closed on a
          # hand-read inventory of four; #831 re-derived the list and found
          # twelve, so two thirds of it had been missed. #834 fixed all twelve
          # with the scanner this check now runs. A unit test can only cover one
          # site at a time and only where the widget is constructible without a
          # registered `Registry` — ten of the twelve are not — whereas the bug
          # is purely *syntactic*, which is exactly what a scanner sees.
          #
          # Deliberately not a crane derivation: it needs no compile, no
          # cargoArtifacts and no target dir, so it goes red in seconds on a
          # cold checkout instead of behind a full workspace build.
          #
          # `cd ${self}` rather than staging the three scanned trees into a
          # sandbox: it keeps the scanned-root list in one place (the script's
          # `DEFAULT_ROOTS`), makes every reported `file:line` a path a
          # developer can open as-is, and costs nothing extra — `formatting`
          # already realises `self` on every flake check. A root that goes
          # missing (a renamed tree) exits 2 rather than passing vacuously, as
          # does a scan that sees implausibly few call sites.
          bind-pins = pkgs.runCommand "trollshell-bind-pin-check" { nativeBuildInputs = [ pkgs.python3 ]; } ''
            cd ${self}
            python3 nix/lint-bind-pins.py
            touch $out
          '';

          # `programs.trollshell.config.core-leds`'s nix-side vocabulary
          # (`style`/`fill` enums, `rows`'s cap) hand-mirrors Rust's
          # `DisplayStyle`/`parse_core_leds_fill`/`MAX_ROWS` (#1041, #1081
          # review M6) with nothing failing if the two drift — and this
          # option is explicitly the template nine more subsystem families
          # copy. Same posture as `bind-pins` above: a source-level defect no
          # compile in this flake can see, so a script rather than a test,
          # with no cargoArtifacts so it goes red in seconds.
          #
          # Deliberately NOT a `cargo test`: a first version of this lived in
          # `trollshell/src/config/core_leds.rs` reading
          # `nix/module-common.nix` off disk via `CARGO_MANIFEST_DIR`, and it
          # passed locally while failing in CI — `nix/package.nix`'s crane
          # source filter keeps only `.rs`/`.toml`/lockfile/CSS/shader files,
          # so the sandboxed `workspace` compile `cargo test --workspace`
          # runs inside has no `.nix` files at all (the same `include_str!`
          # trap CLAUDE.md documents for `assets/`, reached by
          # `std::fs::read_to_string` instead). Widening the crane filter to
          # keep `*.nix` was rejected — every `.nix` edit would then
          # invalidate `workspace`'s source hash and force a full recompile.
          # `nix/lint-core-leds-vocab.py`'s own header has the full story.
          core-leds-vocab =
            pkgs.runCommand "trollshell-core-leds-vocab-check" { nativeBuildInputs = [ pkgs.python3 ]; }
              ''
                cd ${self}
                python3 nix/lint-core-leds-vocab.py
                touch $out
              '';

          # `programs.trollshell.claudeBridge.baseUrl` (`nix/module-common.nix`)
          # is a plain nix string literal naming the same socket
          # `crates/hytte-ai-providers/src/unix.rs`'s `BRIDGE_SOCKET_DIR`/
          # `BRIDGE_SOCKET_FILE`/`BRIDGE_BASE_URL` constants define, and both
          # that crate's and `crates/hytte-claude-bridge/src/main.rs`'s module
          # docs restate in prose (#1099 review M1, #1100; hardened against a
          # #1103 adversarial review's M1/M2/L1/L2 on the nix-to-Rust seam and
          # the doc-prose scan). The Rust constants are already pinned against
          # raw literals by plain `cargo test`
          # (`bridge_url_resolves_to_the_bridge_socket_path`,
          # `the_socket_path_is_the_one_the_client_dials`); nothing compiles
          # the nix literal or either crate's doc prose against them — #1099's
          # review measured that a rename of the Rust file name left
          # `cargo test`, `cargo clippy` and every module-eval check green,
          # because the daemon binds a path only a live session would notice
          # went stale. Same posture as `bind-pins` above: a source-level
          # defect no compile in this flake can see, so a script rather than a
          # test, with no cargoArtifacts so it goes red in seconds.
          # `nix/lint-bridge-socket.py`'s own header has the full story,
          # including why this is not a `cargo test` (the `core-leds-vocab`
          # crane-filter reasoning applies unchanged).
          bridge-socket =
            pkgs.runCommand "trollshell-bridge-socket-check" { nativeBuildInputs = [ pkgs.python3 ]; }
              ''
                cd ${self}
                python3 nix/lint-bridge-socket.py
                touch $out
              '';

          # The preem GL renderer's shaders, compiled in the dialect the shell
          # compiles them in (#893 stage B). Same posture and same reasons as
          # `bind-pins` above: a source-level defect no compile in this flake
          # can see, checked by a script rather than a test, with no
          # cargoArtifacts so it goes red in seconds.
          #
          # Nothing else looks inside those files. They are `include_str!`'d
          # `&'static str`s until a *driver* compiles them, and no check here
          # does — `system-tests` gained a software GL driver (llvmpipe, via
          # `mesa`, #1036) so its own GL-context tests now run for real, but
          # those tests exercise only inline dummy shaders defined in the test
          # module, never these actual `preem_gl`/shader-widget source files.
          # So without this a typo'd identifier ships green and surfaces as a
          # blank chip on glass.
          #
          # The design spec's CI table named **naga** for this row. Measured, it
          # cannot do the job: naga 26's GLSL frontend rejects the entire ES
          # profile — `#version 300/310/320 es` each come back
          # `InvalidVersion(N)` + `InvalidProfile("es")`, so the spec's own
          # fallback (decision 6, "drop to 310 es") does not reach either — and
          # at `#version 450` it stops on `NotImplemented("variable
          # qualifier")` for the `flat in` / `precision` declarations these
          # shaders are written with. The spec preferred naga because glslang is
          # C++ FFI that would have to live in the `hytte-gl` unsafe island: a
          # real objection to *linking* a validator in for #893's untrusted
          # plugin shaders, and none at all to running the binary at build time.
          # So this is glslang, it validates the real `#version 320 es`, and it
          # adds zero `Cargo.lock` entries.
          glsl =
            pkgs.runCommand "trollshell-glsl-check"
              {
                nativeBuildInputs = [
                  pkgs.python3
                  pkgs.glslang
                ];
              }
              ''
                cd ${self}
                python3 nix/lint-glsl.py
                touch $out
              '';

          # Run the hermetic internals suite (#1115): `cargo test --workspace`
          # WITHOUT `--features system-tests` — the same command
          # `nix/package.nix`'s `workspace` derivation ran under its own
          # `doCheck = true` before #1115 turned that off so a consumer's
          # `nix build` doesn't pay for it (and so the deps stage feeding it,
          # `cargoArtifactsBinOnly`, doesn't have to compile the
          # dev-dependency graph for every consumer either — see that file's
          # `cargoArtifacts`/`cargoArtifactsBinOnly` split). Same tests, same
          # gate, only moved to where the gate lives: `nix flake check`
          # already builds the package (#449) and already runs
          # `system-tests` as its own check below; the hermetic suite had no
          # check of its own and rode the package instead.
          #
          # Shape matches hyperhive's own split for the identical problem
          # (Mara, #1115): `hyperhive/nix/rust.nix:43-94` two named
          # `buildDepsOnly` caches, one per audience;
          # `hyperhive/nix/packages/default.nix:61-68` the deploy path takes
          # the no-test-graph one; `hyperhive/nix/checks.nix:58-73` the check
          # takes the one WITH the test graph via a plain `craneLib.cargoTest`
          # rather than `mkCargoDerivation`. Same trade here: `cargoTest`
          # hardcodes `checkPhaseCargoCommand` to `cargoWithProfile test
          # ${cargoExtraArgs} ${cargoTestExtraArgs}`, which is fine for a
          # plain hermetic run — `system-tests` below needs `mkCargoDerivation`
          # directly only because it wraps that command in `xvfb-run` for a
          # display, which this check doesn't need.
          #
          # `trollshell.passthru.commonArgs` already carries `cargoExtraArgs =
          # "--workspace --locked"` (#572, one cargo scope for every stage),
          # so — unlike hyperhive's own `cargo-test`, which has no such shared
          # scope and spells `cargoTestExtraArgs = "--workspace";` itself —
          # nothing further is needed here to get `--workspace`; leaving
          # `cargoTestExtraArgs` at `cargoTest`'s own default (`""`) matches
          # how `clippy` above already reuses the same `cargoExtraArgs` and
          # only adds its own check-specific flags.
          #
          # `cargoArtifacts = trollshell.passthru.cargoArtifacts`, NOT
          # `cargoArtifactsBinOnly`: since #1115 the former is the one built
          # with `buildDepsOnly`'s own default `doCheck = true` (dev-deps +
          # test harnesses compiled and cached), same cache `clippy` above
          # already reuses — the latter has no dev-dependency graph for a
          # `cargo test` run to build against.
          workspace-tests = craneLib.cargoTest (
            trollshell.passthru.commonArgs
            // {
              cargoArtifacts = trollshell.passthru.cargoArtifacts;
              # The same icon-theme gate `nix/package.nix`'s `preCheck` carried
              # before #1115 — see
              # `every_icon_name_exists_in_the_adwaita_theme_on_the_search_path`
              # (crates/hytte-plugin-niri-layouts/src/plugin.rs). nixpkgs puts
              # no icon theme on a build's `XDG_DATA_DIRS` of its own accord,
              # so without this the test silently *skips* instead of gating
              # anything; `TROLLSHELL_REQUIRE_ICON_THEME=1` turns a missing
              # theme into a failure instead, so it can never rot back into
              # a silent no-op. `system-tests` below carries its own copy for
              # the same reason (#1053 review) and this check inherits
              # neither.
              preCheck = ''
                export XDG_DATA_DIRS="${pkgs.adwaita-icon-theme}/share''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
                export TROLLSHELL_REQUIRE_ICON_THEME=1
              '';
              # Leaf/terminal check: nothing consumes its target dir. Same
              # `doInstallCargoArtifacts = false` reasoning as `system-tests`
              # below — crane's default packs the whole (multi-GiB) target
              # dir into `$out`, which is pure waste for a check nothing
              # chains off of.
              doInstallCargoArtifacts = false;
            }
          );

          # Run the `system-tests` cargo-feature bucket (#232): the
          # whole-file-`#![cfg(feature = "system-tests")]` integration tests
          # in hytte-bus/hytte-reactive/hytte-ui, plus the `#[cfg(all(test,
          # feature = "system-tests"))]` GTK unit-test modules in hytte-ui,
          # PLUS the #893 stage B CPU/GL parity harness. Split out to
          # nix/checks/system-tests.nix (#1102), mirroring how `packages`
          # already lives under `nix/*.nix` — see that file for the full
          # rationale (why mkCargoDerivation, the llvmpipe/systemd preCheck,
          # the parity gate). This call passes exactly what the block used
          # to close over here: `craneLib` and the two
          # `trollshell.passthru.*` values it read off the package
          # derivation (`pkgs` is auto-supplied by `callPackage`).
          system-tests = pkgs.callPackage ./nix/checks/system-tests.nix {
            inherit craneLib;
            commonArgs = trollshell.passthru.commonArgs;
            cargoArtifacts = trollshell.passthru.cargoArtifacts;
          };

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
                      # The two LLM backend units (#694). Both on, so their unit
                      # bodies are forced rather than left behind an unevaluated
                      # `lib.mkIf false`, and so the bridge's timeout-ordering
                      # assertion is *constructed* (it lives inside the
                      # claudeBridge mkIf, which the note on the nixos-module
                      # check below explains would otherwise be invisible).
                      claudeBridge = {
                        enable = true;
                        package = stubClaudeBridge;
                        model = "claude-haiku-4-5";
                        # 15 < the pet's 20 below: the ordering invariant the
                        # module asserts, exercised with a raised client budget
                        # rather than the compiled 10s fallback.
                        timeoutSeconds = 15;
                      };
                      petBrain = {
                        enable = true;
                        package = stubLlamaCpp;
                        model = "/var/empty/brain.gguf";
                      };
                      # plugins (#350/#355, attrsOf keyed by id): `demo` gets a
                      # unit; `off` must be filtered out by enable = false.
                      plugins = {
                        demo = {
                          package = stubPlugin;
                          env.DEMO_TOKEN = "hunter2";
                        };
                        off = {
                          package = stubPlugin;
                          enable = false;
                        };
                        # The client half of the bridge's timeout invariant
                        # (#694): a declared `pet` is what switches that
                        # assertion from vacuously true to a real comparison,
                        # and PET_LLM_TIMEOUT_SECS is the string the module has
                        # to parse the way the plugin does (#699/#711).
                        pet = {
                          package = stubPlugin;
                          env = {
                            # The #993 shape: a same-uid socket, not a port.
                            # Spelled out rather than referenced through
                            # `config` (this fixture module takes no arguments)
                            # — the probe below asserts it equals the read-only
                            # `claudeBridge.baseUrl` a real config would use.
                            PET_LLM_URL = "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock";
                            PET_LLM_TIMEOUT_SECS = "20";
                          };
                        };
                      };
                    };
                  }
                  # A second module contributing to the *same* plugin key —
                  # attrsOf submodules must merge per-field across modules
                  # (the point of #355), not conflict or drop an entry.
                  { programs.trollshell.plugins.demo.env.DEMO_EXTRA = "1"; }
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
              units = cfg.systemd.user.services;
              # The declarative plugin launch state (#419): the module renders
              # the plugins option to trollshell/plugins.json — read back here
              # from the generated file's text — instead of emitting units.
              # fromJSON forbids string context (the exec store path), and this
              # probe only inspects the eval, so discarding it is sound here.
              pluginsState = builtins.fromJSON (
                builtins.unsafeDiscardStringContext cfg.xdg.configFile."trollshell/plugins.json".text
              );
              probe =
                # plugins (#419): entries render to the launch-state file, not
                # static units; enable = false is *declared disabled* (listed,
                # not auto-launched) rather than filtered out; and the demo
                # entry's env still merges per-field across the two modules
                # above (#355).
                assert !(units ? trollshell-plugin-demo);
                assert !(units ? trollshell-plugin-off);
                assert pluginsState.plugins.demo.exec == pkgs.lib.getExe stubPlugin;
                assert pluginsState.plugins.demo.enabled;
                assert pluginsState.plugins.demo.env.DEMO_TOKEN == "hunter2";
                assert pluginsState.plugins.demo.env.DEMO_EXTRA == "1";
                assert !pluginsState.plugins.off.enabled;
                # #813 item 2: the fixture above sets systemd.target =
                # "niri-session.target", so the rendered launch state must carry
                # that same value as its top-level "target" key (#707) — the
                # plugin units and the shell's own unit have to bind to the
                # same session target, or teardown comes apart again. The
                # complementary "absent when left at the default" half of this
                # invariant needs its own fixture (this one already forces a
                # non-default target for the LLM-unit assertions below) — see
                # hm-module-plugin-target-default.
                assert pluginsState.target == "niri-session.target";
                # The claude bridge (#866) is a PLUGIN now, not a unit: enabling
                # it must render `plugins.claude-bridge` into the launch state
                # and declare no `trollshell-claude-bridge` unit at all. The
                # negative is the load-bearing half — a stray unit alongside the
                # plugin entry would run the bridge twice and the second copy
                # would find the socket already live and refuse to start.
                assert !(units ? trollshell-claude-bridge);
                assert
                  let
                    b = pluginsState.plugins.claude-bridge;
                  in
                  b.exec == pkgs.lib.getExe stubClaudeBridge
                  && b.enabled
                  # #993: no port is rendered at all, because the bridge
                  # listens on a same-uid socket whose path is not
                  # configurable. The negative is the load-bearing half — a
                  # CLAUDE_BRIDGE_PORT back in this env would mean somebody
                  # restored a listener every local uid can reach.
                  && !(b.env ? CLAUDE_BRIDGE_PORT)
                  && b.env.CLAUDE_BRIDGE_MODEL == "claude-haiku-4-5"
                  && b.env.CLAUDE_BRIDGE_TIMEOUT_SECS == "15"
                  # The default mode, i.e. one that spawns `claude`…
                  && b.env.CLAUDE_BRIDGE_MODE == "subscription"
                  # …so the `anthropic` slot must NOT be declared: an injected
                  # ANTHROPIC_API_KEY is a startup refusal there, not a
                  # credential (#752). The `api` half of this invariant has its
                  # own fixture, hm-module-claude-bridge-api below.
                  && b.secrets == [ ]
                  # THE BILLING/REDIRECT SCRUB (#866, extended #994). The
                  # retired unit's `UnsetEnvironment=` is re-expressed as EMPTY
                  # env values, which is what stops an inherited
                  # ANTHROPIC_API_KEY — or a stray ANTHROPIC_BASE_URL — from
                  # restart-looping the bridge, or silently redirecting it, in
                  # the default mode. Losing any of these is a silent
                  # regression that only bites the users who happen to export
                  # one, so each is asserted by content.
                  && b.env.ANTHROPIC_API_KEY == ""
                  && b.env.ANTHROPIC_AUTH_TOKEN == ""
                  && b.env.CLAUDE_CODE_OAUTH_TOKEN == ""
                  && b.env.CLAUDE_CODE_USE_BEDROCK == ""
                  && b.env.CLAUDE_CODE_USE_VERTEX == ""
                  && b.env.CLAUDE_CODE_USE_FOUNDRY == ""
                  && b.env.ANTHROPIC_BASE_URL == ""
                  && b.env.ANTHROPIC_BEDROCK_BASE_URL == ""
                  && b.env.ANTHROPIC_VERTEX_BASE_URL == ""
                  && b.env.ANTHROPIC_FOUNDRY_BASE_URL == ""
                  && b.env.ANTHROPIC_FOUNDRY_RESOURCE == ""
                  && b.env.ANTHROPIC_FOUNDRY_AUTH_TOKEN == ""
                  && b.env.ANTHROPIC_FOUNDRY_API_KEY == ""
                  && b.env.ANTHROPIC_CUSTOM_HEADERS == "";
                # #993, the client half: `claudeBridge.baseUrl` is read-only and
                # is exactly what a plugin's `*_LLM_URL` has to carry — the
                # replacement for the two-things-must-agree-on-one-number job
                # `claudeBridge.port` used to do. `$XDG_RUNTIME_DIR` is left
                # unexpanded on purpose; logind mints it at login, so nix
                # cannot know it and `hytte-ai-providers` resolves it in the
                # consuming plugin's own process.
                assert pluginsState.plugins.pet.env.PET_LLM_URL == cfg.programs.trollshell.claudeBridge.baseUrl;
                assert pkgs.lib.hasPrefix "unix://" cfg.programs.trollshell.claudeBridge.baseUrl;
                assert units ? trollshell-pet-brain;
                # `builtins.toString` because home-manager's unitOption merge
                # hands some of these back list-wrapped (ExecStart below is
                # `[ "…" ]`, not `"…"`) — toString is identity on a plain string
                # and space-joins the one-element list, so the assertions hold
                # whichever shape the merge produces.
                # petBrain.model is a runtime path, so it gates the unit rather
                # than entering the closure, and lands in llama-server's argv.
                assert
                  builtins.toString units.trollshell-pet-brain.Unit.ConditionPathExists == "/var/empty/brain.gguf";
                assert
                  let
                    exec = builtins.toString units.trollshell-pet-brain.Service.ExecStart;
                  in
                  pkgs.lib.hasInfix "--model /var/empty/brain.gguf" exec
                  && pkgs.lib.hasInfix "--port 8080" exec
                  && pkgs.lib.hasSuffix "--ctx-size 1024 --threads 4" exec;
                builtins.deepSeq {
                  userUnits = units;
                  sessionVars = cfg.home.sessionVariables;
                  assertionPredicates = map (a: a.assertion) cfg.assertions;
                } "ok";
            in
            pkgs.runCommand "trollshell-hm-module-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # #813 item 2's other half: a *separate* homeManagerConfiguration
          # evaluation, deliberately not folded into the hm-module fixture
          # above. That fixture always sets systemd.target =
          # "niri-session.target" (needed to exercise the LLM-backend-unit
          # assertions), so it can only prove the "target" key is emitted
          # when set — never that it's OMITTED when left at the default. The
          # omission is the half that actually protects the #707 fingerprint
          # invariant: a default-configured session's rendered plugins.json
          # must stay byte-identical to the pre-#707 file, so upgrading
          # recycles no already-running plugin (see the pluginsState comment
          # in nix/hm-module.nix). It is also the half a later refactor
          # breaks silently — the plugins would keep launching, just bound to
          # the wrong target, which is the original #707 defect returning by
          # the back door. This check exists solely to give that invariant a
          # fixture, the same idiom nixos-module-nightlight below uses for a
          # different conditional.
          hm-module-plugin-target-default =
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
                      enableNixpkgsReleaseCheck = false;
                    };
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      # systemd.target deliberately left unset (defaults to
                      # graphical-session.target) — the one thing this fixture
                      # exists to cover.
                      plugins.demo.package = stubPlugin;
                    };
                  }
                ];
              };
              cfg = hm.config;
              pluginsState = builtins.fromJSON (
                builtins.unsafeDiscardStringContext cfg.xdg.configFile."trollshell/plugins.json".text
              );
              probe =
                assert !(pluginsState ? target);
                builtins.deepSeq { inherit pluginsState; } "ok";
            in
            pkgs.runCommand "trollshell-hm-module-plugin-target-default-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # #866's mode-conditional secret slot, which the hm-module fixture
          # above can only ever prove one side of (it runs the default
          # `subscription` mode, where the slot must be ABSENT). This is the
          # other side: with `claudeBridge.mode = "api"` the rendered plugin
          # entry must declare the `anthropic` slot, so the launcher injects the
          # keyring's key as ANTHROPIC_API_KEY at spawn — the whole point of
          # moving the bridge onto the launcher, and the one path by which an
          # `api`-mode bridge gets a key without a file.
          #
          # The pair is genuinely two-sided and neither half is decorative:
          # declaring the slot in a `claude` mode stops the bridge starting at
          # all (envguard's #752 refusal), and not declaring it in `api` mode
          # leaves the key file as the only source. A refactor that dropped the
          # `cb.mode == "api"` guard would keep this check green and break the
          # other; dropping the slot entirely would do the reverse.
          hm-module-claude-bridge-api =
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
                      enableNixpkgsReleaseCheck = false;
                    };
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      claudeBridge = {
                        enable = true;
                        package = stubClaudeBridge;
                        mode = "api";
                      };
                    };
                  }
                ];
              };
              cfg = hm.config;
              pluginsState = builtins.fromJSON (
                builtins.unsafeDiscardStringContext cfg.xdg.configFile."trollshell/plugins.json".text
              );
              probe =
                assert !(cfg.systemd.user.services ? trollshell-claude-bridge);
                assert
                  let
                    b = pluginsState.plugins.claude-bridge;
                  in
                  b.env.CLAUDE_BRIDGE_MODE == "api"
                  && b.secrets == [ "anthropic" ]
                  # The scrub is unconditional — the same four empty values in
                  # api mode. That is not a contradiction with the slot above:
                  # the launcher appends injected secrets AFTER the declared env
                  # and systemd lets the later assignment win, so the empty value
                  # is the floor a keyring key overrides (and the key file's
                  # fallback when there is none).
                  && b.env.ANTHROPIC_API_KEY == "";
                builtins.deepSeq { inherit pluginsState; } "ok";
            in
            pkgs.runCommand "trollshell-hm-module-claude-bridge-api-check" { inherit probe; } ''
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
          #
          # Note (#681): this only covers assertions the fixture below actually
          # triggers. An assertion behind `lib.mkIf <someOption>` (like the
          # night-light one in nix/nixos-module.nix) only enters `cfg.assertions`
          # once the fixture sets that option — otherwise it's never even
          # constructed, so this check would stay green whether it were correct,
          # inverted, or deleted. See nixos-module-nightlight below, which exists
          # solely to give that conditional assertion a fixture that sets the
          # option it depends on. A new conditional assertion needs the same
          # treatment to be covered at all — don't assume this check already
          # sees it.
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
                      # plugins (#350/#355, attrsOf keyed by id): `demo` gets a
                      # unit; `off` must be filtered out by enable = false.
                      plugins = {
                        demo = {
                          package = stubPlugin;
                          env.DEMO_TOKEN = "hunter2";
                        };
                        off = {
                          package = stubPlugin;
                          enable = false;
                        };
                      };
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
                  # A second module contributing to the *same* plugin key —
                  # attrsOf submodules must merge per-field across modules
                  # (the point of #355), not conflict or drop an entry.
                  { programs.trollshell.plugins.demo.env.DEMO_EXTRA = "1"; }
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
              units = cfg.systemd.user.services;
              # The declarative plugin launch state (#419): the NixOS module
              # renders the plugins option to /etc/xdg/trollshell/plugins.json
              # (the $XDG_CONFIG_DIRS fallback the shell's launcher reads).
              # fromJSON forbids string context (the exec store path), and this
              # probe only inspects the eval, so discarding it is sound here.
              pluginsState = builtins.fromJSON (
                builtins.unsafeDiscardStringContext cfg.environment.etc."xdg/trollshell/plugins.json".text
              );
              probe =
                # plugins (#419): entries render to the launch-state file, not
                # static units; enable = false is *declared disabled* (listed,
                # not auto-launched) rather than filtered out; and the demo
                # entry's env still merges per-field across the two modules
                # above (#355).
                assert !(units ? trollshell-plugin-demo);
                assert !(units ? trollshell-plugin-off);
                assert pluginsState.plugins.demo.exec == pkgs.lib.getExe stubPlugin;
                assert pluginsState.plugins.demo.enabled;
                assert pluginsState.plugins.demo.env.DEMO_TOKEN == "hunter2";
                assert pluginsState.plugins.demo.env.DEMO_EXTRA == "1";
                assert !pluginsState.plugins.off.enabled;
                builtins.deepSeq {
                  userUnits = builtins.attrNames units;
                  swaybgExec = units.swaybg.serviceConfig.ExecStart;
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

          # #681: a *separate* nixosModules.default evaluation, deliberately
          # not folded into the fixture above. The check above asserts every
          # predicate is true, which is a useful invariant on its own; this one
          # exists to assert the opposite — that setting a home-manager-only
          # nightlight.* option makes exactly one predicate false — and
          # weakening the first fixture to also cover that would lose the
          # all-true guarantee. This is nix/nixos-module.nix's `nlConfigured`
          # assertion (#657/#680), committed here as the fixture case its
          # author validated by hand with a standalone `nix-instantiate`
          # probe that had nowhere in the repo to live.
          nixos-module-nightlight =
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
                      # The one thing this fixture exists to set (#657/#680):
                      # nightlight.* is home-manager-only, so configuring it
                      # through the NixOS module must trip
                      # nix/nixos-module.nix's `nlConfigured` assertion.
                      nightlight.latitude = 52.52;
                    };
                    # Same minimal stubs as the nixos-module fixture above, so
                    # this evaluates without a real machine.
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
              # Same forcing idiom as the check above — predicates only, not
              # every message (see that check's comment on lazily-invalid
              # messages) — but the shape of what we assert is the mirror
              # image: exactly one predicate must be false here, not all of
              # them true. Deliberately not asserting on the total predicate
              # count (~1385 unconfigured, by hand-count while writing this):
              # that number drifts with every nixpkgs bump and would turn this
              # into a maintenance trap unrelated to what it's meant to guard.
              # Asserting on *what* is false — that it's the night-light one,
              # by message content — is both narrower and more stable.
              falsePredicates = builtins.filter (a: !a.assertion) cfg.assertions;
              probe =
                assert builtins.length falsePredicates == 1;
                assert pkgs.lib.hasInfix "nightlight" (builtins.head falsePredicates).message;
                assert pkgs.lib.hasInfix "home-manager" (builtins.head falsePredicates).message;
                builtins.deepSeq { inherit falsePredicates; } "ok";
            in
            pkgs.runCommand "trollshell-nixos-module-nightlight-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # #1041: `programs.trollshell.config.core-leds` renders a base-layer
          # `core-leds.toml` spliced onto the trollshell unit's own
          # `XDG_CONFIG_DIRS` (`nix/hm-module.nix`'s `configBase` +
          # `configDirsUnitEnvironment`) — NOT under
          # `xdg.configFile`/`$XDG_CONFIG_HOME`, which is the OVERLAY
          # `crates/hytte-config/src/xdg.rs` reserves for a hand edit. This
          # check forces the module eval, resolves the rendered store path out
          # of the trollshell **unit**'s own `Service.Environment` — not the
          # login-shell `home.sessionVariables` surface, which #1081 review
          # M1 found gave `nix build` a green check while the unit itself had
          # gone dark (mutation N1: drop `XDG_CONFIG_DIRS` from
          # `Service.Environment` alone, leaving `home.sessionVariables`
          # untouched — `#568`'s exact lesson, applied to this variable
          # instead of a `TROLLSHELL_*` one) — and TOML round-trips its bytes
          # against the values set. The positive half of the #1041
          # module-eval coverage; the negative half (an unknown key) is
          # nixos-module-core-leds-unknown-key below, since the submodule type
          # is declared once in module-common.nix and shared by both modules.
          # `hm-module-core-leds-composes-with-xdg-systemdirs` below covers
          # the *other* half of H1 — that this doesn't collide with a user's
          # own `xdg.systemDirs.config`.
          hm-module-core-leds =
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
                      enableNixpkgsReleaseCheck = false;
                    };
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      config.core-leds = {
                        style = "lcd";
                        rows = "rect";
                      };
                    };
                  }
                ];
              };
              cfg = hm.config;
              # Each entry of `Service.Environment` is a NIX string carrying
              # its own literal `"…"` quoting (`nix/hm-module.nix`'s
              # `"\"${name}=${value}\""`, for systemd's own list-of-quoted-
              # strings syntax) — find the `XDG_CONFIG_DIRS=` one and strip
              # both the embedded quotes and the key.
              environment = cfg.systemd.user.services.trollshell.Service.Environment;
              xdgEntry = pkgs.lib.findFirst (e: pkgs.lib.hasPrefix "\"XDG_CONFIG_DIRS=" e) null environment;
              # `assert` lives on `xdgValue` itself, the first thing that would
              # otherwise try (and fail, less legibly) to coerce a `null`
              # `xdgEntry` to a string — not on a separate `probe` binding
              # nobody downstream forces. The build script below interpolates
              # `${renderedFile}` directly, which is derived from `xdgValue`,
              # so this assert sits on the actual path evaluation takes; a
              # `probe`-shaped wrapper that nothing references is dead (a
              # mutation dropping `configDirsUnitEnvironment` still reds, just
              # via Nix's own "cannot coerce null to a string" instead of this
              # message — measured in review).
              xdgValue =
                assert xdgEntry != null;
                pkgs.lib.removeSuffix "\"" (pkgs.lib.removePrefix "\"XDG_CONFIG_DIRS=" xdgEntry);
              # The leading entry is `configBase` (nix/hm-module.nix): a
              # store path is never a mid-list entry, so splitting on ":" and
              # taking the head is exactly what the shell's own
              # `xdg::config_dirs` parse does.
              base = builtins.head (pkgs.lib.splitString ":" xdgValue);
              renderedFile = "${base}/trollshell/core-leds.toml";
            in
            pkgs.runCommand "trollshell-hm-module-core-leds-check" { nativeBuildInputs = [ pkgs.python3 ]; } ''
              python3 -c '
              import tomllib
              with open("${renderedFile}", "rb") as f:
                  data = tomllib.load(f)
              assert data == {"style": "lcd", "rows": "rect"}, data
              '
              touch $out
            '';

          # #1081 review H1, other half: `xdg.systemDirs.config` is a shared
          # home-manager option — a user (or another module) setting it
          # themselves must compose with `nix/hm-module.nix`'s own entry
          # (list concatenation), not conflict with it. A bare
          # `home.sessionVariables.XDG_CONFIG_DIRS = "…";` here — what routing
          # through `trollshellSessionEnv`/`home.sessionVariables` directly
          # would have produced — throws "conflicting definition values" the
          # moment anything else defines that same option, which this fixture
          # reproduces (`/opt/example/etc/xdg`, the exact path the review
          # measured against). Both entries present is the positive half;
          # a clean eval (no `mkForce`, no throw) is the half that used to
          # fail outright.
          hm-module-core-leds-composes-with-xdg-systemdirs =
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
                      enableNixpkgsReleaseCheck = false;
                    };
                    xdg.systemDirs.config = [ "/opt/example/etc/xdg" ];
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      config.core-leds.style = "crt";
                    };
                  }
                ];
              };
              cfg = hm.config;
              dirs = cfg.xdg.systemDirs.config;
              probe =
                assert builtins.elem "/opt/example/etc/xdg" dirs;
                assert pkgs.lib.any (e: pkgs.lib.hasInfix "trollshell-config-base" e) dirs;
                builtins.deepSeq { inherit dirs; } "ok";
            in
            pkgs.runCommand "trollshell-hm-module-core-leds-composes-with-xdg-systemdirs-check"
              {
                inherit probe;
              }
              ''
                echo "$probe" >/dev/null
                touch $out
              '';

          # The NixOS-module half of the same rendering (#1041): writes
          # straight into /etc/xdg (nix/nixos-module.nix), the default
          # $XDG_CONFIG_DIRS base entry, so no session variable to resolve —
          # `cfg.environment.etc` names the store path directly.
          nixos-module-core-leds =
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
                      config.core-leds = {
                        style = "lcd";
                        rows = "rect";
                      };
                    };
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
              renderedFile = cfg.environment.etc."xdg/trollshell/core-leds.toml".source;
            in
            pkgs.runCommand "trollshell-nixos-module-core-leds-check" { nativeBuildInputs = [ pkgs.python3 ]; }
              ''
                python3 -c '
                import tomllib
                with open("${renderedFile}", "rb") as f:
                    data = tomllib.load(f)
                assert data == {"style": "lcd", "rows": "rect"}, data
                '
                touch $out
              '';

          # The negative half of #1041's module-eval coverage: an unrecognised
          # key under `config.core-leds` must fail at module EVAL, not at
          # runtime when the shell tries to load a nix-store file it cannot
          # parse. `programs.trollshell.config.core-leds` is a typed
          # submodule with no `freeformType` (module-common.nix), so this is
          # really proving the module system's own "does not exist" behaviour
          # applies here — worth pinning anyway, since a later change to a
          # `freeformType`/`extraConfig` escape hatch would silently lose it.
          #
          # Forcing an unrelated top-level attribute (`config.system.
          # stateVersion`) is NOT enough to trigger this, measured: a
          # submodule-typed option's own unmatched-definition check is
          # scoped to that option's nested `evalModules`, run lazily when
          # THAT option's value is actually forced — unlike a genuinely
          # top-level typo (`networking.hostNam`), which the outer
          # `evalModules`' own check catches on almost any access. So this
          # forces `config.programs.trollshell.config.core-leds` itself
          # (`deepSeq`, not just WHNF, since a shallow force can stop short
          # of the bad key).
          #
          # #1081 review M4: `builtins.tryEval` reports only success/failure,
          # never the thrown message — verified, there is no way to recover
          # "The option `…` does not exist"'s text from it — so on its own
          # this check cannot tell "`bogus` is unmatched inside a real
          # `core-leds` option" from "`core-leds` itself doesn't exist any
          # more" (mutation N3: rename the option at
          # `nix/module-common.nix`'s declaration — this check stayed green
          # for the wrong reason). The fix is the **control arm** below:
          # the identical fixture minus `bogus` must come back `success =
          # true`. Only a genuine per-key rejection makes both hold at once —
          # renaming the option away fails the control too.
          nixos-module-core-leds-unknown-key =
            let
              fixture =
                coreLeds:
                (nixpkgs.lib.nixosSystem {
                  inherit system;
                  modules = [
                    self.nixosModules.default
                    {
                      programs.trollshell = {
                        enable = true;
                        package = stubPackage;
                        config.core-leds = coreLeds;
                      };
                      boot.loader.grub.enable = false;
                      fileSystems."/" = {
                        device = "/dev/sda1";
                        fsType = "ext4";
                      };
                      system.stateVersion = "24.11";
                    }
                  ];
                }).config.programs.trollshell.config.core-leds;
              result = builtins.tryEval (builtins.deepSeq (fixture { bogus = "nope"; }) "ok");
              control = builtins.tryEval (builtins.deepSeq (fixture { style = "lcd"; }) "ok");
              probe =
                assert !result.success;
                assert control.success;
                builtins.deepSeq { inherit result control; } "ok";
            in
            pkgs.runCommand "trollshell-nixos-module-core-leds-unknown-key-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # The "lean heavy on nix" counterpart to the Rust ephemeral-EDS
          # harness (#49). Split out to nix/checks/eds-nixos-test.nix
          # (#1102), mirroring how `packages` already lives under
          # `nix/*.nix` — see that file for the full rationale and the test
          # script. This call passes the module fixtures the block used to
          # close over here (`pkgs` is auto-supplied by `callPackage`).
          eds-nixos-test = pkgs.callPackage ./nix/checks/eds-nixos-test.nix {
            inherit probe taskSource calSource;
          };

          # The "lean heavy on nix" harness for the NetworkManager Wi-Fi
          # backend (#96). Split out to nix/checks/wifi-nm-nixos-test.nix
          # (#1102), mirroring how `packages` already lives under
          # `nix/*.nix` — see that file for the full rationale and the test
          # script. This call passes the one module fixture the block used
          # to close over here (`pkgs` is auto-supplied by `callPackage`).
          wifi-nm-nixos-test = pkgs.callPackage ./nix/checks/wifi-nm-nixos-test.nix {
            inherit wifiProbe;
          };
        }
        // bundledPlugins
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
