//! GPU rendering of a grid.
//!
//! Turns a [`tessera_grid::Grid`] plus a [`Theme`] into pixels using `wgpu`
//! (Metal on macOS). Owns the glyph atlas, text shaping, damage tracking and
//! the background / glyph / cursor / decoration passes. The surface it draws
//! into is handed over by the frontend as a raw window handle, so this crate
//! has no `AppKit` dependency and can serve a future Linux frontend unchanged.
//!
//! What exists today is the bootstrap: [`Gpu`] (instance, adapter, device,
//! queue), [`RenderSurface`] (swapchain configuration, resize, `HiDPI` scale,
//! vsync, sRGB) with a frontend-callable [`RenderSurface::clear`] pass, and
//! [`OffscreenTarget`] so the same clear pass runs headless in CI. The grid
//! passes land on top of these.
//!
//! # Threading
//!
//! `wgpu`'s Metal backend requires surface creation on the main thread;
//! everything else may run from any thread. `Gpu` is `Clone + Send + Sync`.
//!
//! # Backends
//!
//! Only the Metal backend is compiled in. A Linux frontend enables `vulkan`
//! on `wgpu` here and changes nothing else.

#![forbid(unsafe_code)]

pub use raw_window_handle;
pub use wgpu;

mod error;
mod gpu;
mod offscreen;
mod size;
mod surface;
mod theme;

pub use error::RenderError;
pub use gpu::Gpu;
pub use offscreen::{
    OffscreenTarget, SUPPORTED_FORMATS, is_supported_format, padded_bytes_per_row,
};
pub use size::{LogicalSize, PhysicalSize, sanitize_scale};
pub use surface::{
    FrameStatus, RenderSurface, SurfaceOptions, build_config, choose_alpha_mode, choose_format,
    choose_present_mode,
};
pub use theme::{Rgba, Theme};
