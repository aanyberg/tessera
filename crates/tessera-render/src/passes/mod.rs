//! The four grid passes and the instance data they share.
//!
//! Every pass emits instanced quads into one [`Instance`] buffer that is
//! rebuilt each frame; the passes are consecutive ranges of it, drawn in
//! order with one pipeline (`shaders/quad.wgsl`):
//!
//! 1. [`background`]: one quad per run of cells whose background is not
//!    the theme default (the target is cleared to that first);
//! 2. [`glyph`]: one quad per shaped glyph, tinted coverage or
//!    premultiplied colour;
//! 3. [`decoration`]: underline, double underline and strikethrough bars;
//! 4. [`cursor`]: the cursor shape, plus the cursor cell's glyph redrawn in
//!    the background color under a block cursor.
//!
//! Coordinates are physical pixels from the target's top-left corner.
//! See ADR 0010 for the layout and the color pipeline.

pub(crate) mod background;
pub(crate) mod cursor;
pub(crate) mod decoration;
pub(crate) mod glyph;

use std::ops::Range;

use bytemuck::{Pod, Zeroable};
use tessera_grid::{Color, Style};

use crate::atlas::{AtlasLayer, AtlasSlot};
use crate::palette::{ColorSlot, Palette};
use crate::shaping::CellMetrics;
use crate::size::PhysicalSize;
use crate::theme::Rgba;

pub(crate) use decoration::DecorationGeometry;

/// What the fragment shader does with an [`Instance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Kind {
    /// Fill the rectangle with `color`.
    Solid = 0,
    /// Multiply `color` by coverage loaded from the alpha atlas.
    AlphaGlyph = 1,
    /// Premultiplied colour loaded from the colour atlas, times `color.a`.
    ColorGlyph = 2,
}

/// One quad: 64 bytes, shared by every pass.
///
/// Matches `InstanceIn` in `shaders/quad.wgsl`; [`Instance::ATTRIBUTES`]
/// is the vertex layout.
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
#[repr(C)]
pub(crate) struct Instance {
    /// `[x, y, width, height]` in pixels.
    pub rect: [f32; 4],
    /// `[x0, y0, x1, y1]`: fragments outside are discarded.
    pub clip: [f32; 4],
    /// Linear, premultiplied RGBA. For glyphs, the tint (alpha layer) or
    /// the opacity (colour layer).
    pub color: [f32; 4],
    /// Atlas texel of the rectangle's top-left corner (glyphs only).
    pub texel: [f32; 2],
    /// A [`Kind`].
    pub kind: u32,
    /// Keeps the struct a multiple of 16 bytes.
    pub pad: u32,
}

impl Instance {
    /// The vertex attributes, locations 0..=4.
    pub const ATTRIBUTES: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
        0 => Float32x4,
        1 => Float32x4,
        2 => Float32x4,
        3 => Float32x2,
        4 => Uint32,
    ];

    /// A solid rectangle of `color` (straight alpha), unclipped.
    pub fn solid(rect: [f32; 4], color: Rgba) -> Self {
        Self {
            rect,
            clip: NO_CLIP,
            color: color.premultiplied(),
            texel: [0.0; 2],
            kind: Kind::Solid as u32,
            pad: 0,
        }
    }

    /// A glyph bitmap at `origin` (its top-left pixel), clipped to `clip`.
    /// A coverage glyph is tinted with `tint`; a colour glyph ignores the
    /// tint's color. Either way `tint.a` scales the glyph's opacity (1 in
    /// the glyph pass, the cursor's opacity for the block cursor redraw).
    pub fn glyph(
        origin: [f32; 2],
        slot: &AtlasSlot,
        atlas_size: u32,
        tint: Rgba,
        clip: [f32; 4],
    ) -> Self {
        let texel = |uv: f32| (uv * f32_from(atlas_size)).round();
        let kind = match slot.layer {
            AtlasLayer::Alpha => Kind::AlphaGlyph,
            AtlasLayer::Color => Kind::ColorGlyph,
        };
        Self {
            rect: [
                origin[0],
                origin[1],
                f32_from(slot.size[0]),
                f32_from(slot.size[1]),
            ],
            clip,
            color: tint.premultiplied(),
            texel: [texel(slot.uv[0]), texel(slot.uv[1])],
            kind: kind as u32,
            pad: 0,
        }
    }
}

/// A clip rectangle that clips nothing.
pub(crate) const NO_CLIP: [f32; 4] = [f32::MIN, f32::MIN, f32::MAX, f32::MAX];

/// Where each pass's instances sit in the frame's buffer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PassRanges {
    pub background: Range<u32>,
    pub glyph: Range<u32>,
    pub decoration: Range<u32>,
    pub cursor: Range<u32>,
}

