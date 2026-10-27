//! Pass 2: glyphs.
//!
//! Each [`ShapedGlyph`] becomes one quad over its atlas bitmap, placed at
//! its cell plus the shaper's whole-pixel offset and the bitmap's bearing.
//! Coverage glyphs are tinted with the owning cell's foreground (after
//! inverse and dim); colour glyphs (emoji) are drawn as they are. Cells
//! with `HIDDEN` draw no glyph. Bold and italic were already chosen by the
//! shaper (real faces or synthesis in the glyph key).

use tessera_grid::{Cell, Style};

use super::{CellColors, Instance, Layout, NO_CLIP, f32_from};
use crate::atlas::GlyphAtlas;
use crate::error::RenderError;
use crate::gpu::Gpu;
use crate::palette::Palette;
use crate::shaping::{ShapedGlyph, Shaper};

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
    /// The quad for `glyph` on row `row`, tinted `tint` and clipped to
    /// `clip`, or `None` when it draws nothing.
    ///
    /// A glyph too large for the atlas is skipped rather than failing the
    /// frame; [`RenderError::AtlasFull`] and GPU errors are returned.
    pub fn instance(
        &mut self,
        layout: &Layout,
        row: usize,
        glyph: &ShapedGlyph,
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
        Ok(Some(Instance::glyph(
            origin,
            &slot,
            self.atlas.size(),
            tint,
            clip,
        )))
    }
}

/// Appends the glyph quads of `rows` (shaped into `shaped`, one entry per
/// row) to `out`.
pub(crate) fn push(
    out: &mut Vec<Instance>,
    layout: &Layout,
    palette: &Palette,
    rows: &[&[Cell]],
    shaped: &[Vec<ShapedGlyph>],
    source: &mut GlyphSource<'_>,
) -> Result<(), RenderError> {
    let cols = layout.visible_cols();
    for (r, (row, glyphs)) in rows.iter().zip(shaped).enumerate() {
        for glyph in glyphs.iter().filter(|g| g.cell_index < cols) {
            let Some(cell) = row.get(glyph.cell_index) else {
                continue;
            };
            if cell.style.has(Style::HIDDEN) {
                continue;
            }
            let fg = CellColors::resolve(palette, cell.style).fg;
            if let Some(instance) = source.instance(layout, r, glyph, fg, NO_CLIP)? {
                out.push(instance);
            }
        }
    }
    Ok(())
}
