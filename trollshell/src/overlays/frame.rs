//! Per-monitor OVERLAY-layer window that paints the dark frame around
//! the workspace cutout. Full-screen, click-through, no exclusive zone.
//!
//! Layered above the bar (which is on `Layer::Top`). Bar widgets remain
//! interactive because the frame's input region is empty — every click
//! falls through to the layer below.
//!
//! The frame paints the bar's dark gradient (3-stop, 90deg, screen-width
//! aligned) into the L/R/bottom border regions and carves four rounded
//! inner corners around the workspace cutout. Top inset is the bar's
//! exclusive zone.
//!
//! Visual constants — match `etc/niri/frame.kdl` struts and the
//! post-restyle bar geometry from `style.css`. If any of these change,
//! update both sides.

use hytte::prelude::*;
use hytte::ui::layer_window;

/// Bar height after restyle: `padding: 6px 12px` (12 vertical) + `min-height: 32px` = 44.
/// Top inset of the frame (= top of the workspace cutout).
const BAR_HEIGHT: f64 = 44.0;

/// Frame thickness on left, right, and bottom. Must match the niri
/// `struts` values in `etc/niri/frame.kdl`.
const FRAME_THICKNESS: f64 = 12.0;

/// Corner radius for all four corners of the workspace cutout.
const CUTOUT_RADIUS: f64 = 16.0;

/// Mount one frame overlay on `monitor`.
pub fn install(_monitor: &Monitor) {
    // Skeleton — implementation lands in tasks 3 and 4.
}