impl PassRanges {
    /// The four passes with their debug labels, in draw order.
    pub fn labelled(&self) -> [(&'static str, Range<u32>); 4] {
        [
            ("tessera.background", self.background.clone()),
            ("tessera.glyph", self.glyph.clone()),
            ("tessera.decoration", self.decoration.clone()),
            ("tessera.cursor", self.cursor.clone()),
        ]
    }
}

/// The pixel geometry of the grid in one target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Layout {
    /// Top-left of cell (0, 0): the theme padding in whole pixels.
    pub origin: [u32; 2],
    /// Cell width and height.
    pub cell: [u32; 2],
    /// Baseline from the cell top, whole pixels.
    pub baseline: u32,
    /// The target's size.
    pub target: PhysicalSize,
    /// Width of beam and outline strokes and height of the underline
    /// cursor: one pixel per point of backing scale.
    pub stroke: u32,
    /// Where decorations go inside a cell.
    pub decoration: DecorationGeometry,
}

impl Layout {
    /// The layout for `metrics` at backing `scale` with `padding` logical
    /// pixels around the grid, in a `target`-sized texture.
    pub fn new(metrics: CellMetrics, scale: f32, padding: f32, target: PhysicalSize) -> Self {
        let pad = px_from(padding * scale);
        Self {
            origin: [pad, pad],
            cell: [metrics.width_px.max(1), metrics.height_px.max(1)],
            baseline: px_from(metrics.baseline_px),
            target,
            stroke: px_from(scale).max(1),
            decoration: DecorationGeometry::new(metrics),
        }
    }

    /// How many rows start inside the target.
    pub fn visible_rows(&self) -> usize {
        visible(self.target.height, self.origin[1], self.cell[1])
    }

    /// How many columns start inside the target.
    pub fn visible_cols(&self) -> usize {
        visible(self.target.width, self.origin[0], self.cell[0])
    }

    /// Top-left pixel of cell `(row, col)`.
    pub fn cell_origin(&self, row: usize, col: usize) -> [f32; 2] {
        [
            f32_from(self.origin[0]) + f32_from_usize(col) * f32_from(self.cell[0]),
            f32_from(self.origin[1]) + f32_from_usize(row) * f32_from(self.cell[1]),
        ]
    }

    /// `[x, y, w, h]` of `cols` cells starting at `(row, col)`.
    pub fn cells_rect(&self, row: usize, col: usize, cols: usize) -> [f32; 4] {
        let [x, y] = self.cell_origin(row, col);
        [
            x,
            y,
            f32_from_usize(cols) * f32_from(self.cell[0]),
            f32_from(self.cell[1]),
        ]
    }
}

/// Number of `cell`-sized steps from `origin` that start before `extent`.
fn visible(extent: u32, origin: u32, cell: u32) -> usize {
    let cells = extent.saturating_sub(origin).div_ceil(cell.max(1));
    usize::try_from(cells).unwrap_or(usize::MAX)
}

/// A cell's resolved colors, after inverse video and dim.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CellColors {
    /// Glyph and decoration color, always opaque.
    pub fg: Rgba,
    /// Background color: opaque unless it is the theme default.
    pub bg: Rgba,
    /// Whether the background differs from the cleared target, so the
    /// background pass must paint it.
    pub paint_bg: bool,
}

impl CellColors {
    /// Resolves `style` against `palette`.
    ///
    /// Inverse video swaps the resolved colors; a background that is not
    /// the theme default (explicit, or swapped in by inverse) is opaque,
    /// and so is every foreground. Dim applies to the final foreground.
    #[cfg(test)]
    pub fn resolve(palette: &Palette, style: Style) -> Self {
        let (paint_bg, bg) = Self::background(palette, style);
        Self {
            fg: Self::foreground(palette, style),
            bg,
            paint_bg,
        }
    }

    /// The glyph and decoration color of `style` alone (what
    /// [`Self::resolve`] gives as `fg`), for the passes that need nothing
    /// else.
    pub fn foreground(palette: &Palette, style: Style) -> Rgba {
        let (color, slot) = if style.has(Style::INVERSE) {
            (style.bg, ColorSlot::Background)
        } else {
            (style.fg, ColorSlot::Foreground)
        };
        if style.has(Style::DIM) {
            palette.resolve_dim(color, slot)
        } else {
            palette.resolve(color, slot).with_alpha(1.0)
        }
    }

    /// Whether the background pass paints `style`'s background, and its
    /// color (what [`Self::resolve`] gives as `paint_bg` and `bg`).
    pub fn background(palette: &Palette, style: Style) -> (bool, Rgba) {
        if style.has(Style::INVERSE) {
            let fg = palette.resolve(style.fg, ColorSlot::Foreground);
            (true, fg.with_alpha(1.0))
        } else if style.bg == Color::Default {
            (false, palette.background())
        } else {
            let bg = palette.resolve(style.bg, ColorSlot::Background);
            (true, bg.with_alpha(1.0))
        }
    }
}

/// `v` as `f32`; pixel coordinates are far below 2^24.
pub(crate) fn f32_from(v: u32) -> f32 {
    #[expect(clippy::cast_precision_loss, reason = "pixel coordinates are small")]
    let f = v as f32;
    f
}

