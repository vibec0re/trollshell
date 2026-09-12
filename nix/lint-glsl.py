#!/usr/bin/env python3
"""Compile every shipped preem GLSL source, in the dialect the shell compiles it in.

WHY THIS EXISTS
---------------
The preem GL renderer (#893 stage B) ships its shaders as `include_str!`'d
`*.vert`/`*.frag` under `trollshell/src/plugins/preem_gl/`. Nothing in
`cargo test` looks inside them: they are opaque `&'static str`s until a driver
compiles them, and CI has no driver — `nix flake check`'s system-tests bucket
runs `xvfb-run` in a sandbox with no `/dev/dri` and no mesa in the closure. So
a typo'd identifier, a type error or a `#version 320 es` violation would ship
green and only surface as a blank chip on Annika's glass.

The design spec's CI table asked for this row and named **naga**
(`front::glsl`) as the validator. Measured, naga cannot do it: naga 26's GLSL
frontend rejects the whole ES profile — `#version 300/310/320 es` each come
back `InvalidVersion(N)` + `InvalidProfile("es")` — so the spec's own fallback
("drop to `310 es`") does not reach either, and even at `#version 450` it stops
at `NotImplemented("variable qualifier")` on the `flat in`/`precision`
declarations these shaders are written with. The spec's reason for preferring
naga over glslang was that glslang is C++ FFI that would have to live in the
`hytte-gl` unsafe island and drag a build closure — a real objection to
*linking* a validator into the shell for #893's untrusted plugin shaders, and
no objection at all to *invoking the binary* from a nix check. So this check
runs the reference ES compiler over the shipped sources at build time, adds
zero `Cargo.lock` entries, and goes red in seconds on a cold checkout.

#893's own trust boundary — validating an untrusted plugin's shader at
*runtime*, before it reaches the driver — is a different problem, and it was
answered by deciding there is no such validator: the plugin socket is the
boundary (route 0), naga cannot read a plugin's ES shader anyway, and this
script only ever sees sources that live in this tree.

WHAT IT CHECKS
--------------
Exactly what the shell compiles, assembled the same way:

  * the version/precision header is read out of `crates/hytte-ui/src/
    gl_surface.rs`'s `GLSL_HEADER` const rather than repeated here, so a
    dialect change cannot leave the check validating the old one;
  * a body that is spliced by a `concat!` in any `*.rs` beside the shaders
    (the blur's `BLUR_DIR`, the gauge's `LAYER`) is compiled once per splice,
    with the splice text, because that body does not compile on its own — and
    compiling it "as written" would be checking a source that never ships;
  * every other body is compiled as-is, at the stage its extension names;
  * **#893's shader widget** the same way: its vertex stage
    (`shader_*.vert`, anywhere under `crates/hytte-ui/src/`) compiles as
    written, and **every** plugin-supplied fragment *body* the crane filter
    ships — that is, every `.frag` anywhere in the tree that `preem_gl/` does
    not already own — compiles with the interface `SHADER_PREAMBLE` spliced in
    front of it, read out of `shader_surface.rs`, so adding a uniform to the
    published contract changes what this validates in the same commit.

    The scan really is tree-wide now: repo root, recursive, for both
    `.frag` and `.vert`, minus `target/`, `.git/`, `.claude/`, `.direnv/` and
    a `result`/`result-<output>` build-output symlink (none of which ever
    carry shader source — see `EXCLUDED_DIR_NAMES`). `nix/package.nix`'s
    filter is
    `lib.hasSuffix ".frag"` / `".vert"` on the full path with **no** directory
    constraint at all, so anything narrower than the whole tree agrees with
    that filter by convention rather than by construction — and convention
    shipped real holes twice: a one-entry literal list, then a
    `crates/*/shaders` glob, each missed a real `.frag` the filter still ships
    (#968's second review, the L5 residual). `.vert` had the same hole one
    layer further in, since it was never scanned outside `preem_gl/` and a
    *non-recursive* `WIDGET_SHADER_DIR.glob("shader_*.vert")` (#980). A
    `.vert` found outside both of those two shapes is not one this script has
    an assembly recipe for — unlike a `.frag`, which is unconditionally a
    widget body — so it is a hard failure (exit 2) rather than a shader this
    check silently never compiled.

It refuses to pass vacuously, and every one of those guards is exit **2** (a
broken check) rather than exit 1 (a broken shader): a missing header, a missing
shader directory, an unknown extension, a subdirectory it does not know how to
compile, a `.vert` file outside the two known shapes, fewer shaders than
`MIN_SHADERS`, fewer compilations than `MIN_COMPILATIONS`, or no spliced body
at all. Deleting `scope_decay.frag` — the file carrying the whole phosphor
recurrence — used to be green.

WHAT IT DOES NOT CHECK
----------------------
Each stage is compiled **alone**, so the vertex↔fragment varying interface is
not validated: `fullscreen.vert`'s `out vec2 v_uv` against `blur.frag`'s
`in vec2 v_uv` would still link at runtime with a mismatched type or a missing
declaration on one side. Linking the pairs here would mean teaching this script
the pipeline's pass table, which lives in `program.rs` as Rust — the same
information `GlPipeline` already carries and `GlSurface` already links for real
on glass. Uniform-name drift against the bag *is* covered, from the other side:
`the_mapping_fills_every_uniform_the_shaders_read` parses these same files and
asserts both directions.

RUNNING IT BY HAND
------------------
    nix shell nixpkgs#python3 nixpkgs#glslang --command python3 nix/lint-glsl.py

from the repo root. `nix flake check`'s `glsl` check runs the same line.
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

# The shader directory, and the two Rust files the assembly is derived from.
SHADER_DIR = Path("trollshell/src/plugins/preem_gl")
HEADER_SOURCE = Path("crates/hytte-ui/src/gl_surface.rs")
PROGRAM_SOURCE = SHADER_DIR / "program.rs"

# ── #893's shader widget ─────────────────────────────────────────────────────
#
# A plugin's fragment body is assembled differently: header + the interface
# PREAMBLE + the body, with no vertex stage of its own. Bodies that live in this
# tree get compiled here for the same reason the preem ones do — a typo would
# otherwise ship green and surface as an empty rect on glass, with only a
# journal line to say so.
#
# A plugin's *runtime* source is a different problem with a different answer and
# this script does not touch it: #893 settled that the plugin socket is the
# trust boundary, so there is no runtime validator at all (and naga, the only
# Rust GLSL frontend in reach, cannot parse the ES profile anyway).
WIDGET_SHADER_DIR = Path("crates/hytte-ui/src")
"""Where the widget's own **vertex** stage lives — compiled as written."""

