//! Release gate budgets (#216): the live heap of one idle session, of the
//! same session with a full scrollback, and the wall time from
//! `tessera_app_new` to the first frame of the first session.
//!
//! Each budget is main's measured value plus 20 %, recorded beside it with
//! the date, the machine (`sysctl hw.model`) and the compiler. A change that
//! moves a value past its budget either fixes the regression or, when the
//! cost is intended, raises the budget in the same change with a new
//! measurement (`docs/software-factory.md`, hang protection).
//!
//! The heap is counted by this binary's global allocator: every Rust
//! allocation of every thread (the session's reader and writer threads
//! too), not what Metal, Core Text or libc allocate on their own. The
//! sessions run `/bin/bash --noprofile --norc` with a fixed prompt and
//! shell integration off, so no number depends on the machine's dotfiles.
//!
//! Release-only (debug builds keep the same structures but run them far
//! slower): the gate's release timing step runs them in the `timing` group,
//! one at a time, and retries once under load. Under nextest each test is
//! its own process, so the process-wide count sees only its own session.

#![cfg(target_os = "macos")]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::ffi::{CString, c_char};
use std::process::{Command, Stdio};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tessera_app_core::SessionId;
use tessera_app_core::view::MIN_FRAME_INTERVAL;
use tessera_ffi::{
    App, Error, FrameStatus, SpawnOptions, tessera_app_free, tessera_app_new,
    tessera_session_spawn, tessera_session_write, tessera_surface_attach, tessera_surface_render,
    tessera_test_with_core,
};

/// Live heap bytes of one idle 80x24 session after its first prompt,
/// counted from an application that has settled with no session.
///
/// 2026-11-24, Mac16,10, rustc 1.100.0: measured 123 690 bytes (123 686 to
/// 123 690 over seven runs).
const IDLE_SESSION_HEAP_BUDGET: usize = 148_428;

/// Live heap bytes of the same session once its scrollback holds
/// 10 000 lines of 80 columns (the default `scrollback_lines` cap).
///
/// 2026-11-24, Mac16,10, rustc 1.100.0: measured 16 763 996 bytes (16 763 992
/// to 16 764 396 over seven runs).
const FULL_SCROLLBACK_SESSION_HEAP_BUDGET: usize = 20_116_796;

/// Wall time from `tessera_app_new` through the first session's spawn to
/// its first presented frame on an off-screen `CAMetalLayer`, best of
/// [`STARTUP_RUNS`] fresh processes.
///
/// 2026-11-24, Mac16,10, rustc 1.100.0: measured 16.37 ms (the median of
/// six best-of-5 runs at load 18-20, 15.47 to 17.97 ms; with the measuring
/// thread at the main thread's class, best-of-5 runs read 16.8 to 19.8 ms
/// at load 30-35 and 18.0 to 18.8 ms at load 82-88). The bar assumes the
/// default scheduling priority: run at background priority (`taskpolicy
/// -b`) it fails.
const STARTUP_BUDGET: Duration = Duration::from_micros(19_645);

/// Fresh processes the startup bar takes the best of.
const STARTUP_RUNS: usize = 5;

/// Lines the scrollback test prints: more than the scrollback cap plus a
/// screen, so the scrollback ends full whatever the prompt took.
const PRINTED_LINES: usize = 10_100;

/// The scrollback cap of the default configuration.
const SCROLLBACK_LINES: usize = 10_000;

/// The prompt of every session (`PS1`); a screen whose last row is this
/// prompt has an idle shell.
const PROMPT: &str = "budget$ ";

/// How long a shell may take to reach a prompt, or an off-screen layer to
/// present a frame. Only a broken run waits this long: every wait polls.
const TIMEOUT: Duration = Duration::from_secs(60);

/// How often the waits look.
const POLL: Duration = Duration::from_millis(10);

/// The heap counts as settled once it moved less than [`SETTLE_SLACK`]
/// bytes over this many polls of [`SETTLE_POLL`] in a row.
const SETTLE_POLLS: usize = 5;
const SETTLE_POLL: Duration = Duration::from_millis(50);
const SETTLE_SLACK: usize = 4096;

/// Set in the child processes of the startup bar: the test then measures
/// one startup and prints it on a line starting with [`STARTUP_LINE`].
const CHILD_VAR: &str = "TESSERA_BUDGET_STARTUP_CHILD";
const STARTUP_LINE: &str = "startup-nanos: ";

// ---------------------------------------------------------------------------
// Counting allocator
// ---------------------------------------------------------------------------

/// [`System`], counting the bytes currently allocated by every thread.
struct Counting;

/// Bytes allocated and not yet freed, process-wide.
static LIVE: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static ALLOCATOR: Counting = Counting;

