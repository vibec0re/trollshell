// The **split-flap board / nixie readout** (#1155): the fold, the fixture and
// the composite, in one body spliced three ways by `flip_board.rs` — the lit
// layer at the kit's grid, the nixie's wide-halo combine, and the blit.
//
// # What the kit does, and where this differs
//
// `hytte-preem/src/split_flap.rs` rasterises into a **logical** `cols × rows`
// buffer and replicates it `scale` times on the way out (`render` ends in
// `Frame::upscale`), which is the `Scope`'s shape and not the `DotMatrix`'s:
// `u_grid` here is that pre-upscale buffer and the natural size the reconciler
// asks for is `grid × scale`. So a shipping board — `scale` defaults to 2 —
// always draws through this file's **continuous** branch, and that is where the
// whole improvement lives.
//
// Three things in a frame, and only one of them is continuous:
//
//   * the **fixture** — the two card faces, the nixie's unlit cathode stack,
//     the bezel and the hinge slot — is painted at whole logical pixels and is
//     byte-identical in every frame of a flip (the kit's own
//     `the_fixture_never_moves_while_the_cards_do`). It is sampled here at the
//     fragment's logical pixel, which is exactly what `Frame::upscale`
//     replicates, so it is the kit's fixture at any viewport.
//   * the **glyphs** are 5x7 bitmap pixels and square is the look, so the
//     resting halves are floored to a logical pixel too — `textbox.frag`'s rule
//     (bigger hard-edged pixels, never a smoothed font).
//   * the **falling card** genuinely moves between rows. A rotation is not a
//     translation, so the kit resamples: a destination row takes the exact
//     coverage-weighted average of the source rows it now spans, and a partly
//     covered row blends the card over what is behind it. The kit does that per
//     *logical* row and then replicates the answer, so the flap's free edge and
//     the fold's boundary come out as `scale`-tall bands of one stair. Here the
//     same integral is taken over the **fragment's own** vertical footprint, so
//     the edge and the boundary land where the screen can draw them.
//
// The nixie has no sub-pixel geometry at all — it is a bitmap cross-fade
// between two cathodes — so its emission is point-sampled on both arms and its
// improvement is the **halo**, read bilinearly at the fragment's resolution off
// the snap (#1186's tap) rather than replicated out of the kit's grid.
//
// # Two bloom stages, because a tube glows twice
//
// `FlipBoard::render` blooms a nixie **twice**: a wide weak pass at
// `radius + NIXIE_HALO_RADIUS_BONUS` / strength `NIXIE_HALO_STRENGTH` first,
// then the palette's own on top, each one `max`-combining into the emission so
// the core never dims. `Emission::bloom` is not linear and the second pass
// blurs the *result* of the first, so the two cannot be folded. Hence
// `LAYER_HALO1`: an offscreen pass that recombines the wide halo into the
// emission at the grid, whose output is what the second blur reads.
//
// A split-flap board blooms once, and a skin with no bloom not at all. Both
// fall out of the same passes rather than a second pipeline: a radius of `0`
// makes `blur.frag` the identity and a strength of `0` makes the combine
// `max(v, 0) == v`, exactly as the kit's `Emission::bloom` early return does.
// (The one shape that would not: a hypothetical skin with `radius: 0` and
// `strength > 256`, where `max(v, v * strength / 256)` exceeds `v` while the
// kit returns early. No skin has one — `program.rs`'s module docs record the
// same corner for the scope.)
//
// # Nothing geometric is a GLSL literal
//
// Every length is a uniform read off `FlipBoard::metrics()` (`pub` since
// #1155, on the `Gauge::dial` / `SEVEN_SEG_BARS` precedent), every intensity is
// the kit's own constant, and every per-cell number — the fold's band, the
// shading, the free edge's rule and peak, the two nixie levels — is computed
// once on the CPU from the kit's own `flap_theta`/`ignite`/`afterglow` and
// arrives in the strip. The only numbers restated here are `GLYPH_W`/`GLYPH_H`
// (pinned to `hytte_preem::font`'s by `flip_board.rs`'s source scan) and the
// CRT pass's four fixed-point constants, which every shader on this seam
// restates and `program::assert_crt_constants` holds to the kit's items.
//
// # Division, and where it is allowed (#1298/#1309)
//
// GLSL ES 3.20 §4.7.1 pins `a + b`, `a - b` and `a * b` to a correctly rounded
// result and allows `a / b` **2.5 ULP**. So the sample point is rebuilt from
// the fragment's own integer index and scaled by `u_px_step` — divided once on
// the CPU in `gl_surface.rs`, where IEEE-754 says correctly rounded — and this
// file's `main` contains no division of its own.
//
// Two divisions remain inside the fold, and they are **the kit's**, not this
// arm's: `(lo - band_lo) / squash` maps a destination row onto its source span,
// and `acc / span` normalises the coverage integral. `FlipBoard::compose_flap`
// spells both, so removing them here would not make the two arms agree — it
// would make them disagree. `flip_board.rs`'s
// `the_coverage_bytes_are_never_decided_by_the_divides_slack` is the census:
// every byte of every 1:1 parity case re-derived with both quotients perturbed
// by ±2.5 ULP, asserting the byte never moves, with a negative control that
// perturbs by a millionth and requires that it does.
//
// A third division — `cover / fp.y`, which turns the covered *length* of a
// fragment into the covered *fraction* the kit's unit-tall row already is —
// exists **only on the continuous branch**, where nothing is held bit-exact.
// The snapped branch's row is one buffer pixel tall by construction, so the
// length is the fraction and there is nothing to divide by.
//
// # The composite is integer
//
// For the reason `scope_blit.frag` gives at length: the kit's `mix` is
// `(a * (255 - t) + b * t + 127) / 255`, so a float composite would differ in
// the last bit almost everywhere.

