//! Pass 5: the cursor.
//!
//! - [`CursorShape::Block`]: the theme cursor color over the cell, then
//!   what the cell shows redrawn in the cell's background color, dimmed
//!   for a DIM cell (the cursor's text color), clipped to the cursor, so
//!   the cell reads as
//!   inverted. Both are scaled by the cursor color's alpha. Redrawn are
//!   every glyph that reaches into the cursor, logically (its cells
//!   include a cursor cell: the whole ligature under a cursor on its
//!   second cell, the whole wide character) or by its ink (a ligature
//!   whose last character carries a mark of its own, an italic overhang),
//!   and the cell's underlines, overline and strikethrough, and a hovered
//!   link's underline. Colour glyphs (emoji) keep their colors.
//! - [`CursorShape::Beam`]: a bar [`Layout::stroke`] pixels wide at the
//!   cell's left edge.
//! - [`CursorShape::Underline`]: a bar `stroke` pixels high at the cell's
//!   bottom edge.
//! - [`CursorShape::HollowBlock`] (and every shape while unfocused): the
//!   cell's outline, `stroke` pixels wide.
//!
//! On either half of a wide character the cursor covers both columns. A
//! cursor that is absent, not visible, or outside the target draws
//! nothing.

use tessera_grid::{Cell, Style};

use super::decoration::DECORATIONS;
use super::glyph::GlyphSource;
use super::overlay::{Mark, Span};
use super::{CellColors, Instance, Layout, f32_from};
use crate::error::RenderError;
use crate::frame::{CursorShape, FrameInput};
use crate::palette::Palette;
use crate::shaping::ShapedGlyph;
use crate::theme::Rgba;

/// How many cells past its own a glyph's ink may reach and still be
/// considered for the block cursor's redraw (italic overhang, a ligature
/// drawn by its first character).
const INK_REACH: usize = 4;

/// The columns the cursor at `col` covers in `cells`, of which `cols` are
/// visible: `(first, count)`, two for either half of a wide character
/// whose two halves are visible, else one.
pub(crate) fn cursor_cells(cells: &[Cell], col: usize, cols: usize) -> (usize, usize) {
    let at = |c: usize| cells.get(c);
    let first = if at(col).is_some_and(Cell::is_spacer)
        && col > 0
        && at(col - 1).is_some_and(Cell::is_wide)
    {
        col - 1
    } else {
        col
    };
    let wide = at(first).is_some_and(Cell::is_wide)
        && at(first + 1).is_some_and(Cell::is_spacer)
        && first + 1 < cols;
    (first, if wide { 2 } else { 1 })
}

/// True when the rectangles `[x, y, w, h]` overlap.
fn overlaps(a: [f32; 4], b: [f32; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

/// Appends the cursor of `frame` to `out`; `row_work` gives a row's shaped
/// glyphs and highlight spans (the block cursor redraws the glyphs and
/// link underlines covering its cells).
pub(crate) fn push<'g>(
    out: &mut Vec<Instance>,
    layout: &Layout,
    palette: &Palette,
    color: Rgba,
    frame: &FrameInput<'_>,
    row_work: impl Fn(usize) -> Option<(&'g [ShapedGlyph], &'g [Span])>,
    source: &mut GlyphSource<'_>,
) -> Result<(), RenderError> {
    let Some((cursor, shape)) = frame.drawn_cursor() else {
        return Ok(());
    };
    let (row, col) = (usize::from(cursor.row), usize::from(cursor.col));
    let cols = layout.visible_cols();
    if row >= layout.visible_rows() || col >= cols {
        return Ok(());
    }
    let cells = frame.rows.get(row).copied().unwrap_or_default();
    let (first, count) = cursor_cells(cells, col, cols);
    let rect @ [x, y, w, h] = layout.cells_rect(row, first, count);
    out.extend(
        shape_rects(rect, f32_from(layout.stroke), shape).map(|r| Instance::solid(r, color)),
    );
    if shape != CursorShape::Block {
        return Ok(());
    }
    let Some(cell) = cells.get(first) else {
        return Ok(());
    };
    // The cursor's text color, scaled by its opacity: alpha 0 draws
    // exactly nothing and alpha 1 exactly inverts the cell. In between
    // this approximates a blend of the inverted and the plain cell; the
    // cursor color stays under the glyph, an excess of
    // a·c·(1−a)·(cursor − plain) for coverage c (ADR 0010).
    let tint = CellColors::cursor_text(palette, cell.style).with_alpha(color.clamped().a);
    let clip = [x, y, x + w, y + h];
    let (glyphs, spans) = row_work(row).unwrap_or_default();
    let end = first + count;
    let near =
        |g: &&ShapedGlyph| g.cell_index < end + 1 && g.cell_index + g.cell_span + INK_REACH > first;
    for glyph in glyphs.iter().filter(near) {
        let Some(owner) = cells.get(glyph.cell_index) else {
            continue;
        };
        if owner.style.has(Style::HIDDEN) {
            continue;
        }
        let logical = glyph.cell_index < end && first < glyph.cell_index + glyph.cell_span;
        let Some(instance) = source.instance(layout, row, (glyph, owner.style), tint, clip)? else {
            continue;
        };
        if logical || overlaps(instance.rect, rect) {
            out.push(instance);
        }
    }
    if cell.style.flags & DECORATIONS != 0 && !cell.style.has(Style::HIDDEN) {
        layout
            .decoration
            .push(out, rect, cell.style, (tint, tint), clip);
    }
    // A hovered link's underline (overlay pass) under the cursor.
    let (first_col, last_col) = (first, first + count - 1);
    let linked = spans.iter().any(|s| {
        s.mark == Mark::Underline
            && usize::from(s.first) <= last_col
            && first_col <= usize::from(s.last)
    });
    if linked {
        let bar = super::overlay::underline_rect(layout, row, first, count);
        out.push(Instance::solid(bar, tint).clipped(clip));
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

    fn wide_row() -> Vec<Cell> {
        use tessera_grid::CellFlags;
        let mut row = vec![Cell::new('a', Style::DEFAULT); 4];
        row[1] = Cell {
            flags: CellFlags::WIDE,
            ..Cell::new('中', Style::DEFAULT)
        };
        row[2] = Cell {
            flags: CellFlags::SPACER,
            ..Cell::BLANK
        };
        row
    }

    #[test]
    fn a_cursor_on_either_half_of_a_wide_character_covers_both() {
        let row = wide_row();
        assert_eq!(cursor_cells(&row, 0, 4), (0, 1));
        assert_eq!(cursor_cells(&row, 1, 4), (1, 2), "on the wide half");
        assert_eq!(cursor_cells(&row, 2, 4), (1, 2), "on the spacer");
        assert_eq!(cursor_cells(&row, 3, 4), (3, 1));
        assert_eq!(cursor_cells(&row, 9, 4), (9, 1), "past the row");
        assert_eq!(cursor_cells(&row, 1, 2), (1, 1), "spacer past the edge");
        // A stray spacer with no wide cell before it stays one column.
        let mut stray = row.clone();
        stray[1] = Cell::BLANK;
        assert_eq!(cursor_cells(&stray, 2, 4), (2, 1));
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
