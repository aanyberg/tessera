//! The callback table through which the core notifies the frontend.

use std::ffi::{CString, c_char, c_void};

use tessera_app_core::{Remote, SessionEvent, SessionId, SessionObserver};

/// Functions the core calls when something happens in a session, or when
/// the appearance of every view changes.
///
/// Installed with `tessera_app_set_callbacks`, which copies the table: the
/// struct itself may be freed as soon as that call returns. Every function
/// pointer may be `NULL` to ignore that event.
///
/// # Threads
///
/// Session callbacks run on an internal reader thread (one per session),
/// and `on_appearance_changed` on the configuration watch's thread, or on
/// the calling thread before `tessera_app_set_display_options` returns;
/// no other callback runs on the thread that calls `tessera_*`. A host
/// lock held around `tessera_app_set_display_options` must therefore not
/// be taken again in `on_appearance_changed`. A callback must return quickly, must
/// not call any `tessera_*` function, and must not wait for the thread that
/// does (for example with a synchronous dispatch to the main queue): closing
/// a session, replacing the callbacks and freeing the application all wait
/// for a callback in progress to return, so breaking this rule deadlocks.
/// Marshal work to the main thread asynchronously instead.
///
/// Callbacks of one session are called in order; callbacks of different
/// sessions, and `on_appearance_changed`, may run concurrently. Events of a new session can arrive before
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
///
/// # ABI
///
/// Fields are only ever added at the end (`on_appearance_changed`,
/// `on_session_clipboard`, `on_config_applied` and `on_session_remote`
/// were). A host compiled against an older header
/// passes a shorter table, which this core would read past: rebuild the
/// host against the header that ships with the core it links. Initialise the table with designated
/// initialisers (C) or every field (Swift), so a new field is `NULL`.
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
    /// program sent is removed). Called with the last title of each chunk
    /// read from the PTY only, since each title replaces the previous one:
    /// a program that sets many titles at once causes one call.
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
    /// What the views draw with changed: the configuration watch applied
    /// an edit of `config.toml` or of the theme file in use (colours,
    /// opacity, blur, font size or default cursor style), or
    /// `tessera_app_set_display_options` changed some session's opacity or
    /// blur. `opaque` is whether every view's background is now fully
    /// opaque (`false`: some view is translucent). The host reads each
    /// session's `tessera_session_appearance` and makes each window and
    /// layer opaque or translucent, blurred or not, to match (ADR 0043),
    /// and redraws every view: each surface switches its compositing mode
    /// at its next frame, and a view whose display link idles would
    /// otherwise keep the old colours until its next output. Called on the
    /// configuration watch's thread, or on the calling thread before
    /// `tessera_app_set_display_options` returns; not called for a change
    /// that alters nothing the views draw, but a change of what the
    /// frontend draws is reported: a session's blur or background colour
    /// (`tessera_session_appearance`) or the window's blur radius
    /// (`tessera_app_blur_radius`), once per configuration applied however
    /// many sessions it changed. It names no session: one call per apply,
    /// not one per session affected, so the host reads every session's
    /// `tessera_session_appearance` again. Like every callback it must not call
    /// `tessera_*`: in particular
    /// `tessera_app_set_callbacks` or `tessera_app_free` from it deadlocks,
    /// since both wait for this call to return.
    pub on_appearance_changed: Option<unsafe extern "C" fn(user_data: *mut c_void, opaque: bool)>,
    /// The session's program wrote text to the clipboard (OSC 52) and the
    /// configuration allows it (`[terminal] clipboard_write`, default
    /// on): the host puts it on the system pasteboard. `text` points to
    /// `len` bytes of UTF-8, valid only during the call and NOT
    /// NUL-terminated: the text is passed on exactly as the program sent
    /// it, so it may contain NUL, ESC and other control characters (paste
    /// it with bracketed paste, as for any clipboard text). At most 1 MiB.
    /// Called at most once per chunk of output (the last write wins).
    /// Programs can never read the clipboard.
    pub on_session_clipboard: Option<
        unsafe extern "C" fn(user_data: *mut c_void, session_id: u64, text: *const u8, len: usize),
    >,
    /// The configuration watch applied a configuration (after an edit of
    /// `config.toml` or of the theme file in use), whether or not anything
    /// in it changed; `generation` counts the configurations applied since
    /// the application was created (the first one applied is 1). The host
    /// reads what it shows from the configuration again: the menus' key
    /// equivalents with `tessera_app_keymap_json`, which reports the
    /// generation it describes. Called after `on_appearance_changed` when
    /// both are, on the configuration watch's thread, under the same rules
    /// as every callback: marshal the work to the main thread.
    pub on_config_applied: Option<unsafe extern "C" fn(user_data: *mut c_void, generation: u64)>,
    /// Where the session's shell runs changed (#205): `kind` is a
    /// [`RemoteKind`] (`TESSERA_REMOTE_KIND_LOCAL`, `_HOST` or `_UNNAMED`).
    /// For `_HOST`, `host` points to `len` bytes of UTF-8 naming the other
    /// machine, valid only during the call and NOT NUL-terminated: already
    /// sanitised (no control, bidi-control or line-separator characters),
    /// never empty and at most 253 bytes (`TESSERA_MAX_REMOTE_HOST_LEN`).
    /// Otherwise `host` is `NULL` and `len` is 0. The host shows a remote
    /// session in its title (`⇢ host:`, or `⇢ ssh` for `_UNNAMED`) unless
    /// `general.remote_indicator` is `"off"`. Called only when the state
    /// changes, at most once per chunk of output; a new session starts
    /// local. The host is the child's claim, never a security decision.
    pub on_session_remote: Option<
        unsafe extern "C" fn(
            user_data: *mut c_void,
            session_id: u64,
            kind: u8,
            host: *const u8,
            len: usize,
        ),
    >,
}

