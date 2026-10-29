//! Device loss: `Gpu::is_lost`, `RenderError::DeviceLost` and the
//! `test-hooks` fault injection (`Gpu::lose_device_for_test`).
//!
//! A destroyed device must be reported as lost by every clone of its
//! `Gpu`, every later GPU call must fail with `DeviceLost` (never panic or
//! hang), and other `Gpu`s must be unaffected. `TESSERA_SKIP_GPU_TESTS=1`
//! makes every test return early, like the other GPU suites.

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tessera_grid::{Cell, Grid};
use tessera_render::{
    FontConfig, FrameInput, Gpu, OffscreenTarget, PhysicalSize, RenderError, Renderer, Rgba,
    Shaper, Theme,
};

const SKIP_VAR: &str = "TESSERA_SKIP_GPU_TESTS";
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
/// How long any single call on a lost device may take before the test
/// calls it a hang.
const HANG: Duration = Duration::from_secs(10);

fn gpu() -> Option<Gpu> {
    if std::env::var_os(SKIP_VAR).is_some() {
        eprintln!("skipping: {SKIP_VAR} is set");
        return None;
    }
    Some(Gpu::headless().unwrap_or_else(|e| {
        panic!("no GPU adapter available ({e}); set {SKIP_VAR}=1 to skip the GPU tests")
    }))
}

fn menlo() -> Shaper {
    Shaper::new(&FontConfig {
        family: "Menlo".into(),
        size_pt: 13.0,
        scale: 1.0,
        fallback: Vec::new(),
    })
    .expect("Menlo ships with macOS")
}

/// Runs `f` on another thread and fails the test if it takes longer than
/// [`HANG`] (a wait on a lost device that never completes).
fn within_deadline<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(HANG)
        .unwrap_or_else(|_| panic!("{what} did not return within {HANG:?} on a lost device"))
}

/// How long one test may take before [`watchdog`] fails it.
const LIMIT: Duration = Duration::from_secs(60);

/// Runs `f` on its own thread and fails the test if it has not returned
/// within `limit`: a GPU call blocked inside `wgpu` (a deadlock, a wait on
/// a lost device) never reaches an in-test deadline, and would hang the
/// suite. On a timeout it records a stack sample of the whole process
/// (macOS `sample`) and names the file in the failure.
fn watchdog(f: impl FnOnce() + Send + 'static) {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    match rx.recv_timeout(LIMIT) {
        Ok(()) => worker.join().expect("finished"),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            std::panic::resume_unwind(worker.join().expect_err("panicked"));
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let pid = std::process::id();
            let file = std::env::temp_dir().join(format!("tessera-hang-{pid}.txt"));
            let sampled = std::process::Command::new("/usr/bin/sample")
                .args([pid.to_string().as_str(), "2", "-file"])
                .arg(&file)
                .output()
                .is_ok_and(|out| out.status.success());
            let dump = if sampled {
                format!("stack sample in {}", file.display())
            } else {
                "no stack sample (`sample` failed)".to_owned()
            };
            panic!("hung for over {LIMIT:?} inside a GPU call; {dump}");
        }
    }
}

fn is_device_lost<T: std::fmt::Debug>(result: &Result<T, RenderError>) -> bool {
    matches!(result, Err(RenderError::DeviceLost))
}

#[test]
fn a_fresh_gpu_is_not_lost() {
    watchdog(|| {
        let Some(gpu) = gpu() else { return };
        assert!(!gpu.is_lost());
        gpu.capture_errors(|| ()).expect("an empty scope succeeds");
    });
}

#[test]
fn the_hook_loses_the_device_for_every_clone_and_only_that_gpu() {
    watchdog(|| {
        let Some(gpu) = gpu() else { return };
        let other = Gpu::headless().expect("a second device");
        let clone = gpu.clone();

        gpu.lose_device_for_test();

        assert!(gpu.is_lost());
        assert!(
            clone.is_lost(),
            "a clone made before the loss shares the flag"
        );
        assert!(gpu.clone().is_lost(), "a clone made after the loss too");
        assert!(!other.is_lost(), "another device is unaffected");
        assert!(is_device_lost(&gpu.capture_errors(|| ())));
        // Losing an already lost device is harmless.
        gpu.lose_device_for_test();
        assert!(gpu.is_lost());

        // The other device still draws.
        let target = OffscreenTarget::new(&other, PhysicalSize::new(4, 4), FORMAT);
        target
            .clear(&other, Rgba::WHITE)
            .expect("clear on the other device");
        let px = target
            .read_back(&other)
            .expect("readback on the other device");
        assert!(px.iter().all(|&c| c == 255));
    });
}

