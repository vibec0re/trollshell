# Preem on the GPU: a `GtkGLArea` renderer for `Scope` (stage B)

**Date:** 2026-09-06
**Status:** Proposed — this is the veto window. No GL code is written before Annika has read it.
**Issues:** #893 (shader widget + trust boundary), #886 (stage A probe — the numbers), #881 (epic), #865 (the architecture call), #863 (the perf driver)

## Summary

Stage A measured `GtkGLArea` on glass and came back GO. Stage B swaps the
renderer for **one** preem kind — `Scope` — behind the existing `Renderer` seam
in `trollshell/src/plugins/preem_render.rs`, under the **unchanged** `Node::Preem`
wire vocabulary. Plugins send state; the shell reconciles it; for a GL kind the
reconciled state becomes uniforms. That is Annika's own #863 sentence, executed.

`Scope` goes first because it is the one kind with an inter-frame accumulation
buffer — a `Vec<u16>` phosphor decayed with `(v * retained) >> 8` every step
(`crates/hytte-preem/src/scope.rs:344-351`), plus a `box_blur` bloom
(`style.rs:1112-1124`). A ping-pong FBO gets both for free. Gauge is mid-retune
(#930/#931) and stays CPU.

This spec settles three things nothing upstream did: how a `forbid(unsafe_code)`
workspace issues GL draw calls, how a GPU frame reaches the reconciler without a
readback, and what #893's trust boundary is now that robustness turned out to be
unreachable.

## What stage A settled (quoting only the numbers the #893 verdict quotes)

| phase                          | avg   | p50   | p95       | max     | jank | frames |
| ------------------------------ | ----- | ----- | --------- | ------- | ---- | ------ |
| **layer-shell**, areas 140×177 |       |       |           |         |      |        |
| idle                           | 16.67 | 16.67 | 16.67     | 16.67   | 0    | 300    |
| cairo-x3                       | 16.62 | 16.68 | 16.68     | 16.87   | 0    | 301    |
| gl-x1                          | 16.51 | 16.68 | 16.70     | 16.92   | 0    | 303    |
| gl-x3                          | 16.63 | 16.69 | **16.77** | 33.01\* | 0    | 301    |
| **window**, areas 140×1788     |       |       |           |         |      |        |
| cairo-x3                       | 17.37 | 16.66 | 20.82     | 33.35   | 3    | 288    |
| gl-x1                          | 16.57 | 16.68 | 16.68     | 17.81   | 0    | 302    |
| gl-x3                          | 17.20 | 16.68 | 19.00     | 35.75   | 2    | 290    |

\* one doubled frame in 301 that the jank counter (`> 2 × p50` = 33.38) missed by
0.37 ms. `areas=3` in both runs, so a full bar is extrapolation from N = 1→3 —
which is why `--areas 8` is the acceptance gate below and not a claim here.

Also settled: contexts negotiate `GLAPI(GLES) version=3.2 legacy=false`;
`glarea[0<->1].shared` and `[0<->2].shared` both PASS (one share group per
display); `gsk.glshader_compile` FAILs under `GskVulkanRenderer`; all three
outputs are `scale=1.0000 scale_factor=1`, so `RESAMPLED` never fired;
`gdk.robust_context_api: absent` even though the NVIDIA stack (`10de:2203`)
advertises `EGL_EXT_create_context_robustness` and `GL_KHR/ARB/EXT_robustness`.

## The hard problem: GL draw calls under `unsafe_code = "forbid"`

`gl_probe` issues **no** GL calls at all, by design (`crates/hytte-ui/examples/gl_probe.rs:25-33`).
Stage B has to. Three options, weighed honestly:

| option                                                                                                                                                                    | verdict                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **(a) `hytte-gl`** — a tiny crate that is the second `unsafe_code = "allow"` island, modelled exactly on `hytte-ecal`, wrapping the `gl` crate behind a safe, minimal API | **Recommended.** `gl 0.14.0` is **already in `Cargo.lock`** — `gdk4 0.11.2` lists it as a dependency — as are `gl_generator`, `khronos_api`, `xml-rs` and `libloading 0.8.9` (via `clang-sys`). **Zero new lock entries.** The precedent and the mechanics are written down: `crates/hytte-ecal/Cargo.toml` hand-mirrors the root lints table with `unsafe_code = "allow"` because workspace-lints inheritance is all-or-nothing (root `Cargo.toml:53-56`). |
| **(b) wgpu → `GdkGLTexture`/`GdkDmabufTexture`**                                                                                                                          | Rejected. wgpu is safe at its own API, but adopting GDK's existing EGL context is `wgpu_hal::gles::Adapter::new_external` (unsafe), and exporting a dmabuf needs `wgpu-hal` + external-memory (more unsafe). So it buys no safety and costs ~100 lock entries.                                                                                                                                                                                              |
| **(c) `glow` / `epoxy`**                                                                                                                                                  | Rejected. Both mark every entry point `unsafe fn` exactly like `gl` does, so they need the same island — and each adds a lock entry `gl` does not.                                                                                                                                                                                                                                                                                                          |

`hytte-gl` stays GTK-free and deliberately small: compile a program, upload a
uniform or a 1-D data texture, bind an FBO, draw a quad or a point array, swap a
ping-pong pair, read back for the parity harness. Handles are RAII and `!Send`.
Function pointers come from `libloading::Library::new("libepoxy.so.0")` — GTK has
libepoxy loaded already, so `dlopen` by soname returns the loaded handle rather
than searching. **Verify that on glass**; the fallback is a build-time path from
`pkg-config`.

## PR 1: the seam

**Keep `enum Renderer`, add a `ScopeGl` variant.** Not a `Backend` trait: the
enum is private, derives only `Debug`, and its eight hand-written matches include
`advance`/`animates`/`render`, which have no catch-all — so a new variant is a
compile error exactly where it should be. Two arms _do_ have catch-alls and will
accept a missing arm silently; they are the PR's review checklist:
`Renderer::update`'s `_ => {}` (`preem_render.rs:2023`) and `config_eq`'s
`_ => false` (`:1507`).

`ScopeGl` keeps `pending` / `idle` / `fades` / `settle_steps` / `steps`
**verbatim** from `Renderer::Scope` (`:541-560`) and drops the `kit::Scope`.
`advance()` becomes bookkeeping only — no `Vec<u16>` decay pass, no polyline
stamping. That is #863's retirement, not an optimisation of it. `animates()` is
the _same expression_ — `pending.is_some() || (fades && idle < settle_steps)`
(`:2192-2198`) — so #926's frame clock parks and unparks identically and
`pump.rs` needs no change at all. `scope_settle_steps` (`:337-351`) stays the CPU
integer replay of the kit's recurrence: nothing ever reads back from the GPU to
ask "is it still fading".

**The frame reaches the reconciler as a new node, not as a readback.** Rendering
to an FBO and `glReadPixels`-ing into `Arc<[u8]>` for `UiNode::Pixels` would keep
the reconciler untouched and defeat the entire point (a per-chip, per-frame
pipeline stall). So `map_widget` (`:900-1008`) returns a new variant instead:

```rust
Node::GlSurface {
    id: Option<NodeId>,
    width: u32, height: u32,   // logical px: cols*scale × rows*scale
    program: GlProgram,        // Copy enum naming a shell-registered program
    state: Arc<GlUniforms>,    // plain data; dedup by Arc::ptr_eq then by value
    classes: Vec<String>,
}
```

`Node` derives `Clone, Debug, PartialEq` (`crates/hytte-ui/src/widget_tree.rs:117`),
so the payload is plain data — no closures. `GlUniforms` is preem-agnostic (a
uniform bag plus an optional `Arc<[f32]>` data texture) and programs are
registered at shell startup, so `hytte-ui` learns nothing about `Scope`. The GLSL
lives in `trollshell/src/plugins/preem_gl/*.glsl` via `include_str!` — under
`trollshell/src/`, deliberately _not_ under the top-level `assets/` that crane's
source filter strips (#480/#446).

**Reconciler consequences**, all mirroring the `Pixels` arm one-for-one:
`NodeKind::GlSurface` (`widget_tree.rs:505`), `build_node` (`:805`),
`update_in_place` (`:1121`), `node_id` (`:1651`), `classes` (`:1673`). `hytte-ui`
gains a `GlSurface` widget (a `gtk::GLArea` subclass) owning the phosphor
ping-pong — sized on `resize`, freed on `unrealize`, `set_auto_render(false)`
plus `queue_render()` on state change, as the probe does.

**The draw must be idempotent.** GTK calls `render` whenever it needs the texture
— a resize, a re-composite — not only when we ask. So `GlUniforms` carries a
monotonic `step_seq` (total animation steps since build) and the widget stores
the last one it drew, running `step_seq - last` decay/plot passes: zero on a
repeat, so it just re-blits. Catch-up is already bounded upstream
(`MAX_CATCHUP_STEPS = 8`, `MAX_TICK_DT_US`, `preem_render.rs:239`).

**Multi-monitor.** The CPU arm shares one `Arc<[u8]>` across monitors (#911/#927):
one rasterisation, N surfaces. The GL arm has one GLArea and one phosphor pair
per monitor, so a scope on two monitors accumulates twice. Fed the same batches
at the same `step_seq` they stay visually equivalent, but a monitor that was
unmapped and remaps resumes from its own trail. Recommendation: accept and
document (it converges within `settle_steps` — 17 at the default persistence);
`clear-on-map` is the fix if it ever reads wrong.

**When the CPU arm is used.** "No CPU fallback" was Annika's call about the
_shader widget_, which has no CPU implementation. Kit widgets do, and it is the
reference, so falling back is free and strictly better than a blank chip. CPU is
used when (1) `TROLLSHELL_PREEM_RENDERER=cpu` — the PR 1 default, so main cannot
regress the bar before the acceptance run, with the flip to `gl` its own one-line
PR; (2) the kind has no GL arm (everything but `Scope` in PR 1); or (3)
`GLArea::error()` is set after realize or `context()` is `None` — per instance,
warned once, no process-wide probe.

## State → uniforms

| CPU (`scope.rs`)                                     | GPU                                                                                                                                                                                                                                                        |
| ---------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `persistence` (256ths)                               | `uniform int u_retained`; decay pass on an **`R8UI`** texture computing `(v * retained) >> 8` — integer, so the recurrence is bit-identical                                                                                                                |
| `ScopeState::samples` (≤ `MAX_SCOPE_SAMPLES` = 4096) | 1-D `R32F` texture + `u_sample_count`; `sample_at`'s lerp is `texelFetch` + the same index math                                                                                                                                                            |
| `sanitize` (±inf → 0.0, the axis)                    | already applied on the wire by `clamp_in_place`; the shader must **not** re-clamp differently                                                                                                                                                              |
| polyline span + 5-tap `GLOW`                         | one point-array vertex per column covering `[min(prev,row), max(prev,row)]` from the same `row_for` formula, expanded ±2 rows, blended with `GL_MAX` (the kit stamps with `max`, not `+=`) — **not** `GL_LINES`, whose rasterisation rules would not match |
| `draw_graticule`                                     | pure function of `cols/rows/GRID_DIV/GRID_T/AXIS_T`, recomputed in the blit shader; no buffer                                                                                                                                                              |
| `Emission::composite`                                | `mix(under, ink, t)` with the kit's `(a*(255-t) + b*t + 127)/255` rounding, in integers                                                                                                                                                                    |
| `frame.upscale(scale)`                               | the blit point-samples into the allocation; no upscale buffer                                                                                                                                                                                              |

## Skins, bloom, accent

`Palette` is `pub(crate)` (`style.rs:766`), so the GL arm needs one small
additive change in `hytte-preem`: a public plain-data `palette_snapshot(style)`
resolved **through the existing `palette()`**, so the `with_pins` + `set_accent`
precedence (`style.rs:477-497`) is reused rather than reimplemented. GTK-free, no
new deps.

Bloom for `Scope` is the skin's **raw** radius. #930's halving is gauge-local by
explicit design (`gauge.rs:556-558`: "the `Scope` standing next to a gauge keeps
the skin's halo exactly as the skin defines it") — do not generalise it into a
shared uniform. Same separable box blur, two passes, constant `2r+1` divisor
including at clipped edges, `max`-combined.

Accent re-tint (#862/#864) needs no program rebuild and no widget rebuild:
`tint_in_process_surfaces` already calls `request_preem_repaint_all`, and the next
mapping pass rebuilds `GlUniforms` with the new ink. `invalidate_cached_frames`
(`:1346-1358`) special-cases `Renderer::TextBox` because it bakes its palette at
construction; `ScopeGl` resolves at render time like the CPU `Scope`, so it needs
no entry there.

## Parity and tests

CPU goldens keep gating the CPU arm **byte-exactly** — unchanged. The GL arm gets
a bounded per-channel delta against the same goldens, per the #893 verdict.

**CI has no GL.** `flake.nix`'s `system-tests` check runs `xvfb-run -a cargo test`
in a nix sandbox with no `/dev/dri` and no mesa in the closure. So CI checks what
it can, all pure-CPU:

| check                                                                                       | where                                        |
| ------------------------------------------------------------------------------------------- | -------------------------------------------- |
| the shipped GLSL parses and validates as `#version 320 es` under **naga**                   | `cargo test` in `trollshell`                 |
| `(ScopeConfig, ScopeState, step_seq)` → `GlUniforms` against a golden uniform table         | `preem_render` unit test                     |
| the state machine: `animates()`, `settle_steps`, `step_seq` monotonicity, catch-up clamping | `preem_render` unit test                     |
| `plan_diff` / `NodeKind` for `GlSurface`                                                    | `widget_tree.rs`, headless, existing pattern |

**Live-verify only:** the actual pixels, the parity delta, `--areas 8`. The
parity harness is a new `hytte-ui` example (`preem_gl_diff`) — examples ship
nowhere, `nix/package.nix`'s `postInstall` copies a hardcoded list — rendering
the same state through both arms, reading back the GL one, and printing
per-channel mean / p99 / max plus a structural check that each column's peak row
matches.

**Proposed ceiling:** mean |Δ| ≤ 2/255, p99 ≤ 8/255, max ≤ 32/255 per channel. If
the span-quad design above holds the observed numbers should be far tighter (only
`round()` at exact .5 boundaries can differ); PR 1 records what it measured and
the ceiling tightens to observed + margin.

## Trust boundary for #893 — route 1, validation only

Chosen, not forced: the NVIDIA stack advertises every robustness extension, but
`gdk.robust_context_api: absent` means GDK requests none of them and
`GtkGLArea` exposes no knob, so routes 2 and 4 are expensive rather than moot.

- **Validator: naga** (`front::glsl`). Safe Rust; `glslang`/`shaderc` are C++ FFI
  that would have to live in the unsafe island and drag a large build closure.
  Lock delta ≈ 15 new entries — paid by #893, **not** by stage B PR 1.
- **Caps:** source ≤ 16 KiB; after parse, reject > 4096 IR expressions, any loop
  whose trip count is not a compile-time constant, and any sampler the shell did
  not bind. These are heuristics, deliberately: they bound the _ordinary_ mistake,
  not an adversary.
- **Compile error → the broken-widget placeholder** plus one warning. Note
  `Warned::slot` (`preem_render.rs:679-690`) is at its 8-diagnostic `u8` ceiling;
  a new diagnostic needs a slot.
- **Blast radius, plainly: the whole shell.** One share group per display, no
  `LOSE_CONTEXT_ON_RESET`, so a GPU reset takes every preem context down with no
  notification and GTK will not recover on its own. Realistic worst case is a
  shell restart; whether the Vulkan compositor survives a GL-channel reset on the
  same device is unknown and untested.
- **Upgrade path: route 3** (out-of-process shader host, own context and share
  group, frames back as dmabuf) — reached if a hang is ever observed on glass,
  not before.
- **Dialect: `#version 320 es`.** GDK negotiated `GLAPI(GLES) version=3.2` even
  with `allowed=GLAPI(GL | GLES)`.

## Fractional scale

Allocate logical × the **integer** `scale_factor`, as `gtk_gl_area_allocate_buffers`
does. All three outputs are integer-scaled, so `RESAMPLED` never fired and could
not have. Latent hazard, documented not fixed: on a 1.5× output the GLArea
renders at 2× and GSK resamples down, while `PixelSurface`
(`ScalingFilter::Nearest`, never reads `scale_factor`) does not — a pixel-exact
kit reintroduces a resample. If it ever bites: snap chip sizes to the integer
grid, or own the `GdkGLTextureBuilder` (stage A §3 — the same pipeline minus
GTK's wrapper).

## Perf budget and acceptance

- `--areas 8`, layer-shell, on glass: **jank 0**, and p95 within **0.5 ms** of
  the idle baseline (16.67 ms). The gl-x3 layer number to beat is 16.77.
- The real bar, preem-demo's scope on two monitors: no new jank versus the CPU
  arm, A/B'd with the same env var.
- GL was _cheaper_ than the Cairo path it replaces in the window run (p95 19.00
  vs 20.82, jank 2 vs 3) in a **debug** build, so that is conservative in GL's
  favour.

## Touched files

- `crates/hytte-gl/` — new. The unsafe island: `Cargo.toml` (hand-mirrored lints,
  `unsafe_code = "allow"`), `src/lib.rs` (safe wrappers), loader.
- `crates/hytte-ui/src/gl_surface.rs` — new. `gtk::GLArea` subclass, program
  registry, ping-pong ownership. `src/lib.rs` — `pub mod gl_surface;`.
- `crates/hytte-ui/src/widget_tree.rs` — `Node::GlSurface`, `NodeKind`,
  `build_node`, `update_in_place`, `node_id`, `classes`.
- `crates/hytte-ui/examples/preem_gl_diff.rs` — new, the parity harness.
- `crates/hytte-preem/src/style.rs` — `palette_snapshot`, additive.
- `trollshell/src/plugins/preem_render.rs` — `Renderer::ScopeGl` + its eight
  match arms, `map_widget`'s GL branch, the env-var selection.
- `trollshell/src/plugins/preem_gl/` — new: uniform building, `*.glsl`.
- `Cargo.toml` (workspace members), `docs/live-verify.md` (a GL section).

No change to `hytte-plugin-proto`, `hytte-plugin`, any plugin, or `pump.rs`.

## Out of scope

- **The other kit widgets on GL** — `Gauge` (mid-retune, #930/#931), `Marquee`,
  `DotMatrix`, `LedStrip`, `SevenSeg`, `FlipBoard`, `TextBox`. One per PR, after
  `Scope` proves the seam.
- **The shader widget itself (#893).** This spec settles its trust boundary and
  dialect; the widget, its wire vocabulary addition and the naga dependency are
  its own PR.
- **Skins (#885/#397)** beyond what `palette_snapshot` carries.
- **Route 3**, the out-of-process shader host. Named, not built.
- **Fractional-scale snapping.** Unprovable on this hardware.

## Open questions for Annika

1. PR 1 opt-in (`TROLLSHELL_PREEM_RENDERER=gl`, default CPU) with a default flip
   after your `--areas 8` run — or GL on by default immediately? _(rec: opt-in)_
2. `hytte-gl` as the workspace's second `unsafe_code = "allow"` crate, modelled
   on `hytte-ecal`? _(rec: yes — zero new lock entries)_
3. Per-monitor phosphor divergence: accept, or clear-on-map? _(rec: accept)_
4. Parity ceiling mean 2 / p99 8 / max 32 per channel — right ballpark?
   _(rec: yes, tighten to observed + margin after PR 1)_
5. #893's caps — 16 KiB source, 4096 IR expressions, no unbounded loops?
6. If naga cannot parse `#version 320 es`, drop the plugin-facing dialect to
   `310 es` (a 3.2 context accepts it) or take a heavier validator?
   _(rec: `310 es`)_

## References

- #886 — stage A: `GskGLShader` dead, one context per area and one share group
  per display, GLArea **is** the render-to-texture path, the four routes, and
  Annika's probe transcript.
- #893 — the verdict this spec implements, and the shader widget.
- #881 / #865 / #863 — the epic, "state over the wire → reconcile → set
  uniforms", and the `box_blur` cost being retired.
- `crates/hytte-ui/examples/gl_probe.rs` — the probe, and why it issues no GL.
- `docs/superpowers/specs/2026-07-11-plugin-widgets-frontend-b.md` — the node
  vocabulary this extends.
- `docs/live-verify.md` — "Preem state over the wire", where the GL checks land.
