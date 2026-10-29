//! End-to-end tests of the surface API (`tessera_surface_*`), calling the
//! `extern "C"` functions exactly as a frontend would, with a bare
//! `CAMetalLayer` created through the Objective-C runtime.
//!
//! Argument checks run everywhere. Tests that attach a surface (and so
//! bring up the GPU) return early when `TESSERA_SKIP_GPU_TESTS` is set,
//! like the render suites. A layer outside any window may not vend
//! drawables, so a frame may report `NotReady` instead of `Presented`, and
//! frames rendered back to back may be `Paced`; all are accepted where a
//! test only needs frames to succeed.

#![cfg(target_os = "macos")]

mod common;

use std::ffi::{CString, c_char};
use std::ptr;
use std::time::{Duration, Instant};

use tessera_app_core::SessionId;
use tessera_app_core::view::{MIN_FRAME_INTERVAL, ViewSize};
use tessera_ffi::{
    App, Error, FrameStatus, SpawnOptions, SurfaceMetrics, tessera_app_free, tessera_app_new,
    tessera_session_close, tessera_session_resize, tessera_session_spawn, tessera_session_write,
    tessera_surface_attach, tessera_surface_detach, tessera_surface_metrics,
    tessera_surface_render, tessera_surface_resize, tessera_surface_set_font_size,
    tessera_test_surface_panic, tessera_test_with_core,
};

const TIMEOUT: Duration = Duration::from_secs(5);

/// What `Handle::metrics` pre-fills: no field holds a value a surface
/// reports.
const SENTINEL: SurfaceMetrics = SurfaceMetrics {
    width_px: 0xdead,
    height_px: 0xbeef,
    scale: -1.0,
    cell_width_px: 0xdead,
    cell_height_px: 0xbeef,
    baseline_px: -1.0,
    padding_px: 0xdead,
    rows: 0xbeef,
    cols: 0xdead,
    font_size_pt: -1.0,
    cursor_row: 0xbeef,
    cursor_col: 0xdead,
    cursor_on_screen: true,
    cursor_wide: true,
};
const SKIP_VAR: &str = "TESSERA_SKIP_GPU_TESTS";

/// `false` (with a note) when the GPU tests are switched off.
fn gpu_enabled() -> bool {
    if std::env::var_os(SKIP_VAR).is_some() {
        eprintln!("{SKIP_VAR} is set; skipping");
        return false;
    }
    true
}

/// Just enough of the Objective-C runtime to make a `CAMetalLayer`.
mod objc {
    use std::ffi::{CStr, c_void};

    type Id = *mut c_void;
    type Sel = *mut c_void;

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> Id;
        fn sel_registerName(name: *const std::ffi::c_char) -> Sel;
        fn objc_msgSend();
    }

    // Linked for `CAMetalLayer`'s class to be registered with the runtime.
    #[link(name = "QuartzCore", kind = "framework")]
    unsafe extern "C" {}

    /// `objc_msgSend` typed for a message with no arguments.
    fn send0() -> unsafe extern "C" fn(Id, Sel) -> Id {
        // SAFETY: `objc_msgSend` is called with the signature of the method
        // it dispatches to; every use below sends a no-argument message
        // returning an object (or `void`, whose return register is ignored).
        unsafe {
            std::mem::transmute::<unsafe extern "C" fn(), unsafe extern "C" fn(Id, Sel) -> Id>(
                objc_msgSend,
            )
        }
    }

    fn sel(name: &CStr) -> Sel {
        // SAFETY: `name` is a NUL-terminated selector name.
        unsafe { sel_registerName(name.as_ptr()) }
    }

    /// An owned (+1) `CAMetalLayer` (or other object), released on drop.
    pub struct Layer(Id);

    impl Layer {
        /// `[CAMetalLayer new]`; the core sets its device, format and size
        /// when it configures the surface.
        pub fn new() -> Self {
            Self::of_class(c"CAMetalLayer")
        }

        /// `[<class> new]`, for objects that are not a `CAMetalLayer`.
        pub fn of_class(name: &CStr) -> Self {
            // SAFETY: a NUL-terminated class name.
            let class = unsafe { objc_getClass(name.as_ptr()) };
            assert!(!class.is_null(), "{name:?} is not registered");
            // SAFETY: `+new` takes no arguments and returns a +1 object.
            let object = unsafe { send0()(class, sel(c"new")) };
            assert!(!object.is_null(), "[{name:?} new] failed");
            Self(object)
        }

        /// The `CAMetalLayer *` to hand to `tessera_surface_attach`.
        pub fn as_ptr(&self) -> *mut c_void {
            self.0
        }

        /// `-[CALayer isOpaque]`.
        pub fn is_opaque(&self) -> bool {
            // SAFETY: `-isOpaque` takes no arguments and returns a `BOOL`
            // (one byte, 0 or 1, on every Apple ABI), read as `u8`.
            let send = unsafe {
                std::mem::transmute::<unsafe extern "C" fn(), unsafe extern "C" fn(Id, Sel) -> u8>(
                    objc_msgSend,
                )
            };
            // SAFETY: a live layer and a no-argument getter.
            unsafe { send(self.0, sel(c"isOpaque")) != 0 }
        }
    }

    impl Drop for Layer {
        fn drop(&mut self) {
            // SAFETY: `-release` takes no arguments; this balances `+new`.
            unsafe { send0()(self.0, sel(c"release")) };
        }
    }
}

