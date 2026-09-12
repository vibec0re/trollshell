// `DotMatrix::render` — the unlit matrix, the lit glyph dots and the
// composite, as **one body compiled twice**.
//
// **The layer is a `const int LAYER` prepended by the Rust side**
// (`dot_matrix.rs` `concat!`s it ahead of this body), exactly the way
// `blur.frag`'s `BLUR_DIR` and `gauge.frag`'s own `LAYER` are: `GlUniforms` is
// one bag applied to every pass, so there is nowhere to say "this pass is the
// lit one". Two programs, one body — and here that is the only way the two
// passes can share `falloff` and `site_at`. A hand-copied second dot falloff is
// precisely the duplicate that drifts, and a drift between *these* two shows up
// as a halo that does not sit on the dots it came from.
//
//   * `LAYER == LAYER_LIT` writes the **lit layer** into an R8 aux texture: one
//     falloff dot per set font pixel, which is what the separable blur (the
//     shared `blur.frag`) turns into the skin's halo.
//   * `LAYER == LAYER_BLIT` writes the **screen**: the field, the ghost matrix
//     painted flat under everything (so it never blooms, exactly as the kit
//     paints it before the emission), the lit layer with its halo max-combined,
//     the CRT pass and the composite toward the ink.
//
// # Why the lit layer is recomputed here instead of sampled back
//
// The blit does **not** read the aux texture the lit pass wrote. It calls the
// same `lit_intensity` again, from the fragment's own position on the lattice —
// and that is the whole improvement. The aux texture exists only to be blurred.
//
// The CPU kit stamps each font pixel as a `dot_px`×`dot_px` block read out of a
// fixed integer table (`Dots::falloff`), so a dot is a *replicated font pixel*:
// blown up by GSK's nearest-neighbour scaling on a chip that layout gave more
// room than its natural size, it magnifies into visible squares. Here the
// falloff is the kit's own law evaluated at the fragment's continuous position
// on the lattice, so a stretched chip draws round dots at the screen's
// resolution rather than a magnified 4×4 block.
//
// At 1:1 — the natural size, which is what the reconciler requests and what the
// parity harness measures — the two are the **same arithmetic**: `site_at` is
// then evaluated at the pixel centre, where its `q` values are exactly the
// integers `Dots::new` feeds `intensity`, so the float path *is* the integer
// path. `u_viewport == u_grid` is the test that says so, and it snaps the
// coordinate to the pixel centre rather than trusting a `v_uv` round trip.
//
// Every composite step is **integer**, for the reason `scope_blit.frag` gives
// at length: the kit's `mix` is `(a * (255 - t) + b * t + 127) / 255`, so a
// float composite would differ in the last bit almost everywhere.

const int LAYER_LIT = 0;
const int LAYER_BLIT = 1;

// ── hytte-preem/src/font.rs — the 5×7 cell and its one-column gap. The kit's
// own public constants; `dot_matrix.rs` reads them from the crate rather than
// copying them, and `the_shader_and_the_mapping_agree_about_the_font_metrics`
// reads these three back out of this file so the two cannot drift.
const int GLYPH_W = 5;
const int GLYPH_H = 7;
const int SPACING = 1;

// ── hytte-preem/src/dot_matrix.rs — the radial falloff law, in the exact
// integers `intensity` is written in. `intensity` is private there, so these
// four numbers are the one thing on this side that is a copy rather than a
// read: 255 on the plateau (`s ≤ 1/2`), the `255 → 120` segment `795 - 1080*s`,
// and the `120 → 25` segment `238.75 - 190*s` continued to its floor.
// `the_falloff_law_is_the_kits_own_ghost_dot` measures the Rust mirror of these
// against the kit's rendered bytes, and
// `the_shader_and_the_mapping_agree_about_the_falloff_knots` reads them back
// out of this file, so a change to either side that is not a change to both
// goes red.
const float CORE = 255.0;
const float SEG1_BASE = 795.0;
const float SEG1_SLOPE = 1080.0;
const float SEG2_BASE = 955.0;
const float SEG2_SLOPE = 760.0;

// hytte-preem/src/style.rs — the CRT pass, verbatim from `scope_blit.frag`.
const int MASK_ONE = 256;
const int COORD_ONE = 1024;
const int BAND_DIV = 9;
const int CORNER_DIV = 6;

in vec2 v_uv;

uniform sampler2D u_tex0;   // the 1-D R32F glyph strip: one texel per glyph
                            // column, holding that column's GLYPH_H row bits
