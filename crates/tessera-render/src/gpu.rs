//! Instance, adapter, device and queue bootstrap.

use std::sync::Arc;

use crate::error::RenderError;

/// The GPU objects every surface and offscreen target shares.
///
/// Cloning is cheap: `wgpu` handles are reference counted. One `Gpu` per
/// process is the intended shape; every window's [`crate::RenderSurface`]
/// is created against it.
#[derive(Debug, Clone)]
pub struct Gpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

impl Gpu {
    /// The instance descriptor this crate uses.
    ///
    /// Frontends that must build a surface themselves (for example
    /// `tessera-ffi` wrapping a `CAMetalLayer` pointer through
    /// `wgpu::SurfaceTargetUnsafe`) create their instance with this so the
    /// surface and the [`Gpu`] agree on backends.
    #[must_use]
    pub fn instance_descriptor() -> wgpu::InstanceDescriptor {
        wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        }
    }

    /// Creates an instance with [`Self::instance_descriptor`].
    #[must_use]
    pub fn new_instance() -> wgpu::Instance {
        wgpu::Instance::new(Self::instance_descriptor())
    }

    /// The device limits requested from `adapter`.
    ///
    /// Starts from `wgpu`'s defaults and raises every resolution-related
    /// limit (texture dimensions, buffer sizes, ...) to what the adapter
    /// supports, so a Retina window larger than 2048 physical pixels on a
    /// side gets a full-size swapchain. Alignment limits stay at the
    /// defaults, which every Metal device satisfies.
    #[must_use]
    pub fn required_limits(adapter: &wgpu::Adapter) -> wgpu::Limits {
        wgpu::Limits::defaults().using_resolution(adapter.limits())
    }

    /// Brings up a GPU without any surface.
    ///
    /// This is what headless tests and offscreen rendering use. Blocks the
    /// calling thread while the adapter and device are requested.
    pub fn headless() -> Result<Self, RenderError> {
        Self::with_compatible_surface(Self::new_instance(), None)
    }

    /// Brings up a GPU on `instance`, preferring an adapter that can present
    /// to `surface` when one is given.
    ///
    /// GPU errors that no error scope captures are logged through
    /// `tracing` instead of panicking (which is `wgpu`'s default), so a
    /// validation problem inside a frame callback never unwinds through the
    /// frontend.
    pub fn with_compatible_surface(
        instance: wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
    ) -> Result<Self, RenderError> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: surface,
            ..Default::default()
        }))?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("tessera"),
                required_features: wgpu::Features::empty(),
                required_limits: Self::required_limits(&adapter),
                ..Default::default()
            }))?;
        device.on_uncaptured_error(Arc::new(|error: wgpu::Error| {
            tracing::error!(target: "tessera_render", %error, "uncaptured GPU error");
        }));
        Ok(Self {
            instance,
            adapter,
            device,
            queue,
        })
    }

    /// The instance surfaces are created from.
    #[must_use]
    pub fn instance(&self) -> &wgpu::Instance {
        &self.instance
    }

    /// The physical adapter in use.
    #[must_use]
    pub fn adapter(&self) -> &wgpu::Adapter {
        &self.adapter
    }

    /// The logical device.
    #[must_use]
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The submission queue.
    #[must_use]
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Largest width or height a 2D texture or surface may have.
    ///
    /// This is the device's limit, which [`Self::required_limits`] raises to
    /// the adapter's hardware maximum.
    #[must_use]
    pub fn max_texture_dimension(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }

    /// Largest buffer the device will allocate, in bytes.
    ///
    /// Readbacks stage the whole texture in one buffer, so this bounds the
    /// size of an [`crate::OffscreenTarget`].
    #[must_use]
    pub fn max_buffer_size(&self) -> u64 {
        self.device.limits().max_buffer_size
    }

    /// Runs `f` inside a validation error scope and returns any error it
    /// raised as [`RenderError::Gpu`].
    ///
    /// Error scopes are per thread, so independent threads sharing one
    /// `Gpu` may call this concurrently; `wgpu`'s `std` feature (enabled in
    /// this crate's `Cargo.toml`) is what makes the scope stack thread-local
    /// rather than process-global. `tests/headless_threads.rs` guards that.
    pub fn capture_errors<T>(&self, f: impl FnOnce() -> T) -> Result<T, RenderError> {
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let out = f();
        match pollster::block_on(scope.pop()) {
            None => Ok(out),
            Some(error) => Err(RenderError::Gpu(error.to_string())),
        }
    }

    /// Records and submits a render pass that clears `view` to `color`.
    ///
    /// `color` is linear and straight; it is written premultiplied
    /// ([`crate::Rgba::to_clear_color`]), like everything the renderer
    /// draws, and the GPU encodes it if the view's format is sRGB. A
    /// validation error (for example a view that is not renderable) is
    /// returned instead of panicking.
    pub fn clear_view(
        &self,
        view: &wgpu::TextureView,
        color: crate::Rgba,
    ) -> Result<(), RenderError> {
        self.capture_errors(|| {
            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("tessera.clear"),
                });
            {
                let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("tessera.clear"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(color.to_clear_color()),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
            }
            self.queue.submit(Some(encoder.finish()));
        })
    }
}