use objc::Layer;

/// An application handle freed on drop.
struct Handle(*mut App);

impl Handle {
    fn new() -> Self {
        let mut app = ptr::null_mut();
        // SAFETY: `app` is writable storage for one pointer.
        common::hermetic_config_dir();
        assert_eq!(unsafe { tessera_app_new(&raw mut app) }, Error::Ok);
        Self(app)
    }

    /// Spawns `/bin/sh` reading commands from the test.
    fn spawn_shell(&self) -> u64 {
        self.spawn_sh(&[])
    }

    /// Spawns `/bin/sh` with `args` (after `argv[0]`).
    fn spawn_sh(&self, args: &[&str]) -> u64 {
        let program = CString::new("/bin/sh").unwrap();
        let owned: Vec<CString> = args.iter().map(|arg| CString::new(*arg).unwrap()).collect();
        let pointers: Vec<*const c_char> = owned.iter().map(|arg| arg.as_ptr()).collect();
        let options = SpawnOptions {
            program: program.as_ptr(),
            args: if pointers.is_empty() {
                ptr::null()
            } else {
                pointers.as_ptr()
            },
            arg_count: pointers.len(),
            cwd: ptr::null(),
            rows: 24,
            cols: 80,
            width_px: 0,
            height_px: 0,
        };
        let mut id = 0;
        // SAFETY: live handle, valid options, writable id.
        let code = unsafe { tessera_session_spawn(self.0, &raw const options, &raw mut id) };
        assert_eq!(code, Error::Ok);
        id
    }

    fn attach(&self, id: u64, layer: &Layer, width: u32, height: u32, scale: f32) -> Error {
        // SAFETY: live handle; `layer` is a live `CAMetalLayer` that outlives
        // the view in every test (it is detached, closed or freed first).
        unsafe { tessera_surface_attach(self.0, id, layer.as_ptr(), width, height, scale) }
    }

    fn resize(&self, id: u64, width: u32, height: u32, scale: f32) -> Error {
        // SAFETY: live handle.
        unsafe { tessera_surface_resize(self.0, id, width, height, scale) }
    }

    fn render(&self, id: u64) -> (Error, FrameStatus) {
        // Pre-set to a value a failing call must overwrite.
        let mut status = FrameStatus::Presented;
        // SAFETY: live handle; `status` is writable.
        let code = unsafe { tessera_surface_render(self.0, id, &raw mut status) };
        (code, status)
    }

    fn set_font_size(&self, id: u64, size_pt: f32) -> Error {
        // SAFETY: live handle.
        unsafe { tessera_surface_set_font_size(self.0, id, size_pt) }
    }

    /// Reads the surface's metrics into a struct pre-filled with a
    /// sentinel, so a failing call can be seen to leave it alone.
    fn metrics(&self, id: u64) -> (Error, SurfaceMetrics) {
        let mut metrics = SENTINEL;
        // SAFETY: live handle; `metrics` is writable.
        let code = unsafe { tessera_surface_metrics(self.0, id, &raw mut metrics) };
        (code, metrics)
    }