PREAMBLE_SOURCE = WIDGET_SHADER_DIR / "shader_surface.rs"
"""The Rust file carrying `SHADER_PREAMBLE`, read rather than duplicated."""

# `nix/package.nix`'s crane filter is `lib.hasSuffix ".frag"` / `".vert"` on
# the full repo-relative path — **no** directory constraint at all. Two prior
# spellings of this scan each agreed with that filter by convention rather
# than by construction, and each shipped a real hole this way: a one-entry
# literal list (`WIDGET_BODY_DIRS = [Path("crates/hytte-plugin-preem-demo/
# shaders")]`) missed a second plugin's `shaders/` dir entirely; the
# `crates/*/shaders` glob that replaced it still missed
# `crates/<crate>/src/stray.frag` and `trollshell/shaders/stray2.frag`, both
# measured shipping green (#968 second review, the L5 residual). `.vert` had
# the same shape of hole one layer further in — it was never scanned outside
# `SHADER_DIR` and a *non-recursive*, name-prefixed
# `WIDGET_SHADER_DIR.glob("shader_*.vert")` (#980). The only spelling that
# agrees with the filter by construction, for both extensions, is a walk of
# the whole tree.
#
# `EXCLUDED_DIR_NAMES` is *not* part of matching the filter — the filter has
# no such list — it exists purely so this scanner does not walk into
# `target/` (large, and never shader source), `.git/` (ditto, plus binary
# objects), `.claude/` / `.direnv/` (tool state, already `.gitignore`d), or a
# `result`/`result-<output>` symlink left by a previous `nix build` (which
# resolves into the store — arbitrarily large, and a candidate for a walk
# cycle if `os.walk` ever *did* follow it, which is why the pruning happens
# during the walk rather than as a filter on its results; see
# `find_tree_wide`).
#
# The nix-symlink match is deliberately narrow — `name == "result" or
# name.startswith("result-")`, the exact two shapes `nix build` names its
# output links (`result`, or `result-<output>` for a multi-output
# derivation) — **not** a bare `name.startswith("result")`. A prefix match
# would prune any legitimately-tracked directory that happens to start with
# those letters (`results/`, `resultset/`, `result_cache/`, …), which is
# precisely the same *convention*-shaped hole this scan exists to close for
# `.frag`/`.vert` discovery itself, just relocated into the exclusion list.
EXCLUDED_DIR_NAMES = {"target", ".git", ".claude", ".direnv"}


