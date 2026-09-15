// `LedMatrix::render` — the ghost grid, the lit lamps at their own
// brightnesses, the halo and the composite, in **one pass over one 1-D data
// strip** (#1156).
//
// # The lattice is the meter's, not the dot matrix's — measured, not assumed
//
// #1156 asks whether this can reuse #1144's `dot_matrix` shader with a per-LED
// brightness texture "if the geometry matches". It does not. A dot-matrix cell
// is a `dot_px`-pitch **disc** carrying a radial falloff table, its bezel is one
// dot cell derived from that pitch, its lattice is grouped into 5x7 character
// cells with an undotted spacing column between them, and every lit dot is the
// same `CORE` intensity modulated only by a font *bit*. A panel lamp is a flat
// `LED_MATRIX_CELL`-px **square** on a fixed `CELL + GAP` pitch inside a fixed
// `PAD` bezel, carrying its own `0..=255` analog brightness and — uniquely in
// the kit — its own ink. Not one of those five is shared.
//
// What *is* shared is `led_strip.frag`'s (#1153): the emission is a union of
// axis-aligned rectangles, which is exactly the shape whose separable box blur
// has a closed form. So this is the meter's pipeline in two dimensions — one
// pass, `aux: 0`, no blur target and no read-back — with the two things a panel
// has that a meter does not (a per-lamp intensity and a per-lamp ink) arriving
// as the four floats per slot of `GlUniforms::data`.
//
// # The kit's lamps are squares, not discs — and that is load-bearing here
//
// #1156's parent asks for "round LEDs". The kit does not draw them:
// `hytte-preem/src/led_matrix.rs`'s `stamp_cell` fills a solid `CELL`x`CELL`
// square at one intensity, and the same issue asks for the 1:1 comparison to be
// pinned **bit-exact** against that kit. Those two asks are not jointly
// satisfiable — a disc and a square disagree at the first corner pixel — so
// what this shader draws is the kit's own lamp, and what it evaluates at the
// fragment's resolution is that lamp's *edge* and its *halo*. This is the
// second time the question has come up and it is answered the same way
// (`led_strip.frag`'s header): making the kit's lamps round is a change to
// `hytte-preem` and a decision for the issue thread, not something a renderer
// gets to make unilaterally while claiming parity with the renderer it differs
// from.
//
// # The improvement, stated as what actually moves
//
// The CPU arm rasterises into a `Frame` at the panel's own small buffer size
// (181x49 px for 64 cores in the shipping wide shape) and the shell blows that
// up with `PixelSurface`'s nearest-neighbour scaling — `core_panel_scale`
// picks 1x to 5x depending on the core count. Two things are replicated
// logical pixels there:
//
//   * the **lamp edge**, so at 2x every lamp is a 16 px block of four copies
//     of each kit pixel and the gutter between two lamps is a hard 6 px step;
//   * the **halo**, which is a box blur of the emission grid and therefore a
//     grid-resolution staircase however much room the card gives the panel —
//     the most visible of the two, since the VFD skin the panel ships on is
//     mostly halo.
//
// Here both are continuous functions of the fragment's own position: the lamp
// is a box-filter *coverage* (the exact area of the fragment's footprint inside
// the lamp square, weighted by that lamp's own brightness), and the halo is the
// kit's own separable box blur solved in closed form and evaluated *at* the
// fragment. Nothing is interpolated and nothing is read back.
//
// # Why it is still bit-exact at 1:1
//
// Every one of the kit's boundaries — `PAD`, each `cell_x0(c)`/`cell_y0(r)` and
// their `+ CELL` — is an **integer** buffer coordinate, and the blur window's
// half-width `(2r + 1) / 2` is a half-integer. So at a pixel centre the
// footprint `[x, x+1] x [y, y+1]` lies wholly inside or wholly outside a lamp
// (coverage is exactly 0 or 1, never a fraction, and the weighted sum collapses
// onto that one lamp's own integer amount), and the blur's measures come out as
// exactly the integer column and row *counts* the kit sums. The two `floor`s
// below are then the kit's own two truncating integer divisions, taken over
// numerators under 1786 — exact in `f32`. The float path *is* the integer path
// there, the way `led_strip.frag`'s is.
//
// `u_viewport == u_grid` snaps the sample to that pixel centre rather than
// trusting a `v_uv` round trip, exactly as the meter and the dot matrix do, and
// with the same caveat: deleting the snap moves no pixel under llvmpipe,
// because the arithmetic above does not need it. It is insurance against a
// driver whose `v_uv` interpolation is not exact at a pixel centre. Keep it; do
// not read a green harness as evidence for it.
//
// Every composite step is **integer**, for the reason `scope_blit.frag` gives
// at length: the kit's `mix` is `(a * (255 - t) + b * t + 127) / 255`, so a
// float composite would differ in the last bit almost everywhere.

