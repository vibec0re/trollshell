// The vertex stage of #893's shader widget: one oversized triangle covering the
// viewport, generated entirely from `gl_VertexID`. There is no vertex buffer,
// and a plugin never writes or replaces this — the wire contract says a plugin
// supplies the *fragment* body only, so the geometry is the shell's.
//
// A single triangle rather than two: a quad's shared diagonal rasterises some
// fragments twice on some drivers, which for a shader that accumulates or
// blends would be a silent inconsistency rather than harmless overdraw.
//
//   id 0 -> (-1, -1)   id 1 -> ( 3, -1)   id 2 -> (-1,  3)
//
// `v_uv` is 0..1 across the *viewport*, and the fragment stage narrows the
// viewport to the letterboxed fit rect before drawing — so `v_uv` spans the
// widget's own rect and needs no origin uniform. Origin is bottom-left, which is
// GL's, and the contract states it so a plugin author is not left guessing which
// way `v_uv.y` runs.
//
// This is deliberately a near-copy of `trollshell/src/plugins/preem_gl/fullscreen.vert`.
// Sharing one file across the two crates would mean either an `include_str!`
// reaching out of `hytte-ui` into the binary (impossible) or a third crate for
// twelve lines of GLSL; both are worse than two files that `nix/lint-glsl.py`
// compiles independently.

out vec2 v_uv;

void main() {
    vec2 p = vec2(
        float((gl_VertexID & 1) << 2) - 1.0,
        float((gl_VertexID & 2) << 1) - 1.0
    );
    v_uv = (p + 1.0) * 0.5;
    gl_Position = vec4(p, 0.0, 1.0);
}
