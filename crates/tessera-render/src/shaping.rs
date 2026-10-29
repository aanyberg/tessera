//! Cell metrics and shaping of grid lines into positioned glyphs.
//!
//! A [`Shaper`] turns one row of [`tessera_grid::Cell`]s into
//! [`ShapedGlyph`]s. Cells are grouped into runs that share a face, a style
//! (bold / italic) and a width class; each run is shaped with `harfrust`
//! (through `cosmic-text`), so ligatures and contextual alternates apply
//! within a run and never across a style change or a space. Glyphs are
//! anchored to their cell rather than to a running pen position, so a
//! font whose advance is not a whole number of pixels never drifts off the
//! grid.
//!
//! A cell whose grapheme cluster has several scalars (a combining mark, an
//! emoji ZWJ sequence, a skin-tone modifier, VS16, a flag) is shaped from
//! the whole cluster text in [`tessera_grid::Line::graphemes`], so the
//! sequence is drawn as the font composes it, centred in the cell (or the
//! two cells of a wide cluster).

use tessera_grid::{Cell, GraphemeTable, Line, Style};
use unicode_width::UnicodeWidthChar;

use crate::atlas::GlyphKey;
use crate::boxdraw;
use crate::error::RenderError;
use crate::font::{
    FaceId, FontConfig, FontStyle, Fonts, Inner, MAX_SCALE, RawGlyph, ResolvedFace,
    sanitize_font_size,
};
use crate::size::sanitize_scale;

/// The size of one grid cell in device pixels.
///
/// Width and height are whole pixels so cells tile without seams; the
/// baseline is measured down from the cell's top edge and is also a whole
/// number of pixels (kept as `f32` for the shader).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    /// Cell width: the primary face's advance of `0` (else `M`), rounded.
    pub width_px: u32,
    /// Cell height: ascent + descent + line gap, rounded.
    pub height_px: u32,
    /// Distance from the cell top to the baseline, centring the face's
    /// ascent + descent in the cell.
    pub baseline_px: f32,
}

impl CellMetrics {
    /// Metrics for a face with the given (pixel) measurements.
    fn from_face(ascent: f32, descent: f32, leading: f32, advance: f32) -> Self {
        let width_px = round_px(advance).max(1);
        let content = ascent + descent;
        let height_px = round_px(content + leading.max(0.0)).max(1);
        let slack = f32_from(height_px) - content;
        let baseline_px = (ascent + slack / 2.0)
            .round()
            .clamp(0.0, f32_from(height_px));
        Self {
            width_px,
            height_px,
            baseline_px,
        }
    }
}

/// One glyph placed relative to its cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapedGlyph {
    /// Index into the shaped `cells` slice of the first cell this glyph
    /// covers.
    pub cell_index: usize,
    /// Number of cells the glyph covers: 1, or more for a ligature (its
    /// own characters plus the spacer cells assigned to it) or for a
    /// shaper cluster holding several characters (a base and a combining
    /// mark in their own cells).
    ///
    /// This is the glyph's logical extent, for cursor and selection, not
    /// its ink bounds: italic overhang, Cascadia's `...` and wide
    /// characters draw beyond it, so damage tracking must pad the range or
    /// use the atlas bitmap's bounds. Cells holding only characters the
    /// shaper hides (U+200B, U+00AD, ...) are never owned on their own:
    /// they are covered when they fall inside a ligature's range or merge
    /// into a base's grapheme cluster (U+FE0F, U+200D, U+17B4 under
    /// `MonotoneGraphemes`), and otherwise belong to no glyph, as do blank
    /// cells and spacers with no drawing neighbour.
    ///
    /// Several glyphs drawing one cell range (a base and its marks, or a
    /// ligature drawn in pieces) are consecutive in the output and repeat
    /// the same `cell_index` and `cell_span`.
    pub cell_span: usize,
    /// Glyph index in [`Self::face_id`]; for [`FaceId::BUILTIN`] this is the
    /// code point.
    pub glyph_id: u16,
    /// The face that draws the glyph.
    pub face_id: FaceId,
    /// Horizontal distance in device pixels from the left edge of cell
    /// `cell_index` to the glyph origin, a multiple of 1/3 px (see
    /// [`Self::split_x_offset`]).
    pub x_offset: f32,
    /// Vertical shift of the origin from the baseline in whole device
    /// pixels, positive up (from the font's mark positioning).
    pub y_offset: f32,
    /// `true` when the glyph's face has colour tables (`sbix`, `COLR` or
    /// `CBDT`).
    ///
    /// Only a hint: it pins the subpixel phase to 0 and isolates the
    /// glyph in its own run. A glyph of a colour face that has no colour
    /// data of its own rasterises as coverage, so the texture to sample
    /// and whether to tint are decided by [`crate::AtlasSlot::layer`],
    /// never by this flag.
    pub is_color: bool,
    /// Bold was requested but the face is not bold; the rasteriser
    /// emboldens the outline.
    pub synthetic_bold: bool,
    /// Italic was requested but the face is upright; the rasteriser skews
    /// the outline by 14°.
    pub synthetic_italic: bool,
}

impl ShapedGlyph {
    /// Splits [`Self::x_offset`] into whole pixels (rounded down) and
    /// thirds of a pixel (`0..=2`).
    ///
    /// The renderer places the atlas bitmap at
    /// `cell_x + whole + AtlasSlot::offset[0]`; the thirds are baked into
    /// the bitmap through [`GlyphKey::subpixel_x`].
    #[must_use]
    pub fn split_x_offset(&self) -> (i32, u8) {
        let thirds = round_i32(self.x_offset * 3.0);
        let sub = u8::try_from(thirds.rem_euclid(3)).unwrap_or(0);
        (thirds.div_euclid(3), sub)
    }
}

/// Shapes grid lines for one font configuration and size.
#[derive(Debug)]
pub struct Shaper {
    fonts: Fonts,
    size_pt: f32,
    scale: f32,
    size_px: f32,
    metrics: CellMetrics,
}

impl Shaper {
    /// Loads the system fonts, resolves `config` and computes cell metrics
    /// at `config.size_pt * config.scale`.
    ///
    /// Fails only when no usable monospaced font is installed.
    pub fn new(config: &FontConfig) -> Result<Self, RenderError> {
        let fonts = Fonts::new(config)?;
        Ok(Self::with_fonts(fonts, config.size_pt, config.scale))
    }

