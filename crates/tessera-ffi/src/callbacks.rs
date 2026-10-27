//! The callback table through which the core notifies the frontend.

use std::ffi::{CString, c_char, c_void};

use tessera_app_core::{SessionEvent, SessionId, SessionObserver};

/// Functions the core calls when something happens in a session.
///
/// Installed with `tessera_app_set_callbacks`, which copies the table: the
/// struct itself may be freed as soon as that call returns. Every function
/// pointer may be `NULL` to ignore that event.
///
/// # Threads
///
/// Callbacks run on an internal reader thread (one per session), never on
/// the thread that calls `tessera_*`. A callback must return quickly, must
/// not call any `tessera_*` function, and must not wait for the thread that
/// does (for example with a synchronous dispatch to the main queue): closing
/// a session, replacing the callbacks and freeing the application all wait
/// for a callback in progress to return, so breaking this rule deadlocks.
/// Marshal work to the main thread asynchronously instead.
///
/// Callbacks of one session are called in order; callbacks of different
/// sessions may run concurrently. Events of a new session can arrive before
/// `tessera_session_spawn` has returned its id.
///
/// # Ownership and lifetime
///
/// * `user_data` is owned by the host and passed back unchanged to every
///   callback. It must stay valid, and be usable from any thread, until the
///   table is replaced by another `tessera_app_set_callbacks` call or the
///   application is freed with `tessera_app_free`; once either call returns
///   the old `user_data` is never used again, and the host may free it.
/// * Every `const char *` handed to a callback is a NUL-terminated UTF-8
///   string owned by the core and valid only for the duration of that call;
///   copy it to keep it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Callbacks {
    /// Opaque host pointer passed as the first argument of every callback.
    /// May be `NULL`.
    pub user_data: *mut c_void,
    /// The session wrote output and the terminal has applied it; the
    /// frontend should schedule a redraw. Called once per chunk read from
    /// the PTY, so it may fire often; coalesce redraws.
    pub on_session_output: Option<unsafe extern "C" fn(user_data: *mut c_void, session_id: u64)>,
    /// The session's program set its window or tab title. `title` is a
    /// NUL-terminated UTF-8 string valid only during the call (any NUL the
    /// program sent is removed).
    pub on_session_title:
        Option<unsafe extern "C" fn(user_data: *mut c_void, session_id: u64, title: *const c_char)>,
    /// The session's program rang the bell.
    pub on_session_bell: Option<unsafe extern "C" fn(user_data: *mut c_void, session_id: u64)>,
    /// The session's child exited. `code` is its exit code, or `-1` when it
    /// was terminated by a signal; `signal` is that signal's number, or `0`
    /// when it exited normally. This is the last callback for the session;
    /// the session id stays valid (writes fail) until
    /// `tessera_session_close`. Not called for a session the host closed.
    pub on_session_exit: Option<
        unsafe extern "C" fn(user_data: *mut c_void, session_id: u64, code: i32, signal: i32),
    >,
}

/// Adapts a [`Callbacks`] table to the core's observer trait.
pub(crate) struct CallbackObserver(Callbacks);

impl CallbackObserver {
    pub(crate) fn new(callbacks: Callbacks) -> Self {
        Self(callbacks)
    }
}

// SAFETY: the host promises (see `Callbacks`) that the function pointers and
// `user_data` may be used from any thread for as long as the table is
// installed; the core uninstalls the table before the host may free it.
unsafe impl Send for CallbackObserver {}
// SAFETY: as above; the table is only read.
unsafe impl Sync for CallbackObserver {}

impl SessionObserver for CallbackObserver {
    fn on_session_event(&self, id: SessionId, event: SessionEvent) {
        let callbacks = &self.0;
        let user_data = callbacks.user_data;
        let id = id.get();
        match event {
            SessionEvent::Output => {
                if let Some(callback) = callbacks.on_session_output {
                    // SAFETY: host contract on `Callbacks`.
                    unsafe { callback(user_data, id) };
                }
            }
            SessionEvent::Title(title) => {
                if let Some(callback) = callbacks.on_session_title {
                    let title = c_string_lossy(title);
                    // SAFETY: host contract on `Callbacks`; `title` outlives
                    // the call.
                    unsafe { callback(user_data, id, title.as_ptr()) };
                }
            }
            SessionEvent::Bell => {
                if let Some(callback) = callbacks.on_session_bell {
                    // SAFETY: host contract on `Callbacks`.
                    unsafe { callback(user_data, id) };
                }
            }
            SessionEvent::Exit(status) => {
                if let Some(callback) = callbacks.on_session_exit {
                    let code = status.code().unwrap_or(-1);
                    let signal = status.signal().unwrap_or(0);
                    // SAFETY: host contract on `Callbacks`.
                    unsafe { callback(user_data, id, code, signal) };
                }
            }
        }
    }
}

/// Converts `text` to a C string, dropping interior NULs.
fn c_string_lossy(text: String) -> CString {
    CString::new(text).unwrap_or_else(|error| {
        let mut bytes = error.into_vec();
        bytes.retain(|&byte| byte != 0);
        CString::new(bytes).expect("all NUL bytes were removed")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interior_nuls_are_removed() {
        assert_eq!(c_string_lossy("a\0b\0".into()).as_bytes(), b"ab");
        assert_eq!(c_string_lossy("plain".into()).as_bytes(), b"plain");
    }
}
