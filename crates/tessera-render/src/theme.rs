//! Colors and the user-facing appearance contract.

/// Linear-space RGBA color.
///
/// Values are linear (not sRGB-encoded). When the render target has an sRGB
/// format the GPU encodes on write, so a clear to `Rgba { r: 0.25, .. }`
/// lands as byte `137` in an `*Srgb` texture and as `64` in a plain `Unorm`
/// one. See [`crate::OffscreenTarget`] for the test that pins this down.
///
/// Components are meant to lie in `0.0..=1.0`. Theme colors ultimately come
/// from user configuration, so nothing here panics on bad input: every
/// conversion to a GPU color goes through [`Rgba::clamped`], which maps
/// `NaN` to `0.0` and clamps the rest into range.
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

impl Rgba {
    /// Opaque black.
    pub const BLACK: Self = Self::new(0.0, 0.0, 0.0, 1.0);
    /// Opaque white.
    pub const WHITE: Self = Self::new(1.0, 1.0, 1.0, 1.0);
    /// Fully transparent black.
    pub const TRANSPARENT: Self = Self::new(0.0, 0.0, 0.0, 0.0);

    /// Builds a color from linear components.
    #[must_use]
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// `true` when every component is finite and within `0.0..=1.0`.
    #[must_use]
    pub fn is_valid(self) -> bool {
        [self.r, self.g, self.b, self.a]
            .into_iter()
            .all(|c| (0.0..=1.0).contains(&c))
    }

    /// Returns the color with every component clamped to `0.0..=1.0`.
    ///
    /// `NaN` becomes `0.0`, `-inf` becomes `0.0` and `+inf` becomes `1.0`,
    /// so an invalid user-supplied color renders as the nearest valid one
    /// instead of whatever the GPU does with a `NaN` clear value.
    #[must_use]
    pub fn clamped(self) -> Self {
        fn unit(c: f32) -> f32 {
            if c.is_nan() { 0.0 } else { c.clamp(0.0, 1.0) }
        }
        Self::new(unit(self.r), unit(self.g), unit(self.b), unit(self.a))
    }
}

impl From<Rgba> for wgpu::Color {
    /// Converts through [`Rgba::clamped`], so the GPU never sees a
    /// `NaN` or out-of-range clear value.
    fn from(c: Rgba) -> Self {
        let c = c.clamped();
        Self {
            r: f64::from(c.r),
            g: f64::from(c.g),
            b: f64::from(c.b),
            a: f64::from(c.a),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_colors_are_unchanged_by_clamping() {
        for c in [
            Rgba::BLACK,
            Rgba::WHITE,
            Rgba::TRANSPARENT,
            Rgba::new(0.25, 0.5, 1.0, 0.75),
        ] {
            assert!(c.is_valid(), "{c:?}");
            assert_eq!(c.clamped(), c);
        }
    }

    #[test]
    fn invalid_components_clamp_into_range() {
        let c = Rgba::new(f32::NAN, f32::INFINITY, -1.0, 2.0);
        assert!(!c.is_valid());
        assert_eq!(c.clamped(), Rgba::new(0.0, 1.0, 0.0, 1.0));
        assert_eq!(Rgba::new(f32::NEG_INFINITY, 0.5, 0.5, 0.5).clamped().r, 0.0);
    }

    #[test]
    fn gpu_color_conversion_clamps() {
        let c: wgpu::Color = Rgba::new(f32::NAN, 1.5, -0.5, 1.0).into();
        assert_eq!(c.r, 0.0);
        assert_eq!(c.g, 1.0);
        assert_eq!(c.b, 0.0);
        assert_eq!(c.a, 1.0);
    }
}
