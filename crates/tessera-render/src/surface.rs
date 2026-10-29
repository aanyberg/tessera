//! A presentable surface: configuration, resize, `HiDPI` scale and present mode.

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use crate::error::RenderError;
use crate::frame::FrameInput;
use crate::gpu::Gpu;
use crate::renderer::Renderer;
use crate::size::{LogicalSize, PhysicalSize, sanitize_scale};
use crate::theme::Rgba;

/// Frontend-chosen presentation knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceOptions {
    /// Synchronise presentation with the display's refresh (`Fifo`), or
    /// present as soon as a frame is ready (`AutoNoVsync`: mailbox or
    /// immediate, whichever the platform offers).
    pub vsync: bool,
    /// Prefer an sRGB-encoded swapchain format so linear colors are encoded
    /// by the GPU on write. When the adapter offers none, the linear twin is
    /// used and [`RenderSurface::is_srgb`] reports `false`.
    pub prefer_srgb: bool,
    /// Ask for a swapchain that composites with per-pixel alpha, for
    /// translucent terminal backgrounds. Falls back to opaque when the
    /// surface does not support it.
    pub transparent: bool,
}

impl Default for SurfaceOptions {
    fn default() -> Self {
        Self {
            vsync: true,
            prefer_srgb: true,
            transparent: false,
        }
    }
}

/// What happened when a frame was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum FrameStatus {
    /// The frame was drawn and queued for presentation.
    Presented,
    /// The frame was drawn and presented, but the swapchain no longer
    /// matches the window. Nothing was reconfigured: the caller should call
    /// [`RenderSurface::resize`] with the view's current size and scale
    /// before the next frame.
    Suboptimal,
    /// Nothing was drawn by this call. [`RenderSurface::render`] returns it
    /// when the surface had nothing to draw into (zero-sized, occluded, or
    /// no drawable). `tessera-app-core`'s views also return it when nothing
    /// changed since the last presented frame, and when the frame came
    /// sooner than their 240-frames-per-second limit allows (paced); the
    /// three are not told apart yet. A host that renders from a display
    /// link of up to 240 Hz is never paced and need not retry: its next
    /// tick draws whatever is pending. A host that renders faster, or from
    /// output notifications, must schedule another render after a skip,
    /// since it cannot tell a paced skip from the others.
    Skipped,
}

/// A window-backed swapchain plus the configuration that drives it.
///
/// Created once per terminal view, resized by the frontend whenever the
/// view's size or backing scale changes, and drawn into once per display
/// link tick.
#[derive(Debug)]
pub struct RenderSurface {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    options: SurfaceOptions,
    scale: f32,
    /// `true` once `config` has been applied with a non-empty size.
    configured: bool,
    /// Transparency asked for by [`Self::request_transparent`], applied
    /// at the start of the next frame.
    pending_transparent: Option<bool>,
}

impl RenderSurface {
    /// Creates a surface from a window handle and brings up a [`Gpu`] that
    /// can present to it.
    ///
    /// This is the safe path for the winit dev window and any frontend that
    /// implements `raw-window-handle`. `size` is the view's current size in
    /// physical pixels and `scale` its backing scale factor.
    ///
    /// # Panics
    ///
    /// `wgpu`'s Metal backend requires surface creation on the main thread.
    pub fn from_window<W>(
        window: W,
        size: PhysicalSize,
        scale: f32,
        options: SurfaceOptions,
    ) -> Result<(Gpu, Self), RenderError>
    where
        W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
    {
        let instance = Gpu::new_instance();
        let surface = instance.create_surface(window)?;
        Self::from_wgpu_surface(instance, surface, size, scale, options)
    }

    /// Wraps a surface the frontend already created on `instance` (which
    /// must come from [`Gpu::new_instance`]) and brings up a compatible
    /// [`Gpu`].
    ///
    /// This is the path for `tessera-ffi`, which turns a `CAMetalLayer`
    /// pointer into a `wgpu::Surface` with its own `unsafe` block.
    pub fn from_wgpu_surface(
        instance: wgpu::Instance,
        surface: wgpu::Surface<'static>,
        size: PhysicalSize,
        scale: f32,
        options: SurfaceOptions,
    ) -> Result<(Gpu, Self), RenderError> {
        let gpu = Gpu::with_compatible_surface(instance, Some(&surface))?;
        let this = Self::attach(&gpu, surface, size, scale, options)?;
        Ok((gpu, this))
    }

