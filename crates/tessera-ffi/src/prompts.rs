//! Prompt navigation and the last command's output (ADR 0039).
//!
//! The shell marks its prompts and commands with OSC 133 (Tessera's
//! shell integration scripts send them); these calls read those marks.
//! Call them from the UI (main) thread, like the selection calls: ⌘↑ and
//! ⌘↓ come back from `tessera_session_handle_key` as the commands
//! `prompt_previous` and `prompt_next`, which the host runs with
//! `tessera_session_scroll_to_prompt`; View > Select Last Output runs
//! `tessera_session_select_last_output`, and its menu item is enabled by
//! `tessera_session_has_command_output`.

use tessera_app_core::{Result as CoreResult, ScrollTarget, SessionId};

use crate::app::App;
use crate::error::{self, Error};
use crate::panic::ffi_guard;

/// Runs `f` on the live application `app` points to and stores its answer
/// in `*out` (`false` on failure).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle; `out` must be `NULL` or point to
/// a writable `bool`.
unsafe fn answer(
    function: &'static str,
    app: *mut App,
    out: *mut bool,
    f: impl FnOnce(&App) -> CoreResult<bool>,
) -> Error {
    if out.is_null() {
        return Error::NullPointer;
    }
    // SAFETY: non-null and writable per the contract.
    unsafe { out.write(false) };
    // SAFETY: the caller passes NULL or a live handle.
    let Some(app) = (unsafe { App::from_ptr(app) }) else {
        return Error::NullPointer;
    };
    match f(app) {
        Ok(value) => {
            // SAFETY: as above.
            unsafe { out.write(value) };
            Error::Ok
        }
        Err(error) => error::report(function, &error),
    }
}

/// Scrolls session `session_id`'s view to the shell's previous prompt
/// (`previous` true: the nearest prompt above the view's top row) or its
/// next one (the nearest below it, or the live screen after the newest),
/// and stores in `*moved` whether the view moved.
///
/// The view stops at the oldest prompt and at the live screen. When the
/// scrollback holds no prompt mark (a shell without integration), the
/// call pages instead, like `TESSERA_SCROLL_TARGET_PAGE_UP` or
/// `_PAGE_DOWN`, however long the history. On the alternate screen (a
/// full-screen application) nothing moves. A search of the history reads
/// at most 1 000 000 lines, a chunk at a time, so output is never blocked
/// for long; a prompt further away is not found and nothing moves, but
/// when that search finds no prompt at all the call pages.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` or `moved` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session. On
/// failure `*moved` is `false` (when `moved` is not `NULL`).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `moved` must be `NULL` or point
/// to a writable `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_scroll_to_prompt(
    app: *mut App,
    session_id: u64,
    previous: bool,
    moved: *mut bool,
) -> Error {
    ffi_guard!("tessera_session_scroll_to_prompt", {
        let target = if previous {
            ScrollTarget::PreviousPrompt
        } else {
            ScrollTarget::NextPrompt
        };
        // SAFETY: guaranteed by the caller.
        unsafe {
            answer("tessera_session_scroll_to_prompt", app, moved, |app| {
                app.core.scroll_to(SessionId::from_raw(session_id), target)
            })
        }
    })
}

/// Selects the output of session `session_id`'s last finished command:
/// the lines from its first output line to the line before the next
/// prompt, without the trailing blank lines, and stores in `*selected`
/// whether it selected anything. The view scrolls to the output's start
/// when it is not visible. Copy (`tessera_session_selection_text`) then
/// gives its text.
///
/// A command still running is skipped. Nothing is selected, the earlier
/// selection stays and `*selected` is `false` when the last finished
/// command printed nothing, when its output's first line has left the
/// scrollback, without OSC 133 marks, and on the alternate screen.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` or `selected` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session. On
/// failure `*selected` is `false` (when `selected` is not `NULL`).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `selected` must be `NULL` or
/// point to a writable `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_select_last_output(
    app: *mut App,
    session_id: u64,
    selected: *mut bool,
) -> Error {
    ffi_guard!("tessera_session_select_last_output", {
        // SAFETY: guaranteed by the caller.
        unsafe {
            answer("tessera_session_select_last_output", app, selected, |app| {
                app.core
                    .select_command_output(SessionId::from_raw(session_id))
            })
        }
    })
}

/// Stores in `*out` whether session `session_id` holds a finished command
/// whose output Select Last Output may select (to enable its menu item):
/// one that printed at least one line still in the scrollback. It reads
/// the session's command records only, without searching the history, so
/// it is cheap enough for every menu validation; when it is `true`,
/// `tessera_session_select_last_output` can still find nothing to select
/// (for example after a resize moved the lines).
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` or `out` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session. On
/// failure `*out` is `false` (when `out` is not `NULL`).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `out` must be `NULL` or point to
/// a writable `bool`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_has_command_output(
    app: *mut App,
    session_id: u64,
    out: *mut bool,
) -> Error {
    ffi_guard!("tessera_session_has_command_output", {
        // SAFETY: guaranteed by the caller.
        unsafe {
            answer("tessera_session_has_command_output", app, out, |app| {
                app.core.has_command_output(SessionId::from_raw(session_id))
            })
        }
    })
}
