//! The grid renderer: shapes a [`FrameInput`], fills the glyph atlas and
//! draws the four passes into any render target, re-shaping and
//! re-uploading only the rows the frame's damage names (ADR 0016).

use std::num::NonZeroU64;

use tessera_grid::{Cell, DamageRows, GraphemeTable, Style};

use crate::atlas::{AtlasStats, DEFAULT_ATLAS_SIZE, GlyphAtlas};
use crate::damage::{self, ROW_PASSES, Row, Slots};
use crate::error::RenderError;
use crate::frame::FrameInput;
use crate::gpu::Gpu;
use crate::palette::Palette;
use crate::passes::glyph::GlyphSource;
use crate::passes::{self, Instance, Layout, PassRanges};
use crate::shaping::{CellMetrics, Shaper};
use crate::size::PhysicalSize;
use crate::theme::Theme;

/// Size of the uniform block: viewport width and height plus padding.
const GLOBALS_SIZE: u64 = 16;

/// The instance buffer never shrinks below this many instances.
const MIN_INSTANCES: u64 = 1024;

/// Bytes per instance.
const INSTANCE_SIZE: u64 = std::mem::size_of::<Instance>() as u64;

/// Draws frames of terminal cells with one font and one theme.
///
/// One renderer per terminal view: it owns that view's [`Shaper`] (so a
/// font size change in one window leaves the others alone, see
/// [`Shaper::with_fonts`]) and glyph atlas. It is not tied to a target:
/// [`Self::render`] draws into any texture view, which is how
/// [`crate::RenderSurface::render`] and [`crate::OffscreenTarget::render`]
/// share it.
///
/// Each frame clears the target to [`Theme::background`] and draws the
/// background, glyph, decoration and cursor passes in that order, from
/// one instance buffer (ADR 0010). Rows keep their shaped glyphs and
/// instances between frames: only the rows in [`FrameInput::damage`] are
/// shaped and uploaded again, plus every row when the layout, theme, font
/// size or atlas contents changed under them (ADR 0016).
#[derive(Debug)]
pub struct Renderer {
    shaper: Shaper,
    atlas: GlyphAtlas,
    theme: Theme,
    palette: Palette,
    shader: wgpu::ShaderModule,
    pipeline_layout: wgpu::PipelineLayout,
    bind_group: wgpu::BindGroup,
    globals: wgpu::Buffer,
    instance_buffer: wgpu::Buffer,
    /// Capacity of `instance_buffer` in instances.
    instance_capacity: u64,
    /// Most instances one buffer may hold: the device's
    /// `max_buffer_size` over the instance size.
    max_instances: u64,
    /// One pipeline per target format seen so far.
    pipelines: Vec<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
    /// Every visible row's shaped glyphs and instances, kept between
    /// frames.
    rows: Vec<Row>,
    /// What `rows` were built for; `None` until a frame succeeds and after
    /// anything that invalidates them (a failed frame, a theme change).
    built: Option<BuildKey>,
    /// The atlas counters when `rows` last referenced valid atlas slots: a
    /// later eviction or clear may have moved a glyph of a row that was
    /// not rebuilt.
    atlas_seen: (u64, u64),
    /// Where each row's instances sit in the instance buffer.
    slots: Slots,
    /// A CPU copy of the instance buffer, laid out by `slots`.
    mirror: Vec<Instance>,
    /// The cursor's instances this frame.
    cursor: Vec<Instance>,
    /// A row copy with `HIDDEN` cells blanked, for shaping.
    shaping_row: Vec<Cell>,
    /// The target size the globals buffer holds, so an unchanged size is
    /// not uploaded again.
    globals_size: Option<PhysicalSize>,
    /// Counters for tests and diagnostics.
    stats: RenderStats,
    /// Test hook: the next upload fails after the mirror was written.
    #[cfg(test)]
    fail_next_upload: bool,
}

/// What a renderer's cached rows depend on besides the cells.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BuildKey {
    layout: Layout,
    /// The shaper's pixel size: a font change that keeps the cell size
    /// still changes every glyph.
    size_px_bits: u32,
    /// Rows drawn (the frame's rows that start inside the target).
    rows: usize,
}