// hytte-preem/src/style.rs — the CRT pass, verbatim from `led_strip.frag`.
const int MASK_ONE = 256;
const int COORD_ONE = 1024;
const int BAND_DIV = 9;
const int CORNER_DIV = 6;

// How many lamps one interval may straddle, on one axis, before the coverage
// and halo integrals start dropping material.
//
// A bound is required — GLSL needs the loops to terminate, and the *true* bound
// is the column (or row) count, which is a uniform. It never binds in any
// configuration anyone draws: the blur window is at most `2*3 + 1 = 7` buffer
// px against a lamp pitch of `CELL + GAP = 11`, so the halo integral touches
// two lamps per axis, and a fragment's footprint is one buffer pixel at natural
// size and *smaller* above it (the panel is only ever upscaled — see
// `core_panel_scale`). It could only bind on a panel a layout has squeezed
// below about 1/88 of its natural width, where one fragment covers eight whole
// lamps and there is no panel left to read either way.
const int MAX_SPAN_CELLS = 8;

// Floats per slot in the data strip: the lamp's `0..=255` brightness, then its
// ink's three channels. See `led_matrix.rs`'s `Lamps`.
const int LAMP_STRIDE = 4;

in vec2 v_uv;

uniform ivec2 u_grid;       // the buffer, which for this widget is the kit's
                            // own pre-upscale `Frame` — the panel's size knob
                            // is its lamp count, and the shell's integer
                            // `core_panel_scale` is what blows it up
uniform ivec2 u_viewport;   // the pass's viewport — equal to the grid at 1:1
uniform ivec2 u_cells;      // (cols, rows) — `LedMatrix::cols`/`rows`, already
                            // past the kit's `max(1)` clamp
uniform int u_ghost_slots;  // slots the ghost pass paints, row-major from the
                            // top-left — `LedMatrix::ghost_slots`, which is
                            // where `Fill::Spare` vs `Fill::Blank` is decided
uniform int u_cell;         // `hytte_preem::LED_MATRIX_CELL` …
uniform int u_gap;          // … `LED_MATRIX_GAP` …
uniform int u_pad;          // … and `LED_MATRIX_PAD`, all read off the kit
                            // rather than re-declared here, since a uniform can
                            // carry a Rust `const` where a GLSL literal cannot
uniform int u_ghost_on;     // 0 on a skin with no unlit grid (OLED, CRT)
uniform vec4 u_ghost;       // the unlit lamp colour, channels as 0..255
uniform int u_bloom_radius; // box-blur radius in buffer px; 0 = no bloom
uniform int u_bloom_strength;   // halo strength in 256ths; 0 = no bloom
uniform vec4 u_bg;          // the skin's field, channels as 0..255
uniform int u_mask_on;
uniform int u_mask_pitch;   // the CRT comb, **not** re-phased: `LedMatrix`
uniform int u_mask_phase;   // hands `Emission::composite` the skin's own
uniform int u_scanline_keep;    // `palette.mask` untouched, exactly as the
uniform int u_corner_keep;      // meter does (#1091 lists it under that)

