//! The grid renderer: shapes a [`FrameInput`], fills the glyph atlas and
//! draws the four passes into any render target.

use std::num::NonZeroU64;

use tessera_grid::{Cell, Style};

use crate::atlas::{AtlasStats, DEFAULT_ATLAS_SIZE, GlyphAtlas};
use crate::error::RenderError;
use crate::frame::FrameInput;
use crate::gpu::Gpu;
use crate::palette::Palette;
use crate::passes::glyph::GlyphSource;
use crate::passes::{self, Instance, Layout, PassRanges};
use crate::shaping::{CellMetrics, ShapedGlyph, Shaper};
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
/// one instance buffer rebuilt per frame (ADR 0010).
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
    /// This frame's instances; the outer allocation is reused.
    instances: Vec<Instance>,
    /// This frame's shaped rows. Only the outer vector is reused; each
    /// row's glyph vector is allocated by the shaper every frame (caching
    /// shaped rows is #21's damage tracking).
    shaped: Vec<Vec<ShapedGlyph>>,
    /// A row copy with `HIDDEN` cells blanked, for shaping.
    shaping_row: Vec<Cell>,
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
                instances: Vec::new(),
                shaped: Vec::new(),
                shaping_row: Vec::new(),
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

    /// Draws `frame` into `view`, a `size`-pixel texture of `format`.
    ///
    /// Clears the whole view to [`Theme::background`], then draws the
    /// background, glyph, decoration and cursor passes. Colors are linear;
    /// an sRGB `format` encodes them on write, a linear one stores them as
    /// they are (as [`Gpu::clear_view`] does). An empty `size` draws
    /// nothing.
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
        self.atlas.begin_frame();
        self.shaped.clear();
        let cols = layout.visible_cols();
        for row in frame.rows.iter().take(layout.visible_rows()) {
            let row = &row[..row.len().min(cols)];
            // Hidden cells are shaped as blanks, so no ligature reaches
            // across a hidden boundary (a hidden `>` must not turn a
            // visible `=` into an arrow).
            let glyphs = if row.iter().any(|c| c.style.has(Style::HIDDEN)) {
                self.shaping_row.clear();
                self.shaping_row.extend(row.iter().map(|c| {
                    let mut cell = *c;
                    if cell.style.has(Style::HIDDEN) {
                        cell.ch = ' ';
                    }
                    cell
                }));
                self.shaper.shape_line(&self.shaping_row)
            } else {
                self.shaper.shape_line(row)
            };
            self.shaped.push(glyphs);
        }
        let ranges = match self.build(gpu, &layout, frame) {
            Err(RenderError::AtlasFull(layer)) => {
                self.atlas.recover_from_full(gpu, layer)?;
                self.build(gpu, &layout, frame)?
            }
            other => other?,
        };
        self.draw(gpu, view, format, size, &ranges)
    }

    /// Fills `self.instances` with the four passes.
    fn build(
        &mut self,
        gpu: &Gpu,
        layout: &Layout,
        frame: &FrameInput<'_>,
    ) -> Result<PassRanges, RenderError> {
        let out = &mut self.instances;
        out.clear();
        let palette = &self.palette;
        let mut source = GlyphSource {
            shaper: &self.shaper,
            atlas: &mut self.atlas,
            gpu,
        };
        let mark = |out: &Vec<Instance>| u32::try_from(out.len()).unwrap_or(u32::MAX);
        passes::background::push(out, layout, palette, frame.rows);
        let background = 0..mark(out);
        passes::glyph::push(out, layout, palette, frame.rows, &self.shaped, &mut source)?;
        let glyph = background.end..mark(out);
        passes::decoration::push(out, layout, palette, frame.rows);
        let decoration = glyph.end..mark(out);
        passes::cursor::push(
            out,
            layout,
            palette,
            self.theme.cursor,
            frame,
            &self.shaped,
            &mut source,
        )?;
        let cursor = decoration.end..mark(out);
        Ok(PassRanges {
            background,
            glyph,
            decoration,
            cursor,
        })
    }

    /// Uploads `self.instances` and records the passes.
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
        let needed = u64::try_from(self.instances.len()).unwrap_or(u64::MAX);
        // New GPU objects replace the old ones only once they validated,
        // so a failed frame never leaves the renderer holding an invalid
        // buffer or pipeline.
        if let Some(capacity) = grown_capacity(needed, self.instance_capacity, self.max_instances)?
        {
            let buffer = gpu.capture_errors(|| instance_buffer(device, capacity))?;
            self.instance_buffer = buffer;
            self.instance_capacity = capacity;
        }
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
        gpu.capture_errors(|| {
            queue.write_buffer(&self.globals, 0, bytemuck::cast_slice(&globals));
            if !self.instances.is_empty() {
                queue.write_buffer(
                    &self.instance_buffer,
                    0,
                    bytemuck::cast_slice(&self.instances),
                );
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
        })
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
}
