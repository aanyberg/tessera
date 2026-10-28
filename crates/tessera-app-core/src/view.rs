//! Views: a session drawn into a frontend's surface (ADR 0014).
//!
//! A view is one session's on-screen presence: the swapchain of the
//! frontend's layer ([`tessera_render::RenderSurface`]), the
//! [`tessera_render::Renderer`] that draws into it, the font size and a
//! snapshot buffer for the session's visible rows. There is at most one view
//! per session; [`crate::App`] owns them, keyed by [`crate::SessionId`], and
//! drives them through [`crate::App::attach_view`],
//! [`crate::App::resize_view`], [`crate::App::render_view`],
//! [`crate::App::set_view_font_size`] and [`crate::App::detach_view`].
//!
//! The grid follows the surface: whenever a view's pixel size, backing
//! scale or font size changes, the session is resized to
//! [`grid_size_for`] the new geometry.
//!
//! This module re-exports the few `tessera-render` and `wgpu` items a
//! frontend needs to hand a surface over, so `tessera-ffi` depends on this
//! crate alone.

use tessera_config::Config;
use tessera_grid::{Cell, Grid, Point};
use tessera_pty::PtySize;
pub use tessera_render::{CellMetrics, FrameStatus, PhysicalSize, RenderError, wgpu};
use tessera_render::{
    CursorShape, CursorState, Fonts, FrameInput, Gpu, RenderSurface, Renderer, Shaper,
    SurfaceOptions, Theme, sanitize_font_size, sanitize_scale,
};

/// A view's geometry: the surface in physical pixels, its backing scale and
/// the grid that fits in it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewSize {
    /// The surface's size in physical pixels (after clamping to the GPU's
    /// largest texture).
    pub pixels: PhysicalSize,
    /// Backing scale factor (1.0 on a standard display, 2.0 on Retina),
    /// after [`tessera_render::sanitize_scale`].
    pub scale: f32,
    /// Rows of cells that fit, at least 1 and at most [`MAX_VIEW_ROWS`].
    pub rows: u16,
    /// Columns of cells that fit, at least 1 and at most
    /// [`MAX_VIEW_COLS`].
    pub cols: u16,
}

/// Most columns a grid derived from a surface gets ([`grid_size_for`]).
///
/// A 16384-pixel surface at backing scale 0.25 with a 4 pt font would
/// otherwise fit 16382 columns and as many rows, about 5.4 GB of cells
/// once the grid follows the surface. 2048 columns of the smallest
/// readable font still span several 8K displays.
pub const MAX_VIEW_COLS: u16 = 2048;

/// Most rows a grid derived from a surface gets ([`grid_size_for`]); see
/// [`MAX_VIEW_COLS`].
pub const MAX_VIEW_ROWS: u16 = 1024;

/// How many `(rows, cols)` of `metrics`-sized cells fit in a surface of
/// `size` physical pixels at backing `scale`, with `padding_pt` logical
/// points of padding on every side.
///
/// The padding is rounded to whole pixels the way the renderer rounds it
/// (`round(padding_pt * scale)`, a non-finite or negative product counting
/// as 0); `scale` is sanitised like [`tessera_render::sanitize_scale`].
/// Only whole cells count. Both dimensions are at least 1, and at most
/// [`MAX_VIEW_ROWS`] rows and [`MAX_VIEW_COLS`] columns.
#[must_use]
pub fn grid_size_for(
    size: PhysicalSize,
    scale: f32,
    padding_pt: f32,
    metrics: CellMetrics,
) -> (u16, u16) {
    let pad = whole_px(padding_pt * sanitize_scale(scale));
    let cells = |extent: u32, cell: u32, max: u16| {
        let usable = extent.saturating_sub(pad.saturating_mul(2));
        u16::try_from(usable / cell.max(1))
            .unwrap_or(u16::MAX)
            .clamp(1, max)
    };
    (
        cells(size.height, metrics.height_px, MAX_VIEW_ROWS),
        cells(size.width, metrics.width_px, MAX_VIEW_COLS),
    )
}

