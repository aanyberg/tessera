//! Command-finished notifications (#470, ADR 0048): the decision the host
//! asks for on each `on_session_command_finished`.

use std::time::Duration;

use tessera_app_core::NotifyDecision;

use crate::app::App;
use crate::panic::ffi_guard;

/// `tessera_app_command_finished_decision`: neither mark nor post.
pub const COMMAND_FINISHED_NOTHING: u8 = 0;
/// `tessera_app_command_finished_decision`: mark the pane, post nothing
/// (the rate limit dropped the notification).
pub const COMMAND_FINISHED_MARK: u8 = 1;
/// `tessera_app_command_finished_decision`: mark the pane and post.
pub const COMMAND_FINISHED_MARK_AND_POST: u8 = 2;

/// Most UTF-8 bytes of the tab title a notification shows (S3); the host
/// cuts it on a character boundary.
pub const MAX_NOTIFICATION_TEXT_BYTES: usize = 256;
/// Most notifications posted in any 60 s, app-wide (S6).
pub const MAX_NOTIFICATIONS_PER_MINUTE: usize = 10;

const _: () = assert!(COMMAND_FINISHED_NOTHING == NotifyDecision::Nothing as u8);
const _: () = assert!(COMMAND_FINISHED_MARK == NotifyDecision::Mark as u8);
const _: () = assert!(COMMAND_FINISHED_MARK_AND_POST == NotifyDecision::MarkAndPost as u8);
const _: () = assert!(MAX_NOTIFICATION_TEXT_BYTES == tessera_app_core::MAX_NOTIFICATION_TEXT_BYTES);
const _: () =
    assert!(MAX_NOTIFICATIONS_PER_MINUTE == tessera_app_core::MAX_NOTIFICATIONS_PER_MINUTE);

/// What the host does about a command that finished after `duration_ms`
/// milliseconds (`on_session_command_finished`) in a pane that had
/// (`focused`) or did not have the keyboard when it finished: the app is
/// active, the pane's window key, its tab selected and the pane the
/// window's focused one (ADR 0048 D1). Returns
/// `TESSERA_COMMAND_FINISHED_NOTHING` (0), `_MARK` (1: mark the pane, post
/// nothing) or `_MARK_AND_POST` (2).
///
/// Decided against `[notifications]` of the configuration in effect now
/// (`command_finished = "unfocused"`, a duration of at least
/// `command_finished_after` seconds, the pane unfocused; the exit status
/// never matters), then, only for a finish that qualifies, an app-wide
/// limit of `TESSERA_MAX_NOTIFICATIONS_PER_MINUTE` posts in any 60 s, which
/// this call counts: ask once per finish. Returns 0 for a `NULL` `app`
/// (or a panic, which is logged).
///
/// # Safety
///
/// `app` must be `NULL` or a live handle. Main thread only, like every
/// `tessera_app_*` call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tessera_app_command_finished_decision(
    app: *const App,
    duration_ms: u64,
    focused: bool,
) -> u8 {
    let mut decision = COMMAND_FINISHED_NOTHING;
    ffi_guard!(void "tessera_app_command_finished_decision", {
        // SAFETY: the caller passes NULL or a live handle.
        if let Some(app) = unsafe { App::from_ptr(app) } {
            decision = app
                .core
                .command_finished_decision(Duration::from_millis(duration_ms), focused)
                as u8;
        }
    });
    decision
}
