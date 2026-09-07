// The beam's vertical glow kernel, evaluated per fragment.
//
// `hytte-preem/src/scope.rs`'s `stamp_beam` stamps every row of the polyline
// span `lo..=hi` with the 5-tap `GLOW` kernel, taking the `max` at each pixel.
// The union of those taps over the whole span is exactly:
//
//     |distance outside [lo, hi]|   0 -> CORE   1 -> GLOW_INNER   2 -> GLOW_OUTER
//
// so one distance test replaces the stamp loop. Rows further than GLOW_SPAN
// contribute nothing; they are only rasterised because the quad's extent is the
// span padded by the kernel's reach.
//
// Rows outside the buffer are clipped by the viewport, which is the same
// silent clip `Scope::stamp` does with its bounds check.

const int CORE = 255;
const int GLOW_INNER = 130;
const int GLOW_OUTER = 45;

flat in int v_lo;
flat in int v_hi;

out vec4 o_intensity;

void main() {
    int row = int(gl_FragCoord.y);
    int outside = 0;
    if (row < v_lo) {
        outside = v_lo - row;
    } else if (row > v_hi) {
        outside = row - v_hi;
    }
    int intensity = 0;
    if (outside == 0) {
        intensity = CORE;
    } else if (outside == 1) {
        intensity = GLOW_INNER;
    } else if (outside == 2) {
        intensity = GLOW_OUTER;
    }
    // Blended with GL_MAX, so a zero here leaves the decayed trail untouched —
    // which is what `phosphor[i].max(intensity)` does on the CPU.
    o_intensity = vec4(float(intensity) / 255.0, 0.0, 0.0, 1.0);
}
