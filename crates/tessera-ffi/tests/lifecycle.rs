//! End-to-end tests of the application and session lifecycle, calling the
//! `extern "C"` functions exactly as a frontend would.
//!
//! Sessions run `/bin/sh` in real PTYs; every wait is bounded by
//! [`TIMEOUT`].

#![cfg(unix)]

mod common;

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::process::Command;
use std::ptr;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use tessera_ffi::{
    App, Callbacks, Error, SpawnOptions, tessera_app_config_path, tessera_app_free,
    tessera_app_new, tessera_app_set_callbacks, tessera_session_clear_scrollback,
    tessera_session_close, tessera_session_resize, tessera_session_spawn, tessera_session_write,
    tessera_test_panic,
};

const TIMEOUT: Duration = Duration::from_secs(5);

/// What the callbacks observed.
#[derive(Debug, Default)]
struct Seen {
    outputs: usize,
    /// Sessions that produced at least one `on_session_output`.
    output_from: Vec<u64>,
    bells: usize,
    titles: Vec<String>,
    /// `(session_id, code, signal)` per exit.
    exits: Vec<(u64, i32, i32)>,
}

/// The `user_data` of every test: callbacks record into it and wake waiters.
#[derive(Default)]
struct Recorder {
    seen: Mutex<Seen>,
    changed: Condvar,
}

impl Recorder {
    fn seen(&self) -> MutexGuard<'_, Seen> {
        self.seen.lock().unwrap()
    }

    fn record(&self, f: impl FnOnce(&mut Seen)) {
        f(&mut self.seen());
        self.changed.notify_all();
    }

    /// Waits until `done` holds, failing the test after [`TIMEOUT`].
    fn wait_for(&self, what: &str, mut done: impl FnMut(&Seen) -> bool) -> MutexGuard<'_, Seen> {
        let (seen, timeout) = self
            .changed
            .wait_timeout_while(self.seen(), TIMEOUT, |seen| !done(seen))
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "timed out waiting for {what}: {seen:?}"
        );
        seen
    }

    fn wait_exit(&self, id: u64) -> (i32, i32) {
        let seen = self.wait_for("session exit", |seen| {
            seen.exits.iter().any(|&(exit_id, ..)| exit_id == id)
        });
        let &(_, code, signal) = seen.exits.iter().find(|exit| exit.0 == id).unwrap();
        (code, signal)
    }

    fn callbacks(&self) -> Callbacks {
        Callbacks {
            user_data: ptr::from_ref(self).cast_mut().cast(),
            on_session_output: Some(on_output),
            on_session_title: Some(on_title),
            on_session_bell: Some(on_bell),
            on_session_exit: Some(on_exit),
            on_appearance_changed: None,
        }
    }
}

/// # Safety
///
/// `user_data` is a `Recorder` that outlives the application.
unsafe fn recorder<'a>(user_data: *mut c_void) -> &'a Recorder {
    // SAFETY: every test installs a `Recorder` it keeps alive past
    // `tessera_app_free`.
    unsafe { &*user_data.cast::<Recorder>() }
}

unsafe extern "C" fn on_output(user_data: *mut c_void, id: u64) {
    // SAFETY: see `recorder`.
    unsafe { recorder(user_data) }.record(|seen| {
        seen.outputs += 1;
        if !seen.output_from.contains(&id) {
            seen.output_from.push(id);
        }
    });
}