    /// A shaper on already-loaded fonts, so several surfaces can share one
    /// font database.
    ///
    /// The shaper keeps its own invalidation counter, so `fonts` itself
    /// never sees its [`Self::set_font_size`]: build each window's atlas
    /// with [`crate::GlyphAtlas::for_shaper`] (or from [`Self::fonts`],
    /// never from `fonts`). A [`Self::set_font_size`] in one window then
    /// clears that window's atlas and leaves the other windows' alone.
    ///
    /// `size_pt` is clamped like [`crate::sanitize_font_size`]; `scale` is
    /// sanitised like [`crate::sanitize_scale`] and capped at
    /// [`MAX_SCALE`], so the pixel size is bounded whatever the frontend
    /// passes.
    #[must_use]
    pub fn with_fonts(fonts: Fonts, size_pt: f32, scale: f32) -> Self {
        let size_pt = sanitize_font_size(size_pt);
        let scale = clamp_scale(scale);
        let pixels = em_px(size_pt, scale);
        // Own invalidation counter: `set_font_size` here must not clear
        // the atlases of other shapers (windows) sharing these fonts.
        let fonts = fonts.with_own_epoch();
        let metrics = compute_metrics(&fonts, pixels);
        Self {
            fonts,
            size_pt,
            scale,
            size_px: pixels,
            metrics,
        }
    }

    /// The fonts this shaper draws with, carrying its invalidation
    /// counter; hand them to [`crate::GlyphAtlas::new`] (or use
    /// [`crate::GlyphAtlas::for_shaper`]).
    #[must_use]
    pub fn fonts(&self) -> &Fonts {
        &self.fonts
    }

    /// Font size in points (after clamping).
    #[must_use]
    pub fn size_pt(&self) -> f32 {
        self.size_pt
    }

    /// Backing scale factor (after sanitising and capping at
    /// [`MAX_SCALE`]).
    #[must_use]
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Font size in device pixels: `size_pt * scale`.
    #[must_use]
    pub fn size_px(&self) -> f32 {
        self.size_px
    }

    /// Changes the font size or backing scale.
    ///
    /// Recomputes [`Self::cell_metrics`] and invalidates the glyph atlas:
    /// every [`crate::GlyphAtlas`] built from [`Self::fonts`] clears itself
    /// at its next [`crate::GlyphAtlas::begin_frame`]; atlases of other
    /// shapers sharing the font database are not affected. `size_pt` is
    /// clamped like [`crate::sanitize_font_size`], `scale` like
    /// [`crate::sanitize_scale`] and capped at [`MAX_SCALE`]. A call that
    /// leaves the pixel size unchanged keeps the atlas.
    pub fn set_font_size(&mut self, size_pt: f32, scale: f32) {
        let size_pt = sanitize_font_size(size_pt);
        let scale = clamp_scale(scale);
        let pixels = em_px(size_pt, scale);
        self.size_pt = size_pt;
        self.scale = scale;
        if pixels.to_bits() == self.size_px.to_bits() {
            return;
        }
        self.size_px = pixels;
        self.metrics = compute_metrics(&self.fonts, pixels);
        self.fonts.invalidate();
    }

    /// The cell size at the current font size.
    #[must_use]
    pub fn cell_metrics(&self) -> CellMetrics {
        self.metrics
    }

    /// Shapes one line of cells, each cell from its base character alone.
    ///
    /// Blank and control cells produce no glyph, nor do glyphs that draw
    /// nothing. Box-drawing and block characters (U+2500–U+259F) come from
    /// [`FaceId::BUILTIN`] and fill their cell exactly. Every other cell is
    /// drawn by the first face in the fallback chain that covers it.
    ///
    /// The glyphs partition the line: their cell ranges are in order and
    /// never overlap, and every cell whose character draws is covered
    /// exactly once, including the spacer cells of a ligature (debug
    /// builds assert this). The ranges are logical extents, not ink
    /// bounds. Cells holding only characters the shaper hides are never
    /// owned on their own: they are covered when they fall inside a
    /// ligature's range or merge into a base's grapheme cluster, and
    /// otherwise belong to no glyph, as do blank cells and spacers with no
    /// drawing neighbour (see [`ShapedGlyph::cell_span`]).
    ///
    /// A wide character (CJK, emoji) is centred across its own cell and
    /// the spacer cell after it; the spacer itself is skipped and owned by
    /// no glyph, so the wide glyph's `cell_span` stays 1. A cell is wide
    /// when the grid marks it [`Cell::is_wide`], or when it is not a
    /// cluster and its character is two columns wide on its own (cells
    /// built without the grid's markers).
    ///
    /// The cells carry only a cluster's base character; use
    /// [`Self::shape_grid_line`] or [`Self::shape_line_into`] with the
    /// line's [`GraphemeTable`] to draw whole clusters.
    pub fn shape_line(&mut self, cells: &[Cell]) -> Vec<ShapedGlyph> {
        let mut out = Vec::with_capacity(cells.len());
        self.shape_line_into(cells, &GraphemeTable::new(), &mut out);
        out
    }

    /// Shapes a grid line with its grapheme clusters: [`Self::shape_line`]
    /// on `line.cells`, with each [`tessera_grid::CellFlags::CLUSTER`] cell
    /// shaped from its whole text ([`Line::cluster`]).
    pub fn shape_grid_line(&mut self, line: &Line) -> Vec<ShapedGlyph> {
        let mut out = Vec::with_capacity(line.cells.len());
        self.shape_line_into(&line.cells, &line.graphemes, &mut out);
        out
    }

    /// Shapes `cells` into `out`, which is cleared first and keeps its
    /// allocation (the renderer reuses one vector per row). `clusters` is
    /// the line's cluster table: a [`tessera_grid::CellFlags::CLUSTER`]
    /// cell with an entry there is shaped from the whole cluster, in the
    /// face that covers all of its scalars (the base's face when it does;
    /// VS16 asks for emoji presentation and VS15 for text). Everything
    /// else is as [`Self::shape_line`].
    pub fn shape_line_into(
        &mut self,
        cells: &[Cell],
        clusters: &GraphemeTable,
        out: &mut Vec<ShapedGlyph>,
    ) {
        out.clear();
        let mut inner = self.fonts.lock();
        let ctx = RunContext {
            size_px: self.size_px,
            cell_width: f32_from(self.metrics.width_px),
        };
        // Cells the output must cover; only filled in debug builds.
        let checked = if cfg!(debug_assertions) {
            cells.len()
        } else {
            0
        };
        let mut required = vec![false; checked];
        let mut i = 0;
        while i < cells.len() {
            let cell = &cells[i];
            if cell.is_spacer() || is_blank(cell.ch) {
                i += 1;
                continue;
            }
            let text = cluster_text(cell, clusters, i);
            if text.is_none() && boxdraw::is_builtin(cell.ch) {
                out.push(ShapedGlyph {
                    cell_index: i,
                    cell_span: 1,
                    glyph_id: u16::try_from(u32::from(cell.ch)).unwrap_or(0),
                    face_id: FaceId::BUILTIN,
                    x_offset: 0.0,
                    y_offset: 0.0,
                    is_color: false,
                    synthetic_bold: false,
                    synthetic_italic: false,
                });
                if let Some(r) = required.get_mut(i) {
                    *r = true;
                }
                i += 1;
                continue;
            }
            let style = cell_style(cell);
            let resolved = resolve_cell(&mut inner, cell, text, style);
            let wide = cell_is_wide(cell);
            let isolated = wide || inner.is_color(resolved.face);
            let start = i;
            i += 1;
            while !isolated && i < cells.len() {
                let next = &cells[i];
                let next_text = cluster_text(next, clusters, i);
                if next.is_spacer()
                    || is_blank(next.ch)
                    || (next_text.is_none() && boxdraw::is_builtin(next.ch))
                    || cell_is_wide(next)
                    || cell_style(next) != style
                    || resolve_cell(&mut inner, next, next_text, style) != resolved
                {
                    break;
                }
                i += 1;
            }
            let run = Run {
                cells: &cells[start..i],
                clusters,
                first: start,
                resolved,
                wide,
            };
            shape_run(&mut inner, &ctx, &run, out, &mut required);
        }
        debug_assert_eq!(
            check_partition(out, &required),
            Ok(()),
            "shape_line broke the cell partition"
        );
    }