def _is_excluded_dir(name: str) -> bool:
    """`target`/`.git`/`.claude`/`.direnv`, or a `result`/`result-<output>` link."""
    return name in EXCLUDED_DIR_NAMES or name == "result" or name.startswith("result-")


def find_tree_wide(suffix: str) -> list[Path]:
    """Every file under the repo root ending in `suffix`, minus excluded dirs.

    Walks with `os.walk` rather than `Path.rglob`: dropping a name from
    `dirnames` prunes that subtree *before* `os.walk` descends into it, where
    filtering `rglob`'s results after the fact would still have walked (and,
    for a `result` symlink, `rglob` follows symlinks — walked *through*)
    everything first. `os.walk`'s default `followlinks=False` means a
    `result` symlink is never entered at all, which is the point.
    """
    found: list[Path] = []
    for dirpath, dirnames, filenames in os.walk("."):
        dirnames[:] = [d for d in dirnames if not _is_excluded_dir(d)]
        found.extend(Path(dirpath) / name for name in filenames if name.endswith(suffix))
    return sorted(found)


def widget_bodies() -> list[Path]:
    """Every `.frag` the crane filter ships that the preem half does not own.

    Unconditional: any `.frag` outside `SHADER_DIR` *is* a widget body — there
    is no narrower convention left to check it against, on purpose (see the
    comment above `EXCLUDED_DIR_NAMES`).
    """
    return sorted(path for path in find_tree_wide(".frag") if SHADER_DIR not in path.parents)


def widget_vertex_stages() -> tuple[list[Path], list[Path]]:
    """`(known, unknown)` `.vert` files outside `SHADER_DIR`.

    Unlike a fragment body, a vertex stage has no catch-all recipe: the only
    known assembly is "as written", and the only known *location* for a #893
    widget vertex stage is anywhere under `WIDGET_SHADER_DIR`, name-prefixed
    `shader_*.vert` — the convention `shader_fullscreen.vert` set, and nothing
    has changed since. A `.vert` found anywhere else outside `SHADER_DIR` is
    not a shape this script has a recipe for, so `main` fails loudly on it
    (exit 2) instead of guessing an assembly, or — worse — going back to
    silently never compiling it, which is exactly the hole this tree-wide walk
    exists to close.
    """
    known: list[Path] = []
    unknown: list[Path] = []
    for path in find_tree_wide(".vert"):
        if SHADER_DIR in path.parents:
            continue
        if WIDGET_SHADER_DIR in path.parents and path.name.startswith("shader_"):
            known.append(path)
        else:
            unknown.append(path)
    return sorted(known), sorted(unknown)


# Floors, on the same "current counts, not counts-with-headroom" rule as
# MIN_SHADERS above: a lint that tolerates a missing file cannot tell a deletion
# from a tidy-up.
MIN_WIDGET_STAGES = 1  # shader_fullscreen.vert
MIN_WIDGET_BODIES = 1  # the preem demo's spectrum.frag

# The floors. These are the **current** counts, not counts-with-headroom, and
# the difference is the point: a lint that tolerates one missing file cannot
# tell a deletion from a tidy-up. Adding a shader means bumping the number in
# the same commit, which is a two-second edit and a real review signal;
# `lint-bind-pins.py` sizes its floor with slack because it counts call sites
# across three trees, which is a different problem.
#
# `scope_decay.frag` carries the whole `(v * retained) >> 8` phosphor
# recurrence, and with a floor of five against six files its deletion was
# green.
MIN_SHADERS = 7
# Compilations, not files: `blur.frag` is one body compiled twice. A
# splice that stops being found (a moved `include_str!` path, a `concat!` this
# script's parser stops recognising) drops this — today that shows up as a
# compile failure only because the body happens not to build without its
# splice, which is luck rather than a guard.
#
# Since #893 this counts all three groups (6 preem files → 7 compilations, plus
# 1 widget stage and 1 widget body). Bumped 7 → 9 with them rather than left
# with two compilations of slack: the whole point of a floor at the current
# count is that it cannot tolerate a deletion, and the two per-group floors
# below do not add up to this one on their own.
MIN_COMPILATIONS = 11
# Distinct bodies that must be spliced rather than compiled as written.
MIN_SPLICED_BODIES = 2