unsafe extern "C" fn on_title(user_data: *mut c_void, _id: u64, title: *const c_char) {
    // SAFETY: the core passes a NUL-terminated string valid for the call.
    let title = unsafe { CStr::from_ptr(title) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: see `recorder`.
    unsafe { recorder(user_data) }.record(|seen| seen.titles.push(title));
}

unsafe extern "C" fn on_bell(user_data: *mut c_void, _id: u64) {
    // SAFETY: see `recorder`.
    unsafe { recorder(user_data) }.record(|seen| seen.bells += 1);
}

unsafe extern "C" fn on_exit(user_data: *mut c_void, id: u64, code: i32, signal: i32) {
    // SAFETY: see `recorder`.
    unsafe { recorder(user_data) }.record(|seen| seen.exits.push((id, code, signal)));
}

/// An application handle freed on drop, so a failing assertion does not
/// leak sessions.
struct Handle(*mut App);

impl Handle {
    fn new() -> Self {
        let mut app = ptr::null_mut();
        // SAFETY: `app` is writable storage for one pointer.
        common::hermetic_config_dir();
        assert_eq!(unsafe { tessera_app_new(&raw mut app) }, Error::Ok);
        assert!(!app.is_null());
        Self(app)
    }

    /// A handle whose callbacks record into `recorder`.
    fn observed(recorder: &Recorder) -> Self {
        let handle = Self::new();
        let callbacks = recorder.callbacks();
        // SAFETY: live handle; the table is valid and `recorder` outlives
        // the handle in every test.
        assert_eq!(
            unsafe { tessera_app_set_callbacks(handle.0, &raw const callbacks) },
            Error::Ok
        );
        handle
    }

    fn spawn(&self, program: &str, args: &[&str]) -> Result<u64, Error> {
        let program = CString::new(program).unwrap();
        let args: Vec<CString> = args.iter().map(|arg| CString::new(*arg).unwrap()).collect();
        let arg_ptrs: Vec<*const c_char> = args.iter().map(|arg| arg.as_ptr()).collect();
        let options = SpawnOptions {
            program: program.as_ptr(),
            args: if arg_ptrs.is_empty() {
                ptr::null()
            } else {
                arg_ptrs.as_ptr()
            },
            arg_count: arg_ptrs.len(),
            ..default_options()
        };
        self.spawn_with(&options)
    }

    fn spawn_with(&self, options: &SpawnOptions) -> Result<u64, Error> {
        let mut id = u64::MAX;
        // SAFETY: live handle, valid options, writable id.
        let code = unsafe { tessera_session_spawn(self.0, options, &raw mut id) };
        if code == Error::Ok {
            assert_ne!(id, 0);
            Ok(id)
        } else {
            assert_eq!(id, 0, "out_id is zeroed on failure");
            Err(code)
        }
    }

    fn write(&self, id: u64, bytes: &[u8]) -> Error {
        // SAFETY: live handle; `bytes` is readable for its length.
        unsafe { tessera_session_write(self.0, id, bytes.as_ptr(), bytes.len()) }
    }

    fn close(&self, id: u64) -> Error {
        // SAFETY: live handle.
        unsafe { tessera_session_close(self.0, id) }
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

/// The raw handle, shareable with another thread for
/// `tessera_session_write`, which the contract allows from any thread.
#[derive(Clone, Copy)]
struct SendPtr(*mut App);
// SAFETY: only used for `tessera_session_write`, documented as callable
// from any thread, while the owning test keeps the handle alive.
unsafe impl Send for SendPtr {}

fn default_options() -> SpawnOptions {
    SpawnOptions {
        program: ptr::null(),
        args: ptr::null(),
        arg_count: 0,
        cwd: ptr::null(),
        rows: 24,
        cols: 80,
        width_px: 0,
        height_px: 0,
    }
}

/// Number of threads in this process, from `ps -M` (one header line plus
/// one line per thread).
fn thread_count() -> usize {
    let output = Command::new("ps")
        .args(["-M", "-p", &std::process::id().to_string()])
        .output()
        .expect("run ps");
    String::from_utf8_lossy(&output.stdout).lines().count() - 1
}

/// Thread counts are only meaningful when this test is alone in its
/// process, which is how nextest runs every test; `cargo test` runs tests
/// concurrently on threads of one process.
fn alone_in_process() -> bool {
    std::env::var_os("NEXTEST").is_some()
}

#[test]
fn app_create_free_round_trip() {
    Handle::new().free();
    // Freeing NULL is a no-op.
    // SAFETY: NULL is explicitly allowed.
    unsafe { tessera_app_free(ptr::null_mut()) };
}

#[test]
fn shell_exits_after_written_exit() {
    let recorder = Recorder::default();
    let app = Handle::observed(&recorder);
    let started = Instant::now();
    let id = app.spawn("/bin/sh", &[]).unwrap();
    assert_eq!(app.write(id, b"exit\n"), Error::Ok);
    assert_eq!(recorder.wait_exit(id), (0, 0));
    assert!(started.elapsed() < TIMEOUT);
    assert!(
        recorder.seen().outputs >= 1,
        "on_session_output never fired"
    );
    // The exited session's id stays valid until it is closed.
    assert_eq!(app.write(id, b"x"), Error::Io);
    assert_eq!(app.close(id), Error::Ok);
    assert_eq!(app.close(id), Error::SessionNotFound);
    app.free();
}

#[test]
fn exit_codes_and_signals_are_reported() {
    let recorder = Recorder::default();
    let app = Handle::observed(&recorder);
    let code = app.spawn("/bin/sh", &["-c", "exit 7"]).unwrap();
    let killed = app.spawn("/bin/sh", &["-c", "kill -9 $$"]).unwrap();
    assert_eq!(recorder.wait_exit(code), (7, 0));
    assert_eq!(recorder.wait_exit(killed), (-1, 9));
}

#[test]
fn bell_is_reported() {
    let recorder = Recorder::default();
    let app = Handle::observed(&recorder);
    let id = app.spawn("/bin/sh", &["-c", "printf '\\a'"]).unwrap();
    recorder.wait_exit(id);
    assert!(recorder.seen().bells >= 1);
    assert_eq!(recorder.seen().titles, Vec::<String>::new());
}

#[test]
fn write_is_allowed_from_another_thread() {
    let recorder = Recorder::default();
    let app = Handle::observed(&recorder);
    let id = app.spawn("/bin/sh", &[]).unwrap();
    let shared = SendPtr(app.0);
    let writer = thread::spawn(move || {
        let shared = shared;
        let bytes = b"exit 5\n";
        // SAFETY: the handle outlives the thread (joined below) and write
        // may be called from any thread.
        unsafe { tessera_session_write(shared.0, id, bytes.as_ptr(), bytes.len()) }
    });
    assert_eq!(writer.join().unwrap(), Error::Ok);
    assert_eq!(recorder.wait_exit(id), (5, 0));
}

#[test]
fn resize_and_clear_scrollback_accept_live_sessions() {
    let recorder = Recorder::default();
    let app = Handle::observed(&recorder);
    let id = app.spawn("/bin/sh", &[]).unwrap();
    // SAFETY: live handle throughout.
    unsafe {
        assert_eq!(
            tessera_session_resize(app.0, id, 40, 120, 960, 800),
            Error::Ok
        );
        assert_eq!(
            tessera_session_resize(app.0, id, 0, 120, 0, 0),
            Error::InvalidArgument
        );
        assert_eq!(
            tessera_session_resize(app.0, id, u16::MAX, u16::MAX, 0, 0),
            Error::InvalidArgument,
            "an oversized grid is refused instead of allocated"
        );
        assert_eq!(tessera_session_clear_scrollback(app.0, id), Error::Ok);
    }
    // The shell sees the new size.
    assert_eq!(
        app.write(id, b"[ \"$(stty size)\" = '40 120' ]; exit $?\n"),
        Error::Ok
    );
    assert_eq!(recorder.wait_exit(id), (0, 0));
}

#[test]
fn null_pointers_are_rejected() {
    let app = Handle::new();
    let options = default_options();
    let callbacks = Recorder::default().callbacks();
    let mut id = 0;
    let mut len = 0;
    let null = ptr::null_mut::<App>();
    // SAFETY: every call passes NULL where it is being tested, and valid
    // pointers elsewhere.
    unsafe {
        assert_eq!(tessera_app_new(ptr::null_mut()), Error::NullPointer);
        assert_eq!(
            tessera_app_set_callbacks(null, &raw const callbacks),
            Error::NullPointer
        );
        assert_eq!(
            tessera_session_spawn(null, &raw const options, &raw mut id),
            Error::NullPointer
        );
        assert_eq!(
            tessera_session_spawn(app.0, ptr::null(), &raw mut id),
            Error::NullPointer
        );
        assert_eq!(
            tessera_session_spawn(app.0, &raw const options, ptr::null_mut()),
            Error::NullPointer
        );
        assert_eq!(tessera_session_close(null, 1), Error::NullPointer);
        assert_eq!(
            tessera_session_write(null, 1, b"x".as_ptr(), 1),
            Error::NullPointer
        );
        assert_eq!(
            tessera_session_write(app.0, 1, ptr::null(), 1),
            Error::NullPointer
        );
        assert_eq!(
            tessera_session_resize(null, 1, 24, 80, 0, 0),
            Error::NullPointer
        );
        assert_eq!(
            tessera_session_clear_scrollback(null, 1),
            Error::NullPointer
        );
        assert_eq!(
            tessera_app_config_path(null, ptr::null_mut(), 0, &raw mut len),
            Error::NullPointer
        );
        assert_eq!(
            tessera_app_config_path(app.0, ptr::null_mut(), 8, &raw mut len),
            Error::NullPointer
        );
        // Removing the callbacks with a NULL table is allowed.
        assert_eq!(tessera_app_set_callbacks(app.0, ptr::null()), Error::Ok);
    }
}

#[test]
fn unknown_session_ids_are_rejected() {
    let app = Handle::new();
    for id in [0, 1, 42, u64::MAX] {
        assert_eq!(app.close(id), Error::SessionNotFound);
        assert_eq!(app.write(id, b"x"), Error::SessionNotFound);
        // SAFETY: live handle.
        unsafe {
            assert_eq!(
                tessera_session_resize(app.0, id, 24, 80, 0, 0),
                Error::SessionNotFound
            );
            assert_eq!(
                tessera_session_clear_scrollback(app.0, id),
                Error::SessionNotFound
            );
        }
    }
    // An empty write still names a session.
    // SAFETY: live handle; NULL bytes are allowed with a zero length.
    assert_eq!(
        unsafe { tessera_session_write(app.0, 9, ptr::null(), 0) },
        Error::SessionNotFound
    );
}

#[test]
fn bad_spawn_requests_fail_cleanly() {
    let app = Handle::new();
    assert_eq!(
        app.spawn("/nonexistent/tessera-no-such-program", &[]),
        Err(Error::SpawnFailed)
    );
    assert_eq!(app.spawn("", &[]), Err(Error::InvalidArgument));
    let program = c"/bin/sh";
    let zero_rows = SpawnOptions {
        program: program.as_ptr(),
        rows: 0,
        ..default_options()
    };
    assert_eq!(app.spawn_with(&zero_rows), Err(Error::InvalidArgument));
    let missing_args = SpawnOptions {
        program: program.as_ptr(),
        arg_count: 2,
        ..default_options()
    };
    assert_eq!(app.spawn_with(&missing_args), Err(Error::NullPointer));
    let null_arg = [ptr::null::<c_char>()];
    let null_entry = SpawnOptions {
        program: program.as_ptr(),
        args: null_arg.as_ptr(),
        arg_count: 1,
        ..default_options()
    };
    assert_eq!(app.spawn_with(&null_entry), Err(Error::NullPointer));
    // The login shell takes no arguments, so `args` is never read for it.
    let login_shell_with_bogus_args = SpawnOptions {
        args: ptr::null(),
        arg_count: 2,
        ..default_options()
    };
    let shell = app
        .spawn_with(&login_shell_with_bogus_args)
        .expect("args are ignored for the login shell");
    assert_eq!(app.close(shell), Error::Ok);
    // The application is still fine.
    let recorder = Recorder::default();
    let app = Handle::observed(&recorder);
    let id = app.spawn("/bin/sh", &["-c", "exit 0"]).unwrap();
    assert_eq!(recorder.wait_exit(id), (0, 0));
}

#[test]
fn spawn_sizes_are_bounded_like_resize() {
    assert_eq!(
        (tessera_ffi::MAX_ROWS, tessera_ffi::MAX_COLS),
        (1024, 2048),
        "the header documents these values"
    );
    let recorder = Recorder::default();
    let app = Handle::observed(&recorder);
    let program = c"/bin/sh";
    let script = c"[ \"$(stty size)\" = '1024 2048' ] && exit 4";
    let args = [c"-c".as_ptr(), script.as_ptr()];
    let sized = |rows, cols| SpawnOptions {
        program: program.as_ptr(),
        args: args.as_ptr(),
        arg_count: args.len(),
        rows,
        cols,
        width_px: u16::MAX,
        height_px: u16::MAX,
        ..default_options()
    };
    for (rows, cols) in [
        (tessera_ffi::MAX_ROWS + 1, 80),
        (24, tessera_ffi::MAX_COLS + 1),
        (u16::MAX, u16::MAX),
        (u16::MAX, 0),
    ] {
        assert_eq!(
            app.spawn_with(&sized(rows, cols)),
            Err(Error::InvalidArgument),
            "{rows}x{cols}"
        );
    }
    // The bound itself spawns, with any pixel size, and the child sees it.
    let id = app
        .spawn_with(&sized(tessera_ffi::MAX_ROWS, tessera_ffi::MAX_COLS))
        .unwrap();
    assert_eq!(recorder.wait_exit(id), (4, 0));
    assert_eq!(recorder.seen().exits.len(), 1, "only the bounded spawn ran");
}

#[test]
fn resize_accepts_the_bound_and_refuses_one_more() {
    let recorder = Recorder::default();
    let app = Handle::observed(&recorder);
    let id = app.spawn("/bin/sh", &[]).unwrap();
    let (rows, cols) = (tessera_ffi::MAX_ROWS, tessera_ffi::MAX_COLS);
    // SAFETY: live handle throughout.
    unsafe {
        assert_eq!(
            tessera_session_resize(app.0, id, rows, cols, u16::MAX, u16::MAX),
            Error::Ok
        );
        for (r, c) in [(rows + 1, cols), (rows, cols + 1)] {
            assert_eq!(
                tessera_session_resize(app.0, id, r, c, 0, 0),
                Error::InvalidArgument,
                "{r}x{c}"
            );
        }
    }
    assert_eq!(
        app.write(id, b"[ \"$(stty size)\" = '1024 2048' ]; exit $?\n"),
        Error::Ok
    );
    assert_eq!(recorder.wait_exit(id), (0, 0));
}

#[test]
fn oversized_spawn_arguments_fail_without_starting_anything() {
    let recorder = Recorder::default();
    let app = Handle::observed(&recorder);
    let alone = alone_in_process();
    let threads = thread_count();
    let big = "x".repeat(5 * 1024 * 1024);
    assert_eq!(
        app.spawn("/bin/sh", &["-c", "exit 0", "argv0", &big]),
        Err(Error::SpawnFailed)
    );
    // Many small strings: each costs a pointer as well as its bytes.
    let many: Vec<&str> = std::iter::repeat_n("", 200_000).collect();
    assert_eq!(app.spawn("/bin/sh", &many), Err(Error::SpawnFailed));
    if alone {
        assert_eq!(thread_count(), threads, "a refused spawn left a thread");
    }
    // Nothing was forked: no exit is ever reported.
    thread::sleep(Duration::from_millis(100));
    assert_eq!(recorder.seen().exits, []);
    // The application still spawns.
    let id = app.spawn("/bin/sh", &["-c", "exit 5"]).unwrap();
    assert_eq!(recorder.wait_exit(id), (5, 0));
}

#[test]
fn a_missing_cwd_falls_back_to_the_home_directory() {
    let recorder = Recorder::default();
    let app = Handle::observed(&recorder);
    let args = [
        c"-c".as_ptr(),
        c"test \"$(pwd -P)\" = \"$(cd ~ && pwd -P)\"".as_ptr(),
    ];
    let options = SpawnOptions {
        program: c"/bin/sh".as_ptr(),
        args: args.as_ptr(),
        arg_count: args.len(),
        cwd: c"/nonexistent/tessera-no-such-dir".as_ptr(),
        ..default_options()
    };
    let id = app.spawn_with(&options).unwrap();
    assert_eq!(recorder.wait_exit(id), (0, 0), "child did not start in ~");
}

#[test]
fn a_caught_panic_leaves_the_app_usable() {
    let recorder = Recorder::default();
    let app = Handle::observed(&recorder);
    // SAFETY: live handle.
    assert_eq!(unsafe { tessera_test_panic(app.0) }, Error::Panic);
    // SAFETY: NULL is rejected before the hook runs.
    assert_eq!(
        unsafe { tessera_test_panic(ptr::null_mut()) },
        Error::NullPointer
    );
    let id = app.spawn("/bin/sh", &[]).unwrap();
    assert_eq!(app.write(id, b"exit 3\n"), Error::Ok);
    assert_eq!(recorder.wait_exit(id), (3, 0));
    assert_eq!(app.close(id), Error::Ok);
    app.free();
}

#[test]
fn freeing_with_live_sessions_is_prompt_and_joins_threads() {
    let recorder = Recorder::default();
    let before = thread_count();
    let app = Handle::observed(&recorder);
    let started = Instant::now();
    let ids: Vec<u64> = (0..3).map(|_| app.spawn("/bin/sh", &[]).unwrap()).collect();
    // Several children ignore SIGHUP, so freeing has to escalate to
    // SIGKILL; their grace periods must overlap rather than add up. Each
    // child's first output proves its trap is installed.
    let stubborn: Vec<u64> = (0..3)
        .map(|_| {
            app.spawn(
                "/bin/sh",
                &["-c", "trap '' HUP; printf ready; while :; do sleep 1; done"],
            )
            .unwrap()
        })
        .collect();
    drop(
        recorder.wait_for("the stubborn children to ignore SIGHUP", |seen| {
            stubborn.iter().all(|id| seen.output_from.contains(id))
        }),
    );
    if alone_in_process() {
        assert!(thread_count() > before);
    }
    let freeing = Instant::now();
    app.free();
    let free_took = freeing.elapsed();
    assert!(started.elapsed() < TIMEOUT, "took {:?}", started.elapsed());
    // The SIGHUP grace period ran out and the core escalated to SIGKILL.
    assert!(
        free_took >= Duration::from_secs(1),
        "free took {free_took:?}"
    );
    // One shared hangup grace period (1 s) plus prompt kills, not one
    // grace period per stubborn child.
    assert!(
        free_took < Duration::from_millis(1_800),
        "free took {free_took:?}"
    );
    // Closed sessions never report an exit, before or after the free.
    let seen = recorder.seen();
    assert!(
        seen.exits
            .iter()
            .all(|(id, ..)| !ids.contains(id) && !stubborn.contains(id)),
        "{seen:?}"
    );
    drop(seen);
    // tessera-pty's own threads finish just after the session reader that
    // waits for them; give them a moment, then expect none left over.
    if alone_in_process() {
        let deadline = Instant::now() + Duration::from_secs(1);
        while thread_count() > before && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(thread_count(), before, "threads leaked");
    }
}

#[test]
fn close_returns_at_once_and_free_waits_for_the_closing_children() {
    let recorder = Recorder::default();
    let before = thread_count();
    let app = Handle::observed(&recorder);
    let stubborn: Vec<u64> = (0..5)
        .map(|_| {
            app.spawn(
                "/bin/sh",
                &["-c", "trap '' HUP; printf ready; while :; do sleep 1; done"],
            )
            .unwrap()
        })
        .collect();
    drop(
        recorder.wait_for("the stubborn children to ignore SIGHUP", |seen| {
            stubborn.iter().all(|id| seen.output_from.contains(id))
        }),
    );
    let closing = Instant::now();
    for &id in &stubborn {
        assert_eq!(app.close(id), Error::Ok);
    }
    let close_took = closing.elapsed();
    assert!(
        close_took < Duration::from_millis(5),
        "5 closes took {close_took:?}"
    );
    for &id in &stubborn {
        assert_eq!(app.close(id), Error::SessionNotFound);
        assert_eq!(app.write(id, b"x"), Error::SessionNotFound);
    }
    // Freeing waits for the background closes, which share one grace.
    app.free();
    let free_took = closing.elapsed();
    assert!(
        free_took >= Duration::from_secs(1),
        "freed {free_took:?} after closing"
    );
    assert!(
        free_took < Duration::from_millis(1_800),
        "freed {free_took:?} after closing"
    );
    assert_eq!(
        recorder.seen().exits,
        Vec::new(),
        "closed sessions report no exit"
    );
    if alone_in_process() {
        let deadline = Instant::now() + Duration::from_secs(1);
        while thread_count() > before && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(thread_count(), before, "threads leaked");
    }
}

/// Set in [`record_qos`]'s slot next to the class, so a thread with no
/// class (0) still counts as recorded.
#[cfg(target_os = "macos")]
const QOS_RECORDED: u64 = 1 << 32;

/// Records the quality-of-service class of the thread an output callback
/// runs on, with [`QOS_RECORDED`], into the `AtomicU64` behind `user_data`.
#[cfg(target_os = "macos")]
unsafe extern "C" fn record_qos(user_data: *mut c_void, _id: u64) {
    unsafe extern "C" {
        fn pthread_self() -> *mut c_void;
        fn pthread_get_qos_class_np(thread: *mut c_void, qos: *mut u32, relative: *mut i32) -> i32;
    }
    let mut qos = 0;
    let mut relative = 0;
    // SAFETY: the calling thread's own handle and two writable integers.
    let status =
        unsafe { pthread_get_qos_class_np(pthread_self(), &raw mut qos, &raw mut relative) };
    assert_eq!(status, 0);
    // SAFETY: the test passes a live `AtomicU64`.
    let slot = unsafe { &*user_data.cast::<AtomicU64>() };
    slot.store(QOS_RECORDED | u64::from(qos), Ordering::SeqCst);
}

#[cfg(target_os = "macos")]
#[test]
fn session_threads_run_at_user_initiated_qos() {
    /// `QOS_CLASS_USER_INITIATED` from `<sys/qos.h>`.
    const QOS_CLASS_USER_INITIATED: u64 = 0x19;
    let qos = AtomicU64::new(0);
    // A second app in the process installs nothing new and changes nothing.
    let first = Handle::new();
    let app = Handle::new();
    first.free();
    let callbacks = Callbacks {
        user_data: ptr::from_ref(&qos).cast_mut().cast(),
        on_session_output: Some(record_qos),
        on_session_title: None,
        on_session_bell: None,
        on_session_exit: None,
        on_appearance_changed: None,
    };
    // SAFETY: live handle; valid table; `qos` outlives the handle.
    assert_eq!(
        unsafe { tessera_app_set_callbacks(app.0, &raw const callbacks) },
        Error::Ok
    );
    let id = app.spawn("/bin/sh", &["-c", "printf x; sleep 5"]).unwrap();
    let deadline = Instant::now() + TIMEOUT;
    while qos.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "no output callback");
        thread::sleep(Duration::from_millis(2));
    }
    // The callback runs on the session's reader thread.
    assert_eq!(
        qos.load(Ordering::SeqCst),
        QOS_RECORDED | QOS_CLASS_USER_INITIATED
    );
    assert_eq!(app.close(id), Error::Ok);
    app.free();
}

#[test]
fn close_stops_callbacks_and_replaced_tables_go_quiet() {
    let first = Recorder::default();
    let second = Recorder::default();
    let app = Handle::observed(&first);
    let id = app.spawn("/bin/sh", &[]).unwrap();
    drop(first.wait_for("prompt output", |seen| seen.outputs >= 1));
    let callbacks = second.callbacks();
    // SAFETY: live handle, valid table.
    assert_eq!(
        unsafe { tessera_app_set_callbacks(app.0, &raw const callbacks) },
        Error::Ok
    );
    let outputs_before = first.seen().outputs;
    assert_eq!(app.write(id, b"echo hi\n"), Error::Ok);
    drop(second.wait_for("output after replacing callbacks", |seen| seen.outputs >= 1));
    assert_eq!(first.seen().outputs, outputs_before);
    assert_eq!(app.close(id), Error::Ok);
    let seen = second.seen();
    assert_eq!(seen.exits, Vec::new(), "closed sessions report no exit");
}

#[test]
fn config_path_round_trips_through_a_buffer() {
    let app = Handle::new();
    let mut len = 0;
    // SAFETY: live handle; a NULL buffer with zero length queries the size.
    assert_eq!(
        unsafe { tessera_app_config_path(app.0, ptr::null_mut(), 0, &raw mut len) },
        Error::Ok
    );
    assert!(len > 0);
    let mut buf = vec![0x7f_u8; len + 1];
    // SAFETY: live handle; `buf` has `len + 1` writable bytes.
    assert_eq!(
        unsafe { tessera_app_config_path(app.0, buf.as_mut_ptr().cast(), len, ptr::null_mut()) },
        Error::InvalidArgument,
        "no room for the NUL"
    );
    assert!(
        buf.iter().all(|&byte| byte == 0x7f),
        "buffer left untouched"
    );
    // SAFETY: as above, now with room for the NUL.
    assert_eq!(
        unsafe {
            tessera_app_config_path(app.0, buf.as_mut_ptr().cast(), buf.len(), ptr::null_mut())
        },
        Error::Ok
    );
    let path = CStr::from_bytes_with_nul(&buf).unwrap().to_str().unwrap();
    assert_eq!(path.len(), len);
    // `TESSERA_CONFIG_DIR` (set by the test harness) replaces
    // `~/Library/Application Support/Tessera`.
    let expected = common::hermetic_config_dir().join("config.toml");
    assert_eq!(Path::new(path), expected, "{path}");
}