    /// The atlas key for `glyph` at the current size.
    ///
    /// Colour and builtin glyphs ignore the subpixel phase (bitmaps cannot
    /// be shifted by a third of a pixel), so their key always has
    /// `subpixel_x == 0`.
    #[must_use]
    pub fn glyph_key(&self, glyph: &ShapedGlyph) -> GlyphKey {
        let (_, sub) = glyph.split_x_offset();
        let builtin = glyph.face_id.is_builtin();
        GlyphKey {
            face_id: glyph.face_id,
            glyph_id: glyph.glyph_id,
            size_px_64: px_to_64ths(self.size_px),
            subpixel_x: if builtin || glyph.is_color { 0 } else { sub },
            synthetic_bold: glyph.synthetic_bold && !builtin,
            synthetic_italic: glyph.synthetic_italic && !builtin,
            cell_size: [self.metrics.width_px, self.metrics.height_px],
            // The baseline is whole and within the cell, so this is exact.
            baseline_px: round_px(self.metrics.baseline_px),
        }
    }
}

/// [`sanitize_scale`], capped at [`MAX_SCALE`].
fn clamp_scale(scale: f32) -> f32 {
    sanitize_scale(scale).min(MAX_SCALE)
}

/// The em in device pixels, never below 1 px (swash treats a zero size as
/// "unscaled"). The scale and size floors already guarantee this; the
/// clamp keeps it true if either floor changes.
fn em_px(size_pt: f32, scale: f32) -> f32 {
    (size_pt * scale).max(1.0)
}

fn compute_metrics(fonts: &Fonts, size_px: f32) -> CellMetrics {
    let face = fonts.primary_face();
    let measured = fonts.lock().metrics(face, size_px);
    if let Some(m) = measured {
        CellMetrics::from_face(m.ascent, m.descent, m.leading, m.cell_advance)
    } else {
        tracing::warn!(
            target: "tessera_render",
            size_px,
            "primary face has no metrics; approximating the cell"
        );
        CellMetrics::from_face(size_px * 0.8, size_px * 0.2, 0.0, size_px * 0.6)
    }
}

/// Per-line constants a run needs.
struct RunContext {
    size_px: f32,
    cell_width: f32,
}

/// A glyph positioned relative to its cluster's first cell.
#[derive(Debug, Clone, Copy)]
struct Placed {
    glyph_id: u16,
    x: f32,
    y: f32,
    /// Horizontal ink extent relative to the glyph origin; `None` for a
    /// glyph that draws nothing.
    ink: Option<(f32, f32)>,
}

/// What a shaper cluster contributes to the cell partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClusterKind {
    /// At least one glyph draws: the cluster owns its cells.
    /// `ligature` is set when the cluster spans several cells or its
    /// glyph is a substitution, so it may absorb neighbouring spacers.
    Drawing { ligature: bool },
    /// Every glyph is empty although the characters draw on their own: a
    /// ligature spacer, assigned as a unit to an adjacent drawing cluster.
    Spacer,
    /// Every glyph is empty and so are the characters' own glyphs
    /// (no-break space, zero-width characters): owns nothing.
    Blank,
    /// Every glyph is empty and every character is one the shaper hides
    /// (harfrust's default-ignorable set: soft hyphen, ZWSP, BOM,
    /// variation selectors, ...), whatever ink its nominal glyph has: owns
    /// nothing, takes no spacers, and is skipped over when a spacer run
    /// looks for its drawing neighbours, because the shaper skips it too
    /// (`-`, ZWSP, `>` still forms an arrow).
    Ignorable,
}

/// One shaper cluster: the cells `cell..cell + span` of the run and the
/// glyphs `glyphs` of `placed`.
#[derive(Debug, Clone)]
struct Cluster {
    cell: usize,
    span: usize,
    glyphs: std::ops::Range<usize>,
    kind: ClusterKind,
}

impl Cluster {
    fn end(&self) -> usize {
        self.cell + self.span
    }
}

/// One run of cells drawn by one face.
struct Run<'a> {
    /// The run's cells.
    cells: &'a [Cell],
    /// The line's cluster table (indexed by line column).
    clusters: &'a GraphemeTable,
    /// Index of `cells[0]` in the line.
    first: usize,
    resolved: ResolvedFace,
    wide: bool,
}

/// A shaper cluster before classification: the cells `cell..cell + span`
/// of the run and the glyphs `glyphs` of the shaper output.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawCluster {
    cell: usize,
    span: usize,
    glyphs: std::ops::Range<usize>,
}

/// Groups the shaper's glyphs by cluster and maps each cluster to the run
/// cells it covers. `cell_of` maps a cluster's byte offset in the run text
/// to its cell; a cluster spans up to the next cluster's cell (at least
/// one), the last one to the end of the run (`cells`).
fn group_clusters(
    raw: &[RawGlyph],
    cell_of: impl Fn(usize) -> usize,
    cells: usize,
) -> Vec<RawCluster> {
    let mut out = Vec::new();
    let mut g = 0;
    while g < raw.len() {
        let cluster = raw[g].cluster;
        let end = g + raw[g..].iter().take_while(|r| r.cluster == cluster).count();
        let cell = cell_of(cluster);
        let next_cell = raw.get(end).map_or(cells, |r| cell_of(r.cluster));
        out.push(RawCluster {
            cell,
            span: next_cell.saturating_sub(cell).max(1),
            glyphs: g..end,
        });
        g = end;
    }
    out
}

