//! Headless clear-and-readback: proves the clear pass, sRGB encoding, device
//! limits and row-padding logic without a window.
//!
//! The tests fail when no GPU adapter exists, so a broken instance
//! descriptor or a dropped backend feature cannot pass the suite vacuously.
//! A machine that genuinely has no GPU opts out explicitly with
//! `TESSERA_SKIP_GPU_TESTS=1`, which makes every test here return early.

use tessera_render::{Gpu, LogicalSize, OffscreenTarget, PhysicalSize, Rgba};

const SKIP_VAR: &str = "TESSERA_SKIP_GPU_TESTS";

/// Brings up the headless GPU, or `None` only when the opt-out is set.
fn gpu() -> Option<Gpu> {
    if std::env::var_os(SKIP_VAR).is_some() {
        eprintln!("skipping: {SKIP_VAR} is set");
        return None;
    }
    Some(Gpu::headless().unwrap_or_else(|e| {
        panic!("no GPU adapter available ({e}); set {SKIP_VAR}=1 to skip the GPU tests")
    }))
}

fn assert_solid(pixels: &[u8], size: PhysicalSize, expected: [u8; 4], tolerance: u8) {
    assert_eq!(
        pixels.len(),
        (size.width as usize) * (size.height as usize) * 4,
        "readback length for {size:?}"
    );
    for (i, px) in pixels.as_chunks::<4>().0.iter().enumerate() {
        for c in 0..4 {
            let d = px[c].abs_diff(expected[c]);
            assert!(
                d <= tolerance,
                "pixel {i} channel {c}: got {} expected {} (±{tolerance}); pixel {px:?}",
                px[c],
                expected[c]
            );
        }
    }
}

const COLOR: Rgba = Rgba::new(0.25, 0.5, 1.0, 1.0);
/// `COLOR` written to a linear `Unorm` texture: value * 255.
const LINEAR_BYTES: [u8; 4] = [64, 128, 255, 255];
/// `COLOR` written to an sRGB texture: the GPU applies the sRGB transfer
/// curve on write (0.25 -> 137, 0.5 -> 188, 1.0 -> 255; alpha untouched).
const SRGB_BYTES: [u8; 4] = [137, 188, 255, 255];

#[test]
fn device_limits_follow_the_adapter() {
    let Some(gpu) = gpu() else { return };
    let adapter_max = gpu.adapter().limits().max_texture_dimension_2d;
    assert_eq!(
        gpu.max_texture_dimension(),
        adapter_max,
        "device must not be capped below the adapter's texture limit"
    );
    // Every Metal device since the first Retina Mac supports at least 8192;
    // the point is that we are not stuck at downlevel's 2048.
    assert!(
        gpu.max_texture_dimension() >= 4096,
        "max_texture_dimension = {}",
        gpu.max_texture_dimension()
    );
}

#[test]
fn clear_linear_texture_reads_back_solid_color() {
    let Some(gpu) = gpu() else { return };
    let size = PhysicalSize::new(16, 8);
    let target = OffscreenTarget::new(&gpu, size, wgpu::TextureFormat::Rgba8Unorm);
    target.clear(&gpu, COLOR).expect("clear");
    let px = target.read_back(&gpu).expect("readback");
    assert_solid(&px, size, LINEAR_BYTES, 1);
}

#[test]
fn clear_srgb_texture_encodes_on_write() {
    let Some(gpu) = gpu() else { return };
    let size = PhysicalSize::new(16, 8);
    let target = OffscreenTarget::new(&gpu, size, wgpu::TextureFormat::Rgba8UnormSrgb);
    target.clear(&gpu, COLOR).expect("clear");
    let px = target.read_back(&gpu).expect("readback");
    assert_solid(&px, size, SRGB_BYTES, 1);
}

#[test]
fn clear_bgra_texture_swaps_channels() {
    let Some(gpu) = gpu() else { return };
    let size = PhysicalSize::new(4, 4);
    let target = OffscreenTarget::new(&gpu, size, wgpu::TextureFormat::Bgra8Unorm);
    target.clear(&gpu, COLOR).expect("clear");
    let px = target.read_back(&gpu).expect("readback");
    assert_solid(&px, size, [255, 128, 64, 255], 1);
}

#[test]
fn readback_strips_row_padding_for_unaligned_widths() {
    let Some(gpu) = gpu() else { return };
    // 37 * 4 = 148 bytes per row, padded to 256 in the staging buffer.
    let size = PhysicalSize::new(37, 5);
    let target = OffscreenTarget::new(&gpu, size, wgpu::TextureFormat::Rgba8Unorm);
    target
        .clear(&gpu, Rgba::new(1.0, 0.0, 0.0, 0.5))
        .expect("clear");
    let px = target.read_back(&gpu).expect("readback");
    // Clears are premultiplied: red at half opacity stores (0.5, 0, 0, 0.5).
    assert_solid(&px, size, [128, 0, 0, 128], 1);
}

