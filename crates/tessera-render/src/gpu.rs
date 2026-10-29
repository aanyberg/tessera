//! Instance, adapter, device and queue bootstrap.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::RenderError;

/// The GPU objects every surface and offscreen target shares.
///
/// Cloning is cheap: `wgpu` handles are reference counted. One live `Gpu`
/// per process is the intended shape, replaced after a loss; every
/// window's [`crate::RenderSurface`] is created against it. Dropping the
/// last clone drops the queue, which waits for the GPU to go idle.
///
/// When `wgpu` loses the device ([`wgpu::Device::destroy`], or one of the
/// device errors `wgpu-hal` reports as a loss),
/// [`Self::is_lost`] reports `true` for this `Gpu` and every clone of it,
/// and nothing drawn on it is shown again; the owner drops it and brings up
/// a new one. A real device removal (an unplugged external GPU) is
/// probably not reported by `wgpu-hal`'s Metal backend 30.0.1, which never
/// checks a command buffer's completion error (see
/// [`crate::RenderError::DeviceLost`]).
#[derive(Debug, Clone)]
pub struct Gpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// Set by the device-lost callback; shared by every clone.
    lost: Arc<AtomicBool>,
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
    /// frontend. A device-lost callback is registered that makes
    /// [`Self::is_lost`] report `true` from then on.
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
        let lost = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&lost);
        device.set_device_lost_callback(move |reason, message| {
            flag.store(true, Ordering::Release);
            tracing::warn!(target: "tessera_render", ?reason, %message, "GPU device lost");
        });
        Ok(Self {
            instance,
            adapter,
            device,
            queue,
            lost,
        })
    }

    /// Whether `wgpu` has lost the device (see
    /// [`crate::RenderError::DeviceLost`] for what it reports as a loss).
    ///
    /// Once `true` it stays `true`: every later frame on this `Gpu` fails
    /// or shows nothing, so the owner drops it, and every surface and
    /// renderer created on it, and brings up a new one. Set by the
    /// device-lost callback `wgpu` runs when it notices the loss, which is
    /// at the latest the next submission or poll after it; this is one
    /// atomic load.
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    /// Destroys the device, as a GPU reset or an unplugged external GPU
    /// would, and waits until `wgpu` has run the device-lost callback, so
    /// [`Self::is_lost`] is `true` when this returns.
    ///
    /// `wgpu` loses a destroyed device only once its queue is empty, at
    /// the next poll; this polls until it has (or sets the flag itself
    /// after a second, in case a backend never reports the loss). Exists
    /// for fault-injection tests (feature `test-hooks`); never call it in
    /// production.
    ///
    /// Call it on the thread that draws with this `Gpu`: `wgpu-core`
    /// 30.0.1 deadlocks when the poll releases the lost device's resources
    /// (tracker lock, then snatch lock) while another thread writes to the
    /// queue (snatch lock, then tracker lock).
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn lose_device_for_test(&self) {
        self.device.destroy();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !self.is_lost() && std::time::Instant::now() < deadline {
            // A destroyed device may report an error here; the callback is
            // what matters.
            let _ = self.device.poll(wgpu::PollType::Poll);
            std::thread::yield_now();
        }
        if !self.is_lost() {
            tracing::warn!(target: "tessera_render", "the device-lost callback did not run");
            self.lost.store(true, Ordering::Release);
        }
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
    /// raised as [`RenderError::Gpu`], or [`RenderError::DeviceLost`] when
    /// the device is lost by the time the scope closes (whether or not `f`
    /// raised an error: work on a lost device does nothing).
    ///
    /// Error scopes are per thread, so independent threads sharing one
    /// `Gpu` may call this concurrently; `wgpu`'s `std` feature (enabled in
    /// this crate's `Cargo.toml`) is what makes the scope stack thread-local
    /// rather than process-global. `tests/headless_threads.rs` guards that.
    pub fn capture_errors<T>(&self, f: impl FnOnce() -> T) -> Result<T, RenderError> {
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let out = f();
        let error = pollster::block_on(scope.pop());
        if self.is_lost() {
            return Err(RenderError::DeviceLost);
        }
        match error {
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
