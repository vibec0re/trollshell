// `Gauge::render` — the dial face, the lit layer and the composite, as **one
// body compiled twice**.
//
// **The layer is a `const int LAYER` prepended by the Rust side** (`gauge.rs`
// `concat!`s it ahead of this body), exactly the way `blur.frag`'s `BLUR_DIR`
// is: `GlUniforms` is one bag applied to every pass, so there is nowhere to say
// "this pass is the lit one". Two programs, one body — and that is not tidiness
// here, it is the only way the two passes can share `segment_shade` and
// `arc_shade`. A hand-copied second distance-to-a-tapered-segment is precisely
// the duplicate that drifts, and a drift between the two would show up as ticks
// that do not line up with the needle they sit under.
//
//   * `LAYER == LAYER_LIT` writes the **lit layer** into an R8 aux texture: the
//     value arc, the motion-blur fan, the needle blade, its counterweight and
//     the hub, max-combined. That texture is what the separable blur (the
//     shared `blur.frag`) turns into the skin's halo.
//   * `LAYER == LAYER_BLIT` writes the **screen**: the flat face (scale arc and
//     tick marks, painted like the `Scope`'s graticule so it never blooms),
//     then the lit layer with its halo max-combined under it, the CRT pass and
//     the composite toward the ink.
//
// # Why this is not the kit's picture scaled up
//
// The CPU kit rasterises a gauge into a **logical** `cols × rows` buffer and
// then replicates it `scale` times (`hytte-preem/src/gauge.rs`'s
// `Frame::upscale`). Every soft edge on the face — and the whole dial is soft
// edges, since a needle sits at an arbitrary angle — is therefore one *logical*
// pixel of anti-aliasing blown up into `scale` screen pixels, which is #1090's
// smear.
//
// Here the grid **is** the native buffer (`cols * scale` by `rows * scale`, see
// `gauge.rs`'s `gauge_surface`), so every length arrives pre-multiplied by
// `u_upscale` and the arc, the ticks and the needle are rasterised at the size
// they are actually shown at. The one constant that is deliberately **not**
// scaled is `FEATHER`, the anti-aliasing ramp: it stays one native pixel wide,
// which is the entire fix. At `scale == 1` the two arms draw the same picture
// at the same resolution, which is where the parity harness measures them.
//
// Every composite step is **integer**, for the reason `scope_blit.frag` gives
// at length: the kit's `mix` is `(a * (255 - t) + b * t + 127) / 255`, so a
// float composite would differ in the last bit almost everywhere.

const int LAYER_LIT = 0;
const int LAYER_BLIT = 1;

// ── hytte-preem/src/gauge.rs — lengths, in *logical* px (scaled by u_upscale
// below), and ratios, which are not scaled.
const float ARC_HW = 0.8;            // scale-arc band half-width
const float VALUE_HW_BONUS = 0.35;   // the lit value arc is this much fatter
const float MAJOR_HW = 0.85;         // major/mid tick half-width
const float MINOR_HW = 0.55;         // minor tick half-width
const float BLADE_TIP = 0.5;         // needle half-width at the tip
const float TAIL_FLARE = 1.25;       // counterweight flare (a ratio)
const float MID_LEN_BONUS = 1.35;    // mid tick over a major one (a ratio)

// The anti-aliasing ramp — **one native pixel**, never scaled. See the module
// header: this is what makes the GL arm sharp where the kit is smeared.
const float FEATHER = 1.15;

// Intensities, of 255 (`hytte-preem/src/gauge.rs`). The flat furniture first,
// then the lit layer.
const int ARC_T = 30;
const int MINOR_T = 52;
const int MAJOR_T = 86;
const int MID_T = 118;
const int VALUE_T = 130;
const int NEEDLE_T = 255;
const int HUB_T = 235;
// The motion-blur blades, newest first — `TRAIL_T`, whose length also sets how
// `TRAIL_SPAN_SECS` is subdivided (the Rust side does that subdivision).
const int TRAIL_T[4] = int[4](150, 104, 66, 34);

// hytte-preem/src/style.rs — the CRT pass, verbatim from `scope_blit.frag`.
const int MASK_ONE = 256;
const int COORD_ONE = 1024;
const int BAND_DIV = 9;
const int CORNER_DIV = 6;

// `f32::EPSILON`, the kit's own degenerate-segment threshold.
const float F32_EPSILON = 1.1920929e-7;

in vec2 v_uv;