/// Shapes `run` and appends the glyphs to `out`. In debug builds it also
/// marks in `required` (indexed like the line) the cells the output must
/// cover.
fn shape_run(
    inner: &mut Inner,
    ctx: &RunContext,
    run: &Run<'_>,
    out: &mut Vec<ShapedGlyph>,
    required: &mut [bool],
) {
    let Run {
        cells,
        clusters: table,
        first,
        resolved,
        wide,
    } = *run;
    let mut text = String::with_capacity(cells.len());
    // Byte offset of each cell's text, plus the end.
    let mut starts: Vec<usize> = Vec::with_capacity(cells.len() + 1);
    for (k, c) in cells.iter().enumerate() {
        starts.push(text.len());
        match cluster_text(c, table, first + k) {
            Some(cluster) => text.push_str(cluster),
            None => text.push(c.ch),
        }
    }
    starts.push(text.len());
    let cell_of = |byte: usize| {
        starts[..cells.len()]
            .partition_point(|&s| s <= byte)
            .saturating_sub(1)
    };
    let raw = inner.shape(resolved.face, &text, ctx.size_px);
    let is_color = inner.is_color(resolved.face);
    let face = resolved.face;

    // Anchor each cluster to its cell, centred in the cells it spans (two
    // per cell for wide characters).
    let mut placed = Vec::with_capacity(raw.len());
    let mut clusters: Vec<Cluster> = Vec::new();
    for group in group_clusters(&raw, cell_of, cells.len()) {
        let RawCluster { cell, span, glyphs } = group;
        let advance: f32 = raw[glyphs.clone()].iter().map(|r| r.x_advance).sum();
        let columns = if wide { 2 * span } else { span };
        let centre = (ctx.cell_width * f32_from_usize(columns) - advance) / 2.0;
        let mut pen = 0.0;
        let from = placed.len();
        for r in &raw[glyphs] {
            placed.push(Placed {
                glyph_id: r.glyph_id,
                x: centre + pen + r.x_offset,
                y: r.y_offset,
                ink: inner.ink_extent(face, r.glyph_id, ctx.size_px),
            });
            pen += r.x_advance;
        }
        let end = (cell + span).min(cells.len());
        let own_text = &text[starts[cell]..starts[end]];
        let kind = classify(inner, face, ctx.size_px, own_text, span, &placed[from..]);
        clusters.push(Cluster {
            cell,
            span,
            glyphs: from..placed.len(),
            kind,
        });
    }

    let owners = partition(&clusters, &placed, ctx.cell_width);
    if cfg!(debug_assertions) {
        for (c, owner) in clusters.iter().zip(&owners) {
            if owner.is_some() {
                for r in &mut required[first + c.cell..first + c.end().min(cells.len())] {
                    *r = true;
                }
            }
        }
    }

    // Each drawing cluster owns its own cells plus the spacer clusters
    // assigned to it; gather those ranges in one pass.
    let mut ranges: Vec<(usize, usize)> = clusters.iter().map(|c| (c.cell, c.end())).collect();
    for (s, owner) in clusters.iter().zip(&owners) {
        if let Some(o) = *owner {
            ranges[o].0 = ranges[o].0.min(s.cell);
            ranges[o].1 = ranges[o].1.max(s.end());
        }
    }

    // Emit each drawing cluster's glyphs on the cells it owns.
    for (d, c) in clusters.iter().enumerate() {
        if !matches!(c.kind, ClusterKind::Drawing { .. }) {
            continue;
        }
        let (start, end) = ranges[d];
        let shift = f32_from_usize(c.cell - start) * ctx.cell_width;
        for p in &placed[c.glyphs.clone()] {
            if p.ink.is_none() {
                continue;
            }
            out.push(ShapedGlyph {
                cell_index: first + start,
                cell_span: end - start,
                glyph_id: p.glyph_id,
                face_id: face,
                x_offset: quantize_third(p.x + shift),
                y_offset: p.y.round(),
                is_color,
                synthetic_bold: resolved.synthetic_bold,
                synthetic_italic: resolved.synthetic_italic,
            });
        }
    }
}

/// Classifies a cluster from its glyphs, `own` (the text of the `span`
/// cells it covers, every scalar of their grapheme clusters).
fn classify(
    inner: &mut Inner,
    face: FaceId,
    size_px: f32,
    own: &str,
    span: usize,
    glyphs: &[Placed],
) -> ClusterKind {
    if glyphs.iter().any(|p| p.ink.is_some()) {
        let substituted = glyphs.iter().filter(|p| p.ink.is_some()).any(|p| {
            own.chars()
                .all(|ch| inner.nominal_glyph(face, ch) != p.glyph_id)
        });
        ClusterKind::Drawing {
            ligature: span > 1 || substituted,
        }
    } else if own.chars().all(is_default_ignorable) {
        ClusterKind::Ignorable
    } else if own.chars().any(|ch| {
        let nominal = inner.nominal_glyph(face, ch);
        nominal != 0 && inner.ink_extent(face, nominal, size_px).is_some()
    }) {
        ClusterKind::Spacer
    } else {
        ClusterKind::Blank
    }
}