    /// The metrics of a session that has a surface.
    fn metrics_ok(&self, id: u64) -> SurfaceMetrics {
        let (code, metrics) = self.metrics(id);
        assert_eq!(code, Error::Ok);
        metrics
    }

    fn detach(&self, id: u64) -> Error {
        // SAFETY: live handle.
        unsafe { tessera_surface_detach(self.0, id) }
    }

    fn close(&self, id: u64) -> Error {
        // SAFETY: live handle.
        unsafe { tessera_session_close(self.0, id) }
    }

    fn write(&self, id: u64, bytes: &[u8]) {
        // SAFETY: live handle; `bytes` is readable for its length.
        let code = unsafe { tessera_session_write(self.0, id, bytes.as_ptr(), bytes.len()) };
        assert_eq!(code, Error::Ok);
    }

    fn with_core<R>(&self, f: impl FnOnce(&tessera_app_core::App) -> R) -> R {
        // SAFETY: live handle, not freed while `f` runs.
        unsafe { tessera_test_with_core(self.0, f) }.expect("live handle")
    }

    fn view_size(&self, id: u64) -> ViewSize {
        self.with_core(|core| core.view_size(SessionId::from_raw(id)))
            .expect("session has a view")
    }

    fn view_ids(&self) -> Vec<u64> {
        self.with_core(|core| core.view_ids().into_iter().map(SessionId::get).collect())
    }

