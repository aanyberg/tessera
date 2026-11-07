import Foundation
internal import TesseraCore

extension TesseraSession {
    // The shell's prompts and commands, by the OSC 133 marks Tessera's shell
    // integration sends (ADR 0039). Every call is made on the main actor like
    // the input calls, and throws ``TesseraCoreError/sessionNotFound`` once
    // the session is closed and ``TesseraCoreError/nullPointer`` after the
    // application shut down.

    /// Scrolls the view to the shell's previous prompt (`previous`: the
    /// nearest above the view's top row) or its next one (the nearest below
    /// it, or the live screen after the newest), and returns whether the view
    /// moved. It stops at the oldest prompt and at the live screen; with no
    /// prompt mark in the scrollback (a shell without integration) it pages
    /// instead, like ``ScrollTarget/pageUp`` and ``ScrollTarget/pageDown``,
    /// however long the history. The core reads at most 1 000 000 lines of
    /// history per call: a prompt further away is not found and nothing
    /// moves. Nothing moves on the alternate screen.
    @MainActor
    @discardableResult
    public func scrollToPrompt(previous: Bool) throws -> Bool {
        var moved = false
        try TesseraCoreError.check(tessera_session_scroll_to_prompt(core.pointer, id, previous, &moved))
        return moved
    }

    /// Selects the output of the last finished command (a running one is
    /// skipped) without the prompts around it or trailing blank lines, and
    /// scrolls to its start if it is not visible; returns whether it selected
    /// anything. Nothing is selected, and the earlier selection stays, when
    /// that command printed nothing, its output left the scrollback, the
    /// shell sends no marks, or on the alternate screen.
    @MainActor
    @discardableResult
    public func selectLastOutput() throws -> Bool {
        var selected = false
        try TesseraCoreError.check(tessera_session_select_last_output(core.pointer, id, &selected))
        return selected
    }

    /// Whether the last finished command printed output still in the
    /// scrollback, from the session's command records alone (no search of
    /// the history): cheap enough to enable Select Last Output in
    /// `validateMenuItem(_:)`. ``selectLastOutput()`` can still find nothing
    /// (after a resize moved the lines, say).
    @MainActor
    public var hasCommandOutput: Bool {
        get throws {
            var has = false
            try TesseraCoreError.check(tessera_session_has_command_output(core.pointer, id, &has))
            return has
        }
    }
}