uniform sampler2D u_tex1;   // blit: the separably-blurred lit layer
uniform int u_data_len;     // texels in the strip (0 = an empty display)
uniform ivec2 u_grid;       // the buffer, which for this widget *is* native
uniform ivec2 u_viewport;   // the pass's viewport — equal to the grid at 1:1
uniform int u_dot;          // the dot pitch in buffer px (the kit's clamp)
uniform int u_cells;        // characters on the display
uniform int u_ghost_on;     // 0 on a skin with no unlit matrix (the OLED)
uniform vec4 u_ghost;       // the unlit dot colour, channels as 0..255
uniform int u_bloom_strength;   // halo strength in 256ths; 0 = no bloom
uniform vec4 u_bg;          // the skin's field, channels as 0..255
uniform vec4 u_ink;         // the skin's lit ink, channels as 0..255
uniform int u_mask_on;
uniform int u_mask_pitch;   // **the dot pitch** — `Mask::with_pitch` (#1091)
uniform int u_mask_phase;   // …and `pitch - 1`, the seam below each dot row
uniform int u_scanline_keep;
uniform int u_corner_keep;

out vec4 o_colour;

// ── the dot hardware (`hytte-preem/src/dot_matrix.rs`'s `Dots`) ─────────────

// `round_div`: `round(num / denom)`, half away from zero, for non-negative
// values. The kit does this in `usize`; every value reaching it here is a small
// integer exactly representable in `f32` (`denom ≤ 64`, `num ≤ 138240`), and
// the quotient is never within `1e-4` of a half-integer boundary at any pitch,
// so the float form returns the kit's own answer.
float round_div(float num, float denom) {
    return floor((2.0 * num + denom) / (2.0 * denom));
}

// `intensity`: the radial falloff, a piecewise-linear ramp in `s`, the squared
// distance from the dot's centre normalised by its half-pitch. `qx`/`qy` are
// the *doubled* offsets from the cell centre, which is what keeps the whole
// thing in exact integers at a pixel centre — see `radius_sq` in the kit.
float falloff(float qx, float qy, float denom) {
    float num = qx * qx + qy * qy;
    // s ≤ 1/2 — the plateau. At MIN_DOT_PX all four pixels sit here, which is
    // why the smallest pitch renders solid rather than washed out.
    if (2.0 * num <= denom) {
        return CORE;
    }
    // s ≤ 5/8 — the 255 → 120 segment.
    if (8.0 * num <= 5.0 * denom) {
        return clamp(round_div(SEG1_BASE * denom - SEG1_SLOPE * num, denom), 0.0, CORE);
    }
    // Everything beyond: the 120 → 25 segment, continued past its knot to the
    // floor — which is why a wide pitch's corners go properly dark.
    float quarters = SEG2_BASE * denom;
    float taken = SEG2_SLOPE * num;
    if (quarters <= taken) {
        return 0.0;
    }
    return clamp(round_div(quarters - taken, 4.0 * denom), 0.0, CORE);
}

// Where a point on the lattice falls: which character cell, which font pixel
// inside it, and how far the point is from that dot's centre.
struct Site {
    bool on;    // inside some cell's dot grid at all
    int cell;   // character index, 0..u_cells-1
    int col;    // glyph column, 0..GLYPH_W-1
    int row;    // glyph row, 0..GLYPH_H-1
    float qx;   // doubled offset from the dot centre, in buffer px …
    float qy;
};

// `DotMatrix::render`'s geometry, inverted: the bezel is one dot cell on every
// side, a character advances `(GLYPH_W + SPACING) * dot`, and the spacing
// column between two cells carries no dots because the hardware has none there.
//
// `p` is a **continuous** buffer coordinate. At 1:1 the caller hands it a pixel
// centre and every value below is exact: `fx`, `within` and `u` are differences
// of exactly representable numbers, and `qx` comes out as the integer
// `2*i + 1 - dot` the kit's `radius_sq` squares.
Site site_at(vec2 p) {
    Site s = Site(false, 0, 0, 0, 0.0, 0.0);
    float pitch = float(u_dot);
    float pad = pitch;                                  // `Dots::pad`
    float advance = float(GLYPH_W + SPACING) * pitch;   // `Dots::advance`

    float fy = p.y - pad;
    if (fy < 0.0 || fy >= float(GLYPH_H) * pitch) {
        return s;
    }
    float rowf = floor(fy / pitch);
    float v = fy - rowf * pitch;

    float fx = p.x - pad;
    if (fx < 0.0) {
        return s;
    }
    float cellf = floor(fx / advance);
    if (cellf >= float(u_cells)) {
        return s;
    }
    float within = fx - cellf * advance;
    if (within >= float(GLYPH_W) * pitch) {
        return s;   // the spacing column between two cells
    }
    float colf = floor(within / pitch);
    float u = within - colf * pitch;

    s.on = true;
    s.cell = int(cellf);
    s.col = int(colf);
    s.row = int(rowf);
    s.qx = 2.0 * u - pitch;
    s.qy = 2.0 * v - pitch;
    return s;
}

