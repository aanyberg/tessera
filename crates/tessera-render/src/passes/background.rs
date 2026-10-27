//! Pass 1: cell backgrounds.
//!
//! The render pass clears the target to the theme background (with its
//! opacity), so only cells whose background differs are drawn: explicit
//! backgrounds and inverse video, both opaque. Horizontal runs of cells
//! with the same color become one quad.

use tessera_grid::Cell;

use super::{CellColors, Instance, Layout};
use crate::palette::Palette;

/// Appends the background quads of `rows` to `out`.
pub(crate) fn push(out: &mut Vec<Instance>, layout: &Layout, palette: &Palette, rows: &[&[Cell]]) {
    let cols = layout.visible_cols();
    for (r, row) in rows.iter().take(layout.visible_rows()).enumerate() {
        // The open run: first column and color.
        let mut run: Option<(usize, CellColors)> = None;
        let mut flush = |run: Option<(usize, CellColors)>, end: usize| {
            if let Some((start, colors)) = run {
                out.push(Instance::solid(
                    layout.cells_rect(r, start, end - start),
                    colors.bg,
                ));
            }
        };
        for (c, cell) in row.iter().take(cols).enumerate() {
            let colors = CellColors::resolve(palette, cell.style);
            let continues = run.is_some_and(|(_, open)| colors.paint_bg && open.bg == colors.bg);
            if continues {
                continue;
            }
            flush(run.take(), c);
            if colors.paint_bg {
                run = Some((c, colors));
            }
        }
        flush(run, row.len().min(cols));
    }
}

#[cfg(test)]
mod tests {
    use tessera_grid::{Color, Style};

    use super::*;
    use crate::shaping::CellMetrics;
    use crate::size::PhysicalSize;
    use crate::theme::Theme;

    fn layout(target: PhysicalSize) -> Layout {
        let metrics = CellMetrics {
            width_px: 10,
            height_px: 20,
            baseline_px: 15.0,
        };
        Layout::new(metrics, 1.0, 0.0, target)
    }

    fn bg(index: u8) -> Cell {
        Cell::new(
            'x',
            Style {
                bg: Color::Indexed(index),
                ..Style::DEFAULT
            },
        )
    }

    #[test]
    fn runs_of_equal_backgrounds_merge() {
        let palette = Palette::new(&Theme::default());
        let row = [Cell::BLANK, bg(1), bg(1), bg(2), Cell::BLANK, bg(2)];
        let rows: [&[Cell]; 1] = [&row];
        let mut out = Vec::new();
        push(
            &mut out,
            &layout(PhysicalSize::new(100, 20)),
            &palette,
            &rows,
        );
        let rects: Vec<_> = out.iter().map(|i| i.rect).collect();
        assert_eq!(
            rects,
            vec![
                [10.0, 0.0, 20.0, 20.0],
                [30.0, 0.0, 10.0, 20.0],
                [50.0, 0.0, 10.0, 20.0],
            ]
        );
    }

    #[test]
    fn cells_outside_the_target_are_skipped() {
        let palette = Palette::new(&Theme::default());
        let row = [bg(1); 8];
        let rows: [&[Cell]; 3] = [&row, &row, &row];
        let mut out = Vec::new();
        push(
            &mut out,
            &layout(PhysicalSize::new(35, 30)),
            &palette,
            &rows,
        );
        let rects: Vec<_> = out.iter().map(|i| i.rect).collect();
        assert_eq!(
            rects,
            vec![[0.0, 0.0, 40.0, 20.0], [0.0, 20.0, 40.0, 20.0]],
            "four partly visible columns, two rows"
        );
    }

    #[test]
    fn default_background_draws_nothing() {
        let palette = Palette::new(&Theme::default());
        let row = [Cell::BLANK; 4];
        let rows: [&[Cell]; 1] = [&row];
        let mut out = Vec::new();
        push(
            &mut out,
            &layout(PhysicalSize::new(40, 20)),
            &palette,
            &rows,
        );
        assert_eq!(out, Vec::new());
    }
}
