// Phosphor decay — the first half of one `Scope::advance` step.
//
// The kit's recurrence, verbatim (`hytte-preem/src/scope.rs`, `decayed`):
//
//     v = (v * retained) >> 8
//
// Integer, truncating, on values in `0..=255`. Reproducing it *exactly* rather
// than approximately is the whole reason the accumulator is a normalized `R8`
// and not a float texture: `GL_R8` stores those 255 values and nothing else,
// the read is `v / 255.0` and the write is `round(f * 255)`, and both are exact
// in `f32` over that range — so `int(round(texture(...).r * 255.0))` recovers
// `v` bit for bit and `float(out) / 255.0` puts it back.
//
// Reads the accumulator's *front* buffer and writes the back one; the host
// swaps the pair after the step's last pass, so the beam stamp that follows
// this can `GL_MAX`-blend into the same target without seeing its own writes
// through the read slot.

in vec2 v_uv;

uniform sampler2D u_tex0;   // accumulator, front
uniform int u_retained;     // ScopeConfig::persistence, 256ths retained per step

out vec4 o_intensity;

void main() {
    ivec2 p = ivec2(gl_FragCoord.xy);
    int v = int(texelFetch(u_tex0, p, 0).r * 255.0 + 0.5);
    int decayed = (v * u_retained) >> 8;
    o_intensity = vec4(float(decayed) / 255.0, 0.0, 0.0, 1.0);
}
