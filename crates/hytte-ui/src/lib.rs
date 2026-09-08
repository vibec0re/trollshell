//! GTK4 + libadwaita + gtk4-layer-shell window primitives.
#![warn(missing_docs)]

mod app;
mod bar;
pub(crate) mod cast;
mod error;
pub mod gl_surface;
mod layer_window;
mod monitor;
pub mod multi_sparkline;
mod pixels;
mod popup;
pub mod shader_surface;
pub mod sparkline;
pub mod widget_tree;

pub use app::{App, AppBuilder};
pub use bar::{Bar, BarHandle, Edge};
pub use error::{Error, Result};
// The GPU counterpart to `PixelSurface` (#893 stage B): a `GtkGLArea` running a
// host-registered shader pipeline for `Node::GlSurface`. The pipeline
// vocabulary and the registry live in the module; only the widget and the
// uniform bag are hoisted here, because those are what a shell names.
pub use gl_surface::{GlProgram, GlSurface, GlUniforms, GlValue};
pub use layer_window::{Anchor, LayerWindowBuilder, Margin, layer_window, on_surface_ready};
pub use monitor::Monitor;
pub use multi_sparkline::MultiSparkline;
// The other GPU surface (#893): a `GtkGLArea` running a *plugin-supplied*
// fragment shader for `Node::Shader`, compiled once and fed a data buffer per
// frame. Its interface contract — the preamble, the guaranteed uniforms — lives
// in the module; the widget, the state and the format are what a shell names.
pub use shader_surface::{ShaderFormat, ShaderState, ShaderSurface};
// Exported for the shell as well as for the plugin reconciler (#857): a shell
// that rasterises a `hytte-preem` surface in-process needs the same
// nearest-neighbor raster widget the reconciler mounts for `Node::Pixels`, and
// reimplementing it against `gtk::Picture` would blur exactly the chunky pixels
// this subclass exists to keep crisp.
pub use pixels::PixelSurface;
pub use popup::{Popup, PopupBuilder, Position as PopupPosition, attach_dismiss_catcher};
pub use sparkline::Sparkline;
pub use widget_tree::{Dir, EventKind, Node, NodeId, Reconciler};

// `Edge` re-exported as `LayerEdge` to avoid colliding with `bar::Edge`.
pub use gtk4_layer_shell::{Edge as LayerEdge, KeyboardMode, Layer, LayerShell};

// Re-export so consumers don't need their own gtk/adw deps.
pub use ::adw;
pub use ::gtk;
