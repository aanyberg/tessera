//! Pass 3: glyphs.
//!
//! Each [`ShapedGlyph`] becomes one quad over its atlas bitmap, placed at
//! its cell plus the shaper's whole-pixel offset and the bitmap's bearing.
//! Coverage glyphs are tinted with the owning cell's foreground (after
//! inverse and dim); colour glyphs (emoji) are drawn as they are. Cells
//! with `HIDDEN` draw no glyph. Bold and italic were already chosen by the
//! shaper (real faces or synthesis in the glyph key).
//!
//! Superscript and subscript (SGR 73/74) reuse the glyph's bitmap at
//! [`SCRIPT_SCALE`] of its size, scaled about the horizontal centre of the
//! glyph's cells and about the baseline, then moved up by
//! [`SUPERSCRIPT_RISE`] or down by [`SUBSCRIPT_DROP`] of the cell height,
//! clipped to their row. Nothing is rasterised again; the shader filters
//! the bitmap bilinearly itself. A cell with both flags draws superscript
//! (mintty).

use tessera_grid::{Cell, Style};

use super::{CellColors, Instance, Layout, NO_CLIP, f32_from, f32_from_usize};
use crate::atlas::GlyphAtlas;
use crate::error::RenderError;
use crate::gpu::Gpu;
use crate::palette::Palette;
use crate::shaping::{ShapedGlyph, Shaper};

/// Size of a superscript or subscript glyph relative to the regular one.
pub(crate) const SCRIPT_SCALE: f32 = 0.7;

/// How far a superscript glyph moves up, in cell heights.
pub(crate) const SUPERSCRIPT_RISE: f32 = 0.3;

/// How far a subscript glyph's baseline moves down, in cell heights.
pub(crate) const SUBSCRIPT_DROP: f32 = 0.2;

/// Where a superscript or subscript glyph whose regular quad starts at
/// `origin` is drawn: the new top-left corner, for a quad
/// [`SCRIPT_SCALE`] times the size. `centre_x` is the horizontal centre of
/// the glyph's cells, `baseline_y` the row's baseline and `cell_height`
/// the row's height, all in pixels. `None` when `style` is neither.
pub(crate) fn script_origin(
    style: Style,
    origin: [f32; 2],
    centre_x: f32,
    baseline_y: f32,
    cell_height: f32,
) -> Option<[f32; 2]> {
    let shift = if style.has(Style::SUPERSCRIPT) {
        -SUPERSCRIPT_RISE * cell_height
    } else if style.has(Style::SUBSCRIPT) {
        SUBSCRIPT_DROP * cell_height
    } else {
        return None;
    };
    let x = centre_x + (origin[0] - centre_x) * SCRIPT_SCALE;
    let y = baseline_y + (origin[1] - baseline_y) * SCRIPT_SCALE + shift;
    Some([x.round(), y.round()])
}

/// `instance` (a glyph of a cell with `style`, its cells starting at
/// `cell`, its regular quad at `origin`) drawn as superscript or
/// subscript. Out of line, so the regular glyph path stays as small as it
/// was before scripts existed.
#[cold]
#[inline(never)]
fn scripted(
    instance: Instance,
    layout: &Layout,
    (glyph, style): (&ShapedGlyph, Style),
    [cell_x, cell_y]: [f32; 2],
    origin: [f32; 2],
) -> Instance {
    let span = f32_from_usize(glyph.cell_span.max(1)) * f32_from(layout.cell[0]);
    let at = script_origin(
        style,
        origin,
        cell_x + span / 2.0,
        cell_y + f32_from(layout.baseline),
        f32_from(layout.cell[1]),
    );
    // Raised and lowered glyphs stay in their row: a subscript's
    // descender or a superscript's accent would otherwise paint into the
    // neighbouring row, which redraws only when it is damaged itself.
    let row = [
        f32::MIN,
        cell_y,
        f32::MAX,
        cell_y + f32_from(layout.cell[1]),
    ];
    at.map_or(instance, |at| instance.resized(at, SCRIPT_SCALE))
        .clipped(row)
}

/// What the glyph quads need besides the instance buffer.
pub(crate) struct GlyphSource<'a> {
    /// Produces the atlas keys.
    pub shaper: &'a Shaper,
    /// Holds the bitmaps.
    pub atlas: &'a mut GlyphAtlas,
    /// For uploads on an atlas miss.
    pub gpu: &'a Gpu,
}

