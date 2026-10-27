//! Error type for GPU bootstrap, presentation, fonts and the glyph atlas.

/// Everything that can go wrong while bringing up or driving the GPU.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RenderError {
    /// No GPU adapter matched the request (no Metal device, or none
    /// compatible with the surface).
    #[error("no compatible GPU adapter: {0}")]
    NoAdapter(#[from] wgpu::RequestAdapterError),
    /// The adapter refused to open a logical device.
    #[error("failed to open GPU device: {0}")]
    RequestDevice(#[from] wgpu::RequestDeviceError),
    /// The window handle could not be turned into a presentable surface.
    #[error("failed to create surface: {0}")]
    CreateSurface(#[from] wgpu::CreateSurfaceError),
    /// The adapter reports no usable format or present mode for the surface.
    #[error("surface is not supported by the adapter")]
    UnsupportedSurface,
    /// The swapchain was lost and could not be recovered by reconfiguring.
    #[error("surface lost")]
    SurfaceLost,
    /// Copying a texture back to the CPU failed.
    #[error("readback failed: {0}")]
    Readback(String),
    /// `wgpu` reported a validation error while recording or submitting
    /// work. The message is `wgpu`'s own description.
    #[error("GPU error: {0}")]
    Gpu(String),
    /// No usable font: neither the configured family, nor the default, nor
    /// any monospaced face could be loaded.
    #[error("no usable font: {0}")]
    NoFont(String),
    /// A glyph bitmap is larger than an atlas layer.
    #[error("glyph of {width}x{height} pixels does not fit in the atlas")]
    GlyphTooLarge {
        /// Bitmap width in pixels.
        width: u32,
        /// Bitmap height in pixels.
        height: u32,
    },
    /// An atlas layer has no room left, even after evicting every glyph not
    /// used in the current frame.
    #[error("glyph atlas {0:?} layer is full")]
    AtlasFull(crate::AtlasLayer),
}
