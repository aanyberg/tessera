import Foundation
import TesseraCore
@testable import TesseraKit
import XCTest

/// The selection, Copy and link calls of `TesseraSession` round-tripped
/// through the C API (#30): a 4 x 40 session whose child prints three lines,
/// one of them a URL, and then copies its input to its output.
@MainActor
final class SelectionTests: XCTestCase {
    private var app: TesseraApp!

    override func setUp() async throws {
        app = try TesseraApp()
    }

    override func tearDown() async throws {
        app?.shutdown()
        app = nil
    }

    private func session(
        printing text: String = #"one two\r\nsee https://a.bc/x now\r\nlast"#, endsWith last: String? = "last"
    ) async throws -> TesseraSession {
        let session = try app.spawn(
            .init(program: "/bin/sh", arguments: ["-c", "printf '\(text)'; exec cat"], rows: 4, cols: 40))
        guard let last else { return session }
        let deadline = ContinuousClock.now + .seconds(5)
        while true {
            try session.selectAll()
            if try session.selectionText()?.hasSuffix(last) == true { break }
            guard ContinuousClock.now < deadline else {
                XCTFail("the child's text is not in the session")
                break
            }
            try await Task.sleep(for: .milliseconds(10))
        }
        try session.selectClear()
        return session
    }

    func testGranularitiesAreTheCValues() {
        XCTAssertEqual(TesseraSession.SelectionGranularity.character.rawValue, TESSERA_GRANULARITY_CHAR.rawValue)
        XCTAssertEqual(TesseraSession.SelectionGranularity.word.rawValue, TESSERA_GRANULARITY_WORD.rawValue)
        XCTAssertEqual(TesseraSession.SelectionGranularity.line.rawValue, TESSERA_GRANULARITY_LINE.rawValue)
    }

    func testAClickSelectsNothingAndADragSelectsCells() async throws {
        let session = try await session()
        XCTAssertFalse(try session.hasSelection)
        XCTAssertNil(try session.selectionText())
        try session.selectBegin(col: 4, row: 0, granularity: .character, block: false)
        XCTAssertFalse(try session.hasSelection, "a click is no selection")
        try session.selectExtend(col: 6, row: 0)
        XCTAssertTrue(try session.hasSelection)
        XCTAssertEqual(try session.selectionText(), "two")
        try session.selectClear()
        XCTAssertFalse(try session.hasSelection)
    }

    func testWordLineBlockAndAdjust() async throws {
        let session = try await session()
        try session.selectBegin(col: 5, row: 0, granularity: .word, block: false)
        XCTAssertEqual(try session.selectionText(), "two")
        try session.selectBegin(col: 5, row: 1, granularity: .line, block: false)
        XCTAssertEqual(try session.selectionText(), "see https://a.bc/x now")
        try session.selectBegin(col: 1, row: 0, granularity: .character, block: true)
        try session.selectExtend(col: 2, row: 2)
        XCTAssertEqual(try session.selectionText(), "ne\nee\nas", "a block is a rectangle")
        try session.selectBegin(col: 0, row: 0, granularity: .character, block: false)
        try session.selectExtend(col: 2, row: 0)
        try session.selectAdjust(col: 6, row: 0)
        XCTAssertEqual(try session.selectionText(), "one two", "the nearer end moved")
    }

    func testSelectAllHasTheTextWithoutTrailingBlanks() async throws {
        let session = try await session()
        try session.selectAll()
        XCTAssertEqual(try session.selectionText(), "one two\nsee https://a.bc/x now\nlast")
    }

    func testASelectionOfBlanksIsEmptyText() async throws {
        let session = try await session(printing: "", endsWith: nil)
        try session.selectAll()
        XCTAssertTrue(try session.hasSelection)
        XCTAssertEqual(try session.selectionText(), "", "selected, but nothing to copy")
    }

    func testTextLongerThanTheFirstBufferIsReadWhole() async throws {
        let long = String(repeating: "abcdefghij", count: 4)
        let session = try await session(printing: "\(long)\\r\\nlast")
        try session.selectAll()
        XCTAssertEqual(try session.selectionText(), "\(long)\nlast")
    }

    func testLinkAtFindsTheURLUnderACell() async throws {
        let session = try await session()
        XCTAssertEqual(try session.link(col: 10, row: 1), "https://a.bc/x")
        XCTAssertNil(try session.link(col: 1, row: 1), "the word before it")
        XCTAssertNil(try session.link(col: 1, row: 0))
    }

    func testHoverIsSetAndCleared() async throws {
        let session = try await session()
        try session.setHover((col: 10, row: 1))
        try session.setHover(nil)
    }

    func testCallsFailOnceTheSessionIsClosed() async throws {
        let session = try await session()
        try session.close()
        XCTAssertThrowsError(try session.selectAll()) { XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound) }
        XCTAssertThrowsError(try session.selectionText())
        XCTAssertThrowsError(try session.link(col: 0, row: 0))
        XCTAssertThrowsError(try session.hasSelection)
    }
}
