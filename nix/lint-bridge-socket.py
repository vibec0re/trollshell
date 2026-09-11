#!/usr/bin/env python3
"""Fail if the claude-bridge socket name drifts between nix and Rust.

THE DEFECT
----------
`hytte-claude-bridge` binds `$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock`
(#993/#1099). That name exists in three places that nothing ties together:

  - `crates/hytte-ai-providers/src/unix.rs` — the canonical source:
    `BRIDGE_SOCKET_DIR`/`BRIDGE_SOCKET_FILE` (the two literals) and
    `BRIDGE_BASE_URL` (their composed `unix://` URL, which
    `hytte-claude-bridge` and every plugin client read rather than
    restating).
  - `crates/hytte-claude-bridge/src/main.rs` — a module-doc sentence that
    spells the same path out in prose ("bound to
    `$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock`"), because a doc comment
    is never compiled against the constants it describes.
  - `nix/module-common.nix` — `programs.trollshell.claudeBridge.baseUrl`'s
    `default`, a plain nix string literal with no way to `import` a Rust
    constant.

#1099's review (M1) measured the gap directly: renaming
`BRIDGE_SOCKET_DIR`/`BRIDGE_SOCKET_FILE` (and so `BRIDGE_BASE_URL`, kept in
step by hand) left `cargo test`, `cargo clippy` and every module-eval check
green, because the nix literal and the Rust constants sit on opposite sides
of the nix/Rust seam that no compiler spans. A live session would notice —
the daemon binds a path no plugin dials — but CI would not. This script is
that missing cross-check, on the `nix/lint-glsl.py` / `nix/lint-bind-pins.py`
/ `nix/lint-core-leds-vocab.py` precedent: a `runCommand` source scan, wired
as `checks.bridge-socket` in `flake.nix`.

It checks two seams, not one:

  1. **Within Rust, across both crates** — `hytte-ai-providers`'s own
     `BRIDGE_BASE_URL` must equal `BRIDGE_SOCKET_DIR`/`BRIDGE_SOCKET_FILE`
     composed (nothing but `cargo test`'s
     `bridge_url_resolves_to_the_bridge_socket_path` catches that today, and
     it compares `BRIDGE_BASE_URL` to a *function's* output, not to the raw
     literals), and `hytte-claude-bridge`'s `main.rs` doc sentence must name
     the same path — a plain doc string that no test reads at all.
  2. **Rust to nix** — `nix/module-common.nix`'s `claudeBridge.baseUrl`
     default must equal `BRIDGE_BASE_URL`.

Nothing here recompiles anything; it is a plain text scan of both trees.

WHY THIS IS A NIX LINT AND NOT A `cargo test`
----------------------------------------------
A `cargo test` cannot read `nix/module-common.nix` inside the sandboxed
`workspace` derivation `nix build` runs its checks in: `nix/package.nix`'s
crane source filter keeps only `.rs`/`.toml`/`Cargo.lock`,
`assets/hytte-ui/style.css` and `.vert`/`.frag` files, so no `.nix` file
exists in that build's source tree at all (the same `include_str!`-of-
`assets/` trap CLAUDE.md documents, reached here by `std::fs::read_to_string`
instead). Widening the crane filter to keep `*.nix` was rejected for
`core-leds-vocab` for the same reason it would be rejected here: every edit
to *any* `.nix` file would invalidate the `workspace` derivation's source
hash and force a full recompile.

A plain `pkgs.runCommand` (the `bind-pins`/`glsl`/`core-leds-vocab`
precedent) reads the real repository tree — there is no crane filter between
a `runCommand`'s `src = ./.;`-shaped input and the checkout — needs no
compile, and reds in seconds.

Run it by hand from the repo root with:

    nix shell nixpkgs#python3 --command python3 nix/lint-bridge-socket.py

The `nix shell` is not optional: **`python3` is deliberately not on the
devShell PATH** (see `nix/lint-bind-pins.py`'s own header), so a bare
`python3 nix/lint-bridge-socket.py` is `command not found`.

USAGE
-----
    python3 nix/lint-bridge-socket.py

Exits 0 when all three spellings agree, 1 naming the first mismatch, 2 when
the scan itself is untrustworthy (a source file is missing, an anchor cannot
be found, or `self_test()` — run first, on every invocation — disagrees with
its own fixtures).
"""

import os
import re
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

