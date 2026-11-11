import Foundation
import TesseraCore
@testable import TesseraKit
import XCTest

/// The search calls of `TesseraSession` round-tripped through the C API
/// (#46b, ADR 0038), and smart case.
@MainActor
final class SearchTests: XCTestCase {
    private var app: TesseraApp!

    override func setUp() async throws {
        app = try TesseraApp()
    }

    override func tearDown() async throws {
        app?.shutdown()
        app = nil
    }

    /// A 4 x 40 session showing `lines`, then copying its input.
    private func session(_ lines: [String]) async throws -> TesseraSession {
        let text = lines.joined(separator: #"\r\n"#)
        let session = try app.spawn(
            .init(program: "/bin/sh", arguments: ["-c", "printf '\(text)'; exec cat > /dev/null"], rows: 4, cols: 40))
        let deadline = ContinuousClock.now + .seconds(5)
        while true {
            try session.selectAll()
            if let last = lines.last, try session.selectionText()?.hasSuffix(last) == true { break }
            guard ContinuousClock.now < deadline else {
                XCTFail("the child's text is not in the session")
                break
            }
            try await Task.sleep(for: .milliseconds(10))
        }
        try session.selectClear()
        return session
    }

    private func finish(_ session: TesseraSession) throws -> TesseraSession.SearchStatus {
        var status = try session.searchStep(budgetMicros: 4_000)
        var steps = 1
        while !status.done && steps < 1_000 {
            status = try session.searchStep(budgetMicros: 4_000)
            steps += 1
        }
        return status
    }

    func testTheConstantsAreTheCOnes() {
        XCTAssertEqual(TesseraSession.searchMaxMatches, 100_000)
        XCTAssertEqual(TesseraSession.searchMaxQueryBytes, 1_024)
        XCTAssertEqual(TesseraSession.searchMaxStepMicros, 1_000_000)
        XCTAssertEqual(TesseraSession.searchMaxMatchesTextBytes, 1 << 20)
    }

    func testAStartedSearchCountsStepsAndWraps() async throws {
        let session = try await session(["hit one", "miss", "hit two", "hit three"])
        XCTAssertEqual(try session.searchStatus, .init(), "no search yet")
        try session.searchStart("hit", regex: false, caseSensitive: false)
        XCTAssertTrue(try session.searchStatus.active)
        let status = try finish(session)
        XCTAssertEqual(status.matches, 3)
        XCTAssertEqual(status.current, 2, "the newest is current once found")
        XCTAssertTrue(status.done)
        XCTAssertEqual(try session.searchOlder().current, 1)
        XCTAssertEqual(try session.searchOlder().current, 0)
        XCTAssertEqual(try session.searchOlder().current, 2, "older wraps to the newest")
        XCTAssertEqual(try session.searchNewer().current, 0, "newer wraps to the oldest")
        let copied = try session.searchMatchesText()
        XCTAssertEqual(copied.text, "hit\nhit\nhit")
        XCTAssertFalse(copied.truncated)
        try session.searchClear()
        XCTAssertEqual(try session.searchStatus, .init(), "cleared")
    }

    func testTheCaseFlagAndRegexOptionReachTheCore() async throws {
        let session = try await session(["one FOO", "two foo", "f.o"])
        try session.searchStart("foo", regex: false, caseSensitive: true)
        XCTAssertEqual(try finish(session).matches, 1)
        try session.searchStart("foo", regex: false, caseSensitive: false)
        XCTAssertEqual(try finish(session).matches, 2)
        try session.searchStart("f.o", regex: false, caseSensitive: false)
        XCTAssertEqual(try finish(session).matches, 1, "a literal dot")
        try session.searchStart("f.o", regex: true, caseSensitive: false)
        XCTAssertEqual(try finish(session).matches, 3, "any character")
    }

    func testRefusalsAreTyped() async throws {
        let session = try await session(["text"])
        XCTAssertThrowsError(try session.searchStart("(", regex: true, caseSensitive: false)) { error in
            guard case .syntax(let message) = error as? TesseraSession.SearchRefusal else {
                return XCTFail("\(error)")
            }
            XCTAssertTrue(message.contains("("), "the message quotes the pattern")
        }
        XCTAssertFalse(try session.searchStatus.active, "a refused pattern clears the search")
        let long = String(repeating: "a", count: TesseraSession.searchMaxQueryBytes + 1)
        XCTAssertThrowsError(try session.searchStart(long, regex: false, caseSensitive: false)) { error in
            XCTAssertEqual(error as? TesseraSession.SearchRefusal, .tooLong)
        }
        XCTAssertThrowsError(try session.searchStart(#"\w{1000}{1000}"#, regex: true, caseSensitive: false)) { error in
            XCTAssertEqual(error as? TesseraSession.SearchRefusal, .tooComplex)
        }
        XCTAssertNoThrow(try session.searchStart(String(long.dropLast()), regex: false, caseSensitive: false))
    }

    func testAClosedSessionHasNoSearch() async throws {
        let session = try await session(["text"])
        try session.searchStart("text", regex: false, caseSensitive: false)
        try session.close()
        XCTAssertThrowsError(try session.searchStatus) { error in
            XCTAssertEqual(error as? TesseraCoreError, .sessionNotFound)
        }
        XCTAssertThrowsError(try session.searchStep(budgetMicros: 4_000))
    }

    /// Every field of the C status reaches ``TesseraSession/SearchStatus``,
    /// `lines_skipped` included (#318).
    func testTheCStatusMapsFieldByField() {
        var c = TesseraSearchStatus()
        XCTAssertEqual(TesseraSession.SearchStatus(c), .init())
        c.matches = 7
        c.has_current = true
        c.current = 3
        c.active = true
        c.done = true
        XCTAssertEqual(TesseraSession.SearchStatus(c), .init(matches: 7, current: 3, active: true, done: true))
        c.has_current = false
        XCTAssertNil(TesseraSession.SearchStatus(c).current, "no current without has_current")
        c.truncated = true
        XCTAssertEqual(
            TesseraSession.SearchStatus(c), .init(matches: 7, active: true, done: true, truncated: true))
        c.truncated = false
        c.lines_skipped = true
        XCTAssertEqual(
            TesseraSession.SearchStatus(c), .init(matches: 7, active: true, done: true, linesSkipped: true))
    }

    /// `searchReveal()` scrolls the view to the current match (the newest
    /// once found) without moving it, and does nothing without a search.
    /// Three matches, so that a reveal wired to the older or the newer move
    /// makes another one current (#375).
    func testRevealScrollsToTheCurrentMatchOnly() async throws {
        let filler = (1...30).map { "line \($0)" }
        let session = try await session(["hit", "hit", "hit"] + filler)
        XCTAssertEqual(try session.searchReveal(), .init(), "no search: nothing")
        XCTAssertEqual(try session.scrollPosition.offset, 0, "no search: the view stays")
        try session.searchStart("hit", regex: false, caseSensitive: false)
        let found = try finish(session)
        XCTAssertEqual(found.matches, 3)
        XCTAssertEqual(found.current, 2, "the newest is current once found")
        XCTAssertEqual(try session.scrollPosition.offset, 0, "a step does not scroll")
        let revealed = try session.searchReveal()
        XCTAssertEqual(revealed, found, "the newest stays current")
        XCTAssertEqual(try session.searchStatus.current, 2)
        let position = try session.scrollPosition
        // The newest match is the third line of the history; the top row
        // shows line `history - offset`.
        XCTAssertGreaterThanOrEqual(position.offset, position.history - 2, "scrolled up to the newest match")
        XCTAssertLessThanOrEqual(position.offset, position.history)
        XCTAssertEqual(try session.searchReveal(), found)
        XCTAssertEqual(try session.scrollPosition, position, "in view: no move")
        try session.searchClear()
        try session.scroll(to: .bottom)
        XCTAssertEqual(try session.searchReveal(), .init(), "cleared: nothing")
        XCTAssertEqual(try session.scrollPosition.offset, 0, "cleared: the view stays")
    }

    // MARK: Smart case

    func testSmartCaseMatchesCaseOnlyForAnUppercaseLetter() {
        let auto = TesseraSession.SearchCase.auto
        XCTAssertFalse(auto.isSensitive(for: "foo", regex: false))
        XCTAssertTrue(auto.isSensitive(for: "Foo", regex: false))
        XCTAssertTrue(auto.isSensitive(for: "straße Ä", regex: false), "any script's uppercase")
        XCTAssertFalse(auto.isSensitive(for: "123 -_", regex: false))
        XCTAssertTrue(auto.isSensitive(for: #"\W"#, regex: false), "a literal W")
        XCTAssertTrue(TesseraSession.SearchCase.sensitive.isSensitive(for: "foo", regex: false))
        XCTAssertFalse(TesseraSession.SearchCase.insensitive.isSensitive(for: "Foo", regex: false))
    }

    func testSmartCaseSkipsEscapesAndClassNamesInARegex() {
        let auto = TesseraSession.SearchCase.auto
        for pattern in [#"\W+"#, #"\S\B\D"#, #"\pL+"#, #"\p{Greek}x"#, #"\P{Lu}"#, #"a\\b"#] {
            XCTAssertFalse(auto.isSensitive(for: pattern, regex: true), pattern)
        }
        for pattern in [#"\WA"#, #"\pLX"#, #"\p{Greek}X"#, #"\\B"#, "[A-Z]"] {
            XCTAssertTrue(auto.isSensitive(for: pattern, regex: true), pattern)
        }
    }
}
