import Foundation
internal import TesseraCore

extension TesseraSession {
    /// How a search compares case (ADR 0038, #46b): the find bar's three
    /// states.
    public enum SearchCase: Sendable, Equatable, CaseIterable {
        /// Smart case: ignores case unless the query has an uppercase
        /// letter (`foo` finds `FOO`, `Foo` does not find `foo`).
        case auto
        /// Matches case.
        case sensitive
        /// Ignores case.
        case insensitive

        /// Whether a search for `query` matches case. In a regex the
        /// character after a backslash is not counted (`\W`, `\S`, `\B`
        /// are classes, not letters), nor the text of a `\p{…}` or `\P{…}`
        /// class name.
        public func isSensitive(for query: String, regex: Bool) -> Bool {
            switch self {
            case .sensitive: true
            case .insensitive: false
            case .auto: Self.hasUppercaseLetter(query, regex: regex)
            }
        }

        static func hasUppercaseLetter(_ query: String, regex: Bool) -> Bool {
            enum State { case text, escaped, className, bracedName }
            var state = State.text
            for character in query {
                switch state {
                case .escaped:
                    state = character == "p" || character == "P" ? .className : .text
                case .className:
                    // `\p{Greek}` up to its brace, `\pL` its one letter.
                    state = character == "{" ? .bracedName : .text
                case .bracedName:
                    if character == "}" { state = .text }
                case .text:
                    if regex && character == "\\" {
                        state = .escaped
                    } else if character.isUppercase {
                        return true
                    }
                }
            }
            return false
        }
    }

    /// Where a session's search stands (`TesseraSearchStatus`).
    public struct SearchStatus: Sendable, Equatable {
        /// Matches found so far in the lines the session shows (the
        /// alternate screen's only while it shows), at most
        /// ``TesseraSession/searchMaxMatches``.
        public var matches: Int
        /// The current match's index in document order (0 is the oldest),
        /// or `nil` without one.
        public var current: Int?
        /// A query is set.
        public var active: Bool
        /// Nothing was left to scan at the last step; output turns it
        /// false again.
        public var done: Bool
        /// More than ``TesseraSession/searchMaxMatches`` matched: the
        /// count reads "more than". Clears once the lines whose matches
        /// gave way are gone.
        public var truncated: Bool
        /// Some lines were too slow for the pattern to search whole (a row
        /// slower than `max(4 x budget, 250 ms)` at the smallest piece was
        /// skipped, the budget being the step's, or a line was cut into
        /// smaller pieces): matches there may be missing from the count.
        /// Clears once those lines are gone.
        public var linesSkipped: Bool

        public init(
            matches: Int = 0, current: Int? = nil, active: Bool = false, done: Bool = false, truncated: Bool = false,
            linesSkipped: Bool = false
        ) {
            self.matches = matches
            self.current = current
            self.active = active
            self.done = done
            self.truncated = truncated
            self.linesSkipped = linesSkipped
        }

        init(_ status: TesseraSearchStatus) {
            self.init(
                matches: Int(status.matches), current: status.has_current ? Int(status.current) : nil,
                active: status.active, done: status.done, truncated: status.truncated,
                linesSkipped: status.lines_skipped)
        }
    }

    /// Why ``searchStart(_:regex:caseSensitive:)`` refused a pattern.
    public enum SearchRefusal: Error, Sendable, Equatable {
        /// Longer than ``TesseraSession/searchMaxQueryBytes`` bytes.
        case tooLong
        /// Not a valid regex. The message is the core's, which quotes the
        /// pattern: show it only where the pattern itself may be shown,
        /// and never log it (S8).
        case syntax(message: String)
        /// A valid regex too large to search.
        case tooComplex
        /// Not UTF-8 (a Swift string always is).
        case notUTF8
    }

    /// The most matches a search keeps (`TESSERA_SEARCH_MAX_MATCHES`).
    public static let searchMaxMatches = Int(TESSERA_SEARCH_MAX_MATCHES)
    /// The longest pattern in bytes (`TESSERA_SEARCH_MAX_QUERY_BYTES`).
    public static let searchMaxQueryBytes = Int(TESSERA_SEARCH_MAX_QUERY_BYTES)
    /// The longest budget of one step (`TESSERA_SEARCH_MAX_STEP_MICROS`).
    public static let searchMaxStepMicros = UInt32(TESSERA_SEARCH_MAX_STEP_MICROS)
    /// The most bytes ``searchMatchesText()`` returns
    /// (`TESSERA_SEARCH_MAX_MATCHES_TEXT_BYTES`).
    public static let searchMaxMatchesTextBytes = Int(TESSERA_SEARCH_MAX_MATCHES_TEXT_BYTES)

    // Every call is made on the main actor, and throws
    // ``TesseraCoreError/sessionNotFound`` once the session is closed and
    // ``TesseraCoreError/nullPointer`` after the application shut down.

