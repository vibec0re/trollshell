#!/usr/bin/env python3
"""Fail if `nix/module-common.nix`'s `core-leds` vocabulary drifts from Rust.

THE DEFECT
----------
`programs.trollshell.config.core-leds` (`nix/module-common.nix`) hand-mirrors
three things from `core-leds.toml`'s Rust schema:

  - `style`'s enum — must list exactly `DisplayStyle::ALL`
    (`crates/hytte-preem/src/style.rs`), in `name()`'s spelling, in `ALL`'s
    order.
  - `fill`'s enum — must match `parse_core_leds_fill`'s accepted strings
    (`trollshell/src/config/core_leds.rs`); `Fill` has no `ALL`/`name()` to
    read this from directly, so the Rust source of truth here is that
    function's own match arms.
  - `rows`'s upper bound (`lib.types.ints.between 0 <n>`) — must equal
    [`MAX_ROWS`] (`trollshell/src/config/core_leds.rs`).

Nothing fails if the two drift: a fifth `DisplayStyle` variant added Rust-side
renders a base file the nix option would reject at eval before anyone ever
gets to `core-leds.toml`; a style renamed Rust-side without the nix edit
renders a base file the shell then rejects per-key at load time, silently,
the first time anyone actually sets it. Since this option is explicitly "the
option shape every later subsystem family copies" (#1041), the mirror is
about to be hand-duplicated nine more times — this is the one place today
that would catch any of them drifting.

WHY THIS IS A NIX LINT AND NOT A `cargo test` (#1081 review, second round)
---------------------------------------------------------------------------
The first version of this guard *was* a `cargo test` in
`trollshell/src/config/core_leds.rs`, reading `nix/module-common.nix` off
disk at test time via `CARGO_MANIFEST_DIR/../nix/module-common.nix`. It
passed locally and failed in CI: `nix/package.nix`'s crane source filter
(`nix/package.nix:79-85`) keeps only `.rs`/`.toml`/`Cargo.lock`,
`assets/hytte-ui/style.css`, and `.vert`/`.frag` files, so the sandboxed
`workspace` derivation `cargo test --workspace` runs inside has no
`nix/module-common.nix` at all — the exact `include_str!`-of-`assets/` trap
CLAUDE.md already documents, just reached by `std::fs::read_to_string`
instead of `include_str!`. Widening the crane filter to keep `*.nix` was
rejected: every edit to *any* `.nix` file would then invalidate the
`workspace` derivation's source hash and force a full recompile
(`nix/package.nix`'s own doc — the filter is deliberately narrow so nix-only
changes are free).

A flake check run by a plain `pkgs.runCommand` (the `bind-pins`/`glsl`
precedent, `nix/lint-bind-pins.py` / `nix/lint-glsl.py`) has the opposite
problem in a good way: it reads the *real repository tree* (there is no
crane filter between a `runCommand`'s `src = ./.;`-shaped input and the
checkout), needs no compile, and reds in seconds. That is what this script
is, wired into `flake.nix`'s `checks.core-leds-vocab`.

Run it by hand from the repo root with:

    nix shell nixpkgs#python3 --command python3 nix/lint-core-leds-vocab.py

The `nix shell` is not optional: **`python3` is deliberately not on the
devShell PATH** (see `nix/lint-bind-pins.py`'s own header), so a bare
`python3 nix/lint-core-leds-vocab.py` is `command not found`.

WHY A HAND-ROLLED SCAN AND NOT A NIX-EVAL ROUND-TRIP
-----------------------------------------------------
Evaluating `nix/module-common.nix` for real (`nix eval` or an in-process
`nix-instantiate`) would need a full module-system `evalModules` call just to
read four literals back out of one option's declaration — heavier than the
thing it is checking, and it could not read the Rust side at all (Nix has no
Rust parser). A small hand-rolled scan of both files, in the
`lint-bind-pins.py` style, is simple enough here that it can name exactly
which anchor it failed to find rather than a bare "no match": both files are
short, `nix fmt`/`rustfmt` already normalise their formatting, and neither
vocabulary is nested more than one bracket deep.

USAGE
-----
    python3 nix/lint-core-leds-vocab.py

Exits 0 when the two agree, 1 naming the first mismatch, 2 when the scan
itself is untrustworthy (a source file is missing, an anchor cannot be found,
or `self_test()` — run first, on every invocation — disagrees with its own
fixtures).
"""

import os
import re
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

STYLE_RS = os.path.join(REPO_ROOT, "crates", "hytte-preem", "src", "style.rs")
CORE_LEDS_RS = os.path.join(REPO_ROOT, "trollshell", "src", "config", "core_leds.rs")
MODULE_COMMON_NIX = os.path.join(REPO_ROOT, "nix", "module-common.nix")


def match_delim(src: str, start: int, open_c: str, close_c: str) -> int:
    """Index just past the delimiter matching the one at/after `start`."""
    depth = 0
    for i in range(start, len(src)):
        if src[i] == open_c:
            depth += 1
        elif src[i] == close_c:
            depth -= 1
            if depth == 0:
                return i + 1
    return -1


