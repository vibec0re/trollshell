// The preem demo's shader widget (#893): an audio-spectrum bar graph with a
// sweeping beam, drawn by the shell on the GPU from this body plus a data
// buffer of `SPECTRUM_BINS` little-endian floats.
//
// THIS IS A FRAGMENT *BODY*, not a complete shader. The shell prepends its own
// header (`#version 320 es` plus the `highp` precision defaults) and the
// interface preamble that declares `v_uv`, `fragColor` and every uniform read
// below — see `hytte_ui::shader_surface::SHADER_PREAMBLE` and the contract on
// `hytte_plugin_proto::wire::Node::Shader`. Re-declaring any of them here is a
// duplicate-declaration compile error.
//
// `nix/lint-glsl.py` compiles this file with exactly that header + preamble in
// front of it, under `nix flake check`'s `glsl` check — so a typo here goes red
// in CI rather than on Annika's glass.
//
// WHAT IT DRAWS, and why each part earns its place on a demo card:
//
//   * bars from `u_data` — the point of the widget: per-frame state arrives as
//     a buffer, not as pixels;
//   * a cap line at each bar's top — so a silent band still reads as a band
//     rather than as an empty column;
//   * a beam driven by `u_time` — the honest witness of the repaint contract.
//     The surface renders when its plugin pushes state and at no other time, so
//     the beam crosses smoothly while audio plays (~20 Hz of spectrum pushes)
//     and steps once a second in silence (the clock heartbeat). That is the
//     design, not a bug: nothing here runs a frame clock for a plugin's shader.

void main() {
    // One row of `u_data_size.x` bands. The texture is NEAREST-filtered and
    // CLAMP_TO_EDGE, so this is a plain nearest read of the band under this
    // column — no interpolation between neighbouring bands.
    float level = clamp(texture(u_data, vec2(v_uv.x, 0.5)).r, 0.0, 1.0);

    // Bars grow from the bottom; `v_uv`'s origin is bottom-left (GL's).
    float lit = step(v_uv.y, level);

    // A one-device-pixel cap line at the band's top, widened by the framebuffer
    // height so it stays one line thick at any allocation.
    float cap = 1.0 - smoothstep(0.0, 2.0 / max(u_resolution.y, 1.0), abs(v_uv.y - level));

    // The sweep: one crossing every four seconds of wall clock.
    float sweep = fract(u_time * 0.25);
    float beam = 1.0 - smoothstep(0.0, 0.06, abs(v_uv.x - sweep));

    // Scanlines at the node's own upscale factor, so a chip drawn at 2x keeps
    // one dark line per logical pixel row instead of per device row.
    float rows = max(u_resolution.y / max(u_scale, 1.0), 1.0);
    float scan = 0.82 + 0.18 * step(0.5, fract(v_uv.y * rows * 0.5));

    // The ink breathes between the skin's own lit ink and the desktop accent,
    // which is a live demonstration that both reach the shader: change the
    // desktop accent with this card open and the warm half of the cycle moves.
    //
    // 2π/9 rad/s — a **9-second** period, and 9 divides 3600, so the cycle is
    // continuous across `u_time`'s hourly wrap. The contract states that rule
    // and this is the reference shader a plugin author copies, so it had better
    // obey it: at the previous `0.6` (period 10.47 s, not a divisor) the ink
    // stepped from "almost all u_fg" to "half accent" in one frame, once an
    // hour. Nothing can lint this — pick a rate `2π·k/3600`, or drive motion
    // from `fract(u_time / P)` with `P` dividing 3600, as the beam above does.
    vec3 ink = mix(u_fg.rgb, u_accent.rgb, 0.5 + 0.5 * sin(u_time * 0.6981317));

    vec3 color = mix(u_bg.rgb, ink, lit * 0.85);
    color = mix(color, u_accent.rgb, cap * 0.9);
    color += u_warning.rgb * beam * 0.22;

    // Premultiplied, as the contract asks — `u_bg.a` is 1.0 on every skin (a
    // preem panel is a screen), so `rgb * a == rgb` and this is opaque output
    // that needs no scaling. A shader that genuinely wanted half-transparency
    // would write `vec4(rgb * a, a)`.
    fragColor = vec4(color * scan, u_bg.a);
}
