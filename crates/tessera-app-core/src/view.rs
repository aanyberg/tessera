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
//!
//! # Damage and pacing (ADR 0016)
//!
//! `View::capture` runs under the session's terminal lock: it asks the
//! grid which rows changed since the last capture
//! ([`Grid::damage_since`]), compares and copies only those into the
//! snapshot, and adds the rows that really changed to the damage the
//! next frame draws. A view scrolled back into the scrollback (ADR 0023)
//! copies the lines at its offset instead, whole when the view moved and
//! compared otherwise, so output that leaves those lines alone damages
//! nothing. `View::render` runs after the lock is released and
//! returns [`FrameStatus::Skipped`] without touching the GPU when nothing
//! changed since the last presented frame (no damage, same cursor, same
//! focus, nothing the renderer must redraw on its own), so an idle
//! terminal costs no GPU work. It also presents at most one frame per
//! [`MIN_FRAME_INTERVAL`], returning [`FrameStatus::Paced`] for a call
//! that comes too soon: a flood of output is captured on every call but
//! only the newest snapshot is drawn, so the reader thread never waits on
//! the GPU and the screen updates at display rate. A surface with nothing
//! to draw into reports [`FrameStatus::NotReady`].

use std::time::{Duration, Instant};

use tessera_config::Config;
use tessera_grid::{Cell, DamageRows, GraphemeTable, Grid, Line, Point};
use tessera_pty::PtySize;
pub use tessera_render::{CellMetrics, FrameStatus, PhysicalSize, RenderError, wgpu};
use tessera_render::{
    CursorShape, CursorState, Fonts, FrameInput, Gpu, RenderSurface, Renderer, Rgba, Shaper,
    SurfaceOptions, Theme, sanitize_font_size, sanitize_scale,
};
use tessera_term::{CursorStyle, Terminal};

/// The shortest time between two presented frames of one view: one
/// display period at 240 Hz, the fastest refresh a Mac display runs at.
///
/// A host that calls [`crate::App::render_view`] more often (a host
/// rendering on every output notification) gets [`FrameStatus::Paced`]
/// for the extra calls and must render again on its next refresh; each call still captures the grid, so a later
/// presented frame shows the newest content. Calls up to [`FRAME_SLACK`]
/// early count as on time, and the schedule never exceeds one frame per
/// interval on average: over any window of `T`, at most
/// `T / MIN_FRAME_INTERVAL + 2` frames are presented. A host driven by a
/// display link of at most 240 Hz is never paced (see [`FRAME_SLACK`]);
/// the limit bounds hosts that render on every output notification.
pub const MIN_FRAME_INTERVAL: Duration = Duration::from_micros(4166);

/// How early a frame may come and still count as on time: half of
/// [`MIN_FRAME_INTERVAL`]. A render is then paced only when it is the
/// third within one interval (two consecutive gaps adding up to less
/// than 1/240 s), which a display link of at most 240 Hz never produces
/// unless it delivers a tick more than a whole period late. A host that
/// renders faster than that, or in bursts closer than about half an
/// interval, can still get [`FrameStatus::Paced`] and must render again
/// after it.
pub const FRAME_SLACK: Duration = Duration::from_micros(2083);

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

/// The theme a view draws with: the colours of `theme` (the resolved
/// `appearance.theme`, see [`tessera_config::ConfigStore::theme`]) and the
/// font family and size of `config` (the size clamped like
/// [`tessera_render::sanitize_font_size`]).
///
/// Colours are sRGB bytes decoded to linear ([`Rgba::from_srgb8`]) with
/// alpha `a / 255`. The background's alpha is the theme's multiplied by
/// `appearance.opacity`, so an opaque theme at opacity 1.0 stays opaque
/// and a translucent theme background stays as translucent as written.
/// Line height and padding are the renderer's defaults.
#[must_use]
pub fn theme_for(config: &Config, theme: &tessera_config::Theme) -> Theme {
    let appearance = &config.appearance;
    let background = rgba(theme.background);
    let defaults = Theme::default();
    Theme {
        font_family: appearance.font_family.clone(),
        font_size: sanitize_font_size(appearance.font_size),
        foreground: rgba(theme.foreground),
        background: background.with_alpha(background.a * appearance.opacity),
        cursor: rgba(theme.cursor),
        selection: rgba(theme.selection),
        search_match: theme.search_match.map_or(defaults.search_match, rgba),
        search_match_current: theme
            .search_match_current
            .map_or(defaults.search_match_current, rgba),
        ansi: theme.ansi.map(rgba),
        ..defaults
    }
}

/// The cursor style a session starts with and returns to (DECSCUSR 0,
/// resets): `[appearance] cursor_shape` and `cursor_blink`
/// ([`Terminal::set_default_cursor_style`]).
pub(crate) fn cursor_style_for(config: &Config) -> CursorStyle {
    let appearance = &config.appearance;
    let shape = match appearance.cursor_shape {
        tessera_config::CursorShape::Block => tessera_term::CursorShape::Block,
        tessera_config::CursorShape::Underline => tessera_term::CursorShape::Underline,
        tessera_config::CursorShape::Beam => tessera_term::CursorShape::Beam,
    };
    CursorStyle {
        shape,
        blink: appearance.cursor_blink,
    }
}

/// The renderer's shape for the terminal's: the same three. Blinking is
/// not drawn yet (the host's blink clock is a follow-up), so it has no
/// counterpart here.
fn render_shape(shape: tessera_term::CursorShape) -> CursorShape {
    match shape {
        tessera_term::CursorShape::Block => CursorShape::Block,
        tessera_term::CursorShape::Underline => CursorShape::Underline,
        tessera_term::CursorShape::Beam => CursorShape::Beam,
    }
}

/// True when a frame at `now` comes too early on the schedule whose last
/// presented frame was due at `last` (see [`MIN_FRAME_INTERVAL`]).
fn is_paced(last: Option<Instant>, now: Instant) -> bool {
    last.is_some_and(|t| now + FRAME_SLACK < t + MIN_FRAME_INTERVAL)
}

/// The due time to record for a frame presented at `now` after one due at
/// `last`: an interval later, but never more than the slack behind the
/// clock, so an idle view does not bank frames.
fn due_time(last: Option<Instant>, now: Instant) -> Instant {
    let earliest = now.checked_sub(FRAME_SLACK).unwrap_or(now);
    last.map_or(earliest, |t| (t + MIN_FRAME_INTERVAL).max(earliest))
}

/// What a view draws into.
#[derive(Debug)]
enum Target {
    /// The frontend's layer.
    Surface(RenderSurface),
    /// A headless texture, for tests.
    #[cfg(test)]
    Offscreen {
        target: tessera_render::OffscreenTarget,
        scale: f32,
        /// Stands in for a surface with no drawable: every frame is
        /// [`FrameStatus::NotReady`] and nothing is drawn.
        not_ready: bool,
    },
}

impl Target {
    fn size(&self) -> PhysicalSize {
        match self {
            Self::Surface(surface) => surface.size(),
            #[cfg(test)]
            Self::Offscreen { target, .. } => target.size(),
        }
    }

    fn scale(&self) -> f32 {
        match self {
            Self::Surface(surface) => surface.scale(),
            #[cfg(test)]
            Self::Offscreen { scale, .. } => *scale,
        }
    }

    /// Asks for a translucent (or opaque) swapchain from the next frame
    /// on; see [`RenderSurface::request_transparent`]. An offscreen
    /// texture has no compositing mode and ignores it.
    fn request_transparent(&mut self, transparent: bool) {
        match self {
            Self::Surface(surface) => surface.request_transparent(transparent),
            #[cfg(test)]
            Self::Offscreen { .. } => {}
        }
    }

    fn resize(&mut self, gpu: &Gpu, size: PhysicalSize, scale: f32) {
        match self {
            Self::Surface(surface) => surface.resize(gpu, size, scale),
            #[cfg(test)]
            Self::Offscreen {
                target, scale: s, ..
            } => {
                *target = tessera_render::OffscreenTarget::new(gpu, size, target.format());
                *s = sanitize_scale(scale);
            }
        }
    }

    fn render(
        &mut self,
        gpu: &Gpu,
        renderer: &mut Renderer,
        frame: &FrameInput<'_>,
    ) -> Result<FrameStatus, RenderError> {
        match self {
            Self::Surface(surface) => surface.render(gpu, renderer, frame),
            #[cfg(test)]
            Self::Offscreen {
                not_ready: true, ..
            } => Ok(FrameStatus::NotReady),
            #[cfg(test)]
            Self::Offscreen { target, .. } => {
                target.render(gpu, renderer, frame)?;
                Ok(FrameStatus::Presented)
            }
        }
    }
}

/// What the last presented frame showed besides the cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shown {
    /// The cursor's cell; `None` while the view is scrolled back and draws
    /// no cursor.
    cursor: Option<Point>,
    focused: bool,
}

/// Whether `theme`'s background lets what is behind the view show through,
/// so its surface needs a per-pixel-alpha swapchain.
pub(crate) fn is_translucent(theme: &Theme) -> bool {
    theme.background.clamped().a < 1.0
}