/// Most bytes of the host `on_session_remote` passes: the longest DNS name
/// (the core's `MAX_REMOTE_HOST_BYTES`, spelled out for the header).
pub const MAX_REMOTE_HOST_LEN: usize = 253;

const _: () = assert!(MAX_REMOTE_HOST_LEN == tessera_app_core::MAX_REMOTE_HOST_BYTES);

/// Where a session's shell runs: the `kind` argument of
/// `on_session_remote`, carried as `uint8_t`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RemoteKind {
    /// On this machine, or nothing says otherwise.
    Local = 0,
    /// On the machine `host` names (its last OSC 7 report).
    Host = 1,
    /// On another machine whose name is not shown: `ssh` runs in the
    /// foreground with no report naming a host, or the report's host was
    /// too long or empty once sanitised.
    Unnamed = 2,
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
            SessionEvent::Clipboard(text) => {
                if let Some(callback) = callbacks.on_session_clipboard {
                    // SAFETY: host contract on `Callbacks`; `text` outlives
                    // the call and `len` is its length in bytes.
                    unsafe { callback(user_data, id, text.as_ptr(), text.len()) };
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
            SessionEvent::Remote(remote) => {
                if let Some(callback) = callbacks.on_session_remote {
                    let (kind, host) = match &remote {
                        Remote::Local => (RemoteKind::Local, None),
                        Remote::Host(host) => (RemoteKind::Host, Some(host.as_bytes())),
                        Remote::Unnamed => (RemoteKind::Unnamed, None),
                    };
                    let (ptr, len) = host.map_or((std::ptr::null(), 0), |h| (h.as_ptr(), h.len()));
                    // SAFETY: host contract on `Callbacks`; `host` outlives
                    // the call and `len` is its length in bytes.
                    unsafe { callback(user_data, id, kind as u8, ptr, len) };
                }
            }
            // Internal to the core (ADR 0039, #45a): no host callback.
            SessionEvent::CommandStarted | SessionEvent::CommandFinished { .. } => {}
        }
    }

    fn on_appearance_changed(&self, opaque: bool) {
        if let Some(callback) = self.0.on_appearance_changed {
            // SAFETY: host contract on `Callbacks`.
            unsafe { callback(self.0.user_data, opaque) };
        }
    }

    fn on_config_applied(&self, generation: u64) {
        if let Some(callback) = self.0.on_config_applied {
            // SAFETY: host contract on `Callbacks`.
            unsafe { callback(self.0.user_data, generation) };
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

    /// The table's C layout: nine pointers, `on_session_remote` last,
    /// right after `on_config_applied` (fields are only appended, S7).
    #[test]
    fn the_session_remote_callback_is_appended_last() {
        use std::mem::{offset_of, size_of};

        let pointer = size_of::<*const c_void>();
        assert_eq!(size_of::<Callbacks>(), 9 * pointer);
        assert_eq!(offset_of!(Callbacks, on_appearance_changed), 5 * pointer);
        assert_eq!(offset_of!(Callbacks, on_session_clipboard), 6 * pointer);
        assert_eq!(offset_of!(Callbacks, on_config_applied), 7 * pointer);
        assert_eq!(offset_of!(Callbacks, on_session_remote), 8 * pointer);
    }

    /// What one `on_session_remote` call passed: kind, host bytes (`None`
    /// for a null pointer) and length.
    type RemoteCall = (u8, Option<Vec<u8>>, usize);

    static REMOTE_CALLS: std::sync::Mutex<Vec<RemoteCall>> = std::sync::Mutex::new(Vec::new());

    unsafe extern "C" fn record_remote(
        _user_data: *mut c_void,
        _session_id: u64,
        kind: u8,
        host: *const u8,
        len: usize,
    ) {
        let bytes = (!host.is_null())
            // SAFETY: the core passes `len` readable bytes at `host`.
            .then(|| unsafe { std::slice::from_raw_parts(host, len) }.to_vec());
        REMOTE_CALLS.lock().unwrap().push((kind, bytes, len));
    }

    #[test]
    fn remote_states_reach_the_callback_with_their_kind_and_host() {
        let observer = CallbackObserver::new(Callbacks {
            user_data: std::ptr::null_mut(),
            on_session_output: None,
            on_session_title: None,
            on_session_bell: None,
            on_session_exit: None,
            on_appearance_changed: None,
            on_session_clipboard: None,
            on_config_applied: None,
            on_session_remote: Some(record_remote),
        });
        let id = SessionId::from_raw(1);
        observer.on_session_event(id, SessionEvent::Remote(Remote::Host("far.example".into())));
        observer.on_session_event(id, SessionEvent::Remote(Remote::Unnamed));
        observer.on_session_event(id, SessionEvent::Remote(Remote::Local));
        assert_eq!(
            *REMOTE_CALLS.lock().unwrap(),
            [
                (1, Some(b"far.example".to_vec()), 11),
                (2, None, 0),
                (0, None, 0),
            ]
        );
        assert_eq!(RemoteKind::Local as u8, 0);
        assert_eq!(RemoteKind::Host as u8, 1);
        assert_eq!(RemoteKind::Unnamed as u8, 2);
        assert_eq!(MAX_REMOTE_HOST_LEN, 253);
    }

    #[test]
    fn interior_nuls_are_removed() {
        assert_eq!(c_string_lossy("a\0b\0".into()).as_bytes(), b"ab");
        assert_eq!(c_string_lossy("plain".into()).as_bytes(), b"plain");
    }
}
