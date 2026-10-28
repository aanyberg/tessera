//! Pass 3: underline, double underline and strikethrough.
//!
//! Bars are solid quads in the cell's foreground color, one per run of
//! cells with the same decoration and color, so an underlined word is one
//! unbroken line. Hidden cells draw no decoration.
//!
//! The geometry comes from [`CellMetrics`]: the font's own underline
//! position and thickness are not exposed by the shaper (ADR 0010), and
//! deriving them from the cell keeps every bar inside its row.

use tessera_grid::{Cell, Style};

use super::{CellColors, Instance, Layout, f32_from};
use crate::palette::Palette;
use crate::shaping::CellMetrics;
use crate::theme::Rgba;

/// Where the decoration bars sit in a cell, in whole pixels from the cell
/// top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecorationGeometry {
    /// Height of every bar.
    pub thickness: u32,
    /// Top of the single underline.
    pub underline: u32,
    /// Tops of the two double-underline bars.
    pub double: [u32; 2],
    /// Top of the strikethrough bar.
    pub strike: u32,
}

impl DecorationGeometry {
    /// Geometry for cells of `metrics`.
    ///
    /// The thickness is one pixel per 16 pixels of cell height, rounded
    /// (at least one). The underline is centred in the space below the baseline,
    /// at least a pixel under it; the double underline is two bars one
    /// thickness apart, centred the same way; the strikethrough is centred
    /// at 30% of the baseline height above the baseline, about half the
    /// x-height of a monospaced face. Everything stays inside the cell.
    pub fn new(metrics: CellMetrics) -> Self {
        let height = metrics.height_px.max(1);
        let baseline = super::px_from(metrics.baseline_px).min(height);
        let thickness = ((height + 8) / 16).max(1);
        let below = height - baseline;
        let last = height.saturating_sub(thickness);
        let underline = (baseline + (below.saturating_sub(thickness) / 2).max(1)).min(last);
        let span = 3 * thickness;
        let double_top =
            (baseline + below.saturating_sub(span) / 2).min(height.saturating_sub(span));
        let double = [double_top, (double_top + 2 * thickness).min(last)];
        let mid = super::px_from(f32_from(baseline) * 0.3);
        let strike = baseline.saturating_sub(mid + thickness / 2).min(last);
        Self {
            thickness,
            underline,
            double,
            strike,
        }
    }

    /// Tops of the bars `style` asks for.
    fn bars(&self, style: Style) -> impl Iterator<Item = u32> + use<> {
        let double = style.has(Style::DOUBLE_UNDERLINE);
        let underline = style.has(Style::UNDERLINE) && !double;
        let strike = style.has(Style::STRIKETHROUGH);
        [
            underline.then_some(self.underline),
            double.then_some(self.double[0]),
            double.then_some(self.double[1]),
            strike.then_some(self.strike),
        ]
        .into_iter()
        .flatten()
    }
}

/// The decoration flags a run must share.
const DECORATIONS: u16 = Style::UNDERLINE | Style::DOUBLE_UNDERLINE | Style::STRIKETHROUGH;

/// Appends the decoration bars of `rows` to `out`.
#[cfg(test)]
pub(crate) fn push(out: &mut Vec<Instance>, layout: &Layout, palette: &Palette, rows: &[&[Cell]]) {
    for (r, row) in rows.iter().take(layout.visible_rows()).enumerate() {
        push_row(out, layout, palette, r, row);
    }
}