// `Dots::ghost_dot`, as an intensity: the unlit matrix shows through at **every**
// dot position of every cell, lit or not — exactly like the hardware.
int ghost_intensity(Site s) {
    if (!s.on) {
        return 0;
    }
    return int(falloff(s.qx, s.qy, float(u_dot * u_dot)));
}

// Is this font pixel set? The strip carries one texel per glyph column, holding
// that column's GLYPH_H row bits with bit `row` set for a lit pixel — see
// `dot_matrix.rs`'s `glyphs`, which builds it from `hytte_preem::font::glyph`
// (an uncovered char already resolved to the hollow `NOTDEF` box there).
bool glyph_bit(int cell, int col, int row) {
    int index = cell * GLYPH_W + col;
    if (index < 0 || index >= u_data_len) {
        return false;
    }
    int bits = int(texelFetch(u_tex0, ivec2(index, 0), 0).r + 0.5);
    return ((bits >> row) & 1) == 1;
}

// `Dots::lit_dot` into `Emission`: the falloff of a set font pixel, and nothing
// where the pixel is clear. The kit's `Emission::add` saturates at 255 and no
// two dot cells overlap, so a stamp is the value rather than an accumulation.
int lit_intensity(Site s) {
    if (!s.on || !glyph_bit(s.cell, s.col, s.row)) {
        return 0;
    }
    return int(falloff(s.qx, s.qy, float(u_dot * u_dot)));
}

// ── the composite (verbatim from `scope_blit.frag`) ────────────────────────

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
    if (LAYER == LAYER_LIT) {
        // An offscreen pass: the viewport **is** the grid and the aux textures
        // are indexed in kit rows throughout (row 0 is the top of the image),
        // so the pixel centre here is already the kit's own sample point — the
        // blit below flips once, and nowhere else.
        vec2 p = floor(gl_FragCoord.xy) + 0.5;
        o_colour = vec4(float(lit_intensity(site_at(p))) / 255.0, 0.0, 0.0, 1.0);
        return;
    }

    int cols = u_grid.x;
    int rows = u_grid.y;
    // Point sampling into the letterboxed fit rect, as `scope_blit.frag` does
    // it — used for the halo and the CRT pass, which are grid-resolution
    // quantities either way.
    int col = clamp(int(v_uv.x * float(cols)), 0, cols - 1);
    int row = clamp(int((1.0 - v_uv.y) * float(rows)), 0, rows - 1);

    // The lattice, on the other hand, is evaluated at the **fragment's own**
    // position — see the module header. At 1:1 that position is the pixel
    // centre, and it is taken from `col`/`row` rather than from the `v_uv`
    // product so no float round trip can move it off the centre the kit
    // samples at.
    vec2 p = vec2(v_uv.x * float(cols), (1.0 - v_uv.y) * float(rows));
    if (u_viewport == u_grid) {
        p = vec2(float(col), float(row)) + 0.5;
    }
    Site s = site_at(p);

    ivec4 bg = ivec4(u_bg + 0.5);
    ivec4 ink = ivec4(u_ink + 0.5);

    // The unlit matrix, painted flat under everything so it never picks up the
    // lit layer's bloom — the kit paints it into the frame before the emission
    // exists, for exactly that reason. A `t` of 0 is skipped, which is what the
    // kit's own `mix(under, ghost, 0)` returns anyway.
    ivec4 under = bg;
    if (u_ghost_on != 0) {
        int ghost = ghost_intensity(s);
        if (ghost > 0) {
            under = mix_kit(bg, ivec4(u_ghost + 0.5), ghost);
        }
    }

    // `Emission::bloom`: the blurred grid scaled by strength/256 and
    // max-combined under the original.
    int lit = lit_intensity(s);
    int halo = min(texel(u_tex1, ivec2(col, row)) * u_bloom_strength / 256, 255);
    lit = min(max(lit, halo), 255);

    // `Emission::composite`: unlit pixels are skipped *before* the mask is
    // consulted, which is why an unlit screen shows no scanlines. The mask is
    // evaluated at the buffer coordinate with no rescaling — unlike the gauge,
    // this widget has no upscale at all, since the dot pitch *is* its size knob
    // (#1091), so the buffer the kit composites over is the one drawn here.
    if (lit > 0) {
        if (u_mask_on != 0) {
            lit = lit * mask_keep(col, row, cols, rows) / MASK_ONE;
        }
        if (lit > 0) {
            under = mix_kit(under, ink, lit);
        }
    }

    // Opaque: a kit frame is a screen, never a sprite. The letterbox padding
    // around the fit rect is the transparent part, and the host clears it.
    o_colour = vec4(vec3(under.rgb) / 255.0, 1.0);
}