/// The real path: `wgpu` runs the device-lost callback itself, with no help
/// from the hook's fallback.
#[test]
fn wgpu_runs_the_device_lost_callback() {
    watchdog(|| {
        let Some(gpu) = gpu() else { return };
        gpu.device().destroy();
        let deadline = Instant::now() + HANG;
        while !gpu.is_lost() && Instant::now() < deadline {
            let _ = gpu.device().poll(wgpu::PollType::Poll);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(gpu.is_lost(), "wgpu never ran the device-lost callback");
    });
}

#[test]
fn every_call_on_a_lost_device_fails_with_device_lost_without_panicking() {
    watchdog(|| {
        let Some(gpu) = gpu() else { return };
        let size = PhysicalSize::new(64, 32);
        let target = OffscreenTarget::new(&gpu, size, FORMAT);
        let mut renderer = Renderer::new(&gpu, Theme::default(), menlo()).expect("renderer");
        let grid = {
            let mut grid = Grid::new(2, 8);
            grid.put_str("lost");
            grid
        };
        let rows: Vec<&[Cell]> = grid
            .visible_lines()
            .iter()
            .map(|l| l.cells.as_slice())
            .collect();
        target
            .render(&gpu, &mut renderer, &FrameInput::new(&rows))
            .expect("a frame before the loss");

        gpu.lose_device_for_test();

        // Damage-free frames, frames with damage and new resources: all fail
        // the same way, and none hangs.
        for _ in 0..3 {
            let result = target.render(&gpu, &mut renderer, &FrameInput::new(&rows));
            assert!(is_device_lost(&result), "{result:?}");
        }
        assert!(is_device_lost(&target.clear(&gpu, Rgba::WHITE)));
        let fresh = OffscreenTarget::new(&gpu, size, FORMAT);
        assert!(is_device_lost(&fresh.clear(&gpu, Rgba::WHITE)));
        assert!(is_device_lost(&Renderer::new(
            &gpu,
            Theme::default(),
            menlo()
        )));
        let readback = {
            let gpu = gpu.clone();
            within_deadline("read_back", move || fresh.read_back(&gpu).map(|_| ()))
        };
        assert!(readback.is_err(), "{readback:?}");
    });
}

/// Metal's command queue holds at most this many command buffers
/// (`wgpu-hal`'s `MAX_COMMAND_BUFFERS`); one more is a device error that
/// `wgpu` treats as a lost device.
const MAX_COMMAND_BUFFERS: usize = 4096;

/// Fills `gpu`'s command queue with recorded, unsubmitted command buffers,
/// so the next one it records fails inside `wgpu-hal` with
/// `DeviceError::Lost`: the path a real device loss takes (`wgpu-core`'s
/// `Device::lose`, which runs the lost callback at once). Keep the buffers
/// alive until the loss has happened.
fn fill_command_queue(gpu: &Gpu) -> Vec<wgpu::CommandBuffer> {
    gpu.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("the device goes idle");
    let buffer = gpu.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("fill"),
        size: 16,
        usage: wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    (1..MAX_COMMAND_BUFFERS)
        .map(|_| {
            let mut encoder = gpu
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            encoder.clear_buffer(&buffer, 0, None);
            encoder.finish()
        })
        .collect()
}

/// The production path: a device error inside `wgpu-hal` makes `wgpu`
/// lose the device and run the callback during the very call that hit it.
#[test]
fn a_device_error_loses_the_device_during_the_call_that_hit_it() {
    watchdog(|| {
        let Some(gpu) = gpu() else { return };
        let other = Gpu::headless().expect("a second device");
        let target = OffscreenTarget::new(&gpu, PhysicalSize::new(4, 4), FORMAT);
        let held = fill_command_queue(&gpu);
        assert!(!gpu.is_lost(), "one command buffer short of the limit");

        let result = target.clear(&gpu, Rgba::WHITE);

        assert!(is_device_lost(&result), "{result:?}");
        assert!(gpu.is_lost());
        assert!(!other.is_lost());
        drop(held);
        assert!(gpu.is_lost(), "a lost device stays lost");
    });
}