/// The PTY size a session drawn in a view of `size` with `metrics`-sized
/// cells should have: `size.rows` x `size.cols` cells, and as pixels the
/// text area those cells cover (`cols * width_px` by `rows * height_px`,
/// each clamped to `u16::MAX`), not the whole surface with its padding,
/// as xterm reports it. `None` while the surface is empty, so a collapsed
/// or hidden view keeps the session's last size instead of shrinking it to
/// one cell.
#[must_use]
pub fn pty_size_for(size: ViewSize, metrics: CellMetrics) -> Option<PtySize> {
    if size.pixels.is_empty() {
        return None;
    }
    let px = |cells: u16, cell_px: u32| {
        u16::try_from(u32::from(cells).saturating_mul(cell_px)).unwrap_or(u16::MAX)
    };
    Some(PtySize::new(size.rows, size.cols).with_pixels(
        px(size.cols, metrics.width_px),
        px(size.rows, metrics.height_px),
    ))
}

/// `v` rounded to whole pixels; `NaN` and negatives are 0, overflow
/// saturates.
fn whole_px(v: f32) -> u32 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "`as` saturates; NaN and negatives map to 0"
    )]
    let px = v.round().max(0.0) as u32;
    px
}

/// The theme a view draws with, from the application's configuration:
/// the default theme with the configured font family and size (clamped
/// like [`tessera_render::sanitize_font_size`]) and the configured opacity
/// as the default background's alpha.
#[must_use]
pub fn theme_for(config: &Config) -> Theme {
    let appearance = &config.appearance;
    let defaults = Theme::default();
    Theme {
        font_family: appearance.font_family.clone(),
        font_size: sanitize_font_size(appearance.font_size),
        background: defaults.background.with_alpha(appearance.opacity),
        ..defaults
    }
}

/// One session's surface, renderer and font size.
///
/// Created by [`crate::App::attach_view`] and only reachable through
/// [`crate::App`]'s view methods. `Send + Sync`, so the application stays shareable across
/// threads, but every call that touches the surface belongs on the main
/// thread (ADR 0014).
#[derive(Debug)]
pub(crate) struct View {
    gpu: Gpu,
    surface: RenderSurface,
    renderer: Renderer,
    font_size_pt: f32,
    snapshot: Snapshot,
}

impl View {
    /// Configures `surface` on `gpu` at `size` and `scale` and builds a
    /// renderer drawing `theme` with a shaper on `fonts`.
    pub(crate) fn new(
        gpu: Gpu,
        surface: wgpu::Surface<'static>,
        fonts: Fonts,
        theme: Theme,
        size: PhysicalSize,
        scale: f32,
    ) -> Result<Self, RenderError> {
        let options = SurfaceOptions {
            transparent: theme.background.clamped().a < 1.0,
            ..SurfaceOptions::default()
        };
        let surface = RenderSurface::attach(&gpu, surface, size, scale, options)?;
        let font_size_pt = sanitize_font_size(theme.font_size);
        let shaper = Shaper::with_fonts(fonts, font_size_pt, surface.scale());
        let renderer = Renderer::new(&gpu, theme, shaper)?;
        Ok(Self {
            gpu,
            surface,
            renderer,
            font_size_pt,
            snapshot: Snapshot::default(),
        })
    }

    /// The surface's size, scale and the grid that fits in it.
    pub(crate) fn size(&self) -> ViewSize {
        let pixels = self.surface.size();
        let scale = self.surface.scale();
        let (rows, cols) = grid_size_for(
            pixels,
            self.renderer.shaper().scale(),
            self.renderer.theme().padding,
            self.renderer.cell_metrics(),
        );
        ViewSize {
            pixels,
            scale,
            rows,
            cols,
        }
    }

    /// The font size in points, after
    /// [`tessera_render::sanitize_font_size`].
    pub(crate) fn font_size(&self) -> f32 {
        self.font_size_pt
    }

    /// The cell size at the current font size and scale, in physical
    /// pixels.
    pub(crate) fn cell_metrics(&self) -> CellMetrics {
        self.renderer.cell_metrics()
    }

    /// The PTY size the session should have for this view (see
    /// [`pty_size_for`]).
    pub(crate) fn pty_size(&self) -> Option<PtySize> {
        pty_size_for(self.size(), self.cell_metrics())
    }