# `glslangValidator` names the stage by extension. `.glsl` is deliberately
# **absent**: it names no stage, so this script could not compile one, and
# `nix/package.nix`'s crane filter does not keep it either — the two files agree
# on exactly this set. A shared body added later needs an entry in both, plus a
# decision about which stage(s) to compile it under.
STAGES = {".vert": "vert", ".frag": "frag"}


def fail(message: str) -> None:
    """Print a diagnostic and exit 2 — the "the check itself is broken" code."""
    print(f"lint-glsl: {message}", file=sys.stderr)
    sys.exit(2)


def unescape_rust(literal: str) -> str:
    """Decode the escapes a Rust string literal can carry in this position.

    Only the three that appear in a GLSL header, spelled out rather than routed
    through `codecs.decode(..., "unicode_escape")` — which would also mangle
    any non-ASCII byte it met.
    """
    out = literal.replace("\\\\", "\x00")
    out = out.replace("\\n", "\n").replace("\\t", "\t").replace('\\"', '"')
    return out.replace("\x00", "\\")


def read_header() -> str:
    """The `GLSL_HEADER` const, as the shell's `Program::compile` prepends it.

    Read out of the Rust source instead of duplicated here so that changing the
    dialect (`320 es` → `310 es`, say — the spec's decision 6) changes what this
    check validates, in the same commit, with nothing to remember.
    """
    if not HEADER_SOURCE.is_file():
        fail(f"{HEADER_SOURCE} is missing — wrong root, or the module moved")
    text = HEADER_SOURCE.read_text(encoding="utf-8")
    match = re.search(
        r"const\s+GLSL_HEADER\s*:\s*&str\s*=\s*(\"(?:[^\"\\]|\\.)*\")\s*;",
        text,
        re.DOTALL,
    )
    if not match:
        fail(f"no `const GLSL_HEADER: &str = \"…\";` in {HEADER_SOURCE}")
    header = unescape_rust(match.group(1)[1:-1])
    if "#version" not in header:
        fail(f"GLSL_HEADER carries no `#version` directive: {header!r}")
    return header


def read_preamble() -> str:
    """The `SHADER_PREAMBLE` const, as `ShaderSurface::draw` splices it (#893).

    A plugin ships a fragment *body*; the shell prepends the version header and
    then this, which declares `v_uv`, `fragColor` and every uniform the contract
    publishes. Compiling a body without it fails on the first identifier, so a
    check that skipped this would be checking a source that never ships.

    Read out of the Rust source for the same reason `read_header` is: adding a
    uniform to the contract then changes what this validates, in the same
    commit, with nothing to remember. The const is a **raw** string (`r"…"`), so
    there are no escapes to undo — keep it that way if you touch it.
    """
    if not PREAMBLE_SOURCE.is_file():
        fail(f"{PREAMBLE_SOURCE} is missing — wrong root, or the module moved")
    text = PREAMBLE_SOURCE.read_text(encoding="utf-8")
    match = re.search(
        r'const\s+SHADER_PREAMBLE\s*:\s*&str\s*=\s*r"((?:[^"])*)"\s*;',
        text,
        re.DOTALL,
    )
    if not match:
        fail(f'no `const SHADER_PREAMBLE: &str = r"…";` in {PREAMBLE_SOURCE}')
    preamble = match.group(1)
    if "fragColor" not in preamble:
        fail(f"SHADER_PREAMBLE declares no `fragColor` output: {preamble!r}")
    return preamble