const int LAYER_LIT = 0;
const int LAYER_HALO1 = 1;
const int LAYER_BLIT = 2;

// `Mechanism`, as `flip_board.rs`'s `mechanism_code` encodes it.
const int MECH_SPLIT_FLAP = 0;
const int MECH_NIXIE = 1;

// `hytte_preem::font`'s glyph box. Restated because a `const` cannot be a
// uniform array bound; `the_shader_restates_only_the_fonts_glyph_box` holds
// both to the kit's own items.
const int GLYPH_W = 5;
const int GLYPH_H = 7;

// Texels per cell in the strip — see `flip_board.rs`'s `CellStrip`.
const int CELL_TEXELS = 14;

// hytte-preem/src/style.rs — the CRT pass, verbatim from `scope_blit.frag`.
const int MASK_ONE = 256;
const int COORD_ONE = 1024;
const int BAND_DIV = 9;
const int CORNER_DIV = 6;

in vec2 v_uv;

uniform sampler2D u_tex0;   // lit: the cell strip. halo1: the lit layer.
                            // blit: the cell strip
uniform sampler2D u_tex1;   // halo1: the wide-blurred lit layer.
                            // blit: the same, for the nixie's broad haze
uniform sampler2D u_tex2;   // blit: the narrow-blurred combined layer — the
                            // skin's own bloom
uniform int u_data_len;     // texels in the strip (0 = an empty board)
uniform ivec2 u_grid;       // the kit's **pre-upscale** buffer
uniform ivec2 u_viewport;   // the pass's viewport — equal to the grid at 1:1
uniform vec2 u_px_step;     // `vec2(u_grid) / vec2(max(u_viewport, 1))`, the
                            // device->buffer step, **divided on the CPU**
                            // (`gl_surface.rs`, #1298)