/// The drawing cluster that owns each cluster's cells (`None` for a
/// blank or ignorable cluster, or a spacer with no drawing neighbour).
///
/// A drawing cluster owns itself, so a merged shaper cluster (a spacer
/// and its ligature glyph in one cluster, as Monaspace emits) is never
/// split or absorbed. Programming ligatures built from contextual
/// alternates otherwise replace all but one character with spacer
/// clusters: `JetBrains Mono` and `Fira Code` put the spacers before the
/// drawing glyph, `Cascadia Code` after it. Each maximal run of spacer
/// clusters is split between the drawing clusters on either side of it,
/// the left part going to the previous one and the rest to the next one,
/// so every owner's cells stay contiguous. Ignorable clusters inside or
/// around a run are part of it for the split but are never assigned,
/// and the neighbours are the nearest non-ignorable clusters. Only a
/// ligature cluster (one that spans several cells or whose glyph is a
/// substitution) takes spacers while the other side is a ligature; a
/// plain letter never absorbs a neighbour's spacer. Between two ligature candidates the
/// split maximises the ink each side lays over the cells it receives;
/// a tie goes to the side whose ink covers more of the whole run of
/// spacers, and if neither side's ink reaches it, to the next cluster
/// (spacers before the drawing glyph are the common convention).
fn partition(clusters: &[Cluster], placed: &[Placed], cell_width: f32) -> Vec<Option<usize>> {
    let mut owners: Vec<Option<usize>> = clusters
        .iter()
        .enumerate()
        .map(|(i, c)| matches!(c.kind, ClusterKind::Drawing { .. }).then_some(i))
        .collect();
    let drawing = |i: usize| match clusters.get(i).map(|c| c.kind) {
        Some(ClusterKind::Drawing { ligature }) => Some(ligature),
        _ => None,
    };
    let mut a = 0;
    while a < clusters.len() {
        if clusters[a].kind != ClusterKind::Spacer {
            a += 1;
            continue;
        }
        let b = a + clusters[a..]
            .iter()
            .take_while(|c| matches!(c.kind, ClusterKind::Spacer | ClusterKind::Ignorable))
            .count();
        let prev = clusters[..a]
            .iter()
            .rposition(|c| c.kind != ClusterKind::Ignorable)
            .and_then(|p| drawing(p).map(|l| (p, l)));
        let next = drawing(b).map(|l| (b, l));
        let split = match (prev, next) {
            (None, None) => None,
            (Some(_), None) | (Some((_, true)), Some((_, false))) => Some(b),
            (None, Some(_)) | (Some((_, false)), Some((_, true))) => Some(a),
            (Some((p, _)), Some((n, _))) => {
                let over_prev: Vec<f32> = (a..b)
                    .map(|s| ink_over(&clusters[p], &clusters[s], placed, cell_width))
                    .collect();
                let over_next: Vec<f32> = (a..b)
                    .map(|s| ink_over(&clusters[n], &clusters[s], placed, cell_width))
                    .collect();
                // suffix[i]: ink of next over spacers a + i..b. Summed from
                // the right rather than by subtraction, so equal scores
                // (ties) stay exactly equal.
                let mut suffix = vec![0.0_f32; over_next.len() + 1];
                for i in (0..over_next.len()).rev() {
                    suffix[i] = over_next[i] + suffix[i + 1];
                }
                let lean_prev = over_prev.iter().sum::<f32>() > suffix[0];
                // score(k): spacers a..k go to prev, k..b to next, with a
                // running prefix so the scan is linear in the run length.
                let mut prefix = 0.0_f32;
                let mut best = (f32::NEG_INFINITY, a);
                for k in a..=b {
                    if k > a {
                        prefix += over_prev[k - a - 1];
                    }
                    let score = prefix + suffix[k - a];
                    if score > best.0 || (lean_prev && score >= best.0) {
                        best = (score, k);
                    }
                }
                Some(best.1)
            }
        };
        if let Some(k) = split {
            for (s, owner) in owners.iter_mut().enumerate().take(b).skip(a) {
                if clusters[s].kind != ClusterKind::Spacer {
                    continue;
                }
                *owner = if s < k {
                    prev.map(|(p, _)| p)
                } else {
                    next.map(|(n, _)| n)
                };
            }
        }
        a = b;
    }
    owners
}

/// How many pixels of `drawer`'s ink lie over the cells of `spacer`.
fn ink_over(drawer: &Cluster, spacer: &Cluster, placed: &[Placed], cell_width: f32) -> f32 {
    let origin = f32_from_usize(drawer.cell) * cell_width;
    let lo = f32_from_usize(spacer.cell) * cell_width;
    let hi = f32_from_usize(spacer.end()) * cell_width;
    placed[drawer.glyphs.clone()]
        .iter()
        .filter_map(|p| p.ink.map(|(l, r)| (origin + p.x + l, origin + p.x + r)))
        .map(|(l, r)| (r.min(hi) - l.max(lo)).max(0.0))
        .sum()
}

/// Checks that `glyphs` partition the line's cells: glyphs that share a
/// cell range (a base and its marks, a multi-glyph ligature) are
/// consecutive and report the same `cell_index` and `cell_span`; distinct
/// ranges are non-empty, in order, never overlap and stay within
/// `required.len()` cells; every cell marked in `required` is covered.
///
/// `required` is derived from the same ownership computation that builds
/// the glyphs, so the coverage half only confirms that emission matches
/// ownership; what this checks independently is ordering, overlap and
/// bounds.
fn check_partition(glyphs: &[ShapedGlyph], required: &[bool]) -> Result<(), String> {
    let mut covered = vec![false; required.len()];
    let mut last: Option<(usize, usize)> = None;
    for g in glyphs {
        let range = (g.cell_index, g.cell_span);
        if last == Some(range) {
            continue;
        }
        if g.cell_span == 0 {
            return Err(format!("empty span: {g:?}"));
        }
        let end = g.cell_index + g.cell_span;
        if end > required.len() {
            return Err(format!("span past the line end: {g:?}"));
        }
        if let Some((start, span)) = last
            && g.cell_index < start + span
        {
            return Err(format!("out of order or overlapping: {g:?}"));
        }
        for c in &mut covered[g.cell_index..end] {
            *c = true;
        }
        last = Some(range);
    }
    match required.iter().zip(&covered).position(|(&r, &c)| r && !c) {
        Some(cell) => Err(format!("cell {cell} is not covered")),
        None => Ok(()),
    }
}

/// The full cluster text of `cell` at line column `col`, when it is a
/// cluster cell and `clusters` has its entry.
fn cluster_text<'a>(cell: &Cell, clusters: &'a GraphemeTable, col: usize) -> Option<&'a str> {
    if cell.is_cluster() {
        clusters.get(col)
    } else {
        None
    }
}

/// The face drawing `cell`, whose cluster text is `text` (`None` for a
/// plain cell): the base character's face, with the presentation a
/// variation selector asks for, when it covers every visible scalar of
/// the cluster; else the face of the first scalar it misses, when that
/// one covers them all; else the base character's face (the rest draw
/// `.notdef`).
fn resolve_cell(
    inner: &mut Inner,
    cell: &Cell,
    text: Option<&str>,
    style: FontStyle,
) -> ResolvedFace {
    let Some(text) = text else {
        return inner.resolve(cell.ch, style);
    };
    let base = match presentation(text) {
        Some(emoji) => inner.resolve_presented(cell.ch, style, emoji),
        None => inner.resolve(cell.ch, style),
    };
    let Some(missing) = first_uncovered(inner, base.face, text) else {
        return base;
    };
    let other = inner.resolve(missing, style);
    if other != base && first_uncovered(inner, other.face, text).is_none() {
        other
    } else {
        base
    }
}

/// The first scalar of `text` that `face` has no glyph for, ignoring the
/// ones the shaper hides (ZWJ, variation selectors).
fn first_uncovered(inner: &mut Inner, face: FaceId, text: &str) -> Option<char> {
    text.chars()
        .filter(|&ch| !is_default_ignorable(ch))
        .find(|&ch| inner.nominal_glyph(face, ch) == 0)
}

/// The presentation a cluster's variation selector asks for: emoji for
/// VS16 (U+FE0F), text for VS15 (U+FE0E), the base's default otherwise.
fn presentation(text: &str) -> Option<bool> {
    text.chars().find_map(|ch| match ch {
        '\u{FE0F}' => Some(true),
        '\u{FE0E}' => Some(false),
        _ => None,
    })
}

fn cell_style(cell: &Cell) -> FontStyle {
    FontStyle::new(cell.style.has(Style::BOLD), cell.style.has(Style::ITALIC))
}

/// Cells that never produce a glyph.
fn is_blank(ch: char) -> bool {
    ch == ' ' || ch.is_control()
}