/// A theme file's sRGB colour as the renderer's linear [`Rgba`].
fn rgba(color: tessera_config::Color) -> Rgba {
    Rgba::from_srgb8(color.r, color.g, color.b).with_alpha(f32::from(color.a) / 255.0)
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
    target: Target,
    renderer: Renderer,
    font_size_pt: f32,
    snapshot: Snapshot,
    /// Rows that changed since the last frame the renderer drew.
    pending: DamageRows,
    /// Cursor and focus of the last presented frame; `None` before the
    /// first one.
    shown: Option<Shown>,
    /// When the last presented frame was due on the [`MIN_FRAME_INTERVAL`]
    /// schedule (at most [`FRAME_SLACK`] before it was actually
    /// presented); `None` before the first one.
    last_presented: Option<Instant>,
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
            transparent: is_translucent(&theme),
            ..SurfaceOptions::default()
        };
        let surface = RenderSurface::attach(&gpu, surface, size, scale, options)?;
        Self::with_target(gpu, Target::Surface(surface), fonts, theme)
    }

    /// A view drawing into a headless `size` texture, for tests.
    #[cfg(test)]
    pub(crate) fn offscreen(
        gpu: Gpu,
        fonts: Fonts,
        theme: Theme,
        size: PhysicalSize,
        scale: f32,
    ) -> Result<Self, RenderError> {
        let target = Target::Offscreen {
            target: tessera_render::OffscreenTarget::new(
                &gpu,
                size,
                wgpu::TextureFormat::Rgba8UnormSrgb,
            ),
            scale: sanitize_scale(scale),
            not_ready: false,
        };
        Self::with_target(gpu, target, fonts, theme)
    }

    fn with_target(
        gpu: Gpu,
        target: Target,
        fonts: Fonts,
        theme: Theme,
    ) -> Result<Self, RenderError> {
        let font_size_pt = sanitize_font_size(theme.font_size);
        let shaper = Shaper::with_fonts(fonts, font_size_pt, target.scale());
        let renderer = Renderer::new(&gpu, theme, shaper)?;
        Ok(Self {
            gpu,
            target,
            renderer,
            font_size_pt,
            snapshot: Snapshot::default(),
            pending: DamageRows::All,
            shown: None,
            last_presented: None,
        })
    }

    /// The GPU this view draws with; [`Gpu::is_lost`] tells
    /// [`crate::App::render_view`] to detach it.
    pub(crate) fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    /// The surface's size, scale and the grid that fits in it.
    pub(crate) fn size(&self) -> ViewSize {
        let pixels = self.target.size();
        let scale = self.target.scale();
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

    /// The padding between each edge of the surface and the grid, in
    /// physical pixels: the theme's padding at the shaper's scale (capped
    /// at [`tessera_render::MAX_SCALE`]), rounded the way the renderer's
    /// layout and [`grid_size_for`] round it.
    pub(crate) fn padding_px(&self) -> u32 {
        whole_px(self.renderer.theme().padding * self.renderer.shaper().scale())
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
    /// shaper's backing scale in step with it. The next frame is drawn
    /// whole.
    pub(crate) fn resize(&mut self, size: PhysicalSize, scale: f32) {
        self.target.resize(&self.gpu, size, scale);
        self.renderer
            .set_font_size(self.font_size_pt, self.target.scale());
        self.pending = DamageRows::All;
    }

    /// Changes the font size (clamped like
    /// [`tessera_render::sanitize_font_size`]). The next frame is drawn
    /// whole.
    pub(crate) fn set_font_size(&mut self, size_pt: f32) {
        self.font_size_pt = sanitize_font_size(size_pt);
        self.renderer
            .set_font_size(self.font_size_pt, self.target.scale());
        self.pending = DamageRows::All;
    }

    /// Draws with `theme`'s colours, opacity and padding from the next
    /// frame on; the font stays as it is ([`Renderer::set_theme`]). The
    /// renderer then reports [`Renderer::needs_redraw`], so the next
    /// [`Self::render`] draws every row even with no damage.
    ///
    /// When the background turns translucent (alpha below 1) or opaque,
    /// the surface is switched to the matching compositing mode as the
    /// next frame starts ([`RenderSurface::request_transparent`]), so the
    /// layer is reconfigured on the thread that draws (the main thread)
    /// even when this runs on another one. An opaque swapchain ignores
    /// alpha and would show the background's premultiplied colour (each
    /// channel times alpha, so darker) as an opaque colour.
    pub(crate) fn set_appearance(&mut self, theme: Theme) {
        self.target.request_transparent(is_translucent(&theme));
        self.renderer.set_theme(theme);
    }

    /// The compositing modes the surface supports; `None` for an
    /// offscreen view.
    #[cfg(any(test, feature = "test-hooks"))]
    #[cfg_attr(
        not(test),
        expect(
            clippy::unnecessary_wraps,
            reason = "offscreen views (tests only) have no compositing mode"
        )
    )]
    pub(crate) fn supported_alpha_modes(&self) -> Option<Vec<wgpu::CompositeAlphaMode>> {
        match &self.target {
            Target::Surface(surface) => Some(surface.supported_alpha_modes(&self.gpu)),
            #[cfg(test)]
            Target::Offscreen { .. } => None,
        }
    }

    /// The surface's compositing mode in effect, and the one the next
    /// frame will switch to; `None` for an offscreen view.
    #[cfg(any(test, feature = "test-hooks"))]
    #[cfg_attr(
        not(test),
        expect(
            clippy::unnecessary_wraps,
            reason = "offscreen views (tests only) have no compositing mode"
        )
    )]
    pub(crate) fn alpha_modes(&self) -> Option<(wgpu::CompositeAlphaMode, bool)> {
        match &self.target {
            Target::Surface(surface) => Some((surface.alpha_mode(), surface.wants_transparent())),
            #[cfg(test)]
            Target::Offscreen { .. } => None,
        }
    }

    /// The theme the renderer draws with.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(crate) fn theme(&self) -> &Theme {
        self.renderer.theme()
    }

    /// Copies what changed in the rows `term` shows `offset` lines above
    /// its live screen (see [`Snapshot::capture`]), and whether its cursor
    /// is visible (DECTCEM), into the snapshot, and adds the rows that
    /// changed to the next frame's damage.
    ///
    /// This is the only part of a frame that runs under the session's
    /// terminal lock; shaping, drawing and waiting for a drawable happen
    /// after it is released. It copies only the rows that can have changed
    /// (comparing them first), so its cost follows the damage and the
    /// screen size, never the length of the scrollback.
    pub(crate) fn capture(&mut self, term: &Terminal, offset: usize) {
        let changed = self.snapshot.capture(term, offset);
        self.pending = self.pending.union(&changed);
    }

    /// Draws the last captured snapshot and presents it, or reports why
    /// nothing was drawn: [`FrameStatus::Skipped`] when nothing changed
    /// since the last presented frame, [`FrameStatus::Paced`] when the
    /// frame came earlier than the [`MIN_FRAME_INTERVAL`] schedule allows,
    /// and [`FrameStatus::NotReady`] when the surface had nothing to draw
    /// into (zero-sized, occluded, no drawable). The first two never touch
    /// the GPU. Damage of a frame that was not drawn carries over to the
    /// next; after `Paced` or `NotReady` the caller must render again for
    /// it to be drawn (a display link does so on its next tick).
    pub(crate) fn render(&mut self, focused: bool) -> Result<FrameStatus, RenderError> {
        let shown = Shown {
            cursor: self.snapshot.shown_cursor(),
            focused,
        };
        let changed =
            !self.pending.is_none() || self.shown != Some(shown) || self.renderer.needs_redraw();
        if !changed {
            return Ok(FrameStatus::Skipped);
        }
        let now = Instant::now();
        if is_paced(self.last_presented, now) {
            return Ok(FrameStatus::Paced);
        }
        let rows = self.snapshot.rows();
        let frame = self
            .snapshot
            .frame(&rows, focused)
            .with_damage(self.pending.clone());
        let status = self.target.render(&self.gpu, &mut self.renderer, &frame);
        match status {
            Ok(status) if status.is_drawn() => {
                self.pending = DamageRows::None;
                self.shown = Some(shown);
                self.last_presented = Some(due_time(self.last_presented, now));
            }
            // Not drawn: the damage stays pending for the next frame.
            Ok(_) => {}
            // The renderer rebuilds everything after an error; so does
            // the next frame here.
            Err(_) => self.pending = DamageRows::All,
        }
        status
    }

    /// Makes an offscreen view behave like a surface without a drawable
    /// (`true`) or draw again (`false`).
    #[cfg(test)]
    pub(crate) fn set_not_ready(&mut self, value: bool) {
        if let Target::Offscreen { not_ready, .. } = &mut self.target {
            *not_ready = value;
        }
    }

    /// The renderer, for its counters.
    #[cfg(test)]
    pub(crate) fn renderer(&self) -> &Renderer {
        &self.renderer
    }

    /// The last captured snapshot.
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// The last frame of an offscreen view, as RGBA bytes.
    #[cfg(test)]
    pub(crate) fn read_back(&self) -> Vec<u8> {
        match &self.target {
            Target::Offscreen { target, .. } => target.read_back(&self.gpu).expect("readback"),
            Target::Surface(_) => panic!("only offscreen views read back"),
        }
    }
}

/// Which lines a snapshot scrolled back into the history holds (ADR
/// 0023), to tell a moved view from output arriving under a still one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScrolledAt {
    /// The top row's line, numbered like [`Grid::scrollback_pushed`]
    /// (`pushed - offset`, wrapping): the same number on the same grid is
    /// the same line while no resize re-wraps the history.
    top: u64,
    /// [`Grid::scrollback_pushed`] at the capture.
    pushed: u64,
    /// [`Grid::scrollback_len`] at the capture.
    len: usize,
}

/// The rows in view, their grapheme clusters and the cursor of a grid,
/// copied so the terminal lock is held only for the copy. The buffers are
/// reused from frame to frame, and only rows that can have changed are
/// compared and copied again.
#[derive(Debug, Default)]
pub(crate) struct Snapshot {
    cells: Vec<Cell>,
    /// Each row's cluster text ([`tessera_grid::Line::graphemes`]).
    clusters: Vec<GraphemeTable>,
    /// Cells per row (every row of a grid has the same length).
    cols: usize,
    cursor: Point,
    /// The terminal hid its cursor (DECTCEM, `CSI ? 25 l`) at the last
    /// capture.
    cursor_hidden: bool,
    /// The cursor's shape at the last capture
    /// ([`Terminal::cursor_style`]); its blink bit is not drawn, so not
    /// kept.
    cursor_shape: CursorShape,
    /// The grid generation of the last capture; `None` before the first.
    generation: Option<u64>,
    /// The history lines the rows hold while the view is scrolled back;
    /// `None` while they are the live screen.
    scrolled: Option<ScrolledAt>,
    /// Rows compared by partial captures, for tests.
    #[cfg(test)]
    compared: usize,
}