/// Work counters of a [`Renderer`], cumulative since it was created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RenderStats {
    /// Frames drawn.
    pub frames: u64,
    /// Rows shaped ([`Shaper::shape_line_into`] calls).
    pub rows_shaped: u64,
    /// Rows whose instances were rebuilt (shaped rows, plus every row when
    /// the atlas moved glyphs under them).
    pub rows_built: u64,
    /// Instances written to the GPU buffer.
    pub instances_uploaded: u64,
    /// Times the instance buffer was laid out again (and uploaded whole).
    pub relayouts: u64,
}

impl Renderer {
    /// A renderer drawing with `shaper` in `theme`, with an atlas of
    /// [`DEFAULT_ATLAS_SIZE`] texels per side.
    ///
    /// The shaper decides the font: [`Theme::font_family`] and
    /// [`Theme::font_size`] are what a frontend builds it from, and are not
    /// read here. Its backing scale is also the scale padding and cursor
    /// strokes are drawn at, so keep it in step with the target's scale
    /// through [`Self::set_font_size`].
    ///
    /// # Errors
    ///
    /// [`RenderError::Gpu`] when the shader or a buffer fails validation.
    pub fn new(gpu: &Gpu, theme: Theme, shaper: Shaper) -> Result<Self, RenderError> {
        Self::with_atlas_size(gpu, theme, shaper, DEFAULT_ATLAS_SIZE)
    }