uniform sampler2D u_tex0;   // the 1-D R32F lamp strip: `LAMP_STRIDE` texels per
                            // slot, row-major
uniform int u_data_len;     // texels in the strip

out vec4 o_colour;

// ── the lamp strip (`led_matrix.rs`'s `lamps`) ──────────────────────────────

// One texel of the strip as an integer `0..=255`, or 0 past its end.
int strip(int index) {
    if (index < 0 || index >= u_data_len) {
        return 0;
    }
    return int(texelFetch(u_tex0, ivec2(index, 0), 0).r + 0.5);
}

// The `Emission` amount of the lamp at (`c`, `r`) — `LedMatrix::lamp_intensities`
// read back, so a slot past the fed levels is 0 because the kit padded it with
// one, not because this decided to.
int amount_at(int c, int r) {
    if (c < 0 || r < 0 || c >= u_cells.x || r >= u_cells.y) {
        return 0;
    }
    return strip((r * u_cells.x + c) * LAMP_STRIDE);
}

// The ink of the lamp at (`c`, `r`), clamped onto the grid —
// `LedMatrix::lamp_inks`, which has already baked in both the colour axis and
// the spare-slot clamp, so this is a plain fetch and not a second copy of
// either rule. Alpha is 255: a kit frame is a screen, and `mix_kit`'s alpha
// channel never reaches `o_colour`.
ivec4 ink_at(int c, int r) {
    int i = clamp(r, 0, u_cells.y - 1) * u_cells.x + clamp(c, 0, u_cells.x - 1);
    int at = i * LAMP_STRIDE;
    return ivec4(strip(at + 1), strip(at + 2), strip(at + 3), 255);
}

// ── the lamp lattice (`hytte-preem/src/led_matrix.rs`) ──────────────────────

// Length of `[lo, hi] ∩ [a, b]`, never negative.
float overlap(float lo, float hi, float a, float b) {
    return max(min(hi, b) - max(lo, a), 0.0);
}

// The lattice pitch: `CELL + GAP`, which `cell_x0`/`cell_y0` both advance by.
float advance() {
    return float(u_cell + u_gap);
}

// The first lattice index `[lo, hi]` can touch on one axis, floored the kit's
// way — `cell_x0(i) = PAD + i * (CELL + GAP)` inverted.
int first_index(float lo) {
    return int(floor((lo - float(u_pad)) / advance()));
}

// How much of `[lo, hi]` lies inside lamp `i`'s square on one axis.
float cell_overlap(float lo, float hi, int i) {
    float x0 = float(u_pad) + float(i) * advance();
    return overlap(lo, hi, x0, x0 + float(u_cell));
}

// Which lamp column (or row) the buffer **pixel** containing `pos` belongs to —
// `index_table`'s arithmetic, which attributes the bezel to the first/last lamp
// and a gutter to the lamp on its left/top, so a halo in the gutter takes the
// colour of the lamp it spilled off. Integer, at the pixel, deliberately: this
// is the one grid-resolution quantity in the widget, exactly as the CRT mask is.
int index_at(float pos, int cells) {
    int p = int(floor(pos));
    return min(max(p - u_pad, 0) / (u_cell + u_gap), cells - 1);
}

// ── the emission ────────────────────────────────────────────────────────────