UNIX_RS = os.path.join(REPO_ROOT, "crates", "hytte-ai-providers", "src", "unix.rs")
BRIDGE_MAIN_RS = os.path.join(REPO_ROOT, "crates", "hytte-claude-bridge", "src", "main.rs")
MODULE_COMMON_NIX = os.path.join(REPO_ROOT, "nix", "module-common.nix")


def str_const(src: str, name: str) -> str:
    """The string literal a `pub const NAME: &str = "...";` binds.

    Anchored on the identifier with a `\\b` before it and `\\s*:` right after,
    so a differently-named constant sharing a prefix or suffix with `name`
    can never match instead.
    """
    m = re.search(rf'\bconst\s+{re.escape(name)}\s*:\s*&str\s*=\s*"([^"]*)"', src)
    if not m:
        raise LookupError(f"const {name}: &str = \"...\" not found")
    return m.group(1)


def first_backtick_literal_with_prefix(src: str, prefix: str) -> str:
    """The first backtick-quoted span in `src` that starts with `prefix`.

    `main.rs`'s module doc states the bridge's path in prose rather than as a
    named constant, so there is no identifier to anchor on — only the shape
    of the literal itself (it is the only backtick-quoted string in the file
    that starts with `$XDG_RUNTIME_DIR/`).
    """
    for m in re.finditer(r"`([^`]*)`", src):
        if m.group(1).startswith(prefix):
            return m.group(1)
    raise LookupError(f"no backtick-quoted literal starting with {prefix!r} found")


def nix_default_str(src: str, anchor: str) -> str:
    """The string literal a `default = "...";` binds, scanning forward from
    the first line naming `anchor` (an option's `lib.mkOption {` opener).

    Scoped to start at `anchor` rather than matching the first `default = "…"`
    in the whole file, since `module-common.nix` declares more than one
    string-valued option.
    """
    start = src.find(anchor)
    if start < 0:
        raise LookupError(f"anchor {anchor!r} not found")
    after = src[start:]
    m = re.search(r'default\s*=\s*"([^"]*)"', after)
    if not m:
        raise LookupError(f'no default = "..." found after anchor {anchor!r}')
    return m.group(1)


def compose_base_url(dir_: str, file_: str) -> str:
    return f"unix://$XDG_RUNTIME_DIR/{dir_}/{file_}"


def compose_doc_path(dir_: str, file_: str) -> str:
    return f"$XDG_RUNTIME_DIR/{dir_}/{file_}"


# Fixtures for `self_test()`, run on every invocation. A clean tree proves
# nothing about whether the extraction functions still work — only a case
# built to disagree can tell the two apart (the same reasoning
# `lint-bind-pins.py`'s header gives for its own fixtures).
def self_test() -> list[str]:
    failures = []

    unix_src = """
    pub const BRIDGE_SOCKET_DIR_LEGACY: &str = "old-trollshell";
    pub const BRIDGE_SOCKET_DIR: &str = "trollshell";
    pub const BRIDGE_SOCKET_FILE: &str = "claude-bridge.sock";
    pub const BRIDGE_BASE_URL: &str = "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock";
    """
    got_dir = str_const(unix_src, "BRIDGE_SOCKET_DIR")
    if got_dir != "trollshell":
        failures.append(f"str_const: expected 'trollshell', matched the legacy decoy: {got_dir!r}")
    got_file = str_const(unix_src, "BRIDGE_SOCKET_FILE")
    if got_file != "claude-bridge.sock":
        failures.append(f"str_const: expected 'claude-bridge.sock', got {got_file!r}")
    got_base_url = str_const(unix_src, "BRIDGE_BASE_URL")
    if got_base_url != "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock":
        failures.append(f"str_const: BRIDGE_BASE_URL mismatch, got {got_base_url!r}")

    main_src = """
    //! One route, `POST /v1/chat/completions`, bound to
    //! `$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock` — the path is not
    //! configurable.
    """
    got_doc = first_backtick_literal_with_prefix(main_src, "$XDG_RUNTIME_DIR/")
    if got_doc != "$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock":
        failures.append(
            f"first_backtick_literal_with_prefix: picked {got_doc!r}, "
            "expected the $XDG_RUNTIME_DIR-prefixed literal, not the decoy "
            "`POST /v1/chat/completions` backtick span before it"
        )

    nix_src = """
    port = lib.mkOption {
      type = lib.types.port;
      default = 8787;
    };

    baseUrl = lib.mkOption {
      type = lib.types.str;
      readOnly = true;
      default = "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock";
    };
    """
    got_nix = nix_default_str(nix_src, "baseUrl = lib.mkOption {")
    if got_nix != "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock":
        failures.append(
            f"nix_default_str: expected the baseUrl default, got {got_nix!r} "
            "(picked the unrelated `port` option's default instead?)"
        )

    if compose_base_url("trollshell", "claude-bridge.sock") != (
        "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock"
    ):
        failures.append("compose_base_url: composition does not match the expected shape")
    if compose_doc_path("trollshell", "claude-bridge.sock") != (
        "$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock"
    ):
        failures.append("compose_doc_path: composition does not match the expected shape")

    return failures