    /// Sets the search to `pattern` (an empty one clears it). The same
    /// pattern and options as the running search keep its matches and
    /// current match; any other starts afresh from the newest line. Nothing
    /// is scanned until ``searchStep(budgetMicros:)``.
    ///
    /// - Throws: ``SearchRefusal`` for a refused pattern (the search is then
    ///   cleared), or a ``TesseraCoreError``.
    @MainActor
    public func searchStart(_ pattern: String, regex: Bool, caseSensitive: Bool) throws {
        var options: UInt32 = 0
        if regex { options |= UInt32(TESSERA_SEARCH_REGEX) }
        if caseSensitive { options |= UInt32(TESSERA_SEARCH_CASE_SENSITIVE) }
        var bytes = Array(pattern.utf8)
        var refusal: UInt32 = 0
        var message = [UInt8](repeating: 0, count: 1024)
        var messageLength = 0
        let capacity = message.count
        let status = bytes.withUnsafeMutableBufferPointer { pattern in
            message.withUnsafeMutableBufferPointer { message in
                tessera_session_search_start(
                    core.pointer, id, pattern.baseAddress, pattern.count, options, &refusal, message.baseAddress,
                    capacity, &messageLength)
            }
        }
        if status == TESSERA_ERROR_INVALID_ARGUMENT {
            switch TesseraSearchRefusal(rawValue: refusal) {
            case TESSERA_SEARCH_REFUSAL_TOO_LONG: throw SearchRefusal.tooLong
            case TESSERA_SEARCH_REFUSAL_SYNTAX:
                throw SearchRefusal.syntax(
                    message: String(decoding: message.prefix(min(messageLength, capacity)), as: UTF8.self))
            case TESSERA_SEARCH_REFUSAL_TOO_COMPLEX: throw SearchRefusal.tooComplex
            case TESSERA_SEARCH_REFUSAL_NOT_UTF8: throw SearchRefusal.notUTF8
            default: break
            }
        }
        try TesseraCoreError.check(status)
    }

    /// Scans for about `budgetMicros` microseconds (clamped to
    /// ``TesseraSession/searchMaxStepMicros``), at least one piece of at
    /// most 64 KiB of text, and returns the status. It returns within
    /// `max(4 x budget, 250 ms)` plus one regex call, which is not
    /// interruptible and bounded by bytes, not time: with a pathological
    /// pattern the first call of a query can block for many seconds (7.3 s
    /// measured on an 8 KB line, ADR 0038), and at the 1 KiB floor up to
    /// about 0.7 s; a row slower than `max(4 x budget, 250 ms)` at the
    /// smallest piece is skipped (``SearchStatus/linesSkipped``).
    /// Call it once per
    /// display refresh while the status is not ``SearchStatus/done`` or
    /// output arrived; without a search it does nothing.
    @MainActor
    public func searchStep(budgetMicros: UInt32) throws -> SearchStatus {
        try searchCall { tessera_session_search_step(core.pointer, id, budgetMicros, $0) }
    }

    /// Makes the match before the current one (toward older output)
    /// current, wrapping to the newest, and scrolls the view to it when it
    /// is out of view.
    @MainActor
    public func searchOlder() throws -> SearchStatus {
        try searchCall { tessera_session_search_older(core.pointer, id, $0) }
    }

    /// Makes the match after the current one (toward newer output)
    /// current, wrapping to the oldest, and scrolls to it.
    @MainActor
    public func searchNewer() throws -> SearchStatus {
        try searchCall { tessera_session_search_newer(core.pointer, id, $0) }
    }

    /// Scrolls the view to the current match when it is out of view,
    /// without moving it (one call when a new query's first match is
    /// found: the newest match is current by itself, but only a move or
    /// this scrolls to it).
    @MainActor
    public func searchReveal() throws -> SearchStatus {
        try searchCall { tessera_session_search_reveal(core.pointer, id, $0) }
    }

    /// The search's status, without scanning.
    @MainActor
    public var searchStatus: SearchStatus {
        get throws { try searchCall { tessera_session_search_status(core.pointer, id, $0) } }
    }

    /// Ends the search; its highlights go from the next frame.
    @MainActor
    public func searchClear() throws {
        try TesseraCoreError.check(tessera_session_search_clear(core.pointer, id))
    }

    /// The text of every match found, in document order, joined by
    /// newlines, at most ``TesseraSession/searchMaxMatchesTextBytes``
    /// bytes; `truncated` says it stopped before a match that would pass
    /// that size. Child text: never log it.
    @MainActor
    public func searchMatchesText() throws -> (text: String, truncated: Bool) {
        // One call into a buffer of the cap: the core reads the terminal
        // again on each call, so sizing it first would read it twice.
        var buffer = [UInt8](repeating: 0, count: Self.searchMaxMatchesTextBytes)
        var length = 0
        var truncated = false
        let capacity = buffer.count
        let status = buffer.withUnsafeMutableBufferPointer {
            tessera_session_search_matches_text(core.pointer, id, $0.baseAddress, capacity, &length, &truncated)
        }
        try TesseraCoreError.check(status)
        return (String(decoding: buffer.prefix(min(length, capacity)), as: UTF8.self), truncated)
    }

    @MainActor
    private func searchCall(_ call: (UnsafeMutablePointer<TesseraSearchStatus>) -> TesseraError) throws -> SearchStatus {
        var status = TesseraSearchStatus()
        try TesseraCoreError.check(call(&status))
        return SearchStatus(status)
    }
}
