//! Scrollback search (ADR 0038).
//!
//! A session has at most one search: `tessera_session_search_start` sets
//! its query, and the host then calls `tessera_session_search_step` from
//! its display link with a small time budget while the returned status is
//! not `done` (and again after output), so a search of a long history
//! never blocks a frame. Matches show on the session's surface from the
//! next `tessera_surface_render`; `tessera_session_search_older` and
//! `_newer` move the current match and scroll the view to it. Call these
//! from the UI (main) thread, like the selection calls.
//!
//! The query and the text of matches are user data: the core never logs
//! them (security rule S8).

use std::slice;
use std::time::Duration;

use tessera_app_core::search::{MAX_MATCHES, MAX_MATCHES_TEXT_BYTES};
use tessera_app_core::{
    Error as CoreError, MAX_QUERY_BYTES, QueryError, QueryOptions, SearchStatus as CoreStatus,
    SessionId,
};

use crate::app::App;
use crate::error::{self, Error};
use crate::panic::ffi_guard;
use crate::selection::copy_out;

/// The `options` bit of `tessera_session_search_start` that reads the
/// pattern as a regex (Rust `regex` syntax: no look-around, no
/// backreferences) instead of a literal string.
pub const SEARCH_REGEX: u32 = 1;

/// The `options` bit of `tessera_session_search_start` that matches case
/// exactly; without it letters match regardless of case. A regex's
/// inline `(?i)` or `(?-i)` wins.
pub const SEARCH_CASE_SENSITIVE: u32 = 2;

/// The longest pattern, in bytes, `tessera_session_search_start` accepts.
pub const SEARCH_MAX_QUERY_BYTES: usize = 1024;

const _: () = assert!(SEARCH_MAX_QUERY_BYTES == MAX_QUERY_BYTES);

/// The most matches a search keeps (the newest); past it the status's
/// `truncated` is set and the count reads "more than".
pub const SEARCH_MAX_MATCHES: usize = 100_000;

const _: () = assert!(SEARCH_MAX_MATCHES == MAX_MATCHES);

/// The longest text, in bytes, `tessera_session_search_matches_text`
/// returns.
pub const SEARCH_MAX_MATCHES_TEXT_BYTES: usize = 1_048_576;

const _: () = assert!(SEARCH_MAX_MATCHES_TEXT_BYTES == MAX_MATCHES_TEXT_BYTES);

/// The longest budget, in microseconds, one
/// `tessera_session_search_step` takes; a larger budget is clamped to it.
pub const SEARCH_MAX_STEP_MICROS: u32 = 1_000_000;

/// Why `tessera_session_search_start` refused a pattern (its `refusal`
/// out-parameter, carried as `uint32_t`).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchRefusal {
    /// The pattern was accepted.
    None = 0,
    /// The pattern is longer than `TESSERA_SEARCH_MAX_QUERY_BYTES`.
    TooLong = 1,
    /// The pattern is not a valid regex; the message says why.
    Syntax = 2,
    /// The regex is valid but too large to search (for example a large
    /// repetition count of a Unicode class).
    TooComplex = 3,
    /// The pattern is not valid UTF-8.
    NotUtf8 = 4,
}

/// Where a session's search stands, filled in by the
/// `tessera_session_search_*` calls.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchStatus {
    /// Matches found so far, in the lines the session shows (only the
    /// alternate screen's while a full-screen application shows it). At
    /// most `TESSERA_SEARCH_MAX_MATCHES`; with `truncated` it reads "more
    /// than".
    pub matches: usize,
    /// The current match's index in document order (0 is the oldest);
    /// meaningful only when `has_current`.
    pub current: usize,
    /// A query is set: false after `tessera_session_search_clear`, an
    /// empty pattern and a refused one.
    pub active: bool,
    /// There is a current match (`current` is meaningful).
    pub has_current: bool,
    /// The last step found nothing left to scan; until then the count may
    /// still grow. New output turns it false again.
    pub done: bool,
    /// More than `TESSERA_SEARCH_MAX_MATCHES` matched; the oldest are not
    /// kept.
    pub truncated: bool,
}

impl SearchStatus {
    const fn from_core(status: CoreStatus) -> Self {
        let (has_current, current) = match status.current {
            Some(current) => (true, current),
            None => (false, 0),
        };
        Self {
            matches: status.matches,
            current,
            active: status.active,
            has_current,
            done: status.done,
            truncated: status.truncated,
        }
    }
}

/// Stores `refusal` in `*out` when `out` is not `NULL`.
///
/// # Safety
///
/// `out` must be `NULL` or writable.
unsafe fn refuse(out: *mut u32, refusal: SearchRefusal) {
    if !out.is_null() {
        // SAFETY: non-null and writable per the contract.
        unsafe { out.write(refusal as u32) };
    }
}

