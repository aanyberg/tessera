//! Pass 4: underlines, overline and strikethrough.
//!
//! Bars are quads, one per run of cells with the same decorations and
//! colors, so an underlined word is one unbroken line. Hidden cells draw
//! no decoration.
//!
//! - Underlines come in the five [`UnderlineStyle`]s: single and double
//!   bars, a curly wave, dots and dashes (the last three are drawn by the
//!   shader from one quad, [`Instance::wave`] and [`Instance::dashes`]).
//!   Their color is the cell's underline color (SGR 58) or, when that is
//!   the default, its foreground.
//! - The overline (SGR 53) is a bar at the top of the cell in the same
//!   color as the underline.
//! - The strikethrough is in the foreground color.
//!
//! The geometry comes from [`CellMetrics`]: the font's own underline
//! position and thickness are not exposed by the shaper (ADR 0010), and
//! deriving them from the cell keeps every bar inside its row.

use tessera_grid::{Cell, Color, Style, UnderlineStyle};

use super::{CellColors, Instance, Layout, f32_from};
use crate::palette::{ColorSlot, Palette};
use crate::shaping::CellMetrics;
use crate::theme::Rgba;

/// Where the decoration bars sit in a cell, in whole pixels from the cell
/// top, and the patterns of the drawn underline styles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DecorationGeometry {
    /// Height of every bar.
    pub thickness: u32,
    /// Top of the single, dotted and dashed underline.
    pub underline: u32,
    /// Tops of the two double-underline bars.
    pub double: [u32; 2],
    /// Top of the strikethrough bar.
    pub strike: u32,
    /// Top of the band the curly underline is drawn in; the band is
    /// `2 * curl_amplitude + thickness` high.
    pub curl_top: u32,
    /// Amplitude of the curly underline: the thickness, at least a pixel,
    /// less only when the cell is too short for the band.
    pub curl_amplitude: u32,
    /// Period of the curly underline and of the dash pattern's pair of
    /// dashes: the cell width (two dashes per cell).
    pub cell_width: f32,
    /// The x every pattern's phase is measured from: the grid's left edge
    /// (0 until [`Self::with_phase_x`]), so dots, dashes and the wave run
    /// on across runs of different colors and under the cursor, also when
    /// the cell width is odd.
    pub phase_x: f32,
}

impl DecorationGeometry {
    /// Geometry for cells of `metrics`.
    ///
    /// The thickness is one pixel per 16 pixels of cell height, rounded
    /// (at least one). The underline is centred in the space below the
    /// baseline, at least a pixel under it; the double underline is two
    /// bars one thickness apart, centred the same way; the curly
    /// underline's band is centred on the single underline; the
    /// strikethrough is centred at 30% of the baseline height above the
    /// baseline, about half the x-height of a monospaced face; the
    /// overline is the cell's top row of pixels. Everything stays inside
    /// the cell.
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
        let curl_amplitude = thickness.min(height.saturating_sub(thickness) / 2);
        let band = 2 * curl_amplitude + thickness;
        let curl_top = underline
            .saturating_sub(curl_amplitude)
            .min(height.saturating_sub(band));
        Self {
            thickness,
            underline,
            double,
            strike,
            curl_top,
            curl_amplitude,
            cell_width: f32_from(metrics.width_px.max(1)),
            phase_x: 0.0,
        }
    }

    /// The same geometry with patterns phased from `x`.
    pub fn with_phase_x(mut self, x: f32) -> Self {
        self.phase_x = x;
        self
    }

    /// The dotted underline's `(period, dot)`: dots as wide as the line is
    /// thick, at twice that pitch.
    pub fn dots(&self) -> (f32, f32) {
        let t = f32_from(self.thickness);
        (2.0 * t, t)
    }

    /// The dashed underline's `(period, dash)`: dashes a third of a cell
    /// long with a sixth of a cell between them.
    pub fn dashes(&self) -> (f32, f32) {
        let third = self.cell_width / 3.0;
        (third + self.cell_width / 6.0, third)
    }

    /// Appends the decorations `flags` asks for over `[x, y, w, _]` (a run
    /// of cells of one row): the underline and overline in `line`, the
    /// strikethrough in `fg`, every quad clipped to `clip`.
    pub fn push(
        &self,
        out: &mut Vec<Instance>,
        [left, cell_top, width, _]: [f32; 4],
        style: Style,
        (fg, line): (Rgba, Rgba),
        clip: [f32; 4],
    ) {
        let thick = f32_from(self.thickness);
        let at = |top: u32| [left, cell_top + f32_from(top), width, thick];
        let bar = |top: u32, color| Instance::solid(at(top), color);
        let mut emit = |instance: Instance| out.push(instance.clipped(clip));
        match style.underline_style() {
            UnderlineStyle::None => {}
            UnderlineStyle::Single => emit(bar(self.underline, line)),
            UnderlineStyle::Double => {
                emit(bar(self.double[0], line));
                emit(bar(self.double[1], line));
            }
            UnderlineStyle::Curly => {
                let amplitude = f32_from(self.curl_amplitude);
                let mut rect = at(self.curl_top);
                rect[3] += 2.0 * amplitude;
                let wave = (self.cell_width, amplitude);
                emit(Instance::wave(rect, line, wave, self.phase_x));
            }
            UnderlineStyle::Dotted => {
                emit(Instance::dashes(
                    at(self.underline),
                    line,
                    self.dots(),
                    self.phase_x,
                ));
            }
            UnderlineStyle::Dashed => {
                let dashes = self.dashes();
                emit(Instance::dashes(
                    at(self.underline),
                    line,
                    dashes,
                    self.phase_x,
                ));
            }
        }
        if style.has(Style::OVERLINE) {
            emit(bar(0, line));
        }
        if style.has(Style::STRIKETHROUGH) {
            emit(bar(self.strike, fg));
        }
    }
}

