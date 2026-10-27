//! GPU rendering of a grid.
//!
//! Turns rows of [`tessera_grid::Cell`]s plus a [`Theme`] into pixels using
//! `wgpu` (Metal on macOS). Owns the glyph atlas, text shaping and the
//! background / glyph / decoration / cursor passes (damage tracking is
//! still to come). The surface it draws into is handed over by the frontend
//! as a raw window handle, so this crate has no `AppKit` dependency and can
//! serve a future Linux frontend unchanged.
//!
//! The bootstrap is [`Gpu`] (instance, adapter, device, queue),
//! [`RenderSurface`] (swapchain configuration, resize, `HiDPI` scale, vsync,
//! sRGB) and [`OffscreenTarget`] so everything also runs headless in CI.
//!
//! Text goes through three pieces (ADR 0007): [`Fonts`] resolves the
//! configured family and its fallback chain from the system fonts and
//! rasterises glyphs; [`Shaper`] computes [`CellMetrics`] and shapes a row of
//! cells into [`ShapedGlyph`]s (ligatures included); [`GlyphAtlas`] caches the
//! rasterised glyphs in an alpha and a colour texture with LRU eviction.
//!
//! [`Renderer`] draws a [`FrameInput`] (rows of cells, cursor, selection,
//! overlays; decoupled from the grid, so the caller picks the visible
//! rows) in four passes: backgrounds, glyphs, decorations and the cursor
//! (ADR 0010). [`Palette`] resolves cell colors against the [`Theme`].
//! [`RenderSurface::render`] presents a frame; [`OffscreenTarget::render`]
//! draws one headless for the golden tests.
//!
//! # Threading
//!
//! `wgpu`'s Metal backend requires surface creation on the main thread;
//! everything else may run from any thread. `Gpu` is `Clone + Send + Sync`,
//! and a [`Renderer`] is `Send + Sync`, so a view's renderer can move to a
//! render thread.
//!
//! # Backends
//!
//! Only the Metal backend is compiled in. A Linux frontend enables `vulkan`
//! on `wgpu` here and changes nothing else.

#![forbid(unsafe_code)]

pub use raw_window_handle;
pub use wgpu;

mod atlas;
mod boxdraw;
mod error;
mod font;
mod frame;
mod gpu;
mod offscreen;
mod palette;
mod passes;
mod renderer;
mod shaping;
mod size;
mod surface;
mod theme;

pub use atlas::{AtlasLayer, AtlasSlot, AtlasStats, DEFAULT_ATLAS_SIZE, GlyphAtlas, GlyphKey};
pub use error::RenderError;
pub use font::{
    DEFAULT_FAMILY, DEFAULT_FONT_SIZE_PT, EMOJI_FAMILY, FaceId, FontConfig, Fonts,
    MAX_FONT_SIZE_PT, MAX_SCALE, MIN_FONT_SIZE_PT, SYSTEM_FALLBACK, StyleSupport,
    sanitize_font_size,
};
pub use frame::{CursorShape, CursorState, FrameInput, Overlay, Selection};
pub use gpu::Gpu;
pub use offscreen::{
    OffscreenTarget, SUPPORTED_FORMATS, is_supported_format, padded_bytes_per_row,
};
pub use palette::{ColorSlot, Palette, indexed_srgb};
pub use renderer::Renderer;
pub use shaping::{CellMetrics, ShapedGlyph, Shaper};
pub use size::{LogicalSize, MIN_SCALE, PhysicalSize, sanitize_scale};
pub use surface::{
    FrameStatus, RenderSurface, SurfaceOptions, build_config, choose_alpha_mode, choose_format,
    choose_present_mode,
};
pub use theme::{Rgba, Theme};
