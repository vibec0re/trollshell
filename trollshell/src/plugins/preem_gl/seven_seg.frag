// `seven_seg` — the ghost figure-8, the lit segments and the composite, as
// **one body compiled twice** (#1154).
//
// **The layer is a `const int LAYER` prepended by the Rust side**
// (`seven_seg.rs` `concat!`s it ahead of this body), exactly the way
// `blur.frag`'s `BLUR_DIR`, `gauge.frag`'s and `dot_matrix.frag`'s own `LAYER`
// are: `GlUniforms` is one bag applied to every pass, so there is nowhere to
// say "this pass is the lit one". Two programs, one body — and here that is the
// only way the two passes can share `strip255`, which is the whole geometry.
//
//   * `LAYER == LAYER_LIT` writes the **lit layer** into an R8 aux texture: the
//     kit's emission, 255 on a lit segment and 0 elsewhere, which is what the
//     separable blur (the shared `blur.frag`) turns into the skin's halo.
//   * `LAYER == LAYER_BLIT` writes the **screen**: the field, the ghost
//     figure-8 painted flat under everything (so it never blooms, exactly as
//     the kit paints it before the emission), the lit layer with its halo
//     max-combined, the CRT pass and the composite toward the ink.
//
// # The segment is a tapered hexagon, and here it is a continuous one
//
// `hytte-preem/src/seven_seg.rs`'s `stamp_bar` insets row/column `k` of a
// `THICK`-wide bar from both ends by `taper(k)`, which for `THICK = 6` is
// `2, 1, 0, 0, 1, 2`. That staircase samples a **45° chamfer**: writing the
// row's own centre as `k + 0.5` and the bar's centre line as `THICK / 2`,
//
//     taper(k) == max(0, |k + 0.5 - THICK/2| - TAPER_EDGE)
//
// exactly, at every `k` (`seven_seg.rs`'s
// `the_taper_law_is_the_kits_own_staircase` asserts it against
// `hytte_preem::seven_seg_taper`). So the segment is the intersection of three
// half-plane pairs — the two long faces, the two ends, and the four chamfers —
// and `bar_cov` is that intersection's signed distance in the only form this
// shader needs it: one coverage per face, `min`-combined, which is the union's
// `min`-of-distances written as areas instead.
//
// The CPU kit draws that hexagon as a six-row staircase at the buffer's own
// resolution and the shell blows the result up with `PixelSurface`'s
// nearest-neighbour scaling, so a stretched chip's mitres are 2×2 (or worse)
// blocks. Here the chamfer is evaluated at the **fragment's** own position and
// antialiased against the fragment's own footprint, so the diagonals are as
// smooth as the screen can draw them rather than a magnified stair. That is
// #865's "could be even better", on the one part of this widget that has a
// diagonal in it.
//
// # Why it is still bit-exact at 1:1
//
// Every one of the kit's boundaries is an **integer** buffer coordinate, and
// `TAPER_EDGE` is a half-integer, so at a pixel centre each of the three
// residuals is a non-zero half-integer: the point test below cannot tie, and it
// returns exactly what `stamp_bar`'s integer range test returns.
//
// The *coverage* does not, and that is why the snap is a branch rather than an
// optimisation. A 45° face cuts a pixel the kit fills whole or not at all, so
// `bar_cov` at 1:1 would return 0.85 and 0.15 along every mitre. `snapped` is
// therefore the point test and the continuous branch the coverage — the
// `textbox.frag` shape, where the kit's discrete disc and the arc it
// approximates are likewise two expressions of one geometry.
//
// `u_viewport == u_grid` is the gate, exactly as in `dot_matrix.frag` and with
// the same caveat: `u_viewport` is the allocation in **device** pixels, so it
// holds only on a scale-1 display showing the chip at its natural size. On a
// `scale_factor >= 2` monitor the continuous branch is the shipping path.
//
// # Why this pipeline has a blur pass where `led_strip.frag` does not
//
// #1153's meter has a closed-form halo because its emission is a *product* of
// two one-dimensional sets — a row of segments across, one band down — so each
// half of the kit's separable blur is a measure rather than a sum. A digit is a
// figure-8: which columns are lit depends on the row, so the vertical pass is a
// genuine sum over rows of a per-row horizontal measure and no closed form
// exists. So this arm takes `dot_matrix.frag`'s shape instead — render the
// emission at `u_grid`, blur it with the kit's own two passes, read it back —
// and the halo is read at the fragment's resolution off the snap (`halo_at`'s
// bilinear tap, #1186) so a stretched chip's bloom is not a grid staircase
// either.
//
// Every composite step is **integer**, for the reason `scope_blit.frag` gives
// at length: the kit's `mix` is `(a * (255 - t) + b * t + 127) / 255`, so a
// float composite would differ in the last bit almost everywhere.