    /// Like [`Self::new`] with atlas layers of `atlas_size` texels per side
    /// (clamped like [`GlyphAtlas::with_size`]).
    pub fn with_atlas_size(
        gpu: &Gpu,
        theme: Theme,
        shaper: Shaper,
        atlas_size: u32,
    ) -> Result<Self, RenderError> {
        let atlas = GlyphAtlas::with_size(gpu, shaper.fonts(), atlas_size);
        let device = gpu.device();
        gpu.capture_errors(|| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("tessera.quad"),
                source: wgpu::ShaderSource::Wgsl(include_str!("shaders/quad.wgsl").into()),
            });
            let texture_entry = |binding| wgpu::BindGroupLayoutEntry {
                binding,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            };
            let bind_group_layout =
                device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("tessera.quad"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::VERTEX,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: NonZeroU64::new(GLOBALS_SIZE),
                            },
                            count: None,
                        },
                        texture_entry(1),
                        texture_entry(2),
                    ],
                });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("tessera.quad"),
                bind_group_layouts: &[Some(&bind_group_layout)],
                immediate_size: 0,
            });
            let globals = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tessera.globals"),
                size: GLOBALS_SIZE,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("tessera.quad"),
                layout: &bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: globals.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(
                            atlas.view(crate::AtlasLayer::Alpha),
                        ),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(
                            atlas.view(crate::AtlasLayer::Color),
                        ),
                    },
                ],
            });
            let instance_buffer = instance_buffer(device, MIN_INSTANCES);
            let palette = Palette::new(&theme);
            Self {
                shaper,
                atlas,
                theme,
                palette,
                shader: module,
                pipeline_layout,
                bind_group,
                globals,
                instance_buffer,
                instance_capacity: MIN_INSTANCES,
                max_instances: (device.limits().max_buffer_size / INSTANCE_SIZE).max(1),
                pipelines: Vec::new(),
                rows: Vec::new(),
                built: None,
                atlas_seen: (0, 0),
                slots: Slots::default(),
                mirror: Vec::new(),
                cursor: Vec::new(),
                shaping_row: Vec::new(),
                globals_size: None,
                stats: RenderStats::default(),
                #[cfg(test)]
                fail_next_upload: false,
            }
        })
    }

    /// The theme in use.
    #[must_use]
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Switches to `theme` from the next frame on.
    ///
    /// Colors, opacity and padding apply immediately; the font fields are
    /// not read (change the font with [`Self::set_font_size`] or a new
    /// renderer).
    pub fn set_theme(&mut self, theme: Theme) {
        self.palette = Palette::new(&theme);
        self.theme = theme;
        self.built = None;
    }

    /// Changes the font size or backing scale; the atlas clears itself at
    /// the next frame when the pixel size changes. See
    /// [`Shaper::set_font_size`].
    pub fn set_font_size(&mut self, size_pt: f32, scale: f32) {
        self.shaper.set_font_size(size_pt, scale);
    }

    /// The shaper drawing the text.
    #[must_use]
    pub fn shaper(&self) -> &Shaper {
        &self.shaper
    }

    /// The cell size at the current font size.
    #[must_use]
    pub fn cell_metrics(&self) -> CellMetrics {
        self.shaper.cell_metrics()
    }

    /// The glyph atlas's counters, including
    /// [`AtlasStats::full_resets`].
    #[must_use]
    pub fn atlas_stats(&self) -> AtlasStats {
        self.atlas.stats()
    }

    /// How much work the frames so far did: rows shaped, instances
    /// uploaded.
    #[must_use]
    pub fn stats(&self) -> RenderStats {
        self.stats
    }

    /// True when the next frame must be drawn even if nothing in it
    /// changed: no frame has been drawn yet (or the last one failed, or
    /// the theme changed), or the fallback font index became ready while a
    /// row shows a `.notdef` glyph that it may now resolve (ADR 0007). A
    /// host that skips frames without damage asks this first.
    #[must_use]
    pub fn needs_redraw(&self) -> bool {
        self.built.is_none()
            || (self.rows.iter().any(|r| r.pending_fallback)
                && self.shaper.fonts().fallback_index_ready())
    }

    /// Draws `frame` into `view`, a `size`-pixel texture of `format`.
    ///
    /// Clears the whole view to [`Theme::background`], then draws the
    /// background, glyph, decoration and cursor passes. Colors are linear;
    /// an sRGB `format` encodes them on write, a linear one stores them as
    /// they are (as [`Gpu::clear_view`] does). An empty `size` draws
    /// nothing.
    ///
    /// Only the rows in [`FrameInput::damage`] are shaped and uploaded
    /// again; the others are drawn from what the previous frame built, so
    /// they must not have changed since. Every row is rebuilt after a
    /// change of layout (target size, font size, padding), theme or row
    /// count, a failed frame, or [`DamageRows::All`]. The cursor, focus and
    /// clear color need no damage. A row showing a `.notdef` glyph is
    /// shaped again once the fallback font index is ready.
    ///
    /// When the atlas runs out of room mid-frame it is reset once and the
    /// frame rebuilt ([`crate::GlyphAtlas::recover_from_full`]).
    ///
    /// # Errors
    ///
    /// [`RenderError::AtlasFull`] when the frame's glyphs do not fit in the
    /// atlas even after that reset (nothing is drawn),
    /// [`RenderError::FrameTooLarge`] when it needs more quads than one
    /// buffer may hold, and
    /// [`RenderError::Gpu`] when recording or submitting fails validation
    /// (for example a `view` that is not a render attachment of `format`).
    /// After an error the next frame rebuilds every row.
    pub fn render(
        &mut self,
        gpu: &Gpu,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        size: PhysicalSize,
        frame: &FrameInput<'_>,
    ) -> Result<(), RenderError> {
        if size.is_empty() {
            return Ok(());
        }
        let layout = Layout::new(
            self.shaper.cell_metrics(),
            self.shaper.scale(),
            self.theme.padding,
            size,
        );
        let result = self
            .prepare(gpu, &layout, frame)
            .and_then(|ranges| self.draw(gpu, view, format, size, &ranges));
        if result.is_err() {
            // The mirror may hold slots the GPU never received: forget it,
            // so the next frame lays out and uploads everything.
            self.built = None;
            self.mirror.clear();
        }
        result
    }

    /// Shapes and builds the damaged rows and the cursor, and uploads what
    /// changed.
    fn prepare(
        &mut self,
        gpu: &Gpu,
        layout: &Layout,
        frame: &FrameInput<'_>,
    ) -> Result<PassRanges, RenderError> {
        self.atlas.begin_frame();
        let key = BuildKey {
            layout: *layout,
            size_px_bits: self.shaper.size_px().to_bits(),
            rows: frame.rows.len().min(layout.visible_rows()),
        };
        let everything = self.built != Some(key);
        self.built = None;
        if everything {
            // Upload all of it, cursor included, whatever the mirror says.
            self.mirror.clear();
        }
        self.rows.resize_with(key.rows, Row::default);
        let cols = layout.visible_cols();
        let fallback_ready = self.shaper.fonts().fallback_index_ready();
        // Rows to shape again: the damage, rows whose length changed, and
        // rows waiting for the fallback index once it is ready.
        let mut reshape = vec![false; key.rows];
        for (r, flag) in reshape.iter_mut().enumerate() {
            let row = &self.rows[r];
            let len = frame.rows[r].len().min(cols);
            *flag = everything
                || damaged(&frame.damage, r)
                || row.len != len
                || (row.pending_fallback && fallback_ready);
        }
        for (r, _) in reshape.iter().enumerate().filter(|(_, f)| **f) {
            self.shape_row(
                r,
                frame.rows[r],
                frame.clusters.get(r),
                cols,
                fallback_ready,
            );
        }
        // Rows to build: the reshaped ones, or all of them when the atlas
        // may have moved a glyph of a row kept from an earlier frame. Only
        // frames touch the atlas today, and a frame ends with every row
        // valid, so this is a guard; evictions during this frame are
        // handled below.
        let mut build = if self.atlas_counters() == self.atlas_seen {
            reshape
        } else {
            vec![true; key.rows]
        };
        let mut recovered = false;
        loop {
            let before = self.atlas_counters();
            match self.build(gpu, layout, frame, &build) {
                Err(RenderError::AtlasFull(layer)) if !recovered => {
                    self.atlas.recover_from_full(gpu, layer)?;
                    recovered = true;
                    build = vec![true; key.rows];
                }
                Err(e) => return Err(e),
                Ok(()) => {
                    // An eviction during a partial build may have taken a
                    // glyph of a row kept from before: build them all once
                    // more (a full build only evicts glyphs no row uses
                    // any more, since every row touches its glyphs).
                    if self.atlas_counters() != before && build.iter().any(|b| !b) {
                        build = vec![true; key.rows];
                        continue;
                    }
                    break;
                }
            }
        }
        self.atlas_seen = self.atlas_counters();
        let ranges = self.upload(gpu, &build)?;
        self.built = Some(key);
        Ok(ranges)
    }

    /// The atlas counters that change when a slot handed out earlier may
    /// no longer hold its glyph.
    fn atlas_counters(&self) -> (u64, u64) {
        let stats = self.atlas.stats();
        (stats.evicted, stats.cleared)
    }

    /// Shapes row `r` (`cells`, with cluster text `clusters`) into its
    /// cached glyph vector.
    fn shape_row(
        &mut self,
        r: usize,
        cells: &[Cell],
        clusters: Option<&GraphemeTable>,
        cols: usize,
        fallback_ready: bool,
    ) {
        static NO_CLUSTERS: GraphemeTable = GraphemeTable::new();
        let cells = &cells[..cells.len().min(cols)];
        let clusters = clusters.unwrap_or(&NO_CLUSTERS);
        let row = &mut self.rows[r];
        // Hidden cells are shaped as blanks, so no ligature reaches
        // across a hidden boundary (a hidden `>` must not turn a visible
        // `=` into an arrow).
        if cells.iter().any(|c| c.style.has(Style::HIDDEN)) {
            self.shaping_row.clear();
            self.shaping_row.extend(cells.iter().map(|c| {
                let mut cell = *c;
                if cell.style.has(Style::HIDDEN) {
                    cell.ch = ' ';
                }
                cell
            }));
            self.shaper
                .shape_line_into(&self.shaping_row, clusters, &mut row.shaped);
        } else {
            self.shaper
                .shape_line_into(cells, clusters, &mut row.shaped);
        }
        row.len = cells.len();
        row.pending_fallback = !fallback_ready
            && row
                .shaped
                .iter()
                .any(|g| g.glyph_id == 0 && !g.face_id.is_builtin());
        self.stats.rows_shaped += 1;
    }

    /// Rebuilds the instances of the rows flagged in `build`, and the
    /// cursor's.
    fn build(
        &mut self,
        gpu: &Gpu,
        layout: &Layout,
        frame: &FrameInput<'_>,
        build: &[bool],
    ) -> Result<(), RenderError> {
        let palette = &self.palette;
        let mut source = GlyphSource {
            shaper: &self.shaper,
            atlas: &mut self.atlas,
            gpu,
        };
        for (r, row) in self.rows.iter_mut().enumerate() {
            if !build[r] {
                continue;
            }
            let cells = &frame.rows[r][..row.len];
            let [bg, glyphs, deco] = &mut row.passes;
            bg.clear();
            glyphs.clear();
            deco.clear();
            passes::background::push_row(bg, layout, palette, r, cells);
            passes::glyph::push_row(
                glyphs,
                layout,
                palette,
                r,
                (cells, &row.shaped),
                &mut source,
            )?;
            passes::decoration::push_row(deco, layout, palette, r, cells);
            self.stats.rows_built += 1;
        }
        self.cursor.clear();
        let rows = &self.rows;
        passes::cursor::push(
            &mut self.cursor,
            layout,
            palette,
            self.theme.cursor,
            frame,
            |r| rows.get(r).map(|row| row.shaped.as_slice()),
            &mut source,
        )
    }

    /// Writes the rebuilt rows and the cursor into the instance buffer, or
    /// lays the buffer out again and writes all of it when a row outgrew
    /// its slot, and returns the pass ranges to draw.
    fn upload(&mut self, gpu: &Gpu, built: &[bool]) -> Result<PassRanges, RenderError> {
        let relayout = !self.slots.fits(&self.rows, self.cursor.len())
            || self.mirror.len() != usize::try_from(self.slots.total()).unwrap_or(usize::MAX);
        if relayout {
            let counts: Vec<[usize; ROW_PASSES]> = self
                .rows
                .iter()
                .map(|row| std::array::from_fn(|p| row.passes[p].len()))
                .collect();
            let exact = Slots::new(&counts, self.cursor.len(), false);
            let roomy = Slots::new(&counts, self.cursor.len(), true);
            let slots = if u64::from(roomy.total()) <= self.max_instances {
                roomy
            } else {
                exact
            };
            self.ensure_capacity(gpu, u64::from(slots.total()))?;
            self.slots = slots;
            self.mirror.clear();
            self.mirror.resize(
                usize::try_from(self.slots.total()).unwrap_or(0),
                damage::EMPTY,
            );
            self.stats.relayouts += 1;
        }
        // Pass by pass, so the slots of consecutive rebuilt rows (adjacent
        // within a pass) merge into one write.
        let mut spans: Vec<std::ops::Range<u32>> = Vec::new();
        for p in 0..ROW_PASSES {
            for (r, row) in self.rows.iter().enumerate() {
                if !relayout && !built[r] {
                    continue;
                }
                let slot = self.slots.row(p, r);
                damage::fill_slot(&mut self.mirror, slot.clone(), &row.passes[p]);
                match spans.last_mut() {
                    Some(last) if last.end == slot.start => last.end = slot.end,
                    _ => spans.push(slot),
                }
            }
        }
        let cursor = self.slots.cursor();
        if relayout || !damage::slot_holds(&self.mirror, cursor.clone(), &self.cursor) {
            damage::fill_slot(&mut self.mirror, cursor.clone(), &self.cursor);
            spans.push(cursor);
        }
        if relayout {
            spans.clear();
            spans.push(0..self.slots.total());
        }
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_upload) {
            return Err(RenderError::Gpu("injected upload failure".into()));
        }
        let queue = gpu.queue();
        gpu.capture_errors(|| {
            for span in &spans {
                let start = usize::try_from(span.start).unwrap_or(0);
                let end = usize::try_from(span.end).unwrap_or(0);
                if start < end {
                    queue.write_buffer(
                        &self.instance_buffer,
                        u64::from(span.start) * INSTANCE_SIZE,
                        bytemuck::cast_slice(&self.mirror[start..end]),
                    );
                }
            }
        })?;
        self.stats.instances_uploaded += spans
            .iter()
            .map(|s| u64::from(s.end - s.start))
            .sum::<u64>();
        Ok(PassRanges {
            background: self.slots.section(0),
            glyph: self.slots.section(1),
            decoration: self.slots.section(2),
            cursor: self.slots.cursor(),
        })
    }

    /// Grows the instance buffer to hold `needed` instances; the new
    /// buffer and capacity replace the old ones only once it validated.
    fn ensure_capacity(&mut self, gpu: &Gpu, needed: u64) -> Result<(), RenderError> {
        let device = gpu.device();
        grow_buffer(
            (&mut self.instance_buffer, &mut self.instance_capacity),
            needed,
            self.max_instances,
            |capacity| gpu.capture_errors(|| instance_buffer(device, capacity)),
        )
    }

    /// Records the passes.
    fn draw(
        &mut self,
        gpu: &Gpu,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        size: PhysicalSize,
        ranges: &PassRanges,
    ) -> Result<(), RenderError> {
        let device = gpu.device();
        let queue = gpu.queue();
        // A new pipeline replaces nothing until it validated, so a failed
        // frame never leaves the renderer holding an invalid one.
        if !self.pipelines.iter().any(|(f, _)| *f == format) {
            let pipeline = gpu.capture_errors(|| self.pipeline(device, format))?;
            self.pipelines.push((format, pipeline));
        }
        let Some((_, pipeline)) = self.pipelines.iter().find(|(f, _)| *f == format) else {
            return Err(RenderError::Gpu(format!("no pipeline for {format:?}")));
        };
        let globals = [
            passes::f32_from(size.width),
            passes::f32_from(size.height),
            0.0,
            0.0,
        ];
        let globals_changed = self.globals_size != Some(size);
        gpu.capture_errors(|| {
            if globals_changed {
                queue.write_buffer(&self.globals, 0, bytemuck::cast_slice(&globals));
            }
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("tessera.frame"),
            });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("tessera.frame"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(self.theme.background.to_clear_color()),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
                for (label, range) in ranges.labelled() {
                    if range.is_empty() {
                        continue;
                    }
                    pass.push_debug_group(label);
                    pass.draw(0..4, range);
                    pass.pop_debug_group();
                }
            }
            queue.submit(Some(encoder.finish()));
        })?;
        self.globals_size = Some(size);
        self.stats.frames += 1;
        Ok(())
    }

    fn pipeline(&self, device: &wgpu::Device, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tessera.quad"),
            layout: Some(&self.pipeline_layout),
            vertex: wgpu::VertexState {
                module: &self.shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: INSTANCE_SIZE,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &Instance::ATTRIBUTES,
                })],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..wgpu::PrimitiveState::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &self.shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        })
    }
}