uniform int u_mechanism;    // MECH_SPLIT_FLAP or MECH_NIXIE
uniform int u_cells;        // cards in the row; 0 is the degenerate board
// `FlipBoard::metrics()`, in logical px — never re-derived from the card
// constants here, which is the whole reason that accessor is `pub`.
uniform int u_cell_w;
uniform int u_cell_h;
uniform int u_hinge;
uniform int u_bezel;
uniform int u_gap;
uniform int u_glyph_px;
uniform int u_glyph_pad;
// The kit's fixture tones, of 255: the upper card's face, the lower card's
// (darker, because the light is above), and the nixie's cathode stack.
uniform int u_face_top;
uniform int u_face_bottom;
uniform int u_cathode;
// `hytte_preem::nixie_cathode_stack()` packed the way a cell's glyphs are —
// the ten digit cathodes stacked, which is what an unlit tube shows.
uniform int u_stack_lo;
uniform int u_stack_hi;

uniform int u_ghost_on;     // 0 on a skin with no ghost (the OLED, the CRT)
uniform vec4 u_ghost;       // the fixture colour, channels as 0..255
uniform int u_bloom_strength;   // the skin's own halo, in 256ths
uniform int u_halo_strength;    // the nixie's wide pass — 0 for a split flap
                            // and for a skin with no bloom. The two radii are
                            // `blur.frag`'s uniforms (`u_bloom_radius` and the
                            // spliced `u_halo_radius`), read by the four blur
                            // passes that build `u_tex1`/`u_tex2` and never by
                            // this file, which only scales the results
uniform vec4 u_bg;          // the skin's field, channels as 0..255
uniform vec4 u_ink;         // the skin's lit ink
uniform int u_mask_on;
uniform int u_mask_pitch;   // the CRT comb, **not** re-phased: this widget has
uniform int u_mask_phase;   // no dot grid for a comb to sit in the seams of, so
uniform int u_scanline_keep;    // it keeps `Mask::CRT` as the skin states it —
uniform int u_corner_keep;  // which is what the kit's own `composite` hands
                            // `Emission::composite` here

out vec4 o_colour;

// ── the strip ──────────────────────────────────────────────────────────────

float strip_at(int index) {
    if (index < 0 || index >= u_data_len) {
        return 0.0;
    }
    return texelFetch(u_tex0, ivec2(index, 0), 0).r;
}

// A strip texel that carries a small non-negative integer.
int strip_int(int index) {
    return int(strip_at(index) + 0.5);
}

// ── the glyph lattice (`FlipBoard::stamp_glyph`/`glyph_at`) ────────────────

// Is row `row`, column `col` of a packed glyph lit? `lo` carries font rows
// 0..3 and `hi` rows 4..6, five bits each, low row first — see
// `flip_board.rs`'s `pack_glyph`. The kit indexes a row's bits from the *left*
// (`GLYPH_W - 1 - col`), and so does this.
int glyph_bit(int lo, int hi, int row, int col) {
    int packed = row < 4 ? lo : hi;
    int shift = (row < 4 ? row : row - 4) * 5;
    int bits = (packed >> shift) & 31;
    return (bits >> (GLYPH_W - 1 - col)) & 1;
}

// `FlipBoard::glyph_at`: whether the glyph is lit at cell-local logical pixel
// (`x`, `y`) — the resting card's binary raster. The kit's `checked_sub(pad)`
// is this file's `x < pad` guard: a coordinate inside the card's padding is
// unlit rather than wrapping onto the last font column.
float glyph_at(int lo, int hi, int x, int y) {
    if (x < u_glyph_pad || y < u_glyph_pad) {
        return 0.0;
    }
    int col = (x - u_glyph_pad) / u_glyph_px;
    int row = (y - u_glyph_pad) / u_glyph_px;
    if (col >= GLYPH_W || row >= GLYPH_H) {
        return 0.0;
    }
    return float(glyph_bit(lo, hi, row, col));
}

