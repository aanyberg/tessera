//! Colors and the user-facing appearance contract.

use tessera_grid::Color;

use crate::font::{DEFAULT_FAMILY, DEFAULT_FONT_SIZE_PT};
use crate::palette::{ColorSlot, indexed_srgb};

/// Linear-space RGBA color.
///
/// Values are linear (not sRGB-encoded). When the render target has an sRGB
/// format the GPU encodes on write, so a clear to `Rgba { r: 0.25, .. }`
/// lands as byte `137` in an `*Srgb` texture and as `64` in a plain `Unorm`
/// one. See [`crate::OffscreenTarget`] for the test that pins this down.
///
/// Every render target holds *premultiplied* alpha: clears and the grid
/// passes write `[r·a, g·a, b·a, a]` (see [`Rgba::premultiplied`] and
/// [`Rgba::to_clear_color`]), which is what Core Animation expects of a
/// non-opaque Metal layer (ADR 0010).
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

    /// An opaque color from sRGB-encoded bytes (`#rrggbb`), decoded to
    /// linear with the exact sRGB transfer function.
    #[must_use]
    pub fn from_srgb8(r: u8, g: u8, b: u8) -> Self {
        Self::new(srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b), 1.0)
    }

    /// The color's red, green and blue encoded as sRGB bytes, rounded to
    /// the nearest byte (after [`Self::clamped`]); alpha is dropped.
    ///
    /// Inverse of [`Self::from_srgb8`] for every byte value.
    #[must_use]
    pub fn to_srgb8(self) -> [u8; 3] {
        let c = self.clamped();
        [
            linear_to_srgb8(c.r),
            linear_to_srgb8(c.g),
            linear_to_srgb8(c.b),
        ]
    }

    /// The same color with alpha replaced by `a`.
    #[must_use]
    pub const fn with_alpha(self, a: f32) -> Self {
        Self::new(self.r, self.g, self.b, a)
    }

    /// The color as premultiplied `[r·a, g·a, b·a, a]`, after
    /// [`Self::clamped`]: what the render passes hand the GPU.
    #[must_use]
    pub fn premultiplied(self) -> [f32; 4] {
        let c = self.clamped();
        [c.r * c.a, c.g * c.a, c.b * c.a, c.a]
    }

    /// The premultiplied GPU clear value for this color, what every clear
    /// in this crate uses. Equal to the plain conversion when opaque.
    #[must_use]
    pub fn to_clear_color(self) -> wgpu::Color {
        let [r, g, b, a] = self.premultiplied();
        wgpu::Color {
            r: f64::from(r),
            g: f64::from(g),
            b: f64::from(b),
            a: f64::from(a),
        }
    }
}

/// The sRGB transfer function: encoded `0.0..=1.0` to linear light.
pub(crate) fn srgb_decode(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// The inverse sRGB transfer function: linear light to encoded.
pub(crate) fn srgb_encode(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// Decodes one sRGB byte to linear light.
fn srgb_to_linear(byte: u8) -> f32 {
    srgb_decode(f32::from(byte) / 255.0)
}

/// Encodes a linear value in `0.0..=1.0` as the nearest sRGB byte.
fn linear_to_srgb8(c: f32) -> u8 {
    let encoded = srgb_encode(c);
    // In range after the clamp; `as` saturates anyway.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0..=255 before the cast"
    )]
    let byte = (encoded * 255.0).round().clamp(0.0, 255.0) as u8;
    byte
}

