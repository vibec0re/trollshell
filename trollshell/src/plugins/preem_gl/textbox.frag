// `TextBox::render` (#1152) — the rounded opaque field and the 5×7 pixel-font
// glyphs on it, as **one pass**.
//
// The simplest pipeline on this seam, and the only one with a single fragment
// program: a textbox has no emission at all. The kit paints a field, cuts its
// corners to transparent and stamps glyph pixels over it — no falloff, no
// bloom, no CRT comb — so there is nothing to blur, nothing to accumulate and
// no layer to splice. `LAYER` does not exist here.
//
// # What is better than the kit, and what deliberately is not
//
// **The corner.** The kit decides its rounded cut on the *pre-scale* buffer
// (`dx² + dy² ≤ corner²` over integer pokes into the margin) and then
// replicates each logical pixel `scale` times, so a `scale = 2` bubble has a
// corner built out of 2×2 blocks and a stretched one has it built out of
// whatever GSK's nearest-neighbour scaling makes. Here the same expression is
// evaluated at the **fragment's own** position on the logical lattice, so the
// arc is as round as the screen can draw it. That is #865's "could be even
// better", on the one part of this widget that has a curve in it.
//
// **The glyphs, deliberately not.** They are 5×7 bitmap pixels and the whole
// look is that they are square: the fragment's logical position is *floored* to
// a logical pixel before the strip is consulted (`texelFetch`, no filtering),
// so a stretched textbox gets bigger hard-edged pixels rather than a smoothed
// font. Smoothing them would be a different widget.
//
// # At 1:1 the two arms are the same arithmetic
//
// `u_viewport == u_grid` — the same test `dot_matrix.frag` uses, and the same
// caveat applies to it (see that file: `u_viewport` is the allocation in
// **device** pixels, so the snap fires only on a scale-1 display at the natural
// size). When it fires, the sample is snapped to the **logical** pixel centre:
//
//     q = floor(pixel_centre / u_upscale) + 0.5
//
// so `q` is exactly `logical + 0.5`, the corner's `fx`/`fy` come out as the
// integers the kit's `corner_delta` returns, and `floor(q)` is the logical
// pixel the kit blitted. The float path *is* the integer path, at any upscale.
//
// The arithmetic behind that: the buffer is bounded by `MAX_BUFFER_DIM` (2048)
// and `u_upscale` by `MAX_SCALE` (8), so `(2*col + 1) / (2*upscale)` has an odd
// numerator and can never be within `1/16` of an integer, against an `f32` ulp
// of `~1e-4` at these magnitudes. `fx` and `fy` are then integers `≤ MAX_CORNER`
// (64) and their squares are exact.
//
// Every color is **integer** bytes divided by 255 at the very end, for the
// reason `scope_blit.frag` gives: there is no mixing here at all (the kit
// `set`s a flat color, it does not composite), so the only way to disagree with
// it is to arrive at a different byte.

// ── hytte-preem/src/font.rs — the 5×7 cell, its one-column gap and the gap
// between wrapped lines. The kit's own public constants;
// `the_shader_and_the_mapping_agree_about_the_font_metrics` reads these four
// back out of this file so the two cannot drift.
const int GLYPH_W = 5;
const int GLYPH_H = 7;
const int SPACING = 1;
const int LINE_GAP = 2;

// Bit 7 of a strip texel: this cell is an **uncovered** char, so its set pixels
// take `u_notdef` rather than `u_ink`. Bits 0..=6 are the glyph's own rows, so
// 7 is the first one free — see `textbox.rs`'s `glyphs`.
const int NOTDEF_BIT = 7;

in vec2 v_uv;

uniform sampler2D u_tex0;   // the 1-D R32F glyph strip: one texel per glyph
                            // column of the laid-out block, row-major by line
uniform int u_data_len;     // texels in the strip (0 = nothing to draw)
uniform ivec2 u_grid;       // the **final** buffer, i.e. logical × u_upscale
uniform ivec2 u_viewport;   // the pass's viewport — equal to the grid at 1:1
uniform ivec2 u_logical;    // the pre-scale buffer the kit lays glyphs out on
uniform int u_upscale;      // `TextBox::scale`, already clamped (≥ 1)
uniform int u_pad;          // field padding in pre-scale px
uniform int u_corner;       // rounded-corner cut radius in pre-scale px
uniform int u_cols;         // glyph cells across the drawn block
uniform int u_lines;        // wrapped lines
uniform vec4 u_bg;          // the field, channels as 0..255
uniform vec4 u_ink;         // the text ink, channels as 0..255
uniform vec4 u_notdef;      // the hollow `.notdef` box's color, channels as 0..255