    fn screen(&self, id: u64) -> String {
        self.with_core(|core| {
            core.with_terminal(SessionId::from_raw(id), |term| {
                term.grid()
                    .visible_lines()
                    .iter()
                    .map(|line| line.text().trim_end().to_owned())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        })
        .expect("live session")
    }

    /// Asks the shell for its window size and waits until the screen shows
    /// `rows cols`.
    fn assert_child_sees(&self, id: u64, rows: u16, cols: u16) {
        self.write(id, b"stty size\n");
        let expected = format!("{rows} {cols}");
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let screen = self.screen(id);
            if screen.lines().any(|line| line.trim_end() == expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "child never reported {expected:?}:\n{screen}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Renders one frame after waiting out the pacing interval, so its
    /// status is not `Paced`.
    fn render_unpaced(&self, id: u64) -> (Error, FrameStatus) {
        std::thread::sleep(2 * MIN_FRAME_INTERVAL);
        self.render(id)
    }

    /// Renders `frames` frames, each of which must succeed.
    fn render_frames(&self, id: u64, frames: usize) -> Vec<FrameStatus> {
        (0..frames)
            .map(|_| {
                let (code, status) = self.render(id);
                assert_eq!(code, Error::Ok);
                assert!(
                    matches!(
                        status,
                        FrameStatus::Presented
                            | FrameStatus::Skipped
                            | FrameStatus::Paced
                            | FrameStatus::NotReady
                    ),
                    "{status:?}"
                );
                status
            })
            .collect()
    }

    fn free(mut self) {
        // SAFETY: live handle, freed exactly once (drop sees NULL).
        unsafe { tessera_app_free(std::mem::replace(&mut self.0, ptr::null_mut())) };
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: NULL or a live handle not freed before.
        unsafe { tessera_app_free(self.0) };
    }
}

#[test]
fn null_pointers_are_rejected() {
    let layer = Layer::new();
    let mut status = FrameStatus::Presented;
    // SAFETY: NULL handles are allowed and rejected; the layer is valid.
    unsafe {
        let null = ptr::null_mut();
        assert_eq!(
            tessera_surface_attach(null, 1, layer.as_ptr(), 800, 600, 1.0),
            Error::NullPointer
        );
        assert_eq!(
            tessera_surface_resize(null, 1, 800, 600, 1.0),
            Error::NullPointer
        );
        assert_eq!(
            tessera_surface_render(null, 1, &raw mut status),
            Error::NullPointer
        );
        assert_eq!(status, FrameStatus::Skipped, "out_status is reset first");
        assert_eq!(
            tessera_surface_set_font_size(null, 1, 14.0),
            Error::NullPointer
        );
        assert_eq!(tessera_surface_detach(null, 1), Error::NullPointer);
    }
    let app = Handle::new();
    let id = app.spawn_shell();
    // SAFETY: live handle; NULL layer and NULL out pointer are rejected.
    unsafe {
        assert_eq!(
            tessera_surface_attach(app.0, id, ptr::null_mut(), 800, 600, 1.0),
            Error::NullPointer
        );
        assert_eq!(
            tessera_surface_render(app.0, id, ptr::null_mut()),
            Error::NullPointer
        );
    }
    assert_eq!(app.view_ids(), Vec::<u64>::new());
}

#[test]
fn calls_need_a_live_session_and_a_surface() {
    let app = Handle::new();
    let id = app.spawn_shell();
    let layer = Layer::new();
    // Rejected before the layer or the GPU is touched.
    let unknown = id + 100;
    assert_eq!(
        app.attach(unknown, &layer, 800, 600, 1.0),
        Error::SessionNotFound
    );
    for scale in [0.0, -2.0, f32::NAN, f32::INFINITY] {
        assert_eq!(
            app.attach(id, &layer, 800, 600, scale),
            Error::InvalidArgument
        );
        assert_eq!(app.resize(id, 800, 600, scale), Error::InvalidArgument);
    }
    // Operations before an attach.
    assert_eq!(app.resize(id, 800, 600, 1.0), Error::NoSurface);
    assert_eq!(app.render(id), (Error::NoSurface, FrameStatus::Skipped));
    assert_eq!(app.set_font_size(id, 14.0), Error::NoSurface);
    assert_eq!(app.detach(id), Error::NoSurface);
    // Unknown sessions.
    assert_eq!(app.resize(unknown, 800, 600, 1.0), Error::SessionNotFound);
    assert_eq!(
        app.render(unknown),
        (Error::SessionNotFound, FrameStatus::Skipped)
    );
    assert_eq!(app.set_font_size(unknown, 14.0), Error::SessionNotFound);
    assert_eq!(app.detach(unknown), Error::SessionNotFound);
    assert_eq!(app.view_ids(), Vec::<u64>::new());
}

#[test]
fn objects_that_are_not_metal_layers_are_rejected() {
    let app = Handle::new();
    let id = app.spawn_shell();
    // wgpu would assert (and panic) on these; the FFI checks the class
    // first and touches no GPU.
    for class in [c"CALayer", c"NSObject"] {
        let object = Layer::of_class(class);
        assert_eq!(
            app.attach(id, &object, 800, 600, 1.0),
            Error::InvalidArgument,
            "{class:?}"
        );
    }
    assert_eq!(app.view_ids(), Vec::<u64>::new());
    assert_eq!(app.detach(id), Error::NoSurface);
}

#[test]
fn metrics_need_a_handle_an_out_pointer_a_session_and_a_surface() {
    let mut metrics = SENTINEL;
    // SAFETY: a NULL handle is rejected; `metrics` is writable.
    let code = unsafe { tessera_surface_metrics(ptr::null_mut(), 1, &raw mut metrics) };
    assert_eq!(code, Error::NullPointer);
    assert_eq!(metrics, SENTINEL);
    let app = Handle::new();
    let id = app.spawn_shell();
    // SAFETY: live handle; a NULL out pointer is rejected.
    let code = unsafe { tessera_surface_metrics(app.0, id, ptr::null_mut()) };
    assert_eq!(code, Error::NullPointer);
    // Errors leave `*out` as it was.
    assert_eq!(app.metrics(id), (Error::NoSurface, SENTINEL));
    assert_eq!(app.metrics(id + 100), (Error::SessionNotFound, SENTINEL));
    if gpu_enabled() {
        let layer = Layer::new();
        assert_eq!(app.attach(id, &layer, 800, 600, 1.0), Error::Ok);
        assert_eq!(app.detach(id), Error::Ok);
        assert_eq!(app.metrics(id), (Error::NoSurface, SENTINEL));
    }
    assert_eq!(app.close(id), Error::Ok);
    assert_eq!(app.metrics(id), (Error::SessionNotFound, SENTINEL));
}

#[test]
fn metrics_report_the_surface_the_grid_and_the_cursor() {
    if !gpu_enabled() {
        return;
    }
    let app = Handle::new();
    // A child whose output the test knows, whatever the environment's
    // shell prompt: "ready", then each line `cat` reads (echo is off, so
    // only `cat` writes; the terminal turns CR into NL on input and NL into
    // CR NL on output).
    let id = app.spawn_sh(&["-c", "stty -echo; printf ready; exec cat"]);
    let layer = Layer::new();
    assert_eq!(app.attach(id, &layer, 1600, 1200, 2.0), Error::Ok);
    let metrics = app.metrics_ok(id);
    let (size, cell, font_size) = app.with_core(|core| {
        let id = SessionId::from_raw(id);
        (
            core.view_size(id).unwrap(),
            core.view_cell_metrics(id).unwrap(),
            core.view_font_size(id).unwrap(),
        )
    });
    assert_eq!((metrics.width_px, metrics.height_px), (1600, 1200));
    assert!((metrics.scale - 2.0).abs() < f32::EPSILON);
    assert_eq!(
        (metrics.cell_width_px, metrics.cell_height_px),
        (cell.width_px, cell.height_px)
    );
    assert!((metrics.baseline_px - cell.baseline_px).abs() < f32::EPSILON);
    // The default theme's 4 pt of padding at 2x.
    assert_eq!(metrics.padding_px, 8);
    assert!((metrics.font_size_pt - font_size).abs() < f32::EPSILON);
    assert_eq!((metrics.rows, metrics.cols), (size.rows, size.cols));

    // `rows` and `cols` are the session's grid: what an explicit resize
    // asked for, until the surface resizes it again.
    // SAFETY: live handle.
    let code = unsafe { tessera_session_resize(app.0, id, 30, 90, 0, 0) };
    assert_eq!(code, Error::Ok);
    let resized = app.metrics_ok(id);
    assert_eq!((resized.rows, resized.cols), (30, 90));
    assert_eq!(app.resize(id, 1600, 1200, 2.0), Error::Ok);
    let back = app.metrics_ok(id);
    assert_eq!((back.rows, back.cols), (size.rows, size.cols));

    // A zoom shows in the font size and the cell.
    assert_eq!(app.set_font_size(id, 26.0), Error::Ok);
    let zoomed = app.metrics_ok(id);
    assert!((zoomed.font_size_pt - 26.0).abs() < f32::EPSILON);
    assert!(zoomed.cell_width_px > metrics.cell_width_px, "{zoomed:?}");
    assert!(zoomed.rows < metrics.rows, "{zoomed:?}");
    assert_eq!(app.set_font_size(id, font_size), Error::Ok);

    // Each line end `cat` copies back moves the cursor to the next row.
    let ready = wait_for_cursor(&app, id, (0, 5));
    assert!(ready.cursor_on_screen && !ready.cursor_wide, "{ready:?}");
    app.write(id, b"\r");
    wait_for_cursor(&app, id, (1, 0));
    app.write(id, b"\r\n");
    wait_for_cursor(&app, id, (3, 0));
    assert_eq!(app.close(id), Error::Ok);
}

/// Waits until the cursor is at `(row, col)` and returns the metrics then.
fn wait_for_cursor(app: &Handle, id: u64, (row, col): (u16, u16)) -> SurfaceMetrics {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let now = app.metrics_ok(id);
        if (now.cursor_row, now.cursor_col) == (row, col) {
            return now;
        }
        assert!(
            Instant::now() < deadline,
            "the cursor is at {:?}, not {:?}:\n{}",
            (now.cursor_row, now.cursor_col),
            (row, col),
            app.screen(id)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_panic_inside_a_surface_call_is_caught() {
    let app = Handle::new();
    let id = app.spawn_shell();
    // SAFETY: live handle.
    assert_eq!(unsafe { tessera_test_surface_panic(app.0) }, Error::Panic);
    // SAFETY: NULL is rejected without panicking.
    assert_eq!(
        unsafe { tessera_test_surface_panic(ptr::null_mut()) },
        Error::NullPointer
    );
    // The view table recovered from the poisoned lock.
    assert_eq!(app.detach(id), Error::NoSurface);
    if gpu_enabled() {
        let layer = Layer::new();
        assert_eq!(app.attach(id, &layer, 640, 480, 1.0), Error::Ok);
        app.render_frames(id, 1);
        assert_eq!(app.detach(id), Error::Ok);
    }
}

/// A lost GPU device (an unplugged external GPU, a driver reset; injected
/// with `App::lose_gpu_for_test`) detaches the surface on the next render,
/// resizing it meanwhile is harmless, and attaching again draws on a new
/// device.
#[test]
fn a_lost_device_detaches_the_surface_and_reattaching_recovers() {
    if !gpu_enabled() {
        return;
    }
    let app = Handle::new();
    let id = app.spawn_shell();
    let layer = Layer::new();
    assert_eq!(app.attach(id, &layer, 640, 480, 2.0), Error::Ok);
    app.render_frames(id, 2);

    assert!(app.with_core(tessera_app_core::App::lose_gpu_for_test));
    assert_eq!(app.resize(id, 800, 600, 2.0), Error::Ok);
    assert_eq!(app.resize(id, 0, 0, 2.0), Error::Ok);
    assert_eq!(app.set_font_size(id, 18.0), Error::Ok);
    assert_eq!(app.render(id), (Error::Gpu, FrameStatus::Skipped));
    assert!(app.view_ids().is_empty(), "the surface is detached");
    assert_eq!(app.render(id), (Error::NoSurface, FrameStatus::Skipped));
    assert_eq!(app.metrics(id).0, Error::NoSurface);

    let layer = Layer::new();
    assert_eq!(app.attach(id, &layer, 640, 480, 2.0), Error::Ok);
    app.render_frames(id, 2);
    assert_eq!(app.metrics_ok(id).width_px, 640);
    assert_eq!(app.detach(id), Error::Ok);
    assert!(
        !app.with_core(tessera_app_core::App::lose_gpu_for_test),
        "the last detach released the GPU"
    );
}

#[test]
fn the_grid_follows_the_surface() {
    if !gpu_enabled() {
        return;
    }
    let app = Handle::new();
    let id = app.spawn_shell();
    let layer = Layer::new();

    // A view attached before it has a size draws nothing and leaves the
    // session at its spawn size.
    assert_eq!(app.attach(id, &layer, 0, 0, 1.0), Error::Ok);
    let empty = app.view_size(id);
    assert_eq!((empty.pixels.width, empty.pixels.height), (0, 0));
    assert_eq!(app.render(id), (Error::Ok, FrameStatus::NotReady));
    // Still pending, so retried: still not ready.
    assert_eq!(app.render(id), (Error::Ok, FrameStatus::NotReady));
    app.assert_child_sees(id, 24, 80);
    assert_eq!(app.detach(id), Error::Ok);

    assert_eq!(app.attach(id, &layer, 800, 600, 1.0), Error::Ok);
    assert_eq!(app.view_ids(), [id]);
    let at_1x = app.view_size(id);
    assert_eq!((at_1x.pixels.width, at_1x.pixels.height), (800, 600));
    assert!(at_1x.rows > 1 && at_1x.cols > 1, "{at_1x:?}");
    app.assert_child_sees(id, at_1x.rows, at_1x.cols);
    eprintln!("800x600 @1x: {:?}", app.render_frames(id, 3));

    // One attach per session.
    let other = Layer::new();
    assert_eq!(
        app.attach(id, &other, 800, 600, 1.0),
        Error::InvalidArgument
    );

    // The same window on a Retina display: twice the pixels, about the
    // same grid.
    assert_eq!(app.resize(id, 1600, 1200, 2.0), Error::Ok);
    let at_2x = app.view_size(id);
    assert_eq!((at_2x.pixels.width, at_2x.pixels.height), (1600, 1200));
    assert!((at_2x.scale - 2.0).abs() < f32::EPSILON);
    assert!(
        at_2x.rows.abs_diff(at_1x.rows) <= 1 && at_2x.cols.abs_diff(at_1x.cols) <= 2,
        "{at_1x:?} vs {at_2x:?}"
    );
    app.assert_child_sees(id, at_2x.rows, at_2x.cols);
    eprintln!("1600x1200 @2x: {:?}", app.render_frames(id, 3));

    // A collapsed view draws nothing and leaves the session's size alone.
    assert_eq!(app.resize(id, 0, 0, 2.0), Error::Ok);
    assert_eq!(app.render_unpaced(id), (Error::Ok, FrameStatus::NotReady));
    app.assert_child_sees(id, at_2x.rows, at_2x.cols);

    assert_eq!(app.resize(id, 800, 600, 1.0), Error::Ok);
    app.render_frames(id, 1);

    // A larger font fits fewer cells; out-of-range sizes are clamped.
    let normal_cell = app
        .with_core(|core| core.view_cell_metrics(SessionId::from_raw(id)))
        .unwrap();
    assert_eq!(app.set_font_size(id, 26.0), Error::Ok);
    let (font_size, zoomed_cell) = app.with_core(|core| {
        let id = SessionId::from_raw(id);
        (
            core.view_font_size(id).unwrap(),
            core.view_cell_metrics(id).unwrap(),
        )
    });
    assert!((font_size - 26.0).abs() < f32::EPSILON, "{font_size}");
    assert!(
        zoomed_cell.width_px > normal_cell.width_px,
        "{normal_cell:?} -> {zoomed_cell:?}"
    );
    let zoomed = app.view_size(id);
    assert!(
        zoomed.cols < at_1x.cols && zoomed.rows < at_1x.rows,
        "{zoomed:?}"
    );
    app.assert_child_sees(id, zoomed.rows, zoomed.cols);
    app.render_frames(id, 1);
    for size in [f32::NAN, 0.0, 1.0e9] {
        assert_eq!(app.set_font_size(id, size), Error::Ok);
        app.render_frames(id, 1);
    }

    assert_eq!(app.detach(id), Error::Ok);
    assert_eq!(app.detach(id), Error::NoSurface, "detach twice");
    assert_eq!(app.render(id), (Error::NoSurface, FrameStatus::Skipped));
    assert_eq!(app.view_ids(), Vec::<u64>::new());

    // The layer can be attached again after a detach.
    assert_eq!(app.attach(id, &layer, 800, 600, 1.0), Error::Ok);
    app.render_frames(id, 1);
    assert_eq!(app.detach(id), Error::Ok);
    assert_eq!(app.close(id), Error::Ok);
}

#[test]
fn closing_a_session_detaches_its_surface() {
    if !gpu_enabled() {
        return;
    }
    let app = Handle::new();
    let id = app.spawn_shell();
    let other = app.spawn_shell();
    let layers = [Layer::new(), Layer::new()];
    assert_eq!(app.attach(id, &layers[0], 400, 300, 2.0), Error::Ok);
    assert_eq!(app.attach(other, &layers[1], 400, 300, 2.0), Error::Ok);
    app.render_frames(id, 1);
    assert_eq!(app.close(id), Error::Ok);
    assert_eq!(app.view_ids(), [other], "the closed session's view is gone");
    assert_eq!(
        app.render(id),
        (Error::SessionNotFound, FrameStatus::Skipped)
    );
    assert_eq!(app.detach(id), Error::SessionNotFound);
    // The other session's surface is unaffected.
    app.render_frames(other, 1);
    assert_eq!(app.close(other), Error::Ok);
    assert_eq!(app.view_ids(), Vec::<u64>::new());
}

#[test]
fn freeing_the_app_detaches_every_surface() {
    if !gpu_enabled() {
        return;
    }
    let layers = [Layer::new(), Layer::new()];
    let app = Handle::new();
    for layer in &layers {
        let id = app.spawn_shell();
        assert_eq!(app.attach(id, layer, 320, 240, 1.0), Error::Ok);
        app.render_frames(id, 1);
    }
    assert_eq!(app.view_ids().len(), 2);
    let start = Instant::now();
    app.free();
    assert!(start.elapsed() < TIMEOUT, "free took {:?}", start.elapsed());
    // `layers` is released only now, after the core let go of it.
    drop(layers);
}

#[test]
fn applying_a_theme_rethemes_attached_views() {
    if !gpu_enabled() {
        return;
    }
    let app = Handle::new();
    let ids = [app.spawn_shell(), app.spawn_shell()];
    let layers = [Layer::new(), Layer::new(), Layer::new()];
    for (id, layer) in ids.iter().zip(&layers) {
        assert_eq!(app.attach(*id, layer, 320, 240, 2.0), Error::Ok);
    }
    let background = |id: u64| {
        app.with_core(|core| core.view_renderer_theme(SessionId::from_raw(id)))
            .expect("the session has a view")
            .background
            .to_srgb8()
    };
    for id in ids {
        assert_eq!(background(id), [0, 0, 0], "Tessera Dark to start with");
    }

    let dracula = tessera_config::Theme::bundled("Dracula").unwrap();
    app.with_core(|core| core.apply_theme(dracula));
    let expected = app.with_core(tessera_app_core::App::view_theme);
    for id in ids {
        let theme = app
            .with_core(|core| core.view_renderer_theme(SessionId::from_raw(id)))
            .unwrap();
        assert_eq!(theme, expected, "view {id} draws with the applied theme");
        assert_eq!(background(id), [0x28, 0x2a, 0x36]);
        // The view keeps its own font size: only the colours changed.
        assert!(app.view_size(id).rows > 1);
        app.render_frames(id, 1);
    }

    // A view attached afterwards starts with the applied theme too.
    let late = app.spawn_shell();
    assert_eq!(app.attach(late, &layers[2], 320, 240, 2.0), Error::Ok);
    assert_eq!(background(late), [0x28, 0x2a, 0x36]);
    for id in ids.into_iter().chain([late]) {
        assert_eq!(app.close(id), Error::Ok);
    }
}

/// A live re-theme between an opaque and a translucent background switches
/// the compositing mode of a view that is already attached (#41's
/// adversarial finding: it used to keep the mode it was attached with, so a
/// translucent theme drew dark on an opaque surface). The switch is
/// requested by `apply_config`, which may run off the main thread, and
/// made by the next frame; the layer's `opaque` property follows.
#[test]
fn a_translucent_retheme_switches_an_attached_surface() {
    use tessera_app_core::view::wgpu::CompositeAlphaMode;

    if !gpu_enabled() {
        return;
    }
    let app = Handle::new();
    let id = app.spawn_shell();
    let layer = Layer::new();
    assert_eq!(app.attach(id, &layer, 320, 240, 2.0), Error::Ok);
    app.render_frames(id, 1);
    let modes = || {
        app.with_core(|core| core.view_alpha_modes(SessionId::from_raw(id)))
            .expect("the session has a view")
    };
    assert_eq!(modes(), (CompositeAlphaMode::Opaque, false));
    assert!(layer.is_opaque());
    // Whether this surface can be translucent at all is decided from what
    // it supports, before anything is applied, so a switch that does not
    // happen fails below rather than skipping.
    let supported = app
        .with_core(|core| core.view_supported_alpha_modes(SessionId::from_raw(id)))
        .expect("the session has a view");
    let translucent = [
        CompositeAlphaMode::PreMultiplied,
        CompositeAlphaMode::PostMultiplied,
        CompositeAlphaMode::Inherit,
    ];
    if !supported.iter().any(|mode| translucent.contains(mode)) {
        eprintln!("this surface supports no per-pixel-alpha mode ({supported:?}); skipping");
        return;
    }

    let config = |opacity: f32| {
        let mut config = tessera_config::Config::default();
        config.appearance.theme = "Dracula".into();
        config.appearance.opacity = opacity;
        config
    };
    let dracula = tessera_config::Theme::bundled("Dracula").unwrap();
    // Applied from another thread, as the configuration watch does.
    app.with_core(|core| {
        std::thread::scope(|scope| {
            scope.spawn(|| core.apply_config(&config(0.5), dracula));
        });
    });
    assert_eq!(
        modes(),
        (CompositeAlphaMode::Opaque, true),
        "requested, not yet switched: the layer is reconfigured by the next frame"
    );
    // Past the frame pacing interval, so the frame really draws.
    std::thread::sleep(Duration::from_millis(10));
    app.render_frames(id, 1);
    let (mode, wanted) = modes();
    assert!(wanted);
    assert!(
        translucent.contains(&mode),
        "switched by the frame: {mode:?}"
    );
    assert!(!layer.is_opaque(), "the layer composites with alpha now");

    // And back.
    app.with_core(|core| core.apply_config(&config(1.0), dracula));
    std::thread::sleep(Duration::from_millis(10));
    app.render_frames(id, 1);
    assert_eq!(modes(), (CompositeAlphaMode::Opaque, false));
    assert!(layer.is_opaque());
    assert_eq!(app.close(id), Error::Ok);
}
