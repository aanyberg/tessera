//! Concurrent use of one shared `Gpu` from several threads.
//!
//! `Gpu::capture_errors` relies on `wgpu`'s error-scope stack being
//! thread-local, which `wgpu` only provides with its `std` feature. Without
//! it the stack is process-global and interleaved push/pop pairs from two
//! threads panic inside `wgpu` ("error scopes must be popped in reverse
//! order"). This test pins the feature so it cannot silently regress.

use std::thread;

use tessera_render::{Gpu, OffscreenTarget, PhysicalSize, Rgba};

const SKIP_VAR: &str = "TESSERA_SKIP_GPU_TESTS";
const THREADS: usize = 8;
const ROUNDS: usize = 300;

fn gpu() -> Option<Gpu> {
    if std::env::var_os(SKIP_VAR).is_some() {
        eprintln!("skipping: {SKIP_VAR} is set");
        return None;
    }
    Some(Gpu::headless().unwrap_or_else(|e| {
        panic!("no GPU adapter available ({e}); set {SKIP_VAR}=1 to skip the GPU tests")
    }))
}

#[test]
fn concurrent_clears_from_many_threads_do_not_panic() {
    let Some(gpu) = gpu() else { return };

    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let gpu = gpu.clone();
            thread::spawn(move || {
                let target = OffscreenTarget::new(
                    &gpu,
                    PhysicalSize::new(8, 8),
                    wgpu::TextureFormat::Rgba8Unorm,
                );
                for _ in 0..ROUNDS {
                    target.clear(&gpu, Rgba::WHITE).expect("clear");
                }
                // Each thread ends on its own color so a cross-thread mix-up
                // of error scopes or submissions would show in the readback.
                #[allow(clippy::cast_precision_loss)]
                let shade = i as f32 / (THREADS - 1) as f32;
                target
                    .clear(&gpu, Rgba::new(shade, shade, shade, 1.0))
                    .expect("clear");
                let px = target.read_back(&gpu).expect("readback");
                (i, shade, px)
            })
        })
        .collect();

    for h in handles {
        let (i, shade, px) = h.join().expect("worker thread panicked");
        assert_eq!(px.len(), 8 * 8 * 4, "thread {i}");
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let expected = (shade * 255.0).round() as u8;
        for (n, p) in px.as_chunks::<4>().0.iter().enumerate() {
            for c in 0..3 {
                assert!(
                    p[c].abs_diff(expected) <= 1,
                    "thread {i} pixel {n}: got {p:?}, expected {expected}"
                );
            }
            assert_eq!(p[3], 255, "thread {i} pixel {n} alpha");
        }
    }
}

#[test]
fn validation_errors_stay_on_their_own_thread() {
    let Some(gpu) = gpu() else { return };
    // One thread keeps clearing a valid target while another repeatedly
    // triggers a validation error. With thread-local scopes the error must
    // land only on the thread that caused it.
    let good = {
        let gpu = gpu.clone();
        thread::spawn(move || {
            let target = OffscreenTarget::new(
                &gpu,
                PhysicalSize::new(4, 4),
                wgpu::TextureFormat::Rgba8Unorm,
            );
            for _ in 0..200 {
                target
                    .clear(&gpu, Rgba::BLACK)
                    .expect("valid clear must not see another thread's error");
            }
        })
    };
    let bad = {
        let gpu = gpu.clone();
        thread::spawn(move || {
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
            for _ in 0..200 {
                let err = gpu
                    .clear_view(&view, Rgba::WHITE)
                    .expect_err("non-renderable view must fail on its own thread");
                assert!(matches!(err, tessera_render::RenderError::Gpu(_)));
            }
        })
    };
    good.join().expect("good thread panicked");
    bad.join().expect("bad thread panicked");
}
