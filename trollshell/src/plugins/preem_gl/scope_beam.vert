// The beam stamp — the second half of one `Scope::advance` step, as geometry.
//
// One quad **instance per logical column**. The CPU kit walks the columns and
// stamps `lo..=hi` (the polyline segment joining this column's row to the
// previous one's) plus the 5-tap vertical glow around it, taking the `max` with
// whatever the decayed trail left there. This emits exactly the rows that stamp
// touches, and the host blends the pass with `GL_MAX` — which is not an
// approximation of the kit's `max`, it is the same operation.
//
// **Not `GL_LINES`.** A line primitive's rasterisation rules (diamond-exit,
// driver-dependent endpoint inclusion) are not the kit's `for y in lo..=hi`,
// and the difference would be a per-column off-by-one that no ceiling would
// forgive. A quad covering `[col, col+1) × [lo-2, hi+3)` is exact.
//
// The row range is recomputed here rather than passed in, because it is a pure
// function of the samples — `row_for(sample_at(col))` and its predecessor —
// and a per-column CPU pass to hand it over is precisely the work #863 is
// removing.

const int GLOW_SPAN = 2;   // hytte-preem/src/scope.rs

// Corner indices for two triangles over one quad: (0,1,2) and (2,1,3), where
// corner `c` has `(c & 1, (c >> 1) & 1)`.
const int CORNERS[6] = int[6](0, 1, 2, 2, 1, 3);

uniform sampler2D u_tex0;   // the 1-D R32F sample strip
uniform int u_data_len;     // samples in it (0 = flatline on the axis)
uniform ivec2 u_grid;       // (cols, rows)
uniform int u_step_back;    // steps back from the newest that this pass replays
uniform int u_batch_step_back; // where the batch is stamped, same units; -1 = never

flat out int v_lo;
flat out int v_hi;

// `hytte-preem/src/scope.rs`'s `sanitize`: NaN and both infinities read as the
// axis, finite values clamp to the rails. Deliberately not a bare `clamp` —
// `+inf` becomes `0.0`, not `1.0`.
float sanitize(float v) {
    return (isnan(v) || isinf(v)) ? 0.0 : clamp(v, -1.0, 1.0);
}

float sample_texel(int i) {
    return sanitize(texelFetch(u_tex0, ivec2(i, 0), 0).r);
}

// `sample_at`, index math and all. Linear interpolation across the sample
// points so a handful of sparse bins render as a continuous waveform.
float sample_at(int x, int width) {
    int count = u_data_len;
    if (count == 0) {
        return 0.0;
    }
    if (count == 1) {
        return sample_texel(0);
    }
    float span = float(count - 1);
    float across = (width <= 1) ? 0.0 : float(x) / float(width - 1);
    float pos = across * span;
    int idx = min(int(pos), count - 1);
    int next = min(idx + 1, count - 1);
    float frac = pos - floor(pos);
    float lo = sample_texel(idx);
    float hi = sample_texel(next);
    return lo + (hi - lo) * frac;
}

// `row_for`: 0.0 is the centre axis, +1.0 the top, -1.0 the bottom, with a
// GLOW_SPAN margin kept at each edge so the glow never clips.
//
// `floor(x + 0.5)` rather than `round(x)`: GLSL leaves the direction of a
// halfway case implementation-defined, while Rust's `f32::round` is
// away-from-zero. The argument here is non-negative by construction (the
// amplitude never exceeds the centre), so `floor(x + 0.5)` *is* away-from-zero
// — and it removes the one place the two ends could legitimately disagree.
int row_for(float value, int height) {
    if (height == 0) {
        return 0;
    }
    float last = float(height - 1);
    float centre = last * 0.5;
    float amplitude = max(centre - float(GLOW_SPAN), 0.0);
    float row = clamp(floor(centre - value * amplitude + 0.5), 0.0, last);
    return int(row);
}

void main() {
    int cols = u_grid.x;
    int rows = u_grid.y;
    int col = gl_InstanceID;

    // A step that is not the one this batch belongs to flatlines on the axis
    // while the trail keeps decaying — the kit's documented empty-batch
    // behaviour, and what a multi-step catch-up replays for every step after
    // the first.
    bool stamped = (u_step_back == u_batch_step_back);
    float here = stamped ? sample_at(col, cols) : 0.0;
    float there = stamped ? sample_at(max(col - 1, 0), cols) : 0.0;

    int row = row_for(here, rows);
    // The kit's `prev` is `None` at the first column, which makes the span a
    // single row rather than a join to a phantom neighbour.
    int prev = (col == 0) ? row : row_for(there, rows);
    int lo = min(prev, row);
    int hi = max(prev, row);
    v_lo = lo;
    v_hi = hi;

    int corner = CORNERS[gl_VertexID];
    float fx = float(corner & 1);
    float fy = float((corner >> 1) & 1);

    float x = float(col) + fx;
    float y0 = float(lo - GLOW_SPAN);
    float y1 = float(hi + GLOW_SPAN + 1);
    float y = mix(y0, y1, fy);

    gl_Position = vec4(
        2.0 * x / float(cols) - 1.0,
        2.0 * y / float(rows) - 1.0,
        0.0,
        1.0
    );
}
