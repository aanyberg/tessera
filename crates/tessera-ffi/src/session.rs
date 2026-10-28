//! Sessions: spawning, input, resizing and closing.

use std::ffi::{CStr, OsStr, c_char};
use std::os::unix::ffi::OsStrExt;
use std::slice;

use tessera_app_core::{PtySize, SessionId, SpawnOptions as CoreSpawnOptions};

use crate::app::App;
use crate::error::{self, Error};
use crate::panic::ffi_guard;

/// What `tessera_session_spawn` starts.
///
/// Every pointer is borrowed for the duration of the spawn call only; the
/// core copies what it needs. Strings are NUL-terminated byte strings
/// (file-system encoding, UTF-8 on macOS).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SpawnOptions {
    /// Program to run: an absolute path, or a name looked up in `PATH`.
    /// `NULL` runs the user's login shell (`$SHELL`, or the password
    /// database's entry) as a login shell.
    pub program: *const c_char,
    /// The program's arguments, excluding `argv[0]`: an array of
    /// `arg_count` non-`NULL` strings. May be `NULL` when `arg_count` is
    /// zero. Ignored, and not read, for the login shell.
    pub args: *const *const c_char,
    /// Number of entries in `args`.
    pub arg_count: usize,
    /// Working directory, or `NULL` for the user's home directory. A path
    /// that is not an existing directory also falls back to the home
    /// directory rather than failing the spawn.
    pub cwd: *const c_char,
    /// Initial number of rows; must be at least 1.
    pub rows: u16,
    /// Initial number of columns; must be at least 1.
    pub cols: u16,
    /// Width of the view in pixels, or 0 when unknown.
    pub width_px: u16,
    /// Height of the view in pixels, or 0 when unknown.
    pub height_px: u16,
}

impl SpawnOptions {
    /// Copies the C description into the core's spawn options.
    ///
    /// # Safety
    ///
    /// Every pointer must satisfy the contract documented on the fields.
    unsafe fn to_core(self) -> Result<CoreSpawnOptions, Error> {
        if self.rows == 0 || self.cols == 0 {
            return Err(Error::InvalidArgument);
        }
        let size = PtySize::new(self.rows, self.cols).with_pixels(self.width_px, self.height_px);
        // The core replaces the session id with the one it assigns.
        let mut options = if self.program.is_null() {
            CoreSpawnOptions::login_shell("")
        } else {
            // SAFETY: non-null, NUL-terminated per the contract.
            let program = unsafe { os_str(self.program) };
            if program.is_empty() {
                return Err(Error::InvalidArgument);
            }
            CoreSpawnOptions::command(program, "")
        };
        // The login shell takes no arguments, so `args` is not read for it.
        if !self.program.is_null() && self.arg_count > 0 {
            if self.args.is_null() {
                return Err(Error::NullPointer);
            }
            // SAFETY: `args` points to `arg_count` pointers per the contract.
            let args = unsafe { slice::from_raw_parts(self.args, self.arg_count) };
            for &arg in args {
                if arg.is_null() {
                    return Err(Error::NullPointer);
                }
                // SAFETY: non-null, NUL-terminated per the contract.
                options = options.arg(unsafe { os_str(arg) });
            }
        }
        if !self.cwd.is_null() {
            // SAFETY: non-null, NUL-terminated per the contract.
            options = options.cwd(unsafe { os_str(self.cwd) });
        }
        Ok(options.size(size))
    }
}

/// Borrows a C string as an `OsStr`.
///
/// # Safety
///
/// `ptr` must be non-null and point to a NUL-terminated string that
/// outlives `'a`.
unsafe fn os_str<'a>(ptr: *const c_char) -> &'a OsStr {
    // SAFETY: guaranteed by the caller.
    OsStr::from_bytes(unsafe { CStr::from_ptr(ptr) }.to_bytes())
}

/// Starts a session described by `*options` and stores its id in
/// `*out_id`.
///
/// The child runs in a fresh PTY with `TERM`, `COLORTERM`, `TERM_PROGRAM`,
/// `TERM_PROGRAM_VERSION` and `TESSERA_SESSION_ID` set. Ids start at 1 and
/// are never reused by the same application, so 0 never names a session.
/// Callbacks for the new session may fire before this returns.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app`, `options` or `out_id` is
/// `NULL` (or an argument string is), `TESSERA_ERROR_INVALID_ARGUMENT` for
/// zero rows or columns or an empty program, `TESSERA_ERROR_SPAWN_FAILED`
/// if no PTY is available, the program cannot be started or a thread could
/// not be created.
/// `*out_id` is set to 0 on failure.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `options` must be `NULL` or point
/// to a valid `TesseraSpawnOptions` whose pointers satisfy the contract
/// documented on its fields. `out_id` must be `NULL` or point to a writable
/// `uint64_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_spawn(
    app: *mut App,
    options: *const SpawnOptions,
    out_id: *mut u64,
) -> Error {
    ffi_guard!("tessera_session_spawn", {
        if out_id.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: non-null and writable per the contract.
        unsafe { out_id.write(0) };
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        // SAFETY: the caller passes NULL or valid options.
        let Some(&options) = (unsafe { options.as_ref() }) else {
            return Error::NullPointer;
        };
        // SAFETY: the option pointers satisfy their documented contract.
        let options = match unsafe { options.to_core() } {
            Ok(options) => options,
            Err(code) => return code,
        };
        match app.core.spawn_session(options) {
            Ok(id) => {
                // SAFETY: as above.
                unsafe { out_id.write(id.get()) };
                Error::Ok
            }
            // An I/O failure while spawning (setting up the PTY's reader
            // and writer or starting its threads) means the session did
            // not start, so it is reported as a failed spawn.
            Err(error) => match error::report("tessera_session_spawn", &error) {
                Error::Io => Error::SpawnFailed,
                code => code,
            },
        }
    })
}

