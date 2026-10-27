//! Logical vs. physical sizes and `HiDPI` scale.
//!
//! Frontends report window sizes in logical points plus a backing scale
//! factor (`NSWindow.backingScaleFactor`, winit's `scale_factor()`); the GPU
//! surface needs integer physical pixels. The conversions here are pure so
//! the rounding rules are unit-tested without a GPU.

/// Size in logical points, as the window system reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogicalSize {
    /// Width in points.
    pub width: f32,
    /// Height in points.
    pub height: f32,
}

impl LogicalSize {
    /// Builds a logical size.
    #[must_use]
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }

    /// Converts to physical pixels at the given scale factor.
    ///
    /// Rounds to the nearest pixel; a non-finite or non-positive scale is
    /// treated as `1.0`. The result may be zero-sized (a minimised window).
    #[must_use]
    pub fn to_physical(self, scale: f32) -> PhysicalSize {
        let scale = sanitize_scale(scale);
        PhysicalSize {
            width: round_dim(self.width * scale),
            height: round_dim(self.height * scale),
        }
    }
}

/// Size in physical pixels, what the surface and textures are measured in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PhysicalSize {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl PhysicalSize {
    /// Builds a physical size.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// `true` when either dimension is zero; such a surface cannot be
    /// configured and frames are skipped until the next non-empty resize.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Converts back to logical points at the given scale factor.
    #[must_use]
    pub fn to_logical(self, scale: f32) -> LogicalSize {
        let scale = sanitize_scale(scale);
        #[allow(clippy::cast_precision_loss)]
        LogicalSize {
            width: self.width as f32 / scale,
            height: self.height as f32 / scale,
        }
    }

    /// Clamps both dimensions to `max` (the device's
    /// `max_texture_dimension_2d`), keeping zero as zero.
    #[must_use]
    pub const fn clamped_to(self, max: u32) -> Self {
        Self {
            width: if self.width > max { max } else { self.width },
            height: if self.height > max { max } else { self.height },
        }
    }

    /// Converts to `wgpu`'s extent for a single 2D layer.
    #[must_use]
    pub const fn extent(self) -> wgpu::Extent3d {
        wgpu::Extent3d {
            width: self.width,
            height: self.height,
            depth_or_array_layers: 1,
        }
    }
}

/// Smallest backing scale factor accepted; smaller positive ones are
/// raised to it.
///
/// At [`crate::MIN_FONT_SIZE_PT`] it still gives a 1 px em: a glyph size
/// that rounds to zero would make swash return unscaled (font-unit)
/// outlines, hundreds of pixels across.
pub const MIN_SCALE: f32 = 0.25;

/// Returns `scale` raised to at least [`MIN_SCALE`] if it is finite and
/// positive, else `1.0`.
#[must_use]
pub fn sanitize_scale(scale: f32) -> f32 {
    if scale.is_finite() && scale > 0.0 {
        scale.max(MIN_SCALE)
    } else {
        1.0
    }
}

fn round_dim(v: f32) -> u32 {
    // `NaN > 0.0` is false, so NaN and anything non-positive become 0.
    // Positive values (including `+inf`, which a product of two finite
    // floats can overflow to) go through `as`, which saturates to
    // `u32::MAX`: a bigger request never yields a smaller surface.
    if v > 0.0 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            v.round() as u32
        }
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_to_physical_at_1x_and_2x() {
        let l = LogicalSize::new(800.0, 600.0);
        assert_eq!(l.to_physical(1.0), PhysicalSize::new(800, 600));
        assert_eq!(l.to_physical(2.0), PhysicalSize::new(1600, 1200));
    }

    #[test]
    fn fractional_scale_rounds_to_nearest_pixel() {
        let l = LogicalSize::new(101.0, 33.0);
        // 101 * 1.5 = 151.5 -> 152 (round half away from zero), 33 * 1.5 = 49.5 -> 50
        assert_eq!(l.to_physical(1.5), PhysicalSize::new(152, 50));
        assert_eq!(
            LogicalSize::new(10.4, 10.6).to_physical(1.0),
            PhysicalSize::new(10, 11)
        );
    }

    #[test]
    fn degenerate_scale_falls_back_to_1x() {
        let l = LogicalSize::new(640.0, 480.0);
        for s in [0.0, -2.0, f32::NAN, f32::INFINITY] {
            assert_eq!(l.to_physical(s), PhysicalSize::new(640, 480), "scale {s}");
        }
    }

    #[test]
    fn tiny_positive_scales_are_floored() {
        for s in [f32::MIN_POSITIVE, 1e-6, 0.1] {
            assert!(
                (sanitize_scale(s) - MIN_SCALE).abs() < f32::EPSILON,
                "scale {s}"
            );
        }
        assert!((sanitize_scale(MIN_SCALE) - MIN_SCALE).abs() < f32::EPSILON);
        assert!((sanitize_scale(1.5) - 1.5).abs() < f32::EPSILON);
    }

    #[test]
    fn zero_and_negative_logical_sizes_are_empty() {
        assert!(LogicalSize::new(0.0, 100.0).to_physical(2.0).is_empty());
        assert!(LogicalSize::new(-5.0, 100.0).to_physical(2.0).is_empty());
        assert!(!LogicalSize::new(1.0, 1.0).to_physical(0.5).is_empty());
    }

    #[test]
    fn huge_logical_sizes_saturate_monotonically() {
        // A product that saturates and one that overflows to infinity must
        // both land on `u32::MAX`, never wrap or collapse to zero.
        assert_eq!(
            LogicalSize::new(1e20, 10.0).to_physical(1.0),
            PhysicalSize::new(u32::MAX, 10)
        );
        assert_eq!(
            LogicalSize::new(f32::MAX, 10.0).to_physical(2.0),
            PhysicalSize::new(u32::MAX, 20)
        );
        assert_eq!(
            LogicalSize::new(f32::INFINITY, 1.0).to_physical(1.0),
            PhysicalSize::new(u32::MAX, 1)
        );
        assert!(LogicalSize::new(f32::NAN, 0.0).to_physical(1.0).is_empty());
        assert!(
            LogicalSize::new(f32::NEG_INFINITY, 1.0)
                .to_physical(1.0)
                .is_empty()
        );
    }

    #[test]
    fn physical_round_trips_to_logical() {
        let p = PhysicalSize::new(1600, 1200);
        let l = p.to_logical(2.0);
        assert_eq!(l, LogicalSize::new(800.0, 600.0));
        assert_eq!(l.to_physical(2.0), p);
    }

    #[test]
    fn clamps_to_device_limit() {
        assert_eq!(
            PhysicalSize::new(20000, 10).clamped_to(16384),
            PhysicalSize::new(16384, 10)
        );
        assert_eq!(
            PhysicalSize::new(0, 10).clamped_to(16384),
            PhysicalSize::new(0, 10)
        );
    }
}
