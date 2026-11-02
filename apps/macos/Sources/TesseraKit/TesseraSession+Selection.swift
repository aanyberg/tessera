import Foundation
internal import TesseraCore

extension TesseraSession {
    /// What a click selects and a drag extends by: `TesseraGranularity`.
    public enum SelectionGranularity: UInt32, Sendable, Equatable {
        /// Single cells: a click, then a drag.
        case character = 0
        /// Words: a double-click.
        case word = 1
        /// Whole logical lines, soft-wrapped rows included: a triple-click
        /// or more.
        case line = 2
    }

    // Cells are view-relative: row 0 is the top row the surface shows at its
    // scroll offset. Every call is made on the main actor like the input
    // calls, and throws ``TesseraCoreError/sessionNotFound`` once the
    // session is closed and ``TesseraCoreError/nullPointer`` after the
    // application shut down.

    /// Starts a selection at the cell (a mouse-down), replacing any earlier
    /// one. A character click selects nothing until ``selectExtend(col:row:)``;
    /// `block` asks for a rectangle.
    @MainActor
    public func selectBegin(col: UInt16, row: UInt16, granularity: SelectionGranularity, block: Bool) throws {
        try TesseraCoreError.check(
            tessera_session_select_begin(core.pointer, id, col, row, granularity.rawValue, block))
    }

    /// Extends the selection to the cell (a drag), by the granularity it
    /// began with. Nothing happens without a selection begun.
    @MainActor
    public func selectExtend(col: UInt16, row: UInt16) throws {
        try TesseraCoreError.check(tessera_session_select_extend(core.pointer, id, col, row))
    }

    /// Moves the end of the selection nearer to the cell (a shift-click),
    /// keeping its granularity and block-ness. Nothing happens without a
    /// selection.
    @MainActor
    public func selectAdjust(col: UInt16, row: UInt16) throws {
        try TesseraCoreError.check(tessera_session_select_adjust(core.pointer, id, col, row))
    }

    /// Selects the scrollback and the screen (the screen only on the
    /// alternate screen).
    @MainActor
    public func selectAll() throws {
        try TesseraCoreError.check(tessera_session_select_all(core.pointer, id))
    }

    /// Drops the selection.
    @MainActor
    public func selectClear() throws {
        try TesseraCoreError.check(tessera_session_select_clear(core.pointer, id))
    }

    /// Whether something is selected (Copy is possible), also while it is
    /// scrolled out of view or holds only blanks. A selection whose text
    /// changed under it is dropped first.
    @MainActor
    public var hasSelection: Bool {
        get throws {
            var has = false
            try TesseraCoreError.check(tessera_session_has_selection(core.pointer, id, &has))
            return has
        }
    }

    /// The selected text for Copy, or `nil` without a selection. A selection
    /// of blanks only gives `""`.
    @MainActor
    public func selectionText() throws -> String? {
        guard try hasSelection else { return nil }
        return try Self.readText { buffer, capacity, length in
            tessera_session_selection_text(core.pointer, id, buffer, capacity, length)
        }
    }

    /// The URL under the cell, as the core detects it (and, once OSC 8 is
    /// stored, as a program wrote it), or `nil`. Whether it may be opened is
    /// the caller's decision.
    @MainActor
    public func link(col: UInt16, row: UInt16) throws -> String? {
        let url = try Self.readText { buffer, capacity, length in
            tessera_session_link_at(core.pointer, id, col, row, buffer, capacity, length)
        }
        return url.isEmpty ? nil : url
    }

    /// Sets the cell the pointer hovers over with Command held, so the core
    /// underlines the link there from the next frame on, or clears it with
    /// `nil`.
    @MainActor
    public func setHover(_ cell: (col: UInt16, row: UInt16)?) throws {
        try TesseraCoreError.check(
            tessera_session_set_hover(core.pointer, id, cell?.col ?? 0, cell?.row ?? 0, cell != nil))
    }

    /// Runs a call of the core's two-step text protocol: ask for the size
    /// with no buffer, then read into one of that size. The text may change
    /// between the calls (output arrives on a core thread), so a text that
    /// outgrew the buffer is asked for again.
    @MainActor
    private static func readText(
        _ call: (UnsafeMutablePointer<UInt8>?, Int, UnsafeMutablePointer<Int>) -> TesseraError
    ) throws -> String {
        var capacity = 0
        for _ in 0..<8 {
            var length = 0
            var bytes = [UInt8](repeating: 0, count: capacity)
            let status = bytes.withUnsafeMutableBufferPointer { call($0.baseAddress, capacity, &length) }
            if status == TESSERA_ERROR_INVALID_ARGUMENT, length > capacity {
                capacity = length
                continue
            }
            try TesseraCoreError.check(status)
            return String(decoding: bytes.prefix(length), as: UTF8.self)
        }
        throw TesseraCoreError.invalidArgument
    }
}