def concat_bodies(text: str) -> list[str]:
    """The argument list of every `concat!( … )` in `text`.

    Paren-matched rather than regexed, for the same reason `lint-bind-pins.py`
    brace-matches: the *live* argument list here is
    `"const ivec2 BLUR_DIR = ivec2(1, 0);\\n"`, and a non-greedy
    `concat!\\((.*?)\\)\\s*;` stops dead at the `);` **inside that string
    literal**, silently yielding a truncated body with no `include_str!` in it —
    which this check would then read as "no splice" and compile a shader that
    cannot compile. String literals are skipped explicitly so a paren inside one
    cannot unbalance the walk either.
    """
    bodies: list[str] = []
    for opening in re.finditer(r"concat!\(", text):
        i = opening.end()
        depth = 1
        while i < len(text) and depth:
            char = text[i]
            if char == '"':
                i += 1
                while i < len(text) and text[i] != '"':
                    i += 2 if text[i] == "\\" else 1
            elif char == "(":
                depth += 1
            elif char == ")":
                depth -= 1
                if depth == 0:
                    break
            i += 1
        if depth == 0:
            bodies.append(text[opening.end() : i])
    return bodies


def read_splices() -> dict[str, list[str]]:
    """Prefix text spliced ahead of a shader body by a `concat!` in this dir.

    `blur.frag` is the original case: it is one body compiled twice, with
    `const ivec2 BLUR_DIR = …;` prepended, because `GlUniforms` is one bag
    applied to every pass and there is nowhere to say "this pass is the
    horizontal one". Such a body does **not** compile on its own, so checking
    it as written would be checking a source that never ships. `gauge.frag`
    (#1143) is the second, with `const int LAYER = …;`.

    **Every `*.rs` in `SHADER_DIR`, not just `program.rs`** — the scan was
    named-file-scoped while the scope was the only pipeline, and the second
    pipeline landing in its own `gauge.rs` beside it would have gone back to
    compiling a spliced body as written (which is to say: failing, loudly, on
    an identifier the splice defines — the lucky failure mode, not a guarantee).
    The same reasoning as the tree-wide `.frag` walk above: agree with where
    the code actually is, by construction.
    """
    sources = sorted(SHADER_DIR.glob("*.rs"))
    if PROGRAM_SOURCE not in sources:
        fail(f"{PROGRAM_SOURCE} is missing — wrong root, or the module moved")
    text = "\n".join(path.read_text(encoding="utf-8") for path in sources)
    splices: dict[str, list[str]] = {}
    for body in concat_bodies(text):
        included = re.findall(r'include_str!\(\s*"([^"]+)"\s*\)', body)
        # The `include_str!` argument is a *path*, not shader text: strip the
        # whole call before collecting literals, or the filename would be
        # spliced into the source as GLSL.
        without_includes = re.sub(r'include_str!\(\s*"[^"]+"\s*\)', "", body)
        prefixes = [
            unescape_rust(lit) for lit in re.findall(r'"((?:[^"\\]|\\.)*)"', without_includes)
        ]
        if len(included) != 1 or not prefixes:
            continue
        splices.setdefault(included[0], []).append("".join(prefixes))
    return splices