/// Closes session `session_id`: detaches its surface if it has one (see
/// `tessera_surface_detach`), hangs up its child (killing it if it does
/// not exit within a second), joins its reader thread and forgets the id.
/// No callback for the session runs after this returns, and
/// `on_session_exit` is not called for it. Takes at most about two seconds.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. Must not be called from a
/// callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_close(app: *mut App, session_id: u64) -> Error {
    ffi_guard!("tessera_session_close", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        error::status(
            "tessera_session_close",
            app.core.close_session(SessionId::from_raw(session_id)),
        )
    })
}

/// Queues `len` bytes from `bytes` as input for session `session_id`.
///
/// **Safe to call from any thread**, concurrently with every other call
/// except `tessera_app_free`. Blocks while the session's input queue
/// (1 MiB) is full because the child is not reading, with no time limit,
/// until the child reads or the session is closed (from another thread);
/// a large paste is best written from a background thread. For keystrokes
/// and pastes use `tessera_session_send_key` and `tessera_session_paste`,
/// which never block; bytes written here bypass their pending input and may
/// overtake or interleave with it (at 64 KiB chunk boundaries, so a pending
/// paste can be split around them). The bytes are copied before this
/// returns.
///
/// Input is queued in chunks of up to 64 KiB. Bytes from one call stay in
/// order, but calls made concurrently from several threads for the same
/// session may interleave at chunk boundaries.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL` or `bytes` is
/// `NULL` with a non-zero `len`, `TESSERA_ERROR_SESSION_NOT_FOUND` if the id
/// is not a live session, `TESSERA_ERROR_IO` if the child has exited or the
/// session is being closed.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `bytes` must point to `len`
/// readable bytes; it may be `NULL` when `len` is zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_write(
    app: *mut App,
    session_id: u64,
    bytes: *const u8,
    len: usize,
) -> Error {
    ffi_guard!("tessera_session_write", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        let data = if len == 0 {
            &[][..]
        } else if bytes.is_null() {
            return Error::NullPointer;
        } else {
            // SAFETY: `bytes` points to `len` readable bytes per the contract.
            unsafe { slice::from_raw_parts(bytes, len) }
        };
        error::status(
            "tessera_session_write",
            app.core.write(SessionId::from_raw(session_id), data),
        )
    })
}

/// Resizes session `session_id`'s terminal to `rows` x `cols` cells
/// (re-wrapping its text) and tells its child that its window is now that
/// size and `width_px` x `height_px` pixels (0 when unknown); the child
/// receives `SIGWINCH`.
///
/// Do not call this for a session with an attached surface
/// (`tessera_surface_attach`): the surface decides its size. A call
/// leaves the child at the requested size only until the next
/// `tessera_surface_resize` or `tessera_surface_set_font_size` resizes it
/// to the surface again.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session,
/// `TESSERA_ERROR_INVALID_ARGUMENT` for zero rows or columns or more than
/// 1024 rows or 2048 columns (the largest grid a surface derives; nothing
/// changes then), `TESSERA_ERROR_IO` if the kernel rejects the size.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_resize(
    app: *mut App,
    session_id: u64,
    rows: u16,
    cols: u16,
    width_px: u16,
    height_px: u16,
) -> Error {
    ffi_guard!("tessera_session_resize", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        let size = PtySize::new(rows, cols).with_pixels(width_px, height_px);
        error::status(
            "tessera_session_resize",
            app.core.resize(SessionId::from_raw(session_id), size),
        )
    })
}

/// Discards session `session_id`'s scrollback (the visible screen is
/// kept).
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_clear_scrollback(app: *mut App, session_id: u64) -> Error {
    ffi_guard!("tessera_session_clear_scrollback", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        error::status(
            "tessera_session_clear_scrollback",
            app.core.clear_scrollback(SessionId::from_raw(session_id)),
        )
    })
}
