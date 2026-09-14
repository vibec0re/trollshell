// `LedStrip::render` — the ghost row, the lit segments, the peak-hold dot and
// both composites, in **one pass with no texture at all** (#1153).
//
// # The kit's LEDs are bars, not discs — and that is load-bearing here
//
// #1153 asks for "N round LEDs as SDF discs". The kit does not draw discs:
// `hytte-preem/src/led_strip.rs`'s `stamp_cell` fills a solid
// `CELL_W`×`CELL_H` = 8×16 rectangle per segment, which is the VU-meter bar a
// peak/level strip has always been. The same issue asks for the 1:1 comparison
// to be pinned **bit-exact** against that kit, and those two asks cannot both
// be met: a disc and a bar disagree at the first corner pixel. So what this
// shader draws is the kit's own segment, and what it evaluates at the
// fragment's resolution is that segment's *edge* and — the bigger half — its
// whole halo. Whether the kit's segments should become discs is a change to
// `hytte-preem` and a decision for the issue thread, not something a renderer
// gets to make unilaterally while claiming parity with the renderer it differs
// from.
//
// # The improvement, stated as what actually moves
//
// The CPU arm rasterises into a `Frame` at the buffer's own resolution and the
// shell blows that up with `PixelSurface`'s nearest-neighbour scaling. Two
// things are replicated logical pixels there:
//
//   * the **segment edge**, which on a chip a layout gave a width that is not a
//     whole multiple of the buffer's makes some LEDs a pixel wider than others;
//   * the **halo**, which is a box blur of the emission grid and therefore a
//     grid-resolution staircase however much room the chip is given.
//
// Here both are continuous functions of the fragment's own position. The
// segment is a box-filter *coverage* — the exact area of the fragment's
// footprint inside the segment rectangle, which is the same thing an SDF
// thresholded at half a footprint would give for an axis-aligned box, written
// as the area because the area is exact rather than approximately antialiased.
// The halo is the kit's own separable box blur **solved in closed form**: for a
// union of axis-aligned rectangles the blur's two passes are one integral each,
// so there is no aux texture, no blur pass and no bilinear read-back — the
// halo's value at a fragment is computed *at* that fragment. That is why this
// pipeline declares `aux: 0` and one frame pass where `dot_matrix`'s declares
// three and four.
//
// # Why it is still bit-exact at 1:1
//
// Every one of the kit's boundaries — `PAD`, `PAD + CELL_H`, each `cell_x0(i)`
// and `cell_x0(i) + CELL_W` — is an **integer** buffer coordinate, and the blur
// window's half-width `(2r + 1) / 2` is a half-integer. So at a pixel centre
// the footprint `[x, x+1]` lies wholly inside or wholly outside a segment
// (coverage is exactly 0 or 1, never a fraction), and the blur's two measures
// `L` and `M` come out as exactly the integer column and row *counts* the kit
// sums. The two `floor`s below are then the kit's own two truncating integer
// divisions, taken over numerators under 1786 — exact in `f32`, and never
// within `1/7` of an integer boundary when the quotient is not itself exact.
// The float path *is* the integer path there, the way `dot_matrix.frag`'s
// `site_at` is at a pixel centre.
//
// `u_viewport == u_grid` snaps the sample to that pixel centre rather than
// trusting a `v_uv` round trip, exactly as the dot matrix does, and with the
// same caveat: deleting the snap moves no pixel under llvmpipe, because the
// arithmetic above does not need it. It is insurance against a driver whose
// `v_uv` interpolation is not exact at a pixel centre — a real risk on hardware
// this has never run on, and one no gate here can see. Keep it; do not read a
// green harness as evidence for it.
//
// Every composite step is **integer**, for the reason `scope_blit.frag` gives
// at length: the kit's `mix` is `(a * (255 - t) + b * t + 127) / 255`, so a
// float composite would differ in the last bit almost everywhere.

// hytte-preem/src/style.rs — the CRT pass, verbatim from `scope_blit.frag`.
const int MASK_ONE = 256;
const int COORD_ONE = 1024;
const int BAND_DIV = 9;
const int CORNER_DIV = 6;

// How many segments one fragment's footprint may straddle before the coverage
// integral starts dropping material.
//
// A bound is required — GLSL needs the loop to terminate, and the *true* bound
// is the segment count, which is a uniform. It never binds in any
// configuration anyone draws: the blur window is at most `2*3 + 1 = 7` buffer
// px against a segment pitch of `CELL_W + GAP = 11`, so the halo integral
// touches two cells, and a fragment's footprint is one buffer pixel at natural
// size and *smaller* above it. It could only bind on a chip CSS has squeezed
// below about 1/88 of its natural width, where one fragment covers eight whole
// segments — at which point the strip is a couple of pixels wide and there is
// no meter left to read either way.
const int MAX_SPAN_CELLS = 8;