// The emission a fragment's own footprint sees: every lamp's `0..=255` amount
// weighted by the fraction of the footprint inside it, summed.
//
// A box filter rather than a point sample, which is the lamp half of #1156's
// improvement. At 1:1 (and at any integer upscale, where the footprint still
// has integer edges against the kit's integer lamp bounds) exactly one lamp has
// weight 1 and the rest have 0, so this is that lamp's own amount and the pin
// holds; between them it is the true area-weighted average, so a lamp a layout
// gave a fractional width keeps its proportions instead of gaining a replicated
// column.
int stamp255(vec2 p, vec2 fp) {
    float lo_x = p.x - fp.x * 0.5;
    float hi_x = p.x + fp.x * 0.5;
    float lo_y = p.y - fp.y * 0.5;
    float hi_y = p.y + fp.y * 0.5;
    int first_c = max(first_index(lo_x), 0);
    int last_c = min(first_index(hi_x), u_cells.x - 1);
    int first_r = max(first_index(lo_y), 0);
    int last_r = min(first_index(hi_y), u_cells.y - 1);
    float sum = 0.0;
    for (int r = first_r; r <= last_r && r - first_r < MAX_SPAN_CELLS; ++r) {
        float oy = cell_overlap(lo_y, hi_y, r);
        if (oy <= 0.0) {
            continue;
        }
        for (int c = first_c; c <= last_c && c - first_c < MAX_SPAN_CELLS; ++c) {
            sum += float(amount_at(c, r)) * cell_overlap(lo_x, hi_x, c) * oy;
        }
    }
    return int(sum / (fp.x * fp.y) + 0.5);
}

// The **ghost** coverage: the same footprint integral over the slots
// `LedMatrix::ghost_slots` says are painted, unweighted — the unlit grid is one
// flat colour, so what varies is only how much of the footprint is on hardware.
int ghost255(vec2 p, vec2 fp) {
    float lo_x = p.x - fp.x * 0.5;
    float hi_x = p.x + fp.x * 0.5;
    float lo_y = p.y - fp.y * 0.5;
    float hi_y = p.y + fp.y * 0.5;
    int first_c = max(first_index(lo_x), 0);
    int last_c = min(first_index(hi_x), u_cells.x - 1);
    int first_r = max(first_index(lo_y), 0);
    int last_r = min(first_index(hi_y), u_cells.y - 1);
    float sum = 0.0;
    for (int r = first_r; r <= last_r && r - first_r < MAX_SPAN_CELLS; ++r) {
        float oy = cell_overlap(lo_y, hi_y, r);
        if (oy <= 0.0) {
            continue;
        }
        for (int c = first_c; c <= last_c && c - first_c < MAX_SPAN_CELLS; ++c) {
            if (r * u_cells.x + c >= u_ghost_slots) {
                continue;
            }
            sum += cell_overlap(lo_x, hi_x, c) * oy;
        }
    }
    return int(255.0 * sum / (fp.x * fp.y) + 0.5);
}

// `Emission::bloom` (`hytte-preem/src/style.rs`) in closed form.
//
// The kit blurs the emission grid separably and **truncatingly**: a horizontal
// pass `tmp[y][x] = sum(src[y][x-r ..= x+r]) / (2r + 1)` then a vertical one
// over `tmp`, each dividing by the full window even where it clips at a buffer
// edge. For this widget `src` is piecewise constant on a lattice of rectangles,
// so for a `y` inside row `r`'s band the horizontal sum is
// `sum_c amount(c, r) * (columns of lamp c inside the clipped window)` — a
// measure, not a sum over samples — and `tmp` is then **constant in `y`** over
// that band. The vertical pass therefore reduces to one term per row band:
// `tmp(r) * (rows of band r inside the clipped window)`. Two nested integrals
// and two floors, with the kit's clipping and its unrenormalised divisor.
//
// The per-row `floor` is inside the row loop rather than outside it, and that
// is the kit's order, not a convenience: `tmp` is a `u16` grid, so the
// horizontal pass truncates **once per row** before the vertical pass ever
// sees it.
//
// A radius or a strength of 0 returns 0, matching `Emission::bloom`'s own early
// return rather than relying on `max(v, v * strength / 256)` absorbing it.
int halo255(vec2 p) {
    if (u_bloom_radius <= 0 || u_bloom_strength <= 0) {
        return 0;
    }
    float win = float(2 * u_bloom_radius + 1);
    float half_win = win * 0.5;
    // The window clips at the buffer, exactly as the kit's index clamps do;
    // the divisor stays the full window, which is what dims the kit's edges.
    float lo_x = max(p.x - half_win, 0.0);
    float hi_x = min(p.x + half_win, float(u_grid.x));
    float lo_y = max(p.y - half_win, 0.0);
    float hi_y = min(p.y + half_win, float(u_grid.y));
    int first_c = max(first_index(lo_x), 0);
    int last_c = min(first_index(hi_x), u_cells.x - 1);
    int first_r = max(first_index(lo_y), 0);
    int last_r = min(first_index(hi_y), u_cells.y - 1);
    float blurred = 0.0;
    for (int r = first_r; r <= last_r && r - first_r < MAX_SPAN_CELLS; ++r) {
        float rows = cell_overlap(lo_y, hi_y, r);
        if (rows <= 0.0) {
            continue;
        }
        float row_sum = 0.0;
        for (int c = first_c; c <= last_c && c - first_c < MAX_SPAN_CELLS; ++c) {
            row_sum += float(amount_at(c, r)) * cell_overlap(lo_x, hi_x, c);
        }
        blurred += floor(row_sum / win) * rows;
    }
    int out255 = int(floor(blurred / win));
    return min(out255 * u_bloom_strength / 256, 255);
}

