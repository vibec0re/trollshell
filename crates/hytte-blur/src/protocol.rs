//! Client bindings for the `ext-background-effect-v1` staging protocol.
//!
//! Re-exported from the maintained `wayland-protocols` crate (feature
//! `staging,client`), which generates them from the same upstream XML. The XML
//! is also vendored at `protocol/ext-background-effect-v1.xml` for reference and
//! to document the exact interface this crate targets (manager v1).
//!
//! The two interfaces:
//! - `ext_background_effect_manager_v1` — bound from the registry; emits a
//!   `capabilities` event (bitfield, `blur = 1`); `get_background_effect`
//!   factories an effect object for a `wl_surface`.
//! - `ext_background_effect_surface_v1` — `set_blur_region(wl_region | NULL)`,
//!   surface-local, double-buffered (applies on the surface's next commit).

pub use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1, ext_background_effect_surface_v1,
};
