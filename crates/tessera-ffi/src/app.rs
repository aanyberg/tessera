//! The application handle: creation, destruction, callbacks, paths.

use std::ffi::c_char;
use std::os::unix::ffi::OsStrExt;
use std::ptr;

use tessera_app_core::{App as CoreApp, ConfigPaths, WatchOptions};

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
/// * Every `tessera_app_*`, `tessera_session_*` and `tessera_surface_*`
///   function is called from one thread, the host's UI (main) thread, with
///   the single exception of `tessera_session_write`, which may be called
///   from any thread, concurrently with any other call except
///   `tessera_app_free`. The surface functions create, configure and
///   present Metal surfaces, which must happen on the main thread.
/// * Callbacks (see `TesseraCallbacks`) fire on internal threads (session
///   readers, the configuration watch).
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

/// Environment variable naming the configuration directory, in place of
/// `~/Library/Application Support/Tessera`, for tests and side-by-side
/// installs. Read by `tessera_app_new`; an empty value is ignored.
const CONFIG_DIR_ENV: &str = "TESSERA_CONFIG_DIR";

/// Creates an application and stores its handle in `*out`.
///
/// The configuration directory is `~/Library/Application Support/Tessera`,
/// or the directory the `TESSERA_CONFIG_DIR` environment variable names
/// when it is set to an absolute path (for tests and side-by-side
/// installs; unset by default; a relative path is ignored with a warning). The application starts with the
/// configuration `config.toml` holds and the theme its `appearance.theme`
/// names, and watches both (and `themes/`) for the rest of its life,
/// applying every edit to its views and reporting it through
/// `on_appearance_changed`. Starting creates nothing on disk: a missing or
/// invalid `config.toml` gives the defaults (logged; a repaired file is
/// picked up by the watch), and a configuration directory that does not
/// exist yet is looked for every two seconds (one `stat`) and watched once
/// it appears, or at once when `tessera_app_create_config` creates it. The
/// exception is a directory whose parent is missing too (only a
/// `TESSERA_CONFIG_DIR` that names nothing can be): it is not looked for,
/// and only `tessera_app_create_config` creates it and starts the watch.
/// Neither fails the call. No callbacks are installed and no session is
/// running.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `out` is `NULL`, and
/// `TESSERA_ERROR_IO` if neither `TESSERA_CONFIG_DIR` names a directory
/// nor the home directory can be determined; `*out` is set to `NULL` on
/// failure.
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
        // Process-wide and idempotent: only the first app installs it.
        tessera_app_core::session::set_thread_start_hook(raise_thread_qos);
        let Some(paths) = config_paths() else {
            tracing::warn!("cannot determine the home directory");
            return Error::Io;
        };
        let app = Box::new(App {
            // Lead decisions D1, D2: the watch lives in the core, and a
            // missing or broken configuration is never an error here.
            core: CoreApp::with_config_dir(paths, WatchOptions::default()),
        });
        // SAFETY: as above.
        unsafe { out.write(Box::into_raw(app)) };
        Error::Ok
    })
}

/// The configuration directory: `TESSERA_CONFIG_DIR` when set to an
/// absolute path, otherwise the current user's.
///
/// A relative `TESSERA_CONFIG_DIR` is refused with a warning: it would name
/// a different directory for every working directory the app is started
/// from.
fn config_paths() -> Option<ConfigPaths> {
    match std::env::var_os(CONFIG_DIR_ENV) {
        Some(dir) if !dir.is_empty() => {
            if std::path::Path::new(&dir).is_absolute() {
                return Some(ConfigPaths::new(dir));
            }
            tracing::warn!(
                dir = %std::path::Path::new(&dir).display(),
                "{CONFIG_DIR_ENV} is not an absolute path; using the default configuration directory"
            );
            ConfigPaths::for_current_user()
        }
        _ => ConfigPaths::for_current_user(),
    }
}

/// Gives the calling thread (a session reader or input writer, or the
/// reaper) the `QOS_CLASS_USER_INITIATED` quality-of-service class.
///
/// Threads the core starts would otherwise run at the default class, below
/// the main thread's `USER_INTERACTIVE`, and the main thread waits on locks
/// they hold (the terminal lock while output is applied, a session's
/// delivery lock while a callback runs): a priority inversion Xcode's Thread
/// Performance Checker reports. `USER_INTERACTIVE` is deliberately not used:
/// parsing output must not compete with the main thread (ADR 0018).
#[cfg(target_os = "macos")]
fn raise_thread_qos() {
    // Declared inside the function so cbindgen, which reads every
    // module-level item, does not copy it into TesseraCore.h.
    unsafe extern "C" {
        // <pthread/qos.h>; `qos_class_t` is an `unsigned int` enum.
        fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    }
    /// `QOS_CLASS_USER_INITIATED` from `<sys/qos.h>`.
    const QOS_CLASS_USER_INITIATED: u32 = 0x19;
    // SAFETY: takes plain integers and changes only the calling thread,
    // which is not a workqueue thread (where the call is refused with an
    // error, not undefined behaviour).
    let status = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED, 0) };
    if status != 0 {
        tracing::debug!(status, "cannot set the thread's quality-of-service class");
    }
}

/// Nothing to do where there is no quality-of-service class.
#[cfg(not(target_os = "macos"))]
fn raise_thread_qos() {}

/// Destroys an application: stops the configuration watch, detaches every
/// surface (releasing the core's references to their layers), closes every
/// session still open (hanging up, and if need be killing, its child),
/// waits for those closes and for
/// every close `tessera_session_close` left running in the background,
/// joins every session thread and releases the handle. A session whose
/// terminal is still held open after `SIGKILL` (a grandchild kept it) has
/// its threads detached instead of joined; they are muted and no longer
/// hold the callback table. Returns once no callback is running or can run
/// again; the installed `user_data` may then be freed. Each child gets one
/// second after its hangup before it is killed and one more after the
/// kill, counted from its own close and running in parallel for all of
/// them, so this blocks for at most about two seconds however many
/// children ignore `SIGHUP`. `NULL` is ignored.
///
/// A configuration change being applied when this is called finishes
/// first: its thread is given a second, then left to end on its own, but
/// an `on_appearance_changed` callback it is running is waited for as long
/// as it runs (like any callback), so a callback that blocks makes this
/// block too.
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

/// Makes the configuration editable: creates the configuration directory
/// (see `tessera_app_new`) and a commented `config.toml` with the defaults
/// where they are missing, and starts watching them if the application is
/// not watching yet, so the edits the user is about to make apply live.
/// Call it before opening `config.toml` for editing: the app's Open Config
/// menu item (#29) and Preferences (#40) are its callers.
/// Idempotent; an existing file is never changed.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`, and
/// `TESSERA_ERROR_IO` if the directory or the file cannot be created or the
/// watch cannot be started.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. Main thread only, like every
/// `tessera_app_*` call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_app_create_config(app: *mut App) -> Error {
    ffi_guard!("tessera_app_create_config", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        match app.core.open_config(WatchOptions::default()) {
            Ok(_) => Error::Ok,
            Err(error) => {
                tracing::warn!(%error, "cannot create the configuration");
                Error::Io
            }
        }
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
