//! Render into a texture with no window, and read the pixels back.
//!
//! This is how CI exercises the clear pass and the grid passes (the golden
//! tests): no display, no surface, just a `Gpu::headless()` device, a
//! texture, and a mapped staging buffer.

use crate::error::RenderError;
use crate::frame::FrameInput;
use crate::gpu::Gpu;
use crate::renderer::Renderer;
use crate::size::PhysicalSize;
use crate::theme::Rgba;

/// Bytes per pixel for the 8-bit RGBA/BGRA formats this target supports.
const BYTES_PER_PIXEL: u32 = 4;

/// The formats [`OffscreenTarget`] accepts: 8-bit-per-channel, four
/// channels, so [`OffscreenTarget::read_back`] returns plain `0..=255`
/// bytes in channel order.
pub const SUPPORTED_FORMATS: [wgpu::TextureFormat; 4] = [
    wgpu::TextureFormat::Rgba8Unorm,
    wgpu::TextureFormat::Rgba8UnormSrgb,
    wgpu::TextureFormat::Bgra8Unorm,
    wgpu::TextureFormat::Bgra8UnormSrgb,
];

/// `true` when `format` is one of [`SUPPORTED_FORMATS`].
#[must_use]
pub fn is_supported_format(format: wgpu::TextureFormat) -> bool {
    SUPPORTED_FORMATS.contains(&format)
}

/// Bytes the staging buffer for a `size` readback needs, or `None` when
/// the padded row does not fit in a `u32`.
fn staging_buffer_size(size: PhysicalSize) -> Option<u64> {
    let padded = padded_bytes_per_row(size.width)?;
    Some(u64::from(padded) * u64::from(size.height))
}

/// A 2D color texture that can be cleared and copied back to the CPU.
#[derive(Debug)]
pub struct OffscreenTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: PhysicalSize,
    format: wgpu::TextureFormat,
}

impl OffscreenTarget {
    /// Allocates a target of `size` pixels in `format`.
    ///
    /// `format` must be one of [`SUPPORTED_FORMATS`]
    /// (`Rgba8Unorm`, `Rgba8UnormSrgb`, `Bgra8Unorm`, `Bgra8UnormSrgb`).
    ///
    /// # Panics
    ///
    /// Panics if `size` is empty, exceeds [`Gpu::max_texture_dimension`] on
    /// either side, or needs a readback staging buffer larger than
    /// [`Gpu::max_buffer_size`] (so every target that can be created can
    /// also be read back), or if `format` is not a supported format. All are
    /// programmer errors in a test harness.
    #[must_use]
    pub fn new(gpu: &Gpu, size: PhysicalSize, format: wgpu::TextureFormat) -> Self {
        assert!(!size.is_empty(), "offscreen target must be non-empty");
        let max = gpu.max_texture_dimension();
        assert!(
            size.width <= max && size.height <= max,
            "offscreen target {size:?} exceeds the device limit of {max} pixels per side"
        );
        let staging = staging_buffer_size(size)
            .unwrap_or_else(|| panic!("offscreen target {size:?} row overflows u32"));
        let max_buffer = gpu.max_buffer_size();
        assert!(
            staging <= max_buffer,
            "offscreen target {size:?} needs a {staging}-byte readback buffer, \
             over the device limit of {max_buffer} bytes"
        );
        assert!(
            is_supported_format(format),
            "offscreen target needs one of {SUPPORTED_FORMATS:?}, got {format:?}"
        );
        let texture = gpu.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("tessera.offscreen"),
            size: size.extent(),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            size,
            format,
        }
    }

    /// Size in pixels.
    #[must_use]
    pub fn size(&self) -> PhysicalSize {
        self.size
    }

    /// Texture format.
    #[must_use]
    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// The view to attach to render passes.
    #[must_use]
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// Clears the whole target to `color` (linear, written premultiplied;
    /// encoded if the format is sRGB).
    pub fn clear(&self, gpu: &Gpu, color: Rgba) -> Result<(), RenderError> {
        gpu.clear_view(&self.view, color)
    }

    /// Draws `frame` with `renderer` into the whole target; see
    /// [`Renderer::render`].
    pub fn render(
        &self,
        gpu: &Gpu,
        renderer: &mut Renderer,
        frame: &FrameInput<'_>,
    ) -> Result<(), RenderError> {
        renderer.render(gpu, &self.view, self.format, self.size, frame)
    }

    /// Copies the texture to the CPU and returns tightly packed rows of
    /// 4-byte pixels in the texture's own channel order (`RGBA` or `BGRA`).
    ///
    /// Blocks until the GPU has finished all submitted work.
    pub fn read_back(&self, gpu: &Gpu) -> Result<Vec<u8>, RenderError> {
        let padded_bytes_per_row = padded_bytes_per_row(self.size.width).ok_or_else(|| {
            RenderError::Readback(format!("row of {} pixels overflows", self.size.width))
        })?;
        let unpadded_bytes_per_row = u64::from(self.size.width) * u64::from(BYTES_PER_PIXEL);
        let unpadded_bytes_per_row = usize::try_from(unpadded_bytes_per_row)
            .map_err(|_| RenderError::Readback("row too large for this platform".into()))?;
        let buffer_size = staging_buffer_size(self.size)
            .ok_or_else(|| RenderError::Readback("texture too large to read back".into()))?;
        let unpadded_size = unpadded_bytes_per_row
            .checked_mul(usize::try_from(self.size.height).unwrap_or(usize::MAX))
            .ok_or_else(|| RenderError::Readback("texture too large to read back".into()))?;

        let staging = gpu.capture_errors(|| {
            let staging = gpu.device().create_buffer(&wgpu::BufferDescriptor {
                label: Some("tessera.readback"),
                size: buffer_size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder =
                gpu.device()
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("tessera.readback"),
                    });
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &staging,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded_bytes_per_row),
                        rows_per_image: Some(self.size.height),
                    },
                },
                self.size.extent(),
            );
            gpu.queue().submit(Some(encoder.finish()));
            staging
        })?;

        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            // The receiver only disappears if the caller already gave up.
            let _ = tx.send(r);
        });
        gpu.device()
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        rx.recv()
            .map_err(|_| RenderError::Readback("map callback dropped".into()))?
            .map_err(|e| RenderError::Readback(e.to_string()))?;

        let mut out = Vec::with_capacity(unpadded_size);
        {
            let mapped = staging
                .slice(..)
                .get_mapped_range()
                .map_err(|e| RenderError::Readback(e.to_string()))?;
            for row in mapped.chunks_exact(padded_bytes_per_row as usize) {
                out.extend_from_slice(&row[..unpadded_bytes_per_row]);
            }
        }
        staging.unmap();
        Ok(out)
    }
}

