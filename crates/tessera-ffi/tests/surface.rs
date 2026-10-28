//! End-to-end tests of the surface API (`tessera_surface_*`), calling the
//! `extern "C"` functions exactly as a frontend would, with a bare
//! `CAMetalLayer` created through the Objective-C runtime.
//!
//! Argument checks run everywhere. Tests that attach a surface (and so
//! bring up the GPU) return early when `TESSERA_SKIP_GPU_TESTS` is set,
//! like the render suites. A layer outside any window may not vend
//! drawables, so a frame may report `Skipped` instead of `Presented`; both
//! are accepted.

#![cfg(target_os = "macos")]

use std::ffi::{CString, c_char};
use std::ptr;
use std::time::{Duration, Instant};

use tessera_app_core::SessionId;
use tessera_app_core::view::ViewSize;
use tessera_ffi::{
    App, Error, FrameStatus, SpawnOptions, tessera_app_free, tessera_app_new,
    tessera_session_close, tessera_session_spawn, tessera_session_write, tessera_surface_attach,
    tessera_surface_detach, tessera_surface_render, tessera_surface_resize,
    tessera_surface_set_font_size, tessera_test_surface_panic, tessera_test_with_core,
};

const TIMEOUT: Duration = Duration::from_secs(5);
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
        assert_eq!(unsafe { tessera_app_new(&raw mut app) }, Error::Ok);
        Self(app)
    }

    /// Spawns `/bin/sh` reading commands from the test.
    fn spawn_shell(&self) -> u64 {
        let program = CString::new("/bin/sh").unwrap();
        let options = SpawnOptions {
            program: program.as_ptr(),
            args: ptr::null::<*const c_char>(),
            arg_count: 0,
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

    /// Renders `frames` frames, each of which must succeed.
    fn render_frames(&self, id: u64, frames: usize) -> Vec<FrameStatus> {
        (0..frames)
            .map(|_| {
                let (code, status) = self.render(id);
                assert_eq!(code, Error::Ok);
                assert!(
                    matches!(status, FrameStatus::Presented | FrameStatus::Skipped),
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
    assert_eq!(app.render(id), (Error::Ok, FrameStatus::Skipped));
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
    assert_eq!(app.render(id), (Error::Ok, FrameStatus::Skipped));
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