    /// Attaches an already-created surface to an existing [`Gpu`], for a
    /// second window sharing the process's device.
    pub fn attach(
        gpu: &Gpu,
        surface: wgpu::Surface<'static>,
        size: PhysicalSize,
        scale: f32,
        options: SurfaceOptions,
    ) -> Result<Self, RenderError> {
        let caps = surface.get_capabilities(gpu.adapter());
        let size = size.clamped_to(gpu.max_texture_dimension());
        let config = build_config(&caps, size, options).ok_or(RenderError::UnsupportedSurface)?;
        let mut this = Self {
            surface,
            config,
            options,
            scale: sanitize_scale(scale),
            configured: false,
            pending_transparent: None,
        };
        this.apply(gpu);
        Ok(this)
    }

    fn apply(&mut self, gpu: &Gpu) {
        if PhysicalSize::new(self.config.width, self.config.height).is_empty() {
            self.configured = false;
        } else {
            self.surface.configure(gpu.device(), &self.config);
            self.configured = true;
        }
    }

    /// Reconfigures for a new physical size and backing scale.
    ///
    /// Call from the frontend's layout / backing-properties-changed hooks.
    /// A zero-sized surface suspends drawing until the next non-empty
    /// resize. Unchanged values are a no-op.
    pub fn resize(&mut self, gpu: &Gpu, size: PhysicalSize, scale: f32) {
        let size = size.clamped_to(gpu.max_texture_dimension());
        let scale = sanitize_scale(scale);
        let changed = size.width != self.config.width
            || size.height != self.config.height
            || !self.configured;
        self.scale = scale;
        if changed {
            self.config.width = size.width;
            self.config.height = size.height;
            self.apply(gpu);
        }
    }

    /// Convenience for frontends that have logical points and a scale.
    pub fn resize_logical(&mut self, gpu: &Gpu, size: LogicalSize, scale: f32) {
        self.resize(gpu, size.to_physical(scale), scale);
    }

    /// Switches presentation knobs and reconfigures.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::UnsupportedSurface`] when the adapter no longer
    /// lists any format for this surface; the previous configuration stays
    /// in effect.
    pub fn set_options(&mut self, gpu: &Gpu, options: SurfaceOptions) -> Result<(), RenderError> {
        if options == self.options {
            return Ok(());
        }
        let caps = self.surface.get_capabilities(gpu.adapter());
        let size = PhysicalSize::new(self.config.width, self.config.height);
        let config = build_config(&caps, size, options).ok_or(RenderError::UnsupportedSurface)?;
        self.config = config;
        self.options = options;
        self.apply(gpu);
        Ok(())
    }

    /// Switches between a swapchain that composites with per-pixel alpha
    /// (`transparent`) and an opaque one, and reconfigures now: the alpha
    /// mode becomes [`choose_alpha_mode`] of the surface's capabilities
    /// (so it stays opaque where the surface lists no per-pixel-alpha
    /// mode). On Metal this also sets the layer's `opaque` property.
    /// Unchanged values are a no-op. Must be called on the main thread
    /// like every reconfiguration; see [`Self::request_transparent`] for
    /// other threads.
    ///
    /// # Errors
    ///
    /// As [`Self::set_options`].
    pub fn set_transparent(&mut self, gpu: &Gpu, transparent: bool) -> Result<(), RenderError> {
        self.pending_transparent = None;
        self.set_options(
            gpu,
            SurfaceOptions {
                transparent,
                ..self.options
            },
        )
    }

    /// Asks for [`Self::set_transparent`] at the start of the next frame
    /// ([`Self::render`] or [`Self::clear`]) instead of now, so a change
    /// decided on another thread reconfigures the layer on the thread that
    /// draws. A later request replaces an earlier one; a request equal to
    /// the options in effect does nothing.
    pub fn request_transparent(&mut self, transparent: bool) {
        self.pending_transparent = Some(transparent);
    }

    /// The compositing modes the adapter lists for this surface; a
    /// per-pixel-alpha one among them is what [`Self::set_transparent`]
    /// switches to.
    #[must_use]
    pub fn supported_alpha_modes(&self, gpu: &Gpu) -> Vec<wgpu::CompositeAlphaMode> {
        self.surface.get_capabilities(gpu.adapter()).alpha_modes
    }

    /// The transparency the next frame will use: the pending request, or
    /// the options in effect.
    #[must_use]
    pub fn wants_transparent(&self) -> bool {
        self.pending_transparent.unwrap_or(self.options.transparent)
    }

    /// Current size in physical pixels.
    #[must_use]
    pub fn size(&self) -> PhysicalSize {
        PhysicalSize::new(self.config.width, self.config.height)
    }

    /// Current size in logical points.
    #[must_use]
    pub fn logical_size(&self) -> LogicalSize {
        self.size().to_logical(self.scale)
    }

    /// Backing scale factor (1.0 on a standard display, 2.0 on Retina).
    #[must_use]
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// The options in effect.
    #[must_use]
    pub fn options(&self) -> SurfaceOptions {
        self.options
    }