const int LAYER_LIT = 0;
const int LAYER_BLIT = 1;

// The continuous reading of `hytte-preem`'s `taper`: how far from a bar's
// centre line the 45° chamfer starts. See the header — this is the **one**
// number on this side that is a law copy rather than a uniform read off the
// kit, and `the_taper_law_is_the_kits_own_staircase` holds it to
// `hytte_preem::seven_seg_taper`'s integers at every row.
const float TAPER_EDGE = 0.5;

// The chamfer residual of a bar that has no chamfer — see `bar_residuals`.
const float NO_CHAMFER = -1e9;

// Bars per digit cell — `hytte_preem::SEVEN_SEG_BARS.len()`, which is also how
// many `u_seg_*` uniforms this file declares, so the two cannot disagree
// silently. The ghost pass lights all of them, i.e. mask `(1 << SEG_COUNT) - 1`
// = the kit's own `SEG_ALL`.
const int SEG_COUNT = 7;
// Bit `SEG_COUNT` of a strip code: this cell is the **colon**, whose two dots
// are not segments and take no mask. The shell's encoding, not the kit's — a
// digit mask occupies bits `0..SEG_COUNT`, so the next one up is free. See
// `seven_seg.rs`'s `strip`.
const int COLON_BIT = 7;

// Steps of the binary search that finds which cell a fragment is in. The
// origins in the strip are strictly increasing, so `ceil(log2(cells))` steps
// suffice; the wire caps a readout at `MAX_STRIP_DIM / SEVEN_SEG_PITCH_PX`
// = 16384 / 40 = 409 cells, which needs 9. 16 covers any count up to 65536,
// which no buffer inside `MAX_RASTER_PIXELS` can reach.
const int CELL_SEARCH_STEPS = 16;

// hytte-preem/src/style.rs — the CRT pass, verbatim from `scope_blit.frag`.
const int MASK_ONE = 256;
const int COORD_ONE = 1024;
const int BAND_DIV = 9;
const int CORNER_DIV = 6;

in vec2 v_uv;

uniform sampler2D u_tex0;   // the 1-D R32F cell strip: two texels per cell,
                            // `[origin_x, code]` — see `seven_seg.rs`'s `strip`
uniform sampler2D u_tex1;   // blit: the separably-blurred lit layer
uniform int u_data_len;     // texels in the strip (0 = an empty readout)
uniform ivec2 u_grid;       // the buffer, which for this widget *is* native:
                            // there is no upscale, the cell metrics are the
                            // size knob (the dot matrix's situation, #1091)
uniform ivec2 u_viewport;   // the pass's viewport — equal to the grid at 1:1
uniform int u_cells;        // display cells, digits and colons together
uniform int u_pad;          // `hytte_preem::SEVEN_SEG_PAD`: every cell's top
                            // edge, and the field padding around the readout
uniform int u_thick;        // `SEVEN_SEG_THICK`: a bar's width across
// The nine elements a cell can stamp, each `(x, y, len, flags)` with `x`/`y`
// relative to the cell origin and `flags` = `vertical | tapered << 1`. Read
// straight off `hytte_preem::SEVEN_SEG_BARS` and `SEVEN_SEG_COLON_DOTS` rather
// than restated here, since a uniform can carry a Rust `const` where a GLSL
// literal cannot. The seven are in the kit's own bit order, A through G.
uniform vec4 u_seg_a;
uniform vec4 u_seg_b;
uniform vec4 u_seg_c;
uniform vec4 u_seg_d;
uniform vec4 u_seg_e;
uniform vec4 u_seg_f;
uniform vec4 u_seg_g;
uniform vec4 u_dot_0;       // the colon's upper dot …
uniform vec4 u_dot_1;       // … and its lower one
uniform int u_ghost_on;     // 0 on a skin with no ghost figure-8 (the OLED)
uniform vec4 u_ghost;       // the unlit segment colour, channels as 0..255
uniform int u_bloom_strength;   // halo strength in 256ths; 0 = no bloom;
                            // there is no `u_bloom_radius` here (#1293 item
                            // 9) — the radius is `blur.frag`'s uniform, read
                            // by the two blur passes that build `u_tex1`, not
                            // by this file, which only reads the blurred
                            // result back and scales it