def bracket_list_after(src: str, anchor: str) -> list[str]:
    """The quoted strings inside the first `[ … ]` found after `anchor`.

    One bracket depth only — correct for the `nix fmt`-formatted enum lists
    this reads, one item per line.
    """
    start = src.find(anchor)
    if start < 0:
        raise LookupError(f"anchor {anchor!r} not found")
    after = src[start:]
    open_i = after.find("[")
    if open_i < 0:
        raise LookupError(f"no '[' found after anchor {anchor!r}")
    close_i = after.find("]", open_i)
    if close_i < 0:
        raise LookupError(f"no ']' found closing the list after anchor {anchor!r}")
    body = after[open_i + 1 : close_i]
    return [tok.strip('"') for tok in body.split() if tok.strip('"')]


def ints_between_after(src: str, anchor: str) -> tuple[int, int]:
    """The two integers passed to a `lib.types.ints.between lo hi` call."""
    start = src.find(anchor)
    if start < 0:
        raise LookupError(f"anchor {anchor!r} not found")
    after = src[start + len(anchor) :]
    tokens = after.split()
    if len(tokens) < 2:
        raise LookupError(f"fewer than two integers after anchor {anchor!r}")
    lo = int(tokens[0])
    hi = int(tokens[1].rstrip(")"))
    return (lo, hi)


def display_style_all(src: str) -> list[str]:
    """`DisplayStyle`'s canonical spelling, in `ALL`'s order.

    Reads `pub const ALL: [Self; N] = [Self::Vfd, Self::Lcd, …];` for the
    variant *order*, then `fn name(self) -> &'static str { … }`'s match arms
    for the variant -> string mapping, and composes the two — this is exactly
    what `DisplayStyle::ALL.iter().map(|s| s.name())` computes at runtime, so
    the nix side is checked against the same sequence the Rust schema itself
    would resolve.
    """
    m = re.search(r"pub const ALL:\s*\[Self;\s*\d+\]\s*=\s*\[([^\]]*)\];", src)
    if not m:
        raise LookupError("DisplayStyle::ALL not found in style.rs")
    variants = [v.strip().removeprefix("Self::") for v in m.group(1).split(",") if v.strip()]

    fn = re.search(r"fn name\(self\)\s*->\s*&'static str\s*\{", src)
    if not fn:
        raise LookupError("DisplayStyle::name() not found in style.rs")
    body_start = fn.end() - 1
    body_end = match_delim(src, body_start, "{", "}")
    if body_end < 0:
        raise LookupError("DisplayStyle::name()'s body brace never closes")
    body = src[body_start:body_end]

    name_of = dict(re.findall(r"Self::(\w+)\s*=>\s*\"([^\"]+)\"", body))
    missing = [v for v in variants if v not in name_of]
    if missing:
        raise LookupError(f"name() has no arm for ALL variant(s): {missing}")
    return [name_of[v] for v in variants]


def fill_parser_vocab(src: str) -> list[str]:
    """`parse_core_leds_fill`'s accepted strings, in source order.

    `Fill` (`crates/hytte-preem/src/led_matrix.rs`) has no `ALL`/`name()` to
    read this from the way `DisplayStyle` does, so the source of truth is
    this parser's own match arms — the single judge `core_leds.rs`'s own
    module doc already calls it.
    """
    fn = re.search(r"fn parse_core_leds_fill\(raw: &str\) -> Result<Fill, &str>\s*\{", src)
    if not fn:
        raise LookupError("parse_core_leds_fill not found in core_leds.rs")
    body_start = fn.end() - 1
    body_end = match_delim(src, body_start, "{", "}")
    if body_end < 0:
        raise LookupError("parse_core_leds_fill's body brace never closes")
    body = src[body_start:body_end]
    return re.findall(r'"([^"]+)"\s*=>\s*Ok\(Fill::', body)


def max_rows(src: str) -> int:
    m = re.search(r"const MAX_ROWS:\s*usize\s*=\s*(\d+)\s*;", src)
    if not m:
        raise LookupError("MAX_ROWS not found in core_leds.rs")
    return int(m.group(1))