out vec4 o_colour;

// `textbox.rs`'s `corner_delta`, continuous: how far `v` pokes into the
// `corner` margin at either end of a span that is `n` **pre-scale** pixels
// wide, `0` in the middle.
//
// The ±0.5 shifts are what make this the kit's own integer function at a pixel
// centre. The kit asks `corner.saturating_sub(x)` and
// `(x + corner).saturating_sub(n - 1)` of an integer `x`; substituting
// `v = x + 0.5` here gives `corner - x` and `x + corner - (n - 1)` exactly.
float poke(float v, float n, float corner) {
    return max(max(corner + 0.5 - v, v - (n - 0.5 - corner)), 0.0);
}

// Is this glyph pixel set, and is its cell an uncovered char? `x` is the bit,
// `y` the notdef flag. The strip carries one texel per glyph column of the
// laid-out block — `(line * u_cols + cell) * GLYPH_W + col` — holding that
// column's GLYPH_H row bits plus `NOTDEF_BIT`; see `textbox.rs`'s `glyphs`,
// which builds it from `hytte_preem::font::glyph` (an uncovered char already
// resolved to the hollow `NOTDEF` box there, and flagged so it can be dimmed).
bvec2 glyph_pixel(int line, int cell, int col, int row) {
    int index = (line * u_cols + cell) * GLYPH_W + col;
    if (index < 0 || index >= u_data_len) {
        return bvec2(false, false);
    }
    int bits = int(texelFetch(u_tex0, ivec2(index, 0), 0).r + 0.5);
    return bvec2(((bits >> row) & 1) == 1, ((bits >> NOTDEF_BIT) & 1) == 1);
}

void main() {
    int gw = u_grid.x;
    int gh = u_grid.y;
    int col = clamp(int(v_uv.x * float(gw)), 0, gw - 1);
    int row = clamp(int((1.0 - v_uv.y) * float(gh)), 0, gh - 1);

    // The fragment's position in the **final** buffer, then on the logical
    // lattice the kit laid the box out on. See the header for which branch a
    // real screen takes and what each one is tested by.
    vec2 p = vec2(v_uv.x * float(gw), (1.0 - v_uv.y) * float(gh));
    if (u_viewport == u_grid) {
        p = vec2(float(col), float(row)) + 0.5;
    }
    vec2 q = p / float(max(u_upscale, 1));
    if (u_viewport == u_grid) {
        // The logical pixel *centre*, which is where the kit sampled.
        q = floor(q) + 0.5;
    }

    // The field, cut to transparent at the corners. `u_corner == 0` leaves
    // `poke` at `0` everywhere and `0 <= 0` holds, so a square box is the same
    // expression rather than a branch — exactly as the kit's is.
    float corner = float(u_corner);
    float fx = poke(q.x, float(u_logical.x), corner);
    float fy = poke(q.y, float(u_logical.y), corner);
    // Outside the cut the kit's buffer keeps `Frame::new`'s zeros, which are
    // transparent *black* and not a transparent field color — a host that
    // ignored alpha would see black rather than the bubble's ground, so the
    // bytes have to be zero and not `u_bg` with a zero alpha.
    vec4 out_px = vec4(0.0, 0.0, 0.0, 0.0);
    if (fx * fx + fy * fy <= corner * corner) {
        out_px = u_bg;
    }

    // The glyph block. Floored to a logical pixel, never interpolated — the
    // 8-bit look is the point. The kit stamps glyphs *after* the field and does
    // not consult the corner cut, so a glyph pixel wins wherever it lands
    // (reachable at `pad = 0` with a non-zero corner).
    ivec2 cell_px = ivec2(floor(q)) - ivec2(u_pad);
    if (cell_px.x >= 0 && cell_px.y >= 0 && u_cols > 0) {
        int line = cell_px.y / (GLYPH_H + LINE_GAP);
        int gy = cell_px.y - line * (GLYPH_H + LINE_GAP);
        int cell = cell_px.x / (GLYPH_W + SPACING);
        int gx = cell_px.x - cell * (GLYPH_W + SPACING);
        if (line < u_lines && cell < u_cols && gy < GLYPH_H && gx < GLYPH_W) {
            bvec2 pixel = glyph_pixel(line, cell, gx, gy);
            if (pixel.x) {
                out_px = pixel.y ? u_notdef : u_ink;
            }
        }
    }

    // The kit writes whole RGBA bytes with no compositing at all, so this is a
    // divide and nothing else — including the alpha, which carries the corner
    // cut and, on a plugin that pins one, a translucent `.notdef`.
    o_colour = out_px / 255.0;
}