// ── the composite (verbatim from `led_strip.frag`) ──────────────────────────

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
    int cols = u_grid.x;
    int rows = u_grid.y;
    // Point sampling into the letterboxed fit rect, as `led_strip.frag` does it
    // — used for the CRT pass, which is a grid-resolution quantity by definition
    // (`MaskRow` is a row of the *buffer*), and for the snapped branch's sample
    // point below.
    int col = clamp(int(v_uv.x * float(cols)), 0, cols - 1);
    int row = clamp(int((1.0 - v_uv.y) * float(rows)), 0, rows - 1);

    bool snapped = (u_viewport == u_grid);
    vec2 p = snapped
        ? vec2(float(col), float(row)) + 0.5
        : vec2(v_uv.x * float(cols), (1.0 - v_uv.y) * float(rows));
    // The fragment's footprint in buffer units. Exactly `(1, 1)` on the snapped
    // branch, by construction rather than by a division that happens to land
    // there — see the header on why that is what makes the coverage integral
    // collapse onto the kit's integer stamp.
    vec2 fp = snapped
        ? vec2(1.0)
        : vec2(float(cols), float(rows)) / vec2(max(u_viewport.x, 1), max(u_viewport.y, 1));

    ivec4 bg = ivec4(u_bg + 0.5);

    // Ghost grid: the unlit lamps show through on ghosting skins, painted
    // **flat** under everything so they never pick up the lit layer's bloom —
    // the kit paints them into the frame before the emission exists, for
    // exactly that reason. It is a `set` there rather than a composite; a `t`
    // of 255 through `mix_kit` is that same `set` to the byte, and a `t` of 0
    // is the field.
    ivec4 under = bg;
    if (u_ghost_on != 0) {
        int ghost = ghost255(p, fp);
        if (ghost > 0) {
            under = mix_kit(bg, ivec4(u_ghost + 0.5), ghost);
        }
    }

    // One `Emission` layer — unlike the meter, a panel has no second pass:
    // the stamp with its halo max-combined under it, the kit's "lit pixels
    // never dim, dark neighbours pick up spill".
    int lit = min(max(stamp255(p, fp), halo255(p)), 255);
    if (lit > 0) {
        if (u_mask_on != 0) {
            lit = lit * mask_keep(col, row, cols, rows) / MASK_ONE;
        }
        if (lit > 0) {
            // The ink is resolved at the **buffer pixel**, not at `p`: the kit
            // reads it out of `index_table`, a per-pixel table, and a halo in
            // the gutter takes the colour of the lamp it came off.
            under = mix_kit(under, ink_at(index_at(p.x, u_cells.x), index_at(p.y, u_cells.y)), lit);
        }
    }

    // Opaque: a kit frame is a screen, never a sprite. The letterbox padding
    // around the fit rect is the transparent part, and the host clears it.
    o_colour = vec4(vec3(under.rgb) / 255.0, 1.0);
}