/// The instance capacity to grow to for a frame of `needed` instances, or
/// `None` when `current` suffices: the next power of two, capped at `max`.
///
/// # Errors
///
/// [`RenderError::FrameTooLarge`] when `needed` exceeds `max`.
fn grown_capacity(needed: u64, current: u64, max: u64) -> Result<Option<u64>, RenderError> {
    if needed > max {
        return Err(RenderError::FrameTooLarge { needed, max });
    }
    if needed <= current {
        return Ok(None);
    }
    Ok(Some(needed.next_power_of_two().min(max)))
}

/// Grows `buffer` (with its `capacity` in instances) to hold `needed`
/// instances with [`grown_capacity`], creating the larger buffer with
/// `create`. The buffer and the capacity change together and only when
/// `create` succeeds, so a failed growth is retried by the next frame.
///
/// # Errors
///
/// [`RenderError::FrameTooLarge`] from [`grown_capacity`], or `create`'s
/// error.
fn grow_buffer<B>(
    (buffer, capacity): (&mut B, &mut u64),
    needed: u64,
    max: u64,
    create: impl FnOnce(u64) -> Result<B, RenderError>,
) -> Result<(), RenderError> {
    if let Some(grown) = grown_capacity(needed, *capacity, max)? {
        *buffer = create(grown)?;
        *capacity = grown;
    }
    Ok(())
}