/// Rounds to the nearest third of a pixel.
fn quantize_third(x: f32) -> f32 {
    (x * 3.0).round() / 3.0
}

/// Characters the shaper hides (harfrust's default-ignorable set): the
/// Unicode `Default_Ignorable_Code_Point` ranges except U+115F, U+1160,
/// U+180F, U+3164, U+FFA0 and U+1BCA0..1BCA3, which harfrust 0.5 shapes
/// as ordinary glyphs.
const DEFAULT_IGNORABLE: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x034F, 0x034F),
    (0x061C, 0x061C),
    (0x17B4, 0x17B5),
    (0x180B, 0x180E),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x206F),
    (0xFE00, 0xFE0F),
    (0xFEFF, 0xFEFF),
    (0xFFF0, 0xFFF8),
    (0x1_D173, 0x1_D17A),
    (0xE_0000, 0xE_0FFF),
];

/// `true` for characters the shaper hides (harfrust's default-ignorable
/// set).
fn is_default_ignorable(ch: char) -> bool {
    in_table(DEFAULT_IGNORABLE, ch)
}

fn in_table(table: &[(u32, u32)], ch: char) -> bool {
    let c = u32::from(ch);
    let i = table.partition_point(|&(_, hi)| hi < c);
    table.get(i).is_some_and(|&(lo, _)| lo <= c)
}

/// Whether `cell` is drawn across two columns. The grid's marker decides
/// for a cluster: `⌚` + VS15 is one column although `⌚` alone is two.
fn cell_is_wide(cell: &Cell) -> bool {
    cell.is_wide() || (!cell.is_cluster() && is_wide(cell.ch))
}

/// `true` for characters that occupy two terminal columns on their own,
/// per `unicode-width` (the same answer `tessera-grid` places them by).
pub(crate) fn is_wide(ch: char) -> bool {
    ch.width() == Some(2)
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "pixel sizes are clamped positive and far below u32::MAX"
)]
fn round_px(v: f32) -> u32 {
    v.max(0.0).round() as u32
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "glyph offsets are a few cells at most"
)]
fn round_i32(v: f32) -> i32 {
    v.round() as i32
}

fn px_to_64ths(size_px: f32) -> u32 {
    round_px(size_px * 64.0)
}

#[expect(clippy::cast_precision_loss, reason = "cell sizes are far below 2^24")]
fn f32_from(v: u32) -> f32 {
    v as f32
}