uniform vec4 u_bg;          // the skin's field, channels as 0..255
uniform vec4 u_ink;         // the skin's lit ink
uniform int u_mask_on;
uniform int u_mask_pitch;   // the CRT comb, **not** re-phased: unlike the two
uniform int u_mask_phase;   // dot surfaces this widget has no dot grid, so it
uniform int u_scanline_keep;    // keeps `Mask::CRT` as the skin states it
uniform int u_corner_keep;

out vec4 o_colour;

// ── the segment geometry (`hytte-preem/src/seven_seg.rs`) ──────────────────

// One half-plane's coverage of a fragment whose footprint measures `w` across
// that face's normal: `r` is the signed distance in the same units, negative
// inside. A box filter of a straight edge, written as the linear ramp it is.
//
// At `w == 1` and a half-integer `r` this is exactly `0` or `1`, which is why
// the axis-aligned faces need no snap of their own; the chamfer does, and the
// header says why.
float face_cov(float r, float w) {
    return clamp(0.5 - r / max(w, 1e-6), 0.0, 1.0);
}

// The three residuals of one bar at a point, in the bar's own frame: `along`
// down its long axis and `across` its thickness, both already folded to
// absolute offsets from the bar's centre. Each is a signed distance along that
// face's normal, **unnormalised for the chamfer** — dividing it by `sqrt(2)`
// would not change its sign, and `bar_cov` divides by a footprint that carries
// the same factor, so the two cancel and no irrational constant appears here at
// all.
//
// An untapered bar (the colon's dots) gets [`NO_CHAMFER`] rather than a small
// negative number: the residual is also divided by the footprint downstream, so
// a `-1.0` would start *limiting* coverage once one fragment spanned more than
// two buffer pixels. A sentinel this far below zero cannot, at any footprint a
// float can hold.
vec3 bar_residuals(float along, float across, float hl, float hs, bool tapered) {
    return vec3(
        across - hs,
        along - hl,
        tapered ? (along + across - TAPER_EDGE - hl) : NO_CHAMFER
    );
}

// Is `p` inside this bar? The **point** test, which is the kit's own integer
// range test at a pixel centre — see the header on why no residual can tie
// there.
bool bar_inside(vec3 r) {
    return max(max(r.x, r.y), r.z) <= 0.0;
}

// The fraction of a fragment's footprint inside this bar: each face's coverage,
// `min`-combined.
//
// `fl`/`fs` are the footprint along the bar's long and short axes. The
// chamfer's unit normal is `(1, 1) / sqrt(2)`, so a box footprint measures
// `(fl + fs) / sqrt(2)` across it against a distance of `r.z / sqrt(2)` — the
// ratio the ramp wants, with both roots cancelled.
//
// `min` rather than the exact area: it is exact for a fragment straddling one
// face, which is every fragment on a straight run, and it slightly overstates
// the cut at a vertex where two faces meet inside one footprint. That is a
// sub-pixel effect on a shape whose vertices are three pixels apart, and it is
// the same trade every convex-SDF antialias makes.
float bar_cov(vec3 r, float fl, float fs) {
    return min(
        min(face_cov(r.x, fs), face_cov(r.y, fl)),
        face_cov(r.z, fl + fs)
    );
}

// One element's intensity at `p`, which is **relative to the cell origin**.
// `fp` is the fragment's footprint in buffer units; `snapped` selects the point
// test over the coverage.
int bar255(vec2 p, vec2 fp, bool snapped, vec4 bar) {
    bool vertical = (int(bar.w + 0.5) & 1) == 1;
    bool tapered = (int(bar.w + 0.5) & 2) == 2;
    float hs = float(u_thick) * 0.5;
    float hl = bar.z * 0.5;
    // The bar's centre, then the offset to it, in (long, short) order.
    vec2 centre = vertical
        ? vec2(bar.x + hs, bar.y + hl)
        : vec2(bar.x + hl, bar.y + hs);
    vec2 d = abs(p - centre);
    float along = vertical ? d.y : d.x;
    float across = vertical ? d.x : d.y;
    vec3 r = bar_residuals(along, across, hl, hs, tapered);
    if (snapped) {
        return bar_inside(r) ? 255 : 0;
    }
    float fl = vertical ? fp.y : fp.x;
    float fs = vertical ? fp.x : fp.y;
    return int(255.0 * bar_cov(r, fl, fs) + 0.5);
}