    /// Swapchain texture format.
    #[must_use]
    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// `true` when the swapchain format is sRGB-encoded.
    #[must_use]
    pub fn is_srgb(&self) -> bool {
        self.config.format.is_srgb()
    }

    /// Present mode in effect.
    #[must_use]
    pub fn present_mode(&self) -> wgpu::PresentMode {
        self.config.present_mode
    }

    /// Alpha compositing mode in effect.
    #[must_use]
    pub fn alpha_mode(&self) -> wgpu::CompositeAlphaMode {
        self.config.alpha_mode
    }

    /// The full configuration, for frontends that need to inspect it.
    #[must_use]
    pub fn config(&self) -> &wgpu::SurfaceConfiguration {
        &self.config
    }

    /// Clears the next swapchain image to `color` (written premultiplied,
    /// see [`Gpu::clear_view`]) and presents it.
    ///
    /// This is the frontend-facing clear pass, for frames with nothing to
    /// draw yet; [`Self::render`] draws the grid. An outdated or lost
    /// swapchain is reconfigured and retried once; a suboptimal one is
    /// still presented and reported as [`FrameStatus::Suboptimal`] so the
    /// caller can resize.
    pub fn clear(&mut self, gpu: &Gpu, color: Rgba) -> Result<FrameStatus, RenderError> {
        self.present_with(gpu, |gpu, view| gpu.clear_view(view, color))
    }

    /// Draws `frame` with `renderer` into the next swapchain image and
    /// presents it.
    ///
    /// The frontend calls this from its display-link callback. Swapchain
    /// handling is the same as for [`Self::clear`]. The renderer's backing
    /// scale should match [`Self::scale`] (see
    /// [`Renderer::set_font_size`]).
    ///
    /// # Errors
    ///
    /// [`RenderError::SurfaceLost`] when the swapchain cannot be
    /// recovered, and whatever [`Renderer::render`] returns; the acquired
    /// image is then dropped without being presented.
    pub fn render(
        &mut self,
        gpu: &Gpu,
        renderer: &mut Renderer,
        frame: &FrameInput<'_>,
    ) -> Result<FrameStatus, RenderError> {
        let (format, size) = (self.format(), self.size());
        self.present_with(gpu, |gpu, view| {
            renderer.render(gpu, view, format, size, frame)
        })
    }

    /// Acquires the next swapchain image, runs `draw` on it and presents
    /// it.
    fn present_with(
        &mut self,
        gpu: &Gpu,
        draw: impl FnOnce(&Gpu, &wgpu::TextureView) -> Result<(), RenderError>,
    ) -> Result<FrameStatus, RenderError> {
        let Some((frame, suboptimal)) = self.acquire(gpu)? else {
            return Ok(FrameStatus::Skipped);
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        draw(gpu, &view)?;
        gpu.queue().present(frame);
        Ok(if suboptimal {
            FrameStatus::Suboptimal
        } else {
            FrameStatus::Presented
        })
    }

    /// The next swapchain image and whether it is suboptimal, or `None`
    /// when the frame should be skipped.
    fn acquire(&mut self, gpu: &Gpu) -> Result<Option<(wgpu::SurfaceTexture, bool)>, RenderError> {
        if let Some(transparent) = self.pending_transparent.take()
            && let Err(error) = self.set_transparent(gpu, transparent)
        {
            // The previous configuration stays in effect; the frame is
            // still drawn, just not with the requested transparency.
            tracing::warn!(target: "tessera_render", %error, transparent, "switching the surface's transparency failed");
        }
        if !self.configured {
            return Ok(None);
        }
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) => (t, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(t) => (t, true),
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(None);
            }
            wgpu::CurrentSurfaceTexture::Outdated
            | wgpu::CurrentSurfaceTexture::Lost
            | wgpu::CurrentSurfaceTexture::Validation => {
                self.apply(gpu);
                match self.surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(t) => (t, false),
                    wgpu::CurrentSurfaceTexture::Suboptimal(t) => (t, true),
                    wgpu::CurrentSurfaceTexture::Timeout
                    | wgpu::CurrentSurfaceTexture::Occluded => return Ok(None),
                    wgpu::CurrentSurfaceTexture::Outdated
                    | wgpu::CurrentSurfaceTexture::Lost
                    | wgpu::CurrentSurfaceTexture::Validation => {
                        return Err(RenderError::SurfaceLost);
                    }
                }
            }
        };
        Ok(Some(frame))
    }
}