/// Rounds a row of `width` 4-byte pixels up to `wgpu`'s copy alignment.
///
/// Returns `None` when the padded row does not fit in a `u32`, which
/// `wgpu`'s `bytes_per_row` requires.
#[must_use]
pub fn padded_bytes_per_row(width: u32) -> Option<u32> {
    let align = u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let bytes = u64::from(width) * u64::from(BYTES_PER_PIXEL);
    u32::try_from(bytes.div_ceil(align) * align).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_padding_rounds_up_to_256() {
        assert_eq!(padded_bytes_per_row(1), Some(256));
        assert_eq!(padded_bytes_per_row(64), Some(256));
        assert_eq!(padded_bytes_per_row(65), Some(512));
        assert_eq!(padded_bytes_per_row(37), Some(256));
    }

    #[test]
    fn only_the_four_8bit_formats_are_supported() {
        use wgpu::TextureFormat as F;
        for f in SUPPORTED_FORMATS {
            assert!(is_supported_format(f), "{f:?}");
        }
        // Same block size, wrong interpretation of the bytes.
        for f in [
            F::Rgba8Uint,
            F::Rgba8Sint,
            F::Rgba8Snorm,
            F::Rgb10a2Unorm,
            F::R32Float,
            F::Rg16Float,
            F::Depth32Float,
            F::Rgb9e5Ufloat,
            F::Rg11b10Ufloat,
        ] {
            assert!(!is_supported_format(f), "{f:?}");
        }
    }

    #[test]
    fn staging_size_is_padded_rows_times_height() {
        assert_eq!(staging_buffer_size(PhysicalSize::new(37, 5)), Some(256 * 5));
        assert_eq!(
            staging_buffer_size(PhysicalSize::new(16384, 16384)),
            Some(1 << 30)
        );
        assert_eq!(staging_buffer_size(PhysicalSize::new(u32::MAX, 1)), None);
    }

    #[test]
    fn row_padding_reports_overflow_instead_of_wrapping() {
        assert_eq!(padded_bytes_per_row(u32::MAX), None);
        assert_eq!(padded_bytes_per_row(1 << 30), None);
        assert_eq!(padded_bytes_per_row((1 << 30) - 64), Some(u32::MAX - 255));
    }
}