/// The flags a decoration run must share.
pub(crate) const DECORATIONS: u16 =
    Style::UNDERLINE_STYLE_MASK | Style::OVERLINE | Style::STRIKETHROUGH;

/// The colors `style`'s decorations are drawn in: the foreground (after
/// inverse and dim) for the strikethrough, and the underline color for
/// underlines and the overline, which is the foreground when the cell has
/// none (SGR 59) and is otherwise resolved like a foreground (dimmed by
/// DIM, never swapped by INVERSE).
pub(crate) fn colors(palette: &Palette, style: Style) -> (Rgba, Rgba) {
    let fg = CellColors::foreground(palette, style);
    let line = match style.underline {
        Color::Default => fg,
        color if style.has(Style::DIM) => palette.resolve_dim(color, ColorSlot::Foreground),
        color => palette
            .resolve(color, ColorSlot::Foreground)
            .with_alpha(1.0),
    };
    (fg, line)
}

/// Appends the decoration bars of `rows` to `out`.
#[cfg(test)]
pub(crate) fn push(out: &mut Vec<Instance>, layout: &Layout, palette: &Palette, rows: &[&[Cell]]) {
    for (r, row) in rows.iter().take(layout.visible_rows()).enumerate() {
        push_row(out, layout, palette, r, row);
    }
}

/// A run of decorated cells: first column, decoration flags, colors.
type Run = (usize, u16, (Rgba, Rgba));