/// Picks a swapchain format from what the adapter offers.
///
/// Prefers the sRGB variant when `prefer_srgb` and the adapter lists one,
/// then the first non-sRGB format, then whatever comes first.
#[must_use]
pub fn choose_format(
    formats: &[wgpu::TextureFormat],
    prefer_srgb: bool,
) -> Option<wgpu::TextureFormat> {
    let first = *formats.first()?;
    let srgb = formats.iter().copied().find(wgpu::TextureFormat::is_srgb);
    let linear = formats.iter().copied().find(|f| !f.is_srgb());
    Some(if prefer_srgb {
        srgb.or(linear).unwrap_or(first)
    } else {
        linear.or(srgb).unwrap_or(first)
    })
}

/// Picks a present mode: `Fifo` for vsync, otherwise the lowest-latency mode
/// the adapter lists (`Mailbox`, then `Immediate`), falling back to
/// `AutoNoVsync`, which `wgpu` guarantees to accept.
#[must_use]
pub fn choose_present_mode(modes: &[wgpu::PresentMode], vsync: bool) -> wgpu::PresentMode {
    if vsync {
        return if modes.contains(&wgpu::PresentMode::Fifo) {
            wgpu::PresentMode::Fifo
        } else {
            wgpu::PresentMode::AutoVsync
        };
    }
    [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
        .into_iter()
        .find(|m| modes.contains(m))
        .unwrap_or(wgpu::PresentMode::AutoNoVsync)
}

/// Picks an alpha mode: a per-pixel-alpha mode when `transparent` and the
/// surface lists one, otherwise `Opaque` if listed, otherwise `Auto`.
#[must_use]
pub fn choose_alpha_mode(
    modes: &[wgpu::CompositeAlphaMode],
    transparent: bool,
) -> wgpu::CompositeAlphaMode {
    use wgpu::CompositeAlphaMode as M;
    if transparent
        && let Some(m) = [M::PreMultiplied, M::PostMultiplied, M::Inherit]
            .into_iter()
            .find(|m| modes.contains(m))
    {
        return m;
    }
    if modes.contains(&M::Opaque) {
        M::Opaque
    } else {
        M::Auto
    }
}

/// Builds a full configuration from the adapter's capabilities.
///
/// Returns `None` when the adapter lists no format at all.
#[must_use]
pub fn build_config(
    caps: &wgpu::SurfaceCapabilities,
    size: PhysicalSize,
    options: SurfaceOptions,
) -> Option<wgpu::SurfaceConfiguration> {
    let format = choose_format(&caps.formats, options.prefer_srgb)?;
    Some(wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        color_space: wgpu::SurfaceColorSpace::Auto,
        width: size.width,
        height: size.height,
        present_mode: choose_present_mode(&caps.present_modes, options.vsync),
        desired_maximum_frame_latency: 2,
        alpha_mode: choose_alpha_mode(&caps.alpha_modes, options.transparent),
        view_formats: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wgpu::{CompositeAlphaMode as A, PresentMode as P, TextureFormat as F};

    #[test]
    fn format_prefers_srgb_when_asked() {
        let formats = [F::Bgra8Unorm, F::Bgra8UnormSrgb, F::Rgba16Float];
        assert_eq!(choose_format(&formats, true), Some(F::Bgra8UnormSrgb));
        assert_eq!(choose_format(&formats, false), Some(F::Bgra8Unorm));
    }

    #[test]
    fn format_falls_back_when_preference_is_unavailable() {
        assert_eq!(choose_format(&[F::Bgra8Unorm], true), Some(F::Bgra8Unorm));
        assert_eq!(
            choose_format(&[F::Bgra8UnormSrgb], false),
            Some(F::Bgra8UnormSrgb)
        );
        assert_eq!(choose_format(&[], true), None);
    }

    #[test]
    fn present_mode_tracks_vsync() {
        let metal = [P::Fifo, P::Immediate];
        assert_eq!(choose_present_mode(&metal, true), P::Fifo);
        assert_eq!(choose_present_mode(&metal, false), P::Immediate);
        assert_eq!(
            choose_present_mode(&[P::Fifo, P::Mailbox], false),
            P::Mailbox
        );
        assert_eq!(choose_present_mode(&[], true), P::AutoVsync);
        assert_eq!(choose_present_mode(&[], false), P::AutoNoVsync);
    }

    #[test]
    fn alpha_mode_honours_transparency_when_available() {
        let metal = [A::Opaque, A::PostMultiplied];
        assert_eq!(choose_alpha_mode(&metal, false), A::Opaque);
        assert_eq!(choose_alpha_mode(&metal, true), A::PostMultiplied);
        assert_eq!(choose_alpha_mode(&[A::Opaque], true), A::Opaque);
        assert_eq!(choose_alpha_mode(&[], true), A::Auto);
    }
}