impl Snapshot {
    /// Brings the snapshot up to date with the rows `term` shows `offset`
    /// lines above its live screen, its cursor, cursor visibility
    /// ([`Modes::cursor_visible`], one copy of the terminal's modes) and
    /// cursor shape ([`Terminal::cursor_style`]), and returns the rows that
    /// changed.
    ///
    /// At offset 0 (and always on the alternate screen, which shows no
    /// scrollback) the rows are the live screen: those whose cells or
    /// clusters changed (see [`Self::capture_grid`]) and the cursor's row
    /// when the cursor was shown or hidden or changed shape. A change of
    /// the blink bit alone damages nothing: blinking is not drawn.
    ///
    /// Above it (`offset` is clamped to the scrollback) the rows are the
    /// logical lines from `scrollback_len - offset` on: history lines, and
    /// the top of the live screen below them while `offset` is smaller
    /// than the screen; the cursor is not drawn, so its changes damage
    /// nothing. See [`Self::capture_history`].
    ///
    /// [`Modes::cursor_visible`]: tessera_term::Modes::cursor_visible
    pub(crate) fn capture(&mut self, term: &Terminal, offset: usize) -> DamageRows {
        let hidden = !term.modes().cursor_visible;
        let shape = render_shape(term.cursor_style().shape);
        let grid = term.grid();
        let offset = if grid.is_alt_screen() {
            0
        } else {
            offset.min(grid.scrollback_len())
        };
        if offset > 0 {
            self.cursor_hidden = hidden;
            self.cursor_shape = shape;
            return self.capture_history(grid, offset);
        }
        if self.scrolled.take().is_some() {
            // The rows hold history lines: copy the live screen whole.
            self.generation = None;
        }
        let changed = self.capture_grid(grid);
        if hidden == self.cursor_hidden && shape == self.cursor_shape {
            return changed;
        }
        self.cursor_hidden = hidden;
        self.cursor_shape = shape;
        changed.with_row(self.cursor.row)
    }

    /// Brings the snapshot up to date with `grid`'s logical lines from
    /// `scrollback_len - offset` on (`0 < offset <= scrollback_len`), and
    /// returns the rows whose cells or clusters changed.
    ///
    /// A view that moved (another top line, a first capture, another size
    /// or the live screen before) is copied whole and returns
    /// [`DamageRows::All`]. A view that stayed on its lines compares
    /// instead: every row when output moved the history (a line pushed,
    /// one evicted or cleared), otherwise only the live rows in view that
    /// the grid reports changed ([`Grid::damage_since`], `offset` rows
    /// lower). So output under a view deep in the history costs one
    /// comparison of the screen per capture, and an idle one nothing.
    fn capture_history(&mut self, grid: &Grid, offset: usize) -> DamageRows {
        let cols = usize::from(grid.cols());
        let rows = usize::from(grid.rows());
        let len = grid.scrollback_len();
        let pushed = grid.scrollback_pushed();
        let at = ScrolledAt {
            top: pushed.wrapping_sub(offset as u64),
            pushed,
            len,
        };
        let first = len - offset;
        let line = |r: usize| {
            grid.logical_line(first + r)
                .expect("the offset is within the scrollback, so every row is a logical line")
        };
        let last = self.scrolled.replace(at);
        let generation = self.generation.replace(grid.damage_generation());
        self.cursor = grid.cursor();
        let (Some(last), Some(generation)) = (last, generation) else {
            self.copy_all((0..rows).map(line), cols);
            return DamageRows::All;
        };
        if last.top != at.top || cols != self.cols || rows != self.clusters.len() {
            self.copy_all((0..rows).map(line), cols);
            return DamageRows::All;
        }
        let range = if last.pushed == pushed && last.len == len {
            // The history did not move: only live rows in view can have
            // changed, and the grid knows which.
            match grid.damage_since(generation) {
                DamageRows::None => return DamageRows::None,
                DamageRows::Rows(range) => {
                    (usize::from(*range.start()) + offset).min(rows)
                        ..(usize::from(*range.end()) + 1 + offset).min(rows)
                }
                DamageRows::All => 0..rows,
            }
        } else {
            0..rows
        };
        let mut changed = DamageRows::None;
        for r in range {
            if self.sync_row(r, line(r), cols) {
                changed = changed.with_row(u16::try_from(r).unwrap_or(u16::MAX));
            }
        }
        changed
    }

    /// Brings the snapshot up to date with `grid`'s visible rows and
    /// cursor, and returns the rows whose cells or clusters changed.
    ///
    /// Only the rows the grid reports changed since the last capture
    /// ([`Grid::damage_since`]) are compared and copied; the grid's
    /// report may include rows that did not change (the cursor row stays
    /// reported until the cursor leaves it), and the comparison drops
    /// those, so an idle grid returns [`DamageRows::None`]. A grid of
    /// another size, or another grid, is copied whole and returns
    /// [`DamageRows::All`].
    fn capture_grid(&mut self, grid: &Grid) -> DamageRows {
        let cols = usize::from(grid.cols());
        let lines = grid.visible_lines();
        let rows = lines.len();
        let damage = match self.generation {
            Some(generation) if cols == self.cols && rows == self.clusters.len() => {
                grid.damage_since(generation)
            }
            _ => DamageRows::All,
        };
        self.generation = Some(grid.damage_generation());
        self.cursor = grid.cursor();
        let range = match &damage {
            DamageRows::None => return DamageRows::None,
            DamageRows::Rows(range) => {
                usize::from(*range.start())..(usize::from(*range.end()) + 1).min(rows)
            }
            DamageRows::All => {
                self.copy_all(lines.iter(), cols);
                return DamageRows::All;
            }
        };
        let mut changed = DamageRows::None;
        for r in range {
            if self.sync_row(r, &lines[r], cols) {
                changed = changed.with_row(u16::try_from(r).unwrap_or(u16::MAX));
            }
        }
        changed
    }

    /// Makes row `r` hold `line` (`cols` cells and its clusters) and
    /// returns whether it did not already: a comparison first, a copy only
    /// when they differ.
    fn sync_row(&mut self, r: usize, line: &Line, cols: usize) -> bool {
        // Rows are `cols` long by construction; pad or cut defensively so
        // `rows()` can chunk evenly.
        let src = &line.cells[..line.cells.len().min(cols)];
        let dst = &mut self.cells[r * cols..(r + 1) * cols];
        #[cfg(test)]
        {
            self.compared += 1;
        }
        let same = dst[..src.len()] == *src
            && dst[src.len()..].iter().all(|c| *c == Cell::default())
            && self.clusters[r] == line.graphemes;
        if same {
            return false;
        }
        dst[..src.len()].copy_from_slice(src);
        dst[src.len()..].fill(Cell::default());
        self.clusters[r].clone_from(&line.graphemes);
        true
    }