// `FlipBoard::glyph_coverage`: the lit fraction of a glyph over the cell-local
// **source** span `[lo, hi)` at logical column `x` — the exact area average of
// the font rows that span covers, normalised by its own (unclipped) length.
//
// This is the fold's resample, and it degenerates *exactly* at rest: an
// unsquashed card maps one destination row onto one source row, so the average
// is that row's single bit and the frame is a bit-for-bit copy of the resting
// glyph.
//
// The loop starts at font row `0` where the kit starts at `floor(lo / (g))`.
// That is deliberate and it is not an approximation: a row entirely before `lo`
// has `row_hi <= lo`, so its clamped overlap is exactly `0.0` and `acc + 0.0`
// is `acc` on any IEEE-754 arithmetic. It buys the removal of one division the
// kit only performs to pick a starting index, on a loop that is seven
// iterations either way.
float glyph_coverage(int lo, int hi, int x, float src_lo, float src_hi) {
    float span = src_hi - src_lo;
    if (span <= 0.0) {
        return 0.0;
    }
    if (x < u_glyph_pad) {
        return 0.0;
    }
    int col = (x - u_glyph_pad) / u_glyph_px;
    if (col >= GLYPH_W) {
        return 0.0;
    }
    float padf = float(u_glyph_pad);
    float clipped_lo = max(src_lo - padf, 0.0);
    float clipped_hi = min(src_hi - padf, float(GLYPH_H * u_glyph_px));
    if (clipped_hi <= clipped_lo) {
        return 0.0;
    }
    float acc = 0.0;
    for (int row = 0; row < GLYPH_H; ++row) {
        float row_lo = float(row * u_glyph_px);
        float row_hi = float((row + 1) * u_glyph_px);
        if (row_lo >= clipped_hi) {
            break;
        }
        if (glyph_bit(lo, hi, row, col) == 1) {
            acc += max(min(clipped_hi, row_hi) - max(clipped_lo, row_lo), 0.0);
        }
    }
    return acc / span;
}

// `hytte_preem::flip_level`: a `0..=255` intensity rounded onto the emission's
// scale. Non-negative by the clamp, so `int()`'s truncation toward zero is the
// floor and `+ 0.5` is round-half-up — which is what `f32::round` is there.
int level(float value) {
    return int(clamp(value, 0.0, 255.0) + 0.5);
}

// ── the row (`FlipBoard::cell_w`/`gap`/`bezel`) ────────────────────────────

// Which card a logical buffer column falls in, or `-1` for the bezel and the
// gaps between cards. The row is uniform — every card is `u_cell_w` wide on a
// `u_cell_w + u_gap` pitch — so this is a divide and a remainder rather than
// the seven-segment readout's binary search over origins.
int cell_of(int col) {
    if (u_cells <= 0) {
        return -1;
    }
    int rel = col - u_bezel;
    if (rel < 0) {
        return -1;
    }
    int pitch = max(u_cell_w + u_gap, 1);
    int index = rel / pitch;
    if (index >= u_cells || rel - index * pitch >= u_cell_w) {
        return -1;
    }
    return index;
}

// This card's local column, given the card `cell_of` returned.
int cell_local_x(int col, int index) {
    return col - u_bezel - index * (u_cell_w + u_gap);
}

// ── the mechanisms ─────────────────────────────────────────────────────────