uniform sampler2D u_tex0;   // blit: the lit layer, 0..255
uniform sampler2D u_tex1;   // blit: the separably-blurred lit layer
uniform ivec2 u_grid;       // the **native** grid, (cols * scale, rows * scale)
uniform float u_upscale;    // `scale`: logical lengths are already multiplied
                            // by it, the constants above are multiplied here
uniform float u_pivot_x;    // the dial's pivot, in native grid coordinates …
uniform float u_pivot_y;
uniform float u_radius;     // … and every length the face resolved, native px
uniform float u_half;       // half the sweep, in radians (an angle: not scaled)
uniform float u_tip;
uniform float u_tail;       // 0.0 when the face is too small for a counterweight
uniform float u_hub;
uniform float u_blade;
uniform float u_major_len;
uniform float u_minor_len;
uniform float u_theta_needle;   // the live needle's dial angle
uniform vec4 u_theta_trail;     // the four motion-blur blades, newest first
uniform float u_value_end;      // dial angle the lit value arc fills to
uniform int u_value_on;         // 0 when the reading is at or below zero
uniform int u_divisions;
uniform int u_subdivisions;     // as the *face* resolved them (MIN_TICK_SPACING)
uniform int u_tick_span;        // how many ticks either side of the nearest one
                                // can reach a fragment — see `gauge.rs`
uniform int u_bloom_strength;   // halo strength in 256ths; 0 = no bloom
uniform vec4 u_bg;              // the skin's field, channels as 0..255
uniform vec4 u_ink;             // the skin's lit ink, channels as 0..255
uniform int u_mask_on;
uniform int u_mask_pitch;
uniform int u_mask_phase;
uniform int u_scanline_keep;
uniform int u_corner_keep;

out vec4 o_colour;

// ── the kit's shape primitives (`hytte-preem/src/gauge.rs`) ─────────────────

// `polar`: the point `radius` from `origin` at dial angle `theta` — measured
// from 12 o'clock and growing clockwise. A negative radius points the other
// way, which is how the counterweight is placed behind the pivot.
vec2 polar(vec2 origin, float radius, float theta) {
    return vec2(origin.x + radius * sin(theta), origin.y - radius * cos(theta));
}

// `coverage`: full inside `half_width`, ramping linearly to nothing over
// FEATHER beyond it.
float coverage(float distance, float half_width) {
    if (distance <= half_width) {
        return 1.0;
    }
    return max(1.0 - (distance - half_width) / FEATHER, 0.0);
}

// `shade`: a 0..1 coverage scaled into an intensity. `floor(x + 0.5)` rather
// than `round()` because GLSL ES leaves `round()`'s halfway direction to the
// implementation, while Rust's `f32::round` is half-away-from-zero — and every
// value here is non-negative, where the two agree.
int shade(float cover, int peak) {
    return int(floor(clamp(cover, 0.0, 1.0) * float(peak) + 0.5));
}

// `Grid::segment`: a soft-edged line whose solid half-width tapers from `wide`
// at `head` to `narrow` at `tail`. A zero-length segment is a disc of radius
// `wide` — which is how the hub is drawn.
int segment_shade(vec2 p, vec2 head, vec2 tail, float wide, float narrow, int peak) {
    vec2 d = tail - head;
    float length2 = dot(d, d);
    float along = length2 > F32_EPSILON ? clamp(dot(p - head, d) / length2, 0.0, 1.0) : 0.0;
    vec2 near = head + d * along;
    return shade(coverage(length(p - near), wide + (narrow - wide) * along), peak);
}

// `Grid::arc`: a soft-edged band of `radius` (± `half_width`) between two dial
// angles, measured to the nearest point on the arc *segment* — the pixel's own
// angle clamped into the sweep, which gives round end caps rather than an
// aliased radial cut. The radial reject is the kit's own cheap pre-test, and
// it also stands in for the kit's bounding-box `span`: a pixel outside that box
// is further than `pad` from the full circle, so it would shade to nothing
// anyway.
int arc_shade(vec2 p, vec2 centre, float radius, float half_width, float start, float end, int peak) {
    if (end < start) {
        return 0;
    }
    vec2 d = p - centre;
    float pad = half_width + FEATHER;
    if (abs(length(d) - radius) > pad) {
        return 0;
    }
    float theta = clamp(atan(d.x, -d.y), start, end);
    return shade(coverage(length(p - polar(centre, radius, theta)), half_width), peak);
}

// ── the face: the scale arc and the tick marks, flat ────────────────────────