// One cell's intensity at `p`, relative to that cell's origin.
//
// `code` is the strip's own encoding: `COLON_BIT` set is the colon, and every
// other value is a digit mask over the seven bars in the kit's bit order. The
// elements of a cell **overlap** — segment `A` and segment `F` share a corner —
// and the kit's two sinks are both idempotent there (`Frame::set` writes a
// colour, `Emission::add` saturates at 255), so a union is exactly `max`.
int cell255(vec2 p, vec2 fp, bool snapped, int code) {
    if ((code & (1 << COLON_BIT)) != 0) {
        return max(
            bar255(p, fp, snapped, u_dot_0),
            bar255(p, fp, snapped, u_dot_1)
        );
    }
    vec4 bars[SEG_COUNT] = vec4[SEG_COUNT](
        u_seg_a, u_seg_b, u_seg_c, u_seg_d, u_seg_e, u_seg_f, u_seg_g
    );
    int best = 0;
    for (int i = 0; i < SEG_COUNT; ++i) {
        if ((code & (1 << i)) != 0) {
            best = max(best, bar255(p, fp, snapped, bars[i]));
        }
    }
    return best;
}

// Cell `i`'s left edge in buffer px — `hytte_preem::seven_seg_layout`'s own
// answer, encoded into the strip rather than re-derived from `PAD`, the cell
// widths and `GAP`.
float cell_x(int i) {
    int index = 2 * i;
    if (index < 0 || index >= u_data_len) {
        return 0.0;
    }
    return texelFetch(u_tex0, ivec2(index, 0), 0).r;
}

// Cell `i`'s code — see `cell255`. `ghost` asks for the *unlit* figure, which
// is every segment of a digit cell and both dots of a colon, i.e. the kit's own
// `SEG_ALL` and the colon it already stamps.
int cell_code(int i, bool ghost) {
    int index = 2 * i + 1;
    if (index < 0 || index >= u_data_len) {
        return 0;
    }
    int code = int(texelFetch(u_tex0, ivec2(index, 0), 0).r + 0.5);
    if (ghost && (code & (1 << COLON_BIT)) == 0) {
        return (1 << SEG_COUNT) - 1;
    }
    return code;
}