// `FlipBoard::compose_flap`, at one fragment.
//
// `x` is the card-local logical column, `row` the card-local logical row (the
// hard-edged half: which resting glyph is behind, and which half of the card
// it belongs to), and `[lo, hi]` the fragment's own vertical extent in
// card-local logical units — one whole row on the snapped branch, the
// fragment's footprint on the continuous one.
int flap255(int base, int x, int row, float lo, float hi, float fstep, bool snapped) {
    float band_lo = strip_at(base + 4);
    float band_hi = strip_at(base + 5);
    float edge_lo = strip_at(base + 6);
    float edge_hi = strip_at(base + 7);
    float edge_peak = strip_at(base + 8);
    float shade = strip_at(base + 9);
    float squash = strip_at(base + 10);
    bool falling_up = strip_int(base + 11) != 0;
    // The leaf is the outgoing card while it folds away above the hinge and
    // the incoming one as it folds in below — the kit's own choice, resolved
    // here rather than encoded a third time.
    int leaf_lo = falling_up ? strip_int(base + 0) : strip_int(base + 2);
    int leaf_hi = falling_up ? strip_int(base + 1) : strip_int(base + 3);

    float covered = max(min(hi, band_hi) - max(lo, band_lo), 0.0);
    // The kit's row is one logical pixel tall, so its covered *length* is the
    // covered *fraction*. A fragment is only that tall on the snapped branch.
    float cover = snapped ? covered : covered / max(fstep, 1e-6);
    bool has_source = covered > 0.0 && squash > 0.0;

    float src_lo = 0.0;
    float src_hi = 0.0;
    if (has_source) {
        float mid = float(u_hinge);
        float from = max(lo, band_lo);
        float to = min(hi, band_hi);
        if (falling_up) {
            src_lo = (from - band_lo) / squash;
            src_hi = (to - band_lo) / squash;
        } else {
            src_lo = mid + (from - mid) / squash;
            src_hi = mid + (to - mid) / squash;
        }
    }

    float ruled = max(min(hi, edge_hi) - max(lo, edge_lo), 0.0);
    float edge = edge_peak * (snapped ? ruled : ruled / max(fstep, 1e-6));

    // The upper half always shows the incoming card's top, the lower half the
    // outgoing card's bottom.
    int behind_lo = row < u_hinge ? strip_int(base + 2) : strip_int(base + 0);
    int behind_hi = row < u_hinge ? strip_int(base + 3) : strip_int(base + 1);
    float behind = glyph_at(behind_lo, behind_hi, x, row) * 255.0;

    // `precise` because the kit's line is `cover.mul_add(…)`, a **fused**
    // multiply-add: GLSL ES 3.20 leaves `fma()` free to be split into a
    // multiply and an add unless the computation is precise, and a split one
    // rounds twice where the kit rounds once.
    precise float value;
    if (has_source) {
        float card = glyph_coverage(leaf_lo, leaf_hi, x, src_lo, src_hi) * 255.0 * shade;
        // A partly covered row blends the card over what is behind it — the
        // card *occludes*, it does not add.
        value = fma(cover, max(card, edge) - behind, behind);
    } else {
        value = behind;
    }
    return value > 0.0 ? level(value) : 0;
}

// `FlipBoard::compose_nixie`, at one fragment: the outgoing cathode's afterglow
// and the incoming one's strike, max-combined — two discharges in one envelope,
// so the brighter wins wherever their strokes coincide rather than summing into
// a blob. No geometry moves, so this is the same point test on both branches.
int nixie255(int base, int x, int row) {
    bool by_out = glyph_at(strip_int(base + 0), strip_int(base + 1), x, row) > 0.0;
    bool by_in = glyph_at(strip_int(base + 2), strip_int(base + 3), x, row) > 0.0;
    int out_level = strip_int(base + 12);
    int in_level = strip_int(base + 13);
    if (by_out && by_in) {
        return max(out_level, in_level);
    }
    if (by_out) {
        return out_level;
    }
    if (by_in) {
        return in_level;
    }
    return 0;
}

// The whole board's emission at a fragment: `p` is the buffer position (a
// logical pixel centre on the snapped branch), `fstep` the fragment's vertical
// footprint in buffer units, and `col`/`row` its logical pixel.
int board255(int col, int row, vec2 p, float fstep, bool snapped) {
    int index = cell_of(col);
    if (index < 0) {
        return 0;
    }
    int local_y = row - u_bezel;
    if (local_y < 0 || local_y >= u_cell_h) {
        return 0;
    }
    int x = cell_local_x(col, index);
    int base = index * CELL_TEXELS;
    if (u_mechanism == MECH_NIXIE) {
        return nixie255(base, x, local_y);
    }
    float lo;
    float hi;
    if (snapped) {
        lo = float(local_y);
        hi = lo + 1.0;
    } else {
        float centre = p.y - float(u_bezel);
        lo = centre - 0.5 * fstep;
        hi = centre + 0.5 * fstep;
    }
    return flap255(base, x, local_y, lo, hi, fstep, snapped);
}