/// `v` as `f32`; cell counts are far below 2^24.
pub(crate) fn f32_from_usize(v: usize) -> f32 {
    #[expect(clippy::cast_precision_loss, reason = "cell counts are small")]
    let f = v as f32;
    f
}

/// `v` rounded to whole pixels; negative and `NaN` become 0.
pub(crate) fn px_from(v: f32) -> u32 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "`as` saturates; NaN and negatives map to 0"
    )]
    let px = v.round().max(0.0) as u32;
    px
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn metrics() -> CellMetrics {
        CellMetrics {
            width_px: 16,
            height_px: 30,
            baseline_px: 24.0,
        }
    }

    #[test]
    fn instance_is_64_bytes() {
        assert_eq!(std::mem::size_of::<Instance>(), 64);
        let end = Instance::ATTRIBUTES[4].offset + 4;
        assert_eq!(end, 60, "kind ends before the padding word");
    }

    #[test]
    fn layout_counts_partially_visible_cells() {
        let layout = Layout::new(
            metrics(),
            2.0,
            4.0,
            PhysicalSize::new(8 + 16 * 3 + 1, 8 + 30),
        );
        assert_eq!(layout.origin, [8, 8]);
        assert_eq!(layout.visible_cols(), 4);
        assert_eq!(layout.visible_rows(), 1);
        assert_eq!(layout.stroke, 2);
        assert_eq!(layout.cells_rect(1, 2, 3), [40.0, 38.0, 48.0, 30.0]);
        let tiny = Layout::new(metrics(), 1.0, 10.0, PhysicalSize::new(5, 5));
        assert_eq!((tiny.visible_cols(), tiny.visible_rows()), (0, 0));
    }

    #[test]
    fn padding_and_stroke_survive_bad_scales() {
        let layout = Layout::new(metrics(), f32::NAN, f32::NAN, PhysicalSize::new(10, 10));
        assert_eq!(layout.origin, [0, 0]);
        assert_eq!(layout.stroke, 1);
    }

    #[test]
    fn inverse_swaps_and_makes_both_opaque() {
        let theme = Theme {
            background: Rgba::new(0.0, 0.0, 0.0, 0.5),
            ..Theme::default()
        };
        let palette = Palette::new(&theme);
        let plain = CellColors::resolve(&palette, Style::DEFAULT);
        assert!(!plain.paint_bg);
        assert_eq!(plain.bg, theme.background, "default bg keeps its opacity");
        let inverse = CellColors::resolve(&palette, Style::DEFAULT.with(Style::INVERSE));
        assert!(inverse.paint_bg);
        assert_eq!(inverse.bg, theme.foreground);
        assert_eq!(inverse.fg, theme.background.with_alpha(1.0));
        let explicit = CellColors::resolve(
            &palette,
            Style {
                bg: Color::Indexed(0),
                ..Style::DEFAULT
            },
        );
        assert!(explicit.paint_bg, "an explicit bg is painted even if equal");
        assert!((explicit.bg.a - 1.0).abs() < f32::EPSILON);
    }

    /// The split lookups give exactly what resolving both colors, swapping
    /// for inverse and dimming afterwards gave (#20's formula).
    #[test]
    fn split_lookups_match_the_reference_formula() {
        let theme = Theme {
            background: Rgba::new(0.2, 0.1, 0.0, 0.5),
            ..Theme::default()
        };
        let palette = Palette::new(&theme);
        let reference = |style: Style| {
            let fg = palette.resolve(style.fg, ColorSlot::Foreground);
            let bg = palette.resolve(style.bg, ColorSlot::Background);
            let inverse = style.has(Style::INVERSE);
            let (fg, bg) = if inverse { (bg, fg) } else { (fg, bg) };
            let paint_bg = inverse || style.bg != Color::Default;
            let fg = fg.with_alpha(1.0);
            let fg = if style.has(Style::DIM) {
                crate::palette::dim(fg)
            } else {
                fg
            };
            let bg = if paint_bg { bg.with_alpha(1.0) } else { bg };
            CellColors { fg, bg, paint_bg }
        };
        let colors = [
            Color::Default,
            Color::Indexed(3),
            Color::Indexed(200),
            Color::Rgb(250, 128, 7),
        ];
        for fg in colors {
            for bg in colors {
                for flags in [0, Style::INVERSE, Style::DIM, Style::INVERSE | Style::DIM] {
                    let style = Style {
                        fg,
                        bg,
                        flags,
                        ..Style::DEFAULT
                    };
                    assert_eq!(
                        CellColors::resolve(&palette, style),
                        reference(style),
                        "{style:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn dim_darkens_the_final_foreground() {
        let palette = Palette::new(&Theme::default());
        let style = Style {
            fg: Color::Rgb(255, 255, 255),
            ..Style::DEFAULT
        };
        let dimmed = CellColors::resolve(&palette, style.with(Style::DIM));
        assert_eq!(dimmed.fg.to_srgb8(), [170, 170, 170]);
    }
}
