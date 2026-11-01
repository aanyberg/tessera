//! `tessera_session_foreground_process` called as a frontend would
//! (issue #34): an interactive `/bin/sh` reports no job, then `sleep` while
//! `sleep 100` runs, then none again after Ctrl-C; `NULL` pointers and
//! unknown or closed sessions fail and zero `*out`.
//!
//! Waits are bounded by attempts (each sleeps [`POLL`]), not by a clock.

#![cfg(unix)]

mod common;

use std::ffi::{CStr, c_char};
use std::ptr;
use std::thread;
use std::time::Duration;

use tessera_ffi::{
    App, Error, FOREGROUND_NAME_LEN, ForegroundProcess, SpawnOptions, tessera_app_free,
    tessera_app_new, tessera_session_close, tessera_session_foreground_process,
    tessera_session_spawn, tessera_session_write,
};

/// Pause between two polls.
const POLL: Duration = Duration::from_millis(10);
/// Polls before a wait gives up: 10 s on an idle machine. The job shows up within
/// the first few polls there (the 2 s bar of #34); the rest is headroom
/// for a loaded machine, as the other wall-clock bars keep (#190).
const ATTEMPTS: usize = 1000;

/// An app with an interactive `/bin/sh` session.
fn app_with_shell() -> (*mut App, u64) {
    common::isolate_home();
    common::hermetic_config_dir();
    let mut app: *mut App = ptr::null_mut();
    // SAFETY: `app` is writable storage for one pointer.
    assert_eq!(unsafe { tessera_app_new(&raw mut app) }, Error::Ok);
    let args: [*const c_char; 1] = [c"-i".as_ptr()];
    let options = SpawnOptions {
        program: c"/bin/sh".as_ptr(),
        args: args.as_ptr(),
        arg_count: args.len(),
        cwd: ptr::null(),
        rows: 24,
        cols: 80,
        width_px: 0,
        height_px: 0,
    };
    let mut id = 0;
    // SAFETY: live handle; valid options; `id` is writable.
    assert_eq!(
        unsafe { tessera_session_spawn(app, &raw const options, &raw mut id) },
        Error::Ok
    );
    (app, id)
}

fn job(app: *mut App, id: u64) -> (Error, ForegroundProcess) {
    let mut out = ForegroundProcess::default();
    // SAFETY: `app` is NULL or live; `out` is writable.
    let status = unsafe { tessera_session_foreground_process(app, id, &raw mut out) };
    (status, out)
}

fn name(job: &ForegroundProcess) -> String {
    // SAFETY: the call NUL-terminates `name` within its 65 bytes.
    unsafe { CStr::from_ptr(job.name.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn write(app: *mut App, id: u64, bytes: &[u8]) {
    // SAFETY: live handle; `bytes` is readable for its length.
    assert_eq!(
        unsafe { tessera_session_write(app, id, bytes.as_ptr(), bytes.len()) },
        Error::Ok
    );
}

fn wait_for(
    app: *mut App,
    id: u64,
    done: impl Fn(&ForegroundProcess) -> bool,
) -> ForegroundProcess {
    let mut last = ForegroundProcess::default();
    for _ in 0..ATTEMPTS {
        let (status, job) = job(app, id);
        assert_eq!(status, Error::Ok);
        if done(&job) {
            return job;
        }
        last = job;
        thread::sleep(POLL);
    }
    panic!("foreground job still {last:?} ({:?})", name(&last));
}

#[test]
fn a_running_job_is_reported_with_its_name() {
    let (app, id) = app_with_shell();
    let idle = wait_for(app, id, |job| !job.running);
    assert_eq!(
        idle,
        ForegroundProcess::default(),
        "an idle shell zeroes *out"
    );

    write(app, id, b"sleep 100\n");
    let busy = wait_for(app, id, |job| job.running && name(job) == "sleep");
    assert!(busy.pid > 0);
    assert!(
        busy.name[5..].iter().all(|&b| b == 0),
        "bytes after the name are zero"
    );

    write(app, id, b"\x03");
    let idle = wait_for(app, id, |job| !job.running);
    assert_eq!(idle, ForegroundProcess::default());

    // SAFETY: live handle and session; the handle is freed once.
    unsafe {
        assert_eq!(tessera_session_close(app, id), Error::Ok);
        tessera_app_free(app);
    }
}

#[test]
fn failures_zero_the_output() {
    let (app, id) = app_with_shell();
    write(app, id, b"sleep 100\n");
    wait_for(app, id, |job| job.running);

    let dirty = ForegroundProcess {
        running: true,
        pid: 7,
        name: [b'x'.cast_signed(); FOREGROUND_NAME_LEN + 1],
    };
    let mut out = dirty;
    // SAFETY: `out` is writable; a NULL app is allowed.
    let status = unsafe { tessera_session_foreground_process(ptr::null_mut(), id, &raw mut out) };
    assert_eq!(status, Error::NullPointer);
    assert_eq!(out, ForegroundProcess::default());

    let mut out = dirty;
    // SAFETY: live handle; `out` is writable.
    let status = unsafe { tessera_session_foreground_process(app, id + 1000, &raw mut out) };
    assert_eq!(status, Error::SessionNotFound);
    assert_eq!(out, ForegroundProcess::default());

    // SAFETY: live handle; a NULL `out` is allowed.
    let status = unsafe { tessera_session_foreground_process(app, id, ptr::null_mut()) };
    assert_eq!(status, Error::NullPointer);

    // SAFETY: live handle and session.
    assert_eq!(unsafe { tessera_session_close(app, id) }, Error::Ok);
    let mut out = dirty;
    // SAFETY: live handle; `out` is writable.
    let status = unsafe { tessera_session_foreground_process(app, id, &raw mut out) };
    assert_eq!(
        status,
        Error::SessionNotFound,
        "a closed session is unknown"
    );
    assert_eq!(out, ForegroundProcess::default());

    // SAFETY: the handle is freed once.
    unsafe { tessera_app_free(app) };
}
