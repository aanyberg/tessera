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

    /// The selection for Copy with Formatting (ADR 0042): the plain text
    /// ``selectionText()`` gives, and the same text as an HTML fragment and
    /// an RTF document with the cells' colours, bold, italic, underline and
    /// strikethrough, made and escaped by the core (the host never parses
    /// them). `nil` without a selection.
    public struct FormattedSelection: Sendable, Equatable {
        /// The plain text, as Copy copies it.
        public let plain: String
        /// The HTML fragment (UTF-8), or `nil` when the formatted copy
        /// would pass the core's 16 MiB cap: copy the plain text alone.
        public let html: Data?
        /// The RTF document (ASCII), or `nil` with ``html``.
        public let rtf: Data?
    }

    /// The selection with its formatting for Copy with Formatting, or `nil`
    /// without a selection. A selection of blanks only gives an empty
    /// ``FormattedSelection/plain`` and no formatting.
    ///
    /// The core formats the selection on every call, so the first call
    /// already passes buffers sized from an estimate
    /// (``formattedCapacities()``): one formatting pass in the common case.
    @MainActor
    public func selectionWithFormatting() throws -> FormattedSelection? {
        guard try hasSelection else { return nil }
        return try readFormatted { html, rtf, plain in
            tessera_session_selection_rich_text(core.pointer, id, &html, &rtf, &plain)
        }
    }

    /// ``selectionWithFormatting()``'s reading, through `call` (the core's
    /// rich-text call, or a test's counting wrapper of it), starting from
    /// the estimate.
    @MainActor
    func readFormatted(
        _ call: (inout TesseraTextBuffer, inout TesseraTextBuffer, inout TesseraTextBuffer) -> TesseraError
    ) throws -> FormattedSelection {
        try Self.readFormatted(capacities: try formattedCapacities(), call)
    }

    /// The most bytes the core's HTML or RTF may have (its 16 MiB cap, past
    /// which it copies the plain text alone): no estimate asks for more.
    static let maxFormattedBytes = 16 << 20
    /// What the HTML and RTF take beyond their text: the RTF colour table
    /// and header, the HTML's outer element.
    static let formattedOverhead = 16 << 10

    /// The buffer sizes (HTML, RTF, plain) the first formatting call
    /// passes: the plain text's size, which the core reports without
    /// formatting anything, and for HTML and RTF eight times that plus
    /// ``formattedOverhead``, at most ``maxFormattedBytes``. A selection
    /// whose styles or escapes need more is asked for again.
    @MainActor
    func formattedCapacities() throws -> [Int] {
        var plain = 0
        let status = tessera_session_selection_text(core.pointer, id, nil, 0, &plain)
        if status != TESSERA_ERROR_INVALID_ARGUMENT { try TesseraCoreError.check(status) }
        return Self.formattedCapacities(plainLength: plain)
    }

    static func formattedCapacities(plainLength: Int) -> [Int] {
        let room = (maxFormattedBytes - formattedOverhead) / 8
        let rich = plainLength >= room ? maxFormattedBytes : plainLength * 8 + formattedOverhead
        return [rich, rich, plainLength]
    }

    /// Runs `call` (the core's rich-text call, or a test's stub) into
    /// buffers of `capacities` (HTML, RTF, plain). Output arriving between
    /// two calls may grow the text, so a text that outgrew its buffer is
    /// asked for again at the sizes the core reported, which for HTML and
    /// RTF never pass ``maxFormattedBytes`` (it refuses past that).
    static func readFormatted(
        capacities initial: [Int],
        _ call: (inout TesseraTextBuffer, inout TesseraTextBuffer, inout TesseraTextBuffer) -> TesseraError
    ) throws -> FormattedSelection {
        var capacities = initial
        for _ in 0..<8 {
            var html = [UInt8](repeating: 0, count: capacities[0])
            var rtf = [UInt8](repeating: 0, count: capacities[1])
            var plain = [UInt8](repeating: 0, count: capacities[2])
            var lengths = [0, 0, 0]
            let status = html.withUnsafeMutableBufferPointer { html in
                rtf.withUnsafeMutableBufferPointer { rtf in
                    plain.withUnsafeMutableBufferPointer { plain in
                        var htmlOut = TesseraTextBuffer(buf: html.baseAddress, cap: capacities[0], len: 0)
                        var rtfOut = TesseraTextBuffer(buf: rtf.baseAddress, cap: capacities[1], len: 0)
                        var plainOut = TesseraTextBuffer(buf: plain.baseAddress, cap: capacities[2], len: 0)
                        let status = call(&htmlOut, &rtfOut, &plainOut)
                        lengths = [htmlOut.len, rtfOut.len, plainOut.len]
                        return status
                    }
                }
            }
            if status == TESSERA_ERROR_INVALID_ARGUMENT, zip(lengths, capacities).contains(where: { $0 > $1 }) {
                // The core reports no formatted size past its cap; one
                // that does breaks the contract and is not allocated.
                guard lengths[0] <= maxFormattedBytes, lengths[1] <= maxFormattedBytes else { break }
                capacities = zip(lengths, capacities).map { Swift.max($0, $1) }
                continue
            }
            let text = String(decoding: plain.prefix(lengths[2]), as: UTF8.self)
            if status == TESSERA_ERROR_REFUSED {
                return FormattedSelection(plain: text, html: nil, rtf: nil)
            }
            try TesseraCoreError.check(status)
            guard lengths[2] > 0 else { return FormattedSelection(plain: "", html: nil, rtf: nil) }
            return FormattedSelection(
                plain: text, html: Data(html.prefix(lengths[0])), rtf: Data(rtf.prefix(lengths[1])))
        }
        throw TesseraCoreError.invalidArgument
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