in vec2 v_uv;

uniform ivec2 u_grid;       // the buffer, which for this widget *is* native:
                            // there is no upscale, the segment metrics are the
                            // size knob (the dot matrix's situation, #1091)
uniform ivec2 u_viewport;   // the pass's viewport — equal to the grid at 1:1
uniform int u_leds;         // segments on the strip; the ghost row is all of them
uniform int u_lit;          // segments the level lights — `kit::lit_count`
uniform int u_peak;         // the peak dot's segment index, or -1 for no dot —
                            // `kit::peak_led`, which is where "a rested, negative
                            // or NaN peak marks nothing" is decided
uniform int u_cell_w;       // `hytte_preem::LED_CELL_W` …
uniform int u_cell_h;       // … `LED_CELL_H` …
uniform int u_gap;          // … `LED_GAP` …
uniform int u_pad;          // … and `LED_PAD`, all read off the kit rather than
                            // re-declared here, since a uniform can carry a Rust
                            // `const` where a GLSL literal cannot
uniform int u_ghost_on;     // 0 on a skin with no unlit row (OLED, CRT)
uniform vec4 u_ghost;       // the unlit segment colour, channels as 0..255
uniform int u_bloom_radius; // box-blur radius in buffer px; 0 = no bloom
uniform int u_bloom_strength;   // halo strength in 256ths; 0 = no bloom
uniform vec4 u_bg;          // the skin's field, channels as 0..255
uniform vec4 u_ink;         // the skin's lit ink
uniform vec4 u_cap;         // the peak dot's cap — `kit::led_cap_ink(ink)`
uniform int u_mask_on;
uniform int u_mask_pitch;   // the CRT comb, **not** re-phased: unlike the two dot
uniform int u_mask_phase;   // surfaces this widget has no dot grid, so it keeps
uniform int u_scanline_keep;    // `Mask::CRT` as it stands (#1091 says so by name)
uniform int u_corner_keep;

out vec4 o_colour;

// ── the segment geometry (`hytte-preem/src/led_strip.rs`) ───────────────────

// Length of `[lo, hi] ∩ [a, b]`, never negative.
float overlap(float lo, float hi, float a, float b) {
    return max(min(hi, b) - max(lo, a), 0.0);
}

// How much of `[lo, hi]` lies inside segments `from ..= to` — the kit's
// `cell_x0(i) = PAD + i * (CELL_W + GAP)` inverted to find which segments the
// interval can touch at all, then summed.
//
// The level layer passes `(0, u_lit - 1)`, the ghost row `(0, u_leds - 1)` and
// the peak dot `(u_peak, u_peak)`: one function, three lit sets, which is what
// keeps the peak dot's halo provably the same arithmetic as the level's rather
// than a second copy of it (the kit runs one `Emission` per layer through one
// `bloom`, for the same reason).
float span_range(float lo, float hi, int from, int to) {
    if (to < from || hi <= lo) {
        return 0.0;
    }
    float advance = float(u_cell_w + u_gap);
    int first = max(int(floor((lo - float(u_pad)) / advance)), from);
    int last = min(int(floor((hi - float(u_pad)) / advance)), to);
    float sum = 0.0;
    for (int i = first; i <= last && i - first < MAX_SPAN_CELLS; ++i) {
        float x0 = float(u_pad) + float(i) * advance;
        sum += overlap(lo, hi, x0, x0 + float(u_cell_w));
    }
    return sum;
}

// How much of `[lo, hi]` lies inside the segment row — `[PAD, PAD + CELL_H)`,
// the band every cell shares (`fill_cell`/`stamp_cell` walk exactly those rows).
float band_span(float lo, float hi) {
    return overlap(lo, hi, float(u_pad), float(u_pad + u_cell_h));
}

// The emission a fragment's own footprint sees: the fraction of the footprint
// inside segments `from ..= to`, as the kit's `0..=255` intensity.
//
// A box filter rather than a point sample, which is the segment half of #1153's
// improvement. At 1:1 (and at any integer upscale) the footprint's edges are
// integers or half-integers against the kit's integer segment bounds, so this
// is exactly 0 or 255 and the pin holds; between them it is the true area, so
// an LED stretched to a fractional width keeps its proportions instead of
// gaining a replicated column.
int stamp255(vec2 p, vec2 fp, int from, int to) {
    float sx = span_range(p.x - fp.x * 0.5, p.x + fp.x * 0.5, from, to) / fp.x;
    float sy = band_span(p.y - fp.y * 0.5, p.y + fp.y * 0.5) / fp.y;
    return int(255.0 * sx * sy + 0.5);
}