def main() -> int:
    if not SHADER_DIR.is_dir():
        fail(f"{SHADER_DIR} is missing — wrong root, or the renderer moved")
    header = read_header()
    splices = read_splices()

    # **Recursive.** `iterdir()` missed anything one directory down, and a
    # directory has no `.suffix` and is not `is_file()`, so a subdirectory fell
    # out of the comprehension *without* reaching the unknown-extension guard
    # below. The crane filter is `lib.hasSuffix ".frag"` on the full path, so
    # directory position is irrelevant to *it* — a broken shader in
    # `preem_gl/shaders/` would ship and this check would never see it, which is
    # precisely the trap it exists to close.
    shaders = sorted(
        path
        for path in SHADER_DIR.rglob("*")
        if path.is_file() and path.suffix not in {".rs"}
    )
    unknown = [path for path in shaders if path.suffix not in STAGES]
    if unknown:
        fail(
            "unknown shader extension(s), so this check does not know which stage to "
            f"compile them as: {', '.join(str(p) for p in unknown)}"
        )
    if len(shaders) < MIN_SHADERS:
        fail(f"only {len(shaders)} shader(s) under {SHADER_DIR}; expected ≥ {MIN_SHADERS}")
    spliced = sum(1 for path in shaders if splices.get(path.name))
    if spliced < MIN_SPLICED_BODIES:
        fail(
            f"{spliced} spliced shader bodies, expected ≥ {MIN_SPLICED_BODIES} — the "
            f"`concat!` scan of {SHADER_DIR}/*.rs found too few, so a body that only "
            "compiles with its prefix would be compiled as written"
        )

    failures = 0
    compiled = 0
    widget_stages = 0
    widget_body_count = 0
    with tempfile.TemporaryDirectory() as tmp:
        staged_index = 0

        def compile_assembled(label: str, prefix: str, path: Path) -> None:
            """Assemble `header + prefix + path` and run glslangValidator on it."""
            nonlocal failures, compiled, staged_index
            source = f"{header}\n{prefix}{path.read_text(encoding='utf-8')}"
            staged = Path(tmp) / f"{path.stem}.{staged_index}{path.suffix}"
            staged_index += 1
            staged.write_text(source, encoding="utf-8")
            result = subprocess.run(
                ["glslangValidator", str(staged)],
                capture_output=True,
                text=True,
                check=False,
            )
            compiled += 1
            if result.returncode == 0:
                print(f"  ok    {label}")
                return
            failures += 1
            print(f"  FAIL  {label}")
            # The line numbers are into the *assembled* source, which is what
            # the driver sees too — so print the prepended line count to make
            # them translatable back to the file on disk.
            offset = header.count("\n") + 1 + prefix.count("\n")
            print(f"        (line numbers include {offset} prepended header/splice lines)")
            for line in (result.stdout + result.stderr).splitlines():
                if line.strip():
                    print(f"        {line}")

        for path in shaders:
            for index, prefix in enumerate(splices.get(path.name, [""])):
                label = path.name if prefix == "" else f"{path.name} (splice {index})"
                compile_assembled(label, prefix, path)

        # ── #893's shader widget ─────────────────────────────────────────────
        #
        # Two shapes, and they are assembled differently. The widget's own
        # vertex stage is a complete shader and compiles as written; a plugin's
        # fragment *body* only compiles with the interface preamble in front of
        # it, which is precisely the splice case the preem half already has.
        preamble = read_preamble()
        known_stages, unknown_verts = widget_vertex_stages()
        if unknown_verts:
            fail(
                "found `.vert` file(s) outside preem_gl/ that are not "
                f"`{WIDGET_SHADER_DIR}/**/shader_*.vert` — this script has no assembly "
                "recipe for them, so it cannot silently compile them (possibly wrong) or "
                "silently skip them (the exact hole a tree-wide scan exists to close): "
                f"{', '.join(str(p) for p in unknown_verts)}"
            )
        for path in known_stages:
            widget_stages += 1
            compile_assembled(f"{path} (widget vertex stage)", "", path)
        # A `shaders/` directory is the *convention* for widget bodies, and
        # this keeps it honest: a non-`.frag` file in one is a file whose stage
        # nobody has decided, and it would ship only if it were a `.frag`.
        for directory in sorted(Path("crates").glob("*/shaders")):
            unknown = [
                q for q in sorted(directory.rglob("*")) if q.is_file() and q.suffix != ".frag"
            ]
            if unknown:
                fail(
                    "a shader-widget body directory may hold only `.frag` bodies (they are "
                    "compiled with the fragment preamble in front); found: "
                    f"{', '.join(str(q) for q in unknown)}"
                )
        # …the *scan* is every `.frag` the crane filter ships, wherever it
        # sits under the repo root, because that filter has no directory
        # constraint. Paths are printed in full: with the scan tree-wide, a
        # bare filename no longer says which crate it came from.
        for path in widget_bodies():
            widget_body_count += 1
            compile_assembled(f"{path} (widget body)", preamble, path)

    print(
        f"lint-glsl: {compiled} shader compilation(s) "
        f"({widget_stages} widget stage(s), {widget_body_count} widget body(ies)), "
        f"{failures} failed"
    )
    if compiled < MIN_COMPILATIONS:
        fail(
            f"only {compiled} compilation(s), expected ≥ {MIN_COMPILATIONS} — a shader "
            "or a splice went missing, so this run checked less than it should have"
        )
    if widget_stages < MIN_WIDGET_STAGES:
        fail(
            f"only {widget_stages} shader-widget stage(s), expected ≥ {MIN_WIDGET_STAGES} — "
            f"the `shader_*.vert` scan of {WIDGET_SHADER_DIR} found nothing, so #893's "
            "vertex stage went unchecked"
        )
    if widget_body_count < MIN_WIDGET_BODIES:
        fail(
            f"only {widget_body_count} shader-widget body(ies), expected ≥ {MIN_WIDGET_BODIES} — "
            "a bundled plugin's fragment body went missing, so the one artifact proving "
            "the #893 contract compiles was never compiled"
        )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