/// True when `damage` covers row `r`.
fn damaged(damage: &DamageRows, r: usize) -> bool {
    u16::try_from(r).map_or(true, |r| damage.contains(r))
}

/// A vertex buffer for `capacity` instances.
fn instance_buffer(device: &wgpu::Device, capacity: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("tessera.instances"),
        size: capacity.max(1) * INSTANCE_SIZE,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_is_send_and_sync() {
        fn is_send_sync<T: Send + Sync>() {}
        is_send_sync::<Renderer>();
    }

    /// The capacity is raised only together with a buffer that was
    /// created; a failed creation leaves both as they were.
    #[test]
    fn capacity_is_raised_only_when_the_buffer_was_created() {
        let mut buffer = "small";
        let mut capacity = 1024;
        let failed = grow_buffer((&mut buffer, &mut capacity), 1500, 1 << 20, |_| {
            Err(RenderError::Gpu("out of memory".into()))
        });
        assert!(failed.is_err());
        assert_eq!((buffer, capacity), ("small", 1024), "nothing changed");
        grow_buffer((&mut buffer, &mut capacity), 1500, 1 << 20, |c| {
            assert_eq!(c, 2048);
            Ok("large")
        })
        .expect("grows");
        assert_eq!((buffer, capacity), ("large", 2048));
        grow_buffer((&mut buffer, &mut capacity), 100, 1 << 20, |_| {
            panic!("a smaller frame does not create a buffer")
        })
        .expect("no growth needed");
    }

    #[test]
    fn capacity_grows_to_a_power_of_two_within_the_limit() {
        assert_eq!(grown_capacity(10, 1024, 1 << 20).ok(), Some(None));
        assert!(
            grown_capacity(900, 1024, 800).is_err(),
            "the limit applies even when the buffer is larger"
        );
        assert_eq!(grown_capacity(1025, 1024, 1 << 20).ok(), Some(Some(2048)));
        assert_eq!(grown_capacity(1025, 1024, 1500).ok(), Some(Some(1500)));
        assert!(matches!(
            grown_capacity(1501, 1024, 1500),
            Err(RenderError::FrameTooLarge {
                needed: 1501,
                max: 1500
            })
        ));
    }

    /// A frame over the instance limit fails cleanly and leaves the
    /// renderer usable.
    #[test]
    fn an_oversized_frame_does_not_poison_the_renderer() {
        if std::env::var_os("TESSERA_SKIP_GPU_TESTS").is_some() {
            return;
        }
        let gpu = Gpu::headless().expect("a Metal adapter");
        let shaper = Shaper::new(&crate::FontConfig::default()).expect("fonts");
        let mut renderer = Renderer::new(&gpu, Theme::default(), shaper).expect("renderer");
        // Room for exactly 1,536 quads: the buffer grows once, then stops.
        renderer.max_instances = 1536;
        let m = renderer.cell_metrics();
        let size = PhysicalSize::new(1000 * m.width_px + 16, 2 * m.height_px + 16);
        let target = crate::OffscreenTarget::new(&gpu, size, wgpu::TextureFormat::Rgba8Unorm);
        // One background quad per cell: the colors alternate.
        let row = |n: usize| -> Vec<Cell> {
            (0..n)
                .map(|i| {
                    let bg = tessera_grid::Color::Indexed(u8::try_from(i % 2).unwrap_or(0));
                    Cell::new(
                        ' ',
                        Style {
                            bg,
                            ..Style::DEFAULT
                        },
                    )
                })
                .collect()
        };
        // Two rows, so up to 2,000 cells fit the target.
        let mut draw = |cells: &[Cell]| {
            let rows: Vec<&[Cell]> = cells.chunks(1000).collect();
            target.render(&gpu, &mut renderer, &FrameInput::new(&rows))
        };
        draw(&row(1200)).expect("grows to 1,536");
        let err = draw(&row(2000)).expect_err("2,000 quads exceed the limit");
        assert!(
            matches!(
                err,
                RenderError::FrameTooLarge {
                    needed: 2000,
                    max: 1536
                }
            ),
            "{err:?}"
        );
        draw(&row(100)).expect("a small frame still renders");
        draw(&row(1500)).expect("so does one at the limit");
    }

    fn small_frame_renderer() -> Option<(Gpu, Renderer, crate::OffscreenTarget)> {
        if std::env::var_os("TESSERA_SKIP_GPU_TESTS").is_some() {
            return None;
        }
        let gpu = Gpu::headless().expect("a Metal adapter");
        let shaper = Shaper::new(&crate::FontConfig::default()).expect("fonts");
        let renderer = Renderer::new(&gpu, Theme::default(), shaper).expect("renderer");
        let m = renderer.cell_metrics();
        let size = PhysicalSize::new(20 * m.width_px + 16, 3 * m.height_px + 16);
        let target = crate::OffscreenTarget::new(&gpu, size, wgpu::TextureFormat::Rgba8Unorm);
        Some((gpu, renderer, target))
    }

    fn frame(rows: &[Vec<Cell>]) -> Vec<&[Cell]> {
        rows.iter().map(Vec::as_slice).collect()
    }

    fn text_row(text: &str) -> Vec<Cell> {
        text.chars()
            .map(|ch| Cell::new(ch, Style::DEFAULT))
            .collect()
    }

    /// A row that drew `.notdef` before the fallback index was ready is
    /// shaped again once it is, without damage from the host (ADR 0007).
    #[test]
    fn rows_waiting_for_the_fallback_index_are_reshaped_when_it_is_ready() {
        let Some((gpu, mut renderer, target)) = small_frame_renderer() else {
            return;
        };
        let row = text_row("abc");
        let rows: [&[Cell]; 1] = [&row];
        target
            .render(&gpu, &mut renderer, &FrameInput::new(&rows))
            .expect("frame");
        assert!(!renderer.needs_redraw());
        // Pretend the row drew a `.notdef` while the index was building.
        renderer.rows[0].pending_fallback = true;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !renderer.shaper().fonts().fallback_index_ready() {
            assert!(
                std::time::Instant::now() < deadline,
                "the index never finished"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(renderer.needs_redraw(), "the host must draw again");
        let shaped = renderer.stats().rows_shaped;
        let frame = FrameInput::new(&rows).with_damage(DamageRows::None);
        target.render(&gpu, &mut renderer, &frame).expect("frame");
        assert_eq!(renderer.stats().rows_shaped, shaped + 1);
        assert!(!renderer.rows[0].pending_fallback, "resolved for good");
        assert!(!renderer.needs_redraw());
    }

    /// Each row's shaped-glyph vector is reused: re-shaping a row writes
    /// into the same allocation.
    #[test]
    fn shaped_rows_reuse_their_vectors() {
        let Some((gpu, mut renderer, target)) = small_frame_renderer() else {
            return;
        };
        let rows_a = [text_row("hello world"), text_row("second row")];
        let rows_b = [text_row("other text!"), text_row("second wor")];
        let a = frame(&rows_a);
        target
            .render(&gpu, &mut renderer, &FrameInput::new(&a))
            .expect("frame");
        let pointers: Vec<_> = renderer.rows.iter().map(|r| r.shaped.as_ptr()).collect();
        let instances: Vec<_> = renderer.rows.iter().map(|r| r.passes[1].as_ptr()).collect();
        let b = frame(&rows_b);
        target
            .render(&gpu, &mut renderer, &FrameInput::new(&b))
            .expect("frame");
        assert_eq!(renderer.stats().rows_shaped, 4);
        let after: Vec<_> = renderer.rows.iter().map(|r| r.shaped.as_ptr()).collect();
        assert_eq!(pointers, after, "shaped vectors reused");
        let after: Vec<_> = renderer.rows.iter().map(|r| r.passes[1].as_ptr()).collect();
        assert_eq!(instances, after, "instance vectors reused");
    }

    /// A failed upload must not leave the mirror claiming slots the GPU
    /// never received: the cursor slot was written to the mirror, the
    /// upload failed, and a later frame with that same cursor must still
    /// upload it.
    #[test]
    fn a_failed_upload_does_not_leave_a_stale_cursor() {
        let Some((gpu, mut renderer, target)) = small_frame_renderer() else {
            return;
        };
        let rows = [text_row("hello"), text_row("world")];
        let rows = frame(&rows);
        let at = |row, col| Some(crate::CursorState::new(row, col, crate::CursorShape::Block));
        let draw = |renderer: &mut Renderer, cursor| {
            let frame = FrameInput::new(&rows)
                .with_cursor(cursor)
                .with_damage(DamageRows::None);
            target.render(&gpu, renderer, &frame)
        };
        target
            .render(
                &gpu,
                &mut renderer,
                &FrameInput::new(&rows).with_cursor(at(0, 0)),
            )
            .expect("first frame");
        renderer.fail_next_upload = true;
        assert!(
            draw(&mut renderer, at(1, 2)).is_err(),
            "the injected failure"
        );
        draw(&mut renderer, at(1, 2)).expect("the same frame again");
        let drawn = target.read_back(&gpu).expect("readback");
        let mut reference = small_frame_renderer().expect("gpu").1;
        target
            .render(
                &gpu,
                &mut reference,
                &FrameInput::new(&rows).with_cursor(at(1, 2)),
            )
            .expect("reference");
        let expected = target.read_back(&gpu).expect("readback");
        assert!(drawn == expected, "the cursor moved on screen too");
    }
}