// `Gauge::render`'s first half plus `tick_marks`.
//
// The tick loop is **windowed**, not a walk of all `divisions * subdivisions`
// marks: ticks sit at a constant angular pitch, so the only ones that can reach
// a fragment are those nearest its own angle. `u_tick_span` is how many either
// side that is — an upper bound computed on the Rust side from the tick pitch
// at the innermost radius a tick reaches. A full walk would be `2049` iterations
// per fragment at the wire's cap, which is a denial of service on a software
// rasteriser; this is O(1).
int face_intensity(vec2 p, vec2 pivot) {
    float s = u_upscale;
    int v = arc_shade(p, pivot, u_radius, ARC_HW * s, -u_half, u_half, ARC_T);

    int steps = max(u_divisions * u_subdivisions, 1);
    vec2 d = p - pivot;
    // The fragment's own dial angle, as a fractional tick index.
    float at = (atan(d.x, -d.y) / (2.0 * u_half) + 0.5) * float(steps);
    int nearest = int(floor(at + 0.5));
    for (int k = nearest - u_tick_span; k <= nearest + u_tick_span; ++k) {
        if (k < 0 || k > steps) {
            continue;
        }
        float length_in;
        float half_width;
        int peak;
        if (k * 2 == steps) {
            // Capped at the radius for the same reason the floors are: a
            // degenerate face must not draw a tick out through its pivot.
            length_in = min(u_major_len * MID_LEN_BONUS, u_radius);
            half_width = MAJOR_HW * s;
            peak = MID_T;
        } else if (k % u_subdivisions == 0) {
            length_in = u_major_len;
            half_width = MAJOR_HW * s;
            peak = MAJOR_T;
        } else {
            length_in = u_minor_len;
            half_width = MINOR_HW * s;
            peak = MINOR_T;
        }
        float theta = (float(k) / float(steps) - 0.5) * 2.0 * u_half;
        v = max(v, segment_shade(
            p,
            polar(pivot, u_radius, theta),
            polar(pivot, u_radius - length_in, theta),
            half_width,
            half_width,
            peak
        ));
    }
    return v;
}

// ── the lit layer ───────────────────────────────────────────────────────────

// `Gauge::blade`: the tapered pointer, plus (for the live needle only, and only
// where the face has the room) the counterweight stub behind the pivot.
int blade_shade(vec2 p, vec2 pivot, float theta, int peak, bool weighted) {
    int v = segment_shade(p, pivot, polar(pivot, u_tip, theta), u_blade, BLADE_TIP * u_upscale, peak);
    if (weighted && u_tail > 0.0) {
        v = max(v, segment_shade(
            p,
            pivot,
            polar(pivot, -u_tail, theta),
            u_blade,
            u_blade * TAIL_FLARE,
            peak
        ));
    }
    return v;
}

// The lit layer, **max**-combined exactly as `Grid::raise` does: overlapping
// shapes keep the brighter one's edge, and a settled needle's motion blur — the
// needle's own geometry when the velocity is zero — vanishes completely rather
// than thickening it.
int lit_intensity(vec2 p, vec2 pivot) {
    int v = 0;
    if (u_value_on != 0) {
        v = max(v, arc_shade(
            p,
            pivot,
            u_radius,
            (ARC_HW + VALUE_HW_BONUS) * u_upscale,
            -u_half,
            u_value_end,
            VALUE_T
        ));
    }
    // Motion blur first, so the blade's own full intensity wins where they
    // overlap.
    for (int i = 0; i < 4; ++i) {
        v = max(v, blade_shade(p, pivot, u_theta_trail[i], TRAIL_T[i], false));
    }
    v = max(v, blade_shade(p, pivot, u_theta_needle, NEEDLE_T, true));
    v = max(v, segment_shade(p, pivot, pivot, u_hub, u_hub, HUB_T));
    return min(v, 255);
}

// ── the composite (verbatim from `scope_blit.frag`) ─────────────────────────

int texel(sampler2D tex, ivec2 p) {
    return int(texelFetch(tex, p, 0).r * 255.0 + 0.5);
}

// `hytte-preem/src/style.rs`'s `mix`: `a` toward `b` by `t`/255, channel-wise,
// with the same `+ 127` rounding.
ivec4 mix_kit(ivec4 a, ivec4 b, int t) {
    int k = clamp(t, 0, 255);
    return (a * (255 - k) + b * k + 127) / 255;
}

// `centered`: see `scope_blit.frag` for the note on the one signed integer
// division in the pipeline.
int centred(int i, int n) {
    if (n == 0) {
        return 0;
    }
    return ((2 * i + 1 - n) * COORD_ONE) / n;
}