/// Writes as much of `message` as fits in `cap` bytes, cut at a character
/// boundary, to `buf`, and its length to `*len`. Does nothing when `len`
/// is `NULL`; writes nothing to a `NULL` `buf`.
///
/// # Safety
///
/// `len` must be `NULL` or writable; `buf` must be `NULL` or valid for
/// `cap` bytes of writes.
unsafe fn write_message(message: &str, buf: *mut u8, cap: usize, len: *mut usize) {
    if len.is_null() {
        return;
    }
    let mut end = if buf.is_null() {
        0
    } else {
        message.len().min(cap)
    };
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    if end > 0 {
        // SAFETY: `buf` is non-null and holds `cap >= end` bytes.
        unsafe { slice::from_raw_parts_mut(buf, end) }.copy_from_slice(&message.as_bytes()[..end]);
    }
    // SAFETY: non-null and writable per the contract.
    unsafe { len.write(end) };
}

/// Sets session `session_id`'s search to the `pattern_len` bytes of UTF-8
/// at `pattern`, read by `options` (`TESSERA_SEARCH_REGEX`,
/// `TESSERA_SEARCH_CASE_SENSITIVE`, or 0 for a literal that ignores case).
/// An empty pattern (`pattern` may then be `NULL`) clears the search. The
/// same pattern and options as the running search keep its matches and
/// current match; any other starts afresh, from the newest line. Nothing
/// is scanned until `tessera_session_search_step`.
///
/// `refusal`, when not `NULL`, receives a `TesseraSearchRefusal` value
/// (`TESSERA_SEARCH_REFUSAL_NONE` on success). For a refused regex,
/// `message` (when not `NULL`, with `message_len` not `NULL`) receives up
/// to `message_cap` bytes of UTF-8 saying why, cut at a character
/// boundary and not NUL-terminated, and `*message_len` their count (0
/// when there is no message). A refused pattern clears the search.
///
/// Returns `TESSERA_ERROR_INVALID_ARGUMENT` for a refused pattern
/// (longer than `TESSERA_SEARCH_MAX_QUERY_BYTES`, not UTF-8, an invalid
/// or too complex regex) or an unknown `options` bit (nothing changes);
/// `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`, or `pattern` is `NULL`
/// with a non-zero `pattern_len`; `TESSERA_ERROR_SESSION_NOT_FOUND` if the
/// id is not a live session.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `pattern` must be valid for
/// `pattern_len` bytes of reads unless `pattern_len` is 0 or above
/// `TESSERA_SEARCH_MAX_QUERY_BYTES` (then it is not read). `refusal` must
/// be `NULL` or point to a writable `uint32_t`, `message_len` `NULL` or a
/// writable `size_t`, and `message` `NULL` or valid for `message_cap`
/// bytes of writes.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn tessera_session_search_start(
    app: *mut App,
    session_id: u64,
    pattern: *const u8,
    pattern_len: usize,
    options: u32,
    refusal: *mut u32,
    message: *mut u8,
    message_cap: usize,
    message_len: *mut usize,
) -> Error {
    ffi_guard!("tessera_session_search_start", {
        // SAFETY: per the contract.
        unsafe {
            refuse(refusal, SearchRefusal::None);
            write_message("", message, message_cap, message_len);
        }
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        if options & !(SEARCH_REGEX | SEARCH_CASE_SENSITIVE) != 0 {
            return Error::InvalidArgument;
        }
        let options = QueryOptions {
            regex: options & SEARCH_REGEX != 0,
            case_sensitive: options & SEARCH_CASE_SENSITIVE != 0,
        };
        let id = SessionId::from_raw(session_id);
        let refused = |kind: SearchRefusal| {
            // A refused pattern clears the search, as the core does.
            match app.core.search_clear(id) {
                Ok(()) => {
                    // SAFETY: per the contract.
                    unsafe { refuse(refusal, kind) };
                    Error::InvalidArgument
                }
                Err(error) => error::report("tessera_session_search_start", &error),
            }
        };
        // The length is checked before the pattern is read, so a bad
        // length never becomes a slice.
        if pattern_len > SEARCH_MAX_QUERY_BYTES {
            return refused(SearchRefusal::TooLong);
        }
        let bytes: &[u8] = if pattern_len == 0 {
            &[]
        } else if pattern.is_null() {
            return Error::NullPointer;
        } else {
            // SAFETY: valid for `pattern_len` bytes per the contract.
            unsafe { slice::from_raw_parts(pattern, pattern_len) }
        };
        let Ok(text) = std::str::from_utf8(bytes) else {
            return refused(SearchRefusal::NotUtf8);
        };
        match app.core.search_start(id, text, options) {
            Ok(()) => Error::Ok,
            Err(CoreError::Query(query)) => {
                let kind = match &query {
                    QueryError::TooLong { .. } => SearchRefusal::TooLong,
                    QueryError::Syntax(why) => {
                        // SAFETY: per the contract.
                        unsafe { write_message(why, message, message_cap, message_len) };
                        SearchRefusal::Syntax
                    }
                    QueryError::TooComplex => SearchRefusal::TooComplex,
                };
                // SAFETY: per the contract.
                unsafe { refuse(refusal, kind) };
                // The core cleared the search; the message quotes the
                // pattern, so only the kind is logged (S8).
                tracing::debug!(?kind, "search pattern refused");
                Error::InvalidArgument
            }
            Err(error) => error::report("tessera_session_search_start", &error),
        }
    })
}