// SAFETY: every call forwards to `System` with the caller's arguments; the
// counter is a side effect that never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let block = unsafe { System.alloc(layout) };
        if !block.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let block = unsafe { System.alloc_zeroed(layout) };
        if !block.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        block
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(block, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded unchanged.
        let moved = unsafe { System.realloc(block, layout, new_size) };
        if !moved.is_null() {
            LIVE.fetch_add(new_size, Ordering::Relaxed);
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        moved
    }
}

/// The live heap once it has stopped moving (or after [`TIMEOUT`]): a
/// session's threads may still be finishing their first reads.
fn settled_heap() -> usize {
    let deadline = Instant::now() + TIMEOUT;
    let mut last = LIVE.load(Ordering::SeqCst);
    let mut still = 0;
    while still < SETTLE_POLLS && Instant::now() < deadline {
        thread::sleep(SETTLE_POLL);
        let now = LIVE.load(Ordering::SeqCst);
        still = if now.abs_diff(last) < SETTLE_SLACK {
            still + 1
        } else {
            0
        };
        last = now;
    }
    last
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// An application handle freed on drop.
struct Handle(*mut App);

impl Handle {
    /// A fresh application; [`configure`] runs first.
    fn new() -> Self {
        let mut app = ptr::null_mut();
        // SAFETY: `app` is writable storage for one pointer.
        assert_eq!(unsafe { tessera_app_new(&raw mut app) }, Error::Ok);
        Self(app)
    }

    /// Spawns `/bin/bash --noprofile --norc` at 80x24.
    fn spawn_bash(&self) -> u64 {
        let program = CString::new("/bin/bash").unwrap();
        let owned = [c"--noprofile", c"--norc"];
        let pointers: Vec<*const c_char> = owned.iter().map(|arg| arg.as_ptr()).collect();
        let options = SpawnOptions {
            program: program.as_ptr(),
            args: pointers.as_ptr(),
            arg_count: pointers.len(),
            cwd: ptr::null(),
            rows: 24,
            cols: 80,
            width_px: 0,
            height_px: 0,
            profile: ptr::null(),
        };
        let mut id = 0;
        // SAFETY: live handle, valid options, writable id.
        let code = unsafe { tessera_session_spawn(self.0, &raw const options, &raw mut id) };
        assert_eq!(code, Error::Ok);
        id
    }

    fn write(&self, id: u64, bytes: &[u8]) {
        // SAFETY: live handle; `bytes` is readable for its length.
        let code = unsafe { tessera_session_write(self.0, id, bytes.as_ptr(), bytes.len()) };
        assert_eq!(code, Error::Ok);
    }

    /// The visible rows (trimmed, without the blank rows below the last
    /// written one) and the scrollback's length.
    fn screen(&self, id: u64) -> (Vec<String>, usize) {
        // SAFETY: live handle, not freed while the closure runs.
        unsafe {
            tessera_test_with_core(self.0, |core| {
                core.with_terminal(SessionId::from_raw(id), |term| {
                    let mut rows: Vec<String> = term
                        .grid()
                        .visible_lines()
                        .iter()
                        .map(|line| line.text().trim_end().to_owned())
                        .collect();
                    while rows.last().is_some_and(String::is_empty) {
                        rows.pop();
                    }
                    (rows, term.grid().scrollback_len())
                })
            })
        }
        .expect("live handle")
        .expect("live session")
    }

    /// Waits until the shell shows its prompt on the last written row and
    /// `ready` holds for the rows above it and the scrollback's length.
    fn wait_for_prompt(&self, id: u64, what: &str, ready: impl Fn(&[String], usize) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let (rows, scrollback) = self.screen(id);
            if let Some((last, above)) = rows.split_last()
                && last == PROMPT.trim_end()
                && ready(above, scrollback)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no {what} after {TIMEOUT:?} (scrollback {scrollback}):\n{}",
                rows.join("\n")
            );
            thread::sleep(POLL);
        }
    }

    /// Attaches an off-screen `CAMetalLayer` to `id` and renders until a
    /// frame is presented.
    fn first_frame(&self, id: u64, layer: &objc::Layer) {
        // SAFETY: live handle; `layer` is a live `CAMetalLayer` that
        // outlives the view (the application is freed first).
        let code = unsafe { tessera_surface_attach(self.0, id, layer.as_ptr(), 1280, 800, 2.0) };
        assert_eq!(code, Error::Ok);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let mut status = FrameStatus::Skipped;
            // SAFETY: live handle; `status` is writable.
            let code = unsafe { tessera_surface_render(self.0, id, &raw mut status) };
            assert_eq!(code, Error::Ok);
            match status {
                FrameStatus::Presented | FrameStatus::Suboptimal => return,
                FrameStatus::Paced | FrameStatus::NotReady | FrameStatus::Skipped => {}
            }
            assert!(
                Instant::now() < deadline,
                "no frame presented after {TIMEOUT:?}: {status:?}"
            );
            thread::sleep(MIN_FRAME_INTERVAL);
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a live handle, freed once.
        unsafe { tessera_app_free(self.0) };
    }
}