#[expect(clippy::cast_precision_loss, reason = "cell counts are far below 2^24")]
fn f32_from_usize(v: usize) -> f32 {
    v as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignorable_table_is_sorted_and_matches_known_characters() {
        for &(lo, hi) in DEFAULT_IGNORABLE {
            assert!(lo <= hi, "{lo:X}..{hi:X}");
        }
        for pair in DEFAULT_IGNORABLE.windows(2) {
            assert!(
                pair[0].1 < pair[1].0,
                "{:X?} overlaps {:X?}",
                pair[0],
                pair[1]
            );
        }
        for ch in [
            '\u{AD}',
            '\u{180E}',
            '\u{17B4}',
            '\u{200B}',
            '\u{200D}',
            '\u{FE0F}',
            '\u{FEFF}',
            '\u{E0001}',
        ] {
            assert!(is_default_ignorable(ch), "U+{:04X}", u32::from(ch));
        }
        // Default_Ignorable in Unicode, but harfrust draws them.
        let drawn = [
            '\u{115F}',
            '\u{1160}',
            '\u{180F}',
            '\u{3164}',
            '\u{FFA0}',
            '\u{1BCA0}',
            '\u{1BCA3}',
        ];
        for ch in ['a', '-', '>', ' ', '\u{A0}', '\u{301}', '\u{2010}']
            .into_iter()
            .chain(drawn)
        {
            assert!(!is_default_ignorable(ch), "U+{:04X}", u32::from(ch));
        }
    }

    #[test]
    fn wide_lookup_follows_unicode_width() {
        for ch in "中\u{1F600}あ한\u{FF21}\u{2B50}\u{1F3FD}".chars() {
            assert!(is_wide(ch), "{ch:?}");
        }
        // East Asian Width N or A: one column, although the old condensed
        // table called some of them wide (#19).
        for ch in "a─é\u{1F0CE}\u{FFFF}\u{1F321}\u{3248}\u{263A}\u{1F1F8}".chars() {
            assert!(!is_wide(ch), "{ch:?}");
        }
    }

    #[test]
    fn a_narrowed_cluster_is_not_drawn_wide() {
        let mut grid = tessera_grid::Grid::new(1, 8);
        // Fixture rows D16 (VS15 narrows a wide emoji) and D11 (VS16
        // widens a narrow one), then a lone CJK cell without markers.
        grid.put_str("\u{231A}\u{FE0E}\u{263A}\u{FE0F}");
        let cells = &grid.line(0).expect("row 0").cells;
        assert!(cells[0].is_cluster() && !cells[0].is_wide(), "D16 cell");
        assert!(!cell_is_wide(&cells[0]), "VS15 cluster is one column");
        assert!(cells[1].is_cluster() && cells[1].is_wide(), "D11 cell");
        assert!(cell_is_wide(&cells[1]), "VS16 cluster is two columns");
        assert!(cell_is_wide(&Cell::new('\u{4E2D}', Style::DEFAULT)));
        assert!(!cell_is_wide(&Cell::new('a', Style::DEFAULT)));
    }

    #[test]
    fn emoji_preference_follows_the_width() {
        use crate::font::prefers_emoji;
        assert!(prefers_emoji('\u{1F600}'));
        assert!(prefers_emoji('\u{231A}'));
        assert!(prefers_emoji('\u{1F3FD}'));
        // Text presentation by default: one column, text fonts first.
        assert!(!prefers_emoji('\u{263A}'));
        assert!(!prefers_emoji('\u{1F321}'));
        assert!(!prefers_emoji('\u{2764}'));
    }

    /// A row as the grid lays it out: wide characters followed by their
    /// spacer cells, and a cluster cell whose base is all that is shaped.
    #[test]
    fn spacer_cells_are_skipped_and_the_partition_holds() {
        let mut grid = tessera_grid::Grid::new(1, 10);
        grid.put_str("a\u{4E2D}\u{6587}e\u{301}\u{1F600}b");
        let cells = &grid.line(0).expect("row 0").cells;
        assert!(cells[2].is_spacer() && cells[4].is_spacer() && cells[7].is_spacer());
        let config = crate::FontConfig {
            family: "Menlo".into(),
            size_pt: 13.0,
            scale: 2.0,
            fallback: Vec::new(),
        };
        let mut shaper = Shaper::new(&config).expect("Menlo ships with macOS");
        let glyphs = shaper.shape_line(cells);
        let mut spans: Vec<(usize, usize)> =
            glyphs.iter().map(|g| (g.cell_index, g.cell_span)).collect();
        spans.dedup();
        assert_eq!(
            spans,
            [(0, 1), (1, 1), (3, 1), (5, 1), (6, 1), (8, 1)],
            "one glyph range per drawn cell, none on a spacer: {glyphs:?}"
        );
        let required: Vec<bool> = cells
            .iter()
            .map(|c| !c.is_spacer() && !is_blank(c.ch))
            .collect();
        assert_eq!(check_partition(&glyphs, &required), Ok(()));
        // Wide glyphs are centred over both columns, so they start right
        // of their cell's left edge, unlike the narrow `a`.
        let w = f32_from(shaper.cell_metrics().width_px);
        let han = glyphs.iter().find(|g| g.cell_index == 1).expect("中");
        assert!(han.x_offset > -w / 2.0 && han.x_offset < w, "{han:?}");
    }

    #[test]
    fn thirds_quantisation() {
        assert!((quantize_third(0.1) - 0.0).abs() < 1e-6);
        assert!((quantize_third(0.2) - 1.0 / 3.0).abs() < 1e-6);
        assert!((quantize_third(-0.5) - (-2.0 / 3.0)).abs() < 1e-6);
        assert!((quantize_third(8.9) - 9.0).abs() < 1e-6);
    }

    #[test]
    fn split_x_offset_floors_and_keeps_thirds() {
        let g = |x| ShapedGlyph {
            cell_index: 0,
            cell_span: 1,
            glyph_id: 1,
            face_id: FaceId::BUILTIN,
            x_offset: x,
            y_offset: 0.0,
            is_color: false,
            synthetic_bold: false,
            synthetic_italic: false,
        };
        assert_eq!(g(0.0).split_x_offset(), (0, 0));
        assert_eq!(g(1.0 / 3.0).split_x_offset(), (0, 1));
        assert_eq!(g(2.0 / 3.0).split_x_offset(), (0, 2));
        assert_eq!(g(8.0 + 2.0 / 3.0).split_x_offset(), (8, 2));
        assert_eq!(g(-1.0 / 3.0).split_x_offset(), (-1, 2));
        assert_eq!(g(-1.0).split_x_offset(), (-1, 0));
    }

    fn glyph_at(cell_index: usize, cell_span: usize) -> ShapedGlyph {
        ShapedGlyph {
            cell_index,
            cell_span,
            glyph_id: 1,
            face_id: FaceId::BUILTIN,
            x_offset: 0.0,
            y_offset: 0.0,
            is_color: false,
            synthetic_bold: false,
            synthetic_italic: false,
        }
    }

    #[test]
    fn partition_check_rejects_overlap_and_gaps() {
        let all = [true; 4];
        // A merged cluster absorbed by an overhanging letter (the old
        // Monaspace italic bug).
        let overlap = [glyph_at(0, 4), glyph_at(1, 3)];
        assert!(check_partition(&overlap, &all).is_err());
        // A dropped last spacer (the old Cascadia `...` bug).
        assert!(check_partition(&[glyph_at(0, 2)], &[true; 3]).is_err());
        assert!(check_partition(&[glyph_at(0, 0)], &[false]).is_err());
        assert!(check_partition(&[glyph_at(3, 2)], &all).is_err());
        // A base and its mark share one range.
        let marks = [glyph_at(0, 1), glyph_at(0, 1), glyph_at(1, 3)];
        assert!(check_partition(&marks, &all).is_ok());
    }

    /// A test cluster: `(cell, span, kind, ink)`.
    type Spec = (usize, usize, ClusterKind, Option<(f32, f32)>);

    /// Clusters `(cell, span, kind, ink)`, each one glyph at its cell's
    /// left edge with ink measured in cells.
    fn owners_of(spec: &[Spec]) -> Vec<Option<usize>> {
        const W: f32 = 10.0;
        let placed: Vec<Placed> = spec
            .iter()
            .map(|&(_, _, _, ink)| Placed {
                glyph_id: 1,
                x: 0.0,
                y: 0.0,
                ink: ink.map(|(l, r)| (l * W, r * W)),
            })
            .collect();
        let clusters: Vec<Cluster> = spec
            .iter()
            .enumerate()
            .map(|(i, &(cell, span, kind, _))| Cluster {
                cell,
                span,
                glyphs: i..i + 1,
                kind,
            })
            .collect();
        partition(&clusters, &placed, W)
    }

    #[test]
    fn ignorable_clusters_own_nothing_and_are_skipped_over() {
        let lig = ClusterKind::Drawing { ligature: true };
        let letter = ClusterKind::Drawing { ligature: false };
        let sp = ClusterKind::Spacer;
        let ign = ClusterKind::Ignorable;
        // JetBrains `a-<ZWSP>>`: the shaper skips the ZWSP, so `-` is the
        // arrow's spacer, not `a`'s.
        let zwsp = owners_of(&[
            (0, 1, letter, Some((0.1, 0.9))),
            (1, 1, sp, None),
            (2, 1, ign, None),
            (3, 1, lig, Some((-1.9, 0.9))),
        ]);
        assert_eq!(zwsp, [Some(0), Some(3), None, Some(3)]);
        // An ignorable before the run does not hide the previous drawer.
        let before = owners_of(&[
            (0, 1, lig, Some((0.1, 1.9))),
            (1, 1, ign, None),
            (2, 1, sp, None),
            (3, 1, letter, Some((0.1, 0.9))),
        ]);
        assert_eq!(before, [Some(0), None, Some(0), Some(3)]);
        // Ignorables alone between letters take nothing and give nothing.
        let alone = owners_of(&[
            (0, 1, letter, Some((0.1, 0.9))),
            (1, 1, ign, None),
            (2, 1, ign, None),
            (3, 1, letter, Some((0.1, 0.9))),
        ]);
        assert_eq!(alone, [Some(0), None, None, Some(3)]);
    }

    #[test]
    fn a_blank_still_ends_a_spacer_run_next_to_ignorables() {
        let lig = ClusterKind::Drawing { ligature: true };
        let letter = ClusterKind::Drawing { ligature: false };
        let sp = ClusterKind::Spacer;
        let ign = ClusterKind::Ignorable;
        let blank = ClusterKind::Blank;
        // The next ligature's ink reaches back over the spacer more than
        // the previous one's, but the blank stops the run before it.
        let after = owners_of(&[
            (0, 1, lig, Some((0.1, 1.5))),
            (1, 1, sp, None),
            (2, 1, ign, None),
            (3, 1, blank, None),
            (4, 1, lig, Some((-3.9, 0.9))),
        ]);
        assert_eq!(after, [Some(0), Some(0), None, None, Some(4)]);
        // Looking back past the ignorable finds the blank, not the letter.
        let before = owners_of(&[
            (0, 1, letter, Some((0.1, 0.9))),
            (1, 1, blank, None),
            (2, 1, ign, None),
            (3, 1, sp, None),
            (4, 1, lig, Some((-0.9, 0.9))),
        ]);
        assert_eq!(before, [Some(0), None, None, Some(4), Some(4)]);
    }

    #[test]
    fn spacers_go_to_the_ligature_whatever_its_reach() {
        let lig = ClusterKind::Drawing { ligature: true };
        let letter = ClusterKind::Drawing { ligature: false };
        let sp = ClusterKind::Spacer;
        // Cascadia `...`: the drawer reaches 0.18 cell into the last spacer.
        let dots = owners_of(&[
            (0, 1, lig, Some((0.7, 2.18))),
            (1, 1, sp, None),
            (2, 1, sp, None),
        ]);
        assert_eq!(dots, [Some(0), Some(0), Some(0)]);
        // `...=`: `=` is a plain letter and never takes a spacer.
        let then_eq = owners_of(&[
            (0, 1, lig, Some((0.7, 2.18))),
            (1, 1, sp, None),
            (2, 1, sp, None),
            (3, 1, letter, Some((0.1, 0.9))),
        ]);
        assert_eq!(then_eq[..3], [Some(0), Some(0), Some(0)]);
        // JetBrains `W->` in italic: the arrow reaches back over the spacer
        // more than `W` overhangs into it.
        let arrow = owners_of(&[
            (0, 1, lig, Some((0.1, 1.25))),
            (1, 1, sp, None),
            (2, 1, lig, Some((-0.84, 0.85))),
        ]);
        assert_eq!(arrow, [Some(0), Some(2), Some(2)]);
        // No ink reaches the spacer: it goes to the next ligature.
        let none = owners_of(&[
            (0, 1, lig, Some((0.1, 0.9))),
            (1, 1, sp, None),
            (2, 1, lig, Some((0.1, 0.9))),
        ]);
        assert_eq!(none, [Some(0), Some(2), Some(2)]);
        // A merged cluster owns its own cells; a blank owns nothing.
        let merged = owners_of(&[
            (0, 1, letter, Some((0.1, 1.3))),
            (1, 3, lig, Some((-1.6, 0.5))),
            (4, 1, ClusterKind::Blank, None),
        ]);
        assert_eq!(merged, [Some(0), Some(1), None]);
    }

    fn raw(cluster: usize) -> RawGlyph {
        RawGlyph {
            glyph_id: 1,
            cluster,
            x_advance: 10.0,
            x_offset: 0.0,
            y_offset: 0.0,
        }
    }

    /// #19 follow-up: raw glyphs are grouped by cluster without a font.
    /// `starts` are the byte offsets of the run's cells.
    #[test]
    fn raw_glyphs_group_into_clusters_over_their_cells() {
        let group = |raw: &[RawGlyph], starts: &[usize]| -> Vec<(usize, usize, usize)> {
            let cell_of = |byte: usize| starts.partition_point(|&s| s <= byte).saturating_sub(1);
            group_clusters(raw, cell_of, starts.len())
                .into_iter()
                .map(|c| (c.cell, c.span, c.glyphs.len()))
                .collect()
        };
        // One glyph per cell.
        assert_eq!(
            group(&[raw(0), raw(1), raw(2)], &[0, 1, 2]),
            [(0, 1, 1), (1, 1, 1), (2, 1, 1)]
        );
        // A base and its mark share a cluster: one cell, two glyphs.
        assert_eq!(
            group(&[raw(0), raw(0), raw(3)], &[0, 3]),
            [(0, 1, 2), (1, 1, 1)]
        );
        // A merged cluster (Monaspace) spans every cell up to the next one.
        assert_eq!(
            group(&[raw(0), raw(0), raw(0), raw(3)], &[0, 1, 2, 3]),
            [(0, 3, 3), (3, 1, 1)]
        );
        // The last cluster spans to the end of the run.
        assert_eq!(
            group(&[raw(0), raw(1)], &[0, 1, 2, 3]),
            [(0, 1, 1), (1, 3, 1)]
        );
        // A cluster byte inside a multi-byte cell maps to that cell.
        assert_eq!(group(&[raw(0), raw(5)], &[0, 4, 8]), [(0, 1, 1), (1, 2, 1)]);
        assert_eq!(group(&[], &[0, 1]), []);
    }

    /// #19 follow-up: Cascadia Code's bold `<<=` (and `<$>`) shape as a
    /// two-cell drawer over `<<` (with one spacer) and `=` drawn on its
    /// own; the partition keeps that as [(0, 2), (2, 1)] rather than
    /// handing `=` to the ligature.
    #[test]
    fn a_drawn_last_character_keeps_its_own_cell() {
        let lig = ClusterKind::Drawing { ligature: true };
        let sp = ClusterKind::Spacer;
        // `<` draws `<<` reaching into the spacer; `=` draws itself.
        let owners = owners_of(&[
            (0, 1, lig, Some((0.1, 1.9))),
            (1, 1, sp, None),
            (2, 1, lig, Some((-0.4, 0.9))),
        ]);
        assert_eq!(owners, [Some(0), Some(0), Some(2)]);
    }

    /// #19 follow-up (ADR 0016): a combining mark on the last character of
    /// a leading-drawer ligature (Cascadia `->` + U+0301 on `>`) makes that
    /// character's cluster draw (the mark has ink), so it owns its cell
    /// and the ligature's span shrinks to one cell. The arrow is still
    /// drawn whole. Accepted by #22 (ADR 0010 amendment): the selection is
    /// a background quad that never consumes spans, and the block cursor
    /// also redraws glyphs whose ink reaches into it, so the shorter span
    /// changes nothing on screen. Pinned so a change here is visible.
    #[test]
    fn a_mark_on_a_ligature_spacer_shortens_the_ligature() {
        let lig = ClusterKind::Drawing { ligature: true };
        let owners = owners_of(&[
            (0, 1, lig, Some((0.1, 1.9))),
            // `>` is an empty spacer glyph plus an inked mark.
            (1, 1, lig, Some((0.3, 0.7))),
        ]);
        assert_eq!(owners, [Some(0), Some(1)]);
    }

    #[test]
    fn metrics_are_whole_pixels_and_baseline_is_inside() {
        let m = CellMetrics::from_face(12.06, 3.07, 0.0, 7.83);
        assert_eq!(m.width_px, 8);
        assert_eq!(m.height_px, 15);
        assert!((m.baseline_px - m.baseline_px.round()).abs() < f32::EPSILON);
        assert!(m.baseline_px > 0.0 && m.baseline_px <= 15.0);
        // Degenerate faces still give a usable cell.
        let tiny = CellMetrics::from_face(0.0, 0.0, -5.0, 0.0);
        assert_eq!((tiny.width_px, tiny.height_px), (1, 1));
    }
}