// The last cell whose origin is at or before `x` — a binary search over the
// strip's strictly increasing origins. Cells have different widths (a colon is
// narrower than a digit), so there is nothing to divide by; the origins are the
// only monotone thing here, and the kit wrote them.
int cell_at(float x) {
    int lo = 0;
    int hi = u_cells - 1;
    for (int s = 0; s < CELL_SEARCH_STEPS && lo < hi; ++s) {
        int mid = (lo + hi + 1) / 2;
        if (cell_x(mid) <= x) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    return lo;
}

// The whole readout's intensity at a buffer position `p`.
//
// Two cells are consulted, `i` and `i + 1`, and that is a bound rather than a
// sample: cells never overlap, so `max` over them is the union, and cell
// `i - 1`'s right edge is at least `SEVEN_SEG_GAP` = 10 buffer px to the left
// of `cell_x(i) <= p.x`. It could only contribute to a footprint wider than
// `2 * GAP`, i.e. on a chip CSS has squeezed below a twentieth of its natural
// width, where the readout is a few pixels across and there is nothing to read
// either way. Cell `i + 1` genuinely can: its left edge may be inside the
// footprint's right half at any scale.
int strip255(vec2 p, vec2 fp, bool snapped, bool ghost) {
    if (u_cells <= 0) {
        return 0;
    }
    int first = cell_at(p.x);
    int best = 0;
    for (int k = 0; k < 2; ++k) {
        int i = first + k;
        if (i >= u_cells) {
            break;
        }
        vec2 local = p - vec2(cell_x(i), float(u_pad));
        best = max(best, cell255(local, fp, snapped, cell_code(i, ghost)));
    }
    return best;
}

// ── the composite (verbatim from `scope_blit.frag`) ────────────────────────

int texel(sampler2D tex, ivec2 p) {
    return int(texelFetch(tex, p, 0).r * 255.0 + 0.5);
}

// The blurred lit layer read at a **continuous** grid position: the four texels
// around `p` mixed bilinearly (#1186, verbatim from `dot_matrix.frag` — see
// that file for why this is hand-written rather than a filtered `texture()`
// call, and why `CLAMP_TO_EDGE`'s border behaviour is spelled out here).
int halo_at(vec2 p) {
    vec2 t = p - 0.5;                   // texel centres sit at integer + 0.5
    vec2 base = floor(t);
    vec2 f = t - base;
    ivec2 lo = ivec2(0, 0);
    ivec2 hi = u_grid - 1;
    ivec2 a = clamp(ivec2(base), lo, hi);
    ivec2 b = clamp(ivec2(base) + 1, lo, hi);
    float v00 = float(texel(u_tex1, ivec2(a.x, a.y)));
    float v10 = float(texel(u_tex1, ivec2(b.x, a.y)));
    float v01 = float(texel(u_tex1, ivec2(a.x, b.y)));
    float v11 = float(texel(u_tex1, ivec2(b.x, b.y)));
    return int(mix(mix(v00, v10, f.x), mix(v01, v11, f.x), f.y) + 0.5);
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
        // blit below flips once, and nowhere else. The emission the kit blurs
        // is `Emission::add(.., 255)` on a lit segment and nothing elsewhere,
        // so this is the point test and never the coverage.
        vec2 p = floor(gl_FragCoord.xy) + 0.5;
        o_colour = vec4(float(strip255(p, vec2(1.0), true, false)) / 255.0, 0.0, 0.0, 1.0);
        return;
    }

    int cols = u_grid.x;
    int rows = u_grid.y;
    // Point sampling into the letterboxed fit rect, as `scope_blit.frag` does
    // it — used for the CRT pass, which is a grid-resolution quantity by
    // definition (`MaskRow` is a row of the *buffer*), and for the snapped
    // branch's sample point below.
    int col = clamp(int(v_uv.x * float(cols)), 0, cols - 1);
    int row = clamp(int((1.0 - v_uv.y) * float(rows)), 0, rows - 1);

    vec2 pc = vec2(v_uv.x * float(cols), (1.0 - v_uv.y) * float(rows));
    bool snapped = (u_viewport == u_grid);
    vec2 p = snapped ? vec2(float(col), float(row)) + 0.5 : pc;
    // The fragment's footprint in buffer units. Exactly `(1, 1)` on the snapped
    // branch by construction rather than by a division that happens to land
    // there — and unread there anyway, since `snapped` takes the point test.
    vec2 fp = snapped
        ? vec2(1.0)
        : vec2(float(cols), float(rows)) / vec2(max(u_viewport.x, 1), max(u_viewport.y, 1));

    ivec4 bg = ivec4(u_bg + 0.5);
    ivec4 ink = ivec4(u_ink + 0.5);

    // The ghost figure-8, painted flat under everything so it never picks up
    // the lit layer's bloom — the kit paints it into the frame before the
    // emission exists, for exactly that reason. It is a `set` there rather than
    // a composite; a `t` of 255 through `mix_kit` is that same `set` to the
    // byte, and a `t` of 0 is the field.
    ivec4 under = bg;
    if (u_ghost_on != 0) {
        int ghost = strip255(p, fp, snapped, true);
        if (ghost > 0) {
            under = mix_kit(bg, ivec4(u_ghost + 0.5), ghost);
        }
    }

    // `Emission::bloom`: the blurred grid scaled by strength/256 and
    // max-combined under the original. The *read* is gated exactly the way the
    // sample point above is (#1186): the single `texelFetch` on the snapped
    // branch, which is what keeps the 1:1 cases bit-exact, and `halo_at`'s
    // bilinear tap on the continuous one.
    int lit = strip255(p, fp, snapped, false);
    int glow = snapped ? texel(u_tex1, ivec2(col, row)) : halo_at(pc);
    int halo = min(glow * u_bloom_strength / 256, 255);
    lit = min(max(lit, halo), 255);

    // `Emission::composite`: unlit pixels are skipped *before* the mask is
    // consulted, which is why an unlit screen shows no scanlines. The mask is
    // evaluated at the buffer coordinate with no rescaling — this widget has no
    // upscale at all (the cell metrics are its size knob), so the buffer the
    // kit composites over is the one drawn here.
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