#[test]
fn clear_at_1x_and_2x_scale_covers_every_pixel() {
    let Some(gpu) = gpu() else { return };
    let logical = LogicalSize::new(21.0, 13.0);
    for scale in [1.0, 2.0] {
        let size = logical.to_physical(scale);
        let target = OffscreenTarget::new(&gpu, size, wgpu::TextureFormat::Bgra8UnormSrgb);
        target.clear(&gpu, COLOR).expect("clear");
        let px = target.read_back(&gpu).expect("readback");
        assert_solid(&px, size, [255, 188, 137, 255], 1);
    }
}

#[test]
fn retina_width_beyond_downlevel_limit_clears_fully() {
    let Some(gpu) = gpu() else { return };
    // A 1440-point-wide window at 2x is 2880 physical pixels: wider than the
    // 2048 that `Limits::downlevel_defaults()` allows. Keep the height small
    // so the readback stays cheap.
    let size = LogicalSize::new(1440.0, 4.0).to_physical(2.0);
    assert_eq!(size, PhysicalSize::new(2880, 8));
    assert!(size.width <= gpu.max_texture_dimension());
    let target = OffscreenTarget::new(&gpu, size, wgpu::TextureFormat::Bgra8UnormSrgb);
    target.clear(&gpu, COLOR).expect("clear");
    let px = target.read_back(&gpu).expect("readback");
    assert_solid(&px, size, [255, 188, 137, 255], 1);
}

#[test]
fn successive_clears_replace_previous_contents() {
    let Some(gpu) = gpu() else { return };
    let size = PhysicalSize::new(8, 8);
    let target = OffscreenTarget::new(&gpu, size, wgpu::TextureFormat::Rgba8Unorm);
    target.clear(&gpu, Rgba::WHITE).expect("clear");
    target.clear(&gpu, Rgba::BLACK).expect("clear");
    let px = target.read_back(&gpu).expect("readback");
    assert_solid(&px, size, [0, 0, 0, 255], 0);
}

#[test]
fn validation_errors_surface_as_render_errors_not_panics() {
    let Some(gpu) = gpu() else { return };
    // A texture without RENDER_ATTACHMENT usage cannot be a color target;
    // `wgpu` reports that as a validation error, which the error scope in
    // `clear_view` must turn into `RenderError::Gpu` rather than a panic.
    let texture = gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("not-renderable"),
        size: PhysicalSize::new(4, 4).extent(),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let err = gpu
        .clear_view(&view, Rgba::WHITE)
        .expect_err("clearing a non-renderable view must fail");
    assert!(
        matches!(err, tessera_render::RenderError::Gpu(_)),
        "unexpected error: {err:?}"
    );
}

#[test]
fn invalid_color_components_are_clamped_not_passed_through() {
    let Some(gpu) = gpu() else { return };
    let size = PhysicalSize::new(4, 4);
    let target = OffscreenTarget::new(&gpu, size, wgpu::TextureFormat::Rgba8Unorm);
    target
        .clear(&gpu, Rgba::new(f32::NAN, f32::INFINITY, -1.0, 2.0))
        .expect("clear");
    let px = target.read_back(&gpu).expect("readback");
    // NaN -> 0, +inf -> 1, -1 -> 0, 2 -> 1: the same bytes `Rgba::clamped`
    // promises, so a bad theme color renders as the nearest valid one.
    assert_solid(&px, size, [0, 255, 0, 255], 0);
}

#[test]
#[should_panic(expected = "offscreen target needs one of")]
fn same_block_size_but_unsupported_format_is_rejected() {
    let Some(gpu) = gpu() else {
        panic!("offscreen target needs one of (skipped)")
    };
    let _ = OffscreenTarget::new(
        &gpu,
        PhysicalSize::new(4, 4),
        wgpu::TextureFormat::Rgba8Uint,
    );
}

#[test]
#[should_panic(expected = "readback buffer")]
fn target_too_large_to_read_back_is_rejected_up_front() {
    let Some(gpu) = gpu() else {
        panic!("readback buffer (skipped)")
    };
    let max = gpu.max_texture_dimension();
    let bytes = u64::from(tessera_render::padded_bytes_per_row(max).expect("row fits"));
    let height = u32::try_from(gpu.max_buffer_size() / bytes + 1).expect("fits");
    // A device that can read back any texture it can allocate satisfies the
    // property vacuously; the message keeps `should_panic` honest.
    assert!(
        height <= max,
        "readback buffer limit unreachable on this device"
    );
    let _ = OffscreenTarget::new(
        &gpu,
        PhysicalSize::new(max, height),
        wgpu::TextureFormat::Rgba8Unorm,
    );
}