/// Appends the decoration bars of row `r`, holding `row`, to `out`.
pub(crate) fn push_row(
    out: &mut Vec<Instance>,
    layout: &Layout,
    palette: &Palette,
    r: usize,
    row: &[Cell],
) {
    let cols = layout.visible_cols();
    {
        let mut run: Option<(usize, Style, Rgba)> = None;
        let mut flush = |run: Option<(usize, Style, Rgba)>, end: usize| {
            let Some((start, style, color)) = run else {
                return;
            };
            let [x, y, w, _] = layout.cells_rect(r, start, end - start);
            let geometry = &layout.decoration;
            for top in geometry.bars(style) {
                out.push(Instance::solid(
                    [x, y + f32_from(top), w, f32_from(geometry.thickness)],
                    color,
                ));
            }
        };
        for (c, cell) in row.iter().take(cols).enumerate() {
            let decorated = cell.style.flags & DECORATIONS != 0 && !cell.style.has(Style::HIDDEN);
            let key = decorated.then(|| {
                let fg = CellColors::foreground(palette, cell.style);
                (cell.style.flags & DECORATIONS, fg)
            });
            let continues = match (run, key) {
                (Some((_, style, color)), Some((flags, fg))) => {
                    style.flags & DECORATIONS == flags && color == fg
                }
                _ => false,
            };
            if continues {
                continue;
            }
            flush(run.take(), c);
            if let Some((flags, fg)) = key {
                let style = Style {
                    flags,
                    ..Style::DEFAULT
                };
                run = Some((c, style, fg));
            }
        }
        flush(run, row.len().min(cols));
    }
}

#[cfg(test)]
mod tests {
    use tessera_grid::Color;

    use super::*;
    use crate::size::PhysicalSize;
    use crate::theme::Theme;

    fn metrics(height_px: u32, baseline_px: f32) -> CellMetrics {
        CellMetrics {
            width_px: 8,
            height_px,
            baseline_px,
        }
    }

    #[test]
    fn bars_stay_inside_the_cell() {
        for height in 1..=80 {
            for baseline in 0..=height {
                #[expect(clippy::cast_precision_loss, reason = "small")]
                let g = DecorationGeometry::new(metrics(height, baseline as f32));
                let bottom = |top: u32| top + g.thickness;
                assert!(g.thickness >= 1);
                if g.thickness <= height {
                    for top in [g.underline, g.double[0], g.double[1], g.strike] {
                        assert!(bottom(top) <= height, "{height}/{baseline}: {g:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn retina_menlo_geometry() {
        // Menlo 13 pt at 2x: 16x30 cells, baseline 24.
        let g = DecorationGeometry::new(metrics(30, 24.0));
        assert_eq!(g.thickness, 2);
        assert_eq!(g.underline, 26, "centred in the 6 px below the baseline");
        assert_eq!(g.double, [24, 28]);
        assert_eq!(g.strike, 16, "about half the x-height above the baseline");
        // Menlo 13 pt at 1x: 8x15 cells, baseline 12.
        let g = DecorationGeometry::new(metrics(15, 12.0));
        assert_eq!(g.thickness, 1);
        assert_eq!((g.underline, g.double, g.strike), (13, [12, 14], 8));
    }

    #[test]
    fn underlined_runs_are_one_bar_per_color() {
        let palette = Palette::new(&Theme::default());
        let u = |fg| {
            Cell::new(
                'a',
                Style {
                    fg,
                    flags: Style::UNDERLINE,
                    ..Style::DEFAULT
                },
            )
        };
        let hidden = Cell::new('a', Style::DEFAULT.with(Style::UNDERLINE | Style::HIDDEN));
        let row = [
            u(Color::Default),
            u(Color::Default),
            u(Color::Indexed(1)),
            hidden,
            Cell::new('a', Style::DEFAULT.with(Style::DOUBLE_UNDERLINE)),
            Cell::new('a', Style::DEFAULT.with(Style::STRIKETHROUGH)),
        ];
        let rows: [&[Cell]; 1] = [&row];
        let layout = Layout::new(metrics(30, 24.0), 1.0, 0.0, PhysicalSize::new(80, 30));
        let mut out = Vec::new();
        push(&mut out, &layout, &palette, &rows);
        let rects: Vec<_> = out.iter().map(|i| i.rect).collect();
        assert_eq!(
            rects,
            vec![
                [0.0, 26.0, 16.0, 2.0],
                [16.0, 26.0, 8.0, 2.0],
                [32.0, 24.0, 8.0, 2.0],
                [32.0, 28.0, 8.0, 2.0],
                [40.0, 16.0, 8.0, 2.0],
            ]
        );
    }
}
