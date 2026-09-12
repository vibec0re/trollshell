{
  pkgs,
  trollshell,
}:
pkgs.mkShell {
  # Reuse exactly the build/runtime deps the package pulls in, so the dev shell
  # and the packaged build never drift. Use the raw passthru lists rather than
  # trollshell.nativeBuildInputs — the latter carries crane's vendoring hooks,
  # which warn noisily ("cargoVendorDir not set") when sourced in a shell.
  inherit (trollshell.devInputs) nativeBuildInputs;
  # `webInputs`: the devShell compiles every member, including the one that
  # needs `webkitgtk-6.0.pc` (#950). The GTK *wrappers* deliberately take the
  # narrower `buildInputs` instead — see nix/package.nix (#1130 M1).
  buildInputs = trollshell.devInputs.webInputs;

  # crane builds the package on nixpkgs' rust but doesn't expose it as a
  # buildInput, so the dev shell pulls the toolchain in directly.
  packages = with pkgs; [
    cargo
    rustc
    clippy
    rustfmt
    rust-analyzer

    # Dev inner-loop accelerators (devShell ONLY — see RUSTFLAGS below).
    # mold: a much faster linker; link time dominates the tail of every
    # incremental build given the heavy native deps (gtk4, libadwaita,
    # pipewire/libspa bindgen, evolution-data-server).
    mold
    # sccache: caches rustc artifacts across worktrees/branches, which the
    # review workflow spins up frequently. Opt-in (see CLAUDE.md) — not wired
    # via RUSTC_WRAPPER here to keep the default `cargo` path unsurprising.
    sccache

    # `system-tests`-feature deps (CLAUDE.md's "Tests" section): dbus-daemon
    # for hytte-bus's ephemeral-broker tests, xvfb-run for hytte-ui's
    # GTK-under-a-headless-display tests. CI's flake.nix `system-tests` check
    # already needs both (nativeCheckInputs there); listing them here too
    # just makes the devShell able to run the same commands locally (#684).
    dbus
    xvfb-run

    # The reference GLSL ES compiler, for the `glsl` flake check
    # (`nix/lint-glsl.py`, #893 stage B). Same reason as the two above: CI's
    # check supplies it in `nativeBuildInputs`, and listing it here lets the
    # identical `python3 nix/lint-glsl.py` run from the devShell.
    glslang
  ];

  # Fixed (non-prepending) values live as env attrs; only the two vars that
  # have to prepend to an existing runtime value stay in the shellHook.
  # (BINDGEN_EXTRA_CLANG_ARGS is left to bindgenHook, inherited via the package's
  # nativeBuildInputs — it sets a more complete value than we could here.)
  env = {
    # Asset path env (TROLLSHELL_DATA_DIR / HYTTE_UI_DATA_DIR) is deliberately
    # NOT set here. The packaged build injects both at runtime via makeWrapper
    # (pointing at the `assets` derivation); the dev `cargo run` relies on the
    # in-crate fallbacks instead — trollshell/assets.rs falls back to
    # CARGO_MANIFEST_DIR, and hytte-ui falls back to its include_str! default
    # stylesheet when HYTTE_UI_DATA_DIR is unset. So dev styling/icons resolve
    # to the live in-repo sources with no extra wiring.

    # Kept explicit so the shellHook's LD_LIBRARY_PATH line below sees it
    # regardless of when bindgenHook's hook fires.
    LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

    RUST_BACKTRACE = "1";

    # Use mold as the linker for dev builds. gcc (the stdenv cc) accepts
    # -fuse-ld=mold natively from gcc 12+ and resolves `mold` off PATH, which
    # the devShell `packages` above provides — so no clang or extra wiring is
    # needed. This MUST live in the devShell only: the packaged crane build
    # (nix/package.nix) runs in a sandbox without mold in its buildInputs, so a
    # repo-level .cargo/config.toml linker setting would make `nix build
    # .#trollshell` fail to link. Keeping it in the shell env leaves the
    # package build untouched.
    RUSTFLAGS = "-C link-arg=-fuse-ld=mold";

    # Nix packages GSettings schemas under share/gsettings-schemas/<pkg>/...,
    # but GLib only finds them at share/glib-2.0/schemas/. wrapGAppsHook
    # translates this at install time; for `cargo run` we point GLib at
    # the raw schema dirs ourselves so org.gnome.desktop.interface (and
    # therefore the active GTK icon theme name) reads cleanly.
    GSETTINGS_SCHEMA_DIR = "${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}/glib-2.0/schemas:${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}/glib-2.0/schemas";

    # #1130 N1 — **the local `system-tests` bucket aborts without this.**
    #
    # `trollshell-agent-window`'s display tests construct a `webkit::WebView`,
    # and WebKitGTK sandboxes its own subprocesses with `bwrap`, which needs
    # nested user namespaces. Where there are none — a nix build sandbox, and
    # the container this is developed in — constructing the view does not fail,
    # it **aborts the whole test binary**: `bwrap: Can't mount proc on
    # /newroot/proc: Operation not permitted`, then `Failed to fully launch
    # dbus-proxy`, SIGABRT, exit 101, with no hint about the cause.
    #
    # `nix/checks/system-tests.nix` exports the same variable, with the longer
    # version of this comment, so CI is green — and before this line the two
    # disagreed: `xvfb-run cargo test --workspace --features system-tests`, the
    # command CLAUDE.md's Tests section documents, passed in CI and aborted
    # here. Keeping the local and CI buckets agreeing is the whole point of
    # that block.
    #
    # devShell-only, like `RUSTFLAGS` above, and it reaches nothing that ships:
    # the packaged window (nix/agent-window.nix) sets no such variable and runs
    # a fully sandboxed engine on a real desktop. It does not buy a *working*
    # web process either — one still crashes here — only a process that can
    # construct the widget; see `webview.rs`'s `gtk_tests` module doc for what
    # that does and does not make testable.
    WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS = "1";
  };

  shellHook = ''
    # Put libclang.so on the dynamic loader's search path so the
    # bindgen consumers (libpipewire-sys + libspa-sys) can `dlopen`
    # it. clang-sys's libloading fallback otherwise leans on
    # LIBCLANG_PATH alone, which can race with sibling bindgen
    # invocations in workspace builds.
    export LD_LIBRARY_PATH="$LIBCLANG_PATH:''${LD_LIBRARY_PATH:-}"

    # mkShell doesn't export icon-theme share paths into XDG_DATA_DIRS
    # via setup hooks, so GTK's icon loader can't find Adwaita symbolics
    # (audio-volume-*-symbolic, display-brightness-symbolic, etc.).
    # Prepend them explicitly so `cargo run` from the devShell sees them.
    export XDG_DATA_DIRS="${pkgs.adwaita-icon-theme}/share:${pkgs.hicolor-icon-theme}/share:$XDG_DATA_DIRS"
  '';
}
