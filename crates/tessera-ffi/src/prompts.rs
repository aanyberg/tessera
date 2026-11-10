//! Prompt navigation and the last command's output (ADR 0039).
//!
//! The shell marks its prompts and commands with OSC 133 (Tessera's
//! shell integration scripts send them); these calls read those marks.
//! Call them from the UI (main) thread, like the selection calls: ⌘↑ and
//! ⌘↓ come back from `tessera_session_handle_key` as the commands
//! `prompt_previous` and `prompt_next`, which the host runs with
//! `tessera_session_scroll_to_prompt`; View > Select Last Output runs
//! `tessera_session_select_last_output`, and its menu item is enabled by
//! `tessera_session_has_command_output`. The exit-status marks the view
//! draws in its left padding (#45b) are explained on hover by
//! `tessera_session_command_at`.

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

/// The exit-status mark on one row of a session's view (#45b), as
/// `tessera_session_command_at` reports it for the mark's hover. All zero
/// (`found` false) for a row with no mark.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommandMark {
    /// `true` when the row starts the prompt of a finished command, which
    /// the view marks in its left padding.
    pub found: bool,
    /// `true` when the shell reported the command's exit status
    /// (`exit_code` holds it); `false` when it did not (the mark is drawn
    /// neutral).
    pub exit_known: bool,
    /// The exit status when `exit_known`, otherwise 0. 0 is success
    /// (drawn green), anything else failure (drawn red).
    pub exit_code: i32,
    /// `true` when the core knows how long the command ran (`duration_ms`
    /// holds it). `false` after a resize re-wrapped the lines, and for a
    /// command that ran in the same chunk of output as the next one.
    pub duration_known: bool,
    /// How long the command ran, in milliseconds (rounded down, saturating),
    /// when `duration_known`; otherwise 0.
    pub duration_ms: u64,
}

impl From<tessera_app_core::CommandMark> for CommandMark {
    fn from(mark: tessera_app_core::CommandMark) -> Self {
        Self {
            found: true,
            exit_known: mark.exit_code.is_some(),
            exit_code: mark.exit_code.unwrap_or(0),
            duration_known: mark.duration.is_some(),
            duration_ms: mark
                .duration
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        }
    }
}

/// Stores in `*out` the exit-status mark on row `row` (0 is the top row
/// on screen, counted in the view as it is scrolled now) of session
/// `session_id`'s view, for the mark's hover: its exit status and how long
/// the command ran. The status is the one the mark is drawn in. Nothing is
/// logged. It reads the rows in view and at most 1024 lines below them,
/// so it is cheap enough to call on every mouse move over the padding.
///
/// Returns `TESSERA_ERROR_NULL_POINTER` if `app` or `out` is `NULL`,
/// `TESSERA_ERROR_SESSION_NOT_FOUND` if the id is not a live session. On
/// failure, and for a row with no mark, `*out` is all zero (when `out` is
/// not `NULL`).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. `out` must be `NULL` or point to
/// a writable `TesseraCommandMark`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_session_command_at(
    app: *mut App,
    session_id: u64,
    row: u16,
    out: *mut CommandMark,
) -> Error {
    ffi_guard!("tessera_session_command_at", {
        if out.is_null() {
            return Error::NullPointer;
        }
        // SAFETY: non-null and writable per the contract.
        unsafe { out.write(CommandMark::default()) };
        // SAFETY: the caller passes NULL or a live handle.
        let Some(app) = (unsafe { App::from_ptr(app) }) else {
            return Error::NullPointer;
        };
        match app
            .core
            .command_mark_at(SessionId::from_raw(session_id), row)
        {
            Ok(mark) => {
                if let Some(mark) = mark {
                    // SAFETY: as above.
                    unsafe { out.write(mark.into()) };
                }
                Error::Ok
            }
            Err(error) => error::report("tessera_session_command_at", &error),
        }
    })
}
