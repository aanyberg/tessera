//! The 256-color palette: theme ANSI colors plus the xterm cube and greys.
//!
//! Indexes `0..16` are the theme's [`Theme::ansi`] colors. Indexes
//! `16..=231` are xterm's 6×6×6 color cube, whose channel levels are
//! `0, 95, 135, 175, 215, 255` (`0` then `55 + 40·n`), and `232..=255` are
//! 24 greys from `#080808` to `#eeeeee` in steps of 10. Both are computed
//! here rather than stored in the theme, which is what every terminal that
//! emulates xterm does: themes customise the 16 ANSI colors only.

use tessera_grid::Color;

use crate::theme::{Rgba, Theme, srgb_decode, srgb_encode};

/// Which side of a cell a [`Color`] paints, deciding what
/// `Color::Default` means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorSlot {
    /// The glyph and its decorations: `Color::Default` is
    /// [`Theme::foreground`].
    Foreground,
    /// The cell background: `Color::Default` is [`Theme::background`].
    Background,
}

/// Channel levels of the xterm color cube.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// The sRGB bytes of xterm color `index` for the computed part of the
/// palette (`16..=255`), or `None` for the 16 ANSI colors, which come from
/// the theme.
///
/// ```
/// use tessera_render::indexed_srgb;
/// assert_eq!(indexed_srgb(1), None);
/// assert_eq!(indexed_srgb(196), Some([0xff, 0x00, 0x00]));
/// assert_eq!(indexed_srgb(232), Some([0x08, 0x08, 0x08]));
/// ```
#[must_use]
pub const fn indexed_srgb(index: u8) -> Option<[u8; 3]> {
    match index {
        0..=15 => None,
        16..=231 => {
            let i = index - 16;
            Some([
                CUBE_LEVELS[(i / 36) as usize],
                CUBE_LEVELS[(i / 6 % 6) as usize],
                CUBE_LEVELS[(i % 6) as usize],
            ])
        }
        232..=255 => {
            let v = 8 + (index - 232) * 10;
            Some([v, v, v])
        }
    }
}

/// A theme's colors resolved once into linear [`Rgba`], for a frame's
/// worth of lookups.
///
/// Built from a [`Theme`] by [`Palette::new`]; [`Palette::resolve`] gives
/// the same answers as [`Theme::resolve`] without recomputing the cube or
/// the sRGB decode per cell.
#[derive(Debug, Clone, PartialEq)]
pub struct Palette {
    indexed: [Rgba; 256],
    foreground: Rgba,
    background: Rgba,
}

impl Palette {
    /// Resolves all 256 indexed colors and the defaults of `theme`.
    #[must_use]
    pub fn new(theme: &Theme) -> Self {
        let indexed = std::array::from_fn(|i| {
            let i = u8::try_from(i).unwrap_or(u8::MAX);
            theme.resolve(Color::Indexed(i), ColorSlot::Foreground)
        });
        Self {
            indexed,
            foreground: theme.foreground.with_alpha(1.0),
            background: theme.background,
        }
    }

    /// Indexed color `index`, opaque.
    #[must_use]
    pub fn indexed(&self, index: u8) -> Rgba {
        self.indexed[usize::from(index)]
    }

    /// The default foreground, opaque.
    #[must_use]
    pub fn foreground(&self) -> Rgba {
        self.foreground
    }

    /// The default background, with the theme's opacity.
    #[must_use]
    pub fn background(&self) -> Rgba {
        self.background
    }

    /// The linear color `color` stands for in `slot`; see
    /// [`Theme::resolve`].
    #[must_use]
    pub fn resolve(&self, color: Color, slot: ColorSlot) -> Rgba {
        match color {
            Color::Default => match slot {
                ColorSlot::Foreground => self.foreground,
                ColorSlot::Background => self.background,
            },
            Color::Indexed(i) => self.indexed(i),
            Color::Rgb(r, g, b) => Rgba::from_srgb8(r, g, b),
        }
    }
}

/// How much `DIM` (SGR 2) darkens the foreground, applied to the
/// sRGB-encoded channels (what xterm and Alacritty do).
pub(crate) const DIM_FACTOR: f32 = 2.0 / 3.0;

