//! The application handle: creation, destruction, callbacks, paths.

use std::ffi::c_char;
use std::os::unix::ffi::OsStrExt;
use std::ptr;

use tessera_app_core::{App as CoreApp, Config, ConfigPaths};

use crate::callbacks::{CallbackObserver, Callbacks};
use crate::error::Error;
use crate::panic::ffi_guard;

/// Opaque handle to one Tessera application: its configuration and every
/// session it runs.
///
/// Created by `tessera_app_new`, destroyed by `tessera_app_free`. Session
/// ids are plain `uint64_t` values scoped to one handle, never pointers.
///
/// # Thread-safety contract
///
/// * Every `tessera_app_*` and `tessera_session_*` function is called from
///   one thread, the host's UI (main) thread, with the single exception of
///   `tessera_session_write`, which may be called from any thread,
///   concurrently with any other call except `tessera_app_free`.
/// * Callbacks (see `TesseraCallbacks`) fire on internal reader threads.
///   They must return quickly, must not call any `tessera_*` function and
///   must not block on the UI thread; the host marshals their work to its
///   main thread asynchronously.
/// * `tessera_app_free` must not race with any other call on the same
///   handle, including `tessera_session_write` from another thread.
#[derive(Debug)]
pub struct App {
    pub(crate) core: CoreApp,
}

impl App {
    /// Borrows the application behind `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` must be `NULL` or a live handle from `tessera_app_new` that is
    /// not freed while the returned reference is used.
    pub(crate) unsafe fn from_ptr<'a>(ptr: *const Self) -> Option<&'a Self> {
        // SAFETY: guaranteed by the caller.
        unsafe { ptr.as_ref() }
    }
}

/// Creates an application and stores its handle in `*out`.
///
/// The configuration directory is `~/Library/Application Support/Tessera`;
/// the default configuration applies (loading `config.toml` is not wired up
/// yet). No callbacks are installed and no session is running.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `out` is `NULL`, and
/// `TESSERA_ERROR_IO` if the home directory cannot be determined; `*out` is
/// set to `NULL` on failure.
///
/// # Safety
///
/// `out` must be `NULL` or point to writable storage for one pointer. The
/// handle written there is owned by the caller and must be released with
/// `tessera_app_free` exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_app_new(out: *mut *mut App) -> Error {
    ffi_guard!("tessera_app_new", {
        if out.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: `out` is non-null and writable per the contract.
        unsafe { out.write(ptr::null_mut()) };
        let Some(paths) = ConfigPaths::for_current_user() else {
            tracing::warn!("cannot determine the home directory");
            return Error::Io;
        };
        let app = Box::new(App {
            core: CoreApp::new(Config::default(), paths),
        });
        // SAFETY: as above.
        unsafe { out.write(Box::into_raw(app)) };
        Error::Ok
    })
}

/// Destroys an application: closes every session (hanging up, and if need
/// be killing, its child), joins every session reader thread and releases
/// the handle. A reader whose terminal is still held open after `SIGKILL`
/// (a grandchild kept it) is detached instead of joined; it is muted and no
/// longer holds the callback table. Returns once no callback is running or
/// can run again; the installed `user_data` may then be freed. All sessions
/// share one hangup grace period and one kill grace period, so this takes
/// at most about two seconds however many children ignore `SIGHUP`.
/// `NULL` is ignored.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle from `tessera_app_new`. No other
/// call on the handle may be in progress or follow, and this must not be
/// called from a callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_app_free(app: *mut App) {
    ffi_guard!(void "tessera_app_free", {
        if !app.is_null() {
            // SAFETY: `app` came from `Box::into_raw` in `tessera_app_new`
            // and the caller gives up the handle.
            drop(unsafe { Box::from_raw(app) });
        }
    });
}

/// Installs the callback table, replacing any previous one, or removes it
/// when `callbacks` is `NULL`. Applies to sessions already running.
///
/// The table is copied; `*callbacks` need not outlive the call. Waits for a
/// callback in progress to return, so once this returns the previous
/// table's functions and `user_data` are never used again.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `callbacks` must be `NULL` or
/// point to a valid `TesseraCallbacks` whose function pointers and
/// `user_data` satisfy the contract documented on that struct. Must not be
/// called from a callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_app_set_callbacks(
    app: *mut App,
    callbacks: *const Callbacks,
) -> Error {
    ffi_guard!("tessera_app_set_callbacks", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        // SAFETY: the caller passes NULL or a valid table, which is copied.
        let callbacks = unsafe { callbacks.as_ref() }.copied();
        app.core
            .set_observer(callbacks.map(|table| Box::new(CallbackObserver::new(table)) as Box<_>));
        Error::Ok
    })
}

/// Copies the path of the main configuration file (`config.toml`) into
/// `buf` as a NUL-terminated string.
///
/// `len` is the capacity of `buf` in bytes, including room for the NUL.
/// When `out_len` is not `NULL`, the path's length in bytes (excluding the
/// NUL) is stored there whether or not it fit, so a caller can pass
/// `buf = NULL, len = 0` to query the size, then allocate `*out_len + 1`
/// bytes. The path is the raw file-system path, which on macOS is UTF-8.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`, or if `buf` is
/// `NULL` while `len` is not zero; `TESSERA_ERROR_INVALID_ARGUMENT` if the
/// path and its NUL do not fit in `len` bytes, in which case `buf` is left
/// untouched (`*out_len` is still set).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `buf` must be `NULL` or point to
/// `len` writable bytes. `out_len` must be `NULL` or point to a writable
/// `size_t`. The core keeps no pointer to either after the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_app_config_path(
    app: *const App,
    buf: *mut c_char,
    len: usize,
    out_len: *mut usize,
) -> Error {
    ffi_guard!("tessera_app_config_path", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        let path = app.core.config_path();
        let bytes = path.as_os_str().as_bytes();
        if !out_len.is_null() {
            // SAFETY: non-null and writable per the contract.
            unsafe { out_len.write(bytes.len()) };
        }
        if buf.is_null() {
            return if len == 0 {
                Error::Ok
            } else {
                Error::NullPointer
            };
        }
        if bytes.len() >= len {
            return Error::InvalidArgument;
        }
        // SAFETY: `buf` has `len > bytes.len()` writable bytes, which do not
        // overlap the freshly allocated `path`.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast::<u8>(), bytes.len());
            buf.add(bytes.len()).write(0);
        }
        Error::Ok
    })
}