def read(path: str) -> str:
    with open(path, encoding="utf-8") as fh:
        return fh.read()


def main() -> int:
    failures = self_test()
    if failures:
        print("bridge-socket scan: SELF-TEST FAILED", file=sys.stderr)
        for line in failures:
            print(f"  {line}", file=sys.stderr)
        print(
            "\nThe scanner disagrees with its own fixtures, so any verdict it gives on the\n"
            "tree is meaningless. Fix the extraction functions rather than the fixtures.",
            file=sys.stderr,
        )
        return 2

    missing = [p for p in (UNIX_RS, BRIDGE_MAIN_RS, MODULE_COMMON_NIX) if not os.path.isfile(p)]
    if missing:
        print(f"bridge-socket scan: file(s) not found: {', '.join(missing)}", file=sys.stderr)
        print("  (run from inside the repository)", file=sys.stderr)
        return 2

    unix_src = read(UNIX_RS)
    main_src = read(BRIDGE_MAIN_RS)
    nix_src = read(MODULE_COMMON_NIX)

    try:
        rust_dir = str_const(unix_src, "BRIDGE_SOCKET_DIR")
        rust_file = str_const(unix_src, "BRIDGE_SOCKET_FILE")
        rust_base_url = str_const(unix_src, "BRIDGE_BASE_URL")
        bridge_doc_path = first_backtick_literal_with_prefix(main_src, "$XDG_RUNTIME_DIR/")
        nix_base_url = nix_default_str(nix_src, "baseUrl = lib.mkOption {")
    except LookupError as e:
        print(f"bridge-socket scan: {e}", file=sys.stderr)
        print(
            "  (an anchor this script depends on has moved or been reworded — "
            "update nix/lint-bridge-socket.py to match)",
            file=sys.stderr,
        )
        return 2

    expected_base_url = compose_base_url(rust_dir, rust_file)
    expected_doc_path = compose_doc_path(rust_dir, rust_file)

    mismatches = []
    if rust_base_url != expected_base_url:
        mismatches.append(
            "hytte-ai-providers self-consistency: unix.rs's BRIDGE_BASE_URL is "
            f"{rust_base_url!r}, but BRIDGE_SOCKET_DIR/BRIDGE_SOCKET_FILE compose "
            f"{expected_base_url!r}"
        )
    if bridge_doc_path != expected_doc_path:
        mismatches.append(
            "hytte-claude-bridge doc drift: main.rs's module doc names "
            f"{bridge_doc_path!r}, but hytte-ai-providers's BRIDGE_SOCKET_DIR/"
            f"BRIDGE_SOCKET_FILE compose {expected_doc_path!r}"
        )
    if nix_base_url != rust_base_url:
        mismatches.append(
            "nix drift: nix/module-common.nix's claudeBridge.baseUrl defaults to "
            f"{nix_base_url!r}, but hytte-ai-providers::BRIDGE_BASE_URL is "
            f"{rust_base_url!r}"
        )

    if mismatches:
        print(
            f"\nERROR: {len(mismatches)} claude-bridge socket name mismatch(es):\n",
            file=sys.stderr,
        )
        for line in mismatches:
            print(f"  - {line}", file=sys.stderr)
        print(
            "\nThe bridge socket path is spelled out in three places that no compiler\n"
            "spans (crates/hytte-ai-providers/src/unix.rs's constants, "
            "crates/hytte-claude-bridge/src/main.rs's\nmodule doc, and "
            "nix/module-common.nix's claudeBridge.baseUrl default) — update whichever\n"
            "side fell behind so the daemon binds the path its clients actually dial.",
            file=sys.stderr,
        )
        return 1

    print(
        f"bridge-socket scan: {rust_base_url!r} — hytte-ai-providers, "
        "hytte-claude-bridge's doc and nix agree",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