/// Points the process at the test home and at a hermetic configuration
/// directory whose `config.toml` turns shell integration off, and sets the
/// shells' environment: [`PROMPT`], no history file and no deprecation
/// notice from `/bin/bash`. Call it before the first [`Handle::new`].
fn configure() {
    common::isolate_home();
    let dir = common::hermetic_config_dir();
    std::fs::write(
        dir.join("config.toml"),
        "[terminal]\nshell_integration = \"off\"\n",
    )
    .expect("write config.toml");
    // SAFETY: called before the application or any thread starts, and
    // nextest runs each test in its own process (see `common`).
    unsafe {
        std::env::set_var("PS1", PROMPT);
        std::env::set_var("HISTFILE", "/dev/null");
        std::env::set_var("BASH_SILENCE_DEPRECATION_WARNING", "1");
        // Inherited hooks would run code at every prompt or at start.
        std::env::remove_var("PROMPT_COMMAND");
        std::env::remove_var("PS0");
        std::env::remove_var("BASH_ENV");
    }
}

/// Runs the calling thread at `QOS_CLASS_USER_INTERACTIVE`, the class of
/// the app's main thread, which makes these calls in the real app.
fn run_as_the_main_thread() {
    unsafe extern "C" {
        // <pthread/qos.h>; `qos_class_t` is an `unsigned int` enum.
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    /// `QOS_CLASS_USER_INTERACTIVE` from `<sys/qos.h>`.
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
    // SAFETY: takes plain integers and changes only the calling thread, a
    // test thread and not a workqueue thread.
    let status = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
    assert_eq!(
        status, 0,
        "cannot set the thread's quality-of-service class"
    );
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

    /// Sends the no-argument message `name` to `receiver`.
    fn send0(receiver: Id, name: &CStr) -> Id {
        // SAFETY: `objc_msgSend` is called with the signature of the method
        // it dispatches to: every use sends a no-argument message returning
        // an object (or `void`, whose return register is ignored).
        let send = unsafe {
            std::mem::transmute::<unsafe extern "C" fn(), unsafe extern "C" fn(Id, Sel) -> Id>(
                objc_msgSend,
            )
        };
        // SAFETY: a live receiver, a NUL-terminated selector name and a
        // no-argument message.
        unsafe { send(receiver, sel_registerName(name.as_ptr())) }
    }

    /// An owned (+1) `CAMetalLayer`, released on drop.
    pub struct Layer(Id);

    impl Layer {
        /// `[CAMetalLayer new]`; the core sets its device, format and size
        /// when it configures the surface.
        pub fn new() -> Self {
            // SAFETY: a NUL-terminated class name.
            let class = unsafe { objc_getClass(c"CAMetalLayer".as_ptr()) };
            assert!(!class.is_null(), "CAMetalLayer is not registered");
            let layer = send0(class, c"new");
            assert!(!layer.is_null(), "[CAMetalLayer new] failed");
            Self(layer)
        }

        /// The `CAMetalLayer *` to hand to `tessera_surface_attach`.
        pub fn as_ptr(&self) -> *mut c_void {
            self.0
        }
    }

    impl Drop for Layer {
        fn drop(&mut self) {
            // Balances `+new`.
            send0(self.0, c"release");
        }
    }
}

// ---------------------------------------------------------------------------
// Heap budgets
// ---------------------------------------------------------------------------

/// Starts an application, lets it settle, spawns the session and waits for
/// its first prompt; returns the handle, the session and the settled heap
/// the application held before the session.
fn idle_session() -> (Handle, u64, usize) {
    assert!(
        std::env::var_os("NEXTEST_RUN_ID").is_some(),
        "the heap budgets count every thread of the process: run them under nextest \
         (one process per test); under `cargo test` the tests' sessions share it"
    );
    configure();
    let app = Handle::new();
    let before = settled_heap();
    let id = app.spawn_bash();
    app.wait_for_prompt(id, "first prompt", |_, _| true);
    (app, id, before)
}

/// Fails when `bytes` is over `budget`, naming both.
#[track_caller]
fn assert_within(what: &str, bytes: usize, budget: usize) {
    eprintln!("{what}: {bytes} bytes (budget {budget})");
    assert!(
        bytes <= budget,
        "{what} holds {bytes} bytes of heap, over its budget of {budget} bytes \
         (main's measurement plus 20 %); fix the regression or, if the cost is \
         intended, re-measure and raise the budget with the new value"
    );
}

/// One idle 80x24 shell after its first prompt holds at most
/// [`IDLE_SESSION_HEAP_BUDGET`] bytes of heap.
#[test]
#[cfg_attr(debug_assertions, ignore = "memory budget; run in release")]
fn an_idle_session_stays_within_its_heap_budget() {
    let (_app, _id, before) = idle_session();
    let session = settled_heap().saturating_sub(before);
    assert_within("an idle session", session, IDLE_SESSION_HEAP_BUDGET);
}

/// The same session with 10 000 lines of 80 columns in its scrollback holds
/// at most [`FULL_SCROLLBACK_SESSION_HEAP_BUDGET`] bytes of heap.
#[test]
#[cfg_attr(debug_assertions, ignore = "memory budget; run in release")]
fn a_full_scrollback_stays_within_its_heap_budget() {
    let (app, id, before) = idle_session();
    // `$((6 * 7))` keeps the echoed command line from matching the marker.
    let command = format!(
        "for ((i = 0; i < {PRINTED_LINES}; i++)); do printf '%080d\\n' \"$i\"; done; \
         echo \"done-$((6 * 7))\"\n"
    );
    app.write(id, command.as_bytes());
    app.wait_for_prompt(id, "prompt after the lines", |above, scrollback| {
        above.last().is_some_and(|row| row == "done-42") && scrollback == SCROLLBACK_LINES
    });
    let session = settled_heap().saturating_sub(before);
    assert_within(
        "a session with a full scrollback",
        session,
        FULL_SCROLLBACK_SESSION_HEAP_BUDGET,
    );
}

// ---------------------------------------------------------------------------
// Startup budget
// ---------------------------------------------------------------------------

/// One startup in this process: `tessera_app_new`, the first session's
/// spawn and its first presented frame, timed together.
fn one_startup() -> Duration {
    configure();
    run_as_the_main_thread();
    let layer = objc::Layer::new();
    let start = Instant::now();
    let app = Handle::new();
    let id = app.spawn_bash();
    app.first_frame(id, &layer);
    let took = start.elapsed();
    drop(app);
    took
}

/// Runs this test in a fresh process, which measures one startup, and
/// returns what it measured.
fn startup_in_a_child() -> Duration {
    let name = "startup_to_the_first_frame_stays_within_its_budget";
    let mut child = Command::new(std::env::current_exe().expect("this test binary"))
        .args([name, "--exact", "--nocapture", "--include-ignored"])
        .env(CHILD_VAR, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start the child");
    let deadline = Instant::now() + 2 * TIMEOUT;
    while child.try_wait().expect("poll the child").is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the startup child did not finish in {:?}", 2 * TIMEOUT);
        }
        thread::sleep(POLL);
    }
    let output = child.wait_with_output().expect("the child's output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "the child failed: {stdout}");
    let nanos = stdout
        .lines()
        .find_map(|line| line.strip_prefix(STARTUP_LINE))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or_else(|| panic!("no {STARTUP_LINE:?} line in the child's output: {stdout}"));
    Duration::from_nanos(nanos)
}

/// `tessera_app_new` + the first session + its first frame, rendered
/// off-screen, take at most [`STARTUP_BUDGET`], best of [`STARTUP_RUNS`]
/// fresh processes (so process-wide caches such as the GPU and the fonts
/// start cold every time).
#[test]
#[cfg_attr(debug_assertions, ignore = "timing bar; run in release")]
fn startup_to_the_first_frame_stays_within_its_budget() {
    if std::env::var_os(CHILD_VAR).is_some() {
        let took = one_startup();
        println!("{STARTUP_LINE}{}", took.as_nanos());
        return;
    }
    if std::env::var_os("TESSERA_SKIP_GPU_TESTS").is_some() {
        eprintln!("TESSERA_SKIP_GPU_TESTS is set; skipping");
        return;
    }
    let runs: Vec<Duration> = (0..STARTUP_RUNS).map(|_| startup_in_a_child()).collect();
    let best = runs.iter().min().copied().expect("at least one run");
    eprintln!("startup: best {best:?} of {runs:?} (budget {STARTUP_BUDGET:?})");
    assert!(
        best <= STARTUP_BUDGET,
        "startup to the first frame took {best:?} at best ({runs:?}), over its budget of \
         {STARTUP_BUDGET:?} (main's measurement plus 20 %); fix the regression or, if the \
         cost is intended, re-measure and raise the budget with the new value"
    );
}
