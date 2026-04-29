//! Per-monitor layer-shell overlays — lock screen, OSD, dialogs, toast.
//! Each module exposes a `pub fn install(...)` that wires the overlay
//! to the relevant signal source. Phase 2 of the reorg populates this
//! by `git mv`-ing the existing overlay files out of `widgets/`.