/// Runs `f` on the live application `app` points to and stores the status
/// it returns in `*out` (zeroed first, and left zeroed on failure).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle; `out` must be `NULL` or
/// writable.
unsafe fn with_status(
    function: &'static str,
    app: *mut App,
    out: *mut SearchStatus,
    f: impl FnOnce(&App) -> tessera_app_core::Result<CoreStatus>,
) -> Error {
    if out.is_null() {
        return Error::NullPointer;
    }
    // SAFETY: non-null and writable per the contract.
    unsafe { out.write(SearchStatus::default()) };
    // SAFETY: the caller passes NULL or a live handle.
    let Some(app) = (unsafe { App::from_ptr(app) }) else {
        return Error::NullPointer;
    };
    match f(app) {
        Ok(status) => {
            // SAFETY: as above.
            unsafe { out.write(SearchStatus::from_core(status)) };
            Error::Ok
        }
        Err(error) => error::report(function, &error),
    }
}

/// Scans session `session_id`'s history for its search for about
/// `budget_us` microseconds (at most `TESSERA_SEARCH_MAX_STEP_MICROS`):
/// chunk after chunk until nothing is left or the budget has passed, at
/// least one chunk, each holding the terminal lock only to copy at most
/// 256 lines. The screen is searched first, then the history from newest
/// to oldest; new output is searched as it arrives. Stores the status in
/// `*out`. Call it once per display refresh while `out->done` is false or
/// output arrives; without a search it does nothing.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` or `out` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session. On
/// failure `*out` is zeroed (when `out` is not `NULL`).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `out` must be `NULL` or point to
/// a writable `TesseraSearchStatus`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_search_step(
    app: *mut App,
    session_id: u64,
    budget_us: u32,
    out: *mut SearchStatus,
) -> Error {
    ffi_guard!("tessera_session_search_step", {
        let budget = Duration::from_micros(u64::from(budget_us.min(SEARCH_MAX_STEP_MICROS)));
        // SAFETY: per the contract.
        unsafe {
            with_status("tessera_session_search_step", app, out, |app| {
                app.core
                    .search_step(SessionId::from_raw(session_id), budget)
            })
        }
    })
}

/// Makes the match before the current one (toward older output) current,
/// wrapping from the oldest to the newest, scrolls session `session_id`'s
/// view to show it when it is out of view (centred), and stores the status
/// in `*out`. Without matches nothing moves.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` or `out` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session. On
/// failure `*out` is zeroed (when `out` is not `NULL`).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `out` must be `NULL` or point to
/// a writable `TesseraSearchStatus`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_search_older(
    app: *mut App,
    session_id: u64,
    out: *mut SearchStatus,
) -> Error {
    ffi_guard!("tessera_session_search_older", {
        // SAFETY: per the contract.
        unsafe {
            with_status("tessera_session_search_older", app, out, |app| {
                app.core.search_older(SessionId::from_raw(session_id))
            })
        }
    })
}

/// Makes the match after the current one (toward newer output) current,
/// wrapping from the newest to the oldest, and scrolls to it as
/// `tessera_session_search_older` does.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` or `out` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session. On
/// failure `*out` is zeroed (when `out` is not `NULL`).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `out` must be `NULL` or point to
/// a writable `TesseraSearchStatus`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_search_newer(
    app: *mut App,
    session_id: u64,
    out: *mut SearchStatus,
) -> Error {
    ffi_guard!("tessera_session_search_newer", {
        // SAFETY: per the contract.
        unsafe {
            with_status("tessera_session_search_newer", app, out, |app| {
                app.core.search_newer(SessionId::from_raw(session_id))
            })
        }
    })
}

/// Stores where session `session_id`'s search stands in `*out`, without
/// scanning.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` or `out` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session. On
/// failure `*out` is zeroed (when `out` is not `NULL`).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `out` must be `NULL` or point to
/// a writable `TesseraSearchStatus`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_search_status(
    app: *mut App,
    session_id: u64,
    out: *mut SearchStatus,
) -> Error {
    ffi_guard!("tessera_session_search_status", {
        // SAFETY: per the contract.
        unsafe {
            with_status("tessera_session_search_status", app, out, |app| {
                app.core.search_status(SessionId::from_raw(session_id))
            })
        }
    })
}

