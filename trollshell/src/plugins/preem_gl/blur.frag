// One half of the bloom's separable box blur.
//
// **The direction is a `const ivec2 BLUR_DIR` prepended by the Rust side**
// (`program.rs` `concat!`s it ahead of this body), because `GlUniforms` is one
// bag applied to every pass — there is nowhere to say "this pass is the
// horizontal one". Two shaders, one body.
//
// The kit's `box_blur` (`hytte-preem/src/style.rs`), exactly:
//
//     tmp[x] = sum(src[max(0, x-r) ..= min(w-1, x+r)]) / (2r + 1)
//
// Two things a reimplementation gets wrong and this does not:
//
//   * the window **clips** at the edges but the divisor stays the full
//     `2r + 1`, so edge pixels dim — deliberate in the kit ("invisible under
//     the padded frames the widgets render, and it keeps the math
//     branch-free"), and a renormalised divisor would brighten every border;
//   * the division is **integer and truncating**, per pass. Folding the two
//     passes into one 2-D kernel, or dividing in floats, changes bytes.
//
// A radius of `0` makes this the identity (`sum / 1 == v`), which is how a
// glow-free skin (the reflective LCD) passes through: the host runs the blur
// passes unconditionally and the blit's `max(v, blurred * strength / 256)`
// with `strength == 0` then reduces to `v`. Same bytes as the CPU kit's
// `Emission::bloom` early return, without a second pipeline.

in vec2 v_uv;

uniform sampler2D u_tex0;
uniform ivec2 u_grid;
uniform int u_bloom_radius;

out vec4 o_intensity;

void main() {
    ivec2 p = ivec2(gl_FragCoord.xy);
    int radius = max(u_bloom_radius, 0);
    int window = 2 * radius + 1;
    int sum = 0;
    for (int d = -radius; d <= radius; ++d) {
        ivec2 q = p + BLUR_DIR * d;
        if (q.x < 0 || q.y < 0 || q.x >= u_grid.x || q.y >= u_grid.y) {
            continue;
        }
        sum += int(texelFetch(u_tex0, q, 0).r * 255.0 + 0.5);
    }
    o_intensity = vec4(float(sum / window) / 255.0, 0.0, 0.0, 1.0);
}