# Fixtures for `self_test()`, run on every invocation. A clean tree proves
# nothing about whether the extraction functions still work — only a case
# built to disagree can tell the two apart (the same reasoning
# `lint-bind-pins.py`'s header gives for its own fixtures).
def self_test() -> list[str]:
    failures = []

    style_src = """
    pub const ALL: [Self; 4] = [Self::Vfd, Self::Lcd, Self::Oled, Self::Crt];
    pub fn name(self) -> &'static str {
        match self {
            Self::Vfd => "vfd",
            Self::Lcd => "lcd",
            Self::Oled => "oled",
            Self::Crt => "crt",
        }
    }
    """
    got = display_style_all(style_src)
    if got != ["vfd", "lcd", "oled", "crt"]:
        failures.append(f"display_style_all: expected the four in ALL's order, got {got}")

    fill_src = """
    fn parse_core_leds_fill(raw: &str) -> Result<Fill, &str> {
        match raw {
            "spare" => Ok(Fill::Spare),
            "blank" => Ok(Fill::Blank),
            other => Err(other),
        }
    }
    """
    got = fill_parser_vocab(fill_src)
    if got != ["spare", "blank"]:
        failures.append(f"fill_parser_vocab: expected [spare, blank], got {got}")
    if "other" in got:
        failures.append("fill_parser_vocab: the catch-all arm must never be captured")

    if max_rows("const MAX_ROWS: usize = 64;") != 64:
        failures.append("max_rows: did not read the literal back")
    # A number sharing digits with a nearby, differently-named constant must
    # not be picked up instead.
    if max_rows("const OTHER_ROWS: usize = 6;\nconst MAX_ROWS: usize = 64;") != 64:
        failures.append("max_rows: matched the wrong constant")

    nix_src = """
    style = lib.mkOption {
      type = lib.types.nullOr (
        lib.types.enum [
          "vfd"
          "lcd"
          "oled"
          "crt"
        ]
      );
    };
    rows = lib.mkOption {
      type = lib.types.nullOr (
        lib.types.either (lib.types.ints.between 0 64) (lib.types.enum [ "rect" ])
      );
    };
    """
    got = bracket_list_after(nix_src, "style = lib.mkOption {")
    if got != ["vfd", "lcd", "oled", "crt"]:
        failures.append(f"bracket_list_after: expected the four nix-side, got {got}")
    got_bounds = ints_between_after(nix_src, "ints.between ")
    if got_bounds != (0, 64):
        failures.append(f"ints_between_after: expected (0, 64), got {got_bounds}")

    return failures


def read(path: str) -> str:
    with open(path, encoding="utf-8") as fh:
        return fh.read()


def main() -> int:
    failures = self_test()
    if failures:
        print("core-leds-vocab scan: SELF-TEST FAILED", file=sys.stderr)
        for line in failures:
            print(f"  {line}", file=sys.stderr)
        print(
            "\nThe scanner disagrees with its own fixtures, so any verdict it gives on the\n"
            "tree is meaningless. Fix the extraction functions rather than the fixtures.",
            file=sys.stderr,
        )
        return 2

    missing = [p for p in (STYLE_RS, CORE_LEDS_RS, MODULE_COMMON_NIX) if not os.path.isfile(p)]
    if missing:
        print(f"core-leds-vocab scan: file(s) not found: {', '.join(missing)}", file=sys.stderr)
        print("  (run from inside the repository)", file=sys.stderr)
        return 2

    style_src = read(STYLE_RS)
    core_leds_src = read(CORE_LEDS_RS)
    nix_src = read(MODULE_COMMON_NIX)

    try:
        rust_style = display_style_all(style_src)
        nix_style = bracket_list_after(nix_src, "style = lib.mkOption {")
        rust_fill = fill_parser_vocab(core_leds_src)
        nix_fill = bracket_list_after(nix_src, "fill = lib.mkOption {")
        rust_max_rows = max_rows(core_leds_src)
        nix_rows_lo, nix_rows_hi = ints_between_after(nix_src, "ints.between ")
    except LookupError as e:
        print(f"core-leds-vocab scan: {e}", file=sys.stderr)
        print(
            "  (an anchor this script depends on has moved or been reworded — "
            "update nix/lint-core-leds-vocab.py to match)",
            file=sys.stderr,
        )
        return 2

    mismatches = []
    if nix_style != rust_style:
        mismatches.append(
            f"style: nix/module-common.nix has {nix_style}, "
            f"but DisplayStyle::ALL is {rust_style}"
        )
    if nix_fill != rust_fill:
        mismatches.append(
            f"fill: nix/module-common.nix has {nix_fill}, "
            f"but parse_core_leds_fill accepts {rust_fill}"
        )
    if nix_rows_lo != 0:
        mismatches.append(
            f"rows: nix/module-common.nix's lower bound is {nix_rows_lo}, expected 0 "
            "(the file's own spelling of `rect`)"
        )
    if nix_rows_hi != rust_max_rows:
        mismatches.append(
            f"rows: nix/module-common.nix's upper bound is {nix_rows_hi}, "
            f"but MAX_ROWS is {rust_max_rows}"
        )

    if mismatches:
        print(
            f"\nERROR: {len(mismatches)} core-leds vocabulary mismatch(es) between "
            "nix/module-common.nix and trollshell's Rust schema:\n",
            file=sys.stderr,
        )
        for line in mismatches:
            print(f"  - {line}", file=sys.stderr)
        print(
            "\nnix/module-common.nix's `programs.trollshell.config.core-leds` hand-mirrors "
            "this\nvocabulary (see that option's own description) — update whichever side "
            "fell\nbehind so a base-layer render and the shell's own parser agree.",
            file=sys.stderr,
        )
        return 1

    print(
        f"core-leds-vocab scan: style {rust_style}, fill {rust_fill}, "
        f"rows 0-{rust_max_rows} — nix and Rust agree",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