/// Ends session `session_id`'s search: its matches and their highlights
/// go (from the next `tessera_surface_render`).
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session.
///
/// # Safety
///
/// `app` must be `NULL` or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_search_clear(app: *mut App, session_id: u64) -> Error {
    ffi_guard!("tessera_session_search_clear", {
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        error::status(
            "tessera_session_search_clear",
            app.core.search_clear(SessionId::from_raw(session_id)),
        )
    })
}

/// Copies the text of every match session `session_id`'s search has found
/// ("copy all matches"), in document order, joined by newlines, at most
/// `TESSERA_SEARCH_MAX_MATCHES_TEXT_BYTES` bytes; `*truncated` (when
/// `truncated` is not `NULL`) says the text stopped before a match that
/// would have passed that size.
///
/// Stores the text's length in bytes in `*len` and writes the UTF-8 bytes
/// (no NUL terminator) to `buf` when `cap` holds them all; without matches
/// `*len` is 0. The text is read again on each call, so call it once with
/// a buffer of `TESSERA_SEARCH_MAX_MATCHES_TEXT_BYTES` bytes rather than
/// once to size it.
///
/// Returns `TESSERA_ERROR_INVALID_ARGUMENT` when the text is longer than
/// `cap`: nothing is written, and `*len` holds the size needed. Returns
/// `TESSERA_ERROR_NULL_POINTER` if `app` or `len` is `NULL`, or `buf` is
/// `NULL` with text to write; `TESSERA_ERROR_SESSION_NOT_FOUND` if the id
/// is not a live session. On those failures `*len` is 0 (when `len` is not
/// `NULL`).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `len` must be `NULL` or point to
/// a writable `size_t`, `truncated` `NULL` or a writable `bool`, and `buf`
/// `NULL` or valid for `cap` bytes of writes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_search_matches_text(
    app: *mut App,
    session_id: u64,
    buf: *mut u8,
    cap: usize,
    len: *mut usize,
    truncated: *mut bool,
) -> Error {
    ffi_guard!("tessera_session_search_matches_text", {
        if len.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: non-null and writable per the contract.
        unsafe { len.write(0) };
        if !truncated.is_null() {
            // SAFETY: non-null and writable per the contract.
            unsafe { truncated.write(false) };
        }
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        match app
            .core
            .search_matches_text(SessionId::from_raw(session_id))
        {
            Ok(text) => {
                if !truncated.is_null() {
                    // SAFETY: as above.
                    unsafe { truncated.write(text.truncated) };
                }
                // SAFETY: `len` and `buf` per the contract.
                unsafe { copy_out(&text.text, buf, cap, len) }
            }
            Err(error) => error::report("tessera_session_search_matches_text", &error),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;

    #[test]
    fn a_message_is_cut_at_a_character_boundary() {
        let mut buf = [b'#'; 8];
        let mut len = 99;
        // SAFETY: `buf` holds 8 bytes; `len` is writable.
        unsafe { write_message("ab\u{e9}cd", buf.as_mut_ptr(), 3, &raw mut len) };
        assert_eq!((len, &buf[..4]), (2, &b"ab##"[..]));
        // SAFETY: as above.
        unsafe { write_message("ab\u{e9}cd", buf.as_mut_ptr(), 8, &raw mut len) };
        assert_eq!((len, &buf[..6]), (6, "ab\u{e9}cd".as_bytes()));
        // SAFETY: a NULL buffer is never written.
        unsafe { write_message("abc", ptr::null_mut(), 8, &raw mut len) };
        assert_eq!(len, 0);
        // SAFETY: a NULL length makes it a no-op.
        unsafe { write_message("abc", buf.as_mut_ptr(), 8, ptr::null_mut()) };
    }

    #[test]
    fn refusal_values_are_stable() {
        let values = [
            SearchRefusal::None,
            SearchRefusal::TooLong,
            SearchRefusal::Syntax,
            SearchRefusal::TooComplex,
            SearchRefusal::NotUtf8,
        ]
        .map(|refusal| refusal as u32);
        assert_eq!(values, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_status_without_a_current_match_says_so() {
        let status = SearchStatus::from_core(CoreStatus {
            active: true,
            matches: 0,
            current: None,
            done: true,
            truncated: false,
        });
        assert!(!status.has_current);
        let status = SearchStatus::from_core(CoreStatus {
            current: Some(3),
            matches: 4,
            ..CoreStatus::default()
        });
        assert_eq!((status.has_current, status.current), (true, 3));
    }
}