/// Appends the decoration bars of row `r`, holding `row`, to `out`.
pub(crate) fn push_row(
    out: &mut Vec<Instance>,
    layout: &Layout,
    palette: &Palette,
    r: usize,
    row: &[Cell],
) {
    let cols = layout.visible_cols();
    let row = &row[..row.len().min(cols)];
    // Most rows have no decoration at all: one cheap scan instead of the
    // run bookkeeping per cell.
    if row.iter().all(|cell| cell.style.flags & DECORATIONS == 0) {
        return;
    }
    let mut run: Option<Run> = None;
    let mut flush = |run: Option<Run>, end: usize| {
        let Some((start, flags, colors)) = run else {
            return;
        };
        let style = Style {
            flags,
            ..Style::DEFAULT
        };
        let rect = layout.cells_rect(r, start, end - start);
        layout
            .decoration
            .push(out, rect, style, colors, super::NO_CLIP);
    };
    for (c, cell) in row.iter().enumerate() {
        let flags = cell.style.flags & DECORATIONS;
        let decorated = flags != 0 && !cell.style.has(Style::HIDDEN);
        let key = decorated.then(|| (flags, colors(palette, cell.style)));
        let continues = match (run, key) {
            (Some((_, open, open_colors)), Some((flags, colors))) => {
                open == flags && open_colors == colors
            }
            _ => false,
        };
        if continues {
            continue;
        }
        flush(run.take(), c);
        if let Some((flags, colors)) = key {
            run = Some((c, flags, colors));
        }
    }
    flush(run, row.len());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passes::Kind;
    use crate::size::PhysicalSize;
    use crate::theme::Theme;

    fn metrics(height_px: u32, baseline_px: f32) -> CellMetrics {
        metrics_w(8, height_px, baseline_px)
    }

    fn metrics_w(width_px: u32, height_px: u32, baseline_px: f32) -> CellMetrics {
        CellMetrics {
            width_px,
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
                    let band = 2 * g.curl_amplitude + g.thickness;
                    assert!(g.curl_top + band <= height, "{height}/{baseline}: {g:?}");
                }
            }
        }
    }

    /// Every quad of every style, drawn over one cell, as `[x, y, w, h]`
    /// relative to the cell, with its kind and pattern parameters.
    fn quads(g: &DecorationGeometry, flags: u16) -> Vec<([f32; 4], u32, [f32; 2])> {
        let mut out = Vec::new();
        let style = Style::DEFAULT.with(flags);
        let colors = (Rgba::WHITE, Rgba::WHITE);
        g.push(
            &mut out,
            [0.0, 0.0, g.cell_width, 0.0],
            style,
            colors,
            super::super::NO_CLIP,
        );
        out.iter().map(|i| (i.rect, i.kind, i.texel)).collect()
    }

    /// D8: at the smallest font size and at 1x scale every style's quads
    /// lie inside the cell (no negative height, nothing reaching the next
    /// row).
    #[test]
    fn every_style_stays_inside_small_cells() {
        let menlo_cell = |size_pt: f32, scale: f32| {
            crate::Shaper::new(&crate::FontConfig {
                size_pt,
                scale,
                ..crate::FontConfig::default()
            })
            .expect("fonts")
            .cell_metrics()
        };
        let all = Style::UNDERLINE | Style::OVERLINE | Style::STRIKETHROUGH;
        let shapes = [
            0,
            Style::DOUBLE_UNDERLINE,
            Style::CURLY_UNDERLINE,
            Style::DOTTED_UNDERLINE,
            Style::DASHED_UNDERLINE,
        ];
        let mut cells = vec![
            menlo_cell(crate::MIN_FONT_SIZE_PT, 1.0),
            menlo_cell(13.0, 1.0),
        ];
        cells.extend((1..=6).flat_map(|h| (0..=h).map(move |b| metrics(h, f32_from(b)))));
        for m in cells {
            let g = DecorationGeometry::new(m);
            for shape in shapes {
                for ([x, y, w, h], _, _) in quads(&g, all | shape) {
                    assert!(w > 0.0 && h > 0.0, "{m:?} {shape}: {w}x{h}");
                    assert!(x >= 0.0 && x + w <= g.cell_width, "{m:?} {shape}");
                    let height = f32_from(m.height_px.max(1));
                    assert!(y >= 0.0 && y + h <= height, "{m:?} {shape}: {y}+{h}");
                }
            }
        }
    }

    /// The pattern constants of the brief at 2x (16 x 30 cells) and 1x
    /// (8 x 15).
    #[test]
    fn style_geometry_at_both_scales() {
        let solid = Kind::Solid as u32;
        let wave = Kind::Wave as u32;
        let dashes = Kind::Dashes as u32;
        let retina = DecorationGeometry::new(metrics_w(16, 30, 24.0));
        let single = |top| ([0.0, top, 16.0, 2.0], solid, [0.0; 2]);
        assert_eq!(quads(&retina, Style::UNDERLINE), [single(26.0)]);
        assert_eq!(
            quads(&retina, Style::UNDERLINE | Style::DOUBLE_UNDERLINE),
            [single(24.0), single(28.0)],
            "two bars one thickness apart"
        );
        assert_eq!(
            quads(&retina, Style::UNDERLINE | Style::CURLY_UNDERLINE),
            [([0.0, 24.0, 16.0, 6.0], wave, [16.0, 2.0])],
            "amplitude = thickness, period = cell width, centred on the underline"
        );
        assert_eq!(
            quads(&retina, Style::UNDERLINE | Style::DOTTED_UNDERLINE),
            [([0.0, 26.0, 16.0, 2.0], dashes, [4.0, 2.0])],
            "thickness-sized dots at twice the thickness"
        );
        assert_eq!(
            quads(&retina, Style::UNDERLINE | Style::DASHED_UNDERLINE),
            [([0.0, 26.0, 16.0, 2.0], dashes, [8.0, 16.0 / 3.0])],
            "dashes of a third of a cell, gaps of a sixth"
        );
        assert_eq!(quads(&retina, Style::OVERLINE), [single(0.0)]);

        let one = DecorationGeometry::new(metrics_w(8, 15, 12.0));
        let single = |top| ([0.0, top, 8.0, 1.0], solid, [0.0; 2]);
        assert_eq!(quads(&one, Style::UNDERLINE), [single(13.0)]);
        assert_eq!(
            quads(&one, Style::UNDERLINE | Style::DOUBLE_UNDERLINE),
            [single(12.0), single(14.0)]
        );
        assert_eq!(
            quads(&one, Style::UNDERLINE | Style::CURLY_UNDERLINE),
            [([0.0, 12.0, 8.0, 3.0], wave, [8.0, 1.0])],
            "at least a pixel of amplitude"
        );
        assert_eq!(
            quads(&one, Style::UNDERLINE | Style::DOTTED_UNDERLINE),
            [([0.0, 13.0, 8.0, 1.0], dashes, [2.0, 1.0])]
        );
        assert_eq!(
            quads(&one, Style::UNDERLINE | Style::DASHED_UNDERLINE),
            [([0.0, 13.0, 8.0, 1.0], dashes, [4.0, 8.0 / 3.0])]
        );
        assert_eq!(quads(&one, Style::OVERLINE), [single(0.0)]);
    }

    /// The underline color (SGR 58) colors underlines and the overline,
    /// never the strikethrough; DIM dims it, INVERSE does not swap it, and
    /// the default is the foreground after inverse and dim.
    #[test]
    fn underline_color_rules() {
        let palette = Palette::new(&Theme::default());
        let red = palette.resolve(Color::Indexed(1), ColorSlot::Foreground);
        let plain = Style::DEFAULT;
        let (fg, line) = colors(&palette, plain);
        assert_eq!(line, fg, "no underline color: the foreground");
        let colored = Style {
            underline: Color::Indexed(1),
            ..plain
        };
        assert_eq!(colors(&palette, colored), (fg, red));
        assert_eq!(colors(&palette, colored.with(Style::INVERSE)).1, red);
        let dimmed = colors(&palette, colored.with(Style::DIM)).1;
        assert_eq!(
            dimmed,
            palette.resolve_dim(Color::Indexed(1), ColorSlot::Foreground)
        );
        assert_ne!(dimmed, red);
        let inverse = colors(&palette, plain.with(Style::INVERSE));
        assert_eq!(inverse.1, inverse.0, "the swapped foreground");
        assert_eq!(
            inverse.0,
            CellColors::foreground(&palette, plain.with(Style::INVERSE))
        );
    }

    #[test]
    fn runs_split_on_style_and_underline_color() {
        let palette = Palette::new(&Theme::default());
        let cell = |flags, underline| {
            Cell::new(
                'a',
                Style {
                    underline,
                    flags,
                    ..Style::DEFAULT
                },
            )
        };
        let u = Style::UNDERLINE;
        let row = [
            cell(u, Color::Default),
            cell(u, Color::Default),
            cell(u | Style::CURLY_UNDERLINE, Color::Default),
            cell(u | Style::CURLY_UNDERLINE, Color::Indexed(1)),
            cell(Style::OVERLINE, Color::Indexed(1)),
        ];
        let rows: [&[Cell]; 1] = [&row];
        let layout = Layout::new(metrics(30, 24.0), 1.0, 0.0, PhysicalSize::new(80, 30));
        let mut out = Vec::new();
        push(&mut out, &layout, &palette, &rows);
        let rects: Vec<_> = out.iter().map(|i| (i.rect[0], i.rect[2], i.kind)).collect();
        let wave = Kind::Wave as u32;
        assert_eq!(
            rects,
            [
                (0.0, 16.0, 0),
                (16.0, 8.0, wave),
                (24.0, 8.0, wave),
                (32.0, 8.0, 0)
            ],
            "a curly cell never merges with a single one, nor two underline colors"
        );
        let red = palette.resolve(Color::Indexed(1), ColorSlot::Foreground);
        assert_eq!(out[3].color, red.premultiplied(), "overline in SGR 58");
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
            Cell::new(
                'a',
                Style::DEFAULT.with(Style::UNDERLINE | Style::DOUBLE_UNDERLINE),
            ),
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
