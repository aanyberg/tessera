//! GPU rendering of a grid.
//!
//! Turns a [`tessera_grid::Grid`] plus a [`Theme`] into pixels using `wgpu`
//! (Metal on macOS). Owns the glyph atlas, text shaping, damage tracking and
//! the background / glyph / cursor / decoration passes. The surface it draws
//! into is handed over by the frontend as a raw window handle, so this crate
//! has no `AppKit` dependency and can serve a future Linux frontend unchanged.
//!
//! The `wgpu` dependency lands in M2; this crate currently defines the theme
//! and surface contracts the frontend compiles against.

#![forbid(unsafe_code)]

/// Linear-space RGBA color.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    /// Red, 0.0..=1.0.
    pub r: f32,
    /// Green, 0.0..=1.0.
    pub g: f32,
    /// Blue, 0.0..=1.0.
    pub b: f32,
    /// Alpha, 0.0..=1.0.
    pub a: f32,
}

/// Everything about appearance that the renderer needs and the user may change.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    /// Primary font family.
    pub font_family: String,
    /// Font size in points.
    pub font_size: f32,
    /// Line height multiplier.
    pub line_height: f32,
    /// Default foreground.
    pub foreground: Rgba,
    /// Default background. Alpha below 1.0 enables transparency.
    pub background: Rgba,
    /// Cursor color.
    pub cursor: Rgba,
    /// Selection highlight.
    pub selection: Rgba,
    /// The 16 ANSI colors.
    pub ansi: [Rgba; 16],
    /// Padding around the grid in logical pixels.
    pub padding: f32,
}