    /// Replaces the whole snapshot with `lines`, `cols` cells each: one
    /// straight copy, no comparison and no default fill first (this runs
    /// under the terminal lock on every full-screen scroll, resize and
    /// move of a scrolled-back view).
    fn copy_all<'a>(
        &mut self,
        lines: impl ExactSizeIterator<Item = &'a Line> + Clone,
        cols: usize,
    ) {
        let count = lines.len();
        self.cols = cols;
        self.cells.clear();
        self.cells.reserve(count * cols);
        for line in lines.clone() {
            // Rows are `cols` long by construction; pad or cut defensively
            // so `rows()` can chunk evenly.
            let src = &line.cells[..line.cells.len().min(cols)];
            self.cells.extend_from_slice(src);
            if src.len() < cols {
                self.cells
                    .resize(self.cells.len() + cols - src.len(), Cell::default());
            }
        }
        self.clusters.truncate(count);
        let have = self.clusters.len();
        for (r, line) in lines.enumerate() {
            if r < have {
                self.clusters[r].clone_from(&line.graphemes);
            } else {
                self.clusters.push(line.graphemes.clone());
            }
        }
    }

    /// Each captured row's cluster text, for tests.
    #[cfg(test)]
    pub(crate) fn clusters(&self) -> &[GraphemeTable] {
        &self.clusters
    }

    /// The captured rows, top to bottom.
    pub(crate) fn rows(&self) -> Vec<&[Cell]> {
        if self.cols == 0 {
            return Vec::new();
        }
        self.cells.chunks(self.cols).collect()
    }

    /// The cursor cell a frame of this snapshot draws: `None` while the
    /// view is scrolled back, where the live cursor is off the rows shown.
    pub(crate) fn shown_cursor(&self) -> Option<Point> {
        self.scrolled.is_none().then_some(self.cursor)
    }

    /// The frame drawing `rows` (this snapshot's [`Self::rows`]) with the
    /// rows' grapheme clusters: a cursor of the captured shape at the
    /// captured cursor position, drawn unless the terminal hid it
    /// (DECTCEM) or the view is scrolled back, `focused` (an unfocused
    /// frame draws every shape hollow), no selection and no overlays, and
    /// the default damage ([`DamageRows::All`]). Cursor blink (a host
    /// clock) is still to come.
    pub(crate) fn frame<'a>(&'a self, rows: &'a [&'a [Cell]], focused: bool) -> FrameInput<'a> {
        let Point { row, col } = self.cursor;
        let visible = !self.cursor_hidden && self.scrolled.is_none();
        FrameInput::new(rows)
            .with_clusters(&self.clusters)
            .with_cursor(Some(
                CursorState::new(row, col, self.cursor_shape).with_visible(visible),
            ))
            .with_focus(focused)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

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
        let theme = theme_for(&config, &tessera_config::Theme::default());
        assert_eq!(theme.font_family, "Monaco");
        assert!((theme.font_size - tessera_render::MAX_FONT_SIZE_PT).abs() < f32::EPSILON);
        assert!((theme.background.a - 0.5).abs() < f32::EPSILON);
        assert_eq!(
            theme.foreground,
            Theme::default().foreground,
            "Tessera Dark has the renderer's default colours"
        );
    }

    /// Tessera Dark at the default configuration is exactly the renderer's
    /// default theme, so the bundled default changes nothing on screen.
    #[test]
    fn tessera_dark_is_the_renderer_default() {
        let theme = theme_for(&Config::default(), &tessera_config::Theme::default());
        assert_eq!(theme, Theme::default());
        assert!((theme.selection.a - 0.6).abs() < f32::EPSILON);
    }

    #[test]
    fn theme_colours_and_opacity() {
        use tessera_config::Color;

        let mut source = tessera_config::Theme::bundled("Dracula").unwrap().clone();
        let config = Config::default();
        let theme = theme_for(&config, &source);
        assert_eq!(theme.background, Rgba::from_srgb8(0x28, 0x2a, 0x36));
        assert_eq!(theme.foreground, Rgba::from_srgb8(0xf8, 0xf8, 0xf2));
        assert_eq!(theme.cursor, Rgba::from_srgb8(0xf8, 0xf8, 0xf2));
        assert_eq!(theme.selection, Rgba::from_srgb8(0x44, 0x47, 0x5a));
        assert_eq!(theme.ansi[9], Rgba::from_srgb8(0xff, 0x6e, 0x6e));
        for (i, (got, want)) in theme.ansi.iter().zip(source.ansi).enumerate() {
            assert_eq!(got.to_srgb8(), [want.r, want.g, want.b], "ansi {i}");
        }
        // The spec fixture's `theme_for` rows: theme alpha times opacity.
        for (background_alpha, opacity, expected) in [
            (0xff, 1.0, 1.0),
            (0xff, 0.5, 0.5),
            (0x80, 1.0, 128.0 / 255.0),
            (0x80, 0.5, 64.0 / 255.0),
            (0xff, 0.0, 0.0),
        ] {
            source.background = Color::rgb(0x28, 0x2a, 0x36).with_alpha(background_alpha);
            let mut config = Config::default();
            config.appearance.opacity = opacity;
            let a = theme_for(&config, &source).background.a;
            assert!(
                (a - expected).abs() < 1e-6,
                "alpha {background_alpha:#x} at opacity {opacity}: {a}"
            );
        }
        source.selection = Color::rgb(0x26, 0x4f, 0x78).with_alpha(0x99);
        let selection = theme_for(&config, &source).selection;
        assert!((selection.a - 0.6).abs() < f32::EPSILON);
        // Search highlights: the renderer's defaults unless the theme sets
        // them.
        let defaults = Theme::default();
        let theme = theme_for(&config, &source);
        assert_eq!(theme.search_match, defaults.search_match);
        assert_eq!(theme.search_match_current, defaults.search_match_current);
        source.search_match = Some(Color::rgb(0x11, 0x22, 0x33).with_alpha(0x80));
        source.search_match_current = Some(Color::rgb(0x44, 0x55, 0x66));
        let theme = theme_for(&config, &source);
        assert_eq!(
            theme.search_match,
            Rgba::from_srgb8(0x11, 0x22, 0x33).with_alpha(128.0 / 255.0)
        );
        assert_eq!(
            theme.search_match_current,
            Rgba::from_srgb8(0x44, 0x55, 0x66)
        );
    }

    #[test]
    fn dectcem_hides_the_cursor_and_damages_its_row() {
        let mut term = Terminal::new(3, 4);
        term.advance(b"\r\nab");
        let mut snapshot = Snapshot::default();
        snapshot.capture(&term, 0);
        let visible = |snapshot: &Snapshot| {
            let rows = snapshot.rows();
            snapshot
                .frame(&rows, true)
                .cursor
                .map(|cursor| cursor.visible)
        };
        assert_eq!(visible(&snapshot), Some(true));
        assert_eq!(snapshot.capture(&term, 0), DamageRows::None, "idle");
        term.advance(b"\x1b[?25l");
        assert_eq!(snapshot.capture(&term, 0), DamageRows::Rows(1..=1));
        assert_eq!(visible(&snapshot), Some(false));
        assert_eq!(snapshot.capture(&term, 0), DamageRows::None, "still hidden");
        term.advance(b"\x1b[?25h");
        assert_eq!(snapshot.capture(&term, 0), DamageRows::Rows(1..=1));
        assert_eq!(visible(&snapshot), Some(true));
    }

    #[test]
    fn the_frame_draws_the_snapshot_with_a_block_cursor() {
        let mut term = Terminal::new(3, 4);
        term.advance(b"ab\r\ncd");
        let mut snapshot = Snapshot::default();
        snapshot.capture(&term, 0);
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

    /// The steady underline the cursor-style tests configure, so the
    /// configured default and an explicit block differ.
    const UNDERLINE: CursorStyle = CursorStyle {
        shape: tessera_term::CursorShape::Underline,
        blink: false,
    };

    /// The shape of the cursor `snapshot`'s focused frame carries.
    fn frame_shape(snapshot: &Snapshot) -> Option<CursorShape> {
        let rows = snapshot.rows();
        snapshot
            .frame(&rows, true)
            .cursor
            .map(|cursor| cursor.shape)
    }

    /// A1: each DECSCUSR shape reaches the frame; Ps 0 is the configured
    /// default (here an underline, so it is not the block before it).
    #[test]
    fn decscusr_shapes_reach_the_frame() {
        let mut term = Terminal::new(3, 4);
        term.set_default_cursor_style(UNDERLINE);
        let mut snapshot = Snapshot::default();
        snapshot.capture(&term, 0);
        assert_eq!(frame_shape(&snapshot), Some(CursorShape::Underline));
        for (seq, shape) in [
            (&b"\x1b[5 q"[..], CursorShape::Beam),
            (b"\x1b[4 q", CursorShape::Underline),
            (b"\x1b[2 q", CursorShape::Block),
            (b"\x1b[0 q", CursorShape::Underline),
        ] {
            term.advance(seq);
            snapshot.capture(&term, 0);
            assert_eq!(frame_shape(&snapshot), Some(shape), "{seq:?}");
        }
    }

    /// A2 and D-FOCUS: DECTCEM hides every shape, and an unfocused frame
    /// draws every shape hollow.
    #[test]
    fn hidden_wins_over_the_shape_and_unfocused_is_hollow() {
        for (seq, shape) in [
            (&b"\x1b[2 q"[..], CursorShape::Block),
            (b"\x1b[4 q", CursorShape::Underline),
            (b"\x1b[6 q", CursorShape::Beam),
        ] {
            let mut term = Terminal::new(3, 4);
            term.advance(seq);
            let mut snapshot = Snapshot::default();
            snapshot.capture(&term, 0);
            let rows = snapshot.rows();
            let focused = snapshot.frame(&rows, true);
            assert_eq!(focused.drawn_cursor().map(|(_, s)| s), Some(shape));
            let unfocused = snapshot.frame(&rows, false);
            assert_eq!(
                unfocused.drawn_cursor().map(|(_, s)| s),
                Some(CursorShape::HollowBlock),
                "{seq:?} unfocused"
            );
            term.advance(b"\x1b[?25l");
            snapshot.capture(&term, 0);
            let rows = snapshot.rows();
            let frame = snapshot.frame(&rows, true);
            assert_eq!(
                frame.cursor.map(|c| (c.shape, c.visible)),
                Some((shape, false))
            );
            assert_eq!(frame.drawn_cursor(), None, "{seq:?} hidden");
        }
    }

    /// A3 and A4: a shape change damages the cursor's row and nothing
    /// else; the same shape again, Ps 0 on the default and a change of the
    /// blink bit alone damage nothing and leave the frame as it was.
    #[test]
    fn a_shape_change_damages_only_the_cursor_row() {
        let mut term = Terminal::new(4, 6);
        term.advance(b"ab\r\ncd\r\nef");
        let mut snapshot = Snapshot::default();
        snapshot.capture(&term, 0);
        assert_eq!(snapshot.capture(&term, 0), DamageRows::None, "idle");
        term.advance(b"\x1b[0 q");
        assert_eq!(
            snapshot.capture(&term, 0),
            DamageRows::None,
            "Ps 0 on the default"
        );
        term.advance(b"\x1b[2 q");
        assert_eq!(
            snapshot.capture(&term, 0),
            DamageRows::None,
            "a steady block again"
        );
        term.advance(b"\x1b[5 q");
        assert_eq!(snapshot.capture(&term, 0), DamageRows::Rows(2..=2), "beam");
        term.advance(b"\x1b[5 q");
        assert_eq!(snapshot.capture(&term, 0), DamageRows::None, "beam again");
        term.advance(b"\x1b[2 q");
        assert_eq!(snapshot.capture(&term, 0), DamageRows::Rows(2..=2), "block");
        let mut steady = Snapshot::default();
        steady.capture(&term, 0);
        term.advance(b"\x1b[1 q");
        assert!(term.cursor_style().blink);
        assert_eq!(snapshot.capture(&term, 0), DamageRows::None, "blink only");
        let (steady_rows, rows) = (steady.rows(), snapshot.rows());
        assert_eq!(
            snapshot.frame(&rows, true),
            steady.frame(&steady_rows, true),
            "blink only: the same frame"
        );
        // And a live default change reaches a session without a style.
        term.advance(b"\x1b[0 q");
        assert_eq!(
            snapshot.capture(&term, 0),
            DamageRows::None,
            "back to a steady block"
        );
        term.set_default_cursor_style(UNDERLINE);
        assert_eq!(
            snapshot.capture(&term, 0),
            DamageRows::Rows(2..=2),
            "new default"
        );
        assert_eq!(frame_shape(&snapshot), Some(CursorShape::Underline));
        term.set_default_cursor_style(UNDERLINE);
        assert_eq!(
            snapshot.capture(&term, 0),
            DamageRows::None,
            "the same default"
        );
    }

    /// A3 on a real view: an unchanged style is `Skipped`, a changed one
    /// is drawn.
    #[test]
    fn an_unchanged_style_skips_the_frame() {
        let Some(mut view) = offscreen_view(3, 10) else {
            return;
        };
        let mut term = Terminal::new(3, 10);
        view.capture(&term, 0);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        for (seq, status) in [
            (&b"\x1b[2 q"[..], FrameStatus::Skipped),
            (b"\x1b[0 q", FrameStatus::Skipped),
            (b"\x1b[1 q", FrameStatus::Skipped),
            (b"\x1b[4 q", FrameStatus::Presented),
            (b"\x1b[4 q", FrameStatus::Skipped),
        ] {
            next_frame();
            term.advance(seq);
            view.capture(&term, 0);
            assert_eq!(view.render(true).expect("frame"), status, "{seq:?}");
        }
    }

    /// A5: the pixels of the cursor's cell after DECSCUSR 5, 4 and 2, on a
    /// red background with a green cursor. The beam is a column band at
    /// the cell's left edge and the underline a row band at its bottom,
    /// each the renderer's one stroke wide (measured, and the two must
    /// agree: the stroke is not public); the block fills the cell. The
    /// cell to the right stays background.
    #[test]
    fn the_cursor_cell_shows_the_shape() {
        let Some(mut view) = offscreen_view(2, 4) else {
            return;
        };
        view.set_appearance(Theme {
            background: tessera_render::Rgba::new(1.0, 0.0, 0.0, 1.0),
            cursor: tessera_render::Rgba::new(0.0, 1.0, 0.0, 1.0),
            ..view.theme().clone()
        });
        let width = usize::try_from(view.size().pixels.width).expect("width");
        let pad = usize::try_from(view.padding_px()).expect("padding");
        let metrics = view.cell_metrics();
        let (cw, ch) = (
            usize::try_from(metrics.width_px).expect("cell width"),
            usize::try_from(metrics.height_px).expect("cell height"),
        );
        let mut term = Terminal::new(2, 4);
        let mut draw = |seq: &[u8]| {
            next_frame();
            term.advance(seq);
            view.capture(&term, 0);
            assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
            let image = view.read_back();
            // The cursor's cell and the one right of it, true where green.
            let cell = |col: usize| -> Vec<Vec<bool>> {
                (0..ch)
                    .map(|y| {
                        (0..cw)
                            .map(|x| {
                                let at = ((pad + y) * width + pad + col * cw + x) * 4;
                                match image[at..at + 4] {
                                    [0, 255, 0, 255] => true,
                                    [255, 0, 0, 255] => false,
                                    ref other => panic!("{seq:?}: pixel {x},{y} is {other:?}"),
                                }
                            })
                            .collect()
                    })
                    .collect()
            };
            assert!(cell(1).iter().flatten().all(|g| !g), "{seq:?}: next cell");
            cell(0)
        };
        let beam = draw(b"\x1b[5 q");
        let stroke = beam[0].iter().take_while(|g| **g).count();
        assert!((1..cw / 2).contains(&stroke), "beam {stroke} px of {cw}");
        for row in &beam {
            let expect: Vec<bool> = (0..cw).map(|x| x < stroke).collect();
            assert_eq!(*row, expect, "beam: left {stroke} columns only");
        }
        let underline = draw(b"\x1b[4 q");
        for (y, row) in underline.iter().enumerate() {
            assert!(
                row.iter().all(|g| *g == (y >= ch - stroke)),
                "underline row {y}"
            );
        }
        let block = draw(b"\x1b[2 q");
        assert!(block.iter().flatten().all(|g| *g), "block fills the cell");
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
        snapshot.capture(&term, 0);
        let rows = snapshot.rows();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.len() == 4));
        assert_eq!(rows[0][0].ch, 'a');
        assert_eq!(rows[1][1].ch, 'd');
        assert_eq!(snapshot.cursor, Point::new(1, 2));
        // Recapturing a smaller grid reuses the buffer and forgets the old rows.
        snapshot.capture(&Terminal::new(1, 2), 0);
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
        snapshot.capture(&term, 0);
        let mut best = Duration::MAX;
        for _ in 0..50 {
            // Forget the generation: time a full copy, the worst case.
            snapshot.generation = None;
            let start = Instant::now();
            snapshot.capture(&term, 0);
            best = best.min(start.elapsed());
        }
        assert_eq!(snapshot.rows().len(), 60);
        eprintln!("capturing 200x60 cells: best of 50 is {best:?}");
        assert!(
            best < Duration::from_millis(2),
            "capturing 200x60 cells took {best:?}"
        );
    }

    #[test]
    fn capture_reports_only_rows_that_changed() {
        let mut term = Terminal::new(4, 10);
        let mut snapshot = Snapshot::default();
        assert_eq!(snapshot.capture(&term, 0), DamageRows::All, "first capture");
        assert_eq!(snapshot.capture(&term, 0), DamageRows::None, "idle");
        term.advance(b"hi");
        assert_eq!(snapshot.capture(&term, 0), DamageRows::Rows(0..=0));
        // The grid keeps reporting the cursor row; the comparison drops it.
        assert_eq!(
            term.grid().damage_since(term.grid().damage().generation),
            DamageRows::Rows(0..=0)
        );
        assert_eq!(snapshot.capture(&term, 0), DamageRows::None);
        // A cursor move changes no cells.
        term.advance(b"\x1b[3;5H");
        assert_eq!(snapshot.capture(&term, 0), DamageRows::None);
        assert_eq!(snapshot.cursor, Point::new(2, 4));
        term.advance(b"x\x1b[1;1Hy");
        assert_eq!(snapshot.capture(&term, 0), DamageRows::Rows(0..=2));
        assert_eq!(snapshot.rows()[2][4].ch, 'x');
        assert_eq!(snapshot.rows()[0][0].ch, 'y');
        // Clusters are copied with their row.
        term.advance(b"\x1b[4;1He\xcc\x81");
        assert_eq!(snapshot.capture(&term, 0), DamageRows::Rows(3..=3));
        assert_eq!(snapshot.clusters[3].get(0), Some("e\u{301}"));
        term.resize(5, 10);
        assert_eq!(snapshot.capture(&term, 0), DamageRows::All);
        assert_eq!(snapshot.rows().len(), 5);
    }

    /// A headless view of `rows` x `cols` Menlo 13 pt cells at 1x, or
    /// `None` when GPU tests are skipped.
    fn offscreen_view(rows: u16, cols: u16) -> Option<View> {
        if std::env::var_os("TESSERA_SKIP_GPU_TESTS").is_some() {
            return None;
        }
        let gpu = Gpu::headless().expect("a Metal adapter");
        let fonts = Fonts::new(&tessera_render::FontConfig {
            family: "Menlo".into(),
            size_pt: 13.0,
            scale: 1.0,
            fallback: Vec::new(),
        })
        .expect("Menlo ships with macOS");
        let theme = Theme {
            font_family: "Menlo".into(),
            font_size: 13.0,
            ..Theme::default()
        };
        let size = PhysicalSize::new(
            u32::from(cols) * MENLO_1X.width_px + 8,
            u32::from(rows) * MENLO_1X.height_px + 8,
        );
        let view = View::offscreen(gpu, fonts, theme, size, 1.0).expect("view");
        assert_eq!(view.cell_metrics(), MENLO_1X);
        Some(view)
    }

    /// Waits out the frame interval, so the next render is not paced.
    fn next_frame() {
        std::thread::sleep(MIN_FRAME_INTERVAL + Duration::from_millis(1));
    }

    #[test]
    fn the_same_snapshot_twice_is_presented_then_skipped() {
        let Some(mut view) = offscreen_view(4, 20) else {
            return;
        };
        let mut term = Terminal::new(4, 20);
        term.advance(b"hello");
        view.capture(&term, 0);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        let frames = view.renderer().stats().frames;
        next_frame();
        view.capture(&term, 0);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Skipped);
        assert_eq!(view.renderer().stats().frames, frames, "no GPU work");
        // Without even a capture in between, and however soon: an
        // unchanged frame is never paced.
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Skipped);
        view.last_presented = Some(Instant::now());
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Skipped);
    }

    #[test]
    fn a_one_row_change_reshapes_exactly_that_row() {
        let Some(mut view) = offscreen_view(6, 30) else {
            return;
        };
        let mut term = Terminal::new(6, 30);
        for row in 0..6 {
            term.advance(format!("\x1b[{};1Hrow {row} of text", row + 1).as_bytes());
        }
        view.capture(&term, 0);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        let before = view.renderer().stats();
        next_frame();
        term.advance(b"\x1b[3;1Hchanged");
        view.capture(&term, 0);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        let after = view.renderer().stats();
        assert_eq!(after.rows_shaped - before.rows_shaped, 1, "only row 2");
        assert_eq!(after.frames - before.frames, 1);
    }

    #[test]
    fn cursor_and_focus_changes_draw_without_shaping() {
        let Some(mut view) = offscreen_view(3, 10) else {
            return;
        };
        let mut term = Terminal::new(3, 10);
        term.advance(b"abc");
        view.capture(&term, 0);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        let shaped = view.renderer().stats().rows_shaped;
        next_frame();
        term.advance(b"\x1b[2;2H");
        view.capture(&term, 0);
        assert_eq!(
            view.render(true).expect("frame"),
            FrameStatus::Presented,
            "cursor moved"
        );
        next_frame();
        assert_eq!(
            view.render(false).expect("frame"),
            FrameStatus::Presented,
            "focus lost"
        );
        next_frame();
        assert_eq!(view.render(false).expect("frame"), FrameStatus::Skipped);
        assert_eq!(
            view.renderer().stats().rows_shaped,
            shaped,
            "no row re-shaped"
        );
    }

    #[test]
    fn frames_within_one_interval_are_paced_and_damage_carries_over() {
        let Some(mut view) = offscreen_view(3, 10) else {
            return;
        };
        let mut term = Terminal::new(3, 10);
        view.capture(&term, 0);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        term.advance(b"one");
        view.capture(&term, 0);
        // A frame was just presented: the next one is not due for an
        // interval, and half an interval of slack does not cover that.
        view.last_presented = Some(Instant::now());
        let frames = view.renderer().stats().frames;
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Paced);
        assert_eq!(view.renderer().stats().frames, frames, "no GPU work");
        // Within the slack of its due time it is on time, and draws the
        // row the paced call captured.
        let shaped = view.renderer().stats().rows_shaped;
        view.last_presented =
            Instant::now().checked_sub(MIN_FRAME_INTERVAL.saturating_sub(FRAME_SLACK / 2));
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        assert_eq!(
            view.renderer().stats().rows_shaped - shaped,
            1,
            "row 0 kept its damage"
        );
    }

    /// Real renders with new content each, on the real clock: the first
    /// two within one interval are on time (the second within the slack),
    /// the third is `Paced`, and after the interval the paced content is
    /// `Presented`. A run in which drawing took so long that the three did
    /// not fit in one interval proves nothing and is repeated.
    #[test]
    fn three_renders_within_one_interval_are_paced_then_presented() {
        let Some(mut view) = offscreen_view(3, 10) else {
            return;
        };
        let mut term = Terminal::new(3, 10);
        let mut paced = 0;
        for round in 0..20 {
            next_frame();
            next_frame();
            let start = Instant::now();
            let mut statuses = Vec::new();
            for ch in [b"a", b"b", b"c"] {
                term.advance(ch);
                view.capture(&term, 0);
                statuses.push(view.render(true).expect("frame"));
            }
            let fit = Instant::now() < start + MIN_FRAME_INTERVAL;
            assert_eq!(statuses[..2], [FrameStatus::Presented; 2], "round {round}");
            if !fit {
                continue;
            }
            assert_eq!(statuses[2], FrameStatus::Paced, "round {round}");
            paced += 1;
            let frames = view.renderer().stats().frames;
            next_frame();
            assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
            assert_eq!(view.renderer().stats().frames, frames + 1);
            next_frame();
            assert_eq!(view.render(true).expect("frame"), FrameStatus::Skipped);
            if paced == 3 {
                return;
            }
        }
        panic!("three renders never fit in one interval ({paced} did)");
    }

    /// A surface without a drawable reports `NotReady`, draws nothing and
    /// keeps the damage, which the next ready frame draws; it is not
    /// counted on the pacing schedule.
    #[test]
    fn a_surface_that_is_not_ready_keeps_the_frame_pending() {
        let Some(mut view) = offscreen_view(3, 10) else {
            return;
        };
        let mut term = Terminal::new(3, 10);
        term.advance(b"abc");
        view.capture(&term, 0);
        view.set_not_ready(true);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::NotReady);
        assert_eq!(view.renderer().stats().frames, 0, "nothing drawn");
        assert_eq!(view.last_presented, None, "not on the schedule");
        // Retried at once (a not-ready frame is not paced): still not ready.
        assert_eq!(view.render(true).expect("frame"), FrameStatus::NotReady);
        view.set_not_ready(false);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        assert_eq!(view.renderer().stats().frames, 1);

        // A one-row change while not ready is drawn, that row only, by the
        // first ready frame; the cursor stays where it was.
        next_frame();
        term.advance(b"\x1b[2;1Hx\x1b[1;4H");
        view.capture(&term, 0);
        view.set_not_ready(true);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::NotReady);
        view.set_not_ready(false);
        let shaped = view.renderer().stats().rows_shaped;
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        assert_eq!(
            view.renderer().stats().rows_shaped - shaped,
            1,
            "row 1 kept its damage"
        );
        assert_eq!(view.renderer().stats().frames, 2);

        // Not ready with nothing pending is `Skipped`: nothing to retry.
        next_frame();
        view.set_not_ready(true);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Skipped);
    }

    /// A display link's jitter never makes its tick paced, at 60, 120,
    /// 144, 165 and 240 Hz: hand-picked late and early ticks (one tick 3 ms
    /// late is the case where a 1 ms slack paced the next, on-time tick,
    /// and a host that stops its link on `Skipped` lost the burst's last
    /// frame), then 20 000 ticks each late by a pseudo-random amount below
    /// 95% of a period (a link delivers its ticks in order).
    #[test]
    fn display_link_ticks_are_never_paced() {
        let start = Instant::now();
        let ms = |v: f64| Duration::from_secs_f64(v / 1000.0);
        for (period, jitter) in [
            (16.667, &[0.0, 4.0, -4.0, 4.0, 0.0, 15.0, 0.0][..]),
            (
                8.333,
                &[0.0, 3.0, 0.0, 0.0, -2.0, 4.0, -4.0, 0.0, 1.0, 7.9, 0.0][..],
            ),
            (6.944, &[0.0, 3.0, 0.0, -3.0, 3.0, 6.5, 0.0][..]),
            (6.061, &[0.0, 3.0, 0.0, -3.0, 3.0, 5.7, 0.0][..]),
            (4.167, &[0.0, 2.0, 0.0, -2.0, 2.0, 3.9, 0.0][..]),
        ] {
            let mut last = None;
            for (k, j) in jitter.iter().enumerate() {
                #[expect(clippy::cast_precision_loss, reason = "a few ticks")]
                let now = start + ms(20.0 + period * k as f64 + j);
                assert!(
                    !is_paced(last, now),
                    "tick {k} at {period} ms, jitter {j} ms"
                );
                last = Some(due_time(last, now));
            }
        }
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        for period in [16.667, 8.333, 6.944, 6.061, 4.167] {
            let mut last = None;
            for k in 0..20_000u32 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                #[expect(clippy::cast_precision_loss, reason = "a fraction")]
                let late = (seed % 1000) as f64 / 1000.0 * 0.95 * period;
                let now = start + ms(20.0 + period * f64::from(k) + late);
                assert!(
                    !is_paced(last, now),
                    "tick {k} at {period} ms, {late:.2} ms late"
                );
                last = Some(due_time(last, now));
            }
        }
        // Renders faster than the display: the schedule lets the first two
        // through (one may come up to the slack early) and paces the rest
        // until the next interval (4.17 ms).
        let now = start + ms(50.0);
        let mut last = None;
        for k in 0..2 {
            let t = now + ms(f64::from(k));
            assert!(!is_paced(last, t), "render {k}");
            last = Some(due_time(last, t));
        }
        assert!(is_paced(last, now + ms(2.0)));
        assert!(is_paced(last, now + ms(3.5)));
        assert!(!is_paced(last, now + ms(4.3)), "on time again");
    }

    /// A flood: 10 000 4 KiB reads of `yes` output while the view
    /// captures and renders in a loop for two seconds. Frames are paced:
    /// each presented frame is due an interval `I` (1/240 s) after the
    /// previous one's due time, may come [`FRAME_SLACK`] (`I / 2`) early,
    /// and its due time never lags the clock by more than the slack. So
    /// the first and last of `n` frames are at least `(n - 1) * I - 2 *
    /// slack = (n - 2) * I` apart, and two seconds hold at most `2 s / I +
    /// 2 = 482` frames. A debug build presents about 350 to 481 (the loop
    /// is fast enough to hit the limit on an idle machine) and, without
    /// pacing, about 1 000, so removing the pacing fails the test. The
    /// flood never starves the frames below 30 per second, and the
    /// snapshot's memory does not grow with the output.
    #[test]
    fn a_flood_renders_at_display_rate() {
        let Some(mut view) = offscreen_view(24, 80) else {
            return;
        };
        let mut term = Terminal::new(24, 80);
        let chunk: Vec<u8> = b"y\n".repeat(2048);
        let window = Duration::from_secs(2);
        let start = Instant::now();
        let (mut fed, mut presented, mut calls) = (0, 0u32, 0u32);
        let mut longest_capture = Duration::ZERO;
        let mut capacity = None;
        while start.elapsed() < window {
            if fed < 10_000 {
                term.advance(&chunk);
                fed += 1;
            }
            let t = Instant::now();
            view.capture(&term, 0);
            longest_capture = longest_capture.max(t.elapsed());
            calls += 1;
            if view.render(true).expect("frame") == FrameStatus::Presented {
                presented += 1;
            }
            let cap = (
                view.snapshot.cells.capacity(),
                view.snapshot.clusters.capacity(),
            );
            assert!(capacity.is_none_or(|c| c == cap), "snapshot grew: {cap:?}");
            capacity = Some(cap);
        }
        eprintln!(
            "flood: {fed} reads, {calls} calls, {presented} frames presented in {window:?}; \
             longest capture {longest_capture:?}"
        );
        let max = 2 * 240 + 2;
        assert!(presented <= max, "{presented} frames > {max}: not paced");
        assert!(presented >= 2 * 30, "{presented} frames < 60: starved");
    }

    /// The snapshot's rows and clusters exactly as `term`'s grid holds them.
    fn assert_snapshot_is(snapshot: &Snapshot, term: &Terminal) {
        let lines = term.grid().visible_lines();
        let rows = snapshot.rows();
        assert_eq!(rows.len(), lines.len());
        for (r, (row, line)) in rows.iter().zip(lines).enumerate() {
            assert_eq!(*row, line.cells.as_slice(), "row {r} cells");
            assert_eq!(snapshot.clusters[r], line.graphemes, "row {r} clusters");
        }
        assert_eq!(snapshot.clusters.len(), lines.len());
        assert_eq!(snapshot.cursor, term.grid().cursor());
    }

    /// A full capture (the path every full-screen scroll and resize takes,
    /// under the terminal lock) copies the grid exactly, whatever the
    /// snapshot held before, and compares no row on the way.
    #[test]
    fn a_full_capture_is_an_exact_copy_without_comparisons() {
        let mut snapshot = Snapshot::default();
        let mut term = Terminal::new(5, 12);
        term.advance("e\u{301}x 中文 👍🏽\r\n\x1b[41mred\x1b[0m a\u{338}\r\n".as_bytes());
        assert_eq!(snapshot.capture(&term, 0), DamageRows::All);
        assert_snapshot_is(&snapshot, &term);
        assert_eq!(snapshot.compared, 0, "no comparison on the full path");
        // Partial captures compare only the reported rows.
        term.advance(b"\x1b[3;1Hzz");
        assert_eq!(snapshot.capture(&term, 0), DamageRows::Rows(2..=2));
        assert!(snapshot.compared > 0);
        assert_snapshot_is(&snapshot, &term);
        // Smaller, then larger again: stale rows and clusters are replaced.
        for (rows, cols) in [(3, 7), (8, 20), (8, 20)] {
            term.resize(rows, cols);
            term.advance("\x1b[1;1H❤\u{fe0f}e\u{301}".as_bytes());
            let compared = snapshot.compared;
            snapshot.generation = None;
            assert_eq!(snapshot.capture(&term, 0), DamageRows::All);
            assert_eq!(
                snapshot.compared, compared,
                "no comparison on the full path"
            );
            assert_snapshot_is(&snapshot, &term);
        }
    }

    /// A red theme with blue text: every pixel the grid does not draw is
    /// red, and kept rows' glyph instances must be rebuilt for the text.
    fn red() -> Theme {
        Theme {
            background: tessera_render::Rgba::new(1.0, 0.0, 0.0, 1.0),
            foreground: tessera_render::Rgba::new(0.0, 0.0, 1.0, 1.0),
            ..Theme::default()
        }
    }

    /// A re-theme with nothing else changed is drawn, never skipped as
    /// unchanged: `Renderer::set_theme` makes the renderer ask for a redraw.
    #[test]
    fn a_theme_change_is_drawn_without_damage() {
        let Some(mut view) = offscreen_view(3, 10) else {
            return;
        };
        let mut term = Terminal::new(3, 10);
        term.advance(b"abc");
        view.capture(&term, 0);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        next_frame();
        view.capture(&term, 0);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Skipped);
        view.set_appearance(red());
        next_frame();
        view.capture(&term, 0);
        let shaped = view.renderer().stats().rows_shaped;
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        assert_eq!(view.renderer().stats().rows_shaped - shaped, 3, "every row");
        let image = view.read_back();
        // The padding corner and a blank cell are the new background.
        assert_eq!(image[..4], [255, 0, 0, 255], "the corner is red");
        let last = image.len() - 4;
        assert_eq!(image[last..], [255, 0, 0, 255]);
        next_frame();
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Skipped);
    }

    /// A re-theme together with a one-row change draws every row in the
    /// new theme, the changed one included: the same pixels as a new view
    /// drawing that screen in that theme.
    #[test]
    fn a_theme_change_with_partial_damage_redraws_everything() {
        let (Some(mut view), Some(mut fresh)) = (offscreen_view(3, 10), offscreen_view(3, 10))
        else {
            return;
        };
        let mut term = Terminal::new(3, 10);
        term.advance(b"abc\r\ndef");
        view.capture(&term, 0);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        next_frame();
        term.advance(b"\x1b[3;1Hxyz");
        view.capture(&term, 0);
        view.set_appearance(red());
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        fresh.set_appearance(red());
        fresh.capture(&term, 0);
        assert_eq!(fresh.render(true).expect("frame"), FrameStatus::Presented);
        assert!(
            view.read_back() == fresh.read_back(),
            "every row in the new theme"
        );
    }

    // ------------------------------------------------- scrollback viewport

    /// The text of `snapshot`'s rows, trailing blanks trimmed.
    fn texts(snapshot: &Snapshot) -> Vec<String> {
        snapshot
            .rows()
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.ch)
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// A 4x10 terminal with `L0`..`L{n-1}` written one per line: the first
    /// `n - 3` lines are in the scrollback, the last three on the screen
    /// above the cursor's empty row.
    fn numbered_terminal(n: usize) -> Terminal {
        let mut term = Terminal::new(4, 10);
        for i in 0..n {
            term.advance(format!("L{i}\r\n").as_bytes());
        }
        term
    }

    /// Whether `snapshot`'s focused frame draws a cursor.
    fn cursor_drawn(snapshot: &Snapshot) -> bool {
        let rows = snapshot.rows();
        snapshot.frame(&rows, true).drawn_cursor().is_some()
    }

    /// The offset that keeps `top` (a [`ScrolledAt::top`]) in view: how a
    /// session's anchor resolves while output continues.
    fn anchored(term: &Terminal, top: u64) -> usize {
        usize::try_from(term.grid().scrollback_pushed() - top).expect("offset")
    }

    #[test]
    fn a_scrolled_back_snapshot_shows_the_history_at_its_offset() {
        let term = numbered_terminal(10);
        assert_eq!(term.grid().scrollback_len(), 7);
        let mut snapshot = Snapshot::default();
        assert_eq!(snapshot.capture(&term, 0), DamageRows::All);
        assert_eq!(texts(&snapshot), ["L7", "L8", "L9", ""]);
        assert!(cursor_drawn(&snapshot));
        assert_eq!(snapshot.shown_cursor(), Some(Point::new(3, 0)));
        // Two lines back: two history lines over the top of the screen.
        assert_eq!(snapshot.capture(&term, 2), DamageRows::All);
        assert_eq!(texts(&snapshot), ["L5", "L6", "L7", "L8"]);
        assert!(!cursor_drawn(&snapshot), "no cursor while scrolled back");
        assert_eq!(snapshot.shown_cursor(), None);
        // Deeper than the screen: history only.
        snapshot.capture(&term, 6);
        assert_eq!(texts(&snapshot), ["L1", "L2", "L3", "L4"]);
        // Past the oldest line: the oldest line on top.
        snapshot.capture(&term, 1_000);
        assert_eq!(texts(&snapshot), ["L0", "L1", "L2", "L3"]);
        // And back to the live screen, whole, with its cursor.
        assert_eq!(snapshot.capture(&term, 0), DamageRows::All);
        assert_snapshot_is(&snapshot, &term);
        assert!(cursor_drawn(&snapshot));
    }

    /// The Done-when case at the snapshot: a view 50 lines back while
    /// 1 000 more lines arrive keeps byte-identical rows and damages
    /// nothing; a cursor that moves or hides under it damages nothing.
    #[test]
    fn output_under_a_view_deep_in_the_history_damages_nothing() {
        let mut term = numbered_terminal(200);
        let mut snapshot = Snapshot::default();
        snapshot.capture(&term, 50);
        let top = snapshot.scrolled.expect("scrolled").top;
        let before = (snapshot.cells.clone(), snapshot.clusters.clone());
        assert_eq!(texts(&snapshot)[0], "L147");
        for i in 200..1_200 {
            term.advance(format!("L{i}\r\n").as_bytes());
            assert_eq!(
                snapshot.capture(&term, anchored(&term, top)),
                DamageRows::None,
                "line {i}"
            );
        }
        assert_eq!(anchored(&term, top), 1_050);
        term.advance(b"\x1b[2;3Hxy\x1b[?25l\x1b[5 q");
        assert_eq!(snapshot.capture(&term, 1_050), DamageRows::None);
        assert_eq!((snapshot.cells.clone(), snapshot.clusters.clone()), before);
    }

    /// A view less than a screen back shows the top of the live screen:
    /// output there damages exactly the rows it changed, and output that
    /// scrolls those rows into the history moves nothing.
    #[test]
    fn output_under_a_shallow_view_damages_the_rows_it_changes() {
        let mut term = numbered_terminal(10);
        let mut snapshot = Snapshot::default();
        snapshot.capture(&term, 1);
        assert_eq!(texts(&snapshot), ["L6", "L7", "L8", "L9"]);
        let top = snapshot.scrolled.expect("scrolled").top;
        // Screen row 1 (`L8`) is view row 2.
        term.advance(b"\x1b[2;3Hx");
        assert_eq!(snapshot.capture(&term, 1), DamageRows::Rows(2..=2));
        assert_eq!(texts(&snapshot), ["L6", "L7", "L8x", "L9"]);
        // The cursor row (screen row 1) stays reported; it is compared.
        assert_eq!(snapshot.capture(&term, 1), DamageRows::None);
        // Typing on the cursor's row: the screen's last row is not in view.
        term.advance(b"\x1b[4;1Hhidden");
        assert_eq!(snapshot.capture(&term, 1), DamageRows::None);
        // Ten lines later the rows in view are all history, unchanged.
        term.advance(&b"\r\n".repeat(10));
        assert_eq!(
            snapshot.capture(&term, anchored(&term, top)),
            DamageRows::None
        );
        assert_eq!(texts(&snapshot), ["L6", "L7", "L8x", "L9"]);
        // A line that was still being written when the view was taken
        // shows its new text when it reaches the view.
        let mut term = numbered_terminal(10);
        term.advance(b"half");
        snapshot.capture(&term, 3);
        let top = snapshot.scrolled.expect("scrolled").top;
        assert_eq!(texts(&snapshot), ["L4", "L5", "L6", "L7"]);
        snapshot.capture(&term, 1);
        let top_one = snapshot.scrolled.expect("scrolled").top;
        assert_ne!(top, top_one);
        assert_eq!(texts(&snapshot), ["L6", "L7", "L8", "L9"]);
        snapshot.capture(&term, 0);
        snapshot.capture(&term, 1);
        assert_eq!(texts(&snapshot)[3], "L9");
        term.advance(b" done\r\n\r\n");
        let offset = anchored(&term, top_one);
        assert_eq!(
            snapshot.capture(&term, offset),
            DamageRows::None,
            "L6..L9 as they were"
        );
        snapshot.capture(&term, offset - 1);
        assert_eq!(texts(&snapshot)[3], "half done");
    }

    /// Moving the view redraws every row, staying on its lines redraws
    /// nothing, and the live screen comes back whole.
    #[test]
    fn moving_the_view_damages_every_row() {
        let term = numbered_terminal(30);
        let mut snapshot = Snapshot::default();
        snapshot.capture(&term, 0);
        for offset in [3, 4, 27, 1] {
            assert_eq!(snapshot.capture(&term, offset), DamageRows::All, "{offset}");
            assert_eq!(
                snapshot.capture(&term, offset),
                DamageRows::None,
                "{offset} again"
            );
        }
        assert_eq!(snapshot.capture(&term, 0), DamageRows::All);
        assert_snapshot_is(&snapshot, &term);
        assert_eq!(snapshot.capture(&term, 0), DamageRows::None);
    }

    /// The alternate screen shows no scrollback: any offset draws the live
    /// alternate screen with its cursor.
    #[test]
    fn the_alternate_screen_shows_no_scrollback() {
        let mut term = numbered_terminal(30);
        term.advance(b"\x1b[?1049h\x1b[Hfull screen");
        let mut snapshot = Snapshot::default();
        snapshot.capture(&term, 5);
        assert_snapshot_is(&snapshot, &term);
        assert_eq!(texts(&snapshot)[0], "full scree");
        assert!(cursor_drawn(&snapshot));
        assert_eq!(
            snapshot.capture(&term, 0),
            DamageRows::None,
            "the same screen"
        );
    }

    /// Fast path against reference: after any sequence of output (lines,
    /// edits on the screen, clears, RIS, resizes, the alternate screen)
    /// and views (anchored, moved, live), a snapshot kept up to date shows
    /// exactly what a fresh one does, and the damage it reports covers
    /// every row that changed. 2 000 seeded runs of 40 steps.
    #[test]
    fn scrolled_captures_agree_with_a_fresh_capture() {
        const PIECES: [&[u8]; 12] = [
            b"line\r\n",
            b"abcdefghij\r\n",
            b"\r\n\r\n\r\n",
            b"\x1b[2;2Hzz",
            b"\x1b[1;1H\xe4\xb8\xad\xcc\x81",
            b"\x1b[41m \x1b[0m",
            b"\x1b[2J",
            b"\x1b[3J",
            b"\x1bc",
            b"\x1b[?1049h",
            b"\x1b[?1049l",
            b"\x1b[2;3r\n\n\n\x1b[r",
        ];
        let mut seed = 0x31_u64;
        let mut next = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for run in 0..2_000 {
            let mut term = Terminal::new(4, 6);
            let mut snapshot = Snapshot::default();
            let mut offset = 0;
            for step in 0..40 {
                match next(10) {
                    0..=5 => {
                        let piece = PIECES[usize::try_from(next(12)).expect("index")];
                        term.advance(piece);
                    }
                    6 => {
                        let rows = u16::try_from(1 + next(6)).expect("rows");
                        let cols = u16::try_from(1 + next(8)).expect("cols");
                        term.resize(rows, cols);
                    }
                    _ => offset = usize::try_from(next(20)).expect("offset"),
                }
                // Half the time the view keeps its lines, as a session's
                // anchor does.
                if let Some(at) = snapshot.scrolled.filter(|_| next(2) == 0) {
                    let pushed = term.grid().scrollback_pushed();
                    offset = usize::try_from(pushed.wrapping_sub(at.top)).unwrap_or(0);
                }
                let before = snapshot
                    .rows()
                    .iter()
                    .map(|r| r.to_vec())
                    .collect::<Vec<_>>();
                let clusters_before = snapshot.clusters.clone();
                let damage = snapshot.capture(&term, offset);
                let mut fresh = Snapshot::default();
                fresh.capture(&term, offset);
                let at = format!("run {run} step {step} offset {offset}");
                assert_eq!(snapshot.rows(), fresh.rows(), "{at}");
                assert_eq!(snapshot.clusters, fresh.clusters, "{at}");
                assert_eq!(snapshot.shown_cursor(), fresh.shown_cursor(), "{at}");
                assert_eq!(cursor_drawn(&snapshot), cursor_drawn(&fresh), "{at}");
                let rows = snapshot.rows();
                if before.len() == rows.len() && damage != DamageRows::All {
                    for (r, row) in rows.iter().enumerate() {
                        let changed =
                            before[r] != *row || clusters_before[r] != snapshot.clusters[r];
                        let reported = damage.contains(u16::try_from(r).expect("row"));
                        assert!(!changed || reported, "{at}: row {r} changed unreported");
                    }
                }
            }
        }
    }

    /// The Done-when frame check: a view scrolled back is drawn once, then
    /// skipped while output arrives under it; moving it draws again.
    #[test]
    fn a_still_scrolled_view_skips_frames_while_output_continues() {
        let Some(mut view) = offscreen_view(4, 10) else {
            return;
        };
        let mut term = numbered_terminal(60);
        view.capture(&term, 0);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        next_frame();
        view.capture(&term, 20);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        let top = view.snapshot.scrolled.expect("scrolled").top;
        let frames = view.renderer().stats().frames;
        for i in 0..200 {
            next_frame();
            // New lines, and the cursor moved and hidden under the view.
            term.advance(
                format!("more {i}\r\n\x1b[?25l\x1b[{};{}H", i % 4 + 1, i % 9 + 1).as_bytes(),
            );
            view.capture(&term, anchored(&term, top));
            assert_eq!(
                view.render(true).expect("frame"),
                FrameStatus::Skipped,
                "{i}"
            );
        }
        assert_eq!(view.renderer().stats().frames, frames, "no GPU work");
        next_frame();
        view.capture(&term, anchored(&term, top) + 1);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        let shaped = view.renderer().stats().rows_shaped;
        next_frame();
        view.capture(&term, 0);
        assert_eq!(view.render(true).expect("frame"), FrameStatus::Presented);
        assert_eq!(view.renderer().stats().rows_shaped - shaped, 4, "every row");
    }

    /// A 50x200 grid with `history` lines of 200-column text in its
    /// scrollback (the perf corpus's screen; the terminal's own cap is
    /// 10 000, so the grid is built directly).
    fn long_history(history: usize) -> Grid {
        let mut grid = Grid::with_scrollback(50, 200, history);
        let mut text = String::new();
        while text.len() < 200 {
            text.push_str(ASCII);
        }
        text.truncate(200);
        grid.goto(49, 0);
        for _ in 0..history + 49 {
            grid.put_str(&text);
            grid.carriage_return();
            grid.line_feed();
        }
        assert_eq!(grid.scrollback_len(), history);
        grid
    }

    /// One frame's work under the lock and after it, as `render_view` does
    /// it: the capture at `offset` (the grid's part of
    /// [`Snapshot::capture`]) and the frame built from it.
    fn capture_and_frame(snapshot: &mut Snapshot, grid: &Grid, offset: usize) -> usize {
        let damage = if offset == 0 {
            snapshot.scrolled = None;
            snapshot.capture_grid(grid)
        } else {
            snapshot.capture_history(grid, offset)
        };
        let rows = snapshot.rows();
        let frame = snapshot.frame(&rows, true);
        std::hint::black_box(&frame);
        usize::from(damage.is_none()) + frame.rows.len()
    }

    /// The best of `reps` full captures (and frames) at `offset`, each a
    /// view that moved: the generation is forgotten at offset 0, and the
    /// view alternates between `offset` and the line below it above it.
    fn best_full_capture(grid: &Grid, offset: usize, reps: usize) -> Duration {
        let mut snapshot = Snapshot::default();
        let mut best = Duration::MAX;
        for i in 0..reps {
            snapshot.generation = None;
            let at = if offset == 0 { 0 } else { offset + i % 2 };
            let start = Instant::now();
            std::hint::black_box(capture_and_frame(&mut snapshot, grid, at));
            best = best.min(start.elapsed());
        }
        best
    }

    /// #31 smoothness proxy for "scrolling 100k lines of history is
    /// smooth", headless: on a 50x200 grid with 100 000 lines of history,
    /// stepping the view 3 lines per frame over the whole history keeps
    /// the 99th percentile of capture + frame under 1 ms, and the cost
    /// does not grow with the history: a capture at offset 50 000 costs
    /// within 1.10x of one at offset 0, and one at offset 5 000 within
    /// 1.05x between 10 000 and 100 000 lines of history (best of 400
    /// each; three attempts, as the other release timing bars).
    #[test]
    #[cfg_attr(debug_assertions, ignore = "timing bar; run in release")]
    fn scrolling_through_a_long_history_is_smooth() {
        const ATTEMPTS: usize = 3;
        let big = long_history(100_000);
        let small = long_history(10_000);
        let mut failures = Vec::new();
        for attempt in 1..=ATTEMPTS {
            let mut snapshot = Snapshot::default();
            let mut times = Vec::with_capacity(100_000 / 3 + 1);
            for offset in (0..=100_000).step_by(3) {
                let start = Instant::now();
                std::hint::black_box(capture_and_frame(&mut snapshot, &big, offset));
                times.push(start.elapsed());
            }
            times.sort_unstable();
            let p99 = times[times.len() * 99 / 100];
            let live = best_full_capture(&big, 0, 400);
            let deep = best_full_capture(&big, 50_000, 400);
            let short = best_full_capture(&small, 5_000, 400);
            let long = best_full_capture(&big, 5_000, 400);
            let ratio = |a: Duration, b: Duration| a.as_secs_f64() / b.as_secs_f64();
            eprintln!(
                "attempt {attempt}: {} frames, p50 {:?}, p99 {p99:?}, max {:?}; full capture at 0 \
                 {live:?}, at 50 000 {deep:?} ({:.3}x); at 5 000 with 10k history {short:?}, \
                 100k {long:?} ({:.3}x)",
                times.len(),
                times[times.len() / 2],
                times.last().expect("frames"),
                ratio(deep, live),
                ratio(long, short),
            );
            let mut miss = Vec::new();
            if p99 >= Duration::from_millis(1) {
                miss.push(format!("p99 {p99:?} >= 1 ms"));
            }
            if ratio(deep, live) > 1.10 {
                miss.push(format!(
                    "offset 50 000 costs {:.3}x offset 0",
                    ratio(deep, live)
                ));
            }
            if ratio(long, short) > 1.05 {
                miss.push(format!("100k history costs {:.3}x 10k", ratio(long, short)));
            }
            if miss.is_empty() {
                return;
            }
            failures.push(format!("attempt {attempt}: {}", miss.join(", ")));
        }
        panic!("{failures:#?}");
    }

    /// The history count, and so a scrolled view, does not depend on how
    /// the output was split: the same bytes fed whole and byte by byte
    /// push the same lines and show the same rows at the same offset.
    #[test]
    fn a_scrolled_view_does_not_depend_on_chunking() {
        let mut bytes = Vec::new();
        for i in 0..120 {
            bytes.extend(format!("L{i} \u{4e2d}e\u{301}\x1b[41mx\x1b[0m\r\n").as_bytes());
            if i % 17 == 0 {
                bytes.extend(b"\x1b[2;3r\n\n\x1b[r\x1bM\x1b[S");
            }
        }
        let mut whole = Terminal::new(5, 12);
        whole.advance(&bytes);
        let mut split = Terminal::new(5, 12);
        for byte in &bytes {
            split.advance(std::slice::from_ref(byte));
        }
        assert_eq!(
            whole.grid().scrollback_pushed(),
            split.grid().scrollback_pushed()
        );
        assert_eq!(whole.grid().scrollback_len(), split.grid().scrollback_len());
        for offset in [1, 4, 30, 100] {
            let (mut a, mut b) = (Snapshot::default(), Snapshot::default());
            a.capture(&whole, offset);
            b.capture(&split, offset);
            assert_eq!(a.rows(), b.rows(), "offset {offset}");
            assert_eq!(a.clusters, b.clusters, "offset {offset}");
        }
    }

    /// A flood of output under a view scrolled back (10 000 reads of 4 KiB
    /// of `yes` on 50x200, the view anchored as a session anchors it): no
    /// capture is slow (99th percentile under 1 ms, as for scrolling) and
    /// the snapshot's memory does not grow. Release only.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "timing bar; run in release")]
    fn a_flood_under_a_scrolled_view_keeps_captures_short() {
        let mut term = full_terminal(50, 200);
        term.advance(&b"history\r\n".repeat(5_000));
        let chunk: Vec<u8> = b"y\n".repeat(2048);
        let mut snapshot = Snapshot::default();
        snapshot.capture(&term, 1_000);
        let top = snapshot.scrolled.expect("scrolled").top;
        let capacity = (snapshot.cells.capacity(), snapshot.clusters.capacity());
        let mut times = Vec::with_capacity(10_000);
        for _ in 0..10_000 {
            term.advance(&chunk);
            let offset = anchored(&term, top).min(term.grid().scrollback_len());
            let start = Instant::now();
            std::hint::black_box(snapshot.capture(&term, offset));
            times.push(start.elapsed());
        }
        times.sort_unstable();
        let p99 = times[times.len() * 99 / 100];
        eprintln!(
            "flood under a scrolled view: p50 {:?}, p99 {p99:?}, max {:?}",
            times[times.len() / 2],
            times.last().expect("captures")
        );
        assert!(p99 < Duration::from_millis(1), "p99 {p99:?}");
        assert_eq!(
            (snapshot.cells.capacity(), snapshot.clusters.capacity()),
            capacity,
            "snapshot grew"
        );
    }
}
