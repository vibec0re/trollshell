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
*runtime*, before it reaches the driver — is a different problem with a
different answer, and this script does not solve it.

WHAT IT CHECKS
--------------
Exactly what the shell compiles, assembled the same way:

  * the version/precision header is read out of `crates/hytte-ui/src/
    gl_surface.rs`'s `GLSL_HEADER` const rather than repeated here, so a
    dialect change cannot leave the check validating the old one;
  * a body that is spliced by `program.rs`'s `concat!` (the blur's
    `BLUR_DIR`) is compiled once per splice, with the splice text, because
    that body does not compile on its own — and compiling it "as written"
    would be checking a source that never ships;
  * every other body is compiled as-is, at the stage its extension names.

It refuses to pass vacuously, and every one of those guards is exit **2** (a
broken check) rather than exit 1 (a broken shader): a missing header, a missing
shader directory, an unknown extension, a subdirectory it does not know how to
compile, fewer shaders than `MIN_SHADERS`, fewer compilations than
`MIN_COMPILATIONS`, or no spliced body at all. Deleting `scope_decay.frag` —
the file carrying the whole phosphor recurrence — used to be green.

WHAT IT DOES NOT CHECK
----------------------
Each stage is compiled **alone**, so the vertex↔fragment varying interface is
not validated: `fullscreen.vert`'s `out vec2 v_uv` against `scope_blur.frag`'s
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

import re
import subprocess
import sys
import tempfile
from pathlib import Path

# The shader directory, and the two Rust files the assembly is derived from.
SHADER_DIR = Path("trollshell/src/plugins/preem_gl")
HEADER_SOURCE = Path("crates/hytte-ui/src/gl_surface.rs")
PROGRAM_SOURCE = SHADER_DIR / "program.rs"

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
MIN_SHADERS = 6
# Compilations, not files: `scope_blur.frag` is one body compiled twice. A
# splice that stops being found (a moved `include_str!` path, a `concat!` this
# script's parser stops recognising) drops this to 6 — today that shows up as a
# compile failure only because the body happens not to build without its
# splice, which is luck rather than a guard.
MIN_COMPILATIONS = 7
# Distinct bodies that must be spliced rather than compiled as written.
MIN_SPLICED_BODIES = 1

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
    """Prefix text spliced ahead of a shader body by `program.rs`'s `concat!`.

    `scope_blur.frag` is the live case: it is one body compiled twice, with
    `const ivec2 BLUR_DIR = …;` prepended, because `GlUniforms` is one bag
    applied to every pass and there is nowhere to say "this pass is the
    horizontal one". Such a body does **not** compile on its own, so checking
    it as written would be checking a source that never ships.
    """
    if not PROGRAM_SOURCE.is_file():
        fail(f"{PROGRAM_SOURCE} is missing — wrong root, or the module moved")
    text = PROGRAM_SOURCE.read_text(encoding="utf-8")
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
            f"`concat!` scan of {PROGRAM_SOURCE} found nothing, so a body that only "
            "compiles with its prefix would be compiled as written"
        )

    failures = 0
    compiled = 0
    with tempfile.TemporaryDirectory() as tmp:
        for path in shaders:
            for index, prefix in enumerate(splices.get(path.name, [""])):
                source = f"{header}\n{prefix}{path.read_text(encoding='utf-8')}"
                staged = Path(tmp) / f"{path.stem}.{index}{path.suffix}"
                staged.write_text(source, encoding="utf-8")
                label = path.name if prefix == "" else f"{path.name} (splice {index})"
                result = subprocess.run(
                    ["glslangValidator", str(staged)],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                compiled += 1
                if result.returncode == 0:
                    print(f"  ok    {label}")
                    continue
                failures += 1
                print(f"  FAIL  {label}")
                # The line numbers are into the *assembled* source, which is
                # what the driver sees too — so print the header's line count
                # to make them translatable back to the file on disk.
                offset = header.count("\n") + 1 + prefix.count("\n")
                print(f"        (line numbers include {offset} prepended header/splice lines)")
                for line in (result.stdout + result.stderr).splitlines():
                    if line.strip():
                        print(f"        {line}")

    print(f"lint-glsl: {compiled} shader compilation(s), {failures} failed")
    if compiled < MIN_COMPILATIONS:
        fail(
            f"only {compiled} compilation(s), expected ≥ {MIN_COMPILATIONS} — a shader "
            "or a splice went missing, so this run checked less than it should have"
        )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
