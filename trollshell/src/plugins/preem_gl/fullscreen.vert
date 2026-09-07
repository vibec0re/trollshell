// One oversized triangle covering the viewport, generated entirely from
// `gl_VertexID` — there is no vertex buffer anywhere in this pipeline.
//
// A single triangle rather than two: a quad's shared diagonal rasterises some
// fragments twice on some drivers, and every pass that uses this shader is an
// exact integer computation, so a doubly-shaded fragment would be a silent
// inconsistency rather than a harmless overdraw.
//
//   id 0 -> (-1, -1)   id 1 -> ( 3, -1)   id 2 -> (-1,  3)
//
// `v_uv` is 0..1 across the *viewport*, which is what the blit needs: the
// screen pass narrows the viewport to the letterbox fit rect, so a
// viewport-relative coordinate needs no origin uniform.

out vec2 v_uv;

void main() {
    vec2 p = vec2(
        float((gl_VertexID & 1) << 2) - 1.0,
        float((gl_VertexID & 2) << 1) - 1.0
    );
    v_uv = (p + 1.0) * 0.5;
    gl_Position = vec4(p, 0.0, 1.0);
}