// floor(sqrt(n)) for a non-negative `n`, matching `i64::isqrt`.
int isqrt(int n) {
    if (n <= 0) {
        return 0;
    }
    int s = int(sqrt(float(n)));
    if ((s + 1) * (s + 1) <= n) {
        s += 1;
    }
    if (s * s > n) {
        s -= 1;
    }
    return s;
}

// `MaskRow::keep`: radial vignette × rounded-glass edge ramp × scanline comb.
int mask_keep(int x, int y, int w, int h) {
    int shortSide = min(w, h);
    int band = shortSide / BAND_DIV;
    int radius = shortSide / CORNER_DIV;

    int u = centred(x, w);
    int v = centred(y, h);
    int r2 = (u * u + v * v) / 2;
    int depth = MASK_ONE - u_corner_keep;
    int radial = clamp(MASK_ONE - (depth * r2) / (COORD_ONE * COORD_ONE), 0, MASK_ONE);

    int edge = MASK_ONE;
    if (band > 0) {
        int ex = min(x, w - 1 - x);
        int ey = min(y, h - 1 - y);
        int d;
        if (ex < radius && ey < radius) {
            int dx = radius - ex;
            int dy = radius - ey;
            d = max(radius - isqrt(dx * dx + dy * dy), 0);
        } else {
            d = min(ex, ey);
        }
        d = min(d, band);
        edge = MASK_ONE * d / band;
    }

    int comb = MASK_ONE;
    if (u_mask_pitch != 0 && (y % u_mask_pitch) == u_mask_phase) {
        comb = u_scanline_keep;
    }
    return radial * edge / MASK_ONE * comb / MASK_ONE;
}

void main() {
    vec2 pivot = vec2(u_pivot_x, u_pivot_y);

    if (LAYER == LAYER_LIT) {
        // An offscreen pass: the viewport **is** the grid, and the accumulator
        // and every aux texture are indexed in kit rows throughout (row 0 is
        // the top of the image), so `gl_FragCoord` is already the kit's own
        // `(fx(x), fx(y))` pixel centre — the blit below flips once, and
        // nowhere else.
        o_colour = vec4(float(lit_intensity(floor(gl_FragCoord.xy), pivot)) / 255.0, 0.0, 0.0, 1.0);
        return;
    }

    int cols = u_grid.x;
    int rows = u_grid.y;
    // Point sampling into the letterboxed fit rect, exactly as
    // `scope_blit.frag` does it, and with the same honest caveat when the
    // allocation is not an integer multiple of the grid. At the natural size —
    // what the reconciler requests and what the parity harness measures — the
    // fit rect *is* the grid and this is the identity.
    int col = clamp(int(v_uv.x * float(cols)), 0, cols - 1);
    int row = clamp(int((1.0 - v_uv.y) * float(rows)), 0, rows - 1);
    vec2 p = vec2(float(col), float(row));

    ivec4 bg = ivec4(u_bg + 0.5);
    ivec4 ink = ivec4(u_ink + 0.5);

    // The face, redrawn flat every frame so it never picks up the lit layer's
    // bloom — the kit paints it under the emission for exactly that reason.
    ivec4 under = bg;
    int face = face_intensity(p, pivot);
    if (face > 0) {
        under = mix_kit(bg, ink, face);
    }

    // `Emission::bloom`: the blurred grid scaled by strength/256 and
    // max-combined under the original.
    ivec2 q = ivec2(col, row);
    int lit = texel(u_tex0, q);
    int halo = min(texel(u_tex1, q) * u_bloom_strength / 256, 255);
    lit = min(max(lit, halo), 255);

    // `Emission::composite`: unlit pixels are skipped *before* the mask is
    // consulted, which is why an unlit CRT shows no scanlines.
    if (lit > 0) {
        if (u_mask_on != 0) {
            // **The mask keeps its logical pitch.** The skin's comb and
            // vignette are screen-space furniture the kit resolves against the
            // *logical* buffer, so they are evaluated at the logical
            // coordinate here rather than at the native one — otherwise a 4-row
            // comb on a `scale = 2` dial would come out twice as dense as the
            // same skin's scope beside it. Only the *geometry* gains
            // resolution; the skin does not change.
            int s = max(int(u_upscale + 0.5), 1);
            lit = lit * mask_keep(col / s, row / s, max(cols / s, 1), max(rows / s, 1)) / MASK_ONE;
        }
        if (lit > 0) {
            under = mix_kit(under, ink, lit);
        }
    }

    // Opaque: a kit frame is a screen, never a sprite. The letterbox padding
    // around the fit rect is the transparent part, and the host clears it.
    o_colour = vec4(vec3(under.rgb) / 255.0, 1.0);
}
