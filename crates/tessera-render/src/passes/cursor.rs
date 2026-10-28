//! Pass 4: the cursor.
//!
//! - [`CursorShape::Block`]: the theme cursor color over the cell, then the
//!   glyphs covering the cell redrawn in the cell's background color,
//!   clipped to the cell, so the cell reads as inverted. Both are scaled
//!   by the cursor color's alpha.
//! - [`CursorShape::Beam`]: a bar [`Layout::stroke`] pixels wide at the
//!   cell's left edge.
//! - [`CursorShape::Underline`]: a bar `stroke` pixels high at the cell's
//!   bottom edge.
//! - [`CursorShape::HollowBlock`] (and every shape while unfocused): the
//!   cell's outline, `stroke` pixels wide.
//!
//! A cursor that is absent, not visible, or outside the target draws
//! nothing. The cursor covers one column; wide cells get a two-column
//! cursor with issue #22.

use tessera_grid::Style;

use super::glyph::GlyphSource;
use super::{CellColors, Instance, Layout, f32_from};
use crate::error::RenderError;
use crate::frame::{CursorShape, FrameInput};
use crate::palette::Palette;
use crate::shaping::ShapedGlyph;
use crate::theme::Rgba;

/// Appends the cursor of `frame` to `out`; `shaped` gives a row's shaped
/// glyphs (the block cursor redraws the ones covering its cell).
pub(crate) fn push<'g>(
    out: &mut Vec<Instance>,
    layout: &Layout,
    palette: &Palette,
    color: Rgba,
    frame: &FrameInput<'_>,
    shaped: impl Fn(usize) -> Option<&'g [ShapedGlyph]>,
    source: &mut GlyphSource<'_>,
) -> Result<(), RenderError> {
    let Some((cursor, shape)) = frame.drawn_cursor() else {
        return Ok(());
    };
    let (row, col) = (usize::from(cursor.row), usize::from(cursor.col));
    if row >= layout.visible_rows() || col >= layout.visible_cols() {
        return Ok(());
    }
    let rect @ [x, y, w, h] = layout.cells_rect(row, col, 1);
    out.extend(
        shape_rects(rect, f32_from(layout.stroke), shape).map(|r| Instance::solid(r, color)),
    );
    if shape != CursorShape::Block {
        return Ok(());
    }
    // Redraw the glyphs covering the cell in its background color.
    let cells = frame.rows.get(row).copied().unwrap_or_default();
    let (Some(cell), Some(glyphs)) = (cells.get(col), shaped(row)) else {
        return Ok(());
    };
    // Scaled by the cursor's opacity: alpha 0 draws exactly nothing and
    // alpha 1 exactly inverts the cell. In between this approximates a
    // blend of the inverted and the plain cell; the cursor color stays
    // under the glyph, an excess of a·c·(1−a)·(cursor − plain) for
    // coverage c (ADR 0010).
    let tint = CellColors::background(palette, cell.style)
        .1
        .with_alpha(color.clamped().a);
    let clip = [x, y, x + w, y + h];
    let covering = glyphs
        .iter()
        .filter(|g| g.cell_index <= col && col < g.cell_index + g.cell_span);
    for glyph in covering {
        let hidden = cells
            .get(glyph.cell_index)
            .is_some_and(|owner| owner.style.has(Style::HIDDEN));
        if hidden {
            continue;
        }
        if let Some(instance) = source.instance(layout, row, glyph, tint, clip)? {
            out.push(instance);
        }
    }
    Ok(())
}

/// The solid rectangles of `shape` over the cell `[x, y, w, h]`, with
/// strokes `stroke` pixels wide (capped to the cell); unused entries are
/// empty and filtered out.
fn shape_rects(
    [x, y, w, h]: [f32; 4],
    stroke: f32,
    shape: CursorShape,
) -> impl Iterator<Item = [f32; 4]> {
    let (sx, sy) = (stroke.min(w / 2.0), stroke.min(h / 2.0));
    let rects = match shape {
        CursorShape::Block => [[x, y, w, h], [0.0; 4], [0.0; 4], [0.0; 4]],
        CursorShape::Beam => [[x, y, stroke.min(w), h], [0.0; 4], [0.0; 4], [0.0; 4]],
        CursorShape::Underline => {
            let height = stroke.min(h);
            [[x, y + h - height, w, height], [0.0; 4], [0.0; 4], [0.0; 4]]
        }
        CursorShape::HollowBlock => [
            [x, y, w, sy],
            [x, y + h - sy, w, sy],
            [x, y + sy, sx, h - 2.0 * sy],
            [x + w - sx, y + sy, sx, h - 2.0 * sy],
        ],
    };
    rects.into_iter().filter(|r| r[2] > 0.0 && r[3] > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: [f32; 4] = [16.0, 30.0, 16.0, 30.0];

    fn rects(shape: CursorShape, stroke: f32) -> Vec<[f32; 4]> {
        shape_rects(CELL, stroke, shape).collect()
    }

    #[test]
    fn block_fills_the_cell() {
        assert_eq!(rects(CursorShape::Block, 2.0), vec![CELL]);
    }

    #[test]
    fn beam_and_underline_are_one_stroke() {
        assert_eq!(rects(CursorShape::Beam, 2.0), vec![[16.0, 30.0, 2.0, 30.0]]);
        assert_eq!(
            rects(CursorShape::Underline, 2.0),
            vec![[16.0, 58.0, 16.0, 2.0]]
        );
    }

    #[test]
    fn hollow_block_is_four_non_overlapping_edges() {
        let edges = rects(CursorShape::HollowBlock, 2.0);
        assert_eq!(
            edges,
            vec![
                [16.0, 30.0, 16.0, 2.0],
                [16.0, 58.0, 16.0, 2.0],
                [16.0, 32.0, 2.0, 26.0],
                [30.0, 32.0, 2.0, 26.0],
            ]
        );
        let area: f32 = edges.iter().map(|r| r[2] * r[3]).sum();
        assert!((area - (16.0 * 30.0 - 12.0 * 26.0)).abs() < f32::EPSILON);
    }

    #[test]
    fn strokes_never_exceed_a_tiny_cell() {
        let tiny = [0.0, 0.0, 1.0, 1.0];
        for shape in [
            CursorShape::Block,
            CursorShape::Beam,
            CursorShape::Underline,
            CursorShape::HollowBlock,
        ] {
            for r in shape_rects(tiny, 8.0, shape) {
                assert!(r[0] >= 0.0 && r[1] >= 0.0, "{shape:?}: {r:?}");
                assert!(r[0] + r[2] <= 1.0 && r[1] + r[3] <= 1.0, "{shape:?}: {r:?}");
            }
        }
    }
}
