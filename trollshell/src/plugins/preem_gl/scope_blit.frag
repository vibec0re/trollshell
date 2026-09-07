// `Scope::render` — the graticule, the bloom's max-combine, the CRT pass and
// the composite, in one fragment.
//
// The CPU kit builds this in four visits to the buffer: `Frame::filled` with
// the field, `draw_graticule` over it, `Emission::bloom` over the lit layer,
// then `Emission::composite` mixing each lit pixel toward the ink. All four are
// pure functions of the pixel, so on the GPU they collapse into one pass with
// no intermediate buffer — including `Frame::upscale`, which becomes the
// point-sampling below rather than a second allocation.
//
// Every arithmetic step here is **integer**, and that is not stylistic: the
// kit's `mix` is `(a * (255 - t) + b * t + 127) / 255` and its mask is a chain
// of 256ths, so a float composite would differ in the last bit almost
// everywhere and turn a parity ceiling of 2/255 into a coin flip.

// hytte-preem/src/scope.rs
const int GRID_DIV = 12;   // grid line every this many columns/rows
const int GRID_T = 26;     // grid intensity toward the ink, of 255
const int AXIS_T = 56;     // centre-cross intensity, brighter than the grid

// hytte-preem/src/style.rs
const int MASK_ONE = 256;    // fixed-point one for a mask factor
const int COORD_ONE = 1024;  // fixed-point one for the vignette's coordinates
const int BAND_DIV = 9;      // edge-ramp width is min(w, h) / this
const int CORNER_DIV = 6;    // rounded-glass corner radius is min(w, h) / this

in vec2 v_uv;

uniform sampler2D u_tex0;      // accumulator: the phosphor, 0..255
uniform sampler2D u_tex1;      // the separably-blurred phosphor
uniform ivec2 u_grid;          // (cols, rows)
uniform int u_bloom_strength;  // halo strength in 256ths; 0 = no bloom
uniform vec4 u_bg;             // the skin's field, channels as 0..255
uniform vec4 u_ink;            // the skin's lit ink, channels as 0..255
uniform int u_mask_on;         // 1 when the skin carries the CRT pass
uniform int u_mask_pitch;      // scanline comb pitch, in rows
uniform int u_mask_phase;      // a row is dimmed when row % pitch == phase
uniform int u_scanline_keep;   // light kept on a comb row, in 256ths
uniform int u_corner_keep;     // light kept at the extreme corner, in 256ths

out vec4 o_colour;

int texel(sampler2D tex, ivec2 p) {
    return int(texelFetch(tex, p, 0).r * 255.0 + 0.5);
}

// `hytte-preem/src/style.rs`'s `mix`: `a` toward `b` by `t`/255, channel-wise,
// with the same `+ 127` rounding.
ivec4 mix_kit(ivec4 a, ivec4 b, int t) {
    int k = clamp(t, 0, 255);
    return (a * (255 - k) + b * k + 127) / 255;
}

// `centered`: the pixel centre's normalized coordinate in COORD_ONE units,
// about -COORD_ONE at the near face and +COORD_ONE at the far one. Doubled
// internally so an odd extent gets a true middle pixel. Integer division
// truncates toward zero in GLSL exactly as it does in Rust.
int centred(int i, int n) {
    if (n == 0) {
        return 0;
    }
    return ((2 * i + 1 - n) * COORD_ONE) / n;
}

// floor(sqrt(n)) for a non-negative `n`, matching `i64::isqrt`. The `f32` sqrt
// is within one of the answer over the range this is used on (n is at most
// twice a corner radius squared, a few hundred thousand), so two corrections
// settle it exactly.
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

// `MaskRow::keep`: the CRT pass's attenuation at one pixel, in 256ths.
// Radial vignette × rounded-glass edge ramp × scanline comb, all multiplicative
// — "light the glass takes at the rim is not handed back on a scanline".
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
    int cols = u_grid.x;
    int rows = u_grid.y;

    // `Frame::upscale`, as point sampling. `v_uv` is viewport-relative and the
    // viewport *is* the letterbox fit rect, so this needs no origin: at a
    // pixel centre `v_uv.x = (X + 0.5) / fit_w`, and the floor below is the
    // nearest-neighbour rule `PixelSurface` asks GSK for.
    //
    // One honest difference from the CPU arm when the allocation is **not** an
    // integer multiple of the grid — `fit_rect` fits the grid's aspect, it does
    // not snap to a multiple. There the CPU arm resamples twice (the kit's
    // `Frame::upscale` by `scale`, then GSK nearest to the fit rect) and this
    // resamples once, cols -> fit_w; two nearest steps and one are not the same
    // map, so a column can land a pixel apart. At the natural size — what the
    // reconciler requests and what the parity harness measures — `fit_w` *is*
    // `cols * scale` and the two agree exactly.
    //
    // Row 0 is the *top* of the image (the kit's convention) while GL's window
    // origin is bottom-left, so the vertical axis is flipped here and nowhere
    // else: the accumulator is indexed in kit rows throughout.
    int col = clamp(int(v_uv.x * float(cols)), 0, cols - 1);
    int row = clamp(int((1.0 - v_uv.y) * float(rows)), 0, rows - 1);

    ivec4 bg = ivec4(u_bg + 0.5);
    ivec4 ink = ivec4(u_ink + 0.5);

    // The graticule, redrawn flat every frame so it never picks up the trace's
    // decay: a faint grid, then the brighter centre cross drawn last so it wins.
    ivec4 under = bg;
    if ((col % GRID_DIV) == 0 || (row % GRID_DIV) == 0) {
        under = mix_kit(bg, ink, GRID_T);
    }
    if (col == (cols / 2) || row == (rows / 2)) {
        under = mix_kit(bg, ink, AXIS_T);
    }

    // `Emission::bloom`: the blurred grid scaled by strength/256 and
    // max-combined under the original, so lit pixels never dim and dark
    // neighbours pick up spill.
    ivec2 p = ivec2(col, row);
    int lit = texel(u_tex0, p);
    int halo = min(texel(u_tex1, p) * u_bloom_strength / 256, 255);
    lit = min(max(lit, halo), 255);

    // `Emission::composite`: unlit pixels are skipped *before* the mask is
    // consulted, which is why an unlit CRT shows no scanlines.
    if (lit > 0) {
        if (u_mask_on != 0) {
            lit = lit * mask_keep(col, row, cols, rows) / MASK_ONE;
        }
        if (lit > 0) {
            under = mix_kit(under, ink, lit);
        }
    }

    // Opaque: a kit frame is a *screen*, never a sprite, and every colour it
    // composites carries alpha `0xff`. The letterbox padding around the fit
    // rect is the transparent part, and the host clears that separately.
    o_colour = vec4(vec3(under.rgb) / 255.0, 1.0);
}