impl GlyphSource<'_> {
    /// The quad for `glyph` on row `row`, owned by a cell of `style`
    /// (superscript and subscript are drawn smaller), tinted `tint` and
    /// clipped to `clip`, or `None` when it draws nothing.
    ///
    /// A glyph too large for the atlas is skipped rather than failing the
    /// frame; [`RenderError::AtlasFull`] and GPU errors are returned.
    pub fn instance(
        &mut self,
        layout: &Layout,
        row: usize,
        (glyph, style): (&ShapedGlyph, Style),
        tint: crate::theme::Rgba,
        clip: [f32; 4],
    ) -> Result<Option<Instance>, RenderError> {
        let key = self.shaper.glyph_key(glyph);
        let slot = match self.atlas.get_or_insert(self.gpu, key) {
            Ok(slot) => slot,
            Err(RenderError::GlyphTooLarge { width, height }) => {
                tracing::debug!(
                    target: "tessera_render",
                    width,
                    height,
                    "glyph too large for the atlas; not drawn"
                );
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        if slot.is_empty() {
            return Ok(None);
        }
        let [cell_x, cell_y] = layout.cell_origin(row, glyph.cell_index);
        let (whole, _) = glyph.split_x_offset();
        #[expect(
            clippy::cast_precision_loss,
            reason = "glyph offsets are a few hundred pixels at most"
        )]
        let origin = [
            cell_x + (whole + slot.offset[0]) as f32,
            cell_y + f32_from(layout.baseline) - glyph.y_offset - slot.offset[1] as f32,
        ];
        let instance = Instance::glyph(origin, &slot, self.atlas.size(), tint, clip);
        if style.flags & (Style::SUPERSCRIPT | Style::SUBSCRIPT) == 0 {
            return Ok(Some(instance));
        }
        Ok(Some(scripted(
            instance,
            layout,
            (glyph, style),
            [cell_x, cell_y],
            origin,
        )))
    }
}

/// Appends the glyph quads of row `r`, holding `row` shaped into
/// `glyphs`, to `out`.
pub(crate) fn push_row(
    out: &mut Vec<Instance>,
    layout: &Layout,
    palette: &Palette,
    r: usize,
    (row, glyphs): (&[Cell], &[ShapedGlyph]),
    source: &mut GlyphSource<'_>,
) -> Result<(), RenderError> {
    let cols = layout.visible_cols();
    {
        for glyph in glyphs.iter().filter(|g| g.cell_index < cols) {
            let Some(cell) = row.get(glyph.cell_index) else {
                continue;
            };
            if cell.style.has(Style::HIDDEN) {
                continue;
            }
            let fg = CellColors::foreground(palette, cell.style);
            if let Some(instance) = source.instance(layout, r, (glyph, cell.style), fg, NO_CLIP)? {
                out.push(instance);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL_H: f32 = 30.0;

    /// A glyph whose regular quad is 10 x 20 at (3, 4) in a 16 x 30 cell
    /// at the origin with its baseline at 24.
    fn place(flags: u16) -> Option<[f32; 2]> {
        script_origin(Style::DEFAULT.with(flags), [3.0, 4.0], 8.0, 24.0, CELL_H)
    }

    #[test]
    fn regular_glyphs_are_not_moved() {
        assert_eq!(place(0), None);
        assert_eq!(place(Style::BOLD | Style::OVERLINE), None);
    }

    #[test]
    fn superscript_shrinks_about_the_centre_and_rises() {
        // x: 8 + (3 - 8) * 0.7 = 4.5 -> 5 (rounded half away from zero).
        // y: 24 + (4 - 24) * 0.7 - 9 = 1.
        assert_eq!(place(Style::SUPERSCRIPT), Some([5.0, 1.0]));
        let [x, _] = place(Style::SUPERSCRIPT).unwrap_or_default();
        let width = 10.0 * SCRIPT_SCALE;
        assert!((x + width / 2.0 - 8.0).abs() <= 0.5, "still centred");
    }

    #[test]
    fn subscript_drops_its_baseline_by_a_fifth_of_the_cell() {
        // y: 24 + (4 - 24) * 0.7 + 6 = 16; the scaled glyph's baseline
        // (14 px below its top) is at 30 = 24 + 0.2 * 30.
        assert_eq!(place(Style::SUBSCRIPT), Some([5.0, 16.0]));
        let bottom = 16.0 + 20.0 * SCRIPT_SCALE;
        assert!((bottom - (24.0 + SUBSCRIPT_DROP * CELL_H)).abs() < 0.01);
    }

    #[test]
    fn superscript_wins_over_subscript() {
        assert_eq!(
            place(Style::SUPERSCRIPT | Style::SUBSCRIPT),
            place(Style::SUPERSCRIPT)
        );
    }

    #[test]
    fn resized_instances_sample_the_whole_bitmap() {
        let base = Instance {
            rect: [3.0, 4.0, 10.0, 20.0],
            ..crate::damage::EMPTY
        };
        let small = base.resized([5.0, 1.0], SCRIPT_SCALE);
        assert_eq!(small.rect[..2], [5.0, 1.0]);
        assert!((small.rect[2] - 7.0).abs() < 1e-4 && (small.rect[3] - 14.0).abs() < 1e-4);
        assert!(
            (small.rect[2] * small.extra - 10.0).abs() < 1e-4,
            "10 texels across"
        );
    }
}