// `FlipBoard::paint_fixture`'s tone at a logical pixel, of 255, or `0` where
// the fixture paints nothing. The bezel, the gaps and a skin with no ghost are
// all "nothing", which is the kit's own `let Some(ghost) = … else { return }`.
int fixture255(int col, int row) {
    int index = cell_of(col);
    if (index < 0) {
        return 0;
    }
    int local_y = row - u_bezel;
    if (local_y < 0 || local_y >= u_cell_h) {
        return 0;
    }
    if (u_mechanism == MECH_SPLIT_FLAP) {
        return local_y < u_hinge ? u_face_top : u_face_bottom;
    }
    int x = cell_local_x(col, index);
    return glyph_at(u_stack_lo, u_stack_hi, x, local_y) > 0.0 ? u_cathode : 0;
}

// ── the composite (verbatim from `seven_seg.frag`) ─────────────────────────

int texel(sampler2D tex, ivec2 p) {
    return int(texelFetch(tex, p, 0).r * 255.0 + 0.5);
}

// A blurred layer read at a **continuous** grid position: the four texels
// around `p` mixed bilinearly (#1186, verbatim from `dot_matrix.frag`).
//
// Its `+ 0.5` is a truncation boundary the geometry reaches at an integer
// stretch, exactly as `seven_seg.frag`'s does, and it is deliberately left
// alone for the same reason: the boundary is reached with a non-zero value, so
// there is no exact numerator to lean on, and moving it would change the
// bloom's bytes everywhere rather than only where an implementation could
// disagree. It is worth one 255th of `glow`, which the scaling below and the
// `max` can only shrink. The snapped branch never reaches it.
int halo_at(sampler2D tex, vec2 p) {
    vec2 t = p - 0.5;                   // texel centres sit at integer + 0.5
    vec2 base = floor(t);
    vec2 f = t - base;
    ivec2 lo = ivec2(0, 0);
    ivec2 hi = u_grid - 1;
    ivec2 a = clamp(ivec2(base), lo, hi);
    ivec2 b = clamp(ivec2(base) + 1, lo, hi);
    float v00 = float(texel(tex, ivec2(a.x, a.y)));
    float v10 = float(texel(tex, ivec2(b.x, a.y)));
    float v01 = float(texel(tex, ivec2(a.x, b.y)));
    float v11 = float(texel(tex, ivec2(b.x, b.y)));
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

// `MaskRow::keep`: radial vignette x rounded-glass edge ramp x scanline comb.
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
        int col = int(p.x);
        int row = int(p.y);
        o_colour = vec4(float(board255(col, row, p, 1.0, true)) / 255.0, 0.0, 0.0, 1.0);
        return;
    }

    if (LAYER == LAYER_HALO1) {
        // `Emission::bloom`'s first pass for a nixie: the wide, weak haze
        // max-combined back into the emission, so the second blur runs over
        // exactly what the kit's second `bloom` call sees. For a split flap —
        // and for any skin with no bloom — `u_halo_strength` is `0` and this is
        // the identity.
        ivec2 q = ivec2(gl_FragCoord.xy);
        int emitted = texel(u_tex0, q);
        int haze = min(texel(u_tex1, q) * u_halo_strength / 256, 255);
        o_colour = vec4(float(max(emitted, haze)) / 255.0, 0.0, 0.0, 1.0);
        return;
    }

    int cols = u_grid.x;
    int rows = u_grid.y;
    // **The sample point is built from the fragment's own integer index, not
    // from the interpolant's value** (#1298). `v_uv` is read exactly once more
    // in this file, through a `floor` with a half-fragment margin, and never
    // again as a number. `gl_FragCoord` cannot stand in: the screen pass
    // narrows the viewport to the letterbox fit rect, whose origin this file
    // has no uniform for.
    vec2 vp = vec2(max(u_viewport.x, 1), max(u_viewport.y, 1));
    vec2 fi = clamp(floor(v_uv * vp), vec2(0.0), vp - 1.0);
    // …the same fragment as a **top-down** device-pixel centre, which is the
    // kit's row order (`gl_FragCoord`/`v_uv` run bottom-up, and this is the one
    // flip in the pipeline).
    vec2 px = vec2(fi.x, vp.y - 1.0 - fi.y) + 0.5;
    // …and in buffer units, by **one multiply** against a uniform the CPU
    // divided correctly (#1298).
    vec2 pc = px * u_px_step;
    int col = clamp(int(pc.x), 0, cols - 1);
    int row = clamp(int(pc.y), 0, rows - 1);

    bool snapped = (u_viewport == u_grid);
    vec2 p = snapped ? vec2(float(col), float(row)) + 0.5 : pc;
    // The fragment's footprint in buffer units, which is the same step — the
    // device->buffer ratio *is* how much buffer one fragment covers. Exactly
    // `1` on the snapped branch by construction rather than by a division that
    // happens to land there.
    float fstep = snapped ? 1.0 : u_px_step.y;

    ivec4 bg = ivec4(u_bg + 0.5);
    ivec4 ink = ivec4(u_ink + 0.5);

    // The fixture, painted flat under everything so it never picks up the lit
    // layer's bloom — the kit paints it into the frame before the emission
    // exists, for exactly that reason. It is a `set` there rather than a
    // composite; a `t` of 255 through `mix_kit` is that same `set` to the byte.
    ivec4 under = bg;
    if (u_ghost_on != 0) {
        int tone = fixture255(col, row);
        if (tone > 0) {
            under = mix_kit(bg, ivec4(u_ghost + 0.5), tone);
        }
    }

    // `Emission::bloom`, twice: the nixie's wide haze and then the skin's own,
    // each max-combined under the original. The reads are gated exactly the way
    // the sample point is (#1186) — a single `texelFetch` on the snapped
    // branch, which is what keeps the 1:1 cases bit-exact, and the bilinear tap
    // on the continuous one.
    int lit = board255(col, row, p, fstep, snapped);
    int haze = snapped ? texel(u_tex1, ivec2(col, row)) : halo_at(u_tex1, pc);
    lit = min(max(lit, min(haze * u_halo_strength / 256, 255)), 255);
    int glow = snapped ? texel(u_tex2, ivec2(col, row)) : halo_at(u_tex2, pc);
    lit = min(max(lit, min(glow * u_bloom_strength / 256, 255)), 255);

    // `Emission::composite`: unlit pixels are skipped *before* the mask is
    // consulted, which is why an unlit screen shows no scanlines. The mask is
    // evaluated at the buffer coordinate with no rescaling — the kit composites
    // over the pre-upscale buffer, which is the one drawn here.
    if (lit > 0) {
        if (u_mask_on != 0) {
            lit = lit * mask_keep(col, row, cols, rows) / MASK_ONE;
        }
        if (lit > 0) {
            under = mix_kit(under, ink, lit);
        }
    }

    // The hinge slots, cut **last**, over the finished composite: they are gaps
    // in the fixture, so no glow crosses them on any skin. A nixie has none.
    if (u_mechanism == MECH_SPLIT_FLAP && row == u_bezel + u_hinge && cell_of(col) >= 0) {
        under = bg;
    }

    // Opaque: a kit frame is a screen, never a sprite. The letterbox padding
    // around the fit rect is the transparent part, and the host clears it.
    o_colour = vec4(vec3(under.rgb) / 255.0, 1.0);
}
