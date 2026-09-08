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
  * a body that is spliced by `program.rs`'s `concat!` (the blur's
    `BLUR_DIR`) is compiled once per splice, with the splice text, because
    that body does not compile on its own — and compiling it "as written"
    would be checking a source that never ships;
  * every other body is compiled as-is, at the stage its extension names;
  * **#893's shader widget** the same way: its vertex stage
    (`crates/hytte-ui/src/shader_*.vert`) compiles as written, and every
    plugin-supplied fragment *body* shipped in this tree (today
    `crates/hytte-plugin-preem-demo/shaders/*.frag`) compiles with the
    interface `SHADER_PREAMBLE` spliced in front of it — read out of
    `shader_surface.rs`, so adding a uniform to the published contract changes
    what this validates in the same commit.

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

# Directories holding plugin-supplied fragment **bodies** shipped in this tree.
# Each `.frag` under one of these is compiled as `header + preamble + body`,
# which is exactly what `ShaderSurface::draw` hands the driver.
WIDGET_BODY_DIRS = [Path("crates/hytte-plugin-preem-demo/shaders")]

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
MIN_SHADERS = 6
# Compilations, not files: `scope_blur.frag` is one body compiled twice. A
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
MIN_COMPILATIONS = 9
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
    widget_stages = 0
    widget_bodies = 0
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
        for path in sorted(WIDGET_SHADER_DIR.glob("shader_*.vert")):
            widget_stages += 1
            compile_assembled(f"{path.name} (widget vertex stage)", "", path)
        for directory in WIDGET_BODY_DIRS:
            if not directory.is_dir():
                fail(f"{directory} is missing — wrong root, or a demo's shaders moved")
            unknown = [p for p in sorted(directory.rglob("*")) if p.is_file() and p.suffix != ".frag"]
            if unknown:
                fail(
                    "a shader-widget body directory may hold only `.frag` bodies (they are "
                    "compiled with the fragment preamble in front); found: "
                    f"{', '.join(str(p) for p in unknown)}"
                )
            for path in sorted(directory.rglob("*.frag")):
                widget_bodies += 1
                compile_assembled(f"{path.name} (widget body)", preamble, path)

    print(
        f"lint-glsl: {compiled} shader compilation(s) "
        f"({widget_stages} widget stage(s), {widget_bodies} widget body(ies)), {failures} failed"
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
    if widget_bodies < MIN_WIDGET_BODIES:
        fail(
            f"only {widget_bodies} shader-widget body(ies), expected ≥ {MIN_WIDGET_BODIES} — "
            "a bundled plugin's fragment body went missing, so the one artifact proving "
            "the #893 contract compiles was never compiled"
        )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