impl From<Rgba> for wgpu::Color {
    /// Converts through [`Rgba::clamped`], so the GPU never sees a
    /// `NaN` or out-of-range clear value. The result is straight
    /// (not premultiplied) alpha; clears use [`Rgba::to_clear_color`].
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
///
/// Opacity: `background.a` below 1.0 makes cells with the default
/// background translucent (cells with an explicit background and all
/// glyphs stay opaque); `cursor.a` and `selection.a` are the cursor's and
/// the selection highlight's opacity. The font fields describe the font
/// a frontend should build its [`crate::Shaper`] with; the renderer itself
/// draws with the shaper it is given (see [`crate::Renderer::new`]).
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

impl Theme {
    /// The color `color` stands for in this theme, in `slot`.
    ///
    /// `Color::Default` is [`Self::foreground`] or [`Self::background`],
    /// indexes `0..16` come from [`Self::ansi`], `16..=231` are the xterm
    /// 6×6×6 color cube and `232..=255` its 24 greys (both computed, see
    /// [`crate::Palette`]), and `Rgb` is taken as sRGB bytes. Only the
    /// default background keeps the theme's alpha; every other result is
    /// opaque. [`crate::Palette`] caches the same lookup for a frame.
    #[must_use]
    pub fn resolve(&self, color: Color, slot: ColorSlot) -> Rgba {
        match color {
            Color::Default => match slot {
                ColorSlot::Foreground => self.foreground.with_alpha(1.0),
                ColorSlot::Background => self.background,
            },
            Color::Indexed(i) => match indexed_srgb(i) {
                Some([r, g, b]) => Rgba::from_srgb8(r, g, b),
                None => self.ansi[usize::from(i)].with_alpha(1.0),
            },
            Color::Rgb(r, g, b) => Rgba::from_srgb8(r, g, b),
        }
    }
}

impl Default for Theme {
    /// Menlo 13 pt, light grey on black, the xterm 16-color palette, a
    /// grey cursor and a translucent blue selection.
    fn default() -> Self {
        const fn hex(rgb: u32) -> [u8; 3] {
            let [_, r, g, b] = rgb.to_be_bytes();
            [r, g, b]
        }
        let srgb = |rgb: u32| {
            let [r, g, b] = hex(rgb);
            Rgba::from_srgb8(r, g, b)
        };
        let ansi = [
            0x00_0000, 0xcd_0000, 0x00_cd00, 0xcd_cd00, 0x00_00ee, 0xcd_00cd, 0x00_cdcd, 0xe5_e5e5,
            0x7f_7f7f, 0xff_0000, 0x00_ff00, 0xff_ff00, 0x5c_5cff, 0xff_00ff, 0x00_ffff, 0xff_ffff,
        ]
        .map(srgb);
        Self {
            font_family: DEFAULT_FAMILY.to_owned(),
            font_size: DEFAULT_FONT_SIZE_PT,
            line_height: 1.0,
            foreground: srgb(0xe5_e5e5),
            background: srgb(0x00_0000),
            cursor: srgb(0xb0_b0b0),
            selection: srgb(0x26_4f78).with_alpha(0.6),
            ansi,
            padding: 4.0,
        }
    }
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
    fn srgb_bytes_round_trip_exactly() {
        for b in 0..=255u8 {
            let c = Rgba::from_srgb8(b, b, b);
            assert_eq!(c.to_srgb8(), [b, b, b], "byte {b} decoded to {}", c.r);
            assert!(c.is_valid());
        }
        // Known points of the transfer curve.
        assert_eq!(Rgba::new(0.25, 0.5, 1.0, 1.0).to_srgb8(), [137, 188, 255]);
        assert!((Rgba::from_srgb8(188, 0, 0).r - 0.5).abs() < 0.003);
    }

    #[test]
    fn premultiplied_scales_color_by_alpha() {
        assert_eq!(
            Rgba::new(1.0, 0.5, 0.0, 0.5).premultiplied(),
            [0.5, 0.25, 0.0, 0.5]
        );
        assert_eq!(
            Rgba::new(f32::NAN, 2.0, 0.0, 1.0).premultiplied()[..2],
            [0.0, 1.0]
        );
    }

    #[test]
    fn theme_resolves_default_ansi_cube_and_rgb() {
        let theme = Theme {
            background: Rgba::new(0.1, 0.1, 0.1, 0.5),
            ..Theme::default()
        };
        assert_eq!(
            theme.resolve(Color::Default, ColorSlot::Foreground),
            theme.foreground
        );
        let see_through = Theme {
            foreground: Rgba::new(1.0, 1.0, 1.0, 0.25),
            ..theme.clone()
        };
        assert!(
            (see_through.resolve(Color::Default, ColorSlot::Foreground).a - 1.0).abs()
                < f32::EPSILON,
            "only the default background keeps the theme's alpha"
        );
        assert_eq!(
            theme.resolve(Color::Default, ColorSlot::Background),
            theme.background,
            "the default background keeps the theme's opacity"
        );
        for i in 0..16u8 {
            for slot in [ColorSlot::Foreground, ColorSlot::Background] {
                assert_eq!(
                    theme.resolve(Color::Indexed(i), slot),
                    theme.ansi[usize::from(i)]
                );
            }
        }
        assert_eq!(
            theme
                .resolve(Color::Indexed(196), ColorSlot::Background)
                .to_srgb8(),
            [0xff, 0, 0]
        );
        let rgb = theme.resolve(Color::Rgb(1, 2, 3), ColorSlot::Foreground);
        assert_eq!((rgb.to_srgb8(), rgb.a), ([1, 2, 3], 1.0));
    }

    #[test]
    fn clear_color_is_premultiplied() {
        let c = Rgba::new(0.5, 1.0, 0.25, 0.5).to_clear_color();
        assert_eq!((c.r, c.g, c.b, c.a), (0.25, 0.5, 0.125, 0.5));
        let opaque = Rgba::new(0.25, 0.5, 1.0, 1.0);
        assert_eq!(opaque.to_clear_color(), wgpu::Color::from(opaque));
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
