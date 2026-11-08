import Foundation
internal import TesseraCore

extension TesseraSession {
    /// A snapshot of the text the session's view shows, for assistive
    /// clients such as VoiceOver (ADR 0041): the rows at the view's scroll
    /// offset, each with its trailing blanks trimmed, one `"\n"` after every
    /// row but the last kept, with the cursor and the selection as offsets
    /// into it.
    ///
    /// Offsets and lengths are UTF-16 code units, as `NSRange` counts them;
    /// line `n` is row `n` of the view; cells are view-relative, row 0 the
    /// top row the view shows. The core builds the text and answers every
    /// lookup, so the Swift side never maps cells to characters itself.
    ///
    /// The text is the child's (docs/security.md S3): the core sanitises it
    /// (no control character, no bidirectional formatting control, no line
    /// or paragraph separator) and caps it. Never log it (S8).
    ///
    /// A snapshot owns its data and never changes, so it stays valid after
    /// the session closes; it is freed when the last reference goes.
    public final class AccessibleText {
        /// A rectangle of cells, view-relative.
        public struct Cells: Sendable, Equatable {
            public var row: Int
            public var col: Int
            /// At least 1.
            public var rows: Int
            /// 0 for an empty range.
            public var cols: Int

            public init(row: Int, col: Int, rows: Int, cols: Int) {
                self.row = row
                self.col = col
                self.rows = rows
                self.cols = cols
            }
        }

        private let handle: OpaquePointer

        /// The text. `(string as NSString).length` is ``length``.
        public let string: String
        /// The text's length in UTF-16 units.
        public let length: Int
        /// The number of lines: at least 1, except 0 for a view scrolled back
        /// over an empty history.
        public let lineCount: Int
        /// The cursor's offset, or ``length`` while the view is scrolled back
        /// and shows no cursor.
        public let insertionPoint: Int
        /// Whether the view shows the cursor.
        public let hasCursor: Bool
        /// The selection clipped to the text, or an empty range at
        /// ``insertionPoint`` without one in view. Never `NSNotFound`.
        public let selectedRange: NSRange
        /// Whether ``selectedRange`` is a selection.
        public let hasSelection: Bool

        fileprivate init(handle: OpaquePointer) throws {
            self.handle = handle
            var contents = TesseraAccessibleTextContents()
            do {
                try TesseraCoreError.check(tessera_accessible_text_contents(handle, &contents))
            } catch {
                tessera_accessible_text_free(handle)
                throw error
            }
            string = String(utf16CodeUnits: contents.text, count: contents.length)
            length = contents.length
            lineCount = contents.line_count
            insertionPoint = contents.insertion_point
            hasCursor = contents.has_cursor
            selectedRange = NSRange(location: contents.selected.location, length: contents.selected.length)
            hasSelection = contents.has_selection
        }

        deinit {
            tessera_accessible_text_free(handle)
        }

        /// The line holding offset `index`; an offset at or past the end
        /// gives the last line, an empty text line 0.
        public func line(forIndex index: Int) -> Int {
            var line = 0
            _ = tessera_accessible_text_line_for_index(handle, max(0, index), &line)
            return line
        }

        /// Line `line`'s range, its `"\n"` included (the last line has
        /// none), or `nil` past the last line.
        public func range(forLine line: Int) -> NSRange? {
            guard line >= 0 else { return nil }
            var range = TesseraTextRange()
            guard tessera_accessible_text_line_range(handle, line, &range) == TESSERA_ERROR_OK else { return nil }
            return NSRange(location: range.location, length: range.length)
        }

        /// The range of the character holding offset `index`: a whole
        /// grapheme cluster, a `"\n"` alone, an empty range at the end for an
        /// offset at or past it.
        public func characterRange(at index: Int) -> NSRange {
            var range = TesseraTextRange()
            _ = tessera_accessible_text_char_range(handle, max(0, index), &range)
            return NSRange(location: range.location, length: range.length)
        }

        /// The range of the character in cell (`col`, `row`): the whole
        /// character for a cell with text (either half of a wide one), the
        /// line's last character right of its text, an empty range at the
        /// start of an empty line, an empty range at the end below the last
        /// line.
        public func range(atCol col: UInt16, row: UInt16) -> NSRange {
            var range = TesseraTextRange()
            _ = tessera_accessible_text_range_at_cell(handle, col, row, &range)
            return NSRange(location: range.location, length: range.length)
        }

        /// The cells `range` covers (clamped to the text), as the rectangle
        /// that encloses them; an empty range is 0 columns wide at the left
        /// edge of its cell.
        public func cells(for range: NSRange) -> Cells {
            let raw = TesseraTextRange(location: max(0, range.location), length: max(0, range.length))
            var rect = TesseraCellRect()
            _ = tessera_accessible_text_cells_for_range(handle, raw, &rect)
            return Cells(row: rect.row, col: rect.col, rows: rect.rows, cols: rect.cols)
        }

        /// What to announce for the output that turned `previous` (an
        /// earlier snapshot of the same view) into this one: the new lines
        /// down to the cursor's, without the old cursor line when unchanged
        /// and without blank ones, joined with `"\n"`, at most
        /// `TESSERA_MAX_ANNOUNCEMENT_CHARS` characters (the newest kept).
        /// Empty when the cursor stayed on its line or either view is
        /// scrolled back.
        public func announcement(since previous: AccessibleText) -> String {
            var length = 0
            guard tessera_accessible_text_announcement(previous.handle, handle, nil, 0, &length) != TESSERA_ERROR_OK,
                length > 0
            else { return "" }
            var units = [UInt16](repeating: 0, count: length)
            let status = units.withUnsafeMutableBufferPointer { buffer in
                tessera_accessible_text_announcement(previous.handle, handle, buffer.baseAddress, buffer.count, &length)
            }
            guard status == TESSERA_ERROR_OK else { return "" }
            return String(decoding: units.prefix(length), as: UTF16.self)
        }
    }

    /// Takes a snapshot of the text the view shows now. Holds the session's
    /// terminal lock only to copy the rows, as a frame does. Call it when an
    /// assistive client asks or is attached, never per output callback
    /// without one (docs/security.md S6).
    ///
    /// - Throws: ``TesseraCoreError/sessionNotFound`` once the session is
    ///   closed and ``TesseraCoreError/nullPointer`` after the application
    ///   shut down.
    @MainActor
    public func accessibleText() throws -> AccessibleText {
        var handle: OpaquePointer?
        try TesseraCoreError.check(tessera_session_accessible_text(core.pointer, id, &handle))
        guard let handle else { throw TesseraCoreError.nullPointer }
        return try AccessibleText(handle: handle)
    }
}