    /// Reconfigures the surface for `size` and `scale` and keeps the
    /// shaper's backing scale in step with it.
    pub(crate) fn resize(&mut self, size: PhysicalSize, scale: f32) {
        self.surface.resize(&self.gpu, size, scale);
        self.renderer
            .set_font_size(self.font_size_pt, self.surface.scale());
    }

    /// Changes the font size (clamped like
    /// [`tessera_render::sanitize_font_size`]).
    pub(crate) fn set_font_size(&mut self, size_pt: f32) {
        self.font_size_pt = sanitize_font_size(size_pt);
        self.renderer
            .set_font_size(self.font_size_pt, self.surface.scale());
    }

    /// Copies what the next frame shows out of `grid`.
    ///
    /// This is the only part of a frame that runs under the session's
    /// terminal lock; shaping, drawing and waiting for a drawable happen
    /// after it is released.
    pub(crate) fn capture(&mut self, grid: &Grid) {
        self.snapshot.capture(grid);
    }

    /// Draws the last captured snapshot and presents it.
    pub(crate) fn render(&mut self, focused: bool) -> Result<FrameStatus, RenderError> {
        let rows = self.snapshot.rows();
        let frame = self.snapshot.frame(&rows, focused);
        self.surface.render(&self.gpu, &mut self.renderer, &frame)
    }
}

/// The visible rows and cursor of a grid, copied so the terminal lock is
/// held only for the copy. The buffer is reused from frame to frame.
#[derive(Debug, Default)]
pub(crate) struct Snapshot {
    cells: Vec<Cell>,
    /// Cells per row (every row of a grid has the same length).
    cols: usize,
    cursor: Point,
}

impl Snapshot {
    /// Replaces the snapshot with `grid`'s visible rows and cursor.
    pub(crate) fn capture(&mut self, grid: &Grid) {
        self.cells.clear();
        self.cols = usize::from(grid.cols());
        for line in grid.visible_lines() {
            // Rows are `cols` long by construction; pad or cut defensively
            // so `rows()` can chunk evenly.
            let cells = &line.cells[..line.cells.len().min(self.cols)];
            self.cells.extend_from_slice(cells);
            self.cells
                .resize(self.cells.len() + self.cols - cells.len(), Cell::default());
        }
        self.cursor = grid.cursor();
    }

    /// The captured rows, top to bottom.
    pub(crate) fn rows(&self) -> Vec<&[Cell]> {
        if self.cols == 0 {
            return Vec::new();
        }
        self.cells.chunks(self.cols).collect()
    }