/// `color` dimmed by [`DIM_FACTOR`] in sRGB space, alpha unchanged.
pub(crate) fn dim(color: Rgba) -> Rgba {
    let c = color.clamped();
    let scale = |v: f32| srgb_decode(srgb_encode(v) * DIM_FACTOR);
    Rgba::new(scale(c.r), scale(c.g), scale(c.b), c.a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(rgb: u32) -> [u8; 3] {
        let [_, r, g, b] = rgb.to_be_bytes();
        [r, g, b]
    }

    /// Spot values from xterm's `256colres.h`.
    const KNOWN: &[(u8, u32)] = &[
        (16, 0x00_0000),
        (17, 0x00_005f),
        (21, 0x00_00ff),
        (46, 0x00_ff00),
        (51, 0x00_ffff),
        (59, 0x5f_5f5f),
        (88, 0x87_0000),
        (102, 0x87_8787),
        (124, 0xaf_0000),
        (160, 0xd7_0000),
        (196, 0xff_0000),
        (201, 0xff_00ff),
        (208, 0xff_8700),
        (226, 0xff_ff00),
        (231, 0xff_ffff),
        (232, 0x08_0808),
        (233, 0x12_1212),
        (244, 0x80_8080),
        (250, 0xbc_bcbc),
        (255, 0xee_eeee),
    ];

    #[test]
    fn known_xterm_values() {
        for &(index, rgb) in KNOWN {
            assert_eq!(indexed_srgb(index), Some(hex(rgb)), "index {index}");
        }
    }

    #[test]
    fn every_index_resolves() {
        let theme = Theme::default();
        let palette = Palette::new(&theme);
        for index in 0..=255u8 {
            let c = palette.indexed(index);
            assert!(c.is_valid(), "{index}: {c:?}");
            assert!((c.a - 1.0).abs() < f32::EPSILON, "{index} is opaque");
            match indexed_srgb(index) {
                None => assert_eq!(c, theme.ansi[usize::from(index)], "{index}"),
                Some(bytes) => assert_eq!(c.to_srgb8(), bytes, "{index}"),
            }
            assert_eq!(
                palette.resolve(Color::Indexed(index), ColorSlot::Background),
                theme.resolve(Color::Indexed(index), ColorSlot::Background),
                "palette and theme agree on {index}"
            );
        }
    }

    #[test]
    fn cube_is_ordered_red_green_blue() {
        for r in 0..6u8 {
            for g in 0..6u8 {
                for b in 0..6u8 {
                    let index = 16 + 36 * r + 6 * g + b;
                    assert_eq!(
                        indexed_srgb(index),
                        Some([
                            CUBE_LEVELS[usize::from(r)],
                            CUBE_LEVELS[usize::from(g)],
                            CUBE_LEVELS[usize::from(b)]
                        ])
                    );
                }
            }
        }
    }

    #[test]
    fn greys_step_by_ten() {
        let greys: Vec<u8> = (232..=255u8)
            .map(|i| indexed_srgb(i).expect("grey")[0])
            .collect();
        assert_eq!(greys.len(), 24);
        assert!(greys.windows(2).all(|w| w[1] - w[0] == 10));
        assert_eq!((greys[0], greys[23]), (0x08, 0xee));
    }

    #[test]
    fn defaults_follow_the_slot() {
        let theme = Theme {
            background: Rgba::new(0.0, 0.0, 0.0, 0.25),
            ..Theme::default()
        };
        let palette = Palette::new(&theme);
        assert_eq!(
            palette.resolve(Color::Default, ColorSlot::Foreground),
            theme.foreground
        );
        assert_eq!(
            palette.resolve(Color::Default, ColorSlot::Background),
            theme.background
        );
        assert_eq!(
            palette
                .resolve(Color::Rgb(0x12, 0x34, 0x56), ColorSlot::Background)
                .to_srgb8(),
            [0x12, 0x34, 0x56]
        );
    }

    #[test]
    fn dim_scales_encoded_channels() {
        let dimmed = dim(Rgba::from_srgb8(255, 150, 0));
        assert_eq!(dimmed.to_srgb8(), [170, 100, 0]);
        assert!((dim(Rgba::WHITE.with_alpha(0.5)).a - 0.5).abs() < f32::EPSILON);
    }
}
