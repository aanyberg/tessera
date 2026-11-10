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
    /// history per call, a chunk at a time: a prompt further away is not
    /// found and nothing moves, but when that look finds no prompt at all
    /// the call pages. Nothing moves on the alternate screen.
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

    /// The finished command whose prompt is on the view's `row` (0 is the
    /// top), as the exit-status mark in the left padding draws it (#45b,
    /// ADR 0039), or `nil` when that row has no mark: no prompt there, the
    /// command is running or ran nothing, the shell sends no marks, or the
    /// row is outside the view. The exit status comes from the screen's
    /// marks; the duration from the session's command records, which keep
    /// the last 1000 commands and forget a command's lines when a resize
    /// re-wraps the history, so ``CommandMark/duration`` is `nil` then.
    @MainActor
    public func commandMark(atRow row: UInt16) throws -> CommandMark? {
        var mark = TesseraCommandMark()
        try TesseraCoreError.check(tessera_session_command_at(core.pointer, id, row, &mark))
        guard mark.found else { return nil }
        return CommandMark(
            exitCode: mark.exit_known ? mark.exit_code : nil,
            duration: mark.duration_known ? .milliseconds(mark.duration_ms) : nil)
    }

    /// A finished command's exit-status mark: see ``commandMark(atRow:)``.
    public struct CommandMark: Equatable, Sendable {
        /// The status the shell reported (`D;<code>`), `nil` when it sent
        /// none.
        public var exitCode: Int32?
        /// How long the command ran, `nil` when the session's records no
        /// longer know it (too old, after a resize, or a command the shell
        /// reported without its start).
        public var duration: Duration?

        /// A mark with these fields.
        public init(exitCode: Int32?, duration: Duration?) {
            self.exitCode = exitCode
            self.duration = duration
        }
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