    /// The frame drawing `rows` (this snapshot's [`Self::rows`]): a
    /// visible block cursor at the captured cursor position, `focused`
    /// (an unfocused frame draws the cursor hollow), no selection and no
    /// overlays. The viewport (#31), cursor blink and shape (#27, #29) are
    /// still to come.
    pub(crate) fn frame<'a>(&self, rows: &'a [&'a [Cell]], focused: bool) -> FrameInput<'a> {
        let Point { row, col } = self.cursor;
        FrameInput::new(rows)
            .with_cursor(Some(CursorState::new(row, col, CursorShape::Block)))
            .with_focus(focused)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use tessera_term::Terminal;

    use super::*;

    /// Menlo 13 pt at 1x (what `tessera-render`'s shaping tests pin).
    const MENLO_1X: CellMetrics = CellMetrics {
        width_px: 8,
        height_px: 15,
        baseline_px: 12.0,
    };
    /// Menlo 13 pt at 2x.
    const MENLO_2X: CellMetrics = CellMetrics {
        width_px: 16,
        height_px: 30,
        baseline_px: 24.0,
    };

    #[test]
    fn grid_size_at_1x_and_2x() {
        let size = PhysicalSize::new(800, 600);
        // 800 - 2 * 4 = 792 px / 8 = 99 cols; 600 - 8 = 592 / 15 = 39 rows.
        assert_eq!(grid_size_for(size, 1.0, 4.0, MENLO_1X), (39, 99));
        // At 2x the padding is 8 px: 784 / 16 = 49 cols, 584 / 30 = 19 rows.
        assert_eq!(grid_size_for(size, 2.0, 4.0, MENLO_2X), (19, 49));
        // The same logical window on a Retina display has the same grid.
        assert_eq!(
            grid_size_for(PhysicalSize::new(1600, 1200), 2.0, 4.0, MENLO_2X),
            (39, 99)
        );
    }

    #[test]
    fn padding_is_subtracted_on_both_sides() {
        let size = PhysicalSize::new(80, 150);
        assert_eq!(grid_size_for(size, 1.0, 0.0, MENLO_1X), (10, 10));
        // 1 px of padding per side costs a whole cell in each direction.
        assert_eq!(grid_size_for(size, 1.0, 1.0, MENLO_1X), (9, 9));
        // Non-finite or negative padding counts as none.
        assert_eq!(grid_size_for(size, 1.0, f32::NAN, MENLO_1X), (10, 10));
        assert_eq!(grid_size_for(size, 1.0, -3.0, MENLO_1X), (10, 10));
        // A bad scale is 1.0.
        assert_eq!(grid_size_for(size, f32::NAN, 1.0, MENLO_1X), (9, 9));
    }

    #[test]
    fn grid_is_never_below_one_by_one() {
        for size in [
            PhysicalSize::new(0, 0),
            PhysicalSize::new(7, 14),
            PhysicalSize::new(8, 0),
        ] {
            assert_eq!(grid_size_for(size, 1.0, 4.0, MENLO_1X), (1, 1), "{size:?}");
        }
        let degenerate = CellMetrics {
            width_px: 0,
            height_px: 0,
            baseline_px: 0.0,
        };
        assert_eq!(
            grid_size_for(PhysicalSize::new(3, 2), 1.0, 0.0, degenerate),
            (2, 3)
        );
    }

    #[test]
    fn huge_sizes_are_capped() {
        let huge = PhysicalSize::new(u32::MAX, u32::MAX);
        assert_eq!(
            grid_size_for(huge, 1.0, 4.0, MENLO_1X),
            (MAX_VIEW_ROWS, MAX_VIEW_COLS)
        );
        // The largest Metal surface at the smallest scale and font: 1x1 px
        // cells would give 16382 x 16382 without the cap.
        let tiny_cells = CellMetrics {
            width_px: 1,
            height_px: 1,
            baseline_px: 1.0,
        };
        let largest = PhysicalSize::new(16_384, 16_384);
        assert_eq!(grid_size_for(largest, 0.25, 4.0, tiny_cells), (1024, 2048));
        // Just under the cap is not affected.
        assert_eq!(
            grid_size_for(PhysicalSize::new(2047, 1023), 1.0, 0.0, tiny_cells),
            (1023, 2047)
        );
        assert_eq!(
            grid_size_for(huge, f32::MAX, f32::MAX, MENLO_1X),
            (1, 1),
            "padding that saturates leaves no room"
        );
    }

    #[test]
    fn pty_size_follows_the_view_unless_it_is_empty() {
        let size = ViewSize {
            pixels: PhysicalSize::new(800, 600),
            scale: 1.0,
            rows: 39,
            cols: 99,
        };
        // The text area, without the padding: 99 * 8 by 39 * 15.
        assert_eq!(
            pty_size_for(size, MENLO_1X),
            Some(PtySize::new(39, 99).with_pixels(792, 585))
        );
        // A capped grid reports the capped text area, not the surface.
        let huge = ViewSize {
            pixels: PhysicalSize::new(70_000, 100_000),
            rows: MAX_VIEW_ROWS,
            cols: MAX_VIEW_COLS,
            ..size
        };
        assert_eq!(
            pty_size_for(huge, MENLO_1X),
            Some(PtySize::new(1024, 2048).with_pixels(2048 * 8, 1024 * 15))
        );
        let giant_cells = CellMetrics {
            width_px: u32::MAX,
            height_px: u32::MAX,
            baseline_px: 0.0,
        };
        assert_eq!(
            pty_size_for(huge, giant_cells),
            Some(PtySize::new(1024, 2048).with_pixels(u16::MAX, u16::MAX))
        );
        for pixels in [PhysicalSize::new(0, 600), PhysicalSize::new(800, 0)] {
            assert_eq!(pty_size_for(ViewSize { pixels, ..size }, MENLO_1X), None);
        }
    }

    #[test]
    fn theme_comes_from_the_configuration() {
        let mut config = Config::default();
        config.appearance.font_family = "Monaco".into();
        config.appearance.font_size = 1000.0;
        config.appearance.opacity = 0.5;
        let theme = theme_for(&config);
        assert_eq!(theme.font_family, "Monaco");
        assert!((theme.font_size - tessera_render::MAX_FONT_SIZE_PT).abs() < f32::EPSILON);
        assert!((theme.background.a - 0.5).abs() < f32::EPSILON);
        assert_eq!(
            theme.foreground,
            Theme::default().foreground,
            "colors stay the defaults"
        );
    }

    #[test]
    fn the_frame_draws_the_snapshot_with_a_block_cursor() {
        let mut term = Terminal::new(3, 4);
        term.advance(b"ab\r\ncd");
        let mut snapshot = Snapshot::default();
        snapshot.capture(term.grid());
        let rows = snapshot.rows();
        let frame = snapshot.frame(&rows, true);
        let text: Vec<String> = frame
            .rows
            .iter()
            .map(|row| row.iter().map(|cell| cell.ch).collect())
            .collect();
        assert_eq!(text, ["ab  ", "cd  ", "    "]);
        let cursor = CursorState::new(1, 2, CursorShape::Block);
        assert_eq!(frame.cursor, Some(cursor));
        assert!(cursor.visible);
        assert!(frame.focused);
        assert_eq!(frame.selection, None);
        assert_eq!(frame.overlays.len(), 0);
        assert_eq!(frame.drawn_cursor(), Some((cursor, CursorShape::Block)));

        let unfocused = snapshot.frame(&rows, false);
        assert!(!unfocused.focused);
        assert_eq!(
            unfocused.drawn_cursor(),
            Some((cursor, CursorShape::HollowBlock))
        );
    }

    /// The `ascii` line of the wave's performance corpus.
    const ASCII: &str =
        "The quick brown fox jumps over the lazy dog 0123456789 ~!@#$%^&*()_+-=[]{};':\",./<>?";

    fn full_terminal(rows: u16, cols: u16) -> Terminal {
        let mut term = Terminal::new(rows, cols);
        let mut text = String::new();
        while text.len() < usize::from(rows) * usize::from(cols) {
            text.push_str(ASCII);
        }
        term.advance(text.as_bytes());
        term
    }

    #[test]
    fn snapshot_copies_rows_and_cursor() {
        let mut term = Terminal::new(3, 4);
        term.advance(b"ab\r\ncd");
        let mut snapshot = Snapshot::default();
        snapshot.capture(term.grid());
        let rows = snapshot.rows();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.len() == 4));
        assert_eq!(rows[0][0].ch, 'a');
        assert_eq!(rows[1][1].ch, 'd');
        assert_eq!(snapshot.cursor, Point::new(1, 2));
        // Recapturing a smaller grid reuses the buffer and forgets the old rows.
        snapshot.capture(Terminal::new(1, 2).grid());
        assert_eq!(snapshot.rows().len(), 1);
        assert_eq!(snapshot.cursor, Point::new(0, 0));
    }

    /// The terminal lock is held only while a frame is copied out of the
    /// grid: at 200x60 cells of the `ascii` corpus line that copy must stay
    /// well under 2 ms (ADR 0014 records the release-build number).
    ///
    /// This times [`Snapshot::capture`] alone. It does not prove that
    /// `App::render_view` releases the lock before drawing: moving
    /// `View::render` inside `with_terminal` would not fail it.
    #[test]
    fn snapshot_of_a_large_screen_is_fast() {
        let term = full_terminal(60, 200);
        let mut snapshot = Snapshot::default();
        snapshot.capture(term.grid());
        let mut best = Duration::MAX;
        for _ in 0..50 {
            let start = Instant::now();
            snapshot.capture(term.grid());
            best = best.min(start.elapsed());
        }
        assert_eq!(snapshot.rows().len(), 60);
        eprintln!("capturing 200x60 cells: best of 50 is {best:?}");
        assert!(
            best < Duration::from_millis(2),
            "capturing 200x60 cells took {best:?}"
        );
    }
}