// `Emission::bloom` (`hytte-preem/src/style.rs`) in closed form.
//
// The kit blurs the emission grid separably and **truncatingly**: a horizontal
// pass `tmp[y][x] = sum(src[y][x-r ..= x+r]) / (2r + 1)` then a vertical one
// over `tmp`, each dividing by the full window even where it clips at a buffer
// edge. For this widget `src` is 255 exactly on a union of axis-aligned
// rectangles, so each sum is `255 ×` a *measure*: horizontally the lit length
// inside the clipped window, vertically the number of window rows in the
// segment band — and the whole two-pass blur is two integrals and two floors,
// with the same clipping and the same unrenormalised divisor.
//
// A radius or a strength of 0 returns 0, matching `Emission::bloom`'s own early
// return rather than relying on `max(v, v * strength / 256)` absorbing it —
// which it only does while `strength <= 256`, a caveat `scope_surface` records
// and this widget simply does not have to carry.
int halo255(vec2 p, int from, int to) {
    if (u_bloom_radius <= 0 || u_bloom_strength <= 0) {
        return 0;
    }
    float win = float(2 * u_bloom_radius + 1);
    float half_win = win * 0.5;
    // The window clips at the buffer, exactly as the kit's index clamps do;
    // the divisor stays the full window, which is what dims the kit's edges.
    float lo = max(p.x - half_win, 0.0);
    float hi = min(p.x + half_win, float(u_grid.x));
    int tmp = int(floor(255.0 * span_range(lo, hi, from, to) / win));
    float top = max(p.y - half_win, 0.0);
    float bottom = min(p.y + half_win, float(u_grid.y));
    int blurred = int(floor(float(tmp) * band_span(top, bottom) / win));
    return min(blurred * u_bloom_strength / 256, 255);
}

// One `Emission` layer: the stamp, its halo max-combined under it — the kit's
// "lit pixels never dim, dark neighbours pick up spill".
int layer255(vec2 p, vec2 fp, int from, int to) {
    if (to < from) {
        return 0;
    }
    return min(max(stamp255(p, fp, from, to), halo255(p, from, to)), 255);
}

// ── the composite (verbatim from `scope_blit.frag`) ────────────────────────

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
    // Point sampling into the letterboxed fit rect, as `scope_blit.frag` does
    // it — used for the CRT pass, which is a grid-resolution quantity by
    // definition (`MaskRow` is a row of the *buffer*), and for the snapped
    // branch's sample point below.
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
    ivec4 ink = ivec4(u_ink + 0.5);
    ivec4 cap = ivec4(u_cap + 0.5);

    // Ghost row: the unlit segments show through on ghosting skins, painted
    // **flat** under everything so they never pick up the lit layer's bloom —
    // the kit paints them into the frame before the emission exists, for
    // exactly that reason. It is a `set` there rather than a composite; a `t`
    // of 255 through `mix_kit` is that same `set` to the byte, and a `t` of 0
    // is the field.
    ivec4 under = bg;
    if (u_ghost_on != 0) {
        int ghost = stamp255(p, fp, 0, u_leds - 1);
        if (ghost > 0) {
            under = mix_kit(bg, ivec4(u_ghost + 0.5), ghost);
        }
    }

    // The CRT pass, once: the kit resolves it per `Emission::composite` call
    // over the same buffer geometry, so both layers below see one answer.
    int keep = (u_mask_on != 0) ? mask_keep(col, row, cols, rows) : MASK_ONE;

    // Level pass, then the peak dot **on top of it** — the kit composites the
    // two emissions in that order into one frame, so the dot's `under` is
    // whatever the level left, not the field.
    int level = layer255(p, fp, 0, u_lit - 1);
    if (level > 0) {
        level = level * keep / MASK_ONE;
        if (level > 0) {
            under = mix_kit(under, ink, level);
        }
    }

    if (u_peak >= 0) {
        int dot = layer255(p, fp, u_peak, u_peak);
        if (dot > 0) {
            dot = dot * keep / MASK_ONE;
            if (dot > 0) {
                // The cap is a brighter ink, not a brighter *screen*: the peak
                // dot is light on the same glass, so it takes the same pass.
                under = mix_kit(under, cap, dot);
            }
        }
    }

    // Opaque: a kit frame is a screen, never a sprite. The letterbox padding
    // around the fit rect is the transparent part, and the host clears it.
    o_colour = vec4(vec3(under.rgb) / 255.0, 1.0);
}
